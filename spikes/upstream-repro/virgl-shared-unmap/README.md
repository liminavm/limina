# virgl: settle a CPU write into a shared resource before unmap returns — NOT sendable

**Patch.** After a CPU write to a `PIPE_BIND_SHARED` texture, `virgl_texture_transfer_unmap` flushes
the transfer queue and waits for the resource to go idle, so a consumer in another context cannot
read the buffer's previous contents.

**Not reproducible on an upstream stack.** `virgl-shared-unmap.c` writes a new value through
`gbm_bo_map` each iteration and reads it back from a second virgl context (separate gbm device +
EGL display, dma-buf imported as an `EGLImage` and read through a framebuffer). On stock QEMU +
virglrenderer every read sees the value just written, with or without the patch: the control queue
executes the producer's transfer before the consumer's read.

The staleness this patch fixes needs a consumer that executes outside that order — on limina, a
venus context importing the GBM buffer, whose commands run on the renderer's own ring thread while
the transfer waits on the control queue. Upstream vkr cannot import a virgl resource at all
(`vkr: failed to import resource: invalid res_id`, see `../venus-dmabuf-import/`), so that consumer
does not exist upstream. The patch stays a limina carry; revisit it if upstream vkr gains virgl
resource import.

    cc -o virgl-shared-unmap virgl-shared-unmap.c $(pkg-config --cflags libdrm) -lgbm -lEGL -lGLESv2
    ./virgl-shared-unmap

## Results

| Mesa | Setup | Result |
|---|---|---|
| `main` b39d173ca93 | QEMU guest, virgl on Intel Iris Plus G7, virglrenderer 1.3.0 | 0 of 20 reads stale |
| `main` + patch | same | 0 of 20 reads stale |

Measured 2026-10-05.
