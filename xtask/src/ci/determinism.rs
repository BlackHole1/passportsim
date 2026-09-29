//! The determinism steps of `xtask ci t1` and `t2` (`docs/ARCHITECTURE.md`, Determinism).
//!
//! Each step runs named milestone tests of M1 and M3 (determinism, restore equivalence) with the
//! corpus, and turns what they print into a step status. The lines are read by `outcome.rs`, the
//! one parser every test step of `xtask ci` shares, so a line means the same here as in the tier's
//! `t1-tests` step:
//!
//! - a test prints `RAN <test> <image or leg>` for what it ran, and `SKIP <test>: <reason>` for an
//!   image it could not find. A test is skipped only if it skipped and nothing of it ran; a step
//!   all of whose tests skipped takes the strongest of their statuses (BLOCKED, then
//!   SKIPPED-CORPUS, then SKIPPED). Each test that printed a SKIP or PENDING line is recorded as
//!   the sub-step `<step>.<test>`, with the status `outcome.rs` gives its reasons (SKIPPED-CORPUS
//!   for an absent data root or corpus id);
//! - a comparison that could not run, or ran over nothing (constant frames, no PCM, a window cut
//!   short, a profile that selects nothing yet), prints `NOT_RUN <test> <leg>: <reason>`, and each
//!   leg is recorded as the sub-step `<step>.<leg>`, NOT_RUN with its reasons, so the receipt's
//!   not-run count is honest;
//! - on `node-jsc-parity` the step is NOT_RUN unless a Node leg ran, because Node is the cross-host
//!   engine; a missing `jsc` leaves the step PASS and records `node-jsc-parity.jsc`
//!   NOT_RUN. Its note names the module the legs run;
//! - a failing test fails the step.
//!
//! The tests these steps own are skipped by the tier's `t1-tests` or `t2-tests` step, so nothing
//! runs twice ([`skip_args`]).

use std::path::{Path, PathBuf};

use super::model::{Status, StepResult};
use super::outcome::{self, Marker};
use super::runner::Ctx;

/// One determinism step.
pub struct Row {
    /// Step name in the tier.
    pub step: &'static str,
    /// `t1` or `t2`.
    pub tier: &'static str,
    /// Exact test names; the prefix `t<n>_m<m>_` names the test binary `m<m>`.
    pub tests: &'static [&'static str],
    /// Whether the step builds the wasm32 leg first and hands it to the tests.
    pub wasm: bool,
}

/// Every determinism step, in tier order.
pub const ROWS: &[Row] = &[
    Row {
        step: "determinism-short",
        tier: "t1",
        tests: &["t1_m1_determinism_at_entry", "t1_m3_determinism_bsp_i2c"],
        wasm: false,
    },
    Row {
        step: "node-jsc-parity",
        tier: "t1",
        tests: &[
            "t1_m1_native_equals_node_and_jsc",
            "t1_m3_native_equals_node_and_jsc",
            "t1_m6_i2s_eof_spacing_and_pcm_hash_parity",
        ],
        wasm: true,
    },
    Row {
        step: "restore-equivalence-short",
        tier: "t1",
        tests: &["t1_m3_restore_equivalence_short"],
        wasm: false,
    },
    Row {
        step: "determinism-long",
        tier: "t2",
        tests: &["t2_m3_slice_invariance_both_profiles"],
        wasm: false,
    },
    Row {
        step: "snapshot-anywhere",
        tier: "t2",
        tests: &["t2_m3_snapshot_anywhere_fresh_process"],
        wasm: false,
    },
    Row {
        step: "restore-equivalence-all",
        tier: "t2",
        tests: &["t2_m3_restore_equivalence_all_instants"],
        wasm: false,
    },
];

/// The variable the wasm legs read (`tests/milestones/determinism.rs`).
pub const WASM_ENV: &str = "PEMU_DET_WASM";

/// `--skip <test>` for every test a determinism step of `tier` owns, for the tier's prefixed run.
pub fn skip_args(tier: &str) -> Vec<String> {
    ROWS.iter()
        .filter(|row| row.tier == tier)
        .flat_map(|row| row.tests.iter())
        .flat_map(|test| ["--skip".to_string(), (*test).to_string()])
        .collect()
}

/// The cargo arguments that run `row`'s tests, with the profile every T1 and T2 test run uses
/// (`tiers::test_profile`).
pub fn test_args(row: &Row) -> Vec<String> {
    let mut args: Vec<String> = ["test"].map(String::from).to_vec();
    args.extend(
        super::tiers::test_profile(row.tier)
            .iter()
            .map(|a| a.to_string()),
    );
    // `--workspace`, not `-p pemu-milestones`: the prefixed run builds with the workspace's feature
    // set, and the same set here reuses that build.
    args.push("--workspace".to_string());
    let mut bins: Vec<&str> = row
        .tests
        .iter()
        .filter_map(|t| t.split('_').nth(1))
        .collect();
    bins.sort_unstable();
    bins.dedup();
    for bin in bins {
        args.extend(["--test".to_string(), bin.to_string()]);
    }
    args.push("--".to_string());
    args.push("--exact".to_string());
    args.extend(row.tests.iter().map(|t| (*t).to_string()));
    args.push("--show-output".to_string());
    args
}

/// What a finished row whose tests passed reports: its own status and note, and one sub-step per
/// leg that did not run or test that skipped, so each is visible in the receipt and counted
/// (module documentation).
pub struct Verdict {
    pub status: Status,
    pub note: String,
    pub sub_steps: Vec<StepResult>,
}

/// The note of the wasm row: where its wasm32 module comes from. The legs boot through the ABI the
/// Worker uses (`crates/pemu-wasm/js/abi_parity.cjs`), not a module of their own.
pub const WASM_SOURCE: &str =
    "pemu_wasm ABI (pemu_new, pemu_load, pemu_build, pemu_call, pemu_run)";

/// Reads the `RAN`, `SKIP`, `PENDING` and `NOT_RUN` lines of a row's tests through the one parser
/// of `outcome.rs` (module documentation).
pub fn verdict(row: &Row, stdout: &str) -> Verdict {
    let report = outcome::read(stdout);
    let ran = report
        .markers
        .iter()
        .filter(|m| matches!(m, Marker::Ran { .. }))
        .count();
    let mut sub_steps = report.orphan_sub_steps(row.step);
    sub_steps.extend(report.skip_sub_steps(row.step, row.tests));
    let legs = report.not_run_legs();
    sub_steps.extend(report.not_run_sub_steps(row.step));
    // An orphan SKIP or PENDING line fails the step.
    if report.has_orphans() {
        return Verdict {
            status: Status::Fail,
            note: "a SKIP or PENDING line names no test of this run".to_string(),
            sub_steps,
        };
    }
    // A test skipped only when it skipped and nothing of it ran.
    if row.tests.iter().all(|test| report.skipped_whole(test)) {
        let statuses: Vec<(Status, String)> = row
            .tests
            .iter()
            .filter_map(|test| report.skip_status(test))
            .collect();
        let worst = [Status::Blocked, Status::SkippedCorpus, Status::Skipped]
            .into_iter()
            .find(|want| statuses.iter().any(|(status, _)| status == want))
            .unwrap_or(Status::Skipped);
        let why = statuses
            .iter()
            .find(|(status, _)| *status == worst)
            .map_or("", |(_, why)| why.as_str());
        let label = match worst {
            Status::Blocked => "blocked",
            Status::SkippedCorpus => "corpus absent",
            _ => "skipped",
        };
        return Verdict {
            status: worst,
            note: format!("{label}: {why}"),
            sub_steps,
        };
    }
    let (status, note) = if row.wasm {
        let engines = |name: &str| {
            report
                .markers
                .iter()
                .any(|m| matches!(m, Marker::Ran { leg, .. } if *leg == name))
        };
        if legs.contains(&"node") || !engines("node") {
            let why = report
                .not_run_reason("node")
                .unwrap_or_else(|| "no node leg reported".to_string());
            (
                Status::NotRun,
                format!("node not run: {why}; {WASM_SOURCE}"),
            )
        } else {
            let jsc = if legs.contains(&"jsc") {
                "jsc not run (sub-step)"
            } else {
                "jsc equal"
            };
            (
                Status::Pass,
                format!("{ran} legs, node equal, {jsc}; {WASM_SOURCE}"),
            )
        }
    } else {
        let mut note = format!("{} tests, {ran} runs", row.tests.len());
        if !sub_steps.is_empty() {
            note.push_str(&format!(
                "; {} sub-steps not run, blocked or skipped",
                sub_steps.len()
            ));
        }
        (Status::Pass, note)
    };
    Verdict {
        status,
        note,
        sub_steps,
    }
}

/// The wasm32 core `pemu-wasm` builds to under `target`, the module the web bundle ships.
pub fn wasm_module(target: &Path) -> PathBuf {
    target.join("wasm32-unknown-unknown/wasm-release/pemu_wasm.wasm")
}

/// Why [`build_wasm_core`] has no module.
pub enum CoreBuild {
    /// The `wasm32-unknown-unknown` target is not installed.
    NoTarget,
    /// The build failed; the command's outcome.
    Failed(super::runner::Exec),
}

/// Builds the wasm32 core with the `wasm-release` profile, logged as `log_name`, and returns the
/// module path ([`wasm_module`]). The node parity row and the Playwright rows serve this module.
pub fn build_wasm_core(ctx: &Ctx, log_name: &str) -> Result<PathBuf, CoreBuild> {
    let build = ctx.command(
        "cargo",
        &[
            "build",
            "-p",
            "pemu-wasm",
            "--lib",
            "--target",
            "wasm32-unknown-unknown",
            "--profile",
            "wasm-release",
        ],
    );
    let exec = ctx.exec(log_name, build, false);
    if !exec.success {
        let log = std::fs::read_to_string(&exec.log).unwrap_or_default();
        return Err(if log.contains("target may not be installed") {
            CoreBuild::NoTarget
        } else {
            CoreBuild::Failed(exec)
        });
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.root.join("target"));
    Ok(wasm_module(&ctx.root.join(target)))
}

/// Runs `row` as a step of `ctx` (module documentation). `data_root` sets the corpus root on the
/// test command, as the tier's prefixed test step does.
pub fn run(ctx: &mut Ctx, row: &Row, data_root: impl Fn(&mut std::process::Command)) {
    let mut cmd_args = test_args(row);
    let mut wasm_path = None;
    if row.wasm {
        match build_wasm_core(ctx, &format!("{}-build", row.step)) {
            Ok(path) => wasm_path = Some(path),
            Err(exec) => {
                let step = match exec {
                    CoreBuild::NoTarget => StepResult::instant(
                        row.step,
                        Status::NotRun,
                        "the wasm32-unknown-unknown target is not installed",
                    ),
                    CoreBuild::Failed(exec) => ctx.finish(row.step, &exec, ""),
                };
                return ctx.record(step);
            }
        }
    }
    let args: Vec<&str> = cmd_args.iter_mut().map(|a| a.as_str()).collect();
    let mut cmd = ctx.command("cargo", &args);
    data_root(&mut cmd);
    if let Some(path) = &wasm_path {
        cmd.env(WASM_ENV, path);
    }
    let exec = ctx.exec(row.step, cmd, true);
    let _ = std::fs::write(exec.log.with_extension("stdout.log"), &exec.stdout);
    ctx.tests.extend(
        outcome::read(&exec.stdout)
            .outcomes()
            .into_iter()
            .filter(|t| row.tests.contains(&t.name.as_str())),
    );
    if !exec.success {
        let step = ctx.finish(row.step, &exec, "");
        return ctx.record(step);
    }
    let found = verdict(row, &exec.stdout);
    ctx.record(StepResult {
        duration: exec.duration,
        ..StepResult::instant(row.step, found.status, found.note)
    });
    for sub in found.sub_steps {
        ctx.record(sub);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(step: &str) -> &'static Row {
        ROWS.iter().find(|r| r.step == step).unwrap()
    }

    fn sub<'a>(v: &'a Verdict, name: &str) -> Option<&'a StepResult> {
        v.sub_steps.iter().find(|s| s.name == name)
    }

    #[test]
    fn every_row_names_tests_of_its_tier_and_the_prefixed_run_skips_them() {
        for r in ROWS {
            for t in r.tests {
                assert!(t.starts_with(&format!("{}_m", r.tier)), "{t}");
            }
        }
        let skips = skip_args("t1");
        assert_eq!(skips.len(), 2 * 6);
        assert!(skips.contains(&"t1_m3_restore_equivalence_short".to_string()));
        assert!(!skip_args("t2").iter().any(|a| a.starts_with("t1_")));
    }

    #[test]
    fn test_args_name_each_binary_once_and_the_exact_tests() {
        let args = test_args(row("node-jsc-parity"));
        let joined = args.join(" ");
        assert!(
            joined.starts_with(
                "test --profile ci-test --workspace --test m1 --test m3 --test m6 -- --exact "
            ),
            "{joined}"
        );
        assert!(joined.ends_with("t1_m6_i2s_eof_spacing_and_pcm_hash_parity --show-output"));
    }

    #[test]
    fn a_missing_node_leg_is_not_run_and_a_missing_jsc_leg_is_a_not_run_sub_step() {
        let parity = row("node-jsc-parity");
        let no_node = "NOT_RUN t1_m1_native_equals_node_and_jsc node: node is not on PATH\n\
                       RAN t1_m1_native_equals_node_and_jsc jsc: x equals native\n";
        let v = verdict(parity, no_node);
        assert_eq!(v.status, Status::NotRun);
        assert!(v.note.contains("node is not on PATH"), "{}", v.note);
        assert_eq!(
            sub(&v, "node-jsc-parity.node").unwrap().status,
            Status::NotRun
        );

        let no_jsc = "RAN t1_m1_native_equals_node_and_jsc node: x equals native\n\
                      NOT_RUN t1_m1_native_equals_node_and_jsc jsc: jsc missing at the system path: broken host\n";
        let v = verdict(parity, no_jsc);
        assert_eq!(v.status, Status::Pass);
        assert!(v.note.contains(WASM_SOURCE), "{}", v.note);
        let jsc = sub(&v, "node-jsc-parity.jsc").expect("a receipt-visible sub-step");
        assert_eq!(jsc.status, Status::NotRun);
        assert!(jsc.reason.contains("broken host"));

        let both = "RAN a node: x\nRAN a jsc: x\n";
        let v = verdict(parity, both);
        assert_eq!(v.status, Status::Pass);
        assert!(v.sub_steps.is_empty());
        assert_eq!(
            verdict(parity, "").status,
            Status::NotRun,
            "silence is no pass"
        );
    }

    #[test]
    fn one_image_skipped_while_others_ran_is_not_a_skipped_test() {
        let short = row("determinism-short");
        let all = "test t1_m1_determinism_at_entry ... ok\n\
                   test t1_m3_determinism_bsp_i2c ... ok\n\
                   SKIP t1_m1_determinism_at_entry: no data root: set PASSPORTSIM_DATA_ROOT\n\
                   SKIP t1_m3_determinism_bsp_i2c: corpus id `pk` unavailable: no data root\n";
        let v = verdict(short, all);
        assert_eq!(v.status, Status::SkippedCorpus, "{}", v.note);
        assert!(
            v.note.starts_with("corpus absent: no data root"),
            "{}",
            v.note
        );
        assert!(
            v.sub_steps
                .iter()
                .all(|s| s.status == Status::SkippedCorpus)
        );
        let part = "test t1_m1_determinism_at_entry ... ok\n\
                   test t1_m3_determinism_bsp_i2c ... ok\n\
                   SKIP t1_m1_determinism_at_entry: corpus id `official` unavailable: absent\n\
                    RAN t1_m1_determinism_at_entry pk\n\
                    SKIP t1_m3_determinism_bsp_i2c: corpus id `pk` unavailable: absent\n";
        let v = verdict(short, part);
        assert_eq!(v.status, Status::Pass, "{}", v.note);
        // It ran `pk`, so the skipped `official` leg is a NOT_RUN sub-step.
        let s = sub(&v, "determinism-short.t1_m1_determinism_at_entry").unwrap();
        assert_eq!(s.status, Status::NotRun);
        assert!(s.reason.contains("`official` unavailable"));
        let s = sub(&v, "determinism-short.t1_m3_determinism_bsp_i2c").unwrap();
        assert_eq!(s.status, Status::SkippedCorpus);
        let orphan = format!("{part}SKIP t1_m9_elsewhere: blocked: waits\n");
        let v = verdict(short, &orphan);
        assert_eq!(v.status, Status::Fail, "{}", v.note);
        assert_eq!(
            sub(&v, "determinism-short.orphan-marker.t1_m9_elsewhere")
                .unwrap()
                .status,
            Status::Fail
        );
    }

    #[test]
    fn a_blocked_or_pending_determinism_test_blocks_the_step_and_a_tool_skip_is_skipped() {
        let short = row("determinism-short");
        let out = "test t1_m1_determinism_at_entry ... ok\n\
                   test t1_m3_determinism_bsp_i2c ... ok\n\
                   SKIP t1_m1_determinism_at_entry: corpus id `pk` unavailable: absent\n\
                   PENDING t1_m3_determinism_bsp_i2c: golden `pk.trace` awaits approval\n";
        let v = verdict(short, out);
        assert_eq!(v.status, Status::Blocked, "{}", v.note);
        assert!(v.note.contains("awaiting golden approval"), "{}", v.note);
        let tool = "test t1_m1_determinism_at_entry ... ok\n\
                    test t1_m3_determinism_bsp_i2c ... ok\n\
                    SKIP t1_m1_determinism_at_entry: no objdump\n\
                    SKIP t1_m3_determinism_bsp_i2c: no objdump\n";
        assert_eq!(verdict(short, tool).status, Status::Skipped);
    }

    #[test]
    fn vacuous_legs_are_not_run_sub_steps_with_their_reasons_merged() {
        let all = row("restore-equivalence-all");
        let out = "RAN t2_m3_restore_equivalence_all_instants pk-rom\n\
                   NOT_RUN t2_m3_restore_equivalence_all_instants frame-constant: `pk` rom: none\n\
                   NOT_RUN t2_m3_restore_equivalence_all_instants frame-constant: `official` boot: none\n\
                   NOT_RUN t2_m3_restore_equivalence_all_instants official-menu: M5\n";
        let v = verdict(all, out);
        assert_eq!(v.status, Status::Pass);
        let frame = sub(&v, "restore-equivalence-all.frame-constant").unwrap();
        assert_eq!(frame.status, Status::NotRun);
        assert!(frame.reason.contains("`pk` rom") && frame.reason.contains("`official` boot"));
        assert_eq!(
            sub(&v, "restore-equivalence-all.official-menu")
                .unwrap()
                .status,
            Status::NotRun
        );
        assert_eq!(v.sub_steps.len(), 2);
    }
}
