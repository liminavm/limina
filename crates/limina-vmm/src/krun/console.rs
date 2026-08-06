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
/// wedge caught rebasing libkrun, 2026-08-06: vCPU 0 spins in-guest at 100% with zero VM
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

/// SPIKE (M12 #1, `spikes/m12-spice-port/`): expose a **named** virtio-serial data port
/// called `com.redhat.spice.0`, which is the exact trigger stock Fedora's
/// `/usr/lib/udev/rules.d/70-spice-vdagentd.rules` matches on to start `spice-vdagentd`.
///
/// Gated behind `LIMINA_SPICE_PORT=1` so it can never perturb a normal boot. The point is
/// to answer the gating M12 question empirically: is a named multiport enough to wake a
/// **stock** guest's dormant spice-vdagent, with zero libkrun changes?
///
/// The host end is a socketpair whose bytes we hex-dump — `vdagentd` announces its
/// capabilities as soon as it opens the port, so *receiving anything at all* is the
/// positive oracle (not just the device node existing).
pub fn attach_spice_probe_port(vmr: &mut VmResources) -> Result<()> {
    // socketpair, not pipes: one fd is both readable and writable, and libkrun dups it
    // for input and output separately (`create_explicit_ports` -> `input_to_raw_fd_dup`),
    // so there's no double-close to worry about.
    let mut fds = [0 as RawFd; 2];
    // SAFETY: standard socketpair; the return value is checked.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("socketpair for the spice probe port");
    }
    let (host_fd, guest_fd) = (fds[0], fds[1]);

    let port = PortConfig::InOut {
        name: "com.redhat.spice.0".to_string(),
        input_fd: guest_fd,
        output_fd: guest_fd,
    };

    // Append to the existing explicit console device if there is one (so our port lands as
    // port 1 = /dev/vport0p1 alongside hvc0); otherwise stand up a device just for it.
    match vmr.virtio_consoles.iter_mut().find_map(|c| match c {
        VirtioConsoleConfigMode::Explicit(ports) => Some(ports),
        _ => None,
    }) {
        Some(ports) => ports.push(port),
        None => vmr
            .virtio_consoles
            .push(VirtioConsoleConfigMode::Explicit(vec![port])),
    }

    // Play the SPICE server's role just far enough to prove the transport: announce our
    // capabilities to the agent and see whether it announces back. `vdagentd` speaks first
    // only in reply, so a silent read would prove nothing — we have to open the dialogue.
    // A real broker announces once per port open, so the probe does too. Announcing on a *repeating* timer looks to `vdagentd`
    // like a new SPICE client connecting over and over ("sent client disconnected" / "New
    // client connected" every interval in its debug log), which resets its clipboard state
    // continuously and suppresses the very grabs we're trying to observe. So: retry only
    // until the agent first speaks, then stay quiet and answer its greeting on demand.
    let greeted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_greeted = greeted.clone();
    std::thread::Builder::new()
        .name("limina-spice-announce".into())
        .spawn(move || {
            // Retry covers the race where the guest hasn't opened the port yet; it stops as
            // soon as the agent says anything, so a live session sees exactly one announce.
            while !writer_greeted.load(std::sync::atomic::Ordering::Relaxed) {
                send_announce(host_fd);
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        })
        .context("spawning the spice probe announce thread")?;

    // Reader: drain (so the guest never blocks) and decode whatever the agent says.
    std::thread::Builder::new()
        .name("limina-spice-probe".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                // SAFETY: reading into a buffer we own, length passed correctly.
                let n = unsafe {
                    libc::read(host_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n <= 0 {
                    log::warn!("spice-probe: host end closed (read returned {n})");
                    return;
                }
                greeted.store(true, std::sync::atomic::Ordering::Relaxed);
                let got = &buf[..n as usize];
                let hex: Vec<String> = got.iter().map(|b| format!("{b:02x}")).collect();
                log::warn!("spice-probe: guest sent {n} bytes: {}", hex.join(" "));
                // Decode the chunk + message header if it's there (8 + 20 bytes).
                if got.len() >= 28 {
                    let u32at = |o: usize| {
                        u32::from_le_bytes([got[o], got[o + 1], got[o + 2], got[o + 3]])
                    };
                    let (msg_type, payload) = (u32at(12), u32at(28));
                    log::warn!(
                        "spice-probe:   chunk{{port={}, size={}}} msg{{protocol={}, type={}, size={}}}",
                        u32at(0),
                        u32at(4),
                        u32at(8),
                        msg_type,
                        u32at(24),
                    );
                    // A fresh `vdagentd` greets us with ANNOUNCE_CAPABILITIES(request=1);
                    // answering that (and only that) is how a restarted daemon re-learns a
                    // client is connected, without the reconnect churn of a blind timer.
                    if msg_type == VD_AGENT_ANNOUNCE_CAPABILITIES && payload == 1 {
                        log::warn!("spice-probe: agent requested an announce — replying once");
                        send_announce(host_fd);
                    }
                }
            }
        })
        .context("spawning the spice probe drain thread")?;

    log::warn!("spice-probe: exposed named virtio-serial port com.redhat.spice.0");
    Ok(())
}

/// `VD_AGENT_ANNOUNCE_CAPABILITIES` — the message type both sides greet each other with.
const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 6;

/// Write one announce to the agent, logging what went out.
fn send_announce(fd: RawFd) {
    let msg = announce_capabilities();
    // SAFETY: writing a buffer we own, length passed correctly.
    let n = unsafe { libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len()) };
    log::warn!("spice-probe: sent ANNOUNCE_CAPABILITIES ({n} bytes written)");
}

/// A `VD_AGENT_ANNOUNCE_CAPABILITIES` from the server (us) to the guest agent, on the wire:
/// `VDIChunkHeader{port, size}` + `VDAgentMessage{protocol, type, opaque, size}` + payload
/// `VDAgentAnnounceCapabilities{request, caps[]}` (see `spice/vd_agent.h`). `request = 1`
/// asks the agent to announce back, which is the reply we're looking for.
fn announce_capabilities() -> Vec<u8> {
    const VDP_CLIENT_PORT: u32 = 1;
    const VD_AGENT_PROTOCOL: u32 = 1;
    // One 32-bit cap word is enough for the clipboard bits; the agent derives caps_size
    // from the message size, so a short word count is legal.
    const CAP_CLIPBOARD: u32 = 3;
    const CAP_CLIPBOARD_BY_DEMAND: u32 = 5;
    const CAP_CLIPBOARD_SELECTION: u32 = 6;
    let caps: u32 =
        (1 << CAP_CLIPBOARD) | (1 << CAP_CLIPBOARD_BY_DEMAND) | (1 << CAP_CLIPBOARD_SELECTION);

    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes()); // request an announce back
    payload.extend_from_slice(&caps.to_le_bytes());

    let mut msg = Vec::new();
    msg.extend_from_slice(&VD_AGENT_PROTOCOL.to_le_bytes());
    msg.extend_from_slice(&VD_AGENT_ANNOUNCE_CAPABILITIES.to_le_bytes());
    msg.extend_from_slice(&0u64.to_le_bytes()); // opaque
    msg.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    msg.extend_from_slice(&payload);

    let mut out = Vec::new();
    out.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
    out.extend_from_slice(&(msg.len() as u32).to_le_bytes());
    out.extend_from_slice(&msg);
    out
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
