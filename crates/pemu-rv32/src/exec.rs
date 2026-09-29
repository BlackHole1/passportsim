//! Instruction semantics and the hart state they update (RISC-V unprivileged specification,
//! ESP32-C3 TRM chapter 1). [`exec_op`] runs one [`Op`] against a [`Bus`] for `ref_step` and for
//! every op the block engine does not run itself.
//!
//! Faults are precise traps; misaligned data accesses never trap on this hart (the bus splits
//! them). Interrupts are taken only between runs, so an instruction that can make one deliverable
//! (a Zicsr write to `mstatus`, `mret`) reports [`Stepped::RetiredStop`] to keep delivery
//! independent of block size.

use crate::bus::{Access, Bus, HartView};
use crate::cost::Pipe;
use crate::csr::{CSR_MSTATUS, Csr, CsrCx, CsrOp};
use crate::op::{
    self, F_STORE, F_WRITES_SP, K_ADD, K_ADDI, K_AND, K_ANDI, K_AUIPC, K_BEQ, K_BGE, K_BGEU, K_BLT,
    K_BLTU, K_BNE, K_CSRRC, K_CSRRCI, K_CSRRS, K_CSRRSI, K_CSRRW, K_CSRRWI, K_DIV, K_DIVU,
    K_EBREAK, K_ECALL, K_FALL, K_FENCEI, K_FETCH_FAULT, K_HOOK, K_ILLEGAL, K_JAL, K_JALR, K_LB,
    K_LBU, K_LH, K_LHU, K_LUI, K_LW, K_MRET, K_MUL, K_MULH, K_MULHSU, K_MULHU, K_NOP, K_OR, K_ORI,
    K_REM, K_REMU, K_SB, K_SH, K_SLL, K_SLLI, K_SLT, K_SLTI, K_SLTIU, K_SLTU, K_SRA, K_SRAI, K_SRL,
    K_SRLI, K_SUB, K_SW, K_WFI, K_XOR, K_XORI, Op,
};
use crate::refstep::StepResult;
use crate::spmon::{SpMonitor, SpSpill};
use crate::trap::{Trap, take_exception};

/// Architectural state of the single RV32IMC hart.
pub struct Hart {
    pub x: [u32; 32],
    pub pc: u32,
    pub csr: Csr,
    pub wfi: bool,
    /// Retired instructions, plus instructions credited by fast-forward.
    pub insns: u64,
    /// Used by the poll tracker.
    pub stores: u64,
    /// ASSIST_DEBUG stack monitor mirror, refreshed from `Bus::sp_monitor()` after every `OkStop`.
    pub spmon: SpMonitor,
    /// Cycles `crate::cost` charged beyond one per retired instruction.
    pub extra: u64,
    /// Cleared by trap and interrupt entry.
    pub pipe: Pipe,
}

impl Hart {
    /// The clock position, `insns + extra`.
    #[inline]
    pub fn pos(&self) -> u64 {
        self.insns.wrapping_add(self.extra)
    }

    #[inline]
    pub fn view(&self, pc: u32) -> HartView {
        HartView {
            insns: self.insns,
            extra: self.extra,
            pc,
        }
    }
}

/// What one [`exec_op`] did beyond the hart state it already updated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stepped {
    Retired,
    /// Retired after a bus `OkStop` or with an interrupt possibly deliverable: the caller
    /// refreshes `Hart::spmon` and leaves the block.
    RetiredStop,
    /// Retired with an `x2` write that violated the stack monitor; subsumes a bus `OkStop`.
    Spilled(SpSpill),
    /// Nothing is committed and `Hart::pc` is the vector.
    Trapped(Trap),
    /// `wfi` retired with `Hart::wfi` set; the run loop wakes the hart and clears it.
    Waiting,
    Flushed,
    /// [`op::K_HOOK`]: nothing executed; the payload is the `HookId` value.
    Hooked(u32),
    Fell,
}

impl Stepped {
    /// `None` for the synthetic kinds, which `ref_step` never builds.
    pub fn step_result(self) -> Option<StepResult> {
        match self {
            Stepped::Retired | Stepped::RetiredStop | Stepped::Spilled(_) | Stepped::Flushed => {
                Some(StepResult::Retired)
            }
            Stepped::Trapped(trap) => Some(StepResult::Trapped(trap)),
            Stepped::Waiting => Some(StepResult::Wfi),
            Stepped::Hooked(_) | Stepped::Fell => None,
        }
    }

    /// The caller must leave the block after this op although it carries no [`op::F_TERM`].
    pub fn stops_block(self) -> bool {
        matches!(
            self,
            Stepped::RetiredStop | Stepped::Spilled(_) | Stepped::Trapped(_)
        )
    }

    pub fn refreshes_spmon(self) -> bool {
        matches!(self, Stepped::RetiredStop | Stepped::Spilled(_))
    }
}

/// Executes `op` of the block at `block_pc`; on every outcome `Hart::pc` is where execution
/// continues.
pub fn exec_op<B: Bus>(
    hart: &mut Hart,
    bus: &mut B,
    block_pc: u32,
    op: &Op,
    strict_csr: bool,
) -> Stepped {
    let pc = block_pc.wrapping_add(u32::from(op.pc_off));
    let next = pc.wrapping_add(u32::from(op.len));
    let a = hart.x[(op.rs1 & 31) as usize];
    let b = hart.x[(op.rs2 & 31) as usize];

    if let Some(value) = alu(op, a, b) {
        write_rd(hart, op.rd, value);
        return retired(hart, op, next, false);
    }

    match op.kind {
        K_NOP => retired(hart, op, next, false),
        K_LB | K_LH | K_LW | K_LBU | K_LHU => load(hart, bus, op, pc, next, a),
        K_SB | K_SH | K_SW => store(hart, bus, op, pc, next, a, b),
        K_BEQ | K_BNE | K_BLT | K_BGE | K_BLTU | K_BGEU => {
            let target = if branch_taken(op.kind, a, b) {
                op.imm as u32
            } else {
                op.imm2
            };
            retired(hart, op, target, false)
        }
        K_JAL => {
            write_rd(hart, op.rd, op.imm2);
            retired(hart, op, op.imm as u32, false)
        }
        K_JALR => {
            // Read the target before the link write, since rd may be rs1.
            let target = a.wrapping_add(op.imm as u32) & !1;
            write_rd(hart, op.rd, op.imm2);
            retired(hart, op, target, false)
        }
        k if op::is_csr(k) => csr(hart, bus, op, pc, next, a, strict_csr),
        K_ECALL => trapped(hart, pc, Trap::ecall_from_machine()),
        K_EBREAK => trapped(hart, pc, Trap::breakpoint(pc)),
        K_ILLEGAL => trapped(hart, pc, Trap::illegal_instruction(op.imm2)),
        K_FETCH_FAULT => trapped(
            hart,
            pc,
            Trap {
                cause: op.imm as u32,
                tval: op.imm2,
            },
        ),
        K_MRET => {
            crate::trap::mret(hart);
            hart.insns += 1;
            Stepped::RetiredStop
        }
        K_WFI => {
            hart.wfi = true;
            hart.pc = op.imm2;
            hart.insns += 1;
            Stepped::Waiting
        }
        K_FENCEI => {
            hart.pc = op.imm2;
            hart.insns += 1;
            Stepped::Flushed
        }
        K_HOOK => {
            hart.pc = pc;
            Stepped::Hooked(op.imm2)
        }
        K_FALL => {
            hart.pc = op.imm2;
            Stepped::Fell
        }
        // Unreachable for a decoded op; a release build traps instead of panicking.
        _ => {
            debug_assert!(false, "op kind {} is not a kind", op.kind);
            trapped(hart, pc, Trap::illegal_instruction(op.imm2))
        }
    }
}

/// The value a computational kind writes to `rd`, or `None` for any other kind. M never traps;
/// the division edge cases follow the RISC-V unprivileged specification, "Division Operations".
fn alu(op: &Op, a: u32, b: u32) -> Option<u32> {
    let imm = op.imm as u32;
    let shamt = imm & 31;
    let value = match op.kind {
        K_LUI | K_AUIPC => imm,
        K_ADDI => a.wrapping_add(imm),
        K_SLTI => u32::from((a as i32) < op.imm),
        K_SLTIU => u32::from(a < imm),
        K_XORI => a ^ imm,
        K_ORI => a | imm,
        K_ANDI => a & imm,
        K_SLLI => a << shamt,
        K_SRLI => a >> shamt,
        K_SRAI => ((a as i32) >> shamt) as u32,
        K_ADD => a.wrapping_add(b),
        K_SUB => a.wrapping_sub(b),
        K_SLL => a << (b & 31),
        K_SLT => u32::from((a as i32) < (b as i32)),
        K_SLTU => u32::from(a < b),
        K_XOR => a ^ b,
        K_SRL => a >> (b & 31),
        K_SRA => ((a as i32) >> (b & 31)) as u32,
        K_OR => a | b,
        K_AND => a & b,
        K_MUL => a.wrapping_mul(b),
        K_MULH => ((i64::from(a as i32) * i64::from(b as i32)) >> 32) as u32,
        K_MULHSU => ((i64::from(a as i32) * i64::from(b)) >> 32) as u32,
        K_MULHU => ((u64::from(a) * u64::from(b)) >> 32) as u32,
        K_DIV => match (a as i32, b as i32) {
            (_, 0) => u32::MAX,
            (i32::MIN, -1) => i32::MIN as u32,
            (x, y) => (x / y) as u32,
        },
        K_DIVU => match b {
            0 => u32::MAX,
            y => a / y,
        },
        K_REM => match (a as i32, b as i32) {
            (x, 0) => x as u32,
            (i32::MIN, -1) => 0,
            (x, y) => (x % y) as u32,
        },
        K_REMU => match b {
            0 => a,
            y => a % y,
        },
        _ => return None,
    };
    Some(value)
}

fn branch_taken(kind: u8, a: u32, b: u32) -> bool {
    match kind {
        K_BEQ => a == b,
        K_BNE => a != b,
        K_BLT => (a as i32) < (b as i32),
        K_BGE => (a as i32) >= (b as i32),
        K_BLTU => a < b,
        _ => a >= b,
    }
}

fn write_rd(hart: &mut Hart, rd: u8, value: u32) {
    let r = (rd & 31) as usize;
    if r != 0 {
        hart.x[r] = value;
    }
}

/// `stop` is a bus `Access::OkStop` on this instruction.
fn retired(hart: &mut Hart, op: &Op, next_pc: u32, stop: bool) -> Stepped {
    hart.pc = next_pc;
    hart.insns += 1;
    if op.flags & F_WRITES_SP != 0
        && hart.spmon.armed()
        && let Some(spill) = hart.spmon.check(hart.x[2])
    {
        return Stepped::Spilled(spill);
    }
    if stop {
        Stepped::RetiredStop
    } else {
        Stepped::Retired
    }
}

fn trapped(hart: &mut Hart, pc: u32, trap: Trap) -> Stepped {
    take_exception(hart, pc, trap);
    Stepped::Trapped(trap)
}

fn load<B: Bus>(hart: &mut Hart, bus: &mut B, op: &Op, pc: u32, next: u32, a: u32) -> Stepped {
    let addr = a.wrapping_add(op.imm as u32);
    let view = hart.view(pc);
    let (raw, stop) = match bus.load_slow(addr, load_size(op.kind), &view) {
        Access::Ok(raw) => (raw, false),
        Access::OkStop(raw) => (raw, true),
        Access::Fault(trap) => return trapped(hart, pc, trap),
    };
    write_rd(hart, op.rd, extend(op.kind, raw));
    retired(hart, op, next, stop)
}

fn store<B: Bus>(
    hart: &mut Hart,
    bus: &mut B,
    op: &Op,
    pc: u32,
    next: u32,
    a: u32,
    b: u32,
) -> Stepped {
    debug_assert_ne!(op.flags & F_STORE, 0, "store kind without F_STORE");
    let addr = a.wrapping_add(op.imm as u32);
    let view = hart.view(pc);
    let stop = match bus.store_slow(addr, store_size(op.kind), b, &view) {
        Access::Ok(()) => false,
        Access::OkStop(()) => true,
        Access::Fault(trap) => return trapped(hart, pc, trap),
    };
    hart.stores += 1;
    retired(hart, op, next, stop)
}

fn load_size(kind: u8) -> u8 {
    match kind {
        K_LB | K_LBU => 1,
        K_LH | K_LHU => 2,
        _ => 4,
    }
}

fn store_size(kind: u8) -> u8 {
    match kind {
        K_SB => 1,
        K_SH => 2,
        _ => 4,
    }
}

/// Masks the high bits of `raw`, so a bus returning a wider value cannot leak into the result.
fn extend(kind: u8, raw: u32) -> u32 {
    match kind {
        K_LB => raw as u8 as i8 as i32 as u32,
        K_LBU => raw & 0xFF,
        K_LH => raw as u16 as i16 as i32 as u32,
        K_LHU => raw & 0xFFFF,
        _ => raw,
    }
}

/// `csrrs` and `csrrc` forms with a zero source become [`CsrOp::Read`].
fn csr<B: Bus>(
    hart: &mut Hart,
    bus: &mut B,
    op: &Op,
    pc: u32,
    next: u32,
    a: u32,
    strict: bool,
) -> Stepped {
    let immediate_form = op::is_csr_imm(op.kind);
    let source = if immediate_form { op.imm as u32 } else { a };
    let writes_zero_source = if immediate_form {
        op.imm == 0
    } else {
        op.rs1 & 31 == 0
    };
    let csr_op = match op.kind {
        K_CSRRW | K_CSRRWI => CsrOp::Write(source),
        K_CSRRS | K_CSRRSI if !writes_zero_source => CsrOp::Set(source),
        K_CSRRC | K_CSRRCI if !writes_zero_source => CsrOp::Clear(source),
        _ => CsrOp::Read,
    };
    let cx = CsrCx {
        insns: hart.pos(),
        insn: csr_insn_bits(op),
        strict,
    };
    match hart.csr.access(bus, cx, (op.imm2 & 0xFFF) as u16, csr_op) {
        Ok(value) => {
            write_rd(hart, op.rd, value);
            // A write to `mstatus` can set MIE, so it ends the run.
            let ends_run = csr_op.writes() && (op.imm2 & 0xFFF) as u16 == CSR_MSTATUS;
            retired(hart, op, next, ends_run)
        }
        Err(trap) => trapped(hart, pc, trap),
    }
}

/// The Zicsr instruction bits for `mtval`, rebuilt from the op.
fn csr_insn_bits(op: &Op) -> u32 {
    let funct3: u32 = match op.kind {
        K_CSRRW => 1,
        K_CSRRS => 2,
        K_CSRRC => 3,
        K_CSRRWI => 5,
        K_CSRRSI => 6,
        _ => 7,
    };
    let rs1_field = if op::is_csr_imm(op.kind) {
        op.imm as u32 & 31
    } else {
        u32::from(op.rs1 & 31)
    };
    ((op.imm2 & 0xFFF) << 20)
        | (rs1_field << 15)
        | (funct3 << 12)
        | (u32::from(op.rd & 31) << 7)
        | 0x73
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{CodePage, PageTable};
    use crate::csr::{
        CSR_MSCRATCH, CSR_MSTATUS, CSR_MVENDORID, CsrEffect, MSTATUS_MIE, MSTATUS_MPIE,
    };
    use crate::op::{
        K_COUNT, K_CSRRC, K_CSRRCI, K_CSRRS, K_CSRRSI, K_CSRRW, K_CSRRWI, KIND_NAMES, flags_for,
        is_synthetic, kind_name,
    };
    use crate::trap::{
        EXC_BREAKPOINT, EXC_ECALL_M, EXC_ILLEGAL_INSN, EXC_INSN_ACCESS_FAULT,
        EXC_LOAD_ACCESS_FAULT, EXC_STORE_ACCESS_FAULT,
    };

    /// Block start PC of every test op; ops carry `pc_off` 0 unless a test sets it.
    const PC: u32 = 0x4200_0100;
    const VECTOR: u32 = 0x4038_0000;
    /// Base of the test RAM (SRAM1 data view).
    const RAM_BASE: u32 = 0x3FC8_0000;
    const RAM_LEN: usize = 64;
    /// ASSIST_DEBUG CORE_0_SP_MIN: the test bus answers `OkStop` here, as the SoC does.
    const STOP_ADDR: u32 = 0x600C_E038;
    const BAD_ADDR: u32 = 0x0000_0010;

    /// RAM at `RAM_BASE`, one `OkStop` address, a fault everywhere else, and a recording.
    struct RamBus {
        ram: [u8; RAM_LEN],
        spmon: SpMonitor,
        /// Address, size, `HartView::insns` and `HartView::pc` of the last data access.
        last: Option<(u32, u8, u64, u32)>,
        /// Every `csr_custom` call: number, op and instruction count.
        csr_calls: Vec<(u16, CsrOp, u64)>,
        /// What `csr_custom` returns, unless `csr_trap` is set.
        csr_value: u32,
        csr_trap: Option<Trap>,
    }

    impl RamBus {
        fn new() -> RamBus {
            let mut ram = [0u8; RAM_LEN];
            for (i, byte) in ram.iter_mut().enumerate() {
                *byte = 0x80 | i as u8;
            }
            RamBus {
                ram,
                spmon: SpMonitor::default(),
                last: None,
                csr_calls: Vec::new(),
                csr_value: 0,
                csr_trap: None,
            }
        }

        fn offset(&self, addr: u32, size: u8) -> Option<usize> {
            let off = addr.checked_sub(RAM_BASE)? as usize;
            (off + usize::from(size) <= RAM_LEN).then_some(off)
        }

        fn word(&self, addr: u32) -> u32 {
            let off = self.offset(addr, 4).expect("address is in the test RAM");
            u32::from_le_bytes(self.ram[off..off + 4].try_into().unwrap())
        }
    }

    impl Bus for RamBus {
        fn pages(&self) -> &PageTable {
            unreachable!("exec_op is the uncached path and never reads the page table")
        }
        fn arena(&mut self) -> *mut u8 {
            unreachable!("exec_op is the uncached path and never reads the arena")
        }
        fn load_slow(&mut self, addr: u32, size: u8, hart: &HartView) -> Access<u32> {
            self.last = Some((addr, size, hart.insns, hart.pc));
            if addr == STOP_ADDR {
                return Access::OkStop(0x5A5A_5A5A);
            }
            match self.offset(addr, size) {
                Some(off) => {
                    let mut value = 0u32;
                    for i in (0..usize::from(size)).rev() {
                        value = (value << 8) | u32::from(self.ram[off + i]);
                    }
                    Access::Ok(value)
                }
                None => Access::Fault(Trap::load_access_fault(addr)),
            }
        }
        fn store_slow(&mut self, addr: u32, size: u8, val: u32, hart: &HartView) -> Access<()> {
            self.last = Some((addr, size, hart.insns, hart.pc));
            if addr == STOP_ADDR {
                return Access::OkStop(());
            }
            match self.offset(addr, size) {
                Some(off) => {
                    for i in 0..usize::from(size) {
                        self.ram[off + i] = (val >> (8 * i)) as u8;
                    }
                    Access::Ok(())
                }
                None => Access::Fault(Trap::store_access_fault(addr)),
            }
        }
        fn sp_monitor(&self) -> SpMonitor {
            self.spmon
        }
        fn fetch_code(&mut self, _vaddr: u32) -> Result<CodePage<'_>, Trap> {
            unreachable!("exec_op never fetches; ref_step does")
        }
        fn csr_custom(
            &mut self,
            csr: u16,
            op: CsrOp,
            insns: u64,
        ) -> Result<(u32, CsrEffect), Trap> {
            self.csr_calls.push((csr, op, insns));
            match self.csr_trap {
                Some(trap) => Err(trap),
                None => Ok((self.csr_value, CsrEffect::None)),
            }
        }
        fn wfi_wake(&mut self) -> bool {
            false
        }
        fn pmp_changed(&mut self, _csr: &Csr) {}
    }

    /// Nonzero counters and enabled interrupts, so a test sees which fields an instruction moved.
    fn hart() -> Hart {
        let mut csr = Csr::new();
        csr.mtvec = VECTOR | 1;
        csr.mstatus = MSTATUS_MIE;
        Hart {
            x: [0; 32],
            pc: PC,
            csr,
            wfi: false,
            insns: 40,
            stores: 7,
            spmon: SpMonitor::default(),
            extra: 0,
            pipe: Default::default(),
        }
    }

    fn op(kind: u8, rd: u8, rs1: u8, rs2: u8, imm: i32, imm2: u32) -> Op {
        Op {
            kind,
            rd,
            rs1,
            rs2,
            imm,
            imm2,
            len: 4,
            flags: flags_for(kind, rd),
            pc_off: 0,
        }
    }

    fn run(hart: &mut Hart, bus: &mut RamBus, o: &Op) -> Stepped {
        exec_op(hart, bus, PC, o, false)
    }

    /// Runs `kind` with `x[1] = a`, `x[3] = b` into `x[5]` and returns the value written.
    fn alu_value(kind: u8, a: u32, b: u32, imm: i32) -> u32 {
        let name = kind_name(kind);
        let mut h = hart();
        let mut bus = RamBus::new();
        h.x[1] = a;
        h.x[3] = b;
        let o = op(kind, 5, 1, 3, imm, 0);
        assert_eq!(run(&mut h, &mut bus, &o), Stepped::Retired, "{name}");
        assert_eq!(h.pc, PC + 4, "{name}: next pc");
        assert_eq!(h.insns, 41, "{name}: retired");
        assert_eq!(h.stores, 7, "{name}: no store");
        assert_eq!(h.x[0], 0, "{name}: x0");
        h.x[5]
    }

    #[test]
    fn nop_retires_and_advances_past_the_instruction() {
        let mut h = hart();
        let mut bus = RamBus::new();
        assert_eq!(
            run(&mut h, &mut bus, &op(K_NOP, 0, 0, 0, 0, 0)),
            Stepped::Retired
        );
        assert_eq!((h.pc, h.insns, h.stores), (PC + 4, 41, 7));
        assert_eq!(h.x, [0; 32]);
        let mut o = op(K_NOP, 0, 0, 0, 0, 0);
        o.len = 2;
        assert_eq!(run(&mut h, &mut bus, &o), Stepped::Retired);
        assert_eq!(h.pc, PC + 2);
    }

    #[test]
    fn computational_kinds_compute_their_values() {
        // kind, x[rs1], x[rs2], imm, expected x[rd].
        let table: &[(u8, u32, u32, i32, u32)] = &[
            (K_LUI, 0, 0, 0x1234_5000, 0x1234_5000),
            (K_AUIPC, 0, 0, 0x4200_1100, 0x4200_1100),
            (K_ADDI, 10, 0, -3, 7),
            (K_SLTI, -5i32 as u32, 0, -4, 1),
            (K_SLTI, -5i32 as u32, 0, -6, 0),
            (K_SLTIU, 1, 0, -1, 1),
            (K_XORI, 0xF0F0_F0F0, 0, 0xFF, 0xF0F0_F00F),
            (K_ORI, 0xF0, 0, 0x0F, 0xFF),
            (K_ANDI, 0xFF, 0, -16, 0xF0),
            (K_SLLI, 1, 0, 31, 0x8000_0000),
            (K_SRLI, 0x8000_0000, 0, 31, 1),
            (K_SRAI, 0x8000_0000, 0, 31, 0xFFFF_FFFF),
            (K_ADD, 0xFFFF_FFFF, 2, 0, 1),
            (K_SUB, 1, 2, 0, 0xFFFF_FFFF),
            (K_SLL, 1, 33, 0, 2),
            (K_SLT, -1i32 as u32, 0, 0, 1),
            (K_SLTU, 0xFFFF_FFFF, 0, 0, 0),
            (K_XOR, 0xAAAA_AAAA, 0xFFFF_FFFF, 0, 0x5555_5555),
            (K_SRL, 0x8000_0000, 33, 0, 0x4000_0000),
            (K_SRA, 0x8000_0000, 33, 0, 0xC000_0000),
            (K_OR, 0xF0, 0x0F, 0, 0xFF),
            (K_AND, 0xFF, 0x0F, 0, 0x0F),
            (K_MUL, 0x0001_0001, 0x0001_0001, 0, 0x0002_0001),
            (K_MULH, 0x8000_0000, 2, 0, 0xFFFF_FFFF),
            (K_MULHSU, 0xFFFF_FFFF, 2, 0, 0xFFFF_FFFF),
            (K_MULHU, 0xFFFF_FFFF, 0xFFFF_FFFF, 0, 0xFFFF_FFFE),
            (K_DIV, -7i32 as u32, 2, 0, -3i32 as u32),
            (K_DIVU, 7, 2, 0, 3),
            (K_REM, -7i32 as u32, 2, 0, -1i32 as u32),
            (K_REMU, 7, 2, 0, 1),
        ];
        for &(kind, a, b, imm, want) in table {
            assert_eq!(
                alu_value(kind, a, b, imm),
                want,
                "{} a {a:#x} b {b:#x} imm {imm}",
                kind_name(kind)
            );
        }
    }

    #[test]
    fn shifts_use_only_the_low_five_bits_of_the_amount() {
        for extra in [0u32, 32, 64, 0xFFFF_FFE0] {
            assert_eq!(alu_value(K_SLL, 0x1234_5678, extra, 0), 0x1234_5678);
            assert_eq!(alu_value(K_SRL, 0x1234_5678, extra, 0), 0x1234_5678);
            assert_eq!(alu_value(K_SRA, 0x8765_4321, extra, 0), 0x8765_4321);
            assert_eq!(alu_value(K_SLL, 0x1234_5678, extra | 4, 0), 0x2345_6780);
            assert_eq!(alu_value(K_SRL, 0x1234_5678, extra | 4, 0), 0x0123_4567);
            assert_eq!(alu_value(K_SRA, 0x8765_4321, extra | 4, 0), 0xF876_5432);
        }
        assert_eq!(alu_value(K_SLLI, 0x1234_5678, 0, 0), 0x1234_5678);
        assert_eq!(alu_value(K_SRAI, 0x8000_0000, 0, 1), 0xC000_0000);
        assert_eq!(alu_value(K_SRLI, 0x8000_0000, 0, 1), 0x4000_0000);
    }

    #[test]
    fn m_extension_division_edge_cases() {
        const MIN: u32 = 0x8000_0000;
        assert_eq!(alu_value(K_DIV, 7, 0, 0), 0xFFFF_FFFF);
        assert_eq!(alu_value(K_DIV, -7i32 as u32, 0, 0), 0xFFFF_FFFF);
        assert_eq!(alu_value(K_DIV, 0, 0, 0), 0xFFFF_FFFF);
        assert_eq!(alu_value(K_DIVU, 7, 0, 0), 0xFFFF_FFFF);
        assert_eq!(alu_value(K_DIVU, 0, 0, 0), 0xFFFF_FFFF);
        assert_eq!(alu_value(K_REM, 7, 0, 0), 7);
        assert_eq!(alu_value(K_REM, -7i32 as u32, 0, 0), -7i32 as u32);
        assert_eq!(alu_value(K_REMU, 7, 0, 0), 7);
        assert_eq!(alu_value(K_DIV, MIN, -1i32 as u32, 0), MIN);
        assert_eq!(alu_value(K_REM, MIN, -1i32 as u32, 0), 0);
        assert_eq!(alu_value(K_DIVU, MIN, -1i32 as u32, 0), 0);
        assert_eq!(alu_value(K_REMU, MIN, -1i32 as u32, 0), MIN);
        // Signed and unsigned high halves differ on the same bit patterns.
        assert_eq!(alu_value(K_MULH, 0xFFFF_FFFF, 0xFFFF_FFFF, 0), 0);
        assert_eq!(alu_value(K_MULHU, 0xFFFF_FFFF, 2, 0), 1);
        assert_eq!(
            alu_value(K_MULHSU, 0xFFFF_FFFF, 0xFFFF_FFFF, 0),
            0xFFFF_FFFF
        );
        assert_eq!(alu_value(K_MULHSU, MIN, 0xFFFF_FFFF, 0), MIN);
        assert_eq!(alu_value(K_MULHU, MIN, 0xFFFF_FFFF, 0), 0x7FFF_FFFF);
    }

    #[test]
    fn loads_sign_and_zero_extend_the_addressed_bytes() {
        // The test RAM holds 0x80 | offset, so every byte and half has its high bit set.
        let mut bus = RamBus::new();
        let base = RAM_BASE;
        for (kind, want) in [
            (K_LB, 0xFFFF_FF88),
            (K_LBU, 0x0000_0088),
            (K_LH, 0xFFFF_8988),
            (K_LHU, 0x0000_8988),
            (K_LW, 0x8B8A_8988),
        ] {
            let mut h = hart();
            h.x[1] = base;
            let o = op(kind, 5, 1, 0, 8, 0);
            assert_eq!(
                run(&mut h, &mut bus, &o),
                Stepped::Retired,
                "{}",
                kind_name(kind)
            );
            assert_eq!(h.x[5], want, "{}", kind_name(kind));
            assert_eq!((h.pc, h.insns, h.stores), (PC + 4, 41, 7));
            let size = match kind {
                K_LB | K_LBU => 1,
                K_LH | K_LHU => 2,
                _ => 4,
            };
            assert_eq!(bus.last, Some((RAM_BASE + 8, size, 40, PC)));
        }
    }

    #[test]
    fn loads_add_a_negative_offset_and_leave_x0_alone() {
        let mut h = hart();
        let mut bus = RamBus::new();
        h.x[1] = RAM_BASE + 16;
        assert_eq!(
            run(&mut h, &mut bus, &op(K_LW, 0, 1, 0, -16, 0)),
            Stepped::Retired
        );
        assert_eq!(bus.last, Some((RAM_BASE, 4, 40, PC)));
        assert_eq!(h.x[0], 0);
        assert_eq!(h.insns, 41);
    }

    #[test]
    fn stores_write_only_their_bytes_and_count_in_hart_stores() {
        for (kind, want) in [
            (K_SB, 0x8B8A_8944),
            (K_SH, 0x8B8A_4344),
            (K_SW, 0x4142_4344),
        ] {
            let mut h = hart();
            let mut bus = RamBus::new();
            h.x[1] = RAM_BASE;
            h.x[3] = 0x4142_4344;
            let o = op(kind, 0, 1, 3, 8, 0);
            assert_eq!(
                run(&mut h, &mut bus, &o),
                Stepped::Retired,
                "{}",
                kind_name(kind)
            );
            assert_eq!(bus.word(RAM_BASE + 8), want, "{}", kind_name(kind));
            assert_eq!(
                (h.pc, h.insns, h.stores),
                (PC + 4, 41, 8),
                "{}",
                kind_name(kind)
            );
            assert_eq!(h.x[3], 0x4142_4344, "{}: value register", kind_name(kind));
        }
    }

    #[test]
    fn a_faulting_load_leaves_precise_trap_state() {
        let mut h = hart();
        let mut bus = RamBus::new();
        h.x[1] = BAD_ADDR;
        h.x[5] = 0xDEAD_BEEF;
        let mut o = op(K_LW, 5, 1, 0, 0, 0);
        o.pc_off = 8;
        let out = run(&mut h, &mut bus, &o);
        assert_eq!(out, Stepped::Trapped(Trap::load_access_fault(BAD_ADDR)));
        assert_eq!(
            out.step_result(),
            Some(StepResult::Trapped(Trap::load_access_fault(BAD_ADDR)))
        );
        assert_eq!(h.x[5], 0xDEAD_BEEF, "the destination is untouched");
        assert_eq!(h.insns, 40, "a trapping instruction does not retire");
        assert_eq!(h.stores, 7);
        assert_eq!(h.csr.mepc, PC + 8, "mepc is the op pc, not the block pc");
        assert_eq!(h.csr.mcause, EXC_LOAD_ACCESS_FAULT);
        assert_eq!(h.csr.mtval, BAD_ADDR);
        assert_eq!(h.pc, VECTOR, "exceptions enter at vector slot 0");
        assert!(!h.csr.mstatus_mie(), "trap entry clears MIE");
        assert_ne!(h.csr.mstatus & MSTATUS_MPIE, 0, "MPIE took the old MIE");
    }

    #[test]
    fn a_faulting_store_neither_counts_nor_writes() {
        let mut h = hart();
        let mut bus = RamBus::new();
        h.x[1] = BAD_ADDR;
        h.x[3] = 0x1234_5678;
        let out = run(&mut h, &mut bus, &op(K_SW, 0, 1, 3, 4, 0));
        assert_eq!(
            out,
            Stepped::Trapped(Trap::store_access_fault(BAD_ADDR + 4))
        );
        assert_eq!(h.insns, 40);
        assert_eq!(h.stores, 7, "a rejected store does not count");
        assert_eq!(h.csr.mepc, PC);
        assert_eq!(h.csr.mcause, EXC_STORE_ACCESS_FAULT);
        assert_eq!(h.csr.mtval, BAD_ADDR + 4);
        assert_eq!(h.pc, VECTOR);
    }

    #[test]
    fn okstop_from_the_bus_retires_and_stops_the_block() {
        let mut h = hart();
        let mut bus = RamBus::new();
        h.x[1] = STOP_ADDR;
        let out = run(&mut h, &mut bus, &op(K_LW, 5, 1, 0, 0, 0));
        assert_eq!(out, Stepped::RetiredStop);
        assert!(out.stops_block());
        assert_eq!(out.step_result(), Some(StepResult::Retired));
        assert_eq!(h.x[5], 0x5A5A_5A5A);
        assert_eq!((h.pc, h.insns), (PC + 4, 41));

        let mut h = hart();
        h.x[1] = STOP_ADDR;
        let out = run(&mut h, &mut bus, &op(K_SW, 0, 1, 3, 0, 0));
        assert_eq!(out, Stepped::RetiredStop);
        assert_eq!((h.insns, h.stores), (41, 8), "a stopped store still counts");
    }

    const TAKEN: u32 = 0x4200_0200;
    const FALL: u32 = PC + 4;

    fn branch_pc(kind: u8, a: u32, b: u32) -> u32 {
        let mut h = hart();
        let mut bus = RamBus::new();
        h.x[1] = a;
        h.x[3] = b;
        let o = op(kind, 0, 1, 3, TAKEN as i32, FALL);
        assert_eq!(
            run(&mut h, &mut bus, &o),
            Stepped::Retired,
            "{}",
            kind_name(kind)
        );
        assert_eq!(h.insns, 41, "{}", kind_name(kind));
        h.pc
    }

    #[test]
    fn branches_pick_the_taken_target_or_the_fall_through() {
        // Signed and unsigned comparisons disagree on these operands.
        let neg = -1i32 as u32;
        for (kind, a, b, taken) in [
            (K_BEQ, 5, 5, true),
            (K_BEQ, 5, 6, false),
            (K_BNE, 5, 6, true),
            (K_BNE, 5, 5, false),
            (K_BLT, neg, 0, true),
            (K_BLT, 0, neg, false),
            (K_BLT, 5, 5, false),
            (K_BGE, 0, neg, true),
            (K_BGE, 5, 5, true),
            (K_BGE, neg, 0, false),
            (K_BLTU, 0, neg, true),
            (K_BLTU, neg, 0, false),
            (K_BGEU, neg, 0, true),
            (K_BGEU, 5, 5, true),
            (K_BGEU, 0, neg, false),
        ] {
            let want = if taken { TAKEN } else { FALL };
            assert_eq!(
                branch_pc(kind, a, b),
                want,
                "{} {a:#x} {b:#x}",
                kind_name(kind)
            );
        }
    }

    #[test]
    fn a_two_byte_aligned_branch_target_does_not_trap() {
        let mut h = hart();
        let mut bus = RamBus::new();
        let target = 0x4200_0202;
        let mut o = op(K_BEQ, 0, 1, 0, target, PC + 2);
        o.len = 2;
        assert_eq!(run(&mut h, &mut bus, &o), Stepped::Retired);
        assert_eq!(h.pc, target as u32);
        assert_eq!(h.csr.mcause, 0, "no trap was taken");
    }

    #[test]
    fn jal_writes_the_link_value_and_jumps() {
        let mut h = hart();
        let mut bus = RamBus::new();
        let out = run(&mut h, &mut bus, &op(K_JAL, 1, 0, 0, TAKEN as i32, PC + 4));
        assert_eq!(out, Stepped::Retired);
        assert_eq!(h.x[1], PC + 4, "ra takes the op pc plus len");
        assert_eq!((h.pc, h.insns), (TAKEN, 41));

        let mut h = hart();
        let mut o = op(K_JAL, 0, 0, 0, TAKEN as i32, PC + 2);
        o.len = 2;
        assert_eq!(run(&mut h, &mut bus, &o), Stepped::Retired);
        assert_eq!(h.x[0], 0);
        assert_eq!(h.pc, TAKEN);
    }

    #[test]
    fn jalr_clears_bit_zero_and_reads_rs1_before_linking() {
        let mut h = hart();
        let mut bus = RamBus::new();
        h.x[6] = 0x4200_0301;
        // jalr ra, 2(t1): the odd sum loses bit 0.
        assert_eq!(
            run(&mut h, &mut bus, &op(K_JALR, 1, 6, 0, 2, PC + 4)),
            Stepped::Retired
        );
        assert_eq!(h.pc, 0x4200_0302);
        assert_eq!(h.x[1], PC + 4);

        let mut h = hart();
        h.x[1] = 0x4200_0400;
        assert_eq!(
            run(&mut h, &mut bus, &op(K_JALR, 1, 1, 0, -4, PC + 4)),
            Stepped::Retired
        );
        assert_eq!(h.pc, 0x4200_03FC);
        assert_eq!(h.x[1], PC + 4);

        let mut h = hart();
        h.x[1] = 0x4200_0500;
        let mut o = op(K_JALR, 0, 1, 0, 0, PC + 2);
        o.len = 2;
        assert_eq!(run(&mut h, &mut bus, &o), Stepped::Retired);
        assert_eq!((h.pc, h.x[0], h.x[1]), (0x4200_0500, 0, 0x4200_0500));
    }

    #[test]
    fn csr_kinds_read_the_old_value_and_apply_their_operation() {
        let mut h = hart();
        let mut bus = RamBus::new();
        h.x[1] = 0x1234_5678;
        let csr = u32::from(CSR_MSCRATCH);

        assert_eq!(
            run(&mut h, &mut bus, &op(K_CSRRW, 5, 1, 0, 0, csr)),
            Stepped::Retired
        );
        assert_eq!((h.x[5], h.csr.mscratch), (0, 0x1234_5678));
        assert_eq!((h.pc, h.insns), (PC + 4, 41));

        // csrrs with rs1 x0 reads without writing.
        h.x[1] = 0x00FF_0000;
        assert_eq!(
            run(&mut h, &mut bus, &op(K_CSRRS, 6, 0, 0, 0, csr)),
            Stepped::Retired
        );
        assert_eq!((h.x[6], h.csr.mscratch), (0x1234_5678, 0x1234_5678));

        assert_eq!(
            run(&mut h, &mut bus, &op(K_CSRRS, 7, 1, 0, 0, csr)),
            Stepped::Retired
        );
        assert_eq!((h.x[7], h.csr.mscratch), (0x1234_5678, 0x12FF_5678));
        assert_eq!(
            run(&mut h, &mut bus, &op(K_CSRRC, 8, 1, 0, 0, csr)),
            Stepped::Retired
        );
        assert_eq!((h.x[8], h.csr.mscratch), (0x12FF_5678, 0x1200_5678));

        assert_eq!(
            run(&mut h, &mut bus, &op(K_CSRRWI, 9, 0, 0, 31, csr)),
            Stepped::Retired
        );
        assert_eq!((h.x[9], h.csr.mscratch), (0x1200_5678, 31));
        assert_eq!(
            run(&mut h, &mut bus, &op(K_CSRRSI, 10, 0, 0, 0, csr)),
            Stepped::Retired
        );
        assert_eq!(
            (h.x[10], h.csr.mscratch),
            (31, 31),
            "a zero immediate does not write"
        );
        assert_eq!(
            run(&mut h, &mut bus, &op(K_CSRRCI, 11, 0, 0, 1, csr)),
            Stepped::Retired
        );
        assert_eq!((h.x[11], h.csr.mscratch), (31, 30));

        // rd x0 keeps x0 zero while the write still happens.
        assert_eq!(
            run(&mut h, &mut bus, &op(K_CSRRWI, 0, 0, 0, 7, csr)),
            Stepped::Retired
        );
        assert_eq!((h.x[0], h.csr.mscratch), (0, 7));
    }

    #[test]
    fn an_illegal_csr_access_traps_with_the_instruction_bits() {
        let mut h = hart();
        let mut bus = RamBus::new();
        h.x[6] = 0x99;
        // csrrw x5, mvendorid, x6: a write to a read-only CSR.
        let o = op(K_CSRRW, 5, 6, 0, 0, u32::from(CSR_MVENDORID));
        assert_eq!(csr_insn_bits(&o), 0xF113_12F3);
        let out = run(&mut h, &mut bus, &o);
        assert_eq!(
            out,
            Stepped::Trapped(Trap::illegal_instruction(0xF113_12F3))
        );
        assert_eq!(h.csr.mcause, EXC_ILLEGAL_INSN);
        assert_eq!(h.csr.mtval, 0xF113_12F3);
        assert_eq!(h.csr.mepc, PC);
        assert_eq!((h.pc, h.insns, h.x[5]), (VECTOR, 40, 0));

        // csrrwi x5, mscratch, 31 encodes its immediate in the rs1 field.
        let imm_form = op(K_CSRRWI, 5, 0, 0, 31, u32::from(CSR_MSCRATCH));
        assert_eq!(csr_insn_bits(&imm_form), 0x340F_D2F3);
    }

    #[test]
    fn a_custom_csr_reaches_the_bus_with_the_instruction_count() {
        let mut h = hart();
        let mut bus = RamBus::new();
        bus.csr_value = 0xABCD;
        h.x[1] = 0x55;
        let o = op(K_CSRRW, 5, 1, 0, 0, 0x7C0);
        assert_eq!(run(&mut h, &mut bus, &o), Stepped::Retired);
        assert_eq!(h.x[5], 0xABCD);
        assert_eq!(bus.csr_calls, vec![(0x7C0, CsrOp::Write(0x55), 40)]);

        // An unknown number outside the custom ranges traps only in the strict mode.
        let mut h = hart();
        let unknown = op(K_CSRRS, 5, 0, 0, 0, 0x321);
        assert_eq!(
            exec_op(&mut h, &mut bus, PC, &unknown, false),
            Stepped::Retired
        );
        let mut h = hart();
        let out = exec_op(&mut h, &mut bus, PC, &unknown, true);
        assert_eq!(
            out,
            Stepped::Trapped(Trap::illegal_instruction(csr_insn_bits(&unknown)))
        );
        assert_eq!(h.csr.mcause, EXC_ILLEGAL_INSN);
    }

    #[test]
    fn ecall_ebreak_and_illegal_take_their_exceptions_precisely() {
        for (o, cause, tval) in [
            (op(K_ECALL, 0, 0, 0, 0, 0), EXC_ECALL_M, 0),
            (op(K_EBREAK, 0, 0, 0, 0, 0), EXC_BREAKPOINT, PC + 4),
            (
                op(K_ILLEGAL, 0, 0, 0, 0, 0xDEAD_BEEF),
                EXC_ILLEGAL_INSN,
                0xDEAD_BEEF,
            ),
        ] {
            let mut h = hart();
            let mut bus = RamBus::new();
            let mut o = o;
            o.pc_off = 4;
            let out = run(&mut h, &mut bus, &o);
            assert_eq!(
                out,
                Stepped::Trapped(Trap { cause, tval }),
                "{}",
                kind_name(o.kind)
            );
            assert_eq!(h.csr.mepc, PC + 4, "{}", kind_name(o.kind));
            assert_eq!(h.csr.mcause, cause, "{}", kind_name(o.kind));
            assert_eq!(h.csr.mtval, tval, "{}", kind_name(o.kind));
            assert_eq!((h.pc, h.insns), (VECTOR, 40), "{}", kind_name(o.kind));
        }
    }

    #[test]
    fn a_fetch_fault_op_raises_the_recorded_trap_at_its_own_pc() {
        let mut h = hart();
        let mut bus = RamBus::new();
        let mut o = op(K_FETCH_FAULT, 0, 0, 0, EXC_INSN_ACCESS_FAULT as i32, PC + 6);
        o.len = 0;
        o.pc_off = 6;
        let out = run(&mut h, &mut bus, &o);
        assert_eq!(
            out,
            Stepped::Trapped(Trap::instruction_access_fault(PC + 6))
        );
        assert_eq!(h.csr.mepc, PC + 6);
        assert_eq!(h.csr.mcause, EXC_INSN_ACCESS_FAULT);
        assert_eq!(h.csr.mtval, PC + 6);
        assert_eq!((h.pc, h.insns), (VECTOR, 40));
    }

    #[test]
    fn a_write_to_mstatus_ends_the_run_and_other_csr_accesses_do_not() {
        let mut bus = RamBus::new();
        let cases = [
            (K_CSRRSI, 8, CSR_MSTATUS, Stepped::RetiredStop),
            (K_CSRRCI, 8, CSR_MSTATUS, Stepped::RetiredStop),
            (K_CSRRWI, 0, CSR_MSTATUS, Stepped::RetiredStop),
            (K_CSRRSI, 0, CSR_MSTATUS, Stepped::Retired),
            (K_CSRRWI, 3, CSR_MSCRATCH, Stepped::Retired),
        ];
        for (kind, imm, csr, want) in cases {
            let mut h = hart();
            let out = run(&mut h, &mut bus, &op(kind, 5, 0, 0, imm, u32::from(csr)));
            assert_eq!(out, want, "kind {kind} imm {imm} csr {csr:#x}");
            assert_eq!(out.step_result(), Some(StepResult::Retired));
            assert_eq!((h.pc, h.insns), (PC + 4, 41));
        }
    }

    #[test]
    fn mret_wfi_and_fencei_retire_and_redirect_the_pc() {
        let mut h = hart();
        let mut bus = RamBus::new();
        h.csr.mepc = 0x4038_0100;
        h.csr.mstatus = MSTATUS_MPIE;
        assert_eq!(
            run(&mut h, &mut bus, &op(K_MRET, 0, 0, 0, 0, 0)),
            Stepped::RetiredStop,
            "mret can make an interrupt deliverable, so it ends the run"
        );
        assert_eq!((h.pc, h.insns), (0x4038_0100, 41));
        assert!(h.csr.mstatus_mie(), "mret restores MIE from MPIE");

        let mut h = hart();
        let out = run(&mut h, &mut bus, &op(K_WFI, 0, 0, 0, 0, PC + 4));
        assert_eq!(out, Stepped::Waiting);
        assert_eq!(out.step_result(), Some(StepResult::Wfi));
        assert!(h.wfi, "the hart halts in wfi");
        assert_eq!((h.pc, h.insns), (PC + 4, 41), "the pc advances past wfi");

        let mut h = hart();
        let out = run(&mut h, &mut bus, &op(K_FENCEI, 0, 0, 0, 0, PC + 4));
        assert_eq!(out, Stepped::Flushed);
        assert_eq!(out.step_result(), Some(StepResult::Retired));
        assert_eq!((h.pc, h.insns), (PC + 4, 41));
    }

    #[test]
    fn hook_and_fall_execute_nothing() {
        let mut h = hart();
        let mut bus = RamBus::new();
        let mut o = op(K_HOOK, 0, 0, 0, 0, 17);
        o.len = 0;
        o.pc_off = 12;
        let out = run(&mut h, &mut bus, &o);
        assert_eq!(out, Stepped::Hooked(17));
        assert_eq!(out.step_result(), None);
        assert_eq!(
            (h.pc, h.insns),
            (PC + 12, 40),
            "the hart pc is the hooked pc"
        );

        let mut h = hart();
        let mut o = op(K_FALL, 0, 0, 0, 0, PC + 64);
        o.len = 0;
        o.pc_off = 64;
        let out = run(&mut h, &mut bus, &o);
        assert_eq!(out, Stepped::Fell);
        assert_eq!(out.step_result(), None);
        assert_eq!((h.pc, h.insns), (PC + 64, 40));
    }

    const SP_MIN: u32 = 0x3FC8_1000;
    const SP_MAX: u32 = 0x3FC8_2000;

    fn guarded_hart(sp: u32, on_min: bool, on_max: bool) -> Hart {
        let mut h = hart();
        h.x[2] = sp;
        h.spmon = SpMonitor {
            on_min,
            on_max,
            min: SP_MIN,
            max: SP_MAX,
        };
        h
    }

    /// `addi sp, sp, imm`, the prologue every IDF task stack grows through.
    fn addi_sp(imm: i32) -> Op {
        let o = op(K_ADDI, 2, 2, 0, imm, 0);
        assert_ne!(o.flags & F_WRITES_SP, 0, "addi sp must carry F_WRITES_SP");
        o
    }

    /// The instruction still retires; the caller raises source 54 at the next boundary.
    #[test]
    fn the_stack_monitor_trips_on_an_addi_that_moves_sp_out_of_bounds() {
        let mut bus = RamBus::new();
        let mut h = guarded_hart(SP_MIN + 0x10, true, false);
        let out = run(&mut h, &mut bus, &addi_sp(-0x20));
        assert_eq!(out, Stepped::Spilled(SpSpill::Min));
        assert!(out.stops_block());
        assert_eq!(out.step_result(), Some(StepResult::Retired));
        assert_eq!(h.x[2], SP_MIN - 0x10, "the write itself stands");
        assert_eq!((h.pc, h.insns), (PC + 4, 41));
        assert_eq!(
            h.csr.mcause, 0,
            "the spill is no exception (source 54 is the SoC's)"
        );

        // The upper bound is the other direction, and the bounds are inclusive.
        let mut h = guarded_hart(SP_MAX, false, true);
        assert_eq!(run(&mut h, &mut bus, &addi_sp(0)), Stepped::Retired);
        let mut h = guarded_hart(SP_MAX, false, true);
        assert_eq!(
            run(&mut h, &mut bus, &addi_sp(4)),
            Stepped::Spilled(SpSpill::Max)
        );
        let mut h = guarded_hart(SP_MIN, true, true);
        assert_eq!(run(&mut h, &mut bus, &addi_sp(0)), Stepped::Retired);
    }

    #[test]
    fn enabling_the_monitor_while_sp_is_out_of_bounds_trips_on_the_next_sp_write() {
        let mut bus = RamBus::new();
        let mut h = guarded_hart(SP_MIN + 0x10, false, false);

        assert_eq!(run(&mut h, &mut bus, &addi_sp(-0x20)), Stepped::Retired);
        assert_eq!(h.x[2], SP_MIN - 0x10);

        // The ASSIST_DEBUG INTR_ENA write reaches the hart as a fresh monitor mirror.
        bus.spmon = SpMonitor {
            on_min: true,
            on_max: false,
            min: SP_MIN,
            max: SP_MAX,
        };
        h.spmon = bus.sp_monitor();
        assert!(h.spmon.armed());
        assert_eq!(
            h.insns, 41,
            "enabling the monitor executes nothing by itself"
        );

        // The next write to sp trips, although it leaves sp where it was.
        let out = run(&mut h, &mut bus, &addi_sp(0));
        assert_eq!(out, Stepped::Spilled(SpSpill::Min));
        assert_eq!((h.x[2], h.insns), (SP_MIN - 0x10, 42));
    }

    #[test]
    fn the_stack_monitor_never_trips_while_it_is_off() {
        let mut bus = RamBus::new();
        for sp in [0u32, SP_MIN - 1, SP_MAX + 1, u32::MAX] {
            let mut h = guarded_hart(sp, false, false);
            assert_eq!(
                run(&mut h, &mut bus, &addi_sp(0)),
                Stepped::Retired,
                "sp {sp:#x}"
            );
            let mut h = guarded_hart(sp, false, false);
            h.spmon.min = 0;
            h.spmon.max = 0;
            assert_eq!(
                run(&mut h, &mut bus, &addi_sp(0)),
                Stepped::Retired,
                "sp {sp:#x}"
            );
        }
    }

    #[test]
    fn the_stack_monitor_ignores_instructions_that_do_not_write_sp() {
        let mut bus = RamBus::new();

        let mut h = guarded_hart(SP_MIN + 0x10, true, true);
        let other = op(K_ADDI, 3, 2, 0, -0x20, 0);
        assert_eq!(other.flags & F_WRITES_SP, 0);
        assert_eq!(run(&mut h, &mut bus, &other), Stepped::Retired);
        assert_eq!((h.x[3], h.x[2]), (SP_MIN - 0x10, SP_MIN + 0x10));

        // A store of an out-of-bounds value read from sp.
        let mut h = guarded_hart(RAM_BASE, true, true);
        let store = op(K_SW, 0, 1, 2, 0, 0);
        assert_eq!(store.flags & F_WRITES_SP, 0);
        h.x[1] = RAM_BASE;
        assert_eq!(run(&mut h, &mut bus, &store), Stepped::Retired);
    }

    #[test]
    fn the_stack_monitor_checks_every_write_to_sp() {
        let mut bus = RamBus::new();

        // lw sp, 8(x1): the test RAM word 0x8B8A8988 is above the upper bound.
        let mut h = guarded_hart(SP_MIN + 0x10, true, true);
        h.x[1] = RAM_BASE;
        let load = op(K_LW, 2, 1, 0, 8, 0);
        assert_ne!(load.flags & F_WRITES_SP, 0);
        assert_eq!(run(&mut h, &mut bus, &load), Stepped::Spilled(SpSpill::Max));
        assert_eq!(h.x[2], 0x8B8A_8988);

        // jalr sp, 0(x1): the link value is far below the lower bound.
        let mut h = guarded_hart(SP_MIN + 0x10, true, true);
        h.x[1] = 0x4200_0800;
        let jalr = op(K_JALR, 2, 1, 0, 0, 0x10);
        assert_ne!(jalr.flags & F_WRITES_SP, 0);
        assert_eq!(run(&mut h, &mut bus, &jalr), Stepped::Spilled(SpSpill::Min));
        assert_eq!((h.x[2], h.pc), (0x10, 0x4200_0800));

        // lw sp, 0(x1) from the OkStop address: the spill subsumes the stop.
        let mut h = guarded_hart(SP_MIN + 0x10, true, true);
        h.x[1] = STOP_ADDR;
        let load_stop = op(K_LW, 2, 1, 0, 0, 0);
        let out = run(&mut h, &mut bus, &load_stop);
        assert_eq!(out, Stepped::Spilled(SpSpill::Max));
        assert!(out.stops_block() && out.refreshes_spmon());
        assert_eq!(h.x[2], 0x5A5A_5A5A);
    }

    /// Synthetic kinds retire nothing, trapping kinds do not count, the rest retire.
    #[test]
    fn every_op_kind_executes() {
        for kind in 0..K_COUNT {
            let name = kind_name(kind);
            let mut h = hart();
            let mut bus = RamBus::new();
            h.x[1] = RAM_BASE;
            let mut o = op(kind, 5, 1, 3, 8, PC + 4);
            if op::is_csr(kind) {
                o.imm2 = u32::from(CSR_MSCRATCH);
                o.imm = 1;
            }
            if op::is_branch(kind) || op::is_jump(kind) {
                o.imm = TAKEN as i32;
            }
            if kind == K_FETCH_FAULT {
                o.imm = EXC_INSN_ACCESS_FAULT as i32;
            }
            if is_synthetic(kind) {
                o.len = 0;
            }
            let out = exec_op(&mut h, &mut bus, PC, &o, false);
            let want = match kind {
                K_ECALL | K_EBREAK | K_ILLEGAL | K_FETCH_FAULT => {
                    assert_eq!(h.pc, VECTOR, "{name}");
                    None
                }
                K_WFI => Some(StepResult::Wfi),
                _ if is_synthetic(kind) => None,
                _ => Some(StepResult::Retired),
            };
            let retires = want == Some(StepResult::Retired) || want == Some(StepResult::Wfi);
            assert_eq!(h.insns, if retires { 41 } else { 40 }, "{name}: insns");
            assert_eq!(h.x[0], 0, "{name}: x0 stays zero");
            match want {
                Some(result) => assert_eq!(out.step_result(), Some(result), "{name}"),
                None if matches!(kind, K_HOOK | K_FALL) => {
                    assert_eq!(out.step_result(), None, "{name}")
                }
                None => assert!(
                    matches!(out, Stepped::Trapped(_)),
                    "{name}: expected a trap, got {out:?}"
                ),
            }
            assert_eq!(KIND_NAMES[kind as usize], name, "{name}");
        }
    }

    #[test]
    fn step_result_maps_every_outcome() {
        assert_eq!(Stepped::Retired.step_result(), Some(StepResult::Retired));
        assert_eq!(
            Stepped::RetiredStop.step_result(),
            Some(StepResult::Retired)
        );
        assert_eq!(
            Stepped::Spilled(SpSpill::Max).step_result(),
            Some(StepResult::Retired)
        );
        assert_eq!(Stepped::Flushed.step_result(), Some(StepResult::Retired));
        assert_eq!(Stepped::Waiting.step_result(), Some(StepResult::Wfi));
        let trap = Trap::ecall_from_machine();
        assert_eq!(
            Stepped::Trapped(trap).step_result(),
            Some(StepResult::Trapped(trap))
        );
        assert_eq!(Stepped::Hooked(3).step_result(), None);
        assert_eq!(Stepped::Fell.step_result(), None);
        for out in [
            Stepped::Retired,
            Stepped::Flushed,
            Stepped::Waiting,
            Stepped::Fell,
            Stepped::Hooked(3),
        ] {
            assert!(!out.stops_block(), "{out:?}");
            assert!(!out.refreshes_spmon(), "{out:?}");
        }
        assert!(Stepped::Trapped(trap).stops_block());
        assert!(!Stepped::Trapped(trap).refreshes_spmon());
        for out in [Stepped::RetiredStop, Stepped::Spilled(SpSpill::Min)] {
            assert!(out.stops_block(), "{out:?}");
            assert!(out.refreshes_spmon(), "{out:?}");
        }
    }
}
