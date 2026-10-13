# virtio-gpu-limina — the guest kernel half of the Metal-level GPU transport

The in-tree `virtio_gpu` DRM driver plus limina's native-context extension, shipped out of tree so
a stock guest kernel can take it (the design is `spikes/metal-heap-addressing/SKETCH.md`).

- `virtgpu_limina.c` — the extension: negotiates `VIRTIO_GPU_F_LIMINA_EXT` (bit 23), finds the
  arena window (shm id 0x80) and the doorbell window (shm id 0x81), and gives each open file one
  context with its doorbell page and arenas, mapped by `mmap()` on the same file.
- `virtgpu_limina_drm.h` — its UAPI: ioctls at `DRM_COMMAND_BASE + 0x20..0x26`, clear of
  upstream's. Besides contexts, arenas and rings, shared arenas (exported as GEM objects, so they
  travel as dma-bufs, and imported into other contexts) and fences for a context's commits.
- `0001-hook-the-limina-extension.patch` — the only change to upstream files: the feature bit,
  the ioctl table, the device fields, `mmap` routed through the extension, and two helpers in
  `virtgpu_vq.c`: one sends a command and waits for its response, the other queues a fenced
  `FENCE`. It is plain `git diff` output: to change it, commit the stable tag's
  `drivers/gpu/drm/virtio/` files to a scratch git repository, `git apply` the patch, edit, and
  `git diff` again.

Without the feature bit the module behaves exactly as upstream. The host offers the bit only when
the worker runs with `LIMINA_GPU_METAL=1` (`crates/limina-vmm/src/krun/metal_ext.rs`).

Build: `scripts/build-virtio-gpu-limina.sh [KTAG] [KREL]` vendors the upstream sources of a stable
series, applies the hook, and with `KREL` compiles the module against that Fedora kernel in the
`limina-build` container. `guest/limina-gpu-smoke` exercises it from userspace.
