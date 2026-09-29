//! Records of a `MockMachine` session: comparable input summaries, the input journal and the
//! trait call record.

use pemu_core::input::{ButtonId, InputEvent, SerialChan};
use pemu_core::time::VTime;
use pemu_machine::machine::{At, InputError};
use pemu_machine::stops::{StopReason, StopSet};

/// A comparable copy of an `InputEvent`. Variants whose payload types lack `Clone` keep only what
/// can be copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MockInput {
    Button {
        id: ButtonId,
        down: bool,
    },
    Power {
        down: bool,
    },
    UsbCable {
        plugged: bool,
    },
    UsbClient {
        open: bool,
    },
    UsbLine {
        dtr: bool,
        rts: bool,
    },
    SerialIn {
        chan: SerialChan,
        data: Vec<u8>,
    },
    /// `BatterySet` is a placeholder.
    Battery,
    /// Number of NFC operations; `NfcOp` is a placeholder.
    NfcTap {
        ops: usize,
    },
    MicChunk {
        seq: u64,
        samples: Vec<i16>,
    },
    NetFrame {
        seq: u64,
        data: Vec<u8>,
    },
    HciPacket {
        seq: u64,
        data: Vec<u8>,
    },
    RtcEpoch {
        unix_us: u64,
    },
    /// `EnvChange` is a placeholder.
    Env,
}

impl From<&InputEvent> for MockInput {
    fn from(ev: &InputEvent) -> MockInput {
        match ev {
            InputEvent::Button { id, down } => MockInput::Button {
                id: *id,
                down: *down,
            },
            InputEvent::Power { down } => MockInput::Power { down: *down },
            InputEvent::UsbCable { plugged } => MockInput::UsbCable { plugged: *plugged },
            InputEvent::UsbClient { open } => MockInput::UsbClient { open: *open },
            InputEvent::UsbLine { dtr, rts } => MockInput::UsbLine {
                dtr: *dtr,
                rts: *rts,
            },
            InputEvent::SerialIn { chan, data } => MockInput::SerialIn {
                chan: *chan,
                data: data.clone(),
            },
            InputEvent::Battery(_) => MockInput::Battery,
            InputEvent::NfcTap { ops } => MockInput::NfcTap { ops: ops.len() },
            InputEvent::MicChunk { seq, samples } => MockInput::MicChunk {
                seq: *seq,
                samples: samples.clone(),
            },
            InputEvent::NetFrame { seq, data } => MockInput::NetFrame {
                seq: *seq,
                data: data.clone(),
            },
            InputEvent::HciPacket { seq, data } => MockInput::HciPacket {
                seq: *seq,
                data: data.clone(),
            },
            InputEvent::RtcEpoch { unix_us } => MockInput::RtcEpoch { unix_us: *unix_us },
            InputEvent::Env(_) => MockInput::Env,
        }
    }
}

/// One journaled input: its sequence number (from 0) and the instant it applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalRecord {
    pub seq: u64,
    pub vt: VTime,
    pub input: MockInput,
}

/// One `MachineApi` call as the mock saw it, in call order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MockCall {
    /// `run`: its limits and the outcome it returned.
    Run {
        until: Option<VTime>,
        max_insns: Option<u64>,
        stops: StopSet,
        reason: StopReason,
        vt: VTime,
        insns: u64,
    },
    /// `input`: the requested instant, the input and the result.
    Input {
        at: At,
        input: MockInput,
        result: Result<u64, InputError>,
    },
    /// `io`.
    Io,
    /// `now` and the time it returned.
    Now { vt: VTime },
    /// `guest_mem`.
    GuestMem,
    /// `receipt`, at the virtual time of the call.
    Receipt { vt: VTime },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t0_input_summary_copies_the_payload() {
        let ev = InputEvent::SerialIn {
            chan: SerialChan(1),
            data: b"ping".to_vec(),
        };
        assert_eq!(
            MockInput::from(&ev),
            MockInput::SerialIn {
                chan: SerialChan(1),
                data: b"ping".to_vec()
            }
        );
        let ev = InputEvent::Button {
            id: ButtonId::Ok,
            down: false,
        };
        assert_eq!(
            MockInput::from(&ev),
            MockInput::Button {
                id: ButtonId::Ok,
                down: false
            }
        );
        let ev = InputEvent::NfcTap { ops: Vec::new() };
        assert_eq!(MockInput::from(&ev), MockInput::NfcTap { ops: 0 });
    }
}
