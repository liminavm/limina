// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Forward the guest's virtio-snd stream lifetime to the supervisor.
//!
//! The supervisor announces the VM to macOS as a media player for as long as the guest holds
//! its audio device open, so that the media keys, the Control Center transport and the headset
//! gestures reach the guest through macOS's own arbitration rather than through a focus rule
//! (`docs/design/media-keys-now-playing.md`). Two facts reach it from here: whether the guest's
//! PCM stream is running, and whether that stream is carrying sound or bit-exact silence. The
//! second is what makes a pause visible in half a second instead of the five the stream lifetime
//! takes, and macOS routes a discrete `pause` at us the moment a headset comes off.
//!
//! This is the pipe between the two, and nothing else: no filtering by stream, no debouncing,
//! no idea of what "playing" means. Those are the supervisor's, because they are policy.

use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use devices::virtio::{PcmAudibility, PcmAudibilityFn, PcmEvent, PcmStateFn, PcmStreamState};

/// How long the guest's playback buffers must be bit-exact silent before we call it a pause.
///
/// Measured on a Fedora 44 guest, 2026-09-01: pausing a video leaves ~3 s of zero-filled buffers
/// before the buffers stop and ~5 s before the PCM stream reports `stop`, so anything under three
/// seconds is a win; and in 242 s of real playback the longest run of bit-exact silence content
/// itself produced was 340 ms, at a track change. 500 ms clears that with margin and still beats
/// a hand reaching for a headset. A run that is wrong costs one swallowed `pause`, which the
/// stream lifetime then corrects.
pub const PAUSE_SILENCE: std::time::Duration = std::time::Duration::from_millis(500);

/// Build the callback libkrun's snd device calls on every PCM transition, writing one line per
/// transition to the supervisor's control socketpair.
pub fn control_writer(control_fd: i32) -> Option<PcmStateFn> {
    let file = writer(control_fd, "announce itself as a player")?;
    Some(Arc::new(move |state: PcmStreamState| {
        let event = match state.event {
            PcmEvent::Prepare => "prepare",
            PcmEvent::Start => "start",
            PcmEvent::Stop => "stop",
            PcmEvent::Release => "release",
        };
        write_line(&file, format!("audio {} {event}\n", state.stream_id));
    }))
}

/// Build the callback the snd device calls when the playback stream crosses between sound and
/// silence, writing the same one line per edge to the same socketpair.
pub fn audibility_writer(control_fd: i32) -> Option<PcmAudibilityFn> {
    let file = writer(control_fd, "notice a pause before the stream ends")?;
    Some(Arc::new(
        move |stream_id: u32, audibility: PcmAudibility| {
            let word = match audibility {
                PcmAudibility::Audible => "audible",
                PcmAudibility::Silent => "silent",
            };
            write_line(&file, format!("audio {stream_id} {word}\n"));
        },
    ))
}

/// Dup `control_fd` and wrap it. Borrowed, not consumed: the display backend writes to the same
/// fd, so every writer here owns its own copy for the life of the VM.
fn writer(control_fd: i32, lost: &str) -> Option<Mutex<std::fs::File>> {
    if control_fd < 0 {
        return None;
    }
    let dup = unsafe { libc::dup(control_fd) };
    if dup < 0 {
        log::error!("snd: dup(control_fd) failed; the VM will not {lost}");
        return None;
    }
    Some(Mutex::new(std::fs::File::from(unsafe {
        OwnedFd::from_raw_fd(dup)
    })))
}

/// One `write` for the whole line, newline included. The control socketpair has several writers
/// — the display backend on the GPU thread and these on the device thread — and only a single
/// small write is atomic against the others; a line assembled in fragments can interleave into
/// an unparseable one.
fn write_line(file: &Mutex<std::fs::File>, line: String) {
    let Ok(mut f) = file.lock() else { return };
    if let Err(e) = f.write_all(line.as_bytes()) {
        log::error!("snd: control write failed: {e}");
    }
}
