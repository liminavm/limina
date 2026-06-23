# Runtime display resize (M8 — "the #1 display gap")

**Goal:** dragging the limina window edge reflows the guest desktop to the new resolution, no
reboot. Resizing the NSWindow updates the guest's preferred mode and the guest re-modesets.

**Decisions (2026-06-23, with user):**
- **Modeset fires at the END of a live resize** (`windowDidEndLiveResize`), one per gesture — a guest
  modeset is expensive (mutter reconfigures everything); the window shows the old surface scaled
  during the drag, snaps crisp on release. Matches GNOME Boxes / virt-manager.
- **RED-first test = L1 sysfs mode check** (fast; proves host→guest mechanism without a display
  server).

## The correct flow (resize goes THROUGH the guest — not a host-side buffer swap)

A host-side "just reconfigure the worker buffers" shortcut is WRONG: it leaves the guest rendering at
the old resolution into a differently-sized host buffer. The guest must re-modeset:

```
NSWindow drag-end (supervisor)
  → supervisor sends "resize W H" over the display socketpair  (reverse direction; worker adds a reader)
  → worker (limina-vmm) calls the libkrun DisplayResizeHandle
  → libkrun: update displays[id] dims (+regen EDID), set events_read |= VIRTIO_GPU_EVENT_DISPLAY,
             raise a virtio-gpu CONFIG-CHANGE interrupt
  → guest virtio-gpu DRM driver: config-change → re-read GET_DISPLAY_INFO → new preferred mode
             → drm_kms_helper_hotplug_event → GNOME/mutter re-modesets
  → guest: CREATE_2D + SET_SCANOUT at W×H
  → backend configure_scanout(new dims) fires (ALREADY reallocates staging/canvas/ring/stride)
  → worker sends "surface id0 id1 W H" → supervisor reconciles window + clears lookup cache (ALREADY)
```

The hard parts already exist: `configure_scanout` reallocates everything on a geometry change
(`crates/limina-display/src/iosurface.rs:148-188`), and the supervisor already handles a guest-driven
mode change (`crates/limina/src/window.rs:388-415`). **The only missing piece is the runtime
trigger** — libkrun never raises a config-change for the GPU device today (`device.rs:194`
`events_read: 0` hardcoded; `interrupt.try_signal_config_change()` exists at `mmio.rs:147` but is
unused by the GPU device).

## Key architectural point: internal Rust API, no C ABI

limina consumes libkrun via the internal Rust crates (see memory `limina-key-findings`), so there is
**no need for a `krun_display_resize` C call reaching a live device**. limina-vmm holds a Rust handle
directly. Design the trigger as a `DisplayResizeHandle` the GPU device exposes at construction.

## Layer 1 — libkrun GPU device (new patch under `patches/libkrun/`)

Files: `src/devices/src/virtio/gpu/{device.rs, worker.rs, virtio_gpu.rs}`,
`src/devices/src/virtio/mmio.rs` (config-change signal already there).

- **Shared state created in `Gpu::new`:**
  - `events_read: Arc<AtomicU32>` — the config-space events field.
  - `resize_evt: EventFd` (EFD_NONBLOCK) — wakes the worker's epoll for a pending resize.
  - `resize_state: Arc<Mutex<Option<(display_id, w, h)>>>` — the pending request.
- **Expose `Gpu::display_resize_handle() -> DisplayResizeHandle`** = clones of {`resize_evt`,
  `resize_state`, `events_read`}, plus `num_displays`. Method `request(id,w,h)`: validate id<num,
  store in `resize_state`, `resize_evt.write(1)`.
- **`read_config`** (device.rs:192): return `events_read: self.events_read.load(Relaxed)` instead of
  0. **`write_config`** (currently just warns): at the `events_clear` offset, clear those bits
  (`events_read.fetch_and(!clear)`) — that's the guest's ack.
- **Worker** (`worker.rs`): take `resize_evt` + `resize_state` + `events_read`; add `resize_evt` to
  the service-loop epoll (alongside control/cursor/stop/present). On wake: read the pending
  `(id,w,h)`, update `virtio_gpu` displays (new method below), `events_read.fetch_or(DISPLAY)`, then
  `interrupt.try_signal_config_change()`. (Inactive case: the eventfd stays hot → applied on next
  `service()`; the next GET_DISPLAY_INFO returns the new dims regardless.)
- **`VirtioGpu`** (`virtio_gpu.rs`): the `displays` Box is cloned per-worker and used by
  `display_info()` (:1370) + `get_edid()` (:1380). Add `fn set_display_size(id, w, h)` that mutates
  `self.displays[id].{width,height}` and regenerates the EDID blob. (Decide: share `displays` as
  `Arc<Mutex<…>>` vs. keep the worker's clone and mutate it in-thread. In-thread mutation via the
  WorkerCmd/eventfd is simpler and avoids a lock on the hot display_info path — prefer it.)
- `VIRTIO_GPU_EVENT_DISPLAY = 1<<0` already at `protocol.rs:482`.

## Layer 2 — worker (limina-vmm)

- The krun facade (`crates/limina-vmm/src/krun/mod.rs`) obtains the `DisplayResizeHandle` from the Gpu
  device after building it and stashes it.
- A reader thread on the display control socketpair (worker end) parses `resize <w> <h>` lines and
  calls `handle.request(0, w, h)`. (Today the display socketpair is worker→supervisor write-only,
  `main.rs:229`; add the reverse-direction reader.)

## Layer 3 — supervisor (crates/limina/src/window.rs)

- Install an `NSWindowDelegate`; implement **`windowDidEndLiveResize:`** → content size × `backingScaleFactor`
  = guest pixels → write `resize <w> <h>` to the worker over the socketpair.
- Also add a `--display-control-socket <path>` to `limina` (supervisor): a unix socket that accepts
  `resize W H` from an external controller and forwards it the same way. This is the **test/automation
  back-door** AND a useful scripting hook; the NSWindow delegate and this socket funnel into one
  internal `forward_resize(w,h)`.
- **Feedback-loop guard:** host resize → guest resize → `surface W H` msg → `setContentSize` →
  would re-fire `windowDidEndLiveResize`. Track last-sent vs last-applied size and suppress the echo.

## Layer 4 — RED-first test (L1 sysfs)

- `Guest::resize_display(w,h)` harness hook → connects to the supervisor's `--display-control-socket`
  and writes `resize w h`.
- `crates/limina-test/tests/l1_resize.rs`: boot L1 with a display (virtio-gpu present) + the console
  shell; read `/sys/class/drm/<card>-Virtio-*/modes` (discover the connector), assert initial mode;
  `resize_display(W,H)` to a **non-standard** size (e.g. 900×650, not in virtio-gpu's built-in common
  list, so its appearance proves the host preferred mode propagated); poll modes until the new size
  appears (config-change → connector re-probe updates sysfs without a display server). RED before the
  layers exist; GREEN after.
- Add to `scripts/test-boot.sh` as `--test l1_resize`. (Verify the L1 kernel has `CONFIG_DRM_VIRTIO_GPU`
  + a connector that exposes `/sys/.../modes`; `l1_display` already draws to `/dev/fb0` so the DRM
  device is present.)

## Then: windowed human-verification

Boot `limina --window` (Fedora), drag the window, confirm GNOME reflows to the new size and the
present path stays clean (no tear/stale-surface). Verify the feedback-loop guard doesn't oscillate.

## Map citations (point-in-time; re-verify before editing)

- Host: NSWindow `window.rs:256-281`; control protocol `window.rs:175-231`; guest-driven resize
  reconcile `window.rs:388-415`; IOSurface ring `iosurface.rs:49-188`; stride=`width*4`
  `iosurface.rs:484`; worker display sink `limina-vmm/src/main.rs:226-233`,
  `krun/mod.rs:170-210`.
- libkrun: `DisplayInfo` `gpu/display.rs:8-24`; GET_DISPLAY_INFO `gpu/virtio_gpu.rs:1370-1377`;
  GET_EDID `:1380-1387`; SET_SCANOUT `:839-851`; `virtio_gpu_config` `gpu/mod.rs:35-41`;
  read/write_config `gpu/device.rs:192-220`; worker loop + WorkerCmd `gpu/worker.rs:37-49,225-278`;
  device worker_tx/stopfd `gpu/device.rs:56-66,222-305`; config-change signal `virtio/mmio.rs:147-162`;
  `VIRTIO_GPU_EVENT_DISPLAY` `gpu/protocol.rs:482`.
