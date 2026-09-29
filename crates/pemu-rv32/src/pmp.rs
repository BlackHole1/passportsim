//! Physical memory protection rules of the ESP32-C3 hart (ESP32-C3 TRM chapter 1, RISC-V
//! Privileged Specification "Physical Memory Protection").
//!
//! Granularity is 0, since IDF programs entry 15 as NA4 (IDF
//! `esp_hw_support/port/esp32c3/cpu_region_protect.c:106-107`); UNVERIFIED on silicon. Overlaps
//! follow the lowest-entry rule; UNVERIFIED on the C3, whose IDF never overlaps entries.

use crate::csr::Csr;

pub const PMP_ENTRIES: usize = 16;
pub const PMPCFG_CSRS: usize = 4;
/// CSR numbers of `pmpcfg0` and `pmpaddr0` (IDF `riscv/include/riscv/csr.h:92-96`).
pub const CSR_PMPCFG0: u16 = 0x3A0;
pub const CSR_PMPADDR0: u16 = 0x3B0;

/// Configuration byte fields (IDF `riscv/include/riscv/encoding.h:172-181`).
pub const PMP_R: u8 = 0x01;
pub const PMP_W: u8 = 0x02;
pub const PMP_X: u8 = 0x04;
pub const PMP_A: u8 = 0x18;
/// Locks the entry until reset and enforces it in machine mode.
pub const PMP_L: u8 = 0x80;
pub const PMP_TOR: u8 = 0x08;
pub const PMP_NA4: u8 = 0x10;
pub const PMP_NAPOT: u8 = 0x18;
const CFG_WRITABLE: u8 = PMP_R | PMP_W | PMP_X | PMP_A | PMP_L;

const ADDR_SPACE_END: u64 = 1 << 32;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AddrMatch {
    Off,
    /// Top of range: the previous address register is the lower bound.
    Tor,
    Na4,
    Napot,
}

/// [`Perm::bits`] equals the configuration byte bits and the page flags `PF_R`, `PF_W`, `PF_X`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Perm {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl Perm {
    pub const NONE: Perm = Perm {
        read: false,
        write: false,
        execute: false,
    };
    pub const RWX: Perm = Perm {
        read: true,
        write: true,
        execute: true,
    };

    pub const fn from_bits(bits: u8) -> Perm {
        Perm {
            read: bits & PMP_R != 0,
            write: bits & PMP_W != 0,
            execute: bits & PMP_X != 0,
        }
    }

    pub const fn bits(self) -> u8 {
        (self.read as u8) | (self.write as u8) << 1 | (self.execute as u8) << 2
    }

    pub const fn allows(self, kind: AccessKind) -> bool {
        match kind {
            AccessKind::Read => self.read,
            AccessKind::Write => self.write,
            AccessKind::Execute => self.execute,
        }
    }
}

/// A denial raises mcause 5 (load), 7 (store) or 1 (fetch); the caller raises it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
    Execute,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Privilege {
    /// Machine mode: only locked entries restrict it, and no match allows the access.
    Machine,
    /// Any mode below machine mode: every entry restricts it, and no match denies the access.
    User,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PmpCfg {
    pub matching: AddrMatch,
    pub perm: Perm,
    pub locked: bool,
}

impl PmpCfg {
    pub const fn decode(byte: u8) -> PmpCfg {
        let matching = match byte & PMP_A {
            0 => AddrMatch::Off,
            PMP_TOR => AddrMatch::Tor,
            PMP_NA4 => AddrMatch::Na4,
            _ => AddrMatch::Napot,
        };
        PmpCfg {
            matching,
            perm: Perm::from_bits(byte),
            locked: byte & PMP_L != 0,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PmpRegion {
    /// Lower numbers take priority.
    pub index: usize,
    /// Never [`AddrMatch::Off`].
    pub matching: AddrMatch,
    pub start: u64,
    /// Exclusive, clipped to the 32-bit address space, so `start < end <= 1 << 32`.
    pub end: u64,
    pub perm: Perm,
    pub locked: bool,
}

/// Value of CSR `pmpcfgN`, entry `4n` in the low byte.
pub fn read_pmpcfg(cfg: &[u8; PMP_ENTRIES], n: usize) -> u32 {
    assert!(n < PMPCFG_CSRS, "pmpcfg{n} is not implemented on the C3");
    let base = 4 * n;
    u32::from_le_bytes([cfg[base], cfg[base + 1], cfg[base + 2], cfg[base + 3]])
}

/// Writes CSR `pmpcfgN`, skipping locked bytes; returns whether page permissions need a refold.
pub fn write_pmpcfg(cfg: &mut [u8; PMP_ENTRIES], n: usize, value: u32) -> bool {
    assert!(n < PMPCFG_CSRS, "pmpcfg{n} is not implemented on the C3");
    let mut changed = false;
    for (k, &byte) in value.to_le_bytes().iter().enumerate() {
        let slot = &mut cfg[4 * n + k];
        if *slot & PMP_L != 0 {
            continue;
        }
        let legal = legalize_cfg(byte);
        changed |= *slot != legal;
        *slot = legal;
    }
    changed
}

/// Clears reserved bits 6:5, and W when R is clear (RISC-V Privileged Specification).
/// UNVERIFIED on C3 silicon: IDF never writes either pattern.
const fn legalize_cfg(byte: u8) -> u8 {
    let byte = byte & CFG_WRITABLE;
    if byte & (PMP_R | PMP_W) == PMP_W {
        byte & !PMP_W
    } else {
        byte
    }
}

pub fn read_pmpaddr(addr: &[u32; PMP_ENTRIES], i: usize) -> u32 {
    assert!(i < PMP_ENTRIES, "pmpaddr{i} is not implemented on the C3");
    addr[i]
}

/// Writes CSR `pmpaddrI`; returns whether the value changed. A locked TOR entry `i + 1` also
/// locks `pmpaddr[i]`, its lower bound.
pub fn write_pmpaddr(
    cfg: &[u8; PMP_ENTRIES],
    addr: &mut [u32; PMP_ENTRIES],
    i: usize,
    value: u32,
) -> bool {
    assert!(i < PMP_ENTRIES, "pmpaddr{i} is not implemented on the C3");
    if pmpaddr_locked(cfg, i) {
        return false;
    }
    let changed = addr[i] != value;
    addr[i] = value;
    changed
}

pub fn pmpaddr_locked(cfg: &[u8; PMP_ENTRIES], i: usize) -> bool {
    assert!(i < PMP_ENTRIES, "pmpaddr{i} is not implemented on the C3");
    let own = cfg[i] & PMP_L != 0;
    let next_tor = cfg
        .get(i + 1)
        .is_some_and(|&next| next & PMP_L != 0 && next & PMP_A == PMP_TOR);
    own || next_tor
}

#[derive(Copy, Clone, Debug)]
pub struct Pmp<'a> {
    cfg: &'a [u8; PMP_ENTRIES],
    addr: &'a [u32; PMP_ENTRIES],
}

impl<'a> Pmp<'a> {
    pub fn new(cfg: &'a [u8; PMP_ENTRIES], addr: &'a [u32; PMP_ENTRIES]) -> Self {
        Pmp { cfg, addr }
    }

    pub fn from_csr(csr: &'a Csr) -> Self {
        Pmp::new(&csr.pmpcfg, &csr.pmpaddr)
    }

    pub fn cfg(&self, i: usize) -> PmpCfg {
        PmpCfg::decode(self.cfg[i])
    }

    /// Entry `i` as an address range, or `None` when it matches nothing below 4 GB.
    pub fn region(&self, i: usize) -> Option<PmpRegion> {
        let cfg = self.cfg(i);
        let here = u64::from(self.addr[i]);
        let (start, end) = match cfg.matching {
            AddrMatch::Off => return None,
            AddrMatch::Tor => {
                let below = if i == 0 {
                    0
                } else {
                    u64::from(self.addr[i - 1])
                };
                (below << 2, here << 2)
            }
            AddrMatch::Na4 => (here << 2, (here << 2) + 4),
            AddrMatch::Napot => {
                // t trailing ones give 2^(t+3) bytes; they and the 0 above them are not base bits.
                let ones = self.addr[i].trailing_ones();
                let base = (here & !((1u64 << (ones + 1)) - 1)) << 2;
                (base, base + (1u64 << (ones + 3)))
            }
        };
        let end = end.min(ADDR_SPACE_END);
        (start < end).then_some(PmpRegion {
            index: i,
            matching: cfg.matching,
            start,
            end,
            perm: cfg.perm,
            locked: cfg.locked,
        })
    }

    /// Lowest index (highest priority) first.
    pub fn regions(&self) -> impl Iterator<Item = PmpRegion> + 'a {
        let view = *self;
        (0..PMP_ENTRIES).filter_map(move |i| view.region(i))
    }

    /// The lowest entry matching any byte decides; if it does not cover every byte, the access
    /// fails in every mode. A `len` of 0 counts as 1.
    pub fn check(&self, start: u32, len: u32, kind: AccessKind, privilege: Privilege) -> bool {
        match self.decide(start, len) {
            Decision::NoMatch => privilege == Privilege::Machine,
            Decision::Partial => false,
            Decision::Full(region) => effective_perm(&region, privilege).allows(kind),
        }
    }

    /// Permissions for the whole range, or `None` when it crosses its deciding entry's boundary
    /// (the SoC sends such pages to the slow path).
    pub fn range_perm(&self, start: u32, len: u32, privilege: Privilege) -> Option<Perm> {
        match self.decide(start, len) {
            Decision::NoMatch if privilege == Privilege::Machine => Some(Perm::RWX),
            Decision::NoMatch => Some(Perm::NONE),
            Decision::Partial => None,
            Decision::Full(region) => Some(effective_perm(&region, privilege)),
        }
    }

    fn decide(&self, start: u32, len: u32) -> Decision {
        let first = u64::from(start);
        let end = first + u64::from(len.max(1));
        match self.regions().find(|r| first < r.end && r.start < end) {
            None => Decision::NoMatch,
            Some(r) if r.start <= first && end <= r.end => Decision::Full(r),
            Some(_) => Decision::Partial,
        }
    }
}

enum Decision {
    NoMatch,
    Partial,
    Full(PmpRegion),
}

fn effective_perm(region: &PmpRegion, privilege: Privilege) -> Perm {
    if privilege == Privilege::Machine && !region.locked {
        Perm::RWX
    } else {
        region.perm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use AccessKind::{Execute, Read, Write};
    use Privilege::{Machine, User};

    #[derive(Default)]
    struct State {
        cfg: [u8; PMP_ENTRIES],
        addr: [u32; PMP_ENTRIES],
    }

    impl State {
        fn view(&self) -> Pmp<'_> {
            Pmp::new(&self.cfg, &self.addr)
        }

        fn set_cfg(&mut self, i: usize, byte: u8) -> bool {
            let n = i / 4;
            let shift = (i % 4) * 8;
            let old = read_pmpcfg(&self.cfg, n);
            write_pmpcfg(
                &mut self.cfg,
                n,
                (old & !(0xFF << shift)) | u32::from(byte) << shift,
            )
        }

        fn set_addr(&mut self, i: usize, value: u32) -> bool {
            write_pmpaddr(&self.cfg, &mut self.addr, i, value)
        }

        /// IDF `PMP_ENTRY_SET` (`riscv/include/riscv/csr.h:124-127`): address, then OR in the cfg.
        fn idf_entry_set(&mut self, i: usize, addr: u32, cfg: u8) {
            self.set_addr(i, addr >> 2);
            let n = i / 4;
            let old = read_pmpcfg(&self.cfg, n);
            write_pmpcfg(&mut self.cfg, n, old | u32::from(cfg) << ((i % 4) * 8));
        }

        fn check(&self, start: u32, len: u32, kind: AccessKind, privilege: Privilege) -> bool {
            self.view().check(start, len, kind, privilege)
        }
    }

    /// IDF `PMPADDR_NAPOT(START, END) >> PMP_SHIFT` (`riscv/include/riscv/csr.h:107-111`), in
    /// 64 bits so that a 4 GB range works.
    fn napot(start: u64, size: u64) -> u32 {
        assert!(size.is_power_of_two() && size >= 8 && start.is_multiple_of(size));
        ((start | ((size - 1) >> 1)) >> 2) as u32
    }

    #[test]
    fn tor_entry_0_starts_at_address_0() {
        let mut s = State::default();
        s.set_addr(0, 0x1000 >> 2);
        s.set_cfg(0, PMP_TOR | PMP_R);
        let region = s.view().region(0).unwrap();
        assert_eq!(
            (region.start, region.end, region.matching),
            (0, 0x1000, AddrMatch::Tor)
        );
        assert!(s.check(0, 1, Read, User));
        assert!(s.check(0xFFC, 4, Read, User));
        assert!(!s.check(0, 4, Write, User));
        assert!(!s.check(0x800, 1, Execute, User));
        assert!(!s.check(0x1000, 1, Read, User));
        assert!(s.check(0x1000, 1, Write, Machine));
    }

    #[test]
    fn tor_takes_its_lower_bound_from_the_previous_address() {
        let mut s = State::default();
        // Entry 0 is OFF but its address still bounds entry 1.
        s.set_addr(0, 0x1000 >> 2);
        s.set_addr(1, 0x2000 >> 2);
        s.set_cfg(1, PMP_TOR | PMP_R | PMP_W);
        assert_eq!(s.view().region(0), None);
        let region = s.view().region(1).unwrap();
        assert_eq!((region.start, region.end), (0x1000, 0x2000));
        assert!(!s.check(0xFFF, 1, Read, User));
        assert!(s.check(0x1000, 0x1000, Write, User));
        assert!(!s.check(0x2000, 1, Read, User));
        // A lower bound at or above the upper bound matches nothing.
        s.set_addr(0, 0x2000 >> 2);
        assert_eq!(s.view().region(1), None);
        s.set_addr(0, 0x3000 >> 2);
        assert_eq!(s.view().region(1), None);
        assert!(!s.check(0x2800, 1, Read, User));
        s.set_cfg(0, PMP_TOR | PMP_R);
        s.set_addr(0, 0);
        assert_eq!(s.view().region(0), None);
    }

    #[test]
    fn napot_sizes_from_8_bytes_to_4_gb() {
        for bits in 3..=32u32 {
            let size = 1u64 << bits;
            let start = 0x9ABC_DEF8u64 & !(size - 1);
            let mut s = State::default();
            s.set_addr(5, napot(start, size));
            s.set_cfg(5, PMP_NAPOT | PMP_R | PMP_X);
            let region = s.view().region(5).unwrap();
            assert_eq!(
                (region.start, region.end),
                (start, start + size),
                "size 2^{bits}"
            );
            let last = (start + size - 1) as u32;
            assert!(s.check(start as u32, 1, Read, User), "size 2^{bits}");
            assert!(s.check(last, 1, Execute, User), "size 2^{bits}");
            assert!(!s.check(last, 1, Write, User), "size 2^{bits}");
            if bits < 32 {
                assert!(
                    s.check(start as u32, size as u32, Read, User),
                    "size 2^{bits}"
                );
                assert!(!s.check(start as u32 - 1, 1, Read, User), "size 2^{bits}");
                if let Some(after) = last.checked_add(1) {
                    assert!(!s.check(after, 1, Read, User), "size 2^{bits}");
                }
            } else {
                assert!(s.check(0, u32::MAX, Read, User));
            }
        }
    }

    #[test]
    fn napot_base_keeps_the_bits_above_the_size() {
        let mut s = State::default();
        // pmpaddr 0b10: no trailing ones, 8 bytes at 8 (bit 3 of the base is set).
        s.set_addr(0, napot(8, 8));
        assert_eq!(s.addr[0], 0b10);
        s.set_cfg(0, PMP_NAPOT | PMP_R);
        let region = s.view().region(0).unwrap();
        assert_eq!((region.start, region.end), (8, 16));
        s.set_addr(1, napot(0x3FC8_0000, 0x2_0000));
        s.set_cfg(1, PMP_NAPOT | PMP_R);
        let region = s.view().region(1).unwrap();
        assert_eq!((region.start, region.end), (0x3FC8_0000, 0x3FCA_0000));
    }

    #[test]
    fn napot_ranges_above_4_gb_are_clipped() {
        let mut s = State::default();
        // IDF PMPADDR_ALL (all ones): 2^35 bytes from 0, clipped to the 32-bit space.
        s.set_addr(0, 0xFFFF_FFFF);
        s.set_cfg(0, PMP_NAPOT | PMP_R);
        let region = s.view().region(0).unwrap();
        assert_eq!((region.start, region.end), (0, 1 << 32));
        assert!(s.check(0xFFFF_FFFC, 4, Read, User));
        // 8 GB from 0 also clips; 2 GB above 4 GB matches nothing.
        s.set_addr(0, 0x3FFF_FFFF);
        assert_eq!(s.view().region(0).map(|r| r.end), Some(1 << 32));
        s.set_addr(0, 0x5FFF_FFFF);
        assert_eq!(s.view().region(0), None);
    }

    #[test]
    fn na4_covers_four_bytes() {
        let mut s = State::default();
        s.set_addr(3, 0x3FC8_0010 >> 2);
        s.set_cfg(3, PMP_NA4 | PMP_R | PMP_W);
        let region = s.view().region(3).unwrap();
        assert_eq!(
            (region.start, region.end, region.matching),
            (0x3FC8_0010, 0x3FC8_0014, AddrMatch::Na4)
        );
        assert!(s.check(0x3FC8_0010, 4, Write, User));
        assert!(s.check(0x3FC8_0013, 1, Read, User));
        assert!(!s.check(0x3FC8_0014, 1, Read, User));
        assert!(!s.check(0x3FC8_000F, 1, Read, User));
        // Granularity 0: every address bit is stored and reads back.
        s.set_addr(4, 0xFFFF_FFFF);
        assert_eq!(read_pmpaddr(&s.addr, 4), 0xFFFF_FFFF);
        s.set_addr(3, 0x4000_0000);
        assert_eq!(s.view().region(3), None);
    }

    #[test]
    fn pmpcfg_packs_entries_little_endian() {
        let mut s = State::default();
        assert!(write_pmpcfg(&mut s.cfg, 2, 0x1F1B_0D09));
        assert_eq!(s.cfg[8..12], [0x09, 0x0D, 0x1B, 0x1F]);
        assert_eq!(read_pmpcfg(&s.cfg, 2), 0x1F1B_0D09);
        assert_eq!(read_pmpcfg(&s.cfg, 0), 0);
        assert!(!write_pmpcfg(&mut s.cfg, 2, 0x1F1B_0D09));
        assert!(s.set_addr(9, 0x1234));
        assert!(!s.set_addr(9, 0x1234));
    }

    #[test]
    #[should_panic(expected = "pmpcfg4")]
    fn pmpcfg_beyond_the_c3_entries_panics() {
        let mut s = State::default();
        write_pmpcfg(&mut s.cfg, 4, 0);
    }

    #[test]
    #[should_panic(expected = "pmpaddr16 is not implemented on the C3")]
    fn read_pmpaddr_beyond_the_c3_entries_panics() {
        let s = State::default();
        read_pmpaddr(&s.addr, PMP_ENTRIES);
    }

    #[test]
    #[should_panic(expected = "pmpaddr16 is not implemented on the C3")]
    fn pmpaddr_locked_beyond_the_c3_entries_panics() {
        let s = State::default();
        pmpaddr_locked(&s.cfg, PMP_ENTRIES);
    }

    #[test]
    fn reserved_bits_and_w_without_r_are_legalized() {
        let mut s = State::default();
        s.set_cfg(0, 0x60 | PMP_NAPOT | PMP_R);
        assert_eq!(s.cfg[0], PMP_NAPOT | PMP_R);
        s.set_cfg(1, PMP_NAPOT | PMP_W | PMP_X);
        assert_eq!(s.cfg[1], PMP_NAPOT | PMP_X);
        assert_eq!(
            s.view().cfg(1).perm,
            Perm {
                read: false,
                write: false,
                execute: true
            }
        );
        s.set_cfg(2, PMP_TOR | PMP_W);
        assert_eq!(s.cfg[2], PMP_TOR);
        s.set_cfg(3, PMP_NA4 | PMP_R | PMP_W | PMP_L);
        assert_eq!(s.cfg[3], PMP_NA4 | PMP_R | PMP_W | PMP_L);
        s.set_addr(4, napot(0x1_0000, 0x1000));
        s.set_cfg(4, PMP_NAPOT | PMP_W | PMP_L);
        assert!(!s.check(0x1_0000, 4, Write, User));
        assert!(!s.check(0x1_0000, 4, Write, Machine));
    }

    #[test]
    fn locked_entry_ignores_cfg_writes() {
        let mut s = State::default();
        s.set_addr(1, napot(0x1000, 0x1000));
        assert!(write_pmpcfg(
            &mut s.cfg,
            0,
            u32::from(PMP_L | PMP_NAPOT | PMP_R) << 8
        ));
        // The lock cannot be cleared and the other bytes of the CSR stay writable.
        assert!(write_pmpcfg(&mut s.cfg, 0, 0x0000_1F0F));
        assert_eq!(s.cfg[0..4], [0x0F, PMP_L | PMP_NAPOT | PMP_R, 0, 0]);
        assert!(!s.set_cfg(1, 0));
        assert!(!s.set_cfg(1, PMP_NAPOT | PMP_R | PMP_W | PMP_X));
        assert_eq!(s.cfg[1], PMP_L | PMP_NAPOT | PMP_R);
        assert!(s.view().cfg(1).locked);
    }

    #[test]
    fn locked_entry_ignores_address_writes() {
        let mut s = State::default();
        s.set_addr(6, napot(0x4000, 0x100));
        s.set_cfg(6, PMP_L | PMP_NAPOT | PMP_R);
        assert!(pmpaddr_locked(&s.cfg, 6));
        assert!(!s.set_addr(6, 0));
        assert_eq!(read_pmpaddr(&s.addr, 6), napot(0x4000, 0x100));
        // A locked NAPOT entry does not lock the address below it.
        assert!(!pmpaddr_locked(&s.cfg, 5));
        assert!(s.set_addr(5, 0x77));
    }

    #[test]
    fn locked_tor_entry_locks_the_previous_address() {
        let mut s = State::default();
        s.set_addr(2, 0x1000 >> 2);
        s.set_addr(3, 0x2000 >> 2);
        s.set_cfg(3, PMP_TOR | PMP_R);
        assert!(!pmpaddr_locked(&s.cfg, 2));
        s.set_cfg(3, PMP_L | PMP_TOR | PMP_R);
        assert!(pmpaddr_locked(&s.cfg, 2));
        assert!(!s.set_addr(2, 0));
        assert!(!s.set_addr(3, 0));
        assert_eq!((s.addr[2], s.addr[3]), (0x1000 >> 2, 0x2000 >> 2));
        // Only the address: configuration byte 2 is still writable.
        assert!(s.set_cfg(2, PMP_NAPOT | PMP_R));
        assert_eq!(
            s.view().region(3).map(|r| (r.start, r.end)),
            Some((0x1000, 0x2000))
        );
        // Entry 0 locked as TOR has no address below it; entry 15 locked as TOR locks 14.
        s.set_cfg(0, PMP_L | PMP_TOR);
        assert!(pmpaddr_locked(&s.cfg, 0));
        s.set_cfg(15, PMP_L | PMP_TOR);
        assert!(pmpaddr_locked(&s.cfg, 14));
        assert!(pmpaddr_locked(&s.cfg, 15));
    }

    #[test]
    fn partial_overlap_fails_in_every_mode() {
        let mut s = State::default();
        s.set_addr(0, napot(0x1000, 0x1000));
        for cfg in [
            PMP_NAPOT | PMP_R | PMP_W | PMP_X,
            PMP_L | PMP_NAPOT | PMP_R | PMP_W | PMP_X,
        ] {
            s.cfg[0] = cfg;
            for privilege in [Machine, User] {
                assert!(s.check(0x1000, 0x1000, Read, privilege));
                assert!(
                    !s.check(0xFFE, 4, Read, privilege),
                    "{cfg:#x} {privilege:?}"
                );
                assert!(
                    !s.check(0x1FFE, 4, Write, privilege),
                    "{cfg:#x} {privilege:?}"
                );
                assert!(
                    !s.check(0xFFF, 0x1002, Execute, privilege),
                    "{cfg:#x} {privilege:?}"
                );
            }
        }
        // An access that runs past 4 GB only partly matches an entry ending at 4 GB.
        s.set_addr(1, napot(0xFFFF_F000, 0x1000));
        s.cfg[1] = PMP_NAPOT | PMP_R;
        assert!(s.check(0xFFFF_FFFC, 4, Read, User));
        assert!(!s.check(0xFFFF_FFFE, 4, Read, User));
        assert!(!s.check(0xFFFF_FFFE, 4, Read, Machine));
    }

    #[test]
    fn lowest_numbered_matching_entry_wins() {
        let mut s = State::default();
        s.set_addr(0, napot(0x1000, 0x1000));
        s.set_cfg(0, PMP_NAPOT | PMP_R);
        s.set_addr(1, napot(0, 0x10000));
        s.set_cfg(1, PMP_NAPOT | PMP_R | PMP_W);
        assert!(!s.check(0x1000, 4, Write, User));
        assert!(s.check(0x3000, 4, Write, User));
        // Entry 1 covers the whole access, but entry 0 matches first and only partly.
        assert!(!s.check(0xFFE, 4, Read, User));
        let (cfg, addr) = (s.cfg, s.addr);
        s.cfg[..2].copy_from_slice(&[cfg[1], cfg[0]]);
        s.addr[..2].copy_from_slice(&[addr[1], addr[0]]);
        assert!(s.check(0x1000, 4, Write, User));
        assert!(s.check(0xFFE, 4, Read, User));
    }

    #[test]
    fn machine_mode_is_restricted_only_by_locked_entries() {
        let mut s = State::default();
        s.set_addr(0, 0x1FFF_FFFF);
        s.set_cfg(0, PMP_NAPOT);
        for kind in [Read, Write, Execute] {
            assert!(s.check(0x4200_0000, 4, kind, Machine));
            assert!(!s.check(0x4200_0000, 4, kind, User));
        }
        assert_eq!(
            s.view().range_perm(0x4200_0000, 0x1000, Machine),
            Some(Perm::RWX)
        );
        s.set_cfg(0, PMP_NAPOT | PMP_X);
        assert!(s.check(0, 4, Write, Machine));
        s.set_cfg(0, PMP_L | PMP_NAPOT | PMP_X);
        assert!(!s.check(0, 4, Write, Machine));
        assert!(!s.check(0, 4, Read, Machine));
        assert!(s.check(0, 4, Execute, Machine));
    }

    #[test]
    fn regions_list_active_entries_in_priority_order() {
        let mut s = State::default();
        s.set_addr(1, 0x2000 >> 2);
        s.set_cfg(1, PMP_TOR | PMP_R | PMP_X);
        s.set_addr(2, 0x3000 >> 2); // entry 2 stays OFF
        s.set_addr(4, napot(0x8000, 0x8000));
        s.set_cfg(4, PMP_L | PMP_NAPOT | PMP_R | PMP_W);
        s.set_addr(7, 0x100 >> 2);
        s.set_cfg(7, PMP_NA4);
        let regions: Vec<PmpRegion> = s.view().regions().collect();
        let region = |index, matching, start, end, bits, locked| PmpRegion {
            index,
            matching,
            start,
            end,
            perm: Perm::from_bits(bits),
            locked,
        };
        assert_eq!(
            regions,
            [
                region(1, AddrMatch::Tor, 0, 0x2000, PMP_R | PMP_X, false),
                region(4, AddrMatch::Napot, 0x8000, 0x1_0000, PMP_R | PMP_W, true),
                region(7, AddrMatch::Na4, 0x100, 0x104, 0, false),
            ]
        );
        assert_eq!(regions[1].perm.bits(), PMP_R | PMP_W);
        assert_eq!(Perm::from_bits(0xFF), Perm::RWX);
        assert_eq!(Perm::RWX.bits(), 7);
    }

    #[test]
    fn range_perm_gives_one_permission_set_or_a_partial_page() {
        let mut s = State::default();
        s.set_addr(0, napot(0x3FC8_0000, 0x2_0000));
        s.set_cfg(0, PMP_L | PMP_NAPOT | PMP_R | PMP_W);
        s.set_addr(1, 0x3FCA_0800 >> 2);
        s.set_cfg(1, PMP_NA4 | PMP_R);
        let pmp = s.view();
        let rw = Perm::from_bits(PMP_R | PMP_W);
        for privilege in [Machine, User] {
            assert_eq!(pmp.range_perm(0x3FC8_0000, 0x1000, privilege), Some(rw));
            assert_eq!(pmp.range_perm(0x3FC9_F000, 0x1000, privilege), Some(rw));
            assert_eq!(pmp.range_perm(0x3FC8_0000, 0, privilege), Some(rw));
            assert_eq!(pmp.range_perm(0x3FC7_F800, 0x1000, privilege), None);
            assert_eq!(pmp.range_perm(0x3FCA_0000, 0x1000, privilege), None);
        }
        assert_eq!(pmp.range_perm(0x3FCA_0800, 4, Machine), Some(Perm::RWX));
        assert_eq!(
            pmp.range_perm(0x3FCA_0800, 4, User),
            Some(Perm::from_bits(PMP_R))
        );
        assert_eq!(
            pmp.range_perm(0x5000_0000, 0x1000, Machine),
            Some(Perm::RWX)
        );
        assert_eq!(pmp.range_perm(0x5000_0000, 0x1000, User), Some(Perm::NONE));
    }

    /// The sixteen `PMP_ENTRY_SET` calls of IDF v5.5.3 `esp_cpu_configure_region_protection`
    /// (`cpu_region_protect.c:32-107`) with the `SOC_*` bounds written out.
    const IDF_C3_ENTRY_SET: [(u32, u8); PMP_ENTRIES] = {
        const NONE: u8 = PMP_L | PMP_TOR;
        const R: u8 = NONE | PMP_R;
        const RW: u8 = R | PMP_W;
        const RX: u8 = R | PMP_X;
        const RWX: u8 = RW | PMP_X;
        [
            (0x2000_0000, NONE),         // SOC_DEBUG_LOW: gap at the bottom
            (0x2800_0000, RWX),          // SOC_DEBUG_HIGH: debug region
            (0x3C00_0000, NONE),         // SOC_DROM_LOW: gap
            (0x3FC8_0000, R),            // SOC_DRAM_LOW: DROM and the gap after it
            (0x3FCE_0000, RW),           // SOC_DRAM_HIGH: DRAM
            (0x3FF2_0000, R),            // SOC_DROM_MASK_HIGH: gap and mask DROM
            (0x4006_0000, RX),           // SOC_IROM_MASK_HIGH: gap and mask IROM
            (0x4037_C000, NONE),         // SOC_IRAM_LOW: gap
            (0x403E_0000, RWX),          // SOC_IRAM_HIGH: IRAM
            (0x4280_0000, RX),           // SOC_IROM_HIGH: gap and IROM
            (0x5000_0000, NONE),         // SOC_RTC_IRAM_LOW: gap
            (0x5000_2000, RWX),          // SOC_RTC_IRAM_HIGH: RTC memory
            (0x6000_0000, NONE),         // SOC_PERIPHERAL_LOW: gap
            (0x6010_0000, RW),           // SOC_PERIPHERAL_HIGH: peripherals
            (u32::MAX, NONE),            // UINT32_MAX: all but the last 4 bytes
            (u32::MAX, PMP_L | PMP_NA4), // last 4 bytes
        ]
    };

    /// The ranges and permissions IDF intends, per the comments of `cpu_region_protect.c:38-107`.
    const IDF_C3_INTENDED: [(u64, u64, u8); PMP_ENTRIES] = [
        (0x0000_0000, 0x2000_0000, 0),
        (0x2000_0000, 0x2800_0000, 7),
        (0x2800_0000, 0x3C00_0000, 0),
        (0x3C00_0000, 0x3FC8_0000, 1),
        (0x3FC8_0000, 0x3FCE_0000, 3),
        (0x3FCE_0000, 0x3FF2_0000, 1),
        (0x3FF2_0000, 0x4006_0000, 5),
        (0x4006_0000, 0x4037_C000, 0),
        (0x4037_C000, 0x403E_0000, 7),
        (0x403E_0000, 0x4280_0000, 5),
        (0x4280_0000, 0x5000_0000, 0),
        (0x5000_0000, 0x5000_2000, 7),
        (0x5000_2000, 0x6000_0000, 0),
        (0x6000_0000, 0x6010_0000, 3),
        (0x6010_0000, 0xFFFF_FFFC, 0),
        (0xFFFF_FFFC, 0x1_0000_0000, 0),
    ];

    #[test]
    fn idf_c3_region_protection_layout_gives_the_intended_permissions() {
        let mut s = State::default();
        for (i, &(addr, cfg)) in IDF_C3_ENTRY_SET.iter().enumerate() {
            s.idf_entry_set(i, addr, cfg);
        }
        // Every write landed: IDF writes an address before locking the entry above it.
        for (i, &(addr, cfg)) in IDF_C3_ENTRY_SET.iter().enumerate() {
            assert_eq!((s.addr[i], s.cfg[i]), (addr >> 2, cfg), "entry {i}");
        }
        let regions: Vec<PmpRegion> = s.view().regions().collect();
        assert_eq!(regions.len(), PMP_ENTRIES);
        for (region, &(start, end, bits)) in regions.iter().zip(&IDF_C3_INTENDED) {
            let expect = (start, end, Perm::from_bits(bits), true);
            assert_eq!(
                (region.start, region.end, region.perm, region.locked),
                expect
            );
            let len = (end - start) as u32;
            for privilege in [Machine, User] {
                let perm = s.view().range_perm(start as u32, len, privilege);
                assert_eq!(
                    perm,
                    Some(Perm::from_bits(bits)),
                    "{start:#x} {privilege:?}"
                );
                for kind in [Read, Write, Execute] {
                    let allowed = Perm::from_bits(bits).allows(kind);
                    assert_eq!(s.check(start as u32, 1, kind, privilege), allowed);
                    assert_eq!(s.check((end - 1) as u32, 1, kind, privilege), allowed);
                }
            }
        }
        assert!(!s.check(0, 4, Read, Machine), "null pointer");
        assert!(
            !s.check(0x3C00_0000, 4, Write, Machine),
            "store to flash DROM"
        );
        assert!(
            !s.check(0x3FC8_0000, 2, Execute, Machine),
            "fetch from DRAM"
        );
        assert!(
            s.check(0x4000_0000, 2, Execute, Machine),
            "fetch from mask ROM"
        );
        assert!(s.check(0x4038_0000, 4, Execute, Machine), "fetch from IRAM");
        assert!(s.check(0x6000_0000, 4, Write, Machine), "store to UART0");
        assert!(
            !s.check(0x6000_0000, 2, Execute, Machine),
            "fetch from a peripheral"
        );
        assert!(
            !s.check(0x3FCD_FFFE, 4, Read, Machine),
            "load across the DRAM end"
        );
        assert_eq!(s.view().range_perm(0x3FCD_F000, 0x2000, Machine), None);
        let before = (s.cfg, s.addr);
        for (i, &(addr, cfg)) in IDF_C3_ENTRY_SET.iter().enumerate() {
            assert!(!s.set_addr(i, addr >> 3));
            assert!(!s.set_cfg(i, cfg & !PMP_L));
        }
        assert_eq!((s.cfg, s.addr), before);
    }
}
