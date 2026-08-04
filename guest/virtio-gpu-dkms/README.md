# guest/virtio-gpu-dkms — the stock-4k venus enabler

An out-of-tree build of the in-tree virtio-gpu DRM driver, carrying one patch, for
**stock 4 KiB-page guests**. `scripts/build-virtio-gpu-dkms.sh` vendors the driver
sources from a stable kernel tag matching the target guest's series, applies
`0001-align-host-visible-allocations-to-16-KiB.patch`, and packages the result so
`dkms install` shadows the in-tree module (depmod prefers `extra/`). Proven end to
end on stock F44 in `spikes/venus-4k-dkms/RESULTS.md` — the first time venus worked
on the stock tier.

## Why the patch lives here and not in a kernel patch series

It used to be `patches/linux/0004`, applied to the **enhanced** 16 KiB kernel as well.
That series retired on 2026-08-04 when the enhanced kernel moved to the fork model
(`liminavm/linux`, branch `limina` — see `third_party/manifest.toml`), and the patch
did not move with it, for two reasons:

- **It is a no-op on the enhanced kernel.** `PAGE_ALIGN` is already 16 KiB there, so
  the enhanced-series copy never did anything; only 4 KiB guests need it.
- **It is known-rejected upstream, so it does not belong on a branch whose purpose is
  upstreamable delta.** The identical hardcoded-alignment shape was posted for Asahi
  (Finkelstein, Jan 2025) and declined: per-architecture alignment does not belong in
  a DRM driver, it belongs in the protocol. That protocol now exists and is merged —
  `VIRTIO_GPU_F_BLOB_ALIGNMENT` (Sergio Lopez, in 7.2-rc, backed by a ratified
  virtio-spec change): the device advertises `blob_alignment`, guest userspace rounds
  blob sizes, and `verify_blob` rejects misaligned ones with `-EINVAL`.

## The exit (not yet reachable)

Retiring this module needs the whole negotiated chain to be real *on the stock tier*,
and two of its three links are not ours to ship:

1. libkrun's virtio-gpu device advertises `blob_alignment = 16384` — **ours**, worth
   doing regardless (it also covers a future 4 KiB/FEX enhanced kernel).
2. The guest kernel is ≥ 7.2 — stock Fedora's, not ours.
3. The guest Mesa queries `VIRTGPU_PARAM_BLOB_ALIGNMENT` and rounds — stock Fedora's
   Mesa on this tier, not ours.

**Order matters:** advertising the feature before link 3 is in place turns working
odd-size allocations into clean `-EINVAL` failures. So this module stays until stock
Fedora catches up on both counts.

Background: `docs/upstreaming/ledger/linux.md`, memory `limina-blob-map-16k-alignment`.
The functional host-side pair is `patches/libkrun/0043` (hv map/unmap size rounding);
this patch is the guest-side window-offset half.
