//! Commands. Each is a `repr(C)` payload behind a [`CmdHeader`], optionally followed by bytes
//! whose lengths the payload states (MSL source, function names, arrays).
//!
//! The model is Metal 4's, as KosmicKrisp's bridge drives it on the host:
//! - **Objects** (textures, views, samplers, libraries, pipelines, depth-stencil states, argument
//!   tables, fences, events) are named by `u32` handles the guest assigns per context, in one
//!   namespace for every kind. `0` is "none". Buffers have no objects: a buffer is a range of an
//!   arena, named by a [`BufRef`] or a GPU address (arena base + offset).
//! - **Command buffers** are explicit: [`BeginCommandBuffer`] … [`EndCommandBuffer`], with
//!   encoders opened and closed inside. [`Commit`] submits every command buffer ended since the
//!   last commit, in order, and later publishes its seqno in
//!   [`RingControl::completed`](crate::ring::RingControl::completed).
//! - **Ordering** is what the stream says: queue order across commits, and within a command buffer
//!   the barriers and fences it encodes, as in Metal 4. The replayer adds none.
//! - **Answers** come back through memory. A command that needs one carries a `reply`
//!   [`BufRef`] to a [`Reply`] the host fills and then marks done; a commit carries one to a
//!   [`Feedback`]. A create with a reply reports failure there and leaves the context alive; the
//!   same create without one loses the context instead.
//! - Enum-valued fields carry Metal's raw values (`MTLPixelFormat`, `MTLLoadAction`, `MTLStages`,
//!   …). The replayer checks them against what it supports.
//! - No field has implicit padding: a [`Command`]'s bytes are written to the ring as they are.
//!
//! Every struct here also exists in C, generated from these definitions
//! (`include/limina_gpu_wire.h`, kept current by a test).

/// The header in front of every command.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CmdHeader {
    pub op: u16,
    pub flags: u16,
    /// Bytes from the start of this header to the start of the next one.
    pub len: u32,
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

/// Splits `n` items of `T` off the front of `bytes`. `T` must be a wire struct.
pub fn decode_array<T: Wire>(
    bytes: &[u8],
    n: usize,
) -> Option<(impl Iterator<Item = T> + '_, &[u8])> {
    let len = n.checked_mul(size_of::<T>())?;
    if bytes.len() < len {
        return None;
    }
    let (head, rest) = bytes.split_at(len);
    // `as_chunks` needs the size as a const argument, which a generic `size_of` cannot be.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    let items = head.chunks_exact(size_of::<T>()).map(|c| {
        // SAFETY: each chunk is size_of::<T>() bytes; every bit pattern is valid for a wire struct.
        unsafe { c.as_ptr().cast::<T>().read_unaligned() }
    });
    Some((items, rest))
}

/// The raw bytes of a wire struct, for passing as a command's trailing data.
pub fn bytes_of<T: Wire>(v: &T) -> &[u8] {
    // SAFETY: wire structs have no padding, so every byte is initialised.
    unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

/// The raw bytes of a slice of wire structs.
pub fn bytes_of_slice<T: Wire>(v: &[T]) -> &[u8] {
    // SAFETY: as `bytes_of`, for each element; the slice is contiguous.
    unsafe { core::slice::from_raw_parts(v.as_ptr().cast::<u8>(), size_of_val(v)) }
}

/// A struct that is part of the protocol: `repr(C)`, no implicit padding, every bit pattern valid.
///
/// # Safety
/// As for [`Command`].
pub unsafe trait Wire: Copy + 'static {}

/// Description of one wire struct, for the C header.
#[cfg(test)]
pub(crate) struct CStruct {
    pub name: &'static str,
    pub doc: &'static str,
    pub size: usize,
    pub op: Option<(&'static str, u16)>,
    pub fields: &'static [CField],
}

#[cfg(test)]
pub(crate) struct CField {
    pub name: &'static str,
    pub ty: &'static str,
    pub offset: usize,
}

/// Declares wire structs (and, with `= OP(value)`, commands), the `op` module, and the
/// description the C header is generated from.
macro_rules! wire {
    (
        extra_ops { $($xop:ident = $xval:expr;)* }
        $(
            $(#[doc = $doc:literal])*
            struct $name:ident $(= $opname:ident($opval:expr))? {
                $( $(#[doc = $fdoc:literal])* $f:ident : $t:ty ),* $(,)?
            }
        )*
    ) => {
        /// Opcodes.
        pub mod op {
            $( pub const $xop: u16 = $xval; )*
            $( $( pub const $opname: u16 = $opval; )? )*
        }

        $(
            $(#[doc = $doc])*
            #[repr(C)]
            #[derive(Clone, Copy, Debug, Default, PartialEq)]
            pub struct $name {
                $( $(#[doc = $fdoc])* pub $f: $t, )*
            }

            // SAFETY: repr(C) of integers, floats, byte arrays and other wire structs; the test
            // `no_implicit_padding` checks that the fields add up to the size.
            unsafe impl Wire for $name {}

            $(
                // SAFETY: as for `Wire`.
                unsafe impl Command for $name {
                    const OP: u16 = op::$opname;
                }
            )?
        )*

        #[cfg(test)]
        pub(crate) const C_STRUCTS: &[CStruct] = &[
            $(
                CStruct {
                    name: stringify!($name),
                    doc: concat!($($doc, "\n",)*),
                    size: size_of::<$name>(),
                    op: wire!(@op $($opname, $opval)?),
                    fields: &[
                        $( CField {
                            name: stringify!($f),
                            ty: stringify!($t),
                            offset: core::mem::offset_of!($name, $f),
                        }, )*
                    ],
                },
            )*
        ];

        #[cfg(test)]
        pub(crate) const C_EXTRA_OPS: &[(&str, u16)] = &[ $( (stringify!($xop), $xval), )* ];
    };
    (@op $opname:ident, $opval:expr) => { Some((stringify!($opname), $opval)) };
    (@op) => { None };
}

/// Arena sizes are multiples of this, the host page.
pub const ARENA_PAGE: u64 = 16 * 1024;

/// Most color attachments a render pass or pipeline has.
pub const MAX_COLOR_ATTACHMENTS: usize = 8;
/// Most sample positions a render pass sets.
pub const MAX_SAMPLE_POSITIONS: usize = 16;

/// [`Reply::done`] once the host has written the reply.
pub const REPLY_DONE: u32 = 1;

/// [`SyncObject`] kinds.
pub mod sync_kind {
    pub const FENCE: u32 = 0;
    pub const EVENT: u32 = 1;
    pub const SHARED_EVENT: u32 = 2;
}

/// [`Barrier`] kinds: which `MTL4CommandEncoder` barrier to encode.
pub mod barrier_kind {
    /// `barrierAfterStages:beforeQueueStages:`
    pub const AFTER_STAGES: u32 = 0;
    /// `barrierAfterEncoderStages:beforeEncoderStages:`
    pub const AFTER_ENCODER_STAGES: u32 = 1;
    /// `barrierAfterQueueStages:beforeStages:`
    pub const AFTER_QUEUE_STAGES: u32 = 2;
}

/// [`RenderState`] kinds: the render encoder setter to call.
pub mod render_state {
    /// `setStencilFrontReferenceValue:backReferenceValue:` (`a`, `b`)
    pub const STENCIL_REFERENCE: u32 = 0;
    /// `setFrontFacingWinding:` (`a`)
    pub const WINDING: u32 = 1;
    /// `setCullMode:` (`a`)
    pub const CULL_MODE: u32 = 2;
    /// `setDepthClipMode:` (`a`)
    pub const DEPTH_CLIP_MODE: u32 = 3;
    /// `setDepthBias:slopeScale:clamp:` (`f[0..3]`)
    pub const DEPTH_BIAS: u32 = 4;
    /// `setDepthTestMinBound:maxBound:` (`f[0..2]`)
    pub const DEPTH_TEST_BOUNDS: u32 = 5;
    /// `setVisibilityResultMode:offset:` (`a`, `offset`), `offset` relative to the open pass's
    /// `BeginRenderPass::visibility`
    pub const VISIBILITY_RESULT: u32 = 6;
    /// `setColorStoreAction:atIndex:` (`a` = action, `b` = index)
    pub const COLOR_STORE_ACTION: u32 = 7;
    /// `setDepthStoreAction:` (`a`)
    pub const DEPTH_STORE_ACTION: u32 = 8;
    /// `setStencilStoreAction:` (`a`)
    pub const STENCIL_STORE_ACTION: u32 = 9;
}

wire! {
    extra_ops {
        PAD = 0x0000;
    }

    /// A range start in an arena: GPU address `arena base + offset`. Arena `0` is "none".
    struct BufRef {
        arena: u32,
        _pad: u32,
        offset: u64,
    }

    /// Written by the host for a command that names it. The host writes `status` and `values`
    /// first and `done` (= `REPLY_DONE`) last, with release ordering; the guest zeroes `done`
    /// before sending and reads the rest after seeing it set.
    struct Reply {
        done: u32,
        /// `0`, or the `ring::Error` the command failed with.
        status: u32,
        values: [u64; 6],
    }

    /// Written by the host for each [`Commit`] before its seqno is published, `done` last.
    struct Feedback {
        done: u32,
        /// `0`, or the `MTL4CommitFeedback` error code.
        error: u32,
        gpu_start: f64,
        gpu_end: f64,
    }

    /// What the guest needs to know about the host GPU, written by [`QueryDevice`].
    struct DeviceInfo {
        /// `crate::VERSION` of the replayer.
        version: u32,
        /// How many `MTLGPUFamilyApple*` families the device supports.
        apple_family: u32,
        /// `MTLDevice.name`, NUL-terminated.
        name: [u8; 32],
        /// `MTLDevice.architecture.name`, NUL-terminated.
        architecture: [u8; 32],
        registry_id: u64,
        /// Bit `n` is set if `supportsTextureSampleCount:n`.
        sample_counts: u32,
        max_threadgroup_memory: u32,
        max_threads_per_threadgroup: [u32; 3],
        max_argument_buffer_samplers: u32,
        max_buffer_length: u64,
        recommended_max_working_set: u64,
        timestamp_frequency: u64,
        sparse_tile_bytes: u32,
        /// `heapBufferSizeAndAlignWithLength:options:`'s alignment for the arena options; a
        /// buffer's size in an arena is its length.
        heap_buffer_align: u32,
        /// The host's macOS version (`NSProcessInfo.operatingSystemVersion`): Metal's behaviour
        /// changes with it, and KosmicKrisp works around some of it by version.
        os_version: [u32; 3],
        _pad: u32,
    }

    /// The fields of an `MTLTextureDescriptor` the guest chooses.
    struct TextureDesc {
        /// `MTLTextureType`.
        texture_type: u32,
        /// `MTLPixelFormat`.
        pixel_format: u32,
        width: u32,
        height: u32,
        depth: u32,
        mip_levels: u32,
        array_length: u32,
        sample_count: u32,
        /// `MTLTextureUsage` bits.
        usage: u32,
        /// `allowGPUOptimizedContents`.
        allow_gpu_optimized: u32,
        _pad: [u32; 2],
    }

    /// A texture in an arena. With `bytes_per_row` zero it is placed in the arena's heap
    /// (`newTextureWithDescriptor:offset:`); otherwise it views the arena's buffer linearly
    /// (`newTextureWithDescriptor:offset:bytesPerRow:`). The reply's `values[0]` is its
    /// `gpuResourceID`.
    struct CreateTexture = CREATE_TEXTURE(0x0100) {
        handle: u32,
        arena: u32,
        offset: u64,
        bytes_per_row: u64,
        desc: TextureDesc,
        reply: BufRef,
    }

    /// A texture whose storage is surface `surface` of the context (`newTextureWithDescriptor:
    /// iosurface:plane:`), as the worker filed it for a buffer shared beyond this transport: a 2D
    /// texture of one level and one sample, no larger than the surface, in the surface's own pixel
    /// layout. The reply's `values[0]` is its `gpuResourceID`.
    struct CreateTextureFromSurface = CREATE_TEXTURE_FROM_SURFACE(0x0109) {
        handle: u32,
        surface: u32,
        desc: TextureDesc,
        reply: BufRef,
    }

    /// `newTextureViewWithPixelFormat:textureType:levels:slices:[swizzle:]` of `parent`. The
    /// reply's `values[0]` is its `gpuResourceID`.
    struct CreateTextureView = CREATE_TEXTURE_VIEW(0x0101) {
        handle: u32,
        parent: u32,
        pixel_format: u32,
        texture_type: u32,
        base_level: u32,
        level_count: u32,
        base_slice: u32,
        slice_count: u32,
        /// `MTLTextureSwizzle` per channel, used when `use_swizzle` is non-zero.
        swizzle: [u32; 4],
        use_swizzle: u32,
        _pad: u32,
        reply: BufRef,
    }

    /// The fields of an `MTLSamplerDescriptor` the guest chooses.
    struct SamplerDesc {
        min_filter: u32,
        mag_filter: u32,
        mip_filter: u32,
        address_s: u32,
        address_t: u32,
        address_r: u32,
        border_color: u32,
        compare_function: u32,
        reduction_mode: u32,
        normalized_coordinates: u32,
        max_anisotropy: u32,
        lod_min_clamp: f32,
        lod_max_clamp: f32,
        _pad: u32,
    }

    /// An `MTLSamplerState`. The reply's `values[0]` is its `gpuResourceID`.
    struct CreateSampler = CREATE_SAMPLER(0x0102) {
        handle: u32,
        _pad: u32,
        desc: SamplerDesc,
        reply: BufRef,
    }

    /// An MSL library, compiled by the context's `MTL4Compiler` from the `source_len` bytes of
    /// UTF-8 that follow.
    struct CreateLibrary = CREATE_LIBRARY(0x0103) {
        handle: u32,
        source_len: u32,
        /// `MTLLanguageVersion`.
        language_version: u32,
        /// `MTLMathMode`.
        math_mode: u32,
        /// `MTLMathFloatingPointFunctions`.
        math_fp_functions: u32,
        _pad: u32,
        reply: BufRef,
    }

    /// An `MTL4RenderPipelineDescriptor`; the vertex function's name follows, then the
    /// fragment's (absent when `fragment_library` is `0`).
    struct CreateRenderPipeline = CREATE_RENDER_PIPELINE(0x0104) {
        handle: u32,
        vertex_library: u32,
        fragment_library: u32,
        vertex_name_len: u32,
        fragment_name_len: u32,
        /// `MTLPrimitiveTopologyClass`.
        topology_class: u32,
        raster_sample_count: u32,
        rasterization_enabled: u32,
        max_vertex_amplification: u32,
        _pad: u32,
        /// `MTLPixelFormat` per color attachment; `0` for none.
        color_formats: [u32; 8],
        reply: BufRef,
    }

    /// An `MTL4ComputePipelineDescriptor`; the function's name follows.
    struct CreateComputePipeline = CREATE_COMPUTE_PIPELINE(0x0105) {
        handle: u32,
        library: u32,
        name_len: u32,
        _pad: u32,
        max_total_threads: u64,
        reply: BufRef,
    }

    /// An `MTLStencilDescriptor`, with `MTLStencilOperation` and `MTLCompareFunction` values.
    struct StencilDesc {
        stencil_failure: u32,
        depth_failure: u32,
        depth_stencil_pass: u32,
        compare_function: u32,
        read_mask: u32,
        write_mask: u32,
    }

    /// An `MTLDepthStencilState`.
    struct CreateDepthStencil = CREATE_DEPTH_STENCIL(0x0106) {
        handle: u32,
        depth_compare: u32,
        depth_write: u32,
        has_front: u32,
        has_back: u32,
        _pad: u32,
        front: StencilDesc,
        back: StencilDesc,
    }

    /// An `MTL4ArgumentTable` with `max_buffer_bind_count` buffer bindings.
    struct CreateArgumentTable = CREATE_ARGUMENT_TABLE(0x0107) {
        handle: u32,
        max_buffer_bind_count: u32,
    }

    /// An `MTLFence`, `MTLEvent` or `MTLSharedEvent` (`kind`, one of `sync_kind`). A shared
    /// event starts at `initial`, and the host keeps the `u64` at `mirror` at least as high as its
    /// signaled value, which is how the guest reads and waits on it.
    struct SyncObject = CREATE_SYNC(0x0108) {
        handle: u32,
        kind: u32,
        initial: u64,
        mirror: BufRef,
    }

    /// Releases the guest's handle. Metal 4 does not retain what command buffers reference, so
    /// the host keeps the object until every commit made before the destroy has completed; the
    /// handle is free for reuse at once.
    struct Destroy = DESTROY(0x01ff) {
        handle: u32,
        _pad: u32,
    }

    /// One `setAddress:atIndex:` of [`SetAddresses`].
    struct AddressBinding {
        index: u32,
        _pad: u32,
        address: u64,
    }

    /// `count` [`AddressBinding`]s follow, applied to argument table `table` in order.
    struct SetAddresses = SET_ADDRESSES(0x0180) {
        table: u32,
        count: u32,
    }

    /// `MTLSharedEvent.signaledValue = value`, from the CPU.
    struct SetSharedEvent = SET_SHARED_EVENT(0x0181) {
        handle: u32,
        _pad: u32,
        value: u64,
    }

    /// `getBytes:` (`to_texture` zero) or `replaceRegion:` of a texture region, from or into
    /// arena memory at `data`, on the CPU when the command executes.
    struct TextureTransfer = TEXTURE_TRANSFER(0x0182) {
        texture: u32,
        to_texture: u32,
        level: u32,
        slice: u32,
        origin: [u32; 3],
        size: [u32; 3],
        bytes_per_row: u64,
        bytes_per_image: u64,
        data: BufRef,
        reply: BufRef,
    }

    /// Writes a [`DeviceInfo`] at `dst`.
    struct QueryDevice = QUERY_DEVICE(0x0190) {
        dst: BufRef,
        reply: BufRef,
    }

    /// `heapTextureSizeAndAlignWithDescriptor:`; reply `values[0]` is the size, `[1]` the
    /// alignment.
    struct QueryTextureSize = QUERY_TEXTURE_SIZE(0x0191) {
        desc: TextureDesc,
        reply: BufRef,
    }

    /// `minimumLinearTextureAlignmentForPixelFormat:`; reply `values[0]`.
    struct QueryLinearAlignment = QUERY_LINEAR_ALIGNMENT(0x0192) {
        pixel_format: u32,
        _pad: u32,
        reply: BufRef,
    }

    /// `sampleTimestamps:gpuTimestamp:`; reply `values[0]` is the GPU timestamp, `[1]` the CPU's.
    struct QueryTimestamp = QUERY_TIMESTAMP(0x0193) {
        reply: BufRef,
    }

    /// Opens a command buffer on a pooled allocator. One is open at a time.
    struct BeginCommandBuffer = BEGIN_COMMAND_BUFFER(0x0200) {}

    /// Closes the open command buffer; the next [`Commit`] submits it.
    struct EndCommandBuffer = END_COMMAND_BUFFER(0x0201) {}

    /// Opens an `MTL4ComputeCommandEncoder`, for dispatches and copies alike.
    struct BeginCompute = BEGIN_COMPUTE(0x0202) {}

    /// One attachment of a [`BeginRenderPass`]. `clear` is RGBA for a color attachment and
    /// `clear[0]` the depth for the depth attachment.
    struct RenderAttachment {
        /// Texture handle; `0` leaves the attachment unused.
        texture: u32,
        level: u32,
        slice: u32,
        depth_plane: u32,
        /// `MTLLoadAction`.
        load_action: u32,
        /// `MTLStoreAction`.
        store_action: u32,
        resolve_texture: u32,
        resolve_level: u32,
        resolve_slice: u32,
        resolve_depth_plane: u32,
        /// `MTLMultisampleDepthResolveFilter` or `MTLMultisampleStencilResolveFilter`.
        resolve_filter: u32,
        clear_stencil: u32,
        clear: [f64; 4],
    }

    /// Opens an `MTL4RenderCommandEncoder` on an `MTL4RenderPassDescriptor`.
    struct BeginRenderPass = BEGIN_RENDER_PASS(0x0203) {
        width: u32,
        height: u32,
        array_length: u32,
        default_raster_sample_count: u32,
        sample_position_count: u32,
        _pad: u32,
        /// `visibilityResultBuffer`; arena `0` for none.
        visibility: BufRef,
        /// `x, y` pairs.
        sample_positions: [f32; 32],
        colors: [RenderAttachment; 8],
        depth: RenderAttachment,
        stencil: RenderAttachment,
    }

    /// `endEncoding` of the open encoder.
    struct EndEncoding = END_ENCODING(0x0204) {}

    /// A barrier of kind `kind` (one of `barrier_kind`), with `MTLStages` masks.
    struct Barrier = BARRIER(0x0205) {
        kind: u32,
        after: u32,
        before: u32,
        /// `MTL4VisibilityOptions`.
        visibility: u32,
    }

    /// `updateFence:afterEncoderStages:` (`wait` zero) or `waitForFence:beforeEncoderStages:`.
    struct FenceOp = FENCE(0x0206) {
        fence: u32,
        stages: u32,
        wait: u32,
        _pad: u32,
    }

    /// `setComputePipelineState:`.
    struct SetComputePipeline = SET_COMPUTE_PIPELINE(0x0210) {
        handle: u32,
        _pad: u32,
    }

    /// `setArgumentTable:` on a compute encoder, or `setArgumentTable:atStages:` with
    /// `MTLRenderStages` `stages` on a render encoder.
    struct SetArgumentTable = SET_ARGUMENT_TABLE(0x0211) {
        table: u32,
        stages: u32,
    }

    /// `dispatchThreads:threadsPerThreadgroup:`.
    struct DispatchThreads = DISPATCH_THREADS(0x0212) {
        grid: [u32; 3],
        threads_per_group: [u32; 3],
    }

    /// `dispatchThreadgroups:threadsPerThreadgroup:`.
    struct DispatchThreadgroups = DISPATCH_THREADGROUPS(0x0213) {
        groups: [u32; 3],
        threads_per_group: [u32; 3],
    }

    /// `dispatchThreadgroupsWithIndirectBuffer:threadsPerThreadgroup:`.
    struct DispatchIndirect = DISPATCH_INDIRECT(0x0214) {
        threads_per_group: [u32; 3],
        _pad: u32,
        address: u64,
    }

    /// `copyFromBuffer:sourceOffset:toBuffer:destinationOffset:size:`.
    struct CopyBuffer = COPY_BUFFER(0x0215) {
        src: BufRef,
        dst: BufRef,
        length: u64,
    }

    /// Copies between a buffer and a texture region, in the direction `to_texture` says, with
    /// `MTLBlitOption` `options`.
    struct CopyBufferTexture = COPY_BUFFER_TEXTURE(0x0216) {
        texture: u32,
        to_texture: u32,
        level: u32,
        slice: u32,
        options: u32,
        origin: [u32; 3],
        size: [u32; 3],
        _pad: u32,
        buffer: BufRef,
        bytes_per_row: u64,
        bytes_per_image: u64,
    }

    /// `copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:…`.
    struct CopyTexture = COPY_TEXTURE(0x0217) {
        src: u32,
        src_slice: u32,
        src_level: u32,
        dst: u32,
        dst_slice: u32,
        dst_level: u32,
        src_origin: [u32; 3],
        src_size: [u32; 3],
        dst_origin: [u32; 3],
        _pad: u32,
    }

    /// `fillBuffer:range:value:` over `length` bytes from `buffer`.
    struct FillBuffer = FILL_BUFFER(0x0218) {
        buffer: BufRef,
        length: u64,
        value: u32,
        _pad: u32,
    }

    /// `setRenderPipelineState:`.
    struct SetRenderPipeline = SET_RENDER_PIPELINE(0x0220) {
        handle: u32,
        _pad: u32,
    }

    /// `setViewports:count:`; `count` [`Viewport`]s follow.
    struct SetViewports = SET_VIEWPORTS(0x0221) {
        count: u32,
        _pad: u32,
    }

    /// An `MTLViewport`.
    struct Viewport {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        znear: f64,
        zfar: f64,
    }

    /// `setScissorRects:count:`; `count` [`Scissor`]s follow.
    struct SetScissors = SET_SCISSORS(0x0222) {
        count: u32,
        _pad: u32,
    }

    /// An `MTLScissorRect`.
    struct Scissor {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    }

    /// `setDepthStencilState:`.
    struct SetDepthStencil = SET_DEPTH_STENCIL(0x0223) {
        handle: u32,
        _pad: u32,
    }

    /// One fixed-function setter of the render encoder (`kind`, one of `render_state`).
    struct RenderState = RENDER_STATE(0x0224) {
        kind: u32,
        a: u32,
        b: u32,
        _pad: u32,
        offset: u64,
        f: [f32; 4],
    }

    /// `setVertexAmplificationCount:viewMappings:`; `count` `u32` render-target array index
    /// offsets follow.
    struct SetAmplification = SET_AMPLIFICATION(0x0225) {
        count: u32,
        _pad: u32,
    }

    /// `drawPrimitives:vertexStart:vertexCount:instanceCount:baseInstance:`.
    struct Draw = DRAW(0x0230) {
        /// `MTLPrimitiveType`.
        primitive: u32,
        vertex_start: u32,
        vertex_count: u32,
        instance_count: u32,
        base_instance: u32,
        _pad: u32,
    }

    /// `drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferLength:…`.
    struct DrawIndexed = DRAW_INDEXED(0x0231) {
        primitive: u32,
        index_count: u32,
        /// `MTLIndexType`.
        index_type: u32,
        instance_count: u32,
        base_vertex: i32,
        base_instance: u32,
        index_address: u64,
        index_length: u64,
    }

    /// `drawPrimitives:indirectBuffer:`, or with `indexed` non-zero
    /// `drawIndexedPrimitives:indexType:indexBuffer:indexBufferLength:indirectBuffer:`.
    struct DrawIndirect = DRAW_INDIRECT(0x0232) {
        primitive: u32,
        indexed: u32,
        index_type: u32,
        _pad: u32,
        index_address: u64,
        index_length: u64,
        indirect: u64,
    }

    /// Submits the command buffers ended since the last commit, in one `commit:count:options:`.
    /// The host fills `feedback` (arena `0` for none) and then publishes `seqno`. Seqnos must
    /// increase. A commit with nothing to submit commits an empty command buffer.
    struct Commit = COMMIT(0x0300) {
        seqno: u64,
        feedback: BufRef,
    }

    /// `signalEvent:value:` (`wait` zero) or `waitForEvent:value:` on the queue, between
    /// commits: no command buffer may be ended and not yet committed. A signal of a shared event
    /// also updates its mirror once the value is reached.
    struct QueueEvent = QUEUE_EVENT(0x0301) {
        event: u32,
        wait: u32,
        value: u64,
    }
}

impl BufRef {
    pub const fn new(arena: u32, offset: u64) -> Self {
        Self {
            arena,
            _pad: 0,
            offset,
        }
    }

    pub const NONE: Self = Self::new(0, 0);

    pub const fn is_none(&self) -> bool {
        self.arena == 0
    }
}

// Layouts are part of the protocol; a change here is a VERSION bump.
const _: () = {
    assert!(size_of::<CmdHeader>() == 8);
    assert!(size_of::<BufRef>() == 16);
    assert!(size_of::<Reply>() == 56);
    assert!(size_of::<Feedback>() == 24);
    assert!(size_of::<TextureDesc>() == 48);
    assert!(size_of::<RenderAttachment>() == 80);
    assert!(size_of::<BeginRenderPass>() == 40 + 128 + 10 * 80);
    assert!(size_of::<EndEncoding>() == 0);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// No two commands share an op: a duplicate would make the later one's handler unreachable.
    #[test]
    fn ops_are_unique() {
        let ops = || C_STRUCTS.iter().filter_map(|s| s.op);
        for (i, (name, op)) in ops().enumerate() {
            if let Some((other, _)) = ops().skip(i + 1).find(|(_, o)| *o == op) {
                panic!("{name} and {other} are both op {op:#06x}");
            }
        }
    }

    /// Every wire struct's fields add up to its size, so there is no implicit padding anywhere.
    #[test]
    fn no_implicit_padding() {
        for s in C_STRUCTS {
            let mut end = 0;
            for f in s.fields {
                assert_eq!(
                    f.offset, end,
                    "{}.{} leaves a gap before it",
                    s.name, f.name
                );
                end = f.offset + c_size(f.ty);
            }
            assert_eq!(end, s.size, "{} has trailing padding", s.name);
        }
    }

    /// Size of a field type as it appears in the macro.
    pub(super) fn c_size(ty: &str) -> usize {
        let ty = ty.replace(' ', "");
        if let Some(inner) = ty.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            let (elem, n) = inner.split_once(';').expect("array type");
            return c_size(elem) * n.parse::<usize>().expect("array length");
        }
        match ty.as_str() {
            "u8" => 1,
            "u16" => 2,
            "u32" | "i32" | "f32" => 4,
            "u64" | "i64" | "f64" => 8,
            name => {
                C_STRUCTS
                    .iter()
                    .find(|s| s.name == name)
                    .unwrap_or_else(|| panic!("unknown field type {name}"))
                    .size
            }
        }
    }
}
