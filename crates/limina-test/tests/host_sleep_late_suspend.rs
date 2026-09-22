// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A guest that finishes OUR suspend only after the host has woken must still be woken.
//!
//! `willSleep` pulses the guest's sleep button and holds the host's sleep ack for a bounded
//! budget. A guest slower than that budget is paused wherever it got to and resumed on
//! `didWake` — and then carries on with the suspend we asked for. If `didWake` judged "was
//! this sleep ours, and is the guest asleep?" once, at the instant of unpausing, the answer is
//! "not asleep yet": the guest is left alone, completes the suspend moments later, and parks
//! in PSCI `SYSTEM_SUSPEND` with nothing left to wake it. The window keeps its last frame and
//! the VM never comes back. That is what a tester's stock Fedora guest did on a real host
//! wake.
//!
//! The slow suspend is made deterministic with a guest-side delay on `systemd-suspend.service`
//! (a runtime drop-in under `/run`, so the disk is untouched) that outlasts the bracket's
//! device budget. The host sleep itself is the `LIMINA_HOST_SLEEP_SEAM` stand-in, which runs
//! the same `willSleep`/`didWake` code the IOKit handler does.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// Longer than the bracket's device budget (`power.rs` `DEVICE_WAIT`, 15 s), so the host
/// "sleeps" while the guest is still on its way into suspend.
const SUSPEND_DELAY_S: u64 = 25;

/// How long the host stays asleep. The guest's remaining delay runs only after the wake.
const SLEEP_GAP: Duration = Duration::from_secs(5);

/// True once the worker is stopped (`T`) — the seam released the ack and cut the vCPUs.
fn worker_stopped(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with('T'))
        .unwrap_or(false)
}

#[test]
fn a_suspend_completed_after_host_wake_is_woken() {
    if !limina_test::require_hvf_or_skip("a_suspend_completed_after_host_wake_is_woken") {
        return;
    }

    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_supervisor_log()
            .with_env("LIMINA_HOST_SLEEP_SEAM", "1")
            .with_env("RUST_LOG", "warn,limina_vmm=info,limina=info,krun_vmm=info"),
        Err(e) => {
            eprintln!("SKIPPED a_suspend_completed_after_host_wake_is_woken: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable");
    eprintln!("guest SSH up: {banner}");

    guest
        .ssh_exec(&format!(
            "sudo mkdir -p /run/systemd/system/systemd-suspend.service.d && \
             printf '[Service]\\nExecStartPre=/usr/bin/sleep {SUSPEND_DELAY_S}\\n' | \
             sudo tee /run/systemd/system/systemd-suspend.service.d/limina-slow.conf >/dev/null && \
             sudo systemctl daemon-reload"
        ))
        .expect("installing the slow-suspend drop-in");

    let boot_id = guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading the pre-sleep boot_id")
        .trim()
        .to_string();
    let worker = guest.worker_pid().expect("resolving the worker pid");

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

    let back = guest.ssh_poll("true", Duration::from_secs(120));

    let log = guest.supervisor_log();
    for line in log.lines() {
        if line.contains("quiesce")
            || line.contains("host sleep")
            || line.contains("host wake")
            || line.contains("SYSTEM_SUSPEND")
        {
            eprintln!("  bracket: {line}");
        }
    }
    // The premise: the host must have gone to sleep BEFORE the guest finished suspending,
    // or this run exercised the ordinary path and proves nothing.
    assert!(
        log.contains("guest reached only No") || log.contains("guest reached only DevicesOnly"),
        "the guest finished suspending inside the bracket's budget, so the late path was not \
         exercised — did the slow-suspend drop-in apply?"
    );

    back.expect(
        "the guest never came back after the simulated host wake — it completed the suspend \
         we asked for after didWake had already decided it was not ours to wake",
    );

    let boot_id_after = guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading the post-wake boot_id")
        .trim()
        .to_string();
    assert_eq!(
        boot_id_after, boot_id,
        "the guest rebooted instead of resuming"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("shutting down the woken guest");
    eprintln!("teardown outcome: {outcome:?}");
}
