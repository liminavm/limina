// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Running-state tracking: the `run/lock` flock + `run/supervisor.pid`.
//!
//! The flock is the source of truth ("is it running?") because the kernel releases
//! it on process death, however the supervisor exits (every exit path is
//! `process::exit`, and SIGKILL is always possible). The pidfile is only used to
//! find *who* to signal; a stale pidfile next to a free lock means "stopped".

use anyhow::{Context, Result};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use super::bundle::VmBundle;

pub const LOCK_FILE: &str = "lock";
pub const PID_FILE: &str = "supervisor.pid";

/// Held (or deliberately leaked via `std::mem::forget`) by the running supervisor.
/// Dropping it closes the fd and releases the flock.
#[derive(Debug)]
pub struct RunLock {
    _file: File,
}

fn lock_path(bundle: &VmBundle) -> PathBuf {
    bundle.run_dir().join(LOCK_FILE)
}

fn pid_path(bundle: &VmBundle) -> PathBuf {
    bundle.run_dir().join(PID_FILE)
}

fn try_flock(f: &File, op: libc::c_int) -> std::io::Result<()> {
    let rc = unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Take the exclusive run lock, failing fast if the VM is already running.
pub fn acquire(bundle: &VmBundle) -> Result<RunLock> {
    std::fs::create_dir_all(bundle.run_dir())
        .with_context(|| format!("creating {}", bundle.run_dir().display()))?;
    let path = lock_path(bundle);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    try_flock(&file, libc::LOCK_EX).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            let who = read_pid(bundle)
                .map(|p| format!(" (supervisor pid {p})"))
                .unwrap_or_default();
            anyhow::anyhow!("VM {} is already running{who}", bundle.dir_name())
        } else {
            anyhow::Error::new(e).context(format!("locking {}", path.display()))
        }
    })?;
    Ok(RunLock { _file: file })
}

/// Record this process as the VM's supervisor. Call right after `acquire`.
pub fn write_pidfile(bundle: &VmBundle) -> Result<()> {
    let path = pid_path(bundle);
    std::fs::write(&path, format!("{}\n", std::process::id()))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn read_pid(bundle: &VmBundle) -> Option<i32> {
    std::fs::read_to_string(pid_path(bundle))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmStatus {
    Stopped,
    Running {
        /// The supervisor pid from the pidfile; 0 if the pidfile is unreadable
        /// (running is still certain — the flock says so).
        pid: i32,
    },
}

impl VmStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, VmStatus::Running { .. })
    }
}

/// Ask a running supervisor to stop. One SIGTERM runs the graceful ladder (control-plane
/// SHUTDOWN → GPIO power button → the stock guest agent → wait); `force` sends a **second**
/// SIGTERM, which the supervisor treats as "kill now" — the only thing that ends a guest which
/// ignores every rung.
///
/// The second signal must be *counted*, and standard signals do not queue: two SIGTERMs sent
/// back to back while one is still pending collapse into a single delivery, leaving the
/// supervisor's counter at 1 and silently turning a `--force` into an ordinary stop. So force
/// re-delivers on a short cadence until the supervisor is gone (or we run out of patience),
/// which also covers a supervisor busy enough to still be in the first handler.
pub fn signal_stop(pid: i32, force: bool) -> Result<()> {
    anyhow::ensure!(pid > 0, "refusing to signal pid {pid}");
    let deliver = || -> Result<()> {
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!("signaling supervisor pid {pid}: {e}");
        }
        Ok(())
    };
    deliver()?;
    if force {
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            // Gone already (or reaped by its parent): nothing left to force.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return Ok(());
            }
            deliver()?;
        }
    }
    Ok(())
}

/// Ask a running supervisor to suspend the VM (M9.2). One `SIGTSTP` starts the suspend bracket:
/// the supervisor relays it to the worker, which pulses the guest suspend button, waits for the
/// guest to s2idle-quiesce, snapshots it, and exits 126 — after which the supervisor persists the
/// `[suspended]` state and tears down. If the guest cannot quiesce (e.g. a virtiofs mount refuses
/// s2idle) the bracket wakes it and the VM keeps running; the request is a no-op the caller detects
/// by the VM still being up (see [`wait_stopped`]).
pub fn signal_suspend(pid: i32) -> Result<()> {
    anyhow::ensure!(pid > 0, "refusing to signal pid {pid}");
    let rc = unsafe { libc::kill(pid, libc::SIGTSTP) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        anyhow::bail!("signaling supervisor pid {pid} to suspend: {e}");
    }
    Ok(())
}

/// Poll a bundle's run state until it stops or the timeout expires.
pub fn wait_stopped(bundle: &VmBundle, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !status(bundle).is_running() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Probe a bundle's run state via the flock (shared probe, immediately released).
pub fn status(bundle: &VmBundle) -> VmStatus {
    let Ok(file) = File::open(lock_path(bundle)) else {
        return VmStatus::Stopped; // no lock file → never started
    };
    match try_flock(&file, libc::LOCK_SH) {
        Ok(()) => VmStatus::Stopped, // lock free; fd close releases our probe
        Err(_) => VmStatus::Running {
            pid: read_pid(bundle).unwrap_or(0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmlib::bundle::tests::{basic_opts, scratch_library};
    use crate::vmlib::import::create;

    /// flock conflicts even between two fds in one process, so fail-fast and the
    /// status probe are both testable without spawning children.
    #[test]
    fn acquire_is_exclusive_and_status_tracks_it() {
        let lib = scratch_library("runtime");
        let bundle = create(&basic_opts("locky"), &lib).unwrap();

        assert_eq!(status(&bundle), VmStatus::Stopped);

        let lock = acquire(&bundle).unwrap();
        write_pidfile(&bundle).unwrap();
        assert_eq!(
            status(&bundle),
            VmStatus::Running {
                pid: std::process::id() as i32
            }
        );

        // A second acquire fails fast and names the holder.
        let err = acquire(&bundle).unwrap_err().to_string();
        assert!(err.contains("already running"), "unexpected error: {err}");
        assert!(
            err.contains(&std::process::id().to_string()),
            "error should name the pid: {err}"
        );

        // Releasing the lock flips status back; the stale pidfile doesn't lie.
        drop(lock);
        assert_eq!(status(&bundle), VmStatus::Stopped);

        std::fs::remove_dir_all(&lib).ok();
    }
}
