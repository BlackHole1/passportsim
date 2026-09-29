//! The TLSF heap walker.
//!
//! `registered_heaps` heads a `heap_t_` list. Each region's `heap` points at a
//! `multi_heap_info`, whose `heap_data` is the TLSF `control_t`. The pool starts at
//! `heap_data + control->size`, its first block header sits four bytes before the pool, each next
//! block is `block + 8 + (size & ~3) - 4`, and the sentinel has size zero. The low two bits of
//! `size` are the free and previous-free flags. A chain that does not advance, leaves the pool, or
//! runs past [`MAX_BLOCKS`] is reported and abandoned.

use crate::GuestMemory;
use crate::layout::Layouts;
use crate::{IntrospectError, Warning};
use pemu_loader::symbols::SymbolTable;

/// Blocks the walker will follow in one pool before declaring the chain unbounded.
pub const MAX_BLOCKS: u32 = 65_536;

/// Heap regions the walker will follow before declaring the region list unbounded.
pub const MAX_REGIONS: u32 = 64;

/// `block_header_t.size` bit 0: this block is free.
pub const BLOCK_FREE: u32 = 1;
/// `block_header_t.size` bit 1: the previous physical block is free.
pub const BLOCK_PREV_FREE: u32 = 2;

/// Bytes between a block header and the payload, and the header bytes a block borrows from its
/// predecessor.
const BLOCK_START_OFFSET: u32 = 8;
/// Bytes of a block header that are not part of the previous block's payload.
const BLOCK_HEADER_OVERHEAD: u32 = 4;

/// One registered heap region.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HeapRegion {
    pub heap_t: u32,
    /// The capability words the allocator matches against.
    pub caps: [u32; 3],
    pub start: u32,
    pub end: u32,
    pub info: u32,
    pub free_bytes: u32,
    /// The low-water mark.
    pub minimum_free_bytes: u32,
    pub pool_size: u32,
    /// `heap_data`, which is the TLSF `control_t`.
    pub control: u32,
    /// `control->size`, the bytes the control block occupies before the pool.
    pub control_size: u32,
    /// `control->sl_index_count`, the second-level subdivision.
    pub sl_index_count: u32,
    pub small_block_size: u32,
    pub used_blocks: u32,
    pub free_blocks: u32,
    /// As `size & ~3`.
    pub largest_free_raw: u32,
    /// [`fit_size`] of `largest_free_raw`: the largest allocation the region can serve.
    pub largest_free_fit: u32,
}

impl HeapRegion {
    pub fn render(&self) -> String {
        format!(
            "{:#010x}-{:#010x} free={} min={} blocks={}u/{}f largest={}/{}",
            self.start,
            self.end,
            self.free_bytes,
            self.minimum_free_bytes,
            self.used_blocks,
            self.free_blocks,
            self.largest_free_raw,
            self.largest_free_fit
        )
    }
}

/// Every registered heap region, with the totals `inspect heap` reports.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HeapSnapshot {
    /// In `registered_heaps` order.
    pub regions: Vec<HeapRegion>,
    pub warnings: Vec<Warning>,
}

impl HeapSnapshot {
    pub fn total_free(&self) -> u64 {
        self.regions.iter().map(|r| u64::from(r.free_bytes)).sum()
    }

    pub fn total_minimum_free(&self) -> u64 {
        self.regions
            .iter()
            .map(|r| u64::from(r.minimum_free_bytes))
            .sum()
    }

    /// Largest allocation any region can serve.
    pub fn largest_free_fit(&self) -> u32 {
        self.regions
            .iter()
            .map(|r| r.largest_free_fit)
            .max()
            .unwrap_or(0)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for r in &self.regions {
            out.push_str(&r.render());
            out.push('\n');
        }
        out.push_str(&format!(
            "total free={} min={} largest={}\n",
            self.total_free(),
            self.total_minimum_free(),
            self.largest_free_fit()
        ));
        for w in &self.warnings {
            out.push_str(&format!("warning: {w}\n"));
        }
        out
    }
}

/// Rounds a free-block size down to what TLSF can hand out, as `tlsf_fit_size` and
/// `multi_heap_get_info_impl` do: `interval = (1 << msb) / sl_index_count`.
pub fn fit_size(size: u32, sl_index_count: u32) -> u32 {
    if size == 0 || sl_index_count == 0 {
        return size;
    }
    let msb = 31 - size.leading_zeros();
    let interval = (1u32 << msb) / sl_index_count;
    match interval {
        0 => size,
        i => size & !(i - 1),
    }
}

pub fn walk_heaps(
    layouts: &Layouts,
    syms: &SymbolTable,
    mem: &dyn GuestMemory,
) -> Result<HeapSnapshot, IntrospectError> {
    let heap_t = layouts.require("heap_t_")?;
    let info = layouts.require("multi_heap_info")?;
    let control = layouts.require("control_t")?;
    let head = crate::freertos::symbol(syms, "registered_heaps")?;
    let mut out = HeapSnapshot::default();
    let mut at = mem.u32(head)?;
    let mut seen: Vec<u32> = Vec::new();
    while at != 0 {
        if seen.contains(&at) {
            out.warnings.push(Warning {
                what: "registered_heaps",
                at,
                detail: "the region list revisits a heap, so it is a cycle".into(),
            });
            break;
        }
        if seen.len() as u32 >= MAX_REGIONS {
            out.warnings.push(Warning {
                what: "registered_heaps",
                at,
                detail: format!("the region list is longer than {MAX_REGIONS} heaps"),
            });
            break;
        }
        seen.push(at);
        let next = heap_t.u32(mem, at, "next").unwrap_or(0);
        match read_region(layouts, heap_t, info, control, mem, at, &mut out.warnings) {
            Ok(region) => out.regions.push(region),
            Err(e) => out.warnings.push(Warning {
                what: "heap_t_",
                at,
                detail: e.to_string(),
            }),
        }
        at = next;
    }
    Ok(out)
}

fn read_region(
    layouts: &Layouts,
    heap_t: &crate::layout::StructLayout,
    info: &crate::layout::StructLayout,
    control: &crate::layout::StructLayout,
    mem: &dyn GuestMemory,
    at: u32,
    warnings: &mut Vec<Warning>,
) -> Result<HeapRegion, IntrospectError> {
    let caps_at = at.wrapping_add(heap_t.offset("caps")?);
    let info_at = heap_t.u32(mem, at, "heap")?;
    let mut region = HeapRegion {
        heap_t: at,
        caps: [
            mem.u32(caps_at)?,
            mem.u32(caps_at.wrapping_add(4))?,
            mem.u32(caps_at.wrapping_add(8))?,
        ],
        start: heap_t.u32(mem, at, "start")?,
        end: heap_t.u32(mem, at, "end")?,
        info: info_at,
        free_bytes: info.u32(mem, info_at, "free_bytes")?,
        minimum_free_bytes: info.u32(mem, info_at, "minimum_free_bytes")?,
        pool_size: info.u32(mem, info_at, "pool_size")?,
        control: info.u32(mem, info_at, "heap_data")?,
        ..HeapRegion::default()
    };
    region.control_size = control.u32(mem, region.control, "size")?;
    region.sl_index_count = control.bits(mem, region.control, "sl_index_count")?;
    region.small_block_size = control.bits(mem, region.control, "small_block_size")?;
    walk_pool(layouts, mem, &mut region, warnings)?;
    region.largest_free_fit = fit_size(region.largest_free_raw, region.sl_index_count);
    Ok(region)
}

fn walk_pool(
    layouts: &Layouts,
    mem: &dyn GuestMemory,
    region: &mut HeapRegion,
    warnings: &mut Vec<Warning>,
) -> Result<(), IntrospectError> {
    let block = layouts.require("block_header_t")?;
    let pool = region.control.wrapping_add(region.control_size);
    let limit = u64::from(region.end).max(u64::from(pool));
    let mut at = pool.wrapping_sub(BLOCK_HEADER_OVERHEAD);
    for _ in 0..MAX_BLOCKS {
        let raw = match block.u32(mem, at, "size") {
            Ok(v) => v,
            Err(e) => {
                warnings.push(Warning {
                    what: "tlsf pool",
                    at,
                    detail: e.to_string(),
                });
                return Ok(());
            }
        };
        let size = raw & !(BLOCK_FREE | BLOCK_PREV_FREE);
        if size == 0 {
            return Ok(());
        }
        if raw & BLOCK_FREE != 0 {
            region.free_blocks += 1;
            region.largest_free_raw = region.largest_free_raw.max(size);
        } else {
            region.used_blocks += 1;
        }
        let next = at
            .wrapping_add(BLOCK_START_OFFSET)
            .wrapping_add(size)
            .wrapping_sub(BLOCK_HEADER_OVERHEAD);
        if next <= at {
            warnings.push(Warning {
                what: "tlsf pool",
                at,
                detail: format!(
                    "a block of {size} bytes does not advance the chain; the pool is not \
                     followed further"
                ),
            });
            return Ok(());
        }
        if u64::from(next) > limit {
            warnings.push(Warning {
                what: "tlsf pool",
                at,
                detail: format!(
                    "the next block {next:#010x} is past the region end {:#010x}; the pool is \
                     not followed further",
                    region.end
                ),
            });
            return Ok(());
        }
        at = next;
    }
    warnings.push(Warning {
        what: "tlsf pool",
        at,
        detail: format!("the block chain is longer than {MAX_BLOCKS} blocks"),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryImage;
    use crate::freertos::synthetic::symbols;
    use crate::layout::{Bitfield, MemberLayout, StructLayout};

    /// `registered_heaps`, as the synthetic FreeRTOS symbol table places it.
    const HEAPS: u32 = 0x3fca_1500;
    const HEAP_T: u32 = 0x3fcd_0000;
    const INFO: u32 = 0x3fcd_0100;
    /// The `control_t`, which is also `heap_data`.
    const CONTROL: u32 = 0x3fcd_1000;
    const CONTROL_SIZE: u32 = 64;
    const REGION_END: u32 = 0x3fcd_2000;
    /// The second block, which is free.
    const FREE_BLOCK: u32 = 0x3fcd_1140;

    fn member(path: &str, offset: u32) -> MemberLayout {
        MemberLayout {
            path: path.to_string(),
            offset,
            bits: None,
            size: None,
        }
    }

    fn bitfield(path: &str, offset: u32, bit: u8, width: u32) -> MemberLayout {
        MemberLayout {
            path: path.to_string(),
            offset,
            bits: Some(Bitfield { bit, width }),
            size: None,
        }
    }

    /// The heap layouts as the official ELF resolves them.
    fn layouts() -> Layouts {
        let mut l = Layouts::new();
        l.insert(StructLayout::new(
            "heap_t_",
            36,
            vec![
                member("caps", 0),
                member("start", 12),
                member("end", 16),
                member("heap_mux", 20),
                member("heap", 28),
                member("next", 32),
            ],
        ));
        l.insert(StructLayout::new(
            "multi_heap_info",
            20,
            vec![
                member("lock", 0),
                member("free_bytes", 4),
                member("minimum_free_bytes", 8),
                member("pool_size", 12),
                member("heap_data", 16),
            ],
        ));
        l.insert(StructLayout::new(
            "control_t",
            36,
            vec![
                member("size", 20),
                bitfield("sl_index_count", 17, 6, 6),
                bitfield("small_block_size", 18, 7, 8),
                bitfield("fl_index_count", 16, 0, 5),
            ],
        ));
        l.insert(StructLayout::new(
            "block_header_t",
            16,
            vec![member("prev_phys_block", 0), member("size", 4)],
        ));
        l
    }

    /// One region with a 256-byte used block, a 600-byte free block and the sentinel.
    fn image() -> MemoryImage {
        let mut mem = MemoryImage::new();
        mem.map_zeroed(HEAPS, 4);
        mem.map_zeroed(HEAP_T, 36);
        mem.map_zeroed(INFO, 20);
        mem.map_zeroed(CONTROL, 0x1000);
        mem.put_u32(HEAPS, HEAP_T);
        mem.put_u32(HEAP_T, 0x0000_1800); // caps[0]
        mem.put_u32(HEAP_T + 12, CONTROL); // start
        mem.put_u32(HEAP_T + 16, REGION_END); // end
        mem.put_u32(HEAP_T + 28, INFO); // heap
        mem.put_u32(HEAP_T + 32, 0); // next
        mem.put_u32(INFO + 4, 608); // free_bytes
        mem.put_u32(INFO + 8, 500); // minimum_free_bytes
        mem.put_u32(INFO + 12, 4096); // pool_size
        mem.put_u32(INFO + 16, CONTROL); // heap_data
        mem.put_u32(CONTROL + 20, CONTROL_SIZE);
        // fl_index_count 5 at bit 0, sl_index_count 16 at bit 14, small_block_size 64 at
        // bit 23 of the word at offset 16.
        mem.put_u32(CONTROL + 16, 5 | (16 << 14) | (64 << 23));
        mem.put_u32(CONTROL + CONTROL_SIZE - 4 + 4, 256);
        mem.put_u32(FREE_BLOCK + 4, 600 | BLOCK_FREE);
        mem.put_u32(FREE_BLOCK + 8 + 600 - 4 + 4, 0); // sentinel
        mem
    }

    #[test]
    fn the_heap_walk_reads_regions_blocks_and_the_largest_fit() {
        let snapshot = walk_heaps(&layouts(), &symbols(), &image()).expect("the walk succeeds");
        assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);
        let r = &snapshot.regions[0];
        assert_eq!(r.control_size, CONTROL_SIZE);
        assert_eq!(r.sl_index_count, 16);
        assert_eq!(r.small_block_size, 64);
        assert_eq!(r.caps, [0x0000_1800, 0, 0]);
        assert_eq!((r.used_blocks, r.free_blocks), (1, 1));
        assert_eq!(r.largest_free_raw, 600);
        // 600 rounds down to the second-level interval (1 << 9) / 16 = 32.
        assert_eq!(r.largest_free_fit, 576);
        assert_eq!(
            snapshot.render(),
            "0x3fcd1000-0x3fcd2000 free=608 min=500 blocks=1u/1f largest=600/576\n\
             total free=608 min=500 largest=576\n"
        );
    }

    /// Raw largest free block to fit size, for rows measured on the official firmware.
    #[test]
    fn fit_size_reproduces_the_measured_rows() {
        assert_eq!(fit_size(7_712, 8), 7_680);
        assert_eq!(fit_size(10_152, 8), 9_216);
        assert_eq!(fit_size(43_832, 16), 43_008);
        assert_eq!(fit_size(115_616, 16), 114_688);
        assert_eq!(fit_size(0, 16), 0);
        assert_eq!(fit_size(1_000, 0), 1_000);
        assert_eq!(fit_size(8, 16), 8);
    }

    #[test]
    fn a_corrupt_block_chain_is_reported_not_followed() {
        let mut mem = image();
        mem.put_u32(FREE_BLOCK + 4, 0xffff_fffc | BLOCK_FREE);
        let snapshot = walk_heaps(&layouts(), &symbols(), &mem).expect("the walk still returns");
        let w = &snapshot.warnings[0];
        assert_eq!(w.what, "tlsf pool");
        assert_eq!(w.at, FREE_BLOCK);
        assert!(w.detail.contains("does not advance the chain"));
        assert_eq!(snapshot.regions[0].used_blocks, 1);

        let mut mem = image();
        mem.put_u32(FREE_BLOCK + 4, 0x1_0000 | BLOCK_FREE);
        let snapshot = walk_heaps(&layouts(), &symbols(), &mem).expect("the walk still returns");
        assert!(snapshot.warnings[0].detail.contains("past the region end"));

        let mut mem = image();
        mem.put_u32(INFO + 16, 0x4000_0000);
        let snapshot = walk_heaps(&layouts(), &symbols(), &mem).expect("the walk still returns");
        assert!(!snapshot.warnings.is_empty());
    }

    #[test]
    fn a_cyclic_region_list_is_reported() {
        let mut mem = image();
        mem.put_u32(HEAP_T + 32, HEAP_T);
        let snapshot = walk_heaps(&layouts(), &symbols(), &mem).expect("the walk still returns");
        assert_eq!(snapshot.regions.len(), 1);
        let w = snapshot
            .warnings
            .iter()
            .find(|w| w.what == "registered_heaps")
            .expect("the cycle is reported");
        assert!(w.detail.contains("cycle"));
    }

    #[test]
    fn missing_heap_layouts_and_symbols_are_errors() {
        assert_eq!(
            walk_heaps(&Layouts::new(), &symbols(), &image()),
            Err(IntrospectError::MissingStruct { name: "heap_t_" })
        );
        let empty = pemu_loader::symbols::SymbolTable::default();
        assert_eq!(
            walk_heaps(&layouts(), &empty, &image()),
            Err(IntrospectError::MissingSymbol {
                name: "registered_heaps"
            })
        );
    }
}
