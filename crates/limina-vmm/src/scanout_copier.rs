// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The GPU blit that copies the Metal transport's scanouts before they are shown.
//!
//! A transport scanout is rendered out of this process, by the GPU host, and finished before the
//! guest flushes it. Showing it zero-copy holds a fenced guest off the buffer until the display
//! has let go of it, a refresh after latch, so every flip completes a refresh late. Showing a
//! copy holds the guest only for the copy: about a millisecond for a 2560x1440 frame on a busy
//! GPU, where the same copy on the CPU takes 1.6 ms at the median and much longer in the tail
//! (`spikes/transport-copy/RESULTS.md`).
//!
//! virglrs owns the ring the copies land in and the fence the guest waits on; this is the GPU it
//! has none of ([`virglrenderer::venus::present_copy::Copier`]). Each copy is a command buffer of
//! one blit on a queue of our own, committed on the virtio-gpu thread without waiting, and
//! finished from its completion handler. Anything Metal refuses falls back to the CPU copy.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex, OnceLock};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder,
    MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLTexture,
    MTLTextureDescriptor,
};
use virglrenderer::metal::SurfacePort;
use virglrenderer::venus::present_copy::{Copier, CopyJob, set_copier};

unsafe extern "C" {
    fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> i32;
}

struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
}

// SAFETY: an `id<MTLDevice>` and an `id<MTLCommandQueue>` are documented thread-safe.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl Gpu {
    /// A 4-byte texture over `surface`'s storage. The blit moves bytes, so BGRA serves whatever
    /// 4-byte format the scanout holds.
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

/// The surface a send right names; the right itself is given up.
fn surface(port: SurfacePort) -> Option<CFRetained<IOSurfaceRef>> {
    let port = port.into_raw();
    let surface = IOSurfaceRef::lookup_from_mach_port(port);
    // SAFETY: the one send right `into_raw` gave us, deallocated once, in our own task. The
    // lookup retained the surface, so it outlives the right.
    #[allow(deprecated)] // libc deprecates mach_task_self in favour of the mach2 crate.
    unsafe {
        mach_port_deallocate(libc::mach_task_self(), port)
    };
    surface
}

struct Blitter {
    gpu: OnceLock<Option<Gpu>>,
}

impl Blitter {
    fn gpu(&self) -> Option<&Gpu> {
        self.gpu
            .get_or_init(|| {
                let device = MTLCreateSystemDefaultDevice()?;
                let queue = device.newCommandQueue()?;
                Some(Gpu { device, queue })
            })
            .as_ref()
    }

    /// Commit `job`'s blit, finishing it when the blit completes, or hand the job back when Metal
    /// cannot take it.
    fn commit(&self, job: CopyJob) -> Result<(), CopyJob> {
        let (Some(src), Some(dst)) = (
            job.source().and_then(surface),
            job.target().and_then(surface),
        ) else {
            return Err(job);
        };
        // The handler is a shared block; the job is taken once, by whichever call comes first.
        let job = Arc::new(Mutex::new(Some(job)));
        let taken = job.clone();
        let committed = self.blit(&src, &dst, move |copied| {
            if let Some(job) = taken.lock().unwrap_or_else(|e| e.into_inner()).take() {
                job.finish(copied);
            }
        });
        if committed {
            return Ok(());
        }
        let job = job.lock().unwrap_or_else(|e| e.into_inner()).take();
        job.map_or(Ok(()), Err)
    }

    /// Commit one blit from `src` to `dst` and call `done` with whether it completed. False, with
    /// `done` never called, when Metal cannot take it.
    ///
    /// Inside a pool of its own: the virtio-gpu thread this runs on never drains one, so the
    /// descriptors and the encoder Metal autoreleases would otherwise live as long as the worker,
    /// some 700 bytes a present.
    fn blit(&self, src: &IOSurfaceRef, dst: &IOSurfaceRef, done: impl Fn(bool) + 'static) -> bool {
        autoreleasepool(|_| self.encode(src, dst, done))
    }

    fn encode(
        &self,
        src: &IOSurfaceRef,
        dst: &IOSurfaceRef,
        done: impl Fn(bool) + 'static,
    ) -> bool {
        let Some(gpu) = self.gpu() else {
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
        // SAFETY: a retained command buffer keeps both textures alive until it completes.
        unsafe { blit.copyFromTexture_toTexture(&from, &to) };
        blit.endEncoding();
        let done = RcBlock::new(
            move |commands: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                // SAFETY: Metal passes the command buffer that completed, alive for the call.
                done(unsafe { commands.as_ref() }.status() == MTLCommandBufferStatus::Completed);
            },
        );
        // SAFETY: Metal copies the block before this returns.
        unsafe { commands.addCompletedHandler(RcBlock::as_ptr(&done)) };
        commands.commit();
        true
    }
}

impl Copier for Blitter {
    fn copy(&self, job: CopyJob) {
        if let Err(job) = self.commit(job) {
            job.finish(false);
        }
    }
}

/// Install the copier in the renderer. The Metal device is made on the first copy, so a VM with
/// no transport scanout never makes one.
pub fn install() {
    set_copier(Some(Arc::new(Blitter {
        gpu: OnceLock::new(),
    })));
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::sync::mpsc;

    use objc2_core_foundation::{
        CFDictionary, CFNumber, CFNumberType, CFString, kCFTypeDictionaryKeyCallBacks,
        kCFTypeDictionaryValueCallBacks,
    };
    use objc2_io_surface::{
        kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat,
        kIOSurfaceWidth,
    };

    use super::*;

    fn bgra_surface(w: i32, h: i32) -> CFRetained<IOSurfaceRef> {
        let num = |v: i32| -> CFRetained<CFNumber> {
            // SAFETY: a pointer to an i32 for the SInt32 type.
            unsafe { CFNumber::new(None, CFNumberType::SInt32Type, (&v as *const i32).cast()) }
                .expect("a number")
        };
        let values = [
            num(w),
            num(h),
            num(4),
            num(w * 4),
            num(i32::from_be_bytes(*b"BGRA")),
        ];
        // SAFETY: framework key constants and live CFNumbers; the dictionary retains both.
        unsafe {
            let mut keys: [*const c_void; 5] = [
                (kIOSurfaceWidth as *const CFString).cast(),
                (kIOSurfaceHeight as *const CFString).cast(),
                (kIOSurfaceBytesPerElement as *const CFString).cast(),
                (kIOSurfaceBytesPerRow as *const CFString).cast(),
                (kIOSurfacePixelFormat as *const CFString).cast(),
            ];
            let mut vals: [*const c_void; 5] =
                values.each_ref().map(|v| (&**v as *const CFNumber).cast());
            let dict = CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                vals.as_mut_ptr(),
                keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .expect("a dictionary");
            IOSurfaceRef::new(&dict).expect("a surface")
        }
    }

    /// Live malloc blocks over every zone.
    fn blocks_in_use() -> u64 {
        // SAFETY: zeroed POD; a null zone asks for every zone's statistics, summed into `st`.
        let mut st: libc::malloc_statistics_t = unsafe { std::mem::zeroed() };
        unsafe { libc::malloc_zone_statistics(std::ptr::null_mut(), &mut st) };
        u64::from(st.blocks_in_use)
    }

    /// The copier runs on the virtio-gpu thread, which never drains an autorelease pool: whatever
    /// a copy autoreleases (the texture descriptors, the blit encoder) has to be released by the
    /// copy itself, or it lives as long as the thread. On the dogfood that was ~700 B a present,
    /// 2.4 GB in 16 hours. This thread, like that one, has no pool of its own.
    #[test]
    fn a_copy_leaves_nothing_behind_on_a_thread_without_a_pool() {
        std::thread::spawn(|| {
            let blitter = Blitter {
                gpu: OnceLock::new(),
            };
            if blitter.gpu().is_none() {
                eprintln!("no Metal device; skipped");
                return;
            }
            let (src, dst) = (bgra_surface(64, 8), bgra_surface(64, 8));
            let (tx, rx) = mpsc::channel();
            let copy = || {
                let tx = tx.clone();
                assert!(blitter.blit(&src, &dst, move |ok| tx.send(ok).unwrap()));
                assert!(rx.recv().expect("the blit completes"), "the blit completed");
            };
            for _ in 0..50 {
                copy();
            }
            const N: u64 = 1000;
            let before = blocks_in_use();
            for _ in 0..N {
                copy();
            }
            let grown = blocks_in_use().saturating_sub(before);
            assert!(
                grown < N / 2,
                "{grown} more live blocks after {N} copies: each copy left its autoreleased \
                 objects behind"
            );
        })
        .join()
        .expect("the copy thread");
    }
}
