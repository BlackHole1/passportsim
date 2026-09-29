//! Audio verification on the host side of the core-crate line: the audio work that needs the
//! platform libm or a file system.
//!
//! | Piece | Why it is not in `pemu-api` |
//! |---|---|
//! | [`read_wav`], [`read_mic_file`] | `std::fs`; a host installs [`read_mic_file`] behind `pemu_api::commands::mic_set::MicIo` |
//! | [`parse_wav`] | only needed next to the file reads; the core writes WAVs but never reads one |
//! | [`dbfs`], [`amplitude_at_dbfs`] | `log10` and `powf` |
//! | [`reference_tone`] | `sin`: the float reference the integer CORDIC tone of `mic_set` is checked against |
//! | [`correlate`] | runs on captured artifacts after a run, never on a state path |
//!
//! The tone analysis and WAV encoding stay in `pemu_api::commands::audio_capture`, because a
//! browser build must compute the same numbers.

use std::fmt;
use std::path::Path;

/// Full scale of a 16-bit sample for dBFS, so a full-scale sine peaks at 0 dBFS and a -6 dBFS
/// tone at 16422.
pub const FULL_SCALE: f64 = 32_767.0;

/// A decoded 16-bit PCM WAV file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wav {
    pub fs: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

impl Wav {
    #[must_use]
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }

    /// Slot `channel` of every frame.
    #[must_use]
    pub fn channel(&self, channel: u16) -> Vec<i16> {
        let stride = usize::from(self.channels.max(1));
        self.samples
            .chunks_exact(stride)
            .filter_map(|frame| frame.get(usize::from(channel)).copied())
            .collect()
    }
}

/// Why a byte string is not a WAV this module reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WavError {
    NotRiffWave,
    Truncated,
    /// No `fmt ` chunk before `data`, or no `data` chunk.
    MissingChunk(&'static str),
    /// A format other than 16-bit integer PCM (tag 1, or 0xFFFE extensible).
    Unsupported {
        tag: u16,
        bits: u16,
    },
    Io(String),
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavError::NotRiffWave => f.write_str("not a RIFF/WAVE file"),
            WavError::Truncated => f.write_str("a chunk runs past the end of the file"),
            WavError::MissingChunk(name) => write!(f, "no `{name}` chunk"),
            WavError::Unsupported { tag, bits } => write!(
                f,
                "format tag {tag} with {bits} bits; only 16-bit integer PCM is read"
            ),
            WavError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for WavError {}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, WavError> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(WavError::Truncated)
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, WavError> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(WavError::Truncated)
}

/// Parses a RIFF/WAVE byte string: walks the chunks (skipping `LIST` and others, with the pad byte
/// after odd-sized chunks), reads `fmt ` and decodes `data` as little-endian 16-bit samples.
///
/// # Errors
///
/// Returns why the bytes are not a 16-bit PCM WAV.
pub fn parse_wav(bytes: &[u8]) -> Result<Wav, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotRiffWave);
    }
    let mut at = 12;
    let mut format: Option<(u16, u32)> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(bytes, at + 4)? as usize;
        let body = at + 8;
        let end = body.checked_add(size).ok_or(WavError::Truncated)?;
        if id == b"fmt " {
            if size < 16 || end > bytes.len() {
                return Err(WavError::Truncated);
            }
            let tag = u16_at(bytes, body)?;
            let channels = u16_at(bytes, body + 2)?;
            let fs = u32_at(bytes, body + 4)?;
            let bits = u16_at(bytes, body + 14)?;
            if !(tag == 1 || tag == 0xFFFE) || bits != 16 || channels == 0 {
                return Err(WavError::Unsupported { tag, bits });
            }
            format = Some((channels, fs));
        } else if id == b"data" {
            let (channels, fs) = format.ok_or(WavError::MissingChunk("fmt "))?;
            // A writer that never patched the size leaves it too large; take what is there.
            let data = &bytes[body..end.min(bytes.len())];
            let samples = data
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            return Ok(Wav {
                fs,
                channels,
                samples,
            });
        }
        at = end + size % 2;
    }
    Err(WavError::MissingChunk("data"))
}

/// Reads and parses a WAV file.
///
/// # Errors
///
/// Returns the I/O failure or why the file is not a 16-bit PCM WAV.
pub fn read_wav(path: &Path) -> Result<Wav, WavError> {
    let bytes =
        std::fs::read(path).map_err(|err| WavError::Io(format!("{}: {err}", path.display())))?;
    parse_wav(&bytes)
}

/// Why [`read_mic_file`] did not return audio. Its `Display` never names a host path: the
/// command layer reports it to an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicFileError {
    /// The name could leave the audio root: absolute, a drive, a backslash, an empty, `.` or `..`
    /// segment, or a symbolic link that resolves outside the root.
    Refused,
    /// Missing, not a regular file, or not readable. One variant, so a caller cannot probe the
    /// host's file system through the difference.
    Unreadable,
    /// Read, but not a 16-bit PCM WAV.
    Invalid(WavError),
}

impl fmt::Display for MicFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MicFileError::Refused => f.write_str("the name is outside the audio root"),
            MicFileError::Unreadable => f.write_str("not a readable audio file"),
            MicFileError::Invalid(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for MicFileError {}

/// The confinement policy of the `file` microphone source: `name` resolved under `root`.
///
/// 1. `name` is relative and forward-slashed, with no empty, `.` or `..` segment and no `:` or
///    backslash (as `pemu_api::commands::mic_set::check_file_name`);
/// 2. the canonicalized path, symbolic links resolved, is still under the canonical root;
/// 3. it is a regular file.
///
/// # Errors
///
/// [`MicFileError::Refused`] for rules 1 and 2, [`MicFileError::Unreadable`] for a missing root or
/// file and for rule 3.
pub fn resolve_mic_file(root: &Path, name: &str) -> Result<std::path::PathBuf, MicFileError> {
    let bad_segment = name
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    if name.is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name.contains(':')
        || bad_segment
    {
        return Err(MicFileError::Refused);
    }
    let root = std::fs::canonicalize(root).map_err(|_| MicFileError::Unreadable)?;
    let path = std::fs::canonicalize(root.join(name)).map_err(|_| MicFileError::Unreadable)?;
    if !path.starts_with(&root) {
        return Err(MicFileError::Refused);
    }
    if !path.is_file() {
        return Err(MicFileError::Unreadable);
    }
    Ok(path)
}

/// The `file` microphone source's reader: `name` confined to `root` by [`resolve_mic_file`], then
/// decoded. A host adapts it into `pemu_api::commands::mic_set::MicIo`, mapping `Refused` and
/// `Unreadable` to that side's `Unreadable` and `Invalid` to its `Invalid`.
///
/// # Errors
///
/// Returns why the file was refused, could not be read, or is not a 16-bit PCM WAV.
pub fn read_mic_file(root: &Path, name: &str) -> Result<Wav, MicFileError> {
    let path = resolve_mic_file(root, name)?;
    let bytes = std::fs::read(path).map_err(|_| MicFileError::Unreadable)?;
    parse_wav(&bytes).map_err(MicFileError::Invalid)
}

/// A peak magnitude in dBFS against [`FULL_SCALE`]; 0 gives negative infinity.
#[must_use]
pub fn dbfs(peak: f64) -> f64 {
    20.0 * (peak / FULL_SCALE).log10()
}

/// The peak sample value of a level in dBFS, rounded to the nearest LSB and clamped to `i16`.
#[must_use]
pub fn amplitude_at_dbfs(db: f64) -> i16 {
    (FULL_SCALE * 10f64.powf(db / 20.0))
        .round()
        .clamp(0.0, FULL_SCALE) as i16
}

/// A floating-point sine of peak `amplitude`, starting at phase 0, for checking integer tones.
#[must_use]
pub fn reference_tone(hz: f64, amplitude: f64, fs: u32, frames: usize) -> Vec<f64> {
    (0..frames)
        .map(|n| amplitude * (std::f64::consts::TAU * hz * n as f64 / f64::from(fs)).sin())
        .collect()
}

/// How closely a captured signal follows an injected one.
#[derive(Clone, Debug, PartialEq)]
pub struct Correlation {
    /// Pearson coefficient of the mean-removed overlap at the best lag, in `[-1, 1]`; a gain does
    /// not change it.
    pub coefficient: f64,
    /// Samples the output lags the input by.
    pub lag: usize,
    /// Least-squares gain from input to output at that lag.
    pub gain: f64,
    /// Samples the coefficient was taken over.
    pub overlap: usize,
}

/// The best normalized cross-correlation of `output` against `input` for output delays
/// `0..=max_lag`, over their overlap. `None` when no lag leaves an overlap of at least 16 samples
/// or either side is constant over it.
#[must_use]
pub fn correlate(input: &[i16], output: &[i16], max_lag: usize) -> Option<Correlation> {
    let mut best: Option<Correlation> = None;
    for lag in 0..=max_lag {
        if lag >= output.len() {
            break;
        }
        let n = input.len().min(output.len() - lag);
        if n < 16 {
            continue;
        }
        let x = &input[..n];
        let y = &output[lag..lag + n];
        let mean = |s: &[i16]| s.iter().map(|v| f64::from(*v)).sum::<f64>() / n as f64;
        let (mx, my) = (mean(x), mean(y));
        let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
        for (a, b) in x.iter().zip(y) {
            let (a, b) = (f64::from(*a) - mx, f64::from(*b) - my);
            sxy += a * b;
            sxx += a * a;
            syy += b * b;
        }
        if sxx <= 0.0 || syy <= 0.0 {
            continue;
        }
        let coefficient = sxy / (sxx * syy).sqrt();
        if best.as_ref().is_none_or(|b| coefficient > b.coefficient) {
            best = Some(Correlation {
                coefficient,
                lag,
                gain: sxy / sxx,
                overlap: n,
            });
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WAV with an odd-sized `LIST` chunk (and its pad byte) between `fmt ` and `data`, the way
    /// common tools write one.
    fn wav_with_list(channels: u16, fs: u32, samples: &[i16]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"WAVEfmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&fs.to_le_bytes());
        body.extend_from_slice(&(fs * 2 * u32::from(channels)).to_le_bytes());
        body.extend_from_slice(&(2 * channels).to_le_bytes());
        body.extend_from_slice(&16u16.to_le_bytes());
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(b"abc\0");
        body.extend_from_slice(b"data");
        body.extend_from_slice(&((samples.len() * 2) as u32).to_le_bytes());
        for s in samples {
            body.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn a_wav_with_an_extra_padded_chunk_parses_to_its_samples() {
        let wav = parse_wav(&wav_with_list(2, 16_000, &[1, -2, 300, -400])).expect("a WAV");
        assert_eq!((wav.fs, wav.channels), (16_000, 2));
        assert_eq!(wav.samples, vec![1, -2, 300, -400]);
        assert_eq!(wav.frames(), 2);
        assert_eq!(wav.channel(1), vec![-2, -400]);
    }

    #[test]
    fn non_wav_truncated_and_non_16_bit_files_are_refused() {
        assert_eq!(parse_wav(b"RIFX\0\0\0\0WAVE"), Err(WavError::NotRiffWave));
        let mut eight_bit = wav_with_list(1, 8_000, &[0]);
        eight_bit[34] = 8;
        assert_eq!(
            parse_wav(&eight_bit),
            Err(WavError::Unsupported { tag: 1, bits: 8 })
        );
        let whole = wav_with_list(1, 8_000, &[0, 1]);
        assert_eq!(parse_wav(&whole[..30]), Err(WavError::Truncated));
        let no_data = &whole[..whole.len() - 12];
        assert_eq!(parse_wav(no_data), Err(WavError::MissingChunk("data")));
    }

    #[test]
    fn minus_6_dbfs_is_16422_and_back() {
        assert_eq!(amplitude_at_dbfs(-6.0), 16_422);
        assert!((dbfs(16_422.0) + 6.0).abs() < 1e-3);
        assert_eq!(amplitude_at_dbfs(0.0), i16::MAX);
        assert_eq!(dbfs(FULL_SCALE), 0.0);
    }

    #[test]
    fn a_delayed_scaled_copy_correlates_fully_and_reports_its_lag_and_gain() {
        let input: Vec<i16> = reference_tone(440.0, 10_000.0, 16_000, 4_000)
            .iter()
            .zip(reference_tone(1_310.0, 3_000.0, 16_000, 4_000))
            .map(|(a, b)| (a + b).round() as i16)
            .collect();
        let mut output = vec![0i16; 37];
        output.extend(input.iter().map(|s| s / 2));
        let c = correlate(&input, &output, 64).expect("an overlap");
        assert_eq!(c.lag, 37);
        assert!(c.coefficient > 0.9999, "{}", c.coefficient);
        assert!((c.gain - 0.5).abs() < 1e-3, "{}", c.gain);

        let silence = vec![0i16; 4_000];
        assert_eq!(correlate(&input, &silence, 8), None);
    }

    #[test]
    fn a_mic_file_is_read_only_from_under_its_root_and_errors_name_no_path() {
        let base = std::env::temp_dir().join(format!("pemu-verify-mic-{}", std::process::id()));
        let root = base.join("root");
        std::fs::create_dir_all(root.join("clips")).expect("temp dir");
        std::fs::write(root.join("clips/a.wav"), wav_with_list(1, 16_000, &[5, -5]))
            .expect("write");
        std::fs::write(root.join("junk.wav"), b"not audio").expect("write");
        std::fs::write(base.join("outside.wav"), wav_with_list(1, 16_000, &[1])).expect("write");

        let wav = read_mic_file(&root, "clips/a.wav").expect("under the root");
        assert_eq!(wav.samples, vec![5, -5]);
        for name in [
            "../outside.wav",
            "/etc/hosts",
            "clips/../../outside.wav",
            "c:a.wav",
            "a\\b.wav",
            "",
            "./clips/a.wav",
        ] {
            assert_eq!(
                read_mic_file(&root, name),
                Err(MicFileError::Refused),
                "{name}"
            );
        }
        assert_eq!(
            read_mic_file(&root, "missing.wav"),
            Err(MicFileError::Unreadable)
        );
        assert_eq!(read_mic_file(&root, "clips"), Err(MicFileError::Unreadable));
        assert_eq!(
            read_mic_file(&root, "junk.wav"),
            Err(MicFileError::Invalid(WavError::NotRiffWave))
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.join("outside.wav"), root.join("escape.wav"))
                .expect("symlink");
            assert_eq!(
                read_mic_file(&root, "escape.wav"),
                Err(MicFileError::Refused)
            );
        }
        let base_text = base.display().to_string();
        for err in [MicFileError::Refused, MicFileError::Unreadable] {
            assert!(!err.to_string().contains(&base_text));
            assert!(!err.to_string().contains('/'), "{err}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
