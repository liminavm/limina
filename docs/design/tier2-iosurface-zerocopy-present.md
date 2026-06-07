# Tier-2 design: IOSurface zero-copy scanout present (crossings B + D)

Status: design / scout (2026-06-07). De-risked by `spikes/moltenvk-iosurface/` (commit
`fb65e7a`). Continues `docs/design/tier2-coexist-gpu.md` (Phase 3) and `docs/roadmap.md`
M4 task 4. Venus orientation + the four-crossings frame: memory `limina-tier2-venus`.

## Goal

Present a venus-rendered scanout to the macOS display with **no CPU copy**, and let the
**mutter compositor run on venus** (its KMS scanout images currently fail to create). Both
reduce to one capability: **IOSurface is the macOS dmabuf** — back venus exportable/scanout
images with an IOSurface-imported `MTLTexture`, and present that IOSurface directly.

The spike proved the GPU half is viable on MoltenVK 1.4.1 via the **import path**: create an
IOSurface → `newTextureWithDescriptor:iosurface:` → import as a `VkImage` backing
(`VkImportMetalTextureInfoEXT`) → GPU renders into it → the IOSurface's bytes show the write
zero-copy → IOSurface is accepted as `CALayer.contents`.

## The pleasant surprise: the cross-process present seam is ALREADY IOSurface

The VMM worker (`limina-vmm`, runs libkrun + vkr) and the AppKit UI (`limina` supervisor, owns
the `NSWindow`/`CAMetalLayer`) are **separate processes**. The worker→supervisor frame
transport is already a **global IOSurface looked up by id**:

- `crates/limina-display/src/iosurface.rs` — the window display backend creates a ring of
  `kIOSurfaceIsGlobal` IOSurfaces (`configure_scanout`, ~:145-170) and, on `present_frame`
  (:209-264), copies the frame into the next ring surface and sends `"frame <id>"` over
  `control_fd` (:262).
- The supervisor (`crates/limina/src/window.rs`) does `IOSurfaceLookup(id)` and shows it in the
  `CAMetalLayer`.

So E2 (worker→supervisor) is **already zero-copy**. What is NOT zero-copy today is the *feed*
into that IOSurface — two copies:

1. **GPU→CPU readback.** `virtio_gpu.rs flush_resource` (:637-679) calls `read_2d_resource`
   (rutabaga `transfer_read`) into the `alloc_frame` buffer — the per-frame full-framebuffer
   readback (the likely cause of #29's windowed ~40 vs headless ~440).
2. **CPU→IOSurface.** `present_frame` swizzles into a canvas and `copy_canvas_into_surface`
   (iosurface.rs:349-356) memcpys the canvas into the global IOSurface.

**The zero-copy target:** have vkr render the venus scanout image *directly into a global
IOSurface*, and publish *that* IOSurface's id to the supervisor via the existing `"frame
<id>"` channel — eliminating both copies. The supervisor side needs **no change** (it already
presents any global IOSurface by id).

## Data flow

```
TODAY (2 copies):
 guest mutter → venus VkImage → [vkr renders to a plain MTLTexture]
   → guest exports image as a virtio-gpu blob → SET_SCANOUT_BLOB (panics today)
     ─ today's working path is plain SET_SCANOUT + RESOURCE_FLUSH ─
   → libkrun flush_resource: read_2d_resource READBACK ⟶ alloc_frame buf   (copy 1)
   → display backend: swizzle+copy_canvas_into_surface ⟶ global IOSurface   (copy 2)
   → "frame <id>" → supervisor IOSurfaceLookup → CAMetalLayer

TARGET (zero copy):
 guest mutter → venus VkImage [vkr backs it with a GLOBAL IOSurface-imported MTLTexture]
   → GPU renders straight into the IOSurface
   → SET_SCANOUT_BLOB → libkrun resolves resource_id → vkr's IOSurface id (in-process)
   → new display vtable: present_surface(scanout_id, iosurface_id)
   → "frame <id>" → supervisor IOSurfaceLookup → CAMetalLayer     (no copies)
```

## Touch-points (with file:line)

**A. vkr host VkImage create — back exportable images with an IOSurface.**
`third_party/virglrenderer/src/venus/vkr_image.c:11-35` (`vkr_dispatch_vkCreateImage` →
`vkr_image_create_and_add` → generated `vkr_image_gen.h` calls `vk->CreateImage`). Today it
forwards the guest pNext verbatim; the comment (:15-31) notes images are not forced external.
On `__APPLE__`, when the guest chains a `VkExternalMemoryImageCreateInfo` with a handle type
MoltenVK can't honor (the #30 failure: `extHandleTypes=0x1` → `VK_ERROR_FEATURE_NOT_PRESENT`),
**create a global IOSurface + `newTextureWithDescriptor:iosurface:`, replace the external info
with `VkImportMetalTextureInfoEXT`, and record resource→IOSurface(id).** This both (D) makes
mutter's `vkCreateImage` succeed and (B) makes the image presentable. (Trigger = "guest wants
this image exportable/for scanout"; refine the exact predicate during impl.)

**B. vkr IOSurface allocator (new) — mirror the shm/Metal-buffer model.**
Model: `vkr_metal_helpers.m:39-83` (`vkr_mtl_shm_alloc`: shm_open+mmap+`newBufferWithBytesNoCopy`
Shared MTLBuffer) and its caller `vkr_device_memory.c:302-326` (HOST_VISIBLE → MTLBuffer import,
`VkImportMemoryMetalHandleInfoEXT`). Add a sibling `vkr_mtl_iosurface_alloc(device, w, h, fmt)`
returning `{ IOSurfaceRef (global), id<MTLTexture> }`. Scanout images use the **image** import
(A); this is the shared helper.

**C. Export channel — resolve resource_id → IOSurface, IN-PROCESS.**
vkr and libkrun are in the **same process** (render-server is a thread), so no fd serialization
is needed for present: the `IOSurfaceRef`/`IOSurfaceID` is a valid in-process handle. Mirror the
existing host-pointer shim `virgl_renderer_resource_get_map_ptr`
(`third_party/virglrenderer/src/virglrenderer.c:1356-1373`) with a new
`virgl_renderer_resource_get_iosurface_id(res_handle, uint32_t *id)` that returns the IOSurface
id for an image-backed resource. (The map_blob/`get_map_ptr` path stays for host-visible *memory*
blobs / #28; scanout images take this new path.)

**D. libkrun SET_SCANOUT_BLOB — implement (today panics).**
`third_party/libkrun/src/devices/src/virtio/gpu/worker.rs:391-392`
(`GpuCommand::SetScanoutBlob(_) => panic!`). Implement: look up the resource, call the new
virgl API to get its IOSurface id, mark the scanout as IOSurface-backed, and **skip the
readback** in `flush_resource` for it. Resource→host-handle lookup precedent:
`virtio_gpu.rs resource_map_blob:1025-1081` (already calls `rutabaga.export_blob`/`map_info`).
`set_scanout`/`flush_resource` live at `virtio_gpu.rs:549-679`.

**E. Display C ABI + window backend — add a surface-present path.**
`third_party/libkrun/include/libkrun_display.h` is CPU-buffer only today (`alloc_frame` →
`uint8_t*`, then `present_frame`; vtable struct :210-229). Add a feature
`KRUN_DISPLAY_FEATURE_SURFACE` + `present_surface_fn(instance, scanout_id, iosurface_id,
damage)`. Implement in `crates/limina-display/src/iosurface.rs`: on `present_surface`, just
`send("frame {iosurface_id}")` — **reuse the existing supervisor lookup**, skip
alloc_frame/swizzle/copy entirely. The **capture** backend (`lib.rs`) implements it by
`IOSurfaceLookup`+read for the PNG oracle. Supervisor (`crates/limina/src/window.rs`) is
unchanged. Worker selects the backend at `crates/limina-vmm/src/krun/mod.rs:196-203`.

**D′ (#30 coexistence). Extension filter.** `vkr_device.c:150-176` strips
`external_memory_dma_buf`/`image_drm_format_modifier`/`queue_family_foreign`/`external_memory_fd`
on `__APPLE__` and re-appends `external_memory_metal`+`metal_objects` if MoltenVK supports them.
The IOSurface image path (A) must coexist with this — we satisfy the guest's external-image
*intent* via IOSurface host-side rather than forwarding the unsupported handle types.

## Open design questions (resolve when implementing — host-side where possible)

1. **~~Resource→IOSurface linkage~~ — PARTLY RESOLVED by the Phase 1b capture (2026-06-07,
   `spikes/moltenvk-iosurface/scanout-capture.md`).** The failing scanout image is a single,
   precise type: `VK_FORMAT_B8G8R8A8_UNORM` (BGRA8 — our IOSurface format), display-sized
   (1280×800), OPTIMAL, `MUTABLE_FORMAT`, with `VkExternalMemoryImageCreateInfo handleTypes=0x1`
   (OPAQUE_FD) + `VkImageFormatListCreateInfo`. It fails **host-side** at vkCreateImage with
   `VK_ERROR_FEATURE_NOT_PRESENT` (host MoltenVK lacks external_memory_fd, which we strip). So
   **fix A's trigger = on `__APPLE__`, vkCreateImage with a `VkExternalMemoryImageCreateInfo`
   (handleTypes != 0)** — narrow (only this one image type; the other ~325 format-list-only
   images already succeed). The scanout image uses a **dedicated allocation** (`vkAllocateMemory
   dedImage=…, exportHandleTypes=0`) and does **NOT** go through `export_blob` — so the resolve
   (C) is **image→IOSurface directly** (track the id on `struct vkr_image`), not via the blob
   channel. STILL OPEN: part (b) — how the image links to the SET_SCANOUT_BLOB resource — is
   unobservable until A makes the create succeed; capture it in Phase 2. Also handle
   `MUTABLE_FORMAT`: the IOSurface MTLTexture must allow the format-list view formats (BGRA8
   UNORM+sRGB; `MTLTextureUsagePixelFormatView`).
2. **Multi-buffering / tearing.** Today's backend uses a 3-deep IOSurface ring so the
   compositor never samples a half-written surface (iosurface.rs comments ~:83). With venus
   rendering directly, rely on the guest's own swapchain/double-buffering, or have vkr rotate a
   small IOSurface pool per scanout. Need a present/acquire handshake so we publish a surface
   only after the GPU finished it (the venus fence we already retire, #27).
3. **Format/modifier negotiation.** mutter/venus request format + modifier; IOSurface is BGRA8
   linear. Confirm the negotiated format matches (the spike used BGRA8) and handle the
   X8/A8 and stride cases (`KRUN_DISPLAY_FORMAT_*` already enumerates them).
4. **#28 is NOT on this path.** Present reads host-side (GPU→IOSurface→supervisor, all host
   coherent). Guest-side readback (`glReadPixels`, screenshots, feedback) is crossing C / #28,
   separate. Do not conflate.

## Phasing (each independently testable, host-side first)

1. ✅ **DONE — vkr IOSurface image backing (A+B).** Allocator `vkr_mtl_iosurface_alloc` (fork
   `59d02c6`, proven host-side, `spikes/moltenvk-iosurface/vkr_alloc_test.m`). Fix A
   (`vkr_image.c` fork `c99448b`): on `__APPLE__`, a vkCreateImage carrying
   `VkExternalMemoryImageCreateInfo` (handleTypes!=0) with a mappable format → allocate a global
   IOSurface, drop the external info, chain `VkImportMetalTextureInfoEXT`; track on
   `struct vkr_image.mtl_iosurface`, free on destroy. (C resolve API still TODO — needs part-(b).)
2. ✅ **DONE (image-create) — #30 Vulkan wall cleared.** Booted mutter-on-venus (2026-06-07):
   all 14 external scanout `vkCreateImage` now return **ret=0** (IOSurface-backed) instead of -8,
   and gnome-shell loads `libvulkan_virtio`. **NEW WALL (separate): mutter's KMS/GBM scanout** —
   `Failed to initialize accelerated iGPU/dGPU framebuffer sharing: KMS CRTC doesn't support GBM
   format` → gnome-shell `ABRT`, *before* it reaches SET_SCANOUT_BLOB. This is a virtio-gpu KMS
   plane/format-negotiation issue (guest kernel + how the device advertises scanout formats),
   distinct from the Vulkan image path. Part-(b) (image→SET_SCANOUT_BLOB linkage) is still
   unobservable until this KMS wall is cleared. **→ next investigation.**
3. **Zero-copy present (D+E).** Implement SET_SCANOUT_BLOB + `present_surface`; publish vkr's
   IOSurface id; remove the readback for IOSurface scanouts. Verify no `read_2d_resource` per
   frame (trace) and that windowed perf rises toward the headless ~440 (#29).
4. **Hardening.** Multi-buffer/acquire handshake; format/modifier matrix; capture-oracle parity.

## RED-first tests (drive the shipped binaries; `crates/limina-test`)

- L0/host: a vkr-path unit (or spike) asserts an exportable image yields a global IOSurface id
  and the GPU clear is visible in it (extends `spikes/moltenvk-iosurface`).
- L1/L2 (HVF, enhanced 16k guest): mutter-on-venus boots without the #30 image-create failure;
  a windowed venus app shows **no per-frame `read_2d_resource`** in a libkrun trace; the capture
  oracle still produces a correct non-black PNG via the surface path.
