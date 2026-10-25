//! The GPU host process: answers the worker's control requests and runs one ring consumer thread
//! per context.

use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use limina_gpu_wire::ring::{self, Consumer};
use objc2::rc::{Retained, autoreleasepool};
use objc2_metal::MTLDevice;

use crate::exec::{Arenas, Executor, Mailbox, Placeholders, Surfaces};
use crate::ipc::{self, Pod};
use crate::journal::ContextExport;
use crate::mach::{self, Mapping, Message, ReceivePort, Semaphore};
use crate::metal::{self, Arena, Device};

struct RingThread {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

struct Context {
    doorbell: Arc<Semaphore>,
    arenas: Arenas,
    surfaces: Surfaces,
    mailbox: Arc<Mailbox>,
    ring: Option<RingThread>,
}

/// How long the server waits for a ring thread to answer an export: longer than its drain.
const EXPORT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

impl Drop for Context {
    fn drop(&mut self) {
        if let Some(r) = self.ring.take() {
            r.stop.store(true, std::sync::atomic::Ordering::Release);
            self.doorbell.signal();
            // A panic never returns here: the hook in `main` ends the process.
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
    /// Arenas holding the holes below restored arenas' addresses, until every arena stands.
    placeholders: Vec<Arena>,
    /// Objects holding resource IDs below restored objects', until every context stands.
    id_placeholders: Placeholders,
    /// The heap size and the non-arena footprint that next earn a warning.
    next_heap_warn: u64,
    next_other_warn: u64,
}

/// How often the GPU host logs what it holds, and checks it against the warnings below.
const STATS_PERIOD: Duration = Duration::from_secs(300);
/// The heap that earns the first warning, and every doubling after it. A healthy GPU host's heap
/// is tens of MiB; one that leaked a frame's encoder each frame passed this in about an hour.
const HEAP_WARN: u64 = 1 << 30;
/// The same for the footprint beyond the arenas (textures over surfaces, Metal's own pools): a
/// leak the heap's bound cannot see.
const OTHER_WARN: u64 = 8 << 30;

/// Metal places allocations on this boundary.
const GRANULE: u64 = 0x8000;
/// What an allocation takes beyond its size, before rounding up to [`GRANULE`]: a 16 KiB arena
/// takes 0x8000 of address space, a 1 MiB one 0x108000.
const GUARD: u64 = 0x4000;
/// Placeholders a single restored arena may need before giving up.
const MAX_FILLS: usize = 64;

/// The GPU host's `main`. Never returns.
pub fn main() -> ! {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_micros()
        .init();
    exit_with_parent();
    // A thread that unwound alone would leave whatever it served looking alive with nothing
    // behind it: a ring thread's context would never progress or fail, and its fences would wait
    // for good. The process ends instead, which the worker watches for and reports as every
    // context lost. (A panic in the commit-feedback block crosses `extern "C"` and aborts anyway.)
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));
    if std::env::var_os("LIMINA_GPU_HOST_TEST_PANIC").is_some() {
        // Off unless a test asks: a thread that panics once the server is up.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(1));
            panic!("a test asked for a panic")
        });
    }
    if let Err(e) = serve() {
        log::error!("GPU host: {e}");
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
    log::info!("GPU host up on {}", device.name());
    let mut server = Server {
        device,
        contexts: HashMap::new(),
        exports: HashMap::new(),
        placeholders: Vec::new(),
        id_placeholders: Placeholders::default(),
        next_heap_warn: HEAP_WARN,
        next_other_warn: OTHER_WARN,
    };
    let mut next_stats = Instant::now() + STATS_PERIOD;
    loop {
        let wait = next_stats.saturating_duration_since(Instant::now());
        if let Some(msg) = service.receive(Some(wait.as_millis().max(1) as u32))? {
            // Each request's autoreleased Metal objects, drained before the next (see
            // `Executor::run`).
            autoreleasepool(|_| server.handle(msg));
        }
        if Instant::now() >= next_stats {
            server.log_stats();
            next_stats = Instant::now() + STATS_PERIOD;
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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
            ipc::BIND_SURFACE => match (ipc::SurfaceRequest::read(&msg.data), msg.ports.first()) {
                (Some(r), Some(port)) => (status(self.bind_surface(r, port.name())), vec![]),
                _ => (status(ipc::ERR_MALFORMED), vec![]),
            },
            ipc::RELEASE_SURFACE => match ipc::SurfaceRequest::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => (status(self.release_surface(r)), vec![]),
            },
            ipc::RESTORE_ARENA => match ipc::RestoreArena::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => match self.restore_arena(r) {
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
            ipc::STATS => (Message::new(0, self.stats().bytes()), vec![]),
            ipc::CONTEXT_STATS => (Message::new(0, self.context_stats().bytes()), vec![]),
            ipc::RESTORE_DONE => {
                self.placeholders.clear();
                self.id_placeholders = Placeholders::default();
                (status(ipc::OK), vec![])
            }
            ipc::RESTORE_CONTEXT => match (ipc::RestoreContext::read(&msg.data), msg.ports.first())
            {
                (Some(r), Some(entry)) => (status(self.restore_context(r, entry)), vec![]),
                _ => (status(ipc::ERR_MALFORMED), vec![]),
            },
            ipc::EXPORT_CONTEXT => match ipc::CtxRequest::read(&msg.data) {
                None => (status(ipc::ERR_MALFORMED), vec![]),
                Some(r) => match self.export_context(r.ctx) {
                    Ok((reply, entry)) => (
                        Message::new(0, reply.bytes()),
                        vec![(entry.into_name(), mach::MSG_TYPE_MOVE_SEND)],
                    ),
                    Err(s) => (
                        Message::new(
                            0,
                            ipc::ExportReply {
                                status: s,
                                ..Default::default()
                            }
                            .bytes(),
                        ),
                        vec![],
                    ),
                },
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

    fn stats(&self) -> ipc::Stats {
        let mut st = ipc::Stats {
            footprint: footprint(),
            heap: heap_in_use(),
            contexts: self.contexts.len() as u32,
            exports: self.exports.len() as u32,
            ..Default::default()
        };
        // An exported arena is also its context's until either lets go: count it once.
        let mut arenas: HashMap<*const Arena, u64> = HashMap::new();
        for c in self.contexts.values() {
            st.rings += c.ring.is_some() as u32;
            st.surfaces += lock(&c.surfaces).len() as u32;
            st.objects += c.mailbox.objects.load(Ordering::Relaxed);
            st.buried += c.mailbox.buried.load(Ordering::Relaxed);
            st.record_bytes += c.mailbox.record_bytes.load(Ordering::Relaxed);
            for a in lock(&c.arenas).values() {
                arenas.insert(Arc::as_ptr(a), a.size);
            }
        }
        for a in self.exports.values() {
            arenas.insert(Arc::as_ptr(a), a.size);
        }
        for a in &self.placeholders {
            arenas.insert(a, a.size);
        }
        st.arenas = arenas.len() as u32;
        st.arena_bytes = arenas.values().sum();
        st
    }

    fn context_stats(&self) -> ipc::ContextStatsReply {
        let mut all: Vec<ipc::ContextStats> = self
            .contexts
            .iter()
            .map(|(&ctx, c)| {
                let arenas = lock(&c.arenas);
                ipc::ContextStats {
                    ctx,
                    objects: c.mailbox.objects.load(Ordering::Relaxed),
                    buried: c.mailbox.buried.load(Ordering::Relaxed),
                    arenas: arenas.len() as u32,
                    surfaces: lock(&c.surfaces).len() as u32,
                    arena_bytes: arenas.values().map(|a| a.size).sum(),
                    record_bytes: c.mailbox.record_bytes.load(Ordering::Relaxed),
                    kinds: c
                        .mailbox
                        .kinds
                        .each_ref()
                        .map(|k| k.load(Ordering::Relaxed)),
                    ..Default::default()
                }
            })
            .collect();
        all.sort_by_key(|c| (std::cmp::Reverse(c.objects), c.ctx));
        let mut r = ipc::ContextStatsReply {
            total: all.len() as u32,
            ..Default::default()
        };
        all.truncate(ipc::MAX_CONTEXT_STATS);
        r.count = all.len() as u32;
        r.contexts[..all.len()].copy_from_slice(&all);
        r
    }

    /// Logs what the GPU host holds, at `warn` when its heap or its footprint beyond the arenas
    /// passes the next threshold, so a default-filtered log still shows a leak's first day.
    fn log_stats(&mut self) {
        let st = self.stats();
        let other = st.footprint.saturating_sub(st.arena_bytes);
        let mut crossed = false;
        while st.heap >= self.next_heap_warn {
            self.next_heap_warn *= 2;
            crossed = true;
        }
        while other >= self.next_other_warn {
            self.next_other_warn *= 2;
            crossed = true;
        }
        let level = if crossed {
            log::Level::Warn
        } else {
            log::Level::Info
        };
        log::log!(level, "{}", describe(&st));
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
                surfaces: Arc::new(Mutex::new(HashMap::new())),
                mailbox: Arc::new(Mailbox::default()),
                ring: None,
            },
        );
        Ok(name)
    }

    fn create_arena(&mut self, r: ipc::CreateArena) -> Result<(ipc::ArenaReply, mach::Port), i32> {
        let ctx = self.contexts.get(&r.ctx).ok_or(ipc::ERR_NO_CONTEXT)?;
        if lock(&ctx.arenas).contains_key(&r.arena) {
            return Err(ipc::ERR_EXISTS);
        }
        let arena = Arena::new(&self.device, r.size).ok_or(ipc::ERR_NO_MEMORY)?;
        self.file_arena(r.ctx, r.arena, arena)
    }

    /// An arena of `r.size` bytes at exactly `r.gpu_base`. Metal places arenas lowest-address
    /// first-fit, so a probe that lands lower has found a hole: a placeholder fills it up to the
    /// address, and the probe goes again.
    fn restore_arena(
        &mut self,
        r: ipc::RestoreArena,
    ) -> Result<(ipc::ArenaReply, mach::Port), i32> {
        let ctx = self.contexts.get(&r.ctx).ok_or(ipc::ERR_NO_CONTEXT)?;
        if lock(&ctx.arenas).contains_key(&r.arena) {
            return Err(ipc::ERR_EXISTS);
        }
        for _ in 0..MAX_FILLS {
            let probe = Arena::new(&self.device, r.size).ok_or(ipc::ERR_NO_MEMORY)?;
            if probe.gpu_base == r.gpu_base {
                return self.file_arena(r.ctx, r.arena, probe);
            }
            let hole = probe.gpu_base;
            drop(probe);
            let span = r.gpu_base.wrapping_sub(hole);
            if hole > r.gpu_base || span < GRANULE || span % GRANULE != 0 {
                log::warn!(
                    "restore: arena {} of context {} wanted at {:#x}, the space offers {hole:#x}",
                    r.arena,
                    r.ctx,
                    r.gpu_base
                );
                return Err(ipc::ERR_BAD_VALUE);
            }
            let filler = Arena::new(&self.device, span - GUARD).ok_or(ipc::ERR_NO_MEMORY)?;
            if filler.gpu_base != hole {
                log::warn!(
                    "restore: a placeholder for {hole:#x} landed at {:#x}",
                    filler.gpu_base
                );
                return Err(ipc::ERR_BAD_VALUE);
            }
            self.placeholders.push(filler);
        }
        log::warn!(
            "restore: arena {} of context {} wanted at {:#x}, still not there after {MAX_FILLS} \
             placeholders",
            r.arena,
            r.ctx,
            r.gpu_base
        );
        Err(ipc::ERR_BAD_VALUE)
    }

    /// Files `arena` as arena `id` of context `ctx` and makes the entry the worker maps it by.
    fn file_arena(
        &mut self,
        ctx: u32,
        id: u32,
        arena: Arena,
    ) -> Result<(ipc::ArenaReply, mach::Port), i32> {
        let ctx = self.contexts.get(&ctx).ok_or(ipc::ERR_NO_CONTEXT)?;
        let mut arenas = lock(&ctx.arenas);
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
        arenas.insert(id, Arc::new(arena));
        Ok((reply, entry))
    }

    fn export_context(&mut self, ctx: u32) -> Result<(ipc::ExportReply, mach::Port), i32> {
        let c = self.contexts.get(&ctx).ok_or(ipc::ERR_NO_CONTEXT)?;
        let export = if c.ring.is_some() {
            let (tx, rx) = std::sync::mpsc::channel();
            *c.mailbox.export.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
            c.doorbell.signal();
            rx.recv_timeout(EXPORT_WAIT).map_err(|_| {
                log::warn!("context {ctx}: no export from its ring thread");
                ipc::ERR_INTERNAL
            })?
        } else {
            ContextExport::default()
        };
        let bytes = export.encode();
        let blob = mach::Anonymous::new(bytes.len() as u64).map_err(|e| {
            log::warn!("export memory: {e}");
            ipc::ERR_NO_MEMORY
        })?;
        // SAFETY: the mapping is at least `bytes.len()` long and ours alone.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), blob.as_ptr(), bytes.len()) };
        // The entry keeps the pages once the mapping goes.
        let (entry, _) = mach::make_memory_entry(blob.addr(), blob.size()).map_err(|e| {
            log::warn!("export memory entry: {e}");
            ipc::ERR_INTERNAL
        })?;
        Ok((
            ipc::ExportReply {
                status: ipc::OK,
                _pad: 0,
                len: bytes.len() as u64,
            },
            entry,
        ))
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

    fn bind_surface(&mut self, r: ipc::SurfaceRequest, port: u32) -> i32 {
        let Some(ctx) = self.contexts.get(&r.ctx) else {
            return ipc::ERR_NO_CONTEXT;
        };
        let Some(surface) = metal::Surface::from_port(port) else {
            return ipc::ERR_BAD_VALUE;
        };
        let mut surfaces = ctx.surfaces.lock().unwrap_or_else(|e| e.into_inner());
        if surfaces.contains_key(&r.surface) {
            log::debug!(
                "surface: context {} resource {} already bound, IOSurface {} refused",
                r.ctx,
                r.surface,
                surface.id()
            );
            return ipc::ERR_EXISTS;
        }
        log::debug!(
            "surface: context {} binds resource {} to IOSurface {} ({}x{})",
            r.ctx,
            r.surface,
            surface.id(),
            surface.width(),
            surface.height()
        );
        surfaces.insert(r.surface, surface);
        ipc::OK
    }

    fn release_surface(&mut self, r: ipc::SurfaceRequest) -> i32 {
        let Some(ctx) = self.contexts.get(&r.ctx) else {
            return ipc::ERR_NO_CONTEXT;
        };
        let removed = ctx
            .surfaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&r.surface);
        log::debug!(
            "surface: context {} releases resource {} (IOSurface {:?})",
            r.ctx,
            r.surface,
            removed.as_ref().map(metal::Surface::id)
        );
        if removed.is_some() {
            ipc::OK
        } else {
            ipc::ERR_NO_ARENA
        }
    }

    fn restore_context(&mut self, r: ipc::RestoreContext, entry: &mach::Port) -> i32 {
        let export = match Mapping::map(entry, r.len.max(1).next_multiple_of(metal::PAGE)) {
            // SAFETY: the mapping holds at least `len` bytes.
            Ok(m) => ContextExport::decode(unsafe {
                std::slice::from_raw_parts(m.as_ptr(), r.len as usize)
            }),
            Err(e) => {
                log::warn!("restore: map export: {e}");
                return ipc::ERR_MALFORMED;
            }
        };
        let Some(export) = export else {
            log::warn!("restore: context {}: malformed export", r.ring.ctx);
            return ipc::ERR_MALFORMED;
        };
        let position = export.position;
        self.start_ring(
            r.ring,
            |region, size| {
                // SAFETY: as for `attach`, checked by `start_ring`.
                unsafe { Consumer::resume(region, size, position) }
            },
            |exec, ids| exec.restore(&export, ids),
        )
    }

    fn attach_ring(&mut self, r: ipc::AttachRing) -> i32 {
        self.start_ring(
            r,
            |region, size| {
                // SAFETY: inside the arena, for the region's whole size, checked by `start_ring`.
                unsafe { Consumer::attach(region, size) }
            },
            |_, _| {},
        )
    }

    /// Starts context `r.ctx`'s ring thread over the ring `r` locates, consumed by what `consumer`
    /// makes of it, once `prepare` has had the executor.
    fn start_ring(
        &mut self,
        r: ipc::AttachRing,
        consumer: impl FnOnce(NonNull<u8>, u32) -> Result<Consumer, ring::Error>,
        prepare: impl FnOnce(&mut Executor, &mut Placeholders),
    ) -> i32 {
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
        let consumer = match consumer(region, r.data_size) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("attach ring: {e:?}");
                return ipc::ERR_BAD_VALUE;
            }
        };
        let Some(mut exec) = Executor::new(
            self.device.clone(),
            consumer,
            arena,
            ctx.arenas.clone(),
            ctx.surfaces.clone(),
            ctx.mailbox.clone(),
        ) else {
            return ipc::ERR_INTERNAL;
        };
        prepare(&mut exec, &mut self.id_placeholders);
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

/// One line for the log and for `limina debug`.
pub fn describe(st: &ipc::Stats) -> String {
    const MIB: u64 = 1 << 20;
    format!(
        "gpu-host: footprint {} MiB, heap {} MiB, arenas {} ({} MiB), contexts {} ({} with a \
         ring), exports {}, surfaces {}, objects {} (records {} MiB), buried {}",
        st.footprint / MIB,
        st.heap / MIB,
        st.arenas,
        st.arena_bytes / MIB,
        st.contexts,
        st.rings,
        st.exports,
        st.surfaces,
        st.objects,
        st.record_bytes / MIB,
        st.buried
    )
}

/// One line per context, as `limina debug` lists the contexts holding the most objects.
pub fn describe_context(c: &ipc::ContextStats) -> String {
    const MIB: u64 = 1 << 20;
    let mut line = format!(
        "gpu-host context {}: objects {} (records {} KiB), buried {}, arenas {} ({} MiB), \
         surfaces {}",
        c.ctx,
        c.objects,
        c.record_bytes >> 10,
        c.buried,
        c.arenas,
        c.arena_bytes / MIB,
        c.surfaces
    );
    let mut sep = "; ";
    for (name, &n) in ipc::KINDS.iter().zip(&c.kinds) {
        if n != 0 {
            line += &format!("{sep}{name} {n}");
            sep = ", ";
        }
    }
    line
}

/// This process's `phys_footprint`: what macOS bills it, Activity Monitor's "Memory".
fn footprint() -> u64 {
    // SAFETY: zeroed POD that proc_pid_rusage fills; the buffer argument is `rusage_info_t`, a
    // pointer to the struct.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            (&raw mut info).cast::<libc::rusage_info_t>(),
        )
    };
    if rc == 0 { info.ri_phys_footprint } else { 0 }
}

/// Bytes malloc has handed out and not had back, over every zone.
fn heap_in_use() -> u64 {
    // SAFETY: zeroed POD; a null zone asks for every zone's statistics, summed into `st`.
    let mut st: libc::malloc_statistics_t = unsafe { std::mem::zeroed() };
    unsafe { libc::malloc_zone_statistics(std::ptr::null_mut(), &mut st) };
    st.size_in_use as u64
}

/// Exits when the parent (the worker) does, so a GPU host never outlives its VM.
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
