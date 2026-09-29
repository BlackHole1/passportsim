//! ASSIST_DEBUG: the hardware stack guard, the reset PC record and the debugger-presence bit
//! (`specs/blocks/assist_debug.toml`).
//!
//! A hot block: `rtos_int_enter` and `rtos_int_exit` rewrite the stack bounds and toggle the
//! monitor on every interrupt. The check itself runs in the CPU ([`SpMonitor::check`] after an
//! op that writes `x2`); this model publishes the monitor through [`Wiring::SpMonitor`], latches
//! `INTR_RAW` and `SP_PC` on a reported spill, drives source 54 and handles `INTR_CLR`.
//!
//! `INTR_ENA` bits 8 and 9 enable the monitor and `INTR_RLS` bits 8 and 9 the interrupt output,
//! as IDF `assist_debug_ll.h:13,64-79` uses them. The FreeRTOS port clears `INTR_ENA` around a
//! context switch, so rewriting the bounds cannot trip the guard.
//!
//! While `RCD_EN` is 3 a reset freezes PC and SP into `RCD_PDEBUGPC/SP`, which ROM
//! `boot_prepare` prints as `Saved PC`. The area, PIF, bus monitors and the log unit are stored
//! only and never raise source 54.

use pemu_core::fidelity::Fidelity;
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{RegSpec, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_rv32::spmon::{SpMonitor, SpSpill};

use crate::r#gen::regs_assist_debug as table;
use crate::regs::{Ports, Regs, Table, write_bits};

use super::{Block, Cx, Peripheral, RegRead, RegWrite, Wiring};

/// `ASSIST_DEBUG_CORE_0_SP_SPILL_MIN_*` in each of the four interrupt registers.
pub const SP_SPILL_MIN: u32 = 1 << 8;
/// `ASSIST_DEBUG_CORE_0_SP_SPILL_MAX_*` in each of the four interrupt registers.
pub const SP_SPILL_MAX: u32 = 1 << 9;
/// Both stack-guard bits.
pub const SP_SPILL: u32 = SP_SPILL_MIN | SP_SPILL_MAX;

/// `RCD_RECORDEN` and `RCD_PDEBUGEN` together, as the bootloader and ROM write them.
pub const RCD_EN_BOTH: u32 = 0b11;

/// Interrupt source 54, which `esp_system/hw_stack_guard.c` routes to CPU line 27, level,
/// priority 4.
pub const SOURCE: IrqSource = irq::ASSIST_DEBUG;

/// `ASSIST_DEBUG_CORE_0_DEBUG_MODULE_ACTIVE`, bit 1 of DEBUG_MODE.
const DEBUG_MODULE_ACTIVE: u8 = 1;

pub struct Registers;

impl Table<{ table::REG_COUNT }> for Registers {
    const BLOCK: &'static str = "assist_debug";

    fn specs() -> &'static [RegSpec; table::REG_COUNT] {
        &table::REGS
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct StackGuard {
    regs: Regs<Registers, { table::REG_COUNT }>,
}

impl StackGuard {
    /// The stack guard as the CPU sees it: `INTR_ENA` bits 8 and 9 with `SP_MIN` and `SP_MAX`.
    pub fn monitor(&self) -> SpMonitor {
        let ena = self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_INTR_ENA);
        SpMonitor {
            on_min: ena & SP_SPILL_MIN != 0,
            on_max: ena & SP_SPILL_MAX != 0,
            min: self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_SP_MIN),
            max: self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_SP_MAX),
        }
    }

    /// The latched violations: `INTR_RAW`.
    pub fn raw(&self) -> u32 {
        self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_INTR_RAW)
    }

    /// PC of the instruction whose `sp` write violated a bound: `SP_PC`.
    pub fn spill_pc(&self) -> u32 {
        self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_SP_PC)
    }

    /// The engine reported a violation at `pc`: latch `INTR_RAW` and `SP_PC` and re-drive source
    /// 54. The engine ends the block after the violating instruction, so the interrupt boundary
    /// does not depend on block size.
    pub fn record_spill(&mut self, spill: SpSpill, pc: u32, ports: &mut Ports) {
        let bit = match spill {
            SpSpill::Min => SP_SPILL_MIN,
            SpSpill::Max => SP_SPILL_MAX,
        };
        let raw = self.raw() | bit;
        self.regs.set(table::idx::ASSIST_DEBUG_CORE_0_INTR_RAW, raw);
        self.regs.set(table::idx::ASSIST_DEBUG_CORE_0_SP_PC, pc);
        self.drive_source(ports);
    }

    /// Freezes the PC and SP the ROM prints as `Saved PC`, while `RCD_EN` is 3. The machine calls
    /// this just before the reset fan-out; the record is Chip-scope only, so it survives every
    /// reset but power-on.
    pub fn record_reset(&mut self, pc: u32, sp: u32) {
        if self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_RCD_EN) & RCD_EN_BOTH != RCD_EN_BOTH {
            return;
        }
        self.regs
            .set(table::idx::ASSIST_DEBUG_CORE_0_RCD_PDEBUGPC, pc);
        self.regs
            .set(table::idx::ASSIST_DEBUG_CORE_0_RCD_PDEBUGSP, sp);
    }

    /// The PC and SP frozen at the last reset.
    pub fn reset_record(&self) -> (u32, u32) {
        (
            self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_RCD_PDEBUGPC),
            self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_RCD_PDEBUGSP),
        )
    }

    /// Sets `DEBUG_MODULE_ACTIVE` (`esp_cpu_dbgr_is_attached`). It stays 0 unless the emulator's
    /// gdb stub is attached: with `CONFIG_ESP_DEBUG_OCDAWARE` a 1 changes panic and watchdog
    /// handling.
    pub fn set_debugger_attached(&mut self, attached: bool) {
        self.regs.set_field(
            table::idx::ASSIST_DEBUG_CORE_0_DEBUG_MODE,
            DEBUG_MODULE_ACTIVE,
            1,
            u32::from(attached),
        );
    }

    pub fn reset_block(&mut self, kind: ResetKind, ports: &mut Ports) {
        self.regs.reset(kind);
        self.drive_source(ports);
    }

    /// The reset `SYSTEM_RST_EN_ASSIST_DEBUG` applies: the monitor, bounds and latched
    /// violations go back to reset, and `RCD_EN` and the record survive. Class A: in the
    /// `probe_stack` capture, a stack overflow after `esp_hw_stack_guard_init` pulsed this reset
    /// still prints `Saved PC` on the next boot.
    pub fn peripheral_reset(&mut self, kind: ResetKind, ports: &mut Ports) {
        let rcd_en = self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_RCD_EN);
        let record = self.reset_record();
        self.regs.reset(kind);
        self.regs
            .set(table::idx::ASSIST_DEBUG_CORE_0_RCD_EN, rcd_en);
        self.regs
            .set(table::idx::ASSIST_DEBUG_CORE_0_RCD_PDEBUGPC, record.0);
        self.regs
            .set(table::idx::ASSIST_DEBUG_CORE_0_RCD_PDEBUGSP, record.1);
        self.drive_source(ports);
    }

    pub fn load(&mut self, off: u32, size: Size, ports: &mut Ports) -> u32 {
        self.regs.read(off, size, Self::ID, ports.now, ports.ledger)
    }

    /// Writes the low `size` bytes of `val` at `off`. Returns the new monitor when the write
    /// touched one of the three registers the CPU mirrors.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        ports: &mut Ports,
    ) -> Option<SpMonitor> {
        self.regs
            .write(off, size, val, Self::ID, ports.now, ports.ledger)?;
        match self.regs.index_of(off & !3) {
            Some(table::idx::ASSIST_DEBUG_CORE_0_INTR_CLR) => {
                // `RAW &= ~value` over the bits this access wrote, not the stored word: the CLR
                // fields are RW, so the word keeps bytes a narrow write did not address.
                let raw = self.raw() & !write_bits(off, size, val);
                self.regs.set(table::idx::ASSIST_DEBUG_CORE_0_INTR_RAW, raw);
                self.drive_source(ports);
                None
            }
            Some(table::idx::ASSIST_DEBUG_CORE_0_INTR_RLS) => {
                self.drive_source(ports);
                None
            }
            Some(
                table::idx::ASSIST_DEBUG_CORE_0_INTR_ENA
                | table::idx::ASSIST_DEBUG_CORE_0_SP_MIN
                | table::idx::ASSIST_DEBUG_CORE_0_SP_MAX,
            ) => Some(self.monitor()),
            _ => None,
        }
    }

    /// `src_level[54] = (RAW & RLS & 0x300) != 0`.
    fn drive_source(&mut self, ports: &mut Ports) {
        let rls = self.regs.get(table::idx::ASSIST_DEBUG_CORE_0_INTR_RLS);
        ports
            .irq
            .set_source(SOURCE, self.raw() & rls & SP_SPILL != 0);
    }
}

impl Peripheral for StackGuard {
    const ID: PeriphId = super::id::ASSIST_DEBUG;
    const BASE: u32 = <super::block::AssistDebug as Block>::BASE;
    const SIZE: u32 = <super::block::AssistDebug as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.reset_block(kind, &mut Ports::of(cx));
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, &mut Ports::of(cx)),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let epoch = cx.irq.epoch();
        match self.store(off, size, val, &mut Ports::of(cx)) {
            // OkStop, so the engine copies the monitor into `Hart::spmon` before the next
            // instruction.
            Some(monitor) => RegWrite {
                stop: true,
                wiring: Wiring::SpMonitor(monitor),
            },
            None => RegWrite {
                stop: cx.irq.epoch() != epoch,
                wiring: Wiring::None,
            },
        }
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.regs.class_at(off)
    }
}

pub type Model = StackGuard;

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::reset::{ResetCause, ResetFanout, ResetScope};
    use pemu_core::sched::Scheduler;
    use pemu_core::time::VTime;

    use crate::intc::IrqFabric;

    const INTR_ENA: u32 = 0x000;
    const INTR_RAW: u32 = 0x004;
    const INTR_RLS: u32 = 0x008;
    const INTR_CLR: u32 = 0x00C;
    const SP_MIN: u32 = 0x038;
    const SP_MAX: u32 = 0x03C;
    const SP_PC: u32 = 0x040;
    const RCD_EN: u32 = 0x044;
    const RCD_PDEBUGPC: u32 = 0x048;
    const RCD_PDEBUGSP: u32 = 0x04C;
    const DEBUG_MODE: u32 = 0x098;
    const DATE: u32 = 0x1FC;
    /// One of the unmodeled monitors: stored, read back, nothing more.
    const AREA_PC: u32 = 0x030;

    /// Placeholder stack bounds in the C3 DRAM range; no device value appears here.
    const TASK_BOTTOM: u32 = 0x3FC8_1000;
    const TASK_TOP: u32 = 0x3FC8_2000;
    const ISR_BOTTOM: u32 = 0x3FC8_4000;
    const ISR_TOP: u32 = 0x3FC8_5000;

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
                now: VTime(0),
                sched: &mut self.sched,
                irq: &mut self.irq,
                ledger: &mut self.ledger,
            }
        }

        fn source(&self) -> bool {
            self.irq.source(SOURCE)
        }
    }

    struct Dut {
        h: Harness,
        guard: StackGuard,
    }

    impl Dut {
        fn new() -> Dut {
            Dut {
                h: Harness::new(),
                guard: StackGuard::default(),
            }
        }

        fn read(&mut self, off: u32) -> u32 {
            self.guard.load(off, Size::B4, &mut self.h.ports())
        }

        fn write(&mut self, off: u32, val: u32) -> Option<SpMonitor> {
            self.guard.store(off, Size::B4, val, &mut self.h.ports())
        }

        fn spill(&mut self, spill: SpSpill, pc: u32) {
            self.guard.record_spill(spill, pc, &mut self.h.ports());
        }

        fn reset(&mut self, kind: ResetKind) {
            self.guard.reset_block(kind, &mut self.h.ports());
        }

        fn source(&self) -> bool {
            self.h.source()
        }
    }

    #[test]
    fn the_register_offsets_follow_the_generated_table() {
        let named = [
            (INTR_ENA, table::idx::ASSIST_DEBUG_CORE_0_INTR_ENA),
            (INTR_RAW, table::idx::ASSIST_DEBUG_CORE_0_INTR_RAW),
            (INTR_RLS, table::idx::ASSIST_DEBUG_CORE_0_INTR_RLS),
            (INTR_CLR, table::idx::ASSIST_DEBUG_CORE_0_INTR_CLR),
            (SP_MIN, table::idx::ASSIST_DEBUG_CORE_0_SP_MIN),
            (SP_MAX, table::idx::ASSIST_DEBUG_CORE_0_SP_MAX),
            (SP_PC, table::idx::ASSIST_DEBUG_CORE_0_SP_PC),
            (RCD_EN, table::idx::ASSIST_DEBUG_CORE_0_RCD_EN),
            (RCD_PDEBUGPC, table::idx::ASSIST_DEBUG_CORE_0_RCD_PDEBUGPC),
            (RCD_PDEBUGSP, table::idx::ASSIST_DEBUG_CORE_0_RCD_PDEBUGSP),
            (DEBUG_MODE, table::idx::ASSIST_DEBUG_CORE_0_DEBUG_MODE),
            (DATE, table::idx::ASSIST_DEBUG_DATE),
        ];
        for (off, idx) in named {
            assert_eq!(u32::from(table::REGS[idx].off), off, "{idx}");
        }

        for (idx, suffix) in [
            (table::idx::ASSIST_DEBUG_CORE_0_INTR_ENA, "ENA"),
            (table::idx::ASSIST_DEBUG_CORE_0_INTR_RAW, "RAW"),
            (table::idx::ASSIST_DEBUG_CORE_0_INTR_RLS, "RLS"),
            (table::idx::ASSIST_DEBUG_CORE_0_INTR_CLR, "CLR"),
        ] {
            for (shift, bound) in [(8u8, "MIN"), (9u8, "MAX")] {
                let want = format!("ASSIST_DEBUG_CORE_0_SP_SPILL_{bound}_{suffix}");
                let found = table::REGS[idx]
                    .fields
                    .iter()
                    .any(|f| f.shift == shift && f.width == 1 && f.name == want);
                assert!(found, "{want} at bit {shift}");
            }
        }
        assert_eq!(SP_SPILL_MIN, 1 << 8);
        assert_eq!(SP_SPILL_MAX, 1 << 9);
        assert_eq!(SP_SPILL, 0x300, "the MIN and MAX spill bits together");
    }

    #[test]
    fn the_sp_monitor_follows_the_portasm_stop_set_bounds_start_sequence() {
        // `portasm.S` `rtos_int_enter`: stop the monitor, save the task stack, set the ISR stack
        // bounds, start the monitor again.
        let mut d = Dut::new();

        // Init: the interrupt output is enabled once and stays on.
        d.write(INTR_RLS, SP_SPILL);
        d.write(SP_MIN, TASK_BOTTOM);
        d.write(SP_MAX, TASK_TOP);
        let started = d
            .write(INTR_ENA, SP_SPILL)
            .expect("ENA publishes a monitor");
        assert!(started.armed());
        assert_eq!((started.min, started.max), (TASK_BOTTOM, TASK_TOP));

        // 1. ESP_HW_STACK_GUARD_MONITOR_STOP_CUR_CORE clears the INTR_ENA bits.
        let stopped = d.write(INTR_ENA, 0).expect("ENA publishes a monitor");
        assert!(
            !stopped.armed(),
            "the monitor is off while the port switches"
        );
        assert_eq!(d.read(INTR_ENA), 0);

        // 2 and 3. The new SP_MIN is above the old SP_MAX, so a running monitor would spill
        // here; step 1 prevents it.
        let after_min = d.write(SP_MIN, ISR_BOTTOM).expect("SP_MIN publishes");
        assert!(!after_min.armed());
        assert_eq!(
            after_min.check(TASK_TOP),
            None,
            "a stopped monitor is silent"
        );
        let after_max = d.write(SP_MAX, ISR_TOP).expect("SP_MAX publishes");
        assert!(!after_max.armed());

        // 4. ESP_HW_STACK_GUARD_MONITOR_START_CUR_CORE sets them again.
        let restarted = d
            .write(INTR_ENA, SP_SPILL)
            .expect("ENA publishes a monitor");
        assert!(restarted.armed());
        assert_eq!((restarted.min, restarted.max), (ISR_BOTTOM, ISR_TOP));
        assert_eq!(restarted.check(ISR_BOTTOM - 4), Some(SpSpill::Min));
        assert_eq!(restarted.check(ISR_TOP + 4), Some(SpSpill::Max));
        assert_eq!(
            restarted.check(ISR_BOTTOM),
            None,
            "the bounds are inclusive"
        );
        assert_eq!(restarted.check(ISR_TOP), None, "the bounds are inclusive");

        assert_eq!(d.read(INTR_RAW), 0);
        assert!(!d.source());

        // `rtos_int_exit` runs the same four steps with the resumed task's bounds.
        d.write(INTR_ENA, 0);
        d.write(SP_MIN, TASK_BOTTOM);
        d.write(SP_MAX, TASK_TOP);
        let resumed = d
            .write(INTR_ENA, SP_SPILL)
            .expect("ENA publishes a monitor");
        assert_eq!((resumed.min, resumed.max), (TASK_BOTTOM, TASK_TOP));
        assert!(!d.source());
    }

    #[test]
    fn only_the_three_mirrored_registers_publish_a_monitor() {
        let mut d = Dut::new();
        for off in [INTR_ENA, SP_MIN, SP_MAX] {
            assert!(d.write(off, 0x3FC8_1234).is_some(), "{off:#05X} publishes");
        }
        for off in [INTR_RLS, INTR_CLR, RCD_EN, DATE] {
            assert!(d.write(off, 0).is_none(), "{off:#05X} does not publish");
        }
    }

    #[test]
    fn a_violation_latches_raw_and_the_pc_and_rls_gates_the_source() {
        let mut d = Dut::new();
        d.write(SP_MIN, TASK_BOTTOM);
        d.write(SP_MAX, TASK_TOP);
        d.write(INTR_ENA, SP_SPILL);

        d.spill(SpSpill::Min, 0x4200_1234);
        assert_eq!(d.read(INTR_RAW), SP_SPILL_MIN);
        assert_eq!(d.read(SP_PC), 0x4200_1234);
        assert!(!d.source(), "INTR_RLS gates the interrupt output");

        d.write(INTR_RLS, SP_SPILL);
        assert!(d.source());

        d.spill(SpSpill::Max, 0x4200_5678);
        assert_eq!(d.read(INTR_RAW), SP_SPILL);
        assert_eq!(d.read(SP_PC), 0x4200_5678);
        assert!(d.source());

        d.write(INTR_CLR, SP_SPILL_MIN);
        assert_eq!(d.read(INTR_RAW), SP_SPILL_MAX);
        assert!(d.source(), "the other bound is still latched");
        d.write(INTR_CLR, SP_SPILL_MAX);
        assert_eq!(d.read(INTR_RAW), 0);
        assert!(!d.source());
        assert_eq!(d.read(SP_PC), 0x4200_5678, "INTR_CLR does not touch SP_PC");
    }

    #[test]
    fn int_clr_clears_only_the_bits_the_access_wrote() {
        let mut d = Dut::new();
        d.write(SP_MIN, TASK_BOTTOM);
        d.write(SP_MAX, TASK_TOP);
        d.write(INTR_ENA, SP_SPILL);
        d.write(INTR_RLS, SP_SPILL);

        d.write(INTR_CLR, SP_SPILL);
        assert_eq!(d.read(INTR_CLR), SP_SPILL, "the mask stays stored");

        d.spill(SpSpill::Min, 0x4200_1234);
        assert_eq!(d.read(INTR_RAW), SP_SPILL_MIN);
        assert!(d.source());

        d.guard.store(INTR_CLR, Size::B1, 0x00, &mut d.h.ports());
        assert_eq!(
            d.read(INTR_RAW),
            SP_SPILL_MIN,
            "a byte write of 0 clears no bit of another byte"
        );
        assert!(d.source());

        d.guard
            .store(INTR_CLR + 1, Size::B1, 0x01, &mut d.h.ports());
        assert_eq!(d.read(INTR_RAW), 0);
        assert!(!d.source());
    }

    #[test]
    fn raw_and_sp_pc_ignore_a_software_write() {
        let mut d = Dut::new();
        d.write(INTR_RLS, SP_SPILL);
        d.write(INTR_RAW, SP_SPILL);
        d.write(SP_PC, 0xDEAD_BEEF);
        assert_eq!(d.read(INTR_RAW), 0);
        assert_eq!(d.read(SP_PC), 0);
        assert!(!d.source(), "software cannot forge a violation");
    }

    #[test]
    fn the_other_monitors_of_the_block_never_raise_the_source() {
        let mut d = Dut::new();
        d.write(INTR_RLS, !0);
        assert_eq!(d.read(INTR_RLS) & SP_SPILL, SP_SPILL);
        assert!(!d.source());

        let others = 0x0FFF & !SP_SPILL;
        d.guard
            .regs
            .set(table::idx::ASSIST_DEBUG_CORE_0_INTR_RAW, others);
        d.guard.drive_source(&mut d.h.ports());
        assert_eq!(d.read(INTR_RAW), others);
        assert!(!d.source(), "only bits 8 and 9 drive source 54");
    }

    #[test]
    fn the_reset_record_survives_a_watchdog_reset_and_a_power_on_clears_it() {
        let mut d = Dut::new();
        d.write(RCD_EN, RCD_EN_BOTH);
        d.guard.record_reset(0x4038_0A1C, 0x3FC8_1F40);

        let wdt = ResetKind::of(ResetCause::TG0WDT_SYS).expect("documented cause");
        assert_eq!(wdt.fanout, ResetFanout::AllBlocks);
        assert_ne!(wdt.scope, ResetScope::Chip, "not a power-on");
        d.reset(wdt);
        assert_eq!(d.read(RCD_PDEBUGPC), 0x4038_0A1C);
        assert_eq!(d.read(RCD_PDEBUGSP), 0x3FC8_1F40);
        assert_eq!(d.read(RCD_EN), 0, "the rest of the block is restored");
        assert_eq!(d.guard.reset_record(), (0x4038_0A1C, 0x3FC8_1F40));

        // A power-on reset clears them, so the ROM prints no `Saved PC` line.
        let power_on = ResetKind::of(ResetCause::POWERON).expect("documented cause");
        assert_eq!(power_on.scope, ResetScope::Chip);
        d.reset(power_on);
        assert_eq!(d.guard.reset_record(), (0, 0));
    }

    #[test]
    fn the_esp_restart_reset_kind_leaves_the_whole_block_alone() {
        // Cause 0x0C reaches only the hart and SENSITIVE, so `reset` here must be harmless.
        let restart = ResetKind::of(ResetCause::RTC_SW_CPU).expect("documented cause");
        assert_eq!(restart.fanout, ResetFanout::CpuAndPms);

        let mut d = Dut::new();
        d.write(RCD_EN, RCD_EN_BOTH);
        d.guard.record_reset(0x4038_0A1C, 0x3FC8_1F40);
        d.write(INTR_RLS, SP_SPILL);
        d.write(SP_MIN, TASK_BOTTOM);
        d.write(SP_MAX, TASK_TOP);
        d.write(INTR_ENA, SP_SPILL);

        d.reset(restart);
        assert_eq!(d.read(RCD_EN), RCD_EN_BOTH);
        assert_eq!(d.read(SP_MIN), TASK_BOTTOM);
        assert_eq!(d.guard.reset_record(), (0x4038_0A1C, 0x3FC8_1F40));
        assert!(d.guard.monitor().armed());
    }

    #[test]
    fn the_record_freezes_only_while_rcd_en_is_three() {
        // The bootloader writes 3 before arming the watchdog; before that the record must not move.
        let mut d = Dut::new();
        d.guard.record_reset(0x4000_0000, 0x3FC8_0000);
        assert_eq!(d.guard.reset_record(), (0, 0));
        d.write(RCD_EN, 1);
        d.guard.record_reset(0x4000_0000, 0x3FC8_0000);
        assert_eq!(d.guard.reset_record(), (0, 0), "RECORDEN alone is not 3");
        d.write(RCD_EN, RCD_EN_BOTH);
        d.guard.record_reset(0x4000_0000, 0x3FC8_0000);
        assert_eq!(d.guard.reset_record(), (0x4000_0000, 0x3FC8_0000));
    }

    #[test]
    fn debug_mode_reads_zero_until_the_stub_attaches() {
        let mut d = Dut::new();
        assert_eq!(d.read(DEBUG_MODE), 0);
        d.write(DEBUG_MODE, !0);
        assert_eq!(d.read(DEBUG_MODE), 0, "the register is read-only");
        d.guard.set_debugger_attached(true);
        assert_eq!(d.read(DEBUG_MODE), 1 << DEBUG_MODULE_ACTIVE);
        d.guard.set_debugger_attached(false);
        assert_eq!(d.read(DEBUG_MODE), 0);
    }

    #[test]
    fn a_reset_drops_the_monitor_and_the_pending_source() {
        let mut d = Dut::new();
        d.write(INTR_RLS, SP_SPILL);
        d.write(SP_MIN, TASK_BOTTOM);
        d.write(SP_MAX, TASK_TOP);
        d.write(INTR_ENA, SP_SPILL);
        d.spill(SpSpill::Min, 0x4200_0000);
        assert!(d.source());

        d.reset(ResetKind::of(ResetCause::RTC_SW_SYS).expect("documented cause"));
        assert!(!d.source(), "the block is restored and the source drops");
        assert_eq!(d.read(INTR_RAW), 0);
        assert_eq!(d.read(SP_PC), 0);
        let monitor = d.guard.monitor();
        assert!(!monitor.armed());
        // An unconfigured guard spans the whole address space.
        assert_eq!((monitor.min, monitor.max), (0, u32::MAX));
    }

    #[test]
    fn a_byte_write_of_a_bound_still_publishes_the_monitor() {
        let mut d = Dut::new();
        let published = d.guard.store(SP_MIN + 1, Size::B1, 0x20, &mut d.h.ports());
        assert_eq!(published.map(|m| m.min), Some(0x2000));
        assert_eq!(d.read(SP_MIN), 0x2000);
    }

    #[test]
    fn the_date_register_reads_its_constant() {
        // Drivers use DATE as a presence check.
        let mut d = Dut::new();
        assert_eq!(d.read(DATE), 0x0200_8010);
    }

    #[test]
    fn the_model_reports_the_table_fidelity_class() {
        let d = Dut::new();
        assert_eq!(d.guard.fidelity(INTR_ENA), Fidelity::B);
        assert_eq!(d.guard.fidelity(DEBUG_MODE), Fidelity::B);
        assert_eq!(d.guard.fidelity(SP_PC), Fidelity::B);
        assert_eq!(d.guard.fidelity(INTR_CLR), Fidelity::B);
        assert_eq!(d.guard.fidelity(AREA_PC), Fidelity::U);
        assert_ne!(d.guard.fidelity(0), Fidelity::U);
    }
}
