//! btsnoop capture at the VHCI boundary.
//!
//! [`Capture`] is part of the BLE module state, so it reaches every snapshot. A snapshot export
//! redacts by value against a `SecretSet` that holds no session key, so [`Capture::record`] zeroes
//! key material on the way in. Addresses are left to the value pass of `pemu-api::redact`.
//!
//! [`Capture::set_secrets`] keeps key material for a pairing trace. The mode is module state set
//! by a journaled step, so it rides snapshots and forks, and it taints the instance.

use super::hci;
use pemu_core::snap::snap_struct;

pub const CAPTURE_BYTES: usize = 64 * 1024;
/// Microseconds from midnight 1 January of year 0 to the Unix epoch, the btsnoop timestamp origin.
pub const EPOCH_OFFSET_US: u64 = 0x00DC_DDB3_0F2F_8000;
/// The btsnoop datalink type of HCI UART (H4).
pub const DATALINK_H4: u32 = 1002;

pub mod dir {
    /// Host to controller (`esp_vhci_host_send_packet`).
    pub const TO_CONTROLLER: u8 = 0;
    /// Controller to host (`notify_host_recv`).
    pub const TO_HOST: u8 = 1;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// Virtual time, in microseconds.
    pub at_us: u64,
    pub dir: u8,
    /// The H4 packet, indicator first.
    pub packet: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub enabled: bool,
    pub records: Vec<Record>,
    pub bytes: u64,
    pub dropped: u64,
    pub redacted: u64,
    /// Whether key material is kept. Off by default.
    pub secrets: bool,
}

impl Default for Capture {
    fn default() -> Capture {
        Capture {
            enabled: true,
            records: Vec::new(),
            bytes: 0,
            dropped: 0,
            redacted: 0,
            secrets: false,
        }
    }
}

snap_struct!(Record { at_us, dir, packet });
snap_struct!(Capture {
    enabled,
    records,
    bytes,
    dropped,
    redacted,
    secrets
});

impl Capture {
    /// Turning it off drops what it holds, so a run without a capture carries none in its
    /// snapshots.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.records = Vec::new();
            self.bytes = 0;
        }
    }

    /// A change of mode drops the records and resets [`Capture::redacted`], so one file never mixes
    /// redacted and full records. [`Capture::dropped`] is not reset: it is the btsnoop cumulative
    /// drop count of the boundary, not of the records.
    pub fn set_secrets(&mut self, secrets: bool) {
        if self.secrets == secrets {
            return;
        }
        self.secrets = secrets;
        self.records = Vec::new();
        self.bytes = 0;
        self.redacted = 0;
    }

    /// Records `packet`, dropping the oldest records beyond the bound. Key material is zeroed
    /// unless [`Capture::secrets`] is set.
    pub fn record(&mut self, at_us: u64, dir: u8, packet: &[u8]) {
        if !self.enabled {
            return;
        }
        let mut redacted = packet.to_vec();
        if !self.secrets {
            redact(&mut redacted);
            if redacted != packet {
                self.redacted += 1;
            }
        }
        self.bytes += redacted.len() as u64;
        self.records.push(Record {
            at_us,
            dir,
            packet: redacted,
        });
        let mut drop = 0;
        while self.bytes > CAPTURE_BYTES as u64 && drop < self.records.len() {
            self.bytes -= self.records[drop].packet.len() as u64;
            drop += 1;
        }
        if drop > 0 {
            self.records.drain(..drop);
            self.dropped += drop as u64;
        }
    }

    /// The btsnoop file: big-endian, datalink 1002 (H4, so every packet keeps its indicator octet);
    /// flags bit 0 marks a packet the host received, bit 1 a command or an event. Timestamps are
    /// virtual time from the Unix epoch, so two runs of the same journal write the same file.
    pub fn to_btsnoop(&self) -> Vec<u8> {
        let mut out = b"btsnoop\0".to_vec();
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&DATALINK_H4.to_be_bytes());
        for r in &self.records {
            let packet = &r.packet;
            let len = packet.len() as u32;
            let kind = matches!(packet.first(), Some(&hci::H4_COMMAND | &hci::H4_EVENT));
            let flags = u32::from(r.dir == dir::TO_HOST) | (u32::from(kind) << 1);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(&(self.dropped.min(u64::from(u32::MAX)) as u32).to_be_bytes());
            out.extend_from_slice(&(EPOCH_OFFSET_US + r.at_us).to_be_bytes());
            out.extend_from_slice(packet);
        }
        out
    }
}

fn zero(bytes: &mut [u8], from: usize, to: usize) {
    let to = to.min(bytes.len());
    if from < to {
        bytes[from..to].fill(0);
    }
}

fn command_opcode(packet: &[u8]) -> Option<u16> {
    (packet.first() == Some(&hci::H4_COMMAND) && packet.len() >= 4)
        .then(|| u16::from_le_bytes([packet[1], packet[2]]))
}

/// Zeroes the key material of one H4 packet: the LTK of `HCI_LE_Long_Term_Key_Request_Reply` and
/// `HCI_LE_Enable_Encryption`, both IRKs of `HCI_LE_Add_Device_To_Resolving_List`, the key,
/// plaintext and result of `HCI_LE_Encrypt`, the `HCI_LE_Rand` numbers, the link keys of
/// `HCI_Link_Key_Request_Reply` and the Link Key Notification event, the DHKey of LE Generate
/// DHKey Complete, and the values of the SMP key-exchange PDUs (Core Vol 4 Part E §7.1.10,
/// §7.7.24, §7.8.22 to §7.8.24, §7.8.38, §7.7.65.9; Vol 3 Part H §3.5 and §3.6). ATT
/// payloads are kept.
pub fn redact(packet: &mut [u8]) {
    match packet.first() {
        Some(&hci::H4_COMMAND) => {
            // Parameters start at octet 4 (indicator, opcode, length).
            let p = 4;
            match command_opcode(packet) {
                Some(0x040B) => zero(packet, p + 6, p + 22),
                Some(0x2017) => zero(packet, p, p + 32),
                Some(0x2019) => zero(packet, p + 12, p + 28),
                Some(0x201A) => zero(packet, p + 2, p + 18),
                Some(0x2027) => zero(packet, p + 7, p + 39),
                _ => {}
            }
        }
        Some(&hci::H4_EVENT) if packet.len() >= 3 => {
            let code = packet[1];
            if code == hci::event::COMMAND_COMPLETE && packet.len() >= 7 {
                // Indicator, code, length, Num_HCI_Command_Packets, opcode; then the status.
                let op = u16::from_le_bytes([packet[4], packet[5]]);
                if matches!(op, 0x2017 | 0x2018) {
                    zero(packet, 7, packet.len());
                }
            } else if code == hci::event::LINK_KEY_NOTIFICATION {
                zero(packet, 3 + 6, 3 + 22);
            } else if code == hci::event::LE_META && packet.get(3) == Some(&0x09) {
                // LE Generate DHKey Complete: subevent, status, DHKey.
                zero(packet, 5, 5 + 32);
            }
        }
        Some(&hci::H4_ACL) => {
            let Some((_, pb, _)) = hci::acl_parts(packet) else {
                return;
            };
            // An SMP PDU fits a start fragment: L2CAP length and channel 0x0006 at octets 5 to 8,
            // the SMP code at 9.
            if pb == hci::PB_CONTINUING || packet.len() < 10 {
                return;
            }
            if u16::from_le_bytes([packet[7], packet[8]]) != super::central::cid::SMP {
                return;
            }
            if matches!(packet[9], 0x03 | 0x04 | 0x06 | 0x07 | 0x08 | 0x0A | 0x0D) {
                zero(packet, 10, packet.len());
            }
        }
        _ => {}
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub flags: u32,
    /// Microseconds since the Unix epoch.
    pub unix_us: u64,
    pub packet: Vec<u8>,
}

/// Parses a btsnoop file of datalink 1002, refusing a bad header, a length mismatch or a truncated
/// record.
pub fn parse(file: &[u8]) -> Result<Vec<Parsed>, String> {
    let header = file.get(..16).ok_or("btsnoop: shorter than its header")?;
    if &header[..8] != b"btsnoop\0" {
        return Err("btsnoop: bad identification pattern".to_string());
    }
    let be32 = |b: &[u8], at: usize| u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    if be32(header, 8) != 1 || be32(header, 12) != DATALINK_H4 {
        return Err("btsnoop: not version 1 with datalink 1002".to_string());
    }
    let mut at = 16;
    let mut out = Vec::new();
    while at < file.len() {
        let rec = file
            .get(at..at + 24)
            .ok_or("btsnoop: truncated record header")?;
        let (orig, incl) = (be32(rec, 0) as usize, be32(rec, 4) as usize);
        if orig != incl {
            return Err("btsnoop: a truncated packet".to_string());
        }
        let ts = u64::from_be_bytes(rec[16..24].try_into().unwrap_or_default());
        let packet = file
            .get(at + 24..at + 24 + incl)
            .ok_or("btsnoop: truncated packet data")?;
        out.push(Parsed {
            flags: be32(rec, 8),
            unix_us: ts
                .checked_sub(EPOCH_OFFSET_US)
                .ok_or("btsnoop: a timestamp before the Unix epoch")?,
            packet: packet.to_vec(),
        });
        at += 24 + incl;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_round_trips_with_directions_kinds_and_virtual_instants() {
        let mut c = Capture::default();
        c.record(1_000, dir::TO_CONTROLLER, &[0x01, 0x03, 0x0C, 0x00]);
        c.record(
            1_500,
            dir::TO_HOST,
            &[0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00],
        );
        c.record(2_000, dir::TO_HOST, &[0x02, 0x01, 0x20, 0x01, 0x00, 0xAA]);
        let file = c.to_btsnoop();
        assert_eq!(&file[..8], b"btsnoop\0");
        let parsed = parse(&file).expect("parses");
        assert_eq!(
            parsed
                .iter()
                .map(|p| (p.flags, p.unix_us))
                .collect::<Vec<_>>(),
            [(0b10, 1_000), (0b11, 1_500), (0b01, 2_000)]
        );
        assert_eq!(parsed[2].packet, [0x02, 0x01, 0x20, 0x01, 0x00, 0xAA]);
        assert!(parse(&file[..file.len() - 1]).is_err());
    }

    #[test]
    fn the_capture_drops_its_oldest_records_beyond_its_bound() {
        let mut c = Capture::default();
        let packet = vec![0x02; 1_000];
        for i in 0..70 {
            c.record(i, dir::TO_HOST, &packet);
        }
        assert_eq!(c.records.len(), CAPTURE_BYTES / 1_000);
        assert_eq!(c.dropped, 70 - (CAPTURE_BYTES / 1_000) as u64);
        assert_eq!(c.records[0].at_us, c.dropped);
    }

    fn command(op: u16, params: &[u8]) -> Vec<u8> {
        let mut p = vec![0x01];
        p.extend_from_slice(&op.to_le_bytes());
        p.push(params.len() as u8);
        p.extend_from_slice(params);
        p
    }

    #[test]
    fn key_material_never_enters_the_capture() {
        let key = [0xA5u8; 16];
        let mut ltk_reply = vec![0x01, 0x00];
        ltk_reply.extend_from_slice(&key);
        let mut enable = vec![0x01, 0x00];
        enable.extend_from_slice(&[0x11; 10]);
        enable.extend_from_slice(&key);
        let mut resolving = vec![0x00, 1, 2, 3, 4, 5, 6];
        resolving.extend_from_slice(&key);
        resolving.extend_from_slice(&key);
        let mut encrypt = key.to_vec();
        encrypt.extend_from_slice(&key);
        let mut smp_ltk = vec![0x02, 0x01, 0x20, 0x15, 0x00, 0x11, 0x00, 0x06, 0x00, 0x06];
        smp_ltk.extend_from_slice(&key);
        let mut rand_done = vec![0x04, 0x0E, 0x0C, 0x05, 0x18, 0x20, 0x00];
        rand_done.extend_from_slice(&[0xA5; 8]);
        let packets = [
            command(0x201A, &ltk_reply),
            command(0x2019, &enable),
            command(0x2027, &resolving),
            command(0x2017, &encrypt),
            smp_ltk,
            rand_done,
        ];
        let mut c = Capture::default();
        for p in &packets {
            c.record(0, dir::TO_CONTROLLER, p);
        }
        // The stored record, not only the written file, is free of key material, because the
        // module state reaches a snapshot export.
        assert_eq!(c.redacted, packets.len() as u64);
        let mut stored = Vec::new();
        for r in &c.records {
            stored.extend_from_slice(&r.packet);
        }
        let written = parse(&c.to_btsnoop()).expect("parses");
        for (w, original) in written.iter().zip(&packets) {
            assert_eq!(w.packet.len(), original.len(), "the length is kept");
        }
        for bytes in [&stored, &c.to_btsnoop()] {
            assert!(
                !bytes.windows(4).any(|w| w == [0xA5; 4]),
                "no key byte run survives: {bytes:02x?}"
            );
        }
        // The handle and the random/EDIV of Enable Encryption are not key material.
        assert_eq!(c.records[1].packet[4..6], [0x01, 0x00]);
        assert_eq!(c.records[1].packet[6..16], [0x11; 10]);
        let reset = command(0x0C03, &[]);
        c.record(1, dir::TO_CONTROLLER, &reset);
        assert_eq!(c.records.last().map(|r| r.packet.clone()), Some(reset));
        assert_eq!(c.redacted, packets.len() as u64);
    }

    #[test]
    fn the_full_fidelity_opt_in_keeps_the_key_material_and_a_switch_drops_what_was_held() {
        let key = [0xA5u8; 16];
        let mut ltk_reply = vec![0x01, 0x00];
        ltk_reply.extend_from_slice(&key);
        let packet = command(0x201A, &ltk_reply);

        let mut c = Capture::default();
        assert!(!c.secrets, "a capture is redacted unless asked otherwise");
        c.record(0, dir::TO_CONTROLLER, &packet);
        assert_eq!(c.redacted, 1);
        assert!(!c.records[0].packet.windows(4).any(|w| w == [0xA5; 4]));

        c.set_secrets(true);
        assert!(c.records.is_empty());
        assert_eq!(c.bytes, 0);
        assert_eq!(c.redacted, 0);
        c.record(1, dir::TO_CONTROLLER, &packet);
        assert_eq!(c.records[0].packet, packet, "stored as it crossed");
        assert_eq!(c.redacted, 0, "nothing was zeroed");
        let file = c.to_btsnoop();
        assert_eq!(parse(&file).expect("parses")[0].packet, packet);

        c.set_secrets(true);
        assert_eq!(c.records.len(), 1);

        c.set_secrets(false);
        assert!(c.records.is_empty());
        c.record(2, dir::TO_CONTROLLER, &packet);
        assert_eq!(c.redacted, 1);
        assert!(!c.to_btsnoop().windows(4).any(|w| w == [0xA5; 4]));
    }

    #[test]
    fn the_full_fidelity_mode_round_trips_through_the_snapshot_codec() {
        let mut c = Capture::default();
        c.set_secrets(true);
        c.record(7, dir::TO_HOST, &[0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00]);
        use pemu_core::snap::SnapValue;
        let mut bytes = Vec::new();
        c.snap_write(&mut bytes);
        let mut r = pemu_core::snap::SnapReader::new(&bytes, "capture");
        let back = Capture::snap_read(&mut r).expect("decodes");
        assert_eq!(back, c);
        assert!(back.secrets);
    }

    #[test]
    fn a_disabled_capture_records_nothing_and_drops_what_it_held() {
        let mut c = Capture::default();
        c.record(0, dir::TO_CONTROLLER, &[0x01, 0x03, 0x0C, 0x00]);
        assert_eq!(c.records.len(), 1);
        c.set_enabled(false);
        assert_eq!((c.records.len(), c.bytes), (0, 0));
        c.record(1, dir::TO_CONTROLLER, &[0x01, 0x03, 0x0C, 0x00]);
        assert!(c.records.is_empty(), "nothing is recorded while it is off");
        assert_eq!(parse(&c.to_btsnoop()).expect("parses").len(), 0);
        c.set_enabled(true);
        c.record(2, dir::TO_CONTROLLER, &[0x01, 0x03, 0x0C, 0x00]);
        assert_eq!(c.records.len(), 1);
    }

    #[test]
    fn packets_without_key_material_are_written_unchanged() {
        let mut c = Capture::default();
        let adv = command(0x2008, &[3, 2, 1, 6]);
        let att = vec![
            0x02, 0x01, 0x20, 0x07, 0x00, 0x03, 0x00, 0x04, 0x00, 0x12, 0x03, 0x00,
        ];
        c.record(0, dir::TO_CONTROLLER, &adv);
        c.record(0, dir::TO_HOST, &att);
        let parsed = parse(&c.to_btsnoop()).expect("parses");
        assert_eq!(parsed[0].packet, adv);
        assert_eq!(parsed[1].packet, att);
    }
}
