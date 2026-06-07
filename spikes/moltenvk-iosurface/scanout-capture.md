# Phase 1b capture: what mutter/venus issues for a scanout image (#30 CRUX)

Date: 2026-06-07. Method: temporary `LIMINA-SCANOUT` fprintf instrumentation in the
virglrenderer fork (`vkr_image.c` vkCreateImage/BindImageMemory, `vkr_device_memory.c`
vkAllocateMemory/export_blob — uncommitted working-tree diagnostic), rebuilt the dylib
(`scripts/build-virglrenderer.sh`), booted the base Fedora image on venus, and forced the
GNOME session onto venus (autologin in `/etc/gdm/custom.conf` + zink env in the user's
`~/.config/environment.d/zink.conf`, then `systemctl restart gdm`). 940 LIMINA-SCANOUT host
lines captured.

## Result — exactly ONE image type fails (the scanout framebuffer)

```
LIMINA-SCANOUT vkCreateImage fmt=44 1280x800x1 mips=1 layers=1 usage=0x400097 tiling=0 flags=0x8
LIMINA-SCANOUT    pNext EXTERNAL_MEMORY_IMAGE handleTypes=0x1
LIMINA-SCANOUT    pNext sType=1000147000
LIMINA-SCANOUT vkCreateImage -> ret=-8         (72×, gnome-shell crash-loop)
```
Decoded:
- **fmt=44 = `VK_FORMAT_B8G8R8A8_UNORM`** — BGRA8, *exactly* our IOSurface format.
- **1280×800** = the display size → this is the compositor's scanout/framebuffer image.
- usage=0x400097 = TRANSFER_SRC|TRANSFER_DST|SAMPLED|COLOR_ATTACHMENT|INPUT_ATTACHMENT|0x400000.
- tiling=0 = OPTIMAL; flags=0x8 = `VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT`.
- pNext #1 = **`VkExternalMemoryImageCreateInfo`, handleTypes=0x1 = `OPAQUE_FD`** (NOT dma_buf;
  venus uses opaque-fd for renderer-internal sharing). This is what MoltenVK can't honor.
- pNext #2 = sType 1000147000 = `VkImageFormatListCreateInfo` (the mutable-format view list).
- **ret = -8 = `VK_ERROR_FEATURE_NOT_PRESENT`** — host MoltenVK vkCreateImage rejects the
  external-memory image (we strip external_memory_fd at vkCreateDevice; MoltenVK lacks it).
  Matches the guest journal: `vn_CreateImage FAILED result=-8` → `ZINK: vkCreateImage failed
  (VK_ERROR_FEATURE_NOT_PRESENT)` → gnome-shell core-dump → fallback.

## What this settles

1. **The failure is HOST-side** (reaches vkr → MoltenVK), so fix A belongs in vkr. Confirmed.
2. **Fix A trigger is precise and narrow:** on `__APPLE__`, a `vkCreateImage` whose pNext has a
   `VkExternalMemoryImageCreateInfo` with `handleTypes != 0`. Only ~1 image type matches (the
   BGRA8 display-sized scanout); all 325 non-external (format-list-only) images already succeed.
3. **Action for A:** create an IOSurface (`vkr_mtl_iosurface_alloc`, BGRA8 for fmt=44),
   *remove* the `VkExternalMemoryImageCreateInfo` from the chain, inject
   `VkImportMetalTextureInfoEXT(mtlTexture)`, keep the `VkImageFormatListCreateInfo`. Handle
   `MUTABLE_FORMAT` — the IOSurface MTLTexture must allow the listed view formats (BGRA8 UNORM
   + sRGB); set MTLTextureUsagePixelFormatView / map the format list. Track image→IOSurface(id)
   in `struct vkr_image` for the resolve API (C).
4. **The scanout image does NOT use export_blob.** All 216 export_blob events are host-visible
   memory blobs (`blob_flags=0x1 USE_MAPPABLE, valid_fd_types=0x4 FD_SHM, mtl_shm=1`) — the
   #28 path, unrelated. The scanout image uses a **dedicated allocation** (vkAllocateMemory
   `dedImage=<handle>`, `exportHandleTypes=0`). So resolve = image→IOSurface directly, not via
   the blob channel.
5. **Part (b) of the CRUX (blob→scanout linkage at SET_SCANOUT_BLOB) is still unseen** because
   mutter never gets past image-create today. It will be observable once A lands and the image
   succeeds — capture it then (Phase 2).

## Secondary observations (note, not blockers)
- `gnome-shell: KMS CRTC doesn't support GBM format` + `Failed to query buffer age, got error
  3003` — mutter's virtio-gpu KMS/GBM plane-format path has its own gaps; revisit for full
  present, separate from the image-create fix.
- gnome-shell **core-dumped** on the venus failure (didn't fall back cleanly) — once A makes
  the create succeed, this should stop.

## Reproduce
Boot base on Image-16k + `--net` + a display; in the guest set autologin + the zink
`environment.d` (see `/tmp/scanout-*.sh` from the session) and `systemctl restart gdm`;
read the worker stderr for `LIMINA-SCANOUT` lines. (Instrumentation is an uncommitted
working-tree diagnostic in the virglrenderer fork.)
