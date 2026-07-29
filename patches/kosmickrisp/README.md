# patches/kosmickrisp — limina host KosmicKrisp + host-zink patches

KosmicKrisp (KK) is the **host** Vulkan-on-Metal driver (`libvulkan_kosmickrisp.dylib`) that
backs the guest's venus over virglrenderer. The limina enhanced-tier desktop runs
**guest zink → guest venus → virglrenderer (vkr) → KK → Metal → Apple GPU**. This dir carries
our patch series over the KK mesa tree.

Unlike the guest mesa build (`patches/mesa/`, built in the Apple `container`), KK is built
**natively on macOS** from a **case-sensitive APFS volume** `/Volumes/mesa-cs/mesa` (the host
APFS is case-insensitive and can't even check out mesa). The shipped dylib lives at
`target/limina.app/Contents/Frameworks/libvulkan_kosmickrisp.dylib`. See
`docs/drivers/kosmickrisp.rst` for the build recipe.

## Base
- **`UPSTREAM_BASE`** = `178a3d73968` (`egl/gbm: Eliminate local variable "max_age" in
  get_back_bo`). Mesa main; the `/Volumes/mesa-cs/mesa` checkout is pinned here.

## Patches
- **`0001-limina-KosmicKrisp-zink-host-stack-patches-for-the-v.patch`** — the full
  host-tree delta (21 files, ~1340 lines) that builds the shipped KK dylib and the host
  zink-on-KK GL stack. **Recovered 2026-06-24**: it had only ever existed as an
  *uncommitted working tree* in `/Volumes/mesa-cs/mesa` — never committed or exported, the
  enhanced-tier discipline gap (same class of bug as the venus dma-buf patch,
  `patches/mesa/0008`). Now committed on the `limina/kosmickrisp` branch in that tree.
  Components (a follow-up **should split these into logical patches**):
  - **kosmickrisp/vulkan** — encoder + render-pass + bind-cache (`kk_encoder.c`,
    `kk_cmd_buffer.*`), draw path (`kk_cmd_draw.c`, +484), transform-feedback NIR lowering
    (`kk_nir_lower_xfb.c` [new], `kk_shader.*`, `meson.build`), queries (`kk_query_pool.c`),
    external-memory opaque-fd/dma_buf host side (`kk_device_memory.c`,
    `kk_physical_device.c`), pool/descriptor/device.
  - **kosmickrisp/bridge** — Metal encoder/buffer/texture/command-buffer (`mtl_*.m/.h`).
  - **zink** — `driCheckOption` driconf guard on the surfaceless→sw-vk→zink loader path
    (`zink_screen.c`); see `spikes/virgl-zink-kk`.
  - **venus** — `vn_wsi.c` present-region (mirror of `patches/mesa/0005`; incidental in this
    host tree, which builds KK not venus).
- **`0002-kk-lay-out-DRM-format-modifier-attachment-images-as-.patch`** — render a
  DRM-format-modifier image used as a color/depth **attachment** as *tiled* (heap-backed)
  instead of *linear* (buffer-backed). venus (`patches/mesa/0008`) force-advertises
  `EXT_image_drm_format_modifier`, so mutter allocates its render targets with
  `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`; KK forced every non-OPTIMAL image linear →
  `newTextureWithDescriptor:offset:bytesPerRow:` buffer-backed texture, which **Metal won't
  accept as a render-pass attachment** → `mtl_new_render_command_encoder_with_descriptor`
  returns nil → `kk_render_encoder` asserts (`kk_encoder.c:299`) before the desktop renders.
  In a VM the DRM modifier is host-meaningless (backing is a host Metal heap), so an
  attachment modifier image is laid out tiled/renderable. Plain `VK_IMAGE_TILING_LINEAR`
  images stay linear — that path is vkr's IOSurface-backed scanout (renderable via
  `newBufferWithBytesNoCopy`). One conditional in `kk_image_layout_init`; with it the
  accelerated GNOME desktop renders through venus→KK→Metal. (It then surfaced a separate
  attachment-less render-pass crash, fixed in `0003`.)
- **`0003-kk-clamp-attachment-less-render-pass-target-size-sam.patch`** — clamp an
  **attachment-less** render pass's Metal render-target size and sample count to **≥ 1**.
  A `vkCmdBeginRendering` with zero color/depth attachments has nothing to derive the target
  size/sample count from; KK took `renderTargetWidth/Height` from the guest `renderArea`
  (which can be **0×0** — seen from `gst-plugin-scan`'s zink GL probes) and
  `defaultRasterSampleCount` from the pipeline (which can be 0). Metal returns nil for a
  0-sized / 0-sample attachment-less descriptor → `kk_render_encoder` asserts and kills the
  worker. Clamp both to ≥ 1 (a 0-area pass renders nothing anyway). Was masked by the `0002`
  crash; with both, the accelerated desktop boots and **stays up** through session startup.
- **`0004-kk-give-heap-less-host-imported-tiled-image-planes-a.patch`** — a guest **wgpu** app
  (ghost-ui, wgpu 29 over venus) crashed the worker: `kk_image_plane_bind` asserted
  (`plane->layout.linear || mem->bo->mtl_handle`) on an **OPTIMAL-tiling color attachment** (a
  wgpu render target) bound to **host-pointer-imported** memory. KK can't make a heap from a host
  pointer (`kk_device_memory.c`), so `mem->bo->mtl_handle` is NULL and a tiled image — which
  can't be textured from a buffer — aborts the bind. venus host-imports it because
  wgpu/gpu-allocator **maps** the memory (KK exposes a single host-visible type, so every
  allocation is mappable); mutter dodges it only because mesa doesn't persistently map its render
  targets. Steering tiled images to a `DEVICE_LOCAL`-only type did **not** work — venus ignores a
  host-side type the guest never maps. Fix host-side: a non-linear plane bound to heap-less memory
  gets its **own** heap-backed `kk_bo` (`kk_alloc_bo`, a real Metal heap) and is textured from it
  (freed in `kk_image_plane_finish`). Safe because an OPTIMAL image is opaque to the guest CPU
  (touched only via GPU copies/renders). Validated with ghost-ui on the F44 enhanced guest: the
  worker survives and the previously-fatal images take the private-heap path. NOTE: a separate
  gap then surfaces — see TODO (`TEXTURE_FORMAT_16BIT_NORM`).
- **`0005-kk-advertise-VK_EXT_custom_border_color-lift-zink-on.patch`** — advertise
  `VK_EXT_custom_border_color`. The sampler impl (`kk_sampler.c`:
  `VK_BORDER_COLOR_{INT,FLOAT}_CUSTOM_EXT`, `sampler->custom_border`) and the
  `maxCustomBorderColorSamplers` property were already in tree, but the extension + its feature
  bits (`customBorderColors`, `customBorderColorWithoutFormat`) were never exposed, so the driver
  didn't actually offer the feature. zink lists it as a **base requirement** and warned loudly
  about it. NOTE: contrary to the patch subject, this was **not** what capped GL at 3.1 — it
  cleared a correctness *warning* but the version was unchanged (a lead-not-cause; `0006`'s
  `ARB_depth_clamp` was the actual 3.1 gate). Still worth having (removes a real zink base-req
  warning). Built into the shipped `.app` since 2026-06-30.
- **`0006-kk-advertise-VK_EXT_depth_clip_enable-GL_ARB_depth_c.patch`** — advertise
  `VK_EXT_depth_clip_enable` + the `depthClipEnable` feature, and make `kk_cmd_draw.c` honor the
  decoupled state via `vk_rasterization_state_depth_clip_enable()` (Metal `setDepthClipMode:`
  clip/clamp; clip-disabled → clamp). KK already had core `depthClamp` + the Metal bridge; this
  exposes the EXT zink needs to advertise `GL_ARB_depth_clamp`, which is a **Mesa desktop-GL 3.2
  version gate** (`version.c` `ver_3_2`). **Verified it lifts zink-on-KK from GL 3.1 → GL 3.2 core**
  (`glprobe`: `ctx granted 3.2 core`; `glprobe-after-0006.txt`) and clears the depth_clip_enable
  warning. Corrects the earlier wrong "geometry shaders cap at 3.1" theory — Mesa's desktop 3.2/3.3
  gates don't require geometry shaders (only the GLES path does). Next gate for 3.3 = the 3.3 ext
  set (`ARB_blend_func_extended`/`dualSrcBlend`, `ARB_timer_query`, …), not geometry shaders.

- **`0007-kk-clamp-guest-controlled-transform-feedback-buffer-.patch`** — **security fix.**
  The XFB command handlers `0001` added index the fixed 4-element `gfx->xfb.buf[]`
  (`kk_cmd_buffer.h`) with a **guest-controlled** base and no clamp:
  `kk_CmdBindTransformFeedbackBuffersEXT` (`idx = firstBinding + i`) and
  `kk_Cmd{Begin,End}TransformFeedbackEXT` (`idx = firstCounterBuffer + i`). venus is
  untrusted from KK's side; a non-conformant guest passing a value past
  `maxTransformFeedbackBuffers` (4) makes KK write a guest-controlled buffer
  address/size to a guest-controlled offset past the array — a host memory-corruption
  primitive (controlled address **and** value). Clamp each loop (`idx` is monotonic, so
  `break` once out of range). Found in the 2026-07 upstreaming triage
  (`docs/upstreaming/00-obvious-fixes-and-security.md` §2B); reproduced RED/GREEN with
  `spikes/venus-draw-probe/xfb-oob-probe.c` (pre-fix SIGBUS on a write 1.34 GB past the
  array; fixed survives — see `spikes/venus-draw-probe/xfb-oob-RESULTS.md`). Upstream KK
  has no XFB (limina-authored), so no coordinated-disclosure embargo. When `0001` is
  split, this folds into the XFB-lowering component.
- **`0008-kk-implement-timestamp-queries-vkCmdWriteTimestamp2-.patch`** — fills KK's upstream
  timestamp-query TODO (`timestampValidBits` hardcoded 0 + empty `kk_CmdWriteTimestamp2`, both
  from the "kk: Add KosmicKrisp" import). This was the **sole remaining GL 3.3 gate** for
  zink-on-KK (`GL_ARB_timer_query`, gated on `timestampValidBits > 0`), so the baseline GL tier
  goes **3.2 → 3.3 core / GLSL 330**. Apple GPUs sample the GPU timestamp counter only at
  `MTLCounterSamplingPointAtStageBoundary` and the resolved value is already CPU-ns
  (`timestampPeriod=1`): a one-shot blit encoder samples at its start boundary and a **separate
  fenced** blit encoder `resolveCounters` straight into the query report BO **in GPU command
  order** (a CPU completion-handler write would race an in-stream `CmdCopyQueryPoolResults` and
  KK's GPU-encoded fence signal); in-render-pass writes defer to pass end. Gated on
  `mtl_device_supports_timestamps()`. Bridge additions: `mtl_device_supports_timestamps`,
  `mtl_new_timestamp_sample_buffer`, `mtl_new_blit_command_encoder_timestamp`,
  `mtl_blit_resolve_timestamp`. Design + Metal probes + validation in `spikes/kk-timestamp/`;
  real-timer exercise in `spikes/virgl-zink-kk/timerprobe.c`. Upstreamable (fills an upstream TODO).
  **The GPU resolve described above is gone as of `0013`** — see below; the parenthetical about a
  CPU write racing `CmdCopyQueryPoolResults` was the right worry and is now answered with explicit
  ordering rather than by avoiding the CPU.
- **`0009-vk-meta-skip-empty-rects-in-vk_meta_clear_attachments.patch`** — guest-triggerable VMM
  abort: an empty clear rect trips a KK `vk_meta` assert.
- **`0010` / `0011` — timestamp resolve, superseded by `0013`.** `0010` probed for GPUs that cannot
  resolve a counter sample from the command buffer that took it and split the command buffer there;
  `0011` made that probe fail-safe and multi-sample after a single-sample probe cached "healthy" on
  ~4% of boots. Both narrowed the M4 Pro failure (94% → 18%) without closing it, because the real
  rule is that a sample materialises at command-buffer **completion**, which no amount of GPU-side
  scheduling can wait for. `0013` deletes the probe and the GPU resolve entirely.
- **`0012-kk-the-timestamp-sampling-encoder-must-carry-work-th.patch`** — the sampling blit encoder
  encoded no data movement, and Metal **elides an empty blit encoder**, so the sample was never
  taken (M4 Pro: CPU peek reads 0 in 20 runs of 20; with a `fillBuffer` it reads a real timestamp 20
  of 20). M1 Max does not elide it, which is why it went unnoticed there — and why `0010` was
  "validated" on hardware that has neither defect. `0013` keeps the fill and gives it a second job.
- **`0013-kk-resolve-timestamp-queries-on-the-CPU-and-order-th.patch`** — replaces the GPU
  `resolveCounters:` with a CPU `resolveCounterRange:` at command-buffer completion (50/50 runs
  correct on both M1 Max and M4 Pro, vs 94%/18% failure for the GPU resolve), and supplies the
  ordering that moving the write out of GPU command order costs:
  - the sampling encoder's fill is now `0xff`, so the report reads **unavailable** in GPU command
    order from the moment the clock is sampled until the CPU value lands;
  - every command buffer carrying samples takes a sequence number, retired by its completion
    handler, and a shared event tracks the contiguous retired prefix. `vkCmdResetQueryPool` and
    `vkCmdCopyQueryPoolResults` wait on it with `encodeWaitForEvent:` (retiring the sampling command
    buffer first — waiting for it from inside itself would deadlock); `vkResetQueryPool`,
    `vkDestroyQueryPool` and `vkGetQueryPoolResults`+`WAIT_BIT` wait on the CPU. **venus goes
    through the copy path for every guest `vkGetQueryPoolResults`** (its query-feedback command
    buffer is appended to the same submission), so this is the steady state, not a corner;
  - a bare (no-`WAIT_BIT`) poll publishes any already-materialised in-flight sample instead of
    blocking, which it needs because the completion handler runs *after* the event `vkQueueWaitIdle`
    waits on — 0 of 10 polls succeeded without this, 39 of 40 with it, the remainder an honest
    `VK_NOT_READY` rather than a zero presented as available.

  One path on every GPU, so the machine we develop on exercises what we ship. Upstreamable.
  **Confirmed on the affected hardware**: 100/100 real timestamps for all three probe cases on
  dogfood-mac's M4 Pro (`spikes/kk-timestamp-probe/run-remote-m4.sh`), where every case read
  `0 / avail=1` before. The ~82%-per-timestamp expectation `0012` alone left behind is gone with
  the GPU resolve that caused it.

- **`0014-mesa-expose-GL_AMD_pinned_memory-on-ES2-contexts.patch`** / **`0015-kk-default-the-cmd-pool-BO-cache-to-512-heap-churn-w.patch`** —
  the venus-cmdstream perf arc (2026-07-28): 0014 unlocks the pinned-memory upload path for the
  host-zink reference, 0015 defaults KK's cmd-pool BO cache to 512 entries (heap churn was the top
  draw-time cost; opt-out `LIMINA_KK_BOCACHE=0`). See `docs/perf/venus-cmdstream-overhead.md`.
- **`0016-mesa-st-re-dirty-FS-sampler-views-after-PBO-upload-d.patch`** — fixes an **upstream
  mesa/st regression** (62efee186073, 2026-03-25): the PBO upload/download meta-ops unbind every
  fragment sampler view at the pipe level but the refactored invalidate mask forgot
  `ST_INVALIDATE_FS_SAMPLER_VIEWS`, so the next draw samples null descriptors (zeros) unless
  something else re-dirties texture state. Found by crossmark's upload shape on host zink-KK
  (guest mesa predates the regression); full forensics in `spikes/crossmark/RESULTS.md`.
  **Upstream candidate** with a clean `Fixes:` tag.

## Apply / rebuild
The `/Volumes/mesa-cs/mesa` tree is on the `limina/kosmickrisp` branch with these committed.
To re-create from a fresh checkout: `git checkout 178a3d73968 && git am
patches/kosmickrisp/0*.patch` (the full series), then build per `docs/drivers/kosmickrisp.rst`.

## TODO
- Split `0001` into logical patches (xfb lowering, encoder/render-pass, external memory,
  zink driconf) for review + upstreaming.
- The `kk_render_encoder` assert (`kk_encoder.c:299`) fires under mutter-50 (F44): a draw
  reaches `kk_render_encoder` with `last_used != KK_ENC_RENDER` and
  `need_to_start_render_pass` false — a render-pass-restart tracking gap. (mutter-49.5 /
  dev-enh never hits it.) Fix tracked separately.
- **`TEXTURE_FORMAT_16BIT_NORM` — was a MISDIAGNOSIS (corrected 2026-06-30); NOT a KK/venus gap.**
  The feature is fully advertised + available on venus (proven by `spikes/venus-draw-probe/fmtprobe.c`
  + `wgpu-fmtprobe/`: all six 16-bit-norm formats carry SAMPLED|STORAGE|TRANSFER_SRC|TRANSFER_DST,
  and wgpu `adapter.features()` reports `TEXTURE_FORMAT_16BIT_NORM = true`). The ghost-ui failure is
  that venus's **wayland surface-format list** offers `Rgba16Unorm` ahead of the 8-bit formats
  (lavapipe doesn't offer a 16-bit-unorm swapchain at all), so a conventional wgpu app ("first
  non-sRGB") picks `Rgba16Unorm` — which wgpu can NEVER use as a swapchain (it forbids `Rgba16Unorm`
  as a render attachment *even with the feature enabled*). Fix lives in **guest mesa**
  (`wsi_common_wayland` format intake — drop 16-bit-unorm from the wayland surface list, like
  lavapipe), NOT in KK. See the `limina-kk-feature-gaps` note for the full forensics + probes.
