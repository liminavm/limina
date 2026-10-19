//! TCP from the guest, translated onto host sockets (`docs/design/multi-vm-networking.md` §3.2).
//!
//! netd is not a TCP endpoint of its own: each guest connection is a host socket, and netd speaks
//! just enough TCP on the guest's side to carry the bytes. The guest's SYN is answered only once
//! the host's connect has (refused: RST; unanswered: silence, and the guest retries).
//!
//! - **Guest to host:** the guest's bytes are acknowledged as they land in a bounded buffer, and
//!   the window netd advertises is that buffer's free space; they are written to the host socket
//!   in pieces sized to the room its write event reports (`spikes/netd-tcp-translate`: a larger
//!   non-blocking write mostly fails outright).
//! - **Host to guest, read-and-retain:** what netd reads from the host socket stays in a bounded
//!   buffer until the guest acknowledges it, so it can be sent again: after a retransmission
//!   timeout (go-back-N), or for three duplicate ACKs (the one segment). A full buffer stops the
//!   reads, and the host's own flow control takes over from there.
//!
//! A port forward runs the same machine the other way round: netd accepts on the forward's
//! listening socket, and opens the connection to the guest from the gateway's address, its SYN
//! first.
//!
//! Everything a connection owes the guest (its SYN-ACK, data within the guest's window, its FIN, an
//! ACK) goes out in `flush`, which may be called again and again: a segment the ring has no room
//! for is simply not counted as sent, and goes on the next pass.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use smoltcp::wire::{TcpControl, TcpRepr, TcpSeqNumber};

use crate::poller::{Event, Filter, Poller};

/// The largest segment netd sends the guest: its 1500-byte MTU less the IPv4 and TCP headers.
const MSS: usize = 1460;
/// What netd holds of the host's bytes until the guest acknowledges them.
const RETAIN: usize = 256 * 1024;
/// What netd holds of the guest's bytes until the host socket takes them.
const TO_HOST: usize = 256 * 1024;
/// The window scale netd asks for, so `TO_HOST` fits in the 16-bit window field.
const RCV_SCALE: u8 = 3;
/// The first retransmission timeout. RFC 6298's one second is for a network: the guest is a
/// shared-memory hop away, and what it has not acknowledged in this long was lost.
const RTO_MIN: Duration = Duration::from_millis(200);
const RTO_MAX: Duration = Duration::from_secs(5);
/// Retransmissions without progress before the connection is reset.
const MAX_RETRIES: u32 = 10;
/// How long a connection one side has finished lingers without any traffic.
const CLOSING_IDLE: Duration = Duration::from_secs(120);
/// The most connections at once; a SYN beyond them is reset.
pub const MAX_CONNS: usize = 4096;

/// A connection, by the guest's port and the far end's address as the guest sees it.
pub type Key = (u16, SocketAddrV4);

/// Hands a segment for the guest to the link: its header (with no payload of its own) and its
/// payload in pieces. Whether the ring took it.
pub type Emit<'a> = dyn FnMut(SocketAddrV4, &TcpRepr<'_>, &[&[u8]]) -> bool + 'a;

/// How far `a` is past `b` in sequence space, or 0 if it is not.
fn ahead(a: TcpSeqNumber, b: TcpSeqNumber) -> usize {
    a.0.wrapping_sub(b.0).max(0) as usize
}

/// A fixed-size byte FIFO with room to read into and data to write from, each as up to two
/// slices.
struct Bytes {
    buf: Box<[u8]>,
    start: usize,
    len: usize,
}

impl Bytes {
    fn new(cap: usize) -> Self {
        Self {
            buf: vec![0; cap].into_boxed_slice(),
            start: 0,
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    /// The `n` bytes from `offset`.
    fn slices(&self, offset: usize, n: usize) -> [&[u8]; 2] {
        debug_assert!(offset + n <= self.len);
        let cap = self.buf.len();
        let at = (self.start + offset) % cap;
        let first = n.min(cap - at);
        [&self.buf[at..at + first], &self.buf[..n - first]]
    }

    /// The free space, in order.
    fn spare(&mut self) -> [&mut [u8]; 2] {
        let cap = self.buf.len();
        let end = (self.start + self.len) % cap;
        let free = cap - self.len;
        let first = free.min(cap - end);
        let (head, tail) = self.buf.split_at_mut(end);
        let (a, _) = tail.split_at_mut(first);
        let (b, _) = head.split_at_mut(free - first);
        [a, b]
    }

    fn commit(&mut self, n: usize) {
        debug_assert!(n <= self.free());
        self.len += n;
    }

    fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.len);
        self.start = (self.start + n) % self.buf.len();
        self.len -= n;
    }

    fn push(&mut self, mut data: &[u8]) {
        debug_assert!(data.len() <= self.free());
        let n = data.len();
        for s in self.spare() {
            let k = s.len().min(data.len());
            s[..k].copy_from_slice(&data[..k]);
            data = &data[k..];
        }
        self.commit(n);
    }
}

fn iovecs<const N: usize>(slices: [&[u8]; N]) -> [libc::iovec; N] {
    slices.map(|s| libc::iovec {
        iov_base: s.as_ptr() as *mut _,
        iov_len: s.len(),
    })
}

/// The result of a non-blocking `readv`/`writev`.
fn io_result(n: isize) -> io::Result<usize> {
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn sock_error(fd: RawFd) -> i32 {
    let mut err: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: an int-sized option into an int.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut err as *mut _ as *mut _,
            &mut len,
        )
    };
    if rc < 0 {
        io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)
    } else {
        err
    }
}

fn is_connected(fd: RawFd) -> bool {
    // SAFETY: an all-zero sockaddr_in is a valid output buffer.
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
    // SAFETY: output into a local of the stated size.
    unsafe { libc::getpeername(fd, &mut addr as *mut _ as *mut _, &mut len) == 0 }
}

fn set_int_opt(fd: RawFd, level: i32, name: i32, value: libc::c_int) -> io::Result<()> {
    // SAFETY: an int-sized option from an int.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            &value as *const _ as *const _,
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Readies a host TCP socket for netd: close-on-exec, non-blocking, and without delaying small
/// writes (the guest's own stack decided when to send them).
fn prepare(fd: RawFd) -> io::Result<()> {
    // SAFETY: flag changes on our own descriptor.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // A peer's reset must surface as EPIPE, not kill netd.
    set_int_opt(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1)?;
    set_int_opt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, 1)
}

/// A non-blocking TCP socket connecting to `dst`, and whether it already has.
fn connect(dst: SocketAddrV4) -> io::Result<(OwnedFd, bool)> {
    // SAFETY: a plain syscall; the result is ours.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    prepare(fd)?;
    let addr = libc::sockaddr_in {
        sin_len: size_of::<libc::sockaddr_in>() as u8,
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: dst.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from(*dst.ip()).to_be(),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: a sockaddr_in of its stated size.
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const _,
            size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        return Ok((sock, true));
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EINPROGRESS) {
        Ok((sock, false))
    } else {
        Err(e)
    }
}

/// Closes `sock` with a reset to its peer rather than a FIN.
fn reset(sock: OwnedFd) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: a linger struct of its stated size.
    unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const _ as *const _,
            size_of::<libc::linger>() as libc::socklen_t,
        );
    }
}

/// The reset answering `seg` when no connection takes it (RFC 9293 §3.10.7.1).
fn reset_for(seg: &TcpRepr<'_>) -> TcpRepr<'static> {
    let (seq_number, ack_number) = match seg.ack_number {
        Some(ack) => (ack, None),
        None => (TcpSeqNumber(0), Some(seg.seq_number + seg.segment_len())),
    };
    TcpRepr {
        src_port: seg.dst_port,
        dst_port: seg.src_port,
        control: TcpControl::Rst,
        seq_number,
        ack_number,
        window_len: 0,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload: &[],
    }
}

struct Conn {
    sock: OwnedFd,
    /// The host's connect has not answered yet.
    connecting: bool,
    /// The guest's side of the sequence space is known: always for a connection the guest
    /// opened, and from its SYN-ACK for one netd opened.
    synced: bool,
    /// The window scale netd's SYN offers.
    offer_scale: Option<u8>,

    /// The guest's initial sequence number, and the next it is to send.
    irs: TcpSeqNumber,
    rcv_nxt: TcpSeqNumber,
    /// netd's initial sequence number; the oldest it has unacknowledged; the next it sends; the
    /// furthest it has sent.
    iss: TcpSeqNumber,
    snd_una: TcpSeqNumber,
    snd_nxt: TcpSeqNumber,
    snd_max: TcpSeqNumber,
    /// The guest's window, in bytes, and the shift its window field takes.
    snd_wnd: usize,
    snd_scale: u8,
    /// The shift netd's window field takes, if the guest agreed to scaling.
    rcv_scale: Option<u8>,
    /// The largest segment the guest takes.
    mss: usize,

    /// The host's bytes not yet acknowledged by the guest, from `retain_seq`.
    retain: Bytes,
    retain_seq: TcpSeqNumber,
    /// The guest's bytes not yet written to the host.
    to_host: Bytes,
    /// The host's side has sent its last byte (read returned 0).
    host_eof: bool,
    /// The guest's side has sent its FIN, which netd has passed on (or will, once `to_host` is
    /// written).
    guest_fin: bool,
    host_shut: bool,

    ack_owed: bool,
    /// The window last advertised, in bytes.
    adv_wnd: usize,
    rto_at: Option<Instant>,
    retries: u32,
    dupacks: u32,
    /// Resend the segment at `snd_una` (three duplicate ACKs, or a partial ACK in recovery).
    resend_one: bool,
    /// In loss recovery until the guest acknowledges this: `snd_max` when it began (NewReno, RFC
    /// 6582), so each further hole is resent as the ACK reaches it rather than on a timeout.
    recover: Option<TcpSeqNumber>,
    /// Send one byte past a zero window, so the guest's ACK says when it opens.
    probe: bool,
    read_armed: bool,
    write_armed: bool,
    last: Instant,
}

impl Conn {
    fn syn_acked(&self) -> bool {
        self.snd_una != self.iss
    }

    /// Where netd's FIN goes in sequence space, once the host's side has ended.
    fn fin_seq(&self) -> Option<TcpSeqNumber> {
        self.host_eof.then(|| self.retain_seq + self.retain.len())
    }

    fn done(&self) -> bool {
        self.guest_fin && self.host_shut && self.fin_seq().is_some_and(|f| self.snd_una == f + 1)
    }

    fn window(&self) -> usize {
        match self.rcv_scale {
            Some(s) => (self.to_host.free() >> s).min(u16::MAX as usize) << s,
            None => self.to_host.free().min(u16::MAX as usize),
        }
    }

    fn rto(&self) -> Duration {
        RTO_MIN
            .saturating_mul(1 << self.retries.min(16))
            .min(RTO_MAX)
    }

    fn header(&mut self, key: Key, control: TcpControl, seq: TcpSeqNumber) -> TcpRepr<'static> {
        let wnd = self.window();
        self.adv_wnd = wnd;
        TcpRepr {
            src_port: key.1.port(),
            dst_port: key.0,
            control,
            seq_number: seq,
            ack_number: self.synced.then_some(self.rcv_nxt),
            window_len: (wnd >> self.rcv_scale.unwrap_or(0)) as u16,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 3],
            timestamp: None,
            payload: &[],
        }
    }

    /// Sends `n` retained bytes from `seq`.
    fn send_data(&mut self, key: Key, seq: TcpSeqNumber, n: usize, emit: &mut Emit<'_>) -> bool {
        let offset = ahead(seq, self.retain_seq);
        let control = if offset + n == self.retain.len() {
            TcpControl::Psh
        } else {
            TcpControl::None
        };
        let header = self.header(key, control, seq);
        let [a, b] = self.retain.slices(offset, n);
        emit(key.1, &header, &[a, b])
    }

    /// Sends whatever the connection owes the guest; false when the ring filled first.
    fn transmit(&mut self, key: Key, now: Instant, emit: &mut Emit<'_>) -> bool {
        if self.connecting {
            return true;
        }
        if self.snd_nxt == self.iss {
            let mut syn = self.header(key, TcpControl::Syn, self.iss);
            // The window in a SYN is never scaled.
            syn.window_len = self.to_host.free().min(u16::MAX as usize) as u16;
            syn.window_scale = self.offer_scale;
            syn.max_seg_size = Some(MSS as u16);
            if !emit(key.1, &syn, &[]) {
                return false;
            }
            self.snd_nxt = self.iss + 1;
            self.snd_max = self.snd_max.max(self.snd_nxt);
            self.ack_owed = false;
        }
        if self.syn_acked() {
            if self.resend_one && self.snd_una != self.snd_nxt {
                let avail = self.retain.len() - ahead(self.snd_una, self.retain_seq);
                let n = avail.min(self.mss);
                if n > 0 {
                    if !self.send_data(key, self.snd_una, n, emit) {
                        return false;
                    }
                    self.ack_owed = false;
                }
            }
            self.resend_one = false;
            loop {
                let offset = ahead(self.snd_nxt, self.retain_seq);
                let avail = self.retain.len().saturating_sub(offset);
                let in_flight = ahead(self.snd_nxt, self.snd_una);
                let mut n = avail
                    .min(self.mss)
                    .min(self.snd_wnd.saturating_sub(in_flight));
                if n == 0 && self.probe && avail > 0 {
                    n = 1;
                }
                if n == 0 {
                    break;
                }
                if !self.send_data(key, self.snd_nxt, n, emit) {
                    return false;
                }
                self.probe = false;
                self.snd_nxt += n;
                self.snd_max = self.snd_max.max(self.snd_nxt);
                self.ack_owed = false;
            }
            if let Some(fin) = self.fin_seq()
                && self.snd_nxt == fin
            {
                let header = self.header(key, TcpControl::Fin, fin);
                if !emit(key.1, &header, &[]) {
                    return false;
                }
                self.snd_nxt = fin + 1;
                self.snd_max = self.snd_max.max(self.snd_nxt);
                self.ack_owed = false;
            }
        }
        let waiting = ahead(self.snd_nxt, self.retain_seq) < self.retain.len();
        if self.rto_at.is_none() && (self.snd_nxt != self.snd_una || (waiting && self.snd_wnd == 0))
        {
            self.rto_at = Some(now + self.rto());
        }
        if self.ack_owed {
            let header = self.header(key, TcpControl::None, self.snd_nxt);
            if !emit(key.1, &header, &[]) {
                return false;
            }
            self.ack_owed = false;
        }
        true
    }

    /// The guest's answer to netd's SYN.
    fn syn_acked_by_guest(&mut self, seg: &TcpRepr<'_>, now: Instant) {
        self.synced = true;
        self.irs = seg.seq_number;
        self.rcv_nxt = seg.seq_number + 1;
        self.mss = seg.max_seg_size.map_or(536, |m| m as usize).min(MSS);
        if let Some(scale) = seg.window_scale {
            self.snd_scale = scale;
            self.rcv_scale = self.offer_scale;
        }
        self.segment(seg, now);
        // The window in a SYN is never scaled.
        self.snd_wnd = seg.window_len as usize;
        self.ack_owed = true;
    }

    /// A segment from the guest on this connection (not a SYN, not a RST).
    fn segment(&mut self, seg: &TcpRepr<'_>, now: Instant) {
        if let Some(ack) = seg.ack_number
            && ack >= self.snd_una
            && ack <= self.snd_max
        {
            let wnd = (seg.window_len as usize) << self.snd_scale;
            if ack > self.snd_una {
                if ack > self.retain_seq {
                    let n = ahead(ack, self.retain_seq).min(self.retain.len());
                    self.retain.consume(n);
                    self.retain_seq += n;
                }
                self.snd_una = ack;
                if self.snd_nxt < ack {
                    self.snd_nxt = ack;
                }
                self.retries = 0;
                self.dupacks = 0;
                self.rto_at = None;
                match self.recover {
                    Some(r) if ack < r => self.resend_one = true,
                    _ => self.recover = None,
                }
            } else if seg.payload.is_empty()
                && seg.control != TcpControl::Fin
                && self.snd_nxt != self.snd_una
            {
                // Every third duplicate resends, in case the resend was lost too.
                self.dupacks += 1;
                if self.dupacks.is_multiple_of(3) {
                    self.resend_one = true;
                    self.recover.get_or_insert(self.snd_max);
                }
            }
            if wnd > self.snd_wnd {
                // An opening window ends a persist wait.
                self.rto_at = self.rto_at.filter(|_| self.snd_nxt != self.snd_una);
            }
            self.snd_wnd = wnd;
        }

        let fin = seg.control == TcpControl::Fin;
        if seg.payload.is_empty() && !fin {
            return;
        }
        self.ack_owed = true;
        if self.guest_fin {
            return;
        }
        let mut data = seg.payload;
        let mut seq = seg.seq_number;
        if self.rcv_nxt > seq {
            let dup = ahead(self.rcv_nxt, seq);
            if dup > data.len() {
                return;
            }
            data = &data[dup..];
            seq = self.rcv_nxt;
        }
        if seq != self.rcv_nxt {
            // Out of order: the duplicate ACK brings the missing piece.
            return;
        }
        let n = data.len().min(self.to_host.free());
        self.to_host.push(&data[..n]);
        self.rcv_nxt += n;
        if fin && n == data.len() {
            self.guest_fin = true;
            self.rcv_nxt += 1;
        }
        self.last = now;
    }

    /// The host socket has room to write `room` bytes.
    fn writable(&mut self, room: usize) -> io::Result<()> {
        let n = room.min(self.to_host.len());
        if n > 0 {
            let iov = iovecs(self.to_host.slices(0, n));
            // SAFETY: two iovecs over live slices of our buffer.
            let r = io_result(unsafe { libc::writev(self.sock.as_raw_fd(), iov.as_ptr(), 2) });
            match r {
                Ok(w) => {
                    let before = self.window();
                    self.to_host.consume(w);
                    let after = self.window();
                    // Tell the guest of a window it had all but run out of, or one that has
                    // grown by a quarter of the buffer since it last heard.
                    if (self.adv_wnd < self.mss && after >= self.mss)
                        || after >= self.adv_wnd + TO_HOST / 4 && after > before
                    {
                        self.ack_owed = true;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Passes the guest's FIN on once everything before it has been written.
    fn shut_host(&mut self) {
        if self.guest_fin && self.to_host.len() == 0 && !self.host_shut {
            // SAFETY: on our own socket.
            unsafe { libc::shutdown(self.sock.as_raw_fd(), libc::SHUT_WR) };
            self.host_shut = true;
        }
    }

    /// The host socket has bytes (or its EOF) to read.
    fn readable(&mut self) -> io::Result<()> {
        while self.retain.free() > 0 {
            let fd = self.sock.as_raw_fd();
            let [a, b] = self.retain.spare();
            let iov = iovecs([&*a, &*b]);
            // SAFETY: two iovecs over the spare room of our buffer.
            match io_result(unsafe { libc::readv(fd, iov.as_ptr(), 2) }) {
                Ok(0) => {
                    self.host_eof = true;
                    return Ok(());
                }
                Ok(n) => self.retain.commit(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct TcpNat {
    conns: HashMap<Key, Conn>,
    by_fd: HashMap<RawFd, Key>,
    /// The connections that may owe the guest a segment or the poller an arming.
    dirty: HashSet<Key>,
    /// The gateway port the last forwarded connection was given.
    next_port: u16,
}

impl TcpNat {
    pub fn owns(&self, fd: RawFd) -> bool {
        self.by_fd.contains_key(&fd)
    }

    /// A segment from the guest. A SYN to an `allowed` destination opens a host connection;
    /// what has no connection is reset.
    pub fn segment(&mut self, key: Key, seg: &TcpRepr<'_>, allowed: bool, emit: &mut Emit<'_>) {
        let now = Instant::now();
        if let Some(c) = self.conns.get_mut(&key) {
            c.last = now;
            match seg.control {
                TcpControl::Rst => {
                    log::debug!("TCP {} → {}: reset by the guest", key.0, key.1);
                    self.remove(key, true);
                    return;
                }
                TcpControl::Syn if seg.ack_number.is_some() => {
                    if !c.synced && seg.ack_number == Some(c.iss + 1) {
                        log::debug!("TCP: {} → guest port {} connected", key.1, key.0);
                        c.syn_acked_by_guest(seg, now);
                        self.dirty.insert(key);
                    } else if c.synced && seg.seq_number == c.irs {
                        // Its SYN-ACK again: netd's ACK was lost.
                        c.ack_owed = true;
                        self.dirty.insert(key);
                    }
                    return;
                }
                _ if !c.synced => return,
                TcpControl::Syn if seg.ack_number.is_none() => {
                    if seg.seq_number == c.irs {
                        // Its SYN again: the SYN-ACK was lost (or the connect is still going).
                        if !c.syn_acked() && !c.connecting {
                            c.snd_nxt = c.iss;
                        }
                        self.dirty.insert(key);
                        return;
                    }
                    if !(c.host_eof || c.guest_fin) {
                        c.ack_owed = true;
                        self.dirty.insert(key);
                        return;
                    }
                    // A new connection on a port whose last one is closing.
                    self.remove(key, true);
                }
                _ if c.connecting => return,
                _ => {
                    c.segment(seg, now);
                    self.dirty.insert(key);
                    return;
                }
            }
        }
        match seg.control {
            TcpControl::Rst => {}
            TcpControl::Syn if seg.ack_number.is_none() => self.open(key, seg, allowed, emit),
            _ => {
                emit(key.1, &reset_for(seg), &[]);
            }
        }
    }

    fn open(&mut self, key: Key, seg: &TcpRepr<'_>, allowed: bool, emit: &mut Emit<'_>) {
        if !allowed || self.conns.len() >= MAX_CONNS {
            emit(key.1, &reset_for(seg), &[]);
            return;
        }
        let (sock, connected) = match connect(key.1) {
            Ok(s) => s,
            Err(e) => {
                log::debug!("TCP {} → {}: {e}", key.0, key.1);
                emit(key.1, &reset_for(seg), &[]);
                return;
            }
        };
        let scale = seg.window_scale.map(|_| RCV_SCALE);
        let conn = Conn {
            connecting: !connected,
            synced: true,
            offer_scale: scale,
            irs: seg.seq_number,
            rcv_nxt: seg.seq_number + 1,
            snd_wnd: seg.window_len as usize,
            snd_scale: seg.window_scale.unwrap_or(0),
            rcv_scale: scale,
            mss: seg.max_seg_size.map_or(536, |m| m as usize).min(MSS),
            ..Conn::new(sock)
        };
        self.by_fd.insert(conn.sock.as_raw_fd(), key);
        self.conns.insert(key, conn);
        self.dirty.insert(key);
    }

    /// Carries a connection accepted on a port forward to the guest's `guest_port`, from the
    /// gateway's `gateway` address. Beyond `MAX_CONNS`, or with no port left, it is closed.
    pub fn open_to_guest(&mut self, guest_port: u16, gateway: Ipv4Addr, sock: OwnedFd) {
        if self.conns.len() >= MAX_CONNS {
            return;
        }
        if let Err(e) = prepare(sock.as_raw_fd()) {
            log::debug!("TCP forward to guest port {guest_port}: {e}");
            return;
        }
        let Some(key) = self.free_key(guest_port, gateway) else {
            return;
        };
        let conn = Conn {
            synced: false,
            offer_scale: Some(RCV_SCALE),
            ..Conn::new(sock)
        };
        self.by_fd.insert(conn.sock.as_raw_fd(), key);
        self.conns.insert(key, conn);
        self.dirty.insert(key);
    }

    /// A source port on the gateway no connection to the guest's `guest_port` uses.
    fn free_key(&mut self, guest_port: u16, gateway: Ipv4Addr) -> Option<Key> {
        const FIRST: u16 = 32768;
        for _ in FIRST..=u16::MAX {
            self.next_port = self.next_port.max(FIRST).wrapping_add(1).max(FIRST);
            let key = (guest_port, SocketAddrV4::new(gateway, self.next_port));
            if !self.conns.contains_key(&key) {
                return Some(key);
            }
        }
        None
    }
}

impl Conn {
    /// A connection on `sock` with nothing yet known of the guest's side.
    fn new(sock: OwnedFd) -> Self {
        // SAFETY: arc4random has no preconditions.
        let iss = TcpSeqNumber(unsafe { libc::arc4random() } as i32);
        Self {
            sock,
            connecting: false,
            synced: false,
            offer_scale: None,
            irs: TcpSeqNumber(0),
            rcv_nxt: TcpSeqNumber(0),
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_max: iss,
            snd_wnd: 0,
            snd_scale: 0,
            rcv_scale: None,
            mss: MSS,
            retain: Bytes::new(RETAIN),
            retain_seq: iss + 1,
            to_host: Bytes::new(TO_HOST),
            host_eof: false,
            guest_fin: false,
            host_shut: false,
            ack_owed: false,
            adv_wnd: 0,
            rto_at: None,
            retries: 0,
            dupacks: 0,
            resend_one: false,
            recover: None,
            probe: false,
            read_armed: false,
            write_armed: false,
            last: Instant::now(),
        }
    }
}

impl TcpNat {
    /// Ends a connection; `abort` resets the host side rather than closing it.
    fn remove(&mut self, key: Key, abort: bool) {
        self.dirty.remove(&key);
        if let Some(c) = self.conns.remove(&key) {
            self.by_fd.remove(&c.sock.as_raw_fd());
            if abort {
                reset(c.sock);
            }
        }
    }

    /// Resets a connection on both sides.
    fn abort(&mut self, key: Key, emit: &mut Emit<'_>) {
        // Before the guest's SYN-ACK there is nothing on its side to reset.
        if let Some(c) = self.conns.get_mut(&key).filter(|c| c.synced) {
            let header = if c.connecting {
                TcpRepr {
                    ack_number: Some(c.irs + 1),
                    ..c.header(key, TcpControl::Rst, TcpSeqNumber(0))
                }
            } else {
                c.header(key, TcpControl::Rst, c.snd_nxt)
            };
            emit(key.1, &header, &[]);
        }
        self.remove(key, true);
    }

    /// A host socket's event.
    pub fn event(&mut self, ev: &Event, emit: &mut Emit<'_>) {
        let Some(&key) = self.by_fd.get(&ev.fd) else {
            return;
        };
        let Some(c) = self.conns.get_mut(&key) else {
            return;
        };
        let result = match ev.filter {
            Filter::Write => {
                c.write_armed = false;
                if c.connecting {
                    match sock_error(ev.fd) {
                        0 if is_connected(ev.fd) => {
                            log::debug!("TCP: {} → {} connected", key.0, key.1);
                            c.connecting = false;
                            Ok(())
                        }
                        0 => Ok(()),
                        e => Err(io::Error::from_raw_os_error(e)),
                    }
                } else {
                    c.writable(ev.data)
                }
            }
            Filter::Read => {
                c.read_armed = false;
                c.last = Instant::now();
                c.readable()
            }
        };
        match result {
            Ok(()) => {
                self.dirty.insert(key);
            }
            Err(e) => {
                log::debug!("TCP {} → {}: {e}", key.0, key.1);
                self.abort(key, emit);
            }
        }
    }

    /// Sends what the changed connections owe the guest, and arms their host sockets for what they
    /// wait on. False when the ring filled: the rest goes on a later pass.
    pub fn flush(&mut self, poller: &Poller, emit: &mut Emit<'_>) -> bool {
        let now = Instant::now();
        let mut all = true;
        let keys: Vec<Key> = self.dirty.drain().collect();
        for key in keys {
            let Some(c) = self.conns.get_mut(&key) else {
                continue;
            };
            if !c.transmit(key, now, emit) {
                all = false;
                self.dirty.insert(key);
            }
            c.shut_host();
            if c.done() {
                log::debug!("TCP {} → {}: closed", key.0, key.1);
                self.remove(key, false);
                continue;
            }
            let fd = c.sock.as_raw_fd();
            let mut armed = Ok(());
            let want_write = c.connecting || c.to_host.len() > 0;
            if want_write && !c.write_armed {
                armed = poller.arm(fd, Filter::Write);
                c.write_armed = armed.is_ok();
            }
            let want_read = !c.connecting && !c.host_eof && c.retain.free() > 0;
            if armed.is_ok() && want_read && !c.read_armed {
                armed = poller.arm(fd, Filter::Read);
                c.read_armed = armed.is_ok();
            }
            if let Err(e) = armed {
                log::warn!("TCP {} → {}: cannot wait on the socket: {e}", key.0, key.1);
                self.abort(key, emit);
            }
        }
        all
    }

    /// Retransmission timeouts and lingering closes.
    pub fn expire(&mut self, now: Instant, emit: &mut Emit<'_>) {
        let mut dead = Vec::new();
        for (&key, c) in &mut self.conns {
            if (c.host_eof || c.guest_fin) && now.duration_since(c.last) >= CLOSING_IDLE {
                dead.push(key);
                continue;
            }
            let Some(at) = c.rto_at else { continue };
            if at > now {
                continue;
            }
            c.rto_at = None;
            if c.snd_nxt != c.snd_una {
                c.retries += 1;
                if c.retries > MAX_RETRIES {
                    dead.push(key);
                    continue;
                }
                c.snd_nxt = c.snd_una;
                c.recover = None;
            } else {
                c.probe = true;
            }
            self.dirty.insert(key);
        }
        for key in dead {
            log::debug!("TCP {} → {}: timed out", key.0, key.1);
            self.abort(key, emit);
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.conns
            .values()
            .flat_map(|c| {
                [
                    c.rto_at,
                    (c.host_eof || c.guest_fin).then(|| c.last + CLOSING_IDLE),
                ]
            })
            .flatten()
            .min()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.conns.len()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};

    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::phy::{Device, DeviceCapabilities, Medium};
    use smoltcp::socket::tcp;
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address};

    use super::*;
    use crate::stack::{Ingress, Link, Stack};
    use limina_netd_wire::VNET_HDR_LEN;

    #[test]
    fn bytes_wrap_around() {
        let mut b = Bytes::new(8);
        b.push(b"abcdef");
        b.consume(4);
        b.push(b"ghijkl");
        assert_eq!(b.len(), 8);
        assert_eq!(b.slices(0, 8), [&b"efgh"[..], &b"ijkl"[..]]);
        assert_eq!(b.slices(3, 3), [&b"h"[..], &b"ij"[..]]);
        b.consume(5);
        let [x, y] = b.spare();
        assert_eq!(x.len() + y.len(), 5);
    }

    /// The guest's NIC, as a smoltcp device: frames it sends go to netd, frames netd makes come
    /// back to it.
    #[derive(Default)]
    struct Wire {
        to_netd: VecDeque<Vec<u8>>,
        to_guest: VecDeque<Vec<u8>>,
    }

    struct Rx(Vec<u8>);
    struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);

    impl smoltcp::phy::RxToken for Rx {
        fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
            f(&self.0[VNET_HDR_LEN..])
        }
    }

    impl smoltcp::phy::TxToken for Tx<'_> {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            let mut frame = vec![0u8; VNET_HDR_LEN + len];
            let r = f(&mut frame[VNET_HDR_LEN..]);
            self.0.push_back(frame);
            r
        }
    }

    impl Device for Wire {
        type RxToken<'a> = Rx;
        type TxToken<'a> = Tx<'a>;

        fn receive(
            &mut self,
            _: smoltcp::time::Instant,
        ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            let frame = self.to_guest.pop_front()?;
            Some((Rx(frame), Tx(&mut self.to_netd)))
        }

        fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
            Some(Tx(&mut self.to_netd))
        }

        fn capabilities(&self) -> DeviceCapabilities {
            let mut caps = DeviceCapabilities::default();
            caps.medium = Medium::Ethernet;
            caps.max_transmission_unit = 1514;
            caps
        }
    }

    /// A smoltcp guest on netd's link, talking to listeners on the host's loopback (which the
    /// real policy refuses; here every destination is allowed).
    struct Guest {
        wire: Wire,
        iface: Interface,
        sockets: SocketSet<'static>,
        stack: Stack,
        tcp: TcpNat,
        poller: Poller,
        /// Which of the frames netd sends the guest (counting from 1) are lost on the way.
        lose: fn(usize) -> bool,
        sent: usize,
    }

    impl Guest {
        fn new() -> Self {
            let link = Link::default();
            let mut wire = Wire::default();
            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress([
                0x52, 0x54, 0, 0x12, 0x34, 0x56,
            ])));
            let mut iface = Interface::new(config, &mut wire, smoltcp::time::Instant::now());
            iface.update_ip_addrs(|a| {
                a.push(IpCidr::new(link.guest.into(), 24)).unwrap();
            });
            iface
                .routes_mut()
                .add_default_ipv4_route(link.gateway)
                .unwrap();
            Self {
                wire,
                iface,
                sockets: SocketSet::new(vec![]),
                stack: Stack::new(link),
                tcp: TcpNat::default(),
                poller: Poller::new().unwrap(),
                lose: |_| false,
                sent: 0,
            }
        }

        fn connect(&mut self, port: u16) -> smoltcp::iface::SocketHandle {
            let mut s = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0; 256 * 1024]),
                tcp::SocketBuffer::new(vec![0; 256 * 1024]),
            );
            s.connect(
                self.iface.context(),
                (Ipv4Address::new(127, 0, 0, 1), port),
                49152 + port % 1000,
            )
            .unwrap();
            self.sockets.add(s)
        }

        /// One pass of netd's loop against one poll of the guest's stack.
        fn step(&mut self) {
            self.iface.poll(
                smoltcp::time::Instant::now(),
                &mut self.wire,
                &mut self.sockets,
            );
            let Self {
                wire,
                stack,
                tcp,
                poller,
                lose,
                sent,
                ..
            } = self;
            // The frames netd makes, minus the ones the test loses on purpose.
            macro_rules! emit {
                () => {
                    &mut |remote: SocketAddrV4, h: &TcpRepr<'_>, p: &[&[u8]]| {
                        stack.tcp_to_guest(remote, h, p, &mut |f| {
                            *sent += 1;
                            if !lose(*sent) {
                                wire.to_guest.push_back(f.to_vec());
                            }
                            true
                        })
                    }
                };
            }
            let frames: Vec<Vec<u8>> = std::mem::take(&mut wire.to_netd).into();
            for frame in &frames {
                let mut replies = Vec::new();
                let ingress = stack.handle(frame, &mut |f| replies.push(f.to_vec()));
                wire.to_guest.extend(replies);
                if let Some(Ingress::Tcp {
                    guest_port,
                    remote,
                    segment,
                }) = ingress
                {
                    tcp.segment((guest_port, remote), &segment, true, emit!());
                }
            }
            let mut events = Vec::new();
            poller
                .wait(Some(Duration::from_millis(1)), &mut events)
                .unwrap();
            for ev in &events {
                tcp.event(ev, emit!());
            }
            tcp.expire(Instant::now(), emit!());
            tcp.flush(poller, emit!());
        }

        fn socket(&mut self, h: smoltcp::iface::SocketHandle) -> &mut tcp::Socket<'static> {
            self.sockets.get_mut::<tcp::Socket>(h)
        }

        /// Steps until `done` holds, for at most 20 s.
        fn until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !done(self) {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                self.step();
            }
        }
    }

    fn listener() -> (TcpListener, u16) {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        (l, port)
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u32).wrapping_mul(2654435761).to_le_bytes()[0] ^ seed)
            .collect()
    }

    /// Moves `up` from the guest to the host and `down` back over one connection, then closes it
    /// from the guest's side first.
    fn round_trip(g: &mut Guest, up: &[u8], down: &[u8]) {
        let (listener, port) = listener();
        let h = g.connect(port);
        g.until("the connection", |g| g.socket(h).may_send());
        let (mut host, _) = listener.accept().unwrap();
        host.set_nonblocking(true).unwrap();

        let (mut sent_up, mut got_up) = (0, Vec::new());
        let (mut sent_down, mut got_down) = (0, Vec::new());
        let mut buf = vec![0u8; 65536];
        g.until("both directions", |g| {
            let s = g.socket(h);
            if sent_up < up.len() && s.can_send() {
                sent_up += s.send_slice(&up[sent_up..]).unwrap();
            }
            while s.can_recv() {
                let n = s.recv_slice(&mut buf).unwrap();
                got_down.extend_from_slice(&buf[..n]);
            }
            if sent_down < down.len() {
                match host.write(&down[sent_down..]) {
                    Ok(n) => sent_down += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("{e}"),
                }
            }
            loop {
                match host.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => got_up.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => panic!("{e}"),
                }
            }
            got_up.len() == up.len() && got_down.len() == down.len()
        });
        assert!(got_up == up, "the guest's bytes arrived intact");
        assert!(got_down == down, "the host's bytes arrived intact");

        // The guest closes first; the host sees EOF, then closes its side.
        g.socket(h).close();
        g.until("the host's EOF", |_| matches!(host.read(&mut buf), Ok(0)));
        drop(host);
        g.until("the guest's close", |g| {
            g.socket(h).state() == tcp::State::TimeWait || g.socket(h).state() == tcp::State::Closed
        });
        g.until("netd to forget the connection", |g| g.tcp.len() == 0);
    }

    #[test]
    fn bytes_cross_both_ways_and_the_connection_closes() {
        let mut g = Guest::new();
        round_trip(&mut g, &pattern(3 << 20, 1), &pattern(5 << 20, 2));
    }

    /// Scattered losses: the guest's duplicate ACKs bring each segment back.
    #[test]
    fn lost_segments_are_sent_again() {
        let mut g = Guest::new();
        g.lose = |n| n.is_multiple_of(50);
        round_trip(&mut g, &pattern(1 << 20, 3), &pattern(2 << 20, 4));
    }

    /// A burst of losses takes the tail of the window with it, which only the retransmission
    /// timeout recovers.
    #[test]
    fn a_lost_burst_is_sent_again_after_a_timeout() {
        let mut g = Guest::new();
        g.lose = |n| (300..500).contains(&n);
        round_trip(&mut g, &pattern(1 << 20, 5), &pattern(2 << 20, 6));
    }

    /// A port forward: netd opens the connection to a guest listening on 22, from the gateway,
    /// and the bytes and closes cross as they do the other way round.
    #[test]
    fn a_forwarded_connection_reaches_the_guest() {
        let mut g = Guest::new();
        let mut s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 256 * 1024]),
            tcp::SocketBuffer::new(vec![0; 256 * 1024]),
        );
        s.listen(22).unwrap();
        let h = g.sockets.add(s);
        // The guest has spoken (its ARP for the gateway) before anything is forwarded to it.
        let _ = g.connect(1);
        g.until("the guest to be heard", |g| g.stack.guest_known());

        let (listener, port) = listener();
        let mut client = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let gateway = g.stack.link().gateway;
        g.tcp.open_to_guest(22, gateway, OwnedFd::from(accepted));
        g.until("the guest to accept", |g| g.socket(h).may_send());
        assert_eq!(
            g.socket(h).remote_endpoint().map(|e| e.addr),
            Some(gateway.into()),
            "the connection comes from the gateway"
        );

        client.write_all(b"SSH-2.0-client\r\n").unwrap();
        let mut got = Vec::new();
        g.until("the client's bytes", |g| {
            let mut buf = [0u8; 64];
            while g.socket(h).can_recv() {
                let n = g.socket(h).recv_slice(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            got.ends_with(b"\r\n")
        });
        assert_eq!(got, b"SSH-2.0-client\r\n");
        g.socket(h).send_slice(b"SSH-2.0-guest\r\n").unwrap();
        g.socket(h).close();
        client.set_nonblocking(true).unwrap();
        let mut back = Vec::new();
        g.until("the guest's bytes and close", |_| {
            let mut buf = [0u8; 64];
            match client.read(&mut buf) {
                Ok(0) => true,
                Ok(n) => {
                    back.extend_from_slice(&buf[..n]);
                    false
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
                Err(e) => panic!("{e}"),
            }
        });
        assert_eq!(back, b"SSH-2.0-guest\r\n");
        drop(client);
        g.until("netd to forget the connection", |g| {
            g.tcp.conns.keys().all(|k| k.0 != 22)
        });
    }

    #[test]
    fn a_refused_connect_resets_the_guest() {
        let (l, port) = listener();
        drop(l);
        let mut g = Guest::new();
        let h = g.connect(port);
        g.step();
        g.until("the reset", |g| g.socket(h).state() == tcp::State::Closed);
        assert_eq!(g.tcp.len(), 0);
    }

    #[test]
    fn the_host_closing_first_reaches_the_guest_as_eof() {
        let (listener, port) = listener();
        let mut g = Guest::new();
        let h = g.connect(port);
        g.until("the connection", |g| g.socket(h).may_send());
        let (mut host, _) = listener.accept().unwrap();
        host.write_all(b"bye").unwrap();
        drop(host);
        let mut got = Vec::new();
        g.until("the guest's EOF", |g| {
            let s = g.socket(h);
            let mut buf = [0u8; 16];
            while s.can_recv() {
                let n = s.recv_slice(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            !s.may_recv()
        });
        assert_eq!(got, b"bye");
        g.socket(h).close();
        g.until("netd to forget the connection", |g| g.tcp.len() == 0);
    }
}
