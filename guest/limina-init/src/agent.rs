// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The control-plane agent (limina-proto over vsock) — the seed of `limina-agent` (D8/M5).
//!
//! Connects out to the host (`CID_HOST:port`, the direction the roadmap fixes: guest
//! connects, host listens), sends HELLO with capabilities + boot facts, then serves the
//! control loop: periodic HEARTBEATs, ERROR(UNSUPPORTED) for unknown message types
//! (never fatal), and SHUTDOWN → SHUTDOWN_ACK → return, upon which PID 1 powers off.
//!
//! Everything is best-effort and panic-free: any connection/protocol failure just
//! returns, and the caller proceeds to its normal power-off path (as PID 1 we must
//! never die). The heartbeat cadence is driven by `poll(2)` on the socket rather than a
//! read timeout, so a slow frame can never be torn mid-read by a timer.

use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::FromRawFd;
use std::time::Duration;

use limina_proto::{read_message, write_message, Heartbeat, Hello, Message, CHANNEL_CONTROL};

/// How often the agent emits a HEARTBEAT while the channel is idle.
const HEARTBEAT_EVERY: Duration = Duration::from_millis(1000);

/// How the agent session ended — the caller's flow depends on it.
#[derive(PartialEq, Eq)]
pub enum AgentEnd {
    /// The host ordered SHUTDOWN (acked): power off NOW, regardless of other cmdline
    /// modes (`limina.hold` etc.).
    Shutdown,
    /// The channel ended without a shutdown order (host gone, connect/protocol failure):
    /// fall through to the init's normal flow.
    Disconnected,
}

/// Run the agent until the host orders a shutdown or the channel dies.
pub fn run(port: u32) -> AgentEnd {
    let Some(mut stream) = connect(port) else {
        super::klog(b"[limina-init] agent: vsock connect failed");
        return AgentEnd::Disconnected;
    };
    let end = match serve(&mut stream) {
        Ok(end) => end,
        Err(_) => {
            super::klog(b"[limina-init] agent: channel error; proceeding to power-off");
            AgentEnd::Disconnected
        }
    };
    // Give the just-written final frame (SHUTDOWN_ACK) a moment to drain through the
    // virtio queue + host muxer before PID 1 yanks the machine down with reboot(2).
    unsafe { super::sleep_ms(50) };
    end
}

/// Connect to `CID_HOST:port` with a brief retry (the host listener + libkrun muxer may
/// need a moment after boot). Returns the socket wrapped as a `File` (it is just an fd
/// we `read`/`write`/`poll`).
fn connect(port: u32) -> Option<File> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_HOST;

        for _ in 0..100 {
            let r = libc::connect(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            );
            if r == 0 {
                return Some(File::from_raw_fd(fd));
            }
            super::sleep_ms(20);
        }
        libc::close(fd);
        None
    }
}

/// The control loop. `Ok(_)` = orderly end (SHUTDOWN acked, or the host hung up).
fn serve(stream: &mut File) -> std::io::Result<AgentEnd> {
    let pagesize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    write_message(
        stream,
        CHANNEL_CONTROL,
        &Message::Hello(Hello {
            agent: concat!("limina-init/", env!("CARGO_PKG_VERSION")).to_string(),
            caps: vec!["heartbeat".to_string(), "shutdown".to_string()],
            pagesize,
        }),
    )?;

    let mut seq: u64 = 0;
    loop {
        if !readable_within(stream, HEARTBEAT_EVERY)? {
            // Idle tick: prove liveness. (Also flows before WELCOME arrives — harmless.)
            seq += 1;
            write_message(
                stream,
                CHANNEL_CONTROL,
                &Message::Heartbeat(Heartbeat { seq }),
            )?;
            continue;
        }
        match read_message(stream) {
            Ok((_, Message::Shutdown(_))) => {
                // Ack, then return — PID 1 powers off immediately.
                let _ = write_message(stream, CHANNEL_CONTROL, &Message::ShutdownAck);
                return Ok(AgentEnd::Shutdown);
            }
            Ok((_, Message::Unknown { msg_type, .. })) => {
                // Never fatal: report and keep serving.
                write_message(stream, CHANNEL_CONTROL, &Message::unsupported(msg_type))?;
            }
            // WELCOME (cap sets unused yet), host heartbeats/errors, stray pressure reports
            // (a guest→host message this agent never receives): noted, no action.
            Ok((_, Message::Welcome(_)))
            | Ok((_, Message::Heartbeat(_)))
            | Ok((_, Message::MemPressure(_)))
            | Ok((_, Message::Error(_))) => {}
            // Clipboard/timesync/vcpu frames (this agent never advertises those caps), HELLO
            // from a host, stray acks: ignore rather than die. A CpuTarget in particular is
            // safe to drop on the floor: the host only ever learns the online count from a
            // report this agent does not send, so it never concludes anything from silence.
            Ok((_, Message::CpuPressure(_)))
            | Ok((_, Message::CpuTarget(_)))
            | Ok((_, Message::ClipOffer(_)))
            | Ok((_, Message::ClipRequest(_)))
            | Ok((_, Message::ClipData(_)))
            | Ok((_, Message::FidoReport(_)))
            | Ok((_, Message::DisplayLayout(_)))
            | Ok((_, Message::TimeSync(_)))
            | Ok((_, Message::Hello(_)))
            | Ok((_, Message::ShutdownAck)) => {}
            // Host hung up: orderly end (the supervisor side owns forcing teardown).
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(AgentEnd::Disconnected),
            Err(e) => return Err(e),
        }
    }
}

/// `poll(2)` the socket for readability, up to `timeout`. `Ok(false)` = idle tick.
/// Distinguishing readable-vs-idle *before* reading is what keeps `read_message`'s
/// blocking `read_exact`s from ever being torn mid-frame by a heartbeat timer.
fn readable_within(stream: &File, timeout: Duration) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut pfd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let r = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
    match r {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                Ok(false) // EINTR: treat as an idle tick, not a channel error
            } else {
                Err(err)
            }
        }
    }
}
