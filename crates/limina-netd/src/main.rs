//! limina-netd: the per-VM user-mode NAT a worker spawns for a NIC that is not on vmnet
//! (`docs/design/multi-vm-networking.md` §3.2).
//!
//! The worker starts it with one end of a socketpair as descriptor 3 (`limina_netd_wire`); netd
//! takes the shared rings from it and serves the guest's link until the worker goes away, which
//! it sees as EOF. It has no other way in: no path, no port, no name.

mod stack;

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::process::ExitCode;

use limina_netd_wire::{MAX_FRAME, NetdEnd, TAG_FRAME, drain, ring};

/// The descriptor the worker hands over.
const CHANNEL_FD: i32 = 3;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_millis()
        .init();
    // SAFETY: descriptor 3 is the worker's handover, and nothing else in this process owns it.
    let sock = unsafe { OwnedFd::from_raw_fd(CHANNEL_FD) };
    match serve(sock) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("limina-netd: {e}");
            ExitCode::FAILURE
        }
    }
}

fn serve(sock: OwnedFd) -> io::Result<()> {
    let mut end = NetdEnd::accept(sock)?;
    let mut stack = stack::Stack::new(stack::Link::default());
    let kq = Kqueue::new(end.sock.as_raw_fd())?;
    let mut frame = vec![0u8; MAX_FRAME];
    log::info!("limina-netd: serving the guest's link");
    loop {
        if drain(end.sock.as_fd())? {
            log::info!("limina-netd: the worker went away");
            return Ok(());
        }
        let mut answered = false;
        while let Some((tag, len)) = end
            .from_guest
            .read_into(&mut frame)
            .map_err(|e| io::Error::other(format!("the guest ring: {e:?}")))?
        {
            if tag != TAG_FRAME {
                continue;
            }
            stack.handle(&frame[..len], &mut |out| {
                // A full ring drops the answer; the guest's protocols retry.
                if end.to_guest.push(TAG_FRAME, &[out]).is_ok() {
                    answered = true;
                }
            });
        }
        if answered && end.to_guest.publish() {
            ring(end.sock.as_fd());
        }
        if end.from_guest.park() {
            kq.wait()?;
            end.from_guest.unpark();
        }
    }
}

/// A kqueue watching the channel for doorbells and EOF.
struct Kqueue(OwnedFd);

impl Kqueue {
    fn new(sock: i32) -> io::Result<Self> {
        // SAFETY: plain syscalls on descriptors we own.
        unsafe {
            let kq = libc::kqueue();
            if kq < 0 {
                return Err(io::Error::last_os_error());
            }
            let kq = OwnedFd::from_raw_fd(kq);
            let ev = libc::kevent {
                ident: sock as usize,
                filter: libc::EVFILT_READ,
                flags: libc::EV_ADD,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            if libc::kevent(
                kq.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(kq))
        }
    }

    /// Sleeps until the channel has something to read (a doorbell, or EOF).
    fn wait(&self) -> io::Result<()> {
        // SAFETY: an output buffer of one event.
        let mut out: libc::kevent = unsafe { std::mem::zeroed() };
        loop {
            let n = unsafe {
                libc::kevent(
                    self.0.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut out,
                    1,
                    std::ptr::null(),
                )
            };
            if n >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}
