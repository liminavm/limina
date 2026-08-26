// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Orderly-shutdown plumbing for the worker.
//!
//! libkrun exposes a "shutdown eventfd": writing to it raises the guest's GPIO
//! power-button line (`Gpio::process` → `trigger_restart_key`, in `krun-devices`),
//! asking the guest to power off orderly. We create that `EventFd`, hand it to
//! `build_microvm`, and write to it from a SIGTERM/SIGINT handler — so the supervisor
//! (SIGTERM) or a terminal Ctrl-C (SIGINT, when run directly) triggers a graceful
//! guest shutdown. If the guest ignores the power button, the supervisor escalates to
//! SIGKILL (decision D3 — the worker is disposable; the UI survives).
//!
//! Whether a *stock* EFI guest honors the GPIO power button depends on its ACPI/DT
//! advertising the button; the enhanced tier (our DT / limina-agent) makes it reliable.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{anyhow, Result};
use utils::eventfd::{EventFd, EFD_NONBLOCK};

/// Write end of the shutdown eventfd, published for the (async-signal-safe) handler. On macOS
/// `EventFd` is a pipe; `as_raw_fd()` is the *read* end (which the GPIO subscriber epolls), so the
/// handler must write the *write* end (`get_write_fd()`) — mirroring `snapshot.rs`/`suspend.rs`.
/// Writing the read end silently EBADFs, so the power button never fired and shutdown fell through
/// to the supervisor's SIGKILL escalation.
static SHUTDOWN_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_signal(_sig: libc::c_int) {
    let fd = SHUTDOWN_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let one: u64 = 1;
        // write() is async-signal-safe; the eventfd is non-blocking.
        unsafe {
            libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
        }
    }
}

/// Create the guest shutdown eventfd and install SIGTERM/SIGINT handlers that signal
/// it. Returns the `EventFd` to pass to `build_microvm` (it is moved into the guest's
/// GPIO device; the raw fd stays valid for the VM's lifetime, which is what the
/// handler writes to).
pub fn install() -> Result<EventFd> {
    let efd = EventFd::new(EFD_NONBLOCK).map_err(|e| anyhow!("shutdown EventFd: {e}"))?;
    let write_fd: RawFd = efd.get_write_fd();
    SHUTDOWN_WRITE_FD.store(write_fd, Ordering::SeqCst);

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_signal as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0
            || libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut()) != 0
        {
            return Err(anyhow!(
                "installing signal handlers: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(efd)
}
