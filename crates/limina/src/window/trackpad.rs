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
use limina_input::touchpad::{Contact, MAX_SLOTS, TOUCHPAD_RES, Touchpad, TouchpadGeometry};
use serde::{Deserialize, Serialize};

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
/// frame); passing each dip on would show the guest a lift and a fresh landing.
pub const THIN_GRACE: Duration = Duration::from_millis(50);

/// The fastest a frame may move a finger, from rest, as a distance per 12 ms: a frame past
/// libinput's 7 mm is discarded as a touch jump. Only the frame after a moved commit's landing
/// comes near it — it carries the whole distance moved before the commit at once — so that
/// frame carries the committing sample, never a newer one further on, spaced by its distance
/// and at least [`MIN_FRAME_INTERVAL`].
pub const COMMIT_SPEED_MM_PER_12MS: f64 = 6.0;

/// How far any finger must move before a guest-owned sequence **commits** — before the guest
/// sees its contacts at all. Past libinput's 1.3 mm tap threshold, with margin: the guest gets
/// the landing positions first and the motion after, so it can never read the gesture as a
/// tap.
pub const COMMIT_MOVE_MM: f64 = 3.0;

/// How long a still two-finger touch waits before it commits anyway (so a resting hold still
/// reaches the guest, e.g. to stop a kinetic scroll). A touch committed this way is then held
/// down for at least [`TAP_GUARD`] in the guest, whatever the fingers do.
pub const COMMIT_HOLD: Duration = Duration::from_millis(200);

/// The least time the guest holds a committed touch: past libinput's 180 ms tap timeout, so
/// a touch the guest sees can never end as a tap.
pub const TAP_GUARD: Duration = Duration::from_millis(200);

/// One local finger, as AppKit reports it: a stable identity, a position normalized to the
/// surface (**top-left** origin), and whether macOS considers it resting.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TouchSample {
    pub id: u64,
    pub x: f64,
    pub y: f64,
    pub resting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    /// Fewer than two fingers so far: nothing is decided yet.
    Pending,
    Guest,
    Host,
}

/// A guest-owned sequence before its contacts reach the guest ([`COMMIT_MOVE_MM`]).
#[derive(Debug)]
struct Uncommitted {
    /// When the sequence first had two fingers.
    since: Instant,
    /// Where each finger was first seen.
    landing: Vec<Contact>,
}

/// The touch-sequence state machine, plus the guest device's contact state.
///
/// **Taps and clicks are macOS's alone.** Two recognizers see the same fingers — macOS turns
/// taps and clicks into mouse clicks, and the guest's libinput would read its own taps from
/// the contacts — and their events arrive on separate, unordered streams (a click can land
/// before the touches it belongs to; touches vanish while the pad is pressed). Deciding per
/// click which one wins was a race. Instead the guest never sees a touch it could read as a
/// tap: contacts reach it only once the sequence has moved or held long enough to be a
/// gesture ([`COMMIT_MOVE_MM`], [`COMMIT_HOLD`], [`TAP_GUARD`]), and every click goes to the
/// tablet exactly as macOS recognised it.
#[derive(Debug)]
pub struct TrackpadSeq {
    geometry: TouchpadGeometry,
    touchpad: Touchpad,
    /// The sequence in flight (fingers down), if any.
    owner: Option<Owner>,
    peak: usize,
    /// A guest-owned sequence not yet shown to the guest.
    uncommitted: Option<Uncommitted>,
    /// When the guest's current touch committed by holding still ([`TAP_GUARD`]). A touch
    /// committed by moving can never be a tap, and lifts when the fingers do.
    held_at: Option<Instant>,
    /// Whether the last *finished* sequence was the guest's — its momentum scroll keeps
    /// arriving after the fingers lift, and belongs to it.
    momentum_guest: bool,
    last_touch: Option<Instant>,
    /// When the last frame went to the guest, and the newest motion held back since.
    last_frame: Option<Instant>,
    pending: Option<Vec<Contact>>,
    /// The earliest the held motion may go out, when a moved commit has spaced it by its
    /// distance ([`COMMIT_SPEED_MM_PER_12MS`]). Until then the held motion is the committing
    /// sample itself, not replaced by newer ones.
    not_before: Option<Instant>,
    /// Since when a guest-owned sequence has had fewer than two fingers, and how many
    /// ([`THIN_GRACE`]).
    thin: Option<(Instant, usize)>,
    /// Whether three-finger sequences are the guest's (the Input menu's switch). Off, they are
    /// the host's like four.
    three_fingers: bool,
}

impl TrackpadSeq {
    pub fn new(geometry: TouchpadGeometry) -> Self {
        Self {
            geometry,
            touchpad: Touchpad::new(),
            owner: None,
            peak: 0,
            uncommitted: None,
            held_at: None,
            momentum_guest: false,
            last_touch: None,
            last_frame: None,
            pending: None,
            not_before: None,
            thin: None,
            three_fingers: true,
        }
    }

    /// Give three-finger sequences to the guest, or leave them to macOS. A guest-owned
    /// three-finger sequence in flight goes to the host.
    pub fn set_three_fingers(&mut self, on: bool) -> Vec<InputEvent> {
        if self.three_fingers == on {
            return Vec::new();
        }
        self.three_fingers = on;
        if !on && self.owner == Some(Owner::Guest) && self.peak >= 3 {
            return self.cancel();
        }
        Vec::new()
    }

    /// The most fingers a guest-owned sequence may reach.
    fn guest_max(&self) -> usize {
        if self.three_fingers { MAX_SLOTS } else { 2 }
    }

    /// The local fingers down now, from one gesture event. `in_view` is whether the pointer
    /// is over the guest's content, or captured. Returns the events for the guest touchpad.
    ///
    /// A **resting** finger (macOS's reading of a thumb on the surface) does not count towards
    /// a sequence, the way macOS's own recognizer ignores it — a pointer move beside a resting
    /// thumb stays one finger. But macOS also marks fingers resting when they merely hold
    /// still mid-gesture, so a resting finger the guest already holds keeps counting.
    pub fn on_touches(
        &mut self,
        touches: &[TouchSample],
        in_view: bool,
        now: Instant,
    ) -> Vec<InputEvent> {
        let guest = self.owner == Some(Owner::Guest);
        let touches: Vec<TouchSample> = touches
            .iter()
            .filter(|t| !t.resting || (guest && self.touchpad.holds(t.id)))
            .copied()
            .collect();
        let touches = touches.as_slice();
        self.last_touch = Some(now);
        let n = touches.len();

        if self.owner == Some(Owner::Guest) && n < 2 {
            // Hold the guest's fingers where they are — the motion held back by the pacing
            // still goes out, it is where they last were — and let [`Self::lift`] decide
            // whether this is a real lift rather than a flicker.
            let since = self.thin.map_or(now, |(since, _)| since);
            self.thin = Some((since, n));
            return self.lift(now);
        }
        self.thin = None;
        let held = self.pending.take();

        if n == 0 {
            self.end_sequence();
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
            _ if self.peak > self.guest_max() => Owner::Host,
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

        if owner != Owner::Guest {
            // One finger is never forwarded — it would drive the guest's cursor against the
            // tablet — and the host's sequences lift whatever the guest was holding.
            self.uncommitted = None;
            return self.touchpad.release_all();
        }

        let contacts: Vec<Contact> = touches
            .iter()
            .map(|t| {
                let (x, y) = self.geometry.to_units(t.x, t.y);
                Contact { id: t.id, x, y }
            })
            .collect();

        if self.touchpad.active() == 0 {
            return self.try_commit(contacts, now);
        }

        let motion_only = contacts.len() == self.touchpad.active();
        if motion_only && self.not_before.is_some() {
            if !self.due(now) {
                self.pending = held;
                return Vec::new();
            }
            if let Some(held) = held {
                // The committing sample goes out first; the newest waits the usual spacing.
                let events = self.send_frame(&held, now);
                self.pending = Some(contacts);
                return events;
            }
        }
        if motion_only && !self.due(now) {
            self.pending = Some(contacts);
            return Vec::new();
        }
        if !self.due(now) {
            // A finger landing or lifting cannot wait, but the fingers already down stay where
            // the guest last saw them: moved in a frame so close to the last one, libinput
            // reads their motion as a touch jump and discards it. Their motion follows at the
            // usual spacing.
            let frame: Vec<Contact> = contacts
                .iter()
                .map(|c| match self.touchpad.position(c.id) {
                    Some((x, y)) => Contact { x, y, ..*c },
                    None => *c,
                })
                .collect();
            let not_before = self.not_before;
            let events = self.send_frame(&frame, now);
            self.not_before = not_before;
            self.pending = Some(contacts);
            return events;
        }
        self.send_frame(&contacts, now)
    }

    /// A guest-owned sequence the guest has not seen yet: show it once it is clearly a
    /// gesture. Moved far enough → the landing positions now and the current ones as the next
    /// (paced) frame; held still long enough → the current positions.
    fn try_commit(&mut self, contacts: Vec<Contact>, now: Instant) -> Vec<InputEvent> {
        let unc = self.uncommitted.get_or_insert_with(|| Uncommitted {
            since: now,
            landing: Vec::new(),
        });
        for c in &contacts {
            if !unc.landing.iter().any(|l| l.id == c.id) {
                unc.landing.push(*c);
            }
        }
        let threshold = (COMMIT_MOVE_MM * TOUCHPAD_RES as f64) as i32;
        let moved = contacts.iter().any(|c| {
            unc.landing
                .iter()
                .find(|l| l.id == c.id)
                .is_some_and(|l| (c.x - l.x).abs().max((c.y - l.y).abs()) >= threshold)
        });
        let held = now.duration_since(unc.since) >= COMMIT_HOLD;
        if !(moved || held) {
            return Vec::new();
        }
        let unc = self.uncommitted.take().expect("just inserted");
        if moved {
            let landing: Vec<Contact> = contacts
                .iter()
                .map(|c| *unc.landing.iter().find(|l| l.id == c.id).unwrap_or(c))
                .collect();
            let events = self.send_frame(&landing, now);
            let far = contacts
                .iter()
                .zip(&landing)
                .map(|(c, l)| f64::from(c.x - l.x).hypot(f64::from(c.y - l.y)))
                .fold(0.0, f64::max)
                / TOUCHPAD_RES as f64;
            let gap = Duration::from_secs_f64(0.012 * far / COMMIT_SPEED_MM_PER_12MS);
            self.not_before = Some(now + gap.max(MIN_FRAME_INTERVAL));
            self.pending = Some(contacts);
            events
        } else {
            self.held_at = Some(now);
            self.send_frame(&contacts, now)
        }
    }

    fn end_sequence(&mut self) {
        if self.owner.is_some() {
            self.momentum_guest = self.owner == Some(Owner::Guest);
            self.owner = None;
            self.peak = 0;
        }
        self.uncommitted = None;
        self.held_at = None;
        self.not_before = None;
    }

    /// Whether held motion may go out now: [`MIN_FRAME_INTERVAL`] after the last frame, and
    /// not before a spaced commit's frame is due.
    fn due(&self, now: Instant) -> bool {
        self.last_frame
            .is_none_or(|t| now.duration_since(t) >= MIN_FRAME_INTERVAL)
            && self.not_before.is_none_or(|t| now >= t)
    }

    fn send_frame(&mut self, contacts: &[Contact], now: Instant) -> Vec<InputEvent> {
        self.not_before = None;
        let events = self.touchpad.frame(contacts);
        if !events.is_empty() {
            self.last_frame = Some(now);
        }
        events
    }

    /// A guest-owned sequence down to fewer than two fingers: send the motion still held back
    /// by the pacing once it is due — the guest must see the fingers move before they lift,
    /// or a quick flick reads as a tap — then lift the guest's fingers, never inside the tap
    /// guard. With every finger up that is at once: in the recordings a drop to none has
    /// never been a flicker, and a kinetic scroll's velocity is read from the motion just
    /// before the lift, so any wait reads as the fingers stopping. With one finger left it
    /// waits out [`THIN_GRACE`], after which the sequence ends if none are down.
    fn lift(&mut self, now: Instant) -> Vec<InputEvent> {
        let Some((since, n)) = self.thin else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if let Some(contacts) = self.pending.take() {
            if !self.due(now) {
                self.pending = Some(contacts);
                return events;
            }
            events = self.send_frame(&contacts, now);
        }
        let graced = now.duration_since(since) >= THIN_GRACE;
        if (n == 0 || graced) && (self.touchpad.active() == 0 || self.guarded(now)) {
            events.extend(self.touchpad.release_all());
            if graced {
                self.thin = None;
                if n == 0 {
                    self.end_sequence();
                }
            }
        }
        events
    }

    /// Whether the guest's touch has been down long enough that lifting it cannot read as a
    /// tap ([`TAP_GUARD`]).
    fn guarded(&self, now: Instant) -> bool {
        self.held_at
            .is_none_or(|t| now.duration_since(t) >= TAP_GUARD)
    }

    /// Periodic work, from the render tick: lift fingers that stayed lifted past
    /// [`THIN_GRACE`] (and the tap guard), lift everything if the touch stream went silent
    /// ([`TOUCH_SILENCE`]), and send the motion held back by [`MIN_FRAME_INTERVAL`].
    pub fn tick(&mut self, now: Instant) -> Vec<InputEvent> {
        if self.thin.is_some() {
            return self.lift(now);
        }
        if let Some(t) = self.last_touch
            && self.touchpad.active() > 0
            && now.duration_since(t) >= TOUCH_SILENCE
        {
            return self.cancel();
        }
        let due = self.due(now);
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

    /// Whether the host's own gesture events (swipe, magnify, rotate, …) belong to a guest
    /// three-finger sequence and must not reach macOS — which would otherwise switch Spaces
    /// or open Mission Control under the guest's swipe. For the whole sequence, not the
    /// instant: a staggered lift passes the last fingers' events on a per-instant count.
    /// Two-finger sequences stay macOS's to recognise: their taps are the guest's clicks.
    pub fn swallows_gestures(&self) -> bool {
        self.owner == Some(Owner::Guest) && self.peak >= 3
    }

    /// A transition that ends forwarding for the rest of this sequence (focus loss, capture
    /// toggling, the VM parking): lift the guest's fingers and give the sequence to the host.
    pub fn cancel(&mut self) -> Vec<InputEvent> {
        self.pending = None;
        self.not_before = None;
        self.thin = None;
        self.uncommitted = None;
        self.held_at = None;
        if self.owner.is_some() {
            self.owner = Some(Owner::Host);
        }
        self.momentum_guest = false;
        self.touchpad.release_all()
    }
}

/// One input the policy consumed, as `LIMINA_TRACKPAD_RECORD` writes it (one JSON object per
/// line). A recording of real hands is the fixture the policy is tested against: AppKit's
/// quirks — touches vanishing while the pad is pressed, a click arriving before its touches,
/// counts flickering — are in the data, where no synthetic gesture would think to put them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Recorded {
    /// A gesture event's local fingers (after the phase filter), with the gate.
    Touches {
        t_us: u64,
        in_view: bool,
        touches: Vec<TouchSample>,
    },
    /// A trackpad mouse-button edge.
    Click {
        t_us: u64,
        down: bool,
        secondary: bool,
    },
}

/// Replaying a [`Recorded`] input stream through the policy — the test harness, and the
/// producer of the stream the guest-side libinput oracle judges.
#[cfg(test)]
pub(crate) mod replay {
    use super::*;
    use limina_input::constants::{BTN_LEFT, BTN_RIGHT, EV_KEY};

    impl Recorded {
        pub fn t_us(&self) -> u64 {
            match self {
                Recorded::Touches { t_us, .. } | Recorded::Click { t_us, .. } => *t_us,
            }
        }
    }

    /// Which guest device an output event went to.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Dev {
        Touchpad,
        Tablet,
    }

    /// One event the guest received, stamped on the recording's clock.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Delivered {
        pub t_us: u64,
        pub dev: Dev,
        pub ev: InputEvent,
    }

    /// The render tick's period, which [`replay`] reproduces between recorded inputs.
    pub const TICK: Duration = Duration::from_micros(16_667);

    /// Drive the policy through a recording exactly as `InputState` does — each input at its
    /// recorded time, [`TrackpadSeq::tick`] at the render tick's cadence in between — and return
    /// what the guest's touchpad and tablet would have received. A click the policy leaves to the
    /// host becomes a tablet button press or release.
    pub fn replay(geometry: TouchpadGeometry, records: &[Recorded]) -> Vec<Delivered> {
        let base = Instant::now();
        let at = |t_us: u64| base + Duration::from_micros(t_us);
        let mut seq = TrackpadSeq::new(geometry);
        let mut out = Vec::new();
        let mut emit = |t_us: u64, dev: Dev, events: Vec<InputEvent>| {
            out.extend(events.into_iter().map(|ev| Delivered { t_us, dev, ev }));
        };
        let tick_us = TICK.as_micros() as u64;
        let mut next_tick = records.first().map_or(0, Recorded::t_us);
        let end = records.last().map_or(0, Recorded::t_us) + 1_000_000;
        for r in records.iter().map(Some).chain(std::iter::once(None)) {
            let until = r.map_or(end, Recorded::t_us);
            while next_tick < until {
                let events = seq.tick(at(next_tick));
                emit(next_tick, Dev::Touchpad, events);
                next_tick += tick_us;
            }
            match r {
                Some(Recorded::Touches {
                    t_us,
                    in_view,
                    touches,
                }) => {
                    let events = seq.on_touches(touches, *in_view, at(*t_us));
                    emit(*t_us, Dev::Touchpad, events);
                }
                Some(Recorded::Click {
                    t_us,
                    down,
                    secondary,
                }) => {
                    let btn = if *secondary { BTN_RIGHT } else { BTN_LEFT };
                    emit(
                        *t_us,
                        Dev::Tablet,
                        vec![
                            InputEvent::new(EV_KEY, btn, *down as i32),
                            InputEvent::syn(),
                        ],
                    );
                }
                None => {}
            }
        }
        out
    }

    /// Parse a recording (JSON lines; blank lines and `#` comments skipped).
    pub fn parse_recording(text: &str) -> Result<Vec<Recorded>, serde_json::Error> {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(serde_json::from_str)
            .collect()
    }
}

/// Recorded real-hand batteries, replayed through the policy and judged by what libinput would
/// make of the result. macOS's own click events are the ground truth for what the user meant:
/// each host secondary click is one intended right-click (a two-finger tap or physical click),
/// each primary click one left-click.
#[cfg(test)]
mod recordings {
    use super::replay::*;
    use super::*;
    use limina_input::constants::*;

    /// libinput's tap window and motion threshold (`evdev-mt-touchpad-tap.c`): a touch that
    /// lifts within this long of landing, having moved less than this far, is a tap.
    const TAP_TIMEOUT_US: u64 = 180_000;
    const TAP_MOVE_UNITS: i32 = 130; // 1.3 mm at 100 units/mm

    const BATTERY_1: &str = include_str!("../../testdata/trackpad/battery-1.jsonl");
    /// Scrolls, flicks, pinches, and one- and two-finger taps and clicks, captured and not.
    const BATTERY_2: &str = include_str!("../../testdata/trackpad/battery-2.jsonl");
    /// Through the HID gesture tap: two-finger flicks, slow scrolls that stop before the lift,
    /// and three-finger swipes.
    const BATTERY_3: &str = include_str!("../../testdata/trackpad/battery-3.jsonl");

    fn geometry() -> TouchpadGeometry {
        TouchpadGeometry::BUILT_IN
    }

    /// Each touch episode the guest touchpad saw (BTN_TOUCH down → up): its duration, and
    /// the largest distance any contact moved from where it landed.
    fn episodes(out: &[Delivered]) -> Vec<(u64, u64, i32)> {
        let mut eps = Vec::new();
        let mut start = None;
        let mut slot = 0usize;
        let mut landed: [Option<(i32, i32)>; MAX_SLOTS] = [None; MAX_SLOTS];
        let mut pos: [(i32, i32); MAX_SLOTS] = [(0, 0); MAX_SLOTS];
        let mut moved = 0i32;
        for d in out.iter().filter(|d| d.dev == Dev::Touchpad) {
            let e = d.ev;
            match (e.type_, e.code) {
                (EV_ABS, ABS_MT_SLOT) => slot = e.value as usize,
                (EV_ABS, ABS_MT_TRACKING_ID) if e.value < 0 => landed[slot] = None,
                (EV_ABS, ABS_MT_TRACKING_ID) => landed[slot] = Some((-1, -1)),
                (EV_ABS, ABS_MT_POSITION_X) => pos[slot].0 = e.value,
                (EV_ABS, ABS_MT_POSITION_Y) => pos[slot].1 = e.value,
                (EV_KEY, BTN_TOUCH) if e.value == 1 => {
                    start = Some(d.t_us);
                    moved = 0;
                }
                (EV_KEY, BTN_TOUCH) => {
                    if let Some(s) = start.take() {
                        eps.push((s, d.t_us - s, moved));
                    }
                }
                (EV_SYN, _) => {
                    for s in 0..MAX_SLOTS {
                        match landed[s] {
                            Some((-1, -1)) => landed[s] = Some(pos[s]),
                            Some((x, y)) => {
                                moved = moved.max((pos[s].0 - x).abs().max((pos[s].1 - y).abs()))
                            }
                            None => {}
                        }
                    }
                }
                _ => {}
            }
        }
        eps
    }

    fn presses(out: &[Delivered], dev: Dev, btn: u16) -> usize {
        out.iter()
            .filter(|d| d.dev == dev && d.ev == InputEvent::new(EV_KEY, btn, 1))
            .count()
    }

    fn check(recording: &str) {
        let records = parse_recording(recording).expect("recording parses");
        let intended = |secondary: bool| {
            records
                .iter()
                .filter(|r| {
                    matches!(r, Recorded::Click { down: true, secondary: s, .. } if *s == secondary)
                })
                .count()
        };
        let out = replay(geometry(), &records);

        // Every click macOS recognised reaches the guest once, through the tablet…
        assert_eq!(presses(&out, Dev::Tablet, BTN_RIGHT), intended(true));
        assert_eq!(presses(&out, Dev::Tablet, BTN_LEFT), intended(false));
        // …and the touchpad adds none of its own: no button, and no touch libinput would
        // read as a tap.
        assert_eq!(presses(&out, Dev::Touchpad, BTN_LEFT), 0, "touchpad button");
        let taps: Vec<_> = episodes(&out)
            .into_iter()
            .filter(|&(_, dur, moved)| dur < TAP_TIMEOUT_US && moved < TAP_MOVE_UNITS)
            .collect();
        assert!(
            taps.is_empty(),
            "{} touch episode(s) libinput would read as a tap (start_us, dur_us, moved): {taps:?}",
            taps.len()
        );
        // The recording's gestures do reach the guest.
        assert!(!episodes(&out).is_empty(), "no gesture reached the guest");
    }

    #[test]
    fn battery_1_gives_one_click_per_intended_click() {
        check(BATTERY_1);
    }

    #[test]
    fn battery_2_gives_one_click_per_intended_click() {
        check(BATTERY_2);
    }

    #[test]
    fn battery_3_gives_one_click_per_intended_click() {
        check(BATTERY_3);
    }

    /// The frames libinput would discard as a touch jump (`tp_detect_jumps` in
    /// `evdev-mt-touchpad.c`): a contact moving more than 20 mm, or 7 mm more than in its last
    /// frame, per 12 ms, between frames up to 30 ms apart. Returns (t_us, slot, mm).
    fn jumps(out: &[Delivered]) -> Vec<(u64, usize, f64)> {
        let mut found = Vec::new();
        let mut slot = 0usize;
        let mut down = [false; MAX_SLOTS];
        let mut pos = [(0i32, 0i32); MAX_SLOTS];
        let mut moved = [false; MAX_SLOTS];
        // Per slot: the last frame's time, position, and normalized speed.
        type Last = Option<(u64, (i32, i32), f64)>;
        let mut last: [Last; MAX_SLOTS] = [None; MAX_SLOTS];
        for d in out.iter().filter(|d| d.dev == Dev::Touchpad) {
            let e = d.ev;
            match (e.type_, e.code) {
                (EV_ABS, ABS_MT_SLOT) => slot = e.value as usize,
                (EV_ABS, ABS_MT_TRACKING_ID) => {
                    down[slot] = e.value >= 0;
                    last[slot] = None;
                }
                (EV_ABS, ABS_MT_POSITION_X) => {
                    pos[slot].0 = e.value;
                    moved[slot] = true;
                }
                (EV_ABS, ABS_MT_POSITION_Y) => {
                    pos[slot].1 = e.value;
                    moved[slot] = true;
                }
                (EV_SYN, _) => {
                    for s in (0..MAX_SLOTS).filter(|&s| down[s] && moved[s]) {
                        let mut speed = 0.0;
                        if let Some((t, (x, y), prev)) = last[s] {
                            let dt_ms = (d.t_us - t) as f64 / 1000.0;
                            let mm = f64::from((pos[s].0 - x).pow(2) + (pos[s].1 - y).pow(2))
                                .sqrt()
                                / TOUCHPAD_RES as f64;
                            if dt_ms > 0.0 && dt_ms <= 30.0 {
                                speed = mm * 12.0 / dt_ms;
                                if speed > 20.0 || speed - prev > 7.0 {
                                    found.push((d.t_us, s, mm));
                                }
                            }
                        }
                        last[s] = Some((d.t_us, pos[s], speed));
                    }
                    moved = [false; MAX_SLOTS];
                }
                _ => {}
            }
        }
        found
    }

    #[test]
    fn no_battery_shows_the_guest_a_touch_jump() {
        for (name, battery) in [("1", BATTERY_1), ("2", BATTERY_2), ("3", BATTERY_3)] {
            let records = parse_recording(battery).expect("recording parses");
            let found = jumps(&replay(geometry(), &records));
            assert!(
                found.is_empty(),
                "battery-{name}: {} frame(s) libinput would discard as a touch jump \
                 (t_us, slot, mm): {found:?}",
                found.len()
            );
        }
    }

    /// Not a check: writes a recording's replay for the guest-side libinput oracle
    /// (`scripts/trackpad-oracle.sh`), which judges it with the real libinput. Reads
    /// `TRACKPAD_RECORDING`, writes `TRACKPAD_REPLAY_OUT` as `t_us dev type code value` lines
    /// under a `# intended right=N left=M` header.
    #[test]
    #[ignore = "a tool for scripts/trackpad-oracle.sh, not a check"]
    fn dump_replay() {
        use std::fmt::Write;
        let input = std::env::var("TRACKPAD_RECORDING").expect("TRACKPAD_RECORDING");
        let output = std::env::var("TRACKPAD_REPLAY_OUT").expect("TRACKPAD_REPLAY_OUT");
        let records = parse_recording(&std::fs::read_to_string(input).expect("read recording"))
            .expect("recording parses");
        let intended = |secondary: bool| {
            records
                .iter()
                .filter(|r| {
                    matches!(r, Recorded::Click { down: true, secondary: s, .. } if *s == secondary)
                })
                .count()
        };
        let mut text = format!(
            "# intended right={} left={}\n",
            intended(true),
            intended(false)
        );
        for d in replay(geometry(), &records) {
            let dev = match d.dev {
                Dev::Touchpad => "touchpad",
                Dev::Tablet => "pointer",
            };
            let _ = writeln!(
                text,
                "{} {dev} {} {} {}",
                d.t_us, d.ev.type_, d.ev.code, d.ev.value
            );
        }
        std::fs::write(output, text).expect("write replay");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use limina_input::constants::*;

    fn t(id: u64, x: f64, y: f64) -> TouchSample {
        TouchSample {
            id,
            x,
            y,
            resting: false,
        }
    }

    fn resting(id: u64, x: f64, y: f64) -> TouchSample {
        TouchSample {
            resting: true,
            ..t(id, x, y)
        }
    }

    /// 100 × 50 mm, so 0.01 of the width is 1 mm and the commit move is 0.03.
    fn seq() -> TrackpadSeq {
        TrackpadSeq::new(TouchpadGeometry {
            width: 10_000,
            height: 5_000,
        })
    }

    fn slots_down(ev: &[InputEvent]) -> usize {
        ev.iter()
            .filter(|e| e.type_ == EV_ABS && e.code == ABS_MT_TRACKING_ID && e.value >= 0)
            .count()
    }

    fn lifted(ev: &[InputEvent]) -> bool {
        ev.contains(&InputEvent::new(EV_KEY, BTN_TOUCH, 0))
    }

    /// Two fingers land and slide 5 mm: committed, and the guest holds them.
    fn committed(s: &mut TrackpadSeq, now: Instant) {
        s.on_touches(&[t(1, 0.10, 0.10), t(2, 0.20, 0.20)], true, now);
        let ev = s.on_touches(&[t(1, 0.15, 0.10), t(2, 0.25, 0.20)], true, now);
        assert_eq!(slots_down(&ev), 2);
    }

    #[test]
    fn a_still_quick_touch_never_reaches_the_guest() {
        // A two-finger tap: land, lift 90 ms later. The guest must not see it — it would read
        // it as a tap, beside macOS's own click for it.
        let mut s = seq();
        let now = Instant::now();
        assert!(
            s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], true, now)
                .is_empty()
        );
        assert!(
            s.swallows_scroll(),
            "the sequence is the guest's all the same"
        );
        let up = now + Duration::from_millis(90);
        assert!(s.on_touches(&[], true, up).is_empty());
        assert!(s.tick(up + THIN_GRACE).is_empty());
    }

    #[test]
    fn motion_commits_with_the_landing_first() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.10, 0.10), t(2, 0.20, 0.20)], true, now);
        // 2 mm: not yet.
        assert!(
            s.on_touches(&[t(1, 0.12, 0.10), t(2, 0.22, 0.20)], true, now)
                .is_empty()
        );
        // 4 mm: the guest gets the landing now…
        let ev = s.on_touches(&[t(1, 0.14, 0.10), t(2, 0.24, 0.20)], true, now);
        assert_eq!(slots_down(&ev), 2);
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 1000)));
        // …and the motion as the next paced frame.
        let ev = s.tick(now + MIN_FRAME_INTERVAL);
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 1400)));
    }

    #[test]
    fn a_still_hold_commits_and_is_held_past_the_tap_timeout() {
        let mut s = seq();
        let now = Instant::now();
        let two = [t(1, 0.1, 0.1), t(2, 0.2, 0.2)];
        s.on_touches(&two, true, now);
        let held = now + COMMIT_HOLD;
        assert_eq!(slots_down(&s.on_touches(&two, true, held)), 2);
        // The fingers lift right away; the guest keeps them down until the guard has passed.
        s.on_touches(&[], true, held);
        assert!(s.tick(held + THIN_GRACE).is_empty());
        assert!(lifted(&s.tick(held + TAP_GUARD)));
    }

    #[test]
    fn a_sequence_begun_outside_the_vm_stays_the_hosts() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.1, 0.1), t(2, 0.2, 0.2)], false, now);
        // Moving over the VM mid-sequence does not hand it over.
        assert!(
            s.on_touches(&[t(1, 0.2, 0.1), t(2, 0.3, 0.3)], true, now)
                .is_empty()
        );
        assert!(!s.swallows_scroll());
    }

    #[test]
    fn a_guest_three_finger_sequence_takes_the_hosts_gestures_until_it_ends() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        assert!(
            !s.swallows_gestures(),
            "two fingers stay macOS's to recognise"
        );
        let three = [t(1, 0.15, 0.1), t(2, 0.25, 0.2), t(3, 0.3, 0.3)];
        s.on_touches(&three, true, now);
        assert!(s.swallows_gestures(), "from the event that reaches three");
        // Through the staggered lift, down to none, until the sequence ends.
        let up = now + TAP_GUARD;
        s.on_touches(&three[..1], true, up);
        assert!(s.swallows_gestures());
        s.on_touches(&[], true, up);
        s.tick(up);
        assert!(
            s.swallows_gestures(),
            "the lift grace is still the sequence"
        );
        s.tick(up + THIN_GRACE);
        assert!(!s.swallows_gestures(), "ended");
    }

    #[test]
    fn a_three_finger_sequence_begun_outside_the_vm_is_left_to_macos() {
        let mut s = seq();
        let now = Instant::now();
        let three = [t(1, 0.1, 0.1), t(2, 0.2, 0.2), t(3, 0.3, 0.3)];
        s.on_touches(&three, false, now);
        s.on_touches(&three, true, now);
        assert!(!s.swallows_gestures());
    }

    #[test]
    fn with_three_fingers_off_a_third_finger_is_the_hosts() {
        let mut s = seq();
        s.set_three_fingers(false);
        let now = Instant::now();
        committed(&mut s, now);
        let three = [t(1, 0.15, 0.1), t(2, 0.25, 0.2), t(3, 0.3, 0.3)];
        assert!(lifted(&s.on_touches(&three, true, now)));
        assert!(!s.swallows_gestures());
        assert!(
            !s.swallows_scroll(),
            "the host's gesture, the host's scroll"
        );
    }

    #[test]
    fn turning_three_fingers_off_mid_sequence_hands_it_to_the_host() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        let three = [t(1, 0.15, 0.1), t(2, 0.25, 0.2), t(3, 0.3, 0.3)];
        s.on_touches(&three, true, now);
        assert!(lifted(&s.set_three_fingers(false)));
        assert!(!s.swallows_gestures());
        assert!(
            s.set_three_fingers(false).is_empty(),
            "unchanged: nothing to do"
        );
    }

    #[test]
    fn a_fourth_finger_takes_the_sequence_for_the_host_and_lifts_the_guest() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        let four = [
            t(1, 0.15, 0.1),
            t(2, 0.25, 0.2),
            t(3, 0.3, 0.3),
            t(4, 0.4, 0.4),
        ];
        assert!(lifted(&s.on_touches(&four, true, now)));
        assert!(!s.swallows_scroll());
        // Back down to three: still the host's (the peak decides).
        assert!(s.on_touches(&four[..3], true, now).is_empty());
    }

    #[test]
    fn dropping_to_one_finger_lifts_the_guest_but_keeps_the_sequence() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        s.tick(now + MIN_FRAME_INTERVAL);
        let later = now + TAP_GUARD;
        assert!(s.on_touches(&[t(2, 0.25, 0.2)], true, later).is_empty());
        assert!(lifted(&s.tick(later + THIN_GRACE)));
        assert!(s.swallows_scroll(), "the sequence is still the guest's");
    }

    #[test]
    fn momentum_after_a_guest_sequence_is_swallowed_until_the_next_touch() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        let up = now + TAP_GUARD;
        assert!(lifted(&s.on_touches(&[], true, up)));
        s.tick(up + THIN_GRACE);
        assert!(s.swallows_scroll(), "the momentum tail is the guest's");
        s.on_touches(&[t(3, 0.1, 0.1)], true, up + THIN_GRACE * 2);
        assert!(!s.swallows_scroll(), "a new touch ends the old momentum");
    }

    #[test]
    fn leaving_the_vm_mid_sequence_hands_it_to_the_host() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        let ev = s.on_touches(&[t(1, 0.15, 0.1), t(2, 0.3, 0.3)], false, now);
        assert!(lifted(&ev));
        assert!(!s.swallows_scroll());
    }

    #[test]
    fn motion_is_paced_but_landings_are_not() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        let t0 = now + MIN_FRAME_INTERVAL;
        s.tick(t0); // the committing motion goes out
        let soon = t0 + MIN_FRAME_INTERVAL / 4;
        assert!(
            s.on_touches(&[t(1, 0.15, 0.1), t(2, 0.3, 0.2)], true, soon)
                .is_empty()
        );
        // A third finger landing is not held back, but the fingers already down stay where
        // the guest last saw them: moved in the same frame, so soon after the last, libinput
        // would read their motion as a touch jump and discard it.
        let ev = s.on_touches(
            &[t(1, 0.15, 0.1), t(2, 0.4, 0.2), t(3, 0.5, 0.5)],
            true,
            soon,
        );
        assert!(ev.contains(&InputEvent::new(EV_KEY, BTN_TOOL_TRIPLETAP, 1)));
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 5000)));
        assert!(!ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 4000)));
        // …and held motion goes out once the spacing has passed.
        s.on_touches(
            &[t(1, 0.15, 0.1), t(2, 0.4, 0.2), t(3, 0.6, 0.5)],
            true,
            soon + MIN_FRAME_INTERVAL / 2,
        );
        let ev = s.tick(soon + MIN_FRAME_INTERVAL);
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 4000)));
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 6000)));
    }

    #[test]
    fn a_resting_thumb_does_not_make_a_pointer_move_two_fingers() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[resting(1, 0.1, 0.9), t(2, 0.5, 0.5)], true, now);
        assert!(!s.swallows_scroll());
    }

    #[test]
    fn guest_fingers_that_come_to_rest_keep_counting() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        let still = [resting(1, 0.15, 0.1), resting(2, 0.25, 0.2)];
        s.on_touches(&still, true, now);
        assert!(!lifted(&s.tick(now + TAP_GUARD * 2)), "no lift");
    }

    /// A fast flick is well past the commit distance by the time it commits, and the guest
    /// sees that whole distance in the frame after the landing. libinput discards a frame
    /// that moves more than 7 mm per 12 ms from rest as a touch jump — and with it the start
    /// of the scroll — so that frame is spaced by its distance.
    #[test]
    fn a_fast_commit_is_spaced_so_the_guest_does_not_read_a_jump() {
        let mut s = seq();
        let now = Instant::now();
        s.on_touches(&[t(1, 0.10, 0.10), t(2, 0.20, 0.20)], true, now);
        let ev = s.on_touches(&[t(1, 0.18, 0.10), t(2, 0.28, 0.20)], true, now);
        assert_eq!(slots_down(&ev), 2, "the landing goes out at once");
        // A newer sample does not replace the committing one, which is still not due.
        let later = now + MIN_FRAME_INTERVAL;
        assert!(
            s.on_touches(&[t(1, 0.19, 0.10), t(2, 0.29, 0.20)], true, later)
                .is_empty()
        );
        assert!(s.tick(later).is_empty());
        let ev = s.tick(now + Duration::from_millis(16));
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 1800)));
    }

    #[test]
    fn a_flick_lifts_the_moment_the_fingers_do() {
        // A kinetic scroll's velocity is read from the last motion before the lift; fingers
        // held still in the guest past that read as a stop. A moved commit can never be a
        // tap, so nothing is held back — including the motion still waiting on the pacing.
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        let up = now + MIN_FRAME_INTERVAL / 2;
        assert!(
            s.on_touches(&[], true, up).is_empty(),
            "the motion is not due"
        );
        let ev = s.tick(now + MIN_FRAME_INTERVAL);
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 1500)));
        assert!(lifted(&ev));
        assert!(s.tick(up + THIN_GRACE).is_empty(), "lifted once");
        assert!(s.swallows_scroll(), "the momentum tail is the guest's");
        // After a lift with no motion held back, nothing waits at all.
        let mut s = seq();
        committed(&mut s, now);
        s.tick(now + MIN_FRAME_INTERVAL);
        assert!(lifted(&s.on_touches(&[], true, now + MIN_FRAME_INTERVAL)));
    }

    #[test]
    fn a_flick_losing_a_finger_at_the_commit_still_shows_its_motion() {
        // battery-2 at 171.67 s: the commit's motion was still held by the pacing when one
        // finger lifted. Dropped, the guest saw a still touch lift 63 ms later — a tap.
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        assert!(s.on_touches(&[t(2, 0.25, 0.2)], true, now).is_empty());
        let ev = s.tick(now + MIN_FRAME_INTERVAL);
        assert!(ev.contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 1500)));
        assert!(!lifted(&ev));
        assert!(lifted(&s.tick(now + THIN_GRACE)));
    }

    #[test]
    fn a_count_flicker_does_not_lift_the_guests_fingers() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        s.tick(now + MIN_FRAME_INTERVAL);
        let blip = now + TAP_GUARD;
        assert!(s.on_touches(&[t(2, 0.25, 0.2)], true, blip).is_empty());
        assert!(s.tick(blip).is_empty());
        let ev = s.on_touches(
            &[t(1, 0.15, 0.1), t(2, 0.26, 0.2)],
            true,
            blip + THIN_GRACE / 5,
        );
        assert_eq!(slots_down(&ev), 0);
        assert!(!lifted(&ev));
    }

    #[test]
    fn silence_lifts_the_guest_fingers_once() {
        let mut s = seq();
        let now = Instant::now();
        committed(&mut s, now);
        s.tick(now + MIN_FRAME_INTERVAL);
        assert!(s.tick(now + TOUCH_SILENCE / 2).is_empty());
        assert!(lifted(&s.tick(now + TOUCH_SILENCE)));
        assert!(s.tick(now + TOUCH_SILENCE * 2).is_empty());
        // The rest of the sequence is the host's: a late event does not re-press.
        assert!(
            s.on_touches(&[t(1, 0.15, 0.1), t(2, 0.25, 0.2)], true, now)
                .is_empty()
        );
    }
}
