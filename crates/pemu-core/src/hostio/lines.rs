//! The serial line index of `HostIo::lines`: one ring of newline marks per serial stream.

use serde::{Deserialize, Serialize};

use super::SerialStream;
use super::ring::{Ring, RingRead, RingRestoreError, RingSlices, ring_check};
use crate::time::VTime;

/// One newline of a serial stream, as the serial matchers and the per-run serial index read it.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LineMark {
    pub stream: SerialStream,
    /// Absolute cursor of the `\n` byte in the stream's [`ByteRing`](super::ByteRing).
    pub offset: u64,
    pub vt: VTime,
}

/// Serial line index of `HostIo::lines`: one bounded ring of [`LineMark`]s per [`SerialStream`],
/// numbered by line, plus the saved read cursor of each stream's byte ring. Snapshot state that
/// survives guest resets. One ring per stream, so a chatty stream never evicts another's marks.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct LineIndex {
    marks: [Ring<LineMark>; SerialStream::ALL.len()],
    read_cursors: [u64; SerialStream::ALL.len()],
}

impl LineIndex {
    /// Keeps at most `capacity_per_stream` marks for each [`SerialStream`].
    pub fn new(capacity_per_stream: usize) -> Self {
        LineIndex {
            marks: SerialStream::ALL.map(|_| Ring::new(capacity_per_stream)),
            read_cursors: [0; SerialStream::ALL.len()],
        }
    }

    pub fn capacity(&self, stream: SerialStream) -> usize {
        self.marks[stream.index()].capacity()
    }

    /// Absolute line cursor of the next line of `stream`: the count of newlines ever emitted.
    pub fn head(&self, stream: SerialStream) -> u64 {
        self.marks[stream.index()].head
    }

    pub fn tail(&self, stream: SerialStream) -> u64 {
        self.marks[stream.index()].tail
    }

    pub fn len(&self, stream: SerialStream) -> usize {
        self.marks[stream.index()].len()
    }

    pub fn is_empty(&self, stream: SerialStream) -> bool {
        self.marks[stream.index()].is_empty()
    }

    /// Same as [`LineIndex::head`].
    pub fn lines(&self, stream: SerialStream) -> u64 {
        self.head(stream)
    }

    pub fn line_counts(&self) -> [u64; SerialStream::ALL.len()] {
        SerialStream::ALL.map(|s| self.head(s))
    }

    /// Address of the mark buffer of `stream`; it never changes for the life of the index.
    pub fn marks_as_ptr(&self, stream: SerialStream) -> *const LineMark {
        self.marks[stream.index()].as_ptr()
    }

    /// Saved byte cursor of the default reader of `stream`'s ring, for serial reads without an
    /// explicit cursor. Starts at 0; guest resets never move it; saved with the index.
    pub fn read_cursor(&self, stream: SerialStream) -> u64 {
        self.read_cursors[stream.index()]
    }

    pub fn read_cursors(&self) -> [u64; SerialStream::ALL.len()] {
        self.read_cursors
    }

    pub fn set_read_cursor(&mut self, stream: SerialStream, cursor: u64) {
        self.read_cursors[stream.index()] = cursor;
    }

    /// Evicts that stream's oldest mark when full.
    pub fn record(&mut self, mark: LineMark) {
        self.marks[mark.stream.index()].write(&[mark]);
    }

    /// Replaces the whole index in place (snapshot restore): stream `i`'s window becomes
    /// `marks[i]` from line cursor `tails[i]`, its read cursor `read_cursors[i]`. Allocates
    /// nothing; on error nothing changes, not even for a stream whose window was acceptable.
    pub fn restore(
        &mut self,
        tails: [u64; SerialStream::ALL.len()],
        marks: [&[LineMark]; SerialStream::ALL.len()],
        read_cursors: [u64; SerialStream::ALL.len()],
    ) -> Result<(), RingRestoreError> {
        for stream in SerialStream::ALL {
            let i = stream.index();
            ring_check(self.marks[i].capacity(), tails[i], marks[i].len())?;
        }
        for stream in SerialStream::ALL {
            let i = stream.index();
            self.marks[i].restore(tails[i], marks[i])?;
        }
        self.read_cursors = read_cursors;
        Ok(())
    }

    /// Records a mark for every `\n` in `bytes`, whose first byte has absolute cursor `start`.
    /// Allocates nothing.
    pub fn index(&mut self, stream: SerialStream, start: u64, bytes: &[u8], vt: VTime) {
        for (i, _) in bytes.iter().enumerate().filter(|(_, b)| **b == b'\n') {
            self.record(LineMark {
                stream,
                offset: start + i as u64,
                vt,
            });
        }
    }

    /// Copies marks of `stream` from line `cursor` (cursor rules of
    /// [`ByteRing::read`](super::ByteRing::read)); [`RingRead::dropped`] counts only its lines.
    pub fn read(&self, stream: SerialStream, cursor: u64, out: &mut [LineMark]) -> RingRead {
        self.marks[stream.index()].read(cursor, out)
    }

    pub fn slices(&self, stream: SerialStream, cursor: u64) -> RingSlices<'_, LineMark> {
        self.marks[stream.index()].slices(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_evicts_only_the_lines_of_the_chatty_stream() {
        let mut lines = LineIndex::new(2);
        lines.index(SerialStream::Uart0Tx, 10, b"a\nb\nc\n", VTime(1));
        lines.index(SerialStream::UsjTx, 0, b"q\n", VTime(2));
        let uart = SerialStream::Uart0Tx;
        assert_eq!(
            (lines.tail(uart), lines.head(uart), lines.len(uart)),
            (1, 3, 2)
        );
        assert_eq!(lines.line_counts(), [1, 3]);

        // The chatty stream lost its own oldest line and nothing of the quiet one.
        let mut out = [LineMark::default(); 4];
        let r = lines.read(uart, 0, &mut out);
        assert_eq!(
            r,
            RingRead {
                n: 2,
                next: 3,
                dropped: 1
            }
        );
        assert_eq!((out[0].offset, out[1].offset), (13, 15));
        let r = lines.read(SerialStream::UsjTx, 0, &mut out);
        assert_eq!(
            r,
            RingRead {
                n: 1,
                next: 1,
                dropped: 0
            }
        );
        assert_eq!(
            out[0],
            LineMark {
                stream: SerialStream::UsjTx,
                offset: 1,
                vt: VTime(2)
            }
        );
        assert!(!lines.is_empty(SerialStream::UsjTx));
    }

    #[test]
    fn line_index_restore_is_in_place_and_matches_the_original() {
        let mut orig = LineIndex::new(3);
        orig.index(SerialStream::UsjTx, 0, b"a\nb\nc\n", VTime(1));
        orig.index(SerialStream::Uart0Tx, 40, b"\n\n", VTime(2));
        orig.set_read_cursor(SerialStream::UsjTx, 4);
        orig.set_read_cursor(SerialStream::Uart0Tx, 41);
        let (usj, uart) = (SerialStream::UsjTx, SerialStream::Uart0Tx);
        assert_eq!((orig.tail(usj), orig.head(usj)), (0, 3));
        assert_eq!((orig.tail(uart), orig.head(uart)), (0, 2));
        assert_eq!((orig.line_counts(), orig.read_cursors()), ([3, 2], [4, 41]));

        // Save what the hostio section keeps, then restore into a used index.
        let saved = |index: &LineIndex, s| -> Vec<LineMark> {
            index.slices(s, index.tail(s)).iter().copied().collect()
        };
        let (usj_marks, uart_marks) = (saved(&orig, usj), saved(&orig, uart));
        let tails = [orig.tail(usj), orig.tail(uart)];
        let mut copy = LineIndex::new(3);
        copy.index(uart, 0, b"\n\n\n\n\n\n\n", VTime(9));
        let ptrs = [copy.marks_as_ptr(usj), copy.marks_as_ptr(uart)];
        let cursors = orig.read_cursors();
        let marks = [usj_marks.as_slice(), uart_marks.as_slice()];
        assert_eq!(copy.restore(tails, marks, cursors), Ok(()));
        assert_eq!(copy, orig);
        assert_eq!([copy.marks_as_ptr(usj), copy.marks_as_ptr(uart)], ptrs);

        let mut fresh = LineIndex::new(3);
        fresh.restore(tails, marks, cursors).unwrap();
        assert_eq!(fresh, orig);
        // Line counts keep counting from the restored values across later evictions.
        fresh.index(usj, 6, b"d\n", VTime(3));
        orig.index(usj, 6, b"d\n", VTime(3));
        assert_eq!(fresh, orig);
        assert_eq!(fresh.lines(usj), 4);
        assert_eq!(fresh.tail(usj), 1);

        // A refused restore changes nothing, not even for the stream whose window fitted.
        let before = fresh.clone();
        let long = [LineMark::default(); 4];
        assert_eq!(
            fresh.restore([0, 0], [marks[0], &long], [0; 2]),
            Err(RingRestoreError::TooLong {
                len: 4,
                capacity: 3
            })
        );
        assert_eq!(
            fresh.restore([0, u64::MAX], marks, [0; 2]),
            Err(RingRestoreError::CursorOverflow)
        );
        assert_eq!(fresh, before);
    }
}
