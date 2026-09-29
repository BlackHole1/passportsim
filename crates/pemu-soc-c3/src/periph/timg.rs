//! Timer groups TIMG0 and TIMG1 (`specs/blocks/timg0.toml`, `timg1.toml`): timer T0, the RTC
//! slow-clock calibration and the main watchdog. Both groups share a layout; a [`Group`] marker
//! carries the row, the interrupt sources and the reset causes.
//!
//! RTCCALI: `select_rtc_slow_clk` loops `do { rtc_clk_cal } while (cal_val == 0)`, so a zero
//! result hangs boot. Only a one-off calibration (a `START` rising edge) sets `RDY`; the cycling
//! calibration the register resets to leaves it clear, as the `probe_campaign_regs` capture reads
//! after `TIMERGROUP_RST`. IDF leaves the cycling measurement by writing a small `TIMEOUT_THRES`
//! and polling for `RDY` or `TIMEOUT`; the model times it out at that write.
//!
//! MWDT is the interrupt watchdog on TIMG1 and the task watchdog on TIMG0: four stages, each a
//! hold in ticks and an action (nothing, interrupt, CPU reset, system reset).
//!
//! T0 is the gptimer; the IDF core does not use it, but it shares the watchdog's lazy counter.
//!
//! Not here: flash-boot watchdog arming. `WDT_FLASHBOOT_MOD_EN` resets to 1 and the bootloader
//! clears it; arming it would make a stalled bring-up reset itself instead of stopping where the
//! ledger can name the block. The bit is stored; the counter is not started.

use core::marker::PhantomData;

use pemu_core::fidelity::Fidelity;
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{RegSpec, Size};
use pemu_core::reset::{ResetCause, ResetKind};
use pemu_core::sched::{EventHandle, PeriphId};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use crate::r#gen::{regs_timg0, regs_timg1};
use crate::regs::{Ports, Regs, Table};

use super::{Block, Cx, Peripheral, RegRead, RegWrite, Wiring};

pub const REG_COUNT: usize = regs_timg0::REG_COUNT;

const _: () = assert!(regs_timg0::REG_COUNT == regs_timg1::REG_COUNT);

/// Picoseconds per cycle of the 40 MHz crystal, the MWDT clock IDF selects
/// (`MWDT_CLK_SRC_DEFAULT = SOC_MOD_CLK_XTAL`).
pub const XTAL_PS: u64 = 25_000;
/// Picoseconds per cycle of the 80 MHz APB clock, the other MWDT and T0 source. A peripheral is
/// handed no `Clock`, so this is fixed at the rate both reference builds hold (UNVERIFIED as a
/// model choice).
pub const APB_PS: u64 = 12_500;
/// RTC slow clock: 136 kHz, shared by the RTC counter, the sleep timer, the RWDT and this
/// calibration.
pub const SLOW_HZ: u64 = 136_000;
pub const XTAL_HZ: u64 = 40_000_000;
/// RC_FAST divided by 256, the `RTC_CALI_CLK_SEL` 1 source, in Hz.
///
/// Class A from the `probe_campaign_timing` capture (`rtc_clk_cal(RTC_CAL_8MD256, 1024)`): 591342
/// and 591126 crystal cycles per 1024 in two runs, so RC_FAST runs at about 17.735 MHz, not the
/// nominal 17.5 MHz. It is an RC oscillator; the RC slow clock of the same device moved 0.8 %
/// between two days' captures.
pub const RC_FAST_D256_HZ: u64 = 69_279;

/// The RC_FAST oscillator itself, about 17.735 MHz. The one place the rate lives: SYSTEM, LEDC,
/// I2C and UART take it from here, so every block counting RC_FAST cycles agrees with the
/// calibration the guest reads.
pub const RC_FAST_HZ: u64 = RC_FAST_D256_HZ * 256;

/// T0 is 54 bits: T0LO 32 and T0HI 22.
const T0_MASK: u64 = (1 << 54) - 1;
const T0_USE_XTAL: u8 = 9;
/// Self-clearing.
const T0_ALARM_EN: u8 = 10;
/// 16 bits.
const T0_DIVIDER: u8 = 13;
const T0_AUTORELOAD: u8 = 29;
/// 1 counts up.
const T0_INCREASE: u8 = 30;
const T0_EN: u8 = 31;
/// Self-clearing.
const T0_UPDATE: u8 = 31;
const T0_HI_WIDTH: u8 = 22;

/// `TIMG_WDT_FLASHBOOT_MOD_EN` in WDTCONFIG0.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "register map; only the tests write it")
)]
const WDT_FLASHBOOT_MOD_EN: u8 = 14;
const WDT_USE_XTAL: u8 = 21;
/// Stage `n`'s two-bit action field is at `29 - 2 * n`.
const WDT_STG0: u8 = 29;
const WDT_EN: u8 = 31;
/// 16 bits.
const WDT_CLK_PRESCALE: u8 = 16;
/// Write-protection key of WDTCONFIG0 to 5 and WDTFEED.
pub const WDT_WKEY: u32 = 0x50D8_3AA1;
pub const WDT_STAGES: usize = 4;

const CALI_START_CYCLING: u8 = 12;
const CALI_CLK_SEL: u8 = 13;
const CALI_RDY: u8 = 15;
/// 15 bits: the slow cycles to count.
const CALI_MAX: u8 = 16;
const CALI_START: u8 = 31;
const CALI_DATA_VLD: u8 = 0;
/// 25 bits.
const CALI_VALUE: u8 = 7;
const CALI_TIMEOUT: u8 = 0;

/// Bit of the four interrupt registers.
const INT_T0: u32 = 1 << 0;
const INT_WDT: u32 = 1 << 1;

const TAG_T0: u16 = 0;
const TAG_WDT: u16 = 1;

pub trait Group: Table<REG_COUNT> + 'static {
    const ID: PeriphId;
    const BASE: u32;
    const SIZE: u32;
    /// Interrupt source of T0: 32 for TIMG0, 34 for TIMG1.
    const T0_SOURCE: IrqSource;
    /// Interrupt source of the watchdog: 33 for TIMG0, 35 for TIMG1.
    const WDT_SOURCE: IrqSource;
    /// Reset cause of a stage whose action is a CPU reset: 0x0B for TIMG0, 0x11 for TIMG1.
    const CPU_RESET: ResetCause;
    /// Reset cause of a stage whose action is a system reset: 0x07 for TIMG0, 0x08 for TIMG1.
    const SYS_RESET: ResetCause;
}

/// TIMG0: the task watchdog and the RTC slow-clock calibration the boot path polls.
pub struct Timg0;

impl Table<REG_COUNT> for Timg0 {
    const BLOCK: &'static str = "timg0";

    fn specs() -> &'static [RegSpec; REG_COUNT] {
        &regs_timg0::REGS
    }
}

impl Group for Timg0 {
    const ID: PeriphId = super::id::TIMG0;
    const BASE: u32 = <super::block::Timg0 as Block>::BASE;
    const SIZE: u32 = <super::block::Timg0 as Block>::SIZE;
    const T0_SOURCE: IrqSource = irq::TG0_T0;
    const WDT_SOURCE: IrqSource = irq::TG0_WDT;
    const CPU_RESET: ResetCause = ResetCause::TG0WDT_CPU;
    const SYS_RESET: ResetCause = ResetCause::TG0WDT_SYS;
}

/// TIMG1: the interrupt watchdog.
pub struct Timg1;

impl Table<REG_COUNT> for Timg1 {
    const BLOCK: &'static str = "timg1";

    fn specs() -> &'static [RegSpec; REG_COUNT] {
        &regs_timg1::REGS
    }
}

impl Group for Timg1 {
    const ID: PeriphId = super::id::TIMG1;
    const BASE: u32 = <super::block::Timg1 as Block>::BASE;
    const SIZE: u32 = <super::block::Timg1 as Block>::SIZE;
    const T0_SOURCE: IrqSource = irq::TG1_T0;
    const WDT_SOURCE: IrqSource = irq::TG1_WDT;
    const CPU_RESET: ResetCause = ResetCause::TG1WDT_CPU;
    const SYS_RESET: ResetCause = ResetCause::TG1WDT_SYS;
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum StageAction {
    /// 0: nothing; the stage still consumes its hold.
    Off,
    /// 1: set `INT_RAW.WDT`, which drives source 33 or 35.
    Interrupt,
    /// 2: CPU reset, cause 0x0B or 0x11.
    CpuReset,
    /// 3: system reset, cause 0x07 or 0x08.
    SystemReset,
}

impl StageAction {
    pub const fn of(bits: u32) -> StageAction {
        match bits & 3 {
            0 => StageAction::Off,
            1 => StageAction::Interrupt,
            2 => StageAction::CpuReset,
            _ => StageAction::SystemReset,
        }
    }
}

#[derive(Copy, Clone, Default, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct Timer {
    epoch: VTime,
    loaded: u64,
    armed: Option<EventHandle>,
}

#[derive(Copy, Clone, Default, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct Wdt {
    epoch: VTime,
    stage: usize,
    running: bool,
    armed: Option<EventHandle>,
    /// When a stage whose action is an interrupt last expired, latched until a feed, a disable
    /// or a reset ([`Model::wdt_interrupt_fired`]).
    fired: Option<VTime>,
    /// The same instant, kept for the whole boot: a feed clears [`Wdt::fired`] but not this, so a
    /// fault decoded after the panic handler fed the watchdogs still knows which one fired.
    last_fired: Option<VTime>,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde", bound = "")]
pub struct Model<G: Group> {
    regs: Regs<G, REG_COUNT>,
    t0: Timer,
    wdt: Wdt,
    #[serde(skip)]
    group: PhantomData<fn() -> G>,
    /// Rate of the RTC slow clock the calibration counts: [`SLOW_HZ`] until the machine applies
    /// its profile's `rtc_slow_hz`. Not serialized, like `rtc_cntl::Model`'s copy.
    #[serde(skip, default = "default_slow_hz")]
    slow_hz: u64,
}

fn default_slow_hz() -> u64 {
    SLOW_HZ
}

pub type Timg0Model = Model<Timg0>;

pub type Timg1Model = Model<Timg1>;

impl<G: Group> Default for Model<G> {
    fn default() -> Model<G> {
        Model {
            regs: Regs::default(),
            t0: Timer::default(),
            wdt: Wdt::default(),
            group: PhantomData,
            slow_hz: SLOW_HZ,
        }
    }
}

impl<G: Group> Model<G> {
    /// Sets the slow clock rate the calibration counts (the profile's `rtc_slow_hz`), which is the
    /// same value `rtc_cntl::Model::set_slow_hz` gets. A rate of 0 is ignored.
    pub fn set_slow_hz(&mut self, hz: u64) {
        if hz != 0 {
            self.slow_hz = hz;
        }
    }
}

impl<G: Group> Model<G> {
    const CONFIG0: usize = regs_timg0::idx::TIMG_WDTCONFIG0;
    const CONFIG1: usize = regs_timg0::idx::TIMG_WDTCONFIG1;
    const CONFIG2: usize = regs_timg0::idx::TIMG_WDTCONFIG2;
    const WPROTECT: usize = regs_timg0::idx::TIMG_WDTWPROTECT;
    const FEED: usize = regs_timg0::idx::TIMG_WDTFEED;
    const T0CONFIG: usize = regs_timg0::idx::TIMG_T0CONFIG;
    const CALICFG: usize = regs_timg0::idx::TIMG_RTCCALICFG;
    const CALICFG1: usize = regs_timg0::idx::TIMG_RTCCALICFG1;
    const CALICFG2: usize = regs_timg0::idx::TIMG_RTCCALICFG2;
    const INT_ENA: usize = regs_timg0::idx::TIMG_INT_ENA_TIMERS;
    const INT_RAW: usize = regs_timg0::idx::TIMG_INT_RAW_TIMERS;
    const INT_ST: usize = regs_timg0::idx::TIMG_INT_ST_TIMERS;

    /// The group's clock was gated for `ps`: the timer and the watchdog read as if that interval
    /// never passed. The caller postpones the pending events by the same interval.
    pub fn clock_gated_for(&mut self, ps: u64) {
        self.t0.epoch = VTime(self.t0.epoch.0.saturating_add(ps));
        self.wdt.epoch = VTime(self.wdt.epoch.0.saturating_add(ps));
    }

    pub fn wdt_stage(&self) -> Option<usize> {
        self.wdt.running.then_some(self.wdt.stage)
    }

    /// When this group's watchdog last drove its interrupt stage, or `None` since the last feed,
    /// disable or reset. A fault envelope classifies a watchdog from this: TIMG0's MWDT is the
    /// task watchdog and TIMG1's the interrupt watchdog. It outlives the interrupt status because
    /// `task_wdt_isr` clears the status and then panics.
    pub fn wdt_interrupt_fired(&self) -> Option<(VTime, bool)> {
        self.wdt.last_fired.map(|at| (at, self.wdt.fired.is_some()))
    }

    pub fn timer_value(&self, now: VTime) -> u64 {
        let config = self.regs.get(Self::T0CONFIG);
        if config >> T0_EN & 1 == 0 {
            return self.t0.loaded;
        }
        let ticks = elapsed(now, self.t0.epoch, self.t0_tick_ps());
        if config >> T0_INCREASE & 1 == 1 {
            self.t0.loaded.wrapping_add(ticks) & T0_MASK
        } else {
            self.t0.loaded.wrapping_sub(ticks) & T0_MASK
        }
    }

    /// Watchdog count at `now`, in ticks since the current stage started.
    pub fn wdt_count(&self, now: VTime) -> u64 {
        if !self.wdt.running {
            return 0;
        }
        elapsed(now, self.wdt.epoch, self.wdt_tick_ps())
    }

    pub fn reset_block(&mut self, kind: ResetKind, ports: &mut Ports) {
        if !kind.clears(G::specs()[Self::CONFIG0].domain) {
            return;
        }
        ports.disarm(&mut self.t0.armed);
        ports.disarm(&mut self.wdt.armed);
        self.regs.reset(kind);
        self.t0 = Timer::default();
        self.wdt = Wdt::default();
        self.apply_t0(ports);
        self.apply_wdt(ports);
        self.drive_sources(ports);
    }

    pub fn load(&mut self, off: u32, size: Size, ports: &mut Ports) -> u32 {
        self.regs.read(off, size, G::ID, ports.now, ports.ledger)
    }

    pub fn store(&mut self, off: u32, size: Size, val: u32, ports: &mut Ports) {
        let word = off & !3;
        let idx = self.regs.index_of(word);
        if idx.is_some_and(|idx| self.write_protected(idx)) {
            // CONFIG0 to 5 and FEED need the key in WDTWPROTECT.
            return;
        }
        let before_t0 = self.regs.get(Self::T0CONFIG);
        let Some(delta) = self
            .regs
            .write(off, size, val, G::ID, ports.now, ports.ledger)
        else {
            return;
        };
        match idx {
            Some(i) if i == Self::T0CONFIG => {
                self.retime_t0(before_t0, ports);
                self.apply_t0(ports);
            }
            Some(i) if i == regs_timg0::idx::TIMG_T0UPDATE => {
                if delta.triggers >> T0_UPDATE & 1 == 1 {
                    self.regs.clear_sc(i, delta.triggers);
                    self.latch_t0_now(ports.now);
                }
            }
            Some(i) if i == regs_timg0::idx::TIMG_T0LOAD => {
                if delta.triggers != 0 {
                    self.regs.clear_sc(i, delta.triggers);
                    let hi = self
                        .regs
                        .field(regs_timg0::idx::TIMG_T0LOADHI, 0, T0_HI_WIDTH);
                    let lo = self.regs.field(regs_timg0::idx::TIMG_T0LOADLO, 0, 32);
                    self.t0.loaded = (u64::from(hi) << 32 | u64::from(lo)) & T0_MASK;
                    self.t0.epoch = ports.now;
                    self.apply_t0(ports);
                }
            }
            Some(i)
                if i == regs_timg0::idx::TIMG_T0ALARMLO || i == regs_timg0::idx::TIMG_T0ALARMHI =>
            {
                self.apply_t0(ports);
            }
            Some(i) if i == Self::CONFIG0 || i == Self::CONFIG1 => {
                if i == Self::CONFIG1 {
                    // Changing the prescaler restarts the tick from now, so the stage that is
                    // running keeps the ticks it has already counted.
                    self.wdt.epoch = ports.now;
                }
                self.apply_wdt(ports);
            }
            Some(i) if (Self::CONFIG2..Self::CONFIG2 + WDT_STAGES).contains(&i) => {
                self.apply_wdt(ports);
            }
            Some(i) if i == Self::FEED => {
                if delta.triggers != 0 || delta.after != delta.before {
                    self.feed(ports);
                }
            }
            Some(i) if i == Self::CALICFG => self.apply_cali(delta.before),
            Some(i) if i == Self::CALICFG2 => self.time_out_cycling(),
            Some(i) if i == Self::INT_ENA => self.drive_sources(ports),
            Some(i) if i == regs_timg0::idx::TIMG_INT_CLR_TIMERS => {
                let raw = self.regs.get(Self::INT_RAW) & !delta.triggers;
                self.regs.set(Self::INT_RAW, raw);
                self.drive_sources(ports);
            }
            _ => {}
        }
    }

    pub fn on_tag(&mut self, tag: u16, ports: &mut Ports) -> Wiring {
        match tag {
            TAG_T0 => {
                self.t0.armed = None;
                self.fire_t0(ports);
                Wiring::None
            }
            TAG_WDT => {
                self.wdt.armed = None;
                self.expire_stage(ports)
            }
            _ => Wiring::None,
        }
    }

    pub fn feed(&mut self, ports: &mut Ports) {
        // A fed watchdog is no longer the outstanding fault; what fired is still remembered for
        // the boot ([`Model::wdt_interrupt_fired`]).
        self.wdt.fired = None;
        self.wdt.stage = 0;
        self.wdt.epoch = ports.now;
        self.apply_wdt(ports);
    }

    /// Whether a write to register `idx` is blocked: CONFIG0 to 5 and FEED accept a write only
    /// while WDTWPROTECT holds the key.
    fn write_protected(&self, idx: usize) -> bool {
        let guarded =
            (Self::CONFIG0..=Self::CONFIG2 + WDT_STAGES - 1).contains(&idx) || idx == Self::FEED;
        guarded && self.regs.get(Self::WPROTECT) != WDT_WKEY
    }

    fn t0_tick_ps(&self) -> u64 {
        let config = self.regs.get(Self::T0CONFIG);
        let source = if config >> T0_USE_XTAL & 1 == 1 {
            XTAL_PS
        } else {
            APB_PS
        };
        // UNVERIFIED: a divider of 0 is taken as the full 16-bit period, the family convention;
        // IDF never writes 0.
        let divider = match config >> T0_DIVIDER & 0xFFFF {
            0 => 1 << 16,
            d => u64::from(d),
        };
        source.saturating_mul(divider)
    }

    /// Picoseconds per watchdog tick: the source period times `WDT_CLK_PRESCALE`. IDF's 20000 on
    /// the crystal makes one tick 500 us.
    fn wdt_tick_ps(&self) -> u64 {
        let config0 = self.regs.get(Self::CONFIG0);
        let source = if config0 >> WDT_USE_XTAL & 1 == 1 {
            XTAL_PS
        } else {
            APB_PS
        };
        let prescale = match self.regs.get(Self::CONFIG1) >> WDT_CLK_PRESCALE & 0xFFFF {
            0 => 1 << 16,
            p => u64::from(p),
        };
        source.saturating_mul(prescale)
    }

    /// Keeps the T0 value across a T0CONFIG write that changes the rate, the direction or the
    /// enable: the counter shows the same value on both sides of the write.
    fn retime_t0(&mut self, before: u32, ports: &mut Ports) {
        let after = self.regs.get(Self::T0CONFIG);
        if before == after {
            return;
        }
        let was_running = before >> T0_EN & 1 == 1;
        let value = if was_running {
            let ps = {
                let source = if before >> T0_USE_XTAL & 1 == 1 {
                    XTAL_PS
                } else {
                    APB_PS
                };
                let divider = match before >> T0_DIVIDER & 0xFFFF {
                    0 => 1 << 16,
                    d => u64::from(d),
                };
                source.saturating_mul(divider)
            };
            let ticks = elapsed(ports.now, self.t0.epoch, ps);
            if before >> T0_INCREASE & 1 == 1 {
                self.t0.loaded.wrapping_add(ticks) & T0_MASK
            } else {
                self.t0.loaded.wrapping_sub(ticks) & T0_MASK
            }
        } else {
            self.t0.loaded
        };
        self.t0.loaded = value;
        self.t0.epoch = ports.now;
    }

    fn apply_t0(&mut self, ports: &mut Ports) {
        ports.disarm(&mut self.t0.armed);
        let config = self.regs.get(Self::T0CONFIG);
        if config >> T0_EN & 1 == 0 || config >> T0_ALARM_EN & 1 == 0 {
            return;
        }
        let hi = self
            .regs
            .field(regs_timg0::idx::TIMG_T0ALARMHI, 0, T0_HI_WIDTH);
        let lo = self.regs.field(regs_timg0::idx::TIMG_T0ALARMLO, 0, 32);
        let alarm = (u64::from(hi) << 32 | u64::from(lo)) & T0_MASK;
        let value = self.timer_value(ports.now);
        let ticks = if config >> T0_INCREASE & 1 == 1 {
            alarm.wrapping_sub(value) & T0_MASK
        } else {
            value.wrapping_sub(alarm) & T0_MASK
        };
        let at = VTime(
            ports
                .now
                .0
                .saturating_add(ticks.saturating_mul(self.t0_tick_ps())),
        );
        ports.rearm(&mut self.t0.armed, G::ID, TAG_T0, at);
    }

    /// The T0 alarm matched: raise the interrupt, clear `ALARM_EN` and reload when `AUTORELOAD`.
    fn fire_t0(&mut self, ports: &mut Ports) {
        let config = self.regs.get(Self::T0CONFIG);
        self.regs.set(Self::T0CONFIG, config & !(1 << T0_ALARM_EN));
        if config >> T0_AUTORELOAD & 1 == 1 {
            let hi = self
                .regs
                .field(regs_timg0::idx::TIMG_T0LOADHI, 0, T0_HI_WIDTH);
            let lo = self.regs.field(regs_timg0::idx::TIMG_T0LOADLO, 0, 32);
            self.t0.loaded = (u64::from(hi) << 32 | u64::from(lo)) & T0_MASK;
        } else {
            self.t0.loaded = self.timer_value(ports.now);
        }
        self.t0.epoch = ports.now;
        let raw = self.regs.get(Self::INT_RAW) | INT_T0;
        self.regs.set(Self::INT_RAW, raw);
        self.drive_sources(ports);
    }

    fn latch_t0_now(&mut self, now: VTime) {
        let value = self.timer_value(now);
        self.regs
            .set_field(regs_timg0::idx::TIMG_T0LO, 0, 32, value as u32);
        self.regs.set_field(
            regs_timg0::idx::TIMG_T0HI,
            0,
            T0_HI_WIDTH,
            (value >> 32) as u32,
        );
    }

    fn apply_wdt(&mut self, ports: &mut Ports) {
        ports.disarm(&mut self.wdt.armed);
        let enabled = self.regs.get(Self::CONFIG0) >> WDT_EN & 1 == 1;
        if !enabled {
            self.wdt.running = false;
            self.wdt.stage = 0;
            self.wdt.fired = None;
            return;
        }
        if !self.wdt.running {
            self.wdt.running = true;
            self.wdt.stage = 0;
            self.wdt.epoch = ports.now;
        }
        if self.wdt.stage >= WDT_STAGES {
            // After stage 3 the behavior is UNVERIFIED; the counter stops.
            return;
        }
        let hold = u64::from(self.regs.get(Self::CONFIG2 + self.wdt.stage));
        let tick = self.wdt_tick_ps().max(1);
        let mut at = VTime(self.wdt.epoch.0.saturating_add(hold.saturating_mul(tick)));
        // The counter is compared with the hold at its own tick, so a hold written below the count
        // trips at the next tick, not at the write. IDF's interrupt-watchdog tick hook writes the
        // holds and then feeds a few instructions later, after a spin that let the count pass;
        // the device neither panics nor resets there (the `probe_reset` IWDT stage).
        if at <= ports.now {
            let ticks = (ports.now.0 - self.wdt.epoch.0) / tick + 1;
            at = VTime(self.wdt.epoch.0.saturating_add(ticks.saturating_mul(tick)));
        }
        ports.rearm(&mut self.wdt.armed, G::ID, TAG_WDT, at);
    }

    fn expire_stage(&mut self, ports: &mut Ports) -> Wiring {
        if !self.wdt.running || self.wdt.stage >= WDT_STAGES {
            return Wiring::None;
        }
        let bits = self.regs.get(Self::CONFIG0) >> (WDT_STG0 - 2 * self.wdt.stage as u8) & 3;
        let action = StageAction::of(bits);
        self.wdt.stage += 1;
        self.wdt.epoch = ports.now;
        self.apply_wdt(ports);
        match action {
            StageAction::Off => Wiring::None,
            StageAction::Interrupt => {
                self.wdt.fired = Some(ports.now);
                self.wdt.last_fired = Some(ports.now);
                let raw = self.regs.get(Self::INT_RAW) | INT_WDT;
                self.regs.set(Self::INT_RAW, raw);
                self.drive_sources(ports);
                Wiring::None
            }
            StageAction::CpuReset => reset_wiring(G::CPU_RESET),
            StageAction::SystemReset => reset_wiring(G::SYS_RESET),
        }
    }

    /// RTCCALICFG: a START rising edge produces a result and sets RDY in the same access (the
    /// `timg0.rtccali_rdy` row). `START_CYCLING` sets no RDY.
    fn apply_cali(&mut self, before: u32) {
        let after = self.regs.get(Self::CALICFG);
        let started = after >> CALI_START & 1 == 1 && before >> CALI_START & 1 == 0;
        if started {
            self.calibrate();
        }
    }

    /// RTCCALICFG2: a write while a cycling calibration runs and no one-off result is ready times
    /// the cycling calibration out, the exit `rtc_clk_cal_internal` waits for after it writes
    /// `TIMEOUT_THRES` 1 (`timg0.rtccali_cycling_timeout`). UNVERIFIED as a timing.
    fn time_out_cycling(&mut self) {
        let cfg = self.regs.get(Self::CALICFG);
        if cfg >> CALI_START_CYCLING & 1 == 1 && cfg >> CALI_RDY & 1 == 0 {
            self.regs.set_field(Self::CALICFG2, CALI_TIMEOUT, 1, 1);
        }
    }

    /// Writes the calibration result: `VALUE = round(40e6 * MAX / f_sel)`. At 136 kHz, 1024 cycles
    /// read 301176. XTAL32K times out instead, because the board has no 32 kHz crystal.
    fn calibrate(&mut self) {
        let cfg = self.regs.get(Self::CALICFG);
        let max = u64::from(cfg >> CALI_MAX & 0x7FFF);
        let clk_sel = cfg >> CALI_CLK_SEL & 3;
        let timeout = clk_sel == 2;
        self.regs
            .set_field(Self::CALICFG2, CALI_TIMEOUT, 1, u32::from(timeout));
        if timeout {
            self.regs.set_field(Self::CALICFG, CALI_RDY, 1, 0);
            self.regs.set_field(Self::CALICFG1, CALI_VALUE, 25, 0);
            self.regs.set_field(Self::CALICFG1, CALI_DATA_VLD, 1, 0);
            return;
        }
        let hz = match clk_sel {
            1 => RC_FAST_D256_HZ,
            _ => self.slow_hz,
        };
        let value = (XTAL_HZ * max + hz / 2) / hz;
        self.regs
            .set_field(Self::CALICFG1, CALI_VALUE, 25, value as u32);
        // `CYCLING_DATA_VLD` follows `START_CYCLING` (UNVERIFIED); the reference 1024-cycle read
        // 0x024C3C00 has it clear, the state `rtc_clk_cal_internal` leaves.
        let cycling = cfg >> CALI_START_CYCLING & 1;
        self.regs
            .set_field(Self::CALICFG1, CALI_DATA_VLD, 1, cycling);
        self.regs.set_field(Self::CALICFG, CALI_RDY, 1, 1);
    }

    fn drive_sources(&mut self, ports: &mut Ports) {
        let st = self.regs.get(Self::INT_RAW) & self.regs.get(Self::INT_ENA) & (INT_T0 | INT_WDT);
        self.regs.set(Self::INT_ST, st);
        ports.irq.set_source(G::T0_SOURCE, st & INT_T0 != 0);
        ports.irq.set_source(G::WDT_SOURCE, st & INT_WDT != 0);
    }
}

fn reset_wiring(cause: ResetCause) -> Wiring {
    match ResetKind::of(cause) {
        Some(kind) => Wiring::ChipReset(kind),
        // Unreachable: every cause a stage raises is documented. Refuse to invent a reset rather
        // than panic inside a peripheral.
        None => Wiring::None,
    }
}

fn elapsed(now: VTime, epoch: VTime, tick_ps: u64) -> u64 {
    now.0.saturating_sub(epoch.0) / tick_ps.max(1)
}

impl<G: Group> Peripheral for Model<G> {
    const ID: PeriphId = G::ID;
    const BASE: u32 = G::BASE;
    const SIZE: u32 = G::SIZE;

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
        // A write that changes a source level ends the CPU block, and so does one that arms an
        // event earlier than every pending one, as SYSTIMER does.
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
        self.on_tag(tag, &mut Ports::of(cx))
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.regs.class_at(off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::reset::ResetScope;
    use pemu_core::sched::{Owner, Scheduler};

    use crate::intc::IrqFabric;

    struct Harness {
        now: VTime,
        sched: Scheduler,
        irq: IrqFabric,
        ledger: FidelityLedger,
        resets: Vec<ResetCause>,
    }

    impl Harness {
        fn new() -> Harness {
            Harness {
                now: VTime(0),
                sched: Scheduler::new(),
                irq: IrqFabric::new(),
                ledger: FidelityLedger::default(),
                resets: Vec::new(),
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

        fn run_to<G: Group>(&mut self, model: &mut Model<G>, t: VTime) {
            while let Some(next) = self.sched.next_time() {
                if next > t {
                    break;
                }
                self.now = next;
                while let Some(key) = self.sched.pop_due(self.now) {
                    assert_eq!(key.owner, Owner::Periph(G::ID));
                    let now = self.now;
                    let wiring = model.on_tag(key.tag, &mut self.ports());
                    if let Wiring::ChipReset(kind) = wiring {
                        self.resets.push(kind.cause);
                        let _ = now;
                    }
                }
            }
            self.now = t;
        }
    }

    fn off(idx: usize) -> u32 {
        u32::from(regs_timg0::REGS[idx].off)
    }

    fn wdt_config0(stages: [u32; WDT_STAGES], enable: bool) -> u32 {
        let mut v = 1 << WDT_USE_XTAL;
        for (n, action) in stages.iter().enumerate() {
            v |= (action & 3) << (WDT_STG0 - 2 * n as u8);
        }
        if enable {
            v |= 1 << WDT_EN;
        }
        v
    }

    /// `wdt_hal` bring-up: prescaler 20000 on the crystal, so one tick is 500 us.
    fn setup_wdt<G: Group>(
        model: &mut Model<G>,
        h: &mut Harness,
        stages: [u32; WDT_STAGES],
        holds: [u32; WDT_STAGES],
    ) {
        model.store(
            off(Model::<G>::WPROTECT),
            Size::B4,
            WDT_WKEY,
            &mut h.ports(),
        );
        model.store(
            off(Model::<G>::CONFIG1),
            Size::B4,
            20_000 << WDT_CLK_PRESCALE,
            &mut h.ports(),
        );
        for (n, hold) in holds.iter().enumerate() {
            model.store(
                off(Model::<G>::CONFIG2 + n),
                Size::B4,
                *hold,
                &mut h.ports(),
            );
        }
        model.store(off(Model::<G>::INT_ENA), Size::B4, INT_WDT, &mut h.ports());
        model.store(
            off(Model::<G>::CONFIG0),
            Size::B4,
            wdt_config0(stages, true),
            &mut h.ports(),
        );
    }

    #[test]
    fn one_thousand_twenty_four_slow_cycles_read_301176() {
        // At 136 kHz, the 1024 cycles `select_rtc_slow_clk` asks for read 301176, from which
        // `rtc_clk_cal` derives 3855053 (0x3AD2CD), Q13.19 us per slow tick.
        let mut h = Harness::new();
        let mut model = Timg0Model::default();
        let cfg = (1024 << CALI_MAX) | (0 << CALI_CLK_SEL) | (1 << CALI_START);
        model.store(off(Model::<Timg0>::CALICFG), Size::B4, cfg, &mut h.ports());

        let status = model.load(off(Model::<Timg0>::CALICFG), Size::B4, &mut h.ports());
        assert_eq!(
            status >> CALI_RDY & 1,
            1,
            "RDY is set inside the start write"
        );
        let value = model.load(off(Model::<Timg0>::CALICFG1), Size::B4, &mut h.ports());
        assert_eq!(value >> CALI_VALUE & 0x1FF_FFFF, 301_176);
        assert_eq!(value, 0x024C_3C00, "the whole RTCCALICFG1 read");
        assert_eq!(value >> CALI_DATA_VLD & 1, 0, "bit 0 is CYCLING_DATA_VLD");

        // The Q13.19 period IDF computes from it (rtc_time.c:146-160).
        let period = ((301_176u64 << 19) + 20_480 - 1) / 40_960;
        assert_eq!(period, 3_855_053);

        // 100 cycles, the `calibrate_ocode` call.
        let cfg = (100 << CALI_MAX) | (1 << CALI_START);
        model.store(off(Model::<Timg0>::CALICFG), Size::B4, 0, &mut h.ports());
        model.store(off(Model::<Timg0>::CALICFG), Size::B4, cfg, &mut h.ports());
        let value = model.load(off(Model::<Timg0>::CALICFG1), Size::B4, &mut h.ports());
        assert_eq!(value >> CALI_VALUE & 0x1FF_FFFF, 29_412);
    }

    /// The register resets to a cycling calibration with RDY clear (the `probe_campaign_regs`
    /// capture reads RTCCALICFG 0x00013000 and RTCCALICFG2 0xFFFFFF98). `rtc_clk_cal_internal`
    /// writes TIMEOUT_THRES 1 and waits for RDY or TIMEOUT; the one-off calibration after it
    /// gives RDY and a nonzero result.
    #[test]
    fn the_cycling_calibration_of_reset_sets_no_rdy_and_times_out_at_a_threshold_write() {
        let reset = regs_timg0::REGS[regs_timg0::idx::TIMG_RTCCALICFG].reset;
        assert_eq!(reset, 0x0001_3000);
        assert_eq!(reset >> CALI_START_CYCLING & 1, 1);

        let mut h = Harness::new();
        let mut model = Timg0Model::default();
        let (cfg, cfg1, cfg2) = (
            off(Model::<Timg0>::CALICFG),
            off(Model::<Timg0>::CALICFG1),
            off(Model::<Timg0>::CALICFG2),
        );
        assert_eq!(
            model.load(cfg, Size::B4, &mut h.ports()),
            0x0001_3000,
            "RDY clear"
        );
        assert_eq!(
            model.load(cfg2, Size::B4, &mut h.ports()),
            0xFFFF_FF98,
            "TIMEOUT clear"
        );

        // rtc_clk_cal_internal: TIMEOUT_THRES 1, then the poll ends on TIMEOUT.
        let thres = model.load(cfg2, Size::B4, &mut h.ports()) & 0x7F | 1 << 7;
        model.store(cfg2, Size::B4, thres, &mut h.ports());
        let status = model.load(cfg2, Size::B4, &mut h.ports());
        assert_eq!(
            status >> CALI_TIMEOUT & 1,
            1,
            "the cycling calibration timed out"
        );

        // The one-off calibration: CLK_SEL 0, START_CYCLING clear, MAX 1024, START.
        let one_off = 1024 << CALI_MAX;
        model.store(cfg, Size::B4, one_off, &mut h.ports());
        model.store(cfg, Size::B4, one_off | 1 << CALI_START, &mut h.ports());
        let status = model.load(cfg, Size::B4, &mut h.ports());
        assert_eq!(
            status >> CALI_RDY & 1,
            1,
            "RDY after the one-off calibration"
        );
        assert_eq!(
            model.load(cfg2, Size::B4, &mut h.ports()) >> CALI_TIMEOUT & 1,
            0
        );
        let value = model.load(cfg1, Size::B4, &mut h.ports());
        assert_eq!(
            value >> CALI_VALUE & 0x1FF_FFFF,
            301_176,
            "and the result is not zero"
        );

        // A threshold write once a result is ready times nothing out.
        model.store(cfg2, Size::B4, thres, &mut h.ports());
        assert_eq!(
            model.load(cfg2, Size::B4, &mut h.ports()) >> CALI_TIMEOUT & 1,
            0
        );
    }

    #[test]
    fn the_missing_32k_crystal_makes_the_calibration_time_out() {
        let mut h = Harness::new();
        let mut model = Timg0Model::default();
        let cfg = (1024 << CALI_MAX) | (2 << CALI_CLK_SEL) | (1 << CALI_START);
        model.store(off(Model::<Timg0>::CALICFG), Size::B4, cfg, &mut h.ports());
        let status = model.load(off(Model::<Timg0>::CALICFG), Size::B4, &mut h.ports());
        assert_eq!(status >> CALI_RDY & 1, 0, "RDY stays clear");
        let timeout = model.load(off(Model::<Timg0>::CALICFG2), Size::B4, &mut h.ports());
        assert_eq!(timeout >> CALI_TIMEOUT & 1, 1);
        let value = model.load(off(Model::<Timg0>::CALICFG1), Size::B4, &mut h.ports());
        assert_eq!(value >> CALI_VALUE & 0x1FF_FFFF, 0);
    }

    #[test]
    fn the_write_protect_key_gates_the_watchdog_registers() {
        // CONFIG0 to 5 and FEED need the key 0x50D83AA1 in WDTWPROTECT.
        assert_eq!(
            regs_timg0::REGS[regs_timg0::idx::TIMG_WDTWPROTECT].reset,
            WDT_WKEY,
            "the register resets to the key, so the bootloader finds it unlocked"
        );
        let mut h = Harness::new();
        let mut model = Timg0Model::default();
        model.store(off(Model::<Timg0>::CONFIG2), Size::B4, 1234, &mut h.ports());
        assert_eq!(
            model.load(off(Model::<Timg0>::CONFIG2), Size::B4, &mut h.ports()),
            1234
        );

        model.store(off(Model::<Timg0>::WPROTECT), Size::B4, 0, &mut h.ports());
        model.store(off(Model::<Timg0>::CONFIG2), Size::B4, 5678, &mut h.ports());
        assert_eq!(
            model.load(off(Model::<Timg0>::CONFIG2), Size::B4, &mut h.ports()),
            1234,
            "a locked group ignores the write and keeps its value"
        );

        model.store(
            off(Model::<Timg0>::WPROTECT),
            Size::B4,
            WDT_WKEY,
            &mut h.ports(),
        );
        model.store(off(Model::<Timg0>::CONFIG2), Size::B4, 5678, &mut h.ports());
        assert_eq!(
            model.load(off(Model::<Timg0>::CONFIG2), Size::B4, &mut h.ports()),
            5678
        );
    }

    #[test]
    fn the_interrupt_watchdog_interrupts_then_resets_the_system() {
        // The interrupt watchdog: MWDT1, prescaler 20000 on the crystal, stage 0 600 ticks
        // interrupt (300 ms), stage 1 1200 ticks system reset, source 35, cause 0x08.
        const TICK_US: u64 = 500;
        let mut h = Harness::new();
        let mut model = Timg1Model::default();
        setup_wdt(&mut model, &mut h, [1, 3, 0, 0], [600, 1200, 0, 0]);
        assert_eq!(model.wdt_stage(), Some(0));

        h.run_to(&mut model, VTime(299 * 1_000_000_000));
        assert!(!h.irq.source(irq::TG1_WDT), "stage 0 is 300 ms");
        h.run_to(&mut model, VTime(600 * TICK_US * 1_000_000));
        assert!(h.irq.source(irq::TG1_WDT), "stage 0 raises source 35");
        assert_eq!(model.wdt_stage(), Some(1));
        assert!(h.resets.is_empty());

        // Stage 1 runs its own hold from the stage-0 expiry and then resets the system.
        h.run_to(&mut model, VTime((600 + 1199) * TICK_US * 1_000_000));
        assert!(h.resets.is_empty());
        h.run_to(&mut model, VTime((600 + 1200) * TICK_US * 1_000_000));
        assert_eq!(h.resets, vec![ResetCause::TG1WDT_SYS], "cause 0x08");
        assert_eq!(ResetCause::TG1WDT_SYS.0, 0x08);
    }

    /// A hold written below the count trips at the next tick, not at the write, so IDF's tick
    /// hook (holds, then a feed after a 2 s spin) does not fire stage 0 in between. Without the
    /// feed the stage fires one tick later.
    #[test]
    fn a_hold_written_below_the_count_trips_at_the_next_tick() {
        const TICK_PS: u64 = 500 * 1_000_000;
        for feed in [true, false] {
            let mut h = Harness::new();
            let mut model = Timg1Model::default();
            setup_wdt(&mut model, &mut h, [1, 3, 0, 0], [10_000, 20_000, 0, 0]);
            // 2 s and a quarter tick into a 5 s hold, the count is 4000 ticks.
            h.run_to(&mut model, VTime(2_000 * 1_000_000_000 + TICK_PS / 4));
            model.store(
                off(Timg1Model::WPROTECT),
                Size::B4,
                WDT_WKEY,
                &mut h.ports(),
            );
            model.store(off(Timg1Model::CONFIG2), Size::B4, 600, &mut h.ports());
            assert_eq!(
                h.sched.next_time(),
                Some(VTime(2_000 * 1_000_000_000 + TICK_PS)),
                "the next tick"
            );
            if feed {
                model.store(off(Timg1Model::FEED), Size::B4, 1, &mut h.ports());
            }
            h.run_to(&mut model, VTime(2_000 * 1_000_000_000 + 2 * TICK_PS));
            assert_eq!(h.irq.source(irq::TG1_WDT), !feed, "feed {feed}");
        }
    }

    #[test]
    fn the_task_watchdog_uses_timg0_source_33_and_its_own_reset_causes() {
        // The task watchdog: MWDT0, stage 0 10000 ticks interrupt on source 33.
        let mut h = Harness::new();
        let mut model = Timg0Model::default();
        setup_wdt(&mut model, &mut h, [1, 2, 0, 0], [10_000, 20_000, 0, 0]);
        h.run_to(&mut model, VTime(5 * 1_000_000_000_000));
        assert!(h.irq.source(irq::TG0_WDT), "10000 ticks is 5 s");
        assert!(!h.irq.source(irq::TG1_WDT), "the other group is untouched");

        // The handler clears the status, and stage 1 asks for a CPU reset, cause 0x0B.
        model.store(
            off(Model::<Timg0>::CONFIG0) + 0x34,
            Size::B4,
            INT_WDT,
            &mut h.ports(),
        );
        h.run_to(&mut model, VTime(15 * 1_000_000_000_000));
        assert_eq!(h.resets, vec![ResetCause::TG0WDT_CPU]);
        assert_eq!(ResetCause::TG0WDT_CPU.0, 0x0B);
    }

    /// The stage interrupt is latched, so a fault envelope can say which watchdog fired after the
    /// handler has cleared the status and panicked.
    #[test]
    fn the_stage_interrupt_is_latched_until_the_watchdog_is_fed_again() {
        let mut h = Harness::new();
        let mut model = Timg0Model::default();
        setup_wdt(&mut model, &mut h, [1, 0, 0, 0], [10_000, 0, 0, 0]);
        assert_eq!(model.wdt_interrupt_fired(), None, "nothing has fired yet");
        h.run_to(&mut model, VTime(5 * 1_000_000_000_000));
        assert!(h.irq.source(irq::TG0_WDT));
        let fired = model.wdt_interrupt_fired().expect("the stage interrupt");
        assert_eq!(fired, (VTime(5 * 1_000_000_000_000), true), "when it fired");

        // `task_wdt_isr` clears the status before it panics; the latch outlives that.
        model.store(
            off(Model::<Timg0>::CONFIG0) + 0x34,
            Size::B4,
            INT_WDT,
            &mut h.ports(),
        );
        assert!(!h.irq.source(irq::TG0_WDT), "the status is cleared");
        assert_eq!(model.wdt_interrupt_fired(), Some(fired), "the latch is not");

        model.store(off(Model::<Timg0>::FEED), Size::B4, 1, &mut h.ports());
        assert_eq!(
            model.wdt_interrupt_fired(),
            Some((fired.0, false)),
            "a fed watchdog is no longer the outstanding fault, but it is still what fired: the \
             ESP-IDF panic handler feeds both watchdogs before it reports anything"
        );

        // Disabling the watchdog ends the outstanding expiry too, but what fired survives: the
        // panic handler disables both watchdogs. Only a block reset forgets it.
        h.run_to(&mut model, VTime(h.now.0 + 5 * 1_000_000_000_000));
        let second = model.wdt_interrupt_fired().expect("the second expiry");
        assert!(second.1 && second.0 > fired.0);
        model.store(
            off(Model::<Timg0>::CONFIG0),
            Size::B4,
            wdt_config0([1, 0, 0, 0], false),
            &mut h.ports(),
        );
        assert_eq!(model.wdt_interrupt_fired(), Some((second.0, false)));
        model.reset_block(
            ResetKind::of(ResetCause::POWERON).expect("a documented cause"),
            &mut h.ports(),
        );
        assert_eq!(model.wdt_interrupt_fired(), None, "a new boot forgets it");
    }

    #[test]
    fn feeding_the_watchdog_restarts_stage_zero() {
        // Any write to WDTFEED restarts at stage 0. The FreeRTOS tick hook feeds the interrupt
        // watchdog every tick, which is why a blocked ISR trips it.
        const TICK_PS: u64 = 500 * 1_000_000;
        let mut h = Harness::new();
        let mut model = Timg1Model::default();
        setup_wdt(&mut model, &mut h, [1, 3, 0, 0], [600, 1200, 0, 0]);
        for feed in 1..=5u64 {
            h.run_to(&mut model, VTime(feed * 500 * TICK_PS));
            assert!(!h.irq.source(irq::TG1_WDT), "fed before stage 0 expires");
            model.store(off(Model::<Timg1>::FEED), Size::B4, 1, &mut h.ports());
            assert_eq!(model.wdt_stage(), Some(0));
            assert_eq!(model.wdt_count(h.now), 0);
        }
        h.run_to(&mut model, VTime(h.now.0 + 600 * TICK_PS));
        assert!(h.irq.source(irq::TG1_WDT));
    }

    #[test]
    fn a_disabled_watchdog_counts_nothing() {
        let mut h = Harness::new();
        let mut model = Timg1Model::default();
        setup_wdt(&mut model, &mut h, [1, 3, 0, 0], [600, 1200, 0, 0]);
        model.store(
            off(Model::<Timg1>::CONFIG0),
            Size::B4,
            wdt_config0([1, 3, 0, 0], false),
            &mut h.ports(),
        );
        assert_eq!(model.wdt_stage(), None);
        h.run_to(&mut model, VTime(10 * 1_000_000_000_000));
        assert!(!h.irq.source(irq::TG1_WDT));
        assert_eq!(h.resets, Vec::new());
        let flashboot = model.load(off(Model::<Timg1>::CONFIG0), Size::B4, &mut h.ports());
        assert_eq!(
            flashboot >> WDT_FLASHBOOT_MOD_EN & 1,
            0,
            "the setup cleared it"
        );
    }

    #[test]
    fn the_stage_actions_decode_as_idf_wdt_types_lists_them() {
        assert_eq!(StageAction::of(0), StageAction::Off);
        assert_eq!(StageAction::of(1), StageAction::Interrupt);
        assert_eq!(StageAction::of(2), StageAction::CpuReset);
        assert_eq!(StageAction::of(3), StageAction::SystemReset);
    }

    /// A write that arms an event earlier than every pending one ends the slice; one that stores
    /// a value or arms a later event does not.
    #[test]
    fn a_write_that_arms_a_sooner_event_ends_the_slice() {
        use crate::intc::testing::TestPorts;
        let mut p = TestPorts::new();
        let mut model = Timg0Model::default();
        let mut w = |p: &mut TestPorts, idx: usize, val: u32| {
            p.with(|cx| Peripheral::write(&mut model, off(idx), Size::B4, val, cx).stop)
        };
        // The watchdog, stage 0 an interrupt after 200 ticks of 500 us: 100 ms, the first event.
        assert!(!w(&mut p, Timg0Model::WPROTECT, WDT_WKEY), "the key");
        assert!(!w(&mut p, Timg0Model::CONFIG1, 20_000 << WDT_CLK_PRESCALE));
        assert!(!w(&mut p, Timg0Model::CONFIG2, 200), "a stored hold");
        assert!(
            w(&mut p, Timg0Model::CONFIG0, wdt_config0([1, 0, 0, 0], true)),
            "the first event ends it"
        );
        // T0 at 1 us a tick off the crystal, its alarm at 1 s: later than the watchdog.
        assert!(!w(&mut p, regs_timg0::idx::TIMG_T0ALARMLO, 1_000_000));
        let config = (1 << T0_USE_XTAL)
            | (40 << T0_DIVIDER)
            | (1 << T0_INCREASE)
            | (1 << T0_ALARM_EN)
            | (1 << T0_EN);
        assert!(
            !w(&mut p, Timg0Model::T0CONFIG, config),
            "an alarm after the pending event does not end it"
        );
        // The alarm moved to 10 us: sooner than 100 ms.
        assert!(
            w(&mut p, regs_timg0::idx::TIMG_T0ALARMLO, 10),
            "a sooner alarm ends it"
        );
    }

    #[test]
    fn t0_counts_at_its_divider_and_alarms_with_autoreload() {
        let mut h = Harness::new();
        let mut model = Timg0Model::default();
        model.store(
            off(regs_timg0::idx::TIMG_T0ALARMLO),
            Size::B4,
            40,
            &mut h.ports(),
        );
        model.store(
            off(regs_timg0::idx::TIMG_T0LOADLO),
            Size::B4,
            0,
            &mut h.ports(),
        );
        model.store(
            off(Model::<Timg0>::INT_ENA),
            Size::B4,
            INT_T0,
            &mut h.ports(),
        );
        let config = (1 << T0_USE_XTAL)
            | (40 << T0_DIVIDER)
            | (1 << T0_AUTORELOAD)
            | (1 << T0_INCREASE)
            | (1 << T0_ALARM_EN)
            | (1 << T0_EN);
        model.store(
            off(Model::<Timg0>::T0CONFIG),
            Size::B4,
            config,
            &mut h.ports(),
        );
        assert_eq!(model.timer_value(VTime(1_000_000)), 1, "one tick per us");
        assert_eq!(model.timer_value(VTime(10_000_000)), 10);

        h.now = VTime(10_000_000);
        model.store(
            off(regs_timg0::idx::TIMG_T0UPDATE),
            Size::B4,
            1 << T0_UPDATE,
            &mut h.ports(),
        );
        assert_eq!(
            model.load(off(regs_timg0::idx::TIMG_T0LO), Size::B4, &mut h.ports()),
            10
        );

        h.run_to(&mut model, VTime(40_000_000));
        assert!(h.irq.source(irq::TG0_T0), "the alarm fired at 40 us");
        let config = model.load(off(Model::<Timg0>::T0CONFIG), Size::B4, &mut h.ports());
        assert_eq!(config >> T0_ALARM_EN & 1, 0, "ALARM_EN self-clears");
        assert_eq!(
            model.timer_value(h.now),
            0,
            "AUTORELOAD reloaded from T0LOAD"
        );
    }

    #[test]
    fn every_register_shares_one_reset_domain_and_a_reset_restores_the_block() {
        let domain = regs_timg0::REGS[regs_timg0::idx::TIMG_WDTCONFIG0].domain;
        for spec in regs_timg0::REGS.iter().chain(regs_timg1::REGS.iter()) {
            assert_eq!(spec.domain, domain, "{}", spec.name);
        }
        for scope in ResetScope::ALL {
            assert!(pemu_core::regstore::resets_in(domain, scope));
        }

        let mut h = Harness::new();
        let mut model = Timg1Model::default();
        setup_wdt(&mut model, &mut h, [1, 3, 0, 0], [600, 1200, 0, 0]);
        h.run_to(&mut model, VTime(300 * 1_000_000_000));
        assert!(h.irq.source(irq::TG1_WDT));

        let sys = ResetKind::of(ResetCause::RTC_SW_SYS).expect("documented cause");
        model.reset_block(sys, &mut h.ports());
        assert!(!h.irq.source(irq::TG1_WDT));
        assert_eq!(model.wdt_stage(), None);
        assert_eq!(h.sched.len(), 0, "the pending stage is cancelled");
        assert_eq!(
            model.load(off(Model::<Timg1>::WPROTECT), Size::B4, &mut h.ports()),
            WDT_WKEY
        );
        let status = model.load(off(Model::<Timg1>::CALICFG), Size::B4, &mut h.ports());
        assert_eq!(
            status, 0x0001_3000,
            "the calibration is back at its reset value"
        );
    }

    #[test]
    fn a_gated_clock_skips_the_gated_interval() {
        let mut h = Harness::new();
        let mut model = Timg0Model::default();
        model.store(
            off(regs_timg0::idx::TIMG_T0ALARMLO),
            Size::B4,
            40,
            &mut h.ports(),
        );
        let config = (1 << T0_USE_XTAL)
            | (40 << T0_DIVIDER)
            | (1 << T0_INCREASE)
            | (1 << T0_ALARM_EN)
            | (1 << T0_EN);
        model.store(
            off(Model::<Timg0>::T0CONFIG),
            Size::B4,
            config,
            &mut h.ports(),
        );
        h.run_to(&mut model, VTime(10_000_000));
        let alarm = h.sched.next_time().expect("the alarm is armed");
        let gate = 3_000_000_000_000;
        model.clock_gated_for(gate);
        h.sched.postpone(gate, |_| true);
        assert_eq!(model.timer_value(VTime(10_000_000 + gate)), 10);
        assert_eq!(h.sched.next_time(), Some(VTime(alarm.0 + gate)));
    }
}
