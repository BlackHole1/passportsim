//! The measurement history and the 10 % regression gate.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

use super::cores::{self, Cluster};
use super::metrics::Metrics;

/// Allowed regression before the gate fails: 10 %.
pub(super) const REGRESSION: f64 = 0.10;

/// Passing records of the same key the rolling baseline is the median of, which resists one noisy
/// run in either direction ([`regressions`]). UNVERIFIED: a design choice.
const BASELINE_RECORDS: usize = 5;

/// Absolute band of the idle-cost gate: 0.002 host s per idle emulated s, a tenth of the native F3
/// ceiling of 0.02. UNVERIFIED: a design choice.
const C_ABSOLUTE_BAND: f64 = 0.002;

/// History file version this module reads and writes.
const HISTORY_VERSION: u64 = 1;

/// OS, architecture and CPU model: the key of every baseline; numbers never cross hosts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Host {
    pub(super) os: String,
    pub(super) arch: String,
    pub(super) cpu: String,
}

impl Host {
    pub(super) fn of_process() -> Host {
        Host {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpu: cpu_model(),
        }
    }

    pub(super) fn to_json(&self) -> Value {
        json!({ "os": self.os, "arch": self.arch, "cpu": self.cpu })
    }
}

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} \"{}\"", self.os, self.arch, self.cpu)
    }
}

/// Runnable threads per core above which this host is not measuring anything: every
/// native target is an absolute host-time budget, and at a load average past the core count the
/// emulator's host seconds are shared with a queue.
pub(crate) const CONTENDED_LOAD_PER_CORE: f64 = 1.0;

/// Cores this host schedules on, for [`CONTENDED_LOAD_PER_CORE`]. Unknown counts as one, which
/// makes the contention test strict rather than lenient.
pub(crate) fn cores() -> f64 {
    std::thread::available_parallelism().map_or(1.0, |n| n.get() as f64)
}

/// `Some(load)` when the host was too busy for a host-time budget to mean anything: the run's
/// guest-side numbers stand, its gates are reported and not enforced, and it never baselines.
pub(super) fn contention(load: Option<f64>) -> Option<f64> {
    load.filter(|load| *load > cores() * CONTENDED_LOAD_PER_CORE)
}

/// One-minute load average of this host, or `None` where it cannot be read.
///
/// It is run context, not a fingerprint: a number, never a name or a path.
pub(crate) fn load_average() -> Option<f64> {
    let out = Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `{ 3.52 3.61 3.58 }`
    String::from_utf8(out.stdout)
        .ok()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// CPU model string: `sysctl -n machdep.cpu.brand_string` on macOS, `PROCESSOR_IDENTIFIER` on
/// Windows, `unknown` otherwise. `xtask bench-k` records the same string.
pub(crate) fn cpu_model() -> String {
    if cfg!(target_os = "macos")
        && let Ok(out) = Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
        && out.status.success()
        && let Ok(text) = String::from_utf8(out.stdout)
    {
        return text.trim().to_string();
    }
    std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".to_string())
}

/// One measurement as stored in the history.
#[derive(Clone, Debug)]
pub(super) struct Record {
    pub(super) workload: String,
    pub(super) host: Host,
    /// Everything besides the host that makes two runs comparable: executor, virtual length,
    /// build profile. Records with a different config never baseline each other.
    pub(super) config: Value,
    pub(super) metrics: Metrics,
    pub(super) commit: String,
    /// True when the working tree had uncommitted changes, so `commit` does not name the code.
    pub(super) dirty: bool,
    pub(super) unix_s: u64,
    /// One-minute load average while the workload ran ([`load_average`]).
    pub(super) load_avg: Option<f64>,
    /// The QoS class and core cluster the host time was measured at ([`Cores`]).
    pub(super) cores: Cores,
}

/// Where a record's host time was spent (the `cores` module): the QoS class the measuring thread
/// ran at and the core cluster its S phase and measured phase ran on.
///
/// S follows the S phase's share on the performance cluster (the `cores` module), so two records
/// compare only when both fields are the same ([`comparable_cores`]).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Cores {
    pub(super) qos: &'static str,
    pub(super) cluster: Cluster,
    /// Share of the S phase's CPU time on the performance cluster ([`Metrics::s_perf_share`]).
    pub(super) s_share: Option<f64>,
    /// Share of the measured phase's CPU time there ([`Metrics::perf_share`]).
    pub(super) share: Option<f64>,
}

impl Cores {
    /// The cores of a run with `metrics`, measured on a thread at the QoS class `qos`.
    pub(super) fn of(metrics: &Metrics, qos: &'static str) -> Cores {
        Cores {
            qos,
            cluster: metrics.cluster(),
            s_share: metrics.s_perf_share,
            share: metrics.perf_share,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "qos": self.qos,
            "cluster": self.cluster.as_str(),
            "s_perf_share": self.s_share,
            "perf_share": self.share,
        })
    }

    /// Why this record's host time is not a measurement of the cores the native targets are
    /// for, or `None` when it is.
    pub(super) fn unmeasured(&self) -> Option<String> {
        (!self.cluster.measures()).then(|| {
            let pct = |s: Option<f64>| {
                s.map_or("no reading".to_string(), |s| format!("{:.0} %", s * 100.0))
            };
            format!(
                "core cluster {}: the S phase ran {} and the measured phase {} on the \
                 performance cluster, where {:.0} % is a measurement",
                self.cluster.as_str(),
                pct(self.s_share),
                pct(self.share),
                cores::RESIDENT_SHARE * 100.0
            )
        })
    }
}

impl Record {
    #[cfg(test)]
    pub(super) fn to_json(&self, regressed: bool) -> Value {
        self.to_json_with(regressed, false)
    }

    /// The stored form. `accepted` marks a record written by `bench --accept`: it pins the
    /// baseline, and no record before it is a baseline any more.
    pub(super) fn to_json_with(&self, regressed: bool, accepted: bool) -> Value {
        json!({
            "workload": self.workload,
            "host": self.host.to_json(),
            "config": self.config,
            "metrics": self.metrics.to_json(),
            "commit": self.commit,
            "dirty": self.dirty,
            "unix_s": self.unix_s,
            "load_avg": self.load_avg,
            "cores": self.cores.to_json(),
            // Set by [`contention`]: the guest-side metrics of this record are exact and its
            // host-side ones are not the emulator's. It is stored so the trend gate can skip it
            // as a baseline, and so a reader of the history knows which rows are trend data.
            "contended": contention(self.load_avg).is_some(),
            "regressed": regressed,
            "accepted": accepted,
        })
    }
}

/// Reads the history at `path`; an absent file is an empty history.
pub(super) fn load_history(path: &Path) -> Result<Vec<Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let doc: Value =
        serde_json::from_str(&text).map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
    if doc["version"].as_u64() != Some(HISTORY_VERSION) {
        return Err(format!(
            "{} has history version {}, this xtask reads {HISTORY_VERSION}",
            path.display(),
            doc["version"]
        ));
    }
    doc["records"]
        .as_array()
        .cloned()
        .ok_or_else(|| format!("{} has no `records` array", path.display()))
}

/// Writes `records` to `path`, through a sibling temporary file so a crash never truncates it.
pub(super) fn save_history(path: &Path, records: &[Value]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let doc = json!({ "version": HISTORY_VERSION, "records": records });
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text + "\n")
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// Median of `values`, which must not be empty.
pub(super) fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// The trend-gated metrics (busy MIPS and idle cost c) and which direction is worse.
const GATED: [(&str, Direction); 2] = [
    ("busy_mips", Direction::HigherIsBetter),
    ("idle_cost", Direction::LowerIsBetter),
];

#[derive(Clone, Copy, Debug)]
enum Direction {
    HigherIsBetter,
    LowerIsBetter,
}

/// The regressions of `new` against `history` (the 10 % gate), one message per metric.
///
/// Only records of the same key and cores ([`comparable_cores`]) count, from the newest accepted
/// record (`bench --accept`) onward. Each metric is compared with a **rolling** baseline (median of
/// the last [`BASELINE_RECORDS`] non-regressed records) and an **anchor** (the accepted record,
/// else the median of the first [`BASELINE_RECORDS`]), which catches slow drift. Idle cost also
/// gets [`C_ABSOLUTE_BAND`]. No baseline passes; a metric the baseline has and the run could not
/// resolve fails. Regressed records never baseline, so a lasting change fails until accepted.
pub(super) fn regressions(history: &[Value], new: &Record) -> Vec<String> {
    let same: Vec<&Value> = same_key(history, new)
        .filter(|r| comparable_cores(r, &new.cores))
        .collect();
    let from = same
        .iter()
        .rposition(|r| r["accepted"] == true)
        .unwrap_or(0);
    let window: Vec<&Value> = same[from..]
        .iter()
        .copied()
        // A contended record measured the guest and not the host, so it never baselines a later
        // run; an accepted one does, because a human pinned it on purpose.
        .filter(|r| (r["regressed"] != true && r["contended"] != true) || r["accepted"] == true)
        .collect();
    let now = new.metrics.to_json();
    let mut found = Vec::new();
    for (metric, direction) in GATED {
        let mut past: Vec<f64> = window
            .iter()
            .rev()
            .filter_map(|r| r["metrics"][metric].as_f64())
            .take(BASELINE_RECORDS)
            .collect();
        if past.is_empty() {
            continue;
        }
        let Some(value) = now[metric].as_f64() else {
            found.push(format!(
                "{}: {metric} has a baseline but this run could not resolve it",
                new.workload
            ));
            continue;
        };
        let rolling = median(&mut past);
        // The anchor: the accepted record itself, else the median of the first records of the
        // key, so one noisy first run does not become the reference for good.
        let anchor = match window.first() {
            Some(first) if first["accepted"] == true => first["metrics"][metric].as_f64(),
            _ => {
                let mut first: Vec<f64> = window
                    .iter()
                    .filter_map(|r| r["metrics"][metric].as_f64())
                    .take(BASELINE_RECORDS)
                    .collect();
                (!first.is_empty()).then(|| median(&mut first))
            }
        };
        let baselines = [
            Some((rolling, format!("rolling median of {} records", past.len()))),
            anchor.map(|a| (a, "anchor".to_string())),
        ];
        // One message per metric: the rolling baseline when it trips, else the anchor.
        if let Some(message) = baselines
            .into_iter()
            .flatten()
            .find_map(|(base, name)| compare(&new.workload, metric, direction, value, base, &name))
        {
            found.push(message);
        }
    }
    found
}

/// The records of `history` with `new`'s workload, host fingerprint and config.
fn same_key<'a>(history: &'a [Value], new: &Record) -> impl Iterator<Item = &'a Value> {
    let host = new.host.to_json();
    let workload = new.workload.clone();
    let config = new.config.clone();
    history.iter().filter(move |r| {
        r["workload"] == workload.as_str() && r["host"] == host && r["config"] == config
    })
}

/// Whether the stored record `r` was measured on the same cores as `new`: the same QoS class and
/// the same core cluster, and that a cluster the native targets are for ([`Cluster::measures`]).
///
/// Like the host fingerprint, this binds accepted records too. A `mixed`, `efficiency` or
/// unclassified (no `cores`) record is never a baseline ([`baseline_exclusions`] counts them). A
/// `mixed` or `efficiency` **new** record is still compared with the performance-cluster records,
/// so a regression is named; [`finish`](super::cli::finish) reports it NOT MEASURED rather than
/// enforcing it.
fn comparable_cores(r: &Value, new: &Cores) -> bool {
    let want = if new.cluster.measures() {
        new.cluster
    } else {
        Cluster::Performance
    };
    r["cores"]["qos"] == new.qos && r["cores"]["cluster"] == want.as_str()
}

/// What [`comparable_cores`] left out of `new`'s baselines, one line per reason; empty when it
/// left out nothing.
pub(super) fn baseline_exclusions(history: &[Value], new: &Record) -> Vec<String> {
    let (mut unclassified, mut other) = (0usize, 0usize);
    for r in same_key(history, new) {
        if r.get("cores").is_none() {
            unclassified += 1;
        } else if !comparable_cores(r, &new.cores) {
            other += 1;
        }
    }
    let mut lines = Vec::new();
    if unclassified > 0 {
        lines.push(format!(
            "{unclassified} earlier record(s) of this workload, host and config carry no core \
             cluster reading (they predate the core-cluster reading), so they cannot be classified and are not \
             baselines"
        ));
    }
    if other > 0 {
        lines.push(format!(
            "{other} earlier record(s) were measured at another QoS class or on another core \
             cluster than this run ({} on {}), so they are not baselines",
            new.cores.qos,
            new.cores.cluster.as_str()
        ));
    }
    lines
}

/// One comparison of [`regressions`]: a message when `value` is past the band around `base`.
fn compare(
    workload: &str,
    metric: &str,
    direction: Direction,
    value: f64,
    base: f64,
    name: &str,
) -> Option<String> {
    let (worse, bound) = match direction {
        Direction::HigherIsBetter => {
            let bound = base * (1.0 - REGRESSION);
            (value < bound, bound)
        }
        // A relative band alone vanishes at a zero or tiny baseline (an idle cost at the
        // resolution of the run), so the bound is never tighter than an absolute step.
        Direction::LowerIsBetter => {
            let bound = (base * (1.0 + REGRESSION)).max(base + C_ABSOLUTE_BAND);
            (value > bound, bound)
        }
    };
    if !worse {
        return None;
    }
    let change = if base > 0.0 {
        format!("{:+.1} %", (value - base) / base * 100.0)
    } else {
        format!("{:+.4} absolute", value - base)
    };
    Some(format!(
        "{workload}: {metric} {value:.4} against the {name} baseline of {base:.4}, {change}, past \
         the bound of {bound:.4} ({:.0} % or, for idle cost, {C_ABSOLUTE_BAND} absolute, \
         whichever is looser)",
        REGRESSION * 100.0
    ))
}
