//! The replayer end to end, with this test process playing the guest: it maps arenas the way the
//! worker does, writes commands into a ring in arena memory, rings the doorbell when the consumer
//! has parked, and reads results back from arena memory. No VM is involved.

use std::path::Path;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use limina_gpu_wire::cmd::{self, BufRef};
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

const MSL: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VOut { float4 pos [[position]]; };

vertex VOut vs(uint vid [[vertex_id]], const device float4* pos [[buffer(0)]]) {
    VOut o;
    o.pos = pos[vid];
    return o;
}

fragment float4 fs(VOut in [[stage_in]]) {
    return float4(0.0, 1.0, 0.0, 1.0);
}

// The destination arrives as a raw GPU address the guest computed (arena base + offset); no
// buffer is bound for it.
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

    /// Commits, kicks if the consumer parked, and waits for the GPU to finish.
    fn commit_and_wait(&mut self) -> Result<(), (u32, u64)> {
        self.seqno += 1;
        self.push(cmd::Commit { seqno: self.seqno }, &[]);
        if self.ring.publish() {
            self.bell.ring();
        }
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

    fn bytes(&self, offset: u64, len: usize) -> &[u8] {
        assert!(offset + len as u64 <= self.arena.mapping.size());
        // SAFETY: inside the mapping, checked above; the GPU has finished writing.
        unsafe { std::slice::from_raw_parts(self.arena.mapping.as_ptr().add(offset as usize), len) }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        assert!(offset + data.len() as u64 <= self.arena.mapping.size());
        // SAFETY: inside the mapping, checked above.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.arena.mapping.as_ptr().add(offset as usize),
                data.len(),
            )
        }
    }
}

fn start() -> Guest {
    let exe = env!("CARGO_BIN_EXE_limina-replayer");
    Guest::new(Replayer::spawn(Path::new(exe)).expect("spawn replayer"))
}

fn create_library(g: &mut Guest, handle: u32) {
    g.push(
        cmd::CreateLibrary {
            handle,
            source_len: MSL.len() as u32,
        },
        &[MSL.as_bytes()],
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

    create_library(&mut g, 1);
    let (vs, fs) = (b"vs", b"fs");
    let mut colors = [0u32; cmd::MAX_COLOR_ATTACHMENTS];
    colors[0] = RGBA8_UNORM;
    g.push(
        cmd::CreateRenderPipeline {
            handle: 2,
            library: 1,
            vertex_name_len: 2,
            fragment_name_len: 2,
            color_formats: colors,
            sample_count: 1,
            ..Default::default()
        },
        &[vs, fs],
    );
    g.push(
        cmd::CreateTexture {
            handle: 3,
            arena: 1,
            offset: TEX,
            texture_type: TEX_2D,
            pixel_format: RGBA8_UNORM,
            width: W,
            height: W,
            depth: 1,
            mip_levels: 1,
            array_length: 1,
            sample_count: 1,
            usage: USAGE_RENDER_TARGET | USAGE_SHADER_READ,
            _pad: 0,
        },
        &[],
    );
    let mut pass = cmd::BeginRenderPass::default();
    pass.colors[0] = cmd::ColorAttachment {
        texture: 3,
        load_action: LOAD_CLEAR,
        store_action: STORE_STORE,
        clear: [1.0, 0.0, 0.0, 1.0],
        ..Default::default()
    };
    g.push(pass, &[]);
    g.push(cmd::SetRenderPipeline { handle: 2, _pad: 0 }, &[]);
    g.push(
        cmd::SetBuffer {
            stage: cmd::stage::VERTEX,
            index: 0,
            buffer: BufRef::new(1, VB),
        },
        &[],
    );
    g.push(
        cmd::SetViewport {
            x: 0.0,
            y: 0.0,
            width: W as f32,
            height: W as f32,
            znear: 0.0,
            zfar: 1.0,
        },
        &[],
    );
    g.push(
        cmd::DrawPrimitives {
            primitive: PRIM_TRIANGLE,
            vertex_count: 3,
            instance_count: 1,
            ..Default::default()
        },
        &[],
    );
    g.push(cmd::EndEncoding {}, &[]);
    g.push(cmd::BeginBlit {}, &[]);
    g.push(
        cmd::CopyTextureToBuffer {
            texture: 3,
            size: [W, W, 1],
            dst: BufRef::new(1, READBACK),
            bytes_per_row: W as u64 * 4,
            bytes_per_image: (W * W) as u64 * 4,
            ..Default::default()
        },
        &[],
    );
    g.push(cmd::EndEncoding {}, &[]);
    g.commit_and_wait().expect("frame");

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

    create_library(&mut g, 1);
    g.push(
        cmd::CreateComputePipeline {
            handle: 2,
            library: 1,
            name_len: 4,
            _pad: 0,
        },
        &[b"poke"],
    );
    g.push(cmd::BeginCompute {}, &[]);
    g.push(cmd::SetComputePipeline { handle: 2, _pad: 0 }, &[]);
    g.push(
        cmd::SetBuffer {
            stage: cmd::stage::COMPUTE,
            index: 0,
            buffer: BufRef::new(1, PTRS),
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
        g.push(cmd::BeginBlit {}, &[]);
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
        g.commit_and_wait().expect("commit");
    }
    assert!(g.bytes(MIB, 4096).iter().all(|&b| b == (1999 & 0xff) as u8));
}
