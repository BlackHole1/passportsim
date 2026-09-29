//! The in-block resume token of the exact-budget engine. The guest-visible CPU behavior it must
//! not change follows the ESP32-C3 TRM chapter 1.
//!
//! `Engine::run` retires exactly `max_insns` instructions, so a block that would cross the budget
//! runs only the prefix that fits. [`ResumeToken`] lets the next `run` re-enter that block;
//! translating a new block mid-block instead would fill the cache with suffixes of blocks it
//! already holds.
//!
//! A mismatched token is not an error: the engine drops it and looks the PC up, so the token never
//! changes what the guest sees. Like the block cache, it is never snapshotted.

/// Where inside a block the next `Engine::run` continues. `block_pc` and `pc` make a stale token
/// detectable rather than merely unlikely.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResumeToken {
    /// Block id in `crate::cache::BlockCache`.
    pub block: u32,
    /// Index of the next op inside that block.
    pub op_index: u32,
    /// `crate::cache::BlockCache::generation` when the token was taken.
    pub generation: u64,
    /// Start PC of the block, re-checked against the block itself.
    pub block_pc: u32,
    /// PC of the op the token names, re-checked against `Hart::pc`.
    pub pc: u32,
}

impl ResumeToken {
    /// True when the token still names the op it was taken at, so the engine may re-enter the
    /// block instead of translating.
    #[inline]
    pub fn matches(&self, generation: u64, block_pc: u32, hart_pc: u32) -> bool {
        self.generation == generation && self.block_pc == block_pc && self.pc == hart_pc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> ResumeToken {
        ResumeToken {
            block: 7,
            op_index: 3,
            generation: 42,
            block_pc: 0x4038_0000,
            pc: 0x4038_000C,
        }
    }

    #[test]
    fn a_token_matches_its_own_generation_block_and_pc() {
        assert!(token().matches(42, 0x4038_0000, 0x4038_000C));
    }

    #[test]
    fn a_flush_or_an_invalidation_rejects_the_token() {
        assert!(!token().matches(43, 0x4038_0000, 0x4038_000C));
    }

    #[test]
    fn a_reused_block_slot_rejects_the_token() {
        assert!(!token().matches(42, 0x4038_1000, 0x4038_000C));
    }

    /// The run loop took an interrupt between runs, so the hart PC is the vector.
    #[test]
    fn a_moved_hart_pc_rejects_the_token() {
        assert!(!token().matches(42, 0x4038_0000, 0x4038_0400));
    }
}
