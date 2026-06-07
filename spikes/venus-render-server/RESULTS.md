# Spike: venus on virglrenderer 1.3.0 render-server-thread model (macOS)

**Goal:** unblock task #25 — get upstream virglrenderer 1.3.0 Venus working on
macOS/MoltenVK (needed for zink's `VK_KHR_maintenance5`, the GL-desktop/Gap-A lever).
The prior session concluded the 1.3.0 render-server *thread* "crashes during venus
context pre-init with zero MoltenVK output → off-main-thread Metal init." Time-boxed
option **C** was to debug that crash. This spike reproduces context-create **without
booting a VM** (`harness.c` links our `third_party/virgl-prefix` 1.3.0 dylib and calls
`virgl_renderer_init(0x2C0)` + `virgl_renderer_context_create_with_flags(1, capset=4)`).

## Finding 1 — the "Metal thread crash" was a MISDIAGNOSIS (it's `O_CLOEXEC`)

`run.sh` reproduced the exact failure in seconds:
```
proxy: failed to pre-initialize context
context_create_with_flags -> 12 (ENOMEM)
server: socket disconnected
```
`"failed to pre-initialize context"` (proxy_context.c:662) is **client-side**, in our
own process, *before* anything reaches the render-server thread or Metal — so the
"socket disconnected" is a *consequence*, not a crash. Instrumenting the 4 pre-init
steps → `shmem=0` (first one fails). Instrumenting `alloc_memfd` →
`os_create_anonymous_file failed errno=22 (EINVAL)` for a 256-byte allocation.

Root cause: mesa's `os_create_anonymous_file` macOS branch calls
`shm_open(name, O_CREAT|O_EXCL|O_RDWR|O_CLOEXEC, 0600)`, but **macOS `shm_open`
rejects `O_CLOEXEC` with EINVAL** (only O_RDONLY/RDWR/CREAT/EXCL/TRUNC are valid —
confirmed with a 6-line probe). So every Venus context's proxy timeline shmem failed
to allocate. **Fix:** open without `O_CLOEXEC`, set `FD_CLOEXEC` via `fcntl()` after
(`src/mesa/util/anon_file.c`). After the fix the harness prints
`SUCCESS: venus context created` — and the render-server thread inits MoltenVK off the
main thread *fine*. There is no Metal thread bug.

## Finding 2 — zink on Venus then hit `vkCreateDevice → EXTENSION_NOT_PRESENT`

In the VM (Image-16k + Fedora, `/opt/mesa-zink` = zink + MR!37115), Venus then
enumerated with **`VK_KHR_maintenance5/6/7`** (the 1.3.0 vkr win), clearing the prior
wall. zink advanced past feature detection to `vkCreateDevice`, which failed
`VK_ERROR_EXTENSION_NOT_PRESENT`. Diffing zink's 35 requested device extensions vs
host MoltenVK's 150 → exactly 4 that venus advertises but MoltenVK lacks:
`VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`,
`VK_EXT_image_drm_format_modifier`, `VK_EXT_queue_family_foreign`.
`vkr_dispatch_vkCreateDevice` forwarded the guest's enabled-ext list verbatim to
MoltenVK. **Fix:** strip those on `__APPLE__` (`src/venus/vkr_device.c`). After it:
```
GL_RENDERER=zink Vulkan 1.2(Virtio-GPU Venus (Apple M1 Max) (MOLTENVK))
LIMINA-DIAG SUCCESS create_screen
```
**zink runs on the Apple GPU via Venus — Gap A device-creation is solved.**
(Non-fatal warnings: MoltenVK lacks `logicOp` + `custom_border_color`; zink proceeds.)

## Finding 3 — NEXT WALL: actual rendering hangs (venus async fence retire)

A surfaceless FBO clear + `glReadPixels`/`glFinish` (`/tmp/eglrender.c`) **hangs**:
context creation and `GL_RENDERER` work, but command submission + fence wait never
completes. Adding `THREAD_SYNC | ASYNC_FENCE_CB` to `GPU_VENUS_FLAGS` (→ `0x3C2`,
matching slp's coexist mask) changed the guest process from uninterruptible **D-state**
to a killable wait — improvement, but rendering still doesn't complete in 30 s. The
1.3.0 model runs Venus in a separate render-server thread, so per-context fences retire
*asynchronously* via the proxy eventfd + sync thread → `write_context_fence` →
rutabaga `fence_handler` → guest IRQ; that retirement path needs work (slp's inline
model retired on the caller thread). This is the core of task #21 "accelerated present".

## Finding 4 — #27 ROOT CAUSE: macOS has no `eventfd`, and the venus fence path needs it

Instrumented the whole fence chain (eprintln/fprintf at each link) and ran the FBO test:
- `LIMINA-FENCE [proxy] submit_fence` fires — the guest submits fences ✓
- `LIMINA-FENCE [render] retire_fence ctx=3` fires — **vkr completes the fence** on the render-server
  thread and `render_context_update_timeline` does `atomic_store(shmem seqno)` + `write_eventfd` ✓
- `write_context_fence` / `write_fence` (rutabaga) — **0×** ✗ → no `fence_handler` → no guest IRQ → hang.

Root cause: virglrenderer's proxy venus-fence completion notification is **eventfd-based**
(`proxy_context_sync_thread` polls `fence_eventfd`; the render-server signals it). But macOS has no
`<sys/eventfd.h>` → `build/config.h` has `/* #undef HAVE_EVENTFD_H */` → `create_eventfd()` returns
**-1** (`src/virgl_util.c:104`). So:
- With `THREAD_SYNC` set (`GPU_VENUS_FLAGS=0x3C2`): `proxy_context_init_fencing` → `create_eventfd`
  → -1 → "failed to create fence eventfd" → **context creation fails**. (Flag passthrough is identity:
  `VirglRendererFlags → i32` is `flags.0 as i32`; no eventfd-error in the log + working contexts ⇒
  THREAD_SYNC is effectively *not* engaged today.)
- Without it (effective `0x2C0`): `init_fencing` returns early, **no sync thread, nothing polls** the
  shmem seqno → fences submit + the render-server completes them, but they are never retired to the
  guest → `glFinish`/`glReadPixels`/`vkQueueWaitIdle` hang forever.

**Either way the eventfd absence is the blocker.** FIX (next): emulate `eventfd` on macOS in
`src/virgl_util.c` — a single pollable fd that can be passed via SCM_RIGHTS and triggered from the
render-server thread (kqueue `EVFILT_USER`, or a self-pipe variant), make `has_eventfd()` true, then
enable `THREAD_SYNC | ASYNC_FENCE_CB`. Alternative: a libkrun poll-thread that periodically calls the
shmem-polling `retire_fences` (no eventfd) — simpler but wasteful. (Secondary: a `CtxCreate →
ComponentError(22)=EINVAL` cascade appears for some contexts — likely the expected capset-2 GL probes
under NO_VIRGL; disambiguate with per-ctx-id logging when fixing fences.)

## Finding 5 — eventfd FIX landed; fence retirement works end-to-end (commit de126b3)

Implemented the macOS eventfd as a **kqueue + EVFILT_USER** shim in `virgl_util.c`. New wall hit:
`proxy: failed to send message: Invalid argument` — **macOS `sendmsg()`/SCM_RIGHTS cannot transfer a
kqueue fd**. Fixed by passing the fence eventfd **by value** in the OP_INIT request under
`ENABLE_SAME_PROCESS_RENDER_SERVER` (the render server is a thread in our process — shared fd table; the
shmem fd still goes via SCM_RIGHTS since it's shm). The render side reads the value and does not close it.

Result (with `GPU_VENUS_FLAGS=0x3C2`): the full chain now fires —
`[proxy] submit_fence → [render] retire_fence ctx=3 → write_context_fence ctx=3 → XXX fence called →
XXX found fence` (guest `signal_used_queue` IRQ delivered). **Confirmed on a real app's Venus context**
(vkr names it `context 3 (glmark2-wayland)`). The #27 fence-retirement blocker is fixed.

## Finding 6 — #27 RESOLVED: GL renders end-to-end on the Apple GPU (Venus feedback was the hang)

The remaining `glFinish`/`glmark2` stall was **not** the ring-fence path (Finding 5 fixed that). It was
Venus's **feedback** mechanism, a *second*, independent sync path. Venus has two ways to retire work:
1. **virtio-gpu per-context ring fences** — proxy `submit_fence` → render-server `retire_fence` → eventfd
   → proxy sync thread → `write_context_fence` → rutabaga `fence_handler` → guest IRQ. (Finding 5's fix.)
2. **Feedback buffers** — `vn_WaitSemaphores`/`vn_update_sync_result` (vn_queue.c) **poll a host-written
   counter in a host-visible blob** with `nanosleep` backoff. zink's `glFinish` uses THIS (vkWaitSemaphores),
   not ring fences.

A staged `eglrender` localised the hang to `vn_update_sync_result`'s poll loop — the guest sat in
`hrtimer_nanosleep` (userspace poll), never a DRM ioctl. The host-visible feedback blob is **not coherent
on the 16 KiB `hv_vm_map` path** (the guest never sees the host's counter write), so the poll spins forever.

**Fix/workaround:** `VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback`
forces Venus onto path (1) — which our eventfd shim retires correctly. With it:
```
GL_RENDERER = zink Vulkan 1.2(Virtio-GPU Venus (Apple M1 Max) (MOLTENVK))
glmark2-wayland: 18/18 scenes render, FPS 32–51, glmark2 Score ~40   (llvmpipe scores ~5 → real GPU)
glFinish / glReadPixels / vkQueueWaitIdle all return, guest_exit=0    (no hang)
```
**zink→Venus→MoltenVK→Metal renders the GL desktop on the Apple M1 Max GPU. Gap A / task #27 done.**
The flags are wired into `scripts/venus-gl-test.sh`.

### Open follow-ups (not blockers)
- **Host-visible blob coherency on the 16 KiB `hv_vm_map` path** is the *real* root cause behind both the
  feedback hang (worked around above) and the black `glReadPixels` (`PIXEL=0,0,0,0`). Rendering output and
  the feedback counter are GPU/host-CPU writes the guest doesn't observe through its mapped view. Fixing it
  re-enables Venus's faster feedback path AND fixes readback/`TRANSFER_FROM_HOST_3D`. Likely a
  MoltenVK managed-vs-shared storage / cache-invalidate or a `hv_vm_map` cache-attribute issue.
- **Perf:** a single windowed VM measured Score ~40; an earlier run measured ~445. The 445 was not cleanly
  reproduced (warm guest shader cache and/or fewer competing VMs — at one point **11 stale capture VMs**
  from un-cleaned `run-enhanced.sh` runs were contending for the GPU). Treat ~40 as the verified floor and
  investigate the window present-path vs cache warmth before quoting a headline number.
- `CtxCreate→ComponentError(22)` spam is the EXPECTED capset-2 GL-probe rejection under NO_VIRGL — ignore.

NOTE: all debug instrumentation has been reverted; `GPU_VENUS_FLAGS=0x3C2` is now correct + required and
kept (uncommitted, limina side).

## Reproduce
- `bash spikes/venus-render-server/run.sh` — context-create harness (host-only, fast).
- `bash spikes/venus-render-server/run.sh lldb` — same under lldb.
- Build the 1.3.0 prefix first: `scripts/build-virglrenderer.sh`.
