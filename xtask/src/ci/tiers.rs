//! Step lists of `xtask ci t0`, `t1` and `t2`.
//!
//! A step whose input is absent on this host reports SKIPPED with the reason. A failing step
//! never stops later steps.

use std::path::{Path, PathBuf};

use super::corpus;
use super::determinism::{self, CoreBuild};
use super::model::{Status, StepResult};
use super::outcome::{self, TestOutcome};
use super::runner::Ctx;
use super::web;

/// Core crates, which must build for `wasm32-unknown-unknown` (`docs/ARCHITECTURE.md`, Layering).
pub const CORE_CRATES: &[&str] = &[
    "pemu-core",
    "pemu-rv32",
    "pemu-soc-c3",
    "pemu-board",
    "pemu-loader",
    "pemu-hle",
    "pemu-radio",
    "pemu-machine",
    "pemu-introspect",
    "pemu-api",
    "pemu-planner",
    "pemu-wasm",
];

/// Hash file of the hashed secret rules (`docs/secrets.md`); only its existence is checked here.
const HASH_FILE: &str = ".config/passportsim/secrets-check.toml";

/// The Windows targets the `w0-*` steps cross-check from a macOS host.
const W0_TARGETS: &[&str] = &["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"];

/// The cargo profile flags of every test run of `tier`: T0 stays a debug build, so it is fast and
/// shares the build of its `clippy` and `build` steps; T1 and T2 build their tests with the
/// `ci-test` profile (release optimization and no debug assertions, without fat LTO), because
/// several tests of those tiers check release figures or need release speed (the esptool
/// `write_flash` runs, the 50 ms boot-cache restore). Every test command of a tier uses the same
/// profile and `--workspace` feature set, so the tier builds its tests once.
pub fn test_profile(tier: &str) -> &'static [&'static str] {
    if tier == "t0" {
        &[]
    } else {
        &["--profile", "ci-test"]
    }
}

/// The test that replays the record of the Playwright smoke and so runs after it
/// ([`browser_journal_replay`]) instead of in the tier's prefixed run.
pub const REPLAY_TEST: &str = "t1_m9b_browser_journal_replays_natively";
/// Step name of the native replay.
pub const REPLAY_STEP: &str = "browser-journal-replay";
/// The variable naming the record `web/tests/m9.spec.ts` writes and the replay test reads.
pub const BROWSER_RECORD_ENV: &str = "PEMU_BROWSER_RECORD";
/// File name of the record an engine's Playwright run writes in the tier's log directory. Every
/// engine gets its own, so the WebKit run of T2 never overwrites the Chromium record or falls back
/// to the spec's shared temp-directory default.
pub fn browser_record_file(engine: &str) -> String {
    format!("browser-record-{engine}.json")
}
/// The variable naming the wasm core the Playwright specs serve and the replay test hashes.
pub const CORE_ENV: &str = "PEMU_E2E_CORE";

/// The parts of T0, which CI runs as parallel jobs. A run with no `--group` runs them all, in this
/// order.
pub const T0_GROUPS: [&str; 4] = ["checks", "test", "package", "browsers"];

/// Runs the steps of `tier` (`t0`, `t1` or `t2`); `groups` selects the parts of T0.
pub fn run(ctx: &mut Ctx, tier: &str, groups: &[String]) {
    match tier {
        "t0" => t0(ctx, groups),
        "t1" => t1(ctx),
        _ => t2(ctx),
    }
}

fn cargo(ctx: &mut Ctx, name: &str, args: &[&str], note: &str) {
    let cmd = ctx.command("cargo", args);
    ctx.run_step(name, cmd, note);
}

fn skipped(ctx: &mut Ctx, name: &str, reason: &str) {
    ctx.record(StepResult::instant(name, Status::Skipped, reason));
}

fn hash_file_exists() -> bool {
    crate::hostdirs::home().is_ok_and(|home| home.join(HASH_FILE).exists())
}

/// T0: needs no corpus or device data.
fn t0(ctx: &mut Ctx, groups: &[String]) {
    let runs = |group: &str| groups.is_empty() || groups.iter().any(|g| g == group);
    if runs("checks") {
        t0_checks(ctx);
    }
    if runs("test") {
        t0_test(ctx);
    }
    if runs("package") {
        let cmd = package_test_command(ctx);
        ctx.test_step(PACKAGE_STEP, cmd, "the release package of this host");
    }
    // T1 and T2, where the macOS browser rows run, are macOS-only, so a Windows T0 runs the
    // Windows rows. A macOS T0 runs no browser.
    if runs("browsers") && std::env::consts::OS == "windows" {
        playwright(
            ctx,
            WINDOWS_BROWSERS_STEP,
            &WINDOWS_ROWS,
            None,
            Corpus::Withheld,
        );
    }
}

/// The lints, the generated-file and policy checks, and the builds that are not tests.
fn t0_checks(ctx: &mut Ctx) {
    cargo(ctx, "fmt", &["fmt", "--all", "--check"], "");
    let clippy = [
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ];
    cargo(ctx, "clippy", &clippy, "");
    // The workspace run leaves `pemu-planner`'s `device` feature off, so its code is linted
    // here separately.
    let clippy_device = [
        "clippy",
        "-p",
        "pemu-planner",
        "--features",
        "device",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ];
    cargo(ctx, "clippy-planner-device", &clippy_device, "");
    // `pemu-host`'s device code (the flow adapters, the full backup, the boot-check command
    // path) is behind the same feature and compiled nowhere else, so it is linted and tested here
    // too. Its unit tests open no port; they enumerate through IOKit or SetupAPI, which opens
    // nothing, and the only process one spawns is this test binary again, on Windows, to count
    // the handles around an enumeration (`platform::serial`).
    let clippy_host_device = [
        "clippy",
        "-p",
        "pemu-host",
        "--features",
        "device",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ];
    cargo(ctx, "clippy-host-device", &clippy_host_device, "");
    // The one step that needs the network: the advisory database is fetched from RustSec.
    ctx.run_step_retrying_network("deny", "cargo", &["deny", "check"], "");
    ctx.xtask_step("layering", &["layering"], "");
    ctx.xtask_step("provenance", &["provenance"], "");
    let rules = if hash_file_exists() {
        "pattern and hashed rules"
    } else {
        "pattern rules only (no local hash file)"
    };
    ctx.xtask_step("secrets-check", &["secrets-check"], rules);
    ctx.xtask_step("codegen-check", &["codegen", "--check"], "");
    ctx.xtask_step("docs-check", &["docs", "--check"], "");
    ctx.xtask_step("mcp-size", &["mcp-size"], "");
    // `xtask` is left out of the debug build because it is this very process: a workspace build
    // unifies features differently from `cargo xtask` and relinks `target/debug/xtask`, which
    // Windows refuses for a running image (`failed to remove file ...\xtask.exe (os error 5)`)
    // and macOS replaces under its own feet. `cargo xtask` having started proves the debug build,
    // `clippy` its lints, and `check-release` the release settings.
    cargo(
        ctx,
        "build",
        &["build", "--workspace", "--exclude", "xtask"],
        "xtask excluded: it is the running process",
    );
    // The release settings, checked for the whole workspace: code under
    // `cfg(debug_assertions)` differs between the profiles. The shipped binary and wasm core are
    // built, with the release workflow's flags, by the `package-tests` step.
    cargo(
        ctx,
        "check-release",
        &["check", "--workspace", "--release"],
        "the workspace with the release settings",
    );
    let mut wasm = vec!["check", "--target", "wasm32-unknown-unknown"];
    for krate in CORE_CRATES {
        wasm.extend(["-p", krate]);
    }
    let note = format!("{} core crates", CORE_CRATES.len());
    cargo(ctx, "wasm32-check", &wasm, &note);
    ctx.xtask_step(
        "wasm-layout-check",
        &["wasm", "--check"],
        "web/src/worker/layout.ts against the Rust constants",
    );
    ctx.xtask_step("riscv-tests", &["riscv-tests"], "");
    // The three oracle checks that read committed data instead of oracle output. They cost a file
    // read each and need no oracle, corpus or toolchain; the rule that a new memory-region touch
    // fails CI holds only if the histogram gate runs in a tier.
    ctx.xtask_step("oracle-regions", &["oracle", "regions", "--check"], "");
    ctx.xtask_step(
        "oracle-known-diffs",
        &["oracle", "known-diffs", "--check"],
        "",
    );
    ctx.xtask_step(
        "oracle-hist",
        &["oracle", "hist", "--check"],
        "coverage gate over the committed sample",
    );
    ctx.xtask_step("portable", &["portable"], "CRLF and portable-name scans");
    hooks_present(ctx);
    w0(ctx);
    bun_test(ctx);
}

/// The tests, less the package tests of the `package` group.
fn t0_test(ctx: &mut Ctx) {
    // The `device` feature's own unit tests (the esptool runner's scratch privacy, deadlines,
    // output draining and isolated environment) are not in the workspace test run either; they
    // spawn only `/bin/sleep` and `/bin/dd` on macOS and Windows PowerShell on Windows, never
    // esptool and never a port. The four device steps run on both hosts, because the feature
    // builds on both; the planner's Windows arm is its own `windows-sys` code, checked by
    // `cargo xtask layering`.
    let test_device = [
        "test",
        "-p",
        "pemu-planner",
        "--features",
        "device",
        "--lib",
    ];
    cargo(ctx, "test-planner-device", &test_device, "");
    let test_host_device = ["test", "-p", "pemu-host", "--features", "device", "--lib"];
    cargo(ctx, "test-host-device", &test_host_device, "");
    // `--show-output` puts the SKIP, PENDING and NOT_RUN lines of passing tests on stdout, where
    // the step reads them (`outcome.rs`). The corpus belongs to T1, but this step inherits the
    // operator's environment, so the command removes the data-root override: every corpus test is
    // SKIPPED-CORPUS here, whoever runs T0.
    let cmd = workspace_test_command(ctx);
    ctx.test_step("test", cmd, "");
    // Cross-host parity against the committed macOS golden, a step of its own so every host's
    // receipt names it. It does not wait behind the workspace run: `cargo test` stops at the first
    // failing binary, which would leave the parity unproved and unreported.
    let cmd = parity_test_command(ctx);
    ctx.test_step(PARITY_STEP, cmd, "tests/golden/cross-host/parity.txt");
    // The engine fuzz at T0's figure. The test's own default is 10^4, so no environment is set
    // here; the 10^6 run is `t2_m0_*` of the T2 tests.
    fuzz(ctx, "fuzz-1e4");
}

/// T1: needs the corpus and local goldens.
fn t1(ctx: &mut Ctx) {
    corpus::step(ctx);
    if hash_file_exists() {
        ctx.xtask_step(
            "secrets-check",
            &["secrets-check"],
            "pattern and hashed rules",
        );
    } else {
        ctx.record(StepResult::instant(
            "secrets-check",
            Status::Fail,
            "hashed rules need ~/.config/passportsim/secrets-check.toml (see docs/secrets.md)",
        ));
    }
    prefixed_tests(ctx, "t1");
    // Right after `t1-tests`, and before any other step can load the host again, so the tier's own
    // parallel test run cannot be what makes the native perf tests measure a busy host.
    host_budget(ctx);
    // The decode corpus and the `--strict` receipt rule are covered by tests the workspace already
    // runs, so these steps assert that those tests still exist rather than running them a second
    // time. A step that silently reports SKIPPED for something a test covers hides the coverage;
    // one that reports PASS without naming what covered it hides its absence.
    covered_by(
        ctx,
        "decode-corpus",
        &["t1_rom_and_corpus_elfs_decode_like_objdump"],
        "the decode corpus against objdump",
    );
    covered_by(
        ctx,
        "strict-receipts",
        &[
            "receipt::tests::a_class_u_touch_is_a_caveat_in_both_modes_and_exits_7_under_strict",
            "receipt::tests::an_unmodeled_first_touch_is_a_lenient_caveat_and_exits_7_under_strict",
        ],
        "the `--strict` exit-code rule (`crates/pemu-api/src/receipt.rs`)",
    );
    // Golden boots and short scenarios are milestone tests of `t1-tests`; these steps carry the
    // status those tests recorded, so a blocked golden reads as blocked here too.
    covered_by(
        ctx,
        "golden-boots",
        &[
            "t1_m4_pk_boot_console",
            "t1_m4_first_screen_frame",
            "t1_m5_official_menu_frame",
        ],
        "the golden boots (the pk console, the first-screen and menu frames)",
    );
    covered_by(
        ctx,
        "scenarios",
        &[
            "t1_m5_official_menu_smoke",
            "t1_m7_scenario_suite_writes_one_junit_file",
        ],
        "the scenario runs (the menu smoke, the scenario suite)",
    );
    determinism_steps(ctx, "t1", &["determinism-short", "node-jsc-parity"]);
    // The Playwright smoke writes the browser record, and the native replay reads it, so the
    // replay runs after the smoke and never in `t1-tests`.
    let record = ctx.log_dir.join(browser_record_file("chromium"));
    // The browser half of the clean-environment first run runs in this smoke
    // (`web/tests/firstRun.spec.ts`) against a package built here.
    let package = smoke_package(ctx);
    let core = playwright(
        ctx,
        "playwright-chromium-smoke",
        &["chromium"],
        package.as_deref(),
        Corpus::Given,
    );
    browser_journal_replay(ctx, &record, core.as_deref());
    // Firefox is a tested engine: its project runs the same suite, less the rows another engine
    // owns (`web/tests/playwright.config.ts`), and is NOT_RUN with the install hint when Playwright
    // Firefox is missing.
    playwright(ctx, FIREFOX_STEP, &["firefox"], None, Corpus::Given);
    determinism_steps(ctx, "t1", &["restore-equivalence-short"]);
}

/// The native replay of the record `record` that the Playwright smoke wrote, with the wasm
/// core it served (`core`) and the corpus root. The step and the replay test are NOT_RUN when the
/// smoke wrote no record (`tests/milestones/m9.rs`).
fn browser_journal_replay(ctx: &mut Ctx, record: &Path, core: Option<&Path>) {
    if !record.exists() {
        let reason = "playwright-chromium-smoke wrote no browser record: its SAB run of \
                      `web/tests/m9.spec.ts` did not reach the export (see its sub-steps)";
        ctx.record(StepResult::instant(REPLAY_STEP, Status::NotRun, reason));
        ctx.tests.push(TestOutcome {
            name: REPLAY_TEST.to_string(),
            status: Status::NotRun,
            reason: reason.to_string(),
            partial: Vec::new(),
        });
        return;
    }
    let args = replay_args();
    let mut cmd = ctx.command("cargo", &args);
    cmd.env(BROWSER_RECORD_ENV, record);
    if let Some(core) = core {
        cmd.env(CORE_ENV, core);
    }
    with_data_root(&mut cmd);
    ctx.test_step(
        REPLAY_STEP,
        cmd,
        "the native replay of the record of playwright-chromium-smoke",
    );
}

/// The cargo arguments of the native replay.
pub fn replay_args() -> Vec<&'static str> {
    let mut args = vec!["test"];
    args.extend(test_profile("t1"));
    args.extend([
        "--workspace",
        "--test",
        "m9",
        "--",
        "--exact",
        REPLAY_TEST,
        "--show-output",
    ]);
    args
}

/// The engine fuzz at T0's 10^4: the engine against `ref_step` at block sizes 1, 3 and 64, with
/// `PEMU_FUZZ_ITERS` removed so the test's own 10^4 default runs.
fn fuzz(ctx: &mut Ctx, name: &str) {
    const TEST: &str = "the_engine_equals_ref_step_at_block_sizes_1_3_and_64";
    let mut cmd = ctx.command(
        "cargo",
        &[
            "test",
            "--release",
            "-p",
            "pemu-rv32",
            "--test",
            "fuzz",
            "--",
            "--exact",
            TEST,
        ],
    );
    cmd.env_remove("PEMU_FUZZ_ITERS");
    let exec = ctx.exec(name, cmd, false);
    let step = ctx.finish(name, &exec, "10^4 iterations (the test's default)");
    ctx.record(step);
}

/// Runs the determinism steps named `steps` of `tier`, in that order (`determinism.rs`).
fn determinism_steps(ctx: &mut Ctx, tier: &str, steps: &[&str]) {
    for name in steps {
        let row = determinism::ROWS
            .iter()
            .find(|row| row.tier == tier && row.step == *name)
            .expect("every determinism step named by a tier is a row");
        determinism::run(ctx, row, with_data_root);
    }
}

/// Asserts that a test whose name contains one of `needles` exists, for a tier step whose exit is
/// proved by tests the workspace already runs. It fails when every such test is renamed away or
/// deleted, which a plain SKIPPED reason cannot do.
///
/// When a test step of this tier already ran some of those tests, the step carries their outcome
/// ([`covered_status`]); tests only other tiers run are counted as existing.
fn covered_by(ctx: &mut Ctx, name: &str, needles: &[&str], what: &str) {
    let (list, _) = match list_tests(ctx, name) {
        Ok(listed) => listed,
        Err(step) => return ctx.record(step),
    };
    let found: Vec<&str> = outcome::test_names(&list)
        .into_iter()
        .filter(|test| needles.iter().any(|needle| test.contains(needle)))
        .collect();
    let step = if found.is_empty() {
        StepResult::instant(
            name,
            Status::Fail,
            format!(
                "no test named `{}` covers {what} any more",
                needles.join("`, `")
            ),
        )
    } else {
        let (status, detail) = covered_status(&ctx.tier, &found, &ctx.tests);
        StepResult::instant(
            name,
            status,
            format!(
                "{} test(s) matching `{}` cover {what}{detail}",
                found.len(),
                needles.join("`, `")
            ),
        )
    };
    ctx.record(step);
}

/// The status of a covered step at `tier` over the outcomes this tier recorded for `found`. A found
/// test of this tier (`<tier>_` name) with no recorded outcome is NOT_RUN "not run by this tier".
/// PASS when every test of this tier passed, or when `found` holds tests of other tiers only,
/// which then only prove the coverage exists; otherwise the strongest non-pass status (FAIL, then
/// BLOCKED, SKIPPED-CORPUS, SKIPPED, NOT_RUN), with each test of this tier.
pub fn covered_status(tier: &str, found: &[&str], outcomes: &[TestOutcome]) -> (Status, String) {
    let prefix = format!("{tier}_");
    let mut ran: Vec<(String, Status)> = Vec::new();
    for name in found {
        let last = name.rsplit("::").next().unwrap_or(name);
        let recorded = outcomes.iter().find(|o| o.name == last);
        match recorded {
            Some(o) => ran.push((o.name.clone(), o.status)),
            None if last.starts_with(&prefix) => ran.push((last.to_string(), Status::NotRun)),
            None => {}
        }
    }
    if ran.is_empty() {
        return (Status::Pass, format!("; no {tier} test among them"));
    }
    let order = [
        Status::Fail,
        Status::Blocked,
        Status::SkippedCorpus,
        Status::Skipped,
        Status::NotRun,
    ];
    let status = order
        .into_iter()
        .find(|want| ran.iter().any(|(_, s)| s == want))
        .unwrap_or(Status::Pass);
    let each: Vec<String> = ran
        .iter()
        .map(|(name, s)| match s {
            Status::NotRun if !outcomes.iter().any(|o| o.name == *name) => {
                format!("{name} not run by this tier")
            }
            s => format!("{name} {}", s.as_str()),
        })
        .collect();
    (status, format!("; {tier} tests: {}", each.join(", ")))
}

/// T2: nightly and on demand.
fn t2(ctx: &mut Ctx) {
    prefixed_tests(ctx, "t2");
    oracle_hist_pk(ctx);
    oracle_diffs(ctx);
    determinism_steps(
        ctx,
        "t2",
        &[
            "determinism-long",
            "snapshot-anywhere",
            "restore-equivalence-all",
        ],
    );
    playwright(
        ctx,
        "playwright-chromium-webkit",
        &["chromium", "webkit"],
        None,
        Corpus::Given,
    );
    // The K and browser-floor gates. `bench-browser` stores the F3 records whose browser rows
    // `bench-check-model` then gates, so it runs first.
    bench_gate(ctx, "bench-k", &["bench-k"], "workload K");
    bench_gate(
        ctx,
        "bench-browser",
        &["bench-browser", "--gate-exits"],
        "browser floors",
    );
    benchmarks(ctx);
}

/// A release `xtask` benchmark gate as step `name`: `bench-k` or `bench-browser`.
///
/// NOT_RUN, not FAIL, when this host lacks what the gate measures against: the preserved spike
/// below the data root or the ESP toolchain (`bench_k::spike_dir`, `bench_k::toolchain`). Each
/// `NOT MEASURED` or `NOT_RUN` line the run prints is a NOT_RUN sub-step, so a gate that passed
/// without judging every part shows what it left out ([`unmeasured`]).
fn bench_gate(ctx: &mut Ctx, name: &str, args: &[&str], what: &str) {
    let missing = crate::bench_k::spike_dir()
        .and_then(|_| crate::bench_k::toolchain(crate::bench_k::GCC))
        .err();
    if let Some(why) = missing {
        let reason = format!(
            "{what} not measured on this host: {}",
            super::model::one_line(&why)
        );
        ctx.record(StepResult::instant(name, Status::NotRun, reason));
        return;
    }
    let mut full = vec!["run", "--release", "-q", "-p", "xtask", "--"];
    full.extend_from_slice(args);
    let cmd = ctx.command("cargo", &full);
    let exec = ctx.exec(name, cmd, false);
    let step = ctx.finish(name, &exec, what);
    ctx.record(step);
    let log = std::fs::read_to_string(&exec.log).unwrap_or_default();
    for (n, line) in unmeasured(&log).into_iter().enumerate() {
        let sub = format!("{name}.unmeasured-{}", n + 1);
        ctx.record(StepResult::instant(&sub, Status::NotRun, line));
    }
}

/// The lines of a step log that say part of what the step gates was not judged: a `NOT MEASURED`
/// floor of `xtask bench` or `bench-browser` (host too busy) and a `NOT_RUN` line. The step may
/// still pass; each line becomes a NOT_RUN sub-step.
pub fn unmeasured(log: &str) -> Vec<&str> {
    log.lines()
        .map(str::trim)
        .filter(|l| l.contains("NOT MEASURED") || l.starts_with("NOT_RUN"))
        .collect()
}

/// The two benchmark steps of T2: `xtask bench`, which measures and applies the 10 % regression
/// gate, and `xtask bench --check-model --gate`, the native and browser model check that
/// fails on any F3 row without a real measurement.
///
/// While no F-suite runs on this checkout (`crate::bench::any_suite_runnable`), both are NOT_RUN
/// with the reason, never SKIPPED. A suite whose corpus id is absent prints its own NOT_RUN line
/// inside the step and does not fail it.
fn benchmarks(ctx: &mut Ctx) {
    match benchmark_plan(crate::bench::any_suite_runnable()) {
        Ok(steps) => {
            for (name, args) in steps {
                cargo(ctx, name, &args, "");
            }
        }
        Err(reason) => {
            for name in BENCHMARK_STEPS {
                ctx.record(StepResult::instant(name, Status::NotRun, reason));
            }
        }
    }
}

/// Names of the T2 benchmark steps.
const BENCHMARK_STEPS: [&str; 2] = ["benchmarks", "bench-check-model"];

/// The cargo invocations of [`benchmarks`], or why they do not run.
fn benchmark_plan(runnable: bool) -> Result<[(&'static str, Vec<&'static str>); 2], &'static str> {
    if !runnable {
        return Err("no F-suite is runnable on this checkout (xtask bench lists what each needs)");
    }
    Ok([
        (
            BENCHMARK_STEPS[0],
            vec![
                "run",
                "--release",
                "-q",
                "-p",
                "xtask",
                "--",
                "bench",
                "--gate-exits",
            ],
        ),
        (
            BENCHMARK_STEPS[1],
            vec![
                "run",
                "-q",
                "-p",
                "xtask",
                "--",
                "bench",
                "--check-model",
                "--gate",
            ]
            .into_iter()
            // On macOS, where Chrome and JSC are gated, every F3 row of the model check is gated,
            // the browser rows from the uncontended F3 records `xtask bench-browser` stored for
            // this host. Elsewhere only the native rows are: T2 is macOS-only, the Windows Chrome
            // arm of the browser floors is a `bench-browser` run of its own on that host, and JSC
            // is not a Windows engine.
            .chain(
                if cfg!(target_os = "macos") {
                    None
                } else {
                    Some(["--gate-engine", "native"])
                }
                .into_iter()
                .flatten(),
            )
            .collect(),
        ),
    ])
}

/// The coverage gate over the real `pk` oracle trace, which lives below the data root because it
/// is 280 MB. The T0 `oracle-hist` step runs the same gate over the committed fixture pair and
/// proves the gate works; this one proves the coverage, and is SKIPPED rather than failed when the
/// trace is absent, because regenerating it needs a built QEMU oracle (`xtask oracle consoles`).
fn oracle_hist_pk(ctx: &mut Ctx) {
    let trace = match crate::hostdirs::home().and_then(|home| {
        crate::hostdirs::data_root(&home, None).map(|root| root.join(crate::oracle::PK_TRACE))
    }) {
        Ok(trace) if trace.exists() => trace,
        Ok(trace) => {
            skipped(
                ctx,
                "oracle-hist-pk",
                &format!(
                    "{} is absent; regenerate it with `xtask oracle consoles`",
                    trace.display()
                ),
            );
            return;
        }
        Err(err) => {
            skipped(ctx, "oracle-hist-pk", &err);
            return;
        }
    };
    let trace = trace.display().to_string();
    ctx.xtask_step(
        "oracle-hist-pk",
        &[
            "oracle",
            "hist",
            "--check",
            "--trace",
            &trace,
            "--baseline",
            crate::oracle::PK_HIST,
        ],
        "coverage gate over the pk oracle trace",
    );
}

/// The oracle diff of the bootloader phase: `xtask oracle diff` over every
/// image whose oracle record is below the data root. The records are regenerated by `xtask oracle
/// boot-trace`, which needs a built QEMU oracle, so the step is SKIPPED with that command when
/// none is there rather than failed; `t2_m2_bootloader_phase_matches_oracle` runs the
/// same comparison and prints its own `SKIP` line.
fn oracle_diffs(ctx: &mut Ctx) {
    let root =
        match crate::hostdirs::home().and_then(|home| crate::hostdirs::data_root(&home, None)) {
            Ok(root) => root,
            Err(err) => {
                skipped(ctx, "oracle-diffs", &err);
                return;
            }
        };
    let ids: Vec<&str> = crate::oracle::PHASE_IMAGES
        .iter()
        .copied()
        .filter(|id| crate::oracle::phase_record(&root, id).exists())
        .collect();
    if ids.is_empty() {
        skipped(
            ctx,
            "oracle-diffs",
            &format!(
                "no oracle phase record below {}; `xtask oracle boot-trace` records them",
                root.join(crate::oracle::PHASE_DIR).display()
            ),
        );
        return;
    }
    let images = ids.join(",");
    ctx.xtask_step(
        "oracle-diffs",
        &["oracle", "diff", "--images", &images],
        &format!("bootloader-phase write streams and call trace against QEMU: {images}"),
    );
}

/// Stdout of `cargo test --workspace -- --list` as step `name`, or the failed step.
fn list_tests(ctx: &mut Ctx, name: &str) -> Result<(String, std::time::Duration), StepResult> {
    let mut args = vec!["test", "--workspace"];
    args.extend(test_profile(&ctx.tier));
    args.extend(["--", "--list"]);
    let cmd = ctx.command("cargo", &args);
    let exec = ctx.exec(name, cmd, true);
    if exec.success {
        Ok((exec.stdout, exec.duration))
    } else {
        Err(ctx.finish(name, &exec, ""))
    }
}

/// The T0 `test` step's command: the whole workspace, with the SKIP/PENDING/NOT_RUN lines on
/// stdout, and the data-root override **removed**.
///
/// Split out so a test can read the removal off the command without building a tier.
pub(super) fn workspace_test_command(ctx: &Ctx) -> std::process::Command {
    let mut cmd = ctx.command(
        "cargo",
        &[
            "test",
            "--workspace",
            "--",
            "--show-output",
            "--skip",
            PARITY_TEST,
            "--skip",
            PACKAGE_TESTS,
        ],
    );
    cmd.env_remove(crate::hostdirs::DATA_ROOT_ENV);
    cmd
}

/// Step name of the package tests in T0.
pub const PACKAGE_STEP: &str = "package-tests";

/// The name filter of the tests of `cargo xtask package` (`xtask/src/package/tests.rs`). Their
/// shared package is a release build of the CLI and the wasm core plus the web bundle, which the
/// workspace step skips so it builds once, in the `package` group.
pub const PACKAGE_TESTS: &str = "package::tests::";

/// The `package-tests` step's command: those tests alone, with the data-root override removed as
/// in [`workspace_test_command`].
pub(super) fn package_test_command(ctx: &Ctx) -> std::process::Command {
    let mut cmd = ctx.command(
        "cargo",
        &[
            "test",
            "-p",
            "xtask",
            "--bin",
            "xtask",
            "--",
            PACKAGE_TESTS,
            "--show-output",
        ],
    );
    cmd.env_remove(crate::hostdirs::DATA_ROOT_ENV);
    cmd
}

/// Step name of the cross-host parity row in T0.
pub const PARITY_STEP: &str = "cross-host-parity";

/// The test that step runs (`tests/milestones/cross_host/main.rs`), which the workspace `test`
/// step skips so it runs once.
pub const PARITY_TEST: &str = "t0_cross_host_parity_equals_the_committed_macos_golden";

/// The `cross-host-parity` step's command: the `cross_host` test binary alone, with the same
/// removal of the data-root override as [`workspace_test_command`] (it reads no corpus).
pub(super) fn parity_test_command(ctx: &Ctx) -> std::process::Command {
    let mut cmd = ctx.command(
        "cargo",
        &[
            "test",
            "-p",
            "pemu-milestones",
            "--test",
            "cross_host",
            "--",
            "--show-output",
        ],
    );
    cmd.env_remove(crate::hostdirs::DATA_ROOT_ENV);
    cmd
}

/// Sets `PASSPORTSIM_DATA_ROOT` on a test step's command to this host's data root, when it exists.
///
/// `pemu-testkit` never resolves a directory role itself, so `corpus_or_skip` reads this one
/// override and otherwise skips. It is set per step and never on the whole run:
/// `secrets-check` refuses a root pointed at from outside, which stops a scan being aimed away
/// from the tree it is supposed to cover.
pub fn with_data_root(cmd: &mut std::process::Command) {
    if let Ok(root) = host_data_root()
        && root.exists()
    {
        cmd.env(crate::hostdirs::DATA_ROOT_ENV, root);
    }
}

/// The data root of this host: the override when one is set, else the host column's row, so a
/// Windows run gets `%LOCALAPPDATA%\passportsim\data\` and not the macOS row joined onto its
/// profile (`hostdirs::data_root` resolves the macOS column only).
pub(crate) fn host_data_root() -> Result<PathBuf, String> {
    crate::hostdirs::data_root_of(&crate::hostdirs::HostDirs::from_process(), None)
}

/// Runs the workspace tests named `<prefix>_*`, SKIPPED while there are none, as a test step
/// (`Ctx::test_step`).
fn prefixed_tests(ctx: &mut Ctx, prefix: &str) {
    let name = format!("{prefix}-tests");
    // The listing compiles every test binary of the tier's profile, which is most of the tier's
    // build time, so it is charged to this step.
    let (list, list_time) = match list_tests(ctx, &name) {
        Ok(listed) => listed,
        Err(step) => return ctx.record(step),
    };
    let wanted = format!("{prefix}_");
    let count = outcome::test_names(&list)
        .iter()
        .filter(|test| {
            test.rsplit("::")
                .next()
                .is_some_and(|last| last.starts_with(&wanted))
        })
        .count();
    if count == 0 {
        return skipped(
            ctx,
            &name,
            &format!("no {wanted} tests in the workspace yet"),
        );
    }
    let args = prefixed_test_args(prefix);
    let elsewhere = args.iter().filter(|a| *a == "--skip").count();
    let note = format!(
        "{count} {wanted} tests in {}, {elsewhere} run by later steps",
        if test_profile(prefix).is_empty() {
            "debug"
        } else {
            "the ci-test profile"
        }
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd = ctx.command("cargo", &args);
    with_data_root(&mut cmd);
    ctx.test_step(&name, cmd, &note);
    if let Some(step) = ctx.steps.iter_mut().find(|step| step.name == name) {
        step.duration += list_time;
    }
}

/// The cargo arguments of the `<prefix>-tests` step: the tier's profile, the `<prefix>_` filter,
/// `--show-output` for `outcome.rs`, and a `--skip` for each test a later step of the tier runs (the
/// determinism rows, and the browser record replay after the Playwright smoke).
pub fn prefixed_test_args(prefix: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["test".into(), "--workspace".into()];
    args.extend(test_profile(prefix).iter().map(|a| a.to_string()));
    args.extend(["--".into(), format!("{prefix}_"), "--show-output".into()]);
    args.extend(determinism::skip_args(prefix));
    if prefix == "t1" {
        args.extend(["--skip".into(), REPLAY_TEST.into()]);
        for test in HOST_BUDGET_TESTS {
            args.extend(["--skip".into(), (*test).into()]);
        }
    }
    args
}

/// The T1 tests that check absolute host-time budgets: F1, F3, F4 and F5 in M5, F6 in M6. They
/// SKIP under `xtask bench`'s contention rule, and inside `t1-tests` the parallel run itself is
/// that load (the M5 test skipped at a load average of 12.2 on 12 cores with nothing else
/// running), so [`HOST_BUDGET_STEP`] runs them alone after [`settle`]. No other test skips on host
/// load: the F7 test asserts no host figure, and the browser rows run in the Playwright steps.
pub const HOST_BUDGET_TESTS: &[&str] = &["t1_m5_native_perf", "t1_m6_native_perf_f6"];

/// Step name of the serial run of [`HOST_BUDGET_TESTS`].
pub const HOST_BUDGET_STEP: &str = "host-budget-perf";

/// The one-minute load average per core [`settle`] waits for before [`HOST_BUDGET_STEP`] runs:
/// half of `xtask bench`'s contention threshold (one per core), because the load average trails the
/// work by a minute and the step's own bench runs add to it while they measure.
pub const SETTLE_LOAD_PER_CORE: f64 = 0.5;

/// The longest [`settle`] waits. The one-minute average decays by `e` per minute once the work
/// behind it stops, so the 12 to 15 `t1-tests` leaves on 12 cores falls to 6 in about a minute and
/// a half on a host with nothing else to do; one still above the target after three minutes is busy
/// for its own reasons, and the tests' contention rule is the one to judge that.
pub const SETTLE_MAX: std::time::Duration = std::time::Duration::from_secs(180);

/// How often [`settle`] reads the load average.
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// What [`settle`] saw.
#[derive(Clone, Debug, PartialEq)]
pub struct Settled {
    /// The last one-minute load average read, `None` where this host has none.
    pub load: Option<f64>,
    /// The target it was compared with (`SETTLE_LOAD_PER_CORE` times the cores).
    pub target: f64,
    /// How long it waited.
    pub waited: std::time::Duration,
}

impl Settled {
    /// Whether the load reached the target (or cannot be read, which nothing can wait out).
    pub fn quiet(&self) -> bool {
        self.load.is_none_or(|load| load <= self.target)
    }

    /// The step note's account of the wait.
    pub fn note(&self) -> String {
        let waited = self.waited.as_secs();
        match self.load {
            None => "no load average on this host, run without a settle".to_string(),
            Some(load) if self.quiet() => format!(
                "settled {waited} s after t1-tests, to a one-minute load average of {load:.1} \
                 (target {:.1})",
                self.target
            ),
            Some(load) => format!(
                "one-minute load average still {load:.1} after {waited} s (target {:.1}); run \
                 anyway, and each test's own contention rule judges the host",
                self.target
            ),
        }
    }
}

/// Waits until `read` (the one-minute load average) is at most `target`, polling every `poll` and
/// giving up after `max`; `sleep` does the waiting, so a test can drive it without a clock.
pub fn settle(
    mut read: impl FnMut() -> Option<f64>,
    mut sleep: impl FnMut(std::time::Duration),
    target: f64,
    max: std::time::Duration,
    poll: std::time::Duration,
) -> Settled {
    let mut waited = std::time::Duration::ZERO;
    loop {
        let settled = Settled {
            load: read(),
            target,
            waited,
        };
        if settled.quiet() || waited >= max {
            return settled;
        }
        let step = poll.min(max - waited);
        sleep(step);
        waited += step;
    }
}

/// The cargo arguments of [`HOST_BUDGET_STEP`]: the tier's profile and feature set, so the build
/// of `t1-tests` is reused, only the binaries that hold the tests, each test by its exact name, and
/// one test thread, so the two tests do not measure each other.
pub fn host_budget_args() -> Vec<String> {
    let mut args: Vec<String> = vec!["test".into()];
    args.extend(test_profile("t1").iter().map(|a| a.to_string()));
    args.push("--workspace".into());
    let mut bins: Vec<&str> = HOST_BUDGET_TESTS
        .iter()
        .filter_map(|t| t.split('_').nth(1))
        .collect();
    bins.sort_unstable();
    bins.dedup();
    for bin in bins {
        args.extend(["--test".into(), bin.into()]);
    }
    args.extend(["--".into(), "--exact".into()]);
    args.extend(HOST_BUDGET_TESTS.iter().map(|t| (*t).to_string()));
    args.extend(["--show-output".into(), "--test-threads=1".into()]);
    args
}

/// [`HOST_BUDGET_STEP`]: [`HOST_BUDGET_TESTS`] alone and in series, after [`settle`], as a test
/// step, so their `RAN`, `SKIP` and failures reach the receipt exactly as they would from
/// `t1-tests`.
fn host_budget(ctx: &mut Ctx) {
    let settled = settle(
        crate::bench::load_average,
        std::thread::sleep,
        crate::bench::cores() * SETTLE_LOAD_PER_CORE,
        SETTLE_MAX,
        SETTLE_POLL,
    );
    let args = host_budget_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd = ctx.command("cargo", &args);
    with_data_root(&mut cmd);
    let note = format!("the native perf tests in series; {}", settled.note());
    ctx.test_step(HOST_BUDGET_STEP, cmd, &note);
    // The wait is the step's own time, so the receipt does not hide it.
    if let Some(step) = ctx.steps.iter_mut().find(|s| s.name == HOST_BUDGET_STEP) {
        step.duration += settled.waited;
    }
}

/// The secret guard's git hooks, reported but never installed here.
fn hooks_present(ctx: &mut Ctx) {
    let name = "hooks-present";
    let root = ctx.root.clone();
    let step = match crate::secrets::hooks::presence(&root) {
        Ok(Ok(note)) => StepResult::instant(name, Status::Pass, &note),
        Ok(Err(why)) => StepResult::instant(name, Status::Fail, &why),
        Err(why) => StepResult::instant(name, Status::Fail, &why),
    };
    ctx.record(step);
}

/// The `w0-*` steps: `cargo clippy -- -D warnings` for both Windows targets from a macOS host,
/// which type-checks the workspace for them as well. The targets come from
/// `rust-toolchain.toml`; xtask never runs `rustup target add`, so a missing one is SKIPPED, not
/// installed.
fn w0(ctx: &mut Ctx) {
    if std::env::consts::OS != "macos" {
        for target in W0_TARGETS {
            let name = format!("w0-{}", short_target(target));
            let reason = "w0 is the Windows cross-check of the macOS host";
            ctx.record(StepResult::instant(&name, Status::NotRun, reason));
        }
        return;
    }
    let sysroot = ctx
        .query("rustc", &["--print", "sysroot"])
        .map(PathBuf::from);
    for target in W0_TARGETS {
        let name = format!("w0-{}", short_target(target));
        let installed = sysroot
            .as_ref()
            .is_some_and(|dir| dir.join("lib/rustlib").join(target).join("lib").is_dir());
        if !installed {
            skipped(ctx, &name, "windows-target-missing");
            continue;
        }
        let args = [
            "clippy",
            "--workspace",
            "--all-targets",
            "--target",
            target,
            "--",
            "-D",
            "warnings",
        ];
        cargo(ctx, &name, &args, target);
    }
}

/// `x86_64-pc-windows-msvc` as `x86_64`, for a readable step name.
fn short_target(target: &str) -> &str {
    target.split('-').next().unwrap_or(target)
}

/// `bun test` of the web package, SKIPPED when `web/package.json` is absent.
fn bun_test(ctx: &mut Ctx) {
    let web = ctx.root.join("web");
    if !Path::new(&web).join("package.json").exists() {
        return skipped(ctx, "bun-test", "no web/package.json");
    }
    // A Bun or Node install that ships only `bun.cmd` is a missing tool, not a failed step
    // (`runner::resolve` never runs a shim).
    match super::runner::resolve("bun") {
        super::runner::Tool::ShimOnly => {
            return skipped(
                ctx,
                "bun-test",
                "bun: only a `.cmd` shim found; install the `.exe`",
            );
        }
        super::runner::Tool::Missing => return skipped(ctx, "bun-test", "bun is not installed"),
        super::runner::Tool::Found(_) => {}
    }
    // Some web tests drive `target/debug/passportsim` and skip, reporting ok, when it is not
    // built, so the step builds it first and those tests always run here.
    cargo(
        ctx,
        "bun-test-cli",
        &["build", "-p", "pemu-cli"],
        "the debug CLI the web tests drive",
    );
    let mut cmd = ctx.command("bun", &["test"]);
    cmd.current_dir(web);
    // Bun exits non-zero when the tree holds no test file, which is not a failure of the step.
    let exec = ctx.exec("bun-test", cmd, false);
    if !exec.success && super::runner::tail(&exec.log, 40).contains("0 test files matching") {
        return skipped(ctx, "bun-test", "no bun tests");
    }
    let step = ctx.finish("bun-test", &exec, "");
    ctx.record(step);
}

/// What `web/tests/browserCheck.ts` reported for one browser row.
#[derive(Debug, PartialEq, Eq)]
pub enum BrowserPresence {
    /// The Playwright browser is installed.
    Present,
    /// It is not; the text is the install hint.
    Missing(String),
    /// The check printed neither answer, so the check itself is broken.
    Broken(String),
}

/// Reads the last `PRESENT` or `MISSING <hint>` line of a browser check.
pub fn browser_presence(stdout: &str) -> BrowserPresence {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line == "PRESENT" {
            return BrowserPresence::Present;
        }
        if let Some(hint) = line.strip_prefix("MISSING ") {
            return BrowserPresence::Missing(hint.trim().to_string());
        }
    }
    BrowserPresence::Broken(format!(
        "web/tests/browserCheck.ts printed neither PRESENT nor MISSING: {}",
        stdout.trim()
    ))
}

/// One Playwright step: `bun run e2e --project=<row>` for each row of `web/tests/browsers.ts` named
/// in `engines`, with `PEMU_E2E_REQUIRE_BROWSERS=1`, so a missing browser fails the gating row
/// rather than letting every test skip. Chromium gates on every host; a row this host lacks
/// ([`host_gap`]) or whose browser is not installed is NOT_RUN with the hint: the suite never
/// downloads a browser, and a step that could not run all its rows must not pass. The specs get
/// [`playwright_env`] (the wasm core, the row's own browser record) and the corpus root when
/// `corpus` gives it; their JSON report becomes the row's sub-steps and test outcomes (`web.rs`).
/// Returns the core path.
fn playwright(
    ctx: &mut Ctx,
    name: &str,
    engines: &[&str],
    package: Option<&Path>,
    corpus: Corpus,
) -> Option<PathBuf> {
    let web_dir = ctx.root.join("web");
    if !Path::new(&web_dir).join("package.json").exists() {
        skipped(ctx, name, "no web/package.json");
        return None;
    }
    match super::runner::resolve("bun") {
        super::runner::Tool::Found(_) => {}
        super::runner::Tool::ShimOnly => {
            skipped(
                ctx,
                name,
                "bun: only a `.cmd` shim found; install the `.exe`",
            );
            return None;
        }
        super::runner::Tool::Missing => {
            skipped(ctx, name, "bun is not installed");
            return None;
        }
    }
    let (core, core_note) = match determinism::build_wasm_core(ctx, &format!("{name}-core-build")) {
        Ok(path) => (path, "the wasm core built here".to_string()),
        Err(failure) => {
            // A row without its core would only skip its real-core tests.
            let (status, reason) = core_failure(&failure, &ctx.root);
            ctx.record(StepResult::instant(name, status, reason));
            return None;
        }
    };
    let core = Some(core);
    // `web/tests/attach.spec.ts` attaches a page to a daemon, so the daemon is built here, once,
    // rather than by the spec inside a test's time budget.
    let daemon = match build_attach_daemon(ctx, &format!("{name}-daemon-build")) {
        Ok(path) => path,
        Err(exec) => {
            let step = ctx.finish(name, &exec, "the attach daemon did not build");
            ctx.record(step);
            return None;
        }
    };
    let mut ran: Vec<&str> = Vec::new();
    // Engines whose run executed, passed or not (for the stale-leg filter).
    let mut executed: Vec<&str> = Vec::new();
    let mut spent = std::time::Duration::ZERO;
    let mut subs = Vec::new();
    let mut tests = Vec::new();
    let mut verdict: Option<(Status, String)> = None;
    for &engine in engines {
        // Playwright Chromium is the row that gates everywhere, so its absence fails the launch;
        // every other row is looked for first and is NOT_RUN with the hint when it is missing.
        if engine != "chromium" {
            if let Some(gap) = host_gap(engine, std::env::consts::OS) {
                verdict = Some((Status::NotRun, gap));
                break;
            }
            let mut check = ctx.command("bun", &["tests/browserCheck.ts", engine]);
            check.current_dir(&web_dir);
            let check_name = format!("{name}-{engine}-check");
            let exec = ctx.exec(&check_name, check, true);
            spent += exec.duration;
            if !exec.success {
                let step = ctx.finish(name, &exec, "");
                verdict = Some((step.status, step.reason));
                break;
            }
            match browser_presence(&exec.stdout) {
                BrowserPresence::Present => {}
                BrowserPresence::Missing(hint) => {
                    let done = if ran.is_empty() {
                        String::new()
                    } else {
                        format!("{} passed; ", ran.join(", "))
                    };
                    verdict = Some((Status::NotRun, format!("{done}{engine} not run: {hint}")));
                    break;
                }
                BrowserPresence::Broken(reason) => {
                    verdict = Some((Status::Fail, reason));
                    break;
                }
            }
        }
        let json = ctx.log_dir.join(format!("{name}-{engine}.json"));
        let _ = std::fs::remove_file(&json);
        let project = format!("--project={engine}");
        let mut cmd = ctx.command("bun", &["run", "e2e", &project, "--reporter=list,json"]);
        cmd.current_dir(&web_dir);
        // A record left by an earlier run would be replayed as if this run wrote it.
        let record = ctx.log_dir.join(browser_record_file(engine));
        let _ = std::fs::remove_file(&record);
        let env = playwright_env(&json, free_port(), core.as_deref(), Some(&record));
        for (key, value) in env {
            if corpus == Corpus::Given || !key.starts_with("PEMU_E2E_IMAGE_") {
                cmd.env(key, value);
            }
        }
        cmd.env(ATTACH_DAEMON_ENV, &daemon);
        if let Some(package) = package {
            cmd.env(PACKAGE_ENV, package);
        }
        match corpus {
            Corpus::Given => with_data_root(&mut cmd),
            Corpus::Withheld => {
                cmd.env_remove(crate::hostdirs::DATA_ROOT_ENV);
            }
        }
        let run_name = format!("{name}-{engine}");
        let exec = ctx.exec(&run_name, cmd, false);
        spent += exec.duration;
        executed.push(engine);
        let text = std::fs::read_to_string(&json)
            .map_err(|err| format!("no JSON report at {}: {}", json.display(), err.kind()));
        let errors = text.as_deref().map(web::report_errors).unwrap_or_default();
        let report = text.and_then(|text| web::read_report(&text));
        match report {
            Ok(found) => {
                subs.extend(web::sub_steps(name, engine, &found));
                tests.extend(found);
            }
            Err(why) if exec.success => {
                verdict = Some((
                    Status::Fail,
                    format!("{engine} passed but its tests cannot be read: {why}"),
                ));
                break;
            }
            Err(_) => {}
        }
        if !errors.is_empty() {
            // A spec that did not load runs none of its tests, whatever the exit code.
            verdict = Some((
                Status::Fail,
                format!("{engine}: the report has errors: {}", errors.join("; ")),
            ));
            break;
        }
        if !exec.success {
            let step = ctx.finish(name, &exec, "");
            verdict = Some((step.status, step.reason));
            break;
        }
        ran.push(engine);
    }
    let (status, reason) =
        verdict.unwrap_or_else(|| (Status::Pass, format!("bun run e2e: {}", ran.join(", "))));
    let reason = format!("{reason}; {}; {core_note}", web::summary(&tests));
    ctx.record(StepResult {
        duration: spent,
        ..StepResult::instant(name, status, reason)
    });
    for sub in web::drop_stale_engine_legs(subs, &executed) {
        ctx.record(sub);
    }
    core
}

/// The variable naming the package directory `web/tests/firstRun.spec.ts` serves from.
pub const PACKAGE_ENV: &str = "PEMU_E2E_PACKAGE";

/// Step name of the package `web/tests/firstRun.spec.ts` opens.
pub const SMOKE_PACKAGE_STEP: &str = "package-smoke";

/// Builds the macOS package with `xtask package` (release binary, web bundle and, on a host with the
/// corpus, the demo) into `smoke-package` beside the run logs, for `web/tests/firstRun.spec.ts`,
/// and returns its package directory: the first `package:` line the command prints. NOT_RUN on any
/// other host; the spec then skips.
fn smoke_package(ctx: &mut Ctx) -> Option<PathBuf> {
    let name = SMOKE_PACKAGE_STEP;
    if std::env::consts::OS != "macos" {
        let reason = "the packaged browser run is macOS-only";
        ctx.record(StepResult::instant(name, Status::NotRun, reason));
        return None;
    }
    // One directory beside the run logs, replaced by each run: the package is about 100 MB with
    // its demo, and a copy per run's log directory would grow `target/` by that every T1.
    let out = ctx
        .log_dir
        .parent()
        .map_or_else(|| ctx.log_dir.join("package"), |d| d.join("smoke-package"));
    let _ = std::fs::remove_dir_all(&out);
    let out_arg = out.display().to_string();
    let cmd = ctx.command("cargo", &smoke_package_args(&out_arg));
    let exec = ctx.exec(name, cmd, true);
    let _ = std::fs::write(exec.log.with_extension("stdout.log"), &exec.stdout);
    let dir = package_dir_of(&exec.stdout);
    let note = match &dir {
        Some(dir) => format!("package for the browser first run: {}", dir.display()),
        None => "package for the browser first run: no `package:` line".to_string(),
    };
    let mut step = ctx.finish(name, &exec, &note);
    if step.status == Status::Pass && dir.is_none() {
        step.status = Status::Fail;
        step.reason = format!("{note}; `xtask package` passed and named no package directory");
    }
    let ok = step.status == Status::Pass;
    ctx.record(step);
    if ok { dir } else { None }
}

/// The cargo arguments of the [`SMOKE_PACKAGE_STEP`] step, writing below `out`.
pub fn smoke_package_args(out: &str) -> Vec<&str> {
    vec![
        "run",
        "-q",
        "-p",
        "xtask",
        "--",
        "package",
        "--target",
        crate::package::MACOS_TARGET,
        "--out",
        out,
        "--no-archive",
    ]
}

/// The native package directory `xtask package` printed: the first `package: <dir>` line whose
/// directory is named `passportsim-<version>-<os>`. Other `package:` lines (the web bundle, the
/// archives, notes such as `no demo embedded`) are not it.
pub fn package_dir_of(stdout: &str) -> Option<PathBuf> {
    let version = env!("CARGO_PKG_VERSION");
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("package: "))
        .map(|dir| PathBuf::from(dir.trim()))
        .find(|dir| {
            dir.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(&format!("passportsim-{version}-")))
                .is_some_and(|os| !os.is_empty() && os != "web" && !os.contains('.'))
        })
}

/// The variable naming the attach daemon `web/tests/attach.spec.ts` starts.
pub const ATTACH_DAEMON_ENV: &str = "PEMU_E2E_ATTACH_DAEMON";

/// Builds `pemu-host`'s `attach_daemon` example, the `serve` daemon with its static seam filled
/// from `web/dist` (`crates/pemu-host/examples/attach_daemon.rs`), with the tier's test profile, and
/// returns its path.
fn build_attach_daemon(ctx: &Ctx, log_name: &str) -> Result<PathBuf, super::runner::Exec> {
    let build = ctx.command(
        "cargo",
        &[
            "build",
            "-p",
            "pemu-host",
            "--example",
            "attach_daemon",
            "--profile",
            "ci-test",
        ],
    );
    let exec = ctx.exec(log_name, build, false);
    if !exec.success {
        return Err(exec);
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.root.join("target"));
    let file = format!("attach_daemon{}", std::env::consts::EXE_SUFFIX);
    Ok(ctx
        .root
        .join(target)
        .join("ci-test")
        .join("examples")
        .join(file))
}

/// Why the browser row `row` (a project of `web/tests/browsers.ts`) cannot run on host `os`: WebKit
/// is the macOS-only row and the installed Chrome and Edge rows are Windows-only, so elsewhere the
/// row is NOT_RUN rather than a pass over nothing.
pub fn host_gap(row: &str, os: &str) -> Option<String> {
    let only = match row {
        "webkit" => "macos",
        WINDOWS_CHROME | WINDOWS_MSEDGE => "windows",
        _ => return None,
    };
    (os != only).then(|| {
        let host = if only == "macos" { "macOS" } else { "Windows" };
        format!("{row} not run: the row is {host}-only, this host is {os}")
    })
}

/// The Windows browser rows (`web/tests/browsers.ts`): the installed Chrome and Edge through
/// Playwright's `chrome` and `msedge` channels.
pub const WINDOWS_CHROME: &str = "windows-chrome";
/// See [`WINDOWS_CHROME`].
pub const WINDOWS_MSEDGE: &str = "windows-msedge";

/// The browser rows of T0 on Windows: Playwright Chromium, the one row that gates on every host,
/// the installed Chrome and Edge, and Playwright Firefox. T1 and T2 are macOS-only, so on Windows
/// these rows run in T0.
pub const WINDOWS_ROWS: [&str; 4] = ["chromium", WINDOWS_CHROME, WINDOWS_MSEDGE, "firefox"];

/// Name of the T1 step that runs the Playwright Firefox row (`web/tests/browsers.ts`).
pub const FIREFOX_STEP: &str = "playwright-firefox";

/// Name of the T0 step that runs [`WINDOWS_ROWS`] on the Windows host.
pub const WINDOWS_BROWSERS_STEP: &str = "playwright-windows";

/// Whether the rows of a Playwright step see the corpus: T1 and T2 pass the data root and the
/// images the card rows load; T0 needs no corpus and clears the root, as its test step does, so a
/// corpus row there is SKIPPED-CORPUS whatever the operator's environment holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corpus {
    Given,
    Withheld,
}

/// The status of a Playwright row whose wasm core could not be built: NOT_RUN when the wasm32
/// target is not installed on this host, FAIL when the build itself failed.
pub fn core_failure(failure: &CoreBuild, root: &Path) -> (Status, String) {
    match failure {
        CoreBuild::NoTarget => (
            Status::NotRun,
            "no wasm core: the wasm32-unknown-unknown target is not installed".to_string(),
        ),
        CoreBuild::Failed(exec) => {
            let log = exec.log.strip_prefix(root).unwrap_or(&exec.log);
            (
                Status::Fail,
                format!(
                    "the wasm core did not build ({}); log {}",
                    exec.failure,
                    log.display()
                ),
            )
        }
    }
}

/// The environment of one Playwright run (`playwright`): browsers required, the web server's port,
/// the JSON report file, the wasm core and the browser record, each when known. The corpus root is
/// set apart, by `with_data_root`.
pub fn playwright_env(
    json: &Path,
    port: Option<u16>,
    core: Option<&Path>,
    record: Option<&Path>,
) -> Vec<(&'static str, std::ffi::OsString)> {
    let mut env = vec![
        ("PEMU_E2E_REQUIRE_BROWSERS", "1".into()),
        ("PLAYWRIGHT_JSON_OUTPUT_FILE", json.as_os_str().to_owned()),
    ];
    if let Some(port) = port {
        // `web/tests/playwright.config.ts` serves on this port; a fixed one collides with any other
        // server or run on the host.
        env.push(("PEMU_WEB_PORT", port.to_string().into()));
    }
    if let Some(core) = core {
        env.push((CORE_ENV, core.as_os_str().to_owned()));
    }
    if let Some(record) = record {
        env.push((BROWSER_RECORD_ENV, record.as_os_str().to_owned()));
    }
    env.extend(e2e_image_env());
    env
}

/// The corpus images the card rows load, as `PEMU_E2E_IMAGE_<NAME>`: `web/tests/preconditions.ts`
/// reads one variable per id and `web/tests/serve.ts` publishes the directory file by file.
///
/// A variable is set only for a directory that exists, the opposite of the browser's rule:
/// `preconditions.ts` throws for a named directory that is absent (a row given an image it cannot
/// find fails), while a host with no corpus at all must skip. Only the child process sees the path.
fn e2e_image_env() -> Vec<(&'static str, std::ffi::OsString)> {
    // The pk boot, NFC and power rows load `pk`; the Wi-Fi scan row loads `demo`.
    const IDS: [(&str, &str); 2] = [("PEMU_E2E_IMAGE_PK", "pk"), ("PEMU_E2E_IMAGE_DEMO", "demo")];
    let Ok(root) = host_data_root().map(|root| root.join("corpus")) else {
        return Vec::new();
    };
    IDS.iter()
        .filter_map(|(var, id)| {
            let dir = root.join(id);
            dir.is_dir().then(|| (*var, dir.into_os_string()))
        })
        .collect()
}

/// A loopback port that was free a moment ago, or `None` when none can be bound.
pub fn free_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    listener.local_addr().ok().map(|addr| addr.port())
}

#[cfg(test)]
mod benchmark_tests {
    use super::*;

    #[test]
    fn benchmarks_are_not_run_while_no_suite_runs_and_gate_once_one_does() {
        assert!(benchmark_plan(false).unwrap_err().contains("no F-suite"));
        let steps = benchmark_plan(true).unwrap();
        // The measuring step applies the hard native gates of M5 and M6.
        assert!(steps[0].1.ends_with(&["bench", "--gate-exits"]));
        assert!(steps[0].1.contains(&"--release"));
        // The model check gates every F3 row on macOS, the native rows elsewhere.
        if cfg!(target_os = "macos") {
            assert!(steps[1].1.ends_with(&["--check-model", "--gate"]));
        } else {
            assert!(steps[1].1.ends_with(&["--gate", "--gate-engine", "native"]));
        }
        assert!(steps[1].1.contains(&"--check-model"));
        // F1 to F6 run, so T2 runs both steps.
        assert!(crate::bench::any_suite_runnable());
    }
}
