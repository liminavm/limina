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
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::CFRetained;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSTimer};
use objc2_io_surface::{IOSurfaceLookup, IOSurfaceRef};
use objc2_quartz_core::CALayer;

/// State shared between the control-channel reader thread, the worker monitor, and the
/// main-thread render timer. Only `Send` data — never AppKit objects.
#[derive(Default)]
pub struct Shared {
    /// Latest scanout surface id reported by the worker, and its geometry.
    surface_id: Option<u32>,
    width: u32,
    height: u32,
    /// Bumped when `surface_id` changes (a new surface to look up).
    surface_gen: u64,
    /// Bumped on each `frame` (the current surface's pixels changed; re-present).
    frame_gen: u64,
    /// Set when the worker/control channel is gone — the window should close.
    worker_exited: bool,
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
                    let id = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let w = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
                    if let (Some(id), Some(w), Some(h)) = (id, w, h) {
                        let mut s = shared.lock().unwrap();
                        s.surface_id = Some(id);
                        s.width = w;
                        s.height = h;
                        s.surface_gen += 1;
                    }
                }
                Some("frame") => shared.lock().unwrap().frame_gen += 1,
                _ => {}
            }
        }
        shared.lock().unwrap().worker_exited = true;
    });
}

/// Run the AppKit window on the main thread until the worker exits (or forever). The
/// render timer polls `shared` and updates the window contents.
pub fn run(shared: Arc<Mutex<Shared>>, mtm: MainThreadMarker) {
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
    view.setWantsLayer(true);
    let layer = view.layer().expect("layer-backed view");
    window.center();
    window.makeKeyAndOrderFront(None);
    app.activate();

    // Per-timer state (main thread only).
    let last_surface_gen = Cell::new(0u64);
    let last_frame_gen = Cell::new(0u64);
    let current: RefCell<Option<CFRetained<IOSurfaceRef>>> = RefCell::new(None);

    let app_for_timer = app.clone();
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let snapshot = {
            let s = shared.lock().unwrap();
            (
                s.worker_exited,
                s.surface_gen,
                s.frame_gen,
                s.surface_id,
                s.width,
                s.height,
            )
        };
        let (exited, surface_gen, frame_gen, surface_id, width, height) = snapshot;

        if exited {
            app_for_timer.stop(None);
            return;
        }

        if surface_gen != last_surface_gen.get() {
            last_surface_gen.set(surface_gen);
            if let Some(id) = surface_id {
                if let Some(surface) = IOSurfaceLookup(id) {
                    window.setContentSize(NSSize::new(width as f64, height as f64));
                    set_layer_surface(&layer, &surface);
                    *current.borrow_mut() = Some(surface);
                    last_frame_gen.set(frame_gen);
                } else {
                    log::error!("window: IOSurfaceLookup({id}) failed");
                }
            }
        } else if frame_gen != last_frame_gen.get() {
            last_frame_gen.set(frame_gen);
            if let Some(surface) = current.borrow().as_ref() {
                // The worker mutated the same surface in place; nudge CA to re-read.
                unsafe { layer.setContents(None) };
                set_layer_surface(&layer, surface);
            }
        }
    });

    // ~60 Hz poll. Keep the timer alive for the app's lifetime.
    let _timer =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / 60.0, true, &block) };

    app.run();
}

/// Set an IOSurface as the layer's contents (it's a CF object accepted by `contents`).
fn set_layer_surface(layer: &CALayer, surface: &CFRetained<IOSurfaceRef>) {
    let obj: &AnyObject = unsafe { &*(&**surface as *const IOSurfaceRef as *const AnyObject) };
    unsafe { layer.setContents(Some(obj)) };
}
