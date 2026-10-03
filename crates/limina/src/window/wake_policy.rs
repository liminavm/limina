// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! When the VM should keep the host display awake, with no IOKit in it.
//!
//! macOS turns the display off after a stretch with no host input, which is exactly what watching
//! a video in the guest looks like from the host. A Mac player asks the system not to, and this is
//! limina doing the same on the guest's behalf. The guest's own word is the one to follow, but a
//! stock guest has no way to give it: GNOME keeps its idle inhibitors on the user's session bus,
//! out of reach of anything stock (`docs/hardening-backlog.md`, "A stock guest's idle inhibitors
//! never reach the host"). So there are two sources, and the better one wins whenever it exists:
//!
//!   - **The guest's inhibitors**, relayed by `limina-agent-session` on the enhanced tier. While
//!     any helper that can report them is connected, they alone decide.
//!   - **A heuristic** otherwise: the guest is playing sound *and* decoding video in hardware at
//!     the same time. Either alone is not enough. Sound alone is also music, which a Mac does not
//!     keep the display on for; decoding alone is also a muted clip looping on a web page.
//!
//! Either way the display is only held while one of the VM's windows is actually on screen: a
//! video playing in a VM the user has put away is not one they are watching.
//!
//! Letting go is not immediate. A release when the user has been hands-off for longer than the
//! display-sleep timeout turns the display off at once, so a buffering stall or the gap between
//! two videos must not release. [`WAKE_HOLD`] rides those out.

use std::time::{Duration, Instant};

use super::media_policy::{Audibility, AudioEvent, PcmEvent};

/// virtio-snd playback is stream 0; stream 1 is mic capture, which says nothing about playback.
const PLAYBACK_STREAM: u32 = 0;

/// How long the reason to stay awake must be gone before the display is let go.
///
/// Longer than a buffering stall, a seek, or the autoplay countdown between two videos, all of
/// which stop the decoder or the sound for a few seconds; short next to any display-sleep timeout
/// a Mac offers (a minute at the least), so a real stop still lets the display sleep on time.
pub(crate) const WAKE_HOLD: Duration = Duration::from_secs(10);

/// What the worker reports about the guest's hardware video decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoEvent {
    /// The decoder ran a frame after a quiet spell.
    Decoding,
    /// No frame for a second (`crates/limina-vmm/src/video_state.rs`).
    Still,
}

impl VideoEvent {
    /// Parse the worker's wire word. Unknown words are ignored rather than guessed at.
    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word {
            "decoding" => Some(VideoEvent::Decoding),
            "still" => Some(VideoEvent::Still),
            _ => None,
        }
    }
}

/// What the IOKit side should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Take the assertion that keeps the display awake.
    Hold,
    /// Release it.
    Release,
}

/// Why the display is wanted awake, for the log line that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    /// A guest application holds an idle inhibitor.
    GuestInhibitor,
    /// The guest plays sound while decoding video.
    Playback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Released,
    Held,
    /// Held, but the reason has gone; released at this instant unless it comes back first.
    Lapsing {
        deadline: Instant,
    },
}

/// The VM's claim on keeping the host display awake.
#[derive(Debug)]
pub(crate) struct WakePolicy {
    state: State,
    hold: Duration,
    /// The guest's playback stream is carrying sound.
    audible: bool,
    /// The guest's hardware decoder is running frames.
    decoding: bool,
    /// One of the VM's windows is on screen.
    visible: bool,
    /// The guest's idle inhibitors, when a helper that reports them is connected.
    inhibited: Option<bool>,
}

impl WakePolicy {
    pub(crate) fn new() -> Self {
        Self::with_hold(WAKE_HOLD)
    }

    pub(crate) fn with_hold(hold: Duration) -> Self {
        WakePolicy {
            state: State::Released,
            hold,
            audible: false,
            decoding: false,
            visible: false,
            inhibited: None,
        }
    }

    /// Anything the guest's audio device reported.
    pub(crate) fn audio(&mut self, stream_id: u32, event: AudioEvent) {
        if stream_id != PLAYBACK_STREAM {
            return;
        }
        // The same reading of the stream as the media session's belief: audibility when the
        // device reports it, the lifecycle as the coarse witness, and an opened-but-unstarted
        // stream as nothing at all.
        match event {
            AudioEvent::Audibility(Audibility::Audible)
            | AudioEvent::Lifecycle(PcmEvent::Start) => self.audible = true,
            AudioEvent::Audibility(Audibility::Silent)
            | AudioEvent::Lifecycle(PcmEvent::Stop | PcmEvent::Release) => self.audible = false,
            AudioEvent::Lifecycle(PcmEvent::Prepare) => {}
        }
    }

    /// Anything the worker reported about the guest's video decoder.
    pub(crate) fn video(&mut self, event: VideoEvent) {
        self.decoding = event == VideoEvent::Decoding;
    }

    /// Whether any of the VM's windows is on screen now.
    pub(crate) fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// The guest's idle inhibitors: `None` when no connected helper reports them, so the
    /// heuristic decides; otherwise whether any guest application holds one.
    pub(crate) fn set_inhibited(&mut self, inhibited: Option<bool>) {
        self.inhibited = inhibited;
    }

    /// Why the display is wanted awake right now, or `None` when it is not.
    pub(crate) fn reason(&self) -> Option<Reason> {
        if !self.visible {
            return None;
        }
        match self.inhibited {
            Some(true) => Some(Reason::GuestInhibitor),
            Some(false) => None,
            None => (self.audible && self.decoding).then_some(Reason::Playback),
        }
    }

    /// Whether the assertion is (still) held.
    pub(crate) fn held(&self) -> bool {
        self.state != State::Released
    }

    /// Time passing, and the inputs set since the last call taking effect. Call it from whatever
    /// already ticks.
    pub(crate) fn tick(&mut self, now: Instant) -> Option<Action> {
        let wanted = self.reason().is_some();
        match (self.state, wanted) {
            (State::Released, true) => {
                self.state = State::Held;
                Some(Action::Hold)
            }
            (State::Held, false) => {
                self.state = State::Lapsing {
                    deadline: now + self.hold,
                };
                None
            }
            (State::Lapsing { .. }, true) => {
                self.state = State::Held;
                None
            }
            (State::Lapsing { deadline }, false) if now >= deadline => {
                self.state = State::Released;
                Some(Action::Release)
            }
            _ => None,
        }
    }

    /// The worker is gone (exit, crash, a suspend). Nothing is playing, so let go at once rather
    /// than serving out the hold, and forget what the old guest was doing. Idempotent.
    pub(crate) fn worker_gone(&mut self) -> Option<Action> {
        self.audible = false;
        self.decoding = false;
        if self.held() {
            self.state = State::Released;
            Some(Action::Release)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLD: Duration = Duration::from_secs(10);

    fn visible_policy() -> WakePolicy {
        let mut p = WakePolicy::with_hold(HOLD);
        p.set_visible(true);
        p
    }

    fn playing(p: &mut WakePolicy) {
        p.audio(0, AudioEvent::Lifecycle(PcmEvent::Start));
        p.audio(0, AudioEvent::Audibility(Audibility::Audible));
        p.video(VideoEvent::Decoding);
    }

    #[test]
    fn video_with_sound_holds_the_display() {
        let mut p = visible_policy();
        let t0 = Instant::now();
        playing(&mut p);
        assert_eq!(p.tick(t0), Some(Action::Hold));
        assert_eq!(p.reason(), Some(Reason::Playback));
        assert_eq!(
            p.tick(t0 + Duration::from_secs(60)),
            None,
            "held, and said once"
        );
    }

    #[test]
    fn sound_alone_is_music_and_does_not_hold() {
        let mut p = visible_policy();
        p.audio(0, AudioEvent::Lifecycle(PcmEvent::Start));
        p.audio(0, AudioEvent::Audibility(Audibility::Audible));
        assert_eq!(p.tick(Instant::now()), None);
    }

    #[test]
    fn decoding_alone_is_a_muted_clip_and_does_not_hold() {
        let mut p = visible_policy();
        p.video(VideoEvent::Decoding);
        assert_eq!(p.tick(Instant::now()), None);
    }

    #[test]
    fn an_opened_stream_is_not_sound() {
        let mut p = visible_policy();
        p.audio(0, AudioEvent::Lifecycle(PcmEvent::Prepare));
        p.video(VideoEvent::Decoding);
        assert_eq!(p.tick(Instant::now()), None);
    }

    #[test]
    fn mic_capture_is_not_playback() {
        let mut p = visible_policy();
        p.audio(1, AudioEvent::Lifecycle(PcmEvent::Start));
        p.audio(1, AudioEvent::Audibility(Audibility::Audible));
        p.video(VideoEvent::Decoding);
        assert_eq!(p.tick(Instant::now()), None);
    }

    #[test]
    fn a_vm_put_away_does_not_hold_and_coming_back_does() {
        let mut p = WakePolicy::with_hold(HOLD);
        let t0 = Instant::now();
        playing(&mut p);
        assert_eq!(p.tick(t0), None, "no window on screen");
        p.set_visible(true);
        assert_eq!(p.tick(t0 + Duration::from_secs(1)), Some(Action::Hold));
    }

    #[test]
    fn a_pause_releases_only_after_the_hold() {
        let mut p = visible_policy();
        let t0 = Instant::now();
        playing(&mut p);
        p.tick(t0);
        p.audio(0, AudioEvent::Audibility(Audibility::Silent));
        p.video(VideoEvent::Still);
        assert_eq!(
            p.tick(t0 + Duration::from_secs(1)),
            None,
            "the clock starts"
        );
        assert!(p.held());
        assert_eq!(
            p.tick(t0 + Duration::from_secs(1) + HOLD - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            p.tick(t0 + Duration::from_secs(1) + HOLD),
            Some(Action::Release)
        );
        assert!(!p.held());
    }

    #[test]
    fn a_stall_inside_the_hold_never_lets_go() {
        let mut p = visible_policy();
        let t0 = Instant::now();
        playing(&mut p);
        p.tick(t0);
        p.video(VideoEvent::Still);
        assert_eq!(p.tick(t0 + Duration::from_secs(1)), None);
        p.video(VideoEvent::Decoding);
        assert_eq!(
            p.tick(t0 + Duration::from_secs(4)),
            None,
            "no re-hold, it was never released"
        );
        assert_eq!(
            p.tick(t0 + Duration::from_secs(30)),
            None,
            "and the old deadline is gone"
        );
        assert!(p.held());
    }

    #[test]
    fn putting_the_vm_away_lets_go_after_the_hold() {
        let mut p = visible_policy();
        let t0 = Instant::now();
        playing(&mut p);
        p.tick(t0);
        p.set_visible(false);
        assert_eq!(p.tick(t0 + Duration::from_secs(1)), None);
        assert_eq!(
            p.tick(t0 + Duration::from_secs(1) + HOLD),
            Some(Action::Release)
        );
    }

    #[test]
    fn the_guests_inhibitor_holds_without_sound_or_video() {
        let mut p = visible_policy();
        p.set_inhibited(Some(true));
        assert_eq!(p.tick(Instant::now()), Some(Action::Hold));
        assert_eq!(p.reason(), Some(Reason::GuestInhibitor));
    }

    #[test]
    fn a_reporting_helper_overrides_the_heuristic() {
        let mut p = visible_policy();
        p.set_inhibited(Some(false));
        playing(&mut p);
        assert_eq!(
            p.tick(Instant::now()),
            None,
            "the guest says nothing inhibits idle"
        );
    }

    #[test]
    fn the_heuristic_returns_when_the_helper_goes() {
        let mut p = visible_policy();
        let t0 = Instant::now();
        p.set_inhibited(Some(false));
        playing(&mut p);
        assert_eq!(p.tick(t0), None);
        p.set_inhibited(None);
        assert_eq!(p.tick(t0 + Duration::from_secs(1)), Some(Action::Hold));
        assert_eq!(p.reason(), Some(Reason::Playback));
    }

    #[test]
    fn the_guests_inhibitor_still_needs_a_window_on_screen() {
        let mut p = WakePolicy::with_hold(HOLD);
        p.set_inhibited(Some(true));
        assert_eq!(p.tick(Instant::now()), None);
    }

    #[test]
    fn a_gone_worker_releases_at_once_and_forgets_the_guest() {
        let mut p = visible_policy();
        let t0 = Instant::now();
        playing(&mut p);
        p.tick(t0);
        assert_eq!(p.worker_gone(), Some(Action::Release));
        assert_eq!(p.worker_gone(), None, "idempotent");
        assert_eq!(
            p.tick(t0 + Duration::from_secs(1)),
            None,
            "the old guest's playback is gone"
        );
    }

    #[test]
    fn video_words_parse_and_unknown_ones_do_not() {
        assert_eq!(VideoEvent::parse("decoding"), Some(VideoEvent::Decoding));
        assert_eq!(VideoEvent::parse("still"), Some(VideoEvent::Still));
        assert_eq!(VideoEvent::parse("paused"), None);
    }
}
