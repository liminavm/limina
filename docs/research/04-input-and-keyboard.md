# 04 — Input, keyboard capture, remapping, fullscreen, mouse capture

Scope: how limina gets host keyboard/mouse events from macOS into the Linux guest through libkrun's virtio-input device, and how it should handle keyboard capture (system key combos), Command/Option remapping, customizable keybindings, layout strategy, relative vs absolute pointer, mouse capture, and fullscreen. All libkrun claims below were read from the locally cloned source at `~/Projects/limina/third_party/libkrun` and are cited as `path:line`. macOS-host claims are standard platform APIs.

---

## 1. What exists today

### 1.1 libkrun input subsystem — file map (all read this pass)

| Path | Role |
|---|---|
| `include/libkrun_input.h` | Public C ABI: `krun_input_event`, config vtable, event-provider vtable. |
| `include/libkrun_display.h` | Public C ABI for the display backend (input is gated behind the GPU/display feature). |
| `include/libkrun.h:1142-1143` | The two public entry points `krun_add_input_device` / `krun_add_input_device_fd`. |
| `src/input/` (workspace member) | Rust crate bridging the C input ABI ↔ internal Rust traits. `lib.rs`, `c_to_rust.rs`, `rust_to_c.rs`, plus a vendored `libkrun_input.h`. |
| `src/devices/src/virtio/input/device.rs` | virtio-input device model: config space (select/subsel), feature negotiation, 2 queues, worker spawn. |
| `src/devices/src/virtio/input/worker.rs` | Worker thread: epoll loop, drains the event provider into the eventq; statusq is a no-op. |
| `src/devices/src/virtio/input/mod.rs` | Module constants (`VIRTIO_ID_INPUT=18`, config-select codes, queue config). |
| `src/devices/src/virtio/input/passthrough.rs` | Built-in event provider that forwards a host Linux `/dev/input/eventN` fd (used by `krun_add_input_device_fd`). |
| `examples/krun_gtk_display/src/input_backend.rs` | Reference host backend: a GTK keyboard config + a touchscreen config + a channel-fed event provider. |
| `examples/krun_gtk_display/src/input_constants.rs` | The Linux `KEY_*`/`ABS_*`/`INPUT_PROP_*` constant table + `SUPPORTED_KEYBOARD_KEYS`. |

Workspace membership confirmed: `Cargo.toml:1-20` lists `src/input` and `src/display`.

### 1.2 Public entry points (`include/libkrun.h`)

```c
// libkrun.h:722-723  — note: opaque pointer + size, NOT the header struct types
int krun_add_input_device(uint32_t ctx_id,
                          const void *config_backend, size_t config_backend_size,
                          const void *event_provider_backend, size_t event_provider_backend_size);
// libkrun.h:735
int krun_add_input_device_fd(uint32_t ctx_id, int input_fd);
// libkrun.h:707  — companion display backend setter, same opaque-ptr+size shape
int32_t krun_set_display_backend(uint32_t ctx_id, const void *display_backend, size_t backend_size);
```

**ABI subtlety:** the two `void*` args are NOT the C `krun_input_config`/`krun_input_event_provider` header structs — they are the Rust `#[repr(C)]` `InputConfigBackend`/`InputEventProviderBackend` structs from `src/input/src/c_to_rust.rs:188-198,246-254` (`{u64 features; const void* create_userdata; PhantomData (ZST); create_fn; vtable}`). The doc comment at `libkrun.h:714-717` calls them `krun_input_config`/`krun_input_event_provider` but the implementation (`lib.rs:1595-1606`) casts the pointer to `InputConfigBackend`. The size arg is a version/layout guard. So when implementing the raw C ABI, limina must lay out the **backend** struct (features + userdata + create_fn + vtable), not the header's `krun_input_config` (which lacks the explicit PhantomData/layout the Rust side expects). Using the Rust `krun_input` crate's `into_input_config`/`into_input_events` (rust_to_c.rs) avoids this footgun entirely.

Feature gating: `src/libkrun/Cargo.toml:17-18` — `gpu = ["vmm/gpu","devices/gpu","krun_display"]`, `input = ["krun_input","vmm/input","devices/input"]`; `src/devices/Cargo.toml:17-18` — `gpu = ["rutabaga_gfx",...]`, `input = ["zerocopy","krun_input"]`. Input is a **separate** feature from gpu (input does not force gpu in the Cargo graph), but `libkrun.h:710` documents that a virtio-input device needs the GPU/display present to be useful, and there is a runtime check: `krun_has_feature(KRUN_FEATURE_INPUT=4)` / `KRUN_FEATURE_GPU=2` (`libkrun.h:1109-1134`). limina should call `krun_has_feature` at startup to confirm the linked dylib has input+gpu.

You may call `krun_add_input_device` multiple times to register several devices (e.g. a keyboard, a relative mouse, an absolute tablet); each becomes its own evdev node in the guest. The vtable-based path (`krun_add_input_device`) gives full control over device identity and capabilities; the fd path (`krun_add_input_device_fd`) only makes sense on a Linux host with a real evdev fd (see §1.6).

### 1.3 The virtio-input C ABI (`libkrun_input.h`)

Error codes (`libkrun_input.h:12-15`): `KRUN_INPUT_ERR_INTERNAL=-1`, `_EAGAIN=-2`, `_METHOD_UNSUPPORTED=-3`, `_INVALID_PARAM=-4`. Feature flags (`libkrun_input.h:18-19`): `KRUN_INPUT_CONFIG_FEATURE_QUERY=1`, `KRUN_INPUT_EVENT_PROVIDER_FEATURE_QUEUE=1`.

**Event struct** (`libkrun_input.h:25-29`) — bit-compatible with Linux `struct input_event` payload:

```c
struct krun_input_event { uint16_t type; uint16_t code; uint32_t value; };
```

`value` is `uint32_t` here; the worker casts it to `i32` when writing to the virtqueue (`worker.rs:178`), so relative deltas (which are negative `s32` in evdev) round-trip correctly via two's complement.

**Event-provider vtable** (`libkrun_input.h:84-88`):

| Method | Required? | Contract |
|---|---|---|
| `destroy` | optional | free the instance |
| `get_ready_efd` | required (`:67`) | return an fd that becomes readable when events are queued; libkrun adds it to its **epoll** set |
| `next_event` | required (`:82`) | non-blocking; returns `1`=wrote one event, `0`=none available, negative=error |

**Config query vtable** (`libkrun_input.h:137-145`): `query_device_name`, `query_serial_name`, `query_device_ids`, `query_event_capabilities(event_type,bitmap,len)`, `query_abs_info(abs_axis,*absinfo)`, `query_properties(bitmap,len)`. All six are mandatory under the `QUERY` feature — `c_to_rust.rs:228-234` rejects the device if any is null.

Supporting structs: `krun_input_device_ids{bustype,vendor,product,version}` (`:93-98`); `krun_input_absinfo{min,max,fuzz,flat,res}` (`:103-109`). Top-level objects (`:147-165`): `krun_input_config{features, create_userdata, create, vtable}` and `krun_input_event_provider{features, create_userdata, create, vtable}`. `create` (`:43`) is a factory returning an opaque self-pointer passed to every later call.

### 1.4 How the device model uses the config vtable (`device.rs`)

The guest driver probes capabilities by writing a `(select, subsel)` pair into config space and reading back the result. `device.rs:54-100` (`update_select`) maps each virtio config-select to a query call:

| virtio select (`mod.rs:24-32`) | code | calls |
|---|---|---|
| `VIRTIO_INPUT_CFG_ID_NAME` | 0x01 | `query_device_name` |
| `VIRTIO_INPUT_CFG_ID_SERIAL` | 0x02 | `query_serial_name` |
| `VIRTIO_INPUT_CFG_ID_DEVIDS` | 0x03 | `query_device_ids` |
| `VIRTIO_INPUT_CFG_PROP_BITS` | 0x10 | `query_properties` (`INPUT_PROP_*` bitmap) |
| `VIRTIO_INPUT_CFG_EV_BITS` | 0x11 | `query_event_capabilities(subsel=EV_*)` → bitmap of supported codes |
| `VIRTIO_INPUT_CFG_ABS_INFO` | 0x12 | `query_abs_info(subsel=ABS_axis)` |

The config payload buffer is 128 bytes (`device.rs:116`, `ConfigPayload::bytes`). **This is the mechanism that decides what kind of device the guest sees**: report `EV_REL`(+`REL_X/Y/WHEEL`) for a relative mouse, or `EV_ABS`(+`ABS_X/Y` with absinfo) + `INPUT_PROP_DIRECT` for an absolute tablet/touchscreen. The example proves both shapes: `GtkKeyboardConfig` (input_backend.rs:76-129) advertises only `EV_KEY` over `SUPPORTED_KEYBOARD_KEYS`; `GtkTouchscreenConfig` (input_backend.rs:146-254) advertises `EV_ABS` `ABS_X/ABS_Y` (+ multitouch `ABS_MT_*`), absinfo, and `INPUT_PROP_DIRECT`.

`device.rs:156` advertises only `VIRTIO_F_VERSION_1`. Two queues, size 256 each (`mod.rs:14-17`): eventq (device→guest) and statusq (guest→device).

### 1.5 Event delivery worker (`worker.rs`)

- Worker runs on its own thread named `"input worker"` (`worker.rs:55-60`). The **event provider instance is created on this worker thread** (`worker.rs:66`), and `next_event` is called only from here — so the provider need not be `Send`/`Sync` across threads (and indeed `InputEventProviderInstance` is `!Send + !Sync`, `c_to_rust.rs:94`). The config instance, by contrast, is created on the device-setup path and is `Send+Sync` (`c_to_rust.rs:91-92`).
- The loop is **epoll-based** (`worker.rs:79-159`) with a 1000 ms timeout. It watches: the provider's ready fd (`EVENTQ_USER`), the eventq kick, the statusq kick, and a stop fd. On any wake it calls `process_event_queue` → `fill_event_virtqueue` (`worker.rs:165-198`), which repeatedly calls `next_event()` until it returns `Ok(None)`, packing `VirtioInputEvent{type,code,value as i32}` (`worker.rs:19-23,175-179`) into the descriptor, then signals the guest.
- **No SYN is auto-inserted.** The worker copies events verbatim. The provider MUST emit `EV_SYN/SYN_REPORT/0` itself after each logical group (standard evdev semantics). `SYN_REPORT=0x00` is defined in `input_constants.rs:134`.
- **statusq is a no-op** (`worker.rs:238-248`, `read_status_virtqueue`): guest→host writes (keyboard LED state for CapsLock/NumLock, key-repeat rate) are read and discarded with a debug log. So there is currently **no host-visible LED/repeat feedback**. Patching this is required if limina wants CapsLock/NumLock LED sync.

**macOS caveat (important):** `worker.rs` uses `utils::epoll::Epoll` and the device uses `utils::eventfd::EventFd`. These are Linux primitives. On Apple Silicon the VMM runs on the host (macOS), so libkrun must provide a Darwin shim for epoll/eventfd, or the input worker won't build/run as-is. The display/GPU side already runs on macOS in current libkrun, so a shim almost certainly exists — but **this must be verified by building libkrun from `third_party` with `--features input` on this host** (see §6). The `get_ready_efd` you return from limina must be a descriptor that this epoll-shim accepts as readable; on macOS that likely means a pipe read-end (no `eventfd(2)` on Darwin).

### 1.6 The fd / passthrough provider (`passthrough.rs`)

`PassthroughInputBackend` (`passthrough.rs:11-148`) is a built-in provider that wraps a single `BorrowedFd` and:
- answers all six config queries by issuing Linux **evdev ioctls** on that fd: `EVIOCGNAME` (0x06), `EVIOCGUNIQ` (0x08), `EVIOCGID` (0x02), `EVIOCGBIT(ev)` (0x20+ev), `EVIOCGABS(axis)` (0x40+axis), `EVIOCGPROP` (0x09) — `passthrough.rs:170-193`;
- in `next_event` (`passthrough.rs:119-147`) reads a `struct input_event`-sized record (`LinuxInputEvent`, `passthrough.rs:150-157`: `timeval` + `u16 type` + `u16 code` + `u32 value`) and forwards it; `EAGAIN` → `Ok(None)`.

This path is **Linux-host-only** as written (it `ioctl`s an evdev node and assumes the `LinuxInputEvent` layout with a host `timeval`). On macOS it is not directly usable: there is no `/dev/input` and the ioctls are Linux uapi. limina on macOS should use the vtable path (`krun_add_input_device`), not the fd path. (A custom pipe carrying `LinuxInputEvent`-shaped records is conceivable but would still hit the evdev-ioctl config queries, so it is not viable without patching `passthrough.rs`.)

### 1.7 The `src/input` bridge crate

`c_to_rust.rs` wraps a C-provided vtable into Rust (`InputEventProviderInstance`, `InputConfigInstance`); `rust_to_c.rs` does the reverse — it provides blanket impls (`IntoInputConfig`, `IntoInputEvents`, `rust_to_c.rs:55-266`) that turn any Rust type implementing `InputQueryConfig`/`InputEventsImpl` + `ObjectNew` into the C `krun_input_config`/`krun_input_event_provider` structs, generating the `extern "C"` trampolines. The traits (`rust_to_c.rs:19-53`): `InputQueryConfig` (six query fns) and `InputEventsImpl` (`get_read_notify_fd` + `next_event`). Event-type enum `InputEventType` (`lib.rs:50-88`): Syn=0, Key=1, Rel=2, Abs=3, Msc=4, Sw=5, Led=0x11, Snd=0x12, Rep=0x14. Helper `write_bitmap(buf, &[codes])` (`lib.rs:96-108`) sets bits and returns the used length — exactly what `query_event_capabilities`/`query_properties` must return.

**These traits are only reachable if limina links the Rust `krun_input` crate directly** (i.e. builds libkrun in-tree / depends on the workspace). If limina instead links the C ABI of a prebuilt dylib, it implements the raw C vtables itself. Either is fine; the Rust trait path is more ergonomic and is what the examples use.

### 1.8 Example reference (`krun_gtk_display`)

- `GtkInputEventProvider` (input_backend.rs:40-65) holds a `PollableChannelReciever<KrunInputEvent>`; `get_read_notify_fd` returns the channel's fd, `next_event` does a `try_recv` (WouldBlock → `Ok(None)`). The GTK UI thread pushes translated events into the channel; the worker thread drains it. **This is the exact architecture limina should mirror** (macOS UI thread → channel/ring + readable fd → libkrun worker).
- `gtk_keycode_to_linux` (input_backend.rs:25-38) is just `gtk_key - 8`, because GTK/X11 hardware keycodes are Linux evdev codes + 8. macOS has no such fixed offset; limina needs an explicit `kVK_* → KEY_*` table.
- Device identity: `vendor = b"RH"` little-endian, keyboard product `0x0001`, touchscreen `0x0003`, `bustype = BUS_VIRTUAL (0)` (input_backend.rs:11-18, 89-97).

### 1.9 Guest kernel requirements

Guest needs `CONFIG_VIRTIO_INPUT`, `CONFIG_INPUT_EVDEV`, and (because input requires the GPU feature) `CONFIG_DRM_VIRTIO_GPU`. Fedora-Workstation-43's stock kernel ships all as modules and GNOME/libinput binds any evdev node automatically — **no guest agent is needed for basic keyboard + pointer** for the boot milestone. (Verify the libkrunfw bundled kernel `.config` has `VIRTIO_INPUT` if relying on the bundled kernel rather than the raw image's own kernel.)

### 1.10 Homebrew dylib reality check

`/opt/homebrew/lib/libkrun.dylib → ../Cellar/libkrun/1.17.4/lib/libkrun.1.17.4.dylib` **DOES export the input + display + gpu symbols** (verified `nm -gU`): `_krun_add_input_device`, `_krun_add_input_device_fd`, `_krun_add_display`, `_krun_set_display_backend`, `_krun_display_set_dpi/_edid/_physical_size/_refresh_rate`, `_krun_set_gpu_options`, `_krun_set_gpu_options2`. So **the Homebrew 1.17.4 bottle was built WITH input + gpu**, and limina can link it directly for an initial spike (still call `krun_has_feature` to be safe). Building from `third_party/libkrun` remains preferable long-term so limina can patch the input worker (statusq feedback, any macOS shim gaps) and track `main`. (Earlier in this research pass several `nm` invocations returned empty output due to a transient tool-harness glitch and led to a wrong "symbols absent" conclusion; the re-run confirmed the symbols are present.)

### 1.11 macOS host facilities

| Facility | API | Use for limina |
|---|---|---|
| App-local key/mouse | `NSView`/`NSWindow` `keyDown:`/`keyUp:`/`flagsChanged:`/`mouseMoved:`/`scrollWheel:` | Default path; events only when our window is key. No entitlement. |
| Virtual keycode | `NSEvent.keyCode` (`kVK_*`, `<Carbon/HIToolbox/Events.h>`) | Layout-independent physical scancode → source for evdev mapping. |
| Modifiers | `NSEvent.modifierFlags` (`Command/Option/Control/Shift/Function/CapsLock`) | Tracked via `flagsChanged:`; needed for Cmd/Option remap. |
| System-combo capture | `CGEventTap` (`kCGSessionEventTap`/`kCGHIDEventTap`) | Intercept Cmd-Tab/Cmd-Space/Cmd-`/F11 before WindowServer. Needs Accessibility (TCC); defeated by Secure Input. |
| Lowest-level HID | `IOHIDManager` | Raw HID usages, can grab keyboard. Heavy; needs entitlement/root. Fallback only. |
| Relative mouse | `CGAssociateMouseAndMouseCursorPosition(false)`, `CGWarpMouseCursorPosition`, `NSEvent.deltaX/deltaY` | Capture mode → `EV_REL`. |
| Cursor hide | `NSCursor hide/unhide`, `CGDisplayHideCursor` | Hide host cursor while captured. |
| Fullscreen (window) | `NSWindow toggleFullScreen:`, `NSWindowCollectionBehaviorFullScreenPrimary` | Spaces fullscreen; respects notch safe-area. |
| Fullscreen (capture) | `CGDisplayCapture` + borderless window, `NSApplicationPresentationOptions` | Exclusive display; must manually inset for notch. |
| HiDPI geometry | `NSScreen.backingScaleFactor`, `convertPointToBacking:`, `safeAreaInsets` | Point→pixel for ABS pointer; notch insets. |

macOS `kVK_*` codes are physical-position (US-QWERTY numbering) and do not change with layout — a clean source for a fixed position→evdev table.

---

## 2. How it works end to end

### 2.1 Control path (device probe)

1. limina builds a `krun_input_config` (features=`QUERY`, six query callbacks) and a `krun_input_event_provider` (features=`QUEUE`, `get_ready_efd`+`next_event`), then calls `krun_add_input_device(ctx_id, &config, &provider)` once per device.
2. `Input::new` (`device.rs:135-149`) immediately calls `config.create` to get the config instance. At guest boot/activate (`device.rs:217-244`) the worker thread is spawned and creates the provider instance (`worker.rs:66`).
3. The guest `virtio_input` driver writes `(select,subsel)` to config space; `device.rs:198-215` → `update_select` calls the matching query callback and exposes the bytes; the guest builds the evdev node from `EV_BITS`/`ABS_INFO`/`PROP_BITS`/ids/name.
4. Device shape is entirely limina's choice via the capability bitmaps (§1.4). Register multiple devices for keyboard + relative mouse + absolute tablet.

### 2.2 Data path (host → guest)

1. macOS delivers `keyDown:`/`mouseMoved:`/etc. on the main thread (or a `CGEventTap` callback on its runloop).
2. limina translates to one or more `krun_input_event`s — e.g. key press = `EV_KEY,code,1` then `EV_SYN,SYN_REPORT,0`; mouse move = `EV_REL,REL_X,dx`, `EV_REL,REL_Y,dy`, `EV_SYN,0,0` — pushes them into a channel/ring and makes the ready fd readable (one byte / channel send).
3. epoll wakes the input worker (`worker.rs:133-141`); it drains via `next_event()` until `Ok(None)` (`worker.rs:172-196`), writes events into eventq descriptors, `add_used`, and `signal_used_queue` (`worker.rs:156`).
4. Guest `virtio_input` injects into the Linux input core; libinput/Wayland/Xorg dispatch to the focused app.
5. Reverse (statusq, LEDs/repeat) is currently dropped (`worker.rs:243-245`).

### 2.3 Keyboard scancode pipeline (concrete)

```
NSEvent.keyCode (kVK_*, physical)        --limina table-->  Linux KEY_* (input-event-codes.h)
NSEvent.modifierFlags (flagsChanged:)    -----------> EV_KEY for KEY_LEFTMETA/LEFTALT/LEFTCTRL/LEFTSHIFT/CAPSLOCK
each logical group                        -----------> EV_SYN/SYN_REPORT/0
```

- The `kVK_* → KEY_*` table is fixed (~110 entries; the `KEY_*` numeric values are in `input_constants.rs:13-121`). Both ends are position-based so layout is irrelevant.
- Pure modifiers arrive only via `flagsChanged:`; track previous flag state and synthesize press/release on transitions.
- **Command/Option swap** = swap two table rows: `kVK_Command → KEY_LEFTALT`, `kVK_Option → KEY_LEFTMETA` (and right variants). Purely host-side; guest layout untouched.

### 2.4 Layout, dead keys, IME

If limina forwards only physical scancodes and lets the **guest own the keymap**, dead keys, AltGr, and the guest IME all work natively in Linux. virtio-input has no character channel, so sending Unicode is not even an option — scancode forwarding is the only correct design. Host-side `UCKeyTranslate`/IME is unnecessary and would be wrong.

---

## 3. Options inventory for limina

### A. Pointer model
**A1. Absolute tablet only** (`EV_ABS`/`ABS_X,ABS_Y`, `min=0,max=32767`, `INPUT_PROP_DIRECT`): map window-local backing-pixel coords → `[0..max]`. Pros: 1:1 cursor, no capture machinery, trivial multi-monitor/fullscreen, best desktop UX. Cons: no raw deltas for FPS/grabbing apps. (Exactly what `GtkTouchscreenConfig` does.)
**A2. Relative mouse only** (`EV_REL`/`REL_X,REL_Y,REL_WHEEL`): capture + warp-to-center + hide cursor, read `deltaX/Y`. Pros: correct for games, Parallels-like capture. Cons: needs explicit capture/release UX; delta scaling.
**A3. Both, switch dynamically (RECOMMENDED):** register tablet + mouse; default absolute, switch to relative on hotkey or when the guest grabs the pointer. Mute the inactive device.

### B. Keyboard capture depth
**B1. Window-only** (`NSView keyDown:`/`flagsChanged:`): zero entitlements; but macOS steals Cmd-Tab/Cmd-Space/F11/mission-control.
**B2. `CGEventTap` while focused (RECOMMENDED for capture mode):** swallow system combos only when the VM window is key and capture is on; forward them to the guest. Needs Accessibility (TCC); blocked by Secure Input; the tap callback must be fast and re-enable on `kCGEventTapDisabledByTimeout`.
**B3. `IOHIDManager` grab:** lowest latency, full keyboard grab; heavy, needs elevated privilege. Fallback only.

### C. Where translation lives
**C1. Host-side `kVK_*→KEY_*` table in limina (RECOMMENDED):** guest owns layout; Cmd/Option swap + custom keybindings are config edits; no guest agent.
**C2. Patch libkrun:** only if a capability can't be expressed (none found for keyboard/pointer). The needed patches are about feedback/threading, not translation (see §5).

### D. Integration shape
**D1. Implement the two vtables via `krun_add_input_device` (RECOMMENDED):** full control; mirror `examples/krun_gtk_display`. Use the Rust `krun_input` traits if building libkrun in-tree, else the raw C vtables.
**D2. `krun_add_input_device_fd`:** NOT viable on macOS — `passthrough.rs` is evdev-ioctl + Linux-`input_event` specific (§1.6).
**D0. Do nothing / reuse upstream:** viable for a first spike — the stock Homebrew dylib already exports the input/gpu/display symbols (§1.10), so limina can link it as-is. The input ABI needs no change for keyboard + relative + absolute pointer. Building libkrun in-tree is still preferred long-term to patch the worker (statusq) and track `main`.

### E. Fullscreen
**E1. `NSWindow toggleFullScreen:` (RECOMMENDED default):** Spaces fullscreen, respects notch.
**E2. `CGDisplayCapture` exclusive:** game-style, hides menu bar/Dock; manual notch inset. Optional toggle.

---

## 4. Recommendation

First milestone (boot Fedora raw image with a usable GUI):
- **libkrun:** the Homebrew 1.17.4 bottle already has input+gpu (§1.10) — link it for the first spike (gate on `krun_has_feature(KRUN_FEATURE_INPUT)`/`KRUN_FEATURE_GPU`). Switch to building from `third_party/libkrun --features input,gpu` once patches (statusq, shims) are needed.
- **Pointer:** start **A1 (absolute tablet)**; add **A2/A3** right after for games.
- **Keyboard:** **B1** to get typing working, then **B2 (`CGEventTap`)** behind a "capture system shortcuts" toggle; modifiers via `flagsChanged:`.
- **Translation:** **C1** host-side `kVK_*→KEY_*` table with Cmd/Option swap + user keybindings; guest owns layout.
- **Integration:** **D1** — mirror `GtkInputEventProvider`: UI thread → channel/ring + readable fd → libkrun worker. Emit explicit `SYN_REPORT`.
- **Fullscreen:** **E1** now, **E2** later.

What must be patched in libkrun (beyond enabling the feature):
1. **macOS epoll/eventfd shim for the input worker** if not already present (`worker.rs` uses `utils::epoll`/`utils::eventfd`) — verify by building; the GPU path already runs on macOS so a shim likely exists.
2. **statusq feedback** (`worker.rs:238-248` is a no-op) if limina wants CapsLock/NumLock LED sync or guest-set key-repeat surfaced to the host.
Everything else (capture layer, translator, pointer-mode state machine, fullscreen, ABS scaling against the display geometry limina feeds via the display API) is limina app code.

---

## 5. Open questions / things to prototype

1. **Build & link:** first try linking the Homebrew dylib (symbols confirmed present); then build libkrun in `third_party` with `--features input,gpu` on this M1 host to confirm it compiles (epoll/eventfd shim) and to enable patching. Decide: link the in-tree dylib (raw C backend structs) or the Rust `krun_input` crate (`into_input_config`/`into_input_events`).
2. **ready_efd on macOS:** what fd type the epoll-shim accepts. Spike: register a device, push events, write to a pipe read-end as `get_ready_efd`, confirm the worker wakes and the guest sees events.
3. **SYN batching:** confirmed the provider must emit `SYN_REPORT` (worker copies verbatim, `worker.rs:175-184`); validate guest behavior with/without trailing SYN on a key and a mouse move.
4. **Multiple devices:** confirm `krun_add_input_device` called 3× yields 3 evdev nodes; verify per-device capability isolation.
5. **statusq / LED:** decide whether to patch `worker.rs` to surface guest LED/repeat writes (needed for CapsLock/NumLock indicator parity).
6. **ABS coordinate space:** wire window backing-pixel coords (respecting `backingScaleFactor` + `safeAreaInsets`) to `ABS_X/Y` `[0..32767]`; confirm no notch drift in fullscreen. Tie max/res in `query_abs_info` to the display resolution limina advertises.
7. **CGEventTap + Secure Input:** measure how often Secure Input disables the tap (password fields elsewhere); design a graceful "shortcuts temporarily unavailable" state; handle `kCGEventTapDisabledByTimeout`.
8. **CapsLock:** verify macOS sticky CapsLock via `flagsChanged:` yields correct `KEY_CAPSLOCK` press+release the guest treats as a toggle.
9. **examples/gui_vm vs krun_gtk_display:** read `gui_vm`'s SDL feeder (`sdl_display_backend.rs`, `window.rs`) as the closer Cocoa/SDL template, if present in this tree.

---

## 6. References

Local source (read this pass, real line numbers):
- `include/libkrun.h:722-723` — `krun_add_input_device(ctx,void*,size,void*,size)`; `:735` `krun_add_input_device_fd`; `:707` `krun_set_display_backend(ctx,void*,size)`; `:629` `krun_add_display`; `:648/663/679/693` `krun_display_set_edid/dpi/physical_size/refresh_rate`; `:1109-1134` `krun_has_feature` + `KRUN_FEATURE_INPUT=4`/`KRUN_FEATURE_GPU=2`.
- `src/libkrun/src/lib.rs:1595-1606` — `krun_add_input_device` casts the `void*` to `InputConfigBackend`; cfg-gated stub at `:1638` when input feature off.
- `include/libkrun_input.h` — event `:25-29`; provider vtable `:84-88` (`get_ready_efd` `:67`, `next_event` `:82`); config vtable `:137-145`; device_ids `:93-98`; absinfo `:103-109`; objects `:147-165`; errors `:12-15`; features `:18-19`.
- `include/libkrun_display.h` — formats `:18-35`; `configure_scanout` (display vs scanout size) `:99-105`; one-GPU-thread / must-not-block note `:131-145`.
- `src/devices/src/virtio/input/device.rs` — config-select dispatch `:54-100`; payload 128 B `:116`; features `:156`; activate/worker spawn `:217-244`.
- `src/devices/src/virtio/input/worker.rs` — worker thread `:55-66`; epoll loop `:79-159`; `fill_event_virtqueue`/verbatim copy `:165-198`; statusq no-op `:238-248`.
- `src/devices/src/virtio/input/mod.rs` — `VIRTIO_ID_INPUT=18`, config-select codes `:19-32`; 2 queues size 256 `:14-17`.
- `src/devices/src/virtio/input/passthrough.rs` — evdev-ioctl config queries `:15-100,170-193`; `LinuxInputEvent` `:150-157`; `next_event` `:119-147` (Linux-host only).
- `src/input/src/lib.rs` — `InputEventType` `:50-88`; `write_bitmap` `:96-108`.
- `src/input/src/rust_to_c.rs` — `InputQueryConfig`/`InputEventsImpl` traits `:19-53`; trampolines `:55-266`.
- `src/input/src/c_to_rust.rs` — instance wrappers; provider `!Send+!Sync` `:94`; config `Send+Sync` `:91-92`; required-method check `:228-234,287`.
- `examples/krun_gtk_display/src/input_backend.rs` — channel provider `:40-65`; keyboard config `:76-129`; touchscreen (ABS+MT, INPUT_PROP_DIRECT) `:146-254`; ids `:11-18`.
- `examples/krun_gtk_display/src/input_constants.rs` — `KEY_*` `:13-121`, `ABS_X/Y` `:123-124`, `INPUT_PROP_DIRECT` `:11`, `SYN_REPORT` `:134`, `SUPPORTED_KEYBOARD_KEYS` `:137-247`.
- `src/libkrun/Cargo.toml:17-18` — `gpu = ["vmm/gpu","devices/gpu","krun_display"]`, `input = ["krun_input","vmm/input","devices/input"]`; `src/devices/Cargo.toml:17-18` — `gpu = ["rutabaga_gfx",...]`, `input = ["zerocopy","krun_input"]` (input and gpu are separate features).
- `src/input/src/c_to_rust.rs:188-198,246-254` — `InputConfigBackend`/`InputEventProviderBackend` `#[repr(C)]` (the real arg layout for the `void*` params).
- `Cargo.toml:1-20` — workspace members incl. `src/input`, `src/display`.
- Homebrew dylib `/opt/homebrew/lib/libkrun.1.17.4.dylib` (`nm -gU`): exports `_krun_add_input_device`, `_krun_add_input_device_fd`, `_krun_add_display`, `_krun_set_display_backend`, `_krun_display_set_{dpi,edid,physical_size,refresh_rate}`, `_krun_set_gpu_options{,2}` — built WITH input+gpu.

External (standard platform docs):
- AppKit `NSEvent` (`keyCode`, `modifierFlags`, `deltaX/Y`), `NSWindow toggleFullScreen:`, `NSScreen` (`backingScaleFactor`, `safeAreaInsets`).
- Core Graphics: `CGEventTapCreate`, `CGAssociateMouseAndMouseCursorPosition`, `CGWarpMouseCursorPosition`, `CGDisplayCapture`, `CGDisplayHideCursor`.
- IOKit `IOHIDManager`; Carbon `<HIToolbox/Events.h>` `kVK_*`.
- Linux `include/uapi/linux/input-event-codes.h` (`EV_*`, `KEY_*`, `REL_*`, `ABS_*`, `INPUT_PROP_*`); virtio-input spec (config select `VIRTIO_INPUT_CFG_*`, eventq/statusq).
- libkrun upstream: https://github.com/containers/libkrun (virtio-input PR #415).
