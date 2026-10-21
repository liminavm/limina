//! Metal's compute classes, as much as the spike's test reaches, over limina's Metal-level
//! transport (`limina-gpu-client`). A command buffer records into its own list; commit turns the
//! list into ring commands under the device's lock, and a completion thread runs the handlers.

use std::ffi::{CString, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use limina_gpu_client::{Gpu, MIB, find_node};
use limina_gpu_wire::cmd::{self, BufRef};
use limina_gpu_wire::ring::PushError;

use crate::blocks::{_Block_copy, _Block_release, Block};
use crate::foundation::{Error, NSString, fail, string, text};
use crate::imp;
use crate::pool::{Pool, QUEUES, QueueState, Region};
use crate::runtime::{Class, Def, Obj, Sel, alloc, dealloc, new, send0};

// The main arena (64 MiB): the ring takes the first 256 KiB.
const REPLY: u64 = MIB;
const INFO: u64 = MIB + 4096;
/// Shared events' signaled values, which the host mirrors here, one u64 each.
const EVENTS: u64 = MIB + 8192;
const EVENTS_END: u64 = 2 * MIB;
/// Queues' completed seqnos, which the host stores here, one u64 each.
const QUEUE_SLOTS: u64 = 2 * MIB;
const QUEUE_SLOTS_END: u64 = 2 * MIB + 64 * 1024;
/// `setBytes` data's alignment in a command buffer's scratch.
const BYTES_ALIGN: u64 = 256;

/// Argument-table slots: Metal's buffer indices 0..31.
const BINDINGS: u32 = 31;
const STAGE_DISPATCH: u32 = 1 << 26;
const STAGE_BLIT: u32 = 1 << 27;
const VISIBILITY_DEVICE: u32 = 1;

/// `MTLCommandBufferStatus`.
const STATUS_COMMITTED: usize = 2;
const STATUS_COMPLETED: usize = 4;
const STATUS_ERROR: usize = 5;

/// `MTL::Size`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Size {
    width: usize,
    height: usize,
    depth: usize,
}

impl Size {
    fn wire(self) -> [u32; 3] {
        [self.width as u32, self.height as u32, self.depth as u32]
    }
}

struct Ptr<T>(*mut T);
// SAFETY: the objects these name are reference counted and outlive their place in a list.
unsafe impl<T> Send for Ptr<T> {}

struct State {
    gpu: Gpu,
    next_handle: u32,
    /// Committed command buffers, each with its last seqno and its queue.
    pending: Vec<(u64, &'static QueueState, Ptr<Obj<CommandBuffer>>)>,
    lost: Option<String>,
    next_event_slot: u64,
    free_event_slots: Vec<u64>,
    next_queue_slot: u64,
}

pub struct Device {
    state: Mutex<State>,
    done: Condvar,
    name: String,
    max_threads: Size,
    info: cmd::DeviceInfo,
    pool: Mutex<Pool>,
}

impl State {
    fn handle(&mut self) -> u32 {
        self.next_handle += 1;
        self.next_handle
    }

    /// Pushes `c` with `extra`, publishing and waiting for the host to make room when the ring is
    /// full: one command buffer can hold more than the ring does.
    fn push<T: cmd::Command>(&mut self, c: T, extra: &[&[u8]]) {
        loop {
            match self.gpu.ring.push(&c, extra) {
                Ok(()) => return,
                Err(PushError::Full) => {
                    self.gpu.kick();
                    if let Some((e, at)) = self.gpu.ring.error() {
                        eprintln!("limina-metal: context lost: error {e} at ring position {at}");
                        std::process::abort();
                    }
                    std::thread::yield_now();
                }
                Err(PushError::TooLarge) => {
                    eprintln!("limina-metal: a command larger than half the ring");
                    std::process::abort();
                }
            }
        }
    }

    /// Commits what was pushed to `q` without waiting for it, through [`push`](Self::push); its
    /// seqno.
    fn commit(&mut self, q: &QueueState) -> u64 {
        self.gpu.seqno += 1;
        let seqno = self.gpu.seqno;
        // Before the push, so a free never misses a commit already on the ring.
        q.committed.store(seqno, Ordering::Release);
        self.push(
            cmd::Commit {
                seqno,
                queue: q.handle,
                feedback: BufRef::NONE,
                ..Default::default()
            },
            &[],
        );
        self.gpu.kick();
        seqno
    }

    /// Pushes `c` with `extra` and waits for its reply.
    fn call<T: cmd::Command>(
        &mut self,
        c: impl FnOnce(BufRef) -> T,
        extra: &[&[u8]],
    ) -> Result<cmd::Reply, String> {
        let reply = self.gpu.reply_slot(REPLY);
        self.push(c(reply), extra);
        let r = self.gpu.reply(REPLY)?;
        match r.status {
            0 => Ok(r),
            s => Err(format!("the GPU host refused it: error {s}")),
        }
    }
}

/// `LIMINA_METAL_TRACE=1`: one line per commit and event to stderr.
fn trace() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| std::env::var_os("LIMINA_METAL_TRACE").is_some_and(|v| v != "0"))
}

macro_rules! trace {
    ($($a:tt)*) => {
        if trace() {
            eprintln!("[limina-metal {:?}] {}", std::thread::current().id(), format!($($a)*));
        }
    };
}

/// Where buffers and scratch come from.
pub fn pool() -> &'static Mutex<Pool> {
    &device().v.pool
}

fn device() -> &'static Obj<Device> {
    static D: OnceLock<usize> = OnceLock::new();
    let p = *D.get_or_init(|| {
        let (path, mut gpu) =
            find_node().expect("limina-metal: no render node with the limina GPU extension");
        let reply = gpu.reply_slot(REPLY);
        gpu.push(
            cmd::QueryDevice {
                dst: BufRef::new(1, INFO),
                reply,
            },
            &[],
        );
        let r = gpu.reply(REPLY).expect("limina-metal: QueryDevice");
        assert_eq!(r.status, 0, "limina-metal: QueryDevice failed");
        let b = gpu.main.read(INFO, size_of::<cmd::DeviceInfo>());
        // SAFETY: the host wrote a DeviceInfo there before the reply; any bytes are a valid one.
        let info = unsafe { b.as_ptr().cast::<cmd::DeviceInfo>().read_unaligned() };
        let want = cmd::feature::LIBRARY_DATA | cmd::feature::FUNCTION_CONSTANTS;
        if info.features & want != want {
            eprintln!(
                "limina-metal: {path}: the GPU host lacks features {:#x}",
                want & !info.features
            );
            std::process::abort();
        }
        let name = String::from_utf8_lossy(&info.name)
            .trim_end_matches('\0')
            .to_owned();
        let [w, h, d] = info.max_threads_per_threadgroup;
        // The same context, through a second descriptor, so the pool needs no lock of State's.
        let file = gpu
            .file
            .try_clone()
            .expect("limina-metal: duplicating the render node's descriptor");
        let dev = new(
            "MTLDevice",
            Device {
                state: Mutex::new(State {
                    gpu,
                    next_handle: 0,
                    pending: Vec::new(),
                    lost: None,
                    next_event_slot: EVENTS,
                    free_event_slots: Vec::new(),
                    next_queue_slot: QUEUE_SLOTS,
                }),
                done: Condvar::new(),
                name: format!("{name} (limina)"),
                max_threads: Size {
                    width: w as usize,
                    height: h as usize,
                    depth: d as usize,
                },
                info,
                pool: Pool::new(file, info.heap_buffer_align as u64),
            },
        );
        std::thread::Builder::new()
            .name("limina-metal-completion".into())
            .spawn(completion)
            .unwrap();
        dev as usize
    });
    // SAFETY: made once and never released.
    unsafe { &*(p as *const Obj<Device>) }
}

/// Retires committed command buffers as the host completes them: marks them complete, runs their
/// handlers, which see that status as in Metal, and only then wakes anyone waiting.
fn completion() {
    let dev = device();
    loop {
        let finished: Vec<_> = {
            let mut s = dev.v.state.lock().unwrap();
            if let Some((e, at)) = s.gpu.ring.error() {
                s.lost = Some(format!("context lost: error {e} at ring position {at}"));
            }
            let lost = s.lost.is_some();
            let (finished, still): (Vec<_>, Vec<_>) = s
                .pending
                .drain(..)
                .partition(|(seqno, q, _)| lost || q.done(*seqno));
            s.pending = still;
            finished
        };
        pool().lock().unwrap().reap();
        let status = if dev.v.state.lock().unwrap().lost.is_some() {
            STATUS_ERROR
        } else {
            STATUS_COMPLETED
        };
        for (_, _, cb) in &finished {
            // SAFETY: the pending list holds a reference to each command buffer.
            let c = unsafe { &(*cb.0).v };
            *c.status.lock().unwrap() = status;
            let handlers = std::mem::take(&mut *c.handlers.lock().unwrap());
            for h in handlers {
                let block = h.0 as *mut Block;
                // SAFETY: a copied CommandBufferHandler block: invoke(block, commandBuffer).
                unsafe {
                    let f: extern "C" fn(*mut Block, *mut Obj<CommandBuffer>) =
                        std::mem::transmute((*block).invoke);
                    f(block, cb.0);
                    _Block_release(block.cast());
                }
            }
            // Its setBytes data, which the GPU is done reading.
            drop(c.scratch.lock().unwrap().take());
            *c.retired.lock().unwrap() = true;
        }
        if !finished.is_empty() {
            dev.v.done.notify_all();
            for (_, _, cb) in finished {
                // SAFETY: the reference the pending list held.
                unsafe { send0(cb.0.cast(), "release") };
            }
        }
        std::thread::sleep(Duration::from_micros(50));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn MTLCreateSystemDefaultDevice() -> *mut Obj<Device> {
    let d = device() as *const Obj<Device> as *mut Obj<Device>;
    // SAFETY: the caller owns the reference a create function returns.
    unsafe { send0(d.cast(), "retain") };
    d
}

fn autoreleased<T>(o: *mut Obj<T>) -> *mut Obj<T> {
    // SAFETY: a new object.
    unsafe { send0(o.cast(), "autorelease") };
    o
}

unsafe extern "C" fn device_name(this: *const Obj<Device>, _: Sel) -> *mut NSString {
    // SAFETY: a live device.
    autoreleased(string(unsafe { &(*this).v.name }))
}

/// The device lives as long as the process.
extern "C" fn device_dealloc(_: *mut Obj<Device>, _: Sel) {}

unsafe extern "C" fn device_max_threads(this: *const Obj<Device>, _: Sel) -> Size {
    // SAFETY: a live device.
    unsafe { (*this).v.max_threads }
}

/// Metal's list of devices. Nil, which MLX takes as "use the default device", spares an NSArray.
#[unsafe(no_mangle)]
pub extern "C" fn MTLCopyAllDevices() -> *mut c_void {
    std::ptr::null_mut()
}

fn info(this: *const Obj<Device>) -> &'static cmd::DeviceInfo {
    // SAFETY: the device lives as long as the process.
    unsafe { &(*this).v.info }
}

pub struct Architecture {
    name: String,
}

unsafe extern "C" fn device_architecture(
    this: *const Obj<Device>,
    _: Sel,
) -> *mut Obj<Architecture> {
    let name = String::from_utf8_lossy(&info(this).architecture)
        .trim_end_matches('\0')
        .to_owned();
    autoreleased(new("MTLArchitecture", Architecture { name }))
}

unsafe extern "C" fn architecture_name(this: *const Obj<Architecture>, _: Sel) -> *mut NSString {
    // SAFETY: a live object.
    autoreleased(string(unsafe { &(*this).v.name }))
}

/// `MTLGPUFamily`: Apple1.. up to the host's, Mac2, the Common ones, Metal3 and Metal4.
unsafe extern "C" fn device_supports_family(this: *const Obj<Device>, _: Sel, f: isize) -> bool {
    let apple = info(this).apple_family as isize;
    match f {
        1001.. if f <= 1000 + apple => true,
        2002 | 3001..=3003 | 5001 | 5002 => true,
        _ => false,
    }
}

unsafe extern "C" fn device_max_buffer_length(this: *const Obj<Device>, _: Sel) -> usize {
    info(this).max_buffer_length as usize
}

unsafe extern "C" fn device_working_set(this: *const Obj<Device>, _: Sel) -> u64 {
    info(this).recommended_max_working_set
}

unsafe extern "C" fn device_max_threadgroup_memory(this: *const Obj<Device>, _: Sel) -> usize {
    info(this).max_threadgroup_memory as usize
}

unsafe extern "C" fn device_registry_id(this: *const Obj<Device>, _: Sel) -> u64 {
    info(this).registry_id
}

extern "C" fn device_true(_: *const Obj<Device>, _: Sel) -> bool {
    true
}

#[derive(Default)]
pub struct HeapDescriptor;

extern "C" fn heap_descriptor_set(_: *mut Obj<HeapDescriptor>, _: Sel, _: usize) {}

/// No heaps yet: Metal may fail to make one, and MLX then allocates from the device.
extern "C" fn device_new_heap(
    _: *const Obj<Device>,
    _: Sel,
    _: *const Obj<HeapDescriptor>,
) -> *mut c_void {
    std::ptr::null_mut()
}

// --- Synchronisation ---

/// One queue carries every command buffer, and each encoder starts after the last's work is done,
/// so a fence between two encoders always holds.
pub struct Fence;

extern "C" fn device_new_fence(_: *const Obj<Device>, _: Sel) -> *mut Obj<Fence> {
    new("MTLFence", Fence)
}

extern "C" fn encoder_fence(_: *const Obj<Encoder>, _: Sel, _: *const Obj<Fence>) {}

/// The host mirrors the event's signaled value into a u64 of the main arena.
pub struct SharedEvent {
    handle: u32,
    slot: u64,
    mirror: *const AtomicU64,
}

unsafe extern "C" fn device_new_shared_event(
    this: *const Obj<Device>,
    _: Sel,
) -> *mut Obj<SharedEvent> {
    // SAFETY: a live device.
    let mut s = unsafe { &(*this).v }.state.lock().unwrap();
    let slot = match s.free_event_slots.pop() {
        Some(slot) => slot,
        None if s.next_event_slot < EVENTS_END => {
            s.next_event_slot += 8;
            s.next_event_slot - 8
        }
        None => {
            eprintln!("limina-metal: out of shared event slots");
            return std::ptr::null_mut();
        }
    };
    s.gpu.main.write(slot, &0u64.to_le_bytes());
    let handle = s.handle();
    s.push(
        cmd::SyncObject {
            handle,
            kind: cmd::sync_kind::SHARED_EVENT,
            initial: 0,
            mirror: BufRef::new(1, slot),
        },
        &[],
    );
    // SAFETY: inside the main arena, which lives as long as the process; aligned to 8.
    let mirror = unsafe { s.gpu.main.ptr.as_ptr().add(slot as usize) }.cast::<AtomicU64>();
    new(
        "MTLSharedEvent",
        SharedEvent {
            handle,
            slot,
            mirror,
        },
    )
}

unsafe extern "C" fn shared_event_value(this: *const Obj<SharedEvent>, _: Sel) -> u64 {
    // SAFETY: a live event, whose mirror the host stores to.
    unsafe { (*(*this).v.mirror).load(Ordering::Acquire) }
}

unsafe extern "C" fn shared_event_set_value(this: *const Obj<SharedEvent>, _: Sel, value: u64) {
    trace!("event {}: CPU sets {value}", unsafe { (*this).v.handle });
    // SAFETY: a live event.
    let handle = unsafe { (*this).v.handle };
    let mut s = device().v.state.lock().unwrap();
    s.push(
        cmd::SetSharedEvent {
            handle,
            _pad: 0,
            value,
        },
        &[],
    );
    s.gpu.kick();
}

unsafe extern "C" fn shared_event_wait(
    this: *const Obj<SharedEvent>,
    s: Sel,
    value: u64,
    timeout_ms: u64,
) -> bool {
    trace!("event {}: CPU waits for {value}", unsafe {
        (*this).v.handle
    });
    let start = Instant::now();
    let mut spins = 0u32;
    loop {
        // SAFETY: a live event.
        if unsafe { shared_event_value(this, s) } >= value {
            return true;
        }
        if device().v.state.lock().unwrap().lost.is_some()
            || start.elapsed() >= Duration::from_millis(timeout_ms)
        {
            return false;
        }
        spins += 1;
        if spins < 1000 {
            std::hint::spin_loop();
        } else {
            std::thread::sleep(Duration::from_micros(20));
        }
    }
}

unsafe extern "C" fn shared_event_dealloc(this: *mut Obj<SharedEvent>, s: Sel) {
    // SAFETY: a live event, at its last reference.
    let (handle, slot) = unsafe { ((*this).v.handle, (*this).v.slot) };
    let mut st = device().v.state.lock().unwrap();
    // The host keeps the event until the work committed before this destroy is done.
    st.push(cmd::Destroy { handle, _pad: 0 }, &[]);
    st.free_event_slots.push(slot);
    drop(st);
    // SAFETY: as in `dealloc`.
    unsafe { dealloc(this, s) };
}

/// Residency is the host's to manage: every arena is resident while the guest maps it.
#[derive(Default)]
pub struct ResidencySet;

unsafe extern "C" fn device_new_residency_set(
    _: *const Obj<Device>,
    _: Sel,
    _desc: *const c_void,
    _error: *mut *mut Obj<Error>,
) -> *mut Obj<ResidencySet> {
    new("MTLResidencySet", ResidencySet)
}

extern "C" fn residency_nop(_: *const c_void, _: Sel) {}
extern "C" fn residency_nop1(_: *const c_void, _: Sel, _: *const c_void) {}
extern "C" fn residency_nop2(_: *const c_void, _: Sel, _: *const c_void, _: usize) {}
extern "C" fn residency_size(_: *const c_void, _: Sel) -> u64 {
    0
}

// --- Buffers ---

pub struct Buffer {
    region: Region,
    length: usize,
}

fn make_buffer(length: usize) -> *mut Obj<Buffer> {
    match pool().lock().unwrap().alloc(length as u64) {
        Ok(region) => new("MTLBuffer", Buffer { region, length }),
        Err(e) => {
            eprintln!("limina-metal: a buffer of {length} bytes: {e}");
            std::ptr::null_mut()
        }
    }
}

extern "C" fn new_buffer(
    _: *const Obj<Device>,
    _: Sel,
    length: usize,
    _options: usize,
) -> *mut Obj<Buffer> {
    make_buffer(length)
}

unsafe extern "C" fn new_buffer_with_bytes(
    _: *const Obj<Device>,
    _: Sel,
    bytes: *const u8,
    length: usize,
    _options: usize,
) -> *mut Obj<Buffer> {
    let b = make_buffer(length);
    if !b.is_null() {
        // SAFETY: the caller's `length` bytes, into a new buffer at least that long.
        unsafe { std::ptr::copy_nonoverlapping(bytes, (*b).v.region.ptr, length) };
    }
    b
}

/// The GPU reaches only arenas, which cannot alias the caller's memory, so: nil, which Metal
/// returns too for memory it cannot wrap. MLX then copies.
extern "C" fn new_buffer_no_copy(
    _: *const Obj<Device>,
    _: Sel,
    _bytes: *const u8,
    _length: usize,
    _options: usize,
    _deallocator: *const c_void,
) -> *mut Obj<Buffer> {
    std::ptr::null_mut()
}

unsafe extern "C" fn buffer_contents(this: *const Obj<Buffer>, _: Sel) -> *mut c_void {
    // SAFETY: a live buffer.
    unsafe { (*this).v.region.ptr.cast() }
}

unsafe extern "C" fn buffer_length(this: *const Obj<Buffer>, _: Sel) -> usize {
    // SAFETY: a live buffer.
    unsafe { (*this).v.length }
}

extern "C" fn buffer_heap(_: *const Obj<Buffer>, _: Sel) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn buffer_allocated_size(this: *const Obj<Buffer>, _: Sel) -> usize {
    // SAFETY: a live buffer.
    unsafe { (*this).v.region.size as usize }
}

/// MTLStorageModeShared.
extern "C" fn buffer_storage_mode(_: *const Obj<Buffer>, _: Sel) -> usize {
    0
}

unsafe extern "C" fn buffer_gpu_address(this: *const Obj<Buffer>, _: Sel) -> u64 {
    // SAFETY: a live buffer.
    unsafe { (*this).v.region.gpu }
}

// --- Libraries and functions ---

pub struct Library {
    handle: u32,
}

#[derive(Default)]
pub struct CompileOptions {
    /// `MTLMathMode`, which the wire carries as is.
    math_mode: u32,
    language_version: u32,
}

unsafe extern "C" fn options_init(
    this: *mut Obj<CompileOptions>,
    _: Sel,
) -> *mut Obj<CompileOptions> {
    // Metal's default: fastMathEnabled, which is MTLMathModeFast.
    // SAFETY: a live object.
    unsafe { (*this).v.math_mode = 2 };
    this
}

unsafe extern "C" fn set_fast_math(this: *mut Obj<CompileOptions>, _: Sel, on: bool) {
    // SAFETY: a live object.
    unsafe { (*this).v.math_mode = if on { 2 } else { 0 } };
}

unsafe extern "C" fn set_math_mode(this: *mut Obj<CompileOptions>, _: Sel, mode: isize) {
    // SAFETY: a live object.
    unsafe { (*this).v.math_mode = mode as u32 };
}

unsafe extern "C" fn language_version(this: *const Obj<CompileOptions>, _: Sel) -> usize {
    // SAFETY: a live object.
    unsafe { (*this).v.language_version as usize }
}

/// Shader logging needs a log stream back from the host, which the wire does not carry.
extern "C" fn set_enable_logging(_: *mut Obj<CompileOptions>, _: Sel, _: bool) {}

unsafe extern "C" fn set_language_version(this: *mut Obj<CompileOptions>, _: Sel, v: usize) {
    // SAFETY: a live object.
    unsafe { (*this).v.language_version = v as u32 };
}

unsafe extern "C" fn new_library_with_url(
    this: *const Obj<Device>,
    _: Sel,
    url: *const Obj<crate::foundation::Url>,
    error: *mut *mut Obj<Error>,
) -> *mut Obj<Library> {
    // SAFETY: a live NSURL.
    load_library(this, unsafe { &(*url).v.path }, error)
}

unsafe extern "C" fn new_library_with_file(
    this: *const Obj<Device>,
    _: Sel,
    path: *const NSString,
    error: *mut *mut Obj<Error>,
) -> *mut Obj<Library> {
    load_library(this, &text(path), error)
}

fn load_library(
    this: *const Obj<Device>,
    path: &str,
    error: *mut *mut Obj<Error>,
) -> *mut Obj<Library> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            fail(error, format!("{path}: {e}"));
            return std::ptr::null_mut();
        }
    };
    let staged = match pool().lock().unwrap().alloc(bytes.len() as u64) {
        Ok(r) => r,
        Err(e) => {
            fail(
                error,
                format!("staging {} bytes of {path}: {e}", bytes.len()),
            );
            return std::ptr::null_mut();
        }
    };
    // SAFETY: a new region at least that long.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), staged.ptr, bytes.len()) };
    // SAFETY: a live device.
    let mut s = unsafe { &(*this).v }.state.lock().unwrap();
    let handle = s.handle();
    // The host copies the bytes before it replies, so the staging region can go after.
    match s.call(
        |reply| cmd::CreateLibraryData {
            handle,
            data_len: bytes.len() as u64,
            data: BufRef::new(staged.arena, staged.offset),
            reply,
            ..Default::default()
        },
        &[],
    ) {
        Ok(_) => new("MTLLibrary", Library { handle }),
        Err(e) => {
            fail(error, format!("{path}: {e}"));
            std::ptr::null_mut()
        }
    }
}

unsafe extern "C" fn new_library_with_source(
    this: *const Obj<Device>,
    _: Sel,
    source: *const NSString,
    options: *const Obj<CompileOptions>,
    error: *mut *mut Obj<Error>,
) -> *mut Obj<Library> {
    let src = text(source);
    let (math_mode, version) = if options.is_null() {
        (2, 0)
    } else {
        // SAFETY: a live CompileOptions.
        let o = unsafe { &(*options).v };
        (o.math_mode, o.language_version)
    };
    // SAFETY: a live device.
    let mut s = unsafe { &(*this).v }.state.lock().unwrap();
    let handle = s.handle();
    match s.call(
        |reply| cmd::CreateLibrary {
            handle,
            source_len: src.len() as u32,
            language_version: version,
            math_mode,
            reply,
            ..Default::default()
        },
        &[src.as_bytes()],
    ) {
        Ok(_) => new("MTLLibrary", Library { handle }),
        Err(e) => {
            fail(error, format!("compiling MSL: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// A function is a value: the library, the name and any constants travel with the pipeline create.
pub struct Function {
    library: u32,
    name: CString,
    constants: Vec<cmd::FunctionConstant>,
}

unsafe extern "C" fn new_function_with_name(
    this: *const Obj<Library>,
    _: Sel,
    name: *const NSString,
) -> *mut Obj<Function> {
    // SAFETY: a live library.
    let library = unsafe { (*this).v.handle };
    new(
        "MTLFunction",
        Function {
            library,
            name: CString::new(text(name)).unwrap(),
            constants: Vec::new(),
        },
    )
}

unsafe extern "C" fn function_name(this: *const Obj<Function>, _: Sel) -> *mut NSString {
    // SAFETY: a live function.
    autoreleased(string(&unsafe { &(*this).v.name }.to_string_lossy()))
}

#[derive(Default)]
pub struct ConstantValues {
    values: Vec<cmd::FunctionConstant>,
}

/// Bytes of a scalar `MTLDataType`.
fn constant_size(data_type: usize) -> Option<usize> {
    Some(match data_type {
        // Bool, Char, UChar.
        53 | 45 | 49 => 1,
        // Short, UShort, Half, BFloat.
        37 | 41 | 16 | 121 => 2,
        // Int, UInt, Float.
        29 | 33 | 3 => 4,
        // Long, ULong.
        81 | 85 => 8,
        _ => return None,
    })
}

unsafe extern "C" fn set_constant_value(
    this: *mut Obj<ConstantValues>,
    _: Sel,
    value: *const u8,
    data_type: usize,
    index: usize,
) {
    let Some(size) = constant_size(data_type) else {
        eprintln!("limina-metal: a function constant of MTLDataType {data_type} is not supported");
        std::process::abort();
    };
    let mut bytes = [0u8; 8];
    // SAFETY: the caller's value holds `size` bytes of that type.
    unsafe { std::ptr::copy_nonoverlapping(value, bytes.as_mut_ptr(), size) };
    let c = cmd::FunctionConstant {
        index: index as u32,
        data_type: data_type as u32,
        value: u64::from_le_bytes(bytes),
    };
    // SAFETY: a live object.
    let v = unsafe { &mut (*this).v.values };
    v.retain(|k| k.index != c.index);
    v.push(c);
}

#[derive(Default)]
pub struct FunctionDescriptor {
    name: String,
    constants: Vec<cmd::FunctionConstant>,
}

unsafe extern "C" fn function_descriptor(
    class: *const Class,
    s: Sel,
) -> *mut Obj<FunctionDescriptor> {
    autoreleased(alloc::<FunctionDescriptor>(class, s))
}

unsafe extern "C" fn fd_set_name(
    this: *mut Obj<FunctionDescriptor>,
    _: Sel,
    name: *const NSString,
) {
    // SAFETY: a live object.
    unsafe { (*this).v.name = text(name) };
}

unsafe extern "C" fn fd_set_specialized_name(
    _: *mut Obj<FunctionDescriptor>,
    _: Sel,
    _: *const NSString,
) {
}

unsafe extern "C" fn fd_set_constant_values(
    this: *mut Obj<FunctionDescriptor>,
    _: Sel,
    values: *const Obj<ConstantValues>,
) {
    // SAFETY: live objects.
    unsafe {
        (*this).v.constants = if values.is_null() {
            Vec::new()
        } else {
            (*values).v.values.clone()
        }
    };
}

unsafe extern "C" fn new_function_with_descriptor(
    this: *const Obj<Library>,
    _: Sel,
    desc: *const Obj<FunctionDescriptor>,
    _error: *mut *mut Obj<Error>,
) -> *mut Obj<Function> {
    // SAFETY: live objects.
    let (library, d) = unsafe { ((*this).v.handle, &(*desc).v) };
    new(
        "MTLFunction",
        Function {
            library,
            name: CString::new(d.name.clone()).unwrap(),
            constants: d.constants.clone(),
        },
    )
}

// --- Pipelines ---

pub struct Pipeline {
    handle: u32,
    max_threads: usize,
}

unsafe extern "C" fn new_compute_pipeline(
    this: *const Obj<Device>,
    _: Sel,
    function: *const Obj<Function>,
    error: *mut *mut Obj<Error>,
) -> *mut Obj<Pipeline> {
    // SAFETY: live objects.
    let (dev, f) = unsafe { (&(*this).v, &(*function).v) };
    let mut s = dev.state.lock().unwrap();
    let handle = s.handle();
    let name = f.name.as_bytes();
    match s.call(
        |reply| cmd::CreateComputePipeline {
            handle,
            library: f.library,
            name_len: name.len() as u32,
            constant_count: f.constants.len() as u32,
            reply,
            ..Default::default()
        },
        &[cmd::bytes_of_slice(&f.constants), name],
    ) {
        Ok(r) => new(
            "MTLComputePipelineState",
            Pipeline {
                handle,
                max_threads: r.values[0] as usize,
            },
        ),
        Err(e) => {
            fail(
                error,
                format!("compute pipeline {}: {e}", f.name.to_string_lossy()),
            );
            std::ptr::null_mut()
        }
    }
}

unsafe extern "C" fn pipeline_max_threads(this: *const Obj<Pipeline>, _: Sel) -> usize {
    // SAFETY: a live pipeline.
    unsafe { (*this).v.max_threads }
}

unsafe extern "C" fn pipeline_execution_width(_: *const Obj<Pipeline>, _: Sel) -> usize {
    // Every Apple GPU's SIMD-group width.
    32
}

// --- Queues, command buffers, encoders ---

/// A host `MTL4CommandQueue` of its own, which lives as long as the process: a command buffer's
/// completion is read from its queue's slot, and queues are few.
pub struct Queue {
    q: &'static QueueState,
}

unsafe extern "C" fn new_command_queue(this: *const Obj<Device>, _: Sel) -> *mut Obj<Queue> {
    // SAFETY: a live device.
    let mut s = unsafe { &(*this).v }.state.lock().unwrap();
    if s.next_queue_slot >= QUEUE_SLOTS_END {
        eprintln!("limina-metal: out of command queue slots");
        return std::ptr::null_mut();
    }
    let slot = s.next_queue_slot;
    s.next_queue_slot += 8;
    s.gpu.main.write(slot, &0u64.to_le_bytes());
    let handle = s.handle();
    s.push(
        cmd::CreateQueue {
            handle,
            completed: BufRef::new(1, slot),
            ..Default::default()
        },
        &[],
    );
    // SAFETY: inside the main arena, which lives as long as the process; aligned to 8.
    let completed = unsafe {
        &*s.gpu
            .main
            .ptr
            .as_ptr()
            .add(slot as usize)
            .cast::<AtomicU64>()
    };
    let q: &'static QueueState = Box::leak(Box::new(QueueState {
        handle,
        completed,
        committed: AtomicU64::new(0),
    }));
    QUEUES.lock().unwrap().push(q);
    trace!("queue {handle}: completed seqnos at {slot:#x}");
    new("MTLCommandQueue", Queue { q })
}

enum Op {
    BeginCompute,
    Pipeline(u32),
    Bind(u32, u64),
    Bytes(u32, Vec<u8>),
    Threads(Size, Size),
    Groups(Size, Size),
    Barrier,
    End,
    /// `encodeWaitForEvent:value:` (true) or `encodeSignalEvent:value:`.
    Event(u32, bool, u64),
}

pub struct CommandBuffer {
    queue: &'static QueueState,
    ops: Mutex<Vec<Op>>,
    handlers: Mutex<Vec<Ptr<c_void>>>,
    status: Mutex<usize>,
    /// Complete, and its handlers have run.
    retired: Mutex<bool>,
    /// Its setBytes data, from commit until it retires.
    scratch: Mutex<Option<Region>>,
}

unsafe extern "C" fn command_buffer(this: *const Obj<Queue>, _: Sel) -> *mut Obj<CommandBuffer> {
    autoreleased(new(
        "MTLCommandBuffer",
        CommandBuffer {
            // SAFETY: a live queue.
            queue: unsafe { (*this).v.q },
            ops: Mutex::new(Vec::new()),
            handlers: Mutex::new(Vec::new()),
            status: Mutex::new(0),
            retired: Mutex::new(false),
            scratch: Mutex::new(None),
        },
    ))
}

pub struct Encoder {
    cb: *mut Obj<CommandBuffer>,
}

impl Encoder {
    fn op(&self, op: Op) {
        // SAFETY: the encoder's command buffer outlives it, as in Metal.
        unsafe { (*self.cb).v.ops.lock().unwrap().push(op) };
    }
}

unsafe extern "C" fn compute_encoder(this: *mut Obj<CommandBuffer>, _: Sel) -> *mut Obj<Encoder> {
    let e = new("MTLComputeCommandEncoder", Encoder { cb: this });
    // SAFETY: just made.
    unsafe { (*e).v.op(Op::BeginCompute) };
    autoreleased(e)
}

unsafe extern "C" fn compute_encoder_with_type(
    this: *mut Obj<CommandBuffer>,
    s: Sel,
    _dispatch_type: usize,
) -> *mut Obj<Encoder> {
    // Metal 4's encoders are concurrent; MLX orders its dispatches with memoryBarrierWithScope:.
    unsafe { compute_encoder(this, s) }
}

unsafe extern "C" fn set_pipeline(this: *const Obj<Encoder>, _: Sel, p: *const Obj<Pipeline>) {
    // SAFETY: live objects.
    unsafe { (*this).v.op(Op::Pipeline((*p).v.handle)) };
}

unsafe extern "C" fn set_buffer(
    this: *const Obj<Encoder>,
    _: Sel,
    b: *const Obj<Buffer>,
    offset: usize,
    index: usize,
) {
    // SAFETY: live objects.
    unsafe {
        (*this)
            .v
            .op(Op::Bind(index as u32, (*b).v.region.gpu + offset as u64))
    };
}

unsafe extern "C" fn set_bytes(
    this: *const Obj<Encoder>,
    _: Sel,
    bytes: *const u8,
    len: usize,
    index: usize,
) {
    // SAFETY: the caller's `len` bytes; Metal copies them at the call too.
    let data = unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec();
    unsafe { (*this).v.op(Op::Bytes(index as u32, data)) };
}

unsafe extern "C" fn dispatch_threads(this: *const Obj<Encoder>, _: Sel, grid: Size, group: Size) {
    // SAFETY: a live encoder.
    unsafe { (*this).v.op(Op::Threads(grid, group)) };
}

unsafe extern "C" fn dispatch_threadgroups(
    this: *const Obj<Encoder>,
    _: Sel,
    groups: Size,
    group: Size,
) {
    // SAFETY: a live encoder.
    unsafe { (*this).v.op(Op::Groups(groups, group)) };
}

unsafe extern "C" fn memory_barrier(this: *const Obj<Encoder>, _: Sel, _scope: usize) {
    // SAFETY: a live encoder.
    unsafe { (*this).v.op(Op::Barrier) };
}

unsafe extern "C" fn end_encoding(this: *const Obj<Encoder>, _: Sel) {
    // SAFETY: a live encoder.
    unsafe { (*this).v.op(Op::End) };
}

unsafe extern "C" fn encode_event(
    this: *const Obj<CommandBuffer>,
    wait: bool,
    event: *const Obj<SharedEvent>,
    value: u64,
) {
    // SAFETY: live objects.
    unsafe {
        (*this)
            .v
            .ops
            .lock()
            .unwrap()
            .push(Op::Event((*event).v.handle, wait, value))
    };
}

unsafe extern "C" fn encode_wait(
    this: *const Obj<CommandBuffer>,
    _: Sel,
    event: *const Obj<SharedEvent>,
    value: u64,
) {
    // SAFETY: as in encode_event.
    unsafe { encode_event(this, true, event, value) };
}

unsafe extern "C" fn encode_signal(
    this: *const Obj<CommandBuffer>,
    _: Sel,
    event: *const Obj<SharedEvent>,
    value: u64,
) {
    // SAFETY: as in encode_event.
    unsafe { encode_event(this, false, event, value) };
}

unsafe extern "C" fn add_completed_handler(
    this: *const Obj<CommandBuffer>,
    _: Sel,
    block: *const c_void,
) {
    // SAFETY: a live command buffer; the block is copied, as Metal does.
    let b = unsafe { _Block_copy(block) };
    unsafe { (*this).v.handlers.lock().unwrap().push(Ptr(b)) };
}

unsafe extern "C" fn commit(this: *mut Obj<CommandBuffer>, _: Sel) {
    let dev = device();
    // SAFETY: a live command buffer.
    let ops = std::mem::take(&mut *unsafe { &(*this).v }.ops.lock().unwrap());
    // One region for all its setBytes data, freed when it retires.
    let bytes: u64 = ops
        .iter()
        .map(|op| match op {
            Op::Bytes(_, d) => (d.len() as u64).next_multiple_of(BYTES_ALIGN),
            _ => 0,
        })
        .sum();
    let scratch = (bytes > 0).then(|| match pool().lock().unwrap().alloc(bytes) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("limina-metal: {bytes} bytes of setBytes data: {e}");
            std::process::abort();
        }
    });
    // SAFETY: a live command buffer; the pending list holds a reference until it completes.
    let q = unsafe {
        *(*this).v.status.lock().unwrap() = STATUS_COMMITTED;
        send0(this.cast(), "retain");
        (*this).v.queue
    };
    let mut s = dev.v.state.lock().unwrap();
    let table = s.handle();
    s.push(
        cmd::CreateArgumentTable {
            handle: table,
            max_buffer_bind_count: BINDINGS,
        },
        &[],
    );
    let mut at = 0u64;
    let mut open = false;
    for op in ops {
        if let Op::Event(event, wait, value) = op {
            trace!(
                "cb {:p}: {} event {event} value {value} on queue {}",
                this,
                if wait { "wait for" } else { "signal" },
                q.handle
            );
            // The wire takes queue events only between commits: what came before goes in its own
            // commit, and the rest of this command buffer in the next.
            if open {
                s.push(cmd::EndCommandBuffer {}, &[]);
                s.commit(q);
                open = false;
            }
            s.push(
                cmd::QueueEvent {
                    event,
                    wait: wait as u32,
                    value,
                    queue: q.handle,
                    ..Default::default()
                },
                &[],
            );
            continue;
        }
        if !open {
            s.push(cmd::BeginCommandBuffer {}, &[]);
            open = true;
        }
        encode(&mut s, table, scratch.as_ref(), &mut at, op);
    }
    // The last commit, which the command buffer completes with, follows its last event.
    if !open {
        s.push(cmd::BeginCommandBuffer {}, &[]);
    }
    s.push(cmd::EndCommandBuffer {}, &[]);
    let seqno = s.commit(q);
    trace!(
        "cb {:p}: committed as seqno {seqno} on queue {}",
        this, q.handle
    );
    // The host holds a destroyed object until the commits made before the destroy complete.
    s.push(
        cmd::Destroy {
            handle: table,
            _pad: 0,
        },
        &[],
    );
    // SAFETY: the pending list's reference keeps the command buffer alive.
    unsafe { *(*this).v.scratch.lock().unwrap() = scratch };
    s.pending.push((seqno, q, Ptr(this)));
}

/// Puts one op other than an event on the ring, inside a begun command buffer.
fn encode(s: &mut State, table: u32, scratch: Option<&Region>, at: &mut u64, op: Op) {
    match op {
        Op::BeginCompute => {
            s.push(cmd::BeginCompute {}, &[]);
            // Metal 3 runs one encoder's work after the last's; Metal 4 needs a barrier for that.
            s.push(
                cmd::Barrier {
                    kind: cmd::barrier_kind::AFTER_QUEUE_STAGES,
                    after: STAGE_DISPATCH | STAGE_BLIT,
                    before: STAGE_DISPATCH | STAGE_BLIT,
                    visibility: VISIBILITY_DEVICE,
                },
                &[],
            );
            s.push(cmd::SetArgumentTable { table, stages: 0 }, &[]);
        }
        Op::Pipeline(handle) => s.push(cmd::SetComputePipeline { handle, _pad: 0 }, &[]),
        Op::Bind(index, address) => bind(s, table, index, address),
        Op::Bytes(index, data) => {
            let r = scratch.unwrap();
            // SAFETY: inside the region, which was sized for all of them.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), r.ptr.add(*at as usize), data.len())
            };
            let address = r.gpu + *at;
            *at += (data.len() as u64).next_multiple_of(BYTES_ALIGN);
            bind(s, table, index, address);
        }
        Op::Threads(grid, group) => s.push(
            cmd::DispatchThreads {
                grid: grid.wire(),
                threads_per_group: group.wire(),
            },
            &[],
        ),
        Op::Groups(groups, group) => s.push(
            cmd::DispatchThreadgroups {
                groups: groups.wire(),
                threads_per_group: group.wire(),
            },
            &[],
        ),
        Op::Barrier => s.push(
            cmd::Barrier {
                kind: cmd::barrier_kind::AFTER_ENCODER_STAGES,
                after: STAGE_DISPATCH,
                before: STAGE_DISPATCH,
                visibility: VISIBILITY_DEVICE,
            },
            &[],
        ),
        Op::End => s.push(cmd::EndEncoding {}, &[]),
        Op::Event(..) => unreachable!("events are put on the ring by commit"),
    }
}

fn bind(s: &mut State, table: u32, index: u32, address: u64) {
    let b = cmd::AddressBinding {
        index,
        _pad: 0,
        address,
    };
    s.push(cmd::SetAddresses { table, count: 1 }, &[cmd::bytes_of(&b)]);
}

unsafe extern "C" fn wait_until_completed(this: *const Obj<CommandBuffer>, _: Sel) {
    let dev = device();
    // SAFETY: a live command buffer.
    let c = unsafe { &(*this).v };
    let mut s = dev.v.state.lock().unwrap();
    loop {
        if *c.retired.lock().unwrap() || *c.status.lock().unwrap() < STATUS_COMMITTED {
            return;
        }
        s = dev.v.done.wait(s).unwrap();
    }
}

unsafe extern "C" fn cb_status(this: *const Obj<CommandBuffer>, _: Sel) -> usize {
    // SAFETY: a live command buffer.
    *unsafe { &(*this).v }.status.lock().unwrap()
}

unsafe extern "C" fn cb_error(_: *const Obj<CommandBuffer>, _: Sel) -> *mut Obj<Error> {
    std::ptr::null_mut()
}

pub fn defs() -> Vec<Def> {
    vec![
        Def {
            name: "MTLDevice",
            superclass: Some("NSObject"),
            methods: vec![
                ("name", imp!(device_name)),
                ("maxThreadsPerThreadgroup", imp!(device_max_threads)),
                ("newBufferWithLength:options:", imp!(new_buffer)),
                (
                    "newBufferWithBytes:length:options:",
                    imp!(new_buffer_with_bytes),
                ),
                (
                    "newBufferWithBytesNoCopy:length:options:deallocator:",
                    imp!(new_buffer_no_copy),
                ),
                ("newLibraryWithURL:error:", imp!(new_library_with_url)),
                (
                    "newLibraryWithSource:options:error:",
                    imp!(new_library_with_source),
                ),
                (
                    "newComputePipelineStateWithFunction:error:",
                    imp!(new_compute_pipeline),
                ),
                ("newCommandQueue", imp!(new_command_queue)),
                ("newLibraryWithFile:error:", imp!(new_library_with_file)),
                ("architecture", imp!(device_architecture)),
                ("supportsFamily:", imp!(device_supports_family)),
                ("maxBufferLength", imp!(device_max_buffer_length)),
                ("recommendedMaxWorkingSetSize", imp!(device_working_set)),
                (
                    "maxThreadgroupMemoryLength",
                    imp!(device_max_threadgroup_memory),
                ),
                ("registryID", imp!(device_registry_id)),
                ("hasUnifiedMemory", imp!(device_true)),
                ("newHeapWithDescriptor:", imp!(device_new_heap)),
                ("newFence", imp!(device_new_fence)),
                ("newSharedEvent", imp!(device_new_shared_event)),
                (
                    "newResidencySetWithDescriptor:error:",
                    imp!(device_new_residency_set),
                ),
                ("dealloc", imp!(device_dealloc)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLArchitecture",
            superclass: Some("NSObject"),
            methods: vec![
                ("name", imp!(architecture_name)),
                ("dealloc", imp!(dealloc::<Architecture>)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLHeapDescriptor",
            superclass: Some("NSObject"),
            methods: vec![
                ("setSize:", imp!(heap_descriptor_set)),
                ("setResourceOptions:", imp!(heap_descriptor_set)),
                ("setStorageMode:", imp!(heap_descriptor_set)),
                ("setHazardTrackingMode:", imp!(heap_descriptor_set)),
                ("dealloc", imp!(dealloc::<HeapDescriptor>)),
            ],
            class_methods: vec![("alloc", imp!(alloc::<HeapDescriptor>))],
        },
        Def {
            name: "MTLFence",
            superclass: Some("NSObject"),
            methods: vec![("dealloc", imp!(dealloc::<Fence>))],
            class_methods: vec![],
        },
        Def {
            name: "MTLSharedEvent",
            superclass: Some("NSObject"),
            methods: vec![
                ("signaledValue", imp!(shared_event_value)),
                ("setSignaledValue:", imp!(shared_event_set_value)),
                ("waitUntilSignaledValue:timeoutMS:", imp!(shared_event_wait)),
                ("dealloc", imp!(shared_event_dealloc)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLResidencySetDescriptor",
            superclass: Some("NSObject"),
            methods: vec![
                ("setInitialCapacity:", imp!(residency_nop1)),
                ("dealloc", imp!(dealloc::<ResidencySet>)),
            ],
            class_methods: vec![("alloc", imp!(alloc::<ResidencySet>))],
        },
        Def {
            name: "MTLResidencySet",
            superclass: Some("NSObject"),
            methods: vec![
                ("requestResidency", imp!(residency_nop)),
                ("endResidency", imp!(residency_nop)),
                ("commit", imp!(residency_nop)),
                ("removeAllAllocations", imp!(residency_nop)),
                ("addAllocation:", imp!(residency_nop1)),
                ("removeAllocation:", imp!(residency_nop1)),
                ("addAllocations:count:", imp!(residency_nop2)),
                ("removeAllocations:count:", imp!(residency_nop2)),
                ("allocatedSize", imp!(residency_size)),
                ("dealloc", imp!(dealloc::<ResidencySet>)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLBuffer",
            superclass: Some("NSObject"),
            methods: vec![
                ("contents", imp!(buffer_contents)),
                ("length", imp!(buffer_length)),
                ("gpuAddress", imp!(buffer_gpu_address)),
                ("heap", imp!(buffer_heap)),
                ("allocatedSize", imp!(buffer_allocated_size)),
                ("storageMode", imp!(buffer_storage_mode)),
                ("dealloc", imp!(dealloc::<Buffer>)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLCompileOptions",
            superclass: Some("NSObject"),
            methods: vec![
                ("init", imp!(options_init)),
                ("setFastMathEnabled:", imp!(set_fast_math)),
                ("setLanguageVersion:", imp!(set_language_version)),
                ("setMathMode:", imp!(set_math_mode)),
                ("languageVersion", imp!(language_version)),
                ("setEnableLogging:", imp!(set_enable_logging)),
                ("dealloc", imp!(dealloc::<CompileOptions>)),
            ],
            class_methods: vec![("alloc", imp!(alloc::<CompileOptions>))],
        },
        Def {
            name: "MTLLibrary",
            superclass: Some("NSObject"),
            methods: vec![
                ("newFunctionWithName:", imp!(new_function_with_name)),
                (
                    "newFunctionWithDescriptor:error:",
                    imp!(new_function_with_descriptor),
                ),
                ("dealloc", imp!(dealloc::<Library>)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLFunction",
            superclass: Some("NSObject"),
            methods: vec![
                ("name", imp!(function_name)),
                ("dealloc", imp!(dealloc::<Function>)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLFunctionConstantValues",
            superclass: Some("NSObject"),
            methods: vec![
                ("setConstantValue:type:atIndex:", imp!(set_constant_value)),
                ("dealloc", imp!(dealloc::<ConstantValues>)),
            ],
            class_methods: vec![("alloc", imp!(alloc::<ConstantValues>))],
        },
        Def {
            name: "MTLFunctionDescriptor",
            superclass: Some("NSObject"),
            methods: vec![
                ("setName:", imp!(fd_set_name)),
                ("setSpecializedName:", imp!(fd_set_specialized_name)),
                ("setConstantValues:", imp!(fd_set_constant_values)),
                ("dealloc", imp!(dealloc::<FunctionDescriptor>)),
            ],
            class_methods: vec![
                ("alloc", imp!(alloc::<FunctionDescriptor>)),
                ("functionDescriptor", imp!(function_descriptor)),
            ],
        },
        Def {
            name: "MTLComputePipelineState",
            superclass: Some("NSObject"),
            methods: vec![
                ("maxTotalThreadsPerThreadgroup", imp!(pipeline_max_threads)),
                ("threadExecutionWidth", imp!(pipeline_execution_width)),
                ("dealloc", imp!(dealloc::<Pipeline>)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLCommandQueue",
            superclass: Some("NSObject"),
            methods: vec![
                ("commandBuffer", imp!(command_buffer)),
                (
                    "commandBufferWithUnretainedReferences",
                    imp!(command_buffer),
                ),
                ("addResidencySet:", imp!(residency_nop1)),
                ("removeResidencySet:", imp!(residency_nop1)),
                ("addResidencySets:count:", imp!(residency_nop2)),
                ("removeResidencySets:count:", imp!(residency_nop2)),
                ("dealloc", imp!(dealloc::<Queue>)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLCommandBuffer",
            superclass: Some("NSObject"),
            methods: vec![
                ("computeCommandEncoder", imp!(compute_encoder)),
                (
                    "computeCommandEncoderWithDispatchType:",
                    imp!(compute_encoder_with_type),
                ),
                ("addCompletedHandler:", imp!(add_completed_handler)),
                ("encodeWaitForEvent:value:", imp!(encode_wait)),
                ("encodeSignalEvent:value:", imp!(encode_signal)),
                ("commit", imp!(commit)),
                ("waitUntilCompleted", imp!(wait_until_completed)),
                ("status", imp!(cb_status)),
                ("error", imp!(cb_error)),
                ("dealloc", imp!(dealloc::<CommandBuffer>)),
            ],
            class_methods: vec![],
        },
        Def {
            name: "MTLComputeCommandEncoder",
            superclass: Some("NSObject"),
            methods: vec![
                ("setComputePipelineState:", imp!(set_pipeline)),
                ("setBuffer:offset:atIndex:", imp!(set_buffer)),
                ("setBytes:length:atIndex:", imp!(set_bytes)),
                (
                    "dispatchThreads:threadsPerThreadgroup:",
                    imp!(dispatch_threads),
                ),
                (
                    "dispatchThreadgroups:threadsPerThreadgroup:",
                    imp!(dispatch_threadgroups),
                ),
                ("memoryBarrierWithScope:", imp!(memory_barrier)),
                ("waitForFence:", imp!(encoder_fence)),
                ("updateFence:", imp!(encoder_fence)),
                ("endEncoding", imp!(end_encoding)),
                ("dealloc", imp!(dealloc::<Encoder>)),
            ],
            class_methods: vec![],
        },
    ]
}
