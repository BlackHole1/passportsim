//! The predecoded instruction, its op kinds and flag bits (RISC-V unprivileged spec, ESP32-C3 TRM
//! chapter 1). `Op` and `F_*` are frozen; the `K_*` kinds are dense from 0 and append-only.
//!
//! Operand convention: registers are read as `x[(r & 31) as usize]`; unused fields are zero, so
//! decodes are bit-identical; targets, link values and the `auipc` result are absolute in `imm` or
//! `imm2`, so ops never need the PC; `len` is 4, 2 or 0 (synthetic) and the op PC is the block PC
//! plus `pc_off`. An op whose only effect is an x0 write is [`K_NOP`] (loads, jumps and CSR ops
//! keep their kind), compressed forms take their expansion's kind, and every encoding outside
//! RV32IMC, Zicsr, Zifencei, `mret` and `wfi`, reserved ones included, is [`K_ILLEGAL`].

/// 16-byte predecoded instruction.
#[repr(C)]
pub struct Op {
    pub kind: u8,
    pub rd: u8,
    pub rs1: u8,
    pub rs2: u8,
    pub imm: i32,
    pub imm2: u32,
    pub len: u8,
    pub flags: u8,
    /// Offset of this instruction from the block start PC.
    pub pc_off: u16,
}

/// rd is x2: run `SpMonitor::check` after the write (only while a monitor bit is on).
pub const F_WRITES_SP: u8 = 1;
/// Ends the block; set exactly for [`is_terminator`] kinds.
pub const F_TERM: u8 = 2;
/// Memory load (poll tracker, watchpoints).
pub const F_LOAD: u8 = 4;
/// Memory store (bumps `Hart::stores`).
pub const F_STORE: u8 = 8;

// Op kinds. Dense from 0; append only.

/// FENCE, `c.nop`, and any op whose only effect is a write to x0.
pub const K_NOP: u8 = 0;
/// `lui`, `c.lui`: `imm` is the U immediate already shifted left by 12.
pub const K_LUI: u8 = 1;
/// `auipc`: `imm` is the absolute result.
pub const K_AUIPC: u8 = 2;
/// `addi`, `c.addi`, `c.addi4spn`, `c.addi16sp`, `c.li`.
pub const K_ADDI: u8 = 3;
pub const K_SLTI: u8 = 4;
pub const K_SLTIU: u8 = 5;
pub const K_XORI: u8 = 6;
pub const K_ORI: u8 = 7;
pub const K_ANDI: u8 = 8;
pub const K_SLLI: u8 = 9;
pub const K_SRLI: u8 = 10;
pub const K_SRAI: u8 = 11;
/// `add`, `c.add`, `c.mv` (`rs1` 0).
pub const K_ADD: u8 = 12;
pub const K_SUB: u8 = 13;
pub const K_SLL: u8 = 14;
pub const K_SLT: u8 = 15;
pub const K_SLTU: u8 = 16;
pub const K_XOR: u8 = 17;
pub const K_SRL: u8 = 18;
pub const K_SRA: u8 = 19;
pub const K_OR: u8 = 20;
pub const K_AND: u8 = 21;
pub const K_MUL: u8 = 22;
pub const K_MULH: u8 = 23;
pub const K_MULHSU: u8 = 24;
pub const K_MULHU: u8 = 25;
/// Division by zero and overflow never trap, here and for `divu`, `rem` and `remu`.
pub const K_DIV: u8 = 26;
pub const K_DIVU: u8 = 27;
pub const K_REM: u8 = 28;
pub const K_REMU: u8 = 29;
pub const K_LB: u8 = 30;
pub const K_LH: u8 = 31;
/// `lw`, `c.lw`, `c.lwsp` (`rs1` 2).
pub const K_LW: u8 = 32;
pub const K_LBU: u8 = 33;
pub const K_LHU: u8 = 34;
pub const K_SB: u8 = 35;
pub const K_SH: u8 = 36;
/// `sw`, `c.sw`, `c.swsp` (`rs1` 2).
pub const K_SW: u8 = 37;
/// `jal`, `c.j`, `c.jal`: `imm` is the target, `imm2` the link value.
pub const K_JAL: u8 = 38;
/// `jalr`, `c.jr`, `c.jalr`: the target is read before the link write, since `rd` may be `rs1`.
pub const K_JALR: u8 = 39;
/// For every branch `imm` is the taken target and `imm2` the fall-through PC.
pub const K_BEQ: u8 = 40;
/// `bne`, `c.bnez` (`rs2` 0).
pub const K_BNE: u8 = 41;
pub const K_BLT: u8 = 42;
pub const K_BGE: u8 = 43;
pub const K_BLTU: u8 = 44;
pub const K_BGEU: u8 = 45;
/// `csrrw`: with `rd` 0 the CSR is not read. For every CSR kind `imm2` is the CSR address.
pub const K_CSRRW: u8 = 46;
/// `csrrs`: with `rs1` 0 the CSR is not written.
pub const K_CSRRS: u8 = 47;
/// `csrrc`: with `rs1` 0 the CSR is not written.
pub const K_CSRRC: u8 = 48;
/// `csrrwi`: `imm` is the zero-extended 5-bit immediate and `rs1` is 0.
pub const K_CSRRWI: u8 = 49;
/// `csrrsi`: no write when `imm` is 0.
pub const K_CSRRSI: u8 = 50;
/// `csrrci`: no write when `imm` is 0.
pub const K_CSRRCI: u8 = 51;
pub const K_ECALL: u8 = 52;
pub const K_EBREAK: u8 = 53;
pub const K_MRET: u8 = 54;
/// `wfi`: `imm2` is the PC where execution resumes.
pub const K_WFI: u8 = 55;
/// `fence.i` (MISC-MEM funct3 001): flushes the translation cache; `imm2` is the next PC.
pub const K_FENCEI: u8 = 56;
/// Raises illegal instruction with `mtval` = `imm2`, the zero-extended instruction bits.
pub const K_ILLEGAL: u8 = 57;
/// Synthetic terminator at a hooked PC: exits with the hook without executing the instruction,
/// which a continuing hook runs once through `Engine::continue_past_hook` (UNVERIFIED). `imm2` is
/// the `HookId` value; `pc_off` is the hooked instruction's offset.
pub const K_HOOK: u8 = 58;
/// Synthetic block end at `max_block_insns` or a page boundary; continues at `imm2`.
pub const K_FALL: u8 = 59;
/// Synthetic terminator for a fetch fault inside a translation: `imm` is the cause, `imm2` the
/// tval, and `pc_off` the faulting instruction (so `mepc` is not the block start).
pub const K_FETCH_FAULT: u8 = 60;
pub const K_COUNT: u8 = 61;

/// Name of each kind, for traces and error reports.
pub const KIND_NAMES: [&str; K_COUNT as usize] = [
    "nop",
    "lui",
    "auipc",
    "addi",
    "slti",
    "sltiu",
    "xori",
    "ori",
    "andi",
    "slli",
    "srli",
    "srai",
    "add",
    "sub",
    "sll",
    "slt",
    "sltu",
    "xor",
    "srl",
    "sra",
    "or",
    "and",
    "mul",
    "mulh",
    "mulhsu",
    "mulhu",
    "div",
    "divu",
    "rem",
    "remu",
    "lb",
    "lh",
    "lw",
    "lbu",
    "lhu",
    "sb",
    "sh",
    "sw",
    "jal",
    "jalr",
    "beq",
    "bne",
    "blt",
    "bge",
    "bltu",
    "bgeu",
    "csrrw",
    "csrrs",
    "csrrc",
    "csrrwi",
    "csrrsi",
    "csrrci",
    "ecall",
    "ebreak",
    "mret",
    "wfi",
    "fence.i",
    "illegal",
    "hook",
    "fall",
    "fetch_fault",
];

/// Name of `kind`, or `"?"` for a value at or above [`K_COUNT`].
pub const fn kind_name(kind: u8) -> &'static str {
    if kind < K_COUNT {
        KIND_NAMES[kind as usize]
    } else {
        "?"
    }
}

/// Kinds that end a block and carry [`F_TERM`]; `fence.i` flushes the cache, [`K_ILLEGAL`] traps.
pub const fn is_terminator(kind: u8) -> bool {
    is_jump(kind)
        || is_branch(kind)
        || is_csr(kind)
        || is_synthetic(kind)
        || matches!(
            kind,
            K_ECALL | K_EBREAK | K_MRET | K_WFI | K_FENCEI | K_ILLEGAL
        )
}

pub const fn is_csr(kind: u8) -> bool {
    matches!(
        kind,
        K_CSRRW | K_CSRRS | K_CSRRC | K_CSRRWI | K_CSRRSI | K_CSRRCI
    )
}

/// The Zicsr immediate forms, whose source value is `imm` rather than `x[rs1]`.
pub const fn is_csr_imm(kind: u8) -> bool {
    matches!(kind, K_CSRRWI | K_CSRRSI | K_CSRRCI)
}

pub const fn is_branch(kind: u8) -> bool {
    matches!(kind, K_BEQ | K_BNE | K_BLT | K_BGE | K_BLTU | K_BGEU)
}

pub const fn is_jump(kind: u8) -> bool {
    matches!(kind, K_JAL | K_JALR)
}

pub const fn is_load(kind: u8) -> bool {
    matches!(kind, K_LB | K_LH | K_LW | K_LBU | K_LHU)
}

pub const fn is_store(kind: u8) -> bool {
    matches!(kind, K_SB | K_SH | K_SW)
}

/// Kinds not decoded from guest bytes (`len` 0, no instruction retired).
pub const fn is_synthetic(kind: u8) -> bool {
    matches!(kind, K_HOOK | K_FALL | K_FETCH_FAULT)
}

pub const fn writes_rd(kind: u8) -> bool {
    matches!(kind, K_LUI..=K_REMU) || is_load(kind) || is_jump(kind) || is_csr(kind)
}

/// The `F_*` flag byte for an op of `kind` with destination `rd`; the only place flags are derived.
pub const fn flags_for(kind: u8, rd: u8) -> u8 {
    let mut f = 0;
    if writes_rd(kind) && rd & 31 == 2 {
        f |= F_WRITES_SP;
    }
    if is_terminator(kind) {
        f |= F_TERM;
    }
    if is_load(kind) {
        f |= F_LOAD;
    }
    if is_store(kind) {
        f |= F_STORE;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind with its name and classes, in kind order. Class letters: T terminator, C CSR,
    /// I CSR immediate form, B branch, J jump, L load, S store, Y synthetic, W writes rd.
    const TABLE: &[(u8, &str, &str)] = &[
        (K_NOP, "nop", ""),
        (K_LUI, "lui", "W"),
        (K_AUIPC, "auipc", "W"),
        (K_ADDI, "addi", "W"),
        (K_SLTI, "slti", "W"),
        (K_SLTIU, "sltiu", "W"),
        (K_XORI, "xori", "W"),
        (K_ORI, "ori", "W"),
        (K_ANDI, "andi", "W"),
        (K_SLLI, "slli", "W"),
        (K_SRLI, "srli", "W"),
        (K_SRAI, "srai", "W"),
        (K_ADD, "add", "W"),
        (K_SUB, "sub", "W"),
        (K_SLL, "sll", "W"),
        (K_SLT, "slt", "W"),
        (K_SLTU, "sltu", "W"),
        (K_XOR, "xor", "W"),
        (K_SRL, "srl", "W"),
        (K_SRA, "sra", "W"),
        (K_OR, "or", "W"),
        (K_AND, "and", "W"),
        (K_MUL, "mul", "W"),
        (K_MULH, "mulh", "W"),
        (K_MULHSU, "mulhsu", "W"),
        (K_MULHU, "mulhu", "W"),
        (K_DIV, "div", "W"),
        (K_DIVU, "divu", "W"),
        (K_REM, "rem", "W"),
        (K_REMU, "remu", "W"),
        (K_LB, "lb", "LW"),
        (K_LH, "lh", "LW"),
        (K_LW, "lw", "LW"),
        (K_LBU, "lbu", "LW"),
        (K_LHU, "lhu", "LW"),
        (K_SB, "sb", "S"),
        (K_SH, "sh", "S"),
        (K_SW, "sw", "S"),
        (K_JAL, "jal", "TJW"),
        (K_JALR, "jalr", "TJW"),
        (K_BEQ, "beq", "TB"),
        (K_BNE, "bne", "TB"),
        (K_BLT, "blt", "TB"),
        (K_BGE, "bge", "TB"),
        (K_BLTU, "bltu", "TB"),
        (K_BGEU, "bgeu", "TB"),
        (K_CSRRW, "csrrw", "TCW"),
        (K_CSRRS, "csrrs", "TCW"),
        (K_CSRRC, "csrrc", "TCW"),
        (K_CSRRWI, "csrrwi", "TCIW"),
        (K_CSRRSI, "csrrsi", "TCIW"),
        (K_CSRRCI, "csrrci", "TCIW"),
        (K_ECALL, "ecall", "T"),
        (K_EBREAK, "ebreak", "T"),
        (K_MRET, "mret", "T"),
        (K_WFI, "wfi", "T"),
        (K_FENCEI, "fence.i", "T"),
        (K_ILLEGAL, "illegal", "T"),
        (K_HOOK, "hook", "TY"),
        (K_FALL, "fall", "TY"),
        (K_FETCH_FAULT, "fetch_fault", "TY"),
    ];

    #[test]
    fn op_is_16_bytes() {
        assert_eq!(core::mem::size_of::<Op>(), 16);
    }

    #[test]
    fn flags_are_distinct_bits() {
        let all = [F_WRITES_SP, F_TERM, F_LOAD, F_STORE];
        let mut seen = 0u8;
        for f in all {
            assert_eq!(f.count_ones(), 1);
            assert_eq!(seen & f, 0);
            seen |= f;
        }
    }

    #[test]
    fn kinds_are_dense_and_unique() {
        assert_eq!(KIND_NAMES.len(), K_COUNT as usize);
        assert_eq!(TABLE.len(), K_COUNT as usize);
        for (i, &(kind, name, _)) in TABLE.iter().enumerate() {
            assert_eq!(
                kind as usize, i,
                "kind {name} is out of order or duplicated"
            );
            assert_eq!(KIND_NAMES[i], name);
            assert_eq!(kind_name(kind), name);
            assert!(!name.is_empty());
            assert_eq!(
                KIND_NAMES.iter().filter(|n| **n == name).count(),
                1,
                "{name}"
            );
        }
        for kind in K_COUNT..=u8::MAX {
            assert_eq!(kind_name(kind), "?");
        }
    }

    #[test]
    fn classifiers_agree_with_table() {
        type Classifier = (char, fn(u8) -> bool);
        let classifiers: [Classifier; 9] = [
            ('T', is_terminator),
            ('C', is_csr),
            ('I', is_csr_imm),
            ('B', is_branch),
            ('J', is_jump),
            ('L', is_load),
            ('S', is_store),
            ('Y', is_synthetic),
            ('W', writes_rd),
        ];
        for &(kind, name, classes) in TABLE {
            for (letter, f) in classifiers {
                assert_eq!(f(kind), classes.contains(letter), "{name}: class {letter}");
            }
        }
        for kind in K_COUNT..=u8::MAX {
            for (letter, f) in classifiers {
                assert!(!f(kind), "kind {kind}: class {letter}");
            }
        }
    }

    #[test]
    fn arch_terminator_classes_carry_f_term() {
        let arch = [
            K_JAL, K_JALR, K_BEQ, K_BNE, K_BLT, K_BGE, K_BLTU, K_BGEU, K_CSRRW, K_CSRRS, K_CSRRC,
            K_CSRRWI, K_CSRRSI, K_CSRRCI, K_ECALL, K_EBREAK, K_MRET, K_WFI, K_HOOK,
        ];
        for kind in arch {
            assert_ne!(flags_for(kind, 0) & F_TERM, 0, "{}", kind_name(kind));
        }
    }

    #[test]
    fn flags_follow_the_rules() {
        for &(kind, name, classes) in TABLE {
            for rd in 0..32u8 {
                let f = flags_for(kind, rd);
                let sp = classes.contains('W') && rd == 2;
                assert_eq!(f & F_WRITES_SP != 0, sp, "{name} rd {rd}");
                assert_eq!(f & F_TERM != 0, classes.contains('T'), "{name}");
                assert_eq!(f & F_LOAD != 0, classes.contains('L'), "{name}");
                assert_eq!(f & F_STORE != 0, classes.contains('S'), "{name}");
            }
        }
        for kind in [K_ADDI, K_ADD, K_LW] {
            assert_eq!(flags_for(kind, 2), F_WRITES_SP | flags_for(kind, 1));
        }
        // Stores and branches never write sp, whatever sits in the rd field.
        assert_eq!(flags_for(K_SW, 2), F_STORE);
        assert_eq!(flags_for(K_BEQ, 2), F_TERM);
    }
}
