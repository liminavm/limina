// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The MediaPlayer half of the VM's media session: performs what [`super::media_policy`] decides.
//!
//! This file holds no decisions. It wires and unwires `MPRemoteCommandCenter` handlers, publishes
//! and clears an `MPNowPlayingInfoCenter` dict, and turns the remote commands macOS routes back at
//! us into the evdev media keys the guest's own desktop already handles. When and whether any of
//! that happens is the policy's business.
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

/// evdev keys the guest's desktop already binds; no guest component is involved.
const KEY_NEXTSONG: u16 = 163;
const KEY_PLAYPAUSE: u16 = 164;
const KEY_PREVIOUSSONG: u16 = 165;

/// Delivers one evdev key to the guest. `Rc` because it is main-thread-only, like everything
/// else here.
type KeySink = std::rc::Rc<dyn Fn(u16)>;

thread_local! {
    /// Where a remote command's key goes. Set once by the window; the command blocks read it,
    /// so they hold no borrow of the window and can outlive an individual VM.
    static KEY_SINK: RefCell<Option<KeySink>> = const { RefCell::new(None) };
}

/// Register the main-thread sink that delivers a media key to the guest.
pub(crate) fn register_key_sink(sink: KeySink) {
    KEY_SINK.with(|s| *s.borrow_mut() = Some(sink));
}

fn deliver(code: u16) {
    let sink = KEY_SINK.with(|s| s.borrow().clone());
    match sink {
        Some(f) => f(code),
        // The window is gone but macOS still thinks we are a player. Not fatal, but it means a
        // retire was missed somewhere, so say so rather than dropping the key in silence.
        None => log::warn!("media: remote command with no key sink; the key was dropped"),
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
    /// What the Now Playing tile calls this VM.
    title: String,
}

impl MediaSession {
    pub(crate) fn new(title: impl Into<String>) -> Self {
        MediaSession {
            targets: Vec::new(),
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
            // The physical key arrives as togglePlayPause, but Control Center's own buttons send
            // the discrete play/pause and nextTrack — so all of them are load-bearing, not
            // defensive. The guest understands only a toggle, so play and pause collapse onto it.
            self.wire(&center.togglePlayPauseCommand(), KEY_PLAYPAUSE);
            self.wire(&center.playCommand(), KEY_PLAYPAUSE);
            self.wire(&center.pauseCommand(), KEY_PLAYPAUSE);
            self.wire(&center.nextTrackCommand(), KEY_NEXTSONG);
            self.wire(&center.previousTrackCommand(), KEY_PREVIOUSSONG);

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
        log::info!("media: retired the VM's media session");
    }

    /// Wire one command to one evdev key.
    ///
    /// # Safety
    /// Caller is on the main thread and holds the MediaPlayer classes' usual expectations.
    unsafe fn wire(&mut self, cmd: &Retained<MPRemoteCommand>, code: u16) {
        let handler = RcBlock::new(move |_ev: NonNull<MPRemoteCommandEvent>| {
            deliver(code);
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
