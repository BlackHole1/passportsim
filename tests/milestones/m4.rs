//! Milestone M4 tests: the display, SPI2, GDMA TX and LEDC. Names use the prefix `t<tier>_m4_`
//! so `xtask ci` can count them.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;

use std::path::PathBuf;
use std::sync::Arc;

use pemu_board::st7789::{
    BOOT_SEQUENCE, FrameView, PANEL_HEIGHT, PANEL_WIDTH, PanelConfig, St7789p3, cmd,
};
use pemu_core::fidelity::FidelityLedger;
use pemu_core::hostio::SerialStream;
use pemu_core::regstore::Size;
use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::elf::ElfInfo;
use pemu_machine::Executor;
use pemu_machine::config::{Assets, MachineConfig};
use pemu_machine::determinism::{self, Variant};
use pemu_machine::hle::{HleFeatureStatus as FeatureStatus, HleTripKind as TripKind};
use pemu_machine::machine::Machine;
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};
use pemu_soc_c3::periph::gdma::{DmaMem, Engine, PERI_SPI2, layout};
use pemu_soc_c3::periph::spi2::{
    CMD_USR, DMA_CONF_TX_ENA, MISC_CS_KEEP_ACTIVE, Master, USER_USR_MOSI,
};
use pemu_soc_c3::wiring::spi2;

/// The `pk` line the boot console test ends on, after its `I (<ms>) ` prefix: dev:L65.
const LVGL_READY: &str = "bsp_lvgl: LVGL 就绪";

/// Lines of the derived device golden claimed: dev:L4 to dev:L65.
const PK_BOOT_LINES: usize = 62;

/// Instructions `pk` gets from power-on to [`LVGL_READY`]: well above what it uses (the `RAN` line
/// prints it), so a regression fails on the missing line rather than on a hang.
const LVGL_INSNS: u64 = 400_000_000;

/// Virtual time the device-like scenario runs before the line reset: how long the device ran
/// before esptool reset it into the captured boot.
const DEVICE_LIKE_PS: u64 = 300_000_000_000;

const BOOT_LINE: MatcherId = MatcherId(0x41);

/// The normalized last boot of `pk` equals dev:L4 to dev:L65 of the derived device golden: the
/// M3 boot lines plus the two `bsp_disp` lines, `LVGL: Starting LVGL task` and `bsp_lvgl: LVGL 就绪`.
///
/// The boot is the device-like `rst:0x15` one: power on, 300 ms virtual, the esptool line reset
/// `UsbLine {rts: 1, dtr: 0}`. The run uses [`m4_config`]; LVGL is ready long before BLE init
/// (dev:L68). The golden is derived on this host and never committed.
#[test]
fn t1_m4_pk_boot_console() {
    let test = "t1_m4_pk_boot_console";
    let id = test.to_string();
    let Some(mut m) = corpus_machine(test, common::PK) else {
        return;
    };
    let Some(golden) = common::derived_golden_or_skip(test, "pk.console.txt") else {
        return;
    };

    let out = m.run(RunLimits {
        until: Some(VTime(DEVICE_LIKE_PS)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{id}: the first boot ends before 300 ms at pc {:#010x}",
        m.hart().pc
    );
    m.input(
        pemu_machine::machine::At::Now,
        pemu_core::input::InputEvent::UsbLine {
            dtr: false,
            rts: true,
        },
    )
    .expect("now is not in the past");

    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(LVGL_INSNS),
        stops: StopSet {
            matchers: vec![(
                BOOT_LINE,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Contains(LVGL_READY.into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let bytes = console_bytes(&mut m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: the reset boot never printed `{LVGL_READY}`; tail:\n{}",
        tail(&String::from_utf8_lossy(&bytes))
    );
    let compared =
        common::assert_console_prefix("pk.console.txt", &golden, &bytes, Some(PK_BOOT_LINES));
    assert_eq!(compared, PK_BOOT_LINES, "{id}");
    println!(
        "RAN {test} pk-boot-device: {compared} lines (dev:L4-L65), {} instructions, LVGL ready at \
         {} ms",
        out.insns,
        m.now().0 / 1_000_000_000
    );
}

/// The ST7789 command trace of the `pk` and `official` boots (commands, parameters and the
/// inter-command delays in virtual ms) equals `specs/st7789-boot.toml`, rows 1 to 23.
///
/// The bytes take the whole machine path (SPI2 `CMD.usr`, the GDMA TX walk, D/C from `GPIO_OUT`
/// bit 20, `BoardPorts::spi2`). `delay_ms_after` is the driver's `vTaskDelay`, which wakes within
/// one 1 ms tick of n ms. A row with no delay is followed within 10 ms, UNVERIFIED as a bound: row
/// 21 (DISPON) is followed 2.6 ms later on both images by the driver's next call.
#[test]
fn t1_m4_st7789_boot_trace() {
    let test = "t1_m4_st7789_boot_trace";
    let id = test.to_string();
    for image in [common::PK, common::OFFICIAL] {
        let Some(mut m) = corpus_machine(test, image) else {
            return;
        };
        let out = m.run(RunLimits {
            until: Some(VTime::from_ms(1_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        let lcd = &m.board().lcd;
        assert!(
            lcd.boot_mismatch().is_none(),
            "{id} {image}: {} (run ended {:?})",
            lcd.boot_mismatch()
                .map(|b| b.to_string())
                .unwrap_or_default(),
            out.reason
        );
        let trace = lcd.trace();
        // Each step's gap reads the command after it, so the trace must hold one more command than the
        // sequence; `boot_mismatch` alone does not promise that.
        assert!(
            trace.len() > BOOT_SEQUENCE.len(),
            "{id} {image}: the panel saw {} commands, the boot sequence has {} and a first \
             command after it (run ended {:?})",
            trace.len(),
            BOOT_SEQUENCE.len(),
            out.reason
        );
        let tick = VTime::from_ms(1).0;
        for (i, step) in BOOT_SEQUENCE.iter().enumerate() {
            let gap = trace[i + 1].at.0 - trace[i].at.0;
            let want = VTime::from_ms(u64::from(step.delay_ms_after)).0;
            if step.delay_ms_after > 0 {
                assert!(
                    gap + tick > want && gap < want + tick,
                    "{id} {image}: step {} {} waits {} ps, the driver delays {} ms",
                    step.index,
                    step.name,
                    gap,
                    step.delay_ms_after
                );
            } else {
                assert!(
                    gap < VTime::from_ms(10).0,
                    "{id} {image}: step {} {} has no driver delay, and the next command came \
                     {gap} ps later",
                    step.index,
                    step.name
                );
            }
        }
    }
}

/// The `pk` first-screen `raw` frame after `bsp_lvgl` ready equals
/// `tests/golden/pk/first-screen.png`, a golden a person approves once.
///
/// `pk` prints `bsp_lvgl: LVGL 就绪` at 230 ms and stops at the BLE tripwire at 334.6 ms, so the
/// first screen is the frame the panel holds at that stop, the same on both executors. The PNG is
/// compared byte for byte. With none committed the test prints a `SKIP` line naming the candidate
/// under the data root and its SHA-256, and writes it only with `PEMU_WRITE_CANDIDATES=1`.
#[test]
fn t1_m4_first_screen_frame() {
    let test = "t1_m4_first_screen_frame";
    let id = test.to_string();
    let mut frames = Vec::new();
    for executor in [Executor::Engine, Executor::Reference] {
        let Some(mut m) = corpus_machine(test, common::PK) else {
            return;
        };
        m.set_executor(executor);
        let out = m.run(RunLimits {
            until: Some(VTime::from_ms(1_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert!(
            matches!(out.reason, StopReason::Tripwire(_)),
            "{id}: {:?}",
            out.reason
        );
        assert!(
            console(&mut m).contains("bsp_lvgl: LVGL 就绪"),
            "{id}: LVGL never became ready"
        );
        let raw = m.board().lcd.frame(FrameView::Raw);
        assert_eq!(
            m.io().frame.pixels(),
            &raw[..],
            "{id}: FramePort carries the panel memory"
        );
        assert!(
            m.io().frame.generation() > 0,
            "{id}: no frame was presented"
        );
        frames.push(raw);
    }
    assert_eq!(
        frames[0], frames[1],
        "{id}: the engine and the reference differ"
    );
    let raw = &frames[0];
    assert!(
        raw.iter().any(|p| *p != raw[0]),
        "{id}: the first screen is one flat colour"
    );
    golden_frame(test, &id, common::PK, "first-screen", raw);
}

/// With the `ble` feature disabled, `pk` stops with `E_TRIPWIRE` at the `esp_bt_controller_init`
/// entry, naming the disabled feature. With the default configuration the ble module binds and the
/// run continues past BLE init.
///
/// "No assert" is no `abort` or `__assert_func` observe hook firing and no `assert failed` line.
/// "No hang" is a tripwire stop within 1 s when disabled, and a run to its 2 s budget by default,
/// never `Stuck`, `Deadlock` or a fault.
#[test]
fn t1_m4_ble_tripwire() {
    let test = "t1_m4_ble_tripwire";
    let id = test.to_string();
    let Some(mut m) = corpus_machine(test, common::PK) else {
        return;
    };
    let Some(elf) = m.assets().app_elf.clone() else {
        panic!("{id}: the pk machine is built with its app ELF");
    };
    let entry = elf
        .symbols
        .addr_of("esp_bt_controller_init")
        .expect("pk links the BLE controller init");
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(1_000)),
        max_insns: None,
        stops: StopSet::default(),
    });
    let text = console(&mut m);
    let StopReason::Tripwire(report) = &out.reason else {
        panic!(
            "{id}: pk ended {:?} at pc {:#010x}, console tail:\n{}",
            out.reason,
            m.hart().pc,
            tail(&text)
        );
    };
    assert_eq!(report.kind, TripKind::DisabledFeature, "{id}: {report:?}");
    assert_eq!(report.feature, Some("ble"), "{id}: {report:?}");
    assert_eq!((report.pc, m.hart().pc), (entry, entry), "{id}: {report:?}");
    assert!(report.detail.contains("esp_bt_controller_init"), "{id}");
    assert_eq!(
        m.hle_binding().record.features.get("ble"),
        Some(&FeatureStatus::Disabled)
    );
    let state = m.hle_state();
    assert_eq!(
        state.observed[1..3],
        [0, 0],
        "{id}: abort or __assert_func ran"
    );
    assert!(!text.contains("assert failed"), "{id}: {}", tail(&text));
    assert!(
        text.contains("bsp_lvgl: LVGL 就绪"),
        "{id}: the boot did not reach LVGL"
    );
    assert!(!text.contains("BLE_INIT"), "{id}: {}", tail(&text));
    // A second run stops at the same instant: a tripwire is never run past.
    let again = m.run(RunLimits::insns(1_000));
    assert_eq!(again.reason, out.reason, "{id}");
    assert_eq!(again.insns, 0, "{id}");
    println!("RAN {test} ble-disabled: E_TRIPWIRE ble at esp_bt_controller_init {entry:#010x}");

    let Some(mut m) = corpus_machine_with(test, common::PK, MachineConfig::default()) else {
        return;
    };
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(2_000)),
        max_insns: None,
        stops: StopSet::default(),
    });
    let text = console(&mut m);
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{id}: by default pk runs to its budget, ended at pc {:#010x}, console tail:\n{}",
        m.hart().pc,
        tail(&text)
    );
    assert_eq!(
        m.hle_binding().record.features.get("ble"),
        Some(&FeatureStatus::Bound),
        "{id}: the ble module binds by default"
    );
    assert_eq!(
        m.hle_state().observed[1..3],
        [0, 0],
        "{id}: abort or __assert_func ran with ble bound"
    );
    assert!(!text.contains("assert failed"), "{id}: {}", tail(&text));
    assert!(
        text.contains("BLE_INIT: BT controller compile version"),
        "{id}: the run did not continue past BLE init: {}",
        tail(&text)
    );
    println!("RAN {test} ble-default: ble bound, BLE_INIT printed, stopped at the 2 s budget");
}

/// A machine over corpus image `id` (its app ELF when it has one), the bundled ROM and the
/// synthesized eFuse, or `None` after a printed skip.
fn corpus_machine(test: &str, id: &str) -> Option<Machine> {
    corpus_machine_with(test, id, m4_config())
}

/// [`corpus_machine`] under an explicit configuration.
fn corpus_machine_with(test: &str, id: &str, cfg: MachineConfig) -> Option<Machine> {
    let image = if id == common::GOLDMINER {
        "goldminer-sanitized-8MB.bin"
    } else {
        "FoloToy-AI-Passport-8MB.bin"
    };
    let path = common::corpus_file_or_skip(test, id, image)?;
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses");
    let elf = if id == common::GOLDMINER {
        None
    } else {
        let path = common::corpus_file_or_skip(test, id, "FoloToy-AI-Passport.elf")?;
        let bytes = std::fs::read(&path).expect("the verified ELF is readable");
        Some(Arc::new(
            ElfInfo::parse(&bytes).expect("the pinned ELF parses"),
        ))
    };
    let assets = Assets::with_bundled_rom(flash, elf, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    Some(Machine::new(cfg, assets).expect("the image fits"))
}

/// The M4 configuration: the BLE module disabled. The first-screen and tripwire tests are defined
/// at the BLE tripwire stop, and the BLE HLE binds by default.
fn m4_config() -> MachineConfig {
    let mut cfg = MachineConfig::default();
    cfg.hle.disabled = vec!["ble".to_string()];
    cfg
}

/// The `pk` boot to `bsp_lvgl: LVGL 就绪` leaves no `Wiring::Spi2Transfer` and no
/// `Wiring::I2sPeriod` unapplied, and draws through the panel.
#[test]
fn t1_m4_pk_boot_to_lvgl_ready_applies_every_spi2_transfer() {
    use pemu_machine::stops::{LinePattern, Matcher, MatcherId};
    let test = "t1_m4_pk_boot_to_lvgl_ready_applies_every_spi2_transfer";
    let Some(mut m) = corpus_machine(test, common::PK) else {
        return;
    };
    let ready = MatcherId(0x4C);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(1_000)),
        max_insns: None,
        stops: StopSet {
            matchers: vec![(
                ready,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Contains("bsp_lvgl: LVGL 就绪".into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let text = console(&mut m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(ready),
        "{test}:\n{}",
        tail(&text)
    );
    let unapplied = m.unapplied_wiring_by_kind();
    assert_eq!(unapplied.spi2_transfer, 0, "{test}: {unapplied:?}");
    assert_eq!(unapplied.i2s_period, 0, "{test}: {unapplied:?}");
    assert!(m.applied_wiring_by_kind().spi2_transfer > 0, "{test}");
    assert!(
        m.board().lcd.ramwr_count() > 0,
        "{test}: no pixel reached the panel"
    );
    assert_eq!(m.dma_faults(), 0, "{test}: a DMA byte was refused");
}

fn console(m: &mut Machine) -> String {
    String::from_utf8_lossy(&console_bytes(m)).into_owned()
}

/// The whole USJ console of `m` as bytes, which is what a golden is compared against.
fn console_bytes(m: &mut Machine) -> Vec<u8> {
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    ring.slices(ring.tail()).iter().copied().collect()
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(8)..].join("\n")
}

/// Compares `raw` with `tests/golden/<id>/<name>.png`; without a committed golden the test prints
/// a `SKIP` line.
fn golden_frame(test: &str, exit: &str, id: &str, name: &str, raw: &[u16]) {
    let png =
        pemu_host::png::encode_rgb565(u32::from(PANEL_WIDTH), u32::from(PANEL_HEIGHT), raw, 1)
            .expect("a panel frame encodes");
    let committed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../golden")
        .join(id)
        .join(format!("{name}.png"));
    if let Ok(golden) = std::fs::read(&committed) {
        assert!(
            golden == png,
            "{exit}: the {id} {name} frame differs from its golden"
        );
        return;
    }
    let Some(image) = common::corpus_file_or_skip(test, id, "FoloToy-AI-Passport-8MB.bin") else {
        return;
    };
    // `<data root>/corpus/<id>/<file>`, so the data root is three levels up.
    let root = image
        .ancestors()
        .nth(3)
        .expect("a corpus file sits under the data root");
    let candidate = format!("scratch/candidates/{id}/{name}.png");
    let written = if std::env::var("PEMU_WRITE_CANDIDATES").as_deref() == Ok("1") {
        let path = root.join(&candidate);
        std::fs::create_dir_all(path.parent().expect("a file has a parent"))
            .expect("the data root is writable");
        std::fs::write(&path, &png).expect("the candidate is written");
        "written"
    } else {
        "set PEMU_WRITE_CANDIDATES=1 to write it"
    };
    common::skip(
        test,
        &format!(
            "{exit} golden tests/golden/{id}/{name}.png is not committed and needs a person's \
             approval; candidate {candidate} under the data root ({written}), sha256 {}",
            pemu_testkit::corpus::sha256_hex(&png)
        ),
    );
}

/// Trace records a fast-forward parity leg keeps, so the folded poll runs of the whole span (about
/// 340 thousand records) are counted, not only the default 4,096. Both legs use the same window.
const PARITY_TRACE_RECORDS: usize = 1_000_000;

/// What one leg of the fast-forward parity test produced.
struct Leg {
    /// [`determinism::report`]: stop reason, `state_hash`, instruction count, virtual time, consoles,
    /// frame and PCM.
    report: String,
    trace: [u8; 32],
    /// Instructions the fast-forward credited without executing them.
    ff_insns: u64,
    polls: u64,
    repeats: u64,
    /// Repeats of the longest folded run, which a confirmation is measured against.
    longest: u64,
    /// Folded runs that reached `poll_ff::CONFIRM_REPEATS`, so the tracker could confirm them.
    confirmed: u64,
    /// Whether the replay window held the whole trace; if not, the counts above are over its tail and
    /// `longest` is a lower bound.
    whole: bool,
}

impl Leg {
    /// The measured counts, which both the `RAN` and the `SKIP` line carry.
    fn counts(&self) -> String {
        let window = if self.whole {
            String::new()
        } else {
            format!(
                ", measured over the last {PARITY_TRACE_RECORDS} trace records because the replay \
                 window did not hold the whole span"
            )
        };
        format!(
            "{} confirmed of {} folded poll runs, longest {} repeats, {} repeats in total, \
             fast-forward saved {} instructions{window}",
            self.confirmed, self.polls, self.longest, self.repeats, self.ff_insns
        )
    }
}

/// One leg: `pk` from power-on to [`LVGL_READY`] with poll fast-forward `poll_ff`, the canonical
/// trace on and the ROM delay shortcut off, or `None` after the corpus skip line.
fn to_lvgl_ready(test: &str, poll_ff: bool) -> Option<Leg> {
    use pemu_core::trace::TraceEvent;

    // Under `device`, SPI2, SPI1 and I2C0 complete by event, so the guest's poll loops run long
    // enough to be confirmed.
    let variant = Variant {
        poll_ff,
        trace: true,
        rom_delay_ff: false,
        profile: pemu_machine::config::TimingProfileId::Device,
        ..Variant::default()
    };
    let mut cfg = variant.config(m4_config());
    cfg.trace.recent = PARITY_TRACE_RECORDS;
    let mut m = corpus_machine_with(test, common::PK, cfg)?;
    variant.apply(&mut m);
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(LVGL_INSNS),
        stops: StopSet {
            matchers: vec![(
                BOOT_LINE,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Contains(LVGL_READY.into()),
                },
            )],
            ..StopSet::default()
        },
    });
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{test}: poll_ff={poll_ff} never printed `{LVGL_READY}`; tail:\n{}",
        tail(&console(&mut m))
    );
    let mut leg = Leg {
        report: determinism::report(&out.reason, &m),
        trace: m.trace_digest(),
        ff_insns: out.ff_insns,
        polls: 0,
        repeats: 0,
        longest: 0,
        confirmed: 0,
        // The window holds the whole stream while no record has fallen out of it.
        whole: m.trace().tail() == 0,
    };
    // The run being folded when the stop fired is not in the window yet, so it is counted here.
    let pending = m.trace().pending().into_iter();
    for rec in m.trace().records().chain(pending) {
        if let TraceEvent::PollRun { count, .. } = rec.ev {
            leg.polls += 1;
            leg.repeats += count;
            leg.longest = leg.longest.max(count);
            if count >= pemu_machine::poll_ff::CONFIRM_REPEATS {
                leg.confirmed += 1;
            }
        }
    }
    Some(leg)
}

/// Poll fast-forward on and off give equal canonical traces from power-on to
/// `bsp_lvgl: LVGL 就绪`, and the instructions the fast-forward saved are recorded.
///
/// The legs run with the whole canonical trace on and the ROM delay shortcut off, so equal digests
/// show no iteration was lost or merged. They run under `device`: under `fast` every block
/// completes inside its access, no loop polls long enough to be confirmed and the comparison is
/// vacuous. The test claims `RAN` once a poll chain is confirmed and otherwise SKIPs; the
/// comparison runs either way as the regression guard.
#[test]
fn t1_m4_poll_fast_forward_parity() {
    let test = "t1_m4_poll_fast_forward_parity";
    let id = test.to_string();
    let Some(on) = to_lvgl_ready(test, true) else {
        return;
    };
    let Some(off) = to_lvgl_ready(test, false) else {
        return;
    };
    assert_eq!(
        on.report, off.report,
        "{id}: the run differs with poll fast-forward on and off"
    );
    assert_eq!(
        determinism::hex(&on.trace),
        determinism::hex(&off.trace),
        "{id}: the canonical trace differs with poll fast-forward on and off"
    );
    // Consecutive identical tracked reads fold into one `PollRun` record with the fast-forward on or
    // off, so the folding is part of what the legs must agree on; this names it when it broke.
    assert_eq!(
        (on.polls, on.repeats, on.longest),
        (off.polls, off.repeats, off.longest),
        "{id}: the folded poll runs differ: on {}, off {}",
        on.counts(),
        off.counts()
    );
    assert_eq!(
        off.ff_insns, 0,
        "{id}: the off leg fast-forwarded instructions with both shortcuts off"
    );
    if on.confirmed > 0 {
        println!(
            "RAN {test} pk to LVGL ready: the canonical traces are equal with poll fast-forward \
             on and off ({}, digest {})",
            on.counts(),
            determinism::hex(&on.trace)
        );
        return;
    }
    common::skip(
        test,
        &format!(
            "{id} the comparison runs and holds but is vacuous: no poll chain to `{LVGL_READY}` \
             reaches the {} repeats a confirmation needs ({}), although the legs run under the \
             `device` profile, whose clocked SPI2, SPI1 and I2C0 made the boot poll; find what \
             stopped the models waiting",
            pemu_machine::poll_ff::CONFIRM_REPEATS,
            on.counts()
        ),
    );
}

/// Length of one demand window: 100 ms of virtual time, in picoseconds.
const DEMAND_WINDOW_PS: u64 = 100_000_000_000;

/// The ESP32-C3 retires at most one instruction per cycle at 160 MHz, so no window's guest demand
/// can exceed 160 MIPS. A measurement above it is a broken clock, not a busy guest.
const CLOCK_MIPS: f64 = 160.0;

/// Guest demand of one 100 ms virtual window: instructions the guest executed in it, with the
/// fast-forward credit removed, over the window's own virtual span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Demand {
    busy_insns: u64,
    span_ps: u64,
}

impl Demand {
    fn mips(self) -> f64 {
        self.busy_insns as f64 / (self.span_ps as f64 * 1e-12) / 1e6
    }
}

/// Cuts a `pk` boot into [`DEMAND_WINDOW_PS`] windows and returns the demand of each, up to and
/// including the window in which `marker` is printed. This is the window runner of `xtask bench`,
/// which `pemu-milestones` cannot depend on; the numbers are guest-side and so identical.
fn boot_demand(m: &mut Machine, marker: &str, budget_ms: u64) -> Option<Vec<Demand>> {
    use pemu_machine::stops::{LinePattern, Matcher, MatcherId};

    let ready = MatcherId(0xE46);
    let stops = StopSet {
        matchers: vec![(
            ready,
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern: LinePattern::Contains(marker.into()),
            },
        )],
        ..StopSet::default()
    };
    let mut out = Vec::new();
    while m.now() < VTime::from_ms(budget_ms) {
        let start = m.now();
        let run = m.run(RunLimits {
            until: Some(VTime(start.0 + DEMAND_WINDOW_PS)),
            max_insns: None,
            stops: stops.clone(),
        });
        out.push(Demand {
            busy_insns: run.insns - run.ff_insns,
            span_ps: run.vt.0 - start.0,
        });
        match run.reason {
            StopReason::Matcher(id) if id == ready => return Some(out),
            StopReason::Until => {}
            _ => return None,
        }
    }
    None
}

/// Nearest-rank percentile `p` of `values`, the definition `xtask bench` uses.
fn percentile(values: &[f64], p: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Records the guest demand per 100 ms window of the `pk` boot to LVGL ready (p95 and maximum).
///
/// Demand carries no host fingerprint. The test asserts that the boot reaches LVGL ready, every
/// window did work, none exceeds what the modeled clock retires, and a second boot gives the same
/// numbers. `xtask bench --workload pk-lvgl` records the same boot into the bench history.
#[test]
fn t1_m4_guest_demand_window() {
    let test = "t1_m4_guest_demand_window";
    let id = test.to_string();
    let marker = "bsp_lvgl: LVGL 就绪";
    // LVGL is ready well before BLE init either way, so the demand does not depend on the radio.
    let Some(mut m) = corpus_machine(test, common::PK) else {
        return;
    };
    let windows = boot_demand(&mut m, marker, 1_000)
        .unwrap_or_else(|| panic!("{id}: `pk` did not print {marker:?} within 1 s virtual"));
    assert!(
        !windows.is_empty() && windows.iter().all(|w| w.busy_insns > 0 && w.span_ps > 0),
        "{id}: an empty or idle-only window is not a demand measurement: {windows:?}"
    );
    let mips: Vec<f64> = windows.iter().map(|w| w.mips()).collect();
    let max = mips.iter().copied().fold(0.0, f64::max);
    assert!(
        max <= CLOCK_MIPS,
        "{id}: a window demands {max:.2} MIPS, past the {CLOCK_MIPS} MIPS the modeled clock \
         retires"
    );

    // A recording nothing can reproduce is not a recording: the same image boots to the same
    // per-window demand.
    let Some(mut again) = corpus_machine(test, common::PK) else {
        return;
    };
    let repeat = boot_demand(&mut again, marker, 1_000).expect("the second boot reaches LVGL");
    assert_eq!(
        windows, repeat,
        "{id}: two boots of one image gave different guest demand"
    );

    let virtual_ms = windows.iter().map(|w| w.span_ps).sum::<u64>() as f64 * 1e-9;
    let per_window: Vec<String> = mips.iter().map(|v| format!("{v:.2}")).collect();
    println!(
        "RAN {test} pk-to-lvgl-ready: {} windows of 100 ms over {virtual_ms:.1} ms virtual; \
         guest demand per window {} MIPS; p95 {:.2}, max {max:.2}, mean {:.2}",
        windows.len(),
        per_window.join(" "),
        percentile(&mips, 95.0),
        windows.iter().map(|w| w.busy_insns).sum::<u64>() as f64 / (virtual_ms * 1e-3) / 1e6,
    );
}

/// `limits-panel`: CASET and RASET beyond column 239 or row 319 clip exactly as the controller
/// does, and no write lands outside the 240 by 320 frame. The bytes take the real path (GDMA TX,
/// SPI2 `CMD.usr`, the `Wiring::Spi2Transfer` join); no corpus is needed.
#[test]
fn t0_m4_limits_panel() {
    let id = "t0_m4_limits_panel".to_string();
    let mut bus = Bus::new();
    let mut panel = St7789p3::new(PanelConfig {
        invon_shows_ram: true,
    });
    panel.set_powered(true);

    // Enough of the boot sequence that panel memory takes 16-bit big-endian pixels.
    bus.command(&mut panel, cmd::SLPOUT, &[]);
    bus.command(&mut panel, cmd::COLMOD, &[0x55]);
    bus.command(&mut panel, cmd::RAMCTRL, &[0x00, 0xF0]);
    bus.command(&mut panel, cmd::DISPON, &[]);

    // A window whose end coordinates run past the panel: columns 200 to 300, rows 310 to 400.
    bus.command(&mut panel, cmd::CASET, &[0x00, 0xC8, 0x01, 0x2C]);
    bus.command(&mut panel, cmd::RASET, &[0x01, 0x36, 0x01, 0x90]);
    assert_eq!(
        panel.window(),
        (200, PANEL_WIDTH - 1, 310, PANEL_HEIGHT - 1),
        "{id}: CASET and RASET clip to the last column and row, they do not wrap"
    );

    // Fill the clipped window exactly: 40 columns by 10 rows.
    let window_pixels = usize::from(PANEL_WIDTH - 200) * usize::from(PANEL_HEIGHT - 310);
    assert_eq!(window_pixels, 400);
    let fill: Vec<u8> = (0..window_pixels)
        .flat_map(|i| [(0x40 + (i >> 8)) as u8, i as u8])
        .collect();
    bus.command(&mut panel, cmd::RAMWR, &fill);

    assert_eq!(
        panel.raw().len(),
        usize::from(PANEL_WIDTH) * usize::from(PANEL_HEIGHT),
        "{id}: panel memory is exactly the 240 by 320 frame"
    );
    assert_eq!(
        panel.pixel(FrameView::Raw, 200, 310),
        0x4000,
        "{id}: the window starts at the clipped origin"
    );
    assert_eq!(
        panel.pixel(FrameView::Raw, PANEL_WIDTH - 1, PANEL_HEIGHT - 1),
        0x4100 | 0x8F,
        "{id}: the last pixel of the clipped window is the last pixel of the frame"
    );
    assert_eq!(
        panel.pixel(FrameView::Raw, 199, 310),
        0,
        "{id}: nothing was written left of the window"
    );
    assert_eq!(
        panel.pixel(FrameView::Raw, 200, 309),
        0,
        "{id}: nothing was written above the window"
    );

    // One more pixel wraps back to the window origin rather than past the frame.
    bus.command(&mut panel, cmd::RAMWRC, &[0xFF, 0xFF]);
    assert_eq!(
        panel.pixel(FrameView::Raw, 200, 310),
        0xFFFF,
        "{id}: the pixel pointer wraps to the window, never outside the frame"
    );

    // Every pixel of the frame outside the clipped window is still untouched.
    let outside = (0..PANEL_HEIGHT)
        .flat_map(|y| (0..PANEL_WIDTH).map(move |x| (x, y)))
        .filter(|(x, y)| !(200..PANEL_WIDTH).contains(x) || !(310..PANEL_HEIGHT).contains(y))
        .filter(|(x, y)| panel.pixel(FrameView::Raw, *x, *y) != 0)
        .count();
    assert_eq!(outside, 0, "{id}: no write landed outside the window");
}

/// The SPI2 master, its GDMA TX channel and the DRAM they share: one `esp_lcd` transaction per
/// call, through the real `Wiring::Spi2Transfer` join.
struct Bus {
    spi2: Master,
    gdma: Engine,
    ram: Ram,
    ledger: FidelityLedger,
    now: VTime,
}

/// Base of the internal DRAM the descriptors and buffers live in (IDF
/// `soc/esp32c3/include/soc/soc.h`).
const DRAM: u32 = 0x3FC8_0000;
const DESC: u32 = DRAM;
const BUF: u32 = DRAM + 0x1000;

impl Bus {
    fn new() -> Bus {
        let mut bus = Bus {
            spi2: Master::default(),
            gdma: Engine::default(),
            ram: Ram(vec![0; 0x8000]),
            ledger: FidelityLedger::default(),
            now: VTime(0),
        };
        let out_peri_sel = reg(layout(0).out_peri_sel);
        bus.gdma
            .store(out_peri_sel, Size::B4, PERI_SPI2, bus.now, &mut bus.ledger);
        bus
    }

    /// One `esp_lcd` command: the command byte with D/C low and CS kept, then the parameters
    /// with D/C high and CS released.
    fn command(&mut self, panel: &mut St7789p3, command: u8, params: &[u8]) {
        self.transaction(panel, false, &[command], !params.is_empty());
        if !params.is_empty() {
            self.transaction(panel, true, params, false);
        }
    }

    /// One SPI2 user transaction, from staging the descriptor to handing the bytes to the panel.
    fn transaction(&mut self, panel: &mut St7789p3, dc: bool, bytes: &[u8], cs_keep: bool) {
        self.now = VTime(self.now.0 + 1_000_000);
        let len = bytes.len() as u32;
        self.ram.desc(DESC, len, true, BUF, 0);
        self.ram.write(BUF, bytes);

        let lay = layout(0);
        self.gdma.store(
            reg(lay.out_link),
            Size::B4,
            (DESC & 0x000F_FFFF) | (1 << 21),
            self.now,
            &mut self.ledger,
        );

        // IDF spi_hal_setup_trans, then spi_hal_user_start.
        let mut w = |off, val| {
            self.spi2
                .store(off, Size::B4, val, self.now, &mut self.ledger)
        };
        w(0x1C, len * 8 - 1);
        w(0x10, USER_USR_MOSI);
        w(0x30, DMA_CONF_TX_ENA);
        w(0x20, if cs_keep { MISC_CS_KEEP_ACTIVE } else { 0 });
        w(0x00, CMD_USR);

        let gpio_out = u32::from(dc) << spi2::DC_GPIO;
        let applied = spi2::collect(&mut self.spi2, &mut self.gdma, gpio_out, &mut self.ram);
        let transfer = applied.transfer.expect("the transaction carried bytes");
        assert_eq!(transfer.bytes, bytes, "the descriptor walk lost bytes");
        panel.transfer(self.now, transfer.dc, &transfer.bytes, transfer.cs_release);
    }
}

fn reg(i: usize) -> u32 {
    u32::from(pemu_soc_c3::r#gen::regs_gdma::REGS[i].off)
}

struct Ram(Vec<u8>);

impl Ram {
    /// Writes one `dma_descriptor_t` the engine owns (IDF `hal/include/hal/dma_types.h`).
    fn desc(&mut self, at: u32, length: u32, suc_eof: bool, buffer: u32, next: u32) {
        let w0 = length | (length << 12) | (u32::from(suc_eof) << 30) | (1 << 31);
        for (i, word) in [w0, buffer, next].into_iter().enumerate() {
            self.write(at + 4 * i as u32, &word.to_le_bytes());
        }
    }
}

impl DmaMem for Ram {
    fn read(&mut self, addr: u32, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            let at = addr.wrapping_add(i as u32).wrapping_sub(DRAM) as usize;
            *b = self.0.get(at).copied().unwrap_or(0);
        }
    }

    fn write(&mut self, addr: u32, data: &[u8]) {
        for (i, b) in data.iter().enumerate() {
            let at = addr.wrapping_add(i as u32).wrapping_sub(DRAM) as usize;
            if let Some(slot) = self.0.get_mut(at) {
                *slot = *b;
            }
        }
    }
}
