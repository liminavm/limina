// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Host-retention regression guard for per-scanout state (the 2026-08-07 jetsam class).
//!
//! A Vulkan compositor that mints a fresh scanout buffer every frame grew the host worker
//! ~850 MB/min in `owned unmapped` until jetsam SIGKILLed it on the user's machine. The
//! holder was the SUPERVISOR's frame-apply surface cache — an unbounded
//! `HashMap<u32, IOSurface>` keyed by scanout id, cleared only on a display-mode change,
//! whose own comment stated the premise it rested on ("the worker reuses a small fixed
//! set, its double buffer"). limina `8e00d94` gave it a bounded, oldest-evicted shape.
//!
//! **This test must boot WINDOWED.** The leak lives in `window::run`, and a
//! `--display-capture` boot never enters that function, so a captured-display version of
//! this test would exercise none of the code under test and pass green against a window
//! that leaks a whole framebuffer per frame. That is the failure mode this file exists to
//! rule out, so it uses [`GuestConfig::with_windowed_coexist_display`] and a real NSWindow
//! opens for the duration of the run.
//!
//! Vehicle: `guest/kmschurn.py` in its `churn-vk` mode — a ctypes KMS presenter that allocates
//! a fresh scanout image per frame **in venus directly** (modifier-tiled, exported dma-buf,
//! prime-imported), clears it on the GPU, binds it with `drmModeAddFB2WithModifiers`,
//! page-flips, and releases the previous one. It takes DRM master directly after the session
//! is isolated to `multi-user.target`; no Wayland, no GDM, no seat. See
//! `spikes/venus-churn-retention/RESULTS.md` for its RED/GREEN validation.
//!
//! The vehicle also has a `churn` mode that allocates with gbm, which is what the reporting
//! compositor did at the time and what this test originally used. The Vulkan arm is the one to
//! guard with: gbm only yields venus blobs when `GALLIUM_DRIVER=zink` is set, which is an env
//! the enhanced tier no longer configures, so that arm silently stops testing the host path
//! under study if the image's selectors change again. It also costs twice the host IOSurfaces
//! for the same pixels (RESULTS.md §0.6). `churn-vk` needs nothing but the venus ICD, and it
//! WRITES each buffer, so the pages are resident and the byte oracle below sees them.
//!
//! Oracle: the `owned unmapped` BYTES in `vmmap -summary` of our own worker — see
//! [`owned_unmapped`] for why bytes and not the region count. `vmmap` of a pid we own is
//! parallel-safe, which `ioclasscount` (system-wide) is not.
//!
//! Paired with a churn-actually-happened assert, and that pairing is the point: the silent
//! failure here is a fallback to a path that never allocates a fresh host surface (dumb
//! buffers, software-2D), which would keep the region count flat while testing nothing. A
//! green retention number means something only alongside evidence the workload ran.
//!
//! Measured separation, this test at 1280x800 on the `churn-vk` arm: **+1213 MiB** with
//! `SurfaceStore`'s cap lifted vs **+98 MiB** with it, over an identical guest workload
//! (`created=301` both times). 1213 MiB is 301 x 4 MiB — one whole retained framebuffer per
//! frame, exactly. The threshold below sits well clear of both.
//!
//! Since limina `93ff513` the supervisor also drops each surface as the guest unrefs it, so the
//! resting number is the guest's live ring rather than the cap: **+11 MiB**. The threshold was
//! tightened to match, which makes this test guard the release path too — see `MAX_BYTE_GROWTH`.
//!
//! Trap, if you ever re-run that A/B by hand: `cargo test -p limina-test` does NOT rebuild
//! `target/debug/limina`, and the harness launches that binary. Editing the supervisor and
//! re-running the test alone reproduces the OLD supervisor and returns a pass either way —
//! which is what an identical A/B always means. `cargo build -p limina` first (or use
//! `scripts/test-boot.sh`, which builds everything).
//!
//! Same prereqs as the other venus L2 tests: 16 KiB kernel + `enhanced.test` disk +
//! KosmicKrisp; SKIPs cleanly if missing. Gated behind `LIMINA_HVF_TESTS`; run via
//! `scripts/test-boot.sh`. In the EXCLUSIVE set in `.config/nextest.toml` — it opens a
//! window and its verdict is a memory measurement.

use std::process::Command;
use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const KMSCHURN: &str = include_str!("../guest/kmschurn.py");

/// Frames to present. At ~60 flips/s this is a five-second run — long enough that an
/// unbounded cache is unmistakable (300 retained framebuffers), short enough that it cannot
/// exhaust the host if the guard ever regresses.
const FRAMES: usize = 300;

/// Windowed display size. Pinned so the scanout size — and so the bytes behind each retained
/// region — do not depend on which host screen the window opens on.
const DISPLAY: (u32, u32) = (1280, 800);

/// Maximum growth in the worker's `owned unmapped` BYTES across the churn run.
///
/// Three regimes this has to separate, all measured at this display size (4 MB per framebuffer):
///
/// | | growth |
/// |---|---|
/// | unbounded retention — one framebuffer per frame | 301 x 4 MB = **1213 MB** |
/// | bounded by the caps alone — `SurfaceStore` (32) + frame cache (8) | ~164 MB ceiling, **98 MB** measured |
/// | the guest's live ring, which is what it should be | **11 MB** (`handles=3`) |
///
/// The threshold used to be 400 MB, which only asked "is it bounded at all" — the residual then
/// *was* the cap holding its 32 surfaces, so there was nothing tighter to ask for. Since limina
/// `93ff513` the supervisor drops each surface when the guest unrefs it, so the resting number
/// tracks the guest's live ring instead of the cap, and 64 MB now catches a regression to
/// cap-bounded (164 MB) as well as to unbounded — a strictly stronger guard, with 5.8x headroom
/// over the measured 11 MB.
const MAX_BYTE_GROWTH: i64 = 64 * 1024 * 1024;

/// The venus ICD selection, and deliberately nothing else. A non-login ssh shell does not
/// source `/etc/environment.d`, so the driver has to be named here; the `-vk` arm needs no
/// gallium/gbm selectors at all, which is half of why it is the arm this test runs.
const VENUS_ENV: &str = "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json";

/// Parse one vmmap size cell ("896K", "436.8M", "8.4G") into bytes.
fn parse_size(cell: &str) -> Option<i64> {
    let (num, mult) = match cell.chars().last()? {
        'K' => (&cell[..cell.len() - 1], 1024.0),
        'M' => (&cell[..cell.len() - 1], 1024.0 * 1024.0),
        'G' => (&cell[..cell.len() - 1], 1024.0 * 1024.0 * 1024.0),
        _ => (cell, 1.0),
    };
    num.parse::<f64>().ok().map(|n| (n * mult) as i64)
}

/// `owned unmapped` BYTES and region count charged to `pid` — memory billed to the task but not
/// mapped into its address space, which is where an IOSurface retained by another process lands
/// (storage bills to the task that CREATED the surface, so these are the worker's even when the
/// supervisor is what holds them).
///
/// **Bytes are the oracle; the region count is only reported.** Measured 2026-08-07: a run that
/// took `owned unmapped` from 19.2 M to 436.8 M moved the region count from 39 to 38 — regions
/// get coalesced, so a count-based assert is blind to hundreds of megabytes. It happens to
/// separate the catastrophic case (620 regions) and nothing finer, which is exactly the kind of
/// oracle that reads as a pass while the thing it guards regresses.
fn owned_unmapped(pid: libc::pid_t) -> (i64, i64) {
    let out = Command::new("vmmap")
        .args(["-summary", &pid.to_string()])
        .output()
        .expect("running vmmap against the worker");
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // The "(graphics)" sibling row is a different, tiny bucket — match the bare one.
        if line.contains("owned unmapped") && !line.contains("graphics") {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // "owned unmapped <VIRTUAL> <RESIDENT> <DIRTY> ... <REGIONS>"
            let bytes = cols.get(2).and_then(|c| parse_size(c)).unwrap_or(0);
            let regions = cols.last().and_then(|c| c.parse::<i64>().ok()).unwrap_or(0);
            return (bytes, regions);
        }
    }
    (0, 0)
}

#[test]
fn windowed_frame_apply_holds_bounded_state_under_scanout_churn() {
    if !limina_test::require_hvf_or_skip(
        "windowed_frame_apply_holds_bounded_state_under_scanout_churn",
    ) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!("SKIP: no KosmicKrisp ICD — venus unavailable, and gbm would not produce blobs");
        return;
    }
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIP: {e:#}");
            return;
        }
    };
    // Turn on just the worker's `[SCANOUT-LEDGER]` line (libkrun `bfc332a`) — the host-side
    // statement of how many scanout binds carried a resource the display path had not already
    // seen. Not asserted on (the guest-side `created=` count is the machine-readable one), but
    // it is what a failure investigation reads first. Scoped to that ONE module on purpose:
    // plain `krun_devices=debug` logs every block-queue interrupt, ~190k lines for this run.
    let cfg = cfg
        .with_windowed_coexist_display(DISPLAY.0, DISPLAY.1)
        .with_net()
        .with_env(
            "RUST_LOG",
            "krun_devices::virtio::gpu::virtio_gpu=debug,warn",
        );

    let mut guest = Guest::boot(&cfg).expect("booting the seated enhanced guest windowed");
    guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd came up");

    // Evict the session compositor: it holds DRM master, and the presenter needs it. The
    // graphical target is not otherwise interesting here — the subject is the host's
    // per-scanout bookkeeping, not anything GNOME does.
    guest
        .ssh_exec("sudo -n systemctl isolate multi-user.target")
        .expect("isolating to multi-user.target");
    std::thread::sleep(Duration::from_secs(3));

    guest
        .ssh_exec(&format!(
            "cat > /tmp/kmschurn.py <<'KMSCHURN_PY_EOF'\n{KMSCHURN}\nKMSCHURN_PY_EOF"
        ))
        .expect("writing kmschurn.py to the guest");

    let worker = guest.worker_pid().expect("worker pid");
    let (before, before_regions) = owned_unmapped(worker);
    eprintln!("worker {worker}: owned unmapped before = {before} B ({before_regions} regions)");

    let out = guest
        .ssh_exec_timeout(
            &format!("sudo -n env {VENUS_ENV} python3 /tmp/kmschurn.py churn-vk {FRAMES}"),
            Duration::from_secs(180),
        )
        .expect("running the churn presenter");
    eprintln!("{out}");

    // Settle before reading: releases and presents are drained on the supervisor's main thread
    // and the last of them land after the guest-side run has returned. Measuring immediately
    // reads the drain mid-flight rather than the resting state.
    let (mut after, mut after_regions) = owned_unmapped(worker);
    for _ in 0..10 {
        std::thread::sleep(Duration::from_secs(1));
        let now = owned_unmapped(worker);
        if now.0 == after {
            break;
        }
        after = now.0;
        after_regions = now.1;
    }
    let growth = after - before;
    eprintln!(
        "worker {worker}: owned unmapped after = {after} B ({after_regions} regions), \
         growth = {} MiB",
        growth / (1024 * 1024)
    );

    // Did the workload actually run? A retention number is meaningless without this: a
    // silent fallback to a path that allocates no host surface would hold the region count
    // flat and look like a pass. `created` counts buffers allocated, one per frame.
    let created = out
        .lines()
        .find_map(|l| l.strip_prefix("CHURN DONE churn-vk "))
        .and_then(|rest| {
            rest.split_whitespace()
                .find_map(|f| f.strip_prefix("created="))
        })
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("no `CHURN DONE churn-vk ... created=N` line in:\n{out}"));
    assert!(
        created > FRAMES,
        "the presenter allocated only {created} buffers for {FRAMES} frames — it did not \
         churn, so the retention number below proves nothing:\n{out}"
    );

    assert!(
        growth <= MAX_BYTE_GROWTH,
        "the worker gained {} MiB of `owned unmapped` across {created} fresh scanout buffers \
         (want <= {} MiB): host-side per-scanout state is unbounded again. Every byte is a \
         retained framebuffer; at this rate a real compositor session ends in a jetsam \
         SIGKILL. The holder to suspect first is the SUPERVISOR, not the worker — IOSurface \
         storage bills to the task that CREATED it, so this is charged to the worker no \
         matter who holds the reference (proven by split-kill, RESULTS.md §0.4). Do NOT \
         judge this by region count: it stayed flat across a 20x byte change.\n{out}",
        growth / (1024 * 1024),
        MAX_BYTE_GROWTH / (1024 * 1024)
    );

    eprintln!(
        "BOUNDED: {created} fresh scanout buffers cost {} MiB retained (cap {} MiB)",
        growth / (1024 * 1024),
        MAX_BYTE_GROWTH / (1024 * 1024)
    );
}
