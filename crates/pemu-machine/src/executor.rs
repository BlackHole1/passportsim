//! The executor behind the run loop: the block engine, or the uncached reference interpreter.
//!
//! After every engine call, [`Machine::drain_invalidations`] hands the engine the pages the SoC
//! invalidated (a store into translated code, an MMU entry write, a flash program). Nothing moves
//! the other way: every translated instruction is fetched through `Bus::fetch_code`, which marks
//! its page `PF_CODE`, so a store into it takes the slow path even inside the same call.
//!
//! [`Executor::Reference`] runs `ref_step`, one instruction per call, and is kept reachable as
//! the oracle the determinism harness compares the engine against. It cannot report a
//! stack-guard violation, because `StepResult` has no variant for one.

use pemu_rv32::bus::Bus;
use pemu_rv32::engine::{Engine, EngineCfg, Exit};
use pemu_rv32::refstep::{StepResult, ref_step_costed};

use pemu_soc_c3::wiring::mmu::{self as mmu_wiring, BlockCache};

use crate::machine::Machine;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Executor {
    #[default]
    Engine,
    Reference,
}

const PAGE: u32 = 4096;

/// A fresh engine for `cfg`. `EngineCfg` is not `Clone` (its fuser is a reference the machine
/// sets), so the fields are copied one by one.
pub(crate) fn engine_for(cfg: &EngineCfg) -> Engine {
    Engine::new(EngineCfg {
        max_block_insns: cfg.max_block_insns,
        strict_csr: cfg.strict_csr,
        fuser: cfg.fuser,
    })
}

struct EngineCache<'a>(&'a mut Engine);

impl BlockCache for EngineCache<'_> {
    fn invalidate_page(&mut self, vpn: u32) {
        self.0.invalidate_vrange(vpn << 12, PAGE);
    }
}

impl Machine {
    /// Runs the executor for at most `budget` (>= 1) instructions, then drains invalidations. The
    /// reference path executes exactly one instruction whatever the budget.
    pub(crate) fn execute(&mut self, budget: u64) -> Exit {
        debug_assert!(budget >= 1, "the loop never asks for an empty slice");
        // A run-past request is for the pc it was made at; a hart that moved (an interrupt taken
        // at the loop head, a reset) drops it, and reaching the pc again is a new fetch.
        let pc = self.hart.pc;
        let skip = self.hle.state.run_past_at.take() == Some(pc);
        let insns = self.hart.insns;
        let exit = match self.executor {
            Executor::Engine => self.with_bus_and_engine(|bus, hart, engine, hooks| {
                if skip {
                    // The engine forgets a continue request when it sees a changed `HookSet`, which
                    // it checks at the start of a run; a zero-instruction run takes the change
                    // first.
                    engine.run(hart, bus, hooks, 0);
                    engine.continue_past_hook(pc);
                }
                engine.run(hart, bus, hooks, budget)
            }),
            Executor::Reference => {
                // The engine ends a slice at a hooked pc before the instruction executes; answer
                // the same exit so hook dispatch matches on both executors.
                if !skip
                    && self.hooks.page_has_hooks(pc)
                    && let Some(id) = self.hooks.get(pc)
                {
                    return Exit::Hook { id };
                }
                // Report the fetch the engine reports through `FetchWatch`, so both paths produce
                // the same sequence of touches.
                if let Some(watch) = self.engine.fetch_watch()
                    && watch.contains(pc)
                    && self.with_bus(|bus, hart| bus.fetch_enter(hart.pos(), pc))
                {
                    Exit::Stop
                } else {
                    // The engine's class cost table is the profile's, so both executors charge the
                    // same position.
                    let costs = self.engine.costs().unwrap_or_default();
                    match self.with_bus(|bus, hart| ref_step_costed(hart, bus, &costs)) {
                        StepResult::Wfi => Exit::Wfi,
                        StepResult::Retired | StepResult::Trapped(_) => Exit::Budget,
                    }
                }
            }
        };
        // A fetch that stalled before the run-past instruction did not run it: keep the request.
        if skip && exit == Exit::Stop && (self.hart.pc, self.hart.insns) == (pc, insns) {
            self.hle.state.run_past_at = Some(pc);
        }
        self.drain_invalidations();
        exit
    }

    pub(crate) fn drain_invalidations(&mut self) -> usize {
        mmu_wiring::drain_invalidations(&mut self.soc, &mut EngineCache(&mut self.engine))
    }

    pub(crate) fn continue_past_hook(&mut self, pc: u32) {
        self.hle.state.run_past_at = Some(pc);
    }

    /// Selects the executor. Switching drops every translated block, because the reference path
    /// does not drain invalidations into a cache it does not run.
    pub fn set_executor(&mut self, executor: Executor) {
        if executor != self.executor {
            self.engine.flush();
            self.executor = executor;
        }
    }

    pub fn executor(&self) -> Executor {
        self.executor
    }

    pub fn engine_stats(&self) -> pemu_rv32::engine::EngineStats {
        self.engine.stats()
    }
}
