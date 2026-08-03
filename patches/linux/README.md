# patches/linux — enhanced-tier guest kernel patches (drm/virtio + virtio-balloon)

Six small virtio patches applied to the **enhanced-tier guest kernel** (the 16 KiB
kernel built from kernel.org stable + the target Fedora's config — see
`scripts/provision/f44/build-kernel-rpm.sh`, which applies `*.patch` from this dir, and the
`limina-devmac-kernel-build` memory). There is no vendored kernel tree; the base is whatever
`KVER` the kernel build script pins, so these are carried as plain `git format-patch` files
authored against drm-misc-shaped sources and applied **tolerantly** (skipped if already
present — they may land upstream in newer kernels; check before bumping KVER, see
`scripts/provision/f44/README.md` §Patch-rebase risks).

The stock tier never sees these: a stock distro kernel must always boot (two-tier tenet);
these only sharpen the enhanced path.

## The patches

- **`0001-drm-virtio-fence-blob-scanout-flushes.patch`** — attach a fence to host3d-blob
  primary-plane `RESOURCE_FLUSH` (the same gate the dumb-buffer path already has), so a host
  that completes flush fences at true present time (limina's fence-accurate present chain,
  `patches/libkrun/0017-0018`) can pace compositor flips honestly. Argued
  behavior-unchanged on QEMU/crosvm (their fences complete at execution). **Pairs with
  libkrun 0018** — the host holds the fence this patch attaches.
  **Status on 7.1.x: SKIPS, and that is verified-safe (2026-07-30).** Upstream refactored
  prepare_fb to a per-plane-state fence allocated for `bo->dumb || drm_gem_is_imported(obj)`
  (v7.1.5 `virtgpu_plane.c:369-379`), and a compositor's scanout FBs arrive as imported
  dmabufs — so their flushes are fenced upstream without our host3d_blob condition. Proof it
  holds in practice: the deployed `7.1.4-limina16k` was built with 0001 equally skipped, and
  the fence-present measurements of 2026-07-27..30 (present-misses.md §12/§29/§31) show the
  chain pacing that guest. Keep the patch for pre-refactor KVERs; the residual theoretical
  gap (a host3d-blob primary FB that is *not* imported) has no known real instance.
- **`0002-drm-virtio-allow-argb8888-on-primary-plane.patch`** — accept ARGB8888 on the
  primary plane so compositors can direct-scanout alpha client buffers (mutter's AR24
  scanout test). Assumes the host treats the topmost scanout as opaque (limina's presenter
  does; QEMU/crosvm behavior asserted, not tested — resolve before upstreaming).
- **`0003-drm-virtio-advertise-linear-modifier.patch`** — advertise `DRM_FORMAT_MOD_LINEAR`
  via the plane modifier list (and drop `fb_modifiers_not_supported`) so LINEAR-tagged
  `ADDFB2` works; the device's only layout *is* linear.
- **`0004-drm-virtio-align-host-visible-allocations-to-16-KiB.patch`** — align host-visible
  blob allocations to 16 KiB so a 4 KiB-page guest maps cleanly on the 16 KiB-page host
  (part of the stock-4k venus enablement; see the `limina-blob-map-16k-alignment` memory).
- **`0005-virtio-balloon-stop-page-reporting-across-suspend.patch`** — stop free-page
  reporting across suspend so ballooning survives an s2idle cycle (M9; shipped in the
  `7.1.4-limina16k` respin, 2026-07-20).
- **`0006-drm-virtio-allow-xbgr8888-abgr8888-on-primary-plane.patch`** — accept the RGBA
  byte orders on the primary plane (+ their `translate_format` cases), so Vulkan clients'
  `R8G8B8A8` buffers and a Vulkan compositor rendering directly into its scanout buffer can
  hit the plane. Host side verified end-to-end first (WindowServer displays `'RGBA'`
  IOSurfaces natively — `spikes/scanout-modifiers/`; vkr + the software-2D swizzles already
  handle all eight orders). LE-only fourccs (no `HOST_` alias exists); applies on top of 0002.
  Upstream note: the durable fix is device-advertised plane formats (a virtio-gpu protocol
  gap — planned with the M15 overlay-plane extension) so drivers stop hardcoding these.

## Upstreaming

The drm/virtio plane/format patches are dri-devel material (0001/0003 near-ready, 0002 needs the opaque-scanout
question answered, possibly as virtio-gpu spec text) — see the triage in
`docs/reviews/2026-07-01-full-review.md` Part II. **Before sending, verify none has landed
upstream already** (`scripts/provision/f44/README.md:71` flags this).
