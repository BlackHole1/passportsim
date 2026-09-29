//! ES8311 audio codec on I2C0 at 7-bit address 0x18 (ES8311 datasheet,
//! `specs/es8311-sequences.toml`).
//!
//! An I2C-controlled, I2S-slave codec. Modeled: the register file, the DAC/ADC state derived from
//! it, playback into a [`PcmLog`] and capture from a [`PcmSource`]. The I2S clock registers that
//! set `fs` live in the SoC I2S0 model; this file only checks the MCLK relation.

use pemu_core::sched::ChipId;
use pemu_core::time::{VTime, frame_time};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::traits::{BoardDomain, Chip, I2cDevice, I2sCodec, PcmFormat};

/// 7-bit I2C address of the codec (the BSP passes the 8-bit 0x30).
pub const ADDRESS: u8 = 0x18;

/// Chip id of the codec: its bus address, unique on this board. UNVERIFIED numbering.
pub const CHIP_ID: ChipId = ChipId(ADDRESS as u16);

pub const REG_COUNT: usize = 256;

/// Registers whose reset value is not zero; every other register resets to 0x00.
pub const RESET_VALUES: &[(u8, u8)] = &[
    (0x00, 0x1F),
    (0x03, 0x10),
    (0x04, 0x10),
    (0x06, 0x03),
    (0x08, 0xFF),
    (0x0C, 0x20),
    (0x0D, 0xFC),
    (0x0E, 0x6A),
    (0x10, 0x13),
    (0x11, 0x7C),
    (0x12, 0x02),
    (0x13, 0x40),
    (0x14, 0x10),
    (0x16, 0x04),
    (0x1B, 0x0C),
    (0x1C, 0x4C),
    (0x37, 0x08),
    (0xFD, 0x83),
    (0xFE, 0x11),
];

/// Registers the host cannot write: the read-only flag register and the three chip-id registers.
pub const READ_ONLY: &[u8] = &[0xFC, 0xFD, 0xFE, 0xFF];

/// INI_REG: writing bit 0 returns every other register to its reset value.
const REG_INI: u8 = 0xFA;
/// RESET: CSM_ON bit 7, MSC bit 6 (1 = master).
const REG_RESET: u8 = 0x00;
/// CLK: MCLK_SEL 7, MCLK_INV 6, MCLK_ON 5, BCLK_ON 4, CLKADC_ON 3, CLKDAC_ON 2.
const REG_CLK: u8 = 0x01;
/// SDP IN (DAC serial port): SDP_IN_SEL 7, SDP_IN_MUTE 6, WL bits 4 to 2, FMT bits 1 to 0.
const REG_SDP_IN: u8 = 0x09;
/// SDP OUT (ADC serial port), same layout as `0x09` without SEL.
const REG_SDP_OUT: u8 = 0x0A;
/// Analog power-down: PDN_ANA bit 7 down to VMIDSEL bits 1 to 0.
const REG_PDN_ANA: u8 = 0x0D;
/// PDN_PGA bit 6, PDN_MOD bit 5, RST_MOD bit 4.
const REG_PDN_PGA: u8 = 0x0E;
/// PDN_DAC bit 1, ENREFR bit 0.
const REG_PDN_DAC: u8 = 0x12;
/// DMIC_ON bit 6, LINSEL bit 4, PGAGAIN bits 3 to 0 in 3 dB steps.
const REG_PGA: u8 = 0x14;
/// ADC_SYNC bit 5, ADC_INV bit 4, ADC_RAMCLR bit 3, ADC_SCALE bits 2 to 0 in 6 dB steps.
const REG_ADC_SCALE: u8 = 0x16;
/// ADC_VOLUME, 0.5 dB per step with 0xBF = 0 dB.
const REG_ADC_VOLUME: u8 = 0x17;
/// DAC_DSMMUTE bit 6, DAC_DEMMUTE bit 5; the driver mutes with mask 0x60.
const REG_DAC_MUTE: u8 = 0x31;
/// DAC_VOLUME, 0.5 dB per step with 0xBF = 0 dB.
pub const REG_DAC_VOLUME: u8 = 0x32;
/// ADC2DAC_SEL bit 7, ADCDAT_SEL bits 6 to 4 (0 = ADC on both slots).
const REG_ADCDAT: u8 = 0x44;

/// 0 dB on both volume registers (0x00 = -95.5 dB, 0xFF = +32 dB).
pub const VOLUME_0DB: u8 = 0xBF;

pub const MCLK_MULTIPLE: u32 = 256;

/// The ES8311 codec. `regs` is a `Vec` (serde derives nothing for arrays over 32).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Es8311 {
    #[serde(deserialize_with = "reg_file")]
    regs: Vec<u8>,
    /// Register pointer set by the first byte of every write transaction.
    pointer: u8,
    phase: Phase,
    mode: AudioMode,
    /// Frame shape the I2S side is clocking. Zero `fs_hz` means not programmed yet.
    format: AudioFormat,
    pub log: PcmLog,
}

/// Rejects a register file that is not exactly [`REG_COUNT`] bytes, so a bad snapshot fails to
/// load instead of panicking later.
fn reg_file<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let regs = Vec::<u8>::deserialize(d)?;
    if regs.len() != REG_COUNT {
        return Err(D::Error::invalid_length(regs.len(), &"256 register bytes"));
    }
    Ok(regs)
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
enum Phase {
    Idle,
    Pointer,
    Data,
    Read,
}

/// Playback and capture mode.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum AudioMode {
    /// Exact transmitted samples, volume as metadata. What goldens compare.
    #[default]
    Digital,
    /// DAC gain applied to playback and PGA gain to capture; class C.
    Analog,
}

impl Default for Es8311 {
    fn default() -> Self {
        Es8311::new()
    }
}

impl Es8311 {
    pub fn new() -> Self {
        Es8311 {
            regs: reset_file(),
            pointer: 0,
            phase: Phase::Idle,
            mode: AudioMode::Digital,
            format: AudioFormat::UNCONFIGURED,
            log: PcmLog::new(PcmLog::DEFAULT_CAPACITY),
        }
    }

    pub fn reg(&self, reg: u8) -> u8 {
        self.regs[reg as usize]
    }

    pub fn registers(&self) -> &[u8] {
        &self.regs
    }

    pub fn mode(&self) -> AudioMode {
        self.mode
    }

    /// Switches the mode. Refused mid-stream, because a [`PcmLog`] record carries its mode.
    pub fn set_mode(&mut self, mode: AudioMode) -> Result<(), ModeChangeRefused> {
        if mode != self.mode && self.log.stream_open() {
            return Err(ModeChangeRefused { requested: mode });
        }
        self.mode = mode;
        Ok(())
    }

    /// Writes one register; the only path that changes the register file.
    pub fn write_reg(&mut self, reg: u8, value: u8) {
        if READ_ONLY.contains(&reg) {
            return;
        }
        self.regs[reg as usize] = value;
        if reg == REG_INI && value & 0x01 != 0 {
            // INI_REG resets every register except 0xFA itself.
            let ini = self.regs[REG_INI as usize];
            self.regs = reset_file();
            self.regs[REG_INI as usize] = ini;
        }
    }
}

fn reset_file() -> Vec<u8> {
    let mut regs = vec![0u8; REG_COUNT];
    for &(reg, value) in RESET_VALUES {
        regs[reg as usize] = value;
    }
    regs
}

/// Refusal of [`Es8311::set_mode`] while a [`PcmLog`] record is open.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ModeChangeRefused {
    pub requested: AudioMode,
}

// -- Derived control state ----------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct DacState {
    pub active: bool,
    pub volume_reg: u8,
    /// Volume in half-decibel steps around 0 dB, `volume_reg - 0xBF`.
    pub volume_half_db: i16,
    /// Whether the driver's mute bits (`0x31 & 0x60`) or the serial-port mute (`0x09` bit 6) are
    /// set.
    pub muted: bool,
    /// Whether the DAC takes the left I2S slot, bit 7 of `0x09` clear.
    pub left_slot: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct AdcState {
    pub active: bool,
    /// Analog PGA gain in decibels, `0x14` bits 3 to 0 in 3 dB steps.
    pub pga_db: u16,
    /// Digital scale in decibels, `0x16` bits 2 to 0 in 6 dB steps.
    pub scale_db: u16,
    /// ADC volume in half-decibel steps around 0 dB, `0x17 - 0xBF`.
    pub volume_half_db: i16,
    /// Whether the serial-port output mute bit (bit 6 of `0x0A`) is set.
    pub muted: bool,
    /// `ADCDAT_SEL`, `0x44` bits 6 to 4; 0 puts ADC data in both slots, 5 is ADC+DACR.
    pub adcdat_sel: u8,
}

impl Es8311 {
    /// Derived DAC state (`dac` is taken by [`I2sCodec`]). Active when CSM_ON, CLKDAC_ON, `PDN_DAC`
    /// clear, `SDP_IN_MUTE` clear, `0x31 & 0x60 == 0` and `0x0D` bit 7 set; every driver path also
    /// satisfies the stricter `0x0D == 0x01`.
    pub fn dac_state(&self) -> DacState {
        let volume_reg = self.reg(REG_DAC_VOLUME);
        let muted = self.reg(REG_DAC_MUTE) & 0x60 != 0 || self.reg(REG_SDP_IN) & 0x40 != 0;
        let powered = self.reg(REG_RESET) & 0x80 != 0
            && self.reg(REG_CLK) & 0x04 != 0
            && self.reg(REG_PDN_ANA) & 0x80 == 0
            && self.reg(REG_PDN_DAC) & 0x02 == 0;
        DacState {
            active: powered && !muted,
            volume_reg,
            volume_half_db: i16::from(volume_reg) - i16::from(VOLUME_0DB),
            muted,
            left_slot: self.reg(REG_SDP_IN) & 0x80 == 0,
        }
    }

    /// Derived ADC state. Active when `0x0E` bits 6 and 5 are clear (PGA and modulator powered),
    /// `0x14` bit 4 is set (analog microphone) and `0x0A` bit 6 is clear (output not muted).
    pub fn adc_state(&self) -> AdcState {
        let muted = self.reg(REG_SDP_OUT) & 0x40 != 0;
        let powered = self.reg(REG_PDN_PGA) & 0x60 == 0 && self.reg(REG_PGA) & 0x10 != 0;
        AdcState {
            active: powered && !muted,
            pga_db: u16::from(self.reg(REG_PGA) & 0x0F) * 3,
            scale_db: u16::from(self.reg(REG_ADC_SCALE) & 0x07) * 6,
            volume_half_db: i16::from(self.reg(REG_ADC_VOLUME)) - i16::from(VOLUME_0DB),
            muted,
            adcdat_sel: (self.reg(REG_ADCDAT) >> 4) & 0x07,
        }
    }

    /// Serial-port word length from `SDP_IN_WL`. Only code 3 (16 bits) is documented; others give
    /// `None`.
    pub fn word_length(&self) -> Option<u8> {
        match (self.reg(REG_SDP_IN) >> 2) & 0x07 {
            3 => Some(16),
            _ => None,
        }
    }

    /// Whether the serial port is in I2S format, `SDP_IN_FMT` (`0x09` bits 1 to 0) 0.
    pub fn is_i2s_format(&self) -> bool {
        self.reg(REG_SDP_IN) & 0x03 == 0
    }

    /// The MCLK the codec expects for `fs_hz`. Saturates rather than wrapping.
    pub fn expected_mclk(fs_hz: u32) -> u32 {
        fs_hz.saturating_mul(MCLK_MULTIPLE)
    }

    /// Checks the MCLK the I2S clock produces; a mismatch is a class-B warning, never an error.
    pub fn check_mclk(fs_hz: u32, mclk_hz: u32) -> Option<MclkMismatch> {
        let expected = Es8311::expected_mclk(fs_hz);
        (expected != mclk_hz).then_some(MclkMismatch {
            fs_hz,
            mclk_hz,
            expected_mclk_hz: expected,
        })
    }
}

/// A class-B warning: the I2S MCLK is not 256 times the sample rate.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MclkMismatch {
    pub fs_hz: u32,
    pub mclk_hz: u32,
    pub expected_mclk_hz: u32,
}

// -- The host volume curve ----------------------------------------------------------------------

/// The driver's hardware-gain constant in micro-decibels, `20 * log10(3.3 / 5.0)`, as an integer
/// that reproduces every row of the driver table without libm.
const HW_GAIN_UDB: i64 = 3_609_121;

/// Micro-decibels of the lowest volume step, 0x00 = -95.5 dB.
const VOLUME_FLOOR_UDB: i64 = 95_500_000;

/// [`REG_DAC_VOLUME`] for a host volume percentage, the curve the codec device applies: 0 means
/// -96 dB, any other `p` means `-50 + 0.5 p` dB, minus the hardware-gain constant; the register is
/// the truncated half-decibel step above the -95.5 dB floor, clamped into a byte. The truncation
/// is the driver's C cast. Volume 80 gives 0xB2.
pub fn volume_register(percent: u8) -> u8 {
    let db_udb: i64 = if percent == 0 {
        -96_000_000
    } else {
        -50_000_000 + 500_000 * i64::from(percent)
    };
    let above_floor = db_udb + HW_GAIN_UDB + VOLUME_FLOOR_UDB;
    if above_floor <= 0 {
        return 0;
    }
    let steps = (above_floor * 2) / 1_000_000;
    steps.clamp(0, 255) as u8
}

// -- I2C control plane ---------------------------------------------------------------------------

impl I2cDevice for Es8311 {
    fn address(&self) -> u8 {
        ADDRESS
    }

    /// The chip acknowledges every address and data byte.
    fn start(&mut self, _t: VTime, read: bool) -> bool {
        self.phase = if read { Phase::Read } else { Phase::Pointer };
        true
    }

    /// The first byte of a write is the register pointer. No auto-increment (UNVERIFIED, unused by
    /// the driver): further data bytes overwrite the same register.
    fn write(&mut self, _t: VTime, byte: u8) -> bool {
        match self.phase {
            Phase::Pointer => {
                self.pointer = byte;
                self.phase = Phase::Data;
            }
            Phase::Data => self.write_reg(self.pointer, byte),
            // Cannot happen on the BSP paths; ignored rather than inventing a state.
            Phase::Idle | Phase::Read => {}
        }
        true
    }

    /// Returns the register the last write named, with no auto-increment.
    fn read(&mut self, _t: VTime) -> u8 {
        self.reg(self.pointer)
    }

    fn stop(&mut self, _t: VTime) {
        self.phase = Phase::Idle;
    }
}

impl Chip for Es8311 {
    /// The register array is in `board_rail`; after an MCU reset the guest reprograms it anyway.
    fn reset(&mut self, domain: BoardDomain) {
        if domain == BoardDomain::BoardRail {
            let log = core::mem::replace(&mut self.log, PcmLog::new(PcmLog::DEFAULT_CAPACITY));
            *self = Es8311::new();
            self.log = log;
            self.log.end_stream();
        }
    }
}

// -- Audio paths ---------------------------------------------------------------------------------

/// Frame shape the I2S side clocks. As a slave the codec cannot derive it from its own
/// registers.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct AudioFormat {
    /// Sample rate in hertz. Zero means the I2S clock registers are not programmed.
    pub fs_hz: u32,
    pub bits: u8,
    pub channels: u16,
}

impl AudioFormat {
    pub const UNCONFIGURED: AudioFormat = AudioFormat {
        fs_hz: 0,
        bits: 0,
        channels: 0,
    };

    /// The audio demo's format: 16 kHz, 16-bit, two slots, payload left.
    pub const DEMO: AudioFormat = AudioFormat {
        fs_hz: 16_000,
        bits: 16,
        channels: 2,
    };
}

/// A host source of mono capture samples at the codec's sample rate.
pub trait PcmSource {
    /// Fills `out` from `t` and returns how many samples it produced; the codec pads with silence.
    fn fill(&mut self, t: VTime, out: &mut [i16]) -> usize;
}

/// A [`PcmSource`] that plays a fixed sample list once and then runs dry.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct SliceSource {
    samples: Vec<i16>,
    at: usize,
}

impl SliceSource {
    pub fn new(samples: &[i16]) -> Self {
        SliceSource {
            samples: samples.to_vec(),
            at: 0,
        }
    }

    pub fn remaining(&self) -> usize {
        self.samples.len() - self.at
    }
}

impl PcmSource for SliceSource {
    fn fill(&mut self, _t: VTime, out: &mut [i16]) -> usize {
        let n = out.len().min(self.remaining());
        out[..n].copy_from_slice(&self.samples[self.at..self.at + n]);
        self.at += n;
        n
    }
}

/// One run of transmitted samples with the codec state that produced it.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PcmLogRecord {
    pub vt_start: VTime,
    pub fs_hz: u32,
    /// Interleaved channels per frame of the recorded samples; the DAC records one.
    pub channels: u16,
    pub first: usize,
    pub volume_reg: u8,
    pub silent: bool,
    pub mode: AudioMode,
}

/// The transmitted-PCM log. Keeps the newest [`PcmLog::capacity`] samples so a snapshot stays
/// bounded.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct PcmLog {
    records: Vec<PcmLogRecord>,
    samples: Vec<i16>,
    capacity: usize,
    dropped: u64,
    /// Whether the next frame continues the newest record.
    open: bool,
    /// Virtual time of the sample after the open run. Not derived from `vt_start`, because
    /// [`PcmLog::evict`] drops samples off a record's front without moving its `vt_start`.
    next_vt: VTime,
}

impl PcmLog {
    /// Samples a log keeps by default: one second of 16 kHz mono playback, twice over.
    pub const DEFAULT_CAPACITY: usize = 32_000;

    pub fn new(capacity: usize) -> Self {
        PcmLog {
            records: Vec::new(),
            samples: Vec::new(),
            capacity,
            dropped: 0,
            open: false,
            next_vt: VTime(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub fn records(&self) -> &[PcmLogRecord] {
        &self.records
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn stream_open(&self) -> bool {
        self.open
    }

    /// Closes the open run; needed before a format or mode change.
    pub fn end_stream(&mut self) {
        self.open = false;
    }

    pub fn clear(&mut self) {
        self.records.clear();
        self.samples.clear();
        self.open = false;
        self.next_vt = VTime(0);
    }
}

/// Amplitude ratios of 0 to 9.5 dB in half-decibel steps, Q16: `round(65536 * 10^(n / 40))`.
/// [`gain_q16`] combines them with whole 10 dB steps, so no libm is needed.
const HALF_DB_Q16: [u64; 20] = [
    65536, 69421, 73534, 77891, 82506, 87396, 92575, 98061, 103872, 110026, 116544, 123450, 130765,
    138514, 146722, 155417, 164625, 174381, 184711, 195656,
];

/// The amplitude ratio of 10 dB, `round(65536 * 10^0.5)`, in Q16.
const TEN_DB_Q16: u64 = 207243;

/// Amplitude ratio of `half_db` half-decibel steps in Q16, saturating at +96 dB, never zero
/// (class C).
fn gain_q16(half_db: i16) -> u64 {
    let positive = |steps: u32| -> u64 {
        let tens = steps / 20;
        let rest = HALF_DB_Q16[(steps % 20) as usize];
        // Each whole 10 dB multiplies by TEN_DB_Q16 / 65536; stop once saturated.
        (0..tens).fold(rest, |acc, _| (acc * TEN_DB_Q16 / 65536).min(1 << 32))
    };
    if half_db >= 0 {
        positive(half_db as u32).min(1 << 32)
    } else {
        let down = positive(half_db.unsigned_abs() as u32);
        (65536u64 * 65536 / down).max(1)
    }
}

fn apply_gain(sample: i16, gain: u64) -> i16 {
    let scaled = (i64::from(sample) * gain as i64) >> 16;
    scaled.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}

impl Es8311 {
    /// Tells the codec what the I2S side clocks; a change closes the open [`PcmLog`] run.
    pub fn set_format(&mut self, format: AudioFormat) {
        if format != self.format {
            self.log.end_stream();
            self.format = format;
        }
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Plays one I2S TX buffer. The DAC takes the left slot, so runs are mono; `analog` applies the
    /// DAC gain (class C). A silent path records zeros to stay aligned with virtual time; nothing
    /// is recorded while `UNCONFIGURED`.
    pub fn play(&mut self, t: VTime, frames: &[i16]) -> usize {
        let fmt = self.format;
        if fmt.fs_hz == 0 || fmt.channels == 0 {
            return 0;
        }
        let dac = self.dac_state();
        let slot = usize::from(!dac.left_slot);
        let stride = usize::from(fmt.channels);
        let gain = (self.mode == AudioMode::Analog).then(|| gain_q16(dac.volume_half_db));
        let mut mono: Vec<i16> = Vec::with_capacity(frames.len() / stride + 1);
        for frame in frames.chunks_exact(stride) {
            let sample = frame[slot.min(stride - 1)];
            mono.push(match (dac.active, gain) {
                (false, _) => 0,
                (true, None) => sample,
                (true, Some(g)) => apply_gain(sample, g),
            });
        }
        let written = mono.len();
        self.log.write(
            PcmLogRecord {
                vt_start: t,
                fs_hz: fmt.fs_hz,
                channels: 1,
                first: 0,
                volume_reg: dac.volume_reg,
                silent: !dac.active,
                mode: self.mode,
            },
            &mono,
        );
        written
    }

    /// Fills one I2S RX buffer from a host source and returns how many real samples it gave.
    /// `analog` applies PGA, scale and ADC volume (class C); a muted or powered-down path gives
    /// silence. While `UNCONFIGURED` nothing is consumed and `out` is untouched.
    pub fn capture<S: PcmSource + ?Sized>(
        &mut self,
        t: VTime,
        source: &mut S,
        out: &mut [i16],
    ) -> usize {
        let fmt = self.format;
        if fmt.fs_hz == 0 || fmt.channels == 0 {
            return 0;
        }
        let stride = usize::from(fmt.channels);
        let adc = self.adc_state();
        let frames = out.len() / stride;
        let mut mono = vec![0i16; frames];
        let got = if adc.active {
            source.fill(t, &mut mono)
        } else {
            0
        };
        if !adc.active {
            mono.fill(0);
        } else if self.mode == AudioMode::Analog {
            let analog_db = i16::try_from(adc.pga_db + adc.scale_db).unwrap_or(i16::MAX);
            let half_db = analog_db.saturating_mul(2) + adc.volume_half_db;
            let gain = gain_q16(half_db);
            for sample in &mut mono {
                *sample = apply_gain(*sample, gain);
            }
        }
        out.fill(0);
        // ADCDAT_SEL 5 (ADC+DACR, the AEC reference) is never selected by the firmware and the DAC
        // right loopback is not modeled (UNVERIFIED): any non-zero value puts the microphone on
        // slot 0 only.
        let both = adc.adcdat_sel == 0;
        for (frame, sample) in out.chunks_exact_mut(stride).zip(mono.iter().copied()) {
            frame[0] = sample;
            if both {
                for slot in &mut frame[1..] {
                    *slot = sample;
                }
            }
        }
        got
    }
}

/// Both methods ignore `fmt` (the frame shape comes from [`Es8311::set_format`]); `adc` captures
/// silence because the trait carries no [`PcmSource`].
impl I2sCodec for Es8311 {
    fn dac(&mut self, t: VTime, _fmt: PcmFormat, frames: &[i16]) {
        self.play(t, frames);
    }

    fn adc(&mut self, t: VTime, _fmt: PcmFormat, out: &mut [i16]) {
        self.capture(t, &mut SliceSource::default(), out);
    }
}

impl PcmLog {
    /// Appends a run, continuing the newest record only if codec state and frame shape match, no
    /// [`PcmLog::end_stream`] came between and the samples arrive where it left off, since a record
    /// is read back as `vt_start` plus an index. One sample period of tolerance absorbs flooring.
    fn write(&mut self, header: PcmLogRecord, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let tolerance = frame_time(VTime(0), 1, header.fs_hz).0;
        let continues = self.open
            && header.vt_start.0.abs_diff(self.next_vt.0) <= tolerance
            && self.records.last().is_some_and(|r| {
                r.fs_hz == header.fs_hz
                    && r.channels == header.channels
                    && r.volume_reg == header.volume_reg
                    && r.silent == header.silent
                    && r.mode == header.mode
            });
        if !continues {
            self.records.push(PcmLogRecord {
                first: self.samples.len(),
                ..header
            });
        }
        self.samples.extend_from_slice(samples);
        self.open = true;
        // From this burst's start, so flooring cannot drift over a long run.
        let frames = samples.len() as u64 / u64::from(header.channels.max(1));
        self.next_vt = frame_time(header.vt_start, frames, header.fs_hz);
        self.evict();
    }

    fn evict(&mut self) {
        if self.samples.len() <= self.capacity {
            return;
        }
        let drop = self.samples.len() - self.capacity;
        self.samples.drain(..drop);
        self.dropped += drop as u64;
        for record in &mut self.records {
            record.first = record.first.saturating_sub(drop);
        }
        while self.records.len() > 1 && self.records[1].first == 0 {
            self.records.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::value::{Error as ValueError, SeqDeserializer};

    #[test]
    fn a_register_file_of_the_wrong_length_is_rejected_on_load() {
        let seq = |bytes: Vec<u8>| SeqDeserializer::<_, ValueError>::new(bytes.into_iter());
        assert!(reg_file(seq(vec![0; REG_COUNT])).is_ok());
        assert!(reg_file(seq(vec![0; REG_COUNT - 1])).is_err());
        assert!(reg_file(seq(Vec::new())).is_err());
    }
}
