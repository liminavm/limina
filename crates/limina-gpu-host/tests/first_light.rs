//! The GPU host end to end, with this test process playing the guest: it maps arenas the way the
//! worker does, writes commands into a ring in arena memory, rings the doorbell when the consumer
//! has parked, and reads results back from arena memory. No VM is involved.

use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use limina_gpu_host::client::{Doorbell, GpuHost, MappedArena};
use limina_gpu_wire::cmd::{self, BufRef, REPLY_DONE};
use limina_gpu_wire::ring::{self, Producer};

const RING_DATA: u32 = 1 << 18;
const MIB: u64 = 1 << 20;

const RGBA8_UNORM: u32 = 70;
const BGRA8_UNORM: u32 = 80;
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
    host: GpuHost,
    bell: Doorbell,
    ring: Producer,
    arena: MappedArena,
    seqno: u64,
}

impl Guest {
    fn new(host: GpuHost) -> Self {
        let bell = host.create_context(1).expect("context");
        let arena = host.create_arena(1, 1, 64 * MIB).expect("arena");
        // SAFETY: the arena mapping outlives the producer (both live in `Guest`); offset 0 is
        // page-aligned and the region is far smaller than the arena.
        let ring =
            unsafe { Producer::init(NonNull::new(arena.mapping.as_ptr()).unwrap(), RING_DATA) };
        host.attach_ring(1, 1, 0, RING_DATA).expect("attach");
        Self {
            host,
            bell,
            ring,
            arena,
            seqno: 0,
        }
    }

    fn push<T: cmd::Command>(&mut self, c: T, extra: &[&[u8]]) {
        self.ring.push(&c, extra).expect("ring space");
    }

    /// Pushes, kicking the consumer and waiting for it while the ring is full.
    fn push_waiting<T: cmd::Command>(&mut self, c: T) {
        let start = Instant::now();
        while let Err(e) = self.ring.push(&c, &[]) {
            assert!(matches!(e, ring::PushError::Full), "{e:?}");
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the consumer stopped"
            );
            self.kick();
            std::thread::sleep(Duration::from_micros(50));
        }
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
                ..Default::default()
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
    let exe = env!("CARGO_BIN_EXE_limina-gpu-host");
    Guest::new(GpuHost::spawn(Path::new(exe)).expect("spawn GPU host"))
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
    let other = g.host.create_arena(1, 2, MIB).expect("second arena");
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

/// A Vulkan texel buffer as KosmicKrisp makes one: a `TextureBuffer` texture over arena memory,
/// as wide as the view has elements (past a 2D texture's 16384) and with the view's range as its
/// `bytesPerRow`, which nothing aligns. WebRender samples one on every frame.
#[test]
fn a_texel_buffer_over_arena_memory_reads_what_the_guest_wrote() {
    const TB_MSL: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct Args { texture_buffer<float> tb; device float* dst; };
kernel void readtb(constant Args& a [[buffer(0)]], uint tid [[thread_position_in_grid]]) {
    a.dst[tid] = a.tb.read(min(tid * 105u, 20000u)).r;
}
"#;
    const R32_FLOAT: u32 = 55;
    const TEX_BUFFER: u32 = 9;
    const TEXELS: u32 = 20001;
    const DATA: u64 = 4 * MIB;
    const ARGS: u64 = 3 * MIB;
    const DST: u64 = 5 * MIB;
    const N: u32 = 192;
    let mut g = start();
    let texels: Vec<u8> = (0..TEXELS).flat_map(|i| (i as f32).to_le_bytes()).collect();
    g.write(DATA, &texels);

    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateTexture {
            handle: 1,
            arena: 1,
            offset: DATA,
            bytes_per_row: TEXELS as u64 * 4,
            desc: cmd::TextureDesc {
                texture_type: TEX_BUFFER,
                pixel_format: R32_FLOAT,
                width: TEXELS,
                height: 1,
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
    assert_eq!(r.status, 0, "the host refused the texel buffer");
    let base = g.arena.gpu_base;
    g.write(ARGS, &r.values[0].to_le_bytes());
    g.write(ARGS + 8, &(base + DST).to_le_bytes());

    create_library(&mut g, 2, TB_MSL, BufRef::NONE);
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateComputePipeline {
            handle: 3,
            library: 2,
            name_len: 6,
            reply,
            ..Default::default()
        },
        &[b"readtb"],
    );
    g.reply(RECORDS);
    table(&mut g, 4, &[(0, base + ARGS)]);
    g.push(cmd::BeginCommandBuffer {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    g.push(cmd::SetComputePipeline { handle: 3, _pad: 0 }, &[]);
    g.push(
        cmd::SetArgumentTable {
            table: 4,
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

    for i in 0..N {
        let at = DST + i as u64 * 4;
        let got = f32::from_le_bytes(g.bytes(at, 4).try_into().unwrap());
        // Up to the last texel, 20000, past where a 2D texture could reach.
        let texel = (i * 105).min(TEXELS - 1);
        assert_eq!(got, texel as f32, "texel {texel}");
    }
}

/// Writes `0xC0FFEE00 + i` into `n` words at GPU address `dst` with the poke kernel.
fn poke(g: &mut Guest, dst: u64, n: u32) {
    create_library(g, 1, MSL, BufRef::NONE);
    run_poke(g, dst, n);
}

/// Library 1's `poke` writing `n` words at GPU address `dst`.
fn run_poke(g: &mut Guest, dst: u64, n: u32) {
    const PTRS: u64 = 3 * MIB;
    g.write(PTRS, &dst.to_le_bytes());
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
    g.host.create_context(2).expect("second context");
    let theirs = g.host.create_arena(2, 1, MIB).expect("their arena");
    g.host.export_arena(2, 1, 77).expect("export");
    g.host.destroy_context(2).expect("context gone");

    let (size, gpu_base) = g.host.import_arena(1, 5, 77).expect("import");
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

    g.host.release_export(77).expect("release");
    assert!(
        g.host.import_arena(1, 6, 77).is_err(),
        "a released export is gone"
    );
    assert!(g.host.export_arena(1, 99, 78).is_err(), "no such arena");
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
    assert_ne!(info.features & cmd::feature::LIBRARY_DATA, 0);
    assert_ne!(info.features & cmd::feature::FUNCTION_CONSTANTS, 0);

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

/// Metal asserts on a pipeline descriptor whose function it cannot find, which would end the GPU
/// host and every context of the VM; the host must answer it as a failed create instead.
#[test]
fn a_pipeline_naming_a_function_the_library_lacks_fails_its_reply() {
    let mut g = start();
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
        &[b"nope"],
    );
    assert_eq!(
        g.reply(RECORDS).status,
        ring::Error::PipelineCreate as u32,
        "compute"
    );
    let mut colors = [0u32; cmd::MAX_COLOR_ATTACHMENTS];
    colors[0] = RGBA8_UNORM;
    for (vs, fs) in [(b"no", b"fs"), (b"vs", b"no")] {
        let reply = g.reply_slot(RECORDS);
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
                reply,
                ..Default::default()
            },
            &[vs, fs],
        );
        assert_eq!(
            g.reply(RECORDS).status,
            ring::Error::PipelineCreate as u32,
            "render {}, {}",
            std::str::from_utf8(vs).unwrap(),
            std::str::from_utf8(fs).unwrap()
        );
    }
    let base = g.arena.gpu_base;
    run_poke(&mut g, base + 32 * MIB, 64);
}

/// `src` compiled to a `.metallib` by Xcode's Metal toolchain
/// (`xcodebuild -downloadComponent MetalToolchain` installs it).
fn metallib(name: &str, src: &str) -> Vec<u8> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let metal = dir.join(format!("{name}.metal"));
    let lib = dir.join(format!("{name}.metallib"));
    std::fs::write(&metal, src).expect("write MSL");
    let out = std::process::Command::new("xcrun")
        .args(["-sdk", "macosx", "metal", "-o"])
        .arg(&lib)
        .arg(&metal)
        .output()
        .expect("xcrun");
    assert!(
        out.status.success(),
        "metal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read(lib).expect("read metallib")
}

/// Where the tests below stage a library's bytes in the guest's arena.
const LIB_DATA: u64 = 16 * MIB;

fn create_library_data(g: &mut Guest, handle: u32, data: &[u8], reply: BufRef) {
    g.write(LIB_DATA, data);
    g.push(
        cmd::CreateLibraryData {
            handle,
            data_len: data.len() as u64,
            data: BufRef::new(1, LIB_DATA),
            reply,
            ..Default::default()
        },
        &[],
    );
}

#[test]
fn a_kernel_from_a_metallib_writes_through_an_address_the_guest_computed() {
    let mut g = start();
    let other = g.host.create_arena(1, 2, MIB).expect("second arena");
    let reply = g.reply_slot(RECORDS + 1024);
    create_library_data(&mut g, 1, &metallib("poke", MSL), reply);
    assert_eq!(g.reply(RECORDS + 1024).status, 0);
    run_poke(&mut g, other.gpu_base, 256);
    // SAFETY: inside the second arena's mapping; the GPU has finished.
    let words = unsafe { std::slice::from_raw_parts(other.mapping.as_ptr().cast::<u32>(), 256) };
    for (i, &w) in words.iter().enumerate() {
        assert_eq!(w, 0xC0FFEE00 + i as u32, "word {i}");
    }
}

#[test]
fn a_metallib_that_does_not_load_fails_its_reply_and_keeps_the_context() {
    let mut g = start();
    let reply = g.reply_slot(RECORDS);
    create_library_data(&mut g, 1, b"MTLB not really a library", reply);
    assert_eq!(g.reply(RECORDS).status, ring::Error::ShaderCompile as u32);
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateLibraryData {
            handle: 1,
            data_len: cmd::MAX_LIBRARY_DATA + 1,
            data: BufRef::new(1, 0),
            reply,
            ..Default::default()
        },
        &[],
    );
    assert_eq!(g.reply(RECORDS).status, ring::Error::BadValue as u32);
    create_library_data(&mut g, 1, &metallib("poke", MSL), BufRef::NONE);
    g.commit_and_wait().expect("still alive");
}

/// The library's record holds its bytes: a restore needs nothing from the arena it came from.
#[test]
fn a_metallib_library_restores_without_its_staging() {
    const DST: u64 = 32 * MIB;
    let mut g = start();
    create_library_data(&mut g, 1, &metallib("poke", MSL), BufRef::NONE);
    g.commit_and_wait().expect("library");
    g.write(LIB_DATA, &[0; 4096]);
    let mut g = restore_into_fresh(g);
    let base = g.arena.gpu_base;
    run_poke(&mut g, base + DST, 64);
    let words = g.bytes(DST, 64 * 4);
    assert_eq!(words[63 * 4..], (0xC0FFEE00u32 + 63).to_le_bytes());
}

const SPEC_MSL: &str = r#"
#include <metal_stdlib>
using namespace metal;

constant bool NEGATE [[function_constant(0)]];
constant int K [[function_constant(1)]];
constant ushort STEP [[function_constant(2)]];

kernel void spec(device int* dst [[buffer(0)]], uint tid [[thread_position_in_grid]]) {
    dst[tid] = (NEGATE ? -K : K) + int(tid) * int(STEP);
}
"#;

/// `MTLDataType` values.
const DATA_BOOL: u32 = 53;
const DATA_INT: u32 = 29;
const DATA_USHORT: u32 = 41;

fn constants(negate: bool, k: i32, step: u16) -> [cmd::FunctionConstant; 3] {
    [
        (0, DATA_BOOL, negate as u64),
        (1, DATA_INT, k as u32 as u64),
        (2, DATA_USHORT, step as u64),
    ]
    .map(|(index, data_type, value)| cmd::FunctionConstant {
        index,
        data_type,
        value,
    })
}

/// Pipeline `handle` for `spec` from `library`, specialised with `consts`.
fn spec_pipeline(
    g: &mut Guest,
    handle: u32,
    library: u32,
    consts: &[cmd::FunctionConstant],
    reply: BufRef,
) {
    g.push(
        cmd::CreateComputePipeline {
            handle,
            library,
            name_len: 4,
            constant_count: consts.len() as u32,
            reply,
            ..Default::default()
        },
        &[cmd::bytes_of_slice(consts), b"spec"],
    );
}

/// `spec` from pipeline `pipeline` over `n` ints at arena offset `dst`, bound through table
/// `table`.
fn run_spec(g: &mut Guest, pipeline: u32, table_handle: u32, dst: u64, n: u32) -> Vec<i32> {
    let base = g.arena.gpu_base;
    table(g, table_handle, &[(0, base + dst)]);
    g.push(cmd::BeginCommandBuffer {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    g.push(
        cmd::SetComputePipeline {
            handle: pipeline,
            _pad: 0,
        },
        &[],
    );
    g.push(
        cmd::SetArgumentTable {
            table: table_handle,
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
    g.bytes(dst, n as usize * 4)
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&w| i32::from_le_bytes(w))
        .collect()
}

fn spec_want(negate: bool, k: i32, step: u16, n: u32) -> Vec<i32> {
    (0..n as i32)
        .map(|i| (if negate { -k } else { k }) + i * step as i32)
        .collect()
}

/// MLX's kernels take their variants as function constants: one function, a pipeline per set of
/// values, from MSL source and from a `.metallib` alike.
#[test]
fn function_constants_specialise_one_function_into_several_pipelines() {
    let mut g = start();
    let reply = g.reply_slot(RECORDS);
    create_library(&mut g, 1, SPEC_MSL, BufRef::NONE);
    create_library_data(&mut g, 2, &metallib("spec", SPEC_MSL), reply);
    assert_eq!(g.reply(RECORDS).status, 0);
    for (handle, library, (negate, k, step)) in [
        (3, 1, (false, 7, 1)),
        (4, 1, (true, 40, 3)),
        (5, 2, (true, -5, 65535)),
    ] {
        let reply = g.reply_slot(RECORDS);
        spec_pipeline(&mut g, handle, library, &constants(negate, k, step), reply);
        let r = g.reply(RECORDS);
        assert_eq!(r.status, 0, "pipeline {handle}");
        let got = run_spec(&mut g, handle, 10 + handle, 32 * MIB, 128);
        assert_eq!(got, spec_want(negate, k, step, 128), "pipeline {handle}");
    }
}

#[test]
fn a_constant_metal_cannot_take_fails_its_reply_and_keeps_the_context() {
    let mut g = start();
    create_library(&mut g, 1, SPEC_MSL, BufRef::NONE);
    let good = constants(false, 1, 1);
    let with = |i: usize, data_type: u32, value: u64| {
        let mut c = good;
        c[i] = cmd::FunctionConstant {
            data_type,
            value,
            ..c[i]
        };
        c
    };
    let cases: [(&str, Vec<cmd::FunctionConstant>, ring::Error); 6] = [
        (
            "not a scalar type",
            with(1, 999, 1).to_vec(),
            ring::Error::BadValue,
        ),
        (
            "a bool of 2",
            with(0, DATA_BOOL, 2).to_vec(),
            ring::Error::BadValue,
        ),
        (
            "an int with bits above it",
            with(1, DATA_INT, 1 << 32).to_vec(),
            ring::Error::BadValue,
        ),
        (
            "an index past Metal's range",
            vec![cmd::FunctionConstant {
                index: 1 << 16,
                ..good[0]
            }],
            ring::Error::BadValue,
        ),
        (
            "no constants for a function that needs them",
            vec![],
            ring::Error::PipelineCreate,
        ),
        (
            "one constant missing",
            good[..2].to_vec(),
            ring::Error::PipelineCreate,
        ),
    ];
    for (what, consts, want) in cases {
        let reply = g.reply_slot(RECORDS);
        spec_pipeline(&mut g, 2, 1, &consts, reply);
        assert_eq!(g.reply(RECORDS).status, want as u32, "{what}");
    }
    spec_pipeline(&mut g, 2, 1, &good, BufRef::NONE);
    assert_eq!(
        run_spec(&mut g, 2, 3, 32 * MIB, 64),
        spec_want(false, 1, 1, 64)
    );
}

/// The constants travel in the pipeline's record, so a restore rebuilds the same specialisation.
#[test]
fn a_specialised_pipeline_restores() {
    let mut g = start();
    create_library(&mut g, 1, SPEC_MSL, BufRef::NONE);
    spec_pipeline(&mut g, 2, 1, &constants(true, 9, 2), BufRef::NONE);
    g.commit_and_wait().expect("pipeline");
    let mut g = restore_into_fresh(g);
    assert_eq!(
        run_spec(&mut g, 2, 3, 32 * MIB, 64),
        spec_want(true, 9, 2, 64)
    );
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
            ..Default::default()
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

/// Commits, without waiting, a command buffer that pokes `N` words at `dst` through table `table`,
/// whose pointer lives at `ptrs`.
fn commit_poke(g: &mut Guest, table_handle: u32, ptrs: u64, dst: u64) {
    commit_poke_on(g, 0, table_handle, ptrs, dst);
}

/// [`commit_poke`] on `queue`.
fn commit_poke_on(g: &mut Guest, queue: u32, table_handle: u32, ptrs: u64, dst: u64) {
    const N: u32 = 256;
    g.write(ptrs, &(g.arena.gpu_base + dst).to_le_bytes());
    let base = g.arena.gpu_base;
    table(g, table_handle, &[(0, base + ptrs)]);
    g.push(cmd::BeginCommandBuffer {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    barrier(g, STAGE_DISPATCH | STAGE_BLIT, STAGE_DISPATCH | STAGE_BLIT);
    g.push(cmd::SetComputePipeline { handle: 2, _pad: 0 }, &[]);
    g.push(
        cmd::SetArgumentTable {
            table: table_handle,
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
    commit_nowait_on(g, queue);
}

fn commit_nowait(g: &mut Guest) {
    commit_nowait_on(g, 0);
}

fn commit_nowait_on(g: &mut Guest, queue: u32) {
    g.push(cmd::EndCommandBuffer {}, &[]);
    g.seqno += 1;
    g.push(
        cmd::Commit {
            seqno: g.seqno,
            queue,
            feedback: BufRef::NONE,
            ..Default::default()
        },
        &[],
    );
    g.kick();
}

/// The poke library and pipeline 2, and shared event `event` with its mirror at `mirror`.
fn poke_setup(g: &mut Guest) {
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
}

fn shared_event(g: &mut Guest, handle: u32, mirror: u64) {
    g.push(
        cmd::SyncObject {
            handle,
            kind: cmd::sync_kind::SHARED_EVENT,
            initial: 0,
            mirror: BufRef::new(1, mirror),
        },
        &[],
    );
}

/// A queue whose completed value the host stores at `slot` in the ring's arena.
fn create_queue(g: &mut Guest, handle: u32, slot: u64) {
    g.push(
        cmd::CreateQueue {
            handle,
            completed: BufRef::new(1, slot),
            ..Default::default()
        },
        &[],
    );
}

fn slot(g: &Guest, at: u64) -> u64 {
    // SAFETY: an aligned u64 in the mapping, written by the host.
    unsafe { &*g.ptr(at, 8).cast::<AtomicU64>() }.load(Ordering::Acquire)
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(10), "{what}");
        std::thread::sleep(Duration::from_micros(100));
    }
}

/// Two queues complete on their own: queue 0 waits for an event only the CPU sets, and the work
/// committed after it on queue 5 runs and completes meanwhile, its completed value reaching its
/// seqno while queue 0's stays behind.
#[test]
fn a_second_queue_completes_while_the_first_waits() {
    let mut g = start();
    const A: u64 = 4 * MIB;
    const B: u64 = 5 * MIB;
    const MIRROR: u64 = RECORDS + 512;
    const SLOT: u64 = RECORDS + 1024;
    poke_setup(&mut g);
    shared_event(&mut g, 9, MIRROR);
    create_queue(&mut g, 5, SLOT);
    g.commit_and_wait().expect("setup");
    let before = g.seqno;

    g.push(
        cmd::QueueEvent {
            event: 9,
            wait: 1,
            value: 1,
            ..Default::default()
        },
        &[],
    );
    commit_poke_on(&mut g, 0, 3, 3 * MIB, A);
    let a = g.seqno;
    commit_poke_on(&mut g, 5, 4, 3 * MIB + 8, B);
    let b = g.seqno;
    wait_until("queue 5's commit did not complete", || slot(&g, SLOT) >= b);
    assert_eq!(poked(&g, B), 256, "queue 5's work");
    assert_eq!(poked(&g, A), 0, "queue 0's work ran before its wait");
    assert_eq!(
        g.ring.completed(),
        before,
        "queue 0 completed past its wait"
    );

    g.push(
        cmd::SetSharedEvent {
            handle: 9,
            _pad: 0,
            value: 1,
        },
        &[],
    );
    g.kick();
    wait_until("queue 0's commit did not complete", || {
        g.ring.completed() >= a
    });
    assert_eq!(poked(&g, A), 256);
    assert!(g.ring.error().is_none());
}

/// A wait on one queue committed before the signal it waits for on another: as in Metal, the
/// second queue's signal releases the first.
#[test]
fn a_wait_committed_before_its_signal_on_another_queue_completes() {
    let mut g = start();
    const A: u64 = 4 * MIB;
    const B: u64 = 5 * MIB;
    const SLOT: u64 = RECORDS + 1024;
    poke_setup(&mut g);
    shared_event(&mut g, 9, RECORDS + 512);
    create_queue(&mut g, 5, SLOT);
    g.commit_and_wait().expect("setup");

    g.push(
        cmd::QueueEvent {
            event: 9,
            wait: 1,
            value: 1,
            queue: 5,
            ..Default::default()
        },
        &[],
    );
    commit_poke_on(&mut g, 5, 4, 3 * MIB + 8, B);
    let b = g.seqno;
    commit_poke_on(&mut g, 0, 3, 3 * MIB, A);
    g.push(
        cmd::QueueEvent {
            event: 9,
            wait: 0,
            value: 1,
            ..Default::default()
        },
        &[],
    );
    g.commit_and_wait().expect("queue 0");
    wait_until("queue 5's commit did not complete", || slot(&g, SLOT) >= b);
    assert_eq!((poked(&g, A), poked(&g, B)), (256, 256));
}

/// A queue is an object like any other: it survives an export and a restore into a fresh GPU host
/// and runs work there, and a commit to a queue that does not exist loses the context.
#[test]
fn a_queue_is_restored_and_an_unknown_one_loses_the_context() {
    let mut g = start();
    const SLOT: u64 = RECORDS + 1024;
    poke_setup(&mut g);
    create_queue(&mut g, 5, SLOT);
    g.commit_and_wait().expect("setup");
    let mut g = restore_into_fresh(g);
    commit_poke_on(&mut g, 5, 4, 3 * MIB + 8, 5 * MIB);
    let b = g.seqno;
    wait_until("the restored queue's commit did not complete", || {
        slot(&g, SLOT) >= b
    });
    assert_eq!(poked(&g, 5 * MIB), 256);

    g.push(cmd::BeginCommandBuffer {}, &[]);
    commit_nowait_on(&mut g, 77);
    wait_until("no error for an unknown queue", || g.ring.error().is_some());
    assert_eq!(
        g.ring.error().map(|(code, _)| code),
        Some(ring::Error::BadHandle as u32)
    );
}

fn poked(g: &Guest, dst: u64) -> usize {
    (0..256u32)
        .filter(|&i| g.bytes(dst + i as u64 * 4, 4) == (0xC0FFEE00 + i).to_le_bytes())
        .count()
}

/// A queue wait holds the work committed after it, as limina-metal sends it when it splits a
/// command buffer around `encodeWaitForEvent:value:`: an empty commit, the wait, a commit with
/// work, then another command buffer's work and an event signal. Nothing after the wait runs, and
/// the second event is not signaled, until the CPU signals the first.
#[test]
fn work_committed_after_a_queue_wait_runs_only_once_the_event_is_signaled() {
    let mut g = start();
    const A: u64 = 4 * MIB;
    const B: u64 = 5 * MIB;
    const MIRROR: u64 = RECORDS + 512;
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
    g.reply(RECORDS);
    for (handle, at) in [(9, MIRROR), (10, MIRROR + 8)] {
        g.push(
            cmd::SyncObject {
                handle,
                kind: cmd::sync_kind::SHARED_EVENT,
                initial: 0,
                mirror: BufRef::new(1, at),
            },
            &[],
        );
    }
    g.commit_and_wait().expect("setup");

    g.push(cmd::BeginCommandBuffer {}, &[]);
    commit_nowait(&mut g);
    g.push(
        cmd::QueueEvent {
            event: 9,
            wait: 1,
            value: 1,
            ..Default::default()
        },
        &[],
    );
    commit_poke(&mut g, 3, 3 * MIB, A);
    commit_poke(&mut g, 4, 3 * MIB + 8, B);
    g.push(
        cmd::QueueEvent {
            event: 10,
            wait: 0,
            value: 1,
            ..Default::default()
        },
        &[],
    );
    g.push(cmd::BeginCommandBuffer {}, &[]);
    commit_nowait(&mut g);
    g.kick();
    std::thread::sleep(Duration::from_millis(300));
    // SAFETY: an aligned u64 in the mapping, written by the host.
    let signaled = unsafe { &*g.ptr(MIRROR + 8, 8).cast::<AtomicU64>() }.load(Ordering::Acquire);
    let early = (poked(&g, A), poked(&g, B), signaled, g.ring.completed());

    g.push(
        cmd::SetSharedEvent {
            handle: 9,
            _pad: 0,
            value: 1,
        },
        &[],
    );
    g.commit_and_wait().expect("after the signal");
    assert_eq!(
        (early, poked(&g, A), poked(&g, B)),
        ((0, 0, 0, 2), 256, 256),
        "(words of A and B, the second event, completed seqno) before the CPU's signal; A and B after"
    );
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

/// Command buffers a guest ends and never commits are held by the replayer until a commit; past a
/// bound the context is lost, rather than the replayer growing until it aborts and takes every
/// context of the VM with it.
#[test]
fn command_buffers_ended_without_a_commit_are_bounded() {
    let mut g = start();
    for _ in 0..20_000 {
        g.push_waiting(cmd::BeginCommandBuffer {});
        g.push_waiting(cmd::EndCommandBuffer {});
    }
    g.kick();
    let start = Instant::now();
    let err = loop {
        if let Some(e) = g.ring.error() {
            break e;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "never lost");
        std::thread::sleep(Duration::from_micros(100));
    };
    assert_eq!(err.0, ring::Error::OutOfMemory as u32);
}

/// The commands of an open command buffer are kept for an export until the commit; a window that
/// never commits is bounded the same way.
#[test]
fn a_command_window_that_never_commits_is_bounded() {
    let mut g = start();
    g.push_waiting(cmd::BeginCommandBuffer {});
    g.push_waiting(cmd::BeginCompute {});
    for _ in 0..1_500_000 {
        g.push_waiting(cmd::Barrier {
            kind: cmd::barrier_kind::AFTER_QUEUE_STAGES,
            after: STAGE_DISPATCH,
            before: STAGE_DISPATCH,
            visibility: VISIBILITY_DEVICE,
        });
    }
    g.push_waiting(cmd::EndEncoding {});
    g.push_waiting(cmd::EndCommandBuffer {});
    let err = g.commit_and_wait().expect_err("must be lost");
    assert_eq!(err.0, ring::Error::OutOfMemory as u32);
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

/// A GPU host that dies is reported, so the worker can answer the fences it will never complete
/// instead of leaving the guest waiting on them.
#[test]
fn a_gpu_host_that_dies_is_reported() {
    let g = start();
    let (tx, rx) = std::sync::mpsc::channel();
    g.host
        .on_exit(move || tx.send(()).expect("the test waits for this"))
        .expect("watching the GPU host");
    // SAFETY: the GPU host is this test's own child.
    assert_eq!(unsafe { libc::kill(g.host.pid(), libc::SIGKILL) }, 0);
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the GPU host's exit was not reported");
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
                ..Default::default()
            },
            &[],
        );
    }
    g.seqno += 1;
    g.push(
        cmd::Commit {
            seqno: g.seqno,
            feedback: BufRef::NONE,
            ..Default::default()
        },
        &[],
    );
    g.kick();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(g.ring.completed(), 0, "the commit is held by the waits");

    let start = Instant::now();
    g.host.destroy_context(1).expect("context gone");
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
    // The GPU host still serves other contexts.
    g.host.create_context(2).expect("a new context");
}

/// A BGRA IOSurface of the worker's, as `SURFACE_CREATE` mints them.
#[allow(deprecated)]
fn bgra_surface(
    w: u32,
    h: u32,
    pitch: i32,
) -> objc2_core_foundation::CFRetained<objc2_io_surface::IOSurfaceRef> {
    use std::ffi::c_void;

    use objc2_core_foundation::{
        CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, kCFTypeDictionaryKeyCallBacks,
        kCFTypeDictionaryValueCallBacks,
    };
    use objc2_io_surface::{
        IOSurfaceCreate, kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight,
        kIOSurfacePixelFormat, kIOSurfaceWidth,
    };

    let num = |v: i32| -> CFRetained<CFNumber> {
        // SAFETY: a pointer to an i32 for the SInt32 type.
        unsafe { CFNumber::new(None, CFNumberType::SInt32Type, (&v as *const i32).cast()) }
            .expect("a number")
    };
    let values = [
        num(w as i32),
        num(h as i32),
        num(4),
        num(pitch),
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
        IOSurfaceCreate(&dict).expect("a surface")
    }
}

/// A texture made over an IOSurface the worker filed for the context renders into the surface's
/// own pages: what makes a buffer this transport draws reachable by vrend and venus, which adopt
/// the same surface.
#[test]
#[allow(deprecated)]
fn a_texture_over_a_bound_surface_renders_into_the_surface() {
    use objc2_io_surface::{
        IOSurfaceCreateMachPort, IOSurfaceGetBaseAddress, IOSurfaceLock, IOSurfaceLockOptions,
        IOSurfaceUnlock,
    };

    const W: u32 = 64;
    const H: u32 = 8;
    const PITCH: i32 = 256;
    let surface = bgra_surface(W, H, PITCH);

    let mut g = start();
    // The right is ours and moves to the GPU host.
    let port = IOSurfaceCreateMachPort(&surface);
    g.host
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

    g.host.release_surface(1, 7).expect("released");
    assert!(g.host.release_surface(1, 7).is_err(), "only once");
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

/// What a guest is told by one fresh GPU host running a fixed sequence of arena and texture
/// creates: each arena's GPU base and each texture's resource ID, in order. With `destroy_mid`,
/// one arena and one texture are destroyed partway through, as they are in a live guest.
fn host_values(destroy_mid: bool) -> Vec<u64> {
    let mut g = start();
    let mut seen = vec![g.arena.gpu_base];
    for (id, mib) in [(2, 8), (3, 1), (4, 32)] {
        seen.push(
            g.host
                .create_arena(1, id, mib * MIB)
                .expect("arena")
                .gpu_base,
        );
    }
    if destroy_mid {
        g.host.destroy_arena(1, 3).expect("destroy");
    }
    seen.push(g.host.create_arena(1, 5, 2 * MIB).expect("arena").gpu_base);
    let texture = |g: &mut Guest, handle: u32| {
        let reply = g.reply_slot(RECORDS);
        g.push(
            cmd::CreateTexture {
                handle,
                arena: 1,
                offset: 16 * MIB + u64::from(handle) * MIB,
                desc: cmd::TextureDesc {
                    texture_type: TEX_2D,
                    pixel_format: RGBA8_UNORM,
                    width: 64,
                    height: 64,
                    depth: 1,
                    mip_levels: 1,
                    array_length: 1,
                    sample_count: 1,
                    usage: USAGE_SHADER_READ,
                    ..Default::default()
                },
                reply,
                ..Default::default()
            },
            &[],
        );
        g.reply(RECORDS).values[0]
    };
    for h in 10..15 {
        seen.push(texture(&mut g, h));
    }
    if destroy_mid {
        g.push(
            cmd::Destroy {
                handle: 12,
                _pad: 0,
            },
            &[],
        );
    }
    seen.push(texture(&mut g, 15));
    seen
}

/// Measurement, not a check: whether two fresh GPU hosts tell the guest the same arena bases and
/// texture IDs for the same sequence, with and without destroys in the middle, and what a replay
/// of only the survivors would be told. Run with `--ignored --nocapture`.
#[test]
#[ignore]
fn are_host_values_reproducible_across_gpu_hosts() {
    for destroy_mid in [false, true] {
        let a = host_values(destroy_mid);
        let b = host_values(destroy_mid);
        eprintln!("destroy_mid={destroy_mid}");
        eprintln!("  run 1: {:x?}", a);
        eprintln!("  run 2: {:x?}", b);
        eprintln!("  equal: {}", a == b);
    }
}

/// Measurement: how texture IDs and arena bases are reused under churn, in one GPU host. Run with
/// `--ignored --nocapture`.
#[test]
#[ignore]
fn how_host_values_are_reused_under_churn() {
    let run = || {
        let mut g = start();
        let mut ids = Vec::new();
        for round in 0..400u32 {
            let reply = g.reply_slot(RECORDS);
            g.push(
                cmd::CreateTexture {
                    handle: 100,
                    arena: 1,
                    offset: 16 * MIB,
                    desc: cmd::TextureDesc {
                        texture_type: TEX_2D,
                        pixel_format: RGBA8_UNORM,
                        width: 64,
                        height: 64,
                        depth: 1,
                        mip_levels: 1,
                        array_length: 1,
                        sample_count: 1,
                        usage: USAGE_SHADER_READ,
                        ..Default::default()
                    },
                    reply,
                    ..Default::default()
                },
                &[],
            );
            let id = g.reply(RECORDS).values[0];
            if round < 6 || round % 50 == 0 || round == 399 {
                ids.push((round, id));
            }
            g.push(
                cmd::Destroy {
                    handle: 100,
                    _pad: 0,
                },
                &[],
            );
            g.commit_and_wait().expect("destroy");
        }
        let mut bases = Vec::new();
        for round in 0..6u32 {
            let a = g.host.create_arena(1, 50, 4 * MIB).expect("arena");
            bases.push((round, a.gpu_base));
            drop(a);
            g.host.destroy_arena(1, 50).expect("destroy");
        }
        (ids, bases)
    };
    let (ids1, bases1) = run();
    let (ids2, bases2) = run();
    eprintln!("texture ids (round, id): {ids1:x?}");
    eprintln!("arena bases (round, base): {bases1:x?}");
    eprintln!(
        "second GPU host equal: ids {} bases {}",
        ids1 == ids2,
        bases1 == bases2
    );
}

fn churn_texture(g: &mut Guest, handle: u32) -> u64 {
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateTexture {
            handle,
            arena: 1,
            offset: 16 * MIB,
            desc: cmd::TextureDesc {
                texture_type: TEX_2D,
                pixel_format: RGBA8_UNORM,
                width: 64,
                height: 64,
                depth: 1,
                mip_levels: 1,
                array_length: 1,
                sample_count: 1,
                usage: USAGE_SHADER_READ,
                ..Default::default()
            },
            reply,
            ..Default::default()
        },
        &[],
    );
    let r = g.reply(RECORDS);
    assert_eq!(r.status, 0, "texture {handle}");
    r.values[0]
}

fn churn_sampler(g: &mut Guest, handle: u32) -> u64 {
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateSampler {
            handle,
            _pad: 0,
            desc: cmd::SamplerDesc {
                lod_max_clamp: 1000.0,
                normalized_coordinates: 1,
                ..Default::default()
            },
            reply,
        },
        &[],
    );
    let r = g.reply(RECORDS);
    assert_eq!(r.status, 0, "sampler {handle}");
    r.values[0]
}

fn churn_destroy(g: &mut Guest, handle: u32) {
    g.push(cmd::Destroy { handle, _pad: 0 }, &[]);
    g.commit_and_wait().expect("destroy");
}

/// Measurement: whether a fresh GPU host can be brought to hand out recorded resource IDs again,
/// by filling the slots below each one with placeholders. Also whether textures and samplers
/// draw from one ID space. Run with `--ignored --nocapture`.
#[test]
#[ignore]
fn can_placeholders_reproduce_recorded_ids() {
    let mut g = start();
    let t1 = churn_texture(&mut g, 1000);
    let s1 = churn_sampler(&mut g, 1001);
    let t2 = churn_texture(&mut g, 1002);
    eprintln!("texture {t1}, sampler {s1}, texture {t2}");
    churn_destroy(&mut g, 1000);
    churn_destroy(&mut g, 1001);
    churn_destroy(&mut g, 1002);

    // The recorded run: twelve textures, three destroyed partway, two more after.
    let mut live = std::collections::BTreeMap::new();
    for h in 10..22 {
        live.insert(h, churn_texture(&mut g, h));
    }
    for h in [12, 13, 15] {
        churn_destroy(&mut g, h);
        live.remove(&h);
    }
    for h in 22..24 {
        live.insert(h, churn_texture(&mut g, h));
    }
    eprintln!("recorded (handle, id): {live:?}");

    // A fresh GPU host, survivors only, each brought to its recorded id.
    let mut g = start();
    let mut by_id: Vec<(u64, u32)> = live.iter().map(|(&h, &id)| (id, h)).collect();
    by_id.sort();
    let mut placeholders = Vec::new();
    let mut next_placeholder = 5000;
    let (mut matched, mut retries) = (0, 0);
    for (target, handle) in by_id {
        loop {
            let p = next_placeholder;
            next_placeholder += 1;
            let id = churn_texture(&mut g, p);
            if id < target {
                placeholders.push(p);
                continue;
            }
            assert_eq!(id, target, "overshot: the slot for {handle} was taken");
            // Every lower slot is held: free this one and take it with the real handle.
            churn_destroy(&mut g, p);
            let got = churn_texture(&mut g, handle);
            if got == target {
                matched += 1;
                break;
            }
            retries += 1;
            assert!(retries < 100, "{handle}: got {got}, wanted {target}");
            churn_destroy(&mut g, handle);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    for p in &placeholders {
        churn_destroy(&mut g, *p);
    }
    eprintln!(
        "fill: {matched}/{} matched, {retries} retries, {} placeholders",
        live.len(),
        placeholders.len()
    );
}

/// Measurement: where a fresh arena lands once others have been destroyed. Run with
/// `--ignored --nocapture`.
#[test]
#[ignore]
fn where_arenas_land_after_holes() {
    let g = start();
    let base = |id: u32, mib: u64| {
        let a = g.host.create_arena(1, id, mib * MIB).expect("arena");
        eprintln!("arena {id} ({mib} MiB) at 0x{:x}", a.gpu_base);
    };
    for (id, mib) in [(2, 8), (3, 1), (4, 32), (5, 2)] {
        base(id, mib);
    }
    for id in [3, 4] {
        g.host.destroy_arena(1, id).expect("destroy");
        eprintln!("destroyed arena {id}");
    }
    for (id, mib) in [(6, 4), (7, 16), (8, 1), (9, 64)] {
        base(id, mib);
    }
}

/// Measurement: whether starting a context (its queue, compiler and residency set) takes GPU
/// addresses from the space arenas are placed in. The same three arenas, with a second context
/// started before them and after them. Run with `--ignored --nocapture`.
#[test]
#[ignore]
fn does_a_context_start_move_arenas() {
    for context_first in [true, false] {
        let g = start();
        let second = |g: &Guest| {
            let _bell = g.host.create_context(2).expect("context");
            let a = g.host.create_arena(2, 1, 4 * MIB).expect("ring arena");
            eprintln!("  context 2 ring arena at 0x{:x}", a.gpu_base);
            // SAFETY: the arena's mapping is leaked below, so it outlives the producer.
            let ring =
                unsafe { Producer::init(NonNull::new(a.mapping.as_ptr()).unwrap(), RING_DATA) };
            g.host.attach_ring(2, 1, 0, RING_DATA).expect("attach");
            std::mem::forget((a, ring));
        };
        eprintln!("context_first={context_first}");
        if context_first {
            second(&g);
        }
        for (id, mib) in [(2, 8), (3, 1), (4, 32)] {
            let a = g.host.create_arena(1, id, mib * MIB).expect("arena");
            eprintln!("  arena {id} at 0x{:x}", a.gpu_base);
        }
        if !context_first {
            second(&g);
        }
    }
}

fn export(g: &mut Guest) -> limina_gpu_host::journal::ContextExport {
    let bytes = g.host.export_context(1).expect("export");
    limina_gpu_host::journal::ContextExport::decode(&bytes).expect("decodes")
}

/// Builds a little of everything a context holds, including objects the guest destroyed while
/// something it made still names them. Returns the texture's and the sampler's resource IDs.
fn populate(g: &mut Guest) -> (u64, u64) {
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
    assert_eq!(g.reply(RECORDS).status, 0);
    g.push(cmd::Destroy { handle: 1, _pad: 0 }, &[]);
    let texture = churn_texture(g, 3);
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateTextureView {
            handle: 4,
            parent: 3,
            pixel_format: RGBA8_UNORM,
            texture_type: TEX_2D,
            level_count: 1,
            slice_count: 1,
            reply,
            ..Default::default()
        },
        &[],
    );
    assert_eq!(g.reply(RECORDS).status, 0);
    g.push(cmd::Destroy { handle: 3, _pad: 0 }, &[]);
    let sampler = churn_sampler(g, 5);
    let base = g.arena.gpu_base;
    table(g, 6, &[(0, base + MIB)]);
    g.push(
        cmd::SyncObject {
            handle: 7,
            kind: cmd::sync_kind::EVENT,
            ..Default::default()
        },
        &[],
    );
    g.push(
        cmd::QueueEvent {
            event: 7,
            wait: 0,
            value: 5,
            ..Default::default()
        },
        &[],
    );
    g.commit_and_wait().expect("commit");
    (texture, sampler)
}

#[test]
fn an_export_holds_every_object_and_what_it_answered() {
    let mut g = start();
    let (texture, sampler) = populate(&mut g);
    let e = export(&mut g);
    assert!(!e.lost);
    assert_eq!(e.last_seqno, g.seqno);
    assert_eq!(e.position, g.ring.position());
    assert!(e.recording.is_empty(), "{:?}", e.recording);
    let by_handle = |h: u32| e.objects.iter().find(|o| o.handle == h).expect("listed");
    // The pipeline still names its library, which only it keeps.
    let pipeline = by_handle(2);
    assert_eq!(pipeline.op, cmd::op::CREATE_COMPUTE_PIPELINE);
    let library = &e.objects[pipeline.refs[0] as usize];
    assert_eq!((library.handle, library.op), (0, cmd::op::CREATE_LIBRARY));
    assert!(pipeline.value[0] >= 64);
    // The view names its parent, and the parent's answer is the resource ID the guest holds.
    let view = by_handle(4);
    let parent = &e.objects[view.refs[0] as usize];
    assert_eq!((parent.handle, parent.op), (0, cmd::op::CREATE_TEXTURE));
    assert_eq!(parent.value[0], texture);
    assert_ne!(view.value[0], 0);
    assert_eq!(by_handle(5).value[0], sampler);
    assert_eq!(e.objects.iter().filter(|o| o.handle != 0).count(), 5);
    let base = g.arena.gpu_base;
    assert_eq!(e.tables, vec![(6, vec![base + MIB, 0, 0, 0])]);
    assert_eq!(e.events, vec![(7, 5)]);
}

/// Command buffers begun and not committed: the export describes the context as the first began,
/// and carries their commands to run again.
#[test]
fn an_export_inside_open_command_buffers_carries_their_commands() {
    let mut g = start();
    populate(&mut g);
    let base = g.arena.gpu_base;
    g.push(cmd::BeginCommandBuffer {}, &[]);
    table(&mut g, 8, &[(1, base + 2 * MIB)]);
    g.push(
        cmd::SetAddresses { table: 6, count: 1 },
        &[cmd::bytes_of_slice(&[cmd::AddressBinding {
            index: 0,
            _pad: 0,
            address: base + 3 * MIB,
        }])],
    );
    g.push(cmd::Destroy { handle: 5, _pad: 0 }, &[]);
    // Answered once the GPU host has consumed everything before it.
    let texture = churn_texture(&mut g, 9);
    assert_ne!(texture, 0);
    let e = export(&mut g);
    assert!(!e.lost);
    let handles: Vec<u32> = e
        .objects
        .iter()
        .map(|o| o.handle)
        .filter(|&h| h != 0)
        .collect();
    assert!(
        handles.contains(&5),
        "a destroy inside the window runs again: {handles:?}"
    );
    assert!(
        !handles.contains(&8) && !handles.contains(&9),
        "{handles:?}"
    );
    assert_eq!(e.tables, vec![(6, vec![base + MIB, 0, 0, 0])]);
    let ops: Vec<u16> = e.recording.iter().map(|(op, _, _)| *op).collect();
    assert_eq!(
        ops,
        vec![
            cmd::op::BEGIN_COMMAND_BUFFER,
            cmd::op::CREATE_ARGUMENT_TABLE,
            cmd::op::SET_ADDRESSES,
            cmd::op::SET_ADDRESSES,
            cmd::op::DESTROY,
            cmd::op::CREATE_TEXTURE,
        ]
    );
}

/// Arenas re-created in a fresh GPU host at the addresses a guest holds, around the holes destroyed
/// arenas left and the space a context's start takes.
#[test]
fn arenas_are_restored_at_their_addresses() {
    let g = start();
    let mut live = Vec::new();
    for (id, mib) in [(2, 8), (3, 1), (4, 32), (5, 2), (6, 1)] {
        let a = g.host.create_arena(1, id, mib * MIB).expect("arena");
        live.push((id, a.mapping.size(), a.gpu_base));
    }
    for id in [3, 5] {
        g.host.destroy_arena(1, id).expect("destroy");
        live.retain(|&(i, _, _)| i != id);
    }
    let a = g.host.create_arena(1, 7, 4 * MIB).expect("arena");
    live.push((7, a.mapping.size(), a.gpu_base));
    live.push((1, g.arena.mapping.size(), g.arena.gpu_base));
    live.sort_by_key(|&(_, _, base)| base);

    let fresh = GpuHost::spawn(Path::new(env!("CARGO_BIN_EXE_limina-gpu-host"))).expect("spawn");
    let _bell = fresh.create_context(1).expect("context");
    let mut kept = Vec::new();
    for &(id, size, base) in &live {
        let a = fresh.restore_arena(1, id, size, base).expect("restore");
        assert_eq!(a.gpu_base, base, "arena {id}");
        assert_eq!(a.mapping.size(), size, "arena {id}");
        kept.push(a);
    }
    fresh.restore_done().expect("done");
    let after = fresh.create_arena(1, 50, MIB).expect("arena");
    // Placeholders are gone: a new arena takes the lowest hole it fits, here arena 3's.
    let lowest_hole = live
        .windows(2)
        .map(|w| (w[0].2 + w[0].1 + 0x8000, w[1].2))
        .find(|&(start, next)| next >= start + MIB + 0x8000)
        .expect("a hole")
        .0;
    assert_eq!(after.gpu_base, lowest_hole);
}

/// A texture outlives the guest's hold on the surface under it: the guest gives the resource back
/// as it destroys the texture, and the destroy can still be on the ring when a snapshot is taken.
/// The restore remakes the texture all the same, and the context comes back whole.
#[test]
fn a_texture_whose_surface_was_released_restores() {
    let surface = bgra_surface(64, 8, 256);
    let mut g = start();
    g.host
        .bind_surface(1, 7, surface.create_mach_port())
        .expect("the surface is filed");
    let desc = cmd::TextureDesc {
        texture_type: TEX_2D,
        pixel_format: BGRA8_UNORM,
        width: 64,
        height: 8,
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
    assert_eq!(g.reply(RECORDS).status, 0, "a texture over the surface");
    g.host.release_surface(1, 7).expect("released");

    let before = export(&mut g);
    let mut g = restore_into_fresh(g);
    let after = export(&mut g);
    assert!(!after.lost, "restored lost: {:?}", g.ring.error());
    assert_eq!(after, before);

    // The remade texture still takes a render pass.
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
    g.commit_and_wait().expect("a pass over the remade texture");
    assert!(g.ring.error().is_none());
}

/// Moves the guest's context into a fresh GPU host the way a snapshot restore does: the arena at
/// its address with its contents, then the context's objects and ring.
fn restore_into_fresh(g: Guest) -> Guest {
    let export = g.host.export_context(1).expect("export");
    let Guest {
        host: old,
        ring,
        arena: old_arena,
        seqno,
        ..
    } = g;
    let fresh = GpuHost::spawn(Path::new(env!("CARGO_BIN_EXE_limina-gpu-host"))).expect("spawn");
    let bell = fresh.create_context(1).expect("context");
    let arena = fresh
        .restore_arena(1, 1, old_arena.mapping.size(), old_arena.gpu_base)
        .expect("arena");
    // SAFETY: both mappings are the arena's size; the old GPU host no longer writes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            old_arena.mapping.as_ptr(),
            arena.mapping.as_ptr(),
            arena.mapping.size() as usize,
        )
    };
    drop(old);
    fresh
        .restore_context(1, 1, 0, RING_DATA, &export)
        .expect("restore");
    fresh.restore_done().expect("done");
    // SAFETY: the new mapping holds a copy of the ring and lives as long as the producer.
    let ring = unsafe { ring.moved_to(NonNull::new(arena.mapping.as_ptr()).unwrap()) };
    Guest {
        host: fresh,
        bell,
        ring,
        arena,
        seqno,
    }
}

/// Textures and samplers made and destroyed first, so the IDs a context holds have holes below.
fn churn_ids(g: &mut Guest) {
    for h in 100..106 {
        churn_texture(g, h);
    }
    for h in 106..110 {
        churn_sampler(g, h);
    }
    for h in [100, 102, 103, 107] {
        churn_destroy(g, h);
    }
}

#[test]
fn a_context_restored_in_a_fresh_gpu_host_is_the_one_the_guest_knew() {
    let mut g = start();
    churn_ids(&mut g);
    populate(&mut g);
    let before = export(&mut g);
    let mut g = restore_into_fresh(g);
    let after = export(&mut g);
    assert!(!after.lost, "restored lost: {:?}", g.ring.error());
    assert_eq!(after, before);

    // The restored objects work: a view of the restored view, a library for the pipeline's name,
    // and a commit that waits for the event to have reached its restored value.
    let reply = g.reply_slot(RECORDS);
    g.push(
        cmd::CreateTextureView {
            handle: 20,
            parent: 4,
            pixel_format: RGBA8_UNORM,
            texture_type: TEX_2D,
            level_count: 1,
            slice_count: 1,
            reply,
            ..Default::default()
        },
        &[],
    );
    assert_eq!(g.reply(RECORDS).status, 0);
    g.push(
        cmd::QueueEvent {
            event: 7,
            wait: 1,
            value: 5,
            ..Default::default()
        },
        &[],
    );
    g.commit_and_wait().expect("commit after restore");
    assert!(g.ring.error().is_none());
}

#[test]
fn a_context_restored_inside_open_command_buffers_runs_them_again() {
    let mut g = start();
    churn_ids(&mut g);
    populate(&mut g);
    let base = g.arena.gpu_base;
    g.push(cmd::BeginCommandBuffer {}, &[]);
    table(&mut g, 8, &[(1, base + 2 * MIB)]);
    g.push(cmd::Destroy { handle: 5, _pad: 0 }, &[]);
    let texture = churn_texture(&mut g, 9);
    let before = export(&mut g);
    let mut g = restore_into_fresh(g);
    let after = export(&mut g);
    assert!(!after.lost, "restored lost: {:?}", g.ring.error());
    assert_eq!(after, before);
    let made = after
        .recording
        .iter()
        .find(|(op, _, _)| *op == cmd::op::CREATE_TEXTURE);
    assert_eq!(made.expect("recorded").2[0], texture);
    g.push(cmd::EndCommandBuffer {}, &[]);
    g.commit_and_wait()
        .expect("the open command buffer commits");
    let e = export(&mut g);
    assert!(e.recording.is_empty());
    let handles: Vec<u32> = e.objects.iter().map(|o| o.handle).collect();
    assert!(handles.contains(&8) && handles.contains(&9) && !handles.contains(&5));
    assert_eq!(
        e.tables.iter().find(|(h, _)| *h == 8).unwrap().1[1],
        base + 2 * MIB
    );
}

/// A commit the frozen guest would have unblocked cannot finish before a snapshot: the export
/// gives up on it and loses the context there and then, so its waiters are answered.
#[test]
fn an_export_that_cannot_drain_loses_the_context() {
    let mut g = start();
    g.push(
        cmd::SyncObject {
            handle: 1,
            kind: cmd::sync_kind::SHARED_EVENT,
            ..Default::default()
        },
        &[],
    );
    g.push(
        cmd::QueueEvent {
            event: 1,
            wait: 1,
            value: 1,
            ..Default::default()
        },
        &[],
    );
    g.seqno += 1;
    g.push(
        cmd::Commit {
            seqno: g.seqno,
            feedback: BufRef::NONE,
            ..Default::default()
        },
        &[],
    );
    g.kick();
    let e = export(&mut g);
    assert!(e.lost);
    assert!(e.objects.is_empty());
    assert_eq!(
        g.ring.error().map(|(code, _)| code),
        Some(13),
        "the ring says why"
    );
}

/// Measurement: whether objects other than arenas take GPU addresses from the space arenas are
/// placed in. A 1 MiB arena after each kind of object; a gap other than 0x8000 is space taken.
/// Run with `--ignored --nocapture`.
#[test]
#[ignore]
fn do_other_objects_take_arena_space() {
    let mut g = start();
    let mut id = 100;
    let mut last = g.host.create_arena(1, id, MIB).expect("arena");
    let mut step = |g: &mut Guest, what: &str| {
        id += 1;
        let a = g.host.create_arena(1, id, MIB).expect("arena");
        eprintln!(
            "after {what:<22} arena at {:#x}, gap {:#x}",
            a.gpu_base,
            a.gpu_base.wrapping_sub(last.gpu_base + last.mapping.size())
        );
        last = a;
    };
    step(&mut g, "nothing");
    create_library(&mut g, 1, MSL, BufRef::NONE);
    g.commit_and_wait().expect("commit");
    step(&mut g, "a library");
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
    assert_eq!(g.reply(RECORDS).status, 0);
    step(&mut g, "a compute pipeline");
    churn_texture(&mut g, 3);
    step(&mut g, "a texture");
    churn_sampler(&mut g, 5);
    step(&mut g, "a sampler");
    let base = g.arena.gpu_base;
    table(&mut g, 6, &[(0, base + MIB)]);
    g.commit_and_wait().expect("commit");
    step(&mut g, "an argument table");
    g.push(
        cmd::SyncObject {
            handle: 7,
            kind: cmd::sync_kind::EVENT,
            ..Default::default()
        },
        &[],
    );
    g.commit_and_wait().expect("commit");
    step(&mut g, "an event");
    g.push(
        cmd::CreateCounterHeap {
            handle: 8,
            count: 2,
            reply: BufRef::NONE,
        },
        &[],
    );
    g.commit_and_wait().expect("commit");
    step(&mut g, "a counter heap");
    // `run_poke` makes its own pipeline 2 and table 3.
    for h in [2, 3] {
        g.push(cmd::Destroy { handle: h, _pad: 0 }, &[]);
    }
    run_poke(&mut g, base + 2 * MIB, 64);
    step(&mut g, "a dispatch");
    for h in [2, 1, 3, 5, 6, 7, 8] {
        g.push(cmd::Destroy { handle: h, _pad: 0 }, &[]);
    }
    g.commit_and_wait().expect("commit");
    step(&mut g, "destroying them");
}

/// A counter heap takes less GPU address space than the smallest arena, so the hole it leaves in
/// a fresh GPU host below the arena made after it is one no arena can fill.
#[test]
fn an_arena_made_after_a_counter_heap_is_restored_at_its_address() {
    let mut g = start();
    let before = g.host.create_arena(1, 2, MIB).expect("arena");
    g.push(
        cmd::CreateCounterHeap {
            handle: 8,
            count: 2,
            reply: BufRef::NONE,
        },
        &[],
    );
    g.commit_and_wait().expect("commit");
    let after = g.host.create_arena(1, 3, MIB).expect("arena");
    let mut live = vec![
        (1, g.arena.mapping.size(), g.arena.gpu_base),
        (2, before.mapping.size(), before.gpu_base),
        (3, after.mapping.size(), after.gpu_base),
    ];
    live.sort_by_key(|&(_, _, base)| base);

    let fresh = GpuHost::spawn(Path::new(env!("CARGO_BIN_EXE_limina-gpu-host"))).expect("spawn");
    let _bell = fresh.create_context(1).expect("context");
    let mut kept = Vec::new();
    for &(id, size, base) in &live {
        let a = fresh.restore_arena(1, id, size, base).expect("restore");
        assert_eq!(a.gpu_base, base, "arena {id}");
        kept.push(a);
    }
    fresh.restore_done().expect("done");
}
