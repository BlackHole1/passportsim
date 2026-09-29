//! pcap capture at the Wi-Fi data-plane boundary: every Ethernet II frame the guest hands
//! `esp_wifi_internal_tx` and every frame the worker hands the RX callback, at the virtual instant
//! it crossed. A frame the RX capacity dropped is not in the file.
//!
//! The capture is Wi-Fi module state, so snapshots carry it and a replay records the same frames.
//! The format is classic libpcap (tcpdump.org `pcap-savefile`), little-endian, microsecond
//! timestamps counted from the Unix epoch in virtual time, so two runs of one journal write the
//! same file. The station MAC may be the device's own, so the written file goes through the
//! secret value pass as a btsnoop file does.

use pemu_core::snap::snap_struct;

/// The most frame bytes a capture keeps, as for btsnoop.
pub const CAPTURE_BYTES: usize = 64 * 1024;
/// `LINKTYPE_ETHERNET` (`DLT_EN10MB`).
pub const LINKTYPE_ETHERNET: u32 = 1;
/// Announced in the global header: no frame is ever cut.
pub const SNAPLEN: u32 = 65_535;

pub mod dir {
    /// Guest to the LAN (`esp_wifi_internal_tx`).
    pub const TX: u8 = 0;
    /// LAN to the guest (the registered RX callback).
    pub const RX: u8 = 1;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// Virtual time, in microseconds.
    pub at_us: u64,
    /// [`dir`].
    pub dir: u8,
    pub frame: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    /// Oldest first.
    pub records: Vec<Record>,
    pub bytes: u64,
    /// Records dropped to stay within [`CAPTURE_BYTES`].
    pub dropped: u64,
}

snap_struct!(Record { at_us, dir, frame });
snap_struct!(Capture {
    records,
    bytes,
    dropped
});

impl Capture {
    /// Records `frame`, dropping the oldest records beyond the bound.
    pub fn record(&mut self, at_us: u64, dir: u8, frame: &[u8]) {
        self.bytes += frame.len() as u64;
        self.records.push(Record {
            at_us,
            dir,
            frame: frame.to_vec(),
        });
        let mut drop = 0;
        while self.bytes > CAPTURE_BYTES as u64 && drop < self.records.len() {
            self.bytes -= self.records[drop].frame.len() as u64;
            drop += 1;
        }
        if drop > 0 {
            self.records.drain(..drop);
            self.dropped += drop as u64;
        }
    }

    pub fn to_pcap(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.bytes as usize + 16 * self.records.len());
        out.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&SNAPLEN.to_le_bytes());
        out.extend_from_slice(&LINKTYPE_ETHERNET.to_le_bytes());
        for r in &self.records {
            let len = r.frame.len() as u32;
            out.extend_from_slice(&((r.at_us / 1_000_000) as u32).to_le_bytes());
            out.extend_from_slice(&((r.at_us % 1_000_000) as u32).to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&r.frame);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::snap::{SnapReader, SnapValue};

    #[test]
    fn a_capture_writes_the_libpcap_format_with_virtual_timestamps() {
        let mut c = Capture::default();
        c.record(1_500_000, dir::TX, &[0xAA; 60]);
        c.record(2_000_001, dir::RX, &[0xBB; 42]);
        let file = c.to_pcap();
        assert_eq!(
            &file[..4],
            &[0xd4, 0xc3, 0xb2, 0xa1],
            "little-endian microsecond magic"
        );
        assert_eq!(u16::from_le_bytes([file[4], file[5]]), 2);
        assert_eq!(u16::from_le_bytes([file[6], file[7]]), 4);
        assert_eq!(u32::from_le_bytes(file[20..24].try_into().unwrap()), 1);
        // First record: 1 s + 500,000 us, 60 bytes.
        let rec = &file[24..];
        assert_eq!(u32::from_le_bytes(rec[0..4].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(rec[4..8].try_into().unwrap()), 500_000);
        assert_eq!(u32::from_le_bytes(rec[8..12].try_into().unwrap()), 60);
        assert_eq!(u32::from_le_bytes(rec[12..16].try_into().unwrap()), 60);
        assert_eq!(file.len(), 24 + 16 + 60 + 16 + 42);
    }

    #[test]
    fn the_oldest_records_leave_first_and_are_counted() {
        let mut c = Capture::default();
        for i in 0..70u64 {
            c.record(i, dir::TX, &vec![0u8; 1_024]);
        }
        assert_eq!(c.bytes, CAPTURE_BYTES as u64);
        assert_eq!(c.records.len(), 64);
        assert_eq!(c.dropped, 6);
        assert_eq!(c.records[0].at_us, 6);
    }

    #[test]
    fn a_capture_round_trips_through_the_module_state() {
        let mut c = Capture::default();
        c.record(7, dir::RX, &[1, 2, 3]);
        let mut bytes = Vec::new();
        c.snap_write(&mut bytes);
        let mut r = SnapReader::new(&bytes, "hle.machine");
        assert_eq!(Capture::snap_read(&mut r).unwrap(), c);
        assert!(r.is_empty());
    }
}
