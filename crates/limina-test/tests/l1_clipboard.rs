// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 test of the host clipboard bridge (M5 task 3, host side).
//!
//! The harness joins the supervisor-owned control plane as a clipboard-capable peer
//! (protocol-identical to the future `limina-agent-session` helper connecting over vsock)
//! and proves both directions against a private NAMED pasteboard (`LIMINA_PASTEBOARD` —
//! the user's real clipboard is never touched):
//!
//! - guest→host: peer OFFERs → host REQUESTs → peer DATA → the pasteboard holds the text.
//! - host→guest: the test writes the pasteboard (as a macOS app would) → host detects the
//!   change-count bump and OFFERs → peer REQUESTs → host DATA carries the text.
//!
//! The L1 guest boots with the init seed agent (no clipboard cap) keeping the VM alive —
//! which also proves clipboard traffic routes only to clipboard-capable peers.
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::{Duration, Instant};

use limina_proto::{ClipData, ClipOffer, ClipRequest, Hello, Message};
use limina_test::{Guest, GuestConfig};

const TEXT_MIME: &str = "text/plain;charset=utf-8";

#[test]
fn l1_clipboard_bridges_both_directions() {
    if !limina_test::require_hvf_or_skip("l1_clipboard_bridges_both_directions") {
        return;
    }

    let pb_name = format!("limina-test-pb-{}", std::process::id());
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_control_agent()
        .with_control_socket()
        .with_supervisor_log()
        .with_env("LIMINA_PASTEBOARD", &pb_name)
        .with_env("LIMINA_CLIP_POLL_MS", "50");
    eprintln!(
        "booting L1 guest, test pasteboard {pb_name}: {:?}",
        cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    // The guest's own seed agent must be up (it keeps the VM alive and is the
    // shutdown-capable peer the orderly teardown below relies on).
    guest
        .wait_for_supervisor_log("guest agent connected: limina-init/", Duration::from_secs(15))
        .expect("supervisor never logged the seed agent handshake");
    let mut conn = guest
        .connect_control(Duration::from_secs(10))
        .expect("connecting to the control socket");

    // Handshake as a clipboard-capable agent.
    conn.send(&Message::Hello(Hello {
        agent: "limina-test-clip/0".into(),
        caps: vec!["clipboard".into()],
        pagesize: 4096,
    }))
    .expect("sending HELLO");
    let (_, welcome) = conn.recv(Duration::from_secs(5)).expect("awaiting WELCOME");
    match welcome {
        Message::Welcome(w) => assert!(
            w.caps.iter().any(|c| c == "clipboard"),
            "host WELCOME lacks the clipboard cap: {w:?}"
        ),
        other => panic!("expected WELCOME, got {other:?}"),
    }

    // --- guest→host: our offer must land on the named pasteboard. -------------------
    conn.send(&Message::ClipOffer(ClipOffer {
        serial: 1,
        mime_types: vec![TEXT_MIME.into()],
    }))
    .expect("sending CLIP_OFFER");
    match conn
        .recv(Duration::from_secs(5))
        .expect("awaiting CLIP_REQUEST")
    {
        (_, Message::ClipRequest(r)) => {
            assert_eq!(r.serial, 1, "request answers the wrong offer");
            assert_eq!(r.mime_type, TEXT_MIME);
        }
        (_, other) => panic!("expected CLIP_REQUEST, got {other:?}"),
    }
    conn.send(&Message::ClipData(ClipData {
        serial: 1,
        mime_type: TEXT_MIME.into(),
        data: b"from-guest-7531".to_vec(),
    }))
    .expect("sending CLIP_DATA");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if limina_test::pasteboard_text(&pb_name).as_deref() == Some("from-guest-7531") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pasteboard never received the guest text (currently {:?})",
            limina_test::pasteboard_text(&pb_name)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("guest→host OK: pasteboard holds the guest text");

    // --- host→guest: a local copy must be offered to us and served on request. ------
    limina_test::set_pasteboard_text(&pb_name, "from-host-8642");
    let serial = match conn
        .recv(Duration::from_secs(5))
        .expect("awaiting CLIP_OFFER")
    {
        (_, Message::ClipOffer(o)) => {
            assert!(
                o.mime_types.iter().any(|m| m == TEXT_MIME),
                "host offer lacks the text mime: {o:?}"
            );
            o.serial
        }
        (_, other) => panic!("expected CLIP_OFFER, got {other:?}"),
    };
    conn.send(&Message::ClipRequest(ClipRequest {
        serial,
        mime_type: TEXT_MIME.into(),
    }))
    .expect("sending CLIP_REQUEST");
    match conn
        .recv(Duration::from_secs(5))
        .expect("awaiting CLIP_DATA")
    {
        (_, Message::ClipData(d)) => {
            assert_eq!(d.serial, serial, "data answers the wrong request");
            assert_eq!(
                String::from_utf8_lossy(&d.data),
                "from-host-8642",
                "host data is not what the test copied"
            );
        }
        (_, other) => panic!("expected CLIP_DATA, got {other:?}"),
    }
    eprintln!("host→guest OK: offer/request/data round-trip");

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(outcome.code, Some(0), "expected orderly power-off");
}
