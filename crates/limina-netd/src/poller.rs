//! The one kqueue netd waits on: the worker's channel, every UDP flow's socket, every DNS
//! question's and every TCP connection's. A closed descriptor leaves the kqueue by itself.
//!
//! The channel, the flows and the questions are watched for reading for as long as they live. A
//! TCP socket is armed one event at a time (`arm`), because both of its filters are
//! level-triggered: left armed, a socket with room to write, or with bytes the guest has no room
//! for yet, would wake the loop on every pass.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Read,
    Write,
}

/// What woke the loop.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub fd: RawFd,
    pub filter: Filter,
    /// For a socket: the bytes readable (`Read`), or the room in its send buffer (`Write`).
    pub data: usize,
}

pub struct Poller(OwnedFd);

impl Poller {
    pub fn new() -> io::Result<Self> {
        // SAFETY: a plain syscall; the result is ours.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just created.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(kq) }))
    }

    fn change(&self, fd: RawFd, filter: i16, flags: u16) -> io::Result<()> {
        let ev = libc::kevent {
            ident: fd as usize,
            filter,
            flags,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one change, no output.
        let rc = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Watches `fd` for readability (or EOF) for as long as it is open.
    pub fn add(&self, fd: RawFd) -> io::Result<()> {
        self.change(fd, libc::EVFILT_READ, libc::EV_ADD)
    }

    /// Wakes the loop once, the next time `fd` is ready for `filter`.
    pub fn arm(&self, fd: RawFd, filter: Filter) -> io::Result<()> {
        let filter = match filter {
            Filter::Read => libc::EVFILT_READ,
            Filter::Write => libc::EVFILT_WRITE,
        };
        self.change(fd, filter, libc::EV_ADD | libc::EV_ONESHOT)
    }

    /// The events that arrived, waiting at most `timeout` (`None`: until one does).
    pub fn wait(&self, timeout: Option<Duration>, out: &mut Vec<Event>) -> io::Result<()> {
        out.clear();
        let ts = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs() as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });
        // SAFETY: an all-zero kevent is a valid output slot.
        let mut events: [libc::kevent; 64] = unsafe { std::mem::zeroed() };
        // SAFETY: output into a local array of its stated length.
        let n = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                ts.as_ref().map_or(std::ptr::null(), |t| t as *const _),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(e)
            };
        }
        out.extend(events[..n as usize].iter().filter_map(|e| {
            let filter = match e.filter {
                libc::EVFILT_READ => Filter::Read,
                libc::EVFILT_WRITE => Filter::Write,
                _ => return None,
            };
            Some(Event {
                fd: e.ident as RawFd,
                filter,
                data: e.data.max(0) as usize,
            })
        }));
        Ok(())
    }
}
