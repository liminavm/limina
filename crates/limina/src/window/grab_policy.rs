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
pub(crate) fn must_drop_grab(captured: bool, on_active_space: bool, has_screen: bool) -> bool {
    captured && (!on_active_space || !has_screen)
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
/// The re-grab hysteresis is the load-bearing part: the pointer must come a real margin back
/// *inside* the content and stay there, with no button down, before the grab retakes it. On a single
/// display a released cursor cannot leave the window at all — fullscreen *is* the screen, and the
/// window server pins it to the top row, which is still window territory — so re-grabbing on mere
/// containment would take the pointer back on the first inward jitter. That is the likeliest way
/// this design ships worse than what it replaces.
pub(crate) fn free_step(st: &mut GrabState, s: &Free) -> FreeOutcome {
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
    if !st.user_released && fit::may_regrab(s.pos, s.fit, out.inside_for, s.buttons_down) {
        st.hold();
        out.grab = true;
    }
    out
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
        }
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
            assert!(!free_step(&mut st, &s).grab, "grabbed from another Space");
        }
        // And it is not merely deferred: coming back is a fresh dwell, not a banked one.
        assert!(!free_step(&mut st, &free(deep, t0 + fit::REGRAB_DWELL * 2)).grab);
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

    #[test]
    fn a_live_grab_is_dropped_when_its_window_goes_out_of_view() {
        // The other half: the bits above only stop it being *taken*. A grab already held when the
        // Space leaves — Ctrl-Up into Mission Control — has to be given back, or the pointer is
        // parked and hidden with nothing on screen to explain why.
        assert!(must_drop_grab(true, false, true), "Space gone");
        assert!(must_drop_grab(true, true, false), "screen gone");
        assert!(!must_drop_grab(true, true, true), "nothing has changed");
        assert!(
            !must_drop_grab(false, false, false),
            "no grab to drop; releasing would unhide a cursor nobody grabbed"
        );
    }

    #[test]
    fn the_regrab_waits_for_the_dwell_deep_inside() {
        let mut st = GrabState::default();
        let t0 = Instant::now();
        let deep = (700.0, 400.0);
        // Arriving is not enough, however far inside.
        assert!(!free_step(&mut st, &free(deep, t0)).grab);
        // Still not, a moment later.
        assert!(!free_step(&mut st, &free(deep, t0 + Duration::from_millis(100))).grab);
        // Past the dwell, yes — once. Afterwards the policy is holding and the free path is done.
        let out = free_step(&mut st, &free(deep, t0 + fit::REGRAB_DWELL));
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
                assert!(!free_step(&mut st, &s).grab, "{pinned:?} re-grabbed");
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
            assert!(!free_step(&mut st, &s).grab);
        }
        // Button up, same position: the dwell was already served, so this takes effect at once.
        assert!(free_step(&mut st, &free(deep, t0 + Duration::from_millis(700))).grab);
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
            assert!(!free_step(&mut st, &s).grab, "the latch must hold inside");
        }
        // Out of the guest — onto the other display — and the grab re-arms.
        let out = free_step(&mut st, &free((-40.0, 400.0), t0 + Duration::from_secs(1)));
        assert!(out.left_guest);
        assert!(!st.user_released());
        // Back in, and after the dwell it takes the pointer again.
        let t1 = t0 + Duration::from_secs(2);
        assert!(!free_step(&mut st, &free(deep, t1)).grab);
        assert!(free_step(&mut st, &free(deep, t1 + fit::REGRAB_DWELL)).grab);
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
        assert!(free_step(&mut st, &free(deep, t0)).ask);
        let mut s = free(deep, t0 + Duration::from_millis(100));
        s.fullscreen_and_key = false;
        let out = free_step(&mut st, &s);
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
        let out = free_step(&mut st, &s);
        assert!(out.ask, "the ask is not the grab's to disable");
        assert!(!out.grab);
        assert_eq!(out.inside_for, None, "and no dwell is kept while off");
    }
}
