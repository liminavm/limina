# Fence-accurate presents (#8) — facts + design

Groundwork for replacing LIMINA_PRESENT_COPY with a correct zero-copy present.
Driven by the round-21 conviction (present-before-GPU-complete) and the lock-only
failure (immutability, not sync, is the load-bearing property — see RESULTS.md).

## Verified facts (path:line into the trees we own)

### Guest kernel (target/test-guest/kernel/cache/linux-v6.12.git, bare repo — use `git cat-file -p HEAD:<path>`)

- **Blob scanouts are unfenced.** `virtgpu_plane.c` `virtio_gpu_plane_prepare_fb`
  allocates `vgfb->fence` only for `bo->dumb` (2D). 3D blob resources send
  `SET_SCANOUT_BLOB` + `RESOURCE_FLUSH` with **no fence** and no wait of any kind.
- When a fence IS present (dumb path), `virtio_gpu_resource_flush`
  (`virtgpu_plane.c:38-67`) passes it to the flush command and synchronously
  `dma_fence_wait_timeout(..., 50ms)` — i.e. the atomic commit **blocks** until the
  host signals the flush fence. This is the hold mechanism we can reuse.
- **Flip-completion events are FAKE.** virtio-gpu has no vblank; `no_vblank=true`
  makes `drm_atomic_helper_fake_vblank` (`drm_atomic_helper.c:2453-2473`) send the
  event at commit-tail time, unconditionally. The only way the host influences flip
  timing is the synchronous fence wait above (it delays commit-tail, hence the fake
  event). Delaying the virtio command *response* holds nothing — the guest never
  waits for it.
- **No implicit sync anywhere.** venus userspace: "implicit fencing is broken (and
  there is no explicit fencing support yet)" (`vn_wsi.c:48-52` in mesa). The kernel
  does not wait on the FB's dma-resv before flushing. The guest simply assumes
  rendering is done at flush time. (It isn't — that's bug #31.)

### libkrun (third_party/libkrun, src/devices/src/virtio/gpu/)

- `flush_resource` (`virtio_gpu.rs:741-817`) presents the scanout IOSurface to the
  display backend **at flush receipt** — mutter's submit time. This is the bug site.
- Fence routing (patch 0010): global-ring fences retire synchronously
  (`mark_fence_completed_sync`); context-ring fences route through rutabaga →
  `write_context_fence` callback (`virgl_renderer.rs:194-210`) → fence handler
  (`virtio_gpu.rs:237-291`) retires used-ring descriptors. A blob flush fence (once
  the kernel attaches one) arrives as a **global-ring** fence today → would retire
  instantly. It must instead be parked until present-complete.

### virglrenderer/vkr (third_party/virglrenderer, src/venus/)

- Context fences are **true GPU completion**, not decode completion:
  `vkr_queue_sync_submit` (`vkr_queue.c:78-112`) does a zero-command
  `vkQueueSubmit(queue, 0, NULL, sync->fence)`; the per-queue sync thread
  (`vkr_queue.c:150-196`) blocks in `vkWaitForFences` and only then calls
  `retire_fence`. So vkr already knows how to wait for "everything submitted to this
  VkQueue so far has executed".
- There is **no scanout-specific completion hook** — fences are per-context ring,
  created by guest command. Nothing host-initiated exists yet.

### KosmicKrisp (/Volumes/mesa-cs/mesa/src/kosmickrisp/)

- `kk_queue_submit` → one MTLCommandBuffer per submit; `kk_encoder_submit`
  (`kk_encoder.c:265-273`) already registers `addCompletedHandler` (true Metal GPU
  completion). VkFence/VkSemaphore = MTLSharedEvent signaled by GPU-encoded events
  (`kk_sync.c`), so vkr's `vkWaitForFences` genuinely tracks GPU completion.

### Ordering fact that makes host-side injection sound

Mutter's frame: GL work → zink → venus ring writes (CPU stores into the ring buffer,
complete before the KMS ioctl returns to mutter) → atomic commit → virtio
`RESOURCE_FLUSH`. So at flush-receipt time the repaint's commands are **in the venus
ring buffer**, though the vkr ring thread may not have decoded them yet (this is
exactly why IOSurfaceLock-at-present failed: nothing submitted to Metal yet to wait
on). Any host-injected wait must be ordered **through the vkr ring**, behind the
pending decode — then it transitively orders behind the Metal submit.

## Design

Two halves, mapping to the two races; clean two-tier split.

### Half 1 — present at GPU completion (host-only, no guest change)

Mechanism (virglrenderer, upstreamable): a new vkr entry point, roughly
`virgl_renderer_context_flush_fence(ctx_id, cookie)` that enqueues — **on the ring
thread's timeline** (so it orders after currently-buffered decodes) — a
zero-command queue submit + sync on each VkQueue of the context, and fires a
callback when the last one retires. (Implementation detail: reuse
`vkr_queue_sync_submit` machinery with a synthetic fence id + dedicated callback,
NOT guest fence ids — those are guest-managed and must not interleave.)

Policy (libkrun): `flush_resource` on an IOSurface scanout doesn't present; it asks
the resource's owning context for a flush fence and presents from the callback
(worker → control fd "frame" message, unchanged downstream). Frames collapse if a
new flush arrives before the previous fence retires (show latest).

This alone fixes the **convicted** race (a complete frame is always what CA samples
at latch time). The reuse race (race 2) remains theoretically open — guest can
repaint the buffer while CA still samples — but it's the marginal SURFACE_RING-class
overlap, never yet observed in isolation.

### Half 2 — buffer hold + honest pacing (enhanced-tier kernel patch)

Kernel patch (small, upstreamable): in `virtio_gpu_plane_prepare_fb`, allocate
`vgfb->fence` for blob scanout FBs too (drop the `bo->dumb` condition for the
primary plane), so `RESOURCE_FLUSH` is fenced and the existing
`dma_fence_wait_timeout` blocks the commit until the host signals.

Host policy: park the blob-flush fence (it's global-ring — needs routing past
`mark_fence_completed_sync`, keyed on "resource is a live scanout") and signal it
only when the presented frame is **safe to overwrite**: present done + CA latched
(in practice: the supervisor's next timer tick after the CATransaction commit, or a
one-vsync delay). This holds mutter's commit-tail → fake flip event → mutter's
pacing becomes host-paced (honest), and the buffer cannot be repainted while CA
samples it. Bonus: this is the same mechanism that fixes the kmscube stall and
gives mutter real frame pacing.

Stock tier (no kernel patch): unfenced blob flushes behave exactly as today →
PRESENT_COPY remains the stock-tier mitigation. Enhanced tier gets fence-accurate
zero-copy. Two-tier guarantee preserved.

### Experiment ladder

1. Implement Half 1; A/B with the recording + scan-anomalies method (long window).
   If clean → copy can default off on the zero-copy path; keep as fallback knob.
2. If rare anomalies persist (race 2 is real), implement Half 2 and re-run.
3. Either way Half 2 is wanted eventually for honest pacing (kmscube, mutter
   frame clock); schedule by value, not by flicker.

### Open questions for implementation

- vkr: which queues to sync — all `vkr_queue`s of the context, or track the queue
  that last submitted? (Start: all queues with pending syncs; zink uses one.)
- 50ms `dma_fence_wait_timeout` in the dumb path: enough headroom for
  present+latch? (60Hz tick ≈ 16.7ms — yes, with margin; never park >1 tick.)
- Multi-scanout: park/signal per scanout id.
- The readback (software-2D) path already copies — untouched by all of this.
