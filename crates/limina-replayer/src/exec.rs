//! A context's ring consumer: decodes commands and replays them on Metal.
//!
//! Every value from the ring is checked before Metal sees it. Metal answers many invalid calls by
//! aborting the process, which a lost context is cheaper than, even with the replayer isolated.

use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use limina_gpu_wire::cmd::{self, BufRef, Command, op};
use limina_gpu_wire::ring::{Consumer, Error, RingControl};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSRange, NSString};
use objc2_metal::{
    MTL4ArgumentTable, MTL4ArgumentTableDescriptor, MTL4CommandAllocator, MTL4CommandBuffer,
    MTL4CommandEncoder, MTL4CommandQueue, MTL4CommitFeedback, MTL4CommitOptions, MTL4Compiler,
    MTL4CompilerDescriptor, MTL4ComputeCommandEncoder, MTL4ComputePipelineDescriptor,
    MTL4FunctionDescriptor, MTL4LibraryDescriptor, MTL4LibraryFunctionDescriptor,
    MTL4RenderCommandEncoder, MTL4RenderPassDescriptor, MTL4RenderPipelineDescriptor,
    MTL4VisibilityOptions, MTLAllocation, MTLClearColor, MTLComputePipelineState, MTLDevice,
    MTLHeap, MTLLibrary, MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLPrimitiveType,
    MTLRenderPipelineState, MTLRenderStages, MTLResidencySet, MTLResidencySetDescriptor, MTLSize,
    MTLStages, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
    MTLViewport,
};

use crate::mach::Semaphore;
use crate::metal::{self, Arena, Device};

/// How long an idle consumer polls before it parks and needs a doorbell. Short: a doorbell costs
/// the guest ~1 µs plus a wake, so spinning longer buys little and burns a core.
const SPIN: Duration = Duration::from_micros(50);
/// Largest Metal buffer index a command may bind, and the size of every argument table.
const MAX_BUFFER_INDEX: u32 = 30;
/// Largest texture dimension accepted.
const MAX_DIM: u32 = 16384;
/// Every stage a queue barrier can name: KosmicKrisp's `MTL_STAGE_ALL`.
const ALL_STAGES: MTLStages = MTLStages(
    MTLStages::Vertex.0
        | MTLStages::Fragment.0
        | MTLStages::Tile.0
        | MTLStages::Object.0
        | MTLStages::Mesh.0
        | MTLStages::ResourceState.0
        | MTLStages::Dispatch.0
        | MTLStages::Blit.0
        | MTLStages::AccelerationStructure.0
        | MTLStages::MachineLearning.0,
);

pub type Arenas = Arc<Mutex<HashMap<u32, Arc<Arena>>>>;

type Po<T> = Retained<ProtocolObject<T>>;

enum Object {
    Texture(Po<dyn MTLTexture>),
    Library(Po<dyn MTLLibrary>),
    RenderPipeline(Po<dyn MTLRenderPipelineState>),
    ComputePipeline(Po<dyn MTLComputePipelineState>),
}

enum Encoder {
    None,
    Render {
        enc: Po<dyn MTL4RenderCommandEncoder>,
        pipeline: bool,
    },
    /// Metal 4 has one compute encoder for dispatches and copies alike; `copies` marks one begun
    /// for copies, which takes no pipeline.
    Compute {
        enc: Po<dyn MTL4ComputeCommandEncoder>,
        max_threads: u64,
        copies: bool,
    },
}

/// A command allocator, and the pool it returns to when the GPU is done with what it holds.
struct Allocator(Po<dyn MTL4CommandAllocator>);
// SAFETY: Metal command allocators may be used from any thread, one at a time; the pool hands each
// to one command buffer at a time.
unsafe impl Send for Allocator {}

type Pool = Arc<Mutex<Vec<Allocator>>>;

struct SendPtr(NonNull<RingControl>);
// SAFETY: the control block is shared memory meant to be written from any thread.
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

/// The argument tables an encoder binds: one per stage that takes buffers.
struct Tables {
    vertex: Po<dyn MTL4ArgumentTable>,
    fragment: Po<dyn MTL4ArgumentTable>,
    compute: Po<dyn MTL4ArgumentTable>,
}

pub struct Executor {
    device: Retained<Device>,
    queue: Po<dyn MTL4CommandQueue>,
    compiler: Po<dyn MTL4Compiler>,
    /// Holds every arena of the context; the queue keeps it resident.
    residency: Po<dyn MTLResidencySet>,
    /// The arenas `residency` holds, by id, as of the last command buffer begun.
    resident: HashMap<u32, Arc<Arena>>,
    allocators: Pool,
    tables: Tables,
    consumer: Consumer,
    /// The arena the ring lives in: its memory must outlive the consumer.
    ring_arena: Arc<Arena>,
    arenas: Arenas,
    objects: HashMap<u32, Object>,
    cb: Option<(Po<dyn MTL4CommandBuffer>, Allocator)>,
    enc: Encoder,
    last_seqno: u64,
    lost: bool,
}

// SAFETY: the executor runs on one thread at a time; Metal objects are usable from any thread.
unsafe impl Send for Executor {}

type R = Result<(), Error>;

fn arg<T: Command>(p: &[u8]) -> Result<(T, &[u8]), Error> {
    cmd::decode::<T>(p).ok_or(Error::BadPayload)
}

fn take(rest: &[u8], len: u32) -> Result<(&[u8], &[u8]), Error> {
    let len = len as usize;
    if rest.len() < len {
        return Err(Error::BadPayload);
    }
    Ok(rest.split_at(len))
}

fn text(bytes: &[u8]) -> Result<Retained<NSString>, Error> {
    let s = std::str::from_utf8(bytes).map_err(|_| Error::BadPayload)?;
    Ok(NSString::from_str(s))
}

fn argument_table(device: &Device) -> Option<Po<dyn MTL4ArgumentTable>> {
    let desc = MTL4ArgumentTableDescriptor::new();
    desc.setMaxBufferBindCount(MAX_BUFFER_INDEX as usize + 1);
    device.newArgumentTableWithDescriptor_error(&desc).ok()
}

fn function(
    lib: &Po<dyn MTLLibrary>,
    name: &[u8],
) -> Result<Retained<MTL4FunctionDescriptor>, Error> {
    let f = MTL4LibraryFunctionDescriptor::new();
    f.setName(Some(&*text(name)?));
    f.setLibrary(Some(lib));
    Ok(Retained::into_super(f))
}

impl Executor {
    /// `consumer` must be attached to a ring inside `ring_arena`.
    pub fn new(
        device: Retained<Device>,
        consumer: Consumer,
        ring_arena: Arc<Arena>,
        arenas: Arenas,
    ) -> Option<Self> {
        let queue = device.newMTL4CommandQueue()?;
        let compiler = device
            .newCompilerWithDescriptor_error(&MTL4CompilerDescriptor::new())
            .ok()?;
        let residency = device
            .newResidencySetWithDescriptor_error(&MTLResidencySetDescriptor::new())
            .ok()?;
        queue.addResidencySet(&residency);
        let tables = Tables {
            vertex: argument_table(&device)?,
            fragment: argument_table(&device)?,
            compute: argument_table(&device)?,
        };
        Some(Self {
            ring_arena,
            queue,
            compiler,
            residency,
            resident: HashMap::new(),
            allocators: Arc::new(Mutex::new(Vec::new())),
            tables,
            device,
            consumer,
            arenas,
            objects: HashMap::new(),
            cb: None,
            enc: Encoder::None,
            last_seqno: 0,
            lost: false,
        })
    }

    /// Drains the ring until `stop`, parking on `doorbell` when idle.
    pub fn run(mut self, doorbell: &Semaphore, stop: &AtomicBool) {
        // Commands are at most half the data area (the producer enforces it; the consumer checks).
        let mut scratch = vec![0u8; 1 << 20];
        let mut idle_since: Option<Instant> = None;
        while !stop.load(Ordering::Acquire) {
            match self.consumer.next(&mut scratch) {
                Ok(Some(n)) => {
                    idle_since = None;
                    if self.lost {
                        continue;
                    }
                    if let Err(e) = self.exec(n.op, n.payload) {
                        log::warn!("context lost: {e:?} at ring position {}", n.at);
                        self.lose(e, n.at);
                    }
                }
                Ok(None) => {
                    let since = *idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() < SPIN {
                        std::hint::spin_loop();
                        continue;
                    }
                    if self.consumer.park() {
                        doorbell.wait();
                        self.consumer.unpark();
                    }
                    idle_since = None;
                }
                Err(e) => {
                    log::warn!("ring broken: {e:?}");
                    self.lose(e, self.consumer.position());
                    // The ring cannot be parsed any further; sleep until destroyed.
                    while !stop.load(Ordering::Acquire) {
                        doorbell.wait();
                    }
                }
            }
        }
        self.end_encoder();
    }

    fn lose(&mut self, e: Error, at: u64) {
        self.consumer.control().fail(e, at);
        self.lost = true;
        self.end_encoder();
        // An open command buffer is dropped uncommitted; its allocator returns to the pool.
        if let Some((cb, alloc)) = self.cb.take() {
            cb.endCommandBuffer();
            self.allocators
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(alloc);
        }
        self.objects.clear();
    }

    fn arena(&self, id: u32) -> Result<Arc<Arena>, Error> {
        self.arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
            .ok_or(Error::BadRange)
    }

    /// The arena behind `r`, checked to hold `len` bytes from `r.offset`.
    fn range(&self, r: BufRef, len: u64) -> Result<Arc<Arena>, Error> {
        let a = self.arena(r.arena)?;
        if !a.contains(r.offset, len) {
            return Err(Error::BadRange);
        }
        Ok(a)
    }

    fn new_handle(&self, h: u32) -> R {
        if h == 0 || self.objects.contains_key(&h) {
            return Err(Error::BadHandle);
        }
        Ok(())
    }

    fn texture(&self, h: u32) -> Result<&Po<dyn MTLTexture>, Error> {
        match self.objects.get(&h) {
            Some(Object::Texture(t)) => Ok(t),
            _ => Err(Error::BadHandle),
        }
    }

    fn library(&self, h: u32) -> Result<&Po<dyn MTLLibrary>, Error> {
        match self.objects.get(&h) {
            Some(Object::Library(l)) => Ok(l),
            _ => Err(Error::BadHandle),
        }
    }

    fn snapshot_arenas(&self) -> HashMap<u32, Arc<Arena>> {
        self.arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Brings the residency set in line with the context's arenas. Arenas come and go between
    /// command buffers, never within one, so this runs as each begins.
    fn sync_residency(&mut self) {
        let now = self.snapshot_arenas();
        let mut changed = false;
        self.resident.retain(|id, a| {
            let keep = now.get(id).is_some_and(|b| Arc::ptr_eq(a, b));
            if !keep {
                self.residency
                    .removeAllocation(ProtocolObject::<dyn MTLAllocation>::from_ref(&*a.heap));
                changed = true;
            }
            keep
        });
        for (id, a) in now {
            if !self.resident.contains_key(&id) {
                self.residency
                    .addAllocation(ProtocolObject::<dyn MTLAllocation>::from_ref(&*a.heap));
                self.resident.insert(id, a);
                changed = true;
            }
        }
        if changed {
            self.residency.commit();
            self.residency.requestResidency();
        }
    }

    fn command_buffer(&mut self) -> Result<Po<dyn MTL4CommandBuffer>, Error> {
        if let Some((cb, _)) = &self.cb {
            return Ok(cb.clone());
        }
        self.sync_residency();
        let alloc = match self
            .allocators
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
        {
            Some(a) => {
                a.0.reset();
                a
            }
            None => Allocator(
                self.device
                    .newCommandAllocator()
                    .ok_or(Error::OutOfMemory)?,
            ),
        };
        let cb = self.device.newCommandBuffer().ok_or(Error::OutOfMemory)?;
        cb.beginCommandBufferWithAllocator(&alloc.0);
        self.cb = Some((cb.clone(), alloc));
        Ok(cb)
    }

    fn end_encoder(&mut self) {
        match std::mem::replace(&mut self.enc, Encoder::None) {
            Encoder::None => {}
            Encoder::Render { enc, .. } => enc.endEncoding(),
            Encoder::Compute { enc, .. } => enc.endEncoding(),
        }
    }

    /// Orders an encoder's work after everything committed or encoded before it on the queue.
    /// Coarse, and correct for a stream that names no dependencies of its own.
    fn wait_for_prior(enc: &ProtocolObject<dyn MTL4CommandEncoder>) {
        enc.barrierAfterQueueStages_beforeStages_visibilityOptions(
            ALL_STAGES,
            ALL_STAGES,
            MTL4VisibilityOptions::Device,
        );
    }

    fn begin_compute(&mut self, copies: bool) -> R {
        self.require_no_encoder()?;
        let cb = self.command_buffer()?;
        let enc = cb.computeCommandEncoder().ok_or(Error::OutOfMemory)?;
        Self::wait_for_prior(ProtocolObject::from_ref(&*enc));
        enc.setArgumentTable(Some(&self.tables.compute));
        self.enc = Encoder::Compute {
            enc,
            max_threads: 0,
            copies,
        };
        Ok(())
    }

    fn copy_encoder(&self) -> Result<&Po<dyn MTL4ComputeCommandEncoder>, Error> {
        match &self.enc {
            Encoder::Compute {
                enc, copies: true, ..
            } => Ok(enc),
            _ => Err(Error::BadState),
        }
    }

    fn exec(&mut self, opcode: u16, p: &[u8]) -> R {
        match opcode {
            op::CREATE_TEXTURE => self.create_texture(arg(p)?.0),
            op::CREATE_LIBRARY => {
                let (c, rest) = arg::<cmd::CreateLibrary>(p)?;
                let (src, _) = take(rest, c.source_len)?;
                self.create_library(c.handle, src)
            }
            op::CREATE_RENDER_PIPELINE => {
                let (c, rest) = arg::<cmd::CreateRenderPipeline>(p)?;
                let (v, rest) = take(rest, c.vertex_name_len)?;
                let (f, _) = take(rest, c.fragment_name_len)?;
                self.create_render_pipeline(&c, v, f)
            }
            op::CREATE_COMPUTE_PIPELINE => {
                let (c, rest) = arg::<cmd::CreateComputePipeline>(p)?;
                let (name, _) = take(rest, c.name_len)?;
                self.create_compute_pipeline(c.handle, c.library, name)
            }
            op::DESTROY => {
                let h = arg::<cmd::Destroy>(p)?.0.handle;
                self.objects.remove(&h).map(|_| ()).ok_or(Error::BadHandle)
            }
            op::BEGIN_RENDER_PASS => self.begin_render_pass(&arg(p)?.0),
            op::BEGIN_BLIT => self.begin_compute(true),
            op::BEGIN_COMPUTE => self.begin_compute(false),
            op::END_ENCODING => {
                if matches!(self.enc, Encoder::None) {
                    return Err(Error::BadState);
                }
                self.end_encoder();
                Ok(())
            }
            op::SET_RENDER_PIPELINE => {
                let h = arg::<cmd::SetRenderPipeline>(p)?.0.handle;
                let Some(Object::RenderPipeline(ps)) = self.objects.get(&h) else {
                    return Err(Error::BadHandle);
                };
                let Encoder::Render { enc, pipeline } = &mut self.enc else {
                    return Err(Error::BadState);
                };
                enc.setRenderPipelineState(ps);
                *pipeline = true;
                Ok(())
            }
            op::SET_BUFFER => self.set_buffer(arg(p)?.0),
            op::SET_VIEWPORT => {
                let v = arg::<cmd::SetViewport>(p)?.0;
                let Encoder::Render { enc, .. } = &self.enc else {
                    return Err(Error::BadState);
                };
                let all = [v.x, v.y, v.width, v.height, v.znear, v.zfar];
                if all.iter().any(|f| !f.is_finite()) {
                    return Err(Error::BadValue);
                }
                enc.setViewport(MTLViewport {
                    originX: v.x as f64,
                    originY: v.y as f64,
                    width: v.width as f64,
                    height: v.height as f64,
                    znear: v.znear as f64,
                    zfar: v.zfar as f64,
                });
                Ok(())
            }
            op::DRAW_PRIMITIVES => {
                let d = arg::<cmd::DrawPrimitives>(p)?.0;
                let Encoder::Render { enc, pipeline } = &self.enc else {
                    return Err(Error::BadState);
                };
                if !pipeline {
                    return Err(Error::BadState);
                }
                if d.primitive > 4 {
                    return Err(Error::BadValue);
                }
                // SAFETY: a pipeline is bound and the primitive type is valid; out-of-range vertex
                // fetches read arena memory, which is the guest's own.
                unsafe {
                    enc.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(
                        MTLPrimitiveType(d.primitive as usize),
                        d.vertex_start as usize,
                        d.vertex_count as usize,
                        d.instance_count as usize,
                        d.base_instance as usize,
                    )
                };
                Ok(())
            }
            op::FILL_BUFFER => {
                let f = arg::<cmd::FillBuffer>(p)?.0;
                if !f.buffer.offset.is_multiple_of(4)
                    || !f.length.is_multiple_of(4)
                    || f.value > 0xff
                {
                    return Err(Error::BadValue);
                }
                let a = self.range(f.buffer, f.length)?;
                let enc = self.copy_encoder()?;
                // SAFETY: the range was checked against the arena.
                unsafe {
                    enc.fillBuffer_range_value(
                        &a.buffer,
                        NSRange::new(f.buffer.offset as usize, f.length as usize),
                        f.value as u8,
                    )
                };
                Ok(())
            }
            op::COPY_BUFFER => {
                let c = arg::<cmd::CopyBuffer>(p)?.0;
                if !(c.src.offset | c.dst.offset | c.length).is_multiple_of(4) {
                    return Err(Error::BadValue);
                }
                let src = self.range(c.src, c.length)?;
                let dst = self.range(c.dst, c.length)?;
                let enc = self.copy_encoder()?;
                // SAFETY: both ranges were checked against their arenas.
                unsafe {
                    enc.copyFromBuffer_sourceOffset_toBuffer_destinationOffset_size(
                        &src.buffer,
                        c.src.offset as usize,
                        &dst.buffer,
                        c.dst.offset as usize,
                        c.length as usize,
                    )
                };
                Ok(())
            }
            op::COPY_TEXTURE_TO_BUFFER => self.copy_texture_to_buffer(&arg(p)?.0),
            op::SET_COMPUTE_PIPELINE => {
                let h = arg::<cmd::SetComputePipeline>(p)?.0.handle;
                let Some(Object::ComputePipeline(ps)) = self.objects.get(&h) else {
                    return Err(Error::BadHandle);
                };
                let Encoder::Compute {
                    enc,
                    max_threads,
                    copies: false,
                } = &mut self.enc
                else {
                    return Err(Error::BadState);
                };
                enc.setComputePipelineState(ps);
                *max_threads = ps.maxTotalThreadsPerThreadgroup() as u64;
                Ok(())
            }
            op::DISPATCH_THREADGROUPS => {
                let d = arg::<cmd::DispatchThreadgroups>(p)?.0;
                let Encoder::Compute {
                    enc,
                    max_threads,
                    copies: false,
                } = &self.enc
                else {
                    return Err(Error::BadState);
                };
                let t = d
                    .threads_per_group
                    .iter()
                    .map(|&v| v as u64)
                    .product::<u64>();
                if *max_threads == 0 || t == 0 || t > *max_threads {
                    return Err(Error::BadValue);
                }
                let size = |v: [u32; 3]| MTLSize {
                    width: v[0] as usize,
                    height: v[1] as usize,
                    depth: v[2] as usize,
                };
                enc.dispatchThreadgroups_threadsPerThreadgroup(
                    size(d.groups),
                    size(d.threads_per_group),
                );
                Ok(())
            }
            op::COMMIT => self.commit(arg::<cmd::Commit>(p)?.0.seqno),
            _ => Err(Error::UnknownOp),
        }
    }

    fn require_no_encoder(&self) -> R {
        match self.enc {
            Encoder::None => Ok(()),
            _ => Err(Error::BadState),
        }
    }

    fn create_texture(&mut self, c: cmd::CreateTexture) -> R {
        self.new_handle(c.handle)?;
        let a = self.arena(c.arena)?;
        if !metal::valid_texture_type(c.texture_type)
            || metal::bytes_per_texel(c.pixel_format).is_none()
            || !(1..=MAX_DIM).contains(&c.width)
            || !(1..=MAX_DIM).contains(&c.height)
            || !(1..=2048).contains(&c.depth)
            || !(1..=15).contains(&c.mip_levels)
            || !(1..=2048).contains(&c.array_length)
            || !matches!(c.sample_count, 1 | 4)
        {
            return Err(Error::BadValue);
        }
        let desc = MTLTextureDescriptor::new();
        desc.setTextureType(MTLTextureType(c.texture_type as usize));
        desc.setPixelFormat(MTLPixelFormat(c.pixel_format as usize));
        // SAFETY: dimensions, counts and type were range-checked above; Metal checks combinations
        // (a non-3D depth, say) by failing the allocation, which we turn into BadValue.
        unsafe {
            desc.setWidth(c.width as usize);
            desc.setHeight(c.height as usize);
            desc.setDepth(c.depth as usize);
            desc.setMipmapLevelCount(c.mip_levels as usize);
            desc.setArrayLength(c.array_length as usize);
            desc.setSampleCount(c.sample_count as usize);
        }
        desc.setResourceOptions(metal::arena_options());
        desc.setUsage(MTLTextureUsage(
            (c.usage & metal::TEXTURE_USAGE_MASK) as usize,
        ));
        let sa = self.device.heapTextureSizeAndAlignWithDescriptor(&desc);
        if sa.size == 0
            || sa.align == 0
            || !c.offset.is_multiple_of(sa.align as u64)
            || !a.contains(c.offset, sa.size as u64)
        {
            return Err(Error::BadRange);
        }
        // SAFETY: offset is aligned and the texture fits inside the heap.
        let t = unsafe {
            a.heap
                .newTextureWithDescriptor_offset(&desc, c.offset as usize)
        }
        .ok_or(Error::BadValue)?;
        self.objects.insert(c.handle, Object::Texture(t));
        Ok(())
    }

    fn create_library(&mut self, handle: u32, src: &[u8]) -> R {
        self.new_handle(handle)?;
        let desc = MTL4LibraryDescriptor::new();
        desc.setSource(Some(&*text(src)?));
        match self.compiler.newLibraryWithDescriptor_error(&desc) {
            Ok(l) => {
                self.objects.insert(handle, Object::Library(l));
                Ok(())
            }
            Err(e) => {
                log::warn!("MSL compile failed: {}", e.localizedDescription());
                Err(Error::ShaderCompile)
            }
        }
    }

    fn create_render_pipeline(
        &mut self,
        c: &cmd::CreateRenderPipeline,
        vertex: &[u8],
        fragment: &[u8],
    ) -> R {
        self.new_handle(c.handle)?;
        let lib = self.library(c.library)?;
        let desc = MTL4RenderPipelineDescriptor::new();
        desc.setVertexFunctionDescriptor(Some(&*function(lib, vertex)?));
        if !fragment.is_empty() {
            desc.setFragmentFunctionDescriptor(Some(&*function(lib, fragment)?));
        }
        for (i, &fmt) in c.color_formats.iter().enumerate() {
            if fmt == 0 {
                continue;
            }
            if metal::bytes_per_texel(fmt).is_none() {
                return Err(Error::BadValue);
            }
            // SAFETY: i < MAX_COLOR_ATTACHMENTS (8), the Metal limit.
            unsafe { desc.colorAttachments().objectAtIndexedSubscript(i) }
                .setPixelFormat(MTLPixelFormat(fmt as usize));
        }
        // Metal 4 pipelines name no depth or stencil format; the pass's attachments decide.
        if !matches!(c.depth_format, 0 | 252)
            || c.stencil_format != 0
            || !matches!(c.sample_count, 0 | 1 | 4)
        {
            return Err(Error::BadValue);
        }
        // SAFETY: 1 or 4, checked above.
        unsafe { desc.setRasterSampleCount(c.sample_count.max(1) as usize) };
        match self
            .compiler
            .newRenderPipelineStateWithDescriptor_compilerTaskOptions_error(&desc, None)
        {
            Ok(ps) => {
                self.objects.insert(c.handle, Object::RenderPipeline(ps));
                Ok(())
            }
            Err(e) => {
                log::warn!("render pipeline failed: {}", e.localizedDescription());
                Err(Error::PipelineCreate)
            }
        }
    }

    fn create_compute_pipeline(&mut self, handle: u32, library: u32, name: &[u8]) -> R {
        self.new_handle(handle)?;
        let desc = MTL4ComputePipelineDescriptor::new();
        desc.setComputeFunctionDescriptor(Some(&*function(self.library(library)?, name)?));
        match self
            .compiler
            .newComputePipelineStateWithDescriptor_compilerTaskOptions_error(&desc, None)
        {
            Ok(ps) => {
                self.objects.insert(handle, Object::ComputePipeline(ps));
                Ok(())
            }
            Err(e) => {
                log::warn!("compute pipeline failed: {}", e.localizedDescription());
                Err(Error::PipelineCreate)
            }
        }
    }

    fn begin_render_pass(&mut self, c: &cmd::BeginRenderPass) -> R {
        self.require_no_encoder()?;
        let desc = MTL4RenderPassDescriptor::new();
        let mut any = false;
        for (i, ca) in c.colors.iter().enumerate() {
            if ca.texture == 0 {
                continue;
            }
            let t = self.texture(ca.texture)?;
            if ca.load_action > 2
                || ca.store_action > 1
                || ca.level as usize >= t.mipmapLevelCount()
                || ca.slice as usize >= t.arrayLength().max(t.depth())
                || ca.clear.iter().any(|f| !f.is_finite())
            {
                return Err(Error::BadValue);
            }
            // SAFETY: i < MAX_COLOR_ATTACHMENTS (8), the Metal limit.
            let att = unsafe { desc.colorAttachments().objectAtIndexedSubscript(i) };
            att.setTexture(Some(t));
            att.setLevel(ca.level as usize);
            att.setSlice(ca.slice as usize);
            att.setLoadAction(MTLLoadAction(ca.load_action as usize));
            att.setStoreAction(MTLStoreAction(ca.store_action as usize));
            att.setClearColor(MTLClearColor {
                red: ca.clear[0],
                green: ca.clear[1],
                blue: ca.clear[2],
                alpha: ca.clear[3],
            });
            any = true;
        }
        if !any {
            return Err(Error::BadValue);
        }
        if c.width != 0 && c.height != 0 {
            desc.setRenderTargetWidth(c.width as usize);
            desc.setRenderTargetHeight(c.height as usize);
        }
        let cb = self.command_buffer()?;
        let enc = cb
            .renderCommandEncoderWithDescriptor(&desc)
            .ok_or(Error::BadValue)?;
        Self::wait_for_prior(ProtocolObject::from_ref(&*enc));
        enc.setArgumentTable_atStages(&self.tables.vertex, MTLRenderStages::Vertex);
        enc.setArgumentTable_atStages(&self.tables.fragment, MTLRenderStages::Fragment);
        self.enc = Encoder::Render {
            enc,
            pipeline: false,
        };
        Ok(())
    }

    fn set_buffer(&mut self, s: cmd::SetBuffer) -> R {
        if s.index > MAX_BUFFER_INDEX {
            return Err(Error::BadValue);
        }
        let a = self.arena(s.buffer.arena)?;
        if s.buffer.offset >= a.size {
            return Err(Error::BadRange);
        }
        let table = match (&self.enc, s.stage) {
            (Encoder::Render { .. }, cmd::stage::VERTEX) => &self.tables.vertex,
            (Encoder::Render { .. }, cmd::stage::FRAGMENT) => &self.tables.fragment,
            (Encoder::Compute { copies: false, .. }, cmd::stage::COMPUTE) => &self.tables.compute,
            _ => return Err(Error::BadState),
        };
        // SAFETY: the address is inside an arena the context's residency set holds, and the index
        // is within the table's size. Draws and dispatches capture the table as encoded.
        unsafe { table.setAddress_atIndex(a.gpu_base + s.buffer.offset, s.index as usize) };
        Ok(())
    }

    fn copy_texture_to_buffer(&mut self, c: &cmd::CopyTextureToBuffer) -> R {
        let t = self.texture(c.texture)?.clone();
        let bpt = metal::bytes_per_texel(t.pixelFormat().0 as u32).ok_or(Error::BadValue)?;
        let level = c.level as usize;
        if level >= t.mipmapLevelCount() || c.slice as usize >= t.arrayLength() {
            return Err(Error::BadValue);
        }
        let dims = [
            (t.width() >> level).max(1) as u64,
            (t.height() >> level).max(1) as u64,
            (t.depth() >> level).max(1) as u64,
        ];
        for ((&o, &n), &dim) in c.origin.iter().zip(&c.size).zip(&dims) {
            if n == 0 || o as u64 + n as u64 > dim {
                return Err(Error::BadValue);
            }
        }
        let row = c.size[0] as u64 * bpt;
        if c.bytes_per_row < row
            || (c.size[2] > 1 && c.bytes_per_image < c.bytes_per_row * c.size[1] as u64)
        {
            return Err(Error::BadValue);
        }
        let len = (c.size[2] as u64 - 1) * c.bytes_per_image
            + (c.size[1] as u64 - 1) * c.bytes_per_row
            + row;
        let dst = self.range(c.dst, len)?;
        let enc = self.copy_encoder()?;
        let v = |a: [u32; 3]| (a[0] as usize, a[1] as usize, a[2] as usize);
        let (ox, oy, oz) = v(c.origin);
        let (sx, sy, sz) = v(c.size);
        // SAFETY: the source region lies inside the level and the destination range inside the
        // arena, both checked above.
        unsafe {
            enc.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                &t,
                c.slice as usize,
                level,
                MTLOrigin { x: ox, y: oy, z: oz },
                MTLSize { width: sx, height: sy, depth: sz },
                &dst.buffer,
                c.dst.offset as usize,
                c.bytes_per_row as usize,
                c.bytes_per_image as usize,
            )
        };
        Ok(())
    }

    fn commit(&mut self, seqno: u64) -> R {
        self.require_no_encoder()?;
        if seqno <= self.last_seqno {
            return Err(Error::BadValue);
        }
        self.last_seqno = seqno;
        self.command_buffer()?;
        let (cb, alloc) = self.cb.take().expect("command_buffer() opened one");
        cb.endCommandBuffer();
        // Arena memory must outlive the GPU's use of it even if the guest destroys the arena
        // meanwhile; the feedback handler holds it, and the allocator, until the GPU is done.
        let mut keep: Vec<Arc<Arena>> = self.resident.values().cloned().collect();
        keep.push(self.ring_arena.clone());
        let ctrl = SendPtr(self.consumer.control_ptr());
        let pool = self.allocators.clone();
        let alloc = Mutex::new(Some(alloc));
        let block = RcBlock::new(move |fb: NonNull<ProtocolObject<dyn MTL4CommitFeedback>>| {
            let _keep = &keep;
            // SAFETY: the ring's arena is in `keep`, so the control block is still mapped.
            let c = unsafe { ctrl.0.as_ref() };
            // SAFETY: Metal passes the live feedback object.
            if let Some(e) = unsafe { fb.as_ref() }.error() {
                log::warn!(
                    "command buffer {seqno} failed: {}",
                    e.localizedDescription()
                );
                c.fail(Error::GpuFault, 0);
            }
            if let Some(a) = alloc.lock().unwrap_or_else(|e| e.into_inner()).take() {
                pool.lock().unwrap_or_else(|e| e.into_inner()).push(a);
            }
            c.complete(seqno);
        });
        let options = MTL4CommitOptions::new();
        // SAFETY: Metal copies the block.
        unsafe { options.addFeedbackHandler(RcBlock::as_ptr(&block)) };
        let mut cbs = [NonNull::from(&*cb)];
        // SAFETY: one live, ended command buffer.
        unsafe {
            self.queue
                .commit_count_options(NonNull::from(&mut cbs).cast(), 1, &options)
        };
        Ok(())
    }
}
