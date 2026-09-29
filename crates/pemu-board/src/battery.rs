//! The cell and the charger behind the CW2017 gauge (`specs/blocks/cw2017.toml`).
//!
//! Class C: no device measurement behind it. The firmware sees only the gauge, so what matters is
//! the curve's shape, a monotonic discharge and the cutoff. Charge is integer milliamp-microseconds
//! (520 mAh is 1.872e12, far inside an `i64`), so integration never rounds.

use pemu_core::sched::ChipId;
use pemu_core::time::VTime;
use serde::{Deserialize, Serialize};

use crate::traits::{BoardDomain, Chip};

/// Chip id of the cell (UNVERIFIED numbering, the first id above the I2C addresses).
pub const CHIP_ID: ChipId = ChipId(0x100);

/// Milliamp-microseconds in one milliamp-hour: one hour is 3.6e9 microseconds.
const MA_US_PER_MAH: i64 = 3_600_000_000;

/// The state-of-charge grid of [`BatteryConfig::ocv_mv`], in whole percent.
pub const OCV_STEP_PERCENT: u32 = 10;

/// Open-circuit voltage in millivolts at 0, 10, ... 100 percent: a generic Li-Po curve
/// (class C). The model needs it strictly increasing, ending at `cv_mv`, starting below
/// `cutoff_mv`.
pub const OCV_MV: [u16; 11] = [
    2950, 3500, 3620, 3680, 3730, 3780, 3850, 3920, 4000, 4100, 4200,
];

/// The cell and charger parameters (`[battery]` and `[power]` rows of the board file).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BatteryConfig {
    pub capacity_mah: u32,
    /// Constant charge current in milliamps (UNVERIFIED).
    pub charge_ma: u32,
    /// Charge current at which charging stops, in milliamps (UNVERIFIED).
    pub term_ma: u32,
    pub cv_mv: u32,
    /// Rail cutoff in millivolts; below it, with no USB 5 V, the rail drops (UNVERIFIED).
    pub cutoff_mv: u32,
    /// Series resistance in milliohms (class C): the load sag and a finite charge taper.
    pub series_mohm: u32,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        BatteryConfig {
            capacity_mah: 520,
            charge_ma: 200,
            term_ma: 20,
            cv_mv: 4200,
            cutoff_mv: 3000,
            series_mohm: 150,
        }
    }
}

impl BatteryConfig {
    fn capacity_ma_us(&self) -> i64 {
        i64::from(self.capacity_mah) * MA_US_PER_MAH
    }

    /// Open-circuit voltage at `soc_milli`, linearly interpolated.
    pub fn ocv_mv(&self, soc_milli: u32) -> u32 {
        let soc_milli = soc_milli.min(100_000);
        let span = OCV_STEP_PERCENT * 1_000;
        let index = (soc_milli / span) as usize;
        if index >= OCV_MV.len() - 1 {
            return u32::from(OCV_MV[OCV_MV.len() - 1]);
        }
        let low = u32::from(OCV_MV[index]);
        let high = u32::from(OCV_MV[index + 1]);
        let into = soc_milli - (index as u32) * span;
        low + (high - low) * into / span
    }

    /// The inverse of [`BatteryConfig::ocv_mv`], clamped to 0..=100 percent.
    pub fn soc_milli_at_ocv(&self, mv: u32) -> u32 {
        if mv <= u32::from(OCV_MV[0]) {
            return 0;
        }
        let span = OCV_STEP_PERCENT * 1_000;
        for index in 0..OCV_MV.len() - 1 {
            let low = u32::from(OCV_MV[index]);
            let high = u32::from(OCV_MV[index + 1]);
            if mv < high {
                let into = (mv - low) * span / (high - low);
                return (index as u32) * span + into;
            }
        }
        100_000
    }
}

/// The four load terms, as integer fractions.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct LoadEstimate {
    /// Fraction of wall time the CPU is not in a wait state, in thousandths.
    pub cpu_permille: u16,
    pub backlight_permille: u16,
    pub radio_tx: bool,
    /// Speaker output RMS as a fraction of full scale, in thousandths.
    pub speaker_permille: u16,
}

/// Current in milliamps each load term draws at its full value (class C, UNVERIFIED magnitudes).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LoadWeights {
    /// Current drawn with every term at zero.
    pub base_ma: u32,
    pub cpu_ma: u32,
    pub backlight_ma: u32,
    pub radio_tx_ma: u32,
    pub speaker_ma: u32,
}

impl Default for LoadWeights {
    fn default() -> Self {
        LoadWeights {
            base_ma: 8,
            cpu_ma: 25,
            backlight_ma: 30,
            radio_tx_ma: 120,
            speaker_ma: 40,
        }
    }
}

impl LoadWeights {
    pub fn load_ma(&self, load: &LoadEstimate) -> u32 {
        let share = |permille: u16, at_full: u32| at_full * u32::from(permille.min(1_000)) / 1_000;
        self.base_ma
            + share(load.cpu_permille, self.cpu_ma)
            + share(load.backlight_permille, self.backlight_ma)
            + if load.radio_tx { self.radio_tx_ma } else { 0 }
            + share(load.speaker_permille, self.speaker_ma)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Battery {
    pub cfg: BatteryConfig,
    pub weights: LoadWeights,
    pub load: LoadEstimate,
    /// Remaining charge in milliamp-microseconds.
    charge_ma_us: i64,
    updated: VTime,
    /// Whether USB 5 V is present, the board's only charger input.
    usb_5v: bool,
    /// Whether the MCU rail is up; with it down the cell holds its charge (class C).
    rail_on: bool,
    charge_done: bool,
    temp_deci_c: i16,
}

impl Default for Battery {
    fn default() -> Self {
        Battery::new(BatteryConfig::default(), Battery::DEFAULT_SOC_MILLI)
    }
}

impl Battery {
    /// Starting state of charge (UNVERIFIED): 80 percent reaches neither termination nor brownout.
    pub const DEFAULT_SOC_MILLI: u32 = 80_000;

    /// Starting temperature in tenths of a degree (UNVERIFIED).
    pub const ROOM_TEMP_DECI_C: i16 = 250;

    /// A cell at `soc_milli` thousandths of a percent, at room temperature, unplugged, rail down.
    pub fn new(cfg: BatteryConfig, soc_milli: u32) -> Self {
        let mut battery = Battery {
            cfg,
            weights: LoadWeights::default(),
            load: LoadEstimate::default(),
            charge_ma_us: 0,
            updated: VTime(0),
            usb_5v: false,
            rail_on: false,
            charge_done: false,
            temp_deci_c: Battery::ROOM_TEMP_DECI_C,
        };
        battery.set_soc_milli(soc_milli);
        battery
    }

    /// The cell after a disconnect and reconnect: charged to `soc_milli`, latch cleared, everything
    /// outside the cell kept, including the integration time so nothing integrates twice.
    pub fn reconnected(&self, soc_milli: u32) -> Battery {
        let mut cell = Battery::new(self.cfg, soc_milli);
        cell.weights = self.weights;
        cell.load = self.load;
        cell.updated = self.updated;
        cell.usb_5v = self.usb_5v;
        cell.rail_on = self.rail_on;
        cell.temp_deci_c = self.temp_deci_c;
        cell
    }

    pub fn soc_milli(&self) -> u32 {
        let capacity = self.cfg.capacity_ma_us();
        if capacity <= 0 {
            return 0;
        }
        let milli = i128::from(self.charge_ma_us) * 100_000 / i128::from(capacity);
        milli.clamp(0, 100_000) as u32
    }

    /// State of charge in whole percent, the number the gauge's integer register carries.
    pub fn soc_percent(&self) -> u8 {
        (self.soc_milli() / 1_000) as u8
    }

    pub fn set_soc_milli(&mut self, soc_milli: u32) {
        let capacity = self.cfg.capacity_ma_us();
        self.charge_ma_us =
            (i128::from(capacity) * i128::from(soc_milli.min(100_000)) / 100_000) as i64;
        self.charge_done = false;
    }

    /// Sets the charge from a terminal voltage at the present net current, so a written voltage
    /// reads back. One iteration; the residue is second order (class C).
    pub fn set_terminal_mv(&mut self, mv: u32) {
        let delta = i64::from(self.net_ma()) * i64::from(self.cfg.series_mohm) / 1_000;
        let ocv = (i64::from(mv) - delta).max(0) as u32;
        self.set_soc_milli(self.cfg.soc_milli_at_ocv(ocv));
    }

    pub fn temp_deci_c(&self) -> i16 {
        self.temp_deci_c
    }

    pub fn set_temp_deci_c(&mut self, temp_deci_c: i16) {
        self.temp_deci_c = temp_deci_c;
    }

    pub fn usb_5v(&self) -> bool {
        self.usb_5v
    }

    /// Plugs or unplugs the charger input. Unplugging clears the termination latch.
    pub fn set_usb_5v(&mut self, present: bool) {
        if self.usb_5v != present {
            self.usb_5v = present;
            self.charge_done = false;
        }
    }

    pub fn rail_on(&self) -> bool {
        self.rail_on
    }

    /// Raises or drops the MCU rail. With the rail down the modeled load is zero.
    pub fn set_rail_on(&mut self, on: bool) {
        self.rail_on = on;
    }

    pub fn load_ma(&self) -> u32 {
        if self.rail_on {
            self.weights.load_ma(&self.load)
        } else {
            0
        }
    }

    /// Charge current in milliamps: constant until the terminal voltage reaches `cv_mv`, then
    /// tapering; zero without USB 5 V or after termination.
    pub fn charge_ma(&self) -> u32 {
        if !self.usb_5v || self.charge_done {
            return 0;
        }
        let raw = self.taper_ma();
        if raw <= self.cfg.term_ma { 0 } else { raw }
    }

    /// The charger current before the termination rule.
    fn taper_ma(&self) -> u32 {
        let ocv = self.cfg.ocv_mv(self.soc_milli());
        let headroom = self.cfg.cv_mv.saturating_sub(ocv);
        let cv_ma = if self.cfg.series_mohm == 0 {
            self.cfg.charge_ma
        } else {
            headroom * 1_000 / self.cfg.series_mohm
        };
        cv_ma.min(self.cfg.charge_ma)
    }

    pub fn charge_done(&self) -> bool {
        self.charge_done
    }

    /// The net current in milliamps: positive into the cell, negative out of it.
    pub fn net_ma(&self) -> i32 {
        self.charge_ma() as i32 - self.load_ma() as i32
    }

    pub fn ocv_mv(&self) -> u32 {
        self.cfg.ocv_mv(self.soc_milli())
    }

    /// Terminal voltage in millivolts, the value the gauge measures.
    pub fn terminal_mv(&self) -> u32 {
        let ocv = i64::from(self.ocv_mv());
        let delta = i64::from(self.net_ma()) * i64::from(self.cfg.series_mohm) / 1_000;
        (ocv + delta).clamp(0, i64::from(u16::MAX)) as u32
    }

    /// Whether the terminal voltage is below the cutoff with no USB 5 V present (brownout).
    pub fn below_cutoff(&self) -> bool {
        !self.usb_5v && self.terminal_mv() < self.cfg.cutoff_mv
    }

    pub fn updated(&self) -> VTime {
        self.updated
    }

    /// Integrates the net current forward to `t` as one rectangle, so the caller must advance
    /// before changing a load term. In the CV taper step size matters; the error is class C.
    pub fn advance(&mut self, t: VTime) {
        if t <= self.updated {
            return;
        }
        // Whole microseconds only; the remainder stays for the next call.
        let dt_us = i64::try_from((t.0 - self.updated.0) / 1_000_000).unwrap_or(i64::MAX);
        self.updated = VTime(self.updated.0.saturating_add(dt_us as u64 * 1_000_000));
        let delta = i128::from(self.net_ma()) * i128::from(dt_us);
        let capacity = i128::from(self.cfg.capacity_ma_us());
        let charge = (i128::from(self.charge_ma_us) + delta).clamp(0, capacity);
        self.charge_ma_us = charge as i64;
        // Latch, so `charge_done` does not flicker with rounding.
        if self.usb_5v && self.taper_ma() <= self.cfg.term_ma {
            self.charge_done = true;
        }
    }
}

impl Chip for Battery {
    /// Resetting the `battery` domain is a disconnect and reconnect.
    fn reset(&mut self, domain: BoardDomain) {
        if domain == BoardDomain::Battery {
            let cfg = self.cfg;
            let weights = self.weights;
            *self = Battery::new(cfg, Battery::DEFAULT_SOC_MILLI);
            self.weights = weights;
        }
    }
}
