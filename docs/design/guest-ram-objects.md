# Guest RAM as owned memory objects, and virtio-mem

**Status:** planned; implementation waits on the measurements in §4, each of which names what would
block it. Research done 2026-10-22, read-only (no HVF run, no build).

Today guest RAM is one private anonymous mapping in the worker, and reclaim releases pages inside it
with `madvise`. This note replaces that with guest RAM built from separate memory objects the worker
owns, released by replacing whole objects, and adds a virtio-mem device so the guest can hand memory
back in aligned blocks. It is the structural answer to several open entries in
`docs/hardening-backlog.md` (*Memory & reclaim*) and to the roadmap's guest-memory migration item.

## 1. What is wrong with the current shape

Guest RAM comes from vm-memory as `MAP_ANONYMOUS|MAP_NORESERVE|MAP_PRIVATE`
(vm-memory 0.18.0 `src/mmap/unix.rs:242`, built at
`third_party/libkrun/src/libkrun/src/vmm/builder.rs:2442-2461`). xnu already splits an anonymous
mapping over 128 MiB into 128 MiB map entries, each with its own VM object (xnu `vm_map.c:3477-3482`,
`ANON_CHUNK_SIZE` in `vm_param.h:253`). A release is `hv_vm_unmap`, then `MADV_FREE_REUSABLE` on the
backing (`third_party/libkrun/src/hvf/src/released_ram.rs:280-322`), healed on the next stage-2 fault.

The problems that shape causes or cannot fix:
1. **Double billing.** A page both the worker and the guest touched is billed twice to
   `phys_footprint` (`spikes/hv-ledger-gap/`, `spikes/vm-ledger-compare/`). The settle sweep debits it,
   but only for guests that send limina-agent pressure reports, so a stock guest keeps ~2x.
2. **Memory outliving the worker.** Compressed and swapped guest pages stay stored after the worker
   exits, owned by nobody, until a host reboot (backlog *Host anonymous memory outlives a worker…*).
   xnu frees compressor slots only when the object dies, when MADV_FREE/REUSABLE kills a non-resident
   page, on decompress, and on recompress over an existing slot; the MADV kill fixes only the task's
   own pmap, not the stage-2 mapping. Who holds the orphaned memory is not known.
3. **Stranded vs converged** cannot be told apart (backlog *`inelastic` hold…*).
4. **Fragmentation.** Scattered 4 KiB inflates leave a ~1.6x warm-read tax after a deep dig (backlog
   *Post-episode warm-read tax…*).
5. **Migration and imports.** Devices reach guest RAM through a fixed linear map; a release under a
   live no-copy GPU import would hand the GPU reclaimed pages; stage-2 map/unmap gets slower with the
   number of mappings (roadmap *Guest-memory migration*).

## 2. Direction A: owned, ledger-tagged blocks

**What Apple does.** `hv_vm_allocate` (macOS 12.1+, SDK `hv_vm_allocate.h:36-60`: "enables accurate
memory accounting") reserves the range with no access, then fills it with 128 MiB memory entries
created `MAP_MEM_NAMED_CREATE | MAP_MEM_LEDGER_TAGGED` and mapped over the reservation with
`VM_FLAGS_OVERWRITE`. That is decoded from the argument constants of its disassembly on this host;
the call targets are unresolved, so it is inference, though confident. libkrun binds it
(`third_party/libkrun/src/hvf/src/bindings.rs`) and never calls it. Apple forum thread 851651
(<https://developer.apple.com/forums/thread/851651>) has an accepted answer that its memory is not
billed once per mapping.

**Why it bills once.** Mapping a tagged or purgeable internal object sets `use_pmap = FALSE`
(`vm_map.c:3515-3535`), so its faults take `PMAP_OPTIONS_ALT_ACCT` (`vm_map.c:14962-14963`,
`vm_fault.c:7009`). Each mapping's `internal` is offset by `alternate_accounting`, and the page is
billed once, to the owner's `tagged_footprint` (`vm_object.c:8740ff`). A task that owns its own
entries needs no entitlement (`vm_memory_entry.c:1529-1532`; the create path at `:360-423`). The
stage-2 half is compiled out of the public xnu drop (`is_stage2 = false` in the sptm pmap), so
whether HVF's stage-2 mapping honours alternate accounting is only inferred, from the forum answer
and from Vz and Parallels VMs showing no double billing. §4.1 settles it.

**The trade-off.** On such objects `MADV_FREE_REUSABLE` succeeds and does nothing
(`vm_map.c:17218-17255`), and deleting or overwriting part of an object frees nothing until the whole
object dies (`spikes/balloon-compressor-zero/xnu-compressor-semantics.md`, Q3). So the release unit
is the object. Object death is also the one path xnu guarantees frees both resident pages and
compressor slots (`vm_object.c:1485ff`), and owned objects are disowned on reap and at task death
(`vm_object.c:1497-1517`, `task.c:2192`).

**The design.**
- Guest RAM is a reservation filled with tagged blocks, and the release unit is the whole block. A
  stock 4 KiB guest's balloon takes single pages, so a large block frees nothing until all of it
  is back; small blocks keep per-page reclaim and cost more (§4.1: 16 KiB blocks take 3.2 s
  to allocate and 491 ms to map at 24 GiB, against 39 ms and 3.5 ms for 2 MiB). In practice the
  balloon takes its pages from the guest's free lists, which hold a freed region as long runs, so
  blocks complete: a stock guest that freed 3 GiB and was ballooned by 2 GiB gave back 2004 MiB on
  2 MiB blocks and 2049 MiB on anonymous RAM (`tests/balloon_touched.rs`, measured 2026-10-22). A
  guest whose free memory is fragmented is not measured. The layout follows
  the VM's reclaim mode, chosen when RAM is built, so a mode changed at runtime takes its layout at
  the next boot:
  - `aggressive`: 2 MiB blocks for the VM's minimum, 16 KiB blocks for the elastic `min..max`
    range, so balloon reclaim stays per page on every guest.
  - `moderate` (the default): 2 MiB blocks everywhere. Reclaim comes from whole blocks, which
    virtio-mem and free page reporting hand back aligned; a balloon on a stock guest frees less.
  - `light`: either retired, or a gentler virtio-mem setting; decided with direction B.
  - `disabled`: 2 MiB blocks, no reclaim.
  Mechanism in libkrun takes the layout as a list of ranges and block sizes; the mapping from mode
  to layout is limina's.
- A release is `hv_vm_unmap` of the block, then overwriting it with a fresh tagged entry. The block
  is mapped again on the next stage-2 fault, as today's heal does, or eagerly.
- The address range stays linear, so `GuestMemoryMmap`, virtio queues and snapshots are unchanged.
- What retires: `MADV_FREE_REUSABLE` and `MADV_FREE_REUSE` on guest RAM; zeroing on release (a new
  object reads as zeros); the ledger settle sweep and its fault plumbing, if §4.1 shows single
  billing.
- What it does not fix: an object pinned by an outside holder still survives the worker (§4.2).

## 3. Direction B: a virtio-mem device

virtio-mem lets the guest plug and unplug memory in aligned blocks inside a device-owned region.
Linux has it on arm64 since v5.18 (6f4abbaa1bd3). Neither libkrun, crosvm nor any macOS VMM has one;
QEMU's works only on Linux hosts.

- **Units.** The guest's sub-block is `max(device block size, pageblock)` (`virtio_mem.c:2561`):
  2 MiB on a 4 KiB guest, 32 MiB on a 16 KiB guest, inside 128 MiB memory blocks. Each maps onto one
  or more tagged blocks of direction A, so an unplug is whole-object death.
- **Stock tier.** Fedora 44 aarch64 builds `CONFIG_VIRTIO_MEM=m` with
  `MHP_DEFAULT_ONLINE_TYPE_ONLINE_AUTO=y`, so plugged memory comes online by itself, but into
  `ZONE_NORMAL`, which makes unplug best-effort.
- **Enhanced tier.** Our kernel onlines the region movable, which makes unplug reliable.
- **Why it matters.** The guest migrates pages out of a block before unplugging it, so unplug is
  compaction (problem 4). The plugged size is an exact contract between host and guest (problem 3).
  Long-term pinned pages are migrated out of movable memory or block offlining, so a block under a
  live GPU import is never handed back (problem 5).
- **Constraints.** The device region must stay out of Cardea's memory map. virtio-mem blocks
  hibernation; suspend-to-idle is fine (`virtio_mem.c:3086-3107`). On arm64 the guest never touches
  unplugged memory, so the host may leave it unmapped.
- **Relation to the balloon.** Base RAM (the VM's `min`) stays balloon-managed; the elastic range
  (`min..max`) becomes virtio-mem where the guest drives the device. A guest without `virtio_mem`
  keeps the balloon over the whole range, as today.

### 3.1 Which layout a boot takes

`crates/limina/src/elastic_mem.rs` picks one of three layouts per worker spawn:

- **Whole.** All of `max` is RAM and the balloon covers `min..max`. Flat CLI runs (no `state.toml`)
  always boot this way, as does any VM whose range is under one 128 MiB memory block.
- **Probe.** As Whole, plus a dormant 128 MiB virtio-mem region the guest is never asked to plug.
  A managed VM boots this way until its guest binds the device; the supervisor then writes
  `virtio_mem = true` to `state.toml`. The dormant device still blocks hibernation.
- **Split.** RAM is `max` minus the region, where the region is `max − min` rounded down to whole
  memory blocks; the worker asks the guest to plug all of it from boot
  (`--virtio-mem-requested-mib`). The policy keeps its decisions and sends them as plug sizes:
  reclaim `r` becomes `mem (region − r)`, rounded up to the 2 MiB device block. It holds until the
  boot-time plug lands (or 120 s pass) and never scrubs. A Split boot runs on `min` until
  `virtio_mem` loads, and for the whole session if it never does; a boot that runs 300 s without
  binding clears the flag, so the next one probes again.

A resume takes the layout recorded in its `[suspended]` record, whatever has been learned since.
A reboot relaunch inside one session takes what has been learned, so it may split where the first
boot probed. `LIMINA_VIRTIO_MEM=<MiB>` is a test lever, not a mode: a fixed region beside Whole RAM,
plugged only through the control socket's `mem` command.

Free page reporting is off for every guest (`crates/limina/src/main.rs:357-362`, the stock
suspend-to-idle crash; the upstream fix 0b45f6927a14 is in v7.2-rc6, Fedora 44 runs 6.19). Where it
is on, its reports are naturally aligned 2 MiB blocks on a 4 KiB guest, which also map onto 2 MiB
tagged blocks with no agent.

## 4. Measurements first, with what would block

Run on an idle host (no other VM, no suite). Each experiment names its oracle and its blocker.

1. **Tagged-block ledger probe** (extend `spikes/hv-ledger-marker`'s `double` mode). Legs:
   `hv_vm_allocate`; self-made 2 MiB and 16 KiB tagged entries; an untagged control. Touch the same
   range from the host and the guest, then read `internal`, `alternate_accounting`,
   `tagged_footprint` and `phys_footprint` (ledger-dump). Then `MADV_FREE_REUSABLE`, then
   `hv_vm_unmap` and overwrite one block, and read the compressor's stored pages a second later; add
   a compressed leg. Also test whether one `hv_vm_map` can span many map entries.
   *Predicted:* 1x billing on the tagged legs, 2x on the control; REUSABLE changes nothing; a
   replaced block frees its resident and compressed pages.
   *Blocker for A:* the tagged legs bill 2x (HVF's stage-2 mapping ignores alternate accounting), or
   a replaced block does not free its pages.
   *Measured* (`spikes/guest-ram-objects/RESULTS.md`): no blocker. Every owned-object leg bills 1x
   through stage-2, REUSABLE does nothing, and a replaced block frees its resident pages at once and
   reads as zeros; one `hv_vm_map` spans any number of blocks. 16 KiB blocks cost 30–60x what
   2 MiB blocks do, so blocks are 2 MiB. On an idle host, a replaced block's compressed pages leave
the host compressor at once (772 MB of 777 MB), as anonymous memory's do under REUSABLE.
2. **Who holds orphaned memory.** Exit {clean, SIGKILL, SIGABRT} × {venus, GPU off} × {GPU host alive,
   killed} × {supervisor alive, killed}. Oracles: `vm_stat` compressor pages and `vm.swapusage`
   before and after; `zprint` for VM objects, compressor pagers and map entries;
   `ioclasscount IOGeneralMemoryDescriptor IOMemoryMap IOSurface`; `lsmp -p` on survivors. Not a
   blocker; it says what direction A leaves to fix.
3. **Cost at scale** inside a live HVF boot: 24 GiB as 16 KiB blocks vs 2 MiB blocks — setup time,
   wired memory, map entry count, boot time, and `hv_vm_map`/`hv_vm_unmap` latency against the number
   of released runs. *Blocker for a block size:* setup or wired cost a user would notice; it picks
   the size rather than blocking A.
4. **Vz re-measured:** repeat `spikes/vm-ledger-compare`'s 6 GiB read and ledger-dump the Vz helper.
   Expected: `tagged_footprint` tracks touched memory and `alternate_accounting` roughly equals
   `internal`. Corroborates §4.1; not a blocker.
5. **virtio-mem on a stock guest:** a prototype device first. Check that Fedora 44 autoloads
   `virtio_mem` on virtio-mmio, which zone it onlines into, and the unplug success rate under a
   pinned workload; repeat on the enhanced kernel with movable onlining.
   *Blocker for B:* the stock guest cannot use the device at all. A low stock unplug rate is not a
   blocker (the balloon stays as the stock fallback); it shapes the policy.
   *Measured 2026-10-22* (`tests/virtio_mem.rs`, stock F44 guest, 4 GiB RAM, a 2 GiB region at
   `0x140000000`): no blocker. `virtio_mem` autoloads on virtio-mmio, plugs the 1 GiB asked in
   2 MiB sub-blocks within 128 MiB memory blocks, and onlines it into `ZONE_NORMAL`
   (`auto_online_blocks` is `online`); MemTotal grows by 1024 MiB. Used and freed, then asked
   back, 1022 MiB unplugs and the worker's footprint falls 1011 MiB. With 200k tmpfs inodes held
   (280 MB of unreclaimable slab) 4 MiB stays plugged in `ZONE_NORMAL` and none in `ZONE_MOVABLE`
   (`online_movable` written to `auto_online_blocks` on the same stock kernel). The inodes are a
   light load: most of the slab can land outside the plugged range, so the stock rate under a
   load that fills the plugged blocks with kernel objects is not measured.

## 5. Implementation order

1. Measurements §4.1 and §4.3 (they gate A and pick the block size).
2. **A in libkrun:** a guest-memory allocator that builds the reservation from tagged blocks, a
   release that replaces blocks, and the heal path mapping blocks back; the balloon and free page
   reporting release through it. Mechanism in libkrun, as now; the block size is policy in limina.
   RED-first: an L2 test that boots a guest, has host and guest touch the same pages, and asserts
   `phys_footprint` bills them once; and one that releases a block and asserts the compressor's
   stored pages fall.
3. Retire the settle sweep, REUSABLE and zero-on-release once §4.1 shows single billing on a stock
   guest.
4. Measurement §4.5 with a prototype virtio-mem device.
5. **B in libkrun:** the virtio-mem device over a tagged-block region, its config space and
   plug/unplug requests; Cardea leaves the region out of the memory map. **B in limina:** the
   policy drives virtio-mem for the elastic range when the guest has bound the device, and the
   balloon otherwise; the enhanced kernel onlines the region movable.
6. Measurement §4.2 runs alongside; whatever holds orphaned memory is fixed on its own terms.

## 6. Prior evidence this note revises

- `spikes/vm-ledger-compare` measured the Vz helper's resident size, not its `phys_footprint`, and
  resident size equals footprint only for untagged memory. Its "Vz gets no exemption" holds for
  resident size; for footprint it is unmeasured (§4.4).
- `spikes/hv-ledger-marker`'s `notag` mode tested an unowned, no-footprint entry, which needs a
  private entitlement. A self-owned tagged entry needs none and was never tested (§4.1).
- Releasing by mapping fresh anonymous memory over a sub-range of a larger object (used by several
  macOS VMMs) lowers the footprint figure while the object keeps the pages and compressor slots until
  it dies. Not an option.
- File-backed guest RAM takes the pages out of `phys_footprint`, so memory pressure handling cannot
  see them. Not an option.
