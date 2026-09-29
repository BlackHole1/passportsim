//! The raw SLIP mode's one piece of protocol awareness: spotting an esptool `SYNC` request, so
//! `--auto-download` can reset a running instance into ROM download mode. Otherwise the endpoint
//! forwards bytes unchanged and never answers a command.
//!
//! Framing: packets are delimited by `0xC0`; inside one, `0xDB 0xDC` stands for `0xC0` and
//! `0xDB 0xDD` for `0xDB`; a request is direction `0x00` then the opcode (`SYNC` is `0x08`).

use super::detect::SLIP_END;

pub const SLIP_ESC: u8 = 0xDB;
pub const SLIP_ESC_END: u8 = 0xDC;
pub const SLIP_ESC_ESC: u8 = 0xDD;
/// Direction byte of a request (host to chip).
pub const REQUEST: u8 = 0x00;
pub const OP_SYNC: u8 = 0x08;

#[derive(Clone, Debug, Default)]
pub struct SyncWatch {
    /// Decoded bytes of the current packet, at most the two header bytes this needs.
    head: [u8; 2],
    len: usize,
    in_packet: bool,
    escaped: bool,
}

impl SyncWatch {
    pub fn new() -> SyncWatch {
        SyncWatch::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> usize {
        let mut syncs = 0;
        for &b in bytes {
            if b == SLIP_END {
                // An END closes the packet and opens the next; an empty packet between two ENDs
                // is the usual filler.
                self.in_packet = true;
                self.len = 0;
                self.escaped = false;
                continue;
            }
            if !self.in_packet {
                continue;
            }
            let decoded = if self.escaped {
                self.escaped = false;
                match b {
                    SLIP_ESC_END => SLIP_END,
                    SLIP_ESC_ESC => SLIP_ESC,
                    other => other,
                }
            } else if b == SLIP_ESC {
                self.escaped = true;
                continue;
            } else {
                b
            };
            if self.len < self.head.len() {
                self.head[self.len] = decoded;
                self.len += 1;
                if self.len == 2 && self.head == [REQUEST, OP_SYNC] {
                    syncs += 1;
                }
            }
        }
        syncs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SYNC` request: header, the `07 07 12 20` magic and 32 x `0x55`.
    fn sync_packet() -> Vec<u8> {
        let mut p = vec![SLIP_END, REQUEST, OP_SYNC, 0x24, 0x00, 0, 0, 0, 0];
        p.extend([0x07, 0x07, 0x12, 0x20]);
        p.extend([0x55; 32]);
        p.push(SLIP_END);
        p
    }

    #[test]
    fn a_sync_request_is_seen_and_other_requests_are_not() {
        let mut w = SyncWatch::new();
        assert_eq!(w.feed(&sync_packet()), 1);
        assert_eq!(
            w.feed(&[SLIP_END, REQUEST, 0x0A, 4, 0, 0, 0, 0, 0, SLIP_END]),
            0
        );
        assert_eq!(w.feed(&[SLIP_END, 0x01, OP_SYNC, SLIP_END]), 0);
    }

    #[test]
    fn a_packet_split_across_reads_is_still_seen_once() {
        let mut w = SyncWatch::new();
        let p = sync_packet();
        let total: usize = p.chunks(1).map(|c| w.feed(c)).sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn escaped_header_bytes_are_decoded_first() {
        let mut w = SyncWatch::new();
        assert_eq!(
            w.feed(&[SLIP_END, SLIP_ESC, SLIP_ESC_ESC, OP_SYNC, SLIP_END]),
            0
        );
        assert_eq!(SyncWatch::new().feed(&[REQUEST, OP_SYNC]), 0);
    }
}
