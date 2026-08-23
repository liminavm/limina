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
- `virgl: let planar YUV formats be looked up in the sampler bitmask`
  (mesa `limina-guest`) — `util_format` gives a planar format no channels, so the
  generic checks rejected NV12/I420 outright and the frontend fell back to
  importing each plane as its own resource. That layout is not expressible: a
  sampler view carries only a format, and I420's two chroma planes are identical
  in format and size, so the host cannot tell U from V.
- `vrend: sample guest-memory blobs from the guest's own pages` +
  `vrend: sample a planar-YUV guest blob by converting it to RGBA`
  (virglrenderer) — macOS has no dmabuf to alias, so fill the texture from the
  guest iovecs and re-read before each command batch that samples it; planar
  formats are advertised sampler-only and converted to RGBA on the way in.

## Measured green (2026-08-23)

Kernel `7.1.8-limina16k.4`, mesa `26.1.7-3.limina.fc44`, virglrenderer `f04641d7`,
via `gl-upload-oracle.sh 1280x720`:

| format | SET_TYPE seen at the host | PNG | verdict |
|---|---|---|---|
| reference (no GL in the pipeline) | — | 34820 B | the baseline SMPTE frame |
| RGBA | `fmt 67 planes 1` | 32675 B | correct |
| NV12 | `fmt 166 planes 2 stride0 1280 off0 0 stride1 1280 off1 921600` | 30681 B | correct |
| I420 | `fmt 165 planes 3 stride0 1536 off0 0 stride1 768 off1 1105920` | 30681 B | correct |

A 20-frame NV12 run gives 20 distinct md5s with the ball visibly advancing, so the
per-batch re-read is picking up new content and not caching the first frame.

**Still unexplained, parked:** `glupload ! gldownload` at *identical* caps issues no
transfer and no draw, so it is not an oracle — the scaling step in
`gl-upload-oracle.sh` is what forces the frame through the GPU. First suspect if a
readback-shaped path ever misbehaves.
