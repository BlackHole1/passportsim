//! CW2017 fuel gauge on I2C0 at 7-bit address 0x63 (CW2017 datasheet, `specs/blocks/cw2017.toml`).
//!
//! Powered by the cell, the gauge keeps its profile and update flag across MCU resets and resets
//! only on a battery disconnect. The reported state of charge follows the cell through a lag.

use pemu_core::sched::ChipId;
use pemu_core::time::VTime;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::battery::Battery;
use crate::traits::{BoardDomain, Chip, I2cDevice};

/// 7-bit I2C address of the gauge (fixed, 0b1100011).
pub const ADDRESS: u8 = 0x63;

pub const CHIP_ID: ChipId = ChipId(ADDRESS as u16);

pub const REG_VERSION: u8 = 0x00;
/// VCELL high byte: 14 bits at 312.5 microvolts per step.
pub const REG_VCELL_H: u8 = 0x02;
/// SOC high byte: whole percent, then 1/256 of a percent in the low byte.
pub const REG_SOC_H: u8 = 0x04;
/// TEMP: degrees Celsius are `-40 + value / 2`.
pub const REG_TEMP: u8 = 0x06;
pub const REG_CONFIG: u8 = 0x08;
pub const REG_INT_CONF: u8 = 0x0A;
/// SOC_ALERT: bit 7 is UPDATE_FLAG, bits 6 to 0 the alert threshold in percent.
pub const REG_SOC_ALERT: u8 = 0x0B;
pub const REG_TEMP_MAX: u8 = 0x0C;
pub const REG_TEMP_MIN: u8 = 0x0D;
pub const REG_PROFILE: u8 = 0x10;
/// Bytes in the battery profile, 0x10 to 0x5F.
pub const PROFILE_LEN: usize = 80;
const NAMED_REGS: usize = 0x10;

/// VERSION as this board's gauge reports it; the datasheet says 0xA0.
pub const VERSION_OVERRIDE: u8 = 0x0F;

/// CONFIG value that puts the gauge to sleep; also its power-on value.
pub const CONFIG_SLEEP: u8 = 0xF0;
pub const CONFIG_RESTART: u8 = 0x30;
pub const CONFIG_ACTIVE: u8 = 0x00;

/// SOC_ALERT bit 7: the host sets it after loading a profile.
pub const UPDATE_FLAG: u8 = 0x80;

/// SOC_ALERT power-on default: update flag clear, alert threshold 20 percent.
pub const SOC_ALERT_FRESH: u8 = 0x14;
/// SOC_ALERT of a gauge the BSP has already provisioned.
pub const SOC_ALERT_PROVISIONED: u8 = SOC_ALERT_FRESH | UPDATE_FLAG;

/// The 80 profile bytes the BSP writes and compares (`specs/boards/cw2017-profile.toml`).
pub const BSP_PROFILE: [u8; PROFILE_LEN] = [
    0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xAD, 0xC7, 0xC8, 0xCA, 0xBD, 0xB1, 0xC1, 0x94,
    0x88, 0xD1, 0xBD, 0x97, 0x88, 0x66, 0x56, 0x4A, 0x3F, 0x33, 0x26, 0x5C, 0x37, 0xD1, 0x27, 0xD8,
    0xCC, 0xB7, 0xCF, 0xB3, 0xB2, 0xAE, 0xA6, 0x9E, 0x99, 0x97, 0x9B, 0x86, 0x47, 0x1E, 0x17, 0x26,
    0x49, 0x96, 0xD9, 0xE1, 0xDD, 0xDC, 0xD4, 0x59, 0x00, 0x00, 0x90, 0x02, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5C,
];

/// VCELL step of 312.5 microvolts as a fraction; the BSP converts with `raw * 3125 / 10000` mV.
const VCELL_UV_NUM: u32 = 3125;
const VCELL_UV_DEN: u32 = 10_000;

/// How long SOC reads out of range after computation starts (class C; the BSP tolerates 5 s).
pub const COMPUTE_WINDOW: VTime = VTime(1_000_000_000_000);

/// Time constant of the reported SOC lag (class C): a load step does not step the percentage,
/// while a discharge stays visible within a demo.
pub const SOC_LAG: VTime = VTime(30_000_000_000_000);

/// SOC_H while computing; the BSP rejects anything above 100.
pub const SOC_COMPUTING: u8 = 0xFF;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ConfigState {
    /// CONFIG 0xF0, also the power-on state.
    #[default]
    Sleep,
    /// CONFIG 0x30: between a sleep and a computation.
    Restart,
    /// CONFIG 0x00: computing a state of charge.
    Computing,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Cw2017 {
    #[serde(deserialize_with = "named_regs")]
    regs: Vec<u8>,
    #[serde(deserialize_with = "profile_bytes")]
    profile: Vec<u8>,
    /// Register pointer; auto-increments across a read (UNVERIFIED).
    pointer: u8,
    phase: Phase,
    state: ConfigState,
    /// When the present computation started; `None` while computing means before this run.
    compute_start: Option<VTime>,
    /// Reported state of charge in thousandths of a percent, lagging the cell model.
    soc_milli: u32,
    /// Whether a computation has ever finished its window, so SOC means something.
    computed: bool,
    /// Whether [`Cw2017::sample`] has met this machine's cell; the first sample does not lag.
    synced: bool,
    updated: VTime,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
enum Phase {
    Idle,
    Pointer,
    Data,
    Read,
}

impl Default for Cw2017 {
    fn default() -> Self {
        Cw2017::provisioned()
    }
}

impl Cw2017 {
    /// The default start state: already provisioned and settled, so `bsp_battery_init` takes its
    /// fast path and `cw_wait_soc_ready` succeeds on its first read, as on the device.
    pub fn provisioned() -> Self {
        let mut gauge = Cw2017::fresh();
        gauge.profile.copy_from_slice(&BSP_PROFILE);
        gauge.regs[REG_SOC_ALERT as usize] = SOC_ALERT_PROVISIONED;
        gauge.regs[REG_CONFIG as usize] = CONFIG_ACTIVE;
        gauge.state = ConfigState::Computing;
        gauge.compute_start = None;
        gauge.computed = true;
        gauge.soc_milli = Battery::DEFAULT_SOC_MILLI;
        gauge.refresh(&Battery::default());
        gauge
    }

    /// The datasheet power-on state, also what a reconnect leaves; `bsp_battery_init` then takes
    /// the slow path of 80 writes and 80 read-backs.
    pub fn fresh() -> Self {
        let mut regs = vec![0u8; NAMED_REGS];
        regs[REG_VERSION as usize] = VERSION_OVERRIDE;
        regs[REG_CONFIG as usize] = CONFIG_SLEEP;
        regs[REG_INT_CONF as usize] = 0x40;
        regs[REG_SOC_ALERT as usize] = SOC_ALERT_FRESH;
        regs[REG_TEMP_MAX as usize] = 0xAA;
        regs[REG_TEMP_MIN as usize] = 0x50;
        Cw2017 {
            regs,
            profile: vec![0u8; PROFILE_LEN],
            pointer: 0,
            phase: Phase::Idle,
            state: ConfigState::Sleep,
            compute_start: None,
            soc_milli: 0,
            computed: false,
            synced: false,
            updated: VTime(0),
        }
    }

    pub fn pointer(&self) -> u8 {
        self.pointer
    }

    pub fn in_transaction(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// Value of one register, exactly as a read at that pointer would return it.
    pub fn reg(&self, reg: u8) -> u8 {
        match reg {
            r if usize::from(r) < NAMED_REGS => self.regs[usize::from(r)],
            r if (REG_PROFILE..REG_PROFILE + PROFILE_LEN as u8).contains(&r) => {
                self.profile[usize::from(r - REG_PROFILE)]
            }
            // Unmapped; reads zero (UNVERIFIED, the BSP never reads one).
            _ => 0,
        }
    }

    pub fn profile(&self) -> &[u8] {
        &self.profile
    }

    pub fn update_flag(&self) -> bool {
        self.regs[REG_SOC_ALERT as usize] & UPDATE_FLAG != 0
    }

    /// Whether the profile equals the BSP's table, which picks the fast or slow init path.
    pub fn profile_matches_bsp(&self) -> bool {
        self.update_flag() && self.profile[..] == BSP_PROFILE[..]
    }

    pub fn state(&self) -> ConfigState {
        self.state
    }

    /// The alert threshold in whole percent, SOC_ALERT bits 6 to 0.
    pub fn alert_threshold_percent(&self) -> u8 {
        self.regs[REG_SOC_ALERT as usize] & !UPDATE_FLAG
    }

    /// Whether the reported SOC has fallen to the alert threshold; never while not computing.
    pub fn soc_alert(&self) -> bool {
        self.computed && self.soc_percent() <= self.alert_threshold_percent()
    }

    pub fn soc_percent(&self) -> u8 {
        (self.soc_milli / 1_000).min(100) as u8
    }

    pub fn soc_milli(&self) -> u32 {
        self.soc_milli
    }

    /// Whether the gauge has finished a computation window, so its SOC registers are in range.
    pub fn soc_ready(&self) -> bool {
        self.computed
    }
}

impl Cw2017 {
    /// Brings the gauge up to `t`: advances the cell, moves the reported SOC one [`lag`] step and
    /// refreshes the measurement registers. The first computation after a start does not lag: the
    /// chip takes the cell voltage as the open-circuit voltage at power-up.
    pub fn sample(&mut self, t: VTime, battery: &mut Battery) {
        battery.advance(t);
        let target = battery.soc_milli();
        if self.state == ConfigState::Computing {
            let ready = match self.compute_start {
                // Settled before this run began: the provisioned start state.
                None => true,
                Some(start) => t.0.saturating_sub(start.0) >= COMPUTE_WINDOW.0,
            };
            if ready && !(self.computed && self.synced) {
                self.computed = true;
                self.synced = true;
                self.soc_milli = target;
            } else if self.computed {
                self.soc_milli = lag(self.soc_milli, target, self.updated, t);
            }
        }
        self.updated = t;
        self.refresh(battery);
    }

    /// Takes the cell's SOC at once (a host override is not drift). Nothing is reported inside the
    /// [`COMPUTE_WINDOW`] or while asleep.
    pub fn override_soc(&mut self, t: VTime, battery: &mut Battery) {
        battery.advance(t);
        if self.state == ConfigState::Computing && self.computed {
            self.soc_milli = battery.soc_milli();
            self.synced = true;
        }
        self.updated = t;
        self.refresh(battery);
    }

    /// The 14-bit VCELL value for a cell voltage in millivolts (`raw = mV / 0.3125`).
    pub fn vcell_raw(mv: u32) -> u16 {
        (mv * VCELL_UV_DEN / VCELL_UV_NUM).min(0x3FFF) as u16
    }

    fn refresh(&mut self, battery: &Battery) {
        let raw = u32::from(Cw2017::vcell_raw(battery.terminal_mv()));
        self.regs[REG_VCELL_H as usize] = (raw >> 8) as u8;
        self.regs[usize::from(REG_VCELL_H) + 1] = raw as u8;
        // `(T + 40) * 2`: a tenth of a degree is a fifth of a step; saturates.
        let temp = (i32::from(battery.temp_deci_c()) + 400) / 5;
        self.regs[REG_TEMP as usize] = temp.clamp(0, 255) as u8;
        if self.state == ConfigState::Computing && !self.computed {
            self.regs[REG_SOC_H as usize] = SOC_COMPUTING;
            self.regs[usize::from(REG_SOC_H) + 1] = SOC_COMPUTING;
        } else if self.computed {
            self.regs[REG_SOC_H as usize] = self.soc_percent();
            // SOC_L is the fraction in 1/256 of a percent.
            let fraction = (self.soc_milli % 1_000) * 256 / 1_000;
            self.regs[usize::from(REG_SOC_H) + 1] = fraction as u8;
        }
    }

    /// Writes one register; the only path that changes gauge state.
    pub fn write_reg(&mut self, t: VTime, reg: u8, value: u8) {
        match reg {
            // Accepted in any CONFIG state (UNVERIFIED whether the chip requires sleep).
            r if (REG_PROFILE..REG_PROFILE + PROFILE_LEN as u8).contains(&r) => {
                self.profile[usize::from(r - REG_PROFILE)] = value;
            }
            REG_CONFIG => {
                self.regs[REG_CONFIG as usize] = value;
                match value {
                    CONFIG_RESTART => self.state = ConfigState::Restart,
                    CONFIG_ACTIVE => {
                        self.state = ConfigState::Computing;
                        self.compute_start = Some(t);
                        self.computed = false;
                    }
                    CONFIG_SLEEP => {
                        self.state = ConfigState::Sleep;
                        self.compute_start = None;
                        self.computed = false;
                    }
                    // Stored but advances nothing: the BSP writes only these three.
                    _ => {}
                }
            }
            REG_INT_CONF | REG_SOC_ALERT | REG_TEMP_MAX | REG_TEMP_MIN => {
                self.regs[usize::from(reg)] = value;
            }
            // VERSION, VCELL, SOC and TEMP are read only; the rest is unmapped.
            _ => {}
        }
    }
}

/// Rejects a snapshot vector of the wrong length, which would put register accesses one index
/// from a panic.
fn fixed_len<'de, D: Deserializer<'de>>(
    d: D,
    len: usize,
    expected: &'static str,
) -> Result<Vec<u8>, D::Error> {
    let bytes = Vec::<u8>::deserialize(d)?;
    if bytes.len() != len {
        return Err(D::Error::invalid_length(bytes.len(), &expected));
    }
    Ok(bytes)
}

fn named_regs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    fixed_len(d, NAMED_REGS, "16 named register bytes (0x00 to 0x0F)")
}

fn profile_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    fixed_len(d, PROFILE_LEN, "80 profile bytes (0x10 to 0x5F)")
}

/// One first-order lag step toward `target` over `t0..t1`, integer only:
/// `delta = (target - from) * dt / (dt + tau)`. Monotone and never overshooting, but not
/// step-size independent; determinism holds because the sample points are guest reads and the 1 s
/// event. The step rounds away from `from`: truncating stalls a whole percent short forever.
fn lag(from: u32, target: u32, t0: VTime, t1: VTime) -> u32 {
    let dt = i128::from(t1.0.saturating_sub(t0.0));
    if dt == 0 {
        return from;
    }
    let tau = i128::from(SOC_LAG.0);
    let gap = i128::from(target) - i128::from(from);
    let step = (gap.abs() * dt + dt + tau - 1) / (dt + tau);
    (i128::from(from) + gap.signum() * step).clamp(0, 100_000) as u32
}

impl I2cDevice for Cw2017 {
    fn address(&self) -> u8 {
        ADDRESS
    }

    /// Acknowledges the address, which is all `bsp_battery_init` checks for presence.
    fn start(&mut self, _t: VTime, read: bool) -> bool {
        self.phase = if read { Phase::Read } else { Phase::Pointer };
        true
    }

    fn write(&mut self, t: VTime, byte: u8) -> bool {
        match self.phase {
            Phase::Pointer => {
                self.pointer = byte;
                self.phase = Phase::Data;
            }
            Phase::Data => {
                self.write_reg(t, self.pointer, byte);
                self.pointer = self.pointer.wrapping_add(1);
            }
            Phase::Idle | Phase::Read => {}
        }
        true
    }

    /// Returns the register at the pointer, which advances (UNVERIFIED; the BSP's two-byte reads
    /// rely on it).
    fn read(&mut self, _t: VTime) -> u8 {
        let value = self.reg(self.pointer);
        self.pointer = self.pointer.wrapping_add(1);
        value
    }

    fn stop(&mut self, _t: VTime) {
        self.phase = Phase::Idle;
    }
}

impl Chip for Cw2017 {
    /// Reaching the `battery` domain means a disconnect: datasheet power-on state.
    fn reset(&mut self, domain: BoardDomain) {
        if domain == BoardDomain::Battery {
            *self = Cw2017::fresh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::value::{Error as ValueError, SeqDeserializer};

    #[test]
    fn the_lag_step_is_monotone_and_cadence_shaped() {
        let half_tau = VTime(SOC_LAG.0 / 2);
        let one_tau = VTime(SOC_LAG.0);
        let one = lag(0, 100_000, VTime(0), one_tau);
        assert_eq!(one, 50_000);
        // Two halves land higher: the second step starts from further on.
        let first = lag(0, 100_000, VTime(0), half_tau);
        assert_eq!(first, 33_334);
        let two = lag(first, 100_000, half_tau, one_tau);
        assert_eq!(two, 55_556);
        assert!(two > one, "the split step is not the single step");
        for steps in [1u64, 2, 7, 100] {
            let mut soc = 0;
            for step in 1..=steps {
                let t0 = VTime(SOC_LAG.0 * 4 * (step - 1) / steps);
                let t1 = VTime(SOC_LAG.0 * 4 * step / steps);
                let next = lag(soc, 100_000, t0, t1);
                assert!(next >= soc && next <= 100_000, "{steps} steps: {next}");
                soc = next;
            }
        }
        assert_eq!(lag(42_000, 100_000, one_tau, one_tau), 42_000);
    }

    #[test]
    fn a_register_file_of_the_wrong_length_is_rejected_on_load() {
        let seq = |bytes: Vec<u8>| SeqDeserializer::<_, ValueError>::new(bytes.into_iter());
        assert!(named_regs(seq(vec![0; NAMED_REGS])).is_ok());
        assert!(named_regs(seq(vec![0; NAMED_REGS - 1])).is_err());
        assert!(named_regs(seq(Vec::new())).is_err());
        assert!(profile_bytes(seq(vec![0; PROFILE_LEN])).is_ok());
        assert!(profile_bytes(seq(vec![0; PROFILE_LEN + 1])).is_err());
        assert!(profile_bytes(seq(Vec::new())).is_err());
    }
}
