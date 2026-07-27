// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 guard for the fence-accurate present chain (libkrun 0017–0021, 0110/0111).
//!
//! The chain was validated by hand in June 2026 and then sat DORMANT — the productized
//! supervisor never armed `LIMINA_FENCE_PRESENT`, and no test exercised it, so every
//! green run since validated only the fire-and-forget present path (found 2026-07-27,
//! `spikes/present-miss/`). This test pins the chain itself: park the zero-copy flush,
//! inject the present fence on vkr ring 63, retire it, present the parked frame, and
//! complete the guest's held flush fence — a wedge anywhere in that loop starves the
//! guest's scanout pipeline and kills the session.
//!
//! Vehicle: the seated ENHANCED golden (gnome-shell rendering zink→venus→IOSurface blob
//! scanouts — the only path that parks frames), with `LIMINA_FENCE_PRESENT=1` forced:
//! the harness display is the ack-less capture sink, where the 0110 default stays off
//! (by design) and deferred presents take the 0111 readback fallback. The shipping
//! (windowed + shown-ack) leg stays covered by the seated A/B in `spikes/present-miss/`.
//!
//! Oracles: the `[FENCEPRESENT]` deferred-present trace line (fires on the first parked
//! present — proves the path is LIVE, not silently fallen back to immediate presents),
//! no deferred-present/readback errors in the worker log, and the session surviving —
//! gnome-shell still up and SSH still responsive after frames have flowed.
//!
//! Same prereqs as the other seated L2s: 16 KiB kernel + `enhanced.test` disk +
//! KosmicKrisp; SKIPs cleanly if missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn fence_present_chain_presents_and_never_wedges() {
    if !limina_test::require_hvf_or_skip("fence_present_chain_presents_and_never_wedges") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED fence_present_chain_presents_and_never_wedges: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }

    let cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log()
            // Force the chain on: the capture sink has no ack channel, so the 0110
            // default (ack-gated) would leave it off — the point here is the chain's
            // mechanics, not the default policy (that's unit-tested in libkrun).
            .with_env("LIMINA_FENCE_PRESENT", "1")
            // The [FENCEPRESENT] oracle is a trace-level line in the gpu module; scope
            // the filter so the log stays readable (FLUSH2 et al. are boot-console 2D
            // volume, bounded on a direct-kernel boot).
            .with_env("RUST_LOG", "krun_devices::virtio::gpu=trace"),
        Err(e) => {
            eprintln!("SKIPPED fence_present_chain_presents_and_never_wedges: {e}");
            return;
        }
    };
    eprintln!(
        "booting the seated enhanced venus desktop with fence-present FORCED ON via {:?}",
        cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // The seated session coming up IS the workload: gdm + gnome-shell on zink→venus
    // present a steady stream of blob-scanout flushes, every one of which must round
    // the park→inject→retire→present→complete-hold loop.
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated enhanced session didn't come up");

    // `pgrep gnome-shell` is satisfied by the GREETER's shell (llvmpipe/sw2d) too, and gdm
    // autologin is the slow, occasionally-flaky step — so gate on the SESSION shell's venus
    // context (init=0x4) appearing in the worker log before judging the present chain. A
    // failure here means "the seated session never went venus", which is an image/session
    // problem, not a fence-present one.
    guest
        .wait_for_supervisor_log("init=0x4 name=Some(\"gnome-shell", Duration::from_secs(180))
        .expect(
            "no venus gnome-shell context appeared — the seated session never came up on \
             zink→venus (autologin flake or image problem; not a fence-present failure)",
        );

    // Liveness oracle: the first parked frame logs [FENCEPRESENT] deferred presents=0.
    // Without it the chain silently degraded to immediate presents and this guard
    // would be testing nothing.
    guest
        .wait_for_supervisor_log(
            "[FENCEPRESENT] deferred presents=",
            Duration::from_secs(120),
        )
        .expect(
            "the [FENCEPRESENT] deferred-present oracle never fired — fence-present did not \
             engage (chain regressed to immediate presents?)",
        );

    // Wedge guard: a stuck hold starves the guest's whole scanout pipeline within one
    // 500 ms ceiling, and a session whose display fence wedges loses gnome-shell (ring
    // FATAL) — so "still seated + still responsive after frames flowed" is the check.
    std::thread::sleep(Duration::from_secs(10));
    let shell = guest
        .ssh_exec("pgrep -x gnome-shell >/dev/null && echo ALIVE")
        .expect("ssh died after fence-present frames flowed");
    assert_eq!(
        shell.trim(),
        "ALIVE",
        "gnome-shell exited under fence-present — scanout pipeline wedge?"
    );

    // No dropped parked frames: the deferred present must either zero-copy or take the
    // 0111 readback fallback; any of these lines means frames were lost or the
    // fallback itself broke.
    let log = guest.supervisor_log();
    for needle in [
        "deferred present_surface failed",
        "deferred readback: alloc_frame failed",
        "deferred readback: read_iosurface failed",
        "deferred readback: present_frame failed",
        "deferred readback: scanout",
    ] {
        assert!(
            !log.contains(needle),
            "worker logged {needle:?} — parked frames are being dropped.\nlog tail:\n{}",
            log.lines().rev().take(30).collect::<Vec<_>>().join("\n")
        );
    }

    guest
        .shutdown(Duration::from_secs(30))
        .expect("guest shutdown");
}
