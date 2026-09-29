//! eFuse controller at 0x60008800 (`specs/blocks/efuse.toml`): a read-only window onto the
//! image the machine supplies, plus the command register the boot path polls.
//!
//! A `READ_CMD` reloads the `RD_*` shadow from the image, as ROM `ets_efuse_read` and IDF
//! `efuse_hal.c:38-48` do. Programming is refused: `PGM_CMD` completes, sets `PGM_DONE` and
//! changes nothing, the safe outcome for a device twin. The error registers stay 0, the only
//! value `efuse_hal.c:75-97` accepts. The interrupt path onto source 24 is not modeled; only the
//! raw done bits latch.
//!
//! The image is synthesized by the machine; no real eFuse word is committed (`docs/secrets.md`).

use pemu_core::fidelity::{Fidelity, FidelityLedger, TouchAccess};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::block::Efuse;
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};
use crate::r#gen::regs_efuse::{REG_COUNT, REGS, idx};
use crate::regs::{self, TouchAt};

/// Words of the `RD_*` shadow window, `EFUSE_RD_WR_DIS` (0x02C) through
/// `EFUSE_RD_SYS_PART2_DATA7` (0x178): the layout of an espefuse dump.
pub const RD_WORDS: usize = 84;

const RD_FIRST: usize = idx::EFUSE_RD_WR_DIS;

/// `EFUSE_CONF.OP_CODE` that arms a read command (IDF `soc/esp32c3/include/soc/efuse_defs.h:12`).
pub const OP_CODE_READ: u32 = 0x5AA5;

/// `EFUSE_CONF.OP_CODE` that arms a program command (IDF
/// `soc/esp32c3/include/soc/efuse_defs.h:13`).
pub const OP_CODE_WRITE: u32 = 0x5A5A;

const CMD_READ: u32 = 1 << 0;

const CMD_PGM: u32 = 1 << 1;

const INT_READ_DONE: u32 = 1 << 0;

const INT_PGM_DONE: u32 = 1 << 1;

const TOUCH_WORDS: usize = REG_COUNT.div_ceil(64);

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    #[serde(with = "regs_serde")]
    regs: RegStore<REG_COUNT>,
    touched: [u64; TOUCH_WORDS],
    /// The eFuse array as [`RD_WORDS`] words, all zero before the machine loads an image.
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, RD_WORDS>")]
    image: Vec<u32>,
    refused_burns: u32,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            regs: RegStore::new(&REGS),
            touched: [0; TOUCH_WORDS],
            image: vec![0; RD_WORDS],
            refused_burns: 0,
        }
    }
}

impl Model {
    /// Loads the eFuse array and reloads the `RD_*` shadow, as a power-on does. Words past
    /// [`RD_WORDS`] are ignored and a short list leaves the rest zero.
    pub fn load_image(&mut self, words: &[u32]) {
        self.image = vec![0; RD_WORDS];
        for (slot, word) in self.image.iter_mut().zip(words) {
            *slot = *word;
        }
        self.reload();
    }

    /// Zeroes the eFuse array and the `RD_*` shadow and keeps everything else: the form a
    /// snapshot export without secrets carries. The restoring machine reloads its own image.
    pub fn redact_image(&mut self) {
        self.load_image(&[]);
    }

    pub fn image(&self) -> &[u32] {
        &self.image
    }

    /// Program commands refused since power-on. The receipt reports a nonzero count, because
    /// espefuse then fails verification and the agent has to know why.
    pub fn refused_burns(&self) -> u32 {
        self.refused_burns
    }

    /// Copies the image into the `RD_*` shadow, as a `READ_CMD` and every reset that reaches the
    /// block do.
    fn reload(&mut self) {
        for i in 0..RD_WORDS {
            self.regs
                .set(RD_FIRST + i, self.image.get(i).copied().unwrap_or(0));
        }
    }

    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    /// Restores the registers of the scopes `kind` clears and reloads the `RD_*` shadow. A
    /// `CPU0_` reset reaches no block.
    pub fn reset_to(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.regs.reset(kind.scope);
        self.reload();
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

    /// Writes the low `size` bytes of `val` at block offset `off`, runs a command the write
    /// triggers and reports the first touch.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((i, byte)) = regs::reg_at(&REGS, off) else {
            regs::hole(off, TouchAccess::Write, at, ledger);
            return;
        };
        regs::touch(&mut self.touched, &REGS, i, TouchAccess::Write, at, ledger);
        let delta = self.regs.write(i, byte, size, val);
        if i == idx::EFUSE_CMD && delta.triggers != 0 {
            self.run_command(delta.triggers);
        }
    }

    /// Completes the command bits written 1 inside the same access (spec row `efuse.cmd`).
    ///
    /// Only the opcode decides whether a read reloads or a program is counted. Both bits clear
    /// and set their done bit whatever `OP_CODE` holds, so a boot that wrote the opcode in an
    /// unexpected order still does not hang.
    fn run_command(&mut self, triggers: u32) {
        let op = self.regs.get(idx::EFUSE_CONF) & 0xFFFF;
        let mut int_raw = self.regs.get(idx::EFUSE_INT_RAW);
        if triggers & CMD_READ != 0 {
            if op == OP_CODE_READ {
                self.reload();
            }
            int_raw |= INT_READ_DONE;
        }
        if triggers & CMD_PGM != 0 {
            if op == OP_CODE_WRITE {
                self.refused_burns = self.refused_burns.saturating_add(1);
            }
            int_raw |= INT_PGM_DONE;
        }
        self.regs.set(idx::EFUSE_INT_RAW, int_raw);
        self.regs.clear_sc(idx::EFUSE_CMD, CMD_READ | CMD_PGM);
    }
}

impl Peripheral for Model {
    const ID: PeriphId = <Efuse as Block>::ID;
    const BASE: u32 = <Efuse as Block>::BASE;
    const SIZE: u32 = <Efuse as Block>::SIZE;

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

    /// Nothing here changes with time: commands complete inside the write, and only a new image
    /// changes a value.
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

    const T: VTime = VTime(1_000);

    const OFF_RD_WR_DIS: u32 = 0x02C;
    const OFF_RD_REPEAT_ERR0: u32 = 0x17C;
    const OFF_RD_RS_ERR0: u32 = 0x1C0;
    const OFF_CONF: u32 = 0x1CC;
    const OFF_STATUS: u32 = 0x1D0;
    const OFF_CMD: u32 = 0x1D4;
    const OFF_INT_RAW: u32 = 0x1D8;

    /// A placeholder image, never a device value; BLOCK1 holds the placeholder MAC of
    /// `docs/secrets.md`.
    fn placeholder_image() -> Vec<u32> {
        let mut words = vec![0u32; RD_WORDS];
        // BLOCK0 w4 `RD_REPEAT_DATA3`: ERR_RST_ENABLE.
        words[5] = 0x8000_0000;
        // BLOCK1 w0 and w1: 02:00:00:C3:00:01.
        words[6] = 0x00C3_0001;
        words[7] = 0x0000_0200;
        words
    }

    fn model() -> (Model, FidelityLedger) {
        (Model::default(), FidelityLedger::default())
    }

    #[test]
    fn identity_matches_the_c3_devices_row() {
        assert_eq!(<Model as Peripheral>::ID, <Efuse as Block>::ID);
        assert_eq!(<Model as Peripheral>::BASE, 0x6000_8800);
        assert_eq!(<Model as Peripheral>::SIZE, 0x800);
    }

    #[test]
    fn the_rd_window_reads_the_loaded_image() {
        let (mut m, mut l) = model();
        m.load_image(&placeholder_image());
        assert_eq!(m.load(OFF_RD_WR_DIS, Size::B4, T, &mut l), 0);
        assert_eq!(
            m.load(OFF_RD_WR_DIS + 5 * 4, Size::B4, T, &mut l),
            0x8000_0000
        );
        assert_eq!(
            m.load(OFF_RD_WR_DIS + 6 * 4, Size::B4, T, &mut l),
            0x00C3_0001
        );
        assert_eq!(m.load(0x178, Size::B4, T, &mut l), 0);
        // A guest write into the read-only shadow is dropped.
        m.store(OFF_RD_WR_DIS + 6 * 4, Size::B4, 0xDEAD_BEEF, T, &mut l);
        assert_eq!(
            m.load(OFF_RD_WR_DIS + 6 * 4, Size::B4, T, &mut l),
            0x00C3_0001
        );
    }

    #[test]
    fn a_short_image_leaves_the_rest_of_the_window_zero() {
        let (mut m, mut l) = model();
        m.load_image(&[0x1111_1111, 0x2222_2222]);
        assert_eq!(m.load(OFF_RD_WR_DIS, Size::B4, T, &mut l), 0x1111_1111);
        assert_eq!(m.load(OFF_RD_WR_DIS + 4, Size::B4, T, &mut l), 0x2222_2222);
        assert_eq!(m.load(OFF_RD_WR_DIS + 8, Size::B4, T, &mut l), 0);
        assert_eq!(m.image().len(), RD_WORDS);
    }

    #[test]
    fn wait_row_efuse_cmd_self_clears_inside_the_access() {
        let (mut m, mut l) = model();
        m.store(OFF_CONF, Size::B4, OP_CODE_READ, T, &mut l);
        m.store(OFF_CMD, Size::B4, CMD_READ, T, &mut l);
        assert_eq!(m.load(OFF_CMD, Size::B4, T, &mut l), 0, "READ_CMD");
        assert_eq!(m.load(OFF_INT_RAW, Size::B4, T, &mut l), INT_READ_DONE);

        m.store(OFF_CONF, Size::B4, OP_CODE_WRITE, T, &mut l);
        m.store(OFF_CMD, Size::B4, CMD_PGM, T, &mut l);
        assert_eq!(m.load(OFF_CMD, Size::B4, T, &mut l), 0, "PGM_CMD");
        assert_eq!(
            m.load(OFF_INT_RAW, Size::B4, T, &mut l),
            INT_READ_DONE | INT_PGM_DONE
        );
        // A done bit is W1C.
        m.store(OFF_INT_RAW, Size::B4, INT_READ_DONE, T, &mut l);
        assert_eq!(m.load(OFF_INT_RAW, Size::B4, T, &mut l), INT_PGM_DONE);
    }

    #[test]
    fn the_block_number_survives_a_command() {
        let (mut m, mut l) = model();
        m.store(OFF_CMD, Size::B4, CMD_READ | (2 << 2), T, &mut l);
        assert_eq!(m.load(OFF_CMD, Size::B4, T, &mut l), 2 << 2);
    }

    /// The opcode gate has no observable effect while programming is refused; the read-back is
    /// what the boot path depends on.
    #[test]
    fn a_read_command_leaves_the_shadow_equal_to_the_image() {
        let (mut m, mut l) = model();
        m.load_image(&placeholder_image());
        for op in [0, OP_CODE_READ, OP_CODE_WRITE] {
            m.store(OFF_CONF, Size::B4, op, T, &mut l);
            m.store(OFF_CMD, Size::B4, CMD_READ, T, &mut l);
            assert_eq!(m.load(OFF_CMD, Size::B4, T, &mut l), 0, "op {op:#X}");
            for (i, word) in m.image().to_vec().iter().enumerate() {
                let off = OFF_RD_WR_DIS + (i as u32) * 4;
                assert_eq!(m.load(off, Size::B4, T, &mut l), *word, "{off:#X}");
            }
        }
        assert_eq!(m.refused_burns(), 0, "no program command was issued");
    }

    #[test]
    fn programming_is_refused_and_counted() {
        let (mut m, mut l) = model();
        m.load_image(&placeholder_image());
        m.store(OFF_CONF, Size::B4, OP_CODE_WRITE, T, &mut l);
        m.store(0x000, Size::B4, 0xFFFF_FFFF, T, &mut l);
        m.store(OFF_CMD, Size::B4, CMD_PGM, T, &mut l);
        assert_eq!(m.refused_burns(), 1);
        assert_eq!(m.load(OFF_RD_WR_DIS, Size::B4, T, &mut l), 0);
        assert_eq!(m.image()[0], 0);
    }

    #[test]
    fn the_error_registers_read_zero() {
        let (mut m, mut l) = model();
        m.load_image(&placeholder_image());
        for off in [OFF_RD_REPEAT_ERR0, OFF_RD_RS_ERR0, OFF_STATUS] {
            m.store(off, Size::B4, 0xFFFF_FFFF, T, &mut l);
            assert_eq!(m.load(off, Size::B4, T, &mut l), 0, "{off:#X}");
        }
    }

    #[test]
    fn reset_reloads_the_shadow_and_a_cpu_reset_keeps_it() {
        let (mut m, mut l) = model();
        m.load_image(&placeholder_image());
        m.store(OFF_CONF, Size::B4, OP_CODE_READ, T, &mut l);

        let cpu = ResetKind::of(ResetCause::RTC_SW_CPU).expect("0x0C is documented");
        m.reset_to(cpu);
        assert_eq!(
            m.load(OFF_CONF, Size::B4, T, &mut l),
            OP_CODE_READ,
            "a CPU reset keeps every digital block"
        );

        let poweron = ResetKind::of(ResetCause::POWERON).expect("0x01 is documented");
        m.reset_to(poweron);
        assert_eq!(m.load(OFF_CONF, Size::B4, T, &mut l), 0);
        assert_eq!(
            m.load(OFF_RD_WR_DIS + 6 * 4, Size::B4, T, &mut l),
            0x00C3_0001
        );
    }

    #[test]
    fn first_touches_are_reported_once_per_register() {
        let (mut m, mut l) = model();
        m.load(OFF_CMD, Size::B4, VTime(1), &mut l);
        m.store(OFF_CMD, Size::B4, 0, VTime(2), &mut l);
        m.load(OFF_CONF, Size::B4, VTime(3), &mut l);
        let touches: Vec<_> = l.first_touches().iter().map(|t| (t.off, t.now)).collect();
        assert_eq!(touches, vec![(OFF_CMD, VTime(1)), (OFF_CONF, VTime(3))]);
        assert_eq!(l.first_touches()[0].access, TouchAccess::Read);
    }

    #[test]
    fn an_offset_with_no_register_reads_zero_and_is_logged() {
        let (mut m, mut l) = model();
        let hole = 0x7F0;
        m.store(hole, Size::B4, 0xFFFF_FFFF, T, &mut l);
        assert_eq!(m.load(hole, Size::B4, T, &mut l), 0);
        assert_eq!(l.first_touches().len(), 1);
        assert_eq!(l.first_touches()[0].off, hole);
    }

    #[test]
    fn a_narrow_access_reaches_only_its_bytes() {
        let (mut m, mut l) = model();
        m.load_image(&[0x1122_3344]);
        assert_eq!(m.load(OFF_RD_WR_DIS, Size::B1, T, &mut l), 0x44);
        assert_eq!(m.load(OFF_RD_WR_DIS + 3, Size::B1, T, &mut l), 0x11);
        assert_eq!(m.load(OFF_RD_WR_DIS + 2, Size::B2, T, &mut l), 0x1122);
    }

    #[test]
    fn fidelity_comes_from_the_generated_table() {
        let m = Model::default();
        assert_eq!(m.fidelity(OFF_CMD), Fidelity::B);
        assert_eq!(m.fidelity(OFF_RD_RS_ERR0), Fidelity::B);
        assert_eq!(m.fidelity(OFF_CONF), Fidelity::U);
        assert_eq!(m.fidelity(0x7F0), Fidelity::U);
    }
}
