//! ROM delay fast-forward.
//!
//! The rev101 `ets_delay_us` busy-waits on the cycle counter:
//!
//! ```text
//! 40047e9e: csrr a5,0x802 ; sub a5,a5,a4 ; bltu a5,a0,40047e9e
//! ```
//!
//! with `a4` the count at entry and `a0` the delay in cycles. Each three-instruction iteration
//! changes only `a5`, so the iteration count is a function of the clock. The head is pinned in
//! `specs/rom-pins.toml` per ROM SHA-256; another ROM gets no hook.
//!
//! At the head, [`Machine::rom_delay_step`] finds the largest `k` such that every iteration takes
//! the branch (`d0 + (cc(i + 3j) - cc(i)) < a0` for `j < k`, compared unwrapped, which can only
//! shrink `k`) and the `3k` instructions end at or before the boundary where the next event,
//! journal input or limit would stop an unforwarded slice. It credits `3k` instructions and sets
//! `a5` to the last skipped `sub`'s result. The pc stays at the head, so the exiting iteration
//! runs on the same instruction as without the shortcut.
//!
//! The shortcut stands aside while a breakpoint or another hook sits on the loop, since those must
//! see every iteration.

use pemu_hle::hooks::{FfKind, HookKind, HookRef};
use pemu_loader::rom::RomImage;
use pemu_rv32::engine::HookId;

use crate::machine::Machine;
use crate::run::RunLimits;

const ROM_PINS_TOML: &str = include_str!("../../../specs/rom-pins.toml");

const DELAY_HOOK_NAME: &str = "FastForward(RomDelayLoop)";

/// Bytes the loop spans after its head: `csrr`, `sub`, `bltu`.
const LOOP_SPAN: u32 = 8;

const ITERATION: u64 = 3;

/// Bound for a hit with nothing else bounding it (a stopped counter and no limit): the loop would
/// spin forever unforwarded too, so this only keeps the arithmetic finite.
const MAX_ITERATIONS: u64 = 1 << 32;

const A0: usize = 10;
const A4: usize = 14;
const A5: usize = 15;

pub fn delay_hook_id() -> HookId {
    HookRef::core(HookKind::FastForward(FfKind::RomDelayLoop)).to_id()
}

/// The delay loop head pinned for this ROM in `specs/rom-pins.toml`, if the image holds the
/// pinned instruction there; `None` for any other ROM.
pub fn pinned_delay_head(rom: &RomImage) -> Option<u32> {
    let key: String = rom
        .elf_sha256()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut in_section = false;
    for line in ROM_PINS_TOML.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("[rom.") {
            in_section = rest.trim_end_matches(']') == key;
            continue;
        }
        if !in_section || !line.contains(DELAY_HOOK_NAME) {
            continue;
        }
        let pc = hex_field(line, "pc")?;
        let insn = hex_field(line, "insn")?;
        let bytes = rom.read(pc, 4)?;
        let held = u32::from_le_bytes(bytes.try_into().ok()?);
        return (held == insn).then_some(pc);
    }
    None
}

fn hex_field(line: &str, name: &str) -> Option<u32> {
    let at = line.find(&format!("{name} = 0x"))? + name.len() + 5;
    let digits: String = line[at..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    u32::from_str_radix(&digits, 16).ok()
}

/// Derived from the ROM and the setter, so not a snapshot section.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct RomDelay {
    pub(crate) head: Option<u32>,
    /// On by default: results do not depend on it.
    pub(crate) enabled: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum DelayStep {
    NotHere,
    Skipped,
    Execute,
}

impl Machine {
    /// One iteration in clock-position units, `q = 3 + taken_branch + split`, and the split
    /// redirect `a` the first iteration skips when the hart fell into the head rather than
    /// branching back; `(3, 0)` without a cost table. `k` iterations credit `3k` instructions and
    /// `k x q - a - 3k` extra cycles.
    fn delay_iteration(&self, head: u32) -> (u64, u64) {
        let Some(c) = self.engine.costs() else {
            return (ITERATION, 0);
        };
        // The head is a 32-bit `csrr`; a redirect into it is split at 2 mod 4 (0x40047e9e is).
        let split = if head & 2 != 0 {
            u64::from(c.split_redirect)
        } else {
            0
        };
        let q = ITERATION + u64::from(c.taken_branch) + split;
        let a = if self.hart.pipe.redirect { 0 } else { split };
        (q, a)
    }

    pub fn set_rom_delay_ff(&mut self, on: bool) {
        self.rom_delay.enabled = on;
        let Some(head) = self.rom_delay.head else {
            return;
        };
        let id = delay_hook_id();
        if on {
            if self.hooks.get(head).is_none() {
                self.hooks.insert(head, id);
            }
        } else if self.hooks.get(head) == Some(id) {
            self.hooks.remove(head);
        }
    }

    pub fn rom_delay_ff(&self) -> bool {
        self.rom_delay.enabled && self.rom_delay.head.is_some()
    }

    pub(crate) fn rom_delay_step(&mut self, lim: &RunLimits, insns_at_start: u64) -> DelayStep {
        let Some(head) = self.rom_delay.head else {
            return DelayStep::NotHere;
        };
        if !self.rom_delay.enabled || self.hart.pc != head {
            return DelayStep::NotHere;
        }
        if lim
            .stops
            .breakpoints
            .iter()
            .any(|&b| (head..=head + LOOP_SPAN).contains(&b))
        {
            return DelayStep::Execute;
        }
        // Any other hook on the loop must see every iteration too, or its fire count would depend
        // on the shortcut. Instructions start on even addresses.
        let delay = delay_hook_id();
        if (head..=head + LOOP_SPAN).step_by(2).any(|pc| {
            self.hle.core.section.user_hooks.contains_key(&pc)
                || self.hooks.get(pc).is_some_and(|id| id != delay)
        }) {
            return DelayStep::Execute;
        }
        let start = self.hart.x[A4];
        let target = u64::from(self.hart.x[A0]);
        let i = self.hart.pos();
        let (q, a) = self.delay_iteration(head);
        let at = |j: u64| if j == 0 { i } else { i + q * j - a };
        let cc0 = self.clock.cycle_count(i);
        let d0 = u64::from((cc0 as u32).wrapping_sub(start));
        if d0 >= target {
            return DelayStep::Execute;
        }

        let now = self.now();
        let mut kmax = match self.next_wake(lim) {
            Some(t) if t <= now => 0,
            Some(t) => (self.clock.insns_until(i, t) + a) / q,
            None => MAX_ITERATIONS,
        };
        if let Some(max) = lim.max_insns {
            kmax = kmax.min(max.saturating_sub(self.budget_spent(insns_at_start)) / ITERATION);
        }
        kmax = kmax.min(MAX_ITERATIONS);

        // Largest k in 0..=kmax whose k iterations all branch back; monotone in k.
        let continues = |m: &Machine, j: u64| d0 + (m.clock.cycle_count(at(j)) - cc0) < target;
        let (mut lo, mut hi) = (0u64, kmax);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if continues(self, mid - 1) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let k = lo;
        if k == 0 {
            return DelayStep::Execute;
        }
        debug_assert!(
            q * k >= a + ITERATION * k,
            "an iteration costs its instructions"
        );
        let last = self.clock.cycle_count(at(k - 1)) as u32;
        self.hart.x[A5] = last.wrapping_sub(start);
        self.hart.insns += ITERATION * k;
        self.hart.extra += q * k - a - ITERATION * k;
        if self.engine.costs().is_some() {
            // The last skipped iteration ended in the taken `bltu`. ROM code is outside the SRAM
            // bank rule's block, so the bank state is the default.
            self.hart.pipe = pemu_rv32::cost::Pipe {
                redirect: true,
                load_rd: 0,
                bank: Default::default(),
            };
        }
        self.ff_insns += ITERATION * k;
        self.poll.invalidate();
        DelayStep::Skipped
    }
}
