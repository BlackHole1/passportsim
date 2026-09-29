//! Cross-host parity: the same scenario on macOS arm64 and on Windows gives equal `state_hash`,
//! console bytes, frame hashes and PCM hashes, and a snapshot saved on one host restores on the
//! other.
//!
//! The scenario is built from code ([`guest`]) so it needs no corpus, and every host compares
//! against one golden of digests recorded on macOS, `tests/golden/cross-host/parity.txt` (T0 step
//! `cross-host-parity`). A mismatch is a defect to fix at its source; after a deliberate change
//! the golden is re-recorded on macOS arm64 with `PEMU_PARITY_RECORD=1 cargo test -p
//! pemu-milestones --test cross_host`.
//!
//! The snapshot is compared by the SHA-256 of its bytes: every section is a canonical postcard
//! encoding free of host order, pointers and clocks, so restoring this host's own bytes is the
//! same as restoring the other host's.
//!
//! Golden keys, per timing profile (`fast` and `device`): `snapshot`, `end` and `after-snapshot`
//! (an [`Observation`](pemu_machine::determinism::Observation) line each), `snapshot-sha256`,
//! `trace` (the canonical MMIO and IRQ trace), `tone` (`audio_capture::analyze` with the exact
//! bits of its f64 results), `wav-sha256` and `png-sha256`.

mod asm;
mod guest;

use std::fmt::Write as _;
use std::path::PathBuf;

use pemu_core::input::{ButtonId, InputEvent, SerialChan};
use pemu_core::snap::{SnapOpts, Snapshot};
use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_machine::Machine;
use pemu_machine::config::{Assets, MachineConfig, TimingProfileId, TraceCfg};
use pemu_machine::determinism::{Since, hex, report, report_since};
use pemu_machine::machine::At;
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{StopReason, StopSet};

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../golden/cross-host/parity.txt"
);

/// Set to `1` on macOS arm64 to rewrite [`GOLDEN`].
const RECORD_ENV: &str = "PEMU_PARITY_RECORD";

/// The only leg that records the golden.
const RECORDING_LEG: &str = "macos-aarch64";

const MAGIC: &str = "#!pemu-parity v1";

/// Instructions either stretch may take before the scenario counts as lost: several times what
/// the `device` profile needs.
const MAX_INSNS: u64 = 40_000_000;

/// When the input is stamped, after the snapshot instant: past the six I2S periods left (96 ms),
/// so the guest waits for it and the poll fast-forward has a wait to skip.
const INPUT_AFTER_PS: u64 = 150_000_000_000;

/// The bytes journaled onto the USB Serial/JTAG OUT endpoint.
const INPUT: &[u8] = b"xhost";

/// This host's leg name, as `xtask ci` names receipt legs (`macos-aarch64`, `windows-x86_64`).
fn leg() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&pemu_loader::sha256(bytes))
}

fn assets(flash: &[u8]) -> Assets {
    let flash = FlashImage::from_merged(flash).expect("the parity image is a merged image");
    Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned")
}

fn config(profile: TimingProfileId) -> MachineConfig {
    MachineConfig {
        profile,
        trace: TraceCfg::all(),
        ..MachineConfig::default()
    }
}

fn to(pc: u32) -> RunLimits {
    RunLimits {
        until: None,
        max_insns: Some(MAX_INSNS),
        stops: StopSet {
            breakpoints: vec![pc],
            ..StopSet::default()
        },
    }
}

fn console(m: &mut Machine) -> String {
    let ring = m.io().serial_ring(pemu_core::hostio::SerialStream::Uart0Tx);
    let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[track_caller]
fn expect_stop(m: &mut Machine, reason: &StopReason, pc: u32, what: &str) {
    assert_eq!(
        *reason,
        StopReason::Breakpoint(pc),
        "the parity guest did not reach {what} at {pc:#x}; UART0 so far:\n{}",
        console(m)
    );
}

/// The entries of one profile, after the host-independent checks that the scenario did what it
/// claims.
fn scenario(
    test: &str,
    name: &str,
    profile: TimingProfileId,
    g: &guest::Guest,
) -> Vec<(String, String)> {
    let flash = &g.flash;
    let mut m = Machine::new(config(profile), assets(flash)).expect("composes");
    let out = m.run(to(g.snap));
    expect_stop(&mut m, &out.reason, g.snap, "the snapshot instant");
    let at = VTime(m.now().0 + INPUT_AFTER_PS);
    m.input(
        At::Vt(at),
        InputEvent::SerialIn {
            chan: SerialChan::USJ,
            data: INPUT.to_vec(),
        },
    )
    .expect("a future instant");
    m.input(
        At::Vt(at),
        InputEvent::Button {
            id: ButtonId::Ok,
            down: true,
        },
    )
    .expect("a future instant");
    let snapshot = report(&out.reason, &m);
    let since = Since::now(&m);
    let bytes = m
        .snapshot(SnapOpts::default())
        .to_bytes()
        .expect("the snapshot encodes");

    let end = m.run(to(g.end));
    expect_stop(&mut m, &end.reason, g.end, "the final `j .`");
    let end_line = report(&end.reason, &m);
    let after = report_since(&end.reason, &m, since);

    // Restore-and-continue on this host, from the bytes the golden pins.
    let mut r = Machine::new(config(profile), assets(flash)).expect("composes");
    r.restore(&Snapshot::from_bytes(&bytes).expect("the snapshot decodes"))
        .expect("the snapshot restores into a fresh machine");
    let own = Since::now(&r);
    let rend = r.run(to(g.end));
    expect_stop(
        &mut r,
        &rend.reason,
        g.end,
        "the final `j .` after the restore",
    );
    assert_eq!(
        report_since(&rend.reason, &r, own),
        after,
        "{name}: the restored run's output after the snapshot differs from the straight run's"
    );

    // What the scenario did, checked on every host so no entry compares nothing.
    let text = console(&mut m);
    let want_input = format!("D3 in {} adc ", INPUT.len());
    assert!(
        text.contains("D3 10 ") && text.contains(&want_input),
        "{name}: the guest did not finish or did not read the input; UART0:\n{text}"
    );
    let io = m.io();
    let frames = io.frame.generation();
    let frames_after = frames - since.frame;
    let pcm = &io.audio_out;
    let mut capture = pemu_api::commands::audio_capture::Capture::starting_at(0);
    capture.drain(pcm);
    let pcm_after = pcm.head() - since.pcm;
    let (fs, channels) = capture.format().expect("one PCM format");
    let left = pemu_api::commands::audio_capture::channel_of(&capture.samples, channels, 0);
    let tone = pemu_api::commands::audio_capture::analyze(&left, fs);
    assert!(
        frames >= 2 && frames_after >= 1,
        "{name}: {frames} frames, {frames_after} after the snapshot"
    );
    assert!(
        pcm_after > 0 && capture.dropped == 0 && tone.fundamental_hz.is_some(),
        "{name}: {} samples, {pcm_after} after the snapshot, {} dropped, tone {:?}",
        capture.samples.len(),
        capture.dropped,
        tone
    );
    let wav = pemu_api::commands::audio_capture::wav_bytes(fs, channels, &capture.samples);
    let png = pemu_host::png::encode_rgb565(
        io.frame.width() as u32,
        io.frame.height() as u32,
        io.frame.pixels(),
        1,
    )
    .expect("the frame encodes");
    println!(
        "RAN {test} {name}: {} instructions ({} fast-forwarded), {frames} frames ({frames_after} \
         after the snapshot), {} PCM samples at {fs} Hz x {channels} ({pcm_after} after), \
         snapshot {} bytes",
        end_line_insns(&end_line),
        out.ff_insns + end.ff_insns,
        capture.samples.len(),
        bytes.len()
    );

    vec![
        (format!("{name}.snapshot"), snapshot),
        (format!("{name}.snapshot-sha256"), sha256_hex(&bytes)),
        (format!("{name}.end"), end_line),
        (format!("{name}.after-snapshot"), after),
        (format!("{name}.trace"), hex(&m.trace_digest())),
        // The JSON `audio capture` prints rounds to hundredths, so the exact bits go beside it.
        (
            format!("{name}.tone"),
            format!(
                "{} rms_bits={:016x} fundamental_bits={:016x}",
                tone.to_json(),
                tone.rms.to_bits(),
                tone.fundamental_hz.map_or(0, f64::to_bits)
            ),
        ),
        (format!("{name}.wav-sha256"), sha256_hex(&wav)),
        (format!("{name}.png-sha256"), sha256_hex(&png)),
    ]
}

fn end_line_insns(line: &str) -> &str {
    line.split(' ')
        .find_map(|f| f.strip_prefix("insns="))
        .unwrap_or("?")
}

fn entries(test: &str) -> Vec<(String, String)> {
    // The guest calls the ROM's `ets_printf`, whose address comes from the bundled ROM ELF the
    // synthetic eFuse selects; the image, and so `image-sha256`, carries it.
    let ets_printf =
        Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
            .expect("the bundled ROM is pinned")
            .rom
            .symbols()
            .addr_of("ets_printf")
            .expect("the bundled ROM ELF names ets_printf");
    let g = guest::build(ets_printf);
    let mut out = vec![("image-sha256".to_string(), sha256_hex(&g.flash))];
    out.extend(scenario(test, "fast", TimingProfileId::Fast, &g));
    out.extend(scenario(test, "device", TimingProfileId::Device, &g));
    out
}

fn golden_path() -> PathBuf {
    PathBuf::from(GOLDEN)
}

fn render(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    writeln!(out, "{MAGIC}").unwrap();
    writeln!(out, "#!recorded-on: {RECORDING_LEG}").unwrap();
    writeln!(
        out,
        "#!command: {RECORD_ENV}=1 cargo test -p pemu-milestones --test cross_host"
    )
    .unwrap();
    writeln!(
        out,
        "#!what: tests/milestones/cross_host/main.rs; digests only, the eFuse is synthetic"
    )
    .unwrap();
    for (key, value) in entries {
        writeln!(out, "{key} = {value}").unwrap();
    }
    out
}

fn parse(text: &str) -> Vec<(String, String)> {
    assert!(
        !text.contains('\r'),
        "{GOLDEN} holds CR bytes: .gitattributes checks text out LF on every host, so this \
         checkout bypassed it"
    );
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some(MAGIC), "{GOLDEN}: not a parity golden");
    lines
        .filter(|l| !l.starts_with("#!"))
        .map(|l| {
            let (k, v) = l
                .split_once(" = ")
                .unwrap_or_else(|| panic!("{GOLDEN}: malformed line {l:?}"));
            (k.to_string(), v.to_string())
        })
        .collect()
}

/// The fields of two observation lines that differ, by name (`state`, `usj`, `pcm`, ...).
fn differing_fields(a: &str, b: &str) -> Vec<String> {
    let fields = |s: &str| -> Vec<(String, String)> {
        s.split(' ')
            .filter_map(|f| f.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let (fa, fb) = (fields(a), fields(b));
    fa.iter()
        .filter(|(k, v)| fb.iter().any(|(kb, vb)| kb == k && vb != v))
        .map(|(k, _)| k.clone())
        .collect()
}

#[test]
fn t0_cross_host_parity_equals_the_committed_macos_golden() {
    let test = "t0_cross_host_parity_equals_the_committed_macos_golden";
    let leg = leg();
    let actual = entries(test);

    if std::env::var(RECORD_ENV).as_deref() == Ok("1") {
        assert_eq!(
            leg, RECORDING_LEG,
            "the parity golden is produced by the macOS leg only"
        );
        let path = golden_path();
        std::fs::create_dir_all(path.parent().unwrap()).expect("tests/golden/cross-host");
        std::fs::write(&path, render(&actual)).expect("the golden is writable");
        println!(
            "RAN {test} record: wrote {} entries to {GOLDEN}",
            actual.len()
        );
        return;
    }

    let text = std::fs::read_to_string(golden_path()).unwrap_or_else(|e| {
        panic!("{GOLDEN}: {e}; record it on {RECORDING_LEG} with {RECORD_ENV}=1")
    });
    let golden = parse(&text);
    let keys = |v: &[(String, String)]| v.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    assert_eq!(
        keys(&golden),
        keys(&actual),
        "{GOLDEN} names other entries than this test computes: re-record it on {RECORDING_LEG}"
    );
    let mut report = String::new();
    for ((key, want), (_, got)) in golden.iter().zip(&actual) {
        if want != got {
            let fields = differing_fields(want, got);
            writeln!(
                report,
                "{key}: differs on {leg}{}\n  golden ({RECORDING_LEG}): {want}\n  this host:            {got}",
                if fields.is_empty() {
                    String::new()
                } else {
                    format!(" in {}", fields.join(", "))
                }
            )
            .unwrap();
        }
    }
    assert!(
        report.is_empty(),
        "cross-host parity fails on {leg}. Every difference is a defect to \
         fix at its source, never a golden to loosen; if behavior changed on purpose, re-record \
         on {RECORDING_LEG}.\n{report}"
    );
    // `pemu_api::receipt::HOST_PARITY` names the recording leg; a leg passes here before its first
    // receipt earns it a place in the constant.
    let claimed: Vec<&str> = pemu_api::receipt::HOST_PARITY.split(',').collect();
    assert!(
        claimed.contains(&RECORDING_LEG),
        "HOST_PARITY {:?} must name {RECORDING_LEG}, the leg that records the golden",
        pemu_api::receipt::HOST_PARITY
    );
    println!(
        "RAN {test} {leg}: {} entries equal the golden recorded on {RECORDING_LEG}, the snapshot \
         bytes included; the restored runs continued to the same observation; receipts {} this \
         leg (host_parity {})",
        golden.len(),
        if claimed.contains(&leg.as_str()) {
            "name"
        } else {
            "do not yet name"
        },
        pemu_api::receipt::HOST_PARITY
    );
}
