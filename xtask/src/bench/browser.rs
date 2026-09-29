//! `cargo xtask bench-browser`: the browser performance floors (the Chrome Worker and Safari / JSC
//! targets), measured in one job per host: Chrome and JavaScriptCore on macOS, Chrome alone on
//! Windows ([`Browser::defaults`]).
//!
//! | Check | Gate | Measured by | Judged here from |
//! |---|---|---|---|
//! | K ratio | K fw-Og median of 7 >= 0.90 x the spike `blockx` median, same job, same host | `web/tests/benchBrowser/worker.js` `k`: both wasm modules in one Worker, runs alternated | the medians of the two series |
//! | busy-MIPS floor | F5 and F6 busy MIPS >= the absolute browser floor of this host | `benchBrowser/worker.js` `suite` over the production core's ABI | [`Metrics::from_phases`] over the windows, fastest run |
//! | machine cost (gate) | the *machine's* F3 host cost paced at 1x <= 7 % of a core, and c <= 0.05 | the F3 windows: counted busy instructions, the same run's S, its idle seconds and its c | the cost model on those numbers ([`MachineCost`]) |
//! | browser paced share (recorded) | the *whole browser's* paced share, against this host's band | the product page paced at 1x, `ps` around it (`web/tests/processCpu.ts`) | CPU seconds over wall seconds, against [`BROWSER_PACED_BANDS`] |
//! | model check | `bench --check-model` for the browser F3 rows | the F3 windows | [`model_report`] on S and c |
//!
//! The browser only measures; every number is derived here with the functions the native suites
//! use, so a browser figure and a native one cannot differ by their arithmetic. Everything
//! measured is built in the same job ([`measure`]); `--from-records DIR` re-judges an earlier
//! run's records and measures nothing. Each suite runs once on the native machine first, and a
//! browser run whose guest ledger differs is refused ([`Ledger`]).
//!
//! S, c and the machine cost follow the contention rule of `xtask bench` ([`contention`]). The K
//! ratio (two halves alternated in one Worker) and the whole-browser paced share (CPU time, a
//! recorded band) are read at any load and print it. On Windows, `--kernel` takes a fw-Og image built elsewhere
//! (the ESP toolchain is optional there), the paced share reads `Win32_Process` instead of `ps`,
//! and records carry busy logical processors instead of a load average ([`load_word`]), so the
//! contention rule cannot fire there.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use super::cli::{Options, git_dirty, git_head, now_unix, suite_key};
use super::history::{Cores, Host, Record, contention, cores, load_history, save_history};
use super::metrics::{Metrics, Window};
use super::model::{
    Engine, F3, F3_ROWS, G1_F3, Measured, Mode, ModelInputs, WINDOW_MS, f3_recorded, model_report,
    native_measurement,
};
use super::run::{BOOT_SLACK_WINDOWS, Subject, corpus_image, run_scenario};
use super::suites::{
    CALIBRATION_PS, CLICK_MS, Clicks, OFFICIAL, Plan, SUITES, Scenario, suite_runnable,
};
use crate::bench_k;
use crate::hostdirs;

/// The QoS class a browser record carries: the browser sets its Worker thread's class itself, and
/// nothing here can set or read it.
const BROWSER_QOS: &str = "set-by-browser";

/// The workloads the row measures: F3 for c and the model check, F5 and F6 for the floors.
const SUITE_IDS: [&str; 3] = [F3, "F5", "F6"];

/// Runs of each suite per engine; the fastest is the measurement, as in `xtask bench`.
const DEFAULT_REPEAT: usize = 5;

/// Wall length of the paced reading: F3 is 60 s of virtual time, and at 1x that is 60 s of wall.
const DEFAULT_PACED_MS: u64 = 60_000;

/// Wall time the paced page runs after its menu line before the reading starts, so the reading is
/// of the settled menu F3 names and not of the boot's tail.
const PACED_SETTLE_MS: u64 = 2_000;

/// Virtual ms per slice of the emulator-alone diagnostic: the web page's longest slice.
const BARE_SLICE_MS: u64 = 8;

/// The emulator-alone legs of the paced-cost attribution, `(id, slice ms, spin)`, each one
/// wall minute at 1x: `2ms`, `8ms` and `100ms` (one F-suite window) fit the cost per `pemu_run`
/// call and per instruction; `8ms-spin` never yields the core, so only its counts are read, and
/// faster retirement there than in `8ms` means the paced cost is the host's answer to idleness.
const BARE_LEGS: [(&str, u64, bool); 4] = [
    ("2ms", 2, false),
    ("8ms", BARE_SLICE_MS, false),
    ("100ms", WINDOW_MS, false),
    ("8ms-spin", BARE_SLICE_MS, true),
];

/// F3 host CPU paced at 1x, at most 7 % of a core, in both browsers. It is the *machine's* cost,
/// judged on [`MachineCost`], not on what `ps` charges the browser's
/// process tree (that is recorded against [`BROWSER_PACED_BANDS`]).
const MACHINE_SHARE_MAX: f64 = 0.07;

/// Idle cost c during F3 at `Max`, at most 0.05, in both browsers.
const BROWSER_C_MAX: f64 = 0.05;

/// How far the paced page's virtual time may drift from its wall time before the reading is not
/// "paced at 1x". The page re-anchors at 250 ms of lag, a quarter of a percent of a
/// minute, so 5 % is wide enough for any honest pacing and far too narrow for a page that fell
/// behind or ran ahead.
const PACED_RATE_BAND: f64 = 0.05;

/// One browser of the row, as Playwright names its project (`web/tests/browsers.ts`) and
/// `--check-model` its engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Browser {
    /// Playwright Chromium: the Chrome column on macOS.
    Chromium,
    /// Playwright WebKit: the Safari / JSC column on macOS.
    Webkit,
    /// The installed Google Chrome through Playwright's `chrome` channel: the Chrome column on
    /// Windows, the one engine gated there.
    WindowsChrome,
}

impl Browser {
    fn project(self) -> &'static str {
        match self {
            Browser::Chromium => "chromium",
            Browser::Webkit => "webkit",
            Browser::WindowsChrome => "windows-chrome",
        }
    }

    /// The target column: Chrome Worker, and Safari / JSC (measured in Playwright WebKit, whose
    /// engine is JavaScriptCore; `web/tests/benchBrowser.spec.ts` says why not Safari itself).
    fn engine(self) -> Engine {
        match self {
            Browser::Chromium | Browser::WindowsChrome => Engine::Chrome,
            Browser::Webkit => Engine::Jsc,
        }
    }

    /// The host OS the browser's Playwright row belongs to, `None` for every host.
    fn host(self) -> Option<&'static str> {
        match self {
            Browser::Chromium => None,
            Browser::Webkit => Some("macos"),
            Browser::WindowsChrome => Some("windows"),
        }
    }

    /// The browsers gated on host `os`: Chrome and JSC on macOS, Chrome alone on
    /// Windows, where the Chrome column is the installed Chrome.
    fn defaults(os: &str) -> Vec<Browser> {
        if os == "windows" {
            vec![Browser::WindowsChrome]
        } else {
            vec![Browser::Chromium, Browser::Webkit]
        }
    }

    fn parse(name: &str) -> Result<Browser, String> {
        match name {
            "chromium" | "chrome" => Ok(Browser::Chromium),
            "webkit" | "jsc" => Ok(Browser::Webkit),
            "windows-chrome" => Ok(Browser::WindowsChrome),
            other => Err(format!(
                "unknown browser `{other}`; they are chromium, webkit and windows-chrome"
            )),
        }
    }
}

/// An absolute busy-MIPS floor of one workload in one browser on one host.
///
/// Floors are per host and never compared across hosts: a host whose fingerprint has no row is
/// measured and recorded, and its floor part says it has none rather than borrowing another's.
#[derive(Clone, Copy, Debug)]
struct Floor {
    os: &'static str,
    arch: &'static str,
    cpu: &'static str,
    engine: Engine,
    workload: &'static str,
    busy_mips: f64,
}

/// The browser busy-MIPS floors, each [`FLOOR_MARGIN`] of the median of three
/// uncontended `bench-browser` runs, each the fastest of five, rounded down to a whole MIPS.
///
/// The harness compiles the core once and hands every Worker the module
/// (`web/tests/benchBrowser.spec.ts` `inWorker`); a per-Worker compilation took 8 to 16 % off S.
/// The Windows floors come from that harness (2.6 to 4.4 of 20 logical processors busy, power
/// throttling off, `web/tests/processCpu.ts` `holdFullSpeed`). **The macOS floors predate it**, so
/// they let a regression of that size through: a floor is never set from a loaded run, and too few
/// new runs were quiet enough to reset them.
const BROWSER_FLOORS: [Floor; 6] = [
    // Chromium 148.0.7778.96: F5 188.90, 195.16, 196.87; F6 183.02, 192.15, 196.00.
    m3_pro(Engine::Chrome, "F5", 165.0),
    m3_pro(Engine::Chrome, "F6", 163.0),
    // Playwright WebKit 26.4: F5 182.98, 187.65, 188.40; F6 182.79, 183.93, 188.78.
    m3_pro(Engine::Jsc, "F5", 159.0),
    m3_pro(Engine::Jsc, "F6", 156.0),
    // Google Chrome 153.0.8010.54 (`windows-chrome`): F5 119.28, 119.75, 119.56; F6 119.51,
    // 119.99, 119.25. Both floors are only just above the 100 MIPS the worst F3 window needs at
    // c = 0.05, because this host's S is.
    windows_i7(Engine::Chrome, "F5", 101.0),
    windows_i7(Engine::Chrome, "F6", 101.0),
];

/// Share of the measured median a floor keeps. The three quiet runs spread 4 % and one run at a
/// load average of 15 to 19 came in 6 % under them, so 15 % keeps a floor out of host noise while
/// still catching a regression half again the size of the 10 % trend gate.
const FLOOR_MARGIN: f64 = 0.85;

/// A floor of the macOS arm64 Apple M3 Pro host.
const fn m3_pro(engine: Engine, workload: &'static str, busy_mips: f64) -> Floor {
    Floor {
        os: "macos",
        arch: "aarch64",
        cpu: "Apple M3 Pro",
        engine,
        workload,
        busy_mips,
    }
}

/// A floor of the Windows 11 i7-12700F host, which Windows names by its
/// `PROCESSOR_IDENTIFIER`, the fingerprint `xtask bench` keys on there).
const fn windows_i7(engine: Engine, workload: &'static str, busy_mips: f64) -> Floor {
    Floor {
        os: "windows",
        arch: "x86_64",
        cpu: WINDOWS_I7,
        engine,
        workload,
        busy_mips,
    }
}

/// `PROCESSOR_IDENTIFIER` of the Windows floor host (Intel Core i7-12700F).
const WINDOWS_I7: &str = "Intel64 Family 6 Model 151 Stepping 2, GenuineIntel";

/// The whole-browser paced share of one engine on one host: a recorded regression band, never a
/// gate. Two thirds of it is the host's answer to a 15 %-duty thread
/// and the browser's own floor, not the machine's cost, but it is what a user's fan hears, so it
/// is watched per host fingerprint and a run outside the band is printed as a regression.
#[derive(Clone, Copy, Debug)]
struct PacedBand {
    os: &'static str,
    arch: &'static str,
    cpu: &'static str,
    engine: Engine,
    /// Median of the three quiet runs the band was set from, as a share of a core.
    median: f64,
}

/// The whole-browser paced shares of the macOS and Windows floor hosts, each the median of three
/// uncontended `bench-browser` runs.
const BROWSER_PACED_BANDS: [PacedBand; 3] = [
    // Chromium 148.0.7778.96: 70.91, 75.05, 74.03 % of a core.
    m3_pro_band(Engine::Chrome, 0.7403),
    // Playwright WebKit 26.4: 71.90, 73.30, 72.78 %.
    m3_pro_band(Engine::Jsc, 0.7278),
    // Google Chrome 153.0.8010.54 on Windows: 35.01, 33.73, 37.45 %.
    PacedBand {
        os: "windows",
        arch: "x86_64",
        cpu: WINDOWS_I7,
        engine: Engine::Chrome,
        median: 0.3501,
    },
];

/// How far from the recorded median a paced share may land and still read as the same machine
/// doing the same thing. The share is mostly how fast a core the OS hands a 15 %-duty thread: the
/// same 168.5 M instructions have read 2.8 % and 20.9 % of a core in this row's history. The quiet
/// runs behind each median spread 5.6 % (Chromium) and 1.9 % (JSC), but other quiet runs of the
/// same code also read 54.05 % and 51.55 %, 27 % and 29 % under them. 30 % holds those and still
/// flags a page shell that doubles its cost; the width is why this is a band and not a gate.
const BAND_MARGIN: f64 = 0.30;

/// A paced band of the macOS arm64 Apple M3 Pro host.
const fn m3_pro_band(engine: Engine, median: f64) -> PacedBand {
    PacedBand {
        os: "macos",
        arch: "aarch64",
        cpu: "Apple M3 Pro",
        engine,
        median,
    }
}

/// The paced band of `engine` on `host` as `(lo, hi)`, if one is set.
fn band_of(host: &Host, engine: Engine) -> Option<(f64, f64)> {
    BROWSER_PACED_BANDS
        .iter()
        .find(|b| b.os == host.os && b.arch == host.arch && b.cpu == host.cpu && b.engine == engine)
        .map(|b| {
            (
                b.median * (1.0 - BAND_MARGIN),
                b.median * (1.0 + BAND_MARGIN),
            )
        })
}

/// The emulator's own host cost of a paced F3 minute, as the cost model defines it
/// (`host s per emulated s = R / S + I x c`, `bench/model.rs`) on one run's measured numbers.
///
/// This is what the 7 % target was derived for. Every term comes from the same job's
/// F3 record, so nothing is a prediction, and nothing moves when the OS gives the paced thread a
/// slower core.
#[derive(Clone, Copy, Debug)]
struct MachineCost {
    /// Instructions the guest retired over the measured windows.
    busy_insns: u64,
    /// Busy speed S of the same run, in MIPS.
    s_mips: f64,
    /// Idle cost c of the same run, in host seconds per idle emulated second.
    c: f64,
    /// Idle emulated seconds of the same run.
    idle_s: f64,
    /// Emulated seconds the run covered. At 1x this is also its wall time.
    virtual_s: f64,
}

impl MachineCost {
    /// Host seconds the retired instructions cost at this run's S.
    fn busy_host_s(self) -> f64 {
        self.busy_insns as f64 / (self.s_mips * 1e6)
    }

    /// Host seconds the idle emulated seconds cost at this run's c.
    fn idle_host_s(self) -> f64 {
        self.idle_s * self.c
    }

    /// Host seconds per emulated second, which at 1x is the share of a core the machine costs.
    fn share(self) -> f64 {
        (self.busy_host_s() + self.idle_host_s()) / self.virtual_s
    }
}

/// The floor of `workload` in `engine` on `host`, if one is set.
fn floor_of(host: &Host, engine: Engine, workload: &str) -> Option<f64> {
    BROWSER_FLOORS
        .iter()
        .find(|f| {
            f.os == host.os
                && f.arch == host.arch
                && f.cpu == host.cpu
                && f.engine == engine
                && f.workload == workload
        })
        .map(|f| f.busy_mips)
}

const USAGE: &str = "usage: cargo run --release -p xtask -- bench-browser \
                     [--browser chromium,webkit,windows-chrome] [--repeat N] [--k-repeat N] \
                     [--paced-ms MS] [--kernel FILE] [--history PATH] [--no-record] \
                     [--gate-exits] [--json] [--from-records DIR]";

/// Command-line options.
struct BrowserOptions {
    /// The host's own ([`Browser::defaults`]) unless `--browser` names others.
    browsers: Vec<Browser>,
    /// A fw-Og kernel image built elsewhere by `bench-k`'s build, for a host without the ESP
    /// toolchain (it is optional on Windows). The spec still checks its checksum and
    /// retired count against `bench_k::WORKLOADS`, so another kernel fails the run.
    kernel: Option<PathBuf>,
    repeat: usize,
    k_repeat: usize,
    paced_ms: u64,
    history: Option<PathBuf>,
    record: bool,
    json: bool,
    /// Apply the browser gates: a miss fails the run. Without it they are printed and not enforced.
    gate_exits: bool,
    /// Judge the records in this directory instead of measuring.
    from_records: Option<PathBuf>,
}

impl BrowserOptions {
    /// Parses `args` for a run on host `os`, which refuses a browser whose row is another host's.
    fn parse(args: &[String], os: &str) -> Result<BrowserOptions, String> {
        let mut o = BrowserOptions {
            browsers: Browser::defaults(os),
            kernel: None,
            repeat: DEFAULT_REPEAT,
            k_repeat: bench_k::DEFAULT_REPEAT,
            paced_ms: DEFAULT_PACED_MS,
            history: None,
            record: true,
            json: false,
            gate_exits: false,
            from_records: None,
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = || {
                it.next()
                    .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))
            };
            let number = |v: &String| -> Result<u64, String> {
                v.parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("{arg} takes a positive number"))
            };
            match arg.as_str() {
                "--browser" => {
                    o.browsers = value()?
                        .split(',')
                        .filter(|b| !b.is_empty())
                        .map(Browser::parse)
                        .collect::<Result<_, _>>()?;
                }
                "--repeat" => o.repeat = number(value()?)? as usize,
                "--k-repeat" => o.k_repeat = number(value()?)? as usize,
                "--paced-ms" => o.paced_ms = number(value()?)?,
                "--kernel" => o.kernel = Some(PathBuf::from(value()?)),
                "--history" => o.history = Some(PathBuf::from(value()?)),
                "--no-record" => o.record = false,
                "--json" => o.json = true,
                "--gate-exits" => o.gate_exits = true,
                "--from-records" => o.from_records = Some(PathBuf::from(value()?)),
                other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
            }
        }
        if o.k_repeat.is_multiple_of(2) {
            return Err("--k-repeat must be odd, so a median exists".to_string());
        }
        if o.browsers.is_empty() {
            return Err("--browser names no browser".to_string());
        }
        if let Some(b) = o
            .browsers
            .iter()
            .find(|b| b.host().is_some_and(|h| h != os))
        {
            return Err(format!(
                "{} is a row of the {} host, and this host is {os}",
                b.project(),
                b.host().unwrap_or_default()
            ));
        }
        Ok(o)
    }
}

/// Entry point of `cargo xtask bench-browser`.
pub fn run(args: &[String]) -> Result<(), String> {
    let host = Host::of_process();
    let o = BrowserOptions::parse(args, &host.os)?;
    println!("xtask bench-browser: browser perf floors");
    println!("host: {host}");
    if cfg!(debug_assertions) {
        return Err(format!(
            "bench-browser runs each suite on the native machine as well and must be built in \
             release; run\n  cargo run --release -p xtask -- bench-browser\n{USAGE}"
        ));
    }
    let root = hostdirs::data_root_of(&hostdirs::HostDirs::from_process(), None)?;
    let loaded = match corpus_image(&root, OFFICIAL)? {
        Subject::Ready(loaded) => loaded,
        Subject::Absent(reason) => {
            println!("NOT_RUN: every browser check runs `official`, and {reason}");
            return Ok(());
        }
    };
    let bench_options = Options::parse(&[])?;
    println!();

    // The guest ledger each browser run must reproduce, from one native run per suite.
    let mut native = Vec::new();
    for id in SUITE_IDS {
        let scenario = scenario(id)?;
        let mut m = loaded.machine(&bench_options)?;
        let key = suite_key(&m, &bench_options, id, &scenario, loaded.sites);
        let run = run_scenario(&mut m, Vec::new(), &scenario, WINDOW_MS)?;
        let ledger = Ledger::of(run.boot_ps, &run.body, run.end.vt_ps);
        println!("native ledger {id}: {ledger}");
        native.push((id, ledger, key));
    }
    println!();

    let dir = match &o.from_records {
        Some(dir) => {
            println!(
                "judging the records in {} (nothing is measured)\n",
                dir.display()
            );
            dir.clone()
        }
        None => measure(&o, &root)?,
    };

    let path = match &o.history {
        Some(path) => path.clone(),
        None => root.join("bench/history.json"),
    };
    let mut history = load_history(&path)?;
    let mut stored = Vec::new();
    let mut failures = Vec::new();
    let mut measured: Vec<(Engine, Measured)> = Vec::new();
    for browser in &o.browsers {
        let file = dir.join(format!("bench-browser-{}.json", browser.project()));
        let text = std::fs::read_to_string(&file).map_err(|e| {
            format!(
                "{}: no record at {}: {e}",
                browser.project(),
                file.display()
            )
        })?;
        let record: Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not JSON: {e}", file.display()))?;
        let judged = judge(*browser, &record, &host, &native)?;
        for line in &judged.lines {
            println!("{line}");
        }
        if let Some(f3) = judged.f3 {
            measured.push((browser.engine(), f3));
        }
        failures.extend(judged.failures);
        for r in judged.stored {
            history.push(r.clone());
            stored.push(r);
        }
        println!();
    }

    // The model check of the browser F3 rows, on this run's S and c.
    let inputs = ModelInputs {
        native: native_measurement(&history, &host, None, None),
        chrome: measured
            .iter()
            .find(|(e, _)| *e == Engine::Chrome)
            .map(|(_, m)| m.clone()),
        jsc: measured
            .iter()
            .find(|(e, _)| *e == Engine::Jsc)
            .map(|(_, m)| m.clone()),
        f3_runnable: suite_runnable(F3) || f3_recorded(&history, &host),
    };
    let engines: Vec<Engine> = o.browsers.iter().map(|b| b.engine()).collect();
    let model = model_report(&F3_ROWS, G1_F3, &inputs, Mode::Gate(engines.clone()));
    println!("browser model check, --check-model for the browser F3 rows:");
    for line in &model.lines {
        println!("   {line}");
    }
    let rows: Vec<&str> = F3_ROWS
        .iter()
        .filter(|r| engines.contains(&r.engine))
        .map(|r| r.name)
        .collect();
    let model_failures: Vec<String> = model
        .failures
        .into_iter()
        .filter(|f| rows.iter().any(|r| f.starts_with(r)))
        .collect();
    if model_failures.is_empty() {
        println!("   the browser F3 rows are reachable at the measured S and c");
    }
    failures.extend(model_failures);
    println!();

    if o.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Array(stored.clone()))
                .map_err(|e| e.to_string())?
        );
    }
    if o.record && !stored.is_empty() {
        save_history(&path, &history)?;
        println!(
            "history: {} ({} records, {} added)",
            path.display(),
            history.len(),
            stored.len()
        );
    } else {
        println!("history: {} (read only)", path.display());
    }
    println!("records: {}", dir.display());
    if failures.is_empty() {
        println!("bench-browser: every part measured passed");
        return Ok(());
    }
    if o.gate_exits {
        return Err(failures.join("\n"));
    }
    for failure in &failures {
        println!("recorded, not gated (--gate-exits applies it): {failure}");
    }
    Ok(())
}

/// The scenario of suite `id` from [`SUITES`], which must run `official`: the browser boots the
/// published demo bundle, and that is `official`.
fn scenario(id: &str) -> Result<Scenario, String> {
    let suite = SUITES
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("no suite {id}"))?;
    match &suite.plan {
        Plan::Run(s) if s.image == OFFICIAL && s.ble.is_none() => Ok(*s),
        Plan::Run(_) => Err(format!(
            "{id} does not run `official` alone, which the browser boots"
        )),
        Plan::NotRun(reason) => Err(format!("{id} is NOT_RUN: {reason}")),
    }
}

/// A suite's clicks as the spec takes them: `[ms, "up" | "down" | "ok"]`.
fn clicks_json(clicks: Clicks) -> Value {
    use pemu_core::input::ButtonId;
    Value::Array(
        clicks
            .schedule()
            .into_iter()
            .map(|(ms, button)| {
                let name = match button {
                    ButtonId::Up => "up",
                    ButtonId::Down => "down",
                    ButtonId::Ok => "ok",
                };
                json!([ms, name])
            })
            .collect(),
    )
}

/// What `web/tests/benchBrowser.spec.ts` receives in `PEMU_BENCH_BROWSER_PARAMS`.
fn params(
    o: &BrowserOptions,
    dir: &Path,
    spike: &Path,
    kbench: &Path,
    kernel: &Path,
) -> Result<Value, String> {
    let k = &bench_k::WORKLOADS[0];
    let suites: Vec<Value> = SUITE_IDS
        .iter()
        .map(|id| {
            let s = scenario(id)?;
            Ok(json!({
                "id": id,
                "repeat": o.repeat,
                "scenario": {
                    "bootTo": s.boot_to,
                    "bootBudgetMs": s.boot_budget_ms,
                    "setup": clicks_json(s.setup),
                    "setupMs": s.setup_ms,
                    "clicks": clicks_json(s.clicks),
                    "bodyMs": s.body_ms,
                },
            }))
        })
        .collect::<Result<_, String>>()?;
    let f3 = scenario(F3)?;
    Ok(json!({
        "recordDir": dir,
        "spikeWasm": spike,
        "kbenchWasm": kbench,
        "kernel": kernel,
        "k": {
            "iters": k.iters,
            "slice": bench_k::DEFAULT_SLICE,
            "maxBlockInsns": 64,
            "repeat": o.k_repeat,
            "checksum": k.checksum,
            "insns": k.insns,
        },
        "suites": suites,
        "windowMs": WINDOW_MS,
        "calibrationMs": CALIBRATION_PS / 1_000_000_000,
        "bootSlackWindows": BOOT_SLACK_WINDOWS,
        "clickMs": CLICK_MS,
        "paced": {
            "bootTo": f3.boot_to,
            "settleMs": PACED_SETTLE_MS,
            "ms": o.paced_ms,
            "bareSliceMs": BARE_SLICE_MS,
            "bareLegs": BARE_LEGS
                .iter()
                .map(|(id, slice_ms, spin)| json!({"id": id, "sliceMs": slice_ms, "spin": spin}))
                .collect::<Vec<_>>(),
        },
    }))
}

/// Runs `cargo` with `args` in `dir`, failing with `what` when it does.
fn cargo(dir: &Path, args: &[&str], what: &str) -> Result<(), String> {
    println!("building {what}");
    let status = Command::new("cargo")
        .current_dir(dir)
        .args(args)
        .status()
        .map_err(|e| format!("cannot run cargo for {what}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("building {what} failed ({status})"))
    }
}

/// Builds everything in this job, runs the spec once per browser, and returns the record directory.
fn measure(o: &BrowserOptions, root: &Path) -> Result<PathBuf, String> {
    let repo = crate::util::workspace_root();
    let dir = repo
        .join("target/bench-browser")
        .join(now_unix().to_string());
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    // The kernel, with `xtask bench-k`'s build (measured in the same job), or the image `--kernel`
    // names on a host without the ESP toolchain. Either way the spec checks what it computes.
    let k = &bench_k::WORKLOADS[0];
    let image = match &o.kernel {
        Some(path) => {
            let image = std::fs::read(path)
                .map_err(|e| format!("--kernel {}: cannot read it: {e}", path.display()))?;
            println!(
                "kernel: {} ({} bytes, SHA-256 {}), built elsewhere; the run checks its checksum \
                 and retired count",
                path.display(),
                image.len(),
                pemu_loader::hex(&pemu_loader::sha256(&image))
            );
            image
        }
        None => {
            let gcc = bench_k::toolchain(bench_k::GCC).map_err(|e| {
                format!(
                    "{e}\n(a host without the ESP toolchain can pass --kernel with the fw-Og image \
                     another host built)"
                )
            })?;
            bench_k::build_kernel(&gcc, &repo, &repo.join("target/bench-kernels"), k)?
        }
    };
    let kernel = dir.join("kernel.bin");
    std::fs::write(&kernel, image)
        .map_err(|e| format!("cannot write {}: {e}", kernel.display()))?;

    let spike_dir = bench_k::spike_dir()?;
    cargo(
        &spike_dir,
        &[
            "build",
            "--release",
            "--quiet",
            "--lib",
            "--target",
            "wasm32-unknown-unknown",
        ],
        "the spike for wasm32",
    )?;
    let spike = spike_dir.join("target/wasm32-unknown-unknown/release/rv32_interp.wasm");
    let wasm = [
        "--target",
        "wasm32-unknown-unknown",
        "--profile",
        "wasm-release",
        "--quiet",
    ];
    let mut args = vec!["build", "-p", "pemu-rv32", "--example", "kbench_wasm"];
    args.extend(wasm);
    cargo(&repo, &args, "our engine's kbench_wasm")?;
    let mut args = vec!["build", "-p", "pemu-wasm", "--lib"];
    args.extend(wasm);
    cargo(&repo, &args, "the wasm core")?;
    let out = repo.join("target/wasm32-unknown-unknown/wasm-release");
    let kbench = out.join("examples/kbench_wasm.wasm");
    let core = out.join("pemu_wasm.wasm");

    let located = pemu_testkit::corpus::locate_id_at(root, OFFICIAL.id);
    let official = located
        .file(OFFICIAL.bin)
        .and_then(|f| f.path.parent().map(Path::to_path_buf))
        .ok_or_else(|| "the `official` corpus has no directory".to_string())?;

    let params = params(o, &dir, &spike, &kbench, &kernel)?;
    for browser in &o.browsers {
        println!(
            "\nrunning web/tests/benchBrowser.spec.ts in {}",
            browser.project()
        );
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .map_err(|e| format!("no free loopback port: {e}"))?;
        let project = format!("--project={}", browser.project());
        let status = Command::new("bun")
            .current_dir(repo.join("web"))
            .args([
                "run",
                "e2e",
                &project,
                "tests/benchBrowser.spec.ts",
                "--reporter=list",
            ])
            .env("PEMU_BENCH_BROWSER_PARAMS", params.to_string())
            .env("PEMU_E2E_CORE", &core)
            .env("PEMU_E2E_OFFICIAL_DIR", &official)
            .env("PEMU_E2E_REQUIRE_BROWSERS", "1")
            .env("PEMU_WEB_PORT", port.to_string())
            .status()
            .map_err(|e| format!("cannot run bun (web/, `bun run e2e`): {e}"))?;
        if !status.success() {
            return Err(format!(
                "web/tests/benchBrowser.spec.ts failed in {} ({status}); its output is above",
                browser.project()
            ));
        }
    }
    println!();
    Ok(dir)
}

/// The guest side of one suite run: exact on any host, and the same on every engine.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ledger {
    boot_ps: u64,
    windows: usize,
    busy_insns: u64,
    end_ps: u64,
}

impl Ledger {
    fn of(boot_ps: u64, body: &[Window], end_ps: u64) -> Ledger {
        Ledger {
            boot_ps,
            windows: body.len(),
            busy_insns: body.iter().map(|w| w.busy_insns).sum(),
            end_ps,
        }
    }
}

impl std::fmt::Display for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "boot at {:.3} ms virtual, {} body windows, {} busy instructions, end at {:.3} ms",
            self.boot_ps as f64 * 1e-9,
            self.windows,
            self.busy_insns,
            self.end_ps as f64 * 1e-9
        )
    }
}

/// The windows of one phase of a browser run.
fn windows(value: &Value, phase: &str) -> Result<Vec<Window>, String> {
    let list = value[phase]
        .as_array()
        .ok_or_else(|| format!("the run has no `{phase}` windows"))?;
    list.iter()
        .map(|w| {
            let n = |k: &str| {
                w[k].as_u64()
                    .ok_or_else(|| format!("a `{phase}` window has no integer `{k}`: {w}"))
            };
            Ok(Window {
                busy_insns: n("busy_insns")?,
                idle_ps: n("idle_ps")?,
                span_ps: n("span_ps")?,
                host_ns: n("host_ns")?,
                // The Worker's thread is the browser's, and nothing here can read which core
                // class it ran on (`super::cores`).
                cores: None,
            })
        })
        .collect()
}

/// A decimal string or number of the record as a `u64`.
fn big(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

/// Median of `values` (odd length by construction of `--k-repeat`).
fn median_of(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// The printed form of one paced-cost attribution leg; empty when the record has no such leg.
///
/// Every leg is one wall minute of the same firmware paced at 1x, read against the others. A page
/// leg carries what `ps` saw per browser process; an emulator-alone leg also carries the Worker's
/// own accounting (`web/tests/benchBrowser/worker.js` `runPaced`) and, when counted, the retired
/// instructions. `stopMs` is printed, not subtracted: a counted leg pays for its own counting, and
/// taking it off would make it agree with the uncounted leg for two reasons at once.
fn paced_leg_lines(label: &str, value: &Value) -> Vec<String> {
    let (Some(cpu_s), Some(wall_ms)) = (value["cpuS"].as_f64(), value["wallMs"].as_f64()) else {
        return Vec::new();
    };
    let wall_s = wall_ms / 1e3;
    let mut out = vec![format!(
        "         {label}: {:.2} % of a core",
        cpu_s / wall_s * 100.0
    )];
    if let Some(list) = value["commands"].as_array() {
        let mut split = Vec::new();
        for p in list {
            let gained = p["gainedS"].as_f64().unwrap_or(0.0);
            if gained / wall_s < 0.005 {
                continue;
            }
            let command = p["command"].as_str().unwrap_or("");
            let kind = command
                .split_whitespace()
                .find(|a| a.starts_with("--type="))
                .unwrap_or("");
            split.push(format!(
                "{} {kind} {:.2} %",
                program_name(command),
                gained / wall_s * 100.0
            ));
        }
        if !split.is_empty() {
            out.push(format!("            {}", split.join(", ")));
        }
    }
    let worker = &value["worker"];
    if let Some(busy_ms) = worker["busyMs"].as_f64() {
        let slices = worker["slices"].as_f64().unwrap_or(0.0);
        out.push(format!(
            "            {} ms {}: {:.2} % of a core inside pemu_run over {slices:.0} calls \
             ({:.3} ms each), {:.2} % in the pause",
            worker["sliceMs"].as_u64().unwrap_or(0),
            if worker["spin"].as_bool() == Some(true) {
                "slices, spinning between them (the share is meaningless, the counts are not)"
            } else {
                "slices"
            },
            busy_ms / wall_ms * 100.0,
            if slices > 0.0 { busy_ms / slices } else { 0.0 },
            worker["waitMs"].as_f64().unwrap_or(f64::NAN) / wall_ms * 100.0,
        ));
    }
    if worker["counted"].as_bool() == Some(true) {
        let busy_insns = worker["busyInsns"].as_f64().unwrap_or(f64::NAN);
        let virtual_s = worker["virtualMs"].as_f64().unwrap_or(f64::NAN) / 1e3;
        let busy_s = worker["busyMs"].as_f64().unwrap_or(f64::NAN) / 1e3;
        out.push(format!(
            "            counts: {:.0} Minsn retired over {virtual_s:.1} s virtual = {:.2} Minsn/s \
             of guest demand, run at {:.1} Minsn/s of host speed; the ff skipped {:.0} Minsn and \
             {:.1} s idle; reading the stop cost {:.2} % of a core",
            busy_insns / 1e6,
            busy_insns / virtual_s / 1e6,
            busy_insns / busy_s / 1e6,
            worker["ffInsns"].as_f64().unwrap_or(f64::NAN) / 1e6,
            worker["idlePs"].as_f64().unwrap_or(f64::NAN) / 1e12,
            worker["stopMs"].as_f64().unwrap_or(f64::NAN) / wall_ms * 100.0,
        ));
    }
    out
}

/// The program of a process's command line without its directory: the first word, or on Windows
/// the quoted path a command line starts with when the path has spaces (`"C:\Program Files\...
/// \chrome.exe" --type=renderer` gives `chrome.exe`).
fn program_name(command: &str) -> &str {
    let first = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => command.split_whitespace().next().unwrap_or(""),
    };
    first.rsplit(['/', '\\']).next().unwrap_or(first)
}

/// The word for a record's load figure (`web/tests/processCpu.ts` `hostLoad`): the load average on
/// macOS, and on Windows, which has none, the logical processors busy over the interval before it.
fn load_word(record: &Value) -> &'static str {
    if record["loadKind"] == "busy-cores" {
        "busy processors"
    } else {
        "load average"
    }
}

/// What judging one browser's record produced.
struct Judged {
    lines: Vec<String>,
    failures: Vec<String>,
    stored: Vec<Value>,
    /// S and c of F3, for the model check.
    f3: Option<Measured>,
}

/// Judges one engine's record against the browser gates.
fn judge(
    browser: Browser,
    record: &Value,
    host: &Host,
    native: &[(&str, Ledger, Value)],
) -> Result<Judged, String> {
    let engine = browser.engine();
    let name = browser.project();
    let version = record["browserVersion"].as_str().unwrap_or("unknown");
    let mut lines = vec![format!(
        "{name} {version} ({} column) on {host}",
        match engine {
            Engine::Chrome => "Chrome Worker",
            _ => "Safari / JSC",
        }
    )];
    if record["host"]["cpu"].as_str() != Some(host.cpu.as_str()) {
        return Err(format!(
            "{name}: the record was taken on `{}`, not on this host (`{}`); numbers never cross hosts",
            record["host"]["cpu"], host.cpu
        ));
    }
    // Windows: the spec switched EcoQoS off for the browser's processes before each part
    // (`web/tests/processCpu.ts` `holdFullSpeed`); a process that refused is named, because its
    // share of the work ran at whatever speed Windows gave a background process.
    if let Some(off) = record["powerThrottlingOff"].as_object() {
        let parts: Vec<String> = off
            .iter()
            .map(|(before, held)| {
                let failed = held["failed"].as_array().map_or(0, Vec::len);
                format!(
                    "{} processes before {before}{}",
                    held["processes"],
                    if failed > 0 {
                        format!(" ({failed} refused: {})", held["failed"])
                    } else {
                        String::new()
                    }
                )
            })
            .collect();
        lines.push(format!(
            "   power throttling (EcoQoS) switched off for the browser: {}",
            parts.join(", ")
        ));
    }
    let mut failures = Vec::new();
    let mut stored = Vec::new();
    let load_of = |v: &Value| v.as_array().and_then(|a| a.first()).and_then(Value::as_f64);

    // ---- The K ratio ----
    let k = &bench_k::WORKLOADS[0];
    let mips = |side: &str| -> Result<Vec<f64>, String> {
        record["k"][side]
            .as_array()
            .ok_or_else(|| format!("{name}: the record has no K `{side}` runs"))?
            .iter()
            .map(|r| {
                let insns = r["insns"].as_f64().unwrap_or(0.0);
                let secs = r["secs"].as_f64().unwrap_or(0.0);
                (secs > 0.0)
                    .then(|| insns / secs / 1e6)
                    .ok_or_else(|| format!("{name}: a K `{side}` run has no time: {r}"))
            })
            .collect()
    };
    let ours = mips("ours")?;
    let spike = mips("spike")?;
    let (ours_median, spike_median) = (median_of(ours.clone()), median_of(spike.clone()));
    let ratio = ours_median / spike_median;
    let k_load = load_of(&record["k"]["loadavgAtEnd"]);
    let fmt = |v: &[f64]| {
        let lo = v.iter().copied().fold(f64::MAX, f64::min);
        let hi = v.iter().copied().fold(0.0, f64::max);
        format!("min {lo:.1}, max {hi:.1}")
    };
    lines.push(format!(
        "   K ratio, K {} (iters {}, {} runs each, alternated in one Worker): ours median {ours_median:.1} \
         Minsn/s ({}), spike blockx {spike_median:.1} ({}), ratio {ratio:.3}: {}{}",
        k.name,
        k.iters,
        ours.len(),
        fmt(&ours),
        fmt(&spike),
        if ratio >= bench_k::GATE { "PASS" } else { "FAIL" },
        k_load.map_or(String::new(), |l| format!(" ({} {l:.1})", load_word(record))),
    ));
    if ratio < bench_k::GATE {
        failures.push(format!(
            "K ratio {name}: K {} is {ratio:.3} of the spike blockx median, below the {:.2} gate \
             (ours {ours_median:.1}, spike {spike_median:.1} Minsn/s)",
            k.name,
            bench_k::GATE
        ));
    }
    stored.push(json!({
        "workload": format!("K {}", k.name),
        "host": host.to_json(),
        "config": {"runtime": engine.name(), "browser": version, "placement": "worker",
                   "iters": k.iters, "slice": bench_k::DEFAULT_SLICE, "max_block_insns": 64},
        "metrics": {"busy_mips": ours_median, "spike_blockx_mips": spike_median, "ratio": ratio,
                    "ours": ours, "spike": spike},
        "commit": git_head(),
        "dirty": git_dirty(),
        "unix_s": now_unix(),
        "load_avg": k_load,
        "contended": contention(k_load).is_some(),
        "regressed": false,
        "accepted": false,
    }));

    // ---- The busy-MIPS floors and the F3 idle cost: the suites ----
    let mut f3 = None;
    // What the machine cost is judged on, from the same F3 record: absent when the run did not
    // resolve S and c, which its own line above says.
    let mut f3_machine: Option<MachineCost> = None;
    let mut f3_load: Option<f64> = None;
    for (id, ledger, key) in native {
        let runs = record["suites"][*id]
            .as_array()
            .ok_or_else(|| format!("{name}: the record has no {id} runs"))?;
        let mut best: Option<(Metrics, Option<f64>, f64, f64)> = None;
        for (i, run) in runs.iter().enumerate() {
            let body = windows(run, "body")?;
            let calibration = windows(run, "calibration")?;
            let got = Ledger::of(
                big(&run["bootPs"]).unwrap_or(0),
                &body,
                big(&run["end"]["vtPs"]).unwrap_or(0),
            );
            if &got != ledger {
                return Err(format!(
                    "{name} {id} run {}: the browser ran a different workload than the native \
                     machine: {got} against {ledger}",
                    i + 1
                ));
            }
            let metrics = Metrics::from_phases(&body, &calibration);
            let load = load_of(&run["loadavg"]);
            let resolution = run["resolutionMs"].as_f64().unwrap_or(f64::NAN);
            // The idle emulated seconds of this run, which the metrics do not carry and the machine
            // gate's idle term needs.
            let idle_s = body.iter().map(|w| w.idle_ps).sum::<u64>() as f64 * 1e-12;
            if best
                .as_ref()
                .is_none_or(|(m, ..)| metrics.wall_s < m.wall_s)
            {
                best = Some((metrics, load, resolution, idle_s));
            }
        }
        let Some((metrics, load, resolution, idle_s)) = best else {
            return Err(format!("{name}: {id} has no runs"));
        };
        let contended = contention(load);
        let s = metrics.busy_mips;
        let c = metrics.idle_cost;
        lines.push(format!(
            "   {id}: {ledger}; fastest of {}: S {} MIPS, c {}, wall {:.3} s, worst window {:.2} ms \
             (timer resolution {resolution:.4} ms{})",
            runs.len(),
            s.map_or("not resolvable".to_string(), |s| format!("{s:.2}")),
            c.map_or("not resolvable".to_string(), |c| format!("{c:.5}")),
            metrics.wall_s,
            metrics.worst_window_ms,
            load.map_or(String::new(), |l| format!(", {} {l:.1}", load_word(record))),
        ));
        for note in &metrics.notes {
            lines.push(format!("      note: {note}"));
        }
        let mut misses = Vec::new();
        if *id == "F5" || *id == "F6" {
            match (floor_of(host, engine, id), s) {
                (Some(floor), Some(s)) => {
                    let verdict = if s >= floor { "meets" } else { "MISSES" };
                    lines.push(format!(
                        "      busy-MIPS floor: S {s:.2} against {floor:.2} MIPS ({FLOOR_MARGIN} of the recorded median): {verdict}"
                    ));
                    if s < floor {
                        misses.push(format!(
                            "busy-MIPS floor {name}: {id} busy MIPS {s:.2} is below this host's floor of {floor:.2}"
                        ));
                    }
                }
                (Some(floor), None) => misses.push(format!(
                    "busy-MIPS floor {name}: {id} busy MIPS is not resolvable, and the floor is {floor:.2}"
                )),
                (None, _) => {
                    lines.push(format!(
                        "      busy-MIPS floor: none set for {engine} on this host fingerprint; \
                         recorded only (floors never cross hosts)",
                        engine = engine.name()
                    ));
                    failures.push(format!(
                        "busy-MIPS floor {name}: no busy-MIPS floor is set for {id} on {host}"
                    ));
                }
            }
        }
        if *id == F3 {
            match c {
                Some(c) => {
                    let verdict = if c <= BROWSER_C_MAX {
                        "meets"
                    } else {
                        "MISSES"
                    };
                    lines.push(format!(
                        "      F3 idle cost: c {c:.5} against {BROWSER_C_MAX}: {verdict}"
                    ));
                    if c > BROWSER_C_MAX {
                        misses.push(format!(
                            "idle cost {name}: F3 idle cost c {c:.5} is over {BROWSER_C_MAX}"
                        ));
                    }
                }
                None => misses.push(format!(
                    "idle cost {name}: F3 idle cost is not resolvable: {}",
                    metrics.notes.join("; ")
                )),
            }
            f3 = s.map(|s| Measured {
                s,
                c,
                source: format!("bench-browser F3 in {name} {version}"),
                stand_in: false,
            });
            f3_load = load;
            f3_machine = s.zip(c).map(|(s_mips, c)| MachineCost {
                busy_insns: metrics.busy_insns,
                s_mips,
                c,
                idle_s,
                virtual_s: metrics.virtual_s,
            });
        }
        match contended {
            Some(load) => {
                for miss in misses {
                    lines.push(format!(
                        "      NOT MEASURED, {} {load:.1} on {} cores: {miss}",
                        load_word(record),
                        cores() as u64
                    ));
                }
            }
            None => failures.extend(misses),
        }
        let mut config = key.clone();
        // The machine key stays as the native suite writes it; the runtime says where it ran,
        // which keeps a browser record out of every native baseline and native S (`is_native`).
        config["runtime"] = json!(engine.name());
        config["browser"] = json!(version);
        config["placement"] = json!("worker");
        config["build"] = json!("wasm-release");
        // The browser sets its Worker thread's class itself and reports no core class, so a
        // browser record is `unobserved`.
        let cores = Cores::of(&metrics, BROWSER_QOS);
        let rec = Record {
            workload: (*id).to_string(),
            host: host.clone(),
            config,
            metrics,
            commit: git_head(),
            dirty: git_dirty(),
            unix_s: now_unix(),
            load_avg: load,
            cores,
        };
        stored.push(rec.to_json_with(false, false));
    }

    // ---- The machine cost gate: the machine's own host cost of the paced minute ----
    let mut machine_misses = Vec::new();
    match f3_machine {
        Some(cost) => {
            let share = cost.share();
            let meets = share <= MACHINE_SHARE_MAX;
            lines.push(format!(
                "   machine cost gate, the machine paced at 1x (the cost model on this run's \
                 numbers): {} busy instructions at S {:.2} MIPS = {:.3} host s, plus {:.1} idle emulated s at c \
                 {:.5} = {:.3} host s, over {:.1} s virtual: {:.2} % of a core against {:.0} %: {}",
                cost.busy_insns,
                cost.s_mips,
                cost.busy_host_s(),
                cost.idle_s,
                cost.c,
                cost.idle_host_s(),
                cost.virtual_s,
                share * 100.0,
                MACHINE_SHARE_MAX * 100.0,
                if meets { "meets" } else { "MISSES" },
            ));
            if !meets {
                machine_misses.push(format!(
                    "machine cost {name}: the machine's F3 host cost paced at 1x is {:.2} % of a \
                     core, over the {:.0} % target (S {:.2} MIPS, c {:.5})",
                    share * 100.0,
                    MACHINE_SHARE_MAX * 100.0,
                    cost.s_mips,
                    cost.c,
                ));
            }
        }
        None => machine_misses.push(format!(
            "machine cost {name}: the machine's paced host cost is not computable, because F3 \
             resolved no S or no c (its line above says why)"
        )),
    }
    // S and c are host seconds, so the machine cost follows the rule its inputs follow.
    match contention(f3_load) {
        Some(load) => {
            for miss in machine_misses {
                lines.push(format!(
                    "      NOT MEASURED, {} {load:.1} on {} cores: {miss}",
                    load_word(record),
                    cores() as u64
                ));
            }
        }
        None => failures.extend(machine_misses),
    }

    // ---- Recorded: the whole browser's paced share against this host's band ----
    let paced = &record["paced"];
    let wall_s = paced["wallMs"].as_f64().unwrap_or(0.0) / 1e3;
    let virtual_s = paced["virtualMs"].as_f64().unwrap_or(0.0) / 1e3;
    let cpu_s = paced["cpuS"].as_f64().unwrap_or(f64::NAN);
    let share = cpu_s / wall_s;
    let rate = virtual_s / wall_s;
    let paced_load = load_of(&paced["loadavgAtStart"]);
    let band = band_of(host, engine);
    lines.push(format!(
        "   browser paced share (recorded), the whole browser paced at 1x for {wall_s:.1} s wall ({virtual_s:.1} s \
         virtual, rate {rate:.3}): {cpu_s:.2} CPU s over {} browser processes, {:.2} % of a core{}{}",
        paced["processes"],
        share * 100.0,
        match band {
            Some((lo, hi)) => format!(
                ", against this host's recorded band of {:.1} to {:.1} %: {}",
                lo * 100.0,
                hi * 100.0,
                if share >= lo && share <= hi {
                    "in band"
                } else {
                    "OUTSIDE THE BAND, a regression to look at (recorded, not gated)"
                }
            ),
            None => format!(
                ", with no band set for {} on this host fingerprint; recorded only (a band never \
                 crosses hosts)",
                engine.name()
            ),
        },
        paced_load.map_or(String::new(), |l| format!(" ({} {l:.1})", load_word(record))),
    ));
    if let Some(list) = paced["commands"].as_array() {
        for p in list {
            let gained = p["gainedS"].as_f64().unwrap_or(0.0);
            if gained > 0.0 {
                let command = p["command"].as_str().unwrap_or("");
                let kind = command
                    .split_whitespace()
                    .find(|a| a.starts_with("--type="))
                    .unwrap_or("");
                lines.push(format!(
                    "      {:>5.2} % {} {kind}",
                    gained / wall_s * 100.0,
                    program_name(command)
                ));
            }
        }
    }
    // The paced-cost attribution: diagnostics beside the gate, never judged. Each leg is
    // the same wall minute with one thing changed, so the differences are what the figure is made
    // of; a leg the spec did not record is simply absent.
    let mut legs = vec![
        (
            "the page with the Perf panel open".to_string(),
            &record["pacedLegs"]["perfOpen"],
        ),
        (
            "the page presenting nothing".to_string(),
            &record["pacedLegs"]["noDisplay"],
        ),
        (
            "the page repainting nothing".to_string(),
            &record["pacedLegs"]["noRepaint"],
        ),
        (
            "the page with the machine paused".to_string(),
            &record["pacedLegs"]["paused"],
        ),
        ("the emulator alone".to_string(), &record["pacedBare"]),
    ];
    for (id, _, _) in BARE_LEGS {
        legs.push((
            format!("the emulator alone, {id}"),
            &record["pacedBareSweep"][id],
        ));
    }
    let attribution: Vec<String> = legs
        .iter()
        .flat_map(|(label, value)| paced_leg_lines(label, value))
        .collect();
    // A record an earlier run wrote carries none of these legs (`--from-records`), and a heading
    // over nothing would read as legs that were taken and came out empty.
    if !attribution.is_empty() {
        lines.push("      attribution (diagnostics, none of it gated):".to_string());
        lines.extend(attribution);
    }
    // A reading that is not this browser's alone, or not at 1x, is not the figure the band was set
    // from, so it is refused rather than compared: the band is not a gate, but a reading that
    // measured something else is a defect in the measurement.
    if let Some(shared) = paced["shared"].as_str() {
        failures.push(format!(
            "browser paced share {name}: the paced reading is not this browser's alone: {shared}"
        ));
    } else if (rate - 1.0).abs() > PACED_RATE_BAND {
        failures.push(format!(
            "browser paced share {name}: the page did not run at 1x during the reading (rate {rate:.3}), so \
             its CPU is not the paced figure"
        ));
    } else if share.is_nan() {
        failures.push(format!(
            "browser paced share {name}: the paced reading has no CPU time, so nothing was measured"
        ));
    }
    stored.push(json!({
        "workload": "F3 paced",
        "host": host.to_json(),
        "config": {"runtime": engine.name(), "browser": version, "placement": "page", "rate": 1},
        "metrics": {"core_share": share, "cpu_s": cpu_s, "wall_s": wall_s, "virtual_s": virtual_s,
                    "machine_core_share": f3_machine.map(MachineCost::share),
                    "band_lo": band.map(|(lo, _)| lo), "band_hi": band.map(|(_, hi)| hi),
                    "in_band": band.map(|(lo, hi)| share >= lo && share <= hi)},
        "commit": git_head(),
        "dirty": git_dirty(),
        "unix_s": now_unix(),
        "load_avg": paced_load,
        "contended": contention(paced_load).is_some(),
        "regressed": false,
        "accepted": false,
    }));

    Ok(Judged {
        lines,
        failures,
        stored,
        f3,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsers_parse_by_project_and_by_column() {
        assert_eq!(Browser::parse("chromium").unwrap().engine(), Engine::Chrome);
        assert_eq!(Browser::parse("jsc").unwrap(), Browser::Webkit);
        assert_eq!(
            Browser::parse("windows-chrome").unwrap().engine(),
            Engine::Chrome
        );
        assert!(Browser::parse("safari").is_err());
        // The record a run reads is the project's (`web/tests/benchBrowser.spec.ts`).
        assert_eq!(Browser::WindowsChrome.project(), "windows-chrome");
    }

    #[test]
    fn options_refuse_an_even_k_repeat() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(BrowserOptions::parse(&args(&["--k-repeat", "6"]), "macos").is_err());
        let o = BrowserOptions::parse(
            &args(&["--browser", "webkit", "--paced-ms", "1000"]),
            "macos",
        )
        .unwrap();
        assert_eq!(o.browsers, [Browser::Webkit]);
        assert_eq!(o.paced_ms, 1000);
        assert_eq!(o.k_repeat, 7);
        assert_eq!(o.kernel, None);
        let o = BrowserOptions::parse(&args(&["--kernel", "k.bin"]), "windows").unwrap();
        assert_eq!(o.kernel, Some(PathBuf::from("k.bin")));
    }

    /// macOS gates Chrome and JSC, Windows gates Chrome alone, and that Chrome is the installed
    /// one of the Windows row; a row of the other host is refused.
    #[test]
    fn each_host_measures_its_own_rows() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mac = BrowserOptions::parse(&[], "macos").unwrap();
        assert_eq!(mac.browsers, [Browser::Chromium, Browser::Webkit]);
        let win = BrowserOptions::parse(&[], "windows").unwrap();
        assert_eq!(win.browsers, [Browser::WindowsChrome]);
        let refused = BrowserOptions::parse(&args(&["--browser", "webkit"]), "windows");
        assert!(refused.err().unwrap().contains("macos host"));
        let refused = BrowserOptions::parse(&args(&["--browser", "windows-chrome"]), "macos");
        assert!(refused.err().unwrap().contains("windows host"));
        // Playwright Chromium runs everywhere, so it can still be measured on Windows by name.
        assert!(BrowserOptions::parse(&args(&["--browser", "chromium"]), "windows").is_ok());
    }

    #[test]
    fn a_process_is_named_by_its_program_on_either_host() {
        let chrome =
            "\"C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe\" --type=renderer";
        assert_eq!(program_name(chrome), "chrome.exe");
        assert_eq!(
            program_name(
                "/Users/x/ms-playwright/chromium-1/chrome-mac/Chromium --type=gpu-process"
            ),
            "Chromium"
        );
        assert_eq!(program_name(""), "");
        assert_eq!(
            load_word(&json!({"loadKind": "busy-cores"})),
            "busy processors"
        );
        assert_eq!(load_word(&json!({})), "load average");
    }

    #[test]
    fn the_browser_suites_are_official_alone_and_carry_their_clicks() {
        for id in SUITE_IDS {
            scenario(id).unwrap();
        }
        let f6 = scenario("F6").unwrap();
        assert_eq!(
            clicks_json(f6.setup),
            json!([[0, "down"], [400, "down"], [800, "ok"], [1200, "ok"]])
        );
    }

    #[test]
    fn the_floors_are_the_margin_of_the_recorded_medians_and_never_cross_hosts() {
        let median = |mut v: [f64; 3]| {
            v.sort_by(f64::total_cmp);
            v[1]
        };
        let measured = [
            (Engine::Chrome, "F5", median([188.90, 195.16, 196.87])),
            (Engine::Chrome, "F6", median([183.02, 192.15, 196.00])),
            (Engine::Jsc, "F5", median([182.98, 187.65, 188.40])),
            (Engine::Jsc, "F6", median([182.79, 183.93, 188.78])),
        ];
        let mac = Host {
            os: "macos".into(),
            arch: "aarch64".into(),
            cpu: "Apple M3 Pro".into(),
        };
        for (engine, workload, m) in measured {
            let floor = floor_of(&mac, engine, workload).unwrap();
            assert_eq!(floor, (m * FLOOR_MARGIN).floor(), "{engine:?} {workload}");
            // The worst measured F3 window needs S >= 100 at c = 0.05.
            assert!(floor > 110.0);
        }
        let other = Host {
            cpu: "Apple M4".into(),
            ..mac
        };
        assert_eq!(floor_of(&other, Engine::Chrome, "F5"), None);
        assert_eq!(floor_of(&other, Engine::Native, "F3"), None);
        // The Windows host: Chrome alone, from its own three runs, and nothing borrowed from the
        // macOS host or given to it.
        let windows = Host {
            os: "windows".into(),
            arch: "x86_64".into(),
            cpu: WINDOWS_I7.into(),
        };
        for (workload, runs) in [
            ("F5", [119.28, 119.75, 119.56]),
            ("F6", [119.51, 119.99, 119.25]),
        ] {
            let floor = floor_of(&windows, Engine::Chrome, workload).unwrap();
            assert_eq!(floor, (median(runs) * FLOOR_MARGIN).floor(), "{workload}");
            assert!(runs.iter().all(|s| *s >= floor));
        }
        assert_eq!(floor_of(&windows, Engine::Jsc, "F5"), None);
        let mac_cpu_on_windows = Host {
            cpu: "Apple M3 Pro".into(),
            ..windows
        };
        assert_eq!(floor_of(&mac_cpu_on_windows, Engine::Chrome, "F5"), None);
    }

    /// The band is the median of three quiet runs, per host fingerprint and per engine,
    /// and it holds every quiet reading this row has recorded on this code.
    #[test]
    fn the_paced_band_is_the_margin_around_the_recorded_medians_and_never_crosses_hosts() {
        let median = |mut v: [f64; 3]| {
            v.sort_by(f64::total_cmp);
            v[1]
        };
        let mac = Host {
            os: "macos".into(),
            arch: "aarch64".into(),
            cpu: "Apple M3 Pro".into(),
        };
        // The three quiet runs behind the band, and the lowest quiet reading the
        // row has recorded on this code, which the band must hold because nothing here caused it.
        let measured = [
            (Engine::Chrome, [0.7091, 0.7505, 0.7403], 0.5405),
            (Engine::Jsc, [0.7190, 0.7330, 0.7278], 0.5155),
        ];
        for (engine, runs, quiet_low) in measured {
            let (lo, hi) = band_of(&mac, engine).unwrap();
            let m = median(runs);
            assert!((lo - m * (1.0 - BAND_MARGIN)).abs() < 1e-9, "{engine:?}");
            assert!((hi - m * (1.0 + BAND_MARGIN)).abs() < 1e-9, "{engine:?}");
            for run in runs {
                assert!(run >= lo && run <= hi, "{engine:?} {run}");
            }
            assert!(quiet_low >= lo, "{engine:?}: {quiet_low} under {lo}");
            // A page shell that doubles its cost is still outside.
            assert!(m * 2.0 > hi, "{engine:?}");
        }
        let other = Host {
            cpu: "Apple M4".into(),
            ..mac
        };
        assert_eq!(band_of(&other, Engine::Chrome), None);
        assert_eq!(band_of(&other, Engine::Native), None);
        // The Windows host's band holds its three runs and a fourth run that was not used to set
        // it (43.40 %).
        let windows = Host {
            os: "windows".into(),
            arch: "x86_64".into(),
            cpu: WINDOWS_I7.into(),
        };
        let runs = [0.3501, 0.3373, 0.3745];
        let (lo, hi) = band_of(&windows, Engine::Chrome).unwrap();
        assert!((lo - median(runs) * (1.0 - BAND_MARGIN)).abs() < 1e-9);
        for run in runs.into_iter().chain([0.4340]) {
            assert!(run >= lo && run <= hi, "{run}");
        }
        assert_eq!(band_of(&windows, Engine::Jsc), None);
    }

    /// The machine cost gate is the cost model on the run's own numbers, and it is a rate, so it is
    /// the one paced figure that does not move with the core the OS hands a sleeping thread.
    #[test]
    fn the_machine_cost_is_the_g1_model_on_the_measured_numbers() {
        // A recorded Chromium run: the F3 minute's counted instructions at that
        // run's S, and its idle emulated seconds at its c.
        let chromium = MachineCost {
            busy_insns: 168_519_671,
            s_mips: 189.95,
            c: 0.01302,
            idle_s: 58.9,
            virtual_s: 60.0,
        };
        assert!((chromium.busy_host_s() - 0.8872).abs() < 1e-3);
        assert!((chromium.idle_host_s() - 0.767).abs() < 1e-3);
        // 1.48 % of a core busy and 1.28 % idle, which is what `--check-model` predicts.
        assert!(
            (chromium.share() - 0.0276).abs() < 5e-4,
            "{}",
            chromium.share()
        );
        assert!(chromium.share() <= MACHINE_SHARE_MAX);
        // A machine half the speed at four times the idle cost is what the gate is for.
        let slow = MachineCost {
            s_mips: 95.0,
            c: 0.05,
            ..chromium
        };
        assert!(slow.share() > MACHINE_SHARE_MAX, "{}", slow.share());
    }

    #[test]
    fn a_ledger_counts_the_body_alone() {
        let w = |busy| Window {
            busy_insns: busy,
            idle_ps: 0,
            span_ps: 1,
            host_ns: 1,
            cores: None,
        };
        let l = Ledger::of(5, &[w(2), w(3)], 9);
        assert_eq!((l.windows, l.busy_insns, l.boot_ps, l.end_ps), (2, 5, 5, 9));
    }
}
