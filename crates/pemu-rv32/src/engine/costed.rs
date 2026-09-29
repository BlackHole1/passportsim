//! The engine's costed block loop: [`Engine::run`] with a class cost table installed
//! ([`Engine::set_costs`], `crate::cost`). CPU behavior follows the ESP32-C3 TRM chapter 1.
//!
//! With a table, a run's budget is in clock-position units (`Hart::pos`) and the run stops at the
//! first instruction boundary whose position reaches it, so an event is never overshot by an
//! amount that depends on where a slice started. Each op is charged exactly what
//! [`crate::refstep::ref_step_costed`] charges per instruction. `Hart::pipe` moves on after every
//! retired op, so a block entered in the middle charges what one entered at its start would have.
//! The `fast` profile never reaches this module.

use super::{
    Engine, ExecTier, Exit, Fast, HaltCause, HookSet, Left, TRAP_LOOP_LIMIT, fast_op, fast_term,
    fetch_op, slot_of,
};
use crate::bus::Bus;
use crate::cache::{NO_BLOCK, SLOT_FALL, SLOT_TAKEN};
use crate::cost::{self, Bank, InsnCosts, Pipe};
use crate::exec::{Hart, Stepped, exec_op};
use crate::op::{self, F_LOAD, F_STORE, F_TERM, Op};
use crate::trap::take_exception;

/// How the op loop of one block entry ended.
enum Ran {
    /// The position reached the target before op `index`, which did not run; `Hart::pc` is its
    /// PC.
    Budget(u32),
    /// The terminator completed; `Hart::pc` is the successor, reached through chain slot `slot`.
    End(usize),
    /// Op `index` ended the block early: a trap, a bus stop, a stack spill, `wfi`, `fence.i`, a
    /// hook.
    Stopped(usize, Stepped),
}

/// The extras of `op` at `pc` that do not need its result, the MMIO row of its data address
/// included, with the bank rule's state after it. A synthetic op is 0.
#[inline(always)]
fn pre_extra<B: Bus>(costs: &InsnCosts, hart: &Hart, bus: &B, pc: u32, op: &Op) -> (u64, Bank) {
    let (mut extra, bank) = entry_extra(costs, hart, pc, op);
    if op.flags & (F_LOAD | F_STORE) != 0 {
        let a = hart.x[(op.rs1 & 31) as usize];
        extra += cost::mmio_extra(costs, bus, op, cost::data_addr(op, a));
    }
    (u64::from(extra), bank)
}

/// The extras that the decode, `Hart::pipe` and the source registers decide, read before the op
/// writes `rd`, with the bank rule's state after the op.
#[inline(always)]
fn entry_extra(costs: &InsnCosts, hart: &Hart, pc: u32, op: &Op) -> (u32, Bank) {
    let a = hart.x[(op.rs1 & 31) as usize];
    let (bank_extra, bank) = cost::bank_step(costs, hart.pipe, pc, op, a);
    (
        cost::entry_extra(costs, hart.pipe, pc, op)
            + bank_extra
            + cost::div_extra(costs, op.kind, a, hart.x[(op.rs2 & 31) as usize]),
        bank,
    )
}

/// Runs `op` through `exec_op` and charges it when it retired: the step `ref_step_costed` takes.
#[inline(never)]
pub(super) fn exec_costed<B: Bus>(
    hart: &mut Hart,
    bus: &mut B,
    costs: &InsnCosts,
    block_pc: u32,
    op: &Op,
    strict_csr: bool,
) -> Stepped {
    let pc = block_pc.wrapping_add(u32::from(op.pc_off));
    let (extra, bank) = pre_extra(costs, hart, bus, pc, op);
    let taken = cost::branch_taken(
        op.kind,
        hart.x[(op.rs1 & 31) as usize],
        hart.x[(op.rs2 & 31) as usize],
    );
    let insns = hart.insns;
    let stepped = exec_op(hart, bus, block_pc, op, strict_csr);
    if hart.insns != insns {
        hart.extra += extra + u64::from(cost::branch_extra(costs, op.kind, taken));
        hart.pipe = Pipe::after(op, taken, bank);
    }
    stepped
}

/// The ops of one block entry from `from`, charged op by op, until the position reaches `target`,
/// the terminator completes or an op ends the block ([`Ran`]).
#[allow(
    clippy::too_many_arguments,
    reason = "the hot loop's state, passed as the uncosted `run_ops` passes it, not bundled into \
              a struct the always-inlined body would have to take apart again"
)]
#[inline(always)]
fn run_costed_ops<B: Bus, const ARMED: bool>(
    hart: &mut Hart,
    bus: &mut B,
    costs: &InsnCosts,
    block_pc: u32,
    ops: &[Op],
    from: usize,
    target: u64,
    strict_csr: bool,
) -> Ran {
    for (index, op) in ops.iter().enumerate().skip(from) {
        let pc = block_pc.wrapping_add(u32::from(op.pc_off));
        if hart.pos() >= target {
            hart.pc = pc;
            return Ran::Budget(index as u32);
        }
        if op.flags & F_TERM == 0 {
            // An access the fast path serves is to an arena page, and a `PF_MMIO` page carries
            // neither `PF_R` nor `PF_W`, so its MMIO row is 0. An MMIO access misses and
            // `exec_costed` charges it whole.
            let (extra, bank) = entry_extra(costs, hart, pc, op);
            let extra = u64::from(extra);
            match fast_op::<B, ARMED>(hart, bus, op) {
                Fast::Retired => {
                    hart.insns += 1;
                    hart.stores += u64::from(op::is_store(op.kind));
                    hart.extra += extra;
                    hart.pipe = Pipe::after(op, false, bank);
                }
                Fast::Spilled(spill) => {
                    hart.insns += 1;
                    hart.stores += u64::from(op::is_store(op.kind));
                    hart.extra += extra;
                    hart.pipe = Pipe::after(op, false, bank);
                    hart.pc = pc.wrapping_add(u32::from(op.len));
                    return Ran::Stopped(index, Stepped::Spilled(spill));
                }
                Fast::Miss => match exec_costed(hart, bus, costs, block_pc, op, strict_csr) {
                    Stepped::Retired => {}
                    other => return Ran::Stopped(index, other),
                },
            }
            continue;
        }
        let (extra, bank) = pre_extra(costs, hart, bus, pc, op);
        let taken = cost::branch_taken(
            op.kind,
            hart.x[(op.rs1 & 31) as usize],
            hart.x[(op.rs2 & 31) as usize],
        );
        return match fast_term::<ARMED>(hart, op) {
            Some((slot, retired)) => {
                if retired != 0 {
                    hart.insns += retired;
                    hart.extra += extra + u64::from(cost::branch_extra(costs, op.kind, taken));
                    hart.pipe = Pipe::after(op, taken, bank);
                }
                Ran::End(slot)
            }
            None => match exec_costed(hart, bus, costs, block_pc, op, strict_csr) {
                Stepped::Retired => Ran::End(slot_of(hart, op)),
                other => Ran::Stopped(index, other),
            },
        };
    }
    unreachable!("every block ends in a terminator")
}

impl<T: ExecTier> Engine<T> {
    /// [`Engine::run`] with a cost table installed; `budget` is in clock-position units. Never
    /// inlined, so the uncosted `run` keeps its own shape.
    #[inline(never)]
    pub(super) fn run_costed_top<B: Bus>(
        &mut self,
        hart: &mut Hart,
        bus: &mut B,
        hooks: &HookSet,
        budget: u64,
        costs: InsnCosts,
    ) -> Exit {
        let target = hart.pos().saturating_add(budget);
        if self.skip_hook_at == Some(hart.pc) {
            if self.fetch_stalls(hart, bus) {
                return Exit::Stop;
            }
            self.skip_hook_at = None;
            self.resume = None;
            if let Some(exit) = self.step_past_hook_costed(hart, bus, &costs) {
                return exit;
            }
            if hart.pos() >= target {
                return Exit::Budget;
            }
        }
        match (hart.spmon.armed(), self.fetch_watch.is_some()) {
            (false, false) => self.run_costed::<B, false, false>(hart, bus, hooks, target, &costs),
            (false, true) => self.run_costed::<B, false, true>(hart, bus, hooks, target, &costs),
            (true, false) => self.run_costed::<B, true, false>(hart, bus, hooks, target, &costs),
            (true, true) => self.run_costed::<B, true, true>(hart, bus, hooks, target, &costs),
        }
    }

    /// `Engine::step_past_hook` with the instruction charged.
    fn step_past_hook_costed<B: Bus>(
        &mut self,
        hart: &mut Hart,
        bus: &mut B,
        costs: &InsnCosts,
    ) -> Option<Exit> {
        let pc = hart.pc;
        let op = match fetch_op(bus, pc) {
            Ok(mut op) => {
                op.pc_off = 0;
                op
            }
            Err(trap) => {
                take_exception(hart, pc, trap);
                return None;
            }
        };
        match exec_costed(hart, bus, costs, pc, &op, self.cfg.strict_csr) {
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
            Stepped::Hooked(hook) => Some(Exit::Hook {
                id: super::HookId(hook),
            }),
        }
    }

    /// The costed block loop, monomorphised on the stack-monitor state and the fetch watch as
    /// `Engine::run_blocks` is. `target` is the position the run stops at.
    fn run_costed<B: Bus, const ARMED: bool, const WATCH: bool>(
        &mut self,
        hart: &mut Hart,
        bus: &mut B,
        hooks: &HookSet,
        target: u64,
        costs: &InsnCosts,
    ) -> Exit {
        let strict = self.cfg.strict_csr;
        if WATCH && self.fetch_stalls(hart, bus) {
            return Exit::Stop;
        }
        let (mut id, mut op_index) = self.enter(hart, bus, hooks);
        let mut idle_blocks = 0u32;
        let mut insns_at_entry = hart.insns;
        let mut block_execs = 0u64;
        let mut chain_hits = 0u64;
        let exit = 'run: loop {
            let block = *self.cache.block(id);
            block_execs += 1;
            let ran = {
                let ops = self.cache.block_ops(&block);
                run_costed_ops::<B, ARMED>(
                    hart,
                    bus,
                    costs,
                    block.pc,
                    ops,
                    op_index as usize,
                    target,
                    strict,
                )
            };
            op_index = 0;
            match ran {
                Ran::Budget(index) => {
                    if index > 0 {
                        self.stats.partial_blocks += 1;
                    }
                    self.set_resume(id, index, hart.pc);
                    break 'run Exit::Budget;
                }
                Ran::End(slot) => {
                    if hart.pos() >= target {
                        break 'run Exit::Budget;
                    }
                    let linked = if slot == SLOT_TAKEN {
                        block.next[SLOT_TAKEN]
                    } else {
                        block.next[SLOT_FALL]
                    };
                    if linked != NO_BLOCK && self.cache.block(linked).pc == hart.pc {
                        chain_hits += 1;
                        id = linked;
                        continue 'run;
                    }
                    if WATCH && self.fetch_stalls(hart, bus) {
                        break 'run Exit::Stop;
                    }
                    let generation = self.cache.generation();
                    let next = self.lookup(hart.pc, bus, hooks);
                    if self.cache.generation() == generation
                        && (!WATCH
                            || self
                                .fetch_watch
                                .is_none_or(|w| w.may_chain(block.pc, hart.pc)))
                    {
                        self.cache.link(id, slot, next);
                    }
                    id = next;
                }
                Ran::Stopped(index, stepped) => match self.leave(hart, bus, id, index, stepped) {
                    Left::Exit(exit) => break 'run exit,
                    Left::Continue => {
                        idle_blocks = if hart.insns == insns_at_entry {
                            idle_blocks + 1
                        } else {
                            0
                        };
                        insns_at_entry = hart.insns;
                        if idle_blocks > TRAP_LOOP_LIMIT {
                            break 'run Exit::Halted(HaltCause::NoProgress { pc: hart.pc });
                        }
                        if hart.pos() >= target {
                            break 'run Exit::Budget;
                        }
                        if WATCH && self.fetch_stalls(hart, bus) {
                            break 'run Exit::Stop;
                        }
                        id = self.lookup(hart.pc, bus, hooks);
                    }
                },
            }
        };
        self.stats.block_execs += block_execs;
        self.stats.chain_hits += chain_hits;
        exit
    }
}
