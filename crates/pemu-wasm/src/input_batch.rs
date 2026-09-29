//! The fixed-layout half of `pemu_input`, used on the interactive path so a button press
//! allocates no JSON string. The buffer is an [`InputBatchHeader`], `count` [`InputRecordAbi`],
//! then a blob area; every offset is checked, so a malformed buffer is an error, never a panic.

use pemu_core::input::{ButtonId, InputEvent, SerialChan};
use pemu_core::time::VTime;
use pemu_machine::machine::At;

use crate::layout::{AT_NOW, INPUT_BATCH_MAGIC, InputBatchHeader, InputKind, InputRecordAbi};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchError {
    /// Shorter than an [`InputBatchHeader`].
    TooShort,
    /// The first word is not [`INPUT_BATCH_MAGIC`].
    BadMagic(u32),
    /// `count` records do not fit in the buffer.
    TruncatedRecords { count: u32, available: usize },
    /// The blob area lies outside the buffer.
    BlobOutOfRange { off: u32, len: u32 },
    /// A record's payload lies outside the blob area.
    PayloadOutOfRange { record: u32, off: u32, len: u32 },
    /// A record's payload length is not a whole number of items of its kind.
    PayloadNotAligned { record: u32, len: u32, item: u32 },
    /// `kind` is not an [`InputKind`].
    UnknownKind { record: u32, kind: u32 },
    /// `a` of an [`InputKind::Button`] record is not a `ButtonId`.
    UnknownButton { record: u32, button: u32 },
    /// A structured kind (battery, NFC tap, environment), sent as JSON `{at, event}` instead.
    JsonOnlyKind { record: u32, kind: u32 },
}

/// One decoded record: when it applies and what it is.
pub struct DecodedInput {
    /// `At::Now`, or the slice-boundary virtual time the worker stamped.
    pub at: At,
    pub event: InputEvent,
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_i64(bytes: &[u8], at: usize) -> i64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    i64::from_le_bytes(word)
}

/// Whether `bytes` is a fixed-layout batch: JSON starts with `[` or `{`, the magic with `P`.
pub fn is_fixed_layout(bytes: &[u8]) -> bool {
    bytes
        .first()
        .is_some_and(|first| !matches!(first, b'[' | b'{'))
}

/// Decodes a fixed-layout batch. Every record is checked before any is returned, so a batch is
/// journaled whole or not at all.
pub fn decode(bytes: &[u8]) -> Result<Vec<DecodedInput>, BatchError> {
    let header_size = size_of::<InputBatchHeader>();
    if bytes.len() < header_size {
        return Err(BatchError::TooShort);
    }
    let magic = read_u32(bytes, 0);
    if magic != INPUT_BATCH_MAGIC {
        return Err(BatchError::BadMagic(magic));
    }
    let count = read_u32(bytes, 4);
    let blob_off = read_u32(bytes, 8);
    let blob_len = read_u32(bytes, 12);

    let record_size = size_of::<InputRecordAbi>();
    let available = bytes.len() - header_size;
    let needed = (count as usize).checked_mul(record_size);
    if needed.is_none_or(|needed| needed > available) {
        return Err(BatchError::TruncatedRecords { count, available });
    }
    let blob_end = (blob_off as usize).checked_add(blob_len as usize);
    if blob_end.is_none_or(|end| end > bytes.len()) {
        return Err(BatchError::BlobOutOfRange {
            off: blob_off,
            len: blob_len,
        });
    }

    let mut out = Vec::with_capacity(count as usize);
    for index in 0..count {
        let base = header_size + index as usize * record_size;
        let at_ps = read_i64(bytes, base);
        let kind = read_u32(bytes, base + 8);
        let a = read_u32(bytes, base + 12);
        let b = read_u32(bytes, base + 16);
        let c = read_u32(bytes, base + 20);
        let payload_off = read_u32(bytes, base + 24);
        let payload_len = read_u32(bytes, base + 28);

        let payload = if payload_len == 0 {
            &[][..]
        } else {
            let start = payload_off as usize;
            let end =
                start
                    .checked_add(payload_len as usize)
                    .ok_or(BatchError::PayloadOutOfRange {
                        record: index,
                        off: payload_off,
                        len: payload_len,
                    })?;
            if start < blob_off as usize || end > blob_off as usize + blob_len as usize {
                return Err(BatchError::PayloadOutOfRange {
                    record: index,
                    off: payload_off,
                    len: payload_len,
                });
            }
            &bytes[start..end]
        };

        let at = if at_ps == AT_NOW {
            At::Now
        } else {
            At::Vt(VTime(at_ps.max(0) as u64))
        };
        out.push(DecodedInput {
            at,
            event: event_of(index, kind, a, b, c, payload)?,
        });
    }
    Ok(out)
}

/// The 64-bit sequence number a live-source record splits over `a` and `b`.
fn seq(a: u32, b: u32) -> u64 {
    u64::from(a) | (u64::from(b) << 32)
}

fn samples_of(record: u32, payload: &[u8]) -> Result<Vec<i16>, BatchError> {
    if !payload.len().is_multiple_of(2) {
        return Err(BatchError::PayloadNotAligned {
            record,
            len: payload.len() as u32,
            item: 2,
        });
    }
    Ok(payload
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect())
}

fn event_of(
    record: u32,
    kind: u32,
    a: u32,
    b: u32,
    c: u32,
    payload: &[u8],
) -> Result<InputEvent, BatchError> {
    let down = b != 0;
    let event = match kind {
        k if k == InputKind::Button as u32 => InputEvent::Button {
            id: match a {
                0 => ButtonId::Up,
                1 => ButtonId::Down,
                2 => ButtonId::Ok,
                other => {
                    return Err(BatchError::UnknownButton {
                        record,
                        button: other,
                    });
                }
            },
            down,
        },
        k if k == InputKind::Power as u32 => InputEvent::Power { down },
        k if k == InputKind::UsbCable as u32 => InputEvent::UsbCable { plugged: down },
        k if k == InputKind::UsbClient as u32 => InputEvent::UsbClient { open: down },
        k if k == InputKind::UsbLine as u32 => InputEvent::UsbLine {
            dtr: a != 0,
            rts: b != 0,
        },
        k if k == InputKind::SerialIn as u32 => InputEvent::SerialIn {
            chan: SerialChan(a as u8),
            data: payload.to_vec(),
        },
        // Structured values, not three scalars, and `web/src/worker/input.ts` never writes them
        // here; they travel as JSON rather than in an invented packing, which would be an ABI
        // version bump.
        k if k == InputKind::Battery as u32
            || k == InputKind::NfcTap as u32
            || k == InputKind::Env as u32 =>
        {
            let _ = (a, b, c);
            return Err(BatchError::JsonOnlyKind { record, kind });
        }
        k if k == InputKind::MicChunk as u32 => InputEvent::MicChunk {
            seq: seq(a, b),
            samples: samples_of(record, payload)?,
        },
        k if k == InputKind::NetFrame as u32 => InputEvent::NetFrame {
            seq: seq(a, b),
            data: payload.to_vec(),
        },
        k if k == InputKind::HciPacket as u32 => InputEvent::HciPacket {
            seq: seq(a, b),
            data: payload.to_vec(),
        },
        k if k == InputKind::RtcEpoch as u32 => InputEvent::RtcEpoch { unix_us: seq(a, b) },
        other => {
            return Err(BatchError::UnknownKind {
                record,
                kind: other,
            });
        }
    };
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_kinds_are_json_only() {
        for kind in [InputKind::Battery, InputKind::NfcTap, InputKind::Env] {
            let err = event_of(7, kind as u32, 1, 2, 3, &[9, 9]).expect_err("refused");
            assert_eq!(
                err,
                BatchError::JsonOnlyKind {
                    record: 7,
                    kind: kind as u32
                }
            );
        }
    }

    /// A batch builder with the exact byte layout `web/src/worker/input.ts` writes.
    struct Batch {
        records: Vec<u8>,
        blob: Vec<u8>,
        count: u32,
    }

    impl Batch {
        fn new() -> Self {
            Batch {
                records: Vec::new(),
                blob: Vec::new(),
                count: 0,
            }
        }

        fn push(&mut self, at_ps: i64, kind: InputKind, a: u32, b: u32, payload: &[u8]) {
            let (off, len) = if payload.is_empty() {
                (0u32, 0u32)
            } else {
                let off = self.blob.len() as u32;
                self.blob.extend_from_slice(payload);
                (off, payload.len() as u32)
            };
            self.records.extend_from_slice(&at_ps.to_le_bytes());
            for word in [kind as u32, a, b, 0, off, len] {
                self.records.extend_from_slice(&word.to_le_bytes());
            }
            self.count += 1;
        }

        fn bytes(&self) -> Vec<u8> {
            let header = size_of::<InputBatchHeader>() as u32;
            let blob_off = header + self.records.len() as u32;
            let mut out = Vec::new();
            out.extend_from_slice(&INPUT_BATCH_MAGIC.to_le_bytes());
            out.extend_from_slice(&self.count.to_le_bytes());
            out.extend_from_slice(&blob_off.to_le_bytes());
            out.extend_from_slice(&(self.blob.len() as u32).to_le_bytes());
            out.extend_from_slice(&self.records);
            let mut fixed = out.clone();
            let shift = blob_off;
            for index in 0..self.count as usize {
                let base = size_of::<InputBatchHeader>() + index * size_of::<InputRecordAbi>();
                let len = read_u32(&out, base + 28);
                if len != 0 {
                    let off = read_u32(&out, base + 24) + shift;
                    fixed[base + 24..base + 28].copy_from_slice(&off.to_le_bytes());
                }
            }
            fixed.extend_from_slice(&self.blob);
            fixed
        }
    }

    #[test]
    fn a_json_batch_is_told_from_a_fixed_layout_one_by_its_first_byte() {
        assert!(!is_fixed_layout(b"[{\"at\":\"now\"}]"));
        assert!(!is_fixed_layout(b"{}"));
        assert!(is_fixed_layout(&INPUT_BATCH_MAGIC.to_le_bytes()));
        assert!(!is_fixed_layout(b""));
    }

    #[test]
    fn a_button_press_decodes_with_its_slice_boundary_stamp() {
        let mut batch = Batch::new();
        batch.push(4_000_000, InputKind::Button, 2, 1, &[]);
        batch.push(AT_NOW, InputKind::Power, 0, 0, &[]);
        let decoded = decode(&batch.bytes()).expect("a well-formed batch decodes");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].at, At::Vt(VTime(4_000_000)));
        assert!(matches!(
            decoded[0].event,
            InputEvent::Button {
                id: ButtonId::Ok,
                down: true
            }
        ));
        assert_eq!(decoded[1].at, At::Now);
        assert!(matches!(
            decoded[1].event,
            InputEvent::Power { down: false }
        ));
    }

    #[test]
    fn payloads_are_read_out_of_the_blob_area() {
        let mut batch = Batch::new();
        batch.push(AT_NOW, InputKind::SerialIn, 0, 0, b"help\r\n");
        batch.push(AT_NOW, InputKind::MicChunk, 7, 0, &[0x01, 0x02, 0xFF, 0xFF]);
        let decoded = decode(&batch.bytes()).expect("a well-formed batch decodes");
        match &decoded[0].event {
            InputEvent::SerialIn { chan, data } => {
                assert_eq!(chan.0, 0);
                assert_eq!(data, b"help\r\n");
            }
            _ => panic!("expected SerialIn"),
        }
        match &decoded[1].event {
            InputEvent::MicChunk { seq, samples } => {
                assert_eq!(*seq, 7);
                assert_eq!(samples, &[0x0201, -1]);
            }
            _ => panic!("expected MicChunk"),
        }
    }

    #[test]
    fn the_sequence_number_spans_both_scalar_words() {
        let mut batch = Batch::new();
        batch.push(AT_NOW, InputKind::RtcEpoch, 0x89AB_CDEF, 0x0123_4567, &[]);
        let decoded = decode(&batch.bytes()).expect("a well-formed batch decodes");
        match decoded[0].event {
            InputEvent::RtcEpoch { unix_us } => assert_eq!(unix_us, 0x0123_4567_89AB_CDEF),
            _ => panic!("expected RtcEpoch"),
        }
    }

    fn err(bytes: &[u8]) -> BatchError {
        match decode(bytes) {
            Ok(decoded) => panic!("expected an error, decoded {} records", decoded.len()),
            Err(error) => error,
        }
    }

    #[test]
    fn a_malformed_batch_is_an_error_and_never_a_panic() {
        assert_eq!(err(&[]), BatchError::TooShort);
        assert_eq!(err(&[0; 15]), BatchError::TooShort);
        assert_eq!(err(&[0; 16]), BatchError::BadMagic(0));

        let mut truncated = Vec::new();
        truncated.extend_from_slice(&INPUT_BATCH_MAGIC.to_le_bytes());
        truncated.extend_from_slice(&3u32.to_le_bytes());
        truncated.extend_from_slice(&16u32.to_le_bytes());
        truncated.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            err(&truncated),
            BatchError::TruncatedRecords {
                count: 3,
                available: 0
            }
        );

        let mut batch = Batch::new();
        batch.push(AT_NOW, InputKind::Button, 9, 1, &[]);
        assert_eq!(
            err(&batch.bytes()),
            BatchError::UnknownButton {
                record: 0,
                button: 9
            }
        );

        let mut odd = Batch::new();
        odd.push(AT_NOW, InputKind::MicChunk, 0, 0, &[1, 2, 3]);
        assert_eq!(
            err(&odd.bytes()),
            BatchError::PayloadNotAligned {
                record: 0,
                len: 3,
                item: 2
            }
        );
    }

    #[test]
    fn a_payload_pointing_outside_the_blob_area_is_refused() {
        let mut batch = Batch::new();
        batch.push(AT_NOW, InputKind::SerialIn, 0, 0, b"ab");
        let mut bytes = batch.bytes();
        let base = size_of::<InputBatchHeader>();
        bytes[base + 28..base + 32].copy_from_slice(&64u32.to_le_bytes());
        assert!(matches!(
            err(&bytes),
            BatchError::PayloadOutOfRange { record: 0, .. }
        ));
    }

    #[test]
    fn an_unknown_kind_is_refused_rather_than_dropped() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&INPUT_BATCH_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&48u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&AT_NOW.to_le_bytes());
        for word in [99u32, 0, 0, 0, 0, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        assert_eq!(
            err(&bytes),
            BatchError::UnknownKind {
                record: 0,
                kind: 99
            }
        );
    }
}
