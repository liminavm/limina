// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Guest suspend-button plumbing for the worker (M9 freeze trigger, stock tier).
//!
//! libkrun's GPIO device exposes a second `gpio-keys` button that emits `KEY_SLEEP`
//! (alongside the poweroff/restart key). Writing to the "suspend eventfd" raises that
//! GPIO line (`Gpio::process` → `trigger_suspend_key`, in `krun-devices`); a stock guest's
//! `systemd-logind` maps `KEY_SLEEP` to `HandleSuspendKey` (default: suspend) and enters
//! suspend-to-idle — **with no guest agent**. That is the stock-tier freeze trigger the M9
//! host-side snapshot needs (the guest runs its own PM freeze/resume around the snapshot).
//!
//! We create that `EventFd`, hand it to `build_microvm` (it moves into the guest's GPIO
//! device), and write to it from a `SIGUSR2` handler. We choose `SIGUSR2` so it stays
//! distinct from the shutdown path (`SIGTERM`/`SIGINT`) and the snapshot path (`SIGUSR1`).
//! For M9.2 the supervisor will drive this around a host-side snapshot; the signal is the
//! spike/mechanism seam.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{anyhow, Result};
use utils::eventfd::{EventFd, EFD_NONBLOCK};

/// Write end of the suspend eventfd, published for the (async-signal-safe) handler. On macOS
/// `EventFd` is a pipe; `as_raw_fd()` is the *read* end (which the GPIO subscriber epolls), so
/// the handler must write the *write* end (`get_write_fd()`) — mirroring `snapshot.rs`.
static SUSPEND_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_sigusr2(_sig: libc::c_int) {
    let fd = SUSPEND_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let one: u64 = 1;
        // write() is async-signal-safe; the eventfd is non-blocking.
        unsafe {
            libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
        }
    }
}

/// Create the guest suspend eventfd and install a `SIGUSR2` handler that signals it.
/// Returns the `EventFd` to pass to `build_microvm` (it is moved into the guest's GPIO
/// device; the raw fd stays valid for the VM's lifetime, which is what the handler writes).
pub fn install() -> Result<EventFd> {
    let efd = EventFd::new(EFD_NONBLOCK).map_err(|e| anyhow!("suspend EventFd: {e}"))?;
    let write_fd: RawFd = efd.get_write_fd();
    SUSPEND_WRITE_FD.store(write_fd, Ordering::SeqCst);

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sigusr2 as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGUSR2, &sa, std::ptr::null_mut()) != 0 {
            return Err(anyhow!(
                "installing SIGUSR2 handler: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(efd)
}
