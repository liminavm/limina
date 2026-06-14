// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M5 heartbeat-liveness surfacing: the control plane tracks per-peer heartbeats and
//! logs when an agent goes silent (and when it recovers) — the supervisor-side signal
//! a status UI consumes later.
//!
//! Vehicle: the L1 guest's seed agent heartbeats normally (and must never be reported
//! silent), while the harness joins the plane as a second peer that deliberately stops
//! talking after HELLO. The supervisor must call out exactly the mute peer, then notice
//! its recovery when a heartbeat finally arrives. `LIMINA_AGENT_SILENT_SECS=1` shortens
//! the threshold so the test stays fast.

use std::time::Duration;

use limina_proto::{Heartbeat, Hello, Message};
use limina_test::{Guest, GuestConfig};

#[test]
fn l1_silent_agent_is_reported_and_recovers() {
    if !limina_test::require_hvf_or_skip("l1_silent_agent_is_reported_and_recovers") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_control_agent()
        .with_control_socket()
        .with_supervisor_log()
        .with_env("LIMINA_AGENT_SILENT_SECS", "1");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // The guest's seed agent is up and heartbeating (it also keeps the VM alive).
    guest
        .wait_for_supervisor_log(
            "guest agent connected: limina-init/",
            Duration::from_secs(15),
        )
        .expect("supervisor never logged the seed agent handshake");

    // Join as a peer that goes mute right after the handshake.
    let mut conn = guest
        .connect_control(Duration::from_secs(10))
        .expect("connecting to the control socket");
    conn.send(&Message::Hello(Hello {
        agent: "limina-test-mute/0".into(),
        caps: vec![],
        pagesize: 4096,
    }))
    .expect("sending HELLO");
    match conn.recv(Duration::from_secs(5)).expect("awaiting WELCOME") {
        (_, Message::Welcome(_)) => {}
        other => panic!("expected WELCOME, got {other:?}"),
    }

    // Silence is noticed, and it names the mute peer specifically.
    guest
        .wait_for_supervisor_log("agent limina-test-mute/0 silent", Duration::from_secs(10))
        .expect("supervisor never reported the mute agent silent");

    // Resuming heartbeats brings it back. Beat continuously like a real recovering
    // agent — a single beat only moves last_seen once, and with the threshold equal to
    // the sweep interval the next sweep can land just past it again (a phase race this
    // test lost on its first run).
    let recovered = (0..50).any(|seq| {
        conn.send(&Message::Heartbeat(Heartbeat { seq }))
            .expect("sending a recovery heartbeat");
        std::thread::sleep(Duration::from_millis(200));
        guest
            .supervisor_log()
            .contains("agent limina-test-mute/0 heartbeating again")
    });
    assert!(
        recovered,
        "supervisor never reported the mute agent recovered:\n{}",
        guest.supervisor_log()
    );

    // The healthy seed agent must never have been reported silent.
    let log = guest.supervisor_log();
    assert!(
        !log.contains("agent limina-init/0.1.0 silent"),
        "the heartbeating seed agent was wrongly reported silent:\n{log}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
