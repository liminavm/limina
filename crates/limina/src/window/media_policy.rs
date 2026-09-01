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

use limina_input::constants::{KEY_NEXTSONG, KEY_PLAYPAUSE, KEY_PREVIOUSSONG};

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

/// What the guest's playback stream is *carrying*, as opposed to whether it is open.
///
/// A guest desktop that pauses a video keeps its PCM stream running and goes on submitting
/// buffers of bit-exact digital silence for a few seconds before its audio server suspends the
/// node — so the lifecycle events arrive seconds after the pause the user made. Audibility is
/// the same fact, seconds earlier. Measured on a Fedora 44 guest 2026-09-01: ~3 s of zero-filled
/// buffers, then no buffers, then `Stop` about 5 s after the click. Real content, by contrast,
/// never went bit-exact silent for longer than 340 ms in 242 s of playback, and the quietest
/// passages measured (peak 0.0026) still contained no zero-filled buffer at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Audibility {
    Audible,
    Silent,
}

/// Everything the worker reports about one of the guest's audio streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioEvent {
    /// The guest moved the stream through its PCM lifecycle.
    Lifecycle(PcmEvent),
    /// What the stream is carrying changed.
    Audibility(Audibility),
}

impl AudioEvent {
    /// Parse the worker's wire word. Unknown words are ignored rather than guessed at.
    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word {
            "audible" => Some(AudioEvent::Audibility(Audibility::Audible)),
            "silent" => Some(AudioEvent::Audibility(Audibility::Silent)),
            other => PcmEvent::parse(other).map(AudioEvent::Lifecycle),
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

/// A transport command macOS routed back at us, before it becomes a key.
///
/// The physical key arrives as `Toggle`; Control Center's buttons, and macOS's own automations
/// (a headset coming off, a call starting), send the discrete `Play` and `Pause`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
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
    /// What we believe the guest is doing. The guest cannot tell us, so this is inferred from
    /// its audio stream and from the toggles we have sent it — see [`MediaPolicy::playing`].
    playing: bool,
}

impl MediaPolicy {
    pub(crate) fn new() -> Self {
        Self::with_hold(RETIRE_HOLD)
    }

    pub(crate) fn with_hold(hold: Duration) -> Self {
        MediaPolicy {
            state: State::Absent,
            hold,
            playing: false,
        }
    }

    /// Whether the VM currently holds (or is still holding) the session.
    pub(crate) fn announced(&self) -> bool {
        !matches!(self.state, State::Absent)
    }

    /// What we believe the guest's playback state is.
    ///
    /// The guest never reports it — no component of ours runs in it — so this is a belief, built
    /// from three things we do see: whether its buffers carry sound or bit-exact silence
    /// ([`Audibility`], the fast one), its stream starting and stopping, and the play/pause
    /// toggles we have sent it ourselves. It can still drift — a video muted in the guest is
    /// silent without being paused — which is why [`Command::Toggle`] is always delivered: the
    /// physical key remains the way out of a wrong belief.
    pub(crate) fn playing(&self) -> bool {
        self.playing
    }

    /// The evdev key one routed command should send the guest, or `None` to swallow it.
    ///
    /// The guest desktop understands only a *toggle*, so a discrete command has to be turned
    /// into one — and a toggle delivered to a guest that is already in the requested state does
    /// the opposite of what was asked. macOS sends a bare `pause` for things that are not a
    /// user pressing pause at all (headphones coming off, a call arriving), so an unconditional
    /// forward starts a paused video every time the user takes their headset off.
    pub(crate) fn remote_command(&mut self, cmd: Command) -> Option<u16> {
        match cmd {
            Command::Next => Some(KEY_NEXTSONG),
            Command::Previous => Some(KEY_PREVIOUSSONG),
            // The one unconditional arm: the physical key means "the other one", whatever we
            // believe, and it is what re-syncs a belief that has drifted.
            Command::Toggle => {
                self.playing = !self.playing;
                Some(KEY_PLAYPAUSE)
            }
            Command::Play if !self.playing => {
                self.playing = true;
                Some(KEY_PLAYPAUSE)
            }
            Command::Pause if self.playing => {
                self.playing = false;
                Some(KEY_PLAYPAUSE)
            }
            Command::Play | Command::Pause => None,
        }
    }

    /// A play/pause key reached the guest without passing through us — the hard grab forwards
    /// the media bucket straight to the guest (`limina_input::auxkey`). The guest flipped, so
    /// the belief has to flip too, or the next routed command is decided from a stale one.
    pub(crate) fn guest_toggled(&mut self) {
        self.playing = !self.playing;
    }

    /// Anything the guest's audio device reported.
    pub(crate) fn stream_event(
        &mut self,
        stream_id: u32,
        event: AudioEvent,
        now: Instant,
    ) -> Option<Action> {
        // Mic capture says nothing about the VM being a player: a guest recording audio is not
        // one the transport keys should reach.
        if stream_id != PLAYBACK_STREAM {
            return None;
        }
        let event = match event {
            AudioEvent::Lifecycle(event) => event,
            // Audibility corrects the belief and nothing else. It must never move the session:
            // holding it is about the guest owning the audio device, not about sound coming out
            // of it this instant, and a retire-and-reannounce over a quiet moment would hand the
            // keys to whatever else the Mac is playing.
            AudioEvent::Audibility(Audibility::Audible) => {
                self.playing = true;
                return None;
            }
            AudioEvent::Audibility(Audibility::Silent) => {
                self.playing = false;
                return None;
            }
        };
        // The lifecycle is the coarse witness, and the only one on a host with no audibility
        // reporting, so it still corrects the belief wherever it speaks — audibility refines it
        // within a buffer or two either way. `Prepare` deliberately says nothing: an opened
        // stream is not a playing one, and PipeWire opens the sink long before sound comes out.
        match event {
            PcmEvent::Start => self.playing = true,
            PcmEvent::Stop | PcmEvent::Release => self.playing = false,
            PcmEvent::Prepare => {}
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
            p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now),
            Some(Action::Announce)
        );
        // Re-announcing would re-claim the ranking from whoever the user is listening to now.
        assert_eq!(
            p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now),
            None
        );
        assert_eq!(
            p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Prepare), now),
            None
        );
    }

    #[test]
    fn an_open_but_unstarted_stream_is_not_a_player() {
        let mut p = MediaPolicy::new();
        // PipeWire prepares the sink long before anything plays through it, and a guest that
        // merely holds the device open has nothing for a transport key to act on.
        assert_eq!(
            p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Prepare), t0()),
            None
        );
        assert!(!p.announced());
    }

    #[test]
    fn mic_capture_is_not_playback() {
        let mut p = MediaPolicy::new();
        assert_eq!(
            p.stream_event(1, AudioEvent::Lifecycle(PcmEvent::Start), t0()),
            None
        );
        assert!(!p.announced());
    }

    #[test]
    fn a_stop_retires_only_after_the_hold() {
        let hold = Duration::from_secs(5);
        let mut p = MediaPolicy::with_hold(hold);
        let now = t0();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        assert_eq!(
            p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Stop), now),
            None
        );
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
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Stop), now);
        // The next track starts inside the hold: no Announce, because we never let go, and an
        // Announce here would jump the ranking on every track change.
        assert_eq!(
            p.stream_event(
                0,
                AudioEvent::Lifecycle(PcmEvent::Start),
                now + Duration::from_secs(1)
            ),
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
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Stop), now);
        // PipeWire stops and then releases; that is one silence, not two.
        assert_eq!(
            p.stream_event(
                0,
                AudioEvent::Lifecycle(PcmEvent::Release),
                now + Duration::from_secs(3)
            ),
            None
        );
        assert_eq!(p.tick(now + hold), Some(Action::Retire));
    }

    #[test]
    fn playing_again_after_a_full_retire_announces_afresh() {
        let hold = Duration::from_secs(5);
        let mut p = MediaPolicy::with_hold(hold);
        let now = t0();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Stop), now);
        assert_eq!(p.tick(now + hold), Some(Action::Retire));
        let later = now + Duration::from_secs(3600);
        assert_eq!(
            p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), later),
            Some(Action::Announce)
        );
    }

    #[test]
    fn a_lost_worker_retires_immediately_and_idempotently() {
        let now = t0();
        let mut p = MediaPolicy::new();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        assert_eq!(p.worker_gone(), Some(Action::Retire));
        assert_eq!(p.worker_gone(), None);
        // Including from the hold, where the guest is already gone but the clock is running.
        let mut p = MediaPolicy::new();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Stop), now);
        assert_eq!(p.worker_gone(), Some(Action::Retire));
    }

    /// The bug the gating exists for: taking a headset off makes macOS send a bare `pause`,
    /// and a `pause` forwarded as a toggle *starts* a paused video (dogfood 2026-08-31).
    #[test]
    fn a_pause_for_an_already_paused_guest_is_swallowed() {
        let mut p = MediaPolicy::new();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), t0());
        assert!(p.playing());
        // The user pauses from the widget: one toggle to the guest.
        assert_eq!(p.remote_command(Command::Pause), Some(KEY_PLAYPAUSE));
        // Headset off. macOS pauses us again; the guest is already paused, so nothing goes.
        assert_eq!(p.remote_command(Command::Pause), None);
        assert!(!p.playing());
        // And play still works from there.
        assert_eq!(p.remote_command(Command::Play), Some(KEY_PLAYPAUSE));
        assert!(p.playing());
        // A second play is likewise a no-op.
        assert_eq!(p.remote_command(Command::Play), None);
    }

    #[test]
    fn the_physical_key_always_toggles() {
        let mut p = MediaPolicy::new();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), t0());
        // Unconditional in both directions: it is the only way to fix a belief that drifted
        // (a guest paused by mouse click whose audio stream stayed open).
        assert_eq!(p.remote_command(Command::Toggle), Some(KEY_PLAYPAUSE));
        assert!(!p.playing());
        assert_eq!(p.remote_command(Command::Toggle), Some(KEY_PLAYPAUSE));
        assert!(p.playing());
        // A hard grab sends the same key without asking us; the belief follows it anyway.
        p.guest_toggled();
        assert!(!p.playing());
    }

    #[test]
    fn track_keys_are_never_gated() {
        let mut p = MediaPolicy::new();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), t0());
        p.remote_command(Command::Pause);
        // Skipping a track is meaningful whatever we believe about play state, and it has no
        // state of its own to get wrong.
        assert_eq!(p.remote_command(Command::Next), Some(KEY_NEXTSONG));
        assert_eq!(p.remote_command(Command::Previous), Some(KEY_PREVIOUSSONG));
        assert!(!p.playing(), "a track key says nothing about playing");
    }

    #[test]
    fn the_stream_corrects_the_belief() {
        let mut p = MediaPolicy::with_hold(Duration::from_secs(5));
        let now = t0();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        // The guest was paused in its own UI and its stream went with it: we learn from that,
        // so the pause macOS sends next is swallowed instead of restarting playback.
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Stop), now);
        assert!(!p.playing());
        assert_eq!(p.remote_command(Command::Pause), None);
        // The next track starts inside the hold: playing again, without a re-announce.
        assert_eq!(
            p.stream_event(
                0,
                AudioEvent::Lifecycle(PcmEvent::Start),
                now + Duration::from_secs(1)
            ),
            None
        );
        assert!(p.playing());
        // A mere prepare is not playback, so it must not resurrect the belief.
        p.stream_event(
            0,
            AudioEvent::Lifecycle(PcmEvent::Stop),
            now + Duration::from_secs(2),
        );
        p.stream_event(
            0,
            AudioEvent::Lifecycle(PcmEvent::Prepare),
            now + Duration::from_secs(2),
        );
        assert!(!p.playing());
    }

    #[test]
    fn mic_capture_does_not_move_the_belief() {
        let mut p = MediaPolicy::new();
        let now = t0();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        p.stream_event(1, AudioEvent::Lifecycle(PcmEvent::Stop), now);
        assert!(p.playing(), "the mic stream is not the playback stream");
    }

    #[test]
    fn wire_words_parse_and_unknown_ones_are_ignored() {
        assert_eq!(PcmEvent::parse("start"), Some(PcmEvent::Start));
        assert_eq!(PcmEvent::parse("release"), Some(PcmEvent::Release));
        assert_eq!(PcmEvent::parse("playing"), None);
    }

    #[test]
    fn silence_pauses_the_belief_before_the_stream_stops() {
        // The dogfood bug, 2026-09-01. The guest was playing, the user clicked pause, and took
        // the headset off within the second. No lifecycle event had arrived yet — PipeWire keeps
        // the node alive for ~5s — so we still believed the guest was playing, forwarded macOS's
        // `Pause` as the only key the guest understands, and the toggle started the video.
        // Audibility is the same news, ~500ms after the click instead of five seconds.
        let now = t0();
        let mut p = MediaPolicy::new();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        p.stream_event(0, AudioEvent::Audibility(Audibility::Audible), now);
        assert!(p.playing());

        p.stream_event(0, AudioEvent::Audibility(Audibility::Silent), now);
        assert!(!p.playing(), "silent buffers mean the guest paused");
        assert_eq!(p.remote_command(Command::Pause), None);
    }

    #[test]
    fn sound_coming_back_is_believed_at_once() {
        let now = t0();
        let mut p = MediaPolicy::new();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        p.stream_event(0, AudioEvent::Audibility(Audibility::Silent), now);
        assert!(!p.playing());
        p.stream_event(0, AudioEvent::Audibility(Audibility::Audible), now);
        assert!(p.playing());
        // And now a pause is worth sending.
        assert_eq!(p.remote_command(Command::Pause), Some(KEY_PLAYPAUSE));
    }

    #[test]
    fn audibility_never_moves_the_session() {
        // Holding the session is about the guest owning the device, not about whether sound is
        // coming out of it right now. Retiring on a quiet moment would hand the keys away
        // mid-album, and coming back is a re-claim.
        let now = t0();
        let mut p = MediaPolicy::new();
        assert_eq!(
            p.stream_event(0, AudioEvent::Audibility(Audibility::Audible), now),
            None,
            "audible alone must not announce"
        );
        assert!(!p.announced());
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        assert!(p.announced());
        assert_eq!(
            p.stream_event(0, AudioEvent::Audibility(Audibility::Silent), now),
            None,
            "silence must not retire"
        );
        assert!(p.announced());
    }

    #[test]
    fn the_mic_stream_audibility_is_ignored() {
        let now = t0();
        let mut p = MediaPolicy::new();
        p.stream_event(0, AudioEvent::Lifecycle(PcmEvent::Start), now);
        p.stream_event(1, AudioEvent::Audibility(Audibility::Silent), now);
        assert!(
            p.playing(),
            "the mic going quiet says nothing about playback"
        );
    }

    #[test]
    fn the_wire_words_parse() {
        assert_eq!(
            AudioEvent::parse("silent"),
            Some(AudioEvent::Audibility(Audibility::Silent))
        );
        assert_eq!(
            AudioEvent::parse("audible"),
            Some(AudioEvent::Audibility(Audibility::Audible))
        );
        assert_eq!(
            AudioEvent::parse("start"),
            Some(AudioEvent::Lifecycle(PcmEvent::Start))
        );
        assert_eq!(AudioEvent::parse("playing"), None);
    }
}
