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
//! surface <id> <id1> <width> <height>   # a new scanout ring is available (id resolved via Mach)
//! frame <id>                            # the surface id to show now
//! cursor <id> <w> <h> <hot_x> <hot_y>   # a new cursor image
//! ```
//!
//! Each scanout/cursor IOSurface is created NON-global and handed to the supervisor over a
//! Mach port (`limina-surfaceport`), keyed by its `IOSurfaceGetID` — so the supervisor resolves
//! the ids in the line protocol without a global `IOSurfaceLookup`, and no stranger process can
//! read the guest screen (`kIOSurfaceIsGlobal` is "insecure"; see spikes/iosurface-machport).
//! Set `LIMINA_GLOBAL_SCANOUT=1` to ALSO mark them global (so `iosdump` works as a debug oracle);
//! with no receiver configured we fall back to global so the supervisor can still look them up.
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
    IOSurfaceGetBytesPerRow, IOSurfaceGetHeight, IOSurfaceGetID, IOSurfaceGetWidth, IOSurfaceLock,
    IOSurfaceLockOptions, IOSurfaceLookup, IOSurfaceRef, IOSurfaceUnlock,
};

use krun_display::{
    DisplayBackend, DisplayBackendBasicFramebuffer, DisplayBackendError, DisplayBackendNew,
    IntoDisplayBackend, Rect, ResourceFormat,
};
use limina_surfaceport::SurfacePortSender;

/// Configuration for [`WindowBackend`]. `Sync` (libkrun's `DisplayBackendNew` bound) and
/// read once on the GPU worker thread.
#[derive(Clone, Debug)]
pub struct WindowConfig {
    /// fd of the worker→supervisor control channel. `-1` disables the protocol (the
    /// surface is still created; useful for standalone testing). The backend dups it.
    pub control_fd: RawFd,
    /// Bootstrap name of the supervisor's surface-port receiver. When set, each scanout/cursor
    /// IOSurface is created NON-global and its Mach port is sent to the supervisor over this
    /// channel (so strangers can't `IOSurfaceLookup` the guest screen). `None` ⇒ legacy global
    /// surfaces (standalone testing with no supervisor receiver).
    pub surface_port_name: Option<String>,
}

/// Depth of the surface ring. Must be ≥ 2 (we alternate ids so Core Animation re-reads). 3
/// gives ~50 ms between reuses of a given surface at 60 fps — comfortably past the window
/// server's composite-hold (~16–33 ms), which is what kills the residual flicker. Bump it if
/// a guest ever presents well above the display refresh.
const SURFACE_RING: usize = 3;

/// A display backend that publishes guest scanouts as shared IOSurfaces.
pub struct WindowBackend {
    control: Option<File>,
    scanout: Option<Scanout>,
    next_frame_id: u32,
    presents: u64,
    /// The current hardware-cursor IOSurface (kept retained so the supervisor can look it up
    /// before we replace it on the next shape change). `None` when the cursor is hidden.
    cursor: Option<CFRetained<IOSurfaceRef>>,
    /// Mach-port channel to the supervisor: each scanout/cursor surface is handed over by its
    /// (opaque, non-resolvable) `IOSurfaceGetID` so the supervisor resolves ids without
    /// `IOSurfaceLookup`. `None` ⇒ legacy global surfaces (no receiver name configured).
    sender: Option<SurfacePortSender>,
    /// Also mark surfaces `kIOSurfaceIsGlobal` (debug escape hatch `LIMINA_GLOBAL_SCANOUT`, so
    /// `iosdump` still works as an oracle). Read once at construction. Default off = secure.
    also_global: bool,
}

struct Scanout {
    width: u32,
    height: u32,
    format: ResourceFormat,
    /// Staging buffer libkrun fills (guest pixel format; full current frame).
    staging: Vec<u8>,
    /// CPU-side BGRA mirror of the frame, kept current by cheap per-rect swizzles. It is the
    /// source of truth we memcpy (no swizzle) into whichever back surface we're about to show,
    /// so each surface is always a complete, self-consistent frame — and we never write the
    /// on-screen one (writing both surfaces per present raced the compositor → flicker).
    canvas: Vec<u8>,
    /// Ring of surfaces: each present writes the next one and tells the supervisor which id
    /// to show, so its `CALayer.contents` changes object identity → Core Animation re-reads
    /// (re-setting the *same* surface is a no-op and never refreshes). The ring is deeper than
    /// a double buffer on purpose: the supervisor's 60 Hz timer latches a surface and the
    /// window server then samples it for ~1–2 composites (~16–33 ms), entirely decoupled from
    /// us. With only two buffers we'd cycle back and overwrite a surface still being composited
    /// (`IOSurfaceLock` is advisory — it doesn't block the compositor's read) → flicker. A
    /// [`SURFACE_RING`]-deep ring keeps a surface untouched long enough for the composite to
    /// finish before we reuse it.
    surfaces: Vec<CFRetained<IOSurfaceRef>>,
    ids: Vec<u32>,
    idx: usize,
    /// Force the next present to swizzle the whole frame into the canvas (a fresh canvas has
    /// no prior content for the area outside the damage rect). Cleared after the first full
    /// present.
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
        let also_global = std::env::var_os("LIMINA_GLOBAL_SCANOUT").is_some();
        let sender = userdata
            .and_then(|c| c.surface_port_name.as_deref())
            .and_then(|name| match SurfacePortSender::lookup(name) {
                Ok(s) => {
                    log::info!("window: scanout surfaces scoped via Mach port (name {name})");
                    Some(s)
                }
                Err(e) => {
                    // Don't fail the VM over this; fall back to global surfaces (degraded security).
                    log::error!("window: surface-port lookup failed ({e}); using GLOBAL surfaces");
                    None
                }
            });
        if sender.is_none() && !also_global {
            log::warn!("window: no surface-port receiver — scanout IOSurfaces will be GLOBAL");
        }
        WindowBackend {
            control,
            scanout: None,
            next_frame_id: 0,
            presents: 0,
            cursor: None,
            // No receiver ⇒ surfaces must stay global or the supervisor can't resolve them.
            also_global: also_global || sender.is_none(),
            sender,
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

    /// Hand a freshly-created surface to the supervisor over the Mach channel, keyed by its id.
    /// No-op when there's no receiver (legacy global path — the supervisor `IOSurfaceLookup`s it).
    fn publish(&self, id: u32, surface: &IOSurfaceRef) {
        if let Some(tx) = self.sender.as_ref() {
            if let Err(e) = tx.send(id, surface) {
                log::error!("window: surface-port send(id={id}) failed: {e}");
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

        let mut surfaces = Vec::with_capacity(SURFACE_RING);
        for _ in 0..SURFACE_RING {
            surfaces.push(
                create_scanout_iosurface(width, height, self.also_global)
                    .ok_or(DisplayBackendError::InternalError)?,
            );
        }
        let ids: Vec<u32> = surfaces.iter().map(|s| IOSurfaceGetID(s)).collect();
        log::info!("window: scanout 0 -> IOSurfaces {ids:?} ({width}x{height} {format:?})");
        // Hand each ring surface to the supervisor over the Mach channel, keyed by its id, BEFORE
        // the `surface` line announces them — so the supervisor can resolve the ids without a
        // global IOSurfaceLookup.
        for (id, surface) in ids.iter().zip(surfaces.iter()) {
            self.publish(*id, surface);
        }

        // The supervisor looks surfaces up lazily by id (one IOSurfaceLookup per `frame <id>`),
        // so the protocol still only needs to name the initial buffer; the rest are discovered
        // as they're shown. Keep the two-id shape for wire compatibility (id1 is ignored).
        let id1 = ids.get(1).copied().unwrap_or(ids[0]);
        self.scanout = Some(Scanout {
            width,
            height,
            format,
            staging: vec![0u8; len],
            canvas: vec![0u8; len],
            surfaces,
            ids,
            idx: 0,
            needs_full: true,
        });
        let id0 = self.scanout.as_ref().unwrap().ids[0];
        self.send(&format!("surface {id0} {id1} {width} {height}"));
        Ok(())
    }

    fn disable_scanout(&mut self, scanout_id: u32) -> Result<(), DisplayBackendError> {
        if scanout_id != 0 {
            return Err(DisplayBackendError::InvalidScanoutId);
        }
        self.scanout = None;
        Ok(())
    }

    /// limina tier-2: zero-copy present of a venus-rendered global IOSurface (SET_SCANOUT_BLOB).
    /// The renderer (vkr) already drew straight into this IOSurface, so we just hand its global
    /// id to the supervisor — which `IOSurfaceLookup`s any id lazily (window.rs) — with no
    /// alloc_frame/swizzle/copy. `configure_scanout` (called by set_scanout_blob) already sized
    /// the window and announced the ring; this overrides which surface to show this frame.
    fn present_surface(
        &mut self,
        scanout_id: u32,
        iosurface_id: u32,
        _rect: Option<&Rect>,
    ) -> Result<(), DisplayBackendError> {
        if scanout_id != 0 {
            return Err(DisplayBackendError::InvalidScanoutId);
        }
        self.presents += 1;
        red_probe(iosurface_id, self.presents);
        self.send(&format!("frame {iosurface_id}"));
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

        // Only swizzle the damaged region into the BGRA canvas. EDK2's GOP flushes one glyph
        // cell (~8×19 px) per present; swizzling the whole 1280×800 frame each time made the
        // firmware/GRUB console crawl (thousands of full-frame transforms). `staging` always
        // holds the full current frame, so swizzling just the rect keeps the canvas correct
        // while touching ~150 px instead of ~1M. The first present after (re)configure must
        // be full — a fresh canvas has no prior content for the untouched area.
        let (rx, ry, rw, rh) = if scanout.needs_full {
            (0, 0, scanout.width, scanout.height)
        } else {
            match rect {
                Some(r) => (r.x, r.y, r.width, r.height),
                None => (0, 0, scanout.width, scanout.height),
            }
        };
        scanout.needs_full = false;
        let (format, width, height) = (scanout.format, scanout.width, scanout.height);
        // Disjoint-field borrow: canvas (&mut) and staging (&) are different fields.
        swizzle_rect_into_canvas(
            &mut scanout.canvas,
            &scanout.staging,
            format,
            width,
            rx,
            ry,
            rw,
            rh,
        );

        // Advance the ring, then memcpy the FULL canvas into the next surface (a plain copy,
        // no swizzle — fast). We only ever write the surface we're about to show; by the time
        // the ring wraps back to it, the supervisor + window server are long done compositing
        // it, so the compositor never samples a half-written surface.
        scanout.idx = (scanout.idx + 1) % scanout.surfaces.len();
        let i = scanout.idx;
        copy_canvas_into_surface(&scanout.surfaces[i], &scanout.canvas, width, height);
        let show_id = scanout.ids[i];
        self.presents += 1;
        self.scanout = Some(scanout);
        self.send(&format!("frame {show_id}"));
        Ok(())
    }

    fn set_cursor(
        &mut self,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        format: ResourceFormat,
        buffer: &[u8],
    ) -> Result<(), DisplayBackendError> {
        // A zero-size image (or too-small buffer) means hide the cursor.
        let need = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4));
        let Some(need) = need.filter(|&n| n > 0 && buffer.len() >= n) else {
            self.cursor = None;
            self.send("cursorhide");
            return Ok(());
        };

        // Publish the cursor image as its own IOSurface (the supervisor shows it in an overlay
        // layer, never the scanout — so cursor motion never touches the present path). Like the
        // scanout ring it's NON-global + Mach-handed unless the escape hatch is on.
        let surface = create_scanout_iosurface(width, height, self.also_global)
            .ok_or(DisplayBackendError::InternalError)?;
        let id = IOSurfaceGetID(&surface);
        self.publish(id, &surface);
        // Swizzle the cursor pixels into BGRA, PRESERVING alpha (unlike the scanout path,
        // which forces opaque): the cursor image is mostly transparent surround.
        let mut canvas = vec![0u8; need];
        for (dst, src) in canvas.chunks_exact_mut(4).zip(buffer.chunks_exact(4)) {
            dst.copy_from_slice(&to_bgra_keep_alpha(format, src));
        }
        copy_canvas_into_surface(&surface, &canvas, width, height);

        self.cursor = Some(surface);
        self.send(&format!("cursor {id} {width} {height} {hot_x} {hot_y}"));
        Ok(())
    }

    fn move_cursor(&mut self, x: u32, y: u32) -> Result<(), DisplayBackendError> {
        // The virtio wire field is u32, but a cursor whose hotspot hangs past the scanout's
        // left/top edge is legitimately negative — the guest kernel casts (e.g. -2 arrives as
        // 4294967294). Recover the signed value before forwarding, so the supervisor draws the
        // sprite partially off-edge instead of dropping the position.
        self.send(&format!("cursormove {} {}", x as i32, y as i32));
        Ok(())
    }
}

/// Flicker oracle (`LIMINA_RED_PROBE=1`): with the guest desktop set to solid red and the
/// workload windowed on top of it, the wallpaper visible AROUND the window contributes a
/// steady baseline of red samples — and a frame where the compositor drops the window's
/// content (the flicker artifact) shows red INSIDE the window region too, spiking the count
/// above baseline. The probe tracks the baseline as a slowly-rising running minimum (so it
/// follows window moves/resizes) and logs frames whose red count jumps above it; the
/// adjacent `[FLUSHDBG]` line identifies which guest resource carried the bad frame.
/// Detection-only, ≤64×64 samples/frame, env-gated; scanout format is BGRA.
fn red_probe(iosurface_id: u32, frame: u64) {
    use std::sync::atomic::{AtomicI8, AtomicU32, Ordering};
    static ENABLED: AtomicI8 = AtomicI8::new(-1);
    /// Running minimum of the per-frame red count (the "window fully drawn" state),
    /// decayed upward by +1/frame so it re-learns after the window moves or shrinks.
    static BASELINE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut on = ENABLED.load(Ordering::Relaxed);
    if on < 0 {
        on = std::env::var_os("LIMINA_RED_PROBE").is_some() as i8;
        ENABLED.store(on, Ordering::Relaxed);
    }
    if on == 0 {
        return;
    }
    let Some(surface) = IOSurfaceLookup(iosurface_id) else {
        return;
    };
    unsafe {
        let opts = IOSurfaceLockOptions::ReadOnly;
        IOSurfaceLock(&surface, opts, ptr::null_mut());
        let base = IOSurfaceGetBaseAddress(&surface).as_ptr() as *const u8;
        let stride = IOSurfaceGetBytesPerRow(&surface);
        let w = IOSurfaceGetWidth(&surface);
        let h = IOSurfaceGetHeight(&surface);
        let step_x = (w / 64).max(1);
        let step_y = (h / 64).max(1);
        let mut red = 0u32;
        let mut samples = 0u32;
        let mut y = 0;
        while y < h {
            let mut x = 0;
            while x < w {
                let p = base.add(y * stride + x * 4);
                let (b, g, r) = (*p, *p.add(1), *p.add(2));
                if r > 200 && g < 60 && b < 60 {
                    red += 1;
                }
                samples += 1;
                x += step_x;
            }
            y += step_y;
        }
        IOSurfaceUnlock(&surface, opts, ptr::null_mut());
        let baseline = BASELINE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                Some(red.min(b.saturating_add(1)))
            })
            .unwrap_or(u32::MAX);
        let threshold = baseline.saturating_add((baseline / 10).max(15));
        if baseline != u32::MAX && red > threshold {
            log::warn!(
                "[REDPROBE] frame={frame} iosurface={iosurface_id} red={red} baseline={baseline} ({samples} samples)"
            );
        }
    }
}

/// Build a `DisplayBackend` for [`WindowBackend`] suitable for `VmResources`. The config
/// is tiny and lives for the whole VM, so it's intentionally leaked to `'static`.
pub fn window_backend(config: WindowConfig) -> DisplayBackend<'static> {
    let leaked: &'static WindowConfig = Box::leak(Box::new(config));
    WindowBackend::into_display_backend(Some(leaked))
}

/// Swizzle just the damage rect of the guest frame (`src`, `format`, `src_width` wide) into the
/// BGRA `canvas` (same dims, tight `src_width*4` stride), forcing opaque alpha (a guest desktop
/// framebuffer is opaque; `X` channels would otherwise read as transparent). The rect is clamped
/// to the frame bounds, so an out-of-range or empty rect is a no-op. This is the only swizzle on
/// the hot path; keeping it rect-sized is what makes the GOP console fast.
#[allow(clippy::too_many_arguments)]
fn swizzle_rect_into_canvas(
    canvas: &mut [u8],
    src: &[u8],
    format: ResourceFormat,
    src_width: u32,
    rx: u32,
    ry: u32,
    rw: u32,
    rh: u32,
) {
    let stride = src_width as usize * 4;
    let height = (canvas.len() / stride.max(1)) as u32;
    // Clamp [rx, rx+rw) × [ry, ry+rh) to [0, src_width) × [0, height).
    let x0 = rx.min(src_width) as usize;
    let y0 = ry.min(height) as usize;
    let x1 = rx.saturating_add(rw).min(src_width) as usize;
    let y1 = ry.saturating_add(rh).min(height) as usize;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for y in y0..y1 {
        let row = y * stride;
        for x in x0..x1 {
            let o = row + x * 4;
            let bgra = to_bgra(format, &src[o..o + 4]);
            canvas[o..o + 4].copy_from_slice(&bgra);
        }
    }
}

/// Copy the full BGRA `canvas` (`width`×`height`, tight stride) into the IOSurface, honoring the
/// surface's own row stride. No swizzle — a straight per-row memcpy, so it's cheap even at full
/// frame, which lets us refresh a stale back buffer without re-swizzling.
fn copy_canvas_into_surface(surface: &IOSurfaceRef, canvas: &[u8], width: u32, height: u32) {
    let src_stride = width as usize * 4;
    unsafe {
        IOSurfaceLock(surface, IOSurfaceLockOptions(0), ptr::null_mut());
        let base = IOSurfaceGetBaseAddress(surface).as_ptr() as *mut u8;
        let dst_stride = IOSurfaceGetBytesPerRow(surface);
        for y in 0..height as usize {
            let s = &canvas[y * src_stride..y * src_stride + src_stride];
            ptr::copy_nonoverlapping(s.as_ptr(), base.add(y * dst_stride), src_stride);
        }
        IOSurfaceUnlock(surface, IOSurfaceLockOptions(0), ptr::null_mut());
    }
}

/// Convert one pixel from `format`'s byte order to BGRA, preserving the source alpha
/// (X formats carry none → opaque). The cursor overlay needs this: its transparent
/// surround flattened to opaque renders as a black rectangle riding with the cursor.
#[inline]
fn to_bgra_keep_alpha(format: ResourceFormat, p: &[u8]) -> [u8; 4] {
    let a = match format {
        ResourceFormat::BGRA | ResourceFormat::RGBA => p[3],
        ResourceFormat::ARGB | ResourceFormat::ABGR => p[0],
        _ => 255,
    };
    let [b, g, r, _] = to_bgra(format, p);
    [b, g, r, a]
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

/// Create a `width`×`height` BGRA scanout IOSurface. When `also_global` is set it is marked
/// `kIOSurfaceIsGlobal` so any process can `IOSurfaceLookup` it by id (the debug escape hatch,
/// and the fallback when there's no Mach-port receiver); otherwise it is NON-global and only the
/// supervisor — to which we hand its Mach port — can resolve it (the secure default).
fn create_scanout_iosurface(
    width: u32,
    height: u32,
    also_global: bool,
) -> Option<CFRetained<IOSurfaceRef>> {
    let pixel_format = i32::from_be_bytes(*b"BGRA");
    // Align the row stride to 256 bytes. A tight `width*4` stride is accepted by IOSurfaceCreate,
    // but CoreAnimation cannot sample the surface as a layer's contents unless the stride meets the
    // GPU's row alignment (64 B on Apple Silicon) — an unaligned surface composites BLANK. With the
    // fixed boot widths (1280/1024/640, all multiples of 16 → width*4 64-aligned) this never showed;
    // runtime resize to an arbitrary width (e.g. 1068 → 1068*4=4272, not 64-aligned) blanked the
    // window. Over-align to 256 (a safe superset of the requirement). `copy_canvas_to_surface`
    // honors the surface's real `bytesPerRow`, so the padded tail of each row is handled correctly.
    let bytes_per_row = (((width * 4) + 255) & !255) as i32;
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

        // IsGlobal is the last key; drop it from the count to create a NON-global surface.
        let count = if also_global { 6 } else { 5 };
        let dict = CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            count as isize,
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
        let s = create_scanout_iosurface(16, 8, true).expect("create");
        assert_ne!(IOSurfaceGetID(&s), 0);
    }

    /// Stranger role: when `LIMINA_LOOKUP_ID` is set, this process exists only to probe whether
    /// that IOSurface id is `IOSurfaceLookup`-able, exiting 0 (found/exposed) or 1 (hidden). The
    /// sibling test below re-execs the test binary in this role so the probe is a genuinely
    /// unrelated process (a non-global surface is resolvable by its *holder*, so an in-process
    /// check would be meaningless).
    #[test]
    fn stranger_lookup_role() {
        let Ok(id) = std::env::var("LIMINA_LOOKUP_ID") else {
            return; // normal run: this role is a no-op
        };
        let id: u32 = id.parse().expect("LIMINA_LOOKUP_ID");
        let found = IOSurfaceLookup(id).is_some();
        std::process::exit(i32::from(!found));
    }

    #[test]
    fn non_global_scanout_is_hidden_from_strangers() {
        // The capability-scoping guarantee: a scanout surface created the secure way (non-global)
        // cannot be read by an unrelated process via IOSurfaceLookup, while a global one can.
        let ng = create_scanout_iosurface(64, 8, false).expect("create non-global");
        let g = create_scanout_iosurface(64, 8, true).expect("create global");
        let ng_id = IOSurfaceGetID(&ng);
        let g_id = IOSurfaceGetID(&g);

        let exe = std::env::current_exe().expect("test exe");
        let probe = |id: u32| -> bool {
            std::process::Command::new(&exe)
                .args(["--exact", "iosurface::tests::stranger_lookup_role"])
                .env("LIMINA_LOOKUP_ID", id.to_string())
                .status()
                .expect("spawn stranger")
                .success()
        };
        // `ng`/`g` stay alive in THIS process for the duration of both probes.
        assert!(
            probe(g_id),
            "a global surface should be readable by a stranger (control)"
        );
        assert!(
            !probe(ng_id),
            "a NON-global scanout must be hidden from a stranger process"
        );
        drop((ng, g));
    }

    #[test]
    fn scanout_surface_row_stride_is_gpu_aligned() {
        // CoreAnimation cannot sample a scanout surface whose row stride isn't GPU-aligned (64 B on
        // Apple Silicon); a tight `width*4` stride composites BLANK for any width that isn't a
        // multiple of 16. The fixed boot widths (1280/1024/640) are all multiples of 16 and hid
        // this; runtime resize to an arbitrary width exposed it (e.g. 1066 → 1066*4 = 4264, not
        // 64-aligned). Every created surface must carry an aligned stride no smaller than width*4.
        for w in [1066u32, 1068, 793, 917, 1, 17, 1280, 640] {
            let s = create_scanout_iosurface(w, 4, true).expect("create");
            let bpr = IOSurfaceGetBytesPerRow(&s);
            assert!(bpr >= (w * 4) as usize, "w={w}: bpr {bpr} < width*4");
            assert_eq!(bpr % 64, 0, "w={w}: bpr {bpr} not 64-byte aligned");
        }
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

    // Read one BGRA pixel out of a tight-stride canvas.
    fn cpx(canvas: &[u8], w: u32, x: usize, y: usize) -> [u8; 4] {
        let o = (y * w as usize + x) * 4;
        [canvas[o], canvas[o + 1], canvas[o + 2], canvas[o + 3]]
    }

    #[test]
    fn rect_swizzle_touches_only_the_damaged_region() {
        let (w, h) = (4u32, 4u32);
        let mut canvas = vec![0u8; (w * h * 4) as usize];

        // Full swizzle with tag=100, then a rect (1,1,2,2) swizzle with tag=200. Outside the
        // rect must keep tag=100; inside must flip to tag=200 — proving the rect bound holds.
        swizzle_rect_into_canvas(
            &mut canvas,
            &staging(w, h, 100),
            ResourceFormat::BGRX,
            w,
            0,
            0,
            w,
            h,
        );
        swizzle_rect_into_canvas(
            &mut canvas,
            &staging(w, h, 200),
            ResourceFormat::BGRX,
            w,
            1,
            1,
            2,
            2,
        );

        // Corners are outside the rect → tag 100, opaque alpha, correct x/y.
        assert_eq!(cpx(&canvas, w, 0, 0), [0, 0, 100, 255]);
        assert_eq!(cpx(&canvas, w, 3, 3), [3, 3, 100, 255]);
        assert_eq!(cpx(&canvas, w, 3, 0), [3, 0, 100, 255]);
        // Inside [1,3)×[1,3) → tag 200.
        assert_eq!(cpx(&canvas, w, 1, 1), [1, 1, 200, 255]);
        assert_eq!(cpx(&canvas, w, 2, 2), [2, 2, 200, 255]);
        // Edge just outside the rect stays 100.
        assert_eq!(cpx(&canvas, w, 3, 1), [3, 1, 100, 255]);
    }

    #[test]
    fn rect_swizzle_clamps_out_of_bounds() {
        let (w, h) = (4u32, 4u32);
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        swizzle_rect_into_canvas(
            &mut canvas,
            &staging(w, h, 50),
            ResourceFormat::BGRX,
            w,
            0,
            0,
            w,
            h,
        );
        // A rect that runs off the right/bottom edge must clamp, not panic or read OOB.
        swizzle_rect_into_canvas(
            &mut canvas,
            &staging(w, h, 60),
            ResourceFormat::BGRX,
            w,
            3,
            3,
            99,
            99,
        );
        // A fully out-of-range rect is a no-op.
        swizzle_rect_into_canvas(
            &mut canvas,
            &staging(w, h, 70),
            ResourceFormat::BGRX,
            w,
            99,
            99,
            4,
            4,
        );
        assert_eq!(cpx(&canvas, w, 3, 3), [3, 3, 60, 255]); // clamped rect reached the last pixel
        assert_eq!(cpx(&canvas, w, 0, 0), [0, 0, 50, 255]); // untouched by either later swizzle
    }

    #[test]
    fn set_cursor_preserves_source_alpha() {
        let mut b = WindowBackend::new(None);

        // A 2×2 BGRA cursor: one opaque white pixel at (0,0), transparent black elsewhere —
        // the shape of a real GNOME cursor image. The transparent surround must stay
        // transparent in the published surface; flattening it to opaque puts a black
        // rectangle around the cursor on screen (seen live on the venus desktop).
        let data = [
            255, 255, 255, 255, /* (1,0) */ 0, 0, 0, 0, //
            /* (0,1) */ 0, 0, 0, 0, /* (1,1) */ 0, 0, 0, 0,
        ];
        b.set_cursor(2, 2, 0, 0, ResourceFormat::BGRA, &data)
            .unwrap();
        let surf = b.cursor.as_ref().expect("cursor surface published");
        unsafe {
            IOSurfaceLock(surf, IOSurfaceLockOptions(0), ptr::null_mut());
            assert_eq!(px(surf, 0, 0), [255, 255, 255, 255]);
            assert_eq!(px(surf, 1, 1), [0, 0, 0, 0]);
            IOSurfaceUnlock(surf, IOSurfaceLockOptions(0), ptr::null_mut());
        }
    }

    #[test]
    fn set_cursor_publishes_surface_then_hides() {
        let mut b = WindowBackend::new(None); // control_fd -1 → no channel, surface still made
        assert!(b.cursor.is_none());

        // A 2×2 BGRX cursor: pixel (x,y) = [x, y, 9, 0].
        let data = staging(2, 2, 9);
        b.set_cursor(2, 2, 1, 1, ResourceFormat::BGRX, &data)
            .unwrap();
        let surf = b.cursor.as_ref().expect("cursor surface published");
        unsafe {
            IOSurfaceLock(surf, IOSurfaceLockOptions(0), ptr::null_mut());
            assert_eq!(px(surf, 0, 0), [0, 0, 9, 255]);
            assert_eq!(px(surf, 1, 1), [1, 1, 9, 255]); // opaque alpha forced
            IOSurfaceUnlock(surf, IOSurfaceLockOptions(0), ptr::null_mut());
        }

        // Zero size hides the cursor (and an empty buffer must not panic).
        b.set_cursor(0, 0, 0, 0, ResourceFormat::BGRX, &[]).unwrap();
        assert!(b.cursor.is_none());
        // A too-small buffer is also treated as hide, not a panic.
        b.set_cursor(4, 4, 0, 0, ResourceFormat::BGRX, &[0, 0, 0, 0])
            .unwrap();
        assert!(b.cursor.is_none());
    }

    #[test]
    fn canvas_copies_whole_frame_into_surface() {
        // The full-frame memcpy must land every pixel through the surface's own row stride.
        let (w, h) = (16u32, 8u32);
        let s = create_scanout_iosurface(w, h, true).expect("create");
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        swizzle_rect_into_canvas(
            &mut canvas,
            &staging(w, h, 77),
            ResourceFormat::BGRX,
            w,
            0,
            0,
            w,
            h,
        );
        copy_canvas_into_surface(&s, &canvas, w, h);
        unsafe {
            IOSurfaceLock(&s, IOSurfaceLockOptions(0), ptr::null_mut());
            assert_eq!(px(&s, 0, 0), [0, 0, 77, 255]);
            assert_eq!(px(&s, 15, 7), [15, 7, 77, 255]);
            assert_eq!(px(&s, 9, 3), [9, 3, 77, 255]);
            IOSurfaceUnlock(&s, IOSurfaceLockOptions(0), ptr::null_mut());
        }
    }
}
