// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — a **Vulkan compositor's** desktop must come back from a restore with its pixels.
//!
//! # The bug this is written against (OPEN — this test is expected to FAIL)
//!
//! The venus snapshot captures a `VkDeviceMemory` by `vkMapMemory` + `memcpy`
//! (`vkr_device_memory_content_copy`). Whatever refuses the map is skipped with a `warn!`,
//! next to a summary line that reads like a success. On a synoik session the two allocations
//! that refuse are its double-buffered scanout images:
//!
//! ```text
//! gpu snapshot: content read failed for ctx 2 mem 66 (14745600 bytes)
//! gpu snapshot: content read failed for ctx 2 mem 204 (14745600 bytes)
//! ```
//!
//! — 2560×1440×4, twice. Opening any new client heals the whole screen, which is what shows the
//! textures were never lost.
//!
//! **The model is not settled, and this test is what showed that.** On its own runs seven
//! allocations are skipped (the compositor's two scanout images and a client's five swapchain
//! images) yet the compositor's scanout comes back *correct*; the only wrong region is an idle
//! GTK window's strip, whose buffer is not in the skipped list. Nor is it known why that window
//! lost its pixels and an equally idle firefox did not — both are still, so neither repaints,
//! and unchanged pixels say only that the compositor still had a good buffer, never that the
//! client could still render. So the capture gap is real and asserted below, but which skipped
//! allocation produces which lost pixels is still open — do not write the fix from the summary
//! alone. Dossier, frames and the leading hypothesis:
//! `spikes/synoik-restore/RESULTS.md`.
//!
//! # Why this cannot be folded into `l2_desktop_restore_landmarks`
//!
//! That test drives mutter, which composites through classic vrend — and vrend resources have
//! their own content dump (payload v6 `classic_contents`), captured by GL readback, which needs
//! no mappable memory. So the same snapshot is complete for one renderer and silently partial
//! for the other, and the GL test passes on a guest carrying this bug. Measured, not assumed:
//! on the same day, the mutter test restored 1000 of 1000 landmark cells unchanged while a
//! synoik session went blank. Two compositors, two independent failure paths, two tests.
//!
//! # What a failure here does and does not mean
//!
//! This gates ONE shape: content that comes back blank and heals on repaint. Two neighbouring
//! shapes are real and are NOT covered — a client that comes back **wedged** and stays broken
//! through a resize (firefox nightly on the dogfood Mac), and a restored session that paints
//! nothing at all until a new client arrives. A fix that turns this test green is not evidence
//! about either.
//!
//! # Oracles
//!
//! 1. **The landmarks are unchanged**, exactly as in the GL test: a still workload, a settled
//!    frame each side, per-cell comparison. Measured on the failing path at 1280x800: 120 of
//!    1000 cells, a 5-cell-wide full-height band — the idle nautilus window's visible strip,
//!    come back black. NOT the whole desktop: on this run the compositor's own scanout was
//!    fine and only the one client that never redraws lost its pixels. Sized for that, since a
//!    12% move has to fail as surely as a 70% one does.
//! 2. **Nothing was skipped at snapshot time.** `content read failed` in the worker log is the
//!    mechanism itself, and it is worth asserting directly rather than only through its
//!    symptom: a partial capture that happens not to show today is still a partial capture.
//! 3. **Colour diversity survives** — the content floor. A blanked desktop collapses it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use limina_test::landmarks::{
    by_row, cell_delta, cell_means, color_diversity, settled_capture, CELL_TOL,
};
use limina_test::{Guest, GuestConfig};

/// synoik's Wayland socket. gdm holds `wayland-0`, so the session's own clients land on `-1`
/// — a client launched against `wayland-0` connects to gdm's compositor and never appears.
const SOCKET: &str = "wayland-1";

/// INFO synoik logs once the compositor is up and accepting clients.
const SERVING: &str = "listening on Wayland socket";

/// How long the compositor gets after sshd answers.
const COMPOSITOR_UP: Duration = Duration::from_secs(180);

/// Settle after the workload is launched.
const SETTLE: Duration = Duration::from_secs(20);

/// Landmark cells allowed to differ across the restore. The GL sibling measures zero on a
/// healthy restore; this is headroom for a clock tick, not a tolerance a pass leans on.
const CELL_MISMATCH_BUDGET: f64 = 0.01;

/// `ssh_exec` with retries, tolerating a non-zero exit (several probes report state *as* their
/// exit code).
fn ssh_soft(guest: &Guest, cmd: &str) -> String {
    let wrapped = format!("{{ {cmd} ; }} 2>&1 || true");
    let mut last_err = String::new();
    for _ in 0..4 {
        match guest.ssh_exec(&wrapped) {
            Ok(out) => return out.trim().to_string(),
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    panic!("ssh `{cmd}` kept failing: {last_err}");
}

/// The same still workload as the GL sibling, on synoik's socket: an idle GTK window, a still
/// WebGL frame, and our own Vulkan client redrawing one fixed triangle. Kept deliberately
/// identical so a difference between the two tests is a difference between the compositors.
fn launch_workload(guest: &Guest) {
    let env = "export XDG_RUNTIME_DIR=/run/user/1000 \
               DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus;";
    ssh_soft(
        guest,
        &format!(
            "{env} \
             systemd-run --user --collect --unit l2s-naut \
               env WAYLAND_DISPLAY={SOCKET} nautilus >/dev/null 2>&1; \
             systemd-run --user --collect --unit l2s-ff \
               env WAYLAND_DISPLAY={SOCKET} MOZ_ENABLE_WAYLAND=1 \
               firefox --new-window file:///tmp/webgl-still.html >/dev/null 2>&1; \
             systemd-run --user --collect --unit l2s-vkstill \
               env WAYLAND_DISPLAY={SOCKET} /tmp/vkstill/vkstill >/dev/null 2>&1; \
             echo launched"
        ),
    );
}

fn workload_procs(guest: &Guest) -> String {
    ssh_soft(
        guest,
        "echo naut=$(pgrep -c nautilus || true) \
             ff=$(pgrep -fc lib64/firefox/firefox || true) \
             vkstill=$(pgrep -c vkstill || true) \
             synoik=$(pgrep -x synoik || true)",
    )
}

#[test]
fn synoik_desktop_survives_snapshot_restore() {
    if !limina_test::require_hvf_or_skip("synoik_desktop_survives_snapshot_restore") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED synoik_desktop_survives_snapshot_restore: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let base_cfg = match GuestConfig::seated_efi_synoik_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED synoik_desktop_survives_snapshot_restore: {e:#}");
            return;
        }
    };

    const NET_MAC: &str = "5a:94:ef:44:0f:ad";
    let devices = |cfg: GuestConfig| {
        cfg.with_supervisor_arg("--no-snd")
            .with_supervisor_arg("--no-battery")
            .with_net_mac(NET_MAC)
    };

    // --- Guest 1: seated synoik session with the workload, snapshot-armed ---
    let cfg1 = devices(base_cfg.clone())
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .with_snapshot();
    eprintln!("EFI-booting the synoik enhanced image (snapshot-armed)");
    let mut g1 = Guest::boot(&cfg1).expect("spawning the limina supervisor");
    g1.wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");

    // The compositor has to be serving before anything else means anything. A synoik guest can
    // be perfectly reachable over ssh with no compositor at all — that is the whole shape of
    // the regression `synoik_session` guards — so a boot-reached-sshd oracle proves nothing.
    let deadline = Instant::now() + COMPOSITOR_UP;
    loop {
        let log = ssh_soft(
            &g1,
            "sudo journalctl --boot=0 -t synoik --no-pager 2>/dev/null | tail -n 200",
        );
        if log.contains(SERVING) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "synoik never logged {SERVING:?} — the compositor did not come up, so there is no \
             desktop to compare across a restore:\n{log}"
        );
        std::thread::sleep(Duration::from_secs(5));
    }
    eprintln!("synoik is serving on {SOCKET}");

    let assets = limina_test::repo_root().join("crates/limina-test/assets");
    g1.scp_to_guest(&assets.join("webgl-still.html"), "/tmp/webgl-still.html")
        .expect("pushing the WebGL page into the guest");
    ssh_soft(&g1, "rm -rf /tmp/vkstill && mkdir -p /tmp/vkstill");
    for f in [
        "vkstill.c",
        "vkstill-spv.h",
        "xdg-shell.xml",
        "vkstill-build.sh",
    ] {
        g1.scp_to_guest(&assets.join(f), &format!("/tmp/vkstill/{f}"))
            .unwrap_or_else(|e| panic!("pushing {f} into the guest: {e}"));
    }
    let built = ssh_soft(&g1, "/tmp/vkstill/vkstill-build.sh 2>&1 | tail -3");
    assert!(
        built.contains("vkstill built"),
        "vkstill did not build in the guest:\n{built}"
    );

    launch_workload(&g1);
    std::thread::sleep(SETTLE);
    let procs_before = workload_procs(&g1);
    let vk_device = ssh_soft(
        &g1,
        "journalctl --user -u l2s-vkstill --no-pager 2>/dev/null | grep -m1 'vkstill: device' \
         || echo none",
    );
    eprintln!("workload up: {procs_before} | {vk_device}");
    assert!(
        vk_device.contains("Venus"),
        "vkstill is not running on venus ({vk_device}) — on a Vulkan compositor this test is \
         entirely about the venus world, and would be gating software rendering"
    );

    let pre = settled_capture(&g1, Duration::from_secs(90))
        .expect("the pre-suspend desktop never settled");
    let means_pre = cell_means(&pre);
    let pre_colors = color_diversity(&pre);
    eprintln!("pre-suspend: {pre_colors} distinct colours");
    assert!(
        pre_colors >= 200,
        "the live synoik desktop shows only {pre_colors} distinct colours — not a real seated \
         frame; there is nothing here to compare against"
    );
    let pid = std::process::id();
    let pre_png = std::env::temp_dir().join(format!("limina-synoik-pre-{pid}.png"));
    if let Some(p) = g1.display_capture_path() {
        let _ = std::fs::copy(p, &pre_png);
    }

    // --- Suspend through the production bracket ---
    //
    // Host side only: the bracket pulses the guest's suspend button and snapshots once it
    // quiesces, which is exactly what `limina suspend` does and what a synoik session was
    // observed to handle. No in-guest `systemctl suspend` — under synoik there is no session
    // manager arrangement to rely on, and an in-guest trigger would also DPMS-blank the
    // display, making a blank post-restore frame ambiguous between "blanked" and "lost".
    g1.suspend_bracket().expect("sending the suspend bracket");
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(180))
        .expect("supervisor did not exit after the suspend bracket");
    assert_eq!(
        outcome.code,
        Some(126),
        "the synoik guest should suspend (worker exit 126); got {outcome:?}"
    );

    // Oracle 2, read here because the snapshot happened inside the bracket above and g1's log
    // dies with it. This is the mechanism, stated in the worker's own words.
    let g1_log = g1.supervisor_log();
    let skipped: Vec<&str> = g1_log
        .lines()
        .filter(|l| l.contains("content read failed"))
        .collect();
    let census: Vec<&str> = g1_log
        .lines()
        .filter(|l| l.contains("gpu snapshot:") && l.contains("memory contents"))
        .collect();
    eprintln!("snapshot census: {}", census.last().unwrap_or(&"(none)"));
    for line in &skipped {
        eprintln!("SKIPPED AT SNAPSHOT: {}", &line[..line.len().min(190)]);
    }

    let scratch = g1.scratch_dir().to_path_buf();
    let snap: PathBuf = std::env::temp_dir().join(format!("limina-synoik-{pid}.snap"));
    let disk: PathBuf = std::env::temp_dir().join(format!("limina-synoik-{pid}.raw"));
    std::fs::copy(g1.snapshot_path().expect("snapshot path configured"), &snap)
        .expect("preserving the snapshot file");
    limina_test::cow_clone(&scratch.join("disk.raw"), &disk)
        .expect("preserving the suspended guest's disk");
    drop(g1);

    let snap_consumed = snap.with_extension("bin.consumed");
    let cleanup = || {
        if std::env::var_os("LIMINA_KEEP_ARTIFACTS").is_some() {
            eprintln!(
                "keeping artifacts: snap={} (or {}) disk={} pre-frame={}",
                snap.display(),
                snap_consumed.display(),
                disk.display(),
                pre_png.display()
            );
            return;
        }
        let _ = std::fs::remove_file(&snap);
        let _ = std::fs::remove_file(&snap_consumed);
        let _ = std::fs::remove_file(&disk);
    };

    // --- Guest 2: fresh worker restoring against the preserved disk ---
    let mut cfg2 = devices(base_cfg.clone())
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .restore_from(&snap);
    if let limina_test::Boot::Firmware { disk: d, .. } = &mut cfg2.boot {
        *d = disk.clone();
    }
    let mut g2 = Guest::boot(&cfg2).expect("spawning the restoring supervisor");
    g2.wait_for_supervisor_log("restoring from snapshot", Duration::from_secs(30))
        .unwrap_or_else(|e| {
            cleanup();
            panic!("restore worker never entered the restore path: {e}");
        });
    g2.wait_for_ssh_banner(Duration::from_secs(180))
        .unwrap_or_else(|e| {
            cleanup();
            panic!("restored guest never became reachable over SSH: {e}");
        });

    let procs_after = workload_procs(&g2);
    // A blank desktop is perfectly still, so this settles quickly on the failing path too —
    // it is not a proxy for health, only for "the frame we are about to judge is the frame the
    // guest means to show".
    let post = match settled_capture(&g2, Duration::from_secs(120)) {
        Ok(f) => f,
        Err(e) => {
            cleanup();
            panic!(
                "no settled post-restore frame: {e}\n\
                 workload procs: {procs_before} -> {procs_after}"
            );
        }
    };
    let means_post = cell_means(&post);
    let post_colors = color_diversity(&post);

    let size_ok = (post.width, post.height) == (pre.width, pre.height);
    let moved: Vec<usize> = if size_ok {
        (0..means_pre.len())
            .filter(|&i| cell_delta(means_pre[i], means_post[i]) > CELL_TOL)
            .collect()
    } else {
        (0..means_pre.len()).collect()
    };
    let moved_share = moved.len() as f64 / means_pre.len() as f64;
    eprintln!(
        "post-restore: procs={procs_after} landmarks moved {}/{} ({:.1}%) colours \
         {pre_colors} -> {post_colors}, {} allocation(s) skipped at snapshot",
        moved.len(),
        means_pre.len(),
        moved_share * 100.0,
        skipped.len()
    );

    let verdict_ok = size_ok
        && moved_share <= CELL_MISMATCH_BUDGET
        && post_colors * 4 >= pre_colors
        && skipped.is_empty()
        && !procs_after.contains("=0");
    if !verdict_ok {
        let post_png = std::env::temp_dir().join(format!("limina-synoik-post-{pid}.png"));
        if let Some(p) = g2.display_capture_path() {
            let _ = std::fs::copy(p, &post_png);
        }
        eprintln!(
            "pixel forensics: pre frame at {}, post frame at {}",
            pre_png.display(),
            post_png.display()
        );
        eprintln!("moved cells by grid row: {:?}", by_row(&moved));
        cleanup();
        panic!(
            "the Vulkan compositor's desktop did not come back the way it went in:\n\
             capture size:      {}x{} -> {}x{}\n\
             landmarks:         {}/{} moved ({:.1}%, budget {:.0}%)\n\
             colour diversity:  {pre_colors} -> {post_colors} (post must keep >= 1/4)\n\
             skipped at capture: {} allocation(s) the snapshot could not read\n\
             workload procs:    {procs_before} -> {procs_after}",
            pre.width,
            pre.height,
            post.width,
            post.height,
            moved.len(),
            means_pre.len(),
            moved_share * 100.0,
            CELL_MISMATCH_BUDGET * 100.0,
            skipped.len(),
        );
    }

    eprintln!(
        "the Vulkan compositor's desktop survived: {}/{} landmarks held, colours \
         {pre_colors} -> {post_colors}, nothing skipped at capture",
        means_pre.len() - moved.len(),
        means_pre.len()
    );
    let _ = std::fs::remove_file(&pre_png);
    cleanup();
}
