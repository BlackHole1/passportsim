//! The Wi-Fi bridge: the gateway terminates the guest's TCP and proxies each connection to a host
//! socket as a WISP v1 stream (MercuryWorkshop `wisp-protocol` specification). The machine is the
//! WISP client; the server is the native bridge's in-process one or the daemon's `/v1/relay`
//! WebSocket in the browser.
//!
//! Outbound packets go into a window a transport reads by cursor ([`Bridge::since`]) and never
//! drains, so reading changes no guest state. Inbound packets arrive as journaled
//! `InputEvent::NetFrame` inputs numbered per stream, so a recorded run replays without the peer.
//!
//! - A guest SYN to [`HOST_NAME`] on an allowlisted port sends `CONNECT` and holds the SYN until
//!   the stream's first server packet: WISP v1 has no connect acknowledgement, so this project's
//!   server sends `CONTINUE` once its socket connects. A server `CLOSE` answers the SYN with RST.
//! - Guest data becomes `DATA` under the server's packet credit; with none left the gateway does
//!   not take the segment and the guest's TCP retransmits it.
//! - WISP has no half-close, so host data after the guest's FIN is not delivered (class C). A
//!   detach resets every bridged connection.
//!
//! A route shadows the scripted service on its port while attached, so `probe_wifi_http`
//! reaches a host server unchanged.

use pemu_core::input::NetRoute;

use pemu_core::snap::snap_struct;

/// The name the scripted zone answers with the gateway's address, whose allowlisted ports reach
/// the host's loopback.
pub const HOST_NAME: &str = "host.emu.internal";

pub mod kind {
    pub const CONNECT: u8 = 0x01;
    pub const DATA: u8 = 0x02;
    /// Server: buffer credit, in packets.
    pub const CONTINUE: u8 = 0x03;
    pub const CLOSE: u8 = 0x04;
}

pub mod reason {
    pub const UNSPECIFIED: u8 = 0x01;
    pub const VOLUNTARY: u8 = 0x02;
    pub const NETWORK: u8 = 0x03;
    /// Invalid information (a reserved address, an invalid port).
    pub const INVALID: u8 = 0x41;
    pub const REFUSED: u8 = 0x44;
    pub const BLOCKED: u8 = 0x48;
    pub const THROTTLED: u8 = 0x49;
}

pub const STREAM_TCP: u8 = 0x01;

/// Client packets the outbound window keeps for a transport that has not read them.
pub const OUT_WINDOW: usize = 512;

/// One WISP packet; the header is little-endian.
pub fn encode(kind: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&stream.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// `None` for fewer than five bytes.
pub fn decode(packet: &[u8]) -> Option<(u8, u32, &[u8])> {
    let head = packet.get(..5)?;
    Some((
        head[0],
        u32::from_le_bytes([head[1], head[2], head[3], head[4]]),
        &packet[5..],
    ))
}

pub fn connect(stream: u32, port: u16, host: &str) -> Vec<u8> {
    let mut payload = vec![STREAM_TCP];
    payload.extend_from_slice(&port.to_le_bytes());
    payload.extend_from_slice(host.as_bytes());
    encode(kind::CONNECT, stream, &payload)
}

pub fn continue_(stream: u32, credit: u32) -> Vec<u8> {
    encode(kind::CONTINUE, stream, &credit.to_le_bytes())
}

pub fn close(stream: u32, why: u8) -> Vec<u8> {
    encode(kind::CLOSE, stream, &[why])
}

/// Stream type, port and host name.
pub fn parse_connect(payload: &[u8]) -> Option<(u8, u16, String)> {
    let ty = *payload.first()?;
    let port = u16::from_le_bytes([*payload.get(1)?, *payload.get(2)?]);
    let host = std::str::from_utf8(payload.get(3..)?).ok()?.to_owned();
    Some((ty, port, host))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BridgeCounters {
    pub connects: u64,
    /// Streams the server refused before they connected (the guest got a RST).
    pub refused: u64,
    pub closed_by_host: u64,
    pub closed_by_guest: u64,
    pub bytes_out: u64,
    pub bytes_in: u64,
    /// Guest segments not taken because the stream had no credit left.
    pub held: u64,
    /// Inbound packets for no stream, or that did not decode.
    pub stray: u64,
}

snap_struct!(BridgeCounters {
    connects,
    refused,
    closed_by_host,
    closed_by_guest,
    bytes_out,
    bytes_in,
    held,
    stray
});

/// Part of the gateway, so part of every snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bridge {
    pub attached: bool,
    pub routes: Vec<NetRoute>,
    /// The per-stream credit the server announced on stream 0, 0 before it did.
    pub credit0: u32,
    /// Stream 0 is the protocol's own, so the first is 1.
    pub next_stream: u32,
    /// Oldest first; the first is at [`Bridge::out_base`].
    pub out: Vec<Vec<u8>>,
    pub out_base: u64,
    /// Client packets evicted before a transport read them.
    pub dropped_out: u64,
    pub next_seq: u64,
    pub inbound: u64,
    /// Inbound packets the stream numbering says were never delivered.
    pub lost_in: u64,
    pub counters: BridgeCounters,
}

snap_struct!(Bridge {
    attached,
    routes,
    credit0,
    next_stream,
    out,
    out_base,
    dropped_out,
    next_seq,
    inbound,
    lost_in,
    counters
});

impl Bridge {
    pub fn routes_port(&self, port: u16) -> bool {
        self.attached && self.routes.iter().any(|r| r.port == port)
    }

    pub fn open(&mut self, port: u16) -> u32 {
        let stream = self.next_stream.max(1);
        self.next_stream = stream.wrapping_add(1).max(1);
        self.counters.connects += 1;
        self.push(connect(stream, port, HOST_NAME));
        stream
    }

    pub fn push(&mut self, packet: Vec<u8>) {
        self.out.push(packet);
        if self.out.len() > OUT_WINDOW {
            let excess = self.out.len() - OUT_WINDOW;
            self.out.drain(..excess);
            self.out_base += excess as u64;
            self.dropped_out += excess as u64;
        }
    }

    /// A cursor older than the window starts at its oldest packet; the loss is in
    /// [`Bridge::dropped_out`].
    pub fn since(&self, cursor: u64) -> (Vec<Vec<u8>>, u64) {
        let head = self.out_base + self.out.len() as u64;
        let from = cursor.clamp(self.out_base, head);
        let skip = (from - self.out_base) as usize;
        (self.out[skip..].to_vec(), head)
    }

    /// Counts a gap in the stream as lost.
    pub fn take_seq(&mut self, seq: u64) {
        if seq > self.next_seq {
            self.lost_in += seq - self.next_seq;
        }
        self.next_seq = self.next_seq.max(seq.saturating_add(1));
        self.inbound += 1;
    }

    /// A re-attach keeps the window and stream position, so a reconnecting transport resumes where
    /// it was.
    pub fn attach(&mut self, routes: &[NetRoute]) {
        self.attached = true;
        self.routes = routes.to_vec();
        self.credit0 = 0;
    }

    /// The caller resets the bridged connections.
    pub fn detach(&mut self) {
        self.attached = false;
        self.routes.clear();
        self.credit0 = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::snap::{SnapReader, SnapValue};

    #[test]
    fn packets_have_the_wisp_v1_little_endian_header() {
        let c = connect(0x0102_0304, 8080, HOST_NAME);
        assert_eq!(&c[..5], &[0x01, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(c[5], STREAM_TCP);
        assert_eq!(u16::from_le_bytes([c[6], c[7]]), 8080);
        assert_eq!(&c[8..], HOST_NAME.as_bytes());
        let (k, s, p) = decode(&c).unwrap();
        assert_eq!((k, s), (kind::CONNECT, 0x0102_0304));
        assert_eq!(
            parse_connect(p),
            Some((STREAM_TCP, 8080, HOST_NAME.to_owned()))
        );
        assert_eq!(continue_(0, 128), vec![3, 0, 0, 0, 0, 128, 0, 0, 0]);
        assert_eq!(close(7, reason::VOLUNTARY), vec![4, 7, 0, 0, 0, 2]);
        assert_eq!(decode(&[1, 2, 3]), None);
    }

    #[test]
    fn the_window_is_read_by_cursor_and_evicts_the_oldest() {
        let mut b = Bridge::default();
        b.attach(&[NetRoute {
            port: 80,
            host_port: 8080,
        }]);
        assert!(b.routes_port(80) && !b.routes_port(81));
        assert_eq!(b.open(80), 1);
        assert_eq!(b.open(80), 2);
        let (packets, next) = b.since(0);
        assert_eq!((packets.len(), next), (2, 2));
        assert_eq!(b.since(next).0.len(), 0, "reading drains nothing");
        for i in 0..OUT_WINDOW as u32 {
            b.push(close(i, 2));
        }
        assert_eq!(b.dropped_out, 2);
        let (packets, next) = b.since(0);
        assert_eq!(packets.len(), OUT_WINDOW);
        assert_eq!(next, OUT_WINDOW as u64 + 2);
        b.detach();
        assert!(!b.routes_port(80));
    }

    #[test]
    fn a_gap_in_the_inbound_stream_is_counted() {
        let mut b = Bridge::default();
        b.take_seq(0);
        b.take_seq(3);
        assert_eq!((b.inbound, b.lost_in, b.next_seq), (2, 2, 4));
    }

    #[test]
    fn the_state_round_trips() {
        let mut b = Bridge::default();
        b.attach(&[NetRoute {
            port: 80,
            host_port: 9,
        }]);
        b.open(80);
        b.take_seq(0);
        let mut bytes = Vec::new();
        b.snap_write(&mut bytes);
        let mut r = SnapReader::new(&bytes, "hle.machine");
        assert_eq!(Bridge::snap_read(&mut r).unwrap(), b);
        assert!(r.is_empty());
    }
}
