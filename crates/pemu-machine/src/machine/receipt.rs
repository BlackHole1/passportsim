//! The machine's receipt: fidelity lists, fault counters and run identity.

use pemu_core::fidelity::FirstTouch;

use super::Machine;

impl Machine {
    /// Drains ledger deltas since the last call. The fidelity fields still cover the whole run:
    /// `caveats` feeds the verdict and exit code, so as a delta a class-U touch would land in
    /// whichever receipt came next.
    pub fn receipt(&mut self) -> Receipt {
        self.receipt_cursor = self.ledger.cursor();
        let (classes_touched, unmodeled_first_touch) = self.fidelity_lists();
        let mut binding = self.hle.core.bound.record.clone();
        for host in &self.hle.hosts {
            let state = self
                .hle
                .state
                .modules
                .get(host.name())
                .map_or(&[][..], Vec::as_slice);
            if let Some(lines) = host.log_lines(state) {
                binding.log_lines.insert(host.name().to_string(), lines);
            }
        }
        Receipt {
            binding: Some(binding),
            fault_counters: Some(FaultCounters {
                dma_faults: self.dma_faults,
                pcm_width_faults: self.pcm_width_faults,
            }),
            determinism: Some(self.journal.class()),
            profile: Some(self.cfg.profile),
            // Read from the machine, never assumed by the command layer.
            efuse: Some(self.cfg.efuse),
            // `pemu_host::boot_cache` selects a tainted machine's store from this field, so a
            // default of `false` would put a device's secrets on disk.
            tainted: self.is_tainted(),
            classes_touched,
            unmodeled_first_touch,
            // Read, not drained: `St7789p3::take_warnings` has its own caller.
            timing_lint: self
                .board
                .lcd
                .warnings()
                .iter()
                .map(|w| format!("{}.{:#04x}", lint_name(w.lint), w.cmd))
                .collect(),
            cpi_milli: Some(self.clock.cpi_milli()),
            // The journal is part of run identity.
            journal_len: Some(self.journal.entries().len() as u64),
        }
    }

    /// The two fidelity lists, built together from one per-touch class: a note the run recorded
    /// for the register, else the model's claim (`class` column of `specs/blocks/<block>.toml`),
    /// else `U`. Never written back into the ledger: it is hashed, so noting from a receipt would
    /// make `state_hash` depend on how many receipts were taken.
    fn fidelity_lists(&mut self) -> (ClassesTouched, Vec<String>) {
        let touches: Vec<FirstTouch> = self.ledger.first_touches().to_vec();
        let mut classes = ClassesTouched::default();
        let mut unmodeled = Vec::new();
        let mut elided = 0u64;
        for touch in &touches {
            let noted = self
                .ledger
                .notes()
                .iter()
                .any(|n| n.subject == touch.subject());
            let class = if noted {
                self.ledger.class_of(touch.subject())
            } else {
                self.soc
                    .devices
                    .fidelity_of(touch.periph, touch.off)
                    .unwrap_or(pemu_core::fidelity::Fidelity::U)
            };
            let block = block_name(touch.periph);
            // A noted register is named `block.REGISTER`, the rest summarized at its block, so a
            // partly modeled block appears in both C and U.
            let subject = if noted {
                register_name(block, touch.off)
            } else {
                block.to_string()
            };
            match class {
                pemu_core::fidelity::Fidelity::C => {
                    if !classes.c.contains(&subject) {
                        classes.c.push(subject);
                    }
                }
                pemu_core::fidelity::Fidelity::U if !touch.allowlisted => {
                    if !classes.u.contains(&subject) {
                        classes.u.push(subject);
                    }
                    // The same fact at register granularity, which is why `Receipt::caveats`
                    // gives the two their own kinds.
                    if unmodeled.len() == UNMODELED_LIMIT {
                        elided += 1;
                    } else {
                        unmodeled.push(register_name(block, touch.off));
                    }
                }
                _ => {}
            }
        }
        if elided > 0 {
            unmodeled.push(format!("... {elided} more"));
        }
        (classes, unmodeled)
    }

    /// First touches after ledger cursor `since` (0 is the whole run), and the next cursor.
    pub fn first_touches_since(&self, since: u64) -> (&[FirstTouch], u64) {
        (self.ledger.delta(since), self.ledger.cursor())
    }

    /// Ledger cursor the last [`Machine::receipt`] call drained up to.
    pub fn receipt_cursor(&self) -> u64 {
        self.receipt_cursor
    }
}

/// Most unmodeled first touches a receipt lists before it elides the rest, so thousands cannot
/// fill the response an agent reads.
pub const UNMODELED_LIMIT: usize = 128;

/// The `c3_devices!` name of a block, or a sentinel word for `UNMAPPED` (a hole inside the
/// peripheral window) and `UNBACKED` (an address outside it).
fn block_name(id: pemu_core::sched::PeriphId) -> &'static str {
    if id == pemu_soc_c3::UNBACKED {
        return "unbacked";
    }
    pemu_soc_c3::periph::BLOCKS
        .get(usize::from(id.0))
        .map_or(crate::hang::UNMAPPED_BLOCK, |b| b.name)
}

/// One register of `block` as a receipt names it. A [`FirstTouch`] records a register, not a
/// field, so the name is `block.REGISTER`, or the offset when the table has none.
fn register_name(block: &str, off: u32) -> String {
    match crate::hang::register_at(block, off) {
        Some(reg) => format!("{block}.{}", reg.name),
        None => format!("{block}.{:#06x}", off & !3),
    }
}

/// The stable receipt word for a timing lint.
fn lint_name(lint: pemu_board::st7789::TimingLint) -> &'static str {
    use pemu_board::st7789::TimingLint;
    match lint {
        TimingLint::CommandAfterSlpout => "st7789.command_after_slpout",
        TimingLint::SleepCycleTooFast => "st7789.sleep_cycle_too_fast",
        TimingLint::RamwrWrongColmod => "st7789.ramwr_wrong_colmod",
    }
}

/// Class C and class U subjects a run leaned on. The machine-side twin of
/// `pemu_api::receipt::ClassesTouched`; the command layer maps one to the other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassesTouched {
    /// Class C subjects, in first-touch order, each named once.
    pub c: Vec<String>,
    /// Class U subjects outside the allowlist, in first-touch order, each named once.
    pub u: Vec<String>,
}

/// Ledger deltas drained by `Machine::receipt`; `crates/pemu-api/src/receipt.rs` assembles the
/// public receipt from them. `None` fields come from a machine that has no such source (a mock).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Receipt {
    /// The HLE binding record: profile id, app ELF SHA-256, per-feature status.
    pub binding: Option<crate::hle::HleBindingRecord>,
    /// Fidelity fault counters since the machine was built.
    pub fault_counters: Option<FaultCounters>,
    /// What the journal lets the run claim, raised by the origins of journaled inputs.
    pub determinism: Option<pemu_core::journal::Determinism>,
    /// The timing profile that actually produced this virtual time, read from the machine.
    pub profile: Option<crate::config::TimingProfileId>,
    /// Where the eFuse image came from. Narrower than [`Receipt::tainted`], which also covers
    /// secret-bearing inputs handed to the instance later.
    pub efuse: Option<crate::config::EfuseSource>,
    /// Whether any input carries device-derived bytes; `pemu_host::boot_cache` keeps such
    /// entries off disk. The command layer ORs in what the `pemu-api` secret store taints later.
    pub tainted: bool,
    /// Class C and class U subjects of the whole run, not the drained delta.
    pub classes_touched: ClassesTouched,
    /// Unmodeled first touches named `block.REGISTER`, capped at [`UNMODELED_LIMIT`] with a final
    /// `... N more`. Overlaps [`Receipt::classes_touched`] at a finer grain by design.
    pub unmodeled_first_touch: Vec<String>,
    /// ST7789 timing-lint warnings, named `st7789.<rule>.<command byte>`.
    pub timing_lint: Vec<String>,
    /// Cycles per instruction in thousandths, as the machine's clock holds it.
    pub cpi_milli: Option<u32>,
    /// Input journal entries so far; part of run identity.
    pub journal_len: Option<u64>,
}

/// Data-path faults a run absorbed instead of stopping: guest-visible behavior not reproduced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaultCounters {
    pub dma_faults: u64,
    /// I2S periods refused for a sample width the PCM path does not pack.
    pub pcm_width_faults: u64,
}
