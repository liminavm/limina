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

use std::sync::mpsc::SyncSender;

use super::input;
use super::present::{self, AckMsg, Shared, SurfaceMap};

/// Resolve a cursor image id to its surface, or arrange for its recovery and return `None`.
///
/// **The cursor's counterpart to [`super::guestwindow::resolve_presented`]**, and it exists for
/// the same reason that one does — with a longer fuse. A scanout that cannot be resolved skips
/// one frame and the guest presents again immediately; a cursor that cannot be resolved leaves
/// **no pointer at all**, and the worker republishes a cursor only when the guest changes shape.
/// A guest sitting on one arrow therefore stays pointerless for as long as it keeps that arrow —
/// minutes of a dogfood session on 2026-08-26, through grab releases, workspace switches and
/// clicks, healed only by hovering something that happened to change the shape.
///
/// So: say WHY (a release, an eviction, or an id we never held — three different faults with
/// three different fixes), and ask the worker for the surface back. The ask is throttled per id
/// inside the store, and rides the ack thread, never a socket write on the main thread.
pub(crate) fn resolve_cursor(
    surface_map: &SurfaceMap,
    ack_tx: &SyncSender<AckMsg>,
    id: u32,
) -> Option<CFRetained<IOSurfaceRef>> {
    // Mach-delivered (non-global) cursor surface first; legacy/global fallback.
    let resolved = surface_map
        .lock()
        .unwrap()
        .get(id)
        .or_else(|| IOSurfaceLookup(id));
    if resolved.is_some() {
        return resolved;
    }
    let (why, ask) = {
        let mut map = surface_map.lock().unwrap();
        (map.why_gone(id), map.request_resurface(id))
    };
    if !ask {
        // A request for this id is already in flight — one line per tick would be noise.
        return None;
    }
    let _ = ack_tx.try_send(AckMsg::Resurface(id));
    match why {
        Some(present::GoneReason::Released) => log::warn!(
            "guest cursor: surface {id} unresolved — the worker RELEASED it while it is the \
             shape the guest is showing. Asking for it back."
        ),
        Some(present::GoneReason::Evicted) => log::warn!(
            "guest cursor: surface {id} unresolved — the store EVICTED it at its cap while it \
             is the shape the guest is showing. Asking for it back."
        ),
        None => log::warn!(
            "guest cursor: surface {id} unresolved — we never held it, and no release or \
             eviction was recorded for it. Asking for it."
        ),
    }
    None
}

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
    ack_tx: &SyncSender<AckMsg>,
    slot: usize,
) -> LayerVerdict {
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
        return LayerVerdict::NotCaptured;
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
    let geom_ok = sw > 0 && sh > 0 && cw > 0 && ch > 0;
    if !visible || !geom_ok || cid.is_none() {
        hide();
        return match (visible, geom_ok) {
            (false, _) => LayerVerdict::GuestHidThisSlot,
            (_, false) => LayerVerdict::NoGeometry,
            _ => LayerVerdict::NoImage,
        };
    }
    let cid = cid.expect("checked just above");
    let Some(surface) = resolve_cursor(surface_map, ack_tx, cid) else {
        hide();
        return LayerVerdict::NoSurface;
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
    LayerVerdict::Drawing
}

/// What one window's capture-cursor layer did this tick, and why — the input to
/// [`undrawn_fault`]. Every variant but `Drawing` means the layer is hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayerVerdict {
    Drawing,
    /// Not captured: the host pointer wears the shape instead, so a hidden layer is correct.
    NotCaptured,
    /// The guest disabled its cursor plane on THIS slot — it is showing the cursor elsewhere,
    /// or nowhere.
    GuestHidThisSlot,
    /// No scanout or no cursor size yet.
    NoGeometry,
    /// The guest never uploaded a cursor image for this slot.
    NoImage,
    /// An image id we cannot resolve to an IOSurface.
    NoSurface,
}

/// The state that should be impossible: the pointer is captured, the guest plainly has a
/// cursor, and the display the user is driving draws nothing.
///
/// Silent in every ordinary case — including the one that looks alarming and is not, a guest
/// that has hidden its cursor everywhere (mouselook, a pointer-locked game). It fires on the
/// shape of the 2026-08-24 dogfood report: the pointer moved and hot corners fired, but nothing
/// was drawn, while the *uncaptured* path kept wearing a shape borrowed from another slot
/// ([`shape_slot`]) and so looked perfectly healthy. Returns the reason, for a caller that logs
/// it once per episode with the full per-slot state; `None` means nothing to say.
pub(crate) fn undrawn_fault(
    capture_slot: usize,
    verdict: LayerVerdict,
    visible: &[bool],
) -> Option<String> {
    if matches!(verdict, LayerVerdict::Drawing | LayerVerdict::NotCaptured) {
        return None;
    }
    // The guest showing no cursor at all is the guest's business, not a fault.
    let elsewhere: Vec<usize> = visible
        .iter()
        .enumerate()
        .filter(|&(s, v)| *v && s != capture_slot)
        .map(|(s, _)| s)
        .collect();
    if !visible.iter().any(|v| *v) {
        return None;
    }
    Some(match verdict {
        LayerVerdict::GuestHidThisSlot => format!(
            "the guest has its cursor plane on {elsewhere:?}, not on the captured slot {capture_slot}"
        ),
        v => format!("slot {capture_slot} says it has a cursor and we drew none ({v:?})"),
    })
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
    ack_tx: &SyncSender<AckMsg>,
    scale_key: u32,
) -> bool {
    let (_gen, visible, id, w, h, hot_x, hot_y) = *cur;
    match id {
        Some(id) if visible && w > 0 && h > 0 => {
            if built.get() == Some((id, scale_key)) {
                return true;
            }
            let geom = CursorGeom { w, h, hot_x, hot_y };
            match build_guest_cursor(id, geom, surface_map, ack_tx, scale_key) {
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

/// One guest cursor image's pixel geometry: size and hotspot, in guest pixels.
#[derive(Clone, Copy)]
struct CursorGeom {
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
}

/// Build an `NSCursor` wearing the guest's cursor image: look up the worker-published
/// IOSurface (BGRA, premultiplied alpha), copy it through a `CGBitmapContext` into a
/// `CGImage`, and wrap it with the guest's hotspot (top-left origin, as NSCursor expects).
/// The bitmap keeps the guest's full pixel resolution; `scale_key` (`cursor_scale_key`
/// units) only shrinks the NSImage *point* size — and the hotspot with it — to the
/// window's content scale, so downscaled cursors stay crisp on Retina backings.
fn build_guest_cursor(
    id: u32,
    geom: CursorGeom,
    surface_map: &SurfaceMap,
    ack_tx: &SyncSender<AckMsg>,
    scale_key: u32,
) -> Option<Retained<NSCursor>> {
    let CursorGeom { w, h, hot_x, hot_y } = geom;
    let surface = resolve_cursor(surface_map, ack_tx, id)?;
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
mod resolve_tests {
    use std::sync::mpsc::sync_channel;
    use std::sync::{Arc, Mutex};

    use super::super::present::{SurfaceMap, SurfaceStore};
    use super::{resolve_cursor, AckMsg};

    /// The missing-pointer fault (dogfood 2026-08-26): the guest is showing a shape whose
    /// surface we cannot resolve. Giving up silently is not one skipped frame — the worker
    /// republishes a cursor only on a shape change, so the pointer stays gone. Ask for it back,
    /// exactly once per throttle window.
    #[test]
    fn an_unresolved_cursor_asks_the_worker_to_republish() {
        // No real IOSurface plausibly wears a near-MAX global id, so the lookup fallback
        // stays deterministic in a test.
        let id = u32::MAX - 23;
        let map: SurfaceMap = Arc::new(Mutex::new(SurfaceStore::default()));
        let (tx, rx) = sync_channel::<AckMsg>(4);

        assert!(
            resolve_cursor(&map, &tx, id).is_none(),
            "an id nobody holds must not resolve"
        );
        match rx.try_recv() {
            Ok(AckMsg::Resurface(asked)) => assert_eq!(asked, id),
            Ok(AckMsg::Shown(..)) => panic!("a cursor resolve must never ack a frame as shown"),
            Err(_) => panic!("an unresolved cursor must ask the worker to publish it again"),
        }

        // Both cursor paths run every tick while the shape stands — a 60 Hz stream of asks and
        // warnings would bury the one line that matters.
        assert!(resolve_cursor(&map, &tx, id).is_none());
        assert!(
            rx.try_recv().is_err(),
            "a second miss inside the throttle window must not re-ask"
        );
    }
}

#[cfg(test)]
mod shape_slot_tests {
    use super::{shape_slot, undrawn_fault, LayerVerdict};

    /// The steady state says nothing at all: the layer is drawing.
    #[test]
    fn a_drawing_layer_is_not_a_fault() {
        assert_eq!(
            undrawn_fault(0, LayerVerdict::Drawing, &[true, false]),
            None
        );
    }

    /// Nor does an uncaptured pointer: the host cursor wears the shape, and the layer is
    /// hidden on purpose.
    #[test]
    fn an_uncaptured_pointer_is_not_a_fault() {
        assert_eq!(
            undrawn_fault(0, LayerVerdict::NotCaptured, &[true, false]),
            None
        );
    }

    /// A guest that has hidden its cursor EVERYWHERE has hidden its cursor: mouselook, a
    /// pointer-locked game. Drawing nothing is the correct answer, and this is the case that
    /// would otherwise make the check noisy enough to be ignored.
    #[test]
    fn a_guest_that_hid_its_cursor_everywhere_is_not_a_fault() {
        assert_eq!(
            undrawn_fault(0, LayerVerdict::GuestHidThisSlot, &[false, false]),
            None
        );
    }

    /// The 2026-08-24 shape: the plane is on another slot, so the captured display draws
    /// nothing while the worn shape borrows the good slot and looks healthy. The message must
    /// name where the cursor actually went.
    #[test]
    fn the_plane_being_on_another_slot_is_the_fault_we_are_hunting() {
        let why = undrawn_fault(0, LayerVerdict::GuestHidThisSlot, &[false, true])
            .expect("the guest has a cursor, just not where we are captured");
        assert!(why.contains('1'), "must name the slot that has it: {why}");
        assert!(why.contains('0'), "and the slot we are captured on: {why}");
    }

    /// The other half: this slot claims a cursor and we still drew nothing. A different bug
    /// with a different fix, so it must not be reported as the one above.
    #[test]
    fn a_slot_that_claims_a_cursor_and_draws_none_reports_its_own_gate() {
        for v in [
            LayerVerdict::NoImage,
            LayerVerdict::NoGeometry,
            LayerVerdict::NoSurface,
        ] {
            let why = undrawn_fault(0, v, &[true, false]).expect("this slot says it has one");
            assert!(why.contains(&format!("{v:?}")), "must name the gate: {why}");
        }
    }

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
