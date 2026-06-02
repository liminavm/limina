// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina display backends for the libkrun virtio-gpu device.
//!
//! A *display backend* is the host's sink for guest scanout frames. libkrun calls our
//! backend's [`DisplayBackendBasicFramebuffer`] methods on its GPU worker thread:
//! `configure_scanout` (resolution/format), `alloc_frame` (hand back a buffer libkrun
//! fills), then `present_frame` (the buffer now holds the frame). We implement that
//! trait in *plain safe Rust*; libkrun's generic [`IntoDisplayBackend`] wrapper
//! auto-generates the `extern "C"` vtable, so limina never hand-writes a `#[repr(C)]`
//! vtable (decision D2.1).
//!
//! ## Two display tiers (mirrors the project's two-tier guarantee)
//! - **Tier 1 — CPU framebuffer (here).** libkrun reads the guest scanout into a host
//!   CPU buffer (`read_2d_resource`, tightly packed `width * 4` BGRA). That copy is the
//!   compatibility floor *and* the natural home for a capture-to-PNG test oracle — to
//!   write a PNG you need bytes in CPU memory anyway. [`CaptureBackend`] is that oracle.
//! - **Tier 2 — zero-copy IOSurface (future).** The guest's accelerated scanout already
//!   lives in a host Metal texture; present it directly to a `CAMetalLayer` with no CPU
//!   readback. That needs libkrun patches (`SetScanoutBlob` + a surface-present method)
//!   and lands later. The capture oracle still applies there via a one-shot IOSurface
//!   readback. The seam is deliberately kept small so that backend slots in alongside.

use std::path::PathBuf;

use krun_display::{
    DisplayBackend, DisplayBackendBasicFramebuffer, DisplayBackendError, DisplayBackendNew,
    IntoDisplayBackend, Rect, ResourceFormat,
};

mod iosurface;
pub use iosurface::{window_backend, WindowBackend, WindowConfig};

/// How many scanouts a [`CaptureBackend`] tracks. M2 drives a single display.
const MAX_TRACKED_SCANOUTS: usize = 1;

/// Configuration for [`CaptureBackend`]. Must be `Sync` (libkrun's `DisplayBackendNew`
/// bound) and is read once, on the GPU worker thread, when the backend is instantiated.
#[derive(Clone, Debug)]
pub struct CaptureConfig {
    /// Where to write the captured frame as a PNG. Overwritten on every present, so the
    /// file always holds the most recent frame (e.g. the last one before guest power-off).
    pub png_path: PathBuf,
}

/// A host display backend that captures guest scanout frames to a PNG file.
///
/// This is the headless validation oracle for the display pipeline (Tier 1): it lets a
/// test boot a guest, let it render, and then assert on real pixels — dimensions, byte
/// order, and content — instead of eyeballing a window.
pub struct CaptureBackend {
    config: CaptureConfig,
    /// The single scanout we track: its current geometry + the host-side frame buffer
    /// libkrun fills. `None` until `configure_scanout`.
    scanout: Option<Scanout>,
    /// Monotonic frame id handed out by `alloc_frame` (libkrun echoes it to `present_frame`).
    next_frame_id: u32,
    /// Count of frames presented so far — surfaced for tests/logging.
    presented: u64,
}

struct Scanout {
    width: u32,
    height: u32,
    format: ResourceFormat,
    /// `width * height * BYTES_PER_PIXEL`; libkrun writes the scanout into this on each
    /// `alloc_frame`. We never reallocate it between `alloc_frame` and `present_frame`
    /// (libkrun holds a raw pointer to it across that span), only on `configure_scanout`.
    buffer: Vec<u8>,
}

impl DisplayBackendNew<CaptureConfig> for CaptureBackend {
    fn new(userdata: Option<&CaptureConfig>) -> Self {
        let config = userdata
            .cloned()
            .expect("CaptureBackend requires a CaptureConfig userdata");
        CaptureBackend {
            config,
            scanout: None,
            next_frame_id: 0,
            presented: 0,
        }
    }
}

impl DisplayBackendBasicFramebuffer for CaptureBackend {
    fn configure_scanout(
        &mut self,
        scanout_id: u32,
        display_width: u32,
        display_height: u32,
        width: u32,
        height: u32,
        format: ResourceFormat,
    ) -> Result<(), DisplayBackendError> {
        if scanout_id as usize >= MAX_TRACKED_SCANOUTS {
            return Err(DisplayBackendError::InvalidScanoutId);
        }
        let len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(ResourceFormat::BYTES_PER_PIXEL))
            .ok_or(DisplayBackendError::InvalidParam)?;
        log::info!(
            "capture: configure scanout {scanout_id}: {width}x{height} {format:?} \
             (display {display_width}x{display_height}, {len} bytes)"
        );
        self.scanout = Some(Scanout {
            width,
            height,
            format,
            buffer: vec![0u8; len],
        });
        Ok(())
    }

    fn disable_scanout(&mut self, scanout_id: u32) -> Result<(), DisplayBackendError> {
        if scanout_id as usize >= MAX_TRACKED_SCANOUTS {
            return Err(DisplayBackendError::InvalidScanoutId);
        }
        log::info!("capture: disable scanout {scanout_id}");
        self.scanout = None;
        Ok(())
    }

    fn alloc_frame(&mut self, scanout_id: u32) -> Result<(u32, &mut [u8]), DisplayBackendError> {
        let scanout = self
            .scanout
            .as_mut()
            .filter(|_| (scanout_id as usize) < MAX_TRACKED_SCANOUTS)
            .ok_or(DisplayBackendError::InvalidScanoutId)?;
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        Ok((frame_id, &mut scanout.buffer))
    }

    fn present_frame(
        &mut self,
        scanout_id: u32,
        _frame_id: u32,
        _rect: Option<&Rect>,
    ) -> Result<(), DisplayBackendError> {
        let scanout = self
            .scanout
            .as_ref()
            .filter(|_| (scanout_id as usize) < MAX_TRACKED_SCANOUTS)
            .ok_or(DisplayBackendError::InvalidScanoutId)?;

        if let Err(e) = write_png(&self.config.png_path, scanout) {
            // A failed capture must not wedge the guest's display pipeline — log and move on.
            log::error!("capture: failed to write {:?}: {e:#}", self.config.png_path);
            return Err(DisplayBackendError::InternalError);
        }
        self.presented += 1;
        log::debug!(
            "capture: presented frame #{} to {:?} ({}x{})",
            self.presented,
            self.config.png_path,
            scanout.width,
            scanout.height
        );
        Ok(())
    }
}

/// Build a `DisplayBackend` for [`CaptureBackend`] suitable for `VmResources`.
///
/// `VmResources::display_backend` needs a `DisplayBackend<'static>`, but libkrun's
/// `into_display_backend` borrows the userdata for the returned backend's lifetime. The
/// config is tiny and lives for the whole VM, so we intentionally leak it to `'static`
/// rather than thread a lifetime through the VM builder.
pub fn capture_backend(config: CaptureConfig) -> DisplayBackend<'static> {
    let leaked: &'static CaptureConfig = Box::leak(Box::new(config));
    CaptureBackend::into_display_backend(Some(leaked))
}

/// Encode a scanout's BGRA/RGBA host buffer as an 8-bit RGBA PNG.
fn write_png(path: &std::path::Path, scanout: &Scanout) -> anyhow::Result<()> {
    use anyhow::Context;

    let mut rgba = vec![0u8; scanout.buffer.len()];
    swizzle_to_rgba(scanout.format, &scanout.buffer, &mut rgba);

    // Write to a temp sibling then rename, so a reader never sees a half-written PNG.
    let tmp = path.with_extension("png.tmp");
    {
        let file = std::fs::File::create(&tmp).with_context(|| format!("create {tmp:?}"))?;
        let w = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(w, scanout.width, scanout.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("png write_header")?;
        writer
            .write_image_data(&rgba)
            .context("png write_image_data")?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename {tmp:?} -> {path:?}"))?;
    Ok(())
}

/// Convert a 4-byte-per-pixel framebuffer in `format`'s in-memory byte order to RGBA.
///
/// libkrun's `ResourceFormat` names list bytes in memory order (e.g. `BGRA` = byte0 B,
/// byte1 G, byte2 R, byte3 A). The `X` (unused) channel maps to opaque alpha 0xFF.
fn swizzle_to_rgba(format: ResourceFormat, src: &[u8], dst: &mut [u8]) {
    // (r_idx, g_idx, b_idx, a_idx); a_idx == None -> force 0xFF.
    let (r, g, b, a): (usize, usize, usize, Option<usize>) = match format {
        ResourceFormat::BGRA => (2, 1, 0, Some(3)),
        ResourceFormat::BGRX => (2, 1, 0, None),
        ResourceFormat::ARGB => (1, 2, 3, Some(0)),
        ResourceFormat::XRGB => (1, 2, 3, None),
        ResourceFormat::RGBA => (0, 1, 2, Some(3)),
        ResourceFormat::RGBX => (0, 1, 2, None),
        ResourceFormat::ABGR => (3, 2, 1, Some(0)),
        ResourceFormat::XBGR => (3, 2, 1, None),
    };
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d[0] = s[r];
        d[1] = s[g];
        d[2] = s[b];
        d[3] = a.map_or(0xFF, |i| s[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swizzle_bgra_to_rgba() {
        // One pixel, BGRA in memory = [B=10, G=20, R=30, A=40] -> RGBA [30,20,10,40].
        let src = [10u8, 20, 30, 40];
        let mut dst = [0u8; 4];
        swizzle_to_rgba(ResourceFormat::BGRA, &src, &mut dst);
        assert_eq!(dst, [30, 20, 10, 40]);
    }

    #[test]
    fn swizzle_bgrx_forces_opaque_alpha() {
        let src = [10u8, 20, 30, 0];
        let mut dst = [0u8; 4];
        swizzle_to_rgba(ResourceFormat::BGRX, &src, &mut dst);
        assert_eq!(dst, [30, 20, 10, 0xFF]);
    }

    #[test]
    fn swizzle_xrgb_forces_opaque_alpha() {
        // XRGB in memory = [X, R=30, G=20, B=10] -> RGBA [30,20,10,255].
        let src = [0u8, 30, 20, 10];
        let mut dst = [0u8; 4];
        swizzle_to_rgba(ResourceFormat::XRGB, &src, &mut dst);
        assert_eq!(dst, [30, 20, 10, 0xFF]);
    }
}
