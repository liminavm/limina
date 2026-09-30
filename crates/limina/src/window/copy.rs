// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Showing a private copy of a guest scanout instead of the guest's own surface.
//!
//! A zero-copy present hands the window server a surface the guest still owns. That is safe only
//! while the guest is held off the surface until the frame has left glass, and the device can hold
//! only a guest that fences its scanout flushes (`scanout_held` in libkrun's display API). The guest
//! kernel fences none of a vrend desktop's flushes, on either tier, and its compositor draws later
//! frames into a buffer the window server is still compositing. Measured 2026-09-12 in the WebGL
//! aquarium at 30k fish: about 60% of frames changed while on glass, and the visible part was older
//! frames flashing back. A copy the guest never sees cannot change under the window server.
//!
//! The copy is a GPU blit into a ring of our own surfaces. The main thread never waits for it:
//! the blit's completion wakes the main queue, and the next apply puts the copy up
//! ([`CopyRing::take_ready`]). Waiting instead cost the main thread 1-6 ms a frame, which the
//! pointer showed as stutter. At most one blit is in flight; a frame that arrives meanwhile waits
//! behind it, and a newer one replaces it, so the window shows the newest frame and never an older
//! one after it. The CPU copy (`diag::copy_surface`) is the fallback when Metal cannot take the
//! surfaces, and runs at once.
//!
//! **The copy reads the guest's surface when the blit runs, not when the frame arrived.** Nothing
//! holds an unfenced guest off the surface in between, so a copy that runs late shows whatever the
//! guest has drawn into it since. Two seams make that observable in a test:
//! `LIMINA_TEST_COPY_DELAY_MS` holds every copy back by that long before it reads the guest's
//! surface, standing in for a busy main thread or GPU, and `LIMINA_PRESENT_COPY_TRACE` logs the
//! first pixel of each frame as it arrives and again as its copy goes up. A difference between
//! the two is a copy that did not show the frame it was made for.

use std::cell::OnceCell;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder,
    MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLTexture,
    MTLTextureDescriptor,
};

/// Copies in use at once: the one on glass, the one the window server may still be latching, and
/// the one being written.
const RING: usize = 3;

type Commands = Retained<ProtocolObject<dyn MTLCommandBuffer>>;

/// What [`CopyRing::submit`] did with a frame.
pub(crate) enum Submitted {
    /// The copy is done (the CPU fallback): show it now.
    Show(CFRetained<IOSurfaceRef>),
    /// The copy is on the GPU, or waiting behind one that is; [`CopyRing::take_ready`] hands it
    /// over.
    Pending,
    /// No ring could be made: show the guest's surface itself.
    Unavailable,
}

/// The copy of frame `id`, from the moment it is started until it goes up.
struct InFlight {
    id: u32,
    /// The guest surface's first pixel when the frame arrived, under `LIMINA_PRESENT_COPY_TRACE`.
    arrived: Option<u32>,
    dst: CFRetained<IOSurfaceRef>,
    stage: Stage,
    started: Instant,
}

enum Stage {
    /// Held back by `LIMINA_TEST_COPY_DELAY_MS`: nothing has read the guest's surface yet.
    Deferred {
        src: CFRetained<IOSurfaceRef>,
        due: Instant,
    },
    /// On the GPU.
    Blit(Commands),
    /// Done at once on the CPU, which only a deferred copy that Metal refused ends up as.
    OnCpu,
}

/// `LIMINA_TEST_COPY_DELAY_MS`: how long to hold each copy back before it reads the guest's
/// surface. A test seam only.
fn test_copy_delay() -> Option<Duration> {
    static DELAY: OnceLock<Option<Duration>> = OnceLock::new();
    *DELAY.get_or_init(|| {
        let ms = std::env::var("LIMINA_TEST_COPY_DELAY_MS")
            .ok()?
            .parse::<u64>()
            .ok()?;
        log::warn!("window: TEST SEAM: every guest frame copy is held back {ms} ms");
        Some(Duration::from_millis(ms))
    })
}

/// `LIMINA_PRESENT_COPY_TRACE`: log the first pixel of each frame as it arrives and as its copy
/// goes up.
fn copy_trace() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("LIMINA_PRESENT_COPY_TRACE").is_some())
}

/// The guest surface's first pixel as frame `id` arrives, when tracing.
fn arrival(src: &CFRetained<IOSurfaceRef>) -> Option<u32> {
    copy_trace().then(|| super::diag::first_pixel(src))
}

/// Log what the copy of frame `id` holds against what the guest's surface held when the frame
/// arrived, as `bgra` bytes in memory order. They differ when the copy read the surface after the
/// guest had drawn into it again.
fn trace_shown(id: u32, arrived: Option<u32>, copy: &CFRetained<IOSurfaceRef>) {
    if let Some(arrived) = arrived {
        log::info!(
            "copy trace: frame {id} arrived bgra={arrived:08x} shown bgra={:08x}",
            super::diag::first_pixel(copy)
        );
    }
}

/// One window's copies, remade whenever the guest's surfaces change size.
#[derive(Default)]
pub(crate) struct CopyRing {
    ring: Vec<CFRetained<IOSurfaceRef>>,
    geom: (usize, usize),
    next: usize,
    in_flight: Option<InFlight>,
    /// The newest frame that arrived while a blit was in flight, with its [`arrival`] pixel.
    waiting: Option<(u32, CFRetained<IOSurfaceRef>, Option<u32>)>,
    cost: CopyCost,
}

impl CopyRing {
    /// Copy frame `id` from `src`. Frames this displaces, which will now never be shown, are
    /// pushed onto `dropped`.
    pub(crate) fn submit(
        &mut self,
        id: u32,
        src: &CFRetained<IOSurfaceRef>,
        dropped: &mut Vec<u32>,
    ) -> Submitted {
        let arrived = arrival(src);
        let geom = (src.width(), src.height());
        if self.geom != geom {
            self.cancel(dropped);
            self.geom = geom;
            self.ring = (0..RING)
                .filter_map(|_| super::diag::create_local_iosurface(geom.0 as u32, geom.1 as u32))
                .collect();
        }
        if self.ring.len() != RING {
            return Submitted::Unavailable;
        }
        if self.in_flight.is_some() {
            if let Some((old, ..)) = self.waiting.replace((id, src.clone(), arrived)) {
                dropped.push(old);
            }
            return Submitted::Pending;
        }
        match self.start(id, src, arrived) {
            Some(dst) => Submitted::Show(dst),
            None => Submitted::Pending,
        }
    }

    /// The copy to put up now, if the blit in flight has finished. Starts the frame waiting
    /// behind it; frames that will never be shown are pushed onto `dropped`.
    pub(crate) fn take_ready(
        &mut self,
        dropped: &mut Vec<u32>,
    ) -> Option<(u32, CFRetained<IOSurfaceRef>)> {
        let flight = self.in_flight.as_mut()?;
        if let Stage::Deferred { src, due } = &flight.stage {
            if Instant::now() < *due {
                return None;
            }
            let src = src.clone();
            flight.stage = match gpu_copy(&src, &flight.dst) {
                Some(commands) => Stage::Blit(commands),
                None => {
                    super::diag::copy_surface(&src, &flight.dst);
                    Stage::OnCpu
                }
            };
        }
        let failed = match &flight.stage {
            Stage::Blit(commands) => match commands.status() {
                MTLCommandBufferStatus::Completed => false,
                MTLCommandBufferStatus::Error => true,
                _ => return None,
            },
            Stage::OnCpu => false,
            Stage::Deferred { .. } => unreachable!("started above"),
        };
        let done = self.in_flight.take()?;
        self.cost
            .record(matches!(done.stage, Stage::Blit(_)), done.started.elapsed());
        let mut ready = Some((done.id, done.dst));
        if !failed && let Some((id, dst)) = &ready {
            trace_shown(*id, done.arrived, dst);
        }
        if failed {
            log::warn!(
                "window: the GPU copy of frame {} failed; skipping it",
                done.id
            );
            dropped.push(done.id);
            ready = None;
        }
        if let Some((id, src, arrived)) = self.waiting.take()
            && let Some(dst) = self.start(id, &src, arrived)
        {
            // Copied at once on the CPU, so it is up before the one just finished could be.
            dropped.extend(ready.map(|(old, _)| old));
            return Some((id, dst));
        }
        ready
    }

    /// Forget every frame not yet shown, pushing them onto `dropped`. A blit already on the GPU
    /// still finishes, into a surface nothing will show.
    pub(crate) fn cancel(&mut self, dropped: &mut Vec<u32>) {
        dropped.extend(self.in_flight.take().map(|f| f.id));
        dropped.extend(self.waiting.take().map(|(id, ..)| id));
    }

    /// Begin copying frame `id` into the next surface of the ring: `Some` when it is already
    /// done (the CPU fallback), `None` when it is on the GPU.
    fn start(
        &mut self,
        id: u32,
        src: &CFRetained<IOSurfaceRef>,
        arrived: Option<u32>,
    ) -> Option<CFRetained<IOSurfaceRef>> {
        let dst = self.ring[self.next % RING].clone();
        self.next = self.next.wrapping_add(1);
        let started = Instant::now();
        if let Some(delay) = test_copy_delay() {
            self.in_flight = Some(InFlight {
                id,
                arrived,
                dst,
                stage: Stage::Deferred {
                    src: src.clone(),
                    due: started + delay,
                },
                started,
            });
            // Nothing else is due to wake the main queue when the delay is up.
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                super::present::wake_main_apply();
            });
            return None;
        }
        if let Some(commands) = gpu_copy(src, &dst) {
            self.in_flight = Some(InFlight {
                id,
                arrived,
                dst,
                stage: Stage::Blit(commands),
                started,
            });
            return None;
        }
        super::diag::copy_surface(src, &dst);
        self.cost.record(false, started.elapsed());
        trace_shown(id, arrived, &dst);
        Some(dst)
    }
}

/// What the copies cost, logged every [`CopyCost::EVERY`] so a slow path shows in the log. A GPU
/// copy is timed from commit to the apply that finds it done, which is what it adds to the frame.
#[derive(Default)]
struct CopyCost {
    copies: u64,
    on_cpu: u64,
    total: Duration,
    max: Duration,
}

impl CopyCost {
    const EVERY: u32 = 1000;

    fn record(&mut self, on_gpu: bool, took: Duration) {
        self.copies += 1;
        self.on_cpu += u64::from(!on_gpu);
        self.total += took;
        self.max = self.max.max(took);
        if self.copies.is_multiple_of(u64::from(Self::EVERY)) {
            log::info!(
                "window: {} guest frames copied before showing ({} on the CPU); the last {}: \
                 mean {:?}, max {:?} from copy to show",
                self.copies,
                self.on_cpu,
                Self::EVERY,
                self.total / Self::EVERY,
                self.max
            );
            self.total = Duration::ZERO;
            self.max = Duration::ZERO;
        }
    }
}

struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
}

impl Gpu {
    /// A 4-byte texture over `surface`'s storage. The blit moves bytes, so BGRA serves whatever
    /// 4-byte format the guest's surface holds.
    fn texture(&self, surface: &IOSurfaceRef) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        // SAFETY: a plain descriptor for a 2D texture of the surface's own size.
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                surface.width(),
                surface.height(),
                false,
            )
        };
        self.device
            .newTextureWithDescriptor_iosurface_plane(&desc, surface, 0)
    }
}

thread_local! {
    /// The main thread's Metal device and queue, made on first use; `None` when there is none.
    static GPU: OnceCell<Option<Gpu>> = const { OnceCell::new() };
}

/// Commit a blit of `src` into `dst` (same size) that wakes the main queue when it completes.
/// `None` when Metal cannot do it, in which case nothing was written.
fn gpu_copy(src: &IOSurfaceRef, dst: &IOSurfaceRef) -> Option<Commands> {
    GPU.with(|gpu| {
        let gpu = gpu
            .get_or_init(|| {
                let device = MTLCreateSystemDefaultDevice()?;
                let queue = device.newCommandQueue()?;
                Some(Gpu { device, queue })
            })
            .as_ref()?;
        let from = gpu.texture(src)?;
        let to = gpu.texture(dst)?;
        let commands = gpu.queue.commandBuffer()?;
        let blit = commands.blitCommandEncoder()?;
        // SAFETY: a retained command buffer keeps both textures alive until it completes.
        unsafe { blit.copyFromTexture_toTexture(&from, &to) };
        blit.endEncoding();
        let wake = RcBlock::new(|_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
            super::present::wake_main_apply()
        });
        // SAFETY: Metal copies the block before this returns.
        unsafe { commands.addCompletedHandler(RcBlock::as_ptr(&wake)) };
        commands.commit();
        Some(commands)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_io_surface::{
        IOSurfaceGetBaseAddress, IOSurfaceGetBytesPerRow, IOSurfaceGetID, IOSurfaceLock,
        IOSurfaceLockOptions, IOSurfaceUnlock,
    };

    /// Fill (or read) every row of a surface under its lock.
    fn rows(surface: &IOSurfaceRef, mut each: impl FnMut(usize, &mut [u8])) {
        let options = IOSurfaceLockOptions(0);
        // SAFETY: the lock pins the base address for the loop, and each row slice stays within
        // one row of the surface's own stride.
        unsafe {
            IOSurfaceLock(surface, options, std::ptr::null_mut());
            let base = IOSurfaceGetBaseAddress(surface).as_ptr() as *mut u8;
            let stride = IOSurfaceGetBytesPerRow(surface);
            for y in 0..surface.height() {
                each(
                    y,
                    std::slice::from_raw_parts_mut(base.add(y * stride), surface.width() * 4),
                );
            }
            IOSurfaceUnlock(surface, options, std::ptr::null_mut());
        }
    }

    impl CopyRing {
        /// Block until the blit in flight, if any, is done.
        fn wait(&self) {
            if let Some(InFlight {
                stage: Stage::Blit(commands),
                ..
            }) = &self.in_flight
            {
                commands.waitUntilCompleted();
            }
        }

        /// Copy `src` as frame `id` and wait for it.
        fn copy_now(
            &mut self,
            id: u32,
            src: &CFRetained<IOSurfaceRef>,
        ) -> CFRetained<IOSurfaceRef> {
            let mut dropped = Vec::new();
            assert!(matches!(
                self.submit(id, src, &mut dropped),
                Submitted::Pending
            ));
            self.wait();
            let (shown, dst) = self.take_ready(&mut dropped).expect("a finished copy");
            assert_eq!(shown, id);
            assert!(dropped.is_empty());
            dst
        }
    }

    #[test]
    fn a_copy_carries_the_guest_surfaces_bytes() {
        // Not 64-aligned, so the two surfaces' strides differ from width * 4.
        let (w, h) = (300, 200);
        let src = super::super::diag::create_local_iosurface(w, h).expect("source surface");
        rows(&src, |y, row| {
            for (x, b) in row.iter_mut().enumerate() {
                *b = (x * 7 + y * 13) as u8;
            }
        });
        let mut ring = CopyRing::default();
        let dst = ring.copy_now(1, &src);
        assert!(
            !std::ptr::eq::<IOSurfaceRef>(&*dst, &*src),
            "a copy, not the source"
        );
        assert_eq!(ring.cost.on_cpu, 0, "the copy ran on the GPU");
        rows(&dst, |y, row| {
            for (x, b) in row.iter().enumerate() {
                assert_eq!(*b, (x * 7 + y * 13) as u8, "byte {x} of row {y}");
            }
        });
    }

    #[test]
    fn the_ring_hands_out_distinct_surfaces_in_turn() {
        let src = super::super::diag::create_local_iosurface(64, 64).expect("source surface");
        let mut ring = CopyRing::default();
        let ids: Vec<u32> = (0..RING as u32 + 1)
            .map(|id| IOSurfaceGetID(&ring.copy_now(id, &src)))
            .collect();
        assert_eq!(ids[0], ids[RING], "the ring comes round");
        assert!(
            ids[..RING]
                .iter()
                .enumerate()
                .all(|(i, a)| !ids[i + 1..RING].contains(a))
        );
    }

    #[test]
    fn a_frame_waiting_behind_a_copy_gives_way_to_a_newer_one() {
        let src = super::super::diag::create_local_iosurface(64, 64).expect("source surface");
        let mut ring = CopyRing::default();
        let mut dropped = Vec::new();
        for id in 1..=3 {
            assert!(matches!(
                ring.submit(id, &src, &mut dropped),
                Submitted::Pending
            ));
        }
        assert_eq!(dropped, [2], "frame 2 was replaced before its copy began");
        ring.wait();
        assert_eq!(ring.take_ready(&mut dropped).map(|(id, _)| id), Some(1));
        ring.wait();
        assert_eq!(ring.take_ready(&mut dropped).map(|(id, _)| id), Some(3));
        assert!(ring.take_ready(&mut dropped).is_none(), "nothing left");
        assert_eq!(dropped, [2]);
    }

    #[test]
    fn a_cancel_drops_every_frame_not_yet_shown() {
        let src = super::super::diag::create_local_iosurface(64, 64).expect("source surface");
        let mut ring = CopyRing::default();
        let mut dropped = Vec::new();
        ring.submit(1, &src, &mut dropped);
        ring.submit(2, &src, &mut dropped);
        ring.cancel(&mut dropped);
        assert_eq!(dropped, [1, 2]);
        assert!(ring.take_ready(&mut dropped).is_none());
    }
}
