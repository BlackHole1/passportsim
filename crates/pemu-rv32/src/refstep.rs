//! Uncached reference single-step interpreter, the oracle for engine fuzzing and the riscv-tests
//! run. Semantics follow the ESP32-C3 TRM chapter 1 through [`crate::exec::exec_op`], which the
//! engine shares, so the two differ only in how they get their ops.
//!
//! A 32-bit instruction straddling a 4 KB page boundary takes its high halfword from a second
//! [`Bus::fetch_code`] at `pc + 2`, since the next page has its own translation and permission
//! and may fault alone. A fetch trap is precise at that pc: nothing commits and `Hart::insns`
//! does not move. Block structure means nothing here, so every retiring stop reports
//! [`StepResult::Retired`]; clearing `Hart::wfi` on a wake is the run loop's.

use crate::bus::Bus;
use crate::cost::{self, InsnCosts, Pipe};
use crate::decode::{decode, decode_at, insn_len};
use crate::exec::{Hart, exec_op};
use crate::op::Op;
use crate::trap::{Trap, take_exception};

/// Lenient: an unknown CSR outside the custom ranges reaches `Bus::csr_custom`. UNVERIFIED: the
/// frozen `ref_step` signature carries no `EngineCfg`, so `strict_csr: true` needs a second
/// entry point.
const REF_STRICT_CSR: bool = false;

/// Uncached reference step: engine runs at block sizes 1, 3 and 64 must agree with it.
pub fn ref_step<B: Bus>(hart: &mut Hart, bus: &mut B) -> StepResult {
    let pc = hart.pc;
    let op = match fetch(bus, pc) {
        Ok(op) => op,
        Err(trap) => {
            take_exception(hart, pc, trap);
            return StepResult::Trapped(trap);
        }
    };
    let stepped = exec_op(hart, bus, pc, &op, REF_STRICT_CSR);
    if stepped.refreshes_spmon() {
        hart.spmon = bus.sp_monitor();
    }
    stepped
        .step_result()
        .expect("the decoder never produces K_HOOK or K_FALL")
}

/// [`ref_step`] with a class cost table: the step's extra cycles go to `Hart::extra` when it
/// retires and `Hart::pipe` moves on. A zero table is exactly [`ref_step`].
pub fn ref_step_costed<B: Bus>(hart: &mut Hart, bus: &mut B, costs: &InsnCosts) -> StepResult {
    if costs.is_zero() {
        return ref_step(hart, bus);
    }
    let pc = hart.pc;
    let op = match fetch(bus, pc) {
        Ok(op) => op,
        Err(trap) => {
            take_exception(hart, pc, trap);
            return StepResult::Trapped(trap);
        }
    };
    let before = hart.pipe;
    let a = hart.x[(op.rs1 & 31) as usize];
    let b = hart.x[(op.rs2 & 31) as usize];
    let taken = cost::branch_taken(op.kind, a, b);
    // The data address is read before the op runs: a load may overwrite its base register.
    let mmio = cost::mmio_extra(costs, bus, &op, cost::data_addr(&op, a));
    let (bank_extra, bank) = cost::bank_step(costs, before, pc, &op, a);
    let insns = hart.insns;
    let stepped = exec_op(hart, bus, pc, &op, REF_STRICT_CSR);
    if hart.insns != insns {
        hart.extra += u64::from(
            cost::entry_extra(costs, before, pc, &op)
                + bank_extra
                + cost::div_extra(costs, op.kind, a, b)
                + cost::branch_extra(costs, op.kind, taken)
                + mmio,
        );
        hart.pipe = Pipe::after(&op, taken, bank);
    }
    if stepped.refreshes_spmon() {
        hart.spmon = bus.sp_monitor();
    }
    stepped
        .step_result()
        .expect("the decoder never produces K_HOOK or K_FALL")
}

fn fetch<B: Bus>(bus: &mut B, pc: u32) -> Result<Op, Trap> {
    let mut head = [0u8; 4];
    let got = {
        let page = bus.fetch_code(pc)?;
        let got = page.bytes.len().min(head.len());
        head[..got].copy_from_slice(&page.bytes[..got]);
        got
    };
    if got < 2 {
        // A mapped code page always holds both bytes of the halfword at a 2-byte-aligned pc, so
        // this is a bus that answered with fewer bytes than it mapped: refuse to execute.
        return Err(Trap::instruction_access_fault(pc));
    }
    if let Some(op) = decode_at(&head[..got], pc) {
        return Ok(op);
    }
    // Only a 32-bit instruction whose high halfword is in the next page gets here. The second
    // fetch starts at `pc + 2`, whether the first page ended after 2 or 3 bytes.
    debug_assert!(got < 4, "decode_at refused {got} bytes");
    debug_assert_eq!(insn_len(u16::from_le_bytes([head[0], head[1]])), 4);
    let next = pc.wrapping_add(2);
    let page = bus.fetch_code(next)?;
    let &[b2, b3, ..] = page.bytes else {
        return Err(Trap::instruction_access_fault(next));
    };
    head[2] = b2;
    head[3] = b3;
    Ok(decode(u32::from_le_bytes(head), pc))
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StepResult {
    Retired,
    /// The trap was already taken.
    Trapped(Trap),
    Wfi,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Access, CodePage, HartView, PageTable};
    use crate::csr::{Csr, CsrEffect, CsrOp};
    use crate::spmon::SpMonitor;
    use crate::trap::{EXC_ILLEGAL_INSN, EXC_INSN_ACCESS_FAULT};

    const CODE_BASE: u32 = 0x4200_0000;
    const PAGE: usize = 4096;
    const CODE_LEN: usize = 2 * PAGE;
    const VECTOR: u32 = 0x4038_0400;

    struct CodeBus {
        code: Vec<u8>,
        /// Relative page numbers whose fetch faults.
        no_exec: Vec<u32>,
        /// Bytes `fetch_code` hands back at most, to model a truncating bus.
        clamp: Option<usize>,
        data: u32,
        fetches: Vec<u32>,
        /// `store_slow` answers `OkStop`, as an ASSIST_DEBUG register write does.
        stop_stores: bool,
        spmon: SpMonitor,
    }

    impl CodeBus {
        fn new(insns: &[u8]) -> CodeBus {
            let mut code = vec![0u8; CODE_LEN];
            code[..insns.len()].copy_from_slice(insns);
            CodeBus {
                code,
                no_exec: Vec::new(),
                clamp: None,
                data: 0,
                fetches: Vec::new(),
                stop_stores: false,
                spmon: SpMonitor::default(),
            }
        }

        fn straddling(word: u32) -> CodeBus {
            let mut bus = CodeBus::new(&[]);
            bus.code[PAGE - 2..PAGE + 2].copy_from_slice(&word.to_le_bytes());
            bus
        }
    }

    impl Bus for CodeBus {
        fn pages(&self) -> &PageTable {
            unreachable!("ref_step is the uncached path and never reads the page table")
        }
        fn arena(&mut self) -> *mut u8 {
            unreachable!("ref_step is the uncached path and never reads the arena")
        }
        fn load_slow(&mut self, _addr: u32, _size: u8, _hart: &HartView) -> Access<u32> {
            Access::Ok(self.data)
        }
        fn store_slow(&mut self, _addr: u32, _size: u8, val: u32, _hart: &HartView) -> Access<()> {
            self.data = val;
            if self.stop_stores {
                Access::OkStop(())
            } else {
                Access::Ok(())
            }
        }
        fn sp_monitor(&self) -> SpMonitor {
            self.spmon
        }
        fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap> {
            self.fetches.push(vaddr);
            let off = vaddr
                .checked_sub(CODE_BASE)
                .filter(|off| (*off as usize) < CODE_LEN)
                .ok_or(Trap::instruction_access_fault(vaddr))? as usize;
            if self.no_exec.contains(&(off as u32 / PAGE as u32)) {
                return Err(Trap::instruction_access_fault(vaddr));
            }
            let end = (off / PAGE + 1) * PAGE;
            let end = match self.clamp {
                Some(max) => end.min(off + max),
                None => end,
            };
            Ok(CodePage {
                bytes: &self.code[off..end],
            })
        }
        fn csr_custom(
            &mut self,
            _csr: u16,
            _op: CsrOp,
            _insns: u64,
        ) -> Result<(u32, CsrEffect), Trap> {
            Ok((0, CsrEffect::None))
        }
        fn wfi_wake(&mut self) -> bool {
            false
        }
        fn pmp_changed(&mut self, _csr: &Csr) {}
    }

    fn hart() -> Hart {
        let mut csr = Csr::new();
        csr.mtvec = VECTOR | 1;
        Hart {
            x: [0; 32],
            pc: CODE_BASE,
            csr,
            wfi: false,
            insns: 0,
            stores: 0,
            spmon: SpMonitor::default(),
            extra: 0,
            pipe: Default::default(),
        }
    }

    /// `addi x1, x0, 5`.
    const ADDI: u32 = 0x0050_0093;
    /// `c.addi x1, 1` (2 bytes).
    const C_ADDI: u16 = 0x0085;
    /// `wfi`.
    const WFI: u32 = 0x1050_0073;
    /// `sw x1, 0(x0)`.
    const SW: u32 = 0x0010_2023;

    #[test]
    fn a_32_bit_instruction_retires_and_advances_the_pc_by_four() {
        let mut bus = CodeBus::new(&ADDI.to_le_bytes());
        let mut h = hart();
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Retired);
        assert_eq!(h.x[1], 5);
        assert_eq!(h.pc, CODE_BASE + 4);
        assert_eq!(h.insns, 1);
        assert_eq!(bus.fetches, vec![CODE_BASE]);
    }

    #[test]
    fn a_compressed_instruction_retires_and_advances_the_pc_by_two() {
        let mut bus = CodeBus::new(&C_ADDI.to_le_bytes());
        let mut h = hart();
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Retired);
        assert_eq!(h.x[1], 1);
        assert_eq!(h.pc, CODE_BASE + 2);
        assert_eq!(bus.fetches, vec![CODE_BASE]);
    }

    #[test]
    fn an_instruction_straddling_a_page_is_fetched_from_both_pages() {
        let mut bus = CodeBus::straddling(ADDI);
        let mut h = hart();
        let pc = CODE_BASE + PAGE as u32 - 2;
        h.pc = pc;
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Retired);
        assert_eq!(h.x[1], 5);
        assert_eq!(h.pc, pc + 4);
        assert_eq!(bus.fetches, vec![pc, pc + 2]);
    }

    #[test]
    fn a_fetch_fault_traps_precisely_at_the_instruction_pc() {
        let mut bus = CodeBus::new(&ADDI.to_le_bytes());
        bus.no_exec.push(0);
        let mut h = hart();
        let trap = Trap::instruction_access_fault(CODE_BASE);
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Trapped(trap));
        assert_eq!(h.csr.mepc, CODE_BASE);
        assert_eq!(h.csr.mcause, EXC_INSN_ACCESS_FAULT);
        assert_eq!(h.csr.mtval, CODE_BASE);
        assert_eq!(h.pc, VECTOR);
        assert_eq!(h.insns, 0);
        assert_eq!(h.x[1], 0);
    }

    #[test]
    fn a_fault_on_the_second_half_records_the_instruction_pc_and_the_faulting_address() {
        let mut bus = CodeBus::straddling(ADDI);
        bus.no_exec.push(1);
        let mut h = hart();
        let pc = CODE_BASE + PAGE as u32 - 2;
        h.pc = pc;
        let trap = Trap::instruction_access_fault(pc + 2);
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Trapped(trap));
        assert_eq!(h.csr.mepc, pc);
        assert_eq!(h.csr.mtval, pc + 2);
        assert_eq!(h.insns, 0);
    }

    #[test]
    fn an_undecodable_instruction_traps_as_illegal_with_its_bits_in_mtval() {
        let mut bus = CodeBus::new(&[0, 0, 0, 0]);
        let mut h = hart();
        let trap = Trap::illegal_instruction(0);
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Trapped(trap));
        assert_eq!(h.csr.mcause, EXC_ILLEGAL_INSN);
        assert_eq!(h.csr.mepc, CODE_BASE);
        assert_eq!(h.insns, 0);
    }

    #[test]
    fn a_page_that_hands_back_less_than_a_halfword_is_an_access_fault() {
        let mut bus = CodeBus::new(&ADDI.to_le_bytes());
        bus.clamp = Some(1);
        let mut h = hart();
        let trap = Trap::instruction_access_fault(CODE_BASE);
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Trapped(trap));
        assert_eq!(h.insns, 0);
    }

    /// A store into an ASSIST_DEBUG bound register answers `OkStop`, and the new bounds apply from
    /// the next instruction.
    #[test]
    fn an_ok_stop_store_refreshes_the_stack_monitor() {
        let mut bus = CodeBus::new(&SW.to_le_bytes());
        bus.stop_stores = true;
        bus.spmon = SpMonitor {
            on_min: true,
            on_max: false,
            min: 0x3fc8_0000,
            max: 0,
        };
        let mut h = hart();
        h.x[1] = 0xabcd_1234;
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Retired);
        assert_eq!(bus.data, 0xabcd_1234);
        assert_eq!(h.pc, CODE_BASE + 4);
        assert_eq!(h.insns, 1);
        assert_eq!(h.stores, 1);
        assert!(h.spmon.on_min);
        assert!(!h.spmon.on_max);
        assert_eq!(h.spmon.min, 0x3fc8_0000);
    }

    #[test]
    fn a_plain_store_leaves_the_stack_monitor_alone() {
        let mut bus = CodeBus::new(&SW.to_le_bytes());
        bus.spmon = SpMonitor {
            on_min: true,
            on_max: true,
            min: 1,
            max: 2,
        };
        let mut h = hart();
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Retired);
        assert!(!h.spmon.on_min);
        assert!(!h.spmon.on_max);
        assert_eq!(h.spmon.min, 0);
    }

    #[test]
    fn wfi_retires_and_reports_a_wait() {
        let mut bus = CodeBus::new(&WFI.to_le_bytes());
        let mut h = hart();
        assert_eq!(ref_step(&mut h, &mut bus), StepResult::Wfi);
        assert!(h.wfi);
        assert_eq!(h.pc, CODE_BASE + 4);
        assert_eq!(h.insns, 1);
    }
}
