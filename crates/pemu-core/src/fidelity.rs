//! Fidelity classes and the per-machine ledger of what a run modeled exactly, approximated, or
//! not at all. A strict run fails on a first touch of an unmodeled register. The ledger never
//! records a register value, so it cannot carry an eFuse identity word or a cardid byte.

use serde::{Deserialize, Deserializer, Serialize};

use crate::sched::PeriphId;
use crate::time::VTime;

/// Fidelity class of a modeled behavior, strongest evidence first.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Fidelity {
    A,
    B,
    C,
    #[default]
    U,
}

impl Fidelity {
    pub const ALL: [Fidelity; 4] = [Fidelity::A, Fidelity::B, Fidelity::C, Fidelity::U];

    /// `A` 3 down to `U` 0; the index into [`LedgerSummary::per_class`].
    pub const fn rank(self) -> u8 {
        match self {
            Fidelity::A => 3,
            Fidelity::B => 2,
            Fidelity::C => 1,
            Fidelity::U => 0,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Fidelity::A => "A",
            Fidelity::B => "B",
            Fidelity::C => "C",
            Fidelity::U => "U",
        }
    }

    pub const fn meaning(self) -> &'static str {
        match self {
            Fidelity::A => "matches the device",
            Fidelity::B => "matches the spec and oracles",
            Fidelity::C => "deliberate approximation",
            Fidelity::U => "unmodeled or unclaimed",
        }
    }

    pub const fn promotion_rule(self) -> &'static str {
        match self {
            Fidelity::A => "a test tied to a device capture id",
            Fidelity::B => "a spec citation plus a passing oracle comparison or an IDF-LL test",
            Fidelity::C => "a declared rationale",
            Fidelity::U => "the default; nothing to promote",
        }
    }

    /// `U` is the absence of a claim.
    pub const fn is_claimed(self) -> bool {
        !matches!(self, Fidelity::U)
    }

    /// The weaker of two claims, since a run may only report its weakest evidence.
    pub const fn merge(self, other: Fidelity) -> Fidelity {
        if other.rank() < self.rank() {
            other
        } else {
            self
        }
    }
}

/// UNVERIFIED: a design choice.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum TouchAccess {
    Read,
    Write,
}

/// First access to one register, without its value or PC; the machine keys it back to the
/// access by `(periph, off)`.
/// UNVERIFIED: a design choice.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FirstTouch {
    pub periph: PeriphId,
    /// Aligned down to 4.
    pub off: u32,
    pub access: TouchAccess,
    pub size: u8,
    pub now: VTime,
    /// Radio pages stay allowlisted as U, so a strict run never fails on them.
    pub allowlisted: bool,
}

impl FirstTouch {
    pub const fn subject(&self) -> LedgerSubject {
        LedgerSubject::Register {
            periph: self.periph,
            off: self.off,
        }
    }
}

/// What a class note is about. Hooks are not here: hook ids belong to crates above this one.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum LedgerSubject {
    /// Every register of the block, unless a `Register` note overrides it.
    Block(PeriphId),
    /// One register, by its offset aligned down to 4.
    Register { periph: PeriphId, off: u32 },
}

/// A class claimed for one subject; citations and test ids stay in the spec tables.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ClassNote {
    pub subject: LedgerSubject,
    pub class: Fidelity,
}

/// What a run reports about its own fidelity (`fidelity.json`, receipts).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct LedgerSummary {
    pub touches: u64,
    /// Indexed by [`Fidelity::rank`] (`per_class[0]` is `U`).
    pub per_class: [u64; 4],
    /// Unallowlisted touches of a `U` register; each stops a `Strictness::Strict` run.
    pub unmodeled: u64,
    /// The weakest class of any unallowlisted touch; `A` when nothing was touched.
    pub worst: Fidelity,
}

/// Per-machine ledger reached through `Cx::ledger`: one first touch per register in report order,
/// notes sorted by subject. Deserializing replays entries, so a bad section cannot break either.
/// UNVERIFIED: a design choice.
#[derive(Default, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FidelityLedger {
    #[serde(deserialize_with = "replay_touches")]
    first_touches: Vec<FirstTouch>,
    #[serde(deserialize_with = "replay_notes")]
    notes: Vec<ClassNote>,
}

fn replay_touches<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<FirstTouch>, D::Error> {
    let mut l = FidelityLedger::default();
    for t in Vec::<FirstTouch>::deserialize(d)? {
        l.first_touch(t);
    }
    Ok(l.first_touches)
}

fn replay_notes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ClassNote>, D::Error> {
    let mut l = FidelityLedger::default();
    for n in Vec::<ClassNote>::deserialize(d)? {
        l.note(n.subject, n.class);
    }
    Ok(l.notes)
}

impl FidelityLedger {
    /// Records a first touch; a repeat of a register keeps one entry with the earlier time.
    pub fn first_touch(&mut self, t: FirstTouch) {
        match self
            .first_touches
            .iter_mut()
            .find(|e| e.subject() == t.subject())
        {
            Some(seen) => {
                if t.now < seen.now {
                    let allowlisted = seen.allowlisted && t.allowlisted;
                    *seen = t;
                    seen.allowlisted = allowlisted;
                } else {
                    seen.allowlisted = seen.allowlisted && t.allowlisted;
                }
            }
            None => self.first_touches.push(t),
        }
    }

    pub fn first_touches(&self) -> &[FirstTouch] {
        &self.first_touches
    }

    pub fn is_touched(&self, periph: PeriphId, off: u32) -> bool {
        let subject = LedgerSubject::Register { periph, off };
        self.first_touches.iter().any(|t| t.subject() == subject)
    }

    /// Cursor past the last touch, for [`FidelityLedger::delta`].
    pub fn cursor(&self) -> u64 {
        self.first_touches.len() as u64
    }

    /// Touches since `cursor`, for `Machine::receipt`; a cursor past the end reads empty.
    pub fn delta(&self, cursor: u64) -> &[FirstTouch] {
        let from = (cursor as usize).min(self.first_touches.len());
        &self.first_touches[from..]
    }

    /// Records a claim; two claims for one subject merge to the weaker one.
    pub fn note(&mut self, subject: LedgerSubject, class: Fidelity) {
        match self.notes.binary_search_by(|n| n.subject.cmp(&subject)) {
            Ok(i) => self.notes[i].class = self.notes[i].class.merge(class),
            Err(i) => self.notes.insert(i, ClassNote { subject, class }),
        }
    }

    pub fn notes(&self) -> &[ClassNote] {
        &self.notes
    }

    /// The subject's own note, else its block's, else `U`.
    pub fn class_of(&self, subject: LedgerSubject) -> Fidelity {
        if let Ok(i) = self.notes.binary_search_by(|n| n.subject.cmp(&subject)) {
            return self.notes[i].class;
        }
        if let LedgerSubject::Register { periph, .. } = subject
            && let Ok(i) = self
                .notes
                .binary_search_by(|n| n.subject.cmp(&LedgerSubject::Block(periph)))
        {
            return self.notes[i].class;
        }
        Fidelity::U
    }

    /// Unallowlisted touches of unmodeled registers, in report order.
    pub fn unmodeled(&self) -> impl Iterator<Item = &FirstTouch> {
        self.first_touches
            .iter()
            .filter(|t| !t.allowlisted && !self.class_of(t.subject()).is_claimed())
    }

    pub fn summary(&self) -> LedgerSummary {
        let mut s = LedgerSummary {
            touches: self.first_touches.len() as u64,
            worst: Fidelity::A,
            ..LedgerSummary::default()
        };
        for t in &self.first_touches {
            let class = self.class_of(t.subject());
            s.per_class[class.rank() as usize] += 1;
            if t.allowlisted {
                continue;
            }
            s.worst = s.worst.merge(class);
            if !class.is_claimed() {
                s.unmodeled += 1;
            }
        }
        s
    }

    /// Folds in a fork's or restore's ledger; `allowlisted` is the conjunction, so a strict
    /// violation survives. Not commutative in entry order: merge the later ledger into the earlier.
    pub fn merge(&mut self, other: &FidelityLedger) {
        for t in &other.first_touches {
            self.first_touch(*t);
        }
        for n in &other.notes {
            self.note(n.subject, n.class);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: PeriphId = PeriphId(3);
    const Q: PeriphId = PeriphId(4);

    fn touch(off: u32, access: TouchAccess) -> FirstTouch {
        FirstTouch {
            periph: P,
            off,
            access,
            size: 4,
            now: VTime(7),
            allowlisted: false,
        }
    }

    fn at(off: u32, now: u64, allowlisted: bool) -> FirstTouch {
        FirstTouch {
            periph: P,
            off,
            access: TouchAccess::Read,
            size: 4,
            now: VTime(now),
            allowlisted,
        }
    }

    fn reg(off: u32) -> LedgerSubject {
        LedgerSubject::Register { periph: P, off }
    }

    #[test]
    fn ledger_starts_empty() {
        assert!(FidelityLedger::default().first_touches().is_empty());
        assert_eq!(
            FidelityLedger::default().summary(),
            LedgerSummary {
                worst: Fidelity::A,
                ..LedgerSummary::default()
            }
        );
    }

    #[test]
    fn ledger_keeps_report_order() {
        let mut l = FidelityLedger::default();
        l.first_touch(touch(8, TouchAccess::Write));
        l.first_touch(touch(0, TouchAccess::Read));
        assert_eq!(
            l.first_touches(),
            &[touch(8, TouchAccess::Write), touch(0, TouchAccess::Read)]
        );
    }

    #[test]
    fn a_register_is_recorded_once_and_keeps_the_earliest_time() {
        let mut l = FidelityLedger::default();
        l.first_touch(at(8, 20, false));
        l.first_touch(at(8, 30, false));
        assert_eq!(l.first_touches(), &[at(8, 20, false)]);
        l.first_touch(at(8, 5, false));
        assert_eq!(l.first_touches(), &[at(8, 5, false)]);
        assert!(l.is_touched(P, 8));
        assert!(!l.is_touched(P, 4));
        assert!(!l.is_touched(Q, 8));
    }

    #[test]
    fn class_notes_merge_to_the_weaker_claim() {
        assert_eq!(Fidelity::A.merge(Fidelity::C), Fidelity::C);
        assert_eq!(Fidelity::C.merge(Fidelity::A), Fidelity::C);
        assert_eq!(Fidelity::U.merge(Fidelity::A), Fidelity::U);
        for a in Fidelity::ALL {
            assert_eq!(a.merge(a), a);
            for b in Fidelity::ALL {
                assert_eq!(a.merge(b), b.merge(a));
                for c in Fidelity::ALL {
                    assert_eq!(a.merge(b).merge(c), a.merge(b.merge(c)));
                }
            }
        }
        let mut l = FidelityLedger::default();
        l.note(reg(0), Fidelity::A);
        l.note(reg(0), Fidelity::B);
        l.note(reg(0), Fidelity::A);
        assert_eq!(l.class_of(reg(0)), Fidelity::B);
    }

    #[test]
    fn a_register_note_overrides_its_block_and_the_default_is_unmodeled() {
        let mut l = FidelityLedger::default();
        assert_eq!(l.class_of(reg(4)), Fidelity::U);
        l.note(LedgerSubject::Block(P), Fidelity::B);
        assert_eq!(l.class_of(reg(4)), Fidelity::B);
        assert_eq!(l.class_of(LedgerSubject::Block(P)), Fidelity::B);
        l.note(reg(4), Fidelity::C);
        assert_eq!(l.class_of(reg(4)), Fidelity::C);
        assert_eq!(l.class_of(reg(8)), Fidelity::B);
        assert_eq!(
            l.class_of(LedgerSubject::Register { periph: Q, off: 4 }),
            Fidelity::U
        );
    }

    #[test]
    fn notes_are_kept_sorted_whatever_order_they_arrive_in() {
        let mut a = FidelityLedger::default();
        let mut b = FidelityLedger::default();
        for s in [
            reg(8),
            LedgerSubject::Block(Q),
            reg(0),
            LedgerSubject::Block(P),
        ] {
            a.note(s, Fidelity::B);
        }
        for s in [
            LedgerSubject::Block(P),
            reg(0),
            LedgerSubject::Block(Q),
            reg(8),
        ] {
            b.note(s, Fidelity::B);
        }
        assert_eq!(a.notes(), b.notes());
        assert!(a.notes().windows(2).all(|w| w[0].subject < w[1].subject));
    }

    #[test]
    fn the_summary_counts_per_class_and_reports_the_weakest_touch() {
        let mut l = FidelityLedger::default();
        l.note(LedgerSubject::Block(P), Fidelity::B);
        l.note(reg(8), Fidelity::C);
        l.first_touch(at(0, 1, false)); // B, from the block note
        l.first_touch(at(8, 2, false)); // C, from its own note
        l.first_touch(FirstTouch {
            periph: Q,
            ..at(0, 3, false)
        }); // U: an unclassified block, and no allowlist
        l.first_touch(FirstTouch {
            periph: Q,
            ..at(4, 4, true)
        }); // U but allowlisted: a radio page
        let s = l.summary();
        assert_eq!(s.touches, 4);
        assert_eq!(s.per_class, [2, 1, 1, 0]);
        assert_eq!(s.unmodeled, 1);
        assert_eq!(s.worst, Fidelity::U);
        let unmodeled: Vec<_> = l.unmodeled().map(|t| (t.periph, t.off)).collect();
        assert_eq!(unmodeled, vec![(Q, 0)]);
    }

    #[test]
    fn the_summary_ignores_allowlisted_pages_when_it_picks_the_worst_class() {
        let mut l = FidelityLedger::default();
        l.note(LedgerSubject::Block(P), Fidelity::B);
        l.first_touch(at(0, 1, false));
        l.first_touch(FirstTouch {
            periph: Q,
            ..at(0, 2, true)
        });
        let s = l.summary();
        assert_eq!(s.worst, Fidelity::B);
        assert_eq!(s.unmodeled, 0);
        assert_eq!(s.per_class, [1, 0, 1, 0]);
    }

    #[test]
    fn the_cursor_reports_what_a_receipt_has_not_seen() {
        let mut l = FidelityLedger::default();
        l.first_touch(at(0, 1, false));
        let c = l.cursor();
        assert_eq!(l.delta(0), &[at(0, 1, false)]);
        assert!(l.delta(c).is_empty());
        l.first_touch(at(4, 2, false));
        l.first_touch(at(0, 3, false)); // a repeat adds nothing
        assert_eq!(l.delta(c), &[at(4, 2, false)]);
        assert_eq!(l.cursor(), 2);
        assert!(l.delta(99).is_empty());
    }

    #[test]
    fn merge_appends_new_registers_and_keeps_the_first_touch() {
        let mut a = FidelityLedger::default();
        a.first_touch(at(0, 10, false));
        a.first_touch(at(4, 20, false));
        a.note(LedgerSubject::Block(P), Fidelity::A);
        let mut b = FidelityLedger::default();
        b.first_touch(at(4, 5, false)); // earlier than a's
        b.first_touch(at(8, 30, false)); // new
        b.note(LedgerSubject::Block(P), Fidelity::C);
        b.note(reg(8), Fidelity::B);

        a.merge(&b);
        assert_eq!(
            a.first_touches(),
            &[at(0, 10, false), at(4, 5, false), at(8, 30, false)]
        );
        assert_eq!(a.class_of(LedgerSubject::Block(P)), Fidelity::C);
        assert_eq!(a.class_of(reg(8)), Fidelity::B);
    }

    /// Encodes like [`FidelityLedger`], standing in for a hostile or stale section.
    #[derive(Serialize)]
    struct RawLedger {
        first_touches: Vec<FirstTouch>,
        notes: Vec<ClassNote>,
    }

    #[test]
    fn a_decoded_ledger_rebuilds_its_invariants() {
        let mut l = FidelityLedger::default();
        l.note(LedgerSubject::Block(P), Fidelity::B);
        l.note(reg(8), Fidelity::C);
        l.first_touch(at(0, 1, false));
        l.first_touch(at(8, 2, false));
        let bytes = postcard::to_allocvec(&l).expect("encode");
        let back: FidelityLedger = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back, l);

        // The same entries, encoded out of order and with a duplicate register.
        let raw = RawLedger {
            first_touches: vec![at(8, 2, false), at(0, 1, false), at(8, 99, true)],
            notes: vec![
                ClassNote {
                    subject: reg(8),
                    class: Fidelity::C,
                },
                ClassNote {
                    subject: LedgerSubject::Block(P),
                    class: Fidelity::B,
                },
                ClassNote {
                    subject: reg(8),
                    class: Fidelity::A,
                },
            ],
        };
        let bytes = postcard::to_allocvec(&raw).expect("encode");
        let back: FidelityLedger = postcard::from_bytes(&bytes).expect("decode");
        assert!(back.notes().windows(2).all(|w| w[0].subject < w[1].subject));
        assert_eq!(back.class_of(reg(8)), Fidelity::C, "the weaker claim wins");
        assert_eq!(back.class_of(reg(0)), Fidelity::B, "the block note applies");
        assert_eq!(
            back.first_touches(),
            &[at(8, 2, false), at(0, 1, false)],
            "one entry per register, allowlisted is the conjunction"
        );
        assert_eq!(back.summary().touches, 2);
    }

    #[test]
    fn merge_is_idempotent_and_cannot_hide_a_strict_violation() {
        let mut a = FidelityLedger::default();
        a.first_touch(at(0, 10, false));
        a.note(reg(0), Fidelity::B);
        let before = a.clone();
        a.merge(&before);
        assert_eq!(a, before);

        // One reporter says the page is allowlisted, the other does not: the strict reading wins.
        let mut strict = FidelityLedger::default();
        strict.first_touch(at(4, 10, false));
        let mut lax = FidelityLedger::default();
        lax.first_touch(at(4, 10, true));
        let mut merged = lax.clone();
        merged.merge(&strict);
        assert_eq!(merged.first_touches(), &[at(4, 10, false)]);
        assert_eq!(merged.summary().unmodeled, 1);
        let mut other_way = strict.clone();
        other_way.merge(&lax);
        assert_eq!(other_way.first_touches(), &[at(4, 10, false)]);
    }
}
