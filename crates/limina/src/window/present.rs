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

/// How many pool slots the supervisor tracks. `VIRTIO_GPU_MAX_SCANOUTS`, the same ceiling the
/// worker's backend enforces — the supervisor does not link the worker, so the constant is
/// restated rather than shared. A slot costs a few words until a connector uses it.
pub(crate) const MAX_SCANOUTS: usize = 16;

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
#[derive(Clone)]
pub(crate) enum AckMsg {
    /// The presented frame's surface id + the surface it replaced.
    Shown(u32, Option<SendSurface>),
    /// We could not resolve `id` and the guest is still presenting it: ask the worker to publish
    /// it again. Rides the same socket as `Shown` because the *answer* must ride the surface port
    /// — one FIFO queue is what makes IOSurface id recycling safe — and this is only the request.
    /// See [`SurfaceStore::request_resurface`].
    Resurface(u32),
}

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
    /// Ids we have asked the worker to re-publish and have not seen come back, with when we
    /// asked. The guest presents a missing id at 60 Hz, so without this every frame would put a
    /// request on the wire while the first one is still in flight.
    pending_resurface: std::collections::HashMap<u32, std::time::Instant>,
    /// The surface each slot's CURRENT cursor image lives in, as named by the worker's `cursor`
    /// control line. **Never evicted, and never dropped on a release.**
    ///
    /// A cursor surface is republished only when the guest changes shape, which it may not do
    /// for minutes — so unlike a scanout, losing one is not a frame skipped but a pointer that
    /// is simply gone until the user happens to hover something that changes it (dogfood
    /// 2026-08-26: no pointer from boot+12 s, unhealable by grab, workspace or click).
    ///
    /// The release guard is not belt-and-braces: the worker never releases a cursor surface, it
    /// only replaces one. A release naming the current cursor id therefore describes a surface
    /// that is not ours — an IOSurface id is reusable the moment its surface dies, and the
    /// release path carries the id the guest's resource was created with.
    cursors: [Option<u32>; MAX_SCANOUTS],
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

/// How long a re-publish request is considered in flight. Long enough that a healthy round trip
/// (a line on the ack socket, a registry lookup, a Mach send) lands well inside it; short enough
/// that a request lost to a worker relaunch is retried within a few frames rather than never.
const RESURFACE_RETRY: std::time::Duration = std::time::Duration::from_millis(250);

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
            pending_resurface: std::collections::HashMap::new(),
            cursors: [None; MAX_SCANOUTS],
        }
    }

    pub(crate) fn insert(&mut self, id: u32, surface: CFRetained<IOSurfaceRef>) {
        // Whatever brought this surface in — a fresh publish or an answer to our request — the
        // id is resolvable again, so any request we were holding down is finished.
        self.pending_resurface.remove(&id);
        if self.map.insert(id, SendSurface(surface)).is_none() {
            log::debug!(
                "surface store: published surface {id} ({} held)",
                self.map.len()
            );
            self.order.push_back(id);
            self.evict_to_cap();
        }
    }

    /// Evict oldest-first until the cap is met, **skipping ids the guest is presenting** and the
    /// slots' current cursor images ([`Self::cursors`] — a cursor is republished only on a shape
    /// change, so evicting one leaves the pointer gone, not one frame skipped). Those are moved
    /// to the back rather than dropped: they are the hot set, and evicting one freezes the
    /// display for it. If everything left is protected we stop and go over the cap — a bounded
    /// overshoot (`PINNED_CAP` + one cursor per slot) is strictly better than a frozen screen.
    fn evict_to_cap(&mut self) {
        let mut skipped = 0usize;
        while self.order.len() > self.cap && skipped < self.order.len() {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if self.pinned.contains(&old) || self.is_cursor(old) {
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

    /// Should we ask the worker to re-publish `id`? True at most once per [`RESURFACE_RETRY`],
    /// per id.
    ///
    /// The store is bounded and the worker's scanout IOSurfaces are **non-global** — capability
    /// scoped, handed over by Mach port — so once one leaves the store, `IOSurfaceLookup` cannot
    /// bring it back and only the process that created it can mint a port for it. Asking is the
    /// only recovery there is; without it a dropped id is a permanent freeze
    /// (`spikes/scanout-blob-freeze/RESULTS.md`).
    pub(crate) fn request_resurface(&mut self, id: u32) -> bool {
        let now = std::time::Instant::now();
        match self.pending_resurface.get(&id) {
            Some(asked) if now.duration_since(*asked) < RESURFACE_RETRY => false,
            _ => {
                self.pending_resurface.insert(id, now);
                true
            }
        }
    }

    /// The guest is presenting `id` right now: protect it from eviction. Idempotent, and bounded
    /// at [`PINNED_CAP`] so the protection cannot grow without limit.
    ///
    /// `LIMINA_PIN_PRESENTED=0` disables the protection. It exists because the pin is a *first*
    /// line of defence and the re-publish recovery behind it is otherwise unreachable — with the
    /// pin on, no workload evicts a presented surface, so nothing ever exercises the path that
    /// heals one. Turning it off is how [`request_resurface`](Self::request_resurface) gets tested
    /// against a real guest.
    pub(crate) fn pin_presented(&mut self, id: u32) {
        static PIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*PIN.get_or_init(|| std::env::var("LIMINA_PIN_PRESENTED").as_deref() != Ok("0")) {
            return;
        }
        if self.pinned.back() == Some(&id) {
            return;
        }
        self.pinned.retain(|&p| p != id);
        self.pinned.push_back(id);
        while self.pinned.len() > PINNED_CAP {
            self.pinned.pop_front();
        }
    }

    /// `slot`'s cursor image now lives in `id` — the worker just published it. Protects the
    /// surface from eviction and from a release naming a recycled id; see [`Self::cursors`].
    pub(crate) fn pin_cursor(&mut self, slot: usize, id: u32) {
        if let Some(c) = self.cursors.get_mut(slot) {
            *c = Some(id);
        }
    }

    /// Is `id` some slot's current cursor image?
    fn is_cursor(&self, id: u32) -> bool {
        self.cursors.contains(&Some(id))
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
        // A release naming a slot's CURRENT cursor cannot be about that surface: the worker
        // replaces cursor surfaces, it never releases one. What it does release is the IOSurface
        // a guest resource was created with — an id that is reusable the moment that surface
        // dies, and may by then belong to our cursor. Acting on it drops the pointer for as long
        // as the guest keeps the same shape. See [`Self::cursors`].
        if self.is_cursor(id) {
            log::warn!(
                "surface store: ignoring a release for {id} — it is a slot's current cursor \
                 image, which the worker never releases, so this release names a recycled id"
            );
            return;
        }
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
        self.cursors = [None; MAX_SCANOUTS];
    }

    /// Drop everything the previous worker published, queueing the ids so the main thread drops
    /// them from its frame cache too. Plain [`clear`](Self::clear) would leave the cache holding
    /// the same framebuffers by another route.
    pub(crate) fn clear_for_new_worker(&mut self) {
        self.released.extend(self.map.keys().copied());
        self.map.clear();
        self.order.clear();
        // The cursor ids belonged to the dead worker; the fresh one publishes its own, and
        // holding these would guard ids that can never come back.
        self.cursors = [None; MAX_SCANOUTS];
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

/// What one guest scanout is showing. There is one of these per pool slot: the guest's
/// connectors are independent displays, and a frame arriving for slot 1 must not disturb the
/// window presenting slot 0.
#[derive(Default)]
pub struct SlotPresent {
    /// The surface id the worker wants shown right now (alternates between its double
    /// buffer so the layer's contents object changes and Core Animation re-reads).
    pub(crate) show_id: Option<u32>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Bumped on any update (new surface geometry or a new frame) — the timer re-applies.
    pub(crate) gen: u64,
    /// Count of *presented frames* (`frame` messages), as opposed to `gen`, which also bumps
    /// on surface geometry. The restore overlay comes down on the first real frame, not on
    /// the fresh worker's surface announcement (which would flash black under the spinner).
    pub(crate) frames: u64,
    /// This scanout's hardware cursor.
    ///
    /// Per slot because the guest enables its cursor plane on one CRTC at a time and disables
    /// the others: a single VM-wide block meant the `cursorhide` for the display the pointer
    /// *left* blanked the shape while the display it arrived on was showing one, so the pointer
    /// vanished while still hit-testing correctly.
    pub(crate) cursor: CursorState,
}

/// One scanout's hardware-cursor state.
///
/// In the normal (absolute) path the host pointer *adopts* this shape over the guest view (see
/// `HostCursor`) and guest-reported positions are ignored — the pointer the user sees is the
/// host one, which the guest tracks via absolute input. In **pointer-capture** mode that no
/// longer holds (the host cursor is grabbed away from the guest position), so the image is
/// composited at `pos_x`/`pos_y`.
/// The last few things that changed one slot's cursor visibility, newest last.
///
/// Nothing reads this in the steady state. It exists because the flags that decide whether the
/// captured cursor is drawn (`visible`, `id`, the slot's geometry) are written from four
/// different guest messages, and when the layer goes silently blank the question is always
/// *which of them last spoke* — a question a `warn`-level log cannot answer after the fact.
/// `Copy` and fixed-size, because [`CursorState`] is copied out of the lock every tick.
#[derive(Clone, Copy, Default)]
pub struct CursorLog {
    /// `(what, value, ms since start)`, oldest first once it has wrapped.
    entries: [Option<(&'static str, u32, f64)>; CURSOR_LOG],
    next: usize,
}

pub(crate) const CURSOR_LOG: usize = 4;

impl CursorLog {
    fn record(&mut self, what: &'static str, value: u32) {
        self.entries[self.next] = Some((what, value, super::capture_tap::trace_ms()));
        self.next = (self.next + 1) % CURSOR_LOG;
    }

    /// Oldest first, for a one-line rendering at the fault.
    pub(crate) fn recent(&self) -> Vec<String> {
        (0..CURSOR_LOG)
            .filter_map(|i| self.entries[(self.next + i) % CURSOR_LOG])
            .map(|(what, value, at)| format!("{what}={value}@{at:.0}ms"))
            .collect()
    }
}

#[derive(Clone, Copy, Default)]
pub struct CursorState {
    pub(crate) id: Option<u32>,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) hot_x: u32,
    pub(crate) hot_y: u32,
    pub(crate) visible: bool,
    /// Guest-reported position (virtio-gpu `MoveCursor`), in that scanout's pixels.
    pub(crate) pos_x: i32,
    pub(crate) pos_y: i32,
    /// Bumped on any shape/visibility change — the timer re-applies.
    pub(crate) gen: u64,
    /// Who last changed the flags above ([`CursorLog`]).
    pub(crate) log: CursorLog,
}

/// State shared between the control-channel reader thread, the worker monitor, and the
/// main-thread render timer. Only `Send` data — never AppKit objects.
#[derive(Default)]
pub struct Shared {
    /// Per-scanout present state, indexed by pool slot. Slot 0 is the display every VM has;
    /// the rest are live only while their connector is.
    pub(crate) slots: [SlotPresent; MAX_SCANOUTS],
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
    /// Whether a guest OS driver has taken the GPU over from boot firmware (the worker's
    /// `guestdriver` line). Firmware paints head 0 and only head 0, so before this the pool is
    /// not ours to arrange; after it, each host panel can own a slot. Cleared on a reboot
    /// relaunch by [`mark_worker_running`], because the fresh worker starts in firmware again.
    pub(crate) guest_driver_ready: bool,
    /// PCM lifecycle transitions the worker has reported and the main thread has not yet fed to
    /// the media-session policy (`super::media_policy`). A queue rather than a latest-state flag
    /// because the transitions themselves are the signal — a start and a stop inside one tick is
    /// a track change, and collapsing it to "stopped" would retire a session that is still
    /// playing.
    pub(crate) audio_events: Vec<(u32, super::media_policy::PcmEvent)>,
    /// A fresh worker was swapped in and nothing has been said to it yet: the device is back to
    /// how it boots, and everything the host believes it told the old one is a lie. Taken once
    /// by the window tick, which re-asserts the arrangement (`DisplayTable::reset_connectors_to_boot`)
    /// and forgets what it thinks the device has been sent.
    pub(crate) device_fresh: bool,
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

/// A fresh worker's endpoints have been swapped in — a guest reboot (`resuming == false`) or a
/// resume from a snapshot. Called **before** the new worker's reader starts, so nothing it says
/// can be wiped by this.
///
/// The slots describe the OLD worker's connectors, and nothing will ever retract them: the new
/// worker only announces what the guest brings up, and a `scanoutgone` for a connector it never
/// knew about cannot arrive. Left stale, a slot above 0 that was live before the relaunch reopens
/// a window whose `show_id` resolves to a dead surface — a black window nothing can close.
///
/// The firmware/OS phase is the other half, and it splits by *why* the worker is fresh. A reboot
/// starts at firmware again, which paints head 0 and only head 0, so the phase must go back. A
/// resume restores a guest whose driver is already up: no second `GET_EDID` is coming, so
/// clearing the phase there would strand the arrangement at "firmware" for the rest of the run.
pub fn mark_worker_swapped(shared: &Arc<Mutex<Shared>>, resuming: bool) {
    let mut s = shared.lock().unwrap();
    s.slots = Default::default();
    // The connectors are the same story as the slots above, one layer up: the fresh device has
    // only slot 0 up, and it was never told any of the identities, modes or positions the host
    // has been remembering as sent. On a REBOOT the phase change catches this on its way back
    // through firmware; a RESUME keeps its phase, so without this the arrangement is never said
    // again and the guest stays on the slot the device happened to boot (the 2026-08-22 stuck
    // resume — docs/hardening-backlog.md §M9 snapshot hardening).
    s.device_fresh = true;
    if !resuming {
        s.guest_driver_ready = false;
        // A reboot is a different guest session: its compositor will report an arrangement of
        // its own, and until it does, keeping the old one would map the pointer through a
        // layout nothing is displaying.
        super::arrangement::forget_guest_layout();
    }
    super::echo::forget();
}

/// Take the "a fresh worker is up and knows nothing" flag, if it is set. Take-once: the tick
/// re-asserts the arrangement on the pass that observes it, and must not do so every frame
/// afterwards.
pub(crate) fn take_device_fresh(shared: &Arc<Mutex<Shared>>) -> bool {
    let mut s = shared.lock().unwrap();
    std::mem::take(&mut s.device_fresh)
}

/// A resume respawn was abandoned before any swap (spawn/gateway failure): tell the window
/// so it can surface the failure instead of showing "Resuming…" over a worker that will
/// never arrive. Called by the monitor thread on its resume-relaunch error paths.
pub fn mark_resume_dead(shared: &Arc<Mutex<Shared>>) {
    shared.lock().unwrap().resume_dead = true;
}

/// Read the control channel on a background thread, updating `shared`. Consumes (owns) `fd` —
/// the supervisor's end of the control socketpair — and closes it when the reader hits EOF.
pub fn spawn_reader(fd: OwnedFd, shared: Arc<Mutex<Shared>>, surface_map: SurfaceMap) {
    std::thread::spawn(move || {
        let reader = BufReader::new(File::from(fd));
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("surface") => {
                    log::info!("window: <- {line}");
                    // surface <id0> <id1> <w> <h> [<scanout>] — geometry + the initial buffer.
                    let id0 = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let _id1 = parts.next();
                    let w = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let slot = parse_slot(parts.next());
                    if let (Some(id0), Some(w), Some(h), Some(slot)) = (id0, w, h, slot) {
                        super::echo::publish_scanout(slot, w, h);
                        let mut s = shared.lock().unwrap();
                        let slot = &mut s.slots[slot];
                        slot.show_id = Some(id0);
                        slot.width = w;
                        slot.height = h;
                        slot.cursor.log.record("scanout", w);
                        slot.gen += 1;
                        drop(s);
                        wake_main_apply();
                    }
                }
                Some("frame") => {
                    // frame <id> [<scanout>] — the buffer to show now, on that slot.
                    let id = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let slot = parse_slot(parts.next());
                    if let (Some(id), Some(slot)) = (id, slot) {
                        let mut s = shared.lock().unwrap();
                        let slot = &mut s.slots[slot];
                        slot.show_id = Some(id);
                        slot.gen += 1;
                        slot.frames += 1;
                        drop(s);
                        wake_main_apply();
                    }
                }
                Some("scanoutgone") => {
                    // scanoutgone <scanout> — the slot has no scanout any more.
                    if let Some(slot) = parse_slot(parts.next()) {
                        super::echo::publish_scanout(slot, 0, 0);
                        let mut s = shared.lock().unwrap();
                        let slot = &mut s.slots[slot];
                        slot.show_id = None;
                        slot.width = 0;
                        slot.height = 0;
                        slot.gen += 1;
                        slot.cursor.log.record("scanoutgone", 0);
                        drop(s);
                        wake_main_apply();
                    }
                }
                Some("audio") => {
                    // audio <stream> <prepare|start|stop|release> — the guest moved a virtio-snd
                    // PCM stream. What it means for the VM's media session is main-thread policy;
                    // the reader only queues it.
                    let stream = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let event = parts.next().and_then(super::media_policy::PcmEvent::parse);
                    if let (Some(stream), Some(event)) = (stream, event) {
                        log::info!("window: <- {line}");
                        shared.lock().unwrap().audio_events.push((stream, event));
                        wake_main_apply();
                    }
                }
                Some("guestdriver") => {
                    // The guest's OS driver took the device over from boot firmware. Until this
                    // arrives, slot 0 is the only scanout anything paints (EDK2's GOP driver
                    // programs head 0 and no other), so display arrangement waits for it.
                    log::info!("window: <- {line}");
                    let mut s = shared.lock().unwrap();
                    s.guest_driver_ready = true;
                    drop(s);
                    wake_main_apply();
                }
                Some("cursor") => {
                    // cursor <id> <w> <h> <hot_x> <hot_y> [<scanout>] — image + hotspot.
                    let id = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let w = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let hx = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let hy = parts.next().and_then(|s| s.parse::<u32>().ok());
                    let slot = parse_slot(parts.next());
                    if let (Some(id), Some(w), Some(h), Some(hx), Some(hy), Some(slot)) =
                        (id, w, h, hx, hy, slot)
                    {
                        let mut s = shared.lock().unwrap();
                        let c = &mut s.slots[slot].cursor;
                        c.id = Some(id);
                        c.w = w;
                        c.h = h;
                        c.hot_x = hx;
                        c.hot_y = hy;
                        c.visible = true;
                        c.gen += 1;
                        c.log.record("shape", id);
                        // Protect the surface for as long as it is the shape being worn: the
                        // guest may not change cursor again for minutes, and this is the only
                        // moment we learn which id that is. See `SurfaceStore::cursors`.
                        surface_map.lock().unwrap().pin_cursor(slot, id);
                        if super::capture_tap::edge_trace() {
                            eprintln!(
                                "[CURSOR] t={:.1} slot={slot} shape id={id} {w}x{h} hot=({hx},{hy}) pos=({},{})",
                                super::capture_tap::trace_ms(),
                                c.pos_x,
                                c.pos_y,
                            );
                        }
                        publish_echo(&s, slot);
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
                    let slot = parse_slot(parts.next());
                    if let (Some(x), Some(y), Some(slot)) = (x, y, slot) {
                        let mut s = shared.lock().unwrap();
                        let c = &mut s.slots[slot].cursor;
                        c.pos_x = x;
                        c.pos_y = y;
                        let visible = c.visible;
                        publish_echo(&s, slot);
                        drop(s);
                        if super::capture_tap::edge_trace() {
                            eprintln!(
                                "[CURSOR] t={:.1} slot={slot} move ({x},{y}) visible={visible}",
                                super::capture_tap::trace_ms(),
                            );
                        }
                    }
                }
                Some("cursorhide") => {
                    if let Some(slot) = parse_slot(parts.next()) {
                        let mut s = shared.lock().unwrap();
                        let c = &mut s.slots[slot].cursor;
                        c.visible = false;
                        c.gen += 1;
                        c.log.record("hide", 0);
                        publish_echo(&s, slot);
                        drop(s);
                        if super::capture_tap::edge_trace() {
                            eprintln!(
                                "[CURSOR] t={:.1} slot={slot} hide",
                                super::capture_tap::trace_ms(),
                            );
                        }
                    }
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

/// Hand one slot's cursor state to the pointer stack's oracle (`window/echo.rs`).
fn publish_echo(s: &Shared, slot: usize) {
    let sp = &s.slots[slot];
    let c = sp.cursor;
    super::echo::publish(
        slot,
        c.visible,
        (c.pos_x, c.pos_y),
        (c.hot_x, c.hot_y),
        (sp.width, sp.height),
    );
}

/// Parse the optional trailing scanout id on a `surface`/`frame` line. Absent means slot 0, so
/// a worker that predates the pool — or a log from one — reads correctly. A slot past the pool
/// is dropped rather than clamped: routing a stray frame onto slot 0's window would show one
/// display's pixels on another, which is worse than showing nothing.
fn parse_slot(field: Option<&str>) -> Option<usize> {
    match field {
        None => Some(0),
        Some(s) => s.parse::<usize>().ok().filter(|n| *n < MAX_SCANOUTS),
    }
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

    /// **The no-pointer fault, at unit scale** (dogfood 2026-08-26). A slot's current cursor
    /// image must survive the cap. It is not a frame that gets skipped: the worker republishes a
    /// cursor only when the guest changes shape, so a guest holding one arrow has no pointer at
    /// all until it happens to change it — minutes, and unreachable by anything the user does
    /// with the grab or the window.
    #[test]
    fn a_cursor_surface_survives_eviction_pressure() {
        let mut store = SurfaceStore::with_cap(2);
        store.insert(50, make_surface());
        store.pin_cursor(0, 50);

        for id in 100..110 {
            store.insert(id, make_surface());
        }

        assert!(
            store.get(50).is_some(),
            "surface 50 is slot 0's cursor and was evicted anyway — this is the missing pointer"
        );
    }

    /// A release naming the current cursor describes a surface that is not ours. The worker
    /// replaces cursor surfaces and never releases one; what it releases is the IOSurface a
    /// guest resource was created with, and an IOSurface id is reusable the moment its surface
    /// dies. Acting on such a release takes the pointer away until the next shape change.
    #[test]
    fn a_release_naming_the_current_cursor_is_ignored() {
        let mut store = SurfaceStore::with_cap(4);
        store.insert(50, make_surface());
        store.pin_cursor(0, 50);

        store.note_released(50);

        assert!(
            store.get(50).is_some(),
            "a recycled-id release dropped the live cursor surface"
        );
        assert!(
            store.take_released().is_empty(),
            "the ignored release must not reach the main thread's frame cache either"
        );
        assert_eq!(
            store.why_gone(50),
            None,
            "the surface never left, so nothing may be recorded against it"
        );

        // Once the guest changes shape the old id is ordinary again, and a release for it lands.
        store.pin_cursor(0, 51);
        store.note_released(50);
        assert!(
            store.get(50).is_none(),
            "a superseded cursor id is releasable"
        );
    }

    /// A resolve miss asks the worker to re-publish the surface — but the guest presents at
    /// 60 Hz, so an unthrottled request would put one line per frame on the ack socket while the
    /// re-publish is still in flight. Ask once, then stay quiet until the request has had time to
    /// land or fail.
    #[test]
    fn resurface_requests_are_throttled_per_id() {
        let mut store = SurfaceStore::with_cap(2);
        assert!(store.request_resurface(9), "the first miss must ask");
        assert!(
            !store.request_resurface(9),
            "the next frame's miss must not ask again — the first request is still in flight"
        );
        assert!(
            store.request_resurface(10),
            "a different id is a different request"
        );
    }

    /// Once the worker hands the surface back, the id is resolvable again — and if it is later
    /// lost a second time, the throttle must not still be holding the old request down.
    #[test]
    fn a_republished_surface_clears_its_pending_request() {
        let mut store = SurfaceStore::with_cap(4);
        assert!(store.request_resurface(9));
        store.insert(9, make_surface());
        assert!(store.get(9).is_some());
        store.note_released(9);
        let _ = store.take_released();
        assert!(
            store.request_resurface(9),
            "the surface came back and went again — this is a new request, not the old one"
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
