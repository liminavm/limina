// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Tier-2 rendering regression tests: replay captured traces on venus and compare frames
//! against a same-boot software-rasterizer replay (the reference — no stored golden
//! images, so intentional mesa/KK changes never invalidate fixtures). Two layers:
//! `venus_replay_matches_llvmpipe_reference` exercises GL through the full
//! zink→venus→host stack (apitrace/eglretrace vs llvmpipe);
//! `venus_vk_replay_matches_lavapipe_reference` exercises native Vulkan on venus
//! (gfxreconstruct vs lavapipe), skipping zink.
//!
//! This is phases 1+2 of the trace-replay plan (memory `limina-trace-replay-plan`,
//! `spikes/trace-replay/RESULTS.md`): every historical venus rendering bug (all-zero vertex
//! buffer, #32 stencil clip, u8 sentinel...) rendered *wrong pixels* while exit codes and
//! FPS counters looked fine — these tests mechanize the pixel-verify discipline.
//!
//! Vehicle: the seated dev-enh golden (gdm autologin → gnome-shell on venus → Xwayland;
//! `apitrace`/`eglretrace` is X11-only). The pre-replay `glmark2-es2` probe doubles as the
//! regression test for the X11-EGL-on-zink crash (mesa patch 0006 + the x11-enabled venus
//! ICD — commit 14cf463): before the fix that exact invocation segfaulted in eglMakeCurrent.
//!
//! SKIPs cleanly without: LIMINA_HVF_TESTS, the 16 KiB kernel, the dev-enh disk, the
//! KosmicKrisp host ICD (machine-local /Volumes/mesa-cs build), or the trace fixture
//! (`fixtures/traces/glmark2-build.trace`, regenerate via
//! `spikes/trace-replay/capture-replay.sh`). Heavy: full seated desktop boot + a ~90 s
//! venus replay (the known X11-present slowness; see the tier-2 ledger).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The session GL environment (zink→venus). SSH shells do NOT inherit the session's
/// environment.d, and without this the GL stack silently falls back to llvmpipe — the
/// classic ENV TRAP (spikes/trace-replay/RESULTS.md). The GL_RENDERER probe below is the
/// guard that proves the env actually took.
const ZINK_ENV: &str = "LD_LIBRARY_PATH=/opt/mesa-zink/lib64 \
     LIBGL_DRIVERS_PATH=/opt/mesa-zink/lib64/dri \
     __EGL_VENDOR_LIBRARY_DIRS=/opt/mesa-zink/share/glvnd/egl_vendor.d \
     MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink \
     VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
     VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback \
     VK_LOADER_DISABLE_DYNAMIC_LIBRARY_UNLOADING=1";

/// Per-frame tolerance: fraction of pixels allowed to differ by more than
/// `CHANNEL_TOLERANCE` in any channel. Measured headroom is enormous: venus-vs-llvmpipe
/// pairs of this trace differ by ~0% of pixels, while a single wrong frame (content vs
/// black, or frame N vs N+100) differs by >20%.
const MAX_BAD_PIXEL_FRACTION: f64 = 0.01;
const CHANNEL_TOLERANCE: u8 = 8;

fn trace_fixture(env_override: &str, name: &str) -> PathBuf {
    match std::env::var(env_override) {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../fixtures/traces/{name}"))
        }
    }
}

/// Boot the seated dev-enh golden with the canonical worker environment
/// (boot-seated-kk.sh: KK as the host Vulkan driver, present-copy on) and wait for the
/// autologin session's Xwayland. Returns `None` (after printing a SKIPPED line) when a
/// machine-local prerequisite is missing.
fn boot_seated(test_name: &str) -> Option<Guest> {
    let cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED {test_name}: {e}");
            return None;
        }
    };
    // venus REQUIRES KosmicKrisp; SKIP up front if it isn't built (else Guest::boot would
    // silently degrade this to the software-2D fallback and the pixel compare would be moot).
    let Some(icd) = limina_test::kosmickrisp_icd() else {
        eprintln!(
            "SKIPPED {test_name}: no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk \
             (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return None;
    };
    // Guest::boot wires KK for the coexist display automatically; we only add present-copy.
    let cfg = cfg
        .with_coexist_display(1280, 800)
        .with_net()
        .with_env("LIMINA_PRESENT_COPY", "1");
    eprintln!("booting the seated dev-enh golden (KK ICD {icd:?})");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // The autologin graphical session (and its Xwayland) comes up well after sshd.
    guest
        .ssh_poll(
            "ls /run/user/1000/.mutter-Xwaylandauth.*",
            Duration::from_secs(180),
        )
        .expect("the autologin session's Xwayland never came up");
    Some(guest)
}

/// Compare same-named snapshot PNGs between `venus_dir` and `ref_dir` with the tolerance
/// gate, asserting at least `min_frames` pairs and a non-blank reference (a both-blank
/// pass proves nothing).
fn compare_snapshot_dirs(venus_dir: &Path, ref_dir: &Path, min_frames: usize) {
    let frames: BTreeSet<String> = std::fs::read_dir(venus_dir)
        .expect("reading venus snapshot dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".png"))
        .collect();
    assert!(
        frames.len() >= min_frames,
        "expected a healthy snapshot set (>= {min_frames}), got {} frames",
        frames.len()
    );

    let mut reference_has_content = false;
    for name in &frames {
        let (vw, vh, venus) = load_png_rgb(&venus_dir.join(name)).expect("decoding venus frame");
        let (rw, rh, reference) =
            load_png_rgb(&ref_dir.join(name)).expect("decoding reference frame (set mismatch?)");
        assert_eq!((vw, vh), (rw, rh), "frame {name}: size mismatch");

        if reference.chunks_exact(3).any(|p| p.iter().any(|&c| c > 32)) {
            reference_has_content = true;
        }

        let bad = venus
            .chunks_exact(3)
            .zip(reference.chunks_exact(3))
            .filter(|(a, b)| {
                a.iter()
                    .zip(b.iter())
                    .any(|(&x, &y)| x.abs_diff(y) > CHANNEL_TOLERANCE)
            })
            .count();
        let frac = bad as f64 / (vw as f64 * vh as f64);
        eprintln!(
            "frame {name}: {bad} pixels off (>{CHANNEL_TOLERANCE}), {:.4}%",
            frac * 100.0
        );
        assert!(
            frac <= MAX_BAD_PIXEL_FRACTION,
            "frame {name}: venus diverges from the software reference on {:.2}% of pixels \
             (tolerance {:.2}%) — venus rendered wrong pixels",
            frac * 100.0,
            MAX_BAD_PIXEL_FRACTION * 100.0
        );
    }
    assert!(
        reference_has_content,
        "the software-reference frames are all blank — the fixture/replay is broken, \
         the comparison proved nothing"
    );
    eprintln!(
        "compared {} frame pairs: venus matches the software reference",
        frames.len()
    );
}

/// Decode a PNG into (width, height, RGB8 pixels). Handles the RGB and RGBA layouts
/// eglretrace snapshots use.
fn load_png_rgb(path: &Path) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let decoder = png::Decoder::new(std::fs::File::open(path)?);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    let rgb = match info.color_type {
        png::ColorType::Rgb => buf,
        png::ColorType::Rgba => buf
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        other => anyhow::bail!("unexpected PNG color type {other:?} in {path:?}"),
    };
    anyhow::ensure!(info.bit_depth == png::BitDepth::Eight, "expected 8-bit PNG");
    Ok((info.width, info.height, rgb))
}

#[test]
fn venus_replay_matches_llvmpipe_reference() {
    if !limina_test::require_hvf_or_skip("venus_replay_matches_llvmpipe_reference") {
        return;
    }
    let trace = trace_fixture("LIMINA_TEST_TRACE", "glmark2-build.trace");
    if !trace.exists() {
        eprintln!(
            "SKIPPED venus_replay_matches_llvmpipe_reference: trace fixture missing at \
             {trace:?}; regenerate with spikes/trace-replay/capture-replay.sh (or set \
             LIMINA_TEST_TRACE)"
        );
        return;
    }
    let Some(guest) = boot_seated("venus_replay_matches_llvmpipe_reference") else {
        return;
    };
    let x11 = "DISPLAY=:0 XAUTHORITY=$(ls /run/user/1000/.mutter-Xwaylandauth.* | head -1) \
               XDG_RUNTIME_DIR=/run/user/1000";

    // Backend guard + X11-crash regression: an X11 EGL client must come up on REAL venus.
    // Pre-14cf463 this segfaulted (kopper NULL-fp); a silent llvmpipe fallback (the env
    // trap) would render the whole comparison meaningless, so assert the renderer string.
    let probe = guest
        .ssh_exec(&format!(
            "env {x11} {ZINK_ENV} glmark2-es2 -b build:duration=1 --size 256x256 2>&1 \
             | grep GL_RENDERER"
        ))
        .expect("X11 EGL probe crashed (the kopper X11 regression?)");
    eprintln!("X11 GL probe: {}", probe.trim());
    assert!(
        probe.contains("Virtio-GPU Venus"),
        "X11 GL stack is not on venus (env trap / venus regression?): {probe}"
    );

    guest
        .scp_to_guest(&trace, "/tmp/replay.trace")
        .expect("pushing the trace fixture into the guest");

    // Replay on both backends, snapshotting every 100th frame. eglretrace exits non-zero
    // on a crashed replay, which ssh_exec turns into a failure.
    for (name, env) in [
        ("venus", ZINK_ENV),
        (
            "llvmpipe",
            "GALLIUM_DRIVER=llvmpipe LIBGL_ALWAYS_SOFTWARE=1",
        ),
    ] {
        let out = guest
            .ssh_exec(&format!(
                "mkdir -p /tmp/snap-{name} && env {x11} {env} eglretrace --headless \
                 --snapshot-interval=100 -s /tmp/snap-{name}/ /tmp/replay.trace 2>&1 \
                 | tail -2"
            ))
            .unwrap_or_else(|e| panic!("{name} replay failed: {e}"));
        eprintln!("{name} replay: {}", out.trim());
        assert!(
            out.contains("Rendered") && out.contains("frames"),
            "{name} replay did not report rendered frames:\n{out}"
        );
    }

    // Pull both snapshot sets and compare frame-by-frame on the host.
    let venus_dir = guest.scratch_dir().join("snap-venus");
    let ref_dir = guest.scratch_dir().join("snap-llvmpipe");
    guest
        .scp_from_guest("/tmp/snap-venus/*.png", &venus_dir)
        .expect("pulling venus snapshots");
    guest
        .scp_from_guest("/tmp/snap-llvmpipe/*.png", &ref_dir)
        .expect("pulling llvmpipe snapshots");
    compare_snapshot_dirs(&venus_dir, &ref_dir, 10);

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}

/// Phase 4 — the prize: GNOME SHELL ITSELF (mutter compositing, captured from the real
/// seated session via an LD_PRELOAD=egltrace.so systemd drop-in; fixture protocol in
/// spikes/trace-replay/capture-replay-shell.sh). The shell's own render graph (StWidget
/// shaders, rounded-corner stencil clips — the #32 bug class, text, blur) reproduces
/// PIXEL-EXACT across venus and llvmpipe on the idle-overview protocol. The first
/// snapshot pair is dropped: the startup fade samples uninitialized textures, which is
/// undefined (backend-divergent) content by spec, not a rendering bug.
#[test]
fn venus_shell_replay_matches_llvmpipe_reference() {
    if !limina_test::require_hvf_or_skip("venus_shell_replay_matches_llvmpipe_reference") {
        return;
    }
    let trace = trace_fixture("LIMINA_TEST_SHELL_TRACE", "gnome-shell-seated.trace");
    if !trace.exists() {
        eprintln!(
            "SKIPPED venus_shell_replay_matches_llvmpipe_reference: trace fixture missing at \
             {trace:?}; regenerate with spikes/trace-replay/capture-replay-shell.sh (or set \
             LIMINA_TEST_SHELL_TRACE)"
        );
        return;
    }
    let Some(guest) = boot_seated("venus_shell_replay_matches_llvmpipe_reference") else {
        return;
    };
    let x11 = "DISPLAY=:0 XAUTHORITY=$(ls /run/user/1000/.mutter-Xwaylandauth.* | head -1) \
               XDG_RUNTIME_DIR=/run/user/1000";

    // Backend guard (the env trap): the venus leg must run on real venus.
    let probe = guest
        .ssh_exec(&format!(
            "env {x11} {ZINK_ENV} glmark2-es2 -b build:duration=1 --size 256x256 2>&1 \
             | grep GL_RENDERER"
        ))
        .expect("X11 EGL probe crashed");
    eprintln!("X11 GL probe: {}", probe.trim());
    assert!(
        probe.contains("Virtio-GPU Venus"),
        "X11 GL stack is not on venus (env trap / venus regression?): {probe}"
    );

    guest
        .scp_to_guest(&trace, "/tmp/shell.trace")
        .expect("pushing the shell trace fixture into the guest");

    // The idle-overview capture is short (~74 frames), so snapshot densely.
    for (name, env) in [
        ("venus", ZINK_ENV),
        (
            "llvmpipe",
            "GALLIUM_DRIVER=llvmpipe LIBGL_ALWAYS_SOFTWARE=1",
        ),
    ] {
        let out = guest
            .ssh_exec(&format!(
                "mkdir -p /tmp/shellsnap-{name} && env {x11} {env} eglretrace --headless \
                 --snapshot-interval=10 -s /tmp/shellsnap-{name}/ /tmp/shell.trace 2>&1 \
                 | grep Rendered"
            ))
            .unwrap_or_else(|e| panic!("{name} shell replay failed: {e}"));
        eprintln!("{name} shell replay: {}", out.trim());
        assert!(
            out.contains("Rendered") && out.contains("frames"),
            "{name} shell replay did not report rendered frames:\n{out}"
        );
    }

    let venus_dir = guest.scratch_dir().join("shellsnap-venus");
    let ref_dir = guest.scratch_dir().join("shellsnap-llvmpipe");
    guest
        .scp_from_guest("/tmp/shellsnap-venus/*.png", &venus_dir)
        .expect("pulling venus shell snapshots");
    guest
        .scp_from_guest("/tmp/shellsnap-llvmpipe/*.png", &ref_dir)
        .expect("pulling llvmpipe shell snapshots");

    // Drop the first snapshot pair (startup-fade undefined content) from both sets.
    for dir in [&venus_dir, &ref_dir] {
        let first = std::fs::read_dir(dir)
            .expect("reading snapshot dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "png"))
            .min()
            .expect("no snapshots to drop");
        eprintln!("dropping startup frame {first:?}");
        std::fs::remove_file(first).expect("removing startup frame");
    }
    compare_snapshot_dirs(&venus_dir, &ref_dir, 3);

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}

/// Phase 2: native Vulkan (no zink) — replay a gfxreconstruct capture of vkcube on venus
/// and on lavapipe (the Vulkan software reference), compare screenshots. The lavapipe leg
/// needs `--remove-unsupported` (the venus capture records instance extensions lavapipe
/// lacks; vkCreateInstance hard-fails the replay otherwise); the venus leg stays strict.
/// /opt/gfxreconstruct is baked into the dev-enh golden (scripts/build-gfxreconstruct.sh).
#[test]
fn venus_vk_replay_matches_lavapipe_reference() {
    if !limina_test::require_hvf_or_skip("venus_vk_replay_matches_lavapipe_reference") {
        return;
    }
    let trace = trace_fixture("LIMINA_TEST_VK_TRACE", "vkcube.gfxr");
    if !trace.exists() {
        eprintln!(
            "SKIPPED venus_vk_replay_matches_lavapipe_reference: trace fixture missing at \
             {trace:?}; regenerate with spikes/trace-replay/capture-replay-vk.sh (or set \
             LIMINA_TEST_VK_TRACE)"
        );
        return;
    }
    let Some(guest) = boot_seated("venus_vk_replay_matches_lavapipe_reference") else {
        return;
    };
    let base = "XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 \
                VK_LOADER_DISABLE_DYNAMIC_LIBRARY_UNLOADING=1";

    // Backend guard: venus must enumerate the real host GPU before the replay means anything.
    let probe = guest
        .ssh_exec(
            "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
             vulkaninfo --summary 2>/dev/null | grep deviceName | head -1",
        )
        .expect("vulkaninfo probe failed");
    eprintln!("venus probe: {}", probe.trim());
    assert!(
        probe.contains("Virtio-GPU Venus"),
        "venus did not enumerate: {probe}"
    );

    guest
        .scp_to_guest(&trace, "/tmp/replay.gfxr")
        .expect("pushing the gfxr fixture into the guest");

    for (name, icd, extra) in [
        ("venus", "virtio_icd.aarch64.json", ""),
        ("lavapipe", "lvp_icd.aarch64.json", "--remove-unsupported"),
    ] {
        let out = guest
            .ssh_exec(&format!(
                "mkdir -p /tmp/vksnap-{name} && env {base} \
                 VK_DRIVER_FILES=/usr/share/vulkan/icd.d/{icd} \
                 /opt/gfxreconstruct/bin/gfxrecon-replay {extra} \
                 --screenshots 10,50,100,150,190 --screenshot-format png \
                 --screenshot-dir /tmp/vksnap-{name} /tmp/replay.gfxr 2>&1 \
                 | grep -E 'Replay FPS|Replay device info'"
            ))
            .unwrap_or_else(|e| panic!("{name} vk replay failed: {e}"));
        eprintln!("{name} vk replay: {}", out.trim());
        assert!(
            out.contains("Replay FPS"),
            "{name} vk replay did not complete:\n{out}"
        );
        // The capture is from venus, so gfxrecon warns with the replay device info iff
        // the replay device differs. The reference leg MUST be on llvmpipe — a vacuous
        // venus-vs-venus comparison would pass while proving nothing (and FPS alone
        // can't tell: both legs are vsync-capped through the session's Wayland WSI).
        if name == "lavapipe" {
            assert!(
                out.contains("llvmpipe"),
                "the reference replay did not run on lavapipe (no device-mismatch info):\n{out}"
            );
        } else {
            assert!(
                !out.contains("Replay device info"),
                "the venus replay ran on a different device than the capture:\n{out}"
            );
        }
    }

    let venus_dir = guest.scratch_dir().join("vksnap-venus");
    let ref_dir = guest.scratch_dir().join("vksnap-lavapipe");
    guest
        .scp_from_guest("/tmp/vksnap-venus/*.png", &venus_dir)
        .expect("pulling venus vk snapshots");
    guest
        .scp_from_guest("/tmp/vksnap-lavapipe/*.png", &ref_dir)
        .expect("pulling lavapipe vk snapshots");
    compare_snapshot_dirs(&venus_dir, &ref_dir, 5);

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
