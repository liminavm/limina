// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina-init — PID 1 for the L1 tiny test guest.
//!
//! The whole job (for now): prove userspace was reached, then power the VM off cleanly.
//! It mounts `devtmpfs` so `/dev/console` exists, writes a marker the host test harness
//! greps for, then issues a PSCI power-off via `reboot(RB_POWER_OFF)` — which libkrun
//! turns into process exit, giving the supervisor a clean stop (exit 0). This is the
//! sub-second L1 counterpart to the multi-second stock-Fedora L2 boot.
//!
//! As PID 1 it must NEVER return or panic (the kernel panics if init dies), so every
//! path ends at [`power_off`]. It will grow into the limina-agent (vsock control plane).

use std::ffi::CString;

/// Marker the host harness asserts on. Keep in sync with `crates/limina-test`.
const MARKER: &[u8] = b"\n[limina-init] LIMINA_L1_USERSPACE_OK\n";

fn main() {
    announce();
    power_off();
}

/// Mount devtmpfs and emit the marker on every channel we can, best-effort.
///
/// The kernel may not have set up a console for us (`unable to open an initial
/// console`), so the reliable path is `/dev/kmsg`: writes there go through printk and
/// out the serial console the host is capturing. We also try `/dev/console` and stdout.
fn announce() {
    unsafe {
        // mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) — populates /dev/kmsg, console.
        let src = CString::new("devtmpfs").unwrap();
        let target = CString::new("/dev").unwrap();
        let fstype = CString::new("devtmpfs").unwrap();
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        );

        // /dev/kmsg → printk → serial: the channel that works even with no stdio.
        write_to(c"/dev/kmsg", MARKER);
        write_to(c"/dev/console", MARKER);
        write_all(1, MARKER);
    }
}

/// Open `path` write-only and write `buf`, ignoring any failure.
unsafe fn write_to(path: &std::ffi::CStr, buf: &[u8]) {
    let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
    if fd >= 0 {
        write_all(fd, buf);
        libc::close(fd);
    }
}

/// Write the whole buffer, ignoring partial writes/errors (best-effort, never panics).
unsafe fn write_all(fd: libc::c_int, mut buf: &[u8]) {
    while !buf.is_empty() {
        let n = libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len());
        if n <= 0 {
            break;
        }
        buf = &buf[n as usize..];
    }
}

/// Clean PSCI power-off. Never returns; spins as a last resort so PID 1 can't die.
fn power_off() -> ! {
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
        // reboot only returns on failure (e.g. missing privilege) — keep PID 1 alive.
        loop {
            libc::pause();
        }
    }
}
