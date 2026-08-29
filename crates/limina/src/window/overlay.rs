// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The M9.4 felt-resume overlay: what the user sees while a suspend or a restore is in
//! flight, instead of a frozen or blank window.
//!
//! Four flavors, one widget:
//! - **Suspend** (`Overlay::suspend`): a dim scrim + spinner over the live (about-to-freeze)
//!   frame, shown while the suspend bracket + snapshot save run (the close path re-shows the
//!   window for the save's duration so this is actually visible).
//! - **Restore** (`Overlay::restore`): the previous session's last-presented frame (the
//!   splash PNG saved at suspend), aspect-fit over black, under the same scrim + spinner —
//!   shown from window creation until the fresh worker presents its first real frame.
//! - **Parked** (`Overlay::parked`): the Parallels-style suspended window (task #18) — the
//!   scrim over the (still-presented) final frame with a centered play glyph. No spinner:
//!   nothing is in flight; the window waits for a click to resume.
//! - **Resuming** (`Overlay::resuming`): scrim + spinner + "Resuming…" after the play click,
//!   over the splash — the same last-frame PNG the Restore flavor uses, and for the same
//!   reason. The dead worker's IOSurface is on the layer when the click lands, but it does not
//!   survive the swap to the fresh worker: measured 2026-08-29 on the dogfood Mac, the window
//!   goes BLACK from the swap until the restore's first present. The splash covers that gap
//!   with the picture the session had.
//!
//! Everything is Core Animation: the scanout view is layer-HOSTING with redraw policy
//! `Never`, so AppKit-drawn controls (NSProgressIndicator) never composite inside it — the
//! spinner is a pre-rendered arc image on a sublayer, rotated by an infinite
//! `CABasicAnimation` (CA animates it without any AppKit drawing). The render timer re-fits
//! the overlay every tick (`fit`) and tears it down via `remove`.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_app_kit::{NSImage, NSView};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow,
    CGBitmapContextGetData, CGBitmapInfo, CGColor, CGColorSpace, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CAAutoresizingMask, CABasicAnimation, CALayer, CAMediaTiming, CATextLayer, CATransaction,
};
use std::path::Path;

/// Centerpiece diameter for a given content size (user-picked): 10% of the
/// smaller dimension, clamped so a tiny window still shows a legible glyph and a big
/// fullscreen panel doesn't get a billboard. The spinner arc and the parked play disc
/// share it — one visual family — and `fit` re-derives it live across resizes.
fn glyph_diameter(bounds: NSRect) -> f64 {
    (bounds.size.width.min(bounds.size.height) * 0.10).clamp(64.0, 128.0)
}

/// Caption font size, scaled with the glyph (~d/5, clamped to stay readable-not-shouty).
fn label_font_size(d: f64) -> f64 {
    (d / 5.0).clamp(15.0, 26.0)
}

/// Vertical gap between the glyph center and the caption center. 1.1×d reproduces the
/// original 44pt at the old 40pt spinner exactly.
fn label_drop(d: f64) -> f64 {
    d * 1.1
}

/// Which overlay this is — decides the centerpiece (spinner vs play glyph), the caption,
/// and whether a splash underlay is expected.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Suspend,
    Restore,
    Parked,
    Resuming,
}

pub(crate) struct Overlay {
    root: Retained<CALayer>,
    /// Bounds-filling sublayers (splash and/or scrim) — re-fit to the view every tick.
    fill: Vec<Retained<CALayer>>,
    /// The centerpiece: the rotating arc, or the parked flavor's play glyph. Positioned
    /// (not framed): a frame is undefined under the rotation transform, and `fit` must
    /// never stretch it.
    spinner: Retained<CALayer>,
    /// The "Suspending…" / "Resuming…" caption under the arc (empty for parked — the play
    /// glyph is its own caption).
    label: Retained<CATextLayer>,
    /// True for the restore/resuming flavors: comes down on the first presented frame. The
    /// suspend flavor instead comes down when the suspend is abandoned (bracket timeout);
    /// parked comes down on the play click (replaced by a resuming overlay).
    pub(crate) until_first_frame: bool,
    /// Which flavor this is — fit() needs it to re-render the right glyph on a size change.
    flavor: Flavor,
    /// The glyph diameter currently rendered, so fit() only re-renders when the window's
    /// size actually moves it.
    diameter: std::cell::Cell<f64>,
}

impl Overlay {
    /// Dim scrim + spinner over the live frame (a suspend is in flight).
    pub(crate) fn suspend(host_layer: &CALayer, content: &NSView) -> Self {
        Self::install(host_layer, content, None, Flavor::Suspend)
    }

    /// Splash (if readable) + scrim + spinner while a restore boots to its first frame.
    pub(crate) fn restore(host_layer: &CALayer, content: &NSView, splash: &Path) -> Self {
        Self::install(host_layer, content, Some(splash), Flavor::Restore)
    }

    /// Scrim + centered play glyph over the final presented frame: the suspended-but-open
    /// window (task #18). Click anywhere in the content to resume.
    pub(crate) fn parked(host_layer: &CALayer, content: &NSView) -> Self {
        Self::install(host_layer, content, None, Flavor::Parked)
    }

    /// Scrim + spinner + "Resuming…" over the final frame, after the play click, until the
    /// respawned worker presents its first frame.
    pub(crate) fn resuming(host_layer: &CALayer, content: &NSView, splash: Option<&Path>) -> Self {
        Self::install(host_layer, content, splash, Flavor::Resuming)
    }

    fn install(
        host_layer: &CALayer,
        content: &NSView,
        splash: Option<&Path>,
        flavor: Flavor,
    ) -> Self {
        let bounds = content.bounds();
        let root = CALayer::new();
        root.setFrame(bounds);
        // CALayer autoresizing (macOS-only CA) adjusts sublayer geometry inside the same
        // transaction as the superlayer's bounds change, keeping the overlay roughly in
        // step with a live resize BETWEEN render-timer ticks (the timer's own geometry
        // pass commits on its own schedule and lags the window by up to a frame). It
        // reduces but does not eliminate the drag-time ghosting — see fit() for the
        // accepted residual and the revisit direction.
        root.setAutoresizingMask(
            CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable,
        );
        let mut fill = Vec::new();
        if let Some(path) = splash {
            // Opaque black base: the restore window has nothing real underneath yet, and
            // the splash aspect-fits inside (its letterbox bars must read black).
            root.setBackgroundColor(Some(&CGColor::new_generic_gray(0.0, 1.0)));
            let img = NSImage::initWithContentsOfFile(
                NSImage::alloc(),
                &NSString::from_str(&path.to_string_lossy()),
            );
            match img {
                Some(img) => {
                    let obj: &AnyObject = &img;
                    let splash_layer = CALayer::new();
                    splash_layer.setFrame(bounds);
                    splash_layer.setAutoresizingMask(
                        CAAutoresizingMask::LayerWidthSizable
                            | CAAutoresizingMask::LayerHeightSizable,
                    );
                    unsafe {
                        splash_layer.setContents(Some(obj));
                        splash_layer.setContentsGravity(&NSString::from_str("resizeAspect"));
                    }
                    root.addSublayer(&splash_layer);
                    fill.push(splash_layer);
                }
                None => log::warn!("restore splash unreadable at {}", path.display()),
            }
            let scrim = CALayer::new();
            scrim.setFrame(bounds);
            scrim.setAutoresizingMask(
                CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable,
            );
            scrim.setBackgroundColor(Some(&CGColor::new_generic_gray(0.0, 0.35)));
            root.addSublayer(&scrim);
            fill.push(scrim);
        } else {
            // Suspend flavor: the root itself is the scrim over the live frame.
            root.setBackgroundColor(Some(&CGColor::new_generic_gray(0.0, 0.35)));
        }

        // The centerpiece: a rotating arc while something is in flight, the static play
        // glyph while parked. Sized to the window (glyph_diameter); fit() re-derives on
        // resize.
        let d = glyph_diameter(bounds);
        let spinner = CALayer::new();
        spinner.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(d, d)));
        spinner.setPosition(NSPoint::new(
            bounds.size.width / 2.0,
            bounds.size.height / 2.0,
        ));
        // All margins flexible = stays centered through the drag at its current size; the
        // settle snap re-derives the size (an ellipse-free alternative to scaling live).
        spinner.setAutoresizingMask(CENTERED_MASK);
        set_glyph_contents(&spinner, flavor, d);
        // Crisp on Retina: the image is rendered at 2× and drawn into the point-sized layer.
        spinner.setContentsScale(2.0);
        if flavor != Flavor::Parked {
            unsafe {
                let anim = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str(
                    "transform.rotation.z",
                )));
                anim.setFromValue(Some(&NSNumber::new_f64(0.0)));
                // Negative = clockwise; one turn per second, forever.
                anim.setToValue(Some(&NSNumber::new_f64(-std::f64::consts::TAU)));
                anim.setDuration(1.0);
                anim.setRepeatCount(f32::INFINITY);
                spinner.addAnimation_forKey(&anim, Some(&NSString::from_str("spin")));
            }
        }
        root.addSublayer(&spinner);

        // The caption ("Suspending…" / "Resuming…"), centered under the arc. The parked
        // flavor gets none — the play glyph is the whole message.
        let label = unsafe {
            let label = CATextLayer::new();
            let text = match flavor {
                Flavor::Suspend => "Suspending…",
                Flavor::Restore | Flavor::Resuming => "Resuming…",
                Flavor::Parked => "",
            };
            let obj: &AnyObject = &NSString::from_str(text);
            label.setString(Some(obj));
            label.setForegroundColor(Some(&CGColor::new_generic_gray(1.0, 0.85)));
            label.setAlignmentMode(&NSString::from_str("center"));
            label.setContentsScale(2.0);
            label.setAutoresizingMask(CENTERED_MASK);
            fit_label(&label, bounds, d, true);
            label
        };
        root.addSublayer(&label);
        host_layer.addSublayer(&root);

        Self {
            root,
            fill,
            spinner,
            label,
            until_first_frame: matches!(flavor, Flavor::Restore | Flavor::Resuming),
            flavor,
            diameter: std::cell::Cell::new(d),
        }
    }

    /// Re-fit to the content view, from the render timer. Geometry (frames, positions,
    /// the glyph's bounds — Core Animation stretches the last-rendered image into them)
    /// tracks every tick, so the glyph scales live through a drag; the EXPENSIVE work —
    /// re-rendering the glyph bitmap and re-rasterizing the caption's font — waits for the
    /// resize to settle (`inLiveResize` off), since doing those per tick read as flicker.
    ///
    /// KNOWN, accepted: during a continuous drag the overlay pieces can
    /// transiently ghost/lag the window by a frame — the timer commits in a different CA
    /// transaction than AppKit's live resize. The autoresizing masks set at install keep
    /// the layers roughly in step between ticks, but did not eliminate it; a proper fix
    /// (geometry from `viewDidLayout`/`layoutSublayers` instead of a timer) is a possible
    /// later revisit.
    pub(crate) fn fit(&self, content: &NSView) {
        let bounds = content.bounds();
        let d = glyph_diameter(bounds);
        let settled = !content.inLiveResize();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.root.setFrame(bounds);
        for layer in &self.fill {
            layer.setFrame(bounds);
        }
        self.spinner
            .setBounds(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(d, d)));
        self.spinner.setPosition(NSPoint::new(
            bounds.size.width / 2.0,
            bounds.size.height / 2.0,
        ));
        if settled && (d - self.diameter.get()).abs() > 0.5 {
            self.diameter.set(d);
            set_glyph_contents(&self.spinner, self.flavor, d);
        }
        fit_label(&self.label, bounds, d, settled);
        CATransaction::commit();
    }

    /// Tear the overlay down (first frame presented, or the suspend was abandoned).
    pub(crate) fn remove(self) {
        self.root.removeFromSuperlayer();
    }
}

/// Render the flavor's glyph at diameter `d` (points) into the layer's contents.
fn set_glyph_contents(layer: &CALayer, flavor: Flavor, d: f64) {
    let img = if flavor == Flavor::Parked {
        play_image(d)
    } else {
        spinner_image(d)
    };
    if let Some(img) = img {
        let obj: &AnyObject = unsafe { &*(&*img as *const CGImage as *const AnyObject) };
        unsafe { layer.setContents(Some(obj)) };
    }
}

/// Autoresizing mask that keeps a fixed-size layer proportionally positioned (all four
/// margins flexible) — for the centered glyph and caption, "stay centered through the
/// drag without scaling".
const CENTERED_MASK: CAAutoresizingMask = CAAutoresizingMask(
    CAAutoresizingMask::LayerMinXMargin.0
        | CAAutoresizingMask::LayerMaxXMargin.0
        | CAAutoresizingMask::LayerMinYMargin.0
        | CAAutoresizingMask::LayerMaxYMargin.0,
);

/// Size and place the caption for glyph diameter `d`: font ~d/5, dropped 1.1×d under the
/// center, box wide enough at any clamped font size. Position tracks every tick; the
/// font-size/box change (a CATextLayer re-rasterize) waits for `settled` — the same
/// anti-flicker split as the glyph re-render.
fn fit_label(label: &CATextLayer, bounds: NSRect, d: f64, settled: bool) {
    if settled {
        let font = label_font_size(d);
        label.setFontSize(font);
        label.setBounds(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new((d * 3.0).max(240.0), font + 6.0),
        ));
    }
    label.setPosition(NSPoint::new(
        bounds.size.width / 2.0,
        bounds.size.height / 2.0 - label_drop(d),
    ));
}

/// Pre-render the spinner arc: a white "comet" ring whose alpha ramps around the circle
/// (opaque head, fading tail), 2× pixels for Retina. BGRA premultiplied little-endian, the
/// same layout the cursor bitmaps use. The ring rides at 0.70–0.90 of the radius — the
/// proportions of the original fixed-size arc — so it thickens with the diameter instead
/// of thinning into a wire.
fn spinner_image(d: f64) -> Option<CFRetained<CGImage>> {
    let px = (d * 2.0) as usize;
    let space = CGColorSpace::new_device_rgb()?;
    let info = CGBitmapInfo::ByteOrder32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    // SAFETY: null data = CG allocates the backing store; 0 bytes-per-row = CG chooses.
    let ctx =
        unsafe { CGBitmapContextCreate(std::ptr::null_mut(), px, px, 8, 0, Some(&space), info) }?;
    unsafe {
        let data = CGBitmapContextGetData(Some(&ctx)) as *mut u32;
        if data.is_null() {
            return None;
        }
        // CG chose the row stride (bytesPerRow=0 above) and ALIGNS it — indexing rows by
        // `px` shears every size whose px*4 isn't stride-aligned (the fixed 80/192px
        // renders were accidentally aligned; proportional sizes exposed it as a glitched
        // glyph at most window sizes).
        let row = CGBitmapContextGetBytesPerRow(Some(&ctx)) / 4;
        let c = px as f64 / 2.0;
        let (r_in, r_out) = (c * 0.70, c * 0.90);
        for y in 0..px {
            for x in 0..px {
                let (dx, dy) = (x as f64 + 0.5 - c, y as f64 + 0.5 - c);
                let r = (dx * dx + dy * dy).sqrt();
                // Soft radial edges (1px feather).
                let radial = ((r - r_in).min(r_out - r).clamp(0.0, 1.0) * 255.0) as u32;
                if radial == 0 {
                    continue;
                }
                // Alpha ramps with the angle: 0 at the tail growing to full at the head.
                let angle = dy.atan2(dx); // -π..π
                let t = (angle + std::f64::consts::PI) / std::f64::consts::TAU; // 0..1
                let a = (radial as f64 * t) as u32;
                // White premultiplied: every channel = alpha.
                data.add(y * row + x).write(a << 24 | a << 16 | a << 8 | a);
            }
        }
        CGBitmapContextCreateImage(Some(&ctx))
    }
}

/// Pre-render the parked play glyph: a translucent white disc with a solid white triangle,
/// 2× pixels for Retina — same BGRA premultiplied layout as the spinner. Drawn per-pixel
/// with soft edges (signed-distance alpha), matching the spinner's hand-rendered style.
fn play_image(d: f64) -> Option<CFRetained<CGImage>> {
    let px = (d * 2.0) as usize;
    let space = CGColorSpace::new_device_rgb()?;
    let info = CGBitmapInfo::ByteOrder32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    // SAFETY: null data = CG allocates the backing store; 0 bytes-per-row = CG chooses.
    let ctx =
        unsafe { CGBitmapContextCreate(std::ptr::null_mut(), px, px, 8, 0, Some(&space), info) }?;
    unsafe {
        let data = CGBitmapContextGetData(Some(&ctx)) as *mut u32;
        if data.is_null() {
            return None;
        }
        // CG chose the row stride (bytesPerRow=0 above) and ALIGNS it — indexing rows by
        // `px` shears every size whose px*4 isn't stride-aligned (the fixed 80/192px
        // renders were accidentally aligned; proportional sizes exposed it as a glitched
        // glyph at most window sizes).
        let row = CGBitmapContextGetBytesPerRow(Some(&ctx)) / 4;
        let c = px as f64 / 2.0;
        let disc_r = c - 2.0;
        // The triangle: an equilateral-ish wedge inscribed in the disc, nudged right so its
        // centroid (not its bounding box) sits at the disc center — the optical center.
        let tri_r = disc_r * 0.42;
        let (tx0, tx1) = (c - tri_r * 0.8, c + tri_r * 1.2);
        let half_h_at = |x: f64| -> f64 {
            // Full height at the left edge tapering to 0 at the tip.
            if x <= tx0 || x >= tx1 {
                0.0
            } else {
                tri_r * (tx1 - x) / (tx1 - tx0)
            }
        };
        for y in 0..px {
            for x in 0..px {
                let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
                let (dx, dy) = (fx - c, fy - c);
                let r = (dx * dx + dy * dy).sqrt();
                // Disc: translucent white fill with a soft edge (1.5px feather).
                let disc = ((disc_r - r) / 1.5).clamp(0.0, 1.0);
                // Triangle: solid white where |dy| is inside the wedge at this x.
                let tri = ((half_h_at(fx) - dy.abs()) / 1.5).clamp(0.0, 1.0);
                let a = (disc * 0.35).max(tri * disc);
                let a = (a * 255.0) as u32;
                if a == 0 {
                    continue;
                }
                // White premultiplied: every channel = alpha.
                data.add(y * row + x).write(a << 24 | a << 16 | a << 8 | a);
            }
        }
        CGBitmapContextCreateImage(Some(&ctx))
    }
}
