//! The worker's side: start a replayer, and drive it on the guest's behalf.

use std::ffi::CString;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use crate::ipc::{self, Pod};
use crate::mach::{self, Mapping, Message, Port, ReceivePort, Semaphore};

/// How long a control request may take. Arena creation is the slow one (a heap allocation).
const CALL_TIMEOUT_MS: u32 = 10_000;
/// How long the replayer may take to say hello.
const HELLO_TIMEOUT_MS: u32 = 10_000;

/// Serialises spawns: the registered-port slot is per task.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// A running replayer process.
pub struct Replayer {
    pid: libc::pid_t,
    service: Port,
    /// One reply port, so calls are serialised.
    calls: Mutex<ReceivePort>,
}

/// A context's doorbell: signalling it wakes the context's ring consumer.
#[derive(Debug)]
pub struct Doorbell(Semaphore);

impl Doorbell {
    pub fn ring(&self) {
        self.0.signal();
    }
}

/// An arena as the worker sees it: the replayer's pages, mapped here.
#[derive(Debug)]
pub struct MappedArena {
    pub mapping: Mapping,
    pub gpu_base: u64,
}

fn status_err(what: &str, status: i32) -> io::Error {
    io::Error::other(format!("replayer refused {what}: status {status}"))
}

impl Replayer {
    /// Starts `exe` (the `limina-replayer` binary) and waits for it to say hello.
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
                    "replayer did not start",
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
            calls: Mutex::new(ReceivePort::new()?),
        })
    }

    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    fn call(&self, id: i32, data: &[u8]) -> io::Result<Message> {
        self.call_with(id, data, &[])
    }

    fn call_with(&self, id: i32, data: &[u8], ports: &[(u32, u8)]) -> io::Result<Message> {
        let reply_port = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        mach::call(
            &self.service,
            &reply_port,
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

impl Drop for Replayer {
    fn drop(&mut self) {
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
        .map_err(|_| io::Error::other("replayer path has a NUL"))?;
    let argv = [path.as_ptr(), std::ptr::null()];
    // The replayer inherits our environment: RUST_LOG, and the Metal debug knobs.
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
