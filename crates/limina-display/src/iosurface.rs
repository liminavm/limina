// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! IOSurface-backed display backend — the window present path.
//!
//! Where [`crate::CaptureBackend`] writes frames to a PNG (the headless oracle), this
//! backend writes each guest scanout into a **shared IOSurface** that the limina supervisor
//! looks up by id and shows via `CALayer.contents` (decision D3: the UI lives in the
//! supervisor, the virtio-gpu device in the worker). The worker→supervisor channel is a
//! plain fd carrying a tiny line protocol:
//!
//! ```text
//! surface <id> <width> <height>   # a new scanout surface is available (look it up)
//! frame                            # a new frame was written (re-present)
//! ```
//!
//! The IOSurface is created `kIOSurfaceIsGlobal` so `IOSurfaceLookup(id)` works in the
//! supervisor (see spikes/m2-window/RESULTS.md; a Mach port is the future robust path).
#![allow(deprecated)] // objc2-io-surface 0.3 renamed the free fns to IOSurfaceRef methods.

use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, RawFd};
use std::ptr;

use objc2_core_foundation::{
    kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary,
    CFNumber, CFNumberType, CFRetained, CFString,
};
use objc2_io_surface::{
    kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfaceIsGlobal,
    kIOSurfacePixelFormat, kIOSurfaceWidth, IOSurfaceCreate, IOSurfaceGetBaseAddress,
    IOSurfaceGetBytesPerRow, IOSurfaceGetID, IOSurfaceLock, IOSurfaceLockOptions, IOSurfaceRef,
    IOSurfaceUnlock,
};

use krun_display::{
    DisplayBackend, DisplayBackendBasicFramebuffer, DisplayBackendError, DisplayBackendNew,
    IntoDisplayBackend, Rect, ResourceFormat,
};

/// Configuration for [`WindowBackend`]. `Sync` (libkrun's `DisplayBackendNew` bound) and
/// read once on the GPU worker thread.
#[derive(Clone, Debug)]
pub struct WindowConfig {
    /// fd of the worker→supervisor control channel. `-1` disables the protocol (the
    /// surface is still created; useful for standalone testing). The backend dups it.
    pub control_fd: RawFd,
}

/// A display backend that publishes guest scanouts as shared IOSurfaces.
pub struct WindowBackend {
    control: Option<File>,
    scanout: Option<Scanout>,
    next_frame_id: u32,
    presents: u64,
}

struct Scanout {
    width: u32,
    height: u32,
    format: ResourceFormat,
    /// Staging buffer libkrun fills (we then transform → BGRA into the IOSurface).
    staging: Vec<u8>,
    /// Double buffer: we write the back surface each present and tell the supervisor which
    /// id to show, so its `CALayer.contents` changes object identity → Core Animation
    /// actually re-reads (re-setting the *same* surface is a no-op and never refreshes).
    surfaces: [CFRetained<IOSurfaceRef>; 2],
    ids: [u32; 2],
    idx: usize,
    /// Force the next present to repaint the whole frame into both surfaces (fresh/reused
    /// surfaces have no prior content). Cleared after the first full present.
    needs_full: bool,
}

impl DisplayBackendNew<WindowConfig> for WindowBackend {
    fn new(userdata: Option<&WindowConfig>) -> Self {
        let control_fd = userdata.map(|c| c.control_fd).unwrap_or(-1);
        let control = if control_fd >= 0 {
            // Dup so our File owns its own fd and the worker keeps theirs.
            let dup = unsafe { libc::dup(control_fd) };
            if dup >= 0 {
                Some(unsafe { File::from_raw_fd(dup) })
            } else {
                log::error!("window: dup(control_fd) failed");
                None
            }
        } else {
            None
        };
        WindowBackend {
            control,
            scanout: None,
            next_frame_id: 0,
            presents: 0,
        }
    }
}

impl WindowBackend {
    fn send(&mut self, line: &str) {
        if let Some(f) = self.control.as_mut() {
            if let Err(e) = writeln!(f, "{line}") {
                log::error!("window: control write failed: {e}");
            }
        }
    }
}

impl DisplayBackendBasicFramebuffer for WindowBackend {
    fn configure_scanout(
        &mut self,
        scanout_id: u32,
        _display_width: u32,
        _display_height: u32,
        width: u32,
        height: u32,
        format: ResourceFormat,
    ) -> Result<(), DisplayBackendError> {
        if scanout_id != 0 {
            return Err(DisplayBackendError::InvalidScanoutId);
        }
        // Reuse the existing surfaces on a same-geometry remodeset. A real guest (Fedora:
        // simpledrm → plymouth → GDM) reconfigures the scanout many times at the same mode;
        // reallocating fresh global IOSurfaces each time would churn their ids and free
        // surfaces out from under the supervisor's pending lookups. Keeping them stable
        // makes the steady state a no-op and avoids that race.
        if let Some(s) = self.scanout.as_mut() {
            if s.width == width && s.height == height && s.format == format {
                // Same mode: keep the surfaces, but force the next present to repaint the
                // whole frame (the guest just re-declared the scanout; play it safe).
                s.needs_full = true;
                return Ok(());
            }
        }
        let len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(ResourceFormat::BYTES_PER_PIXEL))
            .ok_or(DisplayBackendError::InvalidParam)?;

        let s0 =
            create_global_iosurface(width, height).ok_or(DisplayBackendError::InternalError)?;
        let s1 =
            create_global_iosurface(width, height).ok_or(DisplayBackendError::InternalError)?;
        let ids = [IOSurfaceGetID(&s0), IOSurfaceGetID(&s1)];
        log::info!("window: scanout 0 -> IOSurfaces {ids:?} ({width}x{height} {format:?})");

        self.scanout = Some(Scanout {
            width,
            height,
            format,
            staging: vec![0u8; len],
            surfaces: [s0, s1],
            ids,
            idx: 0,
            needs_full: true,
        });
        self.send(&format!("surface {} {} {width} {height}", ids[0], ids[1]));
        Ok(())
    }

    fn disable_scanout(&mut self, scanout_id: u32) -> Result<(), DisplayBackendError> {
        if scanout_id != 0 {
            return Err(DisplayBackendError::InvalidScanoutId);
        }
        self.scanout = None;
        Ok(())
    }

    fn alloc_frame(&mut self, scanout_id: u32) -> Result<(u32, &mut [u8]), DisplayBackendError> {
        let scanout = self
            .scanout
            .as_mut()
            .filter(|_| scanout_id == 0)
            .ok_or(DisplayBackendError::InvalidScanoutId)?;
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        Ok((frame_id, &mut scanout.staging))
    }

    fn present_frame(
        &mut self,
        scanout_id: u32,
        _frame_id: u32,
        rect: Option<&Rect>,
    ) -> Result<(), DisplayBackendError> {
        if scanout_id != 0 {
            return Err(DisplayBackendError::InvalidScanoutId);
        }
        // Take the scanout out so we can write the surface and call &mut self.send().
        let mut scanout = self
            .scanout
            .take()
            .ok_or(DisplayBackendError::InvalidScanoutId)?;

        // Only swizzle the damaged region. EDK2's GOP flushes one glyph cell (~8×19 px) per
        // present; swizzling the whole 1280×800 frame each time made the firmware/GRUB
        // console crawl (thousands of full-frame transforms). `staging` always holds the
        // full current frame, so copying just the rect keeps every pixel correct while
        // touching ~150 px instead of ~1M. The first present after (re)configure must be
        // full — fresh/reused surfaces have no prior content for the untouched area.
        let (rx, ry, rw, rh) = if scanout.needs_full {
            (0, 0, scanout.width, scanout.height)
        } else {
            match rect {
                Some(r) => (r.x, r.y, r.width, r.height),
                None => (0, 0, scanout.width, scanout.height),
            }
        };
        scanout.needs_full = false;

        // Write the rect into BOTH surfaces. We only ever alternate which surface to *show*,
        // so the not-shown one must also receive each damage rect or it goes stale and the
        // next flip would resurrect an old region.
        scanout.idx ^= 1;
        let show_id = scanout.ids[scanout.idx];
        for surface in &scanout.surfaces {
            copy_rect_into_surface(
                surface,
                &scanout.staging,
                scanout.format,
                scanout.width,
                scanout.height,
                rx,
                ry,
                rw,
                rh,
            );
        }
        self.presents += 1;
        self.scanout = Some(scanout);
        self.send(&format!("frame {show_id}"));
        Ok(())
    }
}

/// Build a `DisplayBackend` for [`WindowBackend`] suitable for `VmResources`. The config
/// is tiny and lives for the whole VM, so it's intentionally leaked to `'static`.
pub fn window_backend(config: WindowConfig) -> DisplayBackend<'static> {
    let leaked: &'static WindowConfig = Box::leak(Box::new(config));
    WindowBackend::into_display_backend(Some(leaked))
}

/// Copy the damage rect of a guest scanout (`format`, `src_width`×`src_height`, full frame in
/// `src`) into the BGRA IOSurface, forcing opaque alpha (a guest desktop framebuffer is opaque;
/// `X` channels would otherwise read as transparent). The rect is clamped to the frame bounds,
/// so an out-of-range or empty rect is a no-op.
#[allow(clippy::too_many_arguments)]
fn copy_rect_into_surface(
    surface: &IOSurfaceRef,
    src: &[u8],
    format: ResourceFormat,
    src_width: u32,
    src_height: u32,
    rx: u32,
    ry: u32,
    rw: u32,
    rh: u32,
) {
    // Clamp [rx, rx+rw) × [ry, ry+rh) to [0, src_width) × [0, src_height).
    let x0 = rx.min(src_width) as usize;
    let y0 = ry.min(src_height) as usize;
    let x1 = rx.saturating_add(rw).min(src_width) as usize;
    let y1 = ry.saturating_add(rh).min(src_height) as usize;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let src_stride = src_width as usize * 4;
    unsafe {
        IOSurfaceLock(surface, IOSurfaceLockOptions(0), ptr::null_mut());
        let base = IOSurfaceGetBaseAddress(surface).as_ptr() as *mut u8;
        let dst_stride = IOSurfaceGetBytesPerRow(surface);
        for y in y0..y1 {
            let s = &src[y * src_stride..y * src_stride + src_stride];
            let d = base.add(y * dst_stride);
            for x in x0..x1 {
                let bgra = to_bgra(format, &s[x * 4..x * 4 + 4]);
                ptr::copy_nonoverlapping(bgra.as_ptr(), d.add(x * 4), 4);
            }
        }
        IOSurfaceUnlock(surface, IOSurfaceLockOptions(0), ptr::null_mut());
    }
}

/// Convert one pixel from `format`'s byte order to BGRA with opaque alpha.
#[inline]
fn to_bgra(format: ResourceFormat, p: &[u8]) -> [u8; 4] {
    // (b_idx, g_idx, r_idx) within the source pixel.
    let (b, g, r) = match format {
        ResourceFormat::BGRA | ResourceFormat::BGRX => (0, 1, 2),
        ResourceFormat::ARGB | ResourceFormat::XRGB => (3, 2, 1),
        ResourceFormat::RGBA | ResourceFormat::RGBX => (2, 1, 0),
        ResourceFormat::ABGR | ResourceFormat::XBGR => (1, 2, 3),
    };
    [p[b], p[g], p[r], 255]
}

/// Create a `width`×`height` BGRA IOSurface that other processes can `IOSurfaceLookup`.
fn create_global_iosurface(width: u32, height: u32) -> Option<CFRetained<IOSurfaceRef>> {
    let pixel_format = i32::from_be_bytes(*b"BGRA");
    let bytes_per_row = (width * 4) as i32;
    unsafe {
        let vw = cfnum(width as i32)?;
        let vh = cfnum(height as i32)?;
        let vbpe = cfnum(4)?;
        let vbpr = cfnum(bytes_per_row)?;
        let vpf = cfnum(pixel_format)?;
        let t = kCFBooleanTrue?;

        let mut keys: [*const c_void; 6] = [
            (kIOSurfaceWidth as *const CFString).cast(),
            (kIOSurfaceHeight as *const CFString).cast(),
            (kIOSurfaceBytesPerElement as *const CFString).cast(),
            (kIOSurfaceBytesPerRow as *const CFString).cast(),
            (kIOSurfacePixelFormat as *const CFString).cast(),
            (kIOSurfaceIsGlobal as *const CFString).cast(),
        ];
        let mut values: [*const c_void; 6] = [
            (&*vw as *const CFNumber).cast(),
            (&*vh as *const CFNumber).cast(),
            (&*vbpe as *const CFNumber).cast(),
            (&*vbpr as *const CFNumber).cast(),
            (&*vpf as *const CFNumber).cast(),
            (t as *const objc2_core_foundation::CFBoolean).cast(),
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

unsafe fn cfnum(v: i32) -> Option<CFRetained<CFNumber>> {
    CFNumber::new(
        None,
        CFNumberType::SInt32Type,
        &v as *const i32 as *const c_void,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_bgra_forces_opaque_and_orders_bgra() {
        // BGRX source [B,G,R,X] -> BGRA [B,G,R,255].
        assert_eq!(
            to_bgra(ResourceFormat::BGRX, &[10, 20, 30, 0]),
            [10, 20, 30, 255]
        );
        // XRGB source [X,R,G,B] -> BGRA [B,G,R,255] = [30,20,10,255] for R=10,G=20,B=30.
        assert_eq!(
            to_bgra(ResourceFormat::XRGB, &[0, 10, 20, 30]),
            [30, 20, 10, 255]
        );
    }

    #[test]
    fn creates_a_lookup_able_surface() {
        let s = create_global_iosurface(16, 8).expect("create");
        assert_ne!(IOSurfaceGetID(&s), 0);
    }

    // Read one BGRA pixel back out of a (locked) IOSurface.
    unsafe fn px(surface: &IOSurfaceRef, x: usize, y: usize) -> [u8; 4] {
        let base = IOSurfaceGetBaseAddress(surface).as_ptr() as *const u8;
        let stride = IOSurfaceGetBytesPerRow(surface);
        let p = base.add(y * stride + x * 4);
        [*p, *p.add(1), *p.add(2), *p.add(3)]
    }

    /// A staging frame where pixel (x,y) is BGRX = [x, y, tag, 0].
    fn staging(w: u32, h: u32, tag: u8) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                v[i] = x as u8;
                v[i + 1] = y as u8;
                v[i + 2] = tag;
            }
        }
        v
    }

    #[test]
    fn rect_copy_touches_only_the_damaged_region() {
        let (w, h) = (4u32, 4u32);
        let s = create_global_iosurface(w, h).expect("create");

        // Full copy with tag=100, then a rect (1,1,2,2) copy with tag=200. Outside the rect
        // must keep tag=100; inside must flip to tag=200 — proving the rect bound holds.
        copy_rect_into_surface(
            &s,
            &staging(w, h, 100),
            ResourceFormat::BGRX,
            w,
            h,
            0,
            0,
            w,
            h,
        );
        copy_rect_into_surface(
            &s,
            &staging(w, h, 200),
            ResourceFormat::BGRX,
            w,
            h,
            1,
            1,
            2,
            2,
        );

        unsafe {
            IOSurfaceLock(&s, IOSurfaceLockOptions(0), ptr::null_mut());
            // Corners are outside the rect → tag 100, opaque alpha, correct x/y.
            assert_eq!(px(&s, 0, 0), [0, 0, 100, 255]);
            assert_eq!(px(&s, 3, 3), [3, 3, 100, 255]);
            assert_eq!(px(&s, 3, 0), [3, 0, 100, 255]);
            // Inside [1,3)×[1,3) → tag 200.
            assert_eq!(px(&s, 1, 1), [1, 1, 200, 255]);
            assert_eq!(px(&s, 2, 2), [2, 2, 200, 255]);
            // Edge just outside the rect stays 100.
            assert_eq!(px(&s, 3, 1), [3, 1, 100, 255]);
            IOSurfaceUnlock(&s, IOSurfaceLockOptions(0), ptr::null_mut());
        }
    }

    #[test]
    fn rect_copy_clamps_out_of_bounds() {
        let (w, h) = (4u32, 4u32);
        let s = create_global_iosurface(w, h).expect("create");
        copy_rect_into_surface(
            &s,
            &staging(w, h, 50),
            ResourceFormat::BGRX,
            w,
            h,
            0,
            0,
            w,
            h,
        );
        // A rect that runs off the right/bottom edge must clamp, not panic or read OOB.
        copy_rect_into_surface(
            &s,
            &staging(w, h, 60),
            ResourceFormat::BGRX,
            w,
            h,
            3,
            3,
            99,
            99,
        );
        // A fully out-of-range rect is a no-op.
        copy_rect_into_surface(
            &s,
            &staging(w, h, 70),
            ResourceFormat::BGRX,
            w,
            h,
            99,
            99,
            4,
            4,
        );
        unsafe {
            IOSurfaceLock(&s, IOSurfaceLockOptions(0), ptr::null_mut());
            assert_eq!(px(&s, 3, 3), [3, 3, 60, 255]); // clamped rect reached the last pixel
            assert_eq!(px(&s, 0, 0), [0, 0, 50, 255]); // untouched by either later copy
            IOSurfaceUnlock(&s, IOSurfaceLockOptions(0), ptr::null_mut());
        }
    }
}
