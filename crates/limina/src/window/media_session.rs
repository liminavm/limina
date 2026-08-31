// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The MediaPlayer half of the VM's media session: performs what [`super::media_policy`] decides.
//!
//! This file holds no decisions. It wires and unwires `MPRemoteCommandCenter` handlers, publishes
//! and clears an `MPNowPlayingInfoCenter` dict, and hands the remote commands macOS routes back at
//! us to the policy — which says whether each one becomes an evdev key for the guest's own desktop
//! to act on. When and whether any of that happens is the policy's business.
//!
//! Everything here runs on the **main thread**: MediaPlayer wants it, and it is also where the
//! command handlers then fire, so delivering a key from one takes the same path as a key from an
//! `NSEvent` and needs no extra synchronisation.

use std::cell::RefCell;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSString};
use objc2_media_player::{
    MPNowPlayingInfoCenter, MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter,
    MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};

use super::media_policy::Command;

/// Hands one routed command to the policy, which decides whether it reaches the guest as a key.
/// `Rc` because it is main-thread-only, like everything else here.
type CommandSink = std::rc::Rc<dyn Fn(Command)>;

thread_local! {
    /// Where a remote command goes. Set once by the window; the command blocks read it, so they
    /// hold no borrow of the window and can outlive an individual VM.
    static COMMAND_SINK: RefCell<Option<CommandSink>> = const { RefCell::new(None) };
}

/// Register the main-thread sink that takes a routed command to the policy and on to the guest.
pub(crate) fn register_command_sink(sink: CommandSink) {
    COMMAND_SINK.with(|s| *s.borrow_mut() = Some(sink));
}

fn deliver(cmd: Command) {
    let sink = COMMAND_SINK.with(|s| s.borrow().clone());
    match sink {
        Some(f) => {
            log::info!("media: macOS routed {cmd:?} to us");
            f(cmd)
        }
        // The window is gone but macOS still thinks we are a player. Not fatal, but it means a
        // retire was missed somewhere, so say so rather than dropping the command in silence.
        None => log::warn!("media: remote command with no sink; it was dropped"),
    }
}

/// The VM's registration with macOS's media session.
///
/// Holds the opaque target objects `addTargetWithHandler` hands back, because removing the
/// handlers is what actually releases the media keys — clearing the info dict only drops the
/// Control Center tile, and an app with live handlers stays macOS's fallback target on a Mac with
/// no other player (see `spikes/now-playing-media-keys/RESULTS.md`).
pub(crate) struct MediaSession {
    /// One per wired command, in the order they were wired. Empty when we are not a player.
    targets: Vec<(Retained<MPRemoteCommand>, Retained<AnyObject>)>,
    /// The playback state last published, so a redundant publish is skipped.
    published_playing: bool,
    /// What the Now Playing tile calls this VM.
    title: String,
}

impl MediaSession {
    pub(crate) fn new(title: impl Into<String>) -> Self {
        MediaSession {
            targets: Vec::new(),
            published_playing: false,
            title: title.into(),
        }
    }

    /// Take our turn: wire the handlers and publish as playing.
    pub(crate) fn announce(&mut self) {
        if !self.targets.is_empty() {
            // Announcing twice would re-claim the front of macOS's ranking, taking the keys off
            // whatever the user is listening to. The policy already guarantees this; belt too.
            return;
        }
        unsafe {
            let center = MPRemoteCommandCenter::sharedCommandCenter();
            // The physical key arrives as togglePlayPause, but Control Center's own buttons —
            // and macOS's automations, a headset coming off among them — send the discrete
            // play/pause and nextTrack, so all of them are load-bearing, not defensive. They
            // stay *distinct* all the way to the policy: the guest understands only a toggle,
            // and which toggle is worth sending depends on which command asked.
            self.wire(&center.togglePlayPauseCommand(), Command::Toggle);
            self.wire(&center.playCommand(), Command::Play);
            self.wire(&center.pauseCommand(), Command::Pause);
            self.wire(&center.nextTrackCommand(), Command::Next);
            self.wire(&center.previousTrackCommand(), Command::Previous);

            // Everything we have no key for is explicitly disabled. That is the documented way to
            // say "this player cannot do that"; left at their defaults they advertise a scrubber
            // and a rating control that nothing can service.
            disable(&center.stopCommand());
            disable(&center.seekForwardCommand());
            disable(&center.seekBackwardCommand());
            disable(&center.changePlaybackPositionCommand());
            disable(&center.skipForwardCommand());
            disable(&center.skipBackwardCommand());
            disable(&center.changeShuffleModeCommand());
            disable(&center.changeRepeatModeCommand());
            disable(&center.likeCommand());
            disable(&center.dislikeCommand());
            disable(&center.bookmarkCommand());
            disable(&center.ratingCommand());
            disable(&center.changePlaybackRateCommand());

            let info = MPNowPlayingInfoCenter::defaultCenter();
            // Title only. Without a guest agent limina knows nothing about what is playing, and
            // a title-only dict is enough to be routed to; duration, artwork and a scrubber are
            // the enhanced tier's, and inventing them here would render a lying widget.
            let key = objc2_media_player::MPMediaItemPropertyTitle;
            let title = NSString::from_str(&self.title);
            let dict = NSDictionary::from_slices::<NSString>(
                &[key],
                &[Retained::into_super(title).as_ref()],
            );
            info.setNowPlayingInfo(Some(&dict));
            info.setPlaybackState(MPNowPlayingPlaybackState::Playing);
        }
        self.published_playing = true;
        log::info!("media: announced the VM as a player ({})", self.title);
    }

    /// Step aside: remove the handlers, disable the commands, drop the tile.
    pub(crate) fn retire(&mut self) {
        if self.targets.is_empty() {
            return;
        }
        unsafe {
            for (cmd, target) in self.targets.drain(..) {
                cmd.removeTarget(Some(&target));
                cmd.setEnabled(false);
            }
            let info = MPNowPlayingInfoCenter::defaultCenter();
            info.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
            info.setNowPlayingInfo(None);
        }
        self.published_playing = false;
        log::info!("media: retired the VM's media session");
    }

    /// Publish what the policy believes the guest is doing.
    ///
    /// The widget's button *is* the published state: a tile left at `.playing` while the guest
    /// is paused offers a pause button, and sends `pause` again when it is pressed (spike arm 4).
    /// It is not a re-claim of the session either way — we only ever publish while already
    /// holding it, and the ranking only moves on an announce (spike §7).
    pub(crate) fn set_playing(&mut self, playing: bool) {
        if self.targets.is_empty() || playing == self.published_playing {
            return;
        }
        self.published_playing = playing;
        let state = if playing {
            MPNowPlayingPlaybackState::Playing
        } else {
            MPNowPlayingPlaybackState::Paused
        };
        unsafe { MPNowPlayingInfoCenter::defaultCenter().setPlaybackState(state) };
    }

    /// Wire one MediaPlayer command to the transport command it means.
    ///
    /// # Safety
    /// Caller is on the main thread and holds the MediaPlayer classes' usual expectations.
    unsafe fn wire(&mut self, cmd: &Retained<MPRemoteCommand>, which: Command) {
        let handler = RcBlock::new(move |_ev: NonNull<MPRemoteCommandEvent>| {
            deliver(which);
            // Success even when the policy swallows the command: a failure status invites macOS
            // to route it somewhere else, and the command did reach the right player — we simply
            // had nothing to do for it.
            MPRemoteCommandHandlerStatus::Success
        });
        unsafe {
            cmd.setEnabled(true);
            let target = cmd.addTargetWithHandler(&handler);
            self.targets.push((cmd.clone(), target));
        }
    }
}

impl Drop for MediaSession {
    /// A window closing while the guest was playing must not leave live handlers behind: they
    /// would go on eating the media keys with no VM to send them to.
    fn drop(&mut self) {
        self.retire();
    }
}

/// # Safety
/// Main thread.
unsafe fn disable<T: AsRef<MPRemoteCommand>>(cmd: &T) {
    unsafe { cmd.as_ref().setEnabled(false) };
}
