// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 — **the worker runs outside the supervisor's process tree.**
//!
//! macOS Game Mode clamps every process in an app's process tree to priority 4 while a game is
//! fullscreen: timers ~100 ms late, CPU denied, CoreAudio callbacks stalled up to a second. The
//! supervisor is an app, so a worker it posix_spawns is clamped with it. A process that launchd
//! starts outside any app is left alone (`spikes/game-mode-throttle/RESULTS.md`), so the
//! supervisor has launchd start a `limina-vmm --launcher` job, hands it every channel over Mach,
//! and the launcher spawns the worker.
//!
//! Game Mode itself cannot be turned on from a test; the lineage is what decides it, and that is
//! what this asserts. The vehicle is the L1 guest parked in `limina.hold`.
//!
//! Oracles:
//! 1. The supervisor is not an ancestor of the worker; the worker's parent is a launcher job
//!    whose parent is launchd.
//! 2. The guest's console still reaches the harness (the `hold:` wait), so the worker's
//!    inherited streams crossed over with it.
//! 3. A SIGKILLed supervisor takes the worker with it: once launchd is the parent, nothing else
//!    would ever end a VM that has lost its window.
//!
//! `LIMINA_WORKER_LAUNCH=spawn` keeps the old posix_spawn path; the second test pins that it
//! still boots with the worker as the supervisor's child.

use limina_test::{Guest, GuestConfig, parent_pid, proc_argv};
use std::time::{Duration, Instant};

fn ancestors(mut pid: libc::pid_t) -> Vec<libc::pid_t> {
    let mut chain = Vec::new();
    while let Some(parent) = parent_pid(pid) {
        chain.push(parent);
        if parent <= 1 {
            break;
        }
        pid = parent;
    }
    chain
}

#[test]
fn l1_the_worker_runs_outside_the_supervisor_tree() {
    if !limina_test::require_hvf_or_skip("l1_the_worker_runs_outside_the_supervisor_tree") {
        return;
    }
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_cmdline_token("limina.hold")
        .with_supervisor_log();

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    // --- Oracle 2: the console crossed over with the worker ---
    guest
        .wait_for("hold:", Duration::from_secs(30))
        .expect("guest never parked in limina.hold");

    // --- Oracle 1: lineage ---
    let supervisor = guest.supervisor_pid();
    let worker = guest.worker_pid().expect("finding the worker");
    let chain = ancestors(worker);
    assert!(
        !chain.contains(&supervisor),
        "worker {worker} descends from supervisor {supervisor} (ancestors {chain:?}), so Game \
         Mode clamps it with the app"
    );
    let launcher = chain[0];
    let argv = proc_argv(launcher).unwrap_or_default();
    assert_eq!(
        argv.get(1).map(String::as_str),
        Some("--launcher"),
        "the worker's parent {launcher} is not a launcher job (argv {argv:?})"
    );
    assert_eq!(
        chain.get(1),
        Some(&1),
        "the launcher {launcher} is not launchd's child (ancestors {chain:?})"
    );

    // --- Oracle 3: the supervisor's death ends the VM ---
    unsafe { libc::kill(supervisor, libc::SIGKILL) };
    let t0 = Instant::now();
    while unsafe { libc::kill(worker, 0) } == 0 && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let alive = unsafe { libc::kill(worker, 0) } == 0;
    if alive {
        unsafe { libc::kill(worker, libc::SIGKILL) };
    }
    assert!(
        !alive,
        "worker {worker} outlived its SIGKILLed supervisor by 10 s: an orphaned, windowless VM"
    );
}

#[test]
fn l1_the_spawn_fallback_still_boots() {
    if !limina_test::require_hvf_or_skip("l1_the_spawn_fallback_still_boots") {
        return;
    }
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_cmdline_token("limina.hold")
        .with_env("LIMINA_WORKER_LAUNCH", "spawn");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for("hold:", Duration::from_secs(30))
        .expect("guest never parked in limina.hold");
    let worker = guest.worker_pid().expect("finding the worker");
    assert_eq!(
        parent_pid(worker),
        Some(guest.supervisor_pid()),
        "LIMINA_WORKER_LAUNCH=spawn did not keep the worker as the supervisor's child"
    );
}
