// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Frame-sequence capture: every frame a window puts on glass, as an image plus a tagged record.
//!
//! Armed by `LIMINA_WINDOW_CAPTURE_DIR=<dir>` for the whole run, or at runtime over the debug
//! plane (`limina debug <vm> capture start <dir>` / `capture stop`), which is how a harness
//! captures just the stretch it cares about. The directory's layout and what every field means
//! are defined in `docs/graphics.md` §8; the types are `limina_framecap`'s. `LIMINA_WINDOW_CAPTURE`
//! (one PNG, overwritten) is a separate diagnostic and is untouched by this.
//!
//! **Where it hooks.** [`on_present`] runs from `GuestWindow::show_with_ack`, the one place every
//! window — primary or secondary, zero-copy or copied — puts a surface on its layer. A frame the
//! window shows through a private copy is captured from the copy, which nothing else writes.
//!
//! **What the main thread pays.** One GPU blit of the surface on glass into a capture buffer of
//! our own, encoded and committed, plus a message per record to the sidecar's writer thread:
//! no wait, no memcpy, no file I/O. The blit's completion is waited for, and the buffer read and
//! encoded, on encoder threads; `frames.jsonl` is written, buffered, by one writer thread that
//! also keeps the records in present order. The cost the summary reports is measured around all
//! of it. One indirect cost remains: while a blit reads a surface it holds that surface's use
//! count, and the shown-ack waits for the replaced surface to fall out of use, so a capture can
//! delay an ack by up to the blit's length (about 1 ms).
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
//! (memory: that many surfaces, of whatever sizes the displays have; the free ones are kept per
//! size, so two displays of different sizes reuse their own); a frame shown while all are busy is
//! recorded `queue_full`. Images stop once `LIMINA_WINDOW_CAPTURE_DIR_MAX_MB` (default 4096) has
//! been written; every frame after that is still recorded, as `disk_budget`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use limina_framecap::{
    Cause, HostState, HostStateCause, HostStateRecord, PixelOrder, Presented, Reason, Record,
    Reorder, Sequencer, Summary, Tally,
};
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

/// Which guest frame a present is: the slot it belongs to, the worker's flip count for that slot
/// when the window applied it (`SlotPresent::frames`, under the worker generation `epoch`), and
/// — when the window knows the surface is no guest flip — the host path that put it up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameTag {
    pub(crate) slot: usize,
    pub(crate) flip: u64,
    pub(crate) epoch: u64,
    pub(crate) cause: Option<Cause>,
}

impl FrameTag {
    /// A present of the slot's `show_id`, which is always a guest flip: a scanout announcement
    /// names nothing to show (`SlotPresent::show_id`), so no window presents a buffer no flip
    /// has drawn into and nothing tags [`Cause::ScanoutConfigured`]. A flip shown again is
    /// recognised by the sequencer ([`Cause::Reshow`]).
    pub(crate) fn new(slot: usize, flip: u64, epoch: u64) -> Self {
        FrameTag {
            slot,
            flip,
            epoch,
            cause: None,
        }
    }
}

/// Capture buffers in flight between the main thread and the disk.
const BUFFERS: usize = 8;

/// Encoder threads. At 2560x1440 one frame takes ~25 ms, so four keep up with 60 Hz.
const ENCODERS: usize = 4;

/// How long `capture stop` waits for the frames already copied to be written.
const DRAIN: Duration = Duration::from_secs(30);

/// How long the process's exit waits for them: a wedged GPU must not hold the exit up.
const DRAIN_AT_EXIT: Duration = Duration::from_secs(3);

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
    last_blit: HashMap<usize, (SendCommands, Arc<AtomicBool>)>,
    tx: Option<Sender<Job>>,
    /// The host state last written, `None` until the first: that one is the capture start's.
    host: Option<HostState>,
}

/// The capture buffers not in use, by size, and how many exist of every size together.
#[derive(Default)]
struct Pool {
    free: HashMap<(usize, usize), Vec<SendSurface>>,
    total: usize,
}

/// What the sidecar's writer thread is told.
enum Msg {
    /// Record number `ord` is final; `bytes` of image were written for it.
    Done(u64, Record, u64),
    /// Line number `ord` is a host-state record: in order, but no frame to count.
    Line(u64, String),
    /// What one present cost the main thread, in microseconds.
    Cost(u32),
    /// Write the summary and stop writing; answer with it.
    Close {
        duration: f64,
        incomplete: Option<u64>,
        reply: SyncSender<Summary>,
    },
}

/// Numbered records not yet handed to the writer thread, and the signal for reaching none.
#[derive(Default)]
struct Outstanding {
    n: AtomicU64,
    lock: Mutex<()>,
    zero: Condvar,
}

impl Outstanding {
    fn add(&self) {
        self.n.fetch_add(1, Ordering::SeqCst);
    }

    fn done(&self) {
        if self.n.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _g = lock(&self.lock);
            self.zero.notify_all();
        }
    }

    /// Wait until none are outstanding or `deadline`; how many still are.
    fn wait(&self, deadline: Instant) -> u64 {
        let mut g = lock(&self.lock);
        loop {
            let n = self.n.load(Ordering::SeqCst);
            let left = deadline.saturating_duration_since(Instant::now());
            if n == 0 || left.is_zero() {
                return n;
            }
            // Short slices: a `done` between the load and the wait would otherwise be missed.
            g = self
                .zero
                .wait_timeout(g, left.min(Duration::from_millis(50)))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
}

struct Session {
    dir: PathBuf,
    started: Instant,
    budget: u64,
    bytes: AtomicU64,
    main: Mutex<MainState>,
    pool: Mutex<Pool>,
    out: Sender<Msg>,
    outstanding: Arc<Outstanding>,
}

impl Session {
    /// A record has its number: it is outstanding until [`Self::finish`].
    fn take_ord(&self, m: &mut MainState) -> u64 {
        self.outstanding.add();
        m.next_ord += 1;
        m.next_ord - 1
    }

    /// Record `r`, numbered `ord`, is final. Never touches the disk: the writer thread does.
    fn finish(&self, ord: u64, r: Record, bytes: u64) {
        if self.out.send(Msg::Done(ord, r, bytes)).is_err() {
            log::error!("frame capture: the sidecar writer is gone");
        }
        self.outstanding.done();
    }

    /// Write `state` as a host-state record, unless it is the one already written. The first
    /// one a capture writes is its start's, whatever `cause` says.
    fn host_state(&self, m: &mut MainState, state: HostState, cause: HostStateCause) {
        let cause = match &m.host {
            None => HostStateCause::CaptureStart,
            Some(prev) if prev.same_as(&state) => return,
            Some(_) => cause,
        };
        let record = HostStateRecord::new(
            cause,
            clock_ns(libc::CLOCK_MONOTONIC_RAW),
            clock_ns(libc::CLOCK_REALTIME),
            state.clone(),
        );
        m.host = Some(state);
        let ord = self.take_ord(m);
        if self.out.send(Msg::Line(ord, record.to_line())).is_err() {
            log::error!("frame capture: the sidecar writer is gone");
        }
        self.outstanding.done();
    }

    /// A free capture buffer of `geom`, or a new one. Fewer than [`BUFFERS`] exist in all; at
    /// the cap a free buffer of another size is let go to make room.
    fn buffer(&self, geom: (usize, usize)) -> Option<SendSurface> {
        let mut pool = lock(&self.pool);
        if let Some(b) = pool.free.get_mut(&geom).and_then(Vec::pop) {
            return Some(b);
        }
        if pool.total >= BUFFERS {
            let other = pool
                .free
                .iter_mut()
                .find(|(_, v)| !v.is_empty())
                .and_then(|(_, v)| v.pop());
            other.as_ref()?;
            pool.total -= 1;
        }
        let surface = super::diag::create_local_iosurface(geom.0 as u32, geom.1 as u32)?;
        pool.total += 1;
        Some(SendSurface::new(surface))
    }

    fn give_back(&self, buffer: SendSurface) {
        let s = buffer.into_inner();
        let geom = (s.width(), s.height());
        lock(&self.pool)
            .free
            .entry(geom)
            .or_default()
            .push(SendSurface::new(s));
    }
}

/// The sidecar's writer: keeps records in present order, totals them, and writes them buffered,
/// flushing whenever it has caught up. After the summary it writes nothing more, so a record
/// that finishes late — a stop that gave up waiting — cannot land after the summary.
fn write_sidecar(rx: Receiver<Msg>, sidecar: std::fs::File) {
    use std::io::Write;
    let mut file = std::io::BufWriter::new(sidecar);
    let mut reorder = Reorder::default();
    let mut tally = Tally::default();
    let mut closed = false;
    let mut dirty = false;
    loop {
        let msg = if dirty {
            match rx.try_recv() {
                Ok(m) => m,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if let Err(e) = file.flush() {
                        log::error!("frame capture: writing {}: {e}", limina_framecap::SIDECAR);
                    }
                    dirty = false;
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            }
        };
        match msg {
            Msg::Done(..) | Msg::Line(..) if closed => {}
            Msg::Line(ord, line) => {
                for line in reorder.put(ord, line) {
                    if let Err(e) = writeln!(file, "{line}") {
                        log::error!("frame capture: writing {}: {e}", limina_framecap::SIDECAR);
                    }
                }
                dirty = true;
            }
            Msg::Done(ord, r, bytes) => {
                tally.count(&r, bytes);
                for line in reorder.put(ord, r.to_line()) {
                    if let Err(e) = writeln!(file, "{line}") {
                        log::error!("frame capture: writing {}: {e}", limina_framecap::SIDECAR);
                    }
                }
                dirty = true;
            }
            Msg::Cost(us) => tally.hook_cost(us),
            Msg::Close {
                duration,
                incomplete,
                reply,
            } => {
                let mut summary = tally.summary(duration);
                summary.incomplete = incomplete;
                if let Err(e) = writeln!(file, "{}", summary.to_line()).and_then(|()| file.flush())
                {
                    log::error!("frame capture: writing the summary: {e}");
                }
                closed = true;
                dirty = false;
                let _ = reply.send(summary);
            }
        }
    }
    let _ = file.flush();
}

/// Nanoseconds on `clock`.
pub(crate) fn clock_ns(clock: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid clock id and a timespec to fill.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// The byte order an IOSurface's pixel format names. Everything the worker makes is `'BGRA'`.
///
/// Read off the surface on glass. A copy the window shows is always `'BGRA'` whatever the
/// guest's surface was, so for an `'RGBA'` guest surface shown through a copy this would be
/// wrong; no such surface exists today.
pub(crate) fn pixel_order(surface: &IOSurfaceRef) -> PixelOrder {
    if surface.pixel_format() == u32::from_be_bytes(*b"RGBA") {
        PixelOrder::Rgba
    } else {
        PixelOrder::Bgra
    }
}

/// The host state changed to `state` (`window::host_observe`): a record in a running capture.
pub(crate) fn on_host_state(state: HostState) {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let Some(session) = lock(&SESSION).clone() else {
        return;
    };
    let mut m = lock(&session.main);
    if !m.closed {
        session.host_state(&mut m, state, HostStateCause::Change);
    }
}

/// `frame` went on glass on its window as guest surface `id`, showing `shown` (the guest's
/// surface, or the window's private copy of it). Main thread only; costs one atomic load when no
/// capture is running.
pub(crate) fn on_present(tag: FrameTag, id: u32, shown: &CFRetained<IOSurfaceRef>) {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let t0 = Instant::now();
    let Some(session) = lock(&SESSION).clone() else {
        return;
    };
    session.present(tag, id, shown);
    let us = t0.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
    let _ = session.out.send(Msg::Cost(us));
}

impl Session {
    fn present(&self, tag: FrameTag, id: u32, shown: &CFRetained<IOSurfaceRef>) {
        let mono = clock_ns(libc::CLOCK_MONOTONIC_RAW);
        let real = clock_ns(libc::CLOCK_REALTIME);
        let mut m = lock(&self.main);
        if m.closed {
            return;
        }
        let FrameTag {
            slot,
            flip,
            epoch,
            cause,
        } = tag;
        let Presented { seq, missed, cause } = m.seq.present(slot, epoch, flip, cause);
        if let Some(r) = Record::not_presented(slot, epoch, missed) {
            let ord = self.take_ord(&mut m);
            self.finish(ord, r, 0);
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
            guest_flip: cause.is_none(),
            cause,
            epoch,
            presented_iosurface: Some(id),
            layer_iosurface: Some(shown.id()),
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

/// A job's record and buffer until the encoder is done with them. If the encode unwinds, the
/// buffer goes back and the record is finished as `write_error`: a number that never reaches
/// the writer would hold every later record back for good.
struct Pending<'a> {
    session: &'a Session,
    ord: u64,
    record: Option<Record>,
    buffer: Option<SendSurface>,
}

impl Pending<'_> {
    fn finish(mut self, r: Record, bytes: u64) {
        if let Some(b) = self.buffer.take() {
            self.session.give_back(b);
        }
        self.record = None;
        self.session.finish(self.ord, r, bytes);
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(b) = self.buffer.take() {
            self.session.give_back(b);
        }
        if let Some(r) = self.record.take() {
            self.session
                .finish(self.ord, r.drop_for(Reason::WriteError), 0);
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
    let file = record.file.clone().unwrap_or_default();
    let mut pending = Pending {
        session,
        ord,
        record: Some(record),
        buffer: Some(buffer.clone()),
    };
    let record = |p: &mut Pending| p.record.clone().expect("held until finished");
    commands.0.waitUntilCompleted();
    let failed = commands.0.status() != MTLCommandBufferStatus::Completed;
    // After the wait: the main thread sets this before the guest can have the buffer back.
    if failed || overtaken.load(Ordering::SeqCst) {
        let reason = if failed {
            Reason::CopyFailed
        } else {
            Reason::Overtaken
        };
        let r = record(&mut pending).drop_for(reason);
        pending.finish(r, 0);
        return;
    }
    let s = buffer.into_inner();
    // SAFETY: our own surface, whose blit has completed. A lock that fails leaves the base
    // address unpinned, so nothing is read; one that succeeds pins it for the read, and the
    // slice is exactly its allocation's rows.
    let png = unsafe {
        if s.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut()) != 0 {
            drop(s);
            let r = record(&mut pending).drop_for(Reason::CopyFailed);
            pending.finish(r, 0);
            return;
        }
        let (w, h, bpr) = (s.width(), s.height(), s.bytes_per_row());
        let bytes = std::slice::from_raw_parts(s.base_address().as_ptr() as *const u8, h * bpr);
        let png = limina_framecap::encode_png_rgb(bytes, w, h, bpr, order);
        s.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
        png
    };
    drop(s);
    if let Some(b) = pending.buffer.take() {
        session.give_back(b);
    }
    let written = png.and_then(|png| {
        // Checked again here: several encoders may be past the main thread's check at once.
        if session.bytes.load(Ordering::Relaxed) >= session.budget {
            return Ok(None);
        }
        std::fs::write(session.dir.join(&file), &png)
            .map(|()| Some(png.len() as u64))
            .map_err(|e| e.to_string())
    });
    let r = record(&mut pending);
    match written {
        Ok(Some(n)) => {
            session.bytes.fetch_add(n, Ordering::Relaxed);
            pending.finish(r, n);
        }
        Ok(None) => pending.finish(r.drop_for(Reason::DiskBudget), 0),
        Err(e) => {
            log::warn!("frame capture: {file}: {e}");
            pending.finish(r.drop_for(Reason::WriteError), 0);
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
pub(crate) fn gpu_copy(
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

/// Whether `dir` already holds a capture's files: a sidecar, or any `s<slot>-<seq>.png`.
fn holds_a_capture(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name == limina_framecap::SIDECAR
            || (name.starts_with('s') && name.contains('-') && name.ends_with(".png"))
    })
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
    if holds_a_capture(dir) {
        return Err(format!(
            "{} already holds a capture, and one is never added to",
            dir.display()
        ));
    }
    let sidecar_path = dir.join(limina_framecap::SIDECAR);
    let sidecar = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sidecar_path)
        .map_err(|e| format!("{}: {e}", sidecar_path.display()))?;
    let session = spawn(dir, sidecar).inspect_err(|_| {
        let _ = std::fs::remove_file(&sidecar_path);
    })?;
    // Live before the state is read, and the state written before the session is published:
    // a change sampled from here on waits on `SESSION` and lands after the start's record, and
    // one sampled before it is already in what `current` returns. The start's record is
    // number 0. Before the window's first sample there is nothing to write; the first sample
    // is then the start's.
    ACTIVE.store(true, Ordering::Release);
    if let Some(state) = crate::host_state::current() {
        let mut m = lock(&session.main);
        session.host_state(&mut m, state, HostStateCause::CaptureStart);
    }
    *slot = Some(session);
    let msg = format!(
        "frame capture: capturing every presented frame to {}",
        dir.display()
    );
    log::warn!("{msg}");
    Ok(msg)
}

/// The session and its threads: the sidecar's writer, then the encoders.
fn spawn(dir: &Path, sidecar: std::fs::File) -> Result<Arc<Session>, String> {
    let (out, out_rx) = std::sync::mpsc::channel::<Msg>();
    std::thread::Builder::new()
        .name("limina-framecap-sidecar".into())
        .spawn(move || write_sidecar(out_rx, sidecar))
        .map_err(|e| format!("spawning the sidecar writer: {e}"))?;
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
            last_blit: HashMap::new(),
            tx: Some(tx),
            host: None,
        }),
        pool: Mutex::new(Pool::default()),
        out,
        outstanding: Arc::new(Outstanding::default()),
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
                    let Ok(job) = job else { return };
                    // A panic is contained to its job (`Pending` finishes it): the thread lives on.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        encode(&session, job)
                    }))
                    .is_err()
                    {
                        log::error!("frame capture: an encode panicked; recorded as write_error");
                    }
                }
            })
            .map_err(|e| format!("spawning a frame encoder: {e}"))?;
    }
    Ok(session)
}

/// Stop the capture: wait for the frames already copied, then write and return the summary.
pub(crate) fn stop() -> Result<Summary, String> {
    stop_within(DRAIN)
}

fn stop_within(drain: Duration) -> Result<Summary, String> {
    ACTIVE.store(false, Ordering::Release);
    let session = lock(&SESSION).take().ok_or("no frame capture is running")?;
    {
        let mut m = lock(&session.main);
        m.closed = true;
        // Dropping the sender ends the encoders once the queue is empty.
        m.tx = None;
        // No next frame will come to settle these: a blit still running may read whatever the
        // guest draws next, so it counts as overtaken.
        for (_, (commands, overtaken)) in m.last_blit.drain() {
            if commands.0.status() != MTLCommandBufferStatus::Completed {
                overtaken.store(true, Ordering::SeqCst);
            }
        }
    }
    let duration = session.started.elapsed();
    let unfinished = session.outstanding.wait(Instant::now() + drain);
    let (reply, answer) = std::sync::mpsc::sync_channel(1);
    let _ = session.out.send(Msg::Close {
        duration: duration.as_secs_f64(),
        incomplete: (unfinished > 0).then_some(unfinished),
        reply,
    });
    let summary = answer
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "the sidecar writer did not answer".to_string())?;
    if unfinished > 0 {
        let msg = format!(
            "frame capture: stopped ({}) after waiting {drain:?}: {}",
            session.dir.display(),
            summary.describe()
        );
        log::error!("{msg}");
        return Err(msg);
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

/// Finish a running capture as the process leaves, so its sidecar ends in its summary. Bounded
/// by [`DRAIN_AT_EXIT`]; what is still in flight then is named in the summary.
pub(crate) fn finish_at_exit() {
    if ACTIVE.load(Ordering::Acquire)
        && let Err(e) = stop_within(DRAIN_AT_EXIT)
    {
        log::error!("frame capture: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_holding_a_capture_is_recognised() {
        let dir = std::env::temp_dir().join(format!("limina-framecap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!holds_a_capture(&dir));
        std::fs::write(dir.join("notes.png"), b"").unwrap();
        assert!(
            !holds_a_capture(&dir),
            "an unrelated image is not a capture"
        );
        std::fs::write(dir.join(limina_framecap::file_name(0, 3)), b"").unwrap();
        assert!(holds_a_capture(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn host(visible: bool, priority: i32) -> HostState {
        HostState {
            windows: vec![limina_framecap::WindowState {
                slot: 0,
                visible,
                minimized: false,
            }],
            app_active: visible,
            app_hidden: !visible,
            throttled: priority <= 4,
            main_thread_priority: priority,
            displays: 1,
            displays_asleep: 0,
            screen_locked: Some(false),
            on_console: Some(true),
            thermal_state: limina_framecap::Thermal::Nominal,
            low_power_mode: false,
            no_throttle: false,
        }
    }

    #[test]
    fn host_states_take_their_place_among_the_frames_and_only_changes_are_written() {
        let dir = std::env::temp_dir().join(format!("limina-framecap-h-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(limina_framecap::SIDECAR);
        let _ = std::fs::remove_file(&path);
        let session = spawn(&dir, std::fs::File::create(&path).unwrap()).unwrap();
        {
            let mut m = lock(&session.main);
            // The first state a capture writes is its start's, whatever the caller calls it.
            session.host_state(&mut m, host(true, 31), HostStateCause::Change);
            // A priority wobble is not a change: nothing written.
            session.host_state(&mut m, host(true, 47), HostStateCause::Change);
            let ord = session.take_ord(&mut m);
            session.finish(
                ord,
                Record {
                    seq: Some(1),
                    flip: 1,
                    ..Record::default()
                }
                .drop_for(Reason::QueueFull),
                0,
            );
            session.host_state(&mut m, host(false, 4), HostStateCause::Change);
            m.tx = None;
        }
        assert_eq!(session.outstanding.wait(Instant::now() + DRAIN), 0);
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        session
            .out
            .send(Msg::Close {
                duration: 1.0,
                incomplete: None,
                reply,
            })
            .unwrap();
        let summary = answer.recv().unwrap();
        assert_eq!(
            summary.presented, 1,
            "a host state is no frame: {summary:?}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text
            .lines()
            .map(|l| limina_framecap::parse_line(l).unwrap())
            .collect();
        use limina_framecap::Line;
        match &lines[..] {
            [
                Line::HostState(start),
                Line::Frame(frame),
                Line::HostState(change),
                Line::Summary(_),
            ] => {
                assert_eq!(start.cause, HostStateCause::CaptureStart);
                assert!(start.state.visible());
                assert_eq!(frame.seq, Some(1));
                assert_eq!(change.cause, HostStateCause::Change);
                assert!(change.state.throttled && !change.state.visible());
                assert!(change.t_monotonic_raw_ns >= start.t_monotonic_raw_ns);
            }
            other => panic!("unexpected sidecar: {other:?}\n{text}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_writer_keeps_present_order_and_writes_nothing_after_the_summary() {
        let dir = std::env::temp_dir().join(format!("limina-framecap-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(limina_framecap::SIDECAR);
        let (tx, rx) = std::sync::mpsc::channel();
        let file = std::fs::File::create(&path).unwrap();
        let writer = std::thread::spawn(move || write_sidecar(rx, file));
        let rec = |seq| Record {
            seq: Some(seq),
            flip: seq,
            file: Some(limina_framecap::file_name(0, seq)),
            ..Record::default()
        };
        tx.send(Msg::Done(1, rec(2), 10)).unwrap();
        tx.send(Msg::Done(0, rec(1), 10)).unwrap();
        // Number 2 never finishes before the stop gives up.
        tx.send(Msg::Done(3, rec(4), 10)).unwrap();
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        tx.send(Msg::Close {
            duration: 1.0,
            incomplete: Some(1),
            reply,
        })
        .unwrap();
        let summary = answer.recv().unwrap();
        assert_eq!((summary.captured, summary.incomplete), (3, Some(1)));
        // It finishes late, and must not land after the summary.
        tx.send(Msg::Done(2, rec(3), 10)).unwrap();
        drop(tx);
        writer.join().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text
            .lines()
            .map(|l| limina_framecap::parse_line(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3, "{text}");
        let seqs: Vec<_> = lines[..2]
            .iter()
            .map(|l| match l {
                limina_framecap::Line::Frame(r) => r.seq,
                limina_framecap::Line::Summary(_) | limina_framecap::Line::HostState(_) => None,
            })
            .collect();
        assert_eq!(seqs, [Some(1), Some(2)]);
        assert!(matches!(lines[2], limina_framecap::Line::Summary(_)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
