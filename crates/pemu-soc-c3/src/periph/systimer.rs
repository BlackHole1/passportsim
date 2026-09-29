//! SYSTIMER: the FreeRTOS tick and esp_timer (`specs/blocks/systimer.toml`).
//!
//! Two 52-bit counters count 16 ticks per microsecond from a fixed divider off the 40 MHz crystal,
//! independent of the CPU clock. Three comparators each pick a counter, run one-shot or periodic
//! and drive a level interrupt on source 37, 38 or 39.
//!
//! Nothing ticks: a counter is an epoch and a loaded value, and a comparator is a scheduled event
//! at the exact time its counter reaches the target. Idle time costs one event per alarm.
//!
//! Period mode after the load: IDF `vSystimerSetup` pulses COMP0_LOAD (`systimer_hal.c:118`)
//! before it sets `PERIOD_MODE` (`systimer_hal.c:153`). Taken literally, the load latches a
//! one-shot comparator and the FreeRTOS tick fires once. The `probe_campaign_timing` capture
//! settles it: a TARGETn_CONF write that turns `PERIOD_MODE` on starts period mode from that
//! write, with the period the last COMPn_LOAD latched, not the one the write carries. Period,
//! counter select and target are latched by COMPn_LOAD alone. Clearing `PERIOD_MODE` still waits
//! for COMPn_LOAD (no capture separates the two).
//!
//! A period of 0 in period mode fires once and not again until the next load (same capture).
//!
//! Miss compensation (`SOC_SYSTIMER_ALARM_MISS_COMPENSATE`): a target already past when loaded or
//! enabled fires at once. esp_timer relies on it and does not re-check a target it just computed.

use pemu_core::clock::{
    SYSTIMER_COUNTER_MASK, SYSTIMER_TICK_PS, systimer_count, systimer_deadline,
};
use pemu_core::fidelity::Fidelity;
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{Delta, RegSpec, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventHandle, PeriphId};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use crate::r#gen::regs_systimer as table;
use crate::regs::{Ports, Regs, Table};

use super::{Block, Cx, Peripheral, RegRead, RegWrite, Wiring};

pub const UNIT_COUNT: usize = 2;
pub const COMP_COUNT: usize = 3;

/// `SYSTIMER_TIMER_UNITn_WORK_EN` is bit `30 - n` of CONF.
const UNIT_WORK_EN: u8 = 30;
/// `SYSTIMER_TARGETn_WORK_EN` is bit `24 - n` of CONF.
const TARGET_WORK_EN: u8 = 24;
/// `SYSTIMER_TIMER_UNITn_VALUE_VALID` in UNITn_OP.
const VALUE_VALID: u8 = 29;
/// `SYSTIMER_TIMER_UNITn_UPDATE` in UNITn_OP, a write trigger.
const UPDATE: u8 = 30;
/// `SYSTIMER_TARGETn_PERIOD` in TARGETn_CONF, 26 bits.
const PERIOD_WIDTH: u8 = 26;
/// `SYSTIMER_TARGETn_PERIOD_MODE` in TARGETn_CONF.
const PERIOD_MODE: u8 = 30;
/// `SYSTIMER_TARGETn_TIMER_UNIT_SEL` in TARGETn_CONF.
const UNIT_SEL: u8 = 31;
/// The HI half of a counter or target is 20 bits.
const HI_WIDTH: u8 = 20;
/// Counter ticks from a period-0 load to its one alarm. The probe clears `INT_RAW` in the store
/// right after COMP1_LOAD and still sees the alarm, so it lands after that store; the capture's
/// 22 ticks bound it rather than measure it (class C).
pub const ZERO_PERIOD_TICKS: u64 = 2;

#[derive(Copy, Clone, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct Unit {
    epoch: VTime,
    loaded: u64,
    /// CONF bit `30 - n`: the counter advances while it is set.
    running: bool,
}

impl Unit {
    /// Counter value at `now`, wrapped to 52 bits. A stopped counter holds its value.
    fn value(&self, now: VTime) -> u64 {
        if self.running {
            systimer_count(now, self.epoch, self.loaded)
        } else {
            self.loaded
        }
    }

    fn set_running(&mut self, now: VTime, running: bool) {
        if self.running == running {
            return;
        }
        self.loaded = self.value(now);
        self.epoch = now;
        self.running = running;
    }

    /// Loads `value` at `now`: the UNITn_LOAD trigger.
    fn load(&mut self, now: VTime, value: u64) {
        self.loaded = value & SYSTIMER_COUNTER_MASK;
        self.epoch = now;
    }

    /// Virtual time at which the counter reaches `target`, or `None` while it is stopped.
    fn deadline(&self, target: u64) -> Option<VTime> {
        self.running.then(|| {
            systimer_deadline(
                self.epoch,
                target.wrapping_sub(self.loaded) & SYSTIMER_COUNTER_MASK,
            )
        })
    }
}

/// One comparator, as COMPn_LOAD last latched it.
#[derive(Copy, Clone, Default, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct Comp {
    target: u64,
    /// Ticks between fires in period mode.
    period: u32,
    period_mode: bool,
    /// TARGETn_CONF `TIMER_UNIT_SEL`: which counter the comparator watches.
    unit: usize,
    /// CONF bit `24 - n`: the comparator is active while it is set.
    active: bool,
    /// A one-shot comparator that already raised its interrupt at this target. A later CONF or
    /// LOAD write re-derives the pending fire ([`Model::rearm_all`]) and must not raise it again
    /// through miss compensation. COMPn_LOAD clears it.
    fired: bool,
    /// The scheduled fire, if any. Part of the snapshot, so a restored machine can cancel it.
    armed: Option<EventHandle>,
}

pub struct Registers;

impl Table<{ table::REG_COUNT }> for Registers {
    const BLOCK: &'static str = "systimer";

    fn specs() -> &'static [RegSpec; table::REG_COUNT] {
        &table::REGS
    }
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    regs: Regs<Registers, { table::REG_COUNT }>,
    units: [Unit; UNIT_COUNT],
    comps: [Comp; COMP_COUNT],
}

impl Default for Model {
    /// The reset state, derived from the CONF reset value 0x46000000: counter 0 runs from chip
    /// reset. esp_timer still excludes the ROM and bootloader phases because IDF pulses
    /// `SYSTIMER_RST` in `esp_timer_impl_early_init` (`wiring::gates`).
    fn default() -> Model {
        let regs = Regs::<Registers, { table::REG_COUNT }>::default();
        let conf = regs.get(table::idx::SYSTIMER_CONF);
        let mut units = [Unit::default(); UNIT_COUNT];
        for (n, unit) in units.iter_mut().enumerate() {
            unit.running = conf >> (UNIT_WORK_EN - n as u8) & 1 == 1;
        }
        let mut comps = [Comp::default(); COMP_COUNT];
        for (n, comp) in comps.iter_mut().enumerate() {
            comp.active = conf >> (TARGET_WORK_EN - n as u8) & 1 == 1;
        }
        Model { regs, units, comps }
    }
}

impl Default for Unit {
    fn default() -> Unit {
        Unit {
            epoch: VTime(0),
            loaded: 0,
            running: false,
        }
    }
}

impl Model {
    /// Interrupt source of comparator `n`: sources 37 to 39.
    pub fn source(n: usize) -> IrqSource {
        IrqSource(irq::SYSTIMER_TARGET0.0 + n as u8)
    }

    pub fn counter(&self, n: usize, now: VTime) -> u64 {
        self.units[n].value(now)
    }

    /// The period of comparator `n` in ticks while it is active in period mode with a nonzero
    /// period. Comparator 0 in period mode is the FreeRTOS tick, so the HLE reads the guest's own
    /// tick rate here rather than assuming 1 kHz.
    pub fn alarm_period(&self, n: usize) -> Option<u32> {
        let comp = self.comps.get(n)?;
        (comp.active && comp.period_mode && comp.period != 0).then_some(comp.period)
    }

    /// Restores the reset state and re-derives the counters and comparators from it.
    pub fn reset_block(&mut self, kind: ResetKind, ports: &mut Ports) {
        if !kind.clears(table::REGS[table::idx::SYSTIMER_CONF].domain) {
            return;
        }
        self.regs.reset(kind);
        for comp in &mut self.comps {
            ports.disarm(&mut comp.armed);
        }
        self.units = [Unit::default(); UNIT_COUNT];
        self.comps = [Comp::default(); COMP_COUNT];
        self.apply_conf(ports);
        self.drive_sources(ports);
    }

    pub fn load(&mut self, off: u32, size: Size, ports: &mut Ports) -> u32 {
        self.regs.read(off, size, Self::ID, ports.now, ports.ledger)
    }

    pub fn store(&mut self, off: u32, size: Size, val: u32, ports: &mut Ports) {
        let Some(delta) = self
            .regs
            .write(off, size, val, Self::ID, ports.now, ports.ledger)
        else {
            return;
        };
        match self.regs.index_of(off & !3) {
            Some(table::idx::SYSTIMER_CONF) => self.apply_conf(ports),
            Some(idx @ (table::idx::SYSTIMER_UNIT0_OP | table::idx::SYSTIMER_UNIT1_OP)) => {
                self.apply_op(idx - table::idx::SYSTIMER_UNIT0_OP, &delta, ports);
            }
            Some(idx @ (table::idx::SYSTIMER_COMP0_LOAD..=table::idx::SYSTIMER_COMP2_LOAD)) => {
                let n = idx - table::idx::SYSTIMER_COMP0_LOAD;
                if delta.triggers & 1 != 0 {
                    self.regs.clear_sc(idx, delta.triggers);
                    self.load_comparator(n, ports);
                }
            }
            Some(idx @ (table::idx::SYSTIMER_UNIT0_LOAD | table::idx::SYSTIMER_UNIT1_LOAD)) => {
                let n = idx - table::idx::SYSTIMER_UNIT0_LOAD;
                if delta.triggers & 1 != 0 {
                    self.regs.clear_sc(idx, delta.triggers);
                    let hi =
                        self.regs
                            .field(table::idx::SYSTIMER_UNIT0_LOAD_HI + 2 * n, 0, HI_WIDTH);
                    let lo = self
                        .regs
                        .field(table::idx::SYSTIMER_UNIT0_LOAD_LO + 2 * n, 0, 32);
                    self.units[n].load(ports.now, u64::from(hi) << 32 | u64::from(lo));
                    self.rearm_all(ports);
                }
            }
            Some(idx @ (table::idx::SYSTIMER_TARGET0_CONF..=table::idx::SYSTIMER_TARGET2_CONF)) => {
                // Selecting period mode re-bases the latched period (module docs).
                if delta.before >> PERIOD_MODE & 1 == 0 && delta.after >> PERIOD_MODE & 1 == 1 {
                    self.enter_period_mode(idx - table::idx::SYSTIMER_TARGET0_CONF, ports);
                }
            }
            Some(table::idx::SYSTIMER_INT_ENA) => self.drive_sources(ports),
            Some(table::idx::SYSTIMER_INT_CLR) => {
                let raw = self.regs.get(table::idx::SYSTIMER_INT_RAW) & !delta.triggers;
                self.regs.set(table::idx::SYSTIMER_INT_RAW, raw);
                self.drive_sources(ports);
            }
            _ => {}
        }
    }

    pub fn fire(&mut self, n: usize, ports: &mut Ports) {
        self.comps[n].armed = None;
        self.raise(n, ports);
        if self.comps[n].period_mode && self.comps[n].period != 0 {
            let period = u64::from(self.comps[n].period);
            self.comps[n].target =
                self.comps[n].target.wrapping_add(period) & SYSTIMER_COUNTER_MASK;
            self.rearm(n, ports);
        } else {
            // The target stays loaded and the comparator active, so record that this one-shot
            // or period-0 comparator has had its single edge.
            self.comps[n].fired = true;
        }
    }

    /// CONF: which counters run and which comparators are active.
    fn apply_conf(&mut self, ports: &mut Ports) {
        let conf = self.regs.get(table::idx::SYSTIMER_CONF);
        for n in 0..UNIT_COUNT {
            let running = conf >> (UNIT_WORK_EN - n as u8) & 1 == 1;
            self.units[n].set_running(ports.now, running);
        }
        for n in 0..COMP_COUNT {
            self.comps[n].active = conf >> (TARGET_WORK_EN - n as u8) & 1 == 1;
        }
        self.rearm_all(ports);
    }

    /// UNITn_OP: UPDATE latches the counter into VALUE_HI/LO and sets VALUE_VALID in the same
    /// access, which `systimer_hal_get_counter_value` polls for (`systimer.value_valid`).
    fn apply_op(&mut self, n: usize, delta: &Delta, ports: &mut Ports) {
        if delta.triggers >> UPDATE & 1 == 0 {
            return;
        }
        let value = self.units[n].value(ports.now);
        self.regs.set_field(
            table::idx::SYSTIMER_UNIT0_VALUE_HI + 2 * n,
            0,
            HI_WIDTH,
            (value >> 32) as u32,
        );
        self.regs.set_field(
            table::idx::SYSTIMER_UNIT0_VALUE_LO + 2 * n,
            0,
            32,
            value as u32,
        );
        self.regs
            .set_field(table::idx::SYSTIMER_UNIT0_OP + n, VALUE_VALID, 1, 1);
    }

    /// COMPn_LOAD: latch TARGETn_HI/LO and TARGETn_CONF into comparator `n`. In period mode the
    /// first fire is `counter_at_load + period` (the exact base is UNVERIFIED; IDF relies only on
    /// the cadence). A period of 0 fires once, [`ZERO_PERIOD_TICKS`] after the load.
    fn load_comparator(&mut self, n: usize, ports: &mut Ports) {
        let conf = self.regs.get(table::idx::SYSTIMER_TARGET0_CONF + n);
        let hi = self
            .regs
            .field(table::idx::SYSTIMER_TARGET0_HI + 2 * n, 0, HI_WIDTH);
        let lo = self
            .regs
            .field(table::idx::SYSTIMER_TARGET0_LO + 2 * n, 0, 32);
        let period = conf & ((1 << PERIOD_WIDTH) - 1);
        let period_mode = conf >> PERIOD_MODE & 1 == 1;
        let unit = (conf >> UNIT_SEL & 1) as usize;
        let target = if period_mode {
            let base = self.units[unit].value(ports.now);
            let first = if period == 0 {
                ZERO_PERIOD_TICKS
            } else {
                u64::from(period)
            };
            base.wrapping_add(first) & SYSTIMER_COUNTER_MASK
        } else {
            u64::from(hi) << 32 | u64::from(lo)
        };
        self.comps[n].period = period;
        self.comps[n].period_mode = period_mode;
        self.comps[n].unit = unit;
        self.comps[n].target = target;
        // A new target is a new alarm and may fire at once (miss compensation).
        self.comps[n].fired = false;
        self.rearm(n, ports);
    }

    /// A TARGETn_CONF write turned `PERIOD_MODE` on: period mode from now, first fire one latched
    /// period on. Nothing happens while the latched period is 0, a case no capture shows.
    fn enter_period_mode(&mut self, n: usize, ports: &mut Ports) {
        let period = self.comps[n].period;
        if period == 0 {
            return;
        }
        let base = self.units[self.comps[n].unit].value(ports.now);
        self.comps[n].period_mode = true;
        self.comps[n].target = base.wrapping_add(u64::from(period)) & SYSTIMER_COUNTER_MASK;
        self.comps[n].fired = false;
        self.rearm(n, ports);
    }

    /// The block's clock was gated for `ps`: both counters read as if that interval never passed,
    /// because IDF advances esp_timer by the slept time itself. The caller postpones the pending
    /// fires by the same interval.
    pub fn clock_gated_for(&mut self, ps: u64) {
        for unit in &mut self.units {
            unit.epoch = VTime(unit.epoch.0.saturating_add(ps));
        }
    }

    fn rearm_all(&mut self, ports: &mut Ports) {
        for n in 0..COMP_COUNT {
            self.rearm(n, ports);
        }
    }

    /// Schedules comparator `n` at the time its counter reaches the target, or raises it now when
    /// the target already passed.
    fn rearm(&mut self, n: usize, ports: &mut Ports) {
        ports.disarm(&mut self.comps[n].armed);
        if !self.comps[n].active {
            return;
        }
        let unit = self.units[self.comps[n].unit];
        let Some(at) = unit.deadline(self.comps[n].target) else {
            return;
        };
        if at > ports.now {
            ports.rearm(&mut self.comps[n].armed, Self::ID, n as u16, at);
            return;
        }
        if self.comps[n].fired {
            // Already fired at this target. `systimer_ll_*` writes CONF whenever it stalls a
            // counter or arms another alarm; raising again would add a spurious esp_timer
            // interrupt to every such write.
            return;
        }
        // Miss compensation: the target is already in the past.
        self.raise(n, ports);
        if !self.comps[n].period_mode || self.comps[n].period == 0 {
            self.comps[n].fired = true;
            return;
        }
        // Catch up in whole periods, so a late period alarm keeps its cadence instead of firing
        // once per event across the missed interval. `SysTickIsrHandler` recomputes the missed
        // ticks from the counter.
        let period = u64::from(self.comps[n].period).max(1);
        let behind =
            unit.value(ports.now).wrapping_sub(self.comps[n].target) & SYSTIMER_COUNTER_MASK;
        let steps = behind / period + 1;
        self.comps[n].target = self.comps[n]
            .target
            .wrapping_add(steps.saturating_mul(period))
            & SYSTIMER_COUNTER_MASK;
        if let Some(next) = unit.deadline(self.comps[n].target) {
            ports.rearm(&mut self.comps[n].armed, Self::ID, n as u16, next);
        }
    }

    /// Sets INT_RAW bit `n` and re-drives the source. RAW stays set until CLR.
    fn raise(&mut self, n: usize, ports: &mut Ports) {
        let raw = self.regs.get(table::idx::SYSTIMER_INT_RAW) | 1 << n;
        self.regs.set(table::idx::SYSTIMER_INT_RAW, raw);
        self.drive_sources(ports);
    }

    /// `ST = RAW & ENA`, and ST bit `n` is the level of source `37 + n`.
    fn drive_sources(&mut self, ports: &mut Ports) {
        let st = self.regs.get(table::idx::SYSTIMER_INT_RAW)
            & self.regs.get(table::idx::SYSTIMER_INT_ENA)
            & ((1 << COMP_COUNT) - 1);
        self.regs.set(table::idx::SYSTIMER_INT_ST, st);
        for n in 0..COMP_COUNT {
            ports.irq.set_source(Self::source(n), st >> n & 1 == 1);
        }
    }
}

impl Peripheral for Model {
    const ID: PeriphId = super::id::SYSTIMER;
    const BASE: u32 = <super::block::Systimer as Block>::BASE;
    const SIZE: u32 = <super::block::Systimer as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.reset_block(kind, &mut Ports::of(cx));
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, &mut Ports::of(cx)),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        // A write that changes a source level ends the CPU block so the interrupt is taken at the
        // next instruction. So does one that arms a fire earlier than every pending event: the
        // slice's budget ends at the event that was next when it started, so without the stop a
        // comparator loaded in a busy loop would fire up to a FreeRTOS tick late (the
        // `probe_campaign_timing` `systimer_zero_period` read 4554 ticks for a 2-tick fire).
        let epoch = cx.irq.epoch();
        let next = cx.sched.next_time();
        self.store(off, size, val, &mut Ports::of(cx));
        let sooner = match (next, cx.sched.next_time()) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(before), Some(after)) => after < before,
        };
        RegWrite {
            stop: cx.irq.epoch() != epoch || sooner,
            wiring: Wiring::None,
        }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        self.fire(usize::from(tag), &mut Ports::of(cx));
        Wiring::None
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.regs.class_at(off)
    }
}

/// One SYSTIMER tick in picoseconds: 16 ticks per microsecond.
pub const TICK_PS: u64 = SYSTIMER_TICK_PS;

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::reset::{ResetCause, ResetScope};
    use pemu_core::sched::{Owner, Scheduler};

    use crate::intc::IrqFabric;

    struct Harness {
        now: VTime,
        sched: Scheduler,
        irq: IrqFabric,
        ledger: FidelityLedger,
    }

    impl Harness {
        fn new() -> Harness {
            Harness {
                now: VTime(0),
                sched: Scheduler::new(),
                irq: IrqFabric::new(),
                ledger: FidelityLedger::default(),
            }
        }

        fn ports(&mut self) -> Ports<'_> {
            Ports {
                now: self.now,
                sched: &mut self.sched,
                irq: &mut self.irq,
                ledger: &mut self.ledger,
            }
        }

        fn run_to(&mut self, model: &mut Model, t: VTime) {
            while let Some(next) = self.sched.next_time() {
                if next > t {
                    break;
                }
                self.now = next;
                while let Some(key) = self.sched.pop_due(self.now) {
                    assert_eq!(key.owner, Owner::Periph(Model::ID));
                    model.fire(usize::from(key.tag), &mut self.ports());
                }
            }
            self.now = t;
        }
    }

    fn off(idx: usize) -> u32 {
        u32::from(table::REGS[idx].off)
    }

    /// `vSystimerSetup`: counter 1 cleared and started, alarm 0 on counter 1 in period mode with
    /// `period` ticks, interrupt enabled.
    fn setup_tick(model: &mut Model, h: &mut Harness, period: u32) {
        model.store(
            off(table::idx::SYSTIMER_UNIT1_LOAD_HI),
            Size::B4,
            0,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_UNIT1_LOAD_LO),
            Size::B4,
            0,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_UNIT1_LOAD),
            Size::B4,
            1,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_TARGET0_CONF),
            Size::B4,
            period | 1 << PERIOD_MODE | 1 << UNIT_SEL,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_COMP0_LOAD),
            Size::B4,
            1,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_INT_ENA),
            Size::B4,
            1,
            &mut h.ports(),
        );
        // CONF: counter 0 and 1 running, alarm 0 active.
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) | 1 << 29 | 1 << TARGET_WORK_EN;
        model.store(
            off(table::idx::SYSTIMER_CONF),
            Size::B4,
            conf,
            &mut h.ports(),
        );
    }

    #[test]
    fn the_counters_run_at_sixteen_ticks_per_microsecond() {
        // A fixed divider of 2.5 from the 40 MHz crystal. One tick is 62_500 ps.
        assert_eq!(TICK_PS, 62_500);
        assert_eq!(1_000_000 / TICK_PS, 16, "16 ticks per microsecond");

        let mut h = Harness::new();
        let model = Model::default();
        assert_eq!(model.counter(0, VTime(0)), 0);
        assert_eq!(
            model.counter(0, VTime(TICK_PS - 1)),
            0,
            "a partial tick does not count"
        );
        assert_eq!(model.counter(0, VTime(TICK_PS)), 1);
        assert_eq!(model.counter(0, VTime(1_000_000)), 16, "one microsecond");
        assert_eq!(
            model.counter(0, VTime(1_000_000_000)),
            16_000,
            "one millisecond"
        );
        h.now = VTime(1_000_000_000);
        assert_eq!(model.counter(1, h.now), 0);
    }

    #[test]
    fn the_conf_reset_value_starts_counter_zero_only() {
        // Bit 30 is TIMER_UNIT0_WORK_EN and bit 29 TIMER_UNIT1_WORK_EN.
        let conf = table::REGS[table::idx::SYSTIMER_CONF].reset;
        assert_eq!(conf, 0x4600_0000);
        assert_eq!(
            conf >> UNIT_WORK_EN & 1,
            1,
            "counter 0 runs from chip reset"
        );
        assert_eq!(conf >> (UNIT_WORK_EN - 1) & 1, 0, "counter 1 does not");
        for n in 0..COMP_COUNT {
            assert_eq!(
                conf >> (TARGET_WORK_EN - n as u8) & 1,
                0,
                "alarm {n} is idle"
            );
        }
        let model = Model::default();
        assert!(model.units[0].running);
        assert!(!model.units[1].running);
    }

    #[test]
    fn every_register_shares_one_reset_domain() {
        // `Model::reset_block` tests the CONF register's domain for the whole block.
        let domain = table::REGS[table::idx::SYSTIMER_CONF].domain;
        for spec in table::REGS.iter() {
            assert_eq!(spec.domain, domain, "{}", spec.name);
        }
        for scope in ResetScope::ALL {
            assert!(pemu_core::regstore::resets_in(domain, scope));
        }
    }

    #[test]
    fn the_update_write_latches_the_counter_and_sets_value_valid() {
        // `systimer_hal_get_counter_value` writes UPDATE, polls VALUE_VALID, then reads
        // VALUE_HI and VALUE_LO.
        let mut h = Harness::new();
        let mut model = Model::default();
        h.now = VTime(3_000_000);
        model.store(
            off(table::idx::SYSTIMER_UNIT0_OP),
            Size::B4,
            1 << UPDATE,
            &mut h.ports(),
        );

        let op = model.load(off(table::idx::SYSTIMER_UNIT0_OP), Size::B4, &mut h.ports());
        assert_eq!(
            op >> VALUE_VALID & 1,
            1,
            "VALUE_VALID is set inside the write"
        );
        assert_eq!(op >> UPDATE & 1, 0, "UPDATE is a write trigger and reads 0");

        let hi = model.load(
            off(table::idx::SYSTIMER_UNIT0_VALUE_HI),
            Size::B4,
            &mut h.ports(),
        );
        let lo = model.load(
            off(table::idx::SYSTIMER_UNIT0_VALUE_LO),
            Size::B4,
            &mut h.ports(),
        );
        let snapshot = u64::from(hi) << 32 | u64::from(lo);
        assert_eq!(snapshot, 48, "3 us at 16 ticks per us");
        assert_eq!(snapshot, model.counter(0, h.now));

        // The snapshot is frozen until the next UPDATE.
        h.now = VTime(9_000_000);
        let lo = model.load(
            off(table::idx::SYSTIMER_UNIT0_VALUE_LO),
            Size::B4,
            &mut h.ports(),
        );
        assert_eq!(lo, 48);
        model.store(
            off(table::idx::SYSTIMER_UNIT0_OP),
            Size::B4,
            1 << UPDATE,
            &mut h.ports(),
        );
        let lo = model.load(
            off(table::idx::SYSTIMER_UNIT0_VALUE_LO),
            Size::B4,
            &mut h.ports(),
        );
        assert_eq!(lo, 144, "9 us at 16 ticks per us");
    }

    #[test]
    fn the_unit_load_trigger_sets_the_counter_and_the_epoch() {
        // Light sleep wake uses UNITn_LOAD to advance counter 0.
        let mut h = Harness::new();
        let mut model = Model::default();
        h.now = VTime(1_000_000);
        model.store(
            off(table::idx::SYSTIMER_UNIT0_LOAD_HI),
            Size::B4,
            2,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_UNIT0_LOAD_LO),
            Size::B4,
            5,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_UNIT0_LOAD),
            Size::B4,
            1,
            &mut h.ports(),
        );
        assert_eq!(model.counter(0, h.now), (2 << 32) | 5);
        h.now = VTime(2_000_000);
        assert_eq!(model.counter(0, h.now), (2 << 32) | (5 + 16));
    }

    #[test]
    fn a_stopped_counter_holds_its_value_and_resumes_from_it() {
        let mut h = Harness::new();
        let mut model = Model::default();
        h.now = VTime(1_000_000);
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) & !(1 << UNIT_WORK_EN);
        model.store(
            off(table::idx::SYSTIMER_CONF),
            Size::B4,
            conf,
            &mut h.ports(),
        );
        assert_eq!(model.counter(0, h.now), 16);
        h.now = VTime(5_000_000);
        assert_eq!(
            model.counter(0, h.now),
            16,
            "a stopped counter does not advance"
        );
        let conf = conf | 1 << UNIT_WORK_EN;
        model.store(
            off(table::idx::SYSTIMER_CONF),
            Size::B4,
            conf,
            &mut h.ports(),
        );
        h.now = VTime(6_000_000);
        assert_eq!(
            model.counter(0, h.now),
            32,
            "it resumes from where it stopped"
        );
    }

    #[test]
    fn a_periodic_alarm_drives_the_freertos_tick() {
        // The FreeRTOS tick: counter 1, alarm 0, 16000 ticks at CONFIG_FREERTOS_HZ 1000, source
        // 37.
        const PERIOD: u32 = 16_000;
        let mut h = Harness::new();
        let mut model = Model::default();
        setup_tick(&mut model, &mut h, PERIOD);

        assert!(
            !h.irq.source(Model::source(0)),
            "no tick before the first period"
        );
        h.run_to(&mut model, VTime(999_000_000));
        assert!(!h.irq.source(Model::source(0)));
        h.run_to(&mut model, VTime(1_000_000_000));
        assert!(h.irq.source(Model::source(0)), "the tick fires after 1 ms");
        assert_eq!(
            model.load(off(table::idx::SYSTIMER_INT_ST), Size::B4, &mut h.ports()),
            1,
            "ST is RAW and ENA"
        );

        // The handler clears the status; the level follows it down and comes back next period.
        model.store(
            off(table::idx::SYSTIMER_INT_CLR),
            Size::B4,
            1,
            &mut h.ports(),
        );
        assert!(!h.irq.source(Model::source(0)));
        assert_eq!(
            model.load(off(table::idx::SYSTIMER_INT_RAW), Size::B4, &mut h.ports()),
            0
        );

        let mut ticks = 0;
        for ms in 2..=10u64 {
            h.run_to(&mut model, VTime(ms * 1_000_000_000));
            if h.irq.source(Model::source(0)) {
                ticks += 1;
                model.store(
                    off(table::idx::SYSTIMER_INT_CLR),
                    Size::B4,
                    1,
                    &mut h.ports(),
                );
            }
        }
        assert_eq!(ticks, 9, "one tick per millisecond");
    }

    /// `vSystimerSetup` in IDF's order: COMP0_LOAD before PERIOD_MODE, counter 1 last. Without
    /// the PERIOD_MODE edge taking effect, `pk` takes exactly one tick and then idles in
    /// `esp_cpu_wait_for_intr` for good.
    #[test]
    fn the_idf_setup_order_selects_period_mode_after_the_load() {
        const PERIOD: u32 = 16_000;
        let mut h = Harness::new();
        let mut model = Model::default();
        let w = |model: &mut Model, h: &mut Harness, idx: usize, val: u32| {
            model.store(off(idx), Size::B4, val, &mut h.ports());
        };
        w(&mut model, &mut h, table::idx::SYSTIMER_UNIT1_LOAD_HI, 0);
        w(&mut model, &mut h, table::idx::SYSTIMER_UNIT1_LOAD_LO, 0);
        w(&mut model, &mut h, table::idx::SYSTIMER_UNIT1_LOAD, 1);
        // `systimer_hal_set_alarm_period`: disable, period, load, enable.
        w(
            &mut model,
            &mut h,
            table::idx::SYSTIMER_TARGET0_CONF,
            1 << UNIT_SEL,
        );
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) & !(1 << TARGET_WORK_EN);
        w(&mut model, &mut h, table::idx::SYSTIMER_CONF, conf);
        w(
            &mut model,
            &mut h,
            table::idx::SYSTIMER_TARGET0_CONF,
            PERIOD | 1 << UNIT_SEL,
        );
        w(&mut model, &mut h, table::idx::SYSTIMER_COMP0_LOAD, 1);
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) | 1 << TARGET_WORK_EN;
        w(&mut model, &mut h, table::idx::SYSTIMER_CONF, conf);
        w(
            &mut model,
            &mut h,
            table::idx::SYSTIMER_TARGET0_CONF,
            PERIOD | 1 << PERIOD_MODE | 1 << UNIT_SEL,
        );
        w(&mut model, &mut h, table::idx::SYSTIMER_INT_ENA, 1);
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) | 1 << 29;
        w(&mut model, &mut h, table::idx::SYSTIMER_CONF, conf);
        assert!(
            !h.irq.source(Model::source(0)),
            "no tick before the first period"
        );

        let mut ticks = 0;
        for us in 1..=10_000u64 {
            h.run_to(&mut model, VTime(us * 1_000_000));
            if h.irq.source(Model::source(0)) {
                ticks += 1;
                w(&mut model, &mut h, table::idx::SYSTIMER_INT_CLR, 1);
            }
        }
        assert_eq!(ticks, 10, "one tick per millisecond over 10 ms");
    }

    /// The raw interrupt times of comparator 1 up to `until`, polled every 100 ns and cleared as
    /// the probe clears them, in counter-0 ticks.
    fn raw1_ticks(model: &mut Model, h: &mut Harness, until: VTime) -> Vec<u64> {
        let mut seen = Vec::new();
        while h.now < until {
            let t = VTime(h.now.0 + 100_000);
            h.run_to(model, t);
            if model.regs.get(table::idx::SYSTIMER_INT_RAW) >> 1 & 1 == 1 {
                seen.push(model.counter(0, h.now));
                model.store(
                    off(table::idx::SYSTIMER_INT_CLR),
                    Size::B4,
                    1 << 1,
                    &mut h.ports(),
                );
            }
        }
        seen
    }

    /// As the `probe_campaign_timing` capture measured on comparator 1: in IDF's order the first
    /// alarm is one period after the `PERIOD_MODE` write, and a new period written with the mode
    /// off and on, without COMP1_LOAD, keeps the latched period.
    #[test]
    fn a_period_mode_write_re_bases_the_latched_period_and_not_the_written_one() {
        let mut h = Harness::new();
        let mut model = Model::default();
        let w = |model: &mut Model, h: &mut Harness, idx: usize, val: u32| {
            model.store(off(idx), Size::B4, val, &mut h.ports());
        };
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) | 1 << (TARGET_WORK_EN - 1);
        w(&mut model, &mut h, table::idx::SYSTIMER_CONF, conf);
        h.run_to(&mut model, VTime(1_000_000_000));
        w(
            &mut model,
            &mut h,
            table::idx::SYSTIMER_TARGET1_CONF,
            16_000,
        );
        w(&mut model, &mut h, table::idx::SYSTIMER_COMP1_LOAD, 1);
        w(&mut model, &mut h, table::idx::SYSTIMER_INT_CLR, 1 << 1);
        h.run_to(&mut model, VTime(1_500_000_000));
        let t_mode = model.counter(0, h.now);
        w(
            &mut model,
            &mut h,
            table::idx::SYSTIMER_TARGET1_CONF,
            16_000 | 1 << PERIOD_MODE,
        );
        let seen = raw1_ticks(&mut model, &mut h, VTime(1_500_000_000 + 2_100_000_000));
        assert_eq!(seen.len(), 2, "two alarms in 2.1 ms: {seen:?}");
        assert_eq!(seen[0] - t_mode, 16_000, "one period after the mode write");
        assert_eq!(seen[1] - seen[0], 16_000);

        // A new period with the mode off, then on, and no load: the latched 16000 goes on,
        // re-based at the second write.
        w(
            &mut model,
            &mut h,
            table::idx::SYSTIMER_TARGET1_CONF,
            32_000,
        );
        let t_mode = model.counter(0, h.now);
        w(
            &mut model,
            &mut h,
            table::idx::SYSTIMER_TARGET1_CONF,
            32_000 | 1 << PERIOD_MODE,
        );
        w(&mut model, &mut h, table::idx::SYSTIMER_INT_CLR, 1 << 1);
        let from = h.now;
        let seen = raw1_ticks(&mut model, &mut h, VTime(from.0 + 2_100_000_000));
        assert_eq!(
            seen.len(),
            2,
            "the old period's cadence, not 32000: {seen:?}"
        );
        assert_eq!(seen[0] - t_mode, 16_000);
        assert_eq!(seen[1] - seen[0], 16_000);
    }

    /// Same capture: a period-0 load in period mode fires once, after a store that follows the
    /// load, and never again.
    #[test]
    fn a_zero_period_load_fires_once_after_the_load() {
        let mut h = Harness::new();
        let mut model = Model::default();
        let w = |model: &mut Model, h: &mut Harness, idx: usize, val: u32| {
            model.store(off(idx), Size::B4, val, &mut h.ports());
        };
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) | 1 << (TARGET_WORK_EN - 1);
        w(&mut model, &mut h, table::idx::SYSTIMER_CONF, conf);
        h.run_to(&mut model, VTime(1_000_031_250));
        w(
            &mut model,
            &mut h,
            table::idx::SYSTIMER_TARGET1_CONF,
            1 << PERIOD_MODE,
        );
        let t_load = model.counter(0, h.now);
        w(&mut model, &mut h, table::idx::SYSTIMER_COMP1_LOAD, 1);
        // The probe's INT_CLR one store later does not lose the alarm.
        h.now = VTime(h.now.0 + 6_250);
        w(&mut model, &mut h, table::idx::SYSTIMER_INT_CLR, 1 << 1);
        let seen = raw1_ticks(&mut model, &mut h, VTime(1_000_031_250 + 4_000_000_000));
        assert_eq!(seen.len(), 1, "one alarm in 4 ms: {seen:?}");
        assert!(
            (1..=ZERO_PERIOD_TICKS + 2).contains(&(seen[0] - t_load)),
            "the alarm lands just after the load: {seen:?} from {t_load}"
        );
        assert_eq!(h.sched.len(), 0, "and nothing stays scheduled");
    }

    /// A write that arms a fire earlier than every pending event ends the slice; one that stores
    /// a target or arms a later fire does not.
    #[test]
    fn a_write_that_arms_a_sooner_fire_ends_the_slice() {
        use crate::intc::testing::TestPorts;
        let mut p = TestPorts::new();
        let mut model = Model::default();
        let mut w = |p: &mut TestPorts, idx: usize, val: u32| {
            p.with(|cx| Peripheral::write(&mut model, off(idx), Size::B4, val, cx).stop)
        };
        let conf = Model::default().regs.get(table::idx::SYSTIMER_CONF)
            | 1 << (TARGET_WORK_EN - 1)
            | 1 << (TARGET_WORK_EN - 2);
        assert!(!w(&mut p, table::idx::SYSTIMER_CONF, conf), "nothing armed");
        // Comparator 2, one-shot at 1,000,000 ticks (62.5 ms): the first pending event.
        assert!(
            !w(&mut p, table::idx::SYSTIMER_TARGET2_LO, 1_000_000),
            "a stored target"
        );
        assert!(
            w(&mut p, table::idx::SYSTIMER_COMP2_LOAD, 1),
            "the first event ends it"
        );
        // Comparator 1 in period mode at 100 ms: later than the pending one.
        w(
            &mut p,
            table::idx::SYSTIMER_TARGET1_CONF,
            1_600_000 | 1 << PERIOD_MODE,
        );
        assert!(
            !w(&mut p, table::idx::SYSTIMER_COMP1_LOAD, 1),
            "a fire after the pending one does not end it"
        );
        // A period of 0 arms a fire 2 ticks away, sooner than 62.5 ms.
        w(&mut p, table::idx::SYSTIMER_TARGET1_CONF, 1 << PERIOD_MODE);
        assert!(
            w(&mut p, table::idx::SYSTIMER_COMP1_LOAD, 1),
            "a sooner fire ends it"
        );
    }

    /// Selecting period mode while no period is latched raises nothing and schedules nothing.
    #[test]
    fn period_mode_with_a_zero_period_does_not_load_the_comparator() {
        let mut h = Harness::new();
        let mut model = Model::default();
        let w = |model: &mut Model, h: &mut Harness, idx: usize, val: u32| {
            model.store(off(idx), Size::B4, val, &mut h.ports());
        };
        w(&mut model, &mut h, table::idx::SYSTIMER_INT_ENA, 1);
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) | 1 << 29 | 1 << TARGET_WORK_EN;
        w(&mut model, &mut h, table::idx::SYSTIMER_CONF, conf);
        h.run_to(&mut model, VTime(1_000_000_000));
        w(&mut model, &mut h, table::idx::SYSTIMER_INT_CLR, 1);
        let pending = h.sched.len();

        w(
            &mut model,
            &mut h,
            table::idx::SYSTIMER_TARGET0_CONF,
            1 << PERIOD_MODE | 1 << UNIT_SEL,
        );
        assert_eq!(
            model.load(off(table::idx::SYSTIMER_INT_RAW), Size::B4, &mut h.ports()),
            0,
            "no spurious INT_RAW"
        );
        assert!(!h.irq.source(Model::source(0)));
        assert_eq!(h.sched.len(), pending, "and nothing newly scheduled");
    }

    #[test]
    fn a_one_shot_alarm_fires_once() {
        // esp_timer: counter 0, alarm 2, one-shot, source 39.
        let mut h = Harness::new();
        let mut model = Model::default();
        model.store(
            off(table::idx::SYSTIMER_TARGET2_HI),
            Size::B4,
            0,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_TARGET2_LO),
            Size::B4,
            160,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_TARGET2_CONF),
            Size::B4,
            0,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_COMP2_LOAD),
            Size::B4,
            1,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_INT_ENA),
            Size::B4,
            1 << 2,
            &mut h.ports(),
        );
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) | 1 << (TARGET_WORK_EN - 2);
        model.store(
            off(table::idx::SYSTIMER_CONF),
            Size::B4,
            conf,
            &mut h.ports(),
        );

        h.run_to(&mut model, VTime(9_000_000));
        assert!(!h.irq.source(Model::source(2)), "160 ticks is 10 us");
        h.run_to(&mut model, VTime(10_000_000));
        assert!(h.irq.source(Model::source(2)));
        model.store(
            off(table::idx::SYSTIMER_INT_CLR),
            Size::B4,
            1 << 2,
            &mut h.ports(),
        );
        assert!(!h.irq.source(Model::source(2)));

        h.run_to(&mut model, VTime(100_000_000));
        assert!(
            !h.irq.source(Model::source(2)),
            "a one-shot alarm does not repeat"
        );
    }

    #[test]
    fn a_target_already_in_the_past_fires_at_once() {
        // esp_timer does not re-check a target it computed a few instructions earlier, so a stale
        // target must fire inside the COMP_LOAD write rather than a 52-bit wrap later.
        let mut h = Harness::new();
        let mut model = Model::default();
        model.store(
            off(table::idx::SYSTIMER_TARGET2_CONF),
            Size::B4,
            0,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_INT_ENA),
            Size::B4,
            1 << 2,
            &mut h.ports(),
        );

        // `systimer_hal_set_alarm_target`: disable, target, COMP_LOAD, enable.
        model.store(
            off(table::idx::SYSTIMER_TARGET2_LO),
            Size::B4,
            1_600,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_COMP2_LOAD),
            Size::B4,
            1,
            &mut h.ports(),
        );
        let conf = model.regs.get(table::idx::SYSTIMER_CONF) | 1 << (TARGET_WORK_EN - 2);
        model.store(
            off(table::idx::SYSTIMER_CONF),
            Size::B4,
            conf,
            &mut h.ports(),
        );
        h.now = VTime(1_000_000);
        assert_eq!(model.counter(0, h.now), 16);
        assert!(!h.irq.source(Model::source(2)), "1600 ticks is 100 us away");
        assert_eq!(h.sched.len(), 1, "the alarm is scheduled as an event");

        model.store(
            off(table::idx::SYSTIMER_TARGET2_LO),
            Size::B4,
            8,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_COMP2_LOAD),
            Size::B4,
            1,
            &mut h.ports(),
        );
        assert!(
            h.irq.source(Model::source(2)),
            "the past target fires inside the write"
        );
        assert_eq!(h.sched.len(), 0, "and schedules nothing");

        // It fires once; rewriting CONF re-derives every comparator, and the stale target must
        // not go through miss compensation a second time.
        model.store(
            off(table::idx::SYSTIMER_INT_CLR),
            Size::B4,
            1 << 2,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_CONF),
            Size::B4,
            model.regs.get(table::idx::SYSTIMER_CONF),
            &mut h.ports(),
        );
        assert_eq!(
            model.load(off(table::idx::SYSTIMER_INT_RAW), Size::B4, &mut h.ports()),
            0,
            "miss compensation applies to the loaded target, not to every CONF write"
        );
        assert!(!h.irq.source(Model::source(2)));
    }

    #[test]
    fn a_fired_one_shot_is_not_re_raised_by_a_later_conf_write() {
        // A one-shot that fired keeps its target and stays active, and every CONF write
        // re-derives all three comparators, which must not raise the interrupt again.
        let mut h = Harness::new();
        let mut model = Model::default();
        model.store(
            off(table::idx::SYSTIMER_TARGET2_CONF),
            Size::B4,
            0,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_TARGET2_LO),
            Size::B4,
            160,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_COMP2_LOAD),
            Size::B4,
            1,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_INT_ENA),
            Size::B4,
            1 << 2,
            &mut h.ports(),
        );
        let armed = model.regs.get(table::idx::SYSTIMER_CONF) | 1 << (TARGET_WORK_EN - 2);
        model.store(
            off(table::idx::SYSTIMER_CONF),
            Size::B4,
            armed,
            &mut h.ports(),
        );

        h.run_to(&mut model, VTime(10_000_000));
        assert!(h.irq.source(Model::source(2)), "160 ticks is 10 us");
        model.store(
            off(table::idx::SYSTIMER_INT_CLR),
            Size::B4,
            1 << 2,
            &mut h.ports(),
        );
        assert!(!h.irq.source(Model::source(2)));

        model.store(
            off(table::idx::SYSTIMER_CONF),
            Size::B4,
            armed,
            &mut h.ports(),
        );
        assert_eq!(
            model.load(off(table::idx::SYSTIMER_INT_RAW), Size::B4, &mut h.ports()),
            0,
            "the fired one-shot does not set RAW again"
        );
        assert!(!h.irq.source(Model::source(2)));
        assert_eq!(h.sched.len(), 0, "and schedules nothing");

        // Nor does starting the other counter (`systimer_ll_enable_counter`).
        model.store(
            off(table::idx::SYSTIMER_CONF),
            Size::B4,
            armed | 1 << (UNIT_WORK_EN - 1),
            &mut h.ports(),
        );
        assert_eq!(
            model.load(off(table::idx::SYSTIMER_INT_RAW), Size::B4, &mut h.ports()),
            0
        );

        model.store(
            off(table::idx::SYSTIMER_TARGET2_LO),
            Size::B4,
            320,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_COMP2_LOAD),
            Size::B4,
            1,
            &mut h.ports(),
        );
        h.run_to(&mut model, VTime(20_000_000));
        assert!(
            h.irq.source(Model::source(2)),
            "COMP_LOAD arms the comparator again"
        );
    }

    #[test]
    fn the_raw_bit_stays_set_until_int_clr_and_ena_gates_the_source() {
        let mut h = Harness::new();
        let mut model = Model::default();
        setup_tick(&mut model, &mut h, 16);
        h.run_to(&mut model, VTime(1_000_000));
        assert_eq!(
            model.load(off(table::idx::SYSTIMER_INT_RAW), Size::B4, &mut h.ports()),
            1
        );
        assert!(h.irq.source(Model::source(0)));

        // Masking the interrupt lowers the source but keeps RAW.
        model.store(
            off(table::idx::SYSTIMER_INT_ENA),
            Size::B4,
            0,
            &mut h.ports(),
        );
        assert!(!h.irq.source(Model::source(0)));
        assert_eq!(
            model.load(off(table::idx::SYSTIMER_INT_RAW), Size::B4, &mut h.ports()),
            1
        );
        model.store(
            off(table::idx::SYSTIMER_INT_ENA),
            Size::B4,
            1,
            &mut h.ports(),
        );
        assert!(
            h.irq.source(Model::source(0)),
            "unmasking delivers the pending status"
        );

        model.store(
            off(table::idx::SYSTIMER_INT_CLR),
            Size::B4,
            1,
            &mut h.ports(),
        );
        assert_eq!(
            model.load(off(table::idx::SYSTIMER_INT_RAW), Size::B4, &mut h.ports()),
            0
        );
        assert!(!h.irq.source(Model::source(0)));
    }

    #[test]
    fn the_counters_wrap_at_fifty_two_bits() {
        let mut h = Harness::new();
        let mut model = Model::default();
        let top = SYSTIMER_COUNTER_MASK;
        assert_eq!(top, (1 << 52) - 1);
        model.store(
            off(table::idx::SYSTIMER_UNIT0_LOAD_HI),
            Size::B4,
            (top >> 32) as u32,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_UNIT0_LOAD_LO),
            Size::B4,
            top as u32,
            &mut h.ports(),
        );
        model.store(
            off(table::idx::SYSTIMER_UNIT0_LOAD),
            Size::B4,
            1,
            &mut h.ports(),
        );
        assert_eq!(model.counter(0, h.now), top);
        assert_eq!(
            model.counter(0, VTime(TICK_PS)),
            0,
            "the counter wraps to 0"
        );
        assert_eq!(model.counter(0, VTime(2 * TICK_PS)), 1);
    }

    #[test]
    fn the_snapshot_state_keeps_a_pending_alarm_cancellable() {
        // `Comp::armed` is in the snapshot so a restored machine can cancel the event the
        // scheduler section restored with it.
        let mut h = Harness::new();
        let mut model = Model::default();
        setup_tick(&mut model, &mut h, 16_000);
        assert_eq!(h.sched.len(), 1, "one alarm pending");

        let mut restored = Model {
            units: model.units,
            comps: model.comps,
            ..Model::default()
        };
        let sys = ResetKind::of(ResetCause::RTC_SW_SYS).expect("documented cause");
        restored.reset_block(sys, &mut h.ports());
        assert_eq!(
            h.sched.len(),
            0,
            "the restored handle cancels the event the original scheduled"
        );
    }

    #[test]
    fn a_reset_restores_the_block_and_lowers_its_sources() {
        let mut h = Harness::new();
        let mut model = Model::default();
        setup_tick(&mut model, &mut h, 16_000);
        h.run_to(&mut model, VTime(1_000_000_000));
        assert!(h.irq.source(Model::source(0)));

        let sys = ResetKind::of(ResetCause::RTC_SW_SYS).expect("documented cause");
        model.reset_block(sys, &mut h.ports());
        assert!(!h.irq.source(Model::source(0)));
        assert_eq!(h.sched.len(), 0, "the pending alarm is cancelled");
        assert_eq!(
            model.regs.get(table::idx::SYSTIMER_CONF),
            table::REGS[table::idx::SYSTIMER_CONF].reset
        );
        assert_eq!(
            model.counter(0, h.now),
            0,
            "counter 0 restarts at the reset"
        );
        assert_eq!(model.counter(0, VTime(h.now.0 + 1_000_000)), 16);

        // A CPU-only reset reaches the hart and SENSITIVE, not this block.
        setup_tick(&mut model, &mut h, 16_000);
        let pending = h.sched.len();
        let cpu = ResetKind::of(ResetCause::RTC_SW_CPU).expect("documented cause");
        model.reset_block(cpu, &mut h.ports());
        assert_eq!(h.sched.len(), pending);
    }

    /// A gated clock does not count the gated interval. A running counter and its
    /// comparator both move by it once the caller postpones the block's events.
    #[test]
    fn a_gated_clock_skips_the_gated_interval() {
        let mut h = Harness::new();
        let mut model = Model::default();
        setup_tick(&mut model, &mut h, 16_000);
        h.run_to(&mut model, VTime(5_000_000_000));
        let before = model.counter(1, h.now);
        let next = h.sched.next_time().expect("the tick is armed");
        let gate = 2_000_000_000_000;
        model.clock_gated_for(gate);
        h.sched.postpone(gate, |_| true);
        h.now = VTime(h.now.0 + gate);
        assert_eq!(
            model.counter(1, h.now),
            before,
            "no tick counted while gated"
        );
        assert_eq!(h.sched.next_time(), Some(VTime(next.0 + gate)));
    }
}
