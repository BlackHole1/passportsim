//! The one reader of what a test run prints, for each `xtask ci` step that runs tests.
//!
//! The Rust harness has no "skipped" or "blocked" outcome: a test that returns without proving
//! what it claims has passed as far as the harness knows. So such a test says so on stdout, one
//! line per fact, and this module turns those lines into statuses:
//!
//! | Line | Printed by | Status |
//! |---|---|---|
//! | `RAN <test> <leg>[: <detail>]` | a test, for a leg or image it did run | none by itself; see "partial" below |
//! | `SKIP <test>: blocked: <reason>` | a test that waits on named work, such as a browser test with no image given | BLOCKED |
//! | `SKIP <test>: corpus id `<id>` unavailable: ...` (or `has no file`, `no data root`, a derived golden not below the data root) | `corpus_or_skip` and the golden helpers | SKIPPED-CORPUS |
//! | `SKIP <test>: <other reason>` | any other skip, such as a missing tool or an unembedded payload | SKIPPED |
//! | `PENDING <test>: <reason>` | a test whose golden awaits a person's approval | BLOCKED, "awaiting golden approval" |
//! | `NOT_RUN <test> <leg>: <reason>` | a comparison that could not run or ran over nothing | NOT_RUN, per leg |
//!
//! Every step that runs tests passes `--show-output`, so each line reaches stdout inside the
//! `---- <name> stdout ----` section of the test that printed it.
//!
//! - A test with a `SKIP` or `PENDING` line is a sub-step `<step>.<test>`, each `NOT_RUN` leg a
//!   sub-step `<step>.<leg>`. BLOCKED outranks SKIPPED-CORPUS, which outranks SKIPPED.
//! - **Partial.** A test that printed `RAN` as well as `SKIP` is PASS with its skips as a NOT_RUN
//!   sub-step; it, like a passing test with `NOT_RUN` lines, carries them in
//!   [`TestOutcome::partial`].
//! - Orphan ([`read`]) and malformed ([`Report::malformed`]) marker lines fail the step. The
//!   self-checks of `tests/milestones/common.rs` ([`SELF_CHECKS`]) are not read.

use super::model::{Status, StepResult};

/// One line a test printed, borrowed from the run's stdout (module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Marker<'a> {
    /// `RAN <test> <leg>[: <detail>]`.
    Ran { test: &'a str, leg: &'a str },
    /// `SKIP <test>: <reason>`.
    Skip { test: &'a str, reason: &'a str },
    /// `NOT_RUN <test> <leg>: <reason>`.
    NotRun {
        test: &'a str,
        leg: &'a str,
        reason: &'a str,
    },
    /// `PENDING <test>: <reason>`.
    Pending { test: &'a str, reason: &'a str },
}

/// A test name as a marker spells it: non-empty, no whitespace.
fn test_name(text: &str) -> Option<&str> {
    (!text.is_empty() && !text.contains(char::is_whitespace)).then_some(text)
}

/// The four prefixes a line uses to announce it is a marker.
const MARKER_PREFIXES: [&str; 4] = ["RAN ", "SKIP ", "PENDING ", "NOT_RUN "];

/// Whether `line` claims to be a marker, whatever [`marker`] then makes of it.
pub fn claims_marker(line: &str) -> bool {
    let line = line.trim_end_matches('\r');
    MARKER_PREFIXES.iter().any(|head| line.starts_with(head))
}

/// The marker of one stdout line, or `None` for any other line.
pub fn marker(line: &str) -> Option<Marker<'_>> {
    let line = line.trim_end_matches('\r');
    if let Some(rest) = line.strip_prefix("RAN ") {
        let test = test_name(rest.split([' ', ':']).next()?)?;
        let leg = rest[test.len()..].trim_start();
        let leg = leg.split(':').next().unwrap_or("").trim();
        return Some(Marker::Ran { test, leg });
    }
    if let Some(rest) = line.strip_prefix("SKIP ") {
        let (test, reason) = rest.split_once(": ")?;
        return Some(Marker::Skip {
            test: test_name(test)?,
            reason: reason.trim(),
        });
    }
    if let Some(rest) = line.strip_prefix("PENDING ") {
        let (test, reason) = rest.split_once(": ")?;
        return Some(Marker::Pending {
            test: test_name(test)?,
            reason: reason.trim(),
        });
    }
    if let Some(rest) = line.strip_prefix("NOT_RUN ") {
        let (head, reason) = rest.split_once(": ")?;
        let (test, leg) = head.split_once(' ')?;
        return Some(Marker::NotRun {
            test: test_name(test)?,
            leg: test_name(leg.trim())?,
            reason: reason.trim(),
        });
    }
    None
}

/// What a `SKIP` reason says about the test (module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipKind {
    /// The reason starts with [`BLOCKED`]: the test waits on named work.
    Blocked,
    /// The data root, a corpus id or a file of one, or a golden derived below the data root, is
    /// absent on this host.
    Corpus,
    /// Anything else, such as a tool that is not installed.
    Other,
}

/// The prefix of a `SKIP` reason that makes it BLOCKED.
pub const BLOCKED: &str = "blocked: ";

/// Classifies a `SKIP` reason (module documentation).
pub fn skip_kind(reason: &str) -> SkipKind {
    if reason.starts_with(BLOCKED) {
        return SkipKind::Blocked;
    }
    let corpus = (reason.contains("corpus id `")
        && (reason.contains("` unavailable") || reason.contains("` has no file `")))
        || reason.starts_with("no data root")
        || reason.contains("is not below the data root");
    if corpus {
        SkipKind::Corpus
    } else {
        SkipKind::Other
    }
}

/// A harness result line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Ok,
    Failed,
    Ignored,
}

/// `(name, result)` of a `test <name> ... ok|FAILED|ignored` line.
pub fn harness(line: &str) -> Option<(&str, Harness)> {
    let (name, result) = line
        .trim_end_matches('\r')
        .strip_prefix("test ")?
        .rsplit_once(" ... ")?;
    let result = if result == "ok" {
        Harness::Ok
    } else if result == "FAILED" {
        Harness::Failed
    } else if result.starts_with("ignored") {
        Harness::Ignored
    } else {
        return None;
    };
    Some((name, result))
}

/// Joins distinct reasons with `; `, in first-seen order.
pub fn joined<S: AsRef<str>>(reasons: &[S]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for r in reasons {
        if !seen.contains(&r.as_ref()) {
            seen.push(r.as_ref());
        }
    }
    seen.join("; ")
}

/// Everything a run printed that `xtask ci` reads.
#[derive(Clone, Debug, Default)]
pub struct Report<'a> {
    /// Marker lines that count, in order: every `RAN` and `NOT_RUN` line, and each `SKIP` or
    /// `PENDING` line that names the test it was printed by and is not a helper self-check
    /// ([`read`]).
    pub markers: Vec<Marker<'a>>,
    /// Harness result lines, in order; names are as the harness prints them (`m3::t1_m3_...` or
    /// bare).
    pub results: Vec<(&'a str, Harness)>,
    /// `SKIP` and `PENDING` lines that name no test of the run, or another test than the one whose
    /// output they are in: `(name as printed, harness name of the output section it was in)`.
    pub orphans: Vec<(&'a str, Option<&'a str>)>,
    /// Lines that announce themselves as a marker and then do not parse as one, verbatim. Dropped
    /// silently, such a line would hide what it reported (for example a whitespace name,
    /// `SKIP <test> (esptool leg): ...`), so these fail the step as [`Report::orphans`] do.
    pub malformed: Vec<&'a str>,
}

/// One test of a run and what it proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestOutcome {
    /// Last `::` segment of the harness name.
    pub name: String,
    /// PASS, FAIL, BLOCKED, SKIPPED-CORPUS, SKIPPED, or NOT_RUN for an ignored test.
    pub status: Status,
    pub reason: String,
    /// `<leg>: <reason>` of each leg a passing test did not run (its `NOT_RUN` lines, and its skip
    /// reasons when it also printed `RAN`); empty otherwise.
    pub partial: Vec<String>,
}

/// Module path of the self-checks of `tests/milestones/common.rs`, which call the skip helpers on
/// purpose and are compiled into every milestone binary: a skip they print proves the helper, not
/// an absent corpus, so it is not read.
pub const SELF_CHECKS: &str = "common::self_checks::";

/// The last `::` segment of a harness name.
fn last_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// `NAME` of a `---- NAME stdout ----` section header of `--show-output`.
fn section(line: &str) -> Option<&str> {
    line.strip_prefix("---- ")?.strip_suffix(" stdout ----")
}

/// Reads the markers and harness lines of `stdout`.
///
/// Every well-formed `SKIP` and `PENDING` line counts, except those of the `common.rs` self-checks
/// ([`SELF_CHECKS`]). A `SKIP` or `PENDING` line whose name has no harness result in the same run,
/// is module-qualified, or differs from the test whose `--show-output` section it is in is an
/// orphan: it cannot be attributed, so it fails the step rather than
/// letting the test it came from read as a pass.
pub fn read(stdout: &str) -> Report<'_> {
    let mut report = Report::default();
    let mut raw: Vec<(Option<&str>, Marker<'_>)> = Vec::new();
    let mut current: Option<&str> = None;
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(name) = section(line) {
            current = Some(name);
        } else if let Some(found) = marker(line) {
            raw.push((current, found));
        } else if claims_marker(line) {
            report.malformed.push(line);
        } else if let Some(result) = harness(line) {
            report.results.push(result);
        } else if ["successes:", "failures:", "running "]
            .iter()
            .any(|head| line.starts_with(head))
        {
            // A section runs to the next section, list or test binary.
            current = None;
        }
    }
    for (within, found) in raw {
        let (Marker::Skip { test, .. } | Marker::Pending { test, .. }) = &found else {
            report.markers.push(found);
            continue;
        };
        let test: &str = test;
        let named: Vec<&str> = report
            .results
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| last_segment(name) == test)
            .collect();
        let self_check = match within {
            Some(section) => section.starts_with(SELF_CHECKS),
            None => !named.is_empty() && named.iter().all(|n| n.starts_with(SELF_CHECKS)),
        };
        let orphan = test.contains("::")
            || named.is_empty()
            || within.is_some_and(|section| last_segment(section) != test);
        if self_check && !orphan {
            continue;
        }
        if orphan {
            report.orphans.push((test, within));
        } else {
            report.markers.push(found);
        }
    }
    report
}

impl<'a> Report<'a> {
    /// Whether `test` printed a `RAN` line.
    pub fn ran(&self, test: &str) -> bool {
        self.markers
            .iter()
            .any(|m| matches!(m, Marker::Ran { test: t, .. } if *t == test))
    }

    /// `<leg>: <reason>` of every `NOT_RUN` line of `test`, then `skipped: <reasons>` when it also
    /// printed a `SKIP` or `PENDING` line (a partial skip), in first-seen order.
    pub fn partial_legs(&self, test: &str) -> Vec<String> {
        let mut legs: Vec<String> = Vec::new();
        for m in &self.markers {
            if let Marker::NotRun {
                test: t,
                leg,
                reason,
            } = m
                && *t == test
            {
                let line = format!("{leg}: {reason}");
                if !legs.contains(&line) {
                    legs.push(line);
                }
            }
        }
        let skipped = self.all_skip_reasons(test);
        if !skipped.is_empty() {
            legs.push(format!("skipped: {skipped}"));
        }
        legs
    }

    /// Tests that printed a `SKIP` or `PENDING` line, in first-seen order.
    pub fn skipped_tests(&self) -> Vec<&'a str> {
        let mut tests: Vec<&str> = Vec::new();
        for m in &self.markers {
            if let Marker::Skip { test, .. } | Marker::Pending { test, .. } = m
                && !tests.contains(test)
            {
                tests.push(test);
            }
        }
        tests
    }

    /// The status and reasons the `SKIP` and `PENDING` lines of `test` give it, or `None` when it
    /// printed none. BLOCKED outranks SKIPPED-CORPUS, which outranks SKIPPED, and the reasons are
    /// those of the winning kind (module documentation).
    pub fn skip_status(&self, test: &str) -> Option<(Status, String)> {
        let (mut blocked, mut corpus, mut other) = (Vec::new(), Vec::new(), Vec::new());
        for m in &self.markers {
            match m {
                Marker::Pending { test: t, reason } if *t == test => {
                    blocked.push(format!("awaiting golden approval: {reason}"));
                }
                Marker::Skip { test: t, reason } if *t == test => match skip_kind(reason) {
                    SkipKind::Blocked => blocked.push((*reason).to_string()),
                    SkipKind::Corpus => corpus.push((*reason).to_string()),
                    SkipKind::Other => other.push((*reason).to_string()),
                },
                _ => {}
            }
        }
        [
            (Status::Blocked, blocked),
            (Status::SkippedCorpus, corpus),
            (Status::Skipped, other),
        ]
        .into_iter()
        .find(|(_, reasons)| !reasons.is_empty())
        .map(|(status, reasons)| (status, joined(&reasons)))
    }

    /// Whether the run printed an orphan `SKIP` or `PENDING` line, which fails its step.
    pub fn has_orphans(&self) -> bool {
        !self.orphans.is_empty()
    }

    /// Whether the run printed a line that claims to be a marker and does not parse as one
    /// ([`Report::malformed`]), which fails its step for the same reason an orphan does.
    pub fn has_malformed(&self) -> bool {
        !self.malformed.is_empty()
    }

    /// Whether `test` skipped as a whole: it printed a `SKIP` or `PENDING` line and no `RAN` line.
    pub fn skipped_whole(&self, test: &str) -> bool {
        self.skip_status(test).is_some() && !self.ran(test)
    }

    /// All reasons of the `SKIP` and `PENDING` lines of `test`, of every kind.
    fn all_skip_reasons(&self, test: &str) -> String {
        let why: Vec<String> = self
            .markers
            .iter()
            .filter_map(|m| match m {
                Marker::Skip { test: t, reason } if *t == test => Some((*reason).to_string()),
                Marker::Pending { test: t, reason } if *t == test => {
                    Some(format!("awaiting golden approval: {reason}"))
                }
                _ => None,
            })
            .collect();
        joined(&why)
    }

    /// One sub-step `<step>.<test>` per test of `tests` that printed a `SKIP` or `PENDING` line.
    /// A test that skipped as a whole has the status of its lines (BLOCKED, SKIPPED-CORPUS or
    /// SKIPPED); a test that also printed `RAN` passed on the legs it ran, and what it skipped is
    /// a NOT_RUN sub-step with every reason.
    pub fn skip_sub_steps(&self, step: &str, tests: &[&str]) -> Vec<StepResult> {
        tests
            .iter()
            .filter_map(|test| {
                let (status, reason) = self.skip_status(test)?;
                let (status, reason) = if self.ran(test) {
                    (Status::NotRun, self.all_skip_reasons(test))
                } else {
                    (status, reason)
                };
                Some(StepResult::instant(
                    &format!("{step}.{test}"),
                    status,
                    reason,
                ))
            })
            .collect()
    }

    /// One FAIL sub-step `<step>.orphan-marker.<name>` per orphan `SKIP` or `PENDING` line
    /// ([`read`]).
    pub fn orphan_sub_steps(&self, step: &str) -> Vec<StepResult> {
        let mut names: Vec<&str> = Vec::new();
        for (name, _) in &self.orphans {
            if !names.contains(name) {
                names.push(name);
            }
        }
        names
            .into_iter()
            .map(|name| {
                let within: Vec<&str> = self
                    .orphans
                    .iter()
                    .filter(|(n, _)| *n == name)
                    .filter_map(|(_, within)| *within)
                    .collect();
                let place = if within.is_empty() {
                    String::new()
                } else {
                    format!(", printed by `{}`", joined(&within))
                };
                StepResult::instant(
                    &format!("{step}.orphan-marker.{name}"),
                    Status::Fail,
                    format!(
                        "a SKIP or PENDING line names `{name}`, which is not a test this run \
                         reported under that name{place}"
                    ),
                )
            })
            .collect()
    }

    /// Legs of the `NOT_RUN` lines, in first-seen order.
    pub fn not_run_legs(&self) -> Vec<&'a str> {
        let mut legs: Vec<&str> = Vec::new();
        for m in &self.markers {
            if let Marker::NotRun { leg, .. } = m
                && !legs.contains(leg)
            {
                legs.push(leg);
            }
        }
        legs
    }

    /// The merged reasons of the `NOT_RUN` lines of `leg`.
    pub fn not_run_reason(&self, leg: &str) -> Option<String> {
        let why: Vec<&str> = self
            .markers
            .iter()
            .filter_map(|m| match m {
                Marker::NotRun { leg: l, reason, .. } if *l == leg => Some(*reason),
                _ => None,
            })
            .collect();
        (!why.is_empty()).then(|| joined(&why))
    }

    /// One sub-step `<step>.<leg>` per `NOT_RUN` leg, NOT_RUN with its merged reasons.
    pub fn not_run_sub_steps(&self, step: &str) -> Vec<StepResult> {
        self.not_run_legs()
            .into_iter()
            .filter_map(|leg| {
                let why = self.not_run_reason(leg)?;
                Some(StepResult::instant(
                    &format!("{step}.{leg}"),
                    Status::NotRun,
                    why,
                ))
            })
            .collect()
    }

    /// Every sub-step of a test step: the orphan markers, the skipped tests, then the not-run
    /// legs.
    pub fn sub_steps(&self, step: &str) -> Vec<StepResult> {
        let mut subs = self.orphan_sub_steps(step);
        subs.extend(self.skip_sub_steps(step, &self.skipped_tests()));
        subs.extend(self.not_run_sub_steps(step));
        subs
    }

    /// What each test of a harness result line proved: FAIL for `FAILED`, NOT_RUN for `ignored`,
    /// and for `ok`:
    ///
    /// - FAIL when the test printed an orphan marker, or when the run has an orphan marker that no
    ///   `--show-output` section attributes, since it could have come from any test;
    /// - the status of its `SKIP` and `PENDING` lines when it printed no `RAN` line;
    /// - otherwise PASS, including a test that ran some legs and skipped others, whose skipped
    ///   part is a NOT_RUN sub-step ([`Report::skip_sub_steps`]).
    pub fn outcomes(&self) -> Vec<TestOutcome> {
        let unattributed = self.orphans.iter().find(|(_, within)| within.is_none());
        self.results
            .iter()
            .map(|(name, result)| {
                let last = last_segment(name);
                let own_orphan = self
                    .orphans
                    .iter()
                    .find(|(_, within)| *within == Some(*name));
                let (status, reason) = match (result, own_orphan, unattributed) {
                    (Harness::Failed, _, _) => (Status::Fail, "the test failed".to_string()),
                    (Harness::Ignored, _, _) => (Status::NotRun, "the test is ignored".to_string()),
                    (Harness::Ok, Some((other, _)), _) => (
                        Status::Fail,
                        format!("printed a SKIP or PENDING line naming `{other}`, not itself"),
                    ),
                    (Harness::Ok, None, Some((other, _))) => (
                        Status::Fail,
                        format!("the run printed an orphan SKIP or PENDING line naming `{other}`"),
                    ),
                    (Harness::Ok, None, None) if self.ran(last) => (Status::Pass, String::new()),
                    (Harness::Ok, None, None) => self
                        .skip_status(last)
                        .unwrap_or((Status::Pass, String::new())),
                };
                let partial = if status == Status::Pass {
                    self.partial_legs(last)
                } else {
                    Vec::new()
                };
                TestOutcome {
                    name: last.to_string(),
                    status,
                    reason,
                    partial,
                }
            })
            .collect()
    }

    /// Counts of the sub-steps of this report, for a step's note.
    pub fn note(&self) -> String {
        let subs = self.sub_steps("");
        let c = super::model::counts(&subs);
        let mut note = format!(
            "{} blocked, {} skipped-corpus, {} skipped, {} not run",
            c.blocked, c.skipped_corpus, c.skipped, c.not_run
        );
        if c.fail > 0 {
            note.push_str(&format!(", {} orphan markers", c.fail));
        }
        note
    }
}

/// Test names of `cargo test -- --list` output (lines `path::name: test`).
pub fn test_names(list: &str) -> Vec<&str> {
    list.lines()
        .filter_map(|line| line.trim().strip_suffix(": test"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `cargo test -- --show-output` prints, with lines of the kind `tests/milestones`
    /// prints (one blocked, one without a data root) and a passing test.
    const SHOWN: &str = "\
running 5 tests
test common::self_checks::common_corpus_helpers_skip_when_the_files_are_not_here ... ok
test t1_m3_hang_positive ... ok
test t1_m4_pk_boot_console ... ok
test t1_m3_hang_negative ... ok
test t1_m4_first_screen_frame ... ok

successes:

---- common::self_checks::common_corpus_helpers_skip_when_the_files_are_not_here stdout ----
SKIP common_corpus_helpers_skip_when_the_files_are_not_here: corpus id `pk` unavailable: gone

---- t1_m4_pk_boot_console stdout ----
SKIP t1_m4_pk_boot_console: blocked: the comparison is this package's to add

---- t1_m3_hang_negative stdout ----
SKIP t1_m3_hang_negative: corpus id `pk` unavailable: no data root: set PASSPORTSIM_DATA_ROOT to an absolute path; pemu-testkit resolves no host directory role of its own

---- t1_m4_first_screen_frame stdout ----
PENDING t1_m4_first_screen_frame: golden tests/golden/pk/first-screen.png needs a person's approval

successes:
    common::self_checks::common_corpus_helpers_skip_when_the_files_are_not_here
    t1_m3_hang_positive

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";

    fn outcome<'a>(all: &'a [TestOutcome], name: &str) -> &'a TestOutcome {
        all.iter().find(|o| o.name == name).expect(name)
    }

    #[test]
    fn markers_parse_and_other_lines_do_not() {
        assert_eq!(
            marker("RAN t1_m1_x node: equals native"),
            Some(Marker::Ran {
                test: "t1_m1_x",
                leg: "node"
            })
        );
        assert_eq!(
            marker("RAN t1_m3_x pk"),
            Some(Marker::Ran {
                test: "t1_m3_x",
                leg: "pk"
            })
        );
        assert_eq!(
            marker("SKIP t1_m4_x: blocked: needs GDMA: no DMA\r"),
            Some(Marker::Skip {
                test: "t1_m4_x",
                reason: "blocked: needs GDMA: no DMA"
            })
        );
        assert_eq!(
            marker("NOT_RUN t2_m3_x official-menu: joins at M5"),
            Some(Marker::NotRun {
                test: "t2_m3_x",
                leg: "official-menu",
                reason: "joins at M5"
            })
        );
        assert_eq!(
            marker("PENDING t1_m4_x: awaiting a person"),
            Some(Marker::Pending {
                test: "t1_m4_x",
                reason: "awaiting a person"
            })
        );
        for other in [
            "SKIP no colon here",
            "SKIP two words: reason",
            "NOT_RUN t1_x: no leg",
            "  SKIP t1_x: indented is a detail line, not a marker",
            "`pk` to `bsp_i2c`: 3 variants equal",
            "test t1_x ... ok",
        ] {
            assert_eq!(marker(other), None, "{other}");
        }
        assert_eq!(
            harness("test m3::t1_x ... ok"),
            Some(("m3::t1_x", Harness::Ok))
        );
        assert_eq!(
            harness("test t1_x ... FAILED"),
            Some(("t1_x", Harness::Failed))
        );
        assert_eq!(
            harness("test t1_x ... ignored, needs a device"),
            Some(("t1_x", Harness::Ignored))
        );
        assert_eq!(harness("test result: ok. 5 passed"), None);
    }

    #[test]
    fn skip_reasons_classify_as_blocked_corpus_or_other() {
        assert_eq!(skip_kind("blocked: not written yet"), SkipKind::Blocked);
        for corpus in [
            "corpus id `pk` unavailable: no data root: set PASSPORTSIM_DATA_ROOT",
            "corpus id `pk` has no file `FoloToy-AI-Passport.elf`",
            "no data root: set PASSPORTSIM_DATA_ROOT to an absolute path",
            "golden `pk.console.txt` is not below the data root in goldens/",
        ] {
            assert_eq!(skip_kind(corpus), SkipKind::Corpus, "{corpus}");
        }
        // The browser rows on a host with no corpus to give their image from
        // (`web/tests/preconditions.ts` `corpusAbsence`) are SKIPPED-CORPUS. Where the corpus is
        // and only the variable is unset, the reason starts with `blocked: ` and stays BLOCKED.
        for corpus in [
            "corpus id `pk` unavailable: no data root: set PASSPORTSIM_DATA_ROOT to an absolute \
             path; so no `pk` image can be given (PEMU_E2E_IMAGE_PK) and the pk boot does not \
             run on this host (the corpus is macOS-only)",
            "corpus id `demo` unavailable: no `corpus/demo` directory below the data root; so no \
             `demo` image can be given (PEMU_E2E_IMAGE_DEMO) and the Wi-Fi scan does not run on \
             this host",
        ] {
            assert_eq!(skip_kind(corpus), SkipKind::Corpus, "{corpus}");
        }
        assert_eq!(
            skip_kind(
                "blocked: the pk boot: no `pk` image given (set PEMU_E2E_IMAGE_PK to a merged \
                 bin), and the firmware result waits on the pk boot"
            ),
            SkipKind::Blocked
        );
        for other in [
            "no riscv32-esp-elf-objdump (set PASSPORTSIM_OBJDUMP)",
            "not blocked: later",
            "Blocked: the prefix is lower case",
            "not written yet on a real instance",
        ] {
            assert_eq!(skip_kind(other), SkipKind::Other, "{other}");
        }
    }

    /// A planted test that prints a `blocked: ` `SKIP` line is BLOCKED with its reason; a corpus
    /// test with no data root is SKIPPED-CORPUS; a passing test stays PASS.
    #[test]
    fn a_blocked_skip_line_blocks_a_missing_corpus_is_skipped_corpus_and_a_pass_stays_pass() {
        let report = read(SHOWN);
        let all = report.outcomes();
        assert_eq!(all.len(), 5);

        let blocked = outcome(&all, "t1_m4_pk_boot_console");
        assert_eq!(blocked.status, Status::Blocked);
        assert!(
            blocked.reason.starts_with("blocked: the comparison"),
            "{}",
            blocked.reason
        );

        let corpus = outcome(&all, "t1_m3_hang_negative");
        assert_eq!(corpus.status, Status::SkippedCorpus);
        assert!(corpus.reason.contains("no data root"), "{}", corpus.reason);

        let pending = outcome(&all, "t1_m4_first_screen_frame");
        assert_eq!(pending.status, Status::Blocked);
        assert!(
            pending
                .reason
                .starts_with("awaiting golden approval: golden")
        );

        assert_eq!(outcome(&all, "t1_m3_hang_positive").status, Status::Pass);
        // The helper self-check skips on purpose under its own name, and proves the helper.
        let helper = outcome(
            &all,
            "common_corpus_helpers_skip_when_the_files_are_not_here",
        );
        assert_eq!(helper.status, Status::Pass);

        let subs = report.sub_steps("t1-tests");
        let names: Vec<(&str, Status)> = subs.iter().map(|s| (s.name.as_str(), s.status)).collect();
        assert_eq!(
            names,
            [
                ("t1-tests.t1_m4_pk_boot_console", Status::Blocked),
                ("t1-tests.t1_m3_hang_negative", Status::SkippedCorpus),
                ("t1-tests.t1_m4_first_screen_frame", Status::Blocked),
            ]
        );
        assert_eq!(
            report.note(),
            "2 blocked, 1 skipped-corpus, 0 skipped, 0 not run"
        );
    }

    /// Every well-formed SKIP line counts, whatever the test is named; only the
    /// `common.rs` self-checks are not read.
    #[test]
    fn a_skip_of_a_test_without_a_tier_prefix_is_skipped_and_only_self_checks_are_not_read() {
        let out = "\
test payload::tests::this_builds_embedded_payload_recomputes_to_its_recorded_digest ... ok
test common::self_checks::common_corpus_helpers_skip_when_the_files_are_not_here ... ok
test corpus_passport_images_plan_to_three_writes_clear_of_nvs_and_cardid ... ok

successes:

---- payload::tests::this_builds_embedded_payload_recomputes_to_its_recorded_digest stdout ----
SKIP this_builds_embedded_payload_recomputes_to_its_recorded_digest: no payload was embedded

---- common::self_checks::common_corpus_helpers_skip_when_the_files_are_not_here stdout ----
SKIP common_corpus_helpers_skip_when_the_files_are_not_here: corpus id `pk` unavailable: gone

---- corpus_passport_images_plan_to_three_writes_clear_of_nvs_and_cardid stdout ----
SKIP corpus_passport_images_plan_to_three_writes_clear_of_nvs_and_cardid: corpus id `pk` unavailable: no corpus manifest
";
        let report = read(out);
        let subs: Vec<(String, Status)> = report
            .sub_steps("test")
            .into_iter()
            .map(|s| (s.name, s.status))
            .collect();
        assert_eq!(
            subs,
            [
                (
                    "test.this_builds_embedded_payload_recomputes_to_its_recorded_digest".into(),
                    Status::Skipped
                ),
                (
                    "test.corpus_passport_images_plan_to_three_writes_clear_of_nvs_and_cardid"
                        .into(),
                    Status::SkippedCorpus
                ),
            ]
        );
        assert!(!report.has_orphans());
    }

    /// A marker that names no test of the run, a module-qualified name, or another test
    /// than the one whose output it is in, is an orphan that fails the step and its test.
    #[test]
    fn an_orphan_marker_is_a_failed_sub_step_and_fails_the_test_that_printed_it() {
        let out = "\
test t1_m4_x ... ok
test t1_m4_y ... ok
test t1_m4_z ... ok

successes:

---- t1_m4_x stdout ----
SKIP t1_m4_not_in_this_run: blocked: waits

---- t1_m4_y stdout ----
SKIP m4::t1_m4_y: blocked: waits

---- t1_m4_z stdout ----
PENDING t1_m4_y: golden awaits approval
";
        let report = read(out);
        assert!(report.has_orphans());
        let subs = report.sub_steps("t1-tests");
        let names: Vec<(&str, Status)> = subs.iter().map(|s| (s.name.as_str(), s.status)).collect();
        assert_eq!(
            names,
            [
                ("t1-tests.orphan-marker.t1_m4_not_in_this_run", Status::Fail),
                ("t1-tests.orphan-marker.m4::t1_m4_y", Status::Fail),
                ("t1-tests.orphan-marker.t1_m4_y", Status::Fail),
            ]
        );
        assert!(
            subs[2].reason.contains("printed by `t1_m4_z`"),
            "{}",
            subs[2].reason
        );
        let all = report.outcomes();
        for test in ["t1_m4_x", "t1_m4_y", "t1_m4_z"] {
            assert_eq!(outcome(&all, test).status, Status::Fail, "{test}");
        }
        // Without sections nothing says which test printed it, so no test of the run passes.
        let bare = "test t1_m4_x ... ok\nSKIP t1_m4_gone: blocked: waits\n";
        let all = read(bare).outcomes();
        assert_eq!(outcome(&all, "t1_m4_x").status, Status::Fail);
    }

    /// RAN plus SKIP is PASS, with the skipped part a NOT_RUN sub-step; SKIP alone
    /// keeps its status.
    #[test]
    fn a_test_that_ran_some_legs_passes_and_lists_the_skipped_ones_as_not_run() {
        let out = "\
test t1_m1_rom_banner ... ok
test t1_m1_strict ... ok

successes:

---- t1_m1_rom_banner stdout ----
RAN t1_m1_rom_banner pk
SKIP t1_m1_rom_banner: corpus id `goldminer` unavailable: absent

---- t1_m1_strict stdout ----
SKIP t1_m1_strict: corpus id `pk` unavailable: absent
";
        let report = read(out);
        let all = report.outcomes();
        assert_eq!(outcome(&all, "t1_m1_rom_banner").status, Status::Pass);
        assert_eq!(outcome(&all, "t1_m1_strict").status, Status::SkippedCorpus);
        let subs = report.sub_steps("t1-tests");
        assert_eq!(subs[0].name, "t1-tests.t1_m1_rom_banner");
        assert_eq!(subs[0].status, Status::NotRun);
        assert!(subs[0].reason.contains("`goldminer` unavailable"));
        assert_eq!(subs[1].status, Status::SkippedCorpus);
    }

    #[test]
    fn blocked_outranks_corpus() {
        let out = "SKIP t1_m4_x: corpus id `pk` unavailable: absent\n\
                   SKIP t1_m4_x: blocked: waits on GDMA\n\
                   SKIP t1_m4_x: blocked: waits on GDMA\n\
                   test t1_m4_x ... ok\n\
                   test t1_m4_y ... FAILED\n\
                   SKIP t1_m4_y: blocked: waits\n\
                   test t1_m4_z ... ignored\n";
        let report = read(out);
        let (status, reason) = report.skip_status("t1_m4_x").unwrap();
        assert_eq!(status, Status::Blocked);
        assert_eq!(reason, "blocked: waits on GDMA");
        let all = report.outcomes();
        assert_eq!(outcome(&all, "t1_m4_y").status, Status::Fail);
        assert_eq!(outcome(&all, "t1_m4_z").status, Status::NotRun);
    }
}
