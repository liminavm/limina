// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 vsock test: structured host<->guest assertions over a vsock channel.
//!
//! Where `l1_boot.rs` scrapes the console for a marker, this drives a real **vsock**
//! conversation with the guest agent (our `limina-init`): the guest connects to the host,
//! reports structured facts (`READY pagesize=<N>`), we assert on them and reply, then it
//! powers off cleanly. This is the foundation for richer in-guest test assertions and
//! the seed of the limina-agent control plane (D8).
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const AGENT_PORT: u32 = 1234;

#[test]
fn l1_guest_agent_reports_over_vsock() {
    if !limina_test::require_hvf_or_skip("l1_guest_agent_reports_over_vsock") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_vsock(AGENT_PORT);
    eprintln!("booting L1 guest with vsock agent: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // The guest agent connects to the host shortly after boot.
    let mut conn = guest
        .vsock_accept(Duration::from_secs(15))
        .expect("guest agent did not connect over vsock");

    // It reports structured facts; assert on them (not console text).
    let ready = conn
        .read_line(Duration::from_secs(5))
        .expect("no READY line from agent");
    eprintln!("agent said: {ready:?}");
    assert!(
        ready.starts_with("READY "),
        "expected a READY line, got {ready:?}"
    );

    // Parse the guest page size the agent reported (sanity: a power-of-two page size).
    let pagesize: u64 = ready
        .split_whitespace()
        .find_map(|t| t.strip_prefix("pagesize="))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no pagesize in {ready:?}"));
    eprintln!("guest page size: {pagesize}");
    assert!(
        pagesize.is_power_of_two() && (4096..=65536).contains(&pagesize),
        "implausible guest page size {pagesize}"
    );

    // Reply — the agent powers off once it hears from us.
    conn.write_line("POWEROFF").expect("sending POWEROFF");

    // Clean, agent-driven power-off → worker exit 0.
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(outcome.code, Some(0), "expected clean power-off, got {outcome:?}");
}
