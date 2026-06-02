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
            height,
            format,
            staging: vec![0u8; len],
            surfaces: [s0, s1],
            ids,
            idx: 0,
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
        _rect: Option<&Rect>,
    ) -> Result<(), DisplayBackendError> {
        if scanout_id != 0 {
            return Err(DisplayBackendError::InvalidScanoutId);
        }
        // Take the scanout out so we can write the surface and call &mut self.send().
        let mut scanout = self
            .scanout
            .take()
            .ok_or(DisplayBackendError::InvalidScanoutId)?;
        // Write the *back* buffer, then publish it as the one to show.
        scanout.idx ^= 1;
        let i = scanout.idx;
        copy_into_surface(
            &scanout.surfaces[i],
            &scanout.staging,
            scanout.format,
            scanout.height,
        );
        let show_id = scanout.ids[i];
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

/// Copy a guest scanout (`format`) into the BGRA IOSurface, forcing opaque alpha (a guest
/// desktop framebuffer is opaque; `X` channels would otherwise read as transparent).
fn copy_into_surface(surface: &IOSurfaceRef, src: &[u8], format: ResourceFormat, height: u32) {
    unsafe {
        IOSurfaceLock(surface, IOSurfaceLockOptions(0), ptr::null_mut());
        let base = IOSurfaceGetBaseAddress(surface).as_ptr() as *mut u8;
        let dst_stride = IOSurfaceGetBytesPerRow(surface);
        let src_stride = src.len() / (height.max(1) as usize);
        let row_px = src_stride / 4;
        for y in 0..height as usize {
            let s = &src[y * src_stride..y * src_stride + src_stride];
            let d = base.add(y * dst_stride);
            for x in 0..row_px {
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
}
