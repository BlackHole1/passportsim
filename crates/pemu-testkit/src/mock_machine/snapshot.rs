//! `SnapshotMachine` for `MockMachine`. The mock's state is not serde, so a snapshot holds an index
//! into a store of cloned states shared with its forks, and a `mock.state` section with the
//! state's `Debug` text, which `state_hash` hashes. It restores only into that mock or a fork.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use pemu_core::hostio::HostIo;
use pemu_core::snap::{
    LivePolicy, SectionId, SnapError, SnapHeader, SnapOpts, Snapshot, serde_from_section,
    serde_section,
};
use pemu_core::time::VTime;
use pemu_machine::SnapshotMachine;
use pemu_machine::stops::MatcherId;

use super::MockChan;
use super::script::Reaction;
use super::{
    JournalRecord, MockMachine, MockMatcher, Output, ReleasedEvent, ReleasedFrame, SerialLine,
    StateValue,
};

const SAVED: &str = "mock.saved";
const STATE: &str = "mock.state";

/// Everything of a `MockMachine` a snapshot restores: all of it but the call record and the store.
#[derive(Clone, Debug)]
pub(crate) struct MockState {
    vt: VTime,
    pending: BTreeMap<(VTime, u64), Output>,
    next_order: u64,
    matchers: Vec<(MatcherId, MockMatcher)>,
    reactions: Vec<Reaction>,
    insns_per_us: u64,
    serial: BTreeMap<MockChan, Vec<SerialLine>>,
    frames: Vec<ReleasedFrame>,
    events: Vec<ReleasedEvent>,
    state: BTreeMap<String, StateValue>,
    journal: Vec<JournalRecord>,
    host_io: HostIo,
}

impl MockMachine {
    fn capture(&self) -> MockState {
        MockState {
            vt: self.vt,
            pending: self.pending.clone(),
            next_order: self.next_order,
            matchers: self.matchers.clone(),
            reactions: self.reactions.clone(),
            insns_per_us: self.insns_per_us,
            serial: self.serial.clone(),
            frames: self.frames.clone(),
            events: self.events.clone(),
            state: self.state.clone(),
            journal: self.journal.clone(),
            host_io: self.host_io.clone(),
        }
    }

    fn put_back(&mut self, s: MockState) {
        self.vt = s.vt;
        self.pending = s.pending;
        self.next_order = s.next_order;
        self.matchers = s.matchers;
        self.reactions = s.reactions;
        self.insns_per_us = s.insns_per_us;
        self.serial = s.serial;
        self.frames = s.frames;
        self.events = s.events;
        self.state = s.state;
        self.journal = s.journal;
        self.host_io = s.host_io;
    }

    /// The `Debug` text of the guest-visible state; host rings are left out, as
    /// `Machine::state_hash` leaves out what a host did.
    fn state_text(&self) -> String {
        let mut s = self.capture();
        s.host_io = HostIo::new(0);
        format!("{s:?}")
    }

    fn saved(&self) -> std::sync::MutexGuard<'_, Vec<MockState>> {
        self.saved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn malformed(reason: &'static str) -> SnapError {
    SnapError::Malformed { at: "mock", reason }
}

impl SnapshotMachine for MockMachine {
    fn snapshot(&self, opts: SnapOpts) -> Result<Snapshot, SnapError> {
        let index = {
            let mut saved = self.saved();
            saved.push(self.capture());
            saved.len() as u64 - 1
        };
        let mut snap = Snapshot::new(SnapHeader {
            exported: opts.export,
            redacted: opts.export && !opts.include_secrets,
            ..SnapHeader::new()
        });
        snap.put_raw(SectionId::new(SAVED), serde_section(&index, 1, SAVED)?);
        snap.put_raw(
            SectionId::new(STATE),
            serde_section(&self.state_text(), 1, STATE)?,
        );
        Ok(snap)
    }

    /// A mock holds no secret, so redaction is the header stamp only and erases nothing.
    fn redact(
        &self,
        snapshot: &mut Snapshot,
    ) -> Result<pemu_machine::snapshot::Redaction, SnapError> {
        snapshot.header.exported = true;
        snapshot.header.redacted = true;
        Ok(pemu_machine::snapshot::Redaction::default())
    }

    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), SnapError> {
        let id = SectionId::new(SAVED);
        let index: u64 = serde_from_section(snapshot.section(&id)?, id, 1, SAVED)?;
        let state = usize::try_from(index)
            .ok()
            .and_then(|i| self.saved().get(i).cloned())
            .ok_or(malformed(
                "the snapshot was not taken by this mock or its forks",
            ))?;
        self.put_back(state);
        Ok(())
    }

    fn fork(&self, _live: LivePolicy) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
        let mut copy = MockMachine {
            vt: VTime(0),
            pending: BTreeMap::new(),
            next_order: 0,
            matchers: Vec::new(),
            reactions: Vec::new(),
            insns_per_us: 0,
            serial: BTreeMap::new(),
            frames: Vec::new(),
            events: Vec::new(),
            state: BTreeMap::new(),
            journal: Vec::new(),
            calls: RefCell::new(Vec::new()),
            host_io: HostIo::new(0),
            saved: Arc::clone(&self.saved),
        };
        copy.put_back(self.capture());
        Ok(Box::new(copy))
    }

    fn state_hash(&self) -> [u8; 32] {
        let mut snap = Snapshot::new(SnapHeader::new());
        if let Ok(section) = serde_section(&self.state_text(), 1, STATE) {
            snap.put_raw(SectionId::new(STATE), section);
        }
        snap.canonical_hash()
    }
}
