// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The per-window presentation core — what every window showing a guest scanout is made of,
//! whether it is the primary or a secondary.
//!
//! A guest window is an `NSWindow` whose content view *hosts* a `CALayer` we own outright,
//! plus the machinery to put a presented IOSurface on that layer correctly: the per-window
//! frame cache, the resolve rule and its resurface recovery, the shown-ack naming the surface
//! a frame replaced, the letterbox target's layer write, this window's `extend` strip
//! (`ExtendOverlay`) with its frame mirror and seed, and the capture-cursor sublayer every
//! window needs a copy of (`docs/input-and-windows.md` §4 — a composited cursor can only show
//! the display its window is showing).
//!
//! What this deliberately does NOT own: which `NSWindow` subclass and style the window is
//! born with (the primary needs `LiminaWindow` for key-eligibility under the borderless
//! `extend` overlay; a secondary is a plain window that must never take key), input decoding,
//! when to refit or reconcile (the callers' ticks decide), fullscreen, and lifecycle policy.
//! Those stay with the callers today and migrate here piece by piece as the primary/secondary
//! split narrows (`docs/design/input-windows-restructure.md` Move A;
//! `docs/input-and-windows.md` is the contract every step must preserve).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2_app_kit::{NSColor, NSView, NSViewLayerContentsRedrawPolicy, NSWindow};
use objc2_core_foundation::CFRetained;
use objc2_io_surface::{IOSurfaceLookup, IOSurfaceRef};
use objc2_quartz_core::CALayer;

use super::present::{self, AckMsg, SendSurface, Shared, SurfaceMap, SurfaceStore};

/// One window presenting one guest scanout: the `NSWindow`, the layer-hosting content view,
/// the scanout layer, this window's copy of the composited capture cursor, and the per-window
/// present state (frame cache + the surface currently on glass).
pub(crate) struct GuestWindow {
    pub(crate) window: Retained<NSWindow>,
    pub(crate) view: Retained<NSView>,
    pub(crate) layer: Retained<CALayer>,
    /// This window's copy of the composited capture cursor, a sublayer of [`Self::layer`].
    ///
    /// While the pointer is captured — which fullscreen's grab does on its own
    /// (`docs/design/fullscreen-pointer-grab.md`) — the host `NSCursor` is hidden and the
    /// guest's cursor is *composited* into a layer instead. Every window draws its own
    /// slot's cursor: the guest enables its hardware cursor plane on exactly one CRTC and
    /// hides it on the others, so per-slot drawing makes exactly one window draw.
    pub(crate) cursor_layer: Retained<CALayer>,
    /// This window's `extend` strip — one per window (`docs/input-and-windows.md` §7):
    /// while the window is a native fullscreen Space on a notched panel under
    /// `notch = extend`, the strip shows the top band of the same guest image beside the
    /// housing. `Rc` because the primary shares it into its input/tap/timer closures; a
    /// secondary reaches it only through its window.
    pub(crate) overlay: Rc<super::ExtendOverlay>,
    /// Whether the strip was up last tick — [`Self::seed_strip_if_new`]'s edge detector.
    strip_was_up: Cell<bool>,
    /// This window's own frame cache. Per window, not shared: the cache exists to avoid
    /// re-resolving the id a display is cycling through, and two displays cycle through
    /// different ids.
    cache: RefCell<SurfaceStore>,
    /// The surface currently on the layer, handed to the ack so the sender can wait for it to
    /// leave window-server use — the true off-glass boundary. Interior-mutable because the
    /// present path runs inside `Fn` closures (the frame-apply and the render timer share the
    /// window through `Rc`), where no `&mut self` exists.
    last_ca: RefCell<Option<CFRetained<IOSurfaceRef>>>,
}

/// Resolve a presented surface id to the retained surface to put on glass, or arrange for its
/// recovery and return `None` (skip the frame — never panic the UI).
///
/// This is THE resolve rule, one per window, and it is a free function over the stores so the
/// unhappy path is testable without AppKit:
///
/// - **Pin before resolving**: the frame cache in front of the store means a hot id would
///   otherwise never touch the store and would look idle to the eviction policy — exactly
///   the freeze `spikes/scanout-blob-freeze/` closed.
/// - Prefer the Mach-delivered store (the capability-scoped, non-global scanouts); fall back
///   to a global `IOSurfaceLookup` for the venus zero-copy path (still global) and the legacy
///   no-receiver mode.
/// - **On failure, say WHY and ask for the surface back.** "Unresolved" alone cost days: it
///   reads as a rare race with a remodeset, while the observed fault is the guest presenting
///   an id the worker told us it released — a permanent skip, not a transient one. These
///   scanouts are non-global, so no lookup can recover one — only the worker can publish it
///   again, and until it does every frame naming this id is skipped. The ask is throttled per
///   id inside the store; the request goes to the dedicated ack thread (never a socket write
///   on the AppKit main thread). See `spikes/scanout-blob-freeze/RESULTS.md`.
pub(crate) fn resolve_presented(
    cache: &RefCell<SurfaceStore>,
    surface_map: &SurfaceMap,
    ack_tx: &SyncSender<AckMsg>,
    id: u32,
) -> Option<CFRetained<IOSurfaceRef>> {
    surface_map.lock().unwrap().pin_presented(id);
    let mut cache = cache.borrow_mut();
    let Some(surface) = cache.get_or_insert_with(id, || {
        surface_map
            .lock()
            .unwrap()
            .get(id)
            .or_else(|| IOSurfaceLookup(id))
    }) else {
        let (why, ask) = {
            let mut map = surface_map.lock().unwrap();
            (map.why_gone(id), map.request_resurface(id))
        };
        if ask {
            let _ = ack_tx.try_send(AckMsg::Resurface(id));
        }
        match why {
            Some(present::GoneReason::Released) => log::warn!(
                "window: surface {id} unresolved; skipping frame — the worker RELEASED \
                 this surface and the guest is still presenting it. Recoverable only if \
                 the worker still has it registered."
            ),
            Some(present::GoneReason::Evicted) => log::warn!(
                "window: surface {id} unresolved; skipping frame — the store EVICTED this \
                 surface at its cap and the guest is still presenting it. Asking for it \
                 back."
            ),
            None => log::warn!(
                "window: surface {id} unresolved; skipping frame — we never held it, and \
                 no release or eviction was recorded for it."
            ),
        }
        return None;
    };
    Some(surface)
}

impl GuestWindow {
    /// Wire an already-constructed `NSWindow` into a guest-presentation window.
    ///
    /// The caller decides the window's class, style, collection behavior and title — those
    /// differ between the primary and a secondary. What is identical, and lives here, is the
    /// content wiring every guest window needs for the same reasons:
    ///
    /// - **Layer-HOSTING, not layer-backed** (`setLayer` before `setWantsLayer`): we own the
    ///   layer, so AppKit never draws over the IOSurface we set as its contents.
    /// - **Opaque scanout layer**: the guest scanout is XRGB with a "don't care" alpha
    ///   channel; blending it would composite the desktop transparent.
    /// - **`NSViewLayerContentsRedrawPolicy::Never`**: AppKit must never invalidate or
    ///   redraw contents it does not own — without this a live window resize ends with the
    ///   layer blanked.
    /// - **Black window background**: the letterbox bars ARE the window background, and
    ///   black is what makes them read as bars.
    /// - A hidden capture-cursor sublayer, positioned/shown by `update_capture_cursor`.
    pub(crate) fn wire(window: Retained<NSWindow>) -> Self {
        let view: Retained<NSView> = window.contentView().expect("content view");
        let layer = CALayer::new();
        layer.setOpaque(true);
        let cursor_layer = CALayer::new();
        cursor_layer.setHidden(true);
        layer.addSublayer(&cursor_layer);
        view.setLayer(Some(&layer));
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);
        window.setBackgroundColor(Some(&NSColor::blackColor()));
        GuestWindow {
            window,
            view,
            layer,
            cursor_layer,
            overlay: Rc::new(super::ExtendOverlay::default()),
            strip_was_up: Cell::new(false),
            // `FRAME_CACHE_CAP`, not the store default: this is a per-window FRAME cache (the
            // hot set is the guest's 2–4-buffer ring), and every retained entry is a whole
            // framebuffer. The primary has always used the small cap; a window on the default
            // cap held 4× the intended memory bound.
            cache: RefCell::new(SurfaceStore::with_cap(present::FRAME_CACHE_CAP)),
            last_ca: RefCell::new(None),
        }
    }

    /// Resolve the presented `id` through this window's frame cache — [`resolve_presented`]
    /// over this window's stores. `None` means skip the frame; recovery is already arranged.
    pub(crate) fn resolve(
        &self,
        id: u32,
        surface_map: &SurfaceMap,
        ack_tx: &SyncSender<AckMsg>,
    ) -> Option<CFRetained<IOSurfaceRef>> {
        resolve_presented(&self.cache, surface_map, ack_tx, id)
    }

    /// Build the shown-ack for putting `shown` on glass as frame `id`, recording it as the
    /// surface now on the layer.
    ///
    /// **The ack names the surface this frame REPLACED**, so the sender can hold "shown"
    /// until that one leaves window-server use — the real off-glass boundary. A same-surface
    /// re-flush replaces nothing and carries `None`. The ack identifies the frame by the
    /// GUEST's surface id even when `shown` is a copy (the worker tracks holds by the id it
    /// presented).
    pub(crate) fn ack_for(
        &self,
        id: u32,
        shown: &CFRetained<IOSurfaceRef>,
        ack_tx: &SyncSender<AckMsg>,
    ) -> Option<(SyncSender<AckMsg>, AckMsg)> {
        let prev = self
            .last_ca
            .borrow_mut()
            .replace(shown.clone())
            .filter(|p| !std::ptr::eq::<IOSurfaceRef>(&**p, &**shown));
        Some((
            ack_tx.clone(),
            AckMsg::Shown(id, prev.map(SendSurface::new)),
        ))
    }

    /// Put `src` on glass as frame `id`: the layer write with [`Self::ack_for`]'s shown-ack,
    /// and — when the `extend` strip is up — the strip's copy of the SAME surface. The strip's
    /// window clips the band, this window clips the rest; the strip's transaction carries NO
    /// ack, because one presented frame must produce exactly one "shown" or the worker's
    /// flush fence would be completed twice.
    ///
    /// `src` is normally the resolved surface itself; the primary's diagnostic copy mode
    /// hands a local copy instead, still acked under the guest's `id`.
    pub(crate) fn show_with_ack(
        &self,
        id: u32,
        src: &CFRetained<IOSurfaceRef>,
        ack_tx: &SyncSender<AckMsg>,
    ) {
        let ack = self.ack_for(id, src, ack_tx);
        present::set_layer_surface(&self.layer, src, ack);
        if self.overlay.is_active() {
            present::set_layer_surface(&self.overlay.strip_layer(), src, None);
        }
    }

    /// Put the presented surface `id` on this window's layer (and its strip, when up), with
    /// the shown-ack the worker's flush fence depends on: [`Self::resolve`] +
    /// [`Self::show_with_ack`].
    pub(crate) fn present(&self, id: u32, surface_map: &SurfaceMap, ack_tx: &SyncSender<AckMsg>) {
        let Some(surface) = self.resolve(id, surface_map, ack_tx) else {
            return;
        };
        self.show_with_ack(id, &surface, ack_tx);
    }

    /// On the tick the `extend` strip comes up, hand it the frame this window is already
    /// showing. An idle guest presents nothing, so without this the band would stay black
    /// until something in the guest happened to redraw. No ack — this is not a new frame,
    /// and one frame must produce exactly one "shown".
    pub(crate) fn seed_strip_if_new(&self) {
        let up = self.overlay.is_active();
        let was = self.strip_was_up.replace(up);
        if up && !was {
            if let Some(surface) = self.last_surface() {
                present::set_layer_surface(&self.overlay.strip_layer(), &surface, None);
            }
        }
    }

    /// Composite the captured guest cursor into this window's layer — and the strip's copy of
    /// it, on the same terms as the strip's copy of the picture: the band is a different
    /// window, so a cursor composited only into the main layer is clipped out of existence
    /// there (the pointer would vanish on entering the band while still driving the guest).
    ///
    /// `slot` must be this window's OWN slot, never the pointer's: a composited cursor can
    /// only show the display its window is showing, and the guest says which display has the
    /// cursor by enabling the plane on one slot and hiding the others — every window drawing
    /// its own slot makes exactly one of them draw.
    pub(crate) fn composite_cursor(
        &self,
        slot: usize,
        captured: &AtomicBool,
        shared: &Arc<Mutex<Shared>>,
        surface_map: &SurfaceMap,
        ack_tx: &SyncSender<AckMsg>,
    ) -> super::cursor::LayerVerdict {
        let verdict = super::cursor::update_capture_cursor(
            &self.cursor_layer,
            captured,
            shared,
            surface_map,
            ack_tx,
            slot,
        );
        if self.overlay.has_strip() {
            super::cursor::update_capture_cursor(
                &self.overlay.strip_cursor_layer(),
                captured,
                shared,
                surface_map,
                ack_tx,
                slot,
            );
        }
        verdict
    }

    /// Drop the listed guest-released surface ids from this window's frame cache. The
    /// receive thread has already dropped the store's reference (`note_released`); this is the
    /// per-window half. See `drain_releases` in `window/mod.rs` for why it must run on a path
    /// that keeps ticking when nothing is being drawn.
    pub(crate) fn drop_released(&self, ids: &[u32]) {
        let mut cache = self.cache.borrow_mut();
        for &id in ids {
            cache.remove(id);
        }
    }

    /// The surface currently on the layer — what a strip overlay mirrors when it comes up
    /// between presents (an idle guest presents nothing, so without this the band stays black
    /// until something in the guest redraws).
    pub(crate) fn last_surface(&self) -> Option<CFRetained<IOSurfaceRef>> {
        self.last_ca.borrow().clone()
    }

    /// Drop cached frames from a mode that no longer exists — a modeset allocates fresh
    /// surfaces, and ids from the old mode may be reused for something unrelated.
    pub(crate) fn clear_frame_cache(&self) {
        self.cache.borrow_mut().clear();
    }

    /// Put the letterbox `target` (from [`super::fit::refit_target`]) on this window's scanout
    /// layer — re-asserted whenever CA's copy has drifted from it, **checked against the
    /// LAYER, never against a cache of our own intent**. See [`super::layer_frame_differs`]:
    /// AppKit resets a layer-hosting view's layer to the view's bounds on a layout pass, and a
    /// guard on our own unchanged intent makes that permanent. The strip was always
    /// re-asserted this way; the carrier once was not, and that asymmetry was the whole bug.
    pub(crate) fn apply_fit(&self, target: super::fit::FitRect) {
        if super::layer_frame_differs(&self.layer, target) {
            super::set_layer_frame(&self.layer, target);
        }
    }

    /// The guest picture's rect in this window's view space, read from the LAYER — the frame
    /// Core Animation actually holds, not a cache of what we last asked for. This is the one
    /// geometry the input gate measures events against ([`super::input`]'s `target_of`), the
    /// same read [`super::windows::window_of_slot`] does through the content view: one layer,
    /// one rule, so the pixels and the pointer move together by construction. Guarding on a
    /// cache of intent instead is THE fault class the notch work paid for three times
    /// (`docs/design/input-windows-restructure.md` §Move D).
    pub(crate) fn fit(&self) -> super::fit::FitRect {
        let r = self.layer.frame();
        super::fit::FitRect {
            x: r.origin.x,
            y: r.origin.y,
            w: r.size.width,
            h: r.size.height,
        }
    }

    /// How much taller than its content view this window's guest drawing area is right now:
    /// the housing band while this window's strip claims it, zero otherwise. **One
    /// definition**, because its callers — the per-tick refit and the present path's modeset
    /// refit — once disagreed: the present path fitted the guest into the raw content view,
    /// which under `extend` is exactly the housing inset too short. Its write lands on the
    /// carrier's layer, so the picture the strip continues and the picture the carrier shows
    /// came from different panel heights, and the guest's top bar was drawn twice, 33 pt
    /// apart (2026-08-08, caught in a screenshot after every individual number checked out).
    ///
    /// Keyed on the CLAIM, not on whether the strip is on screen — the strip hides while our
    /// Space is away, and rescaling the guest for that would put a reflow back into every
    /// Space switch, which is the whole thing the strip design removes.
    pub(crate) fn strip_inset(&self) -> f64 {
        if self.overlay.claims_band() {
            self.window
                .screen()
                .map(|s| super::hostdisplay::fullscreen_inset(&s))
                .unwrap_or(0.0)
        } else {
            0.0
        }
    }

    /// Measure and record what AppKit's native fullscreen actually withholds for this panel's
    /// camera housing, if this window is natively fullscreen right now. Returns the
    /// observation (also when nothing was learned, for the caller's trace).
    ///
    /// Native fullscreen is the only state that reveals the housing inset's true cost; the
    /// observation lands in the per-panel store that the strip inset, `avoid` sizing and
    /// `describe_panel` all read. Any window may be the only one that ever goes native on its
    /// panel, so every window records — one rule, one store.
    pub(crate) fn learn_native_inset(&self) -> Option<f64> {
        use objc2_app_kit::NSWindowStyleMask;
        if !self
            .window
            .styleMask()
            .contains(NSWindowStyleMask::FullScreen)
        {
            return None;
        }
        let screen = self.window.screen()?;
        let f = screen.frame().size;
        let v = self.view.bounds().size;
        let observed =
            super::fit::fullscreen_inset_measurement((f.width, f.height), (v.width, v.height));
        if let Some(observed) = observed {
            super::hostdisplay::learn_fullscreen_inset(&screen, observed);
        }
        observed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// The stale-IOSurface fault class (`spikes/scanout-blob-freeze/`): the guest presents an
    /// id we cannot resolve. Skipping the frame silently is a permanent freeze for a
    /// non-global scanout — the resolve must ask the worker to publish it again, exactly once
    /// per throttle window.
    #[test]
    fn unresolved_present_asks_the_worker_to_republish() {
        // No real IOSurface plausibly wears a near-MAX global id, so the lookup fallback
        // stays deterministic in a test.
        let id = u32::MAX - 17;
        let map: SurfaceMap = Arc::new(Mutex::new(SurfaceStore::default()));
        let cache = RefCell::new(SurfaceStore::with_cap(present::FRAME_CACHE_CAP));
        let (tx, rx) = sync_channel::<AckMsg>(4);

        assert!(
            resolve_presented(&cache, &map, &tx, id).is_none(),
            "an id nobody holds must not resolve"
        );
        match rx.try_recv() {
            Ok(AckMsg::Resurface(asked)) => assert_eq!(asked, id),
            Ok(AckMsg::Shown(..)) => panic!("an unresolved frame must never ack as shown"),
            Err(_) => panic!("an unresolved id must ask the worker to publish it again"),
        }

        // The ask is throttled per id inside the store: an immediate re-present of the same
        // id (the guest keeps flushing it) must not spam the worker.
        assert!(resolve_presented(&cache, &map, &tx, id).is_none());
        assert!(
            rx.try_recv().is_err(),
            "a second miss inside the throttle window must not re-ask"
        );
    }
}
