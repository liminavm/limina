//! The channel between a worker and the `limina-netd` it spawns
//! (`docs/design/multi-vm-networking.md` §3.2).
//!
//! The worker hands netd one end of a Unix stream socketpair as an inherited descriptor, so the
//! channel has no name anyone else could reach and nothing on disk to leave behind. Over it:
//!
//! 1. the worker sends a [`Setup`] message carrying a shared-memory descriptor (`SCM_RIGHTS`);
//! 2. from then on every byte is a doorbell, written only when the peer has parked
//!    ([`limina_ring`]'s handshake), and each side drains what it receives before it parks.
//!
//! EOF is the peer's death. The shared memory holds two [`limina_ring`] rings of
//! [`TAG_FRAME`] records, each a frame with its 12-byte `virtio_net_hdr` in front: worker → netd
//! at [`TO_NETD_OFFSET`], netd → worker at [`TO_GUEST_OFFSET`]. The worker creates and formats the
//! region and attaches its ends before netd exists; netd attaches late, which the ring allows.
//!
//! The region is a POSIX shared-memory object whose random name is unlinked straight after it is
//! created, before it is sized, so it can be neither left behind nor opened by name later.
//!
//! A second pair, of datagrams ([`control_socketpair`]), carries the VM's port forwards: the
//! worker binds each forward's listening socket and hands it over ([`Control::Listen`]), so the
//! port stays the worker's across a netd restart and netd never binds one itself.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

use limina_ring::{Consumer, Producer};

/// The descriptor netd finds the channel at.
pub const CHANNEL_FD: i32 = 3;
/// The descriptor netd finds the control pair at.
pub const CONTROL_FD: i32 = 4;

/// The ring protocol number both ends check, and the setup message's magic.
pub const PROTOCOL: u32 = u32::from_le_bytes(*b"NTD2");
/// A record holding one frame, `virtio_net_hdr` first.
pub const TAG_FRAME: u32 = 1;
/// The `virtio_net_hdr_v1` in front of every frame.
pub const VNET_HDR_LEN: usize = 12;
/// The largest frame with its header: vmnet's and libkrun's 65550-byte packet plus the header.
pub const MAX_FRAME: usize = VNET_HDR_LEN + 65550;
/// Each ring's data area.
pub const RING_DATA_SIZE: u32 = 1 << 20;
/// Host page size; each ring starts on a page.
const PAGE: usize = 16384;

const fn page_up(n: usize) -> usize {
    (n + PAGE - 1) & !(PAGE - 1)
}

/// Where the worker → netd ring starts in the region.
pub const TO_NETD_OFFSET: usize = 0;
/// Where the netd → worker ring starts in the region.
pub const TO_GUEST_OFFSET: usize = page_up(limina_ring::region_size(RING_DATA_SIZE));
/// The whole region.
pub const REGION_SIZE: usize = TO_GUEST_OFFSET + page_up(limina_ring::region_size(RING_DATA_SIZE));

const _: () = assert!(RING_DATA_SIZE as usize / 2 - limina_ring::HEADER_LEN >= MAX_FRAME);

/// What the worker tells netd about the region. Both ends compile the layout in; netd refuses a
/// region laid out any other way rather than trusting the numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub protocol: u32,
    pub ring_data_size: u32,
    pub region_size: u64,
    pub to_netd_offset: u64,
    pub to_guest_offset: u64,
}

impl Setup {
    const LEN: usize = 32;

    /// This build's layout.
    pub const fn ours() -> Self {
        Self {
            protocol: PROTOCOL,
            ring_data_size: RING_DATA_SIZE,
            region_size: REGION_SIZE as u64,
            to_netd_offset: TO_NETD_OFFSET as u64,
            to_guest_offset: TO_GUEST_OFFSET as u64,
        }
    }

    fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&self.protocol.to_le_bytes());
        b[4..8].copy_from_slice(&self.ring_data_size.to_le_bytes());
        b[8..16].copy_from_slice(&self.region_size.to_le_bytes());
        b[16..24].copy_from_slice(&self.to_netd_offset.to_le_bytes());
        b[24..32].copy_from_slice(&self.to_guest_offset.to_le_bytes());
        b
    }

    fn decode(b: &[u8; Self::LEN]) -> Self {
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        Self {
            protocol: u32_at(0),
            ring_data_size: u32_at(4),
            region_size: u64_at(8),
            to_netd_offset: u64_at(16),
            to_guest_offset: u64_at(24),
        }
    }
}

fn last_error<T>() -> io::Result<T> {
    Err(io::Error::last_os_error())
}

/// A shared mapping of the channel's memory.
pub struct Region {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: a plain mapping; what is shared through it is the rings' business.
unsafe impl Send for Region {}

impl Region {
    /// Creates a fresh object of `len` bytes and maps it. The returned descriptor is the only way
    /// to reach it.
    pub fn create(len: usize) -> io::Result<(Self, OwnedFd)> {
        let mut rnd = [0u8; 8];
        // SAFETY: fills exactly the buffer it is given.
        unsafe { libc::arc4random_buf(rnd.as_mut_ptr().cast(), rnd.len()) };
        // At most 31 bytes with the slash (PSHMNAMLEN).
        let name = format!("/limina-netd.{:016x}\0", u64::from_le_bytes(rnd));
        // SAFETY: a NUL-terminated name; the mode is passed as the variadic argument.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr().cast(),
                // shm_open refuses O_CLOEXEC (EINVAL); it is set below.
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return last_error();
        }
        // SAFETY: we own the new descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: the same name. Unlinked before anything else, so no path below leaves it behind.
        unsafe { libc::shm_unlink(name.as_ptr().cast()) };
        set_flag(fd.as_fd(), libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
        // SAFETY: a valid descriptor; an object can be sized once, before it is mapped.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return last_error();
        }
        let region = Self::map(fd.as_fd(), len)?;
        Ok((region, fd))
    }

    /// Maps `len` bytes of an object received from the peer. `len` comes from this build's
    /// layout, never from `fstat`, which can report 0 for a received object.
    pub fn map(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        // SAFETY: a fresh shared mapping of a descriptor we hold; the kernel checks the size.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return last_error();
        }
        Ok(Self {
            ptr: NonNull::new(p.cast()).expect("mmap returned null"),
            len,
        })
    }

    /// The address `offset` bytes in.
    pub fn at(&self, offset: usize) -> NonNull<u8> {
        assert!(offset < self.len);
        // SAFETY: inside the mapping.
        unsafe { self.ptr.add(offset) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: our own mapping, unmapped once.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

fn set_flag(fd: BorrowedFd<'_>, get: i32, set: i32, flag: i32) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor we hold.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), get) };
    if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), set, flags | flag) } < 0 {
        return last_error();
    }
    Ok(())
}

/// Makes `fd` non-blocking.
pub fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    set_flag(fd, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)
}

fn pair(kind: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: a two-element array for the pair.
    if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) } != 0 {
        return last_error();
    }
    // SAFETY: both are ours, just created.
    let pair = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [pair.0.as_fd(), pair.1.as_fd()] {
        set_flag(fd, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
        let one: libc::c_int = 1;
        // SAFETY: a valid option of the right size.
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&one as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return last_error();
        }
    }
    Ok(pair)
}

/// A connected pair for a worker and its netd. Both ends are close-on-exec, so another child of
/// the worker never holds netd's end open past netd's death; and neither raises `SIGPIPE`, so a
/// write to a dead peer is an error, not the end of the process.
pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    pair(libc::SOCK_STREAM)
}

/// The control pair, as [`socketpair`] but of datagrams, each one [`Control`] message.
pub fn control_socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    pair(libc::SOCK_DGRAM)
}

/// Sends `body` with `fd`, if any, attached.
fn send_msg(sock: BorrowedFd<'_>, body: &[u8], fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: body.as_ptr() as *mut _,
        iov_len: body.len(),
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: an all-zero msghdr is a valid empty one.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as libc::socklen_t;
        // SAFETY: the control buffer holds exactly one descriptor's message.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as libc::socklen_t;
            libc::CMSG_DATA(c)
                .cast::<libc::c_int>()
                .write_unaligned(fd.as_raw_fd());
        }
    }
    // SAFETY: a fully initialised message.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) };
    if n < 0 {
        return last_error();
    }
    if n as usize != body.len() {
        return Err(io::Error::other("short message"));
    }
    Ok(())
}

/// Receives into `body`: how much arrived (0 at EOF) and the descriptor it carried, close-on-exec.
fn recv_msg(sock: BorrowedFd<'_>, body: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: body.as_mut_ptr().cast(),
        iov_len: body.len(),
    };
    // SAFETY: an all-zero msghdr is a valid empty one.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as libc::socklen_t;
    // SAFETY: buffers sized above.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return last_error();
    }
    let mut fd = None;
    // SAFETY: walking the control messages the kernel just wrote.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let raw = libc::CMSG_DATA(c).cast::<libc::c_int>().read_unaligned();
                fd = Some(OwnedFd::from_raw_fd(raw));
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if let Some(fd) = &fd {
        set_flag(fd.as_fd(), libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
    }
    Ok((n as usize, fd))
}

/// Sends the setup message and the region's descriptor.
fn send_setup(sock: BorrowedFd<'_>, region: BorrowedFd<'_>) -> io::Result<()> {
    send_msg(sock, &Setup::ours().encode(), Some(region))
}

/// Receives the setup message and its descriptor, refusing any other layout. Blocks.
fn recv_setup(sock: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let mut body = [0u8; Setup::LEN];
    let mut got = 0;
    let mut fd: Option<OwnedFd> = None;
    while got < body.len() {
        let (n, f) = recv_msg(sock, &mut body[got..])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        fd = fd.or(f);
        got += n;
    }
    let setup = Setup::decode(&body);
    if setup != Setup::ours() {
        return Err(io::Error::other(format!(
            "the worker's channel is laid out as {setup:?}, not {:?}",
            Setup::ours()
        )));
    }
    fd.ok_or_else(|| io::Error::other("the setup message carried no region"))
}

/// A message on the control pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// Accept connections on the listening socket that comes with this message, and carry each
    /// to `guest_port` on the guest. `id` names the forward for [`Control::Close`].
    Listen { id: u32, guest_port: u16 },
    /// Stop accepting on forward `id`; its connections carry on.
    Close { id: u32 },
    /// The guest is at `ip`, with `mac`: what netd's frames go to until the guest takes netd's
    /// lease. The worker says so when the guest's address is not netd's to give (a NIC on vmnet,
    /// where netd carries only the forwards).
    Guest { mac: [u8; 6], ip: [u8; 4] },
}

impl Control {
    const LEN: usize = 12;

    fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        match *self {
            Control::Listen { id, guest_port } => {
                b[0] = 1;
                b[2..4].copy_from_slice(&guest_port.to_le_bytes());
                b[4..8].copy_from_slice(&id.to_le_bytes());
            }
            Control::Close { id } => {
                b[0] = 2;
                b[4..8].copy_from_slice(&id.to_le_bytes());
            }
            Control::Guest { mac, ip } => {
                b[0] = 3;
                b[2..8].copy_from_slice(&mac);
                b[8..12].copy_from_slice(&ip);
            }
        }
        b
    }

    fn decode(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::LEN] = b.try_into().ok()?;
        let port = u16::from_le_bytes([b[2], b[3]]);
        let id = u32::from_le_bytes(b[4..8].try_into().unwrap());
        match b[0] {
            1 => Some(Control::Listen {
                id,
                guest_port: port,
            }),
            2 => Some(Control::Close { id }),
            3 => Some(Control::Guest {
                mac: b[2..8].try_into().unwrap(),
                ip: b[8..12].try_into().unwrap(),
            }),
            _ => None,
        }
    }
}

/// Sends a control message; a [`Control::Listen`] takes its listening socket along.
pub fn send_control(
    sock: BorrowedFd<'_>,
    msg: Control,
    listener: Option<BorrowedFd<'_>>,
) -> io::Result<()> {
    send_msg(sock, &msg.encode(), listener)
}

/// The next control message, if one is waiting (`sock` non-blocking), with the descriptor it
/// carried. A message netd does not understand is an error, as is a `Listen` without its socket.
pub fn recv_control(sock: BorrowedFd<'_>) -> io::Result<Option<(Control, Option<OwnedFd>)>> {
    let mut body = [0u8; Control::LEN + 1];
    let (n, fd) = match recv_msg(sock, &mut body) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(e),
    };
    let msg = Control::decode(&body[..n])
        .ok_or_else(|| io::Error::other(format!("a control message of {n} bytes: {body:?}")))?;
    if matches!(msg, Control::Listen { .. }) && fd.is_none() {
        return Err(io::Error::other("a forward without its listening socket"));
    }
    Ok(Some((msg, fd)))
}

/// Rings the peer's doorbell. A full socket means the peer has wakes queued already, and a dead
/// one shows as EOF on our side, so neither is an error here.
pub fn ring(sock: BorrowedFd<'_>) {
    let b = 1u8;
    // SAFETY: one byte from a local.
    unsafe {
        libc::send(
            sock.as_raw_fd(),
            (&b as *const u8).cast(),
            1,
            libc::MSG_DONTWAIT,
        )
    };
}

/// Reads every pending doorbell byte. `Ok(true)` when the peer has closed its end.
pub fn drain(sock: BorrowedFd<'_>) -> io::Result<bool> {
    let mut buf = [0u8; 256];
    loop {
        // SAFETY: into a local buffer.
        let n = unsafe {
            libc::recv(
                sock.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        match n {
            0 => return Ok(true),
            n if n > 0 => continue,
            _ => {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::WouldBlock => Ok(false),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(e),
                };
            }
        }
    }
}

/// The worker's end: it produces the guest's frames for netd and consumes netd's for the guest.
pub struct WorkerEnd {
    pub to_netd: Producer,
    pub to_guest: Consumer,
    pub sock: OwnedFd,
    _region: Region,
}

impl WorkerEnd {
    /// Creates the region, formats both rings, attaches this end and sends the region to netd
    /// over `sock`.
    pub fn new(sock: OwnedFd) -> io::Result<Self> {
        let (region, fd) = Region::create(REGION_SIZE)?;
        let proto = |e| io::Error::other(format!("ring: {e:?}"));
        // SAFETY: both rings lie inside the mapping, page-aligned, which outlives them in `Self`;
        // the worker is the only producer of one and the only consumer of the other.
        let (to_netd, to_guest) = unsafe {
            for off in [TO_NETD_OFFSET, TO_GUEST_OFFSET] {
                limina_ring::format(region.at(off), RING_DATA_SIZE, PROTOCOL).map_err(proto)?;
            }
            (
                Producer::attach(region.at(TO_NETD_OFFSET), RING_DATA_SIZE, PROTOCOL)
                    .map_err(proto)?,
                Consumer::attach(region.at(TO_GUEST_OFFSET), RING_DATA_SIZE, PROTOCOL)
                    .map_err(proto)?,
            )
        };
        send_setup(sock.as_fd(), fd.as_fd())?;
        set_nonblocking(sock.as_fd())?;
        Ok(Self {
            to_netd,
            to_guest,
            sock,
            _region: region,
        })
    }
}

/// netd's end: it consumes the guest's frames and produces frames for the guest.
pub struct NetdEnd {
    pub from_guest: Consumer,
    pub to_guest: Producer,
    pub sock: OwnedFd,
    _region: Region,
}

impl NetdEnd {
    /// Receives the worker's region over `sock` (blocking) and attaches this end.
    pub fn accept(sock: OwnedFd) -> io::Result<Self> {
        let fd = recv_setup(sock.as_fd())?;
        let region = Region::map(fd.as_fd(), REGION_SIZE)?;
        drop(fd);
        let proto = |e| io::Error::other(format!("ring: {e:?}"));
        // SAFETY: as in `WorkerEnd::new`, from the other side.
        let (from_guest, to_guest) = unsafe {
            (
                Consumer::attach(region.at(TO_NETD_OFFSET), RING_DATA_SIZE, PROTOCOL)
                    .map_err(proto)?,
                Producer::attach(region.at(TO_GUEST_OFFSET), RING_DATA_SIZE, PROTOCOL)
                    .map_err(proto)?,
            )
        };
        set_nonblocking(sock.as_fd())?;
        Ok(Self {
            from_guest,
            to_guest,
            sock,
            _region: region,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_cross_both_ways_and_a_parked_peer_is_rung() {
        let (w, n) = socketpair().unwrap();
        let mut worker = WorkerEnd::new(w).unwrap();
        // netd arrives after the guest has sent something.
        worker.to_netd.push(TAG_FRAME, &[b"arp who-has"]).unwrap();
        worker.to_netd.publish();
        let mut netd = NetdEnd::accept(n).unwrap();

        let mut buf = vec![0u8; MAX_FRAME];
        assert_eq!(
            netd.from_guest.read_into(&mut buf).unwrap(),
            Some((TAG_FRAME, 11))
        );
        assert_eq!(&buf[..11], b"arp who-has");

        // netd parks; the worker's next publish must ring it, and the ring must arrive.
        assert!(netd.from_guest.park());
        worker.to_netd.push(TAG_FRAME, &[b"dhcp"]).unwrap();
        assert!(worker.to_netd.publish());
        ring(worker.sock.as_fd());
        assert!(!drain(netd.sock.as_fd()).unwrap());
        netd.from_guest.unpark();
        assert_eq!(
            netd.from_guest.read_into(&mut buf).unwrap(),
            Some((TAG_FRAME, 4))
        );

        netd.to_guest.push(TAG_FRAME, &[b"offer"]).unwrap();
        netd.to_guest.publish();
        assert_eq!(
            worker.to_guest.read_into(&mut buf).unwrap(),
            Some((TAG_FRAME, 5))
        );

        // The worker's death is EOF for netd.
        drop(worker);
        assert!(drain(netd.sock.as_fd()).unwrap());
    }

    #[test]
    fn a_region_from_another_layout_is_refused() {
        let (w, n) = socketpair().unwrap();
        let (_region, fd) = Region::create(REGION_SIZE).unwrap();
        let mut bad = Setup::ours();
        bad.ring_data_size *= 2;
        let body = bad.encode();
        let mut iov = libc::iovec {
            iov_base: body.as_ptr() as *mut _,
            iov_len: body.len(),
        };
        let space = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as libc::socklen_t;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as libc::socklen_t;
            libc::CMSG_DATA(c)
                .cast::<libc::c_int>()
                .write_unaligned(fd.as_raw_fd());
            assert_eq!(libc::sendmsg(w.as_raw_fd(), &msg, 0), body.len() as isize);
        }
        assert!(NetdEnd::accept(n).is_err());
    }

    #[test]
    fn control_messages_carry_their_listening_socket() {
        let (worker, netd) = control_socketpair().unwrap();
        set_nonblocking(netd.as_fd()).unwrap();
        assert!(recv_control(netd.as_fd()).unwrap().is_none());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let listen = Control::Listen {
            id: 7,
            guest_port: 22,
        };
        send_control(worker.as_fd(), listen, Some(listener.as_fd())).unwrap();
        send_control(worker.as_fd(), Control::Close { id: 7 }, None).unwrap();

        let (msg, fd) = recv_control(netd.as_fd()).unwrap().unwrap();
        assert_eq!(msg, listen);
        let theirs = std::net::TcpListener::from(fd.unwrap());
        assert_eq!(
            theirs.local_addr().unwrap(),
            addr,
            "the same listening socket"
        );
        let (msg, fd) = recv_control(netd.as_fd()).unwrap().unwrap();
        assert_eq!(msg, Control::Close { id: 7 });
        assert!(fd.is_none());

        let guest = Control::Guest {
            mac: [0x5a, 1, 2, 3, 4, 5],
            ip: [192, 168, 64, 7],
        };
        send_control(worker.as_fd(), guest, None).unwrap();
        assert_eq!(recv_control(netd.as_fd()).unwrap().unwrap().0, guest);
    }

    #[test]
    fn the_layout_fits_in_the_region() {
        assert_eq!(TO_GUEST_OFFSET % PAGE, 0);
        assert!(TO_GUEST_OFFSET >= limina_ring::region_size(RING_DATA_SIZE));
        assert!(REGION_SIZE >= TO_GUEST_OFFSET + limina_ring::region_size(RING_DATA_SIZE));
    }
}
