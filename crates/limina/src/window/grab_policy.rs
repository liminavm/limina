// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The fullscreen pointer grab's **policy**, with no AppKit and no CoreGraphics in it.
//!
//! Six rounds of dogfood bugs on this feature, and *not one* was in the geometry — `fit` has been
//! unit-tested from the start and has been right every time. They were all here, in the decisions:
//! when to take the pointer, when to let it go, which position to trust, which threshold applies.
//! That code lived inside the `CGEventTap` callback, where the only way to exercise it was to boot a
//! VM and push a real mouse against a real edge, so every adjustment cost a dogfood round.
//!
//! So the decisions live here, taking values and returning verdicts. The tap ([`super::capture_tap`])
//! is the adapter: it reads CoreGraphics, calls in, and performs whatever comes back. Two things are
//! deliberately parameters rather than lookups, because they are what made the old code untestable:
//!
//!   - **the display arrangement**, as a `reachable` predicate — the release rule ("only let the
//!     pointer go where there is somewhere for it to go") needs to know what is beyond an edge, and
//!     `CGGetDisplaysWithPoint` cannot be asked about a hypothetical two-display Mac;
//!   - **`now`**, so a gesture is a list of timestamped samples instead of a thing you have to
//!     perform by hand at the right speed.
//!
//! The outcomes are `Copy` structs, not `Vec<Action>`: this runs on every motion event, and an
//! allocation per event on the input path is not worth the tidier shape.

use std::time::{Duration, Instant};

use super::fit;

/// A floor on the push, so a fast graze along an edge cannot satisfy the hold. Small — the hold is
/// what makes the gesture deliberate; this only rules out motion that never really pressed.
pub(crate) const GRAB_PUSH: f64 = 24.0;

/// The ownership facts about one guest window — assembled in ONE place
/// ([`super::input::InputState::window_facts`], primary first) and consumed everywhere ownership
/// is judged, so no judgment re-derives its own answer from AppKit.
///
/// `key` and `on_active_space` are *different questions that disagree exactly where the bugs
/// lived*: key status survives a Space switch, so "is this window focused" says yes about a
/// window the user cannot see, while "is this window on screen" is what the input layer needs
/// wherever it takes something away from the user (Space-blindness bit three times: `912e2fe`,
/// grab round 8, `930ceff`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct WindowFacts {
    /// The guest display slot this window shows.
    pub slot: usize,
    /// The primary window — the one that owns key/keyboard routing and the fullscreen mode.
    pub primary: bool,
    /// `isKeyWindow`: focused. Survives a Space switch — never read this alone to decide
    /// whether the window is in front of the user.
    pub key: bool,
    /// `isOnActiveSpace`: this window's Space is the one on screen on its panel.
    pub on_active_space: bool,
    /// The window is on some screen at all (an unplug can leave it with none).
    pub has_screen: bool,
    /// Showing fullscreen. For the primary this is either the real fullscreen Space or the
    /// `notch = extend` panel (a borderless window that carries no fullscreen style bit).
    /// A secondary reports `false` until a consumer needs its covering state.
    pub fullscreen: bool,
}

/// The primary's facts out of one snapshot. The assembler always includes the primary (even
/// with its view momentarily windowless, where every fact but `fullscreen` is false), so the
/// default here is a defensive fallback, not a case.
pub(crate) fn primary_facts(facts: &[WindowFacts]) -> WindowFacts {
    debug_assert!(
        facts.iter().any(|f| f.primary),
        "no primary in the snapshot"
    );
    facts
        .iter()
        .find(|f| f.primary)
        .copied()
        .unwrap_or_default()
}

/// Losing key status hands the pointer back — ANY capture, the explicit hard grab included. A
/// background VM has no claim on the pointer, and under the fullscreen grab this is routine
/// rather than exotic: our own close-policy Ask sheet, a system alert, the Accessibility
/// prompt. Without it the dialog is unclickable and the cursor is hidden.
///
/// Asked of EVERY guest window, not of the primary. It once read `primary.key`, on the stated
/// premise that "a covered secondary never becomes key" — which is false, because clicking a
/// secondary makes it key and the primary therefore not. On a two-display session that turned
/// every click on the secondary into a flip-flop: the click armed and took the grab, and this
/// predicate dropped it again in the same breath, so the guest never saw the press
/// (2026-08-22). The question is whether the keyboard has left the VM altogether, and it has
/// not while any guest window still holds it.
///
/// Two consumers: the tap per-event (latency), the window tick as backstop (the degraded
/// no-tap path sees no events at all once focus is gone).
pub(crate) fn key_loss_releases(captured: bool, facts: &[WindowFacts]) -> bool {
    captured && !facts.iter().any(|f| f.key)
}

/// A POLICY grab lives only in fullscreen. Leaving it — Cmd-Ctrl-F, or the Space being torn
/// down — must hand the pointer back, because the edge-press release is itself gated on
/// fullscreen: without this the pointer would be held in a windowed VM with no gesture that
/// could free it but the chord. Gated on `holding`, unlike [`key_loss_releases`]: an explicit
/// Cmd-Ctrl-G grab is the user's and survives the mode change.
pub(crate) fn fullscreen_exit_releases(
    holding: bool,
    captured: bool,
    primary: &WindowFacts,
) -> bool {
    holding && captured && !primary.fullscreen
}

/// Which path fed a reveal event. Both can be live at once and they do NOT agree about where
/// the pointer is — traces caught them reporting `y = 982` and `y = 40` for one stationary
/// pointer, same delta, same fit. Until that is explained, each keeps its own continuity state so
/// neither makes the other look like it teleported, and the trace says which one spoke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    pub(crate) fn tag(self) -> &'static str {
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
const REVEAL_SETTLE: Duration = Duration::from_millis(200);

/// A floor on the push, so jitter against the top row can't satisfy [`REVEAL_HOLD`] by resting.
/// Small: the hold is what makes the gesture deliberate, this only rules out noise.
const REVEAL_PUSH: f64 = 40.0;

/// How close to a side edge counts as "in a corner", where the reveal never arms at all.
///
/// Corners belong to the guest — the top-left one is the GNOME overview trigger, and pushing into
/// it necessarily pushes upward too. Matching `fit::CORNER_ZONE` keeps the two gestures from
/// overlapping by construction rather than by tuning.
const REVEAL_CORNER_KEEPOUT: f64 = fit::CORNER_ZONE;

/// How far back below the top edge the pointer must come before the overlay is taken back. Wide
/// enough that using the revealed menu bar doesn't flicker it away, narrow enough to feel prompt.
pub(crate) const REVEAL_MARGIN: f64 = 40.0;

/// Everything the chrome-reveal gesture remembers between events. The pointer is on exactly one
/// panel, so at most one gesture (and one granted ask) exists at a time — the charge, continuity
/// and grant fields are singletons and `owner` names whose they are. `Copy`, like [`GrabState`],
/// so the adapter keeps it in one `Cell` whose single writer keeps the primary-ask mirror synced
/// (`InputState::with_reveal`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct RevealState {
    /// Seconds of actual pushing against the guest's top edge, plus the distance floor. See
    /// [`super::fit::Charge`], which the grab's edge release shares.
    charge: fit::Charge,
    /// When the ask was last granted, for the settle window in [`reveal_step`].
    granted: Option<Instant>,
    /// Where the pointer was on the previous reveal event **from each source**, to tell real
    /// motion from a reflow — see the discontinuity check in [`reveal_step`], and [`RevealSrc`]
    /// for why this cannot be one shared slot.
    last_pos: [Option<(f64, f64)>; 2],
    /// The slot whose panel the gesture currently belongs to. A step from a different slot
    /// releases the old owner's ask and adopts the new one, structurally: there is no
    /// cross-slot state to keep consistent.
    owner: usize,
    /// The slot whose chrome ask is granted, if any. Always `None` or `Some(owner)`.
    ask: Option<usize>,
}

impl RevealState {
    /// The slot whose chrome ask is granted, if any.
    pub(crate) fn ask(&self) -> Option<usize> {
        self.ask
    }

    /// The slot whose panel the gesture currently belongs to.
    pub(crate) fn owner(&self) -> usize {
        self.owner
    }

    /// The chrome ask is moot: the primary left fullscreen, so no panel is covered and the
    /// chrome is reachable everywhere. Clears the gesture and the ask, so the next fullscreen
    /// session starts from the overlay. (The owner survives — it names the last panel, not a
    /// live claim.)
    pub(crate) fn moot(&mut self) {
        self.charge = fit::Charge::default();
        self.last_pos = [None; 2];
        self.granted = None;
        self.ask = None;
    }
}

/// Adopt `slot` as the gesture's owner and grant its ask outright — no push served. The
/// captured edge-release (`InputState::grant_chrome`: the same upward press that frees the
/// pointer also asks for the chrome) and the observed-menu-bar trigger
/// (`InputState::menubar_observed`) both grant through here; the push gesture grants inside
/// [`reveal_step`]. Adopting the owner matters: the `reveal_step` that follows must judge THIS
/// grant's settle window, not a stale gesture's. No-op when the slot's ask is already granted.
pub(crate) fn reveal_grant(st: &mut RevealState, slot: usize, now: Instant) {
    if st.ask == Some(slot) {
        return;
    }
    st.owner = slot;
    st.charge = fit::Charge::default();
    st.last_pos = [None; 2];
    st.ask = Some(slot);
    st.granted = Some(now);
}

/// Whether the pointer is at the TOP of this guest picture, judged in the panel's own view
/// space — the observed-menu-bar grant's targeting rule. Judged per panel, deliberately NOT
/// through the gesture's owner-sticky targeting: that observer can fire with no gesture history
/// at all (the owner would be stale), and at a seam-hold the pointer sits on the boundary,
/// which reads as the ABOVE panel's bottom under any containment rule. "At the top of this
/// panel" is unambiguous: the panel above fails it by its whole height. A couple of points of
/// upward tolerance absorbs the boundary rounding.
pub(crate) fn at_panel_top(p: (f64, f64), fit: fit::FitRect) -> bool {
    let top = fit.y + fit.h;
    p.1 >= top - REVEAL_MARGIN && p.1 <= top + 4.0 && p.0 >= fit.x && p.0 < fit.x + fit.w
}

/// One motion event fed to the chrome-reveal gesture.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Reveal {
    pub now: Instant,
    /// The guest window the pointer's panel shows (the ask belongs to the panel, not to
    /// fit-containment — the menu bar it reveals sits above every fit).
    pub slot: usize,
    /// The pointer in that window's view coordinates.
    pub pos: (f64, f64),
    /// AppKit's `deltaY`: grows downward, so upward push is negative.
    pub delta_y: f64,
    pub fit: fit::FitRect,
    pub src: RevealSrc,
    /// A grabbed pointer must never trip the ask; it has its own path (`reveal_grant`).
    pub captured: bool,
    /// Whether `slot`'s window hosts the guest under an active extend overlay.
    pub overlay_active: bool,
}

/// What one reveal step wants traced — the numbers at the moment the decision was made (for
/// "release", before the state was cleared). `None` means a silent step (captured, or a release
/// inside the settle window).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RevealTrace {
    pub why: &'static str,
    pub charge: f64,
    pub push: f64,
    pub ask: Option<usize>,
}

/// Ask for the macOS chrome back, or give it up again.
///
/// Under the `notch = extend` overlay nothing can appear over the guest — that is the point of
/// it — so the menu bar and the window's own controls need a deliberate way in for the VM's
/// menu actions. Sustained upward push at the guest's top edge is the ask; coming back into
/// the guest is the release. Every constant here was guessed once and wrong once; the trace
/// this returns exists so the next value comes from a recording of the movement the user
/// actually intends instead.
pub(crate) fn reveal_step(st: &mut RevealState, s: &Reveal) -> Option<RevealTrace> {
    let snap = |st: &RevealState, why: &'static str| {
        let (charge, push) = st.charge.get();
        Some(RevealTrace {
            why,
            charge,
            push,
            ask: st.ask,
        })
    };
    if s.captured {
        st.charge = fit::Charge::default();
        return None;
    }
    // The pointer is on a different panel than the gesture it last fed: the old owner's
    // gesture is over — release its ask (leaving the panel IS "coming back into the guest")
    // and adopt the new slot. This is what keeps the singleton charge/continuity cells sound
    // with one gesture per pointer: there is no per-slot state to reconcile, a slot change
    // simply starts over.
    if s.slot != st.owner {
        st.charge = fit::Charge::default();
        st.last_pos = [None; 2];
        st.granted = None;
        st.ask = None;
        st.owner = s.slot;
    }
    const EDGE: f64 = 2.0;
    let top = s.fit.y + s.fit.h;
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
    let prev = st.last_pos[s.src.idx()].replace(s.pos);
    if fit::is_reflow(prev, s.pos, s.delta_y) {
        return snap(st, "reflow");
    }
    if s.pos.1 < top - REVEAL_MARGIN {
        // Genuinely back in the guest: forget the push and take the overlay back.
        //
        // Deliberately NOT gated on the overlay being up. Releasing the ask is the only way
        // the overlay ever returns, and the overlay is down *precisely because* the ask is
        // set — gating this on `overlay_active` makes the state unreachable and the guest
        // stays inset below the housing forever, across fullscreen toggles included. Cost a
        // dogfood round; the release condition must always be live.
        let mut out = None;
        if st.ask == Some(s.slot) {
            // Not within the settle window. Granting the ask drops the overlay, which moves
            // the guest content out from under a stationary pointer; for a few frames the
            // reported position says "back inside the guest" when nothing moved. The user
            // cannot have crossed the whole reveal margin deliberately in that time, so a
            // release this soon is layout, not intent. Without it the ask was granted and
            // withdrawn repeatedly — invisible at speed, but each withdrawal zeroed the
            // accumulated push, so the lean had to be re-earned over and over. That is the
            // "sometimes it takes a lot of pushing" this gesture kept being reported for.
            if st
                .granted
                .is_some_and(|t| s.now.duration_since(t) < REVEAL_SETTLE)
            {
                return None;
            }
            out = snap(st, "release");
        }
        st.charge = fit::Charge::default();
        st.granted = None;
        st.ask = None;
        return out;
    }
    if !s.overlay_active {
        // Nothing to ask for: the chrome is already reachable.
        let out = snap(st, "no-overlay");
        st.charge.lapse();
        return out;
    }
    // Corners are the guest's. Pushing into the top-left one — the GNOME overview trigger —
    // necessarily pushes upward as well, and while the reveal armed there the menu bar kept
    // appearing when the user was reaching for the overview.
    let in_corner = s.pos.0 - s.fit.x <= REVEAL_CORNER_KEEPOUT
        || s.fit.x + s.fit.w - s.pos.0 <= REVEAL_CORNER_KEEPOUT;
    if s.pos.1 < top - EDGE || in_corner {
        return snap(st, if in_corner { "corner" } else { "band" });
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
    if s.delta_y == 0.0 {
        return snap(st, "pinned");
    }
    // Only a genuine downward move — the user pulling away — gives up the gesture.
    if s.delta_y > 0.0 {
        // Moving back into the guest. Travelling along the top row or resting against it must
        // not satisfy the hold by accident, and this is what rules that out.
        let out = snap(st, "not-pushing");
        st.charge.lapse();
        return out;
    }
    // Charge by the time actually spent pushing — see `fit::Charge` for why that is the unit
    // and why it survives a trackpad lift. The ask keeps the baseline grace period; only the
    // grab's *side* edges are more forgiving (`fit::edge_timing`). Pushing up at a visible
    // target is not the same gesture as shoving sideways mid-travel, and dogfood likes this
    // one as it is.
    let (charge, push) = st.charge.push(s.now, -s.delta_y, fit::CHARGE_DECAY);
    if charge >= REVEAL_HOLD && push >= REVEAL_PUSH && st.ask != Some(s.slot) {
        st.ask = Some(s.slot);
        st.granted = Some(s.now);
    }
    snap(st, "push")
}

/// Everything the policy remembers between events. `Copy`, so the tap can keep it in one `Cell`
/// and the tests can fork a gesture mid-way.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct GrabState {
    /// Time spent pushing at [`Self::edge`], the currency of every "and mean it" decision.
    charge: fit::Charge,
    /// Which edge the current press is against; changing edges starts a new gesture.
    edge: Option<fit::Edge>,
    /// When the free pointer became [`fit::deep_inside`] the content, for the re-grab dwell.
    inside_since: Option<Instant>,
    /// The user let the pointer go on purpose (Cmd-Ctrl-G or the Ctrl-Opt chord), so the policy
    /// must not take it straight back. Cleared by leaving the guest or regaining key focus.
    user_released: bool,
    /// The *policy* is holding the pointer — an edge press can end it. An explicit hard grab is
    /// not this, and no edge press releases it.
    holding: bool,
}

impl GrabState {
    /// Drop a half-earned press and dwell, so the next gesture starts from nothing. A finished
    /// gesture must leave nothing behind, or the next one fires at once while a fresh one takes the
    /// full hold — the same gesture feeling different every time.
    pub(crate) fn reset_gesture(&mut self) {
        self.charge.lapse();
        self.edge = None;
        self.inside_since = None;
    }

    pub(crate) fn holding(&self) -> bool {
        self.holding
    }

    /// The policy has taken the pointer.
    pub(crate) fn hold(&mut self) {
        self.reset_gesture();
        self.holding = true;
    }

    /// The policy no longer owns the grab — an edge release, leaving fullscreen, or a promotion to
    /// an explicit hard grab.
    pub(crate) fn stop_holding(&mut self) {
        self.reset_gesture();
        self.holding = false;
    }

    pub(crate) fn user_released(&self) -> bool {
        self.user_released
    }

    /// The user released on purpose. `latched` is false for a *promotion* to a hard grab, where the
    /// pointer is still held and the policy must simply stay out of it.
    pub(crate) fn release_by_user(&mut self, latched: bool) {
        self.reset_gesture();
        self.user_released = latched;
        self.holding = false;
    }

    /// Forget an explicit release, so the grab re-arms. Returns whether there was one.
    pub(crate) fn rearm(&mut self) -> bool {
        std::mem::replace(&mut self.user_released, false)
    }
}

/// Whether the **soft keyboard grab** is engaged: keystrokes, system combos included, go to the
/// guest while the mouse stays free.
///
/// `space_visible` is here for the same reason it is on [`Free`], and was missing for the same
/// reason: key status survives a Space switch, so a three-finger-up into Mission Control left the
/// keyboard pointed at a guest that was no longer on screen. `captured` takes precedence because a
/// full capture owns the keyboard through its own path — the two must not both claim it.
pub(crate) fn soft_keyboard_engaged(
    captured: bool,
    enabled: bool,
    is_key: bool,
    muted: bool,
    space_visible: bool,
) -> bool {
    !captured && enabled && is_key && space_visible && !muted
}

/// Whether the ungrab chord should still be recognized **after it has muted the soft keyboard
/// grab** — that is, whether a Control still held from the last fire re-arms it.
///
/// It used to go deaf the instant it fired, and with the Command/Option swap on (the default) that
/// was a bug with teeth: the guest's Super *is* macOS's Option, so "Control held + Super" and the
/// chord are one physical gesture. After the first fire, later presses fell through to the local
/// monitor and reached the guest as a bare Super — GNOME's overview. Reported 2026-08-09; the
/// trace is in `spikes/modifier-drift/`.
///
/// Gated on `muted` rather than merely on "nothing is grabbed", so this claims Control+Option only
/// in the state the chord itself created. Someone running `--no-soft-kbd-grab` with no pointer
/// grab has no chord to repeat, and keeps Control+Option as an ordinary guest combo.
pub(crate) fn chord_survives_mute(
    captured: bool,
    soft: bool,
    muted: bool,
    is_key: bool,
    space_visible: bool,
) -> bool {
    // `is_key`/`space_visible`: this is a SESSION tap, so without them we would eat Control+Option
    // out from under whatever app the user is actually looking at.
    !captured && !soft && muted && is_key && space_visible
}

/// Whether a grab that is *already held* has lost the context that justified it: our Space is no
/// longer the one on screen (Mission Control, a Space switch, an unplug that relocates us), or the
/// window has no screen at all.
///
/// The counterpart to [`Free::space_visible`], which only stops a grab being *taken*. Both are
/// needed: without this the pointer stays parked and hidden through Mission Control; without that
/// the tap simply re-grabs on the next dwell and the two fight at 60 Hz.
///
/// `has_screen` is defensive. The Space bit is what dogfood measured; a window with no screen at
/// all was never observed holding a grab, but it is the same class of "there is nothing on screen
/// that explains why the pointer is gone" and costs one `||`.
///
/// The judged window is the one OWNING the captured cursor ([`capture_owner`]) — the grab's
/// justification is what is under the pointer, not the primary's own Space. A missing owner
/// (its slot has no window this tick) is the same "nothing on screen explains this" class.
pub(crate) fn must_drop_grab(captured: bool, owner: Option<&WindowFacts>) -> bool {
    captured && owner.is_none_or(|w| !w.on_active_space || !w.has_screen)
}

/// The window a held grab is judged against: the one the grab was taken in, which shows the
/// slot the captured cursor lives in. A grab can be taken on any covered panel, and "is there
/// something on screen that explains why the pointer is gone" is a question about *that*
/// panel's window — the primary's Space leaving says nothing about it.
pub(crate) fn capture_owner(facts: &[WindowFacts], capture_slot: usize) -> Option<&WindowFacts> {
    facts.iter().find(|f| f.slot == capture_slot)
}

/// The [`Free`] sample's arming flags — `(fullscreen_and_key, space_visible)` — judged against
/// the guest window the free pointer is over (`hit_slot`), not the primary.
///
/// The hit window answers the per-panel questions: is THIS panel a fullscreen guest, on its
/// active Space. Judging the per-panel half by the primary was the free-path residue of the
/// Space-blindness class: with the primary's panel swiped to a workspace, the still-visible
/// fullscreen guest on the other panel could never re-arm.
///
/// **Key is asked of every guest window, not of the primary.** It once read `primary.key`, on
/// the stated premise that "a covered secondary never becomes key". That premise is false:
/// clicking a secondary makes it key and the primary therefore *not* key, so on a two-display
/// session the first click on the secondary disarmed the free path and every click after it was
/// refused before the window-server hit test was ever spent — the reported "clicks on the
/// built-in display do not grab, and log nothing" (2026-08-22). What the flag is really asking
/// is whether the keyboard is ours at all, and it is ours whenever ANY guest window holds it.
///
/// With no hit (off every guest content) the primary's own facts stand in — on that path the
/// sample can only disarm and time the dwell, so any window's facts would do; the primary's
/// keep the old shape.
pub(crate) fn free_arming(facts: &[WindowFacts], hit_slot: Option<usize>) -> (bool, bool) {
    let primary = primary_facts(facts);
    let w = hit_slot
        .and_then(|s| facts.iter().find(|f| f.slot == s))
        .copied()
        .unwrap_or(primary);
    (
        facts.iter().any(|f| f.key) && w.fullscreen,
        w.on_active_space,
    )
}

/// One motion event with the pointer **free**.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Free {
    pub now: Instant,
    /// The pointer in view coordinates — routinely *outside* the fit, which is the point.
    pub pos: (f64, f64),
    pub fit: fit::FitRect,
    /// Our window is fullscreen and key: whether this pointer is ours to think about at all.
    pub fullscreen_and_key: bool,
    /// Our window's Space is the one on screen. Separate from `fullscreen_and_key` because **key
    /// status survives a Space switch** — the window goes on claiming the pointer from a Space the
    /// user cannot see, which is exactly how the grab used to be taken behind their back.
    pub space_visible: bool,
    /// `[display] edge-resistance` is not `Off`.
    pub grab_enabled: bool,
    pub buttons_down: bool,
    /// This event is a button press: the user's explicit ask for the grab.
    pub click: bool,
}

/// What the free path owes this event.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct FreeOutcome {
    /// Feed the `notch = extend` chrome ask ([`super::input::InputState::reveal_step`]).
    pub ask: bool,
    /// The pointer left the guest, ending an explicit release — worth a log line.
    pub left_guest: bool,
    /// Take the pointer into the grab.
    pub grab: bool,
    /// How long the pointer has been deep inside, for the trace.
    pub inside_for: Option<Duration>,
}

/// Decide the free path for one event, and advance the dwell.
///
/// `on_guest` answers "would a click here land on one of the guest's windows, or on something
/// macOS put in front of it" — the window server's own hit test, injected, and called at most
/// once per event because it is a round trip. Geometry cannot answer it: a fullscreen guest
/// window covers its whole panel, so the menu bar, an open menu hanging down over the guest's
/// picture, and any other app's panel are all *inside* the fit while being what the user is
/// actually clicking. It is about the WINDOW, not the picture — the two differ by the letterbox,
/// and that difference is a third case the click path keeps separate.
///
/// The re-grab hysteresis is the load-bearing part: the pointer must come a real margin back
/// *inside* the content and stay there, with no button down, before the grab retakes it. On a single
/// display a released cursor cannot leave the window at all — fullscreen *is* the screen, and the
/// window server pins it to the top row, which is still window territory — so re-grabbing on mere
/// containment would take the pointer back on the first inward jitter. That is the likeliest way
/// this design ships worse than what it replaces.
pub(crate) fn free_step(st: &mut GrabState, s: &Free, on_guest: impl Fn() -> bool) -> FreeOutcome {
    // `space_visible` joins the ownership test rather than sitting beside it: a pointer moving over
    // a Space we are not on is not ours by any reading, so it owes us neither a grab nor an ask —
    // and the reset below is what stops the dwell it spent there from being banked for our return.
    let duties = fit::edge_duties(s.fullscreen_and_key && s.space_visible, s.grab_enabled);
    if !duties.ask {
        // Not our pointer. Drop any half-earned dwell so the *next* fullscreen session cannot
        // inherit it and grab on its first twitch.
        st.reset_gesture();
        return FreeOutcome::default();
    }
    let mut out = FreeOutcome {
        // Never conditioned on the grab: the ask is a `notch = extend` feature and the overlay is up
        // (or not) regardless of what the preference says. With the grab OFF this is the only route
        // to the menu bar that exists — the third time the ask was lost was by riding on a check
        // that belonged to something else.
        ask: true,
        ..FreeOutcome::default()
    };
    if !duties.grab {
        return out;
    }

    // A click on the guest's content is the explicit ask for the grab — the one VM convention
    // every user knows — and nothing refuses it: not the margin, not the dwell, not an earlier
    // explicit release (the click IS the user coming back; on a fullscreen-everything rig no
    // other signal ever arrives, since the pointer can never leave guest content and the
    // window never loses key).
    //
    // A click on anything else is the opposite statement, and is taken as one: the user is
    // working in macOS, so the auto re-grab stands down until they ask for the guest again.
    // Without that, using the menus was a fight — the walk back down toward the guest re-took
    // the pointer, and the click on the menu item took it too.
    if s.click {
        if !on_guest() {
            st.user_released = true;
            st.reset_gesture();
            out.left_guest = true;
        } else if fit::point_in_fit(s.pos.0, s.pos.1, s.fit) {
            st.user_released = false;
            st.hold();
            out.grab = true;
        }
        // The third case is a click on our own window but off the guest's picture — the
        // letterbox a fitted mode leaves, and the band a secondary window leaves above the
        // guest. It is not an ask for the grab and it is emphatically not the user leaving for
        // macOS, so it does neither: standing the grab down there made clicking near the
        // picture's edge silently disarm the re-grab, which read as "clicking stopped working".
        return out;
    }

    if fit::deep_inside(s.pos, s.fit) {
        if st.inside_since.is_none() {
            st.inside_since = Some(s.now);
        }
    } else {
        st.inside_since = None;
    }
    out.inside_for = st.inside_since.map(|t| s.now.duration_since(t));

    // Taking the pointer out of the guest ends an explicit release. The latch has to persist while
    // the pointer stays inside — or Cmd-Ctrl-G in fullscreen would be undone by the re-grab a
    // quarter second later, and could not do the one thing it is for. But keying its end only on a
    // focus round trip made the grab feel permanently broken after one Cmd-Ctrl-G: on a fullscreen
    // VM there is nothing else on that display to click, so the regain edge never comes. Leaving
    // and returning is the same intent expressed with the mouse.
    if !fit::point_in_fit(s.pos.0, s.pos.1, s.fit) && st.rearm() {
        out.left_guest = true;
    }
    // `on_guest` last: it is a window-server round trip, and only worth spending once the dwell
    // and the margin have already said yes. A pointer resting deep inside the guest's rect but
    // under an open menu is not in the guest, and must not be taken back mid-menu.
    if !st.user_released
        && fit::may_regrab(s.pos, s.fit, out.inside_for, s.buttons_down)
        && on_guest()
    {
        st.hold();
        out.grab = true;
    }
    out
}

/// The guest just gained screen: hand it the pointer, latch and dwell notwithstanding.
///
/// Distinct from the click's ask only in what triggers it. Both are the user saying "the guest
/// is what I am using now", and both must clear an earlier explicit release — otherwise going
/// fullscreen right after a click on the menus (which is exactly how "Use Other Screens When
/// Fullscreen" is reached) comes up with the pointer free and the re-grab stood down.
impl GrabState {
    pub(crate) fn take_by_policy(&mut self) {
        self.user_released = false;
        self.hold();
    }
}

/// How much of the machine the guest is showing on, as the trigger for the grab above reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScreenGain {
    /// The primary window is showing fullscreen (a real fullscreen Space, or the `notch =
    /// extend` panel).
    pub fullscreen: bool,
    /// Guest windows on a Space that is on screen — one per panel the guest has taken.
    pub covering: usize,
}

/// Read the gain out of a window-facts snapshot.
pub(crate) fn screen_gain(facts: &[WindowFacts]) -> ScreenGain {
    ScreenGain {
        fullscreen: primary_facts(facts).fullscreen,
        covering: facts
            .iter()
            .filter(|f| f.on_active_space && f.has_screen)
            .count(),
    }
}

/// Did this transition hand the guest more of the machine?
///
/// Two events, one meaning: going fullscreen, and a panel joining a session that is already
/// fullscreen ("Use Other Screens When Fullscreen", or plugging a display in). `None` is the
/// first observation of a session — a starting state is not a transition, or every launch
/// would grab the pointer before the user had asked for anything.
pub(crate) fn gained_screen(was: Option<ScreenGain>, now: ScreenGain) -> bool {
    let Some(was) = was else {
        return false;
    };
    now.fullscreen && (!was.fullscreen || now.covering > was.covering)
}

/// One motion event with the pointer **grabbed**.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Press {
    pub now: Instant,
    /// The VIRTUAL cursor after the clamp, which is what makes this simple: it is exactly at the
    /// edge while the deltas keep flowing, so a sustained push is a stream of genuinely-pushing
    /// events with no "pinned, zero delta" case to special-case (the uncaptured chrome ask, driven
    /// by a cursor the window server pins, has to).
    pub pos: (f64, f64),
    pub delta: (f64, f64),
    pub fit: fit::FitRect,
    pub buttons_down: bool,
    /// The configured hold, in seconds; 0 disables the grab entirely.
    pub hold: f64,
    pub side: fit::SideTuning,
    pub fullscreen: bool,
}

/// What a completed edge press earns. Not every press lets the pointer go.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Release {
    /// Free the pointer and move it just past the edge, onto the display that is there.
    Out((f64, f64)),
    /// Free the pointer where it is, and ask for the macOS chrome — the top edge only.
    InPlaceForChrome,
}

/// A press in progress, for the trace. Its absence means this event was not a press at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pressing {
    pub edge: fit::Edge,
    pub charge: f64,
    pub push: f64,
    pub hold: f64,
    pub decay: f64,
}

/// What the grabbed path owes this event.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PressOutcome {
    pub pressing: Option<Pressing>,
    /// Let the pointer go, this way.
    pub release: Option<(fit::Edge, Release)>,
}

/// Charge the edge press this motion represents, and answer with the release it has earned.
///
/// `reachable` answers "is there a display at this view point" — the arrangement, injected. The rule
/// it enforces is the one dogfood asked for after watching the first cut throw the cursor onto a
/// display that was neither above nor below anything: **the pointer is only let go where there is
/// somewhere for it to go.** The top is the exception, because a fullscreen window's top edge IS the
/// top of the screen — there is never anything above it — yet pushing up is how the macOS chrome is
/// asked for, so it releases in place.
pub(crate) fn press_step(
    st: &mut GrabState,
    s: &Press,
    reachable: impl Fn((f64, f64)) -> bool,
) -> PressOutcome {
    // Only the policy's grab. An explicit Cmd-Ctrl-G is an unconditional hold — see
    // `GrabState::holding` — and the chord is its way out.
    if !st.holding || s.hold <= 0.0 || !s.fullscreen {
        return PressOutcome::default();
    }
    // Mid-drag never releases: dragging a guest window against an edge would otherwise ungrab and
    // drop it on the next display. A press only counts once it starts after button-up.
    if s.buttons_down {
        st.charge.lapse();
        return PressOutcome::default();
    }
    let Some(edge) = fit::pressed_edge(s.pos, s.delta.0, s.delta.1, s.fit) else {
        // Inside the content, or travelling along an edge rather than into it.
        st.charge.lapse();
        return PressOutcome::default();
    };
    // Changing edges starts a new gesture rather than inheriting the old one's charge.
    if st.edge.replace(edge) != Some(edge) {
        st.charge = fit::Charge::default();
    }
    let push = match edge {
        fit::Edge::Left | fit::Edge::Right => s.delta.0.abs(),
        fit::Edge::Top | fit::Edge::Bottom => s.delta.1.abs(),
    };
    // The top edge and the sides are different gestures, judged differently.
    let (hold, decay) = fit::edge_timing(s.hold, edge, s.side);
    let (charge, pushed) = st.charge.push(s.now, push, decay);
    let mut out = PressOutcome {
        pressing: Some(Pressing {
            edge,
            charge,
            push: pushed,
            hold,
            decay,
        }),
        release: None,
    };
    if charge < hold || pushed < GRAB_PUSH {
        return out;
    }
    let release = match edge {
        fit::Edge::Top => Some(Release::InPlaceForChrome),
        _ => {
            let p = fit::release_point(s.pos, edge, s.fit);
            reachable(p).then_some(Release::Out(p))
        }
    };
    match release {
        Some(release) => out.release = Some((edge, release)),
        // Earned it — but a press against a dead edge earns nothing, and must not sit there fully
        // charged waiting to fire the instant the pointer slides somewhere releasable. Lapse
        // instead, so sliding along an edge into a reachable stretch starts a fresh, deliberate
        // press.
        None => st.charge.lapse(),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> fit::FitRect {
        // The dogfood Mac's built-in panel, fullscreen: the content IS the screen.
        fit::FitRect {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 982.0,
        }
    }

    /// Every direction reachable — a Mac surrounded by displays.
    fn anywhere(_: (f64, f64)) -> bool {
        true
    }

    /// Nothing beyond any edge — a single-display Mac.
    fn nowhere(_: (f64, f64)) -> bool {
        false
    }

    fn press(pos: (f64, f64), delta: (f64, f64), now: Instant) -> Press {
        Press {
            now,
            pos,
            delta,
            fit: screen(),
            buttons_down: false,
            hold: crate::vmlib::schema::EdgeHold::Standard.seconds(),
            side: fit::SideTuning::default(),
            fullscreen: true,
        }
    }

    /// Hold the pointer against `edge` for `n` events `dt_ms` apart, each shoving `dist` points,
    /// and answer with the release it earned (if any) plus the last press state.
    fn lean(
        st: &mut GrabState,
        edge: fit::Edge,
        n: u32,
        dt_ms: u64,
        dist: f64,
        reachable: impl Fn((f64, f64)) -> bool + Copy,
    ) -> (Option<Release>, Option<Pressing>) {
        let t0 = Instant::now();
        let fit = screen();
        let (pos, delta) = match edge {
            fit::Edge::Left => ((fit.x, 500.0), (-dist, 0.0)),
            fit::Edge::Right => ((fit.x + fit.w, 500.0), (dist, 0.0)),
            fit::Edge::Top => ((700.0, fit.y + fit.h), (0.0, -dist)),
            fit::Edge::Bottom => ((700.0, fit.y), (0.0, dist)),
        };
        let mut last = None;
        for i in 1..=n {
            let s = press(pos, delta, t0 + Duration::from_millis(dt_ms * u64::from(i)));
            let out = press_step(st, &s, reachable);
            last = out.pressing;
            if let Some((got, release)) = out.release {
                assert_eq!(got, edge);
                return (Some(release), last);
            }
        }
        (None, last)
    }

    #[test]
    fn a_side_lean_earns_its_release_sooner_than_the_same_lean_at_the_top() {
        // The asymmetry dogfood asked for, as a gesture rather than as a constant:
        // the identical lean releases at a side and does not at the top.
        let mut st = GrabState::default();
        st.hold();
        // 13 events at 16 ms is 0.192 s of charge (the first event charges nothing): past the side
        // hold of 0.18, short of the top's 0.30. Same events, same speed, different verdict.
        let (side, pressing) = lean(&mut st, fit::Edge::Right, 13, 16, 6.0, anywhere);
        assert!(matches!(side, Some(Release::Out(_))), "{pressing:?}");
        let mut st = GrabState::default();
        st.hold();
        let (top, pressing) = lean(&mut st, fit::Edge::Top, 13, 16, 6.0, anywhere);
        assert_eq!(top, None, "the top must still ask for the full hold");
        let p = pressing.expect("the top press was still charging");
        assert_eq!(p.hold, crate::vmlib::schema::EdgeHold::Standard.seconds());
        // Keep leaning and it does earn it — in place, because there is never a display above a
        // fullscreen window's top edge; that press is the chrome ask.
        let (top, _) = lean(&mut st, fit::Edge::Top, 20, 16, 6.0, nowhere);
        assert_eq!(top, Some(Release::InPlaceForChrome));
    }

    #[test]
    fn a_flick_at_any_edge_never_releases() {
        // The measured shape of an incidental corner throw: three events in 24 ms, moving far.
        for edge in [
            fit::Edge::Left,
            fit::Edge::Right,
            fit::Edge::Top,
            fit::Edge::Bottom,
        ] {
            let mut st = GrabState::default();
            st.hold();
            let (release, _) = lean(&mut st, edge, 3, 8, 60.0, anywhere);
            assert_eq!(release, None, "{edge:?} released on a flick");
        }
    }

    #[test]
    fn a_press_at_a_dead_edge_holds_the_pointer_and_banks_nothing() {
        // Pushing where no display is, is not a request to visit whichever screen the arrangement
        // can reach. And it must not sit there fully charged: sliding along the edge into a
        // reachable stretch has to start a fresh, deliberate press.
        let mut st = GrabState::default();
        st.hold();
        let (release, pressing) = lean(&mut st, fit::Edge::Right, 40, 16, 6.0, nowhere);
        assert_eq!(release, None, "a dead edge must not release");
        let p = pressing.expect("but the press is still recognised");
        // It never sits FULLY charged, waiting to fire the instant the pointer slides somewhere
        // releasable: each time it earns the hold it lapses and starts over. 40 events at 16 ms is
        // more than twice the hold, so a press that banked would be well past it by now.
        assert!(
            st.charge.get().0 < p.hold,
            "a dead edge must not bank a full charge: {:?}",
            st.charge.get()
        );
        assert!(st.holding(), "the pointer stays with the guest");
    }

    #[test]
    fn a_drag_against_an_edge_never_releases() {
        // Dragging a guest window into an edge would otherwise ungrab and drop it on the next
        // display.
        let mut st = GrabState::default();
        st.hold();
        let t0 = Instant::now();
        for i in 1..=60 {
            let mut s = press(
                (screen().w, 500.0),
                (6.0, 0.0),
                t0 + Duration::from_millis(16 * i),
            );
            s.buttons_down = true;
            assert_eq!(press_step(&mut st, &s, anywhere).release, None);
        }
    }

    #[test]
    fn a_corner_lean_reaches_the_guest_and_never_releases() {
        // The guest owns its corners — leaning into the top-left one is how the GNOME overview is
        // opened, and it necessarily pushes into two edges at once.
        let mut st = GrabState::default();
        st.hold();
        let t0 = Instant::now();
        let fit = screen();
        for i in 1..=60 {
            let s = press(
                (fit.x, fit.y + fit.h),
                (-6.0, -6.0),
                t0 + Duration::from_millis(16 * i),
            );
            let out = press_step(&mut st, &s, anywhere);
            assert_eq!(out.pressing, None, "a corner is not a press");
            assert_eq!(out.release, None);
        }
    }

    #[test]
    fn an_explicit_hard_grab_ignores_every_edge_press() {
        // `Cmd-Ctrl-G` is the tool for "the pointer does not leave, for any reason", which is why
        // it is not the default. The chord is its only way out.
        let mut st = GrabState::default();
        // Not `hold()`: a hard grab is not the policy holding the pointer.
        let (release, pressing) = lean(&mut st, fit::Edge::Right, 60, 16, 6.0, anywhere);
        assert_eq!(release, None);
        assert_eq!(pressing, None, "not even charging");
    }

    fn free(pos: (f64, f64), now: Instant) -> Free {
        Free {
            now,
            pos,
            fit: screen(),
            fullscreen_and_key: true,
            space_visible: true,
            grab_enabled: true,
            buttons_down: false,
            click: false,
        }
    }

    fn click(pos: (f64, f64), now: Instant) -> Free {
        Free {
            click: true,
            buttons_down: true,
            ..free(pos, now)
        }
    }

    #[test]
    fn a_click_on_the_content_takes_the_grab_at_once_even_after_an_explicit_release() {
        // Rig 2026-08-21: after one Cmd-Ctrl-G the latch never cleared on a fullscreen-everything
        // Mac (the pointer can never leave guest content, the window never loses key), so the
        // pointer sat deep inside for minutes, clicking, ungrabbed. The click is the ask.
        let mut st = GrabState::default();
        st.release_by_user(true);
        let t0 = Instant::now();
        // Deep inside and dwelling does not override the latch…
        assert!(!free_step(&mut st, &free((700.0, 400.0), t0), || true).grab);
        assert!(
            !free_step(
                &mut st,
                &free((700.0, 400.0), t0 + fit::REGRAB_DWELL),
                || true
            )
            .grab
        );
        assert!(st.user_released());
        // …a click does, immediately, margin and dwell notwithstanding.
        let out = free_step(
            &mut st,
            &click((fit::REGRAB_MARGIN / 2.0, 400.0), t0),
            || true,
        );
        assert!(out.grab);
        assert!(st.holding());
        assert!(!st.user_released());
    }

    #[test]
    fn a_click_outside_the_content_asks_for_nothing() {
        let mut st = GrabState::default();
        let fit = screen();
        let out = free_step(
            &mut st,
            &click((fit.x - 5.0, 400.0), Instant::now()),
            || true,
        );
        assert!(!out.grab);
        assert!(!st.holding());
    }

    #[test]
    fn a_click_on_macos_chrome_over_the_guest_stands_the_regrab_down() {
        // Rig 2026-08-22: with the grab taken on any click, reaching the menu bar over a
        // fullscreen guest was a fight — the walk back down re-took the pointer, and clicking
        // "Displays" took it again, so enabling the second display was nearly impossible. The
        // click is inside the guest's rect by geometry and is not a guest click at all.
        let mut st = GrabState::default();
        let t0 = Instant::now();
        let out = free_step(&mut st, &click((700.0, 4.0), t0), || false);
        assert!(
            !out.grab,
            "a click on the menu bar must not take the pointer"
        );
        assert!(!st.holding());
        assert!(st.user_released(), "and it stands the auto re-grab down");
        assert!(out.left_guest, "worth a line: the user is working in macOS");
        // The menu is open now; resting deep inside the guest's rect, under it, changes nothing.
        assert!(!free_step(&mut st, &free((700.0, 400.0), t0), || false).grab);
        assert!(
            !free_step(
                &mut st,
                &free((700.0, 400.0), t0 + fit::REGRAB_DWELL * 2),
                || false
            )
            .grab
        );
        // Coming back is the same one gesture it always was: a click on the guest itself.
        assert!(free_step(&mut st, &click((700.0, 400.0), t0), || true).grab);
    }

    fn gain(fullscreen: bool, covering: usize) -> ScreenGain {
        ScreenGain {
            fullscreen,
            covering,
        }
    }

    #[test]
    fn going_fullscreen_hands_the_guest_the_pointer() {
        assert!(gained_screen(Some(gain(false, 1)), gain(true, 1)));
    }

    #[test]
    fn a_panel_joining_a_fullscreen_session_hands_it_over_too() {
        // "Use Other Screens When Fullscreen", and plugging a display in: already fullscreen,
        // now covering one more panel.
        assert!(gained_screen(Some(gain(true, 1)), gain(true, 2)));
    }

    #[test]
    fn nothing_else_seizes_the_pointer() {
        // A starting state is not a transition — a launch must not grab before being asked.
        assert!(!gained_screen(None, gain(true, 2)));
        // Steady state, leaving fullscreen, and losing a panel are all silent.
        assert!(!gained_screen(Some(gain(true, 2)), gain(true, 2)));
        assert!(!gained_screen(Some(gain(true, 2)), gain(false, 1)));
        assert!(!gained_screen(Some(gain(true, 2)), gain(true, 1)));
        // A panel gained while NOT fullscreen is an ordinary window on another screen.
        assert!(!gained_screen(Some(gain(false, 1)), gain(false, 2)));
    }

    #[test]
    fn the_screen_gain_trigger_overrides_an_explicit_release() {
        // The path this exists for: click the menus to reach "Use Other Screens When
        // Fullscreen" (which stands the grab down), pick it, and come up grabbed anyway.
        let mut st = GrabState::default();
        st.release_by_user(true);
        assert!(st.user_released());
        st.take_by_policy();
        assert!(!st.user_released());
        assert!(st.holding());
    }

    #[test]
    fn a_click_in_our_own_letterbox_neither_grabs_nor_disarms() {
        // Dogfood 2026-08-22, "clicking doesn't capture consistently": a fitted mode leaves a
        // letterbox inside our window, and a secondary window leaves a band above the guest.
        // Clicks there are on OUR window, so reading them as "the user went to macOS" and
        // latching was what made the next click's dwell path silently dead.
        let mut st = GrabState::default();
        let fit = screen();
        let out = free_step(
            &mut st,
            &click((fit.x - 5.0, 400.0), Instant::now()),
            || true,
        );
        assert!(!out.grab, "off the picture is not an ask for the grab");
        assert!(
            !st.user_released(),
            "and it is not the user leaving, either"
        );
        assert!(!out.left_guest);
    }

    #[test]
    fn the_dwell_regrab_waits_for_whatever_is_in_front_of_the_guest_to_go() {
        // The pointer is deep inside and has served its dwell, but an open menu (or any other
        // app's window) is over that spot: the grab is not the user's intent there.
        let mut st = GrabState::default();
        let t0 = Instant::now();
        let deep = (700.0, 400.0);
        assert!(!free_step(&mut st, &free(deep, t0), || false).grab);
        assert!(!free_step(&mut st, &free(deep, t0 + fit::REGRAB_DWELL * 2), || false).grab);
        // The menu closes; the same resting pointer is now the guest's again.
        assert!(free_step(&mut st, &free(deep, t0 + fit::REGRAB_DWELL * 3), || true).grab);
    }

    #[test]
    fn a_window_whose_space_is_off_screen_never_takes_the_pointer() {
        // Dogfood 2026-08-08: the pointer would freeze and vanish on *another* Space, a couple of
        // seconds after coming to rest. Traced: `captured=true` while `on_active_space=false`, the
        // grab taken two seconds AFTER our Space left.
        //
        // Key status is the trap. A window stays key across a Space switch, so `fullscreen_and_key`
        // still said "this pointer is ours" while limina was not on screen at all; the pointer then
        // served the re-grab dwell over a window nobody could see, and capture parked and hid it
        // where its owner was invisible. Mission Control is the same fault with a shorter fuse.
        let mut st = GrabState::default();
        let t0 = Instant::now();
        let deep = (700.0, 400.0);
        for i in 0..40 {
            let mut s = free(deep, t0 + Duration::from_millis(16 * i));
            s.space_visible = false;
            assert!(
                !free_step(&mut st, &s, || true).grab,
                "grabbed from another Space"
            );
        }
        // And it is not merely deferred: coming back is a fresh dwell, not a banked one.
        assert!(!free_step(&mut st, &free(deep, t0 + fit::REGRAB_DWELL * 2), || true).grab);
    }

    #[test]
    fn the_soft_keyboard_grab_lets_go_when_our_space_is_not_the_one_on_screen() {
        // Dogfood 2026-08-08, found immediately after the pointer half: with the pointer grab now
        // released into Mission Control, the *keyboard* kept feeding the guest. Same trap, one
        // input over — the soft grab engages on key status, and a window keeps that across a Space
        // switch, so limina went on eating keystrokes aimed at a Mission Control it wasn't in.
        let engaged = |is_key, space| soft_keyboard_engaged(false, true, is_key, false, space);
        assert!(engaged(true, true), "our Space, our window: ours");
        assert!(!engaged(true, false), "key, but we are not on screen");
        assert!(!engaged(false, true), "not key: someone else's keyboard");
        // The existing terms still decide on their own.
        assert!(
            !soft_keyboard_engaged(true, true, true, false, true),
            "a full capture owns the keyboard through the other path"
        );
        assert!(
            !soft_keyboard_engaged(false, false, true, false, true),
            "disabled"
        );
        assert!(
            !soft_keyboard_engaged(false, true, true, true, true),
            "muted"
        );
    }

    #[test]
    fn the_chord_stays_armed_after_it_has_muted_the_soft_grab() {
        // The reported bug: with the Cmd/Option swap on, "Ctrl held + Super" IS the chord, so the
        // second press must be recognized rather than falling through to the guest as a bare
        // Super. Muted + key + on-screen is exactly the state the first fire leaves behind.
        assert!(chord_survives_mute(false, false, true, true, true));
        // Not muted: the soft grab (or nothing at all) owns Ctrl+Option, and this path must keep
        // its hands off — leaving it an ordinary guest combo under --no-soft-kbd-grab.
        assert!(!chord_survives_mute(false, false, false, true, true));
        // A live grab has its own chord path; the two must never both claim the edge.
        assert!(!chord_survives_mute(true, false, true, true, true));
        assert!(!chord_survives_mute(false, true, true, true, true));
        // Session tap: without these we would eat Ctrl+Option from whatever app is really focused.
        assert!(
            !chord_survives_mute(false, false, true, false, true),
            "not key"
        );
        assert!(
            !chord_survives_mute(false, false, true, true, false),
            "our Space is not the one on screen"
        );
    }

    /// Facts for a healthy on-screen window; the tests below knock properties out one at a time.
    fn on_glass() -> WindowFacts {
        WindowFacts {
            slot: 0,
            primary: true,
            key: true,
            on_active_space: true,
            has_screen: true,
            fullscreen: true,
        }
    }

    #[test]
    fn a_live_grab_is_dropped_when_its_window_goes_out_of_view() {
        // The other half: the bits above only stop it being *taken*. A grab already held when the
        // Space leaves — Ctrl-Up into Mission Control — has to be given back, or the pointer is
        // parked and hidden with nothing on screen to explain why.
        let mut w = on_glass();
        w.on_active_space = false;
        assert!(must_drop_grab(true, Some(&w)), "Space gone");
        let mut w = on_glass();
        w.has_screen = false;
        assert!(must_drop_grab(true, Some(&w)), "screen gone");
        assert!(!must_drop_grab(true, Some(&on_glass())), "nothing changed");
        assert!(
            must_drop_grab(true, None),
            "an owner with no window at all is the same class"
        );
        assert!(
            !must_drop_grab(false, None),
            "no grab to drop; releasing would unhide a cursor nobody grabbed"
        );
    }

    #[test]
    fn a_held_grab_is_judged_by_the_window_owning_the_cursor_not_the_primary() {
        // The captured cursor lives in the window the grab was taken in, on any covered panel; whether
        // "something on screen explains the hidden pointer" is a question about THAT panel.
        // Judging the primary alone got it wrong in both directions: the cursor parked on a
        // panel whose window was swiped away stayed grabbed and invisible (nothing on screen
        // explained it), and a workspace swipe on the PRIMARY panel dropped a grab that was
        // working fine on the other one.
        let primary = on_glass();
        let mut secondary = on_glass();
        secondary.primary = false;
        secondary.key = false;
        secondary.slot = 1;

        // The cursor's panel was swiped to a macOS workspace: its window left the Space.
        let mut hidden = secondary;
        hidden.on_active_space = false;
        let facts = [primary, hidden];
        assert!(
            must_drop_grab(true, capture_owner(&facts, 1)),
            "parked on a hidden panel: nothing on screen explains the pointer"
        );

        // The PRIMARY's panel was swiped away while the cursor works a still-visible panel.
        let mut primary_off = primary;
        primary_off.on_active_space = false;
        let facts = [primary_off, secondary];
        assert!(
            !must_drop_grab(true, capture_owner(&facts, 1)),
            "the guest under the cursor is on glass; the grab is still justified"
        );

        // The owner's slot has no window this tick at all.
        assert!(must_drop_grab(true, capture_owner(&[primary], 1)));
    }

    #[test]
    fn the_free_grab_arms_by_the_window_under_the_pointer_not_the_primary() {
        // The free-path mirror of the owner-judged drop: with the primary's panel swiped to a
        // workspace, the covering guest still on glass on the OTHER panel must be able to take
        // the pointer back — and offer its seams' resistance. Key stays the primary's: the
        // keyboard reaches the guest through it alone.
        let mut primary_off = on_glass();
        primary_off.on_active_space = false;
        let mut covering = on_glass();
        covering.primary = false;
        covering.key = false;
        covering.slot = 1;
        let facts = [primary_off, covering];
        assert_eq!(
            free_arming(&facts, Some(1)),
            (true, true),
            "the visible covering guest re-arms while the primary is off-Space"
        );
        assert_eq!(
            free_arming(&facts, Some(0)),
            (true, false),
            "over the hidden primary's phantom: the Space veto is the hit window's own"
        );

        // A windowed (non-fullscreen) secondary is not a grab surface, whatever the primary is.
        let mut windowed = covering;
        windowed.fullscreen = false;
        assert_eq!(free_arming(&[on_glass(), windowed], Some(1)), (false, true));

        // Off every guest content the primary's own facts stand in (disarm/dwell only).
        assert_eq!(free_arming(&[on_glass(), covering], None), (true, true));
        assert_eq!(free_arming(&facts, None), (true, false));

        // No guest window holds the keyboard: no grab anywhere.
        let mut keyless = on_glass();
        keyless.key = false;
        assert_eq!(free_arming(&[keyless, covering], Some(1)), (false, true));

        // ...but the SECONDARY holding it arms just as well as the primary. Reported
        // 2026-08-22: clicking the built-in display's guest stopped grabbing entirely, and
        // logged nothing, because that first click made the secondary key — so the primary was
        // not key, the free path disarmed, and every later click was refused before the
        // window-server hit test was spent. `primary.key` was asking the wrong question; the
        // keyboard is ours whenever any guest window has it.
        let mut primary_unfocused = on_glass();
        primary_unfocused.key = false;
        let mut secondary_key = covering;
        secondary_key.key = true;
        assert_eq!(
            free_arming(&[primary_unfocused, secondary_key], Some(1)),
            (true, true),
            "clicking a secondary makes it key; that must not disarm the grab"
        );
    }

    #[test]
    fn losing_key_releases_any_capture_hard_grabs_included() {
        // The escape hatch for "a dialog appeared over a captured VM": the tap consumes every
        // mouse event regardless of key state, so an unclickable sheet with a hidden cursor is
        // what NOT releasing here looks like. Deliberately not gated on the policy's `holding` —
        // an explicit Cmd-Ctrl-G grab must let go of a background VM too.
        let mut w = on_glass();
        w.key = false;
        assert!(key_loss_releases(true, &[w]));
        assert!(
            !key_loss_releases(true, &[on_glass()]),
            "still key: keep it"
        );
        assert!(
            !key_loss_releases(false, &[w]),
            "nothing held, nothing owed"
        );

        // The regression that made clicks on a secondary flip-flop: that click makes the
        // SECONDARY key, so the primary is not, and judging by the primary alone released the
        // grab the same click had just taken — the guest never saw the press.
        let mut secondary_key = on_glass();
        secondary_key.primary = false;
        secondary_key.slot = 1;
        let mut primary_unfocused = on_glass();
        primary_unfocused.key = false;
        assert!(
            !key_loss_releases(true, &[primary_unfocused, secondary_key]),
            "a secondary holding the keyboard keeps the grab"
        );
        assert!(
            key_loss_releases(true, &[primary_unfocused, w]),
            "no guest window has it: the pointer goes back"
        );
    }

    #[test]
    fn leaving_fullscreen_releases_only_the_policys_grab() {
        // The edge-press release is gated on fullscreen, so a policy grab surviving into a
        // windowed VM would have no gesture out but the chord. The user's explicit grab is a
        // different tool — unconditional by design — and survives the mode change.
        let mut w = on_glass();
        w.fullscreen = false;
        assert!(fullscreen_exit_releases(true, true, &w));
        assert!(
            !fullscreen_exit_releases(false, true, &w),
            "a hard grab is the user's; the mode change must not take it"
        );
        assert!(!fullscreen_exit_releases(true, false, &w), "nothing held");
        assert!(
            !fullscreen_exit_releases(true, true, &on_glass()),
            "still fullscreen"
        );
    }

    #[test]
    fn the_primary_is_found_by_its_flag_not_its_position() {
        let mut a = on_glass();
        a.primary = false;
        a.slot = 2;
        let b = on_glass();
        assert_eq!(primary_facts(&[a, b]), b);
    }

    #[test]
    fn the_regrab_waits_for_the_dwell_deep_inside() {
        let mut st = GrabState::default();
        let t0 = Instant::now();
        let deep = (700.0, 400.0);
        // Arriving is not enough, however far inside.
        assert!(!free_step(&mut st, &free(deep, t0), || true).grab);
        // Still not, a moment later.
        assert!(
            !free_step(
                &mut st,
                &free(deep, t0 + Duration::from_millis(100)),
                || true
            )
            .grab
        );
        // Past the dwell, yes — once. Afterwards the policy is holding and the free path is done.
        let out = free_step(&mut st, &free(deep, t0 + fit::REGRAB_DWELL), || true);
        assert!(out.grab);
        assert!(st.holding());
        assert!(out.inside_for.is_some_and(|d| d >= fit::REGRAB_DWELL));
        // Nothing half-earned survives the grab: the edge press that follows starts from zero.
        assert_eq!(st.charge.get(), (0.0, 0.0));
    }

    #[test]
    fn a_pointer_at_the_edge_never_serves_the_dwell() {
        // On one display a released pointer cannot leave the window — the window server pins it to
        // the top row, which is still window territory. Re-grabbing on containment alone would take
        // it straight back and hide it.
        let mut st = GrabState::default();
        let t0 = Instant::now();
        let fit = screen();
        for pinned in [
            (fit.x + fit.w - 1.0, 500.0),
            (700.0, fit.y + fit.h - 1.0),
            (fit.x + 1.0, 500.0),
        ] {
            for i in 0..40 {
                let s = free(pinned, t0 + Duration::from_millis(16 * i));
                assert!(
                    !free_step(&mut st, &s, || true).grab,
                    "{pinned:?} re-grabbed"
                );
            }
        }
    }

    #[test]
    fn a_button_held_down_defers_the_regrab_without_losing_it() {
        // A selection drag that happens to pass deep inside must not be interrupted by a grab.
        let mut st = GrabState::default();
        let t0 = Instant::now();
        let deep = (700.0, 400.0);
        for i in 0..40 {
            let mut s = free(deep, t0 + Duration::from_millis(16 * i));
            s.buttons_down = true;
            assert!(!free_step(&mut st, &s, || true).grab);
        }
        // Button up, same position: the dwell was already served, so this takes effect at once.
        assert!(
            free_step(
                &mut st,
                &free(deep, t0 + Duration::from_millis(700)),
                || true
            )
            .grab
        );
    }

    #[test]
    fn an_explicit_release_survives_inside_and_ends_by_leaving() {
        // This is the bug that made one Cmd-Ctrl-G look like it broke the grab for good: keyed only
        // on a focus round trip, the latch never cleared, because a fullscreen VM has nothing else
        // on that display to click.
        let mut st = GrabState::default();
        st.release_by_user(true);
        let t0 = Instant::now();
        let deep = (700.0, 400.0);
        for i in 0..60 {
            let s = free(deep, t0 + Duration::from_millis(16 * i));
            assert!(
                !free_step(&mut st, &s, || true).grab,
                "the latch must hold inside"
            );
        }
        // Out of the guest — onto the other display — and the grab re-arms.
        let out = free_step(
            &mut st,
            &free((-40.0, 400.0), t0 + Duration::from_secs(1)),
            || true,
        );
        assert!(out.left_guest);
        assert!(!st.user_released());
        // Back in, and after the dwell it takes the pointer again.
        let t1 = t0 + Duration::from_secs(2);
        assert!(!free_step(&mut st, &free(deep, t1), || true).grab);
        assert!(free_step(&mut st, &free(deep, t1 + fit::REGRAB_DWELL), || true).grab);
    }

    #[test]
    fn a_promotion_to_a_hard_grab_does_not_latch_a_release() {
        // Cmd-Ctrl-G from a policy grab promotes in place: the pointer stays held, the policy just
        // stops owning it. Latching there would leave the grab disarmed after the chord releases.
        let mut st = GrabState::default();
        st.hold();
        st.release_by_user(false);
        assert!(!st.holding(), "the policy no longer owns it");
        assert!(!st.user_released(), "but nothing is latched");
    }

    #[test]
    fn a_pointer_that_is_not_ours_is_left_entirely_alone() {
        // Windowed, or another window key: no ask, no grab, and no dwell banked for the next
        // fullscreen session to inherit and fire on its first twitch.
        let mut st = GrabState::default();
        let t0 = Instant::now();
        let deep = (700.0, 400.0);
        assert!(free_step(&mut st, &free(deep, t0), || true).ask);
        let mut s = free(deep, t0 + Duration::from_millis(100));
        s.fullscreen_and_key = false;
        let out = free_step(&mut st, &s, || true);
        assert_eq!(out, FreeOutcome::default(), "nothing is owed");
        assert_eq!(st.inside_since, None, "and the dwell is forgotten");
    }

    #[test]
    fn the_chrome_ask_runs_even_with_the_grab_switched_off() {
        // `Edge hold: Off` means the pointer is free at every edge — but under `notch = extend` the
        // ask is the ONLY route back to the menu bar, and it has now been lost three times by
        // riding on a check that belonged to the grab.
        let mut st = GrabState::default();
        let mut s = free((700.0, 400.0), Instant::now());
        s.grab_enabled = false;
        let out = free_step(&mut st, &s, || true);
        assert!(out.ask, "the ask is not the grab's to disable");
        assert!(!out.grab);
        assert_eq!(out.inside_for, None, "and no dwell is kept while off");
    }

    /// One reveal event at the top edge of the test screen, pushing upward by `-dy`.
    fn reveal(pos: (f64, f64), dy: f64, now: Instant) -> Reveal {
        Reveal {
            now,
            slot: 0,
            pos,
            delta_y: dy,
            fit: screen(),
            src: RevealSrc::Monitor,
            captured: false,
            overlay_active: true,
        }
    }

    /// A point against the top edge, clear of both corner keepouts.
    fn top_point() -> (f64, f64) {
        let fit = screen();
        (700.0, fit.y + fit.h - 0.5)
    }

    #[test]
    fn a_chrome_lean_fires_and_survives_pinned_events() {
        // The recorded failure this pins: a real lean is pushes interleaved with `dy == 0`
        // events (the edge reports zero once the cursor is pinned — 46 of them in one recorded
        // lean). Lapsing on them plateaued the charge below the bar forever; treating them as
        // pushes would bank stillness. They must be NEUTRAL: the lean still fires.
        let mut st = RevealState::default();
        let t0 = Instant::now();
        for i in 1..=40u64 {
            let dy = if i % 3 == 0 { 0.0 } else { -6.0 };
            let s = reveal(top_point(), dy, t0 + Duration::from_millis(16 * i));
            let out = reveal_step(&mut st, &s).expect("the top band always traces");
            if dy == 0.0 {
                assert_eq!(out.why, "pinned");
            }
        }
        assert_eq!(st.ask(), Some(0), "the lean earned the ask");
    }

    #[test]
    fn a_reflow_event_is_dropped_not_lapsed() {
        // Granting the ask re-parents the view and the reflow is reported as pointer motion —
        // measured: `p.y` 982.0 -> 31.9 carrying `dy=+33.0`. The event must be DROPPED: acting
        // on it released the ask under a still-leaning pointer (the chrome oscillated), and
        // lapsing would lose a real gesture's charge to the app's own cursor bookkeeping.
        let mut st = RevealState::default();
        let t0 = Instant::now();
        for i in 1..=5u64 {
            reveal_step(
                &mut st,
                &reveal(top_point(), -6.0, t0 + Duration::from_millis(16 * i)),
            );
        }
        let charged = st.charge.get();
        assert!(charged.0 > 0.0, "the lean had banked something");
        let jump = reveal((700.0, 31.9), 33.0, t0 + Duration::from_millis(16 * 6));
        let out = reveal_step(&mut st, &jump).expect("traced");
        assert_eq!(out.why, "reflow");
        assert_eq!(st.charge.get(), charged, "the charge survives untouched");
    }

    #[test]
    fn the_settle_window_swallows_the_release_right_after_a_grant() {
        // Granting drops the overlay, which moves the content out from under a stationary
        // pointer; for a few frames the position says "back inside the guest" when nothing
        // moved. Releasing on that re-earned the lean over and over ("sometimes it takes a
        // lot of pushing"). Past the settle window, coming back inside releases for real.
        let mut st = RevealState::default();
        let t0 = Instant::now();
        reveal_grant(&mut st, 0, t0);
        let inside = (700.0, 500.0);
        let early = reveal_step(
            &mut st,
            &reveal(inside, 1.0, t0 + Duration::from_millis(100)),
        );
        assert_eq!(early, None, "layout, not intent: swallowed silently");
        assert_eq!(st.ask(), Some(0), "the ask survives the settle window");
        let late = reveal_step(
            &mut st,
            &reveal(inside, 1.0, t0 + Duration::from_millis(300)),
        )
        .expect("a real release traces");
        assert_eq!(late.why, "release");
        assert_eq!(st.ask(), None);
    }

    #[test]
    fn a_step_from_another_panel_releases_the_old_ask_and_adopts_the_slot() {
        // One gesture exists at a time; leaving the panel IS "coming back into the guest".
        let mut st = RevealState::default();
        let t0 = Instant::now();
        reveal_grant(&mut st, 0, t0);
        let mut s = reveal(top_point(), -6.0, t0 + Duration::from_secs(1));
        s.slot = 1;
        reveal_step(&mut st, &s);
        assert_eq!(st.ask(), None, "the old owner's ask is released");
        assert_eq!(st.owner(), 1, "and the gesture belongs to the new panel");
    }

    #[test]
    fn a_corner_lean_never_arms_the_ask() {
        // Corners are the guest's: the top-left one is the GNOME overview trigger, and pushing
        // into it necessarily pushes upward too.
        let mut st = RevealState::default();
        let t0 = Instant::now();
        let fit = screen();
        for i in 1..=60u64 {
            let s = reveal(
                (fit.x + 1.0, fit.y + fit.h - 0.5),
                -6.0,
                t0 + Duration::from_millis(16 * i),
            );
            let out = reveal_step(&mut st, &s).expect("traced");
            assert_eq!(out.why, "corner");
        }
        assert_eq!(st.ask(), None);
    }

    #[test]
    fn a_captured_pointer_clears_the_charge_silently() {
        // A grabbed pointer must never trip the ask; it has its own grant path.
        let mut st = RevealState::default();
        let t0 = Instant::now();
        for i in 1..=5u64 {
            reveal_step(
                &mut st,
                &reveal(top_point(), -6.0, t0 + Duration::from_millis(16 * i)),
            );
        }
        let mut s = reveal(top_point(), -6.0, t0 + Duration::from_millis(96));
        s.captured = true;
        assert_eq!(reveal_step(&mut st, &s), None);
        assert_eq!(st.charge.get(), (0.0, 0.0));
    }

    #[test]
    fn with_no_overlay_the_lean_reports_and_banks_nothing() {
        // Nothing to ask for: the chrome is already reachable.
        let mut st = RevealState::default();
        let t0 = Instant::now();
        for i in 1..=30u64 {
            let mut s = reveal(top_point(), -6.0, t0 + Duration::from_millis(16 * i));
            s.overlay_active = false;
            let out = reveal_step(&mut st, &s).expect("traced");
            assert_eq!(out.why, "no-overlay");
        }
        assert_eq!(st.ask(), None);
        assert_eq!(st.charge.get(), (0.0, 0.0), "lapsed, not banked");
    }

    #[test]
    fn an_outright_grant_adopts_the_owner_once() {
        // grant_chrome / the observed menu bar: the reveal_step that follows must judge THIS
        // grant's settle window, not a stale gesture's — and a repeat grant must not keep
        // re-opening that window.
        let mut st = RevealState::default();
        let t0 = Instant::now();
        reveal_grant(&mut st, 2, t0);
        assert_eq!((st.owner(), st.ask()), (2, Some(2)));
        let first = st.granted;
        reveal_grant(&mut st, 2, t0 + Duration::from_secs(1));
        assert_eq!(st.granted, first, "already granted: a no-op");
    }

    #[test]
    fn moot_clears_the_ask_but_the_owner_names_the_last_panel() {
        let mut st = RevealState::default();
        reveal_grant(&mut st, 3, Instant::now());
        st.moot();
        assert_eq!(st.ask(), None);
        assert_eq!(st.granted, None);
        assert_eq!(st.owner(), 3, "the owner is a name, not a claim");
    }

    #[test]
    fn the_menubar_grant_targets_the_panel_whose_top_the_pointer_is_at() {
        // At a seam-hold the pointer sits ON the boundary, which reads as the ABOVE panel's
        // bottom under any containment rule; "at the top of this panel" is unambiguous.
        let fit = screen();
        let top = fit.y + fit.h;
        assert!(at_panel_top((700.0, top), fit), "the boundary point itself");
        assert!(at_panel_top((700.0, top + 3.0), fit), "boundary rounding");
        assert!(!at_panel_top((700.0, top + 5.0), fit), "the panel above");
        assert!(!at_panel_top((700.0, top - REVEAL_MARGIN - 1.0), fit));
        assert!(
            !at_panel_top((fit.x - 1.0, top), fit),
            "another panel's column"
        );
    }
}
