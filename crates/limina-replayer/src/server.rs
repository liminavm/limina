//! The replayer process: answers the worker's control requests and runs one ring consumer thread
//! per context.

use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use limina_gpu_wire::ring::{self, Consumer};
use objc2::rc::Retained;
use objc2_metal::MTLDevice;

use crate::exec::{Arenas, Executor};
use crate::ipc::{self, Pod};
use crate::mach::{self, Message, ReceivePort, Semaphore};
use crate::metal::{self, Arena, Device};

struct RingThread {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

struct Context {
    doorbell: Arc<Semaphore>,
    arenas: Arenas,
    ring: Option<RingThread>,
}

impl Drop for Context {
    fn drop(&mut self) {
        if let Some(r) = self.ring.take() {
            r.stop.store(true, std::sync::atomic::Ordering::Release);
            self.doorbell.signal();
            let _ = r.join.join();
        }
    }
}

struct Server {
    device: Retained<Device>,
    contexts: HashMap<u32, Context>,
    /// Arenas other contexts may import, by export id (the guest resource that names them).
    /// An export holds its arena whether or not the context that made it is still there.
    exports: HashMap<u32, Arc<Arena>>,
}

/// The replayer's `main`. Never returns.
pub fn main() -> ! {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_micros()
        .init();
    exit_with_parent();
    if let Err(e) = serve() {
        log::error!("replayer: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn serve() -> std::io::Result<()> {
    let parent = mach::lookup_registered()?;
    let device = metal::system_device().ok_or_else(|| std::io::Error::other("no Metal device"))?;
    let service = ReceivePort::new()?;
    let send = service.make_send()?;
    mach::send(
        &parent,
        &Message::new(ipc::HELLO, &[]),
        &[(send.into_name(), mach::MSG_TYPE_MOVE_SEND)],
    )?;
    drop(parent);
    log::info!("replayer up on {}", device.name());
    let mut server = Server {
        device,
        contexts: HashMap::new(),
        exports: HashMap::new(),
    };
    loop {
        let Some(msg) = service.receive(None)? else {
            continue;
        };
        server.handle(msg);
    }
}

fn status(s: i32) -> Message {
    Message::new(0, ipc::Status { status: s }.bytes())
}

impl Server {
    fn handle(&mut self, msg: Message) {
        let Some(reply_to) = msg.reply else {
            log::warn!("request {:#x} without a reply port", msg.id);
            return;
        };
        let (mut reply, ports): (Message, Vec<(u32, u8)>) = match msg.id {
            ipc::CREATE_CONTEXT => match ipc::CtxRequest::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => match self.create_context(r.ctx) {
                    Ok(sem) => (status(ipc::OK), vec![(sem, mach::MSG_TYPE_COPY_SEND)]),
                    Err(s) => (status(s), vec![]),
                },
            },
            ipc::DESTROY_CONTEXT => match ipc::CtxRequest::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => {
                    let s = match self.contexts.remove(&r.ctx) {
                        // Tearing down waits for the context's GPU work; the worker's control
                        // queue waits on this answer, so the context goes on a thread of its own.
                        Some(ctx) => match std::thread::Builder::new()
                            .name(format!("teardown-{}", r.ctx))
                            .spawn(move || drop(ctx))
                        {
                            Ok(_) => ipc::OK,
                            Err(e) => {
                                log::warn!("context {}: torn down inline: {e}", r.ctx);
                                ipc::OK
                            }
                        },
                        None => ipc::ERR_NO_CONTEXT,
                    };
                    (status(s), vec![])
                }
            },
            ipc::CREATE_ARENA => match ipc::CreateArena::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => match self.create_arena(r) {
                    Ok((reply, entry)) => (
                        Message::new(0, reply.bytes()),
                        vec![(entry.into_name(), mach::MSG_TYPE_MOVE_SEND)],
                    ),
                    Err(s) => (
                        Message::new(
                            0,
                            ipc::ArenaReply {
                                status: s,
                                ..Default::default()
                            }
                            .bytes(),
                        ),
                        vec![],
                    ),
                },
            },
            ipc::DESTROY_ARENA => match ipc::ArenaRequest::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => (status(self.destroy_arena(r)), vec![]),
            },
            ipc::ATTACH_RING => match ipc::AttachRing::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => (status(self.attach_ring(r)), vec![]),
            },
            ipc::EXPORT_ARENA => match ipc::ExportArena::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => (status(self.export_arena(r)), vec![]),
            },
            ipc::IMPORT_ARENA => match ipc::ImportArena::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => {
                    let reply = self.import_arena(r).unwrap_or_else(|s| ipc::ArenaReply {
                        status: s,
                        ..Default::default()
                    });
                    (Message::new(0, reply.bytes()), vec![])
                }
            },
            ipc::RELEASE_EXPORT => match ipc::ExportRequest::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => {
                    let s = if self.exports.remove(&r.export).is_some() {
                        ipc::OK
                    } else {
                        ipc::ERR_NO_ARENA
                    };
                    (status(s), vec![])
                }
            },
            other => {
                log::warn!("unknown request {other:#x}");
                (status(ipc::ERR_MALFORMED), vec![])
            }
        };
        reply.id = msg.id + mach::REPLY_ID_OFFSET;
        if let Err(e) = mach::reply(reply_to, &reply, &ports) {
            log::warn!("reply to {:#x}: {e}", msg.id);
        }
    }

    fn create_context(&mut self, ctx: u32) -> Result<u32, i32> {
        if self.contexts.contains_key(&ctx) {
            return Err(ipc::ERR_EXISTS);
        }
        let doorbell = Arc::new(Semaphore::new().map_err(|_| ipc::ERR_INTERNAL)?);
        let name = doorbell.name();
        self.contexts.insert(
            ctx,
            Context {
                doorbell,
                arenas: Arc::new(Mutex::new(HashMap::new())),
                ring: None,
            },
        );
        Ok(name)
    }

    fn create_arena(&mut self, r: ipc::CreateArena) -> Result<(ipc::ArenaReply, mach::Port), i32> {
        let ctx = self.contexts.get(&r.ctx).ok_or(ipc::ERR_NO_CONTEXT)?;
        let mut arenas = ctx.arenas.lock().unwrap_or_else(|e| e.into_inner());
        if arenas.contains_key(&r.arena) {
            return Err(ipc::ERR_EXISTS);
        }
        let arena = Arena::new(&self.device, r.size).ok_or(ipc::ERR_NO_MEMORY)?;
        let (entry, len) =
            mach::make_memory_entry(arena.contents() as u64, arena.size).map_err(|e| {
                log::warn!("arena memory entry: {e}");
                ipc::ERR_INTERNAL
            })?;
        if len < arena.size {
            return Err(ipc::ERR_INTERNAL);
        }
        let reply = ipc::ArenaReply {
            status: ipc::OK,
            _pad: 0,
            size: arena.size,
            gpu_base: arena.gpu_base,
        };
        arenas.insert(r.arena, Arc::new(arena));
        Ok((reply, entry))
    }

    fn destroy_arena(&mut self, r: ipc::ArenaRequest) -> i32 {
        let Some(ctx) = self.contexts.get(&r.ctx) else {
            return ipc::ERR_NO_CONTEXT;
        };
        let removed = ctx
            .arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&r.arena);
        if removed.is_some() {
            ipc::OK
        } else {
            ipc::ERR_NO_ARENA
        }
    }

    fn export_arena(&mut self, r: ipc::ExportArena) -> i32 {
        let Some(ctx) = self.contexts.get(&r.ctx) else {
            return ipc::ERR_NO_CONTEXT;
        };
        if self.exports.contains_key(&r.export) {
            return ipc::ERR_EXISTS;
        }
        let Some(arena) = ctx
            .arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&r.arena)
            .cloned()
        else {
            return ipc::ERR_NO_ARENA;
        };
        self.exports.insert(r.export, arena);
        ipc::OK
    }

    fn import_arena(&mut self, r: ipc::ImportArena) -> Result<ipc::ArenaReply, i32> {
        let ctx = self.contexts.get(&r.ctx).ok_or(ipc::ERR_NO_CONTEXT)?;
        let arena = self
            .exports
            .get(&r.export)
            .ok_or(ipc::ERR_NO_ARENA)?
            .clone();
        let mut arenas = ctx.arenas.lock().unwrap_or_else(|e| e.into_inner());
        if arenas.contains_key(&r.arena) {
            return Err(ipc::ERR_EXISTS);
        }
        let reply = ipc::ArenaReply {
            status: ipc::OK,
            _pad: 0,
            size: arena.size,
            gpu_base: arena.gpu_base,
        };
        arenas.insert(r.arena, arena);
        Ok(reply)
    }

    fn attach_ring(&mut self, r: ipc::AttachRing) -> i32 {
        let Some(ctx) = self.contexts.get_mut(&r.ctx) else {
            return ipc::ERR_NO_CONTEXT;
        };
        if ctx.ring.is_some() {
            return ipc::ERR_EXISTS;
        }
        let Some(arena) = ctx
            .arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&r.arena)
            .cloned()
        else {
            return ipc::ERR_NO_ARENA;
        };
        if !ring::valid_data_size(r.data_size)
            || !r.offset.is_multiple_of(128)
            || !arena.contains(r.offset, ring::region_size(r.data_size) as u64)
        {
            return ipc::ERR_BAD_VALUE;
        }
        // SAFETY: inside the arena, which the executor keeps alive.
        let region = unsafe { NonNull::new_unchecked(arena.contents().add(r.offset as usize)) };
        // SAFETY: as above, for the region's whole size.
        let consumer = match unsafe { Consumer::attach(region, r.data_size) } {
            Ok(c) => c,
            Err(e) => {
                log::warn!("attach ring: {e:?}");
                return ipc::ERR_BAD_VALUE;
            }
        };
        let Some(exec) = Executor::new(self.device.clone(), consumer, arena, ctx.arenas.clone())
        else {
            return ipc::ERR_INTERNAL;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let doorbell = ctx.doorbell.clone();
        let stop2 = stop.clone();
        let join = std::thread::Builder::new()
            .name(format!("ring-{}", r.ctx))
            .spawn(move || exec.run(&doorbell, &stop2));
        match join {
            Ok(join) => {
                ctx.ring = Some(RingThread { stop, join });
                ipc::OK
            }
            Err(_) => ipc::ERR_INTERNAL,
        }
    }
}

/// Exits when the parent (the worker) does, so a replayer never outlives its VM.
fn exit_with_parent() {
    // SAFETY: plain syscalls on our own kqueue.
    unsafe {
        let ppid = libc::getppid();
        let kq = libc::kqueue();
        let ev = libc::kevent {
            ident: ppid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if kq < 0
            || ppid == 1
            || libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) != 0
        {
            libc::_exit(0);
        }
        std::thread::spawn(move || {
            let mut out: libc::kevent = std::mem::zeroed();
            libc::kevent(kq, std::ptr::null(), 0, &mut out, 1, std::ptr::null());
            libc::_exit(0);
        });
    }
}
