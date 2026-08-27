# Does a guest trim return host disk? Yes — and Fedora already does most of it

Measured 2026-08-26 on a `cp -c` clone of `Fedora-Workstation-44.enhanced.raw` (btrfs root,
kernel `7.1.8-limina16k.4`), EFI+venus boot, host allocation read with `du -m` on the backing
`.raw`:

| step | host allocated |
|---|---|
| baseline | 15347 MiB |
| guest wrote a 4 GiB random file, `sync` | 18641 MiB |
| guest `rm` + `sync`, 30 s later | **14604 MiB** |
| guest `fstrim /` | **13646 MiB** |

**The discard path works end to end**: guest `rm` → virtio-blk `VIRTIO_BLK_T_DISCARD` →
imago's punch-hole → the host file shrinks. This is the first end-to-end confirmation of the
imago tail-discard fork delta from outside `spikes/m10-disk-durability/`.

Two things temper how much limina should invest in driving it:

- **Fedora mounts btrfs `discard=async` and enables `fstrim.timer`.** The `rm` alone returned
  the 4 GiB *and* ~740 MiB more, unprompted, within 30 s. Continuous reclaim of freshly-freed
  extents is already the guest's own behavior; limina adds nothing to it.
- **An explicit trim still recovers accumulated residue** — 958 MiB (~6% of the image) on an
  image that had been in use for weeks. Async discard only covers extents freed *while it was
  in effect*; anything freed before that stays allocated forever, because a raw image only ever
  grows. That residue is what an on-demand trim is for.

The value therefore scales with how *unlike* Fedora the guest is: an ext4 guest (no discard by
default), a distro without `fstrim.timer`, or a VM that is rarely up when a weekly timer fires.

## Oracle note for tests

`fstrim -v` reports the size of the ranges it *walked*, not the space recovered — it said
"25.7 GiB trimmed" while recovering 958 MiB. Any test must measure host-side allocated blocks
(`du`/`stat -f %b`), never the guest's own report.
