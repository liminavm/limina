// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 — **the worker's listeners have no filesystem path.**
//!
//! The worker serves four protocols the supervisor connects to: display control, balloon
//! control, the FIDO gadget (CTAPHID for the SEP-backed passkey store) and the fingerprint gadget
//! (the Touch-ID-gated match-on-chip protocol). A listener bound at `$TMPDIR/limina-<kind>-<pid>.sock`
//! takes a connection from any process of the same user. The supervisor instead hands the worker
//! one end of a socketpair per listener and connects by sending it a fresh stream over that link
//! (`limina_launch::connect`), so nothing but the two processes can reach either end.
//!
//! Oracles, with every listener the supervisor would auto-allocate switched on (balloon via
//! `--memory`, FIDO via a store and the test-approve knob, the fingerprint gadget via its knob):
//! 1. No `limina-{balloon,fido-usb,moc-usb,resize}-<supervisor pid>.sock` exists in `$TMPDIR`.
//! 2. The FIDO probe's CTAPHID round trip still completes: that traffic crosses the link.

use limina_test::{Guest, GuestConfig};
use std::time::Duration;

#[test]
fn l1_the_worker_listeners_have_no_path() {
    if !limina_test::require_hvf_or_skip("l1_the_worker_listeners_have_no_path") {
        return;
    }
    let store = std::env::temp_dir().join(format!(
        "limina-fido-l1_no_socket_paths-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&store);
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_memory(256, 512)
        .with_supervisor_arg("--usb")
        .with_supervisor_arg("--fingerprint")
        .with_env("LIMINA_FP_TEST_APPROVE", "1")
        .with_env("LIMINA_FIDO_TEST_APPROVE", "1")
        .with_env("LIMINA_FIDO_STORE", &store.to_string_lossy())
        .with_cmdline_token("limina.fido_probe");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    // --- Oracle 2: FIDO traffic crosses the link ---
    guest
        .wait_for("fido_probe: begin", Duration::from_secs(15))
        .expect("guest did not start the FIDO probe");
    guest
        .wait_for("RESULT: fido_init OK", Duration::from_secs(20))
        .expect("CTAPHID INIT did not complete through the FIDO gadget");

    // --- Oracle 1: no listener has a path ---
    let supervisor = guest.supervisor_pid();
    let tmp = std::env::temp_dir();
    let found: Vec<_> = ["balloon", "fido-usb", "moc-usb", "resize"]
        .iter()
        .map(|kind| tmp.join(format!("limina-{kind}-{supervisor}.sock")))
        .filter(|p| p.exists())
        .collect();

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    let _ = std::fs::remove_file(&store);
    assert!(
        found.is_empty(),
        "worker listeners bound at paths any same-user process can connect to: {found:?}"
    );
    assert!(
        !outcome.forced,
        "harness had to force teardown: {outcome:?}"
    );
}
