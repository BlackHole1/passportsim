//! Poll fast-forward by state equality, and the reads the hang detector watches.
//!
//! [`PollTracker`] lives in the MMIO read slow path, which sees pc, address, value and
//! instruction count but not the hart, so it only counts: consecutive reads by one pc of one
//! address returning one value, one period apart, with nothing in between that could change the
//! next iteration. At [`CONFIRM_REPEATS`] the read answers `OkStop`, ending the slice without
//! changing guest state.
//!
//! [`Machine::poll_step`], in the run loop, copies the architectural state at that read and the
//! next ([`HartSnap`]) and compares the copies field by field (no hash, so no collision case).
//! Equal copies one period apart mean the loop repeats exactly until something changes.
//!
//! [`PollTracker::invalidate`] ends a chain on an MMIO write, a custom CSR access, any `OkStop`,
//! a dispatched event, a journaled input, applied wiring, a taken interrupt, a reset, a hook exit,
//! a watch hit or idle time. `Hart::stores` is part of the compared state, and the INTC epoch and
//! the clock's stall total are compared at every read.

use pemu_core::snap::{
    Section, SnapError, SnapReader, SnapSection, SnapValue, snap_struct, value_from_section,
    value_section,
};
use pemu_core::time::VTime;
use pemu_rv32::csr::TRIGGERS;
use pemu_rv32::exec::Hart;
use pemu_soc_c3::periph::{Cx, DeviceVisitor, Peripheral, Stability, lookup};

use crate::hang::{HangCfg, StuckKind};
use crate::machine::Machine;
use crate::run::RunLimits;
use crate::stops::StopReason;

pub const CONFIRM_REPEATS: u64 = 64;

/// Largest backoff exponent: a loop that keeps failing its confirmation is tried again after at
/// most `CONFIRM_REPEATS << MAX_BACKOFF` repeats.
pub const MAX_BACKOFF: u32 = 10;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Backoff {
    pc: u32,
    addr: u32,
    shift: u32,
}

/// Distance within which two reading pcs count as one loop for the fallback row.
const FALLBACK_WINDOW: u32 = 64;

/// The longest gap between two reads of a hang candidate that still counts as one busy-wait.
/// Two reads further apart are separate visits to the code (a poll function called twice), so
/// the candidate starts over. UNVERIFIED bound: 10 ms is a hundred FreeRTOS ticks at the IDF
/// default and far below `stuck_ms`, while interrupts and events cut in for well under 1 ms.
const MAX_READ_GAP_PS: u64 = 10_000_000_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Phase {
    Counting,
    Snapped,
    /// State-equal one period apart. `again` asks for another fast-forward at the next read,
    /// because the last one stopped at a bound that is not an event.
    Confirmed {
        again: bool,
    },
    Failed,
    /// Confirmed before a run boundary or a restore: the state is copied and compared again before
    /// anything is fast-forwarded.
    Reverify,
}

/// Consecutive identical reads of one register. A loop with several distinct reads never forms a
/// chain, because the canonical trace folds only one; widening this needs the trace sink widened
/// too.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Chain {
    pc: u32,
    addr: u32,
    val: u32,
    size: u8,
    insns: u64,
    period: u64,
    extra: u64,
    /// Class-cost cycles between two reads beyond one per instruction, which must repeat with the
    /// period; 0 until the second read and under `fast`.
    extra_period: u64,
    repeats: u64,
    first_vt: VTime,
    phase: Phase,
    judged: bool,
}

/// A hang candidate: one pc (or window) reading one register and getting one value since
/// `since`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Unchanged {
    pc: u32,
    addr: u32,
    val: u32,
    since: VTime,
    last: VTime,
    /// Not judged before this instant: the register's block had a completion event pending at the
    /// deadline, so the wait is modeled as finite.
    defer: VTime,
    /// The register holds until an input (`Stability::UntilInput`): never a hang.
    exempt: bool,
}

impl Unchanged {
    fn deadline(&self, stuck: u64) -> VTime {
        VTime(self.since.0.saturating_add(stuck)).max(self.defer)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct HangHit {
    pub(crate) kind: StuckKind,
    pub(crate) pc: u32,
    pub(crate) addr: u32,
    pub(crate) val: u32,
    pub(crate) since: VTime,
    pub(crate) at: VTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HartSnap {
    x: [u32; 32],
    pc: u32,
    stores: u64,
    csr: [u32; 11],
    pmpcfg: [u8; 16],
    pmpaddr: [u32; 16],
    tdata1: [u32; TRIGGERS],
    tdata2: [u32; TRIGGERS],
}

impl HartSnap {
    /// Copies `x[1..32]`, the pc, `Hart::stores` and every stored CSR. Clock-derived CSRs have no
    /// store and are covered by the invalidation instead.
    pub(crate) fn of(hart: &Hart) -> HartSnap {
        let c = &hart.csr;
        let mut x = hart.x;
        x[0] = 0;
        HartSnap {
            x,
            pc: hart.pc,
            stores: hart.stores,
            csr: [
                c.mstatus, c.mtvec, c.mepc, c.mcause, c.mtval, c.mscratch, c.tselect, c.tcontrol,
                c.mpcer, c.mpcmr, c.csr000,
            ],
            pmpcfg: c.pmpcfg,
            pmpaddr: c.pmpaddr,
            tdata1: c.tdata1,
            tdata2: c.tdata2,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PollTracker {
    chain: Option<Chain>,
    dirty: bool,
    irq_epoch: u64,
    /// `Clock::stall_ps` at the last read: a cache-fill stall moves time without instructions, so a
    /// period measured across one is not the loop's.
    stall_ps: u64,
    pub(crate) want_check: bool,
    snap: Option<HartSnap>,
    pub(crate) hang: HangCfg,
    unchanged: Option<Unchanged>,
    fallback: Option<Unchanged>,
    pub(crate) hit: Option<HangHit>,
    backoff: Option<Backoff>,
}

impl PollTracker {
    pub(crate) fn with_hang(hang: HangCfg) -> PollTracker {
        PollTracker {
            hang,
            ..PollTracker::default()
        }
    }

    pub(crate) fn threshold(&self, pc: u32, addr: u32) -> u64 {
        match self.backoff {
            Some(b) if (b.pc, b.addr) == (pc, addr) => CONFIRM_REPEATS << b.shift,
            _ => CONFIRM_REPEATS,
        }
    }

    pub(crate) fn failed(&mut self, pc: u32, addr: u32) {
        let shift = match self.backoff {
            Some(b) if (b.pc, b.addr) == (pc, addr) => (b.shift + 1).min(MAX_BACKOFF),
            _ => 1,
        };
        self.backoff = Some(Backoff { pc, addr, shift });
    }

    pub(crate) fn confirmed(&mut self, pc: u32, addr: u32) {
        if self.backoff.is_some_and(|b| (b.pc, b.addr) == (pc, addr)) {
            self.backoff = None;
        }
    }

    #[inline]
    pub(crate) fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// A `run` call starts, or the tracker was restored. A confirmation taken before vouches for
    /// nothing the caller may have changed in between, so a confirmed chain verifies again before
    /// anything is skipped. Everything else stays, and the first hang row is judged once per chain,
    /// so where a hang is reported does not depend on where a caller split the run.
    pub(crate) fn new_run(&mut self) {
        self.want_check = false;
        if let Some(c) = self.chain.as_mut()
            && matches!(c.phase, Phase::Confirmed { .. })
        {
            c.phase = Phase::Reverify;
        }
    }

    /// The hart idled: the candidates' loop was left (a task polling between delays is not
    /// busy-waiting).
    pub(crate) fn idled(&mut self) {
        self.dirty = true;
        self.unchanged = None;
        self.fallback = None;
    }

    /// Records one register read. Returns whether the read must end the slice, for
    /// [`Machine::poll_step`] or a hang report.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_read(
        &mut self,
        pc: u32,
        addr: u32,
        val: u32,
        size: u8,
        insns: u64,
        extra: u64,
        now: VTime,
        irq_epoch: u64,
        stall_ps: u64,
    ) -> bool {
        let continues = !self.dirty
            && irq_epoch == self.irq_epoch
            && stall_ps == self.stall_ps
            && self.chain.is_some_and(|c| {
                c.pc == pc
                    && c.addr == addr
                    && c.val == val
                    && c.size == size
                    && insns > c.insns
                    && extra >= c.extra
                    && (c.period == 0
                        || (insns - c.insns == c.period && extra - c.extra == c.extra_period))
            });
        self.dirty = false;
        self.irq_epoch = irq_epoch;
        self.stall_ps = stall_ps;
        let mut stop = false;
        if continues {
            let threshold = self
                .chain
                .as_ref()
                .map_or(CONFIRM_REPEATS, |c| self.threshold(c.pc, c.addr));
            let c = self.chain.as_mut().expect("checked above");
            if c.period == 0 {
                c.period = insns - c.insns;
                c.extra_period = extra - c.extra;
            }
            c.insns = insns;
            c.extra = extra;
            c.repeats += 1;
            match c.phase {
                Phase::Counting if c.repeats == threshold => stop = true,
                Phase::Snapped | Phase::Reverify | Phase::Confirmed { again: true } => stop = true,
                _ => {}
            }
        } else {
            self.chain = Some(Chain {
                pc,
                addr,
                val,
                size,
                insns,
                period: 0,
                extra,
                extra_period: 0,
                repeats: 1,
                first_vt: now,
                phase: Phase::Counting,
                judged: false,
            });
            self.snap = None;
        }
        self.want_check = stop;
        if self.hang.enabled && self.hit.is_none() && self.hang_at_read(pc, addr, val, now) {
            stop = true;
        }
        stop
    }

    /// The `Unchanged` and `Fallback` hang rows at one read.
    fn hang_at_read(&mut self, pc: u32, addr: u32, val: u32, now: VTime) -> bool {
        let stuck = self.hang.stuck_ps();
        // A read of the loop a confirmation vouched for. Events in between start new chains but
        // not a new loop.
        let mut confirmed = false;
        if let Some(u) = self.unchanged.as_mut()
            && u.pc == pc
        {
            if u.addr != addr || u.val != val || now.0.saturating_sub(u.last.0) > MAX_READ_GAP_PS {
                self.unchanged = None;
            } else if !u.exempt && now >= u.deadline(stuck) {
                self.hit = Some(HangHit {
                    kind: StuckKind::Unchanged,
                    pc,
                    addr,
                    val,
                    since: u.since,
                    at: now,
                });
                return true;
            } else {
                u.last = now;
                confirmed = true;
            }
        }
        if confirmed {
            return false;
        }
        match self.fallback.as_mut() {
            Some(f) if f.pc.abs_diff(pc) < FALLBACK_WINDOW => {
                if f.addr != addr
                    || f.val != val
                    || now.0.saturating_sub(f.last.0) > MAX_READ_GAP_PS
                {
                    *f = Unchanged {
                        pc,
                        addr,
                        val,
                        since: now,
                        last: now,
                        defer: VTime(0),
                        exempt: false,
                    };
                } else if !f.exempt && now >= f.deadline(stuck) {
                    self.hit = Some(HangHit {
                        kind: StuckKind::Fallback,
                        pc: f.pc,
                        addr,
                        val,
                        since: f.since,
                        at: now,
                    });
                    return true;
                } else {
                    f.last = now;
                }
            }
            Some(_) => {}
            None => {
                self.fallback = Some(Unchanged {
                    pc,
                    addr,
                    val,
                    since: now,
                    last: now,
                    defer: VTime(0),
                    exempt: false,
                })
            }
        }
        false
    }

    /// The earliest hang deadline among the candidates, which fast-forward must not skip past.
    fn hang_deadline(&self) -> Option<VTime> {
        if !self.hang.enabled {
            return None;
        }
        let stuck = self.hang.stuck_ps();
        [self.unchanged, self.fallback]
            .into_iter()
            .flatten()
            .filter(|u| !u.exempt)
            .map(|u| u.deadline(stuck))
            .min()
    }

    fn credit(&mut self, k: u64) {
        if let Some(c) = self.chain.as_mut() {
            c.insns += k * c.period;
            c.extra += k * c.extra_period;
            c.repeats += k;
        }
    }

    fn exempt(&mut self, hit: &HangHit) {
        for slot in [&mut self.unchanged, &mut self.fallback] {
            if let Some(u) = slot.as_mut()
                && u.addr == hit.addr
                && u.val == hit.val
            {
                u.exempt = true;
            }
        }
    }
}

struct Stable<'c, 'a> {
    off: u32,
    cx: &'c Cx<'a>,
    out: Option<(Stability, pemu_core::fidelity::Fidelity)>,
}

impl DeviceVisitor for Stable<'_, '_> {
    fn visit<P: Peripheral>(&mut self, dev: &mut P) {
        self.out = Some((dev.stable_until(self.off, self.cx), dev.fidelity(self.off)));
    }
}

impl Machine {
    /// How long the register at `addr` keeps its value, and its fidelity class; `None` for an
    /// unclaimed address. A disabled model is store-only: `Never` and class U.
    pub(crate) fn register_stability(
        &mut self,
        addr: u32,
    ) -> Option<(Stability, pemu_core::fidelity::Fidelity)> {
        let (id, off) = lookup(addr)?;
        if self.model_disabled(id) {
            return Some((Stability::Never, pemu_core::fidelity::Fidelity::U));
        }
        self.with_bus(|bus, _| {
            let mut v = Stable {
                off,
                cx: &bus.inner.cx.periph,
                out: None,
            };
            bus.inner.soc.devices.visit(id, &mut v);
            v.out
        })
    }

    /// The run loop's half of the tracker, right after the slice a read ended: copies or compares
    /// the state, reports the `Unchangeable` row, and fast-forwards a confirmed loop.
    pub(crate) fn poll_step(&mut self, lim: &RunLimits, insns_at_start: u64) -> Option<StopReason> {
        if let Some(hit) = self.poll.hit.take() {
            return self.hang_report(hit);
        }
        if !std::mem::take(&mut self.poll.want_check) {
            return None;
        }
        let chain = self.poll.chain?;
        match chain.phase {
            Phase::Counting | Phase::Reverify => {
                self.poll.snap = Some(HartSnap::of(&self.hart));
                self.set_phase(Phase::Snapped);
                None
            }
            Phase::Snapped => {
                let now = HartSnap::of(&self.hart);
                if self.poll.snap.take().as_ref() != Some(&now) {
                    self.set_phase(Phase::Failed);
                    self.poll.failed(chain.pc, chain.addr);
                    return None;
                }
                self.set_phase(Phase::Confirmed { again: false });
                self.poll.confirmed(chain.pc, chain.addr);
                self.on_confirmed(chain, lim, insns_at_start)
            }
            Phase::Confirmed { .. } => {
                self.set_phase(Phase::Confirmed { again: false });
                self.fast_forward(chain, lim, insns_at_start);
                None
            }
            Phase::Failed => None,
        }
    }

    fn set_phase(&mut self, phase: Phase) {
        if let Some(c) = self.poll.chain.as_mut() {
            c.phase = phase;
        }
    }

    fn on_confirmed(
        &mut self,
        chain: Chain,
        lim: &RunLimits,
        insns_at_start: u64,
    ) -> Option<StopReason> {
        let stability = self.register_stability(chain.addr);
        let exempt = matches!(stability, Some((Stability::UntilInput, _)));
        let first_judgement = !chain.judged;
        if let Some(c) = self.poll.chain.as_mut() {
            c.judged = true;
        }
        if first_judgement
            && self.poll.hang.enabled
            && !exempt
            && self.unchangeable(chain.addr, &stability)
        {
            let now = self.now();
            return self.hang_report(HangHit {
                kind: StuckKind::Unchangeable,
                pc: chain.pc,
                addr: chain.addr,
                val: chain.val,
                since: chain.first_vt,
                at: now,
            });
        }
        // The loop is the tracker's now; the fallback row no longer describes it.
        self.poll.fallback = None;
        let keep = self
            .poll
            .unchanged
            .is_some_and(|u| u.pc == chain.pc && u.addr == chain.addr && u.val == chain.val);
        if !keep {
            self.poll.unchanged = Some(Unchanged {
                pc: chain.pc,
                addr: chain.addr,
                val: chain.val,
                since: chain.first_vt,
                last: self.clock.now(chain.insns + chain.extra),
                defer: VTime(0),
                exempt,
            });
        }
        self.fast_forward(chain, lim, insns_at_start);
        None
    }

    /// Whether nothing can ever change the register at `addr`: its block is disabled or store-only,
    /// or the register is class U with `Stability::Never` and its block has no pending event.
    fn unchangeable(
        &self,
        addr: u32,
        stability: &Option<(Stability, pemu_core::fidelity::Fidelity)>,
    ) -> bool {
        // An unclaimed address reads 0 and is the unmodeled-access policy's concern; a loop on it
        // is judged by the `Unchanged` row like any other register.
        let Some((id, _)) = lookup(addr) else {
            return false;
        };
        if self.model_disabled(id) {
            return true;
        }
        match stability {
            Some((Stability::Never, pemu_core::fidelity::Fidelity::U)) => !self
                .sched
                .pending()
                .iter()
                .any(|(_, _, key)| key.owner == pemu_core::sched::Owner::Periph(id)),
            _ => false,
        }
    }

    /// Credits the `k` whole iterations a confirmed chain can skip: `k x p` instructions with the
    /// clock following, and the skipped reads to the open `PollRun` trace record, nothing else,
    /// because the state already repeats. `k` stops short of the next event, journal input, limit,
    /// the instant the value may change and the hang deadline, so the run continues from an
    /// instruction the unforwarded run also reaches.
    fn fast_forward(&mut self, chain: Chain, lim: &RunLimits, insns_at_start: u64) {
        if !self.poll_ff || chain.period == 0 {
            return;
        }
        let p = chain.period;
        let now = self.now();
        let stable = match self.register_stability(chain.addr) {
            None | Some((Stability::Never, _)) => return,
            Some((Stability::Until(t), _)) => Some(t),
            Some((Stability::UntilNextEvent | Stability::UntilInput, _)) => None,
        };
        let hang = self.poll.hang_deadline();
        let bound = [self.next_wake(lim), stable, hang]
            .into_iter()
            .flatten()
            .min();
        let mut k = match bound {
            Some(t) if t <= now => return,
            // One iteration moves the clock position by `p` instructions plus their class-cost
            // cycles.
            Some(t) => self.clock.insns_until(self.hart.pos(), t) / (p + chain.extra_period),
            None if lim.max_insns.is_some() => u64::MAX,
            None => return,
        };
        if let Some(max) = lim.max_insns {
            k = k.min(max.saturating_sub(self.budget_spent(insns_at_start)) / p);
        }
        if k == 0 {
            return;
        }
        let skipped = k * p;
        self.hart.insns += skipped;
        self.hart.extra += k * chain.extra_period;
        self.ff_insns += skipped;
        self.trace.poll_repeat(k);
        self.tap.credit_repeats(k);
        self.poll.credit(k);
        if let Some(c) = self.poll.chain {
            let at = self.clock.now(c.insns + c.extra);
            if let Some(u) = self.poll.unchanged.as_mut()
                && (u.pc, u.addr, u.val) == (c.pc, c.addr, c.val)
            {
                u.last = at;
            }
        }
        // A bound that is not an event is not followed by an invalidation, so the next read tries
        // again.
        let at_event = bound == self.next_wake(lim);
        if !at_event {
            self.set_phase(Phase::Confirmed { again: true });
        }
    }

    fn hang_report(&mut self, hit: HangHit) -> Option<StopReason> {
        if hit.kind != StuckKind::Unchangeable
            && matches!(
                self.register_stability(hit.addr),
                Some((Stability::UntilInput, _))
            )
        {
            self.poll.exempt(&hit);
            return None;
        }
        // A long wait the model will end is not a hang: while the register's block has a
        // completion event pending, the row is judged again only once that event is due.
        if hit.kind == StuckKind::Unchanged
            && let Some((id, _)) = lookup(hit.addr)
            && let Some(due) = self
                .sched
                .pending()
                .iter()
                .filter(|(_, _, key)| key.owner == pemu_core::sched::Owner::Periph(id))
                .map(|(t, _, _)| *t)
                .min()
        {
            if let Some(u) = self.poll.unchanged.as_mut()
                && (u.addr, u.val) == (hit.addr, hit.val)
            {
                u.defer = due.max(VTime(hit.at.0 + 1));
            }
            return None;
        }
        Some(StopReason::Stuck(self.stuck_report(hit)))
    }
}

/// The poll tracker as the `hang` snapshot section. The hang clocks decide where a run stops, so a
/// run split by a snapshot and restore must stop where the unsplit run does: chain, copy,
/// candidates, backoff and an unreported hit are state. Derived instead: trust in a confirmation
/// (a restore re-verifies), and the INTC epoch and stall total, of which only "moved since the last
/// read" is saved. The configuration is run identity, not section state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HangSection {
    chain: Option<Chain>,
    dirty: bool,
    irq_moved: bool,
    stall_moved: bool,
    snap: Option<HartSnap>,
    unchanged: Option<Unchanged>,
    fallback: Option<Unchanged>,
    hit: Option<HangHit>,
    backoff: Option<Backoff>,
}

fn bad(reason: &'static str) -> SnapError {
    SnapError::Malformed {
        at: HangSection::NAME,
        reason,
    }
}

impl SnapValue for VTimeValue {
    fn snap_write(&self, out: &mut Vec<u8>) {
        self.0.0.snap_write(out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        Ok(VTimeValue(VTime(u64::snap_read(r)?)))
    }
}

struct VTimeValue(VTime);

fn put_vt(t: VTime, out: &mut Vec<u8>) {
    VTimeValue(t).snap_write(out);
}

fn get_vt(r: &mut SnapReader<'_>) -> Result<VTime, SnapError> {
    Ok(VTimeValue::snap_read(r)?.0)
}

impl SnapValue for Phase {
    fn snap_write(&self, out: &mut Vec<u8>) {
        let tag: u8 = match self {
            Phase::Counting => 0,
            Phase::Snapped => 1,
            Phase::Confirmed { again: false } => 2,
            Phase::Confirmed { again: true } => 3,
            Phase::Failed => 4,
            Phase::Reverify => 5,
        };
        tag.snap_write(out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        Ok(match u8::snap_read(r)? {
            0 => Phase::Counting,
            1 => Phase::Snapped,
            2 => Phase::Confirmed { again: false },
            3 => Phase::Confirmed { again: true },
            4 => Phase::Failed,
            5 => Phase::Reverify,
            _ => return Err(bad("unknown poll chain phase")),
        })
    }
}

impl SnapValue for Chain {
    fn snap_write(&self, out: &mut Vec<u8>) {
        self.pc.snap_write(out);
        self.addr.snap_write(out);
        self.val.snap_write(out);
        self.size.snap_write(out);
        self.insns.snap_write(out);
        self.period.snap_write(out);
        self.extra.snap_write(out);
        self.extra_period.snap_write(out);
        self.repeats.snap_write(out);
        put_vt(self.first_vt, out);
        self.phase.snap_write(out);
        self.judged.snap_write(out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        Ok(Chain {
            pc: u32::snap_read(r)?,
            addr: u32::snap_read(r)?,
            val: u32::snap_read(r)?,
            size: u8::snap_read(r)?,
            insns: u64::snap_read(r)?,
            period: u64::snap_read(r)?,
            extra: u64::snap_read(r)?,
            extra_period: u64::snap_read(r)?,
            repeats: u64::snap_read(r)?,
            first_vt: get_vt(r)?,
            phase: Phase::snap_read(r)?,
            judged: bool::snap_read(r)?,
        })
    }
}

impl SnapValue for Unchanged {
    fn snap_write(&self, out: &mut Vec<u8>) {
        self.pc.snap_write(out);
        self.addr.snap_write(out);
        self.val.snap_write(out);
        put_vt(self.since, out);
        put_vt(self.last, out);
        put_vt(self.defer, out);
        self.exempt.snap_write(out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        Ok(Unchanged {
            pc: u32::snap_read(r)?,
            addr: u32::snap_read(r)?,
            val: u32::snap_read(r)?,
            since: get_vt(r)?,
            last: get_vt(r)?,
            defer: get_vt(r)?,
            exempt: bool::snap_read(r)?,
        })
    }
}

impl SnapValue for HangHit {
    fn snap_write(&self, out: &mut Vec<u8>) {
        let kind: u8 = match self.kind {
            StuckKind::Unchangeable => 0,
            StuckKind::Unchanged => 1,
            StuckKind::Fallback => 2,
        };
        kind.snap_write(out);
        self.pc.snap_write(out);
        self.addr.snap_write(out);
        self.val.snap_write(out);
        put_vt(self.since, out);
        put_vt(self.at, out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        Ok(HangHit {
            kind: match u8::snap_read(r)? {
                0 => StuckKind::Unchangeable,
                1 => StuckKind::Unchanged,
                2 => StuckKind::Fallback,
                _ => return Err(bad("unknown hang kind")),
            },
            pc: u32::snap_read(r)?,
            addr: u32::snap_read(r)?,
            val: u32::snap_read(r)?,
            since: get_vt(r)?,
            at: get_vt(r)?,
        })
    }
}

impl SnapValue for Backoff {
    fn snap_write(&self, out: &mut Vec<u8>) {
        self.pc.snap_write(out);
        self.addr.snap_write(out);
        self.shift.snap_write(out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        let b = Backoff {
            pc: u32::snap_read(r)?,
            addr: u32::snap_read(r)?,
            shift: u32::snap_read(r)?,
        };
        if b.shift > MAX_BACKOFF {
            return Err(bad("backoff exponent above MAX_BACKOFF"));
        }
        Ok(b)
    }
}

snap_struct!(HartSnap {
    x,
    pc,
    stores,
    csr,
    pmpcfg,
    pmpaddr,
    tdata1,
    tdata2
});

snap_struct!(HangSection {
    chain,
    dirty,
    irq_moved,
    stall_moved,
    snap,
    unchanged,
    fallback,
    hit,
    backoff,
});

impl HangSection {
    #[cfg(test)]
    pub(crate) fn reverified(&self) -> HangSection {
        let mut s = self.clone();
        if let Some(c) = s.chain.as_mut()
            && matches!(c.phase, Phase::Confirmed { .. })
        {
            c.phase = Phase::Reverify;
        }
        s
    }
}

impl SnapSection for HangSection {
    const NAME: &'static str = pemu_core::snap::SectionId::HANG;
    const VERSION: u16 = 1;

    fn encode(&self) -> Result<Section, SnapError> {
        value_section(self)
    }

    fn decode(section: &Section) -> Result<HangSection, SnapError> {
        value_from_section(section)
    }
}

impl Machine {
    pub fn hang_section(&self) -> HangSection {
        let p = &self.poll;
        HangSection {
            chain: p.chain,
            dirty: p.dirty,
            irq_moved: p.irq_epoch != self.irq.epoch(),
            stall_moved: p.stall_ps != self.clock.stall_ps(),
            snap: p.snap.clone(),
            unchanged: p.unchanged,
            fallback: p.fallback,
            hit: p.hit,
            backoff: p.backoff,
        }
    }

    /// Puts a [`HangSection`] back. Called after the hart, INTC and clock are restored, because the
    /// epoch and the stall total rebase on them.
    pub fn restore_hang_section(&mut self, s: &HangSection) {
        let (epoch, stall) = (self.irq.epoch(), self.clock.stall_ps());
        let hang = self.poll.hang;
        self.poll = PollTracker {
            chain: s.chain,
            dirty: s.dirty,
            irq_epoch: if s.irq_moved {
                epoch.wrapping_add(1)
            } else {
                epoch
            },
            stall_ps: if s.stall_moved {
                stall.wrapping_add(1)
            } else {
                stall
            },
            want_check: false,
            snap: s.snap.clone(),
            hang,
            unchanged: s.unchanged,
            fallback: s.fallback,
            hit: s.hit,
            backoff: s.backoff,
        };
        self.poll.new_run();
    }
}
