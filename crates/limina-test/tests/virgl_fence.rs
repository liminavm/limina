// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Baseline-tier fence honesty: once a stock guest creates a **vrend** (GL) context,
//! Global-ring fences must retire through **virglrenderer's GL timeline** — not be
//! marked completed at decode time.
//!
//! Regresses the loose-fence bug found by the 2026-07-28 crossmark matrix: limina's
//! `create_fence_inner` sync-completed EVERY Global-ring fence (a coexist-venus
//! firmware-wedge fix that swept vrend's real fences along with it), so a stock
//! guest's `glFinish` waited only for host-side *decode*, never for GL/Metal
//! completion. Measured symptom: vrend's fenced desktop frame (0.23 ms) "beating"
//! both host-native references — a guest can't outrun the host it runs on; the
//! fences were lying. Fix: `vrend_ctx_seen` routing in libkrun's virtio_gpu.rs.
//!
//! What this asserts, in order:
//!  1. The routing flip happens: the worker logs the oracle line when the seated
//!     GNOME session creates its first vrend context (RED pre-fix: line never
//!     existed).
//!  2. Routed fences actually RETIRE: the autologin session keeps coming up and a
//!     fenced GL client (`glxinfo`) completes after the flip. A routing bug that
//!     parks Global fences forever would hang the desktop right here.
//!
//! Honesty of the resulting *numbers* is verified by the crossmark instrument
//! (spikes/crossmark), not this test — timing asserts on shared CI-ish hosts flake.
//!
//! Vehicle and SKIP rules identical to `virgl.rs` (stock 4 KiB F44 autologin
//! baseline, coexist GPU, zink-on-KK host GL). Gated behind LIMINA_HVF_TESTS; run
//! via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn virgl_global_fences_retire_through_virglrenderer() {
    if !limina_test::require_hvf_or_skip("virgl_global_fences_retire_through_virglrenderer") {
        return;
    }

    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED virgl_global_fences_retire_through_virglrenderer: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    if limina_test::zink_kk_mesa_prefix().is_none() {
        eprintln!(
            "SKIPPED virgl_global_fences_retire_through_virglrenderer: no zink-on-KK Mesa prefix \
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
            eprintln!("SKIPPED virgl_global_fences_retire_through_virglrenderer: {e}");
            return;
        }
    };
    eprintln!("booting stock 4 KiB F44 (coexist GPU, virgl/zink-on-KK host GL, NAT)");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for_supervisor_log("software_2d = false", Duration::from_secs(60))
        .expect("coexist GPU did not come up (degraded to software-2D?)");

    // 1. THE ORACLE: the first vrend context must flip Global-ring fence routing to
    // virglrenderer. The stock session's GNOME shell creates its vrend context early in
    // the graphical bring-up, well before sshd. RED pre-fix: no such line, ever.
    guest
        .wait_for_supervisor_log(
            "Global-ring fences now retire through virglrenderer",
            Duration::from_secs(300),
        )
        .expect(
            "the vrend fence-routing flip never happened — Global-ring fences are being \
             sync-completed at decode (loose glFinish, the crossmark 2026-07-28 bug)",
        );

    // 2. Routed fences must RETIRE: everything below happens *after* the flip, so the
    // session progressing to a usable seated desktop + a fenced GL client completing
    // proves the virglrenderer path signals fences back to the guest. A parked-forever
    // Global fence wedges the desktop and times these out.
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy (fences wedged post-flip?)");
    eprintln!("guest SSH up: {banner}");
    guest
        .ssh_poll(
            "ls /run/user/1000/.mutter-Xwaylandauth.*",
            Duration::from_secs(240),
        )
        .expect("the autologin session's Xwayland never came up (fences wedged post-flip?)");

    let x11 = "DISPLAY=:0 XAUTHORITY=$(ls /run/user/1000/.mutter-Xwaylandauth.* | head -1) \
               XDG_RUNTIME_DIR=/run/user/1000";
    let renderer = guest
        .ssh_exec(&format!(
            "env {x11} timeout 60 glxinfo 2>/dev/null | grep 'OpenGL renderer string' || true"
        ))
        .expect("ssh to the guest failed");
    eprintln!("glxinfo: {}", renderer.trim());
    assert!(
        renderer.contains("virgl"),
        "expected the guest GL renderer to be virgl (baseline tier); got {renderer:?} — \
         an empty string means glxinfo was killed by `timeout` (fences wedged)"
    );

    // No renderer refusals: the warn-and-fall-back path in create_fence_inner exists so a
    // refusal can never wedge, but in a healthy vrend session it should never fire at all.
    let log = guest.supervisor_log();
    assert!(
        !log.contains("refused by renderer"),
        "virglrenderer refused Global-ring fences — routing is falling back to the sync \
         mark (loose fences again). Log lines:\n{}",
        log.lines()
            .filter(|l| l.contains("refused by renderer"))
            .take(5)
            .collect::<Vec<_>>()
            .join("\n")
    );

    let outcome = guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
