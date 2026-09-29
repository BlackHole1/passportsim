//! The C3 memory map (TRM system and memory chapter, IDF `soc/esp32c3/include/soc/soc.h`) and the
//! one heap allocation that backs it.
//!
//! Every backed range is one [`Region`] of [`REGIONS`]. Two regions that name the same arena
//! offset are two views of the same memory, which is how the ROM data alias and the SRAM1
//! IRAM/DRAM pair are expressed: table entries, not special cases in the bus.
//!
//! | Range | Region | Backing | Views |
//! |---|---|---|---|
//! | 0x40000000-0x4005FFFF | ROM, instruction view | ROM image bytes 0x00000-0x5FFFF | R, X |
//! | 0x3FF00000-0x3FF1FFFF | ROM, data view | the same bytes, image offset 0x40000 | R |
//! | 0x4037C000-0x4037FFFF | SRAM0 (instruction-cache SRAM) | RAM block B | R, W, X |
//! | 0x40380000-0x403DFFFF | SRAM1 as IRAM | RAM block A | R, W, X |
//! | 0x3FC80000-0x3FCDFFFF | SRAM1 as DRAM | the same RAM block A | R, W |
//! | 0x50000000-0x50001FFF | RTC FAST RAM | RAM block C | R, W, X |
//! | 0x60000000-0x600FFFFF | peripherals | none: MMIO | slow path |
//!
//! The two 8 MB flash windows (DROM 0x3C000000, IROM 0x42000000) are not regions: they reach
//! flash through the MMU, whose entries `crate::wiring::mmu` points into [`FLASH_ARENA`].
//!
//! One heap allocation holds every backed byte, each area separated by a guard page of
//! [`ARENA_GUARD_BYTE`]. [`Arena::new`] grows a zeroed `Vec` and boxes it: a large buffer on the
//! stack overflows the 1 MiB stack a Windows `.exe` gets from its PE header.

use pemu_rv32::bus::{PF_MMIO, PF_R, PF_W, PF_X};

use crate::pagetable::{self, PAGE_SIZE, PageTable};

/// One backed range of the address space: `len` bytes at `vbase`, held at `arena` inside the
/// [`Arena`]. Two regions with the same `arena` are two views of the same bytes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Region {
    /// Name of the region.
    pub name: &'static str,
    /// First virtual address of the view.
    pub vbase: u32,
    /// Length in bytes; always a multiple of [`PAGE_SIZE`].
    pub len: u32,
    /// Byte offset of the first byte inside the [`Arena`]; always a multiple of [`PAGE_SIZE`].
    pub arena: u32,
    /// Permission flags of every page of the view: `PF_R`, `PF_W` and `PF_X`.
    pub flags: u32,
}

impl Region {
    /// Whether `addr` lies inside the view.
    #[inline]
    pub const fn covers(&self, addr: u32) -> bool {
        addr >= self.vbase && addr - self.vbase < self.len
    }

    /// Arena offset of `addr`, if the view covers it.
    #[inline]
    pub const fn offset_of(&self, addr: u32) -> Option<usize> {
        if self.covers(addr) {
            Some((self.arena + (addr - self.vbase)) as usize)
        } else {
            None
        }
    }
}

/// Bytes of the ROM image and of its instruction view, 0x40000000-0x4005FFFF.
pub const ROM_LEN: u32 = 0x6_0000;
/// Bytes of the ROM data view, 0x3FF00000-0x3FF1FFFF.
pub const ROM_DATA_LEN: u32 = 0x2_0000;
/// Offset inside the ROM image that the data view starts at: `0x40040000 - 0x40000000`.
pub const ROM_DATA_IMAGE_OFF: u32 = 0x4_0000;
/// Bytes of SRAM0, the instruction-cache SRAM, 0x4037C000-0x4037FFFF.
pub const SRAM0_LEN: u32 = 0x4000;
/// Bytes of SRAM1, seen as IRAM at 0x40380000 and as DRAM at 0x3FC80000.
pub const SRAM1_LEN: u32 = 0x6_0000;
/// Bytes of RTC FAST RAM, 0x50000000-0x50001FFF, the same address on both buses.
pub const RTC_FAST_LEN: u32 = 0x2000;
/// Bytes of flash: 8 MB on this board.
pub const FLASH_LEN: u32 = 0x80_0000;

/// First address of the ROM instruction view.
pub const ROM_BASE: u32 = 0x4000_0000;
/// First address of the ROM data view; `ROM_BASE + ROM_DATA_IMAGE_OFF - ROM_DATA_OFFSET`.
pub const ROM_DATA_BASE: u32 = 0x3FF0_0000;
/// Distance from the ROM data view to its instruction view: 0x140000.
pub const ROM_DATA_OFFSET: u32 = 0x14_0000;
/// First address of SRAM0.
pub const SRAM0_BASE: u32 = 0x4037_C000;
/// First address of SRAM1 seen as IRAM.
pub const SRAM1_IRAM_BASE: u32 = 0x4038_0000;
/// First address of SRAM1 seen as DRAM.
pub const SRAM1_DRAM_BASE: u32 = 0x3FC8_0000;
/// Distance from the DRAM view of SRAM1 to its IRAM view: `SOC_I_D_OFFSET` (`soc.h:177`).
pub const SRAM1_I_D_OFFSET: u32 = 0x70_0000;
/// Bytes of SRAM Block 1, the first block of SRAM1 in both views (TRM table 16.3-1): the block
/// whose code fetches and data accesses contend (`probe_campaign_timing` `cpi_at_*`, `dres_*`).
pub const SRAM_BLOCK1_LEN: u32 = 0x2_0000;
/// First address of RTC FAST RAM.
pub const RTC_FAST_BASE: u32 = 0x5000_0000;
/// First address of the flash DBUS window, DROM.
pub const FLASH_DROM_BASE: u32 = 0x3C00_0000;
/// First address of the flash IBUS window, IROM.
pub const FLASH_IROM_BASE: u32 = 0x4200_0000;
/// Bytes of each flash window: 8 MB of virtual space, 128 pages of 64 KB.
pub const FLASH_WINDOW_LEN: u32 = 0x80_0000;
/// First address of the peripheral window.
pub const MMIO_BASE: u32 = 0x6000_0000;
/// Bytes of the peripheral window: 1 MB, of which `crate::mmio` decodes the first 0xD1 pages.
pub const MMIO_LEN: u32 = 0x10_0000;

/// Bytes of the guard page that separates two arena areas.
pub const ARENA_GUARD_LEN: u32 = PAGE_SIZE;
/// Byte a guard page is filled with, so a run off the end of an area shows in a dump and
/// [`Arena::guards_intact`] catches it in tests.
pub const ARENA_GUARD_BYTE: u8 = 0xDD;

/// Arena offset of the ROM image. A guard page sits below it, so no backed page has arena offset
/// 0 and a page-table entry of 0 stays unambiguously "unmapped".
pub const ROM_ARENA: u32 = ARENA_GUARD_LEN;
/// Arena offset of SRAM0.
pub const SRAM0_ARENA: u32 = ROM_ARENA + ROM_LEN + ARENA_GUARD_LEN;
/// Arena offset of SRAM1, shared by its IRAM and its DRAM view.
pub const SRAM1_ARENA: u32 = SRAM0_ARENA + SRAM0_LEN + ARENA_GUARD_LEN;
/// Arena offset of RTC FAST RAM.
pub const RTC_FAST_ARENA: u32 = SRAM1_ARENA + SRAM1_LEN + ARENA_GUARD_LEN;
/// Arena offset of the 8 MB flash window area, indexed by physical flash address so the DROM and
/// IROM windows of one MMU entry land on the same bytes. `crate::wiring::mmu` fills a 64 KB page
/// of it from [`crate::flash_store::FlashStore`] when an entry maps that page.
pub const FLASH_ARENA: u32 = RTC_FAST_ARENA + RTC_FAST_LEN + ARENA_GUARD_LEN;
/// Bytes of the whole arena, one heap allocation of about 9 MB.
pub const ARENA_LEN: u32 = FLASH_ARENA + FLASH_LEN + ARENA_GUARD_LEN;

/// Every backed range of the address space, in map order.
pub const REGIONS: [Region; 6] = [
    Region {
        name: "sram1_dram",
        vbase: SRAM1_DRAM_BASE,
        len: SRAM1_LEN,
        arena: SRAM1_ARENA,
        flags: PF_R | PF_W,
    },
    Region {
        name: "rom_data",
        vbase: ROM_DATA_BASE,
        len: ROM_DATA_LEN,
        arena: ROM_ARENA + ROM_DATA_IMAGE_OFF,
        flags: PF_R,
    },
    Region {
        name: "rom",
        vbase: ROM_BASE,
        len: ROM_LEN,
        arena: ROM_ARENA,
        flags: PF_R | PF_X,
    },
    Region {
        name: "sram0",
        vbase: SRAM0_BASE,
        len: SRAM0_LEN,
        arena: SRAM0_ARENA,
        flags: PF_R | PF_W | PF_X,
    },
    Region {
        name: "sram1_iram",
        vbase: SRAM1_IRAM_BASE,
        len: SRAM1_LEN,
        arena: SRAM1_ARENA,
        flags: PF_R | PF_W | PF_X,
    },
    Region {
        name: "rtc_fast",
        vbase: RTC_FAST_BASE,
        len: RTC_FAST_LEN,
        arena: RTC_FAST_ARENA,
        flags: PF_R | PF_W | PF_X,
    },
];

/// The region holding `addr`, or `None` for an address no region backs.
#[inline]
pub fn region_of(addr: u32) -> Option<&'static Region> {
    REGIONS.iter().find(|r| r.covers(addr))
}

/// Arena offset of `addr`, if a region backs it.
#[inline]
pub fn arena_offset(addr: u32) -> Option<usize> {
    region_of(addr).and_then(|r| r.offset_of(addr))
}

/// Every virtual page base that maps the same arena page as `addr`, its own view included. The
/// translated-code mark and its invalidation are applied to all of them at once, so a store
/// through one view cannot leave stale blocks translated from the other.
pub fn views_of(addr: u32) -> impl Iterator<Item = u32> {
    let page = region_of(addr).map(|r| (r.arena + (addr - r.vbase)) & !(PAGE_SIZE - 1));
    REGIONS.into_iter().filter_map(move |r| {
        let page = page?;
        (page >= r.arena && page - r.arena < r.len).then(|| r.vbase + (page - r.arena))
    })
}

/// Whether `addr` lies in the peripheral window 0x60000000-0x600FFFFF. Every page of it takes
/// the MMIO slow path, including the undecoded pages above the blocks, which read 0.
#[inline]
pub const fn is_mmio(addr: u32) -> bool {
    addr >= MMIO_BASE && addr - MMIO_BASE < MMIO_LEN
}

/// Whether `addr` lies in one of the two flash windows, DROM or IROM.
#[inline]
pub const fn is_flash_window(addr: u32) -> bool {
    (addr >= FLASH_DROM_BASE && addr - FLASH_DROM_BASE < FLASH_WINDOW_LEN)
        || (addr >= FLASH_IROM_BASE && addr - FLASH_IROM_BASE < FLASH_WINDOW_LEN)
}

/// The MMU entry index a flash-window address translates through: `(va & 0x7FFFFF) >> 16`, one
/// table for both windows.
#[inline]
pub const fn mmu_index(addr: u32) -> u32 {
    (addr & 0x7F_FFFF) >> 16
}

/// The one heap allocation that backs every region, guard pages included.
pub struct Arena {
    bytes: Box<[u8]>,
}

impl Arena {
    /// Bytes of the allocation.
    pub const LEN: usize = ARENA_LEN as usize;

    /// A zeroed arena with its guard pages filled. Grown as a `Vec` and boxed, so no copy ever
    /// exists on the stack.
    pub fn new() -> Arena {
        let mut bytes = vec![0u8; Arena::LEN].into_boxed_slice();
        for start in Arena::guard_offsets() {
            let start = start as usize;
            bytes[start..start + ARENA_GUARD_LEN as usize].fill(ARENA_GUARD_BYTE);
        }
        Arena { bytes }
    }

    /// Offsets of the guard pages: one below every area and one above the last.
    const fn guard_offsets() -> [u32; 6] {
        [
            0,
            SRAM0_ARENA - ARENA_GUARD_LEN,
            SRAM1_ARENA - ARENA_GUARD_LEN,
            RTC_FAST_ARENA - ARENA_GUARD_LEN,
            FLASH_ARENA - ARENA_GUARD_LEN,
            ARENA_LEN - ARENA_GUARD_LEN,
        ]
    }

    /// Every guard page still holds [`ARENA_GUARD_BYTE`], so nothing wrote past an area.
    pub fn guards_intact(&self) -> bool {
        Arena::guard_offsets().into_iter().all(|start| {
            let start = start as usize;
            self.bytes[start..start + ARENA_GUARD_LEN as usize]
                .iter()
                .all(|b| *b == ARENA_GUARD_BYTE)
        })
    }

    /// The bytes, indexed by arena offset.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The bytes, indexed by arena offset.
    #[inline]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Base the page-table entries offset into (`Bus::arena`).
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    /// The `size` bytes at arena offset `off`, little-endian. Bytes past the end read 0, which no
    /// mapped access reaches.
    #[inline]
    pub fn load(&self, off: usize, size: u8) -> u32 {
        let mut val = 0;
        for i in 0..size as usize {
            let byte = self.bytes.get(off + i).copied().unwrap_or(0);
            val |= u32::from(byte) << (i * 8);
        }
        val
    }

    /// Writes the low `size` bytes of `val` at arena offset `off`, little-endian.
    #[inline]
    pub fn store(&mut self, off: usize, size: u8, val: u32) {
        for i in 0..size as usize {
            if let Some(slot) = self.bytes.get_mut(off + i) {
                *slot = (val >> (i * 8)) as u8;
            }
        }
    }
}

impl Default for Arena {
    fn default() -> Self {
        Arena::new()
    }
}

/// Writes the identity page entries of the whole map into `pages`: every region's pages point at
/// their arena bytes with the region's flags, and the peripheral window carries `PF_MMIO`. The
/// flash windows are unmapped and no page carries `PF_SLOW` or `PF_COLD` yet.
pub fn write_identity_pages(pages: &mut PageTable) {
    for region in REGIONS {
        let mut off = 0;
        while off < region.len {
            let vpn = (region.vbase + off) >> 12;
            pages.set_entry(vpn, pagetable::entry(region.arena + off, region.flags));
            off += PAGE_SIZE;
        }
    }
    let mut off = 0;
    while off < MMIO_LEN {
        pages.set_entry((MMIO_BASE + off) >> 12, pagetable::entry(0, PF_MMIO));
        off += PAGE_SIZE;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_answers_every_documented_range() {
        // The CPU address map, one row per backed range, both ends of each.
        let rows: [(&str, u32, u32); 6] = [
            ("rom", 0x4000_0000, 0x4005_FFFF),
            ("rom_data", 0x3FF0_0000, 0x3FF1_FFFF),
            ("sram0", 0x4037_C000, 0x4037_FFFF),
            ("sram1_iram", 0x4038_0000, 0x403D_FFFF),
            ("sram1_dram", 0x3FC8_0000, 0x3FCD_FFFF),
            ("rtc_fast", 0x5000_0000, 0x5000_1FFF),
        ];
        for (name, first, last) in rows {
            for addr in [first, last] {
                let region = region_of(addr).unwrap_or_else(|| panic!("{name} at {addr:#X}"));
                assert_eq!(region.name, name, "{addr:#X}");
            }
            assert!(region_of(first.wrapping_sub(1)).is_none_or(|r| r.name != name));
            assert!(region_of(last.wrapping_add(1)).is_none_or(|r| r.name != name));
        }
        // The holes: allowed by PMP but backed by nothing.
        for addr in [
            0x0000_0000,
            0x2000_0000,
            0x3FCE_0000,
            0x3FEF_FFFF,
            0x403E_0000,
        ] {
            assert_eq!(region_of(addr), None, "{addr:#X}");
        }
        // The flash windows are not regions: they reach flash through the MMU.
        for addr in [
            FLASH_DROM_BASE,
            FLASH_DROM_BASE + FLASH_WINDOW_LEN - 1,
            FLASH_IROM_BASE,
            FLASH_IROM_BASE + FLASH_WINDOW_LEN - 1,
        ] {
            assert_eq!(region_of(addr), None, "{addr:#X}");
            assert!(is_flash_window(addr));
        }
        assert!(!is_flash_window(FLASH_DROM_BASE - 1));
        assert!(!is_flash_window(FLASH_IROM_BASE + FLASH_WINDOW_LEN));
        assert!(is_mmio(MMIO_BASE) && is_mmio(MMIO_BASE + MMIO_LEN - 1));
        assert!(!is_mmio(MMIO_BASE - 1) && !is_mmio(MMIO_BASE + MMIO_LEN));
    }

    #[test]
    fn the_aliases_share_arena_bytes() {
        // The ROM data view is the instruction view 0x140000 above it.
        assert_eq!(
            ROM_DATA_BASE + ROM_DATA_OFFSET,
            ROM_BASE + ROM_DATA_IMAGE_OFF
        );
        for off in [0, 1, 0x19F70, ROM_DATA_LEN - 1] {
            assert_eq!(
                arena_offset(ROM_DATA_BASE + off),
                arena_offset(ROM_DATA_BASE + ROM_DATA_OFFSET + off),
                "rom data view at {off:#X}",
            );
        }
        // SRAM1's DRAM view is its IRAM view 0x700000 above it (SOC_I_D_OFFSET).
        assert_eq!(SRAM1_DRAM_BASE + SRAM1_I_D_OFFSET, SRAM1_IRAM_BASE);
        for off in [0, 1, 0x1D600, SRAM1_LEN - 1] {
            assert_eq!(
                arena_offset(SRAM1_DRAM_BASE + off),
                arena_offset(SRAM1_IRAM_BASE + off),
                "sram1 at {off:#X}",
            );
        }
        // DRAM is not executable, and the ROM data view is read-only.
        assert_eq!(region_of(SRAM1_DRAM_BASE).unwrap().flags, PF_R | PF_W);
        assert_eq!(
            region_of(SRAM1_IRAM_BASE).unwrap().flags,
            PF_R | PF_W | PF_X
        );
        assert_eq!(region_of(ROM_DATA_BASE).unwrap().flags, PF_R);
        assert_eq!(region_of(ROM_BASE).unwrap().flags, PF_R | PF_X);
    }

    #[test]
    fn every_view_of_a_page_is_reachable_from_any_of_them() {
        let views = |addr| views_of(addr).collect::<Vec<_>>();
        // SRAM1 has two views, and each answers with both.
        assert_eq!(
            views(SRAM1_DRAM_BASE + 0x1D600),
            vec![SRAM1_DRAM_BASE + 0x1D000, SRAM1_IRAM_BASE + 0x1D000]
        );
        assert_eq!(
            views(SRAM1_IRAM_BASE + 0x1D600),
            vec![SRAM1_DRAM_BASE + 0x1D000, SRAM1_IRAM_BASE + 0x1D000]
        );
        // The upper 128 KB of the ROM has its data view beside its instruction view; the lower
        // 256 KB has only the instruction view.
        assert_eq!(
            views(ROM_DATA_BASE + 0x19F70),
            vec![ROM_DATA_BASE + 0x19000, ROM_BASE + 0x59000]
        );
        assert_eq!(views(ROM_BASE + 0x59F70), views(ROM_DATA_BASE + 0x19F70));
        assert_eq!(views(ROM_BASE), vec![ROM_BASE]);
        // SRAM0 and RTC FAST have one view each, and an unbacked address none.
        assert_eq!(views(SRAM0_BASE + 0x2000), vec![SRAM0_BASE + 0x2000]);
        assert_eq!(views(RTC_FAST_BASE + 0x1FFF), vec![RTC_FAST_BASE + 0x1000]);
        assert_eq!(views(FLASH_IROM_BASE), Vec::<u32>::new());
        assert_eq!(views(MMIO_BASE), Vec::<u32>::new());
    }

    #[test]
    fn regions_are_page_aligned_and_inside_the_arena() {
        for r in REGIONS {
            assert_eq!(r.vbase % PAGE_SIZE, 0, "{}", r.name);
            assert_eq!(r.len % PAGE_SIZE, 0, "{}", r.name);
            assert_eq!(r.arena % PAGE_SIZE, 0, "{}", r.name);
            assert!(r.arena >= ARENA_GUARD_LEN, "{}", r.name);
            assert!(r.arena + r.len <= ARENA_LEN, "{}", r.name);
            assert_eq!(r.flags & !(PF_R | PF_W | PF_X), 0, "{}", r.name);
        }
        // About 9 MB, dominated by the flash window area.
        assert_eq!(
            ARENA_LEN,
            ROM_LEN + SRAM0_LEN + SRAM1_LEN + RTC_FAST_LEN + FLASH_LEN + 6 * ARENA_GUARD_LEN
        );
        assert!((8 << 20..10 << 20).contains(&ARENA_LEN));
    }

    #[test]
    fn the_mmu_index_is_shared_by_both_flash_windows() {
        // index = (vaddr & 0x7FFFFF) >> 16, one table for both windows.
        assert_eq!(mmu_index(FLASH_IROM_BASE + 0x20), 0);
        assert_eq!(mmu_index(FLASH_DROM_BASE + 0x12_0020), 0x12);
        assert_eq!(mmu_index(FLASH_IROM_BASE + 0x12_0020), 0x12);
        assert_eq!(mmu_index(0x3C7F_0000), 127);
        assert_eq!(mmu_index(FLASH_IROM_BASE + FLASH_WINDOW_LEN - 1), 127);
    }

    #[test]
    fn the_arena_is_heap_built_with_intact_guards() {
        let mut arena = Arena::new();
        assert_eq!(arena.bytes().len(), Arena::LEN);
        assert!(arena.guards_intact());
        // A write inside an area leaves the guards alone; one into a guard is visible.
        arena.store(SRAM1_ARENA as usize, 4, 0xDEAD_BEEF);
        assert_eq!(arena.load(SRAM1_ARENA as usize, 4), 0xDEAD_BEEF);
        assert!(arena.guards_intact());
        arena.store((SRAM1_ARENA - ARENA_GUARD_LEN) as usize, 1, 0);
        assert!(!arena.guards_intact());
    }

    #[test]
    fn arena_access_is_little_endian_and_width_exact() {
        let mut arena = Arena::new();
        let off = SRAM1_ARENA as usize;
        arena.store(off, 4, 0x1122_3344);
        assert_eq!(arena.bytes()[off..off + 4], [0x44, 0x33, 0x22, 0x11]);
        assert_eq!(arena.load(off, 1), 0x44);
        assert_eq!(arena.load(off + 1, 2), 0x2233);
        arena.store(off + 1, 1, 0xFF);
        assert_eq!(arena.load(off, 4), 0x1122_FF44);
    }

    #[test]
    fn identity_pages_map_every_region_and_the_peripheral_window() {
        let mut pages = PageTable::new();
        write_identity_pages(&mut pages);
        for r in REGIONS {
            for addr in [r.vbase, r.vbase + r.len - 1] {
                let e = pages.entry(addr);
                assert_eq!(
                    pagetable::arena_off(e) + (addr & 0xFFF) as usize,
                    r.offset_of(addr).unwrap(),
                    "{} at {addr:#X}",
                    r.name,
                );
                assert_eq!(pagetable::flags(e), pagetable::fold(r.flags), "{}", r.name);
            }
        }
        // The DROM alias and its IROM window resolve to the same arena page.
        assert_eq!(
            pagetable::arena_off(pages.entry(ROM_DATA_BASE)),
            pagetable::arena_off(pages.entry(ROM_BASE + ROM_DATA_IMAGE_OFF)),
        );
        // The peripheral window is MMIO end to end; the flash windows stay unmapped.
        for addr in [MMIO_BASE, 0x600C_5000, MMIO_BASE + MMIO_LEN - 1] {
            assert_eq!(pagetable::flags(pages.entry(addr)), PF_MMIO, "{addr:#X}");
        }
        assert_eq!(pages.entry(FLASH_DROM_BASE), 0);
        assert_eq!(pages.entry(FLASH_IROM_BASE), 0);
        assert_eq!(pages.entry(0x3FCE_0000), 0);
    }
}
