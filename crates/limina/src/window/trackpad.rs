// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! *Policy*: which trackpad touch sequences the guest's multitouch touchpad owns, and what
//! that means for the cooked events the host also delivers. Pure — no AppKit — so the
//! ownership rules are unit-tested; `input.rs` feeds it the local `NSTouch`es.
//!
//! The rule (`docs/design/trackpad-gestures.md` §The ownership rule): a **sequence** runs from
//! the first finger down to the last finger up. One finger is the host tablet's, four or more
//! are the host's gestures, and a sequence whose **peak** count is two or three, begun over
//! the VM, is the guest's. Ownership is per sequence, not per instant: counts ramp while
//! fingers land and lift a few milliseconds apart, and deciding on the instant hands the host
//! the edges of the guest's gestures.
//!
//! While the guest owns a sequence, and through its momentum tail, the host's cooked
//! trackpad scroll is dropped — the guest scrolls from the contacts, and would otherwise get
//! the gesture twice.

use std::time::{Duration, Instant};

use limina_input::InputEvent;
use limina_input::constants::{BTN_LEFT, EV_KEY};
use limina_input::touchpad::{Contact, MAX_SLOTS, Touchpad, TouchpadGeometry};

/// No touch event for this long while the guest holds contacts means the end of the sequence
/// was lost (focus left, the window went away): lift the guest's fingers. Resting fingers keep
/// the gesture stream flowing, so this only fires on a real loss.
pub const TOUCH_SILENCE: Duration = Duration::from_millis(500);

/// The least time between two motion frames sent to the guest. The guest has no timestamps
/// from us — its kernel stamps each frame on arrival — and AppKit delivers the trackpad's
/// samples in back-to-back pairs a fraction of a millisecond apart. libinput reads real finger
/// motion over that near-zero interval as a jump and discards it ("kernel bug: Touch jump
/// detected"); its thresholds assume frames ~12 ms apart. Motion is therefore coalesced to
/// this spacing; fingers landing and lifting are never held back.
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(10);

/// How long a guest-owned sequence may run with fewer than two fingers before the guest is
/// told they lifted. AppKit's count flickers mid-gesture (2→1→2, even 2→0→2, within one
/// frame), and it reports no fingers at all while the pad is pressed down; passing either on
/// made the guest see a fresh two-finger touch, which it reads as a tap — a context menu at
/// the start of the next pinch, a second right-click after a physical one. A real lift is only
/// this late, and never happens while the touchpad's button is held.
pub const THIN_GRACE: Duration = Duration::from_millis(50);

/// A host secondary click this soon after a guest-owned sequence ended is macOS's own
/// tap-to-click reading of that two-finger tap, which the guest has already read itself.
/// macOS delivers it well after the lift (measured: 255–275 ms after the guest's sequence
/// ended); only the secondary button is dropped, because a two-finger tap never makes
/// anything else and a one-finger click right after a gesture is the host's.
pub const CLICK_TAIL: Duration = Duration::from_millis(500);

/// One local finger, as AppKit reports it: a stable identity and a position normalized to
/// the surface, **top-left** origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchSample {
    pub id: u64,
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    /// Fewer than two fingers so far: nothing is decided yet.
    Pending,
    Guest,
    Host,
}

/// Where a host mouse button edge goes.
#[derive(Debug, PartialEq)]
pub enum ClickRoute {
    /// Not the touchpad's business: the tablet takes it, as always.
    Host,
    /// The guest touchpad's own button — the click happened on fingers the guest holds, so
    /// the guest's click method decides what it means (two fingers → right, with GNOME's
    /// `fingers`), and libinput drops the tap it would otherwise also read.
    Touchpad(Vec<InputEvent>),
    /// The guest already produced this click from the contacts (its own tap); drop it.
    Drop,
}

/// The touch-sequence state machine, plus the guest device's contact state.
#[derive(Debug)]
pub struct TrackpadSeq {
    geometry: TouchpadGeometry,
    touchpad: Touchpad,
    /// The sequence in flight (fingers down), if any.
    owner: Option<Owner>,
    peak: usize,
    /// Whether the last *finished* sequence was the guest's — its momentum scroll keeps
    /// arriving after the fingers lift, and belongs to it.
    momentum_guest: bool,
    /// When the last guest-owned sequence ended ([`CLICK_TAIL`]).
    guest_ended: Option<Instant>,
    last_touch: Option<Instant>,
    /// When the last frame went to the guest, and the newest motion held back since.
    last_frame: Option<Instant>,
    pending: Option<Vec<Contact>>,
    /// Since when a guest-owned sequence has had fewer than two fingers, and how many
    /// ([`THIN_GRACE`]).
    thin: Option<(Instant, usize)>,
    /// The touchpad's button is down / a host press was dropped, so its release goes the
    /// same way.
    button: bool,
    dropped_press: bool,
}

impl TrackpadSeq {
    pub fn new(geometry: TouchpadGeometry) -> Self {
        Self {
            geometry,
            touchpad: Touchpad::new(),
            owner: None,
            peak: 0,
            momentum_guest: false,
            guest_ended: None,
            last_touch: None,
            last_frame: None,
            pending: None,
            thin: None,
            button: false,
            dropped_press: false,
        }
    }

    /// The local fingers down now (resting ones excluded), from one gesture event. `in_view`
    /// is whether the pointer is over the guest's content, or captured. Returns the events for
    /// the guest touchpad.
    pub fn on_touches(
        &mut self,
        touches: &[TouchSample],
        in_view: bool,
        now: Instant,
    ) -> Vec<InputEvent> {
        self.last_touch = Some(now);
        self.pending = None;
        let n = touches.len();

        if self.owner == Some(Owner::Guest) && n < 2 {
            // Hold the guest's fingers where they are; [`Self::tick`] lifts them if this is a
            // real lift rather than a flicker.
            let since = self.thin.map_or(now, |(since, _)| since);
            self.thin = Some((since, n));
            return Vec::new();
        }
        self.thin = None;

        if n == 0 {
            self.end_sequence(now);
            return self.touchpad.release_all();
        }

        let owner = *self.owner.get_or_insert_with(|| {
            // A new sequence: whatever momentum was coasting belongs to the old one, and
            // macOS stops it on touch.
            self.momentum_guest = false;
            Owner::Pending
        });
        self.peak = self.peak.max(n);

        let owner = match owner {
            _ if self.peak > MAX_SLOTS => Owner::Host,
            Owner::Pending if self.peak >= 2 => {
                if in_view {
                    Owner::Guest
                } else {
                    Owner::Host
                }
            }
            // The pointer left the VM mid-sequence: hand the rest of it to the host.
            Owner::Guest if !in_view => Owner::Host,
            o => o,
        };
        self.owner = Some(owner);

        if owner == Owner::Guest {
            let contacts: Vec<Contact> = touches
                .iter()
                .map(|t| {
                    let (x, y) = self.geometry.to_units(t.x, t.y);
                    Contact { id: t.id, x, y }
                })
                .collect();
            let motion_only = contacts.len() == self.touchpad.active();
            let too_soon = self
                .last_frame
                .is_some_and(|t| now.duration_since(t) < MIN_FRAME_INTERVAL);
            if motion_only && too_soon {
                self.pending = Some(contacts);
                return Vec::new();
            }
            self.send_frame(&contacts, now)
        } else {
            // One finger is never forwarded — it would drive the guest's cursor against the
            // tablet — and the host's sequences lift whatever the guest was holding.
            self.touchpad.release_all()
        }
    }

    fn end_sequence(&mut self, now: Instant) {
        if self.owner.is_some() {
            let guest = self.owner == Some(Owner::Guest);
            self.momentum_guest = guest;
            if guest {
                self.guest_ended = Some(now);
            }
            self.owner = None;
            self.peak = 0;
        }
    }

    fn send_frame(&mut self, contacts: &[Contact], now: Instant) -> Vec<InputEvent> {
        let events = self.touchpad.frame(contacts);
        if !events.is_empty() {
            self.last_frame = Some(now);
        }
        events
    }

    /// Periodic work, from the render tick: lift fingers that stayed lifted past
    /// [`THIN_GRACE`], lift everything if the touch stream went silent ([`TOUCH_SILENCE`]),
    /// and send the motion held back by [`MIN_FRAME_INTERVAL`].
    pub fn tick(&mut self, now: Instant) -> Vec<InputEvent> {
        if let Some((since, n)) = self.thin
            && !self.button
            && now.duration_since(since) >= THIN_GRACE
        {
            self.thin = None;
            self.pending = None;
            if n == 0 {
                self.end_sequence(now);
            }
            return self.touchpad.release_all();
        }
        // A pressed pad reports no touches for as long as it is held; its release comes as a
        // mouse-up, and focus loss cancels through [`Self::cancel`].
        if let Some(t) = self.last_touch
            && !self.button
            && self.touchpad.active() > 0
            && now.duration_since(t) >= TOUCH_SILENCE
        {
            return self.cancel();
        }
        let due = self
            .last_frame
            .is_none_or(|t| now.duration_since(t) >= MIN_FRAME_INTERVAL);
        match self.pending.take() {
            Some(contacts) if due => self.send_frame(&contacts, now),
            held => {
                self.pending = held;
                Vec::new()
            }
        }
    }

    /// Whether a cooked trackpad scroll event (one with a gesture or momentum phase) belongs
    /// to a guest-owned sequence and must not also reach the guest as wheel motion.
    pub fn swallows_scroll(&self) -> bool {
        match self.owner {
            Some(Owner::Guest) => true,
            Some(_) => false,
            None => self.momentum_guest,
        }
    }

    /// Route one host mouse-button edge (a two-finger click or tap arrives as the
    /// `secondary` button). See [`ClickRoute`].
    pub fn click(&mut self, down: bool, secondary: bool, now: Instant) -> ClickRoute {
        if down {
            if self.touchpad.active() > 0 {
                self.button = true;
                return ClickRoute::Touchpad(button(true));
            }
            let just_ended = self
                .guest_ended
                .is_some_and(|t| now.duration_since(t) < CLICK_TAIL);
            if self.owner == Some(Owner::Guest) || (secondary && just_ended) {
                self.dropped_press = true;
                return ClickRoute::Drop;
            }
            ClickRoute::Host
        } else if std::mem::take(&mut self.button) {
            ClickRoute::Touchpad(button(false))
        } else if std::mem::take(&mut self.dropped_press) {
            ClickRoute::Drop
        } else {
            ClickRoute::Host
        }
    }

    /// A transition that ends forwarding for the rest of this sequence (focus loss, capture
    /// toggling, the VM parking): lift the guest's fingers and its button, and give the
    /// sequence to the host.
    pub fn cancel(&mut self) -> Vec<InputEvent> {
        self.pending = None;
        self.thin = None;
        if self.owner.is_some() {
            self.owner = Some(Owner::Host);
        }
        self.momentum_guest = false;
        let mut events = self.touchpad.release_all();
        if std::mem::take(&mut self.button) {
            events.extend(button(false));
        }
        events
    }
}

fn button(down: bool) -> Vec<InputEvent> {
    vec![
        InputEvent::new(EV_KEY, BTN_LEFT, down as i32),
        InputEvent::syn(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use limina_input::constants::*;

    fn t(id: u64, x: f64, y: f64) -> TouchSample {
        TouchSample { id, x, y }
    }

    fn seq() -> TrackpadSeq {
        TrackpadSeq::new(TouchpadGeometry {
            width: 1000,
            height: 500,
        })
    }

    fn slots_down(ev: &[InputEvent]) -> usize {
        ev.iter()
            .filter(|e| e.type_ == EV_ABS && e.code == ABS_MT_TRACKING_ID && e.value >= 0)
            .count()
    }

    #[test]
    fn two_fingers_over_the_vm_are_the_guests() {
        let mut s = seq();
        let now = Instant::now();
        assert!(s.on_touches(&[t(1, 0.1, 0.1)], true, now).is_empty());
        assert!(!s.swallows_scroll(), "one finger is still the host's");
        let ev = s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.5, 0.5)], true, now);
        assert_eq!(slots_down(&ev), 2);
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 500)));
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, 250)));
        assert!(s.swallows_scroll());
    }

    #[test]
    fn a_sequence_begun_outside_the_vm_stays_the_hosts() {
        let mut s = seq();
        let now = Instant::now();
        assert!(
            s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], false, now)
                .is_empty()
        );
        // Moving over the VM mid-sequence does not hand it over.
        assert!(
            s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.3, 0.3)], true, now)
                .is_empty()
        );
        assert!(!s.swallows_scroll());
    }

    #[test]
    fn a_fourth_finger_takes_the_sequence_for_the_host_and_lifts_the_guest() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2), t(3, 0.3, 0.3)], true, now);
        let four = [
            t(1, 0.1, 0.1),
            t(2, 0.2, 0.2),
            t(3, 0.3, 0.3),
            t(4, 0.4, 0.4),
        ];
        let ev = s.on_touches(&four, true, now);
        assert!(ev.contains(&InputEvent::new(EV_KEY, BTN_TOUCH, 0)));
        assert!(!s.swallows_scroll());
        // Back down to three: still the host's (the peak decides).
        assert!(s.on_touches(&four[..3], true, now).is_empty());
        assert!(!s.swallows_scroll());
    }

    #[test]
    fn dropping_to_one_finger_lifts_the_guest_but_keeps_the_sequence() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now);
        assert!(s.on_touches(&[t(2, 0.2, 0.2)], true, now).is_empty());
        let ev = s.tick(now + THIN_GRACE);
        assert!(ev.contains(&InputEvent::new(EV_KEY, BTN_TOUCH, 0)));
        assert!(s.swallows_scroll(), "the sequence is still the guest's");
    }

    #[test]
    fn momentum_after_a_guest_sequence_is_swallowed_until_the_next_touch() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now);
        assert!(s.on_touches(&[], true, now).is_empty());
        let ev = s.tick(now + THIN_GRACE);
        assert!(ev.contains(&InputEvent::new(EV_KEY, BTN_TOUCH, 0)));
        assert!(s.swallows_scroll(), "the momentum tail is the guest's");
        s.on_touches(&[t(3, 0.1, 0.1)], true, now + THIN_GRACE * 2);
        assert!(!s.swallows_scroll(), "a new touch ends the old momentum");
    }

    #[test]
    fn leaving_the_vm_mid_sequence_hands_it_to_the_host() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now);
        let ev = s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.3, 0.3)], false, now);
        assert!(ev.contains(&InputEvent::new(EV_KEY, BTN_TOUCH, 0)));
        assert!(!s.swallows_scroll());
    }

    #[test]
    fn motion_is_paced_but_landings_are_not() {
        let mut s = seq();
        let now = Instant::now();
        let soon = now + MIN_FRAME_INTERVAL / 4;
        assert_eq!(
            slots_down(&s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now)),
            2
        );
        // Motion right behind the last frame is held back…
        assert!(
            s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.3, 0.3)], true, soon)
                .is_empty()
        );
        assert!(s.tick(soon).is_empty());
        // …a third finger landing is not, and it carries the newest positions…
        let ev = s.on_touches(
            &[t(1, 0.1, 0.1), t(2, 0.4, 0.4), t(3, 0.5, 0.5)],
            true,
            soon,
        );
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 400)));
        assert!(ev.contains(&InputEvent::new(EV_KEY, BTN_TOOL_TRIPLETAP, 1)));
        // …and held motion goes out once the spacing has passed.
        let later = soon + MIN_FRAME_INTERVAL / 2;
        assert!(
            s.on_touches(
                &[t(1, 0.1, 0.1), t(2, 0.4, 0.4), t(3, 0.6, 0.6)],
                true,
                later
            )
            .is_empty()
        );
        let ev = s.tick(soon + MIN_FRAME_INTERVAL);
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 600)));
        assert!(s.tick(soon + MIN_FRAME_INTERVAL * 2).is_empty());
    }

    #[test]
    fn a_count_flicker_does_not_lift_the_guests_fingers() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now);
        let blip = now + THIN_GRACE / 5;
        assert!(s.on_touches(&[], true, blip).is_empty());
        assert!(s.tick(blip).is_empty());
        // Back to two fingers within the grace: motion only, no lift and no new touch.
        let ev = s.on_touches(
            &[t(1, 0.1, 0.1), t(2, 0.25, 0.2)],
            true,
            blip + THIN_GRACE / 5,
        );
        assert_eq!(slots_down(&ev), 0);
        assert!(!ev.contains(&InputEvent::new(EV_KEY, BTN_TOUCH, 0)));
        assert!(s.tick(now + THIN_GRACE * 3).is_empty());
    }

    #[test]
    fn a_click_on_the_guests_fingers_is_the_touchpads_button() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now);
        let press = ClickRoute::Touchpad(vec![
            InputEvent::new(EV_KEY, BTN_LEFT, 1),
            InputEvent::syn(),
        ]);
        assert_eq!(s.click(true, true, now), press);
        // AppKit reports no fingers while the pad is pressed: they are held for the guest…
        s.on_touches(&[], true, now);
        assert!(s.tick(now + THIN_GRACE * 4).is_empty());
        assert_eq!(
            s.click(false, true, now + THIN_GRACE),
            ClickRoute::Touchpad(vec![
                InputEvent::new(EV_KEY, BTN_LEFT, 0),
                InputEvent::syn()
            ])
        );
        // …and returning after the press, they continue the same touch rather than landing anew.
        let back = now + THIN_GRACE * 5;
        let ev = s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, back);
        assert_eq!(slots_down(&ev), 0);
        let ev = s.tick(back + THIN_GRACE);
        assert!(ev.is_empty());
    }

    #[test]
    fn a_primary_click_right_after_a_guest_sequence_is_the_hosts() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now);
        s.on_touches(&[], true, now);
        let ended = now + THIN_GRACE;
        s.tick(ended);
        assert_eq!(s.click(true, false, ended), ClickRoute::Host);
        assert_eq!(s.click(false, false, ended), ClickRoute::Host);
    }

    #[test]
    fn a_click_right_after_a_guest_sequence_is_dropped_and_later_ones_are_the_hosts() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now);
        s.on_touches(&[], true, now);
        let ended = now + THIN_GRACE;
        s.tick(ended);
        assert_eq!(s.click(true, true, ended), ClickRoute::Drop);
        assert_eq!(s.click(false, true, ended), ClickRoute::Drop);
        let later = ended + CLICK_TAIL;
        assert_eq!(s.click(true, true, later), ClickRoute::Host);
        assert_eq!(s.click(false, true, later), ClickRoute::Host);
    }

    #[test]
    fn cancel_releases_a_held_touchpad_button() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now);
        s.click(true, true, now);
        let ev = s.cancel();
        assert!(ev.contains(&InputEvent::new(EV_KEY, BTN_LEFT, 0)));
        assert_eq!(s.click(false, true, now), ClickRoute::Host);
    }

    #[test]
    fn silence_lifts_the_guest_fingers_once() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now);
        assert!(s.tick(now + TOUCH_SILENCE / 2).is_empty());
        let ev = s.tick(now + TOUCH_SILENCE);
        assert!(ev.contains(&InputEvent::new(EV_KEY, BTN_TOUCH, 0)));
        assert!(s.tick(now + TOUCH_SILENCE * 2).is_empty());
        // The rest of the sequence is the host's: a late event does not re-press.
        assert!(
            s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now)
                .is_empty()
        );
    }
}
