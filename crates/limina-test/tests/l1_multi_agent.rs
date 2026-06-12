// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 test: the control plane accepts MULTIPLE concurrent guest connections (M5).
//!
//! The clipboard spike settled the agent topology: a root `limina-agent` daemon plus
//! per-session user helpers, each holding its own vsock connection to the same
//! well-known port. So the host must serve N concurrent peers with per-connection
//! HELLO/caps, and SHUTDOWN must go to every shutdown-capable peer.
//!
//! Vehicle: limina-init runs its built-in agent seed (`limina.agent_port=`) *and* spawns
//! the product `limina-agent` binary (`limina.real_agent`) — two distinct agents
//! connected at once. The supervisor must log BOTH handshakes, and a shutdown must
//! be orderly (either agent powering the guest off counts).
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

#[test]
fn l1_control_plane_serves_two_agents_concurrently() {
    if !limina_test::require_hvf_or_skip("l1_control_plane_serves_two_agents_concurrently") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_control_agent()
        .with_cmdline_token("limina.real_agent")
        .with_supervisor_log();
    eprintln!("booting L1 guest with seed + real agent: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Both agents must complete HELLO/WELCOME *concurrently* — the seed blocks init
    // for the guest's lifetime, so the only way both lines appear is two live
    // connections served in parallel.
    guest
        .wait_for_supervisor_log("guest agent connected: limina-init/", Duration::from_secs(15))
        .expect("supervisor never logged the seed agent handshake");
    guest
        .wait_for_supervisor_log(
            "guest agent connected: limina-agent/",
            Duration::from_secs(15),
        )
        .expect("supervisor never logged the real limina-agent handshake");

    // Orderly shutdown: SHUTDOWN goes to every shutdown-capable peer; whichever
    // powers off first wins (both paths are idempotent PSCI power-off).
    let start = Instant::now();
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    let elapsed = start.elapsed();
    eprintln!("teardown outcome: {outcome:?} in {elapsed:?}");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(outcome.code, Some(0), "expected orderly power-off");
    assert!(
        elapsed < Duration::from_secs(5),
        "orderly shutdown took {elapsed:?} — smells like a fallback path"
    );
}
