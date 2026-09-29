//! Builder API for `MockMachine` scripts: timelines of outputs, matchers and input reactions.

use pemu_core::time::VTime;
use pemu_machine::stops::{MatcherId, StopReason};

use super::{MockChan, MockFrame, MockInput, MockMachine, Output};

/// Default instructions per virtual microsecond: 160 MHz at one instruction per cycle.
pub const DEFAULT_INSNS_PER_US: u64 = 160;

/// Default capacity in bytes of each `HostIo` byte ring of the mock.
pub const DEFAULT_IO_CAPACITY: usize = 4096;

/// Outputs at virtual instants. `at` moves a cursor; each output is placed at the cursor, and
/// outputs at the same instant keep their order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timeline {
    cursor: VTime,
    items: Vec<(VTime, Output)>,
}

impl Timeline {
    pub fn new() -> Timeline {
        Timeline::default()
    }

    pub fn at(mut self, vt: VTime) -> Timeline {
        self.cursor = vt;
        self
    }

    pub fn at_ms(self, ms: u64) -> Timeline {
        self.at(VTime::from_ms(ms))
    }

    pub fn serial(self, chan: MockChan, text: impl Into<String>) -> Timeline {
        self.output(Output::Serial {
            chan,
            text: text.into(),
        })
    }

    pub fn frame(self, frame: MockFrame) -> Timeline {
        self.output(Output::Frame(frame))
    }

    pub fn event(self, name: impl Into<String>) -> Timeline {
        self.output(Output::Event(name.into()))
    }

    pub fn state(self, key: impl Into<String>, value: impl Into<String>) -> Timeline {
        self.output(Output::State {
            key: key.into(),
            value: value.into(),
        })
    }

    pub fn stop(self, reason: StopReason) -> Timeline {
        self.output(Output::Stop(reason))
    }

    pub fn output(mut self, out: Output) -> Timeline {
        self.items.push((self.cursor, out));
        self
    }

    pub fn items(&self) -> &[(VTime, Output)] {
        &self.items
    }
}

/// A matcher, checked only when an output of its kind is released; a hit stops `run` with
/// `StopReason::Matcher(id)`. The mock keeps its own armed set rather than reading
/// `RunLimits::stops`; a script matcher stays armed for the session, so for one wait per `run` use
/// `MockMachine::arm_matcher` and `MockMachine::disarm_matcher` around the call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MockMatcher {
    /// A released line on `chan` contains `contains`.
    Serial { chan: MockChan, contains: String },
    /// An event of this name is released.
    Event(String),
    /// Any frame is released (a new display generation).
    Frame,
    /// State `key` is set to `value`.
    State { key: String, value: String },
}

impl MockMatcher {
    pub(super) fn matches(&self, out: &Output) -> bool {
        match (self, out) {
            (MockMatcher::Serial { chan, contains }, Output::Serial { chan: c, text }) => {
                chan == c && text.contains(contains.as_str())
            }
            (MockMatcher::Event(name), Output::Event(n)) => name == n,
            (MockMatcher::Frame, Output::Frame(_)) => true,
            (MockMatcher::State { key, value }, Output::State { key: k, value: v }) => {
                key == k && value == v
            }
            _ => false,
        }
    }
}

/// Outputs scheduled when a journaled input matches `trigger` (`None` matches every input).
/// Timeline instants are offsets from the input's instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Reaction {
    pub(super) trigger: Option<MockInput>,
    pub(super) timeline: Timeline,
}

/// A complete mock session script; `build` gives the `MockMachine`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockScript {
    pub(super) timeline: Timeline,
    pub(super) matchers: Vec<(MatcherId, MockMatcher)>,
    pub(super) reactions: Vec<Reaction>,
    pub(super) insns_per_us: u64,
    pub(super) io_capacity: usize,
}

impl Default for MockScript {
    fn default() -> MockScript {
        MockScript {
            timeline: Timeline::new(),
            matchers: Vec::new(),
            reactions: Vec::new(),
            insns_per_us: DEFAULT_INSNS_PER_US,
            io_capacity: DEFAULT_IO_CAPACITY,
        }
    }
}

impl MockScript {
    pub fn new() -> MockScript {
        MockScript::default()
    }

    /// Append the outputs of `timeline` at their absolute instants.
    pub fn outputs(mut self, timeline: Timeline) -> MockScript {
        self.timeline.items.extend(timeline.items);
        self
    }

    /// Arm `matcher` as `id` for the whole session; when one output triggers several, the first
    /// armed wins.
    pub fn matcher(mut self, id: MatcherId, matcher: MockMatcher) -> MockScript {
        self.matchers.push((id, matcher));
        self
    }

    /// Schedule `reaction` (instants are offsets) each time an input equal to `input` is
    /// journaled, in the order reactions were added.
    pub fn on_input(mut self, input: MockInput, reaction: Timeline) -> MockScript {
        self.reactions.push(Reaction {
            trigger: Some(input),
            timeline: reaction,
        });
        self
    }

    /// Schedule `reaction` (instants are offsets) each time any input is journaled.
    pub fn on_any_input(mut self, reaction: Timeline) -> MockScript {
        self.reactions.push(Reaction {
            trigger: None,
            timeline: reaction,
        });
        self
    }

    /// Instructions per virtual microsecond reported by `run` (0 means none).
    pub fn insns_per_us(mut self, n: u64) -> MockScript {
        self.insns_per_us = n;
        self
    }

    pub fn io_capacity(mut self, bytes: usize) -> MockScript {
        self.io_capacity = bytes;
        self
    }

    pub fn build(self) -> MockMachine {
        MockMachine::from_script(self)
    }
}
