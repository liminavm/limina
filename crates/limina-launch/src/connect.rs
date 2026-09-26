// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Reach a listener in the worker without a filesystem path.
//!
//! The worker serves protocols the supervisor connects to more than once: display control (one
//! connection per command), balloon control (the policy's long-lived connection plus probes), and
//! the FIDO and fingerprint gadgets (again after every relaunch). A listener bound at a `$TMPDIR`
//! path takes a connection from any process of the same user, and for FIDO and the fingerprint
//! reader that is the front door of the passkey store and of a Touch-ID-gated protocol.
//!
//! So each listener gets a **link** instead: a socketpair the supervisor makes per spawn
//! ([`Connector::arm`]), whose other end the worker inherits like any other channel. Connecting
//! makes a fresh stream pair and sends one end down the link with `SCM_RIGHTS`
//! ([`Connector::connect`]); accepting is receiving it ([`Listener::accept`]). Both sides still
//! hold a plain `UnixStream`, so no protocol changes, and nothing but the two processes can reach
//! either end. When the worker is relaunched the next [`Connector::arm`] replaces the link, and a
//! connect to the old one fails the way a connect to a dead worker's path did.
//!
//! **The accept is acknowledged.** A socket whose only reference is a message in flight does not
//! survive there: once its sender closes its own copy, it arrives already at EOF unless it is
//! received within moments, even with its peer still open (`spikes/scm-rights-inflight/`). So
//! [`Connector::connect`] keeps its copy until the worker writes one byte back for it, and gives up
//! after [`ACCEPT_TIMEOUT`] the way a connect to an unbound path fails at once.
//!
//! An explicit path (the test harness's, another tool's) still works on both sides:
//! [`Endpoint::Path`] and [`ListenAt::Path`].

use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long [`Connector::connect`] waits for the worker to take a connection. Its listeners
/// accept on dedicated threads, so a live worker answers in well under a millisecond; a worker
/// still starting up is reported as not there yet, and every caller already retries that.
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(1);

/// Writing to a peer that has gone must fail with `EPIPE`, not raise SIGPIPE (macOS has no
/// `MSG_NOSIGNAL`).
fn no_sigpipe(s: &UnixStream) {
    let on: libc::c_int = 1;
    // SAFETY: setsockopt on a socket we own, with a correctly sized int.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Control buffer for one fd, aligned for `cmsghdr`.
#[repr(C, align(8))]
struct OneFdControl([u8; 32]);

fn control_space() -> usize {
    // SAFETY: pure arithmetic.
    unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as usize }
}

/// Send `fd` down `link` with one byte of payload.
fn send_fd(link: &UnixStream, fd: BorrowedFd<'_>) -> io::Result<()> {
    let byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let mut control = OneFdControl([0; 32]);
    let space = control_space();
    debug_assert!(space <= control.0.len());
    // SAFETY: a zeroed msghdr is valid; every pointer set below outlives the sendmsg call, and
    // the control buffer holds CMSG_SPACE(int) bytes with cmsghdr alignment.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.0.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as libc::socklen_t;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32);
        *(libc::CMSG_DATA(cmsg) as *mut libc::c_int) = fd.as_raw_fd();
        loop {
            match libc::sendmsg(link.as_raw_fd(), &msg, 0) {
                1 => return Ok(()),
                n if n < 0 => {
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                }
                _ => return Err(io::Error::other("short write on the link")),
            }
        }
    }
}

/// The next fd sent down `link`, close-on-exec. `Ok(None)` once the sender's end is closed.
fn recv_fd(link: &UnixStream) -> io::Result<Option<OwnedFd>> {
    loop {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr() as *mut libc::c_void,
            iov_len: 1,
        };
        let mut control = OneFdControl([0; 32]);
        // SAFETY: as in `send_fd`; the kernel writes at most `msg_controllen` control bytes.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.0.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = control_space() as libc::socklen_t;
            let n = libc::recvmsg(link.as_raw_fd(), &mut msg, 0);
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Ok(None);
            }
            let mut received = None;
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(cmsg) as *const libc::c_int;
                    let count = ((*cmsg).cmsg_len as usize - (data as usize - cmsg as usize))
                        / std::mem::size_of::<libc::c_int>();
                    for i in 0..count {
                        // Own every fd that arrived, so extras close instead of leaking.
                        let fd = OwnedFd::from_raw_fd(*data.add(i));
                        if received.is_none() {
                            received = Some(fd);
                        }
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
            if msg.msg_flags & libc::MSG_CTRUNC != 0 {
                return Err(io::Error::other(
                    "a connection on the link lost its descriptor",
                ));
            }
            // A byte without a descriptor is not a connection; wait for the next one.
            if let Some(fd) = received {
                let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFD);
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC);
                return Ok(Some(fd));
            }
        }
    }
}

/// The supervisor's end of one worker's link, and how many connections it has sent down it and
/// seen accepted.
struct Link {
    stream: UnixStream,
    sent: u64,
    accepted: u64,
}

impl Link {
    /// Read acknowledgements until connection `n` has been accepted, or `deadline` passes.
    fn wait_accepted(&mut self, n: u64, deadline: Instant) -> io::Result<()> {
        while self.accepted < n {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the worker did not accept the connection",
                ));
            }
            let mut pfd = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
            if ready < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if ready == 0 {
                continue;
            }
            let mut acks = [0u8; 64];
            match (&self.stream).read(&mut acks) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the worker closed its link",
                    ));
                }
                // Acks for connections that timed out earlier land here too, and count.
                Ok(k) => self.accepted += k as u64,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// The supervisor's side of one listener's link. Cloning shares it: every clone connects to
/// whichever worker was armed last.
#[derive(Clone, Default)]
pub struct Connector(Arc<Mutex<Option<Link>>>);

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the worker's link")
    }
}

impl Connector {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new link for the worker about to be spawned. Our end replaces the previous worker's;
    /// the returned end is for the worker to inherit (close-on-exec, like every fd the spawn
    /// passes on).
    pub fn arm(&self) -> io::Result<OwnedFd> {
        let (ours, theirs) = UnixStream::pair()?;
        no_sigpipe(&ours);
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(Link {
            stream: ours,
            sent: 0,
            accepted: 0,
        });
        Ok(theirs.into())
    }

    /// Open a connection to the worker's listener. Fails with `NotConnected` before the first
    /// spawn, with `EPIPE` once that worker is gone, and with `TimedOut` when it does not accept
    /// within [`ACCEPT_TIMEOUT`].
    pub fn connect(&self) -> io::Result<UnixStream> {
        let mut link = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let link = link.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "no worker has been started")
        })?;
        let (ours, theirs) = UnixStream::pair()?;
        no_sigpipe(&ours);
        send_fd(&link.stream, theirs.as_fd())?;
        link.sent += 1;
        let n = link.sent;
        link.wait_accepted(n, Instant::now() + ACCEPT_TIMEOUT)?;
        // Only now may our copy go: the worker holds its own, so a close on either side is the
        // other's EOF as with any stream.
        drop(theirs);
        Ok(ours)
    }
}

/// Where the supervisor reaches one of the worker's listeners.
#[derive(Clone, Debug)]
pub enum Endpoint {
    /// A path the worker binds (given explicitly, for the test harness or another tool).
    Path(PathBuf),
    /// The link the supervisor hands each worker it spawns.
    Link(Connector),
}

impl Endpoint {
    pub fn connect(&self) -> io::Result<UnixStream> {
        match self {
            Endpoint::Path(p) => UnixStream::connect(p),
            Endpoint::Link(c) => c.connect(),
        }
    }
}

impl From<PathBuf> for Endpoint {
    fn from(p: PathBuf) -> Self {
        Endpoint::Path(p)
    }
}

/// Where the worker takes connections for one of its listeners.
#[derive(Clone, Debug, PartialEq)]
pub enum ListenAt {
    /// Bind this path.
    Path(PathBuf),
    /// Accept over the link the supervisor placed at this fd.
    Link(RawFd),
}

impl fmt::Display for ListenAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListenAt::Path(p) => write!(f, "{p:?}"),
            ListenAt::Link(fd) => write!(f, "the supervisor's link (fd {fd})"),
        }
    }
}

impl ListenAt {
    /// Start listening. A stale socket left at the path by an earlier run is removed first. The
    /// link's fd is taken over, so a `Link` must be listened on once.
    pub fn listen(self) -> io::Result<Listener> {
        match self {
            ListenAt::Path(p) => {
                let _ = std::fs::remove_file(&p);
                Ok(Listener::Path(UnixListener::bind(&p)?))
            }
            ListenAt::Link(fd) => {
                // SAFETY: fcntl only checks that the descriptor is open.
                if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: the fd the supervisor placed for this listener, which nothing else in
                // this process uses; we own it from here on.
                let link = unsafe { UnixStream::from_raw_fd(fd) };
                // SAFETY: setting close-on-exec on the descriptor we now own.
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
                Ok(Listener::Link(link))
            }
        }
    }
}

/// A listener in the worker, bound at a path or taking connections over the link.
pub enum Listener {
    Path(UnixListener),
    Link(UnixStream),
}

impl Listener {
    /// The next connection. `None` once no more can come: the supervisor closed the link.
    pub fn accept(&self) -> Option<io::Result<UnixStream>> {
        match self {
            Listener::Path(l) => Some(l.accept().map(|(s, _)| s)),
            Listener::Link(link) => match recv_fd(link) {
                Ok(Some(fd)) => {
                    let s = UnixStream::from(fd);
                    no_sigpipe(&s);
                    // Tell the supervisor we hold it (see the module docs). If the link is gone
                    // the supervisor is too, and the next accept says so.
                    let _ = (&*link).write_all(&[0]);
                    Some(Ok(s))
                }
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            },
        }
    }

    /// Connections until the link closes; a path listener never ends.
    pub fn incoming(&self) -> impl Iterator<Item = io::Result<UnixStream>> + '_ {
        std::iter::from_fn(|| self.accept())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Arm `c` and serve its worker side on a thread: every accepted stream goes to the channel.
    fn worker_side(c: &Connector) -> mpsc::Receiver<UnixStream> {
        let fd = c.arm().unwrap();
        let (tx, rx) = mpsc::channel();
        let listener = ListenAt::Link(fd.as_raw_fd()).listen().unwrap();
        std::mem::forget(fd);
        std::thread::spawn(move || {
            for s in listener.incoming() {
                if tx.send(s.unwrap()).is_err() {
                    break;
                }
            }
        });
        rx
    }

    #[test]
    fn a_connection_crosses_the_link_both_ways() {
        let c = Connector::new();
        let accepted = worker_side(&c);
        let mut sup = c.connect().unwrap();
        let mut worker = accepted.recv().unwrap();
        sup.write_all(b"target 4096\n").unwrap();
        let mut got = [0u8; 12];
        worker.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"target 4096\n");
        worker.write_all(b"ok").unwrap();
        let mut back = [0u8; 2];
        sup.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"ok");
    }

    /// The display sender's shape: connect, write, close, all before the worker reads. The bytes
    /// and the EOF must both arrive, however late the read.
    #[test]
    fn connections_written_and_closed_before_the_read_still_deliver() {
        let c = Connector::new();
        let accepted = worker_side(&c);
        for line in ["a", "b"] {
            let mut s = c.connect().unwrap();
            s.write_all(line.as_bytes()).unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        let mut got = Vec::new();
        for _ in 0..2 {
            let mut text = String::new();
            accepted.recv().unwrap().read_to_string(&mut text).unwrap();
            got.push(text);
        }
        assert_eq!(got, ["a", "b"]);
    }

    #[test]
    fn the_listener_ends_when_the_supervisor_goes() {
        let c = Connector::new();
        let fd = c.arm().unwrap();
        let listener = ListenAt::Link(fd.as_raw_fd()).listen().unwrap();
        std::mem::forget(fd);
        drop(c);
        assert!(listener.accept().is_none());
    }

    #[test]
    fn a_worker_that_never_accepts_times_out() {
        let c = Connector::new();
        let fd = c.arm().unwrap();
        let t0 = Instant::now();
        assert_eq!(c.connect().unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(t0.elapsed() >= ACCEPT_TIMEOUT);
        drop(fd);
    }

    #[test]
    fn connecting_to_a_dead_worker_fails_and_rearming_recovers() {
        let c = Connector::new();
        assert_eq!(c.connect().unwrap_err().kind(), io::ErrorKind::NotConnected);
        drop(c.arm().unwrap());
        assert!(c.connect().is_err(), "the old worker's link is closed");
        let accepted = worker_side(&c);
        let mut s = c.connect().unwrap();
        s.write_all(b"x").unwrap();
        let mut got = [0u8; 1];
        accepted.recv().unwrap().read_exact(&mut got).unwrap();
        assert_eq!(&got, b"x");
    }

    #[test]
    fn a_path_endpoint_still_reaches_a_path_listener() {
        let path = std::env::temp_dir().join(format!("limina-connect-{}.sock", std::process::id()));
        let listener = ListenAt::Path(path.clone()).listen().unwrap();
        let mut s = Endpoint::Path(path.clone()).connect().unwrap();
        s.write_all(b"y").unwrap();
        let mut got = [0u8; 1];
        listener
            .accept()
            .unwrap()
            .unwrap()
            .read_exact(&mut got)
            .unwrap();
        assert_eq!(&got, b"y");
        let _ = std::fs::remove_file(&path);
    }
}
