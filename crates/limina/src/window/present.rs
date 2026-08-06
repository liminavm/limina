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

use limina_surfaceport::SurfacePortReceiver;

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
#[derive(Default)]
pub struct SurfaceStore {
    map: std::collections::HashMap<u32, SendSurface>,
    order: VecDeque<u32>,
}

const SURFACE_STORE_CAP: usize = 32;

impl SurfaceStore {
    pub(crate) fn insert(&mut self, id: u32, surface: CFRetained<IOSurfaceRef>) {
        if self.map.insert(id, SendSurface(surface)).is_none() {
            self.order.push_back(id);
            while self.order.len() > SURFACE_STORE_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
    pub(crate) fn get(&self, id: u32) -> Option<CFRetained<IOSurfaceRef>> {
        self.map.get(&id).map(|s| s.0.clone())
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
            Ok((id, surface)) => map.lock().unwrap().insert(id, surface),
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
    use super::parse_cursor_coord;

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
