//! SENSITIVE: the PMS memory-protection registers (`specs/blocks/sensitive.toml`).
//!
//! With `CONFIG_ESP_SYSTEM_MEMPROT_FEATURE_LOCK` the app programs the split lines, areas and
//! monitors and then sets the lock bits on every boot.
//!
//! Every register, lock bits included, is restored by every reset kind, including the CPU-only
//! reset of `esp_restart`: `system_early_init` restarts if any lock bit is still set, so a
//! surviving lock bit is an endless reboot loop. No reset-domain mask expresses that, so the
//! model does not ask [`ResetKind::clears`]. The hardware domain itself is UNVERIFIED.
//!
//! Enforcement (sources 56 to 60) is not modeled; the stored permissions are folded into the
//! page table (`wiring/protection.rs`), so every stored write returns
//! [`Wiring::ProtectionChanged`].

use pemu_core::fidelity::Fidelity;
use pemu_core::regstore::{RegSpec, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};

use crate::r#gen::regs_sensitive as table;
use crate::regs::{Ports, Regs, Table};

use super::{Block, Cx, Peripheral, RegRead, RegWrite, Wiring};

/// Bit 0 of a group's first register is its lock bit.
const LOCK_BIT: u32 = 1;

/// The seven lock registers `esp_mprot_is_conf_locked_any` reads, by offset.
pub const PMS_LOCKS: [u32; 7] = [0x090, 0x0A8, 0x0B4, 0x0C0, 0x0C8, 0x0D8, 0x130];

/// Whether register `idx` is the lock register of a group: its only field is a one-bit
/// `..._LOCK` at bit 0. Derived from the generated table (IDF `sensitive_reg.h`) rather than
/// transcribed.
fn is_lock(idx: usize) -> bool {
    let fields = table::REGS[idx].fields;
    fields.len() == 1
        && fields[0].shift == 0
        && fields[0].width == 1
        && fields[0].name.ends_with("_LOCK")
}

/// Family prefix of a lock register: its name without the trailing `_LOCK` or `_0`. The registers
/// of its group are the ones whose names start with it.
fn family(name: &'static str) -> &'static str {
    name.strip_suffix("_LOCK")
        .or_else(|| name.strip_suffix("_0"))
        .unwrap_or(name)
}

/// The lock register of the group holding register `idx`, or `None` for a register outside every
/// group (`SENSITIVE_CLOCK_GATE` and `SENSITIVE_DATE`).
fn lock_of(idx: usize) -> Option<usize> {
    let name = table::REGS[idx].name;
    (0..=idx).rev().find(|&i| {
        let lock = table::REGS[i].name;
        is_lock(i) && name.starts_with(family(lock))
    })
}

/// The generated `sensitive` register table.
pub struct Registers;

impl Table<{ table::REG_COUNT }> for Registers {
    const BLOCK: &'static str = "sensitive";

    fn specs() -> &'static [RegSpec; table::REG_COUNT] {
        &table::REGS
    }
}

/// Model of the `sensitive` row of the `c3_devices!` table: exact storage with per-group locks.
#[derive(Default, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    regs: Regs<Registers, { table::REG_COUNT }>,
}

impl Model {
    /// Whether the group holding `off` has its lock bit set.
    pub fn locked(&self, off: u32) -> bool {
        self.regs
            .index_of(off & !3)
            .and_then(lock_of)
            .is_some_and(|lock| self.regs.get(lock) & LOCK_BIT != 0)
    }

    /// Whether any of the seven lock bits `esp_mprot_is_conf_locked_any` reads is set.
    pub fn any_pms_lock(&self) -> bool {
        PMS_LOCKS.iter().any(|off| self.locked(*off))
    }

    /// Restores every register, whatever the reset kind.
    pub fn reset_block(&mut self, _kind: ResetKind) {
        self.regs.reset_all();
    }

    /// Reads `size` bytes at `off`; programmed values read back exactly.
    pub fn load(&mut self, off: u32, size: Size, ports: &mut Ports) -> u32 {
        self.regs.read(off, size, Self::ID, ports.now, ports.ledger)
    }

    /// Writes the low `size` bytes of `val` at `off` unless the group is locked. Returns whether
    /// anything was stored, so the caller can skip the protection re-fold.
    pub fn store(&mut self, off: u32, size: Size, val: u32, ports: &mut Ports) -> bool {
        if self.locked(off) {
            return false;
        }
        self.regs
            .write(off, size, val, Self::ID, ports.now, ports.ledger);
        true
    }
}

impl Peripheral for Model {
    const ID: PeriphId = super::id::SENSITIVE;
    const BASE: u32 = <super::block::Sensitive as Block>::BASE;
    const SIZE: u32 = <super::block::Sensitive as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, _cx: &mut Cx) {
        self.reset_block(kind);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, &mut Ports::of(cx)),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let stored = self.store(off, size, val, &mut Ports::of(cx));
        RegWrite {
            // The permissions fold into the page table, so the CPU must leave its block.
            stop: stored,
            wiring: if stored {
                Wiring::ProtectionChanged
            } else {
                Wiring::None
            },
        }
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.regs.class_at(off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::reset::{ResetCause, ResetFanout, ResetScope};
    use pemu_core::sched::Scheduler;

    use crate::intc::IrqFabric;

    struct Harness {
        sched: Scheduler,
        irq: IrqFabric,
        ledger: FidelityLedger,
    }

    impl Harness {
        fn new() -> Harness {
            Harness {
                sched: Scheduler::new(),
                irq: IrqFabric::new(),
                ledger: FidelityLedger::default(),
            }
        }

        fn ports(&mut self) -> Ports<'_> {
            Ports {
                now: pemu_core::time::VTime(0),
                sched: &mut self.sched,
                irq: &mut self.irq,
                ledger: &mut self.ledger,
            }
        }
    }

    #[test]
    fn the_lock_registers_are_the_ones_the_spec_names() {
        // The generated table must agree, because the group list is derived from it.
        let named = [
            (
                0x090,
                "SENSITIVE_CORE_X_IRAM0_DRAM0_DMA_SPLIT_LINE_CONSTRAIN_0",
            ),
            (0x0A8, "SENSITIVE_CORE_X_IRAM0_PMS_CONSTRAIN_0"),
            (0x0B4, "SENSITIVE_CORE_0_IRAM0_PMS_MONITOR_0"),
            (0x0C0, "SENSITIVE_CORE_X_DRAM0_PMS_CONSTRAIN_0"),
            (0x0C8, "SENSITIVE_CORE_0_DRAM0_PMS_MONITOR_0"),
            (0x0D8, "SENSITIVE_CORE_0_PIF_PMS_CONSTRAIN_0"),
            (0x130, "SENSITIVE_CORE_0_PIF_PMS_MONITOR_0"),
        ];
        for ((off, name), listed) in named.iter().zip(PMS_LOCKS) {
            assert_eq!(*off, listed);
            let idx = table::REGS
                .iter()
                .position(|s| u32::from(s.off) == *off)
                .unwrap_or_else(|| panic!("{off:#05X} is not a register of the block"));
            assert_eq!(table::REGS[idx].name, *name);
            assert!(is_lock(idx), "{name} carries its group's lock bit");
            assert_eq!(lock_of(idx), Some(idx), "a lock register locks itself");
            assert!(table::REGS[idx].stable_read, "{name} is read on every boot");
        }
    }

    #[test]
    fn each_group_runs_from_its_lock_register_to_the_next_family() {
        let idx_of = |off: u32| {
            table::REGS
                .iter()
                .position(|s| u32::from(s.off) == off)
                .unwrap_or_else(|| panic!("{off:#05X}"))
        };
        let lock_off = |off: u32| lock_of(idx_of(off)).map(|i| u32::from(table::REGS[i].off));
        assert_eq!(lock_off(0x004), Some(0x000), "ROM_TABLE under its LOCK");
        assert_eq!(
            lock_off(0x00C),
            Some(0x008),
            "PRIVILEGE_MODE_SEL under its LOCK"
        );
        assert_eq!(lock_off(0x0A4), Some(0x090), "the last split-line register");
        assert_eq!(lock_off(0x0B0), Some(0x0A8), "IRAM0 constrain 2");
        assert_eq!(lock_off(0x0BC), Some(0x0B4), "IRAM0 monitor 2");
        assert_eq!(lock_off(0x0C4), Some(0x0C0), "DRAM0 constrain 1");
        assert_eq!(lock_off(0x0D4), Some(0x0C8), "DRAM0 monitor 3");
        assert_eq!(lock_off(0x100), Some(0x0D8), "PIF constrain 10");
        assert_eq!(lock_off(0x12C), Some(0x104), "REGION constrain 10");
        assert_eq!(lock_off(0x148), Some(0x130), "PIF monitor 6");
        assert_eq!(lock_off(0x15C), Some(0x14C), "backup-bus constrain 4");
        assert_eq!(lock_off(0x16C), Some(0x160), "backup-bus monitor 3");
        assert_eq!(lock_off(0x170), None, "CLOCK_GATE is in no group");
        assert_eq!(lock_off(0xFFC), None, "DATE is in no group");
    }

    #[test]
    fn a_locked_group_ignores_writes_and_still_reads_back() {
        let mut h = Harness::new();
        let mut model = Model::default();
        model.store(0x0AC, Size::B4, 0x0000_0249, &mut h.ports());
        model.store(0x0B0, Size::B4, 0x0000_0FFF, &mut h.ports());
        let programmed = [
            model.load(0x0AC, Size::B4, &mut h.ports()),
            model.load(0x0B0, Size::B4, &mut h.ports()),
        ];
        assert_eq!(
            programmed[0], 0x0000_0249,
            "the permissions read back exactly"
        );
        assert!(!model.locked(0x0AC));

        assert!(model.store(0x0A8, Size::B4, LOCK_BIT, &mut h.ports()));
        assert!(model.locked(0x0A8));
        assert!(model.locked(0x0B0), "the last register of the group");
        assert!(!model.store(0x0AC, Size::B4, 0, &mut h.ports()));
        assert!(
            !model.store(0x0A8, Size::B4, 0, &mut h.ports()),
            "not even the lock itself"
        );
        assert_eq!(model.load(0x0AC, Size::B4, &mut h.ports()), programmed[0]);
        assert_eq!(model.load(0x0B0, Size::B4, &mut h.ports()), programmed[1]);
        assert_eq!(model.load(0x0A8, Size::B4, &mut h.ports()), LOCK_BIT);

        assert!(!model.locked(0x0A4), "the split-line group below");
        assert!(!model.locked(0x0B4), "the IRAM0 monitor group above");
        assert!(model.store(0x0B4, Size::B4, LOCK_BIT, &mut h.ports()));
    }

    #[test]
    fn a_register_outside_every_lockable_group_is_plain_storage() {
        let mut h = Harness::new();
        let mut model = Model::default();
        for lock in PMS_LOCKS {
            model.store(lock, Size::B4, LOCK_BIT, &mut h.ports());
        }
        assert!(model.any_pms_lock());
        assert!(!model.locked(0x170));
        assert!(model.store(0x170, Size::B4, 1, &mut h.ports()));
        assert_eq!(model.load(0x170, Size::B4, &mut h.ports()), 1);
    }

    #[test]
    fn the_lock_bits_clear_after_the_esp_restart_reset_kind() {
        let restart = ResetKind::of(ResetCause::RTC_SW_CPU).expect("documented cause");
        assert_eq!(restart.fanout, ResetFanout::CpuAndPms);
        assert_eq!(restart.scope, ResetScope::Core);
        assert!(
            !restart.clears(table::REGS[0].domain),
            "the reset-domain mask alone would keep the lock bits, which is why this model \
             does not ask it"
        );

        let mut h = Harness::new();
        let mut model = Model::default();
        for lock in PMS_LOCKS {
            model.store(lock, Size::B4, LOCK_BIT, &mut h.ports());
        }
        assert!(model.any_pms_lock());

        model.reset_block(restart);
        assert!(
            !model.any_pms_lock(),
            "every lock bit reads 0 after esp_restart"
        );
        for lock in PMS_LOCKS {
            assert!(!model.locked(lock), "{lock:#05X}");
            assert!(
                model.store(lock, Size::B4, 0x11, &mut h.ports()),
                "{lock:#05X}"
            );
        }
    }

    #[test]
    fn every_reset_kind_restores_every_register() {
        // Every documented cause, so a later fan-out change cannot quietly exempt this block.
        let causes = [
            ResetCause::POWERON,
            ResetCause::RTC_SW_SYS,
            ResetCause::RTC_SW_CPU,
            ResetCause::TG0WDT_SYS,
            ResetCause::TG1WDT_CPU,
            ResetCause::RTCWDT_SYS,
            ResetCause::USB_UART_CHIP,
            ResetCause::DEEPSLEEP,
        ];
        for cause in causes {
            let Some(kind) = ResetKind::of(cause) else {
                panic!("{cause:?} has no documented reset");
            };
            let mut h = Harness::new();
            let mut model = Model::default();
            model.store(0x0C4, Size::B4, 0, &mut h.ports());
            for lock in PMS_LOCKS {
                model.store(lock, Size::B4, LOCK_BIT, &mut h.ports());
            }
            assert!(model.any_pms_lock(), "{cause:?}");

            model.reset_block(kind);
            for spec in table::REGS.iter() {
                assert_eq!(
                    model.load(u32::from(spec.off), Size::B4, &mut h.ports()),
                    spec.reset,
                    "{cause:?}: {}",
                    spec.name
                );
            }
            assert!(!model.any_pms_lock(), "{cause:?}");
        }
    }

    #[test]
    fn a_write_reports_a_protection_change_only_when_it_stored_something() {
        let mut h = Harness::new();
        let mut model = Model::default();
        let before = model.load(0x0C4, Size::B4, &mut h.ports());
        assert!(model.store(0x0C0, Size::B4, LOCK_BIT, &mut h.ports()));
        assert!(!model.store(0x0C4, Size::B4, 0, &mut h.ports()));
        assert_eq!(model.load(0x0C4, Size::B4, &mut h.ports()), before);
    }
}
