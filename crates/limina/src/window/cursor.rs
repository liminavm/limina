// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Guest-cursor presentation: host-pointer shape adoption (the macOS pointer wears the
//! guest's cursor image over the view) and capture-mode compositing of the guest cursor into
//! the window's cursor sublayer at its guest-reported position.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_app_kit::{NSCursor, NSImage};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow,
    CGBitmapContextGetData, CGBitmapInfo, CGColorSpace, CGContext, CGImageAlphaInfo,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_io_surface::{
    IOSurfaceGetBaseAddress, IOSurfaceGetBytesPerRow, IOSurfaceGetHeight, IOSurfaceGetWidth,
    IOSurfaceLock, IOSurfaceLockOptions, IOSurfaceLookup, IOSurfaceRef, IOSurfaceUnlock,
};
use objc2_quartz_core::{CALayer, CATransaction};

use super::input;
use super::present::{Shared, SurfaceMap};

/// While the pointer is captured, composite the guest cursor at its guest-reported position into
/// `cursor_layer` (the host NSCursor is hidden in capture mode, so the guest cursor must be drawn
/// or it vanishes). Hidden when not captured, when the guest has hidden its own cursor (pointer-lock
/// games), or before any geometry is known. Runs every timer tick — the position moves continuously,
/// unlike the cursor *shape*. Cheap when idle: it only touches the layer on an actual state change.
pub(crate) fn update_capture_cursor(
    cursor_layer: &CALayer,
    captured: &std::sync::atomic::AtomicBool,
    shared: &Arc<Mutex<Shared>>,
    surface_map: &SurfaceMap,
) {
    use std::sync::atomic::Ordering;
    let hide = || {
        if !cursor_layer.isHidden() {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            cursor_layer.setHidden(true);
            CATransaction::commit();
        }
    };
    if !captured.load(Ordering::Acquire) {
        hide();
        return;
    }
    let (visible, cid, cw, ch, hx, hy, px, py, sw, sh) = {
        let s = shared.lock().unwrap();
        (
            s.cursor_visible,
            s.cursor_id,
            s.cursor_w,
            s.cursor_h,
            s.hot_x,
            s.hot_y,
            s.cursor_pos_x,
            s.cursor_pos_y,
            s.width,
            s.height,
        )
    };
    let geom_ok = visible && sw > 0 && sh > 0 && cw > 0 && ch > 0;
    let Some(cid) = cid.filter(|_| geom_ok) else {
        hide();
        return;
    };
    let Some(surface) = surface_map
        .lock()
        .unwrap()
        .get(cid)
        .or_else(|| IOSurfaceLookup(cid))
    else {
        hide();
        return;
    };
    // Parent (scanout) layer size in points; guest pixels scale into it — 1:1 in the steady state
    // (the window is sized to the guest resolution), stretched during a live resize.
    let bounds = cursor_layer
        .superlayer()
        .map(|l| l.bounds().size)
        .unwrap_or(NSSize::new(sw as f64, sh as f64));
    let scale_x = bounds.width / sw as f64;
    let scale_y = bounds.height / sh as f64;
    let w = cw as f64 * scale_x;
    let h = ch as f64 * scale_y;
    // Image top-left in guest pixels = reported position minus the hotspot.
    let left = (px - hx as i32) as f64 * scale_x;
    let top = (py - hy as i32) as f64 * scale_y;
    // CALayer sublayers use a bottom-left origin; the guest reports a top-down y, so flip.
    let y = bounds.height - top - h;
    let obj: &AnyObject = unsafe { &*(&*surface as *const IOSurfaceRef as *const AnyObject) };
    unsafe {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        cursor_layer.setContents(Some(obj));
        cursor_layer.setFrame(NSRect::new(NSPoint::new(left, y), NSSize::new(w, h)));
        cursor_layer.setHidden(false);
        CATransaction::commit();
    }
}

/// Apply the latest guest cursor state to the host pointer. `cur` is
/// `(gen, visible, id, w, h, hot_x, hot_y)`; `built` caches the IOSurface id of the shape
/// the host pointer already wears, so we only rebuild on an actual shape change (the
/// worker publishes each shape as a fresh IOSurface and keeps it alive until the next).
pub(crate) fn apply_cursor(
    host: &input::HostCursor,
    built: &Cell<Option<u32>>,
    cur: &(u64, bool, Option<u32>, u32, u32, u32, u32),
    surface_map: &SurfaceMap,
) {
    let (_gen, visible, id, w, h, hot_x, hot_y) = *cur;
    match id {
        Some(id) if visible && w > 0 && h > 0 => {
            if built.get() == Some(id) {
                return;
            }
            match build_guest_cursor(id, w, h, hot_x, hot_y, surface_map) {
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
    surface_map: &SurfaceMap,
) -> Option<Retained<NSCursor>> {
    // Mach-delivered (non-global) cursor surface first; legacy/global fallback.
    let surface = surface_map
        .lock()
        .unwrap()
        .get(id)
        .or_else(|| IOSurfaceLookup(id))?;
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
pub(crate) fn blank_cursor() -> Option<Retained<NSCursor>> {
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
