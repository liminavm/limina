// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The launcher lets go of the caller's stdout and stderr before it reports the worker's exit.
//!
//! The supervisor exits on that report, and whoever reads its output (a shell pipeline, a test
//! runner) waits for every writer to close. The launcher unloads its own job after reporting, so
//! one still holding the handed-over stdio outlives the supervisor that owned it, for as long as
//! `launchctl bootout` takes; nextest reads that as a leaked process.
//!
//! Drives the real `limina-vmm` as the launcher, through launchd; the worker it starts only
//! prints its help. Skips where there is no gui domain to load a job into (an ssh session).

use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use limina_launch::{LaunchError, LaunchSpec};

/// Read everything `pipe` holds right now. `true` once every writer has closed it.
fn drain(pipe: &mut std::fs::File, out: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
            Err(e) => panic!("reading the worker's output: {e}"),
        }
    }
}

#[test]
fn the_launcher_releases_the_callers_stdio_before_reporting_the_exit() {
    let mut fds = [0; 2];
    // SAFETY: a plain pipe(2) into a two-element array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    // SAFETY: both fds are fresh and owned only here.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: fcntl on an fd we own.
    unsafe {
        let flags = libc::fcntl(read_end.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(
            read_end.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        );
    }
    let mut pipe = std::fs::File::from(read_end);

    let dir = std::env::temp_dir().join(format!("limina-launcher-stdio-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let launched = limina_launch::launch(LaunchSpec {
        program: Path::new(env!("CARGO_BIN_EXE_limina-vmm")),
        args: vec!["--help".into()],
        env: Vec::new(),
        cwd: dir.clone(),
        fds: vec![(write_end.as_raw_fd(), 1), (write_end.as_raw_fd(), 2)],
        plist_dir: &dir,
        start_timeout: Duration::from_secs(240),
    });
    drop(write_end);
    let mut launched = match launched {
        Ok(l) => l,
        Err(LaunchError::Unavailable(e)) => {
            eprintln!("skipping: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => panic!("{e}"),
    };

    // Block on the report itself: the window being tested opens the moment it is written. The
    // help text fits the pipe's buffer, so the worker cannot block on an undrained pipe.
    let status = launched.wait().expect("waiting on the launcher");
    // The exit report is the supervisor's cue to exit, so nothing of the run may hold the
    // caller's output once it has arrived: no waiting here.
    let mut out = Vec::new();
    let closed = drain(&mut pipe, &mut out);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !out.is_empty(),
        "the worker's output never reached the caller's pipe ({status:?})"
    );
    assert!(
        closed,
        "the worker's exit ({status:?}) was reported while the launcher still held the caller's \
         stdout/stderr"
    );
}
