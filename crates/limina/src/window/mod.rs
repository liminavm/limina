// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The limina native window (decision D3: the UI lives in the supervisor process).
//!
//! The worker publishes each guest scanout as a shared IOSurface and reports it over a
//! control fd (`surface <id> <w> <h>` / `frame`). Here, a reader thread parses that and
//! updates shared state; an `NSTimer` on the **main thread** polls the state and updates
//! the window's `CALayer.contents` to the looked-up IOSurface. All AppKit access stays on
//! the main thread; only plain data (ids/counters) crosses the thread boundary.
#![allow(deprecated)] // objc2-io-surface 0.3 renamed some free fns to methods.
//!
//! Split along its seams (2026-07-01 review, Part I recommendation 5): `present` (surface
//! store/reader/frame-apply/shown-ack), `cursor` (guest-cursor shape + capture compositing),
//! `lifecycle` (worker connection + quit policy), `diag` (capture / present-copy probes),
//! plus the pre-existing `input` and `capture_tap`. This file keeps `run()` — the window,
//! the render timer, and the event monitor.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent, NSEventMask,
    NSEventType, NSViewLayerContentsRedrawPolicy, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_core_foundation::CFRetained;
use objc2_foundation::{
    NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use objc2_io_surface::{IOSurfaceLookup, IOSurfaceRef};
use objc2_quartz_core::{CALayer, CATransaction};

mod capture_tap;
mod cursor;
mod diag;
mod input;
mod lifecycle;
mod present;

pub use lifecycle::{WorkerConn, WorkerIo};
pub use present::{
    empty_surface_map, mark_worker_exited, spawn_reader, surface_rendezvous, Shared, SurfaceMap,
};

// `input` builds the host pointer's default (blank) shape from the cursor module; re-exported
// here so its `super::blank_cursor()` call keeps working across the split.
pub(crate) use cursor::blank_cursor;

use cursor::{apply_cursor, update_capture_cursor};
use diag::{
    capture_ids_from_env, capture_iosurface, copy_surface, create_local_iosurface, sync_surface,
};
use lifecycle::{kill_worker_group, should_initiate_quit};
use present::{register_apply_hook, set_layer_surface};

/// Push a window-resize to the worker over its display-control socket (off the AppKit main
/// thread — a brief connect/write must never beachball the UI). Best-effort: a failure just
/// means this gesture's resize is dropped; the next one retries.
fn send_resize(path: &Path, width: u32, height: u32) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        use std::io::Write;
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(mut stream) => {
                if let Err(e) = writeln!(stream, "resize {width} {height}") {
                    log::warn!("window resize: send {width}x{height} failed: {e}");
                } else {
                    log::info!("window resize: pushed {width}x{height} to the guest");
                }
            }
            Err(e) => log::warn!("window resize: connect {path:?} failed: {e}"),
        }
    });
}

/// Run the AppKit window on the main thread. The render timer polls `shared`, updates the
/// window contents, and — when the worker exits, the window is closed, or Ctrl-C is hit —
/// kills the worker's process group (`worker_pid`) and exits the process. (We exit from
/// the timer rather than `NSApplication::stop`, which doesn't return without a UI event.)
#[allow(clippy::too_many_arguments)]
pub fn run(
    shared: Arc<Mutex<Shared>>,
    mtm: MainThreadMarker,
    conn: Arc<WorkerConn>,
    control: Option<crate::control::ControlPlane>,
    resize_socket: Option<PathBuf>,
    surface_map: SurfaceMap,
    remap: limina_input::keymap::KeyRemap,
    title: &str,
) -> ! {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1024.0, 768.0));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str(title));
    // Allow native (Spaces) full screen: the green title-bar button becomes Enter Full Screen
    // and `toggleFullScreen:` (our Cmd-Ctrl-F host shortcut, below) works. Going fullscreen
    // resizes the window, which the existing resize path reflows into the guest resolution.
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
    let view = window.contentView().expect("content view");
    // Layer-HOSTING (not layer-backed): we own the layer, so AppKit never draws over the
    // IOSurface we set as its contents. Order matters: setLayer before setWantsLayer.
    let layer = CALayer::new();
    // The guest scanout is XRGB (opaque; the X/alpha channel is "don't care" and Venus/zink
    // leaves it 0). Our IOSurface is BGRA, so without this Core Animation alpha-blends the
    // surface and a 0 alpha makes the whole desktop composite transparent (reads as black/
    // white depending on the backdrop). Mark the scanout layer opaque so CA ignores alpha.
    layer.setOpaque(true);
    // Pointer-capture cursor overlay: a sublayer we composite the guest cursor into at its
    // guest-reported position while captured (the host NSCursor is hidden then, so without this
    // the cursor would vanish). Hidden by default; positioned/shown by `update_capture_cursor`.
    let cursor_layer = CALayer::new();
    cursor_layer.setHidden(true);
    layer.addSublayer(&cursor_layer);
    view.setLayer(Some(&layer));
    view.setWantsLayer(true);
    // We OWN the layer's contents (the guest IOSurface) — AppKit must never invalidate or redraw
    // them. Without `Never`, the default policy lets AppKit manage the layer across a live window
    // resize, and at the end of the drag it leaves the layer blank (the IOSurface contents are
    // dropped) until a large enough frame change reallocates the backing. `Never` tells AppKit to
    // keep its hands off so our present is the sole authority on what the layer shows.
    view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);
    window.center();
    // Required for hover (non-dragging) motion to be delivered as MouseMoved events.
    window.setAcceptsMouseMovedEvents(true);
    window.makeKeyAndOrderFront(None);
    app.activate();

    // Per-timer state (main thread only).
    let last_gen = Cell::new(0u64);
    let geom = Cell::new((0u32, 0u32));
    // Diagnostic: dump the presented IOSurface to a PNG (no screen-record perm). LIMINA_WINDOW_CAPTURE.
    let capture_path = std::env::var("LIMINA_WINDOW_CAPTURE").ok();
    // Diagnostic: ids to ALSO dump by global lookup each tick (LIMINA_CAPTURE_IDS — see
    // diag::capture_ids_from_env for the format and why).
    let capture_ids: Vec<u32> = capture_ids_from_env();
    let applies = Cell::new(0u64);
    // Runtime window-resize → guest. The 60 Hz timer debounces the window's content size and,
    // once a drag settles, pushes the new size to the worker over `resize_socket` (which forwards
    // it to the live virtio-gpu → the guest re-modesets). `geom` (the guest's current resolution)
    // is the feedback guard: a window that already matches it — including the guest-driven
    // setContentSize echo — sends nothing. See docs/design/runtime-display-resize.md.
    let resize_sent: Cell<(u32, u32)> = Cell::new((0, 0));
    // The layer frame currently applied (window content size in points). Tracked every tick so the
    // scanout layer keeps filling the window DURING a live resize — the guest hasn't re-modeset
    // yet, so CA scales the current surface to the new frame (smooth stretch) instead of leaving
    // the grown window painting black around a stale layer.
    let layer_geom: Cell<(u32, u32)> = Cell::new((0, 0));
    // Diagnostic (LIMINA_PRESENT_COPY=1): never hand the GUEST's scanout surface to Core
    // Animation — copy it into a private 3-deep ring and show the copy. The zero-copy venus
    // path shares mutter's own double-buffered swapchain with the window server; with no
    // flip-completion feedback the guest reuses/repaints a buffer while CA may still be
    // compositing it (IOSurfaceLock is advisory), so mid-repaint states reach glass — seen
    // as the damaged region (a busy window) blinking out while the rest stays intact. The
    // copy decouples CA from the guest's write cycle entirely; if the flicker vanishes with
    // this on, that race is convicted (the real fix is flip-completion pacing, roadmap #8).
    // Env arms it for the whole run; the marker file toggles it LIVE (touch/rm
    // /tmp/limina-present-copy) so an intermittent flicker can be A/B'd within one session —
    // flicker present → touch → gone → rm → returns is the within-session conviction.
    let present_copy_env = std::env::var_os("LIMINA_PRESENT_COPY").is_some();
    // Lock-only variant (LIMINA_PRESENT_LOCK / touch /tmp/limina-present-lock): keep zero-copy,
    // but IOSurfaceLock+Unlock the guest surface before handing it to CA.
    // A/B VERDICT (2026-06-11): FAILED — visibly worse than no mitigation at all (several
    // anomalies within seconds vs ~5 bursts/hour untreated). Kept as a documented negative
    // result: the copy's load-bearing property is IMMUTABILITY, not the GPU-write sync.
    // (a) At present time the repaint may not be submitted to Metal yet (venus ring decode
    // is async), so the lock has nothing to wait on exactly when it matters; (b) even a
    // complete-at-lock frame is repainted by the guest ~33ms later while CA still samples
    // it (the SURFACE_RING reuse race). Do not enable; use LIMINA_PRESENT_COPY. COPY wins if
    // both are set.
    let present_lock_env = std::env::var_os("LIMINA_PRESENT_LOCK").is_some();
    let copy_ring: RefCell<Vec<CFRetained<IOSurfaceRef>>> = RefCell::new(Vec::new());
    let copy_geom = Cell::new((0u32, 0u32));
    let copy_idx = Cell::new(0usize);
    // Cache looked-up surfaces by id (the worker reuses a small fixed set, its double buffer).
    let cache: RefCell<std::collections::HashMap<u32, CFRetained<IOSurfaceRef>>> =
        RefCell::new(std::collections::HashMap::new());
    // Guest-cursor per-timer state: the last applied cursor gen and the IOSurface id of
    // the shape the host pointer currently wears (so we rebuild only on a shape change).
    let last_cursor_gen = Cell::new(0u64);
    let built_cursor: Cell<Option<u32>> = Cell::new(None);
    // The host pointer's guest-shape adoption, shared with the input monitor (which
    // tracks the pointer crossing the view boundary and asserts/clears the shape).
    let host_cursor = input::HostCursor::new();
    // Pointer-capture flag, shared between the input monitor (which toggles it on Cmd-Ctrl-G),
    // the render timer (which composites the guest cursor while it's set), and the capture tap.
    let captured = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Reliable capture container: a session-level CGEventTap that *consumes* mouse events while
    // captured (so clicks/motion can't escape to host windows) and forwards them to the guest's
    // relative device. Needs Accessibility permission; if absent, capture falls back to the local
    // monitor's warp path (see input.rs). Installed once, on the main thread, before `app.run()`.
    let _capture_tap =
        capture_tap::install(conn.clone(), captured.clone(), remap, host_cursor.clone());

    // Shown-ack channel (#8 leg 2): after Core Animation latches a frame, tell the worker
    // "shown <id>" so it can complete the guest's held flush fence. The blocking send is done on
    // a DEDICATED thread, never the AppKit main thread: the ack fd shares a blocking open-file
    // description with the reader (so it can't be made non-blocking), and MSG_DONTWAIT is NOT
    // honored for AF_UNIX stream sockets on macOS — so a worker that briefly stops draining acks
    // (notably the early-boot window just after a reboot relaunch) would otherwise block the main
    // thread and beachball the whole UI. The completion block only `try_send`s the id (bounded,
    // best-effort — drop if full); this thread sends it to whichever worker is current (conn is
    // swapped on relaunch). A dropped ack is covered by the worker's fallback deadline.
    let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<u32>(64);
    {
        let conn = conn.clone();
        std::thread::spawn(move || {
            while let Ok(id) = ack_rx.recv() {
                // Snapshot the current worker's endpoints and hold the Arc across the send: the
                // ack fd can't be closed (nor its number reused) mid-send even if a relaunch
                // retires this worker concurrently.
                let io = conn.io();
                let line = format!("shown {id}\n");
                // Blocking is fine here — a wedged/booting worker only stalls this thread.
                unsafe {
                    libc::send(
                        io.ack_fd(),
                        line.as_ptr() as *const libc::c_void,
                        line.len(),
                        0,
                    );
                }
            }
        });
    }

    // The frame-apply path, shared by the 60 Hz timer (fallback/liveness) and the
    // dispatch wake-up from the reader thread (event-driven, leg 1 of the latency
    // collapse): applying the moment a frame arrives instead of at the next tick.
    let apply: std::rc::Rc<dyn Fn()> = std::rc::Rc::new({
        let shared = shared.clone();
        let window = window.clone();
        let layer = layer.clone();
        let ack_tx = ack_tx.clone();
        let surface_map = surface_map.clone();
        move || {
            // Keep the scanout layer filling the window every tick — INCLUDING mid live-resize
            // (the timer now fires in common modes, so this runs during the drag). The window
            // grows/shrinks before the guest re-modesets; without this the layer keeps its old
            // frame and the surrounding window paints black. CA scales the current surface to the
            // new frame, so the desktop stretches smoothly during the drag and snaps crisp once
            // the guest re-modesets to the settled size.
            if let Some(v) = window.contentView() {
                let sz = v.frame().size;
                let wh = (sz.width.round() as u32, sz.height.round() as u32);
                if wh != layer_geom.get() && wh.0 > 0 && wh.1 > 0 {
                    layer_geom.set(wh);
                    CATransaction::begin();
                    CATransaction::setDisableActions(true);
                    layer.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), sz));
                    CATransaction::commit();
                }
            }

            // Push the new window size to the guest ONCE the resize gesture ENDS — never during
            // the drag. `inLiveResize()` is true for the whole drag; firing while it's true would
            // re-modeset the guest dozens of times mid-gesture (surface churn + cache clears →
            // the window blanks). So we wait for the drag to finish, then send the settled size.
            // The layer-tracking above keeps the desktop filling the window (scaled) during the
            // drag. Only active once the guest has presented a frame (so `geom` is a real
            // baseline, not 0×0), and skipped when the window already matches the guest (the
            // feedback guard against the guest-driven setContentSize echo).
            if let Some(sock) = &resize_socket {
                let base = geom.get();
                let view = window.contentView();
                let in_live = view.as_ref().map(|v| v.inLiveResize()).unwrap_or(false);
                let size = view
                    .map(|v| v.frame().size)
                    .unwrap_or(NSSize::new(0.0, 0.0));
                let want = (size.width.round() as u32, size.height.round() as u32);
                if base != (0, 0)
                    && !in_live
                    && want.0 >= 64
                    && want.1 >= 64
                    && want != base
                    && want != resize_sent.get()
                {
                    resize_sent.set(want);
                    send_resize(sock, want.0, want.1);
                }
            }

            let (gen, show_id, width, height) = {
                let s = shared.lock().unwrap();
                (s.gen, s.show_id, s.width, s.height)
            };
            if gen == last_gen.get() {
                return;
            }
            last_gen.set(gen);

            let Some(id) = show_id else { return };
            if geom.get() != (width, height) {
                geom.set((width, height));
                window.setContentSize(NSSize::new(width as f64, height as f64));
                // A layer-HOSTING view doesn't auto-size its layer — give it the view's bounds
                // or it stays 0×0 and nothing shows on screen (even though contents is set).
                let bounds = NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(width as f64, height as f64),
                );
                // No implicit animation on the resize either (same reason as set_layer_surface).
                CATransaction::begin();
                CATransaction::setDisableActions(true);
                layer.setFrame(bounds);
                CATransaction::commit();
                // A mode change means the worker allocated fresh surfaces; ids from the old mode
                // are gone (and could be reused for unrelated surfaces), so drop the cache.
                cache.borrow_mut().clear();
            }

            let mut cache = cache.borrow_mut();
            // Resolve the id to a surface once and keep our own retained reference. Prefer the
            // Mach-delivered store (the capability-scoped, non-global scanouts); fall back to a
            // global `IOSurfaceLookup` for the venus zero-copy path (still global) and the legacy
            // no-receiver mode. A failed resolve (the worker freed it during a rapid remodeset
            // before we caught up) is not fatal — skip this frame rather than panic the UI.
            use std::collections::hash_map::Entry;
            let surface = match cache.entry(id) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let resolved = surface_map
                        .lock()
                        .unwrap()
                        .get(id)
                        .or_else(|| IOSurfaceLookup(id));
                    match resolved {
                        Some(s) => e.insert(s),
                        None => {
                            log::warn!("window: surface {id} unresolved; skipping frame");
                            return;
                        }
                    }
                }
            };
            // Shown-ack channel (#8 leg 2): after Core Animation processes this frame's
            // transaction, hand the id to the dedicated ack-sender thread (a bounded, non-blocking
            // try_send) so it can tell the worker "shown <id>" at the real latch boundary — the
            // blocking socket write never touches the AppKit main thread. The ack identifies the
            // frame by the GUEST's surface id even in copy mode (the worker tracks holds by the id
            // it presented); the sender thread targets whichever worker is current after a relaunch.
            let ack_for_frame = Some((ack_tx.clone(), id));
            // Distinct object each frame (the worker alternates ids) → CA re-reads.
            let present_copy =
                present_copy_env || std::fs::metadata("/tmp/limina-present-copy").is_ok();
            if present_copy {
                if copy_geom.get() != (width, height) {
                    copy_geom.set((width, height));
                    let mut ring = copy_ring.borrow_mut();
                    ring.clear();
                    for _ in 0..3 {
                        if let Some(s) = create_local_iosurface(width, height) {
                            ring.push(s);
                        }
                    }
                }
                let ring = copy_ring.borrow();
                if ring.len() == 3 {
                    let dst = &ring[copy_idx.get() % 3];
                    copy_idx.set(copy_idx.get().wrapping_add(1));
                    copy_surface(surface, dst);
                    set_layer_surface(&layer, dst, ack_for_frame);
                } else {
                    set_layer_surface(&layer, surface, ack_for_frame);
                }
            } else {
                let present_lock =
                    present_lock_env || std::fs::metadata("/tmp/limina-present-lock").is_ok();
                if present_lock {
                    sync_surface(surface);
                }
                set_layer_surface(&layer, surface, ack_for_frame);
            }

            // Diagnostic capture of the presented scanout. Periodic (overwrite) so a
            // long-running headless check ends with a recent frame, not just early boot.
            applies.set(applies.get() + 1);
            if applies.get() % 120 == 0 {
                if let Some(path) = &capture_path {
                    capture_iosurface(surface, id, path);
                }
            }
            // Targeted per-id sweep — look each requested global id up fresh (no cache) and
            // dump it, so we can read the venus blob surface directly even when it isn't the
            // presented one.
            if !capture_ids.is_empty() && applies.get() % 30 == 0 {
                if let Some(base) = &capture_path {
                    for &cid in &capture_ids {
                        if let Some(s) = IOSurfaceLookup(cid) {
                            capture_iosurface(&s, cid, &format!("{base}.id{cid}.png"));
                        } else {
                            log::info!("capture: IOSurfaceLookup({cid}) -> none (not alive)");
                        }
                    }
                }
            }
        }
    });
    register_apply_hook(apply.clone());

    // Quit escalation state: set when the user closed the window / hit Ctrl-C and we asked
    // the guest agent to power off; reaching the deadline falls back to SIGKILL.
    let quit_deadline: Cell<Option<std::time::Instant>> = Cell::new(None);

    // Clone a window handle for the input monitor's fullscreen shortcut BEFORE the timer block
    // below moves `window` in.
    let shortcut_window = window.clone();

    let timer_cursor = host_cursor.clone();
    let timer_conn = conn.clone();
    let timer_captured = captured.clone();
    let timer_cursor_layer = cursor_layer.clone();
    let timer_surface_map = surface_map.clone();
    // For the quit-check below: distinguish a real window CLOSE from a mere miniaturize/app-hide
    // (all three make the window not-visible, but only a close should power the guest off).
    let timer_app = app.clone();
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let exited = shared.lock().unwrap().worker_exited;

        // Worker gone (guest powered off, orderly or not): net any process-group
        // stragglers and exit. (`conn.pid()` is the *current* worker — relaunch keeps it fresh.)
        if exited {
            kill_worker_group(timer_conn.pid());
            crate::gateway::cleanup();
            crate::control::cleanup();
            std::process::exit(0);
        }

        // The user closed the window or hit Ctrl-C: prefer an orderly guest power-off via
        // the agent control plane (window-close = the M2 orderly-shutdown clause); without
        // an agent — or once the grace runs out — kill the worker's process group. A minimized
        // window or a hidden app is NOT a close (both report isVisible()==false) — keep running.
        if should_initiate_quit(
            crate::supervisor::stop_requested(),
            window.isVisible(),
            window.isMiniaturized(),
            timer_app.isHidden(),
        ) {
            // A SECOND stop signal (limina stop --force, impatient double Ctrl-C)
            // skips whatever grace remains — mirror of the headless monitor ladder.
            let force_now = crate::supervisor::force_stop_requested()
                || match quit_deadline.get() {
                    None => {
                        let orderly = control
                            .as_ref()
                            .map(|c| c.request_shutdown(crate::control::AGENT_GRACE))
                            .unwrap_or(false);
                        if orderly {
                            log::info!("window closed → asked the guest agent to power off");
                            quit_deadline.set(Some(
                                std::time::Instant::now() + crate::control::AGENT_GRACE,
                            ));
                            false
                        } else {
                            true
                        }
                    }
                    Some(d) => std::time::Instant::now() >= d,
                };
            if force_now {
                kill_worker_group(timer_conn.pid());
                crate::gateway::cleanup();
                crate::control::cleanup();
                std::process::exit(0);
            }
        }

        // Guest cursor shape first — it has its own gen so a shape change (or hide)
        // applies even when the scanout hasn't produced a new frame.
        let cur = {
            let s = shared.lock().unwrap();
            (
                s.cursor_gen,
                s.cursor_visible,
                s.cursor_id,
                s.cursor_w,
                s.cursor_h,
                s.hot_x,
                s.hot_y,
            )
        };
        if cur.0 != last_cursor_gen.get() {
            last_cursor_gen.set(cur.0);
            apply_cursor(&timer_cursor, &built_cursor, &cur, &surface_map);
        }
        // Pointer-capture cursor: while captured, composite the guest cursor at its reported
        // position (the host NSCursor is hidden then). Position moves every frame, so unlike the
        // shape this runs every tick, not gated on `cursor_gen`.
        update_capture_cursor(
            &timer_cursor_layer,
            &timer_captured,
            &shared,
            &timer_surface_map,
        );

        // Frame apply: normally event-driven (dispatch from the reader thread); this is
        // the fallback so a lost wake-up costs one tick, not a stuck frame.
        apply();
    });

    // ~60 Hz poll. Keep the timer alive for the app's lifetime. Schedule it in COMMON modes (not
    // just the default mode): `scheduledTimer...` adds it to NSDefaultRunLoopMode only, so it
    // FREEZES during a live window resize (the run loop runs in NSEventTrackingRunLoopMode while
    // the user drags). A frozen present timer leaves the layer stale → the window goes black mid/
    // post-drag and only recovers on the next forced repaint. Common modes keeps it firing through
    // the drag, so the present + resize-detection stay live the whole time.
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(1.0 / 60.0, true, &block) };
    unsafe {
        NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
    }
    let _timer = timer;

    // Capture keyboard + mouse via a local event monitor and forward them to the worker as
    // evdev events. Swallowed key events return null; pass-through events return themselves.
    let input_state = input::InputState::new(conn.clone(), host_cursor.clone(), remap, captured);
    let monitor_view = view.clone();
    let input_block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: the monitor hands us a valid, live event for the call's duration.
        let ev = unsafe { event.as_ref() };
        // Host shortcuts are intercepted BEFORE the guest sees them. We match on key-down; the
        // orphan key-up that leaks to the guest is dropped by the guest input core (a release of
        // an un-pressed key). Modifier flagsChanged still flow through, which is fine.
        if ev.r#type() == NSEventType::KeyDown {
            if let Some(sc) = input::match_host_shortcut(ev.keyCode(), ev.modifierFlags().0 as u64)
            {
                match sc {
                    input::HostShortcut::ToggleFullScreen => {
                        shortcut_window.toggleFullScreen(None);
                    }
                    input::HostShortcut::ToggleCapture => {
                        input_state.toggle_capture();
                    }
                }
                return std::ptr::null_mut(); // swallow — don't forward to the guest
            }
        }
        let swallow = input_state.handle(ev, &monitor_view);
        if swallow {
            std::ptr::null_mut()
        } else {
            event.as_ptr()
        }
    });
    let input_mask = NSEventMask::KeyDown
        | NSEventMask::KeyUp
        | NSEventMask::FlagsChanged
        | NSEventMask::MouseMoved
        | NSEventMask::LeftMouseDown
        | NSEventMask::LeftMouseUp
        | NSEventMask::LeftMouseDragged
        | NSEventMask::RightMouseDown
        | NSEventMask::RightMouseUp
        | NSEventMask::RightMouseDragged
        | NSEventMask::OtherMouseDown
        | NSEventMask::OtherMouseUp
        | NSEventMask::OtherMouseDragged
        | NSEventMask::ScrollWheel;
    // Keep the monitor alive for the app's lifetime (dropping it removes the monitor).
    let _monitor =
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(input_mask, &input_block) };

    app.run();
    // The run loop only returns if AppKit tears down unexpectedly; the timer is what
    // normally exits us. Either way, don't fall through.
    crate::gateway::cleanup();
    crate::control::cleanup();
    std::process::exit(0);
}
