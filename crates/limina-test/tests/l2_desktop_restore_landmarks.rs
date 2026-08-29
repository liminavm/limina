// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — a working GNOME desktop must come back from a restore LOOKING THE SAME.
//!
//! The regression this exists for: a restored guest re-probed its scanout while the fresh
//! worker still carried virtio-gpu's *default* EDID ("Red Hat, Inc.", 10"), read 2560x1440 on
//! a 10" panel as 250% scale, and mutter constrained every window into the resulting
//! 1024x576 logical screen. The real EDID arrived about a second later and the scale came
//! back — but nothing moved the windows back, so every window on the desktop was left
//! displaced and resized. Fixed by carrying the per-scanout display configuration in the GPU
//! snapshot payload (v7) and re-applying it before the guest resumes.
//!
//! A restore is not a cold boot: the guest driver never went away and re-probes the instant
//! it resumes, so the fresh worker's DEVICE DEFAULTS are visible before any host-side push
//! can correct them. On a cold boot the same hazard is a race that the guest usually wins; on
//! a restore it bites every time. That is the class this test guards, and it is why the
//! oracles below are pre/post *equality* rather than health checks.
//!
//! # Why the workload is firefox + nautilus + vkmark, and why it is STILL
//!
//! Each covers a different way the desktop's pixels can be produced, and the failure modes are
//! not interchangeable:
//!
//! - **nautilus** — an idle GTK window. Static content, host-GL-only textures (icons, glyph
//!   atlases): the class that comes back as flat grey when a content re-upload is skipped.
//! - **firefox on a local WebGL page** — a client with its own GL context. The page draws one
//!   still frame (`assets/webgl-still.html`), so what it put on screen is a fixed picture.
//! - **vkstill** — a Vulkan client on venus, i.e. the other renderer entirely, drawing a
//!   detailed triangle and re-drawing exactly that triangle every frame
//!   (`assets/vkstill.c`, built in the guest at test time). A restore that preserves the
//!   classic vrend world and drops the venus one still passes every process oracle; it cannot
//!   pass this one.
//!
//!   It is ours because the image had nothing suitable. vkmark animates every scene but
//!   `clear`, and `clear` is a flat fill — it catches a surface that came back blank, not one
//!   that came back subtly wrong. Freezing an animated scene with `SIGSTOP` looked like the
//!   cheap way out and is not an option at all: the stopped client held out the suspend
//!   quiesce and the guest never suspended.
//!
//! **Nothing in the workload animates by design.** An animated region has to be excluded from
//! a pre/post comparison, and an excluded region is a region the test does not check — the
//! WebGL canvas and the Vulkan surface are precisely the pixels most worth checking. Holding
//! them still turns them from blind spots into the strongest landmarks on the screen.
//!
//! # The oracles
//!
//! 1. **The landmarks are unchanged.** This is the load-bearing one — the only oracle here
//!    that fires on the regression above. The frame is diced into a grid and every cell must
//!    still hold its colour after the restore. Windows that moved or resized change many cells
//!    at once; content that came back blank, grey or garbled changes them too. Two pre-suspend
//!    captures calibrate out whatever moved anyway (the panel clock), so the oracle needs no
//!    golden image and no hand-tuned mask.
//! 2. **Colour diversity survives.** The content-loss floor from `vrend_session_restore`: a
//!    desktop whose textures came back flat collapses this by orders of magnitude while every
//!    other oracle stays green.
//! 3. **The guest's EDID is unchanged.** The identity pushed before the snapshot has to still
//!    be the one the guest reads afterwards. Note this is only meaningful *because* the test
//!    pushes an identity the restored device's defaults do not carry — see
//!    [`pushed_identity`]; without that, both sides read the same default EDID whether the
//!    carry-over ran or not.
//!
//! Both captures are taken only once the capture file has stopped changing, so each side of
//! the comparison is a settled desktop rather than whatever frame a window animation happened
//! to leave behind.
//!
//! # RED lever
//!
//! `LIMINA_RESTORE_SKIP_DISPLAYS=1` makes the restore leave the fresh device's default display
//! configuration in place — the pre-fix behaviour exactly. Measured with it on: the guest's
//! EDID changes across the restore and **704 of 1000 landmarks move**, against a healthy run's
//! zero. Worth noting what did NOT fire on that run, because it is the whole argument for the
//! landmark oracle existing: every process stayed alive and colour diversity went 4087 -> 4093.
//! A desktop can come back wrong with its content perfectly intact.
//!
//! # What this test does NOT cover
//!
//! **A client that comes back alive but unable to render again.** Everything here is still by
//! construction, so a client whose GPU world died at the restore keeps its last good buffer on
//! screen and every oracle above passes. That shape is real — firefox nightly on the dogfood
//! Mac comes back wedged and a window resize does not heal it — and catching it needs a
//! deliberate post-restore redraw of a named client, which is the follow-up test's job.
//!
//! **A Vulkan compositor** (synoik). Its own framebuffers are device-local venus memory, and
//! the snapshot's content capture reads memory through `vkMapMemory`, which those allocations
//! refuse — so they come back blank and the desktop is empty until unrelated damage forces a
//! full repaint. Nothing here would catch that: mutter composites through classic vrend, whose
//! contents ride the v6 `classic_contents` dump. The two compositor paths fail independently,
//! which is exactly why they get two tests; this is the mutter one. See
//! `spikes/synoik-restore/RESULTS.md`.

use std::path::PathBuf;
use std::time::Duration;

use limina_test::landmarks::{
    by_row, cell_delta, cell_means, color_diversity, settled_capture, CELL_TOL,
};
use limina_test::{DisplayControl, EdidSpec, Guest, GuestConfig};

/// The monitor identity pushed into the guest before the snapshot, and the whole reason this
/// test can fail at all.
///
/// A restore leg boots the worker with the SAME `--display-size` as the suspend leg, so the
/// fresh device's defaults already match what the snapshot carries and the display carry-over
/// has nothing to do — with the RED lever on, the test still passed, at both 1280x800 and
/// 2560x1440. Pushing an identity the defaults do NOT have is what gives the snapshot
/// something to lose. 96 dpi over [`DISPLAY_W`]x[`DISPLAY_H`] describes a ~30" desktop panel,
/// so the guest's compositor settles on scale 1; virtio's default EDID claims 10", which at
/// the same pixel count reads as HiDPI and is what made it pick 250% and re-constrain every
/// window in the original fault.
fn pushed_identity() -> DisplayControl {
    DisplayControl {
        display_id: 0,
        position: None,
        size: Some((DISPLAY_W, DISPLAY_H)),
        connected: None,
        edid: Some(EdidSpec {
            refresh_hz: 60,
            dpi: 96,
            vendor: *b"LMN",
            product_id: 0x4C32,
            serial: 0x0000_0C2D,
            name: "L2 Landmark".into(),
            serial_string: Some("L2-LANDMARK".into()),
            range: None,
            modes: Vec::new(),
            alt_mode: None,
        }),
    }
}

/// Guest display size. Deliberately large: the regression this test guards needs a display
/// whose pixel count over virtio's default 10" EDID reads as a HiDPI panel, which is what made
/// the restored guest's compositor pick a 250% scale and re-constrain every window. At
/// 1280x800 the same broken restore picks scale 1.0, nothing is displaced, and the test passes
/// on code carrying the bug — measured, with the RED lever on. Size is part of the oracle here.
const DISPLAY_W: u32 = 2560;
const DISPLAY_H: u32 = 1440;

/// Share of landmark cells allowed to differ anyway. A healthy restore of this still workload
/// measures ZERO — the frame comes back pixel-identical — so this is pure headroom for the
/// panel clock ticking over a minute boundary mid-run, not a tolerance the pass relies on.
/// A displaced window changes cells by the hundred.
const CELL_MISMATCH_BUDGET: f64 = 0.01;

/// Minimum stable cells for the landmark oracle to mean anything. The workload is still by
/// construction, so nearly every cell should qualify; a run that does not reach this had
/// something moving on screen and is not measuring what it claims to.
const MIN_STABLE_CELLS: usize = 800;

/// Settle after the workload is launched: window-open animations finish, the WebGL page draws
/// its one frame, vkmark gets its cube on screen, and the shell stops compositing transitions.
const SETTLE: Duration = Duration::from_secs(20);

/// `ssh_exec` with retries: a loaded host drops the occasional connection right after the
/// banner poll succeeded.
fn ssh_retry(guest: &Guest, cmd: &str) -> String {
    let mut last_err = String::new();
    for _ in 0..4 {
        match guest.ssh_exec(cmd) {
            Ok(out) => return out.trim().to_string(),
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    panic!("ssh `{cmd}` kept failing: {last_err}");
}

/// The md5 of the connector's EDID blob as the GUEST kernel sees it — the fault's cause,
/// read where the guest reads it. Empty when no connector exposes one.
fn guest_edid_md5(guest: &Guest) -> String {
    ssh_retry(
        guest,
        "cat /sys/class/drm/card*-*/edid 2>/dev/null | md5sum | cut -d' ' -f1",
    )
}

/// Launch the three-app workload in the seated session. Each goes through `systemd-run --user`
/// so it outlives the ssh connection and its failures land in the journal.
fn launch_workload(guest: &Guest) {
    let env = "export XDG_RUNTIME_DIR=/run/user/1000 \
               DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus;";
    ssh_retry(
        guest,
        &format!(
            "{env} \
             systemd-run --user --collect --unit l2-naut \
               env WAYLAND_DISPLAY=wayland-0 nautilus >/dev/null 2>&1; \
             systemd-run --user --collect --unit l2-ff \
               env WAYLAND_DISPLAY=wayland-0 MOZ_ENABLE_WAYLAND=1 \
               firefox --new-window file:///tmp/webgl-still.html >/dev/null 2>&1; \
             systemd-run --user --collect --unit l2-vkstill \
               env WAYLAND_DISPLAY=wayland-0 /tmp/vkstill/vkstill >/dev/null 2>&1; \
             echo launched"
        ),
    );
}

/// Which of the workload's processes are alive, as a stable one-line summary.
fn workload_procs(guest: &Guest) -> String {
    ssh_retry(
        guest,
        "echo naut=$(pgrep -c nautilus || true) \
             ff=$(pgrep -fc lib64/firefox/firefox || true) \
             vkstill=$(pgrep -c vkstill || true)",
    )
}

#[test]
fn seated_gpu_workload_survives_restore_unchanged() {
    if !limina_test::require_hvf_or_skip("seated_gpu_workload_survives_restore_unchanged") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED seated_gpu_workload_survives_restore_unchanged: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    // EFI boot, not the injected-kernel seated path: the display configuration this test
    // gates travels over the guest's own kernel probing its own connectors, which is the
    // production shape. The injected path boots a test kernel with a different display story.
    let base_cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED seated_gpu_workload_survives_restore_unchanged: {e}");
            return;
        }
    };

    // Same device trims and pinned MAC as the other restore gates: virtio_i2c/virtio_snd hold
    // out the quiesce, and a fresh random MAC on the restore leg orphans the guest's cached
    // NIC identity.
    const NET_MAC: &str = "5a:94:ef:44:0f:ac";
    let devices = |cfg: GuestConfig| {
        cfg.with_supervisor_arg("--no-snd")
            .with_supervisor_arg("--no-battery")
            .with_net_mac(NET_MAC)
    };
    std::env::set_var("LIMINA_BRACKET_NO_BUTTON", "1");

    // --- Guest 1: seated desktop with the workload, snapshot-armed ---
    let cfg1 = devices(base_cfg.clone())
        .with_coexist_display(DISPLAY_W, DISPLAY_H)
        .with_net()
        .with_supervisor_log()
        .with_snapshot();
    eprintln!("booting the seated desktop (snapshot-armed)");
    let mut g1 = Guest::boot(&cfg1).expect("spawning the limina supervisor");
    g1.wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    g1.ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated session didn't come up");

    // Premise guard: the workload has to exist in the image, or every oracle below reduces to
    // a bare desktop and the gate silently stops covering what it claims to.
    let tools = ssh_retry(
        &g1,
        "for t in nautilus firefox cc wayland-scanner pkg-config; do \
             command -v $t >/dev/null || echo MISSING:$t; done; echo ok",
    );
    assert!(
        !tools.contains("MISSING:"),
        "the L2 image is missing part of the workload ({tools}) — install it in the image \
         rather than weakening this test"
    );

    let assets = limina_test::repo_root().join("crates/limina-test/assets");
    g1.scp_to_guest(&assets.join("webgl-still.html"), "/tmp/webgl-still.html")
        .expect("pushing the WebGL page into the guest");
    // vkstill is built in the guest rather than shipped: it needs only a C compiler and the
    // guest's own libwayland, so the canonical test image stays untouched by this test.
    ssh_retry(&g1, "rm -rf /tmp/vkstill && mkdir -p /tmp/vkstill");
    for f in [
        "vkstill.c",
        "vkstill-spv.h",
        "xdg-shell.xml",
        "vkstill-build.sh",
    ] {
        g1.scp_to_guest(&assets.join(f), &format!("/tmp/vkstill/{f}"))
            .unwrap_or_else(|e| panic!("pushing {f} into the guest: {e}"));
    }
    let built = ssh_retry(&g1, "/tmp/vkstill/vkstill-build.sh 2>&1 | tail -3");
    assert!(
        built.contains("vkstill built"),
        "vkstill did not build in the guest:\n{built}"
    );

    launch_workload(&g1);
    std::thread::sleep(SETTLE);
    // Give the guest a monitor identity the restored device will not have by default, then let
    // its compositor finish re-laying-out for it before anything is measured.
    g1.update_display(pushed_identity())
        .expect("pushing the monitor identity");
    std::thread::sleep(Duration::from_secs(10));
    let procs_before = workload_procs(&g1);
    // Name the driver vkstill actually got: on a guest that quietly fell back to lavapipe the
    // venus half of this test would be checking software rendering and pass regardless.
    let vk_device = ssh_retry(
        &g1,
        "journalctl --user -u l2-vkstill --no-pager 2>/dev/null | grep -m1 'vkstill: device' \
         || echo none",
    );
    eprintln!("workload up: {procs_before} | {vk_device}");
    assert!(
        vk_device.contains("Venus"),
        "vkstill is not running on venus ({vk_device}) — the Vulkan half of this test would \
         be gating software rendering"
    );
    assert!(
        !procs_before.contains("=0"),
        "part of the workload never started ({procs_before}) — the pre-suspend desktop is not \
         the one this test means to compare against"
    );

    // The reference frame, taken once the desktop's pixels have stopped changing, plus a second read
    // a few seconds later. The workload is still, so the two should agree everywhere except
    // where something moved anyway — the panel clock — and those cells drop out of the
    // comparison rather than being tolerated in it.
    let pre_a =
        settled_capture(&g1, Duration::from_secs(90)).expect("pre-suspend capture never settled");
    std::thread::sleep(Duration::from_secs(5));
    let pre_b = g1
        .read_capture()
        .expect("no second pre-suspend scanout capture");
    assert_eq!(
        (pre_a.width, pre_a.height),
        (pre_b.width, pre_b.height),
        "the two pre-suspend captures disagree on size"
    );

    let means_a = cell_means(&pre_a);
    let means_b = cell_means(&pre_b);
    let stable: Vec<usize> = (0..means_a.len())
        .filter(|&i| cell_delta(means_a[i], means_b[i]) <= CELL_TOL)
        .collect();
    let pre_colors = color_diversity(&pre_b);
    eprintln!(
        "pre-suspend landmarks: {} of {} cells stable, {pre_colors} distinct colours",
        stable.len(),
        means_a.len()
    );
    assert!(
        stable.len() >= MIN_STABLE_CELLS,
        "only {} of {} grid cells held still across two settled frames (need \
         {MIN_STABLE_CELLS}) — something on this desktop is animating, and an animated cell \
         is a cell the comparison cannot check",
        stable.len(),
        means_a.len()
    );
    assert!(
        pre_colors >= 200,
        "the live desktop capture shows only {pre_colors} distinct colours — not a real \
         seated frame; the content floor can't gate on this baseline"
    );

    // Keep the reference frame for failure forensics (g1's scratch dies with it).
    let pid = std::process::id();
    let pre_png = std::env::temp_dir().join(format!("limina-landmark-pre-{pid}.png"));
    if let Some(p) = g1.display_capture_path() {
        let _ = std::fs::copy(p, &pre_png);
    }

    let edid_before = guest_edid_md5(&g1);
    let boot_id = ssh_retry(&g1, "cat /proc/sys/kernel/random/boot_id");
    eprintln!("pre-suspend: edid_md5={edid_before} boot_id={boot_id}");
    assert!(
        !edid_before.is_empty(),
        "the guest exposes no connector EDID — the display-configuration oracle has nothing \
         to compare"
    );

    // --- Suspend through the production bracket (in-guest trigger, no button) ---
    ssh_retry(
        &g1,
        "sudo systemd-run --on-active=2 systemctl suspend -i >/dev/null 2>&1; echo armed",
    );
    g1.suspend_bracket().expect("sending the suspend bracket");
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(120))
        .expect("supervisor did not exit after the suspend bracket");
    assert_eq!(
        outcome.code,
        Some(126),
        "the seated guest should suspend (worker exit 126); got {outcome:?}\n\
         === supervisor+worker log ===\n{}",
        g1.supervisor_log()
    );

    let scratch = g1.scratch_dir().to_path_buf();
    let snap: PathBuf = std::env::temp_dir().join(format!("limina-landmark-{pid}.snap"));
    let disk: PathBuf = std::env::temp_dir().join(format!("limina-landmark-{pid}.raw"));
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
        .with_coexist_display(DISPLAY_W, DISPLAY_H)
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
    g2.wait_for_ssh_banner(Duration::from_secs(120))
        .unwrap_or_else(|e| {
            eprintln!("--- restore supervisor log tail ---");
            let slog = g2.supervisor_log();
            for line in slog.lines().rev().take(25).collect::<Vec<_>>().iter().rev() {
                eprintln!("{line}");
            }
            cleanup();
            panic!("restored guest never became reachable over SSH: {e}");
        });

    let boot_id_after = ssh_retry(&g2, "cat /proc/sys/kernel/random/boot_id");
    if boot_id_after != boot_id {
        cleanup();
        panic!("boot_id changed across restore — the guest rebooted instead of resuming");
    }

    // The suspend DPMS-blanked the display and a headless L2 has no real input to wake it; a
    // blanked compositor issues no frame callbacks, so its clients go idle and the capture
    // would read a legitimately dark desktop. Wake it, then let the workload settle back into
    // the same steady state the reference frames were taken in.
    ssh_retry(
        &g2,
        "export XDG_RUNTIME_DIR=/run/user/1000 \
         DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; \
         busctl --user call org.gnome.ScreenSaver /org/gnome/ScreenSaver \
         org.gnome.ScreenSaver SimulateUserActivity >/dev/null 2>&1; echo woke",
    );
    std::thread::sleep(SETTLE);

    let edid_after = guest_edid_md5(&g2);
    let procs_after = workload_procs(&g2);
    // The restored desktop, once its pixels have stopped changing. It reaches this state through a
    // full repaint out of DPMS blank, so the frame is genuinely re-composited rather than a
    // host-side leftover — and the capture file lives in g2's own scratch, so it cannot be
    // g1's frame either.
    let post =
        settled_capture(&g2, Duration::from_secs(90)).expect("post-restore capture never settled");
    let post_colors = color_diversity(&post);

    let size_ok = (post.width, post.height) == (pre_a.width, pre_a.height);
    let means_post = if size_ok {
        cell_means(&post)
    } else {
        Vec::new()
    };
    let moved: Vec<usize> = stable
        .iter()
        .copied()
        .filter(|&i| !size_ok || cell_delta(means_a[i], means_post[i]) > CELL_TOL)
        .collect();
    let moved_share = moved.len() as f64 / stable.len() as f64;

    eprintln!(
        "post-restore: edid_md5={edid_after} procs={procs_after} \
         landmarks moved {}/{} ({:.1}%) colours {pre_colors} -> {post_colors}",
        moved.len(),
        stable.len(),
        moved_share * 100.0
    );

    let verdict_ok = edid_after == edid_before
        && size_ok
        && moved_share <= CELL_MISMATCH_BUDGET
        && post_colors * 4 >= pre_colors
        && !procs_after.contains("=0");
    if !verdict_ok {
        let post_png = std::env::temp_dir().join(format!("limina-landmark-post-{pid}.png"));
        if let Some(p) = g2.display_capture_path() {
            let _ = std::fs::copy(p, &post_png);
        }
        eprintln!(
            "pixel forensics: pre frame at {}, post frame at {}",
            pre_png.display(),
            post_png.display()
        );
        // Name where the landmarks moved: a displaced window clusters, a blanked desktop
        // spreads everywhere. The rows tell those apart at a glance.
        eprintln!("moved cells by grid row: {:?}", by_row(&moved));
        // The display table's own account of what the restored device answered.
        let log = g2.supervisor_log();
        for line in log
            .lines()
            .filter(|l| l.contains("display") || l.contains("gpu restore:"))
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .iter()
            .rev()
        {
            eprintln!("{}", &line[..line.len().min(190)]);
        }
        cleanup();
        panic!(
            "the desktop did not come back the way it went in:\n\
             guest EDID md5:   {edid_before} -> {edid_after} (must be equal)\n\
             capture size:     {}x{} -> {}x{}\n\
             stable landmarks: {}/{} moved ({:.1}%, budget {:.0}%)\n\
             colour diversity: {pre_colors} -> {post_colors} (post must keep >= 1/4)\n\
             workload procs:   {procs_before} -> {procs_after}",
            pre_a.width,
            pre_a.height,
            post.width,
            post.height,
            moved.len(),
            stable.len(),
            moved_share * 100.0,
            CELL_MISMATCH_BUDGET * 100.0,
        );
    }

    eprintln!(
        "desktop restored unchanged: EDID stable, {}/{} landmarks held, colours \
         {pre_colors} -> {post_colors}",
        stable.len() - moved.len(),
        stable.len()
    );
    let _ = std::fs::remove_file(&pre_png);
    cleanup();
}
