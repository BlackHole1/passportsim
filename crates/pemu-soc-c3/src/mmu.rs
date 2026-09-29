//! The flash MMU (`specs/blocks/mmu.toml`, IDF `soc/esp32c3/include/soc/ext_mem_defs.h`): the
//! 128-entry table at 0x600C5000 that both flash windows translate through.
//!
//! Entry `i` serves DROM and IROM alike, index `(va & 0x7FFFFF) >> 16`. Bits 7:0 are the
//! physical 64 KB page, bit 8 is [`INVALID`]. Entries read back exactly as written, because
//! `esp_mmu_map_init` scans the table to find what the bootloader already mapped.
//!
//! Every entry resets to [`INVALID`]: the hardware content is undefined, but the ROM's
//! `Cache_MMU_Init` writes 0x100 into all 128 entries before anything maps flash.
//!
//! A write returns `Wiring::MmuEntry`; `crate::wiring::mmu` then rewrites the window page entries
//! and invalidates translated blocks. A narrow or straddling write is applied byte by byte and
//! marks every changed entry in [`Mmu::take_dirty`], not only the one index `Wiring::MmuEntry`
//! carries. Neither ROM nor IDF writes that way; it is robustness, not observed behavior.

use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::regstore::Size;
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use crate::mem;
use crate::periph::store_only::{RegBank, TouchTag};
use crate::periph::{Block, Cx, Peripheral, RegRead, RegWrite, Wiring, block};

/// Number of MMU entries: `SOC_MMU_ENTRY_NUM`.
pub const ENTRIES: usize = 128;

/// Bytes of one MMU page: the fixed 64 KB page of the C3.
pub const PAGE_LEN: u32 = 0x1_0000;

/// Bytes of the entry table: 128 entries of 4 bytes, 0x600C5000 to 0x600C51FF.
pub const TABLE_LEN: u32 = 4 * ENTRIES as u32;

/// Bit 8 of an entry: `SOC_MMU_INVALID`. An entry with it set translates nothing.
pub const INVALID: u32 = 1 << 8;

/// Bits 7:0 of an entry: the physical flash page, `paddr >> 16`. `SOC_MMU_VALID_VAL_MASK` is
/// 0xFF, so the table addresses 256 pages, 16 MB of flash.
pub const PAGE_MASK: u32 = 0xFF;

/// The value every entry resets to, and the value the ROM's `Cache_MMU_Init` writes.
pub const RESET_ENTRY: u32 = INVALID;

/// [`mem::mmu_index`] under the MMU's name.
#[inline]
pub const fn index_of(va: u32) -> u32 {
    mem::mmu_index(va)
}

/// Whether `entry` translates: `SOC_MMU_VALID` is bit 8 clear.
#[inline]
pub const fn is_valid(entry: u32) -> bool {
    entry & INVALID == 0
}

/// Physical flash page of `entry`, bits 7:0.
#[inline]
pub const fn phys_page(entry: u32) -> u32 {
    entry & PAGE_MASK
}

/// Physical flash address a valid `entry` translates `va` to: `(entry[7:0] << 16) | (va & 0xFFFF)`.
#[inline]
pub const fn phys_addr(entry: u32, va: u32) -> u32 {
    (phys_page(entry) << 16) | (va & (PAGE_LEN - 1))
}

/// First DROM address entry `index` covers.
#[inline]
pub const fn drom_base(index: u32) -> u32 {
    mem::FLASH_DROM_BASE + index * PAGE_LEN
}

/// First IROM address entry `index` covers.
#[inline]
pub const fn irom_base(index: u32) -> u32 {
    mem::FLASH_IROM_BASE + index * PAGE_LEN
}

/// The 128-entry flash MMU table.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Mmu {
    /// One entry per 64 KB page, as written; always [`ENTRIES`] long. A `Vec` because `serde`
    /// stops deriving for arrays above 32 elements.
    entries: Vec<u32>,
    /// Bit per entry: already reported to the fidelity ledger. Snapshotted, so a restored machine
    /// does not report an entry twice.
    touched: [u64; ENTRIES / 64],
    /// Bit per entry changed since [`Mmu::take_dirty`].
    dirty: [u64; ENTRIES / 64],
    /// The rest of the 0x1000 window: plain storage that behaves as a `StoreOnly` block. Its
    /// first `TABLE_LEN` bytes are never read.
    spare: RegBank,
}

impl Default for Mmu {
    fn default() -> Self {
        Mmu {
            entries: vec![RESET_ENTRY; ENTRIES],
            touched: [0; ENTRIES / 64],
            dirty: [0; ENTRIES / 64],
            spare: RegBank::new(<block::Mmu as Block>::SIZE),
        }
    }
}

impl Mmu {
    /// Tag of the first touches this block reports: not allowlisted.
    pub const TAG: TouchTag = TouchTag {
        periph: <block::Mmu as Block>::ID,
        allowlisted: false,
    };

    /// Entry `index` as last written, or [`RESET_ENTRY`] for an index the table does not have.
    #[inline]
    pub fn entry(&self, index: u32) -> u32 {
        self.entries
            .get(index as usize)
            .copied()
            .unwrap_or(RESET_ENTRY)
    }

    #[inline]
    pub fn entry_of(&self, va: u32) -> u32 {
        self.entry(index_of(va))
    }

    /// Physical flash address `va` reaches, or `None` when its entry is invalid.
    ///
    /// The `MMU_ENTRY_FAULT` interrupt is not raised: an access through an invalid entry reaches
    /// the unbacked path, because `crate::wiring::mmu` leaves the window page entries at 0. Only
    /// this module's tests call it.
    #[inline]
    pub fn translate(&self, va: u32) -> Option<u32> {
        let entry = self.entry_of(va);
        is_valid(entry).then(|| phys_addr(entry, va))
    }

    /// Sets entry `index` from outside the guest (reset, tests) and marks it dirty. Out-of-range
    /// indexes are ignored.
    pub fn set_entry(&mut self, index: u32, entry: u32) {
        let Some(slot) = self.entries.get_mut(index as usize) else {
            return;
        };
        *slot = entry;
        self.mark_dirty(index as usize);
    }

    /// The entry indexes changed since the last call, ascending, and clears the set.
    pub fn take_dirty(&mut self) -> Vec<u32> {
        let mut out = Vec::new();
        for (word, bits) in self.dirty.iter_mut().enumerate() {
            let mut pending = *bits;
            *bits = 0;
            while pending != 0 {
                let bit = pending.trailing_zeros();
                pending &= pending - 1;
                out.push((word * 64) as u32 + bit);
            }
        }
        out
    }

    pub fn is_dirty(&self, index: u32) -> bool {
        let idx = index as usize;
        idx < ENTRIES && self.dirty[idx / 64] & (1 << (idx % 64)) != 0
    }

    /// Reads `size` bytes at `off`, reporting the first touch of each entry. This is
    /// [`Peripheral::read`] without a [`Cx`], so tests need only a ledger and a time.
    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        if !Mmu::in_table(off, size) {
            // Wholly above the table: one bank access at the guest's width, as on `StoreOnly`.
            return self.spare.read(off, size, now, ledger, Mmu::TAG);
        }
        let mut val = 0;
        for i in 0..size as u32 {
            let at = off.wrapping_add(i);
            let byte = match Mmu::slot(at) {
                Some((idx, shift)) => {
                    self.touch(idx, size, now, ledger, true);
                    (self.entries[idx] >> shift) & 0xFF
                }
                // An access that starts in the table and ends above it: its 1 to 3 spare bytes
                // go one at a time, since `Size` has no 3.
                None => self.spare.read(at, Size::B1, now, ledger, Mmu::TAG),
            };
            val |= byte << (i * 8);
        }
        val
    }

    /// Writes the low `size` bytes of `val` at `off`, marks every changed entry dirty, and
    /// returns the lowest changed index (the one `Wiring::MmuEntry` carries).
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> Option<u32> {
        if !Mmu::in_table(off, size) {
            // As in [`Mmu::load`]: one bank access at the guest's width.
            self.spare.write(off, size, val, now, ledger, Mmu::TAG);
            return None;
        }
        let mut first = None;
        for i in 0..size as u32 {
            let at = off.wrapping_add(i);
            let byte = (val >> (i * 8)) & 0xFF;
            match Mmu::slot(at) {
                Some((idx, shift)) => {
                    self.touch(idx, size, now, ledger, false);
                    self.entries[idx] = (self.entries[idx] & !(0xFF << shift)) | (byte << shift);
                    self.mark_dirty(idx);
                    first = Some(first.map_or(idx as u32, |f: u32| f.min(idx as u32)));
                }
                None => self.spare.write(at, Size::B1, byte, now, ledger, Mmu::TAG),
            }
        }
        first
    }

    /// Restores every entry to [`RESET_ENTRY`] and marks them all dirty. Every reset scope in
    /// `specs/blocks/mmu.toml` `reset_domains` restores the table.
    ///
    /// # The caller's obligation
    ///
    /// The page table still maps both windows afterwards and `Peripheral::reset` returns no
    /// `Wiring`, so a reset sequence must then call [`crate::wiring::mmu::apply_all`].
    pub fn reset_table(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        for index in 0..ENTRIES as u32 {
            self.set_entry(index, RESET_ENTRY);
        }
        self.spare.reset();
    }

    #[inline]
    fn slot(off: u32) -> Option<(usize, u32)> {
        (off < TABLE_LEN).then(|| ((off / 4) as usize, (off % 4) * 8))
    }

    #[inline]
    fn in_table(off: u32, size: Size) -> bool {
        (0..size as u32).any(|i| Mmu::slot(off.wrapping_add(i)).is_some())
    }

    fn mark_dirty(&mut self, idx: usize) {
        self.dirty[idx / 64] |= 1 << (idx % 64);
    }

    /// Reports the first touch of entry `idx`, once per entry and per machine.
    fn touch(
        &mut self,
        idx: usize,
        size: Size,
        now: VTime,
        ledger: &mut FidelityLedger,
        read: bool,
    ) {
        let bit = 1u64 << (idx % 64);
        if self.touched[idx / 64] & bit != 0 {
            return;
        }
        self.touched[idx / 64] |= bit;
        let access = if read {
            TouchAccess::Read
        } else {
            TouchAccess::Write
        };
        ledger.first_touch(FirstTouch {
            periph: Mmu::TAG.periph,
            off: (idx * 4) as u32,
            access,
            size: size as u8,
            now,
            allowlisted: Mmu::TAG.allowlisted,
        });
    }
}

/// Model of the `mmu` row of the `c3_devices!` table.
pub type Model = Mmu;

impl Peripheral for Mmu {
    const ID: PeriphId = <block::Mmu as Block>::ID;
    const BASE: u32 = <block::Mmu as Block>::BASE;
    const SIZE: u32 = <block::Mmu as Block>::SIZE;

    /// [`Mmu::reset_table`], which needs nothing from `cx`.
    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        let _ = cx;
        self.reset_table(kind);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    /// A write that changed an entry returns `Wiring::MmuEntry` with the lowest changed index.
    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let changed = self.store(off, size, val, cx.now, cx.ledger);
        RegWrite {
            stop: false,
            wiring: match changed {
                // The index fits a u8: the table has 128 entries.
                Some(index) => Wiring::MmuEntry(index as u8),
                None => Wiring::None,
            },
        }
    }

    /// The class `specs/blocks/mmu.toml` gives the entry at `off`. The MMU has no rows in
    /// `specs/c3-registers.csv`, so its block file carries an `offsets` row that `xtask codegen`
    /// renders into [`crate::gen::classes::mmu`]. The window above the 128 entries reads
    /// `Fidelity::U`.
    fn fidelity(&self, off: u32) -> Fidelity {
        crate::r#gen::classes::mmu::class_at(off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::reset::ResetCause;

    const T: VTime = VTime(7);

    #[test]
    fn the_index_formula_is_shared_by_both_windows() {
        for index in 0..ENTRIES as u32 {
            assert_eq!(index_of(drom_base(index)), index);
            assert_eq!(index_of(irom_base(index)), index);
            assert_eq!(index_of(drom_base(index) + PAGE_LEN - 1), index);
            assert_eq!(index_of(irom_base(index) + PAGE_LEN - 1), index);
        }
        // The official app: IROM 0x42000020 uses entry 0 and DROM 0x3C120020 entry 0x12. The
        // linker lays text and rodata out back to back in entry-index space.
        assert_eq!(index_of(0x4200_0020), 0x00);
        assert_eq!(index_of(0x3C12_0020), 0x12);
        // IROM 0x42000020 + 0x11DF30 spans entries 0x00 to 0x11, DROM 0x3C120020 + 0x32B68
        // entries 0x12 to 0x15.
        assert_eq!(index_of(0x4200_0020 + 0x11_DF30 - 1), 0x11);
        assert_eq!(index_of(0x3C12_0020 + 0x3_2B68 - 1), 0x15);
        // The last entry, 127, is the bootloader's sliding window at vaddr 0x3C7F0000.
        assert_eq!(index_of(0x3C7F_0000), 127);
        assert_eq!(drom_base(127), 0x3C7F_0000);
        assert_eq!(irom_base(127), 0x427F_0000);
        assert_eq!(index_of(0x3C00_0000), index_of(0x4200_0000));
        assert_eq!(index_of(0x3C7F_FFFF), index_of(0x427F_FFFF));
    }

    #[test]
    fn bit_8_is_the_invalid_flag_and_bits_7_to_0_are_the_page() {
        assert_eq!(INVALID, 0x100);
        assert_eq!(RESET_ENTRY, 0x100);
        assert!(!is_valid(0x100));
        assert!(!is_valid(0x1FF));
        assert!(is_valid(0x00));
        assert!(is_valid(0xFF));
        assert_eq!(phys_page(0x1AB), 0xAB);
        assert_eq!(phys_page(0xAB), 0xAB);
        assert_eq!(phys_addr(0x12, 0x4200_0020), 0x12_0020);
        assert_eq!(phys_addr(0x12, 0x3C00_0020), 0x12_0020);
        assert_eq!(phys_addr(0xFF, 0x427F_FFFF), 0xFF_FFFF);
        // A fresh table translates nothing: every entry is invalid.
        let mmu = Mmu::default();
        assert_eq!(mmu.entry(0), INVALID);
        assert_eq!(mmu.translate(0x4200_0000), None);
        assert_eq!(mmu.translate(0x3C7F_0000), None);
        assert_eq!(mmu.entry(ENTRIES as u32), RESET_ENTRY);
    }

    #[test]
    fn entries_read_back_exactly_as_written_and_mark_themselves_dirty() {
        let mut mmu = Mmu::default();
        let mut l = FidelityLedger::default();
        // esp_mmu_map_init scans the table, so nothing may mask the value.
        assert_eq!(
            mmu.store(0x18 * 4, Size::B4, 0x1_0012, T, &mut l),
            Some(0x18)
        );
        assert_eq!(mmu.entry(0x18), 0x1_0012);
        assert_eq!(mmu.load(0x18 * 4, Size::B4, T, &mut l), 0x1_0012);
        assert!(is_valid(mmu.entry(0x18)));
        assert_eq!(mmu.translate(irom_base(0x18) + 0x1234), Some(0x12_1234));
        assert_eq!(mmu.take_dirty(), vec![0x18]);
        assert!(!mmu.is_dirty(0x18));
        assert_eq!(mmu.take_dirty(), Vec::<u32>::new());
        mmu.store(0, Size::B1, 0x5A, T, &mut l);
        assert_eq!(mmu.entry(0), 0x15A);
        assert_eq!(mmu.take_dirty(), vec![0]);
        assert_eq!(
            mmu.store(0x4 + 2, Size::B4, 0xAABB_CCDD, T, &mut l),
            Some(1)
        );
        assert_eq!(mmu.entry(1), 0xCCDD_0100);
        assert_eq!(mmu.entry(2), 0x0000_AABB);
        assert_eq!(mmu.take_dirty(), vec![1, 2]);
    }

    #[test]
    fn the_window_above_the_table_is_storage_and_the_table_is_not_grown_by_it() {
        let mut mmu = Mmu::default();
        let mut l = FidelityLedger::default();
        // Above 0x600C51FF the window is store-only, and no entry answers for it.
        assert_eq!(mmu.store(TABLE_LEN, Size::B4, 0xDEAD_BEEF, T, &mut l), None);
        assert_eq!(mmu.load(TABLE_LEN, Size::B4, T, &mut l), 0xDEAD_BEEF);
        assert_eq!(mmu.take_dirty(), Vec::<u32>::new());
        assert_eq!(mmu.entry(ENTRIES as u32 - 1), RESET_ENTRY);
        mmu.store(TABLE_LEN - 4, Size::B4, 0x21, T, &mut l);
        assert_eq!(mmu.entry(127), 0x21);
        assert_eq!(mmu.load(TABLE_LEN, Size::B4, T, &mut l), 0xDEAD_BEEF);
        let before = l.first_touches().len();
        mmu.store(<block::Mmu as Block>::SIZE, Size::B4, 1, T, &mut l);
        assert_eq!(
            mmu.load(<block::Mmu as Block>::SIZE, Size::B4, T, &mut l),
            0
        );
        assert_eq!(l.first_touches().len(), before);
    }

    /// An offset above the table is reported at the width the guest used, as a `StoreOnly`
    /// block does, not at one byte.
    #[test]
    fn an_access_above_the_table_is_reported_at_its_own_width() {
        for (off, size, width) in [
            (TABLE_LEN, Size::B4, 4u8),
            (TABLE_LEN + 8, Size::B2, 2),
            (0x800, Size::B1, 1),
        ] {
            let mut mmu = Mmu::default();
            let mut l = FidelityLedger::default();
            mmu.load(off, size, T, &mut l);
            let rows: Vec<_> = l
                .first_touches()
                .iter()
                .map(|t| (t.off, t.size, t.access))
                .collect();
            assert_eq!(rows, vec![(off, width, TouchAccess::Read)], "{off:#06X}");
            let mut mmu = Mmu::default();
            let mut l = FidelityLedger::default();
            assert_eq!(mmu.store(off, size, 0x5555_5555, T, &mut l), None);
            let rows: Vec<_> = l
                .first_touches()
                .iter()
                .map(|t| (t.off, t.size, t.access))
                .collect();
            assert_eq!(rows, vec![(off, width, TouchAccess::Write)], "{off:#06X}");
            assert_eq!(
                mmu.load(off, size, T, &mut l),
                0x5555_5555 & ((1u64 << (u32::from(width) * 8)) - 1) as u32
            );
            assert_eq!(mmu.take_dirty(), Vec::<u32>::new());
        }
        let mut mmu = Mmu::default();
        let mut l = FidelityLedger::default();
        mmu.load(0x40, Size::B4, T, &mut l);
        let rows: Vec<_> = l.first_touches().iter().map(|t| (t.off, t.size)).collect();
        assert_eq!(rows, vec![(0x40, 4)]);
    }

    #[test]
    fn each_entry_is_reported_to_the_ledger_once() {
        let mut mmu = Mmu::default();
        let mut l = FidelityLedger::default();
        mmu.load(0, Size::B4, VTime(1), &mut l);
        mmu.store(0, Size::B4, 3, VTime(2), &mut l);
        mmu.store(0x12 * 4, Size::B4, 4, VTime(3), &mut l);
        mmu.load(0x12 * 4 + 1, Size::B1, VTime(4), &mut l);
        let rows: Vec<_> = l
            .first_touches()
            .iter()
            .map(|t| (t.off, t.access, t.now))
            .collect();
        assert_eq!(
            rows,
            vec![
                (0x00, TouchAccess::Read, VTime(1)),
                (0x48, TouchAccess::Write, VTime(3)),
            ]
        );
        assert!(l.is_touched(<block::Mmu as Block>::ID, 0x48));
    }

    #[test]
    fn a_reset_unmaps_every_entry_and_a_cpu_reset_keeps_them() {
        let mut mmu = Mmu::default();
        let mut l = FidelityLedger::default();
        mmu.store(0, Size::B4, 0x20, T, &mut l);
        mmu.store(4, Size::B4, 0x21, T, &mut l);
        mmu.take_dirty();
        // `esp_restart` (cause 0x0C) reaches the hart and SENSITIVE only, so the table stands
        // (`ResetFanout::CpuAndPms`).
        let cpu = ResetKind::of(ResetCause::RTC_SW_CPU).expect("a documented cause");
        assert!(!cpu.fanout.reaches_all_blocks());
        mmu.reset_table(cpu);
        assert_eq!(mmu.entry(0), 0x20);
        assert_eq!(mmu.take_dirty(), Vec::<u32>::new());
        let chip = ResetKind::of(ResetCause::POWERON).expect("a documented cause");
        mmu.reset_table(chip);
        assert_eq!(mmu.entry(0), INVALID);
        assert_eq!(mmu.entry(1), INVALID);
        assert_eq!(mmu.take_dirty().len(), ENTRIES);
    }
}
