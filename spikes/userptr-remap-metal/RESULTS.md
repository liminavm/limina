# Scattered guest pages, remapped into one host range, work as a no-copy Metal buffer

Question: a venus `VK_EXT_external_memory_host` would reach the host as a guest userptr blob. That
blob is a list of pinned, scattered guest pages, and KosmicKrisp imports a host pointer through
`newBufferWithBytesNoCopy` (`kk_device_memory.c:227`). So could the worker stitch those pages into
one contiguous host range (`mach_vm_remap`, `copy=FALSE`) and give that range to Metal? The result
must still be the guest's own pages, coherent in both directions, while the guest keeps running on
them.

The vehicle is a bare-metal guest (`payload.S`). It runs with the MMU on, and guest RAM is mapped
Normal write-back cacheable, which is the shape a Linux guest has. It fills and checks a host-chosen
list of 16 KiB guest pages. Each word encodes its own guest-physical address and a salt
(`gpa ^ salt<<48`), so any checker can compute what a word should be however the alias has
reordered the pages. The host (`probe.m`) remaps the pages into one alias and wraps the alias in a
`StorageModeShared` no-copy `MTLBuffer`. A compute kernel then checks and writes the same pattern
on the GPU. Guest RAM is `MAP_ANON|MAP_PRIVATE`, as vm-memory's `from_ranges` allocates it, and is
`hv_vm_map`'d RWX before anything runs.

Each round runs this sequence:

- The guest fills the pages. The GPU checks them.
- The GPU writes a new pattern. The guest checks it through stage-2.
- The guest refills the pages. The GPU checks them again on the **same** `MTLBuffer`.

The last step catches a buffer that snapshotted or privately copied the pages when it was created
or wired. Negative controls (a check against a salt nobody wrote) must report every word on both
the guest and the GPU, so a broken checker cannot read as a pass.

## Measured

Measured 2026-09-29 on an M1 Max with macOS 26.6.2 (`./build.sh`, sandbox off). Every run passed
every check, with 0 mismatching words in any direction.

| run | userptr | guest layout | remap | VM regions added | `newBufferWithBytesNoCopy` | first GPU pass |
|---|---|---|---|---|---|---|
| default | 4 MiB (256 pages) | all scattered, incl. split and reversed guest neighbours | 0.28 ms | +256 | 0.16 ms | 2.2 ms |
| `--untouched` | 4 MiB | scattered, remapped **before** the guest ever touched them | 0.27 ms | +256 | 0.14 ms | 2.2 ms |
| `--shared` | 4 MiB | scattered, RAM `MAP_SHARED` | 0.28 ms | +256 | 0.13 ms | 2.1 ms |
| `--run 16` | 4 MiB | runs of 16 guest-contiguous pages | 0.025 ms | +16 | 0.02 ms | 2.2 ms |
| `--pages 8192` | 128 MiB | all scattered | 9.0 ms | +8191 | 2.7 ms | 6.9 ms |
| `--pages 8192 --run 64` | 128 MiB | runs of 64 | 0.17 ms | +128 | 0.17 ms | 5.1 ms |
| `--seed 7 --rounds 8` | 4 MiB | scattered, 8 write/fill rounds | 0.27 ms | +256 | 0.10 ms | 2.3 ms |

After teardown (drop the buffer, `mach_vm_deallocate` the alias), the guest's pages fill and check
correctly through both the guest and the VMM's mapping. The region count settles 13 above the
pre-alias count, and it then stays flat under repeated imports. `--cycles` repeats remap, buffer,
GPU check and teardown: 200 cycles at 4 MiB stayed at 120 regions, and 40 cycles of the fully
scattered 128 MiB import stayed at 119. Every cycle's GPU check passed. The residue is a one-time
Metal warm-up, not a per-import leak.

### The 4 KiB questions

- Metal on this OS accepts pointers and lengths finer than the host page, and reads them correctly.
  The same holds on freshly allocated memory that no `MTLBuffer` has ever covered. These probes run
  before any other buffer exists over the alias, so an acceptance cannot come from pages another
  buffer had already wired.
  - `newBufferWithBytesNoCopy(alias + 4096, 16 KiB)` returns a buffer, and a GPU read-back matches
    the guest pages word for word, including across the page boundary into the next alias page.
  - `newBufferWithBytesNoCopy(alias, 4 KiB)` returns a 4096-byte buffer that also reads back
    correctly.
  Both are sub-ranges of whole 16 KiB pages, so this says nothing about 4 KiB-granular *backing*;
  see below.
- In a normal (16 KiB) process `mach_vm_remap` works in whole 16 KiB pages. Remapping 4 KiB that
  starts 4 KiB into a guest page returns a host-page-aligned address, and the new mapping exposes
  **the whole 16 KiB page** (2048/2048 words match the guest page).

### 4 KiB address spaces exist, and the GPU still refuses 4 KiB-scattered backing

`fourk-probe.m` asks the same questions at 4 KiB granularity. It runs as a native arm64 process,
as an x86_64 process under Rosetta, and under `fourk-spawn.c`. `fourk-spawn` launches a program
with the private `_POSIX_SPAWN_FORCE_4K_PAGES` flag (xnu `bsd/sys/spawn.h:69`), which asks
`load_machfile` for a 4 KiB pmap and a page-shift-12 `vm_map` (`bsd/kern/mach_loader.c:732-765`).
The kernel support is compiled for every Apple SoC header in the tree, H13 (M1) to H16
(`__ARM_MIXED_PAGE_SIZE__`, `pexpert/pexpert/arm64/H13.h:82`). Measured 2026-09-29 on the M1 Max,
macOS 26.6.2:

| process | page size seen | scattered 4 KiB `mach_vm_remap` | lone 4 KiB remap | no-copy `MTLBuffer` over 4 KiB-scattered pages | plain 16 KiB-aligned control |
|---|---|---|---|---|---|
| native arm64 | 16384 | truncated to the 16 KiB boundary | whole 16 KiB page | nil | a buffer |
| native arm64 with `_POSIX_SPAWN_FORCE_4K_PAGES` | — | — | — | — | — |
| x86_64 under Rosetta | 4096 (`vm_kernel_page_size` 16384) | lands exactly, coherent with the source | a 4096-byte region at a 4 KiB offset | **nil**, even with the alias 16 KiB-aligned and 16 KiB long | a buffer |

- **A 4 KiB address space is real on the shipping kernel, but only for Rosetta processes.** Every
  native arm64 binary spawned with the flag fails with errno 88 (`EBADMACHO`), including
  `/usr/bin/true`, a trivial program, and one code-signed with 4 KiB hash pages. An x86_64 binary
  spawned with the flag runs. Apple's own test of the flag (`tests/vm/mixed_pagesize.plist`) is
  marked `Disabled` (rdar://133462123), and the sysctl its launcher checks,
  `debug.vm_mixed_pagesize_supported`, exists only on DEVELOPMENT/DEBUG kernels
  (`bsd/vm/vm_unix.c:2491`).
- **Even there, Metal refuses GPU access to 4 KiB-granular backing.** The Rosetta process builds a
  correct 4 KiB-scattered alias, and `newBufferWithBytesNoCopy` returns nil for it however it is
  aligned, while the plain control is accepted. The limit is below the address space. That is
  consistent with the GPU's IOMMU working in 16 KiB pages, which Asahi Linux reports as the reason
  M1 cannot run 4 KiB-page Linux with a working IOMMU. So no host-side remapping trick can present
  4 KiB-scattered guest pages to the GPU.
- **`hv_vm_map` with the 4 KiB IPA granule accepts host addresses that are 4 KiB- but not
  16 KiB-aligned.** Three consecutive 4 KiB pieces of one host buffer mapped at scattered guest
  addresses (`% 16K` = 0x1000, 0x2000, 0x3000) all returned `HV_SUCCESS`. This was checked at map
  time only: no vCPU read through those mappings.

## Conclusions

- **The host half works as designed.** Remapping with `copy=FALSE` into a reserved range keeps one
  set of pages, even for `MAP_PRIVATE` RAM that HVF already maps. That holds whether or not the
  pages were faulted in before the remap. Metal wiring the alias does not privatise it. Guest
  stores, GPU stores and host CPU reads through either mapping all agree, round after round, on
  the same buffer. The guest stays unharmed after teardown.
- **Aliasing only serves 16 KiB guests, and the floor is the GPU, not just the remap.** The
  worker's address space has 16 KiB pages, so an alias is built out of whole host pages and
  scattered 4 KiB guest pages cannot sit side by side in one. A 4 KiB userptr would also expose the
  other 12 KiB of its host page to the GPU, and that memory is unrelated guest memory. A 4 KiB
  address space would not rescue it: native arm64 cannot get one, and where one exists (Rosetta)
  Metal still refuses 4 KiB-scattered backing. The stage-2 IPA granule does not change this either:
  it governs `hv_vm_map` (guest-physical → host), not the host task's own mappings or the GPU's.
  So the alias route needs the guest to report a 16 KiB `minImportedHostPointerAlignment`, and the
  host must refuse any page list that is not 16 KiB-aligned.
- **A 4 KiB guest has a different candidate route: migrate instead of alias.** It needs the 4 KiB
  stage-2 granule (`spikes/hv-ipa-granule/`, limina's default). The host allocates one contiguous
  buffer, `hv_vm_unmap`s the guest's 4 KiB pages, copies them in, and `hv_vm_map`s each 4 KiB
  piece of the buffer back at the original guest addresses. The guest's pages then *are* the
  buffer, which is 16 KiB-granular backing the GPU accepts. Only the `hv_vm_map` step is measured
  here, and three things stand in the way:
  - libkrun's devices reach guest memory through a fixed linear guest → host map
    (`GuestMemoryMmap`), so device DMA into migrated pages would land in the old host pages. The
    memory model would need redirection for them.
  - vCPUs touching the pages mid-migration must wait, as the balloon's heal path does, and freeing
    the import has to reverse everything.
  - `hv_vm_map` accepts the 4 KiB-aligned host addresses that consecutive pieces of one buffer
    need, but only map-time acceptance is measured. A running guest has not yet read or written
    through such a mapping.
- **The cost is one VM map entry per non-contiguous run.** It is not per byte. A fully scattered
  128 MiB import takes 8191 entries, a 9 ms remap and 2.7 ms to wrap in a buffer. The same size in
  runs of 64 takes 128 entries and 0.2 ms. Coalescing runs is mandatory, and so is budgeting
  entries per context. A host address-space leak once grew a worker from 3.5k to 23.6k regions and
  starved it, so a guest that imports many large, fragmented userptrs is the same exhaustible
  resource. How fragmented a real guest's pinned `malloc` is remains unmeasured: it needs a real
  guest reading `/proc/self/pagemap`.

## Not covered (design hazards, not results)

- **The KosmicKrisp import itself.** That means `vkGetMemoryHostPointerPropertiesEXT` and
  `vkAllocateMemory` with `VkImportMemoryHostPointerInfoEXT` on an alias, plus the
  heap-less-tiled-image issue (KK patch 0004). This is the phase-2 spike.
- **The balloon.** Releasing a range means `hv_vm_unmap`, then optionally zeroing it, then
  `MADV_FREE_REUSABLE` on the original host range (`third_party/libkrun/src/hvf/src/released_ram.rs:96-134`).
  The alias shares those pages. A release under a live import would therefore hand the GPU pages
  the host may already have reclaimed, so the GPU reads zeros or stale bytes. Nothing would
  diverge; the import would silently decay. A pinned guest page should never be reported free, so
  this should not happen, but the host must enforce it by refusing a release that overlaps an
  import rather than trust it. Not tested either way: whether the settle sweep's `mprotect` of the
  original mapping reaches the alias.
- **Snapshot/restore.** An import would have to be journalled and re-remapped against the restored
  RAM mapping.
