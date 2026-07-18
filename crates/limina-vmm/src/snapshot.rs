// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Suspend-trigger plumbing for the worker (M9 suspend/resume).
//!
//! When `--snapshot-file` is set, the supervisor suspends the VM by sending the worker a
//! `SIGUSR1`. Serializing a snapshot is not async-signal-safe (it locks the `Vmm`, quiesces the
//! vCPUs over channels, allocates), so the handler does the one safe thing — write a byte to an
//! eventfd — and a dedicated *trigger thread* blocks on that fd, then does the real work
//! (`Vmm::save_snapshot` → exit 126). This mirrors [`crate::shutdown`], which likewise defers the
//! non-signal-safe work (there, to the guest's GPIO device).
//!
//! We choose SIGUSR1 (not the SIGTERM/SIGINT the shutdown path owns) so suspend and power-off stay
//! independent signals; its default disposition is *terminate*, so the handler is only installed
//! when a snapshot file was requested.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{anyhow, Result};
use utils::eventfd::EventFd;

/// Write end of the trigger eventfd, published for the (async-signal-safe) handler. On macOS
/// `EventFd` is a pipe; `as_raw_fd()` is the *read* end, so the handler must target the write end.
static TRIGGER_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_sigusr1(_sig: libc::c_int) {
    let fd = TRIGGER_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let one: u64 = 1;
        // write() is async-signal-safe.
        unsafe {
            libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
        }
    }
}

/// Install the SIGUSR1 suspend-trigger handler and return the trigger `EventFd`. The caller spawns
/// a thread that blocks on [`EventFd::read`] and, on wake, snapshots + exits. Created *blocking*
/// (flag 0) so that read blocks until the signal fires.
pub fn install() -> Result<EventFd> {
    let efd = EventFd::new(0).map_err(|e| anyhow!("snapshot trigger EventFd: {e}"))?;
    let write_fd: RawFd = efd.get_write_fd();
    TRIGGER_WRITE_FD.store(write_fd, Ordering::SeqCst);

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sigusr1 as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()) != 0 {
            return Err(anyhow!(
                "installing SIGUSR1 handler: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(efd)
}
