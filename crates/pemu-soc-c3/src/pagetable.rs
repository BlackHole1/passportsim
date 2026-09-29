//! Page flags and the page table over the address map ([`crate::mem`], from the TRM's system
//! memory map). They are defined in `pemu_rv32::bus`, because `Bus::pages` returns `&PageTable`
//! and the engine fast paths test the flags, and re-exported here.
//!
//! One entry is
//!
//! ```text
//!  31                              12 11        7 6            0
//! +----------------------------------+-----------+--------------+
//! |  arena offset of the page, >> 12 |  reserved | PF_* flags   |
//! +----------------------------------+-----------+--------------+
//! ```
//!
//! so a fast path adds `entry & !0xFFF` and `addr & 0xFFF` to the arena base without a shift. An
//! entry of 0 is unmapped: it names arena offset 0, which [`crate::mem`] keeps as a guard page,
//! and carries no flag, so neither fast rule accepts it.
//!
//! Every entry goes through [`fold`], so its flags are the truth of the two fast-path rules. A
//! slow path takes permissions from [`crate::mem::region_of`] where a region covers the address,
//! otherwise from the folded flags ([`slow_readable`], [`slow_writable`]).

pub use pemu_rv32::bus::{PF_CODE, PF_COLD, PF_MMIO, PF_R, PF_SLOW, PF_W, PF_X, PageTable};

/// Bytes per page.
pub const PAGE_SIZE: u32 = 1 << 12;

/// Mask of the flag bits of an entry (bits 0 to 11).
pub const FLAG_MASK: u32 = PAGE_SIZE - 1;

/// Mask of the arena offset of an entry (bits 12 to 31).
pub const ARENA_MASK: u32 = !FLAG_MASK;

/// Largest arena offset an entry can name: the arena may hold `ARENA_MAX_OFFSET + 1` bytes,
/// which does not fit a 32-bit `usize` and is therefore expressed as the last offset.
pub const ARENA_MAX_OFFSET: u32 = ARENA_MASK;

/// Flags that make data accesses take a slow path, so [`fold`] clears `PF_R` beside them.
pub const PF_NO_FAST_LOAD: u32 = PF_SLOW | PF_MMIO | PF_COLD;

/// Flags the fast store rule does not test but that must still keep a store off the arena:
/// `PF_W` beside either would write the arena instead of the peripheral, so [`fold`] clears it.
pub const PF_NO_FAST_STORE: u32 = PF_MMIO | PF_COLD;

/// The flags as the table may hold them: `PF_R` and `PF_W` dropped where a fast path must not
/// take the access.
#[inline]
pub const fn fold(flags: u32) -> u32 {
    let mut folded = flags;
    if folded & PF_NO_FAST_LOAD != 0 {
        folded &= !PF_R;
    }
    if folded & PF_NO_FAST_STORE != 0 {
        folded &= !PF_W;
    }
    folded
}

/// The entry of a page at arena offset `arena` with `flags`, folded.
///
/// # Panics
///
/// Panics when `arena` is not a multiple of [`PAGE_SIZE`] or `flags` has a bit outside
/// [`FLAG_MASK`]: both are construction errors of the memory map, checked once per page at
/// build time rather than on every access.
#[inline]
pub const fn entry(arena: u32, flags: u32) -> u32 {
    assert!(arena & FLAG_MASK == 0, "arena offset is not page aligned");
    assert!(flags & ARENA_MASK == 0, "flags outside FLAG_MASK");
    arena | fold(flags)
}

/// Arena byte offset of the page of `entry`.
#[inline]
pub const fn arena_off(entry: u32) -> usize {
    (entry & ARENA_MASK) as usize
}

/// Flags of `entry`.
#[inline]
pub const fn flags(entry: u32) -> u32 {
    entry & FLAG_MASK
}

/// Whether an access of `size` bytes at `addr` stays inside its 4 KB page, which both fast-path
/// rules require.
#[inline]
pub const fn in_page(addr: u32, size: u8) -> bool {
    (addr & FLAG_MASK) + size as u32 <= PAGE_SIZE
}

/// The fast load rule: `e & PF_R != 0 && in-page`.
#[inline]
pub const fn fast_load(entry: u32, addr: u32, size: u8) -> bool {
    entry & PF_R != 0 && in_page(addr, size)
}

/// The fast store rule: `e & (PF_W | PF_CODE | PF_SLOW) == PF_W`, in-page.
#[inline]
pub const fn fast_store(entry: u32, addr: u32, size: u8) -> bool {
    entry & (PF_W | PF_CODE | PF_SLOW) == PF_W && in_page(addr, size)
}

/// Whether a slow path may read a page with `flags`.
///
/// [`fold`] drops `PF_R` from an entry only to keep a fast load off the page, never to deny the
/// read, so a `PF_SLOW` or `PF_COLD` entry is readable here even though its `PF_R` is gone. A
/// `PF_MMIO` page is not: it names no arena bytes, and `crate::mmio` answers it instead.
#[inline]
pub const fn slow_readable(flags: u32) -> bool {
    flags & PF_MMIO == 0 && flags & (PF_R | PF_SLOW | PF_COLD) != 0
}

/// Whether a slow path may write a page with `flags`.
///
/// `PF_W` survives [`fold`] for a `PF_SLOW` page, so the bit is the answer. A `PF_COLD` page loses
/// it, so a cold page cannot be writable; that is fine because DROM is read-only.
#[inline]
pub const fn slow_writable(flags: u32) -> bool {
    flags & PF_MMIO == 0 && flags & PF_W != 0
}

/// Arena offset the access at `addr` reads or writes through `entry`.
#[inline]
pub const fn arena_addr(entry: u32, addr: u32) -> usize {
    arena_off(entry) + (addr & FLAG_MASK) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_match_the_page_table_layout() {
        assert_eq!(
            [PF_R, PF_W, PF_X, PF_CODE, PF_SLOW, PF_MMIO, PF_COLD],
            [1, 2, 4, 8, 16, 32, 64]
        );
        assert_eq!(PageTable::PAGES, 1 << 20);
        const _: () = assert!(crate::mem::ARENA_LEN - 1 <= ARENA_MAX_OFFSET);
    }

    #[test]
    fn an_entry_splits_into_an_arena_offset_and_flags() {
        let e = entry(0x12_3000, PF_R | PF_W | PF_X);
        assert_eq!(arena_off(e), 0x12_3000);
        assert_eq!(flags(e), PF_R | PF_W | PF_X);
        assert_eq!(arena_addr(e, 0x4038_0FFF), 0x12_3FFF);
        assert_eq!(arena_addr(e, 0x4038_0000), 0x12_3000);
        // An unmapped page: offset 0, no flags, neither fast path.
        assert_eq!(arena_off(0), 0);
        assert_eq!(flags(0), 0);
        assert!(!fast_load(0, 0, 4) && !fast_store(0, 0, 4));
    }

    #[test]
    fn folding_drops_the_bits_that_would_take_a_slow_page_onto_a_fast_path() {
        // PF_SLOW, PF_MMIO and PF_COLD carry no R bit.
        for slow in [PF_SLOW, PF_MMIO, PF_COLD] {
            assert_eq!(fold(PF_R | PF_W | slow) & PF_R, 0, "{slow}");
            assert!(
                !fast_load(entry(0x1000, PF_R | PF_W | slow), 0, 4),
                "{slow}"
            );
        }
        // The fast store rule names PF_SLOW, so PF_W may stay; PF_MMIO and PF_COLD lose it.
        assert_eq!(fold(PF_R | PF_W | PF_SLOW), PF_W | PF_SLOW);
        assert!(!fast_store(entry(0x1000, PF_R | PF_W | PF_SLOW), 0, 4));
        for quiet in [PF_MMIO, PF_COLD] {
            assert_eq!(fold(PF_R | PF_W | quiet) & PF_W, 0, "{quiet}");
            assert!(
                !fast_store(entry(0x1000, PF_R | PF_W | quiet), 0, 4),
                "{quiet}"
            );
        }
        assert_eq!(fold(PF_R | PF_W | PF_X), PF_R | PF_W | PF_X);
        for flags in 0..=FLAG_MASK {
            assert_eq!(fold(fold(flags)), fold(flags), "{flags:#X}");
        }
    }

    #[test]
    fn the_slow_rules_read_back_what_folding_dropped() {
        // A page with no `Region` row is described by its entry alone, and `fold` took R off it
        // for being slow. The slow path must still read it, or PF_COLD data pages would read 0.
        for slow in [PF_SLOW, PF_COLD] {
            let e = entry(0x1000, PF_R | PF_W | slow);
            assert!(!fast_load(e, 0, 4) && slow_readable(flags(e)), "{slow}");
        }
        assert!(slow_readable(PF_R) && slow_writable(PF_R | PF_W));
        // An MMIO page names no arena bytes: `crate::mmio` answers it, not a memory path.
        assert!(!slow_readable(flags(entry(0, PF_R | PF_MMIO))));
        assert!(!slow_writable(flags(entry(0, PF_R | PF_W | PF_MMIO))));
        // Write permission is the W bit, which survives folding for a PF_SLOW page.
        assert!(slow_writable(flags(entry(0x1000, PF_R | PF_W | PF_SLOW))));
        assert!(!slow_writable(flags(entry(0x1000, PF_R | PF_X))));
        assert!(!slow_readable(flags(0)) && !slow_writable(flags(0)));
    }

    #[test]
    fn the_fast_rules_are_the_page_flag_rules() {
        let ram = entry(0x1000, PF_R | PF_W | PF_X);
        assert!(fast_load(ram, 0x4038_0000, 4) && fast_store(ram, 0x4038_0000, 4));
        // Read-only ROM: loads are fast, stores are not.
        let rom = entry(0x1000, PF_R | PF_X);
        assert!(fast_load(rom, 0x4000_0000, 4) && !fast_store(rom, 0x4000_0000, 4));
        // A translated code page: loads stay fast, stores take the slow path.
        let code = entry(0x1000, PF_R | PF_W | PF_X | PF_CODE);
        assert!(fast_load(code, 0x4038_0000, 4) && !fast_store(code, 0x4038_0000, 4));
        // In-page: an access that straddles the page boundary is never fast, at any width.
        for size in [1u8, 2, 4] {
            let last = 0x4038_1000 - size as u32;
            assert!(fast_load(ram, last, size) && fast_store(ram, last, size));
            assert!(!fast_load(ram, last + 1, size) || size == 1);
            assert!(in_page(last, size) && !in_page(0x4038_0FFF, 2));
        }
        assert!(!fast_load(ram, 0x4038_0FFD, 4) && !fast_store(ram, 0x4038_0FFD, 4));
    }

    #[test]
    fn page_table_starts_unmapped_and_indexes_by_page() {
        let mut pt = PageTable::default();
        assert_eq!(pt.entries().len(), PageTable::PAGES);
        assert_eq!(pt.entry(0x4038_0123), 0);
        pt.set_entry(0x4038_0123 >> 12, entry(0x1000, PF_R | PF_X));
        assert_eq!(pt.entry(0x4038_0FFF), 0x1000 | PF_R | PF_X);
        assert_eq!(pt.entry(0x4038_1000), 0);
    }
}
