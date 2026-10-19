//! limina-netd: the per-VM user-mode NAT a worker spawns for a NIC that is not on vmnet
//! (`docs/design/multi-vm-networking.md` §3.2).
//!
//! The worker starts it with one end of a socketpair as descriptor 3 (`limina_netd_wire`); netd
//! takes the shared rings from it and serves the guest's link until the worker goes away, which
//! it sees as EOF. It has no other way in: no path, no port, no name.
//!
//! One thread and one kqueue: the channel's doorbell, every UDP flow's socket and every DNS
//! question's socket wake the same loop, which drains the guest's ring, answers on the link what
//! the link answers (ARP, DHCP, DNS at the gateway), re-originates the rest on host sockets, and
//! frames what comes back for the guest.

mod dns;
mod poller;
mod stack;
mod udp;

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::ExitCode;
use std::time::Instant;

use limina_netd_wire::{MAX_FRAME, NetdEnd, TAG_FRAME, drain, ring};
use limina_ring::Producer;

use crate::poller::Poller;
use crate::stack::{Ingress, Link, Stack};

/// The descriptor the worker hands over.
const CHANNEL_FD: i32 = 3;
const DNS_PORT: u16 = 53;

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

/// Whether the guest may reach `dst` through the NAT. The host's own loopback stays the host's,
/// and broadcast, multicast and the guest's own subnet have nobody behind netd to reach.
fn routable(dst: Ipv4Addr, link: &Link) -> bool {
    let subnet = |a: Ipv4Addr| a.to_bits() & link.netmask.to_bits();
    !(dst.is_loopback()
        || dst.is_unspecified()
        || dst.is_broadcast()
        || dst.is_multicast()
        || subnet(dst) == subnet(link.gateway))
}

/// Frames bound for the guest, pushed into its ring as they are made.
struct ToGuest<'a> {
    ring: &'a mut Producer,
    pushed: bool,
}

impl ToGuest<'_> {
    /// A full ring drops the frame; the guest's protocols retry.
    fn push(&mut self, frame: &[u8]) {
        if self.ring.push(TAG_FRAME, &[frame]).is_ok() {
            self.pushed = true;
        }
    }
}

struct Netd {
    stack: Stack,
    udp: udp::UdpNat,
    dns: dns::Resolver,
    poller: Poller,
}

impl Netd {
    /// One frame from the guest.
    fn guest_frame(&mut self, frame: &[u8], out: &mut ToGuest<'_>) {
        let Some(Ingress::Udp {
            guest_port,
            dst,
            payload,
        }) = self.stack.handle(frame, &mut |f| out.push(f))
        else {
            return;
        };
        let gateway = self.stack.link().gateway;
        if *dst.ip() == gateway {
            if dst.port() == DNS_PORT {
                self.dns_question(guest_port, payload, out);
            }
            return;
        }
        if !routable(*dst.ip(), self.stack.link()) {
            log::debug!("UDP to {dst} is not routed");
            return;
        }
        if let Err(e) = self.udp.send(guest_port, dst, payload, &self.poller) {
            log::debug!("UDP {guest_port} → {dst}: {e}");
        }
    }

    fn dns_question(&mut self, guest_port: u16, msg: &[u8], out: &mut ToGuest<'_>) {
        let answer = match dns::parse_query(msg) {
            Ok(q) => self.dns.ask(guest_port, q, &self.poller),
            Err(dns::Refusal::Drop) => None,
            Err(dns::Refusal::Answer { id, rcode }) => {
                Some((guest_port, dns::refusal(msg, id, rcode)))
            }
        };
        if let Some(a) = answer {
            self.dns_answer(a, out);
        }
    }

    fn dns_answer(&mut self, (guest_port, msg): dns::Answer, out: &mut ToGuest<'_>) {
        let src = SocketAddrV4::new(self.stack.link().gateway, DNS_PORT);
        self.stack
            .udp_to_guest(src, guest_port, &msg, &mut |f| out.push(f));
    }

    /// A host socket became readable.
    fn readable(&mut self, fd: RawFd, out: &mut ToGuest<'_>) {
        if self.udp.owns(fd) {
            let stack = &mut self.stack;
            self.udp.recv(fd, &mut |src, guest_port, data| {
                stack.udp_to_guest(src, guest_port, data, &mut |f| out.push(f))
            });
        } else if self.dns.owns(fd)
            && let Some(a) = self.dns.process(fd)
        {
            self.dns_answer(a, out);
        }
    }

    fn expire(&mut self, now: Instant, out: &mut ToGuest<'_>) {
        self.udp.expire(now);
        for a in self.dns.expire(now) {
            self.dns_answer(a, out);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        [self.udp.next_deadline(), self.dns.next_deadline()]
            .into_iter()
            .flatten()
            .min()
    }
}

fn serve(sock: OwnedFd) -> io::Result<()> {
    let mut end = NetdEnd::accept(sock)?;
    let poller = Poller::new()?;
    poller.add(end.sock.as_raw_fd())?;
    let mut netd = Netd {
        stack: Stack::new(Link::default()),
        udp: udp::UdpNat::default(),
        dns: dns::Resolver::default(),
        poller,
    };
    let mut frame = vec![0u8; MAX_FRAME];
    let mut ready = Vec::new();
    log::info!("limina-netd: serving the guest's link");
    loop {
        if drain(end.sock.as_fd())? {
            log::info!("limina-netd: the worker went away");
            return Ok(());
        }
        let mut out = ToGuest {
            ring: &mut end.to_guest,
            pushed: false,
        };
        while let Some((tag, len)) = end
            .from_guest
            .read_into(&mut frame)
            .map_err(|e| io::Error::other(format!("the guest ring: {e:?}")))?
        {
            if tag == TAG_FRAME {
                netd.guest_frame(&frame[..len], &mut out);
            }
        }

        // Sleep only when the guest ring is parked; otherwise just collect what is ready.
        let parked = end.from_guest.park();
        let timeout = if parked {
            netd.next_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()))
        } else {
            Some(std::time::Duration::ZERO)
        };
        netd.poller.wait(timeout, &mut ready)?;
        if parked {
            end.from_guest.unpark();
        }
        for &fd in &ready {
            netd.readable(fd, &mut out);
        }
        netd.expire(Instant::now(), &mut out);
        if out.pushed && end.to_guest.publish() {
            ring(end.sock.as_fd());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nat_reaches_out_but_not_into_the_host_or_the_link() {
        let link = Link::default();
        assert!(routable(Ipv4Addr::new(1, 1, 1, 1), &link));
        assert!(routable(Ipv4Addr::new(192, 168, 1, 10), &link), "the LAN");
        assert!(!routable(Ipv4Addr::LOCALHOST, &link));
        assert!(!routable(Ipv4Addr::new(127, 9, 9, 9), &link));
        assert!(!routable(Ipv4Addr::BROADCAST, &link));
        assert!(!routable(Ipv4Addr::new(224, 0, 0, 251), &link), "mDNS");
        assert!(!routable(Ipv4Addr::new(192, 168, 127, 255), &link));
        assert!(!routable(Ipv4Addr::UNSPECIFIED, &link));
    }
}
