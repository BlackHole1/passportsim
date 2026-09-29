//! The PCM rings of `HostIo::audio_out` and `HostIo::audio_in`: samples and run headers, each
//! numbered by absolute cursors.

use serde::{Deserialize, Serialize};

use super::ring::{Ring, RingRead, RingRestoreError, RingSlices, ring_check};
use crate::time::{VTime, frame_time};

/// Header of a run of PCM samples: the run holds the samples from absolute sample cursor `first`
/// up to the `first` of the next record.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PcmRecord {
    pub vt_start: VTime,
    /// Hz.
    pub fs: u32,
    /// Interleaved channels per frame.
    pub channels: u16,
    pub first: u64,
}

impl PcmRecord {
    /// Virtual time of the frame holding sample `cursor`, sample-exact through [`frame_time`]. A
    /// header with `fs` or `channels` 0 (reachable only through a restore) gives `vt_start`.
    pub fn time_of(&self, cursor: u64) -> VTime {
        if self.fs == 0 || self.channels == 0 {
            return self.vt_start;
        }
        let frame = cursor.saturating_sub(self.first) / u64::from(self.channels);
        frame_time(self.vt_start, frame, self.fs)
    }
}

/// PCM ring: interleaved `i16` samples and [`PcmRecord`] headers, each numbered by absolute
/// cursors, and a count of underflow samples. `audio_out` takes [`PcmRing::write`], which evicts
/// the oldest when full. `audio_in` takes [`PcmRing::push`] and is drained with [`PcmRing::pop`] or
/// [`PcmRing::pop_or_silence`], which pads with counted zeros.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct PcmRing {
    samples: Ring<i16>,
    records: Ring<PcmRecord>,
    underflows: u64,
    dropped: u64,
}

/// What one [`PcmRing::inject`] did with a chunk.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct Injected {
    pub kept: usize,
    /// Samples dropped by the call: evicted older frames, a dropped misaligned buffer, and the
    /// chunk's own samples that did not go in.
    pub dropped: usize,
}

impl PcmRing {
    /// Both capacities are fixed for the ring's lifetime.
    pub fn new(sample_capacity: usize, record_capacity: usize) -> Self {
        PcmRing {
            samples: Ring::new(sample_capacity),
            records: Ring::new(record_capacity),
            underflows: 0,
            dropped: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.samples.capacity()
    }

    pub fn record_capacity(&self) -> usize {
        self.records.capacity()
    }

    /// Total samples ever written or pushed.
    pub fn head(&self) -> u64 {
        self.samples.head
    }

    pub fn tail(&self) -> u64 {
        self.samples.tail
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn free(&self) -> usize {
        self.samples.free()
    }

    pub fn record_head(&self) -> u64 {
        self.records.head
    }

    pub fn record_tail(&self) -> u64 {
        self.records.tail
    }

    /// Zeros handed out by [`PcmRing::pop_or_silence`] because the ring ran dry.
    pub fn underflows(&self) -> u64 {
        self.underflows
    }

    /// Address of the sample buffer; it never changes for the life of the ring.
    pub fn as_ptr(&self) -> *const i16 {
        self.samples.as_ptr()
    }

    /// Address of the record-header buffer; it never changes for the life of the ring.
    pub fn records_as_ptr(&self) -> *const PcmRecord {
        self.records.as_ptr()
    }

    /// Appends the whole frames of `samples` emitted from `vt_start`, evicting the oldest when
    /// full, and returns the samples written. The samples continue the newest record when `fs` and
    /// `channels` match and `vt_start` is that record's next frame time; otherwise a new record
    /// starts. A trailing partial frame is not written. Allocates nothing.
    pub fn write(&mut self, vt_start: VTime, fs: u32, channels: u16, samples: &[i16]) -> usize {
        if fs == 0 || channels == 0 {
            return 0;
        }
        let n = samples.len() - samples.len() % usize::from(channels);
        if n == 0 {
            return 0;
        }
        let head = self.samples.head;
        let continues = self
            .records
            .last()
            .is_some_and(|r| r.fs == fs && r.channels == channels && r.time_of(head) == vt_start);
        if !continues {
            self.records.write(&[PcmRecord {
                vt_start,
                fs,
                channels,
                first: head,
            }]);
        }
        self.samples.write(&samples[..n]);
        n
    }

    /// Appends as many of `samples` as fit without evicting and returns that count. The raw
    /// transport; the microphone path takes [`PcmRing::inject`].
    pub fn push(&mut self, samples: &[i16]) -> usize {
        self.samples.push(samples)
    }

    /// Appends the whole frames of `channels`-slot `samples` (microphone audio), dropping the
    /// oldest buffered frames to make room: audio is real time, so only a chunk longer than the
    /// whole ring loses its own oldest frames. Whole frames keep the guest's RX slot alignment: a
    /// trailing partial frame is dropped, and a buffer that is not whole frames (the slot count
    /// changed) is dropped whole first. Every dropped sample adds to [`PcmRing::dropped`].
    pub fn inject(&mut self, samples: &[i16], channels: u16) -> Injected {
        let stride = usize::from(channels);
        if stride == 0 {
            self.dropped += samples.len() as u64;
            return Injected {
                kept: 0,
                dropped: samples.len(),
            };
        }
        let mut dropped = 0;
        if !self.len().is_multiple_of(stride) {
            dropped += self.discard();
        }
        let whole = samples.len() - samples.len() % stride;
        let room = self.capacity() - self.capacity() % stride;
        // A chunk longer than the ring keeps its newest whole frames.
        let keep = whole.min(room);
        let arriving_dropped = samples.len() - keep;
        let need = keep.saturating_sub(self.free());
        let evict = need.div_ceil(stride) * stride;
        self.samples.tail += evict.min(self.len()) as u64;
        let pushed = self.samples.push(&samples[whole - keep..whole]);
        debug_assert_eq!(pushed, keep);
        dropped += evict + arriving_dropped;
        self.dropped += (evict + arriving_dropped) as u64;
        Injected {
            kept: keep,
            dropped,
        }
    }

    /// Drops every buffered sample and returns the count: audio reaching a capture path that is
    /// not running is lost, as on silicon.
    pub fn discard(&mut self) -> usize {
        let n = self.len();
        self.samples.tail = self.samples.head;
        self.dropped += n as u64;
        n
    }

    /// Counts `n` samples dropped before they reached the ring (a microphone chunk that arrived
    /// while nothing was capturing).
    pub fn count_dropped(&mut self, n: usize) {
        self.dropped += n as u64;
    }

    /// The host-to-guest loss counter, beside [`PcmRing::underflows`].
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// [`PcmRing::restore`] leaves the dropped count alone, so a ring whose count no section
    /// carries (`audio_out`) keeps its own.
    pub fn restore_dropped(&mut self, dropped: u64) {
        self.dropped = dropped;
    }

    pub fn pop(&mut self, out: &mut [i16]) -> usize {
        self.samples.pop(out)
    }

    /// As [`PcmRing::pop`], then fills the rest of `out` with zeros counted in
    /// [`PcmRing::underflows`]. Returns the count of buffered samples copied.
    pub fn pop_or_silence(&mut self, out: &mut [i16]) -> usize {
        let n = self.samples.pop(out);
        out[n..].fill(0);
        self.underflows += (out.len() - n) as u64;
        n
    }

    /// Copies samples from `cursor` (cursor rules of [`ByteRing::read`](super::ByteRing::read)).
    pub fn read(&self, cursor: u64, out: &mut [i16]) -> RingRead {
        self.samples.read(cursor, out)
    }

    pub fn slices(&self, cursor: u64) -> RingSlices<'_, i16> {
        self.samples.slices(cursor)
    }

    pub fn read_records(&self, cursor: u64, out: &mut [PcmRecord]) -> RingRead {
        self.records.read(cursor, out)
    }

    pub fn record_slices(&self, cursor: u64) -> RingSlices<'_, PcmRecord> {
        self.records.slices(cursor)
    }

    /// `None` when the sample is not kept or its header was evicted.
    pub fn record_at(&self, cursor: u64) -> Option<PcmRecord> {
        if cursor < self.samples.tail || cursor >= self.samples.head {
            return None;
        }
        let kept = self.records.slices(self.records.tail);
        kept.iter()
            .take_while(|r| r.first <= cursor)
            .last()
            .copied()
    }

    /// Replaces the sample window, the header window and the underflow count in place (snapshot
    /// restore of undrained `audio_in`). Allocates nothing; on error nothing changes.
    pub fn restore(
        &mut self,
        tail: u64,
        samples: &[i16],
        record_tail: u64,
        records: &[PcmRecord],
        underflows: u64,
    ) -> Result<(), RingRestoreError> {
        ring_check(self.samples.capacity(), tail, samples.len())?;
        ring_check(self.records.capacity(), record_tail, records.len())?;
        self.samples.restore(tail, samples)?;
        self.records.restore(record_tail, records)?;
        self.underflows = underflows;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostio::HostIo;

    #[test]
    fn pcm_write_merges_contiguous_runs_with_sample_exact_times() {
        let mut ring = PcmRing::new(1024, 4);
        let chunk = [7i16; 160];
        let t0 = VTime::from_ms(100);
        assert_eq!(ring.write(t0, 16_000, 1, &chunk), 160);
        // The next chunk starts exactly at the next frame time: same record.
        let t0_next = frame_time(t0, 160, 16_000);
        assert_eq!(ring.write(t0_next, 16_000, 1, &chunk), 160);
        assert_eq!(
            (ring.record_tail(), ring.record_head(), ring.head()),
            (0, 1, 320)
        );

        // A gap, a rate change or a channel change opens a new record.
        let t1 = VTime::from_ms(200);
        let t1_next = frame_time(t1, 160, 16_000);
        ring.write(t1, 16_000, 1, &chunk);
        ring.write(t1_next, 44_100, 1, &chunk[..147]);
        ring.write(VTime::from_ms(300), 44_100, 2, &chunk[..10]);
        let firsts: Vec<u64> = ring.record_slices(0).iter().map(|r| r.first).collect();
        assert_eq!(firsts, [0, 320, 480, 627]);

        let at = |c| ring.record_at(c).unwrap();
        assert_eq!(at(319).time_of(319), frame_time(t0, 319, 16_000));
        assert_eq!(at(320).vt_start, t1);
        // 44.1 kHz: frame 100 starts 10^14 / 44100 ps (floored) after the run start.
        assert_eq!(at(580).time_of(580), VTime(t1_next.0 + 2_267_573_696));
        // Stereo: sample 633 is frame 3.
        assert_eq!(
            at(633).time_of(633),
            VTime(VTime::from_ms(300).0 + 68_027_210)
        );
        assert_eq!(ring.record_at(637), None);

        // Evicting a header leaves its kept samples without a record.
        ring.write(VTime::from_ms(400), 8_000, 1, &[1, 2, 3]);
        assert_eq!((ring.record_tail(), ring.record_head()), (1, 5));
        assert_eq!(ring.record_at(0), None);
        assert_eq!(ring.read(0, &mut [0; 1]).dropped, 0);
    }

    #[test]
    fn pcm_write_keeps_whole_frames_and_refuses_an_invalid_format() {
        let mut ring = PcmRing::new(16, 2);
        assert_eq!(ring.write(VTime(0), 16_000, 2, &[1, 2, 3, 4, 5]), 4);
        assert_eq!(ring.write(VTime(0), 0, 1, &[1]), 0);
        assert_eq!(ring.write(VTime(0), 16_000, 0, &[1]), 0);
        assert_eq!(ring.write(VTime(9), 16_000, 2, &[1]), 0);
        assert_eq!((ring.head(), ring.record_head()), (4, 1));
        let mut out = [0i16; 8];
        let r = ring.read(0, &mut out);
        assert_eq!(&out[..r.n], &[1, 2, 3, 4]);
    }

    #[test]
    fn audio_in_underflow_yields_zeros_and_is_counted() {
        let mut io = HostIo::new(8);
        assert_eq!(io.audio_in.push(&[1, 2, 3, 4, 5, 6, 7, 8, 9]), 8);
        let mut out = [-1i16; 6];
        assert_eq!(io.audio_in.pop_or_silence(&mut out), 6);
        assert_eq!(io.audio_in.underflows(), 0);
        assert_eq!(io.audio_in.pop_or_silence(&mut out), 2);
        assert_eq!(out, [7, 8, 0, 0, 0, 0]);
        assert_eq!(io.audio_in.underflows(), 4);
        assert_eq!(io.audio_in.pop(&mut out), 0);
        assert_eq!(io.audio_in.underflows(), 4);
        assert_eq!((io.audio_in.tail(), io.audio_in.head()), (8, 8));
    }

    /// A full microphone ring drops its oldest whole frames, never the newest chunk and never
    /// half a frame, and counts what it dropped.
    #[test]
    fn audio_in_overflow_drops_the_oldest_whole_stereo_frames() {
        // 9 slots hold 4 stereo frames; the odd slot is never used.
        let mut ring = PcmRing::new(9, 1);
        let frame = |k: i16| [k, -k];
        let chunk = |ks: &[i16]| ks.iter().flat_map(|k| frame(*k)).collect::<Vec<i16>>();
        assert_eq!(
            ring.inject(&chunk(&[1, 2, 3]), 2),
            Injected {
                kept: 6,
                dropped: 0
            }
        );
        // Two more frames need 4 slots and 3 are free: one whole old frame goes, not a slot.
        assert_eq!(
            ring.inject(&chunk(&[4, 5]), 2),
            Injected {
                kept: 4,
                dropped: 2
            }
        );
        assert_eq!((ring.tail(), ring.head(), ring.dropped()), (2, 10, 2));
        let mut out = [0i16; 8];
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(out.to_vec(), chunk(&[2, 3, 4, 5]));

        // A trailing half frame is dropped on arrival and never advances the head.
        assert_eq!(
            ring.inject(&[6, -6, 7], 2),
            Injected {
                kept: 2,
                dropped: 1
            }
        );
        assert_eq!((ring.head(), ring.len(), ring.dropped()), (12, 2, 3));

        // A chunk longer than the ring keeps its newest whole frames and evicts everything.
        let long = chunk(&[10, 11, 12, 13, 14, 15]);
        assert_eq!(
            ring.inject(&long, 2),
            Injected {
                kept: 8,
                dropped: 6
            }
        );
        let mut out = [0i16; 8];
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(out.to_vec(), chunk(&[12, 13, 14, 15]));
        assert_eq!(ring.dropped(), 9);

        // A slot-count change drops the misaligned rest whole before the new frames go in.
        assert_eq!(ring.inject(&[1, 2, 3], 1).kept, 3);
        assert_eq!(
            ring.inject(&chunk(&[20]), 2),
            Injected {
                kept: 2,
                dropped: 3
            }
        );
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.discard(), 2);
        ring.count_dropped(5);
        assert_eq!((ring.len(), ring.dropped(), ring.underflows()), (0, 19, 0));
        assert_eq!(ring.inject(&[1, 2], 0).dropped, 2);
    }

    #[test]
    fn pcm_restore_is_in_place_and_all_or_nothing() {
        let mut orig = PcmRing::new(8, 2);
        orig.write(VTime(5), 16_000, 1, &[1; 20]);
        assert_eq!(orig.pop_or_silence(&mut [0; 10]), 8);
        orig.write(VTime(9), 16_000, 1, &[2; 3]);
        assert_eq!((orig.tail(), orig.head(), orig.underflows()), (20, 23, 2));

        let samples: Vec<i16> = orig.slices(orig.tail()).iter().copied().collect();
        let records: Vec<PcmRecord> = orig.record_slices(0).iter().copied().collect();
        let mut copy = PcmRing::new(8, 2);
        copy.write(VTime(0), 8_000, 2, &[3; 30]);
        let ptrs = (copy.as_ptr(), copy.records_as_ptr());
        let r = copy.restore(orig.tail(), &samples, orig.record_tail(), &records, 2);
        assert_eq!(r, Ok(()));
        assert_eq!(copy, orig);
        assert_eq!((copy.as_ptr(), copy.records_as_ptr()), ptrs);

        let before = copy.clone();
        let too_many = [PcmRecord::default(); 3];
        assert_eq!(
            copy.restore(0, &samples, 0, &too_many, 0),
            Err(RingRestoreError::TooLong {
                len: 3,
                capacity: 2
            })
        );
        assert_eq!(
            copy.restore(0, &[0; 9], 0, &records, 0),
            Err(RingRestoreError::TooLong {
                len: 9,
                capacity: 8
            })
        );
        assert_eq!(copy, before);
    }
}
