//! The guest's pings past the gateway, sent again from one unprivileged ICMP socket
//! (`SOCK_DGRAM`, `IPPROTO_ICMP`: no root).
//!
//! macOS hands every such socket every echo reply on the host, whoever sent the request, and
//! leaves the identifier as the sender wrote it (`spikes/netd-icmp-probe/RESULTS.md`). So each
//! guest flow (its identifier and destination) goes out under an identifier of netd's own, and a
//! reply is the guest's only if that identifier and the replying address both match. The kernel
//! does not fill in the checksum either; smoltcp's emitter does.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr, Ipv4Address};

use crate::poller::Poller;

/// How long a flow is remembered without a ping either way.
const IDLE: Duration = Duration::from_secs(60);
/// The most flows at once; the stalest goes to make room.
const MAX_FLOWS: usize = 1024;

struct Flow {
    /// The guest's identifier.
    guest: u16,
    last: Instant,
}

/// Where a reply goes: the address it came from, and the guest's identifier, sequence number and
/// data.
pub type Deliver<'a> = dyn FnMut(Ipv4Address, u16, u16, &[u8]) + 'a;

#[derive(Default)]
pub struct IcmpNat {
    /// Opened at the first ping.
    sock: Option<OwnedFd>,
    /// By netd's identifier and the destination.
    flows: HashMap<(u16, Ipv4Address), Flow>,
    /// netd's identifier for each guest identifier and destination.
    ours: HashMap<(u16, Ipv4Address), u16>,
}

fn sockaddr(ip: Ipv4Address) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_len: size_of::<libc::sockaddr_in>() as u8,
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from(ip).to_be(),
        },
        sin_zero: [0; 8],
    }
}

fn open(poller: &Poller) -> io::Result<OwnedFd> {
    // SAFETY: a plain syscall; the result is ours.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: flag changes on our own descriptor.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    poller.add(fd)?;
    Ok(sock)
}

impl IcmpNat {
    pub fn owns(&self, fd: RawFd) -> bool {
        self.sock.as_ref().is_some_and(|s| s.as_raw_fd() == fd)
    }

    /// Sends the guest's echo request to `dst` under netd's identifier for the flow. One the host
    /// will not send is dropped, as a network would.
    pub fn send(
        &mut self,
        dst: Ipv4Address,
        ident: u16,
        seq_no: u16,
        data: &[u8],
        poller: &Poller,
    ) -> io::Result<()> {
        if self.sock.is_none() {
            self.sock = Some(open(poller)?);
        }
        let ours = match self.ours.get(&(ident, dst)) {
            Some(&o) => o,
            None => {
                if self.flows.len() >= MAX_FLOWS {
                    self.evict_stalest();
                }
                let o = loop {
                    // SAFETY: arc4random has no preconditions.
                    let o = unsafe { libc::arc4random() } as u16;
                    if !self.flows.contains_key(&(o, dst)) {
                        break o;
                    }
                };
                self.ours.insert((ident, dst), o);
                o
            }
        };
        self.flows.insert(
            (ours, dst),
            Flow {
                guest: ident,
                last: Instant::now(),
            },
        );
        let repr = Icmpv4Repr::EchoRequest {
            ident: ours,
            seq_no,
            data,
        };
        let mut msg = vec![0u8; repr.buffer_len()];
        repr.emit(
            &mut Icmpv4Packet::new_unchecked(&mut msg[..]),
            &ChecksumCapabilities::default(),
        );
        let to = sockaddr(dst);
        let fd = self.sock.as_ref().expect("opened above").as_raw_fd();
        // SAFETY: a message and a sockaddr_in of their stated sizes.
        let n = unsafe {
            libc::sendto(
                fd,
                msg.as_ptr().cast(),
                msg.len(),
                0,
                &to as *const _ as *const _,
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if n < 0 {
            log::debug!("ICMP → {dst}: {}", io::Error::last_os_error());
        }
        Ok(())
    }

    /// Reads every reply waiting, handing each of the guest's to `deliver` with the address it
    /// came from and the guest's identifier, sequence number and data.
    pub fn recv(&mut self, deliver: &mut Deliver<'_>) {
        let Some(sock) = &self.sock else { return };
        let mut buf = [0u8; 65536];
        loop {
            let mut from = sockaddr(Ipv4Address::UNSPECIFIED);
            let mut len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
            // SAFETY: output into a local buffer and sockaddr of their stated sizes.
            let n = unsafe {
                libc::recvfrom(
                    sock.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    0,
                    &mut from as *mut _ as *mut _,
                    &mut len,
                )
            };
            if n < 0 {
                return;
            }
            // Each reply starts with its IP header.
            let msg = &buf[..n as usize];
            let Some(ihl) = msg.first().map(|b| ((b & 0xf) as usize) * 4) else {
                continue;
            };
            let Some(icmp) = msg
                .get(ihl..)
                .and_then(|m| Icmpv4Packet::new_checked(m).ok())
            else {
                continue;
            };
            let Ok(Icmpv4Repr::EchoReply {
                ident,
                seq_no,
                data,
            }) = Icmpv4Repr::parse(&icmp, &ChecksumCapabilities::default())
            else {
                continue;
            };
            let src = Ipv4Address::from(u32::from_be(from.sin_addr.s_addr));
            // Every process's replies arrive here; only the ones netd asked for are the guest's.
            if let Some(flow) = self.flows.get_mut(&(ident, src)) {
                flow.last = Instant::now();
                deliver(src, flow.guest, seq_no, data);
            }
        }
    }

    /// Forgets the flows idle since before `now - IDLE`.
    pub fn expire(&mut self, now: Instant) {
        let stale: Vec<(u16, Ipv4Address)> = self
            .flows
            .iter()
            .filter(|(_, f)| now.duration_since(f.last) >= IDLE)
            .map(|(k, _)| *k)
            .collect();
        for key in stale {
            self.forget(key);
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.flows.values().map(|f| f.last + IDLE).min()
    }

    fn evict_stalest(&mut self) {
        if let Some(key) = self
            .flows
            .iter()
            .min_by_key(|(_, f)| f.last)
            .map(|(k, _)| *k)
        {
            self.forget(key);
        }
    }

    fn forget(&mut self, key: (u16, Ipv4Address)) {
        if let Some(f) = self.flows.remove(&key) {
            self.ours.remove(&(f.guest, key.1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replies(nat: &mut IcmpNat, poller: &Poller) -> Vec<(Ipv4Address, u16, u16, Vec<u8>)> {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            poller
                .wait(Some(Duration::from_millis(100)), &mut events)
                .unwrap();
            if events.iter().any(|e| nat.owns(e.fd)) {
                nat.recv(&mut |src, ident, seq, data| got.push((src, ident, seq, data.to_vec())));
            }
        }
        got
    }

    /// The guest's identifier goes out as netd's and comes back as the guest's; a reply to
    /// someone else's ping, which the socket sees too, is not delivered.
    #[test]
    fn a_ping_comes_back_under_the_guests_identifier() {
        let poller = Poller::new().unwrap();
        let mut nat = IcmpNat::default();
        let lo = Ipv4Address::new(127, 0, 0, 1);
        nat.send(lo, 0x4242, 7, b"limina", &poller).unwrap();

        // Someone else's ping of the same address, from a socket of their own.
        // SAFETY: a plain syscall; the result is ours.
        let other = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP) };
        assert!(other >= 0);
        // SAFETY: just created.
        let other = unsafe { OwnedFd::from_raw_fd(other) };
        let repr = Icmpv4Repr::EchoRequest {
            ident: 0x9999,
            seq_no: 1,
            data: b"other",
        };
        let mut msg = vec![0u8; repr.buffer_len()];
        repr.emit(
            &mut Icmpv4Packet::new_unchecked(&mut msg[..]),
            &ChecksumCapabilities::default(),
        );
        let to = sockaddr(lo);
        // SAFETY: a message and a sockaddr_in of their stated sizes.
        unsafe {
            libc::sendto(
                other.as_raw_fd(),
                msg.as_ptr().cast(),
                msg.len(),
                0,
                &to as *const _ as *const _,
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };

        let got = replies(&mut nat, &poller);
        assert_eq!(got, vec![(lo, 0x4242, 7, b"limina".to_vec())]);
    }

    #[test]
    fn idle_flows_are_forgotten() {
        let poller = Poller::new().unwrap();
        let mut nat = IcmpNat::default();
        let lo = Ipv4Address::new(127, 0, 0, 1);
        nat.send(lo, 1, 1, b"x", &poller).unwrap();
        let deadline = nat.next_deadline().unwrap();
        nat.expire(deadline);
        assert!(nat.flows.is_empty() && nat.ours.is_empty());
    }
}
