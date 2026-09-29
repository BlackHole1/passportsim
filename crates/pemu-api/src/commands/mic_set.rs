//! `passportsim mic_set`: the microphone sources (`silence`, `tone`, `file`, and the browser-only
//! `live`).
//!
//! `env` owns the source: this command hands the same `mic` object to `env`, so it journals exactly
//! what the equivalent `env` call does. What it adds is samples, as `InputEvent::MicChunk`, which
//! `wiring::i2s` drains from `HostIo::audio_in` into every I2S0 RX descriptor:
//!
//! - `file`: the core cannot read it and a replay cannot regenerate it, so the samples are
//!   journaled; the host reads it through [`MicIo`];
//! - `tone`: rendered from integers only ([`tone_sample`], CORDIC, no libm in state paths), and
//!   journaled as chunks when `duration_ms` is given;
//! - `silence` is zero samples, which the RX path reads with nothing injected.
//!
//! Chunks are [`CHUNK_FRAMES`] frames stamped at their first frame, so the 8192-sample `audio_in`
//! ring never has to hold a whole file.

use std::sync::{Mutex, MutexGuard, OnceLock};

use pemu_core::input::{InputEvent, MicSource};
use pemu_core::time::{VTime, frame_time};
use pemu_machine::machine::At;

use crate::error::{ApiError, E_LEASE, E_STATE, E_USAGE};
use crate::output::Output;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::env::EnvArgs;
use crate::args::{instance_schema, object, only, opt_str, opt_u64, usage};
use crate::instance::InstanceId;
use crate::pool::Pool;
use crate::session::Session;

/// `atan(2^-i)` in units of `2^-32` turn, for `i` in `0..31`. Mathematical constants, not device
/// data.
const ATAN_TURNS: [i64; 31] = [
    536_870_912,
    316_933_406,
    167_458_907,
    85_004_756,
    42_667_331,
    21_354_465,
    10_679_838,
    5_340_245,
    2_670_163,
    1_335_087,
    667_544,
    333_772,
    166_886,
    83_443,
    41_722,
    20_861,
    10_430,
    5_215,
    2_608,
    1_304,
    652,
    326,
    163,
    81,
    41,
    20,
    10,
    5,
    3,
    1,
    1,
];

/// `prod 1/sqrt(1 + 2^-2i)` over the 31 rotations, in Q30, so the rotation ends on a unit vector.
const CORDIC_GAIN_Q30: i64 = 652_032_874;

const Q30_ONE: i64 = 1 << 30;

/// `sin(2 pi phase / 2^32)` in Q30, from integer shifts and additions only.
///
/// The phase is folded into `[-1/4, 1/4]` turn, inside the range CORDIC converges on, then rotated
/// 31 times. The error is a few Q30 LSB, far below one sample LSB, and the same on every host.
#[must_use]
pub fn sine_q30(phase: u32) -> i64 {
    const QUARTER: i64 = 1 << 30;
    const HALF: i64 = 1 << 31;
    // Reads the turn as signed, `[-1/2, 1/2)`.
    let mut z = i64::from(phase as i32);
    if z > QUARTER {
        z = HALF - z;
    } else if z < -QUARTER {
        z = -HALF - z;
    }
    let (mut x, mut y) = (CORDIC_GAIN_Q30, 0i64);
    for (i, angle) in ATAN_TURNS.iter().enumerate() {
        let (dx, dy) = (y >> i, x >> i);
        if z >= 0 {
            x -= dx;
            y += dy;
            z -= angle;
        } else {
            x += dx;
            y -= dy;
            z += angle;
        }
    }
    y
}

/// Sample `frame` of a `hz` tone of peak `amplitude` at `fs` hertz, from phase 0. The phase is the
/// exact rational `(frame * hz mod fs) / fs` turn, so the tone never drifts. `fs` 0 gives 0.
#[must_use]
pub fn tone_sample(frame: u64, hz: u32, amplitude: i16, fs: u32) -> i16 {
    if fs == 0 {
        return 0;
    }
    let fs64 = u64::from(fs);
    let numerator = (frame % fs64) * u64::from(hz) % fs64;
    let phase = ((numerator << 32) / fs64) as u32;
    let scaled = (sine_q30(phase) * i64::from(amplitude) + Q30_ONE / 2) >> 30;
    scaled.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}

/// `false` for a source whose samples come from the host (`file` and `live`).
pub fn render(source: &MicSource, fs: u32, first_frame: u64, out: &mut [i16]) -> bool {
    match source {
        MicSource::Silence => {
            out.fill(0);
            true
        }
        MicSource::Tone { hz, amplitude } => {
            for (i, sample) in out.iter_mut().enumerate() {
                *sample = tone_sample(first_frame + i as u64, *hz, *amplitude, fs);
            }
            true
        }
        MicSource::File { .. } | MicSource::Live => false,
    }
}

/// One I2S DMA buffer of the BSP (`dma_frame_num=240`), 15 ms at 16 kHz.
pub const CHUNK_FRAMES: usize = 240;

/// The Audio demo's `bsp_audio_set_format(16000, 16, 1)`. The guest rate is dynamic, so it is an
/// argument.
pub const DEFAULT_FS: u32 = 16_000;

/// From the 16 and 24 kHz this board uses up to the 48 kHz a host context runs at.
pub const FS_MIN: u64 = 8_000;
pub const FS_MAX: u64 = 48_000;

pub const DURATION_MS_MAX: u64 = 600_000;

/// Interleaved 16-bit samples.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MicFile {
    pub fs: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

/// Missing and unreadable are one variant on purpose: an I/O error's text names a host path, which
/// no output may carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicFileError {
    /// Not found, not readable, or outside the audio root.
    Unreadable,
    /// The text names the format problem only.
    Invalid(String),
}

/// `pemu-api` opens no file, so the host installs this over `pemu_verify::audio::read_mic_file`,
/// which confines the name to one audio root. The name has already passed [`check_file_name`].
#[derive(Copy, Clone)]
pub struct MicIo {
    pub read: fn(&str) -> Result<MicFile, MicFileError>,
}

/// Relative, forward-slashed, no `.`, `..` or empty segment, no drive or `:`: an artifact path's
/// shape minus the lower-case rule, because the file is the user's.
pub fn check_file_name(name: &str) -> Result<(), ApiError> {
    let refuse = |why: &str| Err(usage("name", why));
    if name.is_empty() {
        return refuse("is empty");
    }
    if name.contains('\\') {
        return refuse("uses a backslash; names are forward-slashed");
    }
    if name.starts_with('/') || name.contains(':') {
        return refuse("is absolute; names are relative to the host's audio root");
    }
    if name
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return refuse("has an empty, `.` or `..` segment");
    }
    Ok(())
}

fn io_slot() -> &'static Mutex<Option<MicIo>> {
    static IO: OnceLock<Mutex<Option<MicIo>>> = OnceLock::new();
    IO.get_or_init(|| Mutex::new(None))
}

pub fn set_io(io: MicIo) {
    let mut guard: MutexGuard<'_, Option<MicIo>> = match io_slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(io);
}

fn io() -> Result<MicIo, ApiError> {
    let guard = match io_slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    (*guard).ok_or_else(|| {
        ApiError::new(E_STATE, "this build cannot read a host audio file")
            .with_hint("a host installs the reader with `commands::mic_set::set_io`")
    })
}

/// Per instance: the next `MicChunk::seq`, counted from 0 per instance (pool ids are not unique
/// across pools). `snapshot restore` resets it from the restored journal, so the journal sees no
/// gap.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct MicState {
    pub next_seq: u64,
    /// Until the instance runs past it, earlier chunks are still waiting and a new source would
    /// play over them.
    pub pending_until: VTime,
}

/// [`DURATION_MS_MAX`] of audio at `fs`. Since a call is refused while earlier chunks are pending,
/// this also caps the frames pending at once.
#[must_use]
pub fn max_frames(fs: u32) -> u64 {
    DURATION_MS_MAX * u64::from(fs) / 1_000
}

/// Rounds toward zero.
#[must_use]
pub fn downmix(file: &MicFile) -> Vec<i16> {
    let stride = usize::from(file.channels.max(1));
    file.samples
        .chunks_exact(stride)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
            (sum / stride as i32) as i16
        })
        .collect()
}

/// The codec puts the same sample in both slots.
#[must_use]
pub fn interleave(mono: &[i16], channels: u16) -> Vec<i16> {
    let stride = usize::from(channels.max(1));
    let mut out = Vec::with_capacity(mono.len() * stride);
    for sample in mono {
        out.extend(core::iter::repeat_n(*sample, stride));
    }
    out
}

const OWN_KEYS: [&str; 4] = ["instance", "fs", "channels", "duration_ms"];

/// Read from `env`'s own schema so this command carries no second copy that could drift.
fn env_mic_schema() -> serde_json::Map<String, serde_json::Value> {
    super::env::input_schema().as_value()["properties"]["mic"]["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicSetArgs {
    /// Carries the instance and the source.
    pub env: EnvArgs,
    pub fs: u32,
    /// Must match the guest's I2S0 RX layout, because `wiring::i2s` pops one sample per slot per
    /// frame and cannot tell a mono chunk from a stereo one: 1 when the driver sets
    /// `RX_CONF.rx_mono` (the demo's format; unverified on a booted image), 2 for the BSP's stereo
    /// configuration. A mismatch halves or doubles the recorded pitch.
    pub channels: u16,
    /// For `tone`; `None` journals the source only.
    pub duration_ms: Option<u64>,
}

impl MicSetArgs {
    /// The source keys go through `env`'s parser, so both commands accept the same sources.
    pub fn from_json(value: &serde_json::Value) -> Result<MicSetArgs, ApiError> {
        let args = object(value)?;
        let source_keys: Vec<String> = env_mic_schema().keys().cloned().collect();
        let mut known: Vec<&str> = source_keys.iter().map(String::as_str).collect();
        known.extend(OWN_KEYS);
        only(args, &known)?;
        let mut mic = serde_json::Map::new();
        for key in &source_keys {
            if let Some(value) = args.get(key) {
                mic.insert(key.clone(), value.clone());
            }
        }
        let mut env_json = serde_json::Map::new();
        env_json.insert("mic".to_owned(), serde_json::Value::Object(mic));
        if let Some(instance) = opt_str(args, "instance")? {
            env_json.insert("instance".to_owned(), instance.into());
        }
        let env = EnvArgs::from_json(&serde_json::Value::Object(env_json))?;
        let fs = opt_u64(args, "fs")?.unwrap_or(u64::from(DEFAULT_FS));
        if !(FS_MIN..=FS_MAX).contains(&fs) {
            return Err(usage("fs", &format!("expected {FS_MIN} to {FS_MAX} Hz")));
        }
        let fs = fs as u32;
        let channels = opt_u64(args, "channels")?.unwrap_or(1);
        if !(1..=2).contains(&channels) {
            return Err(usage("channels", "expected 1 or 2"));
        }
        let duration_ms = opt_u64(args, "duration_ms")?;
        if let Some(ms) = duration_ms
            && (ms == 0 || ms > DURATION_MS_MAX)
        {
            return Err(usage(
                "duration_ms",
                &format!("expected 1 to {DURATION_MS_MAX} ms"),
            ));
        }
        let source = env.mic.as_ref().map(|mic| &mic.source);
        if let Some(MicSource::File { name }) = source {
            check_file_name(name)?;
        }
        if let Some(MicSource::Tone { hz, .. }) = source {
            if u64::from(*hz) * 2 > u64::from(fs) {
                return Err(usage(
                    "hz",
                    &format!("{hz} Hz is above the Nyquist frequency of {fs} Hz"),
                ));
            }
        } else if duration_ms.is_some() {
            return Err(usage(
                "duration_ms",
                "only a `tone` is rendered for a duration; a `file` plays its whole length",
            ));
        }
        Ok(MicSetArgs {
            env,
            fs,
            channels: channels as u16,
            duration_ms,
        })
    }

    #[must_use]
    pub fn source(&self) -> MicSource {
        self.env
            .mic
            .as_ref()
            .map(|mic| mic.source.clone())
            .unwrap_or_default()
    }
}

/// `None` for a source journaled alone.
fn samples_of(args: &MicSetArgs) -> Result<Option<Vec<i16>>, ApiError> {
    match args.source() {
        MicSource::File { name } => {
            let file = (io()?.read)(&name).map_err(|err| match err {
                MicFileError::Unreadable => {
                    ApiError::new(E_STATE, format!("`{name}` is not a readable audio file"))
                        .with_hint("names are relative to the host's audio root")
                }
                MicFileError::Invalid(why) => {
                    ApiError::new(E_STATE, format!("`{name}` is not usable audio: {why}"))
                }
            })?;
            let frames = file.samples.len() as u64 / u64::from(file.channels.max(1));
            if frames > max_frames(args.fs) {
                return Err(usage(
                    "name",
                    &format!("`{name}` is longer than {DURATION_MS_MAX} ms"),
                ));
            }
            if file.fs != args.fs {
                return Err(usage(
                    "fs",
                    &format!(
                        "`{name}` is {} Hz and the capture is {} Hz; this command does not resample",
                        file.fs, args.fs
                    ),
                ));
            }
            Ok(Some(downmix(&file)))
        }
        source @ MicSource::Tone { .. } => Ok(args.duration_ms.map(|ms| {
            let frames = ms * u64::from(args.fs) / 1_000;
            let mut mono = vec![0i16; usize::try_from(frames).unwrap_or(0)];
            render(&source, args.fs, 0, &mut mono);
            mono
        })),
        MicSource::Silence | MicSource::Live => Ok(None),
    }
}

/// Sets the source through `env`, then journals its samples as paced chunks.
pub fn mic_set_on_pool(pool: &mut Pool, args: &MicSetArgs) -> Result<Output, ApiError> {
    let (id, rail_on) = bind_checked(pool, args)?;
    let mut session = pool.checkout(id)?;
    let out = mic_set_on_session(&mut session, args, rail_on);
    pool.checkin(session);
    out
}

/// Under the pool lock: bind, `env`'s lease check, and whether the rail is up.
fn bind_checked(pool: &mut Pool, args: &MicSetArgs) -> Result<(InstanceId, bool), ApiError> {
    let id = pool.bind(SPEC_MIC_SET.annotations, args.env.instance.as_deref())?;
    let now = pool
        .session(id)
        .map(|session| session.now())
        .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
    if let Some(state) = pool.table().get(id) {
        state.lease.check_call(
            crate::lease::LeaseHolder::Agent,
            super::env::SPEC_ENV.annotations,
            now,
        )?;
    }
    let rail_on = pool
        .table()
        .get(id)
        .is_none_or(|state| state.lifecycle != crate::instance::Lifecycle::PoweredOff);
    Ok((id, rail_on))
}

/// On a checked-out session, so the file read and journaling happen without the pool lock.
fn mic_set_on_session(
    session: &mut Session,
    args: &MicSetArgs,
    rail_on: bool,
) -> Result<Output, ApiError> {
    let id = session.id;
    // A new source must not leave an earlier call's chunks playing underneath it.
    if session.now() < session.mic.pending_until {
        return Err(ApiError::new(
            E_STATE,
            format!(
                "microphone chunks of an earlier `mic_set` are still pending until vt={}us",
                session.mic.pending_until.as_us()
            ),
        )
        .with_hint("`run` past that instant first; the new source then starts from silence"));
    }
    // Read before anything is journaled, so an unreadable file changes nothing.
    let mono = samples_of(args)?;
    if mono
        .as_ref()
        .is_some_and(|m| m.len() as u64 > max_frames(args.fs))
    {
        return Err(usage(
            "name",
            &format!(
                "the source is longer than {DURATION_MS_MAX} ms at {} Hz",
                args.fs
            ),
        ));
    }
    let mut env_args = args.env.clone();
    env_args.instance = Some(id.to_string());
    let env_out = super::env::env_on(session, &env_args, rail_on)?;
    let start = session.now();
    let instance = id.to_string();
    let mono = mono.unwrap_or_default();
    let chunks = mono.len().div_ceil(CHUNK_FRAMES) as u64;
    let first_seq = session.mic.next_seq;
    let mut end = start;
    for (k, frames) in mono.chunks(CHUNK_FRAMES).enumerate() {
        let at = frame_time(start, (k * CHUNK_FRAMES) as u64, args.fs);
        end = frame_time(start, (k * CHUNK_FRAMES + frames.len()) as u64, args.fs);
        let when = if k == 0 { At::Now } else { At::Vt(at) };
        session
            .machine()
            .input(
                when,
                InputEvent::MicChunk {
                    seq: first_seq + k as u64,
                    samples: interleave(frames, args.channels),
                },
            )
            .map_err(|_| {
                ApiError::new(
                    E_STATE,
                    format!(
                        "the machine refused microphone chunk {k} at vt={}us; {k} earlier \
                         chunk(s) of this call stay journaled",
                        at.as_us()
                    ),
                )
            })?;
        // Per chunk, so a refusal part-way leaves no gap for the next call (the journal reads a gap
        // as lost data).
        session.mic.next_seq = first_seq + k as u64 + 1;
        session.mic.pending_until = end;
    }
    let receipt = session.receipt();
    let kind = env_out.json["applied"]["mic"].clone();
    let json = serde_json::json!({
        "instance": instance,
        "vt_us": receipt.vt_us,
        "source": kind,
        "effects": env_out.json["effects"].clone(),
        "fs": args.fs,
        "channels": args.channels,
        "chunks": chunks,
        "frames": mono.len(),
        "first_seq": first_seq,
        "until_vt_us": end.as_us(),
    });
    let text = if chunks == 0 {
        format!(
            "{instance} mic_set {}: source journaled at vt={}us",
            kind_text(&kind),
            receipt.vt_us
        )
    } else {
        format!(
            "{instance} mic_set {}: {} frame(s) at {} Hz x{} in {chunks} chunk(s), vt {}us to {}us",
            kind_text(&kind),
            mono.len(),
            args.fs,
            args.channels,
            start.as_us(),
            end.as_us(),
        )
    };
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

fn kind_text(kind: &serde_json::Value) -> &str {
    kind.as_str().unwrap_or("?")
}

/// `env`'s `mic` properties, then this command's own.
pub fn input_schema() -> Schema {
    let mut properties = serde_json::Map::new();
    properties.insert("instance".to_owned(), instance_schema());
    properties.extend(env_mic_schema());
    properties.insert(
        "fs".to_owned(),
        serde_json::json!({ "type": "integer", "minimum": FS_MIN, "maximum": FS_MAX, "description": "Capture rate (16000)." }),
    );
    properties.insert(
        "channels".to_owned(),
        serde_json::json!({ "type": "integer", "minimum": 1, "maximum": 2, "description": "RX slots per frame; match the guest (1 mono)." }),
    );
    properties.insert(
        "duration_ms".to_owned(),
        serde_json::json!({ "type": "integer", "minimum": 1, "maximum": DURATION_MS_MAX, "description": "Tone ms to inject." }),
    );
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kind"],
        "description": "`mic_set` arguments.",
        "properties": properties,
    });
    Schema::try_from(schema).unwrap_or_default()
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "vt_us": { "type": "integer" },
            "source": { "type": "string" },
            "effects": { "type": "array", "items": { "type": "string" } },
            "fs": { "type": "integer" },
            "channels": { "type": "integer" },
            "chunks": { "type": "integer" },
            "frames": { "type": "integer" },
            "first_seq": { "type": "integer" },
            "until_vt_us": { "type": "integer" }
        }
    })
}

/// Set the microphone source and inject its samples.
#[command(
    api_crate = crate,
    name = "mic_set",
    group = audio,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    cli(positional = ["kind"]),
    scenario_step = "mic.set",
    errors(E_USAGE, E_STATE, E_LEASE),
    example(
        title = "Inject 10 s of a 440 Hz tone at -6 dBFS",
        args = r#"{"kind":"tone","hz":440,"amplitude":16422,"duration_ms":10000}"#,
    ),
    example(
        title = "Silence the microphone",
        args = r#"{"kind":"silence"}"#,
    ),
)]
pub fn mic_set(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = MicSetArgs::from_json(&args)?;
    let rail_on = std::cell::Cell::new(true);
    crate::pool::with_session(
        |pool| {
            let (id, rail) = bind_checked(pool, &args)?;
            rail_on.set(rail);
            Ok(id)
        },
        |session| mic_set_on_session(session, &args, rail_on.get()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex as StdMutex};

    use pemu_core::input::EnvChange;

    use crate::commands::audio_capture::{analyze, rms};
    use crate::commands::env::tests::journaled;

    fn events(journal: &Arc<StdMutex<Vec<InputEvent>>>) -> Vec<InputEvent> {
        journal.lock().expect("not poisoned").clone()
    }

    fn args(json: serde_json::Value) -> Result<MicSetArgs, ApiError> {
        MicSetArgs::from_json(&json)
    }

    #[test]
    fn the_cordic_sine_hits_the_cardinal_points_within_a_few_q30_lsb() {
        let close = |got: i64, want: i64| (got - want).abs() <= 8;
        assert!(close(sine_q30(0), 0), "{}", sine_q30(0));
        assert!(close(sine_q30(1 << 30), Q30_ONE), "{}", sine_q30(1 << 30));
        assert!(close(sine_q30(1 << 31), 0), "{}", sine_q30(1 << 31));
        assert!(close(sine_q30(3 << 30), -Q30_ONE), "{}", sine_q30(3 << 30));
        // sin(30 degrees) = 1/2: a twelfth of a turn.
        let twelfth = (1u64 << 32) / 12;
        assert!(
            close(sine_q30(twelfth as u32), Q30_ONE / 2),
            "{}",
            sine_q30(twelfth as u32)
        );
    }

    #[test]
    fn a_tone_is_periodic_bounded_by_its_amplitude_and_has_a_sine_rms() {
        let fs = 16_000;
        let one_second: Vec<i16> = (0..16_000)
            .map(|n| tone_sample(n, 440, 16_422, fs))
            .collect();
        // Frame fs is frame 0 again, however far the tone has run.
        for n in [0u64, 1, 17, 9_999] {
            assert_eq!(
                tone_sample(n, 440, 16_422, fs),
                tone_sample(n + 1_000 * 16_000, 440, 16_422, fs)
            );
        }
        let report = analyze(&one_second, fs);
        assert_eq!(
            report.peak, 16_422,
            "440 Hz at 16 kHz reaches its crest on a sample"
        );
        let want = 16_422.0 / 2f64.sqrt();
        assert!((rms(&one_second) - want).abs() / want < 1e-3);
        let hz = report.fundamental_hz.expect("a tone");
        assert!((hz - 440.0).abs() < 0.05, "{hz}");
    }

    #[test]
    fn render_gives_silence_and_tone_and_leaves_host_sources_to_the_host() {
        let mut out = [7i16; 4];
        assert!(render(&MicSource::Silence, 16_000, 0, &mut out));
        assert_eq!(out, [0; 4]);
        assert!(render(
            &MicSource::Tone {
                hz: 4_000,
                amplitude: 1_000
            },
            16_000,
            0,
            &mut out
        ));
        assert_eq!(
            out,
            [0, 1_000, 0, -1_000],
            "a quarter-rate tone steps a quarter turn"
        );
        assert!(!render(
            &MicSource::File { name: "x".into() },
            16_000,
            0,
            &mut out
        ));
        assert!(!render(&MicSource::Live, 16_000, 0, &mut out));
    }

    #[test]
    fn the_source_is_parsed_by_env_so_both_commands_refuse_the_same_things() {
        let bad = serde_json::json!({"kind": "radio"});
        let ours = args(bad.clone()).expect_err("not a kind");
        let theirs = EnvArgs::from_json(&serde_json::json!({"mic": bad})).expect_err("not a kind");
        assert_eq!(ours.code, E_USAGE);
        assert_eq!(ours.message, theirs.message);
        assert_eq!(
            args(serde_json::json!({"kind": "tone", "hz": 5_000, "amplitude": 1, "fs": 8_000}))
                .expect_err("above Nyquist")
                .code,
            E_USAGE
        );
        assert_eq!(
            args(serde_json::json!({"kind": "silence", "duration_ms": 10}))
                .expect_err("tone only")
                .code,
            E_USAGE
        );
        assert_eq!(
            args(serde_json::json!({"kind": "silence", "channels": 3}))
                .expect_err("1 or 2")
                .code,
            E_USAGE
        );
    }

    #[test]
    fn a_tone_journals_the_env_source_then_numbered_dma_sized_chunks() {
        let (mut pool, id, journal) = journaled();
        let set = args(serde_json::json!({
            "kind": "tone", "hz": 440, "amplitude": 16_422, "duration_ms": 30, "channels": 2
        }))
        .expect("valid");
        let out = mic_set_on_pool(&mut pool, &set).expect("journaled");
        let seen = events(&journal);
        assert_eq!(
            seen[0],
            InputEvent::Env(EnvChange::MicSource(MicSource::Tone {
                hz: 440,
                amplitude: 16_422
            })),
            "the same entry `env` journals"
        );
        // Two DMA buffers.
        assert_eq!(out.json["chunks"], 2);
        assert_eq!(out.json["until_vt_us"], 30_000);
        let first = out.json["first_seq"].as_u64().expect("a seq");
        assert_eq!(first, 0, "a fresh instance numbers its chunks from 0");
        for (k, event) in seen[1..].iter().enumerate() {
            let InputEvent::MicChunk { seq, samples } = event else {
                panic!("event {k} is not a chunk");
            };
            assert_eq!(*seq, first + k as u64);
            assert_eq!(samples.len(), CHUNK_FRAMES * 2);
            let n = (k * CHUNK_FRAMES) as u64 + 5;
            assert_eq!(
                samples[10],
                tone_sample(n, 440, 16_422, 16_000),
                "left slot"
            );
            assert_eq!(samples[11], samples[10], "the same sample in both slots");
        }
        pool.session_mut(id)
            .expect("the instance")
            .run_until(pemu_core::time::VTime::from_ms(30));
        let again = mic_set_on_pool(&mut pool, &set).expect("journaled");
        assert_eq!(again.json["first_seq"].as_u64(), Some(first + 2));
    }

    /// Two pools each mint `p1`.
    #[test]
    fn chunk_numbers_belong_to_the_instance_not_to_the_process() {
        let set = args(serde_json::json!({
            "kind": "tone", "hz": 440, "amplitude": 1_000, "duration_ms": 30
        }))
        .expect("valid");
        let (mut first_pool, first_id, _) = journaled();
        let (mut second_pool, second_id, _) = journaled();
        assert_eq!(
            first_id.to_string(),
            second_id.to_string(),
            "both pools mint p1"
        );
        let a = mic_set_on_pool(&mut first_pool, &set).expect("journaled");
        let b = mic_set_on_pool(&mut second_pool, &set).expect("journaled");
        assert_eq!(a.json["first_seq"], 0);
        assert_eq!(
            b.json["first_seq"], 0,
            "a fresh instance starts its own count"
        );
    }

    static FILE_SAMPLES: StdMutex<Vec<i16>> = StdMutex::new(Vec::new());

    /// `speech.wav` is stereo 16 kHz, `long.wav` is one frame over the cap, `broken.wav` is not a
    /// WAV, and every other name is unreadable with an error that names a host path.
    fn read_stereo_16k(name: &str) -> Result<MicFile, MicFileError> {
        if name == "long.wav" {
            return Ok(MicFile {
                fs: 16_000,
                channels: 1,
                samples: vec![0; max_frames(16_000) as usize + 1],
            });
        }
        if name == "broken.wav" {
            return Err(MicFileError::Invalid("not a RIFF/WAVE file".to_owned()));
        }
        if name != "speech.wav" {
            return Err(MicFileError::Unreadable);
        }
        Ok(MicFile {
            fs: 16_000,
            channels: 2,
            samples: FILE_SAMPLES.lock().expect("not poisoned").clone(),
        })
    }

    #[test]
    fn a_file_is_downmixed_chunked_and_a_rate_mismatch_journals_nothing() {
        set_io(MicIo {
            read: read_stereo_16k,
        });
        *FILE_SAMPLES.lock().expect("not poisoned") =
            (0..300).flat_map(|i: i16| [i * 2, i * 4]).collect();
        let (mut pool, _id, journal) = journaled();

        let wrong = args(serde_json::json!({"kind": "file", "name": "speech.wav", "fs": 24_000}))
            .expect("valid arguments");
        let err = mic_set_on_pool(&mut pool, &wrong).expect_err("no resampling");
        assert_eq!(err.code, E_USAGE);
        assert!(
            events(&journal).is_empty(),
            "a refused file changes nothing"
        );

        let missing = args(serde_json::json!({"kind": "file", "name": "nope.wav"})).expect("valid");
        let err = mic_set_on_pool(&mut pool, &missing).expect_err("unreadable");
        assert_eq!(err.code, E_STATE);
        assert_eq!(
            err.message, "`nope.wav` is not a readable audio file",
            "one message for missing and unreadable, naming only what the caller wrote"
        );
        let broken =
            args(serde_json::json!({"kind": "file", "name": "broken.wav"})).expect("valid");
        let err = mic_set_on_pool(&mut pool, &broken).expect_err("not a WAV");
        assert!(
            err.message.contains("not a RIFF/WAVE file"),
            "{}",
            err.message
        );
        let long = args(serde_json::json!({"kind": "file", "name": "long.wav"})).expect("valid");
        assert_eq!(
            mic_set_on_pool(&mut pool, &long)
                .expect_err("too long")
                .code,
            E_USAGE
        );
        assert!(events(&journal).is_empty(), "refused files change nothing");

        let right = args(serde_json::json!({"kind": "file", "name": "speech.wav"})).expect("valid");
        let out = mic_set_on_pool(&mut pool, &right).expect("journaled");
        assert_eq!(out.json["frames"], 300);
        assert_eq!(out.json["chunks"], 2, "240 + 60 frames");
        let seen = events(&journal);
        let InputEvent::MicChunk { samples, .. } = &seen[2] else {
            panic!("the tail chunk");
        };
        assert_eq!(samples.len(), 60);
        // Frame 250 was (500, 1000): its mono mean is 750.
        assert_eq!(samples[10], 750);
    }

    #[test]
    fn a_file_name_that_could_leave_the_audio_root_is_refused() {
        for name in [
            "/etc/passwd",
            "\\\\server\\share\\a.wav",
            "C:\\a.wav",
            "c:a.wav",
            "../a.wav",
            "clips/../../a.wav",
            "clips\\a.wav",
            "",
        ] {
            let err = args(serde_json::json!({"kind": "file", "name": name}))
                .expect_err("outside the audio root");
            assert_eq!(err.code, E_USAGE, "{name}");
        }
        args(serde_json::json!({"kind": "file", "name": "clips/speech-16k.wav"}))
            .expect("a relative name under the root");
    }

    #[test]
    fn the_command_is_registered_in_the_audio_caps_group() {
        let spec = crate::registry::find("mic_set").expect("#[command] registered it");
        assert_eq!(spec.group, crate::spec::CapsGroup::Audio);
    }

    #[test]
    fn a_new_source_is_refused_while_earlier_chunks_are_pending() {
        let (mut pool, id, journal) = journaled();
        let tone = args(serde_json::json!({
            "kind": "tone", "hz": 440, "amplitude": 1_000, "duration_ms": 30
        }))
        .expect("valid");
        mic_set_on_pool(&mut pool, &tone).expect("journaled");
        let before = events(&journal).len();
        let silence = args(serde_json::json!({"kind": "silence"})).expect("valid");
        let err = mic_set_on_pool(&mut pool, &silence).expect_err("chunks are pending");
        assert_eq!(err.code, E_STATE);
        assert!(err.hint.as_deref().unwrap_or_default().contains("run"));
        assert_eq!(
            events(&journal).len(),
            before,
            "a refused call journals nothing"
        );
        let session = pool.session_mut(id).expect("the instance");
        session.run_until(pemu_core::time::VTime::from_ms(30));
        mic_set_on_pool(&mut pool, &silence).expect("the earlier chunks have played");
        assert_eq!(max_frames(16_000), 9_600_000);
    }

    struct RefusingMachine {
        vt: pemu_core::time::VTime,
        io: pemu_core::hostio::HostIo,
        accept: usize,
        taken: usize,
    }

    crate::commands::start::tests::refuse_snapshots!(RefusingMachine);

    impl pemu_machine::MachineApi for RefusingMachine {
        fn run(&mut self, lim: pemu_machine::run::RunLimits) -> pemu_machine::run::RunOutcome {
            if let Some(until) = lim.until {
                self.vt = pemu_core::time::VTime(self.vt.0.max(until.0));
            }
            pemu_machine::run::RunOutcome {
                reason: pemu_machine::stops::StopReason::Until,
                vt: self.vt,
                insns: 0,
                ff_insns: 0,
                idle_ps: 0,
            }
        }

        fn input(
            &mut self,
            _at: At,
            _ev: InputEvent,
        ) -> Result<u64, pemu_machine::machine::InputError> {
            if self.taken >= self.accept {
                return Err(pemu_machine::machine::InputError::default());
            }
            self.taken += 1;
            Ok(self.taken as u64 - 1)
        }

        fn io(&mut self) -> &mut pemu_core::hostio::HostIo {
            &mut self.io
        }

        fn now(&self) -> pemu_core::time::VTime {
            self.vt
        }

        fn guest_mem(&mut self) -> pemu_machine::machine::GuestMem<'_> {
            unreachable!("`mic_set` reads no guest memory")
        }

        fn is_tainted(&self) -> bool {
            false
        }
        fn receipt(&mut self) -> pemu_machine::machine::Receipt {
            pemu_machine::machine::Receipt::default()
        }
    }

    #[test]
    fn a_refusal_part_way_leaves_no_gap_in_the_chunk_numbers() {
        use crate::commands::start::{Boot, StartArgs};
        let mut pool = Pool::new();
        let start = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        // The env source and two chunks are taken; the third is refused.
        let machine = RefusingMachine {
            vt: pemu_core::time::VTime(0),
            io: pemu_core::hostio::HostIo::new(1024),
            accept: 3,
            taken: 0,
        };
        let id = pool.attach(&start, Box::new(machine));
        pool.table_mut()
            .get_mut(id)
            .expect("just created")
            .transition(
                crate::instance::Lifecycle::Paused,
                pemu_core::time::VTime(0),
            )
            .expect("starting -> paused");
        let tone = args(serde_json::json!({
            "kind": "tone", "hz": 440, "amplitude": 1_000, "duration_ms": 60
        }))
        .expect("valid");
        let err = mic_set_on_pool(&mut pool, &tone).expect_err("the third chunk is refused");
        assert_eq!(err.code, E_STATE);
        let session = pool.session_mut(id).expect("the instance");
        assert_eq!(session.mic.next_seq, 2, "two chunks reached the journal");
        assert_eq!(
            session.mic.pending_until,
            pemu_core::time::VTime::from_ms(30)
        );
    }

    #[test]
    fn the_source_keys_and_bounds_are_the_ones_env_declares() {
        let env = crate::commands::env::input_schema();
        let env_mic = &env.as_value()["properties"]["mic"]["properties"];
        let ours = input_schema();
        for (key, bound) in env_mic.as_object().expect("env declares mic properties") {
            assert_eq!(&ours.as_value()["properties"][key], bound, "{key}");
        }
        assert_eq!(
            ours.as_value()["properties"]["hz"]["maximum"],
            crate::commands::env::MIC_TONE_HZ_MAX
        );
        assert!(
            args(serde_json::json!({"kind": "tone", "hz": 440, "amplitude": 1, "bogus": 1}))
                .is_err()
        );
    }
}
