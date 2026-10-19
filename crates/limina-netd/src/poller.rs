//! The one kqueue netd waits on: the worker's channel, every UDP flow's socket and every DNS
//! query's. A closed descriptor leaves the kqueue by itself.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

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

    /// Watches `fd` for readability (or EOF).
    pub fn add(&self, fd: RawFd) -> io::Result<()> {
        let ev = libc::kevent {
            ident: fd as usize,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD,
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

    /// The descriptors that became readable, waiting at most `timeout` (`None`: until one does).
    pub fn wait(&self, timeout: Option<Duration>, out: &mut Vec<RawFd>) -> io::Result<()> {
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
        out.extend(events[..n as usize].iter().map(|e| e.ident as RawFd));
        Ok(())
    }
}
