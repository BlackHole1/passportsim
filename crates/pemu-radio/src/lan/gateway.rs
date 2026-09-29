//! The virtual LAN's gateway: DHCP server, ARP, DNS, ICMP echo and the scripted services.
//!
//! [`Lan::input`] answers a frame from `esp_wifi_internal_tx` at once; the driver delivers each
//! answer through the RX data plane with the usual worker hop, so no answer reaches the guest
//! inside the call that sent the frame.
//!
//! The protocols are written here over `smoltcp::wire` because a snapshot must carry the state at
//! any instant, half-open TCP included: `smoltcp`'s `Interface` and sockets cannot be serialized,
//! and it has no DHCP server. The gateway only answers, never sends unprompted, so it needs only
//! server halves.

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, DhcpMessageType, DhcpOption, DhcpPacket, DhcpRepr,
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, Icmpv4Packet, Icmpv4Repr,
    IpAddress, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
    TcpSeqNumber, UdpPacket, UdpRepr,
};

use super::bridge::{self, Bridge};
use super::services;
use pemu_core::snap::snap_struct;

/// Also the DNS server. `10.23.0.0/24` rather than `192.168.4.0/24`, the ESP SoftAP default an
/// APSTA guest would collide with (class C).
pub const GATEWAY_IP: [u8; 4] = [10, 23, 0, 1];
pub const NETMASK: [u8; 4] = [255, 255, 255, 0];
pub const POOL_START: [u8; 4] = [10, 23, 0, 100];
pub const POOL_SIZE: u8 = 100;
/// The `02:00:00` prefix marks a synthesized address.
pub const GATEWAY_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x4C, 0x41, 0x4E];
pub const LEASE_S: u32 = 7_200;
/// An Ethernet MTU of 1,500 less 40 octets of IPv4 and TCP header.
pub const GATEWAY_MSS: u16 = 1_460;
/// The gateway consumes every segment at once, so this only bounds how far the guest sends
/// ahead of an acknowledgement (class C).
pub const GATEWAY_WINDOW: u16 = 8_192;
/// The MSS RFC 9293 section 3.7.1 assumes for a peer that announced none.
pub const DEFAULT_MSS: u16 = 536;
/// Deterministic, plus [`ISN_STEP`] per connection, rather than the unpredictable ISN RFC 6528
/// recommends: determinism is the emulator's contract and nothing off the virtual LAN can reach
/// these connections.
pub const ISN_BASE: u32 = 0x6000_0000;
pub const ISN_STEP: u32 = 0x0001_0000;
pub const TTL: u8 = 64;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lease {
    pub mac: [u8; 6],
    pub ip: [u8; 4],
    /// Whether an ACK has bound it (a RELEASE unbinds it).
    pub bound: bool,
}

snap_struct!(Lease { mac, ip, bound });

pub mod tcp_state {
    /// The SYN-ACK is out, its ACK is not in.
    pub const SYN_RECEIVED: u8 = 0;
    pub const ESTABLISHED: u8 = 1;
    /// The client's FIN is in and the gateway's is out, its ACK is not in.
    pub const LAST_ACK: u8 = 2;
    /// A bridged SYN is held while the relay opens the host socket.
    pub const SYN_PENDING: u8 = 3;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TcpConn {
    pub peer_mac: [u8; 6],
    pub peer_ip: [u8; 4],
    pub peer_port: u16,
    pub local_port: u16,
    pub state: u8,
    pub iss: u32,
    pub snd_una: u32,
    pub snd_nxt: u32,
    pub rcv_nxt: u32,
    pub peer_window: u16,
    pub peer_mss: u16,
    pub request: Vec<u8>,
    pub response: Vec<u8>,
    pub answered: bool,
    pub fin_received: bool,
    pub fin_sent: bool,
    /// The bridge stream carrying this connection, 0 for a scripted service.
    pub stream: u32,
    /// The `DATA` packets the relay will still take on the stream (WISP `CONTINUE` credit).
    pub credit: u32,
    pub host_closed: bool,
}

snap_struct!(TcpConn {
    peer_mac,
    peer_ip,
    peer_port,
    local_port,
    state,
    iss,
    snd_una,
    snd_nxt,
    rcv_nxt,
    peer_window,
    peer_mss,
    request,
    response,
    answered,
    fin_received,
    fin_sent,
    stream,
    credit,
    host_closed
});

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LanCounters {
    pub frames_in: u64,
    pub frames_out: u64,
    pub arp_replies: u64,
    pub dhcp_offers: u64,
    pub dhcp_acks: u64,
    pub dhcp_naks: u64,
    pub dhcp_releases: u64,
    pub dhcp_declines: u64,
    pub dns_answers: u64,
    pub icmp_echoes: u64,
    pub tcp_opened: u64,
    pub http_answers: u64,
    /// RSTs sent to a segment no connection or service took.
    pub tcp_resets: u64,
    /// Frames no service took: another host's address, another protocol, or malformed.
    pub unhandled: u64,
}

snap_struct!(LanCounters {
    frames_in,
    frames_out,
    arp_replies,
    dhcp_offers,
    dhcp_acks,
    dhcp_naks,
    dhcp_releases,
    dhcp_declines,
    dns_answers,
    icmp_echoes,
    tcp_opened,
    http_answers,
    tcp_resets,
    unhandled
});

/// Part of the Wi-Fi module state, so part of every snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lan {
    pub leases: Vec<Lease>,
    pub tcp: Vec<TcpConn>,
    /// Connections ever opened, which steps the initial sequence number.
    pub tcp_serial: u32,
    pub ip_id: u16,
    /// Virtual microseconds of the first DHCP ACK, 0 for none: the guest's `GOT_IP` is measured
    /// against it.
    pub first_ack_us: u64,
    pub counters: LanCounters,
    pub bridge: Bridge,
}

snap_struct!(Lan {
    leases,
    tcp,
    tcp_serial,
    ip_id,
    first_ack_us,
    counters,
    bridge
});

fn ip(a: [u8; 4]) -> Ipv4Address {
    Ipv4Address::new(a[0], a[1], a[2], a[3])
}

fn mac(a: [u8; 6]) -> EthernetAddress {
    EthernetAddress(a)
}

impl Lan {
    pub fn input(&mut self, now_us: u64, frame: &[u8]) -> Vec<Vec<u8>> {
        self.counters.frames_in += 1;
        let out = self.dispatch(now_us, frame).unwrap_or_default();
        self.counters.frames_out += out.len() as u64;
        out
    }

    fn dispatch(&mut self, now_us: u64, frame: &[u8]) -> Option<Vec<Vec<u8>>> {
        let eth = EthernetFrame::new_checked(frame).ok()?;
        let repr = EthernetRepr::parse(&eth).ok()?;
        let handled = match repr.ethertype {
            EthernetProtocol::Arp => self.arp(eth.payload()),
            EthernetProtocol::Ipv4 => self.ipv4(now_us, repr.src_addr.0, eth.payload()),
            _ => None,
        };
        if handled.is_none() {
            self.counters.unhandled += 1;
        }
        handled
    }

    /// Answers an ARP request for the gateway's own address, and nothing else.
    fn arp(&mut self, payload: &[u8]) -> Option<Vec<Vec<u8>>> {
        let packet = ArpPacket::new_checked(payload).ok()?;
        let ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_protocol_addr,
            ..
        } = ArpRepr::parse(&packet).ok()?
        else {
            return None;
        };
        if operation != ArpOperation::Request || target_protocol_addr != ip(GATEWAY_IP) {
            // Includes lwIP's ARP check of its offered address, which must find it free.
            return Some(Vec::new());
        }
        let reply = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: mac(GATEWAY_MAC),
            source_protocol_addr: ip(GATEWAY_IP),
            target_hardware_addr: source_hardware_addr,
            target_protocol_addr: source_protocol_addr,
        };
        let mut body = vec![0u8; reply.buffer_len()];
        reply.emit(&mut ArpPacket::new_unchecked(&mut body[..]));
        self.counters.arp_replies += 1;
        Some(vec![ethernet(
            source_hardware_addr.0,
            EthernetProtocol::Arp,
            &body,
        )])
    }

    fn ipv4(&mut self, now_us: u64, src_mac: [u8; 6], payload: &[u8]) -> Option<Vec<Vec<u8>>> {
        let caps = ChecksumCapabilities::default();
        let packet = Ipv4Packet::new_checked(payload).ok()?;
        let repr = Ipv4Repr::parse(&packet, &caps).ok()?;
        let dst = repr.dst_addr.octets();
        let to_gateway = dst == GATEWAY_IP;
        let broadcast = dst == [255, 255, 255, 255] || dst == [10, 23, 0, 255];
        if !to_gateway && !broadcast {
            // Another host: the LAN has none, and only the bridge routes off it.
            return None;
        }
        let src = repr.src_addr.octets();
        let body = &packet.payload()[..repr.payload_len.min(packet.payload().len())];
        match repr.next_header {
            IpProtocol::Udp => {
                let udp = UdpPacket::new_checked(body).ok()?;
                let urepr = UdpRepr::parse(
                    &udp,
                    &IpAddress::Ipv4(repr.src_addr),
                    &IpAddress::Ipv4(repr.dst_addr),
                    &caps,
                )
                .ok()?;
                match urepr.dst_port {
                    smoltcp::wire::DHCP_SERVER_PORT => self.dhcp(now_us, udp.payload()),
                    services::DNS_PORT if to_gateway => {
                        let answer = services::dns_answer(udp.payload())?;
                        self.counters.dns_answers += 1;
                        let datagram = udp_datagram(
                            GATEWAY_IP,
                            src,
                            services::DNS_PORT,
                            urepr.src_port,
                            &answer,
                        );
                        Some(vec![self.ipv4_frame(
                            src_mac,
                            src,
                            IpProtocol::Udp,
                            &datagram,
                        )])
                    }
                    _ => None,
                }
            }
            IpProtocol::Icmp if to_gateway => {
                let icmp = Icmpv4Packet::new_checked(body).ok()?;
                let Icmpv4Repr::EchoRequest {
                    ident,
                    seq_no,
                    data,
                } = Icmpv4Repr::parse(&icmp, &caps).ok()?
                else {
                    return None;
                };
                let reply = Icmpv4Repr::EchoReply {
                    ident,
                    seq_no,
                    data,
                };
                let mut bytes = vec![0u8; reply.buffer_len()];
                reply.emit(&mut Icmpv4Packet::new_unchecked(&mut bytes[..]), &caps);
                self.counters.icmp_echoes += 1;
                Some(vec![self.ipv4_frame(
                    src_mac,
                    src,
                    IpProtocol::Icmp,
                    &bytes,
                )])
            }
            IpProtocol::Tcp if to_gateway => self.tcp(src_mac, src, body),
            _ => None,
        }
    }

    /// The DHCP server (RFC 2131). DISCOVER gets an OFFER, REQUEST of that address an ACK, of any
    /// other a NAK; RELEASE frees the binding and DECLINE is counted. A REQUEST naming another
    /// server is ignored (section 4.3.2). The reply goes to `ciaddr` when the client has one,
    /// broadcast when the client set the broadcast bit, and otherwise unicast to the client's
    /// hardware address and `yiaddr`, which lwIP accepts before its address is configured; a NAK is
    /// always broadcast (section 4.1).
    fn dhcp(&mut self, now_us: u64, payload: &[u8]) -> Option<Vec<Vec<u8>>> {
        let packet = DhcpPacket::new_checked(payload).ok()?;
        let request = DhcpRepr::parse(&packet).ok()?;
        let client = request.client_hardware_address.0;
        if request.relay_agent_ip != Ipv4Address::UNSPECIFIED {
            // A relayed request: the LAN is one link and has no relay.
            return None;
        }
        if let Some(server) = request.server_identifier
            && server != ip(GATEWAY_IP)
        {
            // RFC 2131 section 4.3.2: the client chose another server.
            return Some(Vec::new());
        }
        let reply_type = match request.message_type {
            DhcpMessageType::Discover => {
                self.lease_for(client)?;
                DhcpMessageType::Offer
            }
            DhcpMessageType::Request => {
                let lease = self.lease_for(client)?;
                let asked = request
                    .requested_ip
                    .or((request.client_ip != Ipv4Address::UNSPECIFIED)
                        .then_some(request.client_ip));
                if asked == Some(ip(lease)) {
                    DhcpMessageType::Ack
                } else {
                    DhcpMessageType::Nak
                }
            }
            DhcpMessageType::Release => {
                if let Some(l) = self.leases.iter_mut().find(|l| l.mac == client) {
                    l.bound = false;
                }
                self.counters.dhcp_releases += 1;
                return Some(Vec::new());
            }
            DhcpMessageType::Decline => {
                self.counters.dhcp_declines += 1;
                return Some(Vec::new());
            }
            _ => return None,
        };
        let lease = self.lease_for(client)?;
        let dns = GATEWAY_IP;
        let options = [DhcpOption {
            kind: 6,
            data: &dns,
        }];
        let nak = reply_type == DhcpMessageType::Nak;
        let reply = DhcpRepr {
            message_type: reply_type,
            transaction_id: request.transaction_id,
            secs: 0,
            client_hardware_address: request.client_hardware_address,
            client_ip: if reply_type == DhcpMessageType::Ack {
                request.client_ip
            } else {
                Ipv4Address::UNSPECIFIED
            },
            your_ip: if nak {
                Ipv4Address::UNSPECIFIED
            } else {
                ip(lease)
            },
            server_ip: Ipv4Address::UNSPECIFIED,
            router: (!nak).then(|| ip(GATEWAY_IP)),
            subnet_mask: (!nak).then(|| ip(NETMASK)),
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast: request.broadcast,
            requested_ip: None,
            client_identifier: None,
            server_identifier: Some(ip(GATEWAY_IP)),
            parameter_request_list: None,
            dns_servers: None,
            max_size: None,
            lease_duration: (!nak).then_some(LEASE_S),
            renew_duration: None,
            rebind_duration: None,
            additional_options: if nak { &[] } else { &options },
        };
        let mut body = vec![0u8; reply.buffer_len()];
        reply
            .emit(&mut DhcpPacket::new_unchecked(&mut body[..]))
            .ok()?;
        match reply_type {
            DhcpMessageType::Offer => self.counters.dhcp_offers += 1,
            DhcpMessageType::Ack => {
                self.counters.dhcp_acks += 1;
                if let Some(l) = self.leases.iter_mut().find(|l| l.mac == client) {
                    l.bound = true;
                }
                if self.first_ack_us == 0 {
                    self.first_ack_us = now_us.max(1);
                }
            }
            _ => self.counters.dhcp_naks += 1,
        }
        let (dst_mac, dst_ip) = if nak || request.broadcast {
            ([0xFF; 6], [255, 255, 255, 255])
        } else if request.client_ip != Ipv4Address::UNSPECIFIED {
            (client, request.client_ip.octets())
        } else {
            (client, lease)
        };
        let datagram = udp_datagram(
            GATEWAY_IP,
            dst_ip,
            smoltcp::wire::DHCP_SERVER_PORT,
            smoltcp::wire::DHCP_CLIENT_PORT,
            &body,
        );
        Some(vec![self.ipv4_frame(
            dst_mac,
            dst_ip,
            IpProtocol::Udp,
            &datagram,
        )])
    }

    fn lease_for(&mut self, client: [u8; 6]) -> Option<[u8; 4]> {
        if let Some(l) = self.leases.iter().find(|l| l.mac == client) {
            return Some(l.ip);
        }
        let n = u8::try_from(self.leases.len()).ok()?;
        if n >= POOL_SIZE {
            return None;
        }
        let ip = [
            POOL_START[0],
            POOL_START[1],
            POOL_START[2],
            POOL_START[3] + n,
        ];
        self.leases.push(Lease {
            mac: client,
            ip,
            bound: false,
        });
        Some(ip)
    }

    /// The TCP server half (RFC 9293). A SYN to a service port gets a SYN-ACK; a segment for no
    /// connection gets a RST. The answer goes out within the peer's MSS and window as it
    /// acknowledges. The server never closes first (HTTP/1.1 keeps the connection). The link is
    /// lossless, so nothing is retransmitted; a frame the guest's RX capacity drops is recovered by
    /// the guest's own retransmission.
    fn tcp(&mut self, src_mac: [u8; 6], src: [u8; 4], body: &[u8]) -> Option<Vec<Vec<u8>>> {
        let caps = ChecksumCapabilities::default();
        let packet = TcpPacket::new_checked(body).ok()?;
        let seg = TcpRepr::parse(
            &packet,
            &IpAddress::Ipv4(ip(src)),
            &IpAddress::Ipv4(ip(GATEWAY_IP)),
            &caps,
        )
        .ok()?;
        let at = self.tcp.iter().position(|c| {
            c.peer_ip == src && c.peer_port == seg.src_port && c.local_port == seg.dst_port
        });
        let Some(at) = at else {
            return Some(self.tcp_unconnected(src_mac, src, &seg));
        };
        if seg.control == TcpControl::Rst {
            let conn = self.tcp.remove(at);
            if conn.stream != 0 && !conn.host_closed {
                self.bridge.counters.closed_by_guest += 1;
                self.bridge
                    .push(bridge::close(conn.stream, bridge::reason::VOLUNTARY));
            }
            return Some(Vec::new());
        }
        let mut conn = self.tcp.remove(at);
        let mut out = Vec::new();
        if conn.state == tcp_state::SYN_PENDING {
            // The relay has not answered yet: a retransmitted SYN waits with the first, and
            // nothing else is valid before the handshake.
            self.tcp.insert(at, conn);
            return Some(out);
        }
        if seg.control == TcpControl::Syn {
            // A retransmitted SYN: the SYN-ACK was lost to the guest, so it goes again.
            if conn.state == tcp_state::SYN_RECEIVED {
                out.push(self.segment(&conn, conn.iss, TcpControl::Syn, &[]));
            }
            self.tcp.insert(at, conn);
            return Some(out);
        }
        let seq = seg.seq_number.0 as u32;
        if let Some(ack) = seg.ack_number {
            let ack = ack.0 as u32;
            let in_flight = conn.snd_nxt.wrapping_sub(conn.snd_una);
            let acked = ack.wrapping_sub(conn.snd_una);
            if acked <= in_flight {
                conn.snd_una = ack;
            }
            if conn.state == tcp_state::SYN_RECEIVED && ack == conn.iss.wrapping_add(1) {
                conn.state = tcp_state::ESTABLISHED;
            }
        }
        conn.peer_window = seg.window_len;
        let mut need_ack = false;
        if !seg.payload.is_empty() {
            need_ack = true;
            if seq == conn.rcv_nxt && !conn.fin_received {
                if conn.stream == 0 {
                    conn.request.extend_from_slice(seg.payload);
                    conn.rcv_nxt = conn.rcv_nxt.wrapping_add(seg.payload.len() as u32);
                } else if conn.host_closed {
                    // The host is gone: nothing can carry the bytes, and a RST says so.
                    return Some(vec![self.reset(&conn)]);
                } else if conn.credit == 0 {
                    // No credit: the segment is not taken and the guest's TCP retransmits it, which
                    // is the backpressure of the relay's `CONTINUE`.
                    self.bridge.counters.held += 1;
                } else {
                    conn.credit -= 1;
                    self.bridge.counters.bytes_out += seg.payload.len() as u64;
                    self.bridge
                        .push(bridge::encode(bridge::kind::DATA, conn.stream, seg.payload));
                    conn.rcv_nxt = conn.rcv_nxt.wrapping_add(seg.payload.len() as u32);
                }
            }
        }
        if seg.control == TcpControl::Fin
            && seq.wrapping_add(seg.payload.len() as u32) == conn.rcv_nxt
            && !conn.fin_received
        {
            conn.fin_received = true;
            conn.rcv_nxt = conn.rcv_nxt.wrapping_add(1);
            need_ack = true;
            if conn.stream != 0 && !conn.host_closed {
                // WISP has no half-close: the guest's FIN closes the stream.
                self.bridge.counters.closed_by_guest += 1;
                self.bridge
                    .push(bridge::close(conn.stream, bridge::reason::VOLUNTARY));
                conn.host_closed = true;
            }
        }
        if conn.stream == 0
            && !conn.answered
            && conn.state == tcp_state::ESTABLISHED
            && let Some(answer) = services::http_answer(&conn.request)
        {
            conn.response = answer;
            conn.answered = true;
            self.counters.http_answers += 1;
        }
        if self.flush(&mut conn, &mut out) {
            need_ack = false;
        }
        if need_ack {
            out.push(self.segment(&conn, conn.snd_nxt, TcpControl::None, &[]));
        }
        let closed = conn.fin_sent && conn.fin_received && conn.snd_una == conn.snd_nxt;
        if !closed {
            self.tcp.insert(at, conn);
        }
        Some(out)
    }

    /// Sends what the peer's window allows of the answer, then the gateway's FIN once the answer is
    /// all out and either side has closed. Returns whether anything was sent (which acknowledges
    /// everything received).
    fn flush(&mut self, conn: &mut TcpConn, out: &mut Vec<Vec<u8>>) -> bool {
        let sent_so_far = |c: &TcpConn| c.snd_nxt.wrapping_sub(c.iss.wrapping_add(1)) as usize;
        if conn.state == tcp_state::SYN_PENDING || conn.state == tcp_state::SYN_RECEIVED {
            return false;
        }
        let mut sent = false;
        loop {
            let off = sent_so_far(conn);
            if off >= conn.response.len() || conn.fin_sent {
                break;
            }
            let in_flight = conn.snd_nxt.wrapping_sub(conn.snd_una) as usize;
            let room = usize::from(conn.peer_window).saturating_sub(in_flight);
            let len = (conn.response.len() - off)
                .min(usize::from(conn.peer_mss))
                .min(room);
            if len == 0 {
                break;
            }
            let data = conn.response[off..off + len].to_vec();
            out.push(self.segment(conn, conn.snd_nxt, TcpControl::Psh, &data));
            conn.snd_nxt = conn.snd_nxt.wrapping_add(len as u32);
            sent = true;
        }
        let closing = conn.fin_received || (conn.stream != 0 && conn.host_closed);
        if closing && !conn.fin_sent && sent_so_far(conn) >= conn.response.len() {
            out.push(self.segment(conn, conn.snd_nxt, TcpControl::Fin, &[]));
            conn.snd_nxt = conn.snd_nxt.wrapping_add(1);
            conn.fin_sent = true;
            conn.state = tcp_state::LAST_ACK;
            sent = true;
        }
        sent
    }

    fn reset(&mut self, conn: &TcpConn) -> Vec<u8> {
        self.segment(conn, conn.snd_nxt, TcpControl::Rst, &[])
    }

    pub fn bridge_attach(&mut self, routes: &[pemu_core::input::NetRoute]) {
        self.bridge.attach(routes);
    }

    /// A detach is a link-down: every bridged connection is reset towards the guest and its stream
    /// closed towards the host. Returns the RSTs.
    pub fn bridge_detach(&mut self) -> Vec<Vec<u8>> {
        let (bridged, kept): (Vec<TcpConn>, Vec<TcpConn>) = std::mem::take(&mut self.tcp)
            .into_iter()
            .partition(|c| c.stream != 0);
        self.tcp = kept;
        let mut out = Vec::new();
        for conn in bridged {
            if !conn.host_closed {
                self.bridge
                    .push(bridge::close(conn.stream, bridge::reason::VOLUNTARY));
            }
            out.push(self.reset(&conn));
        }
        self.bridge.detach();
        self.counters.frames_out += out.len() as u64;
        out
    }

    pub fn bridge_packet(&mut self, seq: u64, packet: &[u8]) -> Vec<Vec<u8>> {
        self.bridge.take_seq(seq);
        let Some((kind, stream, payload)) = bridge::decode(packet) else {
            self.bridge.counters.stray += 1;
            return Vec::new();
        };
        let credit =
            |p: &[u8]| (p.len() >= 4).then(|| u32::from_le_bytes([p[0], p[1], p[2], p[3]]));
        if stream == 0 {
            match (kind, credit(payload)) {
                (bridge::kind::CONTINUE, Some(c)) => self.bridge.credit0 = c,
                _ => self.bridge.counters.stray += 1,
            }
            return Vec::new();
        }
        let Some(at) = self.tcp.iter().position(|c| c.stream == stream) else {
            // A stream the guest already closed or reset: the relay's answer crossed ours.
            self.bridge.counters.stray += 1;
            return Vec::new();
        };
        let mut conn = self.tcp.remove(at);
        let mut out = Vec::new();
        let pending = conn.state == tcp_state::SYN_PENDING;
        if pending && (kind == bridge::kind::CONTINUE || kind == bridge::kind::DATA) {
            conn.state = tcp_state::SYN_RECEIVED;
            let synack = self.segment(&conn, conn.iss, TcpControl::Syn, &[]);
            out.push(synack);
        }
        match kind {
            bridge::kind::CONTINUE => match credit(payload) {
                Some(c) => conn.credit = c,
                None => self.bridge.counters.stray += 1,
            },
            bridge::kind::DATA => {
                self.bridge.counters.bytes_in += payload.len() as u64;
                if !conn.fin_sent {
                    conn.response.extend_from_slice(payload);
                }
            }
            bridge::kind::CLOSE if pending => {
                self.bridge.counters.refused += 1;
                let rst = self.reset(&conn);
                out.push(rst);
                self.counters.frames_out += out.len() as u64;
                return out;
            }
            bridge::kind::CLOSE => {
                if !conn.host_closed {
                    self.bridge.counters.closed_by_host += 1;
                }
                conn.host_closed = true;
            }
            _ => self.bridge.counters.stray += 1,
        }
        self.flush(&mut conn, &mut out);
        let closed = conn.fin_sent && conn.fin_received && conn.snd_una == conn.snd_nxt;
        if !closed {
            self.tcp.insert(at, conn);
        }
        self.counters.frames_out += out.len() as u64;
        out
    }

    /// A SYN to a service opens a connection; anything else but a RST gets a RST (RFC 9293
    /// section 3.10.7.1).
    fn tcp_unconnected(
        &mut self,
        src_mac: [u8; 6],
        src: [u8; 4],
        seg: &TcpRepr<'_>,
    ) -> Vec<Vec<u8>> {
        if seg.control == TcpControl::Rst {
            return Vec::new();
        }
        let bridged = self.bridge.routes_port(seg.dst_port);
        if seg.control == TcpControl::Syn
            && seg.ack_number.is_none()
            && (bridged || seg.dst_port == services::HTTP_PORT)
        {
            let iss = ISN_BASE.wrapping_add(ISN_STEP.wrapping_mul(self.tcp_serial));
            self.tcp_serial = self.tcp_serial.wrapping_add(1);
            let conn = TcpConn {
                peer_mac: src_mac,
                peer_ip: src,
                peer_port: seg.src_port,
                local_port: seg.dst_port,
                state: tcp_state::SYN_RECEIVED,
                iss,
                snd_una: iss,
                snd_nxt: iss.wrapping_add(1),
                rcv_nxt: (seg.seq_number.0 as u32).wrapping_add(1),
                peer_window: seg.window_len,
                peer_mss: seg
                    .max_seg_size
                    .unwrap_or(DEFAULT_MSS)
                    .clamp(1, GATEWAY_MSS),
                ..TcpConn::default()
            };
            self.counters.tcp_opened += 1;
            if bridged {
                let mut conn = conn;
                conn.state = tcp_state::SYN_PENDING;
                conn.stream = self.bridge.open(seg.dst_port);
                conn.credit = self.bridge.credit0;
                self.tcp.push(conn);
                return Vec::new();
            }
            let synack = self.segment(&conn, iss, TcpControl::Syn, &[]);
            self.tcp.push(conn);
            return vec![synack];
        }
        self.counters.tcp_resets += 1;
        let seg_len = seg.payload.len() as u32
            + u32::from(matches!(seg.control, TcpControl::Syn | TcpControl::Fin));
        let (seq, ack) = match seg.ack_number {
            Some(ack) => (ack.0 as u32, None),
            None => (0, Some((seg.seq_number.0 as u32).wrapping_add(seg_len))),
        };
        let rst = TcpRepr {
            src_port: seg.dst_port,
            dst_port: seg.src_port,
            control: TcpControl::Rst,
            seq_number: TcpSeqNumber(seq as i32),
            ack_number: ack.map(|a| TcpSeqNumber(a as i32)),
            window_len: 0,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 3],
            timestamp: None,
            payload: &[],
        };
        let bytes = tcp_bytes(&rst, src);
        vec![self.ipv4_frame(src_mac, src, IpProtocol::Tcp, &bytes)]
    }

    fn segment(&mut self, conn: &TcpConn, seq: u32, control: TcpControl, data: &[u8]) -> Vec<u8> {
        let repr = TcpRepr {
            src_port: conn.local_port,
            dst_port: conn.peer_port,
            control,
            seq_number: TcpSeqNumber(seq as i32),
            ack_number: Some(TcpSeqNumber(conn.rcv_nxt as i32)),
            window_len: GATEWAY_WINDOW,
            window_scale: None,
            max_seg_size: (control == TcpControl::Syn).then_some(GATEWAY_MSS),
            sack_permitted: false,
            sack_ranges: [None; 3],
            timestamp: None,
            payload: data,
        };
        let bytes = tcp_bytes(&repr, conn.peer_ip);
        self.ipv4_frame(conn.peer_mac, conn.peer_ip, IpProtocol::Tcp, &bytes)
    }

    fn ipv4_frame(
        &mut self,
        dst_mac: [u8; 6],
        dst_ip: [u8; 4],
        protocol: IpProtocol,
        payload: &[u8],
    ) -> Vec<u8> {
        let repr = Ipv4Repr {
            src_addr: ip(GATEWAY_IP),
            dst_addr: ip(dst_ip),
            next_header: protocol,
            payload_len: payload.len(),
            hop_limit: TTL,
        };
        let mut datagram = vec![0u8; repr.buffer_len() + payload.len()];
        let mut packet = Ipv4Packet::new_unchecked(&mut datagram[..]);
        repr.emit(&mut packet, &ChecksumCapabilities::ignored());
        packet.set_ident(self.ip_id);
        packet.set_dont_frag(true);
        self.ip_id = self.ip_id.wrapping_add(1);
        packet.payload_mut().copy_from_slice(payload);
        packet.fill_checksum();
        ethernet(dst_mac, EthernetProtocol::Ipv4, &datagram)
    }
}

fn ethernet(dst: [u8; 6], ethertype: EthernetProtocol, payload: &[u8]) -> Vec<u8> {
    let repr = EthernetRepr {
        src_addr: mac(GATEWAY_MAC),
        dst_addr: mac(dst),
        ethertype,
    };
    let mut frame = vec![0u8; repr.buffer_len() + payload.len()];
    let mut eth = EthernetFrame::new_unchecked(&mut frame[..]);
    repr.emit(&mut eth);
    eth.payload_mut().copy_from_slice(payload);
    frame
}

fn udp_datagram(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, data: &[u8]) -> Vec<u8> {
    let repr = UdpRepr {
        src_port: sport,
        dst_port: dport,
    };
    let mut bytes = vec![0u8; repr.header_len() + data.len()];
    repr.emit(
        &mut UdpPacket::new_unchecked(&mut bytes[..]),
        &IpAddress::Ipv4(ip(src)),
        &IpAddress::Ipv4(ip(dst)),
        data.len(),
        |buf| buf.copy_from_slice(data),
        &ChecksumCapabilities::default(),
    );
    bytes
}

fn tcp_bytes(repr: &TcpRepr<'_>, dst: [u8; 4]) -> Vec<u8> {
    let mut bytes = vec![0u8; repr.buffer_len()];
    repr.emit(
        &mut TcpPacket::new_unchecked(&mut bytes[..]),
        &IpAddress::Ipv4(ip(GATEWAY_IP)),
        &IpAddress::Ipv4(ip(dst)),
        &ChecksumCapabilities::default(),
    );
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    const STA_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x11, 0x22, 0x33];
    const BROADCAST_MAC: [u8; 6] = [0xFF; 6];

    fn from_sta(dst_mac: [u8; 6], ethertype: EthernetProtocol, payload: &[u8]) -> Vec<u8> {
        let repr = EthernetRepr {
            src_addr: mac(STA_MAC),
            dst_addr: mac(dst_mac),
            ethertype,
        };
        let mut frame = vec![0u8; repr.buffer_len() + payload.len()];
        let mut eth = EthernetFrame::new_unchecked(&mut frame[..]);
        repr.emit(&mut eth);
        eth.payload_mut().copy_from_slice(payload);
        frame
    }

    fn sta_ipv4(src: [u8; 4], dst: [u8; 4], proto: IpProtocol, payload: &[u8]) -> Vec<u8> {
        let repr = Ipv4Repr {
            src_addr: ip(src),
            dst_addr: ip(dst),
            next_header: proto,
            payload_len: payload.len(),
            hop_limit: 255,
        };
        let mut d = vec![0u8; repr.buffer_len() + payload.len()];
        let mut p = Ipv4Packet::new_unchecked(&mut d[..]);
        repr.emit(&mut p, &ChecksumCapabilities::default());
        p.payload_mut().copy_from_slice(payload);
        p.fill_checksum();
        d
    }

    fn dhcp_from_sta(kind: DhcpMessageType, requested: Option<[u8; 4]>, xid: u32) -> Vec<u8> {
        let repr = DhcpRepr {
            message_type: kind,
            transaction_id: xid,
            secs: 0,
            client_hardware_address: mac(STA_MAC),
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip: Ipv4Address::UNSPECIFIED,
            server_ip: Ipv4Address::UNSPECIFIED,
            router: None,
            subnet_mask: None,
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast: false,
            requested_ip: requested.map(ip),
            client_identifier: Some(mac(STA_MAC)),
            server_identifier: requested.map(|_| ip(GATEWAY_IP)),
            parameter_request_list: Some(&[1, 3, 6, 15]),
            dns_servers: None,
            max_size: Some(1500),
            lease_duration: None,
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let mut body = vec![0u8; repr.buffer_len()];
        repr.emit(&mut DhcpPacket::new_unchecked(&mut body[..]))
            .unwrap();
        let udp = udp_datagram(
            [0; 4],
            [255; 4],
            smoltcp::wire::DHCP_CLIENT_PORT,
            smoltcp::wire::DHCP_SERVER_PORT,
            &body,
        );
        from_sta(
            BROADCAST_MAC,
            EthernetProtocol::Ipv4,
            &sta_ipv4([0; 4], [255; 4], IpProtocol::Udp, &udp),
        )
    }

    struct Reply {
        kind: DhcpMessageType,
        yiaddr: [u8; 4],
        dst_mac: [u8; 6],
        dst_ip: [u8; 4],
        lease: Option<u32>,
    }

    fn dhcp_reply(frame: &[u8]) -> Reply {
        let eth = EthernetFrame::new_checked(frame).unwrap();
        assert_eq!(eth.src_addr().0, GATEWAY_MAC);
        let ipp = Ipv4Packet::new_checked(eth.payload()).unwrap();
        assert!(ipp.verify_checksum(), "the IPv4 checksum is right");
        let udp = UdpPacket::new_checked(ipp.payload()).unwrap();
        let caps = ChecksumCapabilities::default();
        UdpRepr::parse(
            &udp,
            &IpAddress::Ipv4(ipp.src_addr()),
            &IpAddress::Ipv4(ipp.dst_addr()),
            &caps,
        )
        .expect("the UDP checksum is right");
        let dhcp = DhcpPacket::new_checked(udp.payload()).unwrap();
        let r = DhcpRepr::parse(&dhcp).unwrap();
        Reply {
            kind: r.message_type,
            yiaddr: r.your_ip.octets(),
            dst_mac: eth.dst_addr().0,
            dst_ip: ipp.dst_addr().octets(),
            lease: r.lease_duration,
        }
    }

    /// As lwIP's client makes it: each reply unicast to the station (broadcast bit clear).
    #[test]
    fn a_station_gets_its_lease_by_discover_offer_request_ack() {
        let mut lan = Lan::default();
        let offer = lan.input(10, &dhcp_from_sta(DhcpMessageType::Discover, None, 7));
        assert_eq!(offer.len(), 1);
        let r = dhcp_reply(&offer[0]);
        assert_eq!(r.kind, DhcpMessageType::Offer);
        assert_eq!(r.yiaddr, POOL_START);
        assert_eq!(
            (r.dst_mac, r.dst_ip),
            (STA_MAC, POOL_START),
            "unicast to chaddr and yiaddr"
        );
        assert_eq!(r.lease, Some(LEASE_S));
        assert_eq!(lan.first_ack_us, 0, "no ACK yet");

        let ack = lan.input(
            20,
            &dhcp_from_sta(DhcpMessageType::Request, Some(POOL_START), 7),
        );
        let r = dhcp_reply(&ack[0]);
        assert_eq!(r.kind, DhcpMessageType::Ack);
        assert_eq!(r.yiaddr, POOL_START);
        assert!(lan.leases[0].bound);
        assert_eq!(lan.first_ack_us, 20);

        let nak = lan.input(
            30,
            &dhcp_from_sta(DhcpMessageType::Request, Some([10, 23, 0, 7]), 8),
        );
        let r = dhcp_reply(&nak[0]);
        assert_eq!(r.kind, DhcpMessageType::Nak);
        assert_eq!((r.dst_mac, r.dst_ip), (BROADCAST_MAC, [255; 4]));
        let c = &lan.counters;
        assert_eq!((c.dhcp_offers, c.dhcp_acks, c.dhcp_naks), (1, 1, 1));
    }

    fn arp_from_sta(target: [u8; 4], sender: [u8; 4]) -> Vec<u8> {
        let repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: mac(STA_MAC),
            source_protocol_addr: ip(sender),
            target_hardware_addr: EthernetAddress([0; 6]),
            target_protocol_addr: ip(target),
        };
        let mut body = vec![0u8; repr.buffer_len()];
        repr.emit(&mut ArpPacket::new_unchecked(&mut body[..]));
        from_sta(BROADCAST_MAC, EthernetProtocol::Arp, &body)
    }

    /// lwIP's check of its offered address (`CONFIG_LWIP_DHCP_DOES_ARP_CHECK`) must find it free.
    #[test]
    fn arp_is_answered_for_the_gateway_only() {
        let mut lan = Lan::default();
        let reply = lan.input(1, &arp_from_sta(GATEWAY_IP, POOL_START));
        assert_eq!(reply.len(), 1);
        let eth = EthernetFrame::new_checked(&reply[0][..]).unwrap();
        assert_eq!(eth.dst_addr().0, STA_MAC);
        let ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            ..
        } = ArpRepr::parse(&ArpPacket::new_checked(eth.payload()).unwrap()).unwrap()
        else {
            panic!("an Ethernet/IPv4 ARP");
        };
        assert_eq!(operation, ArpOperation::Reply);
        assert_eq!(source_hardware_addr.0, GATEWAY_MAC);
        assert_eq!(source_protocol_addr, ip(GATEWAY_IP));
        assert!(
            lan.input(2, &arp_from_sta(POOL_START, [0; 4])).is_empty(),
            "the ARP check of the offered address gets no answer"
        );
        assert_eq!(lan.counters.arp_replies, 1);
    }

    struct Client {
        seq: u32,
        ack: u32,
        port: u16,
    }

    fn seq_len(control: TcpControl, data: usize) -> u32 {
        data as u32 + u32::from(matches!(control, TcpControl::Syn | TcpControl::Fin))
    }

    impl Client {
        fn send(
            &mut self,
            lan: &mut Lan,
            control: TcpControl,
            data: &[u8],
            dport: u16,
        ) -> Vec<Vec<u8>> {
            let repr = TcpRepr {
                src_port: self.port,
                dst_port: dport,
                control,
                seq_number: TcpSeqNumber(self.seq as i32),
                ack_number: (control != TcpControl::Syn).then_some(TcpSeqNumber(self.ack as i32)),
                window_len: 5_760,
                window_scale: None,
                max_seg_size: (control == TcpControl::Syn).then_some(1_440),
                sack_permitted: false,
                sack_ranges: [None; 3],
                timestamp: None,
                payload: data,
            };
            let mut bytes = vec![0u8; repr.buffer_len()];
            repr.emit(
                &mut TcpPacket::new_unchecked(&mut bytes[..]),
                &IpAddress::Ipv4(ip(POOL_START)),
                &IpAddress::Ipv4(ip(GATEWAY_IP)),
                &ChecksumCapabilities::default(),
            );
            self.seq = self.seq.wrapping_add(seq_len(control, data.len()));
            let frame = from_sta(
                GATEWAY_MAC,
                EthernetProtocol::Ipv4,
                &sta_ipv4(POOL_START, GATEWAY_IP, IpProtocol::Tcp, &bytes),
            );
            lan.input(0, &frame)
        }

        fn take(&mut self, frame: &[u8]) -> (TcpControl, Vec<u8>) {
            let eth = EthernetFrame::new_checked(frame).unwrap();
            let ipp = Ipv4Packet::new_checked(eth.payload()).unwrap();
            assert!(ipp.verify_checksum());
            let tcp = TcpPacket::new_checked(ipp.payload()).unwrap();
            let r = TcpRepr::parse(
                &tcp,
                &IpAddress::Ipv4(ipp.src_addr()),
                &IpAddress::Ipv4(ipp.dst_addr()),
                &ChecksumCapabilities::default(),
            )
            .expect("the TCP checksum is right");
            let seq = r.seq_number.0 as u32;
            if seq == self.ack || r.control == TcpControl::Syn {
                self.ack = seq.wrapping_add(seq_len(r.control, r.payload.len()));
            }
            (r.control, r.payload.to_vec())
        }
    }

    const GET: &[u8] =
        b"GET /probe HTTP/1.1\r\nHost: 10.23.0.1\r\nUser-Agent: ESP32 HTTP Client/1.0\r\n\r\n";

    #[test]
    fn a_get_of_the_probe_path_is_answered_over_tcp() {
        let mut lan = Lan::default();
        let mut c = Client {
            seq: 1_000,
            ack: 0,
            port: 49_153,
        };
        let synack = c.send(&mut lan, TcpControl::Syn, &[], services::HTTP_PORT);
        assert_eq!(synack.len(), 1);
        assert_eq!(c.take(&synack[0]).0, TcpControl::Syn);
        assert_eq!(c.ack, ISN_BASE + 1);
        assert!(
            c.send(&mut lan, TcpControl::None, &[], services::HTTP_PORT)
                .is_empty()
        );
        assert_eq!(lan.tcp[0].state, tcp_state::ESTABLISHED);

        let answer = c.send(&mut lan, TcpControl::Psh, GET, services::HTTP_PORT);
        let mut body = Vec::new();
        for f in &answer {
            body.extend(c.take(f).1);
        }
        assert_eq!(
            body,
            services::http_answer(GET).unwrap(),
            "the whole answer, in order"
        );
        assert_eq!(lan.counters.http_answers, 1);

        let fin = c.send(&mut lan, TcpControl::Fin, &[], services::HTTP_PORT);
        assert_eq!(fin.len(), 1);
        assert_eq!(c.take(&fin[0]).0, TcpControl::Fin);
        assert!(
            c.send(&mut lan, TcpControl::None, &[], services::HTTP_PORT)
                .is_empty()
        );
        assert!(lan.tcp.is_empty(), "the connection is gone");
    }

    #[test]
    fn an_answer_is_cut_at_the_peers_mss() {
        let mut lan = Lan::default();
        let mut c = Client {
            seq: 5,
            ack: 0,
            port: 50_000,
        };
        let s = c.send(&mut lan, TcpControl::Syn, &[], services::HTTP_PORT);
        c.take(&s[0]);
        c.send(&mut lan, TcpControl::None, &[], services::HTTP_PORT);
        lan.tcp[0].peer_mss = 16;
        let out = c.send(&mut lan, TcpControl::Psh, GET, services::HTTP_PORT);
        let expected = services::http_answer(GET).unwrap();
        assert_eq!(out.len(), expected.len().div_ceil(16));
        let mut got = Vec::new();
        for f in &out {
            let (_, data) = c.take(f);
            assert!(data.len() <= 16);
            got.extend(data);
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn a_segment_for_no_service_is_reset() {
        let mut lan = Lan::default();
        let mut c = Client {
            seq: 9,
            ack: 0,
            port: 50_001,
        };
        let rst = c.send(&mut lan, TcpControl::Syn, &[], 8_080);
        assert_eq!(c.take(&rst[0]).0, TcpControl::Rst);
        assert_eq!(lan.counters.tcp_resets, 1);
        assert!(lan.tcp.is_empty());
    }

    #[test]
    fn an_echo_request_to_the_gateway_is_answered() {
        let mut lan = Lan::default();
        let req = Icmpv4Repr::EchoRequest {
            ident: 1,
            seq_no: 2,
            data: b"ping",
        };
        let caps = ChecksumCapabilities::default();
        let mut bytes = vec![0u8; req.buffer_len()];
        req.emit(&mut Icmpv4Packet::new_unchecked(&mut bytes[..]), &caps);
        let frame = from_sta(
            GATEWAY_MAC,
            EthernetProtocol::Ipv4,
            &sta_ipv4(POOL_START, GATEWAY_IP, IpProtocol::Icmp, &bytes),
        );
        let out = lan.input(0, &frame);
        let eth = EthernetFrame::new_checked(&out[0][..]).unwrap();
        let ipp = Ipv4Packet::new_checked(eth.payload()).unwrap();
        let icmp = Icmpv4Packet::new_checked(ipp.payload()).unwrap();
        assert_eq!(
            Icmpv4Repr::parse(&icmp, &caps).unwrap(),
            Icmpv4Repr::EchoReply {
                ident: 1,
                seq_no: 2,
                data: b"ping"
            }
        );
    }

    #[test]
    fn a_dns_query_to_the_gateway_is_answered_from_the_zone() {
        let mut lan = Lan::default();
        let mut q = vec![0xAB, 0xCD, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in ["gateway", "passportsim", "lan"] {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.extend_from_slice(&[0, 0, 1, 0, 1]);
        let udp = udp_datagram(POOL_START, GATEWAY_IP, 5353, services::DNS_PORT, &q);
        let frame = from_sta(
            GATEWAY_MAC,
            EthernetProtocol::Ipv4,
            &sta_ipv4(POOL_START, GATEWAY_IP, IpProtocol::Udp, &udp),
        );
        let out = lan.input(0, &frame);
        assert_eq!(out.len(), 1);
        let eth = EthernetFrame::new_checked(&out[0][..]).unwrap();
        let ipp = Ipv4Packet::new_checked(eth.payload()).unwrap();
        let udp = UdpPacket::new_checked(ipp.payload()).unwrap();
        assert_eq!(udp.dst_port(), 5353);
        assert_eq!(&udp.payload()[udp.payload().len() - 4..], &GATEWAY_IP);
        assert_eq!(lan.counters.dns_answers, 1);
    }

    #[test]
    fn a_frame_for_another_host_is_counted_and_not_answered() {
        let mut lan = Lan::default();
        let udp = udp_datagram(POOL_START, [8, 8, 8, 8], 5000, 53, b"x");
        let frame = from_sta(
            GATEWAY_MAC,
            EthernetProtocol::Ipv4,
            &sta_ipv4(POOL_START, [8, 8, 8, 8], IpProtocol::Udp, &udp),
        );
        assert!(lan.input(0, &frame).is_empty());
        assert_eq!(lan.counters.unhandled, 1);
    }

    #[test]
    fn the_lan_state_round_trips_with_a_connection_half_open() {
        use pemu_core::snap::{SnapReader, SnapValue};
        let mut lan = Lan::default();
        lan.input(10, &dhcp_from_sta(DhcpMessageType::Discover, None, 1));
        let mut c = Client {
            seq: 1,
            ack: 0,
            port: 40_000,
        };
        c.send(&mut lan, TcpControl::Syn, &[], services::HTTP_PORT);
        let mut bytes = Vec::new();
        lan.snap_write(&mut bytes);
        let mut r = SnapReader::new(&bytes, "hle.machine");
        assert_eq!(Lan::snap_read(&mut r).unwrap(), lan);
        assert!(r.is_empty());
    }

    use pemu_core::input::NetRoute;

    fn bridged_lan() -> Lan {
        let mut lan = Lan::default();
        lan.bridge_attach(&[NetRoute {
            port: 80,
            host_port: 18_080,
        }]);
        assert!(lan.bridge_packet(0, &bridge::continue_(0, 64)).is_empty());
        assert_eq!(lan.bridge.credit0, 64);
        lan
    }

    fn sent(lan: &Lan, cursor: &mut u64) -> Vec<(u8, u32, Vec<u8>)> {
        let (packets, next) = lan.bridge.since(*cursor);
        *cursor = next;
        packets
            .iter()
            .map(|p| {
                let (k, s, body) = bridge::decode(p).unwrap();
                (k, s, body.to_vec())
            })
            .collect()
    }

    #[test]
    fn a_bridged_connection_holds_the_syn_and_carries_both_directions() {
        let mut lan = bridged_lan();
        let mut cursor = 0;
        let mut c = Client {
            seq: 100,
            ack: 0,
            port: 49_200,
        };
        assert!(
            c.send(&mut lan, TcpControl::Syn, &[], 80).is_empty(),
            "no SYN-ACK before the relay answers"
        );
        let out = sent(&lan, &mut cursor);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, bridge::kind::CONNECT);
        let stream = out[0].1;
        assert_eq!(
            bridge::parse_connect(&out[0].2),
            Some((bridge::STREAM_TCP, 80, bridge::HOST_NAME.to_owned()))
        );
        c.seq = 100;
        assert!(c.send(&mut lan, TcpControl::Syn, &[], 80).is_empty());
        assert!(sent(&lan, &mut cursor).is_empty());

        let synack = lan.bridge_packet(1, &bridge::continue_(stream, 2));
        assert_eq!(synack.len(), 1);
        assert_eq!(c.take(&synack[0]).0, TcpControl::Syn);
        assert!(c.send(&mut lan, TcpControl::None, &[], 80).is_empty());
        assert_eq!(lan.tcp[0].state, tcp_state::ESTABLISHED);

        let ack = c.send(&mut lan, TcpControl::Psh, GET, 80);
        assert_eq!(ack.len(), 1, "the request is acknowledged");
        let out = sent(&lan, &mut cursor);
        assert_eq!(out, vec![(bridge::kind::DATA, stream, GET.to_vec())]);
        assert_eq!(
            lan.counters.http_answers, 0,
            "the scripted service is shadowed"
        );

        let body = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        let data = lan.bridge_packet(2, &bridge::encode(bridge::kind::DATA, stream, body));
        let mut got = Vec::new();
        for f in &data {
            got.extend(c.take(f).1);
        }
        assert_eq!(got, body);
        let fin = lan.bridge_packet(3, &bridge::close(stream, bridge::reason::VOLUNTARY));
        assert_eq!(fin.len(), 1);
        assert_eq!(c.take(&fin[0]).0, TcpControl::Fin);
        c.send(&mut lan, TcpControl::None, &[], 80);
        c.send(&mut lan, TcpControl::Fin, &[], 80);
        assert!(lan.tcp.is_empty(), "the connection is gone");
        assert!(sent(&lan, &mut cursor).is_empty());
        let k = &lan.bridge.counters;
        assert_eq!(
            (k.connects, k.closed_by_host, k.bytes_out),
            (1, 1, GET.len() as u64)
        );
        assert_eq!(k.bytes_in, body.len() as u64);
        assert_eq!((lan.bridge.inbound, lan.bridge.lost_in), (4, 0));
    }

    #[test]
    fn a_refused_stream_resets_the_held_syn() {
        let mut lan = bridged_lan();
        let mut c = Client {
            seq: 7,
            ack: 0,
            port: 49_201,
        };
        c.send(&mut lan, TcpControl::Syn, &[], 80);
        let stream = lan.tcp[0].stream;
        let rst = lan.bridge_packet(1, &bridge::close(stream, bridge::reason::REFUSED));
        assert_eq!(c.take(&rst[0]).0, TcpControl::Rst);
        assert!(lan.tcp.is_empty());
        assert_eq!(lan.bridge.counters.refused, 1);
    }

    /// WISP v1 credit is counted in packets; the guest's retransmission goes out once a `CONTINUE`
    /// restores it.
    #[test]
    fn a_stream_with_no_credit_holds_the_guests_data() {
        let mut lan = bridged_lan();
        let mut cursor = 0;
        let mut c = Client {
            seq: 1,
            ack: 0,
            port: 49_202,
        };
        c.send(&mut lan, TcpControl::Syn, &[], 80);
        let stream = lan.tcp[0].stream;
        let s = lan.bridge_packet(1, &bridge::continue_(stream, 0));
        c.take(&s[0]);
        c.send(&mut lan, TcpControl::None, &[], 80);
        sent(&lan, &mut cursor);
        let before = c.seq;
        c.send(&mut lan, TcpControl::Psh, b"x", 80);
        assert!(sent(&lan, &mut cursor).is_empty(), "held");
        assert_eq!(lan.bridge.counters.held, 1);
        lan.bridge_packet(2, &bridge::continue_(stream, 1));
        c.seq = before;
        c.send(&mut lan, TcpControl::Psh, b"x", 80);
        assert_eq!(
            sent(&lan, &mut cursor),
            vec![(bridge::kind::DATA, stream, b"x".to_vec())]
        );
    }

    /// The port is the scripted service's again after the detach.
    #[test]
    fn a_detach_resets_every_bridged_connection() {
        let mut lan = bridged_lan();
        let mut cursor = 0;
        let mut c = Client {
            seq: 1,
            ack: 0,
            port: 49_203,
        };
        c.send(&mut lan, TcpControl::Syn, &[], 80);
        let stream = lan.tcp[0].stream;
        let s = lan.bridge_packet(1, &bridge::continue_(stream, 8));
        c.take(&s[0]);
        sent(&lan, &mut cursor);
        let rsts = lan.bridge_detach();
        assert_eq!(rsts.len(), 1);
        assert_eq!(c.take(&rsts[0]).0, TcpControl::Rst);
        assert_eq!(
            sent(&lan, &mut cursor),
            vec![(bridge::kind::CLOSE, stream, vec![bridge::reason::VOLUNTARY])]
        );
        assert!(!lan.bridge.attached && lan.tcp.is_empty());
        let mut d = Client {
            seq: 50,
            ack: 0,
            port: 49_204,
        };
        let synack = d.send(&mut lan, TcpControl::Syn, &[], 80);
        assert_eq!(
            d.take(&synack[0]).0,
            TcpControl::Syn,
            "the scripted service answers again"
        );
    }

    /// Without the bridge an allowlisted port is not bridged either.
    #[test]
    fn only_allowlisted_ports_are_bridged() {
        let mut lan = bridged_lan();
        let mut c = Client {
            seq: 1,
            ack: 0,
            port: 49_205,
        };
        let rst = c.send(&mut lan, TcpControl::Syn, &[], 8_080);
        assert_eq!(c.take(&rst[0]).0, TcpControl::Rst);
        assert_eq!(lan.bridge.counters.connects, 0);
    }
}
