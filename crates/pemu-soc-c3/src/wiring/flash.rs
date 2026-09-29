//! `Wiring::FlashWritten`: a flash program or erase refreshes the arena mirror of the pages the
//! MMU maps and invalidates the virtual ranges that map them (`specs/blocks/mmu.toml`).
//!
//! A mapped window page is served from arena bytes copied out of the flash store when the MMU
//! entry was written, so without this a window would keep reading the bytes from before the
//! write. The page unit is [`crate::flash_store::PAGE_LEN`], the 4 KB erase sector the SPI1 model
//! counts in. A missed call is a bounded error: the guest's next `Cache_Invalidate_*` re-reads
//! the store anyway.

use crate::Soc;
use crate::flash_store;
use crate::mem;
use crate::mmu;
use crate::pagetable;
use crate::wiring::mmu::{PAGES_PER_ENTRY, mirror_page};

/// Applies `Wiring::FlashWritten` for physical flash page `phys_page`: refreshes its arena mirror
/// and queues the virtual pages whose blocks the engine must drop.
///
/// Returns the number of MMU entries that map the page. An entry naming a page above the part
/// maps nothing and is not counted, as in `crate::wiring::mmu::map_entry`.
pub fn written(soc: &mut Soc, phys_page: u32) -> usize {
    let mut mapped = 0;
    for index in 0..mmu::ENTRIES as u32 {
        let entry = soc.devices.mmu.entry(index);
        if !mmu::is_valid(entry) {
            continue;
        }
        let base = mmu::phys_page(entry) * mmu::PAGE_LEN;
        // The table can name 16 MB and this board has 8 MB, so a guest can reach an entry above
        // the part; it must count as unmapped here exactly as `map_entry` leaves it.
        if base >= mem::FLASH_LEN {
            continue;
        }
        let first = base / flash_store::PAGE_LEN;
        if phys_page < first || phys_page - first >= PAGES_PER_ENTRY {
            continue;
        }
        if mapped == 0 {
            // One copy serves every entry: the arena is indexed by physical flash address.
            mirror_page(
                soc,
                phys_page,
                mem::FLASH_ARENA + phys_page * flash_store::PAGE_LEN,
            );
        }
        mapped += 1;
        let off = (phys_page - first) * pagetable::PAGE_SIZE;
        // Only the IBUS view can hold translated blocks; invalidating DROM finds nothing.
        soc.invalidate(mmu::irom_base(index) + off);
        soc.invalidate(mmu::drom_base(index) + off);
    }
    mapped
}

/// [`written`] for `count` consecutive pages from `first`.
pub fn written_range(soc: &mut Soc, first: u32, count: u32) -> usize {
    (first..first.saturating_add(count))
        .map(|page| written(soc, page))
        .sum()
}

/// Applies `Wiring::FlashWritten` and answers `false` for every other effect.
pub fn handle(soc: &mut Soc, effect: &crate::periph::Wiring) -> bool {
    match effect {
        crate::periph::Wiring::FlashWritten { phys_page } => {
            written(soc, *phys_page);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flash_store::FlashStore;
    use crate::wiring;
    use crate::wiring::mmu::{apply, drain_invalidations};
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;
    use pemu_core::time::VTime;

    /// A SoC over an erased 8 MB flash with entry 0 mapping physical page 0 of the part.
    fn soc_mapping_page_zero() -> Soc {
        let mut soc = Soc::new(FlashStore::erased());
        let mut ledger = FidelityLedger::default();
        soc.devices.mmu.store(0, Size::B4, 0, VTime(0), &mut ledger);
        apply(&mut soc, 0);
        soc
    }

    #[test]
    fn a_flash_program_refreshes_the_window_and_invalidates_its_blocks() {
        let mut soc = soc_mapping_page_zero();
        let mut engine = wiring::mmu::tests::StubEngine::default();
        // An erased part reads 0xFF through the window.
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), Some(0xFFFF_FFFF));
        engine.translate(&mut soc, mmu::irom_base(0));

        // Program clears bits.
        soc.flash.program(0, &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(written(&mut soc, 0), 1);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 4), Some(0x7856_3412));
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 1);
        assert!(!engine.holds(mmu::irom_base(0)));
        // The DROM view reads the same arena page.
        assert_eq!(soc.load_mem(mmu::drom_base(0), 4), Some(0x7856_3412));
    }

    #[test]
    fn an_erase_refreshes_every_page_of_the_range() {
        let mut soc = soc_mapping_page_zero();
        soc.flash.program(0, &[0]);
        soc.flash.program(flash_store::PAGE_LEN, &[0]);
        written_range(&mut soc, 0, 2);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 1), Some(0));
        assert_eq!(
            soc.load_mem(mmu::irom_base(0) + pagetable::PAGE_SIZE, 1),
            Some(0)
        );
        soc.flash.erase_block(0);
        assert_eq!(written_range(&mut soc, 0, PAGES_PER_ENTRY), 16);
        for page in 0..PAGES_PER_ENTRY {
            let at = mmu::irom_base(0) + page * pagetable::PAGE_SIZE;
            assert_eq!(soc.load_mem(at, 1), Some(0xFF), "page {page}");
        }
        assert!(soc.arena.guards_intact());
    }

    #[test]
    fn a_write_to_flash_nothing_maps_changes_no_window() {
        let mut soc = soc_mapping_page_zero();
        // Physical page 0x100 is 1 MB into the part and no entry names it.
        soc.flash.program(0x100 * flash_store::PAGE_LEN, &[0]);
        assert_eq!(written(&mut soc, 0x100), 0);
        assert_eq!(soc.load_mem(mmu::irom_base(0), 1), Some(0xFF));
        // Mapping it afterwards still sees the write: the entry copies the store's bytes.
        let mut ledger = FidelityLedger::default();
        soc.devices
            .mmu
            .store(4, Size::B4, 0x10, VTime(0), &mut ledger);
        apply(&mut soc, 1);
        assert_eq!(soc.load_mem(mmu::irom_base(1), 1), Some(0));
    }

    #[test]
    fn an_entry_above_the_part_does_not_count_as_mapping_the_page() {
        let mut soc = soc_mapping_page_zero();
        // Physical page 200 is 0xC80000, past the end of an 8 MB part.
        let mut ledger = FidelityLedger::default();
        soc.devices
            .mmu
            .store(5 * 4, Size::B4, 200, VTime(0), &mut ledger);
        apply(&mut soc, 5);
        assert_eq!(soc.pages.entry(mmu::irom_base(5)), 0, "nothing is mapped");
        assert_eq!(soc.load_mem(mmu::irom_base(5), 1), None);
        // 200 * 16 = flash page 3200, the first 4 KB page the entry would cover.
        soc.flash.program(3200 * flash_store::PAGE_LEN, &[0]);
        assert_eq!(written(&mut soc, 3200), 0);
        // Entry 0 still maps physical page 0 and is still counted.
        soc.flash.program(0, &[0]);
        assert_eq!(written(&mut soc, 0), 1);
        assert!(soc.arena.guards_intact());
    }

    #[test]
    fn the_same_physical_page_under_two_entries_is_invalidated_in_both() {
        let mut soc = soc_mapping_page_zero();
        let mut engine = wiring::mmu::tests::StubEngine::default();
        // Entry 127 is the bootloader's sliding window and may name a page another entry maps.
        let mut ledger = FidelityLedger::default();
        soc.devices
            .mmu
            .store(127 * 4, Size::B4, 0, VTime(0), &mut ledger);
        apply(&mut soc, 127);
        engine.translate(&mut soc, mmu::irom_base(0));
        engine.translate(&mut soc, mmu::irom_base(127));
        soc.flash.program(0, &[0]);
        assert_eq!(written(&mut soc, 0), 2);
        assert_eq!(drain_invalidations(&mut soc, &mut engine), 2);
        assert!(!engine.holds(mmu::irom_base(0)));
        assert!(!engine.holds(mmu::irom_base(127)));
    }

    #[test]
    fn each_wiring_module_answers_for_its_own_effects() {
        let mut soc = soc_mapping_page_zero();
        assert!(handle(
            &mut soc,
            &crate::periph::Wiring::FlashWritten { phys_page: 0 }
        ));
        assert!(!handle(&mut soc, &crate::periph::Wiring::CacheCtrl));
        assert!(wiring::mmu::handle(
            &mut soc,
            &crate::periph::Wiring::CacheCtrl
        ));
        assert!(wiring::mmu::handle(
            &mut soc,
            &crate::periph::Wiring::MmuEntry(0)
        ));
        assert!(!wiring::mmu::handle(
            &mut soc,
            &crate::periph::Wiring::FlashWritten { phys_page: 0 }
        ));
        assert!(!wiring::mmu::handle(&mut soc, &crate::periph::Wiring::None));
    }
}
