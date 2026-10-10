// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Frame-sequence capture: every frame a window puts on glass, as an image plus a tagged record.
//!
//! Armed by `LIMINA_WINDOW_CAPTURE_DIR=<dir>` for the whole run, or at runtime over the debug
//! plane (`limina debug <vm> capture start <dir>` / `capture stop`), which is how a harness
//! captures just the stretch it cares about. The directory's layout and what each record's
//! numbers mean are `limina_framecap`'s docs. `LIMINA_WINDOW_CAPTURE` (one PNG, overwritten) is a
//! separate diagnostic and is untouched by this.
//!
//! **Where it hooks.** [`on_present`] runs from `GuestWindow::show_with_ack`, the one place every
//! window — primary or secondary, zero-copy or copied — puts a surface on its layer. A frame the
//! window shows through a private copy is captured from the copy, which nothing else writes.
//!
//! **What the main thread pays.** One GPU blit of the surface on glass into a capture buffer of
//! our own, encoded and committed, and nothing else: no lock, no wait, no memcpy. The blit's
//! completion is waited for, and the buffer read and encoded, on encoder threads.
//!
//! **Why the pixels are the frame's.** A guest that is held off its scanout gets a buffer back
//! only once the frame that REPLACED it is off glass (the shown-ack names the replaced surface),
//! and that ack cannot be sent before the next frame is put on the layer — which runs
//! [`on_present`] for it first. So when the next frame on a slot goes up, the previous frame's
//! blit either has finished, and read the frame's own pixels, or it has not, and might read the
//! guest's next frame: that one is discarded as `overtaken` rather than kept with pixels that
//! could belong to a later present. A copy and the worker's software-2D ring are rewritten only
//! two presents later, which this check covers too. What it cannot cover is a buffer the window
//! itself shows torn: an unheld scanout the window shows without a copy, or the worker's 150 ms
//! fallback releasing a buffer the window never acknowledged. Those reach the capture as they
//! reached the glass.
//!
//! **Bounds.** At most [`BUFFERS`] frames are between the main thread and the disk at once
//! (memory: that many surfaces of the scanout's size); a frame shown while all are busy is
//! recorded `queue_full`. Images stop once `LIMINA_WINDOW_CAPTURE_DIR_MAX_MB` (default 4096) has
//! been written; every frame after that is still recorded, as `disk_budget`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use limina_framecap::{PixelOrder, Reason, Record, Reorder, Sequencer, Summary, Tally};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder,
    MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLTexture,
    MTLTextureDescriptor,
};

use super::present::SendSurface;

/// Which guest frame a present is: the slot it belongs to, and the worker's flip count for that
/// slot when the window applied it (`SlotPresent::frames`, under the worker generation `epoch`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameTag {
    pub(crate) slot: usize,
    pub(crate) flip: u64,
    pub(crate) epoch: u64,
}

/// Capture buffers in flight between the main thread and the disk.
const BUFFERS: usize = 8;

/// Encoder threads. At 2560x1440 one frame takes ~25 ms, so four keep up with 60 Hz.
const ENCODERS: usize = 4;

/// How long a stop waits for the frames already copied to be written.
const DRAIN: Duration = Duration::from_secs(30);

/// Whether a capture is running: the one load a present pays when none is.
static ACTIVE: AtomicBool = AtomicBool::new(false);

static SESSION: Mutex<Option<Arc<Session>>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A command buffer an encoder thread waits on. Waiting on and reading the status of a command
/// buffer is safe from any thread; objc2 leaves the protocol object `!Send` conservatively.
struct SendCommands(Retained<ProtocolObject<dyn MTLCommandBuffer>>);
// SAFETY: see above — the encoder only waits on it and reads its status.
unsafe impl Send for SendCommands {}

/// One copied frame on its way to the disk.
struct Job {
    ord: u64,
    record: Record,
    buffer: SendSurface,
    order: PixelOrder,
    commands: SendCommands,
    /// Set by the main thread when the next frame on this slot went up before the blit finished.
    overtaken: Arc<AtomicBool>,
}

/// The main thread's half of a capture.
struct MainState {
    closed: bool,
    seq: Sequencer,
    next_ord: u64,
    /// Per slot, the last frame's blit and its overtaken flag.
    last_blit: std::collections::HashMap<usize, (SendCommands, Arc<AtomicBool>)>,
    tx: Option<Sender<Job>>,
}

/// The capture buffers not in use, and how many exist.
struct Pool {
    free: Vec<SendSurface>,
    total: usize,
    geom: (usize, usize),
}

/// The disk's half: the sidecar, the order it is written in, and the totals.
struct Out {
    sidecar: std::fs::File,
    reorder: Reorder,
    tally: Tally,
}

struct Session {
    dir: PathBuf,
    started: Instant,
    budget: u64,
    bytes: AtomicU64,
    main: Mutex<MainState>,
    pool: Mutex<Pool>,
    out: Mutex<Out>,
    /// Numbered records not yet written to the sidecar's reorder buffer, and its signal.
    outstanding: Mutex<u64>,
    drained: Condvar,
}

impl Session {
    /// A record has its number: count it as outstanding until [`Self::finish`].
    fn take_ord(&self, m: &mut MainState) -> u64 {
        *lock(&self.outstanding) += 1;
        m.next_ord += 1;
        m.next_ord - 1
    }

    /// Record `r`, numbered `ord`, as final.
    fn finish(&self, ord: u64, r: Record, bytes: u64) {
        {
            use std::io::Write;
            let mut out = lock(&self.out);
            out.tally.count(&r, bytes);
            let ready = out.reorder.put(ord, r.to_line());
            for line in ready {
                if let Err(e) = writeln!(out.sidecar, "{line}") {
                    log::error!("frame capture: writing {}: {e}", limina_framecap::SIDECAR);
                }
            }
        }
        let mut n = lock(&self.outstanding);
        *n -= 1;
        if *n == 0 {
            self.drained.notify_all();
        }
    }

    /// A free capture buffer of `geom`, or a new one while fewer than [`BUFFERS`] exist.
    fn buffer(&self, geom: (usize, usize)) -> Option<SendSurface> {
        let mut pool = lock(&self.pool);
        if pool.geom != geom {
            // A modeset: buffers of the old size are of no further use. Ones still being encoded
            // are dropped when they come back (see `give_back`).
            pool.total -= pool.free.len();
            pool.free.clear();
            pool.geom = geom;
        }
        if let Some(b) = pool.free.pop() {
            return Some(b);
        }
        if pool.total >= BUFFERS {
            return None;
        }
        let surface = super::diag::create_local_iosurface(geom.0 as u32, geom.1 as u32)?;
        pool.total += 1;
        Some(SendSurface::new(surface))
    }

    fn give_back(&self, buffer: SendSurface) {
        let mut pool = lock(&self.pool);
        let s = buffer.into_inner();
        if (s.width(), s.height()) == pool.geom {
            pool.free.push(SendSurface::new(s));
        } else {
            pool.total = pool.total.saturating_sub(1);
        }
    }
}

/// Nanoseconds on `clock`.
fn clock_ns(clock: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid clock id and a timespec to fill.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// The byte order an IOSurface's pixel format names. Everything the worker makes is `'BGRA'`.
fn pixel_order(surface: &IOSurfaceRef) -> PixelOrder {
    if surface.pixel_format() == u32::from_be_bytes(*b"RGBA") {
        PixelOrder::Rgba
    } else {
        PixelOrder::Bgra
    }
}

/// `frame` went on glass on its window as guest surface `id`, showing `shown` (the guest's
/// surface, or the window's private copy of it). Main thread only; costs one atomic load when no
/// capture is running.
pub(crate) fn on_present(tag: FrameTag, id: u32, shown: &CFRetained<IOSurfaceRef>) {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let Some(session) = lock(&SESSION).clone() else {
        return;
    };
    let t0 = Instant::now();
    session.present(tag, id, shown);
    let us = t0.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
    lock(&session.out).tally.hook_cost(us);
}

impl Session {
    fn present(&self, tag: FrameTag, id: u32, shown: &CFRetained<IOSurfaceRef>) {
        let mono = clock_ns(libc::CLOCK_MONOTONIC_RAW);
        let real = clock_ns(libc::CLOCK_REALTIME);
        let mut m = lock(&self.main);
        if m.closed {
            return;
        }
        let FrameTag { slot, flip, epoch } = tag;
        let (seq, missed) = m.seq.present(slot, epoch, flip);
        for f in missed {
            let ord = self.take_ord(&mut m);
            self.finish(ord, Record::not_presented(slot, epoch, f), 0);
        }
        // The next frame on this slot is going up: a blit of the previous one still running
        // could now read the guest's next frame.
        if let Some((commands, overtaken)) = m.last_blit.remove(&slot)
            && commands.0.status() != MTLCommandBufferStatus::Completed
        {
            overtaken.store(true, Ordering::SeqCst);
        }
        let geom = (shown.width(), shown.height());
        let ord = self.take_ord(&mut m);
        let record = Record {
            slot,
            seq: Some(seq),
            flip,
            epoch,
            iosurface: Some(id),
            shown_iosurface: Some(shown.id()),
            width: Some(geom.0 as u32),
            height: Some(geom.1 as u32),
            t_monotonic_raw_ns: Some(mono),
            t_realtime_ns: Some(real),
            file: Some(limina_framecap::file_name(slot, seq)),
            ..Record::default()
        };
        if self.bytes.load(Ordering::Relaxed) >= self.budget {
            self.finish(ord, record.drop_for(Reason::DiskBudget), 0);
            return;
        }
        let Some(buffer) = self.buffer(geom) else {
            self.finish(ord, record.drop_for(Reason::QueueFull), 0);
            return;
        };
        let Some(commands) = gpu_copy(shown, &buffer) else {
            self.give_back(buffer);
            self.finish(ord, record.drop_for(Reason::CopyFailed), 0);
            return;
        };
        let overtaken = Arc::new(AtomicBool::new(false));
        m.last_blit
            .insert(slot, (SendCommands(commands.clone()), overtaken.clone()));
        let job = Job {
            ord,
            record,
            buffer,
            order: pixel_order(shown),
            commands: SendCommands(commands),
            overtaken,
        };
        if let Some(tx) = &m.tx
            && let Err(std::sync::mpsc::SendError(job)) = tx.send(job)
        {
            self.give_back(job.buffer);
            self.finish(job.ord, job.record.drop_for(Reason::WriteError), 0);
        }
    }
}

/// Wait for a copied frame, then encode and write it.
fn encode(session: &Session, job: Job) {
    let Job {
        ord,
        record,
        buffer,
        order,
        commands,
        overtaken,
    } = job;
    commands.0.waitUntilCompleted();
    let failed = commands.0.status() != MTLCommandBufferStatus::Completed;
    // After the wait: the main thread sets this before the guest can have the buffer back.
    if failed || overtaken.load(Ordering::SeqCst) {
        session.give_back(buffer);
        let reason = if failed {
            Reason::CopyFailed
        } else {
            Reason::Overtaken
        };
        session.finish(ord, record.drop_for(reason), 0);
        return;
    }
    let png = {
        let s = buffer.clone().into_inner();
        // SAFETY: our own surface, whose blit has completed; the lock pins its base address for
        // the read, and the slice is exactly its allocation's rows.
        unsafe {
            s.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
            let (w, h, bpr) = (s.width(), s.height(), s.bytes_per_row());
            let bytes = std::slice::from_raw_parts(s.base_address().as_ptr() as *const u8, h * bpr);
            let png = limina_framecap::encode_png_rgb(bytes, w, h, bpr, order);
            s.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
            png
        }
    };
    session.give_back(buffer);
    let file = record.file.clone().unwrap_or_default();
    let written = png.and_then(|png| {
        // Checked again here: several encoders may be past the main thread's check at once.
        if session.bytes.load(Ordering::Relaxed) >= session.budget {
            return Ok(None);
        }
        std::fs::write(session.dir.join(&file), &png)
            .map(|()| Some(png.len() as u64))
            .map_err(|e| e.to_string())
    });
    match written {
        Ok(Some(n)) => {
            session.bytes.fetch_add(n, Ordering::Relaxed);
            session.finish(ord, record, n);
        }
        Ok(None) => session.finish(ord, record.drop_for(Reason::DiskBudget), 0),
        Err(e) => {
            log::warn!("frame capture: {file}: {e}");
            session.finish(ord, record.drop_for(Reason::WriteError), 0);
        }
    }
}

struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
}

thread_local! {
    /// The main thread's Metal device and queue for capture blits; `None` when there is none.
    static GPU: std::cell::OnceCell<Option<Gpu>> = const { std::cell::OnceCell::new() };
}

/// A 4-byte texture over `surface`. The blit moves bytes, so the buffer keeps the source's own
/// byte order, which the encoder is told separately.
fn texture(gpu: &Gpu, surface: &IOSurfaceRef) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
    // SAFETY: a plain descriptor for a 2D texture of the surface's own size.
    let desc = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            surface.width(),
            surface.height(),
            false,
        )
    };
    gpu.device
        .newTextureWithDescriptor_iosurface_plane(&desc, surface, 0)
}

/// Commit a blit of `src` into `dst` (same size). `None` when Metal cannot do it.
fn gpu_copy(
    src: &IOSurfaceRef,
    dst: &SendSurface,
) -> Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>> {
    let dst = dst.clone().into_inner();
    GPU.with(|gpu| {
        let gpu = gpu
            .get_or_init(|| {
                let device = MTLCreateSystemDefaultDevice()?;
                let queue = device.newCommandQueue()?;
                Some(Gpu { device, queue })
            })
            .as_ref()?;
        let from = texture(gpu, src)?;
        let to = texture(gpu, &dst)?;
        let commands = gpu.queue.commandBuffer()?;
        let blit = commands.blitCommandEncoder()?;
        // SAFETY: a retained command buffer keeps both textures alive until it completes.
        unsafe { blit.copyFromTexture_toTexture(&from, &to) };
        blit.endEncoding();
        commands.commit();
        Some(commands)
    })
}

/// `LIMINA_WINDOW_CAPTURE_DIR_MAX_MB`: how many megabytes of images a capture may write.
fn budget_from_env() -> u64 {
    std::env::var("LIMINA_WINDOW_CAPTURE_DIR_MAX_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(4096)
        .saturating_mul(1_000_000)
}

/// Start capturing into `dir`, which must not already hold a capture.
pub(crate) fn start(dir: &Path) -> Result<String, String> {
    let mut slot = lock(&SESSION);
    if let Some(s) = slot.as_ref() {
        return Err(format!(
            "already capturing to {} (capture stop first)",
            s.dir.display()
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let sidecar_path = dir.join(limina_framecap::SIDECAR);
    let sidecar = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sidecar_path)
        .map_err(|e| {
            format!(
                "{} (a directory that already holds a capture is never added to): {e}",
                sidecar_path.display()
            )
        })?;
    let (tx, rx) = std::sync::mpsc::channel::<Job>();
    let session = Arc::new(Session {
        dir: dir.to_path_buf(),
        started: Instant::now(),
        budget: budget_from_env(),
        bytes: AtomicU64::new(0),
        main: Mutex::new(MainState {
            closed: false,
            seq: Sequencer::default(),
            next_ord: 0,
            last_blit: Default::default(),
            tx: Some(tx),
        }),
        pool: Mutex::new(Pool {
            free: Vec::new(),
            total: 0,
            geom: (0, 0),
        }),
        out: Mutex::new(Out {
            sidecar,
            reorder: Reorder::default(),
            tally: Tally::default(),
        }),
        outstanding: Mutex::new(0),
        drained: Condvar::new(),
    });
    let rx: Arc<Mutex<Receiver<Job>>> = Arc::new(Mutex::new(rx));
    for i in 0..ENCODERS {
        let (session, rx) = (session.clone(), rx.clone());
        std::thread::Builder::new()
            .name(format!("limina-framecap-{i}"))
            .spawn(move || {
                loop {
                    // The receiver's lock is held only to take a job, never across an encode.
                    let job = lock(&rx).recv();
                    match job {
                        Ok(job) => encode(&session, job),
                        Err(_) => return,
                    }
                }
            })
            .map_err(|e| format!("spawning a frame encoder: {e}"))?;
    }
    *slot = Some(session);
    ACTIVE.store(true, Ordering::Release);
    let msg = format!(
        "frame capture: capturing every presented frame to {}",
        dir.display()
    );
    log::warn!("{msg}");
    Ok(msg)
}

/// Stop the capture: wait for the frames already copied, then write and return the summary.
pub(crate) fn stop() -> Result<Summary, String> {
    ACTIVE.store(false, Ordering::Release);
    let session = lock(&SESSION).take().ok_or("no frame capture is running")?;
    {
        let mut m = lock(&session.main);
        m.closed = true;
        // Dropping the sender ends the encoders once the queue is empty.
        m.tx = None;
        m.last_blit.clear();
    }
    let duration = session.started.elapsed();
    let deadline = Instant::now() + DRAIN;
    let mut n = lock(&session.outstanding);
    while *n > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        n = session
            .drained
            .wait_timeout(n, left)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
    let unfinished = *n;
    drop(n);
    let summary = {
        use std::io::Write;
        let mut out = lock(&session.out);
        let summary = out.tally.summary(duration.as_secs_f64());
        if let Err(e) = writeln!(out.sidecar, "{}", summary.to_line()) {
            log::error!("frame capture: writing the summary: {e}");
        }
        summary
    };
    if unfinished > 0 {
        return Err(format!(
            "{unfinished} frames were still being written after {DRAIN:?}; {} is incomplete",
            limina_framecap::SIDECAR
        ));
    }
    log::warn!(
        "frame capture: stopped ({}): {}",
        session.dir.display(),
        summary.describe()
    );
    Ok(summary)
}

/// The running capture's directory, for `limina debug <vm> status`.
pub(crate) fn status() -> Option<PathBuf> {
    lock(&SESSION).as_ref().map(|s| s.dir.clone())
}

/// `LIMINA_WINDOW_CAPTURE_DIR`: capture for the whole run. Called once as the window comes up.
pub(crate) fn start_from_env() {
    if let Some(dir) = std::env::var_os("LIMINA_WINDOW_CAPTURE_DIR")
        && let Err(e) = start(Path::new(&dir))
    {
        log::error!("frame capture: LIMINA_WINDOW_CAPTURE_DIR: {e}");
    }
}

/// Finish a running capture as the process leaves, so its sidecar ends in its summary.
pub(crate) fn finish_at_exit() {
    if ACTIVE.load(Ordering::Acquire)
        && let Err(e) = stop()
    {
        log::error!("frame capture: {e}");
    }
}
