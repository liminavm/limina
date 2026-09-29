# Live guest pages move onto host memory we choose, at 4 KiB, without losing a store

Question: at the 4 KiB IPA granule, can the host move live guest pages onto host memory of its
choosing without the guest noticing? The move is: `hv_vm_unmap` the 4 KiB page, copy it, and
`hv_vm_map` the new memory at the same guest-physical address. It must not lose a store made while
the move is in flight. This is the mechanism behind roadmap M6 §Guest-memory migration. Its first
consumer gathers scattered 4 KiB guest pages into one contiguous 16 KiB-aligned host buffer that
Metal takes as a no-copy buffer. No host-side remap can do that (`spikes/userptr-remap-metal/`).

The vehicle is a bare-metal guest (`payload.S`). It runs with the MMU on, and guest RAM is mapped
Normal write-back cacheable, which is the shape a Linux guest has. It fills and checks a host-chosen
list of scattered 4 KiB pages with an address-keyed pattern (`gpa ^ salt<<48`). Guest RAM is one
`MAP_ANON|MAP_PRIVATE` mapping, `hv_vm_map`'d whole at a 4 KiB IPA granule, as vm-memory and
limina set it up. No two pages in the list are adjacent, and about three quarters are not
16 KiB-aligned. The host (`probe.m`) moves pages with `migrate()`: take a lock, unmap, `memcpy`,
map, release the lock. A vCPU that touches a page mid-move takes a stage-2 data abort, waits on
the lock and retries the instruction, the shape of the balloon's heal path. Negative controls (a
check against a salt nobody wrote) must report every word on the guest and on the GPU.

The steps are:

1. The guest fills the pages on their original backing.
2. Every page migrates into one contiguous buffer B.
3. The GPU checks and writes B through a no-copy `MTLBuffer`.
4. The pages migrate back onto their original backing.
5. The race: the guest runs *R* rounds of fill-then-check with no exit, each round with a new
   salt. Meanwhile a host thread ping-pongs every page between its original backing and B, in a
   random order each pass, as fast as it can. A store lost across a move, or a copy taken before
   a store landed, shows up as a stale word in a later check.

## Measured

Measured 2026-09-29 on an M1 Max with macOS 26.6.2 (`./build.sh`, sandbox off). Every run passed
every check.

- **The guest sees the copy and really moves.** After migrating into B the guest reads its old
  pattern. Its next fill lands in B, and the original backing still holds the old pattern.
- **The GPU and the guest share the migrated pages both ways.** A no-copy `MTLBuffer` over B reads
  the guest's fill. The guest reads the GPU's write. The same buffer then reads the guest's next
  fill.
- **Moving back works onto host addresses that are 4 KiB- but not 16 KiB-aligned.** Those are the
  original backing of each unaligned guest page. The guest reads and writes through those mappings
  correctly afterwards.
- **No store is lost in the race.** Across every run, millions of moves took place while the
  guest filled and checked. There were stage-2 faults mid-move, at most 3 in a row on one page,
  and every round's check saw zero stale words. The host read the last round's pattern at each
  page's final backing.
- **Migration adds no host VM map entries.** It changes stage-2 only. The count is flat across the
  migration and settles about +10 after the race, the same at every size, so that is the migrator
  thread and Metal rather than the pages. Aliasing, by contrast, costs one entry per
  non-contiguous run.

| pages | race rounds | page moves in the race | vCPU faults healed | cost per move in the race | cold migration of all pages |
|---|---|---|---|---|---|
| 256 (1 MiB) | 1600–4000 | 4.2M–9.0M | 1352–3401 | 1.84–1.85 µs | 3.1–6.2 µs/page |
| 1024 (4 MiB) | 400–500 | 1.1M–1.5M | 349–455 | 2.54–2.56 µs | 3.3–3.5 µs/page |
| 2048 (8 MiB) | 200 | 639k | 173 | 3.39 µs | 3.9 µs/page |
| 4096 (16 MiB) | 100 | 306k | 91 | 5.19 µs | 5.2 µs/page |
| 8192 (32 MiB) | 60 | 122k | 55 | 8.20 µs | 8.1 µs/page |
| 16384 (64 MiB) | 60 | 80k | 58 | 13.6 µs | 13.1 µs/page |

"Cost per move in the race" covers unmap + copy + map, with the lock held and warm. "Cold" is
the first migration of every page, lock included, splitting the one big RAM mapping for the first
time.

### Stage-2 fragmentation has a price

Every move does the same work: one 4 KiB unmap, a 4 KiB copy and one 4 KiB map. Yet the cost per
move grows with the number of separate stage-2 mappings in the VM: 1.85 µs at 256, 5.2 µs at
4096 and 13.6 µs at 16384. Past about 2k mappings that is roughly +0.7 µs per thousand. The
`memcpy` is a fixed fraction of a microsecond, so the growth is in `hv_vm_unmap`/`hv_vm_map`. That
is consistent with HVF walking something proportional to the mapping count on every call, though
the internals are not visible from here. What was measured is map/unmap cost, not guest access
speed. TLB reach and guest-side cost of a fragmented stage-2 are not measured here.

The consequences:

- Migrating a large, fully scattered import costs about 13 µs per page by 16k pages. A 64 MiB
  userptr at 4 KiB takes ~215 ms, and it gets slower still as the VM's mapping count grows.
- Every scattered 4 KiB mapping taxes **all** later stage-2 operations in that VM. That includes
  the balloon's own release/heal, which is itself a source of scattered mappings. This is a
  measured reason to consolidate the stage-2 mappings of a heavily ballooned guest, as well as its
  host backing.

## Conclusions

- **The HVF half of guest-memory migration works.** It holds at 4 KiB granularity, onto
  16 KiB-aligned buffers and back onto 4 KiB-aligned host addresses. The guest, the GPU and the
  host stay coherent, and it survives a vCPU writing to the pages throughout. HVF's `hv_vm_unmap`
  takes effect for a running vCPU on another core before it returns: nothing stored after the
  unmap reached the old page.
- **It gives a 4 KiB guest what aliasing cannot.** Scattered 4 KiB pages become one no-copy Metal
  buffer, with no host VM entries spent.
- **The cost is stage-2 mapping count, and it compounds.** Coalesce runs into single `hv_vm_map`
  calls wherever guest pages are contiguous. Budget scattered mappings per VM. Treat consolidation
  (re-merging stage-2 mappings) as a first-class operation, not an afterthought.
- **What remains is libkrun's memory model, not HVF.** Roadmap M6 lists it.

## Scope of the race (measurement boundaries)

- **One vCPU.** It runs on its own thread against a migrator on another, so cross-core invalidation
  is exercised. HVF's invalidation across *several* vCPUs is not; a second vCPU racing a disjoint
  page list is the next variant.
- **STP stores only.** `spikes/hv-stage2-write-loss/` covered `DC ZVA` and SIMD stores against
  `hv_vm_protect`, not against unmap/map.
- **Data pages only, never code.**
- **The heal waits on one global lock.** A real implementation keeps per-page state.

## Not covered (design hazards, not results)

- **`released_ram` knows nothing about migration.** A free-page report on a migrated page would
  `hv_vm_unmap` its guest address, which is now B's mapping. The heal would then map the *original*
  backing back, silently reverting the page to stale contents. The redirection layer in roadmap M6
  has to own this.
- **Device DMA.** libkrun's devices reach guest RAM through `GuestMemoryMmap`'s fixed linear map,
  so they would still read and write the original backing of a migrated page. The same redirection
  layer has to cover them.
- **Moving a page out from under a live GPU import.** The `MTLBuffer` keeps the old backing, so
  the guest and GPU would silently diverge. The mechanism must refuse to do this, not merely avoid
  it.
