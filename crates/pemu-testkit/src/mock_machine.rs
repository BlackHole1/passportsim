//! `MockMachine`: a scripted `MachineApi` so host surfaces can be built and tested without a
//! working core. A `MockScript` lists outputs at virtual times (serial lines, frames, events,
//! state changes, stops); `run` releases every output whose time has come, stopping at a limit, a
//! scripted stop or an armed matcher. Inputs are journaled and can trigger scripted reactions,
//! and every trait call is recorded.
//!
//! No host time and only ordered containers, so identical sessions give identical records.
//! Released outputs are kept on the mock and read through its own methods; `MachineApi::io`
//! returns an empty `HostIo`.

mod api;
mod clock;
mod machine;
mod record;
mod script;
mod snapshot;

pub use machine::MockMachine;
pub use record::{JournalRecord, MockCall, MockInput};
pub use script::{MockMatcher, MockScript, Timeline};

use std::sync::Arc;

use pemu_core::time::VTime;
use pemu_machine::stops::StopReason;

/// Panel width in pixels of a raw RGB565 frame.
pub const FRAME_WIDTH: usize = 240;
/// Panel height in pixels of a raw frame.
pub const FRAME_HEIGHT: usize = 320;

/// Serial channel of a scripted line.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MockChan {
    /// USB Serial/JTAG, `HostIo::usj_tx`.
    Usj,
    /// UART0, `HostIo::uart0_tx`.
    Uart0,
}

/// A raw RGB565 frame with its dirty row span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockFrame {
    /// First dirty row and one past the last dirty row.
    pub dirty_rows: (u16, u16),
    pub pixels: Arc<[u16]>,
}

impl MockFrame {
    pub fn solid(rgb565: u16) -> MockFrame {
        MockFrame {
            dirty_rows: (0, FRAME_HEIGHT as u16),
            pixels: vec![rgb565; FRAME_WIDTH * FRAME_HEIGHT].into(),
        }
    }

    /// # Panics
    ///
    /// If `pixels` does not hold `FRAME_WIDTH * FRAME_HEIGHT` values or the span is not within
    /// the panel; a malformed script is a test bug.
    pub fn new(pixels: Vec<u16>, dirty_rows: (u16, u16)) -> MockFrame {
        assert_eq!(
            pixels.len(),
            FRAME_WIDTH * FRAME_HEIGHT,
            "a mock frame holds 240x320 pixels"
        );
        assert!(
            dirty_rows.0 <= dirty_rows.1 && usize::from(dirty_rows.1) <= FRAME_HEIGHT,
            "dirty rows must lie within the panel"
        );
        MockFrame {
            dirty_rows,
            pixels: pixels.into(),
        }
    }
}

/// One scripted output, released when virtual time reaches its instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// A complete serial line without its newline; matchers see whole lines.
    Serial { chan: MockChan, text: String },
    /// A new display generation.
    Frame(MockFrame),
    /// An event ring entry such as `reset`, `panic` or `ui-settled`.
    Event(String),
    /// A named state value, for example a UI summary or lifecycle field.
    State { key: String, value: String },
    /// `run` returns with this reason at this instant.
    Stop(StopReason),
}

/// A released serial line with its absolute line cursor; cursors survive resets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialLine {
    pub cursor: u64,
    pub vt: VTime,
    pub text: String,
}

/// A released frame with its display generation, counted from 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasedFrame {
    pub generation: u64,
    pub vt: VTime,
    pub frame: MockFrame,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasedEvent {
    pub seq: u64,
    pub vt: VTime,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateValue {
    pub vt: VTime,
    pub value: String,
}
