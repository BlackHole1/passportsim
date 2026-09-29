//! The cost model that the performance targets must be consistent with.
//!
//! The F3 rows, the demand they must meet, and `--check-model`, which checks them against
//! measured S and c.

use serde_json::Value;

use super::cli::{Options, history_path};
use super::history::{Host, load_history};
use super::suites::{ROM_BOOT, suite_runnable};

/// Length of one demand window: 100 ms of virtual time, in picoseconds.
pub(super) const WINDOW_PS: u64 = 100_000_000_000;

/// [`WINDOW_PS`] in milliseconds, for the window counts a scenario's virtual lengths imply.
pub(super) const WINDOW_MS: u64 = WINDOW_PS / 1_000_000_000;

/// Demand of the F3 scenario on the `official` image. UNVERIFIED: measured by an earlier profiling
/// run whose data is not in the repository.
#[derive(Clone, Copy, Debug)]
pub(super) struct F3Demand {
    /// Virtual length of F3: 60 s.
    seconds: f64,
    /// Idle share I of the scenario: about 0.983.
    idle_share: f64,
    /// Mean busy demand R over the whole scenario: 2.79 MIPS (`official` menu idle).
    mean_mips: f64,
}

pub(super) const G1_F3: F3Demand = F3Demand {
    seconds: 60.0,
    idle_share: 0.983,
    mean_mips: 2.79,
};

/// Peak 100 ms demand of the worst F3 window: Low Power card entry, 98.18 MIPS.
pub(super) const G1_WORST_WINDOW_MIPS: f64 = 98.18;

/// The limit an F3 target row sets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Limit {
    /// Wall seconds at `Max` pacing for the whole scenario.
    WallSeconds(f64),
    /// Share of one host core, paced at 1x.
    CoreShare(f64),
}

/// Which measured busy speed an F3 row is computed from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Engine {
    Native,
    Chrome,
    Jsc,
}

impl Engine {
    pub(super) fn name(self) -> &'static str {
        match self {
            Engine::Native => "native",
            Engine::Chrome => "chrome",
            Engine::Jsc => "jsc",
        }
    }

    /// The engine `name` spells, for `--gate-engine`.
    pub(super) fn parse(name: &str) -> Result<Engine, String> {
        [Engine::Native, Engine::Chrome, Engine::Jsc]
            .into_iter()
            .find(|e| e.name() == name)
            .ok_or_else(|| format!("unknown engine `{name}`; they are native, chrome and jsc"))
    }
}

/// One F3 target row.
#[derive(Clone, Copy, Debug)]
pub(super) struct F3Row {
    pub(super) name: &'static str,
    pub(super) engine: Engine,
    pub(super) limit: Limit,
    /// The row's own idle-cost ceiling.
    pub(super) c_max: f64,
}

/// Every F3 target row. The browser rows are the *machine's* host cost, not the browser's: the cost
/// model counts the emulator, and `bench-browser` gates the same quantity on counted
/// instructions, so a row that fails here fails there.
pub(super) const F3_ROWS: [F3Row; 3] = [
    F3Row {
        name: "F3 native wall at Max",
        engine: Engine::Native,
        limit: Limit::WallSeconds(2.0),
        c_max: 0.02,
    },
    F3Row {
        name: "F3 Chrome Worker host CPU paced at 1x",
        engine: Engine::Chrome,
        limit: Limit::CoreShare(0.07),
        c_max: 0.05,
    },
    F3Row {
        name: "F3 Safari / JSC host CPU paced at 1x",
        engine: Engine::Jsc,
        limit: Limit::CoreShare(0.07),
        c_max: 0.05,
    },
];

impl F3Row {
    /// The row's quantity from busy speed `s` (MIPS) and idle cost `c`, in the limit's unit:
    /// `seconds x (R / S + I x c)` for wall time, `R / S + I x c` for a core share (host s per
    /// emulated s = R / S + I x c).
    pub(super) fn predict(&self, d: F3Demand, s: f64, c: f64) -> f64 {
        let per_second = d.mean_mips / s + d.idle_share * c;
        match self.limit {
            Limit::WallSeconds(_) => d.seconds * per_second,
            Limit::CoreShare(_) => per_second,
        }
    }

    pub(super) fn limit(&self) -> f64 {
        match self.limit {
            Limit::WallSeconds(v) | Limit::CoreShare(v) => v,
        }
    }

    /// Scale from "host s per emulated s" to the limit's unit.
    fn scale(&self, d: F3Demand) -> f64 {
        match self.limit {
            Limit::WallSeconds(_) => d.seconds,
            Limit::CoreShare(_) => 1.0,
        }
    }

    /// Smallest busy speed that meets the limit at the row's own `c_max`, or `None` when the idle
    /// term alone already exceeds it, so no busy speed can.
    pub(super) fn min_s(&self, d: F3Demand) -> Option<f64> {
        let busy_budget = self.limit() / self.scale(d) - d.idle_share * self.c_max;
        (busy_budget > 0.0).then(|| d.mean_mips / busy_budget)
    }

    /// Largest idle cost that meets the limit at busy speed `s`, or `None` when the busy term
    /// alone exceeds it.
    pub(super) fn max_c(&self, d: F3Demand, s: f64) -> Option<f64> {
        let idle_budget = self.limit() / self.scale(d) - d.mean_mips / s;
        (idle_budget >= 0.0).then(|| idle_budget / d.idle_share)
    }

    fn unit(&self, v: f64) -> String {
        match self.limit {
            Limit::WallSeconds(_) => format!("{v:.3} s"),
            Limit::CoreShare(_) => format!("{:.2} % of a core", v * 100.0),
        }
    }
}

/// Verdict of one F3 row under `--check-model`.
#[derive(Debug, PartialEq)]
pub(super) enum RowVerdict {
    /// Reachable at the measured S under the row's own c ceiling.
    Reachable { predicted: f64 },
    /// Unreachable at any busy speed: the idle term at `c_max` alone exceeds the limit.
    UnreachableAtAnyS { idle_term: f64 },
    /// Unreachable at the measured S under the row's own c ceiling.
    UnreachableAtS { predicted: f64, min_s: f64 },
    /// No busy speed was measured for this row's engine; only the any-S check ran.
    NotMeasured { min_s: f64 },
    /// The measured idle cost is above the row's own ceiling; `predicted` is the row's quantity
    /// at the measured S and c.
    MeasuredCOverCeiling { c: f64, predicted: f64 },
    /// The row's quantity at the measured S and c exceeds the limit.
    UnreachableAtMeasuredC { c: f64, predicted: f64 },
}

/// Recomputes one F3 row from the F3 demand, a measured busy speed `s` (if any) and the row's own
/// idle-cost ceiling.
pub(super) fn check_row(row: &F3Row, d: F3Demand, s: Option<f64>) -> RowVerdict {
    let Some(min_s) = row.min_s(d) else {
        return RowVerdict::UnreachableAtAnyS {
            idle_term: row.scale(d) * d.idle_share * row.c_max,
        };
    };
    match s {
        None => RowVerdict::NotMeasured { min_s },
        Some(s) => {
            let predicted = row.predict(d, s, row.c_max);
            if predicted <= row.limit() {
                RowVerdict::Reachable { predicted }
            } else {
                RowVerdict::UnreachableAtS { predicted, min_s }
            }
        }
    }
}

/// [`check_row`], then the measured idle cost `c` when there is one: the row fails when `c` is above
/// its ceiling or when its quantity at the measured S and c exceeds the limit (the targets are
/// "with c <= ceiling", so both halves are part of the row).
pub(super) fn check_row_measured(
    row: &F3Row,
    d: F3Demand,
    s: Option<f64>,
    c: Option<f64>,
) -> RowVerdict {
    let verdict = check_row(row, d, s);
    let (RowVerdict::Reachable { .. }, Some(s), Some(c)) = (&verdict, s, c) else {
        return verdict;
    };
    let predicted = row.predict(d, s, c);
    if c > row.c_max {
        RowVerdict::MeasuredCOverCeiling { c, predicted }
    } else if predicted > row.limit() {
        RowVerdict::UnreachableAtMeasuredC { c, predicted }
    } else {
        RowVerdict::Reachable { predicted }
    }
}

/// Smallest busy speed that keeps a 100 ms window with peak demand `r` MIPS on time at idle cost
/// `c` (real-time condition): `r x 0.1 / S + (1 - r / 160) x 0.1 x c <= 0.1`.
/// `None` when the idle term alone takes the whole window.
pub(super) fn realtime_min_s(r: f64, c: f64) -> Option<f64> {
    let busy_share = 1.0 - (1.0 - r / 160.0) * c;
    (busy_share > 0.0).then(|| r / busy_share)
}

/// A measured input of `--check-model` and where it came from.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Measured {
    pub(super) s: f64,
    pub(super) c: Option<f64>,
    pub(super) source: String,
    /// True when S is not the F3 row's own engine measurement but `rom-boot` on machine v0's
    /// `ref_step`. Before F3 runs, a row it finds unreachable is reported and does not fail; once
    /// F3 runs, or in [`Mode::Gate`], a stand-in is itself a failure ([`model_report`]).
    pub(super) stand_in: bool,
}

/// Whether `--check-model` reports or gates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// A row with no measurement is reported and passes.
    Report,
    /// `--gate`, the mode of the native and browser model checks: a row of a gated engine that is
    /// not measured, or measured only by the stand-in, fails. An empty list gates every row (T2 on
    /// macOS); `--gate-engine native` gates the native rows alone (a host whose browser phase has
    /// not run).
    Gate(Vec<Engine>),
}

impl Mode {
    /// Whether a row of `engine` is gated.
    fn gates(&self, engine: Engine) -> bool {
        match self {
            Mode::Report => false,
            Mode::Gate(engines) => engines.is_empty() || engines.contains(&engine),
        }
    }
}

/// Inputs of [`model_report`].
#[derive(Clone, Debug)]
pub(super) struct ModelInputs {
    pub(super) native: Option<Measured>,
    pub(super) chrome: Option<Measured>,
    pub(super) jsc: Option<Measured>,
    /// True once F3 can run on this checkout or has a record for this host: from then on the
    /// native row needs an F3 measurement and the stand-in no longer passes.
    pub(super) f3_runnable: bool,
}

/// What [`model_report`] found.
#[derive(Debug, Default)]
pub(super) struct ModelOutcome {
    pub(super) lines: Vec<String>,
    pub(super) failures: Vec<String>,
    rows: usize,
    /// Rows checked against a real (not stand-in) measured S.
    checked: usize,
    not_measured: usize,
    stand_in: usize,
}

impl ModelOutcome {
    /// The closing line. It claims only the checks that ran.
    pub(super) fn summary(&self) -> String {
        if self.checked == self.rows {
            return format!(
                "checked all {} F3 rows against a measured S and c; none is unreachable under its \
                 own c",
                self.rows
            );
        }
        format!(
            "checked {} of {} F3 rows against a measured S ({} not measured, {} with a stand-in S); \
             every row passed the any-speed check, and the unchecked rows are not claimed",
            self.checked, self.rows, self.not_measured, self.stand_in
        )
    }
}

/// The workload id of F3, shared by the suite table and `--check-model`.
pub(super) const F3: &str = "F3";

/// The native S and c `--check-model` uses: `--s-native` (with `--c-native`), else the newest F3
/// record of this host whether or not it regressed, else the newest `rom-boot` record as a
/// stand-in (`bench.rs` module documentation). An F3 record whose S is not resolvable gives `None`
/// and never falls back to the stand-in.
pub(super) fn native_measurement(
    history: &[Value],
    host: &Host,
    s_flag: Option<f64>,
    c_flag: Option<f64>,
) -> Option<Measured> {
    if let Some(s) = s_flag {
        return Some(Measured {
            s,
            c: c_flag,
            source: "--s-native".to_string(),
            stand_in: false,
        });
    }
    let host = host.to_json();
    for (workload, note) in [
        (F3, ""),
        (
            ROM_BOOT,
            ", standing in for F3, which has no record on this host",
        ),
    ] {
        let Some(r) = history
            .iter()
            .rev()
            .find(|r| r["workload"] == workload && r["host"] == host && is_native(r))
        else {
            continue;
        };
        return r["metrics"]["busy_mips"].as_f64().map(|s| Measured {
            s,
            c: r["metrics"]["idle_cost"].as_f64(),
            source: format!(
                "history record {workload} at commit {}{note}",
                r["commit"].as_str().unwrap_or("?")
            ),
            stand_in: workload == ROM_BOOT,
        });
    }
    None
}

/// Whether `record` was measured natively: a browser record (`xtask bench-browser`)
/// names its engine in `config.runtime`.
fn is_native(record: &Value) -> bool {
    matches!(record["config"]["runtime"].as_str(), None | Some("native"))
}

/// The browser S and c `--check-model` uses for `engine`: the `--s-chrome` or
/// `--s-jsc` flag, else the newest uncontended F3 record `xtask bench-browser` stored for this host
/// in that engine. A contended record is skipped: its host seconds are shared with other work, so
/// its S and c are not the browser's. With neither, the row is not measured.
pub(super) fn browser_measurement(
    history: &[Value],
    host: &Host,
    engine: Engine,
    s_flag: Option<f64>,
    c_flag: Option<f64>,
) -> Option<Measured> {
    if let Some(s) = s_flag {
        return Some(Measured {
            s,
            c: c_flag,
            source: format!("--s-{}", engine.name()),
            stand_in: false,
        });
    }
    let host = host.to_json();
    let r = history.iter().rev().find(|r| {
        r["workload"] == F3
            && r["host"] == host
            && r["config"]["runtime"] == engine.name()
            && r["contended"] != true
    })?;
    r["metrics"]["busy_mips"].as_f64().map(|s| Measured {
        s,
        c: r["metrics"]["idle_cost"].as_f64(),
        source: format!(
            "history record F3 in {} {} at commit {}",
            engine.name(),
            r["config"]["browser"].as_str().unwrap_or("?"),
            r["commit"].as_str().unwrap_or("?")
        ),
        stand_in: false,
    })
}

/// Whether this host has any native F3 record.
pub(super) fn f3_recorded(history: &[Value], host: &Host) -> bool {
    let host = host.to_json();
    history
        .iter()
        .any(|r| r["workload"] == F3 && r["host"] == host && is_native(r))
}

/// Checks every row of `rows` (`--check-model`).
pub(super) fn model_report(
    rows: &[F3Row],
    d: F3Demand,
    inputs: &ModelInputs,
    mode: Mode,
) -> ModelOutcome {
    let mut out = ModelOutcome {
        rows: rows.len(),
        ..ModelOutcome::default()
    };
    out.lines.push(format!(
        "demand of F3: {} s virtual, idle share I = {}, mean R = {} MIPS",
        d.seconds, d.idle_share, d.mean_mips
    ));
    for row in rows {
        let m = match row.engine {
            Engine::Native => inputs.native.as_ref(),
            Engine::Chrome => inputs.chrome.as_ref(),
            Engine::Jsc => inputs.jsc.as_ref(),
        };
        let target = row.unit(row.limit());
        out.lines
            .push(format!("{} <= {target} with c <= {}", row.name, row.c_max));
        let stand_in = m.is_some_and(|m| m.stand_in);
        // A stand-in may only inform, and only while F3 cannot run and the mode is Report.
        let gated = mode.gates(row.engine);
        let informative = stand_in && !inputs.f3_runnable && !gated;
        if stand_in {
            out.stand_in += 1;
            if !informative {
                out.failures.push(format!(
                    "{}: S is the rom-boot stand-in, not an F3 measurement ({}); {}",
                    row.name,
                    m.map_or("", |m| m.source.as_str()),
                    if inputs.f3_runnable {
                        "F3 runs now, so run it and record its S"
                    } else {
                        "the gate needs an F3 measurement"
                    }
                ));
            }
        }
        if let Some(m) = m {
            out.lines.push(format!("   S source: {}", m.source));
        }
        let verdict = check_row_measured(row, d, m.map(|m| m.s), m.and_then(|m| m.c));
        let s = m.map_or(0.0, |m| m.s);
        let message = match verdict {
            RowVerdict::Reachable { predicted } => {
                if !stand_in {
                    out.checked += 1;
                }
                out.lines.push(format!(
                    "   reachable: S = {s:.2} MIPS ({}) and c = {} predict {}; the row needs S >= \
                     {:.2}, and at this S it allows c <= {:.5}",
                    row.engine.name(),
                    m.and_then(|m| m.c)
                        .map_or(format!("{} (ceiling)", row.c_max), |c| format!("{c:.5}")),
                    row.unit(predicted),
                    row.min_s(d).unwrap_or_default(),
                    row.max_c(d, s).unwrap_or_default(),
                ));
                None
            }
            RowVerdict::UnreachableAtAnyS { idle_term } => {
                let c_needed = row.limit() / row.scale(d) / d.idle_share;
                // Arithmetic over the row alone: it fails whatever S is, stand-in or not.
                out.failures.push(format!(
                    "{}: unreachable at any busy speed: the idle term alone at its own c = {} is \
                     {}, over the {target} target by {}; the target needs c < {c_needed:.5}",
                    row.name,
                    row.c_max,
                    row.unit(idle_term),
                    row.unit(idle_term - row.limit()),
                ));
                None
            }
            RowVerdict::NotMeasured { min_s } => {
                out.not_measured += 1;
                let line = format!(
                    "{}: not measured for {} ({}); reachable at any S >= {min_s:.2} MIPS",
                    row.name,
                    row.engine.name(),
                    match row.engine {
                        Engine::Native => "native S comes from an F3 record of this host",
                        _ =>
                            "browser S comes from an uncontended `xtask bench-browser` F3 record of this host",
                    }
                );
                let native_due = row.engine == Engine::Native && inputs.f3_runnable;
                if gated || native_due {
                    out.failures.push(format!(
                        "{line}; no measured S, and {}",
                        if native_due {
                            "F3 runs now"
                        } else {
                            "--gate needs one"
                        }
                    ));
                } else {
                    out.lines.push(format!("   {line}"));
                }
                None
            }
            RowVerdict::UnreachableAtS { predicted, min_s } => {
                let c_note = match row.max_c(d, s) {
                    Some(c) => format!("or c <= {c:.5}"),
                    None => "and no c can compensate at this S".to_string(),
                };
                Some(format!(
                    "{}: unreachable at the measured S = {s:.2} MIPS ({}) under its own c = {}: \
                     predicted {}, over the {target} target by {} ({:.1} % relative); it needs \
                     S >= {min_s:.2} MIPS {c_note}",
                    row.name,
                    row.engine.name(),
                    row.c_max,
                    row.unit(predicted),
                    row.unit(predicted - row.limit()),
                    (predicted / row.limit() - 1.0) * 100.0,
                ))
            }
            RowVerdict::MeasuredCOverCeiling { c, predicted } => Some(format!(
                "{}: measured c = {c:.5} is over the row's ceiling {} (at S = {s:.2} MIPS it \
                 predicts {} against the {target} target)",
                row.name,
                row.c_max,
                row.unit(predicted),
            )),
            RowVerdict::UnreachableAtMeasuredC { c, predicted } => Some(format!(
                "{}: at the measured S = {s:.2} MIPS and c = {c:.5} the row predicts {}, over the \
                 {target} target by {}",
                row.name,
                row.unit(predicted),
                row.unit(predicted - row.limit()),
            )),
        };
        if let Some(message) = message {
            if informative {
                out.lines
                    .push(format!("   not failing, S is a stand-in: {message}"));
            } else if !stand_in {
                out.failures.push(message);
            } else {
                out.lines.push(format!("   {message}"));
            }
        }
    }
    for c in [0.28, 0.05, 0.02] {
        if let Some(s) = realtime_min_s(G1_WORST_WINDOW_MIPS, c) {
            out.lines.push(format!(
                "real-time condition, worst F3 window {G1_WORST_WINDOW_MIPS} MIPS at c = {c}: \
                 needs S >= {s:.1} MIPS"
            ));
        }
    }
    out
}

/// `xtask bench --check-model`.
pub(super) fn check_model(o: &Options) -> Result<(), String> {
    let host = Host::of_process();
    let path = history_path(o)?;
    let history = load_history(&path)?;
    let inputs = ModelInputs {
        native: native_measurement(&history, &host, o.s_native, o.c_native),
        chrome: browser_measurement(&history, &host, Engine::Chrome, o.s_chrome, o.c_chrome),
        jsc: browser_measurement(&history, &host, Engine::Jsc, o.s_jsc, o.c_jsc),
        f3_runnable: suite_runnable(F3) || f3_recorded(&history, &host),
    };
    let mode = if o.gate {
        Mode::Gate(o.gate_engines.clone())
    } else {
        Mode::Report
    };
    println!("xtask bench --check-model: F3 rows against the cost model ({mode:?} mode)");
    println!("host: {host}; history: {}", path.display());
    let out = model_report(&F3_ROWS, G1_F3, &inputs, mode);
    for line in &out.lines {
        println!("{line}");
    }
    if out.failures.is_empty() {
        println!("{}", out.summary());
        Ok(())
    } else {
        Err(out.failures.join("\n"))
    }
}
