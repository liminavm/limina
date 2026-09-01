// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — a **Vulkan compositor's** desktop must come back from a restore with its pixels.
//!
//! # The bug this is written against (OPEN — this test is expected to FAIL)
//!
//! A restore re-uploads each classic (vrend) resource's content by rebuilding its transfer box
//! from the resource's *create* dimensions with a zero stride and offset, then pushing the
//! guest backing store in. vrend refuses the transfer whenever that box does not fit the
//! resource or the derived IOV does not match the backing — `resource_contains_box` /
//! `check_iov_bounds` — and every refused resource comes back carrying **no content**:
//!
//! ```text
//! vrend_renderer_transfer_internal: context error reported 0 "HOST" IOV data size exceeds \
//!   resource capacity 328
//! ```
//!
//! Measured on every restore of every session tried, healthy-looking ones included: 137–268
//! refusals on a local poke VM, 244 on the dogfood Mac. The visible symptom is a window that
//! is blank until something repaints it, which is why a cycle can look clean — anything that
//! redraws itself hides its own loss, and incidental damage (a notification, a workspace
//! switch) heals the rest before anyone looks.
//!
//! A second, narrower gap sits behind it: the venus snapshot captures a `VkDeviceMemory` by
//! `vkMapMemory` + `memcpy` (`vkr_device_memory_content_copy`), so a device-local allocation —
//! the compositor's own scanout images — is skipped with a `warn!`. Those pixels live only in
//! host heaps, never in the RAM snapshot. Both are asserted here; the first is the one that has
//! been costing windows. Dossier and frames: `spikes/synoik-restore/RESULTS.md`.
//!
//! # Why this cannot be folded into `l2_desktop_restore_landmarks`
//!
//! That test drives mutter. Two compositors reach the same restore through different paths, and
//! the same snapshot can be complete for one and partial for the other — measured on one day,
//! the mutter test restored 1000 of 1000 landmark cells while a synoik session went blank. Two
//! compositors, two tests.
//!
//! # What a failure here does and does not mean
//!
//! This gates content that comes back missing and heals on repaint. It does NOT cover a client
//! that comes back **wedged** and stays broken through a repaint, nor a restored session that
//! paints nothing at all until a new client arrives — both have been seen. A fix that turns
//! this test green is not evidence about either.
//!
//! # Oracles
//!
//! 1. **The landmarks are unchanged.** A still workload, a settled frame each side, per-cell
//!    comparison. The workload holds still on purpose so that every pixel of it is checkable.
//! 2. **Nothing was skipped at snapshot time** — `content read failed` in the snapshotting
//!    worker's log. A partial capture that happens not to show today is still partial.
//! 3. **Colour diversity survives** — the content floor. A blanked desktop collapses it.
//! 4. **Every classic resource got its content back** — the restoring worker's own count of
//!    refused re-uploads. This is the only oracle here that does not depend on the lost pixels
//!    landing somewhere visible in this run's frame, which is exactly why it belongs: the frame
//!    diff alone called several genuinely broken restores clean.
//! 5. **Every workload process is still alive**, on both sides. Weak on its own — a wedged
//!    client stays alive — but it catches a client that died rather than lost its pixels.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use limina_test::landmarks::{
    by_row, cell_delta, cell_means, color_diversity, settled_capture, CELL_TOL, GRID_COLS,
    GRID_ROWS,
};
use limina_test::{CapturedFrame, Guest, GuestConfig};

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
/// Judged over the desktop BODY only — see [`PANEL_BAND_ROWS`].
const CELL_MISMATCH_BUDGET: f64 = 0.01;

/// Grid rows the translucent top panel occupies, judged separately from the body.
///
/// The panel's blurred backdrop does not come back bit-identical across a restore. The
/// difference is real but sub-perceptual — smooth, low-frequency, max channel delta ~54, at
/// zero vertical displacement, so the content is in place and merely *rendered* differently.
/// The blur then spreads it across the entire band, and the clock and status icons join the
/// diff only because their antialiasing blends against a changed backdrop. Measured at 36 of
/// these 80 cells, tripping a 1% whole-frame budget about half the time while being
/// indistinguishable to a human comparing the two crops side by side.
///
/// So the band gets its own rule. NOT because drift there is uninteresting, but because a
/// restore that actually fails to draw does far more damage than one band of soft glow — it
/// blanks the desktop, loses a window, or parks the compositor on a stale frame, and the body
/// budget, the content-loss lines, the ring-stall oracle and colour diversity all still see
/// that at full sensitivity.
///
/// What the band still asserts is that it is a live, populated panel: its own colour diversity
/// must survive. Excluding the cells without that check would let the panel vanish entirely for
/// free, since at 8% of the frame its loss does not move global diversity enough to trip the
/// whole-frame guard.
const PANEL_BAND_ROWS: u32 = 2;

/// Distinct quantised colours in the top [`PANEL_BAND_ROWS`] of the grid — the band's own
/// version of `color_diversity`, which is what tells "the blur resolved differently" apart
/// from "the panel is gone".
fn band_diversity(frame: &CapturedFrame) -> usize {
    let band_h = (frame.height * PANEL_BAND_ROWS / GRID_ROWS).max(1);
    let mut seen = std::collections::HashSet::new();
    for y in 0..band_h.min(frame.height) {
        for x in (0..frame.width).step_by(4) {
            let px = frame.pixel(x, y);
            seen.insert(
                ((px[0] as u16 >> 4) << 8) | ((px[1] as u16 >> 4) << 4) | (px[2] as u16 >> 4),
            );
        }
    }
    seen.len()
}

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

/// GTK apps to seat alongside the browser windows. Each one is a **classic (vrend) context**
/// whose window is drawn from textures uploaded once — icon atlases, glyph caches — which is
/// the content class the restore's classic re-upload is responsible for. One app is a thin
/// sample of that class; several, of different shapes, is a workload.
const GTK_APPS: &[&str] = &["nautilus", "gnome-text-editor", "gnome-calculator"];

/// Which of [`GTK_APPS`] this image actually ships. Discovered rather than assumed: a missing
/// package must weaken the gate visibly, not silently.
fn present_gtk_apps(guest: &Guest) -> Vec<String> {
    GTK_APPS
        .iter()
        .filter(|a| !ssh_soft(guest, &format!("command -v {a} || true")).is_empty())
        .map(|a| a.to_string())
        .collect()
}

/// A still desktop with something of every kind on it: GTK windows (classic contexts drawn
/// from once-uploaded atlases), two browser windows — one holding a finished WebGL frame, one
/// pure glyph-heavy text — and our own Vulkan client redrawing one fixed triangle. Everything
/// holds still on purpose, so every pixel of it is a landmark.
fn launch_workload(guest: &Guest, apps: &[String]) {
    let env = "export XDG_RUNTIME_DIR=/run/user/1000 \
               DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus;";
    let mut cmd = String::from(env);
    for app in apps {
        cmd.push_str(&format!(
            " systemd-run --user --collect --unit l2s-{app} \
               env WAYLAND_DISPLAY={SOCKET} {app} >/dev/null 2>&1;"
        ));
    }
    // Both browser windows belong to ONE firefox: the second `--new-window` joins the running
    // instance. That is deliberate — two surfaces from one client is the shape that showed the
    // fault is inside a client's compositing, not a lost buffer.
    cmd.push_str(&format!(
        " systemd-run --user --collect --unit l2s-ff \
            env WAYLAND_DISPLAY={SOCKET} MOZ_ENABLE_WAYLAND=1 \
            firefox --new-window file:///tmp/webgl-still.html >/dev/null 2>&1; \
          sleep 20; \
          systemd-run --user --collect --unit l2s-ff2 \
            env WAYLAND_DISPLAY={SOCKET} MOZ_ENABLE_WAYLAND=1 \
            firefox --new-window file:///tmp/still-blocks.html >/dev/null 2>&1; \
          systemd-run --user --collect --unit l2s-vkstill \
            env WAYLAND_DISPLAY={SOCKET} /tmp/vkstill/vkstill >/dev/null 2>&1; \
          echo launched"
    ));
    ssh_soft(guest, &cmd);
}

fn workload_procs(guest: &Guest, apps: &[String]) -> String {
    let mut probe = String::from("echo");
    for app in apps {
        // -f, matching the whole command line: `pgrep NAME` compares against a comm field
        // truncated to 15 characters, so `gnome-text-editor` and `gnome-calculator` matched
        // NOTHING and reported 0 on both sides of the restore — a liveness oracle that could
        // never see a client die, and a `=0` in the verdict string on every run.
        probe.push_str(&format!(" {app}=$(pgrep -fc /usr/bin/{app} || true)"));
    }
    probe.push_str(
        " ff=$(pgrep -fc lib64/firefox/firefox || true) \
          vkstill=$(pgrep -c vkstill || true) \
          synoik=$(pgrep -x synoik || true)",
    );
    ssh_soft(guest, &probe)
}

/// Every way the classic (vrend) content path can lose a resource's pixels, in the workers'
/// own words. Each line means at least one texture comes back empty until something repaints
/// it — which is why this is asserted directly and not only through the frame diff.
///
/// Three mechanisms, deliberately covered together because each one alone has been the whole
/// story at some point in this investigation and none of them was visible in a default log:
///
/// - `re-upload FAILED` — libkrun's guest-shadow transfers, which rebuild a full-level box
///   from the resource's create dimensions and are refused when it does not fit the backing.
/// - `content export ... SKIPPED` — the snapshot's own per-resource GL readback gave up on a
///   resource, so the snapshot never carried its pixels at all.
/// - `content restore ... DROPPED` — the restore had the pixels and could not put them back.
///   `vrend_renderer_restore_ctx_contents` returns success regardless, so this log line is the
///   only account of it.
/// - `vkMapMemory REFUSED` — a venus VkDeviceMemory the snapshot wanted and could not read. The
///   client window buffers land here: the compositor samples them, so their absence is a blank
///   window that only the client's own next repaint fills in.
fn content_losses(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| {
            l.contains("classic content re-upload FAILED")
                || l.contains("classic content restore failed")
                || l.contains("content export ctx") && l.contains("SKIPPED")
                || l.contains("content restore ctx") && l.contains("DROPPED")
                || l.contains("vkMapMemory REFUSED")
        })
        .collect()
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

    // Before anything is captured: stop the desktop doing things of its own accord. An
    // update check that ends in a notification banner is drawn over the top of the screen,
    // which is exactly where this test's landmarks are densest.
    limina_test::quiesce_desktop(&g1);

    let assets = limina_test::repo_root().join("crates/limina-test/assets");
    for page in ["webgl-still.html", "still-blocks.html"] {
        g1.scp_to_guest(&assets.join(page), &format!("/tmp/{page}"))
            .unwrap_or_else(|e| panic!("pushing {page} into the guest: {e}"));
    }
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

    let apps = present_gtk_apps(&g1);
    assert!(
        apps.len() >= 2,
        "this image ships only {apps:?} of {GTK_APPS:?} — the classic-context half of the \
         workload would be a token one, and a pass would mean less than it claims"
    );
    eprintln!("GTK apps present: {apps:?}");
    launch_workload(&g1, &apps);
    std::thread::sleep(SETTLE);
    let procs_before = workload_procs(&g1, &apps);
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

    let procs_after = workload_procs(&g2, &apps);
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

    // Oracle 4: every classic resource got its content back. Read from the RESTORING worker's
    // log, since this is a restore-side failure. It is the one oracle here that does not
    // depend on the lost pixels happening to be visible in this run's frame.
    let g2_log = g2.supervisor_log();
    let mut lost: Vec<String> = content_losses(&g1_log)
        .iter()
        .map(|l| format!("at snapshot: {l}"))
        .collect();
    lost.extend(
        content_losses(&g2_log)
            .iter()
            .map(|l| format!("at restore: {l}")),
    );
    for line in &lost {
        eprintln!("CONTENT LOST: {}", &line[..line.len().min(190)]);
    }

    // Oracle 5: the venus rings still consume. A ring that went FATAL stops draining, the
    // guest's vn_ring_wait_seqno never advances, and the compositor parks in it forever —
    // holding the last frame it presented, which is a CORRECT picture. So this has to be
    // asserted on its own: the landmark diff passes on exactly that dead session.
    let stalls = limina_test::ring_stalls(&g2_log);
    for line in &stalls {
        eprintln!("RING STALLED: {}", &line[..line.len().min(190)]);
    }

    let size_ok = (post.width, post.height) == (pre.width, pre.height);
    let moved: Vec<usize> = if size_ok {
        (0..means_pre.len())
            .filter(|&i| cell_delta(means_pre[i], means_post[i]) > CELL_TOL)
            .collect()
    } else {
        (0..means_pre.len()).collect()
    };
    // A clock in the panel advances whether or not the restore worked, and this test runs
    // long enough to cross a minute boundary. Rather than pay a minute measuring that every
    // run, or blind the comparison over a hardcoded panel region, find out which cells move
    // on their own only when the verdict would otherwise be a failure. A genuinely broken
    // desktop moves cells that are not volatile, so it still fails.
    let mut moved = moved;
    let mut volatile_excluded = 0usize;
    if moved.len() as f64 / means_pre.len() as f64 > CELL_MISMATCH_BUDGET {
        match limina_test::landmarks::volatile_cells(&g2) {
            Ok(volatile) => {
                let before = moved.len();
                moved.retain(|i| !volatile.contains(i));
                volatile_excluded = before - moved.len();
                eprintln!(
                    "{} of {} moved cell(s) also move on a still desktop with only time \
                     passing (a clock, most likely); judging on the remaining {}",
                    volatile_excluded,
                    before,
                    moved.len()
                );
            }
            Err(e) => eprintln!("could not measure which cells move on their own: {e}"),
        }
    }
    // The translucent panel's blur is judged on whether it is still a live panel, the body on
    // whether its pixels held. See PANEL_BAND_ROWS for why the two cannot share one budget.
    let (band_moved, body_moved): (Vec<usize>, Vec<usize>) = moved
        .iter()
        .partition(|&&i| (i as u32) / GRID_COLS < PANEL_BAND_ROWS);
    let body_cells = means_pre.len() - (PANEL_BAND_ROWS * GRID_COLS) as usize;
    let moved_share = body_moved.len() as f64 / body_cells as f64;
    let band_pre = band_diversity(&pre);
    let band_post = band_diversity(&post);
    let band_alive = band_post * 4 >= band_pre;
    eprintln!(
        "post-restore: procs={procs_after} landmarks moved {}/{} body ({:.1}%) + {} in the \
         panel band (blur, band colours {band_pre} -> {band_post}), colours \
         {pre_colors} -> {post_colors}, {} allocation(s) skipped at snapshot, {} content-loss \
         line(s)",
        body_moved.len(),
        body_cells,
        moved_share * 100.0,
        band_moved.len(),
        skipped.len(),
        lost.len()
    );

    let verdict_ok = size_ok
        && moved_share <= CELL_MISMATCH_BUDGET
        && band_alive
        && post_colors * 4 >= pre_colors
        && skipped.is_empty()
        && lost.is_empty()
        && stalls.is_empty()
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
             landmarks:         {}/{} body cells moved ({:.1}%, budget {:.0}%)\n\
             panel band:        {} cell(s) moved, band colours {band_pre} -> {band_post} \
(must keep >= 1/4)\n\
             self-moving cells: {} excluded (they move with only time passing)\n\
             colour diversity:  {pre_colors} -> {post_colors} (post must keep >= 1/4)\n\
             skipped at capture: {} allocation(s) the snapshot could not read\n\
             content loss:      {} line(s) naming resources that lost their pixels\n\
             ring stalls:       {} venus ring(s) stopped consuming after the restore\n\
             workload procs:    {procs_before} -> {procs_after}",
            pre.width,
            pre.height,
            post.width,
            post.height,
            body_moved.len(),
            body_cells,
            moved_share * 100.0,
            CELL_MISMATCH_BUDGET * 100.0,
            band_moved.len(),
            volatile_excluded,
            skipped.len(),
            lost.len(),
            stalls.len(),
        );
    }

    eprintln!(
        "the Vulkan compositor's desktop survived: {}/{} landmarks held, colours \
         {pre_colors} -> {post_colors}, nothing skipped at capture, every classic resource \
         got its content back",
        body_cells - body_moved.len(),
        body_cells
    );
    let _ = std::fs::remove_file(&pre_png);
    cleanup();
}
