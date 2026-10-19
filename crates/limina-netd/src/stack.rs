//! The guest's link as netd sees it: ARP for the gateway and DHCP answered here, UDP and TCP
//! handed up for netd to re-originate, and the guest-bound frames built.
//!
//! Frames come in and go out with their 12-byte `virtio_net_hdr` in front. Checksums in the
//! guest's frames are not checked: with checksum offload they are partial, and the guest is the
//! only sender on the link. netd's own frames carry full checksums.

use std::net::SocketAddrV4;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpMessageType,
    DhcpOption, DhcpPacket, DhcpRepr, ETHERNET_HEADER_LEN, EthernetAddress, EthernetFrame,
    EthernetProtocol, EthernetRepr, IPV4_HEADER_LEN, IpAddress, IpProtocol, Ipv4Address,
    Ipv4Packet, Ipv4Repr, TcpPacket, TcpRepr, UDP_HEADER_LEN, UdpPacket, UdpRepr,
};

use limina_netd_wire::VNET_HDR_LEN;

/// The link netd presents: the addresses gvproxy used, so a guest moving between the two sees
/// the same network.
#[derive(Clone, Debug)]
pub struct Link {
    pub gateway_mac: EthernetAddress,
    pub gateway: Ipv4Address,
    pub guest: Ipv4Address,
    pub netmask: Ipv4Address,
    /// Seconds.
    pub lease: u32,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            gateway_mac: EthernetAddress([0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xdd]),
            gateway: Ipv4Address::new(192, 168, 127, 1),
            guest: Ipv4Address::new(192, 168, 127, 2),
            netmask: Ipv4Address::new(255, 255, 255, 0),
            lease: 3600,
        }
    }
}

/// The guest's MTU: DHCP sets none, so the guest's default.
const MTU: usize = 1500;

/// What the guest sent that netd has to take further than its link.
#[derive(Debug, PartialEq, Eq)]
pub enum Ingress<'a> {
    Udp {
        guest_port: u16,
        dst: SocketAddrV4,
        payload: &'a [u8],
    },
    Tcp {
        guest_port: u16,
        /// The far end, as the guest addresses it.
        remote: SocketAddrV4,
        segment: TcpRepr<'a>,
    },
}

/// The netd side of the guest's link.
pub struct Stack {
    link: Link,
    /// The guest's MAC, once it has spoken.
    guest_mac: Option<EthernetAddress>,
    /// The IPv4 identification of the next datagram netd sends, which fragments share.
    ident: u16,
}

impl Stack {
    pub fn new(link: Link) -> Self {
        Self {
            link,
            guest_mac: None,
            ident: 0,
        }
    }

    pub fn link(&self) -> &Link {
        &self.link
    }

    /// Handles one frame from the guest. Frames netd answers on the link (ARP, DHCP) go to
    /// `reply`; what has to leave the link comes back.
    pub fn handle<'a>(
        &mut self,
        frame: &'a [u8],
        reply: &mut dyn FnMut(&[u8]),
    ) -> Option<Ingress<'a>> {
        let eth = EthernetFrame::new_checked(frame.get(VNET_HDR_LEN..)?).ok()?;
        let repr = EthernetRepr::parse(&eth).ok()?;
        self.guest_mac.get_or_insert(repr.src_addr);
        match repr.ethertype {
            EthernetProtocol::Arp => {
                self.arp(eth.payload(), reply);
                None
            }
            EthernetProtocol::Ipv4 => self.ipv4(repr.src_addr, eth.payload(), reply),
            _ => None,
        }
    }

    /// A UDP datagram from `src` to the guest's `guest_port`, framed (and fragmented to the MTU)
    /// for the guest; each frame goes to `out`.
    pub fn udp_to_guest(
        &mut self,
        src: SocketAddrV4,
        guest_port: u16,
        payload: &[u8],
        out: &mut dyn FnMut(&[u8]),
    ) {
        let dst_mac = self.guest_mac.unwrap_or(EthernetAddress::BROADCAST);
        let dst = SocketAddrV4::new(self.link.guest, guest_port);
        self.udp_frames(dst_mac, src, dst, payload, out);
    }

    /// A TCP segment from `remote` to the guest: `header` (whose own payload is left empty, and
    /// whose destination port is the guest's) followed by `payload`, in pieces. Whether `out` took
    /// the frame.
    pub fn tcp_to_guest(
        &mut self,
        remote: SocketAddrV4,
        header: &TcpRepr<'_>,
        payload: &[&[u8]],
        out: &mut dyn FnMut(&[u8]) -> bool,
    ) -> bool {
        debug_assert!(header.payload.is_empty());
        let data_len: usize = payload.iter().map(|p| p.len()).sum();
        let tcp_len = header.header_len() + data_len;
        let mut buf = frame_buf(IPV4_HEADER_LEN + tcp_len);
        let mut frame = EthernetFrame::new_unchecked(&mut buf[VNET_HDR_LEN..]);
        EthernetRepr {
            src_addr: self.link.gateway_mac,
            dst_addr: self.guest_mac.unwrap_or(EthernetAddress::BROADCAST),
            ethertype: EthernetProtocol::Ipv4,
        }
        .emit(&mut frame);
        let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
        Ipv4Repr {
            src_addr: *remote.ip(),
            dst_addr: self.link.guest,
            next_header: IpProtocol::Tcp,
            payload_len: tcp_len,
            hop_limit: 64,
        }
        .emit(&mut packet, &ChecksumCapabilities::default());
        let mut tcp = TcpPacket::new_unchecked(&mut packet.payload_mut()[..tcp_len]);
        let (src, dst) = (
            IpAddress::from(*remote.ip()),
            IpAddress::from(self.link.guest),
        );
        header.emit(&mut tcp, &src, &dst, &ChecksumCapabilities::ignored());
        let mut at = 0;
        for p in payload {
            tcp.payload_mut()[at..at + p.len()].copy_from_slice(p);
            at += p.len();
        }
        tcp.fill_checksum(&src, &dst);
        out(&buf)
    }

    fn arp(&self, payload: &[u8], reply: &mut dyn FnMut(&[u8])) {
        let Ok(packet) = ArpPacket::new_checked(payload) else {
            return;
        };
        let Ok(ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr,
            source_protocol_addr,
            target_protocol_addr,
            ..
        }) = ArpRepr::parse(&packet)
        else {
            return;
        };
        // Only the gateway is answered for: replying to the guest's probe of its own address
        // would read as a conflict.
        if target_protocol_addr != self.link.gateway {
            return;
        }
        let answer = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: self.link.gateway_mac,
            source_protocol_addr: self.link.gateway,
            target_hardware_addr: source_hardware_addr,
            target_protocol_addr: source_protocol_addr,
        };
        let mut buf = frame_buf(answer.buffer_len());
        let eth = EthernetRepr {
            src_addr: self.link.gateway_mac,
            dst_addr: source_hardware_addr,
            ethertype: EthernetProtocol::Arp,
        };
        let mut frame = EthernetFrame::new_unchecked(&mut buf[VNET_HDR_LEN..]);
        eth.emit(&mut frame);
        answer.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()));
        reply(&buf);
    }

    fn ipv4<'a>(
        &mut self,
        src_mac: EthernetAddress,
        payload: &'a [u8],
        reply: &mut dyn FnMut(&[u8]),
    ) -> Option<Ingress<'a>> {
        let ignore = ChecksumCapabilities::ignored();
        let packet = Ipv4Packet::new_checked(payload).ok()?;
        // The guest fragments only what exceeds its MTU, which nothing it sends through a NAT
        // should; reassembly is not worth carrying for it.
        if packet.more_frags() || packet.frag_offset() != 0 {
            log::debug!("dropping an IPv4 fragment from the guest");
            return None;
        }
        let ip = Ipv4Repr::parse(&packet, &ignore).ok()?;
        let payload: &'a [u8] = &payload[packet.header_len() as usize..][..ip.payload_len];
        match ip.next_header {
            IpProtocol::Udp => {}
            IpProtocol::Tcp => {
                let tcp = TcpPacket::new_checked(payload).ok()?;
                // SYN with FIN or RST, and port 0, do not parse: dropped.
                let segment =
                    TcpRepr::parse(&tcp, &ip.src_addr.into(), &ip.dst_addr.into(), &ignore).ok()?;
                return Some(Ingress::Tcp {
                    guest_port: segment.src_port,
                    remote: SocketAddrV4::new(ip.dst_addr, segment.dst_port),
                    segment,
                });
            }
            _ => return None,
        }
        let udp_packet = UdpPacket::new_checked(payload).ok()?;
        let udp = UdpRepr::parse(
            &udp_packet,
            &ip.src_addr.into(),
            &ip.dst_addr.into(),
            &ignore,
        )
        .ok()?;
        if udp.dst_port == DHCP_SERVER_PORT {
            self.dhcp(src_mac, &payload[UDP_HEADER_LEN..], reply);
            return None;
        }
        let data_len = (udp_packet.len() as usize).checked_sub(UDP_HEADER_LEN)?;
        Some(Ingress::Udp {
            guest_port: udp.src_port,
            dst: SocketAddrV4::new(ip.dst_addr, udp.dst_port),
            payload: payload.get(UDP_HEADER_LEN..UDP_HEADER_LEN + data_len)?,
        })
    }

    fn dhcp(&mut self, src_mac: EthernetAddress, payload: &[u8], reply: &mut dyn FnMut(&[u8])) {
        let Ok(packet) = DhcpPacket::new_checked(payload) else {
            return;
        };
        let Ok(request) = DhcpRepr::parse(&packet) else {
            return;
        };
        let answer = match request.message_type {
            DhcpMessageType::Discover => DhcpMessageType::Offer,
            DhcpMessageType::Request => {
                let wants = request
                    .requested_ip
                    .or(Some(request.client_ip).filter(|ip| !ip.is_unspecified()));
                let for_us = request
                    .server_identifier
                    .is_none_or(|s| s == self.link.gateway);
                if !for_us {
                    return;
                }
                if wants.is_some_and(|ip| ip != self.link.guest) {
                    DhcpMessageType::Nak
                } else {
                    DhcpMessageType::Ack
                }
            }
            _ => return,
        };
        let nak = answer == DhcpMessageType::Nak;
        let dns = self.link.gateway.octets();
        let options = [DhcpOption {
            kind: 6, // domain name servers
            data: &dns,
        }];
        let repr = DhcpRepr {
            message_type: answer,
            transaction_id: request.transaction_id,
            secs: 0,
            client_hardware_address: request.client_hardware_address,
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip: if nak {
                Ipv4Address::UNSPECIFIED
            } else {
                self.link.guest
            },
            server_ip: Ipv4Address::UNSPECIFIED,
            router: (!nak).then_some(self.link.gateway),
            subnet_mask: (!nak).then_some(self.link.netmask),
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast: request.broadcast,
            requested_ip: None,
            client_identifier: None,
            server_identifier: Some(self.link.gateway),
            parameter_request_list: None,
            dns_servers: None,
            max_size: None,
            lease_duration: (!nak).then_some(self.link.lease),
            renew_duration: None,
            rebind_duration: None,
            additional_options: if nak { &[] } else { &options },
        };
        // RFC 2131 §4.1: a NAK, or a client that asked for it, gets a broadcast; otherwise the
        // answer goes to the address it is being given.
        let (dst_mac, dst_ip) = if nak || request.broadcast {
            (EthernetAddress::BROADCAST, Ipv4Address::BROADCAST)
        } else {
            (src_mac, self.link.guest)
        };
        if answer == DhcpMessageType::Ack {
            log::info!(
                "DHCP: leased {} to {}",
                self.link.guest,
                request.client_hardware_address
            );
        }
        let mut msg = vec![0u8; repr.buffer_len()];
        repr.emit(&mut DhcpPacket::new_unchecked(&mut msg[..]))
            .expect("sized by buffer_len");
        let src = SocketAddrV4::new(self.link.gateway, DHCP_SERVER_PORT);
        let dst = SocketAddrV4::new(dst_ip, DHCP_CLIENT_PORT);
        self.udp_frames(dst_mac, src, dst, &msg, reply);
    }

    /// A UDP datagram from `src` to `dst` on the guest's link, in as many frames as the MTU
    /// takes.
    fn udp_frames(
        &mut self,
        dst_mac: EthernetAddress,
        src: SocketAddrV4,
        dst: SocketAddrV4,
        payload: &[u8],
        out: &mut dyn FnMut(&[u8]),
    ) {
        let caps = ChecksumCapabilities::default();
        let mut datagram = vec![0u8; UDP_HEADER_LEN + payload.len()];
        UdpRepr {
            src_port: src.port(),
            dst_port: dst.port(),
        }
        .emit(
            &mut UdpPacket::new_unchecked(&mut datagram[..]),
            &IpAddress::from(*src.ip()),
            &IpAddress::from(*dst.ip()),
            payload.len(),
            |p| p.copy_from_slice(payload),
            &caps,
        );
        let ident = self.ident;
        self.ident = self.ident.wrapping_add(1);
        // Fragments carry multiples of 8 bytes, all but the last.
        let per_frame = (MTU - IPV4_HEADER_LEN) & !7;
        let mut offset = 0;
        loop {
            let len = (datagram.len() - offset).min(per_frame);
            let more = offset + len < datagram.len();
            let mut buf = frame_buf(IPV4_HEADER_LEN + len);
            let mut frame = EthernetFrame::new_unchecked(&mut buf[VNET_HDR_LEN..]);
            EthernetRepr {
                src_addr: self.link.gateway_mac,
                dst_addr: dst_mac,
                ethertype: EthernetProtocol::Ipv4,
            }
            .emit(&mut frame);
            let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
            Ipv4Repr {
                src_addr: *src.ip(),
                dst_addr: *dst.ip(),
                next_header: IpProtocol::Udp,
                payload_len: len,
                hop_limit: 64,
            }
            .emit(&mut packet, &ChecksumCapabilities::ignored());
            packet.set_ident(ident);
            packet.set_dont_frag(!more && offset == 0);
            packet.set_more_frags(more);
            packet.set_frag_offset(offset as u16);
            packet.fill_checksum();
            packet.payload_mut()[..len].copy_from_slice(&datagram[offset..offset + len]);
            out(&buf);
            offset += len;
            if !more {
                break;
            }
        }
    }
}

/// A zeroed buffer for a frame with `l3_len` bytes after the Ethernet header, headers included.
/// The `virtio_net_hdr` stays zero: no offload, checksums complete.
fn frame_buf(l3_len: usize) -> Vec<u8> {
    vec![0u8; VNET_HDR_LEN + ETHERNET_HEADER_LEN + l3_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST_MAC: EthernetAddress = EthernetAddress([0x5a, 0x76, 0x6d, 1, 2, 3]);

    /// A frame from the guest, as its driver would send it.
    fn from_guest(eth: EthernetRepr, l3_len: usize, fill: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let mut buf = frame_buf(l3_len);
        let mut frame = EthernetFrame::new_unchecked(&mut buf[VNET_HDR_LEN..]);
        eth.emit(&mut frame);
        fill(frame.payload_mut());
        buf
    }

    fn dhcp_from_guest(message_type: DhcpMessageType, requested: Option<Ipv4Address>) -> Vec<u8> {
        let repr = DhcpRepr {
            message_type,
            transaction_id: 0x1234_5678,
            secs: 0,
            client_hardware_address: GUEST_MAC,
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip: Ipv4Address::UNSPECIFIED,
            server_ip: Ipv4Address::UNSPECIFIED,
            router: None,
            subnet_mask: None,
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast: false,
            requested_ip: requested,
            client_identifier: Some(GUEST_MAC),
            server_identifier: None,
            parameter_request_list: Some(&[1, 3, 6]),
            dns_servers: None,
            max_size: None,
            lease_duration: None,
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let udp_len = UDP_HEADER_LEN + repr.buffer_len();
        let eth = EthernetRepr {
            src_addr: GUEST_MAC,
            dst_addr: EthernetAddress::BROADCAST,
            ethertype: EthernetProtocol::Ipv4,
        };
        from_guest(eth, IPV4_HEADER_LEN + udp_len, |p| {
            let ip = Ipv4Repr {
                src_addr: Ipv4Address::UNSPECIFIED,
                dst_addr: Ipv4Address::BROADCAST,
                next_header: IpProtocol::Udp,
                payload_len: udp_len,
                hop_limit: 64,
            };
            let caps = ChecksumCapabilities::default();
            let mut packet = Ipv4Packet::new_unchecked(p);
            ip.emit(&mut packet, &caps);
            UdpRepr {
                src_port: DHCP_CLIENT_PORT,
                dst_port: DHCP_SERVER_PORT,
            }
            .emit(
                &mut UdpPacket::new_unchecked(packet.payload_mut()),
                &Ipv4Address::UNSPECIFIED.into(),
                &Ipv4Address::BROADCAST.into(),
                repr.buffer_len(),
                |d| repr.emit(&mut DhcpPacket::new_unchecked(d)).unwrap(),
                &caps,
            );
        })
    }

    /// The DHCP answer in a frame from netd, checked down to its checksums.
    fn dhcp_answer(frame: &[u8]) -> (EthernetAddress, Ipv4Address, DhcpMessageType, Ipv4Address) {
        let caps = ChecksumCapabilities::default();
        let eth = EthernetFrame::new_checked(&frame[VNET_HDR_LEN..]).unwrap();
        let packet = Ipv4Packet::new_checked(eth.payload()).unwrap();
        let ip = Ipv4Repr::parse(&packet, &caps).expect("a valid IPv4 checksum");
        let udp = UdpPacket::new_checked(packet.payload()).unwrap();
        UdpRepr::parse(&udp, &ip.src_addr.into(), &ip.dst_addr.into(), &caps)
            .expect("a valid UDP checksum");
        let dhcp = DhcpPacket::new_checked(udp.payload()).unwrap();
        let repr = DhcpRepr::parse(&dhcp).unwrap();
        assert_eq!(repr.transaction_id, 0x1234_5678);
        (eth.dst_addr(), ip.dst_addr, repr.message_type, repr.your_ip)
    }

    fn answers(stack: &mut Stack, frame: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let _ = stack.handle(frame, &mut |f| out.push(f.to_vec()));
        out
    }

    /// A UDP datagram from the guest's `port` to `dst`, as its driver would frame it.
    fn udp_from_guest(port: u16, dst: SocketAddrV4, payload: &[u8]) -> Vec<u8> {
        let link = Link::default();
        let udp_len = UDP_HEADER_LEN + payload.len();
        let eth = EthernetRepr {
            src_addr: GUEST_MAC,
            dst_addr: link.gateway_mac,
            ethertype: EthernetProtocol::Ipv4,
        };
        from_guest(eth, IPV4_HEADER_LEN + udp_len, |p| {
            let caps = ChecksumCapabilities::default();
            let mut packet = Ipv4Packet::new_unchecked(p);
            Ipv4Repr {
                src_addr: link.guest,
                dst_addr: *dst.ip(),
                next_header: IpProtocol::Udp,
                payload_len: udp_len,
                hop_limit: 64,
            }
            .emit(&mut packet, &caps);
            UdpRepr {
                src_port: port,
                dst_port: dst.port(),
            }
            .emit(
                &mut UdpPacket::new_unchecked(packet.payload_mut()),
                &IpAddress::from(link.guest),
                &IpAddress::from(*dst.ip()),
                payload.len(),
                |d| d.copy_from_slice(payload),
                &caps,
            );
        })
    }

    #[test]
    fn a_guest_datagram_is_handed_up_with_its_port_and_destination() {
        let mut stack = Stack::new(Link::default());
        let dst = SocketAddrV4::new(Ipv4Address::new(1, 1, 1, 1), 53);
        let frame = udp_from_guest(41000, dst, b"query");
        let ingress = stack.handle(&frame, &mut |_| panic!("nothing is answered on the link"));
        assert_eq!(
            ingress,
            Some(Ingress::Udp {
                guest_port: 41000,
                dst,
                payload: b"query"
            })
        );

        let mut fragment = frame.clone();
        let ip = VNET_HDR_LEN + ETHERNET_HEADER_LEN;
        fragment[ip + 6] |= 0x20; // more fragments
        assert_eq!(stack.handle(&fragment, &mut |_| {}), None);
    }

    /// A reply larger than the MTU goes to the guest in fragments that reassemble to the datagram,
    /// each with a valid header checksum, and the UDP checksum valid over the whole.
    #[test]
    fn a_large_reply_is_fragmented_to_the_mtu() {
        let link = Link::default();
        let mut stack = Stack::new(link.clone());
        let _ = stack.handle(
            &udp_from_guest(
                41000,
                SocketAddrV4::new(Ipv4Address::new(9, 9, 9, 9), 53),
                b"q",
            ),
            &mut |_| {},
        );
        let src = SocketAddrV4::new(Ipv4Address::new(9, 9, 9, 9), 53);
        let payload: Vec<u8> = (0..4000u32).map(|i| i as u8).collect();
        let mut frames = Vec::new();
        stack.udp_to_guest(src, 41000, &payload, &mut |f| frames.push(f.to_vec()));
        assert_eq!(frames.len(), 3);

        let caps = ChecksumCapabilities::default();
        let mut datagram = Vec::new();
        let mut ident = None;
        for (i, f) in frames.iter().enumerate() {
            assert!(f.len() - VNET_HDR_LEN - ETHERNET_HEADER_LEN <= MTU);
            let eth = EthernetFrame::new_checked(&f[VNET_HDR_LEN..]).unwrap();
            assert_eq!(eth.dst_addr(), GUEST_MAC);
            let packet = Ipv4Packet::new_checked(eth.payload()).unwrap();
            assert!(packet.verify_checksum(), "fragment {i}'s header checksum");
            assert_eq!(packet.frag_offset() as usize, datagram.len());
            assert_eq!(packet.more_frags(), i + 1 < frames.len());
            assert_eq!(*ident.get_or_insert(packet.ident()), packet.ident());
            datagram.extend_from_slice(packet.payload());
        }
        let udp = UdpPacket::new_checked(&datagram[..]).unwrap();
        UdpRepr::parse(
            &udp,
            &IpAddress::from(*src.ip()),
            &IpAddress::from(link.guest),
            &caps,
        )
        .expect("the UDP checksum over the reassembled datagram");
        assert_eq!(udp.dst_port(), 41000);
        assert_eq!(udp.payload(), &payload[..]);
    }

    #[test]
    fn a_guest_is_offered_and_acked_the_guest_address() {
        let mut stack = Stack::new(Link::default());
        let link = Link::default();
        let offer = answers(
            &mut stack,
            &dhcp_from_guest(DhcpMessageType::Discover, None),
        );
        assert_eq!(offer.len(), 1);
        assert_eq!(
            dhcp_answer(&offer[0]),
            (GUEST_MAC, link.guest, DhcpMessageType::Offer, link.guest)
        );
        let ack = answers(
            &mut stack,
            &dhcp_from_guest(DhcpMessageType::Request, Some(link.guest)),
        );
        assert_eq!(
            dhcp_answer(&ack[0]),
            (GUEST_MAC, link.guest, DhcpMessageType::Ack, link.guest)
        );
    }

    #[test]
    fn a_request_for_another_address_is_refused() {
        let mut stack = Stack::new(Link::default());
        let nak = answers(
            &mut stack,
            &dhcp_from_guest(
                DhcpMessageType::Request,
                Some(Ipv4Address::new(10, 0, 0, 5)),
            ),
        );
        let (mac, ip, kind, _) = dhcp_answer(&nak[0]);
        assert_eq!(
            (mac, ip, kind),
            (
                EthernetAddress::BROADCAST,
                Ipv4Address::BROADCAST,
                DhcpMessageType::Nak
            )
        );
    }

    fn arp_request(target: Ipv4Address, sender: Ipv4Address) -> Vec<u8> {
        let repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: GUEST_MAC,
            source_protocol_addr: sender,
            target_hardware_addr: EthernetAddress([0; 6]),
            target_protocol_addr: target,
        };
        let eth = EthernetRepr {
            src_addr: GUEST_MAC,
            dst_addr: EthernetAddress::BROADCAST,
            ethertype: EthernetProtocol::Arp,
        };
        from_guest(eth, repr.buffer_len(), |p| {
            repr.emit(&mut ArpPacket::new_unchecked(p))
        })
    }

    #[test]
    fn the_gateway_answers_arp_and_nothing_else_does() {
        let link = Link::default();
        let mut stack = Stack::new(link.clone());
        let reply = answers(&mut stack, &arp_request(link.gateway, link.guest));
        assert_eq!(reply.len(), 1);
        let eth = EthernetFrame::new_checked(&reply[0][VNET_HDR_LEN..]).unwrap();
        assert_eq!(eth.dst_addr(), GUEST_MAC);
        let ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_protocol_addr,
            ..
        } = ArpRepr::parse(&ArpPacket::new_checked(eth.payload()).unwrap()).unwrap()
        else {
            panic!("not an Ethernet/IPv4 ARP reply");
        };
        assert_eq!(operation, ArpOperation::Reply);
        assert_eq!(source_hardware_addr, link.gateway_mac);
        assert_eq!(source_protocol_addr, link.gateway);
        assert_eq!(target_protocol_addr, link.guest);
        // The guest's duplicate-address probe of its own address stays unanswered.
        assert!(
            answers(
                &mut stack,
                &arp_request(link.guest, Ipv4Address::UNSPECIFIED)
            )
            .is_empty()
        );
    }

    #[test]
    fn garbage_is_ignored() {
        let mut stack = Stack::new(Link::default());
        assert!(answers(&mut stack, &[]).is_empty());
        assert!(answers(&mut stack, &[0u8; 40]).is_empty());
        let mut truncated = dhcp_from_guest(DhcpMessageType::Discover, None);
        truncated.truncate(60);
        assert!(answers(&mut stack, &truncated).is_empty());
    }
}
