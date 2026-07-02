// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Present-path diagnostics: the LIMINA_WINDOW_CAPTURE / LIMINA_CAPTURE_IDS IOSurface-dump
//! oracles (pixel truth without Screen Recording permission) and the LIMINA_PRESENT_COPY /
//! LIMINA_PRESENT_LOCK present-race probes (see the arming comments in [`super::run`]).

use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber,
    CFNumberType, CFRetained, CFString,
};
use objc2_io_surface::{
    kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat,
    kIOSurfaceWidth, IOSurfaceCreate, IOSurfaceGetBaseAddress, IOSurfaceGetBytesPerRow,
    IOSurfaceGetHeight, IOSurfaceGetWidth, IOSurfaceLock, IOSurfaceLockOptions, IOSurfaceRef,
    IOSurfaceUnlock,
};

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

/// GPU-coherent diagnostic capture: lock the IOSurface (which syncs against the GPU), read its
/// BGRA bytes directly, and write a PNG with alpha forced opaque. Needs no Screen Recording
/// permission. (A premultiplied `CALayer.renderInContext` would zero the RGB wherever the guest's
/// "don't care" scanout alpha is 0; reading the surface directly shows the true scanout content.)
pub(crate) fn capture_iosurface(surface: &IOSurfaceRef, id: u32, path: &str) {
    use objc2_io_surface::IOSurfaceLockOptions;
    unsafe {
        // ReadOnly + default (no AvoidSync) → waits for the GPU to finish writing.
        if surface.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut()) != 0 {
            log::error!("capture: IOSurfaceLock failed");
            return;
        }
        let w = surface.width();
        let h = surface.height();
        let bpr = surface.bytes_per_row();
        let base = surface.base_address().as_ptr() as *const u8;
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            let row = base.add(y * bpr);
            for x in 0..w {
                let px = row.add(x * 4); // BGRA in memory
                let b = *px;
                let g = *px.add(1);
                let r = *px.add(2);
                let o = (y * w + x) * 4;
                rgba[o] = r;
                rgba[o + 1] = g;
                rgba[o + 2] = b;
                rgba[o + 3] = 255; // force opaque — the scanout alpha is "don't care"
            }
        }
        let _ = surface.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());

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
                            .chunks_exact(4)
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
