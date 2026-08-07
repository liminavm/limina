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
//! Vehicle: `guest/kmschurn.py` — a ctypes KMS presenter that allocates a fresh gbm scanout
//! buffer per frame under zink (making them venus blobs, the shape the reporting compositor
//! had), binds it with `drmModeAddFB2`, page-flips, and releases the previous one. It takes
//! DRM master directly after the session is isolated to `multi-user.target`; no Wayland, no
//! GDM, no seat. See `spikes/venus-churn-retention/RESULTS.md` for its RED/GREEN validation.
//!
//! Oracle: the `owned unmapped` REGION COUNT in `vmmap -summary` of our own worker. Region
//! count rather than bytes because it is resolution-independent — each retained scanout is
//! one region whatever the mode, so the same threshold holds on any rig, while a byte
//! threshold would have to track the display size. `vmmap` of a pid we own is also
//! parallel-safe, which `ioclasscount` (system-wide) is not.
//!
//! Paired with a churn-actually-happened assert, and that pairing is the point: the silent
//! failure here is a fallback to a path that never allocates a fresh host surface (dumb
//! buffers, software-2D), which would keep the region count flat while testing nothing. A
//! green retention number means something only alongside evidence the workload ran.
//!
//! Measured separation, this test at 1280x800: **+606 regions** with `SurfaceStore`'s
//! eviction disabled vs **+23** with it, over an identical guest workload (`created=301`
//! both times). The threshold below sits far from both.
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

/// Maximum growth in the worker's `owned unmapped` region count across the churn run.
///
/// The fix retains at most `FRAME_CACHE_CAP` (8) frame-cache entries plus `SurfaceStore`'s
/// own 32, and the measured delta was +13. The bug retains one region per frame: +605. 100
/// is comfortably clear of the first and far below the second — this is a "is it bounded at
/// all" guard, not a tight budget, deliberately, so it does not fail on unrelated churn.
const MAX_REGION_GROWTH: i64 = 100;

/// The guest's venus/zink selection. Without this gbm loads a different gallium driver, the
/// buffers are not venus blobs, and the run silently exercises a path the bug never lived on
/// — `kmschurn.py` echoes what it saw so a failure log shows which it got.
const ZINK_ENV: &str = "GALLIUM_DRIVER=zink MESA_LOADER_DRIVER_OVERRIDE=zink \
                        VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json";

/// `owned unmapped` regions charged to `pid` — memory billed to the task but not mapped into
/// its address space, which is where an IOSurface retained by another process lands (storage
/// bills to the task that CREATED the surface, so these are the worker's even when the
/// supervisor is what holds them).
fn owned_unmapped_regions(pid: libc::pid_t) -> i64 {
    let out = Command::new("vmmap")
        .args(["-summary", &pid.to_string()])
        .output()
        .expect("running vmmap against the worker");
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // The "(graphics)" sibling row is a different, tiny bucket — match the bare one.
        if line.contains("owned unmapped") && !line.contains("graphics") {
            if let Some(count) = line.split_whitespace().last() {
                if let Ok(n) = count.parse::<i64>() {
                    return n;
                }
            }
        }
    }
    0
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
    let before = owned_unmapped_regions(worker);
    eprintln!("worker {worker}: owned unmapped regions before = {before}");

    let out = guest
        .ssh_exec_timeout(
            &format!("sudo -n env {ZINK_ENV} python3 /tmp/kmschurn.py churn {FRAMES}"),
            Duration::from_secs(180),
        )
        .expect("running the churn presenter");
    eprintln!("{out}");

    let after = owned_unmapped_regions(worker);
    let growth = after - before;
    eprintln!("worker {worker}: owned unmapped regions after = {after} (+{growth})");

    // Did the workload actually run? A retention number is meaningless without this: a
    // silent fallback to a path that allocates no host surface would hold the region count
    // flat and look like a pass. `created` counts buffers allocated, one per frame.
    let created = out
        .lines()
        .find_map(|l| l.strip_prefix("CHURN DONE churn "))
        .and_then(|rest| {
            rest.split_whitespace()
                .find_map(|f| f.strip_prefix("created="))
        })
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("no `CHURN DONE churn ... created=N` line in:\n{out}"));
    assert!(
        created > FRAMES,
        "the presenter allocated only {created} buffers for {FRAMES} frames — it did not \
         churn, so the retention number below proves nothing:\n{out}"
    );

    assert!(
        growth <= MAX_REGION_GROWTH,
        "the worker gained {growth} `owned unmapped` regions across {created} fresh scanout \
         buffers (want <= {MAX_REGION_GROWTH}): host-side per-scanout state is unbounded \
         again. Each region is a whole retained framebuffer; at this rate a real compositor \
         session ends in a jetsam SIGKILL. The holder to suspect first is the SUPERVISOR, \
         not the worker — IOSurface storage bills to the task that CREATED it, so these \
         regions are charged to the worker however holds the reference. See \
         `spikes/venus-churn-retention/RESULTS.md`.\n{out}"
    );

    eprintln!(
        "BOUNDED: {created} fresh scanout buffers cost {growth} retained regions (cap \
         {MAX_REGION_GROWTH})"
    );
}
