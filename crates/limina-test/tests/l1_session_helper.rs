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
        // The RemoteDesktop backend is production-default-OFF; this test exists to
        // exercise it, so opt in (limina-init maps the token to LIMINA_CLIPBOARD_RD=1).
        .with_cmdline_token("limina.clipboard_rd")
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
    // The guest serves a per-guest CLONE of the rootfs (parallel isolation) — every
    // post-boot exchange over the rootfs channel goes through the clone, not the source.
    let mock_log = guest.rootfs_dir().join(format!("mock-{uniq}.log"));
    let mock_copy = guest.rootfs_dir().join(format!("mock-{uniq}.copy"));

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
        // RD opted in so the preference assertion is meaningful: the bridge must win
        // over an AVAILABLE RemoteDesktop backend, not a disabled one.
        .with_cmdline_token("limina.clipboard_rd")
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
    // Post-boot rootfs-channel traffic goes through the guest's per-guest clone (see above).
    let mock_log = guest.rootfs_dir().join(format!("mock-{uniq}.log"));
    let mock_copy = guest.rootfs_dir().join(format!("mock-{uniq}.copy"));
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

/// Production default (2026-07-20, user-decided): withOUT the RD opt-in
/// (`LIMINA_CLIPBOARD_RD=1`), the helper must NEVER create a RemoteDesktop session —
/// even when that API is the only backend on offer and the probe grace has long
/// expired. A resident RemoteDesktop session lights GNOME's screen-share indicator for
/// the whole session; the clipboard@limina extension bridge supersedes it. The mock
/// claims only the RemoteDesktop API here (no bridge), so the helper has nothing quiet
/// to land on and must park forever rather than go loud.
#[test]
fn l1_real_session_helper_never_takes_remotedesktop_without_optin() {
    if !limina_test::require_hvf_or_skip(
        "l1_real_session_helper_never_takes_remotedesktop_without_optin",
    ) {
        return;
    }

    let uniq = format!("{}c", std::process::id()); // distinct mock files from the other tests
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_control_agent()
        .with_cmdline_token("limina.dbus")
        .with_cmdline_token("limina.mock_mutter")
        .with_cmdline_token("limina.session_helper")
        // NO limina.clipboard_rd token — the production default.
        .with_cmdline_token(&format!("limina.mock_id={uniq}"))
        .with_supervisor_log();
    let rootfs = rootfs_of(&cfg);
    let mock_log = rootfs.join(format!("mock-{uniq}.log"));
    let _ = std::fs::remove_file(&mock_log);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    // Post-boot rootfs-channel traffic goes through the guest's per-guest clone (see above).
    let mock_log = guest.rootfs_dir().join(format!("mock-{uniq}.log"));

    // The helper announces the disabled fallback on its first failed probe round
    // (its stderr reaches the guest console via limina-init).
    guest
        .wait_for(
            "RemoteDesktop fallback disabled (LIMINA_CLIPBOARD_RD unset)",
            Duration::from_secs(30),
        )
        .expect("helper never reported the RD fallback as disabled");

    // Outlast the enabled-path grace window (RD_GRACE_ATTEMPTS × 2 s ≈ 20 s) with
    // margin: if the gate were broken, the helper would have gone loud by now.
    std::thread::sleep(Duration::from_secs(25));
    let log = std::fs::read_to_string(&mock_log).unwrap_or_default();
    assert!(
        !log.contains("CREATE_SESSION"),
        "helper created a RemoteDesktop session despite the opt-in being absent:\n{log}"
    );
    // And it must still be probing quietly, not connected via a loud backend: the
    // clipboard capability handshake only happens once a backend is chosen.
    assert!(
        !guest
            .supervisor_log()
            .contains("guest agent connected: limina-agent-session/"),
        "helper picked a backend with nothing quiet available and RD opted out"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(outcome.code, Some(0), "expected orderly power-off");

    let _ = std::fs::remove_file(&mock_log);
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
