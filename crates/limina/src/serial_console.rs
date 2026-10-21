// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The guest's serial line (the PL011, `ttyAMA0`), held by the supervisor for the VM's whole
//! life: every boot's output is logged, and `limina console <vm>` watches it live.
//!
//! A quiet boot sends the firmware's and the loaders' text to the serial line only
//! (docs/design/boot-presentation.md §Seeing the boot), and so do the kernel and systemd when the
//! guest's console is there, rescue shells included. So the line is never dropped: each worker
//! gets one end of a socketpair as its PL011 ([`crate::supervisor::spawn_worker`]), and this
//! module reads the other.
//!
//! - **Recorded.** A managed VM writes each boot to `<bundle>/logs/console.log`, rotated like
//!   the other per-boot logs, so a VM that did not boot leaves its evidence behind.
//! - **Watched.** A client of the runtime socket's `console` request gets the recent output
//!   (the last [`BACKLOG_BYTES`], across reboots), then everything as it arrives. Clients
//!   outlive a reboot: the next worker's line feeds them.
//! - **Typed into, by permission.** What a client sends reaches the guest only while the VM's
//!   `console-input` lever is on (`debug_ctl::CONSOLE_INPUT`, off by default, read per byte
//!   batch): the line can be a root rescue shell, and any process of the same user can reach
//!   the runtime socket.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// How much recent output a newly attached client is shown.
pub const BACKLOG_BYTES: usize = 64 * 1024;

/// A client that cannot take output this fast is dropped rather than allowed to stall the line.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_millis(200);

/// The recent output, the attached clients, and the current worker's line for input.
struct Line {
    /// Whether this supervisor holds the line at all: not when the command line wired it
    /// itself (`--console`, `--console-pty`).
    held: bool,
    backlog: VecDeque<u8>,
    clients: Vec<UnixStream>,
    input: Option<UnixStream>,
}

static LINE: Mutex<Line> = Mutex::new(Line {
    held: false,
    backlog: VecDeque::new(),
    clients: Vec::new(),
    input: None,
});

/// Why a `console` request cannot be served, if it cannot.
pub fn refusal() -> Option<&'static str> {
    (!line().held).then_some(
        "this VM's serial line goes where its command line said (--console or --console-pty)",
    )
}

fn line() -> std::sync::MutexGuard<'static, Line> {
    LINE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Keep the last `cap` bytes of `backlog` after appending `bytes`.
fn remember(backlog: &mut VecDeque<u8>, bytes: &[u8], cap: usize) {
    backlog.extend(bytes);
    let excess = backlog.len().saturating_sub(cap);
    backlog.drain(..excess);
}

/// Serve one worker's line: `host` is the supervisor's end of its PL011. Returns at once; a
/// thread reads until the worker exits.
pub fn serve(host: OwnedFd, log: Option<PathBuf>) {
    let host = UnixStream::from(host);
    let input = host.try_clone();
    let mut l = line();
    l.held = true;
    match input {
        Ok(input) => l.input = Some(input),
        Err(e) => log::warn!("serial console: no input to the guest this boot: {e}"),
    }
    drop(l);
    let spawned = std::thread::Builder::new()
        .name("serial-console".into())
        .spawn(move || pump(host, log));
    if let Err(e) = spawned {
        log::warn!("serial console: could not start its reader: {e}");
    }
}

fn pump(mut host: UnixStream, log: Option<PathBuf>) {
    let mut file = log.and_then(|path| match File::create(&path) {
        Ok(f) => Some(f),
        Err(e) => {
            log::warn!("serial console: not logging to {}: {e}", path.display());
            None
        }
    });
    let mut buf = [0u8; 4096];
    loop {
        let n = match host.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let bytes = &buf[..n];
        if let Some(f) = file.as_mut()
            && f.write_all(bytes).is_err()
        {
            file = None;
        }
        let mut l = line();
        remember(&mut l.backlog, bytes, BACKLOG_BYTES);
        l.clients.retain_mut(|c| c.write_all(bytes).is_ok());
    }
}

/// What a client reads when it types with the `console-input` lever off.
pub fn input_refusal() -> String {
    format!(
        "console input is off: {}",
        crate::debug_ctl::how_to_enable(&crate::debug_ctl::CONSOLE_INPUT)
    )
}

/// Attach a runtime-socket client for the rest of its connection: the backlog, then the live
/// output, and its keystrokes to the guest while the lever allows.
pub fn attach(mut client: UnixStream) {
    let Ok(mut out) = client.try_clone() else {
        return;
    };
    let _ = out.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
    let _ = client.set_read_timeout(None);
    {
        // Backlog and registration under one lock, so no output falls between them.
        let mut l = line();
        let (a, b) = l.backlog.as_slices();
        if out.write_all(a).is_err() || out.write_all(b).is_err() {
            return;
        }
        l.clients.push(out);
    }
    let mut buf = [0u8; 1024];
    let mut told = false;
    loop {
        let n = match client.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if !crate::debug_ctl::CONSOLE_INPUT.on() {
            if !told {
                told = true;
                let _ =
                    client.write_all(format!("\r\n[limina: {}]\r\n", input_refusal()).as_bytes());
            }
            continue;
        }
        told = false;
        let input = line().input.as_ref().and_then(|i| i.try_clone().ok());
        if let Some(mut i) = input {
            let _ = i.write_all(&buf[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backlog_keeps_the_most_recent_bytes() {
        let mut b = VecDeque::new();
        remember(&mut b, b"abc", 4);
        remember(&mut b, b"def", 4);
        assert_eq!(b.iter().copied().collect::<Vec<_>>(), b"cdef");
        remember(&mut b, b"0123456789", 4);
        assert_eq!(b.iter().copied().collect::<Vec<_>>(), b"6789");
    }

    #[test]
    fn the_refusal_says_how_to_turn_input_on() {
        let r = input_refusal();
        assert!(r.contains("LIMINA_CONSOLE_INPUT=1"), "{r}");
        assert!(
            r.contains("limina debug <vm> lever console-input on"),
            "{r}"
        );
    }
}
