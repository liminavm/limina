// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina-agent — the product guest agent (M5/D8): the guest end of the control plane.
//!
//! A small daemon for real distro guests (run via `limina-agent.service`): connect out to
//! the host (`CID_HOST:CONTROL_PORT` — the well-known port the limina supervisor always
//! listens on; no configuration needed), send HELLO with capabilities + facts, then
//! serve the channel: heartbeats while idle, ERROR(UNSUPPORTED) for unknown message
//! types (never fatal), and SHUTDOWN → ack → an orderly `systemctl poweroff` (falling
//! back to raw `reboot(2)` on systemd-less guests, e.g. the L1 test guest, where this
//! same binary is exercised end-to-end by `tests/l1_real_agent.rs`).
//!
//! The serve loop intentionally mirrors `guest/limina-init/src/agent.rs` (the frozen test
//! seed) rather than sharing code with it: this one grows product channels (clipboard,
//! display, memory pressure) on its own schedule.
//!
//! Lifecycle: reconnect forever with a small backoff — the agent may start before the
//! host side is reachable, and a dropped channel (host supervisor quirk) must heal
//! without a guest-side restart (systemd `Restart=always` is the outer safety net).

use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::FromRawFd;
use std::time::Duration;

use limina_proto::{
    read_message, write_message, Heartbeat, Hello, Message, CHANNEL_CONTROL, CONTROL_PORT,
};

/// How often the agent emits a HEARTBEAT while the channel is idle.
const HEARTBEAT_EVERY: Duration = Duration::from_millis(1000);
/// Backoff between reconnect attempts.
const RECONNECT_EVERY: Duration = Duration::from_secs(2);

fn main() {
    let port = std::env::var("LIMINA_AGENT_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CONTROL_PORT);
    eprintln!("limina-agent {}: connecting to host port {port}", version());

    // Host shares first (independent of the control channel): mount every
    // `limina-`-tagged virtiofs device at /media/<name>. Needs root, which this daemon
    // has (system unit); a failed mount degrades to mount-by-hand, never fatal.
    mount_limina_shares();

    let mut logged_waiting = false;
    loop {
        match connect_once(port) {
            Some(mut stream) => {
                logged_waiting = false;
                eprintln!("limina-agent: connected");
                match serve(&mut stream) {
                    Ok(End::Shutdown) => {
                        eprintln!("limina-agent: host ordered shutdown; powering off");
                        // Let the just-written SHUTDOWN_ACK drain through the virtio
                        // queue before the machine starts going down.
                        sleep(Duration::from_millis(50));
                        power_off();
                    }
                    Ok(End::Disconnected) => eprintln!("limina-agent: host hung up; reconnecting"),
                    Err(e) => eprintln!("limina-agent: channel error ({e}); reconnecting"),
                }
            }
            None if !logged_waiting => {
                // One line, not a spam-per-retry: the host side may simply not exist
                // (limina older than the control plane, or a non-limina hypervisor).
                eprintln!("limina-agent: host not reachable yet; retrying quietly");
                logged_waiting = true;
            }
            None => {}
        }
        sleep(RECONNECT_EVERY);
    }
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Auto-mount `limina-`-tagged virtiofs shares at `/media/<name>` (same convention as
/// the L1 init seed). Tags come from sysfs (`/sys/fs/virtiofs/<id>/tag`), not the
/// cmdline, so this also works on EFI boots where GRUB owns the cmdline. Skips tags
/// already mounted (agent restarts must not stack mounts).
fn mount_limina_shares() {
    let Ok(devices) = std::fs::read_dir("/sys/fs/virtiofs") else {
        return;
    };
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    for dev in devices.filter_map(|e| e.ok()) {
        let Ok(tag_raw) = std::fs::read(dev.path().join("tag")) else {
            continue;
        };
        let tag = String::from_utf8_lossy(&tag_raw)
            .trim_end_matches(['\0', '\n'])
            .to_string();
        let Some(name) = tag.strip_prefix("limina-") else {
            continue;
        };
        let target = format!("/media/{name}");
        if mounts
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(target.as_str()))
        {
            continue;
        }
        let _ = std::fs::create_dir_all(&target);
        let (Ok(c_tag), Ok(c_target)) = (
            std::ffi::CString::new(tag.clone()),
            std::ffi::CString::new(target.clone()),
        ) else {
            continue;
        };
        let rc = unsafe {
            libc::mount(
                c_tag.as_ptr(),
                c_target.as_ptr(),
                c"virtiofs".as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        if rc == 0 {
            eprintln!("limina-agent: mounted share {tag} at {target}");
        } else {
            eprintln!(
                "limina-agent: failed to mount share {tag} at {target}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// How a served session ended.
enum End {
    Shutdown,
    Disconnected,
}

/// One vsock connect attempt to `CID_HOST:port`.
fn connect_once(port: u32) -> Option<File> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_HOST;
        let r = libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        );
        if r == 0 {
            Some(File::from_raw_fd(fd))
        } else {
            libc::close(fd);
            None
        }
    }
}

/// The control loop (mirrors the limina-init seed; see the module docs for why it's not
/// shared). Heartbeat cadence is poll(2)-driven so a timer can never tear a frame
/// mid-read.
fn serve(stream: &mut File) -> std::io::Result<End> {
    let pagesize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    write_message(
        stream,
        CHANNEL_CONTROL,
        &Message::Hello(Hello {
            agent: format!("limina-agent/{}", version()),
            caps: vec!["heartbeat".to_string(), "shutdown".to_string()],
            pagesize,
        }),
    )?;

    let mut seq: u64 = 0;
    loop {
        if !readable_within(stream, HEARTBEAT_EVERY)? {
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
                let _ = write_message(stream, CHANNEL_CONTROL, &Message::ShutdownAck);
                return Ok(End::Shutdown);
            }
            Ok((_, Message::Unknown { msg_type, .. })) => {
                write_message(stream, CHANNEL_CONTROL, &Message::unsupported(msg_type))?;
            }
            Ok((_, Message::Welcome(_)))
            | Ok((_, Message::Heartbeat(_)))
            | Ok((_, Message::Error(_))) => {}
            // Clipboard frames belong to the session helper (this daemon never
            // advertises the cap); HELLO from a host / stray acks: ignore, don't die.
            Ok((_, Message::ClipOffer(_)))
            | Ok((_, Message::ClipRequest(_)))
            | Ok((_, Message::ClipData(_)))
            | Ok((_, Message::Hello(_)))
            | Ok((_, Message::ShutdownAck)) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(End::Disconnected),
            Err(e) => return Err(e),
        }
    }
}

/// `poll(2)` the socket for readability, up to `timeout`. `Ok(false)` = idle tick.
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
                Ok(false)
            } else {
                Err(err)
            }
        }
    }
}

/// Power the guest off the most orderly way available: systemd if present (a real
/// distro shutdown — unit stops, filesystems unmount), raw `reboot(2)` otherwise
/// (systemd-less guests like the L1 test init's world; needs CAP_SYS_BOOT, which the
/// root service has).
fn power_off() -> ! {
    if let Ok(status) = std::process::Command::new("systemctl")
        .arg("poweroff")
        .status()
    {
        if status.success() {
            // systemd is taking the system down; it will SIGTERM us shortly. Park.
            loop {
                sleep(Duration::from_secs(3600));
            }
        }
    }
    eprintln!("limina-agent: systemctl poweroff unavailable; using reboot(2)");
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    // reboot(2) only returns on failure (e.g. missing CAP_SYS_BOOT): nothing left to do
    // but exit and let the host's escalation ladder finish the job.
    std::process::exit(1);
}

fn sleep(d: Duration) {
    std::thread::sleep(d);
}
