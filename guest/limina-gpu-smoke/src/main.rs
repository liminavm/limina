//! First light for limina's Metal-level GPU transport, from inside a guest.
//!
//! Drives the virtio-gpu extension through the limina DRM ioctls (guest/virtio-gpu-limina): one
//! context, its doorbell page, two arenas, a command ring in the first. Then it draws a triangle
//! and has a kernel write through a GPU address it computed itself, and checks both results in
//! arena memory. Last, it shares an arena with another process the way a dma-buf travels: export
//! as a GEM object, PRIME to an fd, import in a child with its own context, and a kernel there
//! writes what the parent reads back. Then a fence: a commit that waits on a shared event
//! nobody has set, fenced as a sync_file and on a shared arena's dma-buf, must leave both
//! unsignaled until the guest sets the event, and both signaled after; fences on two queues each
//! follow their own queue. Exports must also come back
//! promptly on a quiet device, with nothing else kicking the control queue. Exit status 0 means
//! everything came back right.
//!
//! Usage: limina-gpu-smoke [/dev/dri/renderDN]   (default: the first render node that has the
//! extension)

use std::fs::File;
use std::io;
use std::time::{Duration, Instant};

use limina_gpu_wire::cmd::{self, BufRef};

use limina_gpu_client::{Arena, Gpu, MIB, find_node, ioctl, uapi};

const RGBA8_UNORM: u32 = 70;
const TEX_2D: u32 = 2;
const USAGE_SHADER_READ: u32 = 1;
const USAGE_RENDER_TARGET: u32 = 4;
const LOAD_CLEAR: u32 = 2;
const STORE_STORE: u32 = 1;
const PRIM_TRIANGLE: u32 = 3;
const RENDER_VERTEX: u32 = 1;
const STAGE_FRAGMENT: u32 = 1 << 1;
const STAGE_BLIT: u32 = 1 << 27;
const VISIBILITY_DEVICE: u32 = 1;

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

struct Ptrs { device uint* dst; };

kernel void poke(constant Ptrs& p [[buffer(0)]], uint tid [[thread_position_in_grid]]) {
    p.dst[tid] = 0xC0FFEE00u + tid;
}
"#;

/// Library 1: the shaders in `MSL`.
fn library(g: &mut Gpu) {
    g.push(
        cmd::CreateLibrary {
            handle: 1,
            source_len: MSL.len() as u32,
            ..Default::default()
        },
        &[MSL.as_bytes()],
    );
}

fn triangle(g: &mut Gpu) -> Result<(), String> {
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
    g.main.write(VB, &vb);

    library(g);
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
        &[b"vs", b"fs"],
    );
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
            ..Default::default()
        },
        &[],
    );
    let vb_address = g.main.gpu_base + VB;
    g.table(5, vb_address);
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
            table: 5,
            stages: RENDER_VERTEX,
        },
        &[],
    );
    let vp = cmd::Viewport {
        x: 0.0,
        y: 0.0,
        width: W as f64,
        height: W as f64,
        znear: 0.0,
        zfar: 1.0,
    };
    g.push(
        cmd::SetViewports { count: 1, _pad: 0 },
        &[cmd::bytes_of(&vp)],
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
    g.push(
        cmd::Barrier {
            kind: cmd::barrier_kind::AFTER_QUEUE_STAGES,
            after: STAGE_FRAGMENT,
            before: STAGE_BLIT,
            visibility: VISIBILITY_DEVICE,
        },
        &[],
    );
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
    let took = g.commit_and_wait()?;

    let px = |x: u32, y: u32| {
        let at = READBACK + ((y * W + x) * 4) as u64;
        <[u8; 4]>::try_from(g.main.read(at, 4)).unwrap()
    };
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const RED: [u8; 4] = [255, 0, 0, 255];
    for row in (0..W).step_by(4) {
        let line: String = (0..W)
            .step_by(2)
            .map(|x| match px(x, row) {
                GREEN => '#',
                RED => '.',
                _ => '?',
            })
            .collect();
        println!("  {line}");
    }
    let green = (0..W)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .filter(|&(x, y)| px(x, y) == GREEN)
        .count();
    let expect = (W * W) as f64 * 0.32;
    println!("triangle: {green} green pixels (expected ~{expect:.0}), frame took {took:?}");
    if px(W / 2, W / 2) != GREEN || px(0, 0) != RED || (green as f64 - expect).abs() > expect * 0.1
    {
        return Err("triangle pixels are wrong".into());
    }
    Ok(())
}

fn poke(g: &mut Gpu, other: &Arena) -> Result<(), String> {
    const PTRS: u64 = 3 * MIB;
    const DST: u64 = 4096;
    const N: u32 = 256;
    g.main.write(PTRS, &(other.gpu_base + DST).to_le_bytes());
    g.push(
        cmd::CreateComputePipeline {
            handle: 4,
            library: 1,
            name_len: 4,
            ..Default::default()
        },
        &[b"poke"],
    );
    let ptrs = g.main.gpu_base + PTRS;
    g.table(6, ptrs);
    g.push(cmd::BeginCommandBuffer {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    g.push(cmd::SetComputePipeline { handle: 4, _pad: 0 }, &[]);
    g.push(
        cmd::SetArgumentTable {
            table: 6,
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
    let took = g.commit_and_wait()?;
    let words = other.read(DST, N as usize * 4);
    let bad = words
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .filter(|&(i, w)| u32::from_le_bytes(*w) != 0xC0FFEE00 + i as u32)
        .count();
    println!(
        "poke: {} of {N} words written through 0x{:x}, took {took:?}",
        N as usize - bad,
        other.gpu_base + DST
    );
    if bad != 0 {
        return Err(format!("{bad} words wrong"));
    }
    Ok(())
}

/// An arena shared with a child process through a dma-buf: the child imports it into a context of
/// its own, sees what the parent wrote, and pokes words the parent then reads.
fn share(path: &str, f: &File) -> Result<(), String> {
    const MARK: u64 = 0x11A1_7A5E_D000_0001;
    let (shared, handle) =
        Arena::create_shared(f, 3, MIB).map_err(|e| format!("shared arena: {e}"))?;
    shared.write(0, &MARK.to_le_bytes());
    let mut p = uapi::PrimeHandle {
        handle,
        flags: uapi::DRM_CLOEXEC | uapi::DRM_RDWR,
        fd: -1,
    };
    ioctl(f, uapi::PRIME_HANDLE_TO_FD, &mut p).map_err(|e| format!("PRIME export: {e}"))?;
    let dmabuf = p.fd;

    // SAFETY: the child only runs the code below and leaves with _exit.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        let r = (|| -> Result<(), String> {
            let mut g = Gpu::open(path).map_err(|e| format!("child context: {e}"))?;
            let mut p = uapi::PrimeHandle {
                fd: dmabuf,
                ..Default::default()
            };
            ioctl(&g.file, uapi::PRIME_FD_TO_HANDLE, &mut p)
                .map_err(|e| format!("PRIME import: {e}"))?;
            let other =
                Arena::import(&g.file, 5, p.handle).map_err(|e| format!("arena import: {e}"))?;
            if other.gpu_base != shared.gpu_base || other.size != shared.size {
                return Err(format!(
                    "import is at 0x{:x} ({} B), export at 0x{:x} ({} B)",
                    other.gpu_base, other.size, shared.gpu_base, shared.size
                ));
            }
            let mark = u64::from_le_bytes(other.read(0, 8).try_into().unwrap());
            if mark != MARK {
                return Err(format!("child read 0x{mark:x}, parent wrote 0x{MARK:x}"));
            }
            library(&mut g);
            poke(&mut g, &other)
        })();
        if let Err(e) = &r {
            println!("share (child): {e}");
        }
        // SAFETY: leaves the forked child without running the parent's destructors.
        unsafe { libc::_exit(if r.is_ok() { 0 } else { 1 }) };
    }
    let mut status = 0;
    // SAFETY: waits for the child forked above.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        return Err(format!("child failed (status 0x{status:x})"));
    }
    // The child's context and its import are gone; the parent's export still holds the memory.
    let words = shared.read(4096, 256 * 4);
    let bad = (0..256u32)
        .filter(|&i| {
            let w = &words[i as usize * 4..][..4];
            u32::from_le_bytes(w.try_into().unwrap()) != 0xC0FFEE00 + i
        })
        .count();
    println!(
        "share: child imported arena at 0x{:x}; parent reads {} of 256 words it wrote",
        shared.gpu_base,
        256 - bad
    );
    // SAFETY: the fd PRIME gave us.
    unsafe { libc::close(dmabuf) };
    if bad != 0 {
        return Err(format!("{bad} shared words wrong"));
    }
    Ok(())
}

/// A surface the host makes for sharing beyond this transport: a texture over it renders, and as
/// a dma-buf in another file it is an ordinary host3d blob resource, the shape a compositor's
/// driver imports (the host side, vrend adopting it, is virglrs's own test).
fn surface(path: &str, g: &mut Gpu) -> Result<(), String> {
    const W: u32 = 64;
    const H: u32 = 8;
    const STRIDE: u32 = 256;
    const READBACK: u64 = 6 * MIB;
    const BGRA8_UNORM: u32 = 80;
    // Past the queue step's handles (40..=43): a context's handles are one namespace.
    const TEXTURE: u32 = 50;
    let ar24 = u32::from_le_bytes(*b"AR24");
    let f = &g.file;
    let mut bad = uapi::SurfaceCreate {
        width: W,
        height: H,
        drm_fourcc: u32::from_le_bytes(*b"NV12"),
        stride: STRIDE,
        ..Default::default()
    };
    if ioctl(f, uapi::SURFACE_CREATE, &mut bad).is_ok() {
        return Err("a surface in a format the host cannot share was made".into());
    }
    let mut s = uapi::SurfaceCreate {
        width: W,
        height: H,
        drm_fourcc: ar24,
        stride: STRIDE,
        ..Default::default()
    };
    ioctl(f, uapi::SURFACE_CREATE, &mut s).map_err(|e| format!("SURFACE_CREATE: {e}"))?;

    g.push(
        cmd::CreateTextureFromSurface {
            handle: TEXTURE,
            surface: s.res_handle,
            desc: cmd::TextureDesc {
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
            },
            reply: BufRef::NONE,
        },
        &[],
    );
    g.push(cmd::BeginCommandBuffer {}, &[]);
    let mut pass = cmd::BeginRenderPass::default();
    pass.colors[0] = cmd::RenderAttachment {
        texture: TEXTURE,
        load_action: LOAD_CLEAR,
        store_action: STORE_STORE,
        clear: [0.0, 1.0, 0.0, 1.0],
        ..Default::default()
    };
    g.push(pass, &[]);
    g.push(cmd::EndEncoding {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    g.push(
        cmd::Barrier {
            kind: cmd::barrier_kind::AFTER_QUEUE_STAGES,
            after: STAGE_FRAGMENT,
            before: STAGE_BLIT,
            visibility: VISIBILITY_DEVICE,
        },
        &[],
    );
    g.push(
        cmd::CopyBufferTexture {
            texture: TEXTURE,
            size: [W, H, 1],
            buffer: BufRef::new(1, READBACK),
            bytes_per_row: W as u64 * 4,
            bytes_per_image: (W * H) as u64 * 4,
            ..Default::default()
        },
        &[],
    );
    g.push(cmd::EndEncoding {}, &[]);
    g.push(cmd::EndCommandBuffer {}, &[]);
    g.commit_and_wait()?;
    let green = g
        .main
        .read(READBACK, (W * H * 4) as usize)
        .chunks(4)
        .filter(|p| *p == [0, 255, 0, 255])
        .count();

    let mut p = uapi::PrimeHandle {
        handle: s.handle,
        flags: uapi::DRM_CLOEXEC | uapi::DRM_RDWR,
        fd: -1,
    };
    ioctl(&g.file, uapi::PRIME_HANDLE_TO_FD, &mut p).map_err(|e| format!("PRIME export: {e}"))?;
    let other = File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))?;
    let mut imp = uapi::PrimeHandle {
        fd: p.fd,
        ..Default::default()
    };
    let imported = ioctl(&other, uapi::PRIME_FD_TO_HANDLE, &mut imp);
    // SAFETY: the fd PRIME gave us.
    unsafe { libc::close(p.fd) };
    imported.map_err(|e| format!("PRIME import: {e}"))?;
    let mut info = uapi::ResourceInfo {
        bo_handle: imp.handle,
        ..Default::default()
    };
    ioctl(&other, uapi::RESOURCE_INFO, &mut info).map_err(|e| format!("RESOURCE_INFO: {e}"))?;
    println!(
        "surface: resource {} cleared, {green} of {} pixels green; imported elsewhere as resource \
         {}, blob_mem {}, {} bytes",
        s.res_handle,
        W * H,
        info.res_handle,
        info.blob_mem,
        info.size
    );
    let mut close = uapi::GemClose {
        handle: s.handle,
        pad: 0,
    };
    ioctl(&g.file, uapi::GEM_CLOSE, &mut close).map_err(|e| format!("GEM_CLOSE: {e}"))?;
    if green != (W * H) as usize {
        return Err("the surface did not take the clear".into());
    }
    if info.res_handle != s.res_handle
        || info.blob_mem != uapi::BLOB_MEM_HOST3D
        // The object is page-rounded.
        || info.size < STRIDE * H
    {
        return Err("the import is not the surface's resource".into());
    }
    Ok(())
}

/// Whether `fd` polls readable within `ms`: a signaled sync_file, or a dma-buf with no pending
/// write.
fn ready(fd: i32, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut p, 1, ms) == 1 }
}

/// A commit held back by a queue wait on a shared event, fenced: neither the sync_file nor the
/// shared arena's dma-buf may be ready until the guest sets the event.
fn fence(g: &mut Gpu) -> Result<(), String> {
    const EVENT: u32 = 30;
    let f = &g.file;
    let (_shared, handle) =
        Arena::create_shared(f, 4, MIB).map_err(|e| format!("shared arena: {e}"))?;
    let mut p = uapi::PrimeHandle {
        handle,
        flags: uapi::DRM_CLOEXEC | uapi::DRM_RDWR,
        fd: -1,
    };
    ioctl(f, uapi::PRIME_HANDLE_TO_FD, &mut p).map_err(|e| format!("PRIME export: {e}"))?;
    let dmabuf = p.fd;

    g.push(
        cmd::SyncObject {
            handle: EVENT,
            kind: cmd::sync_kind::SHARED_EVENT,
            initial: 0,
            mirror: BufRef::NONE,
        },
        &[],
    );
    g.push(
        cmd::QueueEvent {
            event: EVENT,
            wait: 1,
            value: 1,
            ..Default::default()
        },
        &[],
    );
    let seqno = g.commit_nowait();
    let mut fence = uapi::Fence {
        seqno,
        handles: &handle as *const u32 as u64,
        num_handles: 1,
        flags: uapi::FENCE_FD,
        fence_fd: -1,
        completed_at: 0,
    };
    ioctl(&g.file, uapi::FENCE, &mut fence).map_err(|e| format!("FENCE: {e}"))?;
    let early = (ready(fence.fence_fd, 300), ready(dmabuf, 0));

    g.push(
        cmd::SetSharedEvent {
            handle: EVENT,
            _pad: 0,
            value: 1,
        },
        &[],
    );
    g.kick();
    let start = Instant::now();
    let signaled = ready(fence.fence_fd, 3000);
    let took = start.elapsed();
    let readable = ready(dmabuf, 0);
    let completed = g.ring.completed();
    println!(
        "fence: before the event sync_file {} dma-buf {}; after, sync_file {} in {took:?}, \
         dma-buf {}, completed {completed} of {seqno}",
        if early.0 { "signaled" } else { "pending" },
        if early.1 { "readable" } else { "busy" },
        if signaled { "signaled" } else { "pending" },
        if readable { "readable" } else { "busy" },
    );
    // SAFETY: fds this function got.
    unsafe {
        libc::close(fence.fence_fd);
        libc::close(dmabuf);
    }
    if early.0 || early.1 {
        return Err("signaled before its commit could complete".into());
    }
    if !signaled || !readable || completed < seqno {
        return Err("not signaled after its commit completed".into());
    }
    Ok(())
}

/// A sync_file fence for `seqno` on the completed value at `completed_at`.
fn queue_fence(g: &Gpu, seqno: u64, completed_at: u32) -> io::Result<i32> {
    let mut fence = uapi::Fence {
        seqno,
        flags: uapi::FENCE_FD,
        fence_fd: -1,
        completed_at,
        ..Default::default()
    };
    ioctl(&g.file, uapi::FENCE, &mut fence)?;
    Ok(fence.fence_fd)
}

/// Fences on two queues, each held by a wait on its own shared event, B's committed and fenced
/// first: A's completing must signal A's fence and leave B's pending, which upstream's fence
/// driver gets wrong when both are on one dma-fence context. Then a fence on a value already
/// reached is signaled as the ioctl returns, and a misaligned slot, or one outside the ring's
/// arena, is refused.
fn queue_fences(g: &mut Gpu) -> Result<(), String> {
    const SLOTS: u64 = 8 * MIB;
    const QA: u32 = 40;
    const QB: u32 = 41;
    const EA: u32 = 42;
    const EB: u32 = 43;
    let (sa_at, sb_at) = (SLOTS as u32, SLOTS as u32 + 8);
    g.create_queue(QA, SLOTS);
    g.create_queue(QB, SLOTS + 8);
    let mut seqnos = Vec::new();
    for (q, e) in [(QB, EB), (QA, EA)] {
        g.push(
            cmd::SyncObject {
                handle: e,
                kind: cmd::sync_kind::SHARED_EVENT,
                initial: 0,
                mirror: BufRef::NONE,
            },
            &[],
        );
        g.push(
            cmd::QueueEvent {
                event: e,
                wait: 1,
                value: 1,
                queue: q,
                ..Default::default()
            },
            &[],
        );
        seqnos.push(g.commit_nowait_on(q));
    }
    let (sb, sa) = (seqnos[0], seqnos[1]);
    let fb = queue_fence(g, sb, sb_at).map_err(|e| format!("FENCE on queue B: {e}"))?;
    let fa = queue_fence(g, sa, sa_at).map_err(|e| format!("FENCE on queue A: {e}"))?;
    let set = |g: &mut Gpu, e| {
        g.push(
            cmd::SetSharedEvent {
                handle: e,
                _pad: 0,
                value: 1,
            },
            &[],
        );
        g.kick();
    };
    set(g, EA);
    let a_done = ready(fa, 3000);
    let b_early = ready(fb, 300);
    set(g, EB);
    let b_done = ready(fb, 3000);
    let fc = queue_fence(g, sa, sa_at).map_err(|e| format!("FENCE on a reached value: {e}"))?;
    let c_now = ready(fc, 0);
    let refused = |at| {
        queue_fence(g, sa, at)
            .map(|fd| {
                // SAFETY: an fd this function got.
                unsafe { libc::close(fd) };
            })
            .err()
            .and_then(|e| e.raw_os_error())
            == Some(libc::EINVAL)
    };
    let misaligned = refused(sa_at + 4);
    let outside = refused(g.main.size as u32);
    println!(
        "queue fences: A {} after its event, B {} before its own and {} after; a reached \
         value's fence {} at once; misaligned slot {}, slot outside the arena {}",
        if a_done { "signaled" } else { "pending" },
        if b_early { "signaled" } else { "pending" },
        if b_done { "signaled" } else { "pending" },
        if c_now { "signaled" } else { "pending" },
        if misaligned { "refused" } else { "taken" },
        if outside { "refused" } else { "taken" },
    );
    // SAFETY: fds this function got.
    unsafe {
        libc::close(fa);
        libc::close(fb);
        libc::close(fc);
    }
    if !a_done || b_early || !b_done {
        return Err("a queue's fence followed another queue's completion".into());
    }
    if !c_now || !misaligned || !outside {
        return Err("a queue fence's fast path or its checks are wrong".into());
    }
    Ok(())
}

/// A fence still pending when its context goes away (the file is closed) is signaled then.
fn orphan_fence(path: &str) -> Result<(), String> {
    const EVENT: u32 = 31;
    let mut g = Gpu::open(path).map_err(|e| format!("second context: {e}"))?;
    g.push(
        cmd::SyncObject {
            handle: EVENT,
            kind: cmd::sync_kind::SHARED_EVENT,
            initial: 0,
            mirror: BufRef::NONE,
        },
        &[],
    );
    g.push(
        cmd::QueueEvent {
            event: EVENT,
            wait: 1,
            value: 1,
            ..Default::default()
        },
        &[],
    );
    let seqno = g.commit_nowait();
    let mut fence = uapi::Fence {
        seqno,
        flags: uapi::FENCE_FD,
        fence_fd: -1,
        ..Default::default()
    };
    ioctl(&g.file, uapi::FENCE, &mut fence).map_err(|e| format!("FENCE: {e}"))?;
    let early = ready(fence.fence_fd, 100);
    let start = Instant::now();
    drop(g);
    let signaled = ready(fence.fence_fd, 5000);
    println!(
        "orphan fence: {} while its context lived, {} {:?} after it was closed",
        if early { "signaled" } else { "pending" },
        if signaled {
            "signaled"
        } else {
            "still pending"
        },
        start.elapsed()
    );
    // SAFETY: the fd FENCE gave us.
    unsafe { libc::close(fence.fence_fd) };
    if early || !signaled {
        return Err("an orphaned fence must signal when its context goes, and not before".into());
    }
    Ok(())
}

/// Round trips of an empty commit (a Metal command buffer with no work, retired by its completion
/// handler) after each of three idle gaps: none, a short one, and one long enough for the host to
/// have parked, which needs the doorbell. The gaps match the venus probe's
/// (spikes/metal-heap-addressing/rtt.c) so the two tables compare row for row.
fn round_trips(g: &mut Gpu) -> Result<(), String> {
    const N: usize = 200;
    for gap in [
        Duration::ZERO,
        Duration::from_micros(200),
        Duration::from_millis(5),
    ] {
        let kicks = g.kicks;
        let mut times = Vec::with_capacity(N);
        for _ in 0..N {
            if !gap.is_zero() {
                std::thread::sleep(gap);
            }
            times.push(g.commit_and_wait()?);
        }
        times.sort();
        let q = |p: usize| times[(times.len() * p / 100).min(times.len() - 1)];
        println!(
            "round trips, gap {gap:?}: p50 {:?} p90 {:?} p99 {:?} max {:?}, {} kicks in {N}",
            q(50),
            q(90),
            q(99),
            times[N - 1],
            g.kicks - kicks
        );
    }
    Ok(())
}

/// Exports of fresh shared arenas, spaced so the control queue goes quiet before each: every one
/// must come back within `EXPORT_BOUND`. An export waits for the host to map the arena, so a
/// command queued without a kick leaves it waiting for whatever traffic comes along next. The
/// arenas (ids 100 on) and their GEM handles live until the file closes.
fn export_latency(f: &File) -> Result<(), String> {
    const EXPORTS: u32 = 20;
    const EXPORT_BOUND: Duration = Duration::from_millis(500);
    let mut worst = Duration::ZERO;
    for i in 0..EXPORTS {
        std::thread::sleep(Duration::from_millis(50));
        let id = 100 + i;
        let mut a = uapi::ArenaCreate {
            arena_id: id,
            flags: uapi::ARENA_SHARED,
            size: 64 * 1024,
            ..Default::default()
        };
        ioctl(f, uapi::ARENA_CREATE, &mut a).map_err(|e| format!("export latency: create: {e}"))?;
        let mut e = uapi::ArenaExport {
            arena_id: id,
            handle: 0,
        };
        let t = Instant::now();
        ioctl(f, uapi::ARENA_EXPORT, &mut e).map_err(|e| format!("export latency: export: {e}"))?;
        let took = t.elapsed();
        worst = worst.max(took);
        if took > EXPORT_BOUND {
            return Err(format!(
                "export latency: export {} of {EXPORTS} took {took:?} (bound {EXPORT_BOUND:?})",
                i + 1
            ));
        }
    }
    println!("export latency: {EXPORTS} exports, worst {worst:?}");
    Ok(())
}

fn main() {
    let found = match std::env::args().nth(1) {
        Some(p) => Gpu::open(&p).map(|g| (p, g)).map_err(|e| e.to_string()),
        None => find_node().ok_or_else(|| "no render node has the limina extension".to_string()),
    };
    let (path, mut g) = match found {
        Ok(f) => f,
        Err(e) => {
            eprintln!("limina-gpu-smoke: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "{path}: context up, arena 1 at gpu 0x{:x} ({} MiB)",
        g.main.gpu_base,
        g.main.size / MIB
    );
    let other = match Arena::create(&g.file, 2, MIB) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("limina-gpu-smoke: second arena: {e}");
            std::process::exit(2);
        }
    };
    let result = triangle(&mut g)
        .and_then(|()| poke(&mut g, &other))
        .and_then(|()| export_latency(&g.file))
        .and_then(|()| share(&path, &g.file))
        .and_then(|()| fence(&mut g))
        .and_then(|()| queue_fences(&mut g))
        .and_then(|()| orphan_fence(&path))
        .and_then(|()| surface(&path, &mut g))
        .and_then(|()| round_trips(&mut g));
    match result {
        Ok(()) => println!("PASS"),
        Err(e) => {
            println!("FAIL: {e}");
            std::process::exit(1);
        }
    }
}
