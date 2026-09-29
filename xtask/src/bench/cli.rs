//! Command line: the options, the measurement driver, the printed report and the history record
//! keys.

use std::path::{Path, PathBuf};

use pemu_loader::hex;
use serde_json::{Value, json};

use super::cores::{self};
use super::gates::{EXIT_GATES, ExitGate, F6_GATES, exit_gate_lines};
use super::history::{
    Cores, Host, Record, baseline_exclusions, contention, cores, load_average, load_history,
    regressions, save_history,
};
use super::metrics::Metrics;
use super::model::{Engine, WINDOW_MS, WINDOW_PS, check_model};
use super::run::{
    BOOT_SLACK_WINDOWS, BleSummary, Subject, corpus_image, repeats_done, reported_run,
    rom_fusion_sites, run_scenario,
};
use super::suites::{
    CALIBRATION_PS, CLICK_MS, DEFAULT_ROM_BOOT_WINDOWS, EndState, METRICS_WAITING, PK_LVGL, Plan,
    ROM_BOOT, Scenario, Suite, all_suites, rom_boot_machine, run_slices, run_windows,
};
use super::wav::{WavArtifact, write_wav};
use crate::hostdirs;

const USAGE: &str = "usage:\n  cargo run --release -p xtask -- bench [--workload ID,ID] \
                     [--repeat N] [--windows N] [--history PATH] [--no-record] [--accept] \
                     [--gate-exits] [--json]\n  cargo run -p xtask -- bench \
                     --check-model [--gate [ENGINE]] [--s-native MIPS [--c-native C]] \
                     [--s-chrome MIPS [--c-chrome C]] [--s-jsc MIPS [--c-jsc C]] \
                     [--history PATH]";

/// Command-line options.
pub(super) struct Options {
    pub(super) check_model: bool,
    pub(super) repeat: usize,
    windows: u64,
    history: Option<PathBuf>,
    record: bool,
    /// Store this run as accepted: it pins the baseline even if it regressed ([`regressions`]).
    pub(super) accept: bool,
    json: bool,
    pub(super) s_native: Option<f64>,
    pub(super) s_chrome: Option<f64>,
    pub(super) s_jsc: Option<f64>,
    pub(super) c_native: Option<f64>,
    pub(super) c_chrome: Option<f64>,
    pub(super) c_jsc: Option<f64>,
    /// `--check-model --gate`: [`Mode::Gate`](super::model::Mode::Gate).
    pub(super) gate: bool,
    /// Engines `--gate` applies to. Empty means every row; T2 names `native` on a host whose
    /// browser phase has not run.
    pub(super) gate_engines: Vec<Engine>,
    /// Workload ids to run; empty runs every one.
    pub(super) workloads: Vec<String>,
    /// `--gate-exits`: apply the hard native gates of the M5 and M6 perf budgets ([`EXIT_GATES`]).
    pub(super) gate_exits: bool,
    /// `--console N`: print the last N console lines of each suite, for diagnosis.
    console: usize,
    /// `--calibration-ms N`: the calibration slice length, for the sweep that sized it.
    calibration_ms: u64,
    /// `--max-block-insns N`: the block-length cap, for the sweep that decides whether to apply it.
    /// `None` keeps `EngineCfg::default`, which is what every recorded run uses.
    pub(super) max_block_insns: Option<u16>,
    /// `--poll-ff off`: disables the poll fast-forward, to measure what it is worth.
    pub(super) poll_ff: bool,
    /// `--window-ms N`: the body's slice length, a diagnostic that sizes what the windowing
    /// costs. Demand is defined on 100 ms virtual windows, so any other length is not a
    /// measurement.
    window_ms: u64,
}

impl Options {
    pub(super) fn parse(args: &[String]) -> Result<Options, String> {
        let mut o = Options {
            check_model: false,
            repeat: 25,
            windows: DEFAULT_ROM_BOOT_WINDOWS,
            history: None,
            record: true,
            accept: false,
            json: false,
            s_native: None,
            s_chrome: None,
            s_jsc: None,
            c_native: None,
            c_chrome: None,
            c_jsc: None,
            gate: false,
            gate_engines: Vec::new(),
            workloads: Vec::new(),
            gate_exits: false,
            console: 0,
            calibration_ms: CALIBRATION_PS / 1_000_000_000,
            max_block_insns: None,
            poll_ff: true,
            window_ms: WINDOW_MS,
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .ok_or_else(|| format!("{name} needs a value\n{USAGE}"))
            };
            let mips = |v: &String, name: &str| -> Result<f64, String> {
                v.parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && *s > 0.0)
                    .ok_or_else(|| format!("{name} takes a positive number of MIPS"))
            };
            let cost = |v: &String, name: &str| -> Result<f64, String> {
                v.parse::<f64>()
                    .ok()
                    .filter(|c| c.is_finite() && *c >= 0.0)
                    .ok_or_else(|| format!("{name} takes a non-negative idle cost"))
            };
            match arg.as_str() {
                "--check-model" => o.check_model = true,
                "--repeat" => {
                    o.repeat = value(arg)?.parse().map_err(|_| "--repeat takes a number")?
                }
                "--windows" => {
                    o.windows = value(arg)?
                        .parse()
                        .map_err(|_| "--windows takes a number")?
                }
                "--history" => o.history = Some(PathBuf::from(value(arg)?)),
                "--no-record" => o.record = false,
                "--accept" => o.accept = true,
                "--json" => o.json = true,
                "--s-native" => o.s_native = Some(mips(value(arg)?, arg)?),
                "--s-chrome" => o.s_chrome = Some(mips(value(arg)?, arg)?),
                "--s-jsc" => o.s_jsc = Some(mips(value(arg)?, arg)?),
                "--c-native" => o.c_native = Some(cost(value(arg)?, arg)?),
                "--c-chrome" => o.c_chrome = Some(cost(value(arg)?, arg)?),
                "--c-jsc" => o.c_jsc = Some(cost(value(arg)?, arg)?),
                "--gate" => o.gate = true,
                "--gate-engine" => o.gate_engines.push(Engine::parse(value(arg)?)?),
                "--gate-exits" => o.gate_exits = true,
                "--max-block-insns" => {
                    o.max_block_insns = Some(
                        value(arg)?
                            .parse()
                            .map_err(|_| "--max-block-insns takes a number")?,
                    )
                }
                "--poll-ff" => o.poll_ff = value(arg)? != "off",
                "--window-ms" => {
                    o.window_ms = value(arg)?
                        .parse()
                        .map_err(|_| "--window-ms takes a number of ms")?
                }
                "--calibration-ms" => {
                    o.calibration_ms = value(arg)?
                        .parse()
                        .map_err(|_| "--calibration-ms takes a number of ms")?
                }
                "--console" => {
                    o.console = value(arg)?
                        .parse()
                        .map_err(|_| "--console takes a number of lines")?
                }
                "--workload" => o.workloads.extend(
                    value(arg)?
                        .split(',')
                        .filter(|id| !id.is_empty())
                        .map(str::to_string),
                ),
                other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
            }
        }
        if o.repeat == 0 {
            return Err("--repeat must be at least 1".to_string());
        }
        let known = |id: &String| {
            all_suites().any(|s| s.id == id.as_str()) || id.as_str() == ROM_BOOT || id == "all"
        };
        if let Some(unknown) = o.workloads.iter().find(|id| !known(id)) {
            return Err(format!(
                "unknown workload `{unknown}`; the ids are F1 to F7, {PK_LVGL}, {ROM_BOOT} and `all`"
            ));
        }
        if o.workloads.iter().any(|id| id == "all") {
            o.workloads.clear();
        }
        for (c, s, name) in [
            (o.c_native, o.s_native, "--c-native"),
            (o.c_chrome, o.s_chrome, "--c-chrome"),
            (o.c_jsc, o.s_jsc, "--c-jsc"),
        ] {
            if c.is_some() && s.is_none() {
                return Err(format!("{name} needs the matching --s-* speed"));
            }
        }
        if o.gate && !o.check_model {
            return Err("--gate applies to --check-model".to_string());
        }
        if !o.gate_engines.is_empty() && !o.gate {
            return Err("--gate-engine narrows --gate".to_string());
        }
        if o.gate_exits && o.check_model {
            return Err("--gate-exits applies to the measuring run, not --check-model".to_string());
        }
        if o.accept && !o.record {
            return Err("--accept stores a record and cannot be combined with --no-record".into());
        }
        if o.windows == 0 {
            return Err("--windows must be at least 1".to_string());
        }
        Ok(o)
    }
}

/// `<data root>/bench/history.json`, or the `--history` override.
pub(super) fn history_path(o: &Options) -> Result<PathBuf, String> {
    if let Some(path) = &o.history {
        return Ok(path.clone());
    }
    let dirs = hostdirs::HostDirs::from_process();
    Ok(hostdirs::data_root_of(&dirs, None)?.join("bench/history.json"))
}

/// Entry point of `cargo xtask bench`.
pub fn run(args: &[String]) -> Result<(), String> {
    let o = Options::parse(args)?;
    if o.check_model {
        return check_model(&o);
    }
    if cfg!(debug_assertions) {
        return Err(format!(
            "bench measures the emulator and must be built in release; run\n  \
             cargo run --release -p xtask -- bench\n{USAGE}"
        ));
    }
    measure(&o)
}

/// Whether `o` asked for workload `id`.
pub(super) fn selected(o: &Options, id: &str) -> bool {
    o.workloads.is_empty() || o.workloads.iter().any(|w| w == id)
}

/// `xtask bench`: runs the selected workloads, prints their metrics, applies the 10 %
/// regression gate against this host's history and, with `--gate-exits`, the hard native gates of
/// the M5 and M6 perf budgets.
fn measure(o: &Options) -> Result<(), String> {
    let host = Host::of_process();
    // Every record is measured on this thread, at the class it asks for here, so the class a
    // record carries does not depend on the launcher (the `cores` module), and records of
    // different classes never compare.
    let qos = cores::request_interactive();
    println!("xtask bench: F-suite workloads");
    println!("host: {host}");
    println!(
        "measuring thread: QoS {qos}; core classes {}",
        cores::core_classes().map_or("not reported".to_string(), |n| n.to_string())
    );
    let path = history_path(o)?;
    let mut history = load_history(&path)?;
    let data_root = hostdirs::data_root_of(&hostdirs::HostDirs::from_process(), None);
    println!();

    let mut stored = Vec::new();
    let mut failures = Vec::new();
    let mut accepted_despite = Vec::new();
    let mut ran = 0usize;

    for suite in all_suites() {
        if !selected(o, suite.id) {
            continue;
        }
        let Some(scenario) = suite.scenario() else {
            let Plan::NotRun(reason) = &suite.plan else {
                unreachable!("a suite with no scenario is NotRun");
            };
            println!("{} {}\n   NOT_RUN: {reason}", suite.id, suite.definition);
            println!("   gates when it runs: {}\n", suite.gates);
            continue;
        };
        let root = match &data_root {
            Ok(root) => root,
            Err(e) => {
                println!(
                    "{} {}\n   NOT_RUN: no data root, so no corpus: {e}\n",
                    suite.id, suite.definition
                );
                continue;
            }
        };
        match measure_suite(o, &host, suite, scenario, root, &history)? {
            None => {}
            Some(outcome) => {
                ran += 1;
                failures.extend(outcome.failures);
                accepted_despite.extend(outcome.accepted_despite);
                history.push(outcome.stored.clone());
                stored.push(outcome.stored);
            }
        }
    }

    if selected(o, ROM_BOOT) {
        let outcome = measure_rom_boot(o, &host, &history)?;
        ran += 1;
        failures.extend(outcome.failures);
        accepted_despite.extend(outcome.accepted_despite);
        history.push(outcome.stored.clone());
        stored.push(outcome.stored);
    }

    for (metric, waits_on) in METRICS_WAITING {
        println!("metric {metric}: not measured; waits on {waits_on}");
    }
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
    if ran == 0 {
        return Err("no workload ran; nothing was measured".to_string());
    }
    for line in &accepted_despite {
        println!("accepted despite: {line}");
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

/// What one measured workload produced.
pub(super) struct Outcome {
    pub(super) stored: Value,
    pub(super) failures: Vec<String>,
    accepted_despite: Vec<String>,
}

/// Turns one reported record into its stored form, its regression verdict and its exit gates.
pub(super) fn finish(
    o: &Options,
    record: Record,
    history: &[Value],
    gates: &[ExitGate],
) -> Outcome {
    // Why this run's host time is not the emulator's on the cores the targets are for, if it is
    // not: a host too busy to measure anything (`contention`), or a run the OS spread over both
    // core clusters or kept on the efficiency one (`Cores::unmeasured`).
    let unmeasured = contention(record.load_avg)
        .map(|load| format!("load average {load:.1} on {} cores", cores() as u64))
        .or_else(|| record.cores.unmeasured());
    let found = regressions(history, &record);
    let regressed = !found.is_empty() && !o.accept && unmeasured.is_none();
    let (lines, mut gate_failures) =
        exit_gate_lines(gates, &record.workload, &record.metrics, record.load_avg);
    for line in lines {
        println!("{line}");
    }
    if unmeasured.is_none() {
        for line in baseline_exclusions(history, &record) {
            println!("   baseline: {line}");
        }
    }
    if !o.gate_exits {
        for failure in gate_failures.drain(..) {
            println!("   recorded, not gated (--gate-exits applies it): {failure}");
        }
    }
    let mut failures = gate_failures;
    if regressed || (unmeasured.is_some() && !o.accept) {
        failures.extend(found.iter().cloned());
    }
    // A contended host did not measure host time, and a run spread over both clusters measured a
    // mix of two core speeds, so neither the exit gates nor the trend gate has anything to judge.
    // They are printed, so the run is never silently green, and they are not enforced, so a
    // shared machine does not invent a regression that is not in the code.
    if let Some(why) = unmeasured {
        for failure in failures.drain(..) {
            println!("   NOT MEASURED, {why}: {failure}");
        }
    }
    println!();
    Outcome {
        stored: record.to_json_with(regressed, o.accept),
        failures,
        accepted_despite: if o.accept { found } else { Vec::new() },
    }
}

/// Runs one F-suite up to `o.repeat` times and reports one run ([`reported_run`]).
///
/// `Ok(None)` means the suite did not run because its corpus id is absent, which is recorded as a
/// printed NOT_RUN line and never as a failure.
fn measure_suite(
    o: &Options,
    host: &Host,
    suite: &Suite,
    scenario: &Scenario,
    root: &Path,
    history: &[Value],
) -> Result<Option<Outcome>, String> {
    println!("{} {}", suite.id, suite.definition);
    let loaded = match corpus_image(root, scenario.image)? {
        Subject::Absent(reason) => {
            println!("   NOT_RUN: {reason}");
            println!("   gates when it runs: {}\n", suite.gates);
            return Ok(None);
        }
        Subject::Ready(loaded) => loaded,
    };
    let sites = loaded.sites;
    let mut runs: Vec<(Metrics, EndState, u64)> = Vec::with_capacity(o.repeat);
    let mut config = None;
    let mut pcm = None;
    let mut ble: Option<BleSummary> = None;
    for i in 0..o.repeat {
        if repeats_done(&runs.iter().map(|(m, ..)| m).collect::<Vec<_>>()) {
            break;
        }
        // The calibration pass: the same boot on a twin machine, in slices short enough that a
        // busy stretch of it does not idle (`CALIBRATION_PS`).
        let mut twin = loaded.machine(o)?;
        let slices = scenario.boot_budget_ms.div_ceil(o.calibration_ms.max(1)) + BOOT_SLACK_WINDOWS;
        let (calibration, _) = run_slices(
            &mut twin,
            o.calibration_ms * 1_000_000_000,
            slices,
            Some(scenario.boot_to),
        )?;
        drop(twin);

        let mut m = loaded.machine(o)?;
        if config.is_none() {
            config = Some(suite_key(&m, o, suite.id, scenario, sites));
        }
        let run = run_scenario(&mut m, calibration, scenario, o.window_ms)?;
        if let Some((_, first, _)) = runs.first()
            && *first != run.end
        {
            return Err(format!(
                "{} is not deterministic: run 1 ended at {first:?}, run {} at {:?}",
                suite.id,
                i + 1,
                run.end
            ));
        }
        if pcm.is_none() {
            pcm = run.pcm.clone();
        }
        // The guest is deterministic, so every repeat of a BLE leg must have done the same
        // thing; a repeat that did not is reported like a differing end state rather than
        // averaged away.
        match (&ble, &run.ble) {
            (None, got) => ble = got.clone(),
            (Some(first), Some(got)) if first != got => {
                return Err(format!(
                    "{} is not deterministic: run 1's BLE leg was {first:?}, run {}'s {got:?}",
                    suite.id,
                    i + 1
                ));
            }
            _ => {}
        }
        if o.console > 0 && i == 0 {
            let lines: Vec<&str> = run.console.lines().collect();
            for line in &lines[lines.len().saturating_sub(o.console)..] {
                println!("   console| {line}");
            }
        }
        runs.push((run.metrics(), run.end.clone(), run.boot_ps));
    }
    let ran = (
        runs.len(),
        runs.iter().filter(|r| r.0.on_measured_cores()).count(),
    );
    let (metrics, end, boot_ps) =
        runs[reported_run(&runs.iter().map(|(m, ..)| m).collect::<Vec<_>>())].clone();

    let cores = Cores::of(&metrics, cores::qos_now());
    let wav = match pcm {
        Some((fs, channels, samples)) if !samples.is_empty() => {
            Some(write_wav(root, suite.id, fs, channels, &samples)?)
        }
        _ => None,
    };
    let record = Record {
        workload: suite.id.to_string(),
        host: host.clone(),
        config: config.unwrap_or(Value::Null),
        metrics,
        commit: git_head(),
        dirty: git_dirty(),
        unix_s: now_unix(),
        load_avg: load_average(),
        cores,
    };
    let extra = Extra {
        boot_ps,
        sites,
        wav: wav.as_ref(),
        ble: ble.as_ref(),
    };
    print_metrics(o, ran, &record, &end, extra);
    let gates: Vec<ExitGate> = EXIT_GATES.iter().chain(F6_GATES.iter()).copied().collect();
    let mut outcome = finish(o, record, history, &gates);
    if let Some(wav) = wav {
        outcome.stored["artifact"] = wav.to_json();
    }
    if let Some(ble) = ble {
        outcome.stored["ble"] = ble.to_json();
    }
    Ok(Some(outcome))
}

/// Runs `rom-boot`, the harness workload of the `bench.rs` module documentation.
fn measure_rom_boot(o: &Options, host: &Host, history: &[Value]) -> Result<Outcome, String> {
    println!("{ROM_BOOT} (harness workload, not an F-suite): bundled ROM from reset");
    let mut runs = Vec::with_capacity(o.repeat);
    let mut sites = None;
    let mut config = None;
    for i in 0..o.repeat {
        if repeats_done(
            &runs
                .iter()
                .map(|(m, _): &(Metrics, _)| m)
                .collect::<Vec<_>>(),
        ) {
            break;
        }
        let mut m = rom_boot_machine()?;
        if sites.is_none() {
            sites = Some(rom_fusion_sites(&m));
            config = Some(machine_key(&m, o.windows));
        }
        let (windows, end) = run_windows(&mut m, o.windows)?;
        if let Some((_, first)) = runs.first()
            && *first != end
        {
            return Err(format!(
                "{ROM_BOOT} is not deterministic: run 1 ended at {first:?}, run {} at {end:?}",
                i + 1
            ));
        }
        runs.push((Metrics::from_windows(&windows), end));
    }
    let ran = (
        runs.len(),
        runs.iter().filter(|r| r.0.on_measured_cores()).count(),
    );
    let (metrics, end) =
        runs[reported_run(&runs.iter().map(|(m, _)| m).collect::<Vec<_>>())].clone();
    let cores = Cores::of(&metrics, cores::qos_now());
    let record = Record {
        workload: ROM_BOOT.to_string(),
        host: host.clone(),
        config: config.unwrap_or(Value::Null),
        metrics,
        commit: git_head(),
        dirty: git_dirty(),
        unix_s: now_unix(),
        load_avg: load_average(),
        cores,
    };
    let extra = Extra {
        boot_ps: 0,
        sites: sites.unwrap_or_default(),
        wav: None,
        ble: None,
    };
    print_metrics(o, ran, &record, &end, extra);
    Ok(finish(o, record, history, &[]))
}

/// Seconds since the epoch, or 0 when the host clock refuses.
pub(super) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What a measured run produced beside its metrics, for the printout.
struct Extra<'a> {
    /// Virtual ps the boot phase took, 0 for a workload with no separate boot.
    boot_ps: u64,
    /// Critical-section fusion sizing of the image (lever 3).
    sites: pemu_rv32::fuse::SiteCount,
    /// The WAV artifact of a capturing suite (F6).
    wav: Option<&'a WavArtifact>,
    /// The BLE ledger of a suite with a radio leg (F7).
    ble: Option<&'a BleSummary>,
}

/// The metrics of one record, as `xtask bench` prints them.
/// `runs` is `(runs made, of which measured on the cores the targets are for)`.
fn print_metrics(
    o: &Options,
    runs: (usize, usize),
    record: &Record,
    end: &EndState,
    extra: Extra<'_>,
) {
    let Extra {
        boot_ps,
        sites,
        wav,
        ble,
    } = extra;
    let m = &record.metrics;
    println!(
        "   fastest of {} runs on measured cores, of {} (at most {}); every run ended at insns \
         {} vt {} ps pc {:#010x} over {} measured windows",
        runs.1, runs.0, o.repeat, end.insns, end.vt_ps, end.pc, end.windows
    );
    if boot_ps > 0 {
        println!(
            "   boot phase           {:.1} ms virtual to the workload's console line",
            boot_ps as f64 * 1e-9
        );
    }
    match m.busy_mips {
        Some(s) => println!("   busy MIPS S          {s:.2} (trend data; no native floor)"),
        None => println!("   busy MIPS S          not resolvable"),
    }
    match m.idle_cost {
        Some(c) => println!("   idle cost c          {c:.5}"),
        None => println!("   idle cost c          not resolvable (see the notes below)"),
    }
    println!(
        "   demand p95 / max     {:.2} / {:.2} MIPS per 100 ms virtual window",
        m.demand_p95_mips, m.demand_max_mips
    );
    println!("   worst window         {:.2} ms host", m.worst_window_ms);
    println!(
        "   real-time factor     {:.3} ({:.2} s virtual in {:.3} s wall at Max)",
        m.real_time_factor, m.virtual_s, m.wall_s
    );
    if let Some(load) = record.load_avg {
        println!("   host load average    {load:.1} (context; nothing gates on it)");
    }
    let share =
        |s: Option<f64>| s.map_or("no reading".to_string(), |s| format!("{:.1} %", s * 100.0));
    println!(
        "   core cluster         {} at QoS {}: the S phase {} and the measured phase {} on the \
         performance cluster ({:.0} % is a measurement)",
        record.cores.cluster.as_str(),
        record.cores.qos,
        share(record.cores.s_share),
        share(record.cores.share),
        cores::RESIDENT_SHARE * 100.0,
    );
    println!(
        "   lever 3 sizing       {} critical-section units of {} candidate `csrrci mstatus` \
         sites, {} writing the threshold",
        sites.units, sites.candidates, sites.threshold_units
    );
    if let Some(wav) = wav {
        println!(
            "   WAV artifact         {} sha256 {} ({} frames at {} Hz, {} channels)",
            wav.path, wav.sha256, wav.frames, wav.fs, wav.channels
        );
    }
    if let Some(ble) = ble {
        println!(
            "   BLE leg              {} advertising events, connected={}, {} connection \
             events, {} notifications from {} polls, {} steps ({} of the last results not \
             ok, {} refused), H4 {} out / {} in",
            ble.adv_events,
            ble.connected,
            ble.connection_events,
            ble.notified,
            ble.polls_journaled,
            ble.steps,
            ble.recent_failed_steps,
            ble.refused_steps,
            ble.tx_packets,
            ble.rx_packets,
        );
    }
    for note in &m.notes {
        println!("   note: {note}");
    }
}

/// Whether `git status --porcelain` reports any change; `true` when git cannot answer, so an
/// unknown tree is never recorded as clean.
pub(super) fn git_dirty() -> bool {
    crate::util::git(&crate::util::workspace_root(), &["status", "--porcelain"])
        .map_or(true, |out| !out.is_empty())
}

/// The config key of a `rom-boot` record, read from the machine it ran on rather than written by
/// hand: the engine configuration, profile, poll fast-forward switch and ROM hash of
/// [`pemu_machine::Machine::config`] and [`pemu_machine::Machine::assets`], plus the window
/// shape and build profile.
/// The executor is part of the key, so block-engine and reference-stepper records never
/// baseline each other.
fn machine_key(m: &pemu_machine::Machine, windows: u64) -> Value {
    let cfg = m.config();
    let rom = hex(m.assets().rom.elf_sha256());
    let mut key = config_key(
        cfg.engine.max_block_insns,
        cfg.engine.strict_csr,
        cfg.engine.fuser.is_some(),
        &format!("{:?}", cfg.profile),
        cfg.poll_ff,
        &rom,
        windows,
    );
    key["engine"]["executor"] = json!(format!("{:?}", m.executor()));
    key
}

/// The config key of an F-suite record: [`machine_key`]'s engine and profile fields, plus what
/// makes two runs of this suite the same workload (the corpus id, the image identity and the
/// scenario's own shape).
pub(super) fn suite_key(
    m: &pemu_machine::Machine,
    o: &Options,
    id: &str,
    s: &Scenario,
    sites: pemu_rv32::fuse::SiteCount,
) -> Value {
    let mut key = machine_key(m, 0);
    let identity = m.assets().identity();
    key["windows"] = Value::Null;
    key["workload"] = json!({
        "id": id,
        "corpus": s.image.id,
        "image_sha256": hex(&identity.image_sha256),
        "app_elf_sha256": identity.app_elf_sha256.as_ref().map(|h| hex(h)),
        "boot_to": s.boot_to,
        "boot_budget_ms": s.boot_budget_ms,
        "setup_clicks": s.setup.schedule().len(),
        "setup_ms": s.setup_ms,
        "clicks": s.clicks.schedule().len(),
        "body_ms": s.body_ms,
        "click_ms": CLICK_MS,
        "ble": s.ble.map(|leg| json!({
            "service": leg.service,
            "events": leg.events,
            "commands": leg.commands,
            "scan_ms": leg.scan_ms,
            "connect_within_ms": leg.connect_within_ms,
            "poll_line": leg.poll_line,
            "poll_answer": leg.poll_answer,
            "poll_period_ms": leg.poll_period_ms,
            "poll_within_ms": leg.poll_within_ms,
        })),
    });
    // The harness shape. A diagnostic run at another window or calibration length measures
    // something else and must never baseline a default one.
    key["harness"] = json!({
        "window_ms": o.window_ms,
        "calibration_ms": o.calibration_ms,
    });
    // Critical-section fusion sizing (lever 3), recorded with the run rather than gating it.
    key["lever3"] = json!({
        "units": sites.units,
        "candidates": sites.candidates,
        "threshold_units": sites.threshold_units,
    });
    key
}

/// The JSON form of [`machine_key`], separate so the key's sensitivity is testable without a
/// machine.
pub(super) fn config_key(
    max_block_insns: u16,
    strict_csr: bool,
    fuser: bool,
    profile: &str,
    poll_ff: bool,
    rom_elf_sha256: &str,
    windows: u64,
) -> Value {
    json!({
        "engine": {
            "max_block_insns": max_block_insns,
            "strict_csr": strict_csr,
            "fuser": fuser,
        },
        "timing_profile": profile,
        "poll_ff": poll_ff,
        "rom_elf_sha256": rom_elf_sha256,
        "windows": windows,
        "window_ps": WINDOW_PS,
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
    })
}

/// `git rev-parse HEAD` of the repository, or `unknown`.
pub(super) fn git_head() -> String {
    crate::util::git_text(
        &crate::util::workspace_root(),
        &["rev-parse", "--short=12", "HEAD"],
    )
    .unwrap_or_else(|_| "unknown".to_string())
}
