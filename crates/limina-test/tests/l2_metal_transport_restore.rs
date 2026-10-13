// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A Vulkan client on the native-context transport must survive a snapshot restore as the same
//! process, still rendering.
//!
//! The transport's state is spread over three owners: the replayer's objects and arenas, the
//! extension's books in the device (contexts, rings, arena mappings, the window surfaces a context
//! made), and the classic renderer, which the compositor imports those surfaces into. A restore
//! that brings back two of the three looks fine to every process oracle: the client is still
//! there, the compositor is still there, and the first frame that touches what was lost either
//! poisons the compositor's context or leaves the window frozen. So this gate asserts, besides the
//! restore's own report, that the client's frames keep changing after the resume.
//!
//! The vehicle is `metal.test`: the stock-kernel seated desktop with the extended virtio-gpu
//! module and KosmicKrisp on the limina bridge (`scripts/provision/deliver-metal-transport.sh`).
//! KosmicKrisp is selected by `VK_DRIVER_FILES` alone; its ICD is not in the loader's path.
//!
//! The renderer replay fails the restore on any command it drops (`gpu restore: … the replay
//! dropped N commands` → `replay FAILED`), so the restore's own verdict is load-bearing here: a
//! classic context losing the type of a surface the extension made is the failure this caught
//! first.

use std::path::PathBuf;
use std::time::Duration;

use limina_test::{CapturedFrame, Guest, GuestConfig};

const TEST: &str = "metal_transport_client_survives_snapshot_restore";

/// Where `deliver-metal-transport.sh` puts KosmicKrisp's ICD; nothing else selects it.
const ICD: &str = "/usr/local/share/limina/vulkan/kk_limina_icd.json";

/// Runs what follows in the seated user's session.
const SESSION: &str = "env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 \
                       DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus";

/// Wakes a display the in-guest suspend blanked: a blanked compositor sends no frame callbacks,
/// and a FIFO client then stops presenting, which would read as a frozen window.
const WAKE: &str = "busctl --user call org.gnome.ScreenSaver /org/gnome/ScreenSaver \
                    org.gnome.ScreenSaver SimulateUserActivity";

/// Pixels two captures must differ in for the window to count as animating. A turning cube
/// changes thousands per second at this display size; the clock changing its minute, the only
/// other motion on an idle desktop, changes a few hundred.
const MIN_CHANGED_PIXELS: usize = 1500;

/// How far apart the two captures of each pair are taken. The capture is rewritten at most once
/// a second, so this spans several.
const CAPTURE_GAP: Duration = Duration::from_secs(4);

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

/// Distinct quantized colors (4 bits/channel, every 4th pixel): a black or flat frame has few.
fn color_diversity(frame: &CapturedFrame) -> usize {
    let mut seen = std::collections::HashSet::new();
    for px in frame.rgba.as_chunks::<4>().0.iter().step_by(4) {
        let key = ((px[0] as u16 >> 4) << 8) | ((px[1] as u16 >> 4) << 4) | (px[2] as u16 >> 4);
        seen.insert(key);
    }
    seen.len()
}

fn changed_pixels(a: &CapturedFrame, b: &CapturedFrame) -> usize {
    assert_eq!(
        (a.width, a.height),
        (b.width, b.height),
        "the capture changed size between two reads"
    );
    a.rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.rgba.as_chunks::<4>().0)
        .filter(|(p, q)| p != q)
        .count()
}

/// Two captures `CAPTURE_GAP` apart: how many pixels changed, and the later frame.
fn animation(g: &Guest, leg: &str) -> (usize, CapturedFrame) {
    let first = g
        .read_capture()
        .unwrap_or_else(|e| panic!("{leg}: no scanout capture: {e}"));
    std::thread::sleep(CAPTURE_GAP);
    let second = g
        .read_capture()
        .unwrap_or_else(|e| panic!("{leg}: no scanout capture: {e}"));
    (changed_pixels(&first, &second), second)
}

/// The vkcube processes in the guest, oldest first.
fn vkcube_pid(g: &Guest) -> String {
    ssh_retry(g, "pgrep -xo vkcube || true")
}

#[test]
fn metal_transport_client_survives_snapshot_restore() {
    if !limina_test::require_hvf_or_skip(TEST) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED {TEST}: no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk (the compositor's \
             host GL needs it)"
        );
        return;
    }
    let base_cfg = match GuestConfig::metal_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED {TEST}: {e}");
            return;
        }
    };

    // As the vrend restore gate: virtio_i2c/virtio_snd hold out the s2idle quiesce, and a fresh
    // MAC on the restore leg orphans the guest's cached NIC identity.
    const NET_MAC: &str = "5a:94:ef:44:0f:ac";
    let devices = |cfg: GuestConfig| {
        cfg.with_supervisor_arg("--no-snd")
            .with_supervisor_arg("--no-battery")
            .with_net_mac(NET_MAC)
    };
    // The bracket must not double-trigger the in-guest suspend; the restore's report lines are
    // the device's info lines.
    unsafe { std::env::set_var("LIMINA_BRACKET_NO_BUTTON", "1") };
    unsafe { std::env::set_var("RUST_LOG", "info") };

    // --- Guest 1: the seated desktop with a client on the transport ---
    let cfg1 = devices(base_cfg.clone())
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .with_snapshot();
    let mut g1 = Guest::boot(&cfg1).expect("spawning the limina supervisor");
    let banner = g1
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");
    g1.ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .unwrap_or_else(|e| panic!("gnome-shell never appeared: {e}"));

    // The premise: the extended module bound, it found the extension, and KosmicKrisp answers on
    // it. Without these the rest would test venus or nothing.
    let missing = "— deliver the transport with scripts/provision/deliver-metal-transport.sh";
    let module = ssh_retry(&g1, "modinfo -n virtio_gpu");
    let krel = ssh_retry(&g1, "uname -r");
    assert_eq!(
        module,
        format!("/lib/modules/{krel}/updates/virtio-gpu.ko"),
        "virtio_gpu is not the extended module {missing}"
    );
    let ext = ssh_retry(&g1, "sudo dmesg | grep -c 'limina extension:' || true");
    assert_ne!(
        ext, "0",
        "the module did not find the limina extension (is LIMINA_GPU_METAL reaching the worker?)"
    );
    let summary = ssh_retry(
        &g1,
        &format!("VK_DRIVER_FILES={ICD} timeout 30 vulkaninfo --summary 2>&1 || true"),
    );
    assert!(
        summary.contains("Apple M1 Max") || summary.contains("deviceName         = Apple"),
        "KosmicKrisp on the transport lists no Apple GPU {missing}:\n{summary}"
    );
    assert!(
        summary.contains("driverName         = KosmicKrisp"),
        "the selected driver is not KosmicKrisp {missing}:\n{summary}"
    );

    // Settle the desktop, then start the client.
    std::thread::sleep(Duration::from_secs(10));
    ssh_retry(
        &g1,
        &format!(
            "{SESSION} VK_DRIVER_FILES={ICD} setsid nohup vkcube --wsi wayland \
             > /tmp/vkcube.log 2>&1 < /dev/null & echo started"
        ),
    );
    g1.ssh_poll(
        "grep -q 'Selected GPU' /tmp/vkcube.log",
        Duration::from_secs(60),
    )
    .unwrap_or_else(|e| panic!("vkcube never selected a GPU: {e}"));
    let selected = ssh_retry(&g1, "grep 'Selected GPU' /tmp/vkcube.log");
    assert!(
        selected.contains("Apple M1 Max"),
        "vkcube is not on KosmicKrisp: {selected}"
    );
    let cube = vkcube_pid(&g1);
    assert!(!cube.is_empty(), "vkcube exited after selecting its GPU");
    std::thread::sleep(Duration::from_secs(5));

    let (pre_changed, pre_frame) = animation(&g1, "pre-suspend");
    let pre_colors = color_diversity(&pre_frame);
    eprintln!(
        "pre-suspend: vkcube pid {cube} on {selected:?}; {pre_changed} pixels changed in \
         {CAPTURE_GAP:?}; {pre_colors} distinct colors"
    );
    assert!(
        pre_changed >= MIN_CHANGED_PIXELS,
        "the live cube is not animating ({pre_changed} pixels changed) — the oracle cannot gate \
         the restore on this baseline"
    );
    assert!(
        pre_colors >= 200,
        "the live desktop shows only {pre_colors} distinct colors — not a seated frame"
    );
    let boot_id = ssh_retry(&g1, "cat /proc/sys/kernel/random/boot_id");

    // --- Suspend through the production bracket ---
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
        "the guest should suspend (worker exit 126); got {outcome:?}\n{}",
        g1.supervisor_log()
    );
    let suspend_log = g1.supervisor_log();
    let carried = suspend_log
        .lines()
        .find(|l| l.contains("gpu snapshot: extension:"))
        .unwrap_or_else(|| panic!("the snapshot has no extension section line:\n{suspend_log}"));
    eprintln!("snapshot: {carried}");
    let contexts: usize = carried
        .split("extension: ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unreadable extension line: {carried}"));
    assert!(
        contexts >= 1 && carried.contains("with rings"),
        "the snapshot carried no extension context, so vkcube's transport state is not in it: \
         {carried}"
    );

    let scratch = g1.scratch_dir().to_path_buf();
    let pid = std::process::id();
    let snap: PathBuf = std::env::temp_dir().join(format!("limina-metal-restore-{pid}.snap"));
    let disk: PathBuf = std::env::temp_dir().join(format!("limina-metal-restore-{pid}.raw"));
    std::fs::copy(g1.snapshot_path().expect("snapshot path configured"), &snap)
        .expect("preserving the snapshot file");
    limina_test::cow_clone(&scratch.join("disk.raw"), &disk)
        .expect("preserving the suspended guest's disk");
    drop(g1);
    let snap_consumed = snap.with_extension("bin.consumed");
    let cleanup = || {
        let _ = std::fs::remove_file(&snap);
        let _ = std::fs::remove_file(&snap_consumed);
        let _ = std::fs::remove_file(&disk);
    };

    // --- Guest 2: a fresh worker and replayer restoring it ---
    let mut cfg2 = devices(base_cfg)
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
            panic!("the restore worker never entered the restore path: {e}");
        });
    if let Err(e) = g2.wait_for_ssh(Duration::from_secs(120)) {
        let log = g2.supervisor_log();
        cleanup();
        panic!("the restored guest never became reachable over SSH: {e}\n{log}");
    }
    let boot_id_after = ssh_retry(&g2, "cat /proc/sys/kernel/random/boot_id");

    // The restore's own report.
    let log = g2.supervisor_log();
    let failures: Vec<&str> = log
        .lines()
        .filter(|l| {
            l.contains("replay dropped")
                || l.contains("replay FAILED")
                || l.contains("staged snapshot replay failed")
                || l.contains("unparseable extension section")
        })
        .collect();
    let complete = log.contains("gpu restore: replay complete");
    let back = log
        .lines()
        .find(|l| l.contains("gpu restore: extension: ") && l.contains("surfaces back"))
        .unwrap_or("(no extension restore line)");
    eprintln!("restore: complete={complete}; {back}");

    // The claim: the same client, still rendering.
    ssh_retry(&g2, &format!("{SESSION} {WAKE} >/dev/null 2>&1; echo woke"));
    std::thread::sleep(Duration::from_secs(5));
    let cube_after = vkcube_pid(&g2);
    let (post_changed, post_frame) = animation(&g2, "post-restore");
    let post_colors = color_diversity(&post_frame);
    eprintln!(
        "post-restore: vkcube pid {cube_after:?} (was {cube}); {post_changed} pixels changed in \
         {CAPTURE_GAP:?}; {post_colors} distinct colors (pre {pre_colors})"
    );

    let ok = failures.is_empty()
        && complete
        && back.contains("surfaces back")
        && boot_id_after == boot_id
        && cube_after == cube
        && post_changed >= MIN_CHANGED_PIXELS
        && post_colors * 4 >= pre_colors;
    if !ok {
        let kept = std::env::temp_dir().join(format!("limina-metal-restore-{pid}.log"));
        let _ = std::fs::write(&kept, &log);
        let png = std::env::temp_dir().join(format!("limina-metal-restore-{pid}.png"));
        if let Some(p) = g2.display_capture_path() {
            let _ = std::fs::copy(p, &png);
        }
        let client = g2
            .ssh_exec("tail -n 20 /tmp/vkcube.log; pgrep -a vkcube")
            .unwrap_or_else(|e| format!("(probe failed: {e})"));
        cleanup();
        panic!(
            "the transport's client did not survive the restore:\n\
             restore failures:   {failures:#?}\n\
             replay complete:    {complete}\n\
             extension restored: {back}\n\
             boot_id:            {boot_id} -> {boot_id_after}\n\
             vkcube pid:         {cube} -> {cube_after:?}\n\
             changed pixels:     {pre_changed} -> {post_changed} (must stay >= \
             {MIN_CHANGED_PIXELS})\n\
             colors:             {pre_colors} -> {post_colors}\n\
             client:\n{client}\n\
             restore-leg log kept at {}, last frame at {}",
            kept.display(),
            png.display()
        );
    }
    eprintln!("vkcube survived the restore: pid {cube}, {post_changed} pixels changing");
    cleanup();
}
