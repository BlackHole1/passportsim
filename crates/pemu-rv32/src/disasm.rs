//! Disassembler: one instruction becomes the text `riscv32-esp-elf-objdump -d` (binutils 2.43.1
//! of `esp-14.2.0_20251107`) prints for it, over the hart's `rv32imc_zicsr_zifencei` ISA
//! (ESP32-C3 TRM chapter 1). Traces, error reports and the decode corpus read this text.
//!
//! The alias set and operand shapes come from running that objdump over probe encodings; no
//! toolchain source was read. `tests/decode_corpus.rs` normalizes objdump's tab to one space and
//! drops its `<symbol>` and `# address` annotations.
//!
//! [`format_op`] takes the instruction bits as well as the [`Op`], because the op cannot reproduce
//! objdump's text: the decoder folds writes to x0 into [`K_NOP`], and objdump prints lenient forms
//! for codepoints the C3 rejects as [`K_ILLEGAL`]. [`op_text`] renders an op's own fields for
//! traces that hold no bytes.

use crate::decode::insn_len;
use crate::op::*;

const REG: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// FENCE set names by 4-bit field (`i`, `o`, `r`, `w` from bit 3 down); objdump prints `unknown`
/// for an empty set.
const FENCE_SET: [&str; 16] = [
    "unknown", "w", "r", "rw", "o", "ow", "or", "orw", "i", "iw", "ir", "irw", "io", "iow", "ior",
    "iorw",
];

const PMPCFG: [&str; 4] = ["pmpcfg0", "pmpcfg1", "pmpcfg2", "pmpcfg3"];

const PMPADDR: [&str; 16] = [
    "pmpaddr0",
    "pmpaddr1",
    "pmpaddr2",
    "pmpaddr3",
    "pmpaddr4",
    "pmpaddr5",
    "pmpaddr6",
    "pmpaddr7",
    "pmpaddr8",
    "pmpaddr9",
    "pmpaddr10",
    "pmpaddr11",
    "pmpaddr12",
    "pmpaddr13",
    "pmpaddr14",
    "pmpaddr15",
];

#[inline]
fn x(r: u32) -> &'static str {
    REG[(r & 31) as usize]
}

/// Name objdump prints for `csr`, or `None` when it prints `0x<hex>`. Covers the C3's machine,
/// trigger and counter CSRs; objdump names more, but only for other architectures.
fn csr_name(csr: u32) -> Option<&'static str> {
    Some(match csr {
        0x300 => "mstatus",
        0x301 => "misa",
        0x302 => "medeleg",
        0x303 => "mideleg",
        0x304 => "mie",
        0x305 => "mtvec",
        0x306 => "mcounteren",
        0x320 => "mcountinhibit",
        0x340 => "mscratch",
        0x341 => "mepc",
        0x342 => "mcause",
        0x343 => "mtval",
        0x344 => "mip",
        0x3a0..=0x3a3 => PMPCFG[(csr - 0x3a0) as usize],
        0x3b0..=0x3bf => PMPADDR[(csr - 0x3b0) as usize],
        0x7a0 => "tselect",
        0x7a1 => "tdata1",
        0x7a2 => "tdata2",
        0x7a3 => "tdata3",
        0x7a4 => "tinfo",
        0x7a5 => "tcontrol",
        0xb00 => "mcycle",
        0xb02 => "minstret",
        0xb80 => "mcycleh",
        0xb82 => "minstreth",
        0xc00 => "cycle",
        0xc01 => "time",
        0xc02 => "instret",
        0xc80 => "cycleh",
        0xc81 => "timeh",
        0xc82 => "instreth",
        0xf11 => "mvendorid",
        0xf12 => "marchid",
        0xf13 => "mimpid",
        0xf14 => "mhartid",
        _ => return None,
    })
}

fn csr_text(csr: u32) -> String {
    match csr_name(csr) {
        Some(name) => name.to_string(),
        None => format!("{csr:#x}"),
    }
}

/// objdump's text for `op` at its own `pc`. `raw` is the encoding the op was decoded from (the
/// first halfword in the low 16 bits) and the authority for everything the op does not keep.
pub fn format_op(op: &Op, pc: u32, raw: u32) -> String {
    match op.kind {
        K_HOOK => format!("hook {}", op.imm2),
        K_FALL => format!("fall {:x}", op.imm2),
        K_FETCH_FAULT => format!("fetch_fault {:#x},{:#x}", op.imm as u32, op.imm2),
        _ if op.len == 2 => render16(raw as u16, pc),
        _ => render32(raw, pc),
    }
}

/// objdump's text for `raw` at `pc`; the high half is read only for a 32-bit instruction.
pub fn format_insn(raw: u32, pc: u32) -> String {
    if insn_len(raw as u16) == 2 {
        render16(raw as u16, pc)
    } else {
        render32(raw, pc)
    }
}

/// A decoded op's own fields at `pc`, for traces that hold no instruction bytes; not objdump's
/// text. [`K_ILLEGAL`] prints as `illegal <bits> (<objdump text>)`, so a trace or trap report
/// never reads as if the encoding had executed.
pub fn op_text(op: &Op, pc: u32) -> String {
    let (k, rd, rs1, rs2) = (op.kind, x(op.rd as u32), x(op.rs1 as u32), x(op.rs2 as u32));
    let imm = op.imm;
    match k {
        K_ILLEGAL => format!("illegal {:#x} ({})", op.imm2, format_op(op, pc, op.imm2)),
        K_HOOK | K_FALL | K_FETCH_FAULT => format_op(op, pc, op.imm2),
        K_LUI | K_AUIPC => format!("{} {rd},{:#x}", kind_name(k), imm as u32),
        K_SLLI | K_SRLI | K_SRAI => format!("{} {rd},{rs1},{imm:#x}", kind_name(k)),
        _ if is_load(k) => format!("{} {rd},{imm}({rs1})", kind_name(k)),
        _ if is_store(k) => format!("{} {rs2},{imm}({rs1})", kind_name(k)),
        _ if is_branch(k) => format!("{} {rs1},{rs2},{:x}", kind_name(k), imm as u32),
        K_JAL => format!("jal {rd},{:x}", imm as u32),
        K_JALR => format!("jalr {rd},{imm}({rs1})"),
        _ if is_csr_imm(k) => format!("{} {rd},{},{imm}", kind_name(k), csr_text(op.imm2)),
        _ if is_csr(k) => format!("{} {rd},{},{rs1}", kind_name(k), csr_text(op.imm2)),
        K_NOP | K_ECALL | K_EBREAK | K_MRET | K_WFI | K_FENCEI => kind_name(k).to_string(),
        K_ADDI | K_SLTI | K_SLTIU | K_XORI | K_ORI | K_ANDI => {
            format!("{} {rd},{rs1},{imm}", kind_name(k))
        }
        _ => format!("{} {rd},{rs1},{rs2}", kind_name(k)),
    }
}

/// `.insn <len>, 0x<bits>` with at least four hex digits, as objdump pads (`0x0007`).
fn unknown(raw: u32, len: u8) -> String {
    let bits = if len == 2 { raw & 0xFFFF } else { raw };
    format!(".insn {len}, {bits:#06x}")
}

/// Lowercase hex without `0x`, as objdump prints targets.
fn target(pc: u32, off: i32) -> String {
    format!("{:x}", pc.wrapping_add(off as u32))
}

#[inline]
fn sext(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

// 32-bit instructions.

#[inline]
fn imm_i(raw: u32) -> i32 {
    (raw as i32) >> 20
}

#[inline]
fn imm_s(raw: u32) -> i32 {
    ((raw >> 20) & 0xFE0) as i32 | ((raw >> 7) & 0x1F) as i32 | (((raw as i32) >> 25) & !0xFFF)
}

#[inline]
fn imm_b(raw: u32) -> i32 {
    let v =
        ((raw >> 19) & 0x1000) | ((raw << 4) & 0x800) | ((raw >> 20) & 0x7E0) | ((raw >> 7) & 0x1E);
    sext(v, 13)
}

#[inline]
fn imm_j(raw: u32) -> i32 {
    let v =
        ((raw >> 11) & 0x100000) | (raw & 0xFF000) | ((raw >> 9) & 0x800) | ((raw >> 20) & 0x7FE);
    sext(v, 21)
}

fn render32(raw: u32, pc: u32) -> String {
    let rd = (raw >> 7) & 31;
    let rs1 = (raw >> 15) & 31;
    let rs2 = (raw >> 20) & 31;
    let f3 = (raw >> 12) & 7;
    match raw & 0x7F {
        0x37 => format!("lui {},{:#x}", x(rd), raw >> 12),
        0x17 => format!("auipc {},{:#x}", x(rd), raw >> 12),
        0x6F => {
            let t = target(pc, imm_j(raw));
            match rd {
                0 => format!("j {t}"),
                1 => format!("jal {t}"),
                _ => format!("jal {},{t}", x(rd)),
            }
        }
        0x67 if f3 == 0 => jalr32(rd, rs1, imm_i(raw)),
        0x63 => branch32(raw, f3, rs1, rs2, pc),
        0x03 => match f3 {
            0 | 1 | 2 | 4 | 5 => {
                let name = ["lb", "lh", "lw", "", "lbu", "lhu"][f3 as usize];
                format!("{name} {},{}({})", x(rd), imm_i(raw), x(rs1))
            }
            _ => unknown(raw, 4),
        },
        0x23 => match f3 {
            0..=2 => {
                let name = ["sb", "sh", "sw"][f3 as usize];
                format!("{name} {},{}({})", x(rs2), imm_s(raw), x(rs1))
            }
            _ => unknown(raw, 4),
        },
        0x13 => op_imm32(raw, f3, rd, rs1),
        0x33 => op_reg32(raw, f3, rd, rs1, rs2),
        0x0F => misc_mem32(raw, f3, rd, rs1),
        0x73 => system32(raw, f3, rd, rs1),
        _ => unknown(raw, 4),
    }
}

fn jalr32(rd: u32, rs1: u32, imm: i32) -> String {
    match (rd, imm) {
        (0, 0) if rs1 == 1 => "ret".to_string(),
        (0, 0) => format!("jr {}", x(rs1)),
        (0, _) => format!("jr {imm}({})", x(rs1)),
        (1, 0) => format!("jalr {}", x(rs1)),
        (1, _) => format!("jalr {imm}({})", x(rs1)),
        _ => format!("jalr {},{imm}({})", x(rd), x(rs1)),
    }
}

/// objdump has a zero-register alias for every signed form and none for the unsigned ones.
fn branch32(raw: u32, f3: u32, rs1: u32, rs2: u32, pc: u32) -> String {
    let t = target(pc, imm_b(raw));
    let (s1, s2) = (x(rs1), x(rs2));
    match f3 {
        0 if rs2 == 0 => format!("beqz {s1},{t}"),
        0 => format!("beq {s1},{s2},{t}"),
        1 if rs2 == 0 => format!("bnez {s1},{t}"),
        1 => format!("bne {s1},{s2},{t}"),
        4 if rs2 == 0 => format!("bltz {s1},{t}"),
        4 if rs1 == 0 => format!("bgtz {s2},{t}"),
        4 => format!("blt {s1},{s2},{t}"),
        5 if rs2 == 0 => format!("bgez {s1},{t}"),
        5 if rs1 == 0 => format!("blez {s2},{t}"),
        5 => format!("bge {s1},{s2},{t}"),
        6 => format!("bltu {s1},{s2},{t}"),
        7 => format!("bgeu {s1},{s2},{t}"),
        _ => unknown(raw, 4),
    }
}

/// objdump prints the full 6-bit shift field, so the RV32-reserved bit 5 shows as `0x20` to `0x3f`.
fn op_imm32(raw: u32, f3: u32, rd: u32, rs1: u32) -> String {
    let imm = imm_i(raw);
    let (d, s) = (x(rd), x(rs1));
    let shamt = (raw >> 20) & 0x3F;
    let top = raw >> 26;
    match f3 {
        0 if rd == 0 && rs1 == 0 && imm == 0 => "nop".to_string(),
        0 if rs1 == 0 => format!("li {d},{imm}"),
        0 if imm == 0 => format!("mv {d},{s}"),
        0 => format!("addi {d},{s},{imm}"),
        1 if top == 0 => format!("slli {d},{s},{shamt:#x}"),
        2 => format!("slti {d},{s},{imm}"),
        3 if imm == 1 => format!("seqz {d},{s}"),
        3 => format!("sltiu {d},{s},{imm}"),
        4 if imm == -1 => format!("not {d},{s}"),
        4 => format!("xori {d},{s},{imm}"),
        5 if top == 0 => format!("srli {d},{s},{shamt:#x}"),
        5 if top == 0x10 => format!("srai {d},{s},{shamt:#x}"),
        6 => format!("ori {d},{s},{imm}"),
        7 if imm == 255 => format!("zext.b {d},{s}"),
        7 => format!("andi {d},{s},{imm}"),
        _ => unknown(raw, 4),
    }
}

fn op_reg32(raw: u32, f3: u32, rd: u32, rs1: u32, rs2: u32) -> String {
    let (d, s1, s2) = (x(rd), x(rs1), x(rs2));
    match (raw >> 25, f3) {
        (0, 2) if rs2 == 0 => format!("sltz {d},{s1}"),
        (0, 2) if rs1 == 0 => format!("sgtz {d},{s2}"),
        (0, 3) if rs1 == 0 => format!("snez {d},{s2}"),
        (0x20, 0) if rs1 == 0 => format!("neg {d},{s2}"),
        (0, _) => {
            let name = ["add", "sll", "slt", "sltu", "xor", "srl", "or", "and"][f3 as usize];
            format!("{name} {d},{s1},{s2}")
        }
        (0x20, 0) => format!("sub {d},{s1},{s2}"),
        (0x20, 5) => format!("sra {d},{s1},{s2}"),
        (1, _) => {
            let name = [
                "mul", "mulh", "mulhsu", "mulhu", "div", "divu", "rem", "remu",
            ][f3 as usize];
            format!("{name} {d},{s1},{s2}")
        }
        _ => unknown(raw, 4),
    }
}

/// objdump prints `.insn` unless the reserved fields are zero, though the decoder takes every
/// funct3 000 encoding as a FENCE.
fn misc_mem32(raw: u32, f3: u32, rd: u32, rs1: u32) -> String {
    if rd != 0 || rs1 != 0 {
        return unknown(raw, 4);
    }
    let (fm, pred, succ) = (raw >> 28, (raw >> 24) & 0xF, (raw >> 20) & 0xF);
    match f3 {
        0 if fm == 8 && pred == 3 && succ == 3 => "fence.tso".to_string(),
        0 if fm == 0 && pred == 0xF && succ == 0xF => "fence".to_string(),
        0 if fm == 0 => format!(
            "fence {},{}",
            FENCE_SET[pred as usize], FENCE_SET[succ as usize]
        ),
        1 if raw >> 20 == 0 => "fence.i".to_string(),
        _ => unknown(raw, 4),
    }
}

fn system32(raw: u32, f3: u32, rd: u32, rs1: u32) -> String {
    let csr = raw >> 20;
    let (d, s) = (x(rd), x(rs1));
    let name = csr_text(csr);
    match f3 {
        0 => match raw {
            0x0000_0073 => "ecall".to_string(),
            0x0010_0073 => "ebreak".to_string(),
            0x3020_0073 => "mret".to_string(),
            0x1050_0073 => "wfi".to_string(),
            0x1020_0073 => "sret".to_string(),
            0x7b20_0073 => "dret".to_string(),
            _ => unknown(raw, 4),
        },
        1 if raw == 0xc000_1073 => "unimp".to_string(),
        1 if rd == 0 => format!("csrw {name},{s}"),
        1 => format!("csrrw {d},{name},{s}"),
        2 if rs1 == 0 => match csr {
            0xc00 => format!("rdcycle {d}"),
            0xc01 => format!("rdtime {d}"),
            0xc02 => format!("rdinstret {d}"),
            0xc80 => format!("rdcycleh {d}"),
            0xc81 => format!("rdtimeh {d}"),
            0xc82 => format!("rdinstreth {d}"),
            _ => format!("csrr {d},{name}"),
        },
        2 if rd == 0 => format!("csrs {name},{s}"),
        2 => format!("csrrs {d},{name},{s}"),
        3 if rd == 0 => format!("csrc {name},{s}"),
        3 => format!("csrrc {d},{name},{s}"),
        5 if rd == 0 => format!("csrwi {name},{rs1}"),
        5 => format!("csrrwi {d},{name},{rs1}"),
        6 if rd == 0 => format!("csrsi {name},{rs1}"),
        6 => format!("csrrsi {d},{name},{rs1}"),
        7 if rd == 0 => format!("csrci {name},{rs1}"),
        7 => format!("csrrci {d},{name},{rs1}"),
        _ => unknown(raw, 4),
    }
}

// Compressed instructions.

#[inline]
fn ciw_imm(w: u32) -> u32 {
    ((w >> 1) & 0x3C0) | ((w >> 7) & 0x30) | ((w >> 2) & 0x8) | ((w >> 4) & 0x4)
}

#[inline]
fn cl_imm(w: u32) -> u32 {
    ((w >> 7) & 0x38) | ((w >> 4) & 0x4) | ((w << 1) & 0x40)
}

#[inline]
fn ci_imm(w: u32) -> i32 {
    sext(((w >> 2) & 0x1F) | ((w >> 7) & 0x20), 6)
}

#[inline]
fn c16sp_imm(w: u32) -> i32 {
    let v = ((w >> 3) & 0x200)
        | ((w << 4) & 0x180)
        | ((w << 1) & 0x40)
        | ((w << 3) & 0x20)
        | ((w >> 2) & 0x10);
    sext(v, 10)
}

#[inline]
fn cj_imm(w: u32) -> i32 {
    let v = ((w >> 1) & 0xB00)
        | ((w << 2) & 0x400)
        | ((w << 1) & 0x80)
        | ((w >> 1) & 0x40)
        | ((w << 3) & 0x20)
        | ((w >> 7) & 0x10)
        | ((w >> 2) & 0xE);
    sext(v, 12)
}

#[inline]
fn cb_imm(w: u32) -> i32 {
    let v = ((w >> 4) & 0x100)
        | ((w << 1) & 0xC0)
        | ((w >> 7) & 0x18)
        | ((w << 3) & 0x20)
        | ((w >> 2) & 0x6);
    sext(v, 9)
}

#[inline]
fn clwsp_imm(w: u32) -> u32 {
    ((w >> 7) & 0x20) | ((w << 4) & 0xC0) | ((w >> 2) & 0x1C)
}

#[inline]
fn cswsp_imm(w: u32) -> u32 {
    ((w >> 7) & 0x3C) | ((w >> 1) & 0xC0)
}

/// Most compressed forms print as their 32-bit expansion; codepoints with no expansion alias keep
/// a `c.` name.
fn render16(raw: u16, pc: u32) -> String {
    let w = raw as u32;
    if raw == 0 {
        return "unimp".to_string();
    }
    // The 3-bit register fields name x8 to x15: bits 4:2 are `rd'` (CIW, CL) or `rs2'` (CS),
    // bits 9:7 are `rs1'`.
    let r3 = x(8 + ((w >> 2) & 7));
    let rs1_3 = x(8 + ((w >> 7) & 7));
    let f3 = w >> 13;
    match w & 3 {
        0 => match f3 {
            0 if ciw_imm(w) == 0 => unknown(w, 2),
            0 => format!("addi {r3},sp,{}", ciw_imm(w)),
            2 => format!("lw {r3},{}({rs1_3})", cl_imm(w)),
            6 => format!("sw {r3},{}({rs1_3})", cl_imm(w)),
            _ => unknown(w, 2),
        },
        1 => q1_16(w, pc),
        2 => q2_16(w),
        _ => unknown(w, 2),
    }
}

fn q1_16(w: u32, pc: u32) -> String {
    let rd = (w >> 7) & 31;
    let d = x(rd);
    let rs1_3 = x(8 + ((w >> 7) & 7));
    let imm = ci_imm(w);
    match w >> 13 {
        0 if rd == 0 && imm == 0 => "nop".to_string(),
        0 if rd == 0 => format!("c.nop {imm}"),
        0 => format!("addi {d},{d},{imm}"),
        1 => format!("jal {}", target(pc, cj_imm(w))),
        2 if rd == 0 => format!("c.li zero,{imm}"),
        2 => format!("li {d},{imm}"),
        3 if rd == 2 => format!("addi sp,sp,{}", c16sp_imm(w)),
        3 => {
            // c.lui: objdump prints the 20-bit field of the expansion, and keeps the `c.lui`
            // name for the rd 0 HINT. A zero immediate is reserved.
            let field = (sext(((w >> 2) & 0x1F) | ((w >> 7) & 0x20), 6) as u32) & 0xFFFFF;
            match () {
                () if field == 0 => unknown(w, 2),
                () if rd == 0 => format!("c.lui zero,{field:#x}"),
                () => format!("lui {d},{field:#x}"),
            }
        }
        4 => misc_alu16(w),
        5 => format!("j {}", target(pc, cj_imm(w))),
        6 => format!("beqz {rs1_3},{}", target(pc, cb_imm(w))),
        _ => format!("bnez {rs1_3},{}", target(pc, cb_imm(w))),
    }
}

/// A zero shift amount prints under objdump's RV128 `c.*64` name; bit 5 set (RV32-reserved)
/// prints leniently as `0x20` to `0x3f`.
fn misc_alu16(w: u32) -> String {
    let d = x(8 + ((w >> 7) & 7));
    let b12 = (w >> 12) & 1;
    let shamt = ((w >> 2) & 0x1F) | (b12 << 5);
    match (w >> 10) & 3 {
        sel @ (0 | 1) => {
            let name = if sel == 0 { "srli" } else { "srai" };
            if shamt == 0 {
                format!("c.{name}64 {d}")
            } else {
                format!("{name} {d},{d},{shamt:#x}")
            }
        }
        2 => format!("andi {d},{d},{}", ci_imm(w)),
        _ if b12 == 1 => unknown(w, 2),
        _ => {
            let name = ["sub", "xor", "or", "and"][((w >> 5) & 3) as usize];
            format!("{name} {d},{d},{}", x(8 + ((w >> 2) & 7)))
        }
    }
}

fn q2_16(w: u32) -> String {
    let rd = (w >> 7) & 31;
    let rs2 = (w >> 2) & 31;
    let (d, s2) = (x(rd), x(rs2));
    let b12 = (w >> 12) & 1;
    match w >> 13 {
        0 => {
            let shamt = rs2 | (b12 << 5);
            match () {
                () if shamt == 0 => format!("c.slli64 {d}"),
                () if rd == 0 => format!("c.slli zero,{shamt:#x}"),
                () => format!("slli {d},{d},{shamt:#x}"),
            }
        }
        2 if rd == 0 => unknown(w, 2),
        2 => format!("lw {d},{}(sp)", clwsp_imm(w)),
        4 => match (b12, rs2, rd) {
            (0, 0, 0) => unknown(w, 2),
            (0, 0, 1) => "ret".to_string(),
            (0, 0, _) => format!("jr {d}"),
            (0, _, 0) => format!("c.mv zero,{s2}"),
            (0, _, _) => format!("mv {d},{s2}"),
            (_, 0, 0) => "ebreak".to_string(),
            (_, 0, _) => format!("jalr {d}"),
            (_, _, 0) => format!("c.add zero,{s2}"),
            (_, _, _) => format!("add {d},{d},{s2}"),
        },
        6 => format!("sw {s2},{}(sp)", cswsp_imm(w)),
        _ => unknown(w, 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;

    /// One line of `riscv32-esp-elf-objdump -d` per row over a probe ELF of these encodings,
    /// normalized as `tests/decode_corpus.rs` does.
    const OBJDUMP: &[(u32, u8, u32, &str)] = &[
        (0x12345537, 4, 0x40000000, "lui a0,0x12345"),
        (0x00001037, 4, 0x40000004, "lui zero,0x1"),
        (0x12345517, 4, 0x4000000c, "auipc a0,0x12345"),
        (0x00000017, 4, 0x40000010, "auipc zero,0x0"),
        (0x00500513, 4, 0x40000014, "li a0,5"),
        (0x00058513, 4, 0x40000018, "mv a0,a1"),
        (0x00000013, 4, 0x4000001c, "nop"),
        (0x00508013, 4, 0x40000020, "addi zero,ra,5"),
        (0xfff58513, 4, 0x40000024, "addi a0,a1,-1"),
        (0x00000513, 4, 0x40000028, "li a0,0"),
        (0x00010113, 4, 0x4000002c, "mv sp,sp"),
        (0xfff5c513, 4, 0x40000030, "not a0,a1"),
        (0x0055c513, 4, 0x40000034, "xori a0,a1,5"),
        (0x0015b513, 4, 0x40000038, "seqz a0,a1"),
        (0x0025b513, 4, 0x4000003c, "sltiu a0,a1,2"),
        (0xfff5b513, 4, 0x40000040, "sltiu a0,a1,-1"),
        (0x0ff5f513, 4, 0x40000044, "zext.b a0,a1"),
        (0xfff5f513, 4, 0x40000048, "andi a0,a1,-1"),
        (0x0055f513, 4, 0x4000004c, "andi a0,a1,5"),
        (0x0055a513, 4, 0x40000050, "slti a0,a1,5"),
        (0x0055e513, 4, 0x40000054, "ori a0,a1,5"),
        (0x00059513, 4, 0x40000058, "slli a0,a1,0x0"),
        (0x01f59513, 4, 0x4000005c, "slli a0,a1,0x1f"),
        (0x0055d513, 4, 0x40000064, "srli a0,a1,0x5"),
        (0x4055d513, 4, 0x40000068, "srai a0,a1,0x5"),
        (0x0005d513, 4, 0x4000006c, "srli a0,a1,0x0"),
        (0x4205d513, 4, 0x40000070, "srai a0,a1,0x20"),
        (0x00c58533, 4, 0x40000074, "add a0,a1,a2"),
        (0x00b00533, 4, 0x40000078, "add a0,zero,a1"),
        (0x40b00533, 4, 0x4000007c, "neg a0,a1"),
        (0x40c58533, 4, 0x40000080, "sub a0,a1,a2"),
        (0x00c59533, 4, 0x40000084, "sll a0,a1,a2"),
        (0x0005a533, 4, 0x40000088, "sltz a0,a1"),
        (0x00b02533, 4, 0x4000008c, "sgtz a0,a1"),
        (0x00c5a533, 4, 0x40000090, "slt a0,a1,a2"),
        (0x00b03533, 4, 0x40000094, "snez a0,a1"),
        (0x00c5b533, 4, 0x40000098, "sltu a0,a1,a2"),
        (0x00c5c533, 4, 0x4000009c, "xor a0,a1,a2"),
        (0x00c5d533, 4, 0x400000a0, "srl a0,a1,a2"),
        (0x40c5d533, 4, 0x400000a4, "sra a0,a1,a2"),
        (0x00c5e533, 4, 0x400000a8, "or a0,a1,a2"),
        (0x00c5f533, 4, 0x400000ac, "and a0,a1,a2"),
        (0x02c58533, 4, 0x400000b4, "mul a0,a1,a2"),
        (0x02c59533, 4, 0x400000b8, "mulh a0,a1,a2"),
        (0x02c5a533, 4, 0x400000bc, "mulhsu a0,a1,a2"),
        (0x02c5b533, 4, 0x400000c0, "mulhu a0,a1,a2"),
        (0x02c5c533, 4, 0x400000c4, "div a0,a1,a2"),
        (0x02c5d533, 4, 0x400000c8, "divu a0,a1,a2"),
        (0x02c5e533, 4, 0x400000cc, "rem a0,a1,a2"),
        (0x02c5f533, 4, 0x400000d0, "remu a0,a1,a2"),
        (0xff858503, 4, 0x400000d4, "lb a0,-8(a1)"),
        (0x00058003, 4, 0x400000d8, "lb zero,0(a1)"),
        (0xff859503, 4, 0x400000dc, "lh a0,-8(a1)"),
        (0x00059003, 4, 0x400000e0, "lh zero,0(a1)"),
        (0xff85a503, 4, 0x400000e4, "lw a0,-8(a1)"),
        (0x0005a003, 4, 0x400000e8, "lw zero,0(a1)"),
        (0xff85c503, 4, 0x400000ec, "lbu a0,-8(a1)"),
        (0x0005c003, 4, 0x400000f0, "lbu zero,0(a1)"),
        (0xff85d503, 4, 0x400000f4, "lhu a0,-8(a1)"),
        (0x0005d003, 4, 0x400000f8, "lhu zero,0(a1)"),
        (0xfec58c23, 4, 0x400000fc, "sb a2,-8(a1)"),
        (0xfec59c23, 4, 0x40000100, "sh a2,-8(a1)"),
        (0xfec5ac23, 4, 0x40000104, "sw a2,-8(a1)"),
        (0x0080006f, 4, 0x40000108, "j 40000110"),
        (0x008000ef, 4, 0x4000010c, "jal 40000114"),
        (0xff9ff2ef, 4, 0x40000110, "jal t0,40000108"),
        (0x00028067, 4, 0x40000114, "jr t0"),
        (0x00428067, 4, 0x40000118, "jr 4(t0)"),
        (0x00008067, 4, 0x4000011c, "ret"),
        (0x000280e7, 4, 0x40000120, "jalr t0"),
        (0xd06080e7, 4, 0x40000124, "jalr -762(ra)"),
        (0x008302e7, 4, 0x40000128, "jalr t0,8(t1)"),
        (0x00c58463, 4, 0x4000012c, "beq a1,a2,40000134"),
        (0x00058463, 4, 0x40000130, "beqz a1,40000138"),
        (0xfeb00ce3, 4, 0x40000134, "beq zero,a1,4000012c"),
        (0x00c59463, 4, 0x40000138, "bne a1,a2,40000140"),
        (0x00059463, 4, 0x4000013c, "bnez a1,40000144"),
        (0xfeb01ce3, 4, 0x40000140, "bne zero,a1,40000138"),
        (0x00c5c463, 4, 0x40000144, "blt a1,a2,4000014c"),
        (0x0005c463, 4, 0x40000148, "bltz a1,40000150"),
        (0xfeb04ce3, 4, 0x4000014c, "bgtz a1,40000144"),
        (0x00c5d463, 4, 0x40000150, "bge a1,a2,40000158"),
        (0x0005d463, 4, 0x40000154, "bgez a1,4000015c"),
        (0xfeb05ce3, 4, 0x40000158, "blez a1,40000150"),
        (0x00c5e463, 4, 0x4000015c, "bltu a1,a2,40000164"),
        (0x0005e463, 4, 0x40000160, "bltu a1,zero,40000168"),
        (0x00c5f463, 4, 0x40000168, "bgeu a1,a2,40000170"),
        (0x0005f463, 4, 0x4000016c, "bgeu a1,zero,40000174"),
        (0x0ff0000f, 4, 0x40000174, "fence"),
        (0x0330000f, 4, 0x40000178, "fence rw,rw"),
        (0x8330000f, 4, 0x4000017c, "fence.tso"),
        (0x0000100f, 4, 0x40000180, "fence.i"),
        (0x0000000f, 4, 0x40000184, "fence unknown,unknown"),
        (0x00000073, 4, 0x40000188, "ecall"),
        (0x00100073, 4, 0x4000018c, "ebreak"),
        (0x30200073, 4, 0x40000190, "mret"),
        (0x10500073, 4, 0x40000194, "wfi"),
        (0x10200073, 4, 0x40000198, "sret"),
        (0x30059573, 4, 0x4000019c, "csrrw a0,mstatus,a1"),
        (0x30059073, 4, 0x400001a0, "csrw mstatus,a1"),
        (0x30002573, 4, 0x400001a4, "csrr a0,mstatus"),
        (0x3005a073, 4, 0x400001a8, "csrs mstatus,a1"),
        (0x3005b073, 4, 0x400001ac, "csrc mstatus,a1"),
        (0x3005b573, 4, 0x400001b0, "csrrc a0,mstatus,a1"),
        (0x30045073, 4, 0x400001b4, "csrwi mstatus,8"),
        (0x30045573, 4, 0x400001b8, "csrrwi a0,mstatus,8"),
        (0x30046073, 4, 0x400001bc, "csrsi mstatus,8"),
        (0x30046573, 4, 0x400001c0, "csrrsi a0,mstatus,8"),
        (0x30047073, 4, 0x400001c4, "csrci mstatus,8"),
        (0x30047573, 4, 0x400001c8, "csrrci a0,mstatus,8"),
        (0x30001073, 4, 0x400001cc, "csrw mstatus,zero"),
        (0x30002073, 4, 0x400001d0, "csrr zero,mstatus"),
        (0xc0002573, 4, 0x4000024c, "rdcycle a0"),
        (0xc0001073, 4, 0x40000274, "unimp"),
        (0xc0002073, 4, 0x40000278, "rdcycle zero"),
        (0xc0102573, 4, 0x40000284, "rdtime a0"),
        (0xc0102073, 4, 0x400002b0, "rdtime zero"),
        (0xc0202573, 4, 0x400002bc, "rdinstret a0"),
        (0xc0202073, 4, 0x400002e8, "rdinstret zero"),
        (0xffffffff, 4, 0x4000035c, ".insn 4, 0xffffffff"),
        (0x0000202f, 4, 0x40000360, ".insn 4, 0x202f"),
        (0x0000, 2, 0x40000000, "unimp"),
        (0xc1c8, 2, 0x40000014, "sw a0,4(a1)"),
        (0x0001, 2, 0x40000018, "nop"),
        (0x0005, 2, 0x4000001a, "c.nop 1"),
        (0x0021, 2, 0x4000001c, "c.nop 8"),
        (0x2001, 2, 0x40000024, "jal 40000024"),
        (0x4005, 2, 0x4000002a, "c.li zero,1"),
        (0x6005, 2, 0x4000003a, "c.lui zero,0x1"),
        (0x8101, 2, 0x4000003e, "c.srli64 a0"),
        (0x8d0d, 2, 0x4000004e, "sub a0,a0,a1"),
        (0x8d2d, 2, 0x40000050, "xor a0,a0,a1"),
        (0x8d4d, 2, 0x40000052, "or a0,a0,a1"),
        (0x8d6d, 2, 0x40000054, "and a0,a0,a1"),
        (0xa001, 2, 0x4000005e, "j 4000005e"),
        (0xc105, 2, 0x40000062, "beqz a0,40000082"),
        (0xe105, 2, 0x40000064, "bnez a0,40000084"),
        (0x0502, 2, 0x40000068, "c.slli64 a0"),
        (0x0002, 2, 0x4000006a, "c.slli64 zero"),
        (0x0006, 2, 0x4000006c, "c.slli zero,0x1"),
        (0x8082, 2, 0x40000076, "ret"),
        (0x8282, 2, 0x40000078, "jr t0"),
        (0x802e, 2, 0x4000007e, "c.mv zero,a1"),
        (0x9002, 2, 0x40000080, "ebreak"),
        (0x9282, 2, 0x40000082, "jalr t0"),
        (0x902e, 2, 0x40000088, "c.add zero,a1"),
    ];

    #[test]
    fn objdump_text_matches_every_shape() {
        for &(bits, len, pc, want) in OBJDUMP {
            assert_eq!(format_insn(bits, pc), want, "bits {bits:#x} at {pc:#x}");
            let op = decode(bits, pc);
            assert_eq!(op.len, len, "length of {bits:#x}");
            assert_eq!(format_op(&op, pc, bits), want, "op of {bits:#x}");
        }
    }

    #[test]
    fn synthetic_kinds_print_their_own_names() {
        let hook = Op {
            kind: K_HOOK,
            imm2: 7,
            len: 0,
            ..decode(0x0000_0013, 0)
        };
        assert_eq!(format_op(&hook, 0x4000_0000, 0), "hook 7");
        let fall = Op {
            kind: K_FALL,
            imm2: 0x4000_1234,
            len: 0,
            ..decode(0x0000_0013, 0)
        };
        assert_eq!(format_op(&fall, 0x4000_0000, 0), "fall 40001234");
        let fault = Op {
            kind: K_FETCH_FAULT,
            imm: 1,
            imm2: 0x4000_1000,
            len: 0,
            ..decode(0x0000_0013, 0)
        };
        assert_eq!(format_op(&fault, 0, 0), "fetch_fault 0x1,0x40001000");
    }

    #[test]
    fn op_text_renders_the_op_fields() {
        // `addi a0,a1,-4`, `lw a0,8(a1)`, `beq a0,a1,+8` and `csrrw a0,mstatus,a1`.
        let pc = 0x4000_0000;
        assert_eq!(op_text(&decode(0xffc5_8513, pc), pc), "addi a0,a1,-4");
        assert_eq!(op_text(&decode(0x0085_a503, pc), pc), "lw a0,8(a1)");
        assert_eq!(op_text(&decode(0x00b5_0463, pc), pc), "beq a0,a1,40000008");
        assert_eq!(op_text(&decode(0x3005_9573, pc), pc), "csrrw a0,mstatus,a1");
        // A compressed instruction prints as its expansion, and a folded write to x0 as `nop`.
        assert_eq!(op_text(&decode(0x0000_852e, pc), pc), "add a0,zero,a1");
        assert_eq!(op_text(&decode(0x0000_0013, pc), pc), "nop");
        assert_eq!(
            op_text(&decode(0xffff_ffff, pc), pc),
            "illegal 0xffffffff (.insn 4, 0xffffffff)"
        );
        // Codepoints objdump renders leniently.
        assert_eq!(
            op_text(&decode(0x1020_0073, pc), pc),
            "illegal 0x10200073 (sret)"
        );
        assert_eq!(
            op_text(&decode(0x7b20_0073, pc), pc),
            "illegal 0x7b200073 (dret)"
        );
        assert_eq!(
            op_text(&decode(0x0000_6101, pc), pc),
            "illegal 0x6101 (addi sp,sp,0)"
        );
        assert_eq!(
            op_text(&decode(0x0000_18b2, pc), pc),
            "illegal 0x18b2 (slli a7,a7,0x2c)"
        );
        assert_eq!(
            op_text(&decode(0x4205_d513, pc), pc),
            "illegal 0x4205d513 (srai a0,a1,0x20)"
        );
    }
}
