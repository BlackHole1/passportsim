//! All host-visible data leaves the core through rings in core memory. Host-to-guest rings are
//! transport only: at every slice boundary the core drains them into `JournalEntry` records, and
//! models consume only journaled input.
//!
//! Every ring numbers its items with absolute `u64` positions that only grow: the window is
//! `[tail, head)` with `head - tail <= capacity`. Readers keep their own cursors and never disturb
//! each other; a read from below `tail` reports how many items were lost (`dropped`) and continues
//! at the oldest kept item. Buffers are allocated once and never move, so the host's typed-array
//! views stay valid. Guest resets never touch `HostIo`.

mod event;
mod frame;
mod lines;
mod pcm;
mod ring;
mod usj;

use serde::{Deserialize, Serialize};

use crate::time::VTime;

pub use event::{EventKind, EventRing, HostEvent};
pub use frame::{FRAME_HEIGHT, FRAME_WIDTH, FramePort};
pub use lines::{LineIndex, LineMark};
pub use pcm::{Injected, PcmRecord, PcmRing};
pub use ring::{ByteRing, RingRead, RingRestoreError, RingSlices};
pub use usj::{UsbHostState, UsjCtrl, UsjEnumeration};

#[derive(Clone, Debug)]
pub struct HostIo {
    /// Raw RGB565 240x320, dirty row span, generation, backlight duty, panel power/sleep/inversion.
    pub frame: FramePort,
    /// Records { vt_start, fs, channels } + i16 samples, sample-exact.
    pub audio_out: PcmRing,
    /// Host -> guest; underflow yields zeros and is counted.
    pub audio_in: PcmRing,
    /// Guest -> host (wire capture).
    pub usj_tx: ByteRing,
    /// Host -> guest.
    pub usj_rx: ByteRing,
    /// Cable, client open, DTR/RTS, enumeration state.
    pub usj_ctrl: UsjCtrl,
    pub uart0_tx: ByteRing,
    /// Reset, panic, sleep, power, frame, ui-settled, fidelity warnings.
    pub events: EventRing,
    pub lines: LineIndex,
}

/// A guest-to-host serial stream indexed by [`LineIndex`].
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum SerialStream {
    #[default]
    UsjTx,
    Uart0Tx,
}

impl SerialStream {
    pub const ALL: [SerialStream; 2] = [SerialStream::UsjTx, SerialStream::Uart0Tx];

    /// Dense index, `0..ALL.len()`.
    pub const fn index(self) -> usize {
        match self {
            SerialStream::UsjTx => 0,
            SerialStream::Uart0Tx => 1,
        }
    }
}

/// Bytes of serial ring per line mark when [`HostIo::new`] sizes a stream's [`LineIndex`]; the
/// budget is per stream, so a chatty stream does not shorten another's index. UNVERIFIED design
/// parameter: IDF log lines are usually longer than 16 bytes, so the index normally spans the
/// whole byte-ring window.
const RING_BYTES_PER_LINE_MARK: usize = 16;

/// Samples per PCM record header when [`HostIo::new`] sizes the header rings. UNVERIFIED design
/// parameter.
const PCM_SAMPLES_PER_RECORD: usize = 256;

/// Byte-ring bytes per event when [`HostIo::new`] sizes the event ring. UNVERIFIED design
/// parameter.
const RING_BYTES_PER_EVENT: usize = 64;

impl HostIo {
    /// Every byte ring of `byte_ring_capacity` bytes, the PCM, event and line rings sized from it,
    /// a blank `frame` and the default host state in `usj_ctrl`.
    pub fn new(byte_ring_capacity: usize) -> Self {
        HostIo {
            frame: FramePort::new(),
            audio_out: PcmRing::new(
                byte_ring_capacity,
                byte_ring_capacity.div_ceil(PCM_SAMPLES_PER_RECORD),
            ),
            audio_in: PcmRing::new(
                byte_ring_capacity,
                byte_ring_capacity.div_ceil(PCM_SAMPLES_PER_RECORD),
            ),
            usj_tx: ByteRing::new(byte_ring_capacity),
            usj_rx: ByteRing::new(byte_ring_capacity),
            usj_ctrl: UsjCtrl::default(),
            uart0_tx: ByteRing::new(byte_ring_capacity),
            events: EventRing::new(byte_ring_capacity.div_ceil(RING_BYTES_PER_EVENT)),
            lines: LineIndex::new(byte_ring_capacity.div_ceil(RING_BYTES_PER_LINE_MARK)),
        }
    }

    pub fn serial_ring(&self, stream: SerialStream) -> &ByteRing {
        match stream {
            SerialStream::UsjTx => &self.usj_tx,
            SerialStream::Uart0Tx => &self.uart0_tx,
        }
    }

    /// Appends guest output emitted at virtual time `vt` to the ring of `stream` and records every
    /// newline in [`HostIo::lines`]. Returns the absolute cursor of the first byte written.
    /// Allocates nothing.
    pub fn serial_write(&mut self, stream: SerialStream, bytes: &[u8], vt: VTime) -> u64 {
        let ring = match stream {
            SerialStream::UsjTx => &mut self.usj_tx,
            SerialStream::Uart0Tx => &mut self.uart0_tx,
        };
        let start = ring.head();
        ring.write(bytes);
        self.lines.index(stream, start, bytes, vt);
        start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn window(ring: &ByteRing, cursor: u64) -> Vec<u8> {
        ring.slices(cursor).iter().copied().collect()
    }

    #[test]
    fn new_builds_rings_of_the_given_capacity() {
        let io = HostIo::new(64);
        assert_eq!(io.usj_tx.capacity(), 64);
        assert_eq!(io.usj_rx.capacity(), 64);
        assert_eq!(io.uart0_tx.capacity(), 64);
        assert_eq!(io.lines.capacity(SerialStream::UsjTx), 4);
        assert_eq!(io.lines.capacity(SerialStream::Uart0Tx), 4);
        assert_eq!(
            (io.audio_out.capacity(), io.audio_out.record_capacity()),
            (64, 1)
        );
        assert_eq!(
            (io.audio_in.capacity(), io.audio_in.record_capacity()),
            (64, 1)
        );
        assert_eq!(io.events.capacity(), 1);
        assert_eq!(io.lines.read_cursors(), [0, 0]);
        assert!(io.usj_tx.is_empty());
        assert_eq!((io.usj_tx.head(), io.usj_tx.tail()), (0, 0));

        assert_eq!(io.frame.pixels().len(), FRAME_WIDTH * FRAME_HEIGHT);
        assert!(io.frame.pixels().iter().all(|&p| p == 0));
        assert_eq!(io.frame.dirty_rows(), None);
        assert_eq!(io.frame.generation(), 0);
        assert_eq!(io.usj_ctrl.host_state(true), UsbHostState::AttachedOpen);
        assert_eq!(io.usj_ctrl.enumeration(), UsjEnumeration::Enumerated);
    }

    #[test]
    fn serial_write_indexes_newlines_per_stream() {
        let mut io = HostIo::new(64);
        let ptr = io.lines.marks_as_ptr(SerialStream::UsjTx);
        assert_eq!(
            io.serial_write(SerialStream::UsjTx, b"boot\nok\n", VTime(5)),
            0
        );
        assert_eq!(io.serial_write(SerialStream::Uart0Tx, b"x\n", VTime(6)), 0);
        assert_eq!(io.serial_write(SerialStream::UsjTx, b"tail", VTime(7)), 8);
        assert_eq!(io.lines.lines(SerialStream::UsjTx), 2);
        assert_eq!(io.lines.lines(SerialStream::Uart0Tx), 1);
        assert_eq!(io.serial_ring(SerialStream::UsjTx).head(), 12);

        let mark = |stream, offset, vt| LineMark {
            stream,
            offset,
            vt: VTime(vt),
        };
        // Each stream reads only its own lines, from its own line cursor.
        let marks: Vec<LineMark> = io
            .lines
            .slices(SerialStream::UsjTx, 0)
            .iter()
            .copied()
            .collect();
        assert_eq!(
            marks,
            [
                mark(SerialStream::UsjTx, 4, 5),
                mark(SerialStream::UsjTx, 7, 5),
            ]
        );
        let uart: Vec<LineMark> = io
            .lines
            .slices(SerialStream::Uart0Tx, 0)
            .iter()
            .copied()
            .collect();
        assert_eq!(uart, [mark(SerialStream::Uart0Tx, 1, 6)]);
        let usj = io.serial_ring(SerialStream::UsjTx);
        assert_eq!(window(usj, marks[1].offset), b"\ntail");
        assert_eq!(io.lines.marks_as_ptr(SerialStream::UsjTx), ptr);
    }
}
