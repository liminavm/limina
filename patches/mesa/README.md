# patches/mesa — limina enhanced-tier Mesa patches (zink)

The limina enhanced tier ships a **patched zink** (GL→Vulkan) so the GNOME/GL stack runs on
**venus → MoltenVK → Metal → Apple GPU** instead of llvmpipe. This dir carries our patch
series over upstream Mesa; the source clone + build happen in the Apple `container` Linux
build env (host APFS is case-insensitive and can't check out mesa) via
`scripts/build-mesa-zink.sh`, which installs the `/opt/mesa-zink` tree we deliver to the guest.

This replaces the old ad-hoc in-guest `~/mesa` build (task #26).

## Base
- **Upstream Mesa main, commit `3515c52e8cf31549b6068ef43c23c89830b6db46`** (pinned in
  `scripts/build-mesa-zink.sh` as `MESA_COMMIT`; 2026-06-07). gitlab.freedesktop.org is
  Anubis-bot-blocked → build clones a GitHub mirror.

## Patches (apply in filename order)
- **`0001-zink-nullDescriptor-emulation-MR37115.diff`** — Mesa **MR !37115**: zink
  nullDescriptor *emulation* (dummy descriptors) so zink runs on Vulkan implementations
  that lack `robustness2.nullDescriptor` — which MoltenVK does. Without it zink bails
  (`Zink requires the nullDescriptor feature of KHR/EXT robustness2`) → llvmpipe. Clean (no
  LIMINA-DIAG debug hacks). Not in any Mesa release/main — we must carry it until it lands.
- **`0002-fbobject-guard-null-pipe-resource-in-discard.diff`** — `do_discard_framebuffer()`
  dereferences `att->Renderbuffer->texture` (a `pipe_resource`) without a NULL check. A
  gbm/kopper winsys back buffer can be `Complete` yet have no `pipe_resource` while the
  swapchain rotates it, so `glInvalidateFramebuffer(GL_BACK)` crashed gnome-shell at swap
  on the mutter native/KMS path. Guard `prsc` for NULL. Upstreamable (#30 seated, wall 1).
- **`0003-zink-guard-missing-external-semaphore-fd-on-dmabuf-import.diff`** — and
  **`0004-...-on-dmabuf-export.diff`** — `zink_screen_import/export_dmabuf_semaphore()`
  call `VKSCR(GetSemaphoreFdKHR)` / `VKSCR(ImportSemaphoreFdKHR)` whenever built with
  libdrm on Linux, without checking `VK_KHR_external_semaphore_fd` is supported. venus over
  MoltenVK has no fd-based external semaphores → those entrypoints are NULL → swap-buffers
  jumps through a NULL function pointer. Gate both on
  `screen->info.have_KHR_external_semaphore_fd` (the flag zink already uses for dmabuf /
  cl_gl_sharing elsewhere); fall back to implicit dmabuf sync. Upstreamable (#30 seated,
  walls 2–3). Together 0002+0003+0004 make seated gnome-shell *survive* on venus (no more
  crash-loop); the remaining blocker is KMS-CRTC scanout-format negotiation, not a crash.
- **`0005-venus-deep-copy-present-region-rectangles.diff`** — venus's async present thread
  (`vn_wsi_clone_present_info`) deep-copies the `VkPresentRegionKHR` array but NOT the
  per-region `pRectangles` (`VkRectLayerKHR[]`); zink's kopper frees its rectangle storage
  as soon as `vkQueuePresentKHR` returns, so the present thread read freed memory —
  `assert(rect->layer == 0)` in wsi_common_wayland on debug builds (killed Firefox at
  first damage-rect present), garbage wl damage on release. Deep-copy the rectangles too.
  Upstream MR candidate. (Found 2026-06-10 chasing WebGL2-aquarium-on-KK; also baked
  directly into `Fedora-Workstation-43.dev-enh.raw`'s `~/mesa-venus` + installed lib.)
- **`0006-zink-kopper-guard-missing-surface-extensions.diff`** — kopper guards for the
  surfaceless/no-WSI path on KK.
- **`0007`** — *removed 2026-06-24.* It was a backport already upstream in 26.x; on the
  newer tree the fuzzy `patch -F5` fallback silently re-applied it at +347 lines, **duplicating**
  `get_external_image_handle_type_props` → `zink_resource.c` "redefinition" build failure. It is
  obsolete (upstream) so it was dropped rather than rebased.
- **`0008-venus-expose-dmabuf-on-opaque-fd-renderer.diff`** — **the load-bearing venus
  patch** (recovered 2026-06-24 from June-11 session `1af547e7`; it had only ever lived in
  dev-enh's in-guest `~/mesa-venus` checkout — never exported, the enhanced-tier discipline
  gap). venus only advertises `VK_EXT_external_memory_dma_buf` / `_image_drm_format_modifier`
  when the **renderer** (host) advertises dma-buf. KK on Metal has no real dma-buf — it
  advertises `KHR_external_memory_fd` (opaque fd). In a VM the guest's "dma-buf" fds are
  virtio-gpu GEM/blob resources anyway, so this patch adds an `else if` in
  `vn_physical_device_init_external_memory` keying off `KHR_external_memory_fd` →
  `renderer_handle_type = OPAQUE_FD` (host handle translated in
  `vn_device_memory_fix_alloc_info`), and force-enables `EXT_image_drm_format_modifier` +
  `EXT_queue_family_foreign` (guest-handled in a VM). **Without it**, stock distro venus
  reports `caps.dmabuf=0` → gbm `create_dumb` fallback → NULL `bo->image` →
  `dri2_allocate_textures` NULL-deref → mutter/gnome-shell SIGSEGV on F44. Requires building
  venus (below). Upstreamable.

## Venus (now built here)
`scripts/build-mesa-zink.sh` now builds venus too (`-Dvulkan-drivers=virtio`, default),
producing `lib64/libvulkan_virtio.so` + the venus ICD in the `/opt/mesa-zink` prefix, with
patch **0008** applied. This is required for the accelerated desktop on a stock distro guest
whose own venus lacks dma-buf (see 0008). Override `VULKAN_DRIVERS=` (empty) to build zink
only. Note the host KosmicKrisp + host-zink patch series lives separately in
`patches/kosmickrisp/` (the `/Volumes/mesa-cs/mesa` tree).

## Re-export / DIAG hygiene
The in-guest working tree carried temporary `LIMINA-DIAG` debug hacks (force
`driver_name_is_inferred=false`, `mesa_loge(__LINE__)` markers) — those are **debug only and
must never be committed here**. Only clean, upstreamable patches belong in this series.
