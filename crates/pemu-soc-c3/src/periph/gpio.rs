//! GPIO at 0x60004000: the boot strap and the 26 output levels and enables the board samples
//! (`specs/blocks/gpio.toml`).
//!
//! `GPIO_STRAP` reads 0x0A on this board (device banner `boot:0xa (SPI_FAST_FLASH_BOOT)`) and is
//! re-latched by every reset that reaches the block. Bit 3 set sends ROM `main` down the flash
//! boot path; a low nibble of 3 would make `boot_prepare` hang, so [`Model::set_strap`] refuses
//! it.
//!
//! `GPIO_OUT` and `GPIO_ENABLE` with their `W1TS`/`W1TC` aliases are the whole output model; a
//! write that can change a pin returns `Wiring::GpioChanged`. `GPIO_IN` follows
//! [`Model::set_in_level`]. The BSP installs no GPIO interrupt handler, so source 16 stays
//! unconnected. The sigma-delta registers at 0xF00 are plain storage.

use pemu_core::fidelity::{Fidelity, FidelityLedger, TouchAccess};
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::block::Gpio;
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};
use crate::r#gen::regs_gpio::{REG_COUNT, REGS, idx};
use crate::regs::{self, TouchAt};

/// Interrupt source of this block; nothing drives it.
pub const IRQ_SOURCE: IrqSource = irq::GPIO;

/// Pins the C3 has: the width of `GPIO_OUT`, `GPIO_ENABLE` and `GPIO_IN`.
pub const PIN_COUNT: u8 = 26;

const PIN_MASK: u32 = (1 << PIN_COUNT) - 1;

/// The strap value of this board.
pub const STRAP_FLASH_BOOT: u32 = 0x0A;

/// The strap value a USB Serial/JTAG download reset latches, `boot:0x6 (DOWNLOAD(USB/UART0))`.
/// The ROM answers SYNC on USB when `GPIO_STRAP & 0xC == 4`; 0x02 would wait on UART0 only
/// (`specs/notes/usj-download-strap.md`).
pub const STRAP_DOWNLOAD: u32 = 0x06;

/// The low nibble ROM `boot_prepare` hangs forever on (at 0x400498ee).
pub const STRAP_FORBIDDEN_NIBBLE: u32 = 3;

/// The pin the panel samples as its data/command line.
pub const PIN_LCD_DC: u8 = 20;

const TOUCH_WORDS: usize = REG_COUNT.div_ceil(64);

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    #[serde(with = "regs_serde")]
    regs: RegStore<REG_COUNT>,
    touched: [u64; TOUCH_WORDS],
    /// The value every reset that reaches this block latches into `GPIO_STRAP`.
    strap: u32,
    /// Levels the board was last told about, so only changed pins are reported.
    published_levels: u32,
    /// Output enables the board was last told about.
    published_enables: u32,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            regs: RegStore::new(&REGS),
            touched: [0; TOUCH_WORDS],
            strap: STRAP_FLASH_BOOT,
            published_levels: 0,
            published_enables: 0,
        }
    }
}

impl Model {
    /// The latched strap value, which `GPIO_STRAP` reads.
    pub fn strap(&self) -> u32 {
        self.regs.get(idx::GPIO_STRAP) & 0xFFFF
    }

    /// Sets the value every later reset latches, and latches it now. Refuses (returns `false`) a
    /// low nibble of [`STRAP_FORBIDDEN_NIBBLE`].
    pub fn set_strap(&mut self, value: u32) -> bool {
        if value & 0xF == STRAP_FORBIDDEN_NIBBLE {
            return false;
        }
        self.strap = value & 0xFFFF;
        self.regs.set(idx::GPIO_STRAP, self.strap);
        true
    }

    /// Whether the strap makes the ROM boot from flash: bit 3 set
    /// (`soc/esp32c3/include/soc/boot_mode.h:13`).
    pub fn straps_to_flash_boot(&self) -> bool {
        self.strap() & 0x8 != 0
    }

    /// Output levels, one bit per pin, unmasked by `GPIO_ENABLE`: the board takes the enable
    /// separately, and a mask would report a spurious change on every enable toggle.
    pub fn out_levels(&self) -> u32 {
        self.regs.get(idx::GPIO_OUT) & PIN_MASK
    }

    pub fn out_enables(&self) -> u32 {
        self.regs.get(idx::GPIO_ENABLE) & PIN_MASK
    }

    pub fn out_level(&self, pin: u8) -> bool {
        pin < PIN_COUNT && self.out_levels() & (1 << pin) != 0
    }

    pub fn out_enabled(&self, pin: u8) -> bool {
        pin < PIN_COUNT && self.out_enables() & (1 << pin) != 0
    }

    /// Sets the level `GPIO_IN` reports for one pin.
    pub fn set_in_level(&mut self, pin: u8, level: bool) {
        if pin >= PIN_COUNT {
            return;
        }
        let bit = 1u32 << pin;
        let v = self.regs.get(idx::GPIO_IN);
        self.regs
            .set(idx::GPIO_IN, if level { v | bit } else { v & !bit });
    }

    /// Pins whose level or enable differs from what the board was last told, one bit per pin.
    pub fn changed_pins(&self) -> u32 {
        ((self.out_levels() ^ self.published_levels)
            | (self.out_enables() ^ self.published_enables))
            & PIN_MASK
    }

    /// Records that the board has been told the current levels and enables.
    pub fn mark_published(&mut self) {
        self.published_levels = self.out_levels();
        self.published_enables = self.out_enables();
    }

    pub fn in_levels(&self) -> u32 {
        self.regs.get(idx::GPIO_IN) & PIN_MASK
    }

    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    /// Restores the registers of the scopes this reset clears and re-latches the strap. Every
    /// reset but `CPU0_` reaches the block, so a USB-UART reset (0x15) latches whatever
    /// [`Model::set_strap`] last left: the download strap or the board's.
    pub fn reset_to(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.regs.reset(kind.scope);
        self.regs.set(idx::GPIO_STRAP, self.strap);
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

    /// Writes the low `size` bytes of `val` at block offset `off`, applies the `W1TS` and `W1TC`
    /// aliases and reports the first touch.
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
        self.regs.write(i, byte, size, val);
        match i {
            idx::GPIO_OUT | idx::GPIO_ENABLE => Wiring::GpioChanged,
            idx::GPIO_OUT_W1TS => self.apply_alias(i, idx::GPIO_OUT, true),
            idx::GPIO_OUT_W1TC => self.apply_alias(i, idx::GPIO_OUT, false),
            idx::GPIO_ENABLE_W1TS => self.apply_alias(i, idx::GPIO_ENABLE, true),
            idx::GPIO_ENABLE_W1TC => self.apply_alias(i, idx::GPIO_ENABLE, false),
            _ => Wiring::None,
        }
    }

    /// Applies a write-only alias to its target register and clears the alias, which reads 0.
    fn apply_alias(&mut self, alias: usize, target: usize, set: bool) -> Wiring {
        let mask = self.regs.get(alias) & PIN_MASK;
        self.regs.set(alias, 0);
        if mask == 0 {
            return Wiring::None;
        }
        let v = self.regs.get(target);
        self.regs
            .set(target, if set { v | mask } else { v & !mask });
        Wiring::GpioChanged
    }
}

impl Peripheral for Model {
    const ID: PeriphId = <Gpio as Block>::ID;
    const BASE: u32 = <Gpio as Block>::BASE;
    const SIZE: u32 = <Gpio as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        let _ = cx;
        self.reset_to(kind);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let wiring = self.store(off, size, val, cx.now, cx.ledger);
        RegWrite {
            stop: false,
            wiring,
        }
    }

    /// An output changes on a write and an input on an `InputEvent`.
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

    const OFF_OUT: u32 = 0x004;
    const OFF_OUT_W1TS: u32 = 0x008;
    const OFF_OUT_W1TC: u32 = 0x00C;
    const OFF_ENABLE: u32 = 0x020;
    const OFF_ENABLE_W1TS: u32 = 0x024;
    const OFF_ENABLE_W1TC: u32 = 0x028;
    const OFF_STRAP: u32 = 0x038;
    const OFF_IN: u32 = 0x03C;
    const OFF_PIN20: u32 = 0x074 + 20 * 4;

    const T: VTime = VTime(2);

    fn model() -> (Model, FidelityLedger) {
        (Model::default(), FidelityLedger::default())
    }

    fn kind(cause: ResetCause) -> ResetKind {
        ResetKind::of(cause).expect("a documented reset cause")
    }

    #[test]
    fn identity_matches_the_c3_devices_row() {
        assert_eq!(<Model as Peripheral>::ID, <Gpio as Block>::ID);
        assert_eq!(<Model as Peripheral>::BASE, 0x6000_4000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x1000);
    }

    #[test]
    fn the_strap_reads_0x0a_and_never_the_forbidden_nibble() {
        let (mut m, mut l) = model();
        assert_eq!(m.load(OFF_STRAP, Size::B4, T, &mut l), STRAP_FLASH_BOOT);
        assert_eq!(m.strap(), 0x0A);
        assert!(m.straps_to_flash_boot(), "IS_1XXX, boot_mode.h:13");
        assert_ne!(m.strap() & 0xF, STRAP_FORBIDDEN_NIBBLE);

        // Read-only.
        m.store(OFF_STRAP, Size::B4, 0xFFFF, T, &mut l);
        assert_eq!(m.load(OFF_STRAP, Size::B4, T, &mut l), STRAP_FLASH_BOOT);

        // The value a reset re-latches is refused when it would hang the ROM.
        assert!(!m.set_strap(0x13), "low nibble 3 hangs boot_prepare");
        assert_eq!(m.strap(), STRAP_FLASH_BOOT);
        assert!(m.set_strap(STRAP_DOWNLOAD));
        assert_eq!(m.load(OFF_STRAP, Size::B4, T, &mut l), STRAP_DOWNLOAD);
        assert!(!m.straps_to_flash_boot());
    }

    #[test]
    fn every_reset_that_reaches_the_block_re_latches_the_strap() {
        let (mut m, mut l) = model();
        m.set_strap(STRAP_DOWNLOAD);
        assert_eq!(m.load(OFF_STRAP, Size::B4, T, &mut l), STRAP_DOWNLOAD);

        // The download session ended; a `CORE_` reset (cause 0x15) latches the board strap.
        m.strap = STRAP_FLASH_BOOT;
        m.reset_to(kind(ResetCause::USB_UART_CHIP));
        assert_eq!(m.load(OFF_STRAP, Size::B4, T, &mut l), STRAP_FLASH_BOOT);

        for cause in [
            ResetCause::RTC_SW_SYS,
            ResetCause::RTCWDT_RTC,
            ResetCause::POWERON,
        ] {
            m.set_strap(STRAP_DOWNLOAD);
            m.strap = STRAP_FLASH_BOOT;
            m.reset_to(kind(cause));
            assert_eq!(
                m.load(OFF_STRAP, Size::B4, T, &mut l),
                STRAP_FLASH_BOOT,
                "{cause:?}"
            );
        }

        m.set_strap(STRAP_DOWNLOAD);
        m.strap = STRAP_FLASH_BOOT;
        m.reset_to(kind(ResetCause::RTC_SW_CPU));
        assert_eq!(m.load(OFF_STRAP, Size::B4, T, &mut l), STRAP_DOWNLOAD);
    }

    /// `gpio_ll_set_level` writes `1 << n` into `GPIO_OUT_W1TS` or `_W1TC`; the LCD callbacks
    /// toggle the enable of GPIO20 the same way.
    #[test]
    fn the_w1ts_and_w1tc_aliases_drive_the_output_and_its_enable() {
        let (mut m, mut l) = model();
        let dc = 1u32 << PIN_LCD_DC;
        assert!(!m.out_level(PIN_LCD_DC));

        let wiring = m.store(OFF_OUT_W1TS, Size::B4, dc, T, &mut l);
        assert!(matches!(wiring, Wiring::GpioChanged));
        assert!(m.out_level(PIN_LCD_DC));
        assert_eq!(m.load(OFF_OUT, Size::B4, T, &mut l), dc);
        assert_eq!(m.load(OFF_OUT_W1TS, Size::B4, T, &mut l), 0, "write only");

        let wiring = m.store(OFF_ENABLE_W1TS, Size::B4, dc, T, &mut l);
        assert!(matches!(wiring, Wiring::GpioChanged));
        assert!(m.out_enabled(PIN_LCD_DC));

        let wiring = m.store(OFF_OUT_W1TC, Size::B4, dc, T, &mut l);
        assert!(matches!(wiring, Wiring::GpioChanged));
        assert!(!m.out_level(PIN_LCD_DC));
        assert!(m.out_enabled(PIN_LCD_DC), "the enable is untouched");

        let wiring = m.store(OFF_ENABLE_W1TC, Size::B4, dc, T, &mut l);
        assert!(matches!(wiring, Wiring::GpioChanged));
        assert!(!m.out_enabled(PIN_LCD_DC));

        assert!(matches!(
            m.store(OFF_OUT_W1TS, Size::B4, 0, T, &mut l),
            Wiring::None
        ));
    }

    #[test]
    fn a_direct_write_to_the_output_registers_raises_the_wiring_effect() {
        let (mut m, mut l) = model();
        assert!(matches!(
            m.store(OFF_OUT, Size::B4, 0x00F0, T, &mut l),
            Wiring::GpioChanged
        ));
        assert_eq!(m.out_levels(), 0x00F0);
        assert!(matches!(
            m.store(OFF_ENABLE, Size::B4, 0x00F0, T, &mut l),
            Wiring::GpioChanged
        ));
        assert_eq!(m.out_enables(), 0x00F0);
        assert!(matches!(
            m.store(OFF_PIN20, Size::B4, 0x4, T, &mut l),
            Wiring::None,
        ));
    }

    #[test]
    fn only_the_twenty_six_modeled_pins_exist() {
        let (mut m, mut l) = model();
        m.store(OFF_OUT, Size::B4, 0xFFFF_FFFF, T, &mut l);
        assert_eq!(m.out_levels(), PIN_MASK);
        assert_eq!(m.load(OFF_OUT, Size::B4, T, &mut l), PIN_MASK);
        assert!(!m.out_level(PIN_COUNT));
        assert!(!m.out_enabled(PIN_COUNT));
    }

    #[test]
    fn the_input_register_follows_the_board_and_ignores_writes() {
        let (mut m, mut l) = model();
        m.store(OFF_IN, Size::B4, 0xFFFF, T, &mut l);
        assert_eq!(m.load(OFF_IN, Size::B4, T, &mut l), 0);
        m.set_in_level(9, true);
        assert_eq!(m.in_levels(), 1 << 9);
        assert_eq!(m.load(OFF_IN, Size::B4, T, &mut l), 1 << 9);
        m.set_in_level(9, false);
        assert_eq!(m.in_levels(), 0);
        m.set_in_level(PIN_COUNT, true);
        assert_eq!(m.in_levels(), 0, "there is no pin 26");
    }

    #[test]
    fn reset_scope_matrix_over_the_output_registers() {
        let (mut m, mut l) = model();
        m.store(OFF_OUT, Size::B4, 0x00F0, T, &mut l);
        m.reset_to(kind(ResetCause::RTC_SW_CPU));
        assert_eq!(m.out_levels(), 0x00F0, "a CPU reset keeps it");
        m.reset_to(kind(ResetCause::RTC_SW_SYS));
        assert_eq!(m.out_levels(), 0);
        assert_eq!(
            m.load(OFF_STRAP, Size::B4, T, &mut l),
            STRAP_FLASH_BOOT,
            "the strap is never cleared to zero"
        );
    }

    #[test]
    fn fidelity_comes_from_the_generated_table() {
        let m = Model::default();
        assert_eq!(
            m.fidelity(OFF_STRAP),
            Fidelity::A,
            "a device capture fixes the strap"
        );
        assert_eq!(m.fidelity(OFF_OUT), Fidelity::B);
        assert_eq!(m.fidelity(OFF_ENABLE), Fidelity::B);
        assert_eq!(
            m.fidelity(OFF_OUT_W1TS),
            Fidelity::B,
            "the alias folds into GPIO_OUT and reaches the board, specs/blocks/gpio.toml"
        );
        assert_eq!(
            m.fidelity(OFF_PIN20),
            Fidelity::C,
            "the per-pin configuration is stored and not acted on, specs/blocks/gpio.toml"
        );
    }
}
