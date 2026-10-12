//! Metal objects the replayer owns outside any one command: the device, and arenas.

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
/// untracked (the replayer orders work itself), the KK allocation pattern.
pub fn arena_options() -> MTLResourceOptions {
    MTLResourceOptions::StorageModeShared | MTLResourceOptions::HazardTrackingModeUntracked
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

/// The pixel formats the replayer accepts, with their bytes per texel. A guest naming any other
/// loses its context: Metal aborts on some invalid formats rather than failing the call.
pub fn bytes_per_texel(format: u32) -> Option<u64> {
    Some(match format {
        10 | 13 => 1,                                         // R8Unorm, R8Uint
        25 | 30 => 2,                                         // R16Float, RG8Unorm
        53 | 54 | 55 | 65 | 70 | 71 | 73 | 80 | 81 | 90 => 4, // R32*, RG16Float, RGBA8*, BGRA8*, RGB10A2
        252 => 4,                                             // Depth32Float
        105 | 115 => 8,                                       // RG32Float, RGBA16Float
        125 => 16,                                            // RGBA32Float
        _ => return None,
    })
}

/// `MTLTextureType` values the replayer accepts.
pub fn valid_texture_type(t: u32) -> bool {
    matches!(t, 0..=7)
}

/// `MTLTextureUsage` bits the replayer passes through.
pub const TEXTURE_USAGE_MASK: u32 = 0x17;
