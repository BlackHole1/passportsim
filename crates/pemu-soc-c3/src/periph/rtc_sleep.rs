//! Sleep, wakeup and brownout of RTC_CNTL (`specs/blocks/rtc_cntl.toml`). The registers sit in the
//! `rtc_cntl` window, so the `rtc_cntl` model stores them and hands the writes that start
//! something to the functions here.
//!
//! Modeled wake sources: the sleep timer and RTC GPIO pads armed on a level; every other enabled
//! trigger is [`WakePlan::unmodeled`]. Sleep rejection is never raised.
//!
//! The brownout threshold is [`BROWNOUT_LVL7_MV`], 2.51 V (IDF
//! `esp_hw_support/power_supply/port/esp32c3/Kconfig.power:24-25`, class B). The supply the
//! detector sees is [`chip_supply_mv`], the board's 3.3 V regulator less its dropout on battery
//! (class C, UNVERIFIED). With those numbers the board's 3000 mV rail cutoff always acts first on
//! battery.
//!
//! What sleep does to the hart, clocks, SRAM and the USB link is `pemu-machine`'s `sleep.rs`.

use pemu_core::time::VTime;

use super::Wiring;
use super::rtc_cntl::Model;
use crate::r#gen::regs_rtc_cntl::idx;

/// Kind of sleep entered through `Wiring::SleepEnter`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SleepKind {
    Light,
    Deep,
}

/// `STATE0.SLEEP_EN`: the write that starts sleep.
pub const STATE0_SLEEP_EN: u32 = 1 << 31;

/// `STATE0.SLP_WAKEUP` b29: set by a wake. Class A: the `sleep_lat` capture reads `STATE0` as
/// 0x20000000 after a light-sleep timer wake and at `app_main` after a deep-sleep wake.
pub const STATE0_SLP_WAKEUP: u32 = 1 << 29;

/// `DIG_PWC.DG_WRAP_PD_EN`: set for `RTC_SLEEP_PD_DIG`, which makes a sleep deep
/// (`rtc_sleep.c:76, 213, 221`).
pub const DIG_PWC_DG_WRAP_PD_EN: u32 = 1 << 31;

/// `SLP_TIMER1.MAIN_TIMER_ALARM_EN`: arms the sleep timer. Write-only, so it reads 0, but the
/// register table stores it, which is where the arming is kept.
pub const SLP_TIMER1_ALARM_EN: u32 = 1 << 16;

/// `SLP_TIMER1.SLP_VAL_HI`, the top 16 bits of the 48-bit alarm.
const SLP_VAL_HI_MASK: u32 = 0xFFFF;

const TICKS_MASK: u64 = 0xFFFF_FFFF_FFFF;

/// `WAKEUP_STATE.WAKEUP_ENA` starts at bit 15; trigger `n` is field bit `n`.
const WAKEUP_ENA_SHIFT: u32 = 15;

/// Wake trigger bits of `WAKEUP_ENA` and `SLP_WAKEUP_CAUSE` (IDF `rtc.h:664-672`).
pub mod trig {
    pub const GPIO: u32 = 1 << 2;
    /// `RTC_TIMER_TRIG_EN`: the sleep timer.
    pub const TIMER: u32 = 1 << 3;
    pub const UART0: u32 = 1 << 6;
    pub const BT: u32 = 1 << 10;
    pub const USB: u32 = 1 << 14;
    pub const BROWNOUT: u32 = 1 << 16;
}

pub const INT_SLP_WAKEUP: u32 = 1 << 0;

pub const INT_BROWN_OUT: u32 = 1 << 9;

/// `INT_RAW.MAIN_TIMER`: the sleep timer reached `SLP_VAL`.
pub const INT_MAIN_TIMER: u32 = 1 << 10;

/// The kind of sleep a `SLEEP_EN` write enters, given `DIG_PWC`.
pub const fn entry_kind(dig_pwc: u32) -> SleepKind {
    if dig_pwc & DIG_PWC_DG_WRAP_PD_EN != 0 {
        SleepKind::Deep
    } else {
        SleepKind::Light
    }
}

/// The effect of a `STATE0` write (already stored; `before` is the old value): a sleep entry on
/// the 0 to 1 edge of `SLEEP_EN`, nothing otherwise. The wake latch clears the bit, so the next
/// sleep is an edge again.
pub fn on_state0_write(model: &Model, before: u32) -> Wiring {
    let regs = model.regs();
    if regs.get(idx::RTC_CNTL_STATE0) & STATE0_SLEEP_EN == 0 || before & STATE0_SLEEP_EN != 0 {
        return Wiring::None;
    }
    Wiring::SleepEnter(entry_kind(regs.get(idx::RTC_CNTL_DIG_PWC)))
}

/// Whether the chip is asleep: `SLEEP_EN` is set and no wake has cleared it yet.
pub fn sleeping(model: &Model) -> Option<SleepKind> {
    let regs = model.regs();
    (regs.get(idx::RTC_CNTL_STATE0) & STATE0_SLEEP_EN != 0)
        .then(|| entry_kind(regs.get(idx::RTC_CNTL_DIG_PWC)))
}

/// The enabled wake triggers, as `WAKEUP_ENA` field bits ([`trig`]).
pub fn wakeup_ena(model: &Model) -> u32 {
    model.regs().get(idx::RTC_CNTL_WAKEUP_STATE) >> WAKEUP_ENA_SHIFT
}

/// The 48-bit `SLP_VAL` alarm in RTC ticks, when the timer is armed.
pub fn timer_target(model: &Model) -> Option<u64> {
    let regs = model.regs();
    let hi = regs.get(idx::RTC_CNTL_SLP_TIMER1);
    if hi & SLP_TIMER1_ALARM_EN == 0 {
        return None;
    }
    let lo = regs.get(idx::RTC_CNTL_SLP_TIMER0);
    Some(((u64::from(hi & SLP_VAL_HI_MASK) << 32) | u64::from(lo)) & TICKS_MASK)
}

/// RTC GPIO pads that can wake the chip on the C3: GPIO0 to GPIO5
/// (IDF `soc/esp32c3/register/soc/rtc_cntl_reg.h:2428`).
pub const RTC_GPIO_PINS: u8 = 6;

/// `GPIO_WAKEUP.GPIO_PIN0_WAKEUP_ENABLE` is b31; pad `n` is `1 << (31 - n)`.
const GPIO_WAKEUP_ENABLE_PIN0: u32 = 31;

/// `GPIO_WAKEUP.GPIO_PIN0_INT_TYPE` is b25:23; pad `n` is at `23 - 3n`.
const GPIO_INT_TYPE_PIN0: u32 = 23;
const GPIO_INT_TYPE_BITS: u32 = 3;

/// `GPIO_WAKEUP.GPIO_WAKEUP_STATUS` b5:0: one bit per pad, read-only, set by the pad that woke
/// the chip.
const GPIO_WAKEUP_STATUS_MASK: u32 = 0x3F;

/// `GPIO_WAKEUP.GPIO_WAKEUP_STATUS_CLR` b6: while written 1, the status field clears. Whether
/// silicon clears on the edge or holds the field at 0 is UNVERIFIED (class C); both agree for
/// IDF's set-then-clear around arming.
pub const GPIO_WAKEUP_STATUS_CLR: u32 = 1 << 6;

/// `GPIO_PIN<n>_INT_TYPE` values (IDF `hal/include/hal/gpio_types.h`). Only the two level
/// triggers can end a sleep: the pad is sampled, not edge-detected, while the digital domain is
/// down.
pub mod int_type {
    pub const LOW_LEVEL: u32 = 4;
    pub const HIGH_LEVEL: u32 = 5;
}

/// The level RTC GPIO pad `pin` wakes on: `false` for `LOW_LEVEL`, `true` for `HIGH_LEVEL`, `None`
/// when the pad is not an enabled RTC pad with a level trigger.
pub fn gpio_wake_level(model: &Model, pin: u8) -> Option<bool> {
    if pin >= RTC_GPIO_PINS {
        return None;
    }
    let reg = model.regs().get(idx::RTC_CNTL_GPIO_WAKEUP);
    if reg & (1 << (GPIO_WAKEUP_ENABLE_PIN0 - u32::from(pin))) == 0 {
        return None;
    }
    let shift = GPIO_INT_TYPE_PIN0 - GPIO_INT_TYPE_BITS * u32::from(pin);
    match (reg >> shift) & ((1 << GPIO_INT_TYPE_BITS) - 1) {
        int_type::LOW_LEVEL => Some(false),
        int_type::HIGH_LEVEL => Some(true),
        _ => None,
    }
}

/// The RTC GPIO pads armed on a level, as a bit per pad.
pub fn gpio_wake_pads(model: &Model) -> u32 {
    (0..RTC_GPIO_PINS)
        .filter(|pin| gpio_wake_level(model, *pin).is_some())
        .fold(0, |acc, pin| acc | 1 << pin)
}

/// Records in the read-only `GPIO_WAKEUP_STATUS` which pads woke the chip (a bit per pad).
pub fn latch_gpio_status(model: &mut Model, pads: u32) {
    let reg = model.regs().get(idx::RTC_CNTL_GPIO_WAKEUP);
    let reg = (reg & !GPIO_WAKEUP_STATUS_MASK) | (pads & GPIO_WAKEUP_STATUS_MASK);
    model.hw_set(idx::RTC_CNTL_GPIO_WAKEUP, reg);
}

/// The effect of a `GPIO_WAKEUP` write: `GPIO_WAKEUP_STATUS_CLR` clears the status field.
pub fn on_gpio_wakeup_write(model: &mut Model) -> Wiring {
    let reg = model.regs().get(idx::RTC_CNTL_GPIO_WAKEUP);
    if reg & GPIO_WAKEUP_STATUS_CLR != 0 {
        model.hw_set(idx::RTC_CNTL_GPIO_WAKEUP, reg & !GPIO_WAKEUP_STATUS_MASK);
    }
    Wiring::None
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct WakePlan {
    /// The instant the timer wake source fires, when it is enabled and armed.
    pub timer_at: Option<VTime>,
    /// RTC GPIO pads armed on a level while [`trig::GPIO`] is enabled, a bit per pad. Which of
    /// them a board drives is the machine's business.
    pub gpio_pads: u32,
    /// Enabled triggers ([`trig`]) the model does not wake on.
    pub unmodeled: u32,
}

/// The wake plan of a sleep entered at `now`.
///
/// An alarm already reached fires at `now`: IDF computes `SLP_VAL` as counter plus sleep time,
/// so a target behind the counter means the entry itself took longer. A sleep with no modeled
/// source sleeps until a power cycle, as the hardware does.
pub fn wake_plan(model: &Model, now: VTime) -> WakePlan {
    let ena = wakeup_ena(model);
    let timer_at = if ena & trig::TIMER != 0 {
        timer_target(model).map(|target| timer_instant(model, now, target))
    } else {
        None
    };
    let gpio_pads = if ena & trig::GPIO != 0 {
        gpio_wake_pads(model)
    } else {
        0
    };
    // The GPIO trigger is modeled only once a pad is armed on a level.
    let gpio_unmodeled = if ena & trig::GPIO != 0 && gpio_pads == 0 {
        trig::GPIO
    } else {
        0
    };
    WakePlan {
        timer_at,
        gpio_pads,
        unmodeled: (ena & !(trig::TIMER | trig::GPIO)) | gpio_unmodeled,
    }
}

/// The first instant at or after `now` at which the RTC counter reads at least `target`.
///
/// `now + ps_of(target - current)` can be up to one tick late because `now` sits inside its
/// tick; bisecting that last tick finds the first instant exactly, so the wake does not depend
/// on where inside a tick the sleep was entered.
fn timer_instant(model: &Model, now: VTime, target: u64) -> VTime {
    let current = model.rtc_ticks(now);
    if target <= current {
        return now;
    }
    let late = now.0.saturating_add(model.ps_of_ticks(target - current));
    let (mut lo, mut hi) = (
        late.saturating_sub(model.ps_of_ticks(1) + 1).max(now.0),
        late,
    );
    if model.rtc_ticks(VTime(lo)) >= target {
        return VTime(lo);
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if model.rtc_ticks(VTime(mid)) >= target {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    VTime(hi)
}

/// Whether the timer of a sleep in progress has fired at `now`. The registers decide, so a stale
/// machine event from an earlier sleep wakes nothing.
pub fn timer_due(model: &Model, now: VTime) -> bool {
    wakeup_ena(model) & trig::TIMER != 0
        && timer_target(model).is_some_and(|target| model.rtc_ticks(now) >= target)
}

/// Latches a wake through `cause` ([`trig`]): `SLP_WAKEUP_CAUSE` holds the trigger bit,
/// `INT_RAW.SLP_WAKEUP` (and `MAIN_TIMER` for the timer) rises, `SLEEP_EN` clears and
/// `STATE0.SLP_WAKEUP` sets. The caller then drives source 27 from `Model::irq_level`.
pub fn latch_wake(model: &mut Model, cause: u32) {
    model.hw_set(idx::RTC_CNTL_SLP_WAKEUP_CAUSE, cause & 0x1_FFFF);
    let state0 = (model.regs().get(idx::RTC_CNTL_STATE0) & !STATE0_SLEEP_EN) | STATE0_SLP_WAKEUP;
    model.hw_set(idx::RTC_CNTL_STATE0, state0);
    let mut raw = INT_SLP_WAKEUP;
    if cause & trig::TIMER != 0 {
        raw |= INT_MAIN_TIMER;
    }
    model.raise_int(raw);
}

/// `BROWN_OUT.ENA`: the detector runs (set in the reset value 0x43FF0010).
pub const BROWN_OUT_ENA: u32 = 1 << 30;

/// `BROWN_OUT.DET`, read only: the supply is below the threshold.
pub const BROWN_OUT_DET: u32 = 1 << 31;

/// The board's regulated chip supply in millivolts (UNVERIFIED, class C).
pub const CHIP_SUPPLY_MV: u32 = 3_300;

/// The regulator's dropout with the cell as its input (UNVERIFIED, class C).
pub const REGULATOR_DROPOUT_MV: u32 = 250;

/// The level 7 detector threshold, 2.51 V (class B).
pub const BROWNOUT_LVL7_MV: u32 = 2_510;

/// The chip supply for a cell at `cell_mv`: 3.3 V with USB 5 V present, otherwise the regulator
/// output, at most its input less the dropout.
pub const fn chip_supply_mv(cell_mv: u32, usb_5v: bool) -> u32 {
    if usb_5v {
        return CHIP_SUPPLY_MV;
    }
    let from_cell = cell_mv.saturating_sub(REGULATOR_DROPOUT_MV);
    if from_cell < CHIP_SUPPLY_MV {
        from_cell
    } else {
        CHIP_SUPPLY_MV
    }
}

/// Runs the brownout detector against `supply_mv`: with `ENA` set and the supply below the
/// threshold, `DET` and `INT_RAW.BROWN_OUT` are set, every time it runs; otherwise `DET` clears.
/// The machine runs it after every board input and chip reset, so firmware that restarts below
/// the threshold sees the brownout again. The reset controls are stored only: IDF sets
/// `reset_enabled=false` and restarts from its own ISR. The caller then drives source 27.
pub fn detect_brownout(model: &mut Model, supply_mv: u32) {
    let reg = model.regs().get(idx::RTC_CNTL_BROWN_OUT);
    let below = reg & BROWN_OUT_ENA != 0 && supply_mv < BROWNOUT_LVL7_MV;
    if below {
        model.hw_set(idx::RTC_CNTL_BROWN_OUT, reg | BROWN_OUT_DET);
        model.raise_int(INT_BROWN_OUT);
    } else if reg & BROWN_OUT_DET != 0 {
        model.hw_set(idx::RTC_CNTL_BROWN_OUT, reg & !BROWN_OUT_DET);
    }
}

/// The level half of the detector: with `DET` still set, `INT_RAW.BROWN_OUT` rises again, so an
/// `INT_CLR` cannot clear a brownout that is still there.
pub fn reassert_brownout(model: &mut Model) {
    if model.regs().get(idx::RTC_CNTL_BROWN_OUT) & BROWN_OUT_DET != 0 {
        model.raise_int(INT_BROWN_OUT);
    }
}

/// Ends a sleep without a wake latch: `SLEEP_EN` cleared, no cause, no raw bit. A reset that
/// comes from the RTC domain while the chip sleeps (the RWDT) leaves the sleep this way.
pub fn abort_sleep(model: &mut Model) {
    let state0 = model.regs().get(idx::RTC_CNTL_STATE0) & !STATE0_SLEEP_EN;
    model.hw_set(idx::RTC_CNTL_STATE0, state0);
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;
    use pemu_core::sched::Scheduler;

    use crate::periph::rtc_cntl::SLOW_HZ;

    const OFF_SLP_TIMER0: u32 = 0x004;
    const OFF_SLP_TIMER1: u32 = 0x008;
    const OFF_STATE0: u32 = 0x018;
    const OFF_WAKEUP_STATE: u32 = 0x03C;
    const OFF_INT_RAW: u32 = 0x044;
    const OFF_DIG_PWC: u32 = 0x088;
    const OFF_SLP_WAKEUP_CAUSE: u32 = 0x0F8;
    const OFF_INT_ENA: u32 = 0x040;
    const OFF_INT_CLR: u32 = 0x04C;
    const OFF_BROWN_OUT: u32 = 0x0D8;
    const OFF_GPIO_WAKEUP: u32 = 0x110;

    /// `STATE0.SLP_REJECT_CAUSE_CLR`, the write IDF makes before `SLEEP_EN`.
    const REJECT_CAUSE_CLR: u32 = 1 << 1;

    struct Harness {
        m: Model,
        l: FidelityLedger,
        s: Scheduler,
        now: VTime,
    }

    impl Harness {
        fn new() -> Harness {
            Harness {
                m: Model::default(),
                l: FidelityLedger::default(),
                s: Scheduler::new(),
                now: VTime(0),
            }
        }

        fn write(&mut self, off: u32, val: u32) -> Wiring {
            self.m
                .store(off, Size::B4, val, self.now, &mut self.s, &mut self.l)
        }

        fn read(&mut self, off: u32) -> u32 {
            self.m.load(off, Size::B4, self.now, &mut self.l)
        }

        /// The IDF timer arming: `SLP_VAL` and the alarm enable.
        fn arm_timer(&mut self, target: u64) {
            self.write(OFF_SLP_TIMER0, target as u32);
            self.write(
                OFF_SLP_TIMER1,
                ((target >> 32) as u32 & SLP_VAL_HI_MASK) | SLP_TIMER1_ALARM_EN,
            );
        }
    }

    fn kind(w: &Wiring) -> Option<SleepKind> {
        match w {
            Wiring::SleepEnter(k) => Some(*k),
            _ => None,
        }
    }

    #[test]
    fn sleep_en_enters_light_or_deep_by_the_digital_wrapper_bit() {
        for (pwc, want) in [
            (0x0055_5010, SleepKind::Light),
            (0x0055_5010 | DIG_PWC_DG_WRAP_PD_EN, SleepKind::Deep),
        ] {
            let mut h = Harness::new();
            h.write(OFF_DIG_PWC, pwc);
            // IDF's order: clear the reject cause first, then set SLEEP_EN.
            assert_eq!(kind(&h.write(OFF_STATE0, REJECT_CAUSE_CLR)), None);
            assert_eq!(
                kind(&h.write(OFF_STATE0, STATE0_SLEEP_EN)),
                Some(want),
                "DIG_PWC {pwc:#x}"
            );
            assert_eq!(sleeping(&h.m), Some(want));
        }
    }

    #[test]
    fn a_write_while_sleep_en_is_already_set_does_not_enter_sleep() {
        let mut h = Harness::new();
        assert_eq!(
            kind(&h.write(OFF_STATE0, STATE0_SLEEP_EN)),
            Some(SleepKind::Light)
        );
        assert_eq!(kind(&h.write(OFF_STATE0, STATE0_SLEEP_EN)), None);
        assert_eq!(kind(&h.write(OFF_STATE0, STATE0_SLEEP_EN | 1 << 22)), None);
        assert_eq!(sleeping(&h.m), Some(SleepKind::Light));
        latch_wake(&mut h.m, trig::TIMER);
        assert_eq!(
            kind(&h.write(OFF_STATE0, STATE0_SLEEP_EN)),
            Some(SleepKind::Light)
        );
    }

    #[test]
    fn a_state0_write_without_sleep_en_enters_nothing() {
        let mut h = Harness::new();
        assert_eq!(kind(&h.write(OFF_STATE0, 1 << 22)), None);
        assert_eq!(sleeping(&h.m), None);
    }

    #[test]
    fn the_timer_is_armed_only_with_its_alarm_enable_and_reads_it_as_zero() {
        let mut h = Harness::new();
        h.write(OFF_SLP_TIMER0, 0x1234_5678);
        h.write(OFF_SLP_TIMER1, 0xABCD);
        assert_eq!(timer_target(&h.m), None, "no MAIN_TIMER_ALARM_EN");
        h.arm_timer(0xABCD_1234_5678);
        assert_eq!(timer_target(&h.m), Some(0xABCD_1234_5678));
        assert_eq!(h.read(OFF_SLP_TIMER1), 0xABCD);
    }

    #[test]
    fn the_timer_wake_is_never_one_tick_early() {
        let mut h = Harness::new();
        h.write(OFF_WAKEUP_STATE, trig::TIMER << WAKEUP_ENA_SHIFT);
        h.now = VTime(123_456_789_011);
        let now_ticks = h.m.rtc_ticks(h.now);
        let target = now_ticks + 5 * SLOW_HZ;
        h.arm_timer(target);
        let plan = wake_plan(&h.m, h.now);
        assert_eq!(plan.unmodeled, 0);
        let at = plan.timer_at.expect("the timer is enabled and armed");
        assert!(h.m.rtc_ticks(at) >= target);
        assert!(
            h.m.rtc_ticks(VTime(at.0 - 1)) < target,
            "the first such instant"
        );
        assert!(timer_due(&h.m, at) && !timer_due(&h.m, VTime(at.0 - 1)));
        let slept = at.0 - h.now.0;
        assert!(slept.abs_diff(5_000_000_000_000) < 1_000_000_000_000 / SLOW_HZ + 1);
    }

    #[test]
    fn an_alarm_behind_the_counter_fires_at_once() {
        let mut h = Harness::new();
        h.write(OFF_WAKEUP_STATE, trig::TIMER << WAKEUP_ENA_SHIFT);
        h.now = VTime(2_000_000_000_000);
        h.arm_timer(10);
        assert_eq!(wake_plan(&h.m, h.now).timer_at, Some(h.now));
    }

    #[test]
    fn a_sleep_without_a_modeled_source_has_no_wake_instant() {
        let mut h = Harness::new();
        h.arm_timer(1_000);
        // The reset value enables GPIO and TIMER; leave GPIO and UART0 only.
        h.write(
            OFF_WAKEUP_STATE,
            (trig::GPIO | trig::UART0) << WAKEUP_ENA_SHIFT,
        );
        let plan = wake_plan(&h.m, h.now);
        assert_eq!(plan.timer_at, None, "the timer trigger is disabled");
        assert_eq!(plan.unmodeled, trig::GPIO | trig::UART0);
    }

    /// A decode that got either stride wrong would read a neighbouring pad's field.
    #[test]
    fn the_gpio_wake_decode_reads_each_pads_enable_and_level() {
        for pin in 0..RTC_GPIO_PINS {
            for (int_type, want) in [
                (int_type::LOW_LEVEL, Some(false)),
                (int_type::HIGH_LEVEL, Some(true)),
                (0, None),
                (1, None),
                (3, None),
            ] {
                let mut h = Harness::new();
                let enable = 1 << (GPIO_WAKEUP_ENABLE_PIN0 - u32::from(pin));
                let shift = GPIO_INT_TYPE_PIN0 - GPIO_INT_TYPE_BITS * u32::from(pin);
                h.write(OFF_GPIO_WAKEUP, enable | (int_type << shift));
                assert_eq!(
                    gpio_wake_level(&h.m, pin),
                    want,
                    "pad {pin}, type {int_type}"
                );
                for other in (0..RTC_GPIO_PINS).filter(|p| *p != pin) {
                    assert_eq!(
                        gpio_wake_level(&h.m, other),
                        None,
                        "pad {other} is not armed"
                    );
                }
                assert_eq!(
                    gpio_wake_pads(&h.m),
                    if want.is_some() { 1 << pin } else { 0 }
                );
            }
        }
        let h = Harness::new();
        assert_eq!(gpio_wake_level(&h.m, RTC_GPIO_PINS), None, "not an RTC pad");
    }

    #[test]
    fn an_armed_pad_makes_the_gpio_trigger_a_modeled_source() {
        let mut h = Harness::new();
        h.write(OFF_WAKEUP_STATE, trig::GPIO << WAKEUP_ENA_SHIFT);
        assert_eq!(wake_plan(&h.m, h.now).gpio_pads, 0);
        assert_eq!(wake_plan(&h.m, h.now).unmodeled, trig::GPIO);
        h.write(
            OFF_GPIO_WAKEUP,
            (1 << GPIO_WAKEUP_ENABLE_PIN0) | (int_type::LOW_LEVEL << GPIO_INT_TYPE_PIN0),
        );
        let plan = wake_plan(&h.m, h.now);
        assert_eq!(plan.gpio_pads, 1);
        assert_eq!(plan.unmodeled, 0);
        // Armed but not enabled in `WAKEUP_ENA`: no pad and no unmodeled trigger.
        h.write(OFF_WAKEUP_STATE, 0);
        let plan = wake_plan(&h.m, h.now);
        assert_eq!((plan.gpio_pads, plan.unmodeled), (0, 0));
    }

    #[test]
    fn the_gpio_wake_status_latches_and_clears() {
        let mut h = Harness::new();
        let armed = (1 << GPIO_WAKEUP_ENABLE_PIN0) | (int_type::LOW_LEVEL << GPIO_INT_TYPE_PIN0);
        h.write(OFF_GPIO_WAKEUP, armed);
        assert_eq!(h.read(OFF_GPIO_WAKEUP) & GPIO_WAKEUP_STATUS_MASK, 0);
        h.write(OFF_GPIO_WAKEUP, armed | 0x2F);
        assert_eq!(
            h.read(OFF_GPIO_WAKEUP) & GPIO_WAKEUP_STATUS_MASK,
            0,
            "the guest cannot write the status"
        );
        latch_gpio_status(&mut h.m, 1);
        assert_eq!(h.read(OFF_GPIO_WAKEUP) & GPIO_WAKEUP_STATUS_MASK, 1);
        assert_eq!(
            gpio_wake_level(&h.m, 0),
            Some(false),
            "the latch left the arming alone"
        );
        h.write(OFF_GPIO_WAKEUP, armed | GPIO_WAKEUP_STATUS_CLR);
        assert_eq!(h.read(OFF_GPIO_WAKEUP) & GPIO_WAKEUP_STATUS_MASK, 0);
    }

    #[test]
    fn the_wake_latch_sets_the_cause_and_raw_bits_and_clears_sleep_en() {
        let mut h = Harness::new();
        h.write(OFF_STATE0, STATE0_SLEEP_EN | (1 << 22));
        latch_wake(&mut h.m, trig::TIMER);
        assert_eq!(h.read(OFF_SLP_WAKEUP_CAUSE), trig::TIMER);
        assert_eq!(
            h.read(OFF_STATE0),
            (1 << 22) | STATE0_SLP_WAKEUP,
            "SLEEP_EN cleared and SLP_WAKEUP set"
        );
        assert_eq!(
            h.read(OFF_INT_RAW) & (INT_SLP_WAKEUP | INT_MAIN_TIMER),
            INT_SLP_WAKEUP | INT_MAIN_TIMER
        );
        assert_eq!(sleeping(&h.m), None);
        // IDF polls INT_RAW with INT_ENA clear, so source 27 stays low.
        assert!(!h.m.irq_level());
    }

    /// Class A: the `sleep_lat` capture.
    #[test]
    fn state0_reads_0x20000000_after_a_timer_wake() {
        let mut h = Harness::new();
        h.write(OFF_STATE0, STATE0_SLEEP_EN);
        latch_wake(&mut h.m, trig::TIMER);
        assert_eq!(h.read(OFF_STATE0), 0x2000_0000);
    }

    #[test]
    fn the_wake_cause_survives_a_deep_sleep_reset() {
        use pemu_core::reset::{ResetCause, ResetKind};
        let mut h = Harness::new();
        latch_wake(&mut h.m, trig::TIMER);
        let kind = ResetKind::of(ResetCause::DEEPSLEEP).expect("cause 0x05 is documented");
        h.m.reset_to(kind, h.now, &mut h.s);
        assert_eq!(h.read(OFF_SLP_WAKEUP_CAUSE), trig::TIMER);
        assert_eq!(h.m.reset_cause(), ResetCause::DEEPSLEEP);
    }

    #[test]
    fn the_brownout_detector_is_a_level_that_an_int_clr_cannot_clear() {
        use pemu_core::reset::{ResetCause, ResetKind};
        let mut h = Harness::new();
        let kind = ResetKind::of(ResetCause::POWERON).expect("cause 0x01 is documented");
        h.m.reset_to(kind, h.now, &mut h.s);
        assert_eq!(h.read(OFF_BROWN_OUT), 0x43FF_0010, "reset value, ENA set");

        detect_brownout(&mut h.m, BROWNOUT_LVL7_MV);
        assert_eq!(
            h.read(OFF_BROWN_OUT) & BROWN_OUT_DET,
            0,
            "at the threshold DET reads 0"
        );
        assert_eq!(h.read(OFF_INT_RAW) & INT_BROWN_OUT, 0);

        detect_brownout(&mut h.m, BROWNOUT_LVL7_MV - 1);
        assert_ne!(h.read(OFF_BROWN_OUT) & BROWN_OUT_DET, 0);
        assert_ne!(h.read(OFF_INT_RAW) & INT_BROWN_OUT, 0);
        assert!(!h.m.irq_level(), "INT_ENA.BROWN_OUT is clear");
        h.write(OFF_INT_ENA, INT_BROWN_OUT);
        assert!(h.m.irq_level(), "enabled, source 27 is raised");

        h.write(OFF_INT_CLR, INT_BROWN_OUT);
        assert_ne!(
            h.read(OFF_INT_RAW) & INT_BROWN_OUT,
            0,
            "re-asserted after INT_CLR"
        );
        assert!(h.m.irq_level());

        h.write(OFF_BROWN_OUT, 0x43FF_0010);
        assert_ne!(h.read(OFF_BROWN_OUT) & BROWN_OUT_DET, 0);

        detect_brownout(&mut h.m, CHIP_SUPPLY_MV);
        assert_eq!(
            h.read(OFF_BROWN_OUT) & BROWN_OUT_DET,
            0,
            "above it DET reads 0 again"
        );
        assert_ne!(
            h.read(OFF_INT_RAW) & INT_BROWN_OUT,
            0,
            "the raw bit waits for its clear"
        );
        h.write(OFF_INT_CLR, INT_BROWN_OUT);
        assert_eq!(
            h.read(OFF_INT_RAW) & INT_BROWN_OUT,
            0,
            "and now the clear holds"
        );

        h.write(OFF_BROWN_OUT, 0x43FF_0010 & !BROWN_OUT_ENA);
        detect_brownout(&mut h.m, 0);
        assert_eq!(h.read(OFF_BROWN_OUT) & BROWN_OUT_DET, 0);
    }

    #[test]
    fn the_chip_supply_is_the_regulator_output_or_usb() {
        assert_eq!(chip_supply_mv(2_500, true), CHIP_SUPPLY_MV);
        assert_eq!(chip_supply_mv(4_200, false), CHIP_SUPPLY_MV);
        assert_eq!(chip_supply_mv(3_000, false), 3_000 - REGULATOR_DROPOUT_MV);
        assert!(chip_supply_mv(3_000, false) >= BROWNOUT_LVL7_MV);
        assert!(
            chip_supply_mv(BROWNOUT_LVL7_MV + REGULATOR_DROPOUT_MV - 1, false) < BROWNOUT_LVL7_MV
        );
    }
}
