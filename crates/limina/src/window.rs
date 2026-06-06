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
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent, NSEventMask,
    NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::CFRetained;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSTimer};
use objc2_io_surface::{IOSurfaceLookup, IOSurfaceRef};
use objc2_quartz_core::{CALayer, CATransaction};

mod input;
pub use input::InputSinks;

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

    /// Hardware-cursor overlay state (decoupled from the scanout above; the worker publishes
    /// the cursor as its own IOSurface and reports moves separately, so the cursor never
    /// touches the scanout present path).
    cursor_id: Option<u32>,
    cursor_w: u32,
    cursor_h: u32,
    hot_x: u32,
    hot_y: u32,
    cursor_x: u32,
    cursor_y: u32,
    cursor_visible: bool,
    /// Bumped on any cursor change (new image, move, or hide) — the timer re-applies.
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
                    }
                }
                Some("frame") => {
                    // frame <id> — the buffer to show now.
                    if let Some(id) = parts.next().and_then(|s| s.parse::<u32>().ok()) {
                        let mut s = shared.lock().unwrap();
                        s.show_id = Some(id);
                        s.gen += 1;
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
                    // cursormove <x> <y> — reposition the cursor in scanout pixels.
                    let x = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let y = parts.next().and_then(|s| s.parse::<u32>().ok());
                    if let (Some(x), Some(y)) = (x, y) {
                        let mut s = shared.lock().unwrap();
                        s.cursor_x = x;
                        s.cursor_y = y;
                        s.cursor_gen += 1;
                    }
                }
                Some("cursorhide") => {
                    let mut s = shared.lock().unwrap();
                    s.cursor_visible = false;
                    s.cursor_gen += 1;
                }
                _ => {}
            }
        }
        log::info!("window: control channel closed (worker gone)");
        shared.lock().unwrap().worker_exited = true;
    });
}

/// Run the AppKit window on the main thread. The render timer polls `shared`, updates the
/// window contents, and — when the worker exits, the window is closed, or Ctrl-C is hit —
/// kills the worker's process group (`worker_pid`) and exits the process. (We exit from
/// the timer rather than `NSApplication::stop`, which doesn't return without a UI event.)
pub fn run(
    shared: Arc<Mutex<Shared>>,
    mtm: MainThreadMarker,
    worker_pid: i32,
    input: InputSinks,
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
    view.setLayer(Some(&layer));
    view.setWantsLayer(true);
    // Hardware-cursor overlay: a sublayer above the scanout contents. anchorPoint (0,0) makes
    // its position the image's corner (we convert from the guest's top-left origin below).
    let cursor_layer = CALayer::new();
    cursor_layer.setAnchorPoint(NSPoint::new(0.0, 0.0));
    cursor_layer.setHidden(true);
    layer.addSublayer(&cursor_layer);
    window.center();
    // Required for hover (non-dragging) motion to be delivered as MouseMoved events.
    window.setAcceptsMouseMovedEvents(true);
    window.makeKeyAndOrderFront(None);
    app.activate();

    // Per-timer state (main thread only).
    let last_gen = Cell::new(0u64);
    let geom = Cell::new((0u32, 0u32));
    // Diagnostic: render the layer to a PNG after a few frames (no screen-record perm).
    let capture_path = std::env::var("LIMINA_WINDOW_CAPTURE").ok();
    let applies = Cell::new(0u64);
    // Cache looked-up surfaces by id (the worker reuses a small fixed set, its double buffer).
    let cache: RefCell<std::collections::HashMap<u32, CFRetained<IOSurfaceRef>>> =
        RefCell::new(std::collections::HashMap::new());
    // Cursor overlay per-timer state: the last applied cursor gen and the currently-shown
    // cursor surface (id + retained ref, so it stays alive while displayed).
    let last_cursor_gen = Cell::new(0u64);
    let cursor_surf: RefCell<Option<(u32, CFRetained<IOSurfaceRef>)>> = RefCell::new(None);

    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let (exited, gen, show_id, width, height) = {
            let s = shared.lock().unwrap();
            (s.worker_exited, s.gen, s.show_id, s.width, s.height)
        };

        // Quit when the worker is gone, the user closed the window, or Ctrl-C was hit:
        // kill the worker's whole process group and exit now.
        if exited || crate::supervisor::stop_requested() || !window.isVisible() {
            unsafe { libc::kill(-worker_pid, libc::SIGKILL) };
            std::process::exit(0);
        }

        // Cursor overlay first — it has its own gen so a move (or hide) applies even when the
        // scanout hasn't produced a new frame.
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
                s.cursor_x,
                s.cursor_y,
            )
        };
        if cur.0 != last_cursor_gen.get() {
            last_cursor_gen.set(cur.0);
            apply_cursor(&cursor_layer, &mut cursor_surf.borrow_mut(), &cur, height);
        }

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
        // Distinct object each frame (the worker alternates ids) → CA re-reads.
        set_layer_surface(&layer, surface);

        // Diagnostic capture of what CA actually renders for the layer. Periodic (overwrite)
        // so a long-running headless check ends with a recent frame, not just early boot.
        applies.set(applies.get() + 1);
        if applies.get() % 120 == 0 {
            if let Some(path) = &capture_path {
                capture_layer(&layer, width, height, path);
            }
        }
    });

    // ~60 Hz poll. Keep the timer alive for the app's lifetime.
    let _timer =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / 60.0, true, &block) };

    // Capture keyboard + mouse via a local event monitor and forward them to the worker as
    // evdev events. Swallowed key events return null; pass-through events return themselves.
    let input_state = input::InputState::new(input);
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
    std::process::exit(0);
}

/// Render the layer (including its IOSurface contents) into an offscreen bitmap and write
/// it as a PNG — an in-app capture that needs no Screen Recording permission. Diagnostic.
fn capture_layer(layer: &CALayer, width: u32, height: u32, path: &str) {
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGBitmapContextGetBytesPerRow, CGBitmapContextGetData,
        CGColorSpaceCreateDeviceRGB, CGImageAlphaInfo,
    };
    let (w, h) = (width as usize, height as usize);
    unsafe {
        let Some(cs) = CGColorSpaceCreateDeviceRGB() else {
            log::error!("capture: no colorspace");
            return;
        };
        // RGBA8, premultiplied last → R,G,B,A bytes in memory (CG allocates the buffer).
        let info = CGImageAlphaInfo::PremultipliedLast.0;
        let Some(ctx) = CGBitmapContextCreate(std::ptr::null_mut(), w, h, 8, 0, Some(&cs), info)
        else {
            log::error!("capture: no bitmap context");
            return;
        };
        layer.renderInContext(&ctx);

        let data = CGBitmapContextGetData(Some(&ctx)) as *const u8;
        if data.is_null() {
            log::error!("capture: null bitmap data");
            return;
        }
        let bpr = CGBitmapContextGetBytesPerRow(Some(&ctx));
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            std::ptr::copy_nonoverlapping(
                data.add(y * bpr),
                rgba.as_mut_ptr().add(y * w * 4),
                w * 4,
            );
        }
        match std::fs::File::create(path) {
            Ok(f) => {
                let mut enc = png::Encoder::new(std::io::BufWriter::new(f), width, height);
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                match enc
                    .write_header()
                    .and_then(|mut w| w.write_image_data(&rgba))
                {
                    Ok(()) => log::info!("capture: wrote layer render to {path}"),
                    Err(e) => log::error!("capture: png write failed: {e}"),
                }
            }
            Err(e) => log::error!("capture: create {path} failed: {e}"),
        }
    }
}

/// Apply the latest cursor state to the overlay sublayer. `cur` is
/// `(gen, visible, id, w, h, hot_x, hot_y, x, y)`; `slot` holds the currently-shown cursor
/// surface (id + retained ref) so it survives across calls and is only re-looked-up when the
/// image id changes. `scanout_h` flips the guest's top-left origin into the layer's
/// bottom-left coordinate space.
#[allow(clippy::type_complexity)]
fn apply_cursor(
    layer: &CALayer,
    slot: &mut Option<(u32, CFRetained<IOSurfaceRef>)>,
    cur: &(u64, bool, Option<u32>, u32, u32, u32, u32, u32, u32),
    scanout_h: u32,
) {
    let (_gen, visible, id, w, h, hot_x, hot_y, x, y) = *cur;
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    match id {
        Some(id) if visible && w > 0 && h > 0 => {
            // Re-look-up only on an image change; the worker keeps the surface alive until the
            // next shape change, and we retain our own ref in `slot`.
            if slot.as_ref().map(|(sid, _)| *sid != id).unwrap_or(true) {
                match IOSurfaceLookup(id) {
                    Some(s) => {
                        set_layer_surface(layer, &s);
                        layer.setBounds(NSRect::new(
                            NSPoint::new(0.0, 0.0),
                            NSSize::new(w as f64, h as f64),
                        ));
                        *slot = Some((id, s));
                    }
                    None => log::warn!("window: cursor IOSurfaceLookup({id}) failed"),
                }
            }
            // Guest position is top-left origin with the hotspot inside the image; the parent
            // layer is bottom-left origin and the cursor's anchor is its own (0,0) corner.
            let px = x as f64 - hot_x as f64;
            let py = scanout_h as f64 - (y as f64 - hot_y as f64) - h as f64;
            layer.setPosition(NSPoint::new(px, py));
            layer.setHidden(false);
        }
        _ => layer.setHidden(true),
    }
    CATransaction::commit();
}

/// Set an IOSurface as the layer's contents (it's a CF object accepted by `contents`).
///
/// Wrapped in a `CATransaction` with actions disabled: this is a layer-HOSTING layer, so a
/// `contents` change otherwise fires an implicit ~0.25 s fade. At 60 fps the fades overlap
/// and the guest desktop visibly flickers; disabling actions makes each frame swap instant.
fn set_layer_surface(layer: &CALayer, surface: &CFRetained<IOSurfaceRef>) {
    let obj: &AnyObject = unsafe { &*(&**surface as *const IOSurfaceRef as *const AnyObject) };
    unsafe {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.setContents(Some(obj));
        CATransaction::commit();
    }
}
