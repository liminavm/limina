# vrend zero-copy scanout: IOSurface-backed presents for the stock (virgl) tier

Status: DESIGN 2026-07-28. Follows the virgl-perf root-cause (`docs/hardening-backlog.md`
§GPU/rendering perf, memory `limina-virgl-vrend-perf`): virgl presents are
readback-per-frame + CPU convert; venus got zero-copy (`tier2-iosurface-zerocopy-present.md`)
and vrend was left on the fallback. Goal: stock-tier presents with no CPU pixel work and
fence-accurate pacing — the two-tier guarantee's "degraded" floor should degrade in GPU
throughput, not in present mechanics.

## Where the two chains stand today (mapped 2026-07-28, citations verified by exploration)

**venus (works):** vkr fix A intercepts external-memory `vkCreateImage`
(`virglrenderer/src/venus/vkr_image.c:70-174`), allocates the IOSurface
(`vkr_mtl_iosurface_alloc`, `vkr_metal_helpers.m:269-350` — registry + Mach-port publish,
deliberately not Vulkan-typed), and on KK backs a **LINEAR** VkImage with a host-pointer
import of `IOSurfaceGetBaseAddress` (`vkr_device_memory.c:574-599` →
`kk_device_memory.c:97,137` `newBufferWithBytesNoCopy`; linear planes texture straight off
the imported buffer, `kk_image.c:835-841`). The id rides `virgl_context_blob.iosurface_id`
→ `virgl_resource.iosurface_id` (only writer: `virglrenderer.c:1258`) →
`virgl_renderer_resource_get_iosurface_id` (component-agnostic, `virglrenderer.c:1270`) →
rutabaga caches at `create_blob` (`virgl_renderer.rs:932`) → `set_scanout_blob` resolves
(`virtio_gpu.rs:1646-1650`) → `flush_resource` → `try_park_present` (fence-accurate, ring
63, needs `resource.ctx_id != 0`) → `present_surface`.

**vrend (gap):** `VIRGL_BIND_SCANOUT` arrives (`vrend_renderer.c:8426` `gr->base.bind`)
and **dies** — its only consumers are `#ifdef WIN32` D3D and `ENABLE_GBM_ALLOCATION`
paths, both compiled out on macOS (meson darwin config sets neither). The scanout texture
is a plain `glTexStorage2D` texture (`vrend_renderer.c:8813`); st/mesa passes zink only
`SAMPLER_VIEW|RENDER_TARGET` binds; zink installs **no** `resource_get_handle` on KK (no
fd external memory) → private-heap MTLTexture, no export. Present = full-frame
`transfer_read` (glReadPixels to CPU) + per-pixel convert + canvas upload; the readback
path never reaches the fence-accurate present (`FENCEPRESENT`).

**The one live import channel on zink-on-KK:** `EXT_external_memory_host` →
`zink_resource_from_user_memory` (`zink_resource.c:2370`) → GL as **`GL_AMD_pinned_memory`**
(`st_extensions.c:1139`, buffers only). Same host-pointer mechanism venus uses.

## Plan: B → A1 → (A2 | C as escalations)

### B — id plumbing (no new mechanism; unblocks measurement)
1. `struct vrend_resource` gains `iosurface_id` (+ the IOSurface ref it owns), next to
   `egl_image`/`gbm_bo` (`vrend_renderer.h:~105`).
2. Publish it in `virgl_renderer_resource_create_internal` alongside `map_info`
   (`virglrenderer.c:~123`): `res->iosurface_id = vrend_renderer_resource_get_iosurface_id(...)`.
   The public getter needs no change.
3. rutabaga: cache `iosurface_id` in `create_3d` like `create_blob` does
   (`virgl_renderer.rs:932` vs `rutabaga_core.rs:168` hardcoded `None`) — or make
   `Rutabaga::iosurface_id` fall through to the component.
4. Worker: plain `set_scanout` resolves the id exactly like `set_scanout_blob`
   (`virtio_gpu.rs:1571-1577` currently hardcodes `None`).

### A1 — GPU-blit producer (no mesa/KK changes; kills all CPU pixel work)
On `vrend_resource_alloc_texture` for `bind & VIRGL_BIND_SCANOUT` (2D, level0, 1-sample,
BGRA8/RGBA8): allocate the IOSurface via `vkr_mtl_iosurface_alloc` (same library,
`meson.build:214-219`; registry + Mach publish come free — respect
`LIMINA_SURFACE_PORT_NAME` scoping), pick **RGBA** IOSurface format to match GLES
readback and skip any swizzle. Create a pinned-memory PBO over `IOSurfaceGetBaseAddress`
(`GL_EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD`). New fork API
`virgl_renderer_resource_sync_iosurface(res_handle)`: bind PBO, `glPixelStorei(GL_PACK_ROW_LENGTH,
bytesPerRow/4)`, `glReadPixels` the scanout rect (GPU-side copy into IOSurface bytes),
issue a vrend fence. Worker `flush_resource`, when scanout has an id and component is
vrend: call sync + park the present on the fence instead of `transfer_read`.
Present cost: one GPU blit + `present_surface(id)`. CPU touches no pixels.

### Fence-accurate present for vrend
- `try_park_present` requires `resource.ctx_id != 0` (only blob-create sets it) and
  injects on ring 63 (a vkr concept). vrend needs its own keying: fence on the flushing
  ctx's ordinary ring-0 timeline (vrend `fence_retire` = "GL work complete" — the signal
  exists) or a dedicated ctx0 fence id namespace. Design detail settled during
  implementation; the parking/holding machinery (`GuestFlushHold`, retire eventfd) is
  component-agnostic.
- ⚠️ Probe first: zink-on-KK likely advertises `EGL_ANDROID_native_fence_sync` through
  KK's `KHR_external_semaphore_fd` stub (`zink_screen.c:781-783`, `kk_physical_device.c:193`
  — no real `vkGetSemaphoreFdKHR`; cf. patches/mesa/0003-0004 NULL-guards). If
  `use_egl_fence` is true-but-broken, `virgl_egl_fence_create` returns `EGL_NO_SYNC_KHR`
  and vrend fence creation fails (`vrend_renderer.c:11393`). May need to force
  `use_egl_fence = false` on this winsys (glFenceSync path works).

### A2 — true zero-copy (escalation; small mesa hook)
Texture-from-user-memory: a gallium/GL path building a LINEAR image over the IOSurface
bytes (zink already handles images in `resource_from_user_memory` create path,
`zink_resource.c:1511-1519`, `:1131-1140`; force `PIPE_BIND_LINEAR` → `VK_IMAGE_TILING_LINEAR`,
the only tiling KK aliases to imported memory). Constraint: `vkGetImageSubresourceLayout`
rowPitch must equal IOSurface bytesPerRow exactly (venus does this dance and bails on
mismatch, `vkr_image.c:196-215`). Mutter then renders *directly* into the IOSurface;
even the A1 blit disappears. Risks: linear render-target perf on Apple GPUs, pitch
fragility, a mesa patch to carry.

### C — KK IOSurface external-memory handle type (widest; only if A2 too fragile)
Teach KK `VK_EXT_external_memory_metal` a texture/IOSurface handle backed by
`newTextureWithDescriptor:iosurface:plane:` (choke point `kk_image.c:835`), letting
*tiled* images be IOSurface-backed and making zink `resource_get_handle` wireable. Blast
radius: KK device memory + image + zink screen caps.

## Spike (task #2, before any wiring)
Extend `spikes/virgl-zink-kk/`:
1. `GL_AMD_pinned_memory` actually exposed by zink-on-KK surfaceless? (`eglprobe` +
   extension check + functional pinned-PBO readback into a malloc'd buffer.)
2. glReadPixels into a pinned PBO over a real IOSurface: pixels land, verified via
   `iosdump` (needs `LIMINA_GLOBAL_SCANOUT=1`) — black-by-design regions per
   `limina-render-verify-golden`.
3. The EGL fence probe: does `virgl_egl_fence_create` on zink-on-KK return a working
   sync?
4. Measure the A1 blit cost at 2560×1440 (µs, GPU) to size the win before wiring.

## Success criteria
- Stock F44 guest, windowed, release AND debug: overview animation presents zero
  `transfer_read` calls (worker log), `FENCEPRESENT` lines appear, frame gaps ≤ 17 ms
  median (parity with today) with CPU present cost ~0.
- Two-tier guarantee intact: a guest whose format/size falls outside the IOSurface path
  (non-BGRA/RGBA scanout, multisample) silently keeps the readback fallback — the
  readback path is not removed.
- venus unaffected (L2 suite green, `venus_fd_census` included).
