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
use objc2_app_kit::{NSCursor, NSEvent, NSEventType, NSView};
use objc2_foundation::{NSPoint, NSRect};

use limina_input::constants::{
    ABS_MAX, ABS_X, ABS_Y, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_ABS, EV_KEY, EV_REL, REL_HWHEEL,
    REL_HWHEEL_HI_RES, REL_WHEEL, REL_WHEEL_HI_RES, REL_X, REL_Y,
};
use limina_input::keymap::{
    capslock_on, macos_keycode_to_linux_remapped, modifier_emit, CapsLockSync, KeyRemap, ModEmit,
    MACOS_KC_CAPSLOCK,
};
use limina_input::InputEvent;

// CoreGraphics (already linked): pointer-capture / mouselook primitives.
//   - `CGAssociateMouseAndMouseCursorPosition(0)` asks the HID layer to stop driving the cursor
//     from the mouse. It's unreliable on its own (the cursor still drifted onto windows behind us
//     on macOS 26), so we ALSO re-pin the cursor to the display centre on every captured move.
//   - `CGWarpMouseCursorPosition` does the re-pin; crucially `NSEvent.deltaX/deltaY` come from the
//     mouse, unaffected by warping, so re-centring never corrupts the motion we integrate.
// `connected` is a `boolean_t` (C `int`); 0 = decoupled (captured), 1 = normal.
extern "C" {
    fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
    fn CGWarpMouseCursorPosition(point: NSPoint) -> i32;
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

/// Apply the host-cursor side of a capture transition: on grab, decouple the HW mouse, park the
/// cursor at centre, and hide it; on release, re-couple, warp the cursor to `release_to` (where
/// the captured virtual cursor ended, so leaving capture is as seamless as entering it), show it,
/// and re-assert the guest shape. Shared by the local-monitor toggle
/// ([`InputState::toggle_capture`]) and the capture tap, so the two stay byte-identical. Main
/// thread only (NSCursor hide/unhide must balance).
pub(crate) fn apply_capture_cursor(
    on: bool,
    host_cursor: &HostCursor,
    release_to: Option<NSPoint>,
    park: Option<NSPoint>,
) {
    if on {
        unsafe {
            CGAssociateMouseAndMouseCursorPosition(0);
            // `park` is a point inside our own window (see `fit::park_point`), so this warp is
            // zero-length and injects no phantom motion. `None` is the degraded local-monitor
            // path, which has no view to compute one from.
            CGWarpMouseCursorPosition(park.unwrap_or_else(main_display_center));
        }
        // Idempotent: NSCursor hide/unhide is a counter — a double-hide (e.g. a stray double
        // toggle) would need a matching double-unhide or the cursor stays gone forever. Only
        // hide if we haven't already, so the count never drifts.
        if !CURSOR_HIDDEN.swap(true, Ordering::AcqRel) {
            NSCursor::hide();
        }
        log::info!("pointer capture: ON (Cmd-Ctrl-G to release)");
    } else {
        unsafe {
            // Warp BEFORE re-associating, and while still hidden: the cursor *appears* at the
            // release point rather than visibly jumping there from the park, and — the part that
            // was a real bug — the hardware cannot move it in between. Associating first made the
            // mouse live while the cursor was still parked, so motion already in flight was
            // applied from the park point; with the park on another display that put the pointer
            // on the wrong screen entirely, raising that screen's Dock instead of the guest's dash.
            if let Some(p) = release_to {
                CGWarpMouseCursorPosition(p);
            }
            CGAssociateMouseAndMouseCursorPosition(1);
        }
        // A warp opens a 0.25 s local-events suppression interval, and this one is applied at the
        // exact moment the user gets the pointer back — so every release ate up to a quarter
        // second of the motion that followed it. Re-associating closes the interval; the
        // `CGAssociateMouseAndMouseCursorPosition(1)` above cannot do it, because the warp comes
        // after. Latent since capture shipped, and load-bearing for the fullscreen grab, whose
        // whole release story is "the motion continues naturally".
        end_warp_suppression();
        if CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
            NSCursor::unhide();
        }
        host_cursor.reassert();
        log::info!("pointer capture: OFF");
    }
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

/// Re-attach the hardware mouse to the cursor.
///
/// Called right after a warp, for a reason that has nothing to do with association: a warp also
/// opens a **local events suppression interval** — 0.25 s by default — during which real mouse
/// movement no longer moves the cursor. Re-associating ends it immediately. Without this, edge
/// resistance felt like it ate travel: after pushing against an edge you had to move a long way
/// back before the cursor unstuck, because a quarter-second of your motion was being discarded
/// by the window server. Harmless when the mouse is already associated, which is the only state
/// the uncaptured path can be in (capture disassociates, and resistance does not run captured).
pub(crate) fn end_warp_suppression() {
    unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
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

/// Whether we currently have the host cursor hidden for capture — keeps `NSCursor::hide`/`unhide`
/// (a reference count) balanced no matter how the toggle is driven.
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

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

/// The macOS virtual keycodes of every held-modifier key (left/right pairs of
/// Command/Shift/Option/Control) — the set force-released at an ungrab boundary.
const MODIFIER_KEYCODES: [u16; 8] = [0x37, 0x36, 0x38, 0x3C, 0x3A, 0x3D, 0x3B, 0x3E];

/// The host pointer's adoption of the guest cursor (main thread only). `cursor` is what
/// the macOS pointer should look like over the guest view (the guest's current cursor
/// image, or a blank cursor while the guest hides it); `inside` tracks whether the pointer
/// is currently over the view so shape updates from the worker apply immediately.
pub struct HostCursor {
    cursor: RefCell<Retained<NSCursor>>,
    inside: Cell<bool>,
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
        })
    }

    /// Adopt a new guest cursor shape (or blank). Takes effect now if the pointer is over
    /// the view, otherwise on the next entry.
    pub fn update(&self, cursor: Retained<NSCursor>) {
        if self.inside.get() {
            cursor.set();
        }
        *self.cursor.borrow_mut() = cursor;
    }

    /// Re-assert the guest cursor shape if the pointer is currently over the view (used when
    /// leaving pointer-capture mode, where we hid the cursor).
    fn reassert(&self) {
        if self.inside.get() {
            self.cursor.borrow().set();
        }
    }

    /// Track the pointer crossing the view boundary. Re-asserts the guest cursor on every
    /// inside motion (AppKit can reset the cursor behind our back — resize edges, window
    /// transitions); restores the arrow when leaving.
    fn on_motion(&self, inside: bool) {
        if inside {
            self.cursor.borrow().set();
        } else if self.inside.get() {
            NSCursor::arrowCursor().set();
        }
        self.inside.set(inside);
    }
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
    guest_buttons: Cell<u8>,
    host_cursor: Rc<HostCursor>,
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
    /// The virtual cursor position in view points (bottom-left origin, the fit rect's space).
    /// Uncaptured motion keeps it at the pointer's last position over the content, so a grab
    /// starts exactly where the cursor was; captured motion integrates deltas into it
    /// ([`fit::capture_step`]); a release warps the host cursor back to it. `None` until the
    /// pointer has ever been placed (then capture seeds at the content centre). Shared with the
    /// capture tap (same main thread → `Rc<Cell>`).
    capture_pos: Rc<Cell<Option<(f64, f64)>>>,
    /// Where the guest scanout currently sits inside the content view, written by the render
    /// path every tick (dynamic mode: the full view — the legacy mapping; host/fixed: the
    /// letterboxed fit rect). The absolute-pointer transform and the inside-gate go through
    /// it, so the pointer can never disagree with the pixels. Same main thread → `Rc<Cell>`.
    fit: Rc<Cell<super::fit::FitRect>>,
    /// Where the hidden host cursor is parked while captured, in CG global coordinates — a point
    /// inside our own window, chosen at grab time. See [`super::fit::park_point`].
    park: Cell<Option<NSPoint>>,
    /// Hi-res scroll accumulators (vertical, horizontal) — see [`ScrollAxis`].
    scroll_y: Cell<ScrollAxis>,
    scroll_x: Cell<ScrollAxis>,
    /// Whether the guest is hosted in the `notch = extend` overlay, and the flag that asks for it
    /// to stand down so the menu bar and the window's controls are reachable. See
    /// [`InputState::reveal_step`].
    overlay_active: Arc<AtomicBool>,
    reveal_chrome: Arc<AtomicBool>,
    /// The chrome ask's charge: seconds of actual pushing against the guest's top edge, plus the
    /// distance floor. See [`super::fit::Charge`], which the grab's edge release shares.
    reveal: Cell<super::fit::Charge>,
    /// When the ask was last granted, for the settle window in [`InputState::reveal_step`].
    reveal_granted: Cell<Option<std::time::Instant>>,
    /// Where the pointer was on the previous reveal event **from each source**, to tell real
    /// motion from a reflow — see the discontinuity check in [`InputState::reveal_step`], and
    /// [`RevealSrc`] for why this cannot be one shared slot.
    reveal_pos: [Cell<Option<(f64, f64)>>; 2],
}

/// Which path fed a reveal event. Both can be live at once and they do NOT agree about where
/// the pointer is — traces caught them reporting `y = 982` and `y = 40` for one stationary
/// pointer, same delta, same fit. Until that is explained, each keeps its own continuity state so
/// neither makes the other look like it teleported, and the trace says which one spoke.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevealSrc {
    /// The session event tap (`capture_tap`), via CG global coordinates.
    Tap,
    /// The local `NSEvent` monitor, via `locationInWindow` — plain AppKit, no global round trip.
    Monitor,
}

impl RevealSrc {
    fn idx(self) -> usize {
        self as usize
    }

    fn tag(self) -> &'static str {
        match self {
            Self::Tap => "tap",
            Self::Monitor => "mon",
        }
    }
}

/// How much *pushing* the pointer must do at the top edge to ask for the chrome back, measured as
/// time actually spent in motion rather than wall-clock since the gesture began.
///
/// **Time, not distance, is the currency here.** Distance rewards a hard shove, and a hard shove
/// is exactly what throwing the pointer at the top-left hot corner looks like — so the menu bar
/// kept appearing while the user was reaching for the GNOME overview. Duration separates them
/// cleanly: flinging into a corner is over in a moment, while asking for the chrome is a deliberate
/// lean. It also makes the gesture feel the same whether the mouse is fast or slow, which
/// accumulated points never did.
/// The accumulation itself is [`super::fit::Charge`], shared with the fullscreen grab's edge
/// release; only the threshold and the policy around it are here.
///
/// 0.25 s is measured, not guessed: in a recording of the intended gestures
/// (`spikes/edge-pressure/analyze-trace.py`) the corner pushes reached a peak charge of 0.021
/// while a chrome lean reached 1.046 — a 50x separation, so the threshold only has to sit
/// somewhere sane inside it. It is chosen at the *felt* end of that range: once the lean is
/// under way the chrome arrives after almost exactly this long.
const REVEAL_HOLD: f64 = 0.25;

/// How long after granting the ask a release is ignored, to let the overlay's teardown settle.
/// Long enough to cover the re-layout, far too short to cover a deliberate move back into the
/// guest across [`REVEAL_MARGIN`].
const REVEAL_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

/// A floor on the push, so jitter against the top row can't satisfy [`REVEAL_HOLD`] by resting.
/// Small: the hold is what makes the gesture deliberate, this only rules out noise.
const REVEAL_PUSH: f64 = 40.0;

/// How close to a side edge counts as "in a corner", where the reveal never arms at all.
///
/// Corners belong to the guest — the top-left one is the GNOME overview trigger, and pushing into
/// it necessarily pushes upward too. Matching `fit::CORNER_ZONE` keeps the two gestures from
/// overlapping by construction rather than by tuning.
const REVEAL_CORNER_KEEPOUT: f64 = super::fit::CORNER_ZONE;

/// How far back below the top edge the pointer must come before the overlay is taken back. Wide
/// enough that using the revealed menu bar doesn't flicker it away, narrow enough to feel prompt.
pub(crate) const REVEAL_MARGIN: f64 = 40.0;

impl InputState {
    // Eight shared handles, each a distinct thing the translator needs for the window's lifetime;
    // bundling them into a struct would move the same list one line up.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conn: Arc<WorkerConn>,
        host_cursor: Rc<HostCursor>,
        remap: KeyRemap,
        captured: Arc<AtomicBool>,
        fit: Rc<Cell<super::fit::FitRect>>,
        capture_pos: Rc<Cell<Option<(f64, f64)>>>,
        overlay_active: Arc<AtomicBool>,
        reveal_chrome: Arc<AtomicBool>,
    ) -> Self {
        Self {
            conn,
            pressed_mods: RefCell::new(HashSet::new()),
            pressed_keys: RefCell::new(HashSet::new()),
            pressed_aux: RefCell::new(HashSet::new()),
            caps: RefCell::new(CapsLockSync::new()),
            guest_buttons: Cell::new(0),
            host_cursor,
            remap,
            captured,
            ungrab_armed: Cell::new(false),
            ungrab_withheld: RefCell::new(Vec::new()),
            capture_pos,
            fit,
            park: Cell::new(None),
            scroll_y: Cell::new(ScrollAxis::default()),
            scroll_x: Cell::new(ScrollAxis::default()),
            overlay_active,
            reveal_chrome,
            reveal: Cell::new(super::fit::Charge::default()),
            reveal_granted: Cell::new(None),
            reveal_pos: [Cell::new(None), Cell::new(None)],
        }
    }

    /// Where the hidden cursor is parked while captured — the tap re-pins to it after every
    /// motion event, so both paths agree on one point and neither warp moves anything.
    pub(crate) fn park_point(&self) -> NSPoint {
        self.park.get().unwrap_or_else(main_display_center)
    }

    fn is_captured(&self) -> bool {
        self.captured.load(Ordering::Acquire)
    }

    /// Release pointer capture if held (no-op otherwise) — for the parked window (task #18),
    /// where the guest the capture served is gone and a hidden, pinned cursor would leave the
    /// user unable to click the play glyph. Main thread only.
    pub fn release_capture(&self, view: &NSView) {
        if self.is_captured() {
            self.toggle_capture(view);
        }
    }

    /// Toggle pointer capture. On grab: decouple the hardware mouse from the cursor (so deltas
    /// flow while the cursor stays frozen) and hide the cursor. On release: restore both and put
    /// the host cursor where the virtual cursor ended, so the transition is seamless in both
    /// directions. Returns the new captured state. Main thread only.
    pub fn toggle_capture(&self, view: &NSView) -> bool {
        let now = !self.is_captured();
        let release_to = if now {
            None
        } else {
            self.capture_pos
                .get()
                .and_then(|p| view_point_to_cg_global(view, p))
        };
        if now {
            let fit = self.fit.get();
            // Seed the virtual cursor from where the pointer REALLY is before deriving the park,
            // so both the guest cursor and the (zero-length) grab warp start from the truth. Our
            // remembered position lags: the capture tap decides the automatic re-grab *before* the
            // window's monitor has processed the same event, and while the pointer was on another
            // display it was not being updated at all. A stale seed shows up twice over — the guest
            // cursor appears where the pointer *was*, and the park warp covers the difference,
            // injecting it as another jump.
            //
            // Only when the pointer is over the content. Off it (a keyboard grab taken while the
            // pointer is on another display) the remembered virtual position is the better answer:
            // clamping the live one would drag the guest cursor to whichever edge happens to be
            // nearest, and the warp is unavoidably long either way.
            if let Some(live) = live_pointer_in_view(view) {
                if super::fit::point_in_fit(live.0, live.1, fit) {
                    self.capture_pos.set(Some(live));
                }
            }
            let seed = self.capture_pos.get();
            let park = super::fit::park_point(seed, fit);
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
            self.park.set(view_point_to_cg_global(view, park));
        }
        self.captured.store(now, Ordering::Release);
        apply_capture_cursor(now, &self.host_cursor, release_to, self.park.get());
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
            self.release_all_held();
        } else {
            self.release_all_modifiers();
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
    fn release_all_modifiers(&self) {
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
        self.sync_capslock(flags);
        self.emit_key(macos_keycode, down);
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
        self.sync_capslock(flags);
        self.emit_modifier(macos_keycode, flags);
    }

    /// Exit the SOFT keyboard grab (Ctrl+Option while focused but not captured): flush the
    /// modifiers the chord pushed into the guest so nothing stays wedged. The caller mutes
    /// soft mode until the window regains key status.
    pub(crate) fn flush_modifiers(&self) {
        self.release_all_modifiers();
    }

    /// Feed a grabbed-mode `flagsChanged` to the ungrab chord and get the verdict for that edge:
    /// [`UngrabAction::Fire`] (release/mute the grab, consume), [`UngrabAction::Withhold`]
    /// (consume without forwarding — the chord is armed and the edge is still ambiguous), or
    /// [`UngrabAction::Forward`] (forward it to the guest as usual).
    pub(crate) fn observe_ungrab_flags(&self, macos_keycode: u16, flags: u64) -> UngrabAction {
        let (armed, action) = ungrab_chord_action(self.ungrab_armed.get(), flags);
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
                self.cancel_ungrab_chord();
                // The guest kernel autorepeats from key-down state; drop macOS repeats.
                if !event.isARepeat() {
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
                self.emit_modifier(event.keyCode(), flags);
                true
            }
            NSEventType::MouseMoved => {
                if self.is_captured() {
                    self.emit_captured_motion(event);
                    return false;
                }
                let inside = self.pointer_inside(event, view);
                self.host_cursor.on_motion(inside);
                if inside {
                    self.emit_motion(event, view);
                }
                false
            }
            NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDragged => {
                if self.is_captured() {
                    self.emit_captured_motion(event);
                    return false;
                }
                let inside = self.pointer_inside(event, view);
                self.host_cursor.on_motion(inside);
                // A drag continues a press: forward it (clamped) even outside the view,
                // but only if the press itself went to the guest.
                if self.guest_buttons.get() != 0 || inside {
                    self.emit_motion(event, view);
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
                if self.is_captured() || self.pointer_inside(event, view) {
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
    fn pointer_inside(&self, event: &NSEvent, view: &NSView) -> bool {
        // SAFETY: we only run on the main thread (the local event monitor's thread).
        let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };
        if event.window(mtm).is_none() {
            return false;
        }
        // The SAME conversion the emitter uses. This gate used to do its own
        // `view.convertPoint_fromView(event.locationInWindow(), None)` — the exact combination
        // `event_point_in_view` exists to correct — so under `notch = extend`, where the guest view
        // lives in the overlay while events are still delivered to the carrier, the gate judged a
        // point in the wrong window's space and dropped motion the emitter would have mapped
        // correctly. A gate and its emitter disagreeing about where the pointer is means the guest
        // cursor silently stops tracking: dogfood saw 350 ms of it, and the freeze only became
        // visible once the grab stopped inheriting the same stale position (2026-08-03).
        let p = event_point_in_view(event, view);
        let inside = super::fit::point_in_fit(p.x, p.y, self.fit.get());
        if super::capture_tap::edge_trace() {
            let old = view.convertPoint_fromView(event.locationInWindow(), None);
            let old_inside = super::fit::point_in_fit(old.x, old.y, self.fit.get());
            if old_inside != inside || (old.x - p.x).abs() + (old.y - p.y).abs() > 0.5 {
                eprintln!(
                    "[MON] t={:.1} gate p=({:.1},{:.1}) inside={inside} | naive=({:.1},{:.1}) \
                     inside={old_inside}",
                    super::capture_tap::trace_ms(),
                    p.x,
                    p.y,
                    old.x,
                    old.y,
                );
            }
        }
        inside
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
    pub fn release_all_held(&self) {
        let mut mods = self.pressed_mods.borrow_mut();
        let mut keys = self.pressed_keys.borrow_mut();
        let mut aux = self.pressed_aux.borrow_mut();
        if mods.is_empty() && keys.is_empty() && aux.is_empty() {
            return;
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
            }
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
    fn emit_press(&self, event: &NSEvent, view: &NSView, btn: u16) -> bool {
        self.cancel_ungrab_chord();
        if self.is_captured() {
            // Captured: no view gate — the virtual cursor is always over the content. Re-send
            // its position with the press (same staleness guard as the uncaptured path below).
            self.guest_buttons
                .set(self.guest_buttons.get() | btn_bit(btn));
            self.send_captured_pos();
            self.send_ptr(InputEvent::new(EV_KEY, btn, 1));
            self.send_ptr(InputEvent::syn());
        } else if self.pointer_inside(event, view) {
            self.guest_buttons
                .set(self.guest_buttons.get() | btn_bit(btn));
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
        if self.guest_buttons.get() & btn_bit(btn) != 0 {
            self.guest_buttons
                .set(self.guest_buttons.get() & !btn_bit(btn));
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
        let p = event_point_in_view(event, view);
        let fit = self.fit.get();
        // Only when the tap is NOT installed. The tap is session-wide, so when it is up it sees
        // every one of these events and is the gesture's real feed; running both makes the same
        // physical motion arrive twice and the two fight — one granting the ask, the other
        // releasing it on the same event, 6 ms apart. One owner, chosen by which mechanism is
        // actually live: the tap when there is a grant, this when there is not.
        if !super::capture_tap::installed() {
            self.reveal_step((p.x, p.y), event.deltaY(), fit, RevealSrc::Monitor);
        }
        self.capture_pos.set(Some(
            super::fit::capture_step(Some((p.x, p.y)), 0.0, 0.0, fit).pos,
        ));
        let (x, y) = super::fit::abs_through_fit(p.x, p.y, fit, ABS_MAX as i32);
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, x));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, y));
        self.send_ptr(InputEvent::syn());
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
        let over = super::fit::edge_overflow((p.x, p.y), event.deltaX(), event.deltaY(), fit);
        if over != (0.0, 0.0) {
            send_edge_overflow(&self.conn, over);
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
        if !self.overlay_active.load(Ordering::Relaxed) {
            return; // nothing to ask for; the chrome is already reachable
        }
        if !self.reveal_chrome.swap(true, Ordering::Relaxed) {
            self.reveal_granted.set(Some(std::time::Instant::now()));
        }
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
        p: (f64, f64),
        delta_y: f64,
        fit: super::fit::FitRect,
        src: RevealSrc,
    ) {
        if self.captured.load(Ordering::Relaxed) {
            self.reveal.set(super::fit::Charge::default());
            return;
        }
        const EDGE: f64 = 2.0;
        let top = fit.y + fit.h;
        // The recording instrument for this gesture. Every constant below was guessed once and
        // wrong once; `[REVEAL]` exists so the next value comes from a trace of the movement the
        // user actually intends instead. Emitted for the whole top band, tagged with why the call
        // ended, since a gesture that does not fire is exactly the interesting case.
        let trace = |s: &Self, why: &str| {
            if super::capture_tap::edge_trace() {
                eprintln!(
                    "[REVEAL] t={:.1} src={} p=({:.1},{:.1}) dy={delta_y:.1} top={top:.1} \
                     overlaid={} push={:.1} charge={:.3} ask={} {why}",
                    super::capture_tap::trace_ms(),
                    src.tag(),
                    p.0,
                    p.1,
                    s.overlay_active.load(Ordering::Relaxed),
                    s.reveal.get().get().1,
                    s.reveal.get().get().0,
                    s.reveal_chrome.load(Ordering::Relaxed),
                );
            }
        };
        // Ignore the event where the *content moved under the pointer* rather than the pointer
        // moving. Granting the ask drops the overlay, which re-parents the view into the inset
        // carrier, and the reflow is reported as pointer motion — badly. Two measured samples
        // from one boot: `p.y` 982.0 -> 31.9 carrying `dy=+33.0` (exactly the notch inset), and
        // `p.y` 947.9 -> 30.6 carrying `dy=+1.0`, a 917-point jump attributed to a one-point
        // move. Either reads as "back inside the guest", which released the ask, restored the
        // geometry, and let the still-leaning pointer re-arm it: the chrome oscillated for as
        // long as the user kept pushing (seen near the right-hand system menus, where the lean
        // naturally lingers).
        //
        // Real motion moves the pointer by its own delta, so the inconsistency is the tell —
        // and it needs no knowledge of *which* transition happened or what the inset is. The
        // event is dropped, not lapsed: a warp from edge resistance trips this too, and losing
        // a gesture's charge to the app's own cursor bookkeeping would be its own bug.
        if super::fit::is_reflow(self.reveal_pos[src.idx()].replace(Some(p)), p, delta_y) {
            trace(self, "reflow");
            return;
        }
        if p.1 < top - REVEAL_MARGIN {
            // Genuinely back in the guest: forget the push and take the overlay back.
            //
            // Deliberately NOT gated on the overlay being up. Releasing the ask is the only way
            // the overlay ever returns, and the overlay is down *precisely because* the ask is
            // set — gating this on `overlay_active` makes the state unreachable and the guest
            // stays inset below the housing forever, across fullscreen toggles included. Cost a
            // dogfood round; the release condition must always be live.
            if self.reveal_chrome.load(Ordering::Relaxed) {
                // Not within the settle window. Granting the ask drops the overlay, which moves
                // the guest content out from under a stationary pointer; for a few frames the
                // reported position says "back inside the guest" when nothing moved. The user
                // cannot have crossed the whole reveal margin deliberately in that time, so a
                // release this soon is layout, not intent. Without it the ask was granted and
                // withdrawn repeatedly — invisible at speed, but each withdrawal zeroed the
                // accumulated push, so the lean had to be re-earned over and over. That is the
                // "sometimes it takes a lot of pushing" this gesture kept being reported for.
                if self
                    .reveal_granted
                    .get()
                    .is_some_and(|t| t.elapsed() < REVEAL_SETTLE)
                {
                    return;
                }
                trace(self, "release");
            }
            self.reveal.set(super::fit::Charge::default());
            self.reveal_granted.set(None);
            self.reveal_chrome.store(false, Ordering::Relaxed);
            return;
        }
        let lapse = |s: &Self| {
            let mut c = s.reveal.get();
            c.lapse();
            s.reveal.set(c);
        };
        if !self.overlay_active.load(Ordering::Relaxed) {
            // Nothing to ask for: the chrome is already reachable.
            trace(self, "no-overlay");
            lapse(self);
            return;
        }
        // Corners are the guest's. Pushing into the top-left one — the GNOME overview trigger —
        // necessarily pushes upward as well, and while the reveal armed there the menu bar kept
        // appearing when the user was reaching for the overview.
        let in_corner =
            p.0 - fit.x <= REVEAL_CORNER_KEEPOUT || fit.x + fit.w - p.0 <= REVEAL_CORNER_KEEPOUT;
        if p.1 < top - EDGE || in_corner {
            trace(self, if in_corner { "corner" } else { "band" });
            return;
        }
        // AppKit deltas grow downward, so upward push is negative.
        //
        // A delta of exactly zero is NOT "stopped pushing" — it is what the edge reports once the
        // cursor is already pinned there, which is to say the normal state of a push that is
        // working. Lapsing on it made the gesture unperformable: a recorded chrome lean contained
        // 46 of them, every one with `dy == 0.0`, each wiping the charge, so it plateaued at 0.384
        // against a 0.45 s bar and could never fire however long the user leaned.
        //
        // Neutral, though — not charging. Letting a zero delta charge would bank *stillness* as
        // pushing again, which is the shove-then-linger hole that the charge model was introduced
        // to close: a flick worth `REVEAL_PUSH` followed by resting would satisfy the hold with no
        // lean at all. So it neither charges nor lapses, and does not count as recent motion.
        if delta_y == 0.0 {
            trace(self, "pinned");
            return;
        }
        // Only a genuine downward move — the user pulling away — gives up the gesture.
        if delta_y > 0.0 {
            // Moving back into the guest. Travelling along the top row or resting against it must
            // not satisfy the hold by accident, and this is what rules that out.
            trace(self, "not-pushing");
            lapse(self);
            return;
        }
        // Charge by the time actually spent pushing — see `fit::Charge` for why that is the unit
        // and why it survives a trackpad lift.
        let mut c = self.reveal.get();
        // The ask keeps the baseline grace period; only the grab's *side* edges are more forgiving
        // (`fit::edge_timing`). Pushing up at a visible target is not the same gesture as shoving
        // sideways mid-travel, and dogfood likes this one as it is.
        let (charge, push) = c.push(
            std::time::Instant::now(),
            -delta_y,
            super::fit::CHARGE_DECAY,
        );
        self.reveal.set(c);
        if charge >= REVEAL_HOLD
            && push >= REVEAL_PUSH
            && !self.reveal_chrome.swap(true, Ordering::Relaxed)
        {
            self.reveal_granted.set(Some(std::time::Instant::now()));
        }
        trace(self, "push");
    }

    /// Capture mode: integrate the event's motion delta into the virtual cursor and drive the
    /// absolute tablet with it. `deltaX/deltaY` carry the pointer-ballistics-processed motion
    /// the macOS cursor would have made, and the absolute device adds no guest-side
    /// acceleration, so captured movement feels exactly like the host cursor.
    fn emit_captured_motion(&self, event: &NSEvent) {
        // Re-pin the (hidden) host cursor to the display centre so it can't drift onto windows
        // behind us — CGAssociate(false) alone doesn't reliably freeze it. The warp doesn't
        // affect `deltaX/deltaY`, so the virtual cursor still integrates clean motion.
        unsafe { CGWarpMouseCursorPosition(self.park_point()) };
        let fit = self.fit.get();
        let step =
            super::fit::capture_step(self.capture_pos.get(), event.deltaX(), event.deltaY(), fit);
        self.capture_pos.set(Some(step.pos));
        let (x, y) = super::fit::abs_through_fit(step.pos.0, step.pos.1, fit, ABS_MAX as i32);
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, x));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, y));
        self.send_ptr(InputEvent::syn());
        send_edge_overflow(&self.conn, step.overflow);
    }

    /// Send the virtual cursor's current absolute position (seeding it if nothing ever
    /// placed it) — the captured-mode analogue of the position-with-press staleness guard.
    fn send_captured_pos(&self) {
        let fit = self.fit.get();
        let p = super::fit::capture_step(self.capture_pos.get(), 0.0, 0.0, fit).pos;
        self.capture_pos.set(Some(p));
        let (x, y) = super::fit::abs_through_fit(p.0, p.1, fit, ABS_MAX as i32);
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, x));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, y));
        self.send_ptr(InputEvent::syn());
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
        let io = self.conn.io();
        send_event(io.ptr_fd(), ev);
    }
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

/// One bit per mouse button for the forwarded-press mask.
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

    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    const SHIFT: u64 = 1 << 17;
    const KC_F: u16 = 0x03;
    const KC_A: u16 = 0x00;

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
}
