//! The WAV files of the artifact directory: `audio/tx-<seq>.wav` is the captured speaker path and
//! `audio/rx-<seq>.wav` the injected mic path.
//!
//! The codec path is 16-bit linear PCM, so the 44-byte RIFF header is written by hand. [`encode`]
//! builds a whole file in memory; [`WavWriter`] streams to a seekable sink and patches the two
//! length fields at the end, so a long capture is never held in memory.

use std::io::{self, Seek, SeekFrom, Write};

pub const HEADER_BYTES: usize = 44;

const FORMAT_PCM: u16 = 1;

/// Byte offset of the `RIFF` chunk size, which counts everything after it.
const RIFF_SIZE_AT: u64 = 4;

const DATA_SIZE_AT: u64 = 40;

/// The format of one WAV file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
}

impl WavSpec {
    /// A 16-bit mono spec at `sample_rate`, the shape of the I2S paths.
    pub fn mono(sample_rate: u32) -> WavSpec {
        WavSpec {
            sample_rate,
            channels: 1,
            bits: 16,
        }
    }

    pub fn frame_bytes(&self) -> u32 {
        u32::from(self.channels) * u32::from(self.bits.div_ceil(8))
    }

    /// Bytes one second occupies, the `ByteRate` field.
    pub fn byte_rate(&self) -> u32 {
        self.sample_rate.saturating_mul(self.frame_bytes())
    }
}

/// The 44-byte canonical header for `data_bytes` of samples. Only the two size fields depend on
/// the sample count, so [`WavWriter`] writes zeros first and patches them later.
pub fn header(spec: &WavSpec, data_bytes: u32) -> [u8; HEADER_BYTES] {
    let mut out = [0u8; HEADER_BYTES];
    let mut put = |at: usize, bytes: &[u8]| out[at..at + bytes.len()].copy_from_slice(bytes);
    put(0, b"RIFF");
    put(4, &(36u32.saturating_add(data_bytes)).to_le_bytes());
    put(8, b"WAVE");
    put(12, b"fmt ");
    put(16, &16u32.to_le_bytes());
    put(20, &FORMAT_PCM.to_le_bytes());
    put(22, &spec.channels.to_le_bytes());
    put(24, &spec.sample_rate.to_le_bytes());
    put(28, &spec.byte_rate().to_le_bytes());
    put(32, &(spec.frame_bytes() as u16).to_le_bytes());
    put(34, &spec.bits.to_le_bytes());
    put(36, b"data");
    put(40, &data_bytes.to_le_bytes());
    out
}

/// A whole WAV file over interleaved samples already in memory.
pub fn encode(spec: &WavSpec, samples: &[i16]) -> Vec<u8> {
    let data_bytes = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(HEADER_BYTES + samples.len() * 2);
    out.extend_from_slice(&header(spec, data_bytes));
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Why [`decode`] refused a file. The text never names a path, because it reaches an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Reads a 16-bit linear PCM RIFF/WAVE file (one or two channels) into its spec and interleaved
/// samples. Chunks other than `fmt ` and `data` are skipped by their size, with the RIFF pad byte
/// after an odd-sized chunk.
pub fn decode(bytes: &[u8]) -> Result<(WavSpec, Vec<i16>), DecodeError> {
    let fail = |why: &str| Err(DecodeError(why.to_string()));
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return fail("not a RIFF WAVE file");
    }
    let mut at = 12usize;
    let mut spec: Option<WavSpec> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("four bytes"));
        let body = at + 8;
        let end = body.saturating_add(size as usize);
        if end > bytes.len() {
            return fail("a chunk runs past the end of the file");
        }
        match id {
            b"fmt " => {
                if size < 16 {
                    return fail("the `fmt ` chunk is shorter than 16 bytes");
                }
                let u16_at = |o: usize| u16::from_le_bytes([bytes[body + o], bytes[body + o + 1]]);
                let format = u16_at(0);
                let channels = u16_at(2);
                let sample_rate =
                    u32::from_le_bytes(bytes[body + 4..body + 8].try_into().expect("four bytes"));
                let bits = u16_at(14);
                if format != FORMAT_PCM {
                    return fail("not uncompressed PCM (WAVE_FORMAT_PCM)");
                }
                if bits != 16 {
                    return fail("not 16-bit PCM");
                }
                if !(1..=2).contains(&channels) {
                    return fail("not one or two channels");
                }
                if sample_rate == 0 {
                    return fail("a sample rate of 0");
                }
                spec = Some(WavSpec {
                    sample_rate,
                    channels,
                    bits,
                });
            }
            b"data" => {
                let Some(spec) = spec else {
                    return fail("the `data` chunk comes before `fmt `");
                };
                let frame = spec.frame_bytes() as usize;
                if !(size as usize).is_multiple_of(frame) {
                    return fail("the `data` chunk does not hold whole frames");
                }
                let samples = bytes[body..end]
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();
                return Ok((spec, samples));
            }
            _ => {}
        }
        at = end + (size as usize & 1);
    }
    fail("no `data` chunk")
}

/// A streaming WAV writer that patches its lengths when it finishes. A capture still running is a
/// readable file up to its header's claim.
#[derive(Debug)]
pub struct WavWriter<W: Write + Seek> {
    sink: W,
    spec: WavSpec,
    data_bytes: u32,
    finished: bool,
}

impl<W: Write + Seek> WavWriter<W> {
    pub fn new(mut sink: W, spec: WavSpec) -> io::Result<WavWriter<W>> {
        sink.write_all(&header(&spec, 0))?;
        Ok(WavWriter {
            sink,
            spec,
            data_bytes: 0,
            finished: false,
        })
    }

    pub fn spec(&self) -> &WavSpec {
        &self.spec
    }

    /// Frames written so far, counted like `n_frames` of an `AUD1` frame.
    pub fn frames(&self) -> u32 {
        let per_frame = self.spec.frame_bytes().max(1);
        self.data_bytes / per_frame
    }

    pub fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        self.sink.write_all(&bytes)?;
        self.data_bytes = self
            .data_bytes
            .saturating_add(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        Ok(())
    }

    /// Patches the two size fields and flushes. Idempotent, so an artifact flush and `Drop` can
    /// both call it.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let end = self.sink.stream_position()?;
        self.sink.seek(SeekFrom::Start(RIFF_SIZE_AT))?;
        self.sink
            .write_all(&(36u32.saturating_add(self.data_bytes)).to_le_bytes())?;
        self.sink.seek(SeekFrom::Start(DATA_SIZE_AT))?;
        self.sink.write_all(&self.data_bytes.to_le_bytes())?;
        self.sink.seek(SeekFrom::Start(end))?;
        self.sink.flush()
    }

    /// The sink. There is no `into_inner` because `Drop` patches the lengths; write into a buffer
    /// you own (`Cursor::new(&mut bytes)`) and read it after the writer is dropped.
    pub fn get_ref(&self) -> &W {
        &self.sink
    }
}

impl<W: Write + Seek> Drop for WavWriter<W> {
    /// Patches the lengths if [`WavWriter::finish`] was not called, so a panicking run still leaves
    /// a playable capture. Errors here are lost, so the flush path calls `finish` explicitly.
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_u32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
    }

    fn read_u16(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(bytes[at..at + 2].try_into().expect("two bytes"))
    }

    #[test]
    fn decode_reads_back_what_encode_wrote_and_skips_unknown_chunks() {
        let spec = WavSpec {
            sample_rate: 16_000,
            channels: 2,
            bits: 16,
        };
        let samples = [0i16, -1, 32_767, -32_768, 5, 6];
        let bytes = encode(&spec, &samples);
        assert_eq!(decode(&bytes), Ok((spec, samples.to_vec())));

        // A LIST chunk of odd size, with its pad byte, between `fmt ` and `data`.
        let mut with_list = bytes[..36].to_vec();
        with_list.extend_from_slice(b"LIST");
        with_list.extend_from_slice(&3u32.to_le_bytes());
        with_list.extend_from_slice(b"abc\0");
        with_list.extend_from_slice(&bytes[36..]);
        assert_eq!(decode(&with_list), Ok((spec, samples.to_vec())));
    }

    #[test]
    fn decode_refuses_what_the_mic_path_cannot_carry_without_naming_a_path() {
        let good = encode(&WavSpec::mono(8_000), &[1, 2]);
        let mut float = good.clone();
        float[20] = 3;
        let mut eight_bit = good.clone();
        eight_bit[34] = 8;
        let truncated = &good[..good.len() - 1];
        for (bytes, why) in [
            (&b"not a wav"[..], "RIFF"),
            (&float[..], "PCM"),
            (&eight_bit[..], "16-bit"),
            (truncated, "past the end"),
        ] {
            let error = decode(bytes).expect_err(why);
            assert!(error.0.contains(why), "{error}");
            assert!(!error.0.contains('/'), "{error}");
        }
    }

    #[test]
    fn the_header_is_the_canonical_44_byte_riff_wave() {
        let spec = WavSpec::mono(16_000);
        let file = encode(&spec, &[0, 1, -1, 32767]);
        assert_eq!(&file[0..4], b"RIFF");
        assert_eq!(&file[8..12], b"WAVE");
        assert_eq!(&file[12..16], b"fmt ");
        assert_eq!(read_u32(&file, 16), 16, "a PCM `fmt ` chunk is 16 bytes");
        assert_eq!(read_u16(&file, 20), FORMAT_PCM);
        assert_eq!(read_u16(&file, 22), 1);
        assert_eq!(read_u32(&file, 24), 16_000);
        assert_eq!(
            read_u32(&file, 28),
            32_000,
            "byte rate is rate x frame size"
        );
        assert_eq!(read_u16(&file, 32), 2, "one 16-bit mono frame is two bytes");
        assert_eq!(read_u16(&file, 34), 16);
        assert_eq!(&file[36..40], b"data");
        assert_eq!(read_u32(&file, 40), 8);
        assert_eq!(read_u32(&file, 4), 36 + 8);
        assert_eq!(file.len(), HEADER_BYTES + 8);
    }

    #[test]
    fn samples_are_little_endian_in_order() {
        let file = encode(&WavSpec::mono(8_000), &[0x0102, -2]);
        assert_eq!(&file[HEADER_BYTES..], &[0x02, 0x01, 0xfe, 0xff]);
    }

    #[test]
    fn a_stereo_spec_doubles_the_frame_and_byte_rates() {
        let spec = WavSpec {
            sample_rate: 48_000,
            channels: 2,
            bits: 16,
        };
        assert_eq!(spec.frame_bytes(), 4);
        assert_eq!(spec.byte_rate(), 192_000);
        let file = encode(&spec, &[1, 2, 3, 4]);
        assert_eq!(read_u16(&file, 32), 4);
        assert_eq!(read_u32(&file, 40), 8, "four samples are two stereo frames");
    }

    #[test]
    fn a_streamed_capture_has_the_same_bytes_as_an_encoded_one() {
        let spec = WavSpec::mono(16_000);
        let samples: Vec<i16> = (0..100).map(|n| (n as i16).wrapping_mul(301)).collect();
        let mut streamed = Vec::new();
        {
            let mut writer = WavWriter::new(Cursor::new(&mut streamed), spec).expect("start");
            for chunk in samples.chunks(7) {
                writer.write(chunk).expect("write");
            }
            assert_eq!(writer.frames(), 100);
            writer.finish().expect("finish");
        }
        assert_eq!(streamed, encode(&spec, &samples));
    }

    #[test]
    fn a_writer_dropped_without_finish_still_leaves_correct_lengths() {
        let spec = WavSpec::mono(16_000);
        let mut buffer = Vec::new();
        {
            let mut writer = WavWriter::new(Cursor::new(&mut buffer), spec).expect("start");
            writer.write(&[7; 32]).expect("write");
        }
        assert_eq!(read_u32(&buffer, 40), 64);
        assert_eq!(read_u32(&buffer, 4), 36 + 64);
        assert_eq!(buffer.len(), HEADER_BYTES + 64);
    }

    #[test]
    fn finishing_twice_is_harmless() {
        let spec = WavSpec::mono(16_000);
        let mut bytes = Vec::new();
        {
            let mut writer = WavWriter::new(Cursor::new(&mut bytes), spec).expect("start");
            writer.write(&[7; 32]).expect("write");
            writer.finish().expect("finish");
            writer.finish().expect("finishing twice is harmless");
        }
        assert_eq!(read_u32(&bytes, 40), 64);
        assert_eq!(read_u32(&bytes, 4), 36 + 64);
    }

    #[test]
    fn an_empty_capture_is_a_valid_header_with_no_samples() {
        let spec = WavSpec::mono(16_000);
        let bytes = encode(&spec, &[]);
        assert_eq!(bytes.len(), HEADER_BYTES);
        assert_eq!(read_u32(&bytes, 40), 0);
        assert_eq!(read_u32(&bytes, 4), 36);
        let mut streamed = Vec::new();
        {
            let mut writer = WavWriter::new(Cursor::new(&mut streamed), spec).expect("start");
            assert_eq!(writer.frames(), 0);
            assert_eq!(writer.get_ref().position(), HEADER_BYTES as u64);
            writer.finish().expect("finish");
        }
        assert_eq!(streamed, bytes);
    }
}
