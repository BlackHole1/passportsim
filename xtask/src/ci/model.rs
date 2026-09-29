//! Step results, their aggregation and the summary table.

use std::time::Duration;

/// Outcome of one CI step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The step ran and succeeded.
    Pass,
    /// The step ran and failed; the tier fails.
    Fail,
    /// The step cannot run for want of an input or tool it names, such as an absent
    /// `web/package.json` or an uninstalled target; the tier does not fail.
    Skipped,
    /// The step exists but could not run here, such as a browser row whose browser is not
    /// installed; the tier does not fail.
    NotRun,
    /// A test returned without proving what it covers: it printed `SKIP <test>: blocked: <reason>`
    /// naming its blocker, or a `PENDING` line for a golden awaiting a person's approval
    /// (`outcome.rs`). Never a pass; the tier does not fail on it.
    Blocked,
    /// A test returned without running because the data root or a corpus file it needs is absent
    /// (`corpus id ... unavailable` from `corpus_or_skip`). Never a pass; the tier does not fail
    /// on it.
    SkippedCorpus,
}

impl Status {
    /// The spelling used in receipts and tables.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skipped => "SKIPPED",
            Status::NotRun => "NOT_RUN",
            Status::Blocked => "BLOCKED",
            Status::SkippedCorpus => "SKIPPED-CORPUS",
        }
    }
}

/// Result of one step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepResult {
    /// Short step name, such as `clippy` or `wasm32-check`.
    pub name: String,
    /// Outcome.
    pub status: Status,
    /// Why the step failed, was skipped or was not run; a short note or empty for a pass.
    pub reason: String,
    /// Wall time of the step.
    pub duration: Duration,
}

impl StepResult {
    /// A step that took no time, with the given status and reason.
    pub fn instant(name: &str, status: Status, reason: impl Into<String>) -> StepResult {
        StepResult {
            name: name.to_string(),
            status,
            reason: reason.into(),
            duration: Duration::ZERO,
        }
    }
}

/// Number of steps per status. BLOCKED and SKIPPED-CORPUS are counted apart from PASS and from
/// each other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub pass: usize,
    pub fail: usize,
    pub skipped: usize,
    pub not_run: usize,
    pub blocked: usize,
    pub skipped_corpus: usize,
}

/// Counts of `steps` per status.
pub fn counts(steps: &[StepResult]) -> Counts {
    let mut counts = Counts::default();
    for step in steps {
        match step.status {
            Status::Pass => counts.pass += 1,
            Status::Fail => counts.fail += 1,
            Status::Skipped => counts.skipped += 1,
            Status::NotRun => counts.not_run += 1,
            Status::Blocked => counts.blocked += 1,
            Status::SkippedCorpus => counts.skipped_corpus += 1,
        }
    }
    counts
}

/// Tier result: `Fail` when any step failed, otherwise `Pass`. Skipped, not-run, blocked and
/// skipped-corpus steps are listed and counted in the receipt but never fail a tier, and an empty
/// step list passes.
pub fn overall(steps: &[StepResult]) -> Status {
    if steps.iter().any(|step| step.status == Status::Fail) {
        Status::Fail
    } else {
        Status::Pass
    }
}

/// `1.234 s` style duration for tables and progress lines.
pub fn seconds(duration: Duration) -> String {
    format!("{:.1} s", duration.as_secs_f64())
}

/// Compact summary table: one line per step, then the counts line.
pub fn summary_table(steps: &[StepResult]) -> String {
    let width = steps
        .iter()
        .map(|step| step.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let mut out = format!(
        "{:<width$}  {:<14}  {:>8}  reason\n",
        "step", "status", "time"
    );
    for step in steps {
        let reason = one_line(&step.reason);
        out.push_str(&format!(
            "{:<width$}  {:<14}  {:>8}  {}\n",
            step.name,
            step.status.as_str(),
            seconds(step.duration),
            reason
        ));
    }
    let c = counts(steps);
    out.push_str(&format!(
        "result: {} ({} pass, {} fail, {} skipped, {} not run, {} blocked, {} skipped-corpus)",
        overall(steps).as_str(),
        c.pass,
        c.fail,
        c.skipped,
        c.not_run,
        c.blocked,
        c.skipped_corpus
    ));
    out
}

/// `text` with tabs, carriage returns and newlines replaced by spaces.
pub fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| {
            if matches!(c, '\t' | '\r' | '\n') {
                ' '
            } else {
                c
            }
        })
        .collect()
}
