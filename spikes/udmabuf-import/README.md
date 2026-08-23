# udmabuf import — probes

Guest-side oracles for the path a GStreamer `glupload` frame takes on the
enhanced tier: a memfd wrapped by `/dev/udmabuf`, PRIME-imported into
virtio-gpu, and typed on the host by `VIRGL_CCMD_PIPE_RESOURCE_SET_TYPE`. When
any link in that chain breaks the host resource stays *untyped*, the guest's
`CREATE_SAMPLER_VIEW` on it is rejected as an illegal resource, and the whole
GL context is poisoned — every later `SUBMIT_3D` fails and the player's window
shows whatever was in memory.

| file | what it does |
|---|---|
| `resinfo-probe.py` | Raw-ioctl walk of memfd → `UDMABUF_CREATE` → `PRIME_FD_TO_HANDLE` → `RESOURCE_INFO`. Prints the `blob_mem` Mesa gates SET_TYPE on. Needs nothing installed. |
| `resinfo-probe.c` | Same, in C, via libdrm. |
| `virtgpu-trace.c` | `LD_PRELOAD` tracer: logs each PRIME import and each `RESOURCE_INFO` a real app makes. |
| `blobmem-shim.c` | `LD_PRELOAD` shim that *forces* `blob_mem=GUEST` on imported handles, to validate a kernel-side fix before building a kernel. |

## Reading the traces

- `blob_mem 0` from `RESOURCE_INFO` ⇒ the guest kernel never recorded the blob
  kind on the imported object. Mesa then treats it as a classic resource and
  never emits SET_TYPE.
- **One `RESOURCE_INFO` for two (or three) `PRIME_FD_TO_HANDLE`** is the
  multi-planar signature: NV12/I420 planes share a dmabuf, so only the first
  plane through the winsys allocates the `virgl_hw_res`.

Whether SET_TYPE actually reached the host is read in the **worker log**, not
in the guest — the guest-side command stream proved unreliable to decode.

## Fixes these probes drove

- `drm/virtio: type and attach PRIME-imported dmabufs` (kernel fork) — set
  `bo->blob_mem` on import, and give the dma-buf GEM funcs `.open`/`.close` so
  the resource is attached to the render context.
- `virgl: report blob_mem for a resource that was already imported`
  (mesa `limina-guest`) — the multi-planar cache-hit path left `*blob_mem` at 0,
  and planes import in reverse order, so plane 0 — the only one allowed to emit
  SET_TYPE — was always the cache hit.
- `vrend: sample guest-memory blobs from the guest's own pages`
  (virglrenderer) — macOS has no dmabuf to alias, so fill the texture from the
  guest iovecs and re-read before each command batch that samples it.
