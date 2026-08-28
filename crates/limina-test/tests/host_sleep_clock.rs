// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host-sleep bracket must not let the guest absorb host sleep into `CLOCK_MONOTONIC`.
//!
//! When the host stops our vCPUs while the guest kernel still believes it is running, the
//! guest's counter keeps advancing (measured: `spikes/s2idle-monotonic/`) and the elapsed
//! time lands in `CLOCK_MONOTONIC`. Nothing can reclaim it afterwards — sleeptime
//! injection moves only REALTIME and BOOTTIME, by construction
//! (`__timekeeping_inject_sleeptime`). On a guest that arms systemd's service watchdogs
//! (Debian: 3 min on journald/udevd/logind) a gap past the watchdog kills logind, which
//! orphans the DRM and input leases and takes the seated session with it.
//!
//! The bracket exists to put the stop INSIDE the interval the kernel classifies as
//! suspend. This test pins that: across a simulated host sleep, REALTIME must absorb the
//! gap and MONOTONIC must not.
//!
//! IOKit cannot drive this in CI — sleeping the host kills the session running the test —
//! so `LIMINA_HOST_SLEEP_SEAM=1` + `SIGURG` runs the real `willSleep` release decision and
//! then `SIGSTOP`s the worker at exactly the moment the ack is released. That is the worst
//! case macOS is entitled to, and it makes the race deterministic: the rendezvous the
//! guest still owes cannot complete while its vCPUs are frozen.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// Simulated host sleep. Long enough to be unambiguous against scheduling noise, short
/// enough to keep the test cheap. `LIMINA_HOST_SLEEP_GAP_S` overrides it — the check that
/// matters is that MONOTONIC does not grow with this: a guest stopped inside its suspend
/// window sees the same few seconds of its own running time whether the host slept for 30
/// seconds or overnight.
fn sleep_gap() -> Duration {
    std::env::var("LIMINA_HOST_SLEEP_GAP_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(90))
}

/// MONOTONIC may legitimately advance by the guest's own running time either side of the
/// stop — the suspend it completes, the resume, and the SSH round trips.
///
/// Measured, not guessed: across gaps of 30 s and 180 s the guest's monotonic advanced
/// 16.06 s and 16.07 s. It is invariant to the gap, which is the whole property under test,
/// so the bound is set from that running time plus headroom rather than from the gap. The
/// pre-fix reading for the default 90 s gap is ~106 s, so this discriminates by 3x.
const MONOTONIC_TOLERANCE_S: f64 = 30.0;

/// Read REALTIME / MONOTONIC / BOOTTIME in one round trip.
fn clocks(guest: &Guest, when: &str) -> (f64, f64, f64) {
    let out = guest
        .ssh_poll(
            "python3 -c 'import time; print(time.clock_gettime(time.CLOCK_REALTIME), \
             time.clock_gettime(time.CLOCK_MONOTONIC), time.clock_gettime(time.CLOCK_BOOTTIME))'",
            Duration::from_secs(90),
        )
        .unwrap_or_else(|e| panic!("reading the guest clocks {when}: {e}"));
    let v: Vec<f64> = out
        .split_whitespace()
        .map(|f| f.parse().expect("parsing a guest clock"))
        .collect();
    assert_eq!(v.len(), 3, "expected three clocks {when}, got {out:?}");
    (v[0], v[1], v[2])
}

/// True once the worker is stopped (`T`) — the ack has been released and our stand-in for
/// macOS has cut the vCPUs.
fn worker_stopped(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with('T'))
        .unwrap_or(false)
}

#[test]
fn host_sleep_is_not_absorbed_into_guest_monotonic() {
    if !limina_test::require_hvf_or_skip("host_sleep_is_not_absorbed_into_guest_monotonic") {
        return;
    }

    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_supervisor_log()
            .with_env("LIMINA_HOST_SLEEP_SEAM", "1")
            // The bracket's own account of how far the guest got is the first thing to
            // read when this test fails.
            .with_env("RUST_LOG", "limina_vmm=info,limina=info"),
        Err(e) => {
            eprintln!("SKIPPED host_sleep_is_not_absorbed_into_guest_monotonic: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable");
    eprintln!("guest SSH up: {banner}");

    // The clocks must be judged on the kernel's own suspend accounting, not on chrony
    // having repaired REALTIME afterwards (and chrony can only ever repair REALTIME).
    let _ = guest.ssh_exec("sudo systemctl stop chronyd || true");

    let boot_id = guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading the pre-sleep boot_id")
        .trim()
        .to_string();
    let worker = guest.worker_pid().expect("resolving the worker pid");
    let sleep_gap = sleep_gap();
    let (real0, mono0, boot0) = clocks(&guest, "before");
    eprintln!("pre-sleep: worker={worker} real={real0:.3} mono={mono0:.3} boot={boot0:.3}");

    // Drive the real willSleep release decision; the seam stops the worker at the ack.
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
    eprintln!("worker stopped at the ack release point; holding {sleep_gap:?}");

    std::thread::sleep(sleep_gap);
    assert_eq!(
        unsafe { libc::kill(worker, libc::SIGCONT) },
        0,
        "SIGCONT to the worker failed"
    );
    eprintln!("worker continued; the seam will pulse the wake key");

    guest
        .ssh_poll("true", Duration::from_secs(120))
        .expect("guest never came back on SSH after the simulated host sleep");

    let boot_id_after = guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading the post-sleep boot_id")
        .trim()
        .to_string();
    assert_eq!(
        boot_id_after, boot_id,
        "boot_id changed across the simulated host sleep — the guest rebooted"
    );

    for line in guest.supervisor_log().lines() {
        if line.contains("quiesce:") || line.contains("host sleep") || line.contains("host wake") {
            eprintln!("  bracket: {line}");
        }
    }

    let (real1, mono1, boot1) = clocks(&guest, "after");
    let (d_real, d_mono, d_boot) = (real1 - real0, mono1 - mono0, boot1 - boot0);
    eprintln!("deltas across a {sleep_gap:?} host sleep: real={d_real:+.3} mono={d_mono:+.3} boot={d_boot:+.3}");

    let gap = sleep_gap.as_secs_f64();

    // REALTIME is deliberately NOT asserted here. It holds only on the path where the guest
    // reached `Parked` and injected the sleep itself; on the backstop path the guest's
    // counter never saw the gap, so its wall clock comes back behind and moves only when an
    // external corrector (chrony, agent TimeSync, qga `guest-set-time`) runs — which is
    // exactly what was observed: +45.97 s after a 30 s gap (chrony had stepped it) but
    // +16.07 s after a 180 s gap (nothing had). That correction belongs to its own test.
    eprintln!("  (REALTIME moved {d_real:+.1}s; not asserted — see the comment)");

    // The invariant. MONOTONIC excludes suspend by definition, so a guest that was stopped
    // inside its suspend window sees ~none of the gap here. A guest stopped outside it
    // sees all of it — and that is the damage no later correction can undo.
    assert!(
        d_mono <= MONOTONIC_TOLERANCE_S,
        "guest CLOCK_MONOTONIC advanced {d_mono:.1}s across a {gap:.0}s host sleep — the \
         host stopped the vCPUs before the guest reached timekeeping_suspend, so the \
         sleep was absorbed as running time. Every systemd WatchdogSec shorter than that \
         has now expired; on a guest that arms them, logind dies here."
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("shutting down the woken guest");
    eprintln!("teardown outcome: {outcome:?}");
}
