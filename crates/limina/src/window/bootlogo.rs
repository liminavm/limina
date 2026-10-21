// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! What the window shows before the guest's first frame: black, and the boot logo where the
//! firmware is about to draw it, so the firmware's first frame changes nothing on screen
//! (docs/design/boot-presentation.md). It comes down when the guest presents.

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSImage, NSView};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CALayer, CATransaction};

/// The firmware's logo (Cardea is built with the same file, `scripts/build-firmware.sh`).
pub(crate) const LOGO: &[u8] = include_bytes!("../../../../assets/firmware/logo.bmp");

/// Where the firmware draws a `width` x `height` logo on a `screen_w` x `screen_h` display: the
/// same rule as Cardea's `cardea_gfx::logo_position`, centred across with its centre 38.2% of
/// the way down, centred instead when it is too tall for that. The two must agree to the pixel.
pub(crate) fn logo_position(
    screen_w: u32,
    screen_h: u32,
    width: u32,
    height: u32,
) -> Option<(u32, u32)> {
    let x = screen_w.checked_sub(width)? / 2;
    let room = screen_h.checked_sub(height)?;
    let golden = (u64::from(screen_h) * 382 / 1000) as u32;
    let y = golden.checked_sub(height / 2).unwrap_or(room / 2);
    Some((x, y))
}

/// A BMP's size in pixels, from its header; `None` for anything else.
pub(crate) fn bmp_size(bmp: &[u8]) -> Option<(u32, u32)> {
    if bmp.get(0..2)? != b"BM" {
        return None;
    }
    let w = i32::from_le_bytes(bmp.get(18..22)?.try_into().ok()?);
    let h = i32::from_le_bytes(bmp.get(22..26)?.try_into().ok()?);
    (w > 0 && h != 0).then(|| (w as u32, h.unsigned_abs()))
}

pub(crate) struct BootLogo {
    root: Retained<CALayer>,
    logo: Retained<CALayer>,
    /// The logo's size in pixels.
    size: (u32, u32),
}

impl BootLogo {
    /// Black over the whole content, the logo on it. `None` if the image will not decode.
    pub(crate) fn install(host_layer: &CALayer, content: &NSView, bmp: &[u8]) -> Option<Self> {
        let size = bmp_size(bmp)?;
        let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(bmp))?;
        let root = CALayer::new();
        root.setBackgroundColor(Some(&CGColor::new_generic_gray(0.0, 1.0)));
        let logo = CALayer::new();
        let obj: &AnyObject = &image;
        unsafe {
            logo.setContents(Some(obj));
            logo.setContentsGravity(&NSString::from_str("resize"));
        }
        root.addSublayer(&logo);
        host_layer.addSublayer(&root);
        let this = Self { root, logo, size };
        this.fit(content);
        Some(this)
    }

    /// Follow the content view: the firmware draws at the window's size in pixels.
    pub(crate) fn fit(&self, content: &NSView) {
        let bounds = content.bounds();
        let scale = content.window().map_or(1.0, |w| w.backingScaleFactor());
        let screen = (
            (bounds.size.width * scale).round() as u32,
            (bounds.size.height * scale).round() as u32,
        );
        let (w, h) = self.size;
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.root.setFrame(bounds);
        match logo_position(screen.0, screen.1, w, h) {
            Some((x, y)) => {
                self.logo.setHidden(false);
                // The layer's origin is the bottom left; the firmware's, the top left.
                self.logo.setFrame(NSRect::new(
                    NSPoint::new(f64::from(x) / scale, f64::from(screen.1 - y - h) / scale),
                    NSSize::new(f64::from(w) / scale, f64::from(h) / scale),
                ));
            }
            None => self.logo.setHidden(true),
        }
        CATransaction::commit();
    }

    pub(crate) fn remove(self) {
        self.root.removeFromSuperlayer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logo_lands_where_cardea_draws_it() {
        // Cardea's own cases (`cardea_gfx::bmp` tests): the two rules must agree to the pixel.
        assert_eq!(logo_position(1280, 800, 256, 256), Some((512, 177)));
        assert_eq!(logo_position(2560, 1440, 512, 512), Some((1024, 294)));
        assert_eq!(logo_position(1281, 801, 255, 255), Some((513, 178)));
        assert_eq!(logo_position(100, 100, 50, 90), Some((25, 5)));
        assert_eq!(logo_position(100, 100, 101, 10), None);
    }

    #[test]
    fn the_embedded_logo_is_a_bmp_of_known_size() {
        assert_eq!(bmp_size(LOGO), Some((256, 256)));
        assert_eq!(bmp_size(b"PNG"), None);
        assert_eq!(bmp_size(&LOGO[..20]), None);
    }
}
