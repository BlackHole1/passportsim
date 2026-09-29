//! Synchronous traps and the trap entry and return sequences of the ESP32-C3 hart (ESP32-C3 TRM
//! chapter 1, Registers 1.7-1.11).
//!
//! Exceptions are precise: nothing of the trapping instruction commits and `mepc` names it. The
//! run loop takes interrupts between `Engine::run` calls. The vector table is vectored:
//! exceptions enter at BASE and interrupt line n at BASE + 4n (IDF `riscv/vectors_intc.S:35-42`).
//! Everything runs in machine mode and the hart has no privilege field, so trap entry stores
//! MPP = 0b11 and `mret` stays in machine mode.

use crate::cost::Pipe;
use crate::csr::{
    Csr, MSTATUS_MIE, MSTATUS_MPIE, MSTATUS_MPP, MTVEC_BASE_MASK, TCONTROL_MPTE, TCONTROL_MTE,
};
use crate::exec::Hart;

/// `mcause` exception codes (TRM Register 1.10); mtval holds the pc, the instruction bits or the
/// data address.
pub const EXC_INSN_ACCESS_FAULT: u32 = 1;
pub const EXC_ILLEGAL_INSN: u32 = 2;
pub const EXC_BREAKPOINT: u32 = 3;
pub const EXC_LOAD_ACCESS_FAULT: u32 = 5;
pub const EXC_STORE_ACCESS_FAULT: u32 = 7;
pub const EXC_ECALL_U: u32 = 8;
pub const EXC_ECALL_M: u32 = 11;
pub const MCAUSE_INTERRUPT: u32 = 1 << 31;

/// A synchronous exception: the `mcause` and `mtval` values the trap writes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Trap {
    pub cause: u32,
    pub tval: u32,
}

impl Trap {
    pub const fn illegal_instruction(insn: u32) -> Trap {
        Trap {
            cause: EXC_ILLEGAL_INSN,
            tval: insn,
        }
    }

    pub const fn instruction_access_fault(pc: u32) -> Trap {
        Trap {
            cause: EXC_INSN_ACCESS_FAULT,
            tval: pc,
        }
    }

    pub const fn load_access_fault(addr: u32) -> Trap {
        Trap {
            cause: EXC_LOAD_ACCESS_FAULT,
            tval: addr,
        }
    }

    pub const fn store_access_fault(addr: u32) -> Trap {
        Trap {
            cause: EXC_STORE_ACCESS_FAULT,
            tval: addr,
        }
    }

    /// The TRM leaves mtval invalid; the pc follows the privileged specification (UNVERIFIED).
    pub const fn breakpoint(pc: u32) -> Trap {
        Trap {
            cause: EXC_BREAKPOINT,
            tval: pc,
        }
    }

    pub const fn ecall_from_machine() -> Trap {
        Trap {
            cause: EXC_ECALL_M,
            tval: 0,
        }
    }
}

/// Takes an exception precisely at the instruction at `epc`: sets mepc, mcause and mtval, moves
/// MIE to MPIE and MTE to MPTE, and jumps to the vector base. `Hart::insns` does not move.
pub fn take_exception(hart: &mut Hart, epc: u32, trap: Trap) {
    debug_assert_eq!(trap.cause & MCAUSE_INTERRUPT, 0, "exception cause {trap:?}");
    let csr = &mut hart.csr;
    csr.mepc = epc & !1;
    csr.mcause = trap.cause;
    csr.mtval = trap.tval;
    enter(csr);
    hart.pc = csr.mtvec & MTVEC_BASE_MASK;
    // A trap flushes the pipeline: nothing before it stalls the handler's first instruction.
    hart.pipe = Pipe::default();
}

/// Takes CPU interrupt `line` (1 to 31) before the instruction at `Hart::pc`, with mtval 0
/// (UNVERIFIED), and clears `Hart::wfi`. The caller checks `Csr::mstatus_mie` and the INTC first.
pub fn take_interrupt(hart: &mut Hart, line: u8) {
    assert!(
        (1..=31).contains(&line),
        "CPU interrupt {line} out of 1..=31"
    );
    let csr = &mut hart.csr;
    csr.mepc = hart.pc & !1;
    csr.mcause = MCAUSE_INTERRUPT | u32::from(line);
    csr.mtval = 0;
    enter(csr);
    hart.pc = (csr.mtvec & MTVEC_BASE_MASK).wrapping_add(4 * u32::from(line));
    hart.wfi = false;
    hart.pipe = Pipe::default();
}

/// Sets MIE to MPIE, MPIE to 1 and MPP to 0b00 as the privileged specification recommends with
/// U mode (the TRM wording that clears MPIE is UNVERIFIED and unobservable by IDF).
pub fn mret(hart: &mut Hart) {
    let csr = &mut hart.csr;
    let mie = if csr.mstatus & MSTATUS_MPIE != 0 {
        MSTATUS_MIE
    } else {
        0
    };
    csr.mstatus = (csr.mstatus & !(MSTATUS_MIE | MSTATUS_MPP)) | MSTATUS_MPIE | mie;
    let mte = if csr.tcontrol & TCONTROL_MPTE != 0 {
        TCONTROL_MTE
    } else {
        0
    };
    csr.tcontrol = (csr.tcontrol & !TCONTROL_MTE) | mte;
    hart.pc = csr.mepc & !1;
}

fn enter(csr: &mut Csr) {
    let mpie = if csr.mstatus & MSTATUS_MIE != 0 {
        MSTATUS_MPIE
    } else {
        0
    };
    csr.mstatus = (csr.mstatus & !(MSTATUS_MIE | MSTATUS_MPIE)) | MSTATUS_MPP | mpie;
    let mpte = if csr.tcontrol & TCONTROL_MTE != 0 {
        TCONTROL_MPTE
    } else {
        0
    };
    csr.tcontrol = (csr.tcontrol & !(TCONTROL_MTE | TCONTROL_MPTE)) | mpte;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spmon::SpMonitor;

    const BASE: u32 = 0x4038_0000;

    fn hart() -> Hart {
        let mut csr = Csr::new();
        csr.mtvec = BASE | 1;
        let mut x = [0; 32];
        x[10] = 0x1234;
        Hart {
            x,
            pc: 0x4200_1234,
            csr,
            wfi: false,
            insns: 77,
            stores: 3,
            spmon: SpMonitor::default(),
            extra: 0,
            pipe: Default::default(),
        }
    }

    #[test]
    fn exception_entry_is_precise() {
        let mut h = hart();
        h.csr.mstatus = MSTATUS_MIE | 1;
        take_exception(&mut h, 0x4200_1230, Trap::load_access_fault(0x10));
        assert_eq!(h.pc, BASE);
        assert_eq!(h.csr.mepc, 0x4200_1230);
        assert_eq!(h.csr.mcause, EXC_LOAD_ACCESS_FAULT);
        assert_eq!(h.csr.mtval, 0x10);
        assert_eq!(
            h.csr.mstatus, 0x0000_1881,
            "MPIE from MIE, MIE clear, MPP machine"
        );
        assert_eq!((h.insns, h.stores, h.x[10]), (77, 3, 0x1234));
    }

    #[test]
    fn exception_entry_with_interrupts_disabled_clears_mpie() {
        let mut h = hart();
        h.csr.mstatus = MSTATUS_MPIE;
        take_exception(&mut h, 0x4200_0000, Trap::illegal_instruction(0x3020_0073));
        assert_eq!(h.csr.mstatus, MSTATUS_MPP);
        assert_eq!(h.csr.mcause, EXC_ILLEGAL_INSN);
        assert_eq!(h.csr.mtval, 0x3020_0073);
    }

    #[test]
    fn trap_constructors_set_cause_and_tval() {
        assert_eq!(Trap::instruction_access_fault(0x44).cause, 1);
        assert_eq!(Trap::instruction_access_fault(0x44).tval, 0x44);
        assert_eq!(Trap::store_access_fault(0x3C00_0000).cause, 7);
        assert_eq!(
            Trap::breakpoint(0x4200_0010),
            Trap {
                cause: 3,
                tval: 0x4200_0010
            }
        );
        assert_eq!(Trap::ecall_from_machine(), Trap { cause: 11, tval: 0 });
    }

    #[test]
    fn interrupt_entry_uses_the_vector_slot_of_its_line() {
        let mut h = hart();
        h.pc = 0x4200_2000;
        h.wfi = true;
        h.csr.mstatus = MSTATUS_MIE;
        h.csr.mtval = 0xFFFF;
        take_interrupt(&mut h, 5);
        assert_eq!(h.pc, BASE + 4 * 5);
        assert_eq!(h.csr.mepc, 0x4200_2000);
        assert_eq!(h.csr.mcause, 0x8000_0005);
        assert_eq!(h.csr.mtval, 0);
        assert_eq!(h.csr.mstatus, MSTATUS_MPIE | MSTATUS_MPP);
        assert!(!h.wfi);
        assert_eq!(h.insns, 77);

        let mut h = hart();
        take_interrupt(&mut h, 31);
        assert_eq!(h.pc, BASE + 0x7C);
    }

    #[test]
    #[should_panic(expected = "out of 1..=31")]
    fn interrupt_line_0_is_rejected() {
        take_interrupt(&mut hart(), 0);
    }

    #[test]
    fn mret_restores_mie_and_returns_to_mepc() {
        let mut h = hart();
        h.csr.mstatus = MSTATUS_MIE | 1;
        take_interrupt(&mut h, 3);
        assert!(!h.csr.mstatus_mie());
        mret(&mut h);
        assert_eq!(h.pc, 0x4200_1234);
        assert_eq!(h.csr.mstatus, MSTATUS_MIE | MSTATUS_MPIE | 1);
    }

    #[test]
    fn mret_with_mpie_clear_leaves_mie_clear() {
        let mut h = hart();
        h.csr.mstatus = MSTATUS_MPP;
        h.csr.mepc = 0x4038_0100;
        mret(&mut h);
        assert_eq!(h.pc, 0x4038_0100);
        assert_eq!(h.csr.mstatus, MSTATUS_MPIE);
    }

    #[test]
    fn trap_entry_and_mret_move_the_trigger_enable() {
        let mut h = hart();
        h.csr.tcontrol = TCONTROL_MTE;
        take_exception(&mut h, 0x4200_0000, Trap::breakpoint(0x4200_0000));
        assert_eq!(h.csr.tcontrol, TCONTROL_MPTE);
        mret(&mut h);
        assert_eq!(h.csr.tcontrol, TCONTROL_MTE | TCONTROL_MPTE);

        let mut h = hart();
        take_interrupt(&mut h, 1);
        assert_eq!(h.csr.tcontrol, 0);
        mret(&mut h);
        assert_eq!(h.csr.tcontrol, 0);
    }
}
