// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The limina native window (decision D3: the UI lives in the supervisor process).
//!
//! The worker publishes each guest scanout as a shared IOSurface and reports it over a
//! control fd (`surface <id> <w> <h>` / `frame`). Here, a reader thread parses that and
//! updates shared state; an `NSTimer` on the **main thread** polls the state and updates
//! the window's `CALayer.contents` to the looked-up IOSurface. All AppKit access stays on
//! the main thread; only plain data (ids/counters) crosses the thread boundary.
#![allow(deprecated)] // objc2-io-surface 0.3 renamed some free fns to methods.
//!
//! Split along its seams: `present` (surface
//! store/reader/frame-apply/shown-ack), `cursor` (guest-cursor shape + capture compositing),
//! `lifecycle` (worker connection + quit policy), `diag` (capture / present-copy probes),
//! plus the pre-existing `input` and `capture_tap`. This file keeps `run()` — the window,
//! the render timer, and the event monitor.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication,
    NSApplicationActivationPolicy, NSApplicationDelegate, NSApplicationTerminateReply,
    NSBackingStoreType, NSColor, NSEvent, NSEventMask, NSEventType, NSMenu, NSMenuDelegate,
    NSMenuItem, NSScreen, NSView, NSViewLayerContentsRedrawPolicy, NSWindow,
    NSWindowCollectionBehavior, NSWindowDelegate, NSWindowOcclusionState, NSWindowStyleMask,
};
use objc2_foundation::{
    NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use objc2_io_surface::IOSurfaceLookup;
use objc2_quartz_core::{CALayer, CATransaction};

pub(crate) mod absfit;
pub(crate) mod arrangement;
mod capture_tap;
mod cursor;
mod diag;
mod displays;
mod echo;
pub(crate) mod fit;
/// Recorded-gesture replay for the grab policy (`LIMINA_EDGE_TRACE` fixtures).
#[cfg(test)]
mod grab_fixture;
mod grab_policy;
mod guestwindow;
mod hostdisplay;
mod input;
mod lifecycle;
mod overlay;
mod present;
mod seams;
mod warp;
mod windows;

pub use lifecycle::{WorkerConn, WorkerIo};
pub use present::{
    empty_surface_map, mark_resume_dead, mark_worker_exited, mark_worker_running,
    mark_worker_suspended, mark_worker_swapped, spawn_reader, surface_rendezvous, Shared,
    SurfaceMap,
};

// `input` builds the host pointer's default (blank) shape from the cursor module; re-exported
// here so its `super::blank_cursor()` call keeps working across the split.
pub(crate) use cursor::blank_cursor;

use cursor::apply_cursor;
use lifecycle::{kill_worker_group, should_initiate_quit};
use present::register_apply_hook;

use limina_displayctl::DisplayCommand;

use crate::vmlib::schema::DisplayResolution;
use crate::vmlib::state::WindowState;

/// Everything `run` needs beyond the live worker channels: display policy, remembered
/// window state, and the resize plumbing. Groups what used to be positional arguments.
pub struct WindowOptions {
    pub resize_socket: Option<PathBuf>,
    pub remap: limina_input::keymap::KeyRemap,
    /// Soft keyboard grab: while the window is key, the tap consumes keyboard input (system
    /// combos included) for the guest, mouse left free. `--no-soft-kbd-grab` opts out.
    pub soft_kbd_grab: bool,
    pub title: String,
    /// Display mode: host (drive the guest to the window's screen size, letterboxing the
    /// window), dynamic (guest follows the window — the original behavior), or fixed.
    pub mode: DisplayResolution,
    /// The resolution the worker booted at.
    pub initial_size: (u32, u32),
    /// First-appearance window content size (points), used when there is no remembered
    /// frame — half the display's area at the guest aspect (`fit::default_window_content`).
    pub default_content: (u32, u32),
    /// Remembered NSWindow frame to restore, if it still lands on a screen.
    pub restore_frame: Option<[f64; 4]>,
    /// Identity key of the panel the VM was fullscreen on. Outranks `restore_frame` when that
    /// panel is still attached — see [`restore_placement`]. The same value already decided the
    /// guest's boot resolution in [`screen_info_for_restore`], so the window has to land on the
    /// screen it names or the guest comes back at another panel's size.
    pub fullscreen_display: Option<u64>,
    /// Go fullscreen once the window is on screen — the VM was fullscreen when it last stopped.
    /// Deferred to the first tick rather than done at creation: `toggleFullScreen:` on a window
    /// that has not finished appearing is silently dropped.
    pub start_fullscreen: bool,
    /// Persist window state here (managed VMs: the bundle's state.toml); None = off.
    pub state_path: Option<PathBuf>,
    /// Shared with the reboot-relaunch monitor: every resolution push records itself here
    /// so a relaunched worker boots at the current size (see `session::pack_size`).
    pub desired_size: Arc<std::sync::atomic::AtomicU64>,
    /// What closing the window does (M9.4): suspend (default), shutdown, or ask. Already
    /// resolved by the session — Suspend only arrives when suspend is armed.
    pub on_window_close: crate::vmlib::schema::WindowCloseAction,
    /// Where the last-presented frame is saved when the worker suspends (the restore
    /// splash, next to the snapshot). None = flat-CLI VM, no splash.
    pub splash_save_path: Option<PathBuf>,
    /// A splash to show from window creation until the first presented frame — set when
    /// this boot restores from a snapshot and the splash file exists.
    pub restore_splash: Option<PathBuf>,
    /// The parked window's resume channel (task #18): a menu/CLI suspend keeps the window
    /// open with a play glyph, and the click sends here — the session's monitor thread,
    /// parked in recv(), respawns the worker (which restores from the pending snapshot).
    /// None disables parking: every suspend exits the process.
    pub resume_worker: Option<std::sync::mpsc::Sender<()>>,
    /// The VM-menu context (Suspend gating, Show in Finder, Copy SSH Command).
    pub menu_ctx: MenuCtx,
    /// Drive the guest at the window's screen in device pixels rather than points, so a Retina
    /// panel renders natively (`[display] hidpi`, default on). See [`fit::Scale`].
    pub hidpi: bool,
    /// What fullscreen does with the camera housing on a notched built-in display
    /// (`[display] notch`, default `avoid`). See [`crate::vmlib::schema::NotchPolicy`].
    pub notch: crate::vmlib::schema::NotchPolicy,
    /// Seconds the pointer must be held against a fullscreen guest's edge before it is
    /// released to the rest of the desktop (`[display] edge-resistance`; 0 disables the
    /// grab). This is the top-edge hold; the sides release sooner — see `fit::edge_timing` and
    /// [`crate::vmlib::schema::DisplayCfg::edge_resistance`].
    pub edge_resistance: f64,
    /// How many virtio-gpu scanouts the worker was given (`--display-pool`). The count is fixed
    /// at boot — `num_scanouts` is device-config state read once at probe — so it bounds how
    /// many host panels this VM can ever show.
    pub display_pool: u32,
}

/// The point-to-guest-pixel scale for the screen a window is currently on. Recomputed per use
/// rather than cached: dragging between a Retina and a 1x display changes it, and AppKit reports
/// no screen at all mid-transition (there, the historical 1:1 is the safe answer).
fn scale_for(window: &NSWindow, hidpi: bool) -> fit::Scale {
    match window.screen() {
        Some(screen) => fit::Scale::new(screen.backingScaleFactor(), hidpi),
        None => fit::Scale::none(),
    }
}

/// Point geometry of the screen a windowed boot targets: `frame` is the full screen size
/// (the match-host guest resolution), `visible` the size minus menu bar/Dock (the
/// window-content clamp for the first-appearance default).
pub struct ScreenInfo {
    pub frame: (u32, u32),
    pub visible: (f64, f64),
    /// `NSScreen.backingScaleFactor` — 2.0 on Retina. Match-host multiplies `frame` by it to
    /// get the boot resolution under HiDPI; window sizing stays in points.
    pub backing: f64,
}

/// The screen a remembered window frame lands on (by its midpoint), or the main screen
/// when there is no frame / no match. `None` off the main thread or on a screen-less
/// host — callers fall back to configured sizes. This is how match-host mode derives the
/// initial guest resolution BEFORE any window exists.
///
/// `notch` decides whether `frame` withholds the camera-housing strip on a notched built-in
/// display; the boot resolution has to agree with what the running window will do, or host mode
/// would modeset the guest on the first tick.
/// A remembered fullscreen display wins over the remembered frame.
///
/// They disagree exactly when it matters: the frame is the last *windowed* placement, which may
/// predate the move to the display the VM was fullscreen on. When that panel is no longer
/// attached we fall through to the frame and then to the main screen — "was fullscreen" is the
/// stronger memory, so undocking relocates the VM rather than quietly windowing it.
pub fn screen_info_for_restore(
    frame: Option<[f64; 4]>,
    fullscreen_display: Option<u64>,
    notch: crate::vmlib::schema::NotchPolicy,
) -> Option<ScreenInfo> {
    let mtm = MainThreadMarker::new()?;
    let by_identity = fullscreen_display.and_then(|key| {
        NSScreen::screens(mtm)
            .into_iter()
            .find(|s| hostdisplay::identity_key_of(s) == key)
    });
    let frame = if by_identity.is_some() { None } else { frame };
    let by_midpoint = frame.and_then(|f| {
        let (mx, my) = (f[0] + f[2] / 2.0, f[1] + f[3] / 2.0);
        NSScreen::screens(mtm).into_iter().find(|s| {
            let sf = s.frame();
            mx >= sf.origin.x
                && mx < sf.origin.x + sf.size.width
                && my >= sf.origin.y
                && my < sf.origin.y + sf.size.height
        })
    });
    let screen = by_identity
        .or(by_midpoint)
        .or_else(|| NSScreen::mainScreen(mtm))?;
    let sz = screen.frame().size;
    let vis = screen.visibleFrame().size;
    let (sw, sh) = fit::usable_content(sz.width, sz.height, notch_inset_for(&screen, notch));
    Some(ScreenInfo {
        frame: (sw.round() as u32, sh.round() as u32),
        visible: (vis.width, vis.height),
        backing: screen.backingScaleFactor(),
    })
}

/// The housing height to withhold from the guest on this screen: the screen's real notch under
/// the `avoid` policy, nothing under `extend` (and nothing on a screen with no housing).
fn notch_inset_for(screen: &NSScreen, notch: crate::vmlib::schema::NotchPolicy) -> f64 {
    match notch {
        // Not the housing height but the height AppKit's own fullscreen inset costs, which is
        // the thing the guest has to match under `avoid` — they differ by a point. See
        // [`hostdisplay::fullscreen_inset`].
        crate::vmlib::schema::NotchPolicy::Avoid => hostdisplay::fullscreen_inset(screen),
        crate::vmlib::schema::NotchPolicy::Extend => 0.0,
    }
}

/// The attached screens as [`restore_placement`] needs them.
fn screen_slots(mtm: MainThreadMarker) -> Vec<ScreenSlot> {
    NSScreen::screens(mtm)
        .into_iter()
        .map(|s| {
            let f = s.frame();
            (
                hostdisplay::identity_key_of(&s),
                [f.origin.x, f.origin.y, f.size.width, f.size.height],
            )
        })
        .collect()
}

/// A screen as [`restore_placement`] needs it: its identity key and its Cocoa frame. Pulled out
/// of `NSScreen` by the caller so the placement rule itself is testable without AppKit.
pub type ScreenSlot = (u64, [f64; 4]);

/// Where a restoring window opens, in screen points — `None` means "no usable memory, center on
/// the main screen" (AppKit's `center()`).
///
/// The rule this encodes: **a remembered fullscreen display outranks the remembered frame**, the
/// same precedence [`screen_info_for_restore`] already applies when it picks the guest's boot
/// resolution. The two MUST agree. When they disagreed, a VM sized for the panel it was
/// fullscreen on opened its window somewhere else and fullscreened there, so the guest arrived at
/// the wrong resolution on the wrong glass.
///
/// The frame and the identity disagree in two ordinary situations, and the frame loses both times:
///
/// - The frame is the last *windowed* placement, which may predate the move to the display the VM
///   was fullscreen on — so it can point at a different, still-attached panel.
/// - The frame is absolute Cocoa coordinates, which are only meaningful within one display
///   *arrangement*. Rearranging the displays in System Settings — or a panel coming back at a
///   different origin — strands it off every screen, and the old code then fell all the way back
///   to centering on the main display. Losing the placement to a rearrangement is expected;
///   losing the *panel* is not, because the identity key survives exactly this.
///
/// A frame that is already on the target panel is kept verbatim (the placement is still good);
/// otherwise the remembered size is centered on the target, clamped to fit it. With no identity
/// match — the panel really is gone — the old behavior stands: keep a frame that still lands on
/// some screen, else center.
pub fn restore_placement(
    frame: Option<[f64; 4]>,
    fullscreen_display: Option<u64>,
    screens: &[ScreenSlot],
) -> Option<[f64; 4]> {
    let on_screen = |f: [f64; 4], s: [f64; 4]| {
        f[0] < s[0] + s[2] && f[0] + f[2] > s[0] && f[1] < s[1] + s[3] && f[1] + f[3] > s[1]
    };
    // The panel the VM was fullscreen on, if it is still attached. `find` by identity key, never
    // by display id — ids are reassigned across reboots and hotplugs, which is the very situation
    // the key exists to survive.
    let target = fullscreen_display
        .and_then(|key| screens.iter().find(|(k, _)| *k == key))
        .map(|(_, s)| *s);
    let Some(t) = target else {
        // No remembered panel (or it is gone): keep a frame that still lands somewhere, else center.
        return frame.filter(|f| screens.iter().any(|(_, s)| on_screen(*f, *s)));
    };
    // Midpoint, not intersection: a frame straddling two panels belongs to the one holding most
    // of it, and "already on the target" has to mean the window really opens there.
    let already_there = frame.is_some_and(|f| {
        let (mx, my) = (f[0] + f[2] / 2.0, f[1] + f[3] / 2.0);
        mx >= t[0] && mx < t[0] + t[2] && my >= t[1] && my < t[1] + t[3]
    });
    if already_there {
        return frame;
    }
    // Center the remembered size on the target, never larger than it: a fullscreen record written
    // before any windowed save carries a *screen-sized* frame, which can exceed the panel it is
    // being restored onto.
    let (w, h) = frame.map_or((t[2], t[3]), |f| (f[2].min(t[2]), f[3].min(t[3])));
    Some([t[0] + (t[2] - w) / 2.0, t[1] + (t[3] - h) / 2.0, w, h])
}

/// Constrain interactive window resize to a fixed aspect ratio, so the content view can't
/// be dragged to a shape the guest never fills (which would only grow the letterbox bars).
/// Host mode locks to the display's aspect (re-applied on display migration); fixed mode to
/// the configured WxH; dynamic stays unconstrained — there the guest follows the window, so
/// a free resize is the point. `setContentAspectRatio` only bounds *user* resizing (and the
/// zoom button); it never resizes the window itself, so the boot frame and any restored frame
/// are left as-is and the letterbox still absorbs any residual mismatch until the next drag.
/// A degenerate ratio is ignored (leaves the window free rather than wedging resize).
fn apply_aspect_lock(window: &NSWindow, ratio: (u32, u32)) {
    if ratio.0 > 0 && ratio.1 > 0 {
        window.setContentAspectRatio(NSSize::new(f64::from(ratio.0), f64::from(ratio.1)));
    }
}

/// Apply a fit rect as the scanout layer's frame, with implicit animation off (same
/// reason as `set_layer_surface`: CA's default action would tween the letterbox).
fn set_layer_frame(layer: &CALayer, r: fit::FitRect) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    layer.setFrame(NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h)));
    CATransaction::commit();
}

/// Whether Core Animation's copy of the layer's frame has drifted from the rect we want.
///
/// **Never decide this from a cache of what we last asked for.** AppKit resets a layer-hosting
/// view's layer to the view's bounds on a layout pass — a display reconfiguration is one — and
/// says nothing. Guarding the write on our own intent then means the drift is permanent, because
/// the intent is still whatever it was. That is exactly how a replug left the guest squashed into
/// the carrier's 949 pt view while the strip went on showing the top of the real 980 pt fit: the
/// top bar drawn twice, 33 pt apart, healing only on a fullscreen toggle or the chrome ask —
/// i.e. on the things that happen to *change* the intent (2026-08-08). Compare against the layer.
///
/// A point of tolerance: these rects come from a letterbox division, and CA stores floats.
fn layer_frame_differs(layer: &CALayer, r: fit::FitRect) -> bool {
    let f = layer.frame();
    (f.origin.x - r.x).abs() > 0.5
        || (f.origin.y - r.y).abs() > 0.5
        || (f.size.width - r.w).abs() > 0.5
        || (f.size.height - r.h).abs() > 0.5
}

/// Snapshot the window's frame + content size for state.toml. `None` while fullscreen (the
/// *windowed* frame is what we remember) or before the window has a real size. The `extend`
/// overlay needs no case of its own: it exists only while the carrier is natively fullscreen,
/// and the carrier is what is asked here.
/// One window-placement reading, or `None` when the window is too small to be worth remembering.
///
/// `prev` is the last placement we persisted. It matters only in fullscreen, where this window's
/// own frame IS the screen: overwriting the remembered frame with it would lose the windowed
/// placement the VM should come back to on the way *out* of fullscreen. So fullscreen keeps the
/// previous frame and records the mode plus which panel it is on; only a windowed reading updates
/// the geometry. (First-ever session that goes fullscreen before any save has no `prev` — it then
/// remembers the screen-sized frame, which is harmless: it is only ever a starting point that is
/// immediately fullscreened.)
fn window_state_snapshot(window: &NSWindow, prev: Option<WindowState>) -> Option<WindowState> {
    let f = window.frame();
    let c = window.contentView()?.frame().size;
    let (cw, ch) = (c.width.round() as u32, c.height.round() as u32);
    if cw < 64 || ch < 64 {
        return None;
    }
    let windowed = WindowState {
        frame: [f.origin.x, f.origin.y, f.size.width, f.size.height],
        content: (cw, ch),
        fullscreen: false,
        fullscreen_display: None,
    };
    if !window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return Some(windowed);
    }
    Some(WindowState {
        fullscreen: true,
        fullscreen_display: window.screen().map(|s| hostdisplay::identity_key_of(&s)),
        ..prev.unwrap_or(windowed)
    })
}

/// Best-effort synchronous state save for the exit paths (the periodic saver is async,
/// so a close right after a move could otherwise lose the final position).
fn save_display_slots(path: Option<&Path>, assignment: Vec<(u64, u32)>) {
    let Some(path) = path else { return };
    let slots: Vec<(i64, u32)> = assignment.into_iter().map(|(k, s)| (k as i64, s)).collect();
    if let Err(e) = crate::vmlib::state::set_display_slots(path, slots) {
        log::warn!("display slot assignment save failed: {e}");
    }
}

/// The fullscreen-uses-other-screens switch, saved beside the assignment.
fn save_fullscreen_all(path: Option<&Path>, on: bool) {
    let Some(path) = path else { return };
    if let Err(e) = crate::vmlib::state::set_fullscreen_all_displays(path, on) {
        log::warn!("fullscreen-all-displays save failed: {e}");
    }
}

/// The Input menu's modifier-normalization switch, saved beside the display state.
fn save_modifier_normalize(path: Option<&Path>, on: bool) {
    let Some(path) = path else { return };
    if let Err(e) = crate::vmlib::state::set_modifier_normalize(path, on) {
        log::warn!("modifier-normalization save failed: {e}");
    }
}

/// The set of displays the user has switched off, saved beside the assignment.
fn save_display_disabled(path: Option<&Path>, disabled: Vec<u64>) {
    let Some(path) = path else { return };
    let keys: Vec<i64> = disabled.into_iter().map(|k| k as i64).collect();
    if let Err(e) = crate::vmlib::state::set_display_disabled(path, keys) {
        log::warn!("switched-off display save failed: {e}");
    }
}

fn save_state_final(path: Option<&Path>, window: &NSWindow) {
    let Some(path) = path else { return };
    // The on-disk record is `prev`: the fullscreen branch needs the windowed frame it must not
    // clobber, and disk always holds the last periodic save.
    let prev = crate::vmlib::state::load(path).and_then(|s| s.window);
    let Some(snap) = window_state_snapshot(window, prev) else {
        return;
    };
    // Merge, never whole-save: on a windowed suspend the monitor thread has just written
    // [suspended]; a full save here would consume it and the VM would cold-boot (M9.4-1a).
    if let Err(e) = crate::vmlib::state::set_window(path, Some(snap)) {
        log::warn!("window state save failed: {e}");
    }
}

/// The `on_window_close = ask` dialog: Suspend / Shut Down / Cancel (→ `None`). Modal on the
/// main thread — the render timer pauses for the answer, which is fine (the guest keeps
/// running; frames resume with the next tick).
fn ask_close_action(
    mtm: MainThreadMarker,
    vm_name: &str,
) -> Option<crate::vmlib::schema::WindowCloseAction> {
    use crate::vmlib::schema::WindowCloseAction;
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!("Close “{vm_name}”?")));
    alert.setInformativeText(&NSString::from_str(
        "Suspend parks the VM and resumes it the next time it starts; \
         Shut Down powers the guest off.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Suspend"));
    alert.addButtonWithTitle(&NSString::from_str("Shut Down"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    let r = alert.runModal();
    if r == NSAlertFirstButtonReturn {
        Some(WindowCloseAction::Suspend)
    } else if r == NSAlertSecondButtonReturn {
        Some(WindowCloseAction::Shutdown)
    } else {
        None
    }
}

/// Push a window-resize to the worker over its display-control socket.
fn send_resize(path: &Path, slot: u32, width: u32, height: u32) {
    // `Resize` is the short form for connector 0. A window on a panel that owns another slot
    // must not use it — it would modeset whichever connector happens to be slot 0, which in a
    // two-panel arrangement is a display the user is not resizing.
    let command = if slot == 0 {
        DisplayCommand::Resize { width, height }
    } else {
        DisplayCommand::Display(limina_displayctl::DisplayControl {
            display_id: slot,
            size: Some((width, height)),
            ..Default::default()
        })
    };
    send_display_command(path, command);
}

/// Push a display command to the worker over its display-control socket (off the AppKit main
/// thread — a brief connect/write must never beachball the UI). Best-effort: a failure just
/// means this gesture's update is dropped; the next one retries.
fn send_display_command(path: &Path, command: DisplayCommand) {
    send_display_commands(path, vec![command]);
}

/// How long to leave the connector down before advertising the new display.
///
/// **The guest needs to *observe* the down state, and that takes wall-clock time.** Our device
/// layer delivers the unplug and the replug as two distinct, ordered config-change events no
/// matter how fast they are written — but a guest that receives both within microseconds
/// coalesces its own re-probe and only ever sees the end state: it picks up the new mode list
/// and keeps the previous monitor's identity, which is exactly the staleness the cycle exists to
/// prevent. Measured on synoik: back-to-back writes leave the identity stale while the modes
/// update; ≥50 ms re-reads it, 3/3. 60 ms buys margin over that floor and is still far below
/// anything visible — the user watched a cycle and saw nothing.
///
/// This is also why an experiment driven by one `nc` per command "proves" no delay is needed:
/// process spawn silently supplies milliseconds. See `spikes/display-identity-hotplug/`.
const CONNECTOR_DOWN_SETTLE: std::time::Duration = std::time::Duration::from_millis(60);

/// How long the captured display must draw no cursor before it is worth saying so
/// ([`cursor::undrawn_fault`]). Long enough that taking the grab, a fresh window and a slot
/// waiting for its first cursor image all pass in silence; far short of the real fault, which
/// stands until the guest re-uploads.
const CURSOR_FAULT_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Push a sequence of display commands **in order**, one connection each, serialized with every
/// other sequence.
///
/// The ordering is the whole point, at both levels. Within a batch: a migration cycle is an
/// unplug followed by a replug, and losing that order leaves the connector down with nothing
/// queued to bring it back — so a batch is written by one thread with blocking writes. Across
/// batches: the settle sleep below parks a batch mid-cycle, and a batch spawned for a later
/// event (a second migration, a dynamic-mode resize) overtaking it lands the earlier replug
/// LAST — the guest keeps a stale identity that nothing repairs, because `identity_sent` was
/// already advanced on the main thread. So all batches drain through ONE queue and one sender
/// thread, submission order = wire order. The worker applies one update per wake and re-kicks
/// its own eventfd while any remain, so each command still reaches the guest as its own
/// config-change event (`third_party/libkrun/src/devices/src/virtio/gpu/{device,worker}.rs`) —
/// we owe it order, and [`CONNECTOR_DOWN_SETTLE`] after an unplug.
fn send_display_commands(path: &Path, commands: Vec<DisplayCommand>) {
    if commands.is_empty() {
        return;
    }
    type Batch = (PathBuf, Vec<DisplayCommand>);
    static SENDER: std::sync::OnceLock<std::sync::mpsc::Sender<Batch>> = std::sync::OnceLock::new();
    let tx = SENDER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Batch>();
        std::thread::spawn(move || {
            while let Ok((path, commands)) = rx.recv() {
                run_display_batch(&path, &commands);
            }
        });
        tx
    });
    if tx.send((path.to_path_buf(), commands)).is_err() {
        log::error!("display-control: the sender thread is gone; commands dropped");
    }
}

/// One batch's wire session: a connection per command, blocking writes, the settle after an
/// unplug, and the bring-the-connector-back recovery if the cycle dies half-way. Runs only on
/// [`send_display_commands`]'s single sender thread — an early return abandons this batch, not
/// the queue.
fn run_display_batch(path: &Path, commands: &[DisplayCommand]) {
    {
        use std::io::Write;
        // Which connector this batch has taken down and not yet brought back, so the recovery
        // below re-connects the one that is actually dark rather than assuming slot 0.
        let mut left_disconnected: Option<u32> = None;
        for command in commands {
            let line = command.to_wire();
            let sent = match std::os::unix::net::UnixStream::connect(path) {
                Ok(mut stream) => match writeln!(stream, "{line}") {
                    Ok(()) => {
                        log::info!("display-control: pushed {line:?} to the guest");
                        true
                    }
                    Err(e) => {
                        log::warn!("display-control: send {line:?} failed: {e}");
                        false
                    }
                },
                Err(e) => {
                    log::warn!("display-control: connect {path:?} failed: {e}");
                    false
                }
            };
            if !sent {
                // Abandoning a cycle half-way is worse than not starting it: a guest left
                // disconnected has no display at all, where a stale identity is merely wrong.
                // Recovery is unconditional — a bare reconnect is harmless on a connector that
                // is already up.
                if let Some(dark) = left_disconnected {
                    log::error!(
                        "display-control: a migration cycle failed after the unplug; \
                         forcing connector {dark} back up"
                    );
                    let bare = DisplayCommand::Display(limina_displayctl::DisplayControl {
                        display_id: dark,
                        connected: Some(true),
                        ..Default::default()
                    });
                    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(path) {
                        let _ = writeln!(stream, "{}", bare.to_wire());
                    }
                }
                return;
            }
            if let DisplayCommand::Display(control) = command {
                if control.connected == Some(false) {
                    left_disconnected = Some(control.display_id);
                    // Give the guest time to actually process the unplug before the display
                    // comes back — see CONNECTOR_DOWN_SETTLE. This runs on the sender thread,
                    // never the main thread, so the window does not stall for it.
                    std::thread::sleep(CONNECTOR_DOWN_SETTLE);
                } else if control.connected == Some(true)
                    && left_disconnected == Some(control.display_id)
                {
                    left_disconnected = None;
                }
            }
        }
    }
}

/// What the VM menu's items need to know about THIS VM (M9.4). Set once by `run` before
/// the menu is built; read by the action methods and the Dock-menu builder. Main-thread
/// only, like all menu state.
#[derive(Clone, Default)]
pub(crate) struct MenuCtx {
    /// Suspend is armed (managed VM: snapshot + state paths both set) — gates the
    /// Suspend item.
    pub(crate) suspend_armed: bool,
    /// The VM's .liminavm bundle directory — gates Show in Finder.
    pub(crate) bundle_dir: Option<PathBuf>,
    /// The ready-to-paste SSH command (NAT gateway forward) — gates Copy SSH Command.
    pub(crate) ssh_cmd: Option<String>,
}

thread_local! {
    static MENU_CTX: RefCell<MenuCtx> = RefCell::new(MenuCtx::default());

    /// What the Displays menu shows, republished by the render timer whenever it changes.
    /// The menu is built from this rather than from the slot table directly, because the table
    /// lives inside the timer's closure and AppKit asks for the menu on its own schedule.
    static DISPLAY_MENU: RefCell<Vec<DisplayMenuRow>> = const { RefCell::new(Vec::new()) };

    /// Displays the user clicked, `(panel key, wanted on)`, drained by the render timer. The
    /// click cannot act directly: switching a display off is a connector cycle, and those are
    /// planned in one place.
    static DISPLAY_TOGGLES: RefCell<Vec<(u64, bool)>> = const { RefCell::new(Vec::new()) };

    /// The Displays menu's "Use Other Screens When Fullscreen" switch — whether fullscreen
    /// lights up every attached panel or takes only the window's own. Off by default; restored
    /// from the VM's state and persisted on change by the render timer, which also reads it
    /// each tick when deciding the presentation (no drain queue needed: a mode bool is not a
    /// connector cycle, the next tick's plan is where it takes effect).
    static FULLSCREEN_ALL_DISPLAYS: Cell<bool> = const { Cell::new(false) };

    /// The Input menu's "Modifier normalization" switch. Seeded from the VM's configuration
    /// (`[input] normalize_modifiers`, default on) when the window comes up, then overridden by
    /// a remembered menu choice from the per-VM state. The render timer hands each change to the
    /// input translator — which drains the keyboard through the old mapping before adopting the
    /// new one — and persists it beside the display switches.
    static MODIFIER_NORMALIZE: Cell<bool> = const { Cell::new(true) };
}

/// Find the menu row a clicked item names. The item's tag is the row's PANEL KEY (bit-cast),
/// never its index: the render timer republishes `DISPLAY_MENU` while the menu can be open, so
/// by click time the list may have shifted (a panel unplugged) — an index would land on a
/// different, still-valid row and silently switch the wrong display. Identity either finds the
/// same display or, for a panel that has since gone, nothing.
fn menu_row_for_tag(rows: &[DisplayMenuRow], tag: isize) -> Option<&DisplayMenuRow> {
    let panel = tag as u64;
    rows.iter().find(|r| r.panel == panel)
}

/// One row of the Displays menu.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DisplayMenuRow {
    /// The host panel this row switches, keyed the way the slot table keys it.
    pub(crate) panel: u64,
    /// What to call it — the panel's own name, as the guest is told it.
    pub(crate) name: String,
    pub(crate) enabled: bool,
    /// The panel the VM's window is on. Its row is checked and dead: switching off the display
    /// you are looking at has no sensible outcome.
    pub(crate) primary: bool,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop; the action
    // signatures below match AppKit's target/action convention.
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "LiminaVmMenuActions"]
    pub struct VmMenuActions;

    unsafe impl NSObjectProtocol for VmMenuActions {}

    unsafe impl NSApplicationDelegate for VmMenuActions {
        // A quit Apple event (osascript "quit", logout) must not exit the
        // supervisor abruptly — that orphans the worker (live-reproduced).
        // Cancel the terminate and route into the same graceful
        // stop the Ctrl-C/window-close path uses; the render timer drives the
        // shutdown ladder and exits the process when the guest is down.
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(
            &self,
            _app: &NSApplication,
        ) -> NSApplicationTerminateReply {
            crate::supervisor::request_stop();
            NSApplicationTerminateReply::TerminateCancel
        }

        // The Dock icon's context menu carries the same VM verbs as the menu bar.
        #[unsafe(method_id(applicationDockMenu:))]
        fn application_dock_menu(&self, _app: &NSApplication) -> Option<Retained<NSMenu>> {
            Some(build_vm_menu(self.mtm(), self))
        }
    }

    // Both dynamic submenus are rebuilt every time they open — Displays because its rows are
    // the host's attached panels, Input because its checkmark can be moved from elsewhere. One
    // delegate serves both, so it must ask WHICH menu opened; repopulating by title is the only
    // identity an NSMenu hands its delegate here.
    unsafe impl NSMenuDelegate for VmMenuActions {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            if menu.title().to_string() == "Input" {
                populate_input_menu(menu, self.mtm(), self);
            } else {
                populate_displays_menu(menu, self.mtm(), self);
            }
        }
    }

    impl VmMenuActions {
        // "Control Center…" / "Settings…": show the running center or spawn a fresh one —
        // the way back to the center (where VM configuration lives), Parallels-style.
        #[unsafe(method(showControlCenter:))]
        fn show_control_center(&self, _sender: &NSMenuItem) {
            if let Err(e) = crate::center::show_or_spawn() {
                log::warn!("opening the control center: {e:#}");
            }
        }

        // Suspend: same path as close-to-suspend / `limina suspend` — the monitor relays
        // the bracket, the overlay dims the live frame, and the window PARKS on the exit
        // (task #18: play glyph, click to resume). While parked the same item reads
        // "Resume" (validateMenuItem:) and routes to the play action.
        #[unsafe(method(suspendVm:))]
        fn suspend_vm(&self, _sender: &NSMenuItem) {
            if parked() {
                log::info!("menu: Resume");
                RESUME_REQUESTED.with(|r| r.set(true));
            } else {
                log::info!("menu: Suspend");
                crate::supervisor::request_suspend();
            }
        }

        // Keep the VM verbs honest across the parked lifecycle (task #18): while parked,
        // "Suspend" becomes "Resume", and Shut Down / Force Stop go dead (there is no
        // worker to stop — quitting the app or clicking play are the verbs that exist).
        // While RESUMING, Suspend goes dead too (mid-respawn) but the stop verbs stay —
        // they are the escape hatch from a hung resume.
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> objc2::runtime::Bool {
            let phase = PARK_STATE.with(|p| p.get());
            let action = item.action();
            if action == Some(objc2::sel!(suspendVm:)) {
                let title = if phase == ParkPhase::Parked {
                    "Resume"
                } else {
                    "Suspend"
                };
                item.setTitle(&NSString::from_str(title));
                return (phase != ParkPhase::Resuming).into();
            }
            if action == Some(objc2::sel!(shutDownVm:))
                || action == Some(objc2::sel!(forceStopVm:))
            {
                return (phase != ParkPhase::Parked).into();
            }
            if action == Some(objc2::sel!(toggleDisplay:)) {
                let tag = item.tag();
                let primary = DISPLAY_MENU
                    .with(|r| menu_row_for_tag(&r.borrow(), tag).is_some_and(|row| row.primary));
                return (!primary).into();
            }
            objc2::runtime::Bool::YES
        }

        // Shut Down: the graceful power-off ladder (agent shutdown → power button →
        // SIGKILL after grace) — identical to Ctrl-C / `limina stop`.
        #[unsafe(method(shutDownVm:))]
        fn shut_down_vm(&self, _sender: &NSMenuItem) {
            log::info!("menu: Shut Down");
            crate::supervisor::request_stop();
        }

        // Force Stop: SIGKILL now, no grace — the hung-guest escape hatch.
        #[unsafe(method(forceStopVm:))]
        fn force_stop_vm(&self, _sender: &NSMenuItem) {
            log::warn!("menu: Force Stop (no grace)");
            crate::supervisor::request_force_stop();
        }

        // Displays ▸ a host panel: switch that display on or off for the guest. The click only
        // records the intent — the render timer drains it into the slot table, which is where
        // every connector cycle is planned.
        #[unsafe(method(toggleDisplay:))]
        fn toggle_display(&self, sender: &NSMenuItem) {
            let tag = sender.tag();
            let Some(row) = DISPLAY_MENU.with(|r| menu_row_for_tag(&r.borrow(), tag).cloned())
            else {
                return;
            };
            if row.primary {
                return;
            }
            log::info!(
                "menu: display {} switched {}",
                row.name,
                if row.enabled { "off" } else { "on" }
            );
            DISPLAY_TOGGLES.with(|t| t.borrow_mut().push((row.panel, !row.enabled)));
        }

        // Displays ▸ Use Other Screens When Fullscreen: whether fullscreen takes over the
        // other panels. The flip is read by the render timer's next plan, in or out of
        // fullscreen alike.
        #[unsafe(method(toggleFullscreenAllDisplays:))]
        fn toggle_fullscreen_all_displays(&self, _sender: &NSMenuItem) {
            let on = !FULLSCREEN_ALL_DISPLAYS.with(|f| f.get());
            log::info!(
                "menu: fullscreen {} other screens",
                if on { "takes" } else { "leaves" }
            );
            FULLSCREEN_ALL_DISPLAYS.with(|f| f.set(on));
        }

        // Input ▸ Modifier normalization: whether the Mac's modifier row is normalized onto
        // the PC row the guest expects. The render timer hands the flip to the translator.
        #[unsafe(method(toggleModifierNormalization:))]
        fn toggle_modifier_normalization(&self, _sender: &NSMenuItem) {
            let on = !MODIFIER_NORMALIZE.with(|f| f.get());
            MODIFIER_NORMALIZE.with(|f| f.set(on));
        }

        // Show in Finder: reveal the .liminavm bundle.
        #[unsafe(method(revealVm:))]
        fn reveal_vm(&self, _sender: &NSMenuItem) {
            let Some(dir) = MENU_CTX.with(|c| c.borrow().bundle_dir.clone()) else {
                return;
            };
            let url = objc2_foundation::NSURL::fileURLWithPath(&NSString::from_str(
                &dir.to_string_lossy(),
            ));
            objc2_app_kit::NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(
                &objc2_foundation::NSArray::from_retained_slice(&[url]),
            );
        }

        // Copy SSH Command: the NAT gateway's inbound forward, ready to paste.
        #[unsafe(method(copySshVm:))]
        fn copy_ssh_vm(&self, _sender: &NSMenuItem) {
            let Some(cmd) = MENU_CTX.with(|c| c.borrow().ssh_cmd.clone()) else {
                return;
            };
            unsafe {
                let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
                pb.clearContents();
                pb.setString_forType(
                    &NSString::from_str(&cmd),
                    objc2_app_kit::NSPasteboardTypeString,
                );
            }
        }
    }
);

/// Republish what the Displays menu shows, if it changed. Called from the render timer, which
/// is the only place that knows both the slot table and which panel the window is on.
fn publish_display_menu(
    table: &displays::DisplayTable,
    names: &[(u64, String)],
    primary: Option<u64>,
) {
    let attached: Vec<u64> = names.iter().map(|(k, _)| *k).collect();
    let rows: Vec<DisplayMenuRow> = table
        .rows(&attached)
        .into_iter()
        .map(|row| DisplayMenuRow {
            panel: row.panel,
            name: names
                .iter()
                .find(|(k, _)| *k == row.panel)
                .map(|(_, n)| n.clone())
                .unwrap_or_default(),
            enabled: row.enabled,
            primary: primary == Some(row.panel),
        })
        .collect();
    DISPLAY_MENU.with(|r| {
        if *r.borrow() != rows {
            *r.borrow_mut() = rows;
        }
    });
}

/// Build the "Displays" menu: one checkable row per host panel the guest has a connector for.
///
/// Empty until the guest's own driver is up and the panels have been given slots, which is also
/// exactly when switching one off would mean anything.
fn build_displays_menu(mtm: MainThreadMarker, actions: &VmMenuActions) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setTitle(&NSString::from_str("Displays"));
    // Rows come and go with the host's monitors, so they are built on open, not once.
    menu.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(actions)));
    populate_displays_menu(&menu, mtm, actions);
    menu
}

fn populate_displays_menu(menu: &NSMenu, mtm: MainThreadMarker, actions: &VmMenuActions) {
    menu.removeAllItems();
    // The mode switch first: it is what decides whether the rows below ever light up.
    let all = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Use Other Screens When Fullscreen"),
            Some(objc2::sel!(toggleFullscreenAllDisplays:)),
            &NSString::from_str(""),
        )
    };
    all.setState(if FULLSCREEN_ALL_DISPLAYS.with(|f| f.get()) {
        objc2_app_kit::NSControlStateValueOn
    } else {
        objc2_app_kit::NSControlStateValueOff
    });
    unsafe { all.setTarget(Some(actions)) };
    menu.addItem(&all);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let rows = DISPLAY_MENU.with(|r| r.borrow().clone());
    if rows.is_empty() {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("No displays yet"),
                None,
                &NSString::from_str(""),
            )
        };
        item.setEnabled(false);
        menu.addItem(&item);
        return;
    }
    for row in rows.iter() {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&row.name),
                Some(objc2::sel!(toggleDisplay:)),
                &NSString::from_str(""),
            )
        };
        // The tag is the PANEL KEY (bit-cast; isize round-trips u64 exactly), which is how the
        // action and the validation find their way back to a display without the menu holding
        // one — and without an index that goes stale when the render timer republishes the rows
        // under an open menu. See `menu_row_for_tag`.
        item.setTag(row.panel as isize);
        item.setState(if row.enabled {
            objc2_app_kit::NSControlStateValueOn
        } else {
            objc2_app_kit::NSControlStateValueOff
        });
        unsafe { item.setTarget(Some(actions)) };
        menu.addItem(&item);
    }
}

/// Build the "Input" menu.
///
/// One row for now. It is a menu rather than a checkbox somewhere in Settings because what it
/// controls is felt continuously while typing, and the answer differs per guest — the row is
/// where a hand already reaching for the menu bar can find it.
fn build_input_menu(mtm: MainThreadMarker, actions: &VmMenuActions) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setTitle(&NSString::from_str("Input"));
    // Built on open so the checkmark tracks a flip made from anywhere else (the config seeds it,
    // the saved state can override it, and a future keybinding could move it too).
    menu.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(actions)));
    populate_input_menu(&menu, mtm, actions);
    menu
}

fn populate_input_menu(menu: &NSMenu, mtm: MainThreadMarker, actions: &VmMenuActions) {
    menu.removeAllItems();
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Modifier Normalization"),
            Some(objc2::sel!(toggleModifierNormalization:)),
            &NSString::from_str(""),
        )
    };
    item.setState(if MODIFIER_NORMALIZE.with(|f| f.get()) {
        objc2_app_kit::NSControlStateValueOn
    } else {
        objc2_app_kit::NSControlStateValueOff
    });
    unsafe { item.setTarget(Some(actions)) };
    menu.addItem(&item);
}

/// Build the "Virtual Machine" verbs menu (shared between the menu bar and the Dock menu).
/// Items whose prerequisite is absent (no bundle, no SSH forward, suspend unarmed) are
/// simply not added — the menu never shows dead verbs.
fn build_vm_menu(mtm: MainThreadMarker, actions: &VmMenuActions) -> Retained<NSMenu> {
    let ctx = MENU_CTX.with(|c| c.borrow().clone());
    let menu = NSMenu::new(mtm);
    menu.setTitle(&NSString::from_str("Virtual Machine"));
    let add = |title: &str, sel: objc2::runtime::Sel, key: &str| {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(sel),
                &NSString::from_str(key),
            )
        };
        unsafe { item.setTarget(Some(actions)) };
        menu.addItem(&item);
    };
    // The Dock menu is rebuilt on every open, so bake the parked-lifecycle titles/verbs in
    // here as well as in validateMenuItem: (which the menu-bar copy relies on).
    let phase = PARK_STATE.with(|p| p.get());
    if ctx.suspend_armed && phase != ParkPhase::Resuming {
        let title = if phase == ParkPhase::Parked {
            "Resume"
        } else {
            "Suspend"
        };
        add(title, objc2::sel!(suspendVm:), "");
    }
    if phase != ParkPhase::Parked {
        add("Shut Down", objc2::sel!(shutDownVm:), "");
        add("Force Stop", objc2::sel!(forceStopVm:), "");
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add("Settings…", objc2::sel!(showControlCenter:), ",");
    if ctx.bundle_dir.is_some() {
        add("Show in Finder", objc2::sel!(revealVm:), "");
    }
    if ctx.ssh_cmd.is_some() {
        add("Copy SSH Command", objc2::sel!(copySshVm:), "");
    }
    menu
}

thread_local! {
    /// Set by [`WindowCloseInterceptor`] when the user asked to close the window (red
    /// button / Cmd-W); consumed by the render timer, which routes it through the
    /// `on_window_close` policy. Main-thread only, like all window state.
    static CLOSE_REQUESTED: Cell<bool> = const { Cell::new(false) };

    /// Where the window is in the suspended-window lifecycle (task #18). Written by the
    /// render timer; read by the VM-menu validation and the input monitor (all main
    /// thread). See [`ParkPhase`].
    static PARK_STATE: Cell<ParkPhase> = const { Cell::new(ParkPhase::Live) };

    /// The play click (content-view click or the menu's Resume) — consumed by the render
    /// timer, which signals the session's monitor thread. Main-thread only.
    static RESUME_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

/// The suspended-window lifecycle (task #18): a menu/CLI suspend parks the window (last
/// frame under a scrim + play glyph) instead of exiting; the play click respawns the
/// worker, which restores from the pending snapshot in the same window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ParkPhase {
    /// Normal operation (also during a restore/suspend overlay — the worker is live or
    /// about to be).
    Live,
    /// The worker suspended and the window stayed up showing the play glyph.
    Parked,
    /// Play was clicked; the respawned worker is restoring. Back to `Live` on its first
    /// presented frame.
    Resuming,
}

/// Is the window currently parked on a suspended VM? (For the menu handlers, which live
/// outside the timer closure.)
pub(crate) fn parked() -> bool {
    PARK_STATE.with(|p| p.get()) == ParkPhase::Parked
}

/// Is there a guest behind this window for the tick to act for? Pure policy.
///
/// The pointer work the tick polls — the grab, the echo follow, the repark, the blank's upkeep,
/// the mapping probe — all serve a guest, and a window parked on a snapshot has none. Nor does
/// one mid-resume: the fresh worker has not presented yet. Both event paths already carry this
/// rule (`capture_tap`'s early return, the NSEvent monitor's parked arm); the tick is the third
/// place it has to hold.
///
/// Rig 2026-08-22: without it, a suspended VM's fullscreen window "gained the screen" on every
/// visit to its Space and took the pointer with it — hidden and pinned, over a guest that did
/// not exist. `parked()` alone is not the question, because the grabs continued through the
/// Resuming phase too.
pub(crate) fn speaks_for_a_guest(phase: ParkPhase) -> bool {
    matches!(phase, ParkPhase::Live)
}

/// What a suspend exit does with the window: park it (menu/CLI suspend — the user kept the
/// window, so keep the VM one click away), or quit the process (the user closed the window
/// or asked to stop — the window's disappearance is the point). Pure policy, decided once
/// per suspend exit.
///
/// `can_park` = the session wired a resume channel (parking without one would strand the
/// window: the play click could never respawn).
pub(crate) fn should_park_on_suspend(
    close_requested: bool,
    stop_requested: bool,
    can_park: bool,
) -> bool {
    can_park && !close_requested && !stop_requested
}

/// Has the resume's fresh worker presented? Pure policy for the timer's Resuming arm — what
/// takes the "Resuming…" overlay down and makes the window live again.
///
/// The epoch is the discriminator, not the frame count. A per-slot counter cannot carry the
/// question across the swap: `mark_worker_swapped` clears the slots, so the fresh worker starts
/// counting from zero, and comparing against the count at the play click asks it to out-present
/// the whole suspended session first. Rig 2026-08-22: the guest came back on both panels and the
/// overlay stayed up regardless — on an idle desktop it need never come down at all.
pub(crate) fn resume_first_frame(worker_epoch: u64, epoch_at_click: u64, frames: u64) -> bool {
    worker_epoch > epoch_at_click && frames > 0
}

/// Did the RESUME's fresh worker die before its first frame? Pure policy for the timer's
/// Resuming arm. The trap it exists to avoid: during a normal resume the OLD worker's
/// `exited` flag stays set from the play click until the monitor thread swaps the fresh
/// worker in and calls `mark_worker_running` — so `exited` alone cannot distinguish "the
/// swap hasn't happened yet" from "the fresh worker crashed". `epoch > baseline` proves a
/// swap happened after the click (the flags describe the FRESH worker), and `resume_dead`
/// covers the respawn never reaching a swap at all (spawn/gateway failure).
pub(crate) fn resume_worker_died(
    exited: bool,
    resume_dead: bool,
    worker_epoch: u64,
    epoch_at_click: u64,
) -> bool {
    resume_dead || (exited && worker_epoch > epoch_at_click)
}

define_class!(
    // SAFETY: NSWindow tolerates subclassing; no Drop; no ivars. The only overrides are the
    // two key-eligibility predicates, and they return the value a titled window would return
    // anyway — so this class behaves exactly like NSWindow until the window goes borderless.
    #[unsafe(super = NSWindow)]
    #[thread_kind = MainThreadOnly]
    #[name = "LiminaWindow"]
    pub(crate) struct LiminaWindow;

    unsafe impl NSObjectProtocol for LiminaWindow {}

    impl LiminaWindow {
        /// A **borderless** `NSWindow` refuses key status, and the `extend` overlay
        /// ([`ExtendOverlay`]) must be borderless — it is the only style the compositor
        /// will let draw beside the camera housing (titled + `fullSizeContentView` is masked;
        /// measured in `spikes/notch-fullscreen/` round 2). A VM window that cannot take
        /// keyboard focus is useless, so override the refusal.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            true
        }
    }
);

/// One `[GRABSTATE]` sample: `(captured, key, on_active_space, has_screen, app_active,
/// occlusion_visible)`.
type GrabStateTrace = (bool, bool, bool, bool, bool, bool);

/// Window level for the `extend` overlay: above the menu bar, so the chrome cannot reveal over
/// the guest. `NSMainMenuWindowLevel` is 24.
///
/// Constant while the overlay is up — the level is not a lever for getting out of something's
/// way. Yielding used to be a level drop to 0 rather than putting the overlay down, which looked
/// cheaper (no re-parent, so no visible reflow) and was wrong in a way only dogfood found
/// (2026-08-08): at level 0 the overlay sits **below the carrier**, whose content view is the
/// empty placeholder [`ExtendOverlay::show`] leaves behind, so the guest did not shrink below the
/// housing, it disappeared — a whole-screen black flash for as long as the yield lasted.
/// [`overlay_yields`] now decides whether the overlay is up at all.
pub(crate) const OVERLAY_LEVEL: isize = 25;

/// One tick's worth of overlay stacking state, compared to suppress repeats in the trace.
#[derive(Clone, Copy, PartialEq, Eq)]
struct OverlaySnapshot {
    active: bool,
    ours: u32,
    focused: u32,
    want: isize,
    have: isize,
}

/// Whether the overlay must get out of the way of something on screen.
///
/// Nothing can appear over an above-menu-bar window — that is the whole point of the overlay —
/// so there has to be one condition under which it stands down, or a system dialog on our screen
/// is unreachable. The Accessibility prompt is the case that matters, since that prompt is how
/// limina earns its capture tap in the first place.
///
/// Three things narrow it, each learned from a symptom:
///
/// - **Where the focused window is.** Dropping whenever limina was merely *inactive* black-strips
///   a guest that is still perfectly visible: focus something on an external display and the
///   built-in's top strip goes black (the fullscreen Space's housing backdrop, drawn at
///   `NSMainMenuWindowLevel`, showing through). A window focused on another display has no claim
///   on our stacking at all.
/// - **How long that has held.** `NSScreen::mainScreen` lags the activation change — for a tick or
///   two after focus leaves for another display, `isActive` is already false while `mainScreen`
///   still names ours, which reads exactly like a dialog opening here. Measured: 8 such transients
///   in one switching session. A dialog is covered for at most [`OVERLAY_SETTLE`]; a stale sample
///   never gets the chance.
/// - **Whether our Space is even showing.** A Space switch on our own display is indistinguishable
///   from a dialog by the first two — limina resigns active, `mainScreen` stays ours — but there is
///   nothing on screen to yield *to*, and yielding mid-switch is visible: it lands in the middle of
///   the animation back, which is exactly when the user is looking at it.
///
/// Returns the updated hold timer and whether to yield **this tick**.
///
/// The timer measures the *whole* condition, which is the only version of it that works. Timing
/// just "inactive and the focus is here" leaves the clock running for the entire time the user
/// spends on another Space — so it is long expired when they come back, and the yield fires the
/// instant `on_active_space` turns true, in the one frame after the animation, hiding the overlay
/// until limina finishes activating. That is the reported snap wearing a different hat
/// (2026-08-08, third round): perfect rendering, then a frame inset below the housing, then back.
fn yield_step(
    app_active: bool,
    focus_on_our_screen: bool,
    on_active_space: bool,
    held_since: Option<std::time::Instant>,
    now: std::time::Instant,
) -> (Option<std::time::Instant>, bool) {
    let condition = !app_active && on_active_space && focus_on_our_screen;
    let since = match (condition, held_since) {
        (false, _) => None,
        (true, Some(started)) => Some(started),
        (true, None) => Some(now),
    };
    let yields = since.is_some_and(|started| now.duration_since(started) >= OVERLAY_SETTLE);
    (since, yields)
}

/// How long "inactive, and the focus is on our screen" must hold before the overlay gives up its
/// place above the menu bar. Long enough to outlast `mainScreen`'s lag behind activation, short
/// enough that a system dialog is not meaningfully delayed.
const OVERLAY_SETTLE: Duration = Duration::from_millis(400);

/// Whether the window with keyboard focus is on the same display as `screen`.
///
/// `NSScreen::mainScreen` is "the screen with the key window", and it follows focus system-wide
/// rather than within our app — which is what makes it answerable while we are inactive, i.e.
/// exactly when [`overlay_level`] needs it.
///
/// Unknown answers conservatively as `true`: that yields the old always-drop behavior, so a
/// missing screen can only cost the black strip, never a covered system dialog.
/// Whether to log overlay level/space transitions to stderr (`LIMINA_OVERLAY_TRACE=1`).
fn overlay_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIMINA_OVERLAY_TRACE").is_some_and(|v| v != "0"))
}

/// Whether to log every display-sizing decision to stderr (`LIMINA_DISPLAY_TRACE=1`): what
/// fullscreen inset was measured and for which display id, what size the host-mode push
/// derived from it, and what the window was reshaped to. The oracle for the class of bug
/// where the guest resolution and the window's own geometry feed each other — a host display
/// hotplug drove eight modesets converging 114 px short of the screen (2026-08-08).
fn display_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIMINA_DISPLAY_TRACE").is_some_and(|v| v != "0"))
}

/// Shared zero for the trace timestamps, so lines from different traces sit on one timeline.
fn trace_clock() -> &'static std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now)
}

/// Wall-clock milliseconds. The other traces are relative to [`trace_clock`], which is enough to
/// time our own signals against each other — but the notch strip's bugs are about what the
/// *window server* does to a window between our ticks, and the only way to see that is to
/// interleave our log with an outside observer's (`spikes/notch-fullscreen/flash-detector.swift`,
/// which polls `CGWindowListCopyWindowInfo`). That needs a clock both can name.
fn epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// One `[STRIP]` line, under `LIMINA_OVERLAY_TRACE`, reporting what *we* believe the strip
/// window's frame is at the moment we act on it. Paired with the detector's view of the same
/// window, it separates "we revealed it at the wrong place" from "the window server moved it and
/// AppKit never told us" — which need opposite fixes.
fn strip_trace(what: &str, window: Option<&NSWindow>, want: Option<(f64, f64, f64, f64)>) {
    if !overlay_trace() {
        return;
    }
    let have = window.map(|w| {
        let f = w.frame();
        (f.origin.x, f.origin.y, f.size.width, f.size.height)
    });
    eprintln!(
        "[STRIP] {} {} cocoa={:?} want={:?} alpha={:?}",
        epoch_ms(),
        what,
        have,
        want,
        window.map(|w| w.alphaValue()),
    );
}

fn focus_is_on_screen(screen: Option<&NSScreen>, mtm: MainThreadMarker) -> bool {
    let (Some(screen), Some(main)) = (screen, NSScreen::mainScreen(mtm)) else {
        return true;
    };
    let ours = hostdisplay::display_id_of(screen);
    ours != 0 && ours == hostdisplay::display_id_of(&main)
}

/// The `notch = extend` overlay: native fullscreen as a **carrier** for the Space, with the guest
/// drawn by a borderless window floating on top of it.
///
/// Neither half works alone. A Space cannot draw beside the camera housing — Apple documents the
/// inset as unconditional ("the system automatically positions the window's contents within the
/// safe area") — while a borderless window is not a Space, so using one on its own costs Mission
/// Control, the swipe and the fullscreen animation. Floating a `.fullScreenAuxiliary` window over
/// the carrier's Space gives both, and because the overlay sits above menu-bar level the chrome
/// cannot appear over the guest at all. All three measured in `spikes/notch-fullscreen/` round 5.
///
/// **The strip shows the housing band and nothing else, and the guest's view never moves.**
///
/// The first cut re-parented the guest's whole `NSView` into a full-panel overlay and back. It
/// worked, and every artifact this file has ever had came from it: the re-parent is a one-frame
/// reflow of the entire guest, and it is bound to `isOnActiveSpace`, which only turns true again
/// once a Space-switch animation has *finished* (2026-08-08 — four attempted fixes, each of which
/// moved the artifact rather than removing it; `spikes/notch-fullscreen/` round 6).
///
/// So the guest's view stays in the carrier for good, and this window covers only the band the
/// carrier cannot reach, showing the top `inset` points of the *same* IOSurface through a second
/// `CALayer`. Both layers get identical geometry in panel space and each window clips to its own
/// bounds ([`fit::notch_strip_frames`]), so the seam is exact by construction. A Space switch now
/// re-parents nothing and reflows nothing; whatever the compositor does with an above-menu-bar
/// window while its Space is away can cost at most a 33 pt band instead of the whole screen.
/// Where the strip window sits in screen points, and where its copy of the guest layer sits inside
/// it — the pair [`fit::notch_strip_frames`] returns, remembered so it can be re-applied before the
/// band is revealed again.
type StripPlacement = ((f64, f64, f64, f64), fit::FitRect);

#[derive(Default)]
pub(crate) struct ExtendOverlay {
    window: RefCell<Option<Retained<NSWindow>>>,
    /// The strip's copy of the guest scanout. Created once and re-used across show/hide so the
    /// present path can hand it every frame without caring whether the strip is up.
    strip_layer: RefCell<Option<Retained<CALayer>>>,
    /// The strip's copy of the captured-pointer cursor — see [`Self::strip_cursor_layer`].
    strip_cursor_layer: RefCell<Option<Retained<CALayer>>>,
    /// Read by the capture tap, which has no access to the `Rc` graph. One fact, one owner.
    active: Arc<std::sync::atomic::AtomicBool>,
    /// Last state logged by `LIMINA_OVERLAY_TRACE`, so a 60 Hz tick reports transitions rather
    /// than a flood.
    traced: Cell<Option<OverlaySnapshot>>,
    /// Last *gate* state logged by `LIMINA_OVERLAY_TRACE` — see the trace in [`Self::reconcile`].
    /// Separate from `traced` because it is reported whether or not the overlay exists: the
    /// interesting question is often why it does *not*.
    gate_traced: Cell<Option<(bool, bool, bool, bool)>>,
    /// Whether the *policy* says the guest owns the housing band, ignoring whether the strip
    /// window happens to be on screen this instant. See [`Self::claims_band`].
    claims_band: Cell<bool>,
    /// The last placement [`Self::place`] computed: the strip's screen rect and its layer's frame
    /// within it. Re-applied by [`Self::show`] *before* the window is ordered back in, so a strip
    /// that was hidden across a display change never appears at its old rect first.
    placement: Cell<Option<StripPlacement>>,
    /// Set when [`Self::show`] had no placement it could trust, so the window went up invisible
    /// and the next [`Self::place`] owes it the reveal.
    reveal_pending: Cell<bool>,
    /// When "inactive, and the focus is on our screen" started holding — see [`overlay_level`].
    /// Cleared the moment it stops, so only a sustained condition ever drops the level.
    yielding_since: Cell<Option<std::time::Instant>>,
}

impl ExtendOverlay {
    fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the strip window has ever been built. Distinct from [`Self::is_active`]: the strip
    /// outlives every hide, and its layers stay valid while it is invisible.
    fn has_strip(&self) -> bool {
        self.window.borrow().is_some()
    }

    /// The strip window itself, if it has been built — every window registers it in the
    /// per-slot input registry so band clicks resolve to its slot (a secondary in
    /// `reconcile_extend`, the primary in `PrimaryDisplay::register`).
    fn strip_window(&self) -> Option<Retained<NSWindow>> {
        self.window.borrow().clone()
    }

    /// Tear the strip down for good — a secondary's overlay dies with its window, unlike the
    /// primary's, which lives for the app. The caller deregisters the strip from the input
    /// registry FIRST (this consumes the window).
    fn close(&self) {
        self.active
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(strip) = self.window.borrow_mut().take() {
            strip.close();
        }
    }

    /// Whether the guest owns the housing band **as policy** — `extend`, fullscreen, on a notched
    /// panel, with neither the chrome ask nor a yield standing it down. Deliberately blind to
    /// whether the strip window is on screen right now, because that flips for a reason the guest
    /// must not react to: the strip is hidden while our Space is away (it would otherwise float
    /// over the neighbouring Space — measured 2026-08-08), and sizing the guest to that would
    /// rescale the whole picture by the band's height twice per Space switch. Standing *down* —
    /// the chrome ask, a dialog — does change it, and there the reflow is the intent.
    fn claims_band(&self) -> bool {
        self.claims_band.get()
    }

    fn flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.active.clone()
    }

    /// The strip's copy of the **capture** cursor layer, created on first use as a sublayer of
    /// [`Self::strip_layer`] — exactly where the carrier keeps its own.
    ///
    /// While the pointer is captured (which fullscreen's grab does on its own) the host `NSCursor`
    /// is hidden and the guest's cursor is *composited*, into a sublayer of the carrier's scanout
    /// layer. That layer is a housing-inset taller than the carrier's window, so its top band is
    /// clipped by the window and drawn by the strip instead — and the strip had a copy of the
    /// picture but not of the cursor. The pointer therefore disappeared on entering the band while
    /// still driving the guest, which reads as a lost pointer rather than a clipped one
    /// (reported 2026-08-08). Both layers have the same bounds, so `update_capture_cursor`
    /// computes the same frame for both and each window clips it to its own share.
    fn strip_cursor_layer(&self) -> Retained<CALayer> {
        let mut slot = self.strip_cursor_layer.borrow_mut();
        slot.get_or_insert_with(|| {
            let layer = CALayer::new();
            layer.setHidden(true);
            self.strip_layer().addSublayer(&layer);
            layer
        })
        .clone()
    }

    /// The strip's copy of the scanout layer, created on first use. The present path sets the
    /// same IOSurface on it as on the carrier's layer; [`Self::place`] gives it its geometry.
    fn strip_layer(&self) -> Retained<CALayer> {
        let mut slot = self.strip_layer.borrow_mut();
        slot.get_or_insert_with(|| {
            let layer = CALayer::new();
            // Same reasoning as the carrier's scanout layer: the guest surface is XRGB with a
            // "don't care" alpha, and blending it would composite the strip transparent.
            layer.setOpaque(true);
            layer
        })
        .clone()
    }

    /// Put the strip window over the housing band and its layer where the guest image continues.
    /// Re-asserted every tick: nothing in AppKit keeps a borderless window in place across a
    /// display reconfiguration, and the layer follows the letterbox fit.
    fn place(&self, screen: &NSScreen, carrier_height: f64, inset: f64, layer_fit: fit::FitRect) {
        let f = screen.frame();
        let (strip, shifted) = fit::notch_strip_frames(
            (f.origin.x, f.origin.y, f.size.width, f.size.height),
            carrier_height,
            inset,
            layer_fit,
        );
        // Only trust a reading taken while the band is actually up. Mid-Space-switch
        // `carrier.screen()` misreads, and on the wrong screen the whole chain degenerates —
        // `notch_inset` is 0 for a notchless panel, so the computed strip is a ZERO-HEIGHT rect
        // over the other display. Caching that and re-applying it on the way back in would show
        // as a missing band. Remember only placements computed while on screen; a hidden strip
        // keeps the last good frame it already has.
        if !self.is_active() {
            return;
        }
        self.placement.set(Some((strip, shifted)));
        self.apply_placement(strip, shifted);
        // A reveal that had no trustworthy frame to go up at waits here, where there is one: this
        // rect was computed from the screen the carrier is on *now*. See [`Self::show`].
        if self.reveal_pending.replace(false) {
            if let Some(window) = self.window.borrow().as_ref() {
                window.setAlphaValue(1.0);
            }
        }
    }

    /// Put the strip window and its layer at an already-computed placement.
    ///
    /// The skip when the frame already matches is safe only because [`Self::hide`] deliberately
    /// leaves AppKit's cached frame *wrong* — see the note there. `NSWindow::frame` is our last
    /// write, not the window server's copy of it, so "already correct" is not evidence the window
    /// is where we think it is.
    fn apply_placement(&self, strip: (f64, f64, f64, f64), layer: fit::FitRect) {
        let Some(window) = self.window.borrow().as_ref().cloned() else {
            return;
        };
        let want = NSRect::new(
            NSPoint::new(strip.0, strip.1),
            NSSize::new(strip.2, strip.3),
        );
        if window.frame() != want {
            strip_trace("place", Some(&window), Some(strip));
            window.setFrame_display(want, true);
        }
        set_layer_frame(&self.strip_layer(), layer);
    }

    fn show(&self, carrier: &NSWindow, mtm: MainThreadMarker) {
        if self.is_active() {
            return;
        }
        // Already built: just bring it back. Rebuilding it per Space switch meant it was born
        // from whatever `carrier.screen()` said at that instant and corrected by `place` a frame
        // later, which showed up as the band flashing on the *other* display before jumping home
        // (2026-08-08). The window keeps its frame while ordered out, so there is nothing to
        // correct on the way back.
        if let Some(strip) = self.window.borrow().as_ref().cloned() {
            // Frame first, then reveal — and only a placement still inside this screen is worth
            // applying. If the arrangement changed while the band was down (the display this was
            // all found on: a monitor unplugged mid-Space-switch), the remembered rect names a
            // screen that no longer exists there, and revealing on it is the very bug this window
            // spent a day on. So in that case we go up *invisible* and let the tick's `place`
            // reveal us once it has computed a frame from the screen we are actually on.
            let placed = match (self.placement.get(), carrier.screen()) {
                (Some((rect, layer)), Some(screen)) => {
                    let f = screen.frame();
                    let inside = rect.0 >= f.origin.x - 1.0
                        && rect.0 + rect.2 <= f.origin.x + f.size.width + 1.0
                        && rect.3 > 0.0;
                    if inside {
                        self.apply_placement(rect, layer);
                    }
                    inside
                }
                _ => false,
            };
            strip_trace("show", Some(&strip), self.placement.get().map(|p| p.0));
            strip.setIgnoresMouseEvents(false);
            self.reveal_pending.set(!placed);
            if placed {
                strip.setAlphaValue(1.0);
            }
            self.active
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let Some(screen) = carrier.screen() else {
            return;
        };
        // A plain NSView, layer-hosting like the carrier's, holding the strip's copy of the
        // scanout. The carrier's view is NOT touched — that is the whole point of this design.
        let host = NSView::new(mtm);
        host.setLayer(Some(&self.strip_layer()));
        host.setWantsLayer(true);
        host.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);
        // Born at the band, not at the screen: an opaque black window the size of the panel,
        // even for the one frame before `place` corrects it, is a full-screen flash.
        let f = screen.frame();
        let inset = hostdisplay::fullscreen_inset(&screen);
        let born = NSRect::new(
            NSPoint::new(f.origin.x, f.origin.y + f.size.height - inset),
            NSSize::new(f.size.width, inset),
        );
        let strip: Retained<NSWindow> = unsafe {
            let w: Retained<LiminaWindow> = msg_send![
                LiminaWindow::alloc(mtm),
                initWithContentRect: born,
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ];
            Retained::cast_unchecked(w)
        };
        // Borderless is the only style the compositor lets draw beside the housing;
        // FullScreenAuxiliary is what lets the window join the carrier's Space rather than
        // yanking the user out of it.
        strip.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenAuxiliary);
        // MUST be false. `isReleasedWhenClosed` defaults to TRUE for a programmatically created
        // NSWindow, so `close()` releases it out from under the `Retained` we are holding — an
        // over-release that segfaults in the next autorelease-pool drain, i.e. inside
        // `NSApplication::run`, nowhere near the code that caused it. Cost a crash on the first
        // Cmd-Tab out of the overlay.
        // SAFETY: plain property setter on a window we own and have not yet shown.
        unsafe { strip.setReleasedWhenClosed(false) };
        strip.setLevel(OVERLAY_LEVEL);
        strip.setOpaque(true);
        strip.setBackgroundColor(Some(NSColor::blackColor().as_ref()));
        strip.setContentView(Some(&host));
        // The guest's top bar lives in this band, so its clicks have to land — but the carrier
        // stays the key window: the strip is `orderFront`, never `makeKeyAndOrderFront`, and the
        // input monitor is app-wide. The strip registers in the per-slot input registry under
        // its window's slot, so an event delivered here decodes against the strip's own layer
        // (the shifted guest-image rect) — the same math as the carrier's, one rule.
        strip.setAcceptsMouseMovedEvents(true);
        strip.orderFront(None);
        self.active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *self.window.borrow_mut() = Some(strip);
    }

    /// Take the band off screen **without ordering the window out**, keeping its place in the
    /// window list — see [`Self::show`]. The guest's own view is never involved either way.
    ///
    /// Alpha, not `orderOut`. `orderOut` removes the window from the window list entirely, so it
    /// belongs to no Space, and the next `orderFront` has to re-insert it — and re-insertion is
    /// where the window server decides which Space (and therefore which display) it lands on. Our
    /// own earlier measurement says that decision can go to the *main* screen regardless of the
    /// window's frame (`spikes/notch-fullscreen/` round 1), and mid-Space-switch "main screen" is
    /// exactly the reading that transiently names the wrong display. That is the band flashing on
    /// the external display. An alpha-0 window is still in the list, still bound, still framed —
    /// it simply draws nothing, which is all that was ever wanted here.
    ///
    /// **It does not stay where we left it, though**, and that is the second half of the flash.
    /// The window server parks a hidden `fullScreenAuxiliary` window while its Space is away —
    /// measured at x=728 on the 2560-wide external display, i.e. squarely on the other panel —
    /// without telling AppKit, which goes on reporting the frame we last set. So [`Self::show`]
    /// looked at a frame that "already matched", skipped the write, and revealed the band at the
    /// server's parked position for the frame before the next tick moved it home.
    ///
    /// The write cannot simply be forced from `show`: two `setFrame:display:` calls in one pass
    /// coalesce against the frame the pass *started* with, so nudging away and back leaves the
    /// nudge, not the destination (measured, 2026-08-08). Desynchronise here instead — park
    /// AppKit's cached frame one point off, while it is invisible and nothing can see it. Then
    /// `show`'s ordinary "is the frame right?" test is false, the write happens, and it lands in
    /// the same transaction as the alpha. One point, because the value is irrelevant: all it has
    /// to be is *not* the rectangle we are about to ask for.
    fn hide(&self, _carrier: &NSWindow) {
        let Some(strip) = self.window.borrow().as_ref().cloned() else {
            return;
        };
        self.active
            .store(false, std::sync::atomic::Ordering::Relaxed);
        strip_trace("hide", Some(&strip), None);
        // A reveal still owed is owed no longer.
        self.reveal_pending.set(false);
        strip.setAlphaValue(0.0);
        // An invisible window must not eat the clicks it is sitting on top of.
        strip.setIgnoresMouseEvents(true);
        let f = strip.frame();
        strip.setFrame_display(
            NSRect::new(
                NSPoint::new(f.origin.x, f.origin.y - 1.0),
                NSSize::new(f.size.width, f.size.height),
            ),
            false,
        );
    }

    /// Keep the overlay in step with the carrier, from the tick that already runs — polled rather
    /// than delegate-driven, like every other window-state read here.
    ///
    /// The overlay's *level* is separate from whether it is up, and is decided by
    /// [`overlay_level`] — above the menu bar unless something on our own screen has focus, in
    /// which case it drops so system dialogs can come forward.
    ///
    /// It is up only while all of these hold:
    /// - the policy is `extend`;
    /// - the carrier is natively fullscreen (the overlay needs a Space to float over);
    /// - the screen actually **has** a camera housing — on an external display native fullscreen
    ///   already covers everything, so an overlay would be risk for no pixels;
    /// - **the user is not asking for the chrome.** Nothing can reveal over the overlay, which is
    ///   the point of it, but the menu bar and the window's controls still have to be reachable
    ///   for the VM's own menu actions. A deliberate shove at the top edge (the edge-resistance
    ///   breakthrough, uncaptured only) sets `reveal_chrome` and puts the overlay down until the
    ///   pointer returns to the guest.
    ///
    /// The *guest's geometry* is deliberately **not** conditioned on `isOnActiveSpace`
    /// ([`Self::claims_band`]) even though the strip window itself is. That gate used to decide
    /// both, because the overlay held the guest's view — and it was the direct cause of the
    /// Space-switch reflow, since it only turns true again once the incoming animation has
    /// finished. Now the strip carries nothing but a copy of the housing band, so it can come and
    /// go with the Space while the guest's size never moves.
    fn reconcile(
        &self,
        carrier: &NSWindow,
        notch: crate::vmlib::schema::NotchPolicy,
        reveal_chrome: bool,
    ) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let screen = carrier.screen();
        // Re-assert every tick: activation changes without any of the up/down conditions moving.
        let active = NSApplication::sharedApplication(mtm).isActive();
        // Read once, used by both halves: it tells the *level* whether there is anything on
        // screen to yield to, and it is the signal whose lateness the overlay's lifetime
        // deliberately no longer depends on (see `want` below).
        let on_active_space = carrier.isOnActiveSpace();
        // Tracked whether or not the overlay is up: it is what *brings it back*, so letting it go
        // stale while down would strand the overlay off-screen for good.
        let focus_here = focus_is_on_screen(screen.as_deref(), mtm);
        let (since, yields) = yield_step(
            active,
            focus_here,
            on_active_space,
            self.yielding_since.get(),
            std::time::Instant::now(),
        );
        self.yielding_since.set(since);
        if let Some(overlay) = self.window.borrow().as_ref() {
            // The level is constant while the overlay is up — see [`OVERLAY_LEVEL`]. Re-asserted
            // rather than set once, because a display reconfiguration can reset it.
            if overlay.level() != OVERLAY_LEVEL {
                overlay.setLevel(OVERLAY_LEVEL);
            }
            // `LIMINA_OVERLAY_TRACE=1`: the oracle for "why is the notch strip black". It reports
            // the level we asked for AND the one the window actually carries, plus both display
            // ids, so a wrong *decision* is distinguishable from a wrong *model* (we are at 25 and
            // the system still paints over us). Transitions only — this runs at 60 Hz.
            if overlay_trace() {
                let ours = screen
                    .as_deref()
                    .map(hostdisplay::display_id_of)
                    .unwrap_or(0);
                let focused = NSScreen::mainScreen(mtm)
                    .map(|s| hostdisplay::display_id_of(&s))
                    .unwrap_or(0);
                let now = OverlaySnapshot {
                    active,
                    ours,
                    focused,
                    want: OVERLAY_LEVEL,
                    have: overlay.level(),
                };
                if self.traced.get() != Some(now) {
                    self.traced.set(Some(now));
                    eprintln!(
                        "[OVERLAY] active={active} ours={ours} focused={focused} \
                         want_level={OVERLAY_LEVEL} have_level={} on_active_space={on_active_space}",
                        overlay.level(),
                    );
                }
            }
        }
        let fullscreen = carrier.styleMask().contains(NSWindowStyleMask::FullScreen);
        // Deliberately NOT gated on `on_active_space` — see the doc comment. The overlay is an
        // auxiliary window of the carrier's Space and travels with it; tearing it down when the
        // Space goes away is what makes coming back visibly snap.
        // What the policy says, and what is actually shown. They differ only while our Space is
        // off screen: the strip is `fullScreenAuxiliary`, but it still draws over the Spaces
        // either side of ours once their switch resolves (measured on a real panel), so it has to
        // come down — and the guest's geometry must NOT follow it down. See `claims_band`.
        let claims_band = notch == crate::vmlib::schema::NotchPolicy::Extend
            && !reveal_chrome
            && !yields
            && fullscreen
            && screen
                .as_ref()
                .is_some_and(|s| hostdisplay::notch_inset(s) > 0.0);
        self.claims_band.set(claims_band);
        let want = claims_band && on_active_space;
        // The *gate*, traced whether or not the overlay exists — `traced` above can only report
        // an overlay that is up, which makes it blind to the question "why is it still down?".
        // Timestamped in ms because the answers here are about *when* a signal arrives relative
        // to a ~0.4 s Space-switch animation, not just what it eventually says.
        if overlay_trace() {
            let now = (want, on_active_space, fullscreen, reveal_chrome);
            if self.gate_traced.get() != Some(now) {
                self.gate_traced.set(Some(now));
                eprintln!(
                    "[OVERLAY-GATE] +{:>7.1}ms want={want} on_active_space={on_active_space} \
                     fullscreen={fullscreen} reveal_chrome={reveal_chrome} up={}",
                    trace_clock().elapsed().as_secs_f64() * 1000.0,
                    self.is_active(),
                );
            }
        }
        if want == self.is_active() {
            // Nothing to switch. The strip's frame is re-asserted by `place` from the tick, which
            // knows the letterbox fit; there is nothing to do here.
            return;
        }
        if want {
            self.show(carrier, mtm);
        } else {
            self.hide(carrier);
        }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop; only
    // `windowShouldClose:` is implemented, so every other delegate behavior keeps its
    // AppKit default (the window otherwise stays on its deliberate no-delegate diet).
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "LiminaWindowCloseInterceptor"]
    struct WindowCloseInterceptor;

    unsafe impl NSObjectProtocol for WindowCloseInterceptor {}

    unsafe impl NSWindowDelegate for WindowCloseInterceptor {
        // Intercept the close (M9.4, user-decided over hide-and-reopen): the window must
        // STAY VISIBLE while the close policy runs — a suspend shows the dim + spinner in
        // place for the save's ~10-20s, an Ask dialog's Cancel is a true no-op, and a
        // shutdown closes the window programmatically. Returning NO here is what keeps
        // the red button from hiding the window before any of that can be seen.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            CLOSE_REQUESTED.with(|c| c.set(true));
            false
        }
    }
);

/// The VM window's minimal main menu: Control Center… (Cmd-Shift-C) and Close
/// Window (Cmd-W — routes to `performClose:`, i.e. the normal VM-shutdown path).
/// The actions object also becomes the app delegate so quit Apple events go
/// through the graceful stop instead of exiting the supervisor under the worker.
fn install_main_menu(mtm: MainThreadMarker, app: &NSApplication) {
    let actions: Retained<VmMenuActions> = unsafe { msg_send![VmMenuActions::alloc(mtm), init] };
    app.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*actions)));
    let menubar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    menubar.addItem(&app_item);
    let app_menu = NSMenu::new(mtm);
    let cc = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Control Center…"),
            Some(objc2::sel!(showControlCenter:)),
            &NSString::from_str("C"), // uppercase ⇒ Cmd-Shift-C
        )
    };
    unsafe { cc.setTarget(Some(&*actions)) };
    app_menu.addItem(&cc);
    let close = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Close Window"),
            Some(objc2::sel!(performClose:)),
            &NSString::from_str("w"),
        )
    };
    app_menu.addItem(&close);
    app_item.setSubmenu(Some(&app_menu));
    // The VM verbs menu (M9.4): every lifecycle action reachable from the menu bar
    // (and the same set from the Dock icon via applicationDockMenu:).
    let vm_item = NSMenuItem::new(mtm);
    menubar.addItem(&vm_item);
    vm_item.setSubmenu(Some(&build_vm_menu(mtm, &actions)));
    let displays_item = NSMenuItem::new(mtm);
    menubar.addItem(&displays_item);
    displays_item.setSubmenu(Some(&build_displays_menu(mtm, &actions)));
    let input_item = NSMenuItem::new(mtm);
    menubar.addItem(&input_item);
    input_item.setSubmenu(Some(&build_input_menu(mtm, &actions)));
    app.setMainMenu(Some(&menubar));
    // NSMenuItem targets are weak; the actions object must live as long as the menu.
    std::mem::forget(actions);
}

/// Run the AppKit window on the main thread. The render timer polls `shared`, updates the
/// window contents, and — when the worker exits, the window is closed, or Ctrl-C is hit —
/// kills the worker's process group (`worker_pid`) and exits the process. (We exit from
/// the timer rather than `NSApplication::stop`, which doesn't return without a UI event.)
pub fn run(
    shared: Arc<Mutex<Shared>>,
    mtm: MainThreadMarker,
    conn: Arc<WorkerConn>,
    control: Option<crate::control::ControlPlane>,
    surface_map: SurfaceMap,
    opts: WindowOptions,
) -> ! {
    let WindowOptions {
        resize_socket,
        remap,
        soft_kbd_grab,
        title,
        mode,
        initial_size,
        default_content,
        restore_frame,
        fullscreen_display,
        start_fullscreen,
        state_path,
        desired_size,
        on_window_close,
        splash_save_path,
        restore_splash,
        resume_worker,
        menu_ctx,
        hidpi,
        notch: cfg_notch,
        edge_resistance,
        display_pool,
    } = opts;

    // The VM menu reads this when install_main_menu (below) builds it — set it first.
    MENU_CTX.with(|c| *c.borrow_mut() = menu_ctx);

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    install_main_menu(mtm, &app);

    // First-appearance content size: half the display's area at the guest aspect (a
    // remembered frame overrides this below). In dynamic mode the guest boots at exactly
    // this size, so the first presented frame is 1:1 from tick zero.
    let rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(
            f64::from(default_content.0.max(64)),
            f64::from(default_content.1.max(64)),
        ),
    );
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    // `LiminaWindow`, not a plain NSWindow: the `extend` overlay is borderless and carries the
    // guest, and only a subclass stays key-eligible there. Identical to NSWindow otherwise.
    let window: Retained<NSWindow> = unsafe {
        let w: Retained<LiminaWindow> = msg_send![
            LiminaWindow::alloc(mtm),
            initWithContentRect: rect,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ];
        Retained::cast_unchecked(w)
    };
    window.setTitle(&NSString::from_str(&title));
    // Allow native (Spaces) full screen: the green title-bar button becomes Enter Full Screen
    // and `toggleFullScreen:` (our Cmd-Ctrl-F host shortcut, below) works. Going fullscreen
    // resizes the window, which the existing resize path reflows into the guest resolution.
    //
    // Under `notch = extend` the green button is deliberately left on the NATIVE path even
    // though Cmd-Ctrl-F does panel fullscreen: native fullscreen still works, it just cannot
    // use the housing strip. Two doors to two different fullscreens is worse than one door to
    // the good one, so this is a wart to revisit — noted in docs/design/display-modes.md.
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
    // The shared presentation wiring — layer-hosting view, opaque scanout layer, the capture
    // cursor sublayer, black letterbox background — is `GuestWindow::wire`, the same core a
    // secondary window is built on. The primary carries everything else (input, fit,
    // fullscreen, lifecycle) around this core; those migrate into `GuestWindow` as the
    // primary/secondary split narrows.
    // `Rc`: the frame-apply closure and the render timer both present through the core, and
    // both are `Fn` captures on the main thread.
    let primary_core = std::rc::Rc::new(guestwindow::GuestWindow::wire(window));
    let window = primary_core.window.clone();
    let view = primary_core.view.clone();
    let layer = primary_core.layer.clone();
    // The runtime resize path never asks the guest for less than 64 pt; don't let the
    // window shrink below what the guest can be driven to.
    window.setContentMinSize(NSSize::new(64.0, 64.0));
    // Restore the remembered placement: the panel the VM was fullscreen on wins over the
    // remembered frame (see `restore_placement` — this MUST agree with the screen
    // `screen_info_for_restore` already sized the guest for), a frame from a since-unplugged
    // display is dropped rather than opened off-screen, and nothing usable means center.
    let restored = restore_placement(restore_frame, fullscreen_display, &screen_slots(mtm))
        .map(|f| NSRect::new(NSPoint::new(f[0], f[1]), NSSize::new(f[2], f[3])));
    match restored {
        Some(r) => window.setFrame_display(r, false),
        None => window.center(),
    }
    // Lock interactive resize to the guest's aspect so the window can't be dragged into a
    // shape that only grows the letterbox. Host mode uses the display's aspect (`initial_size`
    // is the screen the window boots on; re-applied below on display migration); fixed mode
    // the configured WxH. Dynamic stays free — the guest follows the window there.
    match mode {
        DisplayResolution::Host => apply_aspect_lock(&window, initial_size),
        DisplayResolution::Fixed(w, h) => apply_aspect_lock(&window, (w, h)),
        DisplayResolution::Dynamic => {}
    }
    // Required for hover (non-dragging) motion to be delivered as MouseMoved events.
    window.setAcceptsMouseMovedEvents(true);
    window.makeKeyAndOrderFront(None);
    app.activate();

    // Per-timer state (main thread only).
    // Restore the fullscreen the VM stopped in, on the first tick rather than here:
    // `toggleFullScreen:` before the window has finished appearing is silently dropped, and the
    // guest is already sized for this screen's fullscreen content (`initial_display_size`), so
    // the transition costs no re-modeset.
    let pending_fullscreen = Cell::new(start_fullscreen);
    // The primary's per-tick presentation state (gen gate, present/capture diagnostics)
    // lives in its `PrimaryDisplay` role, constructed with the window collection below.
    // `geom` — the guest's current resolution on the primary's slot — is shared with the
    // control plane here, whose pushes gate on "the guest has presented" (`!= (0, 0)`).
    let geom: std::rc::Rc<Cell<(u32, u32)>> = std::rc::Rc::new(Cell::new((0u32, 0u32)));
    // Dynamic mode's runtime window-resize → guest. The 60 Hz timer debounces the window's
    // content size and, once a drag settles, pushes the new size to the worker over
    // `resize_socket` (which forwards it to the live virtio-gpu → the guest re-modesets).
    // `geom` (the guest's current resolution) is the feedback guard: a window that already
    // matches it — including the guest-driven setContentSize echo — sends nothing. See
    // docs/design/runtime-display-resize.md.
    let resize_sent: Cell<(u32, u32)> = Cell::new((0, 0));
    // Host mode's screen tracker: the screen size the guest was last driven to, plus the
    // identity key of the display it came from. Seeded with the boot size (derived from the
    // same screen) and a zero key, so the first poll pushes the full identity once and every
    // later poll is a no-op until something actually changes. Tracking the identity — not just
    // the size — is what catches a move between two same-sized displays, which the size-only
    // check missed entirely.
    let screen_sent: Cell<((u32, u32), u64)> = Cell::new((initial_size, 0));
    // The display identity the guest was last told about, tracked for ALL modes (host folds
    // its push into the size push; dynamic and fixed send identity on its own). Seeded to 0,
    // which no real display hashes to, so the first poll after the guest presents a frame
    // hands over the identity of whichever display it booted on.
    let identity_sent: Cell<u64> = Cell::new(0);
    // What that display was last doing (refresh, and the VRR range derived from it). Separate
    // from the identity so the same panel changing rate is an in-place adjustment rather than a
    // connector cycle. Seeded to 0, which `refresh_of` never yields — it floors to 60 — so the
    // first poll always pushes.
    let mode_sent: Cell<u64> = Cell::new(0);
    // The guest-desktop position each slot was last told to suggest (the arrangement relay).
    // Cleared when the guest reboots to firmware, so the re-entered OS phase is told again.
    let positions_sent: RefCell<std::collections::HashMap<u32, (u32, u32)>> =
        RefCell::new(std::collections::HashMap::new());
    // A fresh worker was swapped in and the arrangement it inherited is a fiction, but nothing
    // can be said to it until it is listening: the pushes travel over the display-control
    // socket, which the restored worker only creates once its snapshot is loaded, and a batch
    // sent into a socket that is not there yet is dropped with the table believing it was sent.
    // So the flag is taken when it is raised and acted on when the new worker presents.
    let reassert_pending: Cell<bool> = Cell::new(false);
    // Window-state persistence (state.toml): the settle-debounced candidate + what's on disk.
    let pending_state: Cell<Option<WindowState>> = Cell::new(None);
    let stable_ticks: Cell<u32> = Cell::new(0);
    let saved_state: Cell<Option<WindowState>> = Cell::new(None);
    // Guest-cursor per-timer state: the last applied cursor gen and the (IOSurface id,
    // content scale) of the shape the host pointer currently wears (so we rebuild only on
    // an actual shape or window-scale change).
    // `(slot, gen)`, not just the gen: each scanout counts its own cursor changes, so the
    // pointer crossing to a display whose counter happens to match would skip the re-apply and
    // leave the previous display's shape on.
    let last_cursor_gen = Cell::new((usize::MAX, 0u64));
    let built_cursor: Cell<Option<(u32, u32)>> = Cell::new(None);
    // Since when the captured-cursor fault has stood, and whether this episode has been
    // reported — so it is said once, and only after it has proved it is not a transient
    // (`cursor::undrawn_fault`).
    let cursor_fault_since: Cell<Option<std::time::Instant>> = Cell::new(None);
    let cursor_fault_said = Cell::new(false);
    // The host pointer's guest-shape adoption, shared with the input monitor (which
    // tracks the pointer crossing the view boundary and asserts/clears the shape).
    let host_cursor = input::HostCursor::new();
    // Pointer-capture flag, shared between the input monitor (which toggles it on Cmd-Ctrl-G),
    // the render timer (which composites the guest cursor while it's set), and the capture tap.
    let captured = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The `notch = extend` overlay (see [`ExtendOverlay`]), reconciled from the render tick. The
    // capture tap reads its flag: a guest hosted in the overlay is fullscreen as far as edge
    // resistance is concerned, even though the overlay carries no fullscreen style bit.
    // The primary's strip overlay is its window core's, like every window's.
    let overlay = primary_core.overlay.clone();
    let overlay_flag = overlay.flag();
    // Set by the capture tap when the user deliberately shoves at the top edge: the gesture that
    // asks for the menu bar and the window's own controls back. See `ExtendOverlay::reconcile`.
    let reveal_chrome = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Reliable capture container: a session-level CGEventTap that *consumes* mouse events while
    // captured (so clicks/motion can't escape to host windows) and integrates them into the
    // virtual cursor driving the absolute tablet. Needs Accessibility permission; if absent,
    // capture falls back to the local monitor's warp path (see input.rs), the toggle path
    // retries the install (a mid-run grant heals without a restart), and the first tap-less
    // capture raises the system prompt. Installed on the main thread, before `app.run()`.
    // The input translator, created before the tap so both share it: the render timer flushes
    // held keys on focus loss, the local event monitor drives it per event, and the tap
    // delegates capture toggles + the ungrab chord to it (one source of truth for the
    // capture-boundary modifier bookkeeping).
    // Which slot the primary window shows: written by the render path and read by the input
    // path, which is why it is declared here rather than beside the arrangement code that
    // maintains it.
    let primary_slot: std::rc::Rc<Cell<u32>> = std::rc::Rc::new(Cell::new(0));
    let pointer_slot: std::rc::Rc<Cell<(usize, f64)>> = std::rc::Rc::new(Cell::new((0, 0.0)));
    // The Input menu's switch starts wherever the configuration put it (`[input]
    // normalize_modifiers` / `--no-normalize-modifiers`). A remembered menu choice overrides it
    // further down, once the per-VM state is loaded.
    MODIFIER_NORMALIZE.with(|f| f.set(remap.normalize));
    let input_state = std::rc::Rc::new(input::InputState::new(
        conn.clone(),
        host_cursor.clone(),
        remap,
        captured.clone(),
        primary_core.clone(),
        overlay_flag.clone(),
        reveal_chrome.clone(),
        primary_slot.clone(),
        pointer_slot.clone(),
    ));
    let _capture_tap = capture_tap::install(
        conn.clone(),
        captured.clone(),
        input_state.clone(),
        soft_kbd_grab,
        view.clone(),
        edge_resistance,
        overlay_flag,
    );

    // Shown-ack channel (#8 leg 2): after Core Animation latches a frame, tell the worker
    // "shown <id>" so it can complete the guest's held flush fence. The blocking send is done on
    // a DEDICATED thread, never the AppKit main thread: the ack fd shares a blocking open-file
    // description with the reader (so it can't be made non-blocking), and MSG_DONTWAIT is NOT
    // honored for AF_UNIX stream sockets on macOS — so a worker that briefly stops draining acks
    // (notably the early-boot window just after a reboot relaunch) would otherwise block the main
    // thread and beachball the whole UI. The completion block only `try_send`s the id (bounded,
    // best-effort — drop if full); this thread sends it to whichever worker is current (conn is
    // swapped on relaunch). A dropped ack is covered by the worker's fallback deadline.
    let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<present::AckMsg>(64);
    {
        let conn = conn.clone();
        // #24 off-glass gating is unconditional (the LIMINA_ACK_ONGLASS kill switch was
        // retired 2026-08-14 — it had been default-on since the fix landed). What remains
        // is the LIVE within-session A/B: `touch /tmp/limina-ack-latch` reverts to
        // latch-only acks (the pre-fix behavior, which tears), `rm` re-arms the gate. The
        // marker is re-stat'ed at most every 500 ms — never per ack (no sync I/O on the
        // frame-pacing path; same treatment as the present-copy markers and libkrun 0113).
        // §29/§30 ack SPLIT: one supervisor message conflated two host events — "the new
        // frame is presented" and "the replaced buffer is off glass". They differ by
        // WindowServer's over-hold tail (useprobe2: clear-of-prev p50 16.2 ms after commit
        // = the swap vblank, but p90 +7 ms / max +24 ms PAST the swap), and the guest's
        // flip fence — its present timestamp AND its buffer-release gate — rode the late
        // edge, which is the §29 "queued early, presented a cycle late" miss class.
        // Split: "shown <id>" (completes the guest flush fence) is sent at the off-glass
        // gate CAPPED at ~one refresh + slack past the latch — identical to the proven
        // tear-safe timing whenever the clear is punctual, and in tail cases the cap fires
        // post-swap (the replaced surface is off glass, merely over-held). "free <id>" is
        // sent at the actually-observed clear (informational to the worker today; the
        // release-truth signal on the wire). LIMINA_PRESENT_FENCE=free restores the
        // old single-edge behavior (fence at full clear, 50 ms cap).
        let fence_capped = std::env::var("LIMINA_PRESENT_FENCE").map_or(true, |v| v != "free");
        let cap_ms = std::env::var("LIMINA_PRESENT_FENCE_CAP_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(20);
        std::thread::spawn(move || {
            let mut marker_at = std::time::Instant::now();
            let mut latch_marker = std::fs::metadata("/tmp/limina-ack-latch").is_ok();
            let send_line = |line: String| {
                // Snapshot the current worker's endpoints and hold the Arc across the send:
                // the ack fd can't be closed (nor its number reused) mid-send even if a
                // relaunch retires this worker concurrently. Blocking is fine here — a
                // wedged/booting worker only stalls this thread.
                let io = conn.io();
                unsafe {
                    libc::send(
                        io.ack_fd(),
                        line.as_ptr() as *const libc::c_void,
                        line.len(),
                        0,
                    );
                }
            };
            while let Ok(msg) = ack_rx.recv() {
                let (id, prev) = match msg {
                    present::AckMsg::Shown(id, prev) => (id, prev),
                    // Out-of-band and unpaced: this is only the *request*. The worker answers by
                    // publishing on the surface port, which is what keeps the ordering that makes
                    // recycled IOSurface ids safe.
                    present::AckMsg::Resurface(id) => {
                        log::warn!(
                            "window: asking the worker to re-publish surface {id} — we dropped it \
                             and the guest is still presenting it"
                        );
                        send_line(format!("resurface {id}\n"));
                        continue;
                    }
                };
                // #24: the completion block (latch) fires ~one refresh BEFORE WindowServer
                // stops sampling the surface this frame replaced (measured p50 17 ms / max
                // 33 ms, spikes/present-pacing/). Acking at latch hands the guest its
                // flip-completion while the old buffer is still being read — it repaints it
                // and the mid-repaint state reaches glass (the zero-copy tear). Hold the ack
                // until the replaced surface leaves window-server use; the caps bound a
                // stuck count (occlusion oddities), and the worker's 150 ms fallback stands
                // behind that.
                if marker_at.elapsed() >= std::time::Duration::from_millis(500) {
                    marker_at = std::time::Instant::now();
                    latch_marker = std::fs::metadata("/tmp/limina-ack-latch").is_ok();
                }
                let gate = !latch_marker;
                let mut freed = true;
                if gate {
                    if let Some(prev) = &prev {
                        let t0 = std::time::Instant::now();
                        let gated = prev.is_in_use();
                        let shown_cap = std::time::Duration::from_millis(if fence_capped {
                            cap_ms
                        } else {
                            50
                        });
                        while prev.is_in_use() && t0.elapsed() < shown_cap {
                            std::thread::sleep(std::time::Duration::from_micros(500));
                        }
                        freed = !prev.is_in_use();
                        if gated {
                            // Engagement oracle (the 0114 lesson): the FIRST gated ack logs
                            // at INFO — one line per run confirms the mode without any
                            // per-frame firehose; the periodic wait sample stays at trace.
                            static GATED: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            let n = GATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if n == 0 {
                                log::info!(
                                    "window: off-glass ack gating ENGAGED (first gated ack waited {:?}, fence cap {:?})",
                                    t0.elapsed(),
                                    shown_cap
                                );
                            } else if n.is_multiple_of(512) {
                                log::trace!(
                                    "window: off-glass ack gate n={n}, last wait {:?}",
                                    t0.elapsed()
                                );
                            }
                        }
                    }
                }
                send_line(format!("shown {id}\n"));
                // Release truth: keep polling the replaced surface past the fence cap (up
                // to the old 50 ms total) and report the observed clear as "free <id>".
                // The worker ignores unknown verbs today; this is the wire's release
                // signal for any future reuse-safety bookkeeping, and it keeps the
                // over-hold tail observable (trace) without moving the fence.
                if !freed {
                    if let Some(prev) = &prev {
                        let t0 = std::time::Instant::now();
                        while prev.is_in_use()
                            && t0.elapsed() < std::time::Duration::from_millis(50)
                        {
                            std::thread::sleep(std::time::Duration::from_micros(500));
                        }
                        freed = !prev.is_in_use();
                        if freed {
                            log::trace!(
                                "window: over-hold tail on frame {id}: replaced surface cleared {:?} after the fence cap",
                                t0.elapsed()
                            );
                        }
                    }
                }
                send_line(format!("free {id}\n"));
            }
        });
    }

    // The frame-apply path, shared by the 60 Hz timer (fallback/liveness) and the
    // dispatch wake-up from the reader thread (event-driven, leg 1 of the latency
    // collapse): applying the moment a frame arrives instead of at the next tick.
    // EVERY guest window, walked uniformly per tick: the primary (this window, wearing its
    // role state) plus one per other connected slot — those stay empty until the guest's
    // second connector is configured, so a single-display VM allocates nothing extra.
    let guest_windows: std::rc::Rc<RefCell<windows::GuestWindows>> = std::rc::Rc::new(
        RefCell::new(windows::GuestWindows::new(windows::PrimaryDisplay::new(
            primary_core.clone(),
            primary_slot.clone(),
            geom.clone(),
            desired_size.clone(),
            hidpi,
            reveal_chrome.clone(),
        ))),
    );
    // Which panel owns which guest connector. Slots are handed out here and nowhere else, so a
    // panel keeps the same connector for as long as the VM runs — see `displays`.
    // Which pool slot the PRIMARY window presents. A panel owns a slot for the life of the VM
    // and slot 0 belongs to whichever panel claimed it first, so the primary window is not
    // pinned to 0: open on the studio display, or drag the window there, and the desktop lives
    // on slot 1 while slot 0 is dark or shows another panel's picture. Everything that reads
    // "the window's guest display" — the present path, the cursor, the liveness probe — goes
    // through this, and `secondary` presents every OTHER connected slot, slot 0 included.
    let display_table: std::rc::Rc<RefCell<displays::DisplayTable>> = {
        let mut table = displays::DisplayTable::new(display_pool);
        // Give every panel the connector it had last run, so the guest sees the same monitors it
        // saw then — connector included, which is half of how a compositor identifies one.
        if let Some(saved) = state_path.as_deref().and_then(crate::vmlib::state::load) {
            let slots: Vec<(u64, u32)> = saved
                .display_slots
                .into_iter()
                .map(|(k, i)| (k as u64, i))
                .collect();
            table.restore_assignment(&slots);
            let off: Vec<u64> = saved
                .display_disabled
                .into_iter()
                .map(|k| k as u64)
                .collect();
            table.restore_disabled(&off);
            // Before the timer's first tick: a start_fullscreen restore reaches the
            // presentation decision on tick one, and it must see the remembered switch.
            FULLSCREEN_ALL_DISPLAYS.with(|f| f.set(saved.fullscreen_all_displays));
            // Absent means the menu has never been touched for this VM, so the configured
            // value stands; present means the user said otherwise and outranks it.
            if let Some(on) = saved.modifier_normalize {
                MODIFIER_NORMALIZE.with(|f| f.set(on));
            }
        }
        std::rc::Rc::new(RefCell::new(table))
    };
    let apply: std::rc::Rc<dyn Fn()> = std::rc::Rc::new({
        let shared = shared.clone();
        let guest_windows = guest_windows.clone();
        let display_table = display_table.clone();
        let primary_slot = primary_slot.clone();
        let slots_state_path = state_path.clone();
        let slots_saved: RefCell<Vec<(u64, u32)>> = RefCell::new(Vec::new());
        let disabled_saved: RefCell<Vec<u64>> = RefCell::new(Vec::new());
        // Seeded with the restored value so the save below fires on change, not on startup.
        let fsall_saved: Cell<bool> = Cell::new(FULLSCREEN_ALL_DISPLAYS.with(|f| f.get()));
        // Same seeding rule: start from whatever the restore left in place, so the first save is a
        // real change and not the startup value written back over the file.
        let normalize_saved: Cell<bool> = Cell::new(MODIFIER_NORMALIZE.with(|f| f.get()));
        let panel_names: RefCell<Vec<(u64, String)>> = RefCell::new(Vec::new());
        let window = window.clone();
        let ack_tx = ack_tx.clone();
        let surface_map = surface_map.clone();
        let desired_size = desired_size.clone();
        let apply_input = input_state.clone();
        move || {
            // Each window's own walk — strip reconcile, refit, gen gate, modeset follow,
            // present — runs in `guest_windows.apply` at the end of this tick, after the control
            // plane below has assigned slots and pushed identities.
            if !window.styleMask().contains(NSWindowStyleMask::FullScreen) {
                // Out of fullscreen the chrome is there for the taking on every panel (the
                // covers drop with the primary's Space), so the ask is moot — and clearing it
                // here means entering fullscreen always starts from the overlay, whatever
                // happened in the last session of it. Through the InputState so the
                // `reveal_chrome` mirror and the per-slot ask can never disagree.
                apply_input.reveal_moot();
            }
            // Resolution pushes to the guest, by display mode.
            if let Some(sock) = &resize_socket {
                // Which physical display the window sits on, and therefore the identity,
                // density and refresh the guest should see. This is deliberately computed for
                // EVERY mode: those are properties of the *host display*, and only the policy
                // that decides the guest's *resolution* differs between modes. Host mode folds
                // the identity into its size push so a migration costs one modeset; dynamic and
                // fixed have no size push to fold it into, so they send it on its own below.
                // `screen()` is None mid-transition, which simply skips the tick.
                let host = window.screen().map(|s| {
                    let inset = notch_inset_for(&s, cfg_notch);
                    hostdisplay::describe(&s, scale_for(&window, hidpi), inset)
                });
                // Two distinct events, deliberately not merged. A *migration* is a different
                // physical panel and earns the connector cycle; an *adjustment* is the same panel
                // changing what it is doing (its refresh rate, and with it the VRR range) and
                // must travel in place — cycling there would black the guest out for the settle
                // every time a ProMotion display changed rate on a monitor the user never
                // touched. Both still have to reach the guest: the refresh and range live in the
                // EDID, so a swallowed adjustment is a guest that can never drive variable
                // refresh.
                // The pool arrangement. Which slot the window's panel owns, and — once the
                // guest's own driver is up — the connects and disconnects that make the guest
                // agree. Before that the guest is still firmware, which paints head 0 and no
                // other, so the table plans nothing and every push below goes to slot 0 exactly
                // as it always did.
                let panel = window.screen().map(|s| hostdisplay::panel_key(&s));
                let (slot, handover) = {
                    let panels: Vec<(u64, String)> = NSScreen::screens(mtm)
                        .iter()
                        .map(|s| (hostdisplay::panel_key(&s), s.localizedName().to_string()))
                        .collect();
                    let attached: Vec<u64> = panels.iter().map(|(k, _)| *k).collect();
                    *panel_names.borrow_mut() = panels;
                    let mut table = display_table.borrow_mut();
                    // The Displays menu's clicks, drained here because a switch is a connector
                    // cycle and every one of those is planned in the same place, one tick later.
                    for (panel, on) in
                        DISPLAY_TOGGLES.with(|t| t.borrow_mut().drain(..).collect::<Vec<_>>())
                    {
                        table.set_enabled(panel, on);
                    }
                    // A fresh worker was swapped in (a resume, or a reboot relaunch): the
                    // device is back to how it boots — slot 0 up, nothing else — and was never
                    // told any of the identities, modes or positions the host remembers as
                    // sent. Forget all of it, so the plan below says the whole arrangement
                    // again. The reboot case also reaches this through the phase branches; a
                    // resume keeps its phase and would otherwise never re-assert, which is the
                    // 2026-08-22 stuck resume (docs/hardening-backlog.md §M9 snapshot
                    // hardening): the table went on believing a slot the restored guest was no
                    // longer driving, so the window watched a dead slot for good.
                    //
                    // Held until the new worker PRESENTS, not done when the swap is announced:
                    // its display-control socket does not exist until its snapshot is loaded,
                    // and a batch sent before that is dropped while the table records it as
                    // said — the same staleness one restore later.
                    if present::take_device_fresh(&shared) {
                        reassert_pending.set(true);
                    }
                    let (os, presented) = {
                        let s = shared.lock().unwrap();
                        (
                            s.guest_driver_ready,
                            s.slots.iter().any(|slot| slot.show_id.is_some()),
                        )
                    };
                    if reassert_pending.get() && presented {
                        reassert_pending.set(false);
                        log::info!(
                            "display: a fresh worker has the device; re-asserting the arrangement"
                        );
                        table.reset_connectors_to_boot();
                        screen_sent.set((initial_size, 0));
                        identity_sent.set(0);
                        mode_sent.set(0);
                        positions_sent.borrow_mut().clear();
                    }
                    if os && table.enter_os_phase() {
                        log::info!(
                            "display: the guest's own driver has the GPU; the scanout pool is \
                             ours to arrange"
                        );
                        // Say the identity again, now that there is a driver to hear it.
                        //
                        // Everything pushed during the firmware phase was said to a device the
                        // guest had not attached yet, and a virtio driver RESETS its device
                        // while probing — so an EDID set before that may simply not survive it.
                        // When it does not, the connector keeps virtio-gpu's own default, which
                        // claims a 10" panel: mutter reads it as "Red Hat, Inc. 10\"" and, at
                        // 2560x1440 on a 10" screen, picks a 250% scale. Which side of the
                        // reset we land on is timing, which is why it only bit some boots.
                        //
                        // Forgetting what we think the guest was told is enough — the next
                        // tick sees the identity as changed and re-announces it as a migration
                        // cycle, this time with the full host display info (range, alt mode)
                        // that was not known that early either. Only slot 0 is connected here
                        // (firmware paints head 0 and no other); every later slot carries its
                        // identity on the connect that brings it up.
                        //
                        // All THREE, and `screen_sent` is the load-bearing one: in host mode —
                        // the default — it alone gates the push that carries the identity, so
                        // clearing only the other two re-announces nothing at all.
                        screen_sent.set((initial_size, 0));
                        identity_sent.set(0);
                        mode_sent.set(0);
                        positions_sent.borrow_mut().clear();
                    } else if !os && table.phase() == displays::Phase::Os {
                        // The worker was relaunched for a guest reboot: the guest is back in
                        // firmware, which paints head 0 and no other.
                        log::info!("display: the guest rebooted; back to the firmware arrangement");
                        table.reset_to_firmware();
                        positions_sent.borrow_mut().clear();
                    }
                    // Fullscreen is what lights up the other panels — when the user opted in
                    // via the Displays menu; a window occupies exactly the display it is on.
                    let presentation = displays::presentation_for(
                        panel,
                        window.styleMask().contains(NSWindowStyleMask::FullScreen),
                        FULLSCREEN_ALL_DISPLAYS.with(|f| f.get()),
                    );
                    // Gated on the guest having presented, like every other push here: a plan
                    // acted on before the first frame would cycle connectors under a guest that
                    // has not finished probing them.
                    match panel.filter(|_| geom.get() != (0, 0)) {
                        Some(panel) => {
                            let pushes = table.plan(presentation, &attached);
                            (table.present_slot(panel, &attached), pushes)
                        }
                        None => (0, Vec::new()),
                    }
                };
                primary_slot.set(slot);
                publish_display_menu(&display_table.borrow(), &panel_names.borrow(), panel);
                {
                    // Persist on change only: this runs at frame rate, and the assignment moves
                    // about as often as a monitor is plugged in.
                    let now = display_table.borrow().assignment();
                    if *slots_saved.borrow() != now {
                        save_display_slots(slots_state_path.as_deref(), now.clone());
                        *slots_saved.borrow_mut() = now;
                    }
                    let off = display_table.borrow().disabled_panels();
                    if *disabled_saved.borrow() != off {
                        save_display_disabled(slots_state_path.as_deref(), off.clone());
                        *disabled_saved.borrow_mut() = off;
                    }
                    let fsall = FULLSCREEN_ALL_DISPLAYS.with(|f| f.get());
                    if fsall_saved.get() != fsall {
                        save_fullscreen_all(slots_state_path.as_deref(), fsall);
                        fsall_saved.set(fsall);
                    }
                    let normalize = MODIFIER_NORMALIZE.with(|f| f.get());
                    if normalize_saved.get() != normalize {
                        save_modifier_normalize(slots_state_path.as_deref(), normalize);
                        normalize_saved.set(normalize);
                    }
                }

                // The arrangement relay (M15 part 4): the guest-desktop position every
                // connected connector should suggest, rebuilt from the host's own panel
                // arrangement (`arrangement::guest_positions` — all slots or none). A position
                // rides the connect that brings its slot up, so the hotplug the guest probes is
                // already placed; a change on an already-connected slot (the user rearranged
                // displays in System Settings) travels as its own in-place push below.
                let desired_positions: std::collections::HashMap<u32, (u32, u32)> = {
                    let table = display_table.borrow();
                    if table.phase() == displays::Phase::Os {
                        let screens = NSScreen::screens(mtm);
                        let top = screens
                            .iter()
                            .map(|s| {
                                let f = s.frame();
                                f.origin.y + f.size.height
                            })
                            .fold(f64::MIN, f64::max);
                        let mut slots: Vec<(u32, arrangement::Placement)> = table
                            .connected_slots()
                            .into_iter()
                            .filter_map(|slot| {
                                let panel = table.panel_of(slot)?;
                                let s = screens
                                    .iter()
                                    .find(|s| hostdisplay::panel_key(s) == panel)?;
                                let f = s.frame();
                                let inset = notch_inset_for(&s, cfg_notch);
                                let (uw, uh) =
                                    fit::usable_content(f.size.width, f.size.height, inset);
                                Some((
                                    slot,
                                    arrangement::Placement {
                                        panel,
                                        // AppKit's global space is y-up; the guest's desktop
                                        // is y-down. Flip against the arrangement's top edge.
                                        frame: arrangement::PointRect {
                                            x: f.origin.x,
                                            y: top - (f.origin.y + f.size.height),
                                            w: f.size.width,
                                            h: f.size.height,
                                        },
                                        logical: (uw.round() as u32, uh.round() as u32),
                                    },
                                ))
                            })
                            .collect();
                        // The metric correction: where the guest's compositor reported its
                        // own logical rects, those sizes replace the prediction — they are
                        // exactly what mutter validates the suggested set against, and the
                        // only way to be right about a fractional scale.
                        arrangement::correct_metric(
                            &mut slots,
                            &arrangement::reported_logical_sizes(),
                        );
                        let placements: Vec<_> = slots.iter().map(|(_, p)| *p).collect();
                        match arrangement::guest_positions(&placements) {
                            Some(positions) => slots
                                .iter()
                                .zip(positions)
                                .map(|((slot, _), (_, xy))| (*slot, xy))
                                .collect(),
                            None => Default::default(),
                        }
                    } else {
                        Default::default()
                    }
                };

                // A handover carries the arriving panel's whole identity on its connect, so it
                // *is* the migration for the slot it brings up — the in-place path below must
                // not also fire for the same event.
                let handed_over = handover.iter().any(|p| {
                    p.slot == slot && matches!(p.action, displays::SlotAction::Connect(_))
                });
                {
                    // In-place position pushes go out BEFORE any connect: a new panel moves
                    // where the already-up connectors belong, and the guest compositor
                    // evaluates the suggested set the moment the connect's hotplug lands —
                    // a position that arrives after it is stale at the only instant it is
                    // read, the set looks overlapping, and the guest falls back to its
                    // linear default for good (rig, 2026-08-19). A slot whose own connect is
                    // in this handover is skipped: its position rides the connect itself.
                    // The device applies queued updates in order, so sending first is
                    // arriving first.
                    let mut sent = positions_sent.borrow_mut();
                    let mut moves = Vec::new();
                    for (&mslot, &xy) in &desired_positions {
                        if handover.iter().any(|p| {
                            p.slot == mslot && matches!(p.action, displays::SlotAction::Connect(_))
                        }) {
                            continue;
                        }
                        // A scanout starts at the device-default (0, 0), so pushing (0, 0)
                        // to a slot never told otherwise changes nothing in the guest — but
                        // it would still cost a config-change cycle at desktop arrival in
                        // every single-display session, perturbing whatever is mid-flight.
                        let told = sent.get(&mslot).copied();
                        if told != Some(xy) && (xy != (0, 0) || told.is_some()) {
                            sent.insert(mslot, xy);
                            moves.push(limina_displayctl::DisplayCommand::Display(
                                limina_displayctl::DisplayControl {
                                    display_id: mslot,
                                    position: Some(xy),
                                    ..Default::default()
                                },
                            ));
                        }
                    }
                    if !moves.is_empty() {
                        send_display_commands(sock, moves);
                    }
                }
                if !handover.is_empty() {
                    let commands: Vec<_> = handover
                        .iter()
                        .filter_map(|push| match push.action {
                            displays::SlotAction::Disconnect => {
                                Some(displays::disconnect_command(push.slot))
                            }
                            displays::SlotAction::Connect(_) if push.slot == slot => {
                                host.as_ref().map(|host| {
                                    hostdisplay::connect_command(
                                        host,
                                        hostdisplay::drives_size(mode, true),
                                        push.slot,
                                        desired_positions.get(&push.slot).copied(),
                                    )
                                })
                            }
                            // A panel that is not the window's, and so always driven to its
                            // own size — see `hostdisplay::drives_size` for why the display
                            // mode does not reach these. If AppKit no longer has that screen —
                            // unplugged between the two reads — drop the push rather than
                            // substituting some other panel's identity, which would have the
                            // guest save a configuration for a monitor that does not exist,
                            // under a connector duplicating the primary's. The panel is gone
                            // from `attached` next tick, so the plan re-diffs it away.
                            displays::SlotAction::Connect(other) => {
                                hostdisplay::describe_panel(other, cfg_notch, mtm).map(|panel| {
                                    hostdisplay::connect_command(
                                        &panel,
                                        hostdisplay::drives_size(mode, false),
                                        push.slot,
                                        desired_positions.get(&push.slot).copied(),
                                    )
                                })
                            }
                        })
                        .collect();
                    send_display_commands(sock, commands);
                }
                {
                    // Positions that rode a connect are recorded as sent; a slot that went
                    // down forgets its position so a later reconnect is told again.
                    let mut sent = positions_sent.borrow_mut();
                    for push in &handover {
                        match push.action {
                            displays::SlotAction::Connect(_) => {
                                match desired_positions.get(&push.slot) {
                                    Some(&xy) => {
                                        sent.insert(push.slot, xy);
                                    }
                                    None => {
                                        sent.remove(&push.slot);
                                    }
                                }
                            }
                            displays::SlotAction::Disconnect => {
                                sent.remove(&push.slot);
                            }
                        }
                    }
                }

                // Two distinct events, deliberately not merged. A *migration* is a different
                // physical panel and earns the connector cycle; an *adjustment* is the same panel
                // changing what it is doing (its refresh rate, and with it the VRR range) and
                // must travel in place — cycling there would black the guest out for the settle
                // every time a ProMotion display changed rate on a monitor the user never
                // touched.
                let migrated = !handed_over
                    && host
                        .as_ref()
                        .is_some_and(|h| h.identity_key() != identity_sent.get());
                let adjusted = host
                    .as_ref()
                    .is_some_and(|h| !migrated && !handed_over && h.mode_key() != mode_sent.get());
                if handed_over {
                    if let Some(host) = host.as_ref() {
                        identity_sent.set(host.identity_key());
                        mode_sent.set(host.mode_key());
                        // `screen_sent` deliberately NOT pre-set: in host mode the block it
                        // gates also re-locks the window's aspect, reshapes it to the arriving
                        // panel and refreshes the relaunch size, and those are owed whatever
                        // carried the identity. Only the *push* is redundant, and it is skipped
                        // at the send below.
                    }
                }

                match mode {
                    // Dynamic: push the window's content size ONCE the resize gesture ENDS —
                    // never during the drag. `inLiveResize()` is true for the whole drag;
                    // firing while it's true would re-modeset the guest dozens of times
                    // mid-gesture (surface churn + cache clears → the window blanks). Only
                    // active once the guest has presented a frame (so `geom` is a real
                    // baseline, not 0×0), and skipped when the window already matches the
                    // guest (the feedback guard against the guest-driven setContentSize echo).
                    DisplayResolution::Dynamic => {
                        let base = geom.get();
                        let view = window.contentView();
                        let in_live = view.as_ref().map(|v| v.inLiveResize()).unwrap_or(false);
                        let size = view
                            .map(|v| v.frame().size)
                            .unwrap_or(NSSize::new(0.0, 0.0));
                        // The window is measured in points; the guest is driven in pixels.
                        let want = scale_for(&window, hidpi).to_guest((size.width, size.height));
                        if base != (0, 0)
                            && !in_live
                            && want.0 >= 64
                            && want.1 >= 64
                            && want != base
                            && want != resize_sent.get()
                        {
                            resize_sent.set(want);
                            desired_size.store(
                                crate::session::pack_size(want.0, want.1),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            send_resize(sock, slot, want.0, want.1);
                        }
                    }
                    // Host: drive the guest to the point size of the screen the window is
                    // on; re-push only when that changes (moved to another display, display
                    // reconfigured). Window drags never modeset the guest. Polled from the
                    // timer — the established pattern (no NSWindowDelegate), and `screen()`
                    // is None mid-transition, which simply skips the tick.
                    DisplayResolution::Host => {
                        if let (Some(screen), Some(host)) = (window.screen(), host.as_ref()) {
                            let want = host.size;
                            let key = host.identity_key();
                            // `adjusted` joins the gate so a refresh-only change still gets
                            // through: it moves neither the size nor the identity, so without it
                            // the push would be skipped and the guest would keep a stale refresh
                            // and VRR range forever.
                            if geom.get() != (0, 0)
                                && want.0 >= 64
                                && want.1 >= 64
                                && ((want, key) != screen_sent.get() || adjusted)
                            {
                                screen_sent.set((want, key));
                                identity_sent.set(key);
                                mode_sent.set(host.mode_key());
                                // Migrated to a differently-shaped display: re-lock resize to
                                // the new screen's aspect so the constraint tracks the screen.
                                apply_aspect_lock(&window, want);
                                // ...and reshape the window itself to that aspect NOW. The guest
                                // is being driven to the new screen's shape (below); the window,
                                // which AppKit left at its old shape on the drag across displays,
                                // would otherwise letterbox until the user resized it. Preserve
                                // the on-screen area, clamp into the new screen's visible frame;
                                // the per-tick fit recompute re-fits the scanout next tick.
                                if let Some(v) = window.contentView() {
                                    let cur = v.frame().size;
                                    let vis = screen.visibleFrame().size;
                                    let fullscreen =
                                        window.styleMask().contains(NSWindowStyleMask::FullScreen);
                                    let reshaped = fit::migration_reshape(
                                        (cur.width, cur.height),
                                        want,
                                        (vis.width, vis.height),
                                        fullscreen,
                                    );
                                    if display_trace() {
                                        eprintln!(
                                            "[DISPTRACE] push want={want:?} key={key:x} \
                                             migrated={migrated} fullscreen={fullscreen} inset={} \
                                             screen_h={} vis_h={} reshape=({},{})->{reshaped:?}",
                                            notch_inset_for(&screen, cfg_notch),
                                            screen.frame().size.height,
                                            vis.height,
                                            cur.width,
                                            cur.height,
                                        );
                                    }
                                    if let Some((nw, nh)) = reshaped {
                                        window.setContentSize(NSSize::new(
                                            f64::from(nw),
                                            f64::from(nh),
                                        ));
                                    }
                                }
                                desired_size.store(
                                    crate::session::pack_size(want.0, want.1),
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                // Three different events, three different pushes:
                                //
                                // - migration: hand over the new display's whole identity —
                                //   name, serial, refresh, density, VRR range — with the size,
                                //   so the guest's compositor recognizes the monitor and applies
                                //   that monitor's remembered configuration. Connector cycles.
                                // - adjustment: the same panel changed its refresh; push the new
                                //   EDID with the size, in place, so the range travels without
                                //   the guest ever losing its display.
                                // - plain resize: the host reconfigured this display's size and
                                //   nothing else, so send no EDID at all and leave the identity
                                //   exactly where it is.
                                if migrated {
                                    send_display_commands(
                                        sock,
                                        hostdisplay::migration_commands(
                                            host,
                                            true,
                                            hostdisplay::HotplugPolicy::from_env(),
                                            slot,
                                        ),
                                    );
                                } else if adjusted {
                                    send_display_command(
                                        sock,
                                        hostdisplay::adjustment_command(host, true, slot),
                                    );
                                } else if !handed_over {
                                    // The connect this tick already carried the size and the
                                    // identity together, so a size push here would be a second
                                    // modeset for one migration.
                                    //
                                    // `Resize` is the short form for display 0 only, so a window
                                    // on a panel that owns another slot sends the general form.
                                    send_display_command(
                                        sock,
                                        DisplayCommand::Display(
                                            limina_displayctl::DisplayControl {
                                                display_id: slot,
                                                size: Some(want),
                                                ..Default::default()
                                            },
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    // Fixed: the resolution is never pushed — the boot --display-size carries
                    // it, and a divergent guest (in-guest xrandr) just letterboxes differently.
                    // The display *identity* still is, below.
                    DisplayResolution::Fixed(..) => {}
                }

                // Identity/mode push for the modes whose size policy has nothing to fold it into.
                // Without this, a dynamic or fixed VM keeps the anonymous boot identity and a
                // flat 300 DPI on every display it is ever dragged to — so an ordinary external
                // monitor reads as Retina to the guest and it picks the wrong scale. These modes
                // may not dictate the guest's resolution, so both pushes go without a size; the
                // guest re-reads either way.
                // Gated on the guest having presented a frame, like every other push here.
                if !matches!(mode, DisplayResolution::Host) && geom.get() != (0, 0) {
                    if let Some(host) = host.as_ref() {
                        if migrated {
                            identity_sent.set(host.identity_key());
                            mode_sent.set(host.mode_key());
                            send_display_commands(
                                sock,
                                hostdisplay::migration_commands(
                                    host,
                                    false,
                                    hostdisplay::HotplugPolicy::from_env(),
                                    slot,
                                ),
                            );
                        } else if adjusted {
                            mode_sent.set(host.mode_key());
                            send_display_command(
                                sock,
                                hostdisplay::adjustment_command(host, false, slot),
                            );
                        }
                    }
                }
            }

            // Every guest window's walk — strip reconcile, refit, gen gate, modeset
            // follow, present — primary first, one collection (`windows::GuestWindows`).
            let slot_panels: std::collections::HashMap<usize, u64> = {
                let table = display_table.borrow();
                table
                    .connected_slots()
                    .into_iter()
                    .filter_map(|s| table.panel_of(s).map(|p| (s as usize, p)))
                    .collect()
            };
            guest_windows.borrow_mut().apply(
                &shared,
                &surface_map,
                &ack_tx,
                &windows::Layout {
                    panels: &slot_panels,
                    notch: cfg_notch,
                    reveal_ask: apply_input.reveal_ask_slot(),
                    mode,
                },
                mtm,
            );
        }
    });
    register_apply_hook(apply.clone());

    // Quit escalation state: set when the user closed the window / hit Ctrl-C and we asked
    // the guest agent to power off; reaching the deadline falls back to SIGKILL.
    let quit_deadline: Cell<Option<std::time::Instant>> = Cell::new(None);

    // M9.4 close-policy state: the action chosen for THIS close episode (Ask answers once; a
    // close-suspend that times out demotes itself to Shutdown here), and when the
    // close-triggered suspend was requested (bounds the wait before falling back).
    let close_choice: Cell<Option<crate::vmlib::schema::WindowCloseAction>> = Cell::new(None);
    let suspend_close_at: Cell<Option<std::time::Instant>> = Cell::new(None);

    // M9.4: intercept window closes so the close policy runs with the window still up
    // (see WindowCloseInterceptor). The Retained binding must outlive the run loop —
    // delegates are weak references (`run` never returns, so this frame suffices).
    let close_interceptor: Retained<WindowCloseInterceptor> =
        unsafe { msg_send![WindowCloseInterceptor::alloc(mtm), init] };
    window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(
        &*close_interceptor,
    )));

    // M9.4 felt-resume overlay: present from startup when restoring (splash until the first
    // presented frame); created by the timer when a suspend request is observed (dim scrim
    // over the live frame). One at a time.
    let window_opened_at = std::time::Instant::now();
    let timer_overlay: std::rc::Rc<RefCell<Option<overlay::Overlay>>> =
        std::rc::Rc::new(RefCell::new(None));
    if let Some(splash) = &restore_splash {
        timer_overlay
            .borrow_mut()
            .replace(overlay::Overlay::restore(&layer, &view, splash));
    }

    // Clone a window handle for the input monitor's fullscreen shortcut BEFORE the timer block
    // below moves `window` in.
    let shortcut_window = window.clone();

    let timer_primary_slot = primary_slot.clone();
    let timer_cursor = host_cursor.clone();
    let timer_primary = primary_core.clone();
    let timer_conn = conn.clone();
    let timer_captured = captured.clone();
    let timer_surface_map = surface_map.clone();
    // For the quit-check below: distinguish a real window CLOSE from a mere miniaturize/app-hide
    // (all three make the window not-visible, but only a close should power the guest off).
    let timer_app = app.clone();
    let timer_state_path = state_path.clone();
    // (`input_state` itself was created further up, before the capture tap, which shares it.)
    let timer_input = input_state.clone();
    // The same reading the tap takes (`[display] edge-resistance`, `Off` = never grab): the
    // screen-gain trigger is the policy grab, so it obeys the policy's own switch.
    let timer_grab_enabled =
        crate::vmlib::schema::EdgeHold::from_toml(edge_resistance).seconds() > 0.0;
    // Window key-focus state carried across ticks, so the timer can detect the key→not-key edge.
    // Seeded with the current state (the window was just made key), so the first tick is a no-op.
    let was_key = Cell::new(window.isKeyWindow());
    let was_on_space = Cell::new(window.isOnActiveSpace());
    // Last `[GRABSTATE]` tuple, so the trace reports transitions rather than 60 lines a second.
    let grab_traced: Cell<Option<GrabStateTrace>> = Cell::new(None);
    // Parked-window resume bookkeeping (task #18): when play was clicked (the felt-resume log)
    // and the worker epoch at the click, which is what tells the fresh worker's frames from the
    // suspended one's ([`resume_first_frame`]).
    let timer_view = view.clone();
    let timer_windows = guest_windows.clone();
    let timer_pointer_slot = pointer_slot.clone();
    let resume_clicked_at: Cell<std::time::Instant> = Cell::new(std::time::Instant::now());
    let resume_epoch_baseline: Cell<u64> = Cell::new(0);
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        // One-shot: the remembered fullscreen, taken on the first tick the window is actually on
        // screen. Not gated on the first frame — the guest is already sized for it, and waiting
        // for pixels would make the transition visible instead of the window simply appearing
        // fullscreen.
        if pending_fullscreen.replace(false)
            && !window.styleMask().contains(NSWindowStyleMask::FullScreen)
        {
            window.toggleFullScreen(None);
        }
        // Let go of what the guest has let go of, every tick — not only when a frame arrives.
        // A compositor that quits stops presenting, and its framebuffers are exactly the ones
        // worth reclaiming (testcomp/supervisor-retention.sh).
        timer_windows.borrow().drain_releases(&timer_surface_map);

        let (exited, worker_suspended, show_id, frames, worker_epoch, resume_dead) = {
            let s = shared.lock().unwrap();
            (
                s.worker_exited,
                s.worker_suspended,
                s.slots[timer_primary_slot.get() as usize].show_id,
                s.slots[timer_primary_slot.get() as usize].frames,
                s.worker_epoch,
                s.resume_dead,
            )
        };

        // Worker gone (guest powered off, orderly or not): net any process-group
        // stragglers and exit. (`conn.pid()` is the *current* worker — relaunch keeps it fresh.)
        // Gated on the Live phase: while Parked the dead worker is EXPECTED (the play glyph is
        // up), and while Resuming the monitor thread clears the flags only after the swap.
        if exited && PARK_STATE.with(|p| p.get()) == ParkPhase::Live {
            // A suspend teardown first saves the last-presented frame as the next restore's
            // splash (M9.4 felt-resume) — the IOSurface outlives the dead worker in our
            // mapping, so the grab still reads the final content.
            if worker_suspended {
                if let (Some(path), Some(id)) = (splash_save_path.as_deref(), show_id) {
                    let resolved = timer_surface_map
                        .lock()
                        .unwrap()
                        .get(id)
                        .or_else(|| IOSurfaceLookup(id));
                    match resolved {
                        Some(surface) => {
                            diag::capture_iosurface(&surface, id, &path.to_string_lossy())
                        }
                        None => log::warn!("splash save: surface {id} unresolved; skipping"),
                    }
                }
                // Park instead of quitting (task #18): a menu/CLI suspend keeps the window
                // up — final frame under a scrim, play glyph in the middle — so the VM is
                // one click from coming back. A close/stop-triggered suspend still quits:
                // the user asked the window to go away.
                if should_park_on_suspend(
                    CLOSE_REQUESTED.with(|c| c.get()),
                    crate::supervisor::stop_requested(),
                    resume_worker.is_some(),
                ) {
                    PARK_STATE.with(|p| p.set(ParkPhase::Parked));
                    // The satisfied request must not re-suspend the VM the moment it
                    // resumes (monitor() SIGTSTPs on a set flag).
                    crate::supervisor::clear_suspend_request();
                    // Whatever grab/held state the session had dies with the worker.
                    timer_input.release_all_held("park");
                    timer_input.release_capture(&timer_view);
                    let mut ov = timer_overlay.borrow_mut();
                    if let Some(o) = ov.take() {
                        o.remove();
                    }
                    if let Some(content) = window.contentView() {
                        if let Some(host_layer) = content.layer() {
                            ov.replace(overlay::Overlay::parked(&host_layer, &content));
                        }
                    }
                    // The other displays' windows go with it. This path returns before the
                    // frame-apply closure runs, so nothing else would ever take them down —
                    // and while covering they are borderless and above the menu bar, which
                    // means a panel frozen on its last frame with no way to dismiss it. They
                    // come back on resume, when the restored worker presents again.
                    timer_windows.borrow_mut().close_secondaries();
                    window.setTitle(&NSString::from_str(&format!("{title} — Suspended")));
                    log::info!("VM suspended; window parked (click to resume)");
                    return;
                }
            }
            save_state_final(timer_state_path.as_deref(), &window);
            kill_worker_group(timer_conn.pid());
            crate::exit_cleanup();
            std::process::exit(0);
        }

        // Parked / resuming (task #18): the play click signals the monitor thread (which
        // respawns the worker into a restore), the "Resuming…" overlay rides until the
        // fresh worker's first presented frame, then the window is live again.
        match PARK_STATE.with(|p| p.get()) {
            ParkPhase::Live => {}
            ParkPhase::Parked => {
                if RESUME_REQUESTED.with(|r| r.take()) {
                    let sent = resume_worker.as_ref().is_some_and(|tx| tx.send(()).is_ok());
                    if sent {
                        PARK_STATE.with(|p| p.set(ParkPhase::Resuming));
                        resume_clicked_at.set(std::time::Instant::now());
                        resume_epoch_baseline.set(worker_epoch);
                        let mut ov = timer_overlay.borrow_mut();
                        if let Some(o) = ov.take() {
                            o.remove();
                        }
                        if let Some(content) = window.contentView() {
                            if let Some(host_layer) = content.layer() {
                                ov.replace(overlay::Overlay::resuming(&host_layer, &content));
                            }
                        }
                        window.setTitle(&NSString::from_str(&title));
                    } else {
                        // The monitor thread is gone — resuming is impossible; quit like a
                        // plain suspend exit would have (the snapshot stays pending, the
                        // next start restores).
                        log::error!("resume channel dead; closing the parked window");
                        save_state_final(timer_state_path.as_deref(), &window);
                        crate::exit_cleanup();
                        std::process::exit(0);
                    }
                }
            }
            ParkPhase::Resuming => {
                // The fresh worker died (or was never spawned) before its first frame: the
                // dogfood 2026-08-10 replay SIGSEGV left the window on "Resuming…" forever
                // — the Live-gated exit branch above never fires in this phase. Surface
                // the failure and quit; the snapshot was consumed at spawn (one-shot by
                // design), so the next start cold-boots.
                if resume_worker_died(
                    exited,
                    resume_dead,
                    worker_epoch,
                    resume_epoch_baseline.get(),
                ) {
                    log::error!(
                        "resume: the fresh worker died before its first frame \
                         (epoch {worker_epoch}, resume_dead {resume_dead}); quitting"
                    );
                    let alert = NSAlert::new(mtm);
                    alert.setMessageText(&NSString::from_str(&format!(
                        "“{title}” failed to resume"
                    )));
                    alert.setInformativeText(&NSString::from_str(
                        "The VM worker crashed while restoring the suspended session. \
                         The VM is now powered off; starting it again will boot fresh.",
                    ));
                    alert.addButtonWithTitle(&NSString::from_str("OK"));
                    alert.runModal();
                    save_state_final(timer_state_path.as_deref(), &window);
                    crate::exit_cleanup();
                    std::process::exit(1);
                }
                if resume_first_frame(worker_epoch, resume_epoch_baseline.get(), frames) {
                    PARK_STATE.with(|p| p.set(ParkPhase::Live));
                    if let Some(o) = timer_overlay.borrow_mut().take() {
                        o.remove();
                    }
                    // The felt-resume endpoint for in-place resumes (perf oracle).
                    log::info!(
                        "resume: first frame presented {:.1}s after the play click",
                        resume_clicked_at.get().elapsed().as_secs_f32()
                    );
                }
            }
        }

        // M9.4 overlay lifecycle: the restore splash comes down on the first presented frame;
        // the suspend dim appears when a suspend request is observed (close-triggered or
        // `limina suspend`) and comes down if the bracket is abandoned (timeout → the VM keeps
        // running). While up, re-fit to the content view every tick. The parked/resuming
        // flavors (task #18) are owned by the PARK_STATE machine above — here they only get
        // the per-tick re-fit, never the take-down/install logic (whose `!suspending` arm
        // would tear the parked overlay down: the request is cleared when the park begins).
        {
            let mut ov = timer_overlay.borrow_mut();
            let live = PARK_STATE.with(|p| p.get()) == ParkPhase::Live;
            let suspending = crate::supervisor::suspend_requested();
            let take_down = match ov.as_ref() {
                _ if !live => false,
                Some(o) if o.until_first_frame => frames > 0,
                Some(_) => !suspending,
                None => false,
            };
            if take_down {
                if let Some(o) = ov.take() {
                    // A suspend flavor coming down = the bracket was ABANDONED (timeout;
                    // the VM keeps running). Forget the close episode too, so a later close
                    // starts a fresh suspend instead of resuming a spent one.
                    if !o.until_first_frame {
                        close_choice.set(None);
                        suspend_close_at.set(None);
                        CLOSE_REQUESTED.with(|c| c.set(false));
                    } else {
                        // The felt-resume endpoint (perf oracle): splash up → first
                        // presented guest frame.
                        log::info!(
                            "restore: first frame presented {:.1}s after the window opened",
                            window_opened_at.elapsed().as_secs_f32()
                        );
                    }
                    o.remove();
                }
            } else if let Some(o) = ov.as_ref() {
                if let Some(content) = window.contentView() {
                    o.fit(&content);
                }
            } else if suspending {
                if let Some(content) = window.contentView() {
                    if let Some(host_layer) = content.layer() {
                        ov.replace(overlay::Overlay::suspend(&host_layer, &content));
                    }
                }
            }
            // The other panels freeze with this one, so they say so with it. Only the suspend
            // flavor reaches them: by the time the window is parked or resuming, the park has
            // closed every secondary.
            timer_windows.borrow_mut().veil(live && suspending);
        }

        // Release keys held when the window loses key focus (e.g. the user hit Cmd-Tab): the local
        // event monitor stops delivering events the instant focus leaves, so the key-up — notably
        // the Command release — never arrives and the key would stick "down" in the guest. Polled
        // here on the key→not-key edge rather than via an NSWindowDelegate (the window's deliberate
        // no-delegate pattern); the timer keeps firing while the app is backgrounded, so it catches
        // the app-switch case too.
        // ONE ownership snapshot for this tick — the same assembler as the tap's per-event one
        // (`InputState::window_facts`), so the tick and the tap can never answer an ownership
        // question differently.
        // The Input menu writes only the switch; adopting it is the tick's job, because the
        // translator has to drain the keyboard through the old mapping first and that must not
        // happen inside a menu click. Idempotent, so an unchanged switch costs a comparison.
        timer_input.set_normalize(MODIFIER_NORMALIZE.with(|f| f.get()));
        let facts = timer_input.window_facts(&timer_view);
        let pf = grab_policy::primary_facts(&facts);
        // App-level, like every other key question here: focus moving from the primary to a
        // covering secondary is movement INSIDE the VM, and dumping the held modifiers for it
        // would drop a chord mid-press whenever the user clicked the other display.
        let is_key = facts.iter().any(|f| f.key);
        if was_key.get() && !is_key {
            timer_input.release_all_held("key-loss");
        } else if !was_key.get() && is_key && input::input_trace() {
            // The regain edge emits nothing today — it is logged so a trace shows the gap between
            // "focus is back" and the first event that tells us anything about the modifiers.
            eprintln!(
                "[INP] t={:.1} key-GAIN (no modifier resync happens here)",
                capture_tap::trace_ms(),
            );
        }
        was_key.set(is_key);

        // Give a live grab back when the window it belongs to is no longer on screen, and flush
        // whatever was held on the way out. Polled here for the same reason the key-release above
        // is: both grabs are driven by input events, and a Space leaving produces none.
        //
        // The flush is on the *edge* and is not conditioned on capture, because the gesture that
        // takes the Space away is itself made of keys: Ctrl-Up is delivered to the guest as
        // Ctrl-down while the soft keyboard grab is still engaged, and the matching key-up then
        // goes to macOS instead — leaving Ctrl stuck down in the guest for good.
        let on_active_space = pf.on_active_space;
        if was_on_space.get() && !on_active_space {
            timer_input.release_all_held("space-leave");
        } else if !was_on_space.get() && on_active_space && input::input_trace() {
            eprintln!(
                "[INP] t={:.1} space-RETURN (no modifier resync happens here)",
                capture_tap::trace_ms(),
            );
        }
        was_on_space.set(on_active_space);
        if grab_policy::must_drop_grab(
            timer_input.captured_flag(),
            grab_policy::capture_owner(&facts, timer_input.capture_slot()),
        ) {
            log::debug!("pointer grab released: the cursor's window is not on screen");
            timer_input.release_capture_gone(&timer_view);
        }
        // Backstop for the tap's per-event key-loss release: without the tap (no Accessibility
        // grant) the local monitor stops seeing events the instant focus leaves, so nothing
        // event-driven can hand a captured pointer back. The tap remains the low-latency
        // consumer of the same predicate; this only changes when the release lands in
        // event-free windows.
        if grab_policy::key_loss_releases(timer_input.captured_flag(), &facts) {
            log::info!("pointer capture: released — the window lost focus (tick backstop)");
            timer_input.release_capture(&timer_view);
        }

        // `LIMINA_EDGE_TRACE`: every bit that could tell us a live pointer grab has lost its
        // context, on one line, on transitions.
        //
        // Reported 2026-08-08: the hard grab sticks through a Mission Control gesture (Ctrl-Up or
        // three fingers up) and through an unplug that moves our Space out of view — the pointer
        // stays decoupled and hidden with no way to reach anything. Nothing releases it today
        // except the user and the suspend path, so a release condition has to be added; which
        // *signal* to hang it on is the open question, and guessing between key status, Space
        // membership and screen presence is how the other two bugs today started. So they are all
        // recorded, and the fix will use whichever ones actually move.
        if capture_tap::edge_trace() {
            let now = (
                timer_input.captured_flag(),
                is_key,
                on_active_space,
                pf.has_screen,
                NSApplication::sharedApplication(mtm).isActive(),
                // Does occlusion move BEFORE the Space switch commits? Every other bit here is
                // known to move at the commit, which is why a captured pointer stays hidden and
                // pinned through the whole animation.
                window
                    .occlusionState()
                    .contains(NSWindowOcclusionState::Visible),
            );
            if grab_traced.get() != Some(now) {
                grab_traced.set(Some(now));
                eprintln!(
                    "[GRABSTATE] t={:.1} captured={} key={} on_active_space={} \
                     has_screen={} app_active={} occl_visible={}",
                    capture_tap::trace_ms(),
                    now.0,
                    now.1,
                    now.2,
                    now.3,
                    now.4,
                    now.5,
                );
            }
        }

        // Remember the window placement: once the frame settles (~half a second stable,
        // not mid live-resize — the snapshot itself skips fullscreen), persist it on a
        // throwaway thread (the send_resize pattern; the write is atomic, so a torn run
        // at worst loses the last save). state.toml is disposable — best-effort throughout.
        if let Some(path) = &timer_state_path {
            let in_live = window
                .contentView()
                .map(|v| v.inLiveResize())
                .unwrap_or(false);
            if !in_live {
                if let Some(snap) = window_state_snapshot(&window, saved_state.get()) {
                    if pending_state.get() != Some(snap) {
                        pending_state.set(Some(snap));
                        stable_ticks.set(0);
                    } else if stable_ticks.get() < 30 {
                        stable_ticks.set(stable_ticks.get() + 1);
                        if stable_ticks.get() == 30 && saved_state.get() != Some(snap) {
                            saved_state.set(Some(snap));
                            let path = path.clone();
                            std::thread::spawn(move || {
                                // Merge (set_window), never whole-save — see save_state_final.
                                if let Err(e) = crate::vmlib::state::set_window(&path, Some(snap)) {
                                    log::warn!("window state save failed: {e}");
                                }
                            });
                        }
                    }
                }
            }
        }

        // The user closed the window (intercepted — the window is still visible) or hit
        // Ctrl-C / `limina stop`. Stop requests always power off; a window CLOSE consults
        // the `[display] on_window_close` policy (M9.4): suspend (default — the dim +
        // spinner run in place, then the process exits), shutdown (the power-off ladder,
        // window closed programmatically), or ask. A minimized window or a hidden app is
        // NOT a close (visibility is only a backstop signal here — the interceptor flag is
        // the real close trigger).
        if should_initiate_quit(
            crate::supervisor::stop_requested(),
            window.isVisible(),
            window.isMiniaturized(),
            timer_app.isHidden(),
        ) || CLOSE_REQUESTED.with(|c| c.get())
        {
            // Parked (task #18): the VM is already suspended — a close or stop just quits
            // the app, leaving the snapshot pending (the next start resumes). No close
            // policy (nothing left to suspend), no shutdown ladder, and no process-group
            // kill: the worker died at the suspend, and its pid may have been recycled by
            // now — kill(-pid) could hit an innocent process.
            if PARK_STATE.with(|p| p.get()) == ParkPhase::Parked {
                save_state_final(timer_state_path.as_deref(), &window);
                crate::exit_cleanup();
                std::process::exit(0);
            }
            use crate::vmlib::schema::WindowCloseAction;
            let action = if crate::supervisor::stop_requested() {
                Some(WindowCloseAction::Shutdown)
            } else if let Some(a) = close_choice.get() {
                Some(a)
            } else {
                let picked = match on_window_close {
                    WindowCloseAction::Ask => ask_close_action(mtm, &title),
                    other => Some(other),
                };
                match picked {
                    Some(a) => {
                        close_choice.set(Some(a));
                        Some(a)
                    }
                    None => {
                        // Cancel: the close was intercepted, the window never went away —
                        // just forget it was asked.
                        CLOSE_REQUESTED.with(|c| c.set(false));
                        None
                    }
                }
            };
            match action {
                None => {}
                Some(WindowCloseAction::Suspend) => {
                    if suspend_close_at.get().is_none() {
                        log::info!("window close → suspending the VM (on_window_close)");
                        crate::supervisor::request_suspend();
                        suspend_close_at.set(Some(std::time::Instant::now()));
                        // The window stays up (close intercepted): the overlay block above
                        // shows the dim + spinner for the save's duration. Success exits
                        // via the `exited` branch; a bracket timeout clears the request
                        // and the overlay block resets this close episode.
                    }
                }
                // Ask cannot reach here (resolved above); route it like Shutdown for safety.
                Some(WindowCloseAction::Shutdown | WindowCloseAction::Ask) => {
                    // A close-triggered shutdown hides the window now, matching the native
                    // close feel (the intercept kept it up; `close()` bypasses the
                    // delegate). Stop-triggered shutdowns keep it visible while the guest
                    // powers off — the pre-existing Ctrl-C behavior.
                    if CLOSE_REQUESTED.with(|c| c.get()) && window.isVisible() {
                        window.close();
                    }
                    // A SECOND stop signal (limina stop --force, impatient double Ctrl-C)
                    // skips whatever grace remains — mirror of the headless monitor ladder.
                    let force_now = crate::supervisor::force_stop_requested()
                        || match quit_deadline.get() {
                            None => {
                                let orderly = control
                                    .as_ref()
                                    .map(|c| c.request_shutdown(crate::control::AGENT_GRACE))
                                    .unwrap_or(false);
                                if orderly {
                                    log::info!(
                                        "window closed → asked the guest agent to power off"
                                    );
                                    quit_deadline.set(Some(
                                        std::time::Instant::now() + crate::control::AGENT_GRACE,
                                    ));
                                    false
                                } else {
                                    true
                                }
                            }
                            Some(d) => std::time::Instant::now() >= d,
                        };
                    if force_now {
                        save_state_final(timer_state_path.as_deref(), &window);
                        kill_worker_group(timer_conn.pid());
                        crate::exit_cleanup();
                        std::process::exit(0);
                    }
                }
            }
        }

        // Guest cursor shape first — it has its own gen so a shape change (or hide)
        // applies even when the scanout hasn't produced a new frame. The shape is built at
        // the window's content scale (the fit rect over the guest resolution), so a resize
        // that rescales the desktop rescales the pointer with it.
        // The shape belongs to the display the POINTER is over, not to the primary: the guest
        // enables its cursor plane on one CRTC and hides it on the others, so reading slot 0
        // meant the `cursorhide` for the display the pointer left blanked the host cursor while
        // the display it arrived on was publishing a perfectly good one.
        // Both halves of the scale come from the window the pointer is over. Mixing the
        // primary's width with another display's guest mode drew the sprite at the wrong size
        // and offset its hotspot by the same factor, so clicks landed away from the drawn tip.
        let (cursor_slot, pointer_content_w) = timer_pointer_slot.get();
        let cursor_fit_w = if pointer_content_w > 0.0 {
            pointer_content_w
        } else {
            timer_primary.fit().w
        };
        // The SHAPE comes from whichever slot the guest has its plane on ([`cursor::shape_slot`]) —
        // routinely not the slot the pointer is over — while the SCALE stays the pointer's own
        // window's, because that is where the sprite is drawn.
        let (cur, guest_w, shape_slot) = {
            let s = shared.lock().unwrap();
            let visible: Vec<bool> = s.slots.iter().map(|sl| sl.cursor.visible).collect();
            let shape_slot = cursor::shape_slot(cursor_slot, &visible);
            let c = s.slots[shape_slot].cursor;
            (
                (c.gen, c.visible, c.id, c.w, c.h, c.hot_x, c.hot_y),
                s.slots[cursor_slot].width,
                shape_slot,
            )
        };
        let scale_key = cursor::cursor_scale_key(cursor_fit_w, guest_w);
        let scale_moved = built_cursor.get().is_some_and(|(_, k)| k != scale_key);
        if (shape_slot, cur.0) != last_cursor_gen.get() || scale_moved {
            // Only on success: a build that failed leaves the pointer blank, and marking the
            // generation done would keep it that way until the guest next changed shape.
            if apply_cursor(&timer_cursor, &built_cursor, &cur, &surface_map, scale_key) {
                last_cursor_gen.set((shape_slot, cur.0));
            }
        }
        // Pointer-capture cursor: while captured, composite the guest cursor at its reported
        // position (the host NSCursor is hidden then). Position moves every frame, so unlike
        // the shape this runs every tick, not gated on `cursor_gen`. Every window draws its
        // OWN slot, never the pointer's — see `GuestWindows::update_capture_cursors`.
        let layer = timer_windows.borrow().update_capture_cursors(
            &timer_captured,
            &shared,
            &timer_surface_map,
            cursor_slot,
        );
        // Quiet until the impossible happens: captured, the guest plainly has a cursor, and the
        // display the user is driving draws none. Then say everything at once — which gate
        // fired, and what last wrote each slot's flags — because this is rare, it self-heals
        // the moment the guest re-uploads, and the report otherwise arrives with a log that
        // cannot answer why (dogfood 2026-08-24). Logged on the transition only; the recovery
        // closes the episode at `info` so its length is readable.
        let fault = layer.and_then(|v| {
            let s = shared.lock().unwrap();
            let visible: Vec<bool> = s.slots.iter().map(|sl| sl.cursor.visible).collect();
            cursor::undrawn_fault(cursor_slot, v, &visible).map(|why| {
                let state: Vec<String> = s
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, sl)| sl.width > 0 || sl.cursor.visible || sl.cursor.id.is_some())
                    .map(|(i, sl)| {
                        let c = sl.cursor;
                        format!(
                            "slot {i}: scanout {}x{} cursor visible={} id={:?} {}x{} at ({},{}) [{}]",
                            sl.width,
                            sl.height,
                            c.visible,
                            c.id,
                            c.w,
                            c.h,
                            c.pos_x,
                            c.pos_y,
                            c.log.recent().join(" ")
                        )
                    })
                    .collect();
                format!("{why}; {}", state.join("; "))
            })
        });
        match fault {
            Some(why) => {
                // A tick or two of nothing is ordinary — taking the grab, a slot whose first
                // image has not landed — and a check that shouts at those gets ignored. Only a
                // state that STANDS is the one worth reading; the real one stood until a
                // modeset.
                let since = match cursor_fault_since.get() {
                    Some(t) => t,
                    None => {
                        let t = std::time::Instant::now();
                        cursor_fault_since.set(Some(t));
                        t
                    }
                };
                if !cursor_fault_said.get() && since.elapsed() >= CURSOR_FAULT_SETTLE {
                    log::warn!(
                        "guest cursor: nothing has been drawn where the captured pointer is for {:.1}s — {why}",
                        since.elapsed().as_secs_f64(),
                    );
                    cursor_fault_said.set(true);
                }
            }
            None => {
                if cursor_fault_said.get() {
                    log::info!("guest cursor: drawing again on the captured slot {cursor_slot}");
                }
                cursor_fault_since.set(None);
                cursor_fault_said.set(false);
            }
        }

        // PROBE (edge-trace only): can the macOS fullscreen menu-bar reveal be OBSERVED?
        // The band's stand-down currently fires on our own push gesture, macOS's reveal fires
        // on its own — two thresholds estimating one intent, and the user stops pushing at
        // whichever fires first (macOS's). If either signal below tracks the actual reveal,
        // the band can be slaved to it instead of estimated. Logged on transition only.
        if capture_tap::edge_trace() {
            use std::cell::Cell;
            thread_local! {
                static MENUBAR_LAST: Cell<(bool, i32, i32)> = const { Cell::new((false, -1, -1)) };
            }
            let vis = objc2_app_kit::NSMenu::menuBarVisible(mtm);
            let gaps: Vec<i32> = objc2_app_kit::NSScreen::screens(mtm)
                .iter()
                .map(|s| {
                    let f = s.frame();
                    let v = s.visibleFrame();
                    ((f.origin.y + f.size.height) - (v.origin.y + v.size.height)) as i32
                })
                .collect();
            let g0 = gaps.first().copied().unwrap_or(-1);
            let g1 = gaps.get(1).copied().unwrap_or(-1);
            let now = (vis, g0, g1);
            MENUBAR_LAST.with(|c| {
                if c.get() != now {
                    c.set(now);
                    eprintln!(
                        "[MENUBAR] t={:.1} visible={vis} top_gaps={gaps:?}",
                        capture_tap::trace_ms()
                    );
                }
            });
        }

        // Chrome ask, slaved to the OBSERVED macOS menu bar: macOS's fullscreen reveal and
        // our band stand-down judge the same push, and two independent thresholds meant the
        // user stopped pushing at whichever fired first (macOS's) with the strip still up —
        // covering the very band the revealed menu bar lives in on a notched panel. See
        // `InputState::menubar_observed`.
        timer_input.menubar_observed(objc2_app_kit::NSMenu::menuBarVisible(mtm), &timer_view);

        // Everything from here to the frame apply serves a guest, so none of it runs without
        // one ([`speaks_for_a_guest`]): a parked or resuming window has a dead worker behind it,
        // and acting for it means seizing the pointer nothing can use, hiding the cursor over a
        // still frame, and probing a device that is gone.
        if speaks_for_a_guest(PARK_STATE.with(|p| p.get())) {
            // The captured cursor follows the guest (`window/echo.rs`): re-base the estimate onto
            // the slot and pixel the guest's cursor echo names, then — once the last position we
            // sent has had time to echo back — compare and warn on a readable disagreement.
            timer_input.follow_guest_echo(&timer_view);
            // …and once the hand pauses, the park follows the cursor onto that panel
            // (`InputState::repark_if_quiescent`), so swipes act where the user is looking.
            timer_input.repark_if_quiescent(&timer_view);
            timer_input.verify_guest_echo();
            // The blank the captured host pointer wears is advisory — AppKit resets the cursor from
            // its own rects, and while the tap consumes motion no event comes back for us to answer
            // with. This is where a stripped wear (a second, static pointer on top of the guest's)
            // is noticed and put back.
            timer_input.verify_captured_wear();
            // Going fullscreen, or a panel joining a session that is already fullscreen ("Use Other
            // Screens When Fullscreen"), hands the guest the pointer. Neither arrives as an event
            // the tap sees, and the second happens while the user is still in a macOS menu — so it
            // is polled here, where the window facts are already being read.
            timer_input.grab_on_screen_gain(&timer_view, timer_grab_enabled);
            // Sweep the absolute device to learn each display's share of it, rather than waiting
            // for the user to cross the seam by accident. Grabbed or not: the mapping is the
            // uncaptured pointer's, so it should be known before the pointer first needs it.
            timer_input.probe_mapping(&timer_view);
        }

        // Frame apply: normally event-driven (dispatch from the reader thread); this is
        // the fallback so a lost wake-up costs one tick, not a stuck frame.
        apply();
    });

    // ~60 Hz poll. Keep the timer alive for the app's lifetime. Schedule it in COMMON modes (not
    // just the default mode): `scheduledTimer...` adds it to NSDefaultRunLoopMode only, so it
    // FREEZES during a live window resize (the run loop runs in NSEventTrackingRunLoopMode while
    // the user drags). A frozen present timer leaves the layer stale → the window goes black mid/
    // post-drag and only recovers on the next forced repaint. Common modes keeps it firing through
    // the drag, so the present + resize-detection stay live the whole time.
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(1.0 / 60.0, true, &block) };
    unsafe {
        NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
    }
    let _timer = timer;

    // Whether a macOS menu is open, for the grab (see `capture_tap::set_menu_open`). Observed
    // rather than asked: AppKit has no "is a menu up" property, and the tap cannot tell a click
    // that closes a menu from a click asking for the guest — the window server answers "guest
    // content" for any point the menu's panel does not happen to cover.
    //
    // `object: None` so it catches every menu in the app, not one we remember to wire up; the
    // observer is deliberately never removed (app lifetime, like the tap's context).
    {
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        let opened = RcBlock::new(move |_n: NonNull<objc2_foundation::NSNotification>| {
            capture_tap::set_menu_open(true);
        });
        let closed = RcBlock::new(move |_n: NonNull<objc2_foundation::NSNotification>| {
            capture_tap::set_menu_open(false);
        });
        unsafe {
            let _ = center.addObserverForName_object_queue_usingBlock(
                Some(objc2_app_kit::NSMenuDidBeginTrackingNotification),
                None,
                None,
                &opened,
            );
            let _ = center.addObserverForName_object_queue_usingBlock(
                Some(objc2_app_kit::NSMenuDidEndTrackingNotification),
                None,
                None,
                &closed,
            );
        }
    }

    // Capture keyboard + mouse via a local event monitor and forward them to the worker as evdev
    // events. Swallowed key events return null; pass-through events return themselves. The
    // translator (`input_state`, an Rc) was created above so the render timer shares it (to flush
    // held keys on focus loss); the monitor moves its handle in and drives it per event.
    let monitor_view = view.clone();
    let input_block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: the monitor hands us a valid, live event for the call's duration.
        let ev = unsafe { event.as_ref() };
        // Parked window (task #18): the guest is suspended, so nothing is forwarded. A
        // left click on the content (the play glyph fills it as an affordance, but the
        // whole content is the target, Parallels-style) requests the resume; everything
        // else passes to AppKit untouched so the window itself (title bar drag, menus,
        // Cmd-W) keeps behaving like a normal mac window.
        if parked() {
            if ev.r#type() == NSEventType::LeftMouseDown {
                let p = input::event_point_in_view(ev, &monitor_view);
                let b = monitor_view.bounds();
                if p.x >= 0.0 && p.y >= 0.0 && p.x < b.size.width && p.y < b.size.height {
                    RESUME_REQUESTED.with(|r| r.set(true));
                    return std::ptr::null_mut(); // the click was the play button, not input
                }
            }
            return event.as_ptr();
        }
        // Host shortcuts are intercepted BEFORE the guest sees them. We match on key-down; the
        // orphan key-up that leaks to the guest is dropped by the guest input core (a release of
        // an un-pressed key). Modifier flagsChanged still flow through, which is fine.
        if ev.r#type() == NSEventType::KeyDown {
            if let Some(sc) = input::match_host_shortcut(ev.keyCode(), ev.modifierFlags().0 as u64)
            {
                match sc {
                    input::HostShortcut::ToggleFullScreen => {
                        // One mechanism for both policies now: `extend` is delivered by an
                        // overlay floating over this very Space (see `ExtendOverlay`), not by a
                        // different kind of fullscreen. That also retires the wart where the
                        // green title-bar button and this shortcut did different things.
                        shortcut_window.toggleFullScreen(None);
                    }
                    input::HostShortcut::ToggleCapture => {
                        // An installed tap consumes this combo itself, so reaching the local
                        // monitor means the tap is MISSING (no Accessibility at startup). Retry —
                        // a grant given since then takes effect on a fresh create — and if it's
                        // still missing, raise the system prompt (once per run) rather than
                        // degrading silently to the warp path.
                        if capture_tap::retry_install() {
                            // Tap present (or just healed by the retry) — capture normally.
                            input_state.toggle_capture(&monitor_view);
                        } else if !capture_tap::prompt_accessibility_once() {
                            // The prompt was already shown earlier and Accessibility is still
                            // ungranted: honor the toggle in degraded (leaky warp) mode. On the
                            // FIRST tap-less toggle prompt_accessibility_once() returns true and we
                            // deliberately do NOT grab — the just-opened Accessibility dialog needs
                            // a clickable cursor, and a captured pointer is parked/consumed. The
                            // user grants, then presses Cmd-Ctrl-G again to actually capture.
                            input_state.toggle_capture(&monitor_view);
                        }
                    }
                }
                return std::ptr::null_mut(); // swallow — don't forward to the guest
            }
        }
        let swallow = input_state.handle(ev, &monitor_view);
        if swallow {
            std::ptr::null_mut()
        } else {
            event.as_ptr()
        }
    });
    let input_mask = NSEventMask::KeyDown
        | NSEventMask::KeyUp
        | NSEventMask::FlagsChanged
        | NSEventMask::MouseMoved
        | NSEventMask::LeftMouseDown
        | NSEventMask::LeftMouseUp
        | NSEventMask::LeftMouseDragged
        | NSEventMask::RightMouseDown
        | NSEventMask::RightMouseUp
        | NSEventMask::RightMouseDragged
        | NSEventMask::OtherMouseDown
        | NSEventMask::OtherMouseUp
        | NSEventMask::OtherMouseDragged
        | NSEventMask::ScrollWheel;
    // Keep the monitor alive for the app's lifetime (dropping it removes the monitor).
    let _monitor =
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(input_mask, &input_block) };

    app.run();
    // The run loop only returns if AppKit tears down unexpectedly; the timer is what
    // normally exits us. Either way, don't fall through.
    crate::exit_cleanup();
    std::process::exit(0);
}

#[cfg(test)]
mod tests {

    /// Cross-BATCH order is as load-bearing as in-batch order: a migration cycle's settle
    /// (CONNECTOR_DOWN_SETTLE) parks its batch mid-flight, and a batch spawned for a later
    /// event (a second migration, a dynamic-mode resize) must not overtake it — the guest
    /// would apply the earlier replug LAST and keep a stale identity nothing repairs. One
    /// sender queue serializes them.
    #[test]
    fn display_batches_reach_the_wire_in_submission_order() {
        use std::io::Read;
        let path =
            std::env::temp_dir().join(format!("limina-dispsend-order-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let cyc = |id: u32, up: bool| {
            limina_displayctl::DisplayCommand::Display(limina_displayctl::DisplayControl {
                display_id: id,
                connected: Some(up),
                ..Default::default()
            })
        };
        let (a1, a2, b1) = (cyc(0, false), cyc(0, true), cyc(1, true));
        let expected = [a1.to_wire(), a2.to_wire(), b1.to_wire()];
        // Batch A parks in the unplug settle; batch B is submitted while A sleeps.
        super::send_display_commands(&path, vec![a1, a2]);
        super::send_display_commands(&path, vec![b1]);
        let mut lines = Vec::new();
        for _ in 0..3 {
            let (mut conn, _) = listener.accept().unwrap();
            let mut s = String::new();
            conn.read_to_string(&mut s).unwrap();
            lines.push(s.trim().to_string());
        }
        let _ = std::fs::remove_file(&path);
        assert_eq!(lines, expected, "a later batch overtook an in-flight cycle");
    }

    /// The Displays menu's tag must survive the row list shifting under an open menu: the
    /// render timer republishes DISPLAY_MENU on hotplug, and a click routed by INDEX would
    /// land on whichever row slid into that position — switching the wrong display. Identity
    /// tags find the same display, or nothing once it is gone.
    #[test]
    fn a_menu_click_survives_the_rows_shifting_under_it() {
        let row = |panel: u64, name: &str| super::DisplayMenuRow {
            panel,
            name: name.into(),
            enabled: true,
            primary: false,
        };
        let before = [row(10, "a"), row(20, "b"), row(0x8000_0000_0000_0003, "c")];
        // The user opens the menu over "c" (its tag bakes in the panel key)...
        let tag = before[2].panel as isize;
        // ...panel "b" unplugs before the click, shifting the list.
        let after = [row(10, "a"), row(0x8000_0000_0000_0003, "c")];
        let hit = super::menu_row_for_tag(&after, tag).expect("the display is still attached");
        assert_eq!(
            hit.name, "c",
            "the click must land on the display the user aimed at"
        );
        // The display itself unplugging means the click lands on nothing, never a neighbour.
        let gone = [row(10, "a"), row(20, "b")];
        assert!(super::menu_row_for_tag(&gone, tag).is_none());
    }
    use super::*;

    /// A migration cycle is an unplug followed by a replug, and inverting them leaves the guest
    /// with the connector DOWN and nothing queued to raise it — no display at all, which is far
    /// worse than the stale identity the cycle exists to fix. The old sender spawned a thread per
    /// command, so two calls could invert; this pins the order on the wire.
    #[test]
    fn a_cycle_reaches_the_socket_as_two_commands_in_order() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("limina-cycle-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("display.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");

        let host = hostdisplay::HostDisplay {
            size: (2560, 1440),
            logical: (2560, 1440),
            edid: limina_displayctl::EdidSpec {
                serial: 0xFEED_FACE,
                name: "Test Panel".into(),
                ..Default::default()
            },
        };
        send_display_commands(
            &path,
            hostdisplay::migration_commands(&host, true, hostdisplay::HotplugPolicy::Cycle, 0),
        );

        // One connection per command; the order they arrive in is the whole assertion.
        let mut lines = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read");
            lines.push(line.trim().to_string());
        }

        assert!(
            lines[0].contains("connected=0"),
            "the unplug must arrive FIRST, got {lines:?}"
        );
        assert!(
            !lines[0].contains("serial="),
            "the unplug must not carry the new identity, got {:?}",
            lines[0]
        );
        assert!(
            lines[1].contains("connected=1"),
            "the replug must arrive second, got {lines:?}"
        );
        assert!(
            lines[1].contains(&format!("serial={}", 0xFEED_FACEu32)),
            "the replug must carry the new EDID, got {:?}",
            lines[1]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Longer than `OVERLAY_SETTLE`: the condition has held.

    #[test]
    fn a_menu_or_cli_suspend_parks_but_a_close_or_stop_still_quits() {
        // Task #18: the play-button window exists precisely for suspends where the user KEPT
        // the window (menu Suspend, `limina suspend`) — parking after the user closed the
        // window (or hit Ctrl-C / `limina stop`) would resurrect a window they dismissed.
        //
        // args: (close_requested, stop_requested, can_park)
        assert!(
            should_park_on_suspend(false, false, true),
            "a menu/CLI suspend with a live resume channel must park"
        );
        assert!(
            !should_park_on_suspend(true, false, true),
            "close-to-suspend must still close the window"
        );
        assert!(
            !should_park_on_suspend(false, true, true),
            "a stop-requested suspend must still quit"
        );
        // No resume channel = parking would strand the window (the play click could never
        // respawn a worker) — always quit.
        assert!(!should_park_on_suspend(false, false, false));
    }

    #[test]
    fn a_window_with_no_worker_behind_it_speaks_for_no_guest() {
        // Rig 2026-08-22: a suspended VM's window took the pointer and hid it on every visit to
        // its Space — `grab_on_screen_gain` fires because a parked fullscreen window really does
        // gain the screen, and its refusal chain never asked whether there is a guest to serve.
        // The tap and the NSEvent monitor both stand down while parked; the tick did not.
        assert!(speaks_for_a_guest(ParkPhase::Live));
        assert!(!speaks_for_a_guest(ParkPhase::Parked));
        // Resuming is the half the obvious test would miss: `parked()` is already false there,
        // and the trace shows the grabs continuing right through it — the fresh worker has not
        // presented, so there is still nothing behind the window.
        assert!(!speaks_for_a_guest(ParkPhase::Resuming));
    }

    #[test]
    fn the_resuming_overlay_comes_down_on_the_fresh_workers_first_frame() {
        // Rig 2026-08-22: the arrangement came back but "Resuming…" stayed up. The dismissal
        // was `frames > <the count at the play click>` on the premise that the counter survives
        // the worker swap — which stopped being true when the swap started clearing the slots,
        // so the fresh worker had to out-present the whole previous session before its own
        // first frame counted. The epoch is what actually separates the two workers.
        //
        // args: (worker_epoch, epoch_at_click, frames)
        assert!(
            resume_first_frame(4, 3, 1),
            "a frame from a worker swapped in after the click IS the first fresh frame"
        );
        assert!(
            !resume_first_frame(4, 3, 0),
            "the fresh worker has not presented yet"
        );
        // Pre-swap: the counter still belongs to the worker that was suspended, however
        // large it is — nothing about it says the resume has produced a pixel.
        assert!(!resume_first_frame(3, 3, 9_000));
    }

    #[test]
    fn a_resume_workers_death_is_detected_without_racing_the_swap() {
        // The dogfood 2026-08-10 hang: the resume worker SIGSEGVed during the venus journal
        // replay, and the window sat on "Resuming…" forever — the dead-worker exit branch is
        // gated on ParkPhase::Live. The detection must not fire in the pre-swap window,
        // where the OLD worker's exited flag is still set but the fresh worker is fine.
        //
        // args: (exited, resume_dead, worker_epoch, epoch_at_click)
        assert!(
            !resume_worker_died(true, false, 3, 3),
            "the old worker's stale exited flag (no swap yet) must not read as a death"
        );
        assert!(
            !resume_worker_died(false, false, 4, 3),
            "a swapped-in, running fresh worker is not a death"
        );
        assert!(
            resume_worker_died(true, false, 4, 3),
            "exited set again AFTER the swap = the fresh worker died"
        );
        assert!(
            resume_worker_died(false, true, 3, 3),
            "a respawn that never reached a swap (spawn/gateway failure) is a death"
        );
    }

    #[test]
    fn the_strips_capture_cursor_hangs_off_the_strips_own_scanout() {
        // The band is a different WINDOW, so a cursor composited only into the carrier's layer is
        // clipped out of existence there — the pointer vanished on entering the housing band while
        // still driving the guest (2026-08-08). What makes the copy correct without any geometry of
        // its own is that it hangs off the strip's scanout layer, exactly as the carrier's hangs off
        // the carrier's: same bounds, same computed frame, each window clipping its own share. A
        // cursor layer created detached would draw at the wrong place, or not at all.
        let overlay = ExtendOverlay::default();
        let cursor = overlay.strip_cursor_layer();
        let scanout = overlay.strip_layer();
        assert!(
            cursor
                .superlayer()
                .is_some_and(|l| std::ptr::eq(&*l, &*scanout)),
            "the strip's cursor layer must be a sublayer of the strip's scanout layer"
        );
        assert!(
            cursor.isHidden(),
            "it must not draw before capture positions it"
        );
    }

    /// `yield_step` for a condition that has already held long enough to act on.
    fn yields_after(active: bool, focus_here: bool, on_space: bool) -> bool {
        let t0 = std::time::Instant::now();
        let (since, _) = yield_step(active, focus_here, on_space, None, t0);
        yield_step(active, focus_here, on_space, since, t0 + OVERLAY_SETTLE).1
    }

    #[test]
    fn a_window_focused_on_another_display_does_not_push_the_overlay_under_the_notch_backdrop() {
        // The dogfood symptom: click something on the external display and the guest's
        // top strip goes black on the internal one, hiding its panel, while the rest stays
        // full-panel. The overlay was still up — it had merely dropped below the fullscreen
        // Space's camera-housing backdrop, which the system draws at menu-bar level.
        assert!(!yields_after(false, false, true));
    }

    #[test]
    fn a_stale_focus_reading_never_drops_the_overlay() {
        // `mainScreen` lags `isActive`, so every deactivation briefly claims the focus is still
        // here. Traced: 8 of these in one switching session. Held for a tick or two they
        // are invisible; held longer they are the black strip that survived the first fix.
        let t0 = std::time::Instant::now();
        let (since, now) = yield_step(false, true, true, None, t0);
        assert!(!now, "the first tick of a condition is never enough");
        assert!(
            !yield_step(false, true, true, since, t0 + Duration::from_millis(16)).1,
            "one tick of 'focus is here' is a stale sample, not a dialog"
        );
    }

    #[test]
    fn focus_settling_on_our_own_screen_still_yields_to_system_windows() {
        // The reason yielding exists at all: the Accessibility prompt opens on our screen and
        // limina resigns active, and an overlay above menu-bar level covers the very dialog that
        // grants limina its capture tap.
        assert!(yields_after(false, true, true));
    }

    /// Switching to another Space on our own display looks exactly like a dialog taking focus
    /// here — limina resigns active and `mainScreen` is still ours — but there is nothing of ours
    /// on screen to yield *to*, and yielding is visible in a way it never is for a dialog.
    #[test]
    fn a_space_that_is_not_showing_has_nothing_to_yield_to() {
        assert!(!yields_after(false, true, false));
        // And the dialog case is unchanged — that one really is on screen, over us.
        assert!(yields_after(false, true, true));
    }

    /// The timer must measure the **whole** condition, not just "inactive and the focus is here".
    ///
    /// Keyed on the narrower pair, the clock runs for the entire time the user is away on another
    /// Space, so it is already expired when they come back: `on_active_space` turns true in the
    /// frame after the animation, the yield fires immediately, and the guest shows inset below the
    /// housing until limina finishes activating. That is the reported snap, third variation —
    /// each fix moved it rather than removing it, because the clock was measuring the wrong thing.
    #[test]
    fn returning_from_another_space_restarts_the_yield_clock() {
        let t0 = std::time::Instant::now();

        // Away: our Space is not showing, so the condition does not hold, however long it lasts.
        let (since, yields) = yield_step(false, true, false, None, t0);
        assert_eq!(since, None);
        assert!(!yields);
        let away = t0 + Duration::from_secs(30);
        let (since, yields) = yield_step(false, true, false, since, away);
        assert_eq!(since, None, "30 s away must not bank credit toward a yield");
        assert!(!yields);

        // The switch resolves: still inactive for a moment, but the clock starts NOW.
        let (since, yields) = yield_step(false, true, true, since, away);
        assert_eq!(since, Some(away));
        assert!(!yields, "the frame the Space returns is never a yield");

        // limina activates within the settle window, as it does — so the overlay never moves.
        let (_, yields) = yield_step(true, true, true, since, away + Duration::from_millis(100));
        assert!(!yields);

        // A dialog that really is holding focus still wins once it has held long enough.
        let (_, yields) = yield_step(false, true, true, since, away + OVERLAY_SETTLE);
        assert!(yields);
    }

    /// A real dogfood display arrangement, read out of its `state.toml`: a built-in
    /// Retina panel as the main screen at the origin, and the 60 Hz external the VM was
    /// fullscreen on off to the right. `EXTERNAL` is the real saved `fullscreen_display` key;
    /// `INTERNAL` is a second, different key.
    const INTERNAL: u64 = 0x31d7_dd41_a04e_0078;
    const EXTERNAL: u64 = 3_964_565_773_887_406_140;
    const INTERNAL_FRAME: [f64; 4] = [0.0, 0.0, 1512.0, 982.0];
    const EXTERNAL_FRAME: [f64; 4] = [3840.0, 919.0, 2048.0, 1286.0];

    /// The midpoint of a frame, for asserting WHICH panel it landed on.
    fn lands_on(frame: [f64; 4], screen: [f64; 4]) -> bool {
        let (mx, my) = (frame[0] + frame[2] / 2.0, frame[1] + frame[3] / 2.0);
        mx >= screen[0]
            && mx < screen[0] + screen[2]
            && my >= screen[1]
            && my < screen[1] + screen[3]
    }

    #[test]
    fn a_rearranged_display_still_restores_fullscreen_on_the_remembered_panel() {
        // The dogfood symptom: "it's always putting the window on the internal
        // screen". The saved frame is absolute Cocoa coordinates from the arrangement it was
        // written in; move the external panel in System Settings (or let it come back at another
        // origin) and that rectangle lands on no screen at all. The old rule then centered on the
        // main display — the built-in — and fullscreened there, throwing away a `fullscreen_display`
        // key that still matches an attached panel perfectly well.
        let moved_external = [1512.0, 0.0, 2048.0, 1286.0];
        let screens = [(INTERNAL, INTERNAL_FRAME), (EXTERNAL, moved_external)];
        let placed = restore_placement(Some(EXTERNAL_FRAME), Some(EXTERNAL), &screens)
            .expect("a matching panel is attached — never fall back to centering on main");
        assert!(
            lands_on(placed, moved_external),
            "restored onto {placed:?}, which is not the remembered panel {moved_external:?}"
        );
    }

    #[test]
    fn the_remembered_panel_outranks_a_windowed_frame_on_another_screen() {
        // The frame is the last *windowed* placement and may predate the move to the display the
        // VM was fullscreen on. `screen_info_for_restore` already sizes the guest for the
        // identity, so placing the window by the frame fullscreens the wrong panel at a
        // resolution meant for the other one.
        let screens = [(INTERNAL, INTERNAL_FRAME), (EXTERNAL, EXTERNAL_FRAME)];
        let windowed_on_internal = [100.0, 100.0, 800.0, 600.0];
        let placed = restore_placement(Some(windowed_on_internal), Some(EXTERNAL), &screens)
            .expect("placed on the remembered panel");
        assert!(
            lands_on(placed, EXTERNAL_FRAME),
            "restored onto {placed:?}, expected the external panel"
        );
    }

    #[test]
    fn a_frame_already_on_the_remembered_panel_is_kept_verbatim() {
        // Nothing to fix: don't re-center a placement that is already right.
        let screens = [(INTERNAL, INTERNAL_FRAME), (EXTERNAL, EXTERNAL_FRAME)];
        let f = [3900.0, 1000.0, 800.0, 600.0];
        assert_eq!(
            restore_placement(Some(f), Some(EXTERNAL), &screens),
            Some(f)
        );
    }

    #[test]
    fn an_unplugged_panel_falls_back_to_the_frame_then_to_center() {
        // "Was fullscreen" is the stronger memory, but the panel is genuinely gone — undocking
        // must not leave the window off-screen, and it must not invent a placement.
        let screens = [(INTERNAL, INTERNAL_FRAME)];
        let on_internal = [10.0, 10.0, 800.0, 600.0];
        assert_eq!(
            restore_placement(Some(on_internal), Some(EXTERNAL), &screens),
            Some(on_internal),
            "a still-valid frame survives the missing panel"
        );
        assert_eq!(
            restore_placement(Some(EXTERNAL_FRAME), Some(EXTERNAL), &screens),
            None,
            "a frame on no screen with no panel to fall back to centers on main"
        );
        assert_eq!(
            restore_placement(None, Some(EXTERNAL), &screens),
            None,
            "nothing remembered at all"
        );
    }

    #[test]
    fn a_windowed_vm_is_placed_by_its_frame_alone() {
        // No fullscreen memory (the common record): unchanged behavior.
        let screens = [(INTERNAL, INTERNAL_FRAME), (EXTERNAL, EXTERNAL_FRAME)];
        let f = [3900.0, 1000.0, 800.0, 600.0];
        assert_eq!(restore_placement(Some(f), None, &screens), Some(f));
        assert_eq!(
            restore_placement(Some([-9000.0, 0.0, 800.0, 600.0]), None, &screens),
            None
        );
    }

    #[test]
    fn a_window_larger_than_the_remembered_panel_is_clamped_onto_it() {
        // The saved frame can be bigger than the panel it is being moved to (a fullscreen record
        // carries the *screen-sized* frame when it was written before any windowed save). Centering
        // it unclamped would put the title bar off the top of the target.
        let small = [0.0, 0.0, 1024.0, 768.0];
        let screens = [(INTERNAL, INTERNAL_FRAME), (EXTERNAL, small)];
        let placed = restore_placement(Some(EXTERNAL_FRAME), Some(EXTERNAL), &screens)
            .expect("placed on the remembered panel");
        assert_eq!(placed, small, "clamped to the panel it is restoring onto");
    }

    #[test]
    fn our_own_windows_never_push_the_overlay_down() {
        // While limina is active the overlay is always on top, wherever the focus sits — our own
        // sheets and dialogs are children of the carrier and order above it regardless.
        for focus_here in [true, false] {
            assert!(!yields_after(true, focus_here, true));
        }
    }
}
