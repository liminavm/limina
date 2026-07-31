# scanout-modifiers spike — non-LINEAR primary-plane scanout (M15 wave 4)

**2026-07-30, M1 Max, macOS 26.5.** Prompted by the compositor side's §31 ask 4 in
`dogfood-guest:Projects/gnome-shell-rs/docs/fork/present-misses.md`: the virtio-gpu primary
plane advertises LINEAR only, which is *why* they render to a private shadow and pay a
full-damage 4K present blit (+ RGBA→BGRA reorder) inside every frame's GPU bracket. Ask:
is non-LINEAR scanout feasible on a Metal host?

## Verdict up front

**Non-LINEAR scanout modifiers are not worth building — the same win is available without
them.** On Apple GPUs (TBDR), the scanout buffer's memory layout is irrelevant to render
cost at compositor workloads: the pass accumulates in tile memory and writes out once, and
a linear writeout is no slower than a twiddled one (measurably *faster* for a plain
fullscreen blit). The correct answer to the guest's ask is therefore not "tiled modifiers"
but: **render the scene directly into the LINEAR scanout dmabuf and delete the
shadow+blit.** The stack already supports the hard part today — their present blit already
*draws into* the imported scanout image; what they skip is using it as the scene's render
target.

## Mechanism map (verified in source)

- **The LINEAR-only advertisement is our own guest-kernel patch.**
  `patches/linux/0003-drm-virtio-advertise-linear-modifier.patch` adds
  `{DRM_FORMAT_MOD_LINEAR}` to `virtgpu_plane.c` (upstream advertises no modifiers at
  all); `0002` added ARGB8888 to `virtio_gpu_formats[]`. The plane's format/modifier menu
  is fully ours to extend — enhanced-tier kernel, one-line entries.
- **The scanout image on KK is a buffer-backed LINEAR MTLTexture over the IOSurface
  bytes.** `patches/virglrenderer/0011`: vkr strips the guest's external/modifier structs,
  forces `VK_IMAGE_TILING_LINEAR`, allocates a global IOSurface pitch-matched to the
  driver's rowPitch, and host-pointer-imports (`VK_EXT_external_memory_host`) the
  IOSurface base address; KK builds the texture from that memory's MTLBuffer, so rendering
  lands in the IOSurface, presented zero-copy by SET_SCANOUT_BLOB.
- **KK already has the modifier-tiling plumbing** (`kk_image_layout.c:215-229`, a limina
  rule: DRM-modifier images with attachment usage are laid out tiled because a guest
  modifier is host-meaningless; plain LINEAR stays linear for the vkr scanout path).
  KK does **not** advertise `VK_EXT_image_drm_format_modifier` itself; vkr's IFP2
  dispatch synthesizes the external-image answers the guest's probing needs, and the guest
  side additionally carries `patches/mesa/0010-venus-image-physdev-native-modifier.diff`.
- **KK's format table does not grant `COLOR_ATTACHMENT`/`BLIT_DST` to LINEAR tiling**
  (`kk_image.c` `kk_get_image_plane_format_features`: color features gated on
  `tiling != VK_IMAGE_TILING_LINEAR`) — yet the scanout image is used as a color
  attachment through the vkr-forced-LINEAR path (virgl 0011's own changelog tracks a
  draws-into-linear-attachment bug found and fixed during enablement). i.e. render-to-
  linear *works* on KK; it is just not honestly advertised as a format feature.

## The measurement (`rtprobe.swift`)

GPU time per pass (`cb.gpuEndTime - gpuStartTime`), 3840×2160 BGRA8 targets, 300 frames
after 20 warmup, M1 Max. `blit` = 1 fullscreen textured draw (their present blit);
`scene` = 60 large blended draws sampling a 4K source (a compositor frame).

| target backing | blit p50/p90 (ms) | scene p50/p90 (ms) |
|---|---|---|
| private tiled (their shadow baseline) | 0.261 / 0.267 | 2.900 / 2.908 |
| shared tiled | 0.261 / 0.269 | 2.901 / 2.909 |
| **buffer-backed linear (today's vkr scanout)** | **0.168 / 0.175** | **2.905 / 2.912** |
| IOSurface-backed (CAMetalLayer-drawable-style) | 0.170 / 0.176 | 2.904 / 2.913 |

- **Scene: all four identical to within 0.2%.** Rendering a full compositor frame into
  the linear scanout buffer costs the same as into a private tiled shadow.
- **Blit: linear targets are ~35% *cheaper* than tiled** (writing the twiddled/compressed
  layout costs more than a raw linear writeout for a pure overwrite pass).
- Buffer-backed vs IOSurface-backed is a wash — no perf reason to add an IOSurface-import
  path to KK; the 0011 host-pointer backing is fine.

## Answers to the spike questions

- **(a) Can the guest render directly into the scanout buffer?** Yes — it already draws
  into it (the present blit is a draw into the imported dmabuf image), and doing the whole
  scene there costs zero extra GPU. The deletable waste is the shadow pass's full-frame
  writeout + the blit (~0.17-0.26 ms raw host GPU at 4K, more through the venus bracket,
  every damaged frame) plus the shader swizzle.
- **(b) Tiled/non-LINEAR modifiers?** Buy nothing on this hardware. **Recommend: answer
  the guest's ask 4 with "no — and you don't want it; render direct-to-LINEAR instead,
  measured free."** Their machinery (smithay direct scanout + buffer-age damage into
  multiple LINEAR GBM buffers) is the guest-side change.
- **(c) Where does LINEAR-only come from?** Us (`patches/linux/0003`). Nothing host-side
  restricts the *format* list.
- **(d) Cheap partial win regardless:** if any blit remains, add `XBGR8888`/`ABGR8888` to
  `virtio_gpu_formats[]` (one line each, same shape as patch 0002) + the host presenter's
  format map, killing the RGBA→BGRA shader reorder. If they go direct-render, the ROP
  swizzles for free and this matters less.

## Addendum: the XBGR/ABGR gate is OPEN — WindowServer displays 'RGBA' IOSurfaces correctly

The one unknown blocking (d) was whether WindowServer correctly interprets an
`'RGBA'`-fourcc IOSurface set directly as `CALayer.contents` (limina's presenter is
layer-hosting — WindowServer, not our code, reads the pixels). Probed 2026-07-30
(`rgbaprobe.swift`): a window with the same test pattern (green strip / red left / blue
right) in a `'BGRA'` reference panel and an `'RGBA'` probe panel, both as opaque layer
contents. **Human oracle: identical.** No channel swap, no garbage — WindowServer handles
RGBA-order natively on macOS 26.5 / M1 Max.

So advertising `XBGR8888`/`ABGR8888` on the primary plane is a go, end to end:
guest-kernel format-list one-liners (patch-0002 shape) + whatever the host presenter's
format map needs; vkr already emits `'RGBA'` IOSurfaces for `VK_FORMAT_R8G8B8A8_*`
scanout images (`vkr_image.c` `gkvm_vkformat_to_iosurface`). Besides killing the guest
compositor's swizzle wherever a blit survives, this enables fullscreen direct scanout of
Vulkan clients' `R8G8B8A8` swapchain buffers, which can never hit the plane today.
Remaining small consumers if/when it ships: the software-2D fallback path and the
BGRA-assuming capture oracles (`vkr_metal_helpers.m:453`, iosdump). Re-confirm the
WindowServer behavior on the shipping macOS when it lands (OS-specific behavior rule).

## Caveats / follow-ups

- Numbers are raw host Metal; through venus→KK the *relative* result is what transfers.
  Cheap to re-confirm on the M4 Pro rig (`run-remote-m4.sh` family) — not expected to
  differ in direction on any Apple GPU (TBDR writeout-once).
- If the compositor's direct-render path wants a **depth/stencil attachment** alongside
  the linear color target, re-verify the 0011-era "linear color + depth attachment renders
  no fragments" KK bug is dead on today's KK (compositors normally don't use depth; their
  hand-rolled Vulkan compositor should confirm).
- KK's format table still doesn't advertise renderable-LINEAR; the guest believes it via
  vkr's synthesis. Works, but the honesty gap is worth a note in the KK fork if we ever
  upstream — a native Vulkan app on bare KK sees different features than a venus guest.
- The probe's blit result (linear cheaper than tiled) also means the *current* stack's
  present blit is not paying a linear-target penalty — the guest's ms/Mpx inflation from
  the blit is the extra full-frame pass itself, not the layout.
