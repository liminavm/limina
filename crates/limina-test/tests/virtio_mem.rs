// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A stock guest plugs the virtio-mem memory the host asks for, uses it, and unplugs it when
//! asked, which gives its pages back to the host (docs/design/guest-ram-objects.md §3, §4.5).
//!
//! Fedora 44's kernel builds `virtio_mem` as a module and onlines hot-added memory itself, into
//! `ZONE_NORMAL`, where unplug is best-effort; the test frees what it touched first, so the
//! memory it asks back is free. It prints the module, the zone and the region the guest saw.

use std::time::{Duration, Instant};

use limina_test::{BalloonStats, Guest, GuestConfig};

const MIB: u64 = 1 << 20;
const REGION_MIB: u64 = 2048;
const PLUG: u64 = 1024 * MIB;

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

fn mem_total(guest: &Guest) -> u64 {
    let out = guest
        .ssh_exec("awk '/^MemTotal:/ {print $2 * 1024}' /proc/meminfo")
        .expect("reading the guest's meminfo");
    out.trim().parse().expect("meminfo gave a number")
}

/// Poll the worker's stats until `done` holds or `timeout` passes; returns the last stats.
fn wait_for(
    guest: &Guest,
    timeout: Duration,
    done: impl Fn(&BalloonStats) -> bool,
) -> BalloonStats {
    let deadline = Instant::now() + timeout;
    loop {
        let stats = guest.balloon_stats().expect("reading the worker's stats");
        if done(&stats) || Instant::now() > deadline {
            return stats;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn a_stock_guest_plugs_and_unplugs_virtio_mem() {
    if !limina_test::require_hvf_or_skip("a_stock_guest_plugs_and_unplugs_virtio_mem") {
        return;
    }
    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = 4096;
            cfg.with_net()
                .with_balloon_control()
                .with_env("LIMINA_VIRTIO_MEM", &REGION_MIB.to_string())
        }
        Err(e) => {
            eprintln!("SKIPPED a_stock_guest_plugs_and_unplugs_virtio_mem: {e}");
            return;
        }
    };
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest sshd never became reachable through the forward");

    let stats = guest.balloon_stats().expect("reading the worker's stats");
    assert_eq!(
        mib(stats.mem_region),
        REGION_MIB,
        "the worker has no virtio-mem device"
    );
    assert_eq!(
        stats.mem_bound, 1,
        "the guest's driver did not bind the device"
    );
    let probe = guest
        .ssh_exec(
            "lsmod | grep -w virtio_mem; sudo dmesg | grep -i virtio_mem; \
             cat /sys/devices/system/memory/auto_online_blocks",
        )
        .unwrap_or_else(|e| format!("(probe failed: {e})"));
    eprintln!("guest view of the device:\n{probe}");
    let total0 = mem_total(&guest);

    guest
        .set_mem_target(PLUG)
        .expect("asking the guest to plug");
    let plugged = wait_for(&guest, Duration::from_secs(60), |s| s.mem_plugged >= PLUG);
    let total1 = mem_total(&guest);
    let zones = guest
        .ssh_exec(
            "for b in /sys/devices/system/memory/memory*; do \
               [ \"$(cat $b/online 2>/dev/null)\" = 1 ] || continue; \
               echo \"$(basename $b) $(cat $b/valid_zones)\"; done | sort -k2 | uniq -c -f1",
        )
        .unwrap_or_else(|e| format!("(zones failed: {e})"));
    eprintln!(
        "plugged {} MiB of {} asked; guest MemTotal {} -> {} MiB; online blocks by zone:\n{zones}",
        mib(plugged.mem_plugged),
        mib(PLUG),
        mib(total0),
        mib(total1)
    );
    assert!(
        plugged.mem_plugged >= PLUG,
        "the guest plugged {} MiB of the {} MiB asked",
        mib(plugged.mem_plugged),
        mib(PLUG)
    );
    assert!(
        total1 >= total0 + PLUG * 9 / 10,
        "the guest's MemTotal grew {} MiB after plugging {} MiB",
        mib(total1.saturating_sub(total0)),
        mib(PLUG)
    );

    // Fill most of the guest's memory so the plugged blocks hold data, then free it all.
    guest
        .ssh_exec(&format!(
            "sudo sh -c 'dd if=/dev/zero of=/dev/shm/fill bs=1M count={} 2>/dev/null; \
             rm /dev/shm/fill' && echo filled",
            mib(total1) * 3 / 4
        ))
        .expect("touching guest memory");
    let fp1 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");

    guest.set_mem_target(0).expect("asking the guest to unplug");
    let unplugged = wait_for(&guest, Duration::from_secs(60), |s| s.mem_plugged == 0);
    std::thread::sleep(Duration::from_secs(1));
    let fp2 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");
    let total2 = mem_total(&guest);
    let gone = plugged.mem_plugged - unplugged.mem_plugged;
    eprintln!(
        "unplugged {} MiB (still plugged {} MiB); guest MemTotal {} MiB; worker footprint {} -> {} \
         MiB",
        mib(gone),
        mib(unplugged.mem_plugged),
        mib(total2),
        mib(fp1),
        mib(fp2)
    );
    assert!(
        gone >= PLUG / 2,
        "the guest unplugged only {} MiB of {} MiB of free memory",
        mib(gone),
        mib(PLUG)
    );
    assert!(
        fp1.saturating_sub(fp2) >= gone / 2,
        "the worker's footprint fell {} MiB after the guest unplugged {} MiB it had touched",
        mib(fp1.saturating_sub(fp2)),
        mib(gone)
    );

    guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
}

/// Plug 1 GiB with the guest onlining hot-added memory as `online` says (`online` puts it in
/// `ZONE_NORMAL`, `online_movable` in `ZONE_MOVABLE`), fill the guest's other memory so new kernel
/// objects must land in the plugged blocks, create 200k tmpfs inodes and keep them, then ask for
/// none. Returns how much the guest plugged and how much it could not unplug.
fn unplug_under_kernel_objects(name: &str, online: &str) -> Option<(u64, u64)> {
    if !limina_test::require_hvf_or_skip(name) {
        return None;
    }
    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = 4096;
            cfg.with_net()
                .with_balloon_control()
                .with_env("LIMINA_VIRTIO_MEM", &REGION_MIB.to_string())
        }
        Err(e) => {
            eprintln!("SKIPPED {name}: {e}");
            return None;
        }
    };
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest sshd never became reachable through the forward");
    guest
        .ssh_exec(&format!(
            "echo {online} | sudo tee /sys/devices/system/memory/auto_online_blocks"
        ))
        .expect("setting the guest's onlining policy");

    guest
        .set_mem_target(PLUG)
        .expect("asking the guest to plug");
    let plugged = wait_for(&guest, Duration::from_secs(60), |s| s.mem_plugged >= PLUG);
    let load = guest
        .ssh_exec(
            r#"sudo sh -c '
              avail=$(awk "/^MemAvailable:/ {print int(\$2 / 1024)}" /proc/meminfo)
              dd if=/dev/zero of=/dev/shm/fill bs=1M count=$((avail - 1024)) 2>/dev/null
              mkdir /dev/shm/inodes && cd /dev/shm/inodes && seq 200000 | xargs touch
              rm /dev/shm/fill
              grep -E "^(Slab|SUnreclaim):" /proc/meminfo
              grep -E "^Node 0, zone|^ +present" /proc/zoneinfo'"#,
        )
        .unwrap_or_else(|e| format!("(load failed: {e})"));

    guest.set_mem_target(0).expect("asking the guest to unplug");
    let left = wait_for(&guest, Duration::from_secs(60), |s| s.mem_plugged == 0).mem_plugged;
    eprintln!(
        "{online}: plugged {} MiB; with 200k inodes held, {} MiB would not unplug\n{load}",
        mib(plugged.mem_plugged),
        mib(left)
    );
    assert!(
        plugged.mem_plugged >= PLUG,
        "the guest plugged {} MiB of the {} MiB asked",
        mib(plugged.mem_plugged),
        mib(PLUG)
    );
    guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    Some((plugged.mem_plugged, left))
}

/// A stock guest's own policy: kernel objects may land in plugged memory and pin it. Measured,
/// not asserted; the balloon stays the stock tier's fallback for what will not unplug.
#[test]
fn kernel_objects_in_normal_onlined_blocks_hold_them() {
    unplug_under_kernel_objects(
        "kernel_objects_in_normal_onlined_blocks_hold_them",
        "online",
    );
}

/// Movable onlining keeps kernel objects out of plugged memory, so all of it unplugs.
#[test]
fn movable_onlined_blocks_unplug_whole_under_kernel_objects() {
    let Some((_, left)) = unplug_under_kernel_objects(
        "movable_onlined_blocks_unplug_whole_under_kernel_objects",
        "online_movable",
    ) else {
        return;
    };
    assert_eq!(
        left,
        0,
        "{} MiB of movable-onlined memory would not unplug",
        mib(left)
    );
}
