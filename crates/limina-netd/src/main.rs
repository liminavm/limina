//! limina-netd: the per-VM user-mode NAT a worker spawns for a NIC that is not on vmnet
//! (`docs/design/multi-vm-networking.md` §3.2).
//!
//! The worker starts it with one end of a socketpair as descriptor 3 (`limina_netd_wire`); netd
//! takes the shared rings from it and serves the guest's link until the worker goes away, which
//! it sees as EOF. Descriptor 4, when present, carries the VM's port forwards. It has no other way
//! in: no path, no port, no name.
//!
//! One thread and one kqueue: the channel's doorbell and every host socket (UDP flows, DNS
//! questions, TCP connections) wake the same loop, which drains the guest's ring, answers on the
//! link what the link answers (ARP, DHCP, DNS at the gateway), re-originates the rest on host
//! sockets, and frames what comes back for the guest.

mod dns;
mod forward;
mod poller;
mod sandbox;
mod stack;
mod tcp;
mod udp;

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use limina_netd_wire::{CHANNEL_FD, CONTROL_FD, MAX_FRAME, NetdEnd, TAG_FRAME, drain, ring};
use limina_ring::Producer;

use crate::poller::{Event, Filter, Poller};
use crate::stack::{Ingress, Link, Stack};

const DNS_PORT: u16 = 53;
/// How soon netd tries again after the guest's ring filled: the worker drains it without telling
/// netd, and a TCP segment left over waits on this rather than on a retransmission timeout.
const RING_FULL_RETRY: Duration = Duration::from_millis(1);
/// The descriptors netd needs at most: every TCP connection, UDP flow and DNS question at once,
/// and a margin.
const FDS_WANTED: u64 = (tcp::MAX_CONNS + udp::MAX_FLOWS + dns::MAX_PENDING + 64) as u64;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_millis()
        .init();
    raise_fd_limit();
    // SAFETY: descriptors 3 and 4 are the worker's handover, and nothing else in this process
    // owns them; 4 is taken only if it is open.
    let sock = unsafe { OwnedFd::from_raw_fd(CHANNEL_FD) };
    let ctl = (unsafe { libc::fcntl(CONTROL_FD, libc::F_GETFD) } >= 0)
        .then(|| unsafe { OwnedFd::from_raw_fd(CONTROL_FD) });
    match serve(sock, ctl) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("limina-netd: {e}");
            ExitCode::FAILURE
        }
    }
}

/// netd inherits the worker's descriptor limit, which for a launchd job is 256: a browser's worth
/// of connections would fail with EMFILE. Raise it to what netd can use, as far as the hard limit
/// allows.
fn raise_fd_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: output into a local.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return;
    }
    if lim.rlim_cur >= FDS_WANTED {
        return;
    }
    let want = FDS_WANTED.min(lim.rlim_max);
    let new = libc::rlimit {
        rlim_cur: want,
        rlim_max: lim.rlim_max,
    };
    // SAFETY: input from a local.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) } != 0 || want < FDS_WANTED {
        log::warn!(
            "limina-netd: descriptor limit {} (wanted {FDS_WANTED}); connections may fail under load",
            if want < FDS_WANTED {
                want
            } else {
                lim.rlim_cur
            }
        );
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
    /// A frame found the ring full this pass.
    full: bool,
}

impl ToGuest<'_> {
    /// Whether the ring took the frame. A dropped datagram is the guest's protocols' to retry; a
    /// TCP segment is netd's, and goes again on the next pass.
    fn push(&mut self, frame: &[u8]) -> bool {
        let ok = self.ring.push(TAG_FRAME, &[frame]).is_ok();
        self.pushed |= ok;
        self.full |= !ok;
        ok
    }
}

struct Netd {
    stack: Stack,
    udp: udp::UdpNat,
    dns: dns::Resolver,
    tcp: tcp::TcpNat,
    forwards: forward::Forwards,
    poller: Poller,
}

/// The TCP translator's way to the guest: through the stack's framing into the ring.
macro_rules! tcp_emit {
    ($stack:expr, $out:expr) => {
        &mut |remote, header, payload| {
            $stack.tcp_to_guest(remote, header, payload, &mut |f| $out.push(f))
        }
    };
}

impl Netd {
    /// One frame from the guest.
    fn guest_frame(&mut self, frame: &[u8], out: &mut ToGuest<'_>) {
        let ingress = self.stack.handle(frame, &mut |f| {
            out.push(f);
        });
        match ingress {
            Some(Ingress::Udp {
                guest_port,
                dst,
                payload,
            }) => self.udp_out(guest_port, dst, payload, out),
            Some(Ingress::Tcp {
                guest_port,
                remote,
                segment,
            }) => {
                let allowed = routable(*remote.ip(), self.stack.link());
                self.tcp.segment(
                    (guest_port, remote),
                    &segment,
                    allowed,
                    tcp_emit!(self.stack, out),
                );
            }
            None => {}
        }
    }

    fn udp_out(
        &mut self,
        guest_port: u16,
        dst: SocketAddrV4,
        payload: &[u8],
        out: &mut ToGuest<'_>,
    ) {
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
        self.stack.udp_to_guest(src, guest_port, &msg, &mut |f| {
            out.push(f);
        });
    }

    /// A host socket's event.
    fn event(&mut self, ev: &Event, out: &mut ToGuest<'_>) -> io::Result<()> {
        let fd = ev.fd;
        if self.forwards.is_control(fd) {
            self.forwards.control(&self.poller)?;
        } else if self.forwards.is_listener(fd) {
            let (stack, tcp) = (&self.stack, &mut self.tcp);
            self.forwards.accept(fd, &mut |guest_port, sock| {
                // Until the guest has spoken it has no address; the connection is closed.
                if stack.guest_known() {
                    tcp.open_to_guest(guest_port, stack.link().gateway, sock);
                }
            });
        } else if self.tcp.owns(fd) {
            self.tcp.event(ev, tcp_emit!(self.stack, out));
        } else if ev.filter != Filter::Read {
        } else if self.udp.owns(fd) {
            let stack = &mut self.stack;
            self.udp.recv(fd, &mut |src, guest_port, data| {
                stack.udp_to_guest(src, guest_port, data, &mut |f| {
                    out.push(f);
                })
            });
        } else if self.dns.owns(fd)
            && let Some(a) = self.dns.process(fd)
        {
            self.dns_answer(a, out);
        }
        Ok(())
    }

    fn expire(&mut self, now: Instant, out: &mut ToGuest<'_>) {
        self.udp.expire(now);
        for a in self.dns.expire(now) {
            self.dns_answer(a, out);
        }
        self.tcp.expire(now, tcp_emit!(self.stack, out));
    }

    /// What the TCP connections owe the guest.
    fn flush(&mut self, out: &mut ToGuest<'_>) {
        self.tcp.flush(&self.poller, tcp_emit!(self.stack, out));
    }

    fn next_deadline(&self) -> Option<Instant> {
        [
            self.udp.next_deadline(),
            self.dns.next_deadline(),
            self.tcp.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

fn serve(sock: OwnedFd, ctl: Option<OwnedFd>) -> io::Result<()> {
    let mut end = NetdEnd::accept(sock)?;
    let poller = Poller::new()?;
    poller.add(end.sock.as_raw_fd())?;
    let mut netd = Netd {
        stack: Stack::new(Link::default()),
        udp: udp::UdpNat::default(),
        dns: dns::Resolver::default(),
        tcp: tcp::TcpNat::default(),
        forwards: forward::Forwards::new(ctl, &poller)?,
        poller,
    };
    // Everything netd will ever hold that is not a socket it makes, it holds now.
    sandbox::enter()?;
    let mut frame = vec![0u8; MAX_FRAME];
    let mut events = Vec::new();
    log::info!("limina-netd: serving the guest's link");
    loop {
        if drain(end.sock.as_fd())? {
            log::info!("limina-netd: the worker went away");
            return Ok(());
        }
        let mut out = ToGuest {
            ring: &mut end.to_guest,
            pushed: false,
            full: false,
        };
        for ev in &events {
            netd.event(ev, &mut out)?;
        }
        netd.expire(Instant::now(), &mut out);
        while let Some((tag, len)) = end
            .from_guest
            .read_into(&mut frame)
            .map_err(|e| io::Error::other(format!("the guest ring: {e:?}")))?
        {
            if tag == TAG_FRAME {
                netd.guest_frame(&frame[..len], &mut out);
            }
        }
        netd.flush(&mut out);
        let full = out.full;
        if out.pushed && end.to_guest.publish() {
            ring(end.sock.as_fd());
        }

        // Sleep only when the guest ring is parked; otherwise just collect what is ready.
        let parked = end.from_guest.park();
        let timeout = if full {
            Some(RING_FULL_RETRY)
        } else if parked {
            netd.next_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()))
        } else {
            Some(Duration::ZERO)
        };
        netd.poller.wait(timeout, &mut events)?;
        if parked {
            end.from_guest.unpark();
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
