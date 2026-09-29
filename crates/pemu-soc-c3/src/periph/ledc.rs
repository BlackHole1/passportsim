//! LEDC (0x60019000, interrupt source 23), the display backlight: channel 0 on GPIO21, low-speed
//! timer 0, 10-bit duty resolution at 5000 Hz (`specs/blocks/ledc.toml`).
//!
//! The divider arithmetic must be exact, not just stored: `ledc_get_freq` reads the duty
//! resolution and divider back and prints the frequency, so `80e6 * 256 / (4000 * 2^10)` must
//! come out as 5000 Hz; without the block it aborts.
//!
//! Channel and timer registers take effect only when their `para_up` bit is written 1; both
//! `para_up` bits read back 0 and are never polled. The integer duty the board sees is
//! `DUTY_R >> 4`; `bsp_display_backlight` computes `1023 * percent / 100`. No image installs a
//! fade ISR, but `INT_ST = RAW & ENA` and source 23 are still kept right.

use pemu_core::fidelity::{Fidelity, FidelityLedger};
use pemu_core::irq_source::irq;
use pemu_core::regstore::{RegSpec, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::time::VTime;

use crate::r#gen::regs_ledc::{BLOCK_SIZE, REG_COUNT, REGS, idx};

use super::reg_file::{RegFile, RegTable};
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring, block};

impl RegTable<REG_COUNT> for block::Ledc {
    const SPECS: &'static [RegSpec; REG_COUNT] = &REGS;
}

pub const CHANNELS: usize = 6;
pub const TIMERS: usize = 4;

/// `LSCHn_CONF0.timer_sel` (bits 1 to 0).
const CONF0_TIMER_SEL: u32 = 0x3;
/// `LSCHn_CONF0.sig_out_en` (bit 2): the channel drives its pin.
const CONF0_SIG_OUT_EN: u32 = 1 << 2;
/// `LSCHn_CONF0.idle_lv` (bit 3): the level the pin idles at when the channel is off.
const CONF0_IDLE_LV: u32 = 1 << 3;
/// `LSCHn_CONF0.para_up` (bit 4, WO): latch HPOINT, DUTY and CONF1; reads back 0.
const CONF0_PARA_UP: u32 = 1 << 4;
/// `LSCHn_CONF1.duty_start` (bit 31): begin the duty change.
const CONF1_DUTY_START: u32 = 1 << 31;
/// `LSTIMERn_CONF.duty_res` (bits 3 to 0).
const TIMER_DUTY_RES: u32 = 0xF;
/// `LSTIMERn_CONF.clk_div` (bits 21 to 4), 10.8 fixed point.
const TIMER_CLK_DIV_SHIFT: u32 = 4;
const TIMER_CLK_DIV: u32 = 0x3_FFFF;
/// `LSTIMERn_CONF.pause` (bit 22): freeze the counter and the output.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "register map; only the tests write it")
)]
const TIMER_PAUSE: u32 = 1 << 22;
const TIMER_RST: u32 = 1 << 23;
/// `LSTIMERn_CONF.para_up` (bit 25, WO): latch the timer configuration; reads back 0.
const TIMER_PARA_UP: u32 = 1 << 25;
/// `CONF.apb_clk_sel` (bits 1 to 0): 1 APB, 2 RC_FAST, 3 XTAL.
const CONF_CLK_SEL: u32 = 0x3;
/// Fractional bits of `LSCHn_DUTY` and `LSCHn_DUTY_R`.
const DUTY_FRACTION: u32 = 4;
/// Fractional bits of `LSTIMERn_CONF.clk_div`.
const DIV_FRACTION: u32 = 8;

/// APB clock, the source `LEDC_AUTO_CLK` picks first and the one the divider 4000 belongs to.
pub const APB_HZ: u32 = 80_000_000;
pub const XTAL_HZ: u32 = 40_000_000;
/// RC_FAST clock: the measured [`super::timg::RC_FAST_HZ`]. UNVERIFIED that LEDC sees it
/// undivided; no image selects it.
pub const RC_FAST_HZ: u32 = super::timg::RC_FAST_HZ as u32;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Step {
    /// Channels whose [`ChannelOut`] changed, one bit each: apply `Wiring::LedcChanged`. A mask,
    /// because a timer latch changes every channel bound to that timer.
    pub changed: u8,
    pub irq: Option<bool>,
    pub stop: bool,
}

impl Step {
    /// The channels [`Step::changed`] names, lowest first.
    pub fn changed_channels(self) -> impl Iterator<Item = u8> {
        let mask = self.changed;
        (0..CHANNELS as u8).filter(move |ch| mask & (1 << ch) != 0)
    }
}

/// The four arguments `BoardPorts::ledc` takes for one channel.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ChannelOut {
    pub channel: u8,
    /// Integer duty: `DUTY_R >> 4` while the channel drives its pin, else the idle level scaled
    /// to full or zero.
    pub duty: u32,
    /// Duty resolution of the channel's timer, so `duty / 2^duty_res` is the fraction.
    pub duty_res: u8,
    /// PWM frequency, recomputed from the timer divider exactly as `ledc_get_freq` does.
    pub freq_hz: u32,
}

/// What `LSCHn_CONF0.para_up` latched for one channel. HPOINT is not kept: the C3 cannot read
/// it back and the board port takes no phase.
#[derive(
    Copy,
    Clone,
    Default,
    PartialEq,
    Eq,
    Debug,
    pemu_core::serde::Serialize,
    pemu_core::serde::Deserialize,
)]
#[serde(crate = "pemu_core::serde")]
struct ChannelLatch {
    /// Latched `LSCHn_DUTY`, four fractional bits included.
    duty: u32,
}

/// What `LSTIMERn_CONF.para_up` latched for one timer.
#[derive(
    Copy,
    Clone,
    Default,
    PartialEq,
    Eq,
    Debug,
    pemu_core::serde::Serialize,
    pemu_core::serde::Deserialize,
)]
#[serde(crate = "pemu_core::serde")]
struct TimerLatch {
    duty_res: u8,
    /// Latched `clk_div`, 10.8 fixed point.
    clk_div: u32,
}

#[derive(Default, pemu_core::serde::Serialize, pemu_core::serde::Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Controller {
    regs: RegFile<block::Ledc, REG_COUNT>,
    channels: [ChannelLatch; CHANNELS],
    timers: [TimerLatch; TIMERS],
    irq: bool,
}

/// Register index stride of `LSCHn_CONF0` and its four followers (0x14 bytes).
const CH_STRIDE: usize = idx::LEDC_LSCH1_CONF0 - idx::LEDC_LSCH0_CONF0;
/// Register index stride of `LSTIMERn_CONF` (8 bytes).
const TIMER_STRIDE: usize = idx::LEDC_LSTIMER1_CONF - idx::LEDC_LSTIMER0_CONF;

impl Controller {
    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.regs.read(off, size, now, ledger)
    }

    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> Step {
        let mut step = Step::default();
        for (i, _) in self.regs.write(off, size, val, now, ledger).iter() {
            if i == idx::LEDC_INT_CLR {
                let clr = self.regs.get(idx::LEDC_INT_CLR);
                self.regs.lower(idx::LEDC_INT_RAW, clr);
                self.regs.set(idx::LEDC_INT_CLR, 0);
                step.irq = self.sync_irq().or(step.irq);
                continue;
            }
            if i == idx::LEDC_INT_ENA {
                step.irq = self.sync_irq().or(step.irq);
                continue;
            }
            for ch in 0..CHANNELS {
                if i == conf0(ch) && self.regs.get(i) & CONF0_PARA_UP != 0 {
                    self.latch_channel(ch);
                    step.changed |= 1 << ch;
                    step.stop = true;
                    step.irq = self.sync_irq().or(step.irq);
                }
            }
            for timer in 0..TIMERS {
                if i == timer_conf(timer) {
                    if self.regs.get(i) & TIMER_RST != 0 {
                        self.regs.set(timer_value(timer), 0);
                    }
                    if self.regs.get(i) & TIMER_PARA_UP != 0 {
                        step.changed |= self.latch_timer(timer);
                        step.stop = true;
                    }
                }
            }
        }
        step
    }

    /// The `BoardPorts::ledc` arguments of channel `ch` from its latched configuration.
    ///
    /// `pause` does not appear: it freezes the output, and this output is already a pure function
    /// of the latch. Treating it as `sig_out_en = 0` would switch the backlight off instead.
    pub fn channel(&self, ch: usize) -> ChannelOut {
        let conf0 = self.regs.get(conf0(ch));
        let timer = (conf0 & CONF0_TIMER_SEL) as usize;
        let duty_res = self.timers[timer].duty_res;
        let full = 1u32 << duty_res;
        let duty = if conf0 & CONF0_SIG_OUT_EN != 0 {
            self.channels[ch].duty >> DUTY_FRACTION
        } else if conf0 & CONF0_IDLE_LV != 0 {
            full
        } else {
            0
        };
        ChannelOut {
            channel: ch as u8,
            duty,
            duty_res,
            freq_hz: self.freq_hz(timer),
        }
    }

    /// The frequency `ledc_get_freq` recomputes for `timer`:
    /// `src_hz * 2^8 / (clk_div * 2^duty_res)`; 0 while no divider is latched.
    pub fn freq_hz(&self, timer: usize) -> u32 {
        let TimerLatch { duty_res, clk_div } = self.timers[timer];
        if clk_div == 0 {
            return 0;
        }
        let ticks = u64::from(clk_div) << duty_res;
        let scaled = u64::from(self.source_hz()) << DIV_FRACTION;
        u32::try_from(scaled / ticks).unwrap_or(u32::MAX)
    }

    pub fn source_hz(&self) -> u32 {
        match self.regs.get(idx::LEDC_CONF) & CONF_CLK_SEL {
            1 => APB_HZ,
            2 => RC_FAST_HZ,
            3 => XTAL_HZ,
            _ => 0,
        }
    }

    /// Level this model drives on interrupt source 23: `INT_ST != 0`.
    pub fn irq_level(&self) -> bool {
        self.regs.get(idx::LEDC_INT_ST) != 0
    }

    pub fn regs(&self) -> &RegFile<block::Ledc, REG_COUNT> {
        &self.regs
    }

    /// `LSCHn_CONF0.para_up`: latch the duty, mirror it into `DUTY_R`, and complete an immediate
    /// duty step (`duty_start` clears, `duty_chng_end` rises). UNVERIFIED: a real fade
    /// (`duty_num > 1`) would step over several timer overflows; no image installs one.
    fn latch_channel(&mut self, ch: usize) {
        self.channels[ch] = ChannelLatch {
            duty: self.regs.get(duty(ch)),
        };
        self.regs.set(duty_r(ch), self.channels[ch].duty);
        self.regs.lower(conf0(ch), CONF0_PARA_UP);
        if self.regs.get(conf1(ch)) & CONF1_DUTY_START != 0 {
            self.regs.lower(conf1(ch), CONF1_DUTY_START);
            self.regs.raise(idx::LEDC_INT_RAW, duty_chng_end(ch));
        }
    }

    /// `LSTIMERn_CONF.para_up`: latch duty resolution and divider, and return the channels whose
    /// [`ChannelOut`] actually changed (`ledc_set_freq` after `ledc_channel_config` is this
    /// write); re-latching the same configuration returns none.
    fn latch_timer(&mut self, timer: usize) -> u8 {
        let before: [ChannelOut; CHANNELS] = core::array::from_fn(|ch| self.channel(ch));
        let conf = self.regs.get(timer_conf(timer));
        self.timers[timer] = TimerLatch {
            duty_res: (conf & TIMER_DUTY_RES) as u8,
            clk_div: (conf >> TIMER_CLK_DIV_SHIFT) & TIMER_CLK_DIV,
        };
        self.regs.lower(timer_conf(timer), TIMER_PARA_UP);
        (0..CHANNELS)
            .filter(|ch| self.channel(*ch) != before[*ch])
            .fold(0, |mask, ch| mask | 1 << ch)
    }

    /// Recomputes `INT_ST = RAW & ENA` and returns the new level of source 23 when it changed.
    fn sync_irq(&mut self) -> Option<bool> {
        let st = self.regs.get(idx::LEDC_INT_RAW) & self.regs.get(idx::LEDC_INT_ENA);
        self.regs.set(idx::LEDC_INT_ST, st);
        let level = st != 0;
        (level != self.irq).then(|| {
            self.irq = level;
            level
        })
    }
}

const fn conf0(ch: usize) -> usize {
    idx::LEDC_LSCH0_CONF0 + CH_STRIDE * ch
}

const fn duty(ch: usize) -> usize {
    idx::LEDC_LSCH0_DUTY + CH_STRIDE * ch
}

const fn conf1(ch: usize) -> usize {
    idx::LEDC_LSCH0_CONF1 + CH_STRIDE * ch
}

const fn duty_r(ch: usize) -> usize {
    idx::LEDC_LSCH0_DUTY_R + CH_STRIDE * ch
}

const fn timer_conf(timer: usize) -> usize {
    idx::LEDC_LSTIMER0_CONF + TIMER_STRIDE * timer
}

const fn timer_value(timer: usize) -> usize {
    idx::LEDC_LSTIMER0_VALUE + TIMER_STRIDE * timer
}

/// `INT_RAW.duty_chng_end_lschn` (bits 9 to 4).
const fn duty_chng_end(ch: usize) -> u32 {
    1 << (4 + ch)
}

impl Peripheral for Controller {
    const ID: PeriphId = <block::Ledc as Block>::ID;
    const BASE: u32 = <block::Ledc as Block>::BASE;
    const SIZE: u32 = BLOCK_SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.regs.reset(kind);
        self.channels = [ChannelLatch::default(); CHANNELS];
        self.timers = [TimerLatch::default(); TIMERS];
        if let Some(level) = self.sync_irq() {
            cx.irq.set_source(irq::LEDC, level);
        }
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let step = self.store(off, size, val, cx.now, cx.ledger);
        if let Some(level) = step.irq {
            cx.irq.set_source(irq::LEDC, level);
        }
        RegWrite {
            stop: step.stop,
            wiring: if step.changed != 0 {
                Wiring::LedcChanged
            } else {
                Wiring::None
            },
        }
    }

    /// `LSTIMERn_CONF` changes only on a guest write. `LSTIMERn_VALUE` is a counter this model
    /// does not run, so everything else answers the conservative [`Stability::Never`].
    fn stable_until(&self, off: u32, _cx: &Cx) -> Stability {
        let timer_confs = (0..TIMERS).map(timer_conf);
        match RegFile::<block::Ledc, REG_COUNT>::index_of(off & !3) {
            Some(i) if timer_confs.clone().any(|t| t == i) => Stability::UntilInput,
            _ => Stability::Never,
        }
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.regs.class(off)
    }
}

pub type Model = Controller;

#[cfg(test)]
mod tests {
    use crate::r#gen::waits;

    use super::*;

    const T: VTime = VTime(11);

    const LSCH0_CONF0: u32 = 0x00;
    const LSCH0_HPOINT: u32 = 0x04;
    const LSCH0_DUTY: u32 = 0x08;
    const LSCH0_CONF1: u32 = 0x0C;
    const LSCH0_DUTY_R: u32 = 0x10;
    const LSTIMER0_CONF: u32 = 0xA0;
    const LSTIMER0_VALUE: u32 = 0xA4;
    const INT_RAW: u32 = 0xC0;
    const INT_ST: u32 = 0xC4;
    const INT_ENA: u32 = 0xC8;
    const INT_CLR: u32 = 0xCC;
    const CONF: u32 = 0xD0;

    /// `CONF1` as `ledc_set_duty_with_hpoint` writes it: an immediate step.
    const CONF1_IMMEDIATE: u32 = (1 << 30) | (1 << 20) | (1 << 10);

    fn write(c: &mut Controller, l: &mut FidelityLedger, off: u32, val: u32) -> Step {
        c.store(off, Size::B4, val, T, l)
    }

    /// The BSP backlight setup: APB, duty resolution 10, divider 4000, channel 0 on timer 0.
    fn backlight_init(c: &mut Controller, l: &mut FidelityLedger) {
        write(c, l, CONF, (1 << 31) | 1);
        write(c, l, LSTIMER0_CONF, 10 | (4000 << 4) | TIMER_PARA_UP);
        write(c, l, LSTIMER0_CONF, 10 | (4000 << 4));
        write(c, l, LSTIMER0_CONF, 10 | (4000 << 4) | TIMER_RST);
        write(c, l, LSTIMER0_CONF, 10 | (4000 << 4));
        write(c, l, LSCH0_HPOINT, 0);
        write(c, l, LSCH0_DUTY, 0);
        write(c, l, LSCH0_CONF1, CONF1_IMMEDIATE);
        update_duty(c, l);
    }

    /// `bsp_display_backlight(percent)`.
    fn set_backlight(c: &mut Controller, l: &mut FidelityLedger, percent: u32) -> u32 {
        let duty = 1023 * percent / 100;
        write(c, l, LSCH0_DUTY, duty << DUTY_FRACTION);
        update_duty(c, l);
        duty
    }

    /// `ledc_update_duty`: `sig_out_en 1`, `duty_start 1`, `para_up 1`.
    fn update_duty(c: &mut Controller, l: &mut FidelityLedger) -> Step {
        write(c, l, LSCH0_CONF0, CONF0_SIG_OUT_EN);
        write(c, l, LSCH0_CONF1, CONF1_IMMEDIATE | CONF1_DUTY_START);
        write(c, l, LSCH0_CONF0, CONF0_SIG_OUT_EN | CONF0_PARA_UP)
    }

    #[test]
    fn the_block_identity_is_the_c3_devices_row() {
        assert_eq!(<Controller as Peripheral>::ID, super::super::id::LEDC);
        assert_eq!(<Controller as Peripheral>::BASE, 0x6001_9000);
        assert_eq!(irq::LEDC.0, 23, "the ledc interrupt source");
        assert_eq!(CH_STRIDE, 5, "channel block of five registers, stride 0x14");
        assert_eq!(TIMER_STRIDE, 2, "timer block of two registers, stride 8");
    }

    #[test]
    fn the_bsp_backlight_latches_duty_1023_at_ten_bits_and_5000_hz() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        backlight_init(&mut c, &mut l);
        assert_eq!(
            c.channel(0),
            ChannelOut {
                channel: 0,
                duty: 0,
                duty_res: 10,
                freq_hz: 5_000,
            }
        );

        let duty = set_backlight(&mut c, &mut l, 100);
        assert_eq!(duty, 1023);
        assert_eq!(
            c.channel(0),
            ChannelOut {
                channel: 0,
                duty: 1023,
                duty_res: 10,
                freq_hz: 5_000,
            }
        );
        assert_eq!(c.freq_hz(0), 5_000, "80e6 * 256 / (4000 * 1024)");
        assert_eq!(c.source_hz(), APB_HZ);
    }

    #[test]
    fn the_backlight_percentages_of_the_bsp_are_integer_duties() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        backlight_init(&mut c, &mut l);
        for (percent, want) in [(100, 1023), (50, 511), (10, 102), (0, 0)] {
            assert_eq!(set_backlight(&mut c, &mut l, percent), want);
            assert_eq!(c.channel(0).duty, want, "{percent} %");
        }
    }

    #[test]
    fn nothing_takes_effect_until_para_up_and_both_para_up_bits_read_zero() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        backlight_init(&mut c, &mut l);

        write(&mut c, &mut l, LSCH0_DUTY, 800 << DUTY_FRACTION);
        assert_eq!(c.channel(0).duty, 0, "an unlatched duty changes nothing");
        assert_eq!(c.load(LSCH0_DUTY_R, Size::B4, T, &mut l), 0);

        let step = write(
            &mut c,
            &mut l,
            LSCH0_CONF0,
            CONF0_SIG_OUT_EN | CONF0_PARA_UP,
        );
        assert_eq!(step.changed, 1 << 0, "the wiring step carries the channel");
        assert_eq!(step.changed_channels().collect::<Vec<_>>(), vec![0]);
        assert!(step.stop, "OkStop, so the machine applies the wiring");
        assert_eq!(c.channel(0).duty, 800);
        assert_eq!(
            c.load(LSCH0_DUTY_R, Size::B4, T, &mut l),
            800 << DUTY_FRACTION,
            "DUTY_R mirrors the latched duty, fractional bits included"
        );
        assert_eq!(
            c.load(LSCH0_CONF0, Size::B4, T, &mut l) & CONF0_PARA_UP,
            0,
            "the channel para_up reads back 0"
        );
        assert_eq!(
            c.load(LSTIMER0_CONF, Size::B4, T, &mut l) & TIMER_PARA_UP,
            0,
            "the timer para_up reads back 0"
        );
    }

    #[test]
    fn an_immediate_duty_step_clears_duty_start_and_raises_duty_chng_end() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        backlight_init(&mut c, &mut l);
        write(&mut c, &mut l, INT_CLR, 0xFFFF);

        set_backlight(&mut c, &mut l, 50);
        assert_eq!(
            c.load(LSCH0_CONF1, Size::B4, T, &mut l) & CONF1_DUTY_START,
            0
        );
        assert_eq!(
            c.load(INT_RAW, Size::B4, T, &mut l),
            duty_chng_end(0),
            "channel 0 is bit 4 of INT_RAW"
        );
    }

    #[test]
    fn a_channel_that_does_not_drive_its_pin_shows_the_idle_level() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        backlight_init(&mut c, &mut l);
        set_backlight(&mut c, &mut l, 100);

        write(&mut c, &mut l, LSCH0_CONF0, CONF0_PARA_UP);
        assert_eq!(c.channel(0).duty, 0, "sig_out_en 0 and idle_lv 0 is off");

        write(&mut c, &mut l, LSCH0_CONF0, CONF0_IDLE_LV | CONF0_PARA_UP);
        assert_eq!(c.channel(0).duty, 1 << 10, "idle_lv 1 is the full level");

        write(
            &mut c,
            &mut l,
            LSCH0_CONF0,
            CONF0_SIG_OUT_EN | CONF0_PARA_UP,
        );
        assert_eq!(c.channel(0).duty, 1023);
    }

    #[test]
    fn a_paused_timer_freezes_the_output_at_its_current_level() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        backlight_init(&mut c, &mut l);
        set_backlight(&mut c, &mut l, 100);
        let running = c.channel(0);
        assert_eq!(running.duty, 1023);

        write(
            &mut c,
            &mut l,
            LSTIMER0_CONF,
            10 | (4000 << 4) | TIMER_PAUSE,
        );
        assert_eq!(c.channel(0), running, "the level is frozen, not dropped");

        // `ledc_timer_resume`, which `ledc_timer_config` ends with.
        write(&mut c, &mut l, LSTIMER0_CONF, 10 | (4000 << 4));
        assert_eq!(c.channel(0), running);
    }

    /// The BSP configures the timer before the channel, which hides this case today.
    #[test]
    fn a_timer_latch_changes_every_channel_bound_to_that_timer() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        backlight_init(&mut c, &mut l);
        set_backlight(&mut c, &mut l, 100);

        // Channel 1 joins timer 0; channel 2 stays on timer 1, which nothing configured.
        write(
            &mut c,
            &mut l,
            LSCH0_CONF0 + 0x14,
            CONF0_SIG_OUT_EN | CONF0_PARA_UP,
        );
        write(
            &mut c,
            &mut l,
            LSCH0_CONF0 + 0x28,
            1 | CONF0_SIG_OUT_EN | CONF0_PARA_UP,
        );

        // `ledc_set_freq`: the same duty resolution at half the frequency, latched on the timer.
        let step = write(
            &mut c,
            &mut l,
            LSTIMER0_CONF,
            10 | (8000 << 4) | TIMER_PARA_UP,
        );
        // Channels 3 to 5 reset to `timer_sel` 0, so they moved too; channel 2 is on timer 1.
        assert_eq!(
            step.changed_channels().collect::<Vec<_>>(),
            vec![0, 1, 3, 4, 5],
            "every channel of timer 0, and no channel of another timer"
        );
        assert!(step.stop, "OkStop, so the machine applies the wiring");
        assert_eq!(c.channel(0).freq_hz, 2_500, "80e6 * 256 / (8000 * 1024)");
        assert_eq!(c.channel(1).freq_hz, 2_500);
        assert_eq!(c.channel(2).freq_hz, 0, "timer 1 is unconfigured");

        // A timer write without `para_up` latches nothing, so it changes no channel.
        let step = write(&mut c, &mut l, LSTIMER0_CONF, 10 | (4000 << 4));
        assert_eq!(step.changed, 0);
        assert_eq!(c.channel(0).freq_hz, 2_500, "the old latch still stands");

        // And a `para_up` that re-latches the configuration already in force changes nothing.
        let step = write(
            &mut c,
            &mut l,
            LSTIMER0_CONF,
            10 | (8000 << 4) | TIMER_PARA_UP,
        );
        assert_eq!(step.changed, 0, "the same configuration is not a change");
    }

    #[test]
    fn the_frequency_follows_the_divider_and_the_clock_source() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        assert_eq!(c.freq_hz(0), 0, "no divider latched yet");
        assert_eq!(c.source_hz(), 0, "no clock selected yet");

        backlight_init(&mut c, &mut l);
        assert_eq!(c.freq_hz(0), 5_000);

        write(&mut c, &mut l, CONF, (1 << 31) | 3);
        assert_eq!(c.source_hz(), XTAL_HZ);
        assert_eq!(c.freq_hz(0), 2_500);

        // Eight-bit duty resolution on APB quadruples it.
        write(&mut c, &mut l, CONF, (1 << 31) | 1);
        write(
            &mut c,
            &mut l,
            LSTIMER0_CONF,
            8 | (4000 << 4) | TIMER_PARA_UP,
        );
        assert_eq!(c.freq_hz(0), 20_000);
        assert_eq!(c.channel(0).duty_res, 8);
    }

    #[test]
    fn int_st_is_raw_and_ena_and_drives_source_23() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        backlight_init(&mut c, &mut l);
        assert!(!c.irq_level(), "nothing enabled the duty_chng_end bit");
        assert_eq!(c.load(INT_ST, Size::B4, T, &mut l), 0);

        let step = write(&mut c, &mut l, INT_ENA, duty_chng_end(0));
        assert_eq!(step.irq, Some(true));
        assert!(c.irq_level());
        assert_eq!(c.load(INT_ST, Size::B4, T, &mut l), duty_chng_end(0));

        let step = write(&mut c, &mut l, INT_CLR, duty_chng_end(0));
        assert_eq!(step.irq, Some(false));
        assert_eq!(c.load(INT_RAW, Size::B4, T, &mut l), 0);
        assert_eq!(
            c.load(INT_CLR, Size::B4, T, &mut l),
            0,
            "INT_CLR is write-only"
        );
    }

    #[test]
    fn a_timer_reset_zeroes_the_counter() {
        let (mut c, mut l) = (Controller::default(), FidelityLedger::default());
        c.regs.set(timer_value(0), 512);
        write(&mut c, &mut l, LSTIMER0_CONF, TIMER_RST);
        assert_eq!(c.load(LSTIMER0_VALUE, Size::B4, T, &mut l), 0);
    }

    #[test]
    fn the_block_file_carries_no_busy_wait_row() {
        assert_eq!(waits::of_block("ledc").count(), 0);
    }
}
