// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M9.2 suspend-bracket trigger for the worker.
//!
//! This is the *production* suspend path (vs the raw M9.1 seams). Where `SIGUSR1`
//! ([`crate::snapshot`]) snapshots *immediately* — a running guest, no quiesce check, for the M9.1
//! mechanism tests — the **bracket** performs the whole indivisible suspend operation the supervisor
//! asks for: pulse the guest suspend button ([`crate::suspend::pulse`]), wait for the guest to
//! s2idle-**quiesce** (every virtio device reset to `INIT`; the [`krun_vmm::Vmm::is_quiesced`]
//! oracle), and only *then* snapshot — so we never capture a mid-flight machine that would wedge on
//! restore. If the guest never quiesces within the timeout the bracket **aborts** (wakes the guest
//! back out of s2idle and keeps running); the supervisor sees no exit-126 and reports the suspend
//! failed. The mechanism lives here (worker, where the quiesce oracle can read device state); the
//! *policy* (persist `Suspended{snapshot}`, teardown) lives in the supervisor.
//!
//! We choose `SIGTSTP` — semantically "suspend" — kept distinct from `SIGUSR1` (raw snapshot),
//! `SIGUSR2` (raw suspend button), `SIGHUP` (restart), and `SIGTERM`/`SIGINT` (power-off). Its
//! default disposition is *stop the process* (job control); installing a handler overrides that, and
//! the headless worker has no controlling terminal, so `SIGTSTP` only ever arrives from the
//! supervisor relaying a `limina suspend`. Installed only when a snapshot file was requested.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{anyhow, Result};
use utils::eventfd::EventFd;

/// Write end of the bracket-trigger eventfd, published for the (async-signal-safe) handler. On macOS
/// `EventFd` is a pipe; `as_raw_fd()` is the *read* end, so the handler must target the write end.
static TRIGGER_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_sigtstp(_sig: libc::c_int) {
    let fd = TRIGGER_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let one: u64 = 1;
        // write() is async-signal-safe.
        unsafe {
            libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
        }
    }
}

/// Install the `SIGTSTP` bracket-trigger handler and return the trigger `EventFd`. The caller spawns
/// a thread that blocks on [`EventFd::read`] and, on wake, runs the suspend bracket. Created
/// *blocking* (flag 0) so the read blocks until the signal fires.
pub fn install() -> Result<EventFd> {
    let efd = EventFd::new(0).map_err(|e| anyhow!("bracket trigger EventFd: {e}"))?;
    let write_fd: RawFd = efd.get_write_fd();
    TRIGGER_WRITE_FD.store(write_fd, Ordering::SeqCst);

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sigtstp as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGTSTP, &sa, std::ptr::null_mut()) != 0 {
            return Err(anyhow!(
                "installing SIGTSTP handler: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(efd)
}
