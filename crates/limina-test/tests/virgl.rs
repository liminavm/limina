// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Baseline-tier 3D regression: a stock 4 KiB guest renders through **virgl/vrend**
//! (zink-on-KosmicKrisp) and a GL **readback** does NOT wedge the GPU.
//!
//! Regresses the libkrun bug fixed by patch 0024: `virtio_gpu::transfer_read`
//! (`TRANSFER_FROM_HOST_3D`) was a `panic!("unimplemented")` stub. venus (zero-copy
//! host-visible blobs) never reads back, so upstream never needed it — but a stock 4 KiB
//! guest on the coexist device drives virgl/vrend, whose copy model DOES read back. `glxinfo`
//! issues exactly one `TRANSFER_FROM_HOST_3D` (empirically confirmed: `buf=None`, the
//! host-resource → guest-iovecs copy). Pre-fix that single readback panicked the GPU-worker
//! thread, after which every later virtio-gpu command blocked on a fence that never completed
//! and the whole guest appeared to hang. The user hit this live: a windowed F44 froze the
//! instant `glxinfo` ran. See memory `limina-baseline-3d-plan` and `spikes/virgl-zink-kk`.
//!
//! Vehicle: the stock 4 KiB F44 autologin baseline (`Fedora-Workstation-44.boot.raw`) on the
//! coexist GPU with the zink-on-KK host-GL worker env — virgl is selected because a 4 KiB guest
//! can't map venus's host-visible blobs. The seated GNOME session's Xwayland is what lets
//! `glxinfo` reach a GL context over SSH (the exact repro path).
//!
//! SKIPs cleanly without: LIMINA_HVF_TESTS, the KosmicKrisp host ICD, the zink-on-KK Mesa
//! prefix (both machine-local /Volumes/mesa-cs builds), the GOP firmware, or the baseline disk.
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`. Heavy: a full seated stock
//! desktop boot (software-2D first-boot can take minutes to reach the autologin session).

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn virgl_readback_does_not_wedge_the_gpu() {
    if !limina_test::require_hvf_or_skip("virgl_readback_does_not_wedge_the_gpu") {
        return;
    }

    // virgl's host GL is zink-on-KosmicKrisp: both the KK Vulkan ICD and the zink-on-KK Mesa
    // prefix are machine-local dev builds. SKIP (don't fail) when either is missing — else
    // Guest::boot would degrade the coexist display to software-2D and never exercise vrend.
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED virgl_readback_does_not_wedge_the_gpu: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    if limina_test::zink_kk_mesa_prefix().is_none() {
        eprintln!(
            "SKIPPED virgl_readback_does_not_wedge_the_gpu: no zink-on-KK Mesa prefix \
             (build spikes/virgl-zink-kk/build-mesa-zink-kk.sh; or set MESA_PREFIX)"
        );
        return;
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_virgl_host_gl()
            .with_net()
            .with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED virgl_readback_does_not_wedge_the_gpu: {e}");
            return;
        }
    };
    eprintln!("booting stock 4 KiB F44 (coexist GPU, virgl/zink-on-KK host GL, NAT)");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // The worker advertises the coexist device (virgl + venus), not software-2D. If KK had been
    // missing, Guest::boot would have degraded to software-2D — but we SKIPped above, so assert
    // the coexist device actually came up (else the readback path under test isn't exercised).
    guest
        .wait_for_supervisor_log("software_2d = false", Duration::from_secs(60))
        .expect("coexist GPU did not come up (degraded to software-2D?)");

    // A stock software-2D first boot is slow; the inbound forward only yields a banner once
    // sshd is reachable, and the autologin Xwayland session comes up well after that.
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");
    guest
        .ssh_poll(
            "ls /run/user/1000/.mutter-Xwaylandauth.*",
            Duration::from_secs(240),
        )
        .expect("the autologin session's Xwayland never came up");

    // THE REPRO: glxinfo over the seated session's Xwayland issues exactly one
    // TRANSFER_FROM_HOST_3D readback. Pre-fix that panicked the GPU-worker thread and wedged the
    // GPU, so glxinfo blocked forever. `ssh_exec` has no command timeout, so wrap the readback in
    // a guest-side `timeout` (+ `|| true` so the SSH call still returns its output): a regression
    // then fails FAST on the assertions below instead of hanging the whole test run.
    let x11 = "DISPLAY=:0 XAUTHORITY=$(ls /run/user/1000/.mutter-Xwaylandauth.* | head -1) \
               XDG_RUNTIME_DIR=/run/user/1000";
    let renderer = guest
        .ssh_exec(&format!(
            "env {x11} timeout 60 glxinfo 2>/dev/null | grep 'OpenGL renderer string' || true"
        ))
        .expect("ssh to the guest failed");
    eprintln!("glxinfo: {}", renderer.trim());

    // Primary regression guard: the GPU-worker thread must not have panicked. The panic is
    // logged (worker stderr → the supervisor log) the instant the readback hits the stub, so
    // this catches the bug fast and unambiguously.
    let log = guest.supervisor_log();
    assert!(
        !log.contains("gpu worker' panicked"),
        "the GPU worker panicked — transfer_read regressed (libkrun patch 0024).\n\
         worker log tail:\n{}",
        log.lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        !log.contains("transfer_read unimplemented"),
        "transfer_read is back to the unimplemented panic stub (libkrun patch 0024 reverted)"
    );

    // And it actually rendered THROUGH virgl (not a silent software fallback, and not an empty
    // string from a `timeout`-killed hang): the renderer string must name virgl.
    assert!(
        renderer.contains("virgl"),
        "expected the guest GL renderer to be virgl (baseline tier); got {renderer:?} — \
         an empty string means glxinfo was killed by `timeout` (the GPU wedged)"
    );

    // The GPU must still be alive AFTER the readback: a second readback must also return.
    // Pre-fix the worker thread was already dead and this would hang (so it's `timeout`-wrapped).
    let second = guest
        .ssh_exec(&format!(
            "env {x11} timeout 60 glxinfo -B 2>/dev/null | grep -i device || true"
        ))
        .expect("ssh to the guest failed");
    assert!(
        second.to_lowercase().contains("virgl") || second.to_lowercase().contains("device"),
        "the GPU was wedged after the first readback (second glxinfo returned {second:?})"
    );
    eprintln!("post-readback GL still responsive: {}", second.trim());

    let outcome = guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
