// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Forward the guest's virtio-snd stream lifetime to the supervisor.
//!
//! The supervisor announces the VM to macOS as a media player for as long as the guest holds
//! its audio device open, so that the media keys, the Control Center transport and the headset
//! gestures reach the guest through macOS's own arbitration rather than through a focus rule
//! (`docs/design/media-keys-now-playing.md`). Everything it needs to know is one bit — is the
//! guest's PCM stream running — and libkrun's snd device already has it exactly.
//!
//! This is the pipe between the two, and nothing else: no filtering by stream, no debouncing,
//! no idea of what "playing" means. Those are the supervisor's, because they are policy.

use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use devices::virtio::{PcmEvent, PcmStateFn, PcmStreamState};

/// Build the callback libkrun's snd device calls on every PCM transition, writing one line per
/// transition to the supervisor's control socketpair.
///
/// `control_fd` is borrowed, not consumed: the display backend writes to the same fd, so this
/// dups it and owns the copy for the life of the VM.
pub fn control_writer(control_fd: i32) -> Option<PcmStateFn> {
    if control_fd < 0 {
        return None;
    }
    let dup = unsafe { libc::dup(control_fd) };
    if dup < 0 {
        log::error!("snd: dup(control_fd) failed; the VM will not announce itself as a player");
        return None;
    }
    let file = Mutex::new(std::fs::File::from(unsafe { OwnedFd::from_raw_fd(dup) }));
    Some(Arc::new(move |state: PcmStreamState| {
        let event = match state.event {
            PcmEvent::Prepare => "prepare",
            PcmEvent::Start => "start",
            PcmEvent::Stop => "stop",
            PcmEvent::Release => "release",
        };
        // One `write` for the whole line, newline included. The control socketpair has two
        // writers — the display backend on the GPU thread and this on the device thread — and
        // only a single small write is atomic against the other; a line assembled in fragments
        // can interleave into an unparseable one.
        let line = format!("audio {} {event}\n", state.stream_id);
        let Ok(mut f) = file.lock() else { return };
        if let Err(e) = f.write_all(line.as_bytes()) {
            log::error!("snd: control write failed: {e}");
        }
    }))
}
