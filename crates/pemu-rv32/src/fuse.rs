//! Critical-section fusion: the `csrrci mstatus` / threshold write / MIE restore sequence of the
//! IDF port recognized as one unit (`specs/blocks/intc.toml` records the threshold write).
//!
//! [`match_critical_section`] recognizes a unit in decoded ops, and [`scan_image`] counts the
//! candidate sites of an image so `xtask bench` can size the lever before anything is fused. The
//! restore is `csrrsi mstatus, 8` or, as the port mostly writes it, `csrrs mstatus, rX` with rX
//! derived from the saved mstatus.
//!
//! No `OpFuser` is installed: every CSR kind is a terminator, so the window the engine offers
//! never holds both CSR ops; the engine accepts only one-for-one rewrites; and no fused op kind
//! exists. A fuser that always returned `None` would be a placeholder.

use crate::op::{F_LOAD, F_STORE, F_TERM, K_ADDI, K_CSRRCI, K_CSRRS, K_CSRRSI, K_LUI, K_SW, Op};

pub const CSR_MSTATUS: u32 = 0x300;

pub const MSTATUS_MIE: i32 = 1 << 3;

/// `INTERRUPT_CORE0_CPU_INT_THRESH` (`specs/c3-registers.csv` intc row).
pub const INT_THRESH_ADDR: u32 = 0x600C_2000 + 0x194;

/// Most ops between the `csrrci` and the restore: room for the threshold write and masking the
/// saved mstatus, without counting unrelated code. UNVERIFIED: a design choice, not taken from a
/// listing of the guest's sequence.
pub const MAX_BODY_OPS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    Immediate,
    /// `csrrs mstatus, rX` with rX derived from the value the `csrrci` saved.
    Register,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CriticalSection {
    /// Ops the unit spans, both CSR ops included.
    pub ops: usize,
    pub stores: usize,
    /// A body `sw` provably hits the threshold register through a `lui` (plus `addi`) base.
    pub threshold_store: bool,
    pub restore: Restore,
}

impl CriticalSection {
    pub fn writes_threshold(&self) -> bool {
        self.threshold_store
    }
}

fn is_mie_toggle(op: &Op, kind: u8) -> bool {
    op.kind == kind && op.imm2 == CSR_MSTATUS && op.imm & MSTATUS_MIE != 0
}

fn bit(r: u8) -> u32 {
    let r = r & 31;
    if r == 0 { 0 } else { 1 << r }
}

/// Recognizes a unit at the start of `window` (decoded ops in address order): a `csrrci mstatus`
/// clearing MIE, a body of at most [`MAX_BODY_OPS`] non-terminator ops holding at least one store,
/// and a restore. A register-form restore must read a register derived from the `csrrci`'s `rd`.
/// A body with no store is not the lever: fusing it saves two dispatches and no MMIO path.
pub fn match_critical_section(window: &[Op]) -> Option<CriticalSection> {
    let first = window.first()?;
    if !is_mie_toggle(first, K_CSRRCI) {
        return None;
    }
    // Decode-time constants from `lui` and `addi` on a known register; an array, since core
    // crates ban `HashMap`.
    let mut known: [Option<u32>; 32] = [None; 32];
    let mut derived = bit(first.rd);
    let mut stores = 0;
    let mut threshold_store = false;
    for (i, op) in window.iter().enumerate().skip(1).take(MAX_BODY_OPS + 1) {
        let restore = if is_mie_toggle(op, K_CSRRSI) {
            Some(Restore::Immediate)
        } else if op.kind == K_CSRRS && op.imm2 == CSR_MSTATUS && derived & bit(op.rs1) != 0 {
            Some(Restore::Register)
        } else {
            None
        };
        if let Some(restore) = restore {
            return (stores > 0).then_some(CriticalSection {
                ops: i + 1,
                stores,
                threshold_store,
                restore,
            });
        }
        if op.flags & F_TERM != 0 || i > MAX_BODY_OPS {
            return None;
        }
        if op.flags & F_STORE != 0 {
            stores += 1;
            if op.kind == K_SW {
                let addr = known[(op.rs1 & 31) as usize].map(|b| b.wrapping_add(op.imm as u32));
                threshold_store |= addr == Some(INT_THRESH_ADDR);
            }
            continue;
        }
        let rd = (op.rd & 31) as usize;
        if rd == 0 {
            continue;
        }
        // `rd` is 0 for kinds without a destination (`crate::op` rule 2), so this only ever
        // forgets a constant.
        known[rd] = match op.kind {
            K_LUI => Some(op.imm as u32),
            K_ADDI => known[(op.rs1 & 31) as usize].map(|base| base.wrapping_add(op.imm as u32)),
            _ => None,
        };
        let from_saved = op.flags & F_LOAD == 0 && derived & (bit(op.rs1) | bit(op.rs2)) != 0;
        if from_saved {
            derived |= bit(op.rd);
        } else {
            derived &= !bit(op.rd);
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteCount {
    pub units: u64,
    pub threshold_units: u64,
    /// `csrrci mstatus` sites touching MIE: every place a unit could start. Against `units` it
    /// says whether the image or the recognizer limits the lever.
    pub candidates: u64,
}

/// Counts the units in a code image loaded at `base`. A raw image has no instruction boundaries
/// (RV32IMC mixes 2- and 4-byte instructions), so every even offset holding a `csrrci mstatus`
/// starts a decode; a misaligned candidate may count, which is fine for lever sizing. Units do
/// not overlap.
pub fn scan_image(bytes: &[u8], base: u32) -> SiteCount {
    let mut count = SiteCount::default();
    let mut off = 0usize;
    while off + 2 <= bytes.len() {
        let pc = base.wrapping_add(off as u32);
        let starts = crate::decode::decode_at(&bytes[off..], pc)
            .is_some_and(|op| is_mie_toggle(&op, K_CSRRCI));
        if !starts {
            off += 2;
            continue;
        }
        count.candidates += 1;
        let mut window = Vec::with_capacity(MAX_BODY_OPS + 2);
        let mut at = off;
        while window.len() < MAX_BODY_OPS + 2 {
            let Some(op) = crate::decode::decode_at(
                &bytes[at.min(bytes.len())..],
                base.wrapping_add(at as u32),
            ) else {
                break;
            };
            at += op.len.max(2) as usize;
            window.push(op);
        }
        match match_critical_section(&window) {
            Some(unit) => {
                count.units += 1;
                count.threshold_units += u64::from(unit.writes_threshold());
                off += window[..unit.ops]
                    .iter()
                    .map(|op| op.len.max(2) as usize)
                    .sum::<usize>();
            }
            None => off += 2,
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;

    fn csr_imm(funct3: u32, rd: u32, uimm: u32, csr: u32) -> u32 {
        (csr << 20) | (uimm << 15) | (funct3 << 12) | (rd << 7) | 0x73
    }
    fn csrrci(rd: u32, uimm: u32, csr: u32) -> u32 {
        csr_imm(7, rd, uimm, csr)
    }
    fn csrrsi(rd: u32, uimm: u32, csr: u32) -> u32 {
        csr_imm(6, rd, uimm, csr)
    }
    fn csrrs(rd: u32, rs1: u32, csr: u32) -> u32 {
        (csr << 20) | (rs1 << 15) | (2 << 12) | (rd << 7) | 0x73
    }
    fn andi(rd: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32 & 0xFFF) << 20) | (rs1 << 15) | (7 << 12) | (rd << 7) | 0x13
    }
    fn lui(rd: u32, value: u32) -> u32 {
        (value & 0xFFFF_F000) | (rd << 7) | 0x37
    }
    fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32 & 0xFFF) << 20) | (rs1 << 15) | (rd << 7) | 0x13
    }
    fn sw(rs2: u32, rs1: u32, imm: i32) -> u32 {
        let imm = imm as u32 & 0xFFF;
        ((imm >> 5) << 25) | (rs2 << 20) | (rs1 << 15) | (2 << 12) | ((imm & 0x1F) << 7) | 0x23
    }
    fn beq(rs1: u32, rs2: u32) -> u32 {
        (rs2 << 20) | (rs1 << 15) | (8 << 8) | 0x63
    }

    fn ops(words: &[u32]) -> Vec<Op> {
        words
            .iter()
            .enumerate()
            .map(|(i, &w)| decode(w, 0x4200_0000 + 4 * i as u32))
            .collect()
    }

    /// Interrupts off, threshold register written through a `lui` base, interrupts on.
    fn thresh_sequence() -> Vec<u32> {
        vec![
            csrrci(15, 8, CSR_MSTATUS),
            lui(14, 0x600C_2000),
            sw(10, 14, 0x194),
            csrrsi(0, 8, CSR_MSTATUS),
        ]
    }

    #[test]
    fn the_threshold_write_between_mie_toggles_is_one_unit() {
        let unit = match_critical_section(&ops(&thresh_sequence())).expect("recognized");
        assert_eq!(unit.ops, 4);
        assert_eq!(unit.stores, 1);
        assert!(unit.writes_threshold());
        assert_eq!(unit.restore, Restore::Immediate);
    }

    #[test]
    fn a_lui_plus_addi_base_is_tracked_to_the_store_address() {
        let words = [
            csrrci(0, 8, CSR_MSTATUS),
            lui(14, 0x600C_2000),
            addi(14, 14, 0x100),
            sw(10, 14, 0x94),
            csrrsi(0, 8, CSR_MSTATUS),
        ];
        let unit = match_critical_section(&ops(&words)).unwrap();
        assert!(unit.writes_threshold());
    }

    #[test]
    fn a_store_through_an_unknown_base_is_a_unit_but_not_the_threshold_write() {
        let words = [
            csrrci(0, 8, CSR_MSTATUS),
            sw(10, 11, 0),
            csrrsi(0, 8, CSR_MSTATUS),
        ];
        let unit = match_critical_section(&ops(&words)).unwrap();
        assert!(!unit.writes_threshold());
    }

    #[test]
    fn a_body_without_a_store_is_not_the_lever() {
        let words = [
            csrrci(0, 8, CSR_MSTATUS),
            addi(10, 10, 1),
            csrrsi(0, 8, CSR_MSTATUS),
        ];
        assert_eq!(match_critical_section(&ops(&words)), None);
    }

    #[test]
    fn a_branch_in_the_body_breaks_the_unit() {
        let words = [
            csrrci(0, 8, CSR_MSTATUS),
            sw(10, 11, 0),
            beq(10, 11),
            csrrsi(0, 8, CSR_MSTATUS),
        ];
        assert_eq!(match_critical_section(&ops(&words)), None);
    }

    #[test]
    fn other_csrs_and_other_bits_are_not_the_mie_toggle() {
        let wrong_csr = [csrrci(0, 8, 0x304), sw(10, 11, 0), csrrsi(0, 8, 0x304)];
        assert_eq!(match_critical_section(&ops(&wrong_csr)), None);
        let wrong_bit = [
            csrrci(0, 2, CSR_MSTATUS),
            sw(10, 11, 0),
            csrrsi(0, 2, CSR_MSTATUS),
        ];
        assert_eq!(match_critical_section(&ops(&wrong_bit)), None);
    }

    #[test]
    fn the_body_bound_is_inclusive_and_one_more_op_rejects() {
        let mut at_bound = vec![csrrci(0, 8, CSR_MSTATUS), sw(10, 11, 0)];
        at_bound.extend(std::iter::repeat_n(addi(12, 12, 1), MAX_BODY_OPS - 1));
        at_bound.push(csrrsi(0, 8, CSR_MSTATUS));
        assert_eq!(
            match_critical_section(&ops(&at_bound)).map(|u| u.ops),
            Some(MAX_BODY_OPS + 2)
        );
        let mut over = vec![csrrci(0, 8, CSR_MSTATUS), sw(10, 11, 0)];
        over.extend(std::iter::repeat_n(addi(12, 12, 1), MAX_BODY_OPS));
        over.push(csrrsi(0, 8, CSR_MSTATUS));
        assert_eq!(match_critical_section(&ops(&over)), None);
    }

    #[test]
    fn a_register_overwritten_after_lui_forgets_its_constant() {
        let words = [
            csrrci(0, 8, CSR_MSTATUS),
            lui(14, 0x600C_2000),
            addi(14, 13, 0),
            sw(10, 14, 0x194),
            csrrsi(0, 8, CSR_MSTATUS),
        ];
        assert!(
            !match_critical_section(&ops(&words))
                .unwrap()
                .writes_threshold()
        );
    }

    #[test]
    fn a_register_form_restore_derived_from_the_saved_mstatus_is_a_unit() {
        // The IDF port's shape: restore through a register derived from the saved mstatus.
        let words = [
            csrrci(15, 8, CSR_MSTATUS),
            lui(14, 0x600C_2000),
            sw(10, 14, 0x194),
            andi(15, 15, 8),
            csrrs(0, 15, CSR_MSTATUS),
        ];
        let unit = match_critical_section(&ops(&words)).expect("register-form restore");
        assert_eq!(unit.ops, 5);
        assert_eq!(unit.restore, Restore::Register);
        assert!(unit.writes_threshold());
    }

    #[test]
    fn a_register_form_restore_from_an_unrelated_or_clobbered_register_is_not_a_unit() {
        let unrelated = [
            csrrci(15, 8, CSR_MSTATUS),
            sw(10, 11, 0),
            csrrs(0, 13, CSR_MSTATUS),
        ];
        assert_eq!(match_critical_section(&ops(&unrelated)), None);
        let clobbered = [
            csrrci(15, 8, CSR_MSTATUS),
            sw(10, 11, 0),
            addi(15, 13, 0),
            csrrs(0, 15, CSR_MSTATUS),
        ];
        assert_eq!(match_critical_section(&ops(&clobbered)), None);
        let from_x0 = [
            csrrci(0, 8, CSR_MSTATUS),
            sw(10, 11, 0),
            csrrs(0, 0, CSR_MSTATUS),
        ];
        assert_eq!(match_critical_section(&ops(&from_x0)), None);
    }

    #[test]
    fn a_threshold_store_followed_by_another_store_still_writes_the_threshold() {
        let words = [
            csrrci(15, 8, CSR_MSTATUS),
            lui(14, 0x600C_2000),
            sw(10, 14, 0x194),
            sw(10, 11, 0),
            csrrsi(0, 8, CSR_MSTATUS),
        ];
        assert!(
            match_critical_section(&ops(&words))
                .unwrap()
                .writes_threshold()
        );
    }

    #[test]
    fn the_image_scan_counts_non_overlapping_units() {
        let mut words = thresh_sequence();
        words.push(addi(10, 10, 1));
        words.extend([
            csrrci(0, 8, CSR_MSTATUS),
            sw(10, 11, 0),
            csrrsi(0, 8, CSR_MSTATUS),
        ]);
        let expected = SiteCount {
            units: 2,
            threshold_units: 1,
            candidates: 2,
        };
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(scan_image(&bytes, 0x4200_0000), expected);
        assert_eq!(scan_image(&bytes[..bytes.len() - 4], 0x4200_0000).units, 1);
    }
}
