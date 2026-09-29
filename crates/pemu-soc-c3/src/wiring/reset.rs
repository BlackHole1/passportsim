//! `Wiring::ChipReset(kind)`: the reset fan-out, one pass over the `c3_devices!` table with each
//! reached block restoring the registers of the scopes the reset clears (TRM, Reset and Clock;
//! IDF `soc/esp32c3/include/soc/reset_reasons.h`).
//!
//! Which blocks are reset is `ResetKind::fanout`: the four `CPU0_` causes reach only the hart,
//! SENSITIVE and RTC_CNTL (which just latches the cause for the ROM's `rst:0xc`); IDF resets by
//! hand what it needs, and wiping UART0 and TIMG on `esp_restart` would break the console. How
//! deep a reset goes is `ResetKind::scope` against each register's domain, in the register tables.
//!
//! The hart's own state is the machine's; sleep and wake live in `periph/rtc_sleep.rs`.

use pemu_core::regstore::Size;
use pemu_core::reset::{ResetCause, ResetFanout, ResetKind, ResetScope};
use pemu_core::sched::PeriphId;
use pemu_rv32::spmon::SpMonitor;

use crate::periph::assist_debug::StackGuard;
use crate::periph::{Cx, DeviceVisitor, Devices, Peripheral, id};
use crate::regs::{Ports, write_bits};

pub const CPU_PERI_RST_EN_OFF: u32 = 0x004;

pub const RST_EN_ASSIST_DEBUG: u32 = 1 << 6;

/// The reset a peripheral reset enable applies to its block: every register of the digital
/// domain, and not the chip-scope record ASSIST_DEBUG keeps for the ROM's `Saved PC` line.
pub(crate) const PERIPHERAL_RESET: ResetKind = ResetKind {
    cause: ResetCause::RTC_SW_CPU,
    scope: ResetScope::Core,
    fanout: ResetFanout::AllBlocks,
};

/// A SYSTEM write that sets `SYSTEM_RST_EN_ASSIST_DEBUG` resets ASSIST_DEBUG and returns the
/// monitor to publish as `Wiring::SpMonitor`.
///
/// A `CPU0_` reset keeps ASSIST_DEBUG armed from the last boot; IDF's `esp_hw_stack_guard_init`
/// pulses this bit before it enables source 54. Without the reset the stale `INTR_RAW` fires at
/// once and every boot after a restart panics with `Stack protection fault`.
pub fn system_reset_enable(
    guard: &mut StackGuard,
    off: u32,
    size: Size,
    val: u32,
    ports: &mut Ports,
) -> Option<SpMonitor> {
    if off & !3 != CPU_PERI_RST_EN_OFF || write_bits(off, size, val) & RST_EN_ASSIST_DEBUG == 0 {
        return None;
    }
    guard.peripheral_reset(PERIPHERAL_RESET, ports);
    Some(guard.monitor())
}

/// Whether a reset of `kind` calls `Peripheral::reset` on block `block`.
pub fn reaches(kind: ResetKind, block: PeriphId) -> bool {
    kind.fanout.reaches_all_blocks() || block == id::SENSITIVE || block == id::RTC_CNTL
}

/// What the caller must re-apply after the fan-out, because `Peripheral::reset` raises no
/// `Wiring` effect.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct AfterReset {
    /// How many blocks were reset.
    pub blocks: u32,
    /// SYSTEM was reset, so the caller must run `wiring::clock::apply` before timing the next
    /// instruction; otherwise a reset from 160 MHz leaves every `ets_delay_us` eight times fast.
    pub rebase_clock: bool,
    /// GPIO was reset, so the caller must run `wiring::gpio::apply`.
    pub republish_gpio: bool,
}

/// What a reset of `kind` would oblige the caller to re-apply, without running the fan-out.
pub fn after_reset(kind: ResetKind) -> AfterReset {
    AfterReset {
        blocks: reached_count(kind) as u32,
        rebase_clock: reaches(kind, id::SYSTEM),
        republish_gpio: reaches(kind, id::GPIO),
    }
}

/// Applies one reset to every block the fan-out reaches, in table order, with `cx.now` at the
/// reset. The caller must then re-apply what the returned [`AfterReset`] names.
pub fn fan_out(devices: &mut Devices, kind: ResetKind, cx: &mut Cx<'_>) -> AfterReset {
    let mut v = FanOut { kind, cx, count: 0 };
    devices.visit_all(&mut v);
    AfterReset {
        blocks: v.count,
        ..after_reset(kind)
    }
}

struct FanOut<'a, 'b> {
    kind: ResetKind,
    cx: &'a mut Cx<'b>,
    count: u32,
}

impl DeviceVisitor for FanOut<'_, '_> {
    fn visit<P: Peripheral>(&mut self, dev: &mut P) {
        if !reaches(self.kind, P::ID) {
            return;
        }
        dev.reset(self.kind, self.cx);
        self.count += 1;
    }
}

/// Blocks a reset of `kind` reaches.
pub fn reached_count(kind: ResetKind) -> usize {
    crate::periph::BLOCKS
        .iter()
        .filter(|b| reaches(kind, b.id))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::regstore::Size;
    use pemu_core::reset::{RESET_CAUSES, ResetCause, ResetFanout, ResetScope};
    use pemu_core::time::VTime;

    use crate::periph::{BLOCK_COUNT, BLOCKS, gpio, system, uart0};

    /// The pulse IDF's stack-guard init writes clears a latched violation and the monitor but
    /// keeps the `Saved PC` record.
    #[test]
    fn the_assist_debug_reset_enable_clears_a_stale_spill_and_keeps_the_record() {
        use crate::intc::IrqFabric;
        use crate::periph::assist_debug::StackGuard;
        use pemu_core::fidelity::FidelityLedger;
        use pemu_core::regstore::Size;
        use pemu_core::sched::Scheduler;
        use pemu_rv32::spmon::SpSpill;
        let (mut sched, mut irq, mut ledger) = (
            Scheduler::new(),
            IrqFabric::new(),
            FidelityLedger::default(),
        );
        let mut ports = Ports {
            now: VTime(0),
            sched: &mut sched,
            irq: &mut irq,
            ledger: &mut ledger,
        };
        let mut guard = StackGuard::default();
        guard.store(0x044, Size::B4, 0x3, &mut ports); // RCD_EN
        guard.store(0x038, Size::B4, 0x3fc8_e000, &mut ports); // SP_MIN
        guard.store(0x03c, Size::B4, 0x3fc8_f000, &mut ports); // SP_MAX
        guard.store(0x000, Size::B4, 0x300, &mut ports); // INTR_ENA
        guard.record_spill(SpSpill::Min, 0x4200_0000, &mut ports);
        guard.record_reset(0x4038_0000, 0x3fc8_d000);
        assert_ne!(guard.raw(), 0);
        assert!(
            system_reset_enable(&mut guard, 0x018, Size::B4, u32::MAX, &mut ports).is_none(),
            "PERIP_RST_EN0 is not the register"
        );
        assert!(
            system_reset_enable(
                &mut guard,
                0x004,
                Size::B4,
                !RST_EN_ASSIST_DEBUG,
                &mut ports
            )
            .is_none()
        );
        assert_ne!(guard.raw(), 0);
        let monitor =
            system_reset_enable(&mut guard, 0x004, Size::B4, RST_EN_ASSIST_DEBUG, &mut ports)
                .expect("the ASSIST_DEBUG bit resets the block");
        assert_eq!(guard.raw(), 0, "the stale violation is gone");
        assert!(!monitor.armed(), "the monitor is off again");
        assert_eq!(
            guard.reset_record(),
            (0x4038_0000, 0x3fc8_d000),
            "Saved PC survives"
        );
    }

    fn kind(cause: ResetCause) -> ResetKind {
        ResetKind::of(cause).expect("a documented reset cause")
    }

    #[test]
    fn an_all_blocks_reset_reaches_every_row() {
        for cause in [
            ResetCause::POWERON,
            ResetCause::RTC_SW_SYS,
            ResetCause::RTCWDT_RTC,
        ] {
            let k = kind(cause);
            assert_eq!(k.fanout, ResetFanout::AllBlocks, "{cause:?}");
            assert_eq!(reached_count(k), BLOCK_COUNT, "{cause:?}");
            assert!(BLOCKS.iter().all(|b| reaches(k, b.id)), "{cause:?}");
        }
    }

    #[test]
    fn a_cpu_reset_reaches_only_sensitive_and_the_cause_latch() {
        let k = kind(ResetCause::RTC_SW_CPU);
        assert_eq!(k.fanout, ResetFanout::CpuAndPms);
        assert!(reaches(k, id::SENSITIVE));
        assert!(reaches(k, id::RTC_CNTL), "RESET_STATE latches rst:0xc");
        assert!(
            !reaches(k, id::UART0),
            "the console keeps its configuration"
        );
        assert!(!reaches(k, id::TIMG0));
        assert!(!reaches(k, id::GPIO));
        assert_eq!(reached_count(k), 2);
    }

    #[test]
    fn a_reset_names_what_the_caller_must_re_apply() {
        for cause in [
            ResetCause::POWERON,
            ResetCause::RTC_SW_SYS,
            ResetCause::USB_UART_CHIP,
            ResetCause::RTCWDT_RTC,
        ] {
            let a = after_reset(kind(cause));
            assert_eq!(a.blocks as usize, BLOCK_COUNT, "{cause:?}");
            assert!(a.rebase_clock, "{cause:?}");
            assert!(a.republish_gpio, "{cause:?}");
        }

        let cpu = after_reset(kind(ResetCause::RTC_SW_CPU));
        assert_eq!(cpu.blocks, 2);
        assert!(!cpu.rebase_clock, "SYSTEM keeps its selector");
        assert!(!cpu.republish_gpio, "the pins keep their levels");
    }

    /// Cause 0x15 after a switch to 160 MHz puts `SYSTEM_SYSCLK_CONF` back to 20 MHz; the `Clock`
    /// stays at 160 MHz until the caller re-runs `wiring::clock::apply`.
    #[test]
    fn a_core_reset_leaves_the_clock_stale_until_it_is_re_applied() {
        use pemu_core::clock::Clock;
        use pemu_core::fidelity::FidelityLedger;

        let mut ledger = FidelityLedger::default();
        let mut sys = system::Model::default();
        let mut clock = Clock::new(sys.cpu_hz(), 1_000);
        assert_eq!(clock.cpu_hz(), 20_000_000, "the XTAL path at reset");

        // SYSCLK_CONF.SOC_CLK_SEL = 1 and CPU_PER_CONF.CPUPERIOD_SEL = 1: the PLL at 160 MHz.
        sys.store(0x008, Size::B4, 1, VTime(0), &mut ledger);
        sys.store(0x058, Size::B4, 1 << 10, VTime(0), &mut ledger);
        assert_eq!(
            crate::wiring::clock::apply(&sys, 20_000_000, &mut clock, 0),
            160_000_000
        );

        let core = kind(ResetCause::USB_UART_CHIP);
        assert!(after_reset(core).rebase_clock);
        sys.reset_to(core);
        assert_eq!(sys.cpu_hz(), 20_000_000, "the selector is back at reset");
        assert_eq!(
            clock.cpu_hz(),
            160_000_000,
            "and nothing has rebased the clock yet"
        );

        assert_eq!(
            crate::wiring::clock::apply(&sys, 20_000_000, &mut clock, 1_000),
            20_000_000
        );
        assert_eq!(clock.cpu_hz(), 20_000_000);
    }

    #[test]
    fn a_core_reset_leaves_the_board_stale_until_it_is_re_applied() {
        use pemu_core::fidelity::FidelityLedger;

        let mut ledger = FidelityLedger::default();
        let mut gpio = gpio::Model::default();
        let dc = 1u32 << gpio::PIN_LCD_DC;
        gpio.store(0x020, Size::B4, dc, VTime(0), &mut ledger);
        gpio.store(0x004, Size::B4, dc, VTime(0), &mut ledger);
        gpio.mark_published();
        assert_eq!(gpio.changed_pins(), 0, "the board knows the DC line is up");

        let core = kind(ResetCause::USB_UART_CHIP);
        assert!(after_reset(core).republish_gpio);
        gpio.reset_to(core);
        assert!(!gpio.out_level(gpio::PIN_LCD_DC));
        assert_eq!(
            gpio.changed_pins(),
            dc,
            "the board still believes the old level until apply runs"
        );
    }

    #[test]
    fn every_documented_cause_has_a_fan_out() {
        for spec in RESET_CAUSES {
            let k = spec.kind;
            let n = reached_count(k);
            match k.fanout {
                ResetFanout::AllBlocks => assert_eq!(n, BLOCK_COUNT, "{}", spec.idf_name),
                ResetFanout::CpuAndPms => assert_eq!(n, 2, "{}", spec.idf_name),
            }
        }
    }

    #[test]
    fn the_scope_matrix_over_three_owned_blocks() {
        let mut ledger = pemu_core::fidelity::FidelityLedger::default();
        let mut sched = pemu_core::sched::Scheduler::default();
        let now = VTime(11);

        // A digital register in each of three blocks.
        let mut gpio = gpio::Model::default();
        let mut system = system::Model::default();
        let mut uart = uart0::Model::default();
        gpio.store(0x004, Size::B4, 0b11, now, &mut ledger);
        system.store(0x008, Size::B4, 1, now, &mut ledger);
        uart.store(0x020, Size::B4, 0x0000_0002, now, &mut sched, &mut ledger);

        // CPU0_: kept, and the fan-out does not even call reset.
        let cpu = kind(ResetCause::RTC_SW_CPU);
        assert!(!cpu.clears(crate::r#gen::DOMAIN_CHIP_SYSTEM_CORE));
        gpio.reset_to(cpu);
        assert_eq!(gpio.out_levels(), 0b11);

        // CORE_: digital cleared, RTC kept. Cause 0x03 is IDF `CORE_SW`, a core-scope reset
        // despite its ROM name `RTC_SW_SYS_RESET`.
        let core = kind(ResetCause::RTC_SW_SYS);
        assert_eq!(core.scope, ResetScope::Core);
        assert!(core.clears(crate::r#gen::DOMAIN_CHIP_SYSTEM_CORE));
        assert!(!core.clears(crate::r#gen::DOMAIN_CHIP_SYSTEM));
        gpio.reset_to(core);
        system.reset_to(core);
        uart.reset_to(core, &mut sched);
        assert_eq!(gpio.out_levels(), 0);
        assert_eq!(
            gpio.strap(),
            gpio::STRAP_FLASH_BOOT,
            "a core reset re-latches the board strap"
        );

        // SYS_ and power-on: the RTC domain too.
        assert!(kind(ResetCause::RTCWDT_RTC).clears(crate::r#gen::DOMAIN_CHIP_SYSTEM));
        assert!(kind(ResetCause::POWERON).clears(crate::r#gen::DOMAIN_CHIP));
        assert!(!kind(ResetCause::RTCWDT_RTC).clears(crate::r#gen::DOMAIN_CHIP));
    }
}
