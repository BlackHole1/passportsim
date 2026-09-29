//! Storage and lookup of translated blocks: chain slots, then a jump cache indexed by `pc >> 1`
//! (RV32IMC PCs are 2-byte aligned, ESP32-C3 TRM chapter 1), then a `BTreeMap`, since
//! `cargo xtask layering` bans `HashMap` in core code for its nondeterministic iteration.
//!
//! An invalidated block's PC becomes [`INVALID_PC`] and every entry re-checks the PC, so stale
//! chain slots and jump-cache entries fall through to the map. Invalidation is per 4 KB page.

use std::collections::BTreeMap;

use crate::op::{self, Op};

pub const JUMP_CACHE_BITS: u32 = 16;

pub const JUMP_CACHE_ENTRIES: usize = 1 << JUMP_CACHE_BITS;

pub const CHAIN_SLOTS: usize = 2;

/// Taken successor of a branch, and the only successor of every other terminator.
pub const SLOT_TAKEN: usize = 0;

pub const SLOT_FALL: usize = 1;

pub const NO_BLOCK: u32 = u32::MAX;

/// `u32::MAX` is odd, so it is never a legal guest PC.
pub const INVALID_PC: u32 = u32::MAX;

pub const PAGE_SHIFT: u32 = 12;

pub const PAGE_SIZE: u32 = 1 << PAGE_SHIFT;

/// Ops held before the next translation flushes the cache (16 MB).
pub const OPS_WATERMARK: usize = 1 << 20;

/// 32 bytes, so block indexing is a shift and a block sits in one cache line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(align(32))]
pub struct Block {
    pub pc: u32,
    pub ops_start: u32,
    pub n_ops: u32,
    pub n_insns: u32,
    /// Differs from [`Block::vpn`] when the last instruction straddles a page boundary.
    pub end_vpn: u32,
    pub next: [u32; CHAIN_SLOTS],
    pub n_stores: u32,
}

const _: () = assert!(size_of::<Block>() == 32, "a Block is no longer 32 bytes");

impl Block {
    #[inline(always)]
    pub fn is_valid(&self) -> bool {
        self.pc != INVALID_PC
    }

    #[inline]
    pub fn vpn(&self) -> u32 {
        self.pc >> PAGE_SHIFT
    }
}

pub struct BlockCache {
    blocks: Vec<Block>,
    ops: Vec<Op>,
    /// Heap-allocated: a 256 KB array would overflow the 1 MB stack a Windows `.exe` gets.
    jc: Box<[u32]>,
    map: BTreeMap<u32, u32>,
    /// A straddling block is listed under both its pages.
    page_blocks: BTreeMap<u32, Vec<u32>>,
    free: Vec<u32>,
    /// Bumped by every flush and invalidation, so a stale resume token is rejected.
    generation: u64,
}

impl Default for BlockCache {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockCache {
    pub fn new() -> BlockCache {
        BlockCache {
            blocks: Vec::new(),
            ops: Vec::new(),
            jc: vec![NO_BLOCK; JUMP_CACHE_ENTRIES].into_boxed_slice(),
            map: BTreeMap::new(),
            page_blocks: BTreeMap::new(),
            free: Vec::new(),
            generation: 0,
        }
    }

    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[inline(always)]
    pub fn block(&self, id: u32) -> &Block {
        &self.blocks[id as usize]
    }

    /// [`BlockCache::block`] without the bounds check, which V8 kept in the chained block loop.
    ///
    /// # Safety
    ///
    /// `id` came from [`BlockCache::finish`], a lookup, or a chain slot of this cache, with no
    /// flush since. Such an id stays in range: `blocks` only grows until a flush (freed slots are
    /// reused in place and start unlinked), and [`BlockCache::link`] refuses out-of-range ids.
    #[inline(always)]
    pub(crate) unsafe fn block_unchecked(&self, id: u32) -> &Block {
        debug_assert!(
            (id as usize) < self.blocks.len(),
            "block id {id} out of range"
        );
        // SAFETY: the caller's contract above.
        unsafe { self.blocks.get_unchecked(id as usize) }
    }

    /// [`BlockCache::block_run`] from the first op, without bounds checks.
    ///
    /// # Safety
    ///
    /// `block` was read from this cache since its last flush, so its op range (at least one op,
    /// set by [`BlockCache::finish`]) is inside `ops`, which only grows until a flush.
    #[inline(always)]
    pub(crate) unsafe fn block_run_unchecked(&self, block: &Block) -> (&[Op], &Op) {
        let start = block.ops_start as usize;
        let term = start + block.n_ops as usize - 1;
        debug_assert!(
            block.n_ops > 0 && term < self.ops.len(),
            "block op range {start}..={term} outside op storage of {}",
            self.ops.len()
        );
        // SAFETY: the caller's contract above: `start..=term` is inside `ops`.
        unsafe {
            (
                self.ops.get_unchecked(start..term),
                self.ops.get_unchecked(term),
            )
        }
    }

    #[inline(always)]
    pub fn block_ops(&self, block: &Block) -> &[Op] {
        let start = block.ops_start as usize;
        &self.ops[start..start + block.n_ops as usize]
    }

    /// The plain ops and the terminator a run of `block` entered at op `from` still executes.
    #[inline(always)]
    pub fn block_run(&self, block: &Block, from: u32) -> (&[Op], &Op) {
        let start = block.ops_start as usize;
        let term = start + block.n_ops as usize - 1;
        let ops = &self.ops[start + from as usize..=term];
        let (term, plain) = ops
            .split_last()
            .expect("every block ends in a terminator (BlockCache::finish)");
        (plain, term)
    }

    /// `next` is checked because the chained loop reads it unchecked; `slot` is only masked.
    #[inline]
    pub fn link(&mut self, id: u32, slot: usize, next: u32) {
        assert!(
            next == NO_BLOCK || (next as usize) < self.blocks.len(),
            "chain slot linked to block {next}, which this cache does not hold"
        );
        self.blocks[id as usize].next[slot & (CHAIN_SLOTS - 1)] = next;
    }

    #[inline(always)]
    pub const fn jc_index(pc: u32) -> usize {
        ((pc >> 1) as usize) & (JUMP_CACHE_ENTRIES - 1)
    }

    /// `Some(id)` only for a valid block starting at `pc`.
    #[inline(always)]
    pub fn jump_cache_hit(&self, pc: u32) -> Option<u32> {
        let id = self.jc[Self::jc_index(pc)];
        if id != NO_BLOCK && self.blocks[id as usize].pc == pc {
            Some(id)
        } else {
            None
        }
    }

    /// Fills the jump cache on a hit.
    pub fn map_hit(&mut self, pc: u32) -> Option<u32> {
        let id = *self.map.get(&pc)?;
        self.jc[Self::jc_index(pc)] = id;
        Some(id)
    }

    #[inline]
    pub fn over_watermark(&self) -> bool {
        self.ops.len() >= OPS_WATERMARK
    }

    /// A translation is `push_op` per op and then one [`BlockCache::finish`].
    #[inline]
    pub fn push_op(&mut self, op: Op) {
        self.ops.push(op);
    }

    #[inline]
    pub fn build_start(&self) -> u32 {
        self.ops.len() as u32
    }

    #[inline]
    pub fn building(&self, start: u32) -> &[Op] {
        &self.ops[start as usize..]
    }

    /// For the one-for-one rewrite `EngineCfg::fuser` may return.
    #[inline]
    pub fn replace_last(&mut self, op: Op) {
        *self
            .ops
            .last_mut()
            .expect("a block under construction holds at least the op just pushed") = op;
    }

    /// Registers the block built from `start`; `last_byte` ends its last instruction.
    pub fn finish(&mut self, pc: u32, start: u32, n_insns: u32, last_byte: u32) -> u32 {
        let vpn = pc >> PAGE_SHIFT;
        let end_vpn = last_byte >> PAGE_SHIFT;
        let n_stores = self.ops[start as usize..]
            .iter()
            .filter(|op| op::is_store(op.kind))
            .count() as u32;
        // The chained block loop relies on every block holding its terminator.
        assert!(
            (start as usize) < self.ops.len(),
            "a block is finished with at least one op"
        );
        let block = Block {
            pc,
            ops_start: start,
            n_ops: self.ops.len() as u32 - start,
            n_insns,
            end_vpn,
            next: [NO_BLOCK; CHAIN_SLOTS],
            n_stores,
        };
        let id = match self.free.pop() {
            Some(id) => {
                self.blocks[id as usize] = block;
                id
            }
            None => {
                self.blocks.push(block);
                (self.blocks.len() - 1) as u32
            }
        };
        self.map.insert(pc, id);
        self.page_blocks.entry(vpn).or_default().push(id);
        if end_vpn != vpn {
            self.page_blocks.entry(end_vpn).or_default().push(id);
        }
        self.jc[Self::jc_index(pc)] = id;
        id
    }

    /// The SoC sets `PF_CODE` on such a page so that stores into it take the slow path.
    pub fn page_is_translated(&self, vpn: u32) -> bool {
        self.page_blocks.contains_key(&vpn)
    }

    pub fn translated_pages(&self) -> impl Iterator<Item = u32> + '_ {
        self.page_blocks.keys().copied()
    }

    pub fn page_block_ids(&self, vpn: u32) -> &[u32] {
        self.page_blocks
            .get(&vpn)
            .map_or(&[][..], |ids| ids.as_slice())
    }

    pub fn invalidate_page(&mut self, vpn: u32) -> usize {
        let Some(ids) = self.page_blocks.remove(&vpn) else {
            return 0;
        };
        for id in &ids {
            let block = &mut self.blocks[*id as usize];
            if !block.is_valid() {
                continue;
            }
            let start_vpn = block.pc >> PAGE_SHIFT;
            let end_vpn = block.end_vpn;
            self.map.remove(&block.pc);
            block.pc = INVALID_PC;
            self.free.push(*id);
            // Drop a straddling block from its other page too, or a reused slot would later be
            // invalidated under a block it no longer holds.
            let other = if start_vpn == vpn { end_vpn } else { start_vpn };
            if other != vpn
                && let Some(list) = self.page_blocks.get_mut(&other)
            {
                list.retain(|other_id| other_id != id);
                if list.is_empty() {
                    self.page_blocks.remove(&other);
                }
            }
        }
        self.generation = self.generation.wrapping_add(1);
        ids.len()
    }

    /// A zero `len` covers the page of `vaddr`; the range clamps at the address-space end.
    pub fn invalidate_vrange(&mut self, vaddr: u32, len: u32) -> usize {
        let last = vaddr.saturating_add(len.saturating_sub(1));
        let first_page = vaddr >> PAGE_SHIFT;
        let last_page = last >> PAGE_SHIFT;
        let mut count = 0;
        for vpn in first_page..=last_page {
            count += self.invalidate_page(vpn);
        }
        count
    }

    /// [`BlockCache::invalidate_vrange`] over translated pages only, for 8 MB flash ranges.
    pub fn invalidate_translated_in(&mut self, vaddr: u32, len: u32) -> usize {
        let last = vaddr.saturating_add(len.saturating_sub(1));
        let pages: Vec<u32> = self
            .page_blocks
            .range((vaddr >> PAGE_SHIFT)..=(last >> PAGE_SHIFT))
            .map(|(vpn, _)| *vpn)
            .collect();
        pages.into_iter().map(|vpn| self.invalidate_page(vpn)).sum()
    }

    /// Drops every translation and compacts op storage (`fence.i`, ROM reload, reset).
    pub fn flush(&mut self) {
        self.blocks.clear();
        self.ops.clear();
        self.map.clear();
        self.page_blocks.clear();
        self.free.clear();
        self.jc.fill(NO_BLOCK);
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn op_count(&self) -> usize {
        self.ops.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{K_ADDI, K_FALL, flags_for};

    fn op(kind: u8, pc_off: u16, len: u8) -> Op {
        Op {
            kind,
            rd: 0,
            rs1: 0,
            rs2: 0,
            imm: 0,
            imm2: 0,
            len,
            flags: flags_for(kind, 0),
            pc_off,
        }
    }

    fn build(cache: &mut BlockCache, pc: u32) -> u32 {
        let start = cache.build_start();
        cache.push_op(op(K_ADDI, 0, 4));
        cache.push_op(op(K_FALL, 4, 0));
        cache.finish(pc, start, 1, pc + 3)
    }

    #[test]
    fn a_fresh_cache_finds_nothing() {
        let mut cache = BlockCache::new();
        assert_eq!(cache.jump_cache_hit(0x4200_0000), None);
        assert_eq!(cache.map_hit(0x4200_0000), None);
        assert_eq!(cache.generation(), 0);
        assert!(!cache.page_is_translated(0x4_2000));
    }

    #[test]
    fn a_finished_block_is_found_by_both_tiers_and_indexed_by_page() {
        let mut cache = BlockCache::new();
        let id = build(&mut cache, 0x4200_0000);
        assert_eq!(cache.jump_cache_hit(0x4200_0000), Some(id));
        assert_eq!(cache.map_hit(0x4200_0000), Some(id));
        assert_eq!(cache.block(id).n_ops, 2);
        assert_eq!(cache.block(id).n_insns, 1);
        assert_eq!(cache.block(id).next, [NO_BLOCK; CHAIN_SLOTS]);
        assert!(cache.page_is_translated(0x4_2000));
        assert_eq!(cache.page_block_ids(0x4_2000), &[id]);
        assert_eq!(cache.translated_pages().collect::<Vec<_>>(), vec![0x4_2000]);
    }

    #[test]
    fn colliding_pcs_evict_each_other_in_the_jump_cache_but_not_in_the_map() {
        let mut cache = BlockCache::new();
        let stride = 1u32 << (JUMP_CACHE_BITS + 1);
        let a = build(&mut cache, 0x4200_0000);
        let b = build(&mut cache, 0x4200_0000 + stride);
        assert_eq!(
            BlockCache::jc_index(0x4200_0000),
            BlockCache::jc_index(0x4200_0000 + stride)
        );
        assert_eq!(cache.jump_cache_hit(0x4200_0000 + stride), Some(b));
        assert_eq!(cache.jump_cache_hit(0x4200_0000), None);
        assert_eq!(cache.map_hit(0x4200_0000), Some(a));
        // The map hit refilled the jump cache, evicting `b` in turn.
        assert_eq!(cache.jump_cache_hit(0x4200_0000), Some(a));
    }

    #[test]
    fn invalidating_a_page_unlinks_its_blocks_and_bumps_the_generation() {
        let mut cache = BlockCache::new();
        let a = build(&mut cache, 0x4200_0000);
        let b = build(&mut cache, 0x4200_0100);
        cache.link(a, SLOT_TAKEN, b);
        let generation = cache.generation();
        assert_eq!(cache.invalidate_page(0x4_2000), 2);
        assert_ne!(cache.generation(), generation);
        assert!(!cache.block(a).is_valid());
        assert!(!cache.block(b).is_valid());
        assert_eq!(cache.jump_cache_hit(0x4200_0000), None);
        assert_eq!(cache.map_hit(0x4200_0000), None);
        assert!(!cache.page_is_translated(0x4_2000));
        // The stale chain slot still names `b`, and the PC check is what rejects it.
        assert_eq!(cache.block(a).next[SLOT_TAKEN], b);
        assert_ne!(cache.block(b).pc, 0x4200_0100);
    }

    #[test]
    fn a_freed_block_slot_is_reused_by_the_next_translation() {
        let mut cache = BlockCache::new();
        let a = build(&mut cache, 0x4200_0000);
        assert_eq!(cache.block_count(), 1);
        cache.invalidate_page(0x4_2000);
        let b = build(&mut cache, 0x4300_0000);
        assert_eq!(b, a, "the freed slot is reused");
        assert_eq!(cache.block_count(), 1);
        assert_eq!(cache.jump_cache_hit(0x4300_0000), Some(b));
    }

    #[test]
    fn a_range_invalidation_covers_every_page_it_touches() {
        let mut cache = BlockCache::new();
        build(&mut cache, 0x4200_0000);
        build(&mut cache, 0x4200_1000);
        build(&mut cache, 0x4200_2000);
        build(&mut cache, 0x4200_3000);
        // From the last byte of page 0 across page 1 into page 2.
        assert_eq!(cache.invalidate_vrange(0x4200_0FFF, PAGE_SIZE + 2), 3);
        assert!(!cache.page_is_translated(0x4_2000));
        assert!(!cache.page_is_translated(0x4_2001));
        assert!(!cache.page_is_translated(0x4_2002));
        assert!(cache.page_is_translated(0x4_2003));
    }

    #[test]
    fn a_zero_length_range_invalidates_the_page_of_its_address() {
        let mut cache = BlockCache::new();
        build(&mut cache, 0x4200_0000);
        assert_eq!(cache.invalidate_vrange(0x4200_0800, 0), 1);
        assert!(!cache.page_is_translated(0x4_2000));
    }

    #[test]
    fn a_range_at_the_end_of_the_address_space_does_not_wrap() {
        let mut cache = BlockCache::new();
        build(&mut cache, 0x0000_0000);
        build(&mut cache, 0xFFFF_F000);
        assert_eq!(cache.invalidate_vrange(0xFFFF_F000, u32::MAX), 1);
        assert!(cache.page_is_translated(0));
        assert!(!cache.page_is_translated(0xF_FFFF));
    }

    #[test]
    fn a_flush_drops_every_block_and_compacts_op_storage() {
        let mut cache = BlockCache::new();
        build(&mut cache, 0x4200_0000);
        build(&mut cache, 0x4200_1000);
        assert_eq!(cache.op_count(), 4);
        let generation = cache.generation();
        cache.flush();
        assert_ne!(cache.generation(), generation);
        assert_eq!(cache.block_count(), 0);
        assert_eq!(cache.op_count(), 0);
        assert_eq!(cache.jump_cache_hit(0x4200_0000), None);
        assert_eq!(cache.map_hit(0x4200_0000), None);
        assert_eq!(cache.translated_pages().count(), 0);
    }

    #[test]
    fn the_ops_watermark_reports_only_when_storage_is_full() {
        let mut cache = BlockCache::new();
        assert!(!cache.over_watermark());
        build(&mut cache, 0x4200_0000);
        assert!(!cache.over_watermark());
    }

    #[test]
    fn block_ops_hands_back_exactly_the_range_of_the_block() {
        let mut cache = BlockCache::new();
        let a = build(&mut cache, 0x4200_0000);
        let b = build(&mut cache, 0x4200_1000);
        let ops_a = cache.block_ops(cache.block(a));
        assert_eq!(ops_a.len(), 2);
        assert_eq!(ops_a[0].kind, K_ADDI);
        assert_eq!(ops_a[1].kind, K_FALL);
        assert_eq!(cache.block(b).ops_start, 2);
        assert_eq!(cache.block(a).vpn(), 0x4_2000);
        assert_eq!(cache.block(b).vpn(), 0x4_2001);
    }

    #[test]
    #[should_panic(expected = "which this cache does not hold")]
    fn a_chain_slot_cannot_name_a_block_the_cache_does_not_hold() {
        let mut cache = BlockCache::new();
        let a = build(&mut cache, 0x4200_0000);
        cache.link(a, SLOT_TAKEN, a + 1);
    }

    #[test]
    fn a_chain_slot_can_be_unlinked() {
        let mut cache = BlockCache::new();
        let a = build(&mut cache, 0x4200_0000);
        cache.link(a, SLOT_TAKEN, a);
        cache.link(a, SLOT_TAKEN, NO_BLOCK);
        assert_eq!(cache.block(a).next[SLOT_TAKEN], NO_BLOCK);
    }

    #[test]
    #[should_panic(expected = "at least one op")]
    fn a_block_with_no_op_is_refused() {
        let mut cache = BlockCache::new();
        let start = cache.build_start();
        cache.finish(0x4200_0000, start, 0, 0x4200_0000);
    }

    #[test]
    fn the_unchecked_reads_agree_with_the_checked_ones() {
        let mut cache = BlockCache::new();
        let _ = build(&mut cache, 0x4200_0000);
        let b = build(&mut cache, 0x4200_0010);
        let block = cache.block(b);
        // SAFETY: `b` was just returned by `finish` and the cache has not been flushed since.
        let (unchecked, (plain, term)) =
            unsafe { (cache.block_unchecked(b), cache.block_run_unchecked(block)) };
        assert!(std::ptr::eq(unchecked, block));
        let (checked_plain, checked_term) = cache.block_run(block, 0);
        assert!(std::ptr::eq(plain, checked_plain));
        assert!(std::ptr::eq(term, checked_term));
    }
}
