// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Regression guard: a live venus scanout must not be readable by an unrelated process.
//!
//! The accelerated tiers' scanout IOSurfaces are minted inside the renderer (virglrs). While it
//! minted them with `kIOSurfaceIsGlobal`, any process running as the user could read the guest's
//! screen by guessing ids — `spikes/venus-draw-probe/iosdump.swift` is exactly that attack. A
//! windowed boot hands the worker `--surface-port-name`, and the worker must then install a
//! publisher so the renderer mints non-global surfaces and hands them to the supervisor by Mach
//! port. This test boots that configuration and asks a stranger process to `IOSurfaceLookup` a
//! surface the guest is scanning out at that moment.
//!
//! This is the L2 twin of `non_global_scanout_is_hidden_from_strangers` in
//! `crates/limina-display/src/iosurface.rs`, which guards the software-2D ring the same way. As
//! there, the probe runs in a re-exec of this test binary: a surface is always resolvable by a
//! process that holds it, so hiding is only observable from one that does not. (This process
//! never holds the worker's surfaces either; the re-exec keeps the probe free of anything a test
//! harness might have looked up.)
//!
//! **Which id.** The worker logs one line per scanout bind at DEBUG,
//! `SET_SCANOUT_BLOB scanout=N res=R -> IOSurface ID (zero-copy)`, and that id is the renderer's
//! own surface for the resource. The INFO `fence-accurate presents ENGAGED (… iosurface ID)` line
//! is NOT usable: it fires once per worker, for the first deferred present, which on a seated boot
//! is GDM's buffer — freed when the test evicts the session, so a stranger would miss it whether
//! or not scoping works. Only binds logged after the presenter announced itself are read.
//!
//! **Why the id is known live.** The presenter is `guest/kmschurn.py static-vk`, which allocates a
//! ring of two venus scanout images once and flips between them until its frame count runs out,
//! releasing nothing before then. It runs detached for far longer than the test needs. Around the
//! probe the test asserts that (a) the presenter is still running and has not reached
//! `CHURN DONE`, and (b) the worker bound that same id again AFTER the probe. So the stranger
//! looked the surface up while the guest was provably scanning it out.
//!
//! **Positive control, first.** The same run is repeated with `LIMINA_GLOBAL_SCANOUT=1` in the
//! worker's environment (the debug escape hatch that additionally marks the surfaces global), and
//! there the stranger MUST find the surface, at the display's size. It runs first, so a broken
//! parse, a stale id or a stranger that cannot look anything up fails loudly instead of reading
//! as "hidden". A tree that ignores the flag (surfaces always global) passes the control and fails
//! the scoped assert, which is the shape a regression has.
//!
//! Same prereqs as the other venus L2 tests: seated EFI enhanced image + KosmicKrisp; SKIPs cleanly
//! if missing. Gated behind `LIMINA_HVF_TESTS`; run via `scripts/test-boot.sh`. It opens a real
//! NSWindow (the surface port only exists on a windowed boot), so like `scanout_churn_retention`
//! it belongs in the EXCLUSIVE set in `.config/nextest.toml`.

use std::ffi::c_void;
use std::process::Command;
use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const KMSCHURN: &str = include_str!("../guest/kmschurn.py");

/// Windowed display size, pinned so the control can check the found surface's dimensions.
const DISPLAY: (u32, u32) = (1280, 800);

/// Venus ICD selection for a non-login ssh shell; the `-vk` arm needs nothing else.
const VENUS_ENV: &str = "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json";

/// Frames for the detached presenter: at ~60 flips/s about half an hour, far past the test.
const PRESENTER_FRAMES: u32 = 100_000;

/// Env var that turns a re-exec of this binary into the stranger probe.
const STRANGER_ENV: &str = "LIMINA_STRANGER_LOOKUP_ID";

/// Stranger exit codes. Distinct from 0/101 so a panic cannot be read as a verdict.
const EXIT_FOUND: i32 = 10;
const EXIT_HIDDEN: i32 = 11;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLookup(id: u32) -> *mut c_void;
    fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// Stranger role: with [`STRANGER_ENV`] set, this process exists only to try `IOSurfaceLookup` on
/// that id, print what it saw and exit [`EXIT_FOUND`] or [`EXIT_HIDDEN`]. A no-op otherwise.
#[test]
fn stranger_lookup_role() {
    let Ok(id) = std::env::var(STRANGER_ENV) else {
        return;
    };
    let id: u32 = id.parse().expect("stranger id");
    // SAFETY: plain IOSurface calls; the looked-up reference is released before exit.
    unsafe {
        let s = IOSurfaceLookup(id);
        if s.is_null() {
            println!("STRANGER HIDDEN {id}");
            std::process::exit(EXIT_HIDDEN);
        }
        let (w, h) = (IOSurfaceGetWidth(s), IOSurfaceGetHeight(s));
        CFRelease(s);
        println!("STRANGER FOUND {id} {w}x{h}");
        std::process::exit(EXIT_FOUND);
    }
}

/// What the stranger saw: `Some((w, h))` if it could look the surface up, `None` if hidden.
fn stranger_lookup(id: u32) -> Option<(usize, usize)> {
    let exe = std::env::current_exe().expect("test exe");
    let out = Command::new(exe)
        .args(["--exact", "stranger_lookup_role", "--nocapture"])
        .env(STRANGER_ENV, id.to_string())
        .output()
        .expect("spawning the stranger");
    let stdout = String::from_utf8_lossy(&out.stdout);
    match out.status.code() {
        Some(EXIT_HIDDEN) => None,
        Some(EXIT_FOUND) => {
            let dims = stdout
                .lines()
                .find_map(|l| l.strip_prefix(&format!("STRANGER FOUND {id} ")))
                .and_then(|d| d.split_once('x'))
                .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
                .unwrap_or_else(|| panic!("stranger found {id} but printed no size:\n{stdout}"));
            Some(dims)
        }
        other => panic!(
            "the stranger probe gave no verdict (exit {other:?}):\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

/// `(iosurface id, count)` for every SET_SCANOUT_BLOB bind in `log`, in first-seen order.
fn scanout_binds(log: &str) -> Vec<(u32, usize)> {
    let mut seen: Vec<(u32, usize)> = Vec::new();
    for line in log.lines() {
        let Some(rest) = line
            .find("SET_SCANOUT_BLOB ")
            .and_then(|i| line[i..].split_once(" -> IOSurface "))
            .map(|(_, r)| r)
        else {
            continue;
        };
        let Some(id) = rest
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        match seen.iter_mut().find(|(i, _)| *i == id) {
            Some((_, n)) => *n += 1,
            None => seen.push((id, 1)),
        }
    }
    seen
}

/// The part of `log` past `offset` (the log only grows, so the offset stays a char boundary).
fn since(log: &str, offset: usize) -> &str {
    log.get(offset..).unwrap_or("")
}

fn binds_of(log: &str, id: u32) -> usize {
    scanout_binds(log)
        .into_iter()
        .find(|(i, _)| *i == id)
        .map_or(0, |(_, n)| n)
}

/// Assert the detached presenter is still flipping its ring: running, and not finished. The `[k]`
/// keeps the pattern from matching the ssh shell running this very pgrep.
fn assert_presenter_alive(guest: &Guest, when: &str) {
    let state = guest
        .ssh_exec(
            "pgrep -f '[k]mschurn.py static-vk' >/dev/null && echo RUNNING || echo GONE; \
             grep -E 'CHURN (DONE|FAIL)' /tmp/kmschurn.out || true",
        )
        .expect("reading the presenter's state");
    assert!(
        state.contains("RUNNING") && !state.contains("CHURN DONE") && !state.contains("CHURN FAIL"),
        "the presenter was not holding its scanout ring {when}, so the id under test may be \
         stale:\n{state}"
    );
}

/// One boot: present a steady venus scanout, find its surface id, and return what a stranger saw
/// for each surface of the presenter's ring (with proof it was live across the probe).
fn stranger_view_of_live_scanouts(
    cfg: &GuestConfig,
    global: bool,
) -> Vec<(u32, Option<(usize, usize)>)> {
    let mut cfg = cfg.clone();
    if global {
        cfg = cfg.with_env("LIMINA_GLOBAL_SCANOUT", "1");
    }
    let arm = if global {
        "control (LIMINA_GLOBAL_SCANOUT=1)"
    } else {
        "scoped"
    };

    let mut guest = Guest::boot(&cfg).expect("booting the seated enhanced guest windowed");
    guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd came up");

    // The presenter needs DRM master, which the session compositor holds.
    guest
        .ssh_exec("sudo -n systemctl isolate multi-user.target")
        .expect("isolating to multi-user.target");
    std::thread::sleep(Duration::from_secs(3));

    guest
        .ssh_exec(&format!(
            "cat > /tmp/kmschurn.py <<'KMSCHURN_PY_EOF'\n{KMSCHURN}\nKMSCHURN_PY_EOF"
        ))
        .expect("writing kmschurn.py to the guest");
    guest
        .ssh_exec(&format!(
            "sudo -n setsid -f env {VENUS_ENV} python3 /tmp/kmschurn.py static-vk \
             {PRESENTER_FRAMES} 2 </dev/null >/tmp/kmschurn.out 2>&1"
        ))
        .expect("starting the steady presenter");
    let started = guest
        .ssh_poll(
            "grep -E 'CHURN (START|FAIL)' /tmp/kmschurn.out",
            Duration::from_secs(60),
        )
        .unwrap_or_else(|e| panic!("[{arm}] the presenter never started: {e:#}"));
    assert!(
        started.contains("CHURN START static-vk"),
        "[{arm}] the presenter failed to start:\n{started}\n{}",
        guest.ssh_exec("cat /tmp/kmschurn.out").unwrap_or_default()
    );

    // Everything bound from here on is the presenter's ring: nothing else is scanning out.
    let offset = guest.supervisor_log().len();
    let deadline = Instant::now() + Duration::from_secs(30);
    let ring = loop {
        let log = guest.supervisor_log();
        let binds = scanout_binds(since(&log, offset));
        if binds.iter().map(|(_, n)| n).sum::<usize>() >= 20 {
            break binds;
        }
        assert!(
            Instant::now() < deadline,
            "[{arm}] the worker logged no venus SET_SCANOUT_BLOB binds for the presenter's ring \
             (binds so far: {binds:?}). Either the presenter is not reaching the host as zero-copy \
             venus blobs, or the worker's RUST_LOG no longer lets \
             krun_devices::virtio::gpu::virtio_gpu=debug through."
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    eprintln!("[{arm}] presenter ring binds (iosurface id, count): {ring:?}");
    assert!(
        ring.len() <= 2,
        "[{arm}] a two-buffer static ring bound {} distinct surfaces: {ring:?}",
        ring.len()
    );

    let mut seen = Vec::new();
    for &(id, _) in &ring {
        assert_ne!(id, 0, "[{arm}] parsed a zero surface id");
        assert_presenter_alive(&guest, "before the probe");
        let before = binds_of(since(&guest.supervisor_log(), offset), id);
        let view = stranger_lookup(id);
        std::thread::sleep(Duration::from_secs(1));
        let after = binds_of(since(&guest.supervisor_log(), offset), id);
        assert_presenter_alive(&guest, "after the probe");
        assert!(
            after > before,
            "[{arm}] surface {id} was not bound again after the probe ({before} -> {after} binds), \
             so it is not proven live across it"
        );
        eprintln!(
            "[{arm}] stranger IOSurfaceLookup({id}) -> {view:?} (live: {before} -> {after} binds)"
        );
        seen.push((id, view));
    }

    let _ = guest.ssh_exec("sudo -n pkill -f '[k]mschurn.py'");
    drop(guest);
    seen
}

#[test]
fn venus_scanout_is_hidden_from_strangers() {
    if !limina_test::require_hvf_or_skip("venus_scanout_is_hidden_from_strangers") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!("SKIP: no KosmicKrisp ICD — venus unavailable");
        return;
    }
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIP: {e:#}");
            return;
        }
    };
    // The bare `warn` keeps every other crate at warn; the one module at debug is what logs the
    // per-bind SET_SCANOUT_BLOB line carrying the surface id.
    let cfg = cfg
        .with_windowed_coexist_display(DISPLAY.0, DISPLAY.1)
        .with_net()
        .with_supervisor_log()
        .with_env(
            "RUST_LOG",
            "krun_devices::virtio::gpu::virtio_gpu=debug,warn",
        );

    // Positive control first: with the surfaces also marked global the stranger must see them,
    // at the display's size. Otherwise a "hidden" below could mean a dead id or a blind stranger.
    let control = stranger_view_of_live_scanouts(&cfg, true);
    for (id, view) in &control {
        assert_eq!(
            *view,
            Some((DISPLAY.0 as usize, DISPLAY.1 as usize)),
            "CONTROL: with LIMINA_GLOBAL_SCANOUT=1 a stranger should find live venus scanout \
             {id} at {}x{}, so the oracle cannot tell hidden from broken: {control:?}",
            DISPLAY.0,
            DISPLAY.1
        );
    }

    let scoped = stranger_view_of_live_scanouts(&cfg, false);
    for (id, view) in &scoped {
        assert!(
            view.is_none(),
            "a stranger process read live venus scanout IOSurface {id} ({view:?}) by id: the \
             renderer minted it global, so any process running as the user can read the guest's \
             screen. A windowed worker must install the surface publisher (--surface-port-name) \
             before the renderer mints its first surface: {scoped:?}"
        );
    }
    eprintln!("SCOPED: control {control:?}, scoped {scoped:?}");
}
