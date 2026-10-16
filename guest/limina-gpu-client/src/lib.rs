//! The guest end of limina's Metal-level GPU transport, over the limina DRM ioctls of
//! guest/virtio-gpu-limina: a context, its doorbell, arenas mapped into this process, and a
//! command ring in the first arena. Shared by the transport's guest programs.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use limina_gpu_wire::cmd::{self, BufRef};
use limina_gpu_wire::ring::Producer;

/// Bytes of the command ring's data region.
pub const RING_DATA: u32 = 1 << 18;
pub const MIB: u64 = 1 << 20;

/// The UAPI of guest/virtio-gpu-limina/virtgpu_limina_drm.h.
pub mod uapi {
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

pub fn ioctl<T>(f: &File, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: `arg` is the struct `req` encodes, alive for the call.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg as *mut T) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn map(f: &File, offset: u64, len: usize) -> io::Result<NonNull<u8>> {
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

pub fn map_gem(f: &File, handle: u32, size: u64) -> io::Result<NonNull<u8>> {
    let mut m = uapi::Map {
        handle,
        ..Default::default()
    };
    ioctl(f, uapi::VIRTGPU_MAP, &mut m)?;
    map(f, m.offset, size as usize)
}

pub struct Arena {
    pub ptr: NonNull<u8>,
    pub size: u64,
    pub gpu_base: u64,
}

impl Arena {
    pub fn create(f: &File, id: u32, size: u64) -> io::Result<Self> {
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
    pub fn create_shared(f: &File, id: u32, size: u64) -> io::Result<(Self, u32)> {
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
    pub fn import(f: &File, id: u32, handle: u32) -> io::Result<Self> {
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

    pub fn write(&self, offset: u64, data: &[u8]) {
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

    pub fn read(&self, offset: u64, len: usize) -> &[u8] {
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

pub struct Gpu {
    pub file: File,
    pub doorbell: NonNull<u32>,
    pub doorbell_len: usize,
    pub ring: Producer,
    pub main: Arena,
    pub seqno: u64,
    pub kicks: u64,
}

impl Gpu {
    pub fn open(path: &str) -> io::Result<Self> {
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
            file,
            doorbell,
            doorbell_len: page,
            ring,
            main,
            seqno: 0,
            kicks: 0,
        })
    }

    pub fn push<T: cmd::Command>(&mut self, c: T, extra: &[&[u8]]) {
        self.ring.push(&c, extra).expect("ring space");
    }

    /// An argument table `handle` whose binding 0 is `address`.
    pub fn table(&mut self, handle: u32, address: u64) {
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
    pub fn commit_nowait(&mut self) -> u64 {
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
    pub fn kick(&mut self) {
        if self.ring.publish() {
            self.kicks += 1;
            // SAFETY: the doorbell page is mapped for the life of `Gpu`; the store is the kick.
            unsafe { self.doorbell.as_ptr().write_volatile(1) };
        }
    }

    /// A cleared reply slot at `offset` in the main arena, to name in a command.
    pub fn reply_slot(&mut self, offset: u64) -> BufRef {
        self.main.write(offset, &[0; size_of::<cmd::Reply>()]);
        BufRef::new(1, offset)
    }

    /// Publishes what was pushed and waits for the host to answer in the reply at `offset`.
    pub fn reply(&mut self, offset: u64) -> Result<cmd::Reply, String> {
        self.kick();
        assert!(offset + size_of::<cmd::Reply>() as u64 <= self.main.size);
        // SAFETY: inside the main arena's mapping, which lives as long as `self`.
        let p = unsafe { self.main.ptr.as_ptr().add(offset as usize) }.cast::<cmd::Reply>();
        // SAFETY: `done` is an aligned u32 the host stores to last, with release ordering.
        let done = unsafe { &*(&raw const (*p).done).cast::<AtomicU32>() };
        let start = Instant::now();
        while done.load(Ordering::Acquire) != cmd::REPLY_DONE {
            if let Some((e, at)) = self.ring.error() {
                return Err(format!("context lost: error {e} at ring position {at}"));
            }
            if start.elapsed() > Duration::from_secs(10) {
                return Err(format!("no reply at {offset:#x} after 10 s"));
            }
            std::hint::spin_loop();
        }
        // SAFETY: the host wrote the reply before `done`.
        Ok(unsafe { p.read() })
    }

    pub fn commit_and_wait(&mut self) -> Result<Duration, String> {
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

pub fn find_node() -> Option<(String, Gpu)> {
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
