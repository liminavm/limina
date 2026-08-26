// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Guest restart-button plumbing for the worker.
//!
//! libkrun's GPIO device exposes a `gpio-keys` button that emits `KEY_RESTART` (alongside the
//! poweroff `KEY_POWER` and suspend `KEY_SLEEP` keys). Writing to the "restart eventfd" raises that
//! GPIO line (`Gpio::process` → `trigger_restart_key`, in `krun-devices`); a stock guest's
//! `systemd-logind` maps `KEY_RESTART` to `HandleRebootKey` (default: reboot). The guest reboots
//! via PSCI `SYSTEM_RESET`, the worker exits [`WORKER_EXIT_REBOOT`], and — when no stop was
//! requested — the supervisor relaunches a fresh worker. So a host-driven restart is a graceful
//! guest reboot, distinct from the poweroff path (`shutdown.rs`, `SIGTERM`/`SIGINT`) and the
//! suspend path (`suspend.rs`, `SIGUSR2`).
//!
//! We create that `EventFd`, hand it to `build_microvm` (it moves into the guest's GPIO device),
//! and write to it from a `SIGHUP` handler. `SIGHUP` keeps it distinct from the other three signal
//! paths; the supervisor drives it for a host-initiated "Restart VM" action.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{anyhow, Result};
use utils::eventfd::{EventFd, EFD_NONBLOCK};

/// Write end of the restart eventfd, published for the (async-signal-safe) handler. On macOS
/// `EventFd` is a pipe; `as_raw_fd()` is the *read* end (which the GPIO subscriber epolls), so the
/// handler must write the *write* end (`get_write_fd()`) — mirroring `shutdown.rs`/`suspend.rs`.
static RESTART_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_sighup(_sig: libc::c_int) {
    let fd = RESTART_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let one: u64 = 1;
        // write() is async-signal-safe; the eventfd is non-blocking.
        unsafe {
            libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
        }
    }
}

/// Create the guest restart eventfd and install a `SIGHUP` handler that signals it. Returns the
/// `EventFd` to pass to `build_microvm` (it is moved into the guest's GPIO device; the raw fd stays
/// valid for the VM's lifetime, which is what the handler writes).
pub fn install() -> Result<EventFd> {
    let efd = EventFd::new(EFD_NONBLOCK).map_err(|e| anyhow!("restart EventFd: {e}"))?;
    let write_fd: RawFd = efd.get_write_fd();
    RESTART_WRITE_FD.store(write_fd, Ordering::SeqCst);

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sighup as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut()) != 0 {
            return Err(anyhow!(
                "installing SIGHUP handler: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(efd)
}
