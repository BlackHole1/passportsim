//! Every instruction in the executable sections of the ESP32-C3 ROM ELFs and the corpus firmware
//! ELFs must decode and disassemble to what `riscv32-esp-elf-objdump -d` prints: 0 mismatches.
//! Each line is checked twice: the [`format_op`] text against objdump's, and the decoded op against
//! [`expect_op`]'s parse of objdump's text, since [`format_op`] renders from the raw bits and alone
//! would not test the op fields. Data lines are skipped; `.insn` and a 2-byte `addi sp,sp,0` are
//! ambiguous and compared on their kind set only. `t0_*` needs only the bundled ROM ELFs; `t1_*`
//! adds corpus ELFs under `$PASSPORTSIM_DATA_ROOT`. objdump is `$PASSPORTSIM_OBJDUMP`, else the
//! toolchain under `$HOME/.espressif`, else `riscv32-esp-elf-objdump` on `PATH`.

// The crate `clippy.toml` bans these APIs for the library only.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pemu_rv32::decode::decode_at;
use pemu_rv32::disasm::format_op;
use pemu_rv32::op::*;

struct Target {
    id: &'static str,
    path: PathBuf,
    /// Floor on objdump's instruction lines, so a toolchain or listing change fails instead of
    /// passing with nothing compared. Set below the true counts to allow a rebuild of the corpus.
    min_lines: usize,
}

struct Counts {
    id: &'static str,
    compared: usize,
    mismatches: usize,
    /// Compared on their kind set only.
    lenient: usize,
    data: usize,
    details: Vec<String>,
}

const MAX_DETAILS: usize = 10;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The rule of `pemu_testkit::corpus` (absolute `PASSPORTSIM_DATA_ROOT` only), copied because this
/// crate may not depend on `pemu-testkit`.
fn data_root() -> Result<PathBuf, &'static str> {
    let Some(dir) = std::env::var_os("PASSPORTSIM_DATA_ROOT")
        .filter(|d| !d.to_string_lossy().trim().is_empty())
    else {
        return Err("no data root: set PASSPORTSIM_DATA_ROOT to an absolute path");
    };
    let root = PathBuf::from(dir);
    if !root.is_absolute() {
        return Err("PASSPORTSIM_DATA_ROOT is not an absolute path");
    }
    Ok(root)
}

fn objdump() -> PathBuf {
    if let Some(bin) = std::env::var_os("PASSPORTSIM_OBJDUMP") {
        return PathBuf::from(bin);
    }
    let bundled = home().map(|h| {
        h.join(
            ".espressif/tools/riscv32-esp-elf/esp-14.2.0_20251107/riscv32-esp-elf/bin/\
             riscv32-esp-elf-objdump",
        )
    });
    match bundled {
        Some(path) if path.is_file() => path,
        _ => PathBuf::from("riscv32-esp-elf-objdump"),
    }
}

fn assets_rom() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/rom")
}

/// `rom0` is not bundled: it comes from the IDF ROM ELF package or the corpus.
fn rom_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let rom0 = [
        home().map(|h| h.join(".espressif/tools/esp-rom-elfs/20241011/esp32c3_rev0_rom.elf")),
        data_root()
            .ok()
            .map(|r| r.join("corpus/rom0/esp32c3_rev0_rom.elf")),
    ];
    if let Some(path) = rom0.into_iter().flatten().find(|p| p.is_file()) {
        targets.push(Target {
            id: "rom0",
            path,
            min_lines: 100_000,
        });
    }
    targets.push(Target {
        id: "rom3",
        path: assets_rom().join("esp32c3_rev3_rom.elf"),
        min_lines: 100_000,
    });
    targets.push(Target {
        id: "rom101",
        path: assets_rom().join("esp32c3_rev101_rom.elf"),
        min_lines: 100_000,
    });
    targets
}

fn corpus_targets() -> Vec<Target> {
    let root = match data_root() {
        Ok(root) => root,
        Err(why) => {
            eprintln!("skip: {why}");
            return Vec::new();
        }
    };
    let files: [(&'static str, &str, usize); 5] = [
        ("pk.elf", "pk/FoloToy-AI-Passport.elf", 200_000),
        ("pk.boot.elf", "pk/bootloader.elf", 5_000),
        ("official.elf", "official/FoloToy-AI-Passport.elf", 200_000),
        ("official.boot.elf", "official/bootloader.elf", 5_000),
        ("probe2.elf", "probe2/radio_heapprobe.elf", 200_000),
    ];
    files
        .into_iter()
        .map(|(id, rel, min_lines)| Target {
            id,
            path: root.join("corpus").join(rel),
            min_lines,
        })
        .collect()
}

/// A hand-rolled ELF32 section reader: the layering policy allows `pemu-rv32` no ELF crate.
struct Section {
    name: String,
    addr: u32,
    data: (usize, usize),
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn sections(elf: &[u8]) -> Result<Vec<Section>, String> {
    if elf.get(..4) != Some(b"\x7fELF") || elf.get(4) != Some(&1) || elf.get(5) != Some(&1) {
        return Err("not a little-endian ELF32 file".to_string());
    }
    let bad = || "truncated section table".to_string();
    let shoff = u32_at(elf, 0x20).ok_or_else(bad)? as usize;
    let shentsize = u16_at(elf, 0x2E).ok_or_else(bad)? as usize;
    let shnum = u16_at(elf, 0x30).ok_or_else(bad)? as usize;
    let shstrndx = u16_at(elf, 0x32).ok_or_else(bad)? as usize;
    let header = |i: usize| -> Result<(u32, u32, u32, usize, usize), String> {
        let at = shoff + i * shentsize;
        let name = u32_at(elf, at).ok_or_else(bad)?;
        let kind = u32_at(elf, at + 4).ok_or_else(bad)?;
        let addr = u32_at(elf, at + 12).ok_or_else(bad)?;
        let off = u32_at(elf, at + 16).ok_or_else(bad)? as usize;
        let size = u32_at(elf, at + 20).ok_or_else(bad)? as usize;
        Ok((name, kind, addr, off, size))
    };
    let (_, _, _, stroff, strsize) = header(shstrndx)?;
    let strtab = elf.get(stroff..stroff + strsize).ok_or_else(bad)?;
    let mut out = Vec::new();
    for i in 0..shnum {
        let (name, kind, addr, off, size) = header(i)?;
        if kind == 8 || size == 0 {
            continue; // SHT_NOBITS holds no bytes in the file
        }
        let start = name as usize;
        let end = start + strtab[start..].iter().position(|b| *b == 0).unwrap_or(0);
        let name = String::from_utf8_lossy(&strtab[start..end]).into_owned();
        if elf.len() < off + size {
            return Err(format!("section {name} runs past the end of the file"));
        }
        out.push(Section {
            name,
            addr,
            data: (off, off + size),
        });
    }
    Ok(out)
}

struct Line {
    addr: u32,
    bits: u32,
    len: usize,
    mnemonic: String,
    /// Without objdump's trailing ` # <address>` and ` <symbol+offset>` annotations.
    operands: String,
}

impl Line {
    fn want(&self) -> String {
        if self.operands.is_empty() {
            self.mnemonic.clone()
        } else {
            format!("{} {}", self.mnemonic, self.operands)
        }
    }

    fn ops(&self) -> Vec<&str> {
        if self.operands.is_empty() {
            Vec::new()
        } else {
            self.operands.split(',').map(str::trim).collect()
        }
    }

    /// `.insn` is an unrecognized instruction, not data.
    fn is_data(&self) -> bool {
        self.mnemonic.starts_with('.') && self.mnemonic != ".insn"
    }
}

/// `None` for elisions, headers and anything else that is not an instruction or data line.
fn parse_line(line: &str) -> Option<Line> {
    let mut fields = line.trim_start().splitn(4, '\t');
    let addr = u32::from_str_radix(fields.next()?.strip_suffix(':')?, 16).ok()?;
    let bits_text = fields.next()?.trim();
    if bits_text.len() != 4 && bits_text.len() != 8 {
        return None;
    }
    let bits = u32::from_str_radix(bits_text, 16).ok()?;
    let mnemonic = fields.next()?.trim();
    let operands = fields.next().unwrap_or("");
    let operands = operands.split(" #").next().unwrap_or("").trim();
    let operands = match operands.rfind(" <") {
        Some(at) if operands.ends_with('>') => &operands[..at],
        _ => operands,
    };
    Some(Line {
        addr,
        bits,
        len: bits_text.len() / 2,
        mnemonic: mnemonic.to_string(),
        operands: operands.to_string(),
    })
}

const REG: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// The CSR names objdump prints; it prints every other address as `0x<hex>`.
const CSR: [(&str, u32); 30] = [
    ("mstatus", 0x300),
    ("misa", 0x301),
    ("medeleg", 0x302),
    ("mideleg", 0x303),
    ("mie", 0x304),
    ("mtvec", 0x305),
    ("mcounteren", 0x306),
    ("mcountinhibit", 0x320),
    ("mscratch", 0x340),
    ("mepc", 0x341),
    ("mcause", 0x342),
    ("mtval", 0x343),
    ("mip", 0x344),
    ("tselect", 0x7a0),
    ("tdata1", 0x7a1),
    ("tdata2", 0x7a2),
    ("tdata3", 0x7a3),
    ("tinfo", 0x7a4),
    ("tcontrol", 0x7a5),
    ("mcycle", 0xb00),
    ("minstret", 0xb02),
    ("mcycleh", 0xb80),
    ("minstreth", 0xb82),
    ("cycle", 0xc00),
    ("time", 0xc01),
    ("instret", 0xc02),
    ("mvendorid", 0xf11),
    ("marchid", 0xf12),
    ("mimpid", 0xf13),
    ("mhartid", 0xf14),
];

fn reg(name: &str) -> Option<u8> {
    REG.iter().position(|r| *r == name).map(|i| i as u8)
}

fn csr(text: &str) -> Option<u32> {
    if let Some(hex) = text.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).ok();
    }
    if let Some(n) = text.strip_prefix("pmpcfg") {
        let n: u32 = n.parse().ok()?;
        return (n < 4).then_some(0x3a0 + n);
    }
    if let Some(n) = text.strip_prefix("pmpaddr") {
        let n: u32 = n.parse().ok()?;
        return (n < 16).then_some(0x3b0 + n);
    }
    CSR.iter().find(|(n, _)| *n == text).map(|(_, a)| *a)
}

/// Signed decimal: I and S immediates and the CSR uimm.
fn dec(text: &str) -> Option<i32> {
    text.parse().ok()
}

/// `0x<hex>`: `lui` and `auipc` fields and shift amounts.
fn hex(text: &str) -> Option<u32> {
    u32::from_str_radix(text.strip_prefix("0x")?, 16).ok()
}

/// Bare hex: absolute branch and jump targets.
fn abs(text: &str) -> Option<u32> {
    u32::from_str_radix(text, 16).ok()
}

fn mem(text: &str) -> Option<(i32, u8)> {
    let (off, base) = text.split_once('(')?;
    Some((dec(off)?, reg(base.strip_suffix(')')?)?))
}

enum Expect {
    /// kind, `rd`, `rs1`, `rs2`, `imm`, `imm2`; fields the kind does not use are 0.
    Is(u8, u8, u8, u8, i32, u32),
    /// Fields unchecked, where objdump's text does not name one op.
    OneOf(&'static [u8]),
}

/// A write to `x0` folds to [`K_NOP`] with no fields.
fn plain(kind: u8, rd: u8, rs1: u8, rs2: u8, imm: i32) -> Expect {
    if rd == 0 {
        Expect::Is(K_NOP, 0, 0, 0, 0, 0)
    } else {
        Expect::Is(kind, rd, rs1, rs2, imm, 0)
    }
}

/// The op objdump's line must decode to, from its mnemonic and operands alone, never the bits.
/// `Err` names a form this oracle does not know yet; it is never accepted silently.
fn expect_op(line: &Line) -> Result<Expect, String> {
    let o = line.ops();
    expect_form(line, &o).ok_or_else(|| {
        format!(
            "{:#x}: no expected op for objdump's `{}` ({} bytes)",
            line.addr,
            line.want(),
            line.len
        )
    })
}

fn expect_form(line: &Line, o: &[&str]) -> Option<Expect> {
    let (addr, bits, len) = (line.addr, line.bits, line.len as u32);
    // Link address, branch fall-through and the resume pc of `wfi` and `fence.i`.
    let next = addr.wrapping_add(len);
    let (o0, o1, o2) = (o.first().copied(), o.get(1).copied(), o.get(2).copied());
    let ill = || Expect::Is(K_ILLEGAL, 0, 0, 0, 0, bits);
    let nop = || Expect::Is(K_NOP, 0, 0, 0, 0, 0);
    let alu = |k: u8| -> Option<Expect> { Some(plain(k, reg(o0?)?, reg(o1?)?, reg(o2?)?, 0)) };
    let imm_alu = |k: u8| -> Option<Expect> { Some(plain(k, reg(o0?)?, reg(o1?)?, 0, dec(o2?)?)) };
    let load = |k: u8| -> Option<Expect> {
        let (off, base) = mem(o1?)?;
        Some(Expect::Is(k, reg(o0?)?, base, 0, off, 0))
    };
    let store = |k: u8| -> Option<Expect> {
        let (off, base) = mem(o1?)?;
        Some(Expect::Is(k, 0, base, reg(o0?)?, off, 0))
    };
    let branch = |k: u8, rs1: &str, rs2: &str, target: &str| -> Option<Expect> {
        let (rs1, rs2) = (reg(rs1)?, reg(rs2)?);
        Some(Expect::Is(k, 0, rs1, rs2, abs(target)? as i32, next))
    };
    // objdump prints the full 6-bit shift field; amounts above 31 are illegal on RV32.
    let shift = |k: u8| -> Option<Expect> {
        let (rd, rs1, shamt) = (reg(o0?)?, reg(o1?)?, hex(o2?)?);
        if shamt > 31 {
            return Some(ill());
        }
        Some(plain(k, rd, rs1, 0, shamt as i32))
    };
    let jump_reg = |rd: u8, text: &str| -> Option<Expect> {
        let (imm, base) = if text.contains('(') {
            mem(text)?
        } else {
            (0, reg(text)?)
        };
        Some(Expect::Is(K_JALR, rd, base, 0, imm, next))
    };
    let csr_reg = |k: u8, rd: &str, name: &str, rs1: &str| -> Option<Expect> {
        let (rd, rs1) = (reg(rd)?, reg(rs1)?);
        Some(Expect::Is(k, rd, rs1, 0, 0, csr(name)?))
    };
    let csr_imm = |k: u8, rd: &str, name: &str, uimm: &str| -> Option<Expect> {
        Some(Expect::Is(k, reg(rd)?, 0, 0, dec(uimm)?, csr(name)?))
    };
    // objdump keeps the `c.` name for a HINT writing x0; the decoder folds it to a NOP.
    let hint_nop = || -> Option<Expect> { (o0? == "zero").then(nop) };

    match (line.mnemonic.as_str(), o.len()) {
        ("lui", 2) => Some(plain(K_LUI, reg(o0?)?, 0, 0, (hex(o1?)? << 12) as i32)),
        ("auipc", 2) => Some(plain(
            K_AUIPC,
            reg(o0?)?,
            0,
            0,
            addr.wrapping_add(hex(o1?)? << 12) as i32,
        )),
        ("nop", 0) => Some(nop()),
        ("li", 2) => Some(plain(K_ADDI, reg(o0?)?, 0, 0, dec(o1?)?)),
        // `c.mv` expands to `add rd,x0,rs2`, the 32-bit `mv` is `addi rd,rs1,0`.
        ("mv", 2) if len == 2 => Some(plain(K_ADD, reg(o0?)?, 0, reg(o1?)?, 0)),
        ("mv", 2) => Some(plain(K_ADDI, reg(o0?)?, reg(o1?)?, 0, 0)),
        // `c.addi` HINT and reserved `c.addi16sp`, both with a zero immediate, print this.
        ("addi", 3) if len == 2 && line.operands == "sp,sp,0" => {
            Some(Expect::OneOf(&[K_ADDI, K_ILLEGAL]))
        }
        ("addi", 3) => imm_alu(K_ADDI),
        ("slti", 3) => imm_alu(K_SLTI),
        ("sltiu", 3) => imm_alu(K_SLTIU),
        ("xori", 3) => imm_alu(K_XORI),
        ("ori", 3) => imm_alu(K_ORI),
        ("andi", 3) => imm_alu(K_ANDI),
        ("not", 2) => Some(plain(K_XORI, reg(o0?)?, reg(o1?)?, 0, -1)),
        ("seqz", 2) => Some(plain(K_SLTIU, reg(o0?)?, reg(o1?)?, 0, 1)),
        ("zext.b", 2) => Some(plain(K_ANDI, reg(o0?)?, reg(o1?)?, 0, 255)),
        ("slli", 3) => shift(K_SLLI),
        ("srli", 3) => shift(K_SRLI),
        ("srai", 3) => shift(K_SRAI),
        ("add", 3) => alu(K_ADD),
        ("sub", 3) => alu(K_SUB),
        ("sll", 3) => alu(K_SLL),
        ("slt", 3) => alu(K_SLT),
        ("sltu", 3) => alu(K_SLTU),
        ("xor", 3) => alu(K_XOR),
        ("srl", 3) => alu(K_SRL),
        ("sra", 3) => alu(K_SRA),
        ("or", 3) => alu(K_OR),
        ("and", 3) => alu(K_AND),
        ("mul", 3) => alu(K_MUL),
        ("mulh", 3) => alu(K_MULH),
        ("mulhsu", 3) => alu(K_MULHSU),
        ("mulhu", 3) => alu(K_MULHU),
        ("div", 3) => alu(K_DIV),
        ("divu", 3) => alu(K_DIVU),
        ("rem", 3) => alu(K_REM),
        ("remu", 3) => alu(K_REMU),
        ("neg", 2) => Some(plain(K_SUB, reg(o0?)?, 0, reg(o1?)?, 0)),
        ("snez", 2) => Some(plain(K_SLTU, reg(o0?)?, 0, reg(o1?)?, 0)),
        ("sltz", 2) => Some(plain(K_SLT, reg(o0?)?, reg(o1?)?, 0, 0)),
        ("sgtz", 2) => Some(plain(K_SLT, reg(o0?)?, 0, reg(o1?)?, 0)),
        ("lb", 2) => load(K_LB),
        ("lh", 2) => load(K_LH),
        ("lw", 2) => load(K_LW),
        ("lbu", 2) => load(K_LBU),
        ("lhu", 2) => load(K_LHU),
        ("sb", 2) => store(K_SB),
        ("sh", 2) => store(K_SH),
        ("sw", 2) => store(K_SW),
        ("j", 1) => Some(Expect::Is(K_JAL, 0, 0, 0, abs(o0?)? as i32, next)),
        ("jal", 1) => Some(Expect::Is(K_JAL, 1, 0, 0, abs(o0?)? as i32, next)),
        ("jal", 2) => Some(Expect::Is(K_JAL, reg(o0?)?, 0, 0, abs(o1?)? as i32, next)),
        ("ret", 0) => Some(Expect::Is(K_JALR, 0, 1, 0, 0, next)),
        ("jr", 1) => jump_reg(0, o0?),
        ("jalr", 1) => jump_reg(1, o0?),
        ("jalr", 2) => {
            let (imm, base) = mem(o1?)?;
            Some(Expect::Is(K_JALR, reg(o0?)?, base, 0, imm, next))
        }
        ("beq", 3) => branch(K_BEQ, o0?, o1?, o2?),
        ("bne", 3) => branch(K_BNE, o0?, o1?, o2?),
        ("blt", 3) => branch(K_BLT, o0?, o1?, o2?),
        ("bge", 3) => branch(K_BGE, o0?, o1?, o2?),
        ("bltu", 3) => branch(K_BLTU, o0?, o1?, o2?),
        ("bgeu", 3) => branch(K_BGEU, o0?, o1?, o2?),
        ("beqz", 2) => branch(K_BEQ, o0?, "zero", o1?),
        ("bnez", 2) => branch(K_BNE, o0?, "zero", o1?),
        ("bltz", 2) => branch(K_BLT, o0?, "zero", o1?),
        ("bgtz", 2) => branch(K_BLT, "zero", o0?, o1?),
        ("bgez", 2) => branch(K_BGE, o0?, "zero", o1?),
        ("blez", 2) => branch(K_BGE, "zero", o0?, o1?),
        // Every FENCE is a NOP, whatever its fm, pred and succ.
        ("fence", 0 | 2) | ("fence.tso", 0) => Some(nop()),
        ("fence.i", 0) => Some(Expect::Is(K_FENCEI, 0, 0, 0, 0, next)),
        ("ecall", 0) => Some(Expect::Is(K_ECALL, 0, 0, 0, 0, 0)),
        ("ebreak", 0) => Some(Expect::Is(K_EBREAK, 0, 0, 0, 0, 0)),
        ("mret", 0) => Some(Expect::Is(K_MRET, 0, 0, 0, 0, 0)),
        ("wfi", 0) => Some(Expect::Is(K_WFI, 0, 0, 0, 0, next)),
        // The C3 implements neither.
        ("sret", 0) | ("dret", 0) => Some(ill()),
        // The all-zero halfword, and `csrrw zero,cycle,zero` which objdump prints as `unimp`.
        ("unimp", 0) if len == 2 => Some(ill()),
        ("unimp", 0) => Some(Expect::Is(K_CSRRW, 0, 0, 0, 0, 0xc00)),
        (".insn", 2) => Some(Expect::OneOf(&[K_ILLEGAL, K_NOP, K_FENCEI])),
        ("csrrw", 3) => csr_reg(K_CSRRW, o0?, o1?, o2?),
        ("csrrs", 3) => csr_reg(K_CSRRS, o0?, o1?, o2?),
        ("csrrc", 3) => csr_reg(K_CSRRC, o0?, o1?, o2?),
        ("csrw", 2) => csr_reg(K_CSRRW, "zero", o0?, o1?),
        ("csrs", 2) => csr_reg(K_CSRRS, "zero", o0?, o1?),
        ("csrc", 2) => csr_reg(K_CSRRC, "zero", o0?, o1?),
        ("csrr", 2) => csr_reg(K_CSRRS, o0?, o1?, "zero"),
        ("csrrwi", 3) => csr_imm(K_CSRRWI, o0?, o1?, o2?),
        ("csrrsi", 3) => csr_imm(K_CSRRSI, o0?, o1?, o2?),
        ("csrrci", 3) => csr_imm(K_CSRRCI, o0?, o1?, o2?),
        ("csrwi", 2) => csr_imm(K_CSRRWI, "zero", o0?, o1?),
        ("csrsi", 2) => csr_imm(K_CSRRSI, "zero", o0?, o1?),
        ("csrci", 2) => csr_imm(K_CSRRCI, "zero", o0?, o1?),
        ("rdcycle", 1) => csr_reg(K_CSRRS, o0?, "cycle", "zero"),
        ("rdtime", 1) => csr_reg(K_CSRRS, o0?, "time", "zero"),
        ("rdinstret", 1) => csr_reg(K_CSRRS, o0?, "instret", "zero"),
        ("rdcycleh", 1) => csr_reg(K_CSRRS, o0?, "0xc80", "zero"),
        ("rdtimeh", 1) => csr_reg(K_CSRRS, o0?, "0xc81", "zero"),
        ("rdinstreth", 1) => csr_reg(K_CSRRS, o0?, "0xc82", "zero"),
        ("c.nop", 1) => dec(o0?).map(|_| nop()),
        ("c.li", 2) | ("c.lui", 2) | ("c.mv", 2) | ("c.add", 2) => hint_nop(),
        ("c.slli", 2) => {
            let shamt = hex(o1?)?;
            (o0? == "zero").then(|| if shamt > 31 { ill() } else { nop() })
        }
        ("c.slli64", 1) => Some(plain(K_SLLI, reg(o0?)?, reg(o0?)?, 0, 0)),
        ("c.srli64", 1) => Some(plain(K_SRLI, reg(o0?)?, reg(o0?)?, 0, 0)),
        ("c.srai64", 1) => Some(plain(K_SRAI, reg(o0?)?, reg(o0?)?, 0, 0)),
        _ => None,
    }
}

fn show(kind: u8, rd: u8, rs1: u8, rs2: u8, imm: i32, imm2: u32) -> String {
    format!(
        "{} rd={rd} rs1={rs1} rs2={rs2} imm={imm} imm2={imm2:#x}",
        kind_name(kind)
    )
}

fn check_op(op: &Op, want: &Expect, line: &Line) -> Result<(), String> {
    let got = || show(op.kind, op.rd, op.rs1, op.rs2, op.imm, op.imm2);
    match *want {
        Expect::Is(kind, rd, rs1, rs2, imm, imm2) => {
            if (op.kind, op.rd, op.rs1, op.rs2, op.imm, op.imm2) != (kind, rd, rs1, rs2, imm, imm2)
            {
                return Err(format!(
                    "{:#x} {:#x}: op is `{}`, objdump `{}` says `{}`",
                    line.addr,
                    line.bits,
                    got(),
                    line.want(),
                    show(kind, rd, rs1, rs2, imm, imm2)
                ));
            }
        }
        Expect::OneOf(kinds) => {
            if !kinds.contains(&op.kind) {
                return Err(format!(
                    "{:#x} {:#x}: op is `{}`, objdump `{}` allows only {:?}",
                    line.addr,
                    line.bits,
                    got(),
                    line.want(),
                    kinds.iter().map(|k| kind_name(*k)).collect::<Vec<_>>()
                ));
            }
        }
    }
    // An illegal instruction carries the bits for `mtval` and nothing else.
    if op.kind == K_ILLEGAL
        && (op.imm2 != line.bits || op.rd != 0 || op.rs1 != 0 || op.rs2 != 0 || op.imm != 0)
    {
        return Err(format!(
            "{:#x} {:#x}: illegal op is `{}`, want only imm2 = the bits",
            line.addr,
            line.bits,
            got()
        ));
    }
    if op.flags != flags_for(op.kind, op.rd) {
        return Err(format!(
            "{:#x} {:#x}: flags {:#x} of `{}`, {:#x} by the kind and rd",
            line.addr,
            line.bits,
            op.flags,
            got(),
            flags_for(op.kind, op.rd)
        ));
    }
    Ok(())
}

/// `Err` is a problem with the file or the tool; mismatches are counted in [`Counts`].
fn compare(target: &Target, tool: &Path) -> Result<Counts, String> {
    let elf = std::fs::read(&target.path).map_err(|e| format!("cannot read the file: {e}"))?;
    let sections = sections(&elf)?;
    let out = Command::new(tool)
        .arg("-d")
        .arg(&target.path)
        .output()
        .map_err(|e| format!("{} did not run: {e}", tool.display()))?;
    if !out.status.success() {
        return Err(format!("objdump -d failed: status {}", out.status));
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    let mut counts = Counts {
        id: target.id,
        compared: 0,
        mismatches: 0,
        lenient: 0,
        data: 0,
        details: Vec::new(),
    };
    let mut section: Option<&Section> = None;
    for line in listing.lines() {
        if let Some(name) = line
            .strip_prefix("Disassembly of section ")
            .and_then(|rest| rest.strip_suffix(':'))
        {
            section = sections.iter().find(|s| s.name == name);
            if section.is_none() {
                return Err(format!(
                    "objdump disassembles section {name}, the ELF has no such"
                ));
            }
            continue;
        }
        let Some(parsed) = parse_line(line) else {
            continue;
        };
        let (addr, bits, len) = (parsed.addr, parsed.bits, parsed.len);
        let Some(sec) = section else {
            return Err(format!(
                "an instruction line at {addr:#x} before any section header"
            ));
        };
        if parsed.is_data() {
            counts.data += 1;
            continue;
        }
        counts.compared += 1;
        let mut fail = |detail: String| {
            counts.mismatches += 1;
            if counts.details.len() < MAX_DETAILS {
                counts.details.push(detail);
            }
        };
        let off = sec.data.0 + (addr.wrapping_sub(sec.addr)) as usize;
        let Some(bytes) = elf
            .get(off..off + len)
            .filter(|_| addr >= sec.addr && off + len <= sec.data.1)
        else {
            fail(format!(
                "{addr:#x}: {len} bytes outside section {}",
                sec.name
            ));
            continue;
        };
        let found = match len {
            2 => u16::from_le_bytes([bytes[0], bytes[1]]) as u32,
            _ => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        };
        if found != bits {
            fail(format!(
                "{addr:#x}: bits {found:#x} in the ELF, {bits:#x} in the listing"
            ));
            continue;
        }
        let Some(op) = decode_at(bytes, addr) else {
            fail(format!("{addr:#x}: decode_at returned None for {bits:#x}"));
            continue;
        };
        if op.len as usize != len {
            fail(format!(
                "{addr:#x}: length {} decoded, {len} printed",
                op.len
            ));
            continue;
        }
        let want = parsed.want();
        let got = format_op(&op, addr, found);
        if got != want {
            fail(format!("{addr:#x} {bits:#x}: `{got}`, objdump `{want}`"));
            continue;
        }
        let expected = expect_op(&parsed)?;
        let lenient = matches!(expected, Expect::OneOf(_));
        if let Err(detail) = check_op(&op, &expected, &parsed) {
            fail(detail);
        }
        // After the last use of `fail`, which borrows `counts`.
        if lenient {
            counts.lenient += 1;
        }
    }
    Ok(counts)
}

/// Fails on any mismatch; missing files and a missing objdump skip with a message.
fn run_corpus(targets: &[Target]) {
    let tool = objdump();
    let version = Command::new(&tool).arg("--version").output();
    if version
        .as_ref()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        println!(
            "decode corpus skipped: no objdump at {} (set PASSPORTSIM_OBJDUMP)",
            tool.display()
        );
        return;
    }
    let mut results = Vec::new();
    let mut missing = Vec::new();
    let mut problems = Vec::new();
    for target in targets {
        if !target.path.is_file() {
            missing.push(target.id);
            continue;
        }
        match compare(target, &tool) {
            Ok(counts) => {
                if counts.compared < target.min_lines {
                    problems.push(format!(
                        "{}: only {} instruction lines from objdump, at least {} expected",
                        target.id, counts.compared, target.min_lines
                    ));
                }
                results.push(counts);
            }
            Err(problem) => problems.push(format!("{}: {problem}", target.id)),
        }
    }
    let mut total = 0;
    let mut bad = 0;
    for counts in &results {
        println!(
            "decode corpus {}: {} instructions compared ({} on their kind set only), {} mismatches, \
             {} data lines skipped",
            counts.id, counts.compared, counts.lenient, counts.mismatches, counts.data
        );
        total += counts.compared;
        bad += counts.mismatches;
    }
    if !missing.is_empty() {
        println!(
            "decode corpus skipped, file missing: {}",
            missing.join(", ")
        );
    }
    assert!(
        problems.is_empty(),
        "objdump, ELF or coverage problems: {problems:#?}"
    );
    let details: Vec<&String> = results.iter().flat_map(|c| c.details.iter()).collect();
    assert_eq!(
        bad, 0,
        "{bad} of {total} instructions mismatch: {details:#?}"
    );
    if results.is_empty() {
        println!(
            "decode corpus skipped: none of the {} files is present",
            targets.len()
        );
    } else {
        println!(
            "decode corpus: {total} instructions compared over {} files",
            results.len()
        );
    }
}

#[test]
fn t0_rom_elfs_decode_like_objdump() {
    run_corpus(&rom_targets());
}

#[test]
fn t1_rom_and_corpus_elfs_decode_like_objdump() {
    let mut targets = rom_targets();
    targets.extend(corpus_targets());
    run_corpus(&targets);
}

/// Guards the op half of the check: a decode with wrong fields but the right text must fail.
#[test]
fn t0_the_op_check_rejects_a_wrong_op() {
    let line = parse_line("40380100:\t008000ef          \tjal\t40380108 <foo>").expect("a line");
    assert_eq!(
        (line.addr, line.bits, line.len),
        (0x4038_0100, 0x0080_00ef, 4)
    );
    assert_eq!(line.want(), "jal 40380108");
    let want = expect_op(&line).expect("an expectation for `jal`");
    let good = decode_at(&line.bits.to_le_bytes(), line.addr).expect("a decode");
    assert_eq!(format_op(&good, line.addr, line.bits), line.want());
    check_op(&good, &want, &line).expect("the real decode agrees with objdump");
    let bogus = Op {
        kind: K_NOP,
        rd: 31,
        rs1: 31,
        rs2: 31,
        imm: 0x7fff_ffff,
        imm2: 0xdead_beef,
        len: 4,
        flags: 0,
        pc_off: 0,
    };
    assert_eq!(
        format_op(&bogus, line.addr, line.bits),
        line.want(),
        "the text comes from the bits, so it cannot catch this"
    );
    let detail = check_op(&bogus, &want, &line).expect_err("the op check must catch it");
    assert!(detail.contains("objdump `jal 40380108`"), "{detail}");
}

#[test]
fn t0_data_lines_are_skipped_and_insn_is_not() {
    for text in [
        "40380100:\t00000000          \t.word\t0x00000000",
        "40380104:\t0000              \t.short\t0x0000",
    ] {
        assert!(parse_line(text).expect("a line").is_data(), "{text}");
    }
    let insn = parse_line("400591f4:\tf09c                \t.insn\t2, 0xf09c").expect("a line");
    assert!(!insn.is_data());
    assert_eq!(insn.want(), ".insn 2, 0xf09c");
    let op = decode_at(&(insn.bits as u16).to_le_bytes(), insn.addr).expect("a decode");
    assert_eq!(format_op(&op, insn.addr, insn.bits), insn.want());
    check_op(&op, &expect_op(&insn).expect("an expectation"), &insn).expect("illegal is allowed");
}
