// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! What the host is doing to this VM's windows and to the supervisor: the record a timing
//! measurement needs to be trusted.
//!
//! The window tick samples the state on the main thread (`window::host_observe`, the only place
//! AppKit answers) and hands it to [`observe`]. This module keeps the latest state and the
//! counters since the first sample, behind a mutex, so the threads that report it — the debug
//! port's responder and the frame capture's start — never touch AppKit. A change is returned to
//! the caller, which writes it into a running frame capture.
//!
//! Which of these states throttle what was measured, not assumed: `docs/graphics.md` §8
//! ("Host state and timing honesty").

use std::sync::Mutex;

use limina_framecap::HostState;

/// The latest state and what has been counted since the first sample.
#[derive(Default)]
pub(crate) struct Tracker {
    state: Option<HostState>,
    /// `CLOCK_MONOTONIC_RAW` ns of the first sample and of the latest.
    first_ns: u64,
    last_ns: u64,
    /// Changes after the first sample.
    transitions: u64,
    /// Time spent, up to the latest sample, with no window on screen and with the main thread
    /// throttled.
    not_visible_ns: u64,
    throttled_ns: u64,
}

/// The tracker's view at one instant: the state and the counters brought up to it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) state: HostState,
    pub(crate) transitions: u64,
    pub(crate) observed_ms: u64,
    pub(crate) not_visible_ms: u64,
    pub(crate) throttled_ms: u64,
}

impl Tracker {
    /// A sample taken at `t_ns`. The time since the previous one is charged to the previous
    /// state, which held until now. `true` when the state changed (the first sample counts as
    /// one, and is no transition).
    pub(crate) fn observe(&mut self, state: HostState, t_ns: u64) -> bool {
        match &self.state {
            None => self.first_ns = t_ns,
            Some(prev) => self.charge(prev.clone(), t_ns),
        }
        self.last_ns = self.last_ns.max(t_ns);
        let changed = self.state.as_ref().is_none_or(|p| !p.same_as(&state));
        if changed && self.state.is_some() {
            self.transitions += 1;
        }
        self.state = Some(state);
        changed
    }

    fn charge(&mut self, prev: HostState, t_ns: u64) {
        let dt = t_ns.saturating_sub(self.last_ns);
        if !prev.visible() {
            self.not_visible_ns += dt;
        }
        if prev.throttled {
            self.throttled_ns += dt;
        }
    }

    /// The state and the counters as of `now_ns`, the current state charged up to it. `None`
    /// before the first sample.
    pub(crate) fn snapshot(&self, now_ns: u64) -> Option<Snapshot> {
        let state = self.state.clone()?;
        let mut t = Tracker {
            state: None,
            ..*self
        };
        t.charge(state.clone(), now_ns.max(self.last_ns));
        let ms = |ns: u64| ns / 1_000_000;
        Some(Snapshot {
            transitions: t.transitions,
            observed_ms: ms(now_ns.max(self.last_ns) - self.first_ns),
            not_visible_ms: ms(t.not_visible_ns),
            throttled_ms: ms(t.throttled_ns),
            state,
        })
    }
}

static TRACKER: Mutex<Tracker> = Mutex::new(Tracker {
    state: None,
    first_ns: 0,
    last_ns: 0,
    transitions: 0,
    not_visible_ns: 0,
    throttled_ns: 0,
});

fn lock() -> std::sync::MutexGuard<'static, Tracker> {
    TRACKER.lock().unwrap_or_else(|p| p.into_inner())
}

/// `CLOCK_MONOTONIC_RAW` in ns: the frame capture's clock, which keeps counting across sleep.
pub(crate) fn now_ns() -> u64 {
    crate::window::frame_capture::clock_ns(libc::CLOCK_MONOTONIC_RAW)
}

/// Record a sample taken at `t_ns`; the new state when it differs from the last one.
pub(crate) fn observe(state: HostState, t_ns: u64) -> Option<HostState> {
    let mut t = lock();
    t.observe(state, t_ns).then(|| t.state.clone()).flatten()
}

/// The latest sampled state, or `None` before the window has sampled any.
pub(crate) fn current() -> Option<HostState> {
    lock().state.clone()
}

/// The debug port's `host-state` answer, after `format`.
pub(crate) fn port_fields() -> Vec<(String, String)> {
    let snap = lock().snapshot(now_ns());
    fields(
        snap.as_ref(),
        crate::window::frame_capture::clock_ns(libc::CLOCK_REALTIME),
    )
}

fn yes_no(b: bool) -> String {
    if b { "yes" } else { "no" }.into()
}

/// The answer's fields for `snap` (none sampled yet: `state=unobserved`), stamped `t_realtime_ns`.
pub(crate) fn fields(snap: Option<&Snapshot>, t_realtime_ns: u64) -> Vec<(String, String)> {
    let mut f: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: String| f.push((k.to_string(), v));
    put("t_realtime_ns", t_realtime_ns.to_string());
    let Some(snap) = snap else {
        // A VM with no window (`--display-capture`, headless), or one asked before its first tick.
        put("state", "unobserved".into());
        return f;
    };
    let s = &snap.state;
    put("state", "observed".into());
    put("visible", yes_no(s.visible()));
    put("windows", s.windows.len().to_string());
    for w in &s.windows {
        put(&format!("window.{}.visible", w.slot), yes_no(w.visible));
        put(&format!("window.{}.minimized", w.slot), yes_no(w.minimized));
    }
    put("app_active", yes_no(s.app_active));
    put("app_hidden", yes_no(s.app_hidden));
    put("throttled", yes_no(s.throttled));
    put("main_thread_priority", s.main_thread_priority.to_string());
    put("displays", s.displays.to_string());
    put("displays_asleep", s.displays_asleep.to_string());
    let opt = |b: Option<bool>| b.map(yes_no).unwrap_or_else(|| "unknown".into());
    put("screen_locked", opt(s.screen_locked));
    put("on_console", opt(s.on_console));
    put("thermal_state", s.thermal_state.as_str().into());
    put("low_power_mode", yes_no(s.low_power_mode));
    put("no_throttle", yes_no(s.no_throttle));
    put("transitions", snap.transitions.to_string());
    put("observed_ms", snap.observed_ms.to_string());
    put("not_visible_ms", snap.not_visible_ms.to_string());
    put("throttled_ms", snap.throttled_ms.to_string());
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use limina_framecap::{Thermal, WindowState};

    const MS: u64 = 1_000_000;

    fn state(visible: bool, hidden: bool, priority: i32) -> HostState {
        HostState {
            windows: vec![WindowState {
                slot: 0,
                visible,
                minimized: false,
            }],
            app_active: visible,
            app_hidden: hidden,
            throttled: priority <= 4,
            main_thread_priority: priority,
            displays: 1,
            displays_asleep: 0,
            screen_locked: None,
            on_console: Some(true),
            thermal_state: Thermal::Nominal,
            low_power_mode: false,
            no_throttle: false,
        }
    }

    #[test]
    fn the_first_sample_is_a_change_but_no_transition() {
        let mut t = Tracker::default();
        assert!(t.observe(state(true, false, 31), 1000 * MS));
        let s = t.snapshot(1000 * MS).unwrap();
        assert_eq!((s.transitions, s.observed_ms, s.not_visible_ms), (0, 0, 0));
        assert_eq!(Tracker::default().snapshot(5), None);
    }

    #[test]
    fn changes_are_counted_and_time_is_charged_to_the_state_that_held() {
        let mut t = Tracker::default();
        let t0 = 1000 * MS;
        assert!(t.observe(state(true, false, 31), t0));
        // A priority wobble within the band is the same state.
        assert!(!t.observe(state(true, false, 47), t0 + 100 * MS));
        // Hidden at +200 ms, napped at +700 ms, shown again at +1500 ms.
        assert!(t.observe(state(false, true, 31), t0 + 200 * MS));
        assert!(t.observe(state(false, true, 4), t0 + 700 * MS));
        assert!(!t.observe(state(false, true, 4), t0 + 1000 * MS));
        assert!(t.observe(state(true, false, 31), t0 + 1500 * MS));
        let s = t.snapshot(t0 + 2000 * MS).unwrap();
        assert_eq!(s.transitions, 3);
        assert_eq!(s.observed_ms, 2000);
        assert_eq!(s.not_visible_ms, 1300);
        assert_eq!(s.throttled_ms, 800);
        assert!(s.state.visible());
    }

    #[test]
    fn a_snapshot_charges_the_current_state_up_to_now_without_changing_the_tracker() {
        let mut t = Tracker::default();
        t.observe(state(false, true, 4), 10 * MS);
        let s = t.snapshot(510 * MS).unwrap();
        assert_eq!(
            (s.not_visible_ms, s.throttled_ms, s.observed_ms),
            (500, 500, 500)
        );
        // Asking twice does not count twice.
        assert_eq!(t.snapshot(510 * MS).unwrap(), s);
        // A clock read before the last sample (another thread's) counts nothing extra.
        assert_eq!(t.snapshot(5 * MS).unwrap().observed_ms, 0);
    }

    #[test]
    fn the_port_answer_names_every_window_and_says_when_nothing_was_sampled() {
        let none = fields(None, 42);
        assert_eq!(
            none,
            vec![
                ("t_realtime_ns".to_string(), "42".to_string()),
                ("state".to_string(), "unobserved".to_string())
            ]
        );
        let mut t = Tracker::default();
        let mut s = state(false, true, 4);
        s.windows.push(WindowState {
            slot: 2,
            visible: false,
            minimized: true,
        });
        t.observe(s, 0);
        let f = fields(t.snapshot(250 * MS).as_ref(), 42);
        let get = |k: &str| f.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("state"), Some("observed"));
        assert_eq!(get("visible"), Some("no"));
        assert_eq!(get("windows"), Some("2"));
        assert_eq!(get("window.0.minimized"), Some("no"));
        assert_eq!(get("window.2.minimized"), Some("yes"));
        assert_eq!(get("app_hidden"), Some("yes"));
        assert_eq!(get("throttled"), Some("yes"));
        assert_eq!(get("main_thread_priority"), Some("4"));
        assert_eq!(get("screen_locked"), Some("unknown"));
        assert_eq!(get("on_console"), Some("yes"));
        assert_eq!(get("thermal_state"), Some("nominal"));
        assert_eq!(get("no_throttle"), Some("no"));
        assert_eq!(get("transitions"), Some("0"));
        assert_eq!(get("not_visible_ms"), Some("250"));
        assert_eq!(get("throttled_ms"), Some("250"));
        // Every key once: a reader taking the first match gets the only one.
        let mut keys: Vec<_> = f.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n);
    }
}
