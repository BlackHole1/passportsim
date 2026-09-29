//! `MockMachine` state: pending and released outputs, the input journal and the call record,
//! with the read accessors surface tests assert on.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pemu_core::hostio::HostIo;
use pemu_core::time::VTime;
use pemu_machine::stops::MatcherId;

use super::script::Reaction;
use super::{
    JournalRecord, MockCall, MockChan, MockMatcher, MockScript, Output, ReleasedEvent,
    ReleasedFrame, SerialLine, StateValue,
};

/// A scripted `MachineApi` implementation; build one with `MockScript::build`. Virtual time starts
/// at 0 and moves only inside `MachineApi::run`.
pub struct MockMachine {
    pub(super) vt: VTime,
    pub(super) pending: BTreeMap<(VTime, u64), Output>,
    pub(super) next_order: u64,
    pub(super) matchers: Vec<(MatcherId, MockMatcher)>,
    pub(super) reactions: Vec<Reaction>,
    pub(super) insns_per_us: u64,
    pub(super) serial: BTreeMap<MockChan, Vec<SerialLine>>,
    pub(super) frames: Vec<ReleasedFrame>,
    pub(super) events: Vec<ReleasedEvent>,
    pub(super) state: BTreeMap<String, StateValue>,
    pub(super) journal: Vec<JournalRecord>,
    /// A `RefCell` because `now` and `taint` take `&self`.
    pub(super) calls: RefCell<Vec<MockCall>>,
    pub(super) host_io: HostIo,
    /// What `SnapshotMachine::snapshot` saved, shared with every fork, so a snapshot of either
    /// restores into either.
    pub(super) saved: Arc<Mutex<Vec<super::snapshot::MockState>>>,
}

impl MockMachine {
    pub(super) fn from_script(script: MockScript) -> MockMachine {
        let MockScript {
            timeline,
            matchers,
            reactions,
            insns_per_us,
            io_capacity,
        } = script;
        let mut m = MockMachine {
            vt: VTime(0),
            pending: BTreeMap::new(),
            next_order: 0,
            matchers,
            reactions,
            insns_per_us,
            serial: BTreeMap::new(),
            frames: Vec::new(),
            events: Vec::new(),
            state: BTreeMap::new(),
            journal: Vec::new(),
            calls: RefCell::new(Vec::new()),
            host_io: HostIo::new(io_capacity),
            saved: Arc::default(),
        };
        for (vt, out) in timeline.items() {
            m.schedule(*vt, out.clone());
        }
        m
    }

    /// Queue `out` for release at `vt`, after every output already queued for that instant.
    pub(super) fn schedule(&mut self, vt: VTime, out: Output) {
        self.pending.insert((vt, self.next_order), out);
        self.next_order += 1;
    }

    pub(super) fn record(&self, call: MockCall) {
        self.calls.borrow_mut().push(call);
    }

    /// Current virtual time, without recording a call.
    pub(super) fn vt(&self) -> VTime {
        self.vt
    }

    pub(super) fn host_io(&mut self) -> &mut HostIo {
        &mut self.host_io
    }

    /// Released lines of `chan`, oldest first; `cursor` counts from 0 per channel.
    pub fn serial_lines(&self, chan: MockChan) -> &[SerialLine] {
        self.serial.get(&chan).map_or(&[], Vec::as_slice)
    }

    /// Released lines of `chan` whose cursor is at least `cursor`.
    pub fn serial_since(&self, chan: MockChan, cursor: u64) -> &[SerialLine] {
        let lines = self.serial_lines(chan);
        let start = usize::try_from(cursor).map_or(lines.len(), |c| c.min(lines.len()));
        &lines[start..]
    }

    /// Released frames, oldest first; generations count from 1.
    pub fn frames(&self) -> &[ReleasedFrame] {
        &self.frames
    }

    pub fn last_frame(&self) -> Option<&ReleasedFrame> {
        self.frames.last()
    }

    pub fn events(&self) -> &[ReleasedEvent] {
        &self.events
    }

    pub fn state(&self, key: &str) -> Option<&StateValue> {
        self.state.get(key)
    }

    pub fn states(&self) -> &BTreeMap<String, StateValue> {
        &self.state
    }

    pub fn journal(&self) -> &[JournalRecord] {
        &self.journal
    }

    pub fn calls(&self) -> Vec<MockCall> {
        self.calls.borrow().clone()
    }

    pub fn take_calls(&mut self) -> Vec<MockCall> {
        self.calls.get_mut().split_off(0)
    }

    /// Arm `matcher` as `id` for the runs that follow, replacing one already armed as `id` in
    /// place. A wait lasts one `run` call, so a host surface arms before the `run` and disarms
    /// after; script-armed matchers instead fire again in every later run.
    pub fn arm_matcher(&mut self, id: MatcherId, matcher: MockMatcher) {
        match self.matchers.iter_mut().find(|(i, _)| *i == id) {
            Some(slot) => slot.1 = matcher,
            None => self.matchers.push((id, matcher)),
        }
    }

    pub fn disarm_matcher(&mut self, id: MatcherId) -> bool {
        let before = self.matchers.len();
        self.matchers.retain(|(i, _)| *i != id);
        self.matchers.len() != before
    }

    pub fn disarm_all_matchers(&mut self) {
        self.matchers.clear();
    }

    /// The armed matchers, in arming order; the first match of a released output wins.
    pub fn armed_matchers(&self) -> &[(MatcherId, MockMatcher)] {
        &self.matchers
    }

    pub fn pending_outputs(&self) -> usize {
        self.pending.len()
    }

    pub fn next_output_at(&self) -> Option<VTime> {
        self.pending.keys().next().map(|(vt, _)| *vt)
    }
}
