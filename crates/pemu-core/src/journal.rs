//! The input journal: the only path by which anything reaches the machine from outside. The run
//! loop drains the host-to-guest rings into [`JournalEntry`] records, totally ordered by
//! `(at, seq)`, before any model sees them; replaying them delivers the same events at the same
//! virtual times. [`Determinism`] records what the run may claim. It never falls, and a replay
//! recomputes it from the entries, so round-tripping them cannot launder a live run.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::input::InputEvent;
use crate::time::VTime;

/// Live-source notes kept for the receipt; [`Journal::live_note_count`] keeps counting past it.
const MAX_LIVE_NOTES: usize = 16;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Never before the `now` it was appended at.
    pub at: VTime,
    /// Unique and increasing in append order; breaks ties in `at`.
    pub seq: u64,
    pub origin: Origin,
    pub ev: InputEvent,
}

/// Where a journaled input came from, which fixes what the run may claim.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum Origin {
    /// An agent command (`input`, `serial write`, `env`). Scripted.
    #[default]
    Agent,
    /// A scenario step. Scripted.
    Scenario,
    /// A person on the web UI, live microphone included: not scripted, fully captured.
    UiLive,
    /// RFC 2217 line state and bytes from `idf.py` or `esptool`: not scripted, fully captured.
    Endpoint,
    /// A live host peer (bridged network, external HCI, wall clock). Its data is journaled.
    Bridge,
}

impl Origin {
    pub fn class(self) -> Determinism {
        match self {
            Origin::Agent | Origin::Scenario => Determinism::Deterministic,
            Origin::UiLive | Origin::Endpoint => Determinism::Replayable,
            Origin::Bridge => Determinism::Live,
        }
    }
}

/// Determinism class of a run, ordered from the strongest claim to the weakest, so a journal
/// keeps the maximum of what its entries imply.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum Determinism {
    /// Every input was scripted: the run re-runs from its configuration and script.
    #[default]
    Deterministic,
    /// Unscripted input, completely journaled: the run replays exactly from the journal.
    Replayable,
    /// The run depended on a host peer the journal cannot stand in for.
    Live,
}

/// A live stream whose chunks the host numbers, so the journal can see data lost before it.
/// UNVERIFIED numbering rule: this module reads the `seq` of `MicChunk`, `NetFrame` and
/// `HciPacket` as a per-stream counter from 0, rising by one per chunk.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum LiveStream {
    #[default]
    Mic,
    Net,
    Hci,
}

impl LiveStream {
    pub const ALL: [LiveStream; 3] = [LiveStream::Mic, LiveStream::Net, LiveStream::Hci];

    pub const fn index(self) -> usize {
        match self {
            LiveStream::Mic => 0,
            LiveStream::Net => 1,
            LiveStream::Hci => 2,
        }
    }
}

/// Why a run is [`Determinism::Live`]. Receipts name it.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LiveReason {
    BridgeInput,
    /// Recorded even before the peer sends anything, because a restore is already refused.
    BridgeAttached,
    LostData {
        stream: LiveStream,
        expected: u64,
        got: u64,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LiveNote {
    pub at: VTime,
    pub reason: LiveReason,
}

/// Why a recorded journal was refused.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum JournalError {
    /// Entry `index` is not after its predecessor in `(at, seq)` order.
    OutOfOrder { index: usize },
    /// Entry `index` reuses a `seq` an earlier entry already has.
    DuplicateSeq { index: usize, seq: u64 },
    /// Entry `index` carries a `seq` that `next_seq` would hand out again.
    NextSeqTooSmall {
        index: usize,
        seq: u64,
        next_seq: u64,
    },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::OutOfOrder { index } => {
                write!(f, "journal entry {index} is out of (at, seq) order")
            }
            JournalError::DuplicateSeq { index, seq } => {
                write!(f, "journal entry {index} reuses seq {seq}")
            }
            JournalError::NextSeqTooSmall {
                index,
                seq,
                next_seq,
            } => write!(
                f,
                "journal entry {index} has seq {seq}, which next_seq {next_seq} would hand out again"
            ),
        }
    }
}

impl std::error::Error for JournalError {}

/// The [`JournalEntry`] records this session holds, in `(at, seq)` order.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(into = "JournalState", try_from = "JournalState")]
pub struct Journal {
    /// `entries[..applied]` are this session's run record, `entries[applied..]` are pending.
    entries: Vec<JournalEntry>,
    applied: usize,
    /// Applied before a [`Journal::restore`], which takes the count without the entries.
    applied_before: u64,
    next_seq: u64,
    class: Determinism,
    live_notes: Vec<LiveNote>,
    live_note_count: u64,
    live_next: [u64; LiveStream::ALL.len()],
}

/// The snapshot and serialized form of [`Journal`]. The applied entries are absent: they are the
/// run record, not machine state, and a live microphone would make every snapshot copy an
/// unbounded history.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct JournalState {
    pub pending: Vec<JournalEntry>,
    /// An opaque count, not an index into `pending`.
    pub cursor: u64,
    /// Carried: the applied entries that used the earlier numbers are not in `pending`.
    pub next_seq: u64,
    pub class: Determinism,
    pub live_notes: Vec<LiveNote>,
    pub live_note_count: u64,
    pub live_next: [u64; LiveStream::ALL.len()],
}

impl Journal {
    pub fn new() -> Self {
        Journal {
            entries: Vec::new(),
            applied: 0,
            applied_before: 0,
            next_seq: 0,
            class: Determinism::Deterministic,
            live_notes: Vec::new(),
            live_note_count: 0,
            live_next: [0; LiveStream::ALL.len()],
        }
    }

    /// Records `ev` to take effect at `at` and returns its `seq`. A past `at` is clamped to `now`,
    /// so an input never lands before an applied one; the run's [`Determinism`] rises to what
    /// `origin` implies.
    pub fn append(&mut self, now: VTime, at: VTime, origin: Origin, ev: InputEvent) -> u64 {
        let at = at.max(now);
        let seq = self.next_seq;
        self.next_seq += 1;

        self.raise(at, origin.class(), LiveReason::BridgeInput);
        self.track_chunk(at, &ev);

        // Every pending entry at or before `at` has a smaller seq, so inserting after the last of
        // them keeps `entries[applied..]` sorted by (at, seq).
        let mut i = self.entries.len();
        while i > self.applied && self.entries[i - 1].at > at {
            i -= 1;
        }
        self.entries.insert(
            i,
            JournalEntry {
                at,
                seq,
                origin,
                ev,
            },
        );
        seq
    }

    /// Virtual time of the earliest entry not applied yet, so the run loop can end a slice on it.
    pub fn next_time(&self) -> Option<VTime> {
        self.entries.get(self.applied).map(|e| e.at)
    }

    /// The next entry due at `now`, marking it applied. Returned owned so the run loop can apply it
    /// while holding the machine; the journal keeps its own copy.
    pub fn pop_due(&mut self, now: VTime) -> Option<JournalEntry> {
        let i = self.applied;
        if self.entries.get(i)?.at > now {
            return None;
        }
        self.applied = i + 1;
        Some(self.entries[i].clone())
    }

    /// How many entries the run has applied, including those before a [`Journal::restore`].
    pub fn cursor(&self) -> u64 {
        self.applied_before + self.applied as u64
    }

    /// The entries this session holds, in delivery order, applied ones first.
    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    pub fn pending(&self) -> &[JournalEntry] {
        &self.entries[self.applied..]
    }

    /// This session's run record, in delivery order; a restore starts it empty.
    pub fn applied(&self) -> &[JournalEntry] {
        &self.entries[..self.applied]
    }

    /// The run record from absolute position `from`, for a cursor-based reader. A `from` before
    /// the first entry this session holds yields the whole record it has.
    pub fn applied_from(&self, from: u64) -> &[JournalEntry] {
        let i = from
            .saturating_sub(self.applied_before)
            .min(self.applied as u64) as usize;
        &self.entries[i..self.applied]
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn live_next(&self, stream: LiveStream) -> u64 {
        self.live_next[stream.index()]
    }

    /// What the run may claim about itself. Never falls.
    pub fn class(&self) -> Determinism {
        self.class
    }

    /// Why the run is live, oldest first, at most `MAX_LIVE_NOTES` of them.
    pub fn live_notes(&self) -> &[LiveNote] {
        &self.live_notes
    }

    pub fn live_note_count(&self) -> u64 {
        self.live_note_count
    }

    /// Declares an attached live host peer, which makes the run live before it sends anything.
    pub fn mark_live(&mut self, now: VTime, reason: LiveReason) {
        self.raise(now, Determinism::Live, reason);
    }

    /// Rebuilds a journal from a whole recording. Class and live notes are recomputed from the
    /// entries, so a replay claims what its inputs imply, not what the recording session claimed.
    pub fn replay(entries: Vec<JournalEntry>) -> Result<Self, JournalError> {
        // The greatest seq, not the last entry's: a future-stamped input keeps a smaller seq.
        let next_seq = entries
            .iter()
            .map(|e| e.seq)
            .max()
            .map_or(0, |m| m.saturating_add(1));
        check_entries(&entries, next_seq)?;

        let mut journal = Journal::new();
        for entry in &entries {
            journal.raise(entry.at, entry.origin.class(), LiveReason::BridgeInput);
            journal.track_chunk(entry.at, &entry.ev);
        }
        journal.next_seq = next_seq;
        journal.entries = entries;
        Ok(journal)
    }

    /// Restores a journal from [`Journal::save`]. Unlike [`Journal::replay`] it keeps the session's
    /// class, live notes and stream expectations, raising an under-claiming state to what its
    /// pending entries imply.
    pub fn restore(state: JournalState) -> Result<Self, JournalError> {
        check_entries(&state.pending, state.next_seq)?;
        let implied = state
            .pending
            .iter()
            .map(|e| e.origin.class())
            .max()
            .unwrap_or_default();
        let mut live_notes = state.live_notes;
        live_notes.truncate(MAX_LIVE_NOTES);
        Ok(Journal {
            entries: state.pending,
            applied: 0,
            applied_before: state.cursor,
            next_seq: state.next_seq,
            class: state.class.max(implied),
            live_notes,
            live_note_count: state.live_note_count,
            live_next: state.live_next,
        })
    }

    pub fn save(&self) -> JournalState {
        JournalState {
            pending: self.pending().to_vec(),
            cursor: self.cursor(),
            next_seq: self.next_seq,
            class: self.class,
            live_notes: self.live_notes.clone(),
            live_note_count: self.live_note_count,
            live_next: self.live_next,
        }
    }

    /// Notes lost data when a stream's numbering jumps forward. A repeated or late chunk does not
    /// move the expectation back, or the next in-order chunk would look like a second loss.
    fn track_chunk(&mut self, at: VTime, ev: &InputEvent) {
        let Some((stream, got)) = live_chunk(ev) else {
            return;
        };
        let slot = &mut self.live_next[stream.index()];
        let expected = *slot;
        *slot = expected.max(got.saturating_add(1));
        if got > expected {
            self.note(
                at,
                LiveReason::LostData {
                    stream,
                    expected,
                    got,
                },
            );
        }
    }

    fn raise(&mut self, at: VTime, class: Determinism, reason: LiveReason) {
        if class == Determinism::Live {
            self.note(at, reason);
        }
        self.class = self.class.max(class);
    }

    fn note(&mut self, at: VTime, reason: LiveReason) {
        self.class = Determinism::Live;
        self.live_note_count += 1;
        if self.live_notes.len() < MAX_LIVE_NOTES {
            self.live_notes.push(LiveNote { at, reason });
        }
    }
}

impl Default for Journal {
    fn default() -> Self {
        Journal::new()
    }
}

/// Strictly increasing `(at, seq)`, a `seq` used at most once, and a `next_seq` above every `seq`
/// present.
fn check_entries(entries: &[JournalEntry], next_seq: u64) -> Result<(), JournalError> {
    for i in 1..entries.len() {
        let (prev, cur) = (&entries[i - 1], &entries[i]);
        if (cur.at, cur.seq) <= (prev.at, prev.seq) {
            return Err(JournalError::OutOfOrder { index: i });
        }
    }
    for (index, e) in entries.iter().enumerate() {
        if e.seq >= next_seq {
            return Err(JournalError::NextSeqTooSmall {
                index,
                seq: e.seq,
                next_seq,
            });
        }
    }

    // `seq` does not rise with delivery order (a future-stamped input keeps a smaller one), so
    // uniqueness is checked on the sorted numbers, not on neighbours.
    let mut seqs: Vec<(u64, usize)> = entries
        .iter()
        .enumerate()
        .map(|(index, e)| (e.seq, index))
        .collect();
    seqs.sort_unstable();
    for pair in seqs.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(JournalError::DuplicateSeq {
                index: pair[1].1,
                seq: pair[1].0,
            });
        }
    }
    Ok(())
}

fn live_chunk(ev: &InputEvent) -> Option<(LiveStream, u64)> {
    match ev {
        InputEvent::MicChunk { seq, .. } => Some((LiveStream::Mic, *seq)),
        InputEvent::NetFrame { seq, .. } => Some((LiveStream::Net, *seq)),
        InputEvent::HciPacket { seq, .. } => Some((LiveStream::Hci, *seq)),
        _ => None,
    }
}

impl From<Journal> for JournalState {
    fn from(j: Journal) -> Self {
        j.save()
    }
}

impl TryFrom<JournalState> for Journal {
    type Error = JournalError;

    fn try_from(state: JournalState) -> Result<Self, Self::Error> {
        Journal::restore(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{ButtonId, SerialChan};

    fn button(down: bool) -> InputEvent {
        InputEvent::Button {
            id: ButtonId::Ok,
            down,
        }
    }

    fn serial(data: &[u8]) -> InputEvent {
        InputEvent::SerialIn {
            chan: SerialChan::USJ,
            data: data.to_vec(),
        }
    }

    fn entry(at: VTime, seq: u64) -> JournalEntry {
        JournalEntry {
            at,
            seq,
            origin: Origin::Agent,
            ev: button(true),
        }
    }

    /// Appends at several instants, some stamped ahead of `now`, with the loop draining what is
    /// due. Returns the journal and the order the entries came out in.
    fn recorded_run() -> (Journal, Vec<JournalEntry>) {
        let mut j = Journal::new();
        let ms = VTime::from_ms;

        j.append(ms(0), ms(0), Origin::Agent, button(true));
        j.append(ms(0), ms(30), Origin::Scenario, button(false));
        j.append(ms(0), ms(10), Origin::Agent, serial(b"help\r\n"));
        j.append(ms(10), ms(1), Origin::Endpoint, serial(b"x"));

        let mut order = Vec::new();
        for now in [ms(0), ms(10), ms(20), ms(30), ms(40)] {
            while let Some(e) = j.pop_due(now) {
                order.push(e);
            }
            if now == ms(20) {
                j.append(now, ms(15), Origin::UiLive, button(true));
            }
        }
        (j, order)
    }

    #[test]
    fn entries_come_out_in_at_then_seq_order() {
        let (j, order) = recorded_run();
        let ms = VTime::from_ms;
        let times: Vec<(VTime, u64)> = order.iter().map(|e| (e.at, e.seq)).collect();
        assert_eq!(
            times,
            vec![
                (ms(0), 0),  // appended at 0 for 0
                (ms(10), 2), // appended at 0 for 10
                (ms(10), 3), // appended at 10 for 1, clamped to 10
                (ms(20), 4), // appended at 20 for 15, clamped to 20
                (ms(30), 1), // appended at 0 for 30
            ]
        );
        assert_eq!(j.cursor(), 5);
        assert!(j.pending().is_empty());
        assert_eq!(j.applied().len(), 5);
        assert_eq!(j.next_time(), None);
    }

    #[test]
    fn pending_entries_wait_for_their_time() {
        let mut j = Journal::new();
        let ms = VTime::from_ms;
        j.append(ms(0), ms(5), Origin::Agent, button(true));
        j.append(ms(0), ms(5), Origin::Agent, button(false));

        assert_eq!(j.next_time(), Some(ms(5)));
        assert!(j.pop_due(ms(4)).is_none());
        assert_eq!(j.cursor(), 0);
        assert_eq!(j.pending().len(), 2);

        assert_eq!(j.pop_due(ms(5)).map(|e| e.seq), Some(0));
        assert_eq!(j.next_time(), Some(ms(5)));
        assert_eq!(j.pop_due(ms(9)).map(|e| e.seq), Some(1));
        assert!(j.pop_due(ms(9)).is_none());
    }

    #[test]
    fn a_replay_reproduces_the_recorded_order_bit_for_bit() {
        let (recorded, order) = recorded_run();

        let mut replayed =
            Journal::replay(recorded.entries().to_vec()).expect("a recorded journal replays");
        let mut replay_order = Vec::new();
        // The replay drains on a coarser slice grid: the delivery order may not depend on it.
        for now in [VTime::from_ms(25), VTime::from_ms(100)] {
            while let Some(e) = replayed.pop_due(now) {
                replay_order.push(e);
            }
        }

        assert_eq!(replay_order, order);
        assert_eq!(
            postcard::to_allocvec(&replay_order).expect("entries serialize"),
            postcard::to_allocvec(&order).expect("entries serialize"),
        );
        assert_eq!(replayed.cursor(), recorded.cursor());
        assert_eq!(replayed.next_seq(), recorded.next_seq());
        assert_eq!(recorded.class(), Determinism::Replayable);
        assert_eq!(replayed.class(), Determinism::Replayable);
    }

    #[test]
    fn a_broken_recording_is_refused() {
        let ms = VTime::from_ms;
        assert_eq!(
            Journal::replay(vec![entry(ms(5), 0), entry(ms(1), 1)]),
            Err(JournalError::OutOfOrder { index: 1 })
        );
        assert_eq!(
            Journal::replay(vec![entry(ms(5), 1), entry(ms(5), 1)]),
            Err(JournalError::OutOfOrder { index: 1 })
        );
        // In (at, seq) order, but two inputs share an identity.
        assert_eq!(
            Journal::replay(vec![entry(ms(1), 5), entry(ms(2), 5)]),
            Err(JournalError::DuplicateSeq { index: 1, seq: 5 })
        );
        // `seq` may fall along the delivery order.
        assert!(Journal::replay(vec![entry(ms(1), 7), entry(ms(2), 3)]).is_ok());
        assert!(Journal::replay(Vec::new()).is_ok());
    }

    #[test]
    fn a_section_whose_next_seq_would_collide_is_refused() {
        let ms = VTime::from_ms;
        let state = |next_seq| JournalState {
            pending: vec![entry(ms(1), 4)],
            cursor: 9,
            next_seq,
            class: Determinism::Deterministic,
            live_notes: Vec::new(),
            live_note_count: 0,
            live_next: [0; LiveStream::ALL.len()],
        };
        assert_eq!(
            Journal::restore(state(4)),
            Err(JournalError::NextSeqTooSmall {
                index: 0,
                seq: 4,
                next_seq: 4,
            })
        );
        assert!(Journal::restore(state(5)).is_ok());
    }

    #[test]
    fn the_class_follows_the_origins_and_never_falls() {
        let ms = VTime::from_ms;
        let mut j = Journal::new();
        assert_eq!(j.class(), Determinism::Deterministic);

        j.append(ms(0), ms(0), Origin::Agent, button(true));
        j.append(ms(1), ms(1), Origin::Scenario, button(false));
        assert_eq!(j.class(), Determinism::Deterministic);

        j.append(ms(2), ms(2), Origin::UiLive, button(true));
        assert_eq!(j.class(), Determinism::Replayable);
        j.append(ms(3), ms(3), Origin::Endpoint, serial(b"esptool"));
        assert_eq!(j.class(), Determinism::Replayable);

        j.append(ms(4), ms(4), Origin::Agent, button(false));
        assert_eq!(j.class(), Determinism::Replayable);
        assert!(j.live_notes().is_empty());

        assert_eq!(Origin::Agent.class(), Determinism::Deterministic);
        assert_eq!(Origin::Scenario.class(), Determinism::Deterministic);
        assert_eq!(Origin::UiLive.class(), Determinism::Replayable);
        assert_eq!(Origin::Endpoint.class(), Determinism::Replayable);
        assert_eq!(Origin::Bridge.class(), Determinism::Live);
        assert!(Determinism::Deterministic < Determinism::Replayable);
        assert!(Determinism::Replayable < Determinism::Live);
    }

    #[test]
    fn a_live_event_marks_the_run_live() {
        let ms = VTime::from_ms;
        let mut j = Journal::new();
        j.append(ms(0), ms(0), Origin::Agent, button(true));
        assert_eq!(j.class(), Determinism::Deterministic);

        j.append(
            ms(1),
            ms(1),
            Origin::Bridge,
            InputEvent::RtcEpoch {
                unix_us: 1_700_000_000_000_000,
            },
        );
        assert_eq!(j.class(), Determinism::Live);
        assert_eq!(
            j.live_notes(),
            [LiveNote {
                at: ms(1),
                reason: LiveReason::BridgeInput,
            }]
        );
        assert_eq!(j.live_note_count(), 1);

        // The payload is in the journal, so a replay needs no bridge but is still live.
        let replayed = Journal::replay(j.entries().to_vec()).expect("replays");
        assert_eq!(replayed.class(), Determinism::Live);
        assert_eq!(replayed.entries(), j.entries());
    }

    #[test]
    fn an_attached_bridge_marks_the_run_live() {
        let mut j = Journal::new();
        j.mark_live(VTime::from_ms(7), LiveReason::BridgeAttached);
        assert_eq!(j.class(), Determinism::Live);
        assert_eq!(j.live_notes()[0].reason, LiveReason::BridgeAttached);
        assert_eq!(j.live_notes()[0].at, VTime::from_ms(7));
        assert!(j.entries().is_empty());
    }

    #[test]
    fn a_gap_in_a_live_stream_marks_the_run_live() {
        let ms = VTime::from_ms;
        let mut j = Journal::new();
        let chunk = |seq| InputEvent::MicChunk {
            seq,
            samples: vec![0; 4],
        };
        j.append(ms(0), ms(0), Origin::UiLive, chunk(0));
        j.append(ms(1), ms(1), Origin::UiLive, chunk(1));
        assert_eq!(j.class(), Determinism::Replayable);

        j.append(ms(2), ms(2), Origin::UiLive, chunk(5));
        assert_eq!(j.class(), Determinism::Live);
        assert_eq!(
            j.live_notes(),
            [LiveNote {
                at: ms(2),
                reason: LiveReason::LostData {
                    stream: LiveStream::Mic,
                    expected: 2,
                    got: 5,
                },
            }]
        );
        // Numbering continues from the chunk that did arrive, so one gap is one note.
        j.append(ms(3), ms(3), Origin::UiLive, chunk(6));
        assert_eq!(j.live_note_count(), 1);

        j.append(
            ms(4),
            ms(4),
            Origin::UiLive,
            InputEvent::NetFrame {
                seq: 0,
                data: vec![1, 2, 3],
            },
        );
        assert_eq!(j.live_note_count(), 1);

        let replayed = Journal::replay(j.entries().to_vec()).expect("replays");
        assert_eq!(replayed.class(), Determinism::Live);
        assert_eq!(replayed.live_notes(), j.live_notes());
    }

    #[test]
    fn a_repeated_chunk_is_not_lost_data() {
        let ms = VTime::from_ms;
        let mut j = Journal::new();
        let chunk = |seq| InputEvent::MicChunk {
            seq,
            samples: vec![0; 4],
        };
        for (i, n) in [0, 1, 2, 1, 3].into_iter().enumerate() {
            j.append(ms(i as u64), ms(i as u64), Origin::UiLive, chunk(n));
        }
        assert_eq!(j.live_notes(), []);
        assert_eq!(j.live_note_count(), 0);
        assert_eq!(j.class(), Determinism::Replayable);

        j.append(ms(5), ms(5), Origin::UiLive, chunk(4));
        assert_eq!(j.live_note_count(), 0);
        j.append(ms(6), ms(6), Origin::UiLive, chunk(9));
        assert_eq!(
            j.live_notes(),
            [LiveNote {
                at: ms(6),
                reason: LiveReason::LostData {
                    stream: LiveStream::Mic,
                    expected: 5,
                    got: 9,
                },
            }]
        );

        let replayed = Journal::replay(j.entries().to_vec()).expect("replays");
        assert_eq!(replayed.live_notes(), j.live_notes());
        assert_eq!(replayed.live_note_count(), j.live_note_count());
    }

    #[test]
    fn a_snapshot_round_trip_keeps_the_cursor_and_the_pending_entries() {
        let ms = VTime::from_ms;
        let (mut j, _) = recorded_run();
        j.append(ms(40), ms(90), Origin::Agent, button(false));
        j.append(ms(40), ms(60), Origin::Scenario, button(true));

        let bytes = postcard::to_allocvec(&j).expect("journal serializes");
        let mut back: Journal = postcard::from_bytes(&bytes).expect("journal deserializes");
        assert_eq!(back.cursor(), j.cursor());
        assert_eq!(back.pending(), j.pending());
        assert_eq!(back.class(), j.class());
        assert_eq!(back.next_seq(), j.next_seq());
        assert_eq!(back.next_time(), j.next_time());
        assert_eq!(back.live_notes(), j.live_notes());
        assert_eq!(back.live_note_count(), j.live_note_count());
        assert_eq!(back.applied(), []);
        assert_eq!(j.applied().len(), 5);

        assert_eq!(
            back.pending().iter().map(|e| e.at).collect::<Vec<_>>(),
            vec![ms(60), ms(90)]
        );
        let seq = back.append(ms(40), ms(50), Origin::Agent, button(true));
        assert_eq!(seq, j.next_seq());
        assert_eq!(back.pop_due(ms(50)).map(|e| e.seq), Some(seq));
        assert_eq!(back.cursor(), j.cursor() + 1);
        // A fixed point, so a chain of snapshots neither grows nor drifts.
        let again = postcard::to_allocvec(&back).expect("journal serializes");
        let twice: Journal = postcard::from_bytes(&again).expect("journal deserializes");
        assert_eq!(twice.save(), back.save());
        assert_eq!(
            postcard::to_allocvec(&twice).expect("journal serializes"),
            again
        );
    }

    #[test]
    fn the_saved_sections_carry_no_applied_entry() {
        let ms = VTime::from_ms;
        let mut j = Journal::new();
        for i in 0..40u64 {
            j.append(
                ms(i),
                ms(i),
                Origin::UiLive,
                InputEvent::MicChunk {
                    seq: i,
                    samples: vec![7; 512],
                },
            );
            while j.pop_due(ms(i)).is_some() {}
        }
        j.append(ms(40), ms(100), Origin::Agent, button(true));

        let state = j.save();
        assert_eq!(state.cursor, 40);
        assert_eq!(state.pending, j.pending());
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.next_seq, 41);
        assert_eq!(j.applied().len(), 40);
        let saved = postcard::to_allocvec(&j).expect("journal serializes");
        let pending_only = postcard::to_allocvec(&j.pending().to_vec()).expect("entries serialize");
        assert!(
            saved.len() < pending_only.len() + 64,
            "the sections are the pending entries plus counters, not the history: {} bytes",
            saved.len()
        );
    }

    /// Deriving the class from the pending entries alone would let a live session come back
    /// deterministic.
    #[test]
    fn a_restore_keeps_the_class_of_the_session_it_saved() {
        let ms = VTime::from_ms;
        let mut j = Journal::new();
        j.mark_live(ms(0), LiveReason::BridgeAttached);
        j.append(ms(0), ms(0), Origin::Agent, button(true));
        j.append(ms(0), ms(50), Origin::Agent, button(false));
        assert_eq!(j.pop_due(ms(0)).map(|e| e.seq), Some(0));
        assert_eq!(j.class(), Determinism::Live);

        let back = Journal::restore(j.save()).expect("the sections restore");
        assert_eq!(back.class(), Determinism::Live);
        assert_eq!(back.live_notes(), j.live_notes());
        assert_eq!(back.live_note_count(), 1);
        assert_eq!(back.cursor(), 1);
        assert_eq!(back.pending().len(), 1);

        let mut weak = j.save();
        weak.class = Determinism::Deterministic;
        weak.pending[0].origin = Origin::UiLive;
        let raised = Journal::restore(weak).expect("the sections restore");
        assert_eq!(raised.class(), Determinism::Replayable);
    }

    /// `next_seq` cannot be derived from the pending entries, which are the only ones left.
    #[test]
    fn a_restore_never_reuses_an_applied_seq() {
        let ms = VTime::from_ms;
        let mut j = Journal::new();
        // Stamped ahead of the run, so it keeps seq 0 and stays pending throughout.
        j.append(ms(0), ms(100), Origin::Scenario, button(true));
        for _ in 0..4 {
            j.append(ms(0), ms(0), Origin::Agent, button(false));
        }
        let applied: Vec<u64> = std::iter::from_fn(|| j.pop_due(ms(0)))
            .map(|e| e.seq)
            .collect();
        assert_eq!(applied, vec![1, 2, 3, 4]);
        assert_eq!((j.cursor(), j.next_seq()), (4, 5));
        assert_eq!(j.pending().iter().map(|e| e.seq).collect::<Vec<_>>(), [0]);

        let mut back = Journal::restore(j.save()).expect("the sections restore");
        assert_eq!(back.next_seq(), 5);
        let seq = back.append(ms(10), ms(10), Origin::Agent, button(true));
        assert_eq!(seq, 5, "a fresh seq, not one of the applied 1..=4");
        assert!(!applied.contains(&seq));
    }

    /// With a borrowed entry this body does not compile (E0499).
    #[test]
    fn due_entries_can_be_applied_from_the_run_loop() {
        struct RunLoop {
            j: Journal,
            seen: Vec<u64>,
        }
        impl RunLoop {
            fn apply_due_journal(&mut self, now: VTime) {
                while let Some(e) = self.j.pop_due(now) {
                    self.apply(e);
                }
            }
            fn apply(&mut self, e: JournalEntry) {
                self.seen.push(e.seq);
            }
        }

        let (j, order) = recorded_run();
        let mut r = RunLoop {
            j: Journal::replay(j.entries().to_vec()).expect("replays"),
            seen: Vec::new(),
        };
        r.apply_due_journal(VTime::from_ms(1_000));
        assert_eq!(r.seen, order.iter().map(|e| e.seq).collect::<Vec<_>>());
    }

    #[test]
    fn applied_entries_keep_their_positions() {
        let (j, order) = recorded_run();
        assert_eq!(j.applied_from(0), order.as_slice());
        assert_eq!(j.applied_from(3), &order[3..]);
        assert_eq!(j.applied_from(order.len() as u64), []);
        assert_eq!(j.applied_from(u64::MAX), []);
        assert_eq!(j.entries().len(), order.len());

        let mut back = Journal::restore(j.save()).expect("the sections restore");
        assert_eq!(back.applied_from(0), []);
        back.append(
            VTime::from_ms(40),
            VTime::from_ms(40),
            Origin::Agent,
            button(true),
        );
        let e = back.pop_due(VTime::from_ms(40)).expect("due");
        assert_eq!(back.applied_from(0), std::slice::from_ref(&e));
        assert_eq!(back.applied_from(5), [e]);
        assert_eq!(back.applied_from(6), []);
    }

    #[test]
    fn an_empty_journal_has_nothing_to_apply() {
        let mut j = Journal::default();
        assert_eq!(j.cursor(), 0);
        assert_eq!(j.next_seq(), 0);
        assert_eq!(j.next_time(), None);
        assert!(j.pop_due(VTime::from_ms(1_000)).is_none());
        assert!(j.entries().is_empty());
        assert!(j.pending().is_empty());
        assert!(j.applied().is_empty());
        assert_eq!(j.class(), Determinism::Deterministic);
        assert_eq!(j.live_note_count(), 0);
    }
}
