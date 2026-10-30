# Worker abort releasing a pooled allocator (poke VM, 2026-10-30)

The worker aborted at 18:45:18Z on Metal's own assertion, releasing a pooled command allocator
from `kk_alloc_pool_acquire()`:

```
Assertion failed: (_allocatedSize == 0), function -[IOGPUMetalDeviceShmemPool dealloc]
  -[IOGPUMetal4CommandAllocator dealloc] ← -[AGXG13XFamilyCommandAllocator_mtlnext dealloc]
  ← mtl_release ← kk_alloc_pool_acquire ← kk_BeginCommandBuffer
  ← virglrs venus begin_command_buffer ← the venus ring thread
```

So an allocator the pool counted as surplus still had memory allocated from it when it was
released. Cause not yet established.

- `limina-vmm-2026-10-30-154523.ips`: the crash report (faulting thread 87,
  `virglrs-ring-…`).
- `worker.log`: the whole worker log of that boot, private paths replaced by `<repo>`,
  `<worktree>` and `~`.

Setup: a CoW clone of `Fedora-Workstation-44.stock.test.raw` (stock kernel, 4 KiB pages), with
Fedora's Mesa 26.2.5 installed in the guest. It was booted EFI+venus from a worktree on limina
`d1d0b0ef`, with host KosmicKrisp from `/Volumes/mesa-cs/build-kk`. That `libvulkan_kosmickrisp.dylib`
was built 2026-10-22 13:52; at the time of the crash the `limina-kk` checkout was at `c40298fd0cc`
(committed 2026-10-25), so the dylib predates it and its exact rev is not recorded.

Guest workload when it aborted: Epiphany 50.6 with `MESA_LOADER_DRIVER_OVERRIDE=zink` (guest zink
on venus), restarted about three minutes earlier on the WebGL aquarium, with the user browsing.
Before that the same boot ran the same Epiphany on zink and on virgl, and the
`zink-msrtt-recursion` reproducer (15 guest SIGSEGVs).
