//! RTC_CNTL (`specs/blocks/rtc_cntl.toml`): the only block in the RTC power domain, which is
//! what makes its reset behavior different.
//!
//! A `CORE_` reset spares the RTC domain, so the reset cause, the eight `STORE` words and the RTC
//! time counter survive into the next boot; a `SYS_` reset clears all but the cause. The ROM
//! banner's `rst:` comes from `RESET_STATE.RESET_CAUSE_PROCPU`, and `esp_reset_reason` reads the
//! hint the previous boot left in `STORE6` ([`Model::reset_to`]).
//!
//! RTC time is one counter at [`SLOW_HZ`]. `TIME_UPDATE` latches it at once: the C3 has no valid
//! bit to poll (`rtc_cntl_ll.h:74-80`). The RWDT and the TIMG `RTCCALI` value must use the same
//! rate, or `gettimeofday` across a sleep and an RWDT timeout disagree. The counter restarts only
//! where the RTC domain is reset (power-on and `SYS_`); the `probe_campaign_reset` capture settles
//! cause 0x12 that way (the app reads 51 ms after a super-watchdog reset). 0x0F and 0x10 follow
//! by class, UNVERIFIED.
//!
//! The RWDT is a four-stage counter on the slow clock behind the `WDTWPROTECT` key, each stage's
//! hold a scheduled event and its action an interrupt or reset cause 0x0D, 0x09 or 0x10. A
//! power-on or `SYS_` reset arms flash-boot protection, which runs stage 0 as a reset, so an image
//! that never feeds it resets after about 2.94 s; the bootloader clears `FLASHBOOT_MOD_EN` first.
//!
//! The super watchdog, while armed ([`Model::swd_active`]), resets the chip with cause 0x12
//! [`SWD_TIMEOUT_PS`] after it was armed (the `probe_campaign_reset` capture: auto-feed off, no
//! feed). Only the three enable bits the ROM and IDF touch are modeled.
//!
//! Sleep, wakeup and brownout live in `periph/rtc_sleep.rs`; here they are storage with a seam
//! each way. Source 27 follows [`Model::irq_level`]; a read changes no status bit.

use pemu_core::fidelity::{Fidelity, FidelityLedger, TouchAccess};
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::{ResetCause, ResetKind};
use pemu_core::sched::{EventHandle, EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::block::RtcCntl;
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};
use crate::r#gen::DOMAIN_CHIP_SYSTEM;
use crate::r#gen::regs_rtc_cntl::{REG_COUNT, REGS, idx};
use crate::regs::{self, TouchAt};

/// The RC_SLOW frequency every slow-clock consumer shares: RTC time, the RWDT, the sleep timer and
/// the TIMG `RTCCALI` result (IDF `clk_tree_defs.h:40` `SOC_CLK_RC_SLOW_FREQ_APPROX`).
pub const SLOW_HZ: u64 = 136_000;

pub const IRQ_SOURCE: IrqSource = irq::RTC_CORE;

/// `OPTIONS0.SW_PROCPU_RST`: the `esp_restart` path, reset cause 0x0C.
const OPTIONS0_SW_PROCPU_RST: u32 = 1 << 5;

/// `OPTIONS0.SW_APPCPU_RST`: no second core on the C3, so the write is only cleared.
const OPTIONS0_SW_APPCPU_RST: u32 = 1 << 4;

/// `OPTIONS0.SW_SYS_RST`: ROM `software_reset`, reset cause 0x03.
const OPTIONS0_SW_SYS_RST: u32 = 1 << 31;

const OPTIONS0_WO: u32 = OPTIONS0_SW_APPCPU_RST | OPTIONS0_SW_PROCPU_RST | OPTIONS0_SW_SYS_RST;

const TIME_UPDATE: u32 = 1 << 31;

/// `SLOW_CLK_CONF.SLOW_CLK_NEXT_EDGE`, which clears itself at the next slow-clock edge
/// ([`TAG_SLOW_EDGE`]).
const SLOW_CLK_NEXT_EDGE: u32 = 1 << 31;

/// The write-only bits of `STATE0`: a software RTC interrupt request and the reject-cause clear.
/// Both are storage here.
const STATE0_WO: u32 = (1 << 0) | (1 << 1);

const OPTION1_FORCE_DOWNLOAD_BOOT: u32 = 1 << 0;

const RESET_CAUSE_MASK: u32 = 0x3F;

/// `INT_RAW.WDT`: the RWDT stage action "interrupt".
pub const INT_WDT: u32 = 1 << 3;

/// `INT_RAW.SWD`: the super-watchdog feed interrupt.
pub const INT_SWD: u32 = 1 << 15;

/// Key `WDTWPROTECT` must hold before a write reaches a `WDTCONFIG` register or `WDTFEED`
/// (`rwdt_ll.h`). The header default is the key itself, so the RWDT starts unlocked.
pub const WDT_WKEY: u32 = 0x50D8_3AA1;

pub const SWD_WKEY: u32 = 0x8F1D_312A;

const WDT_EN: u32 = 1 << 31;

/// `WDTCONFIG0.FLASHBOOT_MOD_EN`: the flash-boot protection armed by a reset.
const WDT_FLASHBOOT_MOD_EN: u32 = 1 << 12;
/// `WDTCONFIG0.PAUSE_IN_SLP`: the counter stands still while the chip sleeps.
const WDT_PAUSE_IN_SLP: u32 = 1 << 9;

/// Shift of stage `n`'s action field in `WDTCONFIG0`: STG0 bit 28, STG1 25, STG2 22, STG3 19.
const fn wdt_stage_shift(stage: u8) -> u32 {
    28 - 3 * stage as u32
}

const WDT_FEED: u32 = 1 << 31;

/// `SWD_CONF.SWD_FEED`, write-only. Not modeled: no ROM or IDF path writes it, nor the feed
/// interrupt about 100 ms before the timeout, which only auto-feed answers. So auto-feed holds the
/// count at 0 here and arming starts a full timeout.
const SWD_FEED: u32 = 1 << 29;

const SWD_RST_FLAG_CLR: u32 = 1 << 28;

const SWD_WO: u32 = SWD_FEED | SWD_RST_FLAG_CLR;

const SWD_DISABLE: u32 = 1 << 30;

/// `SWD_CONF.SWD_AUTO_FEED_EN`, which the bootloader sets and nothing clears
/// (`bootloader_esp32c3.c:79-84`).
const SWD_AUTO_FEED_EN: u32 = 1 << 31;

/// `SWD_CONF.SWD_BYPASS_RST`: the timeout does not reset the chip
/// (`bootloader_ana_super_wdt_reset_config`).
const SWD_BYPASS_RST: u32 = 1 << 17;

/// `SWD_CONF.SWD_RESET_FLAG`, read-only, set by a super-watchdog reset and cleared by
/// `SWD_RST_FLAG_CLR` (TRM 12.3.2.2). The ROM's `clear_super_wdt_reset_flag` writes it without the
/// key, which lands because the reset left `SWD_WPROTECT` at the key. Whether the flag is set at
/// all is UNVERIFIED: the ROM clears it before app code can read it.
const SWD_RESET_FLAG: u32 = 1 << 0;

/// `FIB_SEL.FIB_SUPER_WDT_RST`: while set (the reset value), the super watchdog's reset enable
/// comes from the fixed default rather than `SWD_BYPASS_RST`. The bootloader clears it so
/// `SWD_BYPASS_RST` decides. The ROM never arms the watchdog and a chip does not reset out of a
/// long ROM download wait, so the default reads as "bypassed". UNVERIFIED as a mechanism.
const FIB_SUPER_WDT_RST: u32 = 1 << 2;

/// How long an armed super watchdog runs before it resets the chip, in picoseconds.
///
/// Class B from the `probe_campaign_reset` capture (three runs): each reset falls between the last
/// store before it and the longest store gap after that, and the three midpoints average 3.356 s,
/// spread 1 %. The TRM says "slightly less than one second", which this chip does not show. Kept
/// as a time rather than slow-clock cycles: one chip, one temperature.
pub const SWD_TIMEOUT_PS: u64 = 3_355_000_000_000;

const TAG_RWDT: u16 = 0;
/// The software CPU reset request, due [`SW_CPU_RESET_LATENCY_PS`] after the write.
const TAG_SW_CPU_RESET: u16 = 1;
/// The slow-clock edge that clears a `SLOW_CLK_NEXT_EDGE` request.
///
/// Class A from the `probe_campaign_timing` capture: the request stays set 508 to 1208 CPU cycles
/// and always clears, about one slow period. The model places the edge on the RTC counter's own
/// ticks ([`Model::rtc_ticks`]). No handle is kept: the event only clears the bit, and a stale one
/// falls on an edge of the same grid.
const TAG_SLOW_EDGE: u16 = 2;
const TAG_SWD: u16 = 3;

/// How long after the `OPTIONS0.SW_PROCPU_RST` write the CPU reset takes effect.
///
/// Class A lower bound from the `probe_stack` capture: after `esp_restart_noos` calls ROM
/// `software_reset_cpu`, the next banner reads `Saved PC` at the `j .` right after the call, so the
/// ROM function's `ret` and the jump ran first. One microsecond is UNVERIFIED beyond that bound.
pub const SW_CPU_RESET_LATENCY_PS: u64 = 1_000_000;

/// What an RWDT stage does when its hold expires.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum WdtAction {
    /// 0: the stage is off, so the watchdog stops here.
    Off,
    /// 1: `INT_RAW.WDT` and IRQ source 27.
    Interrupt,
    /// 2: CPU reset, cause 0x0D.
    ResetCpu,
    /// 3: system reset, cause 0x09.
    ResetSystem,
    /// 4: RTC reset, cause 0x10. Values above 4 are UNVERIFIED and treated as this one.
    ResetRtc,
}

impl WdtAction {
    pub const fn of(field: u32) -> WdtAction {
        match field & 0x7 {
            0 => WdtAction::Off,
            1 => WdtAction::Interrupt,
            2 => WdtAction::ResetCpu,
            3 => WdtAction::ResetSystem,
            _ => WdtAction::ResetRtc,
        }
    }

    pub fn cause(self) -> Option<ResetCause> {
        match self {
            WdtAction::Off | WdtAction::Interrupt => None,
            WdtAction::ResetCpu => Some(ResetCause::RTCWDT_CPU),
            WdtAction::ResetSystem => Some(ResetCause::RTCWDT_SYS),
            WdtAction::ResetRtc => Some(ResetCause::RTCWDT_RTC),
        }
    }
}

const TOUCH_WORDS: usize = REG_COUNT.div_ceil(64);

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    #[serde(with = "regs_serde")]
    regs: RegStore<REG_COUNT>,
    touched: [u64; TOUCH_WORDS],
    time_epoch: VTime,
    time_base: u64,
    wdt_stage: u8,
    /// Virtual time the current RWDT stage started. Only `WDTFEED`, a `WDTCONFIG0` write and a
    /// re-arming reset restart the count, so a hold write reschedules against this instant, and
    /// the count survives the `esp_restart_noos` CPU reset.
    wdt_start: VTime,
    wdt_event: Option<EventHandle>,
    /// The pending software CPU reset. Any reset that reaches this block first cancels it.
    sw_cpu_reset_event: Option<EventHandle>,
    /// The pending super-watchdog timeout while armed. A write that leaves it armed keeps the
    /// deadline.
    swd_event: Option<EventHandle>,
    /// Flash-boot protection armed by the last reset, until the guest clears `FLASHBOOT_MOD_EN`.
    wdt_flashboot: bool,
    /// `EFUSE_WDT_DELAY_SEL`, which scales the stage 0 hold (`rwdt_ll.h:119`). The machine sets it
    /// from the eFuse image; the device value is 0.
    wdt_delay_sel: u8,
    /// Rate of the slow clock in Hz: [`SLOW_HZ`] until the machine applies its profile's
    /// `rtc_slow_hz`. Not serialized: the machine re-applies configuration after a restore.
    #[serde(skip, default = "default_slow_hz")]
    slow_hz: u64,
}

fn default_slow_hz() -> u64 {
    SLOW_HZ
}

impl Default for Model {
    fn default() -> Self {
        Model {
            regs: RegStore::new(&REGS),
            touched: [0; TOUCH_WORDS],
            time_epoch: VTime(0),
            time_base: 0,
            wdt_stage: 0,
            wdt_start: VTime(0),
            wdt_event: None,
            sw_cpu_reset_event: None,
            swd_event: None,
            wdt_flashboot: false,
            wdt_delay_sel: 0,
            slow_hz: SLOW_HZ,
        }
    }
}

/// RTC ticks in `ps` picoseconds of the slow clock. `u128` because a run of hours times
/// [`SLOW_HZ`] leaves 64 bits.
pub const fn ticks_in(ps: u64) -> u64 {
    ticks_in_at(SLOW_HZ, ps)
}

pub const fn ticks_in_at(hz: u64, ps: u64) -> u64 {
    ((ps as u128 * hz as u128) / 1_000_000_000_000u128) as u64
}

/// Picoseconds `ticks` of the slow clock take, saturating.
///
/// Rounds up, so `ticks_in(ps_of(n)) >= n`: a deadline is never one tick early. [`ticks_in`]
/// rounds down, as a counter read does.
pub const fn ps_of(ticks: u64) -> u64 {
    ps_of_at(SLOW_HZ, ticks)
}

pub const fn ps_of_at(hz: u64, ticks: u64) -> u64 {
    let ps = (ticks as u128 * 1_000_000_000_000u128).div_ceil(hz as u128);
    if ps > u64::MAX as u128 {
        u64::MAX
    } else {
        ps as u64
    }
}

impl Model {
    pub fn rtc_ticks(&self, now: VTime) -> u64 {
        let dt = now.0.saturating_sub(self.time_epoch.0);
        self.time_base.saturating_add(ticks_in_at(self.slow_hz, dt)) & 0xFFFF_FFFF_FFFF
    }

    pub fn ps_of_ticks(&self, ticks: u64) -> u64 {
        ps_of_at(self.slow_hz, ticks)
    }

    pub fn slow_hz(&self) -> u64 {
        self.slow_hz
    }

    /// Sets the slow clock rate from the profile's `rtc_slow_hz`. The machine calls it before the
    /// first instruction and after a restore with the same value. A rate of 0 is ignored.
    pub fn set_slow_hz(&mut self, hz: u64) {
        if hz != 0 {
            self.slow_hz = hz;
        }
    }

    /// The reset cause the last reset left in `RESET_STATE` (the ROM's `rst:`).
    pub fn reset_cause(&self) -> ResetCause {
        ResetCause((self.regs.get(idx::RTC_CNTL_RESET_STATE) & RESET_CAUSE_MASK) as u8)
    }

    /// `OPTION1.FORCE_DOWNLOAD_BOOT`: the ROM takes the download path at the next system reset.
    pub fn force_download_boot(&self) -> bool {
        self.regs.get(idx::RTC_CNTL_OPTION1) & OPTION1_FORCE_DOWNLOAD_BOOT != 0
    }

    pub fn store_word(&self, n: usize) -> Option<u32> {
        let i = match n {
            0..=3 => idx::RTC_CNTL_STORE0 + n,
            4..=7 => idx::RTC_CNTL_STORE4 + (n - 4),
            _ => return None,
        };
        Some(self.regs.get(i))
    }

    pub fn irq_level(&self) -> bool {
        self.regs.get(idx::RTC_CNTL_INT_ST) != 0
    }

    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    /// Applies a reset: the registers of the scopes it clears, the cause it leaves, and the RTC
    /// counter. The cause is written whatever the fan-out, because a `CPU0_` reset that reaches
    /// no digital block still makes the banner print `rst:0xc`.
    pub fn reset_to(&mut self, kind: ResetKind, now: VTime, sched: &mut Scheduler) {
        if let Some(handle) = self.sw_cpu_reset_event.take() {
            sched.cancel(handle);
        }
        if kind.fanout.reaches_all_blocks() {
            self.regs.reset(kind.scope);
        }
        let state = self.regs.get(idx::RTC_CNTL_RESET_STATE) & !RESET_CAUSE_MASK;
        self.regs.set(
            idx::RTC_CNTL_RESET_STATE,
            state | u32::from(kind.cause.0) & RESET_CAUSE_MASK,
        );
        // The counter and the watchdog state restart only where the RTC domain is reset.
        if kind.clears(DOMAIN_CHIP_SYSTEM) {
            self.time_base = 0;
            self.time_epoch = now;
            self.wdt_flashboot = true;
            self.wdt_stage = 0;
            self.wdt_start = now;
        }
        // The flag the super-watchdog reset leaves for the ROM, set after the register reset
        // above so the reset that raised it does not clear it.
        if kind.cause == ResetCause::SUPER_WDT {
            let conf = self.regs.get(idx::RTC_CNTL_SWD_CONF) | SWD_RESET_FLAG;
            self.regs.set(idx::RTC_CNTL_SWD_CONF, conf);
        }
        self.refresh_int_st();
        self.rearm_wdt(now, sched);
        // A reset that restored the watchdog's registers restarts it; one that kept them (a
        // `CORE_` or `CPU0_` reset) keeps its deadline, as the RWDT's.
        if kind.clears(DOMAIN_CHIP_SYSTEM)
            && let Some(handle) = self.swd_event.take()
        {
            sched.cancel(handle);
        }
        self.rearm_swd(now, sched);
    }

    /// Sets `EFUSE_WDT_DELAY_SEL`, which scales the RWDT stage 0 hold (`rwdt_ll.h:119`). The
    /// machine hands it over from the eFuse image.
    pub fn set_wdt_delay_sel(&mut self, sel: u8, now: VTime, sched: &mut Scheduler) {
        self.wdt_delay_sel = sel & 0x3;
        self.rearm_wdt(now, sched);
    }

    pub fn wdt_delay_sel(&self) -> u8 {
        self.wdt_delay_sel
    }

    /// Sets `EFUSE_WDT_DELAY_SEL` without rearming: an export clears it and a restore puts the
    /// machine's own value back, while the snapshot's RWDT event stays as scheduled.
    pub fn restore_wdt_delay_sel(&mut self, sel: u8) {
        self.wdt_delay_sel = sel & 0x3;
    }

    pub fn wdt_stage(&self) -> Option<(u8, WdtAction)> {
        self.wdt_event?;
        Some((self.wdt_stage, self.wdt_action(self.wdt_stage)))
    }

    /// Whether the super watchdog is armed: its reset enabled (`FIB_SUPER_WDT_RST` and
    /// `SWD_BYPASS_RST` clear), not disabled and not auto-fed.
    pub fn swd_active(&self) -> bool {
        let conf = self.regs.get(idx::RTC_CNTL_SWD_CONF);
        let fib = self.regs.get(idx::RTC_CNTL_FIB_SEL);
        fib & FIB_SUPER_WDT_RST == 0
            && conf & (SWD_BYPASS_RST | SWD_DISABLE | SWD_AUTO_FEED_EN) == 0
    }

    pub fn swd_deadline(&self, sched: &Scheduler) -> Option<VTime> {
        self.swd_event.and_then(|h| sched.time_of(h))
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((i, byte)) = regs::reg_at(&REGS, off) else {
            regs::hole(off, TouchAccess::Read, at, ledger);
            return 0;
        };
        regs::touch(&mut self.touched, &REGS, i, TouchAccess::Read, at, ledger);
        self.regs.read(i, byte, size)
    }

    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        sched: &mut Scheduler,
        ledger: &mut FidelityLedger,
    ) -> Wiring {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((i, byte)) = regs::reg_at(&REGS, off) else {
            regs::hole(off, TouchAccess::Write, at, ledger);
            return Wiring::None;
        };
        regs::touch(&mut self.touched, &REGS, i, TouchAccess::Write, at, ledger);
        if self.write_protected(i) {
            return Wiring::None;
        }
        let before = self.regs.get(i);
        self.regs.write(i, byte, size, val);
        self.after_write(i, before, now, sched)
    }

    /// Whether the key register that guards `i` lacks its key, so the write is dropped. Every IDF
    /// and esptool path writes the key first.
    fn write_protected(&self, i: usize) -> bool {
        match i {
            idx::RTC_CNTL_WDTCONFIG0
            | idx::RTC_CNTL_WDTCONFIG1
            | idx::RTC_CNTL_WDTCONFIG2
            | idx::RTC_CNTL_WDTCONFIG3
            | idx::RTC_CNTL_WDTCONFIG4
            | idx::RTC_CNTL_WDTFEED => self.regs.get(idx::RTC_CNTL_WDTWPROTECT) != WDT_WKEY,
            idx::RTC_CNTL_SWD_CONF => self.regs.get(idx::RTC_CNTL_SWD_WPROTECT) != SWD_WKEY,
            _ => false,
        }
    }

    fn after_write(&mut self, i: usize, before: u32, now: VTime, sched: &mut Scheduler) -> Wiring {
        match i {
            idx::RTC_CNTL_WDTCONFIG0 => {
                if self.regs.get(i) & WDT_FLASHBOOT_MOD_EN == 0 {
                    self.wdt_flashboot = false;
                }
                self.restart_wdt(now, sched);
            }
            // A hold write changes the deadline of the stage already counting, not the count:
            // otherwise a guest rewriting a hold more often than the hold would feed the watchdog.
            idx::RTC_CNTL_WDTCONFIG1
            | idx::RTC_CNTL_WDTCONFIG2
            | idx::RTC_CNTL_WDTCONFIG3
            | idx::RTC_CNTL_WDTCONFIG4 => self.rearm_wdt(now, sched),
            idx::RTC_CNTL_WDTFEED => {
                self.clear_bits(i, WDT_FEED);
                self.restart_wdt(now, sched);
            }
            idx::RTC_CNTL_SWD_CONF => {
                if self.regs.get(i) & SWD_RST_FLAG_CLR != 0 {
                    self.clear_bits(i, SWD_RESET_FLAG);
                }
                self.clear_bits(i, SWD_WO);
                self.rearm_swd(now, sched);
            }
            // `FIB_SUPER_WDT_RST` decides whether `SWD_BYPASS_RST` applies, so it can arm or
            // disarm the super watchdog.
            idx::RTC_CNTL_FIB_SEL => self.rearm_swd(now, sched),
            idx::RTC_CNTL_OPTIONS0 => return self.software_reset(now, sched),
            idx::RTC_CNTL_TIME_UPDATE => self.latch_time(now),
            idx::RTC_CNTL_SLOW_CLK_CONF => {
                // `rtc_clk_wait_for_slow_cycle` waits for this to clear at the next slow edge.
                if self.regs.get(i) & SLOW_CLK_NEXT_EDGE != 0 {
                    self.arm_slow_edge(now, sched);
                }
            }
            idx::RTC_CNTL_STATE0 => {
                self.clear_bits(i, STATE0_WO);
                return super::rtc_sleep::on_state0_write(self, before);
            }
            // A write with `GPIO_WAKEUP_STATUS_CLR` set clears which pad woke the chip.
            idx::RTC_CNTL_GPIO_WAKEUP => return super::rtc_sleep::on_gpio_wakeup_write(self),
            idx::RTC_CNTL_INT_ENA => self.refresh_int_st(),
            idx::RTC_CNTL_INT_CLR => {
                let mask = self.regs.get(i);
                self.clear_bits(i, mask);
                let raw = self.regs.get(idx::RTC_CNTL_INT_RAW) & !mask;
                self.regs.set(idx::RTC_CNTL_INT_RAW, raw);
                self.refresh_int_st();
                super::rtc_sleep::reassert_brownout(self);
            }
            idx::RTC_CNTL_INT_ENA_W1TS | idx::RTC_CNTL_INT_ENA_W1TC => {
                let mask = self.regs.get(i);
                self.clear_bits(i, mask);
                let ena = self.regs.get(idx::RTC_CNTL_INT_ENA);
                let ena = if i == idx::RTC_CNTL_INT_ENA_W1TS {
                    ena | mask
                } else {
                    ena & !mask
                };
                self.regs.set(idx::RTC_CNTL_INT_ENA, ena);
                self.refresh_int_st();
            }
            _ => {}
        }
        Wiring::None
    }

    /// `OPTIONS0` carries the two software reset requests, which read 0. The system reset is
    /// immediate; the CPU reset follows [`SW_CPU_RESET_LATENCY_PS`] later.
    fn software_reset(&mut self, now: VTime, sched: &mut Scheduler) -> Wiring {
        let written = self.regs.get(idx::RTC_CNTL_OPTIONS0) & OPTIONS0_WO;
        self.clear_bits(idx::RTC_CNTL_OPTIONS0, OPTIONS0_WO);
        // The wider reset wins if a guest sets both in one word.
        let cause = if written & OPTIONS0_SW_SYS_RST != 0 {
            ResetCause::RTC_SW_SYS
        } else if written & OPTIONS0_SW_PROCPU_RST != 0 {
            let key = EventKey {
                owner: Owner::Periph(<Model as Peripheral>::ID),
                tag: TAG_SW_CPU_RESET,
            };
            if self.sw_cpu_reset_event.is_none() {
                self.sw_cpu_reset_event = Some(sched.schedule(
                    now,
                    VTime(now.0.saturating_add(SW_CPU_RESET_LATENCY_PS)),
                    key,
                ));
            }
            return Wiring::None;
        } else {
            return Wiring::None;
        };
        match ResetKind::of(cause) {
            Some(kind) => Wiring::ChipReset(kind),
            None => Wiring::None,
        }
    }

    fn latch_time(&mut self, now: VTime) {
        let update = self.regs.get(idx::RTC_CNTL_TIME_UPDATE) & TIME_UPDATE;
        self.clear_bits(idx::RTC_CNTL_TIME_UPDATE, TIME_UPDATE);
        if update == 0 {
            return;
        }
        let ticks = self.rtc_ticks(now);
        self.regs
            .set(idx::RTC_CNTL_TIME_LOW0, (ticks & 0xFFFF_FFFF) as u32);
        self.regs
            .set(idx::RTC_CNTL_TIME_HIGH0, ((ticks >> 32) & 0xFFFF) as u32);
    }

    fn wdt_action(&self, stage: u8) -> WdtAction {
        let conf0 = self.regs.get(idx::RTC_CNTL_WDTCONFIG0);
        if conf0 & WDT_EN == 0 {
            // Flash-boot protection: stage 0 is a reset whatever the STG fields say. Which reset
            // class is UNVERIFIED; the system reset is what an unattended boot's stage uses.
            return if stage == 0 {
                WdtAction::ResetSystem
            } else {
                WdtAction::Off
            };
        }
        WdtAction::of(conf0 >> wdt_stage_shift(stage))
    }

    /// Hold of RWDT stage `stage` in slow-clock ticks. Only stage 0 is scaled by
    /// `1 + EFUSE_WDT_DELAY_SEL` (`rwdt_ll.h:119`).
    fn wdt_hold(&self, stage: u8) -> u64 {
        let i = match stage {
            0 => idx::RTC_CNTL_WDTCONFIG1,
            1 => idx::RTC_CNTL_WDTCONFIG2,
            2 => idx::RTC_CNTL_WDTCONFIG3,
            _ => idx::RTC_CNTL_WDTCONFIG4,
        };
        let hold = u64::from(self.regs.get(i));
        if stage == 0 {
            hold << (1 + u32::from(self.wdt_delay_sel))
        } else {
            hold
        }
    }

    fn wdt_running(&self) -> bool {
        let conf0 = self.regs.get(idx::RTC_CNTL_WDTCONFIG0);
        conf0 & WDT_EN != 0 || (self.wdt_flashboot && conf0 & WDT_FLASHBOOT_MOD_EN != 0)
    }

    fn restart_wdt(&mut self, now: VTime, sched: &mut Scheduler) {
        self.wdt_stage = 0;
        self.wdt_start = now;
        self.rearm_wdt(now, sched);
    }

    /// Cancels the pending stage timeout and schedules the current stage's, skipping stages that
    /// do nothing or hold for no ticks. The deadline is measured from [`Model::wdt_start`], so a
    /// hold write keeps the elapsed time and a `CPU0_` reset keeps it too. A hold shortened below
    /// the elapsed time is clamped to `now` by the scheduler.
    fn rearm_wdt(&mut self, now: VTime, sched: &mut Scheduler) {
        if let Some(handle) = self.wdt_event.take() {
            sched.cancel(handle);
        }
        if !self.wdt_running() {
            return;
        }
        while self.wdt_stage < 4 {
            let hold = self.wdt_hold(self.wdt_stage);
            if hold > 0 && self.wdt_action(self.wdt_stage) != WdtAction::Off {
                let at = VTime(self.wdt_start.0.saturating_add(self.ps_of_ticks(hold)));
                let key = EventKey {
                    owner: Owner::Periph(<Model as Peripheral>::ID),
                    tag: TAG_RWDT,
                };
                self.wdt_event = Some(sched.schedule(now, at, key));
                return;
            }
            self.wdt_stage += 1;
        }
    }

    /// Schedules the super-watchdog timeout when the watchdog has just become armed and cancels it
    /// when it no longer is. An armed watchdog keeps its deadline.
    fn rearm_swd(&mut self, now: VTime, sched: &mut Scheduler) {
        match (self.swd_active(), self.swd_event) {
            (true, None) => {
                let key = EventKey {
                    owner: Owner::Periph(<Model as Peripheral>::ID),
                    tag: TAG_SWD,
                };
                let at = VTime(now.0.saturating_add(SWD_TIMEOUT_PS));
                self.swd_event = Some(sched.schedule(now, at, key));
            }
            (false, Some(handle)) => {
                sched.cancel(handle);
                self.swd_event = None;
            }
            _ => {}
        }
    }

    /// The chip sleeps for `ps` from `now`: with `PAUSE_IN_SLP` the RWDT's current stage ends `ps`
    /// later. `esp_light_sleep_start` arms a 1 s RTC-reset stage with the bit set around every
    /// light sleep, and a 2 s sleep would otherwise reset the chip.
    pub fn wdt_sleep_for(&mut self, ps: u64, now: VTime, sched: &mut Scheduler) {
        if self.regs.get(idx::RTC_CNTL_WDTCONFIG0) & WDT_PAUSE_IN_SLP == 0 || !self.wdt_running() {
            return;
        }
        self.wdt_start = VTime(self.wdt_start.0.saturating_add(ps));
        self.rearm_wdt(now, sched);
    }

    /// Schedules the clear of a `SLOW_CLK_NEXT_EDGE` request at the next tick of
    /// [`Model::rtc_ticks`].
    fn arm_slow_edge(&mut self, now: VTime, sched: &mut Scheduler) {
        let dt = now.0.saturating_sub(self.time_epoch.0);
        let next = ticks_in_at(self.slow_hz, dt).saturating_add(1);
        let at = VTime(
            self.time_epoch
                .0
                .saturating_add(ps_of_at(self.slow_hz, next)),
        );
        sched.schedule(
            now,
            at,
            EventKey {
                owner: Owner::Periph(<Model as Peripheral>::ID),
                tag: TAG_SLOW_EDGE,
            },
        );
    }

    pub fn on_event_tag(&mut self, tag: u16, now: VTime, sched: &mut Scheduler) -> Wiring {
        match tag {
            TAG_RWDT => self.on_wdt_timeout(now, sched),
            TAG_SLOW_EDGE => {
                self.clear_bits(idx::RTC_CNTL_SLOW_CLK_CONF, SLOW_CLK_NEXT_EDGE);
                Wiring::None
            }
            TAG_SW_CPU_RESET => {
                self.sw_cpu_reset_event = None;
                ResetKind::of(ResetCause::RTC_SW_CPU).map_or(Wiring::None, Wiring::ChipReset)
            }
            TAG_SWD => {
                self.swd_event = None;
                ResetKind::of(ResetCause::SUPER_WDT).map_or(Wiring::None, Wiring::ChipReset)
            }
            _ => Wiring::None,
        }
    }

    pub fn on_wdt_timeout(&mut self, now: VTime, sched: &mut Scheduler) -> Wiring {
        self.wdt_event = None;
        let action = self.wdt_action(self.wdt_stage);
        self.wdt_stage = self.wdt_stage.saturating_add(1);
        self.wdt_start = now;
        if let Some(cause) = action.cause() {
            // The reset re-arms the watchdog through `reset_to`, so nothing is scheduled here.
            if let Some(kind) = ResetKind::of(cause) {
                return Wiring::ChipReset(kind);
            }
        }
        if action == WdtAction::Interrupt {
            self.raise_int(INT_WDT);
        }
        self.rearm_wdt(now, sched);
        Wiring::None
    }

    /// Sets raw interrupt bits from the hardware side and recomputes `INT_ST`. Without a `Cx`,
    /// callers outside the entry points drive the level themselves.
    pub fn raise_int(&mut self, mask: u32) {
        let raw = self.regs.get(idx::RTC_CNTL_INT_RAW) | mask;
        self.regs.set(idx::RTC_CNTL_INT_RAW, raw);
        self.refresh_int_st();
    }

    /// Sets register `i` from the hardware side, bypassing its access semantics, and recomputes
    /// `INT_ST`: the wake latch and the brownout detector use it.
    pub fn hw_set(&mut self, i: usize, value: u32) {
        self.regs.set(i, value);
        self.refresh_int_st();
    }

    fn refresh_int_st(&mut self) {
        let st = self.regs.get(idx::RTC_CNTL_INT_RAW) & self.regs.get(idx::RTC_CNTL_INT_ENA);
        self.regs.set(idx::RTC_CNTL_INT_ST, st);
    }

    fn clear_bits(&mut self, i: usize, mask: u32) {
        let v = self.regs.get(i) & !mask;
        self.regs.set(i, v);
    }
}

impl Peripheral for Model {
    const ID: PeriphId = <RtcCntl as Block>::ID;
    const BASE: u32 = <RtcCntl as Block>::BASE;
    const SIZE: u32 = <RtcCntl as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.reset_to(kind, cx.now, cx.sched);
        cx.irq.set_source(IRQ_SOURCE, self.irq_level());
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let wiring = self.store(off, size, val, cx.now, cx.sched, cx.ledger);
        cx.irq.set_source(IRQ_SOURCE, self.irq_level());
        // A next-edge request schedules its clear within one slow period, so the slice ends.
        let edge = regs::reg_at(&REGS, off).is_some_and(|(i, _)| i == idx::RTC_CNTL_SLOW_CLK_CONF)
            && self.regs.get(idx::RTC_CNTL_SLOW_CLK_CONF) & SLOW_CLK_NEXT_EDGE != 0;
        RegWrite { stop: edge, wiring }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        let wiring = self.on_event_tag(tag, cx.now, cx.sched);
        cx.irq.set_source(IRQ_SOURCE, self.irq_level());
        wiring
    }

    /// Every register is written rather than counted, so a read is stable until the machine or
    /// the guest acts, except the interrupt registers, which a watchdog event sets.
    fn stable_until(&self, off: u32, cx: &Cx) -> Stability {
        let _ = cx;
        match regs::reg_at(&REGS, off).map(|(i, _)| i) {
            Some(idx::RTC_CNTL_INT_RAW | idx::RTC_CNTL_INT_ST | idx::RTC_CNTL_SLOW_CLK_CONF) => {
                Stability::UntilNextEvent
            }
            _ => Stability::UntilInput,
        }
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        regs::reg_at(&REGS, off).map_or(Fidelity::U, |(i, _)| REGS[i].class)
    }
}

crate::regs::store_serde!();

#[cfg(test)]
mod tests {
    use super::*;

    const OFF_OPTIONS0: u32 = 0x000;
    const OFF_TIME_UPDATE: u32 = 0x00C;
    const OFF_TIME_LOW0: u32 = 0x010;
    const OFF_TIME_HIGH0: u32 = 0x014;
    const OFF_STATE0: u32 = 0x018;
    const OFF_RESET_STATE: u32 = 0x038;
    const OFF_INT_ENA: u32 = 0x040;
    const OFF_INT_RAW: u32 = 0x044;
    const OFF_INT_ST: u32 = 0x048;
    const OFF_INT_CLR: u32 = 0x04C;
    const OFF_STORE0: u32 = 0x050;
    const OFF_SLOW_CLK_CONF: u32 = 0x074;
    const OFF_WDTCONFIG0: u32 = 0x090;
    const OFF_WDTCONFIG1: u32 = 0x094;
    const OFF_WDTCONFIG2: u32 = 0x098;
    const OFF_WDTCONFIG4: u32 = 0x0A0;
    const OFF_WDTFEED: u32 = 0x0A4;
    const OFF_WDTWPROTECT: u32 = 0x0A8;
    const OFF_SWD_CONF: u32 = 0x0AC;
    const OFF_SWD_WPROTECT: u32 = 0x0B0;
    const OFF_STORE6: u32 = 0x0C0;
    const OFF_OPTION1: u32 = 0x0F4;
    const OFF_FIB_SEL: u32 = 0x10C;
    const OFF_INT_ENA_W1TS: u32 = 0x100;
    const OFF_INT_ENA_W1TC: u32 = 0x104;

    const T: VTime = VTime(0);

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
                now: T,
            }
        }

        fn read(&mut self, off: u32) -> u32 {
            self.m.load(off, Size::B4, self.now, &mut self.l)
        }

        fn write(&mut self, off: u32, val: u32) -> Wiring {
            self.m
                .store(off, Size::B4, val, self.now, &mut self.s, &mut self.l)
        }

        fn reset(&mut self, cause: ResetCause) {
            let kind = ResetKind::of(cause).expect("a documented reset cause");
            self.m.reset_to(kind, self.now, &mut self.s);
        }

        fn run_to_next_event(&mut self) -> Option<Wiring> {
            let at = self.s.next_time()?;
            self.now = at;
            let key = self.s.pop_due(at)?;
            assert_eq!(key.owner, Owner::Periph(<Model as Peripheral>::ID));
            Some(self.m.on_event_tag(key.tag, self.now, &mut self.s))
        }

        fn unlock_wdt(&mut self) {
            self.write(OFF_WDTWPROTECT, WDT_WKEY);
        }
    }

    fn cause_of(wiring: &Wiring) -> Option<ResetCause> {
        match wiring {
            Wiring::ChipReset(kind) => Some(kind.cause),
            _ => None,
        }
    }

    #[test]
    fn identity_matches_the_c3_devices_row() {
        assert_eq!(<Model as Peripheral>::ID, <RtcCntl as Block>::ID);
        assert_eq!(<Model as Peripheral>::BASE, 0x6000_8000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x800);
    }

    /// Every cause the emulator raises reaches `RESET_CAUSE_PROCPU`, including the `CPU0_` one.
    #[test]
    fn the_reset_cause_reaches_reset_state_for_every_fan_out() {
        let mut h = Harness::new();
        for cause in [
            ResetCause::POWERON,
            ResetCause::RTC_SW_SYS,
            ResetCause::RTC_SW_CPU,
            ResetCause::USB_UART_CHIP,
            ResetCause::DEEPSLEEP,
        ] {
            h.reset(cause);
            assert_eq!(h.m.reset_cause(), cause);
            assert_eq!(
                h.read(OFF_RESET_STATE) & RESET_CAUSE_MASK,
                u32::from(cause.0),
                "{cause:?}"
            );
        }
        assert_eq!(h.read(OFF_RESET_STATE) & 0x3000, 0x3000);
    }

    /// A `CORE_` reset keeps the RTC configuration and the `STORE` words; a `SYS_` reset and a
    /// power-on clear both (after the device's super-watchdog reset IDF's RTC time restarts, which
    /// it does only when STORE1 reads 0).
    #[test]
    fn reset_scope_matrix_over_the_rtc_domain() {
        let mut h = Harness::new();
        let write_all = |h: &mut Harness| {
            h.write(OFF_STORE0, 0xA5A5_0000);
            h.write(OFF_STORE6, 0x8000_1234);
            h.write(OFF_INT_ENA, INT_WDT);
        };

        write_all(&mut h);
        h.reset(ResetCause::RTC_SW_SYS);
        assert_eq!(h.read(OFF_INT_ENA), INT_WDT);
        assert_eq!(h.m.store_word(0), Some(0xA5A5_0000));
        assert_eq!(h.m.store_word(6), Some(0x8000_1234));

        for cause in [ResetCause::RTCWDT_RTC, ResetCause::SUPER_WDT] {
            write_all(&mut h);
            h.reset(cause);
            assert_eq!(h.read(OFF_INT_ENA), 0, "{cause:?}");
            assert_eq!(h.m.store_word(0), Some(0), "{cause:?}");
            assert_eq!(h.m.store_word(6), Some(0), "{cause:?}");
        }

        write_all(&mut h);
        h.reset(ResetCause::POWERON);
        assert_eq!(h.m.store_word(0), Some(0));
        assert_eq!(h.m.store_word(6), Some(0));

        write_all(&mut h);
        h.reset(ResetCause::RTC_SW_CPU);
        assert_eq!(h.read(OFF_INT_ENA), INT_WDT);
        assert_eq!(h.m.store_word(6), Some(0x8000_1234));
    }

    #[test]
    fn every_store_word_is_readable_and_indexed() {
        let mut h = Harness::new();
        for n in 0..8u32 {
            let off = if n < 4 {
                OFF_STORE0 + n * 4
            } else {
                0x0B8 + (n - 4) * 4
            };
            h.write(off, 0x1000 + n);
            assert_eq!(h.m.store_word(n as usize), Some(0x1000 + n), "STORE{n}");
        }
        assert_eq!(h.m.store_word(8), None);
    }

    #[test]
    fn time_update_latches_the_counter_at_the_slow_clock_rate() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        assert_eq!(h.m.rtc_ticks(VTime(0)), 0);
        h.now = VTime(1_000_000_000_000);
        assert_eq!(h.m.rtc_ticks(h.now), SLOW_HZ);

        assert_eq!(h.read(OFF_TIME_LOW0), 0);
        h.write(OFF_TIME_UPDATE, TIME_UPDATE);
        assert_eq!(h.read(OFF_TIME_LOW0), SLOW_HZ as u32);
        assert_eq!(h.read(OFF_TIME_HIGH0), 0);
        assert_eq!(h.read(OFF_TIME_UPDATE), 0);

        h.now = VTime(2_000_000_000_000);
        h.write(OFF_TIME_UPDATE, 1 << 27);
        assert_eq!(h.read(OFF_TIME_LOW0), SLOW_HZ as u32);
    }

    #[test]
    fn the_counter_is_48_bits_wide() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        h.now = VTime(ps_of(0x1_0000_0001));
        h.write(OFF_TIME_UPDATE, TIME_UPDATE);
        assert_eq!(h.read(OFF_TIME_HIGH0), 1);
        assert!(h.m.rtc_ticks(h.now) >= 0x1_0000_0000);
        assert_eq!(ticks_in(ps_of(1_000)), 1_000, "the conversions agree");
    }

    /// The counter restarts at a power-on and a `SYS_` reset and keeps running across a `CORE_`
    /// and a `CPU0_` one.
    #[test]
    fn the_counter_restarts_only_on_a_chip_or_system_reset() {
        let mut h = Harness::new();
        h.now = VTime(1_000_000_000_000);
        h.reset(ResetCause::RTC_SW_SYS);
        assert_eq!(h.m.rtc_ticks(h.now), SLOW_HZ, "a core reset keeps it");
        h.reset(ResetCause::RTC_SW_CPU);
        assert_eq!(h.m.rtc_ticks(h.now), SLOW_HZ, "a CPU reset keeps it");
        h.reset(ResetCause::RTCWDT_RTC);
        assert_eq!(h.m.rtc_ticks(h.now), 0, "an RTC-domain reset clears it");
    }

    #[test]
    fn the_software_reset_bits_produce_a_reset_and_read_zero() {
        let mut h = Harness::new();
        let wiring = h.write(OFF_OPTIONS0, OPTIONS0_SW_PROCPU_RST);
        assert!(
            matches!(wiring, Wiring::None),
            "the CPU reset is not at once"
        );
        assert_eq!(h.read(OFF_OPTIONS0) & OPTIONS0_WO, 0);
        let written = h.now;
        let wiring = h.run_to_next_event().expect("the reset is scheduled");
        assert_eq!(cause_of(&wiring), Some(ResetCause::RTC_SW_CPU));
        assert_eq!(
            h.now.0 - written.0,
            SW_CPU_RESET_LATENCY_PS,
            "class A: the device ran the ROM's `ret` and the caller's `j .` before the reset"
        );

        // IDF `rtc_cntl_ll.h:61` writes the whole register for a system reset.
        let wiring = h.write(OFF_OPTIONS0, 0xFFFF_FFFF);
        assert_eq!(cause_of(&wiring), Some(ResetCause::RTC_SW_SYS));
        assert_eq!(h.read(OFF_OPTIONS0) & OPTIONS0_WO, 0);

        assert!(matches!(h.write(OFF_OPTIONS0, 0), Wiring::None));

        assert!(matches!(
            h.write(OFF_OPTIONS0, OPTIONS0_SW_PROCPU_RST),
            Wiring::None
        ));
        h.reset(ResetCause::RTCWDT_BROWN_OUT);
        assert!(
            h.s.pending()
                .iter()
                .all(|(_, _, key)| key.tag != TAG_SW_CPU_RESET),
            "the pending CPU reset was cancelled"
        );
    }

    #[test]
    fn int_st_drives_source_27_through_the_entry_points() {
        use crate::intc::testing::TestPorts;
        let mut p = TestPorts::new();
        let mut m = Model::default();
        m.raise_int(INT_WDT);
        assert!(!p.source(IRQ_SOURCE));
        p.with(|cx| Peripheral::write(&mut m, OFF_INT_ENA, Size::B4, INT_WDT, cx));
        assert!(p.source(IRQ_SOURCE), "RAW & ENA asserts source 27");
        p.with(|cx| Peripheral::write(&mut m, OFF_INT_CLR, Size::B4, INT_WDT, cx));
        assert!(!p.source(IRQ_SOURCE), "INT_CLR lowers it");
        m.raise_int(INT_WDT);
        p.with(|cx| Peripheral::write(&mut m, OFF_INT_ENA_W1TS, Size::B4, INT_WDT, cx));
        assert!(p.source(IRQ_SOURCE));
        // RTC_CNTL keeps its registers across a system reset, so this clears `INT_ENA` with a
        // power-on.
        let kind = ResetKind::of(ResetCause::POWERON).expect("a documented reset cause");
        p.with(|cx| Peripheral::reset(&mut m, kind, cx));
        assert!(!p.source(IRQ_SOURCE), "a power-on reset lowers it");
    }

    #[test]
    fn an_rwdt_interrupt_stage_drives_source_27_from_the_event() {
        use crate::intc::testing::TestPorts;
        let mut p = TestPorts::new();
        let mut m = Model::default();
        let power_on = ResetKind::of(ResetCause::POWERON).expect("a documented reset cause");
        p.with(|cx| Peripheral::reset(&mut m, power_on, cx));
        for (off, val) in [
            (OFF_WDTWPROTECT, WDT_WKEY),
            (OFF_WDTCONFIG1, 1_000),
            (OFF_WDTCONFIG0, WDT_EN | (1 << wdt_stage_shift(0))),
            (OFF_INT_ENA, INT_WDT),
        ] {
            p.with(|cx| Peripheral::write(&mut m, off, Size::B4, val, cx));
        }
        assert!(!p.source(IRQ_SOURCE), "stage 0 has not expired");
        let at = p.sched.next_time().expect("stage 0 is scheduled");
        p.now = at;
        let key = p.sched.pop_due(at).expect("the stage event is due");
        let wiring = p.with(|cx| Peripheral::on_event(&mut m, key.tag, cx));
        assert!(
            matches!(wiring, Wiring::None),
            "an interrupt stage resets nothing"
        );
        assert!(
            p.source(IRQ_SOURCE),
            "the expired interrupt stage asserts source 27"
        );
    }

    #[test]
    fn the_interrupt_registers_follow_raw_and_ena() {
        let mut h = Harness::new();
        h.m.raise_int(INT_WDT);
        assert_eq!(h.read(OFF_INT_RAW), INT_WDT);
        assert_eq!(h.read(OFF_INT_ST), 0);
        assert!(!h.m.irq_level());

        h.write(OFF_INT_ENA_W1TS, INT_WDT);
        assert_eq!(h.read(OFF_INT_ENA), INT_WDT);
        assert_eq!(h.read(OFF_INT_ST), INT_WDT);
        assert!(h.m.irq_level());
        assert_eq!(h.read(OFF_INT_ENA_W1TS), 0, "write only");

        h.write(OFF_INT_ENA_W1TC, INT_WDT);
        assert_eq!(h.read(OFF_INT_ENA), 0);
        assert!(!h.m.irq_level());

        h.write(OFF_INT_ENA, INT_WDT);
        assert!(h.m.irq_level());
        h.write(OFF_INT_CLR, 0xFFFF_FFFF);
        assert_eq!(h.read(OFF_INT_RAW), 0);
        assert_eq!(h.read(OFF_INT_CLR), 0, "write only");
        assert!(!h.m.irq_level());
    }

    #[test]
    fn the_slow_clock_edge_request_clears_at_the_next_slow_edge() {
        let mut h = Harness::new();
        let ticks = h.m.rtc_ticks(h.now);
        h.write(OFF_SLOW_CLK_CONF, SLOW_CLK_NEXT_EDGE | 0x40_0000);
        assert_eq!(
            h.read(OFF_SLOW_CLK_CONF),
            SLOW_CLK_NEXT_EDGE | 0x40_0000,
            "still set right after the write"
        );
        let set_at = h.now;
        assert!(h.run_to_next_event().is_some(), "the edge is scheduled");
        assert_eq!(
            h.read(OFF_SLOW_CLK_CONF),
            0x40_0000,
            "clear at the edge, the rest kept"
        );
        assert_eq!(
            h.m.rtc_ticks(h.now),
            ticks + 1,
            "on the counter's next tick"
        );
        assert_eq!(
            h.m.rtc_ticks(VTime(h.now.0 - 1)),
            ticks,
            "and not a picosecond before it"
        );
        let waited = h.now.0 - set_at.0;
        assert!(
            waited > 0 && waited <= h.m.ps_of_ticks(1),
            "within one slow period: {waited} ps"
        );
    }

    #[test]
    fn a_slow_clock_conf_write_without_the_request_schedules_nothing() {
        let mut h = Harness::new();
        h.write(OFF_SLOW_CLK_CONF, 0x40_0000);
        assert!(h.s.next_time().is_none());
        assert_eq!(h.read(OFF_SLOW_CLK_CONF), 0x40_0000);
    }

    #[test]
    fn option1_carries_the_forced_download_flag() {
        let mut h = Harness::new();
        assert!(!h.m.force_download_boot());
        h.write(OFF_OPTION1, OPTION1_FORCE_DOWNLOAD_BOOT);
        assert!(h.m.force_download_boot());
        h.reset(ResetCause::POWERON);
        assert!(!h.m.force_download_boot());
    }

    #[test]
    fn a_sleep_en_write_is_a_sleep_entry_and_schedules_nothing() {
        let mut h = Harness::new();
        let wiring = h.write(OFF_STATE0, 1 << 31);
        assert!(matches!(
            wiring,
            Wiring::SleepEnter(super::super::rtc_sleep::SleepKind::Light)
        ));
        assert_eq!(h.read(OFF_STATE0), 1 << 31);
        h.write(OFF_STATE0, STATE0_WO);
        assert_eq!(h.read(OFF_STATE0) & STATE0_WO, 0);
        assert!(h.s.is_empty(), "sleep schedules no block event");
    }

    /// A power-on arms the RWDT with stage 0 as a reset and the 200000-tick hold scaled by
    /// `1 + WDT_DELAY_SEL`: about 2.94 s at 136 kHz.
    #[test]
    fn the_flash_boot_watchdog_resets_an_image_that_never_feeds_it() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        assert_eq!(h.m.wdt_stage(), Some((0, WdtAction::ResetSystem)));

        let at = h.s.next_time().expect("the flash-boot stage is scheduled");
        assert_eq!(at, VTime(ps_of(200_000 << 1)));
        assert!(
            (2.9e12..3.0e12).contains(&(at.0 as f64)),
            "about 2.94 s, got {} ps",
            at.0
        );

        let wiring = h.run_to_next_event().expect("the stage fires");
        assert_eq!(cause_of(&wiring), Some(ResetCause::RTCWDT_SYS));
    }

    #[test]
    fn clearing_the_flash_boot_bit_stops_the_watchdog() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        assert!(h.m.wdt_stage().is_some());
        h.unlock_wdt();
        h.write(OFF_WDTCONFIG0, 0);
        assert_eq!(h.m.wdt_stage(), None);
        assert!(h.s.is_empty());
    }

    #[test]
    fn a_feed_restarts_the_first_stage() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        h.unlock_wdt();
        h.write(OFF_WDTCONFIG1, 1_000);
        h.write(OFF_WDTCONFIG2, 2_000);
        h.write(
            OFF_WDTCONFIG0,
            WDT_EN | (1 << wdt_stage_shift(0)) | (3 << wdt_stage_shift(1)),
        );
        assert_eq!(h.m.wdt_stage(), Some((0, WdtAction::Interrupt)));
        let first = h.s.next_time().expect("stage 0 is scheduled");
        assert_eq!(first, VTime(ps_of(1_000 << 1)));

        h.now = VTime(first.0 / 2);
        h.write(OFF_WDTFEED, WDT_FEED);
        assert_eq!(h.read(OFF_WDTFEED), 0, "the feed request reads 0");
        assert_eq!(h.m.wdt_stage(), Some((0, WdtAction::Interrupt)));
        assert_eq!(
            h.s.next_time(),
            Some(VTime(h.now.0 + ps_of(1_000 << 1))),
            "the hold restarts from the feed"
        );
    }

    /// A sleep of `d` moves the running stage's deadline by `d` with `PAUSE_IN_SLP`, and not
    /// without it.
    #[test]
    fn a_sleep_pauses_the_count_only_with_pause_in_slp() {
        for pause in [false, true] {
            let mut h = Harness::new();
            h.reset(ResetCause::POWERON);
            h.unlock_wdt();
            h.write(OFF_WDTCONFIG1, 1_000);
            let bit = if pause { WDT_PAUSE_IN_SLP } else { 0 };
            h.write(OFF_WDTCONFIG0, WDT_EN | bit | (4 << wdt_stage_shift(0)));
            let deadline = h.s.next_time().expect("stage 0 is scheduled");
            h.now = VTime(deadline.0 / 4);
            let slept = 2 * deadline.0;
            let now = h.now;
            h.m.wdt_sleep_for(slept, now, &mut h.s);
            let want = if pause {
                deadline.0 + slept
            } else {
                deadline.0
            };
            assert_eq!(h.s.next_time(), Some(VTime(want)), "pause_in_slp={pause}");
        }
    }

    #[test]
    fn a_hold_write_keeps_the_elapsed_time() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        h.unlock_wdt();
        h.write(OFF_WDTCONFIG1, 1_000);
        h.write(OFF_WDTCONFIG0, WDT_EN | (3 << wdt_stage_shift(0)));
        let deadline = VTime(ps_of(1_000 << 1));
        assert_eq!(h.m.wdt_stage(), Some((0, WdtAction::ResetSystem)));
        assert_eq!(h.s.next_time(), Some(deadline));

        h.now = VTime(deadline.0 / 2);
        h.write(OFF_WDTCONFIG4, 0xFFF);
        assert_eq!(
            h.s.next_time(),
            Some(deadline),
            "a stage this one does not use cannot move its deadline"
        );

        h.write(OFF_WDTCONFIG1, 2_000);
        assert_eq!(h.s.next_time(), Some(VTime(ps_of(2_000 << 1))));

        h.write(OFF_WDTCONFIG1, 1);
        assert_eq!(h.s.next_time(), Some(h.now));

        h.write(OFF_WDTCONFIG1, 2_000);
        let wiring = h.run_to_next_event().expect("stage 0 fires");
        assert_eq!(cause_of(&wiring), Some(ResetCause::RTCWDT_SYS));
    }

    /// `esp_restart_noos` arms the RWDT for 1 s and takes a CPU reset, across which the RWDT keeps
    /// running. A `SYS_` reset re-arms it.
    #[test]
    fn a_cpu_reset_keeps_the_watchdog_counting() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        h.unlock_wdt();
        h.write(OFF_WDTCONFIG1, 1_000);
        h.write(OFF_WDTCONFIG0, WDT_EN | (3 << wdt_stage_shift(0)));
        let deadline = VTime(ps_of(1_000 << 1));
        assert_eq!(h.s.next_time(), Some(deadline));

        h.now = VTime(deadline.0 / 2);
        h.reset(ResetCause::RTC_SW_CPU);
        assert_eq!(h.m.reset_cause(), ResetCause::RTC_SW_CPU);
        assert_eq!(
            h.s.next_time(),
            Some(deadline),
            "the elapsed half survives the CPU reset"
        );

        h.reset(ResetCause::RTCWDT_RTC);
        assert_eq!(
            h.s.next_time(),
            Some(VTime(h.now.0 + ps_of(200_000 << 1))),
            "a SYS_ reset restores the header hold and re-arms flash boot"
        );
    }

    #[test]
    fn the_stages_run_in_order_with_their_own_actions() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        h.unlock_wdt();
        h.write(OFF_WDTCONFIG1, 1_000);
        h.write(OFF_WDTCONFIG2, 2_000);
        h.write(OFF_INT_ENA, INT_WDT);
        h.write(
            OFF_WDTCONFIG0,
            WDT_EN | (1 << wdt_stage_shift(0)) | (2 << wdt_stage_shift(1)),
        );

        let wiring = h.run_to_next_event().expect("stage 0 fires");
        assert!(matches!(wiring, Wiring::None));
        assert_eq!(h.read(OFF_INT_RAW) & INT_WDT, INT_WDT);
        assert!(h.m.irq_level(), "an enabled WDT status drives source 27");
        assert_eq!(h.m.wdt_stage(), Some((1, WdtAction::ResetCpu)));

        let wiring = h.run_to_next_event().expect("stage 1 fires");
        assert_eq!(cause_of(&wiring), Some(ResetCause::RTCWDT_CPU));
        assert_eq!(h.m.wdt_stage(), None, "a reset stage is the last one");
    }

    #[test]
    fn the_efuse_delay_selector_scales_only_the_first_stage() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        h.unlock_wdt();
        h.write(OFF_WDTCONFIG1, 1_000);
        h.write(OFF_WDTCONFIG2, 1_000);
        h.write(
            OFF_WDTCONFIG0,
            WDT_EN | (1 << wdt_stage_shift(0)) | (1 << wdt_stage_shift(1)),
        );
        assert_eq!(h.s.next_time(), Some(VTime(ps_of(1_000 << 1))));

        h.m.set_wdt_delay_sel(2, h.now, &mut h.s);
        assert_eq!(h.s.next_time(), Some(VTime(ps_of(1_000 << 3))));

        h.run_to_next_event().expect("stage 0 fires");
        let stage1 = h.s.next_time().expect("stage 1 is scheduled");
        assert_eq!(
            stage1,
            VTime(h.now.0 + ps_of(1_000)),
            "stages 1 to 3 are unscaled"
        );
    }

    #[test]
    fn the_watchdog_registers_need_their_key() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        h.write(OFF_WDTWPROTECT, 0);
        h.write(OFF_WDTCONFIG1, 12_345);
        assert_eq!(h.read(OFF_WDTCONFIG1), 0x0003_0D40, "the header default");
        h.unlock_wdt();
        h.write(OFF_WDTCONFIG1, 12_345);
        assert_eq!(h.read(OFF_WDTCONFIG1), 12_345);
    }

    /// The bootloader's super-watchdog sequence: `FIB_SUPER_WDT_RST` and `SWD_BYPASS_RST` cleared,
    /// then `SWD_AUTO_FEED_EN` set behind the key, with the flash-boot stage stopped first.
    fn bootloader_swd(h: &mut Harness) {
        h.unlock_wdt();
        h.write(OFF_WDTCONFIG0, 0);
        let fib = h.read(OFF_FIB_SEL);
        h.write(OFF_FIB_SEL, fib & !FIB_SUPER_WDT_RST);
        let conf = h.read(OFF_SWD_CONF);
        h.write(OFF_SWD_CONF, conf & !SWD_BYPASS_RST);
        h.write(OFF_SWD_WPROTECT, SWD_WKEY);
        let conf = h.read(OFF_SWD_CONF);
        h.write(OFF_SWD_CONF, conf | SWD_AUTO_FEED_EN);
        h.write(OFF_SWD_WPROTECT, 0);
    }

    /// At the reset values the super watchdog's reset is bypassed, so the ROM stage schedules
    /// nothing; the bootloader's sequence arms it briefly and leaves it fed.
    #[test]
    fn the_super_watchdog_is_armed_only_with_its_reset_enabled_and_no_feed() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        assert!(
            !h.m.swd_active(),
            "the FIB_SEL reset value bypasses the reset"
        );
        assert_eq!(h.m.swd_deadline(&h.s), None);

        h.write(OFF_SWD_WPROTECT, 0);
        h.write(OFF_SWD_CONF, SWD_AUTO_FEED_EN);
        assert_eq!(
            h.read(OFF_SWD_CONF),
            0x04B0_0000,
            "without the key the write is dropped"
        );

        bootloader_swd(&mut h);
        assert!(!h.m.swd_active(), "the bootloader leaves it auto-fed");
        assert_eq!(h.m.swd_deadline(&h.s), None);
        assert_eq!(h.read(OFF_SWD_CONF), 0x84B0_0000, "the device's boot value");

        h.write(OFF_SWD_WPROTECT, SWD_WKEY);
        h.write(OFF_SWD_CONF, SWD_AUTO_FEED_EN | SWD_FEED);
        assert_eq!(h.read(OFF_SWD_CONF) & SWD_WO, 0, "write-only bits read 0");
        h.write(OFF_SWD_CONF, SWD_DISABLE);
        assert!(!h.m.swd_active(), "disabled");
        h.write(OFF_SWD_CONF, SWD_BYPASS_RST);
        assert!(!h.m.swd_active(), "its reset bypassed");
    }

    /// The `probe_campaign_reset` capture: auto-feed off and no feed resets the chip with cause
    /// 0x12 [`SWD_TIMEOUT_PS`] later, a `SYS_` reset that restarts the RTC counter and sets the
    /// flag the ROM clears without the key.
    #[test]
    fn an_unfed_super_watchdog_resets_with_cause_0x12() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        bootloader_swd(&mut h);
        h.now = VTime(5_000_000_000_000);
        h.write(OFF_SWD_WPROTECT, SWD_WKEY);
        h.write(OFF_SWD_CONF, 0x04B0_0000 | SWD_FEED);
        h.write(OFF_SWD_WPROTECT, 0);
        assert!(h.m.swd_active());
        let deadline = VTime(h.now.0 + SWD_TIMEOUT_PS);
        assert_eq!(h.m.swd_deadline(&h.s), Some(deadline));

        h.now = VTime(h.now.0 + 1_000_000_000);
        h.write(OFF_SWD_WPROTECT, SWD_WKEY);
        assert_eq!(h.m.swd_deadline(&h.s), Some(deadline));

        let wiring = h.run_to_next_event().expect("the timeout fires");
        assert_eq!(h.now, deadline);
        assert_eq!(cause_of(&wiring), Some(ResetCause::SUPER_WDT));
        h.reset(ResetCause::SUPER_WDT);
        assert_eq!(h.m.reset_cause(), ResetCause::SUPER_WDT);
        assert_eq!(
            h.read(OFF_SWD_CONF),
            0x04B0_0001,
            "reset values, the flag set"
        );
        assert_eq!(h.m.rtc_ticks(h.now), 0, "the RTC counter restarts");
        assert!(!h.m.swd_active(), "back behind FIB_SEL");

        // ROM `clear_super_wdt_reset_flag` writes bit 28 without the key, which lands because
        // SWD_WPROTECT reset to the key.
        assert_eq!(h.read(OFF_SWD_WPROTECT), SWD_WKEY);
        let conf = h.read(OFF_SWD_CONF);
        h.write(OFF_SWD_CONF, conf | SWD_RST_FLAG_CLR);
        assert_eq!(h.read(OFF_SWD_CONF), 0x04B0_0000, "the flag clear lands");
        bootloader_swd(&mut h);
        assert_eq!(
            h.read(OFF_SWD_CONF),
            0x84B0_0000,
            "the device's SWD|reset swd_conf"
        );
    }

    /// The `probe_campaign_regs` capture read the values an earlier deep sleep left after an RTS
    /// reset (`rst:0x15`): the RTC domain keeps them across a `CORE_` reset and a deep-sleep wake;
    /// a `SYS_` reset restores them.
    #[test]
    fn the_rtc_domain_survives_a_core_reset_and_not_a_sys_reset() {
        let words = [
            (idx::RTC_CNTL_CLK_CONF, 0x20C8_0298),
            (idx::RTC_CNTL_GPIO_WAKEUP, 0x0200_0081),
            (idx::RTC_CNTL_TIMER1, 0x1419_0143),
        ];
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        for (i, v) in words {
            h.m.hw_set(i, v);
        }
        for cause in [
            ResetCause::USB_UART_CHIP,
            ResetCause::DEEPSLEEP,
            ResetCause::RTC_SW_CPU,
        ] {
            h.reset(cause);
            for (i, v) in words {
                assert_eq!(h.m.regs().get(i), v, "{} after {cause:?}", REGS[i].name);
            }
        }
        h.reset(ResetCause::SUPER_WDT);
        for (i, _) in words {
            assert_eq!(
                h.m.regs().get(i),
                REGS[i].reset,
                "{} after 0x12",
                REGS[i].name
            );
        }
    }

    #[test]
    fn a_core_reset_keeps_the_super_watchdog_counting() {
        let mut h = Harness::new();
        h.reset(ResetCause::POWERON);
        bootloader_swd(&mut h);
        h.write(OFF_SWD_WPROTECT, SWD_WKEY);
        h.write(OFF_SWD_CONF, 0x04B0_0000);
        let deadline = h.m.swd_deadline(&h.s).expect("armed");
        h.now = VTime(SWD_TIMEOUT_PS / 2);
        h.reset(ResetCause::USB_UART_CHIP);
        assert_eq!(h.m.swd_deadline(&h.s), Some(deadline));
        h.reset(ResetCause::DEEPSLEEP);
        assert_eq!(h.m.swd_deadline(&h.s), Some(deadline));
    }

    #[test]
    fn fidelity_comes_from_the_generated_table() {
        let m = Model::default();
        assert_eq!(m.fidelity(OFF_RESET_STATE), Fidelity::B);
        assert_eq!(m.fidelity(OFF_STORE6), Fidelity::B);
        assert_eq!(
            m.fidelity(OFF_OPTIONS0),
            Fidelity::B,
            "the software-reset bits are modeled, specs/blocks/rtc_cntl.toml"
        );
        assert_eq!(
            m.fidelity(OFF_SWD_CONF),
            Fidelity::B,
            "the super watchdog resets the chip after a class C timeout, \
             specs/blocks/rtc_cntl.toml"
        );
    }
}
