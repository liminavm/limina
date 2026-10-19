//! The address the guest holds, as the worker learns it from the DHCP acknowledgements that reach
//! the guest's MAC, and the supervisor's watch on it (`docs/design/multi-vm-networking.md` §3.3).
//!
//! On a bridged NIC the LAN's DHCP server leases the address, so nothing on the host knows it
//! otherwise; on vmnet shared and on netd the acknowledgement passes through the worker all the
//! same, so every NIC reports the same way. [`observed`] wraps a NIC's backend to read the frames
//! it hands the guest; [`Lease`] keeps what they said and tells every watcher when it changes. A
//! NIC whose backend will not start reports why instead, so the supervisor can say it.

use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use devices::virtio::net::backend::{
    ConnectError, ExternalBackend, NetBackend, ReadError, Reconnect, WriteError,
};

/// The virtio-net header (`virtio_net_hdr_v1`) every backend's frames start with.
const VNET_HDR_LEN: usize = 12;

/// How long a push may block on a watcher that stopped reading before it is dropped.
const WATCHER_TIMEOUT: Duration = Duration::from_secs(1);

/// What the guest's NIC has, as watchers are told it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Held {
    /// No acknowledgement yet (or the NIC is not up).
    Nothing,
    /// The address the last acknowledgement gave the guest.
    Address(Ipv4Addr),
    /// The backend would not start, and why.
    Failed(String),
}

impl Held {
    /// The line a watcher reads: `address <ip>`, `address none` or `address failed <why>`.
    pub fn line(&self) -> String {
        match self {
            Held::Nothing => "address none".into(),
            Held::Address(ip) => format!("address {ip}"),
            Held::Failed(why) => format!("address failed {}", why.replace('\n', " ")),
        }
    }
}

struct State {
    held: Held,
    watchers: Vec<UnixStream>,
}

/// The guest's address, and who wants to hear about it.
pub struct Lease {
    mac: [u8; 6],
    state: Mutex<State>,
}

fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Lease {
    pub fn new(mac: [u8; 6]) -> Arc<Self> {
        Arc::new(Lease {
            mac,
            state: Mutex::new(State {
                held: Held::Nothing,
                watchers: Vec::new(),
            }),
        })
    }

    #[cfg(test)]
    fn held(&self) -> Held {
        lock(&self.state).held.clone()
    }

    /// Record what the NIC has now, and tell every watcher if it changed.
    pub fn set(&self, held: Held) {
        let mut state = lock(&self.state);
        if state.held == held {
            return;
        }
        match &held {
            Held::Address(ip) => log::info!("network: the guest holds {ip}"),
            Held::Failed(why) => log::error!("network: the NIC did not start: {why}"),
            Held::Nothing => {}
        }
        let line = format!("{}\n", held.line());
        state.held = held;
        state
            .watchers
            .retain_mut(|w| w.write_all(line.as_bytes()).is_ok());
    }

    /// Keep `stream` told: what the NIC has now, then every change, for as long as it reads.
    pub fn watch(&self, mut stream: UnixStream) -> io::Result<()> {
        stream.set_write_timeout(Some(WATCHER_TIMEOUT))?;
        let mut state = lock(&self.state);
        stream.write_all(format!("{}\n", state.held.line()).as_bytes())?;
        state.watchers.push(stream);
        Ok(())
    }

    fn saw(&self, frame: &[u8]) {
        if let Some(ip) = acked_address(frame, &self.mac) {
            self.set(Held::Address(ip));
        }
    }
}

/// The address a DHCP acknowledgement for `mac` gives, if `frame` (an Ethernet frame) is one.
pub fn acked_address(frame: &[u8], mac: &[u8; 6]) -> Option<Ipv4Addr> {
    if frame.get(12..14)? != [0x08, 0x00] {
        return None;
    }
    let ip = frame.get(14..)?;
    let ihl = usize::from(ip.first()? & 0x0f) * 4;
    if ihl < 20 || *ip.get(9)? != 17 {
        return None;
    }
    let udp = ip.get(ihl..)?;
    if udp.get(0..4)? != [0, 67, 0, 68] {
        return None;
    }
    let bootp = udp.get(8..)?;
    // A reply (op 2) for this Ethernet hardware address, in a DHCP message (the magic cookie).
    if bootp.get(0..3)? != [2, 1, 6]
        || bootp.get(28..34)? != mac
        || bootp.get(236..240)? != [99, 130, 83, 99]
    {
        return None;
    }
    let yiaddr = Ipv4Addr::new(bootp[16], bootp[17], bootp[18], bootp[19]);
    let mut options = bootp.get(240..)?;
    while let Some((&code, rest)) = options.split_first() {
        match code {
            0 => options = rest,
            255 => return None,
            _ => {
                let len = usize::from(*rest.first()?);
                let value = rest.get(1..1 + len)?;
                // DHCP message type 5: DHCPACK.
                if code == 53 {
                    return (value == [5] && !yiaddr.is_unspecified()).then_some(yiaddr);
                }
                options = &rest[1 + len..];
            }
        }
    }
    None
}

/// What a backend that would not start says about it.
pub fn reason(e: &ConnectError) -> String {
    match e {
        ConnectError::Vmnet(why) | ConnectError::External(why) => why.clone(),
        other => format!("{other:?}"),
    }
}

/// A backend whose frames to the guest are read for its lease on the way.
struct Observed {
    inner: Box<dyn NetBackend + Send>,
    lease: Arc<Lease>,
}

impl NetBackend for Observed {
    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        let n = self.inner.read_frame(buf)?;
        if let Some(frame) = buf.get(VNET_HDR_LEN..n) {
            self.lease.saw(frame);
        }
        Ok(n)
    }

    fn write_frame(&mut self, hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError> {
        self.inner.write_frame(hdr_len, buf)
    }

    fn has_unfinished_write(&self) -> bool {
        self.inner.has_unfinished_write()
    }

    fn try_finish_write(&mut self, hdr_len: usize, buf: &[u8]) -> Result<(), WriteError> {
        self.inner.try_finish_write(hdr_len, buf)
    }

    fn raw_socket_fd(&self) -> RawFd {
        self.inner.raw_socket_fd()
    }

    fn extra_fds(&self) -> Vec<RawFd> {
        self.inner.extra_fds()
    }

    fn synthesizes_vnet_hdr(&self) -> bool {
        self.inner.synthesizes_vnet_hdr()
    }

    fn write_retry_delay_us(&self) -> u64 {
        self.inner.write_retry_delay_us()
    }

    fn reconnect(&mut self) -> Reconnect {
        self.inner.reconnect()
    }
}

/// `build`, with the frames of the backend it builds read for the guest's lease, and a backend
/// that will not start reported as the NIC's failure.
pub fn observed(build: ExternalBackend, lease: Arc<Lease>) -> ExternalBackend {
    Arc::new(move |features| match build(features) {
        Ok(inner) => Ok(Box::new(Observed {
            inner,
            lease: lease.clone(),
        }) as Box<dyn NetBackend + Send>),
        Err(e) => {
            lease.set(Held::Failed(reason(&e)));
            Err(e)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x5a, 0x94, 0xef, 1, 2, 3];

    /// A DHCP reply to `chaddr` giving `yiaddr`, of message type `kind`.
    fn reply(chaddr: [u8; 6], yiaddr: [u8; 4], kind: u8) -> Vec<u8> {
        let mut f = vec![0u8; 14 + 20 + 8 + 240];
        f[0..6].copy_from_slice(&chaddr);
        f[12..14].copy_from_slice(&[0x08, 0x00]);
        f[14] = 0x45;
        f[14 + 9] = 17;
        f[34..38].copy_from_slice(&[0, 67, 0, 68]);
        let b = 42;
        f[b..b + 3].copy_from_slice(&[2, 1, 6]);
        f[b + 16..b + 20].copy_from_slice(&yiaddr);
        f[b + 28..b + 34].copy_from_slice(&chaddr);
        f[b + 236..b + 240].copy_from_slice(&[99, 130, 83, 99]);
        // A pad, a server identifier, then the message type.
        f.extend_from_slice(&[0, 54, 4, 192, 168, 13, 1, 53, 1, kind, 255]);
        f
    }

    #[test]
    fn an_ack_for_the_guest_names_its_address() {
        let f = reply(MAC, [192, 168, 13, 77], 5);
        assert_eq!(
            acked_address(&f, &MAC),
            Some(Ipv4Addr::new(192, 168, 13, 77))
        );
    }

    #[test]
    fn an_offer_or_another_guests_ack_names_none() {
        assert_eq!(
            acked_address(&reply(MAC, [192, 168, 13, 77], 2), &MAC),
            None
        );
        let other = [0x5a, 0x94, 0xef, 9, 9, 9];
        assert_eq!(
            acked_address(&reply(other, [192, 168, 13, 78], 5), &MAC),
            None
        );
    }

    #[test]
    fn a_truncated_or_foreign_frame_names_none() {
        let f = reply(MAC, [192, 168, 13, 77], 5);
        for n in [0, 13, 40, 100, f.len() - 4] {
            assert_eq!(acked_address(&f[..n], &MAC), None, "{n} bytes");
        }
        let mut arp = f.clone();
        arp[12..14].copy_from_slice(&[0x08, 0x06]);
        assert_eq!(acked_address(&arp, &MAC), None);
    }

    #[test]
    fn a_watcher_hears_the_address_now_and_each_change() {
        use std::io::{BufRead, BufReader};
        let lease = Lease::new(MAC);
        let (ours, theirs) = UnixStream::pair().unwrap();
        lease.watch(ours).unwrap();
        lease.saw(&reply(MAC, [192, 168, 13, 77], 5));
        lease.saw(&reply(MAC, [192, 168, 13, 77], 5));
        lease.set(Held::Failed("vmnet said\nno".into()));
        let mut lines = BufReader::new(theirs).lines();
        let mut next = || lines.next().unwrap().unwrap();
        assert_eq!(next(), "address none");
        assert_eq!(next(), "address 192.168.13.77");
        assert_eq!(next(), "address failed vmnet said no");
    }

    #[test]
    fn a_backend_that_will_not_start_is_the_nics_failure() {
        let lease = Lease::new(MAC);
        let refuse: ExternalBackend = Arc::new(|_| {
            Err(ConnectError::Vmnet(
                "vmnet_start_interface failed with VMNET_NOT_AUTHORIZED (1010)".into(),
            ))
        });
        assert!(observed(refuse, lease.clone())(0).is_err());
        assert_eq!(
            lease.held(),
            Held::Failed("vmnet_start_interface failed with VMNET_NOT_AUTHORIZED (1010)".into())
        );
    }
}
