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

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSCursor, NSEvent,
    NSEventMask, NSImage, NSViewLayerContentsRedrawPolicy, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber,
    CFNumberType, CFRetained, CFString,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow,
    CGBitmapContextGetData, CGBitmapInfo, CGColorSpace, CGContext, CGImageAlphaInfo,
};
use objc2_foundation::{
    NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use objc2_io_surface::{
    kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat,
    kIOSurfaceWidth, IOSurfaceCreate, IOSurfaceGetBaseAddress, IOSurfaceGetBytesPerRow,
    IOSurfaceGetHeight, IOSurfaceGetWidth, IOSurfaceLock, IOSurfaceLockOptions, IOSurfaceLookup,
    IOSurfaceRef, IOSurfaceUnlock,
};
use objc2_quartz_core::{CALayer, CATransaction};

mod input;

/// The supervisor's live connection to the *current* worker: its pid (for shutdown signaling)
/// and the supervisor-side fds the window talks to it through (input sinks + the shown-ack fd).
///
/// All are swapped atomically when the worker is relaunched after a guest reboot, so the window
/// keeps the same NSWindow/layer/event-monitor and just retargets whichever worker is current.
/// Readers (the AppKit main thread) load each field fresh; the relaunch path publishes the new
/// worker's values via [`WorkerConn::swap`] *before* closing the old fds, so a concurrent input
/// `send` can't hit a reused fd number. A `pid` of 0 / fd of -1 means "no current worker".
pub struct WorkerConn {
    pid: AtomicI32,
    kbd_fd: AtomicI32,
    ptr_fd: AtomicI32,
    ack_fd: AtomicI32,
}

impl WorkerConn {
    pub fn new(pid: i32, kbd_fd: RawFd, ptr_fd: RawFd, ack_fd: RawFd) -> Arc<Self> {
        Arc::new(Self {
            pid: AtomicI32::new(pid),
            kbd_fd: AtomicI32::new(kbd_fd),
            ptr_fd: AtomicI32::new(ptr_fd),
            ack_fd: AtomicI32::new(ack_fd),
        })
    }

    pub fn pid(&self) -> i32 {
        self.pid.load(Ordering::Acquire)
    }
    pub fn kbd_fd(&self) -> RawFd {
        self.kbd_fd.load(Ordering::Acquire)
    }
    pub fn ptr_fd(&self) -> RawFd {
        self.ptr_fd.load(Ordering::Acquire)
    }
    pub fn ack_fd(&self) -> RawFd {
        self.ack_fd.load(Ordering::Acquire)
    }

    /// Publish a freshly-spawned worker's pid + supervisor-side fds (called on relaunch).
    pub fn swap(&self, pid: i32, kbd_fd: RawFd, ptr_fd: RawFd, ack_fd: RawFd) {
        self.kbd_fd.store(kbd_fd, Ordering::Release);
        self.ptr_fd.store(ptr_fd, Ordering::Release);
        self.ack_fd.store(ack_fd, Ordering::Release);
        self.pid.store(pid, Ordering::Release);
    }
}

// libdispatch: wake the main thread to apply a frame the moment it arrives, instead of
// waiting out the 60 Hz poll timer (leg 1 of the present-latency collapse, #8). The
// trampoline runs on the main thread via the main queue, which NSApplication's run loop
// services; it calls the frame-apply hook `run()` registers.
#[allow(non_camel_case_types)]
type dispatch_queue_t = *mut std::ffi::c_void;
extern "C" {
    static _dispatch_main_q: std::ffi::c_void;
    fn dispatch_async_f(
        queue: dispatch_queue_t,
        context: *mut std::ffi::c_void,
        work: extern "C" fn(*mut std::ffi::c_void),
    );
}

thread_local! {
    /// Main-thread frame-apply hook (set once by `run()`); the dispatch trampoline calls it.
    static APPLY_HOOK: RefCell<Option<std::rc::Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

extern "C" fn apply_trampoline(_ctx: *mut std::ffi::c_void) {
    let hook = APPLY_HOOK.with(|h| h.borrow().clone());
    if let Some(f) = hook {
        f();
    }
}

/// Schedule an immediate frame apply on the main thread (callable from any thread).
fn wake_main_apply() {
    unsafe {
        let main_q = &_dispatch_main_q as *const _ as dispatch_queue_t;
        dispatch_async_f(main_q, std::ptr::null_mut(), apply_trampoline);
    }
}

/// State shared between the control-channel reader thread, the worker monitor, and the
/// main-thread render timer. Only `Send` data — never AppKit objects.
#[derive(Default)]
pub struct Shared {
    /// The surface id the worker wants shown right now (alternates between its double
    /// buffer so the layer's contents object changes and Core Animation re-reads).
    show_id: Option<u32>,
    width: u32,
    height: u32,
    /// Bumped on any update (new surface geometry or a new frame) — the timer re-applies.
    gen: u64,
    /// Set when the worker/control channel is gone — the window should close.
    worker_exited: bool,

    /// Guest hardware-cursor state (decoupled from the scanout above; the worker publishes
    /// the cursor image as its own IOSurface). The host pointer *adopts* this shape over
    /// the guest view (see `HostCursor`); guest-reported positions are ignored — the
    /// pointer the user sees is the host one, which the guest tracks via absolute input.
    cursor_id: Option<u32>,
    cursor_w: u32,
    cursor_h: u32,
    hot_x: u32,
    hot_y: u32,
    cursor_visible: bool,
    /// Bumped on any cursor shape/visibility change — the timer re-applies.
    cursor_gen: u64,
}

impl Shared {
    pub fn new() -> Arc<Mutex<Shared>> {
        Arc::new(Mutex::new(Shared::default()))
    }
}

/// Mark the worker as gone (called by the monitor when the worker exits).
pub fn mark_worker_exited(shared: &Arc<Mutex<Shared>>) {
    shared.lock().unwrap().worker_exited = true;
}

/// Read the control channel on a background thread, updating `shared`. Consumes `fd`.
pub fn spawn_reader(fd: RawFd, shared: Arc<Mutex<Shared>>) {
    std::thread::spawn(move || {
        // SAFETY: we own `fd` (the supervisor's end of the control socketpair).
        let reader = BufReader::new(unsafe { File::from_raw_fd(fd) });
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("surface") => {
                    log::info!("window: <- {line}");
                    // surface <id0> <id1> <w> <h> — geometry + the initial buffer to show.
                    let id0 = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let _id1 = parts.next();
                    let w = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
                    if let (Some(id0), Some(w), Some(h)) = (id0, w, h) {
                        let mut s = shared.lock().unwrap();
                        s.show_id = Some(id0);
                        s.width = w;
                        s.height = h;
                        s.gen += 1;
                        drop(s);
                        wake_main_apply();
                    }
                }
                Some("frame") => {
                    // frame <id> — the buffer to show now.
                    if let Some(id) = parts.next().and_then(|s| s.parse::<u32>().ok()) {
                        let mut s = shared.lock().unwrap();
                        s.show_id = Some(id);
                        s.gen += 1;
                        drop(s);
                        wake_main_apply();
                    }
                }
                Some("cursor") => {
                    // cursor <id> <w> <h> <hot_x> <hot_y> — new cursor image + hotspot.
                    let id = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let w = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let hx = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let hy = parts.next().and_then(|s| s.parse::<u32>().ok());
                    if let (Some(id), Some(w), Some(h), Some(hx), Some(hy)) = (id, w, h, hx, hy) {
                        let mut s = shared.lock().unwrap();
                        s.cursor_id = Some(id);
                        s.cursor_w = w;
                        s.cursor_h = h;
                        s.hot_x = hx;
                        s.hot_y = hy;
                        s.cursor_visible = true;
                        s.cursor_gen += 1;
                    }
                }
                Some("cursormove") => {
                    // cursormove <x> <y> — the guest's cursor position. Deliberately
                    // ignored: the visible pointer is the HOST cursor (wearing the guest
                    // shape), and the guest position only echoes our own absolute input
                    // back with a round-trip of lag. (A guest-initiated warp is the one
                    // thing this loses; revisit with pointer capture.)
                }
                Some("cursorhide") => {
                    let mut s = shared.lock().unwrap();
                    s.cursor_visible = false;
                    s.cursor_gen += 1;
                }
                _ => {}
            }
        }
        // The control channel closed: this worker is gone. Do NOT set worker_exited here — the
        // worker *monitor* is the sole authority on that, because a guest reboot also closes this
        // channel and we must NOT quit the window before the monitor relaunches a new worker. On
        // a relaunch a fresh reader is spawned on the new channel; this thread just ends.
        log::info!("window: control channel closed (worker gone)");
    });
}

/// Run the AppKit window on the main thread. The render timer polls `shared`, updates the
/// window contents, and — when the worker exits, the window is closed, or Ctrl-C is hit —
/// kills the worker's process group (`worker_pid`) and exits the process. (We exit from
/// the timer rather than `NSApplication::stop`, which doesn't return without a UI event.)
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

pub fn run(
    shared: Arc<Mutex<Shared>>,
    mtm: MainThreadMarker,
    conn: Arc<WorkerConn>,
    control: Option<crate::control::ControlPlane>,
    resize_socket: Option<PathBuf>,
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
    window.setTitle(&NSString::from_str("limina"));
    let view = window.contentView().expect("content view");
    // Layer-HOSTING (not layer-backed): we own the layer, so AppKit never draws over the
    // IOSurface we set as its contents. Order matters: setLayer before setWantsLayer.
    let layer = CALayer::new();
    // The guest scanout is XRGB (opaque; the X/alpha channel is "don't care" and Venus/zink
    // leaves it 0). Our IOSurface is BGRA, so without this Core Animation alpha-blends the
    // surface and a 0 alpha makes the whole desktop composite transparent (reads as black/
    // white depending on the backdrop). Mark the scanout layer opaque so CA ignores alpha.
    layer.setOpaque(true);
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
    // Diagnostic: ALSO dump specific global IOSurface ids by lookup each tick, regardless of
    // what the window presents. Lets us peek the venus SET_SCANOUT_BLOB surface (e.g. id 38)
    // even when a competing 2D ring is what's on screen. LIMINA_CAPTURE_IDS="33,38,39".
    // Accepts a comma list ("33,38") and/or inclusive ranges ("30-50").
    let capture_ids: Vec<u32> = std::env::var("LIMINA_CAPTURE_IDS")
        .ok()
        .map(|s| {
            let mut out = Vec::new();
            for t in s.split(',') {
                let t = t.trim();
                if let Some((a, b)) = t.split_once('-') {
                    if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                        out.extend(a..=b);
                    }
                } else if let Ok(v) = t.parse::<u32>() {
                    out.push(v);
                }
            }
            out
        })
        .unwrap_or_default();
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
                let fd = conn.ack_fd();
                if fd < 0 {
                    continue;
                }
                let line = format!("shown {id}\n");
                // Blocking is fine here — a wedged/booting worker only stalls this thread.
                unsafe {
                    libc::send(fd, line.as_ptr() as *const libc::c_void, line.len(), 0);
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
            // Look the surface up by id once and keep our own retained reference. A failed
            // lookup (the worker freed it during a rapid remodeset before we caught up) is not
            // fatal — skip this frame rather than panic the UI; the next frame recovers.
            use std::collections::hash_map::Entry;
            let surface = match cache.entry(id) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => match IOSurfaceLookup(id) {
                    Some(s) => e.insert(s),
                    None => {
                        log::warn!("window: IOSurfaceLookup({id}) failed; skipping frame");
                        return;
                    }
                },
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
    APPLY_HOOK.with(|h| *h.borrow_mut() = Some(apply.clone()));

    // Quit escalation state: set when the user closed the window / hit Ctrl-C and we asked
    // the guest agent to power off; reaching the deadline falls back to SIGKILL.
    let quit_deadline: Cell<Option<std::time::Instant>> = Cell::new(None);

    let timer_cursor = host_cursor.clone();
    let timer_conn = conn.clone();
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let exited = shared.lock().unwrap().worker_exited;

        // Worker gone (guest powered off, orderly or not): net any process-group
        // stragglers and exit. (`conn.pid()` is the *current* worker — relaunch keeps it fresh.)
        if exited {
            unsafe { libc::kill(-timer_conn.pid(), libc::SIGKILL) };
            crate::gateway::cleanup();
            crate::control::cleanup();
            std::process::exit(0);
        }

        // The user closed the window or hit Ctrl-C: prefer an orderly guest power-off via
        // the agent control plane (window-close = the M2 orderly-shutdown clause); without
        // an agent — or once the grace runs out — kill the worker's process group.
        if crate::supervisor::stop_requested() || !window.isVisible() {
            let force_now = match quit_deadline.get() {
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
                unsafe { libc::kill(-timer_conn.pid(), libc::SIGKILL) };
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
            apply_cursor(&timer_cursor, &built_cursor, &cur);
        }

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
    let input_state = input::InputState::new(conn.clone(), host_cursor.clone());
    let monitor_view = view.clone();
    let input_block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: the monitor hands us a valid, live event for the call's duration.
        let ev = unsafe { event.as_ref() };
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

/// GPU-coherent diagnostic capture: lock the IOSurface (which syncs against the GPU), read its
/// BGRA bytes directly, and write a PNG with alpha forced opaque. Needs no Screen Recording
/// permission. (A premultiplied `CALayer.renderInContext` would zero the RGB wherever the guest's
/// "don't care" scanout alpha is 0; reading the surface directly shows the true scanout content.)
fn capture_iosurface(surface: &IOSurfaceRef, id: u32, path: &str) {
    use objc2_io_surface::IOSurfaceLockOptions;
    unsafe {
        // ReadOnly + default (no AvoidSync) → waits for the GPU to finish writing.
        if surface.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut()) != 0 {
            log::error!("capture: IOSurfaceLock failed");
            return;
        }
        let w = surface.width();
        let h = surface.height();
        let bpr = surface.bytes_per_row();
        let base = surface.base_address().as_ptr() as *const u8;
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            let row = base.add(y * bpr);
            for x in 0..w {
                let px = row.add(x * 4); // BGRA in memory
                let b = *px;
                let g = *px.add(1);
                let r = *px.add(2);
                let o = (y * w + x) * 4;
                rgba[o] = r;
                rgba[o + 1] = g;
                rgba[o + 2] = b;
                rgba[o + 3] = 255; // force opaque — the scanout alpha is "don't care"
            }
        }
        let _ = surface.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());

        match std::fs::File::create(path) {
            Ok(f) => {
                let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                match enc
                    .write_header()
                    .and_then(|mut wr| wr.write_image_data(&rgba))
                {
                    Ok(()) => {
                        // Report a coarse luminance sum so the log alone tells black vs content.
                        let nonzero = rgba
                            .chunks_exact(4)
                            .filter(|p| p[0] | p[1] | p[2] != 0)
                            .count();
                        log::info!(
                            "capture: wrote IOSurface id={id} {w}x{h} to {path} (nonzero_px={nonzero})"
                        );
                    }
                    Err(e) => log::error!("capture: png write failed: {e}"),
                }
            }
            Err(e) => log::error!("capture: create {path} failed: {e}"),
        }
    }
}

/// Apply the latest guest cursor state to the host pointer. `cur` is
/// `(gen, visible, id, w, h, hot_x, hot_y)`; `built` caches the IOSurface id of the shape
/// the host pointer already wears, so we only rebuild on an actual shape change (the
/// worker publishes each shape as a fresh IOSurface and keeps it alive until the next).
fn apply_cursor(
    host: &input::HostCursor,
    built: &Cell<Option<u32>>,
    cur: &(u64, bool, Option<u32>, u32, u32, u32, u32),
) {
    let (_gen, visible, id, w, h, hot_x, hot_y) = *cur;
    match id {
        Some(id) if visible && w > 0 && h > 0 => {
            if built.get() == Some(id) {
                return;
            }
            match build_guest_cursor(id, w, h, hot_x, hot_y) {
                Some(c) => {
                    host.update(c);
                    built.set(Some(id));
                }
                None => log::warn!("window: building guest cursor from IOSurface {id} failed"),
            }
        }
        _ => {
            // The guest hid its cursor: honor that with a blank (fully transparent) host
            // cursor over the view, falling back to the arrow if we can't build one. A
            // hide before any shape was ever built keeps the default arrow (early boot).
            if built.get().is_some() {
                built.set(None);
                host.update(blank_cursor().unwrap_or_else(NSCursor::arrowCursor));
            }
        }
    }
}

/// Build an `NSCursor` wearing the guest's cursor image: look up the worker-published
/// IOSurface (BGRA, premultiplied alpha), copy it through a `CGBitmapContext` into a
/// `CGImage`, and wrap it with the guest's hotspot (top-left origin, as NSCursor expects).
/// 1 px = 1 pt — the window presents the scanout 1:1 today; revisit for HiDPI.
fn build_guest_cursor(
    id: u32,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
) -> Option<Retained<NSCursor>> {
    let surface = IOSurfaceLookup(id)?;
    let ctx = bgra_bitmap_context(w, h)?;
    unsafe {
        let dst = CGBitmapContextGetData(Some(&ctx)) as *mut u8;
        if dst.is_null() {
            return None;
        }
        let dst_bpr = CGBitmapContextGetBytesPerRow(Some(&ctx));
        if IOSurfaceLock(
            &surface,
            IOSurfaceLockOptions::ReadOnly,
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        let src = IOSurfaceGetBaseAddress(&surface).as_ptr() as *const u8;
        let src_bpr = IOSurfaceGetBytesPerRow(&surface);
        let row = (w as usize * 4)
            .min(dst_bpr)
            .min(src_bpr)
            .min(IOSurfaceGetWidth(&surface) * 4);
        let rows = (h as usize).min(IOSurfaceGetHeight(&surface));
        for y in 0..rows {
            std::ptr::copy_nonoverlapping(src.add(y * src_bpr), dst.add(y * dst_bpr), row);
        }
        IOSurfaceUnlock(
            &surface,
            IOSurfaceLockOptions::ReadOnly,
            std::ptr::null_mut(),
        );
    }
    nscursor_from_context(&ctx, w, h, hot_x, hot_y)
}

/// A fully transparent 1×1 cursor — what the host pointer wears while the guest hides its
/// own (so "no pointer" is honored instead of showing a stale arrow over the view).
fn blank_cursor() -> Option<Retained<NSCursor>> {
    let ctx = bgra_bitmap_context(1, 1)?;
    unsafe {
        let dst = CGBitmapContextGetData(Some(&ctx)) as *mut u32;
        if dst.is_null() {
            return None;
        }
        dst.write(0);
    }
    nscursor_from_context(&ctx, 1, 1, 0, 0)
}

/// A BGRA (premultiplied, little-endian) bitmap context matching the worker's cursor
/// IOSurface layout, so guest pixels copy in byte-for-byte.
fn bgra_bitmap_context(w: u32, h: u32) -> Option<CFRetained<CGContext>> {
    let space = CGColorSpace::new_device_rgb()?;
    let info = CGBitmapInfo::ByteOrder32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    // SAFETY: null data = CG allocates (and owns) the backing store; 0 bytes-per-row = CG
    // chooses. All other arguments are plain values.
    unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            w as usize,
            h as usize,
            8,
            0,
            Some(&space),
            info,
        )
    }
}

fn nscursor_from_context(
    ctx: &CGContext,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
) -> Option<Retained<NSCursor>> {
    let img = CGBitmapContextCreateImage(Some(ctx))?;
    let size = NSSize::new(w as f64, h as f64);
    let nsimage = NSImage::initWithCGImage_size(NSImage::alloc(), &img, size);
    Some(NSCursor::initWithImage_hotSpot(
        NSCursor::alloc(),
        &nsimage,
        NSPoint::new(hot_x as f64, hot_y as f64),
    ))
}

/// Set an IOSurface as the layer's contents (it's a CF object accepted by `contents`).
///
/// Wrapped in a `CATransaction` with actions disabled: this is a layer-HOSTING layer, so a
/// Plain local BGRA IOSurface for the LIMINA_PRESENT_COPY ring (not global — only this
/// process touches it).
fn create_local_iosurface(width: u32, height: u32) -> Option<CFRetained<IOSurfaceRef>> {
    use std::ffi::c_void;
    let pixel_format = i32::from_be_bytes(*b"BGRA");
    // Align the row stride to 256 bytes — a tight `width*4` stride composites BLANK in CoreAnimation
    // for widths that aren't 64-aligned (see the matching note in limina-display's
    // create_global_iosurface). `copy_surface` honors both surfaces' real `bytesPerRow`.
    let bytes_per_row = (((width * 4) + 255) & !255) as i32;
    unsafe fn cfnum(v: i32) -> Option<CFRetained<CFNumber>> {
        unsafe {
            CFNumber::new(
                None,
                CFNumberType::SInt32Type,
                &v as *const i32 as *const c_void,
            )
        }
    }
    unsafe {
        let vw = cfnum(width as i32)?;
        let vh = cfnum(height as i32)?;
        let vbpe = cfnum(4)?;
        let vbpr = cfnum(bytes_per_row)?;
        let vpf = cfnum(pixel_format)?;
        let mut keys: [*const c_void; 5] = [
            (kIOSurfaceWidth as *const CFString).cast(),
            (kIOSurfaceHeight as *const CFString).cast(),
            (kIOSurfaceBytesPerElement as *const CFString).cast(),
            (kIOSurfaceBytesPerRow as *const CFString).cast(),
            (kIOSurfacePixelFormat as *const CFString).cast(),
        ];
        let mut values: [*const c_void; 5] = [
            (&*vw as *const CFNumber).cast(),
            (&*vh as *const CFNumber).cast(),
            (&*vbpe as *const CFNumber).cast(),
            (&*vbpr as *const CFNumber).cast(),
            (&*vpf as *const CFNumber).cast(),
        ];
        let dict = CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )?;
        IOSurfaceCreate(&dict)
    }
}

/// Row-wise copy of one BGRA IOSurface into another (clamped to the smaller geometry).
/// ~4 MB/frame at 1280×800 — trivially cheap next to what it buys (see LIMINA_PRESENT_COPY).
/// Wait for in-flight GPU writes to the surface to land, then release it untouched.
/// IOSurfaceLock is the only cross-process "GPU writes done?" primitive available to us
/// here; the lock/unlock pair costs only the wait itself (no copy, no page faults).
fn sync_surface(surface: &CFRetained<IOSurfaceRef>) {
    unsafe {
        IOSurfaceLock(
            surface,
            IOSurfaceLockOptions::ReadOnly,
            std::ptr::null_mut(),
        );
        IOSurfaceUnlock(
            surface,
            IOSurfaceLockOptions::ReadOnly,
            std::ptr::null_mut(),
        );
    }
}

fn copy_surface(src: &CFRetained<IOSurfaceRef>, dst: &CFRetained<IOSurfaceRef>) {
    unsafe {
        IOSurfaceLock(src, IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
        IOSurfaceLock(dst, IOSurfaceLockOptions(0), std::ptr::null_mut());
        let sb = IOSurfaceGetBaseAddress(src).as_ptr() as *const u8;
        let db = IOSurfaceGetBaseAddress(dst).as_ptr() as *mut u8;
        let ss = IOSurfaceGetBytesPerRow(src);
        let ds = IOSurfaceGetBytesPerRow(dst);
        let w = IOSurfaceGetWidth(src).min(IOSurfaceGetWidth(dst));
        let h = IOSurfaceGetHeight(src).min(IOSurfaceGetHeight(dst));
        let row = w * 4;
        for y in 0..h {
            std::ptr::copy_nonoverlapping(sb.add(y * ss), db.add(y * ds), row);
        }
        IOSurfaceUnlock(dst, IOSurfaceLockOptions(0), std::ptr::null_mut());
        IOSurfaceUnlock(src, IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
    }
}

/// `contents` change otherwise fires an implicit ~0.25 s fade. At 60 fps the fades overlap
/// and the guest desktop visibly flickers; disabling actions makes each frame swap instant.
fn set_layer_surface(
    layer: &CALayer,
    surface: &CFRetained<IOSurfaceRef>,
    ack: Option<(std::sync::mpsc::SyncSender<u32>, u32)>,
) {
    let obj: &AnyObject = unsafe { &*(&**surface as *const IOSurfaceRef as *const AnyObject) };
    unsafe {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // #8 leg 2: the completion block fires once Core Animation has processed this
        // transaction (the new contents latched) — the truthful "shown" boundary the
        // worker needs to complete the guest's held flush fence. The block only hands the id
        // to the dedicated ack-sender thread via a bounded, non-blocking try_send: the actual
        // socket write (which can block on a booting/wedged worker) must never run on the
        // AppKit main thread. A dropped ack (channel full) is covered by the worker's fallback
        // deadline.
        if let Some((tx, id)) = ack {
            let cb = RcBlock::new(move || {
                let _ = tx.try_send(id);
            });
            CATransaction::setCompletionBlock(Some(&cb));
        }
        layer.setContents(Some(obj));
        CATransaction::commit();
    }
}
