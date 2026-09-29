//! The run loop, its limits and outcome, and the `IdlePolicy` extension point.
//!
//! Each turn of [`Machine::run`]: journaled inputs due now, in `(at, seq)` order; scheduled events
//! due now with their wiring; stops and limits at this exact boundary; idle (a WFI hart wakes on a
//! pending line, otherwise time moves to the next wake-up through [`IdlePolicy`]); an interrupt if
//! `mstatus.MIE` allows one; one executor slice whose budget ends at the next deadline, so no
//! event is overshot; then the wiring the slice left and the stops again.
//!
//! The loop installs and removes only [`BREAKPOINT_HOOK`] and the ROM delay hook; every other hook
//! is bound at composition and dispatched by [`Machine::on_hle_hook`].

use pemu_core::sched::Owner;
use pemu_core::time::VTime;
use pemu_core::trace::IrqEvent;
use pemu_rv32::engine::{Exit, HookId};
use pemu_rv32::trap::take_interrupt;
use pemu_soc_c3::intc::{LINE_COUNT, OFF_CPU_INT_ENABLE};
use pemu_soc_c3::periph::id;

use crate::executor::Executor;
use crate::hle::HookAction;
use crate::machine::Machine;
use crate::rom_delay::{DelayStep, delay_hook_id};
use crate::stops::{StopReason, StopSet, Watchdog, WatchdogFire};

/// The most instructions one executor slice runs. It bounds how long the loop goes without
/// looking at host-side state; results never depend on it, because every deadline and stop ends a
/// slice on its own.
pub const MAX_SLICE_INSNS: u64 = 1 << 20;

/// The hook id every breakpoint is installed under. HLE hooks count up from 0, so breakpoints
/// use the top of the range.
pub const BREAKPOINT_HOOK: HookId = HookId(u32::MAX);

pub struct RunLimits {
    pub until: Option<VTime>,
    pub max_insns: Option<u64>,
    pub stops: StopSet,
}

impl RunLimits {
    pub fn insns(max_insns: u64) -> RunLimits {
        RunLimits {
            until: None,
            max_insns: Some(max_insns),
            stops: StopSet::default(),
        }
    }
}

pub struct RunOutcome {
    pub reason: StopReason,
    pub vt: VTime,
    pub insns: u64,
    pub ff_insns: u64,
    pub idle_ps: u64,
}

impl Machine {
    /// Run until a limit or a stop; never reads host time. `insns` and `idle_ps` of the outcome
    /// count this call only.
    pub fn run(&mut self, lim: RunLimits) -> RunOutcome {
        let insns_at_start = self.insns();
        self.idle_ps_at_run_start = self.idle_ps();
        self.ff_insns_at_run_start = self.ff_insns;
        self.arm_stops(&lim.stops);
        // A confirmation from before this call vouches for nothing the caller changed since.
        self.poll.new_run();
        let reason = self.run_to_stop(&lim, insns_at_start);
        self.disarm_stops();
        self.outcome(reason, insns_at_start)
    }

    fn run_to_stop(&mut self, lim: &RunLimits, insns_at_start: u64) -> StopReason {
        loop {
            self.apply_due_journal();
            self.dispatch_due_events();
            if let Some(reason) = self.pending_stop(lim) {
                return reason;
            }
            if let Some(reason) = self.limit_reached(lim, insns_at_start) {
                return reason;
            }

            // A WFI hart with a pending line wakes whatever MIE says, except in light sleep, where
            // the CPU clock is gated and only a wake source resumes it.
            if self.mcu_powered && self.hart.wfi && !self.light_sleeping() && self.irq.wfi_wake() {
                self.hart.wfi = false;
            }
            if !self.mcu_powered || self.hart.wfi {
                match self.idle_step(lim, insns_at_start) {
                    Some(reason) => return reason,
                    None => continue,
                }
            }

            if self.hart.csr.mstatus_mie()
                && let Some(line) = self.irq.deliverable()
            {
                let pc = self.hart.pc;
                self.trace.irq(self.hart.insns, IrqEvent::Take { line, pc });
                take_interrupt(&mut self.hart, line);
                self.poll.invalidate();
            }

            if let Some(reason) = self.breakpoint_before(lim) {
                return reason;
            }
            match self.rom_delay_step(lim, insns_at_start) {
                DelayStep::Skipped => continue,
                // Run past the shortcut's own hook only: a breakpoint on the loop head still stops
                // there, and a user hook that took the head over is dispatched by the slice.
                DelayStep::Execute
                    if !lim.stops.breakpoints.contains(&self.hart.pc)
                        && self.hooks.get(self.hart.pc) == Some(delay_hook_id()) =>
                {
                    self.continue_past_hook(self.hart.pc)
                }
                DelayStep::Execute => {}
                DelayStep::NotHere => {}
            }
            let budget = self.slice_budget(lim, insns_at_start);
            let slice_start = (self.hart.insns, self.hart.pc);
            let exit = self.execute(budget);
            self.apply_pending_wiring();
            match exit {
                Exit::Budget | Exit::Stop | Exit::Wfi => {}
                // A slice that ran before reaching a hooked pc ends like a budget exit; the next
                // slice starts there and dispatches the hook. The reference path always passes
                // through the loop head (journal, events, stops, limits, interrupts) before a hook,
                // so the engine must too, or the order of hook and boundary work would differ
                // between executors.
                Exit::Hook { .. } if (self.hart.insns, self.hart.pc) != slice_start => {}
                Exit::Hook { id } => {
                    // A hook exit ends a poll chain.
                    self.poll.invalidate();
                    // The instruction at the pc has not executed; the next run executes it exactly
                    // once. `HookSet` holds one id per pc, so a breakpoint on a pc another package
                    // hooked is still this run's to report.
                    let pc = self.hart.pc;
                    if id == BREAKPOINT_HOOK || lim.stops.breakpoints.contains(&pc) {
                        self.resume_breakpoint = Some(pc);
                        return StopReason::Breakpoint(pc);
                    }
                    if id == delay_hook_id() && self.rom_delay.enabled {
                        continue;
                    }
                    match self.on_hle_hook(id, pc) {
                        HookAction::RunPast => self.continue_past_hook(pc),
                        HookAction::Moved => {}
                        HookAction::Stop(reason) => return reason,
                    }
                }
                Exit::SpSpill(spill) => self.note_spill(spill),
                Exit::Halted(cause) => return StopReason::Halted(cause),
            }
            // A tripwire the slice raised comes before a hang the same read found: the stray access
            // is the cause, the hang its symptom.
            if let Some(reason) = self.take_pending_trip() {
                self.poll.invalidate();
                return reason;
            }
            if let Some(reason) = self.poll_step(lim, insns_at_start) {
                return reason;
            }
            if let Some(reason) = self.pending_stop(lim) {
                return reason;
            }
        }
    }

    /// One idle step; `None` means the loop goes round again.
    fn idle_step(&mut self, lim: &RunLimits, insns_at_start: u64) -> Option<StopReason> {
        // Once due events are dispatched, pending events do not make a hart live that no
        // interrupt can ever wake.
        if self.mcu_powered && self.hart.wfi && self.wake_unreachable() {
            return Some(StopReason::Deadlock);
        }
        let now = self.now();
        // The call's own `until` is a wake-up, so an idle hart with nothing scheduled idles to the
        // limit instead of reporting a deadlock.
        let next = self.next_wake(lim);
        // An instruction budget bounds idle too: a WFI hart with a self-rearming event (SYSTIMER
        // tick, USJ SOF, TIMG auto-reload) always has a next wake-up, so otherwise the call would
        // never return. The remaining budget converts to virtual time at the current rate.
        let next = match (next, self.idle_budget_end(lim, insns_at_start, now)) {
            (Some(t), Some(end)) => Some(t.min(end)),
            (next, _) => next,
        };
        match self.on_idle(now, next) {
            // A `SkipTo` that does not move time forward is `NoEvent`: everything due at `now` is
            // already consumed, so repeating would spin.
            IdleAction::NoEvent => Some(StopReason::Deadlock),
            IdleAction::SkipTo(t) if t <= now => Some(StopReason::Deadlock),
            IdleAction::SkipTo(t) => {
                self.idle_until(t);
                None
            }
        }
    }

    /// Whether no interrupt can ever wake the WFI hart and nothing pending can reset the chip. A
    /// line wakes the hart only when enabled, routed from a source and at a non-zero priority at or
    /// above the threshold; only a guest write changes that, and a waiting hart writes nothing.
    /// While a watchdog stage (RWDT, TIMG0/TIMG1 MWDT), a board event or a journaled input is
    /// pending the hart is conservatively not declared dead, since it may reset the chip.
    pub(crate) fn wake_unreachable(&self) -> bool {
        if self.interrupt_can_wake() || self.journal.next_time().is_some() {
            return false;
        }
        !self.sched.pending().iter().any(|(_, _, key)| {
            !matches!(key.owner, Owner::Periph(p) if p != id::RTC_CNTL && p != id::TIMG0 && p != id::TIMG1)
        })
    }

    /// Which watchdog last drove its interrupt stage, and when; `None` when neither has since its
    /// last feed, disable or reset. When both latched, the later one led to the current fault. The
    /// handler clears `INT_RAW.WDT` before it panics, so this is the evidence, not the console.
    pub fn watchdog_fired(&self) -> Option<WatchdogFire> {
        let devices = &self.soc.devices;
        [
            (Watchdog::Task, devices.timg0.wdt_interrupt_fired()),
            (Watchdog::Interrupt, devices.timg1.wdt_interrupt_fired()),
        ]
        .into_iter()
        .filter_map(|(which, fired)| fired.map(|(at, unfed)| WatchdogFire { which, at, unfed }))
        .max_by_key(|fire| fire.at.0)
    }

    /// Whether any source is routed to an enabled line at a non-zero priority at or above the
    /// threshold. When false, only a reset wakes a waiting hart.
    pub fn interrupt_can_wake(&self) -> bool {
        let enabled = self.irq.read(OFF_CPU_INT_ENABLE);
        let threshold = self.irq.threshold();
        (0..pemu_core::irq_source::SOURCE_COUNT).any(|s| {
            let line = self.irq.route(pemu_core::irq_source::IrqSource(s as u8));
            let pri = self.irq.priority(line);
            usize::from(line) < LINE_COUNT
                && line != 0
                && enabled & (1 << line) != 0
                && pri != 0
                && pri >= threshold
        })
    }

    /// A stop that fired since the loop last looked: a sleep entry, a tripwire, a watched write or
    /// a matcher, in that order.
    fn pending_stop(&mut self, lim: &RunLimits) -> Option<StopReason> {
        if let Some(reason) = self.take_wiring_stop() {
            return Some(reason);
        }
        if let Some(reason) = self.take_pending_trip() {
            self.poll.invalidate();
            return Some(reason);
        }
        if let Some((addr, pc)) = self.armed.watch_hit.take() {
            self.poll.invalidate();
            return Some(StopReason::Watchpoint { addr, pc });
        }
        self.armed
            .check_matchers(&self.io, &lim.stops.matchers)
            .map(StopReason::Matcher)
    }

    /// The breakpoint stop at the current pc for the reference path, and the once-only resume past
    /// the breakpoint a previous run stopped at.
    fn breakpoint_before(&mut self, lim: &RunLimits) -> Option<StopReason> {
        let pc = self.hart.pc;
        // A resume marker is for the pc it was set at; an interrupt or reset that moved the hart
        // consumes it.
        let resuming = self.resume_breakpoint.take() == Some(pc);
        // A breakpoint on a pc another package hooked stopped before that hook ran. The resume
        // dispatches it now, exactly once, so an observe hook still counts and a tripwire still
        // trips. A panic handler stop already ran its hook (`hook_ran_at`).
        let hook_ran = self.hle.state.hook_ran_at.take() == Some(pc);
        if resuming && hook_ran {
            self.count_user_fire(pc);
        }
        if resuming
            && !hook_ran
            && let Some(id) = self.hooks.get(pc)
            && id != BREAKPOINT_HOOK
            && id != delay_hook_id()
        {
            match self.on_hle_hook(id, pc) {
                HookAction::RunPast => {}
                HookAction::Moved => return None,
                HookAction::Stop(reason) => return Some(reason),
            }
        }
        if resuming {
            self.continue_past_hook(pc);
        }
        match self.executor {
            Executor::Engine => None,
            Executor::Reference => {
                if !resuming && lim.stops.breakpoints.contains(&pc) {
                    self.resume_breakpoint = Some(pc);
                    // The reference path's breakpoint check stands for the engine's hook exit, and
                    // ends a poll chain the same way.
                    self.poll.invalidate();
                    return Some(StopReason::Breakpoint(pc));
                }
                None
            }
        }
    }

    /// Instructions the next slice may run: up to the next deadline and `max_insns`, at most
    /// `max_slice`, at least 1. The deadline part is in clock-position units (`Hart::pos`); since
    /// every instruction costs at least one, the engine never retires more than `max_insns` allows.
    fn slice_budget(&self, lim: &RunLimits, insns_at_start: u64) -> u64 {
        let mut budget = self.max_slice;
        if let Some(max) = lim.max_insns {
            budget = budget.min(max.saturating_sub(self.budget_spent(insns_at_start)));
        }
        if let Some(t) = self.next_wake(lim) {
            budget = budget.min(self.clock.insns_until(self.hart.pos(), t));
        }
        budget.max(1)
    }

    /// Sets the most instructions one executor slice runs, at least 1. Results never depend on it,
    /// so it is not run identity; a restore keeps it and a fork carries it.
    pub fn set_max_slice(&mut self, insns: u64) {
        self.max_slice = insns.max(1);
    }

    pub fn max_slice(&self) -> u64 {
        self.max_slice
    }

    /// Arms `stops` for the run that starts now: breakpoints become engine hooks, watched pages
    /// take the slow path on every view, and matchers start from the output so far. Only hooks with
    /// id [`BREAKPOINT_HOOK`] are the loop's to install and remove.
    fn arm_stops(&mut self, stops: &StopSet) {
        let installed: Vec<u32> = self
            .hooks
            .iter()
            .filter(|&(_, id)| id == BREAKPOINT_HOOK)
            .map(|(pc, _)| pc)
            .collect();
        let mut removed = false;
        for pc in installed {
            if !stops.breakpoints.contains(&pc) {
                self.hooks.remove(pc);
                // Put back a hook bound elsewhere in the set, so the pc holds what `bind` gave it.
                if let Some(id) = self.hle.core.bound.set.get(pc) {
                    self.hooks.insert(pc, id);
                }
                removed = true;
            }
        }
        // A user observe hook added while a breakpoint held its pc takes the pc back now.
        if removed {
            self.install_user_hooks();
        }
        for &pc in &stops.breakpoints {
            if self.hooks.get(pc).is_none() {
                self.hooks.insert(pc, BREAKPOINT_HOOK);
            }
        }
        // The ROM delay hook shares the loop head with a breakpoint; once that is gone the
        // shortcut's hook goes back.
        if self.rom_delay.enabled
            && let Some(head) = self.rom_delay.head
            && self.hooks.get(head).is_none()
        {
            self.hooks.insert(head, crate::rom_delay::delay_hook_id());
        }
        self.armed.arm(stops, &self.io);
        self.armed.watches.clear();
        // A watch `StopSet::check` refuses is never armed.
        for w in stops.watches.iter().filter(|w| w.watchable()) {
            let len = w.len.max(1);
            for page in
                (w.addr & !0xFFF..=(w.addr.saturating_add(len - 1)) & !0xFFF).step_by(0x1000)
            {
                for view in pemu_soc_c3::mem::views_of(page) {
                    if self.soc.mark_slow(view) {
                        self.armed.slow_pages.push(view);
                    }
                }
            }
            if let Some(at) = pemu_soc_c3::mem::arena_offset(w.addr) {
                self.armed.watches.push((at, at + len as usize));
            }
        }
    }

    /// Undoes [`Machine::arm_stops`]'s page-table marks. Breakpoint hooks stay installed until a
    /// run arms a different set, so re-arming the same breakpoints does not re-translate their
    /// pages.
    fn disarm_stops(&mut self) {
        for page in std::mem::take(&mut self.armed.slow_pages) {
            self.soc.unmark_slow(page);
        }
        self.armed.watches.clear();
    }

    /// The limit this run has reached, if any. `until` stops at the first boundary at or after it.
    fn limit_reached(&self, lim: &RunLimits, insns_at_start: u64) -> Option<StopReason> {
        if lim
            .max_insns
            .is_some_and(|m| self.budget_spent(insns_at_start) >= m)
        {
            return Some(StopReason::MaxInsns);
        }
        if lim.until.is_some_and(|t| self.now() >= t) {
            return Some(StopReason::Until);
        }
        None
    }

    /// Instructions this run has spent against `max_insns`: those retired plus idle time in
    /// instruction-equivalents at the current rate, so a hart idling through a periodic event still
    /// comes back.
    pub(crate) fn budget_spent(&self, insns_at_start: u64) -> u64 {
        let idle_ps = self.idle_ps() - self.idle_ps_at_run_start;
        (self.insns() - insns_at_start) + idle_ps / self.clock_ps_per_insn()
    }

    fn idle_budget_end(&self, lim: &RunLimits, insns_at_start: u64, now: VTime) -> Option<VTime> {
        let max = lim.max_insns?;
        let left = max.saturating_sub(self.budget_spent(insns_at_start));
        Some(VTime(now.0.saturating_add(
            left.saturating_mul(self.clock_ps_per_insn()),
        )))
    }

    fn outcome(&self, reason: StopReason, insns_at_start: u64) -> RunOutcome {
        RunOutcome {
            reason,
            vt: self.now(),
            insns: self.insns() - insns_at_start,
            // Poll and ROM delay fast-forward credits; `insns` includes them, since they count as
            // executed.
            ff_insns: self.ff_insns - self.ff_insns_at_run_start,
            idle_ps: self.idle_ps() - self.idle_ps_at_run_start,
        }
    }
}

/// What WFI and sleep do. Decisions depend only on guest state and virtual time, so run identity
/// is unchanged.
pub trait IdlePolicy {
    fn on_idle(&mut self, now: VTime, next_event: Option<VTime>) -> IdleAction;
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IdleAction {
    SkipTo(VTime),
    /// The run loop reports Deadlock or a sleep stop.
    NoEvent,
}

/// The default `IdlePolicy`: skips to the next event, or reports that there is none.
#[derive(Copy, Clone, Debug, Default)]
pub struct SkipToNextEvent;

impl IdlePolicy for SkipToNextEvent {
    fn on_idle(&mut self, _now: VTime, next_event: Option<VTime>) -> IdleAction {
        match next_event {
            Some(t) => IdleAction::SkipTo(t),
            None => IdleAction::NoEvent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_skips_to_the_next_event() {
        let mut p = SkipToNextEvent;
        assert_eq!(
            p.on_idle(VTime(10), Some(VTime(250))),
            IdleAction::SkipTo(VTime(250))
        );
    }

    #[test]
    fn default_policy_reports_no_event_when_nothing_is_scheduled() {
        let policy: &mut dyn IdlePolicy = &mut SkipToNextEvent;
        assert_eq!(policy.on_idle(VTime(10), None), IdleAction::NoEvent);
    }

    #[test]
    fn insns_limits_only_the_instruction_count() {
        let lim = RunLimits::insns(7);
        assert_eq!(lim.max_insns, Some(7));
        assert!(lim.until.is_none());
        assert_eq!(lim.stops, StopSet::default());
    }
}

#[cfg(all(test, feature = "bundled-rom"))]
mod rom_tests {
    use super::*;
    use crate::config::{Assets, MachineConfig};
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;

    fn machine() -> Machine {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        Machine::new(MachineConfig::default(), assets).expect("the ROM fits the ROM window")
    }

    #[test]
    fn a_run_stops_at_its_instruction_limit_and_the_next_one_continues() {
        let mut m = machine();
        let first = m.run(RunLimits::insns(1_000));
        assert_eq!(first.reason, StopReason::MaxInsns);
        assert_eq!(first.insns, 1_000);
        let second = m.run(RunLimits::insns(500));
        assert_eq!(second.insns, 500);
        assert!(second.vt > first.vt);
        assert_eq!(second.ff_insns, 0);
    }

    #[test]
    fn a_run_stops_at_the_first_instruction_boundary_at_or_after_until() {
        let mut m = machine();
        let target = VTime(1_000_000);
        let out = m.run(RunLimits {
            until: Some(target),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until);
        assert!(out.vt >= target, "stopped at {:?}", out.vt);
        // One instruction of slack at most: the loop checks the limit at every boundary.
        assert!(out.vt.0 - target.0 < m.now().0 / out.insns.max(1) + 1);
    }

    #[test]
    fn a_zero_instruction_run_retires_nothing() {
        let mut m = machine();
        let out = m.run(RunLimits::insns(0));
        assert_eq!(out.reason, StopReason::MaxInsns);
        assert_eq!(out.insns, 0);
        assert_eq!(out.vt, VTime(0));
    }
}
