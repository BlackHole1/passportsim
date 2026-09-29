//! `Wiring` effects counted per variant, so a report can name which effects the machine left
//! unapplied rather than only how many.

use pemu_soc_c3::periph::Wiring;

/// One counter per `Wiring` variant (`Wiring::None` is not an effect). Part of the `host`
/// snapshot section.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    pemu_core::serde::Serialize,
    pemu_core::serde::Deserialize,
)]
#[serde(crate = "pemu_core::serde")]
pub struct WiringCounts {
    pub clock_changed: u64,
    pub mmu_entry: u64,
    pub cache_ctrl: u64,
    pub flash_written: u64,
    pub protection_changed: u64,
    pub spi2_transfer: u64,
    pub i2c_run: u64,
    pub i2s_period: u64,
    pub sha_dma: u64,
    pub aes_dma: u64,
    pub adc_sample: u64,
    pub gpio_changed: u64,
    pub ledc_changed: u64,
    pub usj_io: u64,
    pub sp_monitor: u64,
    pub chip_reset: u64,
    pub sleep_enter: u64,
}

impl WiringCounts {
    pub fn note(&mut self, w: &Wiring) {
        let slot = match w {
            Wiring::None => return,
            Wiring::ClockChanged => &mut self.clock_changed,
            Wiring::MmuEntry(_) => &mut self.mmu_entry,
            Wiring::CacheCtrl => &mut self.cache_ctrl,
            Wiring::FlashWritten { .. } => &mut self.flash_written,
            Wiring::ProtectionChanged => &mut self.protection_changed,
            Wiring::Spi2Transfer => &mut self.spi2_transfer,
            Wiring::I2cRun => &mut self.i2c_run,
            Wiring::I2sPeriod(_) => &mut self.i2s_period,
            Wiring::ShaDma => &mut self.sha_dma,
            Wiring::AesDma => &mut self.aes_dma,
            Wiring::AdcSample { .. } => &mut self.adc_sample,
            Wiring::GpioChanged => &mut self.gpio_changed,
            Wiring::LedcChanged => &mut self.ledc_changed,
            Wiring::UsjIo => &mut self.usj_io,
            Wiring::SpMonitor(_) => &mut self.sp_monitor,
            Wiring::ChipReset(_) => &mut self.chip_reset,
            Wiring::SleepEnter(_) => &mut self.sleep_enter,
        };
        *slot += 1;
    }

    /// Every counter added up.
    pub fn total(&self) -> u64 {
        let WiringCounts {
            clock_changed,
            mmu_entry,
            cache_ctrl,
            flash_written,
            protection_changed,
            spi2_transfer,
            i2c_run,
            i2s_period,
            sha_dma,
            aes_dma,
            adc_sample,
            gpio_changed,
            ledc_changed,
            usj_io,
            sp_monitor,
            chip_reset,
            sleep_enter,
        } = *self;
        clock_changed
            + mmu_entry
            + cache_ctrl
            + flash_written
            + protection_changed
            + spi2_transfer
            + i2c_run
            + i2s_period
            + sha_dma
            + aes_dma
            + adc_sample
            + gpio_changed
            + ledc_changed
            + usj_io
            + sp_monitor
            + chip_reset
            + sleep_enter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::reset::{ResetCause, ResetKind};

    #[test]
    fn each_effect_lands_in_its_own_counter_and_none_is_not_counted() {
        let mut c = WiringCounts::default();
        c.note(&Wiring::None);
        assert_eq!(c, WiringCounts::default());
        c.note(&Wiring::MmuEntry(3));
        c.note(&Wiring::MmuEntry(4));
        c.note(&Wiring::ChipReset(
            ResetKind::of(ResetCause::POWERON).expect("0x01 is documented"),
        ));
        assert_eq!(c.mmu_entry, 2);
        assert_eq!(c.chip_reset, 1);
        assert_eq!(c.sp_monitor, 0);
        assert_eq!(c.total(), 3);
    }
}
