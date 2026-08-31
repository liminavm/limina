// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The collection of guest windows — every window presenting a pool slot, iterated by one
//! per-tick walk.
//!
//! [`GuestWindows`] owns the desktop: the PRIMARY (`run()`'s window, wearing its
//! [`PrimaryDisplay`] role state) and one [`SecondaryWindow`] per other connected slot,
//! opened and closed as connectors come and go. Every window runs the same walk each tick —
//! strip reconcile → refit → gen gate → modeset follow → present — through the shared
//! per-window core (`super::guestwindow::GuestWindow`), so a mechanism written for one
//! window exists on all of them; there is no "port to secondaries" step (Move A,
//! `docs/design/input-windows-restructure.md`).
//!
//! What "primary" still means, and where it lives: key/keyboard ownership, the menu, close
//! policy, park/lifecycle, `state.toml` persistence and the display control plane stay in
//! `run()`; the [`PrimaryDisplay`] role carries only the per-tick presentation extras that
//! exist once (the relaunch size, present/capture diagnostics). The primary window (and its
//! strip) is registered in [`WINDOW_SLOTS`] / [`SLOT_WINDOWS`] like every other guest window
//! ([`PrimaryDisplay::register`]), so `input`'s `target_of` decodes every guest window's
//! events through one path: the event window's own layer frame.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSBackingStoreType, NSScreen, NSTrackingArea, NSTrackingAreaOptions, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::CFRetained;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_io_surface::{IOSurfaceLookup, IOSurfaceRef};

use super::present::{AckMsg, Shared, SurfaceMap, MAX_SCANOUTS};
use crate::vmlib::schema::DisplayResolution;

thread_local! {
    /// Which slot each guest window — the primary, every secondary, and each window's
    /// `extend` STRIP window — is showing, keyed by the `NSWindow`'s address.
    ///
    /// The app has ONE local `NSEvent` monitor and it already receives events for every window
    /// — they were merely being decoded in the primary view's space, which is why a click on a
    /// second display went nowhere. So input needs no new plumbing here, only an answer to
    /// "which guest display is this window?", which is this map.
    static WINDOW_SLOTS: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());

    /// The inverse: the window showing each slot. Registered and deregistered together with
    /// [`WINDOW_SLOTS`] (same `open`, same `Drop`), so the two can never disagree. The captured
    /// pointer lives in the window showing its capture slot, and every per-event decision is
    /// judged in that window's space — this map is how the slot resolves to the window.
    static SLOT_WINDOWS: RefCell<HashMap<usize, Retained<NSWindow>>> =
        RefCell::new(HashMap::new());
}

thread_local! {
    /// Which secondary slots currently host the guest under an active extend overlay —
    /// written every tick by [`SecondaryWindow::reconcile_extend`], pruned by
    /// [`GuestWindows::apply`] when a slot's window closes. What
    /// `InputState::overlay_active_of` reads for non-primary slots: the chrome-reveal gesture
    /// only has something to ask past where a band is actually claimed.
    static BAND_ACTIVE: RefCell<HashMap<usize, bool>> = RefCell::new(HashMap::new());
}

/// Whether `slot`'s secondary window currently claims its panel's housing band.
pub(crate) fn band_active(slot: usize) -> bool {
    BAND_ACTIVE.with(|m| m.borrow().get(&slot).copied().unwrap_or(false))
}

/// The guest display a window is showing, or `None` if it is not a guest window's (every
/// guest window registers, the primary included; a strip resolves to its window's slot, so
/// band clicks land).
pub(crate) fn slot_of_window(window: &NSWindow) -> Option<usize> {
    let key = window as *const NSWindow as usize;
    WINDOW_SLOTS.with(|m| m.borrow().get(&key).copied())
}

/// The window showing this slot, plus the rect of the guest's picture within its content
/// view — the same layer-frame rect [`super::input`]'s `target_of` measures events against,
/// so the projection out and the decoding back in share one geometry. `None` for a slot no
/// window shows (one mid-close).
pub(crate) fn window_of_slot(slot: usize) -> Option<(Retained<NSWindow>, super::fit::FitRect)> {
    let window = SLOT_WINDOWS.with(|m| m.borrow().get(&slot).cloned())?;
    let rect = window
        .contentView()
        .and_then(|v| v.layer())
        .map(|l| l.frame())?;
    Some((
        window,
        super::fit::FitRect {
            x: rect.origin.x,
            y: rect.origin.y,
            w: rect.size.width,
            h: rect.size.height,
        },
    ))
}

/// Every slot a guest window is currently showing, the primary's included.
pub(crate) fn hosted_slots() -> Vec<usize> {
    SLOT_WINDOWS.with(|m| m.borrow().keys().copied().collect())
}

/// How the desktop is arranged this tick. The facts the collection cannot read for itself:
/// which panel owns each slot, the notch policy, the chrome ask, the display mode. (Which
/// slot the primary shows and whether the VM is fullscreen are the primary's own —
/// [`GuestWindows::apply`] reads them off its entry.)
pub(crate) struct Layout<'a> {
    /// Every connected slot and the host panel that owns it.
    pub(crate) panels: &'a HashMap<usize, u64>,
    /// What fullscreen does with a notched panel's camera housing.
    pub(crate) notch: crate::vmlib::schema::NotchPolicy,
    /// The slot whose chrome ask is granted, if any (`InputState::reveal_ask_slot`) — the
    /// slot whose strip must stand down so the menu bar and window controls are reachable.
    pub(crate) reveal_ask: Option<usize>,
    /// The VM's display mode. Every window follows the same fit rule: dynamic fills the
    /// usable box (the guest is, or will be, driven to its shape), host and fixed letterbox
    /// the guest's mode into it.
    pub(crate) mode: DisplayResolution,
}

/// One slot's present state, read out from under the shared lock in one pass.
struct SlotSnapshot {
    slot: usize,
    show_id: Option<u32>,
    width: u32,
    height: u32,
    gen: u64,
}

/// What this tick does with one slot's secondary window, after the dismissal pass has closed
/// the windows of slots the table no longer gives a panel.
///
/// Pure, because the rule it carries is the one that is otherwise observable only by booting
/// onto two panels and logging out: **a slot with no geometry is between modes, not gone.**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fate {
    /// No window, and nothing to open one for.
    Idle,
    /// Open a window for this slot, on the panel the table gave it.
    Open,
    /// The window stays, and shows the slot's frame.
    Show,
    /// The window stays — holding its panel, its style and its fullscreen — but the slot has
    /// nothing to present.
    Dark,
}

/// The lifetime decision for one slot, given whether it already has a window, whether the
/// table gives it a panel, and whether the guest currently has a mode on it.
fn slot_fate(open: bool, paneled: bool, live: bool) -> Fate {
    match (open, live) {
        (true, true) => Fate::Show,
        // A window that exists is NOT closed for a dark slot. The guest disables a scanout and
        // reconfigures it for every ordinary modeset — simpledrm → plymouth → gdm on the way
        // up, and again at every session handover — and `scanoutgone` is all we get either
        // way, so it says nothing about whether the connector is still there. Whether a slot
        // has one at all is the table's answer, and the dismissal pass is where it is acted
        // on (docs/graphics.md §"A panel owns a slot").
        (true, false) => Fate::Dark,
        // A slot whose connector is on its way down still has geometry until `scanoutgone`
        // lands, and a window opened in that gap would flash up for the settle and immediately
        // close. So: a panel AND a mode, or nothing.
        (false, true) if paneled => Fate::Open,
        (false, _) => Fate::Idle,
    }
}

/// The PRIMARY window's per-tick presentation state: the same walk every [`SecondaryWindow`]
/// runs — strip reconcile, refit, gen gate, modeset follow, present — plus the role state
/// that exists exactly once. "Primary" is this value attached to `run()`'s window, not a
/// different kind of window (Move A's collection inversion,
/// `docs/design/input-windows-restructure.md`). What stays OUTSIDE, with `run()`: key and
/// keyboard ownership, the menu, close policy, lifecycle/park, `state.toml` persistence, and
/// the display control plane (which panel owns which slot, what to push to the guest).
pub(crate) struct PrimaryDisplay {
    core: Rc<super::guestwindow::GuestWindow>,
    /// Which pool slot this window presents. Shared: the control plane assigns it per tick,
    /// the input path projects events through it.
    slot: Rc<Cell<u32>>,
    /// Last applied `gen`, so an unchanged slot costs nothing per tick.
    last_gen: Cell<u64>,
    /// Last applied guest geometry. Shared with the control plane, whose pushes gate on
    /// "the guest has presented" (`geom != (0, 0)`).
    geom: Rc<Cell<(u32, u32)>>,
    /// Last fit target the letterbox debug log reported — a trace edge detector like
    /// [`Self::geom_traced`], never consulted for behavior (the gate reads the LAYER,
    /// `GuestWindow::fit`).
    fit_traced: Cell<Option<super::fit::FitRect>>,
    /// The relaunch size: dynamic-mode modesets keep it current so a reboot comes back at
    /// whatever resolution the guest last ran (e.g. an in-guest xrandr choice).
    desired_size: Arc<AtomicU64>,
    hidpi: bool,
    /// The chrome-reveal mirror (`reveal_chrome`): the tap's top-edge gesture sets it, this
    /// window's strip stands down on it. The secondaries' equivalent travels as
    /// [`Layout::reveal_ask`]; `InputState` writes both from one decision, so they can never
    /// disagree.
    reveal: Arc<AtomicBool>,
    /// The slot this window (and its strip) is registered under in [`WINDOW_SLOTS`] /
    /// [`SLOT_WINDOWS`] — [`Self::register`]'s change detector. `None` until the first tick.
    registered: Cell<Option<usize>>,
    /// Last `[GEOM]` state logged, so a 60 Hz tick reports transitions rather than a firehose.
    geom_traced: Cell<Option<(u32, u32, u32, bool)>>,
    /// Last `[LAYER]` state logged — the two layer frames as CA actually holds them.
    layer_traced: Cell<Option<(i32, i32, i32, i32)>>,
    // Diagnostic (LIMINA_PRESENT_COPY=1): never hand the GUEST's scanout surface to Core
    // Animation — copy it into a private 3-deep ring and show the copy. The zero-copy venus
    // path shares mutter's own double-buffered swapchain with the window server; with no
    // flip-completion feedback the guest reuses/repaints a buffer while CA may still be
    // compositing it (IOSurfaceLock is advisory), so mid-repaint states reach glass — seen
    // as the damaged region (a busy window) blinking out while the rest stays intact. The
    // copy decouples CA from the guest's write cycle entirely; if the flicker vanishes with
    // this on, that race is convicted (the real fix is flip-completion pacing, roadmap #8).
    // Env arms it for the whole run; the marker file toggles it LIVE (touch/rm
    // /tmp/limina-present-copy) so an intermittent flicker can be A/B'd within one session —
    // flicker present → touch → gone → rm → returns is the within-session conviction.
    present_copy_env: bool,
    // Lock-only variant (LIMINA_PRESENT_LOCK / touch /tmp/limina-present-lock): keep
    // zero-copy, but IOSurfaceLock+Unlock the guest surface before handing it to CA.
    // A/B VERDICT: FAILED — visibly worse than no mitigation at all (several anomalies
    // within seconds vs ~5 bursts/hour untreated). Kept as a documented negative result:
    // the copy's load-bearing property is IMMUTABILITY, not the GPU-write sync. Do not
    // enable; use LIMINA_PRESENT_COPY. COPY wins if both are set.
    present_lock_env: bool,
    /// The live /tmp marker toggles are re-stat'ed at most every 500 ms, NOT per frame: a
    /// synchronous /tmp stat on the main-thread frame apply is a present-path stall source
    /// of exactly the hard-to-attribute kind (same class as libkrun 0113; the worker's
    /// fence-present toggle got the same treatment).
    marker_poll_at: Cell<std::time::Instant>,
    copy_marker: Cell<bool>,
    lock_marker: Cell<bool>,
    copy_ring: RefCell<Vec<CFRetained<IOSurfaceRef>>>,
    copy_geom: Cell<(u32, u32)>,
    copy_idx: Cell<usize>,
    applies: Cell<u64>,
    /// Diagnostic: dump the presented IOSurface to a PNG (no screen-record perm needed).
    /// `LIMINA_WINDOW_CAPTURE`.
    capture_path: Option<String>,

    /// Least time between capture dumps (`LIMINA_WINDOW_CAPTURE_INTERVAL_MS`), and when the
    /// last one was written. Timed rather than counted in applies: a desktop holding still
    /// presents rarely, so an apply-counted cadence leaves the file arbitrarily stale in
    /// exactly the case where the frame is the only oracle — and stale is worse than absent,
    /// because it still reads like a current frame.
    capture_interval: std::time::Duration,
    last_capture: Cell<Option<std::time::Instant>>,
    /// Diagnostic: ids to ALSO dump by global lookup each tick (`LIMINA_CAPTURE_IDS` — see
    /// `diag::capture_ids_from_env` for the format and why).
    capture_ids: Vec<u32>,
}

impl PrimaryDisplay {
    pub(crate) fn new(
        core: Rc<super::guestwindow::GuestWindow>,
        slot: Rc<Cell<u32>>,
        geom: Rc<Cell<(u32, u32)>>,
        desired_size: Arc<AtomicU64>,
        hidpi: bool,
        reveal: Arc<AtomicBool>,
    ) -> Self {
        Self {
            core,
            slot,
            last_gen: Cell::new(0),
            geom,
            fit_traced: Cell::new(None),
            desired_size,
            hidpi,
            reveal,
            registered: Cell::new(None),
            geom_traced: Cell::new(None),
            layer_traced: Cell::new(None),
            present_copy_env: std::env::var_os("LIMINA_PRESENT_COPY").is_some(),
            present_lock_env: std::env::var_os("LIMINA_PRESENT_LOCK").is_some(),
            marker_poll_at: Cell::new(std::time::Instant::now()),
            copy_marker: Cell::new(std::fs::metadata("/tmp/limina-present-copy").is_ok()),
            lock_marker: Cell::new(std::fs::metadata("/tmp/limina-present-lock").is_ok()),
            copy_ring: RefCell::new(Vec::new()),
            copy_geom: Cell::new((0, 0)),
            copy_idx: Cell::new(0),
            applies: Cell::new(0),
            capture_path: std::env::var("LIMINA_WINDOW_CAPTURE").ok(),
            capture_interval: super::diag::capture_interval_from_env(),
            last_capture: Cell::new(None),
            capture_ids: super::diag::capture_ids_from_env(),
        }
    }

    /// Keep this window — and its strip, once it exists — registered under the slot it
    /// currently shows, so `input`'s `target_of` decodes its events through the registry
    /// path like every other guest window's: `locationInWindow` against the event window's
    /// own layer frame. The slot is not fixed (a panel handover moves the primary onto
    /// another slot), so this reconciles per tick rather than registering once.
    ///
    /// Handover ordering: this runs before [`GuestWindows::apply`] drops the secondary that
    /// showed the new slot, so its entry is overwritten here first and the secondary's
    /// identity-guarded `Drop` leaves it alone. The stale [`SLOT_WINDOWS`] entry for the
    /// OLD slot is pruned by identity — never by slot number, so a window that has already
    /// claimed that slot is not evicted.
    fn register(&self) {
        let slot = self.slot.get() as usize;
        let strip = self.core.overlay.strip_window();
        let strip_unregistered = strip.as_ref().is_some_and(|w| {
            let key = &**w as *const NSWindow as usize;
            WINDOW_SLOTS.with(|m| m.borrow().get(&key).copied()) != Some(slot)
        });
        if self.registered.get() == Some(slot) && !strip_unregistered {
            return;
        }
        let key = &*self.core.window as *const NSWindow as usize;
        WINDOW_SLOTS.with(|m| {
            let mut m = m.borrow_mut();
            m.insert(key, slot);
            if let Some(w) = strip {
                m.insert(&*w as *const NSWindow as usize, slot);
            }
        });
        SLOT_WINDOWS.with(|m| {
            let mut m = m.borrow_mut();
            m.retain(|&s, w| &**w as *const NSWindow as usize != key || s == slot);
            m.insert(slot, self.core.window.clone());
        });
        self.registered.set(Some(slot));
    }

    /// Strip reconcile + housing learn — the primary flavor of
    /// [`SecondaryWindow::reconcile_extend`].
    fn reconcile(&self, notch: crate::vmlib::schema::NotchPolicy) {
        self.core.overlay.reconcile(
            &self.core.window,
            notch,
            self.reveal.load(Ordering::Relaxed),
        );
        // Native fullscreen is the only state that reveals what AppKit's own housing inset
        // actually costs; record it so `avoid` can size the guest to the point. The
        // carrier's view is the inset one in BOTH policies now — the strip is a separate
        // window and never changes what this measures — so unlike the re-parenting design
        // there is no state to skip.
        if self
            .core
            .window
            .styleMask()
            .contains(NSWindowStyleMask::FullScreen)
        {
            if let Some(s) = self.core.window.screen() {
                // Read the store BEFORE learning so the trace shows the transition.
                let was = super::hostdisplay::fullscreen_inset(&s);
                let observed = self.core.learn_native_inset();
                if super::display_trace() {
                    let frame = s.frame().size;
                    let sz = self.core.view.bounds().size;
                    eprintln!(
                        "[DISPTRACE] learn id={} name={:?} frame={}x{} view={}x{} \
                         observed={observed:?} was={was}",
                        super::hostdisplay::display_id_of(&s),
                        s.localizedName().to_string(),
                        frame.width,
                        frame.height,
                        sz.width,
                        sz.height,
                    );
                }
            }
        }
    }

    /// The primary's per-tick refit — [`SecondaryWindow::refit`]'s counterpart. Track the
    /// scanout layer to the window every tick, INCLUDING mid live-resize (the timer fires in
    /// common modes, so this runs during the drag): dynamic fills the window (a
    /// layer-HOSTING view doesn't auto-size its layer; CA scales the current surface to the
    /// new frame, so the desktop stretches smoothly during a drag and snaps crisp once the
    /// guest re-modesets); host/fixed aspect-fit the guest resolution into the view — the
    /// letterbox — on the black window background.
    fn refit(&self, mode: DisplayResolution) {
        let Some(v) = self.core.window.contentView() else {
            return;
        };
        let sz = v.frame().size;
        // The area the guest is drawn into. AppKit insets the fullscreen carrier below the
        // housing in both policies; under `extend` the strip window covers that band and
        // shows the top of the same image, so the guest's usable height is the view plus the
        // inset. One number for the fit, the pointer mapping and both layers — THE
        // band-inset rule, `GuestWindow::strip_inset`.
        let strip_inset = self.core.strip_inset();
        let ((sz_w, sz_h), target) = super::fit::refit_target(
            (sz.width, sz.height),
            strip_inset,
            mode == DisplayResolution::Dynamic,
            self.geom.get(),
        );
        // Every input to the guest's height, on one line, whenever any of them moves. The
        // symptom "guest renders below the housing while the band still shows content" has
        // three candidate causes — the policy said no band, the learned inset came back 0,
        // or the view was already the whole panel — and they are indistinguishable from the
        // outside. Traced rather than reasoned about, after a wrong guess.
        if super::display_trace() {
            let (id, notch, learned) = self
                .core
                .window
                .screen()
                .map(|s| {
                    (
                        super::hostdisplay::panel_key(&s),
                        super::hostdisplay::notch_inset(&s),
                        super::hostdisplay::fullscreen_inset(&s),
                    )
                })
                .unwrap_or((0, -1.0, -1.0));
            let now = (
                sz.height as u32,
                strip_inset as u32,
                sz_h as u32,
                self.core.overlay.claims_band(),
            );
            if self.geom_traced.get() != Some(now) {
                self.geom_traced.set(Some(now));
                eprintln!(
                    "[GEOM] {} panel={:x} view={:.0}x{:.0} claims_band={} notch={:.0} \
                     learned={:.0} strip_inset={:.0} -> guest_area={:.0}x{:.0} \
                     screen={:?}",
                    super::epoch_ms(),
                    id,
                    sz.width,
                    sz.height,
                    self.core.overlay.claims_band(),
                    notch,
                    learned,
                    strip_inset,
                    sz_w,
                    sz_h,
                    self.core.window.screen().map(|s| {
                        let f = s.frame();
                        (f.origin.x, f.origin.y, f.size.width, f.size.height)
                    }),
                );
            }
        }
        if sz_w > 0.0 && sz_h > 0.0 {
            let g = self.geom.get();
            if self.fit_traced.get() != Some(target) {
                // Letterboxing in FULLSCREEN means the guest's mode and the panel disagree,
                // and the black bars alone don't say which side is wrong. Log both numbers:
                // bars on the SIDES mean the guest is on a mode of the wrong *aspect* (it
                // settled on a DMT entry rather than the preferred timing), bars top and
                // bottom mean the right aspect at a stale *size*, and a short view with a
                // matching guest means the housing strip never reached us. Diagnosing this
                // on dogfood otherwise costs a round of ssh archaeology.
                if (self
                    .core
                    .window
                    .styleMask()
                    .contains(NSWindowStyleMask::FullScreen)
                    || self.core.overlay.is_active())
                    && (target.w < sz_w - 1.0 || target.h < sz_h - 1.0)
                {
                    log::debug!(
                        "fullscreen letterbox: guest {}x{} into {:.0}x{:.0} pt usable \
                         (view {:.0}x{:.0}, overlay {}) -> {:.0}x{:.0} \
                         at +{:.0}+{:.0}",
                        g.0,
                        g.1,
                        sz_w,
                        sz_h,
                        sz.width,
                        sz.height,
                        self.core.overlay.is_active(),
                        target.w,
                        target.h,
                        target.x,
                        target.y,
                    );
                }
                self.fit_traced.set(Some(target));
            }
            // Re-assert the scanout layer's frame whenever CA's copy has drifted — checked
            // against the LAYER, never a cache of intent; see `GuestWindow::apply_fit`.
            self.core.apply_fit(target);
            // Keep the strip over the housing band and its copy of the layer on the same
            // image, every tick: it has no AppKit machinery holding it in place across a
            // display reconfiguration, and the fit it mirrors can change under it.
            if let Some(s) = self.core.window.screen() {
                self.core.overlay.place(&s, sz.height, strip_inset, target);
            }
            // What the two layers ACTUALLY hold, as opposed to what we last asked for. The
            // guest's top bar was once drawn twice, 33 pt apart, while every input number
            // read correct — because a second writer had put a differently-fitted rect on
            // the carrier's layer. Only the layers themselves can report that.
            if super::display_trace() {
                let c = self.core.layer.frame();
                let s = self.core.overlay.strip_layer().frame();
                let now = (
                    c.origin.y as i32,
                    c.size.height as i32,
                    s.origin.y as i32,
                    s.size.height as i32,
                );
                if self.layer_traced.get() != Some(now) {
                    self.layer_traced.set(Some(now));
                    eprintln!(
                        "[LAYER] {} carrier=({:.0},{:.0} {:.0}x{:.0}) \
                         strip=({:.0},{:.0} {:.0}x{:.0}) target={:?} view={:.0}x{:.0}",
                        super::epoch_ms(),
                        c.origin.x,
                        c.origin.y,
                        c.size.width,
                        c.size.height,
                        s.origin.x,
                        s.origin.y,
                        s.size.width,
                        s.size.height,
                        target,
                        sz.width,
                        sz.height,
                    );
                }
            }
            // If the strip just came up, hand it the frame the carrier is already showing —
            // `GuestWindow::seed_strip_if_new`.
            self.core.seed_strip_if_new();
        }
    }

    /// Gen gate → modeset follow → resolve/present, plus the primary-only present and
    /// capture diagnostics.
    fn present(
        &self,
        snap: &SlotSnapshot,
        surface_map: &SurfaceMap,
        ack_tx: &SyncSender<AckMsg>,
        mode: DisplayResolution,
    ) {
        let SlotSnapshot {
            show_id,
            width,
            height,
            gen,
            ..
        } = *snap;
        if gen == self.last_gen.get() {
            return;
        }
        self.last_gen.set(gen);

        // Gate order is the primary's own: `show_id` BEFORE the modeset follow (a secondary
        // follows geometry first and gates on `show_id` after). Deliberate until proven
        // otherwise — unifying the order would change when a connector that is up before its
        // first frame triggers the dynamic setContentSize follow.
        let Some(id) = show_id else { return };
        if self.geom.get() != (width, height) {
            self.geom.set((width, height));
            if mode == DisplayResolution::Dynamic {
                // Guest-follow (dynamic only): the window tracks guest modesets, as
                // originally shipped.
                let (pw, ph) =
                    super::scale_for(&self.core.window, self.hidpi).to_points((width, height));
                self.core.window.setContentSize(NSSize::new(pw, ph));
                let full = super::fit::FitRect::full(width as f64, height as f64);
                super::set_layer_frame(&self.core.layer, full);
                // Keep the relaunch size current: a reboot then boots at whatever
                // resolution the guest last ran (e.g. an in-guest xrandr choice).
                self.desired_size
                    .store(crate::session::pack_size(width, height), Ordering::Relaxed);
            } else if let Some(v) = self.core.window.contentView() {
                // Host/fixed: the window is host-owned — a guest modeset re-fits the
                // letterbox NOW (not next tick) so this frame presents at the right rect.
                // Through `GuestWindow::strip_inset`, so this agrees with the render tick
                // about how tall the guest's area is.
                let sz = v.frame().size;
                let (_, target) = super::fit::refit_target(
                    (sz.width, sz.height),
                    self.core.strip_inset(),
                    false,
                    (width, height),
                );
                self.core.apply_fit(target);
            }
            // A mode change means the worker allocated fresh surfaces; ids from the old mode
            // are gone (and could be reused for unrelated surfaces), so drop the cache.
            self.core.clear_frame_cache();
        }

        // Resolve the id through the window core — THE resolve rule (pin + cache +
        // Mach-store-then-lookup + resurface recovery on failure), one per window; see
        // `guestwindow::resolve_presented`.
        let Some(surface) = self.core.resolve(id, surface_map, ack_tx) else {
            return;
        };
        let surface = &surface;
        // Shown-ack channel (#8 leg 2): after Core Animation processes this frame's
        // transaction, hand the id to the dedicated ack-sender thread (a bounded,
        // non-blocking try_send) so it can tell the worker "shown <id>" at the real latch
        // boundary — the blocking socket write never touches the AppKit main thread. The ack
        // identifies the frame by the GUEST's surface id even in copy mode (the worker
        // tracks holds by the id it presented); the sender thread targets whichever worker
        // is current after a relaunch. The message also carries the surface this frame
        // replaces as layer contents (the one WindowServer may still be sampling) so the
        // sender can hold the ack until it's truly off glass (#24). A same-surface re-flush
        // carries None — there's nothing replaced to wait on (and the guest is
        // single-buffering, which pacing can't protect).
        // The layer write + strip mirror + shown-ack is `GuestWindow::show_with_ack`, the
        // same call a plain `present` ends in — the copy/lock diagnostics below only decide
        // WHICH surface goes on glass.
        // Distinct object each frame (the worker alternates ids) → CA re-reads.
        if self.marker_poll_at.get().elapsed() >= std::time::Duration::from_millis(500) {
            self.marker_poll_at.set(std::time::Instant::now());
            self.copy_marker
                .set(std::fs::metadata("/tmp/limina-present-copy").is_ok());
            self.lock_marker
                .set(std::fs::metadata("/tmp/limina-present-lock").is_ok());
        }
        let present_copy = self.present_copy_env || self.copy_marker.get();
        if present_copy {
            if self.copy_geom.get() != (width, height) {
                self.copy_geom.set((width, height));
                let mut ring = self.copy_ring.borrow_mut();
                ring.clear();
                for _ in 0..3 {
                    if let Some(s) = super::diag::create_local_iosurface(width, height) {
                        ring.push(s);
                    }
                }
            }
            let ring = self.copy_ring.borrow();
            if ring.len() == 3 {
                let dst = &ring[self.copy_idx.get() % 3];
                self.copy_idx.set(self.copy_idx.get().wrapping_add(1));
                super::diag::copy_surface(surface, dst);
                self.core.show_with_ack(id, dst, ack_tx);
            } else {
                self.core.show_with_ack(id, surface, ack_tx);
            }
        } else {
            let present_lock = self.present_lock_env || self.lock_marker.get();
            if present_lock {
                super::diag::sync_surface(surface);
            }
            self.core.show_with_ack(id, surface, ack_tx);
        }

        // Diagnostic capture of the presented scanout. Periodic (overwrite) so a
        // long-running headless check ends with a recent frame, not just early boot.
        self.applies.set(self.applies.get() + 1);
        if let Some(path) = &self.capture_path {
            let now = std::time::Instant::now();
            let due = self
                .last_capture
                .get()
                .is_none_or(|t| now.duration_since(t) >= self.capture_interval);
            if due {
                self.last_capture.set(Some(now));
                super::diag::capture_iosurface_async(surface, id, path);
            }
        }
        // Targeted per-id sweep — look each requested global id up fresh (no cache) and
        // dump it, so we can read the venus blob surface directly even when it isn't the
        // presented one.
        if !self.capture_ids.is_empty() && self.applies.get().is_multiple_of(30) {
            if let Some(base) = &self.capture_path {
                for &cid in &self.capture_ids {
                    if let Some(s) = IOSurfaceLookup(cid) {
                        super::diag::capture_iosurface_async(
                            &s,
                            cid,
                            &format!("{base}.id{cid}.png"),
                        );
                    } else {
                        log::info!("capture: IOSurfaceLookup({cid}) -> none (not alive)");
                    }
                }
            }
        }
    }
}

/// One window presenting one pool slot: the shared presentation core plus the
/// secondary-only state (cover styling, panel homing, per-slot frame gating).
struct SecondaryWindow {
    core: super::guestwindow::GuestWindow,
    /// Last applied `gen`, so an unchanged slot costs nothing per tick.
    last_gen: u64,
    /// Last applied guest geometry, so the window is resized only on a real modeset.
    geom: (u32, u32),
    /// The host panel this window belongs to, so a restyle can find its screen again. Mutable
    /// because a slot can be recycled to a different panel under a window that stays open.
    panel: Option<u64>,
    /// Whether the window is currently covering its whole panel rather than floating as a
    /// plain titled window. Tracked so a restyle happens on the transition and not per tick.
    covering: bool,
    /// Which mechanism the current cover uses: a native fullscreen Space on this panel
    /// (`true`) or the borderless above-menu-bar cover window (`false`). Decided per
    /// transition by [`native_space_ok`]; meaningless while not covering.
    native_space: bool,
    /// The "Suspending…" veil, while a suspend bracket is in flight ([`GuestWindows::veil`]).
    /// The primary has always had one; without it here a second panel simply stops updating
    /// for the seconds the snapshot takes, with nothing to say why. Only this flavor can ever
    /// reach a secondary: parked and resuming both happen after the park has closed them.
    suspend_overlay: Option<super::overlay::Overlay>,
    /// The housing band the current style owes back to the window background, in points:
    /// nonzero only for a borderless cover under `notch = avoid`. A native Space's content
    /// view is already the safe area, and a floating window has no band. Stored at restyle so
    /// the per-tick [`Self::refit`] does not re-derive screens.
    cover_inset: f64,
    /// Whether the strip window is registered in [`WINDOW_SLOTS`] — done once when it first
    /// exists, so band clicks resolve to this slot.
    strip_registered: bool,
}

/// Every guest window: the primary (`run()`'s window, wearing its [`PrimaryDisplay`] role
/// state) plus one window per other connected slot, the latter created and destroyed as
/// connectors come and go. The secondary set includes slot 0 whenever the primary window
/// sits on a panel that owns some other slot — the pool has no permanently-special index.
pub(crate) struct GuestWindows {
    primary: PrimaryDisplay,
    secondaries: HashMap<usize, SecondaryWindow>,
}

impl GuestWindows {
    pub(crate) fn new(primary: PrimaryDisplay) -> Self {
        Self {
            primary,
            secondaries: HashMap::new(),
        }
    }

    /// Drop, from EVERY window's frame cache, the surfaces the guest has let go of.
    ///
    /// The receive thread has already dropped the store's reference (`note_released`); this
    /// is the other half, and it has to happen on the main thread because the caches are
    /// main-thread state. `take_released` drains the store's list, so the one drain must fan
    /// out to every window — a drain that only purged the primary left a released
    /// framebuffer retained for as long as a secondary's cache kept its entry.
    ///
    /// Called from both the frame-apply ([`Self::apply`]) and the render timer, and the
    /// timer leg is load-bearing: releases arrive whenever the guest frees a scanout,
    /// including long after the last frame — a compositor that quits stops presenting, so a
    /// purge that only ran on frame-apply would never run again and its framebuffers would
    /// stay pinned for the supervisor's life (testcomp/supervisor-retention.sh).
    pub(crate) fn drain_releases(&self, map: &SurfaceMap) {
        let released = map.lock().unwrap().take_released();
        if released.is_empty() {
            return;
        }
        self.primary.core.drop_released(&released);
        for w in self.secondaries.values() {
            w.core.drop_released(&released);
        }
    }

    /// Apply the current shared state to every guest window: run the primary's walk, open a
    /// window for any other live slot that lacks one, present each one's latest frame, and
    /// close the window of any slot whose connector went away.
    ///
    /// Called from the frame-apply closure, so it runs on the main thread and inherits its
    /// wake-on-frame scheduling.
    pub(crate) fn apply(
        &mut self,
        shared: &std::sync::Arc<std::sync::Mutex<Shared>>,
        surface_map: &SurfaceMap,
        ack_tx: &SyncSender<AckMsg>,
        layout: &Layout,
        mtm: MainThreadMarker,
    ) {
        let Layout {
            panels,
            notch,
            mode,
            reveal_ask,
        } = *layout;
        // The primary's own facts, read off its entry: which slot it shows (the control
        // plane assigned it just before this call), and whether the VM is fullscreen — the
        // primary's Space is what covers every other panel.
        let primary = self.primary.slot.get() as usize;
        let cover = self
            .primary
            .core
            .window
            .styleMask()
            .contains(NSWindowStyleMask::FullScreen);

        // Let go of what the guest has let go of before any window resolves this tick.
        self.drain_releases(surface_map);

        // Snapshot every slot under one lock — a per-slot lock/unlock would let the reader
        // interleave and give this pass an inconsistent view of the desktop.
        let (exited, slots): (bool, Vec<SlotSnapshot>) = {
            let s = shared.lock().unwrap();
            let slots = (0..MAX_SCANOUTS)
                .map(|slot| {
                    let d = &s.slots[slot];
                    SlotSnapshot {
                        slot,
                        show_id: d.show_id,
                        width: d.width,
                        height: d.height,
                        gen: d.gen,
                    }
                })
                .collect();
            (s.worker_exited, slots)
        };

        // The primary's walk first, the same shape every window below gets: strip reconcile
        // → refit → gen gate/modeset follow/present. Its window LIFECYCLE stays `run()`'s
        // (park machinery, close policy) — on a dead worker it stays up showing its last
        // frame, it is never this pass's to open or close.
        self.primary.register();
        self.primary.reconcile(notch);
        self.primary.refit(mode);
        if let Some(snap) = slots.iter().find(|s| s.slot == primary) {
            self.primary.present(snap, surface_map, ack_tx, mode);
        }

        // The worker is gone: nothing will ever say `scanoutgone` for these, and a window left
        // up would sit frozen on its last frame. This also covers a guest reboot, where the
        // fresh worker re-announces whatever connectors the guest brings back.
        if exited {
            self.close_secondaries();
            return;
        }
        // The primary window moved onto this slot (a panel handover). Its picture belongs in
        // the main window now, so drop the secondary that was showing it.
        if let Some(w) = self.secondaries.remove(&primary) {
            log::info!("window: guest display {primary} is the main window's now");
            drop(w);
        }
        // A slot the table has taken down. Close on OUR decision rather than waiting for
        // `scanoutgone`: the guest takes a few seconds to notice the unplug and stop presenting,
        // and for those seconds the window sat there showing a display that had been dismissed —
        // leave fullscreen and re-enter quickly enough and nothing appeared to happen at all
        // (observed on the two-panel rig, 2026-08-17).
        let dismissed: Vec<usize> = self
            .secondaries
            .keys()
            .copied()
            .filter(|slot| !panels.contains_key(slot))
            .collect();
        for slot in dismissed {
            if let Some(w) = self.secondaries.remove(&slot) {
                log::info!("window: guest display {slot} was disconnected; closing its window");
                drop(w);
            }
        }
        // Prune band claims to the windows that exist, every tick — a closed slot's stale
        // claim would keep the reveal gesture armable against a band that is gone. Here rather
        // than in `Drop` because a claim is keyed by slot while `Drop` knows only its window,
        // and the strip shares the slot: the map that owns the windows is the one place that
        // knows which slots remain.
        BAND_ACTIVE.with(|m| {
            m.borrow_mut()
                .retain(|slot, _| self.secondaries.contains_key(slot))
        });

        for SlotSnapshot {
            slot,
            show_id,
            width,
            height,
            gen,
        } in slots
        {
            // The primary's slot already had its walk above; it gets no secondary window.
            if slot == primary {
                continue;
            }
            // Whether the guest has a mode on this slot right now — presentation, not
            // lifetime. See [`fate`] for what each combination means.
            let live = width != 0 && height != 0;
            let fate = slot_fate(
                self.secondaries.contains_key(&slot),
                panels.contains_key(&slot),
                live,
            );
            if fate == Fate::Idle {
                continue;
            }
            if fate == Fate::Open {
                let window =
                    SecondaryWindow::open(slot, panels.get(&slot).copied(), width, height, mtm);
                self.secondaries.insert(slot, window);
            }
            let Some(entry) = self.secondaries.get_mut(&slot) else {
                continue;
            };
            // The slot changed hands: its panel was unplugged and another monitor recycled the
            // connector. The window is still sitting on the old panel's coordinates and would
            // look for a screen that is gone, so re-home it before anything else uses `panel`.
            let now = panels.get(&slot).copied();
            if entry.panel != now {
                entry.panel = now;
                entry.covering = !cover; // force the restyle below to run
            }
            // Before the frame early-out: the VM can enter or leave fullscreen on a tick that
            // brings no new frame for this slot, and a window left in the wrong style until the
            // guest next paints is exactly the half-covered panel this fixes.
            if entry.covering != cover {
                entry.covering = cover;
                entry.restyle(cover, notch, mtm);
            }
            // The strip overlay first — it decides whether the guest owns the housing band,
            // and therefore what box the refit computes against (the primary orders these the
            // same way for the same reason).
            entry.reconcile_extend(slot, notch, reveal_ask == Some(slot));
            // Every tick, whatever the state: a native Space resizes the view when its
            // animation completes (ticks after the restyle ran), a layout pass resets a
            // layer-hosting view's layer without a word, and a floating window is Resizable —
            // the user can change the box under us with no modeset anywhere. One refit is the
            // answer to all of them.
            entry.refit(mode);
            if entry.last_gen == gen {
                continue;
            }
            entry.last_gen = gen;
            if fate == Fate::Dark {
                // The ring the guest just disabled is gone, and its ids are free to be reused
                // for something unrelated — so nothing cached may outlive it. The layer keeps
                // the last frame until the new mode's first present replaces it, which is what
                // a monitor does across a mode change and what the primary has always done
                // across the same churn.
                entry.core.clear_frame_cache();
                continue;
            }

            if entry.geom != (width, height) {
                entry.geom = (width, height);
                // Guest-follow is dynamic-mode behavior, exactly as it is for the primary: the
                // window tracks the guest's modesets. Host and fixed keep the window where it
                // is and let the refit letterbox the new mode into it.
                if mode == DisplayResolution::Dynamic {
                    entry.resize(width, height);
                }
                entry.refit(mode);
                // Fresh surfaces were allocated for the new mode; ids from the old one are gone
                // and may be reused for something unrelated.
                entry.core.clear_frame_cache();
            }

            let Some(id) = show_id else { continue };
            entry.core.present(id, surface_map, ack_tx);
        }
    }

    /// Composite the captured guest cursor into every guest window, each from its own
    /// slot's state.
    ///
    /// **No slot is selected.** The guest enables its hardware cursor plane on exactly one CRTC
    /// and hides it on the others, so handing each window its own slot makes exactly one of them
    /// draw — the guest's own answer to "which display is the pointer on", with no host-side
    /// inference to get wrong. Picking a slot here instead (from where the *host* pointer is)
    /// was the previous shape, and in capture mode the host pointer is frozen and says nothing:
    /// it stayed pinned at slot 0 all session, so the primary window kept drawing a cursor the
    /// guest had moved elsewhere while the panel that really had it drew none.
    ///
    /// Returns what the window showing `watch` did, for the drawn-nothing check
    /// ([`super::cursor::undrawn_fault`]); `None` when no window shows that slot.
    pub(crate) fn update_capture_cursors(
        &self,
        captured: &std::sync::atomic::AtomicBool,
        shared: &std::sync::Arc<std::sync::Mutex<Shared>>,
        surface_map: &SurfaceMap,
        ack_tx: &SyncSender<AckMsg>,
        watch: usize,
    ) -> Option<super::cursor::LayerVerdict> {
        let primary_slot = self.primary.slot.get() as usize;
        let mut watched = None;
        let v =
            self.primary
                .core
                .composite_cursor(primary_slot, captured, shared, surface_map, ack_tx);
        if primary_slot == watch {
            watched = Some(v);
        }
        for (slot, w) in &self.secondaries {
            let v = w
                .core
                .composite_cursor(*slot, captured, shared, surface_map, ack_tx);
            if *slot == watch {
                watched = Some(v);
            }
        }
        watched
    }

    /// Close every secondary window. The primary is never this collection's to close: on
    /// worker exit it stays up (the park machinery draws its curtain), and at teardown
    /// `run()` owns it.
    pub(crate) fn close_secondaries(&mut self) {
        self.secondaries.clear();
    }

    /// Wear (or drop) the suspend veil on every secondary, mirroring the primary's.
    ///
    /// A suspend freezes every panel, not just the one the primary is on, and the snapshot save
    /// takes seconds — long enough for a still second display to read as a hang. Driven from the
    /// tick's overlay block so the two go up and come down together, including when the bracket
    /// is abandoned and the VM keeps running.
    pub(crate) fn veil(&mut self, on: bool) {
        for w in self.secondaries.values_mut() {
            match (on, w.suspend_overlay.is_some()) {
                (true, false) => {
                    w.suspend_overlay = Some(super::overlay::Overlay::suspend(
                        &w.core.layer,
                        &w.core.view,
                    ));
                }
                (false, true) => {
                    if let Some(o) = w.suspend_overlay.take() {
                        o.remove();
                    }
                }
                // Up and staying up: re-fit, exactly as the primary's does, so a resize or a
                // restyle under the veil does not leave it the wrong size.
                (true, true) => {
                    if let Some(o) = w.suspend_overlay.as_ref() {
                        o.fit(&w.core.view);
                    }
                }
                (false, false) => {}
            }
        }
    }
}

/// Closing and forgetting is [`Drop`]'s job, so it cannot be forgotten.
///
/// A window left in [`WINDOW_SLOTS`] keeps claiming a slot, and its address is reused by the
/// next window AppKit allocates — which routes a whole display's input to the wrong guest
/// monitor. Making this a `Drop` rather than a method removes the class instead of the
/// instances: every path that drops a `SecondaryWindow` deregisters, including the ones that
/// drop the whole map without calling anything.
impl Drop for SecondaryWindow {
    fn drop(&mut self) {
        // The strip first: read its address and deregister it BEFORE `close` consumes the
        // window — a stale registry entry would route a reused address's input to this slot.
        if let Some(strip) = self.core.overlay.strip_window() {
            let skey = &*strip as *const NSWindow as usize;
            WINDOW_SLOTS.with(|m| m.borrow_mut().remove(&skey));
        }
        self.core.overlay.close();
        let key = &*self.core.window as *const NSWindow as usize;
        WINDOW_SLOTS.with(|m| m.borrow_mut().remove(&key));
        // By window identity, not by whatever slot number this instance believes: a stale
        // instance dropped after its slot was reopened must not unregister the new window.
        SLOT_WINDOWS.with(|m| {
            m.borrow_mut()
                .retain(|_, w| &**w as *const NSWindow as usize != key)
        });
        self.core.window.close();
    }
}

/// Whether this panel's cover can be a native fullscreen Space rather than the borderless
/// cover window.
///
/// One gate: **"Displays have separate Spaces" must be on** (the macOS default). With it off,
/// one Space spans every display and a native fullscreen anywhere blanks the other panels —
/// the borderless cover is the only mechanism that still shows pixels there. `notch = extend`
/// no longer keeps the cover: each secondary carries its own strip overlay
/// ([`SecondaryWindow::reconcile_extend`]), the same design as the primary's — the Space is
/// the carrier and the strip rides above it showing the rows beside the housing.
fn native_space_ok(mtm: MainThreadMarker) -> bool {
    NSScreen::screensHaveSeparateSpaces(mtm)
}

impl SecondaryWindow {
    /// Open a plain windowed window on the slot's panel. Covering is NOT decided here: the
    /// window comes up `covering: false` and [`GuestWindows::apply`]'s own `covering != cover`
    /// check restyles it on the very next statement. Doing it in both places meant two
    /// `toggleFullScreen` calls in one tick, and the second one only checked the style mask —
    /// which AppKit has not necessarily flipped yet mid-transition, so it could ask to leave
    /// the Space it had just asked to enter.
    fn open(
        slot: usize,
        panel: Option<u64>,
        width: u32,
        height: u32,
        mtm: MainThreadMarker,
    ) -> Self {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let rect = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(f64::from(width.max(64)), f64::from(height.max(64))),
        );
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc::<NSWindow>(),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // An NSWindow built this way is released-when-closed, so `close()` would drop the last
        // AppKit reference out from under the `Retained` we keep. The over-release does not
        // fault at the close — it faults later, as an `objc_release` on freed memory while the
        // autorelease pool drains inside `[NSApplication run]`, which reads as a random UI
        // crash. We own this window's lifetime; AppKit must not.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(&format!(
            "limina — display {}",
            slot + 1
        )));
        // The shared presentation wiring: layer-hosting view, opaque scanout layer, capture
        // cursor sublayer, black letterbox background — see `GuestWindow::wire`.
        let core = super::guestwindow::GuestWindow::wire(window);
        let window = &core.window;
        let view = &core.view;

        // On the panel that owns this slot. The slot table hands a host panel its connector and
        // keeps it there, so this is the display the guest was given — not merely the Nth one.
        // A slot with no attached panel (a debug pool, a display unplugged between the plan and
        // this tick) falls back to screen N, then to centering.
        let screens = NSScreen::screens(mtm);
        let target = panel
            .and_then(|key| {
                screens
                    .iter()
                    .find(|s| super::hostdisplay::panel_key(s) == key)
            })
            .or_else(|| screens.iter().nth(slot));
        if let Some(screen) = target {
            let f = screen.visibleFrame();
            window.setFrameOrigin(NSPoint::new(
                f.origin.x + (f.size.width - rect.size.width).max(0.0) / 2.0,
                f.origin.y + (f.size.height - rect.size.height).max(0.0) / 2.0,
            ));
        } else {
            window.center();
        }
        // orderFront, never makeKey: keyboard events reach the guest through the PRIMARY
        // window's monitors, so a secondary that took key focus would silently swallow every
        // keystroke while still showing pixels.
        // A tracking area is what makes this window see the pointer at all.
        //
        // `setAcceptsMouseMovedEvents` is necessary and not sufficient: AppKit delivers
        // `mouseMoved` to the KEY window, and this one is deliberately never key — keyboard
        // reaches the guest through the primary, and a secondary that took key focus would
        // swallow every keystroke while still showing pixels. So without a tracking area the
        // guest's other display received clicks (which do go to a non-key window, since they
        // would activate it) and no motion whatsoever. Measured: warping the host cursor across
        // the covered panel produced not one event.
        //
        // `ActiveAlways` rather than `ActiveInKeyWindow` for the same reason, and `InVisibleRect`
        // so the area tracks the window through a restyle instead of pinning the rect it was
        // built with.
        window.setAcceptsMouseMovedEvents(true);
        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                mtm.alloc::<NSTrackingArea>(),
                view.bounds(),
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(view),
                None,
            )
        };
        view.addTrackingArea(&tracking);
        window.orderFront(None);
        WINDOW_SLOTS.with(|m| {
            m.borrow_mut()
                .insert(&**window as *const NSWindow as usize, slot)
        });
        SLOT_WINDOWS.with(|m| m.borrow_mut().insert(slot, window.clone()));
        log::info!("window: guest display {slot} appeared ({width}x{height}); opened a window");

        SecondaryWindow {
            core,
            suspend_overlay: None,
            last_gen: 0,
            geom: (width, height),
            panel,
            covering: false,
            native_space: false,
            cover_inset: 0.0,
            strip_registered: false,
        }
    }

    /// Take the whole panel, or give it back.
    ///
    /// The VM's fullscreen is one Space on the primary's panel; a Space cannot span displays, so
    /// every other panel is covered the way the `notch = extend` strip covers the housing band —
    /// a borderless window above `NSMainMenuWindowLevel` (see [`super::OVERLAY_LEVEL`]), which is
    /// what stops the menu bar and the Dock drawing over the guest. Without it these stayed plain
    /// titled windows centred in `visibleFrame`, so the secondary panel kept its menu bar strip
    /// and was never really fullscreen.
    ///
    /// `CanJoinAllSpaces` is what makes it survive the primary going fullscreen: that opens a
    /// Space on the primary's panel, and a window belonging only to the previous Space would be
    /// parked the moment the switch finished — the guest's other display would go away exactly
    /// when it was supposed to appear.
    fn restyle(
        &mut self,
        cover: bool,
        notch: crate::vmlib::schema::NotchPolicy,
        mtm: MainThreadMarker,
    ) {
        let screen = self.panel.and_then(|key| {
            NSScreen::screens(mtm)
                .iter()
                .find(|s| super::hostdisplay::panel_key(s) == key)
        });
        if cover {
            let Some(screen) = screen else {
                // No screen to cover: the panel went away between the plan and this tick. Leave
                // the window as it is; the dismissal pass closes it on the next one.
                log::warn!("window: guest display has no panel to cover; leaving it windowed");
                return;
            };
            if native_space_ok(mtm) {
                // A native fullscreen Space on this panel: Mission Control, the swipe, the
                // fullscreen animation — the parity the borderless cover never had. AppKit
                // owns the frame from here; the per-tick layer re-assert in `apply` follows
                // the view through the async transition.
                self.native_space = true;
                self.cover_inset = 0.0;
                self.core
                    .window
                    .setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
                if !self
                    .core
                    .window
                    .styleMask()
                    .contains(NSWindowStyleMask::FullScreen)
                {
                    self.core.window.toggleFullScreen(None);
                }
                return;
            }
            self.native_space = false;
            self.core.window.setStyleMask(NSWindowStyleMask::Borderless);
            self.core
                .window
                .setCollectionBehavior(NSWindowCollectionBehavior::CanJoinAllSpaces);
            self.core.window.setLevel(super::OVERLAY_LEVEL);
            // `frame`, not `visibleFrame`: the point is to cover the menu bar, and `visibleFrame`
            // is defined as the part it does not occupy.
            //
            // The WINDOW always takes the whole panel, even under `avoid`. Withholding the
            // housing band from the window instead would leave the menu bar drawn in it — the
            // panel would still not be fullscreen, only differently so. AppKit's own fullscreen
            // covers the band and paints it black, and this is the same thing: the window owns
            // every pixel and the LAYER is what stops below the housing.
            self.core.window.setFrame_display(screen.frame(), true);
            self.cover_inset = match notch {
                crate::vmlib::schema::NotchPolicy::Avoid => {
                    super::hostdisplay::fullscreen_inset(&screen)
                }
                crate::vmlib::schema::NotchPolicy::Extend => 0.0,
            };
            self.core.window.orderFront(None);
        } else if self.native_space {
            // Leaving a native Space: the toggle animates out and AppKit itself restores the
            // pre-fullscreen frame, so none of the borderless-cover restoration below applies.
            self.native_space = false;
            self.cover_inset = 0.0;
            if self
                .core
                .window
                .styleMask()
                .contains(NSWindowStyleMask::FullScreen)
            {
                self.core.window.toggleFullScreen(None);
            }
        } else {
            self.cover_inset = 0.0;
            self.core.window.setLevel(0);
            self.core
                .window
                .setCollectionBehavior(NSWindowCollectionBehavior::empty());
            self.core.window.setStyleMask(
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
            );
            let (w, h) = self.geom;
            self.resize(w, h);
            if let Some(screen) = screen {
                let f = screen.visibleFrame();
                let size = self.core.window.frame().size;
                self.core.window.setFrameOrigin(NSPoint::new(
                    f.origin.x + (f.size.width - size.width).max(0.0) / 2.0,
                    f.origin.y + (f.size.height - size.height).max(0.0) / 2.0,
                ));
            }
        }
    }

    /// The one letterbox rule for every state this window can be in — the secondary's
    /// counterpart of the primary's per-tick fit.
    ///
    /// The usable box is the content view's bounds less [`Self::cover_inset`] (the housing band
    /// a borderless cover owes back under `avoid`; zero everywhere else). Into it, the same
    /// rule the primary applies at `mod.rs`'s fit: **dynamic fills** — the guest is, or will
    /// be, driven to the box's shape, and bars would flash on every rounding disagreement —
    /// **host and fixed letterbox** the guest's actual mode, centered, black bars from the
    /// window background.
    ///
    /// Runs every tick, compared **against the layer** and written only on drift, because the
    /// box moves without any modeset: a native Space's async transition resizes the view ticks
    /// after the restyle, AppKit resets a layer-hosting view's layer on a layout pass and says
    /// nothing, and a floating window is Resizable — the user can change the box by hand.
    /// (Guarding on a cache of intent instead is the squashed-carrier trap —
    /// `docs/input-and-windows.md` §7.) The layer's frame is also what `input`'s `target_of`
    /// and [`window_of_slot`] measure events against, so the pixels and the pointer move
    /// together by construction.
    fn refit(&self, mode: DisplayResolution) {
        let Some(view) = self.core.window.contentView() else {
            return;
        };
        let b = view.bounds();
        // While the overlay claims the band, the guest's drawing area is a housing-inset
        // TALLER than the view — the layer overshoots the top, the view clips the band, and
        // the strip window shows the clipped part (exactly the primary's shape). THE
        // band-inset rule, `GuestWindow::strip_inset`: the inset comes from the same learned
        // per-panel store the primary's fullscreen feeds, which [`Self::reconcile_extend`]
        // also feeds from THIS window's own native fullscreen.
        let strip_inset = self.core.strip_inset();
        let (_, target) = super::fit::refit_target(
            (b.size.width, (b.size.height - self.cover_inset).max(0.0)),
            strip_inset,
            mode == DisplayResolution::Dynamic,
            self.geom,
        );
        self.core.apply_fit(target);
        // Keep the strip over the band and its copy of the layer on the same image, every
        // tick: nothing in AppKit holds a borderless window in place across a display
        // reconfiguration, and the fit it mirrors can change under it.
        if self.core.overlay.claims_band() {
            if let Some(screen) = self.core.window.screen() {
                self.core
                    .overlay
                    .place(&screen, b.size.height, strip_inset, target);
            }
        }
    }

    /// Drive this window's `extend` strip from the tick, the way the primary's is driven.
    ///
    /// [`super::ExtendOverlay::reconcile`]'s own gates carry over unchanged: it wants the
    /// carrier natively fullscreen (the borderless cover has no FullScreen bit, and there the
    /// window itself draws beside the housing — no strip), the policy `extend`, and a panel
    /// that actually has a housing.
    fn reconcile_extend(
        &mut self,
        slot: usize,
        notch: crate::vmlib::schema::NotchPolicy,
        reveal: bool,
    ) {
        self.core
            .overlay
            .reconcile(&self.core.window, notch, reveal);
        BAND_ACTIVE.with(|m| m.borrow_mut().insert(slot, self.core.overlay.claims_band()));
        // The shared inset learn (`GuestWindow::learn_native_inset`): this window may be the
        // only one that ever goes natively fullscreen on its panel, so it records the housing
        // observation into the same per-panel store the strip inset and `describe_panel`'s
        // avoid path read.
        if self.covering && self.native_space {
            self.core.learn_native_inset();
        }
        // Band clicks must resolve to THIS slot: the strip is its own window, and an event
        // decoded against its layer (the shifted guest-image rect) yields the right unit
        // coordinates by the same math as the main window's. Registered once, when the strip
        // first exists; deregistered by Drop.
        if !self.strip_registered {
            if let Some(strip) = self.core.overlay.strip_window() {
                let slot = WINDOW_SLOTS.with(|m| {
                    m.borrow()
                        .get(&(&*self.core.window as *const NSWindow as usize))
                        .copied()
                });
                if let Some(slot) = slot {
                    WINDOW_SLOTS.with(|m| {
                        m.borrow_mut()
                            .insert(&*strip as *const NSWindow as usize, slot)
                    });
                    self.strip_registered = true;
                }
            }
        }
        // If the strip just came up, hand it the frame the window is already showing —
        // `GuestWindow::seed_strip_if_new`.
        self.core.seed_strip_if_new();
    }

    /// Size the window's content to the guest's mode. Two callers: the dynamic-mode modeset
    /// follow (`apply` gates on the mode), and the un-cover restore in [`Self::restyle`] —
    /// any mode there, since the cover left no floating frame worth going back to. The layer
    /// is [`Self::refit`]'s to place.
    fn resize(&self, width: u32, height: u32) {
        // While covering, the panel decides the window's size and the guest's mode follows it,
        // not the other way round. Re-sizing to the guest's mode here would shrink the window
        // off the panel for however long the modeset takes to settle.
        if self.covering {
            return;
        }
        let scale = self.core.window.backingScaleFactor().max(1.0);
        self.core.window.setContentSize(NSSize::new(
            f64::from(width) / scale,
            f64::from(height) / scale,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{slot_fate, Fate};

    /// The gdm handover, and every modeset before it: the guest disables the scanout and
    /// reconfigures it a moment later. Closing the window there cost the secondary its
    /// fullscreen Space on every logout — and the re-entry that was supposed to give it back
    /// did not always happen (observed on the two-panel rig, 2026-08-22).
    #[test]
    fn a_slot_between_modes_keeps_its_window() {
        assert_eq!(slot_fate(true, true, false), Fate::Dark);
    }

    /// The table is the authority on lifetime, and it has not spoken here — so a dark slot
    /// keeps its window whatever the panel map says this tick.
    #[test]
    fn a_dark_slot_keeps_its_window_even_mid_dismissal() {
        assert_eq!(slot_fate(true, false, false), Fate::Dark);
    }

    #[test]
    fn a_slot_with_a_mode_and_a_panel_gets_a_window() {
        assert_eq!(slot_fate(false, true, true), Fate::Open);
    }

    /// A connector on its way down still has geometry until `scanoutgone` lands; a window
    /// opened in that gap would flash up for the settle and close again.
    #[test]
    fn a_slot_the_table_has_not_given_a_panel_opens_nothing() {
        assert_eq!(slot_fate(false, false, true), Fate::Idle);
    }

    /// Nothing to show and nothing to show it in: a slot that has never come up.
    #[test]
    fn a_dark_slot_with_no_window_opens_nothing() {
        assert_eq!(slot_fate(false, true, false), Fate::Idle);
    }

    #[test]
    fn a_live_slot_with_a_window_presents_into_it() {
        assert_eq!(slot_fate(true, true, true), Fate::Show);
    }
}
