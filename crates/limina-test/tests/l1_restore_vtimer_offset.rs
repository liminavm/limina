// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A restored guest's virtual counter must keep the offset it was restored with.
//!
//! The guest's counter is `mach_absolute_time() - vtimer_offset`. A live pause hides its interval
//! by advancing the offset, so any VM that has paused and resumed — a host sleep on the backstop
//! path hides the whole sleep — carries a non-zero offset, and a snapshot saves it. A restore
//! writes it into HVF. The worker keeps its own record of the offset too, which the next pause
//! advances; if a restore leaves that record at 0, the next pause rebuilds the offset from 0 and
//! the restored part is lost: the guest's counter jumps by it. Measured with a 20 s offset: the
//! restored guest stopped running at the pause and never came back.
//!
//! The first pause is the host-sleep seam (`LIMINA_HOST_SLEEP_SEAM=1` + `SIGURG`, see
//! `host_sleep_clock.rs`): the L1 guest's virtiofs root cannot enter s2idle, so the bracket takes
//! the backstop path, pauses the vCPUs itself, and on wake resumes them with the stop hidden. The
//! second, after the restore, is a snapshot whose save fails: the vCPUs park, the write is
//! refused, and they resume with the pause hidden — with no guest suspend in flight, so the
//! guest's timekeeping is live and a counter jump lands in its `CLOCK_MONOTONIC`.
//! Oracle: the guest's monotonic clock across that second pause, against the host's.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const CPUS: u8 = 2;
/// How long the first pause holds the worker: about the offset the snapshot carries.
const FIRST_HOLD: Duration = Duration::from_secs(20);

fn last_mono_ms(console: &str) -> Option<u64> {
    console.lines().rev().find_map(|line| {
        let rest = &line[line.find("mono_ms=")? + "mono_ms=".len()..];
        rest.split_whitespace().next()?.parse().ok()
    })
}

fn worker_stopped(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with('T'))
        .unwrap_or(false)
}

/// Put the guest through a simulated host sleep that holds the stopped worker for `hold`, and
/// return how far its monotonic clock and the host's moved, in ms, from just before the sleep to
/// once the guest runs again.
fn host_sleep(guest: &Guest, hold: Duration) -> (u64, u64) {
    let worker = guest.worker_pid().expect("resolving the worker pid");
    let mono_before = last_mono_ms(&guest.console()).expect("a heartbeat before the sleep");
    let since = Instant::now();
    assert_eq!(
        unsafe { libc::kill(worker, libc::SIGURG) },
        0,
        "SIGURG to the worker"
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    while !worker_stopped(worker) {
        assert!(
            Instant::now() < deadline,
            "the worker never stopped — the sleep seam did not reach its release point"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(hold);
    assert_eq!(
        unsafe { libc::kill(worker, libc::SIGCONT) },
        0,
        "SIGCONT to the worker"
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(m) = last_mono_ms(&guest.console())
            && m > mono_before + 2000
        {
            for line in guest.supervisor_log().lines() {
                if line.contains("host sleep") || line.contains("host wake") {
                    eprintln!("  bracket: {line}");
                }
            }
            return (m - mono_before, since.elapsed().as_millis() as u64);
        }
        assert!(
            Instant::now() < deadline,
            "the guest never ran again after the wake"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_restored_guest_keeps_its_counter_across_the_next_pause() {
    if !limina_test::require_hvf_or_skip("a_restored_guest_keeps_its_counter_across_the_next_pause")
    {
        return;
    }
    let base = || {
        let mut c = GuestConfig::l1_from_env()
            .expect("resolving L1 guest config")
            .append_cmdline("limina.counter")
            .with_supervisor_log()
            .with_env("LIMINA_HOST_SLEEP_SEAM", "1");
        c.cpus = CPUS;
        c.ram_mib = 512;
        c
    };

    // --- Guest 1: give it a paused past, then snapshot it ---
    let mut g1 = Guest::boot(
        &base()
            .with_snapshot()
            .with_env("LIMINA_RAW_SNAPSHOT_SEAM", "1"),
    )
    .expect("spawning the first supervisor");
    g1.wait_for("LIMINA_COUNTER_READY", Duration::from_secs(20))
        .expect("counter guest did not reach userspace");
    g1.wait_for("mono_ms=", Duration::from_secs(5))
        .expect("no counter heartbeat");
    let (guest_ms, host_ms) = host_sleep(&g1, FIRST_HOLD);
    eprintln!("first pause: host {host_ms} ms, guest monotonic {guest_ms} ms");
    // The guest runs normally through the bracket's quiesce attempt; only the hold, while the
    // worker was stopped, must be missing from its clock. If it is not, the offset is still 0
    // and the rest proves nothing.
    assert!(
        guest_ms + FIRST_HOLD.as_millis() as u64 / 2 < host_ms,
        "the guest's clock absorbed the first hold ({guest_ms} of {host_ms} ms)"
    );

    g1.snapshot().expect("triggering the snapshot");
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(30))
        .expect("supervisor did not exit after the snapshot");
    assert_eq!(
        outcome.code,
        Some(126),
        "the VM should suspend: {outcome:?}"
    );
    let snap = std::env::temp_dir().join(format!("limina-vtimer-{}.bin", std::process::id()));
    std::fs::copy(g1.snapshot_path().expect("snapshot path"), &snap).expect("copying the snapshot");
    drop(g1);
    let cleanup = {
        let snap = snap.clone();
        move || {
            let _ = std::fs::remove_file(&snap);
            let _ = std::fs::remove_file(snap.with_extension("bin.consumed"));
        }
    };

    // --- Guest 2: restore it, and pause it again ---
    let mut g2 = Guest::boot(
        &base()
            .with_env("LIMINA_RAW_SNAPSHOT_SEAM", "1")
            .restore_from(&snap),
    )
    .expect("spawning the restoring supervisor");
    g2.wait_for_supervisor_log("resumed from snapshot at pc=", Duration::from_secs(30))
        .unwrap_or_else(|e| {
            cleanup();
            panic!("the restored guest never resumed: {e}");
        });
    cleanup();
    // A heartbeat from the restored guest itself, past the snapshot's last one.
    let restored_from = last_mono_ms(&g2.console()).unwrap_or(0);
    let deadline = Instant::now() + Duration::from_secs(20);
    while last_mono_ms(&g2.console()).unwrap_or(0) <= restored_from {
        assert!(Instant::now() < deadline, "the restored guest never ran");
        std::thread::sleep(Duration::from_millis(200));
    }
    // The restore consumed the snapshot by renaming it, so its path is free: a directory there
    // makes the next save fail after the vCPUs have parked.
    std::fs::create_dir_all(&snap).expect("blocking the snapshot path");
    let mono_before = last_mono_ms(&g2.console()).expect("a heartbeat before the pause");
    let since = Instant::now();
    let worker = g2.worker_pid().expect("resolving the restored worker");
    assert_eq!(
        unsafe { libc::kill(worker, libc::SIGUSR1) },
        0,
        "SIGUSR1 to the worker"
    );
    let aborted = g2.wait_for_supervisor_log(
        "snapshot: aborted and guest resumed",
        Duration::from_secs(60),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let guest_ms = loop {
        if let Some(m) = last_mono_ms(&g2.console())
            && m > mono_before + 2000
        {
            break m - mono_before;
        }
        if Instant::now() >= deadline {
            break 0;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let host_ms = since.elapsed().as_millis() as u64;
    let _ = std::fs::remove_dir(&snap);
    eprintln!("pause after the restore: host {host_ms} ms, guest monotonic {guest_ms} ms");
    let _ = g2.shutdown(Duration::from_secs(10));
    aborted.expect("the failed save never resumed the guest");
    assert!(
        guest_ms > 0,
        "the restored guest never ran again after a pause: the pause rebuilt the counter offset \
         without the restored part"
    );

    assert!(
        guest_ms <= host_ms + 2000,
        "across a pause after the restore the guest's monotonic clock moved {guest_ms} ms while \
         the host's moved {host_ms} ms: the pause rebuilt the counter offset without the \
         restored part, and the guest's clock jumped forward by it"
    );
}
