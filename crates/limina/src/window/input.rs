// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Translate captured `NSEvent`s into Linux evdev events and ship them to the worker.
//!
//! Runs entirely on the main thread (from the window's local event monitor). Keyboard and
//! pointer go to separate sockets matching the worker's two virtio-input devices. Each
//! logical change is committed with a trailing `EV_SYN`/`SYN_REPORT`, exactly as a real
//! evdev device would. The pointer is absolute: window-local cursor coordinates are scaled
//! into the device's `0..=ABS_MAX` range, so the guest cursor tracks the macOS cursor 1:1.
//!
//! Pointer events are gated to the guest view: AppKit keeps delivering `MouseMoved` to the
//! key window while the cursor is *outside* it (with `locationInWindow` in screen
//! coordinates when no window is associated), so without the gate the guest pointer
//! wanders while the host pointer isn't even over the VM. Motion/press/scroll require the
//! point to be inside the content view; drags and releases follow a press that happened
//! inside (macOS capture semantics), with coordinates clamped to the view.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::WorkerConn;

use objc2::rc::Retained;
use objc2::Message;
use objc2_app_kit::{NSCursor, NSEvent, NSEventType, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect};

use limina_input::constants::{
    ABS_MAX, ABS_X, ABS_Y, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_ABS, EV_KEY, EV_REL, REL_HWHEEL,
    REL_HWHEEL_HI_RES, REL_WHEEL, REL_WHEEL_HI_RES, REL_X, REL_Y,
};
use limina_input::keymap::{
    capslock_on, macos_keycode_to_linux_remapped, modifier_emit, reconcile_modifiers, CapsLockSync,
    KeyRemap, ModEmit, MACOS_KC_CAPSLOCK, MODIFIER_KEYCODES,
};
use limina_input::InputEvent;

// CoreGraphics (already linked): display geometry only. The pointer-capture / mouselook
// primitives (`CGWarpMouseCursorPosition`, `CGAssociateMouseAndMouseCursorPosition`) are
// private to [`super::warp`] — every warp goes through the broker, which is what makes its
// obligation bundle (landing asserts, blank re-assert, suppression bookkeeping) unforgettable.
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> NSRect;
}

/// Centre of the main display in global (points, top-left origin) coordinates — the fixed point
/// we keep the host cursor pinned to while captured.
fn main_display_center() -> NSPoint {
    // SAFETY: plain CoreGraphics queries, no preconditions.
    let b = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    NSPoint::new(
        b.origin.x + b.size.width / 2.0,
        b.origin.y + b.size.height / 2.0,
    )
}

/// Map a view point (bottom-left origin, view coordinates) to CG *global* coordinates
/// (top-left origin of the primary display) — the space `CGWarpMouseCursorPosition` speaks.
/// `None` when the view isn't in a window. NS global coordinates share the primary display's
/// origin with CG but grow upward, so the flip goes through the primary display height.
pub(crate) fn view_point_to_cg_global(view: &NSView, p: (f64, f64)) -> Option<NSPoint> {
    let window = view.window()?;
    let base = view.convertPoint_toView(NSPoint::new(p.0, p.1), None);
    let scr = window.convertPointToScreen(base);
    let h = unsafe { CGDisplayBounds(CGMainDisplayID()) }.size.height;
    Some(NSPoint::new(scr.x, h - scr.y))
}

/// The inverse of [`view_point_to_cg_global`]: a CG *global* point (top-left origin) as a view
/// point (bottom-left origin, view coordinates).
///
/// Unlike the forward direction this is routinely asked about points **outside** the view — a
/// cursor that has crossed onto another display — and answers with coordinates outside the
/// view's bounds, which is exactly what the caller needs to tell "off the guest" from "at its
/// edge". `None` when the view isn't in a window.
pub(crate) fn cg_global_to_view_point(view: &NSView, p: NSPoint) -> Option<(f64, f64)> {
    let window = view.window()?;
    let h = unsafe { CGDisplayBounds(CGMainDisplayID()) }.size.height;
    let base = window.convertPointFromScreen(NSPoint::new(p.x, h - p.y));
    let local = view.convertPoint_fromView(base, None);
    Some((local.x, local.y))
}

/// Where the pointer is **right now**, in view coordinates — asked of the window server rather
/// than remembered from an event. `None` when the view isn't in a window.
///
/// [`NSEvent::mouseLocation`] is a query, not an event, so it is current even when no motion event
/// reached us: while the pointer is over another display our window gets no usable motion (the
/// gate in [`InputState::emit_motion`]'s caller drops points outside the content), and the tap runs
/// *ahead* of the window's monitor, so at the instant a grab is decided our remembered position can
/// be a whole event stale — a long way at speed. See [`InputState::toggle_capture`].
fn live_pointer_in_view(view: &NSView) -> Option<(f64, f64)> {
    let window = view.window()?;
    // `mouseLocation` is NS *screen* space (bottom-left origin), which is what
    // `convertPointFromScreen` decodes — no CG flip here, unlike `cg_global_to_view_point`.
    let base = window.convertPointFromScreen(NSEvent::mouseLocation());
    let local = view.convertPoint_fromView(base, None);
    Some((local.x, local.y))
}

/// The event's cursor position in `view` coordinates.
///
/// `locationInWindow` is relative to **the window the event was delivered to**, while
/// `convertPoint_fromView(_, None)` decodes a point in **the view's current window**. Those are
/// normally the same window and the distinction never shows — until `notch = extend` re-parents
/// the guest view into the overlay, after which events still delivered to the carrier are decoded
/// in the wrong space. Measured: one event reported at `y = 982` by the tap came through here as
/// `y = 65`, same `x`, same delta, 6 ms apart. Going via screen coordinates when the windows
/// differ costs two conversions and removes the whole class.
pub(super) fn event_point_in_view(event: &NSEvent, view: &NSView) -> NSPoint {
    let loc = event.locationInWindow();
    let ev_win = objc2::MainThreadMarker::new().and_then(|mtm| event.window(mtm));
    let base = match (ev_win, view.window()) {
        (Some(ev_win), Some(view_win)) if !std::ptr::eq(&*ev_win, &*view_win) => {
            view_win.convertPointFromScreen(ev_win.convertPointToScreen(loc))
        }
        // `None` source window = the point is already in the view's window base coordinates.
        _ => loc,
    };
    view.convertPoint_fromView(base, None)
}

/// Whether this event was delivered to a window OTHER than the guest view's — under
/// `notch = extend` that means the housing strip, which is its own borderless window sitting over
/// the top of the same picture. Diagnostic only: the pointer's shape is app-wide, but AppKit's
/// own cursor management is per-window, so "which window is under the pointer" is a candidate
/// explanation whenever the shape misbehaves in the band and nowhere else.
fn event_off_view_window(event: &NSEvent, view: &NSView) -> bool {
    let ev_win = objc2::MainThreadMarker::new().and_then(|mtm| event.window(mtm));
    match (ev_win, view.window()) {
        (Some(ev), Some(vw)) => !std::ptr::eq(&*ev, &*vw),
        _ => false,
    }
}

/// A host-intercepted shortcut: recognized BEFORE the key reaches the guest, so the combo
/// drives limina itself. The seed of the configurable-keybinding system (M8); for now just
/// the fullscreen toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostShortcut {
    ToggleFullScreen,
    /// Toggle pointer capture (relative/mouselook mode): grab the host cursor and feed the
    /// guest relative motion, or release it.
    ToggleCapture,
}

/// Recognize a host shortcut from a key-down's macOS keycode + raw `modifierFlags` bitmask,
/// or `None` if the key should go to the guest. Uses the **device-independent class** flag
/// bits, so it's independent of left/right *and* of `--swap-cmd-opt` (the swap changes only
/// which evdev code we emit to the guest, never the macOS modifier state read here). All
/// shortcuts require EXACTLY Command+Control (no Option/Shift) so richer combos still reach
/// the guest.
pub fn match_host_shortcut(keycode: u16, flags: u64) -> Option<HostShortcut> {
    const SHIFT: u64 = 1 << 17;
    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    const KC_F: u16 = 0x03; // Cmd-Ctrl-F — macOS-standard Enter/Exit-Full-Screen
    const KC_G: u16 = 0x05; // Cmd-Ctrl-G — grab/release the pointer (capture mode)
    let held = |m: u64| flags & m != 0;
    if !(held(COMMAND) && held(CONTROL)) || held(OPTION) || held(SHIFT) {
        return None;
    }
    match keycode {
        KC_F => Some(HostShortcut::ToggleFullScreen),
        KC_G => Some(HostShortcut::ToggleCapture),
        _ => None,
    }
}

/// One step of the modifier-only ungrab chord (Control+Option, VMware-style): given whether
/// the chord is currently armed and the new `flagsChanged` bitmask, returns `(armed, fire)`.
/// Arms when EXACTLY Control+Option are held (no Command/Shift); fires when the chord then
/// breaks cleanly (either key lifts, nothing else joined). Any other modifier joining
/// disarms, and the caller disarms on any key/button/scroll — so guest combos that *start*
/// with Ctrl+Alt (Ctrl-Alt-T, Ctrl-Alt-arrows) still reach the guest while captured: they
/// press another key mid-chord, which cancels the ungrab.
pub(crate) fn ungrab_chord_step(armed: bool, flags: u64) -> (bool, bool) {
    const SHIFT: u64 = 1 << 17;
    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    let held = |m: u64| flags & m != 0;
    let (ctrl, opt, cmd, shift) = (held(CONTROL), held(OPTION), held(COMMAND), held(SHIFT));
    if ctrl && opt && !cmd && !shift {
        return (true, false);
    }
    // Named parts (not one expression) — clippy's "minimal" form is unreadable here.
    let chord_broke = !(ctrl && opt);
    let nothing_else_held = !cmd && !shift;
    (false, armed && chord_broke && nothing_else_held)
}

/// What a `flagsChanged` edge means to the ungrab chord — see [`ungrab_chord_action`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UngrabAction {
    /// The chord broke cleanly: drop the grab (or mute the soft one) and consume the event.
    Fire,
    /// The chord is ARMED, so this edge is ambiguous — it may be the start of an ungrab or of a
    /// guest combo. Withhold it from the guest (consume the event); it is replayed verbatim if
    /// the chord is later cancelled.
    Withhold,
    /// Not chord business: replay anything withheld, then forward this edge to the guest.
    Forward,
}

/// The chord step plus what to do with the edge that caused it. Withholding the *arming* edge is
/// what keeps the ungrab gesture out of the guest: without it, the Option press that arms the
/// chord is forwarded before we can know it was an ungrab, so a guest that has no Control held
/// (the usual case — Control was already down before our window took focus, so the guest never
/// saw its press) receives a LONE Alt press, which apps read as "focus the menu bar". Holding the
/// edge until the chord resolves costs nothing: if it resolves into a guest combo instead
/// (Ctrl-Alt-T), [`InputState::cancel_ungrab_chord`] replays it before the key that cancelled it.
pub(crate) fn ungrab_chord_action(armed_before: bool, flags: u64) -> (bool, UngrabAction) {
    let (armed, fire) = ungrab_chord_step(armed_before, flags);
    let action = match (fire, armed) {
        (true, _) => UngrabAction::Fire,
        (false, true) => UngrabAction::Withhold,
        (false, false) => UngrabAction::Forward,
    };
    (armed, action)
}

/// Short names for the modifier keycodes, index-aligned with [`MODIFIER_KEYCODES`] — trace only.
const MODIFIER_NAMES: [&str; 8] = [
    "lcmd", "rcmd", "lshift", "rshift", "lopt", "ropt", "lctrl", "rctrl",
];

/// Trace-friendly name for a macOS modifier keycode (`"other"` for anything else).
fn mod_name(kc: u16) -> &'static str {
    match MODIFIER_KEYCODES.iter().position(|&m| m == kc) {
        Some(i) => MODIFIER_NAMES[i],
        None if kc == MACOS_KC_CAPSLOCK => "caps",
        None => "other",
    }
}

/// Whether to log every keyboard/modifier decision to stderr (`LIMINA_INPUT_TRACE=1`).
///
/// The oracle for "the guest saw the wrong modifier state". The load-bearing question it answers
/// is whether the *host* bitmask and our *believed* pressed-set agree: `flagsChanged` tells us
/// which key changed, never the whole picture, so a modifier that goes down while we aren't
/// looking (another Space, another app) is invisible to us until it moves again — and macOS sends
/// no reconciling edge on refocus. Every line therefore prints both sides plus the drift between
/// them, so a repro shows the divergence rather than requiring it to be inferred.
pub(crate) fn input_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIMINA_INPUT_TRACE").is_some_and(|v| v != "0"))
}

/// The host pointer's adoption of the guest cursor (main thread only). `cursor` is what
/// the macOS pointer should look like over the guest view (the guest's current cursor
/// image, or a blank cursor while the guest hides it); `inside` tracks whether the pointer
/// is currently over the view so shape updates from the worker apply immediately.
pub struct HostCursor {
    cursor: RefCell<Retained<NSCursor>>,
    inside: Cell<bool>,
    /// Whether `cursor` is the transparent shape. `NSCursor` exposes no way to ask, and the
    /// difference between "wearing nothing" and "wearing the guest's arrow" is the whole
    /// question when the pointer goes missing — so it is carried, not derived.
    blank: Cell<bool>,
    /// Whether the last [`Self::verify_captured`] found the wear stripped, so the report is one
    /// line per episode rather than one per tick.
    stripped: Cell<bool>,
    /// The same, for [`Self::verify_free`] — the uncaptured pointer's own wear.
    stripped_free: Cell<bool>,
    /// The pure wear decision — see [`WearState`]. `HostCursor` is its objc shell.
    wear: RefCell<WearState>,
    traced: Cell<Option<(bool, bool, bool)>>,
}

/// What the host pointer is told to put on. The objc shell executes it; the decision is
/// pure so the ghost-cursor rules are testable headless (AppKit refuses `NSCursor`
/// construction without a run loop).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wear {
    /// The transparent 1×1 — "show nothing", even if the cursor is somehow unhidden.
    Blank,
    /// The stored guest shape.
    Stored,
    /// The system arrow (leaving the view).
    Arrow,
}

/// The wear decision as a pure state machine. Each event returns what — if anything — to put
/// on the pointer now.
///
/// The rule that earns the indirection: **while captured the pointer wears the blank, whatever
/// the guest sends.** The hide alone is not enough — AppKit can unhide the cursor behind our
/// refcount (observed 2026-08-19: a guest display rearrangement drove our window
/// reconfiguration and the parked host cursor came back as a static "ghost" wearing the LIVE
/// guest shape — shape updates kept dressing it mid-capture — immune to guest repaints until
/// the grab was toggled). A stray unhide of a transparent cursor shows nothing. Guest shapes
/// arriving mid-capture are stored, not worn; release re-wears the latest.
#[derive(Debug)]
struct WearState {
    inside: bool,
    captured: bool,
    stored_blank: bool,
}

impl WearState {
    fn new() -> Self {
        Self {
            inside: false,
            captured: false,
            stored_blank: true,
        }
    }

    /// The stored shape, as a wear (the guest's shape, or blank when the guest hid it).
    fn stored(&self) -> Wear {
        if self.stored_blank {
            Wear::Blank
        } else {
            Wear::Stored
        }
    }

    /// A new guest shape (or blank) was adopted.
    fn on_update(&mut self, blank: bool) -> Option<Wear> {
        self.stored_blank = blank;
        (self.inside && !self.captured).then(|| self.stored())
    }

    /// Capture began or ended. Entry dresses the pointer in the blank immediately; exit
    /// leaves the wear to the caller's follow-up [`Self::on_reassert`], after the pointer
    /// is placed.
    fn on_set_captured(&mut self, on: bool) -> Option<Wear> {
        self.captured = on;
        on.then_some(Wear::Blank)
    }

    /// Re-assert the stored shape (leaving capture).
    fn on_reassert(&self) -> Option<Wear> {
        (self.inside && !self.captured).then(|| self.stored())
    }

    /// The view the pointer was over is no longer on screen — its Space was swiped away, or
    /// its window went. The pointer is over macOS now, whatever it was over before.
    ///
    /// This exists because `inside` is only ever moved by [`Self::on_motion`], and a Space
    /// switch produces no motion: nothing tells this machine the pointer left. So the wear it
    /// had stayed on — the transparent blank of a capture, or the guest's own shape when the
    /// guest was hiding its cursor — and the pointer was invisible on the Space the user had
    /// just switched to, for as long as it took some other app to set a cursor of its own.
    /// The arrow is unconditional here for the same reason: there is no state in which
    /// wearing the guest's cursor over another Space is right.
    fn on_view_gone(&mut self) -> Option<Wear> {
        self.inside = false;
        Some(Wear::Arrow)
    }

    /// The pointer moved, inside or outside the view.
    fn on_motion(&mut self, inside: bool) -> Option<Wear> {
        let was = self.inside;
        self.inside = inside;
        if inside {
            Some(if self.captured {
                Wear::Blank
            } else {
                self.stored()
            })
        } else if was {
            Some(Wear::Arrow)
        } else {
            None
        }
    }
}

impl HostCursor {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            // Default to a BLANK cursor over the view, not the arrow. Many guests (e.g. mutter on
            // our virtio-gpu) software-render their cursor into the framebuffer and never send a
            // hardware cursor; showing the macOS arrow on top then gives TWO cursors. Staying blank
            // until/unless the guest sends a real hardware-cursor shape (`update`) means the guest's
            // own cursor is the only one visible. Guests that DO use a hardware cursor replace this
            // with their shape on first hover.
            cursor: RefCell::new(super::blank_cursor().unwrap_or_else(NSCursor::arrowCursor)),
            inside: Cell::new(false),
            blank: Cell::new(true),
            stripped: Cell::new(false),
            stripped_free: Cell::new(false),
            wear: RefCell::new(WearState::new()),
            traced: Cell::new(None),
        })
    }

    /// Re-dress the transparent blank mid-capture, unconditionally — for operations that can
    /// strip the wear behind our back (a cross-display warp; the ghost-cursor class,
    /// 2026-08-19). Idempotent while captured; callers only invoke it there.
    pub(crate) fn rewear_captured_blank(&self) {
        self.wear(Some(Wear::Blank));
    }

    /// Is the host pointer still wearing the blank? Asked every tick while captured.
    ///
    /// The wear is advisory: `[NSCursor set]` is the app's *current* cursor and AppKit resets
    /// it from its own cursor rects whenever it handles a mouse-moved — which, while the tap is
    /// consuming motion, is a reset we never get an event to answer. The result is a second,
    /// static pointer sitting on top of the guest's, and every previous instance of this class
    /// was found by eye rather than by the program. So compare the real cursor against the one
    /// instance we ever set (`cursor::blank_cursor` is memoised for exactly this), say so once
    /// per episode, and put it back. Repairing here rather than only reporting is deliberate:
    /// nothing else re-asserts while the tap owns the motion.
    pub(crate) fn verify_captured(&self) {
        let Some(blank) = super::blank_cursor() else {
            return;
        };
        let stripped = NSCursor::currentCursor() != blank;
        if stripped {
            self.wear(Some(Wear::Blank));
        }
        if self.stripped.get() == stripped {
            return;
        }
        self.stripped.set(stripped);
        if stripped {
            log::warn!(
                "pointer capture: the host pointer stopped wearing the blank — something reset it behind us; re-worn"
            );
        }
    }

    /// Every tick while NOT captured: the host pointer must still be wearing the guest's shape.
    ///
    /// The captured half of this rule has existed since the ghost-cursor fix; the free half was
    /// missing, and `on_motion` re-asserts only when an event arrives. So a pointer resting over
    /// the guest kept whatever AppKit last reset it to, and the guest's cursor was there while
    /// the hand moved and gone the moment it stopped (reported 2026-08-22). The shape is our own
    /// instance, so identity settles it — and the report names what it was found wearing,
    /// because "an arrow" (AppKit's cursor rects won) and "nothing" (a transparent shape, or a
    /// hide we never balanced) are different faults with the same appearance on screen.
    pub(crate) fn verify_free(&self) {
        // Only where the guest's shape is the right answer: inside its content, and not while
        // the stored shape is itself the transparent one.
        if !self.inside.get() || self.blank.get() {
            self.stripped_free.set(false);
            return;
        }
        let want = self.cursor.borrow().clone();
        let stripped = NSCursor::currentCursor() != want;
        if stripped {
            want.set();
        }
        if self.stripped_free.get() == stripped {
            return;
        }
        self.stripped_free.set(stripped);
        if stripped {
            let arrow = NSCursor::currentCursor() == NSCursor::arrowCursor();
            let blank = super::blank_cursor().is_some_and(|b| NSCursor::currentCursor() == b);
            log::warn!(
                "pointer: the host pointer stopped wearing the guest's cursor — re-worn (was \
                 wearing {}); if this repeats while the hand is still, the guest's cursor \
                 disappears whenever the pointer rests",
                if arrow {
                    "the system arrow"
                } else if blank {
                    "our transparent blank"
                } else {
                    "some other shape"
                }
            );
        }
    }

    /// Execute a wear decision against the real pointer.
    fn wear(&self, wear: Option<Wear>) {
        match wear {
            Some(Wear::Blank) => super::blank_cursor()
                .unwrap_or_else(NSCursor::arrowCursor)
                .set(),
            Some(Wear::Stored) => self.cursor.borrow().set(),
            Some(Wear::Arrow) => NSCursor::arrowCursor().set(),
            None => {}
        }
    }

    /// Enter/leave pointer-capture mode. On entry the pointer is dressed in the transparent
    /// blank immediately (see [`WearState`] for why the hide alone is not enough); on exit
    /// the caller follows up with [`Self::reassert`] once the pointer is placed.
    pub fn set_captured(&self, on: bool) {
        let w = self.wear.borrow_mut().on_set_captured(on);
        self.wear(w);
    }

    /// Adopt a new guest cursor shape (or blank). Takes effect now if the pointer is over
    /// the view (and not captured — then it is stored for release), otherwise on the next
    /// entry.
    pub fn update(&self, cursor: Retained<NSCursor>, blank: bool) {
        *self.cursor.borrow_mut() = cursor;
        self.blank.set(blank);
        let w = self.wear.borrow_mut().on_update(blank);
        self.wear(w);
    }

    /// Re-assert the guest cursor shape if the pointer is currently over the view (used when
    /// leaving pointer-capture mode, where we hid the cursor). Called by the warp broker's
    /// disengage bundle.
    pub(crate) fn reassert(&self) {
        let w = self.wear.borrow().on_reassert();
        self.wear(w);
    }

    /// Hand the pointer back to macOS because the view it was over has left the screen —
    /// see [`WearState::on_view_gone`].
    pub(crate) fn view_gone(&self) {
        let w = self.wear.borrow_mut().on_view_gone();
        self.wear(w);
        // BOTH `inside` flags, or the arrow does not survive the next tick: `verify_free` asks
        // this one (not the wear machine's) and re-asserts the guest's shape onto a pointer
        // that is no longer over the guest — measured 2026-08-22, the "stopped wearing the
        // guest's cursor — re-worn" warning firing in the same instant as the release.
        self.inside.set(false);
    }

    /// Track the pointer crossing the view boundary. Re-asserts the guest cursor on every
    /// inside motion (AppKit can reset the cursor behind our back — resize edges, window
    /// transitions); restores the arrow when leaving. While captured, what is re-asserted
    /// is the blank, on the same terms.
    fn on_motion(&self, inside: bool, on_strip: bool) {
        self.trace(inside, on_strip);
        let w = self.wear.borrow_mut().on_motion(inside);
        self.wear(w);
        self.inside.set(inside);
    }

    /// `LIMINA_EDGE_TRACE`: what the host pointer is wearing, and where.
    ///
    /// "The pointer vanishes over the housing band" (2026-08-08) has three explanations that look
    /// identical on screen — we never see the motion there, we see it but set a transparent shape,
    /// or we set the right shape and something takes it away. They are separated by exactly these
    /// three bits, so they are traced together, on transitions only.
    fn trace(&self, inside: bool, on_strip: bool) {
        if !super::capture_tap::edge_trace() {
            return;
        }
        let now = (inside, on_strip, self.blank.get());
        if self.traced.get() != Some(now) {
            self.traced.set(Some(now));
            eprintln!(
                "[CURSOR] t={:.1} inside={inside} on_strip={on_strip} blank={}",
                super::capture_tap::trace_ms(),
                self.blank.get(),
            );
        }
    }
}

/// What the window server says a click at a point would hit: the window's number, and whether
/// it is one of the guest's ([`InputState::guest_is_topmost_at`]). The number is kept so a grab
/// that stands down can name what took the click instead of just refusing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TopMost {
    pub(crate) hit: isize,
    pub(crate) ours: bool,
}

/// Main-thread input translator. Tracks which modifier keys are currently held so
/// `flagsChanged` (which carries no up/down) can be turned into press/release pairs.
/// Finger travel (in view points) that scrolls as far as one wheel detent in the guest.
/// GTK apps scroll ~3 lines (~50-60 px) per detent, so ~53 makes guest content track the
/// finger roughly 1:1, matching the native macOS feel (Chromium uses the same 53 px/tick).
const SCROLL_POINTS_PER_DETENT: f64 = 53.0;

/// Per-axis scroll accumulator: converts continuous macOS scrolling deltas into evdev's
/// dual-rate wheel — hi-res "v120" events (1/120th of a detent) for every input, plus a
/// legacy detent event whenever the accumulated hi-res motion crosses ±120, exactly like
/// the kernel's HID core. `carry` keeps the sub-unit rounding residue so slow drags never
/// lose motion; `detent_acc` is the v120 progress toward the next legacy notch.
#[derive(Clone, Copy, Default)]
struct ScrollAxis {
    carry: f64,
    detent_acc: i32,
}

impl ScrollAxis {
    /// Advance the axis by one event's delta and return `(v120, detents)` to emit. Precise
    /// deltas (trackpad / Magic Mouse, in points) map through [`SCROLL_POINTS_PER_DETENT`];
    /// non-precise ones (a physical wheel, line-based units with device-dependent scaling)
    /// keep the legacy one-notch-per-event behavior, expressed in both rates.
    fn step(&mut self, delta: f64, precise: bool) -> (i32, i32) {
        if delta == 0.0 {
            return (0, 0);
        }
        let v120 = if precise {
            let exact = delta * (120.0 / SCROLL_POINTS_PER_DETENT) + self.carry;
            let rounded = exact.round();
            self.carry = exact - rounded;
            rounded as i32
        } else {
            delta.signum() as i32 * 120
        };
        self.detent_acc += v120;
        let detents = self.detent_acc / 120;
        self.detent_acc -= detents * 120;
        (v120, detents)
    }
}

pub struct InputState {
    /// The current worker's input sink fds (swapped on a reboot relaunch). Read fresh per event.
    conn: Arc<WorkerConn>,
    /// macOS keycodes of *held* modifiers believed to be down (toggled on each flagsChanged).
    pressed_mods: RefCell<HashSet<u16>>,
    /// macOS keycodes of *held* non-modifier keys we forwarded as down but not yet up. Tracked
    /// so a focus loss mid-press (Cmd-Tab) can release them — the local monitor stops delivering
    /// events the instant focus leaves, so the key-up would be lost and the key would stick down.
    pressed_keys: RefCell<HashSet<u16>>,
    /// evdev codes of *held* aux keys (media/volume — see [`InputState::tap_aux_key`]). Kept
    /// apart from `pressed_keys` because those are macOS virtual keycodes that get mapped on
    /// release; these are already-resolved evdev codes from a different namespace.
    pressed_aux: RefCell<HashSet<u16>>,
    /// Caps Lock is a *lock* key, not a held modifier: kept in sync with the host LED on every
    /// event (see [`InputState::sync_capslock`]), separate from `pressed_mods`.
    caps: RefCell<CapsLockSync>,
    /// Bitmask of mouse buttons whose *press* we forwarded to the guest. Releases and
    /// drags are forwarded only while set, so a click that started on the title bar (or
    /// outside the window) never reaches the guest in any form.
    buttons: Cell<ButtonLedger>,
    host_cursor: Rc<HostCursor>,
    /// Everything the grab policy remembers between events. It lives HERE, not in the tap's
    /// context, because the tap is event-driven and two of its owners are not: the tick's
    /// screen-gain trigger ([`Self::grab_on_screen_gain`]) has to clear the explicit-release
    /// latch at a moment when no event is arriving at all.
    grab: Cell<super::grab_policy::GrabState>,
    /// The screen the guest occupied at the last tick, for that trigger's edge.
    screen_gain: Cell<Option<super::grab_policy::ScreenGain>>,
    /// The last send whose echo has been folded into the mapping, so one send is one sample.
    sampled: Cell<Option<u64>>,
    /// The deliberate sweep in progress, if any.
    probe: Cell<Option<Probe>>,
    /// When the last sweep ended — the rest between sweeps
    /// ([`super::absfit::PROBE_COOLDOWN`]).
    probe_rested: Cell<Option<std::time::Instant>>,
    /// How many positions the *hand* has put on the device, by either path. A sweep watches
    /// this and yields the moment it changes: the pointer is the user's, and a sweep is only
    /// ever borrowing it.
    hand_sends: Cell<u64>,
    /// When the hand last did so — a sweep waits for a gap in the user's own movement
    /// ([`super::absfit::PROBE_QUIET`]) rather than interrupting a stroke.
    hand_send_at: Cell<Option<std::time::Instant>>,
    /// Keyboard remap policy (e.g. the Command/Option swap), applied to every key/modifier.
    remap: KeyRemap,
    /// Pointer-capture mode: when set, the host cursor is grabbed (frozen and hidden) and the
    /// macOS-accelerated motion deltas integrate into a *virtual* cursor position
    /// ([`InputState::capture_pos`]) that drives the same absolute tablet as uncaptured mode —
    /// so movement feels exactly like the host cursor (same acceleration, same fit mapping, and
    /// libinput never accelerates an absolute device on top). Toggled by `Cmd-Ctrl-G`. Shared
    /// with the render timer (an `Arc<AtomicBool>`), which composites the guest cursor at its
    /// reported position while this is set (the host cursor is hidden, so the guest cursor has
    /// to be drawn).
    captured: Arc<AtomicBool>,
    /// Ungrab-chord (Ctrl+Option) arm state, shared by whichever path is consuming
    /// `flagsChanged` (the tap while captured, this monitor in degraded capture) — see
    /// [`ungrab_chord_step`]. Main thread only.
    ungrab_armed: Cell<bool>,
    /// `flagsChanged` edges withheld from the guest while the chord is armed, oldest first —
    /// each the `(keycode, flags)` we would have forwarded. Replayed in order if the chord is
    /// cancelled, dropped if it fires. See [`ungrab_chord_action`].
    ungrab_withheld: RefCell<Vec<(u16, u64)>>,
    /// The virtual cursor, in the **view space of the window showing [`Self::capture_slot`]**
    /// (bottom-left origin points). One space per capture session: uncaptured motion keeps it
    /// at the pointer's last position over any guest content, with the slot of the window it
    /// was in, so a grab starts exactly where the cursor was; captured motion integrates
    /// deltas into it, clamped to that window's fit ([`InputState::captured_step_and_emit`]);
    /// a release warps the host cursor there. `None` until the pointer has ever been placed
    /// (then capture seeds at the primary content's centre). Every write sets the slot in the
    /// same breath — a point in one window's space read against another window's geometry is
    /// the fault class behind every multi-display capture bug.
    capture_pos: Cell<Option<(f64, f64)>>,
    /// The slot whose window [`Self::capture_pos`] is a point in — the window a held grab is
    /// judged against ([`super::grab_policy::capture_owner`]).
    capture_slot: Cell<usize>,
    /// The primary window's presentation core, for [`Self::primary_fit`]: the gate measures
    /// events against the LAYER's live frame, exactly as the per-slot registry does for a
    /// secondary — one geometry-sourcing rule, so the pointer can never disagree with the
    /// pixels. Same main thread → `Rc`.
    primary: Rc<super::guestwindow::GuestWindow>,
    /// Where the hidden host cursor is parked while captured, in CG global coordinates — a point
    /// inside the capture window, chosen at grab time. See [`super::fit::park_point`].
    park: Cell<Option<NSPoint>>,
    /// The warp broker — the one owner of every `CGWarpMouseCursorPosition` while capture
    /// machinery is live ([`super::warp`]).
    warp: super::warp::WarpBroker,
    /// The last position we told the guest (either path), and when — what the guest's echo
    /// is checked against once it has had time to arrive ([`Self::verify_guest_echo`]).
    sent: Cell<Option<Sent>>,
    /// When we last pushed the relative device (edge pressure). The guest's own barriers move
    /// its pointer on that, legitimately, so the echo is not judged until it has settled.
    last_push: Cell<Option<std::time::Instant>>,
    /// The `sent` sample last verified, so each is judged once.
    echo_checked: Cell<Option<std::time::Instant>>,
    /// The captured cursor's position in the device range — what the guest actually receives,
    /// continuous for the whole session (`captured_step_and_emit`). `None` until the first
    /// captured step seeds it from the fit.
    capture_range: Cell<Option<(f64, f64)>>,
    /// Which guest cursor echoes the captured estimate may still be re-based from
    /// (`follow_guest_echo`). See [`EchoGate`].
    echo_seen: Cell<EchoGate>,
    /// The slot whose window the park currently sits in: the grab's window at the engage, then
    /// the panel the guest's cursor crossed to after each re-park (`repark_if_quiescent`). The
    /// re-pin's warp expectation is this slot's displays; `capture_slot` may already be a
    /// panel ahead of it while the hand is still moving.
    park_slot: Cell<usize>,
    /// Whether the "the park is off the arrangement and cannot be re-derived" warning has
    /// already been printed for the current stretch. The re-pin runs on every motion event, so
    /// an unhealed park would otherwise emit one line per mouse move; cleared the moment a park
    /// is held or re-derived successfully.
    stale_park_reported: Cell<bool>,
    /// When captured motion last moved the cursor — the re-park waits for a pause.
    last_captured_motion: Cell<Option<std::time::Instant>>,
    /// Hi-res scroll accumulators (vertical, horizontal) — see [`ScrollAxis`].
    scroll_y: Cell<ScrollAxis>,
    scroll_x: Cell<ScrollAxis>,
    /// Whether the guest is hosted in the `notch = extend` overlay, and the flag that asks for it
    /// to stand down so the menu bar and the window's controls are reachable. See
    /// [`InputState::reveal_step`].
    overlay_active: Arc<AtomicBool>,
    reveal_chrome: Arc<AtomicBool>,
    /// Everything the chrome-reveal gesture remembers between events — the decisions live in
    /// [`super::grab_policy::reveal_step`] and friends; this file reads AppKit and performs
    /// the verdicts. Written only through [`Self::with_reveal`], which keeps the
    /// [`Self::reveal_chrome`] Arc (what the primary's overlay reconcile reads) mirroring
    /// `ask == primary`; a secondary's reconcile asks [`Self::reveal_ask_slot`] instead.
    reveal: Cell<super::grab_policy::RevealState>,
    /// Last observed `NSMenu::menuBarVisible` — the edge detector for
    /// [`Self::menubar_observed`].
    menubar_seen: Cell<bool>,
    /// The slot the primary window is showing. Events from any other window carry their own
    /// slot ([`super::windows::slot_of_window`]); this is the fallback for the primary's.
    primary_slot: Rc<Cell<u32>>,
    /// The slot the pointer is over and that window's content width, written on every motion.
    ///
    /// The host `NSCursor` wears the *guest's* cursor shape, and the guest publishes one per
    /// scanout — enabling the plane on the CRTC the pointer is on and hiding it on the others.
    /// So the shape to wear is the one belonging to the display the pointer is over, and this
    /// is how the render timer knows which that is.
    pointer_slot: Rc<Cell<(usize, f64)>>,
}

/// One captured-pointer step's outcome, resolved for the per-window policy decisions: the
/// virtual cursor in the capture window's view space, with that window's fit and view. The
/// tap judges the grab's edge press and release against `view_point`/`fit`, and converts
/// release targets to global via `view`.
pub(crate) struct CapturedStep {
    /// The slot the capture window shows — what `capture_slot` says.
    pub(crate) slot: usize,
    /// The cursor in the capture window's view space (bottom-left origin points).
    pub(crate) view_point: (f64, f64),
    /// The guest picture's rect within that view.
    pub(crate) fit: super::fit::FitRect,
    /// The capture window's content view.
    pub(crate) view: Retained<NSView>,
    /// The position the guest received, in the device range.
    pub(crate) range: (f64, f64),
}

/// The virtual cursor resolved in its window: the slot, that slot's view and fit, and the
/// cursor as a point in that view.
struct Projection {
    slot: usize,
    view: Retained<NSView>,
    fit: super::fit::FitRect,
    point: (f64, f64),
}

/// What we last told the guest about its pointer: the slot and the unit position within it.
#[derive(Clone, Copy, Debug)]
struct Sent {
    slot: usize,
    unit: (f64, f64),
    /// What went on the wire, normalised to `0.0..=1.0` — the sample the echo is paired with
    /// to learn each display's share of the device (`window/absfit.rs`). Not the same number
    /// as `unit` once a fit is live, which is the whole point of keeping both.
    device: (f64, f64),
    /// This send's number ([`super::echo::note_send`]), and where the guest's cursor was when
    /// it went out. Together they say when the guest has *answered this send* — which is what
    /// the sample waits for, instead of a fixed settle.
    seq: u64,
    before: Option<(usize, i32, i32)>,
    at: std::time::Instant,
    captured: bool,
    /// This position was the probe's, not the pointer's. It still teaches the mapping, but it
    /// is not judged: the probe deliberately sends places the pointer is not.
    probe: bool,
}

/// A deliberate sweep of the absolute device, to learn each display's share of it without
/// waiting for the user to happen to move there ([`super::absfit::PROBE_SWEEP`]).
#[derive(Clone, Copy, Debug)]
struct Probe {
    /// Index into the sweep.
    step: usize,
    /// The send this step is waiting on, and when it went out (the step's own timeout, for a
    /// guest that answers with no visible move at all).
    seq: u64,
    at: std::time::Instant,
    /// [`InputState::hand_sends`] as the sweep began, so a hand that moves mid-sweep ends it.
    /// A count, not a timestamp: the hand reaches the device by two different paths (captured
    /// motion and the uncaptured mapping) and only one of them keeps a motion time.
    hand: u64,
}

/// How long one sweep step waits for the guest before giving up on it. The answer normally
/// arrives in a frame ([`super::echo::settled`]); this only bounds the pathological case.
const PROBE_STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(120);

/// How long a position must have gone unfollowed before its echo may be sampled — longer than
/// one echo interval (~17 ms on the rig), short enough that any ordinary pause yields samples.
/// See [`InputState::sample_guest_echo`] for what this is protecting against.
const SAMPLE_SETTLE: std::time::Duration = std::time::Duration::from_millis(50);

/// How long captured motion must have paused before the park may follow the cursor across a
/// seam. Long enough that a warp never lands mid-stroke, short enough that the park has moved
/// before fingers regroup for a swipe.
const REPARK_QUIESCENCE: std::time::Duration = std::time::Duration::from_millis(150);

/// How long after the last thing we sent (a position, or a relative push) the guest's echo
/// is judged: long enough for the round trip and a frame, short enough that a wrong-display
/// pointer is caught before the next action rests on it.
const ECHO_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

/// A guest window resolved under a point: the slot it shows, the guest picture's rect within
/// its view, and the point in that view's own coordinates.
pub(crate) struct SurfaceHit {
    pub(crate) slot: usize,
    pub(crate) fit: super::fit::FitRect,
    pub(crate) point: (f64, f64),
}

/// Which guest display an event belongs to, and where the pointer is within its content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Target {
    pub(crate) slot: usize,
    /// `0.0..=1.0` within that display's content, Y already in the guest's top-left origin.
    pub(crate) unit: (f64, f64),
    /// Whether the pointer is actually over the guest's picture, as opposed to the window's
    /// chrome or the letterbox.
    pub(crate) inside: bool,
    /// The width, in view points, of the guest picture in the window this event came from.
    ///
    /// The host cursor wears the guest's cursor image scaled by (content width / guest mode
    /// width), and both halves must come from the SAME window. Taking the width from the
    /// primary while the pointer was over another display drew the sprite at the wrong size —
    /// and scaled its hotspot by the same wrong factor, which offsets where the click really
    /// lands from where the tip is drawn.
    pub(crate) content_w: f64,
    /// The event's position in the resolved window's view coordinates, and the fit it was
    /// judged against — carried so per-slot consumers (the chrome-reveal feed) reuse the one
    /// resolution instead of converting again (a gate and its consumer disagreeing about where
    /// the pointer is is this file's oldest bug class). A `fit.w == 0.0` marks an unresolved
    /// target (no layer this tick); consumers must gate on it.
    pub(crate) point: (f64, f64),
    pub(crate) fit: super::fit::FitRect,
    /// Whether this is the *primary* window's display.
    ///
    /// Load-bearing, not informational. [`InputState::emit_motion`]'s edge-overflow side
    /// effect — the push that makes the guest's pressure barriers charge — is expressed in the
    /// primary view's coordinate space, and a secondary window's pointer position has no
    /// meaning in it. Running it for an event from another window fed the guest relative
    /// motion that never happened.
    pub(crate) primary: bool,
}

impl Target {
    /// Resolve a window-local point against the rect the guest's picture actually occupies in
    /// that window.
    ///
    /// Pure, because *which rect* is the whole question. The gate and the mapping have to use
    /// the same one, and they have to use the one belonging to the window the event came from:
    /// a point on another panel converts into the primary view's space as a coordinate outside
    /// it on every possible arrangement, so gating a second display's events on the primary's
    /// fit rejects all of them.
    pub(crate) fn resolve(
        slot: usize,
        primary: bool,
        px: f64,
        py: f64,
        fit: super::fit::FitRect,
    ) -> Self {
        Target {
            slot,
            unit: super::fit::unit_through_fit(px, py, fit),
            inside: super::fit::point_in_fit(px, py, fit),
            content_w: fit.w,
            point: (px, py),
            fit,
            primary,
        }
    }
}

// `RevealSrc` and the REVEAL_* gesture constants live in the pure home now
// (`grab_policy`); the source enum is re-exported so the adapters keep their name.
pub(crate) use super::grab_policy::RevealSrc;

impl InputState {
    // Nine shared handles, each a distinct thing the translator needs for the window's lifetime;
    // bundling them into a struct would move the same list one line up.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conn: Arc<WorkerConn>,
        host_cursor: Rc<HostCursor>,
        remap: KeyRemap,
        captured: Arc<AtomicBool>,
        primary: Rc<super::guestwindow::GuestWindow>,
        overlay_active: Arc<AtomicBool>,
        reveal_chrome: Arc<AtomicBool>,
        primary_slot: Rc<Cell<u32>>,
        pointer_slot: Rc<Cell<(usize, f64)>>,
    ) -> Self {
        Self {
            conn,
            grab: Cell::new(super::grab_policy::GrabState::default()),
            screen_gain: Cell::new(None),
            sampled: Cell::new(None),
            probe: Cell::new(None),
            probe_rested: Cell::new(None),
            hand_sends: Cell::new(0),
            hand_send_at: Cell::new(None),
            pressed_mods: RefCell::new(HashSet::new()),
            pressed_keys: RefCell::new(HashSet::new()),
            pressed_aux: RefCell::new(HashSet::new()),
            caps: RefCell::new(CapsLockSync::new()),
            buttons: Cell::new(ButtonLedger::default()),
            primary_slot,
            pointer_slot,
            host_cursor,
            remap,
            captured,
            ungrab_armed: Cell::new(false),
            ungrab_withheld: RefCell::new(Vec::new()),
            capture_pos: Cell::new(None),
            capture_slot: Cell::new(0),
            warp: super::warp::WarpBroker::new(),
            sent: Cell::new(None),
            last_push: Cell::new(None),
            echo_checked: Cell::new(None),
            capture_range: Cell::new(None),
            echo_seen: Cell::new(EchoGate::default()),
            park_slot: Cell::new(0),
            stale_park_reported: Cell::new(false),
            last_captured_motion: Cell::new(None),
            primary,
            park: Cell::new(None),
            scroll_y: Cell::new(ScrollAxis::default()),
            scroll_x: Cell::new(ScrollAxis::default()),
            overlay_active,
            reveal_chrome,
            reveal: Cell::new(super::grab_policy::RevealState::default()),
            menubar_seen: Cell::new(true),
        }
    }

    /// The primary window's fit rect, read from its LAYER at ask time
    /// ([`super::guestwindow::GuestWindow::fit`]) — never a cache of intent. The registry
    /// path reads a secondary's layer the same way; this is the primary's half of the one
    /// geometry-sourcing rule.
    pub(crate) fn primary_fit(&self) -> super::fit::FitRect {
        self.primary.fit()
    }

    /// Where the hidden cursor is parked while captured — the tap re-pins to it after every
    /// motion event, so both paths agree on one point and neither warp moves anything.
    ///
    /// While captured the park is ALWAYS set — it was derived from the capture window on the
    /// grab — so a missing one is a broken premise, and the old silent fallback to the main
    /// display's centre is exactly the "host cursor on the other screen" fault. It crashes
    /// instead.
    pub(crate) fn park_point(&self) -> NSPoint {
        match self.park.get() {
            Some(p) => p,
            None => {
                assert!(
                    !self.is_captured(),
                    "pointer capture: captured on slot {} with NO park point — the capture window had no screen position when the park was derived; arrangement: {}",
                    self.capture_slot.get(),
                    super::hostdisplay::describe_arrangement(),
                );
                main_display_center()
            }
        }
    }

    /// The tap's per-event re-pin, through the broker: hold the hidden cursor at the park —
    /// zero-length, injects nothing ([`super::warp::WarpBroker::repin`]). Judged against the
    /// capture window, where the park was derived.
    ///
    /// The park is cached and the arrangement under it is not, so this first asks whether the
    /// point it holds still exists ([`super::warp::repin_verdict`]). Unplugging a display
    /// deletes the coordinates a park on it names, and this path — the tap — runs ahead of
    /// every screen-parameter notification and of the render tick's own hand-back, so on the
    /// first motion event after an unplug it is the only code in a position to notice. It used
    /// to warp anyway and die on `warp_checked`'s first assert (dogfood, 2026-08-23).
    pub(crate) fn repin_park(&self, primary_view: &NSView) {
        let aim = self.slot_aim("repin", self.park_slot.get(), primary_view);
        let park = self.park_point();
        match super::warp::repin_verdict(&super::hostdisplay::displays_at(park), &aim.displays) {
            super::warp::Repin::Hold => {
                self.stale_park_reported.set(false);
                self.warp.repin(park, &aim);
            }
            super::warp::Repin::Rederive => {
                if let Err(why) = self.rederive_park(primary_view, "repin") {
                    // Nowhere to hold the cursor this event. Leaving it where the hardware has
                    // it is the safe move: the pointer is already decoupled and hidden, and the
                    // render tick's `must_drop_grab` hands the grab back on the same
                    // reconfigure — one tick later, without a warp to a dead point.
                    self.report_stale_park_once(why);
                }
            }
        }
    }

    /// Move the park to where the capture window is **now**, and warp the hidden cursor with it.
    /// Shared by the re-pin's repair path and the quiescent cross-panel re-park, which differ
    /// only in what makes them run.
    ///
    /// The re-derived point gets the *same* test the old one just failed. A reconfigure is not
    /// atomic: the window may not have been re-placed yet, so the conversion can hand back a
    /// point that is also on no display — warping there would crash inside the fix for the
    /// crash. `Err` means there is nothing to hold the cursor at this event; the caller decides
    /// how loud that is.
    ///
    /// The repair path breaks the quiescence rule [`Self::repark_if_quiescent`] keeps — this
    /// warp can land mid-stroke, and its vector arrives as guest motion unless the injection
    /// detector recognizes it. That is the accepted cost: the park has to move, because the
    /// place it names no longer exists, and the alternative on this path is the crash.
    fn rederive_park(
        &self,
        primary_view: &NSView,
        stage: &'static str,
    ) -> Result<(), &'static str> {
        let pr = self
            .capture_projection(primary_view)
            .ok_or("the capture slot has no window this tick")?;
        let park = super::fit::park_point(Some(pr.point), pr.fit);
        let new =
            view_point_to_cg_global(&pr.view, park).ok_or("the capture view is not in a window")?;
        let displays = pr
            .view
            .window()
            .map(|w| super::hostdisplay::displays_under_window(&w))
            .unwrap_or_default();
        if super::warp::repin_verdict(&super::hostdisplay::displays_at(new), &displays)
            == super::warp::Repin::Rederive
        {
            return Err("the capture window has not been re-placed on a live display yet");
        }
        let aim = super::warp::Aim {
            stage,
            slot: pr.slot,
            displays,
        };
        self.park_slot.set(pr.slot);
        let old = self.park.get();
        self.park.set(Some(new));
        self.stale_park_reported.set(false);
        if let Some(w) = self.warp.repark(new, old, &aim, &self.host_cursor) {
            log::info!(
                "pointer capture: re-parked into slot {} (warp {:.0},{:.0})",
                pr.slot,
                w.0,
                w.1,
            );
        }
        Ok(())
    }

    /// The park is off the arrangement and could not be re-derived — once per stretch, not once
    /// per motion event.
    fn report_stale_park_once(&self, why: &str) {
        if !self.stale_park_reported.replace(true) {
            log::warn!(
                "pointer capture: the park is off the arrangement and cannot be re-derived \
                 ({why}) — holding the cursor still until the grab is handed back; \
                 arrangement: {}",
                super::hostdisplay::describe_arrangement(),
            );
        }
    }

    /// The warp expectation for a target meant to sit in `slot`'s window
    /// ([`super::warp::Aim`]): the host displays that window covers. Empty when the slot has
    /// no window this tick — the broker then only requires a real display.
    fn slot_aim(
        &self,
        stage: &'static str,
        slot: usize,
        primary_view: &NSView,
    ) -> super::warp::Aim {
        let displays = self
            .slot_surface(slot, primary_view)
            .and_then(|(view, _)| view.window())
            .map(|w| super::hostdisplay::displays_under_window(&w))
            .unwrap_or_default();
        super::warp::Aim {
            stage,
            slot,
            displays,
        }
    }

    /// `LIMINA_WARP_PROBE`'s raw measurement warp — [`super::warp::WarpBroker::probe`].
    pub(crate) fn warp_probe_to(&self, target: NSPoint) {
        self.warp.probe(target);
    }

    fn is_captured(&self) -> bool {
        self.captured.load(Ordering::Acquire)
    }

    /// Whether the pointer is captured, for the window tick's own bookkeeping.
    pub(crate) fn captured_flag(&self) -> bool {
        self.is_captured()
    }

    /// Run one decision against the grab policy's remembered state.
    pub(crate) fn with_grab<T>(
        &self,
        f: impl FnOnce(&mut super::grab_policy::GrabState) -> T,
    ) -> T {
        let mut st = self.grab.get();
        let out = f(&mut st);
        self.grab.set(st);
        out
    }

    /// The grab policy's state, for reads (traces, gating).
    pub(crate) fn grab_state(&self) -> super::grab_policy::GrabState {
        self.grab.get()
    }

    /// Take the grab when the guest gains screen — entering fullscreen, or a panel joining a
    /// session that is already fullscreen ("Use Other Screens When Fullscreen", a display
    /// plugged in). Polled from the tick, because neither transition arrives as an event we
    /// see, and the second one happens while the user is in a macOS menu.
    ///
    /// Guarded on the pointer actually being over guest content: gaining a panel while the
    /// pointer is somewhere else entirely is not an invitation to seize it.
    pub(crate) fn grab_on_screen_gain(&self, primary_view: &NSView, grab_enabled: bool) {
        let facts = self.window_facts(primary_view);
        let now = super::grab_policy::screen_gain(&facts);
        let was = self.screen_gain.replace(Some(now));
        if !super::grab_policy::gained_screen(was, now) {
            return;
        }
        // Past here the guest really did gain the screen, which happens a handful of times in
        // a session — so every way of declining says so. "It did not grab and did not explain
        // itself" is the whole difficulty of this path when a user reports it.
        let refused = if !grab_enabled {
            Some("the grab is turned off")
        } else if self.is_captured() {
            Some("the pointer is already ours")
        } else if self.live_over_guest(primary_view).is_none() {
            Some("the pointer is not over guest content")
        } else {
            None
        };
        if let Some(why) = refused {
            log::info!(
                "pointer capture: the guest gained the screen (fullscreen={}, panels {} -> {}) but the pointer stays where it is — {why}",
                now.fullscreen,
                was.map(|w| w.covering).unwrap_or(0),
                now.covering,
            );
            return;
        }
        self.with_grab(|st| st.take_by_policy());
        log::info!(
            "pointer capture: taken — the guest gained the screen (fullscreen={}, panels {} -> {})",
            now.fullscreen,
            was.map(|w| w.covering).unwrap_or(0),
            now.covering,
        );
        self.toggle_capture(primary_view);
    }

    /// The slot the captured cursor is on — the window a held grab is judged against
    /// ([`super::grab_policy::capture_owner`]). Stale while not captured.
    pub(crate) fn capture_slot(&self) -> usize {
        self.capture_slot.get()
    }

    /// Release pointer capture if held (no-op otherwise) — for the parked window (task #18),
    /// where the guest the capture served is gone and a hidden, pinned cursor would leave the
    /// user unable to click the play glyph. Main thread only.
    pub fn release_capture(&self, view: &NSView) {
        if self.is_captured() {
            self.toggle_capture(view);
        }
    }

    /// Release a grab whose window has left the screen — a Space swiped away, a window with no
    /// screen ([`super::grab_policy::must_drop_grab`]). Nothing about this release is the user
    /// asking for the pointer back at a place they can see, so it hands back
    /// [`super::warp::Handback::Gone`]: no warp, and the system arrow.
    pub fn release_capture_gone(&self, view: &NSView) {
        if self.is_captured() {
            self.toggle_capture_full(view, None, super::warp::Handback::Gone);
        }
    }

    /// Toggle pointer capture. On grab: decouple the hardware mouse from the cursor (so deltas
    /// flow while the cursor stays frozen) and hide the cursor. On release: restore both and put
    /// the host cursor where the virtual cursor ended, so the transition is seamless in both
    /// directions. Returns the new captured state. Main thread only.
    pub fn toggle_capture(&self, view: &NSView) -> bool {
        self.toggle_capture_to(view, None)
    }

    /// As [`Self::toggle_capture`], with the release warp target chosen by the caller — the
    /// grab's edge release computes its own point just past the pressed edge, in the owning
    /// window. `None` on a release projects the virtual cursor into its owning window, the
    /// seamless default. `view` is the primary window's guest view in every case.
    pub(crate) fn toggle_capture_to(
        &self,
        view: &NSView,
        release_to: Option<(NSPoint, super::warp::Aim)>,
    ) -> bool {
        self.toggle_capture_full(view, release_to, super::warp::Handback::Seamless)
    }

    /// As [`Self::toggle_capture_to`], with the handback named — see [`super::warp::Handback`].
    fn toggle_capture_full(
        &self,
        view: &NSView,
        release_to: Option<(NSPoint, super::warp::Aim)>,
        handback: super::warp::Handback,
    ) -> bool {
        let now = !self.is_captured();
        let release_to = if now {
            None
        } else {
            release_to.or_else(|| self.capture_to_global(view))
        };
        // Taking the pointer settles the chrome ask, whatever asked for it.
        //
        // The reveal exists so the pointer can reach the macOS menu bar and the window's own
        // controls; a captured pointer is hidden and pinned and can reach neither. So a held
        // grab and a granted ask are mutually exclusive — which the release direction already
        // assumes ([`Self::grant_chrome`]: the captured edge-push that frees the pointer is
        // also the ask). Without this half, enabling multi-display from the Displays menu
        // while revealed grabs the pointer and leaves the reveal standing, and nothing owns
        // undoing it: the top strip of the panel stays given away until something unrelated
        // happens to end it. This is the same reasoning as leaving fullscreen — the ask is not
        // refused, it has stopped meaning anything.
        if now {
            self.reveal_moot();
        }
        // The host displays the park's window covers — the engage warp must land on one.
        let mut engage_displays = Vec::new();
        // Where the hand is, and the fit it is in — what the guest is told once the grab is
        // live (see the send below). `None` for a grab taken with the pointer off guest
        // content, which has no honest position to hand over.
        let mut engage_send: Option<((f64, f64), super::fit::FitRect)> = None;
        if now {
            // Seed the virtual cursor from where the pointer REALLY is before deriving the park,
            // so both the guest cursor and the (zero-length) grab warp start from the truth. Our
            // remembered position lags: the capture tap decides the automatic re-grab *before* the
            // window's monitor has processed the same event, and while the pointer was on another
            // display it was not being updated at all. A stale seed shows up twice over — the guest
            // cursor appears where the pointer *was*, and the park warp covers the difference,
            // injecting it as another jump.
            //
            // The pointer may be over ANY guest window — the grab spans every covered panel —
            // so the resolution is against each of them, and the window the pointer is over
            // becomes the capture window: the cursor lives in its view space, and the park
            // lands in it. Off every guest content (a keyboard grab taken while the pointer is
            // elsewhere) the remembered position is the better answer: clamping the live one
            // would drag the guest cursor to whichever edge happens to be nearest, and the
            // warp is unavoidably long either way.
            let live = self.live_over_guest(view);
            if let Some(hit) = live.as_ref() {
                self.capture_pos.set(Some(hit.point));
                self.capture_slot.set(hit.slot);
            }
            // A fresh session: the range seeds from the first step's fit position, and the
            // estimate follows the guest's echo from the first fresh one.
            self.capture_range.set(None);
            self.echo_seen.set(EchoGate::engaged(self.echo_key_now()));
            // The park is judged in the capture window's space: the (possibly just-seeded)
            // cursor clamped into that window's fit. With nothing placed — or a remembered
            // position whose window has no surface this tick — fall back to the primary, and
            // let the cursor and the park both seed at its content centre so they agree.
            let (park_view, park_fit, seed) = match self.capture_projection(view) {
                Some(pr) => (pr.view, pr.fit, Some(pr.point)),
                None => {
                    self.capture_pos.set(None);
                    self.capture_slot.set(self.primary_slot.get() as usize);
                    (view.retain(), self.primary_fit(), None)
                }
            };
            let park = super::fit::park_point(seed, park_fit);
            self.park_slot.set(self.capture_slot.get());
            // Conservation check: a warp taken while captured IS guest motion — the window server
            // posts a motion event whose delta is the whole vector — so the only safe park warp is
            // a zero-length one. Three separate bugs on this feature were that same fault at
            // different scales (the park on another display, 1400-2400 pt; the inset exceeding the
            // re-grab margin, up to 24; the stale seed, 42x67), and each was found by a human
            // noticing the cursor move. Measure it here instead, where the geometry is supposed to
            // guarantee zero: a policy re-grab can only happen `REGRAB_MARGIN` inside the content,
            // and the inset is tied to that margin, so anything over the inset means a premise
            // broke. Cheap: one hypot per grab.
            if let Some(s) = seed {
                let warp = (park.0 - s.0).hypot(park.1 - s.1);
                if warp > super::fit::PARK_INSET {
                    log::warn!(
                        "pointer capture: park warp is {warp:.1} pt from {s:?} — that distance \
                         arrives as guest motion; the seed was outside the content or the \
                         park/re-grab coupling broke"
                    );
                } else if warp > 0.5 {
                    // Expected only for an explicit grab taken near an edge, where the pull is
                    // real and small. A policy grab must never reach here.
                    log::debug!("pointer capture: park warp {warp:.1} pt from {s:?}");
                }
            }
            self.park.set(view_point_to_cg_global(&park_view, park));
            engage_send = live
                .is_some()
                .then_some(seed)
                .flatten()
                .map(|s| (s, park_fit));
            engage_displays = park_view
                .window()
                .map(|w| super::hostdisplay::displays_under_window(&w))
                .unwrap_or_default();
        }
        self.captured.store(now, Ordering::Release);
        // The broker performs the whole transition bundle — associate, warp, hide/show, wear.
        if now {
            // The park was derived in `park_view`, so that is the window it must land in.
            let aim = super::warp::Aim {
                stage: "engage",
                slot: self.capture_slot.get(),
                displays: engage_displays,
            };
            self.warp.engage(self.park_point(), &aim, &self.host_cursor);
            // Tell the guest where the hand is, so its cursor comes to meet the pointer.
            //
            // Nothing on the wire says "a grab began", and the guest's cursor is wherever it
            // was when the pointer last left — which, after a macOS Space switch, is not where
            // the hand is any more. Without this the estimate agrees with the hand for exactly
            // one tick and the guest's cursor never moves, so the first stroke back continues
            // from the pre-switch position: the pointer appears to warp backwards (measured
            // 2026-08-22, 420 pt on the rig). Only for a grab taken with the pointer over guest
            // content (`live`) — a keyboard grab from elsewhere has no honest position to send,
            // and the centre fallback would teleport the guest's cursor for no reason.
            if let Some((s, fit)) = engage_send {
                let u = super::fit::unit_through_fit(s.0, s.1, fit);
                self.send_abs_unit(self.capture_slot.get(), u, true);
            }
        } else {
            self.warp.disengage(release_to, &self.host_cursor, handback);
        }
        self.ungrab_armed.set(false);
        // Anything the chord withheld dies here rather than being replayed: both branches below
        // force-release modifiers, so replaying a press would strand it down in the guest.
        self.ungrab_withheld.borrow_mut().clear();
        // Reconcile modifier bookkeeping across the boundary: while captured the TAP forwards
        // modifier edges, so this monitor's believed-pressed sets go stale — and stale state
        // makes `modifier_emit` swallow a later edge (a stuck or missed modifier in the guest).
        // On grab, release everything WE forwarded (the tap re-emits what's still held on its
        // next flagsChanged); on ungrab, force-release every modifier key — the ungrab chord
        // itself is always mid-press at this moment (releases of un-pressed keys are dropped
        // by the guest's input core, so over-releasing is safe).
        if now {
            self.release_all_held("grab-on");
        } else {
            self.release_all_modifiers("grab-off");
        }
        now
    }

    /// Force-release every held-modifier key in the guest and reset the believed-pressed
    /// sets. Called on capture release: whatever the tap left pressed (the ungrab chord at
    /// minimum) must not stay wedged down in the guest.
    ///
    /// Held **aux** keys (media/volume) go with them: an ungrab can change which side owns a
    /// key mid-press, and a stranded press is worse for these than for modifiers — GNOME
    /// repeats a held volume key, so a stuck one ramps the guest to max. (The tap also
    /// forwards any straggling release for a key it already pressed — see
    /// [`limina_input::auxkey::route_aux_event_key`] — so this is the belt to that's braces:
    /// it covers the case where no release ever arrives.)
    fn release_all_modifiers(&self, why: &str) {
        if input_trace() {
            let names: Vec<&str> = self
                .pressed_mods
                .borrow()
                .iter()
                .map(|&kc| mod_name(kc))
                .collect();
            eprintln!(
                "[INP] t={:.1} release_all_modifiers({why}) believed=[{}]",
                super::capture_tap::trace_ms(),
                names.join(","),
            );
        }
        for &kc in &MODIFIER_KEYCODES {
            if let Some(code) = macos_keycode_to_linux_remapped(kc, &self.remap) {
                self.send_kbd(InputEvent::new(EV_KEY, code, 0));
                self.send_kbd(InputEvent::syn());
            }
        }
        self.pressed_mods.borrow_mut().clear();
        self.release_all_aux();
    }

    /// Release every aux key we forwarded as held, and forget them.
    fn release_all_aux(&self) {
        let mut aux = self.pressed_aux.borrow_mut();
        for &code in aux.iter() {
            self.send_kbd(InputEvent::new(EV_KEY, code, 0));
            self.send_kbd(InputEvent::syn());
        }
        aux.clear();
    }

    /// Whether we've forwarded a press for this aux evdev code that hasn't been released —
    /// the tap's "a release always follows its press" check.
    pub(crate) fn is_aux_pressed(&self, code: u16) -> bool {
        self.pressed_aux.borrow().contains(&code)
    }

    /// Whether this macOS keycode has any guest equivalent at all. The tap uses it to decide
    /// between consuming a key and handing it back to macOS: a key we cannot express in the
    /// guest is not ours to swallow.
    pub(crate) fn maps_to_guest(&self, macos_keycode: u16) -> bool {
        macos_keycode_to_linux_remapped(macos_keycode, &self.remap).is_some()
    }

    /// Whether *any* aux key is held. Lets the tap skip the NSEvent bridge entirely for the
    /// common ungrabbed-and-nothing-held event.
    pub(crate) fn any_aux_pressed(&self) -> bool {
        !self.pressed_aux.borrow().is_empty()
    }

    /// Tap-side key forwarding: same bookkeeping as the local monitor (caps-lock sync,
    /// believed-pressed tracking so a focus-loss flush releases tap-forwarded keys too).
    /// The tap calls this for keyDown/keyUp it consumes (captured or soft-grab mode).
    pub(crate) fn tap_key(&self, macos_keycode: u16, down: bool, flags: u64) {
        self.trace_key("TAP-key", macos_keycode, down, flags);
        self.sync_capslock(flags);
        // A key press must arrive wearing the modifiers the user is actually holding, so heal
        // before it goes out — the caller has already cancelled (and replayed) any armed chord.
        if down {
            self.sync_modifiers(flags, None);
        }
        self.emit_key(macos_keycode, down);
    }

    /// `LIMINA_INPUT_TRACE`: a non-modifier key crossing into the guest, with the modifier
    /// picture it is wearing. The bug this exists for is a key arriving *bare* in the guest
    /// because a modifier held across a focus/Space change was never re-announced — so the
    /// interesting part of a key line is the `DRIFT` field, not the key.
    fn trace_key(&self, tag: &str, macos_keycode: u16, down: bool, flags: u64) {
        if !input_trace() {
            return;
        }
        eprintln!(
            "[INP] t={:.1} {tag} kc={macos_keycode:#04x} {}",
            super::capture_tap::trace_ms(),
            if down { "DOWN" } else { "UP" },
        );
        self.trace_mods(tag, None, flags);
    }

    /// Tap-side **aux key** forwarding (media/volume from the `NX_SYSDEFINED` class — see
    /// [`limina_input::auxkey`]). These carry no macOS virtual keycode, so they can't go
    /// through [`InputState::emit_key`]'s keycode map; the caller has already resolved the
    /// evdev code via the bucket policy. Held aux keys are tracked separately so a focus-loss
    /// flush releases them too (a media key held across a Cmd-Tab would otherwise stick down
    /// in the guest, and its key-up is never delivered to us).
    pub(crate) fn tap_aux_key(&self, code: u16, down: bool) {
        self.send_kbd(InputEvent::new(EV_KEY, code, i32::from(down)));
        self.send_kbd(InputEvent::syn());
        if down {
            self.pressed_aux.borrow_mut().insert(code);
        } else {
            self.pressed_aux.borrow_mut().remove(&code);
        }
    }

    /// Tap-side `flagsChanged` forwarding — the modifier twin of [`InputState::tap_key`].
    pub(crate) fn tap_flags(&self, macos_keycode: u16, flags: u64) {
        self.trace_mods("TAP-flags", Some(macos_keycode), flags);
        self.sync_capslock(flags);
        // Only reached on `UngrabAction::Forward` (the caller returns on Fire/Withhold), so the
        // chord's withheld edges stay withheld.
        self.sync_modifiers(flags, Some(macos_keycode));
        self.emit_modifier(macos_keycode, flags);
    }

    /// Exit the SOFT keyboard grab (Ctrl+Option while focused but not captured): flush the
    /// modifiers the chord pushed into the guest so nothing stays wedged. The caller mutes
    /// soft mode until the window regains key status.
    pub(crate) fn flush_modifiers(&self) {
        self.release_all_modifiers("soft-grab-exit");
    }

    /// Feed a `flagsChanged` to the ungrab chord while the keyboard is **not** grabbed — the
    /// state the chord itself creates when it mutes the soft grab. Same state machine as
    /// [`InputState::observe_ungrab_flags`], but nothing it withholds is ever replayed to the
    /// guest: an ungrabbed keyboard's modifier edges were never the guest's to receive, so
    /// replaying them would hand it exactly the gesture the chord is there to intercept.
    ///
    /// This exists because the chord used to go deaf the moment it fired. With the Cmd/Option
    /// swap on — the default — the guest's Super *is* macOS's Option, so "Ctrl held + Super" and
    /// the ungrab chord are one physical gesture; after the first fire muted the soft grab, every
    /// later press fell through to the local monitor and reached the guest as a bare Super.
    /// Reported 2026-08-09 as "it takes 2 Super presses"; see `spikes/modifier-drift/`.
    pub(crate) fn observe_ungrab_flags_ungrabbed(
        &self,
        macos_keycode: u16,
        flags: u64,
    ) -> UngrabAction {
        let (armed, action) = ungrab_chord_action(self.ungrab_armed.get(), flags);
        if input_trace() {
            eprintln!(
                "[INP] t={:.1} chord(ungrabbed) kc={} flags={flags:#x} armed {}->{armed} \
                 action={action:?}",
                super::capture_tap::trace_ms(),
                mod_name(macos_keycode),
                self.ungrab_armed.get(),
            );
        }
        self.ungrab_armed.set(armed);
        // Never accumulate: there is no guest-bound replay on this path, and a leftover entry
        // would be replayed by the *grabbed* path if the grab came back mid-chord.
        self.ungrab_withheld.borrow_mut().clear();
        action
    }

    /// Feed a grabbed-mode `flagsChanged` to the ungrab chord and get the verdict for that edge:
    /// [`UngrabAction::Fire`] (release/mute the grab, consume), [`UngrabAction::Withhold`]
    /// (consume without forwarding — the chord is armed and the edge is still ambiguous), or
    /// [`UngrabAction::Forward`] (forward it to the guest as usual).
    pub(crate) fn observe_ungrab_flags(&self, macos_keycode: u16, flags: u64) -> UngrabAction {
        let (armed, action) = ungrab_chord_action(self.ungrab_armed.get(), flags);
        if input_trace() {
            // Fire and Withhold both return before `tap_flags`, so without this line the chord is
            // invisible to the trace and has to be inferred from its side effects. It is the
            // deciding evidence for "the guest never saw that key at all" — a consumed edge and a
            // forwarded-but-bare edge look identical from the guest end.
            eprintln!(
                "[INP] t={:.1} chord kc={} flags={flags:#x} armed {}->{armed} action={action:?}",
                super::capture_tap::trace_ms(),
                mod_name(macos_keycode),
                self.ungrab_armed.get(),
            );
        }
        self.ungrab_armed.set(armed);
        match action {
            // The gesture was an ungrab after all: the withheld edges were never the guest's.
            UngrabAction::Fire => self.ungrab_withheld.borrow_mut().clear(),
            UngrabAction::Withhold => self
                .ungrab_withheld
                .borrow_mut()
                .push((macos_keycode, flags)),
            UngrabAction::Forward => self.replay_withheld_mods(),
        }
        action
    }

    /// Disarm the ungrab chord — any non-modifier activity (key, button, scroll) between the
    /// chord press and its break means the user was typing a combo, not ungrabbing. Whatever the
    /// chord withheld belongs to the guest after all, so it goes out first: callers run this
    /// *before* forwarding the key/button that cancelled the chord, keeping the order the user
    /// typed (Alt down, then T).
    pub(crate) fn cancel_ungrab_chord(&self) {
        self.ungrab_armed.set(false);
        self.replay_withheld_mods();
    }

    /// Forward, in order, the modifier edges the armed chord held back, then forget them.
    fn replay_withheld_mods(&self) {
        // Drain first: `emit_modifier` must not see a borrow of the queue (and re-entrancy here
        // would double-send).
        let withheld: Vec<(u16, u64)> = self.ungrab_withheld.borrow_mut().drain(..).collect();
        for (keycode, flags) in withheld {
            // The *stored* flags, not the live ones: they are what the guest would have been
            // told at the moment of the press, so the replayed edge is byte-identical (including
            // which physical side of the modifier went down).
            self.emit_modifier(keycode, flags);
        }
    }

    /// Handle one captured event. Returns `true` if it should be swallowed (not passed on
    /// to AppKit) — we swallow keys so unhandled keystrokes don't beep, but let mouse
    /// events through so the title bar / close button keep working.
    pub fn handle(&self, event: &NSEvent, view: &NSView) -> bool {
        // Caps Lock is a lock key kept aligned with the host LED on every event (each carries
        // the live caps bit). This applies deliberate toggles and, crucially, heals drift from
        // a caps toggle done while the VM was unfocused — the monitor sees no event for that and
        // macOS sends no reconciling flagsChanged on refocus, so the next key/pointer event here
        // is what re-syncs the guest. See [`InputState::sync_capslock`].
        self.sync_capslock(event.modifierFlags().0 as u64);
        match event.r#type() {
            NSEventType::KeyDown => {
                self.trace_key(
                    "MON-key",
                    event.keyCode(),
                    true,
                    event.modifierFlags().0 as u64,
                );
                self.cancel_ungrab_chord();
                // The guest kernel autorepeats from key-down state; drop macOS repeats.
                if !event.isARepeat() {
                    // Heal the modifiers first (after the chord replay above), so the key lands
                    // wearing what the user is holding rather than what we last happened to see.
                    self.sync_modifiers(event.modifierFlags().0 as u64, None);
                    self.emit_key(event.keyCode(), true);
                }
                true
            }
            NSEventType::KeyUp => {
                self.emit_key(event.keyCode(), false);
                true
            }
            NSEventType::FlagsChanged => {
                // Degraded (tap-less) capture consumes flagsChanged here — give the ungrab
                // chord (Ctrl+Option) the same meaning it has under the tap.
                let flags = event.modifierFlags().0 as u64;
                self.trace_mods("MON-flags", Some(event.keyCode()), flags);
                if self.is_captured() {
                    match self.observe_ungrab_flags(event.keyCode(), flags) {
                        UngrabAction::Fire => {
                            self.toggle_capture(view);
                            return true;
                        }
                        // Armed: swallow the edge so the ungrab gesture never reaches the guest.
                        UngrabAction::Withhold => return true,
                        UngrabAction::Forward => {}
                    }
                }
                // After the chord has had its say: a `Withhold`/`Fire` returns above, so this
                // never leaks the edges the chord is holding back.
                self.sync_modifiers(flags, Some(event.keyCode()));
                self.emit_modifier(event.keyCode(), flags);
                true
            }
            NSEventType::MouseMoved => {
                if self.is_captured() {
                    self.emit_captured_motion(event, view);
                    return false;
                }
                // The event's OWN display decides, not the primary's: a pointer over another
                // panel is outside the primary's fit on every arrangement, so gating here on it
                // dropped every hover a second display ever saw.
                let t = self.target_of(event, view);
                self.host_cursor
                    .on_motion(t.inside, event_off_view_window(event, view));
                if t.inside {
                    self.emit_motion_to(event, view, t);
                }
                false
            }
            NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDragged => {
                if self.is_captured() {
                    self.emit_captured_motion(event, view);
                    return false;
                }
                let t = self.target_of_drag(event, view);
                self.host_cursor
                    .on_motion(t.inside, event_off_view_window(event, view));
                // A drag continues a press: forward it (clamped) even outside the view,
                // but only if the press itself went to the guest.
                if self.buttons.get().guest_holds() || t.inside {
                    self.emit_motion_to(event, view, t);
                }
                false
            }
            NSEventType::LeftMouseDown => self.emit_press(event, view, BTN_LEFT),
            NSEventType::LeftMouseUp => self.emit_release(BTN_LEFT),
            NSEventType::RightMouseDown => self.emit_press(event, view, BTN_RIGHT),
            NSEventType::RightMouseUp => self.emit_release(BTN_RIGHT),
            NSEventType::OtherMouseDown => self.emit_other_button(event, view, true),
            NSEventType::OtherMouseUp => self.emit_other_button(event, view, false),
            NSEventType::ScrollWheel => {
                // Scroll rides the guest's core pointer wherever that pointer is, so it must be
                // accepted from whichever display the wheel happened over — measured in
                // `spikes/per-display-input/`: a wheel event applies at the core pointer's
                // position, and the position is now per-display.
                if self.is_captured() || self.target_of(event, view).inside {
                    self.emit_scroll(event);
                }
                false
            }
            _ => false,
        }
    }

    /// Is the event's pointer position inside the guest *content* (the fit rect — in
    /// host/fixed modes the letterbox bars are outside, like window chrome)? `false` for
    /// events with no associated window (their location is in screen coordinates and can't
    /// be mapped).
    /// Resolve an event to the guest display it happened on.
    ///
    /// The app's one local `NSEvent` monitor sees every window, and until multi-display every
    /// event could safely be decoded against the primary's view. A secondary window's events
    /// were decoded there too — landing outside the fit, where the gate dropped them — which is
    /// why a second display took no input at all. Every guest window (the primary included,
    /// since Move D) registers in the per-slot registry, so they all decode through one path:
    /// `locationInWindow` against the event window's own layer frame.
    fn target_of(&self, event: &NSEvent, view: &NSView) -> Target {
        // SAFETY: we only run on the main thread (the local event monitor's thread).
        let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };
        let hosted = event
            .window(mtm)
            .and_then(|w| super::windows::slot_of_window(&w).map(|slot| (w, slot)));
        match hosted {
            Some((window, slot)) => {
                let primary = slot == self.primary_slot.get() as usize;
                // The guest's picture is the LAYER's rect, not the content view's: a covered
                // panel gives the camera-housing band back under `notch = avoid`, so the two
                // differ by that band. Reading the layer is what keeps the gate and the mapping
                // agreeing with the pixels rather than with the window — the same rule the
                // primary's fit follows, and the same class of bug as a gate and its emitter
                // disagreeing about where the pointer is.
                let Some(rect) = window
                    .contentView()
                    .and_then(|v| v.layer())
                    .map(|l| l.frame())
                else {
                    return Target {
                        slot,
                        unit: (0.0, 0.0),
                        inside: false,
                        content_w: 0.0,
                        point: (0.0, 0.0),
                        fit: super::fit::FitRect {
                            x: 0.0,
                            y: 0.0,
                            w: 0.0,
                            h: 0.0,
                        },
                        primary,
                    };
                };
                // `locationInWindow` needs no conversion here: it is already in the event's own
                // window's base coordinates, and that is the window we are measuring against.
                let p = event.locationInWindow();
                Target::resolve(
                    slot,
                    primary,
                    p.x,
                    p.y,
                    super::fit::FitRect {
                        x: rect.origin.x,
                        y: rect.origin.y,
                        w: rect.size.width,
                        h: rect.size.height,
                    },
                )
            }
            None => {
                // No window at all: a point with nothing to be inside of.
                if event.window(mtm).is_none() {
                    return Target {
                        slot: self.primary_slot.get() as usize,
                        unit: (0.0, 0.0),
                        inside: false,
                        content_w: 0.0,
                        point: (0.0, 0.0),
                        fit: super::fit::FitRect {
                            x: 0.0,
                            y: 0.0,
                            w: 0.0,
                            h: 0.0,
                        },
                        primary: true,
                    };
                }
                // A window that is no guest window's — the control center, a dialog. Convert
                // into the primary view's space (`event_point_in_view` handles the
                // cross-window hop) and judge against its fit: an affine conversion happily
                // answers for a point over any window anywhere, and for a foreign window it
                // lands outside the fit, where the gate drops it — which is the load-bearing
                // behavior here, "not over guest content". The primary's OWN events no longer
                // take this branch: it registers in the per-slot registry (Move D), so they
                // decode there, against its layer, like every guest window's.
                let p = event_point_in_view(event, view);
                let t = Target::resolve(
                    self.primary_slot.get() as usize,
                    true,
                    p.x,
                    p.y,
                    self.primary_fit(),
                );
                if super::capture_tap::edge_trace() {
                    let old = view.convertPoint_fromView(event.locationInWindow(), None);
                    let old_inside = super::fit::point_in_fit(old.x, old.y, self.primary_fit());
                    if old_inside != t.inside || (old.x - p.x).abs() + (old.y - p.y).abs() > 0.5 {
                        eprintln!(
                            "[MON] t={:.1} gate p=({:.1},{:.1}) inside={} | naive=({:.1},{:.1}) \
                             inside={old_inside}",
                            super::capture_tap::trace_ms(),
                            p.x,
                            p.y,
                            t.inside,
                            old.x,
                            old.y,
                        );
                    }
                }
                t
            }
        }
    }

    fn emit_key(&self, macos_keycode: u16, down: bool) {
        if let Some(code) = macos_keycode_to_linux_remapped(macos_keycode, &self.remap) {
            self.send_kbd(InputEvent::new(EV_KEY, code, down as i32));
            self.send_kbd(InputEvent::syn());
            // Track the held key so a focus loss mid-press can release it (see `release_all_held`).
            if down {
                self.pressed_keys.borrow_mut().insert(macos_keycode);
            } else {
                self.pressed_keys.borrow_mut().remove(&macos_keycode);
            }
        }
    }

    /// Release every key we've forwarded as held — the modifiers (`pressed_mods`) and the
    /// non-modifier keys (`pressed_keys`) — and forget them. Called when the VM window loses key
    /// focus (e.g. the user hit Cmd-Tab): the local event monitor stops delivering events the
    /// instant focus leaves, so the matching key-ups never arrive and the keys would stick "down"
    /// in the guest — a wedged Command then makes the guest compositor eat every later key. Cheap
    /// and idempotent when nothing is held. State is re-learned once focus returns (modifiers from
    /// the next `flagsChanged`, keys from the next key-down), so over-releasing here is safe.
    pub fn release_all_held(&self, why: &str) {
        let mut mods = self.pressed_mods.borrow_mut();
        let mut keys = self.pressed_keys.borrow_mut();
        let mut aux = self.pressed_aux.borrow_mut();
        if mods.is_empty() && keys.is_empty() && aux.is_empty() {
            if input_trace() {
                eprintln!(
                    "[INP] t={:.1} release_all_held({why}) — nothing held",
                    super::capture_tap::trace_ms(),
                );
            }
            return;
        }
        if input_trace() {
            let names: Vec<&str> = mods.iter().map(|&kc| mod_name(kc)).collect();
            eprintln!(
                "[INP] t={:.1} release_all_held({why}) mods=[{}] keys={} aux={}",
                super::capture_tap::trace_ms(),
                names.join(","),
                keys.len(),
                aux.len(),
            );
        }
        for &macos_keycode in mods.iter().chain(keys.iter()) {
            if let Some(code) = macos_keycode_to_linux_remapped(macos_keycode, &self.remap) {
                self.send_kbd(InputEvent::new(EV_KEY, code, 0));
                self.send_kbd(InputEvent::syn());
            }
        }
        // Aux keys are already evdev codes (no keycode map, no remap — the Cmd/Option swap
        // has nothing to say about a media key).
        for &code in aux.iter() {
            self.send_kbd(InputEvent::new(EV_KEY, code, 0));
            self.send_kbd(InputEvent::syn());
        }
        log::debug!(
            "input: released {} held key(s) on focus loss",
            mods.len() + keys.len() + aux.len()
        );
        mods.clear();
        keys.clear();
        aux.clear();
    }

    /// `flagsChanged` reports which modifier key changed but not the direction. [`modifier_emit`]
    /// reads the modifier's *actual* state from the event's flag bitmask rather than toggling a
    /// guess — a single dropped `flagsChanged` (macOS suppresses events while Command is held,
    /// and across focus changes) can't then wedge a modifier "down" in the guest, which would
    /// make the compositor eat every later key. We keep a pressed-set to de-duplicate held
    /// modifiers (one press/one release per change). Lock keys (Caps Lock) instead emit a full
    /// press+release tap per toggle — the guest toggles its own lock on press, so an edge would
    /// stick it (see [`ModEmit::Tap`]); they're not tracked in the pressed-set.
    fn emit_modifier(&self, macos_keycode: u16, raw_flags: u64) {
        // The remap changes which evdev code we emit; `modifier_emit` stays keyed on the
        // *physical* keycode (the macOS modifier-flag state is the physical key's). Caps Lock
        // returns `None` here — it's a lock key handled by `sync_capslock`, not a held modifier.
        let Some(code) = macos_keycode_to_linux_remapped(macos_keycode, &self.remap) else {
            return;
        };
        let was = self.pressed_mods.borrow().contains(&macos_keycode);
        let Some(emit) = modifier_emit(macos_keycode, raw_flags, was) else {
            return;
        };
        match emit {
            ModEmit::None => {}
            ModEmit::Edge(down) => {
                if down {
                    self.pressed_mods.borrow_mut().insert(macos_keycode);
                } else {
                    self.pressed_mods.borrow_mut().remove(&macos_keycode);
                }
                self.send_kbd(InputEvent::new(EV_KEY, code, down as i32));
                self.send_kbd(InputEvent::syn());
                if input_trace() {
                    eprintln!(
                        "[INP] t={:.1}   -> guest mod {} evdev={code} {}",
                        super::capture_tap::trace_ms(),
                        mod_name(macos_keycode),
                        if down { "DOWN" } else { "UP" },
                    );
                }
            }
        }
    }

    /// `LIMINA_INPUT_TRACE`: one line comparing what the host bitmask says every modifier is
    /// doing against what we believe we've told the guest, plus the **drift** — the modifiers
    /// where the two disagree. Drift is the whole diagnosis: a non-empty drift set at the moment
    /// a key is pressed means the guest is about to receive that key wearing the wrong modifiers.
    ///
    /// `tag` names the call site (`MON`/`TAP`/`KEY`/…) and `kc` the keycode the event is *about*
    /// (`None` for events that carry flags without naming a modifier).
    pub(crate) fn trace_mods(&self, tag: &str, kc: Option<u16>, flags: u64) {
        if !input_trace() {
            return;
        }
        let believed = self.pressed_mods.borrow();
        let mut host = Vec::new();
        let mut guest = Vec::new();
        let mut drift = Vec::new();
        for (i, &m) in MODIFIER_KEYCODES.iter().enumerate() {
            let h = limina_input::keymap::modifier_is_down(m, flags).unwrap_or(false);
            let g = believed.contains(&m);
            if h {
                host.push(MODIFIER_NAMES[i]);
            }
            if g {
                guest.push(MODIFIER_NAMES[i]);
            }
            if h != g {
                drift.push(format!(
                    "{}:host={}",
                    MODIFIER_NAMES[i],
                    if h { "DOWN" } else { "up" }
                ));
            }
        }
        eprintln!(
            "[INP] t={:.1} {tag} kc={} flags={flags:#x} host=[{}] guest=[{}]{}",
            super::capture_tap::trace_ms(),
            kc.map(mod_name).unwrap_or("-"),
            host.join(","),
            guest.join(","),
            if drift.is_empty() {
                String::new()
            } else {
                format!("  DRIFT[{}]", drift.join(" "))
            },
        );
    }

    /// Align the guest's **held** modifiers with the host bitmask, emitting whatever edges the
    /// two disagree about. The held-modifier twin of [`InputState::sync_capslock`], and it heals
    /// the same blind spot: a modifier that goes down (or up) while our window isn't receiving
    /// events is never mentioned again, because macOS sends no reconciling `flagsChanged` on
    /// refocus and the key does not move until it is released.
    ///
    /// The case that motivated it (2026-08-09, `spikes/modifier-drift/`): Control held through a
    /// Space switch. Leaving the Space correctly releases it in the guest; coming back restores
    /// nothing, so the next key arrives unmodified — a bare Super, which GNOME reads as "open the
    /// overview". Every bitmask in between carried the Control bit; nothing read it.
    ///
    /// `except` is the modifier the caller is about to emit itself — see
    /// [`reconcile_modifiers`], where excluding it is what keeps Control ahead of Super.
    ///
    /// Called from the `flagsChanged` and key-down paths only, both of which carry
    /// device-dependent bits. Pointer events are deliberately left out: they would heal at class
    /// granularity at best, and [`reconcile_modifiers`] refuses to press on that evidence anyway,
    /// so all they could add is a release we will get from the next key event regardless.
    fn sync_modifiers(&self, raw_flags: u64, except: Option<u16>) {
        let edges = reconcile_modifiers(raw_flags, &self.pressed_mods.borrow(), except);
        for (macos_keycode, down) in edges {
            if input_trace() {
                eprintln!(
                    "[INP] t={:.1}   RESYNC {} {}",
                    super::capture_tap::trace_ms(),
                    mod_name(macos_keycode),
                    if down { "DOWN" } else { "UP" },
                );
            }
            // Through `emit_modifier` rather than straight to the socket, so the pressed-set
            // bookkeeping, the remap and the trace all stay in one place.
            self.emit_modifier(macos_keycode, raw_flags);
        }
    }

    /// Align the guest's caps-lock with the host caps LED (carried by every event's modifier
    /// flags). Emits a press+release tap only when the host LED differs from the believed guest
    /// state ([`CapsLockSync`]), so it both applies deliberate toggles and heals drift from a
    /// caps toggle done while the VM was unfocused — the monitor gets no event for that and
    /// macOS sends no reconciling flagsChanged on refocus, so the next event here re-syncs.
    fn sync_capslock(&self, raw_flags: u64) {
        if self.caps.borrow_mut().observe(capslock_on(raw_flags)) {
            if let Some(code) = macos_keycode_to_linux_remapped(MACOS_KC_CAPSLOCK, &self.remap) {
                self.send_kbd(InputEvent::new(EV_KEY, code, 1));
                self.send_kbd(InputEvent::syn());
                self.send_kbd(InputEvent::new(EV_KEY, code, 0));
                self.send_kbd(InputEvent::syn());
            }
        }
    }

    /// Forward a button press if it lands inside the guest view; remember it so the
    /// matching release (and intervening drags) follow even if the pointer has left.
    /// [`Self::target_of`], for drag events — which macOS delivers to the window that took
    /// the press, wherever the pointer has gone since. Resolving only against that window
    /// clamps the point at its own fit edge once the pointer crosses onto another panel: the
    /// guest pointer pins at the seam while the physical pointer keeps going, and a guest
    /// window dragged toward the other display sticks pressed against the corner. When the
    /// press window's answer is "outside" mid-press, re-resolve the event by screen position
    /// against every guest surface and continue the drag in whichever window the pointer is
    /// really over; over none of them, the press window's clamped answer stands (a drag along
    /// the letterbox or off the desktop edge keeps its old behavior).
    fn target_of_drag(&self, event: &NSEvent, view: &NSView) -> Target {
        let t = self.target_of(event, view);
        if t.inside || !self.buttons.get().guest_holds() {
            return t;
        }
        for (slot, sview, fit) in self.guest_surfaces(view) {
            let p = event_point_in_view(event, &sview);
            if super::fit::point_in_fit(p.x, p.y, fit) {
                let primary = slot == self.primary_slot.get() as usize;
                return Target::resolve(slot, primary, p.x, p.y, fit);
            }
        }
        t
    }

    fn emit_press(&self, event: &NSEvent, view: &NSView, btn: u16) -> bool {
        self.cancel_ungrab_chord();
        if self.is_captured() {
            // Captured: no view gate — the virtual cursor is always over the content. Re-send
            // its position with the press (same staleness guard as the uncaptured path below).
            self.buttons.set(self.buttons.get().pressed(btn_bit(btn)));
            self.send_captured_pos(view);
            self.send_ptr(InputEvent::new(EV_KEY, btn, 1));
            self.send_ptr(InputEvent::syn());
        } else if self.target_of(event, view).inside {
            // The tap, when installed, owns clicks and logs each one with the grab's verdict
            // (`capture_tap::uncaptured_edges`). Without it there is no grab to take at all —
            // this path never had one — so say that rather than letting a click that does
            // nothing also print nothing.
            if !super::capture_tap::installed() {
                log::info!(
                    "pointer capture: click reached the guest through the NSEvent monitor — no \
                     event tap, so no grab is taken (grant Accessibility to enable it)"
                );
            }
            self.buttons.set(self.buttons.get().pressed(btn_bit(btn)));
            // Send the position with the press so the guest clicks where the host did,
            // even if the last forwarded motion is stale (pointer re-entered the view).
            self.emit_motion(event, view);
            self.send_ptr(InputEvent::new(EV_KEY, btn, 1));
            self.send_ptr(InputEvent::syn());
        }
        false
    }

    /// Forward a release only for presses the guest saw (never leaves a button stuck).
    fn emit_release(&self, btn: u16) -> bool {
        let (next, forward) = self.buttons.get().released(btn_bit(btn));
        self.buttons.set(next);
        if forward {
            self.send_ptr(InputEvent::new(EV_KEY, btn, 0));
            self.send_ptr(InputEvent::syn());
        }
        false
    }

    fn emit_other_button(&self, event: &NSEvent, view: &NSView, down: bool) -> bool {
        // buttonNumber 2 is the middle button; ignore further buttons for now.
        if event.buttonNumber() == 2 {
            if down {
                self.emit_press(event, view, BTN_MIDDLE);
            } else {
                self.emit_release(BTN_MIDDLE);
            }
        }
        false
    }

    /// Map the event's window-local cursor position to the absolute device range through
    /// the current fit rect (letterbox offset + scale, Y flipped — AppKit is bottom-left
    /// origin, evdev top-left). Positions outside the content (drags that left it) clamp
    /// to the nearest content edge. With the fit at the full view (dynamic mode) this is
    /// the legacy full-bounds mapping, bit for bit. Also remembers the (clamped) position
    /// as the capture seed, so a grab starts exactly where the cursor was.
    fn emit_motion(&self, event: &NSEvent, view: &NSView) {
        let t = self.target_of(event, view);
        self.emit_motion_to(event, view, t);
    }

    /// As [`Self::emit_motion`], for a caller that has already resolved the target — the gates
    /// all have, and resolving twice would read the layer geometry twice per event.
    fn emit_motion_to(&self, event: &NSEvent, view: &NSView, t: Target) {
        if t.content_w > 0.0 {
            self.pointer_slot.set((t.slot, t.content_w));
        }
        self.send_abs_unit(t.slot, t.unit, false);

        // Remember the position as the capture seed — the event's point in ITS window's view
        // space, with the slot naming that window — so a grab starts exactly where the cursor
        // was. From every window, not just the primary: the grab spans every covered panel, so
        // the fallback seed should be wherever the pointer last was over any guest content.
        // Gated on a resolved fit: an unresolved target carries no usable point.
        if t.fit.w > 0.0 {
            self.capture_pos.set(Some(t.point));
            self.capture_slot.set(t.slot);
        }

        // The chrome ask, fed per slot with the target's own geometry — every covered panel
        // has a band to ask past, not just the primary's. Only when the tap is NOT installed.
        // The tap is session-wide, so when it is up it sees every one of these events and is
        // the gesture's real feed; running both makes the same physical motion arrive twice
        // and the two fight — one granting the ask, the other releasing it on the same event,
        // 6 ms apart. One owner, chosen by which mechanism is actually live: the tap when
        // there is a grant, this when there is not. Gated on a resolved fit: an unresolved
        // target carries a zero rect, and judging a gesture against it is meaningless.
        if !super::capture_tap::installed() && t.fit.w > 0.0 {
            self.reveal_step(t.slot, t.point, event.deltaY(), t.fit, RevealSrc::Monitor);
        }
        // Everything below is expressed in the PRIMARY view's coordinate space and belongs to
        // the primary window alone. An event from another window converts into that space as a
        // point at or beyond the fit's edge by construction, which made `edge_overflow` hand the
        // guest the whole delta of every stroke as relative motion — pressure barriers charging
        // from motion that never happened at that edge, on a display that has no such edge.
        if !t.primary {
            return;
        }
        let p = event_point_in_view(event, view);
        let fit = self.primary_fit();
        // Hand the guest any push *into* an edge as relative motion, so its own pressure
        // barriers charge. The absolute tablet can only report a position, and a barrier needs
        // motion against it, so without this the GNOME hot corner is unreachable — which it was:
        // the only code forwarding pressure lived in the capture tap, quietly making a core
        // guest interaction depend on the Accessibility grant. Proven by driving a uinput mouse
        // into the guest's corner directly, where the overview opened instantly
        // (`spikes/edge-pressure/`).
        //
        // Harmless when the tap *is* installed: while resistance holds it consumes the event, so
        // this never runs for the same motion.
        //
        // Filtered to this slot's OUTER edges (`arrangement::outer_edges_at`): the fit clamp is
        // the window's edge, pressure belongs at the desktop's. An uncaptured pointer moving
        // toward a seam keeps reporting past the fit edge, and unfiltered overflow there is
        // seam pressure — the two devices fighting over one pointer.
        //
        // This is the ONE path that consults `outer_edges_at`, and it is reached from plain
        // uncaptured motion — no button. There is no uncaptured *drag* to test it with: where
        // the tap is installed it owns clicks and takes the grab, so a press captures.
        let over = super::fit::edge_overflow((p.x, p.y), event.deltaX(), event.deltaY(), fit);
        let over = super::arrangement::outer_edges_at(t.slot, t.unit.0, t.unit.1).keep(over);
        if over != (0.0, 0.0) {
            send_edge_overflow(&self.conn, over);
            self.last_push.set(Some(std::time::Instant::now()));
        }
    }

    /// Put the guest's pointer at a unit position on one slot's content.
    ///
    /// Through the guest's reported layout, never straight onto the range: the guest spreads
    /// one absolute device over every connector it has, so a full sweep of ONE window must
    /// cover only that display's share of it. With one display, or no report, the two are
    /// identical ([`super::arrangement::abs_through_report`]). Silent when the slot has no
    /// mapping yet — a pointer that briefly does not move, rather than one that jumps.
    fn send_abs_unit(&self, slot: usize, unit: (f64, f64), captured: bool) {
        let Some((x, y)) = super::absfit::abs_position(slot, unit.0, unit.1, ABS_MAX as i32) else {
            return;
        };
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, x));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, y));
        self.send_ptr(InputEvent::syn());
        self.record_sent(
            slot,
            unit,
            (
                f64::from(x) / f64::from(ABS_MAX),
                f64::from(y) / f64::from(ABS_MAX),
            ),
            captured,
        );
    }

    /// Note what we just told the guest, for [`Self::verify_guest_echo`].
    fn record_sent(
        &self,
        slot: usize,
        unit: (f64, f64),
        device: (f64, f64),
        captured: bool,
    ) -> u64 {
        let seq = super::echo::note_send();
        let probe = self.probe.get().is_some();
        if !probe {
            self.hand_sends.set(self.hand_sends.get() + 1);
            self.hand_send_at.set(Some(std::time::Instant::now()));
        }
        self.sent.set(Some(Sent {
            slot,
            unit,
            device,
            seq,
            before: super::echo::shown_point(&super::echo::snapshot()),
            at: std::time::Instant::now(),
            captured,
            probe,
        }));
        seq
    }

    /// Compare the guest's cursor echo with the last position we sent, once both have
    /// settled — called from the render tick. The guest's pointer is ours to place, so once
    /// our last position has had [`ECHO_SETTLE`] to echo back (and no relative push has moved
    /// it since), the guest's cursor plane must be on the slot we sent and within
    /// [`super::echo::TOLERANCE_PX`] of the pixel. Anything else is a pointer on a display
    /// we did not put it on — the fault behind every "reveal on the other screen" — and it
    /// crashes (`assert!`, by design; see `window/echo.rs`). A guest showing no cursor at all
    /// has hidden its pointer and is not judged.
    pub(crate) fn verify_guest_echo(&self) {
        let Some(sent) = self.sent.get() else {
            return;
        };
        self.sample_guest_echo(&sent);
        if self.echo_checked.get() == Some(sent.at) {
            return;
        }
        let now = std::time::Instant::now();
        if now.duration_since(sent.at) < ECHO_SETTLE
            || self
                .last_push
                .get()
                .is_some_and(|p| now.duration_since(p) < ECHO_SETTLE)
        {
            return;
        }
        self.echo_checked.set(Some(sent.at));
        if sent.probe {
            return; // the probe sends places the pointer is not; there is nothing to judge
        }
        let echo = super::echo::snapshot();
        let e = echo[sent.slot];
        let expect = super::echo::expected_pixel(sent.unit, (e.w, e.h));
        match super::echo::verdict(sent.slot, expect, &echo) {
            Ok(hit) => {
                if super::capture_tap::edge_trace() {
                    eprintln!(
                        "[ECHO] t={:.1} slot={} sent=({:.1},{:.1}) guest=({},{}) {}",
                        super::capture_tap::trace_ms(),
                        sent.slot,
                        expect.0,
                        expect.1,
                        e.x,
                        e.y,
                        match hit {
                            Some(miss) => format!("miss={miss:.1}px"),
                            None => "hidden".to_string(),
                        },
                    );
                }
            }
            Err(why) => log::warn!(
                "guest pointer: {why}; captured={}, capture slot {}, unit sent ({:.4},{:.4}); report: {:?}; arrangement: {}",
                sent.captured,
                self.capture_slot.get(),
                sent.unit.0,
                sent.unit.1,
                super::arrangement::reported_logical_sizes(),
                super::hostdisplay::describe_arrangement(),
            ),
        }
    }

    /// Grant the chrome ask outright, without a gesture of its own.
    ///
    /// The fullscreen grab's top-edge release calls this: pressing upward under `notch = extend`
    /// means "let me at the menu bar", and making that two separate gestures — press to free the
    /// pointer, then lean again to drop the overlay — would be asking twice for one intention.
    /// Releasing it stays [`Self::reveal_step`]'s job, so the overlay comes back the same way it
    /// always has.
    pub(crate) fn grant_chrome(&self) {
        // The grab's release happens in the window owning the captured cursor, so that slot's
        // band is the one being asked past.
        let slot = self.capture_slot.get();
        if !self.overlay_active_of(slot) {
            return; // nothing to ask for; the chrome is already reachable
        }
        let now = std::time::Instant::now();
        self.with_reveal(|st| super::grab_policy::reveal_grant(st, slot, now));
    }

    /// Slave the chrome ask to the OBSERVED macOS menu bar (`NSMenu::menuBarVisible`, called
    /// from the render tick every tick; measured 2026-08-20: it tracks the fullscreen
    /// auto-reveal — false on entering fullscreen, true while the reveal is out).
    ///
    /// The push gesture ([`Self::reveal_step`]) estimates the same intent macOS's own reveal
    /// threshold judges, and the two clocks disagree: macOS reveals first, the user sees the
    /// menu bar appear and stops pushing — and on a notched panel the revealed bar sits in the
    /// very band the strip covers, so it appeared *behind* the strip while our charge never
    /// finished. Observation replaces estimation for this trigger: macOS revealing its bar IS
    /// the grant, macOS re-hiding it IS the release. The push gesture stays as a secondary
    /// trigger, and the captured edge-release keeps [`Self::grant_chrome`].
    ///
    /// The grant is gated on the pointer actually being at the top of a panel with an active
    /// band, so unrelated global toggles (Space switches show the bar too) cannot lower a
    /// strip the user never asked about.
    pub(crate) fn menubar_observed(&self, visible: bool, primary_view: &NSView) {
        if self.menubar_seen.replace(visible) == visible {
            return;
        }
        if self.captured.load(Ordering::Relaxed) {
            return; // a parked pointer never summoned the bar; the grab owns its own path
        }
        if visible {
            // Which panel did macOS reveal for? The one whose TOP edge the pointer is at —
            // judged per panel, in each panel's own view space
            // ([`super::grab_policy::at_panel_top`] says why not the owner-sticky targeting).
            for (slot, view, fit) in self.guest_surfaces(primary_view) {
                let Some(p) = live_pointer_in_view(&view) else {
                    continue;
                };
                let at_top = super::grab_policy::at_panel_top(p, fit);
                if super::capture_tap::edge_trace() {
                    eprintln!(
                        "[MENUBAR-ASK] t={:.1} slot={slot} p=({:.1},{:.1}) top={:.1} \
                         at_top={at_top} overlaid={} ask={:?}",
                        super::capture_tap::trace_ms(),
                        p.0,
                        p.1,
                        fit.y + fit.h,
                        self.overlay_active_of(slot),
                        self.reveal.get().ask(),
                    );
                }
                if at_top && self.overlay_active_of(slot) && self.reveal.get().ask() != Some(slot) {
                    let now = std::time::Instant::now();
                    self.with_reveal(|st| super::grab_policy::reveal_grant(st, slot, now));
                    return;
                }
            }
        } else if self.reveal.get().ask().is_some() {
            self.reveal_moot();
        }
    }

    /// Read-modify-write the reveal state — the ONE writer, which keeps the `reveal_chrome`
    /// Arc (what the primary's overlay reconcile reads) mirroring `ask == primary` on every
    /// change, so the two can never disagree.
    fn with_reveal<T>(&self, f: impl FnOnce(&mut super::grab_policy::RevealState) -> T) -> T {
        let mut st = self.reveal.get();
        let out = f(&mut st);
        self.reveal.set(st);
        self.reveal_chrome.store(
            st.ask() == Some(self.primary_slot.get() as usize),
            Ordering::Relaxed,
        );
        out
    }

    /// Whether `slot`'s window currently hosts the guest under an active extend overlay — the
    /// primary's via the shared flag its reconcile writes, a secondary's via its registry.
    fn overlay_active_of(&self, slot: usize) -> bool {
        if slot == self.primary_slot.get() as usize {
            self.overlay_active.load(Ordering::Relaxed)
        } else {
            super::windows::band_active(slot)
        }
    }

    /// The slot whose chrome ask is granted, if any — what a secondary's reconcile consumes.
    pub(crate) fn reveal_ask_slot(&self) -> Option<usize> {
        self.reveal.get().ask()
    }

    /// The chrome ask is moot — see [`super::grab_policy::RevealState::moot`]. Goes through
    /// the one writer so the mirror clears with it.
    pub(crate) fn reveal_moot(&self) {
        self.with_reveal(|st| st.moot());
    }

    /// Ask for the macOS chrome back, or give it up again.
    ///
    /// Under the `notch = extend` overlay nothing can appear over the guest — that is the point of
    /// it — so the menu bar and the window's own controls need a deliberate way in for the VM's
    /// menu actions. Sustained upward push at the guest's top edge is the ask; coming back into
    /// the guest is the release.
    ///
    /// This lives in the **local monitor**, not the capture tap, on purpose. The tap needs an
    /// Accessibility grant, and a build without one (any freshly-compiled dev binary, since TCC
    /// keys on the code hash) would otherwise have no way to reach the menu bar at all — found
    /// the hard way on the first dogfood run of this feature. The monitor only sees events over
    /// our own window, which is all this needs. Uncaptured only: a grabbed pointer must never
    /// trip it.
    pub(crate) fn reveal_step(
        &self,
        slot: usize,
        p: (f64, f64),
        delta_y: f64,
        fit: super::fit::FitRect,
        src: RevealSrc,
    ) {
        let sample = super::grab_policy::Reveal {
            now: std::time::Instant::now(),
            slot,
            pos: p,
            delta_y,
            fit,
            src,
            captured: self.captured.load(Ordering::Relaxed),
            overlay_active: self.overlay_active_of(slot),
        };
        let trace = self.with_reveal(|st| super::grab_policy::reveal_step(st, &sample));
        // The recording instrument for this gesture. Every constant in the policy was guessed
        // once and wrong once; `[REVEAL]` exists so the next value comes from a trace of the
        // movement the user actually intends instead. Emitted for the whole top band, tagged
        // with why the step ended, since a gesture that does not fire is exactly the
        // interesting case.
        if let Some(t) = trace {
            if super::capture_tap::edge_trace() {
                eprintln!(
                    "[REVEAL] t={:.1} src={} slot={slot} p=({:.1},{:.1}) dy={delta_y:.1} \
                     top={:.1} overlaid={} push={:.1} charge={:.3} ask={:?} {}",
                    super::capture_tap::trace_ms(),
                    src.tag(),
                    p.0,
                    p.1,
                    fit.y + fit.h,
                    sample.overlay_active,
                    t.push,
                    t.charge,
                    t.ask,
                    t.why,
                );
            }
        }
    }

    /// Capture mode: integrate the event's motion delta into the virtual cursor and drive the
    /// absolute tablet with it — the degraded no-tap path, riding the exact step-and-emit the
    /// tap uses so the two cannot map differently.
    fn emit_captured_motion(&self, event: &NSEvent, view: &NSView) {
        // Re-pin the (hidden) host cursor to the park so it can't drift onto windows behind
        // us — CGAssociate(false) alone doesn't reliably freeze it. Zero-length (the cursor is
        // already at the park), so it injects nothing.
        self.repin_park(view);
        let (dx, dy) = self.swallow_warp(event.deltaX(), event.deltaY());
        self.captured_step_and_emit(dx, dy, view);
    }

    /// One captured-pointer step: integrate a motion delta into the virtual cursor, clamped to
    /// the capture window's content; drive the absolute tablet; forward the clamped-off
    /// overflow as edge pressure; and answer with the cursor in that window's view space — the
    /// space every per-window policy decision (edge press, release, park) is judged in. Both
    /// captured paths (the tap and the degraded local monitor) come through here; there is no
    /// second stepper.
    ///
    /// The virtual cursor lives in ONE window for the whole capture session — the window the
    /// grab was taken in (`capture_slot`). It does not cross to another guest window's panel:
    /// the clamp at this window's content edge is the grab's edge, and a seam to a covered
    /// neighbour is an edge like any other for as long as capture lasts. The way to the other
    /// panel is the grab's release (and, in fullscreen, the policy's re-grab there). Crossing
    /// while captured belongs to the next design round, with the devices to do it without
    /// guessing the guest's layout.
    ///
    /// `None` when the capture window has no surface this tick — the cursor briefly does not
    /// move, it never guesses.
    pub(crate) fn captured_step_and_emit(
        &self,
        dx: f64,
        dy: f64,
        primary_view: &NSView,
    ) -> Option<CapturedStep> {
        // Nothing has ever placed the cursor: it seeds at the primary content's centre
        // (`fit::capture_step` on `None`), so the primary is its window.
        if self.capture_pos.get().is_none() {
            self.capture_slot.set(self.primary_slot.get() as usize);
        }
        let slot = self.capture_slot.get();
        let (view, fit) = self.slot_surface(slot, primary_view)?;
        if fit.w <= 0.0 || fit.h <= 0.0 {
            return None;
        }
        // The fit-space estimate: drawn, pressed and released in this window's geometry, and
        // re-based from the guest's echo whenever one arrives (`follow_guest_echo`).
        let step = super::fit::capture_step(self.capture_pos.get(), dx, dy, fit);
        self.capture_pos.set(Some(step.pos));
        if dx != 0.0 || dy != 0.0 {
            self.last_captured_motion
                .set(Some(std::time::Instant::now()));
        }
        let (u, v) = super::fit::unit_through_fit(step.pos.0, step.pos.1, fit);
        // The position the guest receives lives in the device range and stays continuous
        // across seams: host deltas scale by this slot's gain and pin only where the guest's
        // desktop ends. The guest crosses its own seams.
        // The desktop the range is spread over, and this slot's share of it. Both come from
        // the guest's own report where there is one: the share gives the gain exactly, and the
        // desktop is what the step is held on — the range's ends are the bounding box's
        // corners, and a desktop that is not a rectangle has box to spare that is on no
        // monitor. Without a report, the row-of-scanouts estimate and the box, as before.
        let desktop = super::arrangement::desktop_in_range(f64::from(ABS_MAX));
        let gain = match desktop.as_ref().and_then(|d| d.rect_of(slot)) {
            Some(share) => super::fit::range_gain_of_share(fit, share),
            None => {
                let sizes = super::echo::scanout_sizes();
                let row_w: u32 = sizes.iter().map(|s| s.0).sum();
                let row_h: u32 = sizes.iter().map(|s| s.1).max().unwrap_or(0);
                super::fit::range_gain(fit, sizes[slot], row_w, row_h, f64::from(ABS_MAX))
            }
        };
        let (range, pressure) = match (self.capture_range.get(), gain) {
            (Some(r), Some(g)) => {
                let rs = super::fit::range_step(r, dx, dy, g, f64::from(ABS_MAX), desktop.as_ref());
                (rs.pos, rs.overflow)
            }
            // No running position yet (the session's first step), or no mode sizes to scale
            // by: place the fit position through the mapping, as the uncaptured path does.
            _ => {
                let (x, y) = super::absfit::abs_position(slot, u, v, ABS_MAX as i32)?;
                (
                    (f64::from(x), f64::from(y)),
                    super::arrangement::outer_edges_at(slot, u, v).keep(step.overflow),
                )
            }
        };
        // A seam the hand cannot follow is an edge: hold the range inside this slot's own
        // share where the display on the other side is not one the user is looking at, or is
        // not the same display the host panel beside us shows (`super::seams`). What the hold
        // eats is charged to nobody — the guest's desktop really does continue there, so
        // forwarding it as pressure would walk the guest's pointer onto the neighbour while
        // the absolute device snapped it back. The push is answered in fit space instead, by
        // the grab's own edge release, whose charge this clamp is what lets accumulate: before
        // it, the guest crossed first and `follow_guest_echo` re-homed the fit out from under
        // the press (2026-08-24).
        let range =
            super::seams::Hold::of(&self.seam_facts(primary_view), slot, range).apply(range);
        self.capture_range.set(Some(range));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, range.0.round() as i32));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, range.1.round() as i32));
        self.send_ptr(InputEvent::syn());
        self.record_sent(
            slot,
            (u, v),
            (range.0 / f64::from(ABS_MAX), range.1 / f64::from(ABS_MAX)),
            true,
        );
        // What the range's ends ate is the push against the guest desktop's edge, which the
        // guest's own barriers charge on. (The grab's release press charges from the fit
        // position and delta, in `capture_tap`, and needs no pressure on the wire.)
        if pressure != (0.0, 0.0) {
            send_edge_overflow(&self.conn, pressure);
            self.last_push.set(Some(std::time::Instant::now()));
        }
        Some(CapturedStep {
            slot,
            view_point: step.pos,
            fit,
            view,
            range,
        })
    }

    /// Move the park into the window of the panel the guest's cursor has crossed to, once the
    /// hand has paused — called from the render tick, never from the motion path. The park
    /// follows the cursor so everything macOS routes by host-cursor display (a three-finger
    /// swipe, Mission Control) acts on the panel the user is looking at.
    ///
    /// Quiescence is the design, not an optimization: a nonzero warp injects its vector into
    /// the delta stream (see `warp::WarpSwallow`), and only a paused hand makes the injected
    /// event arrive pure enough to recognize. The swipe this serves needs no faster park —
    /// fingers leave the trackpad before a three-finger swipe. One warp per crossing; steady
    /// motion and a cursor resting on its own panel never warp at all.
    ///
    /// The crossing gate is all this adds: the move itself is [`Self::rederive_park`], shared
    /// with the re-pin's repair path so both validate the new point the same way.
    pub(crate) fn repark_if_quiescent(&self, primary_view: &NSView) {
        if !self.is_captured() || self.capture_slot.get() == self.park_slot.get() {
            return;
        }
        if let Some(t) = self.last_captured_motion.get() {
            if t.elapsed() < REPARK_QUIESCENCE {
                return;
            }
        }
        // Nothing to say when this one declines: a crossing whose window is not placeable this
        // tick simply re-parks on a later tick. Only the re-pin's repair path is a symptom.
        let _ = self.rederive_park(primary_view, "repark");
    }

    /// Run one real motion event's delta through the armed injection detector, if any —
    /// consumed by the REAL motion paths only (the tap and the degraded monitor).
    pub(crate) fn swallow_warp(&self, dx: f64, dy: f64) -> (f64, f64) {
        self.warp.swallow(dx, dy)
    }

    /// Feed the mapping one measurement, as soon as the guest has answered the last send.
    ///
    /// Separate from the verdict below and gated differently on purpose. The verdict is a fault
    /// detector, so waiting a fixed [`ECHO_SETTLE`] for it costs nothing; a *sample* is the only
    /// thing standing between the user and a correctly placed pointer, so it is taken the
    /// instant the guest replies ([`super::echo::settled`]) — about a frame, not 150 ms. One
    /// sample per send, and never while a relative push is in flight: pressure moves the guest's
    /// cursor without our asking, and a sample that attributes that motion to our position would
    /// teach a mapping that is wrong by however hard the user leaned.
    fn sample_guest_echo(&self, sent: &Sent) {
        if self.sampled.get() == Some(sent.seq) {
            return;
        }
        if self
            .last_push
            .get()
            .is_some_and(|p| p.elapsed() < ECHO_SETTLE)
        {
            return;
        }
        // The guest must have CAUGHT UP, not merely answered something.
        //
        // A cursor echo is stamped with the latest send at the moment it arrives
        // (`echo.rs`), not with the send it answers — nothing on the wire carries that. While
        // the hand moves fast the sends outrun the echoes (measured during a window drag: a
        // send every ~7 ms against an echo every ~17 ms), so the echo answering send N is
        // stamped N+2 and matches as if it answered it. The sampler then paired the device
        // value of N+2 with the pixel from N — a systematic lag of one or two motions, 15-20 px
        // at drag speed, which is over TOLERANCE_PX. Three of those unseated a correct line,
        // the reseed inherited the same skew and could not state one, and the slot went
        // unlearned — so a fast drag SUMMONED a sweep, mid-drag (2026-08-22).
        //
        // Waiting for the send to be this old, with no newer send since (the seq match below),
        // is what makes the pair honest: the queue has drained and the echo can only be this
        // send's. During a continuous drag nothing is sampled at all, which is correct — the
        // pairing is genuinely not knowable there. Samples resume on any pause, which ordinary
        // pointer use is full of.
        if sent.at.elapsed() < SAMPLE_SETTLE {
            return;
        }
        let echo = super::echo::snapshot();
        if super::echo::settled(sent.seq, sent.before, &echo).is_none() {
            return;
        }
        self.sampled.set(Some(sent.seq));
        super::absfit::observe(sent.device, &echo);
    }

    /// Learn the absolute device's per-display shares deliberately, instead of waiting for the
    /// user to sweep far enough by accident.
    ///
    /// Passive learning has a hole at exactly the moment it is needed most. A slot can only be
    /// measured from samples that landed *on* it, and right after a display joins the mapping is
    /// identity — so whichever slot the guest's cursor happens to be on gets all the samples and
    /// the other gets none, until the user makes a stroke wide enough to cross. Dogfood
    /// 2026-08-22 hit precisely that: one slot learned, the other never did, and with the pointer
    /// misplaced there was no comfortable way to make the crossing stroke.
    ///
    /// The grab is what makes the cure available. While captured the host pointer is parked,
    /// disassociated and invisible, so the device is ours to sweep: the user's pointer does not
    /// move, nothing competes for the echo, and every step's answer is unambiguous. Ten steps at
    /// about a frame each ([`super::echo::settled`]) — a fifth of a second in which the guest's
    /// cursor darts, at a moment when the screen configuration has just changed anyway.
    ///
    /// Ends the moment the hand moves: the user is driving again, and the sweep is not worth
    /// fighting them for. The position they had is put back either way.
    pub(crate) fn probe_mapping(&self, primary_view: &NSView) {
        if let Some(p) = self.probe.get() {
            // The same two reservations the sweep started under, re-read every step: a sweep
            // outlives the tick it began on, so a hand that arrives 100 ms in has to stop it
            // too. A button is watched separately because presses go straight to the device
            // and never reach `record_sent`, so they move nothing the send count can see.
            let interrupted = if self.buttons.get().any_down() {
                Some("a button went down")
            } else if self.hand_sends.get() != p.hand {
                Some("the hand moved")
            } else {
                None
            };
            if let Some(why) = interrupted {
                self.end_probe(p, why, primary_view);
            } else if self.sampled.get() == Some(p.seq) || p.at.elapsed() > PROBE_STEP_TIMEOUT {
                self.log_probe_step(p);
                match p.step + 1 {
                    n if n < super::absfit::PROBE_SWEEP.len() => self.send_probe_step(p, n),
                    _ => self.end_probe(p, "swept", primary_view),
                }
            }
            return;
        }
        // Deliberately not gated on the grab — see [`super::absfit::probe_may_start`], which
        // holds the reasoning and the reservations that ARE real.
        if !super::absfit::probe_may_start(super::absfit::ProbeGate {
            wanted: super::absfit::probe_wanted(),
            buttons_down: self.buttons.get().any_down(),
            since_sweep: self.probe_rested.get().map(|t| t.elapsed()),
            since_hand: self.hand_send_at.get().map(|t| t.elapsed()),
        }) {
            return;
        }
        // A sweep with the pointer captured is a fault, not a mode. The captured pointer is
        // unambiguously the user's and captured motion already feeds the fit continuously, so
        // arriving here means passive learning left a slot unlearned and the cursor is about to
        // be taken out of a moving hand. That is worth a line in the ordinary log: the one time
        // it fired it named the slot and axis, which is what turned "a sweep interrupted my
        // drag" into a specific pairing bug in the sampler within minutes.
        //
        // Deliberately a report and not a veto. Denying the captured case outright would also
        // hide the next cause of it, and an unlearned slot is a real thing to fix, not to
        // suppress — so the sweep still runs and still says why it wanted to.
        if self.is_captured() {
            log::warn!(
                "display: a sweep is starting with the pointer CAPTURED — passive learning should have covered this; what wanted it: {}",
                super::absfit::incomplete_slots()
            );
        }
        log::info!(
            "display: sweeping the absolute device to learn each display's share of it — the guest's cursor moves for a moment"
        );
        self.send_probe_step(
            Probe {
                step: 0,
                seq: 0,
                at: std::time::Instant::now(),
                hand: self.hand_sends.get(),
            },
            0,
        );
    }

    /// One line per sweep step: what went on the wire, what the guest did with it, and where.
    ///
    /// This is the sweep's whole evidence trail — the pairs it is fitting the lines from — so it
    /// is at info, not behind the trace flag. A step with no answer is as informative as one with
    /// an answer, and is printed too.
    fn log_probe_step(&self, p: Probe) {
        let (u, v) = super::absfit::PROBE_SWEEP[p.step];
        let range = f64::from(ABS_MAX);
        let echo = super::echo::snapshot();
        let n = super::absfit::PROBE_SWEEP.len();
        match super::echo::settled(p.seq, None, &echo) {
            Some((slot, x, y)) => log::info!(
                "display: sweep {}/{n} sent ({u:.2},{v:.2}) = abs ({},{}) -> slot {slot} at ({x},{y}) of {}x{}",
                p.step + 1,
                (u * range).round(),
                (v * range).round(),
                echo[slot].w,
                echo[slot].h,
            ),
            None => log::info!(
                "display: sweep {}/{n} sent ({u:.2},{v:.2}) = abs ({},{}) -> no answer in {:?}",
                p.step + 1,
                (u * range).round(),
                (v * range).round(),
                p.at.elapsed(),
            ),
        }
    }

    /// Put one sweep position on the device and wait for its answer.
    fn send_probe_step(&self, p: Probe, step: usize) {
        let (u, v) = super::absfit::PROBE_SWEEP[step];
        let range = f64::from(ABS_MAX);
        // Set the probe BEFORE recording the send, so the send is marked as the probe's.
        self.probe.set(Some(Probe {
            step,
            seq: 0,
            at: std::time::Instant::now(),
            ..p
        }));
        let seq = self.send_device((self.capture_slot.get(), (u, v)), (u * range, v * range));
        self.probe.set(Some(Probe {
            step,
            seq,
            at: std::time::Instant::now(),
            ..p
        }));
    }

    /// Sweep over: put the pointer back where the user left it and let go.
    ///
    /// "Where the user left it" is a place on a display — [`Self::capture_pos`] in
    /// [`Self::capture_slot`]'s window, which both pointer paths keep current — and it is
    /// re-placed through the mapping **as it now stands**. Remembering the device number
    /// instead would put the pointer back through the very mapping the sweep just replaced,
    /// which is a teleport, not a restore.
    fn end_probe(&self, p: Probe, why: &str, primary_view: &NSView) {
        self.probe.set(None);
        self.probe_rested.set(Some(std::time::Instant::now()));
        // Where the pointer belongs, in a display's own terms. Normally where the user left
        // it. But the very first sweep of a session can run before anything has ever recorded
        // that — the grab it arrives with is taken by the guest gaining the screen, and if the
        // host pointer is on macOS chrome at that moment (the menu the user just enabled this
        // from, typically) there is no remembered place at all. Leaving the pointer wherever
        // the last sweep step went is the one outcome that looks broken: it strands the cursor
        // in a corner of a display the user was not on, and the next sweep then "restores" to
        // that stranded spot. The middle of the primary's content is somewhere, and somewhere
        // is the whole requirement.
        let target = self
            .capture_projection(primary_view)
            .map(|pr| {
                (
                    pr.slot,
                    super::fit::unit_through_fit(pr.point.0, pr.point.1, pr.fit),
                )
            })
            .or_else(|| {
                let fit = self.primary_fit();
                (fit.w > 0.0 && fit.h > 0.0).then(|| (self.primary_slot.get() as usize, (0.5, 0.5)))
            });
        let put = target.and_then(|(slot, (u, v))| {
            let (x, y) = super::absfit::abs_position(slot, u, v, ABS_MAX as i32)?;
            self.send_device((slot, (u, v)), (f64::from(x), f64::from(y)));
            Some((slot, u, v, x, y))
        });
        match put {
            Some((slot, u, v, x, y)) => log::info!(
                "display: the sweep ended after {} of {} steps ({why}); pointer put back on slot {slot} at ({u:.4},{v:.4}) = abs ({x},{y})",
                p.step + 1,
                super::absfit::PROBE_SWEEP.len()
            ),
            None => log::info!(
                "display: the sweep ended after {} of {} steps ({why}); the pointer stays where the sweep left it — no place to put it back to",
                p.step + 1,
                super::absfit::PROBE_SWEEP.len()
            ),
        }
    }

    /// Put an absolute position on the device and remember it as the captured cursor's, so the
    /// ordinary captured path carries on from wherever this left it.
    ///
    /// `on` is where the position is *expected to land* — the slot and that display's own unit
    /// — which is what the verifier checks the guest's answer against. It is not the device
    /// unit `pos` carries: on a multi-display guest the two differ by exactly the mapping this
    /// module exists to learn, and feeding the device unit to the verifier makes every restore
    /// look like a pointer on the wrong display by the width of a screen. A sweep step has no
    /// honest answer for it (finding out is the point) and passes its own units, which costs
    /// nothing because a probe send is never verified.
    fn send_device(&self, on: (usize, (f64, f64)), pos: (f64, f64)) -> u64 {
        self.capture_range.set(Some(pos));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, pos.0.round() as i32));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, pos.1.round() as i32));
        self.send_ptr(InputEvent::syn());
        let range = f64::from(ABS_MAX);
        self.record_sent(on.0, on.1, (pos.0 / range, pos.1 / range), true)
    }

    /// Every tick while captured: the host pointer must still be wearing the blank.
    /// See [`HostCursor::verify_captured`] — this is the only thing that re-asserts it while
    /// the tap owns the motion events.
    pub(crate) fn verify_captured_wear(&self) {
        if self.is_captured() {
            self.host_cursor.verify_captured();
        } else {
            self.host_cursor.verify_free();
        }
    }

    /// Follow the guest: re-base the captured estimate onto the slot and pixel the guest's
    /// cursor echo names, and log every crossing. Called from the render tick while
    /// captured. The guest decides which display a range value lands on, so the echo is the
    /// truth the fit-space estimate (drawing, edge press, release target) is kept in step
    /// with; a seam the guest crossed moves the capture window with it, and the estimate
    /// leaves the old fit's edge before any release charge could accumulate there.
    /// Where the guest's cursor is right now, as an [`EchoGate`] key — the position an
    /// engaging grab must not mistake for an answer to what it is about to send.
    fn echo_key_now(&self) -> Option<(usize, i32, i32)> {
        let echo = super::echo::snapshot();
        let on = super::echo::shown_slot(self.capture_slot.get(), &echo)?;
        let c = echo[on];
        (c.w != 0 && c.h != 0).then_some((on, c.x, c.y))
    }

    pub(crate) fn follow_guest_echo(&self, primary_view: &NSView) {
        if !self.is_captured() {
            return;
        }
        // A sweep in flight is moving the guest's cursor on purpose, and none of it is the
        // hand's doing. Following it re-bases the captured cursor onto the sweep's own steps:
        // the park then warps across displays mid-sweep (which reads as leaving the window,
        // and drops the grab), and the position the sweep restores to is wherever its last
        // step happened to land rather than where the user left the pointer.
        if self.probe.get().is_some() {
            return;
        }
        let echo = super::echo::snapshot();
        let was = self.capture_slot.get();
        let Some(on) = super::echo::shown_slot(was, &echo) else {
            return;
        };
        let c = echo[on];
        if c.w == 0 || c.h == 0 {
            return;
        }
        let mut gate = self.echo_seen.get();
        if !gate.adopt((on, c.x, c.y)) {
            return;
        }
        self.echo_seen.set(gate);
        let Some((_, fit)) = self.slot_surface(on, primary_view) else {
            return;
        };
        if fit.w <= 0.0 || fit.h <= 0.0 {
            return;
        }
        let point = super::fit::fit_point_of_pixel((c.x, c.y), (c.w, c.h), fit);
        if on != was {
            log::info!(
                "display: the guest's cursor crossed to slot {on} at ({},{}) of {}x{} (was slot {was}); the captured cursor follows it",
                c.x,
                c.y,
                c.w,
                c.h,
            );
            self.capture_slot.set(on);
        }
        self.capture_pos.set(Some(point));
    }

    /// The ownership facts about every guest window — THE snapshot
    /// ([`super::grab_policy::WindowFacts`]): assembled here and nowhere else, so every
    /// judgment that takes something away from the user reads the same answers. Primary first
    /// and always present (a momentarily windowless primary view reports every fact false
    /// except the extend-panel fullscreen, which does not live on the window).
    pub(crate) fn window_facts(
        &self,
        primary_view: &NSView,
    ) -> Vec<super::grab_policy::WindowFacts> {
        let primary_slot = self.primary_slot.get() as usize;
        let mut out = Vec::new();
        let w = primary_view.window();
        out.push(super::grab_policy::WindowFacts {
            slot: primary_slot,
            primary: true,
            key: w.as_ref().is_some_and(|w| w.isKeyWindow()),
            on_active_space: w.as_ref().is_some_and(|w| w.isOnActiveSpace()),
            has_screen: w.as_ref().is_some_and(|w| w.screen().is_some()),
            // The `notch = extend` panel is fullscreen with no fullscreen style bit — a
            // borderless overlay window, not a Space.
            fullscreen: self.overlay_active.load(Ordering::Relaxed)
                || w.is_some_and(|w| w.styleMask().contains(NSWindowStyleMask::FullScreen)),
        });
        for slot in super::windows::hosted_slots() {
            if slot == primary_slot {
                continue; // the primary's registry entry; its facts were pushed above
            }
            let Some((window, _)) = super::windows::window_of_slot(slot) else {
                continue;
            };
            out.push(super::grab_policy::WindowFacts {
                slot,
                primary: false,
                key: window.isKeyWindow(),
                on_active_space: window.isOnActiveSpace(),
                has_screen: window.screen().is_some(),
                // A covering secondary is a native fullscreen Space; under `notch = extend`
                // the carrier is borderless with no fullscreen style bit and the claimed band
                // is the tell — the same two shapes the primary's line above folds together.
                fullscreen: window.styleMask().contains(NSWindowStyleMask::FullScreen)
                    || super::windows::band_active(slot),
            });
        }
        out
    }

    /// The per-slot facts the seam rule reads ([`super::seams`]): each slot's share of the
    /// device range, the host panel its window covers, and whether that window is a fullscreen
    /// guest on the Space the user is actually looking at.
    ///
    /// The panel comes from the window's own screen rather than from the display table,
    /// because the question is "what is the hand looking at on that panel" and the answer has
    /// to agree with the window facts it is paired with.
    fn seam_facts(&self, primary_view: &NSView) -> Vec<super::seams::SlotFacts> {
        let shares = super::arrangement::range_shares(f64::from(ABS_MAX));
        let panels = super::hostdisplay::active_displays();
        let panel_of = |window: Option<&NSWindow>| {
            let screen = window?.screen()?;
            let id = super::hostdisplay::display_id_of(&screen);
            let b = panels.iter().find(|(pid, _)| *pid == id)?.1;
            Some(super::seams::PanelRect {
                x0: b.origin.x,
                y0: b.origin.y,
                x1: b.origin.x + b.size.width,
                y1: b.origin.y + b.size.height,
            })
        };
        // Keyed on the LIVE scanouts, not on the windows: a display the guest is driving is one
        // the range can lead to whether or not we have a window for it yet, and the seam rule's
        // refusal of the unplaceable ([`super::seams::Hold::of`]) can only refuse what it is
        // told about. A window whose scanout is dead is left out — nothing can cross into it.
        let facts = self.window_facts(primary_view);
        super::echo::scanout_sizes()
            .into_iter()
            .enumerate()
            .filter(|&(_, (w, h))| w != 0 && h != 0)
            .map(|(slot, (w, h))| {
                let f = facts.iter().find(|f| f.slot == slot);
                let window = match f {
                    Some(f) if f.primary => primary_view.window(),
                    Some(_) => super::windows::window_of_slot(slot).map(|(w, _)| w),
                    None => None,
                };
                super::seams::SlotFacts {
                    slot,
                    share: shares.iter().find(|(s, _)| *s == slot).map(|(_, r)| *r),
                    panel: panel_of(window.as_deref()),
                    covered: f.is_some_and(|f| f.fullscreen && f.on_active_space && f.has_screen),
                    pixels: (f64::from(w), f64::from(h)),
                }
            })
            .collect()
    }

    /// Whether `slot`'s window is on its panel's active Space — the captured stepper's
    /// visibility mask, read off one facts snapshot. A slot with no window (no facts entry) is
    /// not visible either.
    fn slot_visible(facts: &[super::grab_policy::WindowFacts], slot: usize) -> bool {
        facts.iter().any(|f| f.slot == slot && f.on_active_space)
    }

    /// The window showing this slot: its view and the guest picture's rect within it. The
    /// primary window's half of the per-slot registry
    /// ([`super::windows::window_of_slot`] is the other).
    fn slot_surface(
        &self,
        slot: usize,
        primary_view: &NSView,
    ) -> Option<(Retained<NSView>, super::fit::FitRect)> {
        if slot == self.primary_slot.get() as usize {
            return Some((primary_view.retain(), self.primary_fit()));
        }
        let (window, fit) = super::windows::window_of_slot(slot)?;
        Some((window.contentView()?, fit))
    }

    /// Every guest window this tick — the primary first, then each secondary.
    fn guest_surfaces(
        &self,
        primary_view: &NSView,
    ) -> Vec<(usize, Retained<NSView>, super::fit::FitRect)> {
        // Only windows on their panel's ACTIVE Space. View conversion is affine and knows
        // nothing about Spaces, so a guest window swiped away still "contains" every point on
        // its panel — a phantom surface. That phantom made the grab's edge release refuse the
        // seam to a hidden display (`releasable` saw guest content where the user sees a macOS
        // workspace) and would seed grabs and continue drags into a display nobody can see. A
        // window that is not on glass is not under the pointer, full stop.
        let facts = self.window_facts(primary_view);
        let mut out = Vec::new();
        if Self::slot_visible(&facts, self.primary_slot.get() as usize) {
            out.push((
                self.primary_slot.get() as usize,
                primary_view.retain(),
                self.primary_fit(),
            ));
        }
        for slot in super::windows::hosted_slots() {
            // The primary's entry was pushed above (first, deliberately — resolution order);
            // its registry registration would list it twice.
            if slot == self.primary_slot.get() as usize {
                continue;
            }
            if !Self::slot_visible(&facts, slot) {
                continue;
            }
            if let Some((window, fit)) = super::windows::window_of_slot(slot) {
                if let Some(view) = window.contentView() {
                    out.push((slot, view, fit));
                }
            }
        }
        out
    }

    /// Resolve a CG global point to the guest window whose content it is over. Containment is
    /// judged against each window's fit, never assumed — view conversion is affine and happily
    /// returns coordinates for a point over some other display entirely, which is exactly how
    /// the old regime confined the grab to the primary panel.
    pub(crate) fn guest_surface_at_global(
        &self,
        p: NSPoint,
        primary_view: &NSView,
    ) -> Option<SurfaceHit> {
        self.guest_surfaces(primary_view)
            .into_iter()
            .find_map(|(slot, view, fit)| {
                let point = cg_global_to_view_point(&view, p)?;
                super::fit::point_in_fit(point.0, point.1, fit).then_some(SurfaceHit {
                    slot,
                    fit,
                    point,
                })
            })
    }

    /// Would a click here actually land on the guest, or on something macOS put in front of it?
    ///
    /// Geometry cannot answer this. A fullscreen guest window covers its whole panel, so the
    /// revealed menu bar, an open menu hanging down over the guest's picture, a notification, a
    /// panel from another app — every one of them is *inside* the guest's fit rect while being
    /// the thing the user is actually clicking. Treating those as guest clicks is what made the
    /// menus unusable: reaching for Displays re-took the pointer on the way, and clicking the
    /// item took it again. The window server already knows the answer it will give the click, so
    /// ask it, and accept only our own guest windows (the notch overlay is ours too, and is
    /// chrome — it must not count).
    ///
    /// `true` when the marker is unavailable: off the main thread this cannot be asked, and the
    /// conservative answer is the one that leaves behaviour as it was.
    pub(crate) fn guest_is_topmost_at(&self, loc: NSPoint, primary_view: &NSView) -> TopMost {
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            return TopMost { hit: 0, ours: true };
        };
        // CG global is top-left origin; the window server takes NS screen space (bottom-left).
        let h = unsafe { CGDisplayBounds(CGMainDisplayID()) }.size.height;
        let ns = NSPoint::new(loc.x, h - loc.y);
        let hit = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(ns, 0, mtm);
        let ours: Vec<isize> = self
            .guest_surfaces(primary_view)
            .iter()
            .filter_map(|(_, view, _)| view.window().map(|w| w.windowNumber()))
            .collect();
        let guest = hit != 0 && ours.contains(&hit);
        if super::capture_tap::edge_trace() {
            eprintln!(
                "[HITTEST] t={:.1} loc=({:.1},{:.1}) hit={hit} guestwindows={ours:?} guest={guest}",
                super::capture_tap::trace_ms(),
                loc.x,
                loc.y,
            );
        }
        // Nothing asks this question unless the pointer is doing something over the guest, so
        // having no guest window to compare against means the hit test cannot answer correctly
        // and every click will read as "the user went to macOS". A real fault, not a state.
        if ours.is_empty() {
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::warn!(
                    "pointer capture: asked which window a click at ({:.0},{:.0}) would hit while no guest window is on screen — the grab cannot be judged",
                    loc.x,
                    loc.y
                );
            }
        }
        TopMost { hit, ours: guest }
    }

    /// As [`Self::guest_surface_at_global`], for the pointer's live position (a query to the
    /// window server, current even when no event reached us — see [`live_pointer_in_view`]).
    fn live_over_guest(&self, primary_view: &NSView) -> Option<SurfaceHit> {
        self.guest_surfaces(primary_view)
            .into_iter()
            .find_map(|(slot, view, fit)| {
                let point = live_pointer_in_view(&view)?;
                super::fit::point_in_fit(point.0, point.1, fit).then_some(SurfaceHit {
                    slot,
                    fit,
                    point,
                })
            })
    }

    /// Resolve a CG global point to the guest window on the PANEL it is on — the chrome
    /// reveal's targeting rule: the ask (and the band it lowers) belongs to the panel the
    /// pointer is on, whatever window the pointer happens to be inside there (the menu bar it
    /// reveals sits above every window's fit, so fit-containment would release the ask at the
    /// exact moment the user reaches what they asked for). Over no guest window's panel, the
    /// primary's conversion stands in: its far-outside point can only release, never arm —
    /// and releasing is the behavior leaving every guest panel must have.
    pub(crate) fn reveal_target_at_global(
        &self,
        loc: NSPoint,
        primary_view: &NSView,
    ) -> (usize, (f64, f64), super::fit::FitRect) {
        // CG global is top-left origin; NSScreen frames are NS screen space (bottom-left,
        // shared origin with the primary display) — the same flip `view_point_to_cg_global`
        // does, inverted.
        let h = unsafe { CGDisplayBounds(CGMainDisplayID()) }.size.height;
        let ns = NSPoint::new(loc.x, h - loc.y);
        let contains = |f: NSRect, eps: f64| {
            ns.x >= f.origin.x - eps
                && ns.x < f.origin.x + f.size.width + eps
                && ns.y >= f.origin.y - eps
                && ns.y < f.origin.y + f.size.height + eps
        };
        // STICKY to the gesture's owner, judged with a small margin, before strict containment
        // decides. Load-bearing for a panel whose top edge is a SEAM (another display above):
        // pushing at that top pins the pointer exactly ON the shared boundary — macOS's own
        // menu-bar hold — and a boundary point fails the lower panel's strict `y < top`, so
        // every event of the actual lean was attributed to the panel ABOVE, whose distant top
        // silently cleared the charge each time (rig, 2026-08-20: the internal's reveal never
        // armed while the primary's worked — the difference was the seam, not the window).
        let owner = self.reveal.get().owner();
        let surfaces = self.guest_surfaces(primary_view);
        for pass in 0..2 {
            for (slot, view, fit) in &surfaces {
                if (pass == 0) != (*slot == owner) {
                    continue;
                }
                let Some(f) = view.window().and_then(|w| w.screen()).map(|s| s.frame()) else {
                    continue;
                };
                if contains(f, if pass == 0 { 2.0 } else { 0.0 }) {
                    if let Some(p) = cg_global_to_view_point(view, loc) {
                        return (*slot, p, *fit);
                    }
                }
            }
        }
        // Over no guest window's panel: the primary's conversion stands in — its far-outside
        // point can only release, never arm, and releasing is what leaving every guest panel
        // must do.
        let p = cg_global_to_view_point(primary_view, loc).unwrap_or((f64::MIN, f64::MIN));
        (self.primary_slot.get() as usize, p, self.primary_fit())
    }

    /// The virtual cursor in its window ([`Projection`]): the window showing `capture_slot`,
    /// and the cursor clamped into that window's fit (a fit that changed under it — a
    /// fullscreen toggle mid-capture — re-pins it silently). `None` when nothing ever placed
    /// the cursor or its slot has no window this tick.
    fn capture_projection(&self, primary_view: &NSView) -> Option<Projection> {
        let pos = self.capture_pos.get()?;
        let slot = self.capture_slot.get();
        let (view, fit) = self.slot_surface(slot, primary_view)?;
        if fit.w <= 0.0 || fit.h <= 0.0 {
            return None;
        }
        Some(Projection {
            slot,
            view,
            fit,
            point: super::fit::capture_step(Some(pos), 0.0, 0.0, fit).pos,
        })
    }

    /// The virtual cursor's CG global position via its window — the default release warp
    /// target, with the expectation that it lands on that window's displays.
    fn capture_to_global(&self, primary_view: &NSView) -> Option<(NSPoint, super::warp::Aim)> {
        let pr = self.capture_projection(primary_view)?;
        let global = view_point_to_cg_global(&pr.view, pr.point)?;
        let aim = super::warp::Aim {
            stage: "release",
            slot: pr.slot,
            displays: pr
                .view
                .window()
                .map(|w| super::hostdisplay::displays_under_window(&w))
                .unwrap_or_default(),
        };
        Some((global, aim))
    }

    /// Send the virtual cursor's current absolute position (seeding it if nothing ever
    /// placed it) — the captured-mode analogue of the position-with-press staleness guard.
    fn send_captured_pos(&self, view: &NSView) {
        self.captured_step_and_emit(0.0, 0.0, view);
    }

    /// Translate one scroll event for the guest. Shared with the capture tap, which bridges its
    /// `CGEvent` to an `NSEvent` to get here — captured and uncaptured scrolling must not be two
    /// different translations.
    pub(crate) fn emit_scroll(&self, event: &NSEvent) {
        self.cancel_ungrab_chord();
        // Trackpads and Magic Mice report precise point deltas (momentum-phase events
        // included, so guest kinetic decay comes free); those flow through the v120
        // accumulators for pixel-smooth guest scrolling. Physical wheels don't, and keep
        // the legacy one-notch-per-event mapping inside `ScrollAxis::step`.
        let precise = event.hasPreciseScrollingDeltas();
        let dy = event.scrollingDeltaY();
        // Natural macOS scroll: right swipe = negative dx; REL_HWHEEL right = +1.
        let dx = -event.scrollingDeltaX();
        let mut any =
            self.emit_scroll_axis(&self.scroll_y, dy, precise, REL_WHEEL_HI_RES, REL_WHEEL);
        any |= self.emit_scroll_axis(&self.scroll_x, dx, precise, REL_HWHEEL_HI_RES, REL_HWHEEL);
        if any {
            self.send_ptr(InputEvent::syn());
        }
    }

    /// Step one scroll axis and emit its dual-rate wheel events: the hi-res v120 delta for
    /// every input, and the legacy detent event whenever the accumulation crosses a notch
    /// (libinput ignores the latter on a hi-res device; older guest stacks need it).
    fn emit_scroll_axis(
        &self,
        axis: &Cell<ScrollAxis>,
        delta: f64,
        precise: bool,
        hi_res_code: u16,
        detent_code: u16,
    ) -> bool {
        let mut a = axis.get();
        let (v120, detents) = a.step(delta, precise);
        axis.set(a);
        if v120 != 0 {
            self.send_ptr(InputEvent::new(EV_REL, hi_res_code, v120));
        }
        if detents != 0 {
            self.send_ptr(InputEvent::new(EV_REL, detent_code, detents));
        }
        v120 != 0 || detents != 0
    }

    fn send_kbd(&self, ev: InputEvent) {
        // Snapshot the current worker's endpoints and hold the Arc across the send, so a
        // reboot relaunch can't close (or let the OS reuse) the fd mid-write.
        let io = self.conn.io();
        send_event(io.kbd_fd(), ev);
    }

    /// Send to the absolute-pointer device — both modes drive it now (captured mode moves a
    /// virtual cursor through the same mapping). The relative-mouse device carries only the
    /// edge-clamped overflow ([`send_edge_overflow`]). Same snapshot-held-across-the-send
    /// rule as `send_kbd`.
    fn send_ptr(&self, ev: InputEvent) {
        if wire_trace() {
            eprintln!(
                "[WIRE] t={} dev=abs type={} code={} value={}",
                wire_now_us(),
                ev.type_,
                ev.code,
                ev.value
            );
        }
        let io = self.conn.io();
        send_event(io.ptr_fd(), ev);
    }
}

/// `LIMINA_POINTER_WIRE_TRACE`: log every pointer event actually written to the guest's
/// absolute and relative devices, stamped with wallclock microseconds. The guest clock is
/// host-anchored (PL031 + TimeSync), so a guest-side recording of where the compositor put
/// the cursor can be correlated event-for-event; `spikes/pointer-units-oracle/` is the
/// consumer.
pub(crate) fn wire_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIMINA_POINTER_WIRE_TRACE").is_some_and(|v| v != "0"))
}

fn wire_now_us() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0)
}

/// Forward clamped-off captured motion to the guest's relative-mouse device as edge
/// *pressure*. mutter's pressure barriers (GNOME's hot corner) fire on motion pushed INTO a
/// barrier while the pointer is pinned — something a pre-clamped absolute stream can never
/// express, which is why the hot corner went dead in capture mode. Away from edges the
/// overflow is zero and the device stays silent; when it does fire, the resulting relative
/// motion cannot drift the guest cursor, because the compositor clamps it at the same screen
/// edge the absolute position is already pinned to.
pub(crate) fn send_edge_overflow(conn: &Arc<WorkerConn>, overflow: (f64, f64)) {
    let dx = overflow.0.round() as i32;
    let dy = overflow.1.round() as i32;
    if dx == 0 && dy == 0 {
        return;
    }
    if wire_trace() {
        eprintln!("[WIRE] t={} dev=rel dx={dx} dy={dy}", wire_now_us());
    }
    // Snapshot rule as everywhere: hold the Arc so the fd stays open across the sends.
    let io = conn.io();
    let fd = io.rel_ptr_fd();
    if dx != 0 {
        send_event(fd, InputEvent::new(EV_REL, REL_X, dx));
    }
    if dy != 0 {
        send_event(fd, InputEvent::new(EV_REL, REL_Y, dy));
    }
    send_event(fd, InputEvent::syn());
}

/// One bit per mouse button, in two masks that answer different questions.
///
/// `guest` is a ledger of what the guest has been **told**: [`Self::released`] forwards a
/// release only for a button whose bit is still set, which is what keeps a press the guest
/// never saw from producing a release it cannot match.
///
/// `tap` is what the physical button is **doing**, as the capture tap sees it — the question
/// the sweep asks. While captured the tap consumes the events the local monitor would have
/// used, so `guest` reads 0 through an entire captured drag and the sweep, which refuses to
/// borrow the pointer while a button is down, was blind to exactly the case it exists for.
/// macOS's sticky drag makes that reachable without any contrivance: the button stays
/// logically held while the hand leaves the trackpad to reposition, so the "hand is quiet"
/// gate opens mid-drag.
///
/// The two must never be merged. Writing the tap's answer into `guest` broke uncaptured
/// clicking outright — every click, not some: the tap runs BEFORE AppKit delivers, so on the
/// release it cleared the bit `emit_release` was about to test and the release was swallowed.
/// One click left the guest's button held down forever, an implicit pointer grab on whatever
/// was under it, after which nothing else on the desktop reacted (measured: seven BTN_LEFT
/// writes on the wire, all `value=1`, not one release among them).
///
/// The bit encoding is shared with [`btn_bit`] (1 left, 2 right, 4 middle).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct ButtonLedger {
    guest: u8,
    tap: u8,
}

/// Which guest cursor echoes the captured estimate may re-base itself from.
///
/// While captured the guest is the authority on where its own cursor is — it clamps, it
/// constrains, it may refuse to go where we put it — so the estimate follows the echo
/// (`InputState::follow_guest_echo`). Each position is adopted once: the echo repeats at the
/// scanout's rate whether or not anything moved, and re-adopting a position the estimate has
/// already taken would pin it there against the hand.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct EchoGate {
    seen: Option<(usize, i32, i32)>,
}

impl EchoGate {
    /// The grab engages while the guest's cursor is at `now` — take that as already adopted.
    ///
    /// The engage seeds the captured cursor from where the host pointer really is and tells
    /// the guest to put its cursor there ([`InputState::toggle_capture_to`]). Until the guest
    /// answers, what it echoes is the position it held *before* the grab, and adopting that
    /// is how the pointer got dragged back across a macOS Space switch: the hand had moved
    /// while away, the guest had not, and one tick was enough to throw the hand's position
    /// away. Holding it as seen means the next CHANGE is the guest arriving where we sent it.
    fn engaged(now: Option<(usize, i32, i32)>) -> Self {
        Self { seen: now }
    }

    /// May the captured estimate be re-based onto this echo? Once per position.
    fn adopt(&mut self, key: (usize, i32, i32)) -> bool {
        if self.seen == Some(key) {
            return false;
        }
        self.seen = Some(key);
        true
    }
}

impl ButtonLedger {
    /// The guest is being told about a press.
    fn pressed(self, bit: u8) -> Self {
        Self {
            guest: self.guest | bit,
            ..self
        }
    }

    /// A release: forward it only if the guest was told about the matching press.
    fn released(self, bit: u8) -> (Self, bool) {
        (
            Self {
                guest: self.guest & !bit,
                ..self
            },
            self.guest & bit != 0,
        )
    }

    /// What the capture tap observed, kept out of the guest's ledger.
    fn noted_by_tap(self, bit: u8, down: bool) -> Self {
        Self {
            tap: if down {
                self.tap | bit
            } else {
                self.tap & !bit
            },
            ..self
        }
    }

    /// Whether the guest is holding a button it was told about — "this drag continues a press
    /// the guest saw", which is a different question from [`Self::any_down`].
    fn guest_holds(self) -> bool {
        self.guest != 0
    }

    /// Whether any button is down by either reckoning — the question the sweep asks.
    fn any_down(self) -> bool {
        self.guest | self.tap != 0
    }
}

impl InputState {
    /// Note a button's state from the CAPTURE TAP, into the tap's own mask.
    /// See [`ButtonLedger`] for why the two masks stay apart.
    pub(crate) fn note_tap_button(&self, bit: u8, down: bool) {
        self.buttons.set(self.buttons.get().noted_by_tap(bit, down));
    }
}

fn btn_bit(btn: u16) -> u8 {
    match btn {
        BTN_LEFT => 1,
        BTN_RIGHT => 2,
        BTN_MIDDLE => 4,
        _ => 0,
    }
}

pub(crate) fn send_event(fd: RawFd, ev: InputEvent) {
    let bytes = ev.to_bytes();
    // One datagram per event. The fd is non-blocking (set in the supervisor) so a full
    // socket can never block the AppKit main thread and freeze the whole UI — we drop the
    // event instead. With the generous socket buffers we set, EAGAIN only happens if the
    // worker has stopped draining (e.g. its input thread died), so warn loudly: it points
    // at a real stall rather than normal backpressure.
    let n = unsafe { libc::send(fd, bytes.as_ptr() as *const libc::c_void, bytes.len(), 0) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EAGAIN) | Some(libc::ENOBUFS) => {
                log::warn!("input: dropped event {ev:?} — worker socket full (worker stalled?)")
            }
            _ => log::trace!("input send failed: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_taps_button_never_touches_the_guests_release_pairing() {
        // The break this pins (2026-08-22): the tap wrote its button observation into the
        // guest's own ledger. The tap runs BEFORE AppKit delivers, so on the release it
        // cleared the bit `emit_release` was about to test — every uncaptured release was
        // swallowed, and one click left the guest holding the button forever, an implicit
        // pointer grab under which the rest of the desktop went dead.
        let l = ButtonLedger::default();

        // A whole uncaptured click, in the order the two paths actually see it.
        let l = l.noted_by_tap(1, true); // tap first...
        let l = l.pressed(1); // ...then the monitor forwards the press
        let l = l.noted_by_tap(1, false); // tap sees the release first
        let (l, forward) = l.released(1); // the monitor must still forward it
        assert!(
            forward,
            "the tap's release must not disarm the guest's own release"
        );
        assert!(!l.guest_holds(), "and the ledger clears with it");
        assert!(!l.any_down(), "with nothing left down anywhere");
    }

    #[test]
    fn a_release_the_guest_was_never_told_about_is_not_forwarded() {
        // A press that landed outside the guest's content is not in the ledger, so its
        // release must not reach the guest either — a release with no press is a button the
        // guest can only get wrong.
        let l = ButtonLedger::default().noted_by_tap(1, true);
        let (l, forward) = l.released(1);
        assert!(!forward, "no press was forwarded, so no release is");
        assert!(!l.guest_holds());
    }

    #[test]
    fn a_captured_drag_is_visible_to_the_sweep_though_the_guest_ledger_is_empty() {
        // While captured the tap consumes the press, so the monitor never records it. The
        // sweep must still see a button down — this is the sticky-drag case that summoned a
        // sweep into the middle of a window drag.
        let l = ButtonLedger::default().noted_by_tap(1, true);
        assert!(l.any_down(), "the sweep sees the physical button");
        assert!(
            !l.guest_holds(),
            "but it is not a press the guest was told about"
        );
        assert!(!l.noted_by_tap(1, false).any_down());
    }

    /// A pointer whose view left the screen gets the arrow back, from any wear.
    ///
    /// The fault (measured 2026-08-22): switching macOS Spaces away from a captured guest, the
    /// pointer kept the transparent blank — `inside` only moves on motion and a Space switch
    /// produces none, so the release re-asserted a shape that was blank, or nothing at all.
    /// The pointer was then invisible on the Space the user had just arrived at, until some
    /// other app happened to set a cursor. Both starting states are the real ones: `inside`
    /// still true because no motion said otherwise, and already false with the capture's blank
    /// on top.
    #[test]
    fn a_view_that_left_the_screen_hands_the_pointer_back_wearing_the_arrow() {
        let mut w = WearState::new();
        w.on_motion(true);
        w.on_set_captured(true);
        assert_eq!(
            w.on_reassert(),
            None,
            "captured, the release's own re-assert offers nothing"
        );
        w.on_set_captured(false);
        assert_eq!(
            w.on_view_gone(),
            Some(Wear::Arrow),
            "the guest's Space is gone — the pointer is over macOS now"
        );
        assert_eq!(
            w.on_reassert(),
            None,
            "and it is no longer inside anything to re-assert into"
        );
    }

    #[test]
    fn the_arrow_comes_back_even_when_the_machine_already_thought_it_was_outside() {
        // `inside` false and the blank worn is the other half of the same fault: `on_motion`
        // would return None here (nothing to restore from), so the wear would simply stay.
        let mut w = WearState::new();
        assert_eq!(w.on_motion(false), None, "not inside, nothing to change");
        assert_eq!(w.on_view_gone(), Some(Wear::Arrow));
    }

    /// Where the guest's cursor was before the grab is not an answer to the grab.
    ///
    /// The fault (measured 2026-08-22, dogfood + rig): coming back from another macOS Space,
    /// the grab is retaken and seeds the captured cursor from where the pointer REALLY is —
    /// and then the very next tick threw that away for the position the guest had been
    /// echoing all along, from before the switch. The hand had moved 420 pt while away; the
    /// pointer resumed where it had been before, which is the jarring part: the guest's
    /// cursor is the thing the user is looking at, and it did not come to meet them.
    #[test]
    fn a_grab_does_not_adopt_where_the_guests_cursor_was_before_it() {
        let before = (0usize, 1346, 680);
        let mut gate = EchoGate::engaged(Some(before));
        assert!(
            !gate.adopt(before),
            "the position the guest held through the whole switch is stale by construction — \
             it cannot be an answer to a seed we have only just sent"
        );
    }

    #[test]
    fn the_guests_answer_to_the_seed_is_adopted() {
        // …and the moment the guest actually moves, the estimate follows it again: that is
        // what makes the captured cursor track a guest that clamps or constrains.
        let mut gate = EchoGate::engaged(Some((0, 1346, 680)));
        assert!(
            gate.adopt((0, 766, 1182)),
            "the guest arrived where we sent it"
        );
        assert!(
            !gate.adopt((0, 766, 1182)),
            "and each position is still taken only once — the echo repeats every frame"
        );
    }

    #[test]
    fn a_grab_with_no_cursor_to_go_stale_adopts_the_first_echo() {
        // A guest showing no cursor at all (hidden, or a slot that has not painted one) gives
        // the engage nothing to hold against, so the first echo that arrives is fresh news.
        let mut gate = EchoGate::engaged(None);
        assert!(gate.adopt((0, 100, 100)));
    }

    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    const SHIFT: u64 = 1 << 17;
    const KC_F: u16 = 0x03;
    const KC_A: u16 = 0x00;

    #[test]
    fn captured_host_pointer_wears_the_blank_whatever_the_guest_sends() {
        // The ghost-cursor class (2026-08-19): AppKit unhid the cursor behind the hide
        // refcount during a window reconfiguration, and the parked pointer reappeared wearing
        // the LIVE guest shape — because shape updates kept dressing it mid-capture. The rule:
        // while captured the pointer wears the transparent blank, whatever arrives; the guest
        // shape is remembered and comes back on release.
        let mut w = WearState::new();
        assert_eq!(
            w.on_motion(true),
            Some(Wear::Blank),
            "default wear is blank"
        );
        assert_eq!(
            w.on_update(false),
            Some(Wear::Stored),
            "uncaptured, a guest shape is worn on arrival"
        );
        assert_eq!(
            w.on_set_captured(true),
            Some(Wear::Blank),
            "capture dresses the pointer in the blank"
        );
        assert_eq!(
            w.on_update(false),
            None,
            "a shape arriving mid-capture is stored, never worn"
        );
        assert_eq!(
            w.on_motion(true),
            Some(Wear::Blank),
            "motion re-asserts the blank while captured"
        );
        assert_eq!(w.on_set_captured(false), None);
        assert_eq!(
            w.on_reassert(),
            Some(Wear::Stored),
            "release re-wears the remembered guest shape"
        );
    }

    #[test]
    fn wear_state_tracks_the_view_boundary() {
        let mut w = WearState::new();
        assert_eq!(w.on_motion(false), None, "outside stays untouched");
        w.on_motion(true);
        w.on_update(false);
        assert_eq!(
            w.on_motion(false),
            Some(Wear::Arrow),
            "leaving the view restores the arrow"
        );
        assert_eq!(
            w.on_motion(true),
            Some(Wear::Stored),
            "re-entry re-wears the stored guest shape"
        );
        w.on_update(true);
        assert_eq!(
            w.on_reassert(),
            Some(Wear::Blank),
            "a guest-hidden cursor reasserts as blank"
        );
    }

    #[test]
    fn cmd_ctrl_f_is_the_fullscreen_toggle() {
        assert_eq!(
            match_host_shortcut(KC_F, COMMAND | CONTROL),
            Some(HostShortcut::ToggleFullScreen)
        );
    }

    #[test]
    fn fullscreen_combo_requires_exactly_cmd_and_ctrl() {
        // Missing either modifier → not a host shortcut (goes to the guest).
        assert_eq!(match_host_shortcut(KC_F, COMMAND), None);
        assert_eq!(match_host_shortcut(KC_F, CONTROL), None);
        // Extra modifiers → pass through (so Cmd-Ctrl-Opt-F etc. reach the guest).
        assert_eq!(match_host_shortcut(KC_F, COMMAND | CONTROL | OPTION), None);
        assert_eq!(match_host_shortcut(KC_F, COMMAND | CONTROL | SHIFT), None);
    }

    #[test]
    fn cmd_ctrl_g_is_the_capture_toggle() {
        assert_eq!(
            match_host_shortcut(0x05, COMMAND | CONTROL), // G
            Some(HostShortcut::ToggleCapture)
        );
    }

    #[test]
    fn other_keys_with_cmd_ctrl_are_not_intercepted() {
        // Cmd-Ctrl-A must still reach the guest.
        assert_eq!(match_host_shortcut(KC_A, COMMAND | CONTROL), None);
    }

    #[test]
    fn bare_f_goes_to_the_guest() {
        assert_eq!(match_host_shortcut(KC_F, 0), None);
    }

    #[test]
    fn ungrab_chord_arms_on_exactly_ctrl_opt_and_fires_on_break() {
        // Ctrl alone: nothing.
        assert_eq!(ungrab_chord_step(false, CONTROL), (false, false));
        // Ctrl+Opt: armed, no fire yet.
        assert_eq!(ungrab_chord_step(false, CONTROL | OPTION), (true, false));
        // One of them lifts: fire.
        assert_eq!(ungrab_chord_step(true, CONTROL), (false, true));
        // Or both lift at once: fire.
        assert_eq!(ungrab_chord_step(true, 0), (false, true));
    }

    #[test]
    fn ungrab_chord_disarms_when_another_modifier_joins() {
        // Cmd joins the chord: disarm, and the later break must NOT fire.
        assert_eq!(
            ungrab_chord_step(true, CONTROL | OPTION | COMMAND),
            (false, false)
        );
        // Shift prevents arming in the first place.
        assert_eq!(
            ungrab_chord_step(false, CONTROL | OPTION | SHIFT),
            (false, false)
        );
        // Breaking an unarmed chord is inert.
        assert_eq!(ungrab_chord_step(false, CONTROL), (false, false));
    }

    #[test]
    fn ungrab_chord_rearms_after_a_disarm_once_exactly_ctrl_opt_again() {
        // Cmd joined (disarmed), then Cmd lifted leaving exactly Ctrl+Opt: armed again.
        assert_eq!(ungrab_chord_step(false, CONTROL | OPTION), (true, false));
    }

    #[test]
    fn arming_edge_is_withheld_so_the_ungrab_gesture_never_reaches_the_guest() {
        // The reported bug: Control is already held (Ctrl-arrow workspace switching) when the
        // window takes focus and soft-grabs, so the guest never saw Control go down. Pressing
        // Option to free the grab used to forward a LONE Alt press to the guest.
        assert_eq!(
            ungrab_chord_action(false, CONTROL | OPTION),
            (true, UngrabAction::Withhold)
        );
        // Releasing either key fires the ungrab; the withheld Option press is dropped, not sent.
        assert_eq!(
            ungrab_chord_action(true, CONTROL),
            (false, UngrabAction::Fire)
        );
    }

    #[test]
    fn a_non_chord_modifier_edge_still_forwards() {
        // Control alone (nothing armed) goes straight to the guest...
        assert_eq!(
            ungrab_chord_action(false, CONTROL),
            (false, UngrabAction::Forward)
        );
        // ...as does the edge that disarms an armed chord (Cmd joining Ctrl+Opt): the withheld
        // Option press is replayed by the caller before this one, so Cmd-Ctrl-Alt-<key> is intact.
        assert_eq!(
            ungrab_chord_action(true, CONTROL | OPTION | COMMAND),
            (false, UngrabAction::Forward)
        );
    }

    #[test]
    fn precise_scroll_maps_one_detents_worth_of_points_to_exactly_120() {
        let mut a = ScrollAxis::default();
        let (v120, detents) = a.step(SCROLL_POINTS_PER_DETENT, true);
        assert_eq!((v120, detents), (120, 1));
        // And the accumulators are back at zero: no residue after an exact detent.
        assert_eq!(a.step(SCROLL_POINTS_PER_DETENT, true), (120, 1));
    }

    #[test]
    fn precise_scroll_carries_rounding_residue_so_slow_drags_lose_nothing() {
        // 53 one-point steps must add up to exactly one detent's worth of v120 despite
        // each step rounding to an integer (120/53 ≈ 2.264 per point).
        let mut a = ScrollAxis::default();
        let mut total_v120 = 0;
        let mut total_detents = 0;
        for _ in 0..53 {
            let (v120, detents) = a.step(1.0, true);
            total_v120 += v120;
            total_detents += detents;
        }
        assert_eq!(total_v120, 120);
        assert_eq!(total_detents, 1);
    }

    #[test]
    fn precise_scroll_negative_direction_mirrors_positive() {
        let mut a = ScrollAxis::default();
        assert_eq!(a.step(-SCROLL_POINTS_PER_DETENT, true), (-120, -1));
        // Partial negative motion: v120 flows, no detent until -120 accumulates.
        let mut b = ScrollAxis::default();
        let (v120, detents) = b.step(-10.0, true);
        assert!(v120 < 0);
        assert_eq!(detents, 0);
    }

    #[test]
    fn direction_reversal_drains_the_detent_accumulator_without_a_phantom_notch() {
        // Scroll almost a notch down, then back up the same amount: net zero, and no
        // detent may fire in either direction.
        let mut a = ScrollAxis::default();
        let (_, d1) = a.step(20.0, true);
        let (_, d2) = a.step(-20.0, true);
        assert_eq!((d1, d2), (0, 0));
        assert_eq!(a.detent_acc, 0);
    }

    #[test]
    fn wheel_scroll_keeps_one_notch_per_event_in_both_rates() {
        // Non-precise (physical wheel) deltas are device-scaled line counts; the legacy
        // behavior — one notch per event in the delta's direction — is preserved, now
        // expressed as ±120 hi-res plus the ±1 detent.
        let mut a = ScrollAxis::default();
        assert_eq!(a.step(0.1, false), (120, 1));
        assert_eq!(a.step(-3.0, false), (-120, -1));
    }

    #[test]
    fn zero_delta_is_inert() {
        let mut a = ScrollAxis::default();
        assert_eq!(a.step(0.0, true), (0, 0));
        assert_eq!(a.step(0.0, false), (0, 0));
    }

    /// The bug F1: a second display's window has its own content rect, and an event from it
    /// must be judged against THAT rect. Judged against the primary's fit — which is what the
    /// motion and scroll gates did — the point falls outside on every arrangement and the
    /// event is dropped, so a second display took clicks (whose gate was already per-window)
    /// and no hover or scroll at all.
    #[test]
    fn a_point_on_another_panel_is_inside_its_own_display() {
        let primary_fit = super::super::fit::FitRect {
            x: 0.0,
            y: 0.0,
            w: 2560.0,
            h: 1440.0,
        };
        // A window on a panel to the LEFT of the primary: its own points are 0..1512, but the
        // same pointer converted into the primary's space is negative.
        let secondary_fit = super::super::fit::FitRect {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 948.0,
        };
        let in_its_own_window = Target::resolve(1, false, 700.0, 400.0, secondary_fit);
        assert!(
            in_its_own_window.inside,
            "the pointer is over its own display"
        );
        assert!(!in_its_own_window.primary);

        let same_pointer_in_primary_space = Target::resolve(0, true, -812.0, 400.0, primary_fit);
        assert!(
            !same_pointer_in_primary_space.inside,
            "and is nowhere near the primary's content, which is why that gate dropped it"
        );
    }

    /// The rect is the guest's picture, not the window: under `notch = avoid` a covered panel
    /// gives the camera-housing band back, and the pointer over that band is over macOS, not
    /// over the guest.
    #[test]
    fn the_housing_band_is_not_inside_the_guest() {
        // Bottom-left origin: the guest occupies y 0..948 of a 982-point panel, band on top.
        let fit = super::super::fit::FitRect {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 948.0,
        };
        assert!(Target::resolve(1, false, 700.0, 947.0, fit).inside);
        assert!(
            !Target::resolve(1, false, 700.0, 960.0, fit).inside,
            "in the housing band, above the guest's picture"
        );
    }

    /// The unit position is measured within the display's own content, so the far edge of a
    /// secondary is 1.0 for that display — the report mapping (`arrangement::abs_through_report`)
    /// turns it into that display's share of the range, and would be wrong relative to anything else.
    #[test]
    fn the_unit_position_is_within_the_displays_own_content() {
        let fit = super::super::fit::FitRect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 500.0,
        };
        let t = Target::resolve(1, false, 1000.0, 500.0, fit);
        assert_eq!(t.unit.0, 1.0);
        assert_eq!(t.unit.1, 0.0, "top-left origin: the top is v=0");
        let mid = Target::resolve(1, false, 500.0, 250.0, fit);
        assert_eq!(mid.unit, (0.5, 0.5));
    }

    /// F2: only the primary's events may drive the side effects, and the flag that decides it
    /// is carried rather than re-derived at each use.
    #[test]
    fn only_the_primary_carries_the_side_effect_flag() {
        let fit = super::super::fit::FitRect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        assert!(Target::resolve(0, true, 50.0, 50.0, fit).primary);
        assert!(!Target::resolve(3, false, 50.0, 50.0, fit).primary);
    }
}
