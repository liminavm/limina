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
//! surface <id> <id1> <width> <height> [<scanout>]  # a new scanout ring (id resolved via Mach)
//! frame <id> [<scanout>]                           # the surface id to show now
//! cursor <id> <w> <h> <hot_x> <hot_y> [<scanout>] # a new cursor image
//! cursormove <x> <y> [<scanout>]                   # where to draw it
//! cursorhide [<scanout>]                           # that scanout has no cursor
//! ```
//!
//! The trailing `<scanout>` names the pool slot the line belongs to — a guest with several
//! connectors presents into each independently, and the supervisor routes each slot to its own
//! window. It is optional on the wire and absent means slot 0, so a single-display log reads
//! exactly as it always did.
//!
//! The cursor lines carry it too, and there it is not a nicety: a guest enables its hardware
//! cursor plane on only the CRTC the pointer is on and disables the others, so the scanout id
//! is the *entire* signal for which display should show the sprite. Without it every cursor
//! landed on slot 0's window — the pointer would be on the second display, hit-testing
//! correctly, while its sprite was drawn on the first.
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

/// How many scanouts the backend will track. `VIRTIO_GPU_MAX_SCANOUTS` is the device's own
/// ceiling and `krun_add_display` enforces it, so a slot at or above this is a malformed guest
/// command rather than a configuration we could honour. Slots cost nothing until configured:
/// an unused one is a `None`.
pub const MAX_SCANOUTS: usize = 16;

/// A display backend that publishes guest scanouts as shared IOSurfaces.
///
/// One entry per pool slot: the guest's connectors are independent displays, each with its own
/// mode, framebuffer and surface ring, and each present names the slot it belongs to so the
/// supervisor can route it to the right window.
pub struct WindowBackend {
    control: Option<File>,
    scanouts: [Option<Scanout>; MAX_SCANOUTS],
    next_frame_id: u32,
    presents: u64,
    /// The current hardware-cursor IOSurface **per slot** (kept retained so the supervisor can
    /// look it up before we replace it on the next shape change). `None` where that slot's
    /// cursor is hidden.
    ///
    /// Per slot because the guest's cursor plane moves between connectors: it enables the plane
    /// on the display the pointer is on and hides it on the others, so a pointer crossing
    /// displays is a `cursor` on one slot and a `cursorhide` on another, back and forth. A
    /// single retain made each of those drop the *other* slot's just-published surface, leaving
    /// its lifetime to the supervisor's bounded store — the same class as the scanout-ring
    /// eviction that froze a display, and a non-global surface cannot be recovered once gone.
    cursor: [Option<CFRetained<IOSurfaceRef>>; MAX_SCANOUTS],
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
            scanouts: [const { None }; MAX_SCANOUTS],
            next_frame_id: 0,
            presents: 0,
            cursor: [const { None }; MAX_SCANOUTS],
            // No receiver ⇒ surfaces must stay global or the supervisor can't resolve them.
            also_global: also_global || sender.is_none(),
            sender,
        }
    }
}

/// Validate a guest-supplied scanout id as a pool slot. The bound is the device's own
/// `VIRTIO_GPU_MAX_SCANOUTS`; anything past it is a malformed command, not a display we could
/// grow into.
fn slot_of(scanout_id: u32) -> Result<usize, DisplayBackendError> {
    let slot = scanout_id as usize;
    (slot < MAX_SCANOUTS)
        .then_some(slot)
        .ok_or(DisplayBackendError::InvalidScanoutId)
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
        let slot = slot_of(scanout_id)?;
        // Reuse the existing surfaces on a same-geometry remodeset. A real guest (Fedora:
        // simpledrm → plymouth → GDM) reconfigures the scanout many times at the same mode;
        // reallocating fresh global IOSurfaces each time would churn their ids and free
        // surfaces out from under the supervisor's pending lookups. Keeping them stable
        // makes the steady state a no-op and avoids that race.
        if let Some(s) = self.scanouts[slot].as_mut() {
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
        log::info!(
            "window: scanout {scanout_id} -> IOSurfaces {ids:?} ({width}x{height} {format:?})"
        );
        // Hand each ring surface to the supervisor over the Mach channel, keyed by its id, BEFORE
        // the `surface` line announces them — so the supervisor can resolve the ids without a
        // global IOSurfaceLookup.
        for (id, surface) in ids.iter().zip(surfaces.iter()) {
            self.publish(*id, surface);
        }

        // The supervisor looks surfaces up lazily by id (one IOSurfaceLookup per `frame <id>`),
        // so the protocol still only needs to name the initial buffer; the rest are discovered
        // as they're shown. Keep the two-id shape for wire compatibility (id1 is ignored).
        let id0 = ids[0];
        let id1 = ids.get(1).copied().unwrap_or(id0);
        self.scanouts[slot] = Some(Scanout {
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
        self.send(&format!(
            "surface {id0} {id1} {width} {height} {scanout_id}"
        ));
        Ok(())
    }

    fn disable_scanout(&mut self, scanout_id: u32) -> Result<(), DisplayBackendError> {
        let slot = slot_of(scanout_id)?;
        self.scanouts[slot] = None;
        // Tell the supervisor the slot has no scanout, so the window presenting it can go away.
        // Without this a disconnected display would keep its window up showing the last frame
        // forever — the guest never presents to it again, so nothing else would ever say so.
        // Slot 0 sees this during ordinary modeset churn (simpledrm → plymouth → GDM each
        // disable then reconfigure), which is why it clears the frame rather than closing a
        // window: the announcement that follows brings it straight back.
        self.send(&format!("scanoutgone {scanout_id}"));
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
        slot_of(scanout_id)?;
        self.presents += 1;
        red_probe(iosurface_id, self.presents);
        self.send(&format!("frame {iosurface_id} {scanout_id}"));
        Ok(())
    }

    /// Tell the supervisor the guest is finished with `iosurface_id`, so it can drop its
    /// reference.
    ///
    /// A backend that hands surfaces to another process has no way to learn one is finished
    /// with: on macOS an IOSurface's storage bills to the task that CREATED it, so the holder
    /// feels no pressure of its own to let go, and the supervisor can otherwise bound its hold
    /// only by size. A compositor that mints a fresh scanout per frame then leaves whole
    /// framebuffers pinned for the supervisor's life — `testcomp/supervisor-retention.sh`
    /// measured 229.7 MiB still held after the compositor had quit. The guest's `RESOURCE_UNREF`
    /// is the accurate signal.
    ///
    /// This rides the surface port rather than the control socket on purpose: it is what keeps a
    /// release naming a recycled IOSurface id from overtaking the publish that reused it. See
    /// `limina_surfaceport`'s module docs for the ordering argument.
    fn release_surface(&mut self, iosurface_id: u32) -> Result<(), DisplayBackendError> {
        if let Some(tx) = self.sender.as_ref() {
            if let Err(e) = tx.release(iosurface_id) {
                log::error!("window: surface-port release(id={iosurface_id}) failed: {e}");
            }
        }
        Ok(())
    }

    /// The guest's OS driver has taken the device over from boot firmware.
    ///
    /// It matters because the two drivers do not agree about scanouts. EDK2's virtio-gpu GOP
    /// driver programs head 0 and nothing else (`OvmfPkg/VirtioGpuDxe/Gop.c`), so slot 0 must be
    /// the connected one for as long as firmware is drawing — GRUB included. Linux enumerates
    /// every scanout and follows connector hotplug, so from here the supervisor is free to give
    /// each host panel its own slot. Anything that arranges displays has to wait for this line.
    fn guest_driver_ready(&mut self) -> Result<(), DisplayBackendError> {
        self.send("guestdriver");
        Ok(())
    }

    fn alloc_frame(&mut self, scanout_id: u32) -> Result<(u32, &mut [u8]), DisplayBackendError> {
        let scanout = self.scanouts[slot_of(scanout_id)?]
            .as_mut()
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
        let slot = slot_of(scanout_id)?;
        // Take the scanout out so we can write the surface and call &mut self.send().
        let mut scanout = self.scanouts[slot]
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
        self.scanouts[slot] = Some(scanout);
        self.send(&format!("frame {show_id} {scanout_id}"));
        Ok(())
    }

    fn set_cursor(
        &mut self,
        scanout_id: u32,
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
            if let Some(slot) = self.cursor.get_mut(scanout_id as usize) {
                *slot = None;
            }
            self.send(&format!("cursorhide {scanout_id}"));
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
        for (dst, src) in canvas
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(buffer.as_chunks::<4>().0)
        {
            dst.copy_from_slice(&to_bgra_keep_alpha(format, src));
        }
        copy_canvas_into_surface(&surface, &canvas, width, height);

        if let Some(slot) = self.cursor.get_mut(scanout_id as usize) {
            *slot = Some(surface);
        }
        self.send(&format!(
            "cursor {id} {width} {height} {hot_x} {hot_y} {scanout_id}"
        ));
        Ok(())
    }

    fn move_cursor(&mut self, scanout_id: u32, x: u32, y: u32) -> Result<(), DisplayBackendError> {
        // The virtio wire field is u32, but a cursor whose hotspot hangs past the scanout's
        // left/top edge is legitimately negative — the guest kernel casts (e.g. -2 arrives as
        // 4294967294). Recover the signed value before forwarding, so the supervisor draws the
        // sprite partially off-edge instead of dropping the position.
        self.send(&format!(
            "cursormove {} {} {scanout_id}",
            x as i32, y as i32
        ));
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

    /// A backend whose control channel is a temp file, so a test can read the exact wire lines
    /// it emitted. The file *is* the protocol as the supervisor sees it — asserting on the
    /// backend's internal state instead would prove nothing about what got routed.
    fn backend_with_wire() -> (WindowBackend, std::path::PathBuf) {
        use std::os::fd::AsRawFd;
        let path = std::env::temp_dir().join(format!(
            "limina-wire-{}-{:?}.txt",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = File::create(&path).expect("wire file");
        let backend = WindowBackend::new(Some(&WindowConfig {
            control_fd: file.as_raw_fd(),
            surface_port_name: None,
        }));
        (backend, path)
    }

    fn wire(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .expect("wire file")
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn every_pool_slot_can_be_configured_and_presented() {
        // The scanout pool's whole point: a slot above 0 is a real display. Until the pool
        // landed the backend answered InvalidScanoutId for every slot but 0, so a guest driving
        // a second monitor got ErrInvalidScanoutId once per frame and nothing was ever shown
        // (measured on a live guest — spikes/scanout-pool/RESULTS.md).
        let (mut b, path) = backend_with_wire();
        for slot in [0u32, 1, MAX_SCANOUTS as u32 - 1] {
            b.configure_scanout(slot, 64, 32, 64, 32, ResourceFormat::BGRX)
                .unwrap_or_else(|e| panic!("configure_scanout({slot}) rejected: {e:?}"));
            let (_id, staging) = b
                .alloc_frame(slot)
                .unwrap_or_else(|e| panic!("alloc_frame({slot}) rejected: {e:?}"));
            staging[0] = slot as u8;
            b.present_frame(slot, 0, None)
                .unwrap_or_else(|e| panic!("present_frame({slot}) rejected: {e:?}"));
        }

        // Every line names its slot, so the supervisor can route it to the right window.
        let lines = wire(&path);
        for slot in [0u32, 1, MAX_SCANOUTS as u32 - 1] {
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with("surface ") && l.ends_with(&format!(" {slot}"))),
                "no `surface … {slot}` line in {lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with("frame ") && l.ends_with(&format!(" {slot}"))),
                "no `frame … {slot}` line in {lines:?}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn slots_keep_their_own_framebuffers() {
        // One shared `Scanout` would make the second slot's configure resize the first, and
        // both slots would then present the same pixels — the failure that looks like "the
        // second monitor mirrors the first".
        let (mut b, path) = backend_with_wire();
        b.configure_scanout(0, 64, 32, 64, 32, ResourceFormat::BGRX)
            .expect("configure slot 0");
        b.configure_scanout(1, 128, 64, 128, 64, ResourceFormat::BGRX)
            .expect("configure slot 1");

        let (_, s0) = b.alloc_frame(0).expect("alloc slot 0");
        assert_eq!(s0.len(), 64 * 32 * 4, "slot 0's staging buffer was resized");
        let (_, s1) = b.alloc_frame(1).expect("alloc slot 1");
        assert_eq!(s1.len(), 128 * 64 * 4, "slot 1 has the wrong staging size");

        // Disabling one slot leaves the other presentable.
        b.disable_scanout(1).expect("disable slot 1");
        assert!(
            b.alloc_frame(1).is_err(),
            "a disabled slot must not hand out a framebuffer"
        );
        b.alloc_frame(0).expect("slot 0 survives slot 1's disable");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_slot_past_the_pool_is_still_rejected() {
        // The bound moved from 1 to MAX_SCANOUTS; it did not disappear. An out-of-range id is
        // a malformed guest command and must stay an error rather than index off the array.
        let (mut b, path) = backend_with_wire();
        let over = MAX_SCANOUTS as u32;
        assert!(matches!(
            b.configure_scanout(over, 64, 32, 64, 32, ResourceFormat::BGRX),
            Err(DisplayBackendError::InvalidScanoutId)
        ));
        assert!(matches!(
            b.present_frame(over, 0, None),
            Err(DisplayBackendError::InvalidScanoutId)
        ));
        assert!(matches!(
            b.present_surface(over, 7, None),
            Err(DisplayBackendError::InvalidScanoutId)
        ));
        assert!(matches!(
            b.disable_scanout(over),
            Err(DisplayBackendError::InvalidScanoutId)
        ));
        let _ = std::fs::remove_file(&path);
    }

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
        b.set_cursor(0, 2, 2, 0, 0, ResourceFormat::BGRA, &data)
            .unwrap();
        let surf = b.cursor[0].as_ref().expect("cursor surface published");
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
        assert!(b.cursor[0].is_none());

        // A 2×2 BGRX cursor: pixel (x,y) = [x, y, 9, 0].
        let data = staging(2, 2, 9);
        b.set_cursor(0, 2, 2, 1, 1, ResourceFormat::BGRX, &data)
            .unwrap();
        let surf = b.cursor[0].as_ref().expect("cursor surface published");
        unsafe {
            IOSurfaceLock(surf, IOSurfaceLockOptions(0), ptr::null_mut());
            assert_eq!(px(surf, 0, 0), [0, 0, 9, 255]);
            assert_eq!(px(surf, 1, 1), [1, 1, 9, 255]); // opaque alpha forced
            IOSurfaceUnlock(surf, IOSurfaceLockOptions(0), ptr::null_mut());
        }

        // Zero size hides the cursor (and an empty buffer must not panic).
        b.set_cursor(0, 0, 0, 0, 0, ResourceFormat::BGRX, &[])
            .unwrap();
        assert!(b.cursor[0].is_none());
        // A too-small buffer is also treated as hide, not a panic.
        b.set_cursor(0, 4, 4, 0, 0, ResourceFormat::BGRX, &[0, 0, 0, 0])
            .unwrap();
        assert!(b.cursor[0].is_none());
    }

    /// A cursor crossing displays is a show on one slot and a hide on another, over and over.
    /// One retain for the whole backend made each of those drop the other slot's surface, whose
    /// only other reference is the supervisor's bounded store — and a non-global IOSurface that
    /// falls out of it cannot be looked up again.
    #[test]
    fn each_slot_keeps_its_own_cursor_surface() {
        let mut b = WindowBackend::new(None);
        let data = staging(2, 2, 9);
        b.set_cursor(0, 2, 2, 0, 0, ResourceFormat::BGRX, &data)
            .unwrap();
        b.set_cursor(1, 2, 2, 0, 0, ResourceFormat::BGRX, &data)
            .unwrap();
        assert!(b.cursor[0].is_some(), "slot 0 lost its surface to slot 1");
        assert!(b.cursor[1].is_some());

        // The pointer leaves slot 0: only slot 0's surface goes.
        b.set_cursor(0, 0, 0, 0, 0, ResourceFormat::BGRX, &[])
            .unwrap();
        assert!(b.cursor[0].is_none());
        assert!(b.cursor[1].is_some(), "hiding slot 0 dropped slot 1");
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
