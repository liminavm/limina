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
    slot: usize,
) {
    use std::sync::atomic::Ordering;
    let hide = || {
        if !cursor_layer.isHidden() {
            if super::capture_tap::edge_trace() {
                eprintln!(
                    "[CURSORLAYER] t={:.1} slot={slot} layer hides",
                    super::capture_tap::trace_ms(),
                );
            }
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
    let (visible, cid, cw, ch, px, py, sw, sh) = {
        let s = shared.lock().unwrap();
        let c = s.slots[slot].cursor;
        (
            c.visible,
            c.id,
            c.w,
            c.h,
            c.pos_x,
            c.pos_y,
            s.slots[slot].width,
            s.slots[slot].height,
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
    // The reported position IS the image's top-left: the guest kernel sends the cursor plane's
    // `crtc_x/crtc_y` (pointer minus hotspot, virtgpu_plane.c) and the hotspot separately.
    // Subtracting the hotspot here again drew the sprite hot_x/hot_y pixels up-left of where
    // the guest has it, so every click landed "slightly right of the visual tip".
    let left = f64::from(px) * scale_x;
    let top = f64::from(py) * scale_y;
    // CALayer sublayers use a bottom-left origin; the guest reports a top-down y, so flip.
    let y = bounds.height - top - h;
    if cursor_layer.isHidden() && super::capture_tap::edge_trace() {
        eprintln!(
            "[CURSORLAYER] t={:.1} slot={slot} layer shows at guest=({px},{py}) frame=({left:.0},{y:.0} {w:.0}x{h:.0})",
            super::capture_tap::trace_ms(),
        );
    }
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

/// The window-to-guest content scale the host pointer shape is built at: guest cursor
/// pixels shrink/grow by the same factor the scanout does through the fit rect, so the
/// pointer stays proportional to the desktop it hovers (a 4K-EDID guest in a small window
/// no longer wears a giant cursor). Quantized so it can key the rebuild cache; degenerate
/// geometry (early boot, zero-size fit) pins to 1:1.
pub(crate) fn cursor_scale_key(fit_w: f64, guest_w: u32) -> u32 {
    let scale = if guest_w > 0 && fit_w > 0.0 {
        fit_w / f64::from(guest_w)
    } else {
        1.0
    };
    let key = (scale * 1024.0).round();
    if key.is_finite() {
        (key as u32).max(1)
    } else {
        1024
    }
}

/// Apply the latest guest cursor state to the host pointer. `cur` is
/// `(gen, visible, id, w, h, hot_x, hot_y)`; `built` caches the IOSurface id and content
/// scale (`cursor_scale_key`) of the shape the host pointer already wears, so we only
/// rebuild on an actual shape or scale change (the worker publishes each shape as a fresh
/// IOSurface and keeps it alive until the next; the scale moves on a window resize).
///
/// Returns whether the state was applied. A shape we could not build is **not** applied: the
/// caller keys its "already applied this generation" cache on the answer, so a transient
/// failure is retried on the next tick instead of leaving the pointer blank until the guest
/// happens to change shape again.
pub(crate) fn apply_cursor(
    host: &input::HostCursor,
    built: &Cell<Option<(u32, u32)>>,
    cur: &(u64, bool, Option<u32>, u32, u32, u32, u32),
    surface_map: &SurfaceMap,
    scale_key: u32,
) -> bool {
    let (_gen, visible, id, w, h, hot_x, hot_y) = *cur;
    match id {
        Some(id) if visible && w > 0 && h > 0 => {
            if built.get() == Some((id, scale_key)) {
                return true;
            }
            match build_guest_cursor(id, w, h, hot_x, hot_y, surface_map, scale_key) {
                Some(c) => {
                    host.update(c, false);
                    built.set(Some((id, scale_key)));
                    true
                }
                None => {
                    log::warn!("window: building guest cursor from IOSurface {id} failed");
                    false
                }
            }
        }
        _ => {
            // The guest hid its cursor: honor that with a blank (fully transparent) host
            // cursor over the view, falling back to the arrow if we can't build one. A
            // hide before any shape was ever built keeps the default arrow (early boot).
            if built.get().is_some() {
                built.set(None);
                host.update(blank_cursor().unwrap_or_else(NSCursor::arrowCursor), true);
            }
            true
        }
    }
}

/// Which slot's cursor shape the host pointer should wear.
///
/// The guest has ONE cursor and enables its plane on ONE CRTC, and **that need not be the slot
/// the pointer is over**: while the absolute device's per-display shares are still being learned
/// (`window/absfit.rs`), or whenever the two disagree at all, the guest's cursor is on a
/// different display from the host pointer. Reading only the pointer's slot meant the
/// `cursorhide` for the display the guest's cursor *left* dressed the host pointer in the blank
/// while another slot was publishing a perfectly good shape — an invisible pointer, and no way
/// to find it again except by toggling the grab (dogfood 2026-08-22, right after enabling "Use
/// Other Screens When Fullscreen"). The blank is for the guest hiding its cursor *everywhere*,
/// which is the only thing that means "no pointer".
///
/// The pointer's own slot still wins when it has one: a sprite straddling a seam lights two
/// planes, and the one under the pointer is the one meant.
pub(crate) fn shape_slot(pointer_slot: usize, visible: &[bool]) -> usize {
    if visible.get(pointer_slot).copied().unwrap_or(false) {
        return pointer_slot;
    }
    visible.iter().position(|v| *v).unwrap_or(pointer_slot)
}

/// Build an `NSCursor` wearing the guest's cursor image: look up the worker-published
/// IOSurface (BGRA, premultiplied alpha), copy it through a `CGBitmapContext` into a
/// `CGImage`, and wrap it with the guest's hotspot (top-left origin, as NSCursor expects).
/// The bitmap keeps the guest's full pixel resolution; `scale_key` (`cursor_scale_key`
/// units) only shrinks the NSImage *point* size — and the hotspot with it — to the
/// window's content scale, so downscaled cursors stay crisp on Retina backings.
fn build_guest_cursor(
    id: u32,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
    surface_map: &SurfaceMap,
    scale_key: u32,
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
    let s = f64::from(scale_key) / 1024.0;
    nscursor_from_context(
        &ctx,
        f64::from(w) * s,
        f64::from(h) * s,
        f64::from(hot_x) * s,
        f64::from(hot_y) * s,
    )
}

/// A fully transparent 1×1 cursor — what the host pointer wears while the guest hides its
/// own (so "no pointer" is honored instead of showing a stale arrow over the view), and what
/// it wears throughout capture.
///
/// **One instance, memoised.** It is worn on every re-assert, so building the bitmap each time
/// would be waste; more importantly the wear is only *advisory* — AppKit resets the cursor from
/// its own cursor rects behind us — so the way to tell whether ours is still on is to compare
/// `NSCursor::currentCursor()` against it by identity ([`input::HostCursor::verify_captured`]),
/// and that needs the same object every time.
pub(crate) fn blank_cursor() -> Option<Retained<NSCursor>> {
    thread_local! {
        static BLANK: Option<Retained<NSCursor>> = build_blank_cursor();
    }
    BLANK.with(|b| b.clone())
}

fn build_blank_cursor() -> Option<Retained<NSCursor>> {
    let ctx = bgra_bitmap_context(1, 1)?;
    unsafe {
        let dst = CGBitmapContextGetData(Some(&ctx)) as *mut u32;
        if dst.is_null() {
            return None;
        }
        dst.write(0);
    }
    nscursor_from_context(&ctx, 1.0, 1.0, 0.0, 0.0)
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

/// Wrap the bitmap context's image as an `NSCursor`. `w`/`h`/`hot_*` are in *points* —
/// they may be smaller than the bitmap's pixel size when the window presents the guest
/// scaled down (NSImage draws the full pixel data into the point size).
fn nscursor_from_context(
    ctx: &CGContext,
    w: f64,
    h: f64,
    hot_x: f64,
    hot_y: f64,
) -> Option<Retained<NSCursor>> {
    let img = CGBitmapContextCreateImage(Some(ctx))?;
    let size = NSSize::new(w, h);
    let nsimage = NSImage::initWithCGImage_size(NSImage::alloc(), &img, size);
    Some(NSCursor::initWithImage_hotSpot(
        NSCursor::alloc(),
        &nsimage,
        NSPoint::new(hot_x, hot_y),
    ))
}

#[cfg(test)]
mod shape_slot_tests {
    use super::shape_slot;

    #[test]
    fn the_pointers_own_slot_wins_when_it_shows_a_cursor() {
        assert_eq!(shape_slot(0, &[true, true]), 0);
        assert_eq!(shape_slot(1, &[true, true]), 1);
    }

    #[test]
    fn a_cursor_on_another_slot_is_still_the_guests_cursor() {
        // The pointer is over the BenQ, the guest's cursor is on the internal: wear the shape
        // it is actually showing, do not blank.
        assert_eq!(shape_slot(0, &[false, true]), 1);
    }

    #[test]
    fn no_cursor_anywhere_falls_back_to_the_pointers_slot() {
        // Which then carries `visible = false`, and the blank is worn — the guest really has
        // hidden its pointer.
        assert_eq!(shape_slot(1, &[false, false]), 1);
        assert_eq!(shape_slot(7, &[]), 7);
    }
}

#[cfg(test)]
mod tests {
    use super::cursor_scale_key;

    #[test]
    fn scale_key_is_1024_at_one_to_one() {
        assert_eq!(cursor_scale_key(1920.0, 1920), 1024);
    }

    #[test]
    fn scale_key_tracks_the_fit_ratio() {
        // Guest at 3456 px wide fitted into a 1728 pt window: half size.
        assert_eq!(cursor_scale_key(1728.0, 3456), 512);
        // Upscale (small fixed guest in a big window) grows past 1:1.
        assert_eq!(cursor_scale_key(1600.0, 800), 2048);
    }

    #[test]
    fn degenerate_geometry_pins_to_one_to_one() {
        assert_eq!(cursor_scale_key(1728.0, 0), 1024);
        assert_eq!(cursor_scale_key(0.0, 1920), 1024);
        assert_eq!(cursor_scale_key(-5.0, 1920), 1024);
    }

    #[test]
    fn tiny_scales_never_quantize_to_zero() {
        assert_eq!(cursor_scale_key(1.0, 100_000), 1);
    }
}
