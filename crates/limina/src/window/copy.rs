// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Showing a private copy of a guest scanout instead of the guest's own surface.
//!
//! A zero-copy present hands the window server a surface the guest still owns. That is safe only
//! while the guest is held off the surface until the frame has left glass, and the device can hold
//! only a guest that fences its scanout flushes (`scanout_held` in libkrun's display API). A stock
//! kernel does not, and its compositor draws later frames into a buffer the window server is still
//! compositing. Measured 2026-09-12 in the WebGL aquarium at 30k fish: about 60% of frames changed
//! while on glass, and the visible part was older frames flashing back. A copy the guest never
//! sees cannot change under the window server.
//!
//! The copy is a GPU blit into a ring of our own surfaces, waited on before it is shown. The CPU
//! copy (`diag::copy_surface`) is the fallback when Metal cannot take the surfaces.

use std::cell::OnceCell;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor,
};

/// Copies in use at once: the one on glass, the one the window server may still be latching, and
/// the one being written.
const RING: usize = 3;

/// One window's copies, remade whenever the guest's surfaces change size.
#[derive(Default)]
pub(crate) struct CopyRing {
    ring: Vec<CFRetained<IOSurfaceRef>>,
    geom: (usize, usize),
    next: usize,
    cost: CopyCost,
}

impl CopyRing {
    /// Copy `src` into the next surface of the ring and return that surface, or `None` when the
    /// ring could not be made (the caller then shows `src` itself).
    pub(crate) fn copy(
        &mut self,
        src: &CFRetained<IOSurfaceRef>,
    ) -> Option<CFRetained<IOSurfaceRef>> {
        let geom = (src.width(), src.height());
        if self.geom != geom {
            self.geom = geom;
            self.ring = (0..RING)
                .filter_map(|_| super::diag::create_local_iosurface(geom.0 as u32, geom.1 as u32))
                .collect();
        }
        if self.ring.len() != RING {
            return None;
        }
        let dst = self.ring[self.next % RING].clone();
        self.next = self.next.wrapping_add(1);
        let start = Instant::now();
        let on_gpu = gpu_copy(src, &dst);
        if !on_gpu {
            super::diag::copy_surface(src, &dst);
        }
        self.cost.record(on_gpu, start.elapsed());
        Some(dst)
    }
}

/// What the copies cost, logged every [`CopyCost::EVERY`] so a slow path shows in the log.
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
                 mean {:?}, max {:?}",
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

/// Blit `src` into `dst` (same size) and wait for it. False when Metal cannot do it, in which
/// case nothing was written.
fn gpu_copy(src: &IOSurfaceRef, dst: &IOSurfaceRef) -> bool {
    GPU.with(|gpu| {
        let Some(gpu) = gpu.get_or_init(|| {
            let device = MTLCreateSystemDefaultDevice()?;
            let queue = device.newCommandQueue()?;
            Some(Gpu { device, queue })
        }) else {
            return false;
        };
        let (Some(from), Some(to)) = (gpu.texture(src), gpu.texture(dst)) else {
            return false;
        };
        let Some(commands) = gpu.queue.commandBuffer() else {
            return false;
        };
        let Some(blit) = commands.blitCommandEncoder() else {
            return false;
        };
        // SAFETY: both textures live until this function returns, and the wait below keeps the
        // GPU's use of them inside that.
        unsafe { blit.copyFromTexture_toTexture(&from, &to) };
        blit.endEncoding();
        commands.commit();
        commands.waitUntilCompleted();
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_io_surface::{
        IOSurfaceGetBaseAddress, IOSurfaceGetBytesPerRow, IOSurfaceLock, IOSurfaceLockOptions,
        IOSurfaceUnlock,
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
        let dst = ring.copy(&src).expect("a ring surface");
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
        let ids: Vec<u32> = (0..RING + 1)
            .map(|_| objc2_io_surface::IOSurfaceGetID(&ring.copy(&src).expect("a ring surface")))
            .collect();
        assert_eq!(ids[0], ids[RING], "the ring comes round");
        assert!(
            ids[..RING]
                .iter()
                .enumerate()
                .all(|(i, a)| !ids[i + 1..RING].contains(a))
        );
    }
}
