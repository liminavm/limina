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
//! Output goes to a file we control; input is optional — `input_fd = -1` yields an
//! output-only console, which the vendored builder handles (no epoll subscriber).

use std::fs::OpenOptions;
use std::os::unix::io::{IntoRawFd, RawFd};

use anyhow::{Context, Result};
use vmm::resources::{SerialConsoleConfig, VmResources};

use crate::config::ConsoleSpec;

/// Attach `console` to `vmr` as the guest's primary serial device.
///
/// The opened fds are intentionally leaked into libkrun (`into_raw_fd`): the device
/// owns them for the lifetime of the VM, which lives until this process exits.
pub fn attach(vmr: &mut VmResources, console: &ConsoleSpec) -> Result<()> {
    let output_fd = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&console.output)
        .with_context(|| format!("opening console output {:?}", console.output))?
        .into_raw_fd();

    // Open input O_RDWR so a FIFO is kqueue-pollable and never sees EOF; `None` -> -1
    // (output-only). The VM is the reader; a host writer feeds guest input.
    let input_fd: RawFd = match &console.input {
        Some(path) => OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening console input {:?}", path))?
            .into_raw_fd(),
        None => -1,
    };

    vmr.disable_implicit_console = true;
    vmr.serial_consoles
        .push(SerialConsoleConfig { input_fd, output_fd });

    Ok(())
}
