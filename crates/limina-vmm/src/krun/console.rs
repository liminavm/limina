// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Serial-console wiring for the krun facade.
//!
//! On the EFI/firmware path a naive boot is blind (no serial, silent EDK2 firmware),
//! so we attach our own explicit serial: it becomes the PL011 the firmware uses as
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

    vmr.serial_consoles.push(SerialConsoleConfig {
        input_fd,
        output_fd,
    });

    Ok(())
}

/// Attach an **output-dropped PL011** when no serial console was requested. The device must
/// exist regardless: consoles are explicit-only since the upstream config redesign, and a
/// guest booted with no PL011 at all wedges intermittently in early boot (the cold-boot
/// wedge caught rebasing libkrun: vCPU 0 spins in-guest at 100% with zero VM
/// exits, secondaries never online, ~3/4 of no-console boots). This restores the device
/// shape the old implicit console always guaranteed.
pub fn attach_dropped(vmr: &mut VmResources) {
    vmr.serial_consoles.push(SerialConsoleConfig {
        input_fd: -1,
        output_fd: -1,
    });
}

/// Attach `spec` as a virtio-console (`hvc0`) — a robust, queue-based bidirectional data
/// console (the PL011 serial is also a working tty now; see [`VirtioConsoleSpec`]).
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

    // Consoles are explicit-only since the upstream config redesign, so this port is
    // hvc0 (console id 0) by construction. ConsoleInOut (not InOut) marks it as a
    // *console* port so the guest exposes it as hvc0 (a data port would be
    // /dev/vport0p1, and `console=hvc0` would find nothing).
    vmr.virtio_consoles
        .push(VirtioConsoleConfigMode::Explicit(vec![
            PortConfig::ConsoleInOut {
                input_fd,
                output_fd,
            },
        ]));

    Ok(())
}

/// Expose the **named** virtio-serial data port `com.redhat.spice.0` on `guest_fd`.
///
/// That exact name is what stock Fedora's `/usr/lib/udev/rules.d/70-spice-vdagentd.rules`
/// matches on, so its presence is enough to start `spice-vdagent` in a guest with nothing
/// of ours installed — the clipboard's stock-tier baseline (M12 #37).
///
/// We only put the device on the bus. `guest_fd` is one end of a socketpair the
/// **supervisor** created before spawning us (`supervisor::spawn_worker`), and the
/// supervisor speaks the agent protocol on the other end, next to the NSPasteboard it is
/// bridging to. Nothing here parses a byte.
///
/// (The `LIMINA_SPICE_PORT=1` probe this replaced lived in `spikes/m12-spice-port/`, which
/// keeps the transcript of the protocol experiments that settled the broker's behavior.)
pub fn attach_spice_port(vmr: &mut VmResources, guest_fd: RawFd) -> Result<()> {
    attach_named_port(vmr, "com.redhat.spice.0", guest_fd)?;
    log::info!("spice: exposed the guest agent port com.redhat.spice.0");
    Ok(())
}

/// Expose the **named** virtio-serial data port `org.qemu.guest_agent.0` on `guest_fd`.
///
/// The stock `qemu-guest-agent` is gated the same way `spice-vdagent` is, one layer down:
/// `/usr/lib/udev/rules.d/99-qemu-guest-agent.rules` matches
/// `SUBSYSTEM=="virtio-ports", ATTR{name}=="org.qemu.guest_agent.0"`, and the unit itself is
/// `BindsTo=dev-virtio\x2dports-org.qemu.guest_agent.0.device`. Fedora's comps make the
/// package mandatory in every desktop variant, so on a stock guest this port is the entire
/// installation cost of the guest agent. The supervisor speaks the protocol
/// (`crates/limina/src/qga/`); nothing here parses a byte.
pub fn attach_qga_port(vmr: &mut VmResources, guest_fd: RawFd) -> Result<()> {
    attach_named_port(vmr, "org.qemu.guest_agent.0", guest_fd)?;
    log::info!("qga: exposed the guest agent port org.qemu.guest_agent.0");
    Ok(())
}

/// Put one named bidirectional data port on the guest's virtio-console device.
///
/// Appends to the existing explicit console device when there is one, so the ports land
/// after `hvc0` in attach order (`/dev/vport0p1`, `/dev/vport0p2`, …); otherwise stands up a
/// device just for this port. Guests find these by *name* under `/dev/virtio-ports/`, so the
/// index is not load-bearing for any udev rule — but the order is deterministic anyway,
/// which keeps a guest's device topology identical across launches.
fn attach_named_port(vmr: &mut VmResources, name: &str, guest_fd: RawFd) -> Result<()> {
    // One fd for both directions: libkrun dups it separately for input and output
    // (`create_explicit_ports` -> `input_to_raw_fd_dup`), so there is no double close.
    let port = PortConfig::InOut {
        name: name.to_string(),
        input_fd: guest_fd,
        output_fd: guest_fd,
    };

    match vmr.virtio_consoles.iter_mut().find_map(|c| match c {
        VirtioConsoleConfigMode::Explicit(ports) => Some(ports),
        _ => None,
    }) {
        Some(ports) => ports.push(port),
        None => vmr
            .virtio_consoles
            .push(VirtioConsoleConfigMode::Explicit(vec![port])),
    }
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
    println!(
        "limina: interactive serial console at {slave_path} — attach with: screen {slave_path}"
    );

    Ok((master, output_fd))
}
