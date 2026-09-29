//! `Wiring::MmuEntry` and `Wiring::CacheCtrl`: an MMU entry write rewrites the page entries of
//! the two flash windows and invalidates the engine's translated blocks (`specs/blocks/mmu.toml`).
//!
//! Entry `i` covers 16 pages at `0x3C000000 + i x 64 KB` (DROM, `PF_R`) and at
//! `0x42000000 + i x 64 KB` (IROM, `PF_R | PF_X`), the permissions the guest's locked PMP entries
//! give these windows. Both views share arena bytes, because [`crate::mem::FLASH_ARENA`] is
//! indexed by physical flash address.
//!
//! Page entries are rewritten only after [`Soc::invalidate`] has run over the pages they
//! replace, because the `PF_CODE` mark lives in the entry being overwritten. Pairing the two
//! views per MMU entry also reaches two entries that map the same physical page (the bootloader
//! keeps entry 127 as a sliding window).

use crate::flash_store;
use crate::mem;
use crate::mmu;
use crate::pagetable::{self, PF_COLD, PF_R, PF_X};
use crate::{Soc, periph};

/// Page-table pages one MMU entry covers in each window.
pub const PAGES_PER_ENTRY: u32 = mmu::PAGE_LEN / pagetable::PAGE_SIZE;

// The arena mirror is copied per flash-store page and mapped per page-table page, so the two
// sizes must match.
const _: () = assert!(flash_store::PAGE_LEN == pagetable::PAGE_SIZE);

/// The engine's cache of translated blocks. The machine drains the SoC's invalidation queue into
/// it after every access that returned `OkStop`.
pub trait BlockCache {
    /// Drop every translated block of virtual page `vpn` (`vaddr >> 12`).
    fn invalidate_page(&mut self, vpn: u32);
}

/// Hands every page the SoC has invalidated to `cache`, empties the queue and returns the count.
///
/// Stores into code, MMU entry writes and flash writes all queue on the same [`Soc`], so the run
/// loop drains one queue.
pub fn drain_invalidations(soc: &mut Soc, cache: &mut dyn BlockCache) -> usize {
    let pages = soc.take_invalidated();
    for vpn in &pages {
        cache.invalidate_page(*vpn);
    }
    pages.len()
}

/// Applies `Wiring::MmuEntry(index)`: rewrites entry `index` and every other entry the model
/// marked dirty, and returns the rewritten indexes, ascending. A narrow write can straddle two
/// entries while `Wiring::MmuEntry(u8)` names only one.
pub fn apply(soc: &mut Soc, index: u8) -> Vec<u32> {
    let mut entries = soc.devices.mmu.take_dirty();
    let index = u32::from(index);
    if !entries.contains(&index) {
        entries.push(index);
        entries.sort_unstable();
    }
    for entry in &entries {
        map_entry(soc, *entry, Refill::FromFlash);
    }
    entries
}

/// Rewrites every MMU entry from the flash store and clears the dirty set.
///
/// A reset owes this call: `Mmu::reset_table` restores the entries but cannot reach the page
/// table from inside a `Peripheral`, so until it runs the windows stay mapped. The host also
/// needs it after changing flash behind the guest.
pub fn apply_all(soc: &mut Soc) {
    soc.devices.mmu.take_dirty();
    for entry in 0..mmu::ENTRIES as u32 {
        map_entry(soc, entry, Refill::FromFlash);
    }
}

/// Applies `Wiring::CacheCtrl`: rewrites both windows, re-reads every mapped flash page the arena
/// is behind on and drops every flash-derived translation. Returns the number of 4 KB pages
/// re-read.
///
/// This is `Cache_Invalidate_ICache_Items` with no real cache: the next window read must see
/// current flash bytes, whatever changed them. The ROM writes SYNC_CTRL once per chunk; only the
/// first call copies anything.
pub fn cache_ctrl(soc: &mut Soc) -> usize {
    let mut copied = 0;
    for entry in 0..mmu::ENTRIES as u32 {
        // Skip an invalid entry that is already unmapped. IDF suspends and resumes the cache
        // around every flash operation, so this saves rewriting all 4096 page entries each time.
        if !mmu::is_valid(soc.devices.mmu.entry(entry)) && windows_unmapped(soc, entry) {
            continue;
        }
        copied += map_entry(soc, entry, Refill::IfStale);
    }
    copied
}

/// Whether both window views of entry `index` are already unmapped.
fn windows_unmapped(soc: &Soc, index: u32) -> bool {
    (0..PAGES_PER_ENTRY).all(|page| {
        let off = page * pagetable::PAGE_SIZE;
        soc.pages.entry(mmu::irom_base(index) + off) == 0
            && soc.pages.entry(mmu::drom_base(index) + off) == 0
    })
}

/// Whether a rewrite also re-reads the mapped flash pages into the arena.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Refill {
    /// Copy every mapped page, for an arena changed outside [`FlashStore`] ([`apply_all`]).
    FromFlash,
    /// Copy only the mapped pages whose mirror the store marks as behind ([`cache_ctrl`]).
    IfStale,
}

/// Rewrites the 16 page entries of `index` in each window from the model's entry; returns how
/// many 4 KB pages it copied into the arena.
fn map_entry(soc: &mut Soc, index: u32, refill: Refill) -> usize {
    let entry = soc.devices.mmu.entry(index);
    let (drom, irom) = (mmu::drom_base(index), mmu::irom_base(index));
    // The translated-code mark lives in the entry about to be replaced.
    for page in 0..PAGES_PER_ENTRY {
        let off = page * pagetable::PAGE_SIZE;
        soc.invalidate(irom + off);
        soc.invalidate(drom + off);
    }
    // A page the 8 MB part lacks maps nothing, and an access then takes the counted unbacked
    // path. An invalid entry lands there too; the `MMU_ENTRY_FAULT` interrupt (source 36) is not
    // modeled.
    let phys = mmu::is_valid(entry)
        .then(|| mmu::phys_page(entry) * mmu::PAGE_LEN)
        .filter(|base| *base < mem::FLASH_LEN);
    let mut copied = 0;
    for page in 0..PAGES_PER_ENTRY {
        let off = page * pagetable::PAGE_SIZE;
        // `PF_COLD` on data pages under a profile that charges the cache. A rewrite mapping the
        // same bytes keeps a warm page warm; `lru16k` keeps every DROM page cold ([`crate::cold`]).
        let old = soc.pages.entry(drom + off);
        let (drom_entry, irom_entry) = match phys {
            None => (0, 0),
            Some(base) => {
                let arena = mem::FLASH_ARENA + base + off;
                let flash_page = (base + off) / flash_store::PAGE_LEN;
                if refill == Refill::FromFlash || !soc.flash.mirror_is_current(flash_page) {
                    copied += usize::from(mirror_page(soc, flash_page, arena));
                }
                let same = old != 0 && old & pagetable::ARENA_MASK == arena;
                let cold = soc.cache.charges()
                    && (refill == Refill::FromFlash
                        || soc.cache.charges_fetches()
                        || !same
                        || old & PF_COLD != 0);
                let drom_entry = if cold {
                    pagetable::entry(arena, PF_R | PF_COLD)
                } else {
                    pagetable::entry(arena, PF_R)
                };
                (drom_entry, pagetable::entry(arena, PF_R | PF_X))
            }
        };
        soc.pages.set_entry((drom + off) >> 12, drom_entry);
        soc.pages.set_entry((irom + off) >> 12, irom_entry);
    }
    copied
}

/// Copies one 4 KB flash page into the arena and records the copy on the store. Answers whether
/// it copied.
pub(crate) fn mirror_page(soc: &mut Soc, flash_page: u32, arena_off: u32) -> bool {
    let Some(src) = soc.flash.page_bytes(flash_page) else {
        return false;
    };
    let at = arena_off as usize;
    soc.arena.bytes_mut()[at..at + flash_store::PAGE_LEN as usize].copy_from_slice(src);
    soc.flash.mark_mirrored(flash_page);
    true
}

/// Applies the two `Wiring` effects of the MMU and answers `false` for every other effect.
pub fn handle(soc: &mut Soc, effect: &periph::Wiring) -> bool {
    match effect {
        periph::Wiring::MmuEntry(index) => {
            apply(soc, *index);
            true
        }
        periph::Wiring::CacheCtrl => {
            cache_ctrl(soc);
            // `INVALIDATE_ENA` empties the cache; the suspend and resume around a flash operation
            // do not.
            if soc.devices.extmem.take_invalidate() {
                soc.cache.flush(&mut soc.pages);
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::flash_store::FlashStore;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;
    use pemu_core::time::VTime;
    use std::collections::BTreeSet;

    /// A stand-in for the block engine: after a drain, no invalidated page is still translated.
    #[derive(Default)]
    pub(crate) struct StubEngine {
        translated: BTreeSet<u32>,
        pub(crate) dropped: Vec<u32>,
    }

    impl BlockCache for StubEngine {
        fn invalidate_page(&mut self, vpn: u32) {
            self.translated.remove(&vpn);
            self.dropped.push(vpn);
        }
    }

    impl StubEngine {
        /// Translates a block at `vaddr`, which marks the code page.
        pub(crate) fn translate(&mut self, soc: &mut Soc, vaddr: u32) {
            soc.fetch(vaddr).expect("an executable page");
            self.translated.insert(vaddr >> 12);
        }

        pub(crate) fn holds(&self, vaddr: u32) -> bool {
            self.translated.contains(&(vaddr >> 12))
        }
    }

    const T: VTime = VTime(1);

    /// A SoC over a flash image whose every 4 KB page starts with its own page number.
    fn soc_with_flash() -> Soc {
        let mut image = vec![0u8; mem::FLASH_LEN as usize];
        for (page, chunk) in image.chunks_mut(flash_store::PAGE_LEN as usize).enumerate() {
            chunk.fill(page as u8);
            chunk[0..4].copy_from_slice(&(page as u32).to_le_bytes());
        }
        Soc::new(FlashStore::new(image.into()).expect("8 MB"))
    }

    /// Writes MMU entry `index` as IDF does (`paddr >> 16`) and applies the returned effect.
    fn write_entry(soc: &mut Soc, index: u32, value: u32) -> Vec<u32> {
        let mut ledger = FidelityLedger::default();
        let changed = soc
            .devices
            .mmu
            .store(index * 4, Size::B4, value, T, &mut ledger);
        assert_eq!(changed, Some(index));
        apply(soc, index as u8)
    }

    #[test]
    fn an_entry_write_maps_both_windows_onto_the_same_flash_page() {
        let mut soc = soc_with_flash();
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), None);
        assert_eq!(soc.load_mem(mmu::drom_base(0), 4), None);
        // The app's IROM starts at 0x42000020 and the factory partition at 0x10000.
        write_entry(&mut soc, 0, 1);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), Some(0x10));
        assert_eq!(soc.load_mem(mmu::drom_base(0), 4), Some(0x10));
        for page in 0..PAGES_PER_ENTRY {
            let off = page * pagetable::PAGE_SIZE;
            assert_eq!(soc.load_mem(mmu::irom_base(0) + off, 4), Some(0x10 + page));
            assert_eq!(soc.load_mem(mmu::drom_base(0) + off, 4), Some(0x10 + page));
        }
        assert_eq!(soc.load_mem(mmu::irom_base(1), 4), None);
        // DROM is read-only and IROM is executable.
        assert!(soc.fetch(mmu::irom_base(0)).is_ok());
        assert!(soc.fetch(mmu::drom_base(0)).is_err());
        assert_eq!(
            soc.store_mem(mmu::drom_base(0), 4, 0xFF),
            crate::Stored::ReadOnly
        );
        assert_eq!(
            soc.store_mem(mmu::irom_base(0), 4, 0xFF),
            crate::Stored::ReadOnly
        );
        assert!(soc.arena.guards_intact());
    }

    #[test]
    fn a_remap_invalidates_the_ibus_range_the_engine_translated() {
        let mut soc = soc_with_flash();
        let mut engine = StubEngine::default();
        write_entry(&mut soc, 0, 1);
        engine.translate(&mut soc, mmu::irom_base(0));
        engine.translate(&mut soc, mmu::irom_base(0) + 0x2000);
        assert!(soc.is_code_page(mmu::irom_base(0)));
        assert!(engine.holds(mmu::irom_base(0)));
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 0);

        write_entry(&mut soc, 0, 2);
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 2);
        assert_eq!(
            engine.dropped,
            vec![mmu::irom_base(0) >> 12, (mmu::irom_base(0) + 0x2000) >> 12]
        );
        assert!(!engine.holds(mmu::irom_base(0)));
        assert!(!engine.holds(mmu::irom_base(0) + 0x2000));
        assert!(!soc.is_code_page(mmu::irom_base(0)));
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), Some(0x20));

        engine.translate(&mut soc, mmu::irom_base(0));
        engine.dropped.clear();
        write_entry(&mut soc, 0, mmu::INVALID);
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 1);
        assert!(!engine.holds(mmu::irom_base(0)));
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), None);
    }

    #[test]
    fn a_store_into_translated_code_invalidates_that_page() {
        let mut soc = soc_with_flash();
        let mut engine = StubEngine::default();
        // A page of SRAM1 has an IRAM and a DRAM view of the same bytes; a store through either
        // must drop blocks translated through the other.
        let code = mem::SRAM1_IRAM_BASE + 0x1000;
        let dram = code - mem::SRAM1_I_D_OFFSET;
        engine.translate(&mut soc, code);
        assert!(soc.is_code_page(code) && soc.is_code_page(dram));
        assert_eq!(
            soc.store_mem(code + 8, 4, 0x1234),
            crate::Stored::Wrote { invalidated: true }
        );
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 2);
        // In `mem::REGIONS` order, the order `Soc::invalidate` walks the views in.
        assert_eq!(engine.dropped, vec![dram >> 12, code >> 12]);
        assert!(!engine.holds(code));
        engine.translate(&mut soc, code);
        engine.dropped.clear();
        assert_eq!(
            soc.store_mem(dram, 4, 0x5678),
            crate::Stored::Wrote { invalidated: true }
        );
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 2);
        assert!(!engine.holds(code));
        assert_eq!(
            soc.store_mem(dram, 4, 0x9ABC),
            crate::Stored::Wrote { invalidated: false }
        );
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 0);
    }

    #[test]
    fn the_rom_cache_invalidate_reaches_the_engine_through_wiring() {
        let mut soc = soc_with_flash();
        let mut engine = StubEngine::default();
        write_entry(&mut soc, 3, 4);
        engine.translate(&mut soc, mmu::irom_base(3));
        // `Cache_Invalidate_ICache_Items`: SYNC_ADDR, SYNC_SIZE, then INVALIDATE_ENA.
        let mut ledger = FidelityLedger::default();
        let effect = soc.devices.extmem.store(
            0x028,
            Size::B4,
            crate::periph::extmem::INVALIDATE_ENA,
            T,
            &mut ledger,
        );
        assert!(matches!(effect, periph::Wiring::CacheCtrl));
        assert!(handle(&mut soc, &effect));
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 1);
        assert!(!engine.holds(mmu::irom_base(3)));
    }

    #[test]
    fn a_cache_control_write_rebuilds_every_window_page() {
        let mut soc = soc_with_flash();
        let mut engine = StubEngine::default();
        write_entry(&mut soc, 3, 4);
        engine.translate(&mut soc, mmu::irom_base(3));
        cache_ctrl(&mut soc);
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 1);
        assert!(!engine.holds(mmu::irom_base(3)));
        assert_eq!(soc.load_mem(mmu::irom_base(3), 4), Some(0x40));
    }

    /// A change nobody announced is visible after a cache-control write, and the skipping is
    /// per page rather than per call.
    #[test]
    fn a_cache_control_write_re_reads_a_page_the_arena_is_behind_on() {
        let mut soc = soc_with_flash();
        write_entry(&mut soc, 0, 1);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), Some(0x10));
        // A change made straight on the store, as a host `flash write` would.
        soc.flash
            .program(0x10 * flash_store::PAGE_LEN, &[0, 0, 0, 0]);
        assert!(!soc.flash.mirror_is_current(0x10), "the store knows");
        assert_eq!(cache_ctrl(&mut soc), 1, "one page was behind");
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), Some(0));
        // The ROM's per-chunk SYNC_CTRL loop pays for that once.
        assert_eq!(cache_ctrl(&mut soc), 0, "nothing is behind any more");
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), Some(0));
        apply_all(&mut soc);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), Some(0));
    }

    #[test]
    fn the_rom_invalidate_makes_the_window_read_current_flash() {
        let mut soc = soc_with_flash();
        write_entry(&mut soc, 2, 5);
        assert_eq!(soc.load_mem(mmu::drom_base(2), 4), Some(0x50));
        // A program clears bits, so 0x50 and 0x10 leaves 0x10.
        soc.flash
            .program(0x50 * flash_store::PAGE_LEN, &[0x10, 0, 0, 0]);
        let mut ledger = FidelityLedger::default();
        let effect = soc.devices.extmem.store(
            0x028,
            Size::B4,
            crate::periph::extmem::INVALIDATE_ENA,
            T,
            &mut ledger,
        );
        assert!(handle(&mut soc, &effect));
        assert_eq!(soc.load_mem(mmu::drom_base(2), 4), Some(0x10));
        assert_eq!(soc.load_mem(mmu::irom_base(2), 4), Some(0x10));
    }

    /// The gap between `Mmu::reset_table` and `apply_all` is visible rather than assumed away.
    #[test]
    fn a_reset_needs_the_full_rebuild_to_reach_the_address_space() {
        let mut soc = soc_with_flash();
        let mut engine = StubEngine::default();
        write_entry(&mut soc, 0, 1);
        engine.translate(&mut soc, mmu::irom_base(0));

        let chip = pemu_core::reset::ResetKind::of(pemu_core::reset::ResetCause::POWERON)
            .expect("a documented cause");
        soc.devices.mmu.reset_table(chip);
        assert_eq!(soc.devices.mmu.entry(0), mmu::INVALID);
        assert_eq!(
            soc.load_mem(mmu::irom_base(0), 4),
            Some(0x10),
            "still mapped"
        );
        assert!(soc.fetch(mmu::irom_base(0)).is_ok());
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 0);
        assert!(engine.holds(mmu::irom_base(0)));

        apply_all(&mut soc);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), None);
        assert!(soc.fetch(mmu::irom_base(0)).is_err());
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 1);
        assert!(!engine.holds(mmu::irom_base(0)));
    }

    #[test]
    fn a_cache_control_write_skips_the_entries_that_map_nothing() {
        let mut soc = soc_with_flash();
        write_entry(&mut soc, 0, 1);
        cache_ctrl(&mut soc);
        // Only entry 0 is rewritten; a skipped entry and a rewritten invalid one both read
        // unmapped.
        assert!(windows_unmapped(&soc, 1));
        assert_eq!(cache_ctrl(&mut soc), 0);
        assert!(windows_unmapped(&soc, 1));
        // An invalid entry whose windows are still mapped is not skipped.
        soc.devices.mmu.set_entry(0, mmu::INVALID);
        assert!(!windows_unmapped(&soc, 0));
        cache_ctrl(&mut soc);
        assert!(windows_unmapped(&soc, 0));
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), None);
    }

    #[test]
    fn an_entry_above_the_part_maps_nothing() {
        let mut soc = soc_with_flash();
        // Pages 128 and above are outside the 8 MB part.
        write_entry(&mut soc, 0, 127);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), Some(0x7F0));
        let before = soc.unbacked_accesses();
        write_entry(&mut soc, 0, 128);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), None);
        assert_eq!(soc.unbacked_accesses(), before, "load_mem does not count");
        assert!(soc.arena.guards_intact());
    }

    #[test]
    fn a_narrow_write_that_straddles_two_entries_applies_both() {
        let mut soc = soc_with_flash();
        let mut ledger = FidelityLedger::default();
        // The low half lands in entry 0, the high half in entry 1.
        let changed = soc
            .devices
            .mmu
            .store(2, Size::B4, 0x0002_0000, T, &mut ledger);
        assert_eq!(changed, Some(0));
        assert_eq!(apply(&mut soc, 0), vec![0, 1]);
        assert_eq!(soc.devices.mmu.entry(0), 0x0000_0100);
        assert_eq!(soc.devices.mmu.entry(1), 0x0000_0002);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), None, "still invalid");
        assert_eq!(soc.load_mem(mmu::irom_base(1), 4), Some(0x20));
    }
}
