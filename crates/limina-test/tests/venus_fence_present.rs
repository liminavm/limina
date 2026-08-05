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
//! Vehicle: the seated ENHANCED golden with the session's GL flipped back to
//! zink→venus **by this test** (a 99- environment.d override on the per-guest disk
//! clone + a gdm restart). Since the 2026-08-04 drop-guest-zink flip the golden's own
//! session rides virgl/vrend, whose EGLImage scanouts never park — the fence-present
//! chain is venus-BLOB-only, so the stock session stopped exercising it (this test
//! caught exactly that on 2026-08-05 when the F43 golden flipped). The zink→venus
//! session is not a legacy config: it models any venus-rendering compositor (the
//! synoik direction — venus-blob framebuffers via SET_SCANOUT_BLOB), which is the
//! class this chain serves. `LIMINA_FENCE_PRESENT=1` is forced: the harness display
//! is the ack-less capture sink, where the 0110 default stays off (by design) and
//! deferred presents take the 0111 readback fallback. The shipping (windowed +
//! shown-ack) leg stays covered by the seated A/B in `spikes/present-miss/`.
//!
//! Oracles: the INFO "fence-accurate presents ENGAGED" line (fires on the first parked
//! present, libkrun 0114 — proves the path is LIVE, not silently fallen back to
//! immediate presents), no deferred-present/readback errors in the worker log, and the
//! session surviving — gnome-shell still up and SSH still responsive after frames flowed.
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
            // The engagement oracle logs at INFO (libkrun 0114) and the harness
            // defaults the spawned supervisor to RUST_LOG=info, so no extra logging
            // env is needed — deliberately NOT debug/trace: the per-fence firehose
            // adds ~2k sync writes/s under load and destabilizes the very pacing
            // this guard watches.
            .with_env("LIMINA_FENCE_PRESENT", "1"),
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

    // Re-create the venus-compositor vehicle: the golden's session GL rides virgl/vrend
    // since drop-guest-zink, and vrend's EGLImage scanouts never park — only venus BLOB
    // scanouts do. Flip THIS clone's session to zink→venus (99- overrides the installed
    // 90-limina-zink.conf; environment.d merges lexically, later wins) and restart gdm so
    // the autologin session comes back up on venus. MESA_LOADER_DRIVER_OVERRIDE=zink also
    // flips gbm's backing driver, making the compositor's KMS buffers venus blobs — that
    // is the point, not a side effect (see memory limina-classic-gbm-venus-import).
    // environment.d is only re-read when the USER manager (user@1000) starts, so the
    // restart must take the manager down too — and in the right ORDER: gdm first.
    // `loginctl terminate-user` + `restart gdm` races: gdm re-seats the autologin
    // session before the manager finishes winding down, the manager never exits, and
    // the new session silently keeps the old virgl env (verified live 2026-08-05 —
    // the manager pid survived and /proc/<shell>/environ still said virgl). The
    // stop-gdm → stop-user@ → start-gdm sequence was likewise verified live: the new
    // session's environ shows zink and gnome-shell maps libvulkan_virtio. Detached
    // (systemd-run) because stopping gdm tears down our own ssh session's scope.
    guest
        .ssh_exec(
            "printf 'GALLIUM_DRIVER=zink\\nMESA_LOADER_DRIVER_OVERRIDE=zink\\n' | \
             sudo tee /etc/environment.d/99-fence-present-zink.conf >/dev/null && \
             sudo systemd-run --no-block sh -c \
             'systemctl stop gdm; systemctl stop user@1000; systemctl start gdm'",
        )
        .expect("flipping the session env to zink→venus");
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never came back after the zink→venus gdm restart");

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

    // Liveness oracle: the first parked frame logs the INFO engagement line. Without
    // it the chain silently degraded to immediate presents and this guard would be
    // testing nothing.
    guest
        .wait_for_supervisor_log("fence-accurate presents ENGAGED", Duration::from_secs(120))
        .expect(
            "the fence-present engagement oracle never fired — the chain did not engage \
             (regressed to immediate presents?)",
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
