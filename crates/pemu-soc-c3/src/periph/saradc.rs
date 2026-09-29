//! APB_SARADC one-shot conversions (`specs/blocks/saradc.toml`, `specs/notes/g3-behavior.md`
//! `g3-adc-read-path`).
//!
//! The block exists for one busy-wait: `adc_oneshot_hal_convert` raises `onetime_start` and
//! `adc_oneshot_ll_get_event` spins until `INT_RAW` bit 31 is set. Without the bit the esp_timer
//! task never leaves the poll and starves `main`.
//!
//! Under `fast` the rising edge latches the code and the done bit inside the same access. Under a
//! profile with a nonzero `adc_conversion_ps` (`device`), the edge schedules the end that much
//! later ([`Model::start_timed`]). That value is fitted from the `probe_campaign_regs` capture at
//! the one clock the driver programs; the divider and `FSM_WAIT` counts are stored, not applied.
//! The button driver reads the ADC about 200 times per emulated second, so this path allocates
//! nothing.
//!
//! The code latched is the inverse of the firmware's own calibration curve at the pin voltage, so
//! the firmware reads back what the board put on GPIO0; `pemu_board::ladder` owns the curve.
//!
//! `INT_ST` (`INT_RAW & INT_ENA`) is the level of source 43. A conversion latches in
//! `wiring::adc` without a `Cx`, so the machine calls [`Model::sync_irq`] right after it.

use pemu_board::ladder::{AdcCal, Atten, inverse_curve};
use pemu_core::fidelity::{Fidelity, FidelityLedger, TouchAccess};
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use crate::r#gen::regs_saradc::{BLOCK_SIZE, REG_COUNT, REGS, idx};
use crate::regs::{self, TouchAt};

use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring, block};
use crate::intc::IrqFabric;

pub const IRQ_SOURCE: IrqSource = irq::APB_ADC;

/// The tag of the event that ends a timed conversion ([`Model::start_timed`]).
const TAG_CONVERTED: u16 = 1;

/// `ONETIME_SAMPLE.onetime_atten`, bits [24:23].
const ONETIME_ATTEN_SHIFT: u32 = 23;
/// `ONETIME_SAMPLE.onetime_channel`, bits [28:25], holding `unit << 3 | channel`.
const ONETIME_CHANNEL_SHIFT: u32 = 25;
const ONETIME_START: u32 = 1 << 29;
const ADC2_ONETIME_SAMPLE: u32 = 1 << 30;
const ADC1_ONETIME_SAMPLE: u32 = 1 << 31;

pub const INT_ADC2_DONE: u32 = 1 << 30;
/// `INT_RAW.adc1_done`, bit 31: the bit `adc_oneshot_ll_get_event` polls.
pub const INT_ADC1_DONE: u32 = 1 << 31;

/// Mask the driver applies to `1_DATA_STATUS`.
pub const DATA_MASK: u32 = 0xFFF;

/// Which converter a one-shot conversion runs on, numbered as the driver's `ADC_UNIT_1` and
/// `ADC_UNIT_2` and as `BoardPorts::adc_mv` takes them.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AdcUnit {
    /// ADC1: the button ladder on GPIO0.
    One,
    /// ADC2: nothing on this board is wired to it.
    Two,
}

impl AdcUnit {
    /// The unit number `BoardPorts::adc_mv` takes, 1 or 2.
    pub const fn number(self) -> u8 {
        match self {
            AdcUnit::One => 1,
            AdcUnit::Two => 2,
        }
    }

    const fn done_bit(self) -> u32 {
        match self {
            AdcUnit::One => INT_ADC1_DONE,
            AdcUnit::Two => INT_ADC2_DONE,
        }
    }

    const fn data_reg(self) -> usize {
        match self {
            AdcUnit::One => idx::APB_SARADC_1_DATA_STATUS,
            AdcUnit::Two => idx::APB_SARADC_2_DATA_STATUS,
        }
    }
}

/// What a rising edge of `onetime_start` asked for.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Conversion {
    pub unit: AdcUnit,
    /// Channel inside the unit, the low three bits of `onetime_channel`.
    pub channel: u8,
    /// Attenuation; `adc_oneshot_hal_setup` writes 3, which is 12 dB.
    pub atten: Atten,
}

crate::regs::store_serde!();

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    #[serde(with = "regs_serde")]
    regs: RegStore<REG_COUNT>,
    /// The eFuse calibration the guest's curve fitting reads; the latched code is its inverse.
    cal: AdcCal,
    touched: u64,
    /// Unit, channel and end (ps of virtual time) of a timed conversion whose end has not fired
    /// yet ([`Model::start_timed`]).
    pending: Option<(u8, u8, u64)>,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            regs: RegStore::new(&REGS),
            cal: AdcCal::default(),
            touched: 0,
            pending: None,
        }
    }
}

impl Model {
    /// The eFuse calibration this block inverts. The default is the synthesized eFuse: version 1
    /// with every calibration field 0, the column the ladder codes 0, 433, 862 and 4095 are
    /// pinned to.
    pub fn calibration(&self) -> AdcCal {
        self.cal
    }

    /// Sets the calibration the conversions invert. `Machine::power_on` passes the decoded BLK2
    /// words; only an `--efuse-dump` image changes the value from [`AdcCal::default`].
    pub fn set_calibration(&mut self, cal: AdcCal) {
        self.cal = cal;
    }

    fn refresh_status(&mut self) {
        let st = self.regs.get(idx::APB_SARADC_INT_RAW) & self.regs.get(idx::APB_SARADC_INT_ENA);
        self.regs.set(idx::APB_SARADC_INT_ST, st);
    }

    /// What an `ONETIME_SAMPLE` value asks for, or `None` when neither converter is enabled (the
    /// driver disables both before enabling the one it wants).
    fn requested(val: u32) -> Option<Conversion> {
        let unit = if val & ADC1_ONETIME_SAMPLE != 0 {
            AdcUnit::One
        } else if val & ADC2_ONETIME_SAMPLE != 0 {
            AdcUnit::Two
        } else {
            return None;
        };
        let channel_field = (val >> ONETIME_CHANNEL_SHIFT) & 0xF;
        Some(Conversion {
            unit,
            atten: Self::atten_of(val),
            // `onetime_channel` is `unit << 3 | channel`; the unit half is redundant with the
            // sample-enable bits, which are what start a converter.
            channel: (channel_field & 0x7) as u8,
        })
    }

    const fn atten_of(val: u32) -> Atten {
        match (val >> ONETIME_ATTEN_SHIFT) & 0x3 {
            0 => Atten::Db0,
            1 => Atten::Db2_5,
            2 => Atten::Db6,
            _ => Atten::Db12,
        }
    }

    /// Latches the code for `mv` at the requested attenuation and the unit's done bit. ADC2
    /// latches 0 (class C: nothing on this board is wired to it).
    pub fn latch(&mut self, conversion: Conversion, mv: u32) {
        let raw = match conversion.unit {
            AdcUnit::One => u32::from(inverse_curve(mv, conversion.atten, self.cal)),
            AdcUnit::Two => 0,
        };
        self.regs.set(conversion.unit.data_reg(), raw & DATA_MASK);
        let raw_int = self.regs.get(idx::APB_SARADC_INT_RAW) | conversion.unit.done_bit();
        self.regs.set(idx::APB_SARADC_INT_RAW, raw_int);
        self.refresh_status();
    }

    pub fn atten(&self) -> Atten {
        Self::atten_of(self.regs.get(idx::APB_SARADC_ONETIME_SAMPLE))
    }

    /// The code a unit last latched, masked as the driver reads it.
    pub fn data(&self, unit: AdcUnit) -> u16 {
        (self.regs.get(unit.data_reg()) & DATA_MASK) as u16
    }

    pub fn done(&self, unit: AdcUnit) -> bool {
        self.regs.get(idx::APB_SARADC_INT_RAW) & unit.done_bit() != 0
    }

    /// Until when a read of `off` keeps returning the same value, for the hang detector.
    /// `1_DATA_STATUS` changes only on an input event or at the end of a running timed
    /// conversion; `INT_RAW` is set inside the starting access or by the block's own event.
    pub fn stability(&self, off: u32) -> Stability {
        match regs::reg_at(&REGS, off) {
            Some((i, _)) if i == idx::APB_SARADC_1_DATA_STATUS && self.pending.is_some() => {
                Stability::UntilNextEvent
            }
            Some((i, _)) if i == idx::APB_SARADC_1_DATA_STATUS => Stability::UntilInput,
            Some((i, _)) if REGS[i].stable_read => Stability::UntilNextEvent,
            _ => Stability::Never,
        }
    }

    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    pub fn reg(&self, off: u32) -> u32 {
        regs::reg_at(&REGS, off).map_or(0, |(idx, _)| self.regs.get(idx))
    }

    pub fn irq_level(&self) -> bool {
        self.regs.get(idx::APB_SARADC_INT_ST) != 0
    }

    /// Drives [`IRQ_SOURCE`] to [`Model::irq_level`]. The machine also calls it after
    /// `wiring::adc::sample`, which latches outside any entry point.
    pub fn sync_irq(&self, irq: &mut IrqFabric) {
        irq.set_source(IRQ_SOURCE, self.irq_level());
    }

    /// Restores the registers on every reset that reaches the blocks. The calibration comes from
    /// the eFuse, which a digital reset does not clear.
    pub fn reset_regs(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.regs.reset(kind.scope);
        self.pending = None;
        self.refresh_status();
    }

    /// Starts the conversion a rising edge asked for so that it ends `conversion_ps` after `now`,
    /// and returns whether it scheduled that end. A start while one runs is dropped (UNVERIFIED:
    /// the driver never starts one before the done bit).
    pub fn start_timed(
        &mut self,
        unit: u8,
        channel: u8,
        now: VTime,
        conversion_ps: u64,
        sched: &mut Scheduler,
    ) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let end = now.0.saturating_add(conversion_ps);
        self.pending = Some((unit, channel, end));
        sched.schedule(
            now,
            VTime(end),
            EventKey {
                owner: Owner::Periph(<Self as Peripheral>::ID),
                tag: TAG_CONVERTED,
            },
        );
        true
    }

    /// The end of a timed conversion: `Wiring::AdcSample` to sample the board and latch, or
    /// `Wiring::None` when a reset overtook the conversion (the scheduler does not cancel the
    /// event) and a later one, if any, ends at its own event.
    pub fn finish_timed(&mut self, now: VTime) -> Wiring {
        match self.pending {
            Some((unit, channel, end)) if end <= now.0 => {
                self.pending = None;
                Wiring::AdcSample { unit, channel }
            }
            _ => Wiring::None,
        }
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((idx, byte_off)) = regs::reg_at(&REGS, off) else {
            regs::hole_classed(off, TouchAccess::Read, at, ledger);
            return 0;
        };
        let touched = std::slice::from_mut(&mut self.touched);
        regs::touch_classed(touched, &REGS, idx, TouchAccess::Read, at, ledger);
        self.regs.read(idx, byte_off, size)
    }

    /// Writes the low `size` bytes of `val` at block offset `off`. A rising edge of
    /// `onetime_start` with a converter enabled returns `Wiring::AdcSample`.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> Wiring {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((idx, byte_off)) = regs::reg_at(&REGS, off) else {
            regs::hole_classed(off, TouchAccess::Write, at, ledger);
            return Wiring::None;
        };
        let touched = std::slice::from_mut(&mut self.touched);
        regs::touch_classed(touched, &REGS, idx, TouchAccess::Write, at, ledger);
        let delta = self.regs.write(idx, byte_off, size, val);
        let mut wiring = Wiring::None;
        match idx {
            idx::APB_SARADC_ONETIME_SAMPLE => {
                let rising = delta.after & !delta.before & ONETIME_START;
                if rising != 0
                    && let Some(conversion) = Self::requested(delta.after)
                {
                    wiring = Wiring::AdcSample {
                        unit: conversion.unit.number(),
                        channel: conversion.channel,
                    };
                }
            }
            idx::APB_SARADC_INT_CLR => {
                // Write-only fields: `delta.after` is exactly the bits written 1, each clearing
                // its `INT_RAW` bit. The register goes back to 0 so a later access to another
                // byte cannot clear a bit twice.
                let raw = self.regs.get(idx::APB_SARADC_INT_RAW) & !delta.after;
                self.regs.set(idx::APB_SARADC_INT_RAW, raw);
                self.regs.set(idx::APB_SARADC_INT_CLR, 0);
            }
            _ => {}
        }
        self.refresh_status();
        wiring
    }
}

impl Peripheral for Model {
    const ID: PeriphId = <block::Saradc as Block>::ID;
    const BASE: u32 = <block::Saradc as Block>::BASE;
    const SIZE: u32 = BLOCK_SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.reset_regs(kind);
        self.sync_irq(cx.irq);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let mut wiring = self.store(off, size, val, cx.now, cx.ledger);
        let mut scheduled = false;
        let conversion_ps = cx.profile.adc_conversion_ps;
        if let Wiring::AdcSample { unit, channel } = wiring
            && conversion_ps > 0
        {
            scheduled = self.start_timed(unit, channel, cx.now, conversion_ps, cx.sched);
            wiring = Wiring::None;
        }
        self.sync_irq(cx.irq);
        RegWrite {
            stop: scheduled || !matches!(wiring, Wiring::None),
            wiring,
        }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        if tag == TAG_CONVERTED {
            self.finish_timed(cx.now)
        } else {
            Wiring::None
        }
    }

    fn stable_until(&self, off: u32, _cx: &Cx) -> Stability {
        self.stability(off)
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        regs::reg_at(&REGS, off).map_or(Fidelity::U, |(i, _)| REGS[i].class)
    }
}

#[cfg(test)]
mod tests {
    use pemu_core::reset::{ResetCause, ResetFanout, ResetScope};

    use super::*;

    const ONETIME_SAMPLE: u32 = 0x020;
    const DATA1: u32 = 0x02C;
    const DATA2: u32 = 0x030;
    const INT_ENA: u32 = 0x040;
    const INT_RAW: u32 = 0x044;
    const INT_ST: u32 = 0x048;
    const INT_CLR: u32 = 0x04C;

    const T: VTime = VTime(5_000);

    #[test]
    fn the_busy_wait_row_answers_the_hang_detector_with_a_stable_read() {
        let model = Model::default();
        assert!(matches!(
            model.stability(INT_RAW),
            Stability::UntilNextEvent
        ));
        assert!(matches!(model.stability(DATA1), Stability::UntilInput));
        assert!(matches!(model.stability(INT_CLR), Stability::Never));
        assert!(
            matches!(model.stability(0x0F0), Stability::Never),
            "an offset with no register row",
        );
    }

    /// `adc_oneshot_hal_setup` plus `adc_oneshot_hal_convert` up to the start write: 12 dB,
    /// the unit enabled, start low then high.
    fn convert(model: &mut Model, ledger: &mut FidelityLedger, unit: u32, channel: u32) -> Wiring {
        let base = (3 << ONETIME_ATTEN_SHIFT) | ((unit << 3 | channel) << ONETIME_CHANNEL_SHIFT);
        let enable = if unit == 0 {
            ADC1_ONETIME_SAMPLE
        } else {
            ADC2_ONETIME_SAMPLE
        };
        model.store(INT_CLR, Size::B4, INT_ADC1_DONE, T, ledger);
        model.store(ONETIME_SAMPLE, Size::B4, base | enable, T, ledger);
        model.store(
            ONETIME_SAMPLE,
            Size::B4,
            base | enable | ONETIME_START,
            T,
            ledger,
        )
    }

    #[test]
    fn the_model_is_the_saradc_row_of_the_table() {
        assert_eq!(<Model as Peripheral>::ID, super::super::id::SARADC);
        assert_eq!(<Model as Peripheral>::BASE, 0x6004_0000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x1000);
        let model = Model::default();
        assert_eq!(model.fidelity(INT_RAW), Fidelity::B);
        assert_eq!(model.fidelity(DATA1), Fidelity::B);
        // The synthesized eFuse: calibration version 1, every field 0.
        assert_eq!(model.calibration(), AdcCal::default());
        assert!(model.calibration().supports_curve_fitting());
        assert_eq!(model.calibration().digi_atten3(), 2_000);
    }

    #[test]
    fn a_rising_start_edge_asks_for_a_sample_and_latching_it_sets_the_done_bit() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let wiring = convert(&mut model, &mut ledger, 0, 0);
        assert!(
            matches!(
                wiring,
                Wiring::AdcSample {
                    unit: 1,
                    channel: 0
                }
            ),
            "the ladder is ADC1 channel 0",
        );
        assert_eq!(
            model.load(INT_RAW, Size::B4, T, &mut ledger) & INT_ADC1_DONE,
            0,
            "nothing is latched until the board is sampled",
        );

        // 300 mV is the nominal DOWN rung of the ladder.
        model.latch(
            Conversion {
                unit: AdcUnit::One,
                channel: 0,
                atten: Atten::Db12,
            },
            300,
        );
        assert_eq!(model.data(AdcUnit::One), 433);
        assert_eq!(
            model.load(DATA1, Size::B4, T, &mut ledger) & DATA_MASK,
            433,
            "the driver reads 1_DATA_STATUS & 0xFFF",
        );
        assert_eq!(
            model.load(INT_RAW, Size::B4, T, &mut ledger) & INT_ADC1_DONE,
            INT_ADC1_DONE
        );
        assert!(model.done(AdcUnit::One));
    }

    /// A second start while one runs schedules nothing, and a reset in between leaves the stale
    /// event nothing to do, even when a later conversion is running by then.
    #[test]
    fn a_timed_start_ends_at_its_event_and_a_reset_cancels_it() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut sched = Scheduler::new();
        let Wiring::AdcSample { unit, channel } = convert(&mut model, &mut ledger, 0, 0) else {
            panic!("a rising start edge asks for a sample");
        };
        assert!(model.start_timed(unit, channel, T, 26_000_000, &mut sched));
        assert!(!model.start_timed(unit, channel, T, 26_000_000, &mut sched));
        assert!(matches!(model.stability(DATA1), Stability::UntilNextEvent));
        assert_eq!(
            model.load(INT_RAW, Size::B4, T, &mut ledger) & INT_ADC1_DONE,
            0
        );
        assert_eq!(sched.pop_due(VTime(T.0 + 25_999_999)), None);
        let key = sched
            .pop_due(VTime(T.0 + 26_000_000))
            .expect("the end is due");
        assert_eq!(key.owner, Owner::Periph(<Model as Peripheral>::ID));
        assert_eq!(sched.pop_due(VTime(u64::MAX)), None, "one event, not two");
        let end = VTime(T.0 + 26_000_000);
        assert!(matches!(
            model.finish_timed(end),
            Wiring::AdcSample {
                unit: 1,
                channel: 0
            }
        ));
        assert!(matches!(model.stability(DATA1), Stability::UntilInput));
        assert!(matches!(model.finish_timed(end), Wiring::None));

        // The stale event fires 1 us before the later conversion's end and must not end it.
        assert!(model.start_timed(unit, channel, T, 26_000_000, &mut sched));
        model.reset_regs(ResetKind {
            cause: ResetCause(0x01),
            scope: ResetScope::Chip,
            fanout: ResetFanout::AllBlocks,
        });
        assert!(matches!(model.finish_timed(end), Wiring::None));
        let later = VTime(T.0 + 1_000_000);
        assert!(model.start_timed(unit, channel, later, 26_000_000, &mut sched));
        assert!(matches!(model.finish_timed(end), Wiring::None));
        assert!(matches!(
            model.finish_timed(VTime(later.0 + 26_000_000)),
            Wiring::AdcSample { .. }
        ));
    }

    /// The nominal rungs (0, 300 and 595 mV) and the board's rungs (3, 274 and 540 mV, the
    /// device's codes 3, 394 and 782 with Down one below, `boards/ai-passport.toml`).
    #[test]
    fn the_ladder_voltages_convert_to_the_button_raw_codes() {
        let mut model = Model::default();
        let conversion = Conversion {
            unit: AdcUnit::One,
            channel: 0,
            atten: Atten::Db12,
        };
        for (mv, code) in [
            (0, 0),
            (75, 107),
            (300, 433),
            (595, 862),
            (3_300, 4_095),
            (3, 3),
            (274, 393),
            (540, 782),
        ] {
            model.latch(conversion, mv);
            assert_eq!(model.data(AdcUnit::One), code, "{mv} mV");
        }
    }

    /// Nothing on this board is wired to ADC2.
    #[test]
    fn an_adc2_conversion_latches_zero_and_its_own_done_bit() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let wiring = convert(&mut model, &mut ledger, 1, 0);
        assert!(matches!(
            wiring,
            Wiring::AdcSample {
                unit: 2,
                channel: 0
            }
        ));
        model.latch(
            Conversion {
                unit: AdcUnit::Two,
                channel: 0,
                atten: Atten::Db12,
            },
            3_300,
        );
        assert_eq!(model.data(AdcUnit::Two), 0);
        assert_eq!(model.load(DATA2, Size::B4, T, &mut ledger), 0);
        assert!(model.done(AdcUnit::Two));
        assert!(!model.done(AdcUnit::One));
    }

    #[test]
    fn a_start_edge_with_no_unit_enabled_converts_nothing() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let wiring = model.store(ONETIME_SAMPLE, Size::B4, ONETIME_START, T, &mut ledger);
        assert!(matches!(wiring, Wiring::None));
        assert_eq!(model.load(INT_RAW, Size::B4, T, &mut ledger), 0);
    }

    #[test]
    fn only_the_rising_edge_of_onetime_start_asks_for_a_sample() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let base = (3 << ONETIME_ATTEN_SHIFT) | ADC1_ONETIME_SAMPLE;
        assert!(matches!(
            model.store(
                ONETIME_SAMPLE,
                Size::B4,
                base | ONETIME_START,
                T,
                &mut ledger
            ),
            Wiring::AdcSample { .. }
        ));
        assert!(matches!(
            model.store(
                ONETIME_SAMPLE,
                Size::B4,
                base | ONETIME_START,
                T,
                &mut ledger
            ),
            Wiring::None
        ));
        model.store(ONETIME_SAMPLE, Size::B4, base, T, &mut ledger);
        assert!(matches!(
            model.store(
                ONETIME_SAMPLE,
                Size::B4,
                base | ONETIME_START,
                T,
                &mut ledger
            ),
            Wiring::AdcSample { .. }
        ));
    }

    #[test]
    fn int_st_drives_source_43() {
        use crate::intc::testing::TestPorts;
        let mut p = TestPorts::new();
        let mut model = Model::default();
        let done = Conversion {
            unit: AdcUnit::One,
            channel: 0,
            atten: Atten::Db12,
        };
        p.with(|cx| Peripheral::write(&mut model, INT_ENA, Size::B4, INT_ADC1_DONE, cx));
        assert!(!p.source(IRQ_SOURCE), "nothing latched yet");
        model.latch(done, 300);
        model.sync_irq(&mut p.irq);
        assert!(p.source(IRQ_SOURCE), "the done bit asserts source 43");
        p.with(|cx| Peripheral::write(&mut model, INT_CLR, Size::B4, INT_ADC1_DONE, cx));
        assert!(!p.source(IRQ_SOURCE), "INT_CLR lowers it");

        model.latch(done, 300);
        model.sync_irq(&mut p.irq);
        assert!(p.source(IRQ_SOURCE));
        let kind = ResetKind::of(ResetCause::RTC_SW_SYS).expect("a documented reset cause");
        p.with(|cx| Peripheral::reset(&mut model, kind, cx));
        assert!(!p.source(IRQ_SOURCE), "a system reset lowers it");
    }

    #[test]
    fn int_status_masks_int_raw_and_int_clr_clears_the_done_bit() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        convert(&mut model, &mut ledger, 0, 0);
        model.latch(
            Conversion {
                unit: AdcUnit::One,
                channel: 0,
                atten: Atten::Db12,
            },
            300,
        );
        assert_eq!(model.load(INT_ST, Size::B4, T, &mut ledger), 0, "no ENA");
        model.store(INT_ENA, Size::B4, INT_ADC1_DONE, T, &mut ledger);
        assert_eq!(model.load(INT_ST, Size::B4, T, &mut ledger), INT_ADC1_DONE);

        model.store(INT_CLR, Size::B4, INT_ADC1_DONE, T, &mut ledger);
        assert!(!model.done(AdcUnit::One));
        assert_eq!(model.load(INT_ST, Size::B4, T, &mut ledger), 0);
        assert_eq!(
            model.load(INT_CLR, Size::B4, T, &mut ledger),
            0,
            "the clear register is write-only and reads 0",
        );
        assert_eq!(
            model.data(AdcUnit::One),
            433,
            "clearing the event keeps the code the conversion latched",
        );
    }

    /// Calibration field 0x020 gives digi 2032 and field 0x220 gives digi 1968.
    #[test]
    fn the_calibration_fields_move_the_codes_as_measured() {
        let mut model = Model::default();
        let conversion = Conversion {
            unit: AdcUnit::One,
            channel: 0,
            atten: Atten::Db12,
        };
        for (field, digi, at300, at595) in [(0x020u16, 2_032, 440, 876), (0x220, 1_968, 426, 848)] {
            model.set_calibration(AdcCal {
                blk_version_major: 1,
                cal_vol_atten3: field,
            });
            assert_eq!(model.calibration().digi_atten3(), digi);
            model.latch(conversion, 300);
            assert_eq!(
                model.data(AdcUnit::One),
                at300,
                "field {field:#x} at 300 mV"
            );
            model.latch(conversion, 595);
            assert_eq!(
                model.data(AdcUnit::One),
                at595,
                "field {field:#x} at 595 mV"
            );
        }
    }

    #[test]
    fn a_reset_restores_the_registers_and_keeps_the_calibration() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let cal = AdcCal {
            blk_version_major: 1,
            cal_vol_atten3: 0x020,
        };
        model.set_calibration(cal);
        convert(&mut model, &mut ledger, 0, 0);
        model.latch(
            Conversion {
                unit: AdcUnit::One,
                channel: 0,
                atten: Atten::Db12,
            },
            300,
        );

        model.reset_regs(ResetKind {
            cause: ResetCause(0x0C),
            scope: ResetScope::Core,
            fanout: ResetFanout::CpuAndPms,
        });
        assert!(model.done(AdcUnit::One), "a CPU0_ reset reaches no block");

        model.reset_regs(ResetKind {
            cause: ResetCause(0x03),
            scope: ResetScope::Core,
            fanout: ResetFanout::AllBlocks,
        });
        assert!(!model.done(AdcUnit::One));
        assert_eq!(model.data(AdcUnit::One), 0);
        assert_eq!(
            model.reg(ONETIME_SAMPLE),
            0x1A00_0000,
            "ONETIME_SAMPLE goes back to its reset value (regs_saradc)",
        );
        assert_eq!(model.calibration(), cal);
    }

    #[test]
    fn the_register_values_round_trip_without_the_static_table() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        model.store(INT_ENA, Size::B4, INT_ADC1_DONE, T, &mut ledger);
        let saved = regs::store_values(&model.regs);
        assert_eq!(saved.len(), REG_COUNT);
        assert_eq!(regs::store_values(&regs::store_from(&REGS, &saved)), saved);
        let short = regs::store_from(&REGS, &saved[..2]);
        assert_eq!(
            short.get(idx::APB_SARADC_ONETIME_SAMPLE),
            REGS[idx::APB_SARADC_ONETIME_SAMPLE].reset
        );
    }
}
