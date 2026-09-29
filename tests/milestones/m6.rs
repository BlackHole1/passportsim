//! Milestone M6 tests: audio. Names use the prefix `t<tier>_m6_` so `xtask ci` can count them.
//!
//! The Audio demo is `main/demo_audio.c` of the `official` build: the menu's third
//! card (DOWN twice, then OK) opens a page whose OK click plays 1 s of a 1 kHz square wave at ±6000
//! in 512-sample writes, and whose UP click records 3 s into one `malloc(96000)` buffer and plays it
//! back, each after `bsp_audio_set_format(16000, 16, 1)`.
//!
//! Buttons go through the registry `input` command where its 330 ms (an 80 ms click and 250 ms
//! run) does not matter, and are journaled at an exact instant where a capture must start before
//! the firmware reacts. Microphone samples are `mic_set` chunks, drained into I2S0 RX by
//! `wiring::i2s`. The t0 tests at the end drive that path with no machine.

// Not every milestone uses every shared helper.
#[allow(dead_code)]
mod common;
use common::{one_bench_record, xtask_bench};
// The host legs of the determinism harness, shared with m1.rs and m3.rs.
#[allow(dead_code)]
mod determinism;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use pemu_api::commands::audio_capture::{self, Capture};
use pemu_api::commands::mic_set::{self, CHUNK_FRAMES};
use pemu_api::instance::InstanceId;
use pemu_core::fidelity::FidelityLedger;
use pemu_core::hostio::{HostIo, SerialStream};
use pemu_core::input::{ButtonId, InputEvent, MicSource};
use pemu_core::regstore::Size;
use pemu_core::sched::{Owner, Scheduler};
use pemu_core::snap::{SectionId, SnapOpts, serde_from_section};
use pemu_core::time::{VTime, frame_time};
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_machine::config::{Assets, MachineConfig};
use pemu_machine::determinism::{Observation, Variant};
use pemu_machine::machine::{At, Machine};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};
use pemu_soc_c3::periph::i2s0::{self, Dir, I2sDma};
use pemu_soc_c3::periph::{Peripheral, Wiring};
use pemu_soc_c3::wiring;
use pemu_testkit::reg_harness::RegHarness;
use pemu_verify::audio as verify;

/// The merged image of the corpus `official` id.
const OFFICIAL_IMAGE: &str = "FoloToy-AI-Passport-8MB.bin";
/// Its app ELF, which the `ui` walker reads.
const OFFICIAL_ELF: &str = "FoloToy-AI-Passport.elf";

/// The demo's square wave: 1 kHz at ±6000, 1000 ms (`demo_audio.c` `TONE_HZ`, `TONE_MS`).
const SQUARE_HZ: u32 = 1_000;
const SQUARE_PEAK: i16 = 6_000;
const SQUARE_FRAMES: usize = 16_000;
/// The demo's recording: 3 s at 16 kHz (`RECORD_SEC`).
const RECORD_FRAMES: usize = 48_000;
/// Length of one ladder click the tests journal themselves, the `input` default.
const CLICK_MS: u64 = 80;

/// The registry and its hooks are process state, so the tests that install them take turns.
static HOST: Mutex<()> = Mutex::new(());

/// One test's hold on the process-wide host: the lock, and a scratch directory with the audio
/// root and the artifacts root in it, removed on drop.
struct Host {
    _guard: MutexGuard<'static, ()>,
    scratch: PathBuf,
}

impl Host {
    fn audio_root(&self) -> PathBuf {
        self.scratch.join("audio")
    }

    fn artifacts(&self) -> PathBuf {
        self.scratch.join("artifacts")
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// Installs the host's real backend and hooks over the corpus `official` image and ELF, with
/// audio and artifacts roots of this test's own, or `None` after the SKIP line.
fn host(test: &str) -> Option<Host> {
    let bin = common::corpus_file_or_skip(test, common::OFFICIAL, OFFICIAL_IMAGE)?;
    let elf = common::corpus_file_or_skip(test, common::OFFICIAL, OFFICIAL_ELF)?;
    let guard = HOST.lock().unwrap_or_else(|e| e.into_inner());
    static WORLD: OnceLock<(Arc<Vec<u8>>, Arc<pemu_host::hooks::ElfContext>)> = OnceLock::new();
    let (image, context) = WORLD.get_or_init(|| {
        let elf = std::fs::read(elf).expect("the verified corpus ELF is readable");
        (
            Arc::new(std::fs::read(bin).expect("the verified corpus image is readable")),
            Arc::new(pemu_host::hooks::ElfContext::parse(&elf).expect("the ELF parses")),
        )
    });
    let (image, context) = (Arc::clone(image), Arc::clone(context));
    let scratch = std::env::temp_dir().join(format!("pemu-m6-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(scratch.join("audio")).expect("a scratch audio root");
    std::fs::create_dir_all(scratch.join("artifacts")).expect("a scratch artifacts root");
    pemu_host::backend::install(
        Arc::new(move |fw: &str| match fw {
            common::OFFICIAL => pemu_host::backend::merged_image(&image),
            other => Err(pemu_api::commands::start::firmware_not_found(other)),
        }),
        None,
        pemu_host::audio_root::AudioRoot::new(scratch.join("audio")),
    );
    pemu_host::hooks::install(pemu_host::hooks::HostHooks {
        elves: Arc::new(move |fw: &str| (fw == common::OFFICIAL).then(|| Arc::clone(&context))),
        scenario_root: pemu_host::hooks::ScenarioRoot::new(None, Vec::new()),
        salt_dir: None,
    });
    pemu_host::boot_cache::install(None);
    Some(Host {
        _guard: guard,
        scratch,
    })
}

/// One registry call, as the daemon and the CLI make it, which must succeed.
#[track_caller]
fn call(name: &str, args: serde_json::Value) -> pemu_api::output::Output {
    let spec = pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"));
    (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args.clone())
        .unwrap_or_else(|e| panic!("`{name}` {args} failed: {e:?}"))
}

fn with_session<R>(id: &str, f: impl FnOnce(&mut pemu_api::commands::start::Session) -> R) -> R {
    let parsed = InstanceId::parse(id).expect("an instance id");
    pemu_api::commands::start::with_pool(|pool| f(pool.session_mut(parsed).expect("a session")))
}

/// Journals a click of `button` whose press starts `after_ms` from now.
fn click_at(id: &str, after_ms: u64, button: ButtonId) {
    with_session(id, |session| {
        let at = VTime(session.now().0 + VTime::from_ms(after_ms).0);
        let release = VTime(at.0 + VTime::from_ms(CLICK_MS).0);
        let machine = session.machine();
        machine
            .input(
                At::Vt(at),
                InputEvent::Button {
                    id: button,
                    down: true,
                },
            )
            .expect("a future press is journaled");
        machine
            .input(
                At::Vt(release),
                InputEvent::Button {
                    id: button,
                    down: false,
                },
            )
            .expect("a future release is journaled");
    });
}

fn console_from(id: &str, from: u64) -> String {
    with_session(id, |session| {
        let ring = session.machine().io().serial_ring(SerialStream::UsjTx);
        String::from_utf8_lossy(&ring.slices(from).iter().copied().collect::<Vec<u8>>())
            .into_owned()
    })
}

fn console_head(id: &str) -> u64 {
    with_session(id, |session| {
        session
            .machine()
            .io()
            .serial_ring(SerialStream::UsjTx)
            .head()
    })
}

/// Starts `official`, lets `app_main` return, and opens the Audio demo from the menu. The page's
/// own label is checked through the `ui` tree, so a flow that stayed on the menu fails here. The
/// instance is returned after one tone has played, with the codec in the demo's format.
fn open_audio_demo() -> String {
    let started = call("start", serde_json::json!({"fw": common::OFFICIAL}));
    let id = started.json["instance"]
        .as_str()
        .expect("an instance id")
        .to_owned();
    let ready = call(
        "run",
        serde_json::json!({
            "instance": id,
            "until": "serial:/main: 就绪:Display=1 Button=1 Audio=1 Battery=1/",
            "timeout": "5s",
        }),
    );
    assert_eq!(
        ready.json["status"], "matched",
        "the codec answers, so the demo can run: {}",
        ready.text
    );
    for button in ["down", "down", "ok"] {
        call(
            "input",
            serde_json::json!({"instance": id, "button": button}),
        );
    }
    let tree = call("ui", serde_json::json!({"instance": id}));
    let text = tree.json["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("OK: 1kHz TONE") && text.contains("UP: RECORD + PLAY"),
        "the Audio demo page is on screen:\n{text}"
    );
    // The demo's first `bsp_audio_set_format(16000, 16, 1)` turns the BSP's stereo TX stream mono,
    // and one WAV holds one format, so one tone plays before any capture.
    call("input", serde_json::json!({"instance": id, "button": "ok"}));
    call("run", serde_json::json!({"instance": id, "for": "1500ms"}));
    id
}

/// A 16-bit mono WAV the instance wrote with `audio_capture`, read back from its artifact.
struct Captured {
    json: serde_json::Value,
    wav: verify::Wav,
}

/// Captures `ms` of the instance's playback with `audio_capture` in `digital` mode into this
/// test's artifacts root and reads the WAV back; its SHA-256 must be the one the command reported.
fn capture(host: &Host, id: &str, ms: u64) -> Captured {
    let dir = pemu_host::artifacts::ArtifactDir::create(&host.artifacts(), "m6", id)
        .expect("an artifact directory");
    let out = pemu_host::artifacts::bind_current(Some(Arc::new(Mutex::new(dir))), || {
        call(
            "audio_capture",
            serde_json::json!({"instance": id, "duration_ms": ms, "mode": "digital", "wav": true}),
        )
    });
    let path = out.json["wav"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("a WAV artifact: {}", out.json));
    assert!(path.ends_with(".wav"), "{path}");
    pemu_api::output::check_artifact_path(path).expect("a relative artifact path");
    let bytes = std::fs::read(host.artifacts().join("m6").join(id).join(path))
        .expect("the WAV is written where the command says");
    assert_eq!(
        out.json["wav"]["sha256"].as_str(),
        Some(pemu_api::commands::snapshot::sha256_hex(&bytes).as_str()),
        "the reported hash is the file's"
    );
    let wav = verify::parse_wav(&bytes).expect("the artifact is a WAV");
    assert_eq!(
        out.json["dropped_samples"], 0,
        "the capture kept up: {}",
        out.text
    );
    assert_eq!(
        out.json["discontinuities"], 0,
        "one continuous run: {}",
        out.text
    );
    Captured {
        json: out.json,
        wav,
    }
}

/// The non-overlapping runs of samples whose magnitude exceeds `floor`, merged across gaps shorter
/// than `gap`, as `(start, end)` sample indices.
fn bursts(samples: &[i16], floor: u16, gap: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (i, s) in samples.iter().enumerate() {
        if s.unsigned_abs() <= floor {
            continue;
        }
        match out.last_mut() {
            Some((_, end)) if i - *end <= gap => *end = i + 1,
            _ => out.push((i, i + 1)),
        }
    }
    out
}

fn stop(id: &str) {
    call("stop", serde_json::json!({"instance": id}));
}

fn mono_of(cap: &Captured) -> Vec<i16> {
    let json = &cap.json;
    assert_eq!(json["mode"], "digital");
    assert_eq!(json["fs"], FS, "{json}");
    assert_eq!(
        (cap.wav.fs, u64::from(cap.wav.channels)),
        (FS, json["channels"].as_u64().unwrap_or(0))
    );
    assert_eq!(
        cap.wav.frames() as u64,
        json["frames"].as_u64().unwrap_or(0)
    );
    cap.wav.channel(0)
}

/// The one burst a capture holds, as `(start, end)`, with everything outside it silent. A sine may
/// touch 0 at either edge, so a burst of `frames` frames reads as up to two frames shorter.
#[track_caller]
fn one_burst(mono: &[i16], frames: usize) -> (usize, usize) {
    let found = bursts(mono, 0, 64);
    assert_eq!(found.len(), 1, "one burst in the capture: {found:?}");
    let (start, end) = found[0];
    assert!(
        (frames - 2..=frames).contains(&(end - start)),
        "a burst of {frames} frames: {start}..{end}"
    );
    (start, end)
}

/// Checks that `played` is a contiguous window of `input` (periodic with `period` samples) sample
/// for sample, and returns the correlation of the playback against the input over delays
/// `0..period`: `verify::correlate` finds the `lag` at which `output[lag..]` follows `input[..]`.
#[track_caller]
fn assert_window_of(played: &[i16], input: &[i16], period: usize) -> verify::Correlation {
    let c = verify::correlate(input, played, period - 1).expect("the input overlaps the playback");
    assert!(
        c.coefficient >= 0.99,
        "correlation {} at lag {}",
        c.coefficient,
        c.lag
    );
    assert_eq!(
        &played[c.lag..],
        &input[..played.len() - c.lag],
        "digital mode with unity modeled gain: the playback is the injected samples"
    );
    assert_eq!(
        &played[..c.lag],
        &input[period - c.lag..period],
        "and the samples before the delay are the end of one input period"
    );
    c
}

/// The Audio demo's OK click, captured for 10 s at 16 kHz in `digital` mode: fundamental 1000 Hz
/// within ±0.5 % and peak magnitude 6000. The capture holds exactly the demo's 16000-frame square
/// wave, 8 samples at +6000 then 8 at -6000, and silence around it, and the WAV reads back as the
/// samples whose hash the command reported.
#[test]
fn t1_m6_audio_demo_tone() {
    let test = "t1_m6_audio_demo_tone";
    let Some(host) = host(test) else {
        return;
    };
    let id = open_audio_demo();
    click_at(&id, 100, ButtonId::Ok);
    let cap = capture(&host, &id, 10_000);
    let mono = mono_of(&cap);
    // The command drains whole I2S periods, so 10 s reads as up to one period of 240 frames more.
    assert!(
        (160_000..160_240).contains(&mono.len()),
        "10 s at 16 kHz: {} frames",
        mono.len()
    );

    let hz = cap.json["analysis"]["fundamental_hz"]
        .as_f64()
        .unwrap_or_else(|| panic!("a fundamental: {}", cap.json));
    let target = f64::from(SQUARE_HZ);
    assert!(
        (hz - target).abs() <= target * 0.005,
        "tone fundamental {hz} Hz"
    );
    assert_eq!(cap.json["analysis"]["peak"], SQUARE_PEAK, "tone peak");
    let report = audio_capture::analyze(&mono, FS);
    assert_eq!(
        report.peak, SQUARE_PEAK as u16,
        "the WAV is what was analyzed"
    );
    let wav_hz = report.fundamental_hz.expect("the WAV has a fundamental");
    assert!(
        (wav_hz - hz).abs() < 0.01,
        "the reported fundamental ({hz}, rounded for JSON) is the WAV's ({wav_hz})"
    );

    let (start, end) = one_burst(&mono, SQUARE_FRAMES);
    assert_eq!(end - start, SQUARE_FRAMES, "a square wave never touches 0");
    let period = (FS / SQUARE_HZ) as usize;
    for (i, sample) in mono[start..end].iter().enumerate() {
        let want = if i % period < period / 2 {
            SQUARE_PEAK
        } else {
            -SQUARE_PEAK
        };
        assert_eq!(*sample, want, "square wave sample {i}");
    }
    println!(
        "RAN {test} official: 10 s at {FS} Hz, fundamental {hz:.3} Hz, peak {}, square burst at \
         frame {start}, WAV {}",
        report.peak, cap.json["wav"]["sha256"]
    );
    stop(&id);
}

/// Sixty seconds virtual of the Audio demo in both directions (seven rounds of the OK tone and the
/// UP record-and-play with a `mic_set` tone) print no I2S or `bsp_audio_write` timeout and no
/// warning or error line.
///
/// The evidence is the capture: all fourteen bursts, every square wave exactly 16000 frames and
/// every playback its full 48000 (a timed-out write would cut one short), with no discontinuity or
/// dropped sample. The console check is auxiliary: IDF `i2s_common` returns `ESP_ERR_TIMEOUT`
/// without a log line while `CONFIG_I2S_ENABLE_DEBUG_LOG` is off.
#[test]
fn t1_m6_no_audio_timeout_log() {
    let test = "t1_m6_no_audio_timeout_log";
    let Some(host) = host(test) else {
        return;
    };
    let id = open_audio_demo();
    let from = console_head(&id);
    call(
        "mic_set",
        serde_json::json!({
            "instance": id, "kind": "tone", "hz": TONE_HZ,
            "amplitude": verify::amplitude_at_dbfs(-6.0), "channels": 1, "duration_ms": 61_000,
        }),
    );
    const ROUND_MS: u64 = 8_500;
    const ROUNDS: u64 = 7;
    for round in 0..ROUNDS {
        click_at(&id, 200 + round * ROUND_MS, ButtonId::Ok);
        click_at(&id, 1_700 + round * ROUND_MS, ButtonId::Up);
    }
    let cap = capture(&host, &id, 60_000);
    let console = console_from(&id, from);

    let lines: Vec<&str> = console.lines().map(str::trim_end).collect();
    let timeouts: Vec<&&str> = lines
        .iter()
        .filter(|l| {
            let lower = l.to_ascii_lowercase();
            ["timeout", "timed out", "time out"]
                .iter()
                .any(|w| lower.contains(w))
        })
        .collect();
    assert!(timeouts.is_empty(), "audio timeout lines: {timeouts:#?}");
    let warnings: Vec<&&str> = lines
        .iter()
        .filter(|l| l.starts_with("E (") || l.starts_with("W ("))
        .collect();
    assert!(
        warnings.is_empty(),
        "audio warning or error lines: {warnings:#?}"
    );

    let mono = mono_of(&cap);
    let found = bursts(&mono, 0, 64);
    assert_eq!(
        found.len(),
        2 * ROUNDS as usize,
        "a tone and a playback per round: {found:?}"
    );
    for (k, (start, end)) in found.iter().enumerate() {
        if k % 2 == 0 {
            // A square wave never touches 0, so its burst is exact.
            assert_eq!(end - start, SQUARE_FRAMES, "square burst {k}");
        } else {
            // A played-back sine may touch 0 at either edge.
            assert!(
                (RECORD_FRAMES - 2..=RECORD_FRAMES).contains(&(end - start)),
                "playback burst {k} is {} frames, not {RECORD_FRAMES}",
                end - start
            );
        }
    }
    assert_eq!(
        cap.json["discontinuities"], 0,
        "the TX stream is continuous over the minute"
    );
    assert_eq!(cap.json["dropped_samples"], 0, "no TX sample lost");
    println!(
        "RAN {test} official: 60 s virtual, {} bursts at full length, 0 discontinuities, 0 dropped \
         samples; auxiliary console check: {} lines, no timeout or warning line; {} microphone \
         samples dropped while the capture path was closed",
        found.len(),
        lines.len(),
        cap.json["mic_dropped_samples"]
    );
    stop(&id);
}

/// `mic-loopback`: a 440 Hz sine at -6 dBFS set with `mic_set` is recorded and played back by the
/// demo's UP flow, and the captured playback correlates at least 0.99 with the input. The modeled
/// gains are unity here: `digital` capture is the transmitted samples, and the host samples
/// override the codec's capture output in I2S0 RX (`wiring::i2s`, an UNVERIFIED seam), so the
/// playback is also a window of the input sample for sample.
#[test]
fn t1_m6_mic_loopback() {
    let test = "t1_m6_mic_loopback";
    let Some(host) = host(test) else {
        return;
    };
    let id = open_audio_demo();
    let amplitude = verify::amplitude_at_dbfs(-6.0);
    let set = call(
        "mic_set",
        serde_json::json!({
            "instance": id, "kind": "tone", "hz": TONE_HZ, "amplitude": amplitude,
            "channels": 1, "duration_ms": 8_000,
        }),
    );
    assert_eq!(set.json["frames"], 128_000, "{}", set.text);
    click_at(&id, 200, ButtonId::Up);
    let cap = capture(&host, &id, 7_500);
    let mono = mono_of(&cap);
    let (start, end) = one_burst(&mono, RECORD_FRAMES);
    let played = &mono[start..end];

    let mut tone = vec![0i16; 128_000];
    assert!(mic_set::render(
        &MicSource::Tone {
            hz: TONE_HZ,
            amplitude
        },
        FS,
        0,
        &mut tone
    ));
    // `tone_sample` is exactly periodic: 440 / 16000 = 11 / 400, so every recording offset has a lag
    // below 400 with the same samples.
    let c = assert_window_of(played, &tone, 400);
    let report = audio_capture::analyze(played, FS);
    assert_eq!(report.peak, amplitude.unsigned_abs());
    println!(
        "RAN {test} official: {} frames played back, correlation {:.6}, gain {:.6}, peak {} \
         ({:.2} dBFS)",
        played.len(),
        c.coefficient,
        c.gain,
        report.peak,
        verify::dbfs(f64::from(report.peak))
    );
    stop(&id);
}

/// The other microphone sources on the machine: a `file` source (a WAV under the host's audio
/// root) reaches the guest sample for sample through the demo's record-and-play, and a `silence`
/// source set after it records silence, not what the file left behind.
#[test]
fn t1_m6_mic_file_and_silence_sources_reach_the_guest() {
    let test = "t1_m6_mic_file_and_silence_sources_reach_the_guest";
    let Some(host) = host(test) else {
        return;
    };
    // A noise file, so the recorded window is located in it without ambiguity.
    let mut state = 0x2545_f491_u32;
    let noise: Vec<i16> = (0..8 * FS as usize)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 16) as i16) / 4
        })
        .collect();
    std::fs::write(
        host.audio_root().join("noise.wav"),
        audio_capture::wav_bytes(FS, 1, &noise),
    )
    .expect("the audio root is writable");

    let id = open_audio_demo();
    let set = call(
        "mic_set",
        serde_json::json!({"instance": id, "kind": "file", "name": "noise.wav", "channels": 1}),
    );
    assert_eq!(set.json["frames"], noise.len(), "{}", set.text);
    click_at(&id, 200, ButtonId::Up);
    let cap = capture(&host, &id, 7_500);
    let mono = mono_of(&cap);
    let (start, end) = one_burst(&mono, RECORD_FRAMES);
    let played = &mono[start..end];
    let head = &played[..32];
    let offsets: Vec<usize> = noise
        .windows(head.len())
        .enumerate()
        .filter(|(_, w)| *w == head)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(offsets.len(), 1, "the recording is one window of the file");
    assert_eq!(&noise[offsets[0]..offsets[0] + played.len()], played);

    // Past the file's last chunk, then silence.
    call("run", serde_json::json!({"instance": id, "for": "1s"}));
    let set = call(
        "mic_set",
        serde_json::json!({"instance": id, "kind": "silence"}),
    );
    assert_eq!(set.json["source"], "silence", "{}", set.text);
    assert_eq!(set.json["chunks"], 0);
    click_at(&id, 200, ButtonId::Up);
    let cap = capture(&host, &id, 7_500);
    let silent = mono_of(&cap);
    assert_eq!(silent.len(), 120_000);
    assert!(
        silent.iter().all(|s| *s == 0),
        "a silence source records silence: {:?}",
        bursts(&silent, 0, 64)
    );
    println!(
        "RAN {test} official: file window at frame {} played back exactly; silence recorded \
         silence",
        offsets[0]
    );
    stop(&id);
}

/// Where the scripted run ends: after the tone (3.0 s) and the whole record-and-play (4.5 s to
/// about 10.9 s) of [`e64_inputs`].
const E64_UNTIL_MS: u64 = 12_000;
/// When the scripted microphone tone starts.
const E64_MIC_FROM_MS: u64 = 4_000;
const E64_MIC_MS: u64 = 5_000;
/// A console pattern the demo never prints, so every leg stops at `until` and nothing else.
const EOF_NEVER: &str = "the I2S EOF line never printed";
/// An instruction limit no leg reaches, passed to every leg alike.
const E64_MAX_INSNS: u64 = 1_000_000_000_000;

/// The script, every input at an absolute instant: DOWN, DOWN and OK open the demo from the
/// settled menu, OK plays the tone, UP records and plays back, and the -6 dBFS 440 Hz tone is the
/// microphone meanwhile, in `mic_set`'s own chunks.
fn e64_inputs() -> Vec<(VTime, InputEvent)> {
    let mut out = Vec::new();
    for (ms, id) in [
        (1_500, ButtonId::Down),
        (2_000, ButtonId::Down),
        (2_500, ButtonId::Ok),
        (3_000, ButtonId::Ok),
        (4_500, ButtonId::Up),
    ] {
        out.push((VTime::from_ms(ms), InputEvent::Button { id, down: true }));
        out.push((
            VTime::from_ms(ms + CLICK_MS),
            InputEvent::Button { id, down: false },
        ));
    }
    let mut tone = vec![0i16; (E64_MIC_MS * u64::from(FS) / 1_000) as usize];
    assert!(mic_set::render(&e64_tone(), FS, 0, &mut tone));
    let from = VTime::from_ms(E64_MIC_FROM_MS);
    for (k, chunk) in tone.chunks(CHUNK_FRAMES).enumerate() {
        out.push((
            frame_time(from, (k * CHUNK_FRAMES) as u64, FS),
            InputEvent::MicChunk {
                seq: k as u64,
                samples: mic_set::interleave(chunk, 1),
            },
        ));
    }
    out.sort_by_key(|(at, _)| *at);
    out
}

fn e64_tone() -> MicSource {
    MicSource::Tone {
        hz: TONE_HZ,
        amplitude: verify::amplitude_at_dbfs(-6.0),
    }
}

/// `official` under `variant` with the script journaled.
fn e64_machine(image: &[u8], variant: &Variant) -> Machine {
    let flash = FlashImage::from_merged(image).expect("a corpus image parses as a merged image");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    let mut m = Machine::new(variant.config(MachineConfig::default()), assets)
        .expect("the image fits the flash");
    variant.apply(&mut m);
    for (at, ev) in e64_inputs() {
        // The wasm gate journals a `pemu_input` mic chunk as live UI input, so the native leg gives it
        // the same origin and the journal cursors agree.
        let origin = if matches!(ev, InputEvent::MicChunk { .. }) {
            pemu_core::journal::Origin::UiLive
        } else {
            pemu_core::journal::Origin::Agent
        };
        m.input_from(At::Vt(at), origin, ev)
            .expect("a future input is journaled");
    }
    m
}

fn e64_stops() -> StopSet {
    StopSet {
        matchers: vec![(
            MatcherId(1),
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern: LinePattern::Contains(EOF_NEVER.into()),
            },
        )],
        ..StopSet::default()
    }
}

fn run_to(m: &mut Machine, t: VTime) -> StopReason {
    m.run(RunLimits {
        until: Some(t),
        max_insns: Some(E64_MAX_INSNS),
        stops: e64_stops(),
    })
    .reason
}

/// One I2S period of 240 frames at 16 kHz: `240 x 1e12 / 16000` ps.
fn period() -> VTime {
    frame_time(VTime(0), CHUNK_FRAMES as u64, FS)
}

/// The pacing clock of one I2S0 direction as the `soc.i2s0` snapshot section holds it: whether it
/// runs, the picosecond its armed period ends, the frames paced since it started, and the instants
/// of the direction's pending I2S0 events in the `sched` section.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pacing {
    running: bool,
    next_end: VTime,
    frames: u64,
    scheduled: Vec<VTime>,
}

/// [`Pacing`] of TX and RX, decoded from a snapshot of `m`. The scheduler's pending event is what
/// fires the period, so a running direction must have exactly one pending I2S0 event of its tag,
/// at `next_end` to the picosecond, which the caller asserts.
fn pacing(m: &Machine) -> [Pacing; 2] {
    let snap = m.snapshot(SnapOpts::default());
    let id = SectionId::soc("i2s0");
    let section = snap.section(&id).expect("a machine snapshot has soc.i2s0");
    let model: i2s0::Model = serde_from_section(section, id, section.version, "soc.i2s0")
        .expect("the section decodes as the I2S0 model");
    let sched: Scheduler = snap.get().expect("a machine snapshot has sched");
    let owner = Owner::Periph(<i2s0::Model as Peripheral>::ID);
    [Dir::Tx, Dir::Rx].map(|dir| Pacing {
        running: model.running(dir),
        next_end: model.period_start(dir, 0),
        frames: model.frames_done(dir),
        scheduled: sched
            .pending()
            .into_iter()
            .filter(|(_, _, key)| key.owner == owner && key.tag == dir.tag())
            .map(|(at, _, _)| at)
            .collect(),
    })
}

/// (1) I2S EOF events are spaced exactly `frames x 1e12 / fs` ps: over seven seconds of tone,
/// recording and playback, at every stop the ended TX and RX periods are exactly those of a 15 ms
/// grid, and each armed period ends on the next grid instant to the picosecond, in the I2S0 model's
/// clock and in the scheduler's pending event. TX frames also land in `HostIo::audio_out` exactly
/// then, and stops one picosecond before and at each TX period end bracket all 466 but period 129.
/// (2) The PCM ring hashes, and the whole determinism report, are equal at block sizes 1 and 64
/// and under Node and `jsc` running the wasm32 build with the same script.
#[test]
fn t1_m6_i2s_eof_spacing_and_pcm_hash_parity() {
    let test = "t1_m6_i2s_eof_spacing_and_pcm_hash_parity";
    let Some(path) = common::corpus_file_or_skip(test, common::OFFICIAL, OFFICIAL_IMAGE) else {
        return;
    };
    let image = std::fs::read(&path).expect("the verified corpus image is readable");
    let until = VTime::from_ms(E64_UNTIL_MS);

    let p = period();
    assert_eq!(p.0, 15_000_000_000, "240 x 1e12 / 16000 ps");
    let mut m = e64_machine(&image, &Variant::default());
    assert_eq!(run_to(&mut m, VTime::from_ms(4_700)), StopReason::Until);
    let last = {
        let ring = &m.io().audio_out;
        let record = ring
            .record_slices(ring.record_tail())
            .iter()
            .last()
            .copied()
            .expect("the demo's codec is open and TX runs");
        assert_eq!((record.fs, record.channels), (FS, 1));
        record.time_of(ring.head())
    };
    // `last` is where the next frame starts, so its period ends one period later.
    let tx_first = VTime(last.0 + p.0);
    let [tx0, rx0] = pacing(&m);
    assert_eq!(
        (&tx0.scheduled[..], &rx0.scheduled[..]),
        (&[tx0.next_end][..], &[rx0.next_end][..]),
        "the scheduler holds one pending period event per direction, at the model's end instant"
    );
    assert!(
        tx0.running && rx0.running,
        "both directions run: {tx0:?} {rx0:?}"
    );
    assert_eq!(
        tx0.next_end, tx_first,
        "the model's armed TX period ends where the host ring says"
    );
    let rx_first = rx0.next_end;
    let frames = CHUNK_FRAMES as u64;
    let tx_base = m.io().audio_out.head();
    let mut capture = Capture::starting_at(tx_base);
    let periods = 7_000 / 15;
    // Periods of a direction whose first end is `first` that have ended by `now`.
    let ended = |now: VTime, first: VTime| {
        if now < first {
            0
        } else {
            (now.0 - first.0) / p.0 + 1
        }
    };
    // Every period end, and the picosecond before it, of both directions.
    let mut stops: Vec<u64> = (0..periods)
        .flat_map(|k| {
            let (tx, rx) = (tx_first.0 + k * p.0, rx_first.0 + k * p.0);
            [tx - 1, tx, rx - 1, rx]
        })
        .collect();
    stops.sort_unstable();
    stops.dedup();
    let mut landed = std::collections::BTreeMap::new();
    for at in stops {
        assert_eq!(run_to(&mut m, VTime(at)), StopReason::Until);
        // A run stops on an instruction boundary at or after `until`, so the check is against where it
        // stopped; a stop exactly on `at` is a picosecond bracket.
        let now = m.now();
        assert!(now.0 >= at);
        let (tx_n, rx_n) = (ended(now, tx_first), ended(now, rx_first));
        let [tx, rx] = pacing(&m);
        assert_eq!(
            (
                m.io().audio_out.head() - tx_base,
                tx.frames - tx0.frames,
                rx.frames - rx0.frames
            ),
            (frames * tx_n, frames * tx_n, frames * rx_n),
            "I2S EOF: the I2S periods that have ended by {now:?} (TX frames on the host ring, TX and \
             RX frames of the model) are exactly those of a period of {} ps from {tx_first:?} \
             (TX) and {rx_first:?} (RX)",
            p.0
        );
        assert_eq!(
            (tx.next_end, rx.next_end),
            (
                VTime(tx_first.0 + tx_n * p.0),
                VTime(rx_first.0 + rx_n * p.0)
            ),
            "I2S EOF: at {now:?} the armed TX and RX periods end on the grid to the picosecond"
        );
        assert_eq!(
            (&tx.scheduled[..], &rx.scheduled[..]),
            (&[tx.next_end][..], &[rx.next_end][..]),
            "I2S EOF: at {now:?} the scheduler fires each direction's next period at exactly that \
             instant"
        );
        landed.insert(at, now.0);
        capture.drain(&m.io().audio_out);
    }
    // Period ends of a direction that a stop landing exactly one picosecond before and a stop
    // landing exactly on do not bracket.
    let unbracketed = |first: VTime| -> Vec<u64> {
        (0..periods)
            .filter(|k| {
                let t = first.0 + k * p.0;
                !(landed[&(t - 1)] == t - 1 && landed[&t] == t)
            })
            .collect()
    };
    let (tx_open, rx_open) = (unbracketed(tx_first), unbracketed(rx_first));
    // A run can stop between instruction boundaries only while the hart idles; while it executes,
    // `until` rounds up to the 6250 ps instruction grid. TX period 129 (6643.892656250 ms, inside the
    // recording) is the one preceded by instructions, so its one-picosecond-before stop lands later.
    // The run is deterministic, so it is always that period; the model clock checked it anyway.
    let t129 = tx_first.0 + 129 * p.0;
    assert_eq!(
        tx_open,
        [129],
        "I2S EOF: TX period ends bracketed by stops, all but period 129"
    );
    assert!(
        landed[&(t129 - 1)] > t129 - 1 && landed[&t129] == t129,
        "period 129: the stop before it rounded up to an instruction boundary ({}), the stop on \
         it landed exactly",
        landed[&(t129 - 1)]
    );
    // The RX period ends are not on the instruction grid and all fall while the hart runs, so no
    // stop brackets them; the model clock is the picosecond check for RX.
    assert_eq!(
        rx_open.len() as u64,
        periods,
        "no RX end is bracketed by stops"
    );
    let (tx_exact, rx_exact) = (
        periods - tx_open.len() as u64,
        periods - rx_open.len() as u64,
    );
    // `PcmRing::write` continues a record only when a period starts exactly where the last ended, so
    // one record means no gap.
    assert_eq!(capture.discontinuities(), 0);
    let (start, end) = one_burst(&capture.samples, RECORD_FRAMES);
    let mut tone = vec![0i16; (E64_MIC_MS * u64::from(FS) / 1_000) as usize];
    assert!(mic_set::render(&e64_tone(), FS, 0, &mut tone));
    assert_window_of(&capture.samples[start..end], &tone, 400);

    // (2) The hashes: block sizes 64 and 1, then the wasm32 legs.
    let mut reports = Vec::new();
    for block in [64u16, 1] {
        let variant = Variant {
            max_block_insns: block,
            ..Variant::default()
        };
        let mut m = e64_machine(&image, &variant);
        let reason = run_to(&mut m, until);
        assert_eq!(reason, StopReason::Until, "{}", variant.label());
        assert!(
            m.io().audio_out.head() >= u64::from(FS) * 8,
            "the run produced PCM to hash"
        );
        let observed = Observation::capture(&m);
        reports.push((
            block,
            observed.pcm,
            pemu_machine::determinism::report(&reason, &m),
        ));
    }
    assert_eq!(
        reports[0].1, reports[1].1,
        "I2S EOF: PCM hash at block 64 and 1"
    );
    assert_eq!(
        reports[0].2, reports[1].2,
        "I2S EOF: the whole report at block 64 and 1"
    );
    println!(
        "RAN {test} official: {periods} TX and {periods} RX periods {} ps apart, every period end \
         of both directions on the grid to the picosecond (model clock and scheduler event), {tx_exact} TX and \
         {rx_exact} RX ends also bracketed by stops; pcm {} at block sizes 64 and 1",
        p.0,
        determinism_hex(&reports[0].1)
    );

    let script = e64_script(test);
    let base = Variant::default();
    let legs = determinism::wasm_legs_scripted(
        &determinism::WasmBoot {
            image: &path,
            pattern: EOF_NEVER,
            prefix: false,
            max_insns: E64_MAX_INSNS,
            max_block_insns: base.max_block_insns,
            max_slice: base.max_slice,
            poll_ff: base.poll_ff,
        },
        Some(&script.0),
    );
    determinism::assert_legs(
        test,
        "`official` Audio demo script, PCM hash and report",
        &reports[0].2,
        legs,
    );
}

fn determinism_hex(bytes: &[u8]) -> String {
    pemu_machine::determinism::hex(bytes)
}

/// The script as `crates/pemu-wasm/js/abi_parity.cjs` reads it: the inputs as `pemu_input` takes
/// them (`{at, event}`, `at` in picoseconds) and the `until` instant. Removed when dropped.
struct Script(PathBuf);

impl Drop for Script {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn e64_script(test: &str) -> Script {
    let inputs: Vec<serde_json::Value> = e64_inputs()
        .into_iter()
        .map(|(at, ev)| {
            serde_json::json!({
                "at": at.0.to_string(),
                "event": serde_json::to_value(&ev).expect("an InputEvent serializes"),
            })
        })
        .collect();
    let body = serde_json::json!({
        "inputs": inputs,
        "until_ps": VTime::from_ms(E64_UNTIL_MS).0.to_string(),
    });
    let path = std::env::temp_dir().join(format!("pemu-{test}-{}.json", std::process::id()));
    std::fs::write(&path, body.to_string()).expect("the temp dir is writable");
    Script(path)
}

/// Native perf, F6 (`official`'s Audio tone for 10 s, measured by `xtask bench` with a history of
/// its own): busy MIPS and idle cost c recorded and trend-gated at 10 %, the host time of the worst
/// 100 ms virtual window at most 30 ms, and the WAV artifact by path (relative to the data root)
/// and hash.
///
/// A busy host does not claim the perf half: when `xtask bench` marks the record `contended` or
/// its runs spread over both core clusters, the artifact is checked and the test SKIPs.
#[test]
fn t1_m6_native_perf_f6() {
    let test = "t1_m6_native_perf_f6";
    let id = test.to_string();
    let Some(image) = common::corpus_file_or_skip(test, common::OFFICIAL, OFFICIAL_IMAGE) else {
        return;
    };
    // `<data root>/corpus/<id>/<file>`, so the data root is three levels up.
    let data_root = image
        .ancestors()
        .nth(3)
        .expect("a corpus file sits under the data root")
        .to_path_buf();
    let dir = std::env::temp_dir().join(format!("pemu-native-perf-f6-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    let history = dir.join("history.json");
    let history_arg = history.to_str().expect("a UTF-8 temporary path");

    let stdout = xtask_bench(&[
        "--workload",
        "F6",
        "--repeat",
        // A ceiling, not a count: `xtask bench` stops once three runs agree within 10 %, which a quiet
        // host reaches in three or four (`bench/run.rs` `settled`).
        "25",
        "--history",
        history_arg,
        "--gate-exits",
        "--json",
    ])
    .unwrap_or_else(|e| panic!("{id}: {e}"));
    let record = one_bench_record(&stdout);
    assert_eq!(record["workload"], "F6");

    // Busy MIPS, c and the hard gate are host seconds, which a contended, `mixed`-cluster or
    // `efficiency`-clamped record did not measure. The guest and the artifact are exact on any host.
    let contended = record["contended"] == serde_json::json!(true);
    let mixed = matches!(
        record["cores"]["cluster"].as_str(),
        Some("mixed" | "efficiency")
    );
    let mut measured = None;
    if !contended && !mixed {
        // No floor is asserted on busy MIPS: it is trend data.
        let s = record["metrics"]["busy_mips"]
            .as_f64()
            .unwrap_or_else(|| panic!("{id}: F6 recorded no busy MIPS: {stdout}"));
        let c = record["metrics"]["idle_cost"]
            .as_f64()
            .unwrap_or_else(|| panic!("{id}: F6 recorded no idle cost: {stdout}"));
        assert!(s > 0.0 && s.is_finite(), "{id}: busy MIPS {s}");
        assert!(c >= 0.0 && c.is_finite(), "{id}: idle cost {c}");
        let worst = record["metrics"]["worst_window_ms"]
            .as_f64()
            .expect("a worst window");
        assert!(
            worst <= 30.0,
            "{id}: the worst 100 ms window of F6 took {worst:.2} ms host, over the 30 ms gate"
        );
        measured = Some((s, c, worst));
    }

    let artifact = &record["artifact"];
    let rel = artifact["path"]
        .as_str()
        .unwrap_or_else(|| panic!("{id}: F6 recorded no WAV artifact: {record}"));
    assert!(rel.ends_with(".wav"), "{id}: {rel}");
    assert!(
        !rel.starts_with('/') && !rel.contains("..") && !rel.contains(':'),
        "{id}: an artifact reference names a path under the data root, never a host path: {rel}"
    );
    let bytes = std::fs::read(data_root.join(rel))
        .unwrap_or_else(|e| panic!("{id}: the WAV is not at the recorded path: {e}"));
    assert_eq!(
        artifact["sha256"].as_str(),
        Some(pemu_testkit::corpus::sha256_hex(&bytes).as_str()),
        "{id}: the recorded hash is not the file's"
    );
    let wav = verify::parse_wav(&bytes).unwrap_or_else(|e| panic!("{id}: not a WAV: {e:?}"));
    assert_eq!(
        (wav.fs, wav.channels),
        (FS, 1),
        "{id}: the demo settles on mono 16 kHz"
    );
    assert_eq!(
        artifact["frames"].as_u64(),
        Some(wav.samples.len() as u64),
        "{id}: the recorded frame count is the file's"
    );
    let report = audio_capture::analyze(&wav.samples, FS);
    let hz = report
        .fundamental_hz
        .unwrap_or_else(|| panic!("{id}: the artifact has no fundamental"));
    let target = f64::from(SQUARE_HZ);
    assert!(
        (hz - target).abs() <= target * 0.005,
        "{id}: the artifact's fundamental is {hz} Hz, not {target} Hz +-0.5 %"
    );
    assert_eq!(
        report.peak, SQUARE_PEAK as u16,
        "{id}: the artifact is the demo's tone at its own peak"
    );

    let Some((s, c, worst)) = measured else {
        let load = record["load_avg"].as_f64().unwrap_or(f64::NAN);
        let why = if contended {
            format!(
                "host busy: one-minute load average {load:.1} on {} cores",
                std::thread::available_parallelism().map_or(1, |n| n.get())
            )
        } else {
            format!("F6 ran on both core clusters: {}", record["cores"])
        };
        common::skip(
            test,
            &format!(
                "{id} {why}; the F6 gate is an absolute host-time budget, so this host did not \
                 measure it (the WAV artifact {rel} and its 1 kHz tone above did run and passed)"
            ),
        );
        std::fs::remove_dir_all(&dir).ok();
        return;
    };

    // The trend gate, proved by a regression it has to catch: a baseline twice as fast as this host's
    // measurement.
    //
    // Twice, not 25 %: F6's S comes from a calibration phase of about 23 windows, so one run's S
    // moves more than a whole-run figure. On an uncontended host S read 210.75 here and 266.53 in
    // the control, where other runs read 263 to 276: the OS had moved the calibration phase onto the
    // efficiency cluster (S tracks the performance-core share with r = 0.97).
    //
    // A contended or spread run makes `xtask bench` report the regression NOT MEASURED rather than
    // fail; it must be named either way.
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&history).expect("the history was written"))
            .expect("the history is JSON");
    let mut faster = record.clone();
    faster["metrics"]["busy_mips"] = serde_json::json!(s * 2.0);
    faster["unix_s"] = serde_json::json!(0);
    doc["records"] = serde_json::json!([faster]);
    let planted = dir.join("planted.json");
    std::fs::write(&planted, doc.to_string()).expect("the temporary directory is writable");
    let refused = match xtask_bench(&[
        "--workload",
        "F6",
        "--repeat",
        "1",
        "--history",
        planted.to_str().expect("a UTF-8 temporary path"),
        "--no-record",
    ]) {
        Err(refused) => refused,
        Ok(out) => {
            assert!(
                out.contains("NOT MEASURED, load average")
                    || out.contains("NOT MEASURED, core cluster"),
                "{id}: a 50 % loss against the baseline must fail the 10 % gate, or be reported \
                 as NOT MEASURED on a contended host or a run spread over both core clusters; it \
                 did neither:\n{out}"
            );
            out
        }
    };
    assert!(
        refused.contains("busy_mips") && refused.contains("F6"),
        "{id}: the trend gate must name the metric and the workload: {refused}"
    );
    std::fs::remove_dir_all(&dir).ok();

    println!(
        "RAN {test} official: F6 busy MIPS {s:.2} and c {c:.5} recorded and trend-gated (a 50 % \
         loss is refused); worst 100 ms window {worst:.2} ms host (<= 30); WAV artifact {rel} \
         sha256 {} under the data root, {} frames at {FS} Hz mono, fundamental {hz:.3} Hz, peak \
         {}",
        artifact["sha256"].as_str().unwrap_or("?"),
        wav.samples.len(),
        report.peak,
    );
}

/// `limits-audio` with `probe_limits`: `malloc(96000)` succeeds (the Audio demo's record buffer),
/// a request the size of the largest free block succeeds and one byte above it fails. The heap
/// figures are measured at probe start. The pinned probe image must be the one built.
#[test]
fn t1_m6_limits_audio() {
    let test = "t1_m6_limits_audio";
    let exit = test.to_string();
    // Probe images are not corpus ids: `xtask probes` builds them into `corpus/probes/` and pins each
    // merged image in `tests/fw/manifest.toml`.
    let Some(official) = common::corpus_file_or_skip(test, common::OFFICIAL, OFFICIAL_IMAGE) else {
        return;
    };
    let path = official
        .ancestors()
        .nth(2)
        .expect("a corpus file sits under corpus/<id>/")
        .join("probes/probe_limits-8MB.bin");
    let Ok(bytes) = std::fs::read(&path) else {
        common::skip(
            test,
            &format!("{exit} corpus/probes/probe_limits-8MB.bin is not built (xtask probes)"),
        );
        return;
    };
    let fw = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fw");
    let manifest = std::fs::read_to_string(fw.join("manifest.toml")).expect("the probe manifest");
    let pinned = manifest
        .split("[[probe]]")
        .find(|block| block.contains("name = \"probe_limits\""))
        .and_then(|block| {
            block
                .lines()
                .find_map(|l| l.strip_prefix("merged_sha256 = \""))
                .map(|v| v.trim_end_matches('"').to_string())
        })
        .expect("tests/fw/manifest.toml pins the probe_limits merged image");
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&bytes),
        pinned,
        "{test}: corpus/probes/probe_limits-8MB.bin is not the pinned build"
    );

    let flash = FlashImage::from_merged(&bytes).expect("a probe image parses");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    let mut m = Machine::new(MachineConfig::default(), assets).expect("the image fits");
    let done = MatcherId(0xD0);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(30_000)),
        max_insns: None,
        stops: StopSet {
            matchers: vec![(
                done,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Prefix("DONE|".into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let console =
        String::from_utf8_lossy(&ring.slices(0).iter().copied().collect::<Vec<u8>>()).into_owned();
    assert_eq!(out.reason, StopReason::Matcher(done), "{test}:\n{console}");

    let line = |tag: &str| -> Vec<(String, String)> {
        let prefix = format!("{tag}|");
        let found = console
            .lines()
            .map(str::trim_end)
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("no {tag} line:\n{console}"));
        found[prefix.len()..]
            .split('|')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let field = |fields: &[(String, String)], key: &str| -> u64 {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or_else(|| panic!("no numeric `{key}` in {fields:?}"))
    };
    let audio = line("AUDIO");
    let above = line("ABOVE");
    assert_eq!(field(&audio, "size"), 96_000);
    assert_eq!(
        field(&audio, "ok"),
        1,
        "audio limits: malloc(96000) succeeds"
    );
    assert!(
        field(&audio, "largest") >= 96_000,
        "the largest free block holds the record buffer"
    );
    assert_eq!(
        field(&above, "at"),
        1,
        "audio limits: a request the size of the largest free block succeeds"
    );
    assert_eq!(
        field(&above, "above"),
        0,
        "audio limits: a request above the largest free block fails"
    );
    let fails: Vec<&str> = console.lines().filter(|l| l.starts_with("FAIL|")).collect();
    assert!(fails.is_empty(), "probe failures: {fails:#?}");
    assert!(
        console.contains("DONE|name=probe_limits|status=ok"),
        "the probe ends ok:\n{console}"
    );
    println!(
        "RAN {test} probe_limits: AUDIO largest={} ok=1, ABOVE largest={} at=1 above=0",
        field(&audio, "largest"),
        field(&above, "largest")
    );
}

/// Block offsets of the I2S0 registers these tests drive (IDF
/// `soc/esp32c3/register/soc/i2s_reg.h`).
const I2S_RX_CONF: u32 = 0x020;
const I2S_TX_CONF: u32 = 0x024;
const I2S_RX_CONF1: u32 = 0x028;
const I2S_TX_CONF1: u32 = 0x02C;
const I2S_RX_CLKM_CONF: u32 = 0x030;
const I2S_TX_CLKM_CONF: u32 = 0x034;
const I2S_RX_CLKM_DIV_CONF: u32 = 0x038;
const I2S_TX_CLKM_DIV_CONF: u32 = 0x03C;
const I2S_RX_TDM_CTRL: u32 = 0x050;
const I2S_TX_TDM_CTRL: u32 = 0x054;
const I2S_RXEOF_NUM: u32 = 0x064;

/// `*_CONF` with the BSP flags and `start` set (IDF `i2s_channel_enable`).
const I2S_CONF_START: u32 = (1 << 19) | (1 << 15) | (1 << 2);
/// `*_CONF` with the BSP flags and `start` clear.
const I2S_CONF_STOP: u32 = (1 << 19) | (1 << 15);

/// The capture rate of the Audio demo.
const FS: u32 = 16_000;
/// Slots per frame of the BSP's std configuration.
const SLOTS: u16 = 2;
/// `RX_CONF.rx_mono`, bit 5: one slot per RX frame, which `bsp_audio_set_format(16000, 16, 1)`
/// sets.
const I2S_RX_CONF_MONO: u32 = 1 << 5;
/// Buffers recorded and played back: 300 ms at 15 ms each.
const BUFFERS: usize = 20;
/// The injected tone.
const TONE_HZ: u32 = 440;

/// The clock `bsp_audio.c` configures, for one direction: PLL_F160M with `div_num` 39 and the
/// fractional 15/0/1/0, `bck_div_num` 7, 16-bit, stereo with both slots enabled. With `slots` 1 it
/// is the Audio demo's mono layout: TX keeps two channels on the wire with only channel 0
/// enabled, and RX sets `rx_mono`.
fn configure_i2s(
    i2s: &mut i2s0::Model,
    ledger: &mut FidelityLedger,
    dir: Dir,
    slots: u16,
    t: VTime,
) {
    let (conf, conf1, clkm, div, tdm) = match dir {
        Dir::Tx => (
            I2S_TX_CONF,
            I2S_TX_CONF1,
            I2S_TX_CLKM_CONF,
            I2S_TX_CLKM_DIV_CONF,
            I2S_TX_TDM_CTRL,
        ),
        Dir::Rx => (
            I2S_RX_CONF,
            I2S_RX_CONF1,
            I2S_RX_CLKM_CONF,
            I2S_RX_CLKM_DIV_CONF,
            I2S_RX_TDM_CTRL,
        ),
    };
    let conf1_bsp = 15 | (7 << 7) | (15 << 13) | (15 << 18) | (15 << 24) | (1 << 29);
    i2s.store(clkm, Size::B4, (2 << 27) | (1 << 26) | 39, t, ledger);
    i2s.store(div, Size::B4, (15 << 18) | 1, t, ledger);
    i2s.store(conf1, Size::B4, conf1_bsp, t, ledger);
    let enabled = if slots == 1 && dir == Dir::Tx {
        0b01
    } else {
        0b11
    };
    i2s.store(tdm, Size::B4, enabled | (1 << 16), t, ledger);
    i2s.store(
        conf,
        Size::B4,
        I2S_CONF_STOP | conf_extra(dir, slots),
        t,
        ledger,
    );
}

/// The bits a direction's `*_CONF` keeps set for this layout on every write.
fn conf_extra(dir: Dir, slots: u16) -> u32 {
    if slots == 1 && dir == Dir::Rx {
        I2S_RX_CONF_MONO
    } else {
        0
    }
}

/// A descriptor ring standing in for GDMA: the RX side keeps what the model wrote, and the TX
/// side hands out the buffers a test queued, oldest first.
struct LoopRing {
    bytes: u32,
    tx: Vec<Vec<u8>>,
    rx: Vec<Vec<u8>>,
}

impl I2sDma for LoopRing {
    fn period_bytes(&mut self, _dir: Dir) -> Option<u32> {
        Some(self.bytes)
    }

    fn take_tx(&mut self, out: &mut Vec<u8>) {
        if !self.tx.is_empty() {
            *out = self.tx.remove(0);
        }
    }

    fn put_rx(&mut self, bytes: &[u8]) {
        self.rx.push(bytes.to_vec());
    }
}

/// Runs [`BUFFERS`] periods of `dir` on the harness, starting the direction first and stopping it
/// after, so the other direction can run next on the same scheduler.
fn run_periods(
    h: &mut RegHarness,
    i2s: &mut i2s0::Model,
    ledger: &mut FidelityLedger,
    ring: &mut LoopRing,
    io: &mut HostIo,
    dir: Dir,
    slots: u16,
) {
    let conf = match dir {
        Dir::Tx => I2S_TX_CONF,
        Dir::Rx => I2S_RX_CONF,
    };
    let extra = conf_extra(dir, slots);
    i2s.store(conf, Size::B4, I2S_CONF_START | extra, h.now, ledger);
    wiring::i2s::service(i2s, dir, h.now, &mut h.sched, ring, &mut h.board, io);
    for period in 0..BUFFERS {
        let due = h.advance_to_next_event().expect("a period is scheduled");
        assert_eq!(due.len(), 1, "{dir:?} period {period}");
        assert_eq!(due[0].tag, dir.tag());
        assert!(matches!(i2s.event(due[0].tag), Wiring::I2sPeriod(d) if d == dir));
        wiring::i2s::service(i2s, dir, h.now, &mut h.sched, ring, &mut h.board, io);
    }
    i2s.store(conf, Size::B4, I2S_CONF_STOP | extra, h.now, ledger);
    wiring::i2s::service(i2s, dir, h.now, &mut h.sched, ring, &mut h.board, io);
}

/// The `mic-loopback` flow at block level: the -6 dBFS tone is injected as `mic_set` chunks,
/// recorded through I2S0 RX, and the recorded descriptors are played back through I2S0 TX into
/// `HostIo::audio_out`. Returns the mono input and the host I/O.
fn loopback(slots: u16) -> (Vec<i16>, HostIo) {
    let amplitude = verify::amplitude_at_dbfs(-6.0);
    let source = MicSource::Tone {
        hz: TONE_HZ,
        amplitude,
    };
    let mut mono = vec![0i16; BUFFERS * CHUNK_FRAMES];
    assert!(mic_set::render(&source, FS, 0, &mut mono));

    let mut h = RegHarness::new();
    let mut ledger = FidelityLedger::default();
    let mut i2s = i2s0::Model::default();
    let mut io = HostIo::new(1 << 16);
    configure_i2s(&mut i2s, &mut ledger, Dir::Rx, slots, h.now);
    configure_i2s(&mut i2s, &mut ledger, Dir::Tx, slots, h.now);
    assert_eq!(
        (i2s.format(Dir::Rx).slots, i2s.format(Dir::Tx).slots),
        (slots as u8, slots as u8)
    );
    let buffer_bytes = (CHUNK_FRAMES * usize::from(slots) * 2) as u32;
    i2s.store(I2S_RXEOF_NUM, Size::B4, buffer_bytes, h.now, &mut ledger);
    assert_eq!((i2s.fs_hz(Dir::Rx), i2s.fs_hz(Dir::Tx)), (FS, FS));

    for chunk in mono.chunks(CHUNK_FRAMES) {
        let samples = mic_set::interleave(chunk, slots);
        assert_eq!(io.audio_in.push(&samples), samples.len());
    }
    let mut ring = LoopRing {
        bytes: buffer_bytes,
        tx: Vec::new(),
        rx: Vec::new(),
    };
    run_periods(
        &mut h,
        &mut i2s,
        &mut ledger,
        &mut ring,
        &mut io,
        Dir::Rx,
        slots,
    );
    assert_eq!(ring.rx.len(), BUFFERS, "one recorded descriptor per period");
    assert_eq!(
        io.audio_in.underflows(),
        0,
        "every recorded sample was injected"
    );

    // Record then play.
    ring.tx = std::mem::take(&mut ring.rx);
    run_periods(
        &mut h,
        &mut i2s,
        &mut ledger,
        &mut ring,
        &mut io,
        Dir::Tx,
        slots,
    );
    (mono, io)
}

/// `mic-loopback` at block level: the injected, recorded and played-back tone correlates at least
/// 0.99 with the input, and in `digital` mode the left slot is sample-exact.
#[test]
fn t0_m6_mic_loopback_through_i2s0_correlates_with_the_injected_tone() {
    let (mono, io) = loopback(SLOTS);
    let mut capture = Capture::starting_at(0);
    capture.drain(&io.audio_out);
    assert_eq!(capture.format(), Some((FS, SLOTS)));
    assert_eq!(capture.discontinuities(), 0, "one continuous playback run");
    assert_eq!(capture.dropped, 0);

    let left = audio_capture::channel_of(&capture.samples, SLOTS, 0);
    let c = verify::correlate(&mono, &left, 64).expect("an overlap");
    assert!(c.coefficient >= 0.99, "correlation {}", c.coefficient);
    assert_eq!(c.lag, 0);
    assert!((c.gain - 1.0).abs() < 1e-9, "digital mode gain {}", c.gain);
    assert_eq!(left, mono, "digital mode: the exact transmitted samples");
    let right = audio_capture::channel_of(&capture.samples, SLOTS, 1);
    assert_eq!(right, mono, "the codec's sample is in both slots");
}

/// On the loopback capture: the integer tone matches a floating-point sine to one LSB, the
/// analysis reads 440 Hz within ±0.5 % and the -6 dBFS peak, and the WAV the command would write
/// reads back sample-exact under a valid artifact path.
#[test]
fn t0_m6_a_capture_is_measured_and_its_wav_reads_back_sample_exact() {
    let (mono, io) = loopback(SLOTS);
    let amplitude = verify::amplitude_at_dbfs(-6.0);
    assert_eq!(amplitude, 16_422);
    let reference =
        verify::reference_tone(f64::from(TONE_HZ), f64::from(amplitude), FS, mono.len());
    let worst = mono
        .iter()
        .zip(&reference)
        .map(|(a, b)| (f64::from(*a) - b).abs())
        .fold(0.0, f64::max);
    assert!(
        worst <= 1.0,
        "the CORDIC tone is within one LSB of a sine: {worst}"
    );

    let mut capture = Capture::starting_at(0);
    capture.drain(&io.audio_out);
    let left = audio_capture::channel_of(&capture.samples, SLOTS, 0);
    let report = audio_capture::analyze(&left, FS);
    let hz = report.fundamental_hz.expect("a tone has a fundamental");
    assert!(
        (hz - f64::from(TONE_HZ)).abs() <= f64::from(TONE_HZ) * 0.005,
        "±0.5 %: {hz}"
    );
    assert_eq!(report.peak, 16_422);
    assert!((verify::dbfs(f64::from(report.peak)) + 6.0).abs() < 0.01);

    let path = "audio/loopback.wav";
    pemu_api::output::check_artifact_path(path).expect("a relative artifact path");
    let bytes = audio_capture::wav_bytes(FS, SLOTS, &capture.samples);
    let file = std::env::temp_dir().join(format!("pemu-m6-{}-loopback.wav", std::process::id()));
    std::fs::write(&file, &bytes).expect("the temp dir is writable");
    let read = verify::read_wav(&file);
    let on_disk = std::fs::read(&file).expect("just written");
    let _ = std::fs::remove_file(&file);
    let wav = read.expect("the artifact parses");
    assert_eq!((wav.fs, wav.channels), (FS, SLOTS));
    assert_eq!(wav.samples, capture.samples);
    assert_eq!(
        pemu_api::commands::snapshot::sha256_hex(&on_disk),
        pemu_api::commands::snapshot::sha256_hex(&bytes),
        "the hash the command reports is the file's"
    );
}

/// The same loopback in the Audio demo's mono layout (480-byte descriptors, `rx_mono`, one TX
/// channel), injected with `mic_set`'s `channels: 1`, is sample-exact.
#[test]
fn t0_m6_mic_loopback_in_the_mono_layout_is_sample_exact() {
    let (mono, io) = loopback(1);
    let mut capture = Capture::starting_at(0);
    capture.drain(&io.audio_out);
    assert_eq!(capture.format(), Some((FS, 1)));
    assert_eq!(
        capture.samples.len(),
        BUFFERS * CHUNK_FRAMES,
        "480-byte buffers, 240 frames each"
    );
    assert_eq!(capture.samples, mono);
    let c = verify::correlate(&mono, &capture.samples, 64).expect("an overlap");
    assert!(c.coefficient >= 0.99, "{}", c.coefficient);
    let hz = audio_capture::fundamental_hz(&capture.samples, FS).expect("a tone");
    assert!(
        (hz - f64::from(TONE_HZ)).abs() <= f64::from(TONE_HZ) * 0.005,
        "{hz}"
    );
}

/// The codec's gating of host microphone audio (`wiring::i2s::MicPath`): with `ADCDAT_SEL` routing
/// the microphone to slot 0 only, slot 1 reads 0; with the path closed (ADC powered down or
/// muted) the buffered audio is dropped and counted, the guest reads silence, and no underflow is
/// counted.
#[test]
fn t0_m6_the_codec_gates_injected_mic_audio() {
    let mut h = RegHarness::new();
    let mut ledger = FidelityLedger::default();
    let mut i2s = i2s0::Model::default();
    let mut io = HostIo::new(1 << 16);
    configure_i2s(&mut i2s, &mut ledger, Dir::Rx, 2, h.now);
    let buffer_bytes = (CHUNK_FRAMES * 4) as u32;
    i2s.store(I2S_RXEOF_NUM, Size::B4, buffer_bytes, h.now, &mut ledger);
    let frames: Vec<i16> = (1..=CHUNK_FRAMES as i16).flat_map(|k| [k, -k]).collect();
    assert_eq!(io.audio_in.inject(&frames, 2).kept, frames.len());
    let mut ring = LoopRing {
        bytes: buffer_bytes,
        tx: Vec::new(),
        rx: Vec::new(),
    };
    let slot0 = wiring::i2s::MicPath {
        open: true,
        first_slot_only: true,
    };
    i2s.store(I2S_RX_CONF, Size::B4, I2S_CONF_START, h.now, &mut ledger);
    let period =
        |h: &mut RegHarness, i2s: &mut i2s0::Model, ring: &mut LoopRing, io: &mut HostIo, mic| {
            wiring::i2s::service_with_mic(
                i2s,
                Dir::Rx,
                h.now,
                &mut h.sched,
                ring,
                &mut h.board,
                io,
                mic,
            );
        };
    period(&mut h, &mut i2s, &mut ring, &mut io, slot0);
    let due = h.advance_to_next_event().expect("a period is scheduled");
    assert!(matches!(i2s.event(due[0].tag), Wiring::I2sPeriod(Dir::Rx)));
    period(&mut h, &mut i2s, &mut ring, &mut io, slot0);
    let got: Vec<i16> = ring.rx[0]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    let want: Vec<i16> = (1..=CHUNK_FRAMES as i16).flat_map(|k| [k, 0]).collect();
    assert_eq!(got, want, "ADCDAT_SEL != 0: the microphone on slot 0 only");
    assert_eq!(
        io.audio_in.dropped(),
        CHUNK_FRAMES as u64,
        "the slot-1 samples that did not reach the guest count as dropped"
    );

    // A closed path: the buffered chunk is dropped, the period reads the codec, nothing underflows.
    assert_eq!(io.audio_in.inject(&frames, 2).kept, frames.len());
    let closed = wiring::i2s::MicPath {
        open: false,
        first_slot_only: false,
    };
    let due = h.advance_to_next_event().expect("a period is scheduled");
    assert!(matches!(i2s.event(due[0].tag), Wiring::I2sPeriod(Dir::Rx)));
    period(&mut h, &mut i2s, &mut ring, &mut io, closed);
    assert_eq!(io.audio_in.len(), 0);
    assert_eq!(io.audio_in.dropped(), (CHUNK_FRAMES + frames.len()) as u64);
    assert_eq!(io.audio_in.underflows(), 0);
    assert!(
        ring.rx[1].iter().all(|b| *b == 0),
        "the codec's silence, not late host audio"
    );
}
