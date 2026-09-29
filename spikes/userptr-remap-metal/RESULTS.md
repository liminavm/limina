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

After teardown (drop the buffer, `mach_vm_deallocate` the alias), the region count returns to
within +13..+14 of the pre-alias count. That residue is the same for 256 and 8192 pages, so it is
Metal's own allocations and not the alias. The guest's pages then fill and check correctly through
both the guest and the VMM's mapping.

### The 4 KiB questions

- Metal on this OS accepts pointers and lengths finer than the host page, and reads them correctly.
  - `newBufferWithBytesNoCopy(alias + 4096, 16 KiB)` returns a buffer, and a GPU read-back matches
    the guest pages word for word, including across the page boundary into the next alias page.
  - `newBufferWithBytesNoCopy(alias, 4 KiB)` returns a 4096-byte buffer that also reads back
    correctly.
- So Metal is **not** what forces 16 KiB. KosmicKrisp's
  `minImportedHostPointerAlignment = os_page_size` (`kk_physical_device.c:789`) is stricter than
  Metal needs.
- `mach_vm_remap` is what forces 16 KiB. Remapping 4 KiB that starts 4 KiB into a guest page
  returns a host-page-aligned address, and the new mapping exposes **the whole 16 KiB page**
  (2048/2048 words match the guest page).

## Conclusions

- **The host half works as designed.** Remapping with `copy=FALSE` into a reserved range keeps one
  set of pages, even for `MAP_PRIVATE` RAM that HVF already maps. That holds whether or not the
  pages were faulted in before the remap. Metal wiring the alias does not privatise it. Guest
  stores, GPU stores and host CPU reads through either mapping all agree, round after round, on
  the same buffer. The guest stays unharmed after teardown.
- **A 4 KiB guest cannot have this, because of the remap rather than Metal.** An alias is built out
  of whole 16 KiB host pages, so scattered 4 KiB guest pages cannot sit side by side in one. A
  4 KiB userptr would also expose the other 12 KiB of its host page to the GPU, and that memory is
  unrelated guest memory. The feature stays on the 16 KiB enhanced tier. The guest must report a
  16 KiB `minImportedHostPointerAlignment`, and the host must refuse any page list that is not
  16 KiB-aligned.
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
- **The balloon.** libkrun's `released_ram` replaces released ranges with fresh
  `MAP_FIXED|MAP_ANON` mappings. An alias taken before such a replacement keeps the *old* pages, so
  the guest and the GPU would silently diverge. A pinned guest page should never be reported free,
  so this should not happen, but the host must enforce it rather than trust it. The settle sweep's
  `mprotect` of the original mapping does not reach the alias, which has its own protection.
- **Snapshot/restore.** An import would have to be journalled and re-remapped against the restored
  RAM mapping.
