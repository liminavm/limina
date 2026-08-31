// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Present-path diagnostics: the LIMINA_WINDOW_CAPTURE / LIMINA_CAPTURE_IDS IOSurface-dump
//! oracles (pixel truth without Screen Recording permission) and the LIMINA_PRESENT_COPY /
//! LIMINA_PRESENT_LOCK present-race probes (see the arming comments in [`super::run`]).

use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber,
    CFNumberType, CFRetained, CFString,
};
use std::time::Duration;

use objc2_io_surface::{
    kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat,
    kIOSurfaceWidth, IOSurfaceCreate, IOSurfaceGetBaseAddress, IOSurfaceGetBytesPerRow,
    IOSurfaceGetHeight, IOSurfaceGetWidth, IOSurfaceLock, IOSurfaceLockOptions, IOSurfaceRef,
    IOSurfaceUnlock,
};

/// Minimum time between periodic capture dumps (`LIMINA_WINDOW_CAPTURE_INTERVAL_MS`, default
/// 1000).
///
/// Counted in TIME, not in applies, because the thing being watched is usually a desktop that
/// is nearly still — and the stiller it is, the less often an apply-counted dump fires. A
/// panel clock ticking once a minute is one apply a minute, so at the old default of 120
/// applies the file on disk was two hours stale while looking perfectly current. That has
/// caught us repeatedly: a probe ends a whole session with no frame, or worse, a test compares
/// two reads of the same unchanged file and concludes the desktop settled.
///
/// A dump still needs something to be presented — a screen where truly nothing happens has
/// nothing new to write — but any presented frame is now written within the interval of it.
pub(crate) fn capture_interval_from_env() -> Duration {
    // The old apply-counted knob, honoured so an existing invocation is not silently ignored.
    // One apply is the useful setting of it, and that is what the time gate does by default.
    if let Ok(v) = std::env::var("LIMINA_WINDOW_CAPTURE_EVERY") {
        if v.trim().parse::<u64>().is_ok() {
            log::warn!(
                "LIMINA_WINDOW_CAPTURE_EVERY counts applies and is superseded by \
                 LIMINA_WINDOW_CAPTURE_INTERVAL_MS, which counts milliseconds; treating it as \
                 a request for the most frequent cadence"
            );
            return Duration::ZERO;
        }
    }
    std::env::var("LIMINA_WINDOW_CAPTURE_INTERVAL_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(1000))
}

/// Parse `LIMINA_CAPTURE_IDS` — the ids to ALSO dump by global lookup each tick, regardless
/// of what the window presents. Lets us peek the venus SET_SCANOUT_BLOB surface (e.g. id 38)
/// even when a competing 2D ring is what's on screen. LIMINA_CAPTURE_IDS="33,38,39".
/// Accepts a comma list ("33,38") and/or inclusive ranges ("30-50").
pub(crate) fn capture_ids_from_env() -> Vec<u32> {
    std::env::var("LIMINA_CAPTURE_IDS")
        .ok()
        .map(|s| {
            let mut out = Vec::new();
            for t in s.split(',') {
                let t = t.trim();
                if let Some((a, b)) = t.split_once('-') {
                    if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                        out.extend(a..=b);
                    }
                } else if let Ok(v) = t.parse::<u32>() {
                    out.push(v);
                }
            }
            out
        })
        .unwrap_or_default()
}

/// One grabbed scanout, detached from the IOSurface so it can be encoded on another thread.
///
/// Holds the surface's own BGRA bytes with their original row stride; the swizzle to RGBA is
/// the encoder's job, not the present thread's.
pub(crate) struct CaptureFrame {
    pub w: usize,
    pub h: usize,
    pub bpr: usize,
    pub bgra: Vec<u8>,
    pub id: u32,
    pub path: String,
}

/// Lock the IOSurface (which syncs against the GPU) and copy its bytes out. Needs no Screen
/// Recording permission. (A premultiplied `CALayer.renderInContext` would zero the RGB wherever
/// the guest's "don't care" scanout alpha is 0; reading the surface directly shows the true
/// scanout content.)
///
/// This is the only part that must run where the surface lives, so it is the only part the
/// present thread pays for: the lock's GPU wait plus one memcpy per row.
pub(crate) fn grab_capture_frame(
    surface: &IOSurfaceRef,
    id: u32,
    path: &str,
) -> Option<CaptureFrame> {
    unsafe {
        // ReadOnly + default (no AvoidSync) → waits for the GPU to finish writing.
        if surface.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut()) != 0 {
            log::error!("capture: IOSurfaceLock failed");
            return None;
        }
        let w = surface.width();
        let h = surface.height();
        let bpr = surface.bytes_per_row();
        let base = surface.base_address().as_ptr() as *const u8;
        let mut bgra = vec![0u8; h * bpr];
        std::ptr::copy_nonoverlapping(base, bgra.as_mut_ptr(), h * bpr);
        let _ = surface.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
        Some(CaptureFrame {
            w,
            h,
            bpr,
            bgra,
            id,
            path: path.to_string(),
        })
    }
}

/// Swizzle BGRA→RGBA with alpha forced opaque, and write the PNG.
///
/// Alpha is forced because the scanout's is "don't care": honouring it renders a correct frame
/// as fully transparent, which reads as a black capture.
pub(crate) fn encode_capture_png(frame: &CaptureFrame) {
    let CaptureFrame {
        w,
        h,
        bpr,
        bgra,
        id,
        path,
    } = frame;
    let (w, h, bpr) = (*w, *h, *bpr);
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = &bgra[y * bpr..y * bpr + w * 4];
        let out = &mut rgba[y * w * 4..(y + 1) * w * 4];
        for x in 0..w {
            let px = &row[x * 4..x * 4 + 4]; // BGRA in memory
            let o = x * 4;
            out[o] = px[2];
            out[o + 1] = px[1];
            out[o + 2] = px[0];
            out[o + 3] = 255; // force opaque — the scanout alpha is "don't care"
        }
    }

    match std::fs::File::create(path) {
        Ok(f) => {
            let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            match enc
                .write_header()
                .and_then(|mut wr| wr.write_image_data(&rgba))
            {
                Ok(()) => {
                    // Report a coarse luminance sum so the log alone tells black vs content.
                    let nonzero = rgba
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .filter(|p| p[0] | p[1] | p[2] != 0)
                        .count();
                    log::info!(
                        "capture: wrote IOSurface id={id} {w}x{h} to {path} (nonzero_px={nonzero})"
                    );
                }
                Err(e) => log::error!("capture: png write failed: {e}"),
            }
        }
        Err(e) => log::error!("capture: create {path} failed: {e}"),
    }
}

/// Hand a grabbed frame to the encoder thread, or drop it.
///
/// Never blocks: the channel holds one frame, and a frame offered while the encoder is still
/// busy is discarded. Dropping is the right call for a diagnostic that overwrites one file —
/// the next present offers a newer frame anyway, and the alternative is making the present
/// thread wait on a PNG deflate.
pub(crate) fn offer_capture(
    tx: &std::sync::mpsc::SyncSender<CaptureFrame>,
    f: CaptureFrame,
) -> bool {
    match tx.try_send(f) {
        Ok(()) => true,
        Err(std::sync::mpsc::TrySendError::Full(f)) => {
            log::debug!("capture: encoder busy, dropping frame for {}", f.path);
            false
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(f)) => {
            log::warn!("capture: encoder gone, dropping frame for {}", f.path);
            false
        }
    }
}

/// The process-wide encoder thread. Spawned on first use.
fn capture_encoder() -> &'static std::sync::mpsc::SyncSender<CaptureFrame> {
    static TX: std::sync::OnceLock<std::sync::mpsc::SyncSender<CaptureFrame>> =
        std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<CaptureFrame>(1);
        std::thread::Builder::new()
            .name("limina-capture".into())
            .spawn(move || {
                while let Ok(f) = rx.recv() {
                    encode_capture_png(&f);
                }
            })
            .expect("spawn capture encoder");
        tx
    })
}

/// Periodic diagnostic capture of a presented scanout.
///
/// The swizzle and the PNG deflate of a full-resolution frame cost hundreds of milliseconds —
/// on the AppKit main thread that is a visible per-second stall of the window and of input,
/// with the guest itself running perfectly (measured 2026-08-30: 41% of the main thread at
/// 2560x1440, once a second). So only the grab happens here; the rest is the encoder thread's.
pub(crate) fn capture_iosurface_async(surface: &IOSurfaceRef, id: u32, path: &str) {
    if let Some(f) = grab_capture_frame(surface, id, path) {
        offer_capture(capture_encoder(), f);
    }
}

/// Synchronous capture, for the one caller that cannot outlive itself: the felt-resume splash
/// save runs as the window parks, so the frame must be on disk before we return.
pub(crate) fn capture_iosurface(surface: &IOSurfaceRef, id: u32, path: &str) {
    if let Some(f) = grab_capture_frame(surface, id, path) {
        encode_capture_png(&f);
    }
}

/// Plain local BGRA IOSurface for the LIMINA_PRESENT_COPY ring (not global — only this
/// process touches it).
pub(crate) fn create_local_iosurface(width: u32, height: u32) -> Option<CFRetained<IOSurfaceRef>> {
    use std::ffi::c_void;
    let pixel_format = i32::from_be_bytes(*b"BGRA");
    // Align the row stride to 256 bytes — a tight `width*4` stride composites BLANK in CoreAnimation
    // for widths that aren't 64-aligned (see the matching note in limina-display's
    // create_global_iosurface). `copy_surface` honors both surfaces' real `bytesPerRow`.
    let bytes_per_row = (((width * 4) + 255) & !255) as i32;
    unsafe fn cfnum(v: i32) -> Option<CFRetained<CFNumber>> {
        unsafe {
            CFNumber::new(
                None,
                CFNumberType::SInt32Type,
                &v as *const i32 as *const c_void,
            )
        }
    }
    unsafe {
        let vw = cfnum(width as i32)?;
        let vh = cfnum(height as i32)?;
        let vbpe = cfnum(4)?;
        let vbpr = cfnum(bytes_per_row)?;
        let vpf = cfnum(pixel_format)?;
        let mut keys: [*const c_void; 5] = [
            (kIOSurfaceWidth as *const CFString).cast(),
            (kIOSurfaceHeight as *const CFString).cast(),
            (kIOSurfaceBytesPerElement as *const CFString).cast(),
            (kIOSurfaceBytesPerRow as *const CFString).cast(),
            (kIOSurfacePixelFormat as *const CFString).cast(),
        ];
        let mut values: [*const c_void; 5] = [
            (&*vw as *const CFNumber).cast(),
            (&*vh as *const CFNumber).cast(),
            (&*vbpe as *const CFNumber).cast(),
            (&*vbpr as *const CFNumber).cast(),
            (&*vpf as *const CFNumber).cast(),
        ];
        let dict = CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )?;
        IOSurfaceCreate(&dict)
    }
}

/// Wait for in-flight GPU writes to the surface to land, then release it untouched.
/// IOSurfaceLock is the only cross-process "GPU writes done?" primitive available to us
/// here; the lock/unlock pair costs only the wait itself (no copy, no page faults).
pub(crate) fn sync_surface(surface: &CFRetained<IOSurfaceRef>) {
    unsafe {
        IOSurfaceLock(
            surface,
            IOSurfaceLockOptions::ReadOnly,
            std::ptr::null_mut(),
        );
        IOSurfaceUnlock(
            surface,
            IOSurfaceLockOptions::ReadOnly,
            std::ptr::null_mut(),
        );
    }
}

/// Row-wise copy of one BGRA IOSurface into another (clamped to the smaller geometry).
/// ~4 MB/frame at 1280×800 — trivially cheap next to what it buys (see LIMINA_PRESENT_COPY).
pub(crate) fn copy_surface(src: &CFRetained<IOSurfaceRef>, dst: &CFRetained<IOSurfaceRef>) {
    unsafe {
        IOSurfaceLock(src, IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
        IOSurfaceLock(dst, IOSurfaceLockOptions(0), std::ptr::null_mut());
        let sb = IOSurfaceGetBaseAddress(src).as_ptr() as *const u8;
        let db = IOSurfaceGetBaseAddress(dst).as_ptr() as *mut u8;
        let ss = IOSurfaceGetBytesPerRow(src);
        let ds = IOSurfaceGetBytesPerRow(dst);
        let w = IOSurfaceGetWidth(src).min(IOSurfaceGetWidth(dst));
        let h = IOSurfaceGetHeight(src).min(IOSurfaceGetHeight(dst));
        let row = w * 4;
        for y in 0..h {
            std::ptr::copy_nonoverlapping(sb.add(y * ss), db.add(y * ds), row);
        }
        IOSurfaceUnlock(dst, IOSurfaceLockOptions(0), std::ptr::null_mut());
        IOSurfaceUnlock(src, IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;
    use std::time::{Duration, Instant};

    fn frame(w: usize, h: usize, bpr: usize, fill: &[u8; 4]) -> CaptureFrame {
        let mut bgra = vec![0u8; h * bpr];
        for y in 0..h {
            for x in 0..w {
                bgra[y * bpr + x * 4..y * bpr + x * 4 + 4].copy_from_slice(fill);
            }
        }
        CaptureFrame {
            w,
            h,
            bpr,
            bgra,
            id: 7,
            path: String::new(),
        }
    }

    /// The present thread must never pay for an encode. A frame offered while the encoder is
    /// still busy is dropped, and the offer returns immediately either way — this is the whole
    /// point of the split (the synchronous version cost 41% of the main thread at 2560x1440).
    #[test]
    fn an_offer_never_waits_on_the_encoder() {
        let (tx, rx) = sync_channel::<CaptureFrame>(1);
        // A big frame, so a caller that encoded inline could not possibly be quick.
        let big = || frame(2560, 1440, 2560 * 4, &[1, 2, 3, 4]);

        let t = Instant::now();
        assert!(offer_capture(&tx, big()), "first frame should be accepted");
        // The encoder has not run, so the slot is still full: the next frame is dropped.
        assert!(!offer_capture(&tx, big()), "second frame should be dropped");
        assert!(!offer_capture(&tx, big()), "third frame should be dropped");
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "offers took {:?} — the caller is doing the encode",
            t.elapsed()
        );

        // Draining lets the next frame through, so dropping is backpressure, not a wedge.
        drop(rx.recv().expect("queued frame"));
        assert!(offer_capture(&tx, big()));
    }

    /// A gone encoder is reported and dropped, not a panic on the present path.
    #[test]
    fn an_offer_to_a_dead_encoder_is_dropped() {
        let (tx, rx) = sync_channel::<CaptureFrame>(1);
        drop(rx);
        assert!(!offer_capture(&tx, frame(2, 2, 8, &[1, 2, 3, 4])));
    }

    /// The swizzle moved off the present thread, so pin what it produces: BGRA in memory comes
    /// back as RGBA, row padding is skipped, and alpha is forced opaque (honouring the
    /// scanout's "don't care" alpha is how a correct frame reads as a black capture).
    #[test]
    fn the_encoder_swizzles_to_rgba_and_forces_alpha() {
        let dir = std::env::temp_dir().join(format!("limina-capture-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap.png");

        // bpr deliberately wider than w*4: the tail bytes are padding and must not be read.
        let mut f = frame(2, 2, 16, &[0x10, 0x20, 0x30, 0x00]); // B=0x10 G=0x20 R=0x30 A=0
        f.path = path.to_string_lossy().into_owned();
        for y in 0..2 {
            f.bgra[y * 16 + 8..y * 16 + 16].copy_from_slice(&[0xff; 8]); // padding
        }
        encode_capture_png(&f);

        let dec = png::Decoder::new(std::fs::File::open(&path).unwrap());
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 2));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        for px in buf[..info.buffer_size()].as_chunks::<4>().0 {
            assert_eq!(px, &[0x30, 0x20, 0x10, 0xff], "swizzle or alpha wrong");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
