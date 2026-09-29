//! The analog register bus master at 0x6000E000 and the analog registers behind it
//! (`specs/blocks/regi2c.toml`).
//!
//! The slave side is a byte array indexed by (block id, register): a read answers the last byte
//! written. ROM `rom_i2c_writeReg_Mask` is a software read-modify-write, so a master that
//! answered every read with 0xFF (as QEMU does) boots but corrupts every masked write.
//!
//! A command completes inside the write that issues it, so `BUSY` (bit 25) always reads 0 and a
//! read command's byte is already in bits 23:16 (row `regi2c.busy`, `within = "same_access"`).
//!
//! Register names other than the three `ANA_CONF` ones (IDF `regi2c_defs.h:12-24`) are UNVERIFIED,
//! decoded from ROM disassembly. The block has no rows in `specs/c3-registers.csv`, so its window
//! is a [`RegBank`]. The one nonzero slave default is [`ULP_DONE_FLAGS`]: without it the o-code
//! calibration loop of `rtc_init.c:216-227` spins for its whole 10 ms timeout.

use std::collections::BTreeMap;

use pemu_core::fidelity::{Fidelity, FidelityLedger};
use pemu_core::regstore::Size;
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::block::Regi2c;
use super::store_only::{RegBank, TouchTag};
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};

pub const OFF_HOST0_CMD: u32 = 0x000;

pub const OFF_HOST1_CMD: u32 = 0x004;

/// `BUSY`, bit 25 of a command register. It always reads 0.
pub const CMD_BUSY: u32 = 1 << 25;

/// The write flag, bit 24 of a command register: 1 stores the data byte, 0 fetches it.
pub const CMD_WRITE: u32 = 1 << 24;

const CMD_DATA_SHIFT: u32 = 16;

const CMD_REG_SHIFT: u32 = 8;

/// Offset of `I2C_MST_ANA_CONF0` (IDF `soc/esp32c3/include/soc/regi2c_defs.h:12`).
pub const OFF_ANA_CONF0: u32 = 0x040;

/// Offset of `ANA_CONFIG` (IDF `regi2c_defs.h:16`).
pub const OFF_ANA_CONFIG: u32 = 0x044;

/// Offset of `ANA_CONFIG2` (IDF `regi2c_defs.h:23`).
pub const OFF_ANA_CONFIG2: u32 = 0x048;

/// `ANA_CONF0` at reset, bits 2 and 3 aside (the bootloader rewrites them before any read): the
/// `probe_campaign_regs` capture reads `0x2100E408` after that write. Whether the other set bits
/// are stored or read-only status is UNVERIFIED; they are stored.
pub const ANA_CONF0_RESET: u32 = 0x2100_E400;

/// Bits of `ANA_CONFIG` the register keeps: the ROM writes the top byte as ones and the device
/// reads it back 0, so it is not implemented.
pub const ANA_CONFIG_BITS: u32 = 0x00FF_FFFF;

/// `ANA_CONFIG2` at reset: bit 2 set, as the device capture reads it (stored or read-only is
/// UNVERIFIED).
pub const ANA_CONFIG2_RESET: u32 = 0x0000_0004;

/// Slave block `I2C_ULP`: the o-code calibration flags and the brownout threshold
/// (`soc/esp32c3/include/soc/regi2c_lp_bias.h:37-43`).
pub const BLOCK_I2C_ULP: u8 = 0x61;

/// Slave block `I2C_BBPLL`, programmed on every switch to the PLL.
pub const BLOCK_I2C_BBPLL: u8 = 0x66;

/// Slave block `I2C_SAR_ADC`: the temperature sensor DAC and the ADC calibration.
pub const BLOCK_I2C_SAR_ADC: u8 = 0x69;

/// Slave block `I2C_BIAS`.
pub const BLOCK_I2C_BIAS: u8 = 0x6A;

/// Slave block `I2C_DIG_REG`: the LDO dbias values `rtc_init` takes from eFuse.
pub const BLOCK_I2C_DIG_REG: u8 = 0x6D;

/// The one slave register with a nonzero default: `I2C_ULP` register 3 with `O_DONE_FLAG` (bit 0)
/// and `BG_O_DONE_FLAG` (bit 3) set.
pub const ULP_DONE_FLAGS: (u8, u8, u8) = (BLOCK_I2C_ULP, 3, 0x09);

pub type Model = Regi2cModel;

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Regi2cModel {
    /// The master registers: stored like an unmodeled block, with command semantics on top.
    bank: RegBank,
    /// The analog register space, keyed by `(block id, register)`.
    slave: BTreeMap<u16, u8>,
}

impl Default for Regi2cModel {
    fn default() -> Self {
        let mut m = Regi2cModel {
            bank: RegBank::new(<Regi2c as Block>::SIZE),
            slave: BTreeMap::new(),
        };
        m.apply_conf_defaults();
        m.apply_slave_defaults();
        m
    }
}

const TAG: TouchTag = TouchTag {
    periph: <Regi2c as Block>::ID,
    allowlisted: false,
};

const fn slave_key(block: u8, reg: u8) -> u16 {
    ((block as u16) << 8) | reg as u16
}

impl Regi2cModel {
    /// The byte a read of `(block, reg)` answers: the last byte written there, else the default.
    pub fn slave_byte(&self, block: u8, reg: u8) -> u8 {
        self.slave.get(&slave_key(block, reg)).copied().unwrap_or(0)
    }

    /// Writes a slave byte from outside the guest.
    pub fn set_slave_byte(&mut self, block: u8, reg: u8, value: u8) {
        self.slave.insert(slave_key(block, reg), value);
    }

    /// The analog space as key and byte pairs, in key order, for a snapshot report.
    pub fn slave_registers(&self) -> impl Iterator<Item = (u8, u8, u8)> + '_ {
        self.slave
            .iter()
            .map(|(key, value)| ((key >> 8) as u8, (key & 0xFF) as u8, *value))
    }

    /// Restores the master registers and the analog space. Whether the analog bytes survive a
    /// core reset is UNVERIFIED; the guest reprograms them in `rtc_init` on every boot.
    pub fn reset_to(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.bank.reset();
        self.apply_conf_defaults();
        self.slave.clear();
        self.apply_slave_defaults();
    }

    /// The `ANA_CONF` reset values the device shows; every other master register resets to 0.
    fn apply_conf_defaults(&mut self) {
        self.bank.set(OFF_ANA_CONF0, ANA_CONF0_RESET);
        self.bank.set(OFF_ANA_CONFIG2, ANA_CONFIG2_RESET);
    }

    fn apply_slave_defaults(&mut self) {
        let (block, reg, value) = ULP_DONE_FLAGS;
        self.set_slave_byte(block, reg, value);
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.bank.read(off, size, now, ledger, TAG)
    }

    /// Writes the low `size` bytes of `val` at block offset `off`, runs the command a host
    /// register write issues and reports the first touch.
    ///
    /// The command runs when the access reaches byte 3, which holds the write flag and `BUSY`.
    /// The ROM writes whole words; the rule for narrower writes is UNVERIFIED and keeps a
    /// partially built command from reaching the analog space.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        self.bank.write(off, size, val, now, ledger, TAG);
        let host = off & !3;
        if host == OFF_ANA_CONFIG {
            let kept = self.bank.get(OFF_ANA_CONFIG) & ANA_CONFIG_BITS;
            self.bank.set(OFF_ANA_CONFIG, kept);
        }
        if host != OFF_HOST0_CMD && host != OFF_HOST1_CMD {
            return;
        }
        if (off & 3) + size as u32 <= 3 {
            return;
        }
        self.run_command(host, now, ledger);
    }

    /// Executes the command in the host register at `off` and writes the result back, `BUSY`
    /// clear.
    fn run_command(&mut self, off: u32, now: VTime, ledger: &mut FidelityLedger) {
        let cmd = self.bank.read(off, Size::B4, now, ledger, TAG);
        let block = (cmd & 0xFF) as u8;
        let reg = ((cmd >> CMD_REG_SHIFT) & 0xFF) as u8;
        let data = ((cmd >> CMD_DATA_SHIFT) & 0xFF) as u8;
        let byte = if cmd & CMD_WRITE != 0 {
            self.set_slave_byte(block, reg, data);
            data
        } else {
            self.slave_byte(block, reg)
        };
        let result =
            (cmd & !(0xFF << CMD_DATA_SHIFT) & !CMD_BUSY) | (u32::from(byte) << CMD_DATA_SHIFT);
        self.bank.write(off, Size::B4, result, now, ledger, TAG);
    }
}

impl Peripheral for Regi2cModel {
    const ID: PeriphId = <Regi2c as Block>::ID;
    const BASE: u32 = <Regi2c as Block>::BASE;
    const SIZE: u32 = <Regi2c as Block>::SIZE;

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

    /// The class `specs/blocks/regi2c.toml` gives the register at `off`, rendered by codegen
    /// into [`crate::gen::classes::regi2c`] since the block has no generated register table.
    fn fidelity(&self, off: u32) -> Fidelity {
        crate::r#gen::classes::regi2c::class_at(off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::reset::ResetCause;

    const T: VTime = VTime(5);

    /// The boot writes a read-modify-write of bits 2 and 3, whole words of `0xFFFBFFFF` and
    /// read-modify-writes setting bits 16 to 9; the `probe_campaign_regs` capture reads these.
    #[test]
    fn the_ana_conf_registers_read_the_device_boot_values() {
        let mut m = Regi2cModel::default();
        let mut l = FidelityLedger::default();
        let t = VTime(0);
        let conf0 = m.load(OFF_ANA_CONF0, Size::B4, t, &mut l);
        m.store(OFF_ANA_CONF0, Size::B4, (conf0 & !0x4) | 0x8, t, &mut l);
        m.store(OFF_ANA_CONFIG, Size::B4, 0xFFFB_FFFF, t, &mut l);
        let conf2 = m.load(OFF_ANA_CONFIG2, Size::B4, t, &mut l);
        m.store(OFF_ANA_CONFIG2, Size::B4, conf2 | 0x0001_FE00, t, &mut l);
        assert_eq!(m.load(OFF_ANA_CONF0, Size::B4, t, &mut l), 0x2100_E408);
        assert_eq!(m.load(OFF_ANA_CONFIG, Size::B4, t, &mut l), 0x00FB_FFFF);
        assert_eq!(m.load(OFF_ANA_CONFIG2, Size::B4, t, &mut l), 0x0001_FE04);
        m.reset_to(ResetKind::of(ResetCause(0x01)).expect("a documented power-on reset"));
        assert_eq!(m.load(OFF_ANA_CONF0, Size::B4, t, &mut l), ANA_CONF0_RESET);
        assert_eq!(
            m.load(OFF_ANA_CONFIG2, Size::B4, t, &mut l),
            ANA_CONFIG2_RESET
        );
    }

    fn model() -> (Regi2cModel, FidelityLedger) {
        (Regi2cModel::default(), FidelityLedger::default())
    }

    /// The word ROM `rom1_chip_i2c_writeReg` builds.
    fn write_cmd(block: u8, reg: u8, data: u8) -> u32 {
        u32::from(block) | (u32::from(reg) << 8) | (u32::from(data) << 16) | 0x0500_0000
    }

    /// The word ROM `rom_chip_i2c_readReg_org` builds.
    fn read_cmd(block: u8, reg: u8) -> u32 {
        u32::from(block) | (u32::from(reg) << 8) | 0x0400_0000
    }

    #[test]
    fn identity_matches_the_c3_devices_row() {
        assert_eq!(<Regi2cModel as Peripheral>::ID, <Regi2c as Block>::ID);
        assert_eq!(<Regi2cModel as Peripheral>::BASE, 0x6000_E000);
        assert_eq!(<Regi2cModel as Peripheral>::SIZE, 0x1000);
    }

    #[test]
    fn wait_row_regi2c_busy_reads_zero_on_both_hosts() {
        let (mut m, mut l) = model();
        for host in [OFF_HOST0_CMD, OFF_HOST1_CMD] {
            m.store(
                host,
                Size::B4,
                write_cmd(BLOCK_I2C_BBPLL, 2, 0x50),
                T,
                &mut l,
            );
            assert_eq!(m.load(host, Size::B4, T, &mut l) & CMD_BUSY, 0, "{host:#X}");
            m.store(host, Size::B4, read_cmd(BLOCK_I2C_BBPLL, 2), T, &mut l);
            assert_eq!(m.load(host, Size::B4, T, &mut l) & CMD_BUSY, 0, "{host:#X}");
        }
    }

    #[test]
    fn a_read_command_answers_the_last_written_byte() {
        let (mut m, mut l) = model();
        m.store(
            OFF_HOST0_CMD,
            Size::B4,
            write_cmd(BLOCK_I2C_BBPLL, 3, 8),
            T,
            &mut l,
        );
        m.store(
            OFF_HOST1_CMD,
            Size::B4,
            read_cmd(BLOCK_I2C_BBPLL, 3),
            T,
            &mut l,
        );
        let result = m.load(OFF_HOST1_CMD, Size::B4, T, &mut l);
        assert_eq!((result >> CMD_DATA_SHIFT) & 0xFF, 8);
        assert_eq!(
            result & 0xFF,
            u32::from(BLOCK_I2C_BBPLL),
            "the block id stays"
        );
        assert_eq!((result >> 8) & 0xFF, 3, "the register number stays");
        assert_eq!(m.slave_byte(BLOCK_I2C_BBPLL, 3), 8);
        assert_eq!(m.slave_byte(BLOCK_I2C_BBPLL, 4), 0, "never written reads 0");
    }

    /// ROM `rom_i2c_writeReg_Mask` is a software read-modify-write: the reason for the store
    /// model.
    #[test]
    fn a_masked_update_keeps_the_bits_it_does_not_write() {
        let (mut m, mut l) = model();
        let (block, reg) = (BLOCK_I2C_DIG_REG, 6);
        m.store(
            OFF_HOST0_CMD,
            Size::B4,
            write_cmd(block, reg, 0b1010_0101),
            T,
            &mut l,
        );
        m.store(OFF_HOST0_CMD, Size::B4, read_cmd(block, reg), T, &mut l);
        let old = ((m.load(OFF_HOST0_CMD, Size::B4, T, &mut l) >> CMD_DATA_SHIFT) & 0xFF) as u8;
        let new = (old & !0x0F) | 0x03;
        m.store(
            OFF_HOST0_CMD,
            Size::B4,
            write_cmd(block, reg, new),
            T,
            &mut l,
        );
        assert_eq!(m.slave_byte(block, reg), 0b1010_0011);
    }

    #[test]
    fn the_ulp_done_flags_read_nine_before_anything_writes_them() {
        let (mut m, mut l) = model();
        let (block, reg, value) = ULP_DONE_FLAGS;
        assert_eq!(m.slave_byte(block, reg), value);
        m.store(OFF_HOST0_CMD, Size::B4, read_cmd(block, reg), T, &mut l);
        let result = m.load(OFF_HOST0_CMD, Size::B4, T, &mut l);
        assert_eq!((result >> CMD_DATA_SHIFT) & 0xFF, u32::from(value));
        assert_eq!(value & 0x1, 1, "O_DONE_FLAG");
        assert_eq!((value >> 3) & 0x1, 1, "BG_O_DONE_FLAG");

        m.store(OFF_HOST0_CMD, Size::B4, write_cmd(block, reg, 0), T, &mut l);
        assert_eq!(m.slave_byte(block, reg), 0);
        m.reset_to(ResetKind::of(ResetCause::POWERON).expect("documented"));
        assert_eq!(m.slave_byte(block, reg), value, "a reset seeds it again");
    }

    #[test]
    fn the_analog_configuration_registers_are_storage() {
        let (mut m, mut l) = model();
        for (off, reset) in [
            (OFF_ANA_CONF0, ANA_CONF0_RESET),
            (OFF_ANA_CONFIG, 0),
            (OFF_ANA_CONFIG2, ANA_CONFIG2_RESET),
        ] {
            assert_eq!(m.load(off, Size::B4, T, &mut l), reset, "{off:#X}");
            m.store(off, Size::B4, 0x0002_0000, T, &mut l);
            assert_eq!(m.load(off, Size::B4, T, &mut l), 0x0002_0000, "{off:#X}");
        }
    }

    #[test]
    fn a_narrow_write_below_the_command_byte_runs_nothing() {
        let (mut m, mut l) = model();
        m.store(OFF_HOST0_CMD, Size::B1, u32::from(BLOCK_I2C_ULP), T, &mut l);
        m.store(OFF_HOST0_CMD + 1, Size::B1, 7, T, &mut l);
        m.store(OFF_HOST0_CMD + 2, Size::B1, 0x42, T, &mut l);
        assert_eq!(m.slave_byte(BLOCK_I2C_ULP, 7), 0, "no command ran yet");
        m.store(OFF_HOST0_CMD + 3, Size::B1, 0x05, T, &mut l);
        assert_eq!(m.slave_byte(BLOCK_I2C_ULP, 7), 0x42);
    }

    #[test]
    fn reset_scope_matrix_over_the_master_and_the_analog_space() {
        let (mut m, mut l) = model();
        let kind = |c| ResetKind::of(c).expect("a documented reset cause");
        m.store(OFF_ANA_CONFIG, Size::B4, 0x1234, T, &mut l);
        m.set_slave_byte(BLOCK_I2C_SAR_ADC, 6, 0x0F);

        m.reset_to(kind(ResetCause::RTC_SW_CPU));
        assert_eq!(m.load(OFF_ANA_CONFIG, Size::B4, T, &mut l), 0x1234);
        assert_eq!(m.slave_byte(BLOCK_I2C_SAR_ADC, 6), 0x0F);

        m.reset_to(kind(ResetCause::RTC_SW_SYS));
        assert_eq!(m.load(OFF_ANA_CONFIG, Size::B4, T, &mut l), 0);
        assert_eq!(m.slave_byte(BLOCK_I2C_SAR_ADC, 6), 0);
    }

    #[test]
    fn the_analog_space_is_listed_in_key_order() {
        let mut m = Regi2cModel::default();
        m.set_slave_byte(BLOCK_I2C_BIAS, 1, 0x11);
        m.set_slave_byte(BLOCK_I2C_BBPLL, 9, 0x22);
        let rows: Vec<_> = m.slave_registers().collect();
        assert_eq!(
            rows,
            vec![
                (BLOCK_I2C_ULP, 3, 0x09),
                (BLOCK_I2C_BBPLL, 9, 0x22),
                (BLOCK_I2C_BIAS, 1, 0x11),
            ]
        );
    }

    #[test]
    fn first_touches_are_reported_once_per_register() {
        let (mut m, mut l) = model();
        m.load(OFF_HOST0_CMD, Size::B4, VTime(1), &mut l);
        m.store(OFF_HOST0_CMD, Size::B4, read_cmd(0x61, 0), VTime(2), &mut l);
        m.load(OFF_ANA_CONFIG, Size::B4, VTime(3), &mut l);
        let offs: Vec<_> = l.first_touches().iter().map(|t| t.off).collect();
        assert_eq!(offs, vec![OFF_HOST0_CMD, OFF_ANA_CONFIG]);
    }

    #[test]
    fn the_command_register_carries_the_block_class() {
        let m = Regi2cModel::default();
        assert_eq!(m.fidelity(OFF_HOST0_CMD), Fidelity::B);
        assert_eq!(m.fidelity(OFF_HOST1_CMD), Fidelity::B);
        assert_eq!(
            m.fidelity(OFF_ANA_CONFIG),
            Fidelity::C,
            "stored, with the master enables deliberately not honored              (specs/blocks/regi2c.toml)"
        );
        assert_eq!(
            m.fidelity(0x100),
            Fidelity::U,
            "an offset no row of the block file names stays U"
        );
    }
}
