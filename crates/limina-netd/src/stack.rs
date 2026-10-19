//! What netd answers on the guest's link itself: ARP for the gateway, and DHCP.
//!
//! Frames come in and go out with their 12-byte `virtio_net_hdr` in front. Checksums in the
//! guest's frames are not checked: with checksum offload they are partial, and the guest is the
//! only sender on the link. netd's own frames carry full checksums.

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpMessageType,
    DhcpOption, DhcpPacket, DhcpRepr, ETHERNET_HEADER_LEN, EthernetAddress, EthernetFrame,
    EthernetProtocol, EthernetRepr, IPV4_HEADER_LEN, IpAddress, IpProtocol, Ipv4Address,
    Ipv4Packet, Ipv4Repr, UDP_HEADER_LEN, UdpPacket, UdpRepr,
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

/// The netd side of the guest's link.
pub struct Stack {
    link: Link,
    /// The guest's MAC, once it has spoken.
    guest_mac: Option<EthernetAddress>,
}

impl Stack {
    pub fn new(link: Link) -> Self {
        Self {
            link,
            guest_mac: None,
        }
    }

    /// Handles one frame from the guest; every frame netd answers with is handed to `reply`.
    pub fn handle(&mut self, frame: &[u8], reply: &mut dyn FnMut(&[u8])) {
        let Some(eth) = frame
            .get(VNET_HDR_LEN..)
            .and_then(|f| EthernetFrame::new_checked(f).ok())
        else {
            return;
        };
        let Ok(repr) = EthernetRepr::parse(&eth) else {
            return;
        };
        self.guest_mac.get_or_insert(repr.src_addr);
        match repr.ethertype {
            EthernetProtocol::Arp => self.arp(eth.payload(), reply),
            EthernetProtocol::Ipv4 => self.ipv4(repr.src_addr, eth.payload(), reply),
            _ => {}
        }
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

    fn ipv4(&self, src_mac: EthernetAddress, payload: &[u8], reply: &mut dyn FnMut(&[u8])) {
        let ignore = ChecksumCapabilities::ignored();
        let Ok(packet) = Ipv4Packet::new_checked(payload) else {
            return;
        };
        let Ok(ip) = Ipv4Repr::parse(&packet, &ignore) else {
            return;
        };
        if ip.next_header != IpProtocol::Udp {
            return;
        }
        let Ok(udp_packet) = UdpPacket::new_checked(packet.payload()) else {
            return;
        };
        let Ok(udp) = UdpRepr::parse(
            &udp_packet,
            &ip.src_addr.into(),
            &ip.dst_addr.into(),
            &ignore,
        ) else {
            return;
        };
        if udp.dst_port == DHCP_SERVER_PORT {
            self.dhcp(src_mac, udp_packet.payload(), reply);
        }
    }

    fn dhcp(&self, src_mac: EthernetAddress, payload: &[u8], reply: &mut dyn FnMut(&[u8])) {
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
        reply(&self.udp_frame(
            dst_mac,
            dst_ip,
            DHCP_SERVER_PORT,
            DHCP_CLIENT_PORT,
            |p| {
                let mut packet = DhcpPacket::new_unchecked(p);
                repr.emit(&mut packet).expect("sized by buffer_len");
            },
            repr.buffer_len(),
        ))
    }

    /// A UDP datagram from the gateway, framed for the guest.
    fn udp_frame(
        &self,
        dst_mac: EthernetAddress,
        dst_ip: Ipv4Address,
        src_port: u16,
        dst_port: u16,
        payload: impl FnOnce(&mut [u8]),
        payload_len: usize,
    ) -> Vec<u8> {
        let udp_len = UDP_HEADER_LEN + payload_len;
        let mut buf = frame_buf(IPV4_HEADER_LEN + udp_len);
        let caps = ChecksumCapabilities::default();
        let mut frame = EthernetFrame::new_unchecked(&mut buf[VNET_HDR_LEN..]);
        EthernetRepr {
            src_addr: self.link.gateway_mac,
            dst_addr: dst_mac,
            ethertype: EthernetProtocol::Ipv4,
        }
        .emit(&mut frame);
        let ip = Ipv4Repr {
            src_addr: self.link.gateway,
            dst_addr: dst_ip,
            next_header: IpProtocol::Udp,
            payload_len: udp_len,
            hop_limit: 64,
        };
        let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
        ip.emit(&mut packet, &caps);
        let src: IpAddress = self.link.gateway.into();
        let dst: IpAddress = dst_ip.into();
        UdpRepr { src_port, dst_port }.emit(
            &mut UdpPacket::new_unchecked(packet.payload_mut()),
            &src,
            &dst,
            payload_len,
            payload,
            &caps,
        );
        buf
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
        stack.handle(frame, &mut |f| out.push(f.to_vec()));
        out
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
