// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 integration test of the REAL `limina-agent-session` binary — no GNOME required.
//!
//! The L1 guest brings up a real session bus (Alpine-extracted dbus-daemon, staged by
//! `build-test-guest.sh`) and `limina-mock-mutter`, a scripted stand-in claiming
//! `org.gnome.Mutter.RemoteDesktop`. The actual product helper runs against it and
//! bridges to the actual supervisor + named pasteboard. Both directions are asserted
//! through the virtio-fs rootfs (a host directory — the mock's observations and the
//! test's triggers are plain files):
//!
//! - host→guest: pasteboard write → supervisor OFFER → helper REQUEST/DATA →
//!   SetSelection on the mock → the mock plays a pasting app (SelectionTransfer) →
//!   the helper's SelectionWrite content appears as `PASTED <text>` in the mock log.
//! - guest→host: the test writes the mock's `.copy` trigger → SelectionOwnerChanged →
//!   helper OFFER → supervisor REQUEST → helper SelectionRead (served by the mock) →
//!   the text lands on the named pasteboard.
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use limina_test::{Boot, Guest, GuestConfig};

fn rootfs_of(cfg: &GuestConfig) -> PathBuf {
    match &cfg.boot {
        Boot::Kernel { rootfs, .. } => rootfs.clone(),
        other => panic!("expected a kernel boot with a rootfs dir, got {other:?}"),
    }
}

#[test]
fn l1_real_session_helper_bridges_clipboard_via_mock_mutter() {
    if !limina_test::require_hvf_or_skip("l1_real_session_helper_bridges_clipboard_via_mock_mutter")
    {
        return;
    }

    let uniq = std::process::id();
    let pb_name = format!("limina-test-pb-sess-{uniq}");
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_control_agent()
        .with_cmdline_token("limina.dbus")
        .with_cmdline_token("limina.mock_mutter")
        .with_cmdline_token("limina.session_helper")
        .with_cmdline_token(&format!("limina.mock_id={uniq}"))
        .with_supervisor_log()
        .with_env("LIMINA_PASTEBOARD", &pb_name)
        .with_env("LIMINA_CLIP_POLL_MS", "50");
    let rootfs = rootfs_of(&cfg);
    let mock_log = rootfs.join(format!("mock-{uniq}.log"));
    let mock_copy = rootfs.join(format!("mock-{uniq}.copy"));
    let _ = std::fs::remove_file(&mock_log);
    let _ = std::fs::remove_file(&mock_copy);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // The REAL helper must come up on the mock compositor and handshake with the
    // supervisor, advertising the clipboard capability.
    guest
        .wait_for_supervisor_log(
            "guest agent connected: limina-agent-session/",
            Duration::from_secs(30),
        )
        .expect("the real limina-agent-session never connected (dbus/mock/helper chain)");

    // --- host→guest: a host copy must reach a pasting guest app. --------------------
    limina_test::set_pasteboard_text(&pb_name, "sess-host-to-guest-42");
    wait_for_file_contains(
        &mock_log,
        "PASTED sess-host-to-guest-42",
        Duration::from_secs(10),
    );
    let log = std::fs::read_to_string(&mock_log).expect("reading mock log");
    assert!(
        log.contains("SET_SELECTION") && log.contains("text/plain;charset=utf-8"),
        "helper never owned the selection with the text mime:\n{log}"
    );
    eprintln!("host→guest OK: the mock's pasting app received the host text");

    // --- guest→host: a scripted guest copy must land on the pasteboard. -------------
    std::fs::write(&mock_copy, "sess-guest-to-host-77").expect("writing the copy trigger");
    let deadline = Instant::now() + Duration::from_secs(10);
    while limina_test::pasteboard_text(&pb_name).as_deref() != Some("sess-guest-to-host-77") {
        assert!(
            Instant::now() < deadline,
            "pasteboard never received the guest copy (currently {:?}); mock log:\n{}",
            limina_test::pasteboard_text(&pb_name),
            std::fs::read_to_string(&mock_log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("guest→host OK: the scripted guest copy landed on the pasteboard");

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(outcome.code, Some(0), "expected orderly power-off");

    let _ = std::fs::remove_file(&mock_log);
    let _ = std::fs::remove_file(&mock_copy);
}

/// The middle backend: with `limina.mock_bridge` the mock ALSO claims
/// org.limina.Clipboard (the clipboard@limina extension stand-in). The real helper
/// must (a) prefer it over the also-present RemoteDesktop API — no session is ever
/// created, so no screen-share indicator on a real desktop — and (b) bridge both
/// directions through it: host→guest is a direct `Set` (`BRIDGE_SET` in the mock log;
/// the compositor-parked source serves pastes, no transfer choreography), guest→host
/// is OwnerChanged → OFFER → `Read` (`BRIDGE_READ`) → pasteboard.
#[test]
fn l1_real_session_helper_prefers_extension_bridge() {
    if !limina_test::require_hvf_or_skip("l1_real_session_helper_prefers_extension_bridge") {
        return;
    }

    let uniq = format!("{}b", std::process::id()); // distinct mock files from the RD test
    let pb_name = format!("limina-test-pb-bridge-{uniq}");
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_control_agent()
        .with_cmdline_token("limina.dbus")
        .with_cmdline_token("limina.mock_mutter")
        .with_cmdline_token("limina.mock_bridge")
        .with_cmdline_token("limina.session_helper")
        .with_cmdline_token(&format!("limina.mock_id={uniq}"))
        .with_supervisor_log()
        .with_env("LIMINA_PASTEBOARD", &pb_name)
        .with_env("LIMINA_CLIP_POLL_MS", "50");
    let rootfs = rootfs_of(&cfg);
    let mock_log = rootfs.join(format!("mock-{uniq}.log"));
    let mock_copy = rootfs.join(format!("mock-{uniq}.copy"));
    let _ = std::fs::remove_file(&mock_log);
    let _ = std::fs::remove_file(&mock_copy);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_supervisor_log(
            "guest agent connected: limina-agent-session/",
            Duration::from_secs(30),
        )
        .expect("the real limina-agent-session never connected (dbus/mock/helper chain)");

    // --- host→guest: a host copy arrives as a direct bridge Set. --------------------
    limina_test::set_pasteboard_text(&pb_name, "bridge-host-to-guest-42");
    wait_for_file_contains(
        &mock_log,
        "BRIDGE_SET bridge-host-to-guest-42",
        Duration::from_secs(10),
    );
    eprintln!("host→guest OK: the bridge received the host text via Set");

    // --- guest→host: a scripted guest copy must land on the pasteboard. -------------
    std::fs::write(&mock_copy, "bridge-guest-to-host-77").expect("writing the copy trigger");
    let deadline = Instant::now() + Duration::from_secs(10);
    while limina_test::pasteboard_text(&pb_name).as_deref() != Some("bridge-guest-to-host-77") {
        assert!(
            Instant::now() < deadline,
            "pasteboard never received the guest copy (currently {:?}); mock log:\n{}",
            limina_test::pasteboard_text(&pb_name),
            std::fs::read_to_string(&mock_log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let log = std::fs::read_to_string(&mock_log).expect("reading mock log");
    assert!(
        log.contains("BRIDGE_READ"),
        "guest→host content did not travel through the bridge backend:\n{log}"
    );
    eprintln!("guest→host OK: the scripted guest copy landed on the pasteboard via Read");

    // --- tier preference: RemoteDesktop must never have been touched. ---------------
    assert!(
        !log.contains("CREATE_SESSION"),
        "helper fell back to RemoteDesktop despite a live extension bridge:\n{log}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(outcome.code, Some(0), "expected orderly power-off");

    let _ = std::fs::remove_file(&mock_log);
    let _ = std::fs::remove_file(&mock_copy);
}

fn wait_for_file_contains(path: &std::path::Path, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if content.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{path:?} never contained {needle:?}; current content:\n{content}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
