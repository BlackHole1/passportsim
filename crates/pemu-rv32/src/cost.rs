//! Per-instruction-class cycle costs and the pipeline state they read across instruction
//! boundaries; row values and their silicon basis are in `specs/timing-profiles.toml`.
//!
//! Each retired instruction costs one cycle plus the extras of [`InsnCosts`], so the clock is
//! `insns + extra`. The block engine and [`crate::refstep::ref_step_costed`] both charge through
//! this module.

use crate::bus::{Bus, PF_MMIO};
use crate::op::{
    self, K_BEQ, K_BGE, K_BGEU, K_BLT, K_BLTU, K_BNE, K_DIV, K_DIVU, K_JAL, K_JALR, K_MRET, K_MULH,
    K_MULHSU, K_MULHU, K_REM, K_REMU, Op,
};

/// Cycles each instruction class costs beyond the base one; all zero is the `fast` profile.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct InsnCosts {
    pub taken_branch: u32,
    pub jump: u32,
    /// A 32-bit instruction at `pc = 2 mod 4` reached by a redirect, charged to that instruction.
    pub split_redirect: u32,
    pub load_use: u32,
    /// Before the part the operands decide ([`div_extra`]).
    pub div_base: u32,
    pub mulh: u32,
    /// In CPU cycles, derived from the profile's rows at the clocks of the moment.
    pub mmio_load: u32,
    pub mmio_store: u32,
    /// A [`PF_MMIO`] access inside the local window, in place of `mmio_load` and `mmio_store`.
    pub mmio_local: u32,
    pub local_mmio_base: u32,
    /// 0 for none.
    pub local_mmio_len: u32,
    /// See [`bank_step`]; the code and data bases are two views of one SRAM block.
    pub bank: u32,
    pub bank_code_base: u32,
    pub bank_data_base: u32,
    /// 0 for none.
    pub bank_len: u32,
}

impl InsnCosts {
    /// Whether every extra is 0; the local window and the bank block alone charge nothing.
    pub fn is_zero(&self) -> bool {
        InsnCosts {
            local_mmio_base: 0,
            local_mmio_len: 0,
            bank_code_base: 0,
            bank_data_base: 0,
            bank_len: 0,
            ..*self
        } == InsnCosts::default()
    }
}

/// SRAM bank interleave: address bit 3 picks one of two banks.
pub const BANK_SHIFT: u32 = 3;

/// Ops after a back-to-back access pair within which a redirect target is still fetched while the
/// run holds its bank: a loop's last access, its counter update and its branch.
const BANK_PAIR_OPS: u8 = 3;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Bank {
    /// Code chunk of the last retired op plus one while in the contended block; 0 for none.
    pub chunk: u32,
    /// The bank (plus one) every op since the last chunk entry accessed in one cycle, else 0.
    pub held: u8,
    pub accessed: bool,
    pub pair: u8,
}

impl Bank {
    pub fn to_bits(self) -> u64 {
        u64::from(self.chunk)
            | (u64::from(self.held & 3) << 32)
            | (u64::from(self.accessed) << 34)
            | (u64::from(self.pair & 7) << 35)
    }

    pub fn from_bits(b: u64) -> Bank {
        Bank {
            chunk: b as u32,
            held: ((b >> 32) & 3) as u8,
            accessed: (b >> 34) & 1 != 0,
            pair: ((b >> 35) & 7) as u8,
        }
    }
}

/// The SRAM bank rule (`specs/timing-profiles.toml` `sram_bank_cycles`): the extra of `op` at `pc`
/// with its base register holding `a`, and the [`Bank`] state after it. A data access holds its
/// bank for its cycle and a stall frees it. A chunk costs [`InsnCosts::bank`] when entered in
/// sequence, unstalled, after only one-cycle accesses in its bank, or when a redirect reaches it
/// within `BANK_PAIR_OPS` of a back-to-back pair. Ops outside the code view clear the state.
#[inline]
pub fn bank_step(costs: &InsnCosts, before: Pipe, pc: u32, op: &Op, a: u32) -> (u32, Bank) {
    if costs.bank == 0 || pc.wrapping_sub(costs.bank_code_base) >= costs.bank_len {
        return (0, Bank::default());
    }
    let st = before.bank;
    let stall = stall_extra(costs, before, pc, op);
    let access = if op::is_load(op.kind) || op::is_store(op.kind) {
        let addr = data_addr(op, a);
        (addr.wrapping_sub(costs.bank_data_base) < costs.bank_len)
            .then_some(((addr >> BANK_SHIFT) & 1) as u8 + 1)
    } else {
        None
    };
    let chunk = pc >> BANK_SHIFT;
    let chunk_bank = (chunk & 1) as u8 + 1;
    let entered = before.redirect || st.chunk != chunk.wrapping_add(1);
    let extra = if before.redirect {
        access == Some(chunk_bank) && st.pair > 0
    } else {
        entered && stall == 0 && st.held == chunk_bank
    };
    let extra = if extra { costs.bank } else { 0 };
    let held = match access {
        Some(b) if entered => b,
        Some(b) if stall == 0 && st.held == b => b,
        _ => 0,
    };
    let pair = if access.is_some() && st.accessed && stall == 0 {
        BANK_PAIR_OPS
    } else {
        st.pair.saturating_sub(1)
    };
    (
        extra,
        Bank {
            chunk: chunk.wrapping_add(1),
            held,
            accessed: access.is_some(),
            pair,
        },
    )
}

/// Pipeline state the cost rules read across an instruction boundary; invisible to the guest.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Pipe {
    pub redirect: bool,
    /// Destination of the last retired instruction if it was a load, else 0.
    pub load_rd: u8,
    pub bank: Bank,
}

impl Pipe {
    pub fn to_byte(self) -> u8 {
        (u8::from(self.redirect) << 7) | (self.load_rd & 31)
    }

    pub fn from_byte(b: u8, bank: Bank) -> Pipe {
        Pipe {
            redirect: b & 0x80 != 0,
            load_rd: b & 31,
            bank,
        }
    }

    #[inline]
    pub fn after(op: &Op, taken: bool, bank: Bank) -> Pipe {
        Pipe {
            redirect: redirects(op.kind, taken),
            load_rd: if op::is_load(op.kind) { op.rd & 31 } else { 0 },
            bank,
        }
    }
}

/// A taken branch redirects whatever its target, `beq` to the next instruction included.
#[inline]
pub fn redirects(kind: u8, taken: bool) -> bool {
    matches!(kind, K_JAL | K_JALR | K_MRET) || (op::is_branch(kind) && taken)
}

#[inline]
pub fn kind_extra(costs: &InsnCosts, kind: u8) -> u32 {
    match kind {
        K_JAL | K_JALR => costs.jump,
        K_MULH | K_MULHSU | K_MULHU => costs.mulh,
        _ => 0,
    }
}

#[inline]
pub fn is_divide(kind: u8) -> bool {
    matches!(kind, K_DIV | K_DIVU | K_REM | K_REMU)
}

/// [`InsnCosts::div_base`] plus one cycle per quotient bit of a normalising divider,
/// `max(clz(|b|) - clz(|a|), 0) + 1` over magnitudes, with `a = x[rs1]`, `b = x[rs2]`.
#[inline]
pub fn div_extra(costs: &InsnCosts, kind: u8, a: u32, b: u32) -> u32 {
    if costs.div_base == 0 || !is_divide(kind) {
        return 0;
    }
    let (a, b) = if matches!(kind, K_DIV | K_REM) {
        ((a as i32).unsigned_abs(), (b as i32).unsigned_abs())
    } else {
        (a, b)
    };
    costs.div_base + b.leading_zeros().saturating_sub(a.leading_zeros()) + 1
}

#[inline]
pub fn reads(op: &Op, r: u8) -> bool {
    r != 0 && !op::is_synthetic(op.kind) && (op.rs1 & 31 == r || op.rs2 & 31 == r)
}

#[inline]
pub fn split_target(pc: u32, op: &Op) -> bool {
    pc & 2 != 0 && op.len == 4
}

/// The static extra; the caller adds [`branch_extra`] and [`mmio_extra`] after execution.
#[inline]
pub fn entry_extra(costs: &InsnCosts, before: Pipe, pc: u32, op: &Op) -> u32 {
    kind_extra(costs, op.kind) + stall_extra(costs, before, pc, op)
}

/// The load-use and redirect stalls before `op` issues (the class rows are its own latency).
#[inline]
pub fn stall_extra(costs: &InsnCosts, before: Pipe, pc: u32, op: &Op) -> u32 {
    let mut extra = 0;
    if before.load_rd != 0 && reads(op, before.load_rd) {
        extra += costs.load_use;
    }
    if before.redirect && split_target(pc, op) {
        extra += costs.split_redirect;
    }
    extra
}

#[inline]
pub fn branch_extra(costs: &InsnCosts, kind: u8, taken: bool) -> u32 {
    if taken && op::is_branch(kind) {
        costs.taken_branch
    } else {
        0
    }
}

/// False for non-branch kinds.
#[inline]
pub fn branch_taken(kind: u8, a: u32, b: u32) -> bool {
    match kind {
        K_BEQ => a == b,
        K_BNE => a != b,
        K_BLT => (a as i32) < (b as i32),
        K_BGE => (a as i32) >= (b as i32),
        K_BLTU => a < b,
        K_BGEU => a >= b,
        _ => false,
    }
}

/// `addr` is computed before the op executes, since a load may overwrite `rs1`.
#[inline]
pub fn mmio_extra<B: Bus>(costs: &InsnCosts, bus: &B, op: &Op, addr: u32) -> u32 {
    let per = if !(op::is_load(op.kind) || op::is_store(op.kind)) {
        return 0;
    } else if addr.wrapping_sub(costs.local_mmio_base) < costs.local_mmio_len {
        costs.mmio_local
    } else if op::is_load(op.kind) {
        costs.mmio_load
    } else {
        costs.mmio_store
    };
    if per != 0 && bus.pages().entry(addr) & PF_MMIO != 0 {
        per
    } else {
        0
    }
}

#[inline]
pub fn data_addr(op: &Op, a: u32) -> u32 {
    a.wrapping_add(op.imm as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;

    fn device() -> InsnCosts {
        InsnCosts {
            taken_branch: 2,
            jump: 1,
            split_redirect: 1,
            load_use: 1,
            div_base: 13,
            mulh: 4,
            mmio_load: 5,
            mmio_store: 7,
            mmio_local: 1,
            local_mmio_base: 0x600C_4000,
            local_mmio_len: 0x1000,
            bank: 1,
            bank_code_base: 0x4038_0000,
            bank_data_base: 0x3FC8_0000,
            bank_len: 0x2_0000,
        }
    }

    /// Steady-state extra of one iteration of a loop at `head` with every access at `data`.
    fn loop_extra(c: &InsnCosts, head: u32, body: &[u32], data: u32) -> u32 {
        let ops: Vec<(u32, Op)> = body
            .iter()
            .scan(head, |pc, w| {
                let at = *pc;
                *pc += 4;
                Some((at, decode(*w, at)))
            })
            .collect();
        let mut pipe = Pipe::default();
        let mut last = 0;
        for _ in 0..4 {
            last = 0;
            for (pc, op) in &ops {
                let (bank, next) = bank_step(c, pipe, *pc, op, data);
                let taken = op::is_branch(op.kind);
                last += entry_extra(c, pipe, *pc, op) + bank + branch_extra(c, op.kind, taken);
                pipe = Pipe::after(op, taken, next);
            }
        }
        last
    }

    /// `probe_campaign_timing` `dres_*`: the 40-byte loop reads 2 or 3 cycles over its own by the
    /// parity of the code and data offsets over 8; Block 2 data or non-Block 1 code, nothing.
    #[test]
    fn a_dense_run_of_block_1_accesses_pays_one_cycle_for_each_chunk_in_its_bank() {
        let c = device();
        let lw = 0x0005_2283; // lw t0, 0(a0)
        let sw = 0x0055_2023; // sw t0, 0(a0)
        let addi = 0xFFF3_8393; // addi t2, t2, -1
        for (insn, taken_extra) in [(lw, 2), (sw, 2)] {
            for c_off in (0..72).step_by(8) {
                let head = 0x4038_2508 + c_off;
                let mut body = vec![insn; 8];
                body.push(addi);
                body.push(0xFC03_9EE3); // bnez t2, head
                for d_off in (0..72).step_by(8) {
                    let data = 0x3FC8_F840 + d_off;
                    let want = if ((c_off + d_off) / 8) % 2 == 0 { 2 } else { 3 };
                    assert_eq!(
                        loop_extra(&c, head, &body, data) - taken_extra,
                        want,
                        "head {head:#x} data {data:#x}"
                    );
                    assert_eq!(loop_extra(&c, head, &body, data + 0x2_0000), taken_extra);
                    assert_eq!(
                        loop_extra(&c, head - 0x4000_0000 + 0x4200_0000, &body, data),
                        taken_extra
                    );
                }
            }
        }
        // Sparse accesses leave the fetch a free cycle: a load, an unrelated addi, eight times.
        let mut body = Vec::new();
        for _ in 0..8 {
            body.push(lw);
            body.push(0x0013_8393); // addi t2, t2, 1
        }
        body.push(addi);
        body.push(0xFA03_9EE3); // bnez t2, back 68 bytes
        for data in [0x3FC8_F840, 0x3FC8_F848] {
            assert_eq!(loop_extra(&c, 0x4038_2500, &body, data), 2);
        }
        let st = Pipe::default();
        let op = decode(lw, 0x4038_2508);
        assert_eq!(
            bank_step(&InsnCosts::default(), st, 0x4038_2508, &op, 0x3FC8_F840),
            (0, Bank::default())
        );
        assert!(
            InsnCosts {
                bank_code_base: 0x4038_0000,
                bank_data_base: 0x3FC8_0000,
                bank_len: 0x2_0000,
                ..InsnCosts::default()
            }
            .is_zero()
        );
        let b = Bank {
            chunk: 0x0807_04A1,
            held: 2,
            accessed: true,
            pair: 3,
        };
        assert_eq!(Bank::from_bits(b.to_bits()), b);
    }

    #[test]
    fn the_pipe_byte_round_trips() {
        for redirect in [false, true] {
            for load_rd in 0..32u8 {
                let p = Pipe {
                    redirect,
                    load_rd,
                    bank: Bank::default(),
                };
                assert_eq!(Pipe::from_byte(p.to_byte(), Bank::default()), p);
            }
        }
        assert_eq!(Pipe::default().to_byte(), 0);
    }

    #[test]
    fn a_load_use_and_a_split_redirect_are_charged_to_the_instruction_after() {
        let c = device();
        let lw = decode(0x0005_2283, 0); // lw t0, 0(a0)
        let add = decode(0x0053_0333, 0); // add t1, t1, t0
        let addi = decode(0x0013_8393, 0); // addi t2, t2, 1
        let after_lw = Pipe::after(&lw, false, Bank::default());
        assert_eq!(after_lw.load_rd, 5);
        assert_eq!(entry_extra(&c, after_lw, 0x100, &add), 1);
        assert_eq!(entry_extra(&c, after_lw, 0x100, &addi), 0);
        let redirected = Pipe {
            redirect: true,
            load_rd: 0,
            bank: Bank::default(),
        };
        assert_eq!(entry_extra(&c, redirected, 0x102, &addi), 1);
        assert_eq!(entry_extra(&c, redirected, 0x104, &addi), 0);
        assert_eq!(entry_extra(&c, Pipe::default(), 0x102, &addi), 0);
        let zero = InsnCosts::default();
        assert!(zero.is_zero());
        assert_eq!(entry_extra(&zero, redirected, 0x102, &addi), 0);
        assert_eq!(entry_extra(&zero, after_lw, 0x100, &add), 0);
    }

    #[test]
    fn jumps_and_taken_branches_redirect_and_cost_their_rows() {
        let c = device();
        let jal = decode(0x0080_00EF, 0x100); // jal ra, +8
        let beq = decode(0x0052_8263, 0x100); // beq t0, t0, +4
        let div = decode(0x0273_42B3, 0x100); // div t0, t1, t2
        assert_eq!(kind_extra(&c, jal.kind), 1);
        assert_eq!(kind_extra(&c, div.kind), 0);
        assert_eq!(kind_extra(&c, beq.kind), 0);
        assert_eq!(branch_extra(&c, beq.kind, true), 2);
        assert_eq!(branch_extra(&c, beq.kind, false), 0);
        assert!(Pipe::after(&jal, false, Bank::default()).redirect);
        assert!(Pipe::after(&beq, true, Bank::default()).redirect);
        assert!(!Pipe::after(&beq, false, Bank::default()).redirect);
        assert!(!Pipe::after(&div, false, Bank::default()).redirect);
    }

    /// The three `probe_campaign_timing` operand pairs cost 15, 34 and 32 cycles.
    #[test]
    fn a_divide_costs_its_base_and_one_cycle_per_quotient_bit() {
        let c = device();
        let div = decode(0x0273_42B3, 0x100); // div t0, t1, t2
        let divu = decode(0x0273_52B3, 0x100); // divu t0, t1, t2
        assert_eq!(div_extra(&c, div.kind, 7, 1_000_000), 14);
        assert_eq!(div_extra(&c, div.kind, 1_000_000, 1), 33);
        assert_eq!(div_extra(&c, div.kind, 1_000_000, 7), 31);
        assert_eq!(
            div_extra(&c, div.kind, (-1_000_000i32) as u32, (-7i32) as u32),
            31
        );
        assert_eq!(div_extra(&c, divu.kind, (-1_000_000i32) as u32, 7), 13 + 30);
        assert_eq!(div_extra(&c, div.kind, 0, 0), 14);
        let mul = decode(0x0262_82B3, 0x100); // mul t0, t0, t1
        assert_eq!(div_extra(&c, mul.kind, 1_000_000, 1), 0);
        assert_eq!(div_extra(&InsnCosts::default(), div.kind, 1_000_000, 1), 0);
    }

    #[test]
    fn the_high_multiplies_cost_their_row() {
        let c = device();
        let mulhu = decode(0x0262_B2B3, 0x100); // mulhu t0, t0, t1
        let mulh = decode(0x0262_92B3, 0x100); // mulh t0, t0, t1
        let mulhsu = decode(0x0262_A2B3, 0x100); // mulhsu t0, t0, t1
        let mul = decode(0x0262_82B3, 0x100); // mul t0, t0, t1
        assert_eq!(kind_extra(&c, mulhu.kind), 4);
        assert_eq!(kind_extra(&c, mulh.kind), 4);
        assert_eq!(kind_extra(&c, mulhsu.kind), 4);
        assert_eq!(kind_extra(&c, mul.kind), 0);
    }

    #[test]
    fn a_window_without_costs_is_a_zero_table() {
        let window = InsnCosts {
            local_mmio_base: 0x600C_4000,
            local_mmio_len: 0x1000,
            ..InsnCosts::default()
        };
        assert!(window.is_zero());
        assert!(!device().is_zero());
    }
}
