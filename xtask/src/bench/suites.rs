//! Workloads: the corpus images, the F-suite table, the `rom-boot` harness and the phase runners
//! a suite boots with.

use std::time::Instant;

use pemu_core::input::ButtonId;
use pemu_machine::stops::StopReason;

use super::cores::CoreTime;
use super::metrics::Window;
use super::model::{F3, WINDOW_PS};

/// A corpus image a suite boots: its corpus id and file names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Image {
    pub(super) id: &'static str,
    pub(super) bin: &'static str,
    pub(super) elf: &'static str,
}

/// The `official` FoloToy image, subject of F1 and F3 to F6.
pub(super) const OFFICIAL: Image = Image {
    id: "official",
    bin: "FoloToy-AI-Passport-8MB.bin",
    elf: "FoloToy-AI-Passport.elf",
};

/// The Passport Keys image, subject of F2 and F7.
pub(super) const PK: Image = Image {
    id: "pk",
    bin: "FoloToy-AI-Passport-8MB.bin",
    elf: "FoloToy-AI-Passport.elf",
};

/// Hold of one click, in virtual ms: `pemu_api::commands::input`'s own figure, so a bench click
/// is the click `passportsim input` sends.
pub(super) const CLICK_MS: u64 = pemu_api::commands::input::CLICK_MS;

/// The clicks a suite's body journals, all at instants relative to the start of the body.
#[derive(Clone, Copy, Debug)]
pub(super) enum Clicks {
    /// Each click at its own ms from the start of the body.
    At(&'static [(u64, ButtonId)]),
    /// `count` clicks of one button, the first at `first_ms` and one every `period_ms`.
    Every {
        button: ButtonId,
        count: u64,
        first_ms: u64,
        period_ms: u64,
    },
}

impl Clicks {
    /// The press and release of every click, in `(ms from the start of the body, event)` order.
    pub(super) fn schedule(self) -> Vec<(u64, ButtonId)> {
        match self {
            Clicks::At(list) => list.to_vec(),
            Clicks::Every {
                button,
                count,
                first_ms,
                period_ms,
            } => (0..count)
                .map(|i| (first_ms + i * period_ms, button))
                .collect(),
        }
    }
}

/// The Passport Keys GATT service and its two characteristics (`pk_ble.c`).
const PK_SERVICE: &str = "12D4FA08-7418-48FA-A95A-B43A2E669E55";
const PK_EVENTS: &str = "12D4FA09-7418-48FA-A95A-B43A2E669E55";
const PK_COMMANDS: &str = "12D4FA0A-7418-48FA-A95A-B43A2E669E55";

/// The BLE leg of a suite: the scripted central (`pemu_radio::ble::central`) driven from this
/// runner, which is what F7's "advertising, one connection, notify stream for 30 s" is made of.
///
/// Not command calls: each leg is a `central::Step` list of the kind `ble_scan`, `ble_connect`
/// and `ble_gatt` build, journaled as one `EnvChange::BleCentral`. The connect script goes in at
/// the start of the unmeasured setup; the body then journals one poll script every
/// [`BleLeg::poll_period_ms`], because `pk` notifies only in answer to a written command line
/// (`pk_protocol.c`).
#[derive(Clone, Copy, Debug)]
pub(super) struct BleLeg {
    /// The vendor service the central connects to.
    pub(super) service: &'static str,
    /// The characteristic it subscribes to and reads notifications from.
    pub(super) events: &'static str,
    /// The characteristic it writes command lines to.
    pub(super) commands: &'static str,
    /// Active scan length, in ms.
    pub(super) scan_ms: u32,
    /// How long the connect step waits for a connectable advertisement, in ms.
    pub(super) connect_within_ms: u32,
    /// The command line one poll writes. A `\n` is appended: `pk_line_feed` frames on it.
    pub(super) poll_line: &'static str,
    /// What the answer to `poll_line` contains.
    pub(super) poll_answer: &'static str,
    /// Virtual ms between two polls of the body.
    pub(super) poll_period_ms: u64,
    /// How long one poll waits for its answer, in ms.
    pub(super) poll_within_ms: u32,
}

/// The BLE leg of F7, named here so `Scenario` can hold a reference to it.
const F7_BLE: BleLeg = BleLeg {
    service: PK_SERVICE,
    events: PK_EVENTS,
    commands: PK_COMMANDS,
    scan_ms: 500,
    connect_within_ms: 2_000,
    // `pk` answers `{"cmd":"ping"}` with `{"t":"pong"}` on the events characteristic
    // and notifies nothing unasked, so the stream is kept running by writing one line a period.
    poll_line: "{\"cmd\":\"ping\"}",
    poll_answer: "{\"t\":\"pong\"}",
    poll_period_ms: 200,
    poll_within_ms: 2_000,
};

/// What a suite does on the machine.
#[derive(Clone, Copy, Debug)]
pub(super) struct Scenario {
    pub(super) image: Image,
    /// Console line that ends the boot phase. For F1 and F2 it is the workload's own end.
    pub(super) boot_to: &'static str,
    /// Virtual ms the boot phase may take before the suite gives up.
    pub(super) boot_budget_ms: u64,
    /// Clicks that put the machine in the state the workload names, and the virtual ms it then
    /// runs before the measured body starts. Neither is measured: F6's Audio card takes about
    /// 1.8 s of virtual time to open its codec, and a workload named "Audio tone demo for 10 s"
    /// measures the demo, not the navigation to it.
    pub(super) setup: Clicks,
    pub(super) setup_ms: u64,
    /// Clicks the body journals before it runs.
    pub(super) clicks: Clicks,
    /// Virtual ms of the body, measured from the start of the body.
    pub(super) body_ms: u64,
    /// True when the boot windows are the measured ones (F1, F2) and the body is empty.
    pub(super) boot_is_the_workload: bool,
    /// True when the body's playback is captured into the WAV artifact.
    pub(super) capture_audio: bool,
    /// The BLE leg, or `None` for a workload that drives no radio. A reference, because a
    /// `BleLeg` by value makes `Plan::Run` an order of magnitude larger than `Plan::NotRun`.
    pub(super) ble: Option<&'static BleLeg>,
}

/// One F-suite.
pub(super) struct Suite {
    pub(super) id: &'static str,
    pub(super) definition: &'static str,
    /// The gates on the suite, for the printout.
    pub(super) gates: &'static str,
    /// How the suite runs, or why it cannot on this checkout.
    pub(super) plan: Plan,
}

/// Whether a suite runs here.
pub(super) enum Plan {
    Run(Scenario),
    /// Declared and not run, with the reason the record carries. No suite is in this state; it
    /// stays so an unrunnable F-suite says so instead of vanishing from [`SUITES`]
    /// (`any_suite_runnable` and the T2 benchmark plan are written against it).
    #[allow(dead_code)]
    NotRun(&'static str),
}

impl Suite {
    pub(super) fn scenario(&self) -> Option<&Scenario> {
        match &self.plan {
            Plan::Run(s) => Some(s),
            Plan::NotRun(_) => None,
        }
    }
}

/// Whether the suite `id` runs on this checkout.
pub(super) fn suite_runnable(id: &str) -> bool {
    SUITES
        .iter()
        .any(|s| s.id == id && matches!(s.plan, Plan::Run(_)))
}

/// Whether any F-suite runs on this checkout; T2 marks its benchmark steps NOT_RUN while none
/// does (`crate::ci::tiers`).
pub fn any_suite_runnable() -> bool {
    SUITES.iter().any(|s| matches!(s.plan, Plan::Run(_)))
}

/// Where `official` has settled on its menu: the line the menu tree test waits for. The card the
/// menu selects there is Display, index 0 of the seven (`MENU_CARDS` of `tests/milestones/m5.rs`),
/// so the Audio card of F6 is two DOWN clicks away.
const OFFICIAL_MENU: &str = "main: 就绪:Display=1 Button=1 Audio=1 Battery=1";

/// F1 to F7.
pub(super) const SUITES: [Suite; 7] = [
    Suite {
        id: "F1",
        definition: "`official`: reset to `Calling app_main()`",
        gates: "M5: wall <= 0.2 s native; busy MIPS and c trend-gated",
        plan: Plan::Run(Scenario {
            image: OFFICIAL,
            boot_to: "Calling app_main()",
            boot_budget_ms: 2_000,
            setup: Clicks::At(&[]),
            setup_ms: 0,
            clicks: Clicks::At(&[]),
            body_ms: 0,
            boot_is_the_workload: true,
            capture_audio: false,
            ble: None,
        }),
    },
    Suite {
        id: "F2",
        definition: "`pk`: reset to `pk_app: ready`",
        gates: "recorded",
        plan: Plan::Run(Scenario {
            image: PK,
            boot_to: "pk_app: ready",
            boot_budget_ms: 5_000,
            setup: Clicks::At(&[]),
            setup_ms: 0,
            clicks: Clicks::At(&[]),
            body_ms: 0,
            boot_is_the_workload: true,
            capture_audio: false,
            ble: None,
        }),
    },
    Suite {
        id: F3,
        definition: "`official`: settled menu idle for 60 s virtual",
        gates: "M5: wall at Max <= 2 s with c <= 0.02; --check-model rows",
        plan: Plan::Run(Scenario {
            image: OFFICIAL,
            boot_to: OFFICIAL_MENU,
            boot_budget_ms: 2_000,
            setup: Clicks::At(&[]),
            setup_ms: 0,
            clicks: Clicks::At(&[]),
            body_ms: 60_000,
            boot_is_the_workload: false,
            capture_audio: false,
            ble: None,
        }),
    },
    Suite {
        id: "F4",
        definition: "`official`: 40 menu clicks at 400 ms",
        gates: "M5: worst 100 ms window after boot <= 30 ms native",
        plan: Plan::Run(Scenario {
            image: OFFICIAL,
            boot_to: OFFICIAL_MENU,
            boot_budget_ms: 2_000,
            setup: Clicks::At(&[]),
            setup_ms: 0,
            clicks: Clicks::Every {
                button: ButtonId::Down,
                count: 40,
                first_ms: 0,
                period_ms: 400,
            },
            // The 40th click is pressed at 15,600 ms; the body runs to the end of its period.
            body_ms: 16_000,
            boot_is_the_workload: false,
            capture_audio: false,
            ble: None,
        }),
    },
    Suite {
        id: "F5",
        definition: "`official`: Display demo for 10 s",
        gates: "M5: worst window (card entry) <= 30 ms; busy MIPS and c trend-gated",
        plan: Plan::Run(Scenario {
            image: OFFICIAL,
            // The settled menu already selects Display, so one OK enters its card and the card
            // entry is the first window of the body.
            boot_to: OFFICIAL_MENU,
            boot_budget_ms: 2_000,
            setup: Clicks::At(&[]),
            setup_ms: 0,
            clicks: Clicks::At(&[(0, ButtonId::Ok)]),
            body_ms: 10_000,
            boot_is_the_workload: false,
            capture_audio: false,
            ble: None,
        }),
    },
    Suite {
        id: "F6",
        definition: "`official`: Audio tone demo for 10 s",
        gates: "M6: worst window <= 30 ms; busy MIPS and c trend-gated; the WAV artifact",
        plan: Plan::Run(Scenario {
            image: OFFICIAL,
            boot_to: OFFICIAL_MENU,
            boot_budget_ms: 2_000,
            // DOWN, DOWN, OK opens the Audio card; its codec is open about 1.8 s later
            // (`bsp_audio: codec 打开 16000Hz/16bit/1ch`), so the tone the body plays waits for
            // the setup to finish (`main/demo_audio.c`, the I2S EOF script of m6.rs).
            setup: Clicks::At(&[
                (0, ButtonId::Down),
                (400, ButtonId::Down),
                (800, ButtonId::Ok),
                (1_200, ButtonId::Ok),
            ]),
            setup_ms: 4_000,
            // The OK that plays the 1 kHz square the body captures. The setup played one
            // already, because the demo's first `bsp_audio_set_format(16000, 16, 1)` turns the
            // BSP's stereo TX stream mono and one WAV holds one format (m6.rs).
            clicks: Clicks::At(&[(0, ButtonId::Ok)]),
            body_ms: 10_000,
            boot_is_the_workload: false,
            capture_audio: true,
            ble: None,
        }),
    },
    Suite {
        id: "F7",
        definition: "`pk`: BLE advertising, one connection, notify stream for 30 s",
        gates: "M8: the record and the BLE demand per 100 ms window; M9 browser worst window, \
                recorded. No native host-time budget is set for F7",
        plan: Plan::Run(Scenario {
            image: PK,
            // `pk` advertises once its app is up, so the boot ends where F2's does and the
            // advertising leg is running before the connect script is journaled.
            boot_to: "pk_app: ready",
            boot_budget_ms: 5_000,
            setup: Clicks::At(&[]),
            // The connect script: scan, connect to the vendor service, MTU 247, discover,
            // subscribe. It is journaled at the start of the setup, so none of it is measured;
            // a workload named "notify stream for 30 s" measures the stream, not the connection.
            setup_ms: 4_000,
            clicks: Clicks::At(&[]),
            body_ms: 30_000,
            boot_is_the_workload: false,
            capture_audio: false,
            ble: Some(&F7_BLE),
        }),
    },
];

/// Workloads outside the F table that a milestone test asks to have recorded: `pk-lvgl` is M4's
/// `pk` boot to LVGL ready, several hundred ms before F2's `pk_app: ready`. It runs here to land
/// in the history and be trend-gated; `tests/milestones/m4.rs` claims the exit on its own run.
const EXTRA_SUITES: [Suite; 1] = [Suite {
    id: PK_LVGL,
    definition: "`pk`: reset to `bsp_lvgl: LVGL 就绪` (M4; not an F-suite)",
    gates: "M4: guest demand per 100 ms window recorded",
    plan: Plan::Run(Scenario {
        image: PK,
        boot_to: "bsp_lvgl: LVGL 就绪",
        boot_budget_ms: 1_000,
        setup: Clicks::At(&[]),
        setup_ms: 0,
        clicks: Clicks::At(&[]),
        body_ms: 0,
        boot_is_the_workload: true,
        capture_audio: false,
        ble: None,
    }),
}];

/// Workload id of the M4 guest-demand recording.
pub(super) const PK_LVGL: &str = "pk-lvgl";

/// Every workload `xtask bench` runs, F-suites first.
pub(super) fn all_suites() -> impl Iterator<Item = &'static Suite> {
    SUITES.iter().chain(EXTRA_SUITES.iter())
}

/// Target metrics `xtask bench` does not measure yet, and the package each waits on.
pub(super) const METRICS_WAITING: [(&str, &str); 2] = [
    (
        "host CPU in paced mode",
        "a paced-mode run of the native loop and of the Worker pacing loop in the browser",
    ),
    (
        "snapshot size and time",
        "a snapshot workload (M3); in-memory save <= 10 ms native is its target",
    ),
];

/// The no-corpus harness workload (`bench.rs` module documentation). Not an F-suite.
pub(super) const ROM_BOOT: &str = "rom-boot";

/// Virtual length of `rom-boot` by default, in 100 ms windows.
pub(super) const DEFAULT_ROM_BOOT_WINDOWS: u64 = 10;

/// Builds the machine `rom-boot` runs: the bundled ROM a synthesized eFuse selects, erased flash,
/// the machine v0 bring-up configuration.
pub(super) fn rom_boot_machine() -> Result<pemu_machine::Machine, String> {
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_machine::config::{Assets, MachineConfig};

    let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
        .map_err(|e| format!("the bundled ROM is not pinned: {e:?}"))?;
    pemu_machine::Machine::new(MachineConfig::default(), assets).map_err(|e| e.to_string())
}

/// What makes two runs of one workload the same run: its end state. Every repeat must agree, or
/// the workload is not deterministic and its numbers are not comparable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EndState {
    pub(super) insns: u64,
    pub(super) vt_ps: u64,
    pub(super) pc: u32,
    /// Measured windows the run produced: a repeat that ends at the same instant after a
    /// different number of windows measured something else.
    pub(super) windows: usize,
}

/// Runs `machine` for `windows` windows of [`WINDOW_PS`] and times each one.
///
/// A run that stops for any reason other than reaching its window end ends the workload there
/// (`Deadlock` means nothing can wake the hart), and the partial window counts with its real
/// span.
pub(super) fn run_windows(
    m: &mut pemu_machine::Machine,
    windows: u64,
) -> Result<(Vec<Window>, EndState), String> {
    let (out, _) = run_phase(m, windows, None)?;
    let end = end_state(m, &out);
    Ok((out, end))
}

/// The end state of a run whose windows are `windows`.
pub(super) fn end_state(m: &pemu_machine::Machine, windows: &[Window]) -> EndState {
    EndState {
        insns: m.hart().insns,
        vt_ps: m.now().0,
        pc: m.hart().pc,
        windows: windows.len(),
    }
}

/// Runs `m` for at most `windows` windows of [`WINDOW_PS`], timing each one, and stops early when
/// `marker` is printed on the console ([`run_slices`]).
pub(super) fn run_phase(
    m: &mut pemu_machine::Machine,
    windows: u64,
    marker: Option<&str>,
) -> Result<(Vec<Window>, StopReason), String> {
    run_slices(m, WINDOW_PS, windows, marker)
}

/// Virtual length of one calibration slice, in picoseconds: 20 ms.
///
/// The calibration pass resolves S alone, so its slices need not be the 100 ms windows. A
/// 100 ms window of a boot that waits on I2C or a tick counts as idling however little it idled
/// (the `official` boot has 2 non-idling windows of 5, below `MIN_BUSY_WINDOWS` in `metrics.rs`).
/// Measured with `--calibration-ms` over F3 on an Apple M3 Pro:
///
/// | slice | non-idling slices | S      | c       |
/// |-------|-------------------|--------|---------|
/// | 1 ms  | 217 of 449        | 242.19 | 0.00952 |
/// | 5 ms  | 42 of 90          | 255.26 | 0.00986 |
/// | 20 ms | 10 of 23          | 265.86 | 0.00955 |
/// | 50 ms | 3 of 9            | 269.70 | 0.00955 |
/// | 100 ms| 2 of 5            | none   | none    |
///
/// S climbs as the slices lengthen, because each `Machine::run` call restarts the poll chain and
/// pays its own entry, and converges by 20 ms (1.4 % below the 50 ms figure). That residue biases S
/// **down** and so c **down**: a calibrated c is the optimistic end of its range. The margin
/// decides it: F3's 0.0095 would have to be wrong by a factor of two to reach the 0.02 of the M5
/// budget, and the table above moves it by 3 %.
pub(super) const CALIBRATION_PS: u64 = 20_000_000_000;

/// [`run_phase`] at an arbitrary slice length. The matcher is armed per slice, so a phase ends on
/// a console line and keeps its slice boundaries; with no marker nothing is armed, so the long
/// idle bodies pay no matcher cost.
pub(super) fn run_slices(
    m: &mut pemu_machine::Machine,
    slice_ps: u64,
    windows: u64,
    marker: Option<&str>,
) -> Result<(Vec<Window>, StopReason), String> {
    use pemu_core::hostio::SerialStream;
    use pemu_core::time::VTime;
    use pemu_machine::run::RunLimits;
    use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopSet};

    /// The one matcher id this module arms.
    const MARKER: MatcherId = MatcherId(0x_F32);

    let stops = match marker {
        None => StopSet::default(),
        Some(line) => StopSet {
            matchers: vec![(
                MARKER,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Contains(line.to_string()),
                },
            )],
            ..StopSet::default()
        },
    };
    let mut out = Vec::with_capacity(windows as usize);
    let mut reason = StopReason::Until;
    for _ in 0..windows {
        let start = m.now();
        // The core-class reading brackets the timed span, so its own cost (well under a
        // microsecond) is never in `host_ns`.
        let cpu = CoreTime::now();
        let host = Instant::now();
        let run = m.run(RunLimits {
            until: Some(VTime(start.0 + slice_ps)),
            max_insns: None,
            stops: stops.clone(),
        });
        let host_ns = host.elapsed().as_nanos() as u64;
        let cores = cpu
            .zip(CoreTime::now())
            .map(|(before, after)| after.since(before));
        out.push(Window {
            busy_insns: run.insns - run.ff_insns,
            idle_ps: run.idle_ps,
            span_ps: run.vt.0 - start.0,
            host_ns,
            cores,
        });
        reason = run.reason.clone();
        match reason {
            StopReason::Until => {}
            StopReason::Matcher(MARKER) | StopReason::Deadlock => break,
            ref other => return Err(format!("the run stopped for {other:?} at {:?}", run.vt)),
        }
    }
    Ok((out, reason))
}
