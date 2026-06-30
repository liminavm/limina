// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 multiple-disks test (M10): a second `--disk` enumerates as `vdb`, read-write, durable,
//! and the boot disk stays `vda` — proving attach order = device order.
//!
//! Drives the real `limina` supervisor on the STOCK Fedora image via EFI firmware → GRUB → BLS
//! (so root mounts by `root=UUID=`, the shipping-tier path — *not* the dev direct-kernel
//! `root=/dev/vda3`). The harness attaches a blank 64 MiB data disk after the boot disk and we
//! SSH in (over `--net`) to inspect the guest:
//!   - `/dev/vdb` exists, is read-write, and is exactly the 64 MiB we created (distinguishing it
//!     from the multi-GB root disk) → the SECOND `--disk` is `vdb`, in order;
//!   - `/` is still backed by a `vda` partition → attaching a data disk did not shift root;
//!   - mkfs + mount + write + sync + unmount + REMOUNT round-trips the data (durable to the
//!     block device within a boot).
//!
//! This is the empirical confirmation of the design's §4.1 ordering claim (host order is
//! source-deterministic; this proves the guest names follow it under the real kernel). It does
//! NOT cover cross-reboot durability — block writeback persistence across a worker relaunch is a
//! separate concern from the disk-attach ordering this test owns.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// 64 MiB blank data disk → 131072 × 512-byte sectors in `/sys/block/vdb/size`.
const DATA_DISK_BYTES: u64 = 64 * 1024 * 1024;
const DATA_DISK_SECTORS: u64 = DATA_DISK_BYTES / 512;
const MARKER: &str = "limina-m10-vdb-data-roundtrip";

#[test]
fn second_disk_is_vdb_read_write_and_durable() {
    if !limina_test::require_hvf_or_skip("second_disk_is_vdb_read_write_and_durable") {
        return;
    }

    // Stock Fedora via EFI firmware (BLS root=UUID) + NAT for SSH + a blank 64 MiB data disk.
    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_net()
        .with_blank_data_disk(DATA_DISK_BYTES);
    eprintln!(
        "booting stock Fedora (EFI/BLS) with a {} MiB blank data disk as the 2nd --disk",
        DATA_DISK_BYTES / 1024 / 1024
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Full stock userspace boot (firmware → GRUB → kernel → systemd → NM → sshd) is slow.
    guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest did not reach sshd");

    // The data disk is attached at boot; udev may settle a beat after sshd, so poll for it.
    guest
        .ssh_poll("test -b /dev/vdb && echo ok", Duration::from_secs(30))
        .expect("/dev/vdb never appeared — the second --disk did not enumerate as vdb");

    // Ordering proof #1: vdb is exactly the size WE created, not the multi-GB root image.
    let sectors = guest
        .ssh_exec("cat /sys/block/vdb/size")
        .expect("reading /sys/block/vdb/size")
        .trim()
        .parse::<u64>()
        .expect("vdb size not a number");
    assert_eq!(
        sectors, DATA_DISK_SECTORS,
        "vdb is {sectors} sectors; expected {DATA_DISK_SECTORS} (64 MiB) — wrong disk landed at vdb"
    );

    // Ordering proof #2: vdb is read-write (not the :ro path), and root is still on vda.
    let ro = guest
        .ssh_exec("cat /sys/block/vdb/ro")
        .expect("reading /sys/block/vdb/ro");
    assert_eq!(ro.trim(), "0", "vdb came up read-only; expected read-write");

    let root_src = guest
        .ssh_exec("findmnt -n -o SOURCE /")
        .expect("reading the root device");
    eprintln!("root device: {}", root_src.trim());
    assert!(
        root_src.contains("vda"),
        "root is not on the boot disk (vda): {root_src:?} — attaching a data disk shifted root"
    );
    assert!(
        !root_src.contains("vdb"),
        "root landed on the data disk (vdb): {root_src:?}"
    );

    // RW + durability round-trip: format vdb, mount it, write a marker, sync, unmount, then
    // REMOUNT (same boot) and read it back. The unmount/remount proves the write is durable to
    // the block device (not just cached in the mounted fs) — the M10 disk-attach claim. (Whole-
    // VM reboot durability is a separate block-writeback concern, not this test's subject.)
    guest
        .ssh_exec("sudo mkfs.ext4 -q -F /dev/vdb")
        .expect("mkfs.ext4 on /dev/vdb");
    guest
        .ssh_exec(&format!(
            "sudo mkdir -p /mnt/data && sudo mount /dev/vdb /mnt/data && \
             echo {MARKER} | sudo tee /mnt/data/marker >/dev/null && sync && sudo umount /mnt/data"
        ))
        .expect("mkfs + mount + write marker + unmount vdb");
    let marker = guest
        .ssh_exec("sudo mount /dev/vdb /mnt/data && cat /mnt/data/marker")
        .expect("remounting vdb and reading the marker back");
    assert_eq!(
        marker.trim(),
        MARKER,
        "data written to vdb did not survive an unmount/remount"
    );
    guest
        .ssh_exec("sudo umount /mnt/data")
        .expect("unmounting vdb");
    eprintln!("vdb data round-tripped through unmount/remount ✓");

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
