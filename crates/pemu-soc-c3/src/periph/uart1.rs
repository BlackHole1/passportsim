//! UART1 (0x60010000): the register file and nothing else (`specs/blocks/uart1.toml`).
//!
//! Not `StoreOnly`, because the device's read-back matters: `UART_INT_CLR`, which boot writes,
//! is `WT` in every field and reads back 0 on silicon. [`super::reg_file::RegFile`] applies the
//! access column of every field of the generated table. No line is driven, no FIFO moves and no
//! interrupt is raised, so every register is class C. The `uart0` model is not reused: its
//! console capture would make every capture ambiguous.

use pemu_core::fidelity::{Fidelity, FidelityLedger};
use pemu_core::regstore::{RegSpec, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::time::VTime;

use crate::r#gen::regs_uart1::{BLOCK_SIZE, REG_COUNT, REGS};

use super::reg_file::{RegFile, RegTable};
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Wiring, block};

impl RegTable<REG_COUNT> for block::Uart1 {
    const SPECS: &'static [RegSpec; REG_COUNT] = &REGS;
}

/// The UART1 register file.
#[derive(Default, pemu_core::serde::Serialize, pemu_core::serde::Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Uart1 {
    regs: RegFile<block::Uart1, REG_COUNT>,
}

impl Uart1 {
    /// Value of `size` bytes at `off` with the access semantics of each field; reports the first
    /// touch.
    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.regs.read(off, size, now, ledger)
    }

    /// Writes the low `size` bytes of `val` at `off` with the access semantics of each field;
    /// reports the first touch. Nothing else happens.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        self.regs.write(off, size, val, now, ledger);
    }
}

impl Peripheral for Uart1 {
    const ID: PeriphId = <block::Uart1 as Block>::ID;
    const BASE: u32 = <block::Uart1 as Block>::BASE;
    const SIZE: u32 = BLOCK_SIZE;

    fn reset(&mut self, kind: ResetKind, _cx: &mut Cx) {
        self.regs.reset(kind);
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

    fn fidelity(&self, off: u32) -> Fidelity {
        self.regs.class(off)
    }
}

/// Model of the `uart1` row of the `c3_devices!` table.
pub type Model = Uart1;

#[cfg(test)]
mod tests {
    use pemu_core::reset::{ResetCause, ResetFanout, ResetScope};

    use crate::r#gen::regs_uart1::idx;

    use super::*;

    mod off {
        pub const FIFO: u32 = 0x000;
        pub const INT_RAW: u32 = 0x004;
        pub const INT_ST: u32 = 0x008;
        pub const INT_ENA: u32 = 0x00C;
        pub const INT_CLR: u32 = 0x010;
        pub const CLKDIV: u32 = 0x014;
    }

    fn model() -> (Uart1, FidelityLedger) {
        (Uart1::default(), FidelityLedger::default())
    }

    /// `StoreOnly` would read back the written value; the device reads 0.
    #[test]
    fn the_write_trigger_registers_read_zero_like_the_device() {
        let (mut u, mut l) = model();
        u.store(off::INT_CLR, Size::B4, u32::MAX, VTime(0), &mut l);
        assert_eq!(
            u.load(off::INT_CLR, Size::B4, VTime(0), &mut l),
            0,
            "UART_INT_CLR is WT in every field: a write triggers and reads back 0"
        );
        // The raw status never moves off its reset value, which is not 0:
        // `UART_TXFIFO_EMPTY_INT_RAW` resets to 1.
        assert_eq!(
            u.load(off::INT_RAW, Size::B4, VTime(0), &mut l),
            REGS[idx::UART_INT_RAW].reset
        );
        assert_eq!(
            u.load(off::INT_ST, Size::B4, VTime(0), &mut l),
            REGS[idx::UART_INT_ST].reset,
            "the model computes no INT_ST = RAW & ENA: it is stored, which is part of why the \
             block stays class U"
        );
    }

    #[test]
    fn a_read_write_register_still_keeps_what_was_written() {
        let (mut u, mut l) = model();
        u.store(off::INT_ENA, Size::B4, 0x3FF, VTime(0), &mut l);
        assert_eq!(u.load(off::INT_ENA, Size::B4, VTime(0), &mut l), 0x3FF);
        u.store(off::CLKDIV, Size::B4, 0x2B6, VTime(0), &mut l);
        assert_eq!(u.load(off::CLKDIV, Size::B4, VTime(0), &mut l), 0x2B6);
        u.store(off::FIFO, Size::B4, 0x41, VTime(0), &mut l);
        assert_eq!(u.load(off::FIFO, Size::B4, VTime(0), &mut l), 0);
    }

    #[test]
    fn the_ledger_and_the_class_come_from_the_generated_table() {
        let (mut u, mut l) = model();
        u.store(off::INT_CLR, Size::B4, 1, VTime(7), &mut l);
        u.load(off::INT_CLR, Size::B4, VTime(9), &mut l);
        let touches = l.first_touches();
        assert_eq!(touches.len(), 1, "one entry per register, not per access");
        assert_eq!(touches[0].off, off::INT_CLR);
        assert!(!touches[0].allowlisted);

        assert_eq!(u.fidelity(off::INT_CLR), REGS[idx::UART_INT_CLR].class);
        assert_eq!(
            u.fidelity(off::INT_CLR),
            Fidelity::C,
            "the read-back is the device's and nothing behind it is modeled"
        );
        assert_eq!(u.fidelity(BLOCK_SIZE - 4), Fidelity::U);
    }

    #[test]
    fn a_reset_restores_the_written_registers() {
        let (mut u, mut l) = model();
        u.store(off::INT_ENA, Size::B4, 0x3FF, VTime(0), &mut l);
        u.regs.reset(ResetKind {
            cause: ResetCause(0x03),
            scope: ResetScope::Core,
            fanout: ResetFanout::AllBlocks,
        });
        assert_eq!(
            u.load(off::INT_ENA, Size::B4, VTime(0), &mut l),
            REGS[idx::UART_INT_ENA].reset
        );
    }
}
