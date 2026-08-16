// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The present path: the surface store + Mach-port rendezvous, the control-channel reader
//! thread (line protocol), the shared present state, the main-thread frame-apply wake-up,
//! and the shown-ack-aware layer present.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::runtime::AnyObject;
use objc2_core_foundation::CFRetained;
use objc2_io_surface::IOSurfaceRef;
use objc2_quartz_core::{CALayer, CATransaction};

use limina_surfaceport::{SurfaceMsg, SurfacePortReceiver};

/// A retained IOSurface that can cross threads. IOSurface is thread-safe — the kernel object is
/// atomically refcounted and designed for cross-process/-thread scanout sharing — but objc2
/// conservatively leaves `CFRetained<IOSurfaceRef>` `!Send`/`!Sync`. The recv thread stores
/// surfaces here and the main thread reads them, so we assert the safety explicitly.
#[derive(Clone)]
pub(crate) struct SendSurface(CFRetained<IOSurfaceRef>);
// SAFETY: IOSurface refcounting + access is thread-safe (Apple's cross-process scanout primitive).
unsafe impl Send for SendSurface {}
unsafe impl Sync for SendSurface {}

impl SendSurface {
    pub(crate) fn new(surface: CFRetained<IOSurfaceRef>) -> Self {
        Self(surface)
    }
    /// Whether any process (here: the window server compositing it) holds the surface in use.
    pub(crate) fn is_in_use(&self) -> bool {
        self.0.is_in_use()
    }
}

/// Shown-ack message: the presented frame's surface id + the surface this frame REPLACED on
/// the layer (`None` on the first frame or a same-surface re-flush). The ack sender holds the
/// ack until the replaced surface leaves window-server use — the true off-glass boundary the
/// worker's fence completion stands on (#24: the CATransaction completion block alone fires
/// ~one refresh BEFORE WindowServer stops sampling the old buffer; see
/// spikes/present-pacing/).
pub(crate) type AckMsg = (u32, Option<SendSurface>);

/// Scanout/cursor IOSurfaces the worker handed us by Mach port, keyed by `IOSurfaceGetID`. The
/// present + cursor paths resolve ids here first (the non-global, capability-scoped surfaces),
/// falling back to `IOSurfaceLookup` only for the venus zero-copy path (still global) and the
/// legacy no-receiver mode. Bounded and oldest-evicted: the worker only ever shows the current
/// ring and cursor, so superseded ids are safe to drop (a stale id falls back to lookup, which
/// fails for a freed non-global surface, so that frame is skipped rather than shown wrong).
pub struct SurfaceStore {
    map: std::collections::HashMap<u32, SendSurface>,
    order: VecDeque<u32>,
    cap: usize,
    /// Ids the worker has released, waiting for the main thread to drop them from its own frame
    /// cache. Carried here rather than in a second channel so the release needs no new plumbing:
    /// the store is already shared between the receive thread and the main thread.
    released: Vec<u32>,
    /// Diagnostic-only: why ids left the map, most recent last, bounded. Drained `released` cannot
    /// answer "why is this id unresolvable?" because the main thread takes it every frame; this
    /// survives so a failed resolve can name its own cause. See
    /// `spikes/scanout-blob-freeze/RESULTS.md` — a guest presenting an id that has left the map
    /// gets every later frame skipped, and both exit paths logged nothing at all, so the fault was
    /// invisible from the host and unreachable from the guest.
    ///
    /// There are exactly **two** ways an id leaves: the worker says the guest released it, or the
    /// cap evicts it. Recording which is what makes the failure self-diagnosing — "unresolved"
    /// alone does not distinguish a release we acted on too eagerly from a cap that is too small,
    /// and those have opposite fixes.
    gone: VecDeque<(u32, GoneReason)>,
    /// Ids the guest is currently presenting, most recent last. **Never evicted.** A compositor
    /// publishes its permanent scanout ring first and then churns client buffers forever, so
    /// FIFO-by-insertion evicts precisely the surfaces in continuous use — and a non-global
    /// surface that leaves the store has no `IOSurfaceLookup` to fall back on, so the display
    /// freezes for that id permanently. See `spikes/scanout-blob-freeze/RESULTS.md`.
    ///
    /// Bounded, so a guest that presents a fresh id every frame cannot pin the store open and
    /// re-create the retention leak the cap exists to stop.
    pinned: VecDeque<u32>,
}

/// Why an id left the store. See [`SurfaceStore::gone`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GoneReason {
    /// The worker reported the guest let the resource go.
    Released,
    /// The cap pushed it out to bound the supervisor's hold on host memory.
    Evicted,
}

/// How many actively-presented ids to protect from eviction. A swapchain ring is 2–4; this holds
/// a full ring plus a transition without letting a pathological guest pin the store open.
const PINNED_CAP: usize = 6;

/// How many departed ids to remember for diagnostics. Only needs to outlive the gap between a
/// departure and the frames that trip over it, which is one frame in the observed fault.
const GONE_HISTORY_CAP: usize = 64;

const SURFACE_STORE_CAP: usize = 32;

/// Cap for the main-thread frame-apply cache (see [`SurfaceStore::get_or_insert_with`]). Smaller
/// than the store's: the cache only saves a mutex + lookup per frame, a miss costs a re-resolve,
/// and the hot set is the guest's buffer ring (2–4 ids). Every retained entry is a whole
/// framebuffer — 14 MiB at 1440p — so the cap is what bounds the supervisor's hold on host memory.
pub(crate) const FRAME_CACHE_CAP: usize = 8;

impl Default for SurfaceStore {
    fn default() -> Self {
        Self::with_cap(SURFACE_STORE_CAP)
    }
}

impl SurfaceStore {
    pub(crate) fn with_cap(cap: usize) -> Self {
        Self {
            map: std::collections::HashMap::new(),
            order: VecDeque::new(),
            cap,
            released: Vec::new(),
            gone: VecDeque::new(),
            pinned: VecDeque::new(),
        }
    }

    pub(crate) fn insert(&mut self, id: u32, surface: CFRetained<IOSurfaceRef>) {
        if self.map.insert(id, SendSurface(surface)).is_none() {
            log::debug!(
                "surface store: published surface {id} ({} held)",
                self.map.len()
            );
            self.order.push_back(id);
            self.evict_to_cap();
        }
    }

    /// Evict oldest-first until the cap is met, **skipping ids the guest is presenting**. Those
    /// are moved to the back rather than dropped: they are the hot set, and evicting one freezes
    /// the display for it. If everything left is pinned we stop and go over the cap — a bounded
    /// overshoot (`PINNED_CAP` entries) is strictly better than a frozen screen.
    fn evict_to_cap(&mut self) {
        let mut skipped = 0usize;
        while self.order.len() > self.cap && skipped < self.order.len() {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if self.pinned.contains(&old) {
                self.order.push_back(old);
                skipped += 1;
                continue;
            }
            self.map.remove(&old);
            self.note_gone(old, GoneReason::Evicted);
            log::debug!(
                "surface store: evicted surface {old} at cap {} — if the guest still presents \
                 it, every later frame naming it is skipped",
                self.cap
            );
        }
    }

    /// The guest is presenting `id` right now: protect it from eviction. Idempotent, and bounded
    /// at [`PINNED_CAP`] so the protection cannot grow without limit.
    pub(crate) fn pin_presented(&mut self, id: u32) {
        if self.pinned.back() == Some(&id) {
            return;
        }
        self.pinned.retain(|&p| p != id);
        self.pinned.push_back(id);
        while self.pinned.len() > PINNED_CAP {
            self.pinned.pop_front();
        }
    }

    /// Record why an id left, for the failed-resolve diagnostic.
    fn note_gone(&mut self, id: u32, reason: GoneReason) {
        if self.gone.len() == GONE_HISTORY_CAP {
            self.gone.pop_front();
        }
        self.gone.push_back((id, reason));
    }

    /// Drop `id`. **Both structures**: leaving a stale entry in `order` would make the cap count
    /// phantom ids and evict *live* surfaces early.
    pub(crate) fn remove(&mut self, id: u32) {
        if self.map.remove(&id).is_some() {
            self.order.retain(|&o| o != id);
        }
    }

    /// The guest let go of `id`: drop our reference now, and queue the id for the main thread to
    /// drop from its frame cache too.
    ///
    /// Safe to act on immediately even if the surface is still on the layer — Core Animation and
    /// `last_ca` hold their own references, so dropping ours cannot pull a displayed frame out
    /// from under the window. And the guest will not present it again, so no later resolve can
    /// miss because of this.
    pub(crate) fn note_released(&mut self, id: u32) {
        let was_held = self.map.contains_key(&id);
        self.remove(id);
        self.released.push(id);
        self.note_gone(id, GoneReason::Released);
        log::debug!("surface store: worker released surface {id} (was_held={was_held})");
    }

    /// Diagnostic: why did `id` leave the store, if it did? Answers "why can I not resolve this?"
    /// at the point of failure. `None` means we never held it (or it left longer ago than the
    /// history keeps), which is a different fault from either departure.
    pub(crate) fn why_gone(&self, id: u32) -> Option<GoneReason> {
        self.gone
            .iter()
            .rev()
            .find(|(gid, _)| *gid == id)
            .map(|(_, r)| *r)
    }

    /// Take the ids released since the last call (main thread; purges the frame cache).
    pub(crate) fn take_released(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.released)
    }

    pub(crate) fn get(&self, id: u32) -> Option<CFRetained<IOSurfaceRef>> {
        self.map.get(&id).map(|s| s.0.clone())
    }

    /// Resolve `id`, caching the result; `resolve` runs only on a miss.
    ///
    /// This is the frame-apply path's cache, and its bound is load-bearing. It used to be a plain
    /// `HashMap` on the premise that "the worker reuses a small fixed set, its double buffer" — a
    /// compositor that mints a fresh scanout resource per frame breaks that premise, giving a
    /// fresh id every frame. Because IOSurface storage bills to the task that *created* it, the
    /// resulting unbounded retention showed up as 8.6 GB of `owned unmapped` in the WORKER while
    /// the references sat here, which is why every worker-side ledger came back balanced. See
    /// `spikes/venus-churn-retention/RESULTS.md` §0.3.
    pub(crate) fn get_or_insert_with(
        &mut self,
        id: u32,
        resolve: impl FnOnce() -> Option<CFRetained<IOSurfaceRef>>,
    ) -> Option<CFRetained<IOSurfaceRef>> {
        if let Some(surface) = self.get(id) {
            return Some(surface);
        }
        let surface = resolve()?;
        self.insert(id, surface.clone());
        Some(surface)
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    /// Drop everything the previous worker published, queueing the ids so the main thread drops
    /// them from its frame cache too. Plain [`clear`](Self::clear) would leave the cache holding
    /// the same framebuffers by another route.
    pub(crate) fn clear_for_new_worker(&mut self) {
        self.released.extend(self.map.keys().copied());
        self.map.clear();
        self.order.clear();
    }

    /// Retained surfaces currently held — the quantity the cap exists to bound.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

/// Shared handle to the [`SurfaceStore`] (worker recv thread writes, main-thread present reads).
pub type SurfaceMap = Arc<Mutex<SurfaceStore>>;

/// Run the surface-port receive loop on a background thread for the supervisor's whole life.
/// Survives worker relaunches: a relaunched worker re-looks-up the same bootstrap name and
/// re-sends its surfaces, which land in the same store.
fn spawn_surface_receiver(receiver: SurfacePortReceiver, map: SurfaceMap) {
    std::thread::spawn(move || loop {
        match receiver.recv(None) {
            Ok(SurfaceMsg::Published(id, surface)) => map.lock().unwrap().insert(id, surface),
            Ok(SurfaceMsg::Released(id)) => map.lock().unwrap().note_released(id),
            Err(e) => {
                log::warn!("window: surface-port recv failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    });
}

/// Set up the surface-port rendezvous: register a receiver under a per-process bootstrap name,
/// start its receive loop, and return the `(name, map)`. Pass `name` to the worker via
/// `--surface-port-name` (so it hands scanouts here instead of making them global) and `map` to
/// [`run`]. The receiver outlives worker relaunches. Falls back gracefully: on error the caller
/// runs without scoping (worker stays global), so this never blocks the VM.
pub fn surface_rendezvous() -> std::io::Result<(String, SurfaceMap)> {
    let name = format!("eti.noronha.limina.{}", std::process::id());
    let receiver = SurfacePortReceiver::register(&name)?;
    let map = empty_surface_map();
    spawn_surface_receiver(receiver, map.clone());
    Ok((name, map))
}

/// An empty surface store. Used as the fallback when the rendezvous fails: the present path then
/// resolves every id via the global `IOSurfaceLookup` fallback (legacy behavior).
pub fn empty_surface_map() -> SurfaceMap {
    Arc::new(Mutex::new(SurfaceStore::default()))
}

// libdispatch: wake the main thread to apply a frame the moment it arrives, instead of
// waiting out the 60 Hz poll timer (leg 1 of the present-latency collapse, #8). The
// trampoline runs on the main thread via the main queue, which NSApplication's run loop
// services; it calls the frame-apply hook `run()` registers.
#[allow(non_camel_case_types)]
type dispatch_queue_t = *mut std::ffi::c_void;
extern "C" {
    static _dispatch_main_q: std::ffi::c_void;
    fn dispatch_async_f(
        queue: dispatch_queue_t,
        context: *mut std::ffi::c_void,
        work: extern "C" fn(*mut std::ffi::c_void),
    );
}

thread_local! {
    /// Main-thread frame-apply hook (set once by `run()`); the dispatch trampoline calls it.
    static APPLY_HOOK: RefCell<Option<std::rc::Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

extern "C" fn apply_trampoline(_ctx: *mut std::ffi::c_void) {
    let hook = APPLY_HOOK.with(|h| h.borrow().clone());
    if let Some(f) = hook {
        f();
    }
}

/// Schedule an immediate frame apply on the main thread (callable from any thread).
fn wake_main_apply() {
    unsafe {
        let main_q = &_dispatch_main_q as *const _ as dispatch_queue_t;
        dispatch_async_f(main_q, std::ptr::null_mut(), apply_trampoline);
    }
}

/// Register the main-thread frame-apply hook (called once by [`super::run`]); the dispatch
/// trampoline invokes it whenever the reader thread wakes the main queue.
pub(crate) fn register_apply_hook(hook: std::rc::Rc<dyn Fn()>) {
    APPLY_HOOK.with(|h| *h.borrow_mut() = Some(hook));
}

/// State shared between the control-channel reader thread, the worker monitor, and the
/// main-thread render timer. Only `Send` data — never AppKit objects.
#[derive(Default)]
pub struct Shared {
    /// The surface id the worker wants shown right now (alternates between its double
    /// buffer so the layer's contents object changes and Core Animation re-reads).
    pub(crate) show_id: Option<u32>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Bumped on any update (new surface geometry or a new frame) — the timer re-applies.
    pub(crate) gen: u64,
    /// Set when the worker/control channel is gone — the window should close.
    pub(crate) worker_exited: bool,
    /// Set (before `worker_exited`) when the worker exited BY SUSPENDING (exit 126): the
    /// window's exit path saves the last-presented frame as the restore splash (M9.4).
    pub(crate) worker_suspended: bool,
    /// Bumped by [`mark_worker_running`] on every respawn swap. Lets the window tell "the
    /// OLD worker's exit flags haven't been cleared yet" (epoch unchanged since the play
    /// click) from "the FRESH resume worker died" (epoch advanced, exited set again) —
    /// the two states are otherwise identical during a resume.
    pub(crate) worker_epoch: u64,
    /// Set by the monitor thread when a resume respawn is abandoned before any swap
    /// (spawn or gateway-restart failure): the window must not wait for a worker that
    /// will never come.
    pub(crate) resume_dead: bool,
    /// Count of *presented frames* (`frame` messages), as opposed to `gen`, which also bumps
    /// on surface geometry. The restore overlay comes down on the first real frame, not on
    /// the fresh worker's surface announcement (which would flash black under the spinner).
    pub(crate) frames: u64,

    /// Guest hardware-cursor state (decoupled from the scanout above; the worker publishes
    /// the cursor image as its own IOSurface). In the normal (absolute) path the host pointer
    /// *adopts* this shape over the guest view (see `HostCursor`) and guest-reported positions
    /// are ignored — the pointer the user sees is the host one, which the guest tracks via
    /// absolute input. In **pointer-capture** mode that no longer holds (the host cursor is
    /// grabbed away from the guest position), so we composite this image at `cursor_pos_*`.
    pub(crate) cursor_id: Option<u32>,
    pub(crate) cursor_w: u32,
    pub(crate) cursor_h: u32,
    pub(crate) hot_x: u32,
    pub(crate) hot_y: u32,
    pub(crate) cursor_visible: bool,
    /// Guest-reported cursor position (virtio-gpu `MoveCursor`), in guest scanout pixels. Used
    /// only while captured — see `cursormove` handling and `update_capture_cursor`.
    pub(crate) cursor_pos_x: i32,
    pub(crate) cursor_pos_y: i32,
    /// Bumped on any cursor shape/visibility change — the timer re-applies.
    pub(crate) cursor_gen: u64,
}

impl Shared {
    pub fn new() -> Arc<Mutex<Shared>> {
        Arc::new(Mutex::new(Shared::default()))
    }
}

/// Mark the worker as gone (called by the monitor when the worker exits).
pub fn mark_worker_exited(shared: &Arc<Mutex<Shared>>) {
    shared.lock().unwrap().worker_exited = true;
}

/// Mark that the worker exited by SUSPENDING (exit 126). Call BEFORE
/// [`mark_worker_exited`] so the window's exit path sees both flags together.
pub fn mark_worker_suspended(shared: &Arc<Mutex<Shared>>) {
    shared.lock().unwrap().worker_suspended = true;
}

/// The parked window resumed: a fresh worker was respawned for the pending snapshot, so the
/// exit flags no longer describe the current worker (task #18). Called by the monitor thread
/// after the respawn's conn swap, mirroring the reboot relaunch (which never set them).
pub fn mark_worker_running(shared: &Arc<Mutex<Shared>>) {
    let mut s = shared.lock().unwrap();
    s.worker_exited = false;
    s.worker_suspended = false;
    s.worker_epoch += 1;
    s.resume_dead = false;
}

/// A resume respawn was abandoned before any swap (spawn/gateway failure): tell the window
/// so it can surface the failure instead of showing "Resuming…" over a worker that will
/// never arrive. Called by the monitor thread on its resume-relaunch error paths.
pub fn mark_resume_dead(shared: &Arc<Mutex<Shared>>) {
    shared.lock().unwrap().resume_dead = true;
}

/// Read the control channel on a background thread, updating `shared`. Consumes (owns) `fd` —
/// the supervisor's end of the control socketpair — and closes it when the reader hits EOF.
pub fn spawn_reader(fd: OwnedFd, shared: Arc<Mutex<Shared>>) {
    std::thread::spawn(move || {
        let reader = BufReader::new(File::from(fd));
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("surface") => {
                    log::info!("window: <- {line}");
                    // surface <id0> <id1> <w> <h> — geometry + the initial buffer to show.
                    let id0 = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let _id1 = parts.next();
                    let w = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
                    if let (Some(id0), Some(w), Some(h)) = (id0, w, h) {
                        let mut s = shared.lock().unwrap();
                        s.show_id = Some(id0);
                        s.width = w;
                        s.height = h;
                        s.gen += 1;
                        drop(s);
                        wake_main_apply();
                    }
                }
                Some("frame") => {
                    // frame <id> — the buffer to show now.
                    if let Some(id) = parts.next().and_then(|s| s.parse::<u32>().ok()) {
                        let mut s = shared.lock().unwrap();
                        s.show_id = Some(id);
                        s.gen += 1;
                        s.frames += 1;
                        drop(s);
                        wake_main_apply();
                    }
                }
                Some("cursor") => {
                    // cursor <id> <w> <h> <hot_x> <hot_y> — new cursor image + hotspot.
                    let id = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let w = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let hx = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let hy = parts.next().and_then(|s| s.parse::<u32>().ok());
                    if let (Some(id), Some(w), Some(h), Some(hx), Some(hy)) = (id, w, h, hx, hy) {
                        let mut s = shared.lock().unwrap();
                        s.cursor_id = Some(id);
                        s.cursor_w = w;
                        s.cursor_h = h;
                        s.hot_x = hx;
                        s.hot_y = hy;
                        s.cursor_visible = true;
                        s.cursor_gen += 1;
                    }
                }
                Some("cursormove") => {
                    // cursormove <x> <y> — the guest's cursor position (guest scanout pixels).
                    // In the absolute path this only echoes our own input back with a round-trip
                    // of lag, so the present path ignores it. But in pointer-CAPTURE mode the
                    // guest drives its own cursor (mouselook / warps) and the host cursor is
                    // grabbed away, so this *is* the only source of truth for where to draw the
                    // guest cursor — store it for `update_capture_cursor`.
                    let x = parts.next().and_then(parse_cursor_coord);
                    let y = parts.next().and_then(parse_cursor_coord);
                    if let (Some(x), Some(y)) = (x, y) {
                        let mut s = shared.lock().unwrap();
                        s.cursor_pos_x = x;
                        s.cursor_pos_y = y;
                    }
                }
                Some("cursorhide") => {
                    let mut s = shared.lock().unwrap();
                    s.cursor_visible = false;
                    s.cursor_gen += 1;
                }
                _ => {}
            }
        }
        // The control channel closed: this worker is gone. Do NOT set worker_exited here — the
        // worker *monitor* is the sole authority on that, because a guest reboot also closes this
        // channel and we must NOT quit the window before the monitor relaunches a new worker. On
        // a relaunch a fresh reader is spawned on the new channel; this thread just ends.
        log::info!("window: control channel closed (worker gone)");
    });
}

/// Parse a guest cursor coordinate. The virtio-gpu wire carries the position as **u32**, but a
/// cursor whose hotspot hangs past the scanout's left/top edge is legitimately *negative* —
/// the guest kernel just casts it (`move +1353+-2` in its own DRM debug), so `4294967294`
/// really means `-2`. Accept the signed form AND the wrapped-unsigned form; rejecting the
/// wrap (the old `parse::<i32>` alone) silently dropped every cursormove along those edges,
/// freezing the captured-mode cursor overlay there.
fn parse_cursor_coord(s: &str) -> Option<i32> {
    s.parse::<i32>()
        .ok()
        .or_else(|| s.parse::<u32>().ok().map(|v| v as i32))
}

/// Set an IOSurface as the layer's contents (it's a CF object accepted by `contents`).
///
/// Wrapped in a `CATransaction` with actions disabled: this is a layer-HOSTING layer, so a
/// `contents` change otherwise fires an implicit ~0.25 s fade. At 60 fps the fades overlap
/// and the guest desktop visibly flickers; disabling actions makes each frame swap instant.
pub(crate) fn set_layer_surface(
    layer: &CALayer,
    surface: &CFRetained<IOSurfaceRef>,
    ack: Option<(std::sync::mpsc::SyncSender<AckMsg>, AckMsg)>,
) {
    let obj: &AnyObject = unsafe { &*(&**surface as *const IOSurfaceRef as *const AnyObject) };
    unsafe {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // #8 leg 2: the completion block fires once Core Animation has processed this
        // transaction (the new contents latched) — the truthful "shown" boundary the
        // worker needs to complete the guest's held flush fence. The block only hands the id
        // to the dedicated ack-sender thread via a bounded, non-blocking try_send: the actual
        // socket write (which can block on a booting/wedged worker) must never run on the
        // AppKit main thread. A dropped ack (channel full) is covered by the worker's fallback
        // deadline.
        if let Some((tx, msg)) = ack {
            let cb = RcBlock::new(move || {
                let _ = tx.try_send(msg.clone());
            });
            CATransaction::setCompletionBlock(Some(&cb));
        }
        layer.setContents(Some(obj));
        CATransaction::commit();
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use objc2_core_foundation::{
        kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber,
        CFString,
    };
    use objc2_io_surface::{
        kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat,
        kIOSurfaceWidth, IOSurfaceCreate,
    };

    use super::{
        parse_cursor_coord, CFRetained, GoneReason, IOSurfaceRef, SurfaceStore, FRAME_CACHE_CAP,
    };

    fn cfnum(v: i32) -> CFRetained<CFNumber> {
        unsafe {
            CFNumber::new(
                None,
                objc2_core_foundation::CFNumberType::SInt32Type,
                &v as *const i32 as *const c_void,
            )
        }
        .unwrap()
    }

    /// A minimal 8×4 BGRA surface — the test cares about how many we retain, not their pixels.
    fn make_surface() -> CFRetained<IOSurfaceRef> {
        unsafe {
            let vw = cfnum(8);
            let vh = cfnum(4);
            let vbpe = cfnum(4);
            let vbpr = cfnum(32);
            let vpf = cfnum(i32::from_be_bytes(*b"BGRA"));
            let mut keys: [*const c_void; 5] = [
                (kIOSurfaceWidth as *const CFString).cast(),
                (kIOSurfaceHeight as *const CFString).cast(),
                (kIOSurfaceBytesPerElement as *const CFString).cast(),
                (kIOSurfaceBytesPerRow as *const CFString).cast(),
                (kIOSurfacePixelFormat as *const CFString).cast(),
            ];
            let mut values: [*const c_void; 5] = [
                (&*vw as *const CFNumber).cast(),
                (&*vh as *const CFNumber).cast(),
                (&*vbpe as *const CFNumber).cast(),
                (&*vbpr as *const CFNumber).cast(),
                (&*vpf as *const CFNumber).cast(),
            ];
            let dict = CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .unwrap();
            IOSurfaceCreate(&dict).expect("create surface")
        }
    }

    /// The 8.6 GB bug (spikes/venus-churn-retention §0.3): a compositor that mints a fresh
    /// scanout resource per frame hands the frame-apply path a fresh IOSurface id every frame.
    /// The cache retained one framebuffer per id and was cleared only on a mode change, so ten
    /// seconds of churn pinned ~620 surfaces. What must hold is that the retained set is bounded
    /// by the cap no matter how many distinct ids go through it.
    #[test]
    fn frame_cache_retains_a_bounded_set_under_per_frame_id_churn() {
        let mut cache = SurfaceStore::with_cap(FRAME_CACHE_CAP);
        for id in 0..500u32 {
            let surface = cache.get_or_insert_with(id, || Some(make_surface()));
            assert!(surface.is_some(), "id {id} should resolve");
            assert!(
                cache.len() <= FRAME_CACHE_CAP,
                "cache grew to {} after {} fresh ids (cap {FRAME_CACHE_CAP})",
                cache.len(),
                id + 1,
            );
        }
    }

    /// The cache must still be a cache: a repeated id resolves once and keeps returning the same
    /// surface, so the steady state (a guest cycling a small buffer ring) costs no re-resolves.
    #[test]
    fn frame_cache_resolves_a_repeated_id_once() {
        let mut cache = SurfaceStore::with_cap(FRAME_CACHE_CAP);
        let mut resolves = 0;
        let mut ptr = None;
        for _ in 0..10 {
            let surface = cache
                .get_or_insert_with(7, || {
                    resolves += 1;
                    Some(make_surface())
                })
                .unwrap();
            let this = &*surface as *const IOSurfaceRef;
            assert_eq!(
                *ptr.get_or_insert(this),
                this,
                "cache returned a new surface"
            );
        }
        assert_eq!(resolves, 1);
    }

    /// A release must drop the surface AND its slot in the eviction order. Removing from the map
    /// alone leaves a phantom id in `order`, and the cap then counts surfaces that are already
    /// gone — so an insert evicts a LIVE surface while the store is not even full, and the frame
    /// that surface belongs to gets skipped.
    ///
    /// It takes releasing a *newer* id to show this, which is also the realistic pattern: a guest
    /// frees the buffer it has just finished with, not its oldest. Release the oldest instead and
    /// the phantoms sit at the front of the queue and are popped harmlessly, so both the bug and
    /// the fix behave identically — an easy way to write a test that proves nothing.
    #[test]
    fn releasing_frees_the_slot_it_took() {
        let mut store = SurfaceStore::with_cap(3);
        for id in 0..3u32 {
            store.insert(id, make_surface());
        }
        store.note_released(2);
        assert_eq!(store.len(), 2, "the released surface must be gone");

        store.insert(10, make_surface());
        assert!(
            store.get(0).is_some(),
            "id 0 was evicted with only 2 live surfaces held under a cap of 3 — the released id \
             still held a slot"
        );
        assert_eq!(store.len(), 3);
        assert_eq!(store.take_released(), vec![2]);
        assert!(
            store.take_released().is_empty(),
            "taking the released list must drain it"
        );
    }

    /// **The scanout-blob freeze, at unit scale.** A surface the guest is actively presenting must
    /// survive the cap, however much unrelated churn arrives. FIFO-by-insertion evicts exactly
    /// backwards for a compositor: it publishes its permanent ring FIRST and then churns client
    /// buffers forever, so the oldest-inserted surfaces are the ones in continuous use. Losing one
    /// freezes the display permanently for that id, because non-global surfaces have no
    /// `IOSurfaceLookup` fallback. See `spikes/scanout-blob-freeze/RESULTS.md`.
    #[test]
    fn a_presented_surface_survives_eviction_pressure() {
        let mut store = SurfaceStore::with_cap(2);
        store.insert(7, make_surface());
        store.insert(12, make_surface());
        // The guest is scanning these two out, alternating, as a compositor ring does.
        store.pin_presented(7);
        store.pin_presented(12);

        // Now a client churns buffers past the cap — Firefox reallocating on fullscreen.
        for id in 100..110 {
            store.insert(id, make_surface());
        }

        assert!(
            store.get(7).is_some(),
            "surface 7 is being presented and was evicted anyway — this is the freeze"
        );
        assert!(
            store.get(12).is_some(),
            "surface 12 is being presented and was evicted anyway — this is the freeze"
        );
    }

    /// The failed-resolve diagnostic must name the *reason* an id left, because the two reasons
    /// have opposite fixes: a release acted on too eagerly vs. a cap that is too small. Getting
    /// "unresolved" with no reason is what made `spikes/scanout-blob-freeze` invisible for days.
    #[test]
    fn why_gone_distinguishes_a_release_from_an_eviction() {
        let mut store = SurfaceStore::with_cap(2);
        store.insert(1, make_surface());
        store.insert(2, make_surface());
        assert_eq!(
            store.why_gone(1),
            None,
            "still held, so it has not gone anywhere"
        );

        store.note_released(1);
        assert_eq!(store.why_gone(1), Some(GoneReason::Released));

        // Refill past the cap: 2 is the oldest survivor, so it is the one pushed out.
        store.insert(3, make_surface());
        store.insert(4, make_surface());
        assert_eq!(store.why_gone(2), Some(GoneReason::Evicted));
        assert!(store.get(2).is_none(), "an evicted id must really be gone");

        assert_eq!(
            store.why_gone(999),
            None,
            "an id we never held is a third case and must not be reported as either departure"
        );
    }

    /// Releasing an id we never held must not corrupt the bound. The worker unrefs plenty of
    /// resources whose surfaces we were never handed (or have already evicted), so this is the
    /// common case, not an edge one.
    #[test]
    fn releasing_an_unknown_id_is_a_no_op_on_the_bound() {
        let mut store = SurfaceStore::with_cap(2);
        store.insert(1, make_surface());
        store.note_released(999);
        store.insert(2, make_surface());
        assert!(store.get(1).is_some() && store.get(2).is_some());
    }

    #[test]
    fn cursor_coords_accept_plain_and_negative_values() {
        assert_eq!(parse_cursor_coord("1353"), Some(1353));
        assert_eq!(parse_cursor_coord("-2"), Some(-2));
        assert_eq!(parse_cursor_coord("0"), Some(0));
    }

    #[test]
    fn cursor_coords_unwrap_u32_cast_negatives_from_the_virtio_wire() {
        // The guest kernel casts a negative pos into the unsigned wire field: -2 arrives as
        // 4294967294. The old i32-only parse dropped these (froze the cursor at left/top
        // edges); they must decode back to the signed value.
        assert_eq!(parse_cursor_coord("4294967294"), Some(-2));
        assert_eq!(parse_cursor_coord(&u32::MAX.to_string()), Some(-1));
    }

    #[test]
    fn cursor_coord_garbage_is_rejected() {
        assert_eq!(parse_cursor_coord("nope"), None);
        assert_eq!(parse_cursor_coord("184467440737095516150"), None);
    }
}
