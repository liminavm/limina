//! A context's ring consumer: decodes commands and replays them on Metal 4.
//!
//! Every value from the ring is checked before Metal sees it. Metal answers many invalid calls by
//! aborting the process, which a lost context is cheaper than, even with the replayer isolated.
//! GPU addresses (argument-table entries, index and indirect buffers) are the exception: a shader
//! can dereference any address it computes, so checking the ones a command names would buy no
//! isolation, and they pass through as given.

use std::collections::{BTreeSet, HashMap};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use limina_gpu_wire::cmd::{self, BufRef, Command, Feedback, REPLY_DONE, Reply, op};
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
    MTL4VisibilityOptions, MTLAllocation, MTLBlitOption, MTLBuffer, MTLClearColor,
    MTLCompareFunction, MTLCompileOptions, MTLComputePipelineState, MTLCullMode, MTLDepthClipMode,
    MTLDepthStencilDescriptor, MTLDepthStencilState, MTLDevice, MTLEvent, MTLFence, MTLGPUFamily,
    MTLHeap, MTLIndexType, MTLLanguageVersion, MTLLibrary, MTLLoadAction,
    MTLMathFloatingPointFunctions, MTLMathMode, MTLMultisampleDepthResolveFilter,
    MTLMultisampleStencilResolveFilter, MTLOrigin, MTLPixelFormat, MTLPrimitiveTopologyClass,
    MTLPrimitiveType, MTLRegion, MTLRenderPassAttachmentDescriptor, MTLRenderPipelineState,
    MTLRenderStages, MTLResidencySet, MTLResidencySetDescriptor, MTLResourceID, MTLSamplePosition,
    MTLSamplerAddressMode, MTLSamplerBorderColor, MTLSamplerDescriptor, MTLSamplerMinMagFilter,
    MTLSamplerMipFilter, MTLSamplerReductionMode, MTLSamplerState, MTLScissorRect, MTLSharedEvent,
    MTLSharedEventListener, MTLSize, MTLStages, MTLStencilDescriptor, MTLStencilOperation,
    MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureSwizzle, MTLTextureSwizzleChannels,
    MTLTextureType, MTLTextureUsage, MTLVertexAmplificationViewMapping, MTLViewport,
    MTLVisibilityResultMode, MTLWinding,
};

use crate::mach::Semaphore;
use crate::metal::{self, Arena, Device, FormatInfo};

/// How long an idle consumer polls before it parks and needs a doorbell. Short: a doorbell costs
/// the guest ~1 µs plus a wake, so spinning longer buys little and burns a core.
const SPIN: Duration = Duration::from_micros(50);
/// Largest texture dimension accepted.
const MAX_DIM: u32 = 16384;
/// Most buffer bindings an argument table may have (Metal's limit).
const MAX_BUFFER_BINDS: u32 = 31;
/// Every `MTLStages` bit.
const ALL_STAGES: u32 = (MTLStages::Vertex.0
    | MTLStages::Fragment.0
    | MTLStages::Tile.0
    | MTLStages::Object.0
    | MTLStages::Mesh.0
    | MTLStages::ResourceState.0
    | MTLStages::Dispatch.0
    | MTLStages::Blit.0
    | MTLStages::AccelerationStructure.0
    | MTLStages::MachineLearning.0) as u32;
/// Every `MTLRenderStages` bit.
const ALL_RENDER_STAGES: u32 = (MTLRenderStages::Vertex.0
    | MTLRenderStages::Fragment.0
    | MTLRenderStages::Tile.0
    | MTLRenderStages::Object.0
    | MTLRenderStages::Mesh.0) as u32;

pub type Arenas = Arc<Mutex<HashMap<u32, Arc<Arena>>>>;

type Po<T> = Retained<ProtocolObject<T>>;

/// A shared event's mirror: the `u64` in arena memory the guest polls.
#[derive(Clone)]
struct Mirror {
    arena: Arc<Arena>,
    offset: u64,
}

impl Mirror {
    fn raise(&self, value: u64) {
        // SAFETY: the range was checked at creation and the arena is held; 8-aligned.
        let word = unsafe {
            &*self
                .arena
                .contents()
                .add(self.offset as usize)
                .cast::<AtomicU64>()
        };
        word.fetch_max(value, Ordering::Release);
    }
}

enum Object {
    Texture(Po<dyn MTLTexture>),
    /// Held only to keep it alive: shaders reach it by resource ID.
    Sampler(#[allow(dead_code)] Po<dyn MTLSamplerState>),
    Library(Po<dyn MTLLibrary>),
    RenderPipeline(Po<dyn MTLRenderPipelineState>),
    ComputePipeline(Po<dyn MTLComputePipelineState>),
    DepthStencil(Po<dyn MTLDepthStencilState>),
    ArgumentTable(Po<dyn MTL4ArgumentTable>, u32),
    Fence(Po<dyn MTLFence>),
    Event(Po<dyn MTLEvent>),
    SharedEvent(Po<dyn MTLSharedEvent>, Option<Mirror>),
}

/// Destroyed objects, held until the GPU can no longer be using them.
struct Grave(#[allow(dead_code)] Vec<Object>);
// SAFETY: Metal objects may be released from any thread.
unsafe impl Send for Grave {}

enum Encoder {
    None,
    Render {
        enc: Po<dyn MTL4RenderCommandEncoder>,
        /// The pass's visibility buffer: arena and base offset.
        visibility: Option<(Arc<Arena>, u64)>,
    },
    Compute {
        enc: Po<dyn MTL4ComputeCommandEncoder>,
        max_threads: u64,
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

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Executor {
    device: Retained<Device>,
    queue: Po<dyn MTL4CommandQueue>,
    compiler: Po<dyn MTL4Compiler>,
    listener: Retained<MTLSharedEventListener>,
    /// Holds every arena of the context; the queue keeps it resident.
    residency: Po<dyn MTLResidencySet>,
    /// The arenas `residency` holds, by id, as of the last command buffer begun.
    resident: HashMap<u32, Arc<Arena>>,
    allocators: Pool,
    consumer: Consumer,
    /// The arena the ring lives in: its memory must outlive the consumer.
    ring_arena: Arc<Arena>,
    arenas: Arenas,
    objects: HashMap<u32, Object>,
    /// The command buffer being recorded.
    open: Option<(Po<dyn MTL4CommandBuffer>, Allocator)>,
    /// Command buffers ended and waiting for the next commit.
    ended: Vec<(Po<dyn MTL4CommandBuffer>, Allocator)>,
    enc: Encoder,
    last_seqno: u64,
    /// Commits submitted so far.
    commits: u64,
    /// Commits (by their position in `commits`) not yet completed.
    in_flight: Arc<Mutex<BTreeSet<u64>>>,
    /// Destroyed objects with the commit that may still use them; see [`Self::reap`].
    graveyard: Vec<(u64, Grave)>,
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

fn check(ok: bool) -> R {
    if ok { Ok(()) } else { Err(Error::BadValue) }
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

fn resource_id(id: MTLResourceID) -> u64 {
    // SAFETY: `MTLResourceID` is a `repr(C)` wrapper of one `u64`.
    unsafe { std::mem::transmute::<MTLResourceID, u64>(id) }
}

fn size3(v: [u32; 3]) -> MTLSize {
    MTLSize {
        width: v[0] as usize,
        height: v[1] as usize,
        depth: v[2] as usize,
    }
}

fn origin3(v: [u32; 3]) -> MTLOrigin {
    MTLOrigin {
        x: v[0] as usize,
        y: v[1] as usize,
        z: v[2] as usize,
    }
}

fn stages(mask: u32) -> Result<MTLStages, Error> {
    check(mask & !ALL_STAGES == 0)?;
    Ok(MTLStages(mask as usize))
}

/// `MTLLanguageVersion` values Metal 4 compilers take; `0` keeps the default.
fn valid_language_version(v: u32) -> bool {
    matches!(
        v,
        0 | 0x1_0001 | 0x1_0002 | 0x2_0000..=0x2_0004 | 0x3_0000..=0x3_0002 | 0x4_0000
    )
}

/// The bytes a buffer<->texture copy touches in the buffer, or `None` if the pitches are too small
/// for the region.
fn linear_extent(
    fmt: FormatInfo,
    size: [u32; 3],
    bytes_per_row: u64,
    bytes_per_image: u64,
) -> Option<u64> {
    let cols = size[0].div_ceil(fmt.block_w) as u64;
    let rows = size[1].div_ceil(fmt.block_h) as u64;
    let row = cols.checked_mul(fmt.bytes)?;
    if bytes_per_row < row {
        return None;
    }
    if size[2] > 1 && bytes_per_image < bytes_per_row.checked_mul(rows)? {
        return None;
    }
    (size[2] as u64 - 1)
        .checked_mul(bytes_per_image)?
        .checked_add((rows - 1).checked_mul(bytes_per_row)?)?
        .checked_add(row)
}

/// Checks that `origin + size` lies inside level `level` of `t` and returns the level's extent.
fn check_region(
    t: &ProtocolObject<dyn MTLTexture>,
    level: u32,
    slice: u32,
    origin: [u32; 3],
    size: [u32; 3],
) -> R {
    let level = level as usize;
    check(level < t.mipmapLevelCount())?;
    let slices = t.arrayLength()
        * if t.textureType() == MTLTextureType::TypeCube
            || t.textureType() == MTLTextureType::TypeCubeArray
        {
            6
        } else {
            1
        };
    check((slice as usize) < slices)?;
    let dims = [
        (t.width() >> level).max(1) as u64,
        (t.height() >> level).max(1) as u64,
        (t.depth() >> level).max(1) as u64,
    ];
    for ((&o, &n), &dim) in origin.iter().zip(&size).zip(&dims) {
        check(n != 0 && o as u64 + n as u64 <= dim)?;
    }
    Ok(())
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
        Some(Self {
            ring_arena,
            queue,
            compiler,
            listener: MTLSharedEventListener::new(),
            residency,
            resident: HashMap::new(),
            allocators: Arc::new(Mutex::new(Vec::new())),
            device,
            consumer,
            arenas,
            objects: HashMap::new(),
            open: None,
            ended: Vec::new(),
            enc: Encoder::None,
            last_seqno: 0,
            commits: 0,
            in_flight: Arc::new(Mutex::new(BTreeSet::new())),
            graveyard: Vec::new(),
            lost: false,
        })
    }

    /// Drains the ring until `stop`, parking on `doorbell` when idle.
    pub fn run(mut self, doorbell: &Semaphore, stop: &AtomicBool) {
        // A payload is at most half the data area (the producer enforces it; the consumer checks).
        let mut scratch = vec![0u8; (self.consumer.data_size() / 2) as usize];
        let mut idle_since: Option<Instant> = None;
        while !stop.load(Ordering::Acquire) {
            match self.consumer.next(&mut scratch) {
                Ok(Some(n)) => {
                    idle_since = None;
                    if self.lost {
                        continue;
                    }
                    if let Err(e) = self.exec(n.op, n.payload) {
                        log::warn!(
                            "context lost: {e:?} at ring position {} (op {:#x})",
                            n.at,
                            n.op
                        );
                        self.lose(e, n.at);
                    }
                }
                Ok(None) => {
                    self.reap();
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
        if let Some((cb, _)) = self.open.take() {
            cb.endCommandBuffer();
        }
        self.ended.clear();
        // Wait for in-flight work before the objects it uses are released.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lock(&self.in_flight).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn lose(&mut self, e: Error, at: u64) {
        self.consumer.control().fail(e, at);
        self.lost = true;
        self.end_encoder();
        // Uncommitted command buffers are dropped; their allocators return to the pool.
        if let Some((cb, alloc)) = self.open.take() {
            cb.endCommandBuffer();
            lock(&self.allocators).push(alloc);
        }
        for (_, alloc) in self.ended.drain(..) {
            lock(&self.allocators).push(alloc);
        }
        let all = std::mem::take(&mut self.objects).into_values().collect();
        self.bury(Grave(all));
    }

    /// Holds `grave` until every commit submitted so far, and the next one, has completed.
    fn bury(&mut self, grave: Grave) {
        self.graveyard.push((self.commits + 1, grave));
    }

    /// Releases destroyed objects no commit can still be using.
    fn reap(&mut self) {
        if self.graveyard.is_empty() {
            return;
        }
        let oldest = lock(&self.in_flight).first().copied().unwrap_or(u64::MAX);
        let commits = self.commits;
        self.graveyard
            .retain(|(tag, _)| !(commits >= *tag && oldest > *tag));
    }

    fn arena(&self, id: u32) -> Result<Arc<Arena>, Error> {
        lock(&self.arenas).get(&id).cloned().ok_or(Error::BadRange)
    }

    /// The arena behind `r`, checked to hold `len` bytes from `r.offset`.
    fn range(&self, r: BufRef, len: u64) -> Result<Arc<Arena>, Error> {
        let a = self.arena(r.arena)?;
        if !a.contains(r.offset, len) {
            return Err(Error::BadRange);
        }
        Ok(a)
    }

    /// A host pointer to `len` bytes at `r`, which must be `align`-aligned.
    fn host(&self, r: BufRef, len: u64, align: u64) -> Result<(Arc<Arena>, *mut u8), Error> {
        if !r.offset.is_multiple_of(align) {
            return Err(Error::BadRange);
        }
        let a = self.range(r, len)?;
        // SAFETY: inside the arena, checked above.
        let p = unsafe { a.contents().add(r.offset as usize) };
        Ok((a, p))
    }

    /// Answers a command that names a reply. Without one, a failure loses the context; with one,
    /// it is reported there and the context lives on.
    fn answer(&self, reply: BufRef, r: Result<[u64; 6], Error>) -> R {
        if reply.is_none() {
            return r.map(|_| ());
        }
        let (_a, p) = self.host(reply, size_of::<Reply>() as u64, 8)?;
        let (status, values) = match r {
            Ok(v) => (0, v),
            Err(e) => {
                log::debug!("reply with error {e:?}");
                (e as u32, [0; 6])
            }
        };
        let p = p.cast::<Reply>();
        // SAFETY: in the arena, aligned and in bounds (checked by `host`); `done` is written last
        // with release ordering, so the guest sees the rest once it sees `done`.
        unsafe {
            (&raw mut (*p).values).write_volatile(values);
            (&raw mut (*p).status).write_volatile(status);
            (*(&raw mut (*p).done).cast::<AtomicU32>()).store(REPLY_DONE, Ordering::Release);
        }
        Ok(())
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

    /// Brings the residency set in line with the context's arenas. Arenas come and go between
    /// command buffers, never within one, so this runs as each begins.
    fn sync_residency(&mut self) {
        let now = lock(&self.arenas).clone();
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

    fn begin_command_buffer(&mut self) -> R {
        if self.open.is_some() {
            return Err(Error::BadState);
        }
        self.sync_residency();
        let alloc = match lock(&self.allocators).pop() {
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
        self.open = Some((cb, alloc));
        Ok(())
    }

    fn open_cb(&self) -> Result<&Po<dyn MTL4CommandBuffer>, Error> {
        match (&self.open, &self.enc) {
            (Some((cb, _)), Encoder::None) => Ok(cb),
            _ => Err(Error::BadState),
        }
    }

    fn end_encoder(&mut self) {
        match std::mem::replace(&mut self.enc, Encoder::None) {
            Encoder::None => {}
            Encoder::Render { enc, .. } => enc.endEncoding(),
            Encoder::Compute { enc, .. } => enc.endEncoding(),
        }
    }

    fn encoder(&self) -> Result<&ProtocolObject<dyn MTL4CommandEncoder>, Error> {
        match &self.enc {
            Encoder::None => Err(Error::BadState),
            Encoder::Render { enc, .. } => Ok(ProtocolObject::from_ref(&**enc)),
            Encoder::Compute { enc, .. } => Ok(ProtocolObject::from_ref(&**enc)),
        }
    }

    fn compute(&self) -> Result<&Po<dyn MTL4ComputeCommandEncoder>, Error> {
        match &self.enc {
            Encoder::Compute { enc, .. } => Ok(enc),
            _ => Err(Error::BadState),
        }
    }

    fn render(&self) -> Result<&Po<dyn MTL4RenderCommandEncoder>, Error> {
        match &self.enc {
            Encoder::Render { enc, .. } => Ok(enc),
            _ => Err(Error::BadState),
        }
    }

    fn exec(&mut self, opcode: u16, p: &[u8]) -> R {
        match opcode {
            op::CREATE_TEXTURE => {
                let c = arg::<cmd::CreateTexture>(p)?.0;
                let r = self.create_texture(&c);
                self.answer(c.reply, r)
            }
            op::CREATE_TEXTURE_VIEW => {
                let c = arg::<cmd::CreateTextureView>(p)?.0;
                let r = self.create_texture_view(&c);
                self.answer(c.reply, r)
            }
            op::CREATE_SAMPLER => {
                let c = arg::<cmd::CreateSampler>(p)?.0;
                let r = self.create_sampler(c.handle, &c.desc);
                self.answer(c.reply, r)
            }
            op::CREATE_LIBRARY => {
                let (c, rest) = arg::<cmd::CreateLibrary>(p)?;
                let (src, _) = take(rest, c.source_len)?;
                let r = self.create_library(&c, src);
                self.answer(c.reply, r)
            }
            op::CREATE_RENDER_PIPELINE => {
                let (c, rest) = arg::<cmd::CreateRenderPipeline>(p)?;
                let (v, rest) = take(rest, c.vertex_name_len)?;
                let (f, _) = take(rest, c.fragment_name_len)?;
                let r = self.create_render_pipeline(&c, v, f);
                self.answer(c.reply, r)
            }
            op::CREATE_COMPUTE_PIPELINE => {
                let (c, rest) = arg::<cmd::CreateComputePipeline>(p)?;
                let (name, _) = take(rest, c.name_len)?;
                let r = self.create_compute_pipeline(&c, name);
                self.answer(c.reply, r)
            }
            op::CREATE_DEPTH_STENCIL => self.create_depth_stencil(&arg(p)?.0),
            op::CREATE_ARGUMENT_TABLE => {
                let c = arg::<cmd::CreateArgumentTable>(p)?.0;
                self.new_handle(c.handle)?;
                check((1..=MAX_BUFFER_BINDS).contains(&c.max_buffer_bind_count))?;
                let desc = MTL4ArgumentTableDescriptor::new();
                desc.setMaxBufferBindCount(c.max_buffer_bind_count as usize);
                let t = self
                    .device
                    .newArgumentTableWithDescriptor_error(&desc)
                    .map_err(|_| Error::OutOfMemory)?;
                self.objects
                    .insert(c.handle, Object::ArgumentTable(t, c.max_buffer_bind_count));
                Ok(())
            }
            op::CREATE_SYNC => self.create_sync(&arg(p)?.0),
            op::DESTROY => {
                let h = arg::<cmd::Destroy>(p)?.0.handle;
                let o = self.objects.remove(&h).ok_or(Error::BadHandle)?;
                self.bury(Grave(vec![o]));
                Ok(())
            }
            op::SET_ADDRESSES => {
                let (c, rest) = arg::<cmd::SetAddresses>(p)?;
                let Some(Object::ArgumentTable(t, max)) = self.objects.get(&c.table) else {
                    return Err(Error::BadHandle);
                };
                let (items, _) = cmd::decode_array::<cmd::AddressBinding>(rest, c.count as usize)
                    .ok_or(Error::BadPayload)?;
                for b in items {
                    check(b.index < *max)?;
                    // SAFETY: the index is within the table; addresses are the guest's own (see
                    // the module comment). Encoders capture the table's state as they encode.
                    unsafe { t.setAddress_atIndex(b.address, b.index as usize) };
                }
                Ok(())
            }
            op::SET_SHARED_EVENT => {
                let c = arg::<cmd::SetSharedEvent>(p)?.0;
                let Some(Object::SharedEvent(e, mirror)) = self.objects.get(&c.handle) else {
                    return Err(Error::BadHandle);
                };
                e.setSignaledValue(c.value);
                if let Some(m) = mirror {
                    m.raise(c.value);
                }
                Ok(())
            }
            op::TEXTURE_TRANSFER => {
                let c = arg::<cmd::TextureTransfer>(p)?.0;
                let r = self.texture_transfer(&c).map(|()| [0; 6]);
                self.answer(c.reply, r)
            }
            op::QUERY_DEVICE => {
                let c = arg::<cmd::QueryDevice>(p)?.0;
                let r = self.query_device(c.dst).map(|()| [0; 6]);
                self.answer(c.reply, r)
            }
            op::QUERY_TEXTURE_SIZE => {
                let c = arg::<cmd::QueryTextureSize>(p)?.0;
                let r = self.texture_descriptor(&c.desc).map(|d| {
                    d.setResourceOptions(metal::arena_options());
                    let sa = self.device.heapTextureSizeAndAlignWithDescriptor(&d);
                    [sa.size as u64, sa.align as u64, 0, 0, 0, 0]
                });
                self.answer(c.reply, r)
            }
            op::QUERY_LINEAR_ALIGNMENT => {
                let c = arg::<cmd::QueryLinearAlignment>(p)?.0;
                let r = match metal::format_info(c.pixel_format) {
                    Some(_) => Ok([
                        self.device
                            .minimumLinearTextureAlignmentForPixelFormat(MTLPixelFormat(
                                c.pixel_format as usize,
                            )) as u64,
                        0,
                        0,
                        0,
                        0,
                        0,
                    ]),
                    None => Err(Error::BadValue),
                };
                self.answer(c.reply, r)
            }
            op::QUERY_TIMESTAMP => {
                let c = arg::<cmd::QueryTimestamp>(p)?.0;
                let (mut cpu, mut gpu) = (0u64, 0u64);
                // SAFETY: two valid out-pointers.
                unsafe {
                    self.device.sampleTimestamps_gpuTimestamp(
                        NonNull::from(&mut cpu),
                        NonNull::from(&mut gpu),
                    )
                };
                self.answer(c.reply, Ok([gpu, cpu, 0, 0, 0, 0]))
            }
            op::BEGIN_COMMAND_BUFFER => self.begin_command_buffer(),
            op::END_COMMAND_BUFFER => {
                self.open_cb()?;
                let (cb, alloc) = self.open.take().expect("open_cb checked it");
                cb.endCommandBuffer();
                self.ended.push((cb, alloc));
                Ok(())
            }
            op::BEGIN_COMPUTE => {
                let enc = self
                    .open_cb()?
                    .computeCommandEncoder()
                    .ok_or(Error::OutOfMemory)?;
                self.enc = Encoder::Compute {
                    enc,
                    max_threads: 0,
                };
                Ok(())
            }
            op::BEGIN_RENDER_PASS => self.begin_render_pass(&arg(p)?.0),
            op::END_ENCODING => {
                if matches!(self.enc, Encoder::None) {
                    return Err(Error::BadState);
                }
                self.end_encoder();
                Ok(())
            }
            op::BARRIER => {
                let b = arg::<cmd::Barrier>(p)?.0;
                let (after, before) = (stages(b.after)?, stages(b.before)?);
                check(b.visibility <= 3)?;
                let vis = MTL4VisibilityOptions(b.visibility as usize);
                let enc = self.encoder()?;
                match b.kind {
                    cmd::barrier_kind::AFTER_STAGES => enc
                        .barrierAfterStages_beforeQueueStages_visibilityOptions(after, before, vis),
                    cmd::barrier_kind::AFTER_ENCODER_STAGES => enc
                        .barrierAfterEncoderStages_beforeEncoderStages_visibilityOptions(
                            after, before, vis,
                        ),
                    cmd::barrier_kind::AFTER_QUEUE_STAGES => enc
                        .barrierAfterQueueStages_beforeStages_visibilityOptions(after, before, vis),
                    _ => return Err(Error::BadValue),
                }
                Ok(())
            }
            op::FENCE => {
                let f = arg::<cmd::FenceOp>(p)?.0;
                let Some(Object::Fence(fence)) = self.objects.get(&f.fence) else {
                    return Err(Error::BadHandle);
                };
                let st = stages(f.stages)?;
                let enc = self.encoder()?;
                if f.wait == 0 {
                    enc.updateFence_afterEncoderStages(fence, st);
                } else {
                    enc.waitForFence_beforeEncoderStages(fence, st);
                }
                Ok(())
            }
            op::SET_COMPUTE_PIPELINE => {
                let h = arg::<cmd::SetComputePipeline>(p)?.0.handle;
                let Some(Object::ComputePipeline(ps)) = self.objects.get(&h) else {
                    return Err(Error::BadHandle);
                };
                let Encoder::Compute { enc, max_threads } = &mut self.enc else {
                    return Err(Error::BadState);
                };
                enc.setComputePipelineState(ps);
                *max_threads = ps.maxTotalThreadsPerThreadgroup() as u64;
                Ok(())
            }
            op::SET_ARGUMENT_TABLE => {
                let s = arg::<cmd::SetArgumentTable>(p)?.0;
                let Some(Object::ArgumentTable(t, _)) = self.objects.get(&s.table) else {
                    return Err(Error::BadHandle);
                };
                match &self.enc {
                    Encoder::Compute { enc, .. } => enc.setArgumentTable(Some(t)),
                    Encoder::Render { enc, .. } => {
                        check(s.stages != 0 && s.stages & !ALL_RENDER_STAGES == 0)?;
                        enc.setArgumentTable_atStages(t, MTLRenderStages(s.stages as usize))
                    }
                    Encoder::None => return Err(Error::BadState),
                }
                Ok(())
            }
            op::DISPATCH_THREADS | op::DISPATCH_THREADGROUPS => {
                let d = arg::<cmd::DispatchThreads>(p)?.0;
                let Encoder::Compute { enc, max_threads } = &self.enc else {
                    return Err(Error::BadState);
                };
                let t: u64 = d.threads_per_group.iter().map(|&v| v as u64).product();
                check(*max_threads != 0 && t != 0 && t <= *max_threads)?;
                if opcode == op::DISPATCH_THREADS {
                    enc.dispatchThreads_threadsPerThreadgroup(
                        size3(d.grid),
                        size3(d.threads_per_group),
                    );
                } else {
                    enc.dispatchThreadgroups_threadsPerThreadgroup(
                        size3(d.grid),
                        size3(d.threads_per_group),
                    );
                }
                Ok(())
            }
            op::DISPATCH_INDIRECT => {
                let d = arg::<cmd::DispatchIndirect>(p)?.0;
                let Encoder::Compute { enc, max_threads } = &self.enc else {
                    return Err(Error::BadState);
                };
                let t: u64 = d.threads_per_group.iter().map(|&v| v as u64).product();
                check(*max_threads != 0 && t != 0 && t <= *max_threads)?;
                // SAFETY: a pipeline is bound and the group size fits it; the address is the
                // guest's own.
                unsafe {
                    enc.dispatchThreadgroupsWithIndirectBuffer_threadsPerThreadgroup(
                        d.address,
                        size3(d.threads_per_group),
                    )
                };
                Ok(())
            }
            op::COPY_BUFFER => {
                let c = arg::<cmd::CopyBuffer>(p)?.0;
                let src = self.range(c.src, c.length)?;
                let dst = self.range(c.dst, c.length)?;
                let enc = self.compute()?;
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
            op::COPY_BUFFER_TEXTURE => self.copy_buffer_texture(&arg(p)?.0),
            op::COPY_TEXTURE => {
                let c = arg::<cmd::CopyTexture>(p)?.0;
                let src = self.texture(c.src)?.clone();
                let dst = self.texture(c.dst)?.clone();
                check_region(&src, c.src_level, c.src_slice, c.src_origin, c.src_size)?;
                check_region(&dst, c.dst_level, c.dst_slice, c.dst_origin, c.src_size)?;
                let enc = self.compute()?;
                // SAFETY: both regions lie inside their textures, checked above.
                unsafe {
                    enc.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                        &src,
                        c.src_slice as usize,
                        c.src_level as usize,
                        origin3(c.src_origin),
                        size3(c.src_size),
                        &dst,
                        c.dst_slice as usize,
                        c.dst_level as usize,
                        origin3(c.dst_origin),
                    )
                };
                Ok(())
            }
            op::FILL_BUFFER => {
                let f = arg::<cmd::FillBuffer>(p)?.0;
                check(f.value <= 0xff)?;
                let a = self.range(f.buffer, f.length)?;
                let enc = self.compute()?;
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
            op::SET_RENDER_PIPELINE => {
                let h = arg::<cmd::SetRenderPipeline>(p)?.0.handle;
                let Some(Object::RenderPipeline(ps)) = self.objects.get(&h) else {
                    return Err(Error::BadHandle);
                };
                self.render()?.setRenderPipelineState(ps);
                Ok(())
            }
            op::SET_VIEWPORTS => {
                let (c, rest) = arg::<cmd::SetViewports>(p)?;
                check((1..=16).contains(&c.count))?;
                let (items, _) = cmd::decode_array::<cmd::Viewport>(rest, c.count as usize)
                    .ok_or(Error::BadPayload)?;
                let vps: Vec<MTLViewport> = items
                    .map(|v| MTLViewport {
                        originX: v.x,
                        originY: v.y,
                        width: v.width,
                        height: v.height,
                        znear: v.znear,
                        zfar: v.zfar,
                    })
                    .collect();
                check(vps.iter().all(|v| {
                    [v.originX, v.originY, v.width, v.height, v.znear, v.zfar]
                        .iter()
                        .all(|f| f.is_finite())
                }))?;
                // SAFETY: `count` viewports, at most 16, all finite.
                unsafe {
                    self.render()?
                        .setViewports_count(NonNull::from(&vps[..]).cast(), vps.len())
                };
                Ok(())
            }
            op::SET_SCISSORS => {
                let (c, rest) = arg::<cmd::SetScissors>(p)?;
                check((1..=16).contains(&c.count))?;
                let (items, _) = cmd::decode_array::<cmd::Scissor>(rest, c.count as usize)
                    .ok_or(Error::BadPayload)?;
                let rects: Vec<MTLScissorRect> = items
                    .map(|s| MTLScissorRect {
                        x: s.x as usize,
                        y: s.y as usize,
                        width: s.width as usize,
                        height: s.height as usize,
                    })
                    .collect();
                // SAFETY: `count` rects, at most 16. Metal validates them against the render
                // target only with the debug layer; without it a rect past the target clips.
                unsafe {
                    self.render()?
                        .setScissorRects_count(NonNull::from(&rects[..]).cast(), rects.len())
                };
                Ok(())
            }
            op::SET_DEPTH_STENCIL => {
                let h = arg::<cmd::SetDepthStencil>(p)?.0.handle;
                let ds = match self.objects.get(&h) {
                    Some(Object::DepthStencil(ds)) => Some(ds),
                    None if h == 0 => None,
                    _ => return Err(Error::BadHandle),
                };
                self.render()?.setDepthStencilState(ds.map(|d| &**d));
                Ok(())
            }
            op::RENDER_STATE => self.render_state(&arg(p)?.0),
            op::SET_AMPLIFICATION => {
                let (c, rest) = arg::<cmd::SetAmplification>(p)?;
                check((1..=32).contains(&c.count))?;
                let (offsets, _) = take(rest, c.count * 4)?;
                let maps: Vec<MTLVertexAmplificationViewMapping> = offsets
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| MTLVertexAmplificationViewMapping {
                        viewportArrayIndexOffset: 0,
                        renderTargetArrayIndexOffset: u32::from_le_bytes(*b),
                    })
                    .collect();
                // SAFETY: `count` mappings, at most 32.
                unsafe {
                    self.render()?
                        .setVertexAmplificationCount_viewMappings(maps.len(), maps.as_ptr())
                };
                Ok(())
            }
            op::DRAW => {
                let d = arg::<cmd::Draw>(p)?.0;
                check(d.primitive <= 4)?;
                // SAFETY: the primitive type is valid; vertex fetches read guest memory.
                unsafe {
                    self.render()?
                        .drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(
                            MTLPrimitiveType(d.primitive as usize),
                            d.vertex_start as usize,
                            d.vertex_count as usize,
                            d.instance_count as usize,
                            d.base_instance as usize,
                        )
                };
                Ok(())
            }
            op::DRAW_INDEXED => {
                let d = arg::<cmd::DrawIndexed>(p)?.0;
                check(d.primitive <= 4 && d.index_type <= 1)?;
                // SAFETY: valid enums; the index buffer is the guest's own memory.
                unsafe {
                    self.render()?
                        .drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferLength_instanceCount_baseVertex_baseInstance(
                            MTLPrimitiveType(d.primitive as usize),
                            d.index_count as usize,
                            MTLIndexType(d.index_type as usize),
                            d.index_address,
                            d.index_length as usize,
                            d.instance_count as usize,
                            d.base_vertex as isize,
                            d.base_instance as usize,
                        )
                };
                Ok(())
            }
            op::DRAW_INDIRECT => {
                let d = arg::<cmd::DrawIndirect>(p)?.0;
                check(d.primitive <= 4 && d.index_type <= 1)?;
                let enc = self.render()?;
                if d.indexed == 0 {
                    enc.drawPrimitives_indirectBuffer(
                        MTLPrimitiveType(d.primitive as usize),
                        d.indirect,
                    );
                } else {
                    // SAFETY: valid enums; both buffers are the guest's own memory.
                    unsafe {
                        enc.drawIndexedPrimitives_indexType_indexBuffer_indexBufferLength_indirectBuffer(
                            MTLPrimitiveType(d.primitive as usize),
                            MTLIndexType(d.index_type as usize),
                            d.index_address,
                            d.index_length as usize,
                            d.indirect,
                        )
                    };
                }
                Ok(())
            }
            op::COMMIT => {
                let c = arg::<cmd::Commit>(p)?.0;
                self.commit(c.seqno, c.feedback)
            }
            op::QUEUE_EVENT => self.queue_event(&arg(p)?.0),
            _ => Err(Error::UnknownOp),
        }
    }

    fn texture_descriptor(
        &self,
        d: &cmd::TextureDesc,
    ) -> Result<Retained<MTLTextureDescriptor>, Error> {
        check(
            metal::valid_texture_type(d.texture_type)
                && metal::format_info(d.pixel_format).is_some()
                && (1..=MAX_DIM).contains(&d.width)
                && (1..=MAX_DIM).contains(&d.height)
                && (1..=2048).contains(&d.depth)
                && (1..=15).contains(&d.mip_levels)
                && (1..=2048).contains(&d.array_length)
                && d.usage & !metal::TEXTURE_USAGE_MASK == 0
                && matches!(d.sample_count, 1 | 2 | 4 | 8)
                && self
                    .device
                    .supportsTextureSampleCount(d.sample_count as usize),
        )?;
        let desc = MTLTextureDescriptor::new();
        desc.setTextureType(MTLTextureType(d.texture_type as usize));
        desc.setPixelFormat(MTLPixelFormat(d.pixel_format as usize));
        // SAFETY: dimensions, counts and type were range-checked above; Metal checks combinations
        // (a non-3D depth, say) by failing the allocation, which becomes BadValue.
        unsafe {
            desc.setWidth(d.width as usize);
            desc.setHeight(d.height as usize);
            desc.setDepth(d.depth as usize);
            desc.setMipmapLevelCount(d.mip_levels as usize);
            desc.setArrayLength(d.array_length as usize);
            desc.setSampleCount(d.sample_count as usize);
        }
        desc.setUsage(MTLTextureUsage(d.usage as usize));
        desc.setAllowGPUOptimizedContents(d.allow_gpu_optimized != 0);
        Ok(desc)
    }

    fn create_texture(&mut self, c: &cmd::CreateTexture) -> Result<[u64; 6], Error> {
        self.new_handle(c.handle)?;
        let a = self.arena(c.arena)?;
        let mut d = c.desc;
        let t = if c.bytes_per_row == 0 {
            let desc = self.texture_descriptor(&d)?;
            desc.setResourceOptions(metal::arena_options());
            let sa = self.device.heapTextureSizeAndAlignWithDescriptor(&desc);
            if sa.size == 0
                || sa.align == 0
                || !c.offset.is_multiple_of(sa.align as u64)
                || !a.contains(c.offset, sa.size as u64)
            {
                return Err(Error::BadRange);
            }
            // SAFETY: offset is aligned and the texture fits inside the heap.
            unsafe {
                a.heap
                    .newTextureWithDescriptor_offset(&desc, c.offset as usize)
            }
        } else {
            // A linear texture: Metal takes a 2D array of one layer only as a 2D texture.
            if d.texture_type == MTLTextureType::Type2DArray.0 as u32 && d.array_length == 1 {
                d.texture_type = MTLTextureType::Type2D.0 as u32;
            }
            check(
                d.texture_type == MTLTextureType::Type2D.0 as u32
                    && d.mip_levels == 1
                    && d.sample_count == 1,
            )?;
            let desc = self.texture_descriptor(&d)?;
            desc.setResourceOptions(metal::arena_options());
            let fmt = metal::format_info(d.pixel_format).ok_or(Error::BadValue)?;
            let align = self
                .device
                .minimumLinearTextureAlignmentForPixelFormat(MTLPixelFormat(
                    d.pixel_format as usize,
                )) as u64;
            let len = linear_extent(fmt, [d.width, d.height, 1], c.bytes_per_row, 0)
                .ok_or(Error::BadValue)?;
            if align == 0
                || !c.offset.is_multiple_of(align)
                || !c.bytes_per_row.is_multiple_of(align)
                || !a.contains(c.offset, len)
            {
                return Err(Error::BadRange);
            }
            a.buffer.newTextureWithDescriptor_offset_bytesPerRow(
                &desc,
                c.offset as usize,
                c.bytes_per_row as usize,
            )
        }
        .ok_or(Error::BadValue)?;
        let id = resource_id(t.gpuResourceID());
        self.objects.insert(c.handle, Object::Texture(t));
        Ok([id, 0, 0, 0, 0, 0])
    }

    fn create_texture_view(&mut self, c: &cmd::CreateTextureView) -> Result<[u64; 6], Error> {
        self.new_handle(c.handle)?;
        let parent = self.texture(c.parent)?;
        let slices = parent.arrayLength()
            * match parent.textureType() {
                MTLTextureType::TypeCube | MTLTextureType::TypeCubeArray => 6,
                _ => 1,
            };
        check(
            metal::format_info(c.pixel_format).is_some()
                && metal::valid_texture_type(c.texture_type)
                && c.level_count != 0
                && c.slice_count != 0
                && c.base_level as usize + c.level_count as usize <= parent.mipmapLevelCount()
                && c.base_slice as usize + c.slice_count as usize <= slices.max(parent.depth())
                && c.swizzle.iter().all(|&s| s <= 5),
        )?;
        let fmt = MTLPixelFormat(c.pixel_format as usize);
        let ty = MTLTextureType(c.texture_type as usize);
        let levels = NSRange::new(c.base_level as usize, c.level_count as usize);
        let sl = NSRange::new(c.base_slice as usize, c.slice_count as usize);
        // SAFETY: the ranges lie inside the parent and the enums are valid; Metal refuses
        // incompatible format or type combinations by returning nil.
        let v = unsafe {
            if c.use_swizzle != 0 {
                let s = |i: usize| MTLTextureSwizzle(c.swizzle[i] as u8);
                parent.newTextureViewWithPixelFormat_textureType_levels_slices_swizzle(
                    fmt,
                    ty,
                    levels,
                    sl,
                    MTLTextureSwizzleChannels {
                        red: s(0),
                        green: s(1),
                        blue: s(2),
                        alpha: s(3),
                    },
                )
            } else {
                parent.newTextureViewWithPixelFormat_textureType_levels_slices(fmt, ty, levels, sl)
            }
        }
        .ok_or(Error::BadValue)?;
        let id = resource_id(v.gpuResourceID());
        self.objects.insert(c.handle, Object::Texture(v));
        Ok([id, 0, 0, 0, 0, 0])
    }

    fn create_sampler(&mut self, handle: u32, s: &cmd::SamplerDesc) -> Result<[u64; 6], Error> {
        self.new_handle(handle)?;
        check(
            s.min_filter <= 1
                && s.mag_filter <= 1
                && s.mip_filter <= 2
                && [s.address_s, s.address_t, s.address_r]
                    .iter()
                    .all(|&a| a <= 5)
                && s.border_color <= 2
                && s.compare_function <= 7
                && s.reduction_mode <= 2
                && s.max_anisotropy <= 16
                && s.lod_min_clamp.is_finite()
                && !s.lod_max_clamp.is_nan(),
        )?;
        let d = MTLSamplerDescriptor::new();
        d.setLodAverage(false);
        d.setSupportArgumentBuffers(true);
        d.setMinFilter(MTLSamplerMinMagFilter(s.min_filter as usize));
        d.setMagFilter(MTLSamplerMinMagFilter(s.mag_filter as usize));
        d.setMipFilter(MTLSamplerMipFilter(s.mip_filter as usize));
        d.setSAddressMode(MTLSamplerAddressMode(s.address_s as usize));
        d.setTAddressMode(MTLSamplerAddressMode(s.address_t as usize));
        d.setRAddressMode(MTLSamplerAddressMode(s.address_r as usize));
        d.setBorderColor(MTLSamplerBorderColor(s.border_color as usize));
        d.setCompareFunction(MTLCompareFunction(s.compare_function as usize));
        d.setReductionMode(MTLSamplerReductionMode(s.reduction_mode as usize));
        d.setNormalizedCoordinates(s.normalized_coordinates != 0);
        d.setMaxAnisotropy(s.max_anisotropy.max(1) as usize);
        d.setLodMinClamp(s.lod_min_clamp);
        d.setLodMaxClamp(s.lod_max_clamp);
        let st = self
            .device
            .newSamplerStateWithDescriptor(&d)
            .ok_or(Error::BadValue)?;
        let id = resource_id(st.gpuResourceID());
        self.objects.insert(handle, Object::Sampler(st));
        Ok([id, 0, 0, 0, 0, 0])
    }

    fn create_library(&mut self, c: &cmd::CreateLibrary, src: &[u8]) -> Result<[u64; 6], Error> {
        self.new_handle(c.handle)?;
        check(
            valid_language_version(c.language_version)
                && c.math_mode <= 2
                && c.math_fp_functions <= 1,
        )?;
        let opts = MTLCompileOptions::new();
        if c.language_version != 0 {
            opts.setLanguageVersion(MTLLanguageVersion(c.language_version as usize));
        }
        opts.setMathMode(MTLMathMode(c.math_mode as isize));
        opts.setMathFloatingPointFunctions(MTLMathFloatingPointFunctions(
            c.math_fp_functions as isize,
        ));
        let desc = MTL4LibraryDescriptor::new();
        desc.setSource(Some(&*text(src)?));
        desc.setOptions(Some(&opts));
        match self.compiler.newLibraryWithDescriptor_error(&desc) {
            Ok(l) => {
                self.objects.insert(c.handle, Object::Library(l));
                Ok([0; 6])
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
    ) -> Result<[u64; 6], Error> {
        self.new_handle(c.handle)?;
        check(
            c.topology_class <= 3
                && (c.raster_sample_count == 0
                    || self
                        .device
                        .supportsTextureSampleCount(c.raster_sample_count as usize))
                && c.max_vertex_amplification <= 2,
        )?;
        let desc = MTL4RenderPipelineDescriptor::new();
        desc.setVertexFunctionDescriptor(Some(&*function(
            self.library(c.vertex_library)?,
            vertex,
        )?));
        if c.fragment_library != 0 {
            desc.setFragmentFunctionDescriptor(Some(&*function(
                self.library(c.fragment_library)?,
                fragment,
            )?));
        }
        for (i, &fmt) in c.color_formats.iter().enumerate() {
            if fmt == 0 {
                continue;
            }
            check(metal::format_info(fmt).is_some())?;
            // SAFETY: i < MAX_COLOR_ATTACHMENTS (8), the Metal limit.
            unsafe { desc.colorAttachments().objectAtIndexedSubscript(i) }
                .setPixelFormat(MTLPixelFormat(fmt as usize));
        }
        desc.setInputPrimitiveTopology(MTLPrimitiveTopologyClass(c.topology_class as usize));
        desc.setRasterizationEnabled(c.rasterization_enabled != 0);
        // SAFETY: a sample count the device supports, and an amplification count of at most 2,
        // which every Metal 4 device takes.
        unsafe {
            desc.setRasterSampleCount(c.raster_sample_count.max(1) as usize);
            if c.max_vertex_amplification != 0 {
                desc.setMaxVertexAmplificationCount(c.max_vertex_amplification as usize);
            }
        }
        match self
            .compiler
            .newRenderPipelineStateWithDescriptor_compilerTaskOptions_error(&desc, None)
        {
            Ok(ps) => {
                self.objects.insert(c.handle, Object::RenderPipeline(ps));
                Ok([0; 6])
            }
            Err(e) => {
                log::warn!("render pipeline failed: {}", e.localizedDescription());
                Err(Error::PipelineCreate)
            }
        }
    }

    fn create_compute_pipeline(
        &mut self,
        c: &cmd::CreateComputePipeline,
        name: &[u8],
    ) -> Result<[u64; 6], Error> {
        self.new_handle(c.handle)?;
        check(c.max_total_threads <= 1024)?;
        let desc = MTL4ComputePipelineDescriptor::new();
        desc.setComputeFunctionDescriptor(Some(&*function(self.library(c.library)?, name)?));
        if c.max_total_threads != 0 {
            desc.setMaxTotalThreadsPerThreadgroup(c.max_total_threads as usize);
        }
        match self
            .compiler
            .newComputePipelineStateWithDescriptor_compilerTaskOptions_error(&desc, None)
        {
            Ok(ps) => {
                let max = ps.maxTotalThreadsPerThreadgroup() as u64;
                self.objects.insert(c.handle, Object::ComputePipeline(ps));
                Ok([max, 0, 0, 0, 0, 0])
            }
            Err(e) => {
                log::warn!("compute pipeline failed: {}", e.localizedDescription());
                Err(Error::PipelineCreate)
            }
        }
    }

    fn create_depth_stencil(&mut self, c: &cmd::CreateDepthStencil) -> R {
        self.new_handle(c.handle)?;
        check(c.depth_compare <= 7)?;
        let stencil = |s: &cmd::StencilDesc| -> Result<Retained<MTLStencilDescriptor>, Error> {
            check(
                s.stencil_failure <= 7
                    && s.depth_failure <= 7
                    && s.depth_stencil_pass <= 7
                    && s.compare_function <= 7,
            )?;
            let d = MTLStencilDescriptor::new();
            d.setStencilFailureOperation(MTLStencilOperation(s.stencil_failure as usize));
            d.setDepthFailureOperation(MTLStencilOperation(s.depth_failure as usize));
            d.setDepthStencilPassOperation(MTLStencilOperation(s.depth_stencil_pass as usize));
            d.setStencilCompareFunction(MTLCompareFunction(s.compare_function as usize));
            d.setReadMask(s.read_mask);
            d.setWriteMask(s.write_mask);
            Ok(d)
        };
        let d = MTLDepthStencilDescriptor::new();
        d.setDepthCompareFunction(MTLCompareFunction(c.depth_compare as usize));
        d.setDepthWriteEnabled(c.depth_write != 0);
        if c.has_front != 0 {
            d.setFrontFaceStencil(Some(&*stencil(&c.front)?));
        }
        if c.has_back != 0 {
            d.setBackFaceStencil(Some(&*stencil(&c.back)?));
        }
        let s = self
            .device
            .newDepthStencilStateWithDescriptor(&d)
            .ok_or(Error::BadValue)?;
        self.objects.insert(c.handle, Object::DepthStencil(s));
        Ok(())
    }

    fn create_sync(&mut self, c: &cmd::SyncObject) -> R {
        self.new_handle(c.handle)?;
        let o = match c.kind {
            cmd::sync_kind::FENCE => {
                Object::Fence(self.device.newFence().ok_or(Error::OutOfMemory)?)
            }
            cmd::sync_kind::EVENT => {
                Object::Event(self.device.newEvent().ok_or(Error::OutOfMemory)?)
            }
            cmd::sync_kind::SHARED_EVENT => {
                let e = self.device.newSharedEvent().ok_or(Error::OutOfMemory)?;
                e.setSignaledValue(c.initial);
                let mirror = if c.mirror.is_none() {
                    None
                } else {
                    let (arena, _) = self.host(c.mirror, 8, 8)?;
                    let m = Mirror {
                        arena,
                        offset: c.mirror.offset,
                    };
                    m.raise(c.initial);
                    Some(m)
                };
                Object::SharedEvent(e, mirror)
            }
            _ => return Err(Error::BadValue),
        };
        self.objects.insert(c.handle, o);
        Ok(())
    }

    fn texture_transfer(&self, c: &cmd::TextureTransfer) -> R {
        let t = self.texture(c.texture)?;
        check_region(t, c.level, c.slice, c.origin, c.size)?;
        let fmt = metal::format_info(t.pixelFormat().0 as u32).ok_or(Error::BadValue)?;
        let len = linear_extent(fmt, c.size, c.bytes_per_row, c.bytes_per_image)
            .ok_or(Error::BadValue)?;
        let (_a, p) = self.host(c.data, len, 1)?;
        let region = MTLRegion {
            origin: origin3(c.origin),
            size: size3(c.size),
        };
        let p = NonNull::new(p.cast::<c_void>()).ok_or(Error::BadRange)?;
        // SAFETY: the region lies inside the level and `len` bytes at `p` inside the arena.
        unsafe {
            if c.to_texture == 0 {
                t.getBytes_bytesPerRow_bytesPerImage_fromRegion_mipmapLevel_slice(
                    p,
                    c.bytes_per_row as usize,
                    c.bytes_per_image as usize,
                    region,
                    c.level as usize,
                    c.slice as usize,
                );
            } else {
                t.replaceRegion_mipmapLevel_slice_withBytes_bytesPerRow_bytesPerImage(
                    region,
                    c.level as usize,
                    c.slice as usize,
                    p,
                    c.bytes_per_row as usize,
                    c.bytes_per_image as usize,
                );
            }
        }
        Ok(())
    }

    fn query_device(&self, dst: BufRef) -> R {
        let d = &self.device;
        let mut info = cmd::DeviceInfo {
            version: limina_gpu_wire::VERSION,
            ..Default::default()
        };
        let copy = |dst: &mut [u8; 32], s: &NSString| {
            let s = s.to_string();
            let n = s.len().min(dst.len() - 1);
            dst[..n].copy_from_slice(&s.as_bytes()[..n]);
        };
        copy(&mut info.name, &d.name());
        copy(&mut info.architecture, &d.architecture().name());
        let mut family = 0;
        while d.supportsFamily(MTLGPUFamily(MTLGPUFamily::Apple1.0 + family)) {
            family += 1;
        }
        info.apple_family = family as u32;
        info.registry_id = d.registryID();
        for n in 0..32 {
            if d.supportsTextureSampleCount(n) {
                info.sample_counts |= 1 << n;
            }
        }
        info.max_threadgroup_memory = d.maxThreadgroupMemoryLength() as u32;
        let t = d.maxThreadsPerThreadgroup();
        info.max_threads_per_threadgroup = [t.width as u32, t.height as u32, t.depth as u32];
        info.max_argument_buffer_samplers = d.maxArgumentBufferSamplerCount() as u32;
        info.max_buffer_length = d.maxBufferLength() as u64;
        info.recommended_max_working_set = d.recommendedMaxWorkingSetSize();
        info.timestamp_frequency = d.queryTimestampFrequency();
        info.sparse_tile_bytes = d.sparseTileSizeInBytes() as u32;
        info.heap_buffer_align = d
            .heapBufferSizeAndAlignWithLength_options(1, metal::arena_options())
            .align as u32;
        let (_a, p) = self.host(dst, size_of::<cmd::DeviceInfo>() as u64, 8)?;
        // SAFETY: in the arena, aligned and in bounds.
        unsafe { p.cast::<cmd::DeviceInfo>().write(info) };
        Ok(())
    }

    fn begin_render_pass(&mut self, c: &cmd::BeginRenderPass) -> R {
        self.open_cb()?;
        let desc = MTL4RenderPassDescriptor::new();
        let attach = |att: &MTLRenderPassAttachmentDescriptor, a: &cmd::RenderAttachment| -> R {
            let t = self.texture(a.texture)?;
            let slices = t.arrayLength().max(t.depth())
                * match t.textureType() {
                    MTLTextureType::TypeCube | MTLTextureType::TypeCubeArray => 6,
                    _ => 1,
                };
            check(
                a.load_action <= 2
                    && a.store_action <= 5
                    && (a.level as usize) < t.mipmapLevelCount()
                    && (a.slice as usize) < slices
                    && (a.depth_plane as usize) < t.depth().max(1),
            )?;
            att.setTexture(Some(t));
            att.setLevel(a.level as usize);
            att.setSlice(a.slice as usize);
            att.setDepthPlane(a.depth_plane as usize);
            att.setLoadAction(MTLLoadAction(a.load_action as usize));
            att.setStoreAction(MTLStoreAction(a.store_action as usize));
            if a.resolve_texture != 0 {
                let r = self.texture(a.resolve_texture)?;
                check((a.resolve_level as usize) < r.mipmapLevelCount())?;
                att.setResolveTexture(Some(r));
                att.setResolveLevel(a.resolve_level as usize);
                att.setResolveSlice(a.resolve_slice as usize);
                att.setResolveDepthPlane(a.resolve_depth_plane as usize);
            }
            Ok(())
        };
        let mut any = false;
        for (i, a) in c.colors.iter().enumerate() {
            if a.texture == 0 {
                continue;
            }
            check(a.clear.iter().all(|f| f.is_finite()))?;
            // SAFETY: i < MAX_COLOR_ATTACHMENTS (8), the Metal limit.
            let att = unsafe { desc.colorAttachments().objectAtIndexedSubscript(i) };
            attach(&att, a)?;
            att.setClearColor(MTLClearColor {
                red: a.clear[0],
                green: a.clear[1],
                blue: a.clear[2],
                alpha: a.clear[3],
            });
            any = true;
        }
        if c.depth.texture != 0 {
            check(c.depth.resolve_filter <= 2 && (0.0..=1.0).contains(&c.depth.clear[0]))?;
            let att = desc.depthAttachment();
            attach(&att, &c.depth)?;
            att.setClearDepth(c.depth.clear[0]);
            att.setDepthResolveFilter(MTLMultisampleDepthResolveFilter(
                c.depth.resolve_filter as usize,
            ));
            any = true;
        }
        if c.stencil.texture != 0 {
            check(c.stencil.resolve_filter <= 1)?;
            let att = desc.stencilAttachment();
            attach(&att, &c.stencil)?;
            att.setClearStencil(c.stencil.clear_stencil);
            att.setStencilResolveFilter(MTLMultisampleStencilResolveFilter(
                c.stencil.resolve_filter as usize,
            ));
            any = true;
        }
        check(
            c.width <= MAX_DIM
                && c.height <= MAX_DIM
                && c.array_length <= 2048
                && c.sample_position_count as usize <= cmd::MAX_SAMPLE_POSITIONS
                && c.sample_positions.iter().all(|f| (0.0..1.0).contains(f)),
        )?;
        if c.width != 0 && c.height != 0 {
            desc.setRenderTargetWidth(c.width as usize);
            desc.setRenderTargetHeight(c.height as usize);
        }
        if c.array_length != 0 {
            desc.setRenderTargetArrayLength(c.array_length as usize);
        }
        // An attachment-less pass with no sample count makes AGX abort; one sample is what such a
        // pass means.
        let samples = match c.default_raster_sample_count {
            0 if !any => 1,
            n => n,
        };
        if samples != 0 {
            check(self.device.supportsTextureSampleCount(samples as usize))?;
            // SAFETY: a sample count the device supports.
            unsafe { desc.setDefaultRasterSampleCount(samples as usize) };
        }
        if c.sample_position_count != 0 {
            let pos: Vec<MTLSamplePosition> = c.sample_positions
                [..2 * c.sample_position_count as usize]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| MTLSamplePosition { x: p[0], y: p[1] })
                .collect();
            // SAFETY: `count` positions inside the pixel, at most 16.
            unsafe { desc.setSamplePositions_count(pos.as_ptr(), pos.len()) };
        }
        let visibility = if c.visibility.is_none() {
            None
        } else {
            let a = self.range(c.visibility, 8)?;
            // SAFETY: the arena's buffer is resident with the rest of the arena.
            unsafe { desc.setVisibilityResultBuffer(Some(&a.buffer)) };
            Some((a, c.visibility.offset))
        };
        let enc = self
            .open_cb()?
            .renderCommandEncoderWithDescriptor(&desc)
            .ok_or(Error::BadValue)?;
        self.enc = Encoder::Render { enc, visibility };
        Ok(())
    }

    fn render_state(&self, s: &cmd::RenderState) -> R {
        let Encoder::Render { enc, visibility } = &self.enc else {
            return Err(Error::BadState);
        };
        use cmd::render_state as rs;
        match s.kind {
            rs::STENCIL_REFERENCE => enc.setStencilFrontReferenceValue_backReferenceValue(s.a, s.b),
            rs::WINDING => {
                check(s.a <= 1)?;
                enc.setFrontFacingWinding(MTLWinding(s.a as usize));
            }
            rs::CULL_MODE => {
                check(s.a <= 2)?;
                enc.setCullMode(MTLCullMode(s.a as usize));
            }
            rs::DEPTH_CLIP_MODE => {
                check(s.a <= 1)?;
                enc.setDepthClipMode(MTLDepthClipMode(s.a as usize));
            }
            rs::DEPTH_BIAS => {
                check(s.f[..3].iter().all(|f| f.is_finite()))?;
                enc.setDepthBias_slopeScale_clamp(s.f[0], s.f[1], s.f[2]);
            }
            rs::DEPTH_TEST_BOUNDS => {
                check(s.f[..2].iter().all(|f| (0.0..=1.0).contains(f)) && s.f[0] <= s.f[1])?;
                enc.setDepthTestMinBound_maxBound(s.f[0], s.f[1]);
            }
            rs::VISIBILITY_RESULT => {
                check(s.a <= 2)?;
                let offset = if s.a == 0 {
                    0
                } else {
                    let (a, base) = visibility.as_ref().ok_or(Error::BadState)?;
                    let off = base.checked_add(s.offset).ok_or(Error::BadRange)?;
                    if !off.is_multiple_of(8) || !a.contains(off, 8) {
                        return Err(Error::BadRange);
                    }
                    off
                };
                // SAFETY: the offset lies inside the pass's visibility buffer, 8-aligned.
                unsafe {
                    enc.setVisibilityResultMode_offset(
                        MTLVisibilityResultMode(s.a as usize),
                        offset as usize,
                    )
                };
            }
            rs::COLOR_STORE_ACTION => {
                check(s.a <= 5 && (s.b as usize) < cmd::MAX_COLOR_ATTACHMENTS)?;
                // SAFETY: a valid action and attachment index.
                unsafe {
                    enc.setColorStoreAction_atIndex(MTLStoreAction(s.a as usize), s.b as usize)
                };
            }
            rs::DEPTH_STORE_ACTION => {
                check(s.a <= 5)?;
                enc.setDepthStoreAction(MTLStoreAction(s.a as usize));
            }
            rs::STENCIL_STORE_ACTION => {
                check(s.a <= 5)?;
                enc.setStencilStoreAction(MTLStoreAction(s.a as usize));
            }
            _ => return Err(Error::BadValue),
        }
        Ok(())
    }

    fn copy_buffer_texture(&self, c: &cmd::CopyBufferTexture) -> R {
        let t = self.texture(c.texture)?;
        check_region(t, c.level, c.slice, c.origin, c.size)?;
        check(c.options <= 2)?;
        let fmt =
            metal::copy_format_info(t.pixelFormat().0 as u32, c.options).ok_or(Error::BadValue)?;
        let len = linear_extent(fmt, c.size, c.bytes_per_row, c.bytes_per_image)
            .ok_or(Error::BadValue)?;
        let buf = self.range(c.buffer, len)?;
        let enc = self.compute()?;
        let opts = MTLBlitOption(c.options as usize);
        // SAFETY: the texture region lies inside the level and the buffer range inside the arena,
        // both checked above.
        unsafe {
            if c.to_texture == 0 {
                enc.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage_options(
                    t,
                    c.slice as usize,
                    c.level as usize,
                    origin3(c.origin),
                    size3(c.size),
                    &buf.buffer,
                    c.buffer.offset as usize,
                    c.bytes_per_row as usize,
                    c.bytes_per_image as usize,
                    opts,
                );
            } else {
                enc.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin_options(
                    &buf.buffer,
                    c.buffer.offset as usize,
                    c.bytes_per_row as usize,
                    c.bytes_per_image as usize,
                    size3(c.size),
                    t,
                    c.slice as usize,
                    c.level as usize,
                    origin3(c.origin),
                    opts,
                );
            }
        }
        Ok(())
    }

    fn queue_event(&self, c: &cmd::QueueEvent) -> R {
        if !self.ended.is_empty() {
            return Err(Error::BadState);
        }
        let event: &ProtocolObject<dyn MTLEvent> = match self.objects.get(&c.event) {
            Some(Object::Event(e)) => e,
            Some(Object::SharedEvent(e, _)) => ProtocolObject::from_ref(&**e),
            _ => return Err(Error::BadHandle),
        };
        if c.wait != 0 {
            self.queue.waitForEvent_value(event, c.value);
            return Ok(());
        }
        self.queue.signalEvent_value(event, c.value);
        if let Some(Object::SharedEvent(e, Some(m))) = self.objects.get(&c.event) {
            let m = m.clone();
            let block = RcBlock::new(
                move |_e: NonNull<ProtocolObject<dyn MTLSharedEvent>>, v: u64| {
                    m.raise(v);
                },
            );
            // SAFETY: Metal copies the block; it holds the mirror's arena.
            unsafe {
                e.notifyListener_atValue_block(&self.listener, c.value, RcBlock::as_ptr(&block))
            };
        }
        Ok(())
    }

    fn commit(&mut self, seqno: u64, feedback: BufRef) -> R {
        if !matches!(self.enc, Encoder::None) || self.open.is_some() || seqno <= self.last_seqno {
            return Err(Error::BadState);
        }
        let fb = if feedback.is_none() {
            None
        } else {
            Some(self.host(feedback, size_of::<Feedback>() as u64, 8)?)
        };
        if self.ended.is_empty() {
            // The seqno still needs a command buffer to complete with.
            self.begin_command_buffer()?;
            let (cb, alloc) = self.open.take().expect("just begun");
            cb.endCommandBuffer();
            self.ended.push((cb, alloc));
        }
        self.last_seqno = seqno;
        self.commits += 1;
        let generation = self.commits;
        lock(&self.in_flight).insert(generation);

        let (cbs, allocs): (Vec<_>, Vec<_>) = self.ended.drain(..).unzip();
        // Arena memory must outlive the GPU's use of it even if the guest destroys the arena
        // meanwhile; the feedback handler holds it, and the allocators, until the GPU is done.
        let mut keep: Vec<Arc<Arena>> = self.resident.values().cloned().collect();
        keep.push(self.ring_arena.clone());
        let ctrl = SendPtr(self.consumer.control_ptr());
        let pool = self.allocators.clone();
        let in_flight = self.in_flight.clone();
        let fb = fb.map(|(a, p)| (a, SendPtr(NonNull::new(p.cast()).expect("arena pointer"))));
        let allocs = Mutex::new(allocs);
        let block = RcBlock::new(move |f: NonNull<ProtocolObject<dyn MTL4CommitFeedback>>| {
            let _keep = &keep;
            // SAFETY: the ring's arena is in `keep`, so the control block is still mapped.
            let c = unsafe { ctrl.0.as_ref() };
            // SAFETY: Metal passes the live feedback object.
            let f = unsafe { f.as_ref() };
            let error = f.error();
            if let Some(e) = &error {
                log::warn!("commit {seqno} failed: {}", e.localizedDescription());
                c.fail(Error::GpuFault, 0);
            }
            if let Some((_a, p)) = &fb {
                let p = p.0.as_ptr().cast::<Feedback>();
                // SAFETY: checked in the arena, which `_a` holds; `done` last, with release.
                unsafe {
                    (&raw mut (*p).error).write_volatile(error.map_or(0, |e| e.code() as u32));
                    (&raw mut (*p).gpu_start).write_volatile(f.GPUStartTime());
                    (&raw mut (*p).gpu_end).write_volatile(f.GPUEndTime());
                    (*(&raw mut (*p).done).cast::<AtomicU32>())
                        .store(REPLY_DONE, Ordering::Release);
                }
            }
            lock(&pool).append(&mut lock(&allocs));
            lock(&in_flight).remove(&generation);
            c.complete(seqno);
        });
        let options = MTL4CommitOptions::new();
        // SAFETY: Metal copies the block.
        unsafe { options.addFeedbackHandler(RcBlock::as_ptr(&block)) };
        let mut ptrs: Vec<NonNull<ProtocolObject<dyn MTL4CommandBuffer>>> =
            cbs.iter().map(|cb| NonNull::from(&**cb)).collect();
        // SAFETY: live, ended command buffers.
        unsafe {
            self.queue.commit_count_options(
                NonNull::from(&mut ptrs[..]).cast(),
                ptrs.len(),
                &options,
            )
        };
        self.reap();
        Ok(())
    }
}
