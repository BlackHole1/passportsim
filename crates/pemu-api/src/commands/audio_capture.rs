//! `passportsim audio_capture`: the guest's playback as a PCM capture, a WAV artifact and a tone
//! analysis.
//!
//! The samples are `HostIo::audio_out`, the [`PcmRing`] `wiring::i2s` appends every transmitted
//! I2S0 buffer to. The default `digital` mode, which goldens compare, is the exact transmitted
//! samples; `analog` needs the ES8311 DAC gain, which `MachineApi` does not reach, so it is
//! refused.
//!
//! This core crate holds the pure parts: gathering ([`Capture`]), tone analysis ([`analyze`], plain
//! arithmetic with the integer CORDIC sine) and WAV encoding ([`wav_bytes`]), so the artifact hash
//! is the same on every host and in the browser. The host writes the file ([`AudioIo`]).

use std::sync::{Mutex, MutexGuard, OnceLock};

use pemu_core::hostio::PcmRing;
use pemu_core::time::VTime;
use pemu_machine::stops::StopReason;

use crate::error::{ApiError, E_DEADLOCK, E_INTERNAL, E_LEASE, E_STATE, E_USAGE};
use crate::output::{ArtifactRef, Output};
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use crate::args::{instance_schema, object, only, opt_bool, opt_str, opt_u64, usage};
use crate::pool::Pool;
use crate::session::Session;

/// Bounds the lag search to `fs / 20` samples; speech and test tones are well above it.
pub const MIN_FUNDAMENTAL_HZ: u32 = 20;

/// 8 LSB is about -72 dBFS, far under the tones measured.
pub const SILENCE_PEAK: u16 = 8;

/// The first dip under it is the period, which keeps an octave error from winning on a
/// harmonic-rich signal.
const YIN_THRESHOLD: f64 = 0.1;

/// A `u16` so `i16::MIN` reads 32768.
#[must_use]
pub fn peak(samples: &[i16]) -> u16 {
    samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0)
}

/// `sqrt` is correctly rounded by IEEE 754 on every host.
#[must_use]
pub fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|s| {
            let v = f64::from(*s);
            v * v
        })
        .sum();
    (sum / samples.len() as f64).sqrt()
}

/// The fundamental of a mono buffer at `fs` hertz, or `None` for silence, a buffer shorter than two
/// periods of [`MIN_FUNDAMENTAL_HZ`], or no periodicity. Plain arithmetic only:
///
/// 1. the analysis region is the loudest stretch, so leading silence cannot hide a tone;
/// 2. YIN gives a coarse period: the first local minimum of the cumulative-mean-normalized
///    difference under `YIN_THRESHOLD`, else the smallest local minimum within
///    `YIN_NOISE_MARGIN` of the global one; none under 0.5 means no periodicity;
/// 3. YIN can lock onto a multiple of a short period (2450 Hz at 16 kHz dips first at lag 13), so
///    the fundamental is the GCD of the harmonics `k * fs / tau` carrying at least
///    `HARMONIC_SHARE` of the strongest;
/// 4. the strongest harmonic is located as a spectral peak (Bartlett window, quarter-bin grid,
///    parabolic interpolation) over growing windows up to the whole buffer, so gaps of zeros do not
///    move it and precision reaches 0.5 %.
#[must_use]
pub fn fundamental_hz(samples: &[i16], fs: u32) -> Option<f64> {
    if fs == 0 || peak(samples) < SILENCE_PEAK {
        return None;
    }
    let max_lag = (fs / MIN_FUNDAMENTAL_HZ) as usize;
    if samples.len() < 2 * max_lag + 2 {
        return None;
    }
    let mean = samples.iter().map(|s| f64::from(*s)).sum::<f64>() / samples.len() as f64;
    let x: Vec<f64> = samples.iter().map(|s| f64::from(*s) - mean).collect();
    // The difference window is one longest period, and the region adds the lags it is compared at.
    let region_len = (2 * max_lag + 1).min(x.len());
    let start = loudest_region(&x, region_len);
    let region = Span {
        start,
        len: region_len,
    };
    let tau = yin_period(&x[start..start + region_len], max_lag)?;
    let (divisor, harmonic, span, located) = harmonic_fundamental(&x, region, tau, fs)?;
    let refined = refine_peak(&x, span, located, fs);
    Some(refined * divisor as f64 / harmonic as f64)
}

/// With no dip under `YIN_THRESHOLD`, a local minimum this close to the global one is as good a
/// period, and the smallest such lag is the fundamental's.
const YIN_NOISE_MARGIN: f64 = 0.1;

/// Bartlett sidelobes stay under 0.06 and white noise at 0 dB SNR under about 0.15 in a 0.1 s
/// region, so 0.3 separates components from leakage.
const HARMONIC_SHARE: f64 = 0.3;

/// By the pigeonhole bound, a multiple-period lock needs fewer than 16 harmonics for every period
/// of at least two samples.
const MAX_HARMONICS: usize = 16;

#[derive(Copy, Clone, Debug)]
struct Span {
    start: usize,
    len: usize,
}

/// Searched in eighth-length steps.
fn loudest_region(x: &[f64], len: usize) -> usize {
    if len >= x.len() {
        return 0;
    }
    let step = (len / 8).max(1);
    let energy = |start: usize| x[start..start + len].iter().map(|v| v * v).sum::<f64>();
    let mut best = (0, energy(0));
    let mut start = step;
    while start + len <= x.len() {
        let e = energy(start);
        if e > best.1 {
            best = (start, e);
        }
        start += step;
    }
    best.0
}

/// Stage 2 of [`fundamental_hz`], from a region `max_lag + 1` samples longer than the difference
/// window.
fn yin_period(x: &[f64], max_lag: usize) -> Option<f64> {
    let window = x.len() - max_lag - 1;
    let mut d = vec![0.0f64; max_lag + 2];
    for (tau, slot) in d.iter_mut().enumerate().skip(1) {
        // Four lanes summed in a fixed order: the same result on every host, and vectorizable.
        let (a, b) = (&x[..window], &x[tau..tau + window]);
        let mut lanes = [0.0f64; 4];
        let whole = window - window % 4;
        for (ca, cb) in a[..whole].chunks_exact(4).zip(b[..whole].chunks_exact(4)) {
            for lane in 0..4 {
                let diff = ca[lane] - cb[lane];
                lanes[lane] += diff * diff;
            }
        }
        let mut sum = lanes[0] + lanes[1] + lanes[2] + lanes[3];
        for (va, vb) in a[whole..].iter().zip(&b[whole..]) {
            let diff = va - vb;
            sum += diff * diff;
        }
        *slot = sum;
    }
    // `d'(0)` is 1 by definition.
    let mut cmnd = vec![1.0f64; max_lag + 2];
    let mut running = 0.0;
    for tau in 1..=max_lag + 1 {
        running += d[tau];
        cmnd[tau] = if running > 0.0 {
            d[tau] * tau as f64 / running
        } else {
            1.0
        };
    }
    let is_local_min = |t: usize| cmnd[t] <= cmnd[t - 1] && cmnd[t] <= cmnd[t + 1];
    // Two samples is the shortest period a sampled signal can carry.
    let tau = match (2..=max_lag).find(|t| cmnd[*t] < YIN_THRESHOLD) {
        Some(mut tau) => {
            while tau < max_lag && cmnd[tau + 1] < cmnd[tau] {
                tau += 1;
            }
            tau
        }
        None => {
            let floor = (2..=max_lag).map(|t| cmnd[t]).fold(f64::INFINITY, f64::min);
            // Nothing even half-way periodic: noise, not a tone.
            if floor >= 0.5 {
                return None;
            }
            (2..=max_lag).find(|t| is_local_min(*t) && cmnd[*t] <= floor + YIN_NOISE_MARGIN)?
        }
    };
    // Parabolic interpolation through (tau - 1, tau, tau + 1).
    let (a, b, c) = (cmnd[tau - 1], cmnd[tau], cmnd[tau + 1]);
    let denom = a - 2.0 * b + c;
    let offset = if denom.abs() > f64::EPSILON {
        (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    Some(tau as f64 + offset)
}

fn unit_phasor(turns: f64) -> (f64, f64) {
    const QUARTER: u32 = 1 << 30;
    let phase = (turns * 4_294_967_296.0).round() as u64 as u32;
    let scale = 1.0 / f64::from(QUARTER);
    let sin = super::mic_set::sine_q30(phase) as f64 * scale;
    let cos = super::mic_set::sine_q30(phase.wrapping_add(QUARTER)) as f64 * scale;
    (cos, sin)
}

/// Bartlett keeps a strong component's sidelobes near -26 dB without the cosine a Hann window
/// needs.
fn windowed(x: &[f64], span: Span) -> Vec<f64> {
    let half = (span.len as f64 + 1.0) / 2.0;
    x[span.start..span.start + span.len]
        .iter()
        .enumerate()
        .map(|(k, v)| v * (1.0 - ((k as f64 + 1.0 - half) / half).abs()))
        .collect()
}

fn magnitude_at(xw: &[f64], hz: f64, fs: u32) -> f64 {
    let (c, s) = unit_phasor(hz / f64::from(fs));
    let (mut pc, mut ps) = (1.0f64, 0.0f64);
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (k, v) in xw.iter().enumerate() {
        re += v * pc;
        im -= v * ps;
        let next_c = pc * c - ps * s;
        ps = ps * c + pc * s;
        pc = next_c;
        // Keep the rotating phasor on the unit circle over long spans.
        if k & 0xFFF == 0xFFF {
            let norm = (pc * pc + ps * ps).sqrt();
            pc /= norm;
            ps /= norm;
        }
    }
    (re * re + im * im).sqrt()
}

/// Refined by a parabola through the grid points either side.
fn local_peak(x: &[f64], span: Span, lo: f64, hi: f64, step: f64, fs: u32) -> (f64, f64) {
    let xw = windowed(x, span);
    let points = (((hi - lo) / step).round() as usize).max(2);
    let mags: Vec<f64> = (0..=points)
        .map(|i| magnitude_at(&xw, lo + step * i as f64, fs))
        .collect();
    let (best, &top) = mags
        .iter()
        .enumerate()
        .fold((0, &f64::NEG_INFINITY), |acc, cur| {
            if cur.1 > acc.1 { cur } else { acc }
        });
    let mut hz = lo + step * best as f64;
    if best > 0 && best < points {
        let (a, b, c) = (mags[best - 1], top, mags[best + 1]);
        let denom = a - 2.0 * b + c;
        if denom.abs() > f64::EPSILON {
            hz += step * (0.5 * (a - c) / denom).clamp(-0.5, 0.5);
        }
    }
    (hz, top)
}

/// Stage 3 of [`fundamental_hz`]: `(divisor, harmonic, span, located hz)`, where the fundamental is
/// `located * divisor / harmonic`. Each harmonic is searched in a window only as long as its band
/// needs, so a wide band costs a few hundred samples per grid point. Magnitudes are divided by the
/// window's Bartlett gain, so harmonics from different window lengths compare as amplitudes.
fn harmonic_fundamental(
    x: &[f64],
    region: Span,
    tau: f64,
    fs: u32,
) -> Option<(usize, usize, Span, f64)> {
    let yin_hz = f64::from(fs) / tau;
    // The parabola places the period within half a sample, so a short period has a large relative
    // error.
    let spread = (0.6 / tau).max(0.06);
    let nyquist = f64::from(fs) / 2.0;
    let mut found: Vec<(usize, Span, f64, f64)> = Vec::new();
    for k in 1..=MAX_HARMONICS {
        let centre = yin_hz * k as f64;
        // Neighbouring bands never overlap, and a search never crosses Nyquist, where a tone's
        // mirror image sits.
        let band = (centre * spread).min(yin_hz * 0.45);
        let wanted = (8.0 * f64::from(fs) / band).ceil() as usize;
        let len = wanted.clamp(MIN_SEARCH_SPAN.min(region.len), region.len);
        let span = Span {
            start: region.start + (region.len - len) / 2,
            len,
        };
        let bin = f64::from(fs) / len as f64;
        let band = band.max(2.0 * bin);
        let lo = (centre - band).max(f64::from(MIN_FUNDAMENTAL_HZ) / 2.0);
        let hi = (centre + band).min(nyquist);
        if lo >= nyquist {
            break;
        }
        let (hz, magnitude) = local_peak(x, span, lo, hi, bin / 2.0, fs);
        found.push((k, span, hz, magnitude / (len as f64 / 2.0)));
    }
    let strongest = found.iter().map(|f| f.3).fold(0.0, f64::max);
    if strongest <= 0.0 {
        return None;
    }
    let present: Vec<&(usize, Span, f64, f64)> = found
        .iter()
        .filter(|f| f.3 >= strongest * HARMONIC_SHARE)
        .collect();
    let divisor = present.iter().fold(0, |g, f| gcd(g, f.0));
    let (harmonic, span, hz, _) = **present.iter().max_by(|a, b| a.3.total_cmp(&b.3))?;
    Some((divisor, harmonic, span, hz))
}

/// Enough periods of the shortest band for noise to average, and cheap at 48 kHz.
const MIN_SEARCH_SPAN: usize = 256;

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Stage 4 of [`fundamental_hz`]: re-located in windows four times longer each step until the
/// window is the whole buffer.
fn refine_peak(x: &[f64], first: Span, mut hz: f64, fs: u32) -> f64 {
    let nyquist = f64::from(fs) / 2.0;
    let mut span = first;
    loop {
        let bin = f64::from(fs) / span.len as f64;
        let lo = (hz - 2.0 * bin).max(bin);
        let hi = (hz + 2.0 * bin).min(nyquist);
        hz = local_peak(x, span, lo, hi, bin / 4.0, fs).0;
        if span.len >= x.len() {
            return hz;
        }
        let centre = span.start + span.len / 2;
        let len = (span.len * 4).min(x.len());
        let start = centre.saturating_sub(len / 2).min(x.len() - len);
        span = Span { start, len };
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToneReport {
    /// `None` for silence or no periodicity.
    pub fundamental_hz: Option<f64>,
    pub peak: u16,
    /// In LSB.
    pub rms: f64,
    pub samples: usize,
}

impl ToneReport {
    /// The fundamental is rounded to a hundredth of a hertz so hosts print the same text.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "fundamental_hz": self.fundamental_hz.map(round_centi),
            "peak": self.peak,
            "rms": round_centi(self.rms),
            "samples": self.samples,
        })
    }
}

fn round_centi(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[must_use]
pub fn analyze(samples: &[i16], fs: u32) -> ToneReport {
    ToneReport {
        fundamental_hz: fundamental_hz(samples, fs),
        peak: peak(samples),
        rms: rms(samples),
        samples: samples.len(),
    }
}

/// The DAC uses the left slot, channel 0.
#[must_use]
pub fn channel_of(samples: &[i16], channels: u16, channel: u16) -> Vec<i16> {
    let stride = usize::from(channels.max(1));
    samples
        .chunks_exact(stride)
        .filter_map(|frame| frame.get(usize::from(channel)).copied())
        .collect()
}

pub const WAV_HEADER_BYTES: usize = 44;

pub const WAV_MEDIA_TYPE: &str = "audio/wav";

/// `fmt ` chunk with format tag 1 (PCM), then one `data` chunk, little-endian. Integer only, so the
/// bytes and hash are the same on every host. A trailing partial frame is dropped.
#[must_use]
pub fn wav_bytes(fs: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let stride = usize::from(channels.max(1));
    let whole = &samples[..samples.len() - samples.len() % stride];
    let data_len = u32::try_from(whole.len() * 2).unwrap_or(u32::MAX);
    let block_align = channels.saturating_mul(2);
    let mut out = Vec::with_capacity(WAV_HEADER_BYTES + whole.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&data_len.saturating_add(36).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&fs.to_le_bytes());
    out.extend_from_slice(&fs.saturating_mul(u32::from(block_align)).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in whole {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Drained in slices because the ring evicts its oldest samples when full; evicted samples are
/// counted, never invented.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    pub from: u64,
    pub cursor: u64,
    /// Interleaved, oldest first.
    pub samples: Vec<i16>,
    /// One per contiguous run of one format, `first` clipped to the capture. Two headers mean a
    /// time gap or a format change.
    pub runs: Vec<pemu_core::hostio::PcmRecord>,
    pub dropped: u64,
}

impl Capture {
    #[must_use]
    pub fn starting_at(from: u64) -> Capture {
        Capture {
            from,
            cursor: from,
            ..Capture::default()
        }
    }

    /// With the run headers that describe them.
    pub fn drain(&mut self, ring: &PcmRing) {
        let start = self.cursor.max(ring.tail());
        self.dropped += start - self.cursor;
        let head = ring.head();
        self.cursor = head;
        if start >= head {
            return;
        }
        self.samples.extend(ring.slices(start).iter().copied());
        let kept = ring.record_slices(ring.record_tail());
        let records: Vec<_> = kept.iter().copied().collect();
        for (i, record) in records.iter().enumerate() {
            let end = records.get(i + 1).map_or(head, |next| next.first);
            if end <= start || record.first >= head {
                continue;
            }
            let first = record.first.max(start);
            let clipped = pemu_core::hostio::PcmRecord {
                vt_start: record.time_of(first),
                fs: record.fs,
                channels: record.channels,
                first,
            };
            let continues = self.runs.last().is_some_and(|last| {
                last.fs == clipped.fs
                    && last.channels == clipped.channels
                    && last.time_of(first) == clipped.vt_start
            });
            if !continues {
                self.runs.push(clipped);
            }
        }
    }

    /// `None` when empty or mixing formats.
    #[must_use]
    pub fn format(&self) -> Option<(u32, u16)> {
        let first = self.runs.first()?;
        self.runs
            .iter()
            .all(|r| r.fs == first.fs && r.channels == first.channels)
            .then_some((first.fs, first.channels))
    }

    /// Runs after the first.
    #[must_use]
    pub fn discontinuities(&self) -> usize {
        self.runs.len().saturating_sub(1)
    }

    #[must_use]
    pub fn vt_start(&self) -> Option<VTime> {
        self.runs.first().map(|r| r.vt_start)
    }
}

/// Installed by the host with the audio root (`pemu_host::audio_root::install`); `pemu-api` opens
/// no file.
#[derive(Copy, Clone)]
pub struct AudioIo {
    /// Returns the relative, forward-slashed path it landed at.
    pub write: fn(&str, &[u8]) -> Result<String, String>,
}

fn io_slot() -> &'static Mutex<Option<AudioIo>> {
    static IO: OnceLock<Mutex<Option<AudioIo>>> = OnceLock::new();
    IO.get_or_init(|| Mutex::new(None))
}

pub fn set_io(io: AudioIo) {
    let mut guard: MutexGuard<'_, Option<AudioIo>> = match io_slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(io);
}

fn installed_io() -> Option<AudioIo> {
    let guard = match io_slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard
}

fn no_writer() -> ApiError {
    {
        ApiError::new(
            E_STATE,
            "this build cannot write an artifact, so `audio_capture` has nowhere to put the WAV",
        )
        .with_hint(
            "omit `wav` or pass false to analyze only; a host installs the writer with \
             `commands::audio_capture::set_io`",
        )
    }
}

/// Ten minutes keeps a typo from holding the instance for an hour of virtual time.
pub const DURATION_MS_MAX: u64 = 600_000;

pub const DEFAULT_DURATION_MS: u64 = 1_000;

pub const AUDIO_DIR: &str = "audio";

/// 48 kHz stereo; this board plays 16 and 24 kHz.
const WORST_SAMPLES_PER_S: u64 = 48_000 * 2;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Exact transmitted samples, volume as metadata. The default, and what goldens compare.
    Digital,
    /// DAC gain applied (class C).
    Analog,
}

impl Mode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Digital => "digital",
            Mode::Analog => "analog",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioCaptureArgs {
    pub instance: Option<String>,
    /// `None` with a `cursor` captures what is already in the ring without running.
    pub duration_ms: Option<u64>,
    /// Captures from this absolute sample cursor without running.
    pub cursor: Option<u64>,
    pub mode: Mode,
    /// 0, the left slot the DAC uses.
    pub channel: u16,
    /// Becomes the file stem.
    pub save_as: Option<String>,
    /// `None` means "when the host can", so an analysis-only build still captures rather than
    /// refusing every call.
    pub wav: Option<bool>,
}

impl Default for AudioCaptureArgs {
    fn default() -> AudioCaptureArgs {
        AudioCaptureArgs {
            instance: None,
            duration_ms: None,
            cursor: None,
            mode: Mode::Digital,
            channel: 0,
            save_as: None,
            wav: None,
        }
    }
}

impl AudioCaptureArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<AudioCaptureArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "duration_ms",
                "cursor",
                "mode",
                "channel",
                "save_as",
                "wav",
            ],
        )?;
        let duration_ms = opt_u64(args, "duration_ms")?;
        if let Some(ms) = duration_ms
            && (ms == 0 || ms > DURATION_MS_MAX)
        {
            return Err(usage(
                "duration_ms",
                &format!("expected 1 to {DURATION_MS_MAX} ms"),
            ));
        }
        let cursor = opt_u64(args, "cursor")?;
        if cursor.is_some() && duration_ms.is_some() {
            return Err(usage(
                "cursor",
                "a capture either runs for `duration_ms` or reads from `cursor`, not both",
            ));
        }
        let mode = match opt_str(args, "mode")? {
            None | Some("digital") => Mode::Digital,
            Some("analog") => Mode::Analog,
            Some(other) => {
                return Err(usage(
                    "mode",
                    &format!("`{other}` is not one of digital, analog"),
                ));
            }
        };
        let channel = u16::try_from(opt_u64(args, "channel")?.unwrap_or(0))
            .map_err(|_| usage("channel", "does not fit"))?;
        let save_as = opt_str(args, "save_as")?.map(str::to_owned);
        if let Some(label) = &save_as
            && !is_label(label)
        {
            return Err(usage(
                "save_as",
                "expected one segment of ^[a-z0-9][a-z0-9._-]*$",
            ));
        }
        Ok(AudioCaptureArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            duration_ms,
            cursor,
            mode,
            channel,
            save_as,
            wav: opt_bool(args, "wav")?,
        })
    }

    /// `audio/<label>.wav`, or `audio/<vt_us>.wav` named by the first captured frame.
    #[must_use]
    pub fn artifact_path(&self, vt_start: VTime) -> String {
        match &self.save_as {
            Some(label) => format!("{AUDIO_DIR}/{label}.wav"),
            None => format!("{AUDIO_DIR}/{:012}.wav", vt_start.as_us()),
        }
    }
}

/// One artifact-path segment, as the input schema declares. `a/b` would pass `check_artifact_path`
/// as two segments, so the schema's rule is checked here.
fn is_label(label: &str) -> bool {
    let mut bytes = label.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// So the ring cannot evict between drains at [`WORST_SAMPLES_PER_S`], with half the ring as
/// margin.
fn slice_us(ring: &PcmRing) -> u64 {
    (ring.capacity() as u64 * 1_000_000 / WORST_SAMPLES_PER_S / 2).max(1)
}

pub fn audio_capture_on(
    session: &mut Session,
    args: &AudioCaptureArgs,
) -> Result<Output, ApiError> {
    audio_capture_with_io(session, args, installed_io())
}

/// With the writer passed in, so a test does not depend on what another installed.
pub fn audio_capture_with_io(
    session: &mut Session,
    args: &AudioCaptureArgs,
    io: Option<AudioIo>,
) -> Result<Output, ApiError> {
    if args.mode == Mode::Analog {
        return Err(ApiError::new(
            E_STATE,
            "`analog` needs the ES8311 DAC gain, which `audio_capture` cannot read in this build",
        )
        .with_hint("use `digital` (the default); `analog` is not supported yet"));
    }
    let io = match (args.wav, io) {
        (Some(false), _) | (None, None) => None,
        (Some(true) | None, Some(io)) => Some(io),
        (Some(true), None) => return Err(no_writer()),
    };
    let start_vt = session.now();
    let mic_dropped_from = session.machine().io().audio_in.dropped();
    let mut capture = match args.cursor {
        Some(cursor) => Capture::starting_at(cursor),
        None => Capture::starting_at(session.machine().io().audio_out.head()),
    };
    let mut stopped: Option<StopReason> = None;
    if let Some(cursor) = args.cursor {
        let head = session.machine().io().audio_out.head();
        if cursor > head {
            return Err(usage(
                "cursor",
                &format!("{cursor} is past the playback ring's head, {head}"),
            ));
        }
        capture.drain(&session.machine().io().audio_out);
    } else {
        let ms = args.duration_ms.unwrap_or(DEFAULT_DURATION_MS);
        let end = VTime(start_vt.0.saturating_add(VTime::from_ms(ms).0));
        let slice = VTime::from_us(slice_us(&session.machine().io().audio_out));
        let mut now = start_vt;
        while now.0 < end.0 {
            let until = VTime(now.0.saturating_add(slice.0).min(end.0));
            let outcome = session.run_until(until);
            capture.drain(&session.machine().io().audio_out);
            now = session.now();
            if outcome.reason != StopReason::Until {
                stopped = Some(outcome.reason);
                break;
            }
        }
    }
    if let Some(reason) = &stopped
        && let Some(fault) = crate::session::fault_of(reason)
    {
        // A deadlock carries its envelope, as `run` reports it.
        return Err(crate::commands::inspect::deadlock_envelope(session, fault));
    }
    if let Some(fault) = crate::session::task_deadlock_fault(session) {
        return Err(crate::commands::inspect::deadlock_envelope(session, fault));
    }
    let mic_dropped = session.machine().io().audio_in.dropped() - mic_dropped_from;
    finish(session, args, &capture, mic_dropped, io)
}

fn finish(
    session: &mut Session,
    args: &AudioCaptureArgs,
    capture: &Capture,
    mic_dropped: u64,
    io: Option<AudioIo>,
) -> Result<Output, ApiError> {
    let receipt = session.receipt();
    let instance = session.id.to_string();
    let Some((fs, channels)) = capture.format() else {
        if capture.runs.is_empty() {
            let json = serde_json::json!({
                "instance": instance,
                "vt_us": receipt.vt_us,
                "mode": args.mode.as_str(),
                "cursor": capture.from,
                "next_cursor": capture.cursor,
                "frames": 0,
                "dropped_samples": capture.dropped,
                "mic_dropped_samples": mic_dropped,
                "analysis": serde_json::Value::Null,
                "wav": serde_json::Value::Null,
            });
            let text = format!(
                "{instance} audio_capture: the guest played nothing (next_cursor {})",
                capture.cursor
            );
            return Ok(Output::new(json, text, receipt));
        }
        return Err(ApiError::new(
            E_STATE,
            format!(
                "the capture spans {} sample formats, which one WAV cannot hold",
                capture.runs.len()
            ),
        )
        .with_hint("capture again after the guest's `set_format`, or pass a later `cursor`"));
    };
    if args.channel >= channels {
        return Err(usage(
            "channel",
            &format!("the capture has {channels} channel(s)"),
        ));
    }
    let mono = channel_of(&capture.samples, channels, args.channel);
    let report = analyze(&mono, fs);
    let frames = mono.len() as u64;
    let vt_start = capture.vt_start().unwrap_or_default();
    let mut artifact = None;
    if let Some(io) = io {
        let bytes = wav_bytes(fs, channels, &capture.samples);
        let path = args.artifact_path(vt_start);
        let written = (io.write)(&path, &bytes).map_err(|err| {
            ApiError::new(E_STATE, format!("the WAV could not be written: {err}"))
        })?;
        let sha256 = super::snapshot::sha256_hex(&bytes);
        artifact = Some(
            ArtifactRef::new(written, sha256, WAV_MEDIA_TYPE, bytes.len() as u64).map_err(
                |err| {
                    ApiError::new(
                        E_INTERNAL,
                        format!("the artifact path is not usable: {err}"),
                    )
                },
            )?,
        );
    }
    let report_json = report.to_json();
    let json = serde_json::json!({
        "instance": instance,
        "vt_us": receipt.vt_us,
        "mode": args.mode.as_str(),
        "cursor": capture.from,
        "next_cursor": capture.cursor,
        "vt_start_us": vt_start.as_us(),
        "fs": fs,
        "channels": channels,
        "frames": frames,
        "duration_ms": frames * 1_000 / u64::from(fs),
        "dropped_samples": capture.dropped,
        "mic_dropped_samples": mic_dropped,
        "discontinuities": capture.discontinuities(),
        "analysis": {
            "channel": args.channel,
            "fundamental_hz": report_json["fundamental_hz"],
            "peak": report_json["peak"],
            "rms": report_json["rms"],
        },
        "wav": artifact.as_ref().map_or(serde_json::Value::Null, ArtifactRef::to_json),
    });
    let fundamental = report
        .fundamental_hz
        .map_or_else(|| "none".to_owned(), |hz| format!("{hz:.2} Hz"));
    let mut text = format!(
        "{instance} audio_capture {frames} frames at {fs} Hz x{channels}: ch{} fundamental \
         {fundamental}, peak {}, rms {:.2}",
        args.channel, report.peak, report.rms
    );
    if capture.dropped > 0 || capture.discontinuities() > 0 {
        text.push_str(&format!(
            "; {} dropped sample(s), {} discontinuit(ies)",
            capture.dropped,
            capture.discontinuities()
        ));
    }
    if let Some(artifact) = &artifact {
        text.push_str(&format!(" -> {}", artifact.path));
    }
    let mut output = Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT);
    if let Some(artifact) = artifact {
        output = output.with_artifact(artifact).map_err(|err| {
            ApiError::new(
                E_INTERNAL,
                format!("the artifact path is not usable: {err}"),
            )
        })?;
    }
    Ok(output)
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`audio_capture` arguments.",
        "properties": {
            "instance": instance_schema(),
            "duration_ms": { "type": "integer", "minimum": 1, "maximum": DURATION_MS_MAX, "description": "Virtual ms to run and capture (1000)." },
            "cursor": { "type": "integer", "minimum": 0, "description": "Sample cursor to read from, without running." },
            "mode": { "type": "string", "enum": ["digital", "analog"], "description": "Playback mode (digital)." },
            "channel": { "type": "integer", "minimum": 0, "description": "Slot to analyze (0)." },
            "save_as": { "type": "string", "pattern": "^[a-z0-9][a-z0-9._-]*$", "description": "Artifact label." },
            "wav": { "type": "boolean", "description": "Write a WAV (true when the host can)." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "vt_us": { "type": "integer" },
            "mode": { "type": "string" },
            "cursor": { "type": "integer" },
            "next_cursor": { "type": "integer" },
            "vt_start_us": { "type": "integer" },
            "fs": { "type": "integer" },
            "channels": { "type": "integer" },
            "frames": { "type": "integer" },
            "duration_ms": { "type": "integer" },
            "dropped_samples": { "type": "integer" },
            "mic_dropped_samples": { "type": "integer" },
            "discontinuities": { "type": "integer" },
            "analysis": {
                "type": ["object", "null"],
                "required": ["channel", "fundamental_hz", "peak", "rms"],
                "properties": {
                    "channel": { "type": "integer" },
                    "fundamental_hz": { "type": ["number", "null"] },
                    "peak": { "type": "integer" },
                    "rms": { "type": "number" }
                }
            },
            "wav": {
                "type": ["object", "null"],
                "required": ["path", "sha256", "media_type", "bytes"],
                "properties": {
                    "path": { "type": "string" },
                    "sha256": { "type": "string" },
                    "media_type": { "type": "string" },
                    "bytes": { "type": "integer" }
                }
            }
        }
    })
}

/// Capture the guest's audio output as a WAV artifact with its fundamental and peak.
#[command(
    api_crate = crate,
    name = "audio_capture",
    group = audio,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(advances_time, needs_instance),
    scenario_step = "audio.capture",
    errors(E_USAGE, E_STATE, E_LEASE, E_DEADLOCK, E_INTERNAL),
    example(
        title = "Run 1 s and measure the playback without writing a file",
        args = r#"{"duration_ms":1000,"wav":false}"#,
    ),
    example(
        title = "Analyze what is already in the ring, without a file",
        args = r#"{"cursor":0,"wav":false}"#,
    ),
)]
pub fn audio_capture(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = AudioCaptureArgs::from_json(&args)?;
    // On the checked-out session, outside the pool lock.
    crate::pool::with_session(
        |pool: &mut Pool| {
            let id = pool.bind(SPEC_AUDIO_CAPTURE.annotations, args.instance.as_deref())?;
            let now = pool
                .session(id)
                .map(Session::now)
                .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
            if let Some(state) = pool.table().get(id) {
                state.lease.check_call(
                    crate::lease::LeaseHolder::Agent,
                    SPEC_AUDIO_CAPTURE.annotations,
                    now,
                )?;
            }
            Ok(id)
        },
        |session| audio_capture_on(session, &args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    use pemu_core::hostio::HostIo;
    use pemu_core::input::InputEvent;
    use pemu_core::time::frame_time;
    use pemu_machine::MachineApi;
    use pemu_machine::machine::{At, GuestMem, InputError, Receipt as LedgerReceipt};
    use pemu_machine::run::{RunLimits, RunOutcome};

    use crate::commands::mic_set::tone_sample;
    use crate::commands::start::{Boot, StartArgs};
    use crate::instance::{InstanceId, Lifecycle};

    static FILES: StdMutex<Option<BTreeMap<String, Vec<u8>>>> = StdMutex::new(None);

    fn test_write(path: &str, bytes: &[u8]) -> Result<String, String> {
        let mut guard = FILES.lock().expect("never poisoned");
        guard
            .get_or_insert_with(BTreeMap::new)
            .insert(path.to_owned(), bytes.to_vec());
        Ok(path.to_owned())
    }

    fn file(path: &str) -> Option<Vec<u8>> {
        FILES
            .lock()
            .expect("never poisoned")
            .as_ref()
            .and_then(|m| m.get(path).cloned())
    }

    fn tone(hz: u32, amplitude: i16, fs: u32, frames: u64) -> Vec<i16> {
        (0..frames)
            .map(|n| tone_sample(n, hz, amplitude, fs))
            .collect()
    }

    /// A 64-bit LCG, so no test depends on a host RNG.
    fn noise(len: usize) -> Vec<i16> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((state >> 48) as i16) / 2
            })
            .collect()
    }

    #[test]
    fn peak_reads_i16_min_as_32768_and_rms_of_a_square_wave_is_its_amplitude() {
        assert_eq!(peak(&[1, -5, 3]), 5);
        assert_eq!(peak(&[i16::MIN, 0]), 32_768);
        assert_eq!(peak(&[]), 0);
        assert_eq!(rms(&[1000, -1000, 1000, -1000]), 1000.0);
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn a_1000_hz_tone_reads_1000_hz_and_peak_6000() {
        let samples = tone(1_000, 6_000, 16_000, 160_000);
        let report = analyze(&samples, 16_000);
        let hz = report.fundamental_hz.expect("a tone has a fundamental");
        assert!((hz - 1_000.0).abs() < 0.01, "{hz}");
        assert_eq!(report.peak, 6_000);
        // A sine's RMS is its peak over sqrt(2).
        assert!(
            (report.rms - 6_000.0 / 2f64.sqrt()).abs() < 1.0,
            "{}",
            report.rms
        );
    }

    #[test]
    fn a_period_that_is_not_a_whole_number_of_samples_is_still_measured_to_a_hundredth_of_a_percent()
     {
        for hz in [440u32, 997, 3_001] {
            let samples = tone(hz, 12_000, 16_000, 32_000);
            let got = fundamental_hz(&samples, 16_000).expect("a tone");
            let err = (got - f64::from(hz)).abs() / f64::from(hz);
            assert!(err < 1e-4, "{hz} Hz read as {got}");
        }
    }

    #[test]
    fn a_harmonic_and_an_offset_do_not_move_the_fundamental() {
        let fs = 16_000;
        let samples: Vec<i16> = (0..16_000u64)
            .map(|n| {
                1_500 + tone_sample(n, 220, 8_000, fs) + tone_sample(n, 440, 6_000, fs)
                    - tone_sample(n, 660, 2_000, fs)
            })
            .collect();
        let got = fundamental_hz(&samples, fs).expect("a periodic signal");
        assert!((got - 220.0).abs() < 0.1, "{got}");
    }

    #[test]
    fn silence_noise_and_a_short_buffer_have_no_fundamental() {
        assert_eq!(fundamental_hz(&vec![0; 16_000], 16_000), None);
        assert_eq!(fundamental_hz(&vec![3; 16_000], 16_000), None);
        assert_eq!(fundamental_hz(&noise(16_000), 16_000), None);
        assert_eq!(
            fundamental_hz(&tone(1_000, 6_000, 16_000, 100), 16_000),
            None
        );
        assert_eq!(fundamental_hz(&tone(1_000, 6_000, 16_000, 16_000), 0), None);
    }

    fn within_half_percent(got: Option<f64>, want: f64, what: &str) {
        let hz = got.unwrap_or_else(|| panic!("{what}: no fundamental, want {want} Hz"));
        assert!(
            (hz - want).abs() <= want * 0.005,
            "{what}: read {hz} Hz, want {want} Hz"
        );
    }

    /// At the three rates the host path carries. The one unresolvable tone is exactly `fs / 2`:
    /// sampled from phase 0 every sample is 0, so the answer is `None`.
    #[test]
    fn every_tone_from_100_hz_to_nyquist_reads_within_half_a_percent() {
        for fs in [16_000u32, 24_000, 48_000] {
            let frames = u64::from(fs) / 4;
            let mut hz = 100;
            while hz <= fs / 2 {
                let samples = tone(hz, 6_000, fs, frames);
                let got = fundamental_hz(&samples, fs);
                if hz == fs / 2 {
                    assert_eq!(got, None, "{hz} Hz at {fs} Hz samples to silence");
                } else {
                    within_half_percent(got, f64::from(hz), &format!("{hz} Hz at {fs} Hz"));
                }
                hz += 50;
            }
        }
    }

    #[test]
    fn a_noisy_tone_reads_correctly_or_not_at_all() {
        let fs = 16_000;
        let clean = tone(1_000, 6_000, fs, 16_000);
        // Uniform noise in [-b, b] has power b^2/3 and the tone 6000^2/2. The ratios are written
        // out because `powf` is not core-safe.
        for (snr_db, ratio) in [(20, 100.0), (9, 7.943), (3, 1.995), (0, 1.0)] {
            let b = (3.0 * 6_000.0f64 * 6_000.0 / 2.0 / ratio).sqrt();
            let noisy: Vec<i16> = clean
                .iter()
                .zip(noise(clean.len()))
                .map(|(t, n)| {
                    let scaled = f64::from(n) / 16_384.0 * b;
                    (f64::from(*t) + scaled).round().clamp(-32_768.0, 32_767.0) as i16
                })
                .collect();
            if let Some(hz) = fundamental_hz(&noisy, fs) {
                assert!(
                    (hz - 1_000.0).abs() <= 5.0,
                    "{snr_db} dB SNR read {hz} Hz, which is neither 1000 Hz nor no answer"
                );
            }
            if snr_db >= 9 {
                within_half_percent(fundamental_hz(&noisy, fs), 1_000.0, &format!("{snr_db} dB"));
            }
        }
    }

    #[test]
    fn leading_silence_does_not_hide_the_tone() {
        let fs = 16_000;
        let mut samples = vec![0i16; 3_200];
        samples.extend(tone(1_000, 6_000, fs, 12_800));
        within_half_percent(
            fundamental_hz(&samples, fs),
            1_000.0,
            "200 ms of zeros first",
        );
    }

    /// Both kinds: a masked tone (phase runs on through the gap) and a paused one (the tone resumes
    /// where it stopped, as a starved I2S TX pads with zeros).
    #[test]
    fn gaps_of_zeros_do_not_pull_the_fundamental_down() {
        let fs = 16_000;
        let mut masked = tone(1_000, 6_000, fs, 16_000);
        masked[8_000..9_600].fill(0);
        within_half_percent(
            fundamental_hz(&masked, fs),
            1_000.0,
            "one 100 ms masked gap",
        );

        let mut paused = Vec::new();
        let mut n = 0u64;
        while paused.len() < 160_000 {
            for _ in 0..8_000 {
                paused.push(tone_sample(n, 1_000, 6_000, fs));
                n += 1;
            }
            paused.extend(core::iter::repeat_n(0, 240));
        }
        within_half_percent(
            fundamental_hz(&paused, fs),
            1_000.0,
            "a 15 ms pause every 500 ms",
        );

        let mut single = tone(1_000, 6_000, fs, 8_000);
        single.extend(core::iter::repeat_n(0, 1_600));
        single.extend((8_000..16_000u64).map(|n| tone_sample(n, 1_000, 6_000, fs)));
        within_half_percent(fundamental_hz(&single, fs), 1_000.0, "one 100 ms pause");
    }

    #[test]
    fn channel_of_takes_one_slot_per_whole_frame() {
        assert_eq!(channel_of(&[1, 2, 3, 4, 5], 2, 0), vec![1, 3]);
        assert_eq!(channel_of(&[1, 2, 3, 4, 5], 2, 1), vec![2, 4]);
        assert_eq!(channel_of(&[1, 2], 2, 2), Vec::<i16>::new());
    }

    #[test]
    fn wav_bytes_is_the_canonical_44_byte_pcm_header_then_the_samples() {
        let bytes = wav_bytes(16_000, 2, &[1, -1, 0x1234, -2, 7]);
        assert_eq!(
            bytes.len(),
            WAV_HEADER_BYTES + 8,
            "the trailing half frame is dropped"
        );
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 8);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            16_000
        );
        assert_eq!(
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            64_000
        );
        assert_eq!(u16::from_le_bytes(bytes[32..34].try_into().unwrap()), 4);
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        assert_eq!(&bytes[44..], &[1, 0, 0xFF, 0xFF, 0x34, 0x12, 0xFE, 0xFF]);
    }

    #[test]
    fn a_capture_drained_in_pieces_keeps_one_run_and_counts_what_the_ring_evicted() {
        let mut ring = PcmRing::new(64, 8);
        let mut capture = Capture::starting_at(0);
        ring.write(VTime(0), 16_000, 2, &[1; 16]);
        capture.drain(&ring);
        ring.write(frame_time(VTime(0), 8, 16_000), 16_000, 2, &[2; 16]);
        capture.drain(&ring);
        assert_eq!(capture.samples.len(), 32);
        assert_eq!(capture.runs.len(), 1);
        assert_eq!(capture.format(), Some((16_000, 2)));
        assert_eq!(capture.discontinuities(), 0);
        // 96 more samples overflow the 64-sample ring before the next drain: 32 are lost.
        ring.write(frame_time(VTime(0), 16, 16_000), 16_000, 2, &[3; 96]);
        capture.drain(&ring);
        assert_eq!(capture.dropped, 32);
        assert_eq!(capture.samples.len(), 32 + 64);
        assert_eq!(capture.cursor, ring.head());
    }

    #[test]
    fn a_gap_is_a_discontinuity_and_a_format_change_has_no_single_format() {
        let mut ring = PcmRing::new(1 << 12, 16);
        ring.write(VTime(0), 16_000, 2, &[1; 32]);
        ring.write(VTime::from_ms(1_000), 16_000, 2, &[1; 32]);
        let mut gap = Capture::starting_at(0);
        gap.drain(&ring);
        assert_eq!(gap.discontinuities(), 1);
        assert_eq!(gap.format(), Some((16_000, 2)));
        let mut late = Capture::starting_at(8);
        late.drain(&ring);
        assert_eq!(late.runs[0].first, 8);
        assert_eq!(late.runs[0].vt_start, frame_time(VTime(0), 4, 16_000));

        ring.write(VTime::from_ms(2_000), 24_000, 1, &[1; 32]);
        let mut mixed = Capture::starting_at(0);
        mixed.drain(&ring);
        assert_eq!(mixed.format(), None);
    }

    /// Plays a tone into `HostIo::audio_out` in 240-frame buffers the way `wiring::i2s` does: left
    /// slot the tone, right slot 0.
    struct ToneMachine {
        vt: VTime,
        io: HostIo,
        fs: u32,
        hz: u32,
        amplitude: i16,
        played: u64,
    }

    crate::commands::start::tests::refuse_snapshots!(ToneMachine);

    impl MachineApi for ToneMachine {
        fn run(&mut self, lim: RunLimits) -> RunOutcome {
            let until = lim.until.unwrap_or(self.vt);
            let due = (u128::from(until.0) * u128::from(self.fs) / 1_000_000_000_000) as u64;
            while self.played + 240 <= due {
                let mut buffer = Vec::with_capacity(480);
                for n in self.played..self.played + 240 {
                    buffer.push(tone_sample(n, self.hz, self.amplitude, self.fs));
                    buffer.push(0);
                }
                let at = frame_time(VTime(0), self.played, self.fs);
                self.io.audio_out.write(at, self.fs, 2, &buffer);
                self.played += 240;
            }
            self.vt = VTime(self.vt.0.max(until.0));
            RunOutcome {
                reason: StopReason::Until,
                vt: self.vt,
                insns: 0,
                ff_insns: 0,
                idle_ps: 0,
            }
        }

        fn input(&mut self, _at: At, _ev: InputEvent) -> Result<u64, InputError> {
            Ok(0)
        }

        fn io(&mut self) -> &mut HostIo {
            &mut self.io
        }

        fn now(&self) -> VTime {
            self.vt
        }

        fn guest_mem(&mut self) -> GuestMem<'_> {
            unreachable!("`audio_capture` reads no guest memory")
        }

        fn is_tainted(&self) -> bool {
            false
        }
        fn receipt(&mut self) -> LedgerReceipt {
            LedgerReceipt::default()
        }
    }

    fn tone_pool() -> (Pool, InstanceId) {
        let machine = ToneMachine {
            vt: VTime(0),
            // 10 s of stereo 16 kHz is 39 rings full, so the capture only succeeds if it drains in
            // slices.
            io: HostIo::new(8192),
            fs: 16_000,
            hz: 1_000,
            amplitude: 6_000,
            played: 0,
        };
        let mut pool = Pool::new();
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&args, Box::new(machine));
        pool.table_mut()
            .get_mut(id)
            .expect("the instance was just created")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        (pool, id)
    }

    fn capture_args(json: serde_json::Value) -> AudioCaptureArgs {
        AudioCaptureArgs::from_json(&json).expect("valid arguments")
    }

    #[test]
    fn a_ten_second_capture_is_lossless_measured_and_written_as_a_wav_artifact() {
        let (mut pool, id) = tone_pool();
        let session = pool.session_mut(id).expect("the instance");
        let args = capture_args(serde_json::json!({"duration_ms": 10_000, "save_as": "demo"}));
        let out = audio_capture_with_io(session, &args, Some(AudioIo { write: test_write }))
            .expect("a capture");
        assert_eq!(out.json["fs"], 16_000);
        assert_eq!(out.json["channels"], 2);
        // 10 s is 666 whole 240-frame buffers; the 667th ends after the capture does.
        assert_eq!(out.json["frames"], 666 * 240);
        assert_eq!(out.json["dropped_samples"], 0);
        assert_eq!(out.json["discontinuities"], 0);
        let hz = out.json["analysis"]["fundamental_hz"]
            .as_f64()
            .expect("a fundamental");
        assert!((hz - 1_000.0).abs() <= 5.0, "1000 Hz within 0.5 %: {hz}");
        assert_eq!(out.json["analysis"]["peak"], 6_000);

        assert_eq!(out.artifacts.len(), 1);
        let artifact = &out.artifacts[0];
        assert_eq!(artifact.path, "audio/demo.wav");
        assert_eq!(artifact.media_type, "audio/wav");
        let bytes = file("audio/demo.wav").expect("the host received the file");
        assert_eq!(artifact.sha256, super::super::snapshot::sha256_hex(&bytes));
        assert_eq!(artifact.bytes, bytes.len() as u64);
        assert_eq!(bytes.len(), WAV_HEADER_BYTES + 666 * 240 * 4);
        assert!(out.absolute_paths().is_empty());

        // Nothing new without running.
        let again =
            capture_args(serde_json::json!({"cursor": out.json["next_cursor"], "wav": false}));
        let empty = audio_capture_on(session, &again).expect("an empty capture");
        assert_eq!(empty.json["frames"], 0);
        assert!(empty.artifacts.is_empty());
    }

    #[test]
    fn analog_mode_both_capture_forms_and_a_bad_label_are_refused() {
        let (mut pool, id) = tone_pool();
        let session = pool.session_mut(id).expect("the instance");
        let analog = capture_args(serde_json::json!({"mode": "analog", "wav": false}));
        let err = audio_capture_on(session, &analog).expect_err("analog needs the codec");
        assert_eq!(err.code, E_STATE);
        assert!(err.hint.as_deref().unwrap_or_default().contains("digital"));

        let both = AudioCaptureArgs::from_json(&serde_json::json!({"cursor": 0, "duration_ms": 5}));
        assert_eq!(both.expect_err("exclusive").code, E_USAGE);
        for label in ["../x", "a/b", "Demo", "-x", ".x", ""] {
            let refused = AudioCaptureArgs::from_json(&serde_json::json!({"save_as": label}));
            assert_eq!(
                refused.expect_err("outside ^[a-z0-9][a-z0-9._-]*$").code,
                E_USAGE,
                "{label}"
            );
        }
        AudioCaptureArgs::from_json(&serde_json::json!({"save_as": "demo-1.take_2"}))
            .expect("inside the pattern");
        let long = AudioCaptureArgs::from_json(&serde_json::json!({"duration_ms": 600_001}));
        assert_eq!(long.expect_err("bounded").code, E_USAGE);
    }

    #[test]
    fn a_cursor_past_the_head_is_a_usage_error() {
        let (mut pool, id) = tone_pool();
        let session = pool.session_mut(id).expect("the instance");
        session.run_until(VTime::from_ms(30));
        let head = session.machine().io().audio_out.head();
        let past = capture_args(serde_json::json!({"cursor": head + 1, "wav": false}));
        let err = audio_capture_on(session, &past).expect_err("never written");
        assert_eq!(err.code, E_USAGE);
        let at_head = capture_args(serde_json::json!({"cursor": head, "wav": false}));
        audio_capture_on(session, &at_head).expect("the head itself is an empty capture");
    }

    #[test]
    fn the_output_schema_types_the_analysis_and_the_artifact() {
        let schema = output_schema();
        let props = &schema.as_value()["properties"];
        let analysis = &props["analysis"]["properties"];
        assert_eq!(analysis["channel"]["type"], "integer");
        assert_eq!(
            analysis["fundamental_hz"]["type"],
            serde_json::json!(["number", "null"])
        );
        assert_eq!(analysis["peak"]["type"], "integer");
        assert_eq!(analysis["rms"]["type"], "number");
        let wav = &props["wav"]["properties"];
        for (key, kind) in [
            ("path", "string"),
            ("sha256", "string"),
            ("media_type", "string"),
            ("bytes", "integer"),
        ] {
            assert_eq!(wav[key]["type"], kind, "{key}");
        }

        let (mut pool, id) = tone_pool();
        let session = pool.session_mut(id).expect("the instance");
        let args = capture_args(serde_json::json!({"duration_ms": 200}));
        let out = audio_capture_with_io(session, &args, Some(AudioIo { write: test_write }))
            .expect("a capture");
        for key in out.json["analysis"]
            .as_object()
            .expect("an analysis")
            .keys()
        {
            assert!(
                analysis.get(key).is_some(),
                "undeclared analysis field {key}"
            );
        }
        for key in out.json["wav"].as_object().expect("an artifact").keys() {
            assert!(wav.get(key).is_some(), "undeclared wav field {key}");
        }
    }

    #[test]
    fn without_a_writer_wav_defaults_to_false() {
        let (mut pool, id) = tone_pool();
        let session = pool.session_mut(id).expect("the instance");
        let plain = capture_args(serde_json::json!({"duration_ms": 200}));
        let out = audio_capture_with_io(session, &plain, None).expect("analysis only");
        assert!(out.artifacts.is_empty());
        assert_eq!(out.json["wav"], serde_json::Value::Null);
        let explicit = capture_args(serde_json::json!({"duration_ms": 200, "wav": true}));
        let err = audio_capture_with_io(session, &explicit, None).expect_err("no writer");
        assert!(
            err.hint
                .as_deref()
                .unwrap_or_default()
                .contains("audio_capture::set_io")
        );
    }

    #[test]
    fn the_command_is_registered_in_the_audio_caps_group() {
        let spec = crate::registry::find("audio_capture").expect("#[command] registered it");
        assert_eq!(spec.group, crate::spec::CapsGroup::Audio);
        assert!(spec.annotations.advances_time);
    }
}
