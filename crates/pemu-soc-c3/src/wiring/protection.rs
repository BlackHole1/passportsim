//! `Wiring::ProtectionChanged`: fold PMP and the SENSITIVE PMS split into the page table
//! (`specs/blocks/sensitive.toml`).
//!
//! Both are programmed once at boot and locked, so checking them per access would pay for a
//! decision that almost never changes. Each 4 KB page gets the permissions that hold for every
//! byte of it; a page where they differ gets `PF_SLOW`, and the slow path asks
//! [`Protection::byte_flags`] from the state [`crate::Soc`] keeps of the last fold.
//!
//! On the official image the PMS split (`_iram_end` 0x4039D600, `_data_start` 0x3FC9D600, one
//! byte of SRAM1 through two views) falls inside one page, so exactly those two page views are
//! slow.
//!
//! Not modeled: the memprot interrupt (sources 56 and 57) a PMS violation raises on silicon; a
//! PMS denial is refused as a synchronous fault instead (UNVERIFIED, and unreachable while
//! [`crate::Soc::pms`] stays [`Pms::OPEN`]). The `SPLITADDR` encoding is UNVERIFIED, which is why
//! [`Pms::split_at`] takes a decoded address. Watchpoint and var-matcher `PF_SLOW` marks are kept
//! by the SoC across a refold.

use pemu_rv32::csr::Csr;
use pemu_rv32::pmp::{AccessKind, PMP_ENTRIES, Perm, Pmp, Privilege};

use crate::Soc;
use crate::mem::{self, Region, SRAM1_DRAM_BASE, SRAM1_IRAM_BASE, SRAM1_LEN};
use crate::pagetable::{self, PAGE_SIZE, PF_CODE, PF_COLD, PF_SLOW, PF_X};

/// The PMS split of SRAM1: one offset separating the IRAM code part from the DRAM data part.
///
/// `esp_mprot_set_prot` denies a fetch from the data part through the IRAM view and a write into
/// the code part through the DRAM view: the W^X rule, and all this type encodes.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Pms {
    /// Byte offset of the split inside SRAM1, or `None` while the monitors are off.
    split: Option<u32>,
}

/// Where one address sits with respect to the PMS split.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Part {
    /// Not in SRAM1: PMS says nothing about it here.
    Outside,
    /// The code part, through the IRAM view.
    IramCode,
    /// The data part, through the IRAM view.
    IramData,
    /// The code part, through the DRAM view.
    DramCode,
    /// The data part, through the DRAM view.
    DramData,
}

impl Part {
    /// Permissions PMS grants this part: IRAM0 regions 0-2 RX, region 3 none; DRAM0 region 0
    /// none, regions 1-3 RW.
    const fn perm(self) -> Perm {
        match self {
            Part::Outside => Perm::RWX,
            Part::IramCode => Perm {
                read: true,
                write: false,
                execute: true,
            },
            Part::DramData => Perm {
                read: true,
                write: true,
                execute: false,
            },
            Part::IramData | Part::DramCode => Perm::NONE,
        }
    }
}

impl Pms {
    /// The monitors off, from reset until `esp_mprot_set_prot` runs.
    pub const OPEN: Pms = Pms { split: None };

    /// The split at `addr`, given through either SRAM1 view, or `None` outside SRAM1.
    pub fn split_at(addr: u32) -> Option<Pms> {
        let off = if addr >= SRAM1_IRAM_BASE && addr - SRAM1_IRAM_BASE <= SRAM1_LEN {
            addr - SRAM1_IRAM_BASE
        } else if addr >= SRAM1_DRAM_BASE && addr - SRAM1_DRAM_BASE <= SRAM1_LEN {
            addr - SRAM1_DRAM_BASE
        } else {
            return None;
        };
        Some(Pms { split: Some(off) })
    }

    pub fn split_offset(self) -> Option<u32> {
        self.split
    }

    /// The split as an address of the IRAM view (`_iram_end`).
    pub fn iram_split(self) -> Option<u32> {
        self.split.map(|off| SRAM1_IRAM_BASE + off)
    }

    /// The split as an address of the DRAM view (`_data_start`).
    pub fn dram_split(self) -> Option<u32> {
        self.split.map(|off| SRAM1_DRAM_BASE + off)
    }

    fn part_of(self, addr: u32) -> Part {
        let Some(split) = self.split else {
            return Part::Outside;
        };
        let (base, code, data) = if addr >= SRAM1_IRAM_BASE && addr - SRAM1_IRAM_BASE < SRAM1_LEN {
            (SRAM1_IRAM_BASE, Part::IramCode, Part::IramData)
        } else if addr >= SRAM1_DRAM_BASE && addr - SRAM1_DRAM_BASE < SRAM1_LEN {
            (SRAM1_DRAM_BASE, Part::DramCode, Part::DramData)
        } else {
            return Part::Outside;
        };
        if addr - base < split { code } else { data }
    }

    /// The permissions PMS grants every byte of `[start, start + len)`, or `None` when the range
    /// contains a boundary of the split. A `len` of 0 counts as one byte.
    ///
    /// Comparing the two ends is not enough: a range from below SRAM1 to above it has both ends
    /// outside and still covers both parts.
    pub fn range_perm(self, start: u32, len: u32) -> Option<Perm> {
        let Some(split) = self.split else {
            return Some(Part::Outside.perm());
        };
        let last = start.checked_add(len.max(1) - 1)?;
        let first_part = self.part_of(start);
        if first_part != self.part_of(last) {
            return None;
        }
        let crosses = [SRAM1_IRAM_BASE, SRAM1_DRAM_BASE].into_iter().any(|base| {
            [base, base + split, base + SRAM1_LEN]
                .into_iter()
                .any(|edge| edge > start && edge <= last)
        });
        (!crosses).then(|| first_part.perm())
    }
}

/// The PMP entries as the hart last wrote them. [`crate::Soc`] keeps them because a `PF_SLOW`
/// page is decided byte by byte long after `Bus::pmp_changed(&Csr)` returned.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct PmpState {
    /// `pmpcfg0` to `pmpcfg15`, one configuration byte per entry.
    cfg: [u8; PMP_ENTRIES],
    /// `pmpaddr0` to `pmpaddr15`, each holding address bits 33:2.
    addr: [u32; PMP_ENTRIES],
}

impl PmpState {
    /// The reset state: every entry `A = OFF`, so machine mode is allowed everything.
    pub const RESET: PmpState = PmpState {
        cfg: [0; PMP_ENTRIES],
        addr: [0; PMP_ENTRIES],
    };

    pub fn from_csr(csr: &Csr) -> PmpState {
        PmpState {
            cfg: csr.pmpcfg,
            addr: csr.pmpaddr,
        }
    }

    pub fn view(&self) -> Pmp<'_> {
        Pmp::new(&self.cfg, &self.addr)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Applied {
    pub changed: usize,
    /// Pages left on the slow path because a boundary falls inside them.
    pub slow: usize,
    /// Pages whose translated blocks the engine must drop because their execute permission
    /// changed.
    pub invalidated: usize,
}

/// PMP and the PMS split together, as the page table sees them.
///
/// Folded for machine mode only: IDF runs everything in machine mode. A guest that dropped to
/// user mode would need a second fold (UNVERIFIED that none ever will).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Protection {
    pmp: PmpState,
    pms: Pms,
}

impl Protection {
    pub fn new(pmp: PmpState, pms: Pms) -> Protection {
        Protection { pmp, pms }
    }

    pub fn from_csr(csr: &Csr, pms: Pms) -> Protection {
        Protection::new(PmpState::from_csr(csr), pms)
    }

    pub fn from_state(pmp: &PmpState, pms: Pms) -> Protection {
        Protection::new(*pmp, pms)
    }

    pub fn pms(&self) -> Pms {
        self.pms
    }

    pub fn pmp(&self) -> PmpState {
        self.pmp
    }

    /// The permissions every byte of `[start, start + len)` gets, or `None` when a PMP entry
    /// boundary or the PMS split falls inside the range.
    pub fn range_perm(&self, start: u32, len: u32) -> Option<Perm> {
        let pmp = self.pmp.view().range_perm(start, len, Privilege::Machine)?;
        let pms = self.pms.range_perm(start, len)?;
        Some(Perm::from_bits(pmp.bits() & pms.bits()))
    }

    /// Whether an access of `size` bytes at `addr` is allowed: the whole per-access rule for a
    /// `PF_SLOW` page. An access that straddles a PMP entry boundary fails even when both sides
    /// allow it.
    pub fn check(&self, addr: u32, size: u8, kind: AccessKind) -> bool {
        let len = u32::from(size);
        self.pmp.view().check(addr, len, kind, Privilege::Machine)
            && self
                .pms
                .range_perm(addr, len)
                .is_some_and(|p| p.allows(kind))
    }

    /// The permissions of the byte at `addr`, as the `PF_R`, `PF_W` and `PF_X` bits of a page
    /// entry. One byte never straddles a boundary, so this always decides; the slow path narrows
    /// the region's flags with it.
    pub fn byte_flags(&self, addr: u32) -> u32 {
        match self.range_perm(addr, 1) {
            Some(perm) => u32::from(perm.bits()),
            // Unreachable for one byte; denying is the safe answer.
            None => 0,
        }
    }

    /// The first address above `addr` at which the permissions may change, or `limit`. A
    /// translation that starts on a `PF_SLOW` page stops here, so a block never covers bytes the
    /// split forbids.
    pub fn perm_end(&self, addr: u32, limit: u32) -> u32 {
        let mut end = limit;
        let mut narrow = |edge: u32| {
            if edge > addr && edge < end {
                end = edge;
            }
        };
        for region in self.pmp.view().regions() {
            for edge in [region.start, region.end] {
                if let Ok(edge) = u32::try_from(edge) {
                    narrow(edge);
                }
            }
        }
        if let Some(split) = self.pms.split_offset() {
            for base in [SRAM1_IRAM_BASE, SRAM1_DRAM_BASE] {
                for edge in [base, base + split, base + SRAM1_LEN] {
                    narrow(edge);
                }
            }
        }
        end
    }

    /// The flags of the page at `page` from its region's `base` flags: narrowed to what PMP and
    /// PMS grant the whole page, or `base | PF_SLOW` when they do not decide it. Narrowing a
    /// straddling page would deny the half the split allows.
    pub fn page_flags(&self, page: u32, base: u32) -> u32 {
        match self.range_perm(page, PAGE_SIZE) {
            Some(perm) => base & u32::from(perm.bits()),
            None => base | PF_SLOW,
        }
    }

    /// Recomputes the page entries of every [`crate::mem::REGIONS`] row and invalidates pages
    /// whose execute permission changed. Starts from the region flags every time, so it is
    /// idempotent; `PF_CODE`, `PF_COLD` and the SoC's own `PF_SLOW` marks survive.
    pub fn apply(&self, soc: &mut Soc) -> Applied {
        soc.pmp = self.pmp;
        soc.pms = self.pms;
        let mut applied = Applied::default();
        for region in mem::REGIONS {
            self.apply_region(soc, &region, &mut applied);
        }
        applied
    }

    fn apply_region(&self, soc: &mut Soc, region: &Region, applied: &mut Applied) {
        let mut off = 0;
        while off < region.len {
            let page = region.vbase + off;
            let old = soc.pages.entry(page);
            let mut marks = pagetable::flags(old) & (PF_CODE | PF_COLD);
            if soc.is_slow_marked(page) {
                marks |= PF_SLOW;
            }
            let flags = self.page_flags(page, region.flags);
            if flags & PF_SLOW != 0 {
                applied.slow += 1;
            }
            let new = pagetable::entry(region.arena + off, flags | marks);
            if new != old {
                soc.pages.set_entry(page >> 12, new);
                applied.changed += 1;
                if (old ^ new) & PF_X != 0 && soc.invalidate(page) {
                    applied.invalidated += 1;
                }
            }
            off += PAGE_SIZE;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stored;
    use crate::mem::{ROM_BASE, ROM_DATA_BASE, RTC_FAST_BASE, SRAM0_BASE};
    use crate::pagetable::{PF_R, PF_W, fast_load, fast_store};
    use pemu_rv32::pmp::{PMP_L, PMP_NA4, PMP_NAPOT, PMP_R, PMP_TOR, PMP_W, PMP_X};

    /// `_iram_end` of the official image.
    const IRAM_END: u32 = 0x4039_D600;
    /// `_data_start` of the official image: the same byte through the DRAM view.
    const DATA_START: u32 = 0x3FC9_D600;

    struct Row(u32, u8);

    /// The locked TOR table `esp_cpu_configure_region_protection` installs.
    fn official_pmp() -> Csr {
        let rows = [
            Row(0x2000_0000, 0),
            Row(0x2800_0000, PMP_R | PMP_W | PMP_X),
            Row(0x3C00_0000, 0),
            Row(0x3FC8_0000, PMP_R),
            Row(0x3FCE_0000, PMP_R | PMP_W),
            Row(0x3FF2_0000, PMP_R),
            Row(0x4006_0000, PMP_R | PMP_X),
            Row(0x4037_C000, 0),
            Row(0x403E_0000, PMP_R | PMP_W | PMP_X),
            Row(0x4280_0000, PMP_R | PMP_X),
            Row(0x5000_0000, 0),
            Row(0x5000_2000, PMP_R | PMP_W | PMP_X),
            Row(0x6000_0000, 0),
            Row(0x6010_0000, PMP_R | PMP_W),
            Row(0xFFFF_FFFC, 0),
        ];
        let mut csr = Csr::new();
        for (i, Row(top, perm)) in rows.into_iter().enumerate() {
            csr.pmpaddr[i] = top >> 2;
            csr.pmpcfg[i] = PMP_L | PMP_TOR | perm;
        }
        // Entry 15 guards the last four bytes of the address space with NA4.
        csr.pmpaddr[15] = 0xFFFF_FFFC >> 2;
        csr.pmpcfg[15] = PMP_L | PMP_NA4;
        csr
    }

    fn region_pages() -> impl Iterator<Item = (Region, u32)> {
        mem::REGIONS
            .into_iter()
            .flat_map(|r| (0..r.len / PAGE_SIZE).map(move |i| (r, r.vbase + i * PAGE_SIZE)))
    }

    #[test]
    fn the_official_pmp_table_leaves_every_region_page_as_the_map_built_it() {
        // Every boundary of the locked table is page aligned and every region row already has
        // its entry's permissions, so locking PMP must not move a page entry.
        let mut soc = Soc::default();
        let csr = official_pmp();
        let applied = Protection::from_csr(&csr, Pms::OPEN).apply(&mut soc);
        assert_eq!(
            applied,
            Applied::default(),
            "the locked table changed a page"
        );

        for (region, page) in region_pages() {
            let entry = soc.pages.entry(page);
            assert_eq!(
                pagetable::flags(entry),
                pagetable::fold(region.flags),
                "{} at {page:#X}",
                region.name
            );
        }
        let prot = Protection::from_csr(&csr, Pms::OPEN);
        for (addr, perm) in [
            (ROM_BASE, PMP_R | PMP_X),
            (ROM_DATA_BASE, PMP_R),
            (SRAM0_BASE, PMP_R | PMP_W | PMP_X),
            (SRAM1_IRAM_BASE, PMP_R | PMP_W | PMP_X),
            (SRAM1_DRAM_BASE, PMP_R | PMP_W),
            (RTC_FAST_BASE, PMP_R | PMP_W | PMP_X),
        ] {
            assert_eq!(
                prot.range_perm(addr, PAGE_SIZE).map(|p| p.bits()),
                Some(perm),
                "{addr:#X}"
            );
        }
        assert!(!prot.check(0, 4, AccessKind::Read));
        assert!(!prot.check(0, 4, AccessKind::Write));
        assert!(!prot.check(0, 2, AccessKind::Execute));
    }

    #[test]
    fn a_range_that_spans_sram1_is_not_decided_by_its_two_ends() {
        let pms = Pms::split_at(IRAM_END).expect("an SRAM1 address");
        assert_eq!(pms.range_perm(SRAM1_IRAM_BASE - 8, 0x10), None, "the base");
        assert_eq!(
            pms.range_perm(SRAM1_IRAM_BASE - PAGE_SIZE, SRAM1_LEN + 2 * PAGE_SIZE),
            None,
            "a range that swallows the whole of SRAM1"
        );
        assert_eq!(pms.range_perm(IRAM_END - 2, 4), None, "the split itself");
        assert_eq!(
            pms.range_perm(SRAM1_IRAM_BASE + SRAM1_LEN - 2, 4),
            None,
            "the top"
        );
        assert_eq!(
            pms.range_perm(0xFFFF_FFFC, 8),
            None,
            "a range off the end of the address space"
        );

        assert_eq!(pms.range_perm(ROM_BASE, PAGE_SIZE), Some(Perm::RWX));
        assert_eq!(
            pms.range_perm(SRAM1_IRAM_BASE, 4).map(|p| p.bits()),
            Some(PMP_R | PMP_X)
        );
        assert_eq!(pms.range_perm(IRAM_END, 4), Some(Perm::NONE));
        assert_eq!(
            pms.range_perm(DATA_START, 4).map(|p| p.bits()),
            Some(PMP_R | PMP_W)
        );
        assert_eq!(pms.range_perm(DATA_START - 4, 4), Some(Perm::NONE));

        assert_eq!(Pms::OPEN.range_perm(0, u32::MAX), Some(Perm::RWX));
        assert_eq!(
            Pms::OPEN.range_perm(SRAM1_IRAM_BASE - 8, 0x10),
            Some(Perm::RWX)
        );
    }

    #[test]
    fn an_access_that_straddles_an_entry_boundary_is_denied_although_both_sides_allow_it() {
        // An access that straddles an entry boundary faults although no byte of it is denied
        // alone, so only the range rule on a `PF_SLOW` page catches it.
        let mut soc = Soc::default();
        let edge = SRAM1_DRAM_BASE + 0x5800;
        let mut csr = Csr::new();
        // Two adjacent locked RW entries meeting 0x800 bytes into a page.
        csr.pmpaddr[0] = edge >> 2;
        csr.pmpcfg[0] = PMP_L | PMP_TOR | PMP_R | PMP_W;
        csr.pmpaddr[1] = 0x4000_0000 >> 2;
        csr.pmpcfg[1] = PMP_L | PMP_TOR | PMP_R | PMP_W;
        let applied = Protection::from_csr(&csr, Pms::OPEN).apply(&mut soc);
        assert_eq!(applied.slow, 1, "the page the boundary falls inside");
        assert!(soc.pages.entry(edge) & PF_SLOW != 0);

        for off in [0u32, 0x7FF, 0x800, 0xFFF] {
            let at = (edge & !0xFFF) + off;
            assert_eq!(soc.load_mem(at, 1), Some(0), "{at:#X}");
            assert_eq!(
                soc.store_mem(at, 1, 0x5A),
                Stored::Wrote { invalidated: false },
                "{at:#X}"
            );
        }
        assert_eq!(soc.load_mem(edge - 4, 4), Some(0x5A00_0000));
        assert_eq!(soc.load_mem(edge, 4), Some(0x0000_005A));
        assert_eq!(soc.load(edge - 2, 4), Err(crate::Refused::Denied));
        assert_eq!(soc.store_mem(edge - 2, 4, 0xFFFF_FFFF), Stored::Denied);
        // Crossing into the slow page from the page below crosses no entry boundary: allowed.
        assert_eq!(soc.load((edge & !0xFFF) - 2, 4), Ok(0x005A_0000));
        assert_eq!(
            soc.load_mem(edge - 2, 2),
            Some(0x5A00),
            "and the refused store wrote nothing: the 0x5A at edge - 1 is the byte store above"
        );
    }

    #[test]
    fn a_refold_keeps_a_slow_mark_that_is_not_the_folds_own() {
        // Without this a refold after `csrw pmpcfg0` would drop a data watchpoint's mark and
        // the watch would never fire again.
        let mut soc = Soc::default();
        let watched = SRAM1_DRAM_BASE + 0x2_0000;
        assert!(fast_store(soc.pages.entry(watched), watched, 4));
        assert!(soc.mark_slow(watched), "the page was not marked");
        assert!(!soc.mark_slow(watched), "and is not marked twice");
        assert!(soc.is_slow_marked(watched));
        assert!(!fast_store(soc.pages.entry(watched), watched, 4));
        assert!(!fast_load(soc.pages.entry(watched), watched, 4));

        let pms = Pms::split_at(IRAM_END).expect("an SRAM1 address");
        let applied = Protection::from_csr(&official_pmp(), pms).apply(&mut soc);
        assert_eq!(
            applied.slow, 2,
            "the fold's own slow pages are still the two"
        );
        assert!(
            soc.pages.entry(watched) & PF_SLOW != 0,
            "the refold kept the watchpoint's mark"
        );
        assert!(!fast_store(soc.pages.entry(watched), watched, 4));
        assert_eq!(
            soc.store_mem(watched, 4, 0x77),
            Stored::Wrote { invalidated: false }
        );
        assert_eq!(soc.load_mem(watched, 4), Some(0x77));

        assert!(soc.unmark_slow(watched));
        assert!(!soc.unmark_slow(watched));
        assert_eq!(pagetable::flags(soc.pages.entry(watched)), PF_R | PF_W);
        assert!(fast_store(soc.pages.entry(watched), watched, 4));
        assert!(fast_load(soc.pages.entry(watched), watched, 4));

        // The fold's own slow page is not the SoC's to drop.
        let straddle = 0x4039_D000;
        assert!(!soc.unmark_slow(straddle), "it was never marked");
        assert!(soc.pages.entry(straddle) & PF_SLOW != 0);
        assert!(soc.mark_slow(straddle) && soc.unmark_slow(straddle));
        assert!(soc.pages.entry(straddle) & PF_SLOW != 0);
        assert!(!soc.mark_slow(crate::mem::FLASH_DROM_BASE));
        assert_eq!(soc.pages.entry(crate::mem::FLASH_DROM_BASE), 0);
    }

    #[test]
    fn the_iram_end_page_is_slow_in_both_views_and_no_other_page_is() {
        let pms = Pms::split_at(IRAM_END).expect("an SRAM1 address");
        assert_eq!(pms, Pms::split_at(DATA_START).expect("the same byte"));
        assert_eq!(pms.iram_split(), Some(IRAM_END));
        assert_eq!(pms.dram_split(), Some(DATA_START));
        assert_eq!(pms.split_offset(), Some(IRAM_END - SRAM1_IRAM_BASE));
        assert_eq!(Pms::split_at(ROM_BASE), None, "PMS splits SRAM1 only");

        let mut soc = Soc::default();
        let applied = Protection::from_csr(&official_pmp(), pms).apply(&mut soc);
        assert_eq!(
            applied.slow, 2,
            "the two views of one page, and nothing else"
        );

        let slow: Vec<u32> = region_pages()
            .filter(|(_, page)| soc.pages.entry(*page) & PF_SLOW != 0)
            .map(|(_, page)| page)
            .collect();
        assert_eq!(slow, [0x3FC9_D000, 0x4039_D000]);

        for page in slow {
            assert!(!fast_load(soc.pages.entry(page), page, 4));
            assert!(!fast_store(soc.pages.entry(page), page, 4));
        }

        // Below the split the page is IRAM code, above it DRAM data, so this page is less
        // permissive than its neighbours in both views.
        let split = IRAM_END - 0x4039_D000;
        for (addr, load, store) in [
            (0x4039_D000, Some(0), Stored::Denied),
            (0x4039_D000 + split - 4, Some(0), Stored::Denied),
            (0x3FC9_D000, None, Stored::Denied),
            (0x3FC9_D000 + split - 4, None, Stored::Denied),
            (
                0x3FC9_D000 + split,
                Some(0),
                Stored::Wrote { invalidated: false },
            ),
            (0x3FC9_DFFC, Some(0), Stored::Wrote { invalidated: false }),
            (0x4039_D000 + split, None, Stored::Denied),
            (0x4039_DFFC, None, Stored::Denied),
        ] {
            assert_eq!(soc.load_mem(addr, 4), load, "load {addr:#X}");
            assert_eq!(soc.store_mem(addr, 4, 0x1234), store, "store {addr:#X}");
        }
        assert_eq!(soc.load_mem(0x3FC9_D000 + split - 2, 4), None);
        assert_eq!(soc.store_mem(0x4039_D000 + split - 2, 4, 1), Stored::Denied);

        // The two views are the same bytes: the mark took the page off the fast paths without
        // unmapping it.
        assert_eq!(soc.load_mem(0x3FC9_D000 + split, 4), Some(0x1234));
        assert_eq!(
            mem::arena_offset(0x3FC9_D000 + split),
            mem::arena_offset(0x4039_D000 + split)
        );
        for page in [0x4039_C000, 0x4039_E000, 0x3FC9_C000, 0x3FC9_E000] {
            assert_eq!(soc.pages.entry(page) & PF_SLOW, 0, "{page:#X}");
        }
        assert_eq!(
            soc.fetch(0x4039_D000).expect("the code half runs").len(),
            split as usize
        );
        assert!(
            soc.fetch(IRAM_END).is_err(),
            "the data half of the page does not execute"
        );
    }

    #[test]
    fn the_split_applies_the_w_xor_x_rule_to_the_two_views() {
        let mut soc = Soc::default();
        let pms = Pms::split_at(IRAM_END).expect("an SRAM1 address");
        Protection::from_csr(&official_pmp(), pms).apply(&mut soc);

        let code_iram = 0x4039_0000;
        let code_dram = 0x3FC9_0000;
        assert_eq!(pagetable::flags(soc.pages.entry(code_iram)), PF_R | PF_X);
        assert_eq!(pagetable::flags(soc.pages.entry(code_dram)), 0);
        // The map grants the write, so this refusal is a protection fault (mcause 7), not the
        // read-only ROM case.
        assert_eq!(soc.store_mem(code_iram, 4, 1), Stored::Denied);
        assert_eq!(soc.store_mem(code_dram, 4, 1), Stored::Denied);
        assert_eq!(soc.load_mem(code_dram, 4), None, "DRAM0 region 0: none");
        assert_eq!(soc.load(code_dram, 4), Err(crate::Refused::Denied));

        let data_iram = 0x403A_0000;
        let data_dram = 0x3FCA_0000;
        assert_eq!(pagetable::flags(soc.pages.entry(data_dram)), PF_R | PF_W);
        assert_eq!(pagetable::flags(soc.pages.entry(data_iram)), 0);
        assert_eq!(
            soc.store_mem(data_dram, 4, 0xABCD),
            Stored::Wrote { invalidated: false }
        );
        assert_eq!(soc.load_mem(data_dram, 4), Some(0xABCD));
        assert_eq!(soc.load_mem(data_iram, 4), None, "IRAM0 region 3: none");
        assert_eq!(soc.store_mem(data_iram, 4, 1), Stored::Denied);
        assert!(
            soc.fetch(data_iram).is_err(),
            "a fetch from the data part faults"
        );
        assert!(soc.fetch(code_iram).is_ok(), "the code part still runs");

        // With the monitors off the map is back to its identity flags; checked on pages the
        // fetch above did not mark.
        let applied = Protection::from_csr(&official_pmp(), Pms::OPEN).apply(&mut soc);
        assert_eq!(applied.slow, 0);
        assert_eq!(
            pagetable::flags(soc.pages.entry(0x403A_8000)),
            PF_R | PF_W | PF_X
        );
        assert_eq!(pagetable::flags(soc.pages.entry(0x3FC9_8000)), PF_R | PF_W);
        assert_eq!(
            pagetable::flags(soc.pages.entry(code_dram)),
            PF_R | PF_W | PF_CODE
        );
    }

    /// A single locked TOR entry from 0 to `top`: the smallest table that restricts machine mode.
    fn one_locked_entry(top: u32, perm: u8) -> Csr {
        let mut csr = Csr::new();
        csr.pmpaddr[0] = top >> 2;
        csr.pmpcfg[0] = PMP_L | PMP_TOR | perm;
        csr
    }

    #[test]
    fn a_denied_permission_binds_the_slow_path_as_well_as_the_fast_one() {
        // The store is `Denied` (mcause 7), not `ReadOnly`: the map grants it and only the
        // locked entry refuses it.
        let mut soc = Soc::default();
        let addr = SRAM1_DRAM_BASE + 0x40;
        assert_eq!(
            soc.store_mem(addr, 4, 0x1111),
            Stored::Wrote { invalidated: false }
        );

        let csr = one_locked_entry(0x4000_0000, PMP_R);
        let applied = Protection::from_csr(&csr, Pms::OPEN).apply(&mut soc);
        assert!(applied.changed > 0 && applied.slow == 0);
        assert_eq!(pagetable::flags(soc.pages.entry(addr)), PF_R);
        assert!(!fast_store(soc.pages.entry(addr), addr, 4));
        assert_eq!(soc.store_mem(addr, 4, 0x2222), Stored::Denied);
        assert_eq!(soc.load_mem(addr, 4), Some(0x1111), "the write was refused");
        // ROM stays `ReadOnly`; the two must not be logged or trapped the same way.
        assert_eq!(soc.store_mem(ROM_BASE, 4, 1), Stored::ReadOnly);

        // Outside every entry machine mode keeps everything.
        assert_eq!(
            pagetable::flags(soc.pages.entry(SRAM1_IRAM_BASE)),
            PF_R | PF_W | PF_X
        );

        let csr = one_locked_entry(0x4000_0000, PMP_R | PMP_W);
        Protection::from_csr(&csr, Pms::OPEN).apply(&mut soc);
        assert_eq!(
            soc.store_mem(addr, 4, 0x2222),
            Stored::Wrote { invalidated: false }
        );
    }

    #[test]
    fn losing_execute_permission_invalidates_the_translated_pages() {
        let mut soc = Soc::default();
        let code = SRAM1_IRAM_BASE + 0x1_0000;
        soc.fetch(code).expect("SRAM1 is executable");
        assert!(soc.is_code_page(code), "the fetch translated the page");
        assert!(
            soc.is_code_page(SRAM1_DRAM_BASE + 0x1_0000),
            "and marked the other view of it"
        );
        assert!(soc.take_invalidated().is_empty());

        // Both views are invalidated, because they hold the same translated bytes.
        let csr = one_locked_entry(0x4040_0000, PMP_R | PMP_W);
        let applied = Protection::from_csr(&csr, Pms::OPEN).apply(&mut soc);
        assert_eq!(
            applied.invalidated, 1,
            "one page lost its execute permission"
        );
        let mut dropped = soc.take_invalidated();
        dropped.sort_unstable();
        assert_eq!(
            dropped,
            [
                (SRAM1_DRAM_BASE + 0x1_0000) >> 12,
                (SRAM1_IRAM_BASE + 0x1_0000) >> 12
            ]
        );
        assert!(!soc.is_code_page(code));
        assert!(soc.fetch(code).is_err(), "and the page no longer executes");

        let applied = Protection::from_csr(&csr, Pms::OPEN).apply(&mut soc);
        assert_eq!(
            applied,
            Applied {
                changed: 0,
                slow: 0,
                invalidated: 0
            }
        );
        assert!(soc.take_invalidated().is_empty());
    }

    /// A 32-bit xorshift, so the property tests draw the same configurations on every host.
    fn xorshift(state: &mut u32) -> u32 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *state = x;
        x
    }

    /// Random PMP entries with boundaries in and around the mapped regions, so partial pages are
    /// common.
    fn random_pmp(seed: &mut u32) -> Csr {
        let mut csr = Csr::new();
        for i in 0..16 {
            let r = xorshift(seed);
            let mode = match r & 3 {
                0 => 0,
                1 => PMP_TOR,
                2 => PMP_NA4,
                _ => PMP_NAPOT,
            };
            let mut perm = ((r >> 8) & 7) as u8;
            // R = 0 with W = 1 is reserved and `write_pmpcfg` legalizes it away.
            if perm & PMP_W != 0 && perm & PMP_R == 0 {
                perm &= !PMP_W;
            }
            let lock = if (r >> 16) & 1 == 1 { PMP_L } else { 0 };
            csr.pmpcfg[i] = mode | perm | lock;
            let addr = 0x3FC0_0000u32.wrapping_add(xorshift(seed) % 0x0080_0000);
            csr.pmpaddr[i] = addr >> 2;
        }
        csr
    }

    #[test]
    fn a_page_the_fold_decides_is_decided_the_same_way_for_every_byte_of_it() {
        // The soundness property: a page without PF_SLOW gets the same answer for every byte.
        let offsets = [0u32, 1, 0x800, PAGE_SIZE - 1];
        let kinds = [AccessKind::Read, AccessKind::Write, AccessKind::Execute];
        let mut seed = 0x5EED_1403;
        let mut decided = 0u32;
        let mut slow = 0u32;
        for _ in 0..24 {
            let csr = random_pmp(&mut seed);
            let pms = match xorshift(&mut seed) % 3 {
                0 => Pms::OPEN,
                1 => Pms::split_at(IRAM_END).expect("an SRAM1 address"),
                _ => Pms::split_at(SRAM1_DRAM_BASE + 0x2_1234).expect("an SRAM1 address"),
            };
            let prot = Protection::from_csr(&csr, pms);
            for (region, page) in region_pages() {
                let flags = prot.page_flags(page, region.flags);
                let mut disagreed = false;
                for kind in kinds {
                    let bit = u32::from(Perm::from_bits(1 << kinds_index(kind)).bits());
                    let first = prot.check(page, 1, kind);
                    for off in offsets {
                        let here = prot.check(page + off, 1, kind);
                        disagreed |= here != first;
                        if flags & PF_SLOW == 0 {
                            assert_eq!(
                                here, first,
                                "{page:#X}+{off:#X} {kind:?} disagrees on a page without PF_SLOW"
                            );
                            assert_eq!(
                                flags & bit != 0,
                                region.flags & bit != 0 && here,
                                "{page:#X}+{off:#X} {kind:?} folds to the wrong bit"
                            );
                        }
                    }
                }
                if flags & PF_SLOW != 0 {
                    slow += 1;
                } else {
                    decided += 1;
                }
                assert!(
                    !disagreed || flags & PF_SLOW != 0,
                    "{page:#X} is decided two ways but carries no PF_SLOW"
                );
                assert_eq!(flags & !PF_SLOW & !region.flags, 0, "{page:#X}");
            }
        }
        // The draw must exercise both answers, or the property proves nothing.
        assert!(
            decided > 1000 && slow > 24,
            "decided {decided}, slow {slow}"
        );
    }

    fn kinds_index(kind: AccessKind) -> u32 {
        match kind {
            AccessKind::Read => 0,
            AccessKind::Write => 1,
            AccessKind::Execute => 2,
        }
    }

    #[test]
    fn folding_the_same_protection_twice_changes_nothing() {
        // `Bus::pmp_changed` runs on every effective CSR write, and the app rewrites the table
        // the bootloader locked.
        let mut seed = 0x1403_5EED;
        for _ in 0..24 {
            let csr = random_pmp(&mut seed);
            let pms = if xorshift(&mut seed) & 1 == 0 {
                Pms::OPEN
            } else {
                Pms::split_at(IRAM_END).expect("an SRAM1 address")
            };
            let mut soc = Soc::default();
            let first = Protection::from_csr(&csr, pms).apply(&mut soc);
            let again = Protection::from_csr(&csr, pms).apply(&mut soc);
            assert_eq!(
                again.changed, 0,
                "the second fold moved {} pages",
                again.changed
            );
            assert_eq!(again.invalidated, 0);
            assert_eq!(again.slow, first.slow);
        }
    }
}
