// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M6 PSI autoballoon: the supervisor's policy turns guest memory-pressure reports into balloon
//! target changes, end-to-end.
//!
//! Gates M6 Step 4 — the `limina_proto::MemPressure` message, the supervisor `BalloonPolicy`
//! (hysteresis), and the wiring through the control plane to the balloon control socket. The stock
//! F44 baseline has the balloon *driver* but not `limina-agent`, so the harness **plays the agent**:
//! it joins the control plane as a peer and injects synthetic `MemPressure`, while the real guest
//! balloon responds to the targets the policy commands. (The agent's own `/proc` reporting is
//! covered by the host-side parser test `MemPressure::from_proc` in `limina-proto`.)
//!
//! - Inject *idle + plenty free* pressure → the policy inflates (reclaim toward the floor) → the
//!   guest balloon's `actual` rises.
//! - Inject *high* pressure → the policy deflates to 0 → `actual` returns to ~0.
//!
//! SKIPs cleanly without `LIMINA_HVF_TESTS`, the GOP firmware, or the baseline disk. Heavy: a full
//! stock boot to sshd.

use std::time::{Duration, Instant};

use limina_proto::{Hello, MemPressure, Message};
use limina_test::{AgentConn, Guest, GuestConfig};

const MIB: u64 = 1 << 20;
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;
/// Under injected idle pressure the policy must inflate the guest balloon to at least this.
const INFLATE_FLOOR: u64 = 1500 * MIB;
/// Under injected high pressure the policy must deflate the balloon back to at most this.
const DEFLATE_CEIL: u64 = 300 * MIB;

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

/// An idle, memory-rich report: `some` pressure 0, ~70% available → the policy reclaims.
fn idle_report() -> MemPressure {
    let total = (MAX_MIB as u64) * 1024; // KiB
    MemPressure {
        some_avg10: 0,
        some_avg60: 0,
        full_avg10: 0,
        full_avg60: 0,
        mem_available_kib: total * 70 / 100,
        mem_total_kib: total,
    }
}

/// A high-pressure report: `some` 50% → the policy releases (deflate to 0).
fn pressure_report() -> MemPressure {
    let total = (MAX_MIB as u64) * 1024;
    MemPressure {
        some_avg10: 5000,
        some_avg60: 5000,
        full_avg10: 2000,
        full_avg60: 2000,
        mem_available_kib: total * 5 / 100,
        mem_total_kib: total,
    }
}

/// Inject `report` once per ~1.2 s (above the policy's 800 ms inflate dwell) while polling the
/// balloon's `actual` until `pred` holds or the timeout elapses. Returns the last `actual` (bytes).
fn drive(
    guest: &Guest,
    conn: &mut AgentConn,
    report: MemPressure,
    timeout: Duration,
    pred: impl Fn(u64) -> bool,
) -> u64 {
    let deadline = Instant::now() + timeout;
    loop {
        conn.send(&Message::MemPressure(report))
            .expect("sending MemPressure to the control plane");
        std::thread::sleep(Duration::from_millis(1200));
        let (actual, _) = guest.balloon_stats().expect("reading balloon stats");
        eprintln!(
            "  injected some_avg10={} → balloon actual={} MiB",
            report.some_avg10,
            mib(actual)
        );
        if pred(actual) || Instant::now() >= deadline {
            return actual;
        }
    }
}

#[test]
fn psi_pressure_drives_the_balloon_target() {
    if !limina_test::require_hvf_or_skip("psi_pressure_drives_the_balloon_target") {
        return;
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_memory(MIN_MIB, MAX_MIB)
            .with_balloon_control()
            .with_control_socket(),
        Err(e) => {
            eprintln!("SKIPPED psi_pressure_drives_the_balloon_target: {e}");
            return;
        }
    };
    eprintln!("booting stock 4 KiB F44 baseline ({MIN_MIB}..{MAX_MIB} MiB, PSI autoballoon)");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // Join the control plane as a peer (standing in for the guest agent) and handshake.
    let mut conn = guest
        .connect_control(Duration::from_secs(30))
        .expect("connecting to the control plane");
    conn.send(&Message::Hello(Hello {
        agent: "limina-test-psi/0".to_string(),
        caps: vec!["mempressure".to_string()],
        pagesize: 4096,
    }))
    .expect("sending Hello");
    match conn.recv(Duration::from_secs(10)) {
        Ok((_, Message::Welcome(_))) => {}
        other => panic!("expected Welcome from the control plane, got {other:?}"),
    }

    // INFLATE: idle + memory-rich reports → the policy reclaims toward the floor.
    eprintln!("injecting idle pressure (expect the policy to inflate the balloon)");
    let inflated = drive(
        &guest,
        &mut conn,
        idle_report(),
        Duration::from_secs(60),
        |a| a >= INFLATE_FLOOR,
    );
    assert!(
        inflated >= INFLATE_FLOOR,
        "the PSI policy did not inflate the balloon under idle pressure: actual={} MiB, want >= {} MiB. \
         (MemPressure -> BalloonPolicy -> balloon socket -> guest inflate is broken.)",
        mib(inflated),
        mib(INFLATE_FLOOR)
    );

    // DEFLATE: high pressure → the policy releases the balloon back to 0.
    eprintln!("injecting high pressure (expect the policy to deflate the balloon)");
    let deflated = drive(
        &guest,
        &mut conn,
        pressure_report(),
        Duration::from_secs(45),
        |a| a <= DEFLATE_CEIL,
    );
    assert!(
        deflated <= DEFLATE_CEIL,
        "the PSI policy did not release the balloon under high pressure: actual={} MiB, want <= {} MiB",
        mib(deflated),
        mib(DEFLATE_CEIL)
    );
    eprintln!(
        "PSI autoballoon loop confirmed: idle→inflate {} MiB, pressure→deflate {} MiB",
        mib(inflated),
        mib(deflated)
    );

    let outcome = guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
