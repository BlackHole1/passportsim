//! The Playwright steps of `xtask ci`, read per test from the JSON report.
//!
//! | Report | Status |
//! |---|---|
//! | `expected` (the test passed) | PASS |
//! | `skipped` with a `fixme` annotation | BLOCKED: the test waits on named work |
//! | `skipped` with a `skip` annotation | classified like a Rust `SKIP` reason (`outcome::skip_kind`) |
//! | `expected` with `expectedStatus` `failed` (`test.fail()`) | BLOCKED: it fails as declared |
//! | `unexpected` or `flaky` | FAIL |
//!
//! A report whose top-level `errors` is not empty (a spec that did not load, a failed global setup)
//! fails its step ([`report_errors`]).
//!
//! Every test that did not pass becomes a sub-step `<step>.<engine>.<spec>:<line>`; each `NOT_RUN`
//! annotation of a passing test (`<row> <leg>: <reason>`) becomes a NOT_RUN sub-step
//! `<step>.<engine>.<row>.<leg>`.

use serde_json::Value;

use super::model::{Status, StepResult};
use super::outcome::{self, SkipKind};

/// One Playwright test of a JSON report, for one project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebTest {
    /// Spec file relative to the test directory (`m9.spec.ts`).
    pub file: String,
    pub line: u64,
    /// The test's own title.
    pub title: String,
    pub status: Status,
    /// Why it did not pass; empty for a pass.
    pub reason: String,
    /// `(row leg, reason)` of each `NOT_RUN` annotation.
    pub not_run: Vec<(String, String)>,
}

impl WebTest {
    /// `<spec>:<line>`, the stable part of a sub-step name.
    pub fn location(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }
}

/// The status and reason of a Playwright test entry (module documentation).
fn status_of(test: &Value, annotations: &[&Value]) -> (Status, String) {
    let annotation = |kind: &str| {
        annotations
            .iter()
            .find(|a| a["type"] == kind)
            .map(|a| a["description"].as_str().unwrap_or("").to_string())
    };
    let expected = test["expectedStatus"].as_str().unwrap_or("passed");
    match test["status"].as_str() {
        // `test.fail()`: the test failed as declared, so what it covers does not work yet.
        Some("expected") if expected == "failed" => {
            let why = annotation("fail").unwrap_or_else(|| "no reason given".to_string());
            (
                Status::Blocked,
                format!("expected to fail (test.fail): {why}"),
            )
        }
        Some("expected") => (Status::Pass, String::new()),
        Some("unexpected") if expected == "failed" => (
            Status::Fail,
            "declared test.fail() but passed: remove the marker".to_string(),
        ),
        Some("skipped") => {
            if let Some(why) = annotation("fixme") {
                return (Status::Blocked, format!("fixme: {why}"));
            }
            let why = annotation("skip").unwrap_or_else(|| "skipped with no reason".to_string());
            let status = match outcome::skip_kind(&why) {
                SkipKind::Blocked => Status::Blocked,
                SkipKind::Corpus => Status::SkippedCorpus,
                SkipKind::Other => Status::Skipped,
            };
            (status, why)
        }
        Some("flaky") => (Status::Fail, "flaky: passed only on a retry".to_string()),
        Some(other) => (Status::Fail, format!("the test failed ({other})")),
        None => (
            Status::Fail,
            "the report gives the test no status".to_string(),
        ),
    }
}

/// `(row leg, reason)` of a `NOT_RUN` annotation's `"<row> <leg>: <reason>"`.
fn not_run_of(description: &str) -> Option<(String, String)> {
    let (head, reason) = description.split_once(": ")?;
    let (row, leg) = head.split_once(' ')?;
    Some((format!("{row}.{leg}"), reason.trim().to_string()))
}

/// The annotations of a test entry and of its results, each once: a static `test.skip` is on the
/// entry, and one pushed while the test runs (`test.info().annotations`) is on its result as well.
fn annotations_of(test: &Value) -> Vec<&Value> {
    let results = test["results"].as_array().into_iter().flatten();
    let mut out: Vec<&Value> = Vec::new();
    let entry = test["annotations"].as_array().into_iter().flatten();
    for a in entry.chain(results.flat_map(|r| r["annotations"].as_array().into_iter().flatten())) {
        let same = |b: &&Value| b["type"] == a["type"] && b["description"] == a["description"];
        if !out.iter().any(same) {
            out.push(a);
        }
    }
    out
}

fn walk(suite: &Value, out: &mut Vec<WebTest>) {
    for spec in suite["specs"].as_array().into_iter().flatten() {
        for test in spec["tests"].as_array().into_iter().flatten() {
            let annotations = annotations_of(test);
            let (status, reason) = status_of(test, &annotations);
            let not_run = annotations
                .iter()
                .filter(|a| a["type"] == "NOT_RUN")
                .filter_map(|a| not_run_of(a["description"].as_str()?))
                .collect();
            out.push(WebTest {
                file: spec["file"].as_str().unwrap_or("").to_string(),
                line: spec["line"].as_u64().unwrap_or(0),
                title: spec["title"].as_str().unwrap_or("").to_string(),
                status,
                reason,
                not_run,
            });
        }
    }
    for child in suite["suites"].as_array().into_iter().flatten() {
        walk(child, out);
    }
}

/// The top-level `errors` of a Playwright JSON report, one line each.
pub fn report_errors(json: &str) -> Vec<String> {
    let Ok(report) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    report["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let text = e["message"].as_str().unwrap_or("an error with no message");
            super::model::one_line(text.lines().next().unwrap_or(text))
        })
        .collect()
}

/// Every test of a Playwright JSON report, or why the report cannot be read.
pub fn read_report(json: &str) -> Result<Vec<WebTest>, String> {
    let report: Value = serde_json::from_str(json)
        .map_err(|err| format!("the JSON report does not parse: {err}"))?;
    let suites = report["suites"]
        .as_array()
        .ok_or("the JSON report has no `suites`")?;
    let mut out = Vec::new();
    for file in suites {
        walk(file, &mut out);
    }
    Ok(out)
}

/// The receipt sub-steps of `tests` run as `step` on `engine` (module documentation).
pub fn sub_steps(step: &str, engine: &str, tests: &[WebTest]) -> Vec<StepResult> {
    let mut subs = Vec::new();
    for test in tests {
        if test.status == Status::Pass {
            for (leg, why) in &test.not_run {
                subs.push(StepResult::instant(
                    &format!("{step}.{engine}.{leg}"),
                    Status::NotRun,
                    why.as_str(),
                ));
            }
            continue;
        }
        let reason = if test.reason.is_empty() {
            test.title.clone()
        } else {
            format!("{}; {}", test.title, test.reason)
        };
        let name = format!("{step}.{engine}.{}", test.location());
        subs.push(StepResult::instant(&name, test.status, reason));
    }
    subs
}

/// `N tests: a pass, b fail, ...` for a Playwright step's note.
pub fn summary(tests: &[WebTest]) -> String {
    let steps: Vec<StepResult> = tests
        .iter()
        .map(|t| StepResult::instant("", t.status, ""))
        .collect();
    let c = super::model::counts(&steps);
    format!(
        "{} tests: {} pass, {} fail, {} blocked, {} skipped, {} skipped-corpus",
        tests.len(),
        c.pass,
        c.fail,
        c.blocked,
        c.skipped,
        c.skipped_corpus
    )
}

/// Drops the NOT_RUN leg sub-steps that name an engine which did run in the same step
/// (`<step>.<engine>.<row>.<engine>`), such as `web/tests/m9.spec.ts`'s fixed
/// `NOT_RUN unpaced-webkit webkit: ...` once the WebKit project ran. A leg
/// naming an engine that did not run is kept.
pub fn drop_stale_engine_legs(subs: Vec<StepResult>, ran: &[&str]) -> Vec<StepResult> {
    subs.into_iter()
        .filter(|sub| {
            let leg = sub.name.rsplit('.').next().unwrap_or("");
            !(sub.status == Status::NotRun && ran.contains(&leg))
        })
        .collect()
}
