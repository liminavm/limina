# KWin with GL compositing never finished starting: virglrs never completed a waited-on GL query

Fixed by virglrs `96a7b3d` and libkrun `e493221a` (see *The fix*). Before them, on
`Fedora-Workstation-44.enhanced.kde.raw`, Plasma 6.7 (KWin 6.7.5,
DRM backend, GL compositing) on the coexist venus/virgl stack never finished starting. The screen
stayed black with a cursor (`evidence-2026-10-03/last-frame.png`). It reproduced across a reboot.

## What is known

- **plasmashell and kded6 time out behind KWin.** Their systemd units restart every ~40 s. KWin's
  D-Bus does not answer: `busctl get-property org.kde.KWin /KWin ...` times out.
- **KWin is not stuck on one lost fence.** Every sample of its main thread is in
  `virtio_gpu_wait_ioctl -> dma_resv_wait_timeout`, but the guest's virtio-gpu fence counter keeps
  advancing at about 15,000 fences/s (`virtio-gpu-irq-fence`: 689721, 1400621, 2137887 at 48 s
  intervals), and the host has nothing parked; the stale-fence watcher never fired. KWin is
  looping submit-and-wait.
- **On the host, the moment KWin's first frame goes out** (22:44:46 in
  `evidence-2026-10-03/worker.log`) there is one `present fence injection failed (resource 5):
  rutabaga component failed with error -22; presenting now` (rolled back, by design) and a 186 ms
  control-queue drain. Then the log is quiet.

## Cause: virglrs never completes a GL query that was not ready when first asked

Reproduced on a clone of the image with GL compositing back on. `strace` of KWin's main thread
shows nothing but `DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST` / `DRM_IOCTL_VIRTGPU_WAIT` pairs on one
buffer, about 6,000 a second. Three gdb samples all show the same stack:

```
virgl_drm_resource_wait <- virgl_resource_transfer_map <- virgl_get_query_result
  <- get_query_object <- KWin::GLRenderTimeQuery::query() <- KWin::OutputFrame::presented
  <- KWin::DrmAtomicCommit::pageFlipped <- drmHandleEvent
```

After each page flip KWin reads its GPU render-time query with a blocking
`glGetQueryObject(GL_QUERY_RESULT)`.

- **Guest Mesa** encodes `VIRGL_CCMD_GET_QUERY_RESULT` once, at end-query time
  (`virgl_query.c`, mesa-guest). To read the result it waits for the query buffer to go idle and
  then re-transfers the buffer until the host has written `VIRGL_QUERY_STATE_DONE`
  (`virgl_get_query_result`, the `while (host_state->query_state != VIRGL_QUERY_STATE_DONE)`
  loop). It never asks again.
- **C virglrenderer** parks a query that is not ready on `waiting_query_list`
  (`vrend_get_query_result`, `vrend_renderer.c:13680-13698`). It re-checks that list before
  retiring fences (`vrend_renderer_check_queries`, called at `:13294` and from
  `vrend_renderer_poll`), so the result is in the buffer by the time the guest's wait returns.
- **virglrs** has no such list. Its `get_query_result` (`src/vrend/context.rs`, "a result that
  is not ready is left for a later poll, which the guest makes by asking again") checks
  `GL_QUERY_RESULT_AVAILABLE` once and writes nothing if it is not set. The guest never asks
  again, so the buffer never says DONE and a blocking read spins forever.

The bug is not specific to KWin or to timer queries: any guest GL client that blocks on a query
result not ready at end-query time hangs the same way, occlusion queries included. Non-blocking
polls (mutter's frame-timing queries, for one) never hang; they just never get an answer.

## The fix

- **virglrs `96a7b3d`** parks a query that is not ready, holds the context fence behind it, and
  answers the query from `Renderer::poll()` on the renderer thread before that fence retires.
  The VMM pumps it through `Renderer::poll_descriptor()`.
- **libkrun `e493221a`** hands that descriptor to the GPU worker's epoll and calls `poll()` when
  it is readable. The snapshot and reset fence drains pump it too, because they hold the worker
  thread while they wait for fences.

Verified 2026-10-03 on the KDE image with GL compositing: KWin reports "Compositing Type: OpenGL"
on `virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))` at under 1% CPU, the Plasma desktop
renders (window capture), and the worker log has no parked-fence reports.

## Evidence

`evidence-2026-10-03/`:
- `worker.log`: host worker and supervisor, host paths scrubbed;
- `guest-journal.txt`: full guest journal of the hung boot;
- `guest-state.txt`: KWin's kernel stack, the fence counter, DRM clients and state, package
  versions;
- `last-frame.png`.
