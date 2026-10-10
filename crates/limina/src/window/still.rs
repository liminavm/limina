// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! `limina debug <vm> capture still <png> [slot]`: the frame on glass on one display, written
//! now, however long ago it went up and whether or not a frame-sequence capture is running.
//!
//! **Where the frame comes from.** Every present notes, per window, the surface it put on its
//! layer and the frame's tag ([`note`], from `GuestWindow::show_with_ack`), and a window that
//! closes forgets its note ([`forget`]). That is the main thread's whole share: a lock, a surface
//! retain and two clock reads per present. The still itself runs on the thread that asked: it
//! blits the noted surface into a buffer of its own, waits for the blit, reads and encodes it
//! (the frame capture's copy and encoder, `frame_capture::gpu_copy` and
//! `limina_framecap::encode_png_rgb`), so a still never waits on the main thread and the main
//! thread never waits on a still.
//!
//! **Why the pixels are the tagged frame's.** A window can put a new frame up while the blit is
//! reading the old one, and from then on the old surface may be drawn into again (by the guest,
//! or by the window's copy ring two presents later). So the note carries a present count, and a
//! still whose display presented during its blit is taken again, up to [`TRIES`] times.
//!
//! `LIMINA_WINDOW_CAPTURE` (one PNG, overwritten once a second) is a separate diagnostic and is
//! untouched by this.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use limina_framecap::Still;
use objc2_core_foundation::CFRetained;
use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};
use objc2_metal::{MTLCommandBuffer, MTLCommandBufferStatus};

use super::frame_capture::{FrameTag, clock_ns, gpu_copy, pixel_order};
use super::present::SendSurface;

/// How many times a still is taken before giving up on a display that keeps presenting during
/// the blit. At 60 Hz a blit (about 1 ms) loses the race rarely; a few tries make it vanish.
const TRIES: usize = 5;

/// What one window has on glass.
struct OnGlass {
    tag: FrameTag,
    presented: u32,
    layer: SendSurface,
    t_monotonic_raw_ns: u64,
    t_realtime_ns: u64,
    /// This window's presents so far: a still compares it across its blit.
    presents: u64,
}

/// Per window (keyed by its `NSWindow`'s address, stable while it lives), what it has on glass.
static ON_GLASS: Mutex<Vec<(usize, OnGlass)>> = Mutex::new(Vec::new());

/// The main window's key, for a still that names no display. 0 until it exists.
static PRIMARY: AtomicUsize = AtomicUsize::new(0);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The window with this key is the main one.
pub(crate) fn set_primary(window: usize) {
    PRIMARY.store(window, Ordering::Release);
}

/// The window `window` just put `layer` on glass as frame `tag`, the worker's surface
/// `presented`. Main thread, every present.
pub(crate) fn note(window: usize, tag: FrameTag, presented: u32, layer: &CFRetained<IOSurfaceRef>) {
    let mono = clock_ns(libc::CLOCK_MONOTONIC_RAW);
    let real = clock_ns(libc::CLOCK_REALTIME);
    let mut all = lock(&ON_GLASS);
    let presents = match all.iter_mut().find(|(k, _)| *k == window) {
        Some((_, g)) => g.presents + 1,
        None => 1,
    };
    let entry = OnGlass {
        tag,
        presented,
        layer: SendSurface::new(layer.clone()),
        t_monotonic_raw_ns: mono,
        t_realtime_ns: real,
        presents,
    };
    match all.iter_mut().find(|(k, _)| *k == window) {
        Some((_, g)) => *g = entry,
        None => all.push((window, entry)),
    }
}

/// The window is closing: it has nothing on glass any more.
pub(crate) fn forget(window: usize) {
    lock(&ON_GLASS).retain(|(k, _)| *k != window);
}

/// What a still reads: the surface, its tag, and the present count to compare after the blit.
struct Pick {
    window: usize,
    tag: FrameTag,
    presented: u32,
    layer: SendSurface,
    t_monotonic_raw_ns: u64,
    t_realtime_ns: u64,
    presents: u64,
}

/// The note for `slot`, or for the main window when `None`.
fn pick(slot: Option<usize>) -> Result<Pick, String> {
    let all = lock(&ON_GLASS);
    let shown = || {
        let mut slots: Vec<_> = all.iter().map(|(_, g)| g.tag.slot).collect();
        slots.sort_unstable();
        slots.dedup();
        slots
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let found = match slot {
        Some(slot) => all
            .iter()
            .find(|(_, g)| g.tag.slot == slot)
            .ok_or_else(|| {
                if all.is_empty() {
                    no_window()
                } else {
                    format!(
                        "no window shows guest display {slot} (displays on glass: {})",
                        shown()
                    )
                }
            })?,
        None => {
            let primary = PRIMARY.load(Ordering::Acquire);
            all.iter().find(|(k, _)| *k == primary).ok_or_else(|| {
                if primary == 0 {
                    no_window()
                } else {
                    "the main window has not put a frame on glass yet".to_string()
                }
            })?
        }
    };
    let (window, g) = found;
    Ok(Pick {
        window: *window,
        tag: g.tag,
        presented: g.presented,
        layer: g.layer.clone(),
        t_monotonic_raw_ns: g.t_monotonic_raw_ns,
        t_realtime_ns: g.t_realtime_ns,
        presents: g.presents,
    })
}

fn no_window() -> String {
    "this VM has no window on glass (a headless run has none; a windowed one shows its first \
     frame once the guest presents)"
        .to_string()
}

/// Whether the window that showed `p` has presented again since.
fn moved_on(p: &Pick) -> bool {
    lock(&ON_GLASS)
        .iter()
        .find(|(k, _)| *k == p.window)
        .is_none_or(|(_, g)| g.presents != p.presents)
}

/// Write the frame on glass on `slot` (the main window's when `None`) to `path` as an RGB PNG,
/// and return its tag line. `path` is used as given, like `capture start`'s directory: a relative
/// one is this process's, which is why `limina debug` makes it absolute before sending it.
pub(crate) fn take(path: &Path, slot: Option<usize>) -> Result<String, String> {
    for _ in 0..TRIES {
        let p = pick(slot)?;
        let layer = p.layer.clone().into_inner();
        let (w, h) = (layer.width(), layer.height());
        let buffer = super::diag::create_local_iosurface(w as u32, h as u32)
            .ok_or("could not allocate a buffer for the still")?;
        let commands = gpu_copy(&layer, &SendSurface::new(buffer.clone()))
            .ok_or("the GPU copy of the frame on glass could not be made")?;
        commands.waitUntilCompleted();
        if commands.status() != MTLCommandBufferStatus::Completed {
            return Err("the GPU copy of the frame on glass failed".into());
        }
        if moved_on(&p) {
            continue;
        }
        let taken_mono = clock_ns(libc::CLOCK_MONOTONIC_RAW);
        let taken_real = clock_ns(libc::CLOCK_REALTIME);
        let png = read_png(&buffer, pixel_order(&layer))?;
        std::fs::write(path, &png).map_err(|e| format!("{}: {e}", path.display()))?;
        let still = Still {
            format: limina_framecap::FORMAT,
            still: true,
            slot: p.tag.slot,
            flip: p.tag.flip,
            epoch: p.tag.epoch,
            guest_flip: p.tag.cause.is_none(),
            cause: p.tag.cause,
            presented_iosurface: p.presented,
            layer_iosurface: layer.id(),
            guest_resource: p.tag.guest_resource(p.tag.cause),
            width: w as u32,
            height: h as u32,
            t_monotonic_raw_ns: p.t_monotonic_raw_ns,
            t_realtime_ns: p.t_realtime_ns,
            taken_t_monotonic_raw_ns: taken_mono,
            taken_t_realtime_ns: taken_real,
            file: path.to_string_lossy().into_owned(),
        };
        return Ok(still.to_line());
    }
    Err(format!(
        "the display presented during each of {TRIES} tries; nothing was written"
    ))
}

/// Read our own buffer, whose blit has completed, as a PNG.
fn read_png(
    s: &CFRetained<IOSurfaceRef>,
    order: limina_framecap::PixelOrder,
) -> Result<Vec<u8>, String> {
    // SAFETY: our own surface, whose blit has completed. A lock that fails leaves the base
    // address unpinned, so nothing is read; one that succeeds pins it for the read, and the
    // slice is exactly its allocation's rows.
    unsafe {
        if s.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut()) != 0 {
            return Err("could not lock the still's buffer".into());
        }
        let (w, h, bpr) = (s.width(), s.height(), s.bytes_per_row());
        let bytes = std::slice::from_raw_parts(s.base_address().as_ptr() as *const u8, h * bpr);
        let png = limina_framecap::encode_png_rgb(bytes, w, h, bpr, order);
        s.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
        png
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test owns the process-global note table, so the cases run in order.
    #[test]
    fn a_still_reads_what_is_on_glass_and_says_clearly_when_nothing_is() {
        let dir = std::env::temp_dir().join(format!("limina-still-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("still.png");

        // No window at all.
        let err = take(&png, None).unwrap_err();
        assert!(err.contains("no window"), "{err}");
        assert!(take(&png, Some(0)).unwrap_err().contains("no window"));

        // A window on slot 1 whose frame is a known colour; the main window has shown nothing.
        let surface = super::super::diag::create_local_iosurface(8, 4).unwrap();
        // SAFETY: our own surface, locked for the write.
        unsafe {
            assert_eq!(
                surface.lock(IOSurfaceLockOptions::empty(), std::ptr::null_mut()),
                0
            );
            let bpr = surface.bytes_per_row();
            let base = surface.base_address().as_ptr() as *mut u8;
            for y in 0..4 {
                for x in 0..8 {
                    let px = base.add(y * bpr + x * 4);
                    // BGRA: a mid blue-green.
                    std::ptr::copy_nonoverlapping([0x40u8, 0x80, 0x10, 0xff].as_ptr(), px, 4);
                }
            }
            surface.unlock(IOSurfaceLockOptions::empty(), std::ptr::null_mut());
        }
        set_primary(0xa);
        let reshow = FrameTag {
            cause: Some(limina_framecap::Cause::Reshow),
            ..FrameTag::new(1, 287, 3).with_resource(Some(5))
        };
        note(0xb, reshow, 44, &surface);
        let err = take(&png, None).unwrap_err();
        assert!(err.contains("main window has not"), "{err}");
        let err = take(&png, Some(2)).unwrap_err();
        assert!(
            err.contains("display 2") && err.contains("on glass: 1"),
            "{err}"
        );

        // The still of slot 1: its pixels and its tag.
        let line = take(&png, Some(1)).unwrap();
        let still = Still::parse(&line).unwrap();
        assert_eq!(
            (
                still.slot,
                still.flip,
                still.epoch,
                still.guest_flip,
                still.cause
            ),
            (1, 287, 3, false, Some(limina_framecap::Cause::Reshow))
        );
        assert_eq!(
            still.guest_resource, None,
            "a re-show is no flip of its resource"
        );
        assert_eq!(
            (still.presented_iosurface, still.layer_iosurface),
            (44, surface.id())
        );
        assert_eq!((still.width, still.height), (8, 4));
        assert!(still.taken_t_realtime_ns >= still.t_realtime_ns);
        assert_eq!(still.file, png.to_string_lossy());
        let mut reader = png::Decoder::new(std::fs::File::open(&png).unwrap())
            .read_info()
            .unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (8, 4));
        assert!(
            buf[..info.buffer_size()]
                .as_chunks::<3>()
                .0
                .iter()
                .all(|p| *p == [0x10, 0x80, 0x40])
        );

        // The main window's frame is the default; a closed window has nothing on glass.
        note(
            0xa,
            FrameTag::new(0, 9, 3).with_resource(Some(12)),
            50,
            &surface,
        );
        let main = Still::parse(&take(&png, None).unwrap()).unwrap();
        assert_eq!(
            (main.slot, main.flip, main.guest_flip, main.guest_resource),
            (0, 9, true, Some(12))
        );
        forget(0xb);
        assert!(take(&png, Some(1)).unwrap_err().contains("display 1"));
        forget(0xa);
        set_primary(0);
        assert!(take(&png, None).unwrap_err().contains("no window"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
