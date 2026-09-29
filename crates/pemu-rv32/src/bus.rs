//! The `Bus` trait the CPU executes against: the hart view its slow paths see, the access result,
//! and the page table and flags the inlined fast paths read. The faults it returns are the
//! exceptions of the ESP32-C3 TRM chapter 1. `Bus` is a frozen interface.
//!
//! `PageTable` and the `PF_*` flags live here rather than in `pemu-soc-c3`, which depends on this
//! crate and re-exports them, because `Bus::pages` returns one and the fast paths test the flags.

use crate::csr::{Csr, CsrEffect, CsrOp};
use crate::spmon::SpMonitor;
use crate::trap::Trap;

pub const PF_R: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_X: u32 = 4;
/// Translated code in this page: stores take the slow path.
pub const PF_CODE: u32 = 8;
/// PMP/PMS partial page, watchpoint, or var matcher: data accesses take the slow path.
pub const PF_SLOW: u32 = 16;
pub const PF_MMIO: u32 = 32;
/// `device` profile: the next DBUS/DROM data load charges a cache-fill stall and clears this; it
/// returns OkStop so the run loop recomputes its budget.
pub const PF_COLD: u32 = 64;

/// One entry per 4 KB virtual page: arena page offset | `PF_*` flags; the SoC owns the encoding.
pub struct PageTable {
    entries: Box<[u32; 1 << 20]>,
}

impl PageTable {
    pub const PAGES: usize = 1 << 20;

    pub fn new() -> Self {
        let entries: Box<[u32; 1 << 20]> = vec![0u32; Self::PAGES]
            .into_boxed_slice()
            .try_into()
            .expect("the vector has PageTable::PAGES entries");
        PageTable { entries }
    }

    #[inline]
    pub fn entry(&self, vaddr: u32) -> u32 {
        self.entries[(vaddr >> 12) as usize]
    }

    #[inline]
    pub fn set_entry(&mut self, vpn: u32, entry: u32) {
        self.entries[vpn as usize] = entry;
    }

    #[inline]
    pub fn entries(&self) -> &[u32; 1 << 20] {
        &self.entries
    }
}

impl Default for PageTable {
    fn default() -> Self {
        Self::new()
    }
}

/// What the bus slow paths see of the hart, so the SoC derives the time at the accessing
/// instruction from [`HartView::pos`].
#[derive(Copy, Clone)]
pub struct HartView {
    pub insns: u64,
    /// Cycles the class costs charged beyond one per retired instruction; 0 under `fast`.
    pub extra: u64,
    pub pc: u32,
}

impl HartView {
    #[inline]
    pub fn pos(&self) -> u64 {
        self.insns.wrapping_add(self.extra)
    }
}

pub enum Access<T> {
    Ok(T),
    /// Finish this instruction, then leave the block.
    OkStop(T),
    Fault(Trap),
}

pub struct CodePage<'a> {
    /// Starts at the requested vaddr and runs to at least the end of its 4 KB page.
    pub bytes: &'a [u8],
}

/// The address space, custom CSRs and interrupt wake-up as the CPU sees them.
pub trait Bus {
    fn pages(&self) -> &PageTable;
    /// Base of the arena that `PageTable` entries offset into.
    fn arena(&mut self) -> *mut u8;
    fn load_slow(&mut self, addr: u32, size: u8, hart: &HartView) -> Access<u32>;
    /// MMIO writes see the exact write-time now.
    fn store_slow(&mut self, addr: u32, size: u8, val: u32, hart: &HartView) -> Access<()>;
    /// Re-pulled into `Hart::spmon` after any OkStop.
    fn sp_monitor(&self) -> SpMonitor;
    /// Executable bytes at `vaddr` ([`CodePage::bytes`]).
    fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap>;
    /// `insns` is the clock position (`Hart::pos`), the instruction count under `fast`.
    fn csr_custom(&mut self, csr: u16, op: CsrOp, insns: u64) -> Result<(u32, CsrEffect), Trap>;
    /// Any pending INTC line, ignoring MIE.
    fn wfi_wake(&mut self) -> bool;
    /// Recompute page permissions.
    fn pmp_changed(&mut self, csr: &Csr);
    /// The fetch side of the cache model: the hart is about to fetch at `pc` inside the watched
    /// window at clock position `insns`. Returns whether the fetch stalled; the caller then ends
    /// the run before the instruction so the run loop recomputes its budget. Called at a run's
    /// first instruction and at every unchained block entry; the engine never chains into a
    /// watched block of another cache line, so the bus sees every line change.
    fn fetch_enter(&mut self, insns: u64, pc: u32) -> bool {
        let _ = (insns, pc);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_table_starts_unmapped_and_indexes_by_page() {
        let mut pt = PageTable::default();
        assert_eq!(pt.entries().len(), PageTable::PAGES);
        assert_eq!(pt.entry(0x4038_0123), 0);
        pt.set_entry(0x4038_0123 >> 12, 0x1000 | PF_R | PF_X);
        assert_eq!(pt.entry(0x4038_0FFF), 0x1000 | PF_R | PF_X);
        assert_eq!(pt.entry(0x4038_1000), 0);
    }
}
