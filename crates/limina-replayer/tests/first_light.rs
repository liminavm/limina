//! The replayer end to end, with this test process playing the guest: it maps arenas the way the
//! worker does, writes commands into a ring in arena memory, rings the doorbell when the consumer
//! has parked, and reads results back from arena memory. No VM is involved.

use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use limina_gpu_wire::cmd::{self, BufRef, REPLY_DONE};
use limina_gpu_wire::ring::{self, Producer};
use limina_replayer::client::{Doorbell, MappedArena, Replayer};

const RING_DATA: u32 = 1 << 18;
const MIB: u64 = 1 << 20;

const RGBA8_UNORM: u32 = 70;
const TEX_2D: u32 = 2;
const USAGE_SHADER_READ: u32 = 1;
const USAGE_RENDER_TARGET: u32 = 4;
const LOAD_CLEAR: u32 = 2;
const STORE_STORE: u32 = 1;
const PRIM_TRIANGLE: u32 = 3;
const RENDER_VERTEX: u32 = 1;
const STAGE_VERTEX: u32 = 1 << 0;
const STAGE_FRAGMENT: u32 = 1 << 1;
const STAGE_DISPATCH: u32 = 1 << 26;
const STAGE_BLIT: u32 = 1 << 27;
const VISIBILITY_DEVICE: u32 = 1;

/// Where the tests keep replies, feedback and other host-written records.
const RECORDS: u64 = 8 * MIB;

const MSL: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VOut { float4 pos [[position]]; };

// The vertex positions arrive as a raw GPU address in an argument table, as KosmicKrisp binds
// everything.
vertex VOut vs(uint vid [[vertex_id]], const device float4* pos [[buffer(0)]]) {
    VOut o;
    o.pos = pos[vid];
    return o;
}

fragment float4 fs(VOut in [[stage_in]]) {
    return float4(0.0, 1.0, 0.0, 1.0);
}

// The destination arrives as a raw GPU address the guest computed (arena base + offset), read from
// arena memory.
struct Ptrs { device uint* dst; };

kernel void poke(constant Ptrs& p [[buffer(0)]], uint tid [[thread_position_in_grid]]) {
    p.dst[tid] = 0xC0FFEE00u + tid;
}
"#;

struct Guest {
    replayer: Replayer,
    bell: Doorbell,
    ring: Producer,
    arena: MappedArena,
    seqno: u64,
}

impl Guest {
    fn new(replayer: Replayer) -> Self {
        let bell = replayer.create_context(1).expect("context");
        let arena = replayer.create_arena(1, 1, 64 * MIB).expect("arena");
        // SAFETY: the arena mapping outlives the producer (both live in `Guest`); offset 0 is
        // page-aligned and the region is far smaller than the arena.
        let ring =
            unsafe { Producer::init(NonNull::new(arena.mapping.as_ptr()).unwrap(), RING_DATA) };
        replayer.attach_ring(1, 1, 0, RING_DATA).expect("attach");
        Self {
            replayer,
            bell,
            ring,
            arena,
            seqno: 0,
        }
    }

    fn push<T: cmd::Command>(&mut self, c: T, extra: &[&[u8]]) {
        self.ring.push(&c, extra).expect("ring space");
    }

    fn kick(&mut self) {
        if self.ring.publish() {
            self.bell.ring();
        }
    }

    /// Commits with `feedback`, kicks if the consumer parked, and waits for the GPU to finish.
    fn commit_with(&mut self, feedback: BufRef) -> Result<(), (u32, u64)> {
        self.seqno += 1;
        self.push(
            cmd::Commit {
                seqno: self.seqno,
                feedback,
            },
            &[],
        );
        self.kick();
        let start = Instant::now();
        while self.ring.completed() < self.seqno {
            if let Some(e) = self.ring.error() {
                return Err(e);
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "GPU did not finish"
            );
            std::thread::sleep(Duration::from_micros(100));
        }
        self.ring.error().map_or(Ok(()), Err)
    }

    fn commit_and_wait(&mut self) -> Result<(), (u32, u64)> {
        self.commit_with(BufRef::NONE)
    }

    fn ptr(&self, offset: u64, len: usize) -> *mut u8 {
        assert!(offset + len as u64 <= self.arena.mapping.size());
        // SAFETY: inside the mapping, checked above.
        unsafe { self.arena.mapping.as_ptr().add(offset as usize) }
    }

    fn bytes(&self, offset: u64, len: usize) -> &[u8] {
        // SAFETY: inside the mapping; the writer has finished.
        unsafe { std::slice::from_raw_parts(self.ptr(offset, len), len) }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        // SAFETY: inside the mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr(offset, data.len()), data.len())
        }
    }

    /// Clears the reply at `offset` and returns its reference.
    fn reply_slot(&mut self, offset: u64) -> BufRef {
        self.write(offset, &[0; size_of::<cmd::Reply>()]);
        BufRef::new(1, offset)
    }

    /// Waits for the reply at `offset`, kicking the ring first.
    fn reply(&mut self, offset: u64) -> cmd::Reply {
        self.kick();
        let p = self
            .ptr(offset, size_of::<cmd::Reply>())
            .cast::<cmd::Reply>();
        // SAFETY: an aligned u32 in the mapping, written by the host.
        let done = unsafe { &*(&raw const (*p).done).cast::<AtomicU32>() };
        let start = Instant::now();
        while done.load(Ordering::Acquire) != REPLY_DONE {
            assert!(
                self.ring.error().is_none(),
                "context lost: {:?}",
                self.ring.error()
            );
            assert!(start.elapsed() < Duration::from_secs(10), "no reply");
            std::thread::sleep(Duration::from_micros(50));
        }
        // SAFETY: the host wrote it before `done`.
        unsafe { p.read() }
    }
}

fn start() -> Guest {
    let exe = env!("CARGO_BIN_EXE_limina-replayer");
    Guest::new(Replayer::spawn(Path::new(exe)).expect("spawn replayer"))
}

fn create_library(g: &mut Guest, handle: u32, src: &str, reply: BufRef) {
    g.push(
        cmd::CreateLibrary {
            handle,
            source_len: src.len() as u32,
            reply,
            ..Default::default()
        },
        &[src.as_bytes()],
    );
}

fn barrier(g: &mut Guest, after: u32, before: u32) {
    g.push(
        cmd::Barrier {
            kind: cmd::barrier_kind::AFTER_QUEUE_STAGES,
            after,
            before,
            visibility: VISIBILITY_DEVICE,
        },
        &[],
    );
}

fn table(g: &mut Guest, handle: u32, bindings: &[(u32, u64)]) {
    g.push(
        cmd::CreateArgumentTable {
            handle,
            max_buffer_bind_count: 4,
        },
        &[],
    );
    let b: Vec<cmd::AddressBinding> = bindings
        .iter()
        .map(|&(index, address)| cmd::AddressBinding {
            index,
            _pad: 0,
            address,
        })
        .collect();
    g.push(
        cmd::SetAddresses {
            table: handle,
            count: b.len() as u32,
        },
        &[cmd::bytes_of_slice(&b)],
    );
}

#[test]
fn a_triangle_drawn_into_an_arena_texture_reads_back() {
    let mut g = start();
    const VB: u64 = MIB;
    const READBACK: u64 = 2 * MIB;
    const TEX: u64 = 4 * MIB;
    const W: u32 = 64;

    let verts: [[f32; 4]; 3] = [
        [0.0, 0.8, 0.0, 1.0],
        [-0.8, -0.8, 0.0, 1.0],
        [0.8, -0.8, 0.0, 1.0],
    ];
    let vb: Vec<u8> = verts
        .iter()
        .flatten()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    g.write(VB, &vb);

    create_library(&mut g, 1, MSL, BufRef::NONE);
    let (vs, fs) = (b"vs", b"fs");
    let mut colors = [0u32; cmd::MAX_COLOR_ATTACHMENTS];
    colors[0] = RGBA8_UNORM;
    g.push(
        cmd::CreateRenderPipeline {
            handle: 2,
            vertex_library: 1,
            fragment_library: 1,
            vertex_name_len: 2,
            fragment_name_len: 2,
            color_formats: colors,
            raster_sample_count: 1,
            rasterization_enabled: 1,
            ..Default::default()
        },
        &[vs, fs],
    );
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateTexture {
            handle: 3,
            arena: 1,
            offset: TEX,
            desc: cmd::TextureDesc {
                texture_type: TEX_2D,
                pixel_format: RGBA8_UNORM,
                width: W,
                height: W,
                depth: 1,
                mip_levels: 1,
                array_length: 1,
                sample_count: 1,
                usage: USAGE_RENDER_TARGET | USAGE_SHADER_READ,
                ..Default::default()
            },
            reply,
            ..Default::default()
        },
        &[],
    );
    let r = g.reply(RECORDS);
    assert_eq!(r.status, 0);
    assert_ne!(r.values[0], 0, "a texture has a resource ID");
    let base = g.arena.gpu_base;
    table(&mut g, 4, &[(0, base + VB)]);

    g.push(cmd::BeginCommandBuffer {}, &[]);
    let mut pass = cmd::BeginRenderPass::default();
    pass.colors[0] = cmd::RenderAttachment {
        texture: 3,
        load_action: LOAD_CLEAR,
        store_action: STORE_STORE,
        clear: [1.0, 0.0, 0.0, 1.0],
        ..Default::default()
    };
    g.push(pass, &[]);
    g.push(cmd::SetRenderPipeline { handle: 2, _pad: 0 }, &[]);
    g.push(
        cmd::SetArgumentTable {
            table: 4,
            stages: RENDER_VERTEX,
        },
        &[],
    );
    g.push(
        cmd::SetViewports { count: 1, _pad: 0 },
        &[cmd::bytes_of(&cmd::Viewport {
            x: 0.0,
            y: 0.0,
            width: W as f64,
            height: W as f64,
            znear: 0.0,
            zfar: 1.0,
        })],
    );
    g.push(
        cmd::Draw {
            primitive: PRIM_TRIANGLE,
            vertex_count: 3,
            instance_count: 1,
            ..Default::default()
        },
        &[],
    );
    g.push(cmd::EndEncoding {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    barrier(&mut g, STAGE_FRAGMENT, STAGE_BLIT);
    g.push(
        cmd::CopyBufferTexture {
            texture: 3,
            size: [W, W, 1],
            buffer: BufRef::new(1, READBACK),
            bytes_per_row: W as u64 * 4,
            bytes_per_image: (W * W) as u64 * 4,
            ..Default::default()
        },
        &[],
    );
    g.push(cmd::EndEncoding {}, &[]);
    g.push(cmd::EndCommandBuffer {}, &[]);
    let fb = BufRef::new(1, RECORDS + 256);
    g.write(fb.offset, &[0; size_of::<cmd::Feedback>()]);
    g.commit_with(fb).expect("frame");

    // SAFETY: written by the host before the seqno was published.
    let f = unsafe {
        g.ptr(fb.offset, size_of::<cmd::Feedback>())
            .cast::<cmd::Feedback>()
            .read()
    };
    assert_eq!((f.done, f.error), (REPLY_DONE, 0));
    assert!(f.gpu_end >= f.gpu_start && f.gpu_start > 0.0, "{f:?}");

    let px = |x: u32, y: u32| {
        let at = READBACK + ((y * W + x) * 4) as u64;
        <[u8; 4]>::try_from(g.bytes(at, 4)).unwrap()
    };
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const RED: [u8; 4] = [255, 0, 0, 255];
    assert_eq!(px(W / 2, W / 2), GREEN, "centre is inside the triangle");
    assert_eq!(px(0, 0), RED, "corner keeps the clear colour");
    assert_eq!(px(W - 1, W - 1), RED);
    assert_eq!(px(W / 2, 2), RED, "above the apex");
    let green = (0..W)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .filter(|&(x, y)| px(x, y) == GREEN)
        .count();
    // Area of the triangle: 0.8 * 1.6 of a 2x2 NDC square, a third of the pixels.
    let expect = (W * W) as f64 * 0.32;
    assert!(
        (green as f64 - expect).abs() < expect * 0.1,
        "{green} green pixels, expected about {expect}"
    );
}

#[test]
fn a_kernel_writes_through_an_address_the_guest_computed() {
    let mut g = start();
    let other = g.replayer.create_arena(1, 2, MIB).expect("second arena");
    const PTRS: u64 = 3 * MIB;
    const DST: u64 = 4096;
    const N: u32 = 256;
    g.write(PTRS, &(other.gpu_base + DST).to_le_bytes());

    create_library(&mut g, 1, MSL, BufRef::NONE);
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateComputePipeline {
            handle: 2,
            library: 1,
            name_len: 4,
            reply,
            ..Default::default()
        },
        &[b"poke"],
    );
    let max = g.reply(RECORDS).values[0];
    assert!(max >= 64, "max threads per group {max}");
    let base = g.arena.gpu_base;
    table(&mut g, 3, &[(0, base + PTRS)]);
    g.push(cmd::BeginCommandBuffer {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    g.push(cmd::SetComputePipeline { handle: 2, _pad: 0 }, &[]);
    g.push(
        cmd::SetArgumentTable {
            table: 3,
            stages: 0,
        },
        &[],
    );
    g.push(
        cmd::DispatchThreadgroups {
            groups: [N / 64, 1, 1],
            threads_per_group: [64, 1, 1],
        },
        &[],
    );
    g.push(cmd::EndEncoding {}, &[]);
    g.push(cmd::EndCommandBuffer {}, &[]);
    g.commit_and_wait().expect("dispatch");

    // SAFETY: inside the second arena's mapping; the GPU has finished.
    let words = unsafe {
        std::slice::from_raw_parts(
            other.mapping.as_ptr().add(DST as usize).cast::<u32>(),
            N as usize,
        )
    };
    for (i, &w) in words.iter().enumerate() {
        assert_eq!(w, 0xC0FFEE00 + i as u32, "word {i}");
    }
}

/// Writes `0xC0FFEE00 + i` into `n` words at GPU address `dst` with the poke kernel.
fn poke(g: &mut Guest, dst: u64, n: u32) {
    const PTRS: u64 = 3 * MIB;
    g.write(PTRS, &dst.to_le_bytes());
    create_library(g, 1, MSL, BufRef::NONE);
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateComputePipeline {
            handle: 2,
            library: 1,
            name_len: 4,
            reply,
            ..Default::default()
        },
        &[b"poke"],
    );
    g.reply(RECORDS);
    let base = g.arena.gpu_base;
    table(g, 3, &[(0, base + PTRS)]);
    g.push(cmd::BeginCommandBuffer {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    g.push(cmd::SetComputePipeline { handle: 2, _pad: 0 }, &[]);
    g.push(
        cmd::SetArgumentTable {
            table: 3,
            stages: 0,
        },
        &[],
    );
    g.push(
        cmd::DispatchThreadgroups {
            groups: [n / 64, 1, 1],
            threads_per_group: [64, 1, 1],
        },
        &[],
    );
    g.push(cmd::EndEncoding {}, &[]);
    g.push(cmd::EndCommandBuffer {}, &[]);
    g.commit_and_wait().expect("dispatch");
}

#[test]
fn an_exported_arena_outlives_its_context_and_imports_elsewhere() {
    let mut g = start();
    g.replayer.create_context(2).expect("second context");
    let theirs = g.replayer.create_arena(2, 1, MIB).expect("their arena");
    g.replayer.export_arena(2, 1, 77).expect("export");
    g.replayer.destroy_context(2).expect("context gone");

    let (size, gpu_base) = g.replayer.import_arena(1, 5, 77).expect("import");
    assert_eq!(size, theirs.mapping.size());
    assert_eq!(
        gpu_base, theirs.gpu_base,
        "an import is the same memory, at the same address"
    );
    const N: u32 = 256;
    poke(&mut g, gpu_base + 4096, N);
    // SAFETY: inside the exporting context's mapping, which the test still holds; the GPU is done.
    let words = unsafe {
        std::slice::from_raw_parts(theirs.mapping.as_ptr().add(4096).cast::<u32>(), N as usize)
    };
    for (i, &w) in words.iter().enumerate() {
        assert_eq!(w, 0xC0FFEE00 + i as u32, "word {i}");
    }

    g.replayer.release_export(77).expect("release");
    assert!(
        g.replayer.import_arena(1, 6, 77).is_err(),
        "a released export is gone"
    );
    assert!(g.replayer.export_arena(1, 99, 78).is_err(), "no such arena");
}

#[test]
fn the_device_answers_queries() {
    let mut g = start();
    const INFO: u64 = RECORDS + 1024;
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::QueryDevice {
            dst: BufRef::new(1, INFO),
            reply,
        },
        &[],
    );
    assert_eq!(g.reply(RECORDS).status, 0);
    // SAFETY: written by the host before the reply.
    let info = unsafe {
        g.ptr(INFO, size_of::<cmd::DeviceInfo>())
            .cast::<cmd::DeviceInfo>()
            .read()
    };
    let name = std::str::from_utf8(&info.name)
        .unwrap()
        .trim_end_matches('\0');
    eprintln!("{name}: {info:?}");
    assert_eq!(info.version, limina_gpu_wire::VERSION);
    assert!(name.starts_with("Apple"), "{name}");
    assert!(info.apple_family >= 7, "Metal 4 needs Apple7 or later");
    assert!(info.sample_counts & (1 << 1 | 1 << 4) == (1 << 1 | 1 << 4));
    assert!(info.heap_buffer_align.is_power_of_two());
    assert!(info.max_threads_per_threadgroup[0] >= 256);
    assert!(info.os_version[0] >= 26, "{:?}", info.os_version);

    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::QueryTextureSize {
            desc: cmd::TextureDesc {
                texture_type: TEX_2D,
                pixel_format: RGBA8_UNORM,
                width: 256,
                height: 256,
                depth: 1,
                mip_levels: 1,
                array_length: 1,
                sample_count: 1,
                usage: USAGE_SHADER_READ,
                ..Default::default()
            },
            reply,
        },
        &[],
    );
    let r = g.reply(RECORDS);
    assert_eq!(r.status, 0);
    assert!(
        r.values[0] >= 256 * 256 * 4 && r.values[1].is_power_of_two(),
        "{r:?}"
    );
}

#[test]
fn a_failed_create_with_a_reply_keeps_the_context() {
    let mut g = start();
    let reply = g.reply_slot(RECORDS);
    create_library(&mut g, 1, "this is not MSL", reply);
    let r = g.reply(RECORDS);
    assert_eq!(r.status, ring::Error::ShaderCompile as u32);
    // The context lives on, and the handle was never taken.
    create_library(&mut g, 1, MSL, BufRef::NONE);
    g.commit_and_wait().expect("still alive");
}

#[test]
fn a_shared_event_signal_reaches_its_mirror() {
    let mut g = start();
    const MIRROR: u64 = RECORDS + 512;
    g.push(
        cmd::SyncObject {
            handle: 1,
            kind: cmd::sync_kind::SHARED_EVENT,
            initial: 3,
            mirror: BufRef::new(1, MIRROR),
        },
        &[],
    );
    g.commit_and_wait().expect("create");
    // SAFETY: an aligned u64 in the mapping, written by the host.
    let mirror = unsafe { &*g.ptr(MIRROR, 8).cast::<AtomicU64>() };
    assert_eq!(mirror.load(Ordering::Acquire), 3);
    g.push(
        cmd::QueueEvent {
            event: 1,
            wait: 0,
            value: 7,
        },
        &[],
    );
    g.kick();
    let start = Instant::now();
    while mirror.load(Ordering::Acquire) < 7 {
        assert!(start.elapsed() < Duration::from_secs(5), "mirror stuck");
        std::thread::sleep(Duration::from_micros(50));
    }
    eprintln!("mirror raised after {:?}", start.elapsed());
    g.push(
        cmd::SetSharedEvent {
            handle: 1,
            _pad: 0,
            value: 9,
        },
        &[],
    );
    g.commit_and_wait().expect("set");
    assert_eq!(mirror.load(Ordering::Acquire), 9);
}

#[test]
fn a_bad_handle_loses_the_context_and_says_where() {
    let mut g = start();
    g.push(
        cmd::Destroy {
            handle: 99,
            _pad: 0,
        },
        &[],
    );
    let at = g.ring.position() - ring::encoded_len::<cmd::Destroy>(0) as u64;
    let err = g.commit_and_wait().expect_err("must fail");
    assert_eq!(err, (ring::Error::BadHandle as u32, at));
}

#[test]
fn many_small_commits_retire_in_order() {
    let mut g = start();
    for _ in 0..2000 {
        g.push(cmd::BeginCommandBuffer {}, &[]);
        g.push(cmd::BeginCompute {}, &[]);
        barrier(
            &mut g,
            STAGE_BLIT | STAGE_DISPATCH | STAGE_VERTEX,
            STAGE_BLIT,
        );
        g.push(
            cmd::FillBuffer {
                buffer: BufRef::new(1, MIB),
                length: 4096,
                value: (g.seqno & 0xff) as u32,
                _pad: 0,
            },
            &[],
        );
        g.push(cmd::EndEncoding {}, &[]);
        g.push(cmd::EndCommandBuffer {}, &[]);
        g.commit_and_wait().expect("commit");
    }
    assert!(g.bytes(MIB, 4096).iter().all(|&b| b == (1999 & 0xff) as u8));
}

/// A context destroyed while its queue waits on events the guest will never signal: the destroy
/// answers at once (the worker's control queue waits on it), and the wedged work is released.
#[test]
fn destroying_a_context_wedged_on_an_event_answers_at_once() {
    let mut g = start();
    for (handle, kind) in [
        (40, cmd::sync_kind::EVENT),
        (41, cmd::sync_kind::SHARED_EVENT),
    ] {
        g.push(
            cmd::SyncObject {
                handle,
                kind,
                initial: 0,
                mirror: BufRef::NONE,
            },
            &[],
        );
        g.push(
            cmd::QueueEvent {
                event: handle,
                wait: 1,
                value: 1,
            },
            &[],
        );
    }
    g.seqno += 1;
    g.push(
        cmd::Commit {
            seqno: g.seqno,
            feedback: BufRef::NONE,
        },
        &[],
    );
    g.kick();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(g.ring.completed(), 0, "the commit is held by the waits");

    let start = Instant::now();
    g.replayer.destroy_context(1).expect("context gone");
    let took = start.elapsed();
    assert!(took < Duration::from_millis(500), "destroy took {took:?}");
    // The waits were released, so the commit ran and retired instead of being abandoned.
    while g.ring.completed() < g.seqno {
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the wedged commit never completed"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    // The replayer still serves other contexts.
    g.replayer.create_context(2).expect("a new context");
}

/// A texture made over an IOSurface the worker filed for the context renders into the surface's
/// own pages: what makes a buffer this transport draws reachable by vrend and venus, which adopt
/// the same surface.
#[test]
#[allow(deprecated)]
fn a_texture_over_a_bound_surface_renders_into_the_surface() {
    use std::ffi::c_void;

    use objc2_core_foundation::{
        CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, kCFTypeDictionaryKeyCallBacks,
        kCFTypeDictionaryValueCallBacks,
    };
    use objc2_io_surface::{
        IOSurfaceCreate, IOSurfaceCreateMachPort, IOSurfaceGetBaseAddress, IOSurfaceLock,
        IOSurfaceLockOptions, IOSurfaceUnlock, kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow,
        kIOSurfaceHeight, kIOSurfacePixelFormat, kIOSurfaceWidth,
    };

    const BGRA8_UNORM: u32 = 80;
    const W: u32 = 64;
    const H: u32 = 8;
    const PITCH: i32 = 256;

    let num = |v: i32| -> CFRetained<CFNumber> {
        // SAFETY: a pointer to an i32 for the SInt32 type.
        unsafe { CFNumber::new(None, CFNumberType::SInt32Type, (&v as *const i32).cast()) }
            .expect("a number")
    };
    let values = [
        num(W as i32),
        num(H as i32),
        num(4),
        num(PITCH),
        num(i32::from_be_bytes(*b"BGRA")),
    ];
    // SAFETY: framework key constants and live CFNumbers; the dictionary retains both.
    let surface = unsafe {
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
        IOSurfaceCreate(&dict).expect("a surface")
    };

    let mut g = start();
    // The right is ours and moves to the replayer.
    let port = IOSurfaceCreateMachPort(&surface);
    g.replayer
        .bind_surface(1, 7, port)
        .expect("the surface is filed");

    let desc = cmd::TextureDesc {
        texture_type: TEX_2D,
        pixel_format: BGRA8_UNORM,
        width: W,
        height: H,
        depth: 1,
        mip_levels: 1,
        array_length: 1,
        sample_count: 1,
        usage: USAGE_RENDER_TARGET | USAGE_SHADER_READ,
        ..Default::default()
    };
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateTextureFromSurface {
            handle: 3,
            surface: 7,
            desc,
            reply,
        },
        &[],
    );
    let r = g.reply(RECORDS);
    assert_eq!(r.status, 0, "a texture over the surface");
    assert_ne!(r.values[0], 0, "with a resource ID");

    // Larger than the surface, and over a surface the context was never given: both refused.
    let reply = g.reply_slot(RECORDS + 256);
    g.push(
        cmd::CreateTextureFromSurface {
            handle: 4,
            surface: 7,
            desc: cmd::TextureDesc {
                width: W + 1,
                ..desc
            },
            reply,
        },
        &[],
    );
    assert_ne!(g.reply(RECORDS + 256).status, 0, "wider than the surface");
    let reply = g.reply_slot(RECORDS + 512);
    g.push(
        cmd::CreateTextureFromSurface {
            handle: 5,
            surface: 8,
            desc,
            reply,
        },
        &[],
    );
    assert_ne!(g.reply(RECORDS + 512).status, 0, "no such surface");

    g.push(cmd::BeginCommandBuffer {}, &[]);
    let mut pass = cmd::BeginRenderPass::default();
    pass.colors[0] = cmd::RenderAttachment {
        texture: 3,
        load_action: LOAD_CLEAR,
        store_action: STORE_STORE,
        clear: [0.0, 1.0, 0.0, 1.0],
        ..Default::default()
    };
    g.push(pass, &[]);
    g.push(cmd::EndEncoding {}, &[]);
    g.push(cmd::EndCommandBuffer {}, &[]);
    g.commit_and_wait().expect("the clear ran");

    // SAFETY: a live surface, locked read-only for the read; rows are PITCH bytes apart.
    let pixels = unsafe {
        IOSurfaceLock(
            &surface,
            IOSurfaceLockOptions::ReadOnly,
            std::ptr::null_mut(),
        );
        let base = IOSurfaceGetBaseAddress(&surface).as_ptr() as *const u8;
        let rows: Vec<Vec<u8>> = (0..H as usize)
            .map(|y| {
                std::slice::from_raw_parts(base.add(y * PITCH as usize), W as usize * 4).to_vec()
            })
            .collect();
        IOSurfaceUnlock(
            &surface,
            IOSurfaceLockOptions::ReadOnly,
            std::ptr::null_mut(),
        );
        rows
    };
    let green = pixels
        .iter()
        .flat_map(|row| row.chunks(4))
        .filter(|p| *p == [0, 255, 0, 255])
        .count();
    assert_eq!(
        green,
        (W * H) as usize,
        "every pixel of the surface is the clear colour"
    );

    g.replayer.release_surface(1, 7).expect("released");
    assert!(g.replayer.release_surface(1, 7).is_err(), "only once");
}

/// Timestamps written around a compute fill and resolved into arena memory read back as two GPU
/// times in order, as KosmicKrisp's timestamp queries read them.
#[test]
fn timestamps_resolve_into_arena_memory_in_order() {
    let mut g = start();
    const OUT: u64 = RECORDS + 1024;
    g.write(OUT, &[0; 16]);
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateCounterHeap {
            handle: 60,
            count: 4,
            reply,
        },
        &[],
    );
    assert_eq!(g.reply(RECORDS).status, 0, "a counter heap of 4 entries");
    g.push(
        cmd::SyncObject {
            handle: 61,
            kind: cmd::sync_kind::FENCE,
            ..Default::default()
        },
        &[],
    );
    let stamp = |g: &mut Guest, index| {
        g.push(
            cmd::WriteTimestamp {
                heap: 60,
                index,
                stage: 0,
                granularity: 0,
            },
            &[],
        )
    };
    g.push(cmd::BeginCommandBuffer {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    stamp(&mut g, 0);
    g.push(
        cmd::FillBuffer {
            buffer: BufRef::new(1, MIB),
            length: 16 * MIB,
            value: 7,
            _pad: 0,
        },
        &[],
    );
    stamp(&mut g, 1);
    // A fence is the only thing that orders a resolve after the stamps: no barrier reaches it.
    g.push(
        cmd::FenceOp {
            fence: 61,
            stages: STAGE_DISPATCH | STAGE_BLIT,
            wait: 0,
            _pad: 0,
        },
        &[],
    );
    g.push(cmd::EndEncoding {}, &[]);
    g.push(
        cmd::ResolveCounterHeap {
            heap: 60,
            first: 0,
            count: 2,
            wait_fence: 61,
            update_fence: 0,
            _pad: 0,
            dst: g.arena.gpu_base + OUT,
        },
        &[],
    );
    g.push(cmd::EndCommandBuffer {}, &[]);
    g.commit_and_wait().expect("commit");
    let t: Vec<u64> = g
        .bytes(OUT, 16)
        .chunks(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    eprintln!("timestamps {t:?}");
    assert!(t[0] != 0 && t[1] >= t[0], "two GPU times in order: {t:?}");
}

/// A timestamp written past the end of its heap loses the context instead of reaching Metal.
#[test]
fn a_timestamp_past_its_heap_loses_the_context() {
    let mut g = start();
    g.push(
        cmd::CreateCounterHeap {
            handle: 60,
            count: 2,
            reply: BufRef::NONE,
        },
        &[],
    );
    g.push(cmd::BeginCommandBuffer {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    g.push(
        cmd::WriteTimestamp {
            heap: 60,
            index: 2,
            stage: 0,
            granularity: 0,
        },
        &[],
    );
    let at = g.ring.position() - ring::encoded_len::<cmd::WriteTimestamp>(0) as u64;
    let err = g.commit_and_wait().expect_err("must fail");
    assert_eq!(err, (ring::Error::BadValue as u32, at));
}
