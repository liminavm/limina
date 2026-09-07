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
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use krun_display::{
    DisplayBackend, DisplayBackendBasicFramebuffer, DisplayBackendError, DisplayBackendNew,
    IntoDisplayBackend, Rect, ResourceFormat,
};

mod iosurface;
pub use iosurface::{WindowBackend, WindowConfig, window_backend};

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
    /// The single scanout we track: its current geometry + the host-side frame buffer
    /// libkrun fills. `None` until `configure_scanout`.
    scanout: Option<Scanout>,
    /// Monotonic frame id handed out by `alloc_frame` (libkrun echoes it to `present_frame`).
    next_frame_id: u32,
    /// Count of frames presented so far — surfaced for tests/logging.
    presented: u64,
    /// Off-thread PNG encoder. `present_frame` only snapshots the buffer and returns; the
    /// writer thread encodes the latest frame. Encoding a 1280x800 PNG takes ~20ms, and the
    /// guest's `RESOURCE_FLUSH` is *synchronous* — a firmware/GRUB console that Blts a glyph
    /// at a time would otherwise stall the boot for tens of seconds. Frames are coalesced:
    /// the encoder always works on the most recent one, dropping any it couldn't keep up with.
    writer: AsyncWriter,
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
        let writer = AsyncWriter::new(config.png_path.clone());
        CaptureBackend {
            scanout: None,
            next_frame_id: 0,
            presented: 0,
            writer,
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
        // A same-geometry re-declaration keeps the buffer it already has. The guest re-declares
        // a scanout it has not changed -- Fedora walks simpledrm -> plymouth -> GDM that way, and
        // a compositor re-laying-out after an EDID change does it once per frame -- and each of
        // those arrives here as a `configure_scanout` identical to the live one. Replacing the
        // buffer on those is not a resize, it is erasing the frame the guest just drew: the next
        // `present_frame` snapshots zeroes and the capture goes black. `WindowBackend` has always
        // taken this branch; this backend reallocating instead is what made a re-laid-out desktop
        // capture as a solid black frame, at 60 re-declarations a second.
        if let Some(s) = self.scanout.as_ref()
            && s.width == width
            && s.height == height
            && s.format == format
        {
            return Ok(());
        }
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
        // LIMINA_CAPTURE_SENTINEL=1 paints the buffer magenta before the device fills it, so a
        // captured frame says which of two things happened. A black frame means something wrote
        // black; a magenta one means nothing wrote at all. Zero cannot make that distinction
        // here: the scanout is BGRX and `swizzle_to_rgba` forces the X channel to opaque alpha,
        // so an untouched buffer and a deliberately cleared one both encode as (0,0,0,255).
        if std::env::var_os("LIMINA_CAPTURE_SENTINEL").is_some() {
            // BGRX byte order: B=0xFF, G=0x00, R=0xFF -> magenta, which no desktop paints
            // edge to edge.
            for px in scanout.buffer.as_chunks_mut::<4>().0 {
                px.copy_from_slice(&[0xFF, 0x00, 0xFF, 0xFF]);
            }
        }
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

        // Snapshot and hand off to the encoder thread; never block the guest's flush on PNG
        // encoding. The writer coalesces, so the file always converges on the latest frame.
        self.writer.submit(PendingFrame {
            width: scanout.width,
            height: scanout.height,
            format: scanout.format,
            buffer: scanout.buffer.clone(),
        });
        self.presented += 1;
        Ok(())
    }
}

/// A frame snapshot handed from the GPU worker thread to the [`AsyncWriter`].
struct PendingFrame {
    width: u32,
    height: u32,
    format: ResourceFormat,
    /// Raw scanout bytes in `format`'s byte order; swizzled to RGBA by the writer.
    buffer: Vec<u8>,
}

/// Shared state between `present_frame` and the encoder thread: a single coalescing slot
/// plus a stop flag.
struct WriterShared {
    state: Mutex<WriterState>,
    cv: Condvar,
}

struct WriterState {
    /// The most recent frame awaiting encode (older un-encoded frames are dropped).
    pending: Option<PendingFrame>,
    stop: bool,
}

/// Encodes captured frames to PNG on a dedicated thread so a synchronous guest flush never
/// waits on PNG encoding. Frames are coalesced to the latest; on drop the thread drains the
/// final pending frame and joins.
struct AsyncWriter {
    shared: Arc<WriterShared>,
    handle: Option<JoinHandle<()>>,
}

impl AsyncWriter {
    fn new(path: PathBuf) -> Self {
        let shared = Arc::new(WriterShared {
            state: Mutex::new(WriterState {
                pending: None,
                stop: false,
            }),
            cv: Condvar::new(),
        });
        let worker = shared.clone();
        let handle = std::thread::Builder::new()
            .name("capture writer".into())
            .spawn(move || {
                let mut presented: u64 = 0;
                loop {
                    let frame = {
                        let mut guard = worker.state.lock().unwrap();
                        loop {
                            if let Some(frame) = guard.pending.take() {
                                break frame;
                            }
                            if guard.stop {
                                return;
                            }
                            guard = worker.cv.wait(guard).unwrap();
                        }
                    };
                    if let Err(e) = write_png(&path, &frame) {
                        log::error!("capture: failed to write {path:?}: {e:#}");
                    } else {
                        presented += 1;
                        log::debug!(
                            "capture: wrote frame #{presented} to {path:?} ({}x{})",
                            frame.width,
                            frame.height
                        );
                    }
                }
            })
            .expect("spawn capture writer thread");
        AsyncWriter {
            shared,
            handle: Some(handle),
        }
    }

    fn submit(&self, frame: PendingFrame) {
        {
            let mut guard = self.shared.state.lock().unwrap();
            guard.pending = Some(frame); // coalesce: replace any not-yet-encoded frame
        }
        self.shared.cv.notify_one();
    }
}

impl Drop for AsyncWriter {
    fn drop(&mut self) {
        {
            let mut guard = self.shared.state.lock().unwrap();
            guard.stop = true;
        }
        self.shared.cv.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
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

/// Encode a captured frame's BGRA/RGBA host buffer as an 8-bit RGBA PNG.
fn write_png(path: &std::path::Path, frame: &PendingFrame) -> anyhow::Result<()> {
    use anyhow::Context;

    let mut rgba = vec![0u8; frame.buffer.len()];
    swizzle_to_rgba(frame.format, &frame.buffer, &mut rgba);

    // Write to a temp sibling then rename, so a reader never sees a half-written PNG.
    let tmp = path.with_extension("png.tmp");
    {
        let file = std::fs::File::create(&tmp).with_context(|| format!("create {tmp:?}"))?;
        let w = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(w, frame.width, frame.height);
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
    for (s, d) in src
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dst.as_chunks_mut::<4>().0.iter_mut())
    {
        d[0] = s[r];
        d[1] = s[g];
        d[2] = s[b];
        d[3] = a.map_or(0xFF, |i| s[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame survives a re-declaration of the scanout it is already on. A compositor
    /// re-laying-out after a monitor change re-declares the same mode once per frame; if each
    /// one replaced the staging buffer, every present in that window would capture zeroes and
    /// a live desktop would read as a solid black frame.
    #[test]
    fn a_same_geometry_reconfigure_keeps_the_frame() {
        let dir = std::env::temp_dir().join(format!("limina-capture-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test scratch");
        let mut b = CaptureBackend::new(Some(&CaptureConfig {
            png_path: dir.join("cap.png"),
        }));
        b.configure_scanout(0, 64, 32, 64, 32, ResourceFormat::BGRX)
            .expect("first configure");

        // The guest draws.
        let (_id, buf) = b.alloc_frame(0).expect("alloc");
        buf.fill(0xAB);

        // ... and re-declares the very same scanout before the frame is presented.
        b.configure_scanout(0, 64, 32, 64, 32, ResourceFormat::BGRX)
            .expect("same-geometry reconfigure");
        let (_id, buf) = b.alloc_frame(0).expect("alloc after reconfigure");
        assert!(
            buf.iter().all(|&x| x == 0xAB),
            "a same-geometry reconfigure erased the frame the guest had drawn"
        );

        // A real mode change still reallocates, and the new buffer is the new size.
        b.configure_scanout(0, 128, 64, 128, 64, ResourceFormat::BGRX)
            .expect("mode change");
        let (_id, buf) = b.alloc_frame(0).expect("alloc after mode change");
        assert_eq!(
            buf.len(),
            128 * 64 * ResourceFormat::BYTES_PER_PIXEL,
            "a real mode change must resize the buffer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
