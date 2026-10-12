//! Commands. Each is a `repr(C)` payload behind a [`CmdHeader`], optionally followed by bytes
//! whose lengths the payload states (MSL source, function names).
//!
//! Conventions:
//! - Handles are `u32`s the guest assigns per context, in one namespace for every object kind.
//!   `0` is "none".
//! - Enum-valued fields carry Metal's raw values (`MTLPixelFormat`, `MTLLoadAction`,
//!   `MTLPrimitiveType`, …). The replayer checks them against what it supports.
//! - Encoders are opened and closed explicitly. Commands between a `BEGIN_*` and
//!   [`END_ENCODING`](op::END_ENCODING) go to that encoder. Encoders accumulate into the ring's
//!   current command buffer until [`Commit`], which submits it and later publishes its seqno in
//!   [`RingControl::completed`](crate::ring::RingControl::completed).
//! - Work on one ring runs in ring order, encoder after encoder. Finer-grained synchronisation is a
//!   later protocol addition, not a different meaning of these commands.
//! - No field has implicit padding: a [`Command`]'s bytes are written to the ring as they are.

/// The header in front of every command.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CmdHeader {
    pub op: u16,
    pub flags: u16,
    /// Bytes from the start of this header to the start of the next one.
    pub len: u32,
}

/// Opcodes.
pub mod op {
    /// Skip `len` bytes. Fills the end of the data area before a wrap.
    pub const PAD: u16 = 0;

    pub const CREATE_TEXTURE: u16 = 0x0100;
    pub const CREATE_LIBRARY: u16 = 0x0101;
    pub const CREATE_RENDER_PIPELINE: u16 = 0x0102;
    pub const CREATE_COMPUTE_PIPELINE: u16 = 0x0103;
    pub const DESTROY: u16 = 0x01ff;

    pub const BEGIN_RENDER_PASS: u16 = 0x0200;
    pub const BEGIN_BLIT: u16 = 0x0201;
    pub const BEGIN_COMPUTE: u16 = 0x0202;
    pub const END_ENCODING: u16 = 0x020f;

    pub const SET_RENDER_PIPELINE: u16 = 0x0210;
    pub const SET_BUFFER: u16 = 0x0211;
    pub const SET_VIEWPORT: u16 = 0x0212;
    pub const DRAW_PRIMITIVES: u16 = 0x0213;

    pub const FILL_BUFFER: u16 = 0x0220;
    pub const COPY_BUFFER: u16 = 0x0221;
    pub const COPY_TEXTURE_TO_BUFFER: u16 = 0x0222;

    pub const SET_COMPUTE_PIPELINE: u16 = 0x0230;
    pub const DISPATCH_THREADGROUPS: u16 = 0x0231;

    pub const COMMIT: u16 = 0x0300;
}

/// A command payload.
///
/// # Safety
/// The type must be `repr(C)` with no implicit padding, and every bit pattern must be a valid
/// value: it is written to the ring as raw bytes and read back with an unaligned copy.
pub unsafe trait Command: Copy + 'static {
    const OP: u16;
}

/// Splits a payload into its fixed part and the bytes after it.
pub fn decode<T: Command>(payload: &[u8]) -> Option<(T, &[u8])> {
    if payload.len() < size_of::<T>() {
        return None;
    }
    // SAFETY: in bounds; every bit pattern is valid for a `Command`.
    let v = unsafe { payload.as_ptr().cast::<T>().read_unaligned() };
    Some((v, &payload[size_of::<T>()..]))
}

/// A range start in an arena: GPU address `arena base + offset`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufRef {
    pub arena: u32,
    pub _pad: u32,
    pub offset: u64,
}

impl BufRef {
    pub const fn new(arena: u32, offset: u64) -> Self {
        Self {
            arena,
            _pad: 0,
            offset,
        }
    }
}

/// Shader stages for [`SetBuffer`].
pub mod stage {
    pub const VERTEX: u32 = 0;
    pub const FRAGMENT: u32 = 1;
    pub const COMPUTE: u32 = 2;
}

macro_rules! commands {
    ($($name:ident = $op:ident;)*) => {
        $(
            // SAFETY: each type below is repr(C), integer/float fields only, padded explicitly;
            // the size assertions after it pin the layout.
            unsafe impl Command for $name {
                const OP: u16 = op::$op;
            }
        )*
    };
}

/// A texture placed in an arena: `MTLHeap newTextureWithDescriptor:offset:`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateTexture {
    pub handle: u32,
    pub arena: u32,
    pub offset: u64,
    /// `MTLTextureType`.
    pub texture_type: u32,
    /// `MTLPixelFormat`.
    pub pixel_format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mip_levels: u32,
    pub array_length: u32,
    pub sample_count: u32,
    /// `MTLTextureUsage` bits.
    pub usage: u32,
    pub _pad: u32,
}

/// An MSL library, compiled from the `source_len` bytes of UTF-8 that follow.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateLibrary {
    pub handle: u32,
    pub source_len: u32,
}

/// A render pipeline from two functions of one library; their names follow, vertex first.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateRenderPipeline {
    pub handle: u32,
    pub library: u32,
    pub vertex_name_len: u32,
    pub fragment_name_len: u32,
    /// `MTLPixelFormat` per color attachment; `0` (`Invalid`) for none.
    pub color_formats: [u32; MAX_COLOR_ATTACHMENTS],
    pub depth_format: u32,
    pub stencil_format: u32,
    pub sample_count: u32,
    pub _pad: u32,
}

/// A compute pipeline from one function of a library; its name follows.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateComputePipeline {
    pub handle: u32,
    pub library: u32,
    pub name_len: u32,
    pub _pad: u32,
}

/// Releases the host's reference to an object. Work already committed keeps using it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Destroy {
    pub handle: u32,
    pub _pad: u32,
}

pub const MAX_COLOR_ATTACHMENTS: usize = 8;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ColorAttachment {
    /// Texture handle; `0` leaves the attachment unused.
    pub texture: u32,
    pub level: u32,
    pub slice: u32,
    /// `MTLLoadAction`.
    pub load_action: u32,
    /// `MTLStoreAction`.
    pub store_action: u32,
    pub _pad: u32,
    pub clear: [f64; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BeginRenderPass {
    pub width: u32,
    pub height: u32,
    pub colors: [ColorAttachment; MAX_COLOR_ATTACHMENTS],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BeginBlit {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BeginCompute {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct EndEncoding {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetRenderPipeline {
    pub handle: u32,
    pub _pad: u32,
}

/// Binds an arena range at a buffer index of a stage of the open encoder.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetBuffer {
    /// One of [`stage`].
    pub stage: u32,
    pub index: u32,
    pub buffer: BufRef,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetViewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub znear: f32,
    pub zfar: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrawPrimitives {
    /// `MTLPrimitiveType`.
    pub primitive: u32,
    pub vertex_start: u32,
    pub vertex_count: u32,
    pub instance_count: u32,
    pub base_instance: u32,
    pub _pad: u32,
}

/// `fillBuffer:range:value:` over `length` bytes from `buffer`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FillBuffer {
    pub buffer: BufRef,
    pub length: u64,
    pub value: u32,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyBuffer {
    pub src: BufRef,
    pub dst: BufRef,
    pub length: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyTextureToBuffer {
    pub texture: u32,
    pub slice: u32,
    pub level: u32,
    pub _pad: u32,
    pub origin: [u32; 3],
    pub size: [u32; 3],
    pub dst: BufRef,
    pub bytes_per_row: u64,
    pub bytes_per_image: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetComputePipeline {
    pub handle: u32,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DispatchThreadgroups {
    pub groups: [u32; 3],
    pub threads_per_group: [u32; 3],
}

/// Submits the current command buffer; `seqno` is published when it finishes. Seqnos must
/// increase.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Commit {
    pub seqno: u64,
}

commands! {
    CreateTexture = CREATE_TEXTURE;
    CreateLibrary = CREATE_LIBRARY;
    CreateRenderPipeline = CREATE_RENDER_PIPELINE;
    CreateComputePipeline = CREATE_COMPUTE_PIPELINE;
    Destroy = DESTROY;
    BeginRenderPass = BEGIN_RENDER_PASS;
    BeginBlit = BEGIN_BLIT;
    BeginCompute = BEGIN_COMPUTE;
    EndEncoding = END_ENCODING;
    SetRenderPipeline = SET_RENDER_PIPELINE;
    SetBuffer = SET_BUFFER;
    SetViewport = SET_VIEWPORT;
    DrawPrimitives = DRAW_PRIMITIVES;
    FillBuffer = FILL_BUFFER;
    CopyBuffer = COPY_BUFFER;
    CopyTextureToBuffer = COPY_TEXTURE_TO_BUFFER;
    SetComputePipeline = SET_COMPUTE_PIPELINE;
    DispatchThreadgroups = DISPATCH_THREADGROUPS;
    Commit = COMMIT;
}

// Layouts are part of the protocol; a change here is a VERSION bump.
const _: () = {
    assert!(size_of::<CmdHeader>() == 8);
    assert!(size_of::<BufRef>() == 16);
    assert!(size_of::<CreateTexture>() == 56);
    assert!(size_of::<CreateLibrary>() == 8);
    assert!(size_of::<CreateRenderPipeline>() == 64);
    assert!(size_of::<CreateComputePipeline>() == 16);
    assert!(size_of::<Destroy>() == 8);
    assert!(size_of::<ColorAttachment>() == 56);
    assert!(size_of::<BeginRenderPass>() == 8 + 8 * 56);
    assert!(size_of::<EndEncoding>() == 0);
    assert!(size_of::<SetBuffer>() == 24);
    assert!(size_of::<SetViewport>() == 24);
    assert!(size_of::<DrawPrimitives>() == 24);
    assert!(size_of::<FillBuffer>() == 32);
    assert!(size_of::<CopyBuffer>() == 40);
    assert!(size_of::<CopyTextureToBuffer>() == 72);
    assert!(size_of::<DispatchThreadgroups>() == 24);
    assert!(size_of::<Commit>() == 8);
};
