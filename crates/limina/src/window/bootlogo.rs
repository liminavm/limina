// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! What the window shows before the guest's first frame: black, and the boot logo where the
//! firmware is about to draw it, so the firmware's first frame changes nothing on screen
//! (docs/design/boot-presentation.md). It comes down when the guest presents.
//!
//! The logo is decoded and placed by Cardea's own code (`cardea_gfx`), so the window and the
//! firmware agree to the pixel, and a VM's logo the firmware would not draw is refused before
//! the VM starts ([`load`]).

use std::path::Path;

use anyhow::Context;
use cardea_gfx::{Bmp, logo_position};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSImage, NSView};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CALayer, CATransaction};

/// The firmware's logo (Cardea is built with the same file, `scripts/build-firmware.sh`).
pub(crate) const LOGO: &[u8] = include_bytes!("../../../../assets/firmware/logo.bmp");

/// The largest logo the firmware takes from the VMM: libkrun's boot-logo window.
const MAX_LOGO_BYTES: u64 = 4 << 20;

/// A VM's own boot logo, read and checked as the firmware will check it.
pub(crate) fn load(path: &Path) -> anyhow::Result<Vec<u8>> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("boot logo {}", path.display()))?
        .len();
    anyhow::ensure!(
        len <= MAX_LOGO_BYTES,
        "boot logo {} is {len} bytes; the firmware takes at most {MAX_LOGO_BYTES}",
        path.display()
    );
    let bmp = std::fs::read(path).with_context(|| format!("boot logo {}", path.display()))?;
    check(&bmp).with_context(|| format!("boot logo {}", path.display()))?;
    Ok(bmp)
}

fn check(bmp: &[u8]) -> anyhow::Result<()> {
    Bmp::parse(bmp).map(|_| ()).map_err(|e| {
        anyhow::anyhow!(
            "the firmware cannot draw it ({e:?}): it takes an uncompressed BMP at 8 \
             (palettised), 24 or 32 bits per pixel"
        )
    })
}

/// `bmp` as Cardea decodes it, re-encoded as a bottom-up 24-bit BMP: the one form every image
/// decoder reads the same way (a 32-bit BMP's fourth byte is reserved to Cardea, alpha to
/// others).
fn as_drawn(bmp: &Bmp<'_>) -> Option<Vec<u8>> {
    let pixels = bmp.decode().ok()?;
    let (w, h) = (bmp.width() as usize, bmp.height() as usize);
    let stride = (w * 3).next_multiple_of(4);
    let data_len = stride * h;
    let mut out = Vec::with_capacity(54 + data_len);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&u32::try_from(54 + data_len).ok()?.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for row in pixels.chunks_exact(w).rev() {
        let start = out.len();
        for p in row {
            let [b, g, r, _] = p.0.to_le_bytes();
            out.extend_from_slice(&[b, g, r]);
        }
        out.resize(start + stride, 0);
    }
    Some(out)
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
        let parsed = Bmp::parse(bmp).ok()?;
        let size = (parsed.width(), parsed.height());
        let drawn = as_drawn(&parsed)?;
        let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&drawn))?;
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

    /// A 2x2 BMP at `bpp` bits (24 or 32), bottom-up, with `extra` as each pixel's fourth byte.
    fn tiny(bpp: u16, extra: u8) -> Vec<u8> {
        let px: [[u8; 3]; 4] = [[1, 2, 3], [4, 5, 6], [7, 8, 9], [10, 11, 12]];
        let bytes = usize::from(bpp / 8);
        let stride = (2 * bytes).next_multiple_of(4);
        let mut b = Vec::new();
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&((54 + stride * 2) as u32).to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&54u32.to_le_bytes());
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&2i32.to_le_bytes());
        b.extend_from_slice(&2i32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&bpp.to_le_bytes());
        b.extend_from_slice(&[0; 24]);
        for row in px.chunks(2) {
            let start = b.len();
            for p in row {
                b.extend_from_slice(p);
                if bytes == 4 {
                    b.push(extra);
                }
            }
            b.resize(start + stride, 0);
        }
        b
    }

    #[test]
    fn the_embedded_logo_is_one_the_firmware_draws() {
        let bmp = Bmp::parse(LOGO).unwrap();
        assert_eq!((bmp.width(), bmp.height()), (256, 256));
        assert!(check(LOGO).is_ok());
    }

    #[test]
    fn a_logo_the_firmware_refuses_is_refused() {
        assert!(check(b"PNG").is_err());
        let mut rle = tiny(24, 0);
        rle[30] = 1; // BI_RLE8
        let why = check(&rle).unwrap_err().to_string();
        assert!(why.contains("uncompressed BMP"), "{why}");
        assert!(check(&tiny(24, 0)[..60]).is_err(), "truncated");
    }

    #[test]
    fn what_the_window_shows_is_what_the_firmware_decodes() {
        // A 32-bit logo whose reserved byte is zero: an alpha-reading decoder would see it
        // transparent; Cardea draws it opaque, and so must the window.
        for bmp in [tiny(24, 0), tiny(32, 0), tiny(32, 0x80)] {
            let parsed = Bmp::parse(&bmp).unwrap();
            let drawn = as_drawn(&parsed).unwrap();
            let again = Bmp::parse(&drawn).unwrap();
            assert_eq!(again.bits_per_pixel(), 24);
            assert_eq!(again.decode().unwrap(), parsed.decode().unwrap());
        }
        assert_eq!(
            as_drawn(&Bmp::parse(&tiny(32, 0)).unwrap()),
            as_drawn(&Bmp::parse(&tiny(24, 0)).unwrap())
        );
    }

    #[test]
    fn a_logo_too_large_for_the_window_is_refused() {
        let dir = std::env::temp_dir().join(format!("limina-logo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.bmp");
        std::fs::write(&big, vec![0u8; MAX_LOGO_BYTES as usize + 1]).unwrap();
        assert!(load(&big).unwrap_err().to_string().contains("at most"));
        let ok = dir.join("ok.bmp");
        std::fs::write(&ok, tiny(24, 0)).unwrap();
        assert_eq!(load(&ok).unwrap(), tiny(24, 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
