# patches/mesa — limina enhanced-tier GUEST Mesa patches (zink + venus)

The limina enhanced tier ships a **patched guest Mesa**: zink (GL→Vulkan) and venus (the
guest Vulkan driver) fixes so the GNOME/GL stack runs on **venus → KosmicKrisp → Metal**
instead of llvmpipe. This dir carries our patches over upstream Mesa; the source clone +
build happen in the Apple `container` Linux build env (host APFS is case-insensitive and
can't check out mesa). The host-side Mesa series (KosmicKrisp + host zink) lives separately
in `patches/kosmickrisp/`.

> **⚠ This is a patch POOL, not a single-base series** (unlike `patches/libkrun`/
> `patches/virglrenderer`): raw context diffs, three different effective bases, and three
> consumers applying different subsets. That shape is documented debt — flagged in the
> 2026-07-01 review (`docs/reviews/2026-07-01-full-review.md` Part III) — to be resolved
> when the patches go to Mesa upstream (each becomes an MR and the pool shrinks). Until
> then, THIS file is the map; keep it current when adding/retiring a diff.

## Bases × consumers (the map)

| Consumer | Base | Applies |
|---|---|---|
| `scripts/build-mesa-zink.sh` (guest GL/zink tree, `/opt/mesa-zink`) | Mesa main `3515c52e8cf3` (pinned as `MESA_COMMIT`, 2026-06-07) | 0001–0006 (the zink pool) |
| `scripts/build-venus.sh` (guest venus ICD rebuild) | tag `mesa-26.1.0` (parametric `MESA_TAG`) | 0009 + 0010 |
| `scripts/provision/f44/build-mesa-rpm.sh` (the SHIPPING F44 RPM) | Fedora F44 `mesa-26.1.3` SRPM | 0001 + 0009 + 0010 + 0011 |

gitlab.freedesktop.org is Anubis-bot-blocked → builds clone a GitHub mirror.

**Duplicate encodings of 0010 (known, deliberate for now):** `venus-dmabuf-patch.py` is the
anchored-string "version-robust" re-encoding of 0010's physdev edits, and
`spikes/venus-draw-probe/patch-venus-dmabuf-nomod.py` is the F44 no-modifier variant —
"keep both until the F44 scanout story is settled" (py header). Retire the py forms when
that settles or when 0010 lands upstream, whichever first.

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
- **`0006-zink-kopper-guard-missing-surface-extensions.diff`** — kopper guards for the
  surfaceless/no-WSI path on KK.
- **`0007`** — *removed 2026-06-24.* It was a backport already upstream in 26.x; on the
  newer tree the fuzzy `patch -F5` fallback silently re-applied it at +347 lines, **duplicating**
  `get_external_image_handle_type_props` → `zink_resource.c` "redefinition" build failure. It is
  obsolete (upstream) so it was dropped rather than rebased.
## Venus present fix (`0009` + `0010`) — built against `mesa-26.1.0` via `scripts/build-venus.sh`

These two **reproduce the working accelerated-tier venus** that the golden
`Fedora-Workstation-43.dev-enh.raw` ran. Root-caused 2026-06-24 (see `docs/dev-enh-recipe.md`):
the present fix was a **lost limina patch set that had only ever lived in dev-enh's in-guest
`~/mesa-venus` tree** — never exported, the same discipline gap as KK `0001`. A clean rebuild
at *any* mesa version (26.1.0 release, 26.2) failed identically at `gbm_surface_lock_front_buffer`
until these were recovered; it was **never a version regression**. They supersede the old
`0005` (present-region; now folded into `0009`) and `0008` (dma-buf; now folded into `0010`),
both retired. dev-enh's base was pinned by multi-file blob fingerprint to **`mesa-26.1.0`** (the
26.1 release branch; the "26.1.0-devel" VERSION string was a red herring — main had bumped to
26.2.0-devel at the April branch-point). The patched files are **byte-identical to `mesa-26.1.0`
except our edits**, so these diffs are **pure limina, no upstream drift**. Inert `if (0) fprintf`
debug traces and a dead `tiling_translated_to_optimal` flag were stripped (DIAG hygiene, below).

- **`0009-venus-wsi-present-fix.diff`** (`vn_wsi.c`, `src/vulkan/wsi/wsi_common.h`,
  `wsi_common_wayland.c`) — the WSI half:
  - `treat_invalid_modifier_as_linear`: macOS-host virtio-gpu advertises every DRM modifier as
    `INVALID`; rewriting those to `LINEAR` keeps mesa on the IOSurface single-memory path
    instead of the prime-blit fallback that breaks zero-copy.
  - `vn_wsi_create_image`: translate `DRM_FORMAT_MODIFIER` swapchain images → `OPTIMAL` and
    strip the modifier pNext, because the in-process renderer returns
    `memoryRequirements.size = 0` for modifier images → trips `wsi_create_native_image_mem` →
    `gbm_surface_lock_front_buffer failed`.
  - the `VkPresentRegionKHR::pRectangles` deep-copy (old `0005`).
- **`0010-venus-image-physdev-native-modifier.diff`** (`vn_physical_device.c`, `vn_image.c`,
  `vn_image.h`) — the device half:
  - **native DRM-format-modifier reporting**: venus itself reports `DRM_FORMAT_MOD_LINEAR`
    (features from OPTIMAL tiling) so kopper's Wayland path can negotiate a modifier at all —
    without it swapchain creation fails. (This is the piece beyond `0008` that `0009` alone
    lacked.)
  - dma_buf-on-opaque-fd: the `else if` keying off `KHR_external_memory_fd` →
    `renderer_handle_type = OPAQUE_FD` + force-enable `EXT_image_drm_format_modifier` /
    `EXT_queue_family_foreign` (old `0008`). Without it stock venus reports `caps.dmabuf=0` →
    gbm dumb-buffer fallback → NULL `bo->image` → gnome-shell SIGSEGV.
  - `vn_image` modifier plane-count + tiling-translation handling.

`scripts/build-venus.sh` (parametric `MESA_TAG`, default `mesa-26.1.0`; `FEDORA_REL` must match
the guest) applies `0009`+`0010` and emits `libvulkan_virtio.so` + the ICD. dev-enh's exact
venus+WSI sources are preserved under `spikes/venus-261-source/`. The host KosmicKrisp +
host-zink series lives separately in `patches/kosmickrisp/`.

- **`0011-venus-wsi-drop-16bit-unorm-swapchain.diff`** (2026-06-30, ships in the F44 RPM as
  mesa 26.1.3-2.limina) — venus/Wayland WSI: drop 16-bit-unorm swapchain formats
  (`R16G16B16A16_UNORM` etc.), matching lavapipe. venus offered a wgpu-unrenderable
  `Rgba16Unorm` Wayland swapchain format, producing the "ghost UI" in wgpu apps. NOT a KK
  gap (see the corrected story in the `limina-kk-feature-gaps` memory). Upstreamable with
  the lavapipe precedent as the argument.
- **`0012-venus-degrade-to-stub-instance-when-ring-setup-fails.diff`** (2026-07-01, the
  two-tier Vulkan-floor fix) — venus: when the renderer connects but the instance ring /
  version handshake fails (on limina: a 4 KiB-page guest under the 16 KiB host can't map
  the ring's 132 KiB shmem blob), degrade to the existing STUB instance (0 devices) instead
  of returning `VK_ERROR_OUT_OF_HOST_MEMORY` — which the loader treats as fatal for the
  WHOLE `vkCreateInstance`, killing lavapipe with it. Root-caused + validated RED→GREEN on
  a stock F44 guest 2026-07-01 (venus+lavapipe → llvmpipe enumerates, no error). Protects
  the enhanced image's *stock-kernel GRUB-fallback boot* today; truly-stock guests get it
  when it lands upstream (strong candidate — the stub path already exists for version
  mismatches). Applied by `build-venus.sh` and the F44 RPM build.

## Re-export / DIAG hygiene
The in-guest working tree carried temporary `LIMINA-DIAG` debug hacks (force
`driver_name_is_inferred=false`, `mesa_loge(__LINE__)` markers) — those are **debug only and
must never be committed here**. Only clean, upstreamable patches belong in this series.
