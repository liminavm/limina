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
use std::os::fd::BorrowedFd;
use std::os::unix::io::{IntoRawFd, RawFd};

use anyhow::{Context, Result, anyhow};
use krun_lib::api::device_builders::ConsoleDevice;
use krun_lib::vmm::resources::{SerialConsoleConfig, VmResources};

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

/// The ports of the guest's one virtio-console device, in the order they were added.
///
/// The data console comes first, so it is `hvc0` whenever there is one, and the named agent
/// ports follow it (`/dev/vport0p1`, `/dev/vport0p2`, …). Guests find those by *name* under
/// `/dev/virtio-ports/`, so the index is not load-bearing for any udev rule — but the order is
/// deterministic anyway, which keeps a guest's device topology identical across launches.
#[derive(Default)]
pub struct Ports {
    ports: Vec<Port>,
}

enum Port {
    /// A console port: `None` disables that direction.
    Console {
        input_fd: Option<RawFd>,
        output_fd: RawFd,
    },
    /// A named bidirectional data port on one fd.
    Named { name: &'static str, fd: RawFd },
}

impl Ports {
    /// Add `spec` as a virtio-console (`hvc0`) — a robust, queue-based bidirectional data
    /// console (the PL011 serial is also a working tty now; see [`VirtioConsoleSpec`]).
    ///
    /// A *console* port (not a data port), so the guest exposes it as hvc0 — a data port would
    /// be /dev/vport0p1, and `console=hvc0` would find nothing — taking the fds verbatim with
    /// no `isatty` gating (our output is a plain file and our input a FIFO, neither a tty).
    /// Output is truncated-on-open; input is opened `O_RDWR` so the FIFO is kqueue-pollable and
    /// never reports EOF (the VM reads; a host writer feeds it). The fds are intentionally
    /// leaked into libkrun for the VM's lifetime (= this process).
    pub fn virtio(&mut self, spec: &VirtioConsoleSpec) -> Result<()> {
        let output_fd = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&spec.output)
            .with_context(|| format!("opening virtio-console output {:?}", spec.output))?
            .into_raw_fd();

        let input_fd = match &spec.input {
            Some(path) => Some(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .with_context(|| format!("opening virtio-console input {path:?}"))?
                    .into_raw_fd(),
            ),
            None => None,
        };

        self.ports.insert(
            0,
            Port::Console {
                input_fd,
                output_fd,
            },
        );
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
    pub fn spice(&mut self, guest_fd: RawFd) {
        self.ports.push(Port::Named {
            name: "com.redhat.spice.0",
            fd: guest_fd,
        });
        log::info!("spice: exposed the guest agent port com.redhat.spice.0");
    }

    /// Expose the **named** virtio-serial data port `org.qemu.guest_agent.0` on `guest_fd`.
    ///
    /// The stock `qemu-guest-agent` is gated the same way `spice-vdagent` is, one layer down:
    /// `/usr/lib/udev/rules.d/99-qemu-guest-agent.rules` matches
    /// `SUBSYSTEM=="virtio-ports", ATTR{name}=="org.qemu.guest_agent.0"`, and the unit itself
    /// is `BindsTo=dev-virtio\x2dports-org.qemu.guest_agent.0.device`. Fedora's comps make the
    /// package mandatory in every desktop variant, so on a stock guest this port is the entire
    /// installation cost of the guest agent. The supervisor speaks the protocol
    /// (`crates/limina/src/qga/`); nothing here parses a byte.
    pub fn qga(&mut self, guest_fd: RawFd) {
        self.ports.push(Port::Named {
            name: "org.qemu.guest_agent.0",
            fd: guest_fd,
        });
        log::info!("qga: exposed the guest agent port org.qemu.guest_agent.0");
    }

    /// The console device carrying every port, or `None` when there are none.
    ///
    /// libkrun dups each fd it is handed (separately for input and output), so a named port's
    /// one fd serves both directions with no double close.
    pub fn build(self) -> Result<Option<ConsoleDevice<'static>>> {
        if self.ports.is_empty() {
            return Ok(None);
        }
        // SAFETY: every fd here was leaked to us for the life of the process — opened and
        // `into_raw_fd`'d above, or a socketpair end the supervisor handed this worker — so
        // borrowing it for 'static is sound.
        let borrow = |fd: RawFd| unsafe { BorrowedFd::borrow_raw(fd) };
        let mut builder = ConsoleDevice::builder();
        for port in self.ports {
            match port {
                Port::Console {
                    input_fd,
                    output_fd,
                } => builder.add_console_inout_port(
                    "",
                    input_fd.map(borrow),
                    Some(borrow(output_fd)),
                ),
                Port::Named { name, fd } => {
                    builder.add_inout_port(name, Some(borrow(fd)), Some(borrow(fd)))
                }
            }
            .map_err(|e| anyhow!("virtio-console port: {e:?}"))?;
        }
        builder
            .build()
            .map(Some)
            .map_err(|e| anyhow!("virtio-console: {e:?}"))
    }
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
