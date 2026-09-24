// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A suspend that aborts after its devices quiesced must come through a host sleep safely.
//!
//! The `willSleep` bracket waits for every virtio device to reach `INIT`, then for every vCPU
//! to park. A guest whose suspend aborts after the first step brings its devices back and runs
//! on. Whatever the bracket concludes, it must not be `Parked` — that outcome resumes keeping
//! the counter, and the whole host sleep would land in the running guest's `CLOCK_MONOTONIC`,
//! where nothing can reclaim it — and the guest must come back without being woken as ours.
//!
//! `pm_test=devices` makes the abort deterministic: the kernel suspends every device, waits
//! `pm_test_delay`, and resumes them without sleeping — exactly a suspend that gave up after
//! `dpm_suspend`. The host sleep is the `LIMINA_HOST_SLEEP_SEAM` stand-in.
//!
//! This pins the path end to end; it does not isolate the park wait's device recheck. On HVF a
//! running guest was never once seen with every vCPU parked across an 8 s wait, with 4 vCPUs or
//! with 1, so here the bracket settles on `DevicesOnly` before the recheck matters. The
//! recheck is pinned by `quiesce::tests::an_aborted_suspend_is_not_parked`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// Long enough that a MONOTONIC that absorbed it cannot hide inside the tolerance.
const SLEEP_GAP: Duration = Duration::from_secs(60);

/// The guest's own running time either side of the stop, with headroom (see
/// `host_sleep_clock`, where it measured ~16 s).
const MONOTONIC_TOLERANCE_S: f64 = 30.0;

fn clocks(guest: &Guest, when: &str) -> (f64, f64) {
    let out = guest
        .ssh_poll(
            "python3 -c 'import time; print(time.clock_gettime(time.CLOCK_REALTIME), \
             time.clock_gettime(time.CLOCK_MONOTONIC))'",
            Duration::from_secs(90),
        )
        .unwrap_or_else(|e| panic!("reading the guest clocks {when}: {e}"));
    let v: Vec<f64> = out
        .split_whitespace()
        .map(|f| f.parse().expect("parsing a guest clock"))
        .collect();
    assert_eq!(v.len(), 2, "expected two clocks {when}, got {out:?}");
    (v[0], v[1])
}

/// True once the worker is stopped (`T`) — the seam released the ack and cut the vCPUs.
fn worker_stopped(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with('T'))
        .unwrap_or(false)
}

#[test]
fn an_aborted_suspend_is_not_taken_for_parked() {
    if !limina_test::require_hvf_or_skip("an_aborted_suspend_is_not_taken_for_parked") {
        return;
    }

    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_supervisor_log()
            .with_env("LIMINA_HOST_SLEEP_SEAM", "1")
            .with_env("RUST_LOG", "warn,limina_vmm=info,limina=info,krun_vmm=info"),
        Err(e) => {
            eprintln!("SKIPPED an_aborted_suspend_is_not_taken_for_parked: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd never became reachable");
    eprintln!("guest SSH up: {banner}");

    // Judge the kernel's own accounting, not chrony's repair of REALTIME.
    let _ = guest.ssh_exec("sudo systemctl stop chronyd || true");
    guest
        .ssh_exec(
            "echo devices | sudo tee /sys/power/pm_test >/dev/null && \
             echo 1 | sudo tee /sys/module/suspend/parameters/pm_test_delay >/dev/null && \
             cat /sys/power/pm_test",
        )
        .expect("arming pm_test=devices (needs CONFIG_PM_DEBUG in the guest kernel)");

    let worker = guest.worker_pid().expect("resolving the worker pid");
    let (real0, mono0) = clocks(&guest, "before");

    assert_eq!(
        unsafe { libc::kill(worker, libc::SIGURG) },
        0,
        "SIGURG to the worker failed — is LIMINA_HOST_SLEEP_SEAM=1 reaching it?"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !worker_stopped(worker) {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker never stopped — the seam did not reach its release point"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("worker stopped at the ack release point; holding {SLEEP_GAP:?}");
    std::thread::sleep(SLEEP_GAP);
    assert_eq!(
        unsafe { libc::kill(worker, libc::SIGCONT) },
        0,
        "SIGCONT to the worker failed"
    );

    let (real1, mono1) = clocks(&guest, "after");
    let d_mono = mono1 - mono0;
    eprintln!(
        "deltas across a {SLEEP_GAP:?} host sleep: real={:+.3} mono={d_mono:+.3}",
        real1 - real0
    );

    let log = guest.supervisor_log();
    for line in log.lines() {
        if line.contains("quiesce") || line.contains("host sleep") || line.contains("host wake") {
            eprintln!("  bracket: {line}");
        }
    }
    // The premise: the devices did quiesce, so the park wait was reached.
    assert!(
        !log.contains("no device quiesce within"),
        "the devices never quiesced, so the park wait was not exercised — did pm_test apply?"
    );

    assert!(
        d_mono <= MONOTONIC_TOLERANCE_S,
        "guest CLOCK_MONOTONIC advanced {d_mono:.1}s across a {:.0}s host sleep — the suspend \
         aborted after its devices quiesced, the idle guest's parked vCPUs were taken for a \
         guest past timekeeping_suspend, and the stop was resumed keeping the counter",
        SLEEP_GAP.as_secs_f64()
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
