# patches/linux — enhanced-tier guest kernel patches (drm/virtio)

Three small drm/virtio patches applied to the **enhanced-tier guest kernel** (the 16 KiB
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
- **`0002-drm-virtio-allow-argb8888-on-primary-plane.patch`** — accept ARGB8888 on the
  primary plane so compositors can direct-scanout alpha client buffers (mutter's AR24
  scanout test). Assumes the host treats the topmost scanout as opaque (limina's presenter
  does; QEMU/crosvm behavior asserted, not tested — resolve before upstreaming).
- **`0003-drm-virtio-advertise-linear-modifier.patch`** — advertise `DRM_FORMAT_MOD_LINEAR`
  via the plane modifier list (and drop `fb_modifiers_not_supported`) so LINEAR-tagged
  `ADDFB2` works; the device's only layout *is* linear.

## Upstreaming

All three are dri-devel material (0001/0003 near-ready, 0002 needs the opaque-scanout
question answered, possibly as virtio-gpu spec text) — see the triage in
`docs/reviews/2026-07-01-full-review.md` Part II. **Before sending, verify none has landed
upstream already** (`scripts/provision/f44/README.md:71` flags this).
