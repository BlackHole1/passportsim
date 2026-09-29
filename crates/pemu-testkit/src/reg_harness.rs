//! `RegHarness`: the model-test harness (a `Cx` plus `MockBoard`, no machine). It holds exactly
//! what a peripheral model may touch: virtual time moved only by the test, a [`Scheduler`] (so a
//! model is tested for the event it scheduled, not a sleep), an [`IrqStub`] and a
//! [`MockBoard`]. With no hart and no bus, a failure names the model rather than the run loop.
//!
//! [`RegHarness::assert_field`] and [`check_field`] look a field up by name in its `RegSpec` and
//! report block, register, field, expected and read value; the generated per-block tests and the
//! busy-wait row tests use them.

use core::fmt;

use pemu_core::irq_source::{IrqSource, SOURCE_COUNT, SOURCES};
use pemu_core::regstore::{FieldSpec, RegSpec, RegStore};
use pemu_core::sched::{EventKey, Scheduler};
use pemu_core::time::VTime;

use crate::mock_board::MockBoard;

/// A register block a field assertion can read: a generated `RegStore` or any table shaped like
/// one. `RegStore<N>` is generic over its register count, hence this object-safe view.
pub trait RegBlock {
    fn reg_count(&self) -> usize;
    fn spec_at(&self, idx: usize) -> &RegSpec;
    /// Stored value of register `idx`, with no read side effect (including WO and WT bits).
    fn stored_at(&self, idx: usize) -> u32;

    fn index_of_name(&self, name: &str) -> Option<usize> {
        (0..self.reg_count()).find(|&idx| self.spec_at(idx).name == name)
    }
}

impl<const N: usize> RegBlock for RegStore<N> {
    fn reg_count(&self) -> usize {
        N
    }

    fn spec_at(&self, idx: usize) -> &RegSpec {
        RegStore::spec(self, idx)
    }

    fn stored_at(&self, idx: usize) -> u32 {
        RegStore::get(self, idx)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldError {
    NoRegister {
        block: String,
        reg: String,
        /// Every register the block does have, in table order.
        known: Vec<&'static str>,
    },
    NoField {
        block: String,
        reg: &'static str,
        field: String,
        /// Every field the register does have, in spec order.
        known: Vec<&'static str>,
    },
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldError::NoRegister { block, reg, known } => write!(
                f,
                "{block} has no register {reg}; it has {}",
                name_list(known)
            ),
            FieldError::NoField {
                block,
                reg,
                field,
                known,
            } => write!(
                f,
                "{block}.{reg} has no field {field}; it has {}",
                name_list(known)
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldMismatch {
    /// Block name as the caller wrote it; the `RegSpec` tables carry none.
    pub block: String,
    pub reg: &'static str,
    pub field: &'static str,
    pub shift: u8,
    pub width: u8,
    /// Already right-shifted to bit 0.
    pub expected: u32,
    pub actual: u32,
}

impl fmt::Display for FieldMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{} (bits {}..{}): expected {:#x}, got {:#x}",
            self.block,
            self.reg,
            self.field,
            self.shift,
            self.shift + self.width - 1,
            self.expected,
            self.actual
        )
    }
}

fn name_list(names: &[&str]) -> String {
    if names.is_empty() {
        return "none".to_string();
    }
    names.join(", ")
}

/// The mask of a field at bit 0. Width 32 is shifted in two steps: `1u32 << 32` overflows.
fn field_mask(field: &FieldSpec) -> u32 {
    if field.width >= 32 {
        u32::MAX
    } else {
        (1u32 << field.width) - 1
    }
}

/// A field's **stored** value by name, right-shifted to bit 0, so a WO or WT field can be asserted
/// and an assertion never fires a read side effect.
pub fn read_field(
    block: &str,
    regs: &dyn RegBlock,
    reg: &str,
    field: &str,
) -> Result<u32, FieldError> {
    let Some(idx) = regs.index_of_name(reg) else {
        return Err(FieldError::NoRegister {
            block: block.to_string(),
            reg: reg.to_string(),
            known: (0..regs.reg_count())
                .map(|i| regs.spec_at(i).name)
                .collect(),
        });
    };
    let spec = regs.spec_at(idx);
    let Some(spec_field) = spec.fields.iter().find(|f| f.name == field) else {
        return Err(FieldError::NoField {
            block: block.to_string(),
            reg: spec.name,
            field: field.to_string(),
            known: spec.fields.iter().map(|f| f.name).collect(),
        });
    };
    Ok((regs.stored_at(idx) >> spec_field.shift) & field_mask(spec_field))
}

/// `Ok(())` when equal, `Err(Ok(mismatch))` when not, and `Err(Err(error))` when the name does not
/// resolve.
#[allow(clippy::result_large_err)]
pub fn check_field(
    block: &str,
    regs: &dyn RegBlock,
    reg: &str,
    field: &str,
    expected: u32,
) -> Result<(), Result<FieldMismatch, FieldError>> {
    let actual = read_field(block, regs, reg, field).map_err(Err)?;
    if actual == expected {
        return Ok(());
    }
    let idx = regs.index_of_name(reg).expect("register resolved above");
    let spec = regs.spec_at(idx);
    let spec_field = spec
        .fields
        .iter()
        .find(|f| f.name == field)
        .expect("field resolved above");
    Err(Ok(FieldMismatch {
        block: block.to_string(),
        reg: spec.name,
        field: spec_field.name,
        shift: spec_field.shift,
        width: spec_field.width,
        expected,
        actual,
    }))
}

/// Asserts a field by name, panicking with the block, the register, the field and both values.
#[track_caller]
pub fn assert_field(block: &str, regs: &dyn RegBlock, reg: &str, field: &str, expected: u32) {
    match check_field(block, regs, reg, field, expected) {
        Ok(()) => {}
        Err(Ok(mismatch)) => panic!("{mismatch}"),
        Err(Err(error)) => panic!("{error}"),
    }
}

/// The interrupt-source stub. The real `IrqFabric` routes sources to CPU lines; here a model test
/// asserts only **which source the model drove and when**.
#[derive(Clone, Debug)]
pub struct IrqStub {
    level: [bool; SOURCE_COUNT],
    changes: Vec<IrqChange>,
}

impl Default for IrqStub {
    fn default() -> IrqStub {
        IrqStub {
            level: [false; SOURCE_COUNT],
            changes: Vec::new(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IrqChange {
    pub t: VTime,
    pub source: IrqSource,
    pub level: bool,
}

impl IrqStub {
    pub fn new() -> IrqStub {
        IrqStub::default()
    }

    /// Drives a source. Only a change is recorded, as in the fabric.
    pub fn set_source(&mut self, t: VTime, source: IrqSource, level: bool) {
        let idx = usize::from(source.0);
        if self.level[idx] == level {
            return;
        }
        self.level[idx] = level;
        self.changes.push(IrqChange { t, source, level });
    }

    pub fn level(&self, source: IrqSource) -> bool {
        self.level[usize::from(source.0)]
    }

    pub fn changes(&self) -> &[IrqChange] {
        &self.changes
    }

    pub fn high(&self) -> Vec<IrqSource> {
        self.level
            .iter()
            .enumerate()
            .filter(|(_, high)| **high)
            .map(|(n, _)| IrqSource(n as u8))
            .collect()
    }

    /// The generated name of a source (`specs/irq-sources.toml`), for a report.
    pub fn source_name(source: IrqSource) -> &'static str {
        SOURCES[usize::from(source.0)].name
    }

    pub fn clear_changes(&mut self) {
        self.changes.clear();
    }
}

/// What a peripheral model may reach while a harness test drives it.
#[derive(Debug)]
pub struct HarnessCx<'a> {
    /// Virtual time of the access.
    pub now: VTime,
    pub sched: &'a mut Scheduler,
    pub irq: &'a mut IrqStub,
    pub board: &'a mut MockBoard,
}

impl HarnessCx<'_> {
    pub fn schedule(&mut self, at: VTime, key: EventKey) -> pemu_core::sched::EventHandle {
        self.sched.schedule(self.now, at, key)
    }

    /// Drives an interrupt source at the context's time.
    pub fn set_irq(&mut self, source: IrqSource, level: bool) {
        self.irq.set_source(self.now, source, level);
    }
}

/// A `Cx` with a scheduler, an IRQ fabric stub and a `MockBoard`, and no machine.
#[derive(Debug, Default)]
pub struct RegHarness {
    /// Virtual time the next context carries, moved by the `advance_*` methods, never by a host
    /// clock.
    pub now: VTime,
    pub sched: Scheduler,
    pub irq: IrqStub,
    pub board: MockBoard,
}

impl RegHarness {
    pub fn new() -> RegHarness {
        RegHarness::default()
    }

    pub fn with_board(board: MockBoard) -> RegHarness {
        RegHarness {
            board,
            ..RegHarness::default()
        }
    }

    pub fn cx(&mut self) -> HarnessCx<'_> {
        HarnessCx {
            now: self.now,
            sched: &mut self.sched,
            irq: &mut self.irq,
            board: &mut self.board,
        }
    }

    /// Moves virtual time to `t` and returns every due event in `(time, seq)` order. Time never
    /// moves backwards, and events already due at [`RegHarness::now`] are still delivered, which
    /// [`RegHarness::advance_to_next_event`] relies on.
    pub fn advance_to(&mut self, t: VTime) -> Vec<EventKey> {
        if t > self.now {
            self.now = t;
        }
        let mut due = Vec::new();
        while let Some(key) = self.sched.pop_due(self.now) {
            due.push(key);
        }
        due
    }

    /// Moves virtual time forward by `ps` picoseconds, saturating.
    pub fn advance_by(&mut self, ps: u64) -> Vec<EventKey> {
        self.advance_to(VTime(self.now.0.saturating_add(ps)))
    }

    /// Moves virtual time to the next scheduled event and returns what came due; `None` when
    /// nothing is pending.
    pub fn advance_to_next_event(&mut self) -> Option<Vec<EventKey>> {
        let next = self.sched.next_time()?;
        Some(self.advance_to(next))
    }

    /// [`assert_field`] against this harness's virtual time in the panic message.
    #[track_caller]
    pub fn assert_field(
        &self,
        block: &str,
        regs: &dyn RegBlock,
        reg: &str,
        field: &str,
        expected: u32,
    ) {
        match check_field(block, regs, reg, field, expected) {
            Ok(()) => {}
            Err(Ok(mismatch)) => panic!("{mismatch} at vt {} ps", self.now.0),
            Err(Err(error)) => panic!("{error}"),
        }
    }
}
