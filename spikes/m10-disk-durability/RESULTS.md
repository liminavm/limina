# M10 disk durability spike — RESULTS

## Question
A first cut of the M10 two-disk test (`crates/limina-test/tests/disks.rs`) found that an **ext4
filesystem written to a data disk (`vdb`) is unmountable after a guest *reboot*** — `blkid` still
reports `ext4`, but `mount` fails with a bad superblock. Within a single boot an
unmount/remount round-trips fine. Is this data loss? Where is it lost?

## Method (minimal vehicles, layered)
Two standalone probes drive the real `limina` supervisor (stock Fedora, EFI/BLS, `--net` SSH) with
a second `--disk`, then inspect each layer:
- `probe.sh` — **raw block writes** (a magic string via `dd` at offsets 0 and 1 MiB, then `sync`),
  no filesystem. Checks the guest read-back and the **host backing file** before and after a reboot.
- `probe-ext4.sh` — the **exact failing sequence** (`mkfs.ext4` + mount + write + sync + umount),
  then checksums the first 8 MiB of `vdb` from both the guest and the host file, before and after a
  reboot, and tries to remount.

## Findings (decisive)
1. **Raw block data survives a reboot perfectly** (`probe.sh`): the guest reads both magics back
   after the reboot, the host file is intact, and `vdb` stays `vdb` (attach order is reboot-stable
   too). So basic durability + ordering across a reboot are fine. **Not data loss.**
2. **The ext4 bytes are also 100% intact** (`probe-ext4.sh`): all four checksums of the first 8 MiB
   are *identical* — `guest-pre == host-pre == guest-post == host-post`. The mkfs/sync reached the
   host file exactly, and nothing changed it across the reboot.
3. **The real error is a capacity/geometry mismatch.** dmesg after the reboot:
   ```
   EXT4-fs (vdb): bad geometry: block count 262144 exceeds size of device (262080 blocks)
   ```
   The fs was sized to 262144 × 1 KiB = 256 MiB (the first-boot capacity), but after the reboot the
   device reports 262080 × 1 KiB = 256 MiB − **64 KiB**. The **backing file shrank by exactly 64 KiB**
   (the leftover `data.raw` is 268369920 bytes, not the `truncate -s 256M` = 268435456). The raw
   probe's file is still exactly 64 MiB.

## Root cause
`mkfs.ext4` issues DISCARD over the device (virtio-blk advertises `VIRTIO_BLK_F_DISCARD`,
`third_party/libkrun/src/devices/src/virtio/block/device.rs:291`). The worker forwards it to imago
(`block/worker.rs:262` → `discard_to_any`). imago's Raw discard tries
`try_discard_by_truncate` **first** (imago-0.2.2 `src/file.rs:818-838`): when the discard range
reaches EOF (`offset + length >= size`), it does **`file.set_len(offset)`** — i.e. **truncates the
backing file** to the discard's start. mkfs discards the device tail (the region past the last ext4
block group, which reaches EOF) → the file is truncated by 64 KiB, and mkfs never rewrites that tail
(it's outside the fs).

The virtio-blk **capacity is read from the file size at open** (`device.rs:304` →
`DiskProperties::new`, `device.rs:82` `disk_size >> SECTOR_SHIFT`). So:
- first boot: file = 256 MiB → capacity 524288 sectors → mkfs makes a 256 MiB fs (mounts fine);
- discard truncates the file to 256 MiB − 64 KiB;
- reboot: the new worker opens the now-shorter file → capacity 524160 sectors → the 256 MiB fs no
  longer fits → mount fails.

This is imago being reasonable for a *growable* image (truncate to reclaim tail space) but **wrong
for a fixed-capacity virtio-blk backing file**, where the device capacity must be stable across
opens. A block device's discard must not change its capacity.

## Fix options (for the follow-up)
- **A — drop the discard feature** (smallest): don't advertise `VIRTIO_BLK_F_DISCARD` /
  `VIRTIO_BLK_F_WRITE_ZEROES`(unmap) in libkrun's block device (`device.rs:291-292`). No discard →
  no truncate → stable capacity. Loses thin-provisioning (the data-disk file only grows). A libkrun
  patch (we carry those).
- **B — re-grow after discard** (keeps the feature, surgical): in the worker's DISCARD/WRITE_ZEROES
  handler (`block/worker.rs:262-300`), after the discard, restore the backing file's logical size to
  the device capacity (`set_len`/`resize` back to `nsectors * 512`). The truncate still reclaims the
  blocks; the re-grow restores the logical size sparsely → space freed AND capacity stable. ~10 lines
  in code we already patch.
- **C — punch-hole, don't truncate** (cleanest upstream): make discard preserve `st_size` (use the
  `F_PUNCHHOLE`/`fallocate(PUNCH_HOLE)` path, never `try_discard_by_truncate`). Correct fix, but it's
  in imago (a crates.io dep) → needs vendoring + `[patch.crates.io]`.

Recommendation: **B** — keeps discard's space reclaim, fixes the capacity invariant, and lives in
the libkrun block worker we already patch. (A if we want the absolute minimal change and don't need
thin reclaim yet.)

## Fix applied: C (imago punch-hole) — VALIDATED
We shipped **C** (the cleanest, upstream-correct fix). imago is vendored under `third_party/imago`
(gitignored, mirroring `third_party/libkrun`) and patched via a `git format-patch` series:
- `patches/imago/0001` — `File::try_discard_by_truncate` never truncates; every discard falls
  through to the punch-hole path (`F_PUNCHHOLE` on macOS), which reclaims blocks while preserving
  the file's logical size, so the device capacity is stable across opens.
- `patches/imago/0002` — pin imago's `vm-memory` to `^0.17` so it unifies with the libkrun stack
  (krun-arch/devices/hvf/vmm are all on 0.17.2); the loose upstream range let the resolver pick a
  semver-incompatible vm-memory for imago, breaking the `ImagoAsRef<VolatileSlice>` bound.

limina's **root `Cargo.toml`** overrides the registry crate with `[patch.crates-io] imago =
{ path = "third_party/imago" }` (the workspace root is what builds the graph; third_party is
excluded). Vendor/patch with `scripts/apply-imago-patch.sh`. imago is pinned to **0.2.2** to match
libkrun's own lock (limina's lock had drifted to 0.2.3, which is what surfaced the vm-memory
mismatch). Rationale: `patches/imago/README.md`.

Re-running `probe-ext4.sh` against the fixed worker: **`ext4 remount: OK`**, the marker reads back
after the reboot, all four checksums identical, and the backing file stays **exactly 256 MiB**
(delta 0; the pre-fix file was −64 KiB). The L2 test `crates/limina-test/tests/disks.rs` now also
asserts cross-reboot durability + a stable `vdb` capacity as a permanent regression guard.

## Reusable artifacts
- `probe.sh` — raw-block reboot-durability + ordering probe.
- `probe-ext4.sh` — ext4 reboot probe with four-way checksums + dmesg (reproduces the bug and
  localizes it to capacity, not data).
Both reuse the already-built+codesigned `target/debug` binaries; run under the Bash tool's
`dangerouslyDisableSandbox` (HVF + network). They leave their `/tmp/m10-*` work dir for inspection.

## Note
This is **orthogonal to M10's CLI/ordering work** (which is correct and validated): it affects any
disk that receives a tail-reaching discard, not just a second disk. The root disk doesn't trip it
(btrfs, and its capacity isn't re-derived into a guest-side geometry the same way). The shipped M10
test asserts within-boot durability and does not depend on this; cross-reboot durability returns for
free once the discard fix lands.
