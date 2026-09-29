//! `Wiring::ClockChanged`: rebase the [`Clock`] onto the CPU frequency the SYSTEM block selects
//! (`specs/blocks/system.toml`).
//!
//! `periph/system.rs` raises the effect on a write to `SYSTEM_SYSCLK_CONF` or
//! `SYSTEM_CPU_PER_CONF`; the machine calls [`apply`] for it and after every reset that reaches
//! SYSTEM. The rebase is continuous: only instructions after `insns` run at the new rate.

use pemu_core::clock::Clock;

use crate::r#gen::regs_system;
use crate::periph::system::{self, ClkSource, PRE_DIV_CNT_MASK};

/// The CPU frequency the SYSTEM block selects. The reset divider on XTAL answers `reset_hz`,
/// because the reset rate follows the board's crystal (`[soc] xtal_hz`), not
/// `system::XTAL_MHZ`; any other setting is `system::Model::cpu_hz`.
pub fn cpu_hz(system: &system::Model, reset_hz: u32) -> u32 {
    let conf = system.regs().get(regs_system::idx::SYSTEM_SYSCLK_CONF);
    let reset = regs_system::REGS[regs_system::idx::SYSTEM_SYSCLK_CONF].reset;
    if system.clk_source() == ClkSource::Xtal && conf & PRE_DIV_CNT_MASK == reset & PRE_DIV_CNT_MASK
    {
        reset_hz
    } else {
        system.cpu_hz()
    }
}

/// Rebases `clock` onto [`cpu_hz`] at instruction `insns` and returns the new frequency in Hz.
///
/// Also called after every reset, because `Peripheral::reset` raises no wiring effect.
pub fn apply(system: &system::Model, reset_hz: u32, clock: &mut Clock, insns: u64) -> u32 {
    let hz = cpu_hz(system, reset_hz);
    clock.rebase(insns, hz);
    hz
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;
    use pemu_core::time::VTime;

    /// One instruction per cycle.
    const CPI_MILLI: u32 = 1_000;

    const OFF_SYSCLK_CONF: u32 = 0x058;
    const OFF_CPU_PER_CONF: u32 = 0x008;

    /// `SOC_CLK_SEL` = 1 selects the BBPLL.
    const SOC_CLK_SEL_PLL: u32 = 1 << 10;

    /// `CPUPERIOD_SEL` = 1 is 160 MHz.
    const CPUPERIOD_160: u32 = 1;

    /// The reset rate of a 40 MHz crystal board, XTAL/2.
    const RESET_HZ: u32 = 20_000_000;

    fn system() -> (system::Model, FidelityLedger) {
        (system::Model::default(), FidelityLedger::default())
    }

    #[test]
    fn a_clock_change_rebases_onto_the_new_cpu_frequency() {
        let (mut sys, mut ledger) = system();
        let mut clock = Clock::new(sys.cpu_hz(), CPI_MILLI);
        assert_eq!(
            clock.cpu_hz(),
            20_000_000,
            "the XTAL divided by PRE_DIV_CNT + 1 = 2 at reset"
        );

        // Run 20 000 instructions at 20 MHz: one millisecond.
        let at = clock.now(20_000);
        assert_eq!(at, VTime(1_000_000_000), "1 ms in picoseconds");

        sys.store(
            OFF_CPU_PER_CONF,
            Size::B4,
            CPUPERIOD_160,
            VTime(0),
            &mut ledger,
        );
        sys.store(
            OFF_SYSCLK_CONF,
            Size::B4,
            SOC_CLK_SEL_PLL,
            VTime(0),
            &mut ledger,
        );
        let hz = apply(&sys, RESET_HZ, &mut clock, 20_000);

        assert_eq!(hz, 160_000_000);
        assert_eq!(clock.cpu_hz(), 160_000_000);
        assert_eq!(clock.now(20_000), at, "a rebase is continuous");
        assert_eq!(
            clock.now(20_000 + 160_000),
            VTime(2_000_000_000),
            "the next million cycles take a millisecond at 160 MHz"
        );
        assert_eq!(sys.apb_hz(), 80_000_000, "the APB stays at 80 MHz on PLL");
    }

    #[test]
    fn applying_the_effect_again_is_a_no_op() {
        let (sys, _) = system();
        let mut clock = Clock::new(sys.cpu_hz(), CPI_MILLI);
        let at = clock.now(10_000);
        assert_eq!(apply(&sys, RESET_HZ, &mut clock, 10_000), 20_000_000);
        assert_eq!(apply(&sys, RESET_HZ, &mut clock, 10_000), 20_000_000);
        assert_eq!(clock.now(10_000), at);
    }
}
