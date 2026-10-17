//! Metal's compute classes, as much as the spike's test reaches, over limina's Metal-level
//! transport (`limina-gpu-client`). A command buffer records into its own list; commit turns the
//! list into ring commands under the device's lock, and a completion thread runs the handlers.

use std::ffi::{CString, c_void};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use limina_gpu_client::{Arena, Gpu, MIB, find_node};
use limina_gpu_wire::cmd::{self, BufRef};

use crate::blocks::{_Block_copy, _Block_release, Block};
use crate::foundation::{Error, NSString, fail, string, text};
use crate::imp;
use crate::runtime::{Class, Def, Obj, Sel, alloc, dealloc, new, send0};

// The main arena (64 MiB): the ring takes the first 256 KiB.
const REPLY: u64 = MIB;
const INFO: u64 = MIB + 4096;
const SCRATCH: u64 = 2 * MIB;
const SCRATCH_END: u64 = 64 * MIB;

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
    next_arena: u32,
    scratch: u64,
    pending: Vec<(u64, Ptr<Obj<CommandBuffer>>)>,
    lost: Option<String>,
}

pub struct Device {
    state: Mutex<State>,
    done: Condvar,
    name: String,
    max_threads: Size,
}

impl State {
    fn handle(&mut self) -> u32 {
        self.next_handle += 1;
        self.next_handle
    }

    fn arena(&mut self, size: u64) -> std::io::Result<(u32, Arena)> {
        self.next_arena += 1;
        let id = self.next_arena;
        Ok((id, Arena::create(&self.gpu.file, id, size.max(16384))?))
    }

    /// Pushes `c` with `extra` and waits for its reply.
    fn call<T: cmd::Command>(
        &mut self,
        c: impl FnOnce(BufRef) -> T,
        extra: &[&[u8]],
    ) -> Result<cmd::Reply, String> {
        let reply = self.gpu.reply_slot(REPLY);
        self.gpu.push(c(reply), extra);
        let r = self.gpu.reply(REPLY)?;
        match r.status {
            0 => Ok(r),
            s => Err(format!("the GPU host refused it: error {s}")),
        }
    }

    /// `data` copied into the scratch region; its GPU address.
    fn scratch(&mut self, data: &[u8]) -> u64 {
        let len = (data.len() as u64).next_multiple_of(256);
        if self.scratch + len > SCRATCH_END {
            // The spike reuses scratch from the start; the libMetal must not before the GPU is done.
            self.scratch = SCRATCH;
        }
        let at = self.scratch;
        self.gpu.main.write(at, data);
        self.scratch += len;
        self.gpu.main.gpu_base + at
    }
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
        let dev = new(
            "MTLDevice",
            Device {
                state: Mutex::new(State {
                    gpu,
                    next_handle: 0,
                    next_arena: 1,
                    scratch: SCRATCH,
                    pending: Vec::new(),
                    lost: None,
                }),
                done: Condvar::new(),
                name: format!("{name} (limina)"),
                max_threads: Size {
                    width: w as usize,
                    height: h as usize,
                    depth: d as usize,
                },
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
            let done = s.gpu.ring.completed();
            let lost = s.lost.is_some();
            let (finished, still): (Vec<_>, Vec<_>) = s
                .pending
                .drain(..)
                .partition(|(seqno, _)| *seqno <= done || lost);
            s.pending = still;
            finished
        };
        let status = if dev.v.state.lock().unwrap().lost.is_some() {
            STATUS_ERROR
        } else {
            STATUS_COMPLETED
        };
        for (_, cb) in &finished {
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
            *c.retired.lock().unwrap() = true;
        }
        if !finished.is_empty() {
            dev.v.done.notify_all();
            for (_, cb) in finished {
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

// --- Buffers ---

pub struct Buffer {
    arena: Arena,
    length: usize,
}

unsafe extern "C" fn new_buffer(
    this: *const Obj<Device>,
    _: Sel,
    length: usize,
    _options: usize,
) -> *mut Obj<Buffer> {
    // SAFETY: a live device.
    let mut s = unsafe { &(*this).v }.state.lock().unwrap();
    match s.arena(length as u64) {
        Ok((_, arena)) => new("MTLBuffer", Buffer { arena, length }),
        Err(e) => {
            eprintln!("limina-metal: a buffer of {length} bytes: {e}");
            std::ptr::null_mut()
        }
    }
}

unsafe extern "C" fn buffer_contents(this: *const Obj<Buffer>, _: Sel) -> *mut c_void {
    // SAFETY: a live buffer.
    unsafe { (*this).v.arena.ptr.as_ptr().cast() }
}

unsafe extern "C" fn buffer_length(this: *const Obj<Buffer>, _: Sel) -> usize {
    // SAFETY: a live buffer.
    unsafe { (*this).v.length }
}

unsafe extern "C" fn buffer_gpu_address(this: *const Obj<Buffer>, _: Sel) -> u64 {
    // SAFETY: a live buffer.
    unsafe { (*this).v.arena.gpu_base }
}

// --- Libraries and functions ---

pub struct Library {
    handle: u32,
}

#[derive(Default)]
pub struct CompileOptions {
    fast_math: bool,
    language_version: u32,
}

unsafe extern "C" fn options_init(
    this: *mut Obj<CompileOptions>,
    _: Sel,
) -> *mut Obj<CompileOptions> {
    // Metal's default.
    // SAFETY: a live object.
    unsafe { (*this).v.fast_math = true };
    this
}

unsafe extern "C" fn set_fast_math(this: *mut Obj<CompileOptions>, _: Sel, on: bool) {
    // SAFETY: a live object.
    unsafe { (*this).v.fast_math = on };
}

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
    let path = unsafe { &(*url).v.path };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            fail(error, format!("{path}: {e}"));
            return std::ptr::null_mut();
        }
    };
    // SAFETY: a live device.
    let mut s = unsafe { &(*this).v }.state.lock().unwrap();
    let staged = match s.arena(bytes.len() as u64) {
        Ok(a) => a,
        Err(e) => {
            fail(
                error,
                format!("staging {} bytes of {path}: {e}", bytes.len()),
            );
            return std::ptr::null_mut();
        }
    };
    staged.1.write(0, &bytes);
    let handle = s.handle();
    // The host copies the bytes before it replies, so the staging arena can go after.
    match s.call(
        |reply| cmd::CreateLibraryData {
            handle,
            data_len: bytes.len() as u64,
            data: BufRef::new(staged.0, 0),
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
    let (fast, version) = if options.is_null() {
        (true, 0)
    } else {
        // SAFETY: a live CompileOptions.
        let o = unsafe { &(*options).v };
        (o.fast_math, o.language_version)
    };
    // SAFETY: a live device.
    let mut s = unsafe { &(*this).v }.state.lock().unwrap();
    let handle = s.handle();
    match s.call(
        |reply| cmd::CreateLibrary {
            handle,
            source_len: src.len() as u32,
            language_version: version,
            // MTLMathModeFast or MTLMathModeSafe.
            math_mode: if fast { 2 } else { 0 },
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

pub struct Queue;

unsafe extern "C" fn new_command_queue(_: *const Obj<Device>, _: Sel) -> *mut Obj<Queue> {
    new("MTLCommandQueue", Queue)
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
}

pub struct CommandBuffer {
    ops: Mutex<Vec<Op>>,
    handlers: Mutex<Vec<Ptr<c_void>>>,
    status: Mutex<usize>,
    /// Complete, and its handlers have run.
    retired: Mutex<bool>,
}

unsafe extern "C" fn command_buffer(_: *const Obj<Queue>, _: Sel) -> *mut Obj<CommandBuffer> {
    autoreleased(new(
        "MTLCommandBuffer",
        CommandBuffer {
            ops: Mutex::new(Vec::new()),
            handlers: Mutex::new(Vec::new()),
            status: Mutex::new(0),
            retired: Mutex::new(false),
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
        (*this).v.op(Op::Bind(
            index as u32,
            (*b).v.arena.gpu_base + offset as u64,
        ))
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
    let mut s = dev.v.state.lock().unwrap();
    let table = s.handle();
    s.gpu.push(
        cmd::CreateArgumentTable {
            handle: table,
            max_buffer_bind_count: BINDINGS,
        },
        &[],
    );
    s.gpu.push(cmd::BeginCommandBuffer {}, &[]);
    for op in ops {
        match op {
            Op::BeginCompute => {
                s.gpu.push(cmd::BeginCompute {}, &[]);
                // Metal 3 runs one encoder's work after the last's; Metal 4 needs a barrier for that.
                s.gpu.push(
                    cmd::Barrier {
                        kind: cmd::barrier_kind::AFTER_QUEUE_STAGES,
                        after: STAGE_DISPATCH | STAGE_BLIT,
                        before: STAGE_DISPATCH | STAGE_BLIT,
                        visibility: VISIBILITY_DEVICE,
                    },
                    &[],
                );
                s.gpu.push(cmd::SetArgumentTable { table, stages: 0 }, &[]);
            }
            Op::Pipeline(handle) => s.gpu.push(cmd::SetComputePipeline { handle, _pad: 0 }, &[]),
            Op::Bind(index, address) => bind(&mut s, table, index, address),
            Op::Bytes(index, data) => {
                let address = s.scratch(&data);
                bind(&mut s, table, index, address);
            }
            Op::Threads(grid, group) => s.gpu.push(
                cmd::DispatchThreads {
                    grid: grid.wire(),
                    threads_per_group: group.wire(),
                },
                &[],
            ),
            Op::Groups(groups, group) => s.gpu.push(
                cmd::DispatchThreadgroups {
                    groups: groups.wire(),
                    threads_per_group: group.wire(),
                },
                &[],
            ),
            Op::Barrier => s.gpu.push(
                cmd::Barrier {
                    kind: cmd::barrier_kind::AFTER_ENCODER_STAGES,
                    after: STAGE_DISPATCH,
                    before: STAGE_DISPATCH,
                    visibility: VISIBILITY_DEVICE,
                },
                &[],
            ),
            Op::End => s.gpu.push(cmd::EndEncoding {}, &[]),
        }
    }
    s.gpu.push(cmd::EndCommandBuffer {}, &[]);
    // The table is never destroyed: whether the host may destroy one that committed work still
    // names is not settled here.
    let seqno = s.gpu.commit_nowait();
    // SAFETY: a live command buffer; the pending list holds a reference until it completes.
    unsafe {
        *(*this).v.status.lock().unwrap() = STATUS_COMMITTED;
        send0(this.cast(), "retain");
    }
    s.pending.push((seqno, Ptr(this)));
}

fn bind(s: &mut State, table: u32, index: u32, address: u64) {
    let b = cmd::AddressBinding {
        index,
        _pad: 0,
        address,
    };
    s.gpu
        .push(cmd::SetAddresses { table, count: 1 }, &[cmd::bytes_of(&b)]);
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
                ("dealloc", imp!(device_dealloc)),
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
                ("endEncoding", imp!(end_encoding)),
                ("dealloc", imp!(dealloc::<Encoder>)),
            ],
            class_methods: vec![],
        },
    ]
}
