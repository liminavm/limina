# 03 — Graphics: virtio-gpu, rutabaga, virglrenderer, Venus

Scope: the full guest-to-host 3D/2D graphics path in libkrun on an Apple-Silicon macOS host running a Linux guest — the `virtio-gpu` device (`src/devices/src/virtio/gpu/*`), the vendored `rutabaga_gfx` abstraction, `virglrenderer` (with the `virgl`/`venus` context types), and how host rendering reaches Metal via MoltenVK. This is the reference for limina's "3D accel + fullscreen + present" feature area and the things we must patch to get a good Linux *desktop* (not just compute) experience. Line citations are against the locally cloned `third_party/libkrun` unless noted.

> Reality check confirmed by source: libkrun is **not** headless. The GPU device drives a pluggable display backend (`configure_scanout`/`alloc_frame`/`present_frame`) and there is a full C ABI for display + input (`libkrun_display.h`, `libkrun_input.h`, `krun_add_display`, `krun_set_display_backend`, `krun_add_input_device*`). Display requires the `gpu` build feature; input requires the `input` feature.

---

## 1. What exists today

### 1.1 Build features and library wiring

| Feature flag | Defined in | Pulls in |
|---|---|---|
| `gpu` (libkrun) | `src/libkrun/Cargo.toml:22` | `devices/gpu`, `rutabaga_gfx/virgl_renderer`, `rutabaga_gfx/virgl_renderer_next` |
| `virgl_resource_map2` (libkrun) | `src/libkrun/Cargo.toml:23` | same as `gpu` + `rutabaga_gfx/virgl_resource_map2` |
| `input` (libkrun) | `src/libkrun/Cargo.toml:24` | `devices/input` |
| `gpu` (devices) | `src/devices/Cargo.toml:27` | `rutabaga_gfx/virgl_renderer` + `virgl_renderer_next` |
| `virgl_renderer` / `virgl_renderer_next` / `virgl_resource_map2` / `gfxstream` / `vulkan_display` | `src/rutabaga_gfx/Cargo.toml:6-13` | rutabaga backend selection |

Build switches in the Makefile: `GPU=1` → `--features gpu`; `INPUT=1` → `--features input`; `VIRGL_RESOURCE_MAP2=1` → `--features virgl_resource_map2` (`Makefile:33-62`). `rutabaga_gfx/build.rs` links `-l virglrenderer` dynamically when `virgl_renderer` is on (so we depend on Homebrew `virglrenderer`).

The Homebrew libkrun 1.17.4 in `/opt/homebrew/lib` is built with `gpu` + `input` (its installed `libkrun.h` exposes `krun_set_gpu_options`, `krun_add_display`, `krun_set_display_backend`, `krun_add_input_device*`, and `KRUN_FEATURE_VIRGL_RESOURCE_MAP2 = 10` at `libkrun.h:991`). Whether that prebuilt was compiled with `virgl_resource_map2` must be confirmed at runtime via `krun_check_nested_virt`-style feature query (`KRUN_FEATURE_GPU` is checked at `src/libkrun/src/lib.rs:2008`). For limina we will almost certainly build our own libkrun, so we control all of this.

### 1.2 The C ABI surface (graphics-relevant)

From `/opt/homebrew/include/libkrun.h` and `src/libkrun/src/lib.rs`:

| C function | lib.rs | Purpose |
|---|---|---|
| `krun_set_gpu_options(ctx, virgl_flags)` | `lib.rs:1517` | enable virtio-gpu, store `virgl_flags` |
| `krun_set_gpu_options2(ctx, virgl_flags, shm_size)` | `lib.rs:1531` | same + set the SHM "vRAM window" size |
| `krun_add_display(ctx, w, h) -> display_id` | `lib.rs:1680` | add a scanout/display (max `KRUN_MAX_DISPLAYS=16`) |
| `krun_display_set_edid(ctx, id, blob, size)` | `lib.rs:1736` | override generated EDID |
| `krun_display_set_dpi(ctx, id, dpi)` | `lib.rs:1804` | DPI for generated EDID |
| `krun_display_set_physical_size(ctx, id, w_mm, h_mm)` | `lib.rs:1772` | physical size for generated EDID |
| `krun_display_set_refresh_rate(ctx, id, hz)` | `lib.rs:1704` | refresh rate for generated EDID |
| `krun_set_display_backend(ctx, *vtable, size)` | `lib.rs:1563` | install the host present callbacks |
| `krun_add_input_device(ctx, cfg, …, events, …)` | `lib.rs` (input feature) | virtio-input via vtable |
| `krun_add_input_device_fd(ctx, fd)` | `lib.rs:1608` | passthrough a host `/dev/input/*` (Linux host only) |

`krun_set_display_backend` `read_unaligned`s a `DisplayBackend` (size-checked against `size_of::<DisplayBackend>()`, `lib.rs:1568`) and calls `.verify()` (`lib.rs:1577`). When `gpu` is off these are stubs returning `-ENOTSUP` (`lib.rs:1551`).

### 1.3 virgl_flags bit definitions

Identical in `libkrun.h:544-554` and `src/rutabaga_gfx/src/rutabaga_utils.rs:351-361`:

```
USE_EGL=1<<0  THREAD_SYNC=1<<1  USE_GLX=1<<2  USE_SURFACELESS=1<<3
USE_GLES=1<<4 USE_EXTERNAL_BLOB=1<<5 VENUS=1<<6 NO_VIRGL=1<<7
USE_ASYNC_FENCE_CB=1<<8 RENDER_SERVER=1<<9 DRM=1<<10
```

`VirglRendererFlags::new()` default (`rutabaga_utils.rs:380-387`) = `use_virgl(true)` (i.e. NOT NO_VIRGL) + `use_venus(false)` + `use_surfaceless(true)` + `use_gles(true)` + `use_render_server(false)`. But libkrun never calls `new()` for the device — the flags come verbatim from the caller's `krun_set_gpu_options` value and are passed raw into `RutabagaBuilder::new(VirglRenderer, virgl_flags, 0)` (`virtio_gpu.rs:267-271`).

**Important: what the in-tree caller actually passes on macOS.** The `gui_vm` example (`examples/gui_vm/src/main.rs:153-161`) calls `krun_set_gpu_options2` with:
`USE_EGL | VENUS | RENDER_SERVER | THREAD_SYNC | USE_ASYNC_FENCE_CB` and `shm_size = 4096` (likely a placeholder; see §1.7).
The EGL/RENDER_SERVER bits are Linux-host context-creation concepts; on the macOS-patched virglrenderer the Venus→MoltenVK path is what runs. Notably **`USE_EXTERNAL_BLOB` is NOT set**, `NO_VIRGL` is not set (so virgl GL is also advertised), and **`DRM` is NOT set** (DRM native-context is a Linux passthrough path). krunkit presumably sets a similar mask, but I could not locate the exact flag literal in the cloned `third_party/krunkit` tree (grep found none) — **treat krunkit's exact flags as unverified.**

### 1.4 The virtio-gpu device

- Two virtqueues (control + cursor), 256 entries each; cursor queue is created but unused by the worker (`device.rs:29-31, 196-209`, `mod.rs:15` `NUM_QUEUES=2`).
- Advertised features (`device.rs:22-27`): `VIRTIO_F_VERSION_1`, `VIRTIO_GPU_F_VIRGL`, `_F_EDID`, `_F_RESOURCE_UUID`, `_F_RESOURCE_BLOB`, `_F_CONTEXT_INIT`. (Not advertised: the non-upstream `_F_RESOURCE_SYNC`, `_F_CREATE_GUEST_HANDLE`, `mod.rs:30-31`.)
- Config space reports `num_scanouts = displays.len()` and `num_capsets = 5` (`device.rs:165-166`).
- `activate()` spawns a single `"gpu worker"` thread (`device.rs:210-222`, `worker.rs:83-88`) which owns the control queue and the `VirtioGpu`/`Rutabaga` state. All GPU work is single-threaded on that worker.
- On macOS, `Gpu` carries a `Sender<WorkerMessage>` (`device.rs:39-40`) used to ask the main VMM thread to perform `hv_vm_map`/`unmap` for blob mapping (see §1.7).

### 1.5 Command handling (`worker.rs:116-347`)

`GpuCommand::decode` → `process_gpu_command`. 20 `VIRTIO_GPU_CMD_*` opcodes exist in `protocol.rs`. Implemented:
`GET_DISPLAY_INFO`, `GET_EDID`, `RESOURCE_CREATE_2D` (mapped to a 3D `TEXTURE_2D`/`RENDER_TARGET` resource, `worker.rs:129-146`), `RESOURCE_UNREF`, `SET_SCANOUT`, `RESOURCE_FLUSH`, `TRANSFER_TO_HOST_2D`, `RESOURCE_ATTACH/DETACH_BACKING`, `RESOURCE_ASSIGN_UUID`, `GET_CAPSET_INFO`/`GET_CAPSET`, `CTX_CREATE/DESTROY`, `CTX_ATTACH/DETACH_RESOURCE`, `RESOURCE_CREATE_3D`, `TRANSFER_TO/FROM_HOST_3D`, `SUBMIT_3D`, `RESOURCE_CREATE_BLOB`, `RESOURCE_MAP_BLOB`, `RESOURCE_UNMAP_BLOB`.
**Explicitly `panic!`/unimplemented**: `UPDATE_CURSOR`, `MOVE_CURSOR` (`worker.rs:190-195`), `SET_SCANOUT_BLOB` (`worker.rs:334-336`), and `GUEST_HANDLE` blobs (`virtio_gpu.rs:730`). `transfer_read` (TRANSFER_FROM_HOST_3D into a guest buffer) also `panic!`s in the device wrapper (`virtio_gpu.rs:591`) — the actual readback used for scanout uses `read_2d_resource` directly instead.

Fences: if a command has `VIRTIO_GPU_FLAG_FENCE`, a `RutabagaFence` is created and the descriptor is parked in `FenceState.descs` until the rutabaga fence handler retires it (`worker.rs:399-435`, handler at `virtio_gpu.rs:162-216`). The handler supports global and per-context/per-ring (`VIRTIO_GPU_FLAG_INFO_RING_IDX`) timelines and uses `max()` to avoid an out-of-order regression (`virtio_gpu.rs:208-214`). With `virgl_renderer_next`, async fence callbacks (`write_context_fence`, `virgl_renderer.rs:194-210`) require `USE_ASYNC_FENCE_CB`.

### 1.6 Scanout / present path (2D framebuffer to host window)

This is the macOS desktop-display path and is **2D-dumb-framebuffer only**:

1. `SET_SCANOUT` → `VirtioGpu::set_scanout` (`virtio_gpu.rs:430-489`): binds a resource to a scanout, validates the resource has a known `ResourceFormat`, then calls `display_backend.configure_scanout(scanout_id, display_w, display_h, w, h, format)`. `resource_id == 0` disables the scanout.
2. `RESOURCE_FLUSH` → `VirtioGpu::flush_resource` (`virtio_gpu.rs:518-546`): for each enabled scanout, `alloc_frame(scanout_id)` returns `(frame_id, &mut [u8])`; then `read_2d_resource` does a `Transfer3D` `transfer_read` of the whole resource (`stride = width * 4`, BGRA/4 bytes-per-pixel, `virtio_gpu.rs:491-515`) into that buffer; then `present_frame(scanout_id, frame_id, Some(rect))`.
3. The host backend (vtable from `libkrun_display.h`) does the actual blit/present. The contract: `create` once, then all calls on the **same thread** (the gpu worker), methods **must not block** (`libkrun_display.h:156-158`).

So the present path **copies pixels from the rendered resource into a CPU buffer the backend owns**, every flush. The backend (a macOS window) then uploads/presents that buffer. 3D content is rendered by virglrenderer into the resource on the host GPU, but the display still goes through a CPU read-back + copy. There is **no zero-copy scanout of a GPU texture to a CoreAnimation/Metal layer** today (`SET_SCANOUT_BLOB`, which would enable that, is unimplemented).

Display metadata: `DisplayInfo { width, height, edid }` (`display.rs:7-24`); EDID is either caller-provided or generated by `EdidInfo::new` from refresh-rate + physical-size/DPI (`display.rs:14-23`, generator in `edid.rs`). `NoopDisplayBackend` (`display.rs:65-102`) rejects everything — used when no backend is set (`builder.rs:1067` `DisplayBackend::new_noop()`), i.e. headless.

### 1.7 Blob resources, mapping, and the SHM "vRAM window"

- `RESOURCE_CREATE_BLOB` → `resource_create_blob` (`virtio_gpu.rs:719-749`). `blob_mem` values: `GUEST=0x1`, `HOST3D=0x2`, `HOST3D_GUEST=0x3` (`protocol.rs`). For `HOST3D` no guest iovecs are attached; otherwise guest pages are passed as iovecs. `GUEST_HANDLE` flag → `panic!` (unimplemented).
- The GPU SHM region is a reserved guest-physical window allocated by `ShmManager` starting at `arch_mem_info.shm_start_addr` (i.e. above guest RAM; the exact base is arch-defined, not a literal I verified). It is created only when `gpu_virgl_flags.is_some()`, with `size = gpu_shm_size.unwrap_or(1 << 33)` = **8 GiB default** (`builder.rs:1627-1631`, `create_gpu_region`; `ShmManager` in `device_manager/shm.rs`). The region is wired into the device via `set_shm_region(VirtioShmRegion { host_addr, guest_addr, size })` (`builder.rs:2514-2522`), where `host_addr` is the host VA of the region's guest_addr. On macOS the actual blob backing is mapped lazily through `hv_vm_map` (below), so the reserved window is GPA space, not committed host RAM.
- `RESOURCE_MAP_BLOB` → `resource_map_blob`. Three implementations:
  - **Linux, no `virgl_resource_map2`** (`virtio_gpu.rs:757-813`): `export_blob` an fd, host-side `mmap(MAP_SHARED|MAP_FIXED)` into `shm_region.host_addr + offset`. Rejects `OPAQUE_FD`.
  - **Linux, `virgl_resource_map2`** (`virtio_gpu.rs:814-889`): `SHM`/`DMABUF` fds are `mmap`'d directly; everything else goes through `virgl_renderer_resource_map2` (the patched-virglrenderer entry point, `virgl_renderer.rs:699-726`). The long comment (`virtio_gpu.rs:842-855`) documents the muvm camera/dma-buf fix.
  - **macOS** (`virtio_gpu.rs:890-942`): expects `RUTABAGA_MEM_HANDLE_TYPE_APPLE` (`= 0x0006`, `rutabaga_utils.rs:629`; note `0x0005` is ZIRCON, `0x0004` is SHM). Uses `rutabaga.map_ptr(resource_id)` (backed by `virgl_renderer_resource_get_map_ptr`, `virgl_renderer.rs:362-368`) to get a **host CPU pointer**, then sends `WorkerMessage::GpuAddMapping(map_ptr, guest_addr, size)` to the VMM thread (`virtio_gpu.rs:918-928`).
- The VMM thread handles it in `Vm::add_mapping` → `hvf_vm.unmap_memory(guest_addr,len)` then `hvf_vm.map_memory(host_addr=map_ptr, guest_addr, len)` (`vmm/src/macos/vstate.rs:120-156` region; `unmap`+`map` = `hv_vm_unmap`/`hv_vm_map`). This makes the host GPU blob's pages directly visible in the guest's address space (host-visible / DAX-style), so guest Mesa can read/write GPU memory without a copy. `RESOURCE_UNMAP_BLOB` reverses it (`virtio_gpu.rs:978-1013`).
- `map_info` returns cache attributes (`RUTABAGA_MAP_CACHE_*`) which are forwarded to the guest as `OkMapInfo` (`virtio_gpu.rs:810-812`); `map_info` ORs in `RUTABAGA_MAP_ACCESS_RW` on the virgl path (`virgl_renderer.rs:355`).

> Key macOS-specific patch already present in this tree: `RUTABAGA_MEM_HANDLE_TYPE_APPLE` (0x0006), `virgl_renderer_resource_get_map_ptr` (used only under `cfg(target_os="macos")`, `virgl_renderer.rs:361-368`), and `VIRGL_RENDERER_BLOB_FD_TYPE_APPLE` (`virgl_renderer.rs:405`) — these are **not** in upstream virglrenderer; they are the Apple/MoltenVK blob-export additions that the Homebrew (or our) `virglrenderer` must also carry. (Confirm the Homebrew build has the Apple patches before relying on this.)

### 1.8 rutabaga structure

- `RutabagaBuilder::new(RutabagaComponentType::VirglRenderer, virgl_flags, 0)` is the only component used by libkrun (`virtio_gpu.rs:267`). Other components exist in the vendored crate: `Rutabaga2D`, `CrossDomain` (Wayland/X11/PipeWire passthrough), `Gfxstream` (behind `gfxstream` feature, not built) — `rutabaga_core.rs:14-27`.
- Channels for cross-domain are auto-configured from `WAYLAND_DISPLAY`/`DISPLAY`/`PIPEWIRE_*` env vars (`virtio_gpu.rs:226-265`). On macOS only the Wayland channel struct is built (X11/PW are `cfg(target_os="linux")`), and there is no Wayland compositor, so cross-domain is effectively inert on macOS.
- Capsets advertised = 5 (`device.rs:166`); `RUTABAGA_CAPSET_VENUS=4` (`rutabaga_utils.rs:171`). The guest queries capsets to discover virgl (GL) vs venus (Vulkan) availability.
- Fallback: if building rutabaga with the requested flags fails, libkrun retries with `NO_VIRGL` only (`create_fallback_rutabaga`, `virtio_gpu.rs:284-300`) — a degraded mode that still allows blob/venus paths but disables virgl GL.

### 1.9 Host-side rendering on macOS (how pixels actually get made)

- **Venus (Vulkan)**: guest Mesa `venus` driver serializes Vulkan commands over virtio-gpu context-type Venus → host virglrenderer `venus` backend → **MoltenVK** → **Metal**. This is the high-performance path and the one krunkit/gui_vm enable (`VENUS` bit set).
- **virgl (GL)**: guest Mesa `virgl` Gallium driver → host virglrenderer GL backend. Upstream virglrenderer's GL path needs a host GL context (EGL/GLX). macOS has no system EGL; Homebrew `libepoxy` + a software/ANGLE-ish path would be needed, and there is no evidence libkrun's macOS build wires a working host GL context. **Assume virgl GL is effectively non-functional / unaccelerated on the macOS host** unless proven otherwise; the supported macOS path is Venus.
- **zink** (guest): guest OpenGL → Mesa `zink` → guest Vulkan → Venus → MoltenVK → Metal. This is how GL apps get acceleration on a Venus-only host: do GL→VK translation *in the guest*, not on the host. This is the strategically important path for a Linux desktop (most desktop apps are GL/GLX/EGL).

### 1.10a Copy vs share: a consolidated answer (verified)

The single most-asked question — "do guest and host share buffers, or is there copying?" —
has **three different answers depending on the path**, all verified against this tree:

| Path | What moves | Copy / serialize? |
|---|---|---|
| **Bulk GPU memory** (3D resources, Venus host-visible blobs) | the buffer's host pages are mapped into the guest | **Zero-copy, genuinely shared** |
| **Venus command submission** (Vulkan API forwarding) | the verbs are marshalled into the ring; large payloads are *referenced*, not streamed | Per-call (de)serialize of commands; **no buffer copy** |
| **Scanout → host window present** | the whole framebuffer is read back to a CPU buffer each flush | **Full-frame copy per present** |

1. **Bulk GPU memory is shared, not copied.** `resource_map_blob` (macOS path,
   `virtio_gpu.rs:891-942`) takes the renderer's own allocation via
   `rutabaga.map_ptr(resource_id)` (`:903`, backed by `virgl_renderer_resource_get_map_ptr`),
   and for `RUTABAGA_MEM_HANDLE_TYPE_APPLE` (`:906`) sends
   `WorkerMessage::GpuAddMapping(map_ptr, guest_addr, size)` to the VMM thread (`:918-926`).
   `Vm::add_mapping` (`vmm/src/macos/vstate.rs:133-151`) then does `hvf_vm.unmap_memory` +
   **`hvf_vm.map_memory(host_addr, guest_addr, len)` → `hv_vm_map`** (`hvf/src/lib.rs:274`).
   So the host GPU buffer's physical pages are stitched directly into the guest's SHM "vRAM
   window"; the guest CPU/GPU reads and writes the *same memory* — no copy, no serialization.
   This is the whole point of host-visible blobs / `USE_EXTERNAL_BLOB` / the `map2` patch.
2. **Venus serializes verbs, shares nouns.** Venus is API-remoting: Vulkan calls are encoded
   into the command ring and replayed on the host (the per-call cost behind the ~77% figure
   in §1.10), but the large data (textures, vertex/index/uniform buffers, render targets)
   lives in the shared host-visible blobs from (1) and is *referenced by handle*, not pushed
   through the ring. "Serialize the commands, share the buffers."
3. **Scanout present is a full-frame CPU copy — the one real copy on the hot path.**
   `flush_resource` (`virtio_gpu.rs:518-546`) runs, per flush:
   `alloc_frame(scanout_id)` → **`read_2d_resource`** → `present_frame`. `read_2d_resource`
   (`:491-515`) calls `rutabaga.transfer_read(...)`, copying the **entire** framebuffer
   (tightly packed BGRA, `stride = width*4`) out of GPU memory into the backend's CPU buffer.
   At 4K that is ~33 MB/frame (~2 GB/s at 60 Hz). Two sharp edges: the readback does
   `.unwrap()` on the transfer (`:512`) — a resource the renderer can't read **panics the GPU
   worker**; and `SET_SCANOUT_BLOB` (the zero-copy alternative) currently `panic!`s
   (`worker.rs:334`).

> **Nuance on (3) for our M1 host:** on UMA, if the limina display backend returns an
> `MTLBuffer.contents()` (`StorageModeShared`) pointer from `alloc_frame`, libkrun's
> `transfer_read` writes straight into memory Metal can sample — so it is effectively **one
> copy total** (the unavoidable virtio readback), not two, and there is no second host-side
> upload. See doc 09 §"Frame path". The fully zero-copy goal (no readback at all) is option C
> below: make the scanout resource itself a host-visible/IOSurface blob the layer samples.

### 1.10 Performance (external, label = reported)

Confirmed via web: **llama.cpp ggml-vulkan on macOS runs at ~77% of llama.cpp ggml-metal** on the same Mac, and libkrun's Vulkan *API-forwarding* (Venus→virglrenderer→MoltenVK) adds **minimal overhead on top of that** — i.e. the dominant gap is Vulkan-vs-Metal in the app, not the virtualization layer (Red Hat Developer 2025-09-18; Sergio López / sinrega.org; LunarG "State of Vulkan on Apple" Jan 2026). Red Hat also reports a combined ~40x improvement over the prior macOS-container GPU baseline. `vulkaninfo` in the guest shows the device as **"Virtio-GPU Venus (Apple M1)"**. Caveat: all of this measures **compute**; the *graphics present* overhead (the per-flush CPU read-back in §1.6) is a separate, publicly unmeasured cost that limina must benchmark itself.

---

## 2. How it works end to end

Boot/desktop frame, macOS host + Linux guest, Venus path:

```
Guest app (Vulkan)              Guest app (OpenGL)
   │                                 │  Mesa zink (GL→VK)
   ▼                                 ▼
Mesa venus driver  ◄──────────────── (zink emits Vulkan)
   │  virtio-gpu DRM driver (virtgpu): CONTEXT_INIT(capset=VENUS),
   │  CMD_SUBMIT_3D (serialized VK stream), CREATE_BLOB, MAP_BLOB,
   │  CREATE_FENCE
   ▼
[ virtio-gpu control vq ] ── kick ──► libkrun "gpu worker" thread
   ▼
worker.rs process_gpu_command ──► VirtioGpu (virtio_gpu.rs) ──► Rutabaga
   ▼
rutabaga VirglRenderer component (virgl_renderer.rs) ──► virglrenderer (Homebrew)
   ▼  venus backend
MoltenVK ──► Metal ──► Apple GPU
```

- **Memory for GPU buffers**: guest CREATE_BLOB(HOST3D) → host allocates → MAP_BLOB → libkrun `map_ptr` + `WorkerMessage::GpuAddMapping` → VMM thread `hv_vm_map(map_ptr → guest_addr in the 8 GiB SHM window)` → guest reads/writes GPU memory directly (DAX/host-visible). Fences (`CREATE_FENCE` + async callback) signal completion back through `RutabagaFenceHandler`, which retires the parked descriptor and raises the used-queue interrupt.
- **Display/present**: guest does normal virtio-gpu 2D scanout on the framebuffer it composited (often via the kernel's simple-KMS on the dumb scanout resource): `SET_SCANOUT` (configure host window size/format) then `RESOURCE_FLUSH`. libkrun reads the resource back to CPU (`read_2d_resource`) and hands the buffer to the host display backend's `alloc_frame`/`present_frame`. The macOS backend (we write it) presents the buffer in an `NSWindow`/`CAMetalLayer`.
- **Input** flows the other way via virtio-input devices (`krun_add_input_device*`), out of scope for this doc (see input doc).

Threads/locks: one gpu worker thread; virglrenderer is global, thread-bound, init-once (`virgl_renderer.rs:304-313`) — all rutabaga calls must stay on the worker thread. The display vtable is also called only from that thread. Blob map/unmap is the only cross-thread hop (worker → VMM via channel, synchronous reply).

---

## 3. Options inventory for limina

The limina display feature splits into two largely independent decisions: **(A) host rendering backend** (how 3D is produced) and **(B) host present/display backend** (how the framebuffer reaches the screen). Plus optional **(C) zero-copy scanout** work.

### A. Host rendering backend

**A1. Venus (Vulkan) over MoltenVK — reuse upstream (recommended).**
- Pros: already the supported macOS path; near-native compute perf; Fedora 43 guest ships Mesa with `venus` + `zink` so GL apps work via zink; minimal libkrun changes; matches krunkit/gui_vm config.
- Cons: every guest GL app pays GL→VK (zink) translation in-guest; Venus + MoltenVK feature gaps (MoltenVK is not a complete Vulkan 1.3); some extensions zink wants may be missing → fallback to llvmpipe (software) for unsupported apps. Present still CPU-copied (see C).

**A2. virgl (GL) on the host.**
- Pros: native-ish GL semantics for guest GL apps without zink.
- Cons: needs a working host GL context on macOS (EGL/GLX/CGL) — not wired in libkrun's macOS build; macOS GL is deprecated and capped at 4.1. Effectively a dead end on Apple Silicon. Do not pursue.

**A3. gfxstream.**
- Pros: another rutabaga backend; good for Android-style guests.
- Cons: not built in this tree (`gfxstream` feature off), designed around a different guest userspace; no Fedora desktop story. Skip.

**A4. Do nothing / software (llvmpipe in guest).**
- Pros: zero host GPU work, always correct.
- Cons: slow; fine only as a fallback. Already implicitly available.

### B. Host present/display backend (we must write one for macOS)

**B0. Reuse upstream.** There is no shipping macOS display backend in libkrun — `gui_vm` uses GTK (Linux), `krunkit` is headless (no `krun_set_display_backend`). So "do nothing" = headless. Not acceptable for limina.

**B1. Implement `krun_set_display_backend` vtable in Swift/Obj-C/Rust against `CAMetalLayer`/`NSWindow` (recommended).**
- Implement `configure_scanout` (size the layer + allocate N CPU frame buffers), `alloc_frame` (return a writable buffer), `present_frame` (upload buffer → texture → present, respecting damage rect). Double/triple-buffer to satisfy `alloc_frame` not blocking.
- Pros: full control of fullscreen, multi-display, HiDPI/Retina scaling, present timing; pure use of the existing stable ABI; no libkrun patch needed for basic display.
- Cons: CPU read-back + upload per frame (bounded by `read_2d_resource`); fine for desktop UI, suboptimal for full-screen 3D/video.

**B2. Build a Rust display backend crate (mirror `krun_gtk_display`) using `objc2`/`metal` crates.**
- Pros: stays in Rust, integrates with limina app; reuse the `DisplayBackend`/`Rect`/`ResourceFormat` types from the `krun_display` crate (`src/display/src/lib.rs`).
- Cons: same CPU-copy cost as B1.

### C. Zero-copy / accelerated scanout (optimization, patch libkrun)

**C1. Implement `SET_SCANOUT_BLOB` + present a GPU texture directly (the way to kill the one
remaining hot-path copy — see §1.10a path 3). This is a scheduled M4 task, not vague "later".**
- Today `SET_SCANOUT_BLOB` `panic!`s (`worker.rs:334`). Patch libkrun to (a) accept scanout-from-blob, (b) export the blob's Metal/IOSurface-backed texture, (c) extend the display vtable with a "present_texture(scanout_id, iosurface/blob_id)" path so the backend can wrap it in a `CAMetalLayer` drawable with no CPU copy.
- Pros: eliminates the per-frame read-back (§1.10a); needed for smooth full-screen 3D/video; also removes the `read_2d_resource` `.unwrap()` panic on the present path.
- Cons: requires libkrun + display-ABI changes and a virglrenderer that can export an IOSurface/Metal texture for a scanout resource (the Apple blob path exports a `map_ptr`, not necessarily a renderable texture handle). New ABI surface to design and maintain.
- **Sequencing:** the bulk-memory zero-copy (§1.10a path 1) already works and ships in M4
  with Venus; this scanout zero-copy is the *second* M4 deliverable, layered on the M2
  CPU-pull present path. Until it lands, present stays the (single, UMA-shared) copy from M2.

**C2. Present-sync / fences for tear-free display.**
- Couple `present_frame` to the guest's flush fence and to CAMetalLayer vsync. Likely needs explicit-sync plumbing (Mesa `virtio-gpu` explicit sync) to avoid presenting half-rendered frames. Patch territory.

### D. Memory window sizing
The default 8 GiB SHM "vRAM window" (`builder.rs`) is guest-physical reservation, not committed host RAM (mappings are lazy via `hv_vm_map`). limina can keep it large; revisit only if it collides with our dynamic-memory/ballooning GPA layout. Expose via `krun_set_gpu_options2`.

---

## 4. Recommendation

For the first milestone (boot `Fedora-Workstation-43.raw` to a usable desktop) and the near-term roadmap:

1. **Rendering: A1 (Venus over MoltenVK).** Build libkrun ourselves with `--features gpu`. Pass `virgl_flags = USE_EGL | VENUS | RENDER_SERVER | THREAD_SYNC | USE_ASYNC_FENCE_CB` to match the in-tree `gui_vm` macOS config (`main.rs:153-161`); the EGL/RENDER_SERVER bits are effectively no-ops on the macOS virglrenderer but we keep them for parity with the proven config. Do **not** set `NO_VIRGL` (keep virgl advertised) and do not bother with `DRM`/`USE_EXTERNAL_BLOB`. Rely on Fedora 43's in-guest Mesa `venus` for Vulkan and **`zink` for GL desktop apps**. Spike whether `virgl_resource_map2` is relevant on macOS (its `resource_map_blob` is a separate `cfg(macos)` impl, so likely Linux-only).
2. **Present: B1/B2 — write a macOS `CAMetalLayer` display backend** implementing the `krun_display_basic_framebuffer_vtable`. This is the single biggest piece of net-new code and unblocks the milestone with **no libkrun patch**. Use multi-buffering so `alloc_frame` never blocks; honor the damage rect in `present_frame`.
3. **C (zero-copy scanout) is a scheduled M4 task, sequenced after the M2 CPU-pull present and the
   M4 Venus/bulk-blob work — not an open-ended "someday".** Bulk 3D memory is already shared
   zero-copy via `hv_vm_map` (§1.10a path 1); the remaining copy is the per-flush scanout readback
   (§1.10a path 3). Removing it = patch libkrun to implement `SET_SCANOUT_BLOB` + extend the display
   ABI for an IOSurface/texture present (reusing the M4 virglrenderer Apple-blob build). Spike the
   IOSurface↔MoltenVK interop before committing the ABI.

What must be patched / built by us:
- **Build by us**: a macOS Metal/Cocoa display backend (the vtable) — required.
- **Verify, possibly patch**: that the Homebrew (or our) `virglrenderer` carries the **Apple blob patches** (`VIRGL_RENDERER_BLOB_FD_TYPE_APPLE`, `virgl_renderer_resource_get_map_ptr`). If not, build virglrenderer from the libkrun-flavored fork.
- **Later patch (libkrun + virglrenderer + display ABI)**: `SET_SCANOUT_BLOB` and zero-copy texture present; explicit-sync/present fencing; possibly cursor (`UPDATE_CURSOR`/`MOVE_CURSOR` are unimplemented → no hardware cursor; we'd composite the cursor or implement these).
- **Guest side**: Fedora 43 ships **Mesa 25.2.x** (e.g. `mesa-25.2.7-2.fc43`) which includes the `venus` Vulkan driver and `zink`; ensure the kernel has `CONFIG_DRM_VIRTIO_GPU=y/m` (default in Fedora Workstation; verify in the actual `.raw`). May want `MESA_LOADER_DRIVER_OVERRIDE`/`__GLX_VENDOR_LIBRARY_NAME`/zink env to force zink for GL where MoltenVK supports it. Note: the `.raw` is a 60 GiB MBR image with an EFI partition (ID 0xEF/0xEA), so the boot doc (01/02) must handle EFI/disk boot, not bundled-kernel boot.

---

## 5. Open questions / things to prototype

1. **Apple blob support in our virglrenderer.** Confirm Homebrew `virglrenderer` exports `RUTABAGA_MEM_HANDLE_TYPE_APPLE` / `virgl_renderer_resource_get_map_ptr`. If absent, MAP_BLOB on macOS fails and Venus host-visible memory breaks. Spike: build libkrun `gpu` + a trivial Vulkan guest, watch MAP_BLOB path.
2. **Does Fedora 43's guest Mesa pick venus automatically** over virtio-gpu, and does zink-on-venus actually accelerate desktop GL (GNOME, Firefox), or fall back to llvmpipe? Spike: boot the image, check `glxinfo`/`vulkaninfo`/`eglinfo` and `MESA` driver selection; measure GNOME Shell responsiveness.
3. **Present cost.** Measure `read_2d_resource` + upload per frame at 1440p/Retina. Is CPU-copy present good enough for desktop, or do we need C1 sooner? Spike: instrument the display backend.
4. **MoltenVK feature coverage** vs what zink/venus request (e.g. `VK_EXT_*` zink needs, geometry/tessellation, `hostImageCopy`). Identify apps that fall to software.
5. **Cursor.** With `UPDATE_CURSOR`/`MOVE_CURSOR` unimplemented, how does the guest cursor render? Likely software-composited by the guest; verify, else we must implement cursor commands or composite host-side.
6. **HiDPI/Retina + fullscreen + multi-display** mapping: how do `krun_add_display`/EDID DPI interact with macOS backing-scale factor? Decide whether to expose one large scanout or per-monitor scanouts.
7. **virgl_resource_map2 on macOS**: is it needed at all (the macOS MAP_BLOB impl is its own `cfg`), or is it Linux-only? Confirm the feature is harmless/no-op on macOS so our build flags are clean.
8. **Re-verify the ~75% Metal figure** and, separately, benchmark *graphics* (not just compute) present throughput, which the public numbers don't cover.
9. **EGL/DRM/RENDER_SERVER bits on macOS**: confirm they are truly ignored by the macOS virglrenderer (krunkit sets them) so we don't carry misleading flags — or trim them.

---

## 6. References

Local source (third_party/libkrun unless noted):
- Device: `src/devices/src/virtio/gpu/{mod.rs, device.rs, worker.rs, virtio_gpu.rs, display.rs, edid.rs, protocol.rs}`
- Key lines: `virtio_gpu.rs:267-300` (rutabaga build + fallback), `:430-546` (set_scanout/flush/read_2d), `:719-1013` (blob create/map/unmap), `:162-216` (fence handler); `worker.rs:116-347` (command dispatch), `:399-435` (fence parking); `device.rs:22-27` (features), `:165-166` (config), `:210-222` (worker spawn).
- Rutabaga/virgl: `src/rutabaga_gfx/src/{rutabaga_core.rs, rutabaga_utils.rs, virgl_renderer.rs}`; flags `rutabaga_utils.rs:351-361,378-449`; handle types `:184-188`; virgl FFI wrappers `virgl_renderer.rs:288-816` (init `:289-345`, map_ptr `:362-368`, export_blob `:388-418`, resource_map2 `:699-726`).
- C ABI: `src/libkrun/src/lib.rs:1517-1819` (gpu/display/input fns); headers `/opt/homebrew/include/{libkrun.h:544-706, libkrun_display.h, libkrun_input.h}`.
- macOS mapping: `src/vmm/src/macos/vstate.rs:133-161` (`add_mapping`/`remove_mapping` → `hvf_vm.unmap_memory`+`map_memory`, i.e. hv_vm_unmap/hv_vm_map). SHM window: `src/vmm/src/builder.rs:1627-1631` (size `unwrap_or(1<<33)`, `create_gpu_region`), `:2490-2523` (`attach_gpu_device`, `set_shm_region`); `src/vmm/src/device_manager/shm.rs` (`ShmManager`/`ShmRegion`). Handle types `rutabaga_utils.rs:624-629` (APPLE=0x0006).
- Build: `Makefile:33-62`; `src/libkrun/Cargo.toml:15-19`; `src/devices/Cargo.toml:17-19,44,52`; `src/rutabaga_gfx/Cargo.toml:10-21`; `src/rutabaga_gfx/build.rs` (probes `epoxy`+`virglrenderer`, `libdrm` only on Linux).
- Caller (macOS config): `examples/gui_vm/src/main.rs:153-161` (virgl_flags = USE_EGL|VENUS|RENDER_SERVER|THREAD_SYNC|USE_ASYNC_FENCE_CB, shm_size=4096), display/input wiring `:175-219`. krunkit flags: not found in the cloned tree (unverified).
- Protocol opcodes: `src/devices/src/virtio/gpu/protocol.rs:27-56` (0x100–0x301), blob constants `:78-86`. KRUN_FEATURE_* in `libkrun.h:981-991` (GPU=2, INPUT=4, VIRGL_RESOURCE_MAP2=10).
- Rust display types crate: `src/display/src/lib.rs`.

External (verified this session unless noted):
- Sergio López, "Enabling containers to access the GPU on macOS" — https://sinrega.org/2024-03-06-enabling-containers-gpu-macos/ (Venus→virglrenderer→MoltenVK→Metal; `vulkaninfo` shows "Virtio-GPU Venus (Apple M1)").
- Red Hat Developer, "Reach native speed with MacOS llama.cpp container inference" (2025-09-18) and "How we improved AI inference on macOS Podman containers" (2025-06-05) — API-forwarding/Venus, ~40x improvement.
- ggml-org/llama.cpp Discussion #8042 / LunarG "The State of Vulkan on Apple" (Jan 2026) — ggml-vulkan ≈ **77%** of ggml-metal on macOS; libkrun forwarding adds minimal overhead.
- Fedora Packages / Fedora Discussion — Fedora 43 ships **Mesa 25.2.x** (`mesa-25.2.7-2.fc43`).
- Mesa `venus`/`zink` driver docs (docs.mesa3d.org/drivers/venus.html).
- virglrenderer (gitlab.freedesktop.org/virgl/virglrenderer) + the libkrun/Apple-patched fork for `BLOB_FD_TYPE_APPLE` / `virgl_renderer_resource_get_map_ptr`.
