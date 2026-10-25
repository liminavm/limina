//! The worker's side: start a GPU host, and drive it on the guest's behalf.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::ipc::{self, Pod};
use crate::mach::{self, Mapping, Message, Port, ReceivePort, Semaphore};

/// How long a control request may take. Arena creation is the slow one (a heap allocation).
const CALL_TIMEOUT_MS: u32 = 10_000;
/// How long the GPU host may take to say hello.
const HELLO_TIMEOUT_MS: u32 = 10_000;

/// Serialises spawns: the registered-port slot is per task.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// A running GPU host process.
pub struct GpuHost {
    pid: libc::pid_t,
    service: Port,
    /// Serialises calls: the GPU host answers one at a time anyway.
    calls: Mutex<()>,
    /// Set as [`Drop`] kills the process, so [`Self::on_exit`] stays quiet about an exit it asked for.
    dropping: Arc<AtomicBool>,
}

/// A context's doorbell: signalling it wakes the context's ring consumer.
#[derive(Debug)]
pub struct Doorbell(Semaphore);

impl Doorbell {
    pub fn ring(&self) {
        self.0.signal();
    }
}

/// An arena as the worker sees it: the GPU host's pages, mapped here.
#[derive(Debug)]
pub struct MappedArena {
    pub mapping: Mapping,
    pub gpu_base: u64,
}

fn status_err(what: &str, status: i32) -> io::Error {
    io::Error::other(format!("GPU host refused {what}: status {status}"))
}

impl GpuHost {
    /// Starts `exe` (the `limina-gpu-host` binary) and waits for it to say hello.
    pub fn spawn(exe: &Path) -> io::Result<Self> {
        let bootstrap = ReceivePort::new()?;
        let send = bootstrap.make_send()?;
        let pid = {
            let _g = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            mach::register_for_children(&[send.name()])?;
            let r = spawn_process(exe);
            mach::register_for_children(&[])?;
            r?
        };
        drop(send);
        let hello = match bootstrap.receive(Some(HELLO_TIMEOUT_MS)) {
            Ok(Some(m)) if m.id == ipc::HELLO && m.ports.len() == 1 => m,
            Ok(Some(m)) => {
                kill(pid);
                return Err(io::Error::other(format!("unexpected hello {:#x}", m.id)));
            }
            Ok(None) => {
                kill(pid);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "GPU host did not start",
                ));
            }
            Err(e) => {
                kill(pid);
                return Err(e);
            }
        };
        let service = hello.ports.into_iter().next().unwrap();
        Ok(Self {
            pid,
            service,
            calls: Mutex::new(()),
            dropping: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Calls `f` once, on a thread of its own, when the GPU host process exits of its own accord:
    /// every context it served is lost then, and nothing else will say so. The kill in [`Drop`]
    /// is not reported. The process is not reaped here, so that kill can never reach a recycled
    /// pid.
    pub fn on_exit(&self, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
        // SAFETY: plain syscalls on a descriptor this owns.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `kqueue` returned a descriptor nothing else owns.
        let kq = unsafe { std::os::fd::OwnedFd::from_raw_fd(kq) };
        let change = libc::kevent {
            ident: self.pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one change, no events asked for.
        let rc = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        // A process that has already exited cannot be watched; it is reported at once.
        if rc < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(io::Error::last_os_error());
        }
        let exited = rc < 0;
        let dropping = self.dropping.clone();
        std::thread::Builder::new()
            .name("gpu-host-exit".into())
            .spawn(move || {
                if !exited {
                    // SAFETY: an all-zero kevent is a valid buffer for one event.
                    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
                    loop {
                        // SAFETY: waits for one event into `event`, with no timeout.
                        let n = unsafe {
                            libc::kevent(
                                kq.as_raw_fd(),
                                std::ptr::null(),
                                0,
                                &mut event,
                                1,
                                std::ptr::null(),
                            )
                        };
                        if n > 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                        {
                            break;
                        }
                    }
                }
                if !dropping.load(Ordering::Acquire) {
                    f();
                }
            })?;
        Ok(())
    }

    fn call(&self, id: i32, data: &[u8]) -> io::Result<Message> {
        self.call_with(id, data, &[])
    }

    fn call_with(&self, id: i32, data: &[u8], ports: &[(u32, u8)]) -> io::Result<Message> {
        let _serial = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        mach::call(
            &self.service,
            &Message::new(id, data),
            ports,
            CALL_TIMEOUT_MS,
        )
    }

    fn simple(&self, what: &str, id: i32, data: &[u8]) -> io::Result<()> {
        let r = self.call(id, data)?;
        let s = ipc::Status::read(&r.data).ok_or_else(|| io::Error::other("short reply"))?;
        if s.status != ipc::OK {
            return Err(status_err(what, s.status));
        }
        Ok(())
    }

    pub fn create_context(&self, ctx: u32) -> io::Result<Doorbell> {
        let r = self.call(ipc::CREATE_CONTEXT, ipc::CtxRequest { ctx }.bytes())?;
        let s = ipc::Status::read(&r.data).ok_or_else(|| io::Error::other("short reply"))?;
        if s.status != ipc::OK {
            return Err(status_err("context", s.status));
        }
        let port = r
            .ports
            .into_iter()
            .next()
            .ok_or_else(|| io::Error::other("no doorbell in reply"))?;
        Ok(Doorbell(Semaphore::from_port(port)))
    }

    pub fn destroy_context(&self, ctx: u32) -> io::Result<()> {
        self.simple(
            "context destroy",
            ipc::DESTROY_CONTEXT,
            ipc::CtxRequest { ctx }.bytes(),
        )
    }

    /// Creates an arena of at least `size` bytes and maps it here.
    pub fn create_arena(&self, ctx: u32, arena: u32, size: u64) -> io::Result<MappedArena> {
        let r = self.call(
            ipc::CREATE_ARENA,
            ipc::CreateArena { ctx, arena, size }.bytes(),
        )?;
        self.mapped_arena(r)
    }

    fn mapped_arena(&self, r: Message) -> io::Result<MappedArena> {
        let a = ipc::ArenaReply::read(&r.data).ok_or_else(|| io::Error::other("short reply"))?;
        if a.status != ipc::OK {
            return Err(status_err("arena", a.status));
        }
        let entry = r
            .ports
            .into_iter()
            .next()
            .ok_or_else(|| io::Error::other("no memory entry in reply"))?;
        let mapping = Mapping::map(&entry, a.size)?;
        Ok(MappedArena {
            mapping,
            gpu_base: a.gpu_base,
        })
    }

    /// Context `ctx` as it travels across a snapshot (an encoded
    /// [`ContextExport`](crate::journal::ContextExport)).
    pub fn export_context(&self, ctx: u32) -> io::Result<Vec<u8>> {
        let r = self.call(ipc::EXPORT_CONTEXT, ipc::CtxRequest { ctx }.bytes())?;
        let a = ipc::ExportReply::read(&r.data).ok_or_else(|| io::Error::other("short reply"))?;
        if a.status != ipc::OK {
            return Err(status_err("export", a.status));
        }
        let entry = r
            .ports
            .into_iter()
            .next()
            .ok_or_else(|| io::Error::other("no memory entry in reply"))?;
        // SAFETY: `vm_page_size` is set by the kernel before any code runs.
        let page = unsafe { libc::vm_page_size } as u64;
        let mapping = Mapping::map(&entry, a.len.max(1).next_multiple_of(page))?;
        // SAFETY: the mapping holds at least `len` bytes.
        Ok(unsafe { std::slice::from_raw_parts(mapping.as_ptr(), a.len as usize) }.to_vec())
    }

    /// Re-creates arena `arena` at `gpu_base`, the address a restored guest holds, and maps it
    /// here. Arenas go in address order; [`Self::restore_arenas_done`] follows the last.
    pub fn restore_arena(
        &self,
        ctx: u32,
        arena: u32,
        size: u64,
        gpu_base: u64,
    ) -> io::Result<MappedArena> {
        let r = self.call(
            ipc::RESTORE_ARENA,
            ipc::RestoreArena {
                ctx,
                arena,
                size,
                gpu_base,
            }
            .bytes(),
        )?;
        self.mapped_arena(r)
    }

    /// Starts context `ctx`'s ring where `export` (an encoded
    /// [`ContextExport`](crate::journal::ContextExport)) left it, with its objects rebuilt.
    /// Its arenas, and any surfaces it uses, must be in place.
    pub fn restore_context(
        &self,
        ctx: u32,
        arena: u32,
        offset: u64,
        data_size: u32,
        export: &[u8],
    ) -> io::Result<()> {
        let blob = mach::Anonymous::new(export.len() as u64)?;
        // SAFETY: the allocation is at least `export.len()` long and ours alone.
        unsafe { std::ptr::copy_nonoverlapping(export.as_ptr(), blob.as_ptr(), export.len()) };
        let (entry, _) = mach::make_memory_entry(blob.addr(), blob.size())?;
        let req = ipc::RestoreContext {
            ring: ipc::AttachRing {
                ctx,
                arena,
                offset,
                data_size,
                _pad: 0,
            },
            len: export.len() as u64,
        };
        let r = self.call_with(
            ipc::RESTORE_CONTEXT,
            req.bytes(),
            &[(entry.into_name(), mach::MSG_TYPE_MOVE_SEND)],
        )?;
        let s = ipc::Status::read(&r.data).ok_or_else(|| io::Error::other("short reply"))?;
        if s.status != ipc::OK {
            return Err(status_err("context restore", s.status));
        }
        Ok(())
    }

    /// Every arena and context is restored: the GPU host lets go of what held their places.
    pub fn restore_done(&self) -> io::Result<()> {
        self.simple("restore done", ipc::RESTORE_DONE, &[])
    }

    /// What the GPU host holds and what it costs.
    pub fn stats(&self) -> io::Result<ipc::Stats> {
        let r = self.call(ipc::STATS, &[])?;
        ipc::Stats::read(&r.data).ok_or_else(|| io::Error::other("short reply"))
    }

    /// The contexts holding the most objects, most first, and how many contexts there are.
    pub fn context_stats(&self) -> io::Result<(Vec<ipc::ContextStats>, u32)> {
        let r = self.call(ipc::CONTEXT_STATS, &[])?;
        let a =
            ipc::ContextStatsReply::read(&r.data).ok_or_else(|| io::Error::other("short reply"))?;
        let n = (a.count as usize).min(ipc::MAX_CONTEXT_STATS);
        Ok((a.contexts[..n].to_vec(), a.total))
    }

    pub fn destroy_arena(&self, ctx: u32, arena: u32) -> io::Result<()> {
        self.simple(
            "arena destroy",
            ipc::DESTROY_ARENA,
            ipc::ArenaRequest { ctx, arena }.bytes(),
        )
    }

    /// Makes `arena` of `ctx` importable as `export`, by any context, until released.
    pub fn export_arena(&self, ctx: u32, arena: u32, export: u32) -> io::Result<()> {
        self.simple(
            "arena export",
            ipc::EXPORT_ARENA,
            ipc::ExportArena { ctx, arena, export }.bytes(),
        )
    }

    /// Makes `arena` of `ctx` the memory of `export`; answers its size and GPU address.
    pub fn import_arena(&self, ctx: u32, arena: u32, export: u32) -> io::Result<(u64, u64)> {
        let r = self.call(
            ipc::IMPORT_ARENA,
            ipc::ImportArena { ctx, arena, export }.bytes(),
        )?;
        let a = ipc::ArenaReply::read(&r.data).ok_or_else(|| io::Error::other("short reply"))?;
        if a.status != ipc::OK {
            return Err(status_err("arena import", a.status));
        }
        Ok((a.size, a.gpu_base))
    }

    /// Files the IOSurface `port` names for `ctx` under `surface`, for the context to make
    /// textures over. The send right travels with the request.
    pub fn bind_surface(&self, ctx: u32, surface: u32, port: u32) -> io::Result<()> {
        let r = self.call_with(
            ipc::BIND_SURFACE,
            ipc::SurfaceRequest { ctx, surface }.bytes(),
            &[(port, mach::MSG_TYPE_MOVE_SEND)],
        )?;
        let s = ipc::Status::read(&r.data).ok_or_else(|| io::Error::other("short reply"))?;
        if s.status != ipc::OK {
            return Err(status_err("surface bind", s.status));
        }
        Ok(())
    }

    pub fn release_surface(&self, ctx: u32, surface: u32) -> io::Result<()> {
        self.simple(
            "surface release",
            ipc::RELEASE_SURFACE,
            ipc::SurfaceRequest { ctx, surface }.bytes(),
        )
    }

    pub fn release_export(&self, export: u32) -> io::Result<()> {
        self.simple(
            "export release",
            ipc::RELEASE_EXPORT,
            ipc::ExportRequest { export }.bytes(),
        )
    }

    /// Starts consuming the ring the guest initialised at `offset` in `arena`.
    pub fn attach_ring(&self, ctx: u32, arena: u32, offset: u64, data_size: u32) -> io::Result<()> {
        self.simple(
            "ring",
            ipc::ATTACH_RING,
            ipc::AttachRing {
                ctx,
                arena,
                offset,
                data_size,
                _pad: 0,
            }
            .bytes(),
        )
    }
}

impl Drop for GpuHost {
    fn drop(&mut self) {
        self.dropping.store(true, Ordering::Release);
        kill(self.pid);
    }
}

fn kill(pid: libc::pid_t) {
    // SAFETY: our own child; reaping it keeps it from lingering as a zombie.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
    }
}

fn spawn_process(exe: &Path) -> io::Result<libc::pid_t> {
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(exe.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("GPU host path has a NUL"))?;
    let argv = [path.as_ptr(), std::ptr::null()];
    // The GPU host inherits our environment: RUST_LOG, and the Metal debug knobs.
    unsafe extern "C" {
        static environ: *const *const libc::c_char;
    }
    let mut pid = 0;
    // SAFETY: valid NUL-terminated argv and environ; no file actions or attributes.
    let rc = unsafe {
        libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            argv.as_ptr() as *const *mut libc::c_char,
            environ as *const *mut libc::c_char,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(pid)
}
