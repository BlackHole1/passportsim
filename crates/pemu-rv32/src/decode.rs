//! The RV32IMC, Zicsr and Zifencei decoder (RISC-V unprivileged specification; the ESP32-C3 TRM
//! chapter 1 instruction set): instruction bits plus a PC become one [`Op`] following the
//! convention of [`crate::op`]. `pc_off` is left 0 for the block builder.

use crate::op::*;

/// The C3 has 16- and 32-bit instructions only; a longer encoding is illegal from its first word.
#[inline(always)]
pub const fn insn_len(lo: u16) -> u8 {
    if lo & 3 == 3 { 4 } else { 2 }
}

/// The high 16 bits of `word` are ignored for a compressed instruction.
#[inline]
pub fn decode(word: u32, pc: u32) -> Op {
    if word as u16 & 3 == 3 {
        decode32(word, pc)
    } else {
        decode16(word & 0xFFFF, pc)
    }
}

/// `None` when `bytes` is shorter than the instruction, as at the end of a code page.
#[inline]
pub fn decode_at(bytes: &[u8], pc: u32) -> Option<Op> {
    let &[b0, b1, ref rest @ ..] = bytes else {
        return None;
    };
    let lo = u16::from_le_bytes([b0, b1]);
    if insn_len(lo) == 2 {
        return Some(decode16(lo as u32, pc));
    }
    let &[b2, b3, ..] = rest else {
        return None;
    };
    Some(decode32(
        lo as u32 | (u16::from_le_bytes([b2, b3]) as u32) << 16,
        pc,
    ))
}

#[inline(always)]
const fn op(kind: u8, rd: u32, rs1: u32, rs2: u32, imm: i32, imm2: u32, len: u8) -> Op {
    Op {
        kind,
        rd: rd as u8,
        rs1: rs1 as u8,
        rs2: rs2 as u8,
        imm,
        imm2,
        len,
        flags: flags_for(kind, rd as u8),
        pc_off: 0,
    }
}

/// An op whose only effect is the `x[rd]` write: [`K_NOP`] for x0, so no executor needs a fixup.
#[inline(always)]
const fn plain(kind: u8, rd: u32, rs1: u32, rs2: u32, imm: i32, len: u8) -> Op {
    if rd == 0 {
        op(K_NOP, 0, 0, 0, 0, 0, len)
    } else {
        op(kind, rd, rs1, rs2, imm, 0, len)
    }
}

#[inline(always)]
const fn illegal(raw: u32, len: u8) -> Op {
    op(K_ILLEGAL, 0, 0, 0, 0, raw, len)
}

#[inline(always)]
const fn sext(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

#[inline(always)]
const fn bits(v: u32, hi: u32, lo: u32) -> u32 {
    (v >> lo) & ((1 << (hi - lo + 1)) - 1)
}

fn decode32(raw: u32, pc: u32) -> Op {
    let rd = bits(raw, 11, 7);
    let f3 = bits(raw, 14, 12);
    let rs1 = bits(raw, 19, 15);
    let rs2 = bits(raw, 24, 20);
    let f7 = bits(raw, 31, 25);
    let imm_i = (raw as i32) >> 20;
    let imm_s = ((raw as i32) >> 25) << 5 | bits(raw, 11, 7) as i32;
    let imm_b = ((raw as i32) >> 31) << 12
        | (bits(raw, 7, 7) << 11) as i32
        | (bits(raw, 30, 25) << 5) as i32
        | (bits(raw, 11, 8) << 1) as i32;
    let imm_u = (raw & 0xFFFF_F000) as i32;
    let imm_j = ((raw as i32) >> 31) << 20
        | (bits(raw, 19, 12) << 12) as i32
        | (bits(raw, 20, 20) << 11) as i32
        | (bits(raw, 30, 21) << 1) as i32;
    // Link value, branch fall-through, and where `wfi` and `fence.i` resume.
    let next = pc.wrapping_add(4);
    let ill = illegal(raw, 4);

    match bits(raw, 6, 0) {
        0b011_0111 => plain(K_LUI, rd, 0, 0, imm_u, 4),
        0b001_0111 => plain(K_AUIPC, rd, 0, 0, pc.wrapping_add(imm_u as u32) as i32, 4),
        0b110_1111 => op(
            K_JAL,
            rd,
            0,
            0,
            pc.wrapping_add(imm_j as u32) as i32,
            next,
            4,
        ),
        0b110_0111 if f3 == 0 => op(K_JALR, rd, rs1, 0, imm_i, next, 4),
        0b110_0011 => match f3 {
            0b000 | 0b001 | 0b100 | 0b101 | 0b110 | 0b111 => {
                let target = pc.wrapping_add(imm_b as u32) as i32;
                op(branch_kind(f3), 0, rs1, rs2, target, next, 4)
            }
            _ => ill,
        },
        // A load into x0 keeps its kind: it can fault or read MMIO.
        0b000_0011 => match f3 {
            0b000 => op(K_LB, rd, rs1, 0, imm_i, 0, 4),
            0b001 => op(K_LH, rd, rs1, 0, imm_i, 0, 4),
            0b010 => op(K_LW, rd, rs1, 0, imm_i, 0, 4),
            0b100 => op(K_LBU, rd, rs1, 0, imm_i, 0, 4),
            0b101 => op(K_LHU, rd, rs1, 0, imm_i, 0, 4),
            _ => ill,
        },
        0b010_0011 => match f3 {
            0b000 => op(K_SB, 0, rs1, rs2, imm_s, 0, 4),
            0b001 => op(K_SH, 0, rs1, rs2, imm_s, 0, 4),
            0b010 => op(K_SW, 0, rs1, rs2, imm_s, 0, 4),
            _ => ill,
        },
        // On RV32 a shift-immediate with imm[5] (bit 25) set is illegal.
        0b001_0011 => match f3 {
            0b000 => plain(K_ADDI, rd, rs1, 0, imm_i, 4),
            0b010 => plain(K_SLTI, rd, rs1, 0, imm_i, 4),
            0b011 => plain(K_SLTIU, rd, rs1, 0, imm_i, 4),
            0b100 => plain(K_XORI, rd, rs1, 0, imm_i, 4),
            0b110 => plain(K_ORI, rd, rs1, 0, imm_i, 4),
            0b111 => plain(K_ANDI, rd, rs1, 0, imm_i, 4),
            0b001 if f7 == 0b000_0000 => plain(K_SLLI, rd, rs1, 0, rs2 as i32, 4),
            0b101 if f7 == 0b000_0000 => plain(K_SRLI, rd, rs1, 0, rs2 as i32, 4),
            0b101 if f7 == 0b010_0000 => plain(K_SRAI, rd, rs1, 0, rs2 as i32, 4),
            _ => ill,
        },
        // The M kinds follow funct3 in kind order from K_MUL.
        0b011_0011 => match (f7, f3) {
            (0b000_0000, _) => plain(base_op_kind(f3), rd, rs1, rs2, 0, 4),
            (0b010_0000, 0b000) => plain(K_SUB, rd, rs1, rs2, 0, 4),
            (0b010_0000, 0b101) => plain(K_SRA, rd, rs1, rs2, 0, 4),
            (0b000_0001, _) => plain(K_MUL + f3 as u8, rd, rs1, rs2, 0, 4),
            _ => ill,
        },
        // Every FENCE is a NOP whatever its fields: GCC atomics and `vectors.S` emit several.
        0b000_1111 => match f3 {
            0b000 => op(K_NOP, 0, 0, 0, 0, 0, 4),
            0b001 => op(K_FENCEI, 0, 0, 0, 0, next, 4),
            _ => ill,
        },
        // Whole-word matches keep sret, sfence.vma and reserved funct12 values illegal.
        0b111_0011 => match f3 {
            0b000 => match raw {
                0x0000_0073 => op(K_ECALL, 0, 0, 0, 0, 0, 4),
                0x0010_0073 => op(K_EBREAK, 0, 0, 0, 0, 0, 4),
                0x3020_0073 => op(K_MRET, 0, 0, 0, 0, 0, 4),
                0x1050_0073 => op(K_WFI, 0, 0, 0, 0, next, 4),
                _ => ill,
            },
            0b001 => op(K_CSRRW, rd, rs1, 0, 0, bits(raw, 31, 20), 4),
            0b010 => op(K_CSRRS, rd, rs1, 0, 0, bits(raw, 31, 20), 4),
            0b011 => op(K_CSRRC, rd, rs1, 0, 0, bits(raw, 31, 20), 4),
            0b101 => op(K_CSRRWI, rd, 0, 0, rs1 as i32, bits(raw, 31, 20), 4),
            0b110 => op(K_CSRRSI, rd, 0, 0, rs1 as i32, bits(raw, 31, 20), 4),
            0b111 => op(K_CSRRCI, rd, 0, 0, rs1 as i32, bits(raw, 31, 20), 4),
            _ => ill,
        },
        // A, F, D, the RV64 word opcodes, longer encodings and unused opcodes.
        _ => ill,
    }
}

/// funct3 010 and 011 are not defined and never reach this.
#[inline(always)]
const fn branch_kind(f3: u32) -> u8 {
    match f3 {
        0b000 => K_BEQ,
        0b001 => K_BNE,
        0b100 => K_BLT,
        0b101 => K_BGE,
        0b110 => K_BLTU,
        _ => K_BGEU,
    }
}

#[inline(always)]
const fn base_op_kind(f3: u32) -> u8 {
    match f3 {
        0b000 => K_ADD,
        0b001 => K_SLL,
        0b010 => K_SLT,
        0b011 => K_SLTU,
        0b100 => K_XOR,
        0b101 => K_SRL,
        0b110 => K_OR,
        _ => K_AND,
    }
}

/// Register number of a 3-bit compressed register field: x8 to x15.
#[inline(always)]
const fn creg(f: u32) -> u32 {
    f + 8
}

fn decode16(raw: u32, pc: u32) -> Op {
    let f3 = bits(raw, 15, 13);
    let rd = bits(raw, 11, 7);
    let rs2 = bits(raw, 6, 2);
    let rdc = creg(bits(raw, 4, 2));
    let rs1c = creg(bits(raw, 9, 7));
    let rs2c = creg(bits(raw, 4, 2));
    let next = pc.wrapping_add(2);
    let ill = illegal(raw, 2);

    match (raw & 3, f3) {
        // --- Quadrant 0 ---
        // c.addi4spn. nzuimm 0 is reserved, which covers the all-zero c.unimp word.
        (0b00, 0b000) => {
            let uimm = bits(raw, 12, 11) << 4
                | bits(raw, 10, 7) << 6
                | bits(raw, 6, 6) << 2
                | bits(raw, 5, 5) << 3;
            if uimm == 0 {
                ill
            } else {
                op(K_ADDI, rdc, 2, 0, uimm as i32, 0, 2)
            }
        }
        // c.lw, c.sw.
        (0b00, 0b010) => op(K_LW, rdc, rs1c, 0, cl_offset(raw) as i32, 0, 2),
        (0b00, 0b110) => op(K_SW, 0, rs1c, rs2c, cl_offset(raw) as i32, 0, 2),
        // c.fld, c.flw, c.fsd, c.fsw need D or F, and funct3 100 is reserved.
        (0b00, _) => ill,

        // --- Quadrant 1 ---
        // c.addi and c.nop; the nzimm 0 HINT keeps the addi kind.
        (0b01, 0b000) => {
            if rd == 0 {
                op(K_NOP, 0, 0, 0, 0, 0, 2)
            } else {
                op(K_ADDI, rd, rd, 0, ci_imm(raw), 0, 2)
            }
        }
        // c.jal (RV32 only).
        (0b01, 0b001) => op(K_JAL, 1, 0, 0, cj_target(raw, pc), next, 2),
        // c.li: addi from x0.
        (0b01, 0b010) => plain(K_ADDI, rd, 0, 0, ci_imm(raw), 2),
        // c.addi16sp (rd 2) and c.lui (any other rd); both reserve a zero immediate.
        (0b01, 0b011) if rd == 2 => {
            let imm = sext(
                bits(raw, 12, 12) << 9
                    | bits(raw, 6, 6) << 4
                    | bits(raw, 5, 5) << 6
                    | bits(raw, 4, 3) << 7
                    | bits(raw, 2, 2) << 5,
                10,
            );
            if imm == 0 {
                ill
            } else {
                op(K_ADDI, 2, 2, 0, imm, 0, 2)
            }
        }
        (0b01, 0b011) => {
            let nzimm = bits(raw, 12, 12) << 5 | bits(raw, 6, 2);
            if nzimm == 0 {
                ill
            } else {
                plain(K_LUI, rd, 0, 0, sext(nzimm, 6) << 12, 2)
            }
        }
        // MISC-ALU: c.srli, c.srai, c.andi and the CA-format register forms.
        (0b01, 0b100) => decode16_misc_alu(raw, rs1c, rs2c, ill),
        // c.j: jal with rd 0.
        (0b01, 0b101) => op(K_JAL, 0, 0, 0, cj_target(raw, pc), next, 2),
        // c.beqz, c.bnez: compare against x0.
        (0b01, 0b110) => op(K_BEQ, 0, rs1c, 0, cb_target(raw, pc), next, 2),
        (0b01, _) => op(K_BNE, 0, rs1c, 0, cb_target(raw, pc), next, 2),

        // --- Quadrant 2 ---
        // c.slli; RV32 reserves shamt bit 5, and the shamt 0 HINT keeps the slli kind.
        (0b10, 0b000) => {
            if bits(raw, 12, 12) != 0 {
                ill
            } else {
                plain(K_SLLI, rd, rd, 0, rs2 as i32, 2)
            }
        }
        // c.lwsp; rd 0 is reserved.
        (0b10, 0b010) => {
            let uimm = bits(raw, 12, 12) << 5 | bits(raw, 6, 4) << 2 | bits(raw, 3, 2) << 6;
            if rd == 0 {
                ill
            } else {
                op(K_LW, rd, 2, 0, uimm as i32, 0, 2)
            }
        }
        (0b10, 0b100) => match (bits(raw, 12, 12), rd, rs2) {
            // c.jr; rs1 0 is reserved.
            (0, 0, 0) => ill,
            (0, _, 0) => op(K_JALR, 0, rd, 0, 0, next, 2),
            // c.mv: add from x0.
            (0, _, _) => plain(K_ADD, rd, 0, rs2, 0, 2),
            (_, 0, 0) => op(K_EBREAK, 0, 0, 0, 0, 0, 2),
            // c.jalr.
            (_, _, 0) => op(K_JALR, 1, rd, 0, 0, next, 2),
            (_, _, _) => plain(K_ADD, rd, rd, rs2, 0, 2),
        },
        // c.swsp.
        (0b10, 0b110) => {
            let uimm = bits(raw, 12, 9) << 2 | bits(raw, 8, 7) << 6;
            op(K_SW, 0, 2, rs2, uimm as i32, 0, 2)
        }
        // c.fldsp, c.flwsp, c.fsdsp, c.fswsp need D or F.
        (0b10, _) => ill,

        // Quadrant 3 is the 32-bit encoding, handled by `decode32`.
        _ => ill,
    }
}

/// Zero-extended word offset of c.lw and c.sw (CL and CS formats), scaled by 4.
#[inline(always)]
const fn cl_offset(raw: u32) -> u32 {
    bits(raw, 12, 10) << 3 | bits(raw, 6, 6) << 2 | bits(raw, 5, 5) << 6
}

/// Sign-extended 6-bit immediate of the CI format (c.addi, c.li, c.andi).
#[inline(always)]
const fn ci_imm(raw: u32) -> i32 {
    sext(bits(raw, 12, 12) << 5 | bits(raw, 6, 2), 6)
}

/// Absolute target of c.j and c.jal at `pc` (CJ format, 12-bit signed offset).
#[inline(always)]
const fn cj_target(raw: u32, pc: u32) -> i32 {
    let off = sext(
        bits(raw, 12, 12) << 11
            | bits(raw, 11, 11) << 4
            | bits(raw, 10, 9) << 8
            | bits(raw, 8, 8) << 10
            | bits(raw, 7, 7) << 6
            | bits(raw, 6, 6) << 7
            | bits(raw, 5, 3) << 1
            | bits(raw, 2, 2) << 5,
        12,
    );
    pc.wrapping_add(off as u32) as i32
}

/// Absolute taken target of c.beqz and c.bnez at `pc` (CB format, 9-bit signed offset).
#[inline(always)]
const fn cb_target(raw: u32, pc: u32) -> i32 {
    let off = sext(
        bits(raw, 12, 12) << 8
            | bits(raw, 11, 10) << 3
            | bits(raw, 6, 5) << 6
            | bits(raw, 4, 3) << 1
            | bits(raw, 2, 2) << 5,
        9,
    );
    pc.wrapping_add(off as u32) as i32
}

/// Quadrant 1 funct3 100. `rdc` is both the destination and the first source.
#[inline(always)]
fn decode16_misc_alu(raw: u32, rdc: u32, rs2c: u32, ill: Op) -> Op {
    let shamt = bits(raw, 12, 12) << 5 | bits(raw, 6, 2);
    match bits(raw, 11, 10) {
        // c.srli, c.srai: RV32 reserves shamt bit 5; the shamt 0 HINT keeps the kind.
        0b00 | 0b01 => {
            if shamt & 0x20 != 0 {
                ill
            } else {
                let kind = if bits(raw, 10, 10) == 0 {
                    K_SRLI
                } else {
                    K_SRAI
                };
                op(kind, rdc, rdc, 0, (shamt & 31) as i32, 0, 2)
            }
        }
        0b10 => op(K_ANDI, rdc, rdc, 0, ci_imm(raw), 0, 2),
        // c.sub, c.xor, c.or, c.and; bit 12 set is RV64 or reserved.
        _ => match (bits(raw, 12, 12), bits(raw, 6, 5)) {
            (0, 0b00) => op(K_SUB, rdc, rdc, rs2c, 0, 0, 2),
            (0, 0b01) => op(K_XOR, rdc, rdc, rs2c, 0, 0, 2),
            (0, 0b10) => op(K_OR, rdc, rdc, rs2c, 0, 0, 2),
            (0, _) => op(K_AND, rdc, rdc, rs2c, 0, 0, 2),
            _ => ill,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PC every case decodes at: inside IRAM, 4-byte aligned, so absolute targets are easy to read.
    const PC: u32 = 0x4038_0100;
    const NEXT4: u32 = PC + 4;
    const NEXT2: u32 = PC + 2;

    /// Expected kind, rd, rs1, rs2, imm, imm2, len; `run` checks the flags against [`flags_for`].
    type Fields = (u8, u8, u8, u8, i32, u32, u8);

    type Case = (u32, Fields);

    fn fields(o: &Op) -> Fields {
        (o.kind, o.rd, o.rs1, o.rs2, o.imm, o.imm2, o.len)
    }

    #[track_caller]
    fn run(cases: &[Case]) {
        for &(word, want) in cases {
            let got = decode(word, PC);
            assert_eq!(
                fields(&got),
                want,
                "{word:#010x} decoded as {}",
                kind_name(got.kind)
            );
            assert_eq!(
                got.flags,
                flags_for(want.0, want.1),
                "{word:#010x} flags of {}",
                kind_name(got.kind)
            );
            assert_eq!(got.pc_off, 0, "{word:#010x} pc_off");
            let len = got.len as usize;
            let bytes = word.to_le_bytes();
            let at = decode_at(&bytes[..len], PC).expect("enough bytes");
            assert_eq!(fields(&at), want, "{word:#010x} through decode_at");
            assert_eq!(insn_len(word as u16), got.len, "{word:#010x} insn_len");
        }
    }

    const fn ill4(word: u32) -> Case {
        (word, (K_ILLEGAL, 0, 0, 0, 0, word, 4))
    }
    const fn ill2(word: u32) -> Case {
        (word, (K_ILLEGAL, 0, 0, 0, 0, word, 2))
    }

    // --- 32-bit encoders, written from the RISC-V unprivileged specification base formats ---

    const fn r_type(f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, opc: u32) -> u32 {
        f7 << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | rd << 7 | opc
    }
    const fn i_type(imm: i32, rs1: u32, f3: u32, rd: u32, opc: u32) -> u32 {
        (imm as u32 & 0xFFF) << 20 | rs1 << 15 | f3 << 12 | rd << 7 | opc
    }
    const fn s_type(imm: i32, rs2: u32, rs1: u32, f3: u32, opc: u32) -> u32 {
        let v = imm as u32;
        (v >> 5 & 0x7F) << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | (v & 31) << 7 | opc
    }
    const fn b_type(imm: i32, rs2: u32, rs1: u32, f3: u32, opc: u32) -> u32 {
        let v = imm as u32;
        (v >> 12 & 1) << 31
            | (v >> 5 & 0x3F) << 25
            | rs2 << 20
            | rs1 << 15
            | f3 << 12
            | (v >> 1 & 0xF) << 8
            | (v >> 11 & 1) << 7
            | opc
    }
    const fn u_type(imm20: u32, rd: u32, opc: u32) -> u32 {
        (imm20 & 0xF_FFFF) << 12 | rd << 7 | opc
    }
    const fn j_type(imm: i32, rd: u32, opc: u32) -> u32 {
        let v = imm as u32;
        (v >> 20 & 1) << 31
            | (v >> 1 & 0x3FF) << 21
            | (v >> 11 & 1) << 20
            | (v >> 12 & 0xFF) << 12
            | rd << 7
            | opc
    }

    // --- Compressed encoders, written from the C extension formats ---

    /// CR: c.jr, c.jalr, c.mv, c.add, c.ebreak (quadrant 2, funct3 100).
    const fn cr(b12: u32, rd: u32, rs2: u32) -> u32 {
        0b100 << 13 | b12 << 12 | rd << 7 | rs2 << 2 | 0b10
    }
    /// CI: imm[5] at bit 12, imm[4:0] at bits 6:2 (c.addi, c.li, c.lui, c.slli).
    const fn ci(f3: u32, imm: i32, rd: u32, q: u32) -> u32 {
        let v = imm as u32;
        f3 << 13 | (v >> 5 & 1) << 12 | rd << 7 | (v & 31) << 2 | q
    }
    /// CIW: c.addi4spn, nzuimm[5:4|9:6|2|3] (quadrant 0, funct3 000).
    const fn ciw(uimm: u32, rdc: u32) -> u32 {
        (uimm >> 4 & 3) << 11
            | (uimm >> 6 & 0xF) << 7
            | (uimm >> 2 & 1) << 6
            | (uimm >> 3 & 1) << 5
            | (rdc - 8) << 2
    }
    /// CL and CS word forms: c.lw (q 00, funct3 010) and c.sw (q 00, funct3 110), uimm[5:3|2|6].
    const fn clw(f3: u32, uimm: u32, rs1c: u32, regc: u32) -> u32 {
        f3 << 13
            | (uimm >> 3 & 7) << 10
            | (rs1c - 8) << 7
            | (uimm >> 2 & 1) << 6
            | (uimm >> 6 & 1) << 5
            | (regc - 8) << 2
    }
    /// CA: c.sub, c.xor, c.or, c.and (quadrant 1, funct3 100, funct2 11).
    const fn ca(b12: u32, sub: u32, rdc: u32, rs2c: u32) -> u32 {
        0b100 << 13 | b12 << 12 | 0b11 << 10 | (rdc - 8) << 7 | sub << 5 | (rs2c - 8) << 2 | 0b01
    }
    /// CB immediate form: c.srli (funct2 00), c.srai (01), c.andi (10).
    const fn cbi(f2: u32, imm: i32, rdc: u32) -> u32 {
        let v = imm as u32;
        0b100 << 13 | (v >> 5 & 1) << 12 | f2 << 10 | (rdc - 8) << 7 | (v & 31) << 2 | 0b01
    }
    /// CB branch form: c.beqz (funct3 110), c.bnez (111), offset imm[8|4:3|7:6|2:1|5].
    const fn cb(f3: u32, off: i32, rs1c: u32) -> u32 {
        let v = off as u32;
        f3 << 13
            | (v >> 8 & 1) << 12
            | (v >> 3 & 3) << 10
            | (rs1c - 8) << 7
            | (v >> 6 & 3) << 5
            | (v >> 1 & 3) << 3
            | (v >> 5 & 1) << 2
            | 0b01
    }
    /// CJ: c.jal (funct3 001), c.j (101), offset imm[11|4|9:8|10|6|7|3:1|5].
    const fn cj(f3: u32, off: i32) -> u32 {
        let v = off as u32;
        f3 << 13
            | (v >> 11 & 1) << 12
            | (v >> 4 & 1) << 11
            | (v >> 8 & 3) << 9
            | (v >> 10 & 1) << 8
            | (v >> 6 & 1) << 7
            | (v >> 7 & 1) << 6
            | (v >> 1 & 7) << 3
            | (v >> 5 & 1) << 2
            | 0b01
    }
    /// c.lwsp: uimm[5|4:2|7:6].
    const fn clwsp(uimm: u32, rd: u32) -> u32 {
        0b010 << 13
            | (uimm >> 5 & 1) << 12
            | rd << 7
            | (uimm >> 2 & 7) << 4
            | (uimm >> 6 & 3) << 2
            | 0b10
    }
    /// c.swsp: uimm[5:2|7:6].
    const fn cswsp(uimm: u32, rs2: u32) -> u32 {
        0b110 << 13 | (uimm >> 2 & 0xF) << 9 | (uimm >> 6 & 3) << 7 | rs2 << 2 | 0b10
    }
    /// c.addi16sp: nzimm[9|4|6|8:7|5], rd field 2.
    const fn caddi16sp(imm: i32) -> u32 {
        let v = imm as u32;
        0b011 << 13
            | (v >> 9 & 1) << 12
            | 2 << 7
            | (v >> 4 & 1) << 6
            | (v >> 6 & 1) << 5
            | (v >> 7 & 3) << 3
            | (v >> 5 & 1) << 2
            | 0b01
    }

    const T_U: &[Case] = &[
        (
            u_type(0x12345, 5, 0b011_0111),
            (K_LUI, 5, 0, 0, 0x1234_5000, 0, 4),
        ),
        (u_type(0x12345, 0, 0b011_0111), (K_NOP, 0, 0, 0, 0, 0, 4)),
        (
            u_type(0xF_FFFF, 6, 0b011_0111),
            (K_LUI, 6, 0, 0, -4096, 0, 4),
        ),
        (
            u_type(0x10, 6, 0b001_0111),
            (K_AUIPC, 6, 0, 0, PC.wrapping_add(0x10_000) as i32, 0, 4),
        ),
        (
            u_type(0xF_FFFF, 7, 0b001_0111),
            (K_AUIPC, 7, 0, 0, PC.wrapping_sub(0x1000) as i32, 0, 4),
        ),
        (u_type(0x10, 0, 0b001_0111), (K_NOP, 0, 0, 0, 0, 0, 4)),
    ];

    const T_OP_IMM: &[Case] = &[
        (
            i_type(-1, 1, 0b000, 3, 0b001_0011),
            (K_ADDI, 3, 1, 0, -1, 0, 4),
        ),
        (
            i_type(5, 1, 0b010, 3, 0b001_0011),
            (K_SLTI, 3, 1, 0, 5, 0, 4),
        ),
        (
            i_type(-5, 1, 0b011, 3, 0b001_0011),
            (K_SLTIU, 3, 1, 0, -5, 0, 4),
        ),
        (
            i_type(0x7FF, 1, 0b100, 3, 0b001_0011),
            (K_XORI, 3, 1, 0, 0x7FF, 0, 4),
        ),
        (
            i_type(-0x800, 1, 0b110, 3, 0b001_0011),
            (K_ORI, 3, 1, 0, -0x800, 0, 4),
        ),
        (
            i_type(7, 1, 0b111, 3, 0b001_0011),
            (K_ANDI, 3, 1, 0, 7, 0, 4),
        ),
        (
            i_type(31, 1, 0b001, 3, 0b001_0011),
            (K_SLLI, 3, 1, 0, 31, 0, 4),
        ),
        (
            i_type(7, 1, 0b101, 3, 0b001_0011),
            (K_SRLI, 3, 1, 0, 7, 0, 4),
        ),
        (
            i_type(0x407, 1, 0b101, 3, 0b001_0011),
            (K_SRAI, 3, 1, 0, 7, 0, 4),
        ),
        (
            i_type(-1, 1, 0b000, 0, 0b001_0011),
            (K_NOP, 0, 0, 0, 0, 0, 4),
        ),
        // RV32 reserves imm[5] of a shift-immediate (shamt 32) and every other funct7.
        ill4(i_type(0x020, 1, 0b001, 3, 0b001_0011)),
        ill4(i_type(0x020, 1, 0b101, 3, 0b001_0011)),
        ill4(i_type(0x207, 1, 0b101, 3, 0b001_0011)),
    ];

    const T_OP: &[Case] = &[
        (
            r_type(0, 2, 1, 0b000, 3, 0b011_0011),
            (K_ADD, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(0b010_0000, 2, 1, 0b000, 3, 0b011_0011),
            (K_SUB, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(0, 2, 1, 0b001, 3, 0b011_0011),
            (K_SLL, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(0, 2, 1, 0b010, 3, 0b011_0011),
            (K_SLT, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(0, 2, 1, 0b011, 3, 0b011_0011),
            (K_SLTU, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(0, 2, 1, 0b100, 3, 0b011_0011),
            (K_XOR, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(0, 2, 1, 0b101, 3, 0b011_0011),
            (K_SRL, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(0b010_0000, 2, 1, 0b101, 3, 0b011_0011),
            (K_SRA, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(0, 2, 1, 0b110, 3, 0b011_0011),
            (K_OR, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(0, 2, 1, 0b111, 3, 0b011_0011),
            (K_AND, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(1, 2, 1, 0b000, 3, 0b011_0011),
            (K_MUL, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(1, 2, 1, 0b001, 3, 0b011_0011),
            (K_MULH, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(1, 2, 1, 0b010, 3, 0b011_0011),
            (K_MULHSU, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(1, 2, 1, 0b011, 3, 0b011_0011),
            (K_MULHU, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(1, 2, 1, 0b100, 3, 0b011_0011),
            (K_DIV, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(1, 2, 1, 0b101, 3, 0b011_0011),
            (K_DIVU, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(1, 2, 1, 0b110, 3, 0b011_0011),
            (K_REM, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(1, 2, 1, 0b111, 3, 0b011_0011),
            (K_REMU, 3, 1, 2, 0, 0, 4),
        ),
        (
            r_type(1, 2, 1, 0b000, 0, 0b011_0011),
            (K_NOP, 0, 0, 0, 0, 0, 4),
        ),
        ill4(r_type(0b010_0000, 2, 1, 0b001, 3, 0b011_0011)),
        ill4(r_type(0b000_0010, 2, 1, 0b000, 3, 0b011_0011)),
    ];

    const T_MEM: &[Case] = &[
        (i_type(4, 1, 0b000, 3, 0b000_0011), (K_LB, 3, 1, 0, 4, 0, 4)),
        (
            i_type(-4, 1, 0b001, 3, 0b000_0011),
            (K_LH, 3, 1, 0, -4, 0, 4),
        ),
        (
            i_type(0x7FF, 1, 0b010, 3, 0b000_0011),
            (K_LW, 3, 1, 0, 0x7FF, 0, 4),
        ),
        (
            i_type(4, 1, 0b100, 3, 0b000_0011),
            (K_LBU, 3, 1, 0, 4, 0, 4),
        ),
        (
            i_type(4, 1, 0b101, 3, 0b000_0011),
            (K_LHU, 3, 1, 0, 4, 0, 4),
        ),
        (i_type(4, 1, 0b010, 0, 0b000_0011), (K_LW, 0, 1, 0, 4, 0, 4)),
        ill4(i_type(4, 1, 0b011, 3, 0b000_0011)),
        ill4(i_type(4, 1, 0b110, 3, 0b000_0011)),
        ill4(i_type(4, 1, 0b111, 3, 0b000_0011)),
        (
            s_type(-8, 2, 1, 0b000, 0b010_0011),
            (K_SB, 0, 1, 2, -8, 0, 4),
        ),
        (s_type(8, 2, 1, 0b001, 0b010_0011), (K_SH, 0, 1, 2, 8, 0, 4)),
        (
            s_type(-0x800, 2, 1, 0b010, 0b010_0011),
            (K_SW, 0, 1, 2, -0x800, 0, 4),
        ),
        ill4(s_type(8, 2, 1, 0b011, 0b010_0011)),
        ill4(s_type(8, 2, 1, 0b111, 0b010_0011)),
    ];

    const T_CTRL: &[Case] = &[
        (
            j_type(0x100, 1, 0b110_1111),
            (K_JAL, 1, 0, 0, PC.wrapping_add(0x100) as i32, NEXT4, 4),
        ),
        (
            j_type(-0x200, 0, 0b110_1111),
            (K_JAL, 0, 0, 0, PC.wrapping_sub(0x200) as i32, NEXT4, 4),
        ),
        (
            i_type(0x24, 5, 0b000, 1, 0b110_0111),
            (K_JALR, 1, 5, 0, 0x24, NEXT4, 4),
        ),
        (
            i_type(-1, 5, 0b000, 0, 0b110_0111),
            (K_JALR, 0, 5, 0, -1, NEXT4, 4),
        ),
        ill4(i_type(0, 5, 0b001, 1, 0b110_0111)),
        (
            b_type(0x40, 2, 1, 0b000, 0b110_0011),
            (K_BEQ, 0, 1, 2, PC.wrapping_add(0x40) as i32, NEXT4, 4),
        ),
        (
            b_type(-0x40, 2, 1, 0b001, 0b110_0011),
            (K_BNE, 0, 1, 2, PC.wrapping_sub(0x40) as i32, NEXT4, 4),
        ),
        (
            b_type(0xFFE, 2, 1, 0b100, 0b110_0011),
            (K_BLT, 0, 1, 2, PC.wrapping_add(0xFFE) as i32, NEXT4, 4),
        ),
        (
            b_type(-0x1000, 2, 1, 0b101, 0b110_0011),
            (K_BGE, 0, 1, 2, PC.wrapping_sub(0x1000) as i32, NEXT4, 4),
        ),
        (
            b_type(2, 2, 1, 0b110, 0b110_0011),
            (K_BLTU, 0, 1, 2, PC.wrapping_add(2) as i32, NEXT4, 4),
        ),
        (
            b_type(2, 2, 1, 0b111, 0b110_0011),
            (K_BGEU, 0, 1, 2, PC.wrapping_add(2) as i32, NEXT4, 4),
        ),
        ill4(b_type(2, 2, 1, 0b010, 0b110_0011)),
        ill4(b_type(2, 2, 1, 0b011, 0b110_0011)),
    ];

    const T_SYSTEM: &[Case] = &[
        // mstatus (0x300), mepc (0x341) and the ESP performance counter 0x7E2.
        (
            i_type(0x300, 5, 0b001, 6, 0b111_0011),
            (K_CSRRW, 6, 5, 0, 0, 0x300, 4),
        ),
        (
            i_type(0x300, 5, 0b010, 6, 0b111_0011),
            (K_CSRRS, 6, 5, 0, 0, 0x300, 4),
        ),
        (
            i_type(0x7E2, 5, 0b011, 6, 0b111_0011),
            (K_CSRRC, 6, 5, 0, 0, 0x7E2, 4),
        ),
        (
            i_type(0x341, 31, 0b101, 6, 0b111_0011),
            (K_CSRRWI, 6, 0, 0, 31, 0x341, 4),
        ),
        (
            i_type(0x341, 8, 0b110, 6, 0b111_0011),
            (K_CSRRSI, 6, 0, 0, 8, 0x341, 4),
        ),
        (
            i_type(0xFFF, 0, 0b111, 6, 0b111_0011),
            (K_CSRRCI, 6, 0, 0, 0, 0xFFF, 4),
        ),
        // `rd` 0 keeps the CSR kind: the CSR is still accessed.
        (
            i_type(0x300, 1, 0b001, 0, 0b111_0011),
            (K_CSRRW, 0, 1, 0, 0, 0x300, 4),
        ),
        (0x0000_0073, (K_ECALL, 0, 0, 0, 0, 0, 4)),
        (0x0010_0073, (K_EBREAK, 0, 0, 0, 0, 0, 4)),
        (0x3020_0073, (K_MRET, 0, 0, 0, 0, 0, 4)),
        (0x1050_0073, (K_WFI, 0, 0, 0, 0, NEXT4, 4)),
        // uret, sret, dret, sfence.vma, funct3 100, and a nonzero rd on a privileged encoding.
        ill4(0x0020_0073),
        ill4(0x1020_0073),
        ill4(0x7B20_0073),
        ill4(0x1200_0073),
        ill4(0x3020_00F3),
        ill4(i_type(0x300, 1, 0b100, 2, 0b111_0011)),
    ];

    const T_MISC_MEM: &[Case] = &[
        (0x0FF0_000F, (K_NOP, 0, 0, 0, 0, 0, 4)),
        (0x0330_000F, (K_NOP, 0, 0, 0, 0, 0, 4)),
        (0x0310_000F, (K_NOP, 0, 0, 0, 0, 0, 4)),
        (0x0230_000F, (K_NOP, 0, 0, 0, 0, 0, 4)),
        (0x0000_000F, (K_NOP, 0, 0, 0, 0, 0, 4)),
        (0x8330_000F, (K_NOP, 0, 0, 0, 0, 0, 4)),
        (
            i_type(0x033, 3, 0b000, 2, 0b000_1111),
            (K_NOP, 0, 0, 0, 0, 0, 4),
        ),
        (0x0000_100F, (K_FENCEI, 0, 0, 0, 0, NEXT4, 4)),
        (
            i_type(0x123, 5, 0b001, 7, 0b000_1111),
            (K_FENCEI, 0, 0, 0, 0, NEXT4, 4),
        ),
        ill4(i_type(0, 0, 0b010, 0, 0b000_1111)),
        ill4(i_type(0, 0, 0b111, 0, 0b000_1111)),
    ];

    const T_UNIMPLEMENTED: &[Case] = &[
        ill4(0x1000_22AF), // lr.w (A)
        ill4(0x0CA5_22AF), // amoswap.w (A)
        ill4(0x0000_2007), // flw (F)
        ill4(0x0000_2027), // fsw (F)
        ill4(0x0000_3007), // fld (D)
        ill4(0x0000_0053), // OP-FP
        ill4(0x0000_0043), // FMADD
        ill4(0x0000_001B), // OP-IMM-32 (RV64)
        ill4(0x0000_003B), // OP-32 (RV64)
        ill4(0x0000_005B), // reserved custom-2
        ill4(0x0000_007F), // 48-bit or longer encoding: not implemented
        ill4(0xFFFF_FFFF),
    ];

    const T_C0: &[Case] = &[
        (ciw(16, 10), (K_ADDI, 10, 2, 0, 16, 0, 2)),
        (ciw(1020, 15), (K_ADDI, 15, 2, 0, 1020, 0, 2)),
        (clw(0b010, 8, 10, 11), (K_LW, 11, 10, 0, 8, 0, 2)),
        (clw(0b010, 124, 8, 15), (K_LW, 15, 8, 0, 124, 0, 2)),
        (clw(0b110, 4, 10, 11), (K_SW, 0, 10, 11, 4, 0, 2)),
        (clw(0b110, 0, 15, 8), (K_SW, 0, 15, 8, 0, 0, 2)),
        ill2(ciw(0, 10)),
        ill2(0x0000),
        ill2(0b001 << 13), // c.fld (D)
        ill2(0b011 << 13), // c.flw (F)
        ill2(0b100 << 13), // reserved
        ill2(0b101 << 13), // c.fsd (D)
        ill2(0b111 << 13), // c.fsw (F)
    ];

    const T_C1: &[Case] = &[
        (ci(0b000, 0, 0, 0b01), (K_NOP, 0, 0, 0, 0, 0, 2)),
        (ci(0b000, 5, 0, 0b01), (K_NOP, 0, 0, 0, 0, 0, 2)),
        (ci(0b000, -1, 10, 0b01), (K_ADDI, 10, 10, 0, -1, 0, 2)),
        (ci(0b000, 31, 15, 0b01), (K_ADDI, 15, 15, 0, 31, 0, 2)),
        (ci(0b000, 0, 10, 0b01), (K_ADDI, 10, 10, 0, 0, 0, 2)),
        (
            cj(0b001, 0x10),
            (K_JAL, 1, 0, 0, PC.wrapping_add(0x10) as i32, NEXT2, 2),
        ),
        (
            cj(0b001, -0x800),
            (K_JAL, 1, 0, 0, PC.wrapping_sub(0x800) as i32, NEXT2, 2),
        ),
        (ci(0b010, -32, 10, 0b01), (K_ADDI, 10, 0, 0, -32, 0, 2)),
        (ci(0b010, 5, 0, 0b01), (K_NOP, 0, 0, 0, 0, 0, 2)),
        (caddi16sp(-16), (K_ADDI, 2, 2, 0, -16, 0, 2)),
        (caddi16sp(496), (K_ADDI, 2, 2, 0, 496, 0, 2)),
        (caddi16sp(-512), (K_ADDI, 2, 2, 0, -512, 0, 2)),
        (ci(0b011, 0x1F, 10, 0b01), (K_LUI, 10, 0, 0, 0x1F000, 0, 2)),
        (ci(0b011, -1, 10, 0b01), (K_LUI, 10, 0, 0, -4096, 0, 2)),
        (ci(0b011, 1, 0, 0b01), (K_NOP, 0, 0, 0, 0, 0, 2)),
        (cbi(0b00, 7, 10), (K_SRLI, 10, 10, 0, 7, 0, 2)),
        (cbi(0b01, 31, 15), (K_SRAI, 15, 15, 0, 31, 0, 2)),
        (cbi(0b00, 0, 10), (K_SRLI, 10, 10, 0, 0, 0, 2)),
        (cbi(0b01, 0, 10), (K_SRAI, 10, 10, 0, 0, 0, 2)),
        (cbi(0b10, -8, 10), (K_ANDI, 10, 10, 0, -8, 0, 2)),
        (cbi(0b10, 31, 10), (K_ANDI, 10, 10, 0, 31, 0, 2)),
        (ca(0, 0b00, 10, 11), (K_SUB, 10, 10, 11, 0, 0, 2)),
        (ca(0, 0b01, 10, 11), (K_XOR, 10, 10, 11, 0, 0, 2)),
        (ca(0, 0b10, 10, 11), (K_OR, 10, 10, 11, 0, 0, 2)),
        (ca(0, 0b11, 8, 15), (K_AND, 8, 8, 15, 0, 0, 2)),
        (
            cj(0b101, -0x20),
            (K_JAL, 0, 0, 0, PC.wrapping_sub(0x20) as i32, NEXT2, 2),
        ),
        (
            cj(0b101, 0x7FE),
            (K_JAL, 0, 0, 0, PC.wrapping_add(0x7FE) as i32, NEXT2, 2),
        ),
        (
            cb(0b110, 8, 10),
            (K_BEQ, 0, 10, 0, PC.wrapping_add(8) as i32, NEXT2, 2),
        ),
        (
            cb(0b111, -8, 10),
            (K_BNE, 0, 10, 0, PC.wrapping_sub(8) as i32, NEXT2, 2),
        ),
        (
            cb(0b110, -0x100, 15),
            (K_BEQ, 0, 15, 0, PC.wrapping_sub(0x100) as i32, NEXT2, 2),
        ),
        ill2(caddi16sp(0)),
        ill2(ci(0b011, 0, 10, 0b01)),
        ill2(cbi(0b00, 32, 10)),
        ill2(cbi(0b01, 32, 10)),
        ill2(ca(1, 0b00, 10, 11)),
        ill2(ca(1, 0b01, 10, 11)),
        ill2(ca(1, 0b10, 10, 11)),
        ill2(ca(1, 0b11, 10, 11)),
    ];

    const T_C2: &[Case] = &[
        (ci(0b000, 1, 10, 0b10), (K_SLLI, 10, 10, 0, 1, 0, 2)),
        (ci(0b000, 31, 15, 0b10), (K_SLLI, 15, 15, 0, 31, 0, 2)),
        (ci(0b000, 0, 10, 0b10), (K_SLLI, 10, 10, 0, 0, 0, 2)),
        (ci(0b000, 1, 0, 0b10), (K_NOP, 0, 0, 0, 0, 0, 2)),
        (clwsp(4, 10), (K_LW, 10, 2, 0, 4, 0, 2)),
        (clwsp(252, 15), (K_LW, 15, 2, 0, 252, 0, 2)),
        (cr(0, 1, 0), (K_JALR, 0, 1, 0, 0, NEXT2, 2)),
        (cr(0, 10, 11), (K_ADD, 10, 0, 11, 0, 0, 2)),
        (cr(0, 0, 10), (K_NOP, 0, 0, 0, 0, 0, 2)),
        (cr(1, 0, 0), (K_EBREAK, 0, 0, 0, 0, 0, 2)),
        (cr(1, 10, 0), (K_JALR, 1, 10, 0, 0, NEXT2, 2)),
        (cr(1, 10, 11), (K_ADD, 10, 10, 11, 0, 0, 2)),
        (cr(1, 0, 10), (K_NOP, 0, 0, 0, 0, 0, 2)),
        (cswsp(8, 8), (K_SW, 0, 2, 8, 8, 0, 2)),
        (cswsp(252, 1), (K_SW, 0, 2, 1, 252, 0, 2)),
        (cswsp(0, 31), (K_SW, 0, 2, 31, 0, 0, 2)),
        ill2(ci(0b000, 32, 10, 0b10)),
        ill2(clwsp(4, 0)),
        ill2(cr(0, 0, 0)),
        ill2(0b001 << 13 | 0b10), // c.fldsp (D)
        ill2(0b011 << 13 | 0b10), // c.flwsp (F)
        ill2(0b101 << 13 | 0b10), // c.fsdsp (D)
        ill2(0b111 << 13 | 0b10), // c.fswsp (F)
    ];

    const T_C_SP: &[Case] = &[
        (caddi16sp(-32), (K_ADDI, 2, 2, 0, -32, 0, 2)),
        (ci(0b000, -16, 2, 0b01), (K_ADDI, 2, 2, 0, -16, 0, 2)),
        (ci(0b010, 8, 2, 0b01), (K_ADDI, 2, 0, 0, 8, 0, 2)),
        (cr(0, 2, 10), (K_ADD, 2, 0, 10, 0, 0, 2)),
        (cr(1, 2, 10), (K_ADD, 2, 2, 10, 0, 0, 2)),
        (clwsp(8, 2), (K_LW, 2, 2, 0, 8, 0, 2)),
        (ci(0b000, 1, 2, 0b10), (K_SLLI, 2, 2, 0, 1, 0, 2)),
    ];

    const ALL: &[&[Case]] = &[
        T_U,
        T_OP_IMM,
        T_OP,
        T_MEM,
        T_CTRL,
        T_SYSTEM,
        T_MISC_MEM,
        T_UNIMPLEMENTED,
        T_C0,
        T_C1,
        T_C2,
        T_C_SP,
    ];

    #[test]
    fn lui_and_auipc() {
        run(T_U);
    }

    #[test]
    fn op_imm_and_shift_immediates() {
        run(T_OP_IMM);
    }

    #[test]
    fn op_register_and_m_extension() {
        run(T_OP);
    }

    #[test]
    fn loads_and_stores() {
        run(T_MEM);
    }

    #[test]
    fn jumps_and_branches() {
        run(T_CTRL);
    }

    #[test]
    fn system_csr_and_privileged() {
        run(T_SYSTEM);
    }

    #[test]
    fn fence_is_a_nop_and_fencei_terminates() {
        run(T_MISC_MEM);
    }

    #[test]
    fn unimplemented_opcodes_are_illegal() {
        run(T_UNIMPLEMENTED);
    }

    #[test]
    fn compressed_quadrant_0() {
        run(T_C0);
    }

    #[test]
    fn compressed_quadrant_1() {
        run(T_C1);
    }

    #[test]
    fn compressed_quadrant_2() {
        run(T_C2);
    }

    #[test]
    fn compressed_forms_that_write_sp() {
        run(T_C_SP);
        for &(word, want) in T_C_SP {
            let got = decode(word, PC);
            assert_eq!(got.rd, 2, "{word:#06x}");
            assert_ne!(
                got.flags & F_WRITES_SP,
                0,
                "{word:#06x} ({}) must carry F_WRITES_SP",
                kind_name(want.0)
            );
        }
    }

    /// The encoders above are the inverse of the decoder's own field mapping, so anchor them on
    /// compressed words that appear verbatim in `riscv32-esp-elf-objdump` output.
    #[test]
    fn known_compressed_words_match_the_encoders() {
        assert_eq!(ci(0b000, -16, 2, 0b01), 0x1141); // addi sp,sp,-16
        assert_eq!(ci(0b010, 0, 10, 0b01), 0x4501); // li a0,0
        assert_eq!(ciw(16, 10), 0x0808); // addi a0,sp,16
        assert_eq!(cswsp(8, 8), 0xC422); // sw s0,8(sp)
        assert_eq!(cr(0, 1, 0), 0x8082); // jr ra
        assert_eq!(cr(1, 0, 0), 0x9002); // c.ebreak
        assert_eq!(cr(0, 10, 11), 0x852E); // mv a0,a1
    }

    #[test]
    fn every_decodable_kind_is_covered() {
        let mut seen = [false; K_COUNT as usize];
        for table in ALL {
            for &(word, _) in *table {
                let got = decode(word, PC);
                assert!(!is_synthetic(got.kind), "{word:#010x} decoded as synthetic");
                seen[got.kind as usize] = true;
            }
        }
        for kind in 0..K_COUNT {
            assert_eq!(
                seen[kind as usize],
                !is_synthetic(kind),
                "kind {} coverage",
                kind_name(kind)
            );
        }
    }

    #[test]
    fn decoding_is_deterministic_and_ignores_the_unused_half() {
        for table in ALL {
            for &(word, want) in *table {
                assert_eq!(fields(&decode(word, PC)), fields(&decode(word, PC)));
                if want.6 == 2 {
                    let noise = word | 0xFFFF_0000;
                    assert_eq!(fields(&decode(noise, PC)), want, "{word:#06x} with noise");
                }
            }
        }
    }

    #[test]
    fn insn_len_follows_the_low_two_bits() {
        for lo in 0..=u16::MAX {
            assert_eq!(insn_len(lo), if lo & 3 == 3 { 4 } else { 2 });
        }
    }

    #[test]
    fn decode_at_needs_the_whole_instruction() {
        let word = i_type(4, 1, 0b010, 3, 0b000_0011);
        let bytes = word.to_le_bytes();
        assert!(decode_at(&bytes[..0], PC).is_none());
        assert!(decode_at(&bytes[..1], PC).is_none());
        assert!(decode_at(&bytes[..2], PC).is_none());
        assert!(decode_at(&bytes[..3], PC).is_none());
        assert_eq!(
            decode_at(&bytes, PC).map(|o| fields(&o)),
            Some(fields(&decode(word, PC)))
        );
        let c = ci(0b000, -16, 2, 0b01);
        let half = c.to_le_bytes();
        assert!(decode_at(&half[..1], PC).is_none());
        assert_eq!(
            decode_at(&half[..2], PC).map(|o| fields(&o)),
            Some((K_ADDI, 2, 2, 0, -16, 0, 2))
        );
    }

    #[test]
    fn flags_always_come_from_the_kind() {
        for table in ALL {
            for &(word, _) in *table {
                let got = decode(word, PC);
                assert_eq!(got.flags, flags_for(got.kind, got.rd), "{word:#010x}");
                assert_eq!(got.flags & F_TERM != 0, is_terminator(got.kind));
                assert_eq!(got.flags & F_LOAD != 0, is_load(got.kind));
                assert_eq!(got.flags & F_STORE != 0, is_store(got.kind));
            }
        }
    }

    #[test]
    fn illegal_keeps_the_raw_word() {
        for table in ALL {
            for &(word, want) in *table {
                if want.0 != K_ILLEGAL {
                    continue;
                }
                let got = decode(word, PC);
                assert_eq!(got.imm2, word & if want.6 == 2 { 0xFFFF } else { !0 });
                assert_eq!((got.rd, got.rs1, got.rs2, got.imm), (0, 0, 0, 0));
                assert_ne!(got.flags & F_TERM, 0);
            }
        }
    }

    #[test]
    fn compressed_register_fields_are_x8_to_x15() {
        for f in 0..8 {
            assert_eq!(creg(f), f + 8);
        }
        for table in [T_C0, T_C1] {
            for &(word, want) in table {
                if want.0 == K_ILLEGAL || want.0 == K_NOP || is_jump(want.0) {
                    continue;
                }
                for r in [want.1, want.2, want.3] {
                    assert!(r == 0 || r == 2 || (8..16).contains(&r), "{word:#06x} x{r}");
                }
            }
        }
    }
}
