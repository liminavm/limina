// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Serial-console wiring for the krun facade.
//!
//! On the EFI/firmware path libkrun's *implicit* serial is hardcoded output-dropped
//! and the EDK2 firmware is silent, so a naive boot is blind. We disable the implicit
//! console and attach our own serial: it becomes the PL011 the firmware uses as
//! ConOut, so EDK2 + GRUB + (with `console=ttyAMA0` in the guest cmdline) the kernel
//! are all visible. Verified end-to-end in `spikes/m1-boot` and `spikes/m1-boot-internal`.
//!
//! Two wirings (see [`ConsoleSpec`]):
//! - [`ConsoleSpec::File`]: output to a file we control; input optional (`-1` = none).
//! - [`ConsoleSpec::Pty`]: a pseudo-terminal for an *interactive* console — the guest
//!   serial is both readable and writable, and the slave path is printed so a human can
//!   `screen <path>` into EDK2/GRUB and (with a console attached) a login shell.

use std::fs::OpenOptions;
use std::os::unix::io::{IntoRawFd, RawFd};

use anyhow::{Context, Result};
use vmm::resources::{PortConfig, SerialConsoleConfig, VirtioConsoleConfigMode, VmResources};

use crate::config::{ConsoleSpec, VirtioConsoleSpec};

/// Attach `console` to `vmr` as the guest's primary serial device.
///
/// The opened fds are intentionally leaked into libkrun (`into_raw_fd` / raw pty fds):
/// the device owns them for the lifetime of the VM, which lives until this process exits.
pub fn attach(vmr: &mut VmResources, console: &ConsoleSpec) -> Result<()> {
    let (input_fd, output_fd) = match console {
        ConsoleSpec::File { output, input } => {
            let output_fd = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(output)
                .with_context(|| format!("opening console output {output:?}"))?
                .into_raw_fd();

            // Open input O_RDWR so a FIFO is kqueue-pollable and never sees EOF; `None`
            // -> -1 (output-only). The VM is the reader; a host writer feeds guest input.
            let input_fd: RawFd = match input {
                Some(path) => OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .with_context(|| format!("opening console input {path:?}"))?
                    .into_raw_fd(),
                None => -1,
            };
            (input_fd, output_fd)
        }
        ConsoleSpec::Pty => open_pty()?,
    };

    vmr.disable_implicit_console = true;
    vmr.serial_consoles.push(SerialConsoleConfig {
        input_fd,
        output_fd,
    });

    Ok(())
}

/// Attach `spec` as a virtio-console (`hvc0`) — a bidirectional console the guest kernel
/// exposes as a real tty without any FDT/driver workaround (the PL011 tty path deadlocks
/// the guest amba probe; see [`VirtioConsoleSpec`]).
///
/// We disable the implicit console so this *explicit* port lands at console id 0 (`hvc0`),
/// then wire it via [`PortConfig::InOut`] — which, unlike libkrun's autoconfigure path,
/// takes the fds verbatim with no `isatty` gating (our output is a plain file and our
/// input a FIFO, neither a tty). Output is truncated-on-open; input is opened `O_RDWR` so
/// the FIFO is kqueue-pollable and never reports EOF (the VM reads; a host writer feeds it).
/// The fds are intentionally leaked into libkrun for the VM's lifetime (= this process).
pub fn attach_virtio(vmr: &mut VmResources, spec: &VirtioConsoleSpec) -> Result<()> {
    let output_fd = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&spec.output)
        .with_context(|| format!("opening virtio-console output {:?}", spec.output))?
        .into_raw_fd();

    let input_fd: RawFd = match &spec.input {
        Some(path) => OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening virtio-console input {path:?}"))?
            .into_raw_fd(),
        None => -1,
    };

    // disable_implicit_console so our explicit port is hvc0 (console id 0), not hvc1.
    // ConsoleInOut (not InOut) marks it as a *console* port so the guest exposes it as
    // hvc0 (a data port would be /dev/vport0p1, and `console=hvc0` would find nothing).
    vmr.disable_implicit_console = true;
    vmr.virtio_consoles
        .push(VirtioConsoleConfigMode::Explicit(vec![
            PortConfig::ConsoleInOut {
                input_fd,
                output_fd,
            },
        ]));

    Ok(())
}

/// Allocate a pseudo-terminal master and return `(input_fd, output_fd)` for the guest
/// serial, both referring to the master (separate fds so the builder can own each without
/// a double close). The master is non-blocking: the guest writes serial bytes from the
/// vCPU thread (`PL011::handle_write`), so a slow or absent reader must never block it —
/// detached, output bytes are dropped rather than stalling a vCPU. The slave device path
/// is printed for a human to attach with `screen <path>` (or `minicom`, `cu`).
fn open_pty() -> Result<(RawFd, RawFd)> {
    // SAFETY: standard POSIX pty allocation; we check every return value.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master < 0 {
        return Err(std::io::Error::last_os_error()).context("posix_openpt");
    }
    if unsafe { libc::grantpt(master) } != 0 {
        return Err(std::io::Error::last_os_error()).context("grantpt");
    }
    if unsafe { libc::unlockpt(master) } != 0 {
        return Err(std::io::Error::last_os_error()).context("unlockpt");
    }

    // ptsname is not thread-safe, but we call it once before any threads touch the pty.
    let slave_ptr = unsafe { libc::ptsname(master) };
    if slave_ptr.is_null() {
        return Err(std::io::Error::last_os_error()).context("ptsname");
    }
    let slave_path = unsafe { std::ffi::CStr::from_ptr(slave_ptr) }
        .to_string_lossy()
        .into_owned();

    // Non-blocking master so a detached/slow reader can't stall the vCPU serial write.
    let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
        return Err(std::io::Error::last_os_error()).context("set pty master O_NONBLOCK");
    }

    // The builder owns input_fd and output_fd separately (each wrapped in a File), so hand
    // it two distinct fds for the one master; dup shares the file description (and its
    // O_NONBLOCK), so both ends stay non-blocking.
    let output_fd = unsafe { libc::dup(master) };
    if output_fd < 0 {
        return Err(std::io::Error::last_os_error()).context("dup pty master");
    }

    // Printed (not logged) so it's visible regardless of RUST_LOG; this is how the human
    // finds the console to attach to.
    println!("limina: interactive serial console at {slave_path} — attach with: screen {slave_path}");

    Ok((master, output_fd))
}
