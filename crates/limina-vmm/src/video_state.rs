// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Forward whether the guest is decoding video in hardware to the supervisor.
//!
//! The supervisor keeps the host display awake while a guest plays a video, and on a guest with
//! nothing of ours installed the best evidence it has is the guest playing audio *and* decoding
//! video at the same time (`crates/limina/src/window/wake_policy.rs`). The audio half comes from
//! [`crate::audio_state`]; this is the video half. virglrs reports every unit its host decoder
//! runs ([`virglrenderer::vrend::video::DecodeObserver`]), and this turns that per-frame stream
//! into two edges on the control socketpair: `video decoding` on the first frame after a quiet
//! spell, `video still` once no frame has arrived for [`STILL_AFTER`].
//!
//! Like `audio_state`, this is a pipe and not a policy: how long a pause must last before the
//! display may sleep is the supervisor's decision. What is decided here is only the line rate --
//! one line per edge, never one per frame.

use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use virglrenderer::vrend::video::{DecodeObserver, set_decode_observer};

/// How long without a decoded frame before the guest counts as no longer decoding.
///
/// Comfortably above any frame interval a player produces (a 24 fps film is 42 ms per frame, and
/// a decoder fed in bursts still runs ahead of display by a few frames at most), and short enough
/// that a pause reaches the supervisor within a second. A buffering stall longer than this reads
/// as `still`; the supervisor's own hold is what keeps that from letting the display sleep.
pub const STILL_AFTER: Duration = Duration::from_secs(1);

/// An edge to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    Decoding,
    Still,
}

impl Edge {
    fn line(self) -> &'static str {
        match self {
            Edge::Decoding => "video decoding\n",
            Edge::Still => "video still\n",
        }
    }
}

/// Which side of [`STILL_AFTER`] the guest is on, and when it last decoded.
#[derive(Debug)]
struct Tracker {
    decoding: bool,
    last: Option<Instant>,
}

impl Tracker {
    fn new() -> Self {
        Tracker {
            decoding: false,
            last: None,
        }
    }

    /// A unit was decoded at `now`.
    fn frame(&mut self, now: Instant) -> Option<Edge> {
        self.last = Some(now);
        (!std::mem::replace(&mut self.decoding, true)).then_some(Edge::Decoding)
    }

    /// Time passed. Reports `still` once the last frame is [`STILL_AFTER`] old.
    fn check(&mut self, now: Instant) -> Option<Edge> {
        let quiet = self
            .last
            .is_none_or(|last| now.duration_since(last) >= STILL_AFTER);
        (self.decoding && quiet).then(|| {
            self.decoding = false;
            Edge::Still
        })
    }

    /// When [`Self::check`] could next report, or `None` while there is nothing to time.
    fn due(&self) -> Option<Instant> {
        self.decoding.then(|| {
            self.last
                .map_or_else(Instant::now, |last| last + STILL_AFTER)
        })
    }
}

/// The observer virglrs calls on each codec's decode thread.
struct Reporter {
    tracker: Mutex<Tracker>,
    /// Wakes the timer thread when decoding starts; it sleeps without a deadline otherwise, so an
    /// idle guest costs no wakeups.
    started: Condvar,
    file: Mutex<std::fs::File>,
}

impl Reporter {
    fn send(&self, edge: Edge) {
        // One write for the whole line: the display backend and the audio callbacks write to the
        // same socketpair from other threads, and only a single small write is atomic against them.
        let Ok(mut f) = self.file.lock() else { return };
        if let Err(e) = f.write_all(edge.line().as_bytes()) {
            log::error!("video: control write failed: {e}");
        }
    }

    /// The timer thread: reports `still` when the frames stop.
    fn watch(self: Arc<Self>) {
        let mut tracker = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match tracker.due() {
                None => {
                    tracker = self
                        .started
                        .wait(tracker)
                        .unwrap_or_else(|e| e.into_inner());
                }
                Some(due) => {
                    let wait = due.saturating_duration_since(Instant::now());
                    tracker = self
                        .started
                        .wait_timeout(tracker, wait)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                    if let Some(edge) = tracker.check(Instant::now()) {
                        self.send(edge);
                    }
                }
            }
        }
    }
}

impl DecodeObserver for Reporter {
    fn decoded(&self) {
        let mut tracker = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(edge) = tracker.frame(Instant::now()) {
            self.send(edge);
            self.started.notify_one();
        }
    }
}

/// Report decode activity on `control_fd` for the life of the VM. Borrowed, not consumed: the
/// display backend writes to the same fd, so this owns its own copy. Install before the renderer
/// starts, since a codec keeps the observer it was created under.
pub fn install(control_fd: i32) {
    if control_fd < 0 {
        return;
    }
    // SAFETY: dup of a descriptor this process holds open for the life of the VM.
    let dup = unsafe { libc::dup(control_fd) };
    if dup < 0 {
        log::error!("video: dup(control_fd) failed; the host display may sleep during playback");
        return;
    }
    let reporter = Arc::new(Reporter {
        tracker: Mutex::new(Tracker::new()),
        started: Condvar::new(),
        // SAFETY: `dup` is a fresh descriptor nothing else owns.
        file: Mutex::new(std::fs::File::from(unsafe { OwnedFd::from_raw_fd(dup) })),
    });
    let watcher = Arc::clone(&reporter);
    if let Err(e) = std::thread::Builder::new()
        .name("limina-video-state".into())
        .spawn(move || watcher.watch())
    {
        log::error!("video: could not start the decode timer ({e}); not reporting decode activity");
        return;
    }
    set_decode_observer(Some(reporter));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn the_first_frame_reports_decoding_and_the_rest_say_nothing() {
        let mut t = Tracker::new();
        let start = t0();
        assert_eq!(t.frame(start), Some(Edge::Decoding));
        assert_eq!(t.frame(start + Duration::from_millis(40)), None);
        assert_eq!(t.frame(start + Duration::from_millis(80)), None);
    }

    #[test]
    fn frames_closer_than_the_threshold_never_report_still() {
        let mut t = Tracker::new();
        let start = t0();
        t.frame(start);
        for i in 1..100 {
            let now = start + Duration::from_millis(40 * i);
            assert_eq!(t.check(now), None);
            t.frame(now);
        }
    }

    #[test]
    fn a_quiet_spell_reports_still_once_and_the_next_frame_decoding_again() {
        let mut t = Tracker::new();
        let start = t0();
        t.frame(start);
        assert_eq!(
            t.check(start + STILL_AFTER - Duration::from_millis(1)),
            None
        );
        assert_eq!(t.check(start + STILL_AFTER), Some(Edge::Still));
        assert_eq!(
            t.check(start + STILL_AFTER * 3),
            None,
            "still is reported once"
        );
        assert_eq!(t.frame(start + STILL_AFTER * 4), Some(Edge::Decoding));
    }

    #[test]
    fn nothing_is_timed_while_the_guest_is_not_decoding() {
        let mut t = Tracker::new();
        assert_eq!(t.due(), None, "an idle guest costs the timer no wakeups");
        let start = t0();
        t.frame(start);
        assert_eq!(t.due(), Some(start + STILL_AFTER));
        t.check(start + STILL_AFTER);
        assert_eq!(t.due(), None);
    }
}
