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

## The shortfall is the guest spending the space again while we wait for the trim

Measured 2026-09-24 by instrumenting `l2_qga_fstrim` itself — same vehicle, same seated
enhanced image, same qga tick — and reading the **guest's** accounting beside the host's at
every stage. That run passed, by 51 MiB, which is the same knife-edge the failing ones sat on.

| stage | host file (`st_blocks`) | guest `Used` | btrfs `Device allocated` |
|---|---|---|---|
| floor | 13952 MiB | 12335 MiB | 16408 MiB |
| after the 2048 MiB write | 16097 MiB | | |
| after `rm` + 30 s | 16108 MiB | 12339 MiB | 15384 MiB |
| after the qga trim | **15033 MiB** | **13335 MiB** | 16408 MiB |

Two things fall out of the right-hand columns, and neither is the discard path.

**The guest wrote ~996 MiB between the delete and the trim.** `Used` goes 12339 → 13335 MiB
and `df` agrees (12369 → 13365). The journal accounts for 8 MiB of that; the rest is the
seated desktop's own background work, in the ~3 minutes the test spends waiting for a 200 s
trim cadence to come round. btrfs put it straight back into the block group it had just
reclaimed — `Device allocated` returns to exactly the 16408 MiB it started at, the same
1 GiB chunk.

So the host cannot give those blocks back: **they are in use again.** The trim returned
1075 MiB; had the desktop written nothing it would have returned ~2071 MiB, which is what
the headless probe above measures (2047 MiB). The arithmetic closes.

**The trim channel is exonerated outright.** Running `fstrim -m 1M /` over ssh immediately
after the supervisor's qga trim freed **exactly 0 further MiB** — the SELinux-confined
`qemu-guest-agent` had already taken everything that was takeable. Dropping to `-m 0` added
90 MiB, matching the ~1 % the headless probe saw. Both remaining suspects from the previous
section are therefore dead:

- *Not the channel.* qga `guest-fstrim` and ssh `fstrim` return the same blocks.
- *Not the discard path.* It returned every block that was still free when it ran.

### It is deterministic, not noise

A second instrumented boot of the same image reproduces it to the byte. Guest `Used`, in
bytes, across two independent runs:

| | floor | after `rm` | after the trim |
|---|---|---|---|
| run 1 | 12 934 225 920 | 12 938 178 560 | 13 985 071 104 |
| run 2 | 12 934 225 920 | 12 933 009 408 | 13 985 193 984 |

The two runs land 123 KB apart after a four-minute window — that is not a desktop doing
whatever it happens to feel like. `Device allocated` is identical in both runs at every
stage (16408 → 15384 → 16408 MiB). Something the same size runs every boot.

### Why this makes the test a coin flip

The assertion asks the host file to shrink by more than `PAYLOAD_MIB / 2` = 1024 MiB. The
payload really does free ~2050 MiB, but the desktop reliably spends ~1000 MiB of it back
before the trim fires, leaving ~1050 MiB to recover against a 1024 MiB bar. Every measured
run — 1007, 1012, 1074, 1075, 1083 MiB — lands within tens of MiB of the line, in both
directions. The test is measuring the desktop's appetite, not the discard path.

### What is spending it: an offline system update

A third instrumented run diffed a list of likely directories either side of the wait:

| | at the delete | after the trim |
|---|---|---|
| `/var/lib/dnf` | 4708 MiB | **5707 MiB** |
| `/var/cache` | 342 MiB | 343 MiB |
| `/var/log` | 325 MiB | 333 MiB |
| `/home` | 2663 MiB | 2663 MiB |
| `/var/lib/flatpak` | 21 MiB | 21 MiB |

`+999 MiB into /var/lib/dnf`, and the newest files on the box are
`/usr/lib/sysimage/libdnf5/offline/transaction.json` and its
`offline-transaction-state.toml` — the manifest dnf5 writes when it has finished staging an
**offline system update**. `dnf5daemon-server.service` is running and holds the largest RSS
on the machine; `gnome-software` is what asked it. A fresh F44 Workstation downloads its
pending updates within minutes of login, every boot, which is exactly the determinism above.

Two things hid it from the obvious search, and both are worth remembering:

- **`find -newermt` is blind to downloaded packages.** librepo stamps each RPM with the
  server's `Last-Modified`, so a freshly downloaded package can carry an mtime from weeks
  ago. Only `-newerct` (ctime, which userspace cannot set) sees them.
- **`find / -xdev` stops at btrfs subvolume boundaries.** Subvolumes get distinct
  `st_dev` values, so `-xdev` never reached `/home` — or, here, the rest of `/var`. A
  `du` given each path explicitly does not have that problem.

That is why the first search came back reporting 0 MiB written while a gigabyte was landing.

## The fix

The host can only hand back blocks that are **still free when the trim runs**. So:

- `limina_test::quiesce_desktop` now knows about F44: it kills `gnome-software`, then stops
  **and masks** `dnf5daemon-server.service` and `flatpak-system-helper.service` alongside the
  PackageKit units it already handled. Masking matters — these are socket-, timer- and
  D-Bus-activated, so a stop alone just invites the next request to restart them.
  `l2_synoik_restore_landmarks`, its other caller, was quieting a service F44 no longer ships.
- `l2_qga_fstrim` calls it before *any* measurement, and gained a control oracle: the guest's
  own `df` used-space is read at the delete and again after the trim, and a run where the
  guest re-used more than `PAYLOAD_MIB / 4` fails saying **that**, naming `du -xsm` as the way
  to find the new culprit, instead of blaming the discard path.
- The old panic text — "The discard is not reaching the backing file" — was wrong every time
  it fired. It now reports how much came back of how much was asked, states that the guest
  re-used almost nothing (because the control above just proved it), and points at
  `second-trim-probe.sh` for reproducing the path without the supervisor.

### Measured after the fix

Three consecutive runs, the last of them against exactly the committed code:

| | before | after |
|---|---|---|
| fill delta for a 2048 MiB payload | +2145, +2153 MiB | **+2049, +2050, +2052 MiB** |
| guest re-used during the wait | ~996-1001 MiB | **0 MiB, every run** |
| host blocks returned by the trim | 1007-1083 MiB | **2043, 2045, 2044 MiB** |
| margin over the 1024 MiB bar | −17 to +59 MiB | **+1019 to +1021 MiB** |
| residue above the floor | ~1081-1145 MiB | **6-8 MiB** |

The test now measures what the headless probe at the top of this file measures — ~2044 MiB
against 2047 MiB, and 6-8 MiB of residue against 5 MiB — which is the point: the seated
desktop was never supposed to be part of the oracle. It also finishes ~60 s sooner, because
the guest is no longer competing for the disk.

