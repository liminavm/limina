// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 orderly-shutdown test: the SUPERVISOR-owned control plane (M5 task 1, round 2).
//!
//! Where `l1_agent.rs` has the harness speak the protocol (limina passes the vsock plumbing
//! through), this exercises the product path: limina itself binds the control socket at the
//! well-known port, handshakes the agent, and — the payoff — turns SIGTERM (and
//! window-close, same ladder) into an **orderly guest power-off** via SHUTDOWN instead of
//! the GPIO power button the guest would ignore.
//!
//! The guest stays alive precisely because the agent is serving the channel; the
//! supervisor's SHUTDOWN is what ends it. Without the control plane this exact flow ends
//! in a forced SIGKILL after the 20s grace (the old behavior), so the asserts below
//! (exit 0, not forced, within seconds) prove the orderly path end-to-end.
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

#[test]
fn l1_sigterm_powers_guest_off_via_agent() {
    if !limina_test::require_hvf_or_skip("l1_sigterm_powers_guest_off_via_agent") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_control_agent()
        .with_supervisor_log();
    eprintln!(
        "booting L1 guest against the supervisor control plane: {:?}",
        cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // The supervisor's control plane must complete the HELLO/WELCOME handshake (this also
    // asserts the structured facts made it across — pagesize is in the log line).
    guest
        .wait_for_supervisor_log(
            "guest agent connected: limina-init/",
            Duration::from_secs(15),
        )
        .expect("supervisor never logged the agent handshake");
    assert!(
        guest.supervisor_log().contains("pagesize="),
        "handshake log line lacks the pagesize fact:\n{}",
        guest.supervisor_log()
    );

    // SIGTERM the supervisor (what Guest::shutdown sends): the orderly ladder must end in
    // an agent-driven clean power-off — exit 0, not forced, and FAST (the fallback path
    // would burn the 5s agent grace + power-button grace and end in SIGKILL).
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
        "expected an orderly agent-driven power-off, got {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "orderly shutdown took {elapsed:?} — smells like a fallback path, not the agent"
    );
}
