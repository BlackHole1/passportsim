//! Flash-cache stall accounting for the `device` timing profile, and the cache-error rule of a
//! disabled cache (IDF `esp32c3/rom/cache.h`; `specs/timing-profiles.toml`).
//!
//! Flash bytes come from the arena at full speed; what silicon charges is a stall. Three models:
//!
//! - [`CacheModel::ColdPage`]: the first DROM **data load** per 4 KB page after an MMU write or a
//!   cache flush pays one fill; fetches pay nothing.
//! - [`CacheModel::Lru16k`]: the chip's 16 KB cache, 8 ways of 32-byte lines in 64 sets, LRU,
//!   charged on a miss for **fetches and data loads** alike (`MAX_ICACHE_SIZE` 16384,
//!   `MAX_ICACHE_WAYS` 8, `MIN_CACHE_LINE_SIZE` 32; one cache behind both buses,
//!   `SOC_SHARED_IDCACHE_SUPPORTED`).
//! - [`CacheModel::Fifo16k`]: the same cache replacing first in, first out per set, as silicon
//!   does: in the `probe_campaign_timing` capture the EXTMEM IBUS and DBUS miss counters match
//!   FIFO on all 25 `TIME|ways_*` lines, where true LRU, tree pseudo-LRU and random miss by tens
//!   to hundreds; code and data share the ways (`ways_mixed_*`). The tag item in `cache.h`
//!   carries a 3-bit `fifo_cnt`. This is the `device` profile's variant.
//!
//! The caller charges the stall through `Clock::stall`; a charging load returns `OkStop` and a
//! charging fetch ends the run before the instruction, so the run loop recomputes its budget.
//! Under `fast` the fill is 0 ps and nothing is charged.
//!
//! The model depends on the instruction stream alone, so no block size, slice boundary, executor
//! or restore point can move it. A fetch touches its line only when it leaves the previous
//! fetch's line, so the engine's `FetchWatch` and the reference executor agree; a data load
//! touches its line every time.
//!
//! A miss starts its line's transfer [`CacheTiming::miss_cycles`] after it happens, once the
//! previous transfer ends; words arrive in order from [`CacheTiming::first_word_ps`] to
//! [`CacheTiming::fill_ps`], and an access waits for its own word only.
//!
//! A flash access with the cache disabled or its bus shut is an access error, not a silent 0
//! ([`CacheGate::error`]): the EXTMEM status bit latches, source 61 is asserted, and the access
//! traps at once, so the IDF panic handler rewrites `mcause` to 25 and prints "Cache error".

use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_rv32::trap::Trap;

use crate::mem;
use crate::pagetable::{self, PAGE_SIZE, PF_COLD, PF_R, PageTable};

/// Bytes one `cold_page` fill covers, also the page of the table and of the `PF_COLD` mark.
pub const CACHE_UNIT: u32 = PAGE_SIZE;

/// Bytes of the instruction cache, fixed on this chip (`memory.ld.in:24`, `MAX_ICACHE_SIZE`).
pub const ICACHE_BYTES: u32 = 16 * 1024;

/// log2 of the cache line, the unit one line fill covers (`MIN_CACHE_LINE_SIZE` 32).
pub const LINE_SHIFT: u32 = 5;

/// Ways of each set (`MAX_ICACHE_WAYS`).
pub const ICACHE_WAYS: usize = 8;

pub const ICACHE_SETS: usize = (ICACHE_BYTES >> LINE_SHIFT) as usize / ICACHE_WAYS;

/// Picoseconds of one line fill from first principles: a DIO read (`0xBB`) of 32 bytes at 80 MHz,
/// the `pk` image header's mode and speed, is 8 command, 12 address, 4 mode and 128 data clocks,
/// 1.9 us. **UNVERIFIED**: the fitted value is `specs/timing-profiles.toml` `cache_fill_ps`, passed
/// through [`CacheAccount::with_timing`]; this constant serves [`CacheAccount::device`].
pub const CACHE_FILL_PS: u64 = 1_900_000;

/// Tag of an empty way, and the previous-fetch line before any fetch. The 9 MB arena keeps every
/// real line far below it.
const LINE_NONE: u32 = u32::MAX;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CacheModel {
    /// First data load per 4 KB page after an MMU write or a cache flush, marked with `PF_COLD`;
    /// fetches are not charged.
    ColdPage,
    /// A miss in the 16 KB, 8-way LRU cache of 32-byte lines, for fetches and data loads:
    /// execute-in-place code that does not fit keeps missing and a hot loop that fits does not.
    Lru16k,
    /// FIFO per set, as silicon does: a hit changes nothing, a miss fills an empty way if there
    /// is one and otherwise replaces the line filled longest ago.
    Fifo16k,
}

impl CacheModel {
    /// Whether this is one of the 32-byte line caches, which differ only in replacement.
    pub const fn lines(self) -> bool {
        matches!(self, CacheModel::Lru16k | CacheModel::Fifo16k)
    }
}

/// The snapshot state of a [`CacheAccount`]. The model and the fill timing are configuration and
/// are not in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct CacheState {
    /// [`ICACHE_SETS`] x [`ICACHE_WAYS`] physical line numbers, set by set, each set ordered most
    /// recently used (`lru16k`) or filled (`fifo16k`) first; `u32::MAX` for an empty way, always
    /// at the end.
    pub ways: Vec<u32>,
    pub last_fetch: u32,
    pub fills: u64,
    /// Physical line of the last transfer, `u32::MAX` for none or after a flush.
    pub fill_line: u32,
    pub fill_start_ps: u64,
    pub busy_until_ps: u64,
}

/// How long a line fill takes (`specs/timing-profiles.toml` `cache_fill_ps`,
/// `cache_first_word_ps` and `cache_miss_cycles`). Configuration, not state.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheTiming {
    /// Picoseconds from the start of a transfer to its last word; under
    /// [`CacheModel::ColdPage`] the stall of one page fill.
    pub fill_ps: u64,
    /// Picoseconds from the start of a transfer to its first word; 0 makes a blocking fill.
    pub first_word_ps: u64,
    pub miss_cycles: u32,
}

impl CacheTiming {
    /// Whether words arrive one by one, so the CPU runs them as they come and reads one ahead; a
    /// blocking fill does neither.
    fn streams(&self) -> bool {
        self.first_word_ps > 0 && self.first_word_ps < self.fill_ps
    }

    /// Picoseconds from the start of a transfer to word `word` (0 to 7): evenly spaced from
    /// `first_word_ps` to `fill_ps`, or `fill_ps` for every word of a blocking fill.
    fn arrival(&self, word: u32) -> u64 {
        if self.first_word_ps == 0 || self.first_word_ps >= self.fill_ps {
            return self.fill_ps.max(self.first_word_ps);
        }
        let spread = self.fill_ps - self.first_word_ps;
        self.first_word_ps
            + spread * u64::from(word.min(LINE_WORDS - 1)) / u64::from(LINE_WORDS - 1)
    }
}

const LINE_WORDS: u32 = 1 << (LINE_SHIFT - 2);

/// The flash-cache stall account of one machine. It never holds a byte; it only answers how many
/// picoseconds a flash access costs.
#[derive(Clone, Debug)]
pub struct CacheAccount {
    model: CacheModel,
    /// How long a fill takes; `fill_ps` 0 under `fast`.
    timing: CacheTiming,
    /// Resident physical lines, [`ICACHE_WAYS`] per set (line caches only; see
    /// [`CacheState::ways`]).
    ways: Vec<u32>,
    /// Physical line of the previous instruction fetch, the fetch side's own: under `lru16k` a
    /// fetch never refreshes a line a data load made most recent.
    last_fetch: u32,
    /// Fills charged since power-on, for the receipts.
    fills: u64,
    /// Physical line of the last transfer, [`LINE_NONE`] for none (line caches only).
    fill_line: u32,
    fill_start_ps: u64,
    busy_until_ps: u64,
}

impl CacheAccount {
    pub fn fast() -> CacheAccount {
        CacheAccount::with_fill_ps(CacheModel::ColdPage, 0)
    }

    pub fn device(model: CacheModel) -> CacheAccount {
        CacheAccount::with_fill_ps(model, CACHE_FILL_PS)
    }

    pub fn with_fill_ps(model: CacheModel, fill_ps: u64) -> CacheAccount {
        CacheAccount::with_timing(
            model,
            CacheTiming {
                fill_ps,
                ..CacheTiming::default()
            },
        )
    }

    pub fn with_timing(model: CacheModel, timing: CacheTiming) -> CacheAccount {
        CacheAccount {
            model,
            timing,
            ways: vec![LINE_NONE; ICACHE_SETS * ICACHE_WAYS],
            last_fetch: LINE_NONE,
            fills: 0,
            fill_line: LINE_NONE,
            fill_start_ps: 0,
            busy_until_ps: 0,
        }
    }

    pub fn charges(&self) -> bool {
        self.timing.fill_ps > 0
    }

    /// Whether instruction fetches are charged (the engine installs `FetchWatch` for this): a
    /// line cache with a fill cost.
    pub fn charges_fetches(&self) -> bool {
        self.charges() && self.model.lines()
    }

    pub fn state(&self) -> CacheState {
        CacheState {
            ways: self.ways.clone(),
            last_fetch: self.last_fetch,
            fills: self.fills,
            fill_line: self.fill_line,
            fill_start_ps: self.fill_start_ps,
            busy_until_ps: self.busy_until_ps,
        }
    }

    /// Puts back a state [`CacheAccount::state`] took. A state of another shape leaves the
    /// account empty.
    pub fn restore(&mut self, state: &CacheState) {
        if state.ways.len() == self.ways.len() {
            self.ways.clone_from(&state.ways);
        } else {
            self.ways.fill(LINE_NONE);
        }
        self.last_fetch = state.last_fetch;
        self.fills = state.fills;
        self.fill_line = state.fill_line;
        self.fill_start_ps = state.fill_start_ps;
        self.busy_until_ps = state.busy_until_ps;
    }

    pub fn model(&self) -> CacheModel {
        self.model
    }

    pub fn fill_ps(&self) -> u64 {
        self.timing.fill_ps
    }

    pub fn timing(&self) -> CacheTiming {
        self.timing
    }

    pub fn fills(&self) -> u64 {
        self.fills
    }

    /// Picoseconds a data load of `addr` at `now` stalls the CPU. `cycle_ps` is one CPU cycle at
    /// the current clock, the unit of the miss cycles.
    ///
    /// Under [`CacheModel::ColdPage`] a charge clears the page's `PF_COLD` mark, so later loads
    /// are free until the next flush or MMU write. Under the line caches the mark stays, so every
    /// load comes back here. Only flash windows are charged; internal RAM is not cached.
    pub fn load(&mut self, pages: &mut PageTable, addr: u32, now: u64, cycle_ps: u64) -> u64 {
        match self.model {
            CacheModel::ColdPage => {
                if pages.entry(addr) & PF_COLD == 0 {
                    return 0;
                }
                warm(pages, addr);
                self.charge()
            }
            CacheModel::Lru16k | CacheModel::Fifo16k if !self.charges() => 0,
            CacheModel::Lru16k | CacheModel::Fifo16k => match physical_line(pages, addr) {
                Some(line) => self.access(line, addr, now, cycle_ps),
                None => 0,
            },
        }
    }

    /// Picoseconds an instruction fetch of `addr` at `now` stalls the CPU.
    ///
    /// [`CacheModel::ColdPage`] charges nothing. The line caches charge by physical line
    /// ([`physical_line`]); a fetch from the previous fetch's line touches nothing, nor does the
    /// second half of a 32-bit instruction straddling two lines. The fetch waits for the word that
    /// ends the straight-line run from `addr` ([`run_end`]), read from `arena`.
    pub fn fetch(
        &mut self,
        pages: &PageTable,
        arena: &[u8],
        addr: u32,
        now: u64,
        cycle_ps: u64,
    ) -> u64 {
        match self.model {
            CacheModel::ColdPage => 0,
            CacheModel::Lru16k | CacheModel::Fifo16k => {
                if !self.charges() {
                    return 0;
                }
                let Some(line) = physical_line(pages, addr) else {
                    return 0;
                };
                if line == self.last_fetch {
                    return 0;
                }
                self.last_fetch = line;
                let base = (line as usize) << LINE_SHIFT;
                let bytes = arena.get(base..base + (1 << LINE_SHIFT)).unwrap_or(&[]);
                let run = run_of(bytes, addr & ((1 << LINE_SHIFT) - 1));
                if !self.timing.streams() {
                    // A blocking fill: the run's last word is the whole wait.
                    return self.arrive(line, run.end >> 2, now, cycle_ps) - now;
                }
                self.streamed(pages, addr, line, run, now, cycle_ps)
            }
        }
    }

    /// The stall of a fetch that enters `line` at `addr` with the straight-line `run` there,
    /// for a fill whose words arrive in order:
    ///
    /// - the instructions before the run's last one execute as their words arrive, overlapping
    ///   the fill at their cost on a hit;
    /// - the fetch unit reads one word ahead, so the last instruction waits for the word after
    ///   the run's last word: the next word of the line or, for an unconditional transfer in the
    ///   line's last word, word 0 of the next line, which that read misses and fills.
    ///
    /// The engine charges the whole stall before the entering instruction, so the stall is the
    /// latest of those arrivals less the cycles of the instructions before the last one.
    fn streamed(
        &mut self,
        pages: &PageTable,
        addr: u32,
        line: u32,
        run: Run,
        now: u64,
        cycle_ps: u64,
    ) -> u64 {
        let last_word = run.end >> 2;
        let at_last = self.arrive(line, last_word, now, cycle_ps);
        let mut done = at_last.saturating_add(u64::from(run.in_last_word) * cycle_ps);
        if run.transfers {
            let ahead = if last_word + 1 < LINE_WORDS {
                self.arrive(line, last_word + 1, now, cycle_ps)
            } else {
                let next = (addr | ((1 << LINE_SHIFT) - 1)).wrapping_add(1);
                match physical_line(pages, next) {
                    Some(next_line) => {
                        let asked = at_last
                            .max(now.saturating_add(u64::from(run.before_last_word) * cycle_ps));
                        self.arrive(next_line, 0, asked, cycle_ps)
                    }
                    None => at_last,
                }
            };
            done = done.max(ahead);
        }
        let overlap = now.saturating_add(u64::from(run.insns.saturating_sub(1)) * cycle_ps);
        done.saturating_sub(overlap)
    }

    /// Drops everything the cache holds: an `ICACHE_SYNC_CTRL` invalidate, a cache disable, or a
    /// reset.
    ///
    /// Every variant marks each mapped DROM page cold again, so data loads keep reaching
    /// [`CacheAccount::load`]; the line caches also empty their ways. The IROM window is left
    /// alone: fetches reach the account through the engine. Under `fast` nothing is touched, which
    /// saves 2048 rewritten entries per invalidate.
    pub fn flush(&mut self, pages: &mut PageTable) {
        if self.timing.fill_ps == 0 {
            return;
        }
        if self.model.lines() {
            self.ways.fill(LINE_NONE);
            self.last_fetch = LINE_NONE;
            self.fill_line = LINE_NONE;
        }
        mark_cold(pages, mem::FLASH_DROM_BASE, mem::FLASH_WINDOW_LEN);
    }

    /// Accesses physical line `line` and says whether it was resident. A miss puts it at the
    /// front of the set, replacing the last way (the LRU line, the oldest fill, or an empty way).
    /// A hit moves the line to the front under `lru16k` and changes nothing under `fifo16k`.
    fn touch(&mut self, line: u32) -> bool {
        let set = (line as usize) % ICACHE_SETS;
        let ways = &mut self.ways[set * ICACHE_WAYS..(set + 1) * ICACHE_WAYS];
        let at = ways.iter().position(|way| *way == line);
        if at.is_some() && self.model == CacheModel::Fifo16k {
            return true;
        }
        let from = at.unwrap_or(ICACHE_WAYS - 1);
        ways[..=from].rotate_right(1);
        ways[0] = line;
        at.is_some()
    }

    fn access(&mut self, line: u32, addr: u32, now: u64, cycle_ps: u64) -> u64 {
        self.arrive(line, (addr >> 2) & (LINE_WORDS - 1), now, cycle_ps) - now
    }

    /// When word `word` of `line`, asked for at `now`, is there (never before `now`): a miss
    /// starts a transfer once the flash is free; a hit on the line in transfer waits for its word;
    /// any other hit is there at once.
    fn arrive(&mut self, line: u32, word: u32, now: u64, cycle_ps: u64) -> u64 {
        let word = word & (LINE_WORDS - 1);
        if self.touch(line) {
            if line != self.fill_line {
                return now;
            }
            let arrives = self.fill_start_ps.saturating_add(self.timing.arrival(word));
            return arrives.max(now);
        }
        self.fills += 1;
        let asked = now.max(self.busy_until_ps);
        let start =
            asked.saturating_add(u64::from(self.timing.miss_cycles).saturating_mul(cycle_ps));
        self.fill_line = line;
        self.fill_start_ps = start;
        self.busy_until_ps = start.saturating_add(self.timing.fill_ps);
        start.saturating_add(self.timing.arrival(word)).max(now)
    }

    fn charge(&mut self) -> u64 {
        self.fills += 1;
        self.timing.fill_ps
    }
}

/// The offset in its 32-byte line of the last byte a fetch entering at `offset` runs through: the
/// end of the first unconditional control transfer (jump, return or system instruction,
/// compressed forms included), or the line's end. Conditional branches are assumed to fall
/// through. The rule depends only on the line's bytes and the entry offset, so both executors
/// charge at the same boundary.
pub fn run_end(line: &[u8], offset: u32) -> u32 {
    run_of(line, offset).end
}

/// The straight-line run a fetch entering a 32-byte line at `offset` executes there, with what the
/// streaming fetch reads of it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub end: u32,
    /// Whether the run ends with an unconditional control transfer, after which the fetch unit's
    /// read-ahead is thrown away; false when it runs to the line's end.
    pub transfers: bool,
    pub insns: u32,
    pub before_last_word: u32,
    pub in_last_word: u32,
}

pub fn run_of(line: &[u8], offset: u32) -> Run {
    let last = (1u32 << LINE_SHIFT) - 1;
    let mut at = offset & !1;
    let mut starts: [u8; 16] = [0; 16];
    let mut n = 0usize;
    let finish = |end: u32, transfers: bool, starts: &[u8]| {
        let word = end & !3;
        let before = starts.iter().filter(|s| u32::from(**s) < word).count() as u32;
        let insns = starts.len() as u32;
        Run {
            end,
            transfers,
            insns,
            before_last_word: before,
            in_last_word: insns.saturating_sub(before).saturating_sub(1),
        }
    };
    while at < last {
        let Some(lo) = line.get(at as usize..at as usize + 2) else {
            return finish(last, false, &starts[..n]);
        };
        let half = u16::from_le_bytes([lo[0], lo[1]]);
        starts[n] = at as u8;
        n += 1;
        if half & 3 != 3 {
            if compressed_transfer(half) {
                return finish(at + 1, true, &starts[..n]);
            }
            at += 2;
            continue;
        }
        if at + 3 > last {
            return finish(last, false, &starts[..n]);
        }
        // JALR, JAL, and SYSTEM with funct3 0 (ecall, ebreak, mret, wfi); BRANCH is conditional
        // and the CSR instructions do not transfer control.
        let system = half & 0x7F == 0x73 && (half >> 12) & 7 == 0;
        if matches!(half & 0x7F, 0x67 | 0x6F) || system {
            return finish(at + 3, true, &starts[..n]);
        }
        at += 4;
    }
    finish(last, false, &starts[..n])
}

/// Whether a 16-bit instruction transfers control unconditionally: `c.j`, `c.jal` (quadrant 1)
/// or `c.jr`, `c.jalr`, `c.ebreak` (quadrant 2, funct3 100 with `rs2` 0 and `rs1` or bit 12
/// set), per the RISC-V "C" extension.
fn compressed_transfer(half: u16) -> bool {
    let funct3 = half >> 13;
    match half & 3 {
        1 => matches!(funct3, 0b001 | 0b101),
        2 => funct3 == 0b100 && (half >> 2) & 31 == 0 && half & 0x1F80 != 0,
        _ => false,
    }
}

/// The physical cache line of `addr`, or `None` outside the flash windows or on an unmapped page.
///
/// Lines are keyed by the **physical** address (the arena offset), not the virtual one: both
/// windows translate through the same MMU entry, so the DROM and IROM views of a flash line are
/// one line (`SOC_SHARED_IDCACHE_SUPPORTED`).
fn physical_line(pages: &PageTable, addr: u32) -> Option<u32> {
    if !mem::is_flash_window(addr) {
        return None;
    }
    let entry = pages.entry(addr);
    if entry == 0 {
        return None;
    }
    let physical = pagetable::arena_off(entry) as u32 + (addr & (PAGE_SIZE - 1));
    Some(physical >> LINE_SHIFT)
}

impl Default for CacheAccount {
    /// The `fast` profile, the default until calibration validates `device`.
    fn default() -> CacheAccount {
        CacheAccount::fast()
    }
}

/// Marks every mapped page of `[vbase, vbase + len)` `PF_COLD`, so its next data load pays a
/// fill. An MMU entry write does this to the data pages it rewrites under `device`, and
/// [`CacheAccount::flush`] to the whole DROM window. Unmapped pages are skipped: marking entry 0
/// would turn "unmapped" into a mapped page at arena offset 0.
pub fn mark_cold(pages: &mut PageTable, vbase: u32, len: u32) {
    let mut off = 0;
    while off < len {
        let at = vbase.wrapping_add(off);
        let entry = pages.entry(at);
        if entry != 0 {
            let arena = entry & pagetable::ARENA_MASK;
            let flags = pagetable::flags(entry) | PF_COLD;
            pages.set_entry(at >> 12, pagetable::entry(arena, flags));
        }
        off += PAGE_SIZE;
    }
}

/// Clears the `PF_COLD` mark of the page holding `addr` and gives back its `PF_R`. Only flash
/// data pages (`PF_R`) are ever marked, and [`crate::pagetable::fold`] drops `PF_R` while the mark
/// is set, so restoring it is exact; a `PF_SLOW` page loses it again in the same fold.
pub fn warm(pages: &mut PageTable, addr: u32) {
    let entry = pages.entry(addr);
    if entry & PF_COLD == 0 {
        return;
    }
    let arena = entry & pagetable::ARENA_MASK;
    let flags = (pagetable::flags(entry) & !PF_COLD) | PF_R;
    pages.set_entry(addr >> 12, pagetable::entry(arena, flags));
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CacheAccess {
    Fetch,
    Load,
    Store,
}

/// `EXTMEM_CORE0_IBUS_ACS_MSK_ICACHE_ST`, bit 0 of `EXTMEM_CORE0_ACS_CACHE_INT_ST_REG`
/// (0x600C408C): an IBUS access while the cache is masked.
pub const ACS_IBUS_MSK_IC: u32 = 1 << 0;

/// `EXTMEM_CORE0_DBUS_ACS_MSK_ICACHE_ST`, bit 3 of `EXTMEM_CORE0_ACS_CACHE_INT_ST_REG`
/// (0x600C408C): a DBUS access while the cache is masked.
pub const ACS_DBUS_MSK_IC: u32 = 1 << 3;

/// The two bus gates of `EXTMEM_ICACHE_CTRL` and `EXTMEM_ICACHE_CTRL1` as a decision.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CacheGate {
    /// `EXTMEM_ICACHE_ENABLE`, bit 0 of `EXTMEM_ICACHE_CTRL` (0x600C4000).
    pub enabled: bool,
    /// `EXTMEM_ICACHE_SHUT_IBUS`, bit 0 of `EXTMEM_ICACHE_CTRL1` (0x600C4004).
    pub shut_ibus: bool,
    /// `EXTMEM_ICACHE_SHUT_DBUS`, bit 1 of `EXTMEM_ICACHE_CTRL1` (0x600C4004).
    pub shut_dbus: bool,
}

const CTRL_ICACHE_ENABLE: u32 = 1 << 0;
const CTRL1_SHUT_IBUS: u32 = 1 << 0;
const CTRL1_SHUT_DBUS: u32 = 1 << 1;

impl CacheGate {
    /// The reset state: cache off and both buses shut (`EXTMEM_ICACHE_CTRL` 0,
    /// `EXTMEM_ICACHE_CTRL1` 3).
    pub const RESET: CacheGate = CacheGate {
        enabled: false,
        shut_ibus: true,
        shut_dbus: true,
    };

    /// Both buses open through an enabled cache, as `Cache_Resume_ICache` leaves it.
    pub const OPEN: CacheGate = CacheGate {
        enabled: true,
        shut_ibus: false,
        shut_dbus: false,
    };

    pub const fn from_regs(ctrl: u32, ctrl1: u32) -> CacheGate {
        CacheGate {
            enabled: ctrl & CTRL_ICACHE_ENABLE != 0,
            shut_ibus: ctrl1 & CTRL1_SHUT_IBUS != 0,
            shut_dbus: ctrl1 & CTRL1_SHUT_DBUS != 0,
        }
    }

    /// Whether an access to `addr` reaches flash. Only the two flash windows are gated.
    pub const fn allows(&self, addr: u32, access: CacheAccess) -> bool {
        self.error(addr, access).is_none()
    }

    /// The cache error an access to `addr` raises, or `None` when it reaches flash normally.
    ///
    /// The window decides the bus: `0x42000000` upwards is IBUS, `0x3C000000` upwards DBUS. An
    /// access is blocked when the cache is disabled or that bus is shut, the states
    /// `cache_ll_l1_disable_bus` and `Cache_Suspend_ICache` leave. Latching the status bit and
    /// asserting the source are the caller's job: this module holds no registers.
    pub const fn error(&self, addr: u32, access: CacheAccess) -> Option<CacheError> {
        let drom =
            addr >= mem::FLASH_DROM_BASE && addr - mem::FLASH_DROM_BASE < mem::FLASH_WINDOW_LEN;
        let irom =
            addr >= mem::FLASH_IROM_BASE && addr - mem::FLASH_IROM_BASE < mem::FLASH_WINDOW_LEN;
        let (shut, status_bit) = if irom {
            (self.shut_ibus, ACS_IBUS_MSK_IC)
        } else if drom {
            (self.shut_dbus, ACS_DBUS_MSK_IC)
        } else {
            return None;
        };
        if self.enabled && !shut {
            return None;
        }
        Some(CacheError {
            status_bit,
            source: irq::CACHE_CORE0_ACS,
            trap: trap_of(addr, access),
        })
    }
}

/// The synchronous exception a cache error raises at once, before any interrupt.
///
/// A fetch takes mcause 2 on the 0 word a masked cache returns; `panic_soc_check_pseudo_cause`
/// rewrites it to 25 and prints "Cache error". A load takes mcause 5 and a store mcause 7 with
/// the data address. **UNVERIFIED**: the fetch cause may be 1, and the store cause is not
/// confirmed on silicon.
const fn trap_of(addr: u32, access: CacheAccess) -> Trap {
    match access {
        CacheAccess::Fetch => Trap::illegal_instruction(0),
        CacheAccess::Load => Trap::load_access_fault(addr),
        CacheAccess::Store => Trap::store_access_fault(addr),
    }
}

/// What an access through a disabled cache or a shut bus produces.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CacheError {
    /// Bit to latch in `EXTMEM_CORE0_ACS_CACHE_INT_ST_REG`, gated by its ENA register.
    pub status_bit: u32,
    /// Interrupt source to assert: 61, which IDF routes to line 25.
    pub source: IrqSource,
    pub trap: Trap,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{FLASH_ARENA, FLASH_DROM_BASE, FLASH_IROM_BASE, SRAM1_DRAM_BASE};
    use crate::pagetable::{PF_W, PF_X, fast_load};
    use pemu_rv32::trap::{EXC_ILLEGAL_INSN, EXC_LOAD_ACCESS_FAULT, EXC_STORE_ACCESS_FAULT};

    const CYCLE_PS: u64 = 6_250;

    static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// A clock at least one second after the last call's: far past any transfer, so each access
    /// stalls as a blocking fill would.
    fn later() -> u64 {
        const SECOND_PS: u64 = 1_000_000_000_000;
        CLOCK.fetch_add(SECOND_PS, std::sync::atomic::Ordering::Relaxed) + SECOND_PS
    }

    /// A table with the first `pages` 4 KB pages of each flash window mapped read-only, as an MMU
    /// entry write leaves it.
    fn mapped_flash(pages: u32) -> PageTable {
        let mut table = PageTable::new();
        for i in 0..pages {
            let arena = FLASH_ARENA + i * PAGE_SIZE;
            table.set_entry(
                (FLASH_DROM_BASE + i * PAGE_SIZE) >> 12,
                pagetable::entry(arena, PF_R),
            );
            table.set_entry(
                (FLASH_IROM_BASE + i * PAGE_SIZE) >> 12,
                pagetable::entry(arena, PF_R | PF_X),
            );
        }
        table
    }

    #[test]
    fn the_cold_page_variant_charges_the_first_data_load_of_each_page() {
        let mut pages = mapped_flash(3);
        let mut acc = CacheAccount::device(CacheModel::ColdPage);
        mark_cold(&mut pages, FLASH_DROM_BASE, 2 * PAGE_SIZE);

        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE + 0x10, later(), CYCLE_PS),
            CACHE_FILL_PS
        );
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE + 0x20, later(), CYCLE_PS),
            0,
            "same page"
        );
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE + PAGE_SIZE, later(), CYCLE_PS),
            CACHE_FILL_PS
        );
        assert_eq!(
            acc.load(
                &mut pages,
                FLASH_DROM_BASE + 2 * PAGE_SIZE,
                later(),
                CYCLE_PS
            ),
            0
        );
        assert_eq!(acc.fills(), 2);

        for page in 0..3 {
            assert_eq!(
                acc.fetch(
                    &pages,
                    &[],
                    FLASH_IROM_BASE + page * PAGE_SIZE,
                    later(),
                    CYCLE_PS
                ),
                0
            );
        }
        assert_eq!(acc.fills(), 2);

        acc.flush(&mut pages);
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE, later(), CYCLE_PS),
            CACHE_FILL_PS
        );
        assert_eq!(
            acc.load(
                &mut pages,
                FLASH_DROM_BASE + 2 * PAGE_SIZE,
                later(),
                CYCLE_PS
            ),
            CACHE_FILL_PS
        );
        assert_eq!(acc.fills(), 4);
    }

    #[test]
    fn a_charged_page_goes_back_on_the_fast_load_path() {
        // PF_COLD pages carry no R bit in the fast mask; clearing the mark must give it back, or
        // the page would stay off the fast path for the rest of the run.
        let mut pages = mapped_flash(1);
        let mut acc = CacheAccount::device(CacheModel::ColdPage);
        assert!(fast_load(pages.entry(FLASH_DROM_BASE), FLASH_DROM_BASE, 4));

        mark_cold(&mut pages, FLASH_DROM_BASE, PAGE_SIZE);
        assert_eq!(pages.entry(FLASH_DROM_BASE) & PF_COLD, PF_COLD);
        assert!(!fast_load(pages.entry(FLASH_DROM_BASE), FLASH_DROM_BASE, 4));

        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE, later(), CYCLE_PS),
            CACHE_FILL_PS
        );
        assert_eq!(
            pages.entry(FLASH_DROM_BASE),
            pagetable::entry(FLASH_ARENA, PF_R)
        );
        assert!(fast_load(pages.entry(FLASH_DROM_BASE), FLASH_DROM_BASE, 4));
        assert_eq!(pages.entry(FLASH_DROM_BASE) & PF_W, 0);
    }

    #[test]
    fn marking_leaves_unmapped_pages_unmapped() {
        // Entry 0 means unmapped; marking it would map arena offset 0, a guard page.
        let mut pages = mapped_flash(1);
        mark_cold(&mut pages, FLASH_DROM_BASE, 4 * PAGE_SIZE);
        assert_eq!(pages.entry(FLASH_DROM_BASE) & PF_COLD, PF_COLD);
        assert_eq!(pages.entry(FLASH_DROM_BASE + PAGE_SIZE), 0);
        let mut acc = CacheAccount::device(CacheModel::ColdPage);
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE + PAGE_SIZE, later(), CYCLE_PS),
            0
        );
    }

    #[test]
    fn the_lru16k_variant_is_eight_ways_of_32_byte_lines_over_fetches_and_loads() {
        assert_eq!((ICACHE_SETS, ICACHE_WAYS, 1 << LINE_SHIFT), (64, 8, 32));
        let mut pages = mapped_flash(8);
        let mut acc = CacheAccount::device(CacheModel::Lru16k);
        let same_set = |i: u32| FLASH_IROM_BASE + i * (ICACHE_SETS as u32) * 32;

        for i in 0..8 {
            assert_eq!(
                acc.fetch(&pages, &[], same_set(i), later(), CYCLE_PS),
                CACHE_FILL_PS,
                "cold line {i}"
            );
        }
        for i in 0..8 {
            assert_eq!(
                acc.fetch(&pages, &[], same_set(i), later(), CYCLE_PS),
                0,
                "resident line {i}"
            );
        }
        assert_eq!(acc.fills(), 8);
        assert_eq!(
            acc.fetch(&pages, &[], same_set(8), later(), CYCLE_PS),
            CACHE_FILL_PS
        );
        assert_eq!(
            acc.fetch(&pages, &[], same_set(0), later(), CYCLE_PS),
            CACHE_FILL_PS,
            "0 was evicted"
        );
        assert_eq!(acc.fetch(&pages, &[], same_set(2), later(), CYCLE_PS), 0);
        assert_eq!(
            acc.fetch(&pages, &[], same_set(1), later(), CYCLE_PS),
            CACHE_FILL_PS,
            "1 was evicted"
        );

        let fills = acc.fills();
        assert_eq!(
            acc.fetch(&pages, &[], FLASH_IROM_BASE + 32, later(), CYCLE_PS),
            CACHE_FILL_PS
        );
        assert_eq!(acc.fetch(&pages, &[], same_set(2), later(), CYCLE_PS), 0);
        assert_eq!(acc.fills(), fills + 1);

        // The unit is the line, not the page.
        let fills = acc.fills();
        assert_eq!(
            acc.fetch(&pages, &[], same_set(2) + 4, later(), CYCLE_PS),
            0
        );
        assert_eq!(
            acc.fetch(&pages, &[], same_set(2) + 32, later(), CYCLE_PS),
            CACHE_FILL_PS
        );
        assert_eq!(acc.fills(), fills + 1);

        // Fetches and data loads share lines by *physical* line: the DROM and IROM views of one
        // flash page are one line.
        let fills = acc.fills();
        assert_eq!(
            acc.load(
                &mut pages,
                FLASH_DROM_BASE + 2 * (ICACHE_SETS as u32) * 32 + 8,
                later(),
                CYCLE_PS
            ),
            0,
            "the IROM view of this line is already resident"
        );
        assert_eq!(acc.fills(), fills);
        assert_eq!(
            acc.load(
                &mut pages,
                FLASH_DROM_BASE + 5 * PAGE_SIZE,
                later(),
                CYCLE_PS
            ),
            CACHE_FILL_PS
        );
        assert_eq!(
            acc.load(
                &mut pages,
                FLASH_DROM_BASE + 5 * PAGE_SIZE + 31,
                later(),
                CYCLE_PS
            ),
            0
        );
        assert_eq!(acc.fills(), fills + 1);
        let fills = acc.fills();
        assert_eq!(
            acc.load(
                &mut pages,
                FLASH_DROM_BASE + 8 * PAGE_SIZE,
                later(),
                CYCLE_PS
            ),
            0
        );
        assert_eq!(
            acc.fetch(
                &pages,
                &[],
                FLASH_IROM_BASE + 8 * PAGE_SIZE,
                later(),
                CYCLE_PS
            ),
            0
        );
        assert_eq!(acc.fills(), fills);

        assert_eq!(acc.load(&mut pages, SRAM1_DRAM_BASE, later(), CYCLE_PS), 0);
        assert_eq!(
            acc.fetch(&pages, &[], SRAM1_DRAM_BASE, later(), CYCLE_PS),
            0
        );
        // A load never takes the `PF_COLD` mark off: every load must come back to be counted.
        mark_cold(&mut pages, FLASH_DROM_BASE, 8 * PAGE_SIZE);
        assert_eq!(acc.load(&mut pages, FLASH_DROM_BASE, later(), CYCLE_PS), 0);
        assert_ne!(pages.entry(FLASH_DROM_BASE) & PF_COLD, 0);

        let state = acc.state();
        let mut copy = CacheAccount::device(CacheModel::Lru16k);
        copy.restore(&state);
        assert_eq!(copy.state(), state);
        assert_eq!(copy.fetch(&pages, &[], same_set(2), later(), CYCLE_PS), 0);
        acc.flush(&mut pages);
        let fills = acc.fills();
        for i in 0..8 {
            assert_eq!(
                acc.fetch(&pages, &[], same_set(i), later(), CYCLE_PS),
                CACHE_FILL_PS,
                "after a flush, line {i}"
            );
        }
        assert_eq!(acc.fills(), fills + 8);
    }

    #[test]
    fn the_fifo16k_variant_replaces_the_line_filled_longest_ago() {
        // `probe_campaign_timing` `TIME|ways_retouch_n10`, lines 0 to 7, 0, 1, 8, 9 of one set
        // each pass, reads 640 IBUS misses over 64 passes: ten a pass, the re-touched 0 and 1
        // included, which only FIFO gives (true LRU 512).
        let pages = mapped_flash(8);
        let same_set = |i: u32| FLASH_IROM_BASE + i * (ICACHE_SETS as u32) * 32;
        let pass = [0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 8, 9];
        let misses = |model: CacheModel| -> u64 {
            let mut acc = CacheAccount::device(model);
            for i in pass {
                acc.fetch(&pages, &[], same_set(i), later(), CYCLE_PS);
            }
            let cold = acc.fills();
            for _ in 0..64 {
                for i in pass {
                    acc.fetch(&pages, &[], same_set(i), later(), CYCLE_PS);
                }
            }
            acc.fills() - cold
        };
        assert_eq!(misses(CacheModel::Fifo16k), 640);
        assert_eq!(misses(CacheModel::Lru16k), 512);

        let mut pages = mapped_flash(8);
        let mut acc = CacheAccount::device(CacheModel::Fifo16k);
        for i in 0..8 {
            acc.fetch(&pages, &[], same_set(i), later(), CYCLE_PS);
        }
        assert_eq!(acc.fetch(&pages, &[], same_set(0), later(), CYCLE_PS), 0);
        assert_eq!(
            acc.fetch(&pages, &[], same_set(8), later(), CYCLE_PS),
            CACHE_FILL_PS
        );
        assert_eq!(
            acc.fetch(&pages, &[], same_set(0), later(), CYCLE_PS),
            CACHE_FILL_PS,
            "0 was replaced although it had just hit"
        );
        // The set now holds, newest first, 0, 8, 7 to 2. Data loads share the ways
        // (`ways_mixed_*`): a load of line 10 of set 0 replaces the oldest, 2, and leaves 3.
        let fills = acc.fills();
        assert_eq!(
            acc.load(
                &mut pages,
                FLASH_DROM_BASE + 5 * PAGE_SIZE,
                later(),
                CYCLE_PS
            ),
            CACHE_FILL_PS
        );
        assert_eq!(acc.fills(), fills + 1);
        assert_eq!(acc.fetch(&pages, &[], same_set(3), later(), CYCLE_PS), 0);
        assert_eq!(
            acc.fetch(&pages, &[], same_set(2), later(), CYCLE_PS),
            CACHE_FILL_PS
        );
        let state = acc.state();
        let mut copy = CacheAccount::device(CacheModel::Fifo16k);
        copy.restore(&state);
        assert_eq!(copy.state(), state);
        acc.flush(&mut pages);
        assert!(acc.state().ways.iter().all(|w| *w == LINE_NONE));
    }

    #[test]
    fn the_fast_profile_charges_nothing_in_either_variant() {
        assert_eq!(CacheAccount::default().fill_ps(), 0);
        for model in [
            CacheModel::ColdPage,
            CacheModel::Lru16k,
            CacheModel::Fifo16k,
        ] {
            let mut pages = mapped_flash(2);
            let mut acc = CacheAccount::with_fill_ps(model, 0);
            mark_cold(&mut pages, FLASH_DROM_BASE, 2 * PAGE_SIZE);
            assert_eq!(
                acc.load(&mut pages, FLASH_DROM_BASE, later(), CYCLE_PS),
                0,
                "{model:?}"
            );
            assert_eq!(
                acc.fetch(&pages, &[], FLASH_IROM_BASE, later(), CYCLE_PS),
                0,
                "{model:?}"
            );
            assert_eq!(acc.model(), model);
        }

        // A flush under `fast` must not mark 2048 DROM pages cold to charge zero picoseconds:
        // every mark costs a rewritten entry now and a slow-path load later.
        for model in [
            CacheModel::ColdPage,
            CacheModel::Lru16k,
            CacheModel::Fifo16k,
        ] {
            let mut pages = mapped_flash(2);
            let before: Vec<u32> = (0..2)
                .map(|i| pages.entry(FLASH_DROM_BASE + i * PAGE_SIZE))
                .collect();
            CacheAccount::with_fill_ps(model, 0).flush(&mut pages);
            for (i, entry) in before.iter().enumerate() {
                let at = FLASH_DROM_BASE + i as u32 * PAGE_SIZE;
                assert_eq!(
                    pages.entry(at) & PF_COLD,
                    0,
                    "{model:?} marked {at:#X} cold"
                );
                assert_eq!(pages.entry(at), *entry, "{model:?} moved {at:#X}");
            }
            CacheAccount::device(model).flush(&mut pages);
            assert_ne!(pages.entry(FLASH_DROM_BASE) & PF_COLD, 0, "{model:?}");
        }
    }

    #[test]
    fn a_flash_access_through_a_disabled_cache_is_a_cache_error() {
        // With ICACHE_ENABLE 0, or the bus's SHUT bit set, a fetch or data access in either flash
        // window latches the matching CORE0_ACS_CACHE_INT_ST bit, asserts source 61 and faults.
        let off = CacheGate::from_regs(0, 0);
        assert_eq!(
            off,
            CacheGate {
                enabled: false,
                shut_ibus: false,
                shut_dbus: false
            }
        );

        let fetch = off
            .error(FLASH_IROM_BASE, CacheAccess::Fetch)
            .expect("cache disabled");
        assert_eq!(fetch.status_bit, ACS_IBUS_MSK_IC);
        assert_eq!(fetch.source, irq::CACHE_CORE0_ACS);
        assert_eq!(fetch.trap.cause, EXC_ILLEGAL_INSN);
        assert_eq!(fetch.trap.tval, 0, "a masked cache returns 0");

        let load = off
            .error(FLASH_DROM_BASE + 0x40, CacheAccess::Load)
            .expect("cache disabled");
        assert_eq!(load.status_bit, ACS_DBUS_MSK_IC);
        assert_eq!(load.trap.cause, EXC_LOAD_ACCESS_FAULT);
        assert_eq!(load.trap.tval, FLASH_DROM_BASE + 0x40);

        let store = off
            .error(FLASH_DROM_BASE, CacheAccess::Store)
            .expect("cache disabled");
        assert_eq!(store.trap.cause, EXC_STORE_ACCESS_FAULT);

        for addr in [SRAM1_DRAM_BASE, 0x4000_0000, 0x6000_0000, 0x5000_0000] {
            assert_eq!(off.error(addr, CacheAccess::Load), None, "{addr:#X}");
            assert!(off.allows(addr, CacheAccess::Fetch), "{addr:#X}");
        }
        for base in [FLASH_DROM_BASE, FLASH_IROM_BASE] {
            assert!(!off.allows(base, CacheAccess::Load));
            assert!(!off.allows(base + mem::FLASH_WINDOW_LEN - 1, CacheAccess::Load));
            assert!(off.allows(base + mem::FLASH_WINDOW_LEN, CacheAccess::Load));
        }
    }

    #[test]
    fn a_shut_bus_blocks_only_its_own_window() {
        // `Cache_Suspend_ICache` sets both SHUT bits and `Cache_Resume_ICache` clears them;
        // `cache_ll_l1_disable_bus` sets one at a time.
        assert_eq!(
            CacheGate::from_regs(1, 3),
            CacheGate {
                enabled: true,
                shut_ibus: true,
                shut_dbus: true
            }
        );
        assert_eq!(CacheGate::RESET, CacheGate::from_regs(0, 3));
        assert_eq!(CacheGate::OPEN, CacheGate::from_regs(1, 0));

        let ibus_shut = CacheGate::from_regs(1, 1);
        assert!(!ibus_shut.allows(FLASH_IROM_BASE, CacheAccess::Fetch));
        assert!(ibus_shut.allows(FLASH_DROM_BASE, CacheAccess::Load));

        let dbus_shut = CacheGate::from_regs(1, 2);
        assert!(dbus_shut.allows(FLASH_IROM_BASE, CacheAccess::Fetch));
        assert!(!dbus_shut.allows(FLASH_DROM_BASE, CacheAccess::Load));

        for base in [FLASH_DROM_BASE, FLASH_IROM_BASE] {
            for access in [CacheAccess::Fetch, CacheAccess::Load, CacheAccess::Store] {
                assert_eq!(
                    CacheGate::OPEN.error(base, access),
                    None,
                    "{base:#X} {access:?}"
                );
            }
        }
        assert!(!CacheGate::RESET.allows(FLASH_IROM_BASE, CacheAccess::Fetch));
        assert!(!CacheGate::RESET.allows(FLASH_DROM_BASE, CacheAccess::Load));
    }

    /// The `device` fill timing: 10 miss cycles, word 0 at 0.59 us, the line at 1.9775 us.
    fn overlapping() -> CacheAccount {
        CacheAccount::with_timing(
            CacheModel::Lru16k,
            CacheTiming {
                fill_ps: 1_977_500,
                first_word_ps: 590_000,
                miss_cycles: 10,
            },
        )
    }

    /// A miss waits for its own word only, the flash stays busy with the rest of the line, and the
    /// next miss waits for it; a hit on the line in transfer waits for its word.
    #[test]
    fn a_miss_waits_for_its_word_and_the_next_miss_for_the_transfer() {
        let mut pages = mapped_flash(2);
        let mut acc = overlapping();
        let t = 1_000_000_000;
        let miss = 10 * CYCLE_PS;
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE, t, CYCLE_PS),
            miss + 590_000
        );
        let start = t + miss;
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE + 28, start + 590_000, CYCLE_PS),
            1_977_500 - 590_000
        );
        let word3 = 590_000 + 3 * (1_977_500 - 590_000) / 7;
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE + 12, start + 590_000, CYCLE_PS),
            word3 - 590_000
        );
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE + 12, start + word3, CYCLE_PS),
            0
        );
        // The next line, asked 100 ns after word 0 arrived: it waits for the transfer, then its
        // own 10 cycles and first word.
        let asked = start + 690_000;
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE + 32, asked, CYCLE_PS),
            start + 1_977_500 + miss + 590_000 - asked
        );
        let late = start + 100_000_000;
        assert_eq!(
            acc.load(&mut pages, FLASH_DROM_BASE + 64, late, CYCLE_PS),
            miss + 590_000
        );
        assert_eq!(acc.fills(), 3);
        let state = acc.state();
        let mut copy = overlapping();
        copy.restore(&state);
        assert_eq!(copy.state(), state);
        assert_eq!(
            copy.load(&mut pages, FLASH_DROM_BASE + 64 + 28, late, CYCLE_PS),
            acc.load(&mut pages, FLASH_DROM_BASE + 64 + 28, late, CYCLE_PS)
        );
        let mut blocking = CacheAccount::with_fill_ps(CacheModel::Lru16k, 1_960_000);
        assert_eq!(
            blocking.load(&mut pages, FLASH_DROM_BASE, t, CYCLE_PS),
            1_960_000
        );
        assert_eq!(
            blocking.load(&mut pages, FLASH_DROM_BASE + 32, t + 1_960_000, CYCLE_PS),
            1_960_000
        );
    }

    /// A fetch that enters a line runs its words as they arrive and, before the run's last
    /// instruction completes, waits for the word after the run's last word.
    #[test]
    fn a_fetch_streams_its_run_and_waits_for_the_word_ahead() {
        let pages = mapped_flash(2);
        let mut acc = overlapping();
        let t = 1_000_000_000;
        let miss = 10 * CYCLE_PS;
        let spacing = (1_977_500 - 590_000) / 7;
        let word = |w: u64| miss + 590_000 + w * spacing;
        let mut line = vec![0u8; (FLASH_ARENA + 2 * PAGE_SIZE) as usize];
        // c.addi, c.addi, c.jr ra: the run ends in word 1 with a transfer, so the read of word 2
        // is outstanding when c.jr completes, two instructions after the entry.
        line[FLASH_ARENA as usize..][..6].copy_from_slice(&[0x05, 0x04, 0x05, 0x04, 0x82, 0x80]);
        assert_eq!(
            run_of(&line[FLASH_ARENA as usize..][..32], 0),
            Run {
                end: 5,
                transfers: true,
                insns: 3,
                before_last_word: 2,
                in_last_word: 0,
            }
        );
        assert_eq!(
            acc.fetch(&pages, &line, FLASH_IROM_BASE, t, CYCLE_PS),
            word(2) - 2 * CYCLE_PS
        );
        // No transfer in the next line: sixteen c.unimp halves run to the line's end.
        let late = t + 100_000_000;
        assert_eq!(
            acc.fetch(&pages, &line, FLASH_IROM_BASE + 32, late, CYCLE_PS),
            miss + 1_977_500 + CYCLE_PS - 15 * CYCLE_PS
        );
        assert_eq!(
            acc.fetch(&pages, &line, FLASH_IROM_BASE + 36, late, CYCLE_PS),
            0
        );
        assert_eq!(acc.fills(), 2);
        // A jump in the line's last word reads word 0 of the next line ahead: that read misses
        // and fills the line once the flash is free.
        let mut acc = overlapping();
        line[FLASH_ARENA as usize + 28..][..4].copy_from_slice(&[0x6F, 0x00, 0x00, 0x00]);
        let run = run_of(&line[FLASH_ARENA as usize..][..32], 28);
        assert_eq!((run.end, run.transfers, run.insns), (31, true, 1));
        let entry = acc.fetch(&pages, &line, FLASH_IROM_BASE + 28, t, CYCLE_PS);
        let flash_free = t + miss + 1_977_500;
        assert_eq!(entry, flash_free + miss + 590_000 - t);
        assert_eq!(acc.fills(), 2);
        let asked = t + entry;
        assert!(acc.fetch(&pages, &line, FLASH_IROM_BASE + 32, asked, CYCLE_PS) > 0);
        assert_eq!(acc.fills(), 2);
        let mut blocking = CacheAccount::with_fill_ps(CacheModel::Lru16k, 1_960_000);
        assert_eq!(
            blocking.fetch(&pages, &line, FLASH_IROM_BASE + 28, t, CYCLE_PS),
            1_960_000
        );
        assert_eq!(blocking.fills(), 1);
    }

    #[test]
    fn a_run_ends_at_the_first_unconditional_transfer_or_the_line_end() {
        let nop32 = [0x13, 0x00, 0x00, 0x00]; // addi x0, x0, 0
        let beq = [0x63, 0x00, 0x00, 0x00]; // beq x0, x0, +0: conditional, runs on
        let jal = [0xEF, 0x00, 0x00, 0x00]; // jal ra, +0
        let csrr = [0x73, 0x27, 0x20, 0x7E]; // csrr a4, 0x7e2: not a transfer
        let mret = [0x73, 0x00, 0x20, 0x30]; // mret
        let mut line = [0u8; 32];
        for w in 0..8 {
            line[w * 4..w * 4 + 4].copy_from_slice(&nop32);
        }
        assert_eq!(run_end(&line, 0), 31);
        line[8..12].copy_from_slice(&beq);
        line[12..16].copy_from_slice(&csrr);
        line[16..20].copy_from_slice(&jal);
        assert_eq!(run_end(&line, 0), 19);
        assert_eq!(run_end(&line, 20), 31);
        line[24..28].copy_from_slice(&mret);
        assert_eq!(run_end(&line, 20), 27);
        let mut c = [0u8; 32];
        c[..2].copy_from_slice(&[0x01, 0xE1]); // c.bnez s0, +0
        c[2..4].copy_from_slice(&[0x01, 0xA0]); // c.j +0
        assert_eq!(run_end(&c, 0), 3);
        let mut tail = [0u8; 32];
        for w in 0..8 {
            tail[w * 4..w * 4 + 4].copy_from_slice(&nop32);
        }
        tail[28..30].copy_from_slice(&[0x01, 0x00]); // c.nop
        tail[30..32].copy_from_slice(&[0x13, 0x00]); // low half of a 32-bit instruction
        assert_eq!(run_end(&tail, 28), 31);
        assert_eq!(run_end(&[], 4), 31);
    }
}
