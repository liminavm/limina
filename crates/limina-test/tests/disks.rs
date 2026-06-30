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
//!     block device within a boot);
//!   - the ext4 survives a guest **reboot** (worker relaunch) — both the data (the marker reads
//!     back) and the device **geometry** (`vdb` is still exactly 64 MiB). This is the regression
//!     guard for the discard-truncate bug: `mkfs.ext4` discards the device tail, which used to
//!     make imago truncate the backing file, shrinking the capacity on relaunch so the fs no
//!     longer fit ("bad geometry"). Fixed by patches/imago/0001 (discard punch-holes, never
//!     truncates). See spikes/m10-disk-durability/RESULTS.md.
//!   - **stable identity (Phase 2):** the disk resolves by `/dev/disk/by-id/virtio-<block_id>`
//!     (boot disk `virtio-root` → vda, second disk `virtio-disk1` → vdb), and that handle is
//!     stable across the reboot. libkrun patch 0038 exposes the positional `block_id` as the
//!     virtio-blk serial; without it the serial is the host inode hash (not clone/move-stable).
//!
//! This is the empirical confirmation of the design's §4.1 ordering claim (host order is
//! source-deterministic; this proves the guest names follow it under the real kernel).
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

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

    // Stable identity (Phase 2): the worker assigns positional block ids (`disks[0]`=`"root"`,
    // second disk=`"disk1"`), and libkrun patch 0038 exposes block_id as the virtio-blk serial, so
    // the guest gets `/dev/disk/by-id/virtio-<block_id>`. Assert the data disk resolves by id to
    // vdb (and the boot disk to vda). RED without 0038: the serial would be the host inode hash, so
    // `virtio-disk1` wouldn't exist and the readlink returns empty. The by-id is what survives an
    // image clone/move (the inode serial doesn't) — the M9.4 snapshot-clone handle.
    guest
        .ssh_poll(
            "test -e /dev/disk/by-id/virtio-disk1 && echo ok",
            Duration::from_secs(15),
        )
        .expect("/dev/disk/by-id/virtio-disk1 never appeared — serial != block_id (patch 0038?)");
    let byid_data = guest
        .ssh_exec("readlink -f /dev/disk/by-id/virtio-disk1")
        .expect("resolving virtio-disk1 by-id");
    assert_eq!(
        byid_data.trim(),
        "/dev/vdb",
        "virtio-disk1 by-id does not resolve to vdb"
    );
    let byid_root = guest
        .ssh_exec("readlink -f /dev/disk/by-id/virtio-root")
        .expect("resolving virtio-root by-id");
    assert_eq!(
        byid_root.trim(),
        "/dev/vda",
        "virtio-root by-id does not resolve to the boot disk vda"
    );
    eprintln!("by-id: virtio-root → /dev/vda, virtio-disk1 → /dev/vdb ✓");

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

    // Cross-reboot durability + the discard-truncate regression guard. Reboot the guest (a
    // worker relaunch — see reboot.rs) and re-open vdb. With the pre-fix imago, mkfs.ext4's
    // tail discard had truncated the backing file, so on relaunch vdb came back 64 KiB short
    // and the ext4 (sized to the original capacity) failed to mount with "bad geometry". We
    // assert both invariants hold: the device is still exactly 64 MiB (capacity stable across
    // relaunch) AND the filesystem mounts and the marker survives.
    let boot_id_1 = guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading boot id before reboot");
    eprintln!("boot id before reboot: {}", boot_id_1.trim());
    guest
        .ssh_exec("sudo systemd-run --on-active=1 systemctl reboot")
        .expect("scheduling guest reboot");

    let deadline = Instant::now() + Duration::from_secs(180);
    let mut relaunched = false;
    while Instant::now() < deadline {
        if let Ok(out) = guest.ssh_exec("cat /proc/sys/kernel/random/boot_id") {
            let id = out.trim();
            if !id.is_empty() && id != boot_id_1.trim() {
                eprintln!("boot id after reboot:  {id} (relaunch confirmed)");
                relaunched = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    assert!(
        relaunched,
        "guest never came back with a fresh boot after reboot — relaunch failed"
    );

    // udev may settle a beat after sshd on the fresh boot.
    guest
        .ssh_poll("test -b /dev/vdb && echo ok", Duration::from_secs(30))
        .expect("/dev/vdb did not reappear after the reboot");
    let sectors_post = guest
        .ssh_exec("cat /sys/block/vdb/size")
        .expect("reading /sys/block/vdb/size after reboot")
        .trim()
        .parse::<u64>()
        .expect("post-reboot vdb size not a number");
    assert_eq!(
        sectors_post, DATA_DISK_SECTORS,
        "vdb capacity changed across the reboot: {sectors_post} sectors (expected \
         {DATA_DISK_SECTORS}) — a discard truncated the backing file"
    );
    let marker_post = guest
        .ssh_exec("sudo mount /dev/vdb /mnt/data && cat /mnt/data/marker")
        .expect("mounting vdb after reboot (bad geometry would fail here)");
    assert_eq!(
        marker_post.trim(),
        MARKER,
        "data written to vdb did not survive the reboot"
    );
    guest
        .ssh_exec("sudo umount /mnt/data")
        .expect("unmounting vdb after reboot");
    eprintln!("vdb data + geometry survived a reboot ✓");

    // The by-id handle is stable across the reboot too (the whole point — a name that doesn't
    // depend on probe order or the host inode).
    let byid_data_post = guest
        .ssh_exec("readlink -f /dev/disk/by-id/virtio-disk1")
        .expect("resolving virtio-disk1 by-id after reboot");
    assert_eq!(
        byid_data_post.trim(),
        "/dev/vdb",
        "virtio-disk1 by-id did not survive the reboot"
    );
    eprintln!("by-id virtio-disk1 → /dev/vdb stable across reboot ✓");

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}

/// 64 MiB *virtual* qcow2; the physical file is ~200 KiB, so the guest only sees 64 MiB if the
/// worker opened it AS qcow2 (the discriminator for format auto-detection).
const QCOW2_VIRT_BYTES: u64 = 64 * 1024 * 1024;
const QCOW2_VIRT_SECTORS: u64 = QCOW2_VIRT_BYTES / 512;
const QCOW2_MARKER: &str = "limina-m10-qcow2-roundtrip";

/// M10 Phase 4: a **qcow2** data disk is auto-detected, read-write, and survives a reboot.
///
/// The worker detects the image format by magic (`krun/mod.rs` `detect_image_type`) — no `:qcow2`
/// suffix needed. The decisive check is `/sys/block/vdb/size`: a `qemu-img`-created qcow2 has a
/// 64 MiB *virtual* size but a tiny physical file, so the guest sees 64 MiB only if it was opened
/// as qcow2; opened as raw, vdb would be the ~200 KiB physical size (RED without detection). Then
/// mkfs + write + reboot + read-back proves the qcow2 is genuinely read-write through imago and its
/// capacity is stable across a worker relaunch.
#[test]
fn qcow2_data_disk_reads_writes_and_survives_reboot() {
    if !limina_test::require_hvf_or_skip("qcow2_data_disk_reads_writes_and_survives_reboot") {
        return;
    }

    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_net()
        .with_qcow2_data_disk(QCOW2_VIRT_BYTES);
    eprintln!("booting stock Fedora (EFI/BLS) with a 64 MiB-virtual qcow2 as the 2nd --disk");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest did not reach sshd");
    guest
        .ssh_poll("test -b /dev/vdb && echo ok", Duration::from_secs(30))
        .expect("/dev/vdb never appeared — the qcow2 did not enumerate");

    // Discriminator: the guest sees the qcow2's VIRTUAL size only if it was opened as qcow2.
    let sectors = guest
        .ssh_exec("cat /sys/block/vdb/size")
        .expect("reading /sys/block/vdb/size")
        .trim()
        .parse::<u64>()
        .expect("vdb size not a number");
    assert_eq!(
        sectors, QCOW2_VIRT_SECTORS,
        "vdb is {sectors} sectors; expected {QCOW2_VIRT_SECTORS} (64 MiB virtual) — the qcow2 was \
         opened as raw (physical size), so format auto-detection didn't fire"
    );

    // Read-write through imago: mkfs + write a marker + sync + unmount.
    guest
        .ssh_exec("sudo mkfs.ext4 -q -F /dev/vdb")
        .expect("mkfs.ext4 on the qcow2 vdb");
    guest
        .ssh_exec(&format!(
            "sudo mkdir -p /mnt/data && sudo mount /dev/vdb /mnt/data && \
             echo {QCOW2_MARKER} | sudo tee /mnt/data/marker >/dev/null && sync && sudo umount /mnt/data"
        ))
        .expect("mkfs + mount + write marker + unmount the qcow2");

    // Reboot (worker relaunch): the qcow2 reopens, capacity is stable, data survives.
    let boot_id_1 = guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading boot id before reboot");
    guest
        .ssh_exec("sudo systemd-run --on-active=1 systemctl reboot")
        .expect("scheduling guest reboot");
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut relaunched = false;
    while Instant::now() < deadline {
        if let Ok(out) = guest.ssh_exec("cat /proc/sys/kernel/random/boot_id") {
            let id = out.trim();
            if !id.is_empty() && id != boot_id_1.trim() {
                relaunched = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    assert!(relaunched, "guest never came back after reboot");

    guest
        .ssh_poll("test -b /dev/vdb && echo ok", Duration::from_secs(30))
        .expect("/dev/vdb did not reappear after reboot");
    let sectors_post = guest
        .ssh_exec("cat /sys/block/vdb/size")
        .expect("reading /sys/block/vdb/size after reboot")
        .trim()
        .parse::<u64>()
        .expect("post-reboot vdb size not a number");
    assert_eq!(
        sectors_post, QCOW2_VIRT_SECTORS,
        "qcow2 vdb capacity changed across the reboot: {sectors_post} sectors"
    );
    let marker = guest
        .ssh_exec("sudo mount /dev/vdb /mnt/data && cat /mnt/data/marker")
        .expect("mounting the qcow2 after reboot");
    assert_eq!(
        marker.trim(),
        QCOW2_MARKER,
        "data written to the qcow2 did not survive the reboot"
    );
    guest
        .ssh_exec("sudo umount /mnt/data")
        .expect("unmounting the qcow2 after reboot");
    eprintln!("qcow2 vdb: 64 MiB virtual, read-write, data + capacity survived a reboot ✓");

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}

/// M10 Phase 3a — **boot from install media**. An EFI-bootable aarch64 installer ISO, attached as
/// the *sole* disk (so it is `vda`, read-only) via the firmware path, must reach its own
/// bootloader: the firmware's El Torito + FAT driver stack discovers the embedded ESP and
/// chainloads `\EFI\BOOT\BOOTAA64.EFI` (→ GRUB). No `--kernel`, no separate root disk, no guest
/// agent — pure firmware self-discovery. This is the path to installing a fresh guest from scratch.
///
/// Evidence: GRUB renders its menu over the firmware console (PL011 serial + GOP); reaching
/// "GRUB version" proves the El Torito EFI image was found and launched, and "Install Fedora"
/// confirms it is *this ISO's* installer menu (not a stray match). The matching GOP-scanout PNG is
/// the separate visual proof in `spikes/m10-iso-boot/`.
///
/// SKIPs when HVF is unavailable or the large gitignored ISO / GOP firmware is absent (like the
/// other image-gated L2s); set `LIMINA_TEST_ISO` / `LIMINA_GOP_FIRMWARE` to point elsewhere.
#[test]
fn boots_efi_iso_to_bootloader() {
    if !limina_test::require_hvf_or_skip("boots_efi_iso_to_bootloader") {
        return;
    }
    let cfg = match GuestConfig::iso_boot_from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP boots_efi_iso_to_bootloader: {e}");
            return;
        }
    };
    eprintln!("booting an EFI-bootable aarch64 installer ISO as the sole disk on the GOP firmware");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor on the ISO");

    // Firmware → El Torito → ESP → BOOTAA64.EFI → GRUB. Generous timeout: firmware init + GRUB.
    guest
        .wait_for("GRUB version", Duration::from_secs(90))
        .expect(
            "firmware did not chainload the ISO's bootloader (GRUB) from its El Torito EFI image",
        );
    // It's the ISO's own installer menu, not an incidental "GRUB version" elsewhere.
    guest
        .wait_for("Install Fedora", Duration::from_secs(10))
        .expect("the ISO's GRUB installer menu did not render");
    eprintln!("ISO booted to its GRUB bootloader (El Torito → ESP → BOOTAA64.EFI) ✓");

    // The installer guest has no OS to power off; teardown SIGTERMs the supervisor, which SIGKILLs
    // the worker after its grace window (so the outcome is a forced stop — that's expected here).
    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
