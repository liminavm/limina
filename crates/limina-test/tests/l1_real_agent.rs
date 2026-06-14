// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 test of the PRODUCT guest agent binary (`guest/limina-agent`), end-to-end.
//!
//! `l1_shutdown.rs` proves the supervisor-owned control plane against limina-init's
//! built-in agent seed; this boots the **real `limina-agent` daemon** (staged at
//! `/limina-agent` in the L1 rootfs, spawned by init on `limina.real_agent`) and asserts the
//! same orderly ladder: it connects at the well-known CONTROL_PORT with **no
//! configuration** (no cmdline port token — the daemon's default), handshakes, and on
//! SIGTERM powers the guest off itself (raw `reboot(2)` here; `systemctl poweroff` on a
//! real distro). `limina.hold` keeps init alive so the *agent* is provably what ends the VM.
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

#[test]
fn l1_real_agent_binary_handshakes_and_powers_off() {
    if !limina_test::require_hvf_or_skip("l1_real_agent_binary_handshakes_and_powers_off") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_cmdline_token("limina.real_agent")
        .with_cmdline_token("limina.hold")
        .with_supervisor_log();
    eprintln!(
        "booting L1 guest with the real limina-agent: {:?}",
        cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // The PRODUCT daemon must identify itself — proving the staged binary (not init's
    // seed) is what connected, on the default well-known port.
    guest
        .wait_for_supervisor_log(
            "guest agent connected: limina-agent/",
            Duration::from_secs(15),
        )
        .expect("supervisor never logged the limina-agent handshake");

    // SIGTERM → SHUTDOWN → the daemon powers the guest off (init is parked in limina.hold,
    // so a clean exit can only come from the agent).
    let start = Instant::now();
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    let elapsed = start.elapsed();
    eprintln!("teardown outcome: {outcome:?} in {elapsed:?}");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(
        outcome.code,
        Some(0),
        "expected the agent-driven power-off, got {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "orderly shutdown took {elapsed:?} — smells like a fallback path, not the agent"
    );
}
