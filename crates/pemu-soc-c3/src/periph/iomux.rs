//! IO_MUX at 0x60009000: per-pad function select, pulls, input enable and drive strength, stored
//! for read-back (`specs/blocks/iomux.toml`).
//!
//! IDF gives the pads as one macro family, `IO_MUX_GPIOn_REG = 0x60009004 + 4n`, so
//! `specs/c3-registers.csv` has no rows and there is no generated table: the block is `PIN_CTRL`
//! plus [`PAD_COUNT`] pad words, held directly. There is no pad-level electrical model; a pad
//! with `MCU_SEL` = [`PIN_FUNC_GPIO`] has its level in `periph/gpio.rs`.

use pemu_core::fidelity::{Fidelity, FidelityLedger};
use pemu_core::regstore::Size;
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use pemu_core::fidelity::{FirstTouch, TouchAccess};

use super::block::Iomux;
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};

/// Pads the C3 has, and the number of `IO_MUX_GPIOn` registers.
pub const PAD_COUNT: u8 = 26;

/// Block offset of `IO_MUX_PIN_CTRL`, the clock-output select before the pad array.
pub const OFF_PIN_CTRL: u32 = 0x000;

pub const OFF_PAD0: u32 = 0x004;

/// `MCU_SEL` value that routes a pad to the GPIO matrix.
pub const PIN_FUNC_GPIO: u32 = 1;

/// One pad field: the low bit and the width.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PadField {
    pub lsb: u8,
    pub width: u8,
}

impl PadField {
    const fn new(lsb: u8, width: u8) -> Self {
        PadField { lsb, width }
    }

    pub const fn mask(self) -> u32 {
        (((1u64 << self.width) - 1) as u32) << self.lsb
    }

    pub const fn of(self, pad: u32) -> u32 {
        (pad >> self.lsb) & (((1u64 << self.width) - 1) as u32)
    }
}

/// `SLP_OE`: sleep-configuration output enable (IDF `soc/esp32c3/register/soc/io_mux_reg.h`).
pub const SLP_OE: PadField = PadField::new(0, 1);
/// `SLP_SEL`: use the sleep configuration for this pad.
pub const SLP_SEL: PadField = PadField::new(1, 1);
/// `SLP_PD`: pull-down in the sleep configuration.
pub const SLP_PD: PadField = PadField::new(2, 1);
/// `SLP_PU`: pull-up in the sleep configuration.
pub const SLP_PU: PadField = PadField::new(3, 1);
/// `SLP_IE`: input enable in the sleep configuration.
pub const SLP_IE: PadField = PadField::new(4, 1);
/// `SLP_DRV`: drive strength in the sleep configuration.
pub const SLP_DRV: PadField = PadField::new(5, 2);
pub const FUN_PD: PadField = PadField::new(7, 1);
pub const FUN_PU: PadField = PadField::new(8, 1);
pub const FUN_IE: PadField = PadField::new(9, 1);
/// `FUN_DRV`: drive strength, 0 to 3.
pub const FUN_DRV: PadField = PadField::new(10, 2);
/// `MCU_SEL`: pad function, [`PIN_FUNC_GPIO`] for the GPIO matrix.
pub const MCU_SEL: PadField = PadField::new(12, 3);
/// `FILTER_EN`: glitch filter on the input path.
pub const FILTER_EN: PadField = PadField::new(15, 1);

/// Bits of a pad register with a field: all of bits 15:0. Bits 16 and above read 0 and drop
/// writes (UNVERIFIED: no device capture of a pad register).
pub const PAD_MASK: u32 = SLP_OE.mask()
    | SLP_SEL.mask()
    | SLP_PD.mask()
    | SLP_PU.mask()
    | SLP_IE.mask()
    | SLP_DRV.mask()
    | FUN_PD.mask()
    | FUN_PU.mask()
    | FUN_IE.mask()
    | FUN_DRV.mask()
    | MCU_SEL.mask()
    | FILTER_EN.mask();

const WORD_COUNT: usize = 1 + PAD_COUNT as usize;

/// The reset value of every register, `PIN_CTRL` first.
///
/// GPIO0 to GPIO21 are the device's: the `probe_campaign_regs` capture, with the boot's own field
/// writes taken out, gives `FUN_DRV` 2 (3 on the USB pads 18 and 19), `FUN_IE` on 2 to 10, 12 to
/// 17, 20 and 21, and `FUN_WPU` on 9. This agrees with TRM table 5.12-1 except GPIO21, where the
/// device reads `FUN_IE` set. `PIN_CTRL` and the pads past GPIO21 are UNVERIFIED and reset to 0.
const RESET_WORDS: [u32; WORD_COUNT] = {
    let mut w = [0u32; WORD_COUNT];
    let drv2 = 2 << 10;
    let ie = 1 << 9;
    let wpu = 1 << 8;
    // GPIO0, GPIO1 (XTAL_32K_P/N) and GPIO11 (VDD_SPI): input disabled.
    w[1] = drv2;
    w[2] = drv2;
    w[12] = drv2;
    // GPIO2 to GPIO8 and GPIO10: input enabled.
    let mut pad = 2;
    while pad <= 10 {
        w[1 + pad] = drv2 | ie;
        pad += 1;
    }
    // GPIO9, the boot strap: input enabled, pulled up.
    w[10] = drv2 | ie | wpu;
    // GPIO12 to GPIO17, the flash pads: input enabled, pulled up.
    let mut pad = 12;
    while pad <= 17 {
        w[1 + pad] = drv2 | ie | wpu;
        pad += 1;
    }
    // GPIO18 and GPIO19, the USB pads: drive 3, input disabled.
    w[19] = 3 << 10;
    w[20] = 3 << 10;
    // GPIO20 (U0RXD) and GPIO21 (U0TXD): input enabled, pulled up.
    w[21] = drv2 | ie | wpu;
    w[22] = drv2 | ie | wpu;
    w
};

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    words: [u32; WORD_COUNT],
    touched: u32,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            words: RESET_WORDS,
            touched: 0,
        }
    }
}

impl Model {
    /// Block offset of the register of pad `pin`, if the pad exists.
    pub fn pad_off(pin: u8) -> Option<u32> {
        (pin < PAD_COUNT).then(|| OFF_PAD0 + u32::from(pin) * 4)
    }

    /// The stored pad word of `pin`, or 0 for a pad the chip does not have.
    pub fn pad(&self, pin: u8) -> u32 {
        if pin < PAD_COUNT {
            self.words[1 + pin as usize]
        } else {
            0
        }
    }

    /// Sets the whole pad word of `pin` from outside the guest, as a board states its wiring.
    /// Reserved bits are dropped.
    pub fn set_pad(&mut self, pin: u8, value: u32) {
        if pin < PAD_COUNT {
            self.words[1 + pin as usize] = value & PAD_MASK;
        }
    }

    pub fn pin_ctrl(&self) -> u32 {
        self.words[0]
    }

    /// The value of one pad field, e.g. `m.pad_field(20, iomux::MCU_SEL)`.
    pub fn pad_field(&self, pin: u8, field: PadField) -> u32 {
        field.of(self.pad(pin))
    }

    pub fn mcu_sel(&self, pin: u8) -> u32 {
        self.pad_field(pin, MCU_SEL)
    }

    pub fn is_gpio(&self, pin: u8) -> bool {
        self.mcu_sel(pin) == PIN_FUNC_GPIO
    }

    pub fn input_enabled(&self, pin: u8) -> bool {
        self.pad_field(pin, FUN_IE) != 0
    }

    pub fn pull_up(&self, pin: u8) -> bool {
        self.pad_field(pin, FUN_PU) != 0
    }

    pub fn pull_down(&self, pin: u8) -> bool {
        self.pad_field(pin, FUN_PD) != 0
    }

    /// Drive strength of `pin`, 0 to 3. Stored for read-back only.
    pub fn drive(&self, pin: u8) -> u32 {
        self.pad_field(pin, FUN_DRV)
    }

    /// Restores the window to [`RESET_WORDS`].
    pub fn reset_to(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.words = RESET_WORDS;
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        let Some(i) = word_at(off) else {
            report(off, TouchAccess::Read, size, now, ledger);
            return 0;
        };
        self.touch(i, TouchAccess::Read, size, now, ledger);
        (self.words[i] >> ((off & 3) * 8)) & size_mask(size)
    }

    /// Writes the low `size` bytes of `val` at block offset `off`, dropping the reserved bits of a
    /// pad register, and reports the first touch. No wiring effect: `periph/gpio.rs` owns the
    /// levels the board sees.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        let Some(i) = word_at(off) else {
            report(off, TouchAccess::Write, size, now, ledger);
            return;
        };
        self.touch(i, TouchAccess::Write, size, now, ledger);
        let shift = (off & 3) * 8;
        let mask = size_mask(size) << shift;
        let next = (self.words[i] & !mask) | ((val << shift) & mask);
        self.words[i] = if i == 0 { next } else { next & PAD_MASK };
    }

    fn touch(
        &mut self,
        i: usize,
        access: TouchAccess,
        size: Size,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        let bit = 1u32 << i;
        if self.touched & bit != 0 {
            return;
        }
        self.touched |= bit;
        report(reg_off(i), access, size, now, ledger);
    }
}

/// Index of the register holding block offset `off`, if the block has one there.
fn word_at(off: u32) -> Option<usize> {
    let aligned = off & !3;
    let i = (aligned / 4) as usize;
    (i < WORD_COUNT).then_some(i)
}

/// Block offset of register `i`, the inverse of [`word_at`].
const fn reg_off(i: usize) -> u32 {
    (i as u32) * 4
}

/// Mask of the bytes an access of `size` covers.
const fn size_mask(size: Size) -> u32 {
    match size {
        Size::B1 => 0xFF,
        Size::B2 => 0xFFFF,
        Size::B4 => u32::MAX,
    }
}

/// Records a touch, also of an offset with no register (which reads 0 and drops the write).
fn report(off: u32, access: TouchAccess, size: Size, now: VTime, ledger: &mut FidelityLedger) {
    ledger.first_touch(FirstTouch {
        periph: <Model as Peripheral>::ID,
        off: off & !3,
        access,
        size: size as u8,
        now,
        allowlisted: false,
    });
}

impl Peripheral for Model {
    const ID: PeriphId = <Iomux as Block>::ID;
    const BASE: u32 = <Iomux as Block>::BASE;
    const SIZE: u32 = <Iomux as Block>::SIZE;

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
        self.store(off, size, val, cx.now, cx.ledger);
        RegWrite {
            stop: false,
            wiring: Wiring::None,
        }
    }

    fn stable_until(&self, off: u32, cx: &Cx) -> Stability {
        let _ = (off, cx);
        Stability::UntilInput
    }

    /// No generated table, so the class is written here: B for the pad registers, U otherwise.
    fn fidelity(&self, off: u32) -> Fidelity {
        match word_at(off) {
            Some(i) if i > 0 => Fidelity::B,
            _ => Fidelity::U,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::reset::ResetCause;

    const T: VTime = VTime(3);

    /// The pad word `gpio_config` writes for an output: function GPIO, input and pull-up on,
    /// drive 2.
    const PAD_GPIO_OUT: u32 =
        (PIN_FUNC_GPIO << 12) | FUN_IE.mask() | FUN_PU.mask() | (2 << FUN_DRV.lsb);

    fn model() -> (Model, FidelityLedger) {
        (Model::default(), FidelityLedger::default())
    }

    fn kind(cause: ResetCause) -> ResetKind {
        ResetKind::of(cause).expect("a documented reset cause")
    }

    #[test]
    fn identity_matches_the_c3_devices_row() {
        assert_eq!(<Model as Peripheral>::ID, <Iomux as Block>::ID);
        assert_eq!(<Model as Peripheral>::BASE, 0x6000_9000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x1000);
    }

    #[test]
    fn the_pad_registers_are_one_indexed_family() {
        assert_eq!(Model::pad_off(0), Some(0x004));
        assert_eq!(Model::pad_off(20), Some(0x054));
        assert_eq!(Model::pad_off(PAD_COUNT - 1), Some(0x068));
        assert_eq!(Model::pad_off(PAD_COUNT), None, "there is no pad 26");
    }

    #[test]
    fn a_pad_word_reads_back_field_by_field() {
        let (mut m, mut l) = model();
        let off = Model::pad_off(PAD_COUNT - 6).expect("pad 20 exists");
        m.store(off, Size::B4, PAD_GPIO_OUT, T, &mut l);

        assert_eq!(m.load(off, Size::B4, T, &mut l), PAD_GPIO_OUT);
        assert_eq!(m.mcu_sel(20), PIN_FUNC_GPIO);
        assert!(m.is_gpio(20));
        assert!(m.input_enabled(20));
        assert!(m.pull_up(20));
        assert!(!m.pull_down(20));
        assert_eq!(m.drive(20), 2);
        assert_eq!(m.pad_field(20, FILTER_EN), 0);
        assert_eq!(
            m.pad_field(20, SLP_SEL),
            0,
            "the GPIO output word leaves sleep select clear"
        );
        assert_eq!(m.pad_field(20, SLP_OE), 0);

        assert_eq!(m.pad(19), 0xC00);
        assert!(!m.is_gpio(19), "function 0 is not the GPIO matrix");
    }

    /// Bits 16 and above read 0 (UNVERIFIED); every bit below is a field.
    #[test]
    fn reserved_pad_bits_read_zero() {
        let (mut m, mut l) = model();
        let off = Model::pad_off(9).expect("pad 9 exists");
        m.store(off, Size::B4, 0xFFFF_FFFF, T, &mut l);
        assert_eq!(m.load(off, Size::B4, T, &mut l), PAD_MASK);
        assert_eq!(PAD_MASK & !0xFFFF, 0);
        assert_eq!(PAD_MASK, 0xFFFF, "the whole low half-word is a field");
    }

    /// `PIN_SLP_PULLUP_ENABLE` sets bit 3 and reads it back; `IO_MUX_GPIO` is a `stable_read`
    /// register, so a dropped field would fail an oracle read comparison.
    #[test]
    fn the_sleep_fields_read_back() {
        let (mut m, mut l) = model();
        let off = Model::pad_off(9).expect("pad 9 exists");

        m.store(off, Size::B4, SLP_PU.mask(), T, &mut l);
        assert_eq!(m.load(off, Size::B4, T, &mut l), 0x8, "SLP_PU is bit 3");
        assert_eq!(m.pad_field(9, SLP_PU), 1);

        for (field, bits) in [
            (SLP_OE, 0x0001),
            (SLP_SEL, 0x0002),
            (SLP_PD, 0x0004),
            (SLP_PU, 0x0008),
            (SLP_IE, 0x0010),
            (SLP_DRV, 0x0060),
        ] {
            m.set_pad(9, field.mask());
            assert_eq!(m.pad(9), bits, "{field:?}");
            assert_eq!(m.load(off, Size::B4, T, &mut l), bits, "{field:?}");
        }

        assert_eq!(
            SLP_OE.mask()
                | SLP_SEL.mask()
                | SLP_PD.mask()
                | SLP_PU.mask()
                | SLP_IE.mask()
                | SLP_DRV.mask(),
            0x007F,
            "bits 6:0 are the sleep configuration"
        );
        assert_eq!(
            FUN_PD.mask() | FUN_PU.mask() | FUN_IE.mask() | FUN_DRV.mask(),
            0x0F80,
            "bits 11:7 are the functional configuration"
        );
    }

    #[test]
    fn pin_ctrl_is_not_masked_like_a_pad() {
        let (mut m, mut l) = model();
        m.store(OFF_PIN_CTRL, Size::B4, 0xFFFF_FFFF, T, &mut l);
        assert_eq!(m.pin_ctrl(), 0xFFFF_FFFF);
        assert_eq!(m.fidelity(OFF_PIN_CTRL), Fidelity::U);
        assert_eq!(m.fidelity(OFF_PAD0), Fidelity::B);
    }

    #[test]
    fn narrow_accesses_reach_the_addressed_bytes() {
        let (mut m, mut l) = model();
        let off = Model::pad_off(2).expect("pad 2 exists");
        m.store(off, Size::B4, PAD_GPIO_OUT, T, &mut l);
        assert_eq!(m.load(off, Size::B1, T, &mut l), PAD_GPIO_OUT & 0xFF);
        assert_eq!(m.load(off + 1, Size::B1, T, &mut l), PAD_GPIO_OUT >> 8);
        assert_eq!(m.load(off, Size::B2, T, &mut l), PAD_GPIO_OUT & 0xFFFF);

        m.store(off + 1, Size::B1, 0x00, T, &mut l);
        assert_eq!(m.pad(2), PAD_GPIO_OUT & 0xFF);
    }

    #[test]
    fn a_board_can_set_a_pad_and_the_guest_reads_it() {
        let (mut m, mut l) = model();
        m.set_pad(20, PAD_GPIO_OUT | 0xFFFF_0000);
        let off = Model::pad_off(20).expect("pad 20 exists");
        assert_eq!(m.load(off, Size::B4, T, &mut l), PAD_GPIO_OUT);
        m.set_pad(PAD_COUNT, 0xFFFF);
        assert_eq!(m.pad(PAD_COUNT), 0, "there is no pad 26");
    }

    #[test]
    fn reset_scope_matrix_over_the_pad_words() {
        let (mut m, mut l) = model();
        let off = Model::pad_off(20).expect("pad 20 exists");
        m.store(off, Size::B4, PAD_GPIO_OUT, T, &mut l);
        m.reset_to(kind(ResetCause::RTC_SW_CPU));
        assert_eq!(m.pad(20), PAD_GPIO_OUT, "a CPU reset keeps it");
        m.reset_to(kind(ResetCause::RTC_SW_SYS));
        assert_eq!(m.pad(20), 0xB00, "the reset value");
    }

    #[test]
    fn the_pads_reset_to_the_device_values() {
        let m = Model::default();
        let want = [
            0x800, 0x800, 0xA00, 0xA00, 0xA00, 0xA00, 0xA00, 0xA00, 0xA00, 0xB00, 0xA00, 0x800,
            0xB00, 0xB00, 0xB00, 0xB00, 0xB00, 0xB00, 0xC00, 0xC00, 0xB00, 0xB00, 0, 0, 0, 0,
        ];
        for (pin, want) in want.iter().enumerate() {
            assert_eq!(m.pad(pin as u8), *want, "GPIO{pin}");
        }
        assert_eq!(m.pin_ctrl(), 0);
        // The capture's GPIO0 read, 0x802, is this with IDF's sleep sweep on top.
        assert_eq!(m.pad(0) | SLP_SEL.mask(), 0x802);
    }

    #[test]
    fn the_rest_of_the_window_is_a_hole() {
        let (mut m, mut l) = model();
        let hole = OFF_PAD0 + u32::from(PAD_COUNT) * 4;
        m.store(hole, Size::B4, 0xFFFF_FFFF, T, &mut l);
        assert_eq!(m.load(hole, Size::B4, T, &mut l), 0);
        assert_eq!(m.fidelity(hole), Fidelity::U);
    }
}
