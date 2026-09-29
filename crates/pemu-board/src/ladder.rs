//! The ADC button ladder on GPIO0, and the inverse of the IDF ADC curve fitting
//! (`components/esp_adc/adc_cali_curve_fitting.c`) that the SAR ADC model needs.
//!
//! Three buttons share one 10 kOhm pull-up, so the pin carries one voltage for the pressed set.
//! The SAR ADC model (`saradc.rs`) converts through [`inverse_curve`], kept here next to the
//! codes it must reproduce.
//! The helpers expand to journal entries, so a replay needs none of this logic.

use pemu_core::input::{ButtonId, InputEvent};
use pemu_core::journal::{Journal, Origin};
use pemu_core::time::VTime;
use serde::{Deserialize, Serialize};

/// ADC attenuation. Only 12 dB is used by this firmware and has known coefficients; the others
/// reuse its curve (UNVERIFIED).
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum Atten {
    Db0,
    Db2_5,
    Db6,
    /// 12 dB: what `button_adc` configures.
    #[default]
    Db12,
}

/// The eFuse ADC calibration fields the 12 dB inverse needs. With `blk_version_major` other than
/// 1 the scheme fails and the firmware reads 0 mV (UP held forever).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AdcCal {
    /// eFuse `BLK_VERSION_MAJOR`; the scheme needs 1.
    pub blk_version_major: u8,
    /// eFuse `ADC1_CAL_VOL_ATTEN3`, 10 bits with bit 9 as the sign.
    pub cal_vol_atten3: u16,
}

impl Default for AdcCal {
    /// The synthesized eFuse: version 1, every calibration field 0.
    fn default() -> Self {
        AdcCal {
            blk_version_major: 1,
            cal_vol_atten3: 0,
        }
    }
}

impl AdcCal {
    /// Whether the curve-fitting scheme can be created (else `ESP_ERR_NOT_SUPPORTED`).
    pub const fn supports_curve_fitting(self) -> bool {
        self.blk_version_major == ADC_CALIB_VER
    }

    /// The stored digital value of the 12 dB reference point:
    /// `digi = 2000 + (bit9 ? -(field & 0x1FF) : field & 0x1FF)`.
    pub const fn digi_atten3(self) -> u32 {
        let magnitude = (self.cal_vol_atten3 & 0x1FF) as u32;
        if self.cal_vol_atten3 & 0x200 != 0 {
            CAL_VOL_BASE - magnitude
        } else {
            CAL_VOL_BASE + magnitude
        }
    }
}

const ADC_CALIB_VER: u8 = 1;
/// The digital value the `ADC1_CAL_VOL_ATTENn` fields are an offset from.
const CAL_VOL_BASE: u32 = 2000;
/// Reference voltage of the 12 dB calibration point, in millivolts.
const ATTEN3_REF_MV: u32 = 1370;
/// Full-scale code of a 12-bit conversion (`1_DATA_STATUS & 0xFFF`).
pub const RAW_MAX: u16 = 4095;
/// Denominator of every curve-fitting term.
const TERM_SCALE: u64 = 10_000_000_000_000_000;
/// Numerators of the five error terms, in the order `t0..t4`.
const TERM_NUM: [u64; 5] = [
    14_912_262_772_850_453,
    228_549_975_564_099,
    356_391_935_717,
    179_964_582,
    42_046,
];

/// The forward curve-fitting conversion: a 12-bit code to millivolts at 12 dB, in the integer
/// math `esp_adc/adc_cali_curve_fitting.c` uses.
///
/// ```text
/// coeff_a = 65536 * 1370 / digi        v1 = raw * coeff_a / 65536
/// error   = -t0 - t1 + t2 - t3 + t4    mV = v1 - error
/// ```
///
/// Each term truncates before the signed sum, so the mapping is non-monotonic.
pub fn curve_mv(raw: u16, atten: Atten, cal: AdcCal) -> u32 {
    let _ = atten;
    if !cal.supports_curve_fitting() {
        // Without calibration `get_adc_voltage` returns 0 mV on the C3.
        return 0;
    }
    let digi = cal.digi_atten3();
    if digi == 0 {
        return 0;
    }
    let coeff_a = (65_536 * ATTEN3_REF_MV as u64) / digi as u64;
    let v1 = (raw as u64 * coeff_a) / 65_536;
    let mut powers = 1u64;
    let mut terms = [0i64; 5];
    for (index, term) in terms.iter_mut().enumerate() {
        *term = ((powers.saturating_mul(TERM_NUM[index])) / TERM_SCALE) as i64;
        powers = powers.saturating_mul(v1);
    }
    let error = -terms[0] - terms[1] + terms[2] - terms[3] + terms[4];
    let mv = v1 as i64 - error;
    if mv < 0 { 0 } else { mv as u32 }
}

/// The code the hardware would report for a pin voltage: the first code whose voltage reaches
/// the target (the forward curve is not monotonic), saturating at [`RAW_MAX`].
pub fn inverse_curve(mv: u32, atten: Atten, cal: AdcCal) -> u16 {
    for raw in 0..=RAW_MAX {
        if curve_mv(raw, atten, cal) >= mv {
            return raw;
        }
    }
    RAW_MAX
}

/// The pin voltages and codes of one ladder (`[buttons]` of the board file). `raw_code` holds the
/// device's codes (`campaign_regs` probe); `mv` is what the synthesized curve maps onto them.
/// Down converts to 393, not 394: both read 274 mV and the inverse takes the first.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LadderConfig {
    pub adc_unit: u8,
    /// ADC channel; `ADC_CHANNEL_0` on GPIO0.
    pub adc_channel: u8,
    pub gpio: u8,
    /// Pin voltage with UP pressed (0 Ohm to ground; the device's code 3 is the ADC's offset).
    pub up_mv: u32,
    /// Pin voltage with DOWN pressed (1/11 of the pull-up rail).
    pub down_mv: u32,
    /// Pin voltage with OK pressed (2.2/12.2 of the pull-up rail).
    pub ok_mv: u32,
    /// Pin voltage with nothing pressed: the rail, which saturates the ADC.
    pub released_mv: u32,
    /// The four codes, in the [`ButtonId::ALL`] order followed by released.
    pub raw_code: [u16; 4],
    /// GPIO0 deep-sleep wake level; a pin below this reads low (class C, UNVERIFIED).
    pub digital_vil_mv: u32,
    /// Whether the ladder's pull-up holds GPIO0 high in deep sleep. It does not (class B, the
    /// `campaign_reset` probe woke at once from a GPIO0-low deep sleep): IDF enables the chip's ~45
    /// kOhm pull-up on the pad, and against the unpowered 10 kOhm pull-up the pad sits near 0.6 V,
    /// below V_IL. Which supply is off is UNVERIFIED without the schematic.
    pub pullup_held_in_deep_sleep: bool,
}

impl Default for LadderConfig {
    fn default() -> Self {
        LadderConfig {
            adc_unit: 1,
            adc_channel: 0,
            gpio: 0,
            up_mv: 3,
            down_mv: 274,
            ok_mv: 540,
            released_mv: 3_300,
            raw_code: [3, 393, 782, 4_095],
            digital_vil_mv: 825,
            pullup_held_in_deep_sleep: false,
        }
    }
}

impl LadderConfig {
    pub const fn button_mv(&self, id: ButtonId) -> u32 {
        match id {
            ButtonId::Up => self.up_mv,
            ButtonId::Down => self.down_mv,
            ButtonId::Ok => self.ok_mv,
        }
    }

    pub const fn button_raw(&self, id: ButtonId) -> u16 {
        match id {
            ButtonId::Up => self.raw_code[0],
            ButtonId::Down => self.raw_code[1],
            ButtonId::Ok => self.raw_code[2],
        }
    }

    pub const fn released_raw(&self) -> u16 {
        self.raw_code[3]
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ButtonLadder {
    cfg: LadderConfig,
    /// Which buttons are held, in [`ButtonId::ALL`] order.
    down: [bool; 3],
}

impl Default for ButtonLadder {
    fn default() -> Self {
        ButtonLadder::new(LadderConfig::default())
    }
}

impl ButtonLadder {
    pub fn new(cfg: LadderConfig) -> Self {
        ButtonLadder {
            cfg,
            down: [false; 3],
        }
    }

    pub fn config(&self) -> &LadderConfig {
        &self.cfg
    }

    /// Applies one `InputEvent::Button`; returns whether the pressed set changed.
    pub fn set(&mut self, id: ButtonId, down: bool) -> bool {
        let slot = &mut self.down[id as usize];
        let changed = *slot != down;
        *slot = down;
        changed
    }

    pub fn is_down(&self, id: ButtonId) -> bool {
        self.down[id as usize]
    }

    /// Releases every button. No domain reset calls this.
    pub fn release_all(&mut self) {
        self.down = [false; 3];
    }

    /// The pin voltage for the pressed set: parallel resistors to ground, so the lowest wins
    /// (UNVERIFIED). Exact for UP plus anything; DOWN plus OK only lands in the right window.
    pub fn adc_mv(&self) -> u32 {
        let mut mv = self.cfg.released_mv;
        for id in ButtonId::ALL {
            if self.is_down(id) {
                mv = mv.min(self.cfg.button_mv(id));
            }
        }
        mv
    }

    pub fn raw_code(&self, cal: AdcCal) -> u16 {
        inverse_curve(self.adc_mv(), Atten::Db12, cal)
    }

    /// Whether GPIO0 reads low while the chip runs (class C).
    pub fn reads_low(&self) -> bool {
        self.adc_mv() < self.cfg.digital_vil_mv
    }

    /// Whether GPIO0 reads low in deep sleep.
    pub fn low_in_deep_sleep(&self) -> bool {
        !self.cfg.pullup_held_in_deep_sleep || self.reads_low()
    }
}

pub const CLICK_HOLD_MS: u64 = 60;
/// Milliseconds a [`long_press`] holds the button down; `LONG_PRESS_START` fires at 1500 ms.
pub const LONG_PRESS_HOLD_MS: u64 = 1_600;
/// Milliseconds to run after the release: `SINGLE_CLICK` arrives about 185 ms after it.
pub const SETTLE_MS: u64 = 200;

/// One step of an [`InputScript`]: an event and how long after the script's start it happens.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScriptStep {
    pub at_ms: u64,
    pub ev: InputEvent,
}

/// What a helper expands to: journal entries and how long the caller must run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InputScript {
    pub steps: Vec<ScriptStep>,
    /// How long after the script's start the caller should run for the guest to observe it.
    pub run_for_ms: u64,
}

impl InputScript {
    /// Appends every step to `journal`, stamped `now + at_ms`, and returns the time to run until.
    pub fn append_to(&self, journal: &mut Journal, now: VTime, origin: Origin) -> VTime {
        for step in &self.steps {
            let at = VTime(now.0.saturating_add(VTime::from_ms(step.at_ms).0));
            journal.append(now, at, origin, step.ev.clone());
        }
        VTime(now.0.saturating_add(VTime::from_ms(self.run_for_ms).0))
    }
}

/// `press(b, ms)`: raw control, a hold of exactly `ms` with no settle window.
pub fn press(id: ButtonId, ms: u64) -> InputScript {
    InputScript {
        steps: vec![
            ScriptStep {
                at_ms: 0,
                ev: InputEvent::Button { id, down: true },
            },
            ScriptStep {
                at_ms: ms,
                ev: InputEvent::Button { id, down: false },
            },
        ],
        run_for_ms: ms,
    }
}

/// `click(b)`: down, 60 ms, up, then run until release plus [`SETTLE_MS`].
pub fn click(id: ButtonId) -> InputScript {
    let mut script = press(id, CLICK_HOLD_MS);
    script.run_for_ms = CLICK_HOLD_MS + SETTLE_MS;
    script
}

/// `long_press(b)`: a 1600 ms hold, clearing the 1500 ms `LONG_PRESS_START`, then the settle
/// window.
pub fn long_press(id: ButtonId) -> InputScript {
    let mut script = press(id, LONG_PRESS_HOLD_MS);
    script.run_for_ms = LONG_PRESS_HOLD_MS + SETTLE_MS;
    script
}
