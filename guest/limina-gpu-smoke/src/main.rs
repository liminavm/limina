//! First light for limina's Metal-level GPU transport, from inside a guest.
//!
//! Drives the virtio-gpu extension through the limina DRM ioctls (guest/virtio-gpu-limina): one
//! context, its doorbell page, two arenas, a command ring in the first. Then it draws a triangle
//! and has a kernel write through a GPU address it computed itself, and checks both results in
//! arena memory. Last, it shares an arena with another process the way a dma-buf travels: export
//! as a GEM object, PRIME to an fd, import in a child with its own context, and a kernel there
//! writes what the parent reads back. Then a fence: a commit that waits on a shared event
//! nobody has set, fenced as a sync_file and on a shared arena's dma-buf, must leave both
//! unsignaled until the guest sets the event, and both signaled after. Exit status 0 means
//! everything came back right.
//!
//! Usage: limina-gpu-smoke [/dev/dri/renderDN]   (default: the first render node that has the
//! extension)

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use limina_gpu_wire::cmd::{self, BufRef};
use limina_gpu_wire::ring::Producer;

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

/// The UAPI of guest/virtio-gpu-limina/virtgpu_limina_drm.h.
mod uapi {
    #[repr(C)]
    #[derive(Default)]
    pub struct CtxCreate {
        pub flags: u32,
        pub pad: u32,
        pub doorbell_offset: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct ArenaCreate {
        pub arena_id: u32,
        pub flags: u32,
        pub size: u64,
        pub gpu_base: u64,
        pub mmap_offset: u64,
    }

    pub const ARENA_SHARED: u32 = 1;
    pub const FENCE_FD: u32 = 1;

    #[repr(C)]
    #[derive(Default)]
    pub struct Fence {
        pub seqno: u64,
        pub handles: u64,
        pub num_handles: u32,
        pub flags: u32,
        pub fence_fd: i32,
        pub pad: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct ArenaExport {
        pub arena_id: u32,
        pub handle: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct ArenaImport {
        pub arena_id: u32,
        pub handle: u32,
        pub size: u64,
        pub gpu_base: u64,
    }

    /// virtgpu_drm.h's drm_virtgpu_map.
    #[repr(C)]
    #[derive(Default)]
    pub struct Map {
        pub offset: u64,
        pub handle: u32,
        pub pad: u32,
    }

    /// drm.h's drm_prime_handle.
    #[repr(C)]
    #[derive(Default)]
    pub struct PrimeHandle {
        pub handle: u32,
        pub flags: u32,
        pub fd: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct SurfaceCreate {
        pub width: u32,
        pub height: u32,
        pub drm_fourcc: u32,
        pub stride: u32,
        pub handle: u32,
        pub res_handle: u32,
    }

    /// virtgpu_drm.h's drm_virtgpu_resource_info.
    #[repr(C)]
    #[derive(Default)]
    pub struct ResourceInfo {
        pub bo_handle: u32,
        pub res_handle: u32,
        pub size: u32,
        pub blob_mem: u32,
    }

    /// drm.h's drm_gem_close.
    #[repr(C)]
    #[derive(Default)]
    pub struct GemClose {
        pub handle: u32,
        pub pad: u32,
    }

    pub const BLOB_MEM_HOST3D: u32 = 2;

    #[repr(C)]
    #[derive(Default)]
    pub struct RingAttach {
        pub arena_id: u32,
        pub data_size: u32,
        pub offset: u64,
    }

    const fn iowr(nr: u32, size: usize) -> libc::c_ulong {
        // _IOC(_IOC_READ|_IOC_WRITE, 'd', DRM_COMMAND_BASE + nr, size)
        core_iowr(0x40 + nr, size)
    }
    const fn core_iowr(nr: u32, size: usize) -> libc::c_ulong {
        ((3u32 << 30) | ((size as u32) << 16) | ((b'd' as u32) << 8) | nr) as libc::c_ulong
    }
    const fn iow_core(nr: u32, size: usize) -> libc::c_ulong {
        ((1u32 << 30) | ((size as u32) << 16) | ((b'd' as u32) << 8) | nr) as libc::c_ulong
    }
    const fn iow(nr: u32, size: usize) -> libc::c_ulong {
        ((1u32 << 30) | ((size as u32) << 16) | ((b'd' as u32) << 8) | (0x40 + nr)) as libc::c_ulong
    }

    pub const CTX_CREATE: libc::c_ulong = iowr(0x20, size_of::<CtxCreate>());
    pub const ARENA_CREATE: libc::c_ulong = iowr(0x21, size_of::<ArenaCreate>());
    pub const RING_ATTACH: libc::c_ulong = iow(0x23, size_of::<RingAttach>());
    pub const ARENA_EXPORT: libc::c_ulong = iowr(0x24, size_of::<ArenaExport>());
    pub const ARENA_IMPORT: libc::c_ulong = iowr(0x25, size_of::<ArenaImport>());
    pub const FENCE: libc::c_ulong = iowr(0x26, size_of::<Fence>());
    pub const SURFACE_CREATE: libc::c_ulong = iowr(0x27, size_of::<SurfaceCreate>());
    pub const RESOURCE_INFO: libc::c_ulong = iowr(0x05, size_of::<ResourceInfo>());
    pub const GEM_CLOSE: libc::c_ulong = iow_core(0x09, size_of::<GemClose>());
    pub const VIRTGPU_MAP: libc::c_ulong = iowr(0x01, size_of::<Map>());
    pub const PRIME_HANDLE_TO_FD: libc::c_ulong = core_iowr(0x2d, size_of::<PrimeHandle>());
    pub const PRIME_FD_TO_HANDLE: libc::c_ulong = core_iowr(0x2e, size_of::<PrimeHandle>());
    pub const DRM_CLOEXEC: u32 = libc::O_CLOEXEC as u32;
    pub const DRM_RDWR: u32 = libc::O_RDWR as u32;
}

fn ioctl<T>(f: &File, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: `arg` is the struct `req` encodes, alive for the call.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg as *mut T) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn map(f: &File, offset: u64, len: usize) -> io::Result<NonNull<u8>> {
    // SAFETY: a fresh shared mapping of the device file; nothing else is at the address.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            offset as libc::off_t,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(NonNull::new(p.cast()).expect("mmap returned null"))
}

fn map_gem(f: &File, handle: u32, size: u64) -> io::Result<NonNull<u8>> {
    let mut m = uapi::Map {
        handle,
        ..Default::default()
    };
    ioctl(f, uapi::VIRTGPU_MAP, &mut m)?;
    map(f, m.offset, size as usize)
}

struct Arena {
    ptr: NonNull<u8>,
    size: u64,
    gpu_base: u64,
}

impl Arena {
    fn create(f: &File, id: u32, size: u64) -> io::Result<Self> {
        let mut a = uapi::ArenaCreate {
            arena_id: id,
            size,
            ..Default::default()
        };
        ioctl(f, uapi::ARENA_CREATE, &mut a)?;
        let ptr = map(f, a.mmap_offset, a.size as usize)?;
        Ok(Self {
            ptr,
            size: a.size,
            gpu_base: a.gpu_base,
        })
    }

    /// A shared arena, exported as a GEM object and mapped through it; returns the GEM handle too.
    fn create_shared(f: &File, id: u32, size: u64) -> io::Result<(Self, u32)> {
        let mut a = uapi::ArenaCreate {
            arena_id: id,
            flags: uapi::ARENA_SHARED,
            size,
            ..Default::default()
        };
        ioctl(f, uapi::ARENA_CREATE, &mut a)?;
        let mut e = uapi::ArenaExport {
            arena_id: id,
            handle: 0,
        };
        ioctl(f, uapi::ARENA_EXPORT, &mut e)?;
        let ptr = map_gem(f, e.handle, a.size)?;
        Ok((
            Self {
                ptr,
                size: a.size,
                gpu_base: a.gpu_base,
            },
            e.handle,
        ))
    }

    /// Arena `id` made the memory of the exported GEM object `handle`, mapped through it.
    fn import(f: &File, id: u32, handle: u32) -> io::Result<Self> {
        let mut i = uapi::ArenaImport {
            arena_id: id,
            handle,
            ..Default::default()
        };
        ioctl(f, uapi::ARENA_IMPORT, &mut i)?;
        let ptr = map_gem(f, handle, i.size)?;
        Ok(Self {
            ptr,
            size: i.size,
            gpu_base: i.gpu_base,
        })
    }

    fn write(&self, offset: u64, data: &[u8]) {
        assert!(offset + data.len() as u64 <= self.size);
        // SAFETY: inside the mapping, checked above.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.ptr.as_ptr().add(offset as usize),
                data.len(),
            )
        }
    }

    fn read(&self, offset: u64, len: usize) -> &[u8] {
        assert!(offset + len as u64 <= self.size);
        // SAFETY: inside the mapping, checked above; read only after the GPU finished.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(offset as usize), len) }
    }
}

/// A mapping keeps the device file open; unmapping is what lets closing it end the context.
impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: the mapping `create`/`import` made, unused once the arena is dropped.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.size as usize) };
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: the doorbell mapping `open` made; nothing stores to it after this.
        unsafe { libc::munmap(self.doorbell.as_ptr().cast(), self.doorbell_len) };
    }
}

struct Gpu {
    _file: File,
    doorbell: NonNull<u32>,
    doorbell_len: usize,
    ring: Producer,
    main: Arena,
    seqno: u64,
    kicks: u64,
}

impl Gpu {
    fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut c = uapi::CtxCreate::default();
        ioctl(&file, uapi::CTX_CREATE, &mut c)?;
        let page = 4096.max(unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize);
        let doorbell = map(&file, c.doorbell_offset, page)?.cast();
        let main = Arena::create(&file, 1, 64 * MIB)?;
        // SAFETY: the arena mapping lives as long as `Gpu`; offset 0 is page-aligned and the ring
        // region is far smaller than the arena.
        let ring = unsafe { Producer::init(main.ptr, RING_DATA) };
        let mut at = uapi::RingAttach {
            arena_id: 1,
            data_size: RING_DATA,
            offset: 0,
        };
        ioctl(&file, uapi::RING_ATTACH, &mut at)?;
        Ok(Self {
            _file: file,
            doorbell,
            doorbell_len: page,
            ring,
            main,
            seqno: 0,
            kicks: 0,
        })
    }

    fn push<T: cmd::Command>(&mut self, c: T, extra: &[&[u8]]) {
        self.ring.push(&c, extra).expect("ring space");
    }

    /// An argument table `handle` whose binding 0 is `address`.
    fn table(&mut self, handle: u32, address: u64) {
        self.push(
            cmd::CreateArgumentTable {
                handle,
                max_buffer_bind_count: 1,
            },
            &[],
        );
        let b = cmd::AddressBinding {
            index: 0,
            _pad: 0,
            address,
        };
        self.push(
            cmd::SetAddresses {
                table: handle,
                count: 1,
            },
            &[cmd::bytes_of(&b)],
        );
    }

    /// Commits what was pushed without waiting for it; returns its seqno.
    fn commit_nowait(&mut self) -> u64 {
        self.seqno += 1;
        self.push(
            cmd::Commit {
                seqno: self.seqno,
                feedback: BufRef::NONE,
            },
            &[],
        );
        self.kick();
        self.seqno
    }

    /// Publishes what was pushed, ringing the doorbell if the host parked.
    fn kick(&mut self) {
        if self.ring.publish() {
            self.kicks += 1;
            // SAFETY: the doorbell page is mapped for the life of `Gpu`; the store is the kick.
            unsafe { self.doorbell.as_ptr().write_volatile(1) };
        }
    }

    fn commit_and_wait(&mut self) -> Result<Duration, String> {
        self.seqno += 1;
        self.push(
            cmd::Commit {
                seqno: self.seqno,
                feedback: BufRef::NONE,
            },
            &[],
        );
        let start = Instant::now();
        if self.ring.publish() {
            self.kicks += 1;
            // SAFETY: the doorbell page is mapped for the life of `Gpu`; the store is the kick.
            unsafe { self.doorbell.as_ptr().write_volatile(1) };
        }
        while self.ring.completed() < self.seqno {
            if let Some((e, at)) = self.ring.error() {
                return Err(format!("context lost: error {e} at ring position {at}"));
            }
            if start.elapsed() > Duration::from_secs(10) {
                return Err(format!(
                    "seqno {} not complete after 10 s (completed {})",
                    self.seqno,
                    self.ring.completed()
                ));
            }
            std::hint::spin_loop();
        }
        Ok(start.elapsed())
    }
}

fn find_node() -> Option<(String, Gpu)> {
    (128..136).find_map(|n| {
        let path = format!("/dev/dri/renderD{n}");
        match Gpu::open(&path) {
            Ok(g) => Some((path, g)),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    eprintln!("{path}: {e}");
                }
                None
            }
        }
    })
}

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
            ioctl(&g._file, uapi::PRIME_FD_TO_HANDLE, &mut p)
                .map_err(|e| format!("PRIME import: {e}"))?;
            let other =
                Arena::import(&g._file, 5, p.handle).map_err(|e| format!("arena import: {e}"))?;
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
    let xr24 = u32::from_le_bytes(*b"XR24");
    let f = &g._file;
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
        drm_fourcc: xr24,
        stride: STRIDE,
        ..Default::default()
    };
    ioctl(f, uapi::SURFACE_CREATE, &mut s).map_err(|e| format!("SURFACE_CREATE: {e}"))?;

    g.push(
        cmd::CreateTextureFromSurface {
            handle: 40,
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
        texture: 40,
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
            texture: 40,
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
    ioctl(&g._file, uapi::PRIME_HANDLE_TO_FD, &mut p).map_err(|e| format!("PRIME export: {e}"))?;
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
    ioctl(&g._file, uapi::GEM_CLOSE, &mut close).map_err(|e| format!("GEM_CLOSE: {e}"))?;
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
    let f = &g._file;
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
        pad: 0,
    };
    ioctl(&g._file, uapi::FENCE, &mut fence).map_err(|e| format!("FENCE: {e}"))?;
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
    ioctl(&g._file, uapi::FENCE, &mut fence).map_err(|e| format!("FENCE: {e}"))?;
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
    let other = match Arena::create(&g._file, 2, MIB) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("limina-gpu-smoke: second arena: {e}");
            std::process::exit(2);
        }
    };
    let result = triangle(&mut g)
        .and_then(|()| poke(&mut g, &other))
        .and_then(|()| share(&path, &g._file))
        .and_then(|()| surface(&path, &mut g))
        .and_then(|()| fence(&mut g))
        .and_then(|()| orphan_fence(&path))
        .and_then(|()| round_trips(&mut g));
    match result {
        Ok(()) => println!("PASS"),
        Err(e) => {
            println!("FAIL: {e}");
            std::process::exit(1);
        }
    }
}
