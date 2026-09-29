//! Deterministic virtual time of `MockMachine`: releasing due outputs, stopping at a scripted
//! stop, a matcher hit or a limit, and journaling inputs with their reactions.

use pemu_core::input::InputEvent;
use pemu_core::time::VTime;
use pemu_machine::machine::{At, InputError};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::StopReason;

use super::{
    JournalRecord, MockInput, MockMachine, Output, ReleasedEvent, ReleasedFrame, SerialLine,
    StateValue,
};

/// `VTime` units per microsecond, taken from `VTime::from_us` so the mock follows `pemu-core`.
const UNITS_PER_US: u128 = VTime::from_us(1).0 as u128;

impl MockMachine {
    /// Instructions run from time 0 to `vt` at the constant scripted rate, floored.
    pub(super) fn insns_at(&self, vt: VTime) -> u64 {
        let n = u128::from(vt.0) * u128::from(self.insns_per_us) / UNITS_PER_US;
        u64::try_from(n).unwrap_or(u64::MAX)
    }

    /// The first instant at which `insns` more instructions have run, or `None` if the rate is 0.
    /// Rates above `UNITS_PER_US` per microsecond can overshoot.
    fn insns_deadline(&self, insns: u64) -> Option<VTime> {
        if insns == 0 {
            return Some(self.vt);
        }
        if self.insns_per_us == 0 {
            return None;
        }
        let target = u128::from(self.insns_at(self.vt)) + u128::from(insns);
        let units = (target * UNITS_PER_US).div_ceil(u128::from(self.insns_per_us));
        Some(VTime(u64::try_from(units).unwrap_or(u64::MAX)).max(self.vt))
    }

    /// Advance virtual time for one `run`. The limit is the earlier of `until` (a past instant
    /// means now) and the `max_insns` instant; on a tie `Until` wins. Due outputs are released in
    /// instant, then scheduling order; the first scripted stop or matcher hit returns at its
    /// output's instant and later outputs stay pending. With no limit, the mock reports `Deadlock`
    /// once nothing is left to release.
    pub(super) fn advance(&mut self, lim: &RunLimits) -> (StopReason, VTime) {
        let until = lim.until.map(|t| (t.max(self.vt), StopReason::Until));
        let insns = lim
            .max_insns
            .and_then(|n| self.insns_deadline(n))
            .map(|t| (t, StopReason::MaxInsns));
        let limit = [until, insns].into_iter().flatten().min_by_key(|(t, _)| *t);
        while let Some(&(at, _)) = self.pending.keys().next() {
            if limit.as_ref().is_some_and(|(t, _)| at > *t) {
                break;
            }
            let Some((_, out)) = self.pending.pop_first() else {
                break;
            };
            self.vt = self.vt.max(at);
            if let Some(reason) = self.release(out) {
                return (reason, self.vt);
            }
        }
        match limit {
            Some((t, reason)) => {
                self.vt = t;
                (reason, t)
            }
            None => (StopReason::Deadlock, self.vt),
        }
    }

    /// Release `out` now; a `Stop` output stops by itself, any other on the first armed matcher it
    /// triggers.
    fn release(&mut self, out: Output) -> Option<StopReason> {
        let hit = self
            .matchers
            .iter()
            .find(|(_, m)| m.matches(&out))
            .map(|(id, _)| StopReason::Matcher(*id));
        let vt = self.vt;
        match out {
            Output::Serial { chan, text } => {
                let lines = self.serial.entry(chan).or_default();
                let cursor = lines.len() as u64;
                lines.push(SerialLine { cursor, vt, text });
            }
            Output::Frame(frame) => {
                let generation = self.frames.len() as u64 + 1;
                self.frames.push(ReleasedFrame {
                    generation,
                    vt,
                    frame,
                });
            }
            Output::Event(name) => {
                let seq = self.events.len() as u64;
                self.events.push(ReleasedEvent { seq, vt, name });
            }
            Output::State { key, value } => {
                self.state.insert(key, StateValue { vt, value });
            }
            Output::Stop(reason) => return Some(reason),
        }
        hit
    }

    /// Journal `ev` at `at` and schedule the reactions it triggers. A past instant is refused with
    /// `InputError`, unjournaled and unnumbered, so host surfaces exercise that arm.
    pub(super) fn journal_input(
        &mut self,
        at: At,
        ev: &InputEvent,
    ) -> (MockInput, Result<u64, InputError>) {
        let input = MockInput::from(ev);
        let vt = match at {
            At::Now => self.vt,
            At::Vt(t) => t,
        };
        if vt < self.vt {
            return (input, Err(InputError {}));
        }
        let seq = self.journal.len() as u64;
        self.journal.push(JournalRecord {
            seq,
            vt,
            input: input.clone(),
        });
        let due: Vec<(VTime, Output)> = self
            .reactions
            .iter()
            .filter(|r| r.trigger.as_ref().is_none_or(|t| *t == input))
            .flat_map(|r| r.timeline.items().iter())
            .map(|(off, out)| (VTime(vt.0.saturating_add(off.0)), out.clone()))
            .collect();
        for (t, out) in due {
            self.schedule(t, out);
        }
        (input, Ok(seq))
    }
}
