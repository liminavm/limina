// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 agent test: the limina-proto control plane through the shipped binaries (M5 task 1).
//!
//! Supersedes the old `l1_vsock.rs` line-protocol test: the guest agent (`limina-init`, the
//! seed of `limina-agent`) now speaks the framed control-plane protocol. One boot exercises
//! the whole first conversation:
//!
//!   HELLO (facts + caps) → WELCOME → HEARTBEAT liveness →
//!   unknown message type → ERROR(UNSUPPORTED) with the channel surviving →
//!   SHUTDOWN → SHUTDOWN_ACK → agent-driven clean power-off (worker exit 0).
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::Duration;

use limina_proto::{Message, Shutdown, Welcome, CHANNEL_CONTROL, ERR_UNSUPPORTED};
use limina_test::{Guest, GuestConfig};

const AGENT_PORT: u32 = 1234;

/// An arbitrary message type id no peer implements (robustness probe).
const BOGUS_TYPE: u8 = 200;

#[test]
fn l1_agent_handshake_heartbeat_unsupported_shutdown() {
    if !limina_test::require_hvf_or_skip("l1_agent_handshake_heartbeat_unsupported_shutdown") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_vsock(AGENT_PORT);
    eprintln!("booting L1 guest with control-plane agent: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    let mut conn = guest
        .agent_accept(Duration::from_secs(15))
        .expect("guest agent did not connect over vsock");

    // HELLO: identification, capability set, and structured boot facts.
    let (channel, msg) = conn.recv(Duration::from_secs(5)).expect("no HELLO frame");
    assert_eq!(channel, CHANNEL_CONTROL);
    let hello = match msg {
        Message::Hello(h) => h,
        other => panic!("expected HELLO first, got {other:?}"),
    };
    eprintln!("agent hello: {hello:?}");
    assert!(
        hello.agent.starts_with("limina-init/"),
        "unexpected agent id {:?}",
        hello.agent
    );
    assert!(
        hello.caps.iter().any(|c| c == "shutdown"),
        "agent does not advertise the shutdown capability: {:?}",
        hello.caps
    );
    assert!(
        hello.pagesize.is_power_of_two() && (4096..=65536).contains(&hello.pagesize),
        "implausible guest page size {}",
        hello.pagesize
    );

    conn.send(&Message::Welcome(Welcome {
        caps: vec!["shutdown".into()],
    }))
    .expect("sending WELCOME");

    // Liveness: the agent heartbeats on its own cadence.
    let (_, msg) = conn.recv(Duration::from_secs(5)).expect("no HEARTBEAT");
    match msg {
        Message::Heartbeat(hb) => eprintln!("heartbeat: {hb:?}"),
        other => panic!("expected HEARTBEAT after WELCOME, got {other:?}"),
    }

    // Robustness: an unknown message type must be answered with ERROR(UNSUPPORTED) and
    // must NOT kill the channel (heartbeats may interleave).
    conn.send(&Message::Unknown {
        msg_type: BOGUS_TYPE,
        payload: vec![0xa0], // an empty CBOR map; content is irrelevant
    })
    .expect("sending bogus-type frame");
    let err = loop {
        let (_, msg) = conn
            .recv(Duration::from_secs(5))
            .expect("no ERROR reply for the unknown message type");
        match msg {
            Message::Error(e) => break e,
            Message::Heartbeat(_) => continue,
            other => panic!("expected ERROR (or interleaved HEARTBEAT), got {other:?}"),
        }
    };
    assert_eq!(err.code, ERR_UNSUPPORTED, "wrong error code: {err:?}");
    assert_eq!(err.ref_type, BOGUS_TYPE, "wrong offender type: {err:?}");

    // Orderly shutdown: SHUTDOWN → SHUTDOWN_ACK → the agent powers the guest off.
    conn.send(&Message::Shutdown(Shutdown { grace_ms: 5000 }))
        .expect("sending SHUTDOWN");
    loop {
        let (_, msg) = conn.recv(Duration::from_secs(5)).expect("no SHUTDOWN_ACK");
        match msg {
            Message::ShutdownAck => break,
            Message::Heartbeat(_) => continue,
            other => panic!("expected SHUTDOWN_ACK (or HEARTBEAT), got {other:?}"),
        }
    }

    // Clean, agent-driven power-off → worker exit 0 (PSCI), supervisor not forced.
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(
        outcome.code,
        Some(0),
        "expected clean power-off, got {outcome:?}"
    );
}
