//! Event scheduler: a binary heap of `(ps, seq, slot)` over a generation-tagged slot table.
//! Cancel bumps the slot generation in O(1); a stale heap entry is dropped when it reaches the top.
//! Snapshots hold the slot table, free list and next `seq`; the heap is rebuilt on restore.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::time::VTime;

/// Key of a scheduled event: the owning model and an owner-local tag.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct EventKey {
    pub owner: Owner,
    pub tag: u16,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub enum Owner {
    Periph(PeriphId),
    Chip(ChipId),
    Radio(RadioId),
    Machine(MachineTimer),
    Journal,
}

/// Handle to a scheduled event; cancelled by a generation bump.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct EventHandle {
    slot: u32,
    r#gen: u32,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct Pending {
    ps: u64,
    seq: u64,
    key: EventKey,
}

/// The generation bumps whenever the event fires or is cancelled, so every handle goes stale.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct Slot {
    r#gen: u32,
    pending: Option<Pending>,
}

/// Heap entries allowed beyond twice the live count before the heap is rebuilt.
const COMPACT_SLACK: usize = 32;

/// Ordered by (time, seq); seq is insertion order and part of the snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(into = "SchedState", try_from = "SchedState")]
pub struct Scheduler {
    heap: BinaryHeap<Reverse<(u64, u64, u32)>>,
    slots: Vec<Slot>,
    /// Reused last-in first-out.
    free: Vec<u32>,
    next_seq: u64,
    live: usize,
}

impl Scheduler {
    /// UNVERIFIED: a design choice.
    pub fn new() -> Self {
        Scheduler {
            heap: BinaryHeap::new(),
            slots: Vec::new(),
            free: Vec::new(),
            next_seq: 0,
            live: 0,
        }
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    /// Schedules `key` at `at`; `at < now` is clamped to `now`.
    pub fn schedule(&mut self, now: VTime, at: VTime, key: EventKey) -> EventHandle {
        let ps = at.max(now).0;
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                self.slots.push(Slot {
                    r#gen: 0,
                    pending: None,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let s = &mut self.slots[slot as usize];
        s.pending = Some(Pending { ps, seq, key });
        self.heap.push(Reverse((ps, seq, slot)));
        self.live += 1;
        EventHandle {
            slot,
            r#gen: s.r#gen,
        }
    }

    /// O(1). A handle whose event already fired or was cancelled is a no-op, even after slot reuse.
    pub fn cancel(&mut self, h: EventHandle) {
        let Some(s) = self.slots.get_mut(h.slot as usize) else {
            return;
        };
        if s.r#gen != h.r#gen || s.pending.is_none() {
            return;
        }
        self.release(h.slot);
        self.discard_stale_top();
        // Also require more heap entries than slots, or a few live events would rescan the
        // peak-size slot table every COMPACT_SLACK cancels.
        if self.heap.len() > 2 * self.live + COMPACT_SLACK && self.heap.len() > self.slots.len() {
            self.rebuild_heap();
        }
    }

    pub fn next_time(&self) -> Option<VTime> {
        self.heap.peek().map(|Reverse((ps, _, _))| VTime(*ps))
    }

    /// Pops the earliest event due at or before `now`.
    pub fn pop_due(&mut self, now: VTime) -> Option<EventKey> {
        let &Reverse((ps, seq, slot)) = self.heap.peek()?;
        if ps > now.0 {
            return None;
        }
        self.heap.pop();
        let key = match self.slots[slot as usize].pending {
            Some(p) if p.seq == seq => p.key,
            // Unreachable while the top is kept live; tolerate it rather than panic.
            _ => {
                self.discard_stale_top();
                return self.pop_due(now);
            }
        };
        self.release(slot);
        self.discard_stale_top();
        Some(key)
    }

    pub fn is_pending(&self, h: EventHandle) -> bool {
        self.time_of(h).is_some()
    }

    pub fn time_of(&self, h: EventHandle) -> Option<VTime> {
        let s = self.slots.get(h.slot as usize)?;
        match s.pending {
            Some(p) if s.r#gen == h.r#gen => Some(VTime(p.ps)),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Pending events in delivery order, `(time, seq, key)`, for `inspect sched`.
    pub fn pending(&self) -> Vec<(VTime, u64, EventKey)> {
        let mut v: Vec<_> = self
            .slots
            .iter()
            .filter_map(|s| s.pending)
            .map(|p| (VTime(p.ps), p.seq, p.key))
            .collect();
        v.sort_unstable_by_key(|&(t, seq, _)| (t, seq));
        v
    }

    /// Moves every pending event `pick` accepts `by_ps` later (saturating), keeping handles and
    /// `seq`; returns how many moved. Light sleep gates the digital clocks, so their events slip.
    pub fn postpone(&mut self, by_ps: u64, mut pick: impl FnMut(&EventKey) -> bool) -> usize {
        let mut moved = 0;
        for slot in &mut self.slots {
            if let Some(p) = slot.pending.as_mut()
                && pick(&p.key)
            {
                p.ps = p.ps.saturating_add(by_ps);
                moved += 1;
            }
        }
        if moved > 0 && by_ps > 0 {
            self.rebuild_heap();
        }
        moved
    }

    fn release(&mut self, slot: u32) {
        let s = &mut self.slots[slot as usize];
        s.pending = None;
        s.r#gen = s.r#gen.wrapping_add(1);
        self.free.push(slot);
        self.live -= 1;
    }

    /// Pops heap tops whose slot no longer holds that `seq`.
    fn discard_stale_top(&mut self) {
        while let Some(&Reverse((_, seq, slot))) = self.heap.peek() {
            if matches!(self.slots[slot as usize].pending, Some(p) if p.seq == seq) {
                break;
            }
            self.heap.pop();
        }
    }

    fn rebuild_heap(&mut self) {
        let entries: Vec<_> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.pending.map(|p| Reverse((p.ps, p.seq, i as u32))))
            .collect();
        self.heap = BinaryHeap::from(entries);
    }
}

/// Compares the canonical state only; the heap may hold different stale entries.
impl PartialEq for Scheduler {
    fn eq(&self, other: &Self) -> bool {
        self.next_seq == other.next_seq && self.slots == other.slots && self.free == other.free
    }
}

impl Eq for Scheduler {}

/// Serialized form of a [`Scheduler`]. Generations and free-list order are state: peripheral
/// sections hold `EventHandle`s, which must stay valid across restore.
#[derive(Serialize, Deserialize)]
struct SchedState {
    next_seq: u64,
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl From<Scheduler> for SchedState {
    fn from(s: Scheduler) -> Self {
        SchedState {
            next_seq: s.next_seq,
            slots: s.slots,
            free: s.free,
        }
    }
}

#[derive(Debug)]
struct SchedStateError(&'static str);

impl fmt::Display for SchedStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid scheduler state: {}", self.0)
    }
}

impl TryFrom<SchedState> for Scheduler {
    type Error = SchedStateError;

    /// The free list must name exactly the empty slots, once each, and every pending `seq` must
    /// be unique and below `next_seq`.
    fn try_from(st: SchedState) -> Result<Self, Self::Error> {
        if st.slots.len() > u32::MAX as usize {
            return Err(SchedStateError("too many slots"));
        }
        let mut free = BTreeSet::new();
        for &f in &st.free {
            let slot = st
                .slots
                .get(f as usize)
                .ok_or(SchedStateError("free slot out of range"))?;
            if slot.pending.is_some() || !free.insert(f) {
                return Err(SchedStateError("free list does not match empty slots"));
            }
        }
        let mut seqs = BTreeSet::new();
        let mut live = 0;
        for p in st.slots.iter().filter_map(|s| s.pending) {
            if p.seq >= st.next_seq || !seqs.insert(p.seq) {
                return Err(SchedStateError(
                    "pending seq repeated or not below next_seq",
                ));
            }
            live += 1;
        }
        // Every free slot is empty and listed once, so equal counts mean every empty slot is free.
        if live + free.len() != st.slots.len() {
            return Err(SchedStateError("free list does not match empty slots"));
        }
        let mut sched = Scheduler {
            heap: BinaryHeap::new(),
            slots: st.slots,
            free: st.free,
            next_seq: st.next_seq,
            live,
        };
        sched.rebuild_heap();
        Ok(sched)
    }
}

/// UNVERIFIED: a design choice, as are the three ids below.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct PeriphId(pub u16);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ChipId(pub u16);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct RadioId(pub u16);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct MachineTimer(pub u16);

#[cfg(test)]
mod tests {
    use super::*;

    fn key(tag: u16) -> EventKey {
        EventKey {
            owner: Owner::Periph(PeriphId(3)),
            tag,
        }
    }

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn orders_by_time_then_insertion_and_clamps_the_past() {
        let mut s = Scheduler::default();
        let now = VTime(100);
        s.schedule(now, VTime(300), key(1));
        s.schedule(now, VTime(200), key(2));
        s.schedule(now, VTime(300), key(3));
        s.schedule(now, VTime(50), key(4)); // clamped to now
        assert_eq!(s.len(), 4);
        assert_eq!(s.next_time(), Some(now));
        assert_eq!(s.pop_due(VTime(99)), None);
        assert_eq!(s.pop_due(now), Some(key(4)));
        assert_eq!(s.pop_due(now), None);
        assert_eq!(s.pop_due(VTime(1_000)), Some(key(2)));
        assert_eq!(s.pop_due(VTime(1_000)), Some(key(1)));
        assert_eq!(s.pop_due(VTime(1_000)), Some(key(3)));
        assert_eq!(s.pop_due(VTime(1_000)), None);
        assert!(s.is_empty());
        assert_eq!(s.next_time(), None);
        assert_eq!(s.next_seq, 4);
        assert_eq!(s.slots.len(), 4);
        assert_eq!(s.free.len(), 4);
        assert!(s.slots.iter().all(|sl| sl.pending.is_none()));
        assert_eq!(s.pending(), vec![]);
    }

    #[test]
    fn cancel_is_by_generation() {
        let mut s = Scheduler::new();
        let a = s.schedule(VTime(0), VTime(10), key(1));
        s.cancel(a);
        assert!(!s.is_pending(a));
        assert_eq!(s.next_time(), None);
        let b = s.schedule(VTime(0), VTime(20), key(2));
        assert_eq!(b.slot, a.slot);
        assert_ne!(b.r#gen, a.r#gen);
        s.cancel(a); // stale handle: the newer event in the slot stays
        assert_eq!(s.time_of(b), Some(VTime(20)));
        assert_eq!(s.pop_due(VTime(20)), Some(key(2)));
        let c = s.schedule(VTime(20), VTime(30), key(3));
        s.cancel(b); // already fired: no effect on c
        assert!(s.is_pending(c));
        s.cancel(EventHandle { slot: 99, r#gen: 0 }); // out of range: no effect
        assert_eq!(s.pending(), vec![(VTime(30), 2, key(3))]);
    }

    #[test]
    fn cancelled_future_events_do_not_accumulate() {
        let mut s = Scheduler::new();
        s.schedule(VTime(0), VTime(5), key(0));
        for i in 0..10_000u64 {
            let h = s.schedule(VTime(0), VTime(1_000_000 + i), key(1));
            s.cancel(h);
            assert!(s.heap.len() <= 2 * s.len() + COMPACT_SLACK + 1);
        }
        assert_eq!(s.slots.len(), 2);
        assert_eq!(s.pop_due(VTime(u64::MAX)), Some(key(0)));
        assert_eq!(s.pop_due(VTime(u64::MAX)), None);
    }

    #[test]
    fn compaction_cost_stays_amortized_after_a_peak() {
        const PEAK: u64 = 1_000;
        const ITERS: u64 = 20_000;
        let mut s = Scheduler::new();
        let hs: Vec<_> = (0..PEAK)
            .map(|i| s.schedule(VTime(0), VTime(1_000_000 + i), key(1)))
            .collect();
        for h in hs {
            s.cancel(h);
        }
        assert_eq!(s.slots.len(), PEAK as usize);
        assert!(s.is_empty());
        // A live event at the top, so only a rebuild shrinks the heap.
        s.schedule(VTime(0), VTime(5), key(0));
        let mut visits = 0u64;
        for i in 0..ITERS {
            let h = s.schedule(VTime(0), VTime(2_000_000 + i), key(1));
            let before = s.heap.len();
            s.cancel(h);
            assert_eq!(s.len(), 1);
            if s.heap.len() < before {
                visits += s.slots.len() as u64; // a rebuild scanned the slot table
            }
            assert!(s.heap.len() <= s.slots.len().max(2 * s.len() + COMPACT_SLACK) + 1);
        }
        assert_eq!(s.slots.len(), PEAK as usize);
        assert!(
            visits <= 4 * ITERS,
            "{visits} slot visits for {ITERS} cancels over {} slots",
            s.slots.len()
        );
        assert_eq!(s.pop_due(VTime(u64::MAX)), Some(key(0)));
        assert_eq!(s.pop_due(VTime(u64::MAX)), None);
    }

    #[test]
    fn postcard_round_trip_keeps_state_handles_and_order() {
        let mut s = Scheduler::new();
        let a = s.schedule(VTime(0), VTime(30), key(1));
        let b = s.schedule(VTime(0), VTime(10), key(2));
        let c = s.schedule(VTime(0), VTime(20), key(3));
        s.cancel(a); // frees a slot
        let d = s.schedule(VTime(0), VTime(20), key(4)); // reuses it, so generations differ
        assert_eq!(d.slot, a.slot);
        assert_eq!(s.pop_due(VTime(10)), Some(key(2)));
        assert!(!s.is_pending(b));

        let bytes = postcard::to_allocvec(&s).expect("encode");
        let mut r: Scheduler = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(r, s);
        assert_eq!(r.len(), 2);
        assert_eq!(r.next_time(), Some(VTime(20)));
        assert_eq!(r.time_of(c), Some(VTime(20)));
        assert_eq!(r.time_of(d), Some(VTime(20)));
        assert!(!r.is_pending(a));
        assert!(!r.is_pending(b));
        assert_eq!(r.pending(), s.pending());
        // c was scheduled before d at the same time.
        assert_eq!(r.pop_due(VTime(20)), Some(key(3)));
        assert_eq!(r.pop_due(VTime(20)), Some(key(4)));
        assert_eq!(r.pop_due(VTime(u64::MAX)), None);
        let e = r.schedule(VTime(20), VTime(40), key(5));
        assert_eq!(r.pending(), vec![(VTime(40), 4, key(5))]);
        assert!(r.is_pending(e));
    }

    #[test]
    fn postcard_rejects_an_inconsistent_section() {
        let mut s = Scheduler::new();
        s.schedule(VTime(0), VTime(9), key(1));
        let mut st = SchedState::from(s);
        st.free.push(0); // slot 0 is pending
        let bytes = postcard::to_allocvec(&st).expect("encode");
        assert!(postcard::from_bytes::<Scheduler>(&bytes).is_err());
    }

    #[test]
    fn restore_rejects_inconsistent_state() {
        let mut s = Scheduler::new();
        let h = s.schedule(VTime(0), VTime(9), key(1));
        s.schedule(VTime(0), VTime(9), key(2));
        s.cancel(h);
        let good = || SchedState::from(s.clone());
        assert_eq!(Scheduler::try_from(good()).unwrap(), s);
        let mut bad = good();
        bad.free.push(1); // slot 1 is pending
        assert!(Scheduler::try_from(bad).is_err());
        let mut bad = good();
        bad.free.push(0); // listed twice
        assert!(Scheduler::try_from(bad).is_err());
        let mut bad = good();
        bad.free.clear(); // empty slot 0 not free
        assert!(Scheduler::try_from(bad).is_err());
        let mut bad = good();
        bad.free.push(7); // out of range
        assert!(Scheduler::try_from(bad).is_err());
        let mut bad = good();
        bad.next_seq = 1; // pending seq 1 is not below next_seq
        assert!(Scheduler::try_from(bad).is_err());
    }

    /// A key carrying the event index, so a delivered key names which event fired.
    fn indexed_key(i: u32) -> EventKey {
        EventKey {
            owner: Owner::Periph(PeriphId((i >> 16) as u16)),
            tag: i as u16,
        }
    }

    struct Ev {
        ps: u64,
        handle: EventHandle,
        alive: bool,
        cancelled: bool,
    }

    #[test]
    fn property_random_insert_cancel_pop() {
        for seed in 1..=40u64 {
            let mut rng = XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut s = Scheduler::new();
            let mut evs: Vec<Ev> = Vec::new();
            let mut now = VTime(0);
            let mut last_fired: Option<(u64, u64)> = None;
            let mut fired = 0usize;

            for _ in 0..2_000 {
                match rng.below(10) {
                    0..=4 => {
                        let at = if rng.below(8) == 0 {
                            VTime(now.0.saturating_sub(rng.below(1_000)))
                        } else {
                            VTime(now.0 + rng.below(10_000))
                        };
                        let i = evs.len() as u32;
                        let h = s.schedule(now, at, indexed_key(i));
                        assert_eq!(s.time_of(h), Some(at.max(now)));
                        evs.push(Ev {
                            ps: at.max(now).0,
                            handle: h,
                            alive: true,
                            cancelled: false,
                        });
                    }
                    // Cancel a random event, live or not, to exercise stale handles.
                    5..=6 => {
                        if !evs.is_empty() {
                            let i = rng.below(evs.len() as u64) as usize;
                            let was = evs[i].alive;
                            s.cancel(evs[i].handle);
                            assert!(!s.is_pending(evs[i].handle));
                            if was {
                                evs[i].alive = false;
                                evs[i].cancelled = true;
                            }
                        }
                    }
                    7..=8 => {
                        while let Some(k) = s.pop_due(now) {
                            let i = (((k.owner_index() as u32) << 16) | k.tag as u32) as usize;
                            let ev = &mut evs[i];
                            assert!(ev.alive, "a fired or cancelled event fired again");
                            assert!(!ev.cancelled, "a cancelled event fired");
                            assert!(ev.ps <= now.0, "an event fired before its time");
                            let this = (ev.ps, i as u64);
                            if let Some(prev) = last_fired {
                                assert!(prev < this, "pop order is not (time, seq)");
                            }
                            last_fired = Some(this);
                            ev.alive = false;
                            fired += 1;
                        }
                    }
                    _ => now = VTime(now.0 + rng.below(3_000)),
                }

                let live: Vec<u64> = evs.iter().filter(|e| e.alive).map(|e| e.ps).collect();
                assert_eq!(s.len(), live.len());
                assert_eq!(s.is_empty(), live.is_empty());
                assert_eq!(s.next_time(), live.iter().min().copied().map(VTime));
                let listed = s.pending();
                assert_eq!(listed.len(), live.len());
                assert!(
                    listed
                        .windows(2)
                        .all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1))
                );
            }
            assert!(fired > 100, "seed {seed} fired only {fired} events");
        }
    }

    impl EventKey {
        /// Inverse of the owner half of `indexed_key`.
        fn owner_index(self) -> u16 {
            match self.owner {
                Owner::Periph(PeriphId(n)) => n,
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn postpone_moves_the_picked_events_and_keeps_their_handles() {
        let mut s = Scheduler::new();
        let now = VTime(0);
        let a = EventKey {
            owner: Owner::Periph(PeriphId(1)),
            tag: 0,
        };
        let b = EventKey {
            owner: Owner::Periph(PeriphId(2)),
            tag: 0,
        };
        let ha = s.schedule(now, VTime(100), a);
        let hb = s.schedule(now, VTime(150), b);
        let ha2 = s.schedule(now, VTime(100), a);
        assert_eq!(s.postpone(1_000, |k| k.owner == a.owner), 2);
        assert_eq!(s.time_of(ha), Some(VTime(1_100)));
        assert_eq!(s.time_of(ha2), Some(VTime(1_100)));
        assert_eq!(s.time_of(hb), Some(VTime(150)));
        assert_eq!(s.next_time(), Some(VTime(150)));
        assert_eq!(s.pop_due(VTime(2_000)), Some(b));
        s.cancel(ha);
        assert_eq!(
            s.pop_due(VTime(2_000)),
            Some(a),
            "the second `a` is still due"
        );
        assert_eq!(s.pop_due(VTime(2_000)), None);
        let hc = s.schedule(now, VTime(5), a);
        assert_eq!(s.postpone(u64::MAX, |_| true), 1);
        assert_eq!(s.time_of(hc), Some(VTime(u64::MAX)), "saturating");
    }
}
