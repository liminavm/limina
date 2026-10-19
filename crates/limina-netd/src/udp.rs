//! UDP from the guest, re-originated on host sockets: one connected socket per guest port and
//! destination, so replies come back on it and nothing else does. Flows end after a minute idle.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use crate::poller::Poller;

/// How long a flow lives without a datagram either way.
const IDLE: Duration = Duration::from_secs(60);
/// The most flows at once; the stalest goes to make room.
const MAX_FLOWS: usize = 1024;

type Key = (u16, SocketAddrV4);

struct Flow {
    sock: UdpSocket,
    last: Instant,
    /// Whether anything has come back on it yet.
    answered: bool,
}

#[derive(Default)]
pub struct UdpNat {
    flows: HashMap<Key, Flow>,
    by_fd: HashMap<RawFd, Key>,
}

impl UdpNat {
    /// Sends the guest's datagram from `guest_port` to `dst`, opening the flow if it is new. A
    /// datagram the host cannot take right now is dropped, as a network would.
    pub fn send(
        &mut self,
        guest_port: u16,
        dst: SocketAddrV4,
        payload: &[u8],
        poller: &Poller,
    ) -> io::Result<()> {
        let key = (guest_port, dst);
        if !self.flows.contains_key(&key) {
            if self.flows.len() >= MAX_FLOWS {
                self.evict_stalest();
            }
            let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
            sock.connect(dst)?;
            sock.set_nonblocking(true)?;
            poller.add(sock.as_raw_fd())?;
            self.by_fd.insert(sock.as_raw_fd(), key);
            self.flows.insert(
                key,
                Flow {
                    sock,
                    last: Instant::now(),
                    answered: false,
                },
            );
        }
        let flow = self.flows.get_mut(&key).expect("inserted above");
        flow.last = Instant::now();
        match flow.sock.send(payload) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => {
                log::debug!("UDP {guest_port} → {dst}: {e}");
                Ok(())
            }
        }
    }

    pub fn owns(&self, fd: RawFd) -> bool {
        self.by_fd.contains_key(&fd)
    }

    /// Reads every datagram waiting on `fd`'s flow, handing each to `deliver` with the address it
    /// came from and the guest port it is for.
    pub fn recv(&mut self, fd: RawFd, deliver: &mut dyn FnMut(SocketAddrV4, u16, &[u8])) {
        let Some(&key) = self.by_fd.get(&fd) else {
            return;
        };
        let Some(flow) = self.flows.get_mut(&key) else {
            return;
        };
        let mut buf = [0u8; 65536];
        loop {
            match flow.sock.recv(&mut buf) {
                Ok(n) => {
                    flow.last = Instant::now();
                    if !flow.answered {
                        flow.answered = true;
                        log::debug!("UDP: {} answered the guest's port {}", key.1, key.0);
                    }
                    deliver(key.1, key.0, &buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // An ICMP error from the far end (port unreachable) surfaces here; the guest's
                // own timeouts cover it.
                Err(e) => {
                    log::debug!("UDP {} ← {}: {e}", key.0, key.1);
                    return;
                }
            }
        }
    }

    /// Ends the flows idle since before `now - IDLE`.
    pub fn expire(&mut self, now: Instant) {
        let stale: Vec<Key> = self
            .flows
            .iter()
            .filter(|(_, f)| now.duration_since(f.last) >= IDLE)
            .map(|(k, _)| *k)
            .collect();
        for key in stale {
            self.close(key);
        }
    }

    /// When the next flow goes idle, if any is open.
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
            self.close(key);
        }
    }

    fn close(&mut self, key: Key) {
        if let Some(flow) = self.flows.remove(&key) {
            self.by_fd.remove(&flow.sock.as_raw_fd());
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.flows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_readable(poller: &Poller) -> Vec<RawFd> {
        let mut fds = Vec::new();
        poller.wait(Some(Duration::from_secs(5)), &mut fds).unwrap();
        fds
    }

    #[test]
    fn a_datagram_goes_out_and_its_reply_comes_back_to_the_guest_port() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(dst) = server.local_addr().unwrap() else {
            unreachable!()
        };
        let poller = Poller::new().unwrap();
        let mut nat = UdpNat::default();
        nat.send(40000, dst, b"ping", &poller).unwrap();

        let mut buf = [0u8; 16];
        let (n, from) = server.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");
        server.send_to(b"pong", from).unwrap();

        let fds = wait_readable(&poller);
        assert_eq!(fds.len(), 1);
        assert!(nat.owns(fds[0]));
        let mut got = Vec::new();
        nat.recv(fds[0], &mut |src, port, data| {
            got.push((src, port, data.to_vec()))
        });
        assert_eq!(got, vec![(dst, 40000, b"pong".to_vec())]);

        // The same guest port and destination reuse the flow.
        nat.send(40000, dst, b"again", &poller).unwrap();
        assert_eq!(nat.len(), 1);
        let (_, from2) = server.recv_from(&mut buf).unwrap();
        assert_eq!(from2, from);
    }

    #[test]
    fn idle_flows_end() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(dst) = server.local_addr().unwrap() else {
            unreachable!()
        };
        let poller = Poller::new().unwrap();
        let mut nat = UdpNat::default();
        nat.send(1, dst, b"x", &poller).unwrap();
        let deadline = nat.next_deadline().unwrap();
        nat.expire(deadline - Duration::from_millis(1));
        assert_eq!(nat.len(), 1);
        nat.expire(deadline);
        assert_eq!(nat.len(), 0);
        assert_eq!(nat.next_deadline(), None);
    }
}
