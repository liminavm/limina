// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 — **an ordinary stop never kills a running guest.**
//!
//! Every rung of the shutdown ladder is a *request*: the control-plane agent, the GPIO power
//! button, the stock `qemu-guest-agent`. A guest may ignore all of them — it may be mid-write,
//! showing an unsaved-work dialog, or simply not listening. Until 2026-08-26 the supervisor
//! SIGKILLed the worker when the grace ran out, which turns "I closed the window" into lost
//! work the user never agreed to risk. Ending a guest that will not stop is now an explicit
//! human act: a second stop signal (double Ctrl-C, `limina stop --force`) or Force Stop in the
//! window's menu.
//!
//! The vehicle is the L1 guest parked in `limina.hold` with **no agent**: `limina-init` sits in
//! `animate_forever`, so nothing answers a shutdown request over the control plane, nothing acts
//! on the GPIO power button, and no `qemu-guest-agent` exists — exactly the "ignores everything"
//! case, without having to break a real guest.
//!
//! Oracles:
//! 1. Past the grace the supervisor **says** the guest is still running, rather than killing it.
//! 2. The worker is still alive after that — the assertion that would have failed before.
//! 3. A second stop signal ends it, so the escape hatch still works.

use limina_test::{Guest, GuestConfig};
use std::time::{Duration, Instant};

#[test]
fn l1_an_ordinary_stop_waits_for_a_guest_that_ignores_it() {
    if !limina_test::require_hvf_or_skip("l1_an_ordinary_stop_waits_for_a_guest_that_ignores_it") {
        return;
    }
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_cmdline_token("limina.hold")
        .with_supervisor_log();
    let grace = cfg.shutdown_grace;

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for("hold:", Duration::from_secs(30))
        .expect("guest never parked in limina.hold");

    // --- Oracle 1: the stop is a request, and the supervisor says so ---
    unsafe { libc::kill(guest.supervisor_pid(), libc::SIGTERM) };
    guest
        .wait_for_supervisor_log("has not powered off", grace + Duration::from_secs(20))
        .unwrap_or_else(|e| {
            panic!(
                "the supervisor never reported the guest as still running ({e}). If it exited \
                 instead, an ordinary stop killed a guest that merely ignored the request — the \
                 thing this test exists to prevent. Log tail:\n{}",
                tail(&guest)
            )
        });

    // --- Oracle 2: …and the VM is still there ---
    let worker = guest.worker_pid().unwrap_or_else(|e| {
        panic!(
            "the worker is gone after an ordinary stop ({e}):\n{}",
            tail(&guest)
        )
    });
    assert!(
        unsafe { libc::kill(worker, 0) } == 0,
        "worker {worker} is not alive after an ordinary stop:\n{}",
        tail(&guest)
    );

    // --- Oracle 3: the escape hatch still ends it ---
    let t0 = Instant::now();
    unsafe { libc::kill(guest.supervisor_pid(), libc::SIGTERM) };
    let outcome = guest
        .wait_for_exit(Duration::from_secs(30))
        .expect("waiting for the forced stop");
    assert!(
        !outcome.forced,
        "the second stop signal did not end the VM; the harness had to ({outcome:?})"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(20),
        "the forced stop took {:?} — force is supposed to skip every remaining grace",
        t0.elapsed()
    );
}

/// The supervisor log's shutdown-ladder lines, for a failure message.
fn tail(guest: &Guest) -> String {
    let log = guest.supervisor_log();
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| {
            l.contains("shutdown") || l.contains("power") || l.contains("stop") || l.contains("qga")
        })
        .collect();
    if lines.is_empty() {
        "(the supervisor logged nothing about stopping)".to_string()
    } else {
        lines.join("\n")
    }
}
