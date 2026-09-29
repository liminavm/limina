# Live guest pages move onto host memory we choose, at 4 KiB, without losing a store

Question: at the 4 KiB IPA granule, can the host move live guest pages onto host memory of its
choosing without the guest noticing? The move is: `hv_vm_unmap` the 4 KiB page, copy it, and
`hv_vm_map` the new memory at the same guest-physical address. It must not lose a store made while
the move is in flight. This is the mechanism behind roadmap M6 §Guest-memory migration. Its first
consumer gathers scattered 4 KiB guest pages into one contiguous 16 KiB-aligned host buffer that
Metal takes as a no-copy buffer. No host-side remap can do that (`spikes/userptr-remap-metal/`).

The vehicle is a bare-metal guest (`payload.S`). It runs with the MMU on, and guest RAM is mapped
Normal write-back cacheable, which is the shape a Linux guest has. It fills and checks a host-chosen
list of scattered 4 KiB pages with an address-keyed pattern (`gpa ^ salt<<48`). It runs on 1–4
vCPUs, each on its own host thread with its own control block (`--vcpus`, `--mode`):

- **`disjoint`:** each vCPU owns whole pages.
- **`interleaved`:** every vCPU works on every page, each owning every *N*-th 16-byte unit. Each
  page is then live in every vCPU's TLB at once. A move must invalidate all of them, and a vCPU
  that HVF missed would store into the old page, which its next check would catch. Guest RAM is one
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
5. The race: every vCPU runs *R* rounds of fill-then-check with no exit, each round with a new
   salt. Meanwhile a host thread ping-pongs every page between its original backing and B, in a
   random order each pass, as fast as it can. A store lost across a move, or a copy taken before
   a store landed, shows up as a stale word in a later check.

The positive control is `--sabotage`. It takes each race move's copy *before* the unmap, the
realistic ordering bug, and so proves the checkers can see a lost store.

## Measured

Measured 2026-09-29 on an M1 Max with macOS 26.6.2 (`./build.sh`, sandbox off). Every run passed
every check, except that the `--sabotage` runs fail exactly the stale-word check, as designed.

- **The guest sees the copy and really moves.** After migrating into B the guest reads its old
  pattern. Its next fill lands in B, and the original backing still holds the old pattern.
- **The GPU and the guest share the migrated pages both ways.** A no-copy `MTLBuffer` over B reads
  the guest's fill. The guest reads the GPU's write. The same buffer then reads the guest's next
  fill.
- **Moving back works onto host addresses that are 4 KiB- but not 16 KiB-aligned.** Those are the
  original backing of each unaligned guest page. The guest reads and writes through those mappings
  correctly afterwards.
- **No store is lost in the race, on any vCPU.** Across every run, up to millions of moves took
  place while the vCPUs filled and checked. Every vCPU took stage-2 faults mid-move, at most 3 in a
  row on one page, and every round's check on every vCPU saw zero stale words. The host read the
  last round's pattern at each page's final backing.
- **The checkers do catch a lost store.** With `--sabotage`, every vCPU reports tens of thousands
  of stale words in every mode.
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
time. All rows above are 1 vCPU.

### Several vCPUs

Every run is 4000 rounds unless noted.

| vCPUs | mode | pages | page moves | faults healed per vCPU | stale words, correct migration | stale words, `--sabotage` | cost per move |
|---|---|---|---|---|---|---|---|
| 1 | — | 256 | 1.3M (500 rounds) | 448 | 0 | — | 1.85 µs |
| 2 | disjoint | 256 | 737k | 1269–1332 | 0 on each | 28–34k on each (500 rounds) | 2.38 µs |
| 2 | interleaved | 256 | 1.38M | 2403–2494 | 0 on each | 25–27k on each (500 rounds) | 2.52 µs |
| 4 | disjoint | 256 | 132k | 477–535 | 0 on each | — | 3.00 µs |
| 4 | interleaved | 256 | 274k | 2450–2590 | 0 on each | 23–27k on each (1000 rounds) | 4.54 µs |
| 4 | interleaved | 1024 | 251k (1000 rounds) | 660–774 | 0 on each | — | 3.98 µs |

- **HVF invalidates a moved page on every vCPU before `hv_vm_unmap` returns.** In interleaved mode
  all four vCPUs hold every page live and store to it throughout, and no store reached an old
  backing.
- **A move costs more the more vCPUs run.** With the same 256 pages, cost rises from 1.85 µs with 1
  vCPU to 3.0 µs with 4 disjoint and 4.5 µs with 4 interleaved. That is consistent with the
  invalidation having to reach every running vCPU, but that cause is inferred, not measured.

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
  host stay coherent, and it survives up to four vCPUs writing to the same pages throughout. HVF's
  `hv_vm_unmap` takes effect on every running vCPU before it returns: nothing stored after the
  unmap reached the old page.
- **It gives a 4 KiB guest what aliasing cannot.** Scattered 4 KiB pages become one no-copy Metal
  buffer, with no host VM entries spent.
- **The cost is stage-2 mapping count, and it compounds.** It grows further with running vCPUs.
  Coalesce runs into single `hv_vm_map` calls wherever guest pages are contiguous. Budget scattered
  mappings per VM. Treat consolidation (re-merging stage-2 mappings) as a first-class operation,
  not an afterthought.
- **What remains is libkrun's memory model, not HVF.** Roadmap M6 lists it.

## Scope of the race (measurement boundaries)

- **Up to four vCPUs, one migrator thread.** Two hosts migrating at once, or migration racing the
  balloon's own unmap/map, is not exercised.
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
