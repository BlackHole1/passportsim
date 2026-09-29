//! SYSTEM at 0x600C0000: clock and reset enables, the CPU frequency selector, the four software
//! interrupts and the RTC fast-memory checksum (`specs/blocks/system.toml`).
//!
//! A write to `SYSCLK_CONF.SOC_CLK_SEL` or `CPU_PER_CONF.CPUPERIOD_SEL` raises
//! `Wiring::ClockChanged`. `SYSCLK_CONF.CLK_XTAL_FREQ` must read 40, or ROM `ets_get_apb_freq`
//! scales every `ets_delay_us` wrongly; the generated table cannot carry that, so the model writes
//! it after every reset.
//!
//! `PERIP_CLK_EN0/1` and `PERIP_RST_EN0/1` are stored here and applied by `crate::wiring::gates`;
//! the per-block read latches it needs live here ([`Model::latch`]).
//!
//! `CPU_INTR_FROM_CPU_0..3` bit 0 is a level on sources 50 to 53, driven on every write and reset.
//! That drive is what starts the scheduler: `xPortStartScheduler` enables interrupts while
//! `port_xSchedulerRunning` is still 0, so the first `vPortYield` writes `FROM_CPU_0` and returns
//! without polling. The first context switch must happen within those few instructions, or
//! `start_cpu0` parks in its `j .` loop.

use pemu_core::fidelity::{Fidelity, FidelityLedger, TouchAccess};
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::block::System;
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};
use crate::r#gen::regs_system::{REG_COUNT, REGS, idx};
use crate::intc::IrqFabric;
use crate::regs::{self, TouchAt};

/// The crystal on this board, in MHz, as `SYSCLK_CONF.CLK_XTAL_FREQ` must read it.
pub const XTAL_MHZ: u32 = 40;

const XTAL_FREQ_SHIFT: u32 = 12;

const XTAL_FREQ_MASK: u32 = 0x7F << XTAL_FREQ_SHIFT;

/// `SYSCLK_CONF.PRE_DIV_CNT`, the XTAL divider minus one.
pub(crate) const PRE_DIV_CNT_MASK: u32 = 0x3FF;

const SOC_CLK_SEL_SHIFT: u32 = 10;

const CPUPERIOD_SEL_MASK: u32 = 0x3;

const CRC_START: u32 = 1 << 8;

/// `RTC_FASTMEM_CONFIG.RTC_MEM_CRC_FINISH`, read-only.
const CRC_FINISH: u32 = 1 << 31;

const CRC_ADDR_SHIFT: u32 = 9;

const CRC_LEN_SHIFT: u32 = 20;

/// Eleven-bit width of both CRC address and length fields.
const CRC_FIELD_MASK: u32 = 0x7FF;

/// The four software interrupt sources `CPU_INTR_FROM_CPU_0..3` drive.
pub const FROM_CPU_SOURCES: [IrqSource; 4] = [
    irq::FROM_CPU_INTR0,
    irq::FROM_CPU_INTR1,
    irq::FROM_CPU_INTR2,
    irq::FROM_CPU_INTR3,
];

const TOUCH_WORDS: usize = REG_COUNT.div_ceil(64);

/// `PERIP_CLK_EN1` bit 9, reserved and 1 after reset (TRM register 16.4; the capture reads the
/// register 0x200 with every crypto clock off). Not in the generated table, so the model writes
/// it after every reset and ignores guest writes to it.
const CLK_EN1_RESERVED_ONE: u32 = 1 << 9;

/// Blocks whose read latch this model holds: I2S0, AES and SPI2 (`crate::wiring::gates`).
pub const LATCHES: usize = 3;

/// The CPU clock source `SYSCLK_CONF.SOC_CLK_SEL` selects.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ClkSource {
    /// 0: the 40 MHz crystal, divided by `PRE_DIV_CNT + 1`.
    Xtal,
    /// 1: the BBPLL, 80 or 160 MHz by `CPUPERIOD_SEL`.
    Pll,
    /// 2 and 3: the RC_FAST oscillator, about 17.7 MHz ([`RC_FAST_HZ`]).
    RcFast,
}

/// The RC_FAST frequency for `SOC_CLK_SEL = 2`, in Hz: the measured
/// [`super::timg::RC_FAST_HZ`] (class A), not the nominal 17.5 MHz.
pub const RC_FAST_HZ: u32 = super::timg::RC_FAST_HZ as u32;

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    #[serde(with = "regs_serde")]
    regs: RegStore<REG_COUNT>,
    touched: [u64; TOUCH_WORDS],
    /// The last value each clock-gated block returned while clocked, which it keeps returning
    /// while its clock is off. 0 after a reset of the block.
    latch: [u32; LATCHES],
}

impl Default for Model {
    fn default() -> Self {
        let mut m = Model {
            regs: RegStore::new(&REGS),
            touched: [0; TOUCH_WORDS],
            latch: [0; LATCHES],
        };
        m.publish_xtal_freq();
        m.publish_reserved();
        m
    }
}

impl Model {
    pub fn clk_source(&self) -> ClkSource {
        match (self.regs.get(idx::SYSTEM_SYSCLK_CONF) >> SOC_CLK_SEL_SHIFT) & 0x3 {
            0 => ClkSource::Xtal,
            1 => ClkSource::Pll,
            _ => ClkSource::RcFast,
        }
    }

    /// CPU frequency in Hz, decoded as `rtc_clk_cpu_freq_get_config` does (`rtc_clk.c:260-302`).
    pub fn cpu_hz(&self) -> u32 {
        match self.clk_source() {
            ClkSource::Xtal => {
                let div = (self.regs.get(idx::SYSTEM_SYSCLK_CONF) & PRE_DIV_CNT_MASK) + 1;
                XTAL_MHZ * 1_000_000 / div
            }
            ClkSource::Pll => {
                if self.regs.get(idx::SYSTEM_CPU_PER_CONF) & CPUPERIOD_SEL_MASK == 0 {
                    80_000_000
                } else {
                    160_000_000
                }
            }
            ClkSource::RcFast => RC_FAST_HZ,
        }
    }

    /// APB frequency in Hz: the CPU frequency except on the PLL, where the APB stays at 80 MHz.
    pub fn apb_hz(&self) -> u32 {
        match self.clk_source() {
            ClkSource::Pll => 80_000_000,
            _ => self.cpu_hz(),
        }
    }

    /// Level of `CPU_INTR_FROM_CPU_<n>` bit 0; `false` for an index above 3.
    pub fn from_cpu_level(&self, n: usize) -> bool {
        match n {
            0..=3 => self.regs.get(idx::SYSTEM_CPU_INTR_FROM_CPU_0 + n) & 1 != 0,
            _ => false,
        }
    }

    /// Drives [`FROM_CPU_SOURCES`] to the four `CPU_INTR_FROM_CPU_<n>` bit 0 levels. An unchanged
    /// level costs no fabric epoch; a changed one ends the executor slice, so the interrupt is
    /// taken at the next instruction boundary.
    pub fn sync_irq(&self, irq: &mut IrqFabric) {
        for (n, src) in FROM_CPU_SOURCES.iter().enumerate() {
            irq.set_source(*src, self.from_cpu_level(n));
        }
    }

    /// `PERIP_RST_EN0` (`bank` 0) or `PERIP_RST_EN1` (`bank` 1): a 1 holds that peripheral in
    /// reset.
    pub fn perip_rst_en(&self, bank: usize) -> u32 {
        match bank {
            0 => self.regs.get(idx::SYSTEM_PERIP_RST_EN0),
            1 => self.regs.get(idx::SYSTEM_PERIP_RST_EN1),
            _ => 0,
        }
    }

    /// `PERIP_CLK_EN0` (`bank` 0) or `PERIP_CLK_EN1` (`bank` 1): a 1 clocks that peripheral.
    pub fn perip_clk_en(&self, bank: usize) -> u32 {
        match bank {
            0 => self.regs.get(idx::SYSTEM_PERIP_CLK_EN0),
            1 => self.regs.get(idx::SYSTEM_PERIP_CLK_EN1),
            _ => 0,
        }
    }

    /// The value latch `slot` holds: what its block last returned while clocked.
    pub fn latch(&self, slot: usize) -> u32 {
        self.latch[slot]
    }

    /// Records what the block of latch `slot` returned, or 0 when a reset reached it.
    pub fn set_latch(&mut self, slot: usize, val: u32) {
        self.latch[slot] = val;
    }

    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    /// Restores the registers of the scopes this reset clears and republishes the read-only
    /// crystal frequency.
    pub fn reset_to(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.regs.reset(kind.scope);
        self.publish_xtal_freq();
        self.publish_reserved();
        self.latch = [0; LATCHES];
    }

    /// Sets the reserved `PERIP_CLK_EN1` bit 9, which reads 1 ([`CLK_EN1_RESERVED_ONE`]).
    fn publish_reserved(&mut self) {
        let v = self.regs.get(idx::SYSTEM_PERIP_CLK_EN1);
        self.regs
            .set(idx::SYSTEM_PERIP_CLK_EN1, v | CLK_EN1_RESERVED_ONE);
    }

    /// Writes 40 into the read-only `CLK_XTAL_FREQ` field.
    fn publish_xtal_freq(&mut self) {
        let v = self.regs.get(idx::SYSTEM_SYSCLK_CONF) & !XTAL_FREQ_MASK;
        self.regs
            .set(idx::SYSTEM_SYSCLK_CONF, v | (XTAL_MHZ << XTAL_FREQ_SHIFT));
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

    /// Writes the low `size` bytes of `val` at block offset `off`, applies what the write
    /// triggers and reports the first touch.
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
        let Some((i, byte)) = regs::reg_at(&REGS, off) else {
            regs::hole(off, TouchAccess::Write, at, ledger);
            return Wiring::None;
        };
        regs::touch(&mut self.touched, &REGS, i, TouchAccess::Write, at, ledger);
        let before = self.regs.get(i);
        self.regs.write(i, byte, size, val);
        match i {
            idx::SYSTEM_SYSCLK_CONF => {
                self.publish_xtal_freq();
                Wiring::ClockChanged
            }
            idx::SYSTEM_CPU_PER_CONF => Wiring::ClockChanged,
            idx::SYSTEM_RTC_FASTMEM_CONFIG => {
                self.rtc_mem_crc(before);
                Wiring::None
            }
            _ => Wiring::None,
        }
    }

    /// `RTC_MEM_CRC_START` 0 to 1 computes the checksum and sets `FINISH`; 1 to 0 clears `FINISH`
    /// (row `system.rtc_fastmem_crc_finish`).
    ///
    /// The checksum depends on `ADDR` and `LEN` only, not on RTC FAST RAM, which a peripheral
    /// cannot reach. So a wake stub changed across deep sleep still validates. Class C;
    /// UNVERIFIED against silicon, whose polynomial is unknown.
    fn rtc_mem_crc(&mut self, before: u32) {
        let after = self.regs.get(idx::SYSTEM_RTC_FASTMEM_CONFIG);
        if before & CRC_START == 0 && after & CRC_START != 0 {
            let addr = (after >> CRC_ADDR_SHIFT) & CRC_FIELD_MASK;
            let len = (after >> CRC_LEN_SHIFT) & CRC_FIELD_MASK;
            self.regs
                .set(idx::SYSTEM_RTC_FASTMEM_CRC, fastmem_checksum(addr, len));
            self.regs
                .set(idx::SYSTEM_RTC_FASTMEM_CONFIG, after | CRC_FINISH);
        } else if before & CRC_START != 0 && after & CRC_START == 0 {
            self.regs
                .set(idx::SYSTEM_RTC_FASTMEM_CONFIG, after & !CRC_FINISH);
        }
    }
}

/// The stable checksum [`Model::rtc_mem_crc`] publishes: an FNV-1a mix of address and length,
/// never 0, so a checker cannot mistake it for an uninitialized register.
const fn fastmem_checksum(addr: u32, len: u32) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    h = (h ^ addr).wrapping_mul(0x0100_0193);
    h = (h ^ len).wrapping_mul(0x0100_0193);
    h | 1
}

impl Peripheral for Model {
    const ID: PeriphId = <System as Block>::ID;
    const BASE: u32 = <System as Block>::BASE;
    const SIZE: u32 = <System as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.reset_to(kind);
        self.sync_irq(cx.irq);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let wiring = self.store(off, size, val, cx.now, cx.ledger);
        self.sync_irq(cx.irq);
        RegWrite {
            stop: false,
            wiring,
        }
    }

    /// Every register is guest-written, and the checksum completes inside its write.
    fn stable_until(&self, off: u32, cx: &Cx) -> Stability {
        let _ = (off, cx);
        Stability::UntilInput
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        regs::reg_at(&REGS, off).map_or(Fidelity::U, |(i, _)| REGS[i].class)
    }
}

crate::regs::store_serde!();

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::reset::ResetCause;

    const OFF_CPU_PERI_RST_EN: u32 = 0x004;
    const OFF_CPU_PER_CONF: u32 = 0x008;
    const OFF_PERIP_CLK_EN0: u32 = 0x010;
    const OFF_PERIP_RST_EN0: u32 = 0x018;
    const OFF_PERIP_RST_EN1: u32 = 0x01C;
    const OFF_FROM_CPU_0: u32 = 0x028;
    const OFF_RTC_FASTMEM_CONFIG: u32 = 0x048;
    const OFF_RTC_FASTMEM_CRC: u32 = 0x04C;
    const OFF_SYSCLK_CONF: u32 = 0x058;

    const T: VTime = VTime(7);

    fn model() -> (Model, FidelityLedger) {
        (Model::default(), FidelityLedger::default())
    }

    fn kind(cause: ResetCause) -> ResetKind {
        ResetKind::of(cause).expect("a documented reset cause")
    }

    #[test]
    fn identity_matches_the_c3_devices_row() {
        assert_eq!(<Model as Peripheral>::ID, <System as Block>::ID);
        assert_eq!(<Model as Peripheral>::BASE, 0x600C_0000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x1000);
    }

    /// ROM `boot_prepare` reads the crystal frequency here; 0x00028001 is the power-on value.
    #[test]
    fn the_crystal_frequency_reads_forty_after_every_reset() {
        let (mut m, mut l) = model();
        assert_eq!(m.load(OFF_SYSCLK_CONF, Size::B4, T, &mut l), 0x0002_8001);
        m.store(OFF_SYSCLK_CONF, Size::B4, 0, T, &mut l);
        assert_eq!(
            m.load(OFF_SYSCLK_CONF, Size::B4, T, &mut l) & XTAL_FREQ_MASK,
            XTAL_MHZ << XTAL_FREQ_SHIFT,
            "the field is read-only"
        );
        m.reset_to(kind(ResetCause::POWERON));
        assert_eq!(m.load(OFF_SYSCLK_CONF, Size::B4, T, &mut l), 0x0002_8001);
    }

    #[test]
    fn the_frequency_decode_follows_the_two_selectors() {
        let (mut m, mut l) = model();
        assert_eq!(m.clk_source(), ClkSource::Xtal);
        assert_eq!(m.cpu_hz(), 20_000_000);
        assert_eq!(m.apb_hz(), 20_000_000);

        let wiring = m.store(OFF_SYSCLK_CONF, Size::B4, 0, T, &mut l);
        assert!(matches!(wiring, Wiring::ClockChanged));
        assert_eq!(m.cpu_hz(), 40_000_000);

        let wiring = m.store(OFF_SYSCLK_CONF, Size::B4, 1 << SOC_CLK_SEL_SHIFT, T, &mut l);
        assert!(matches!(wiring, Wiring::ClockChanged));
        assert_eq!(m.clk_source(), ClkSource::Pll);
        assert_eq!(m.cpu_hz(), 80_000_000);
        assert_eq!(m.apb_hz(), 80_000_000);

        let wiring = m.store(OFF_CPU_PER_CONF, Size::B4, 0xD, T, &mut l);
        assert!(matches!(wiring, Wiring::ClockChanged));
        assert_eq!(m.cpu_hz(), 160_000_000);
        assert_eq!(m.apb_hz(), 80_000_000);

        m.store(OFF_SYSCLK_CONF, Size::B4, 2 << SOC_CLK_SEL_SHIFT, T, &mut l);
        assert_eq!(m.clk_source(), ClkSource::RcFast);
        assert_eq!(m.cpu_hz(), RC_FAST_HZ);
        assert_eq!(m.apb_hz(), RC_FAST_HZ);
    }

    /// `CPU_PER_CONF` resets to 0xC (`PLL_FREQ_SEL`, `CPU_WAIT_MODE_FORCE_ON`).
    #[test]
    fn the_cpu_per_conf_reset_value_selects_eighty_megahertz_on_the_pll() {
        let (mut m, mut l) = model();
        assert_eq!(m.load(OFF_CPU_PER_CONF, Size::B4, T, &mut l), 0xC);
        m.store(OFF_SYSCLK_CONF, Size::B4, 1 << SOC_CLK_SEL_SHIFT, T, &mut l);
        assert_eq!(m.cpu_hz(), 80_000_000);
    }

    #[test]
    fn wait_row_rtc_fastmem_crc_finish_is_set_inside_the_access() {
        let (mut m, mut l) = model();
        assert_eq!(
            m.load(OFF_RTC_FASTMEM_CONFIG, Size::B4, T, &mut l) & CRC_FINISH,
            0
        );
        let cfg = m.load(OFF_RTC_FASTMEM_CONFIG, Size::B4, T, &mut l);
        m.store(OFF_RTC_FASTMEM_CONFIG, Size::B4, cfg | CRC_START, T, &mut l);
        assert_eq!(
            m.load(OFF_RTC_FASTMEM_CONFIG, Size::B4, T, &mut l) & CRC_FINISH,
            CRC_FINISH
        );
        let crc = m.load(OFF_RTC_FASTMEM_CRC, Size::B4, T, &mut l);
        assert_ne!(crc, 0, "a checker must not read an empty register");

        // The same ADDR and LEN give the same value, which a producer and a checker across a
        // sleep need.
        m.store(OFF_RTC_FASTMEM_CONFIG, Size::B4, cfg, T, &mut l);
        assert_eq!(
            m.load(OFF_RTC_FASTMEM_CONFIG, Size::B4, T, &mut l) & CRC_FINISH,
            0
        );
        m.store(OFF_RTC_FASTMEM_CONFIG, Size::B4, cfg | CRC_START, T, &mut l);
        assert_eq!(m.load(OFF_RTC_FASTMEM_CRC, Size::B4, T, &mut l), crc);

        let other = (cfg & !(CRC_FIELD_MASK << CRC_LEN_SHIFT)) | (0x100 << CRC_LEN_SHIFT);
        m.store(OFF_RTC_FASTMEM_CONFIG, Size::B4, other, T, &mut l);
        m.store(
            OFF_RTC_FASTMEM_CONFIG,
            Size::B4,
            other | CRC_START,
            T,
            &mut l,
        );
        assert_ne!(m.load(OFF_RTC_FASTMEM_CRC, Size::B4, T, &mut l), crc);
    }

    #[test]
    fn the_fastmem_config_reset_value_covers_the_window() {
        let (mut m, mut l) = model();
        let cfg = m.load(OFF_RTC_FASTMEM_CONFIG, Size::B4, T, &mut l);
        assert_eq!(cfg, 0x7FF0_0000);
        assert_eq!((cfg >> CRC_LEN_SHIFT) & CRC_FIELD_MASK, 0x7FF);
        assert_eq!((cfg >> CRC_ADDR_SHIFT) & CRC_FIELD_MASK, 0);
    }

    #[test]
    fn the_four_software_interrupts_publish_a_level() {
        let (mut m, mut l) = model();
        assert_eq!(FROM_CPU_SOURCES[0], IrqSource(50));
        assert_eq!(FROM_CPU_SOURCES[3], IrqSource(53));
        for n in 0..4u32 {
            assert!(!m.from_cpu_level(n as usize));
            m.store(OFF_FROM_CPU_0 + n * 4, Size::B4, 1, T, &mut l);
            assert!(m.from_cpu_level(n as usize), "FROM_CPU_{n}");
            m.store(OFF_FROM_CPU_0 + n * 4, Size::B4, 0, T, &mut l);
            assert!(!m.from_cpu_level(n as usize));
        }
        assert!(!m.from_cpu_level(4));
    }

    /// The scheduler start: `esp_crosscore_int_init` routes source 50 onto a priority-1 line,
    /// THRESH is 1 and `vPortYield` writes `CPU_INTR_FROM_CPU_0`.
    #[test]
    fn a_from_cpu_write_raises_the_software_interrupt_line() {
        use crate::intc::testing::TestPorts;
        use crate::intc::{OFF_CPU_INT_ENABLE, OFF_CPU_INT_PRI, OFF_CPU_INT_THRESH};

        const LINE: u32 = 4;
        let mut p = TestPorts::new();
        // The routing `esp_crosscore_int_init` leaves: level type, priority 1, THRESH 1.
        for src in FROM_CPU_SOURCES {
            p.irq.write(u32::from(src.0) * 4, LINE);
        }
        p.irq.write(OFF_CPU_INT_ENABLE, 1 << LINE);
        p.irq.write(OFF_CPU_INT_PRI + 4 * LINE, 1);
        p.irq.write(OFF_CPU_INT_THRESH, 1);

        let mut m = Model::default();
        for n in 0..4u32 {
            let src = FROM_CPU_SOURCES[n as usize];
            assert_eq!(p.irq.deliverable(), None, "FROM_CPU_{n} starts low");
            p.with(|cx| Peripheral::write(&mut m, OFF_FROM_CPU_0 + n * 4, Size::B4, 1, cx));
            assert!(p.source(src), "FROM_CPU_{n} drives source {}", src.0);
            assert_eq!(p.irq.deliverable(), Some(LINE as u8), "FROM_CPU_{n}");
            p.with(|cx| Peripheral::write(&mut m, OFF_FROM_CPU_0 + n * 4, Size::B4, 0, cx));
            assert!(!p.source(src), "the handler's clear lowers FROM_CPU_{n}");
            assert_eq!(p.irq.deliverable(), None);

            p.with(|cx| Peripheral::write(&mut m, OFF_FROM_CPU_0 + n * 4, Size::B4, 1, cx));
            p.with(|cx| Peripheral::reset(&mut m, kind(ResetCause::RTC_SW_SYS), cx));
            assert!(!p.source(src), "a system reset lowers FROM_CPU_{n}");
        }
    }

    /// `PERIP_RST_EN1` starts at 0x1FE, holding the crypto blocks in reset until a driver
    /// clears them.
    #[test]
    fn the_clock_gates_and_reset_enables_are_readable_storage() {
        let (mut m, mut l) = model();
        assert_eq!(
            m.load(OFF_PERIP_CLK_EN0, Size::B4, T, &mut l),
            0xF9C1_E06F,
            "PERIP_CLK_EN0 reset"
        );
        assert_eq!(m.perip_rst_en(1), 0x1FE, "crypto blocks start in reset");
        assert_eq!(
            m.load(OFF_CPU_PERI_RST_EN, Size::B4, T, &mut l),
            0xC0,
            "CPU_PERI_RST_EN reset"
        );

        // Bit 4 has no field in `SYSTEM_PERIP_RST_EN0`: reserved, it ignores the write.
        let wiring = m.store(OFF_PERIP_RST_EN0, Size::B4, 0xFFFF_FFFF, T, &mut l);
        assert!(matches!(wiring, Wiring::None));
        assert_eq!(m.perip_rst_en(0), 0xFFFF_FFEF);
        assert_eq!(m.load(OFF_PERIP_RST_EN1, Size::B4, T, &mut l), 0x1FE);
        assert_eq!(m.perip_rst_en(2), 0, "no third bank");
    }

    #[test]
    fn the_reserved_clock_enable_bit_reads_one() {
        const OFF_PERIP_CLK_EN1: u32 = 0x014;
        let (mut m, mut l) = model();
        assert_eq!(m.load(OFF_PERIP_CLK_EN1, Size::B4, T, &mut l), 0x200);
        m.store(OFF_PERIP_CLK_EN1, Size::B4, 0x4, T, &mut l);
        assert_eq!(m.load(OFF_PERIP_CLK_EN1, Size::B4, T, &mut l), 0x204);
        m.store(OFF_PERIP_CLK_EN1, Size::B4, 0, T, &mut l);
        assert_eq!(m.load(OFF_PERIP_CLK_EN1, Size::B4, T, &mut l), 0x200);
        assert_eq!(m.perip_clk_en(1), 0x200);
        m.set_latch(1, 7);
        m.reset_to(kind(ResetCause::RTC_SW_SYS));
        assert_eq!(m.perip_clk_en(1), 0x200);
        assert_eq!(m.latch(1), 0, "a reset clears the latches");
    }

    #[test]
    fn reset_scope_matrix_over_a_digital_block() {
        let (mut m, mut l) = model();
        m.store(OFF_PERIP_RST_EN0, Size::B4, 0xFFFF_FFFF, T, &mut l);
        m.reset_to(kind(ResetCause::RTC_SW_CPU));
        assert_eq!(m.perip_rst_en(0), 0xFFFF_FFEF, "a CPU reset keeps it");
        m.reset_to(kind(ResetCause::RTC_SW_SYS));
        assert_eq!(m.perip_rst_en(0), 0, "a core reset restores it");
    }

    #[test]
    fn first_touches_and_holes_are_reported() {
        let (mut m, mut l) = model();
        m.load(OFF_SYSCLK_CONF, Size::B4, VTime(1), &mut l);
        m.store(OFF_SYSCLK_CONF, Size::B4, 0, VTime(2), &mut l);
        let hole = 0x0F00;
        assert_eq!(m.load(hole, Size::B4, VTime(3), &mut l), 0);
        let offs: Vec<_> = l.first_touches().iter().map(|t| t.off).collect();
        assert_eq!(offs, vec![OFF_SYSCLK_CONF, hole]);
    }

    #[test]
    fn fidelity_comes_from_the_generated_table() {
        let m = Model::default();
        assert_eq!(m.fidelity(OFF_SYSCLK_CONF), Fidelity::B);
        assert_eq!(m.fidelity(OFF_CPU_PER_CONF), Fidelity::B);
        assert_eq!(m.fidelity(OFF_RTC_FASTMEM_CONFIG), Fidelity::B);
        assert_eq!(
            m.fidelity(OFF_PERIP_CLK_EN0),
            Fidelity::B,
            "the clock gate is the device's on the six blocks a capture gated and gates nothing \
             on the rest, specs/blocks/system.toml"
        );
    }
}
