# venus-4k-dkms — venus on stock 4 KiB guests via an out-of-tree virtio-gpu module

**Date:** 2026-07-03. **Verdict: WORKS end-to-end.** A stock Fedora 44 guest (4 KiB
stock kernel, stock Mesa) gets a fully working venus once the `limina-virtio-gpu`
DKMS module is installed — the first time venus has ever worked on the stock tier.

## What was proven (live, on a stock F44 `accessible` clone)

1. **The DKMS module builds against real Fedora kernel-devel** (6.19.10-300.fc44) from
   the vendored in-tree sources + `patches/linux/0004` (16 KiB start alignment for
   host-visible window allocations). `scripts/build-virtio-gpu-dkms.sh` assembles the
   tree; `dkms add/install` puts it in `/lib/modules/$KVER/extra/`, where depmod
   precedence shadows the in-tree module.
2. **AUTOINSTALL survives kernel updates**: the guest unexpectedly rebooted into
   7.0.14-201.fc44 mid-validation and DKMS had rebuilt + installed for it — the
   6.19-vendored sources compiled clean against 7.0.x (small API surface).
3. **initramfs is the gotcha**: the kernel RPM generates its initramfs BEFORE dkms
   autoinstall runs, so early KMS loads the STOCK module until a `dracut -f` after
   `dkms install`. Delivery packaging must hook that per kernel (verified: taint
   stays 0 / no `extra/` in initramfs without it; with it, taint 12288 +
   "loading out-of-tree module taints kernel" = ours is loaded).
4. **venus works**: `vulkaninfo --summary` lists `Virtio-GPU Venus (Apple M1 Max)`
   next to llvmpipe, and `vkmap-stress.c` (this dir) PASSES 60 host-visible
   allocations — odd sizes cycling every 4 KiB residue mod 16 KiB, with hole reuse —
   mapped + write/read-verified on venus. Pre-module the same guest failed
   `vkCreateInstance` outright.

Full fix stack: libkrun 0043 (host hv map/unmap size rounding) + virglrenderer 0023
(zink map_info) + this module (guest window offset alignment). See memory
`limina-blob-map-16k-alignment`.

## The graceful-degradation finding (tests/venus_fallback.rs)

Without the module, Mesa venus (25.x and 26.0.x, stock) fails `vkCreateInstance`
with `VK_ERROR_OUT_OF_HOST_MEMORY` — and the Vulkan **loader treats OOM as fatal
for the whole instance chain**, masking a perfectly healthy lavapipe (it only
*skips* an ICD for `INCOMPATIBLE_DRIVER`, observed with dzn). So stock-tier Vulkan
is entirely dead by default, not "degraded to lavapipe". Isolation proof:
`VK_DRIVER_FILES=lvp_icd → works; virtio_icd alone → OOM`.

Upstream fix to pursue: Mesa `vn` should return a skippable VkResult when its
transport/ring setup fails. Until Fedora ships that, `tests/venus_fallback.rs`
asserts the truthful contract (lavapipe-explicit floor works, default path fails
structuredly, session survives) and auto-tightens once the default path starts
succeeding.

## Files

- `vkmap-stress.c` — C stress (needs gcc + vulkan-loader-devel in the guest);
  the money oracle for "venus mapping actually works".
- `vkprobe.py` — dependency-free (python3 + libvulkan via ctypes) probe used by
  `tests/venus_fallback.rs` on pristine stock images. ctypes gotcha baked in:
  argtypes are load-bearing (ints truncate 64-bit handles otherwise).

## Follow-ups

- Wire the DKMS tarball into the guest-tools payload (delivery flow, user's call
  on placement) + docs/images.md component versions.
- Upstream: the Mesa vn skippable-error fix; the virtgpu alignment patch to
  dri-devel (64 KiB or negotiated granule for the general case).
- 4 KiB enhanced kernel variant (FEX story) can carry 0004 in-tree; the 16 KiB
  kernel needs nothing (PAGE_ALIGN already 16 KiB).
