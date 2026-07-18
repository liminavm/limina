// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Guest wake-button plumbing for the worker (M9 restore).
//!
//! libkrun's GPIO device exposes a `KEY_WAKEUP` button whose FDT node carries `wakeup-source`, so
//! the guest arms its irq for wake (`enable_irq_wake`) during suspend-to-idle. Writing to the "wake
//! eventfd" raises that GPIO line (`Gpio::process` → `trigger_wake_key`, in `krun-devices`), which is
//! the **only** line that brings the guest OUT of s2idle — the other buttons' edges are masked while
//! suspended and do not wake it.
//!
//! In production the worker pulses this **internally** on the M9 restore path, once a quiesced
//! snapshot has been reloaded and the vCPUs are live, so the guest runs its own s2idle `.resume`
//! path and re-initialises its virtio devices. [`pulse`] is that entry point. We also install a
//! `SIGWINCH` handler as an out-of-band **test seam** (drive the wake on a live s2idle guest without
//! a restore); `SIGWINCH` is never delivered to the headless worker in normal operation.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{anyhow, Result};
use utils::eventfd::{EventFd, EFD_NONBLOCK};

/// Write end of the wake eventfd, published for the (async-signal-safe) handler and [`pulse`]. On
/// macOS `EventFd` is a pipe; `as_raw_fd()` is the *read* end (which the GPIO subscriber epolls), so
/// we must write the *write* end (`get_write_fd()`) — mirroring `shutdown.rs`/`suspend.rs`.
static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

fn pulse_fd() {
    let fd = WAKE_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let one: u64 = 1;
        // write() is async-signal-safe; the eventfd is non-blocking.
        unsafe {
            libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
        }
    }
}

extern "C" fn handle_sigwinch(_sig: libc::c_int) {
    pulse_fd();
}

/// Inject a guest wake (raise the `KEY_WAKEUP` GPIO line). Called on the M9 restore path once the
/// reloaded guest's vCPUs are live, to bring it out of s2idle. Async-signal-safe.
// Wired into the restore path in a follow-up; the SIGWINCH seam exercises the same mechanism now.
#[allow(dead_code)]
pub fn pulse() {
    pulse_fd();
}

/// Create the guest wake eventfd and install the `SIGWINCH` test-seam handler. Returns the `EventFd`
/// to pass to `build_microvm` (it is moved into the guest's GPIO device; the raw fd stays valid for
/// the VM's lifetime, which is what [`pulse`]/the handler writes).
pub fn install() -> Result<EventFd> {
    let efd = EventFd::new(EFD_NONBLOCK).map_err(|e| anyhow!("wake EventFd: {e}"))?;
    let write_fd: RawFd = efd.get_write_fd();
    WAKE_WRITE_FD.store(write_fd, Ordering::SeqCst);

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sigwinch as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut()) != 0 {
            return Err(anyhow!(
                "installing SIGWINCH handler: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(efd)
}
