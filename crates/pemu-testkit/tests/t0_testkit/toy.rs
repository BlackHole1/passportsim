//! A toy peripheral for the harness self-tests: the smallest thing that exercises what a real
//! model does against the harness. A `RegStore` with fields of several access kinds; a write
//! trigger completed by a scheduled event rather than a stall; a valid flag set when it fires
//! (the shape of SYSTIMER's VALUE_VALID after an UPDATE write); an interrupt raised on
//! completion; and a board port driven on enable.

use pemu_board::traits::BoardPorts;
use pemu_core::fidelity::Fidelity;
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{FieldAccess, FieldSpec, RESET_BY_ALL_SCOPES, RegSpec, RegStore, Size};
use pemu_core::sched::{EventKey, Owner, PeriphId};
use pemu_core::time::VTime;
use pemu_testkit::reg_harness::HarnessCx;

pub const BLOCK: &str = "TOY";

pub const CONF: usize = 0;
pub const STATUS: usize = 1;

pub const CONF_OFF: u16 = 0x00;
pub const STATUS_OFF: u16 = 0x04;

/// The toy's own scheduler owner.
pub const OWNER: Owner = Owner::Periph(PeriphId(0x709));

/// Event tag of the conversion completion.
pub const TAG_DONE: u16 = 1;

/// Picoseconds one `DIV` step costs; the toy's whole timing model.
pub const PS_PER_DIV: u64 = 1_000_000;

/// The interrupt source the toy raises; any source works, the stub only needs a number.
pub const SOURCE: IrqSource = irq::SYSTIMER_TARGET0;

/// The toy's two registers.
pub static SPECS: [RegSpec; 2] = [
    RegSpec {
        name: "CONF",
        off: CONF_OFF,
        reset: 0x0000_0004,
        fields: &[
            FieldSpec {
                name: "ENABLE",
                shift: 0,
                width: 1,
                access: FieldAccess::Rw,
                reset: 0,
            },
            FieldSpec {
                name: "DIV",
                shift: 2,
                width: 6,
                access: FieldAccess::Rw,
                reset: 1,
            },
            FieldSpec {
                name: "UPDATE",
                shift: 31,
                width: 1,
                access: FieldAccess::Wt,
                reset: 0,
            },
        ],
        domain: RESET_BY_ALL_SCOPES,
        stable_read: true,
        class: Fidelity::B,
        cite: "toy peripheral of the harness self-tests",
    },
    RegSpec {
        name: "STATUS",
        off: STATUS_OFF,
        reset: 0,
        fields: &[
            FieldSpec {
                name: "VALUE_VALID",
                shift: 0,
                width: 1,
                access: FieldAccess::Ro,
                reset: 0,
            },
            FieldSpec {
                name: "VALUE",
                shift: 8,
                width: 8,
                access: FieldAccess::Ro,
                reset: 0,
            },
            FieldSpec {
                name: "DONE",
                shift: 1,
                width: 1,
                access: FieldAccess::W1c,
                reset: 0,
            },
        ],
        domain: RESET_BY_ALL_SCOPES,
        stable_read: true,
        class: Fidelity::B,
        cite: "toy peripheral of the harness self-tests",
    },
];

/// The toy peripheral: a register store and the one value it produces.
pub struct Toy {
    pub regs: RegStore<2>,
    pub next_value: u8,
    /// GPIO the toy drives when `ENABLE` changes.
    pub gpio: u8,
}

impl Default for Toy {
    fn default() -> Toy {
        Toy::new()
    }
}

impl Toy {
    pub fn new() -> Toy {
        Toy {
            regs: RegStore::new(&SPECS),
            next_value: 0x5A,
            gpio: 7,
        }
    }

    /// A 32-bit register read at block offset `off`, with read side effects: a WT bit reads 0.
    pub fn read(&mut self, off: u16) -> u32 {
        match self.regs.index_of(off) {
            Some(idx) => self.regs.read(idx, 0, Size::B4),
            None => 0,
        }
    }

    /// A 32-bit register write at block offset `off`. `UPDATE` (a WT bit) schedules completion
    /// `DIV` steps ahead instead of producing the value now; a change of `ENABLE` drives the
    /// board GPIO, which puts the call in the `MockBoard` log.
    pub fn write(&mut self, cx: &mut HarnessCx<'_>, off: u16, value: u32) {
        let Some(idx) = self.regs.index_of(off) else {
            return;
        };
        let delta = self.regs.write(idx, 0, Size::B4, value);
        if idx != CONF {
            return;
        }
        let was_enabled = delta.before & 1 != 0;
        let is_enabled = delta.after & 1 != 0;
        if was_enabled != is_enabled {
            cx.board.gpio_out(cx.now, self.gpio, is_enabled, true);
        }
        if delta.triggers & (1 << 31) != 0 && is_enabled {
            let div = u64::from((delta.after >> 2) & 0x3F);
            self.regs.set(STATUS, 0);
            cx.schedule(
                VTime(cx.now.0 + div * PS_PER_DIV),
                EventKey {
                    owner: OWNER,
                    tag: TAG_DONE,
                },
            );
        }
    }

    /// The completion event: publish the value, set `VALUE_VALID` and `DONE`, raise the source.
    pub fn on_event(&mut self, cx: &mut HarnessCx<'_>, tag: u16) {
        if tag != TAG_DONE {
            return;
        }
        let value = u32::from(self.next_value);
        self.regs.set(STATUS, (value << 8) | 0b11);
        cx.set_irq(SOURCE, true);
    }

    /// Guest acknowledgement: writing 1 to `DONE` clears it and lowers the source.
    pub fn ack(&mut self, cx: &mut HarnessCx<'_>) {
        self.regs.write(STATUS, 0, Size::B4, 0b10);
        cx.set_irq(SOURCE, false);
    }
}
