// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Read the session's idle inhibitors from the compositor itself, through `ext-idle-notify-v1`.
//!
//! A compositor keeps the truth about idle inhibitors to itself: a Wayland client inhibits with
//! `zwp_idle_inhibit_manager_v1`, a D-Bus one through whatever owns `org.freedesktop.ScreenSaver`,
//! and where that ends up differs per desktop (gnome-session on GNOME, the compositor on wlroots,
//! the compositor again on synoik, which took the name from gnome-session's proxy). No interface
//! lists them. What every compositor with `ext-idle-notify-v1` version 2 does state is the
//! *effect*, twice over:
//!
//!   - `get_idle_notification`: idle after the timeout, **unless something inhibits idle**;
//!   - `get_input_idle_notification` (version 2): idle after the timeout of no input, ignoring
//!     inhibitors.
//!
//! With both registered for the same timeout, "input idle but not idle" is an inhibitor. That
//! covers every route into the compositor at once, Wayland and D-Bus alike, because it reads the
//! compositor's own verdict rather than any one of its inputs.
//!
//! The reading only exists once the user has stopped giving input for [`IDLE_AFTER`]; before
//! that the answer is "not inhibited". That is enough for the host: its display sleeps only after
//! a minute or more without host input, and every guest input event is host input.
//!
//! The two notifications are separate timers in the compositor, so their `idled` events may
//! arrive in separate reads. A lone input-idle in that gap would read as an inhibitor for a
//! moment and make the host take and drop its assertion; [`SETTLE`] holds the verdict back
//! until it has stood that long.
//!
//! A notification that is already idle does not always notice an inhibitor that comes or goes
//! afterwards: wlroots resumes it when one appears, KWin does not, so on KWin a video started
//! (or stopped) while the user is away would go unseen until the next keystroke. While input is
//! idle the inhibitor-respecting notification is therefore replaced every [`REARM_EVERY`]: a
//! fresh one goes idle only if nothing inhibits by then. The verdict published before the
//! replacement stands until the fresh one has had time to answer.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

/// How long without input before the compositor's two verdicts can disagree. Short next to the
/// shortest display-sleep timeout a Mac offers (a minute), so the host hears about an inhibitor
/// long before it would act on its absence.
pub const IDLE_AFTER: Duration = Duration::from_secs(5);

/// How long "input idle but not idle" must stand before it counts as an inhibitor.
pub const SETTLE: Duration = Duration::from_secs(1);

/// How often, while input is idle, the inhibitor-respecting notification is replaced to catch an
/// inhibitor that came or went since it went idle. With [`IDLE_AFTER`] and [`SETTLE`] it bounds
/// how late such a change is reported, well inside a minute.
pub const REARM_EVERY: Duration = Duration::from_secs(20);

/// How often the follow loop wakes with nothing to read, so a settling verdict is published on
/// time.
const TICK: Duration = Duration::from_millis(250);

/// Which of the two notifications an event is for.
#[derive(Clone, Copy)]
enum Kind {
    /// Respects idle inhibitors.
    Idle,
    /// Ignores them.
    InputIdle,
}

/// The two notifications' states and the clocks around them: the pure half, tested without a
/// compositor.
#[derive(Debug, Default)]
pub struct Verdict {
    idle: bool,
    input_idle: bool,
    /// When "input idle but not idle" last began, while it holds.
    disagreeing_since: Option<Instant>,
    /// When input last went idle, while it is.
    input_idle_since: Option<Instant>,
    /// When the inhibitor-respecting notification was last replaced.
    rearmed_at: Option<Instant>,
    /// The verdict to keep reporting until the replacement has had time to answer.
    held: Option<(Instant, bool)>,
}

impl Verdict {
    fn set(&mut self, kind: Kind, idle: bool, now: Instant) {
        match kind {
            Kind::Idle => self.idle = idle,
            Kind::InputIdle => {
                self.input_idle = idle;
                self.input_idle_since = idle.then_some(now);
                // Input itself settles the question: active means not reported, idle starts a
                // fresh reading.
                self.held = None;
            }
        }
        self.recompute(now);
    }

    fn recompute(&mut self, now: Instant) {
        let disagree = self.input_idle && !self.idle;
        match (disagree, self.disagreeing_since) {
            (true, None) => self.disagreeing_since = Some(now),
            (false, _) => self.disagreeing_since = None,
            _ => {}
        }
    }

    /// Whether the inhibitor-respecting notification is due for replacement.
    fn rearm_due(&self, now: Instant) -> bool {
        let Some(since) = self.input_idle_since else {
            return false;
        };
        let last = self.rearmed_at.map_or(since, |r| r.max(since));
        now.duration_since(last) >= REARM_EVERY
    }

    /// The inhibitor-respecting notification was just replaced by a fresh, not-yet-idle one.
    fn rearmed(&mut self, now: Instant) {
        let before = self.inhibited(now);
        // The fresh notification answers IDLE_AFTER from now at the earliest; a second SETTLE of
        // margin keeps a late `idled` from reading as an inhibitor for a tick.
        self.held = Some((now + IDLE_AFTER + 2 * SETTLE, before));
        self.rearmed_at = Some(now);
        self.idle = false;
        self.recompute(now);
    }

    /// Whether something in the session inhibits idle, as of `now`.
    pub fn inhibited(&self, now: Instant) -> bool {
        if let Some((until, held)) = self.held
            && now < until
        {
            return held;
        }
        self.disagreeing_since
            .is_some_and(|since| now.duration_since(since) >= SETTLE)
    }
}

struct State {
    verdict: Verdict,
}

/// A live subscription to the compositor's idle verdicts.
pub struct Compositor {
    notifier: ExtIdleNotifierV1,
    seat: wl_seat::WlSeat,
    /// The inhibitor-respecting notification, replaced while input is idle (see the module doc).
    idle: ExtIdleNotificationV1,
    // Kept alive: dropping a notification object would unsubscribe it.
    _input_idle: ExtIdleNotificationV1,
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
}

/// Subscribe, or say why this compositor cannot be read this way.
pub fn probe() -> Result<Compositor, String> {
    let conn = crate::wayland_clip::connect_display().map_err(|e| format!("no Wayland: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    // Version 2 or nothing: without the input-only notification there is no second verdict to
    // compare against, and one alone cannot tell an inhibitor from a user at the keyboard.
    let notifier = globals
        .bind::<ExtIdleNotifierV1, _, _>(&qh, 2..=2, ())
        .map_err(|e| format!("no ext-idle-notify-v1 version 2: {e}"))?;
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=1, ())
        .map_err(|e| format!("no seat: {e}"))?;
    let ms = IDLE_AFTER.as_millis() as u32;
    let idle = notifier.get_idle_notification(ms, &seat, &qh, Kind::Idle);
    let input_idle = notifier.get_input_idle_notification(ms, &seat, &qh, Kind::InputIdle);
    let mut state = State {
        verdict: Verdict::default(),
    };
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("roundtrip: {e}"))?;
    Ok(Compositor {
        notifier,
        seat,
        idle,
        _input_idle: input_idle,
        conn,
        queue,
        state,
    })
}

impl Compositor {
    /// Whether something inhibits idle right now.
    pub fn inhibited(&self) -> bool {
        self.state.verdict.inhibited(Instant::now())
    }

    /// Follow the compositor until the connection fails, handing every verdict to `publish`
    /// (repeats included; the caller dedups). Returns why it stopped.
    pub fn follow(mut self, mut publish: impl FnMut(bool)) -> String {
        loop {
            if let Err(e) = self.queue.dispatch_pending(&mut self.state) {
                return format!("dispatch: {e}");
            }
            let now = Instant::now();
            if self.state.verdict.rearm_due(now) {
                self.idle.destroy();
                self.idle = self.notifier.get_idle_notification(
                    IDLE_AFTER.as_millis() as u32,
                    &self.seat,
                    &self.queue.handle(),
                    Kind::Idle,
                );
                self.state.verdict.rearmed(now);
            }
            publish(self.state.verdict.inhibited(now));
            if let Err(e) = self.conn.flush() {
                return format!("flush: {e}");
            }
            // None means events are already queued: dispatch them before reading more.
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut fd, 1, TICK.as_millis() as libc::c_int) };
            if ready > 0
                && let Err(e) = guard.read()
            {
                return format!("read: {e}");
            }
            // A timeout or EINTR drops the guard, which cancels the read.
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

/// No events: the notifier only hands out notifications.
impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, Kind> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        kind: &Kind,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let idle = match event {
            ext_idle_notification_v1::Event::Idled => true,
            ext_idle_notification_v1::Event::Resumed => false,
            _ => return,
        };
        state.verdict.set(*kind, idle, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn idle_with_no_inhibitor_is_not_inhibited() {
        let t = Instant::now();
        let mut v = Verdict::default();
        v.set(Kind::InputIdle, true, at(t, 0));
        v.set(Kind::Idle, true, at(t, 3));
        assert!(!v.inhibited(at(t, 5000)));
    }

    #[test]
    fn input_idle_without_idle_is_an_inhibitor_once_it_settles() {
        let t = Instant::now();
        let mut v = Verdict::default();
        v.set(Kind::InputIdle, true, at(t, 0));
        assert!(
            !v.inhibited(at(t, 500)),
            "the other timer may simply not have fired yet"
        );
        assert!(v.inhibited(at(t, 1000)));
    }

    #[test]
    fn input_ends_the_inhibitor_reading() {
        let t = Instant::now();
        let mut v = Verdict::default();
        v.set(Kind::InputIdle, true, at(t, 0));
        assert!(v.inhibited(at(t, 2000)));
        v.set(Kind::InputIdle, false, at(t, 2100));
        assert!(!v.inhibited(at(t, 2200)));
    }

    #[test]
    fn an_inhibitor_released_while_idle_ends_the_reading() {
        let t = Instant::now();
        let mut v = Verdict::default();
        v.set(Kind::InputIdle, true, at(t, 0));
        assert!(v.inhibited(at(t, 2000)));
        v.set(Kind::Idle, true, at(t, 9000));
        assert!(!v.inhibited(at(t, 9001)));
    }

    #[test]
    fn a_replacement_catches_an_inhibitor_taken_while_idle() {
        // KWin: both notifications idle, then an inhibitor appears and nothing is resumed.
        let t = Instant::now();
        let mut v = Verdict::default();
        v.set(Kind::InputIdle, true, at(t, 0));
        v.set(Kind::Idle, true, at(t, 0));
        assert!(!v.rearm_due(at(t, 19_999)));
        assert!(v.rearm_due(at(t, 20_000)));
        v.rearmed(at(t, 20_000));
        assert!(
            !v.inhibited(at(t, 22_000)),
            "the old verdict stands while the fresh notification has not had time to answer"
        );
        // Nothing fires: the inhibitor holds the fresh one off.
        assert!(v.inhibited(at(t, 27_000)));
        assert!(!v.rearm_due(at(t, 39_999)));
    }

    #[test]
    fn a_replacement_catches_an_inhibitor_released_while_idle() {
        let t = Instant::now();
        let mut v = Verdict::default();
        v.set(Kind::InputIdle, true, at(t, 0));
        assert!(v.inhibited(at(t, 2_000)));
        v.rearmed(at(t, 20_000));
        assert!(
            v.inhibited(at(t, 22_000)),
            "no dip while the replacement settles"
        );
        v.set(Kind::Idle, true, at(t, 25_000));
        assert!(!v.inhibited(at(t, 27_000)));
    }

    #[test]
    fn input_cancels_a_held_verdict() {
        let t = Instant::now();
        let mut v = Verdict::default();
        v.set(Kind::InputIdle, true, at(t, 0));
        v.rearmed(at(t, 20_000));
        v.set(Kind::InputIdle, false, at(t, 21_000));
        assert!(!v.inhibited(at(t, 21_001)));
        assert!(
            !v.rearm_due(at(t, 60_000)),
            "nothing to re-arm while the user is active"
        );
    }

    #[test]
    fn an_inhibitor_taken_while_idle_settles_afresh() {
        let t = Instant::now();
        let mut v = Verdict::default();
        v.set(Kind::InputIdle, true, at(t, 0));
        v.set(Kind::Idle, true, at(t, 0));
        // The compositor resumes the inhibitor-respecting notification when an inhibitor appears.
        v.set(Kind::Idle, false, at(t, 20_000));
        assert!(!v.inhibited(at(t, 20_500)));
        assert!(v.inhibited(at(t, 21_000)));
    }
}
