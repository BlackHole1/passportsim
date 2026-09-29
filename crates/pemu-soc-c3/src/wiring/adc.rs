//! `Wiring::AdcSample`: one SAR ADC conversion against the board (`specs/blocks/saradc.toml`).
//!
//! It runs inside the access that raised `onetime_start`, so the driver's first poll of
//! `INT_RAW` bit 31 already sees the done bit (row `saradc.adc1_done`, `within = same_access`).

use pemu_board::traits::BoardPorts;
use pemu_core::time::VTime;

use crate::periph::saradc::{AdcUnit, Conversion, Model};

/// Samples `unit` and `channel` on `board` at `now` and latches the result into `adc`.
///
/// `unit` other than 1 or 2 is ignored, so a corrupt wiring value latches nothing.
pub fn sample(adc: &mut Model, now: VTime, unit: u8, channel: u8, board: &dyn BoardPorts) {
    let Some(unit) = (match unit {
        1 => Some(AdcUnit::One),
        2 => Some(AdcUnit::Two),
        _ => None,
    }) else {
        return;
    };
    let mv = board.adc_mv(now, unit.number(), channel);
    adc.latch(
        Conversion {
            unit,
            channel,
            // The attenuation the same access wrote to `ONETIME_SAMPLE`.
            atten: adc.atten(),
        },
        mv,
    );
}
