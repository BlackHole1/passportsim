//! Block-cache engine, its configuration and exits, and the `OpFuser` and `ExecTier` extension
//! points. CPU behavior follows the ESP32-C3 TRM chapter 1.
//!
//! [`Engine::run`] walks translated blocks until exactly `max_insns` retired or something ended
//! the run; interrupts are the caller's, between runs. `fast_op`, `fast_term` and `sp_check`
//! re-implement [`crate::exec::exec_op`] for register and mapped-memory kinds, and
//! `tests/fuzz.rs` holds the two equal against `ref_step`.

use std::collections::{BTreeMap, BTreeSet};

use crate::bus::{Bus, PF_CODE, PF_COLD, PF_MMIO, PF_R, PF_SLOW, PF_W, PF_X};
use crate::cache::{Block, BlockCache, NO_BLOCK, PAGE_SHIFT, PAGE_SIZE, SLOT_FALL, SLOT_TAKEN};
use crate::cost::InsnCosts;
use crate::decode::{decode, decode_at, insn_len};
use crate::exec::{Hart, Stepped, exec_op};
use crate::op::{
    self, F_TERM, F_WRITES_SP, K_ADD, K_ADDI, K_AND, K_ANDI, K_AUIPC, K_BEQ, K_BGE, K_BGEU, K_BLT,
    K_BLTU, K_BNE, K_CSRRC, K_CSRRCI, K_CSRRS, K_CSRRSI, K_CSRRW, K_CSRRWI, K_DIV, K_DIVU,
    K_EBREAK, K_ECALL, K_FALL, K_FENCEI, K_FETCH_FAULT, K_HOOK, K_ILLEGAL, K_JAL, K_JALR, K_LB,
    K_LBU, K_LH, K_LHU, K_LUI, K_LW, K_MRET, K_MUL, K_MULH, K_MULHSU, K_MULHU, K_NOP, K_OR, K_ORI,
    K_REM, K_REMU, K_SB, K_SH, K_SLL, K_SLLI, K_SLT, K_SLTI, K_SLTIU, K_SLTU, K_SRA, K_SRAI, K_SRL,
    K_SRLI, K_SUB, K_SW, K_WFI, K_XOR, K_XORI, Op, flags_for,
};
use crate::resume::ResumeToken;
use crate::spmon::SpSpill;
use crate::trap::{Trap, take_exception};

mod costed;

pub const DEFAULT_MAX_BLOCK_INSNS: u16 = 64;

/// Blocks that may retire nothing in a row before [`Engine::run`] reports [`Exit::Halted`]; only
/// a re-trapping trap produces one, so two already prove an infinite loop. UNVERIFIED: silicon
/// spins instead, so whether the machine should report `E_STUCK` is open.
pub const TRAP_LOOP_LIMIT: u32 = 8;

/// Flash windows on the instruction and data buses, 128 MMU entries of 64 KB each
/// (`specs/blocks/mmu.toml`).
const FLASH_IBUS_BASE: u32 = 0x4200_0000;
const FLASH_DBUS_BASE: u32 = 0x3C00_0000;
const FLASH_WINDOW_LEN: u32 = 128 * 64 * 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    Budget,
    /// A bus `OkStop` or a possibly deliverable interrupt; `Hart::spmon` was refreshed.
    Stop,
    Wfi,
    Hook {
        id: HookId,
    },
    /// A write to `x2` violated the stack monitor.
    SpSpill(SpSpill),
    Halted(HaltCause),
}

pub struct EngineCfg {
    /// Maximum instructions per block; 0 is read as 1.
    pub max_block_insns: u16,
    /// Whether an unknown CSR outside the custom ranges traps instead of reaching the bus.
    pub strict_csr: bool,
    /// Only one-for-one rewrites are applied.
    pub fuser: Option<&'static dyn OpFuser>,
}

impl Default for EngineCfg {
    fn default() -> Self {
        EngineCfg {
            max_block_insns: DEFAULT_MAX_BLOCK_INSNS,
            strict_csr: false,
            fuser: None,
        }
    }
}

/// Guest code whose fetches the bus charges by cache line. Blocks inside end at line boundaries
/// and never chain across lines, so [`Bus::fetch_enter`] sees every line change.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FetchWatch {
    pub base: u32,
    pub len: u32,
    /// log2 of the cache line, in bytes.
    pub line_shift: u32,
}

impl FetchWatch {
    #[inline(always)]
    pub fn contains(&self, pc: u32) -> bool {
        pc.wrapping_sub(self.base) < self.len
    }

    #[inline]
    pub fn may_chain(&self, from: u32, to: u32) -> bool {
        !self.contains(to) || from >> self.line_shift == to >> self.line_shift
    }
}

/// Rewrites a window of decoded ops into one fused op. `Send + Sync` keeps the machine `Send`.
pub trait OpFuser: Send + Sync {
    /// Returns the fused op and the number of ops it replaces.
    fn fuse(&self, window: &[Op]) -> Option<(Op, usize)>;
}

/// Optional executor for hot blocks, such as a JIT, consulted by `Engine::run`.
pub trait ExecTier {
    /// `None` has the block interpreted.
    fn run_hot<B: Bus>(
        &mut self,
        pc: u32,
        hart: &mut Hart,
        bus: &mut B,
        max_insns: u64,
    ) -> Option<Exit>;
}

#[derive(Copy, Clone, Debug, Default)]
pub struct NoFusion;

impl OpFuser for NoFusion {
    fn fuse(&self, _window: &[Op]) -> Option<(Op, usize)> {
        None
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct InterpreterOnly;

impl ExecTier for InterpreterOnly {
    #[inline(always)]
    fn run_hot<B: Bus>(
        &mut self,
        _pc: u32,
        _hart: &mut Hart,
        _bus: &mut B,
        _max_insns: u64,
    ) -> Option<Exit> {
        None
    }
}

/// `T` is generic so the hot path stays monomorphized over `Bus`.
pub struct Engine<T = InterpreterOnly> {
    cfg: EngineCfg,
    tier: T,
    cache: BlockCache,
    stats: EngineStats,
    resume: Option<ResumeToken>,
    hook_gen: u64,
    /// Pages that held a hook at the last sync, so removing a hook invalidates its page too.
    hooked_pages: BTreeSet<u32>,
    /// PC whose hook the next `run` runs past exactly once.
    skip_hook_at: Option<u32>,
    spill_pc: u32,
    fetch_watch: Option<FetchWatch>,
    /// `None` runs the uncosted loops, whose budget is in instructions.
    costs: Option<InsnCosts>,
}

impl Engine {
    pub fn new(cfg: EngineCfg) -> Self {
        Engine::with_tier(cfg, InterpreterOnly)
    }
}

impl<T: ExecTier> Engine<T> {
    pub fn with_tier(cfg: EngineCfg, tier: T) -> Self {
        Engine {
            cfg,
            tier,
            cache: BlockCache::new(),
            stats: EngineStats::default(),
            resume: None,
            hook_gen: 0,
            hooked_pages: BTreeSet::new(),
            skip_hook_at: None,
            spill_pc: 0,
            fetch_watch: None,
            costs: None,
        }
    }

    /// A change flushes the cache, since watched blocks are cut and chained by the window's rule.
    pub fn set_fetch_watch(&mut self, watch: Option<FetchWatch>) {
        if watch != self.fetch_watch {
            self.fetch_watch = watch;
            self.flush();
        }
    }

    pub fn fetch_watch(&self) -> Option<FetchWatch> {
        self.fetch_watch
    }

    /// With a table the budget is in `Hart::pos` units; a zero table is no table. A change drops
    /// the resume token, whose budget was counted in the other unit.
    pub fn set_costs(&mut self, costs: Option<InsnCosts>) {
        let costs = costs.filter(|c| !c.is_zero());
        if costs != self.costs {
            self.costs = costs;
            self.drop_resume();
        }
    }

    pub fn costs(&self) -> Option<InsnCosts> {
        self.costs
    }

    #[inline(always)]
    fn fetch_stalls<B: Bus>(&self, hart: &Hart, bus: &mut B) -> bool {
        match self.fetch_watch {
            Some(watch) if watch.contains(hart.pc) => fetch_enter(bus, hart.pos(), hart.pc),
            _ => false,
        }
    }

    /// PC of the instruction the last [`Exit::SpSpill`] reported, for ASSIST_DEBUG `SP_PC`; after
    /// a jump the hart PC is elsewhere. A fused op reports its first instruction.
    pub fn spill_pc(&self) -> u32 {
        self.spill_pc
    }

    pub fn cfg(&self) -> &EngineCfg {
        &self.cfg
    }

    #[inline]
    fn max_block_insns(&self) -> u32 {
        u32::from(self.cfg.max_block_insns.max(1))
    }

    /// Retires exactly `max_insns` instructions unless it stops earlier.
    pub fn run<B: Bus>(
        &mut self,
        hart: &mut Hart,
        bus: &mut B,
        hooks: &HookSet,
        max_insns: u64,
    ) -> Exit {
        self.sync_hooks(hooks);
        if max_insns == 0 {
            return Exit::Budget;
        }
        if let Some(costs) = self.costs {
            return self.run_costed_top(hart, bus, hooks, max_insns, costs);
        }
        let target = hart.insns.saturating_add(max_insns);

        // Run past the hook uncached, so the cached block keeps its `K_HOOK` terminator.
        if self.skip_hook_at == Some(hart.pc) {
            if self.fetch_stalls(hart, bus) {
                return Exit::Stop;
            }
            self.skip_hook_at = None;
            self.resume = None;
            if let Some(exit) = self.step_past_hook(hart, bus) {
                return exit;
            }
            if hart.insns >= target {
                return Exit::Budget;
            }
        }

        // The monitor state and the fetch watch are constant for a run, so the block loop is
        // monomorphised on both.
        if self.fetch_watch.is_some() {
            return self.run_watched(hart, bus, hooks, target);
        }
        if hart.spmon.armed() {
            self.run_blocks::<B, true, false>(hart, bus, hooks, target)
        } else {
            self.run_blocks::<B, false, false>(hart, bus, hooks, target)
        }
    }

    #[inline(never)]
    fn run_watched<B: Bus>(
        &mut self,
        hart: &mut Hart,
        bus: &mut B,
        hooks: &HookSet,
        target: u64,
    ) -> Exit {
        if hart.spmon.armed() {
            self.run_blocks::<B, true, true>(hart, bus, hooks, target)
        } else {
            self.run_blocks::<B, false, true>(hart, bus, hooks, target)
        }
    }

    /// Handles what [`run_chained`] hands back and a resumed first block; the split keeps few
    /// enough values live for a wasm engine's register allocator, which otherwise spills heavily.
    fn run_blocks<B: Bus, const ARMED: bool, const WATCH: bool>(
        &mut self,
        hart: &mut Hart,
        bus: &mut B,
        hooks: &HookSet,
        target: u64,
    ) -> Exit {
        let strict = self.cfg.strict_csr;
        // Fetch before a resume token or lookup picks the block, so the bus sees neither.
        if WATCH && self.fetch_stalls(hart, bus) {
            return Exit::Stop;
        }
        let (mut id, mut op_index) = self.enter(hart, bus, hooks);
        let mut idle_blocks = 0u32;
        let mut insns_at_entry = hart.insns;
        // Locals, since in `self.stats` they would be reloaded around every block.
        let mut block_execs = 0u64;
        let mut chain_hits = 0u64;
        // Carried in a local to keep a store-to-load forward off every block's critical path;
        // paths other than a whole block resynchronize from `Hart::insns`.
        let mut remaining = target - hart.insns;
        let exit = 'run: loop {
            debug_assert_eq!(
                remaining,
                target.saturating_sub(hart.insns),
                "the carried budget drifted from Hart::insns"
            );
            debug_assert!(remaining > 0, "a block is entered with budget left");
            let outcome = if op_index == 0 {
                let ran = run_chained::<B, T, ARMED>(
                    &self.cache,
                    &mut self.tier,
                    hart,
                    bus,
                    id,
                    remaining,
                    strict,
                );
                id = ran.id;
                remaining = ran.remaining;
                chain_hits += ran.chains;
                block_execs += ran.chains + u64::from(ran.end.ran_block());
                match ran.end {
                    Chained::Exit(exit) => break 'run exit,
                    Chained::Budget => break 'run Exit::Budget,
                    Chained::Cut => Outcome::Cut,
                    Chained::Ended(index, stepped) => Outcome::Ended(index, stepped),
                    Chained::Spilled(index, spill) => {
                        let block = self.cache.block(id);
                        let (plain, _) = self.cache.block_run(block, 0);
                        let (index, stepped) =
                            spilled(hart, block.pc, plain, 0, &plain[index], spill);
                        Outcome::Ended(index, stepped)
                    }
                    Chained::Unchained(slot) => Outcome::Unchained(slot),
                }
            } else {
                // A resumed block; the tier is consulted for whole blocks only.
                let block = self.cache.block(id);
                let left = u64::from(block.n_insns - op_index);
                if left > remaining {
                    Outcome::Cut
                } else {
                    block_execs += 1;
                    match run_resumed::<B, ARMED>(&self.cache, hart, bus, id, op_index, strict) {
                        Err((index, stepped)) => Outcome::Ended(index, stepped),
                        Ok(slot) => {
                            op_index = 0;
                            remaining -= left;
                            if remaining == 0 {
                                break 'run Exit::Budget;
                            }
                            match chain_of(&self.cache, self.cache.block(id), slot, hart.pc) {
                                Some(next) => {
                                    chain_hits += 1;
                                    id = next;
                                    continue 'run;
                                }
                                None => Outcome::Unchained(slot),
                            }
                        }
                    }
                }
            };
            match outcome {
                Outcome::Cut => {
                    self.stats.partial_blocks += 1;
                    // `remaining < left <= n_insns`, so the op the prefix stops before exists.
                    let stopped = run_partial::<B, ARMED>(
                        &self.cache,
                        hart,
                        bus,
                        id,
                        op_index,
                        remaining as u32,
                        strict,
                    );
                    match stopped {
                        None => {
                            self.set_resume(id, op_index + remaining as u32, hart.pc);
                            break 'run Exit::Budget;
                        }
                        Some((index, stepped)) => match self.leave(hart, bus, id, index, stepped) {
                            Left::Exit(exit) => break 'run exit,
                            Left::Continue => {
                                remaining = target - hart.insns;
                                if remaining == 0 {
                                    break 'run Exit::Budget;
                                }
                                if WATCH && self.fetch_stalls(hart, bus) {
                                    break 'run Exit::Stop;
                                }
                                id = self.lookup(hart.pc, bus, hooks);
                                op_index = 0;
                            }
                        },
                    }
                }
                Outcome::Unchained(slot) => {
                    // A lookup may translate and so flush the cache: link only within one
                    // generation.
                    if WATCH && self.fetch_stalls(hart, bus) {
                        break 'run Exit::Stop;
                    }
                    let generation = self.cache.generation();
                    let next = self.lookup(hart.pc, bus, hooks);
                    if self.cache.generation() == generation
                        && (!WATCH
                            || self
                                .fetch_watch
                                .is_none_or(|w| w.may_chain(self.cache.block(id).pc, hart.pc)))
                    {
                        self.cache.link(id, slot, next);
                    }
                    id = next;
                }
                Outcome::Ended(index, stepped) => {
                    // None of these can chain: the PC they leave is not the slot's.
                    op_index = 0;
                    match self.leave(hart, bus, id, index, stepped) {
                        Left::Exit(exit) => break 'run exit,
                        Left::Continue => {
                            // Only a trap can leave a block having retired nothing.
                            idle_blocks = if hart.insns == insns_at_entry {
                                idle_blocks + 1
                            } else {
                                0
                            };
                            insns_at_entry = hart.insns;
                            if idle_blocks > TRAP_LOOP_LIMIT {
                                break 'run Exit::Halted(HaltCause::NoProgress { pc: hart.pc });
                            }
                            remaining = target - hart.insns;
                            if remaining == 0 {
                                break 'run Exit::Budget;
                            }
                            if WATCH && self.fetch_stalls(hart, bus) {
                                break 'run Exit::Stop;
                            }
                            id = self.lookup(hart.pc, bus, hooks);
                        }
                    }
                }
            }
        };
        self.stats.block_execs += block_execs;
        self.stats.chain_hits += chain_hits;
        exit
    }

    pub fn step<B: Bus>(&mut self, hart: &mut Hart, bus: &mut B, hooks: &HookSet) -> Exit {
        self.run(hart, bus, hooks, 1)
    }

    /// After an MMU remap, a RAM code write or a hook change.
    pub fn invalidate_vrange(&mut self, vaddr: u32, len: u32) {
        let dropped = self.cache.invalidate_vrange(vaddr, len);
        self.stats.blocks_invalidated += dropped as u64;
        if dropped > 0 {
            self.drop_resume();
        }
    }

    /// Invalidates both flash windows after a flash program or erase: the engine cannot invert the
    /// MMU, so this is a sound superset of [`Engine::invalidate_vrange`].
    pub fn invalidate_flash_page(&mut self, _phys_page: u32) {
        self.stats.flash_invalidations += 1;
        let a = self
            .cache
            .invalidate_translated_in(FLASH_IBUS_BASE, FLASH_WINDOW_LEN);
        let b = self
            .cache
            .invalidate_translated_in(FLASH_DBUS_BASE, FLASH_WINDOW_LEN);
        self.stats.blocks_invalidated += (a + b) as u64;
        self.drop_resume();
    }

    pub fn flush(&mut self) {
        self.cache.flush();
        self.stats.flushes += 1;
        self.drop_resume();
    }

    pub fn stats(&self) -> EngineStats {
        EngineStats {
            blocks_live: self.cache.block_count() as u64,
            ops_live: self.cache.op_count() as u64,
            ..self.stats
        }
    }

    /// The next [`Engine::run`] executes the instruction at hooked `pc` once without its hook
    /// exit. UNVERIFIED: no spec fixes this mechanism; it is the engine's choice.
    pub fn continue_past_hook(&mut self, pc: u32) {
        self.skip_hook_at = Some(pc);
    }

    /// The SoC sets `PF_CODE` on such pages so stores into them take the slow path.
    pub fn page_is_translated(&self, vaddr: u32) -> bool {
        self.cache.page_is_translated(vaddr >> PAGE_SHIFT)
    }

    /// In page order, for a SoC rebuilding page permissions.
    pub fn translated_pages(&self) -> impl Iterator<Item = u32> + '_ {
        self.cache.translated_pages()
    }

    fn drop_resume(&mut self) {
        self.resume = None;
    }

    fn set_resume(&mut self, id: u32, op_index: u32, pc: u32) {
        let block = self.cache.block(id);
        self.resume = (op_index < block.n_ops).then_some(ResumeToken {
            block: id,
            op_index,
            generation: self.cache.generation(),
            block_pc: block.pc,
            pc,
        });
    }

    fn enter<B: Bus>(&mut self, hart: &mut Hart, bus: &mut B, hooks: &HookSet) -> (u32, u32) {
        if let Some(token) = self.resume.take()
            && token.generation == self.cache.generation()
            && (token.block as usize) < self.cache.block_count()
            && token.matches(
                self.cache.generation(),
                self.cache.block(token.block).pc,
                hart.pc,
            )
        {
            self.stats.resumes += 1;
            return (token.block, token.op_index);
        }
        (self.lookup(hart.pc, bus, hooks), 0)
    }

    #[inline(always)]
    fn lookup<B: Bus>(&mut self, pc: u32, bus: &mut B, hooks: &HookSet) -> u32 {
        match self.cache.jump_cache_hit(pc) {
            Some(id) => {
                self.stats.jump_cache_hits += 1;
                id
            }
            None => self.lookup_slow(pc, bus, hooks),
        }
    }

    #[inline(never)]
    fn lookup_slow<B: Bus>(&mut self, pc: u32, bus: &mut B, hooks: &HookSet) -> u32 {
        if let Some(id) = self.cache.map_hit(pc) {
            self.stats.map_hits += 1;
            return id;
        }
        self.translate(pc, bus, hooks)
    }

    /// A faulting fetch becomes a [`K_FETCH_FAULT`], so the trap is raised when the block runs.
    fn translate<B: Bus>(&mut self, pc0: u32, bus: &mut B, hooks: &HookSet) -> u32 {
        if self.cache.over_watermark() {
            self.cache.flush();
            self.stats.flushes += 1;
            self.resume = None;
        }
        self.stats.blocks_built += 1;
        let start = self.cache.build_start();
        let max = self.max_block_insns();
        let page = pc0 >> PAGE_SHIFT;
        let hooked_page = hooks.page_has_hooks(pc0);
        // A watched block ends at its cache line as every block ends at its page.
        let line = self
            .fetch_watch
            .filter(|w| w.contains(pc0))
            .map(|w| w.line_shift);
        let mut pc = pc0;
        let mut n_insns = 0u32;
        // May reach into the next page, which then also indexes the block.
        let mut last_byte = pc0;
        loop {
            let pc_off = pc.wrapping_sub(pc0) as u16;
            if hooked_page && let Some(hook) = hooks.get(pc) {
                self.cache.push_op(synthetic(K_HOOK, pc_off, hook.0));
                break;
            }
            let op = match fetch_op(bus, pc) {
                Ok(mut op) => {
                    op.pc_off = pc_off;
                    op
                }
                Err(trap) => {
                    // `tval` is `pc + 2` for the second halfword of a straddling instruction.
                    last_byte = last_byte.max(trap.tval);
                    let mut fault = synthetic(K_FETCH_FAULT, pc_off, trap.tval);
                    fault.imm = trap.cause as i32;
                    self.cache.push_op(fault);
                    break;
                }
            };
            let len = u32::from(op.len);
            let terminator = op.flags & F_TERM != 0;
            self.cache.push_op(op);
            n_insns += 1;
            last_byte = pc.wrapping_add(len.wrapping_sub(1));
            self.offer_to_fuser(start);
            if terminator {
                break;
            }
            let next = pc.wrapping_add(len);
            if n_insns >= max
                || next >> PAGE_SHIFT != page
                || line.is_some_and(|l| next >> l != pc0 >> l)
            {
                self.cache
                    .push_op(synthetic(K_FALL, next.wrapping_sub(pc0) as u16, next));
                break;
            }
            pc = next;
        }
        self.cache.finish(pc0, start, n_insns, last_byte)
    }

    /// Only a one-for-one rewrite is applied, because [`run_ops`] and [`run_partial`] count
    /// instructions by op index.
    fn offer_to_fuser(&mut self, start: u32) {
        let Some(fuser) = self.cfg.fuser else {
            return;
        };
        let window = self.cache.building(start);
        let Some((fused, replaced)) = fuser.fuse(window) else {
            return;
        };
        let last = window.last().expect("an op was just pushed");
        let same_shape = fused.flags & F_TERM == last.flags & F_TERM
            && fused.pc_off == last.pc_off
            && fused.len == last.len;
        debug_assert!(
            replaced == 1 && same_shape,
            "a fusion of {replaced} ops was offered: this engine takes one-for-one rewrites \
             only, because a fused op standing for several instructions needs the per-block \
             instruction index for exact budgets, which this engine does not keep \
             (`crate::fuse`)"
        );
        if replaced != 1 || !same_shape {
            self.stats.fusions_rejected += 1;
            return;
        }
        self.cache.replace_last(fused);
        self.stats.fusions += 1;
    }

    /// Handles op `index` that ended block `id` early.
    fn leave<B: Bus>(
        &mut self,
        hart: &mut Hart,
        bus: &mut B,
        id: u32,
        index: usize,
        stepped: Stepped,
    ) -> Left {
        match stepped {
            // Refreshing `Hart::spmon` here makes bounds written by `rtos_int_enter` apply from
            // the next instruction whatever the block size.
            Stepped::RetiredStop => {
                hart.spmon = bus.sp_monitor();
                self.set_resume(id, index as u32 + 1, hart.pc);
                Left::Exit(Exit::Stop)
            }
            Stepped::Spilled(spill) => {
                hart.spmon = bus.sp_monitor();
                let block = self.cache.block(id);
                self.spill_pc = block
                    .pc
                    .wrapping_add(u32::from(self.cache.block_ops(block)[index].pc_off));
                self.set_resume(id, index as u32 + 1, hart.pc);
                Left::Exit(Exit::SpSpill(spill))
            }
            // A trap is not an exit: the run continues at the vector.
            Stepped::Trapped(_) => Left::Continue,
            Stepped::Waiting => Left::Exit(Exit::Wfi),
            Stepped::Flushed => {
                self.flush();
                Left::Continue
            }
            Stepped::Hooked(hook) => Left::Exit(Exit::Hook { id: HookId(hook) }),
            Stepped::Fell | Stepped::Retired => Left::Continue,
        }
    }

    /// Invalidates pages that hold or held a hook whenever `HookSet::generation` changed.
    fn sync_hooks(&mut self, hooks: &HookSet) {
        if hooks.generation() == self.hook_gen {
            return;
        }
        self.hook_gen = hooks.generation();
        let now: BTreeSet<u32> = hooks.iter().map(|(pc, _)| pc >> PAGE_SHIFT).collect();
        let mut dropped = 0usize;
        for vpn in self.hooked_pages.union(&now) {
            dropped += self.cache.invalidate_page(*vpn);
        }
        self.stats.blocks_invalidated += dropped as u64;
        self.hooked_pages = now;
        self.resume = None;
        self.skip_hook_at = None;
    }

    /// Executes the instruction at `Hart::pc` uncached, ignoring its hook; `None` carries on.
    fn step_past_hook<B: Bus>(&mut self, hart: &mut Hart, bus: &mut B) -> Option<Exit> {
        let pc = hart.pc;
        let op = match fetch_op(bus, pc) {
            Ok(op) => op,
            Err(trap) => {
                take_exception(hart, pc, trap);
                return None;
            }
        };
        match exec_op(hart, bus, pc, &op, self.cfg.strict_csr) {
            Stepped::Retired | Stepped::Trapped(_) | Stepped::Fell => None,
            Stepped::RetiredStop => {
                hart.spmon = bus.sp_monitor();
                Some(Exit::Stop)
            }
            Stepped::Spilled(spill) => {
                hart.spmon = bus.sp_monitor();
                self.spill_pc = pc;
                Some(Exit::SpSpill(spill))
            }
            Stepped::Waiting => Some(Exit::Wfi),
            Stepped::Flushed => {
                self.flush();
                None
            }
            Stepped::Hooked(hook) => Some(Exit::Hook { id: HookId(hook) }),
        }
    }
}

enum Left {
    Exit(Exit),
    Continue,
}

enum Outcome {
    /// The budget ends inside the block.
    Cut,
    /// The op at this index ended the block early.
    Ended(usize, Stepped),
    /// The block ran to its end and this chain slot names no valid block at `Hart::pc`.
    Unchained(usize),
}

enum Chained {
    /// The tier ran the block.
    Exit(Exit),
    Budget,
    /// The budget ends inside the block, which did not run.
    Cut,
    Ended(usize, Stepped),
    /// Neither the spilling op nor the ops before it are counted yet.
    Spilled(usize, SpSpill),
    Unchained(usize),
}

impl Chained {
    /// Whether the block was entered, as [`EngineStats::block_execs`] counts.
    #[inline(always)]
    fn ran_block(&self) -> bool {
        !matches!(self, Chained::Exit(_) | Chained::Cut)
    }
}

struct Ran {
    id: u32,
    remaining: u64,
    chains: u64,
    end: Chained,
}

/// Runs whole blocks through chain slots on the fast paths; at the first declined op
/// [`run_rest`] takes over. Never inlined, so a browser allocates registers for this loop alone.
#[inline(never)]
fn run_chained<B: Bus, T: ExecTier, const ARMED: bool>(
    cache: &BlockCache,
    tier: &mut T,
    hart: &mut Hart,
    bus: &mut B,
    mut id: u32,
    mut remaining: u64,
    strict_csr: bool,
) -> Ran {
    let mut chains = 0u64;
    let end = 'chain: loop {
        // SAFETY: `id` is the caller's, which came from a lookup of this cache, or a chain slot
        // read below; the cache is borrowed for the whole loop, so it has not been flushed since.
        let block = unsafe { cache.block_unchecked(id) };
        debug_assert_eq!(block.pc & 1, 0, "a block starts at an even pc");
        debug_assert!(remaining > 0, "a block is entered with budget left");
        if let Some(exit) = tier.run_hot(block.pc, hart, bus, remaining) {
            break Chained::Exit(exit);
        }
        let left = u64::from(block.n_insns);
        if left > remaining {
            break Chained::Cut;
        }
        // SAFETY: `block` was just read from this cache, under the same borrow.
        let (plain, term) = unsafe { cache.block_run_unchecked(block) };
        let declined = 'ops: {
            for op in plain {
                match fast_op::<B, ARMED>(hart, bus, op) {
                    Fast::Retired => {}
                    Fast::Miss => break 'ops Some(index_in(plain, op)),
                    Fast::Spilled(spill) => {
                        break 'chain Chained::Spilled(index_in(plain, op), spill);
                    }
                }
            }
            None
        };
        let fast = match declined {
            None => fast_term::<ARMED>(hart, term),
            Some(_) => None,
        };
        let slot = match fast {
            Some((slot, retired)) => {
                debug_assert_eq!(left, plain.len() as u64 + retired);
                hart.insns += left;
                hart.stores += u64::from(block.n_stores);
                slot
            }
            None => match run_rest::<B, ARMED>(
                cache,
                hart,
                bus,
                id,
                declined.unwrap_or(plain.len()),
                strict_csr,
            ) {
                Ok(slot) => slot,
                Err((index, stepped)) => break Chained::Ended(index, stepped),
            },
        };
        remaining -= left;
        // Checked before the lookup, so a run ending on a block boundary translates nothing extra.
        if remaining == 0 {
            break Chained::Budget;
        }
        let linked = if slot == SLOT_TAKEN {
            block.next[SLOT_TAKEN]
        } else {
            block.next[SLOT_FALL]
        };
        // SAFETY: a chain slot of a block of this cache holds `NO_BLOCK`, which is tested first,
        // or an id `BlockCache::link` checked was in range, and nothing has flushed the cache.
        if linked != NO_BLOCK && unsafe { cache.block_unchecked(linked) }.pc == hart.pc {
            chains += 1;
            id = linked;
        } else {
            break Chained::Unchained(slot);
        }
    };
    Ran {
        id,
        remaining,
        chains,
        end,
    }
}

/// The rest of block `id` from the declined op `index`. Out of line, so the op loop of
/// [`run_chained`] calls nothing.
#[inline(never)]
fn run_rest<B: Bus, const ARMED: bool>(
    cache: &BlockCache,
    hart: &mut Hart,
    bus: &mut B,
    id: u32,
    index: usize,
    strict_csr: bool,
) -> Result<usize, (usize, Stepped)> {
    let block = cache.block(id);
    let (plain, term) = cache.block_run(block, 0);
    let from = match plain.get(index) {
        Some(op) => fall_back(hart, bus, block.pc, plain, 0, op, strict_csr)?,
        None => {
            hart.insns += index as u64;
            hart.stores += stores_in(plain);
            index
        }
    };
    let rest = &plain[from..];
    let pending = run_ops::<B, ARMED>(hart, bus, block.pc, rest, stores_in(rest), strict_csr)
        .map_err(|(index, stepped)| (index + from, stepped))?;
    run_term::<B, ARMED>(hart, bus, block.pc, term, pending, strict_csr)
        .map_err(|stepped| (plain.len(), stepped))
}

/// Runs block `id` from a resume token's `op_index`; at most once per run, so out of line.
#[cold]
#[inline(never)]
fn run_resumed<B: Bus, const ARMED: bool>(
    cache: &BlockCache,
    hart: &mut Hart,
    bus: &mut B,
    id: u32,
    op_index: u32,
    strict_csr: bool,
) -> Result<usize, (usize, Stepped)> {
    let block = cache.block(id);
    let from = op_index as usize;
    let (plain, term) = cache.block_run(block, op_index);
    // The block's store count covers all its plain ops; this runs only the tail.
    let pending = run_ops::<B, ARMED>(hart, bus, block.pc, plain, stores_in(plain), strict_csr)
        .map_err(|(index, stepped)| (index + from, stepped))?;
    run_term::<B, ARMED>(hart, bus, block.pc, term, pending, strict_csr)
        .map_err(|stepped| (from + plain.len(), stepped))
}

/// Runs the terminator after the plain ops, `pending` of them not yet in `Hart::insns`; `Ok`
/// with the chain slot it selected.
#[inline(always)]
fn run_term<B: Bus, const ARMED: bool>(
    hart: &mut Hart,
    bus: &mut B,
    block_pc: u32,
    term: &Op,
    pending: u64,
    strict_csr: bool,
) -> Result<usize, Stepped> {
    match fast_term::<ARMED>(hart, term) {
        // One write of `Hart::insns` for the whole block.
        Some((slot, retired)) => {
            hart.insns += pending + retired;
            Ok(slot)
        }
        None => {
            hart.insns += pending;
            match exec_op(hart, bus, block_pc, term, strict_csr) {
                Stepped::Retired => Ok(slot_of(hart, term)),
                other => Err(other),
            }
        }
    }
}

/// The block chain slot `slot` names, when it starts at `pc`. Constant indices on each side of
/// the test make the read a plain load where `next[slot]` was a bounds check.
#[inline(always)]
fn chain_of(cache: &BlockCache, block: &Block, slot: usize, pc: u32) -> Option<u32> {
    let linked = if slot == SLOT_TAKEN {
        block.next[SLOT_TAKEN]
    } else {
        block.next[SLOT_FALL]
    };
    (linked != NO_BLOCK && cache.block(linked).pc == pc).then_some(linked)
}

/// Out of line so the block loop's layout does not change when a [`FetchWatch`] is installed.
#[inline(never)]
fn fetch_enter<B: Bus>(bus: &mut B, pos: u64, pc: u32) -> bool {
    bus.fetch_enter(pos, pc)
}

fn synthetic(kind: u8, pc_off: u16, imm2: u32) -> Op {
    Op {
        kind,
        rd: 0,
        rs1: 0,
        rs2: 0,
        imm: 0,
        imm2,
        len: 0,
        flags: flags_for(kind, 0),
        pc_off,
    }
}

/// A 32-bit instruction straddling a code page fetches its second halfword separately, which may
/// fault where the first did not.
fn fetch_op<B: Bus>(bus: &mut B, pc: u32) -> Result<Op, Trap> {
    let mut head = [0u8; 4];
    let got = {
        let page = bus.fetch_code(pc)?;
        let got = page.bytes.len().min(head.len());
        head[..got].copy_from_slice(&page.bytes[..got]);
        got
    };
    if got < 2 {
        return Err(Trap::instruction_access_fault(pc));
    }
    if let Some(op) = decode_at(&head[..got], pc) {
        return Ok(op);
    }
    debug_assert!(got < 4, "decode_at refused {got} bytes");
    debug_assert_eq!(insn_len(u16::from_le_bytes([head[0], head[1]])), 4);
    let next = pc.wrapping_add(2);
    let page = bus.fetch_code(next)?;
    let &[b2, b3, ..] = page.bytes else {
        return Err(Trap::instruction_access_fault(next));
    };
    head[2] = b2;
    head[3] = b3;
    Ok(decode(u32::from_le_bytes(head), pc))
}

/// Runs plain `ops`, `stores` of them stores; `Ok(pending)` still to add to `Hart::insns`, or the
/// op that ended the run with both counters exact.
///
/// The loop touches neither counter nor `Hart::pc`: a store counter in the loop defeats
/// JavaScriptCore's B3 tail duplication. `Hart::stores` is exact before every fall-back, where the
/// MMIO slow path reads it.
#[inline(always)]
fn run_ops<B: Bus, const ARMED: bool>(
    hart: &mut Hart,
    bus: &mut B,
    block_pc: u32,
    ops: &[Op],
    stores: u64,
    strict_csr: bool,
) -> Result<u64, (usize, Stepped)> {
    debug_assert_eq!(
        stores,
        stores_in(ops),
        "the caller's store count is not the ops'"
    );
    // Ops already added to the counters, which only the fall-back does.
    let mut done = 0usize;
    // No `enumerate`: the index is derived only on the out-of-line stop paths.
    for op in ops.iter() {
        match fast_op::<B, ARMED>(hart, bus, op) {
            Fast::Retired => continue,
            Fast::Spilled(spill) => return Err(spilled(hart, block_pc, ops, done, op, spill)),
            Fast::Miss => match fall_back(hart, bus, block_pc, ops, done, op, strict_csr) {
                Ok(next) => done = next,
                Err(stopped) => return Err(stopped),
            },
        }
    }
    hart.stores += if done == 0 {
        stores
    } else {
        stores_in(&ops[done..])
    };
    Ok((ops.len() - done) as u64)
}

/// `op` must be an element of `ops`.
fn index_in(ops: &[Op], op: &Op) -> usize {
    (op as *const Op as usize - ops.as_ptr() as usize) / size_of::<Op>()
}

/// Brings both counters up to and including the spilling `op`, and `Hart::pc` past it.
#[cold]
#[inline(never)]
fn spilled(
    hart: &mut Hart,
    block_pc: u32,
    ops: &[Op],
    done: usize,
    op: &Op,
    spill: SpSpill,
) -> (usize, Stepped) {
    let index = index_in(ops, op);
    hart.insns += (index + 1 - done) as u64;
    hart.stores += stores_in(&ops[done..=index]);
    hart.pc = block_pc
        .wrapping_add(u32::from(op.pc_off))
        .wrapping_add(u32::from(op.len));
    (index, Stepped::Spilled(spill))
}

/// Brings both counters up to `op` and runs it through `exec_op`, which counts it itself.
#[cold]
#[inline(never)]
fn fall_back<B: Bus>(
    hart: &mut Hart,
    bus: &mut B,
    block_pc: u32,
    ops: &[Op],
    done: usize,
    op: &Op,
    strict_csr: bool,
) -> Result<usize, (usize, Stepped)> {
    let index = index_in(ops, op);
    hart.insns += (index - done) as u64;
    hart.stores += stores_in(&ops[done..index]);
    match exec_op(hart, bus, block_pc, op, strict_csr) {
        Stepped::Retired => Ok(index + 1),
        other => Err((index, other)),
    }
}

fn stores_in(ops: &[Op]) -> u64 {
    ops.iter().filter(|op| op::is_store(op.kind)).count() as u64
}

/// Masking [`Op::kind`] with this drops the range check in front of the jump table.
const KIND_SPAN: u8 = 64;

const _: () = assert!(
    op::K_COUNT <= KIND_SPAN,
    "crate::op has more kinds than the engine's dispatch mask spans: raise KIND_SPAN to the next \
     power of two (engine::fast_op, engine::fast_term)"
);

enum Fast {
    /// The caller counts it; `Hart::pc` and `Hart::insns` are untouched.
    Retired,
    Spilled(SpSpill),
    /// The caller runs the op through [`crate::exec::exec_op`].
    Miss,
}

/// The dispatch over kinds that touch only registers and mapped memory.
#[allow(clippy::manual_range_patterns, reason = "see the 61 | 62 | 63 arm")]
#[inline(always)]
fn fast_op<B: Bus, const ARMED: bool>(hart: &mut Hart, bus: &mut B, op: &Op) -> Fast {
    let a = hart.x[(op.rs1 & 31) as usize];
    let b = hart.x[(op.rs2 & 31) as usize];
    let imm = op.imm as u32;
    // One `br_table` in wasm, with load widths constant so a load pays one dispatch. Each arm
    // writes its register itself: a shared join block is too big for JavaScriptCore's B3 tail
    // duplication. The write is unguarded, since x0-only writes decode to `K_NOP`.
    macro_rules! set {
        ($value:expr) => {{
            debug_assert_ne!(
                op.rd & 31,
                0,
                "a computational op wrote x0 (crate::op makes that K_NOP)"
            );
            hart.x[(op.rd & 31) as usize] = $value;
            sp_check::<ARMED>(hart, op)
        }};
    }
    match op.kind & (KIND_SPAN - 1) {
        K_NOP => Fast::Retired,
        K_LUI | K_AUIPC => set!(imm),
        K_ADDI => set!(a.wrapping_add(imm)),
        K_SLTI => set!(u32::from((a as i32) < op.imm)),
        K_SLTIU => set!(u32::from(a < imm)),
        K_XORI => set!(a ^ imm),
        K_ORI => set!(a | imm),
        K_ANDI => set!(a & imm),
        K_SLLI => set!(a << (imm & 31)),
        K_SRLI => set!(a >> (imm & 31)),
        K_SRAI => set!(((a as i32) >> (imm & 31)) as u32),
        K_ADD => set!(a.wrapping_add(b)),
        K_SUB => set!(a.wrapping_sub(b)),
        K_SLL => set!(a << (b & 31)),
        K_SLT => set!(u32::from((a as i32) < (b as i32))),
        K_SLTU => set!(u32::from(a < b)),
        K_XOR => set!(a ^ b),
        K_SRL => set!(a >> (b & 31)),
        K_SRA => set!(((a as i32) >> (b & 31)) as u32),
        K_OR => set!(a | b),
        K_AND => set!(a & b),
        K_MUL => set!(a.wrapping_mul(b)),
        K_MULH => set!(((i64::from(a as i32) * i64::from(b as i32)) >> 32) as u32),
        K_MULHSU => set!(((i64::from(a as i32) * i64::from(b)) >> 32) as u32),
        K_MULHU => set!(((u64::from(a) * u64::from(b)) >> 32) as u32),
        K_DIV => set!(match (a as i32, b as i32) {
            (_, 0) => u32::MAX,
            (i32::MIN, -1) => i32::MIN as u32,
            (x, y) => (x / y) as u32,
        }),
        K_DIVU => set!(match b {
            0 => u32::MAX,
            y => a / y,
        }),
        K_REM => set!(match (a as i32, b as i32) {
            (x, 0) => x as u32,
            (i32::MIN, -1) => 0,
            (x, y) => (x % y) as u32,
        }),
        K_REMU => set!(match b {
            0 => a,
            y => a % y,
        }),
        K_LB => match fast_load::<1>(bus, a.wrapping_add(imm)) {
            // A load keeps its kind when `rd` is 0, so it needs `write_rd`.
            Some(raw) => {
                write_rd(hart, op.rd, raw as u8 as i8 as i32 as u32);
                sp_check::<ARMED>(hart, op)
            }
            None => Fast::Miss,
        },
        K_LH => match fast_load::<2>(bus, a.wrapping_add(imm)) {
            Some(raw) => {
                write_rd(hart, op.rd, raw as u16 as i16 as i32 as u32);
                sp_check::<ARMED>(hart, op)
            }
            None => Fast::Miss,
        },
        K_LW => match fast_load::<4>(bus, a.wrapping_add(imm)) {
            Some(raw) => {
                write_rd(hart, op.rd, raw);
                sp_check::<ARMED>(hart, op)
            }
            None => Fast::Miss,
        },
        K_LBU => match fast_load::<1>(bus, a.wrapping_add(imm)) {
            Some(raw) => {
                write_rd(hart, op.rd, raw);
                sp_check::<ARMED>(hart, op)
            }
            None => Fast::Miss,
        },
        K_LHU => match fast_load::<2>(bus, a.wrapping_add(imm)) {
            Some(raw) => {
                write_rd(hart, op.rd, raw);
                sp_check::<ARMED>(hart, op)
            }
            None => Fast::Miss,
        },
        // Counted from the block's static store count.
        K_SB => {
            if fast_store::<1>(bus, a.wrapping_add(imm), b) {
                Fast::Retired
            } else {
                Fast::Miss
            }
        }
        K_SH => {
            if fast_store::<2>(bus, a.wrapping_add(imm), b) {
                Fast::Retired
            } else {
                Fast::Miss
            }
        }
        K_SW => {
            if fast_store::<4>(bus, a.wrapping_add(imm), b) {
                Fast::Retired
            } else {
                Fast::Miss
            }
        }
        // Every other kind, written out so the dispatch is a bare jump table.
        K_JAL | K_JALR | K_BEQ | K_BNE | K_BLT | K_BGE | K_BLTU | K_BGEU | K_CSRRW | K_CSRRS
        | K_CSRRC | K_CSRRWI | K_CSRRSI | K_CSRRCI | K_ECALL | K_EBREAK | K_MRET | K_WFI
        | K_FENCEI | K_ILLEGAL | K_HOOK | K_FALL | K_FETCH_FAULT => Fast::Miss,
        // Unreachable kinds that complete the span. Three literals, not the `61..=63` clippy asks
        // for: a range pattern puts the range check back in front of the jump table.
        61 | 62 | 63 => Fast::Miss,
        _ => Fast::Miss,
    }
}

#[inline(always)]
fn sp_check<const ARMED: bool>(hart: &Hart, op: &Op) -> Fast {
    if ARMED
        && op.flags & F_WRITES_SP != 0
        && let Some(spill) = hart.spmon.check(hart.x[2])
    {
        return Fast::Spilled(spill);
    }
    Fast::Retired
}

#[inline(always)]
fn slot_of(hart: &Hart, term: &Op) -> usize {
    if op::is_branch(term.kind) && hart.pc != term.imm as u32 {
        SLOT_FALL
    } else {
        SLOT_TAKEN
    }
}

/// The dispatch over terminators that only move the PC; returns the chain slot and the retired
/// count. With `ARMED`, a jump linking into `x2` goes to `exec_op`.
#[inline(always)]
fn fast_term<const ARMED: bool>(hart: &mut Hart, op: &Op) -> Option<(usize, u64)> {
    if ARMED && op.flags & F_WRITES_SP != 0 {
        return None;
    }
    let a = hart.x[(op.rs1 & 31) as usize];
    let b = hart.x[(op.rs2 & 31) as usize];
    // One arm per branch kind: a shared `branch_taken` compiled into a second dispatch.
    let slot = match op.kind & (KIND_SPAN - 1) {
        // `K_FALL` retires nothing.
        K_FALL => {
            hart.pc = op.imm2;
            return Some((SLOT_TAKEN, 0));
        }
        K_JAL => {
            write_rd(hart, op.rd, op.imm2);
            hart.pc = op.imm as u32;
            SLOT_TAKEN
        }
        K_JALR => {
            // Read the target before the link write, since `rd` may be `rs1`.
            let target = a.wrapping_add(op.imm as u32) & !1;
            write_rd(hart, op.rd, op.imm2);
            hart.pc = target;
            SLOT_TAKEN
        }
        K_BEQ => branch(hart, op, a == b),
        K_BNE => branch(hart, op, a != b),
        K_BLT => branch(hart, op, (a as i32) < (b as i32)),
        K_BGE => branch(hart, op, (a as i32) >= (b as i32)),
        K_BLTU => branch(hart, op, a < b),
        K_BGEU => branch(hart, op, a >= b),
        _ => return None,
    };
    Some((slot, 1))
}

#[inline(always)]
fn branch(hart: &mut Hart, op: &Op, taken: bool) -> usize {
    if taken {
        hart.pc = op.imm as u32;
        SLOT_TAKEN
    } else {
        hart.pc = op.imm2;
        SLOT_FALL
    }
}

/// Restores x0 rather than testing `rd`: two stores cost less than a mispredictable branch.
#[inline(always)]
fn write_rd(hart: &mut Hart, rd: u8, value: u32) {
    hart.x[(rd & 31) as usize] = value;
    hart.x[0] = 0;
}

/// The exact-budget prefix: runs `count` ops of block `id` from `op_index`. Cold, since it runs
/// at most once per `run`.
#[cold]
#[inline(never)]
fn run_partial<B: Bus, const ARMED: bool>(
    cache: &BlockCache,
    hart: &mut Hart,
    bus: &mut B,
    id: u32,
    op_index: u32,
    count: u32,
    strict_csr: bool,
) -> Option<(usize, Stepped)> {
    let block = cache.block(id);
    let ops = cache.block_ops(block);
    let from = op_index as usize;
    let to = from + count as usize;
    debug_assert!(
        to < ops.len(),
        "the partial prefix is shorter than the block"
    );
    let prefix = &ops[from..to];
    let ran = run_ops::<B, ARMED>(hart, bus, block.pc, prefix, stores_in(prefix), strict_csr)
        .map_err(|(index, stepped)| (index + from, stepped));
    match ran {
        Ok(pending) => {
            hart.insns += pending;
            // `run_ops` leaves `Hart::pc` alone on the fast path.
            hart.pc = block.pc.wrapping_add(u32::from(ops[to].pc_off));
            None
        }
        Err(stopped) => Some(stopped),
    }
}

const PF_ALL: u32 = PF_R | PF_W | PF_X | PF_CODE | PF_SLOW | PF_MMIO | PF_COLD;

/// Byte offset of `vaddr` in the arena. UNVERIFIED soundness contract: the SoC owns the entry
/// encoding (low [`PAGE_SHIFT`] bits `PF_*` flags, the rest the page offset), and this result feeds
/// the unchecked pointer arithmetic of [`fast_load`] and [`fast_store`].
#[inline(always)]
fn arena_offset(entry: u32, vaddr: u32) -> usize {
    debug_assert_eq!(
        entry & (PAGE_SIZE - 1) & !PF_ALL,
        0,
        "a page-table entry set a low bit that is not a PF_* flag: the page-table encoding \
         has changed, and the arena offset this decodes is outside the arena"
    );
    ((entry & !(PAGE_SIZE - 1)) | (vaddr & (PAGE_SIZE - 1))) as usize
}

#[inline(always)]
fn in_page(vaddr: u32, size: u32) -> bool {
    (vaddr & (PAGE_SIZE - 1)) + size <= PAGE_SIZE
}

/// `PF_SLOW`, `PF_MMIO` and `PF_COLD` pages carry no `PF_R`, so one test sends them to the bus.
#[inline(always)]
fn fast_load<const N: usize>(bus: &mut impl Bus, vaddr: u32) -> Option<u32> {
    if !in_page(vaddr, N as u32) {
        return None;
    }
    let entry = bus.pages().entry(vaddr);
    if entry & PF_R == 0 {
        return None;
    }
    let offset = arena_offset(entry, vaddr);
    let base = bus.arena();
    // SAFETY: the entry carries `PF_R`, so the SoC mapped this page to the arena page at
    // `entry & !0xFFF` and the whole `N`-byte access lies inside it (`in_page`); `base` is the
    // arena the same `Bus` just handed out and is valid for that page. The read is of exactly
    // `N` bytes and needs no alignment.
    let value = unsafe {
        let at = base.add(offset);
        match N {
            1 => u32::from(*at),
            2 => u32::from(u16::from_le_bytes(*at.cast::<[u8; 2]>())),
            _ => u32::from_le_bytes(*at.cast::<[u8; 4]>()),
        }
    };
    Some(value)
}

/// A store into translated code (`PF_CODE`) always reaches the bus, which invalidates the page.
#[inline(always)]
fn fast_store<const N: usize>(bus: &mut impl Bus, vaddr: u32, value: u32) -> bool {
    if !in_page(vaddr, N as u32) {
        return false;
    }
    let entry = bus.pages().entry(vaddr);
    if entry & (PF_W | PF_CODE | PF_SLOW) != PF_W {
        return false;
    }
    let offset = arena_offset(entry, vaddr);
    let base = bus.arena();
    // SAFETY: as in `fast_load`, with `PF_W` and a write of exactly `N` bytes.
    unsafe {
        let at = base.add(offset);
        match N {
            1 => *at = value as u8,
            2 => *at.cast::<[u8; 2]>() = (value as u16).to_le_bytes(),
            _ => *at.cast::<[u8; 4]>() = value.to_le_bytes(),
        }
    }
    true
}

/// UNVERIFIED: the field set is the engine's own, not taken from a spec.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineStats {
    pub blocks_built: u64,
    /// Partial entries included.
    pub block_execs: u64,
    pub chain_hits: u64,
    pub jump_cache_hits: u64,
    pub map_hits: u64,
    pub partial_blocks: u64,
    pub resumes: u64,
    /// Including those at `crate::cache::OPS_WATERMARK`.
    pub flushes: u64,
    pub blocks_invalidated: u64,
    pub flash_invalidations: u64,
    pub fusions: u64,
    /// A speed loss, not a guest-visible difference.
    pub fusions_rejected: u64,
    /// Valid or invalidated.
    pub blocks_live: u64,
    pub ops_live: u64,
}

/// UNVERIFIED: the variants are the engine's own.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HaltCause {
    /// Cause not yet modeled.
    Unspecified,
    /// A trap that traps again at its own vector for [`TRAP_LOOP_LIMIT`] blocks.
    NoProgress {
        /// PC the hart was looping at.
        pc: u32,
    },
}

/// UNVERIFIED placeholder: no spec defines the type.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HookId(pub u32);

/// Hooks by PC, consulted while building blocks. UNVERIFIED placement: here rather than in
/// `pemu-hle` because `Engine::run` takes it and `pemu-hle` depends on this crate.
#[derive(Clone, Debug, Default)]
pub struct HookSet {
    by_pc: BTreeMap<u32, HookId>,
    pages: BitSet,
    /// Raw identifier: `gen` is a reserved keyword in edition 2024.
    r#gen: u64,
}

/// UNVERIFIED: the method set is the engine's own.
impl HookSet {
    /// Returns the hook it replaced.
    pub fn insert(&mut self, pc: u32, id: HookId) -> Option<HookId> {
        self.pages.insert((pc >> 12) as usize);
        self.r#gen = self.r#gen.wrapping_add(1);
        self.by_pc.insert(pc, id)
    }

    /// Reclaims the page bit with the page's last hook, since a stale bit taxes every later
    /// translation of that page.
    pub fn remove(&mut self, pc: u32) -> Option<HookId> {
        let removed = self.by_pc.remove(&pc)?;
        self.r#gen = self.r#gen.wrapping_add(1);
        let page = pc >> 12;
        let still_hooked = self
            .by_pc
            .range((page << 12)..)
            .next()
            .is_some_and(|(next, _)| next >> 12 == page);
        if !still_hooked {
            self.pages.remove(page as usize);
        }
        Some(removed)
    }

    pub fn get(&self, pc: u32) -> Option<HookId> {
        self.by_pc.get(&pc).copied()
    }

    pub fn page_has_hooks(&self, pc: u32) -> bool {
        self.pages.contains((pc >> 12) as usize)
    }

    /// Changes whenever the set changes.
    pub fn generation(&self) -> u64 {
        self.r#gen
    }

    pub fn len(&self) -> usize {
        self.by_pc.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_pc.is_empty()
    }

    /// In PC order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, HookId)> + '_ {
        self.by_pc.iter().map(|(pc, id)| (*pc, *id))
    }
}

/// UNVERIFIED placeholder: the engine's own type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    pub fn insert(&mut self, bit: usize) {
        let word = bit / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1 << (bit % 64);
    }

    pub fn remove(&mut self, bit: usize) {
        if let Some(word) = self.words.get_mut(bit / 64) {
            *word &= !(1 << (bit % 64));
        }
    }

    pub fn contains(&self, bit: usize) -> bool {
        self.words
            .get(bit / 64)
            .is_some_and(|w| w & (1 << (bit % 64)) != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Access, CodePage, HartView, PF_X, PageTable};
    use crate::cache::OPS_WATERMARK;
    use crate::csr::{Csr, CsrEffect, CsrOp};
    use crate::spmon::SpMonitor;

    /// SRAM1 seen through the instruction bus.
    const BASE: u32 = 0x4038_0000;
    const PAGES: u32 = 4;
    const VECTOR: u32 = 0x4038_3800;

    // Instruction encoders (RISC-V unprivileged specification, RV32I formats).

    fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32) << 20) | (rs1 << 15) | (rd << 7) | 0x13
    }

    fn lw(rd: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32) << 20) | (rs1 << 15) | (2 << 12) | (rd << 7) | 0x03
    }

    fn lbu(rd: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32) << 20) | (rs1 << 15) | (4 << 12) | (rd << 7) | 0x03
    }

    fn sw(rs1: u32, rs2: u32, imm: i32) -> u32 {
        let imm = imm as u32;
        ((imm >> 5) << 25) | (rs2 << 20) | (rs1 << 15) | (2 << 12) | ((imm & 31) << 7) | 0x23
    }

    fn jal(rd: u32, off: i32) -> u32 {
        let o = off as u32;
        let imm = ((o >> 20) & 1) << 31
            | ((o >> 1) & 0x3FF) << 21
            | ((o >> 11) & 1) << 20
            | ((o >> 12) & 0xFF) << 12;
        imm | (rd << 7) | 0x6F
    }

    fn beq(rs1: u32, rs2: u32, off: i32) -> u32 {
        let o = off as u32;
        ((o >> 12) & 1) << 31
            | ((o >> 5) & 0x3F) << 25
            | (rs2 << 20)
            | (rs1 << 15)
            | ((o >> 1) & 0xF) << 8
            | ((o >> 11) & 1) << 7
            | 0x63
    }

    const ECALL: u32 = 0x0000_0073;
    const WFI: u32 = 0x1050_0073;
    const FENCE_I: u32 = 0x0000_100F;
    const ILLEGAL: u32 = 0;

    /// RAM at [`BASE`] behind both the page table and the slow paths, so a test can flip one
    /// page's flags and compare the two routes.
    struct TestBus {
        arena: Vec<u8>,
        pages: PageTable,
        slow_loads: u32,
        slow_stores: u32,
        fetches: u32,
        /// Relative page numbers whose `fetch_code` faults.
        no_exec: BTreeSet<u32>,
        spmon: SpMonitor,
        stop_stores: bool,
        /// Stand-in ASSIST_DEBUG bounds register: a store installs [`TestBus::spmon_armed`].
        spmon_at: Option<u32>,
        spmon_armed: SpMonitor,
        wake: bool,
        csr: BTreeMap<u16, u32>,
        /// Each only when it differs from the previous report, as `pemu_soc_c3::cold` does.
        fetch_lines: Vec<u32>,
        fetch_seen: BTreeSet<u32>,
        /// Whether a line not yet in `fetch_seen` stalls.
        fetch_misses: bool,
        /// `(insns, pc)` of every stall.
        fetch_stalls: Vec<(u64, u32)>,
    }

    impl TestBus {
        fn new(words: &[u32]) -> TestBus {
            TestBus::with_pages(PAGES, words)
        }

        fn with_pages(pages: u32, words: &[u32]) -> TestBus {
            let mut arena = vec![0u8; (pages * PAGE_SIZE) as usize];
            for (i, w) in words.iter().enumerate() {
                arena[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
            let mut bus = TestBus {
                arena,
                pages: PageTable::new(),
                slow_loads: 0,
                slow_stores: 0,
                fetches: 0,
                no_exec: BTreeSet::new(),
                spmon: SpMonitor::default(),
                stop_stores: false,
                spmon_at: None,
                spmon_armed: SpMonitor::default(),
                wake: false,
                csr: BTreeMap::new(),
                fetch_lines: Vec::new(),
                fetch_seen: BTreeSet::new(),
                fetch_misses: false,
                fetch_stalls: Vec::new(),
            };
            for p in 0..pages {
                bus.map(p, PF_R | PF_W | PF_X);
            }
            bus
        }

        /// Writes the page-table entry of relative page `p`, in the encoding `arena_offset` reads.
        fn map(&mut self, p: u32, flags: u32) {
            let vpn = (BASE >> PAGE_SHIFT) + p;
            self.pages.set_entry(vpn, (p * PAGE_SIZE) | flags);
        }

        fn offset(&self, addr: u32, size: u8) -> Option<usize> {
            let off = addr.checked_sub(BASE)? as usize;
            (off + usize::from(size) <= self.arena.len()).then_some(off)
        }

        /// Overwrites the word at instruction index `i`, as self-modifying code does.
        fn poke(&mut self, i: usize, word: u32) {
            self.arena[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }

        /// Overwrites `bytes` at `addr`, for code that is not word-aligned.
        fn poke_bytes(&mut self, addr: u32, bytes: &[u8]) {
            let off = (addr - BASE) as usize;
            self.arena[off..off + bytes.len()].copy_from_slice(bytes);
        }

        fn word(&self, addr: u32) -> u32 {
            let off = (addr - BASE) as usize;
            u32::from_le_bytes([
                self.arena[off],
                self.arena[off + 1],
                self.arena[off + 2],
                self.arena[off + 3],
            ])
        }
    }

    impl Bus for TestBus {
        fn pages(&self) -> &PageTable {
            &self.pages
        }
        fn arena(&mut self) -> *mut u8 {
            self.arena.as_mut_ptr()
        }
        fn load_slow(&mut self, addr: u32, size: u8, _hart: &HartView) -> Access<u32> {
            self.slow_loads += 1;
            match self.offset(addr, size) {
                Some(off) => {
                    let mut v = 0u32;
                    for i in (0..usize::from(size)).rev() {
                        v = (v << 8) | u32::from(self.arena[off + i]);
                    }
                    Access::Ok(v)
                }
                None => Access::Fault(Trap::load_access_fault(addr)),
            }
        }
        fn store_slow(&mut self, addr: u32, size: u8, val: u32, _hart: &HartView) -> Access<()> {
            self.slow_stores += 1;
            let Some(off) = self.offset(addr, size) else {
                return Access::Fault(Trap::store_access_fault(addr));
            };
            for i in 0..usize::from(size) {
                self.arena[off + i] = (val >> (8 * i)) as u8;
            }
            if self.spmon_at == Some(addr) {
                self.spmon = self.spmon_armed;
                return Access::OkStop(());
            }
            if self.stop_stores {
                Access::OkStop(())
            } else {
                Access::Ok(())
            }
        }
        fn sp_monitor(&self) -> SpMonitor {
            self.spmon
        }
        fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap> {
            self.fetches += 1;
            let off = self
                .offset(vaddr, 1)
                .ok_or(Trap::instruction_access_fault(vaddr))?;
            if self.no_exec.contains(&(off as u32 / PAGE_SIZE)) {
                return Err(Trap::instruction_access_fault(vaddr));
            }
            let end = (off / PAGE_SIZE as usize + 1) * PAGE_SIZE as usize;
            Ok(CodePage {
                bytes: &self.arena[off..end.min(self.arena.len())],
            })
        }
        fn csr_custom(
            &mut self,
            csr: u16,
            op: CsrOp,
            _insns: u64,
        ) -> Result<(u32, CsrEffect), Trap> {
            let slot = self.csr.entry(csr).or_default();
            let before = *slot;
            if let CsrOp::Write(v) = op {
                *slot = v;
            }
            Ok((before, CsrEffect::None))
        }
        fn wfi_wake(&mut self) -> bool {
            self.wake
        }
        fn pmp_changed(&mut self, _csr: &Csr) {}
        fn fetch_enter(&mut self, insns: u64, pc: u32) -> bool {
            let line = pc >> LINE_SHIFT;
            if self.fetch_lines.last() == Some(&line) {
                return false;
            }
            self.fetch_lines.push(line);
            if self.fetch_misses && self.fetch_seen.insert(line) {
                self.fetch_stalls.push((insns, pc));
                return true;
            }
            false
        }
    }

    const LINE_SHIFT: u32 = 5;

    fn watch() -> Option<FetchWatch> {
        Some(FetchWatch {
            base: BASE,
            len: PAGES * PAGE_SIZE,
            line_shift: LINE_SHIFT,
        })
    }

    fn hart() -> Hart {
        let mut csr = Csr::new();
        csr.mtvec = VECTOR | 1;
        Hart {
            x: [0; 32],
            pc: BASE,
            csr,
            wfi: false,
            insns: 0,
            stores: 0,
            spmon: SpMonitor::default(),
            extra: 0,
            pipe: Default::default(),
        }
    }

    fn cfg(max_block_insns: u16) -> EngineCfg {
        EngineCfg {
            max_block_insns,
            ..EngineCfg::default()
        }
    }

    fn engine(max_block_insns: u16) -> Engine {
        Engine::new(cfg(max_block_insns))
    }

    /// `n` times `addi x1, x1, 1`, then a self-loop, so a longer budget stays well defined.
    fn straight(n: usize) -> Vec<u32> {
        let mut words = vec![addi(1, 1, 1); n];
        words.push(jal(0, 0));
        words
    }

    // Block shape.

    #[test]
    fn a_block_ends_at_the_first_terminator() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(3));
        let mut h = hart();
        assert_eq!(
            e.run(&mut h, &mut bus, &HookSet::default(), 3),
            Exit::Budget
        );
        assert_eq!(h.x[1], 3);
        let block = *e.cache.block(0);
        assert_eq!((block.n_ops, block.n_insns), (4, 4));
        assert_eq!(e.stats().blocks_built, 1);
    }

    #[test]
    fn a_block_ends_after_max_block_insns_with_a_fall_through() {
        for max in [1u16, 3, 64] {
            let mut e = engine(max);
            let mut bus = TestBus::new(&straight(70));
            let mut h = hart();
            e.run(&mut h, &mut bus, &HookSet::default(), 1);
            let block = *e.cache.block(0);
            assert_eq!(block.n_insns, u32::from(max), "max_block_insns {max}");
            assert_eq!(block.n_ops, u32::from(max) + 1, "the K_FALL terminator");
            let ops = e.cache.block_ops(&block);
            assert_eq!(ops[ops.len() - 1].kind, K_FALL);
            assert_eq!(ops[ops.len() - 1].len, 0);
            assert_eq!(ops[ops.len() - 1].imm2, BASE + 4 * u32::from(max));
        }
    }

    #[test]
    fn a_block_ends_at_a_page_boundary() {
        let mut words = vec![addi(1, 1, 1); 1030];
        words[1029] = jal(0, 0);
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        let mut h = hart();
        h.pc = BASE + 4 * 1020;
        e.run(&mut h, &mut bus, &HookSet::default(), 4);
        let block = *e.cache.block(0);
        assert_eq!(block.n_insns, 4, "four instructions to the end of the page");
        let ops = e.cache.block_ops(&block);
        assert_eq!(ops[ops.len() - 1].kind, K_FALL);
        assert_eq!(ops[ops.len() - 1].imm2, BASE + PAGE_SIZE);
    }

    // Exact budgets and in-block resume.

    #[test]
    fn a_run_retires_exactly_its_budget_at_every_block_size() {
        for max in [1u16, 3, 64] {
            for budget in [1u64, 2, 5, 17, 40] {
                let mut e = engine(max);
                let mut bus = TestBus::new(&straight(200));
                let mut h = hart();
                assert_eq!(
                    e.run(&mut h, &mut bus, &HookSet::default(), budget),
                    Exit::Budget
                );
                assert_eq!(h.insns, budget, "max {max}, budget {budget}");
                assert_eq!(u64::from(h.x[1]), budget);
                assert_eq!(h.pc, BASE + 4 * budget as u32);
            }
        }
    }

    #[test]
    fn stopping_inside_a_block_resumes_instead_of_translating() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(64));
        let mut h = hart();
        for _ in 0..60 {
            assert_eq!(
                e.run(&mut h, &mut bus, &HookSet::default(), 1),
                Exit::Budget
            );
        }
        assert_eq!(h.insns, 60);
        assert_eq!(h.x[1], 60);
        let stats = e.stats();
        assert_eq!(stats.blocks_built, 1, "one translation for the whole block");
        assert_eq!(
            stats.partial_blocks, 60,
            "every slice stopped inside the block"
        );
        assert_eq!(stats.resumes, 59, "every slice but the first re-entered it");
    }

    #[test]
    fn an_invalidation_rejects_the_resume_token() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(64));
        let mut h = hart();
        e.run(&mut h, &mut bus, &HookSet::default(), 4);
        e.invalidate_vrange(BASE, 4);
        e.run(&mut h, &mut bus, &HookSet::default(), 4);
        assert_eq!(h.insns, 8);
        assert_eq!(h.x[1], 8);
        assert_eq!(e.stats().blocks_built, 2);
        assert_eq!(e.stats().resumes, 0);
    }

    #[test]
    fn a_zero_budget_run_retires_nothing() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(4));
        let mut h = hart();
        assert_eq!(
            e.run(&mut h, &mut bus, &HookSet::default(), 0),
            Exit::Budget
        );
        assert_eq!(h.insns, 0);
        assert_eq!(h.pc, BASE);
    }

    #[test]
    fn step_retires_one_instruction() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(4));
        let mut h = hart();
        assert_eq!(e.step(&mut h, &mut bus, &HookSet::default()), Exit::Budget);
        assert_eq!(h.insns, 1);
        assert_eq!(h.pc, BASE + 4);
    }

    // Chaining.

    #[test]
    fn a_loop_runs_on_its_chain_slots() {
        // 0: addi x1, x1, 1
        // 1: beq x0, x0, -4   (back to 0)
        let words = [addi(1, 1, 1), beq(0, 0, -4)];
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        let mut h = hart();
        e.run(&mut h, &mut bus, &HookSet::default(), 100);
        assert_eq!(h.insns, 100);
        assert_eq!(h.x[1], 50);
        let stats = e.stats();
        assert_eq!(stats.blocks_built, 1);
        assert!(
            stats.chain_hits >= 48,
            "chain hits {} of {} block execs",
            stats.chain_hits,
            stats.block_execs
        );
    }

    #[test]
    fn a_branch_uses_the_taken_and_the_not_taken_chain_slot() {
        // 0: beq x1, x0, +8 -> 2 ; 1: ecall ; 2: addi x1, x1, 1 ; 3: jal x0, -12 -> 0
        let words = [beq(1, 0, 8), jal(0, 0), addi(1, 1, 1), jal(0, -12)];
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        let mut h = hart();
        // Taken on the first turn, not taken on the second.
        e.run(&mut h, &mut bus, &HookSet::default(), 6);
        assert_eq!(h.x[1], 1);
        assert_eq!(h.pc, BASE + 4);
        let entry = *e.cache.block(0);
        assert_ne!(entry.next[SLOT_TAKEN], NO_BLOCK, "taken slot linked");
        assert_ne!(entry.next[SLOT_FALL], NO_BLOCK, "not-taken slot linked");
        assert_ne!(entry.next[SLOT_TAKEN], entry.next[SLOT_FALL]);
        assert_eq!(e.cache.block(entry.next[SLOT_TAKEN]).pc, BASE + 8);
        assert_eq!(e.cache.block(entry.next[SLOT_FALL]).pc, BASE + 4);
    }

    // Invalidation, self-modifying code and flush.

    #[test]
    fn a_translated_page_is_reported_so_the_soc_can_mark_it_code() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(4));
        let mut h = hart();
        assert!(!e.page_is_translated(BASE));
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        assert!(e.page_is_translated(BASE));
        assert!(!e.page_is_translated(BASE + PAGE_SIZE));
        assert_eq!(
            e.translated_pages().collect::<Vec<_>>(),
            vec![BASE >> PAGE_SHIFT]
        );
    }

    #[test]
    fn self_modifying_code_runs_the_new_instruction_after_invalidation() {
        for max in [1u16, 3, 64] {
            let words = [addi(1, 1, 1), jal(0, -4)];
            let mut e = engine(max);
            let mut bus = TestBus::new(&words);
            let mut h = hart();
            e.run(&mut h, &mut bus, &HookSet::default(), 6);
            assert_eq!(h.x[1], 3, "max {max}");
            bus.poke(0, addi(1, 1, 10));
            e.invalidate_vrange(BASE, 4);
            e.run(&mut h, &mut bus, &HookSet::default(), 6);
            assert_eq!(h.x[1], 33, "max {max}: three more turns at +10");
        }
    }

    #[test]
    fn a_store_into_a_code_page_always_reaches_the_bus() {
        // sw x2, 0(x3) with x3 = BASE + 0x40
        let words = [sw(3, 2, 0), jal(0, 0)];
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        let mut h = hart();
        h.x[2] = 0xDEAD_BEEF;
        h.x[3] = BASE + 0x40;
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        assert_eq!(bus.slow_stores, 0, "a plain RW page takes the fast path");
        assert_eq!(bus.word(BASE + 0x40), 0xDEAD_BEEF);

        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        bus.map(0, PF_R | PF_W | PF_X | PF_CODE);
        let mut h = hart();
        h.x[2] = 0xDEAD_BEEF;
        h.x[3] = BASE + 0x40;
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        assert_eq!(bus.slow_stores, 1, "PF_CODE forces the slow path");
        assert_eq!(bus.word(BASE + 0x40), 0xDEAD_BEEF);
    }

    /// Fast stores are counted from the block's static store count, which these three could skew.
    #[test]
    fn stores_are_counted_around_a_slow_store_a_partial_block_and_a_resume() {
        // sw x2,0(x3); sw x2,0(x4); addi x1,x1,1; sw x2,4(x3); sw x2,8(x3); jal x0,0
        let words = [
            sw(3, 2, 0),
            sw(4, 2, 0),
            addi(1, 1, 1),
            sw(3, 2, 4),
            sw(3, 2, 8),
            jal(0, 0),
        ];
        let fresh = || {
            let mut bus = TestBus::new(&words);
            bus.map(2, PF_R | PF_W | PF_X | PF_CODE);
            let mut h = hart();
            h.x[2] = 0x1234_5678;
            h.x[3] = BASE + PAGE_SIZE;
            h.x[4] = BASE + 2 * PAGE_SIZE;
            (bus, h)
        };
        let hooks = HookSet::default();

        let mut e = engine(64);
        let (mut bus, mut h) = fresh();
        assert_eq!(e.run(&mut h, &mut bus, &hooks, 6), Exit::Budget);
        assert_eq!(h.insns, 6);
        assert_eq!(
            bus.slow_stores, 1,
            "the store into the PF_CODE page took the slow path"
        );
        assert_eq!(h.stores, 4, "every accepted store counts once");

        for cut in 1..6 {
            let mut e = engine(64);
            let (mut bus, mut h) = fresh();
            assert_eq!(e.run(&mut h, &mut bus, &hooks, cut), Exit::Budget);
            let expect_first = [1, 2, 2, 3, 4][cut as usize - 1];
            assert_eq!(
                h.stores, expect_first,
                "stores after a {cut}-instruction prefix"
            );
            assert_eq!(e.run(&mut h, &mut bus, &hooks, 6 - cut), Exit::Budget);
            assert_eq!(
                (h.insns, h.stores),
                (6, 4),
                "stores after resuming at {cut}"
            );
        }
    }

    #[test]
    fn crossing_the_ops_watermark_inside_a_run_does_not_link_a_flushed_block() {
        let mut e = engine(1);
        let mut bus = TestBus::new(&straight(8));
        let mut h = hart();
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        assert_eq!(e.cache.op_count(), 2, "one `addi` and its K_FALL");
        // The transition out of the next block is the translation that flushes.
        while e.cache.op_count() < OPS_WATERMARK - 2 {
            e.cache.push_op(synthetic(K_FALL, 0, 0));
        }
        assert_eq!(
            e.run(&mut h, &mut bus, &HookSet::default(), 4),
            Exit::Budget
        );
        assert_eq!(h.insns, 5);
        assert_eq!(h.x[1], 5);
        assert_eq!(e.stats().flushes, 1, "the watermark flushed once");
        assert!(
            e.cache.op_count() < OPS_WATERMARK,
            "the flush reclaimed op storage"
        );
    }

    #[test]
    fn a_remap_of_a_page_invalidates_only_that_page() {
        let mut words = straight(4);
        words[3] = jal(0, 4 * (1024 - 3)); // jump into page 1
        let mut image = words.clone();
        image.resize(1024, 0);
        image.push(addi(2, 2, 7));
        image.push(jal(0, 0));
        let mut e = engine(64);
        let mut bus = TestBus::new(&image);
        let mut h = hart();
        e.run(&mut h, &mut bus, &HookSet::default(), 6);
        assert_eq!(h.x[2], 7);
        assert!(e.page_is_translated(BASE));
        assert!(e.page_is_translated(BASE + PAGE_SIZE));
        e.invalidate_vrange(BASE + PAGE_SIZE, PAGE_SIZE);
        assert!(e.page_is_translated(BASE));
        assert!(!e.page_is_translated(BASE + PAGE_SIZE));
    }

    #[test]
    fn a_block_is_dropped_by_the_page_its_last_instruction_straddles() {
        for max in [1u16, 3, 64] {
            // 0x40380FFE  addi x1, x1, 1   bytes 93 80 10 00: `10 00` is on the next page
            // 0x40381002  jal  x0, -4      back to the addi
            let straddle = BASE + PAGE_SIZE - 2;
            let mut e = engine(max);
            let mut bus = TestBus::new(&[]);
            bus.poke_bytes(straddle, &addi(1, 1, 1).to_le_bytes());
            bus.poke_bytes(straddle + 4, &jal(0, -4).to_le_bytes());
            let mut h = hart();
            h.pc = straddle;
            e.run(&mut h, &mut bus, &HookSet::default(), 6);
            assert_eq!(h.x[1], 3, "max {max}: three turns at +1");
            assert!(
                e.page_is_translated(BASE + PAGE_SIZE),
                "max {max}: the second page holds two bytes of the block"
            );

            // Rewrite the immediate to 16 through the two bytes on the second page only.
            bus.poke_bytes(BASE + PAGE_SIZE, &addi(1, 1, 16).to_le_bytes()[2..]);
            e.invalidate_vrange(BASE + PAGE_SIZE, 2);
            e.run(&mut h, &mut bus, &HookSet::default(), 6);
            assert_eq!(h.x[1], 51, "max {max}: three more turns at +16");
        }
    }

    #[test]
    fn a_straddling_block_is_dropped_once_by_either_page() {
        let straddle = BASE + PAGE_SIZE - 2;
        for invalidate_first in [BASE, BASE + PAGE_SIZE] {
            let mut e = engine(64);
            let mut bus = TestBus::new(&[]);
            bus.poke_bytes(straddle, &addi(1, 1, 1).to_le_bytes());
            bus.poke_bytes(straddle + 4, &jal(0, -4).to_le_bytes());
            let mut h = hart();
            h.pc = straddle;
            e.run(&mut h, &mut bus, &HookSet::default(), 6);
            assert_eq!(e.stats().blocks_built, 2);
            e.invalidate_vrange(invalidate_first, 1);
            assert!(!e.page_is_translated(invalidate_first));
            let after_first = e.stats().blocks_invalidated;
            e.invalidate_vrange(BASE, PAGE_SIZE * 2);
            assert_eq!(
                e.stats().blocks_invalidated,
                2,
                "invalidating 0x{invalidate_first:08x} first dropped {after_first}, and a block \
                 is never dropped twice"
            );
            assert!(!e.page_is_translated(BASE));
            assert!(!e.page_is_translated(BASE + PAGE_SIZE));
        }
    }

    /// 700 pages of one-instruction blocks pass the watermark inside a single run.
    #[test]
    fn a_flush_at_the_op_watermark_does_not_link_a_freed_block() {
        const PAGES_HERE: u32 = 700;
        const INSNS: u64 = 700_000;
        let words = vec![addi(1, 1, 1); (PAGES_HERE * PAGE_SIZE / 4) as usize];
        let mut e = engine(1);
        let mut bus = TestBus::with_pages(PAGES_HERE, &words);
        let mut h = hart();
        assert_eq!(
            e.run(&mut h, &mut bus, &HookSet::default(), INSNS),
            Exit::Budget
        );
        assert_eq!(h.insns, INSNS);
        assert_eq!(h.x[1] as u64, INSNS);
        assert!(
            e.stats().flushes > 0,
            "the run has to pass the watermark for this to test anything"
        );
    }

    #[test]
    fn a_block_whose_last_instruction_straddles_a_page_belongs_to_both_pages() {
        let at = (PAGE_SIZE - 2) as usize;
        for store_page in [BASE, BASE + PAGE_SIZE] {
            let mut e = engine(64);
            let mut bus = TestBus::new(&[]);
            bus.arena[at..at + 4].copy_from_slice(&addi(1, 1, 1).to_le_bytes());
            bus.arena[at + 4..at + 8].copy_from_slice(&jal(0, 0).to_le_bytes());
            let mut h = hart();
            h.pc = BASE + PAGE_SIZE - 2;
            assert_eq!(
                e.run(&mut h, &mut bus, &HookSet::default(), 1),
                Exit::Budget
            );
            assert_eq!(h.x[1], 1);
            assert!(e.page_is_translated(BASE), "the page the block starts in");
            assert!(
                e.page_is_translated(BASE + PAGE_SIZE),
                "the page its second halfword came from"
            );
            assert_eq!(
                e.translated_pages().collect::<Vec<_>>(),
                vec![BASE >> PAGE_SHIFT, (BASE >> PAGE_SHIFT) + 1]
            );

            bus.arena[at..at + 4].copy_from_slice(&addi(1, 1, 0x100).to_le_bytes());
            e.invalidate_vrange(store_page, 2);
            assert!(
                !e.page_is_translated(BASE) && !e.page_is_translated(BASE + PAGE_SIZE),
                "store into 0x{store_page:08x}: the block left neither page index behind"
            );
            h.pc = BASE + PAGE_SIZE - 2;
            e.run(&mut h, &mut bus, &HookSet::default(), 1);
            assert_eq!(
                h.x[1],
                1 + 0x100,
                "store into 0x{store_page:08x}: the new instruction ran"
            );
        }
    }

    #[test]
    fn invalidating_one_page_of_a_straddling_block_clears_it_from_the_other() {
        let at = (PAGE_SIZE - 2) as usize;
        let mut e = engine(64);
        let mut bus = TestBus::new(&[]);
        bus.arena[at..at + 4].copy_from_slice(&addi(1, 1, 1).to_le_bytes());
        bus.arena[at + 4..at + 8].copy_from_slice(&jal(0, 0).to_le_bytes());
        let mut h = hart();
        h.pc = BASE + PAGE_SIZE - 2;
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        e.invalidate_vrange(BASE, 2);
        assert_eq!(
            e.cache.page_block_ids((BASE >> PAGE_SHIFT) + 1),
            &[] as &[u32]
        );
        // The freed slot goes to a block on page 2, which page 1 must not drop.
        bus.arena[2 * PAGE_SIZE as usize..2 * PAGE_SIZE as usize + 4]
            .copy_from_slice(&jal(0, 0).to_le_bytes());
        h.pc = BASE + 2 * PAGE_SIZE;
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        assert!(e.page_is_translated(BASE + 2 * PAGE_SIZE));
        e.invalidate_vrange(BASE + PAGE_SIZE, 2);
        assert!(
            e.page_is_translated(BASE + 2 * PAGE_SIZE),
            "the reused slot survived an invalidation of a page it was never translated from"
        );
    }

    #[test]
    fn fence_i_flushes_the_cache_and_carries_on() {
        let words = [addi(1, 1, 1), FENCE_I, addi(1, 1, 1), jal(0, 0)];
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        let mut h = hart();
        assert_eq!(
            e.run(&mut h, &mut bus, &HookSet::default(), 4),
            Exit::Budget
        );
        assert_eq!(h.x[1], 2);
        assert_eq!(h.insns, 4);
        assert_eq!(e.stats().flushes, 1);
        assert!(e.stats().blocks_built >= 2);
    }

    #[test]
    fn flush_and_flash_invalidation_drop_translations() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(4));
        let mut h = hart();
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        assert!(e.page_is_translated(BASE));
        e.flush();
        assert!(!e.page_is_translated(BASE));
        assert_eq!(e.stats().flushes, 1);
        e.invalidate_flash_page(0);
        assert_eq!(e.stats().flash_invalidations, 1);
    }

    /// One page of code inside a flash-mapped window, the only kind of machine
    /// `Engine::invalidate_flash_page` drops anything from.
    struct FlashBus {
        base: u32,
        arena: Vec<u8>,
        pages: PageTable,
    }

    impl FlashBus {
        fn new(base: u32, words: &[u32]) -> FlashBus {
            let mut arena = vec![0u8; PAGE_SIZE as usize];
            for (i, w) in words.iter().enumerate() {
                arena[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
            let mut pages = PageTable::new();
            pages.set_entry(base >> PAGE_SHIFT, PF_R | PF_X | PF_CODE);
            FlashBus { base, arena, pages }
        }

        fn offset(&self, addr: u32, size: u8) -> Option<usize> {
            let off = addr.checked_sub(self.base)? as usize;
            (off + usize::from(size) <= self.arena.len()).then_some(off)
        }
    }

    impl Bus for FlashBus {
        fn pages(&self) -> &PageTable {
            &self.pages
        }
        fn arena(&mut self) -> *mut u8 {
            self.arena.as_mut_ptr()
        }
        fn load_slow(&mut self, addr: u32, size: u8, _hart: &HartView) -> Access<u32> {
            match self.offset(addr, size) {
                Some(off) => {
                    let mut v = 0u32;
                    for i in (0..usize::from(size)).rev() {
                        v = (v << 8) | u32::from(self.arena[off + i]);
                    }
                    Access::Ok(v)
                }
                None => Access::Fault(Trap::load_access_fault(addr)),
            }
        }
        fn store_slow(&mut self, addr: u32, size: u8, val: u32, _hart: &HartView) -> Access<()> {
            let Some(off) = self.offset(addr, size) else {
                return Access::Fault(Trap::store_access_fault(addr));
            };
            for i in 0..usize::from(size) {
                self.arena[off + i] = (val >> (8 * i)) as u8;
            }
            Access::Ok(())
        }
        fn sp_monitor(&self) -> SpMonitor {
            SpMonitor::default()
        }
        fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap> {
            let off = self
                .offset(vaddr, 1)
                .ok_or(Trap::instruction_access_fault(vaddr))?;
            Ok(CodePage {
                bytes: &self.arena[off..],
            })
        }
        fn csr_custom(
            &mut self,
            _csr: u16,
            _op: CsrOp,
            _insns: u64,
        ) -> Result<(u32, CsrEffect), Trap> {
            Ok((0, CsrEffect::None))
        }
        fn wfi_wake(&mut self) -> bool {
            false
        }
        fn pmp_changed(&mut self, _csr: &Csr) {}
    }

    #[test]
    fn a_flash_write_drops_the_blocks_of_both_flash_windows() {
        for base in [FLASH_IBUS_BASE, FLASH_DBUS_BASE] {
            let mut e = engine(64);
            let mut bus = FlashBus::new(base, &[addi(1, 1, 1), jal(0, -4)]);
            let mut h = hart();
            h.pc = base;
            e.run(&mut h, &mut bus, &HookSet::default(), 4);
            assert_eq!(h.x[1], 2, "window 0x{base:08x}");
            assert!(e.page_is_translated(base), "window 0x{base:08x}");

            e.invalidate_flash_page(0);
            assert_eq!(e.stats().flash_invalidations, 1);
            assert!(
                !e.page_is_translated(base),
                "window 0x{base:08x}: the flash write left a block behind"
            );
            assert_eq!(
                e.stats().blocks_invalidated,
                1,
                "window 0x{base:08x}: exactly the one translated block"
            );

            bus.arena[0..4].copy_from_slice(&addi(1, 1, 10).to_le_bytes());
            e.run(&mut h, &mut bus, &HookSet::default(), 4);
            assert_eq!(h.x[1], 22, "window 0x{base:08x}: two turns at +10");
        }
    }

    #[test]
    fn a_flash_write_does_not_walk_the_windows_page_by_page() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(4));
        let mut h = hart();
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        let before = e.stats();
        e.invalidate_flash_page(0);
        let after = e.stats();
        assert_eq!(after.blocks_invalidated, before.blocks_invalidated);
        assert_eq!(after.blocks_live, before.blocks_live);
        assert!(
            e.page_is_translated(BASE),
            "SRAM code is not in a flash window"
        );
    }

    // Hooks.

    #[test]
    fn a_hooked_pc_becomes_a_terminator_and_exits_without_executing() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(4));
        let mut h = hart();
        let mut hooks = HookSet::default();
        hooks.insert(BASE + 8, HookId(9));
        assert_eq!(
            e.run(&mut h, &mut bus, &hooks, 10),
            Exit::Hook { id: HookId(9) }
        );
        assert_eq!(h.pc, BASE + 8, "the hart stops at the hooked pc");
        assert_eq!(h.insns, 2, "the hooked instruction did not run");
        assert_eq!(h.x[1], 2);
        assert_eq!(
            e.run(&mut h, &mut bus, &hooks, 10),
            Exit::Hook { id: HookId(9) }
        );
        assert_eq!(h.insns, 2);
    }

    // Fetch watch (the fetch side of the `lru16k` cache model).

    /// Crosses lines 0 and 1 three times, then spins at line 4 after 13 instructions.
    fn line_loop() -> Vec<u32> {
        let mut w = vec![addi(0, 0, 0); 33];
        w[0] = addi(1, 0, 3);
        w[1] = jal(0, 36); // to word 10, line 1
        w[10] = addi(1, 1, -1);
        w[11] = beq(1, 0, 12); // to word 14 when x1 is 0
        w[12] = jal(0, -44); // to word 1, line 0
        w[14] = jal(0, 72); // to word 32, line 4
        w[32] = jal(0, 0);
        w
    }

    /// The line changes of [`line_loop`].
    fn line_loop_lines() -> Vec<u32> {
        [0u32, 1, 0, 1, 0, 1, 4]
            .iter()
            .map(|l| (BASE >> LINE_SHIFT) + l)
            .collect()
    }

    /// Runs `words` for `total` instructions in slices of `slice`; returns the line reports and
    /// stalls.
    fn run_watched(
        words: &[u32],
        max: u16,
        slice: u64,
        total: u64,
        misses: bool,
    ) -> (Vec<u32>, Vec<(u64, u32)>) {
        let mut e = engine(max);
        e.set_fetch_watch(watch());
        let mut bus = TestBus::new(words);
        bus.fetch_misses = misses;
        let mut h = hart();
        let mut guard = 0;
        while h.insns < total {
            let budget = slice.min(total - h.insns);
            match e.run(&mut h, &mut bus, &HookSet::default(), budget) {
                Exit::Budget => {}
                Exit::Stop => assert_eq!(bus.fetch_stalls.last(), Some(&(h.insns, h.pc))),
                other => panic!("unexpected exit {other:?}"),
            }
            guard += 1;
            assert!(guard < 10_000, "the run makes progress");
        }
        (bus.fetch_lines, bus.fetch_stalls)
    }

    #[test]
    fn a_watched_block_ends_at_its_line_and_is_not_chained_across_lines() {
        let mut e = engine(64);
        e.set_fetch_watch(watch());
        let mut bus = TestBus::new(&straight(20));
        let mut h = hart();
        assert_eq!(
            e.run(&mut h, &mut bus, &HookSet::default(), 20),
            Exit::Budget
        );
        let block = *e.cache.block(0);
        assert_eq!(block.n_insns, 8, "the first block ends at its 32-byte line");
        let ops = e.cache.block_ops(&block);
        assert_eq!(ops[ops.len() - 1].kind, K_FALL);
        assert_eq!(ops[ops.len() - 1].imm2, BASE + 32);
        let first = BASE >> LINE_SHIFT;
        assert_eq!(bus.fetch_lines, vec![first, first + 1, first + 2]);
        // A second pass reports the same lines: no chain slot crosses a line.
        let mut h = hart();
        bus.fetch_lines.clear();
        e.run(&mut h, &mut bus, &HookSet::default(), 20);
        assert_eq!(bus.fetch_lines, vec![first, first + 1, first + 2]);
    }

    #[test]
    fn without_a_watch_no_fetch_is_reported_and_blocks_keep_their_shape() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(20));
        let mut h = hart();
        e.run(&mut h, &mut bus, &HookSet::default(), 20);
        assert!(bus.fetch_lines.is_empty());
        assert_eq!(e.cache.block(0).n_insns, 21, "20 `addi` and the self-loop");
    }

    #[test]
    fn the_line_reports_and_stalls_do_not_depend_on_block_size_or_slice() {
        let words = line_loop();
        let want_lines = line_loop_lines();
        let mut want_stalls = None;
        for max in [1u16, 3, 64] {
            for slice in [1u64, 2, 5, 40] {
                let (lines, none) = run_watched(&words, max, slice, 40, false);
                assert_eq!(lines, want_lines, "max {max}, slice {slice}");
                assert!(none.is_empty());
                let (lines, stalls) = run_watched(&words, max, slice, 40, true);
                assert_eq!(lines, want_lines, "max {max}, slice {slice}, with stalls");
                assert_eq!(stalls.len(), 3, "max {max}, slice {slice}");
                let want = want_stalls.get_or_insert_with(|| stalls.clone());
                assert_eq!(&stalls, want, "max {max}, slice {slice}");
            }
        }
        assert_eq!(
            want_stalls
                .expect("ran")
                .iter()
                .map(|s| s.0)
                .collect::<Vec<_>>(),
            vec![0, 2, 13],
            "line 0 before any instruction, line 1 at the first jal's target, line 4 at the last"
        );
    }

    #[test]
    fn a_stall_at_a_run_past_pc_keeps_the_request() {
        let mut e = engine(64);
        e.set_fetch_watch(watch());
        let mut bus = TestBus::new(&straight(20));
        let mut h = hart();
        let mut hooks = HookSet::default();
        hooks.insert(BASE + 32, HookId(9));
        assert_eq!(
            e.run(&mut h, &mut bus, &hooks, 20),
            Exit::Hook { id: HookId(9) }
        );
        assert_eq!(h.insns, 8);
        // As after a cache flush between the hook and the run past.
        bus.fetch_lines.clear();
        bus.fetch_misses = true;
        e.continue_past_hook(BASE + 32);
        assert_eq!(e.run(&mut h, &mut bus, &hooks, 4), Exit::Stop);
        assert_eq!((h.pc, h.insns), (BASE + 32, 8), "the stall retired nothing");
        assert_eq!(e.run(&mut h, &mut bus, &hooks, 4), Exit::Budget);
        assert_eq!(
            (h.pc, h.insns),
            (BASE + 48, 12),
            "the hooked instruction ran once"
        );
        assert_eq!(bus.fetch_stalls, vec![(8, BASE + 32)]);
    }

    #[test]
    fn continue_past_hook_runs_the_hooked_instruction_exactly_once() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(6));
        let mut h = hart();
        let mut hooks = HookSet::default();
        hooks.insert(BASE + 8, HookId(9));
        e.run(&mut h, &mut bus, &hooks, 10);
        assert_eq!(h.insns, 2);
        e.continue_past_hook(BASE + 8);
        assert_eq!(e.run(&mut h, &mut bus, &hooks, 3), Exit::Budget);
        assert_eq!(h.insns, 5);
        assert_eq!(h.x[1], 5);
        assert_eq!(h.pc, BASE + 20);
    }

    #[test]
    fn a_hook_change_invalidates_only_hooked_pages() {
        let mut image = straight(4);
        image[3] = jal(0, 4 * (1024 - 3));
        image.resize(1024, 0);
        image.push(addi(2, 2, 1));
        image.push(jal(0, 0));
        let mut e = engine(64);
        let mut bus = TestBus::new(&image);
        let mut h = hart();
        let mut hooks = HookSet::default();
        e.run(&mut h, &mut bus, &hooks, 6);
        assert!(e.page_is_translated(BASE));
        assert!(e.page_is_translated(BASE + PAGE_SIZE));
        hooks.insert(BASE + 4, HookId(1));
        h.pc = BASE + PAGE_SIZE;
        e.run(&mut h, &mut bus, &hooks, 1);
        assert!(!e.page_is_translated(BASE), "the hooked page was dropped");
        assert!(
            e.page_is_translated(BASE + PAGE_SIZE),
            "the page with no hook kept its block"
        );
    }

    #[test]
    fn removing_a_hook_invalidates_the_page_it_was_on() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&straight(6));
        let mut h = hart();
        let mut hooks = HookSet::default();
        hooks.insert(BASE + 8, HookId(9));
        assert!(matches!(
            e.run(&mut h, &mut bus, &hooks, 10),
            Exit::Hook { .. }
        ));
        hooks.remove(BASE + 8);
        assert_eq!(e.run(&mut h, &mut bus, &hooks, 4), Exit::Budget);
        assert_eq!(h.insns, 6);
        assert_eq!(h.x[1], 6);
    }

    // Stops, stack monitor, wfi and traps.

    #[test]
    fn an_ok_stop_store_refreshes_the_monitor_and_leaves_the_block() {
        let words = [sw(3, 2, 0), addi(1, 1, 1), jal(0, 0)];
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        bus.map(0, PF_R | PF_X); // no PF_W, so the store takes the slow path
        bus.stop_stores = true;
        bus.spmon = SpMonitor {
            on_min: true,
            on_max: false,
            min: 0x3FC8_0000,
            max: 0,
        };
        let mut h = hart();
        h.x[3] = BASE + 0x40;
        assert_eq!(e.run(&mut h, &mut bus, &HookSet::default(), 3), Exit::Stop);
        assert_eq!(h.insns, 1);
        assert!(h.spmon.on_min);
        assert_eq!(h.spmon.min, 0x3FC8_0000);
        assert_eq!(e.stats().blocks_built, 1);
        assert_eq!(
            e.run(&mut h, &mut bus, &HookSet::default(), 2),
            Exit::Budget
        );
        assert_eq!(e.stats().blocks_built, 1);
        assert_eq!(e.stats().resumes, 1);
        assert_eq!(h.x[1], 1);
    }

    #[test]
    fn a_stack_spill_ends_the_block_at_the_same_insns_for_every_block_size() {
        // Three `addi sp, sp, -0x100`, starting above the bound and falling through it.
        let words = [addi(2, 2, -0x100); 3];
        for max in [1u16, 3, 64] {
            let mut e = engine(max);
            let mut bus = TestBus::new(&words);
            let mut h = hart();
            h.x[2] = 0x3FC8_0200;
            h.spmon = SpMonitor {
                on_min: true,
                on_max: false,
                min: 0x3FC8_0000,
                max: 0,
            };
            bus.spmon = h.spmon;
            assert_eq!(
                e.run(&mut h, &mut bus, &HookSet::default(), 3),
                Exit::SpSpill(SpSpill::Min),
                "max {max}"
            );
            // The bounds are inclusive, so the third write is the one below.
            assert_eq!(h.insns, 3, "max {max}: the third write crosses the bound");
            assert_eq!(
                e.spill_pc(),
                BASE + 8,
                "max {max}: the pc of the third write"
            );
            assert_eq!(h.x[2], 0x3FC8_0000_u32.wrapping_sub(0x100));
        }
    }

    /// `Engine::run` monomorphises on `SpMonitor::armed` once per run, which is only correct
    /// because every path that can change the monitor ends the run.
    #[test]
    fn an_interrupt_that_rewrites_the_bounds_spills_at_the_same_insns_at_every_block_size() {
        /// Stand-in ASSIST_DEBUG bounds register, on the second page so it is not code.
        const MON: u32 = BASE + PAGE_SIZE + 0x40;
        /// First instruction of the interrupt entry.
        const ENTRY: u32 = BASE + 16;
        let words = [
            // The interrupted code: two instructions and an idle loop.
            addi(1, 1, 1),
            addi(1, 1, 1),
            jal(0, 0),
            ILLEGAL,
            // The entry: arm the monitor, then walk `sp` down through the new minimum.
            sw(4, 5, 0),
            addi(2, 2, -0x100),
            addi(2, 2, -0x100),
            addi(2, 2, -0x100),
            jal(0, 0),
        ];
        for max in [1u16, 3, 64] {
            let mut e = engine(max);
            let mut bus = TestBus::new(&words);
            bus.map(1, PF_R | PF_SLOW); // the register page: every store reaches the bus
            bus.spmon_at = Some(MON);
            bus.spmon_armed = SpMonitor {
                on_min: true,
                on_max: false,
                min: 0x3FC8_0000,
                max: 0,
            };
            let mut h = hart();
            h.x[2] = 0x3FC8_0200;
            h.x[4] = MON;

            // Two instructions of ordinary code with the monitor off.
            assert_eq!(
                e.run(&mut h, &mut bus, &HookSet::default(), 2),
                Exit::Budget,
                "max {max}"
            );
            assert!(!h.spmon.armed(), "max {max}: the monitor starts off");

            h.pc = ENTRY;
            assert_eq!(
                e.run(&mut h, &mut bus, &HookSet::default(), 16),
                Exit::Stop,
                "max {max}: the bounds write answers OkStop"
            );
            assert_eq!(h.insns, 3, "max {max}");
            assert!(h.spmon.on_min, "max {max}: the new bounds reached the hart");
            assert_eq!(h.spmon.min, 0x3FC8_0000, "max {max}");

            assert_eq!(
                e.run(&mut h, &mut bus, &HookSet::default(), 16),
                Exit::SpSpill(SpSpill::Min),
                "max {max}"
            );
            assert_eq!(
                h.insns, 6,
                "max {max}: the third write is the one that crosses the new bound"
            );
            assert_eq!(h.x[2], 0x3FC8_0000_u32.wrapping_sub(0x100), "max {max}");
        }
    }

    #[test]
    fn bounds_rewritten_by_an_ok_stop_spill_at_the_same_insns_for_every_block_size() {
        let words = [
            sw(3, 2, 0),
            addi(2, 2, -0x100),
            addi(2, 2, -0x100),
            addi(2, 2, -0x100),
            jal(0, 0),
        ];
        for max in [1u16, 3, 64] {
            let mut e = engine(max);
            let mut bus = TestBus::new(&words);
            bus.map(0, PF_R | PF_X); // no PF_W, so the store takes the slow path
            bus.stop_stores = true;
            // What the entry code left behind: a minimum 0x80000 above the one the hart runs with.
            bus.spmon = SpMonitor {
                on_min: true,
                on_max: false,
                min: 0x3FC8_0000,
                max: 0,
            };
            let mut h = hart();
            h.x[2] = 0x3FC8_0200;
            h.x[3] = BASE + 0x40;
            h.spmon = SpMonitor {
                on_min: true,
                on_max: false,
                min: 0x3FC0_0000,
                max: 0,
            };
            assert_eq!(
                e.run(&mut h, &mut bus, &HookSet::default(), 8),
                Exit::Stop,
                "max {max}"
            );
            assert_eq!(h.insns, 1, "max {max}");
            assert_eq!(
                h.spmon.min, 0x3FC8_0000,
                "max {max}: the new bound reached the hart"
            );
            assert_eq!(
                e.run(&mut h, &mut bus, &HookSet::default(), 7),
                Exit::SpSpill(SpSpill::Min),
                "max {max}"
            );
            // Under the old bounds none of the three writes would trip.
            assert_eq!(h.insns, 4, "max {max}");
            assert_eq!(h.x[2], 0x3FC8_0000_u32.wrapping_sub(0x100), "max {max}");
        }
    }

    /// The memory fast path has its own copy of the check.
    #[test]
    fn a_stack_spill_from_a_fast_path_load_ends_the_block() {
        // lw sp, 0(x3) loading a value below the bound.
        let words = [lw(2, 3, 0), addi(1, 1, 1), jal(0, 0)];
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        bus.arena[0x40..0x44].copy_from_slice(&0x1000_0000u32.to_le_bytes());
        let mut h = hart();
        h.x[3] = BASE + 0x40;
        h.spmon = SpMonitor {
            on_min: true,
            on_max: false,
            min: 0x3FC8_0000,
            max: 0,
        };
        bus.spmon = h.spmon;
        assert_eq!(
            e.run(&mut h, &mut bus, &HookSet::default(), 3),
            Exit::SpSpill(SpSpill::Min)
        );
        assert_eq!(bus.slow_loads, 0, "the load took the fast path");
        assert_eq!(h.insns, 1);
        assert_eq!(h.x[2], 0x1000_0000);
    }

    #[test]
    fn wfi_is_a_terminator_and_ends_the_run() {
        let words = [addi(1, 1, 1), WFI, addi(1, 1, 1)];
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        let mut h = hart();
        assert_eq!(e.run(&mut h, &mut bus, &HookSet::default(), 3), Exit::Wfi);
        assert!(h.wfi);
        assert_eq!(h.insns, 2);
        assert_eq!(h.pc, BASE + 8);
    }

    #[test]
    fn a_trap_continues_at_the_vector_inside_the_same_run() {
        let mut image = vec![ECALL];
        image.resize((VECTOR - BASE) as usize / 4, 0);
        image.push(addi(1, 1, 5));
        image.push(jal(0, 0));
        let mut e = engine(64);
        let mut bus = TestBus::new(&image);
        let mut h = hart();
        assert_eq!(
            e.run(&mut h, &mut bus, &HookSet::default(), 1),
            Exit::Budget
        );
        assert_eq!(h.insns, 1, "the ecall retired nothing, the addi did");
        assert_eq!(h.x[1], 5);
        assert_eq!(h.csr.mepc, BASE);
    }

    /// A fault at its own vector loops forever on silicon; `run` must still return.
    #[test]
    fn a_trap_that_faults_at_its_own_vector_halts() {
        let mut e = engine(64);
        let mut bus = TestBus::new(&[ILLEGAL]);
        let mut h = hart();
        h.csr.mtvec = 1; // vector at 0, which is not mapped: every fetch there faults
        let exit = e.run(&mut h, &mut bus, &HookSet::default(), 1_000);
        assert_eq!(exit, Exit::Halted(HaltCause::NoProgress { pc: 0 }));
        assert_eq!(h.insns, 0);
    }

    // Memory fast paths.

    #[test]
    fn a_load_without_read_permission_falls_through_to_the_bus() {
        let words = [lw(1, 3, 0), jal(0, 0)];
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        bus.arena[0x40..0x44].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        bus.map(0, PF_W | PF_X); // no PF_R
        let mut h = hart();
        h.x[3] = BASE + 0x40;
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        assert_eq!(bus.slow_loads, 1);
        assert_eq!(h.x[1], 0x1234_5678);
    }

    #[test]
    fn an_access_crossing_a_page_boundary_falls_through_to_the_bus() {
        let words = [lw(1, 3, 0), jal(0, 0)];
        let mut e = engine(64);
        let mut bus = TestBus::new(&words);
        let mut h = hart();
        h.x[3] = BASE + PAGE_SIZE - 2;
        e.run(&mut h, &mut bus, &HookSet::default(), 1);
        assert_eq!(bus.slow_loads, 1, "a straddling word is not a fast load");
    }

    /// The fast path must extend exactly as `crate::exec` does.
    #[test]
    fn the_fast_and_the_slow_load_agree_on_every_width() {
        let words = [lw(1, 3, 0), lbu(4, 3, 1), jal(0, 0)];
        let value = 0x8091_A2B3u32;
        let mut fast = TestBus::new(&words);
        fast.arena[0x40..0x44].copy_from_slice(&value.to_le_bytes());
        let mut slow = TestBus::new(&words);
        slow.arena[0x40..0x44].copy_from_slice(&value.to_le_bytes());
        slow.map(0, PF_W | PF_X);
        let mut a = hart();
        let mut b = hart();
        a.x[3] = BASE + 0x40;
        b.x[3] = BASE + 0x40;
        let mut e1 = engine(64);
        let mut e2 = engine(64);
        e1.run(&mut a, &mut fast, &HookSet::default(), 2);
        e2.run(&mut b, &mut slow, &HookSet::default(), 2);
        assert_eq!(fast.slow_loads, 0);
        assert_eq!(slow.slow_loads, 2);
        assert_eq!(a.x, b.x);
        assert_eq!(a.x[1], value);
        assert_eq!(a.x[4], 0xA2);
    }

    #[test]
    fn arena_offset_reads_the_low_twelve_bits_as_flags() {
        assert_eq!(arena_offset(0x5000 | PF_R | PF_W, 0x4038_0123), 0x5123);
        assert_eq!(arena_offset(0, 0x4038_0FFF), 0xFFF);
        assert!(in_page(0x4038_0FFC, 4));
        assert!(!in_page(0x4038_0FFD, 4));
        assert!(in_page(0x4038_0FFF, 1));
    }

    // Extension points and the hook set.

    #[test]
    fn hook_set_insert_get_and_generation() {
        let mut set = HookSet::default();
        assert!(set.is_empty());
        assert!(!set.page_has_hooks(0x4200_0010));
        assert_eq!(set.insert(0x4200_0010, HookId(5)), None);
        assert_eq!(set.get(0x4200_0010), Some(HookId(5)));
        assert_eq!(set.get(0x4200_0014), None);
        assert!(set.page_has_hooks(0x4200_0FF0));
        assert!(!set.page_has_hooks(0x4200_1000));
        let generation = set.generation();
        assert_eq!(set.insert(0x4200_0010, HookId(6)), Some(HookId(5)));
        assert_ne!(set.generation(), generation);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![(0x4200_0010, HookId(6))]
        );
        assert_eq!(set.len(), 1);
        assert_eq!(set.remove(0x4200_0010), Some(HookId(6)));
        assert_eq!(set.remove(0x4200_0010), None);
        assert!(set.is_empty());
    }

    #[test]
    fn removing_the_last_hook_of_a_page_gives_the_page_bit_back() {
        let mut set = HookSet::default();
        set.insert(0x4200_0010, HookId(1));
        set.insert(0x4200_0FFC, HookId(2));
        set.insert(0x4200_1000, HookId(3));
        assert!(set.page_has_hooks(0x4200_0010));
        set.remove(0x4200_0010);
        assert!(
            set.page_has_hooks(0x4200_0010),
            "one hook is left on the page"
        );
        set.remove(0x4200_0FFC);
        assert!(!set.page_has_hooks(0x4200_0010), "the page bit came back");
        assert!(
            set.page_has_hooks(0x4200_1000),
            "the next page kept its own"
        );
        let generation = set.generation();
        assert_eq!(set.remove(0x4200_0010), None);
        assert_eq!(set.generation(), generation);
        assert!(set.page_has_hooks(0x4200_1000));
    }

    fn plain_op() -> Op {
        Op {
            kind: 0,
            rd: 0,
            rs1: 0,
            rs2: 0,
            imm: 0,
            imm2: 0,
            len: 4,
            flags: 0,
            pc_off: 0,
        }
    }

    #[test]
    fn no_fusion_fuses_nothing() {
        let fuser: &'static dyn OpFuser = &NoFusion;
        assert!(fuser.fuse(&[]).is_none());
        assert!(fuser.fuse(&[plain_op(), plain_op()]).is_none());
    }

    /// Rewrites `add rd, rs1, x0` into `addi rd, rs1, 0` and counts the windows offered.
    struct MoveToAddi {
        windows: std::sync::atomic::AtomicUsize,
        last_len: std::sync::atomic::AtomicUsize,
    }

    impl OpFuser for MoveToAddi {
        fn fuse(&self, window: &[Op]) -> Option<(Op, usize)> {
            use std::sync::atomic::Ordering::Relaxed;
            self.windows.fetch_add(1, Relaxed);
            self.last_len.store(window.len(), Relaxed);
            let last = window.last()?;
            if last.kind != K_ADD || last.rs2 & 31 != 0 {
                return None;
            }
            let fused = Op {
                kind: K_ADDI,
                rd: last.rd,
                rs1: last.rs1,
                rs2: 0,
                imm: 0,
                imm2: last.imm2,
                len: last.len,
                flags: flags_for(K_ADDI, last.rd),
                pc_off: last.pc_off,
            };
            Some((fused, 1))
        }
    }

    static MOVE_TO_ADDI: MoveToAddi = MoveToAddi {
        windows: std::sync::atomic::AtomicUsize::new(0),
        last_len: std::sync::atomic::AtomicUsize::new(0),
    };

    fn add(rd: u32, rs1: u32, rs2: u32) -> u32 {
        (rs2 << 20) | (rs1 << 15) | (rd << 7) | 0x33
    }

    #[test]
    fn block_building_offers_the_block_to_the_fuser() {
        use std::sync::atomic::Ordering::Relaxed;
        let words = [add(1, 2, 0), addi(3, 3, 5), jal(0, 0)];

        let mut plain = engine(64);
        let mut bus = TestBus::new(&words);
        let mut h = hart();
        h.x[2] = 0x1234_5678;
        plain.run(&mut h, &mut bus, &HookSet::default(), 3);
        assert_eq!((h.x[1], h.x[3]), (0x1234_5678, 5));
        assert_eq!(plain.cache.block_ops(plain.cache.block(0))[0].kind, K_ADD);
        assert_eq!(plain.stats().fusions, 0, "the default fuser is None");

        MOVE_TO_ADDI.windows.store(0, Relaxed);
        let mut fused = Engine::new(EngineCfg {
            max_block_insns: 64,
            fuser: Some(&MOVE_TO_ADDI),
            ..EngineCfg::default()
        });
        let mut bus = TestBus::new(&words);
        let mut h = hart();
        h.x[2] = 0x1234_5678;
        fused.run(&mut h, &mut bus, &HookSet::default(), 3);

        assert_eq!(MOVE_TO_ADDI.windows.load(Relaxed), 3);
        assert_eq!(MOVE_TO_ADDI.last_len.load(Relaxed), 3);
        // The rewrite is in the op array, and the guest cannot tell.
        let ops = fused.cache.block_ops(fused.cache.block(0));
        assert_eq!(ops[0].kind, K_ADDI);
        assert_eq!(ops[0].pc_off, 0);
        assert_eq!(ops[1].kind, K_ADDI);
        assert_eq!((h.x[1], h.x[3]), (0x1234_5678, 5));
        assert_eq!(fused.stats().fusions, 1);
        assert_eq!(fused.stats().fusions_rejected, 0);
    }

    /// A bus the no-op tier must never touch.
    struct UntouchedBus;

    impl Bus for UntouchedBus {
        fn pages(&self) -> &PageTable {
            unreachable!("no-op tier touched the bus")
        }
        fn arena(&mut self) -> *mut u8 {
            unreachable!("no-op tier touched the bus")
        }
        fn load_slow(&mut self, _a: u32, _s: u8, _h: &HartView) -> Access<u32> {
            unreachable!("no-op tier touched the bus")
        }
        fn store_slow(&mut self, _a: u32, _s: u8, _v: u32, _h: &HartView) -> Access<()> {
            unreachable!("no-op tier touched the bus")
        }
        fn sp_monitor(&self) -> SpMonitor {
            unreachable!("no-op tier touched the bus")
        }
        fn fetch_code(&mut self, _v: u32) -> Result<CodePage<'_>, Trap> {
            unreachable!("no-op tier touched the bus")
        }
        fn csr_custom(&mut self, _c: u16, _o: CsrOp, _i: u64) -> Result<(u32, CsrEffect), Trap> {
            unreachable!("no-op tier touched the bus")
        }
        fn wfi_wake(&mut self) -> bool {
            unreachable!("no-op tier touched the bus")
        }
        fn pmp_changed(&mut self, _c: &Csr) {
            unreachable!("no-op tier touched the bus")
        }
    }

    #[test]
    fn interpreter_only_never_runs_a_block() {
        let mut tier = InterpreterOnly;
        let mut h = hart();
        let before = (h.pc, h.insns);
        assert!(tier.run_hot(h.pc, &mut h, &mut UntouchedBus, 64).is_none());
        assert_eq!((h.pc, h.insns), before);
    }

    /// A tier that claims every block.
    struct AlwaysHot(u32);

    impl ExecTier for AlwaysHot {
        fn run_hot<B: Bus>(
            &mut self,
            _pc: u32,
            _hart: &mut Hart,
            _bus: &mut B,
            _max_insns: u64,
        ) -> Option<Exit> {
            self.0 += 1;
            Some(Exit::Stop)
        }
    }

    #[test]
    fn an_installed_tier_is_consulted_before_the_interpreter() {
        let mut e = Engine::with_tier(cfg(64), AlwaysHot(0));
        let mut bus = TestBus::new(&straight(4));
        let mut h = hart();
        assert_eq!(e.run(&mut h, &mut bus, &HookSet::default(), 4), Exit::Stop);
        assert_eq!(h.insns, 0, "the tier claimed the block");
        assert_eq!(h.pc, BASE);
        assert_eq!(e.tier.0, 1);
    }

    #[test]
    fn new_installs_config_and_interpreter_only() {
        let fuser: &'static dyn OpFuser = &NoFusion;
        let e = Engine::new(EngineCfg {
            max_block_insns: 3,
            strict_csr: true,
            fuser: Some(fuser),
        });
        assert_eq!(e.cfg().max_block_insns, 3);
        assert!(e.cfg().strict_csr);
        assert!(e.cfg().fuser.is_some());
        let _: InterpreterOnly = e.tier;
    }

    #[test]
    fn the_default_config_is_the_documented_engine() {
        let d = EngineCfg::default();
        assert_eq!(d.max_block_insns, DEFAULT_MAX_BLOCK_INSNS);
        assert_eq!(d.max_block_insns, 64);
        assert!(!d.strict_csr);
        assert!(d.fuser.is_none());
    }

    #[test]
    fn engine_and_config_are_send_and_sync() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Option<&'static dyn OpFuser>>();
        send_sync::<EngineCfg>();
        send_sync::<Engine>();
    }

    #[test]
    fn with_tier_installs_the_given_tier() {
        let e = Engine::with_tier(cfg(1), InterpreterOnly);
        assert_eq!(e.cfg().max_block_insns, 1);
        assert!(e.cfg().fuser.is_none());
    }

    #[test]
    fn a_zero_max_block_insns_is_read_as_one() {
        let mut e = engine(0);
        let mut bus = TestBus::new(&straight(4));
        let mut h = hart();
        e.run(&mut h, &mut bus, &HookSet::default(), 2);
        assert_eq!(e.cache.block(0).n_insns, 1);
        assert_eq!(h.insns, 2);
    }

    /// The invariant [`fast_op`]'s unconditional register write rests on.
    #[test]
    fn rule_6_folds_a_write_to_x0_into_k_nop() {
        let computational = [
            ("addi", addi(0, 1, 7)),
            ("add", add(0, 1, 2)),
            ("lui", (0x1234 << 12) | 0x37),
        ];
        for (name, word) in computational {
            let op = decode(word, BASE);
            assert_eq!(op.kind, K_NOP, "{name} x0 must decode to K_NOP");
        }
        // Loads and jumps keep their kind with `rd` 0, and x0 stays 0 when they run.
        let mut e = engine(64);
        let mut bus = TestBus::new(&[lw(0, 3, 0), jal(0, 4), jal(0, 0)]);
        bus.arena[0x40..0x44].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let mut h = hart();
        h.x[3] = BASE + 0x40;
        assert_eq!(
            decode(lw(0, 3, 0), BASE).kind,
            K_LW,
            "a load keeps its kind"
        );
        assert_eq!(decode(jal(0, 4), BASE).kind, K_JAL, "jal keeps its kind");
        e.run(&mut h, &mut bus, &HookSet::default(), 2);
        assert_eq!(
            h.x[0], 0,
            "x0 is still zero after a load and a jump into it"
        );
    }
}
