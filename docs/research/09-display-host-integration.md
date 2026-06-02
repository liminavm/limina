# 09 — Display Host Integration (window, fullscreen, multi-display, HiDPI)

Scope: how limina presents the Linux guest's framebuffer in a native macOS window. Covers libkrun's `virtio-gpu` → display-backend contract (the `krun_set_display_backend` vtable defined in `libkrun_display.h`), how scanout frames flow guest→host and exactly what pixel data we receive, and the design of a native Cocoa + Metal presentation layer (NSWindow/CAMetalLayer, vsync, HiDPI/Retina, dynamic resize → EDID/mode change, multi-monitor, fullscreen, cursor). Interlocks with doc 03 (3D/blob zero-copy) for the texture handoff and doc 04 (input) for pointer/keyboard.

> Verification status: the **display + input C ABIs and the public display C API in `libkrun.h` are fully verified** from local source (cited `path:line`), including `krun_display_set_edid` (`libkrun.h:648`), `KRUN_MAX_DISPLAYS 16` (`libkrun.h:614`; matches `src/display/src/lib.rs:26` and `VIRTIO_GPU_MAX_SCANOUTS=16` in `src/devices/src/virtio/gpu/protocol.rs:283`). The **`main.rs` config sequence is verified** from the GTK example. The only items still `[VERIFY]` are *internal vmm gpu-device details* (frame-ring depth, exact `display_*` vs `width/height` semantics, whether the CPU copy is `read_2d`/`transfer_to_host`) — the gpu device files are now located (`src/devices/src/virtio/gpu/{display,edid,virtio_gpu,worker}.rs`) but their bodies were not fully read this session due to an intermittent tool outage. See §6.

---

## 1. The verified vtable contract (`libkrun_display.h`)

This is the exact, current contract limina must implement. The backend is a **C object with a vtable**, created lazily by libkrun on a dedicated thread.

### 1.A Lifecycle & threading (load-bearing)

- libkrun calls `create` (if non-NULL) **on a specific gpu thread**, then makes **all** subsequent vtable calls from **that same thread** (`libkrun_display.h:156-158`). So the backend is single-threaded from libkrun's side — limina does *not* need internal locking between vtable calls, but **does** need a thread hop to AppKit's main thread (see §3.D).
- "the display methods should not block for a long time otherwise this will negatively impact performance of the emulated GPU device" (`libkrun_display.h:157-158`). present_frame must be cheap; heavy GPU work / waiting on vsync must not stall it.
- Struct **must be zero-initialized** so future/unset fields are NULL (`libkrun_display.h:162-163`). limina should `MaybeUninit::zeroed()` / `Default` the struct.

### 1.B The structs (`libkrun_display.h:165-182`)

```c
struct krun_display_basic_framebuffer_vtable {   // :165
    krun_display_destroy_fn           destroy;            // optional
    krun_display_disable_scanout_fn   disable_scanout;    // required
    krun_display_configure_scanout_fn configure_scanout;  // required
    krun_display_alloc_frame_fn       alloc_frame;        // required
    krun_display_present_frame_fn     present_frame;       // required
};
union  krun_display_vtable { struct krun_display_basic_framebuffer_vtable basic_framebuffer; };  // :173
struct krun_display_backend {                    // :177
    uint64_t                 features;        // set KRUN_DISPLAY_FEATURE_BASIC_FRAMEBUFFER (=1, :40)
    void                    *create_userdata; // optional
    krun_display_create_fn   create;          // optional
    union krun_display_vtable vtable;
};
```

The backend is passed to libkrun **by value, with its size**, via `krun_set_display_backend(ctx, &backend as *const c_void, size_of_val(&backend))` (GTK example `main.rs:193-197`). libkrun copies it. `features` selects which vtable methods are required; only `KRUN_DISPLAY_FEATURE_BASIC_FRAMEBUFFER` exists today.

### 1.C Callback signatures (verified)

| Callback (typedef @ line) | Signature | Semantics |
| --- | --- | --- |
| `create_fn` (:54) | `int32(void **instance, const void *userdata, const void *reserved)` | Create self; write your object ptr to `*instance` (may be NULL). `userdata` = `create_userdata`. |
| `destroy_fn` (:65) | `int32(void *instance)` | Optional teardown. |
| `configure_scanout_fn` (:82) | `int32(void*, u32 scanout_id, u32 display_width, u32 display_height, u32 width, u32 height, u32 format)` | (Re)configure a scanout. **Two sizes (confirmed `virtio_gpu.rs:478-485`):** `display_width/height` come from the **per-display config** (`DisplayInfo`, i.e. what you passed to `krun_add_display`); `width/height` come from the **guest's `SET_SCANOUT`** resource size (the actual framebuffer). They can differ (guest may scan out a smaller/different mode than the display's nominal size). `format` is the scanout resource's format. |
| `disable_scanout_fn` (:100) | `int32(void*, u32 scanout_id)` | Blank/tear down a scanout. |
| `alloc_frame_fn` (:119) | `int32(void*, u32 scanout_id, uint8_t **buffer, size_t *buffer_size)` | **libkrun asks the backend for a CPU buffer to write the next frame into.** Backend sets `*buffer` (writable host memory) and `*buffer_size`. Returns a **non-negative `frame_id`** or negative `KRUN_DISPLAY_ERR_*`. |
| `present_frame_fn` (:146) | `int32(void*, u32 scanout_id, u32 frame_id, const struct krun_rect *damage_area)` | libkrun has written pixels into the `frame_id` buffer; present it. `damage_area` (`krun_rect{x,y,w,h}`, :121) is an optional hint; **NULL ⇒ whole frame damaged**. After this call the `frame_id` is consumed; buffer must not be mutated by the backend afterward (:131-134). |

**Critical architectural fact (now confirmed from the device source).** The flush path is `flush_resource()` → for each enabled scanout: `alloc_frame(scanout_id) -> (frame_id, buffer)` → `read_2d_resource(rutabaga, resource, buffer)` → `present_frame(scanout_id, frame_id, Some(rect))` (`virtio_gpu.rs:518-536`). So:
- It is a **CPU pull/readback model**: libkrun calls `rutabaga.transfer_read(...)` to copy the guest/host resource into **the buffer the backend returned from `alloc_frame`** (`virtio_gpu.rs:509-512`). There is **no zero-copy / dmabuf / IOSurface handle** in this vtable today.
- `read_2d_resource` **always reads the entire resource**, tightly packed, `stride = width * BYTES_PER_PIXEL` (`virtio_gpu.rs:496-507`). So libkrun writes a **full, contiguous `width*height*4` frame** into our buffer every flush — even though `present_frame` *also* receives the `rect`. **Implication:** the damage `rect` is a present-time hint only; the *copy* into our buffer is always full-frame. limina can still use `rect` to limit the texture `replaceRegion` upload, but cannot assume only `rect` changed in the buffer. Buffer stride is fixed at `width*4` — match it exactly when wrapping a `MTLBuffer`.
- The whole sequence runs **synchronously on the gpu worker thread** (it's inside command processing); `present_frame` returning is what lets the guest's flush complete. Keep it cheap.
- `OUT_OF_BUFFERS` (`:16`) lets the backend signal `alloc_frame` exhaustion; libkrun calls `alloc_frame` fresh for **every** flush, so a backend can ring-buffer or even hand back a single reused buffer (since the copy is synchronous before `present_frame`). A small ring is still better for decoupling the present (main-thread) from the copy (gpu thread).

### 1.D Pixel formats (`libkrun_display.h:18-33`)

Verified enum (value = virtio-gpu format number):

| Macro | Value | Metal `MTLPixelFormat` |
| --- | --- | --- |
| `KRUN_DISPLAY_FORMAT_B8G8R8A8_UNORM` | 1 | `.bgra8Unorm` (native, ideal) |
| `KRUN_DISPLAY_FORMAT_B8G8R8X8_UNORM` | 2 | `.bgra8Unorm` (ignore alpha) |
| `KRUN_DISPLAY_FORMAT_A8R8G8B8_UNORM` | 3 | needs swizzle |
| `KRUN_DISPLAY_FORMAT_X8R8G8B8_UNORM` | 4 | needs swizzle |
| `KRUN_DISPLAY_FORMAT_R8G8B8A8_UNORM` | 67 | `.rgba8Unorm` |
| `KRUN_DISPLAY_FORMAT_X8B8G8R8_UNORM` | 68 | `.rgba8Unorm` (ignore alpha) |
| `KRUN_DISPLAY_FORMAT_A8B8G8R8_UNORM` | 121 | `.rgba8Unorm` |
| `KRUN_DISPLAY_FORMAT_R8G8B8X8_UNORM` | 134 | `.rgba8Unorm` (ignore alpha) |

All are 32bpp. Linux virtio-gpu/DRM commonly drives `B8G8R8X8`/`B8G8R8A8` for the primary plane, mapping cleanly to `.bgra8Unorm` — the common case is a straight `replaceRegion` with no swizzle.

### 1.E Error codes (`libkrun_display.h:11-16`)
`INTERNAL -1`, `METHOD_UNSUPPORTED -2`, `INVALID_SCANOUT_ID -3`, `INVALID_PARAM -4`, `OUT_OF_BUFFERS -5`. Return ≥0 on success (`frame_id` for `alloc_frame`, 0 for others).

---

## 2. Config-time C API (verified from GTK example `main.rs`)

The host-side setup sequence (GTK example `examples/krun_gtk_display/src/main.rs:149-221`):

```rust
let ctx = krun_create_ctx();
krun_set_vm_config(ctx, 4 /*vcpus*/, 4096 /*MiB*/);                 // :151
krun_set_gpu_options2(ctx,                                          // :153
    VIRGLRENDERER_USE_EGL | VIRGLRENDERER_VENUS
  | VIRGLRENDERER_RENDER_SERVER | VIRGLRENDERER_THREAD_SYNC
  | VIRGLRENDERER_USE_ASYNC_FENCE_CB, 4096 /*shm bytes? */);
krun_set_root(ctx, root);                                          // :163
krun_set_exec(ctx, exe, argv, envp);                              // :173
for d in displays {
    let display_id = krun_add_display(ctx, d.width, d.height);     // :176  -> returns u32 id
    krun_display_set_refresh_rate(ctx, display_id, rate);          // :178  (optional)
    krun_display_set_dpi(ctx, display_id, dpi);                    // :183  (optional)
    krun_display_set_physical_size(ctx, display_id, w_mm, h_mm);   // :186  (optional)
}
krun_set_display_backend(ctx, &backend, size_of(backend));        // :193  ONE backend, all displays
for fd in passthrough_inputs { krun_add_input_device_fd(ctx, fd); } // :203
for dev in input_devices {                                        // :212
    krun_add_input_device(ctx, &config, sizeof, &event_provider, sizeof);
}
krun_start_enter(ctx);                                            // :221  (blocks; runs the VM)
```

Verified facts:
- **`krun_add_display(ctx, width, height) -> display_id` (verified `libkrun.h:629`, returns `0..KRUN_MAX_DISPLAYS-1`)** — add up to **16** displays (`KRUN_MAX_DISPLAYS=16`, `libkrun.h:614`; `VIRTIO_GPU_MAX_SCANOUTS=16`, `protocol.rs:283`). Each returns an id; the vtable `scanout_id` corresponds to these displays. Multi-display is first-class config.
- **Per-display monitor metadata** is set via `krun_display_set_refresh_rate` / `_set_dpi` / `_set_physical_size` (`main.rs:178,183,186`; declared `libkrun.h:693,663,679`) — libkrun **generates an EDID** from these. **`krun_display_set_edid(ctx, display_id, blob, size)` (verified `libkrun.h:648`)** lets us supply a **raw EDID blob instead**; per its doc (`libkrun.h:634-637`) a custom EDID makes "all display parameters except width and height ignored", and libkrun does **not** validate that the EDID matches the `krun_add_display` width/height. So limina can either (a) feed DPI/physical-size and let libkrun build the EDID, or (b) hand-craft an EDID (e.g. to advertise a precise preferred mode / Retina pixel size). All are **config-time**.
- **`krun_set_display_backend` takes ONE backend** for the whole ctx (`main.rs:193`), and the vtable methods are keyed by `scanout_id`. So limina writes **one backend object that multiplexes all displays by `scanout_id`** (not one backend per window).
- All display calls (`krun_add_display`, `krun_display_set_edid/_dpi/_physical_size/_refresh_rate`, `krun_set_display_backend`) take a **config `ctx_id`** and are documented for pre-`krun_start_enter` setup. **No runtime resize / EDID-update / hotplug entry point is exposed in `libkrun.h`** — confirmed by enumerating the `krun_display_*` symbols (only the four config setters + `set_edid` exist). This is the single biggest gap for window-follow dynamic resize and almost certainly requires a libkrun patch (§4.F, §6 item 2). The guest *can* be told about scanout sizes at the virtio-gpu protocol level (`GET_DISPLAY_INFO`/`GET_EDID` + a config-change interrupt), so the patch is plumbing an existing capability out to a new C call, not new device work.
- The gpu path requires `krun_set_gpu_options2` (virgl flags). For a pure 2D framebuffer boot we still likely need the gpu device enabled; `[VERIFY]` whether a display works without virgl/venus or if gpu options are mandatory.
- `krun_init_log(fd_or_target, level, style, 0)` (`main.rs:130-146`) — use `KRUN_LOG_LEVEL_TRACE` while bringing up the backend.

The `krun-sys` crate exposes all of these as Rust FFI (`use krun_sys::{...}` in `main.rs:8-16`).
**Superseded by decision D2.1** (`docs/design/architecture.md`): limina does **not** use the C ABI /
`krun-sys` — it depends on the vendored `krun-*` crates directly and sets these via `VmResources`
fields (`displays`, `display_backend`, `gpu_virgl_flags`) in native Rust. The C-symbol analysis
above still holds as the description of what capability exists; the runtime-resize gap becomes a
Rust API we add to `krun-vmm`/`krun-devices` rather than a new C call.

---

## 3. Verified input ABI (`libkrun_input.h`) — for §4 cursor/pointer coupling

Two separate vtable objects per input device, both passed to `krun_add_input_device(ctx, &config, sz, &events, sz)` (`main.rs:212`):

- **`krun_input_event_provider`** (`libkrun_input.h:160`): `get_ready_efd` (required, :67) returns an **eventfd** that libkrun epolls; `next_event` (required, :82) is polled to drain `struct krun_input_event {u16 type; u16 code; u32 value;}` (:25) — i.e. **raw Linux `evdev` events** (EV_KEY/EV_REL/EV_ABS). Non-blocking; returns 1/0/neg.
- **`krun_input_config`** (`libkrun_input.h:150`): `query_device_name`, `query_device_ids` (bus/vendor/product/version, :93), `query_event_capabilities` (evbit bitmaps), `query_abs_info` (`absinfo{min,max,fuzz,flat,res}`, :103), `query_properties`. This is how the guest enumerates the virtio-input device as a keyboard / relative mouse / **absolute pointer / touchscreen** (the GTK example builds a touchscreen with abs axes, `main.rs:236-255`).
- `krun_add_input_device_fd(ctx, fd)` (:203) passes through a host `/dev/input/eventN` directly (Linux-host only; not applicable on macOS).

**Implication for cursor (§4.E):** there is **no virtio-gpu cursor-queue callback** in the display vtable. The host presents only the scanout framebuffer; the guest cursor is either composited by the guest into the scanout, or we use an **absolute-pointer input device** + draw an `NSCursor`. We feed pointer motion as EV_ABS via the input event provider.

---

## 4. End-to-end flow & options

### 3.D Threads / main-thread rule (combined with §1.A)
libkrun drives `configure_scanout`/`alloc_frame`/`present_frame` from **one gpu thread**. **AppKit/Cocoa window+view mutation must be on the macOS main thread.** Required design: the vtable thread writes pixels into a backend-owned buffer pool and signals; the **main thread (CVDisplayLink or MTKView.draw)** uploads to the `MTLTexture` and presents. `present_frame` itself must return fast (§1.A), so it should just publish "frame N ready" (atomic swap of a buffer index), never wait on the drawable.

### Frame path (verified CPU pull model)

> **Scope:** this CPU-pull copy applies *only to the scanout→window present*. The bulk **3D
> GPU memory** (Venus host-visible blobs) is **zero-copy / genuinely shared** via
> `hv_vm_map` — see doc 03 §1.10a for the full copy-vs-share breakdown. So the per-frame
> readback below is the *one* copy on the hot path, and doc 03 option C (SET_SCANOUT_BLOB,
> scheduled in M4) is what removes even that.

1. Guest virtio-gpu/DRM binds scanout → libkrun → `configure_scanout(id, dw,dh, w,h, fmt)`. limina sizes a `MTLTexture(w×h, fmt)` and a ring of N CPU buffers of `w*h*4`.
2. Each frame: libkrun calls `alloc_frame(id, &buf, &size)` → limina returns the next free ring buffer + its `frame_id`. libkrun copies guest pixels into it. libkrun calls `present_frame(id, frame_id, damage)`. limina marks that buffer "ready+damage", returns immediately.
3. Main-thread `CVDisplayLink` tick: take newest ready buffer, `texture.replaceRegion(damage)`, encode blit→`CAMetalDrawable`, present.

### Options inventory

- **A. Reuse GTK or SDL example backend** — fastest bring-up, but GTK/SDL drags a foreign window/event loop that fights AppKit (no native fullscreen/Spaces, poor Retina, no native key capture/clipboard). Use only as a milestone-1 crutch; reject for the product. (The GTK example is the cleanest *ABI* reference; SDL example is the texture-update reference.)
- **B. Native NSWindow + CAMetalLayer, CPU present (RECOMMENDED v1).** Implement the vtable in Rust against a `CAMetalLayer`. `configure_scanout`→size texture+ring; `alloc_frame`→hand out a shared-storage `MTLBuffer.contents()` (UMA on M1: the "CPU buffer" *is* GPU-visible, so libkrun's write lands directly in a buffer Metal can blit — effectively one copy total, the unavoidable virtio transfer). `present_frame`→publish; CVDisplayLink draws. Native window ⇒ fullscreen, multi-display, key capture, clipboard all reachable. Cons: not zero-copy for 3D (one blit/frame), fine for desktop.
- **C. Zero-copy IOSurface (scheduled M4, after the v1 CPU-pull path; needs libkrun patch + doc 03).** The current vtable cannot do this — would require **patching `libkrun_display.h`** to add a new `features` bit + `present_texture` callback that hands the backend a host-mapped blob / IOSurface / fd for the scanout (so virgl/MoltenVK renders straight into an IOSurface-backed `MTLTexture`), plus libkrun accepting `SET_SCANOUT_BLOB` (today it panics, `worker.rs:334`). Removes the one remaining hot-path copy (doc 03 §1.10a path 3); highest value for smooth full-screen 3D/video. Land it in M4 layered on this section's v1 path; spike IOSurface↔MoltenVK interop first.
- **D. MTKView vs raw CAMetalLayer** — `MTKView` wires CVDisplayLink/drawable/resize for free (good for v1); raw layer gives finer present control (better for v2). Either is fine.
- **E. Cursor: software v1 (guest composites cursor into scanout, or draw NSCursor from EV_ABS position), hardware overlay v2.** No cursor-queue exists in the vtable (§3), so hardware-cursor would also need a libkrun patch.
- **F. Multi-display & resize.** Multi-display is native: `krun_add_display` × N, dispatch vtable by `scanout_id` to per-display windows. **Dynamic resize is the gap:** all sizing appears config-time. To make the guest follow window resize we likely need **(a)** a libkrun patch adding a runtime "reconfigure display / push EDID / hotplug" call that raises the virtio-gpu config-change interrupt, plus **(b)** a **limina guest agent** to apply the mode in the GNOME/Wayland session. Native macOS fullscreen via `NSWindow.toggleFullScreen`; exclusive capture via `CGDisplayCapture`.

---

## 5. Recommendation

Pursue **B (native NSWindow + CAMetalLayer, CPU pull path)** for v1, using `MTKView` (D) for bring-up speed, and architect toward **C (IOSurface zero-copy)** for v2.

Concretely:
1. ~~Depend on the in-tree `krun-sys` crate for the FFI.~~ **(D2.1)** Depend on the vendored
   `krun-vmm`/`krun-display`/`krun-devices` crates directly; configure via `VmResources` in Rust.
2. Implement `struct krun_display_backend` in `limina/src/display/macos.rs`: zero-init it, set `features = KRUN_DISPLAY_FEATURE_BASIC_FRAMEBUFFER`, fill the 4 required `extern "C"` fns + optional create/destroy. Keep a per-`scanout_id` map of `{MTLTexture, ring of shared-storage MTLBuffers, ready-index}`.
3. `alloc_frame`: return `MTLBuffer.contents()` (StorageModeShared — zero extra copy on M1 UMA) and a monotonic `frame_id`; recycle buffers; return `OUT_OF_BUFFERS (-5)` if the ring is exhausted.
4. `present_frame`: blit `damage_area` from the buffer into the texture and atomically publish; return 0 immediately. A `CVDisplayLink`/`MTKView.draw` on the **main thread** presents the latest texture to the `CAMetalLayer` drawable.
5. HiDPI: `layer.contentsScale = window.backingScaleFactor`; drive `drawableSize` from the view's backing bounds; advertise **native Retina pixel dimensions** to the guest at `krun_add_display(w_px, h_px)` and via `krun_display_set_dpi`/`_physical_size` so the guest renders at native resolution.
6. Map `format` per §1.D table to `MTLPixelFormat`; fast-path `B8G8R8A8/X8 → .bgra8Unorm` (no swizzle).
7. Input/cursor: add an **absolute-pointer** virtio-input device (config vtable advertising EV_ABS with `absinfo` matching the scanout size) + a keyboard; feed AppKit events as evdev via the event-provider's eventfd (doc 04). Draw the cursor as `NSCursor` from the EV_ABS coordinates (software cursor) for v1.

**Must be patched in libkrun (anticipated):**
- **Runtime display reconfigure / EDID / hotplug** — nothing in the example or headers does this at runtime; needed for window-follow resize. (Confirm against `libkrun.h` / gpu device first.)
- For v2: a **new display-backend feature bit + zero-copy surface-export callback** (IOSurface/fd) so 3D avoids the per-frame blit (joint with doc 03).
- Optional: **cursor-queue callback** for a hardware cursor.

**We build:** the native Cocoa/Metal presenter, the buffer-ring/CVDisplayLink draw loop, the per-display EDID/metadata mapping, and the guest agent's display-config applier (shared with doc 04).

---

## 6. Open questions / spikes

1. RESOLVED: `krun_display_set_edid(ctx, display_id, blob, size)` exists (`libkrun.h:648`) and takes a raw EDID blob; custom EDID overrides all params except width/height and is unvalidated (`libkrun.h:634-637`). `KRUN_MAX_DISPLAYS=16` confirmed (`libkrun.h:614`).
2. **Runtime resize/hotplug.** Is there ANY post-`krun_start_enter` call to change a display's size / push new EDID / raise a virtio-gpu config-change? If not, this is the #1 patch. Spike: resize window → see if guest mode can change at all.
3. **Is the gpu device usable for plain 2D** without `VIRGLRENDERER_VENUS`/render-server, or are `krun_set_gpu_options2` flags mandatory for any display? Affects min footprint.
4. RESOLVED: libkrun calls `alloc_frame`→copy→`present_frame` **synchronously per flush** (`virtio_gpu.rs:528-536`); at most one `frame_id` is outstanding at a time within a flush, and the copy completes before `present_frame`. A 2-3 deep ring is optional (decouples main-thread present from the gpu-thread copy), not required. `OUT_OF_BUFFERS` is only hit if the backend itself refuses to vend.
5. RESOLVED: `display_width/height` = the configured `DisplayInfo` size (from `krun_add_display`); `width/height` = the guest's `SET_SCANOUT` resource size; they may differ (`virtio_gpu.rs:478-485`). Decide how limina reconciles a guest mode smaller than the window (letterbox vs scale-blit).
5b. RESOLVED: the per-flush copy is **always full-frame, tightly packed `width*4` stride** (`read_2d_resource`, `virtio_gpu.rs:496-512`); the `present_frame` damage `rect` is a present hint, not a copy bound. Wrap the `alloc_frame` buffer with exactly `width*4` bytes-per-row.
6. **UMA shared-buffer correctness:** confirm libkrun is happy writing into a `MTLBuffer.contents()` pointer we return from `alloc_frame` (alignment/stride: does libkrun assume tightly-packed `w*4` stride? `buffer_size` is "mostly a sanity check", `:113`).
7. **Main-thread present latency:** measure the cost of the gpu-thread→main-thread hop vs presenting directly from the vtable thread with a layer configured for background present.
8. **IOSurface ↔ virglrenderer/MoltenVK** interop feasibility on macOS (doc 03) before committing to v2 C.
9. RESOLVED: scanout cap = 16 (`KRUN_MAX_DISPLAYS`/`VIRTIO_GPU_MAX_SCANOUTS`). The internal scanout array is `[Option<VirtioGpuScanout>; 16]` (`virtio_gpu.rs:156`) — read `VirtioGpuScanout` + the flush→backend dispatch to resolve the CPU-copy mechanism and frame-ring depth (item 4).

---

## 7. References

Verified local source (line numbers confirmed this session):
- `~/Projects/limina/third_party/libkrun/include/libkrun_display.h` (symlink → `src/display/libkrun_display.h`) — vtable, formats, errors, threading: lines 11-33, 40, 54-182.
- `~/Projects/limina/third_party/libkrun/include/libkrun_input.h` (→ `src/input/libkrun_input.h`) — input event/config/provider vtables: lines 25-29, 67, 82, 93-109, 137-165.
- `~/Projects/limina/third_party/libkrun/examples/krun_gtk_display/src/main.rs` — config-time call sequence: lines 8-16, 130-221, 236-255. (Backend impl: `src/display_backend.rs`, `src/display_worker.rs`, `src/scanout_paintable/` — not yet read.)

Verified C API (this session):
- `~/Projects/limina/third_party/libkrun/include/libkrun.h` — `KRUN_MAX_DISPLAYS` (:614), `krun_set_gpu_options/2` (:595,:609), `krun_add_display` (:629), `krun_display_set_edid` (:648), `_set_dpi` (:663), `_set_physical_size` (:679), `_set_refresh_rate` (:693), `krun_set_display_backend` (:706), `krun_add_input_device` (:722), `krun_add_input_device_fd` (:736).
- `~/Projects/limina/third_party/libkrun/src/display/src/lib.rs:26` (`MAX_DISPLAYS=16`); `src/devices/src/virtio/gpu/protocol.rs:283` (`VIRTIO_GPU_MAX_SCANOUTS=16`); `virtio_gpu.rs:156` (`scanouts: [Option<VirtioGpuScanout>;16]`). Rust-side ABI wrappers exist at `src/display/src/{c_to_rust,rust_to_c,lib}.rs` and `src/input/src/{c_to_rust,rust_to_c,lib}.rs`.

To read next (bodies not fully opened this session):
- `~/Projects/limina/third_party/libkrun/src/devices/src/virtio/gpu/{virtio_gpu,display,edid,worker}.rs` (scanout/flush → backend dispatch, CPU-copy mechanism, ring depth, `display_*` vs `width/height`, EDID generation).
- `~/Projects/limina/third_party/libkrun/src/libkrun/src/lib.rs` (C-API impls; `MAX_DISPLAYS` check at :1684, `DisplayInfoEdid`/`PhysicalSize` at :64).
- `~/Projects/limina/third_party/libkrun/examples/gui_vm/src/main.rs` (note: this file's content this session was identical to the GTK example — confirm it isn't a symlink/duplicate; the brief expected SDL `window.rs`/`sdl_display_backend.rs` which were NOT present under `gui_vm/src/` — only `main.rs` + `krun_utils.rs` were listed).

External (verify via WebFetch when networked):
- virtio-gpu spec (OASIS virtio v1.x §5.7): scanout/flush/blob/EDID/display-info/cursor.
- Apple: `CAMetalLayer`, `MTKView`, `CVDisplayLink`, `IOSurface`, `NSWindow` fullscreen, `CGDisplayCapture`, `backingScaleFactor`, `MTLStorageModeShared` on Apple Silicon.
- Related limina docs: 03 (3D/blob zero-copy), 04 (input/keyboard).
