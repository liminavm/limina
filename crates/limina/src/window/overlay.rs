// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The M9.4 felt-resume overlay: what the user sees while a suspend or a restore is in
//! flight, instead of a frozen or blank window.
//!
//! Two flavors, one widget:
//! - **Suspend** (`Overlay::suspend`): a dim scrim + spinner over the live (about-to-freeze)
//!   frame, shown while the suspend bracket + snapshot save run (the close path re-shows the
//!   window for the save's duration so this is actually visible).
//! - **Restore** (`Overlay::restore`): the previous session's last-presented frame (the
//!   splash PNG saved at suspend), aspect-fit over black, under the same scrim + spinner —
//!   shown from window creation until the fresh worker presents its first real frame.
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
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetData, CGBitmapInfo,
    CGColor, CGColorSpace, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CABasicAnimation, CALayer, CAMediaTiming, CATextLayer, CATransaction};
use std::path::Path;

/// Spinner diameter in points.
const SPINNER_D: f64 = 40.0;
/// Vertical gap between the spinner center and the caption center, in points.
const LABEL_DROP: f64 = 44.0;

pub(crate) struct Overlay {
    root: Retained<CALayer>,
    /// Bounds-filling sublayers (splash and/or scrim) — re-fit to the view every tick.
    fill: Vec<Retained<CALayer>>,
    /// The rotating arc. Positioned (not framed): a frame is undefined under the rotation
    /// transform, and `fit` must never stretch it.
    spinner: Retained<CALayer>,
    /// The "Suspending…" / "Resuming…" caption under the arc.
    label: Retained<CATextLayer>,
    /// True for the restore flavor: comes down on the first presented frame. The suspend
    /// flavor instead comes down when the suspend is abandoned (bracket timeout).
    pub(crate) until_first_frame: bool,
}

impl Overlay {
    /// Dim scrim + spinner over the live frame (a suspend is in flight).
    pub(crate) fn suspend(host_layer: &CALayer, content: &NSView) -> Self {
        Self::install(host_layer, content, None, false)
    }

    /// Splash (if readable) + scrim + spinner while a restore boots to its first frame.
    pub(crate) fn restore(host_layer: &CALayer, content: &NSView, splash: &Path) -> Self {
        Self::install(host_layer, content, Some(splash), true)
    }

    fn install(
        host_layer: &CALayer,
        content: &NSView,
        splash: Option<&Path>,
        until_first_frame: bool,
    ) -> Self {
        let bounds = content.bounds();
        let root = CALayer::new();
        root.setFrame(bounds);
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
            scrim.setBackgroundColor(Some(&CGColor::new_generic_gray(0.0, 0.35)));
            root.addSublayer(&scrim);
            fill.push(scrim);
        } else {
            // Suspend flavor: the root itself is the scrim over the live frame.
            root.setBackgroundColor(Some(&CGColor::new_generic_gray(0.0, 0.35)));
        }

        let spinner = CALayer::new();
        spinner.setBounds(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(SPINNER_D, SPINNER_D),
        ));
        spinner.setPosition(NSPoint::new(
            bounds.size.width / 2.0,
            bounds.size.height / 2.0,
        ));
        if let Some(img) = spinner_image() {
            let obj: &AnyObject = unsafe { &*(&*img as *const CGImage as *const AnyObject) };
            unsafe { spinner.setContents(Some(obj)) };
        }
        // Crisp on Retina: the image is rendered at 2× and drawn into the point-sized layer.
        spinner.setContentsScale(2.0);
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
        root.addSublayer(&spinner);

        // The caption ("Suspending…" / "Resuming…"), centered under the arc.
        let label = unsafe {
            let label = CATextLayer::new();
            let text = if until_first_frame {
                "Resuming…"
            } else {
                "Suspending…"
            };
            let obj: &AnyObject = &NSString::from_str(text);
            label.setString(Some(obj));
            label.setFontSize(13.0);
            label.setForegroundColor(Some(&CGColor::new_generic_gray(1.0, 0.85)));
            label.setAlignmentMode(&NSString::from_str("center"));
            label.setContentsScale(2.0);
            label.setBounds(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(240.0, 18.0),
            ));
            label.setPosition(NSPoint::new(
                bounds.size.width / 2.0,
                bounds.size.height / 2.0 - LABEL_DROP,
            ));
            label
        };
        root.addSublayer(&label);
        host_layer.addSublayer(&root);

        Self {
            root,
            fill,
            spinner,
            label,
            until_first_frame,
        }
    }

    /// Re-fit to the content view (called from the render timer; cheap, animation-free for
    /// the geometry — the spin animation is unaffected by a position change).
    pub(crate) fn fit(&self, content: &NSView) {
        let bounds = content.bounds();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.root.setFrame(bounds);
        for layer in &self.fill {
            layer.setFrame(bounds);
        }
        self.spinner.setPosition(NSPoint::new(
            bounds.size.width / 2.0,
            bounds.size.height / 2.0,
        ));
        self.label.setPosition(NSPoint::new(
            bounds.size.width / 2.0,
            bounds.size.height / 2.0 - LABEL_DROP,
        ));
        CATransaction::commit();
    }

    /// Tear the overlay down (first frame presented, or the suspend was abandoned).
    pub(crate) fn remove(self) {
        self.root.removeFromSuperlayer();
    }
}

/// Pre-render the spinner arc: a white "comet" ring whose alpha ramps around the circle
/// (opaque head, fading tail), 2× pixels for Retina. BGRA premultiplied little-endian, the
/// same layout the cursor bitmaps use.
fn spinner_image() -> Option<CFRetained<CGImage>> {
    let px = (SPINNER_D * 2.0) as usize;
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
        let c = px as f64 / 2.0;
        let (r_in, r_out) = (c - 12.0, c - 4.0);
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
                data.add(y * px + x).write(a << 24 | a << 16 | a << 8 | a);
            }
        }
        CGBitmapContextCreateImage(Some(&ctx))
    }
}
