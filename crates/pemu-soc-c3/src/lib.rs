//! ESP32-C3 SoC (TRM system and memory chapter): memory arena, page table, MMU and EXTMEM,
//! flash store, MMIO map, interrupt fabric, GDMA view, flash-cache accounting, one file per
//! peripheral and one file per wiring effect.
//!
//! [`Soc`] owns the bytes and decides what an address means: [`mem`] is the map, [`pagetable`] the
//! fast-path table over it, [`mmio`] the dispatch into [`periph`], and [`flash_store`] the 8 MB
//! behind the MMU. [`SocBus`] adapts it all to the CPU's `Bus`.
//!
//! Everything that only moves bytes, and the counter CSRs ([`Soc::counter_csr`], which need only
//! the [`Clock`]), lives on [`Soc`] and is tested without a peripheral context; [`SocBus`] adds
//! the peripherals and the board.
//!
//! Access outcomes: an address nothing backs reads 0, ignores the write and is logged, unless a
//! locked PMP entry denies it, which traps. A store into a range the map makes read-only (ROM, a
//! flash window) is ignored and logged. A backed access the PMP and PMS fold denies traps
//! (mcause 5 or 7) and is counted, not logged: it is modelled behavior, not a fidelity gap.

pub mod cold;
pub mod dma;
pub mod flash_store;
pub mod r#gen;
pub mod intc;
pub mod mem;
pub mod mmio;
pub mod mmu;
pub mod pagetable;
pub mod periph;
pub mod regs;
pub mod wiring;

use std::fmt;

use pemu_board::traits::BoardPorts;
use pemu_core::clock::Clock;
use pemu_core::fidelity::{FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::sched::PeriphId;
use pemu_core::time::VTime;
use pemu_rv32::bus::{Access, Bus, CodePage, HartView, PageTable};
use pemu_rv32::csr::{
    CSR_MPCCR, CSR_MPCER, CSR_MPCMR, CSR_UPCCR, CSR_UPCER, CSR_UPCMR, Csr, CsrEffect, CsrOp,
    MPCER_CYCLE, MPCER_WRITE_MASK, MPCMR_COUNT_EN, MPCMR_WRITE_MASK,
};
use pemu_rv32::pmp::AccessKind;
use pemu_rv32::spmon::SpMonitor;
use pemu_rv32::trap::Trap;

use crate::flash_store::{FlashError, FlashStore};
use crate::mem::Arena;
use crate::pagetable::{PF_CODE, PF_X};
use crate::periph::{Cx, Devices, Wiring};
use crate::wiring::protection::{PmpState, Pms, Protection};

/// Base of the SYSTEM block (`c3_devices!`).
const SYSTEM_BASE: u32 = 0x600C_0000;

/// Ledger id of an access to an address nothing backs outside the peripheral window, whose `off`
/// is the absolute address. Not [`crate::periph::id::UNMAPPED`], whose `off` is window-relative:
/// the ledger deduplicates on `(periph, off)`, so one id would let a hole at 0x600D1000 swallow
/// an access to 0x000D1000.
pub const UNBACKED: PeriphId = PeriphId(u16::MAX - 1);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SocError {
    /// The ROM image does not fit in the 0x60000-byte ROM window.
    RomLen(usize),
    Flash(FlashError),
}

impl fmt::Display for SocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SocError::RomLen(len) => write!(
                f,
                "ROM image is {len} bytes, more than the {} of the ROM window",
                mem::ROM_LEN
            ),
            SocError::Flash(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SocError {}

impl From<FlashError> for SocError {
    fn from(e: FlashError) -> SocError {
        SocError::Flash(e)
    }
}

/// What a store into memory did.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Stored {
    /// The bytes were written; `invalidated` when the store hit a page of translated code.
    Wrote {
        /// A page of translated code was written, so the engine must drop its blocks.
        invalidated: bool,
    },
    /// The map makes the range read-only (ROM, a flash window): ignored and logged.
    ReadOnly,
    /// The map allows the store and the PMP and PMS fold denies it: nothing written, mcause 7.
    Denied,
    /// Nothing backs the range: ignored and logged.
    Unbacked,
}

/// Why a load could not be answered: an address nothing backs reads 0 and is a fidelity gap, an
/// address protection denies is a trap and is not.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Nothing backs the address, or it is in the peripheral window: reads 0 and is logged.
    Unbacked,
    /// The map backs the address and the PMP and PMS fold denies the read: mcause 5, `mtval` =
    /// the address.
    Denied,
}

impl Refused {
    /// The trap this refusal owes the guest, or `None` when the load reads 0 and is logged.
    pub fn trap(self, addr: u32) -> Option<Trap> {
        match self {
            Refused::Denied => Some(Trap::load_access_fault(addr)),
            Refused::Unbacked => None,
        }
    }
}

impl Stored {
    /// The trap this outcome owes the guest, or `None` when the store is ignored and logged. Only
    /// a range protection closed faults.
    pub fn trap(self, addr: u32) -> Option<Trap> {
        match self {
            Stored::Denied => Some(Trap::store_access_fault(addr)),
            Stored::Wrote { .. } | Stored::ReadOnly | Stored::Unbacked => None,
        }
    }
}

/// The SoC state behind [`SocBus`]: arena, page table, MMIO devices and flash store. The
/// interrupt fabric sits in the machine's [`Cx`] instead, because a slow path hands
/// `Cx::irq` to a peripheral while [`SocBus`] already holds `&mut Soc`.
pub struct Soc {
    pub arena: Arena,
    pub pages: PageTable,
    pub devices: Devices,
    pub flash: FlashStore,
    invalidated: Vec<u32>,
    /// Virtual page numbers marked `PF_SLOW` for a reason the PMP and PMS fold does not know: a
    /// watchpoint or a var matcher ([`Soc::mark_slow`]).
    slow_marks: Vec<u32>,
    wiring: Vec<Wiring>,
    /// Stack monitor, re-pulled after any OkStop; ASSIST_DEBUG sets it through
    /// `Wiring::SpMonitor`.
    sp_monitor: SpMonitor,
    unbacked: u64,
    /// Stores into a backed but read-only range, counted apart from `unbacked`.
    readonly_stores: u64,
    /// Peripheral-window accesses of a width no register access uses (`mmio::reg_size`). The
    /// engine only issues 1, 2 and 4, so this stays 0.
    bad_width: u64,
    unknown_csrs: u64,
    /// `mpcer` as last written through CSR 0x7E0 or alias 0x800. The [`Csr`] store is the
    /// architectural copy; this one exists because enabling the counter needs `mpcer.CYCLE` and
    /// `mpcmr.COUNT_EN` together and `Bus::csr_custom` sees one CSR at a time.
    pcer: u32,
    /// `mpcmr` as last written through CSR 0x7E1 or alias 0x801; see `pcer`.
    pcmr: u32,
    protection_changes: u64,
    /// Loads and stores the fold denied on a backed page (mcause 5 or 7). Not a fidelity gap, so
    /// counted rather than logged.
    protection_faults: u64,
    /// The PMP entries as of the last fold, so a `PF_SLOW` page can be decided byte by byte
    /// ([`Protection::byte_flags`]). `Bus::pmp_changed` is the only writer.
    pub pmp: PmpState,
    /// The SENSITIVE PMS split folded into the page entries beside PMP. It stays [`Pms::OPEN`]
    /// until a SENSITIVE model decodes the split and the machine applies
    /// `Wiring::ProtectionChanged`, so a PMP write refolds with whatever split is here.
    pub pms: Pms,
    /// The flash-cache stall account ([`cold`]); the machine installs the profile's model and
    /// fill timing. Under `fast` it charges nothing.
    pub cache: cold::CacheAccount,
}

/// Every page base whose entry covers the same arena bytes as `addr`: the aliases of
/// [`mem::views_of`], or the page of `addr` itself when no region backs it (a flash-window page,
/// whose DROM and IROM views `crate::wiring::mmu` pairs).
fn views_of_page(addr: u32) -> impl Iterator<Item = u32> {
    let own = mem::region_of(addr)
        .is_none()
        .then_some(addr & !pagetable::FLAG_MASK);
    mem::views_of(addr).chain(own)
}

/// What one byte of a slow-path access resolved to. `map` is what the memory map grants and
/// `allowed` what the PMP and PMS fold leaves, so a refusal can tell unbacked, read-only and
/// denied apart.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct Byte {
    off: usize,
    /// Flags the map gives the page: the [`mem::Region`] row's, or the entry's for a page the
    /// table maps that no row covers.
    map: u32,
    allowed: u32,
}

fn note_touch(
    ledger: &mut FidelityLedger,
    now: VTime,
    periph: PeriphId,
    off: u32,
    size: u8,
    access: TouchAccess,
) {
    ledger.first_touch(FirstTouch {
        periph,
        off,
        access,
        size,
        now,
        allowlisted: false,
    });
}

impl Soc {
    pub fn new(flash: FlashStore) -> Soc {
        let mut pages = PageTable::new();
        mem::write_identity_pages(&mut pages);
        Soc {
            arena: Arena::new(),
            pages,
            devices: Devices::default(),
            flash,
            invalidated: Vec::new(),
            slow_marks: Vec::new(),
            wiring: Vec::new(),
            cache: cold::CacheAccount::fast(),
            sp_monitor: SpMonitor::default(),
            unbacked: 0,
            readonly_stores: 0,
            bad_width: 0,
            unknown_csrs: 0,
            // CPU reset values: mpcer 0 (CYCLE clear) and mpcmr 0b11 (COUNT_EN set), as
            // `Csr::new`, so the counter is disabled until the guest sets CYCLE.
            pcer: 0,
            pcmr: MPCMR_WRITE_MASK,
            protection_changes: 0,
            protection_faults: 0,
            pmp: PmpState::RESET,
            pms: Pms::OPEN,
        }
    }

    /// Copies a ROM image into the ROM window; one 0x60000-byte buffer serves both views.
    pub fn load_rom(&mut self, image: &[u8]) -> Result<(), SocError> {
        if image.len() > mem::ROM_LEN as usize {
            return Err(SocError::RomLen(image.len()));
        }
        let at = mem::ROM_ARENA as usize;
        self.arena.bytes_mut()[at..at + image.len()].copy_from_slice(image);
        Ok(())
    }

    /// Where the byte at `addr` lives in the arena and what its page allows, for a slow path.
    ///
    /// The page table is the translation authority; [`mem::REGIONS`] only seeds it, so a mapped
    /// flash-window page, which has no region, is resolved through its entry. A region row
    /// answers first with its unfolded flags, because [`pagetable::fold`] drops permission bits
    /// only to keep a fast path off the page.
    fn resolve(&self, addr: u32) -> Option<Byte> {
        let (off, map, allowed) = match mem::region_of(addr) {
            Some(region) => (
                region.offset_of(addr)?,
                region.flags,
                self.region_flags(addr, region),
            ),
            None => {
                let entry = self.pages.entry(addr);
                // An unmapped page, and a peripheral page, name no arena bytes: the peripheral
                // window is `crate::mmio`'s to answer.
                if entry == 0 || pagetable::flags(entry) & pagetable::PF_MMIO != 0 {
                    return None;
                }
                let flags = pagetable::flags(entry);
                (pagetable::arena_addr(entry, addr), flags, flags)
            }
        };
        (off < Arena::LEN).then_some(Byte { off, map, allowed })
    }

    /// Permissions of a byte a [`mem::Region`] backs: the row's flags narrowed by PMP and PMS, so
    /// protection binds the slow path as well as the fast one.
    ///
    /// A page the fold decided whole carries the answer in its entry. A `PF_SLOW` page does not,
    /// because only part of it keeps the row's permissions, so the byte is decided against the
    /// PMP entries and PMS split kept from that fold.
    fn region_flags(&self, addr: u32, region: &mem::Region) -> u32 {
        let entry = pagetable::flags(self.pages.entry(addr));
        if entry & pagetable::PF_SLOW == 0 {
            return region.flags & entry;
        }
        region.flags & self.protection().byte_flags(addr)
    }

    fn protection(&self) -> Protection {
        Protection::from_state(&self.pmp, self.pms)
    }

    /// Whether protection allows the whole access, asked when an access touches a `PF_SLOW` page.
    ///
    /// Byte-by-byte resolution misses one rule: an access that straddles a PMP entry boundary
    /// faults even when both entries allow it. Other pages are decided whole, so no boundary
    /// falls inside them; both ends of the range are tested.
    fn slow_access_allowed(&self, addr: u32, size: u8, kind: AccessKind) -> bool {
        let last = addr.wrapping_add(u32::from(size.max(1)) - 1);
        let slow = (self.pages.entry(addr) | self.pages.entry(last)) & pagetable::PF_SLOW;
        slow == 0 || self.protection().check(addr, size, kind)
    }

    /// Why an access to a byte nothing backs is refused.
    ///
    /// The protection fold walks region rows only, so an unbacked page carries no protection
    /// answer; without this check an access there would read 0 even under a locked PMP entry. A
    /// NULL read in an IDF task is the common case: the app's locked entries leave address 0 with
    /// no permission, and `probes/probe_panic` must fault there (mcause 5).
    fn unbacked_refusal(&self, addr: u32, size: u8, kind: AccessKind) -> Refused {
        if self.protection().check(addr, size, kind) {
            Refused::Unbacked
        } else {
            Refused::Denied
        }
    }

    /// The `size` bytes at `addr`, or why they cannot be read.
    ///
    /// The fast rule answers in one indexed read; anything it rejects, including a page-straddling
    /// access, is resolved byte by byte through [`Soc::resolve`]. A byte the fold denies refuses
    /// the whole load, and so does a range straddling a protection boundary on a `PF_SLOW` page.
    pub fn load(&self, addr: u32, size: u8) -> Result<u32, Refused> {
        let entry = self.pages.entry(addr);
        if pagetable::fast_load(entry, addr, size) {
            return Ok(self.arena.load(pagetable::arena_addr(entry, addr), size));
        }
        if !self.slow_access_allowed(addr, size, AccessKind::Read) {
            return Err(Refused::Denied);
        }
        let mut val = 0;
        for i in 0..size {
            let at = addr.wrapping_add(u32::from(i));
            let byte = self
                .resolve(at)
                .ok_or_else(|| self.unbacked_refusal(addr, size, AccessKind::Read))?;
            if !pagetable::slow_readable(byte.allowed) {
                // Every region row is readable, so a refused region byte was denied by
                // protection; an unreadable table-only page is the unbacked case.
                return Err(if pagetable::slow_readable(byte.map) {
                    Refused::Denied
                } else {
                    Refused::Unbacked
                });
            }
            val |= u32::from(self.arena.bytes()[byte.off]) << (i * 8);
        }
        Ok(val)
    }

    pub fn load_mem(&self, addr: u32, size: u8) -> Option<u32> {
        self.load(addr, size).ok()
    }

    /// Writes the low `size` bytes of `val` at `addr`.
    ///
    /// A store any byte of the range rejects writes nothing, so the trap is precise. A store into
    /// a page of translated code clears its `PF_CODE` and records the page.
    pub fn store_mem(&mut self, addr: u32, size: u8, val: u32) -> Stored {
        let entry = self.pages.entry(addr);
        if pagetable::fast_store(entry, addr, size) {
            self.arena
                .store(pagetable::arena_addr(entry, addr), size, val);
            return Stored::Wrote { invalidated: false };
        }
        if !self.slow_access_allowed(addr, size, AccessKind::Write) {
            return Stored::Denied;
        }
        for i in 0..size {
            let at = addr.wrapping_add(u32::from(i));
            match self.resolve(at) {
                None => {
                    return match self.unbacked_refusal(addr, size, AccessKind::Write) {
                        Refused::Denied => Stored::Denied,
                        Refused::Unbacked => Stored::Unbacked,
                    };
                }
                Some(byte) if !pagetable::slow_writable(byte.allowed) => {
                    // A read-only row is ignored and logged; a writable row the fold closed owes
                    // the guest mcause 7.
                    return if pagetable::slow_writable(byte.map) {
                        Stored::Denied
                    } else {
                        Stored::ReadOnly
                    };
                }
                Some(_) => {}
            }
        }
        for i in 0..size {
            let at = addr.wrapping_add(u32::from(i));
            let byte = self.resolve(at).expect("checked above");
            self.arena.bytes_mut()[byte.off] = (val >> (i * 8)) as u8;
        }
        let last = addr.wrapping_add(u32::from(size) - 1);
        let mut invalidated = false;
        for at in [addr, last] {
            if self.pages.entry(at) & PF_CODE != 0 {
                invalidated |= self.invalidate(at);
            }
        }
        Stored::Wrote { invalidated }
    }

    /// Executable bytes from `vaddr` to the end of its 4 KB page; the page is marked as holding
    /// translated code so later stores into it take the slow path.
    pub fn fetch(&mut self, vaddr: u32) -> Result<&[u8], Trap> {
        let fault = Trap::instruction_access_fault(vaddr);
        let byte = self.resolve(vaddr).ok_or(fault)?;
        if byte.allowed & PF_X == 0 {
            return Err(fault);
        }
        self.mark_code(vaddr);
        // Every row of `mem::REGIONS` is page aligned, so the page end is also the region end.
        let page_end = (vaddr & !pagetable::FLAG_MASK).wrapping_add(pagetable::PAGE_SIZE);
        // On a page the fold could not decide, execute permission may end inside it: a block
        // must stop where the permission does, or it would run past the PMS split.
        let end = if self.pages.entry(vaddr) & pagetable::PF_SLOW != 0 {
            self.protection().perm_end(vaddr, page_end)
        } else {
            page_end
        };
        let len = (end - vaddr) as usize;
        self.arena
            .bytes()
            .get(byte.off..byte.off + len)
            .ok_or(fault)
    }

    /// Marks the page holding `addr` for the data slow path for a reason the fold does not know
    /// (a data watchpoint or a var matcher). True when the page was not already marked.
    ///
    /// The mark is kept here too because every protection change rebuilds the region entries and
    /// would drop a bit written only into the table. An unmapped page is not marked: a flag alone
    /// would turn entry 0 into a mapped page.
    pub fn mark_slow(&mut self, addr: u32) -> bool {
        let vpn = addr >> 12;
        if self.pages.entry(addr) == 0 || self.slow_marks.contains(&vpn) {
            return false;
        }
        self.slow_marks.push(vpn);
        self.refold_page(addr);
        true
    }

    /// Drops a mark [`Soc::mark_slow`] left on the page holding `addr`; true when there was one.
    /// The entry keeps `PF_SLOW` when a PMP boundary or the PMS split straddles the page.
    pub fn unmark_slow(&mut self, addr: u32) -> bool {
        let vpn = addr >> 12;
        let Some(at) = self.slow_marks.iter().position(|v| *v == vpn) else {
            return false;
        };
        self.slow_marks.remove(at);
        self.refold_page(addr);
        true
    }

    pub fn is_slow_marked(&self, addr: u32) -> bool {
        self.slow_marks.contains(&(addr >> 12))
    }

    /// Rewrites the entry of the page holding `addr` from the map, the last fold's protection and
    /// this SoC's marks. It changes `PF_SLOW` only, so nothing needs invalidating.
    fn refold_page(&mut self, addr: u32) {
        let page = addr & !pagetable::FLAG_MASK;
        let entry = self.pages.entry(page);
        if entry == 0 {
            return;
        }
        let mut marks = pagetable::flags(entry) & (PF_CODE | pagetable::PF_COLD);
        if self.is_slow_marked(page) {
            marks |= pagetable::PF_SLOW;
        }
        let (arena, flags) = match mem::region_of(page) {
            Some(region) => {
                let flags = self.protection().page_flags(page, region.flags);
                (region.arena + (page - region.vbase), flags)
            }
            // `fold` dropped `PF_R` from a table-only page only to keep fast loads off it, so the
            // read permission comes back when the mark goes (as in [`crate::cold::warm`]).
            None => (
                entry & pagetable::ARENA_MASK,
                (pagetable::flags(entry) & !pagetable::PF_SLOW) | pagetable::PF_R,
            ),
        };
        self.pages
            .set_entry(page >> 12, pagetable::entry(arena, flags | marks));
    }

    /// Marks the page holding `vaddr`, and every other view of the same bytes, as holding
    /// translated code. The official image's `_iram_end` page is IRAM code and DRAM data at once
    /// (0x403xxxxx and 0x3FCxxxxx), so marking one view would let a store through the other leave
    /// stale blocks.
    pub fn mark_code(&mut self, vaddr: u32) {
        for view in views_of_page(vaddr) {
            let entry = self.pages.entry(view);
            if entry == 0 {
                continue;
            }
            // A table-only page carries no alias information, so it is marked only when its own
            // entry is executable.
            if mem::region_of(view).is_none() && pagetable::flags(entry) & PF_X == 0 {
                continue;
            }
            self.pages.set_entry(view >> 12, entry | PF_CODE);
        }
    }

    /// Drops the translated-code mark of the page holding `addr` and of every other view of it,
    /// and records each for the engine; true when any held translated code.
    pub fn invalidate(&mut self, addr: u32) -> bool {
        let mut hit = false;
        for view in views_of_page(addr) {
            let vpn = view >> 12;
            let entry = self.pages.entry(view);
            if entry & PF_CODE == 0 {
                continue;
            }
            self.pages.set_entry(vpn, entry & !PF_CODE);
            if !self.invalidated.contains(&vpn) {
                self.invalidated.push(vpn);
            }
            hit = true;
        }
        hit
    }

    pub fn take_invalidated(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.invalidated)
    }

    pub fn is_code_page(&self, addr: u32) -> bool {
        self.pages.entry(addr) & PF_CODE != 0
    }

    pub fn take_wiring(&mut self) -> Vec<Wiring> {
        std::mem::take(&mut self.wiring)
    }

    pub fn sp_monitor(&self) -> SpMonitor {
        self.sp_monitor
    }

    pub fn set_sp_monitor(&mut self, spmon: SpMonitor) {
        self.sp_monitor = spmon;
    }

    pub fn counter_registers(&self) -> (u32, u32) {
        (self.pcer, self.pcmr)
    }

    /// Puts back what [`Soc::counter_registers`] returned. It re-derives nothing: the clock's
    /// counting flag is its own snapshot field.
    pub fn set_counter_registers(&mut self, pcer: u32, pcmr: u32) {
        self.pcer = pcer;
        self.pcmr = pcmr;
    }

    pub fn unbacked_accesses(&self) -> u64 {
        self.unbacked
    }

    pub fn readonly_stores(&self) -> u64 {
        self.readonly_stores
    }

    /// Peripheral-window accesses of a width no register access uses; non-zero means a caller
    /// defect.
    pub fn bad_width_accesses(&self) -> u64 {
        self.bad_width
    }

    pub fn unknown_csr_accesses(&self) -> u64 {
        self.unknown_csrs
    }

    /// The custom CSRs the SoC answers: the performance counter and its user aliases, against
    /// `&mut Soc` and `&mut Clock` alone so it is tested without a peripheral context.
    ///
    /// - **0x7E0, 0x7E1 and the aliases 0x800, 0x801** arrive as `CsrOp::Read` or as
    ///   `CsrOp::Write(new)` with the value the [`Csr`] store is about to hold; `mpcer.CYCLE` and
    ///   `mpcmr.COUNT_EN` are kept here and every access re-derives [`Clock::set_counting`] (TRM
    ///   Registers 1.12, 1.13).
    /// - **0x7E2 and the alias 0x802** have no store: the value comes from the clock, a write
    ///   loads it through [`Clock::set_cycle_count`] as `esp_cpu_set_cycle_count` does, and the
    ///   value before the op is returned.
    ///
    /// `mpcmr.COUNT_SAT` is not applied and the counter wraps at 32 bits, although TRM Register
    /// 1.13 gives the bit as saturate-instead-of-wrap and it *resets set*: a saturating counter
    /// would freeze 27 s into a run at 160 MHz and hang every `ets_delay_us` after it. What silicon
    /// saturates is UNVERIFIED. Every other custom number, dedicated GPIO CSRs 0x803 to 0x805
    /// included, raises illegal instruction.
    pub fn counter_csr(
        &mut self,
        clock: &mut Clock,
        csr: u16,
        op: CsrOp,
        insns: u64,
    ) -> Result<(u32, CsrEffect), Trap> {
        match csr {
            CSR_MPCCR | CSR_UPCCR => {
                let before = clock.cycle_count(insns) as u32;
                if let Some(new) = op.new_value(before) {
                    clock.set_cycle_count(insns, u64::from(new));
                }
                Ok((before, CsrEffect::None))
            }
            CSR_MPCER | CSR_UPCER | CSR_MPCMR | CSR_UPCMR => {
                let (slot, mask) = match csr {
                    CSR_MPCER | CSR_UPCER => (&mut self.pcer, MPCER_WRITE_MASK),
                    _ => (&mut self.pcmr, MPCMR_WRITE_MASK),
                };
                let before = *slot;
                if let Some(new) = op.new_value(before) {
                    *slot = new & mask;
                }
                let enabled = self.pcer & MPCER_CYCLE != 0 && self.pcmr & MPCMR_COUNT_EN != 0;
                clock.set_counting(insns, enabled);
                Ok((before, CsrEffect::None))
            }
            _ => {
                self.unknown_csrs += 1;
                Err(Trap::illegal_instruction(0))
            }
        }
    }

    /// The ledger deduplicates by subject, so a polled hole costs one entry.
    fn note_unbacked(
        &mut self,
        ledger: &mut FidelityLedger,
        now: VTime,
        addr: u32,
        size: u8,
        access: TouchAccess,
    ) {
        self.unbacked += 1;
        note_touch(ledger, now, UNBACKED, addr & !3, size, access);
    }

    fn note_readonly_store(
        &mut self,
        ledger: &mut FidelityLedger,
        now: VTime,
        addr: u32,
        size: u8,
    ) {
        self.readonly_stores += 1;
        note_touch(ledger, now, UNBACKED, addr & !3, size, TouchAccess::Write);
    }

    /// Records a peripheral-window access whose width no register access uses: answered with 0
    /// and logged rather than dropped silently.
    fn note_bad_width(
        &mut self,
        ledger: &mut FidelityLedger,
        now: VTime,
        addr: u32,
        size: u8,
        access: TouchAccess,
    ) {
        self.bad_width += 1;
        mmio::record_unmapped(ledger, now, addr, size, access);
    }

    pub fn protection_changes(&self) -> u64 {
        self.protection_changes
    }

    /// Loads and stores the PMP and PMS fold denied on a backed page (mcause 5 or 7): modelled
    /// behavior, so counted here and not written into the ledger.
    pub fn protection_faults(&self) -> u64 {
        self.protection_faults
    }

    fn note_protection_fault(&mut self) {
        self.protection_faults += 1;
    }
}

impl Default for Soc {
    fn default() -> Self {
        Soc::new(FlashStore::default())
    }
}

/// SoC side of the context of one bus access. Every slow path sets `now` from
/// `HartView::insns` before calling a peripheral; `clock` turns the instruction count into
/// that `now`, and `periph` is the machine-built peripheral context.
pub struct SocCx<'a> {
    pub now: VTime,
    /// The clock that derives `now` from an instruction count. Exclusive, because CSR 0x7E0 to
    /// 0x7E2 and their aliases drive [`Clock::set_counting`] and [`Clock::set_cycle_count`].
    pub clock: &'a mut Clock,
    pub periph: Cx<'a>,
}

impl SocCx<'_> {
    /// Sets `now`, for this access and every peripheral it reaches, from the clock position
    /// (`HartView::pos`, the instruction count under `fast`).
    #[inline]
    pub fn set_now(&mut self, hart: &HartView) {
        self.now = self.clock.now(hart.pos());
        self.periph.now = self.now;
    }
}

pub struct SocBus<'a> {
    pub soc: &'a mut Soc,
    /// Board traits only; the SoC never names a chip.
    pub board: &'a mut dyn BoardPorts,
    pub cx: SocCx<'a>,
}

impl Bus for SocBus<'_> {
    fn pages(&self) -> &PageTable {
        &self.soc.pages
    }

    fn arena(&mut self) -> *mut u8 {
        self.soc.arena.as_mut_ptr()
    }

    /// Load slow path: PF_SLOW, PF_MMIO and PF_COLD pages. `exec_op` takes every load through
    /// here, so plain RAM reaching it is correct too.
    fn load_slow(&mut self, addr: u32, size: u8, hart: &HartView) -> Access<u32> {
        self.cx.set_now(hart);
        if mem::is_mmio(addr) {
            let Some(width) = mmio::reg_size(size) else {
                let (ledger, now) = (&mut *self.cx.periph.ledger, self.cx.now);
                self.soc
                    .note_bad_width(ledger, now, addr, size, TouchAccess::Read);
                return Access::Ok(0);
            };
            let read = mmio::read(&mut self.soc.devices, addr, width, &mut self.cx.periph);
            return if read.stop {
                Access::OkStop(read.val)
            } else {
                Access::Ok(read.val)
            };
        }
        // While the profile charges the cache every mapped DROM page is `PF_COLD`, so a flash
        // load lands here and stalls until its word arrives; the run ends so the budget is
        // recomputed.
        let stall = if mem::is_flash_window(addr) {
            let cycle_ps = pemu_core::clock::ps_per_cycle(self.cx.clock.cpu_hz());
            self.soc
                .cache
                .load(&mut self.soc.pages, addr, self.cx.now.0, cycle_ps)
        } else {
            0
        };
        if stall > 0 {
            let counts = self.cx.periph.profile.cache_stall_counts_cycles;
            self.cx.clock.stall(hart.insns, stall, counts);
        }
        let why = match self.soc.load(addr, size) {
            Ok(val) if stall > 0 => return Access::OkStop(val),
            Ok(val) => return Access::Ok(val),
            Err(why) => why,
        };
        // A locked PMP entry that denies the access: mcause 5 with `mtval` = the address.
        if let Some(trap) = why.trap(addr) {
            self.soc.note_protection_fault();
            return Access::Fault(trap);
        }
        // Otherwise an address nothing backs reads 0 and is logged.
        let (ledger, now) = (&mut *self.cx.periph.ledger, self.cx.now);
        self.soc
            .note_unbacked(ledger, now, addr, size, TouchAccess::Read);
        Access::Ok(0)
    }

    /// Store slow path; returns `OkStop` when the store hit translated code (the block may be the
    /// one just invalidated) or a peripheral write asked to stop or produced wiring.
    ///
    /// Inlined, with [`mmio::write`] inlined into it, so a register store is one call from the
    /// engine's slow path to the device: the bootloader hashes the app through the SHA text
    /// registers with one such store every five instructions, and in V8 on x86-64 the extra calls
    /// were most of that path's time.
    #[inline(always)]
    fn store_slow(&mut self, addr: u32, size: u8, val: u32, hart: &HartView) -> Access<()> {
        self.cx.set_now(hart);
        if mem::is_mmio(addr) {
            let Some(width) = mmio::reg_size(size) else {
                let (ledger, now) = (&mut *self.cx.periph.ledger, self.cx.now);
                self.soc
                    .note_bad_width(ledger, now, addr, size, TouchAccess::Write);
                return Access::Ok(());
            };
            let write = mmio::write(&mut self.soc.devices, addr, width, val, &mut self.cx.periph);
            let mut wired = !matches!(write.wiring, Wiring::None);
            if wired {
                self.soc.wiring.push(write.wiring);
            }
            // A SYSTEM peripheral reset enable resets the block it names; the stack guard's new
            // monitor is published like any ASSIST_DEBUG write's.
            if (SYSTEM_BASE..SYSTEM_BASE + 0x1000).contains(&addr)
                && let Some(monitor) = wiring::reset::system_reset_enable(
                    &mut self.soc.devices.assist_debug,
                    addr - SYSTEM_BASE,
                    width,
                    val,
                    &mut crate::regs::Ports::of(&mut self.cx.periph),
                )
            {
                self.soc.wiring.push(Wiring::SpMonitor(monitor));
                wired = true;
            }
            // A SPI1 transaction latches its array half here, inside the write that started it,
            // so a read has filled `W` before the guest polls `CMD` again and a program or erase
            // has reached the store. The write reported the first changed page; the rest are
            // reported page by page.
            if self.soc.devices.spi1.is_pending()
                && let Some(changed) = self.soc.devices.spi1.service(&mut self.soc.flash)
            {
                for phys_page in changed.first_page + 1..changed.first_page + changed.pages {
                    self.soc.wiring.push(Wiring::FlashWritten { phys_page });
                }
            }
            return if write.stop || wired {
                Access::OkStop(())
            } else {
                Access::Ok(())
            };
        }
        let stored = self.soc.store_mem(addr, size, val);
        // A range the map allows and the fold closed: a store fault, mcause 7 with `mtval` = the
        // address. Modelled behavior, so counted and not logged.
        if let Some(trap) = stored.trap(addr) {
            self.soc.note_protection_fault();
            return Access::Fault(trap);
        }
        match stored {
            Stored::Wrote { invalidated: true } => Access::OkStop(()),
            Stored::Wrote { invalidated: false } => Access::Ok(()),
            // A write into a range the map makes read-only, ROM above all, is ignored and logged.
            Stored::ReadOnly => {
                let (ledger, now) = (&mut *self.cx.periph.ledger, self.cx.now);
                self.soc.note_readonly_store(ledger, now, addr, size);
                Access::Ok(())
            }
            Stored::Denied => Access::Fault(Trap::store_access_fault(addr)),
            Stored::Unbacked => {
                let (ledger, now) = (&mut *self.cx.periph.ledger, self.cx.now);
                self.soc
                    .note_unbacked(ledger, now, addr, size, TouchAccess::Write);
                Access::Ok(())
            }
        }
    }

    fn sp_monitor(&self) -> SpMonitor {
        self.soc.sp_monitor()
    }

    fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap> {
        self.soc.fetch(vaddr).map(|bytes| CodePage { bytes })
    }

    fn csr_custom(&mut self, csr: u16, op: CsrOp, insns: u64) -> Result<(u32, CsrEffect), Trap> {
        self.soc.counter_csr(self.cx.clock, csr, op, insns)
    }

    fn wfi_wake(&mut self) -> bool {
        self.cx.periph.irq.wfi_wake()
    }

    /// Recomputes page permissions: PMP and the SENSITIVE split are folded into `PF_R`, `PF_W`,
    /// `PF_X` and `PF_SLOW` per page by [`crate::wiring::protection`], always starting from the
    /// [`mem::REGIONS`] flags, so the fold is idempotent. The entries are kept on the SoC because
    /// a `PF_SLOW` page is decided from them later and `&Csr` lives only for this call.
    fn pmp_changed(&mut self, csr: &Csr) {
        self.soc.protection_changes += 1;
        let pms = self.soc.pms;
        Protection::from_csr(csr, pms).apply(self.soc);
    }

    /// The fetch side of the line cache models ([`cold`]): the engine reports the fetch at `pc`
    /// whenever the stream may have entered another line, and a miss stalls before the
    /// instruction executes.
    fn fetch_enter(&mut self, pos: u64, pc: u32) -> bool {
        let now = self.cx.clock.now(pos).0;
        let cycle_ps = pemu_core::clock::ps_per_cycle(self.cx.clock.cpu_hz());
        let stall =
            self.soc
                .cache
                .fetch(&self.soc.pages, self.soc.arena.bytes(), pc, now, cycle_ps);
        if stall == 0 {
            return false;
        }
        let counts = self.cx.periph.profile.cache_stall_counts_cycles;
        self.cx.clock.stall(pos, stall, counts);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{
        FLASH_ARENA, FLASH_DROM_BASE, FLASH_IROM_BASE, MMIO_BASE, ROM_BASE, ROM_DATA_BASE,
        ROM_DATA_IMAGE_OFF, ROM_LEN, RTC_FAST_BASE, SRAM0_BASE, SRAM1_DRAM_BASE, SRAM1_IRAM_BASE,
    };
    use crate::pagetable::{PF_COLD, PF_R, PF_W};
    use crate::periph::{BLOCK_COUNT, id};
    use pemu_rv32::csr::CsrCx;

    fn rom_image() -> Vec<u8> {
        (0..ROM_LEN).map(|off| (off >> 4) as u8).collect()
    }

    fn soc_with_rom() -> Soc {
        let mut soc = Soc::default();
        soc.load_rom(&rom_image()).expect("0x60000 bytes");
        soc
    }

    /// A [`Bus`] over a [`Soc`] and a [`Clock`] only, so a CSR access runs through the real
    /// `pemu_rv32::Csr`, which decides which op reaches [`Bus::csr_custom`].
    struct CsrBus<'a> {
        soc: &'a mut Soc,
        clock: &'a mut Clock,
    }

    impl Bus for CsrBus<'_> {
        fn pages(&self) -> &PageTable {
            &self.soc.pages
        }
        fn arena(&mut self) -> *mut u8 {
            self.soc.arena.as_mut_ptr()
        }
        fn load_slow(&mut self, addr: u32, size: u8, _hart: &HartView) -> Access<u32> {
            Access::Ok(self.soc.load_mem(addr, size).unwrap_or(0))
        }
        fn store_slow(&mut self, addr: u32, size: u8, val: u32, _hart: &HartView) -> Access<()> {
            self.soc.store_mem(addr, size, val);
            Access::Ok(())
        }
        fn sp_monitor(&self) -> SpMonitor {
            self.soc.sp_monitor()
        }
        fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap> {
            self.soc.fetch(vaddr).map(|bytes| CodePage { bytes })
        }
        fn csr_custom(
            &mut self,
            csr: u16,
            op: CsrOp,
            insns: u64,
        ) -> Result<(u32, CsrEffect), Trap> {
            self.soc.counter_csr(self.clock, csr, op, insns)
        }
        fn wfi_wake(&mut self) -> bool {
            false
        }
        fn pmp_changed(&mut self, csr: &Csr) {
            self.soc.protection_changes += 1;
            let pms = self.soc.pms;
            Protection::from_csr(csr, pms).apply(self.soc);
        }
    }

    /// A guest `csrw` of a locked PMP entry, as `esp_cpu_configure_region_protection` writes it,
    /// from the Zicsr instruction to the page entry the fast paths read.
    #[test]
    fn a_pmp_csr_write_refolds_the_page_permissions() {
        use crate::mem::{SRAM1_DRAM_BASE, SRAM1_IRAM_BASE};
        use crate::pagetable::{PF_X, fast_store};
        use pemu_rv32::pmp::{CSR_PMPADDR0, CSR_PMPCFG0, PMP_L, PMP_R, PMP_TOR};

        let mut soc = Soc::default();
        let mut clock = Clock::default();
        let mut csr = Csr::new();
        let mut bus = CsrBus {
            soc: &mut soc,
            clock: &mut clock,
        };
        assert!(fast_store(
            bus.soc.pages.entry(SRAM1_DRAM_BASE),
            SRAM1_DRAM_BASE,
            4
        ));

        // Entry 0 as TOR from 0 to 0x40000000, locked, read only: DRAM and the ROM data view lose
        // write and execute, and IRAM above the top keeps everything, because an address matching
        // no entry is allowed in machine mode.
        let addr = 0x4000_0000u32 >> 2;
        csr.access(&mut bus, at(10), CSR_PMPADDR0, CsrOp::Write(addr))
            .expect("pmpaddr0 is writable");
        csr.access(
            &mut bus,
            at(20),
            CSR_PMPCFG0,
            CsrOp::Write(u32::from(PMP_L | PMP_TOR | PMP_R)),
        )
        .expect("pmpcfg0 is writable");

        assert_eq!(
            bus.soc.protection_changes(),
            2,
            "both writes were effective"
        );
        assert_eq!(
            pagetable::flags(bus.soc.pages.entry(SRAM1_DRAM_BASE)),
            PF_R,
            "the fold took PF_W off the DRAM view"
        );
        assert!(!fast_store(
            bus.soc.pages.entry(SRAM1_DRAM_BASE),
            SRAM1_DRAM_BASE,
            4
        ));
        assert_eq!(
            bus.soc.store_mem(SRAM1_DRAM_BASE, 4, 1),
            Stored::Denied,
            "and the slow path refuses it too"
        );
        assert_eq!(
            pagetable::flags(bus.soc.pages.entry(SRAM1_IRAM_BASE)),
            PF_R | PF_W | PF_X,
            "the IRAM view is above the entry and keeps everything"
        );
        assert_eq!(bus.soc.pmp, PmpState::from_csr(&csr));
    }

    #[test]
    fn a_denied_access_faults_instead_of_reading_zero_under_the_unbacked_id() {
        // A locked PMP entry that denies an access raises mcause 1, 5 or 7 with mtval = the
        // address. A fabricated 0 under `UNBACKED` would report a memprot violation as a fidelity
        // gap in a hole.
        use pemu_rv32::pmp::{PMP_L, PMP_R, PMP_TOR};

        let mut soc = Soc::default();
        let mut csr = Csr::new();
        // One locked TOR entry below 0x40000000, read only: SRAM1's DRAM view is writable on the
        // map, and only PMP refuses the store.
        csr.pmpaddr[0] = 0x4000_0000 >> 2;
        csr.pmpcfg[0] = PMP_L | PMP_TOR | PMP_R;
        Protection::from_csr(&csr, Pms::OPEN).apply(&mut soc);

        let addr = SRAM1_DRAM_BASE + 0x80;
        assert_eq!(soc.load(addr, 4), Ok(0), "read is still granted");
        assert_eq!(soc.store_mem(addr, 4, 0xBEEF), Stored::Denied);
        let trap = Stored::Denied.trap(addr).expect("a store fault");
        assert_eq!(
            (trap.cause, trap.tval),
            (7, addr),
            "mcause 7, mtval = address"
        );

        csr.pmpcfg[0] = PMP_L | PMP_TOR;
        Protection::from_csr(&csr, Pms::OPEN).apply(&mut soc);
        assert_eq!(soc.load(addr, 4), Err(Refused::Denied));
        let trap = Refused::Denied.trap(addr).expect("a load fault");
        assert_eq!(
            (trap.cause, trap.tval),
            (5, addr),
            "mcause 5, mtval = address"
        );

        // The "ignored and logged" outcomes stay that way: a store into ROM, and an unbacked
        // access while PMP allows it.
        assert_eq!(soc.store_mem(ROM_BASE, 4, 1), Stored::ReadOnly);
        assert_eq!(Stored::ReadOnly.trap(ROM_BASE), None);
        // The locked entry denies everything below 0x40000000, so an unbacked address there faults
        // too, as a NULL read in an IDF task does.
        assert_eq!(soc.load(0x3FCE_0000, 4), Err(Refused::Denied));
        assert_eq!(soc.store_mem(0x3FCE_0000, 4, 1), Stored::Denied);
        assert_eq!(soc.load(0, 4), Err(Refused::Denied));
        assert_eq!(soc.load(0x4100_0000, 4), Err(Refused::Unbacked));
        assert_eq!(soc.store_mem(0x4100_0000, 4, 1), Stored::Unbacked);
        csr.pmpcfg[0] = PMP_L | PMP_TOR | PMP_R | pemu_rv32::pmp::PMP_W;
        Protection::from_csr(&csr, Pms::OPEN).apply(&mut soc);
        assert_eq!(soc.load(0x3FCE_0000, 4), Err(Refused::Unbacked));
        assert_eq!(soc.store_mem(0x3FCE_0000, 4, 1), Stored::Unbacked);
        let fresh = Soc::default();
        assert_eq!(fresh.load(0, 4), Err(Refused::Unbacked));
        assert_eq!(Refused::Unbacked.trap(0x3FCE_0000), None);
        assert_eq!(Stored::Unbacked.trap(0x3FCE_0000), None);
        assert_eq!(
            Stored::Wrote { invalidated: true }.trap(addr),
            None,
            "a store that landed owes nothing"
        );
    }

    /// The locked TOR table `esp_cpu_configure_region_protection` writes: the top address and
    /// permission of entries 0 to 14, and entry 15 as NA4 with no permission.
    fn idf_pmp_csr() -> Csr {
        use pemu_rv32::pmp::{PMP_L, PMP_NA4, PMP_R, PMP_TOR, PMP_W, PMP_X};
        const RWX: u8 = PMP_R | PMP_W | PMP_X;
        let table: [(u32, u8); 15] = [
            (0x2000_0000, 0),
            (0x2800_0000, RWX),
            (0x3C00_0000, 0),
            (0x3FC8_0000, PMP_R),
            (0x3FCE_0000, PMP_R | PMP_W),
            (0x3FF2_0000, PMP_R),
            (0x4006_0000, PMP_R | PMP_X),
            (0x4037_C000, 0),
            (0x403E_0000, RWX),
            (0x4280_0000, PMP_R | PMP_X),
            (0x5000_0000, 0),
            (0x5000_2000, RWX),
            (0x6000_0000, 0),
            (0x6010_0000, PMP_R | PMP_W),
            (0xFFFF_FFFC, 0),
        ];
        let mut csr = Csr::new();
        for (i, (top, perm)) in table.into_iter().enumerate() {
            csr.pmpaddr[i] = top >> 2;
            csr.pmpcfg[i] = PMP_L | PMP_TOR | perm;
        }
        csr.pmpaddr[15] = 0xFFFF_FFFC >> 2;
        csr.pmpcfg[15] = PMP_L | PMP_NA4;
        csr
    }

    /// Under the real IDF table an address nothing backs reads 0 where its entry grants the
    /// access and faults where it does not.
    #[test]
    fn the_idf_pmp_table_splits_unbacked_holes_by_entry_permission() {
        let mut soc = Soc::default();
        Protection::from_csr(&idf_pmp_csr(), Pms::OPEN).apply(&mut soc);

        assert_eq!(soc.load(0, 4), Err(Refused::Denied));
        assert_eq!(soc.store_mem(0, 4, 1), Stored::Denied);
        assert_eq!(soc.load(0x3FCE_0000, 4), Err(Refused::Unbacked));
        assert_eq!(Refused::Unbacked.trap(0x3FCE_0000), None);
        assert_eq!(soc.store_mem(0x3FCE_0000, 4, 1), Stored::Denied);
        assert_eq!(
            Stored::Denied.trap(0x3FCE_0000).map(|t| (t.cause, t.tval)),
            Some((7, 0x3FCE_0000))
        );
        assert_eq!(soc.load(0x4100_0000, 4), Err(Refused::Unbacked));
        assert_eq!(soc.store_mem(0x4100_0000, 4, 1), Stored::Denied);
        assert_eq!(soc.load(0x4280_0000, 4), Err(Refused::Denied));
        assert_eq!(
            soc.store_mem(SRAM1_DRAM_BASE + 0x80, 4, 7),
            Stored::Wrote { invalidated: false }
        );
        assert_eq!(soc.load(SRAM1_DRAM_BASE + 0x80, 4), Ok(7));
    }

    fn at(insns: u64) -> CsrCx {
        CsrCx {
            insns,
            insn: 0x0000_10F3,
            strict: false,
        }
    }

    #[test]
    fn the_counter_control_csrs_the_rom_writes_reach_the_clock_instead_of_trapping() {
        // ROM `_init` writes CSR 0x800 and 0x801 early. `pemu_rv32::csr` routes them into
        // `Bus::csr_custom` before updating its own store, so a bus that refuses them stops the
        // ROM at its first counter-CSR write.
        let mut soc = Soc::default();
        let mut clock = Clock::default();
        let mut csr = Csr::new();
        let mut bus = CsrBus {
            soc: &mut soc,
            clock: &mut clock,
        };

        // Reset: mpcmr has COUNT_EN, mpcer has no CYCLE, so the counter is disabled and reads 0.
        assert_eq!(csr.read(&mut bus, at(1_000), CSR_UPCCR), Ok(0));
        assert!(!bus.clock.counting());

        assert_eq!(csr.write(&mut bus, at(10), CSR_UPCER, MPCER_CYCLE), Ok(()));
        assert_eq!(
            csr.write(&mut bus, at(11), CSR_UPCMR, MPCMR_WRITE_MASK),
            Ok(())
        );
        assert_eq!(csr.mpcer, MPCER_CYCLE);
        assert!(bus.clock.counting());

        assert_eq!(csr.read(&mut bus, at(11), CSR_MPCER), Ok(MPCER_CYCLE));
        assert_eq!(csr.read(&mut bus, at(11), CSR_MPCMR), Ok(MPCMR_WRITE_MASK));

        // `ets_delay_us` reads 0x802 and spins until the count passes its target: at CPI 1 and
        // 160 MHz it moves one per instruction from the enable at instruction 10.
        assert_eq!(csr.read(&mut bus, at(1_010), CSR_UPCCR), Ok(1_000));
        assert_eq!(csr.read(&mut bus, at(2_010), CSR_MPCCR), Ok(2_000));

        // `esp_cpu_set_cycle_count` writes 0x7E2: the write loads the counter through
        // `Clock::set_cycle_count` and reads back the value before it.
        assert_eq!(
            csr.access(&mut bus, at(2_010), CSR_MPCCR, CsrOp::Write(5)),
            Ok(2_000)
        );
        assert_eq!(csr.read(&mut bus, at(2_020), CSR_UPCCR), Ok(15));

        assert_eq!(csr.write(&mut bus, at(2_020), CSR_MPCER, 0), Ok(()));
        assert!(!bus.clock.counting());
        assert_eq!(csr.read(&mut bus, at(9_999), CSR_UPCCR), Ok(15));

        assert_eq!(
            csr.write(&mut bus, at(2_020), CSR_MPCER, MPCER_CYCLE),
            Ok(())
        );
        assert!(bus.clock.counting());
        assert_eq!(
            csr.access(&mut bus, at(2_020), CSR_UPCMR, CsrOp::Clear(MPCMR_COUNT_EN)),
            Ok(MPCMR_WRITE_MASK)
        );
        assert!(!bus.clock.counting());

        assert_eq!(
            csr.read(&mut bus, at(2_020), 0x7C0),
            Err(Trap::illegal_instruction(0x0000_10F3))
        );
        assert_eq!(bus.soc.unknown_csr_accesses(), 1);
    }

    #[test]
    fn a_page_the_table_maps_that_no_region_covers_is_resolved_by_the_table() {
        // `crate::wiring::mmu` maps DROM pages into the flash window area and marks them PF_COLD
        // under `device`; no `Region` covers either window, so a slow path that asked only
        // `mem::region_of` would call the page unbacked.
        let mut soc = Soc::default();
        let at = FLASH_ARENA as usize;
        soc.arena.bytes_mut()[at..at + 4].copy_from_slice(&[1, 2, 3, 4]);

        let cold = pagetable::entry(FLASH_ARENA, PF_R | PF_COLD);
        assert_eq!(cold & PF_R, 0, "fold drops R beside PF_COLD");
        soc.pages.set_entry(FLASH_DROM_BASE >> 12, cold);
        assert_eq!(soc.load_mem(FLASH_DROM_BASE, 4), Some(0x0403_0201));
        assert_eq!(soc.load_mem(FLASH_DROM_BASE + 2, 2), Some(0x0403));
        assert_eq!(soc.store_mem(FLASH_DROM_BASE, 4, 0), Stored::ReadOnly);
        assert_eq!(soc.load_mem(FLASH_DROM_BASE, 4), Some(0x0403_0201));
        // The page above is still unmapped, and a peripheral page is not memory however the table
        // marks it.
        assert_eq!(soc.load_mem(FLASH_DROM_BASE + 0x1000, 4), None);
        assert_eq!(
            soc.store_mem(FLASH_DROM_BASE + 0x1000, 4, 1),
            Stored::Unbacked
        );
        assert_eq!(soc.load_mem(MMIO_BASE, 4), None);
        assert_eq!(soc.store_mem(MMIO_BASE, 4, 1), Stored::Unbacked);
        assert!(soc.fetch(MMIO_BASE).is_err());
        assert_eq!(soc.load_mem(FLASH_DROM_BASE + 0xFFE, 4), None);

        // A writable mapped page takes the store, and the IROM view can be translated: the mark
        // lands on its own entry, since `views_of` knows no alias for it.
        let rw = pagetable::entry(FLASH_ARENA + 0x1000, PF_R | PF_W | PF_X);
        soc.pages.set_entry(FLASH_IROM_BASE >> 12, rw);
        assert_eq!(
            soc.store_mem(FLASH_IROM_BASE + 8, 4, 0xAABB_CCDD),
            Stored::Wrote { invalidated: false }
        );
        assert_eq!(soc.load_mem(FLASH_IROM_BASE + 8, 4), Some(0xAABB_CCDD));
        assert_eq!(
            soc.fetch(FLASH_IROM_BASE).expect("executable").len(),
            0x1000
        );
        assert!(soc.is_code_page(FLASH_IROM_BASE));
        assert_eq!(
            soc.store_mem(FLASH_IROM_BASE + 8, 4, 0),
            Stored::Wrote { invalidated: true }
        );
        assert_eq!(soc.take_invalidated(), vec![FLASH_IROM_BASE >> 12]);
        assert!(!soc.is_code_page(FLASH_IROM_BASE));
        soc.mark_code(FLASH_DROM_BASE);
        assert!(!soc.is_code_page(FLASH_DROM_BASE));
        assert!(soc.arena.guards_intact());
    }

    #[test]
    fn an_unbacked_address_is_a_different_ledger_subject_from_a_hole_in_the_window() {
        // The ledger deduplicates on (periph, off), and 0x600D1000 (window-relative 0xD1000) and
        // 0x000D1000 (absolute 0xD1000) collide. One id for both would swallow the second touch.
        let mut soc = Soc::default();
        let mut ledger = FidelityLedger::default();
        mmio::record_unmapped(&mut ledger, VTime(7), 0x600D_1000, 4, TouchAccess::Read);
        soc.note_unbacked(&mut ledger, VTime(9), 0x000D_1000, 4, TouchAccess::Read);
        assert_eq!(ledger.first_touches().len(), 2);
        assert!(ledger.is_touched(id::UNMAPPED, 0xD_1000));
        assert!(ledger.is_touched(UNBACKED, 0xD_1000));
        assert_ne!(UNBACKED, id::UNMAPPED);
        assert!(
            BLOCK_COUNT < UNBACKED.0 as usize,
            "no block owns the sentinel"
        );

        soc.note_readonly_store(&mut ledger, VTime(11), ROM_BASE + 0x40, 4);
        assert_eq!(soc.unbacked_accesses(), 1);
        assert_eq!(soc.readonly_stores(), 1);
        assert_eq!(ledger.first_touches().len(), 3);

        soc.note_unbacked(&mut ledger, VTime(13), 0x000D_1002, 1, TouchAccess::Write);
        assert_eq!(ledger.first_touches().len(), 3);
        assert_eq!(soc.unbacked_accesses(), 2);
    }

    #[test]
    fn a_peripheral_access_of_a_width_no_register_uses_is_recorded_not_dropped() {
        // A width `mmio::reg_size` rejects reaches no model, so it is logged here; the engine
        // issues only 1, 2 and 4, so the counter says a caller is wrong.
        let mut soc = Soc::default();
        let mut ledger = FidelityLedger::default();
        assert!(mmio::reg_size(8).is_none());
        soc.note_bad_width(
            &mut ledger,
            VTime(1),
            MMIO_BASE + 0x40,
            8,
            TouchAccess::Read,
        );
        assert_eq!(soc.bad_width_accesses(), 1);
        assert!(ledger.is_touched(id::UNMAPPED, 0x40));
        assert_eq!(ledger.first_touches()[0].size, 8);
    }

    #[test]
    fn the_large_buffers_are_heap_built_by_construction() {
        // A Windows .exe gets a 1 MiB stack from its PE header, so neither the arena nor the page
        // table may ever live on the stack: both are one pointer wide by value.
        // `clippy::large_stack_arrays` and `large_stack_frames` catch an array literal that
        // materializes before it is boxed.
        const WINDOWS_STACK: usize = 1 << 20;
        const _: () = assert!(Arena::LEN > WINDOWS_STACK);
        const _: () = assert!(PageTable::PAGES * size_of::<u32>() > WINDOWS_STACK);
        assert_eq!(size_of::<Arena>(), size_of::<usize>() * 2);
        assert_eq!(size_of::<PageTable>(), size_of::<usize>());
        // A SoC is still a small value, so `Soc::new` returns one without copying 9 MB through a
        // stack frame. The bound is loose on purpose: `Devices` grows with every modelled block,
        // and what matters is that nothing of the arena's order of magnitude goes inline.
        const SOC_MAX: usize = WINDOWS_STACK / 16;
        assert!(size_of::<Soc>() < SOC_MAX, "Soc is {}", size_of::<Soc>());
        let soc = Soc::default();
        assert_eq!(soc.arena.bytes().len(), Arena::LEN);
        assert!(soc.arena.guards_intact());
        assert_eq!(soc.pages.entries().len(), PageTable::PAGES);
    }

    #[test]
    fn a_rom_image_larger_than_the_window_is_refused() {
        let mut soc = Soc::default();
        assert_eq!(
            soc.load_rom(&vec![0; ROM_LEN as usize + 1]),
            Err(SocError::RomLen(ROM_LEN as usize + 1))
        );
        assert!(soc.load_rom(&[1, 2, 3]).is_ok());
        assert_eq!(soc.load_mem(ROM_BASE, 4), Some(0x0003_0201));
        assert!(soc.arena.guards_intact());
    }

    #[test]
    fn the_drom_alias_reads_the_same_bytes_as_its_irom_window() {
        // Data address 0x3FF00000 + x is the same byte as instruction address 0x40040000 + x, for
        // x < 0x20000; the ROM ELF's .rodata VMA 0x3FF19F70 has LMA 0x40059F70.
        let soc = soc_with_rom();
        for x in [0u32, 4, 0x19F70, 0x1EE3C, 0x1_FFFC] {
            let data = soc.load_mem(ROM_DATA_BASE + x, 4);
            let insn = soc.load_mem(ROM_BASE + ROM_DATA_IMAGE_OFF + x, 4);
            assert_eq!(data, insn, "rom alias at {x:#X}");
            assert_eq!(
                data,
                Some(u32::from_le_bytes(
                    rom_image()[(ROM_DATA_IMAGE_OFF + x) as usize..][..4]
                        .try_into()
                        .unwrap()
                ))
            );
        }
        assert_eq!(soc.load_mem(ROM_DATA_BASE - 4, 4), None);
        assert_eq!(soc.load_mem(ROM_DATA_BASE + 0x2_0000, 4), None);
        let mut soc = soc;
        let before = soc.load_mem(ROM_DATA_BASE + 0x19F70, 4);
        assert_eq!(
            soc.store_mem(ROM_BASE + ROM_DATA_IMAGE_OFF + 0x19F70, 4, 0),
            Stored::ReadOnly
        );
        assert_eq!(
            soc.store_mem(ROM_DATA_BASE + 0x19F70, 4, 0),
            Stored::ReadOnly
        );
        assert_eq!(soc.load_mem(ROM_DATA_BASE + 0x19F70, 4), before);
        assert_ne!(before, Some(0));
    }

    #[test]
    fn the_sram1_views_share_their_bytes() {
        // 0x3FC80000 + x is the same byte as 0x40380000 + x for x < 0x60000.
        let mut soc = Soc::default();
        soc.store_mem(SRAM1_DRAM_BASE + 0x1D600, 4, 0xDEAD_BEEF);
        assert_eq!(
            soc.load_mem(SRAM1_IRAM_BASE + 0x1D600, 4),
            Some(0xDEAD_BEEF)
        );
        soc.store_mem(SRAM1_IRAM_BASE + 0x1D600, 2, 0x1234);
        assert_eq!(
            soc.load_mem(SRAM1_DRAM_BASE + 0x1D600, 4),
            Some(0xDEAD_1234)
        );
        assert!(soc.fetch(SRAM1_DRAM_BASE).is_err());
        assert!(soc.fetch(SRAM1_IRAM_BASE).is_ok());
        assert!(soc.fetch(SRAM0_BASE).is_ok());
        assert!(soc.fetch(RTC_FAST_BASE).is_ok());
    }

    #[test]
    fn a_store_into_a_code_page_invalidates_every_view_of_it() {
        let mut soc = Soc::default();
        let code = SRAM1_IRAM_BASE + 0x1D000;
        let data = SRAM1_DRAM_BASE + 0x1D000;

        assert_eq!(soc.fetch(code + 0x40).expect("executable").len(), 0xFC0);
        assert!(soc.is_code_page(code) && soc.is_code_page(data));
        assert!(soc.take_invalidated().is_empty());

        assert_eq!(
            soc.store_mem(code + 0x80, 4, 0x1234_5678),
            Stored::Wrote { invalidated: true }
        );
        assert_eq!(soc.load_mem(code + 0x80, 4), Some(0x1234_5678));
        assert!(!soc.is_code_page(code) && !soc.is_code_page(data));
        let mut pages = soc.take_invalidated();
        pages.sort_unstable();
        assert_eq!(pages, vec![data >> 12, code >> 12]);
        assert!(soc.take_invalidated().is_empty());

        assert_eq!(
            soc.store_mem(code + 0x80, 4, 0),
            Stored::Wrote { invalidated: false }
        );

        // A store through the data view invalidates it too: the `_iram_end` page of the official
        // image is IRAM code and DRAM data at once.
        soc.fetch(code).expect("executable");
        assert_eq!(
            soc.store_mem(data + 0x100, 1, 0xFF),
            Stored::Wrote { invalidated: true }
        );
        assert_eq!(soc.take_invalidated().len(), 2);

        soc.fetch(code).expect("executable");
        assert_eq!(
            soc.store_mem(code + 0x1000, 4, 0),
            Stored::Wrote { invalidated: false }
        );
        assert!(soc.is_code_page(code));
        assert!(soc.take_invalidated().is_empty());
    }

    #[test]
    fn a_store_that_any_byte_rejects_writes_nothing() {
        let mut soc = soc_with_rom();
        let last = SRAM1_DRAM_BASE + mem::SRAM1_LEN - 2;
        assert_eq!(soc.store_mem(last, 4, 0xFFFF_FFFF), Stored::Unbacked);
        assert_eq!(soc.load_mem(last, 2), Some(0));
        assert_eq!(soc.load_mem(last, 4), None);
        assert_eq!(soc.load_mem(FLASH_IROM_BASE, 4), None);
        assert_eq!(soc.store_mem(FLASH_IROM_BASE, 4, 1), Stored::Unbacked);
        assert!(soc.fetch(FLASH_IROM_BASE).is_err());
        assert!(soc.arena.guards_intact());
    }

    #[test]
    fn an_access_that_straddles_a_page_boundary_still_reads_and_writes() {
        let mut soc = Soc::default();
        let at = SRAM1_DRAM_BASE + 0x1FFE;
        assert_eq!(
            soc.store_mem(at, 4, 0x1122_3344),
            Stored::Wrote { invalidated: false }
        );
        assert_eq!(soc.load_mem(at, 4), Some(0x1122_3344));
        assert_eq!(soc.load_mem(SRAM1_DRAM_BASE + 0x1FFC, 4), Some(0x3344_0000));
        assert_eq!(soc.load_mem(SRAM1_DRAM_BASE + 0x2000, 4), Some(0x0000_1122));
        assert_eq!(soc.load_mem(SRAM1_IRAM_BASE + 0x1FFE, 4), Some(0x1122_3344));
    }

    #[test]
    fn a_fetch_runs_to_the_end_of_its_page_and_marks_it() {
        let mut soc = soc_with_rom();
        let bytes = soc.fetch(ROM_BASE).expect("ROM is executable");
        assert_eq!(bytes.len(), pagetable::PAGE_SIZE as usize);
        assert_eq!(bytes[0], 0);
        assert_eq!(bytes[0x10], 1);
        let bytes = soc.fetch(ROM_BASE + 0xFFE).expect("ROM is executable");
        assert_eq!(bytes.len(), 2);
        assert!(soc.is_code_page(ROM_BASE));
        // The upper ROM page carries the mark in its data view too; ROM is read-only, so nothing
        // can actually write it.
        soc.fetch(ROM_BASE + ROM_DATA_IMAGE_OFF)
            .expect("executable");
        assert!(soc.is_code_page(ROM_DATA_BASE));
        assert_eq!(
            soc.fetch(0x3FCE_0000).unwrap_err(),
            Trap::instruction_access_fault(0x3FCE_0000)
        );
    }
}
