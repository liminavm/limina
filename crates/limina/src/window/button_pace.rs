// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Spacing of the tablet's button events, so the guest's libinput never reads two clicks as
//! one bouncing switch.
//!
//! libinput debounces the buttons of every pointer device that is neither virtual nor a
//! touchpad — the guest tablet is one — over a 25 ms window
//! (`libinput-plugin-button-debounce.c`, `DEBOUNCE_TIMEOUT_BOUNCE`): a release followed by a
//! press and a release inside it is contact bounce, and the press and its release are dropped.
//! macOS sends a double-tap's second click exactly so: the first click's release, then the
//! second's press and release within ~0.1–15 ms of it (`spikes/raw-trackpad/`). The guest saw
//! one click, and a double-tap almost never double-clicked.
//!
//! Each transition of a button therefore goes out at least [`BUTTON_GAP`] after that button's
//! previous one; sooner ones wait, in order, and the render tick sends them when due.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The least time between two transitions of one button: libinput's 25 ms bounce window, with
/// margin.
pub(crate) const BUTTON_GAP: Duration = Duration::from_millis(30);

#[derive(Debug, Default)]
pub(crate) struct ButtonPacer {
    /// Each button's last transition sent, and when.
    last: Vec<(u16, Instant)>,
    /// Transitions waiting on the gap, oldest first. Order is kept across buttons too.
    queue: VecDeque<(u16, bool)>,
}

impl ButtonPacer {
    /// A button changed: the transitions to send now (it, or nothing if it has to wait).
    pub(crate) fn push(&mut self, btn: u16, down: bool, now: Instant) -> Vec<(u16, bool)> {
        self.queue.push_back((btn, down));
        self.due(now)
    }

    /// The waiting transitions whose gap has passed, in order; stops at the first that has
    /// to wait longer.
    pub(crate) fn due(&mut self, now: Instant) -> Vec<(u16, bool)> {
        let mut out = Vec::new();
        while let Some(&(btn, down)) = self.queue.front() {
            let last = self.last.iter().find(|(b, _)| *b == btn).map(|&(_, t)| t);
            if last.is_some_and(|t| now.duration_since(t) < BUTTON_GAP) {
                break;
            }
            self.queue.pop_front();
            match self.last.iter_mut().find(|(b, _)| *b == btn) {
                Some(entry) => entry.1 = now,
                None => self.last.push((btn, now)),
            }
            out.push((btn, down));
        }
        out
    }

    /// Whether transitions are waiting.
    pub(crate) fn pending(&self) -> bool {
        !self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: u16 = 0x110;
    const RIGHT: u16 = 0x111;

    #[test]
    fn a_double_tap_reaches_the_guest_as_two_clicks_libinput_keeps() {
        // macOS's double-tap: press, release 50 ms later, then the second click's press and
        // release 0.1 ms after that.
        let mut p = ButtonPacer::default();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_micros(n * 100);
        assert_eq!(p.push(LEFT, true, ms(0)), vec![(LEFT, true)]);
        assert_eq!(p.push(LEFT, false, ms(500)), vec![(LEFT, false)]);
        assert!(
            p.push(LEFT, true, ms(501)).is_empty(),
            "inside the bounce window"
        );
        assert!(p.push(LEFT, false, ms(502)).is_empty());
        assert!(p.pending());
        assert!(p.due(ms(700)).is_empty());
        assert_eq!(p.due(ms(800)), vec![(LEFT, true)]);
        assert!(p.due(ms(1000)).is_empty());
        assert_eq!(p.due(ms(1100)), vec![(LEFT, false)]);
        assert!(!p.pending());
    }

    #[test]
    fn ordinary_clicks_are_not_delayed() {
        let mut p = ButtonPacer::default();
        let t0 = Instant::now();
        assert_eq!(p.push(LEFT, true, t0), vec![(LEFT, true)]);
        let up = t0 + Duration::from_millis(90);
        assert_eq!(p.push(LEFT, false, up), vec![(LEFT, false)]);
        // Another button is its own switch.
        assert_eq!(p.push(RIGHT, true, up), vec![(RIGHT, true)]);
    }

    #[test]
    fn a_waiting_transition_holds_back_later_ones_of_other_buttons() {
        // Order is kept: a right press after a waiting left release must not overtake it.
        let mut p = ButtonPacer::default();
        let t0 = Instant::now();
        p.push(LEFT, true, t0);
        let soon = t0 + Duration::from_millis(5);
        assert!(p.push(LEFT, false, soon).is_empty());
        assert!(p.push(RIGHT, true, soon).is_empty());
        assert_eq!(p.due(t0 + BUTTON_GAP), vec![(LEFT, false), (RIGHT, true)]);
    }
}
