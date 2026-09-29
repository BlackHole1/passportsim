//! HCI packet formats of the virtual LE controller, restated from the Bluetooth Core
//! Specification 5.4 (Vol 4 Part A for H4, Vol 4 Part E for HCI, Vol 1 Part F for error codes,
//! Vol 6 Part B §4.6 for LE feature bits, Vol 2 Part C §3.3 for LMP feature bits).

/// H4 packet indicators (Core Vol 4 Part A §2).
pub const H4_COMMAND: u8 = 0x01;
pub const H4_ACL: u8 = 0x02;
pub const H4_EVENT: u8 = 0x04;

/// Event codes (Core Vol 4 Part E §7.7).
pub mod event {
    pub const DISCONNECTION_COMPLETE: u8 = 0x05;
    pub const ENCRYPTION_CHANGE: u8 = 0x08;
    pub const READ_REMOTE_VERSION_COMPLETE: u8 = 0x0C;
    /// BR/EDR only; named so a capture can redact it.
    pub const LINK_KEY_NOTIFICATION: u8 = 0x18;
    pub const COMMAND_COMPLETE: u8 = 0x0E;
    pub const COMMAND_STATUS: u8 = 0x0F;
    pub const NUMBER_OF_COMPLETED_PACKETS: u8 = 0x13;
    pub const LE_META: u8 = 0x3E;
    pub const ENCRYPTION_CHANGE_V2: u8 = 0x59;
}

/// LE Meta subevent codes (Core Vol 4 Part E §7.7.65).
pub mod subevent {
    pub const CONNECTION_COMPLETE: u8 = 0x01;
    pub const LONG_TERM_KEY_REQUEST: u8 = 0x05;
    pub const ENHANCED_CONNECTION_COMPLETE: u8 = 0x0A;
    pub const CONNECTION_UPDATE_COMPLETE: u8 = 0x03;
    pub const READ_REMOTE_FEATURES_COMPLETE: u8 = 0x04;
    pub const DATA_LENGTH_CHANGE: u8 = 0x07;
    pub const PHY_UPDATE_COMPLETE: u8 = 0x0C;
    pub const ENHANCED_CONNECTION_COMPLETE_V2: u8 = 0x29;
}

/// Controller error codes (Core Vol 1 Part F §1.3).
pub mod status {
    pub const SUCCESS: u8 = 0x00;
    pub const UNKNOWN_COMMAND: u8 = 0x01;
    pub const UNKNOWN_CONNECTION: u8 = 0x02;
    pub const MEMORY_CAPACITY_EXCEEDED: u8 = 0x07;
    pub const COMMAND_DISALLOWED: u8 = 0x0C;
    pub const UNSUPPORTED_FEATURE: u8 = 0x11;
    pub const INVALID_PARAMETERS: u8 = 0x12;
    pub const REMOTE_USER_TERMINATED: u8 = 0x13;
    pub const LOCAL_HOST_TERMINATED: u8 = 0x16;
    pub const ADVERTISING_TIMEOUT: u8 = 0x3C;
}

/// Core Vol 4 Part E §5.4.1: OCF in the low 10 bits.
pub const fn opcode(ogf: u16, ocf: u16) -> u16 {
    (ogf << 10) | ocf
}

pub const OGF_VENDOR: u16 = 0x3F;

/// Opcodes whose HCI entry has no return parameters, so the controller acknowledges them with
/// `HCI_Command_Status` and reports the outcome in a later event (Core Vol 4 Part E §4.4). An
/// unsupported one gets a Command Status with Unknown HCI Command; any other unsupported opcode
/// gets a Command Complete, which §4.5 leaves to the vendor.
/// `HCI_Host_Number_Of_Completed_Packets` (0x0C35) is never acknowledged when it succeeds
/// (§7.3.40).
pub const ASYNC_OPCODES: [u16; 52] = [
    0x0401, 0x0405, 0x0406, 0x0409, 0x040A, 0x040F, 0x0411, 0x0413, 0x0415, 0x0417, 0x0419, 0x041B,
    0x041C, 0x041D, 0x041F, 0x0428, 0x0429, 0x042A, 0x043D, 0x043E, 0x043F, 0x0443, 0x0444, 0x0801,
    0x0803, 0x0804, 0x0807, 0x080B, 0x0810, 0x0C35, 0x0C53, 0x0C5F, 0x200D, 0x2013, 0x2016, 0x2019,
    0x2025, 0x2026, 0x2032, 0x2043, 0x2044, 0x205E, 0x2064, 0x2066, 0x2068, 0x2069, 0x206A, 0x206B,
    0x206D, 0x2077, 0x207E, 0x2085,
];

pub fn is_async(op: u16) -> bool {
    ASYNC_OPCODES.contains(&op)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command<'a> {
    pub opcode: u16,
    /// Exactly `Parameter_Total_Length` bytes.
    pub params: &'a [u8],
}

impl<'a> Command<'a> {
    /// `None` when the packet is shorter than its header or its stated length.
    pub fn parse(h4: &'a [u8]) -> Option<Command<'a>> {
        let (&[kind, lo, hi, len], rest) = h4.split_first_chunk::<4>()?;
        if kind != H4_COMMAND {
            return None;
        }
        Some(Command {
            opcode: u16::from_le_bytes([lo, hi]),
            params: rest.get(..usize::from(len))?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acl {
    pub handle: u16,
    pub len: u16,
}

impl Acl {
    /// `None` when the packet is shorter than its header or its stated length.
    pub fn parse(h4: &[u8]) -> Option<Acl> {
        let (&[kind, h0, h1, l0, l1], rest) = h4.split_first_chunk::<5>()?;
        let len = u16::from_le_bytes([l0, l1]);
        if kind != H4_ACL || rest.len() < usize::from(len) {
            return None;
        }
        Some(Acl {
            handle: u16::from_le_bytes([h0, h1]) & 0x0FFF,
            len,
        })
    }
}

/// `Packet_Boundary_Flag` values (Core Vol 4 Part E §5.4.2). The first fragment of a host packet
/// to the controller.
pub const PB_FIRST_NON_FLUSHABLE: u8 = 0b00;
pub const PB_CONTINUING: u8 = 0b01;
/// The first fragment of an automatically flushable packet, the form an LE controller uses
/// towards the host.
pub const PB_FIRST_FLUSHABLE: u8 = 0b10;

/// Handle in the low 12 bits, boundary flag in bits 12 and 13, broadcast flag 0.
pub fn acl(handle: u16, pb: u8, data: &[u8]) -> Vec<u8> {
    let word = (handle & 0x0FFF) | (u16::from(pb & 0b11) << 12);
    let mut out = Vec::with_capacity(5 + data.len());
    out.push(H4_ACL);
    out.extend_from_slice(&word.to_le_bytes());
    // Every fragment this controller builds is at most 251 octets (`LE_ACL_Data_Packet_Length`).
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(data);
    out
}

pub fn acl_parts(h4: &[u8]) -> Option<(u16, u8, &[u8])> {
    let header = Acl::parse(h4)?;
    let pb = (h4[2] >> 4) & 0b11;
    Some((header.handle, pb, &h4[5..5 + usize::from(header.len)]))
}

pub fn event(code: u8, params: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + params.len());
    out.push(H4_EVENT);
    out.push(code);
    // Every event this controller builds is far below 255 parameter bytes.
    out.push(params.len() as u8);
    out.extend_from_slice(params);
    out
}

/// No LE-only controller sends SCO; it is named so [`h4_packet_len`] can measure a packet it must
/// not mis-split.
pub const H4_SCO: u8 = 0x03;
pub const H4_ISO: u8 = 0x05;

/// The ISO and ACL length fields are 16 bits wide, so framing never buffers more than this.
pub const H4_MAX_PACKET: usize = 5 + 0xFFFF;

/// How many bytes the H4 packet at the front of `bytes` takes, `Ok(None)` while the stream has not
/// carried enough to tell, and `Err` for an undefined indicator: framing is then lost and the
/// reader must drop the connection. The length field is 1 byte for command, SCO and event, 2
/// little-endian bytes for ACL and ISO, of which ISO uses the low 14 bits.
pub fn h4_packet_len(bytes: &[u8]) -> Result<Option<usize>, u8> {
    let Some(&indicator) = bytes.first() else {
        return Ok(None);
    };
    let (header, length_at, wide) = match indicator {
        H4_COMMAND => (4usize, 3usize, false),
        H4_ACL => (5, 3, true),
        H4_SCO => (4, 3, false),
        H4_EVENT => (3, 2, false),
        H4_ISO => (5, 3, true),
        other => return Err(other),
    };
    if bytes.len() < header {
        return Ok(None);
    }
    let payload = if wide {
        // ISO uses 14 bits with 2 reserved; masking is harmless for ACL (Core Vol 4 Part E §5.4.5).
        let word = u16::from_le_bytes([bytes[length_at], bytes[length_at + 1]]);
        usize::from(if indicator == H4_ISO {
            word & 0x3FFF
        } else {
            word
        })
    } else {
        usize::from(bytes[length_at])
    };
    Ok(Some(header + payload))
}

/// Frames an H4 byte stream into whole packets for the external-HCI transport, where a socket
/// read may return half a packet, three packets or one byte.
#[derive(Clone, Debug, Default)]
pub struct H4Stream {
    held: Vec<u8>,
}

impl H4Stream {
    pub fn new() -> H4Stream {
        H4Stream::default()
    }

    pub fn buffered(&self) -> usize {
        self.held.len()
    }

    /// Appends `bytes` and returns every whole packet now in the stream, oldest first.
    ///
    /// `Err(indicator)` means framing is lost for good. Packets already whole are not returned with
    /// the error, because a caller that has lost framing cannot trust them either.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, u8> {
        self.held.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut from = 0usize;
        loop {
            match h4_packet_len(&self.held[from..])? {
                Some(len) if self.held.len() - from >= len => {
                    out.push(self.held[from..from + len].to_vec());
                    from += len;
                }
                _ => break,
            }
        }
        self.held.drain(..from);
        Ok(out)
    }
}

/// `HCI_Command_Complete` (§7.7.14); `return_params` starts with the status. The device reports
/// `num` = 5 (`ble.toml` `num_command_packets`).
pub fn command_complete(num: u8, op: u16, return_params: &[u8]) -> Vec<u8> {
    let mut params = vec![num];
    params.extend_from_slice(&op.to_le_bytes());
    params.extend_from_slice(return_params);
    event(event::COMMAND_COMPLETE, &params)
}

pub fn command_status(num: u8, status: u8, op: u16) -> Vec<u8> {
    let [lo, hi] = op.to_le_bytes();
    event(event::COMMAND_STATUS, &[status, num, lo, hi])
}

/// Each handle is followed by its count, the arrayed-parameter order of §5.2.
pub fn number_of_completed_packets(pairs: &[(u16, u16)]) -> Vec<u8> {
    let mut params = vec![pairs.len() as u8];
    for (handle, count) in pairs {
        params.extend_from_slice(&handle.to_le_bytes());
        params.extend_from_slice(&count.to_le_bytes());
    }
    event(event::NUMBER_OF_COMPLETED_PACKETS, &params)
}

pub fn le_meta(subevent: u8, params: &[u8]) -> Vec<u8> {
    let mut all = vec![subevent];
    all.extend_from_slice(params);
    event(event::LE_META, &all)
}

pub fn set_bit(map: &mut [u8], bit: usize) {
    if let Some(byte) = map.get_mut(bit / 8) {
        *byte |= 1 << (bit % 8);
    }
}

pub fn mask_has(mask: u64, bit: u32) -> bool {
    mask & (1u64 << bit) != 0
}

/// `Event_Mask` bits (§7.3.1).
pub mod mask {
    pub const DISCONNECTION_COMPLETE: u32 = 4;
    pub const ENCRYPTION_CHANGE: u32 = 7;
    pub const READ_REMOTE_VERSION_COMPLETE: u32 = 11;
    pub const LE_META: u32 = 61;
    /// `Event_Mask_Page_2` bit (§7.3.69).
    pub const PAGE2_ENCRYPTION_CHANGE_V2: u32 = 25;
    /// The default `Event_Mask`, bits 0 to 44 (§7.3.1).
    pub const DEFAULT: u64 = 0x0000_1FFF_FFFF_FFFF;
    /// The default `LE_Event_Mask`, bits 0 to 4 (§7.8.1).
    pub const LE_DEFAULT: u64 = 0x1F;
}

#[cfg(test)]
mod tests {
    /// A reader that sees one byte at a time finds the same packets as one that sees the whole
    /// stream.
    #[test]
    fn an_h4_stream_is_framed_by_indicator_and_length_however_it_arrives() {
        use super::{H4Stream, h4_packet_len};
        let command = vec![super::H4_COMMAND, 0x03, 0x0C, 0x00];
        let event = super::event(super::event::COMMAND_COMPLETE, &[1, 0x03, 0x0C, 0x00]);
        let acl = super::acl(0x0001, super::PB_FIRST_FLUSHABLE, &[0xAA; 64]);
        let iso = {
            let mut p = vec![super::H4_ISO, 0x01, 0x00];
            // 4 payload octets, with the two reserved bits of the length field set.
            p.extend_from_slice(&(0xC000u16 | 4).to_le_bytes());
            p.extend_from_slice(&[1, 2, 3, 4]);
            p
        };
        let sco = vec![super::H4_SCO, 0x01, 0x00, 2, 9, 9];
        let packets = [command, event, acl, iso, sco];
        for p in &packets {
            assert_eq!(h4_packet_len(p), Ok(Some(p.len())), "{p:02x?}");
            assert_eq!(h4_packet_len(&p[..2.min(p.len() - 1)]), Ok(None));
        }
        let stream: Vec<u8> = packets.iter().flatten().copied().collect();

        let mut whole = H4Stream::new();
        assert_eq!(whole.feed(&stream), Ok(packets.to_vec()));
        assert_eq!(whole.buffered(), 0);

        let mut byte_at_a_time = H4Stream::new();
        let mut found = Vec::new();
        for b in &stream {
            found.extend(byte_at_a_time.feed(&[*b]).expect("framed"));
        }
        assert_eq!(found, packets.to_vec());
        assert_eq!(byte_at_a_time.buffered(), 0);

        let mut partial = H4Stream::new();
        assert_eq!(
            partial.feed(&stream[..stream.len() - 1]),
            Ok(packets[..4].to_vec())
        );
        assert!(partial.buffered() > 0);

        assert_eq!(h4_packet_len(&[0x07, 0, 0]), Err(0x07));
        assert_eq!(H4Stream::new().feed(&[0x07, 0, 0]), Err(0x07));
    }

    use super::*;

    #[test]
    fn a_command_parses_only_with_its_stated_length() {
        let reset = [0x01, 0x03, 0x0C, 0x00];
        assert_eq!(
            Command::parse(&reset),
            Some(Command {
                opcode: 0x0C03,
                params: &[]
            })
        );
        assert_eq!(Command::parse(&[0x01, 0x01, 0x0C, 0x08, 0xFF]), None);
        assert_eq!(Command::parse(&[0x02, 0x03, 0x0C, 0x00]), None);
    }

    #[test]
    fn command_complete_and_status_carry_the_command_packet_count() {
        // The Reset answer of the probe capture `device-probe_vhci`.
        assert_eq!(
            command_complete(5, 0x0C03, &[0]),
            [0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00]
        );
        assert_eq!(
            command_status(5, 0x00, 0x041D),
            [0x04, 0x0F, 0x04, 0x00, 0x05, 0x1D, 0x04]
        );
    }

    #[test]
    fn number_of_completed_packets_interleaves_handles_and_counts() {
        assert_eq!(
            number_of_completed_packets(&[(0x0001, 2), (0x0002, 1)]),
            [
                0x04, 0x13, 0x09, 0x02, 0x01, 0x00, 0x02, 0x00, 0x02, 0x00, 0x01, 0x00
            ]
        );
    }

    #[test]
    fn acl_header_masks_the_flags_out_of_the_handle() {
        assert_eq!(
            Acl::parse(&[0x02, 0x01, 0x20, 0x02, 0x00, 0xAA, 0xBB]),
            Some(Acl { handle: 1, len: 2 })
        );
        assert_eq!(Acl::parse(&[0x02, 0x01, 0x20, 0x03, 0x00, 0xAA]), None);
    }

    #[test]
    fn acl_packets_carry_the_boundary_flag_above_the_handle() {
        let packet = acl(0x0001, PB_FIRST_FLUSHABLE, &[0xAA, 0xBB]);
        assert_eq!(packet, [0x02, 0x01, 0x20, 0x02, 0x00, 0xAA, 0xBB]);
        assert_eq!(
            acl_parts(&packet),
            Some((1, PB_FIRST_FLUSHABLE, &[0xAA, 0xBB][..]))
        );
        let cont = acl(0x0EFF, PB_CONTINUING, &[]);
        assert_eq!(acl_parts(&cont), Some((0x0EFF, PB_CONTINUING, &[][..])));
    }

    #[test]
    fn the_async_table_holds_the_two_g2_gaps_and_is_sorted() {
        assert!(is_async(0x041D) && is_async(0x2013));
        assert!(!is_async(0x0C03));
        assert!(ASYNC_OPCODES.windows(2).all(|w| w[0] < w[1]));
    }
}
