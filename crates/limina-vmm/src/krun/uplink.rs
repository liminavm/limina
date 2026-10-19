//! Where the host sends traffic, as the NIC's switch needs to know it
//! (`docs/design/multi-vm-networking.md` §3.4, §3.5): the route to the Internet and the route to
//! vmnet's subnet, read with `RTM_GET` on a routing socket, and a watch on the routing socket for
//! when either may have changed.
//!
//! The policy is [`wanted`]: vmnet serves unless its subnet is routed away from its bridge (a VPN
//! client's local-network route, which takes the guests' replies with it) or the way out is a
//! tunnel (under which vmnet's NAT lost forwarded UDP); then netd does.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use super::switch::Side;

/// An address on the Internet whose route is the host's way out. Nothing is sent to it.
pub const INTERNET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

/// A route the host would use for an address: the interface, and the destination and prefix of
/// the entry that matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub ifname: String,
    pub dst: Ipv4Addr,
    pub prefix: u8,
}

/// Interfaces that carry a tunnel: a VPN client's.
fn is_tunnel(ifname: &str) -> bool {
    ["utun", "ipsec", "ppp", "tun", "tap"]
        .iter()
        .any(|p| ifname.starts_with(p))
}

/// Where the NIC should be, given the route to the Internet and the route to vmnet's subnet (when
/// it is known), and why.
pub fn wanted(uplink: Option<&Route>, subnet: Option<&Route>) -> (Side, String) {
    // A route to a whole subnet or narrower is someone's decision about vmnet's addresses; the
    // default route, and a VPN's split halves of it, are not.
    if let Some(r) = subnet
        && r.prefix >= 8
        && !r.ifname.starts_with("bridge")
        && r.ifname != "lo0"
    {
        return (
            Side::Netd,
            format!(
                "vmnet's subnet is routed through {} ({}/{})",
                r.ifname, r.dst, r.prefix
            ),
        );
    }
    if let Some(r) = uplink
        && is_tunnel(&r.ifname)
    {
        return (
            Side::Netd,
            format!("the way out is the tunnel {}", r.ifname),
        );
    }
    (
        Side::Vmnet,
        match uplink {
            Some(r) => format!("the way out is {}", r.ifname),
            None => "the host has no way out".into(),
        },
    )
}

fn routing_socket() -> io::Result<OwnedFd> {
    // SAFETY: a plain syscall; the result is ours.
    let fd = unsafe { libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, libc::AF_UNSPEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: a flag change on our own descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok(sock)
}

/// A `sockaddr`'s room in a routing message: its length rounded up to 4 bytes, and 4 when 0.
fn sa_room(len: usize) -> usize {
    if len == 0 { 4 } else { 1 + ((len - 1) | 3) }
}

/// The prefix length of a routing message's netmask, which the kernel trims to the bytes that are
/// not zero.
fn prefix_of(mask: &[u8]) -> u8 {
    let bytes: Vec<u8> = (4..8).map(|i| mask.get(i).copied().unwrap_or(0)).collect();
    bytes.iter().map(|b| b.count_ones() as u8).sum()
}

/// The route the host would use for `addr`, or `None` when it has none.
pub fn route_to(addr: Ipv4Addr) -> io::Result<Option<Route>> {
    let sock = routing_socket()?;
    let hdr = size_of::<libc::rt_msghdr>();
    let sin = size_of::<libc::sockaddr_in>();
    // SAFETY: plain data.
    let mut msg: libc::rt_msghdr = unsafe { std::mem::zeroed() };
    msg.rtm_msglen = (hdr + sin) as u16;
    msg.rtm_version = libc::RTM_VERSION as u8;
    msg.rtm_type = libc::RTM_GET as u8;
    msg.rtm_addrs = libc::RTA_DST;
    // SAFETY: no preconditions.
    msg.rtm_pid = unsafe { libc::getpid() };
    msg.rtm_seq = 1;
    let dst = libc::sockaddr_in {
        sin_len: sin as u8,
        sin_family: libc::AF_INET as u8,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from(addr).to_be(),
        },
        sin_zero: [0; 8],
    };
    let mut buf = vec![0u8; 2048];
    // SAFETY: both are plain data of the sizes copied.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&msg as *const libc::rt_msghdr).cast(),
            buf.as_mut_ptr(),
            hdr,
        );
        std::ptr::copy_nonoverlapping(
            (&dst as *const libc::sockaddr_in).cast(),
            buf.as_mut_ptr().add(hdr),
            sin,
        );
    }
    // SAFETY: a message of the length its header states.
    if unsafe { libc::write(sock.as_raw_fd(), buf.as_ptr().cast(), hdr + sin) } < 0 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(e)
        };
    }
    loop {
        // SAFETY: reading into our own buffer of its stated size.
        let n = unsafe { libc::read(sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        if n < hdr {
            continue;
        }
        // SAFETY: at least a header's worth of bytes; read unaligned.
        let reply: libc::rt_msghdr = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
        if reply.rtm_type != libc::RTM_GET as u8
            || reply.rtm_pid != msg.rtm_pid
            || reply.rtm_seq != 1
        {
            continue;
        }
        let mut name = [0 as libc::c_char; libc::IF_NAMESIZE];
        // SAFETY: a buffer of IF_NAMESIZE, as if_indextoname wants.
        let ifname = unsafe { libc::if_indextoname(reply.rtm_index as u32, name.as_mut_ptr()) };
        let ifname = if ifname.is_null() {
            String::new()
        } else {
            // SAFETY: if_indextoname wrote a NUL-terminated name.
            unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        let (mut dst, mut prefix) = (Ipv4Addr::UNSPECIFIED, 32);
        let mut at = hdr;
        let mut has_mask = false;
        for bit in 0..8 {
            if reply.rtm_addrs & (1 << bit) == 0 {
                continue;
            }
            let Some(&len) = buf[..n].get(at) else { break };
            let sa = &buf[at..(at + len as usize).min(n)];
            match 1 << bit {
                libc::RTA_DST if sa.len() >= 8 && sa[1] == libc::AF_INET as u8 => {
                    dst = Ipv4Addr::new(sa[4], sa[5], sa[6], sa[7]);
                }
                libc::RTA_NETMASK => {
                    has_mask = true;
                    prefix = prefix_of(sa);
                }
                _ => {}
            }
            at += sa_room(len as usize);
        }
        // A host route carries no mask.
        if !has_mask && reply.rtm_flags & 0x4 == 0 {
            prefix = 0;
        }
        return Ok(Some(Route {
            ifname,
            dst,
            prefix,
        }));
    }
}

/// A watch on the host's routes: [`RouteWatch::wait`] returns when any may have changed.
pub struct RouteWatch(OwnedFd);

impl RouteWatch {
    pub fn new() -> io::Result<Self> {
        Ok(Self(routing_socket()?))
    }

    /// Waits up to `timeout` for a routing message, and reads every one waiting. Whether any came.
    pub fn wait(&self, timeout: Duration) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
        if ready <= 0 {
            return false;
        }
        let mut buf = [0u8; 2048];
        loop {
            // SAFETY: reading into our own buffer of its stated size, without blocking.
            let n = unsafe {
                libc::recv(
                    self.0.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                return true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(ifname: &str, dst: [u8; 4], prefix: u8) -> Route {
        Route {
            ifname: ifname.into(),
            dst: dst.into(),
            prefix,
        }
    }

    #[test]
    fn vmnet_serves_while_its_subnet_is_on_its_bridge_and_the_way_out_is_plain() {
        let (side, _) = wanted(
            Some(&route("en0", [0, 0, 0, 0], 0)),
            Some(&route("bridge100", [192, 168, 65, 0], 24)),
        );
        assert_eq!(side, Side::Vmnet);
    }

    /// The local-network route a VPN client installs for vmnet's subnet takes the guest's replies
    /// away from the bridge.
    #[test]
    fn a_subnet_routed_elsewhere_moves_the_nic_to_netd() {
        let (side, why) = wanted(
            Some(&route("en0", [0, 0, 0, 0], 0)),
            Some(&route("en0", [192, 168, 65, 0], 24)),
        );
        assert_eq!(side, Side::Netd);
        assert!(why.contains("en0"), "{why}");
    }

    /// With no interface up the subnet's address falls to the default route, which says nothing
    /// about vmnet; a VPN's split halves of the default route neither.
    #[test]
    fn the_default_route_or_its_halves_are_no_claim_on_the_subnet() {
        for subnet in [
            route("en0", [0, 0, 0, 0], 0),
            route("utun4", [128, 0, 0, 0], 1),
        ] {
            let (side, _) = wanted(Some(&route("en0", [0, 0, 0, 0], 0)), Some(&subnet));
            assert_eq!(side, Side::Vmnet, "{subnet:?}");
        }
    }

    /// The host's own address on the bridge is a local route on lo0, which is no claim either.
    #[test]
    fn a_local_route_is_no_claim_on_the_subnet() {
        let (side, _) = wanted(
            Some(&route("en0", [0, 0, 0, 0], 0)),
            Some(&route("lo0", [192, 168, 65, 1], 32)),
        );
        assert_eq!(side, Side::Vmnet);
    }

    #[test]
    fn a_tunnel_as_the_way_out_moves_the_nic_to_netd() {
        let (side, why) = wanted(Some(&route("utun4", [0, 0, 0, 0], 1)), None);
        assert_eq!(side, Side::Netd);
        assert!(why.contains("utun4"), "{why}");
    }

    #[test]
    fn netmasks_are_read_from_their_trimmed_bytes() {
        assert_eq!(prefix_of(&[0, 0]), 0);
        assert_eq!(prefix_of(&[7, 2, 0, 0, 255, 255, 255]), 24);
        assert_eq!(prefix_of(&[8, 2, 0, 0, 255, 255, 255, 255]), 32);
        assert_eq!(prefix_of(&[5, 2, 0, 0, 128]), 1);
    }

    /// The live table, read only: loopback's route is on lo0, and the way out has an interface.
    #[test]
    fn the_hosts_routes_read_back() {
        let lo = route_to(Ipv4Addr::LOCALHOST)
            .unwrap()
            .expect("a route to loopback");
        assert_eq!(lo.ifname, "lo0");
        if let Some(out) = route_to(INTERNET).unwrap() {
            assert!(!out.ifname.is_empty(), "{out:?}");
        }
    }
}
