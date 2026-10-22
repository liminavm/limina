// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A guest suspended with virtio-mem memory plugged resumes with it plugged and its contents
//! intact (docs/design/guest-ram-objects.md §3).
//!
//! The region sits above the shared-memory window, where the snapshot takes nothing by itself,
//! and the guest's driver reads the plugged size back on thaw: the snapshot carries the plugged
//! blocks as RAM and the device's state beside them.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;
const PLUG: u64 = 1024 * MIB;

fn boot_id(guest: &Guest) -> String {
    guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading the boot id")
        .trim()
        .to_string()
}

fn blob_sum(guest: &Guest) -> String {
    guest
        .ssh_exec("sudo sha256sum /dev/shm/blob")
        .expect("checksumming the blob")
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn plugged_virtio_mem_survives_a_suspend_and_restore() {
    if !limina_test::require_hvf_or_skip("plugged_virtio_mem_survives_a_suspend_and_restore") {
        return;
    }
    let base = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = 4096;
            cfg.with_net()
                .with_balloon_control()
                .with_env("LIMINA_VIRTIO_MEM", "2048")
                .with_supervisor_log()
        }
        Err(e) => {
            eprintln!("SKIPPED plugged_virtio_mem_survives_a_suspend_and_restore: {e}");
            return;
        }
    };

    let mut g1 = Guest::boot(&base.clone().with_snapshot()).expect("spawning the supervisor");
    g1.wait_for_ssh(Duration::from_secs(300))
        .expect("guest did not reach sshd");
    let booted = boot_id(&g1);
    g1.set_mem_target(PLUG).expect("asking the guest to plug");
    let deadline = Instant::now() + Duration::from_secs(60);
    while g1.balloon_stats().expect("stats").mem_plugged < PLUG {
        assert!(Instant::now() < deadline, "the guest did not plug 1 GiB");
        std::thread::sleep(Duration::from_millis(500));
    }
    // Fill nearly all of the guest's free memory, so the blob lands in plugged blocks too.
    let fill = g1
        .ssh_exec(
            r#"sudo sh -c '
              avail=$(awk "/^MemAvailable:/ {print int(\$2 / 1024)}" /proc/meminfo)
              mount -o remount,size=$((avail + 512))M /dev/shm
              dd if=/dev/urandom of=/dev/shm/blob bs=1M count=$((avail - 384)) 2>/dev/null
              ls -l /dev/shm/blob'"#,
        )
        .expect("writing the blob");
    let sum = blob_sum(&g1);
    eprintln!("blob before the suspend: {fill} sha256 {sum}");

    g1.suspend_bracket().expect("sending the suspend bracket");
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(180))
        .expect("supervisor did not exit after the suspend bracket");
    let suspend_log = g1.supervisor_log();
    assert_eq!(
        outcome.code,
        Some(126),
        "the guest did not suspend: {outcome:?}\n{suspend_log}"
    );

    let dir = std::env::temp_dir().join(format!("limina-l2-vmem-snap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory for the snapshot");
    let snap = dir.join("snapshot");
    let disk = dir.join("disk.raw");
    std::fs::copy(g1.snapshot_path().expect("a snapshot path"), &snap).expect("keeping it");
    limina_test::cow_clone(&g1.scratch_dir().join("disk.raw"), &disk).expect("keeping the disk");
    drop(g1);

    let mut cfg2 = base.restore_from(&snap);
    if let limina_test::Boot::Firmware { disk: d, .. } = &mut cfg2.boot {
        *d = disk.clone();
    }
    let mut g2 = Guest::boot(&cfg2).expect("spawning the restoring supervisor");
    g2.wait_for_launch_kind(1, true, Duration::from_secs(60))
        .expect("the supervisor did not resume the snapshot");
    g2.wait_for_ssh(Duration::from_secs(120))
        .expect("the restored guest never reached sshd");
    assert_eq!(
        boot_id(&g2),
        booted,
        "the guest rebooted instead of resuming\n{}",
        g2.supervisor_log()
    );
    let stats = g2.balloon_stats().expect("stats after the restore");
    assert_eq!(
        stats.mem_plugged,
        PLUG,
        "the restored device has {} MiB plugged",
        stats.mem_plugged / MIB
    );
    assert_eq!(
        blob_sum(&g2),
        sum,
        "the blob changed across the suspend: plugged memory lost its contents"
    );
    eprintln!(
        "restored with {} MiB plugged and the blob intact",
        stats.mem_plugged / MIB
    );

    g2.shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    std::fs::remove_dir_all(&dir).expect("removing the snapshot");
}
