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

## One trim already returns everything — so `l2_qga_fstrim`'s ~50 % is not the discard path

Measured 2026-09-23 with `second-trim-probe.sh` on a `cp -c` clone of
`Fedora-Workstation-44.enhanced.test.raw`, the same image the test uses, booted headless
(EFI + `--net`, no seated session). Root remounted `nodiscard` and `fstrim.timer` stopped, so
nothing comes back unprompted; 2048 MiB of `/dev/urandom` written, `sync`ed and deleted; host
allocation read as `st_blocks` after each trim.

| cycle | `fstrim -m` | host allocated | freed | cumulative | walked |
|---|---|---|---|---|---|
| — | (floor) | 13948 MiB | | | |
| — | after write | 15998 MiB | | | |
| — | after delete | 16000 MiB | | | held, as intended |
| 1 | 1 MiB | 13953 MiB | **2047 MiB** | 2047 MiB | 25.3 GiB |
| 2 | 1 MiB | 13955 MiB | −2 MiB | 2045 MiB | 5.7 GiB |
| 3 | 0 | 13934 MiB | 21 MiB | 2066 MiB | 6 GiB |
| 4 | 0 | 13936 MiB | −2 MiB | 2064 MiB | 4 GiB |

**One trim returns the whole payload, at the minimum the supervisor actually uses.** The
±3 MiB between later cycles is noise, and the run ends *below* the floor it started at.

Two candidate explanations for the test's shortfall are dead:

- **Not a missing second cycle.** Repeat trims free nothing, because nothing is left.
- **Not `qga::trim::MIN_EXTENT`.** Cycle 1 ran at exactly that 1 MiB minimum and recovered
  2047 MiB; dropping to no minimum added 21 MiB, about 1 % of the payload.

So the discard path — FITRIM → virtio-blk `VIRTIO_BLK_T_DISCARD` → imago's punch-hole — returns
everything asked of it, and the panic text in `l2_qga_fstrim.rs` ("the discard is not reaching
the backing file") is misleading: a gigabyte demonstrably does reach it.

### What is still unexplained

Four suite runs of `l2_qga_fstrim` recovered 1012–1074 MiB of the same 2048 MiB payload and
left ~1145 MiB above the floor, where this probe leaves ~5 MiB. The assertion asks for more
than half, so those runs land within a few MiB of the line and the test passes or fails on
noise — one of the four passed.

This probe differs from the test in two ways that were not isolated, and the next measurement
should take them one at a time:

- **The trim channel.** The probe runs `fstrim` over ssh; the test drives the supervisor's
  periodic `guest-fstrim` through the SELinux-confined `qemu-guest-agent`.
- **Guest activity.** The probe boots headless and idle; the test boots the seated desktop,
  whose own writes are visible in its larger fill delta (+2149 MiB for a 2048 MiB payload,
  against +2050 MiB here).

The residue is also suspiciously close to one btrfs data chunk (1 GiB), which is worth checking
against `btrfs filesystem usage` in a failing run before theorising further.
