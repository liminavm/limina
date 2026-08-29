// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! When the VM should hold macOS's media session, with no MediaPlayer in it.
//!
//! limina announces the VM to macOS as a player while the guest holds its audio device open, so
//! the transport keys, the Control Center buttons and the headset gestures reach the guest through
//! macOS's own arbitration instead of a focus rule. The design and the measurements behind it are
//! in `docs/design/media-keys-now-playing.md` and `spikes/now-playing-media-keys/RESULTS.md`; two
//! of those measurements are what this file exists to encode, because both are easy to get wrong
//! in a way nothing would ever report.
//!
//!   - **Retiring means unwiring the handlers, not just clearing the tile.** An app that clears its
//!     info dict but leaves its command handlers registered stays macOS's fallback target on a Mac
//!     with no other player, and goes on receiving the media keys forever. A limina that did that
//!     would swallow three keys and forward them to a guest that is playing nothing.
//!   - **Announcing re-claims the front of the ranking**, so it must happen on a transition and
//!     never on a timer, a refresh, or a track gap. Re-announcing while the guest merely re-opens
//!     its stream between tracks would take the keys back from a host player the user has since
//!     switched to.
//!
//! The [`Action`]s are what the macOS side performs; the decisions are here, driven by values, so
//! the whole state machine is exercised without a VM or a Mac.

use std::time::{Duration, Instant};

/// virtio-snd playback is stream 0 in libkrun's device; stream 1 is mic capture, which says
/// nothing about the VM being a player.
const PLAYBACK_STREAM: u32 = 0;

/// How long a stopped stream is still treated as playing.
///
/// PipeWire keeps its sink node open across a pause and only suspends it after an idle timeout,
/// and a track change stops and restarts the stream. Retiring on the first `stop` would hand the
/// session away mid-album and leave the user's next press going somewhere else — and because
/// re-announcing is a re-claim, coming back would not be free.
pub(crate) const RETIRE_HOLD: Duration = Duration::from_secs(5);

/// A PCM lifecycle transition, as libkrun's snd device reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PcmEvent {
    Prepare,
    Start,
    Stop,
    Release,
}

impl PcmEvent {
    /// Parse the worker's wire word. Unknown words are ignored rather than guessed at.
    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word {
            "prepare" => Some(PcmEvent::Prepare),
            "start" => Some(PcmEvent::Start),
            "stop" => Some(PcmEvent::Stop),
            "release" => Some(PcmEvent::Release),
            _ => None,
        }
    }
}

/// What the macOS side should do. Deliberately coarse: `Announce` is "wire the command handlers
/// and publish the info dict as playing", `Retire` is "remove the targets, disable the commands
/// and clear the dict". They are coarse because the measurements only ever moved the two halves
/// together, so which of them carries the session is not something this code should imply it
/// knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Announce,
    Retire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not registered. The media keys are macOS's business entirely.
    Absent,
    /// Registered and published as playing.
    Holding,
    /// Registered and published, but the guest's stream has gone; retiring at this instant
    /// unless the stream comes back first.
    Lapsing { deadline: Instant },
}

/// The VM's side of macOS's media-session arbitration.
#[derive(Debug)]
pub(crate) struct MediaPolicy {
    state: State,
    hold: Duration,
}

impl MediaPolicy {
    pub(crate) fn new() -> Self {
        Self::with_hold(RETIRE_HOLD)
    }

    pub(crate) fn with_hold(hold: Duration) -> Self {
        MediaPolicy {
            state: State::Absent,
            hold,
        }
    }

    /// Whether the VM currently holds (or is still holding) the session.
    pub(crate) fn announced(&self) -> bool {
        !matches!(self.state, State::Absent)
    }

    /// A PCM transition from the guest.
    pub(crate) fn stream_event(
        &mut self,
        stream_id: u32,
        event: PcmEvent,
        now: Instant,
    ) -> Option<Action> {
        // Mic capture says nothing about the VM being a player: a guest recording audio is not
        // one the transport keys should reach.
        if stream_id != PLAYBACK_STREAM {
            return None;
        }
        match (self.state, event) {
            // The guest started playing and we were not a player: take our turn.
            (State::Absent, PcmEvent::Start) => {
                self.state = State::Holding;
                Some(Action::Announce)
            }
            // Already holding. Nothing to do, and emphatically no re-announce: that would
            // re-claim the ranking from whatever the user is actually listening to.
            (State::Holding, PcmEvent::Start | PcmEvent::Prepare) => None,
            // The stream is coming back inside the hold — a track change, a PipeWire
            // re-prepare. We never let go, so there is nothing to take back.
            (State::Lapsing { .. }, PcmEvent::Start | PcmEvent::Prepare) => {
                self.state = State::Holding;
                None
            }
            // The guest let go. Start the clock rather than retiring now.
            (State::Holding, PcmEvent::Stop | PcmEvent::Release) => {
                self.state = State::Lapsing {
                    deadline: now + self.hold,
                };
                None
            }
            // A second stop while already lapsing does not extend the reprieve: the guest has
            // only become quieter, and the clock is measuring the same silence.
            (State::Lapsing { .. }, PcmEvent::Stop | PcmEvent::Release) => None,
            // Not a player, and a stop or a bare prepare does not make us one. Only `start`
            // does — a stream that is merely open is not one that is playing.
            (State::Absent, _) => None,
        }
    }

    /// Time passing. Call it from whatever already ticks; it only acts at the deadline.
    pub(crate) fn tick(&mut self, now: Instant) -> Option<Action> {
        match self.state {
            State::Lapsing { deadline } if now >= deadline => {
                self.state = State::Absent;
                Some(Action::Retire)
            }
            _ => None,
        }
    }

    /// The worker is gone (exit, crash, a suspend). Whatever the guest was playing, it is not
    /// playing it now, and there is nothing left to send a key to — so step aside at once
    /// rather than serving the hold out. Idempotent.
    pub(crate) fn worker_gone(&mut self) -> Option<Action> {
        if self.announced() {
            self.state = State::Absent;
            Some(Action::Retire)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_started_playback_stream_announces_once() {
        let mut p = MediaPolicy::new();
        let now = t0();
        assert_eq!(
            p.stream_event(0, PcmEvent::Start, now),
            Some(Action::Announce)
        );
        // Re-announcing would re-claim the ranking from whoever the user is listening to now.
        assert_eq!(p.stream_event(0, PcmEvent::Start, now), None);
        assert_eq!(p.stream_event(0, PcmEvent::Prepare, now), None);
    }

    #[test]
    fn an_open_but_unstarted_stream_is_not_a_player() {
        let mut p = MediaPolicy::new();
        // PipeWire prepares the sink long before anything plays through it, and a guest that
        // merely holds the device open has nothing for a transport key to act on.
        assert_eq!(p.stream_event(0, PcmEvent::Prepare, t0()), None);
        assert!(!p.announced());
    }

    #[test]
    fn mic_capture_is_not_playback() {
        let mut p = MediaPolicy::new();
        assert_eq!(p.stream_event(1, PcmEvent::Start, t0()), None);
        assert!(!p.announced());
    }

    #[test]
    fn a_stop_retires_only_after_the_hold() {
        let hold = Duration::from_secs(5);
        let mut p = MediaPolicy::with_hold(hold);
        let now = t0();
        p.stream_event(0, PcmEvent::Start, now);
        assert_eq!(p.stream_event(0, PcmEvent::Stop, now), None);
        assert_eq!(
            p.tick(now + Duration::from_secs(4)),
            None,
            "still within the hold"
        );
        assert_eq!(p.tick(now + hold), Some(Action::Retire));
        assert!(!p.announced());
        // And only once — a retired policy has nothing left to give back.
        assert_eq!(p.tick(now + Duration::from_secs(60)), None);
    }

    #[test]
    fn a_track_gap_keeps_the_session_without_reclaiming_it() {
        let mut p = MediaPolicy::with_hold(Duration::from_secs(5));
        let now = t0();
        p.stream_event(0, PcmEvent::Start, now);
        p.stream_event(0, PcmEvent::Stop, now);
        // The next track starts inside the hold: no Announce, because we never let go, and an
        // Announce here would jump the ranking on every track change.
        assert_eq!(
            p.stream_event(0, PcmEvent::Start, now + Duration::from_secs(1)),
            None
        );
        assert_eq!(
            p.tick(now + Duration::from_secs(600)),
            None,
            "the clock was cancelled"
        );
        assert!(p.announced());
    }

    #[test]
    fn a_second_stop_does_not_extend_the_reprieve() {
        let hold = Duration::from_secs(5);
        let mut p = MediaPolicy::with_hold(hold);
        let now = t0();
        p.stream_event(0, PcmEvent::Start, now);
        p.stream_event(0, PcmEvent::Stop, now);
        // PipeWire stops and then releases; that is one silence, not two.
        assert_eq!(
            p.stream_event(0, PcmEvent::Release, now + Duration::from_secs(3)),
            None
        );
        assert_eq!(p.tick(now + hold), Some(Action::Retire));
    }

    #[test]
    fn playing_again_after_a_full_retire_announces_afresh() {
        let hold = Duration::from_secs(5);
        let mut p = MediaPolicy::with_hold(hold);
        let now = t0();
        p.stream_event(0, PcmEvent::Start, now);
        p.stream_event(0, PcmEvent::Stop, now);
        assert_eq!(p.tick(now + hold), Some(Action::Retire));
        let later = now + Duration::from_secs(3600);
        assert_eq!(
            p.stream_event(0, PcmEvent::Start, later),
            Some(Action::Announce)
        );
    }

    #[test]
    fn a_lost_worker_retires_immediately_and_idempotently() {
        let now = t0();
        let mut p = MediaPolicy::new();
        p.stream_event(0, PcmEvent::Start, now);
        assert_eq!(p.worker_gone(), Some(Action::Retire));
        assert_eq!(p.worker_gone(), None);
        // Including from the hold, where the guest is already gone but the clock is running.
        let mut p = MediaPolicy::new();
        p.stream_event(0, PcmEvent::Start, now);
        p.stream_event(0, PcmEvent::Stop, now);
        assert_eq!(p.worker_gone(), Some(Action::Retire));
    }

    #[test]
    fn wire_words_parse_and_unknown_ones_are_ignored() {
        assert_eq!(PcmEvent::parse("start"), Some(PcmEvent::Start));
        assert_eq!(PcmEvent::parse("release"), Some(PcmEvent::Release));
        assert_eq!(PcmEvent::parse("playing"), None);
    }
}
