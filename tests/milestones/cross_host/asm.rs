//! A minimal RV32IM assembler for the parity guest ([`super::guest`]): the encodings of the RISC-V
//! unprivileged specification, labels, and the few pseudo-instructions the guest uses. The tree
//! carries no bootable firmware that needs no corpus, so the guest is assembled here.

use std::collections::BTreeMap;

/// `zero`.
pub const ZERO: u32 = 0;
/// `ra`.
pub const RA: u32 = 1;
/// `t0` to `t2`.
pub const T0: u32 = 5;
pub const T1: u32 = 6;
pub const T2: u32 = 7;
/// `s0`: the guest's running digest ([`super::guest`]), preserved across ROM calls.
pub const S0: u32 = 8;
/// `s1`: the digest's multiplier.
pub const S1: u32 = 9;
/// `a0` to `a4`.
pub const A0: u32 = 10;
pub const A1: u32 = 11;
pub const A2: u32 = 12;
pub const A3: u32 = 13;
pub const A4: u32 = 14;
/// `s2` to `s4`: loop state that must survive a ROM call.
pub const S2: u32 = 18;
pub const S3: u32 = 19;
pub const S4: u32 = 20;

/// A position in the code, bound once with [`Asm::bind`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Label(usize);

/// What a fixup patches once its label is bound.
#[derive(Copy, Clone)]
enum Fix {
    Branch { funct3: u32, rs1: u32, rs2: u32 },
    Jal { rd: u32 },
}

pub struct Asm {
    base: u32,
    words: Vec<u32>,
    bound: BTreeMap<Label, u32>,
    labels: usize,
    fixups: Vec<(usize, Label, Fix)>,
}

impl Asm {
    pub fn new(base: u32) -> Asm {
        Asm {
            base,
            words: Vec::new(),
            bound: BTreeMap::new(),
            labels: 0,
            fixups: Vec::new(),
        }
    }

    pub fn here(&self) -> u32 {
        self.base + 4 * self.words.len() as u32
    }

    pub fn label(&mut self) -> Label {
        self.labels += 1;
        Label(self.labels - 1)
    }

    /// Binds `l` to [`Asm::here`].
    pub fn bind(&mut self, l: Label) {
        let at = self.here();
        assert!(self.bound.insert(l, at).is_none(), "label bound twice");
    }

    /// The address `l` was bound to.
    pub fn addr(&self, l: Label) -> u32 {
        self.bound[&l]
    }

    pub fn finish(mut self) -> Vec<u8> {
        for (i, l, fix) in std::mem::take(&mut self.fixups) {
            let from = self.base + 4 * i as u32;
            let off = self.addr(l).wrapping_sub(from) as i32;
            self.words[i] = match fix {
                Fix::Branch { funct3, rs1, rs2 } => {
                    assert!((-4096..4096).contains(&off), "branch out of range");
                    branch(funct3, rs1, rs2, off)
                }
                Fix::Jal { rd } => {
                    assert!((-(1 << 20)..(1 << 20)).contains(&off), "jal out of range");
                    jal(rd, off)
                }
            };
        }
        self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn emit(&mut self, word: u32) {
        self.words.push(word);
    }

    fn r(&mut self, funct7: u32, funct3: u32, rd: u32, rs1: u32, rs2: u32) {
        self.emit(funct7 << 25 | rs2 << 20 | rs1 << 15 | funct3 << 12 | rd << 7 | 0x33);
    }

    fn i(&mut self, opcode: u32, funct3: u32, rd: u32, rs1: u32, imm: i32) {
        assert!((-2048..2048).contains(&imm), "12-bit immediate {imm}");
        self.emit(((imm as u32) & 0xFFF) << 20 | rs1 << 15 | funct3 << 12 | rd << 7 | opcode);
    }

    pub fn lui(&mut self, rd: u32, imm20: u32) {
        self.emit((imm20 & 0xF_FFFF) << 12 | rd << 7 | 0x37);
    }
    pub fn addi(&mut self, rd: u32, rs1: u32, imm: i32) {
        self.i(0x13, 0, rd, rs1, imm);
    }
    pub fn andi(&mut self, rd: u32, rs1: u32, imm: i32) {
        self.i(0x13, 7, rd, rs1, imm);
    }
    pub fn slli(&mut self, rd: u32, rs1: u32, shamt: u32) {
        self.i(0x13, 1, rd, rs1, shamt as i32);
    }
    pub fn srli(&mut self, rd: u32, rs1: u32, shamt: u32) {
        self.i(0x13, 5, rd, rs1, shamt as i32);
    }
    pub fn add(&mut self, rd: u32, rs1: u32, rs2: u32) {
        self.r(0, 0, rd, rs1, rs2);
    }
    pub fn xor(&mut self, rd: u32, rs1: u32, rs2: u32) {
        self.r(0, 4, rd, rs1, rs2);
    }
    pub fn and(&mut self, rd: u32, rs1: u32, rs2: u32) {
        self.r(0, 7, rd, rs1, rs2);
    }
    pub fn mul(&mut self, rd: u32, rs1: u32, rs2: u32) {
        self.r(1, 0, rd, rs1, rs2);
    }
    pub fn mulhu(&mut self, rd: u32, rs1: u32, rs2: u32) {
        self.r(1, 3, rd, rs1, rs2);
    }
    pub fn divu(&mut self, rd: u32, rs1: u32, rs2: u32) {
        self.r(1, 5, rd, rs1, rs2);
    }
    pub fn remu(&mut self, rd: u32, rs1: u32, rs2: u32) {
        self.r(1, 7, rd, rs1, rs2);
    }
    pub fn lw(&mut self, rd: u32, rs1: u32, off: i32) {
        self.i(0x03, 2, rd, rs1, off);
    }
    pub fn sw(&mut self, rs2: u32, rs1: u32, off: i32) {
        assert!((-2048..2048).contains(&off));
        let o = off as u32;
        self.emit((o >> 5 & 0x7F) << 25 | rs2 << 20 | rs1 << 15 | 2 << 12 | (o & 0x1F) << 7 | 0x23);
    }
    pub fn jalr(&mut self, rd: u32, rs1: u32, off: i32) {
        self.i(0x67, 0, rd, rs1, off);
    }
    fn branch_to(&mut self, funct3: u32, rs1: u32, rs2: u32, l: Label) {
        self.fixups
            .push((self.words.len(), l, Fix::Branch { funct3, rs1, rs2 }));
        self.emit(0);
    }
    pub fn beq(&mut self, rs1: u32, rs2: u32, l: Label) {
        self.branch_to(0, rs1, rs2, l);
    }
    pub fn bne(&mut self, rs1: u32, rs2: u32, l: Label) {
        self.branch_to(1, rs1, rs2, l);
    }
    /// `jal rd, l`.
    pub fn jal(&mut self, rd: u32, l: Label) {
        self.fixups.push((self.words.len(), l, Fix::Jal { rd }));
        self.emit(0);
    }
    /// `j l`.
    pub fn j(&mut self, l: Label) {
        self.jal(ZERO, l);
    }
    /// `ret`.
    pub fn ret(&mut self) {
        self.jalr(ZERO, RA, 0);
    }
    /// `mv rd, rs`.
    pub fn mv(&mut self, rd: u32, rs: u32) {
        self.addi(rd, rs, 0);
    }

    /// `li rd, value`: `lui` and `addi`, the `addi` compensating for its sign extension.
    pub fn li(&mut self, rd: u32, value: u32) {
        let lo = ((value & 0xFFF) as i32) << 20 >> 20;
        let hi = value.wrapping_sub(lo as u32) >> 12;
        if hi != 0 {
            self.lui(rd, hi);
            if lo != 0 {
                self.addi(rd, rd, lo);
            }
        } else {
            self.addi(rd, ZERO, lo);
        }
    }

    /// `*addr = value`, through `t0` and `t1`.
    pub fn store(&mut self, addr: u32, value: u32) {
        self.li(T0, addr);
        self.li(T1, value);
        self.sw(T1, T0, 0);
    }

    /// `rd = *addr`, through `t0`.
    pub fn load(&mut self, rd: u32, addr: u32) {
        self.li(T0, addr);
        self.lw(rd, T0, 0);
    }

    /// Spins until `*addr & mask` is not zero (`set`) or is zero (`!set`), through `t0` to `t2`.
    pub fn wait(&mut self, addr: u32, mask: u32, set: bool) {
        self.li(T0, addr);
        self.li(T1, mask);
        let top = self.label();
        self.bind(top);
        self.lw(T2, T0, 0);
        self.and(T2, T2, T1);
        if set {
            self.beq(T2, ZERO, top);
        } else {
            self.bne(T2, ZERO, top);
        }
    }

    /// `jalr ra, target` through `t0`.
    pub fn call_abs(&mut self, target: u32) {
        self.li(T0, target);
        self.jalr(RA, T0, 0);
    }
}

fn branch(funct3: u32, rs1: u32, rs2: u32, off: i32) -> u32 {
    let o = off as u32;
    (o >> 12 & 1) << 31
        | (o >> 5 & 0x3F) << 25
        | rs2 << 20
        | rs1 << 15
        | funct3 << 12
        | (o >> 1 & 0xF) << 8
        | (o >> 11 & 1) << 7
        | 0x63
}

fn jal(rd: u32, off: i32) -> u32 {
    let o = off as u32;
    (o >> 20 & 1) << 31
        | (o >> 1 & 0x3FF) << 21
        | (o >> 11 & 1) << 20
        | (o >> 12 & 0xFF) << 12
        | rd << 7
        | 0x6F
}

/// The encoder against words from the specification's own examples and from the machine's tests.
#[test]
fn the_encodings_match_known_words() {
    let mut a = Asm::new(0x4038_0000);
    a.lui(T0, 0x60002); // `lui t0, 0x60002` in `pemu_api`'s guest: 0x600022B7.
    a.sw(T1, T0, 0); // 0x0062A023.
    a.lw(A1, T0, 0x58); // 0x0582A583.
    a.li(T1, 0x2D6B_6F74);
    let end = a.label();
    a.bind(end);
    a.j(end); // `j .` is 0x0000006F.
    let words: Vec<u32> = a
        .finish()
        .chunks(4)
        .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
        .collect();
    assert_eq!(&words[..3], &[0x6000_22B7, 0x0062_A023, 0x0582_A583]);
    // `li` of a value with bit 11 set rounds the upper part up (0x2D6B7337, 0xF7430313 there).
    assert_eq!(&words[3..5], &[0x2D6B_7337, 0xF743_0313]);
    assert_eq!(words[5], 0x0000_006F);
}
