// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 regression test: TWO clipboard peers, each with its own serial namespace.
//!
//! Real guests run one `limina-agent-session` per graphical session (dogfood-guest was
//! running three at once: a GNOME session, a niri session, and — unintentionally — one in
//! the gdm greeter). Each helper is a separate process whose offer serial starts at 0 and
//! counts *its own* selection changes, so serials from different peers are unrelated
//! numbers.
//!
//! The host used to ratchet a **single** `guest_serial` across all peers and drop any
//! offer below the high-water mark. A long-lived session therefore pushed the ratchet up
//! and permanently silenced every other session: a freshly started helper offering serial
//! 1, 2, 3… was below the mark, so the host never even sent a REQUEST and that session's
//! clipboard silently never reached the host. (Observed on dogfood-guest 2026-07-31: the
//! niri session's copies went nowhere while host→guest still worked — host→guest uses the
//! host's own single counter, so only the merged direction broke.)
//!
//! Here peer A copies with high serials, then peer B — a distinct session, serials from 1
//! — must still be served. Build the guest first: `scripts/build-test-guest.sh`.
//! Gated behind LIMINA_HVF_TESTS.

use std::time::{Duration, Instant};

use limina_proto::{ClipData, ClipOffer, Hello, Message};
use limina_test::{AgentConn, Guest, GuestConfig};

const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Join the control plane as another clipboard-capable peer (one per guest session).
fn join_as_clipboard_peer(guest: &mut Guest, name: &str) -> AgentConn {
    let mut conn = guest
        .connect_control(Duration::from_secs(10))
        .expect("connecting to the control socket");
    conn.send(&Message::Hello(Hello {
        agent: name.into(),
        caps: vec!["clipboard".into()],
        pagesize: 4096,
    }))
    .expect("sending HELLO");
    match conn.recv(Duration::from_secs(5)).expect("awaiting WELCOME") {
        (_, Message::Welcome(_)) => {}
        (_, other) => panic!("expected WELCOME, got {other:?}"),
    }
    conn
}

/// Wait for this peer's CLIP_REQUEST, ignoring host→guest CLIP_OFFERs that arrive on the
/// same channel (a late joiner is sent the current host clipboard immediately).
fn await_request(conn: &mut AgentConn, who: &str, timeout: Duration) -> u64 {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "{who}: host never sent a CLIP_REQUEST — its offer was dropped, so this \
             session's clipboard can never reach the host"
        );
        match conn.recv(left) {
            Ok((_, Message::ClipRequest(r))) => return r.serial,
            Ok((_, Message::ClipOffer(_))) => continue, // host→guest traffic; not ours
            Ok((_, other)) => panic!("{who}: unexpected {other:?}"),
            Err(_) => panic!(
                "{who}: host never sent a CLIP_REQUEST — its offer was dropped, so this \
                 session's clipboard can never reach the host"
            ),
        }
    }
}

/// Offer `text` at `serial` from `conn` and wait for it to land on the pasteboard.
fn copy_from_peer(conn: &mut AgentConn, who: &str, serial: u64, text: &str, pb_name: &str) {
    conn.send(&Message::ClipOffer(ClipOffer {
        serial,
        mime_types: vec![TEXT_MIME.into()],
    }))
    .expect("sending CLIP_OFFER");
    let got = await_request(conn, who, Duration::from_secs(5));
    assert_eq!(got, serial, "{who}: request answers the wrong offer");
    conn.send(&Message::ClipData(ClipData {
        serial,
        mime_type: TEXT_MIME.into(),
        data: text.as_bytes().to_vec(),
    }))
    .expect("sending CLIP_DATA");

    let deadline = Instant::now() + Duration::from_secs(5);
    while limina_test::pasteboard_text(pb_name).as_deref() != Some(text) {
        assert!(
            Instant::now() < deadline,
            "{who}: pasteboard never received {text:?} (currently {:?})",
            limina_test::pasteboard_text(pb_name)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn l1_clipboard_serials_are_per_peer_not_global() {
    if !limina_test::require_hvf_or_skip("l1_clipboard_serials_are_per_peer_not_global") {
        return;
    }

    let pb_name = format!("limina-test-pb-multi-{}", std::process::id());
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_control_agent()
        .with_control_socket()
        .with_supervisor_log()
        .with_env("LIMINA_PASTEBOARD", &pb_name)
        .with_env("LIMINA_CLIP_POLL_MS", "50");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_supervisor_log(
            "guest agent connected: limina-init/",
            Duration::from_secs(15),
        )
        .expect("supervisor never logged the seed agent handshake");

    // Session A: long-lived, so its serial counter has climbed.
    let mut a = join_as_clipboard_peer(&mut guest, "limina-test-session-a/0");
    copy_from_peer(&mut a, "session A", 500, "copied-in-session-a", &pb_name);
    eprintln!("session A OK at serial 500");

    // Session B: a *different* session's helper, freshly started — its serials begin at 1,
    // far below A's. THE regression: the host used to compare them against A's high-water
    // mark and silently drop every one of them.
    let mut b = join_as_clipboard_peer(&mut guest, "limina-test-session-b/0");
    copy_from_peer(&mut b, "session B", 1, "copied-in-session-b", &pb_name);
    eprintln!("session B OK at serial 1 despite A's higher serials");

    // And B keeps working (its own ratchet advances normally)…
    copy_from_peer(
        &mut b,
        "session B",
        2,
        "copied-in-session-b-again",
        &pb_name,
    );

    // …while A is unaffected by B's low numbers: A's own ratchet still moves forward.
    copy_from_peer(
        &mut a,
        "session A",
        501,
        "copied-in-session-a-again",
        &pb_name,
    );

    // Each peer must still reject its OWN stale offers — the per-peer ratchet is the point,
    // not the absence of one. A replayed 500 (< A's current 501) must draw no request.
    a.send(&Message::ClipOffer(ClipOffer {
        serial: 500,
        mime_types: vec![TEXT_MIME.into()],
    }))
    .expect("sending a stale CLIP_OFFER");
    match a.recv(Duration::from_millis(750)) {
        Err(_) => {}                         // no reply: correct
        Ok((_, Message::ClipOffer(_))) => {} // host→guest traffic is fine
        Ok((_, Message::ClipRequest(r))) => {
            panic!(
                "host requested a stale offer (serial {}) from session A",
                r.serial
            )
        }
        Ok((_, other)) => panic!("unexpected {other:?}"),
    }

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(outcome.code, Some(0), "expected orderly power-off");
}
