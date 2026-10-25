//! Metal objects the GPU host owns outside any one command: the device, and arenas.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBuffer, MTLCreateSystemDefaultDevice, MTLDevice, MTLHazardTrackingMode, MTLHeap,
    MTLHeapDescriptor, MTLHeapType, MTLResourceOptions, MTLStorageMode,
};

pub type Device = ProtocolObject<dyn MTLDevice>;

/// Arena sizes are rounded up to the host page.
pub const PAGE: u64 = 16 * 1024;
/// Largest single arena. The guest asks for more arenas rather than bigger ones.
pub const MAX_ARENA: u64 = 4 << 30;

/// The options every arena and every resource placed in one is created with: CPU-visible,
/// untracked (the GPU host orders work itself), the KK allocation pattern.
pub fn arena_options() -> MTLResourceOptions {
    MTLResourceOptions::StorageModeShared | MTLResourceOptions::HazardTrackingModeUntracked
}

/// An IOSurface the worker handed over: textures can be made over it.
#[derive(Clone)]
pub struct Surface(pub objc2_core_foundation::CFRetained<objc2_io_surface::IOSurfaceRef>);
// SAFETY: IOSurfaces are thread-safe and reference counted atomically.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    /// The surface a send right names; the right is only borrowed.
    pub fn from_port(port: u32) -> Option<Self> {
        objc2_io_surface::IOSurfaceRef::lookup_from_mach_port(port).map(Surface)
    }

    /// A surface of the GPU host's own, `width` x `height` elements of `bytes_per_element`, rows
    /// at whatever pitch IOSurface aligns them to.
    pub fn new(width: u32, height: u32, bytes_per_element: u32) -> Option<Self> {
        use objc2_core_foundation::{
            CFDictionary, CFNumber, CFRetained, CFString, kCFTypeDictionaryKeyCallBacks,
            kCFTypeDictionaryValueCallBacks,
        };
        use objc2_io_surface::{
            IOSurfaceRef, kIOSurfaceBytesPerElement, kIOSurfaceHeight, kIOSurfaceWidth,
        };
        use std::ffi::c_void;

        let values: [CFRetained<CFNumber>; 3] = [
            CFNumber::new_i32(i32::try_from(width).ok()?),
            CFNumber::new_i32(i32::try_from(height).ok()?),
            CFNumber::new_i32(i32::try_from(bytes_per_element).ok()?),
        ];
        // SAFETY: framework key constants and live CFNumbers; the dictionary retains both.
        unsafe {
            let mut keys: [*const c_void; 3] = [
                (kIOSurfaceWidth as *const CFString).cast(),
                (kIOSurfaceHeight as *const CFString).cast(),
                (kIOSurfaceBytesPerElement as *const CFString).cast(),
            ];
            let mut vals: [*const c_void; 3] =
                values.each_ref().map(|v| (&**v as *const CFNumber).cast());
            let dict = CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                vals.as_mut_ptr(),
                keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )?;
            IOSurfaceRef::new(&dict).map(Surface)
        }
    }

    pub fn width(&self) -> u32 {
        self.0.width() as u32
    }

    pub fn height(&self) -> u32 {
        self.0.height() as u32
    }

    pub fn bytes_per_element(&self) -> u64 {
        self.0.bytes_per_element() as u64
    }

    /// The system-wide id, for logs only: it names the surface to a reader of the log, not to a
    /// process, which can look a non-global surface up by it only from its owner.
    pub fn id(&self) -> u32 {
        self.0.id()
    }
}

pub fn system_device() -> Option<Retained<Device>> {
    MTLCreateSystemDefaultDevice()
}

/// A placement heap and the buffer aliasing all of it.
pub struct Arena {
    pub heap: Retained<ProtocolObject<dyn MTLHeap>>,
    pub buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
    pub size: u64,
    pub gpu_base: u64,
}

// SAFETY: heaps and buffers are thread-safe to use from any thread; the arena is immutable after
// creation.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    pub fn new(device: &Device, size: u64) -> Option<Self> {
        let size = size.checked_next_multiple_of(PAGE)?;
        if size == 0 || size > MAX_ARENA || size > device.maxBufferLength() as u64 {
            return None;
        }
        let desc = MTLHeapDescriptor::new();
        desc.setType(MTLHeapType::Placement);
        desc.setStorageMode(MTLStorageMode::Shared);
        desc.setHazardTrackingMode(MTLHazardTrackingMode::Untracked);
        desc.setSize(size as usize);
        let heap = device.newHeapWithDescriptor(&desc)?;
        // SAFETY: offset 0 and the heap's full size: the alias every placed resource lives in.
        let buffer =
            unsafe { heap.newBufferWithLength_options_offset(size as usize, arena_options(), 0) }?;
        let gpu_base = buffer.gpuAddress();
        Some(Self {
            heap,
            buffer,
            size,
            gpu_base,
        })
    }

    /// Host address of the arena's first byte.
    pub fn contents(&self) -> *mut u8 {
        self.buffer.contents().as_ptr().cast()
    }

    /// Whether `[offset, offset + len)` lies inside the arena.
    pub fn contains(&self, offset: u64, len: u64) -> bool {
        offset.checked_add(len).is_some_and(|end| end <= self.size)
    }
}

/// How a pixel format lays out in memory: bytes per block and the block's size in texels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatInfo {
    pub bytes: u64,
    pub block_w: u32,
    pub block_h: u32,
}

const fn texel(bytes: u64) -> FormatInfo {
    FormatInfo {
        bytes,
        block_w: 1,
        block_h: 1,
    }
}

const fn block(bytes: u64, w: u32, h: u32) -> FormatInfo {
    FormatInfo {
        bytes,
        block_w: w,
        block_h: h,
    }
}

/// The `MTLPixelFormat`s the GPU host accepts, with their layout. A guest naming any other loses
/// its context: Metal aborts on some invalid formats rather than failing the call. Formats Apple
/// GPUs lack (PVRTC aside, which they have but no guest API asks for; Depth24Unorm_Stencil8;
/// the 4:2:2 formats) are left out.
///
/// The combined depth-stencil formats report their depth plane; copies name a plane with
/// `MTLBlitOption`, see [`copy_format_info`].
pub fn format_info(format: u32) -> Option<FormatInfo> {
    Some(match format {
        1 | 10..=14 | 253 => texel(1), // A8, R8*, Stencil8
        20 | 22..=25 | 30..=34 | 40..=43 | 250 => texel(2), // R16*, RG8*, 16-bit packed, Depth16
        53..=55 | 60 | 62..=65 | 70..=74 | 80 | 81 | 90..=94 | 252 | 261 => texel(4),
        554 | 555 => texel(4),                                // BGR10_XR
        103..=105 | 110 | 112..=115 | 552 | 553 => texel(8),  // RG32*, RGBA16*, BGRA10_XR
        260 => texel(4),        // Depth32Float_Stencil8: the depth plane
        123..=125 => texel(16), // RGBA32*
        130 | 131 | 140 | 141 => block(8, 4, 4), // BC1, BC4
        132..=135 | 142 | 143 | 150..=153 => block(16, 4, 4), // BC2, BC3, BC5, BC6H, BC7
        170 | 172 | 180..=183 => block(8, 4, 4), // EAC R11, ETC2 RGB8 and RGB8A1
        174 | 176 | 178 | 179 => block(16, 4, 4), // EAC RG11, EAC RGBA8
        186..=236 => {
            let (w, h) = astc_block((format - 186) % 18)?;
            block(16, w, h)
        }
        _ => return None,
    })
}

/// ASTC footprints, by the index of a format within its 18-value run (sRGB from 186, LDR from
/// 204, HDR from 222).
fn astc_block(i: u32) -> Option<(u32, u32)> {
    Some(match i {
        0 => (4, 4),
        1 => (5, 4),
        2 => (5, 5),
        3 => (6, 5),
        4 => (6, 6),
        6 => (8, 5),
        7 => (8, 6),
        8 => (8, 8),
        9 => (10, 5),
        10 => (10, 6),
        11 => (10, 8),
        12 => (10, 10),
        13 => (12, 10),
        14 => (12, 12),
        _ => return None,
    })
}

/// The layout a buffer<->texture copy with `MTLBlitOption` `options` uses for `format`, and the
/// option to hand Metal: one plane of a combined depth-stencil format, or the format itself. A
/// depth-only or stencil-only format named with its own plane is that format, copied with no
/// option; KosmicKrisp names the plane of every depth or stencil copy.
pub fn copy_format_info(format: u32, options: u32) -> Option<(FormatInfo, u32)> {
    match (format, options) {
        (_, 0) => format_info(format).map(|f| (f, 0)),
        (260, 1) => Some((texel(4), 1)), // DepthFromDepthStencil
        (260, 2) => Some((texel(1), 2)), // StencilFromDepthStencil
        (250 | 252, 1) | (253, 2) => format_info(format).map(|f| (f, 0)),
        _ => None,
    }
}

/// `MTLTextureType` values the GPU host accepts (1D through 2DMultisampleArray; not
/// TextureBuffer).
pub fn valid_texture_type(t: u32) -> bool {
    matches!(t, 0..=8)
}

/// `MTLTextureUsage` bits the GPU host passes through: ShaderRead, ShaderWrite, RenderTarget,
/// PixelFormatView, ShaderAtomic.
pub const TEXTURE_USAGE_MASK: u32 = 0x37;

#[cfg(test)]
mod tests {
    use objc2_metal::{MTL4CounterHeapDescriptor, MTL4CounterHeapType};

    use super::*;

    const MIB: u64 = 1 << 20;

    /// Measurement: the address space objects of each kind take from the arena region, from the
    /// gap between the arenas either side. 0 is none; an arena takes its size plus 0x8000. Run
    /// with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn footprints_of_small_objects() {
        let device = system_device().expect("device");
        let footprint = |make: &dyn Fn() -> Box<dyn std::any::Any>| {
            let a = Arena::new(&device, MIB).unwrap();
            let x = make();
            let b = Arena::new(&device, MIB).unwrap();
            let f = b.gpu_base.wrapping_sub(a.gpu_base + a.size + 0x8000);
            drop((a, x, b));
            f
        };
        for kib in [1u64, 4, 8, 16, 24, 32, 48, 64, 96, 128] {
            let len = (kib * 1024) as usize;
            let f = footprint(&|| {
                Box::new(
                    device
                        .newBufferWithLength_options(len, arena_options())
                        .unwrap(),
                )
            });
            eprintln!("buffer {kib:>4} KiB: {f:#x}");
        }
        for kib in [16u64, 32, 48, 64] {
            let f = footprint(&|| Box::new(Arena::new(&device, kib * 1024).unwrap()));
            eprintln!("arena  {kib:>4} KiB: {f:#x}");
        }
        for count in [1usize, 2, 64, 1024, 4096] {
            let f = footprint(&|| {
                let desc = MTL4CounterHeapDescriptor::new();
                desc.setType(MTL4CounterHeapType::Timestamp);
                // SAFETY: a positive count.
                unsafe { desc.setCount(count) };
                Box::new(device.newCounterHeapWithDescriptor_error(&desc).unwrap())
            });
            eprintln!("counter heap {count:>6}: {f:#x}");
        }
    }
}
