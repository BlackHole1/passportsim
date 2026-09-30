//! Unit tests of `xtask ci`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::corpus::{self, Outcome};
use super::model::{self, Status, StepResult};
use super::outcome::{self as outcome, TestOutcome};
use super::parse_args;
use super::receipt::{self, Receipt, json_string};
use super::runner::Ctx;

fn step(name: &str, status: Status, reason: &str, millis: u64) -> StepResult {
    StepResult {
        duration: Duration::from_millis(millis),
        ..StepResult::instant(name, status, reason)
    }
}

#[test]
fn json_string_escapes_quotes_backslashes_and_controls() {
    assert_eq!(json_string("plain"), "\"plain\"");
    assert_eq!(
        json_string("a\"b\\c\nd\re\tf\u{1}g\u{7f}h"),
        "\"a\\\"b\\\\c\\nd\\re\\tf\\u0001g\\u007fh\""
    );
    assert_eq!(json_string("caf\u{e9} \u{2014}"), "\"caf\u{e9} \u{2014}\"");
}

#[test]
fn receipt_json_has_every_field_and_escaped_steps() {
    let receipt = Receipt {
        tier: "t0".into(),
        groups: vec![],
        leg: "macos-aarch64".into(),
        os: "macos".into(),
        arch: "aarch64".into(),
        target: "aarch64-apple-darwin".into(),
        commit: "0123456789abcdef".into(),
        short_commit: "0123456".into(),
        dirty: Some(false),
        rustc: "rustc 1.93.0".into(),
        cargo: "cargo 1.93.0".into(),
        host: "aarch64-apple-darwin".into(),
        started_unix: 1_700_000_000,
        duration: Duration::from_millis(1500),
        steps: vec![
            step("fmt", Status::Pass, "", 12),
            step("layering", Status::Skipped, "xtask layering: \"stub\"\n", 3),
        ],
    };
    let json = receipt.to_json();
    for needle in [
        "\"schema\": \"passportsim/ci-receipt/5\",",
        "\"groups\": [],",
        "\"os\": \"macos\",",
        "\"arch\": \"aarch64\",",
        "\"target\": \"aarch64-apple-darwin\",",
        "\"dirty\": false,",
        "\"started_utc\": \"2023-11-14T22:13:20Z\",",
        "\"duration_ms\": 1500,",
        "\"result\": \"PASS\",",
        "\"counts\": {\"pass\": 1, \"fail\": 0, \"skipped\": 1, \"not_run\": 0, \"blocked\": 0, \"skipped_corpus\": 0},",
        "{\"name\": \"fmt\", \"status\": \"PASS\", \"reason\": \"\", \"duration_ms\": 12},",
        "\"reason\": \"xtask layering: \\\"stub\\\"\\n\", \"duration_ms\": 3}\n  ]\n}\n",
    ] {
        assert!(json.contains(needle), "missing {needle} in\n{json}");
    }
    assert_eq!(receipt.file_name(), "t0-0123456-20231114T221320Z.json");
    let empty = Receipt {
        steps: vec![],
        dirty: None,
        ..receipt
    };
    assert!(empty.to_json().ends_with("\"steps\": []\n}\n"));
    assert!(empty.to_json().contains("\"dirty\": null,"));
}

#[test]
fn utc_conversion() {
    assert_eq!(receipt::iso_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(receipt::iso_utc(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(receipt::iso_utc(4_107_542_399), "2100-02-28T23:59:59Z");
    assert_eq!(receipt::stamp_utc(1_700_000_000), "20231114T221320Z");
}

#[test]
fn data_root_from_config() {
    let home = Path::new("/home/u");
    let default = home.join("Library/Application Support/passportsim");
    assert_eq!(crate::hostdirs::data_root(home, None), Ok(default.clone()));
    assert_eq!(
        crate::hostdirs::data_root(home, Some("[efuse]\ndefault = \"synth\"\n")),
        Ok(default)
    );
    let tilde = "[paths]\ndata_root = \"~/data/pemu\"\n";
    assert_eq!(
        crate::hostdirs::data_root(home, Some(tilde)),
        Ok(home.join("data/pemu"))
    );
    let absolute = "[paths]\ndata_root = \"/srv/pemu\"\n";
    assert_eq!(
        crate::hostdirs::data_root(home, Some(absolute)),
        Ok(PathBuf::from("/srv/pemu"))
    );
    assert!(crate::hostdirs::data_root(home, Some("[paths]\ndata_root = 3\n")).is_err());
}

#[test]
fn status_aggregation() {
    assert_eq!(model::overall(&[]), Status::Pass);
    let mut steps = vec![
        step("a", Status::Pass, "", 1),
        step("b", Status::Skipped, "stub", 1),
        step("c", Status::NotRun, "no docker", 1),
    ];
    assert_eq!(model::overall(&steps), Status::Pass);
    steps.push(step("d", Status::Fail, "exit status 1", 1));
    steps.push(step("e", Status::Pass, "", 1));
    assert_eq!(model::overall(&steps), Status::Fail);
    let c = model::counts(&steps);
    assert_eq!((c.pass, c.fail, c.skipped, c.not_run), (2, 1, 1, 1));
    let table = model::summary_table(&steps);
    assert!(
        table.ends_with(
            "result: FAIL (2 pass, 1 fail, 1 skipped, 1 not run, 0 blocked, 0 skipped-corpus)"
        ),
        "{table}"
    );
    assert!(super::verdict("t0", &steps, None).is_err());
    assert!(super::verdict("t0", &steps[..3], None).is_ok());
    assert!(super::verdict("t0", &steps[..3], Some("disk full".into())).is_err());
}

const SAMPLE_LIST: &str = "\
m0::t0_m0_decoder_smoke: test
m0::t1_m0_banner: test
pemu_core::time::tests::t2_m5_long: test
crates/pemu-core/src/lib.rs - doc (line 3): test
m9::t1_m9a_browser: test
t1_m10_boot: test
m1::helper_t0_m1_not_a_claim: test
m1::t0_m1_bench: benchmark

6 tests, 1 benchmark
";

#[test]
fn test_names_read_the_test_lines_of_a_listing() {
    let names = outcome::test_names(SAMPLE_LIST);
    assert_eq!(names.len(), 7);
    assert_eq!(names[0], "m0::t0_m0_decoder_smoke");
    assert!(!names.iter().any(|n| n.contains("t0_m1_bench")));
}

#[test]
fn corpus_outcomes() {
    let text = "\
[pk]
bin = \"~/c/pk.bin\"
elf = \"/abs/pk.elf\"
pt = \"~/c/pt.bin\"
sha256 = { bin = \"AB\", elf = \"cd\" }

[demo]
bin = \"~/c/demo.bin\"
sha256 = { bin = \"ef\" }
";
    let home = Path::new("/h");
    // Compared as paths, component by component, not as text: on Windows `~/c/pk.bin` expands
    // to `/h\c/pk.bin`, which is the same path as `/h/c/pk.bin` there.
    let hash = |path: &Path| {
        if path == Path::new("/h/c/pk.bin") {
            Some("ab".to_string())
        } else if path == Path::new("/abs/pk.elf") {
            Some("00".to_string())
        } else {
            None
        }
    };
    let entries = corpus::check(text, home, &hash).expect("parses");
    let outcome = |id: &str, kind: &str| {
        entries
            .iter()
            .find(|e| e.id == id && e.kind == kind)
            .map(|e| e.outcome)
    };
    assert_eq!(entries.len(), 4);
    assert_eq!(outcome("pk", "bin"), Some(Outcome::Match));
    assert_eq!(outcome("pk", "elf"), Some(Outcome::Mismatch));
    assert_eq!(outcome("pk", "pt"), Some(Outcome::NoHash));
    assert_eq!(outcome("demo", "bin"), Some(Outcome::Missing));
    assert!(corpus::check("not = [toml", home, &hash).is_err());
}

/// A program is looked up once, `.cmd` and `.bat` are never executable, and a path with a
/// separator is taken as given.
#[test]
fn external_programs_resolve_without_a_shell() {
    use super::runner::{Tool, resolve};
    assert!(matches!(resolve("cargo"), Tool::Found(path) if path.is_absolute()));
    assert_eq!(resolve("pemu-no-such-program-1a2b3c"), Tool::Missing);
    assert_eq!(
        resolve("/nonexistent/pemu-no-such-program"),
        Tool::Missing,
        "a path with a separator is not searched on PATH"
    );
    let cargo = std::env::var_os("CARGO").expect("cargo runs the tests");
    assert!(matches!(resolve(&cargo.to_string_lossy()), Tool::Found(_)));
}

#[test]
fn argument_parsing() {
    let args = |text: &str| {
        text.split_whitespace()
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let opts = parse_args(&args("t0")).expect("parses");
    assert_eq!(opts.tier, "t0");
    assert!(opts.groups.is_empty());
    let opts = parse_args(&args("t0 --group test --group package")).expect("parses");
    assert_eq!(opts.groups, ["test", "package"]);
    assert!(parse_args(&args("--help")).expect("parses").help);
    for bad in [
        "",
        "t3",
        "t0 t1",
        "t0 --group",
        "t0 --group lint",
        "t1 --group test",
        "t0 --linux-container",
        "t0 --milestone 5",
        "t0 --emit-steps",
    ] {
        assert!(parse_args(&args(bad)).is_err(), "`{bad}` should not parse");
    }
}

/// A Windows receipt has the same fields, named from `std::env::consts` on that host.
#[test]
fn a_windows_receipt_names_its_host_and_target() {
    let receipt = Receipt {
        tier: "t0".into(),
        groups: vec!["checks".into(), "test".into()],
        leg: "windows-x86_64".into(),
        os: "windows".into(),
        arch: "x86_64".into(),
        target: "x86_64-pc-windows-msvc".into(),
        commit: "0123456789abcdef".into(),
        short_commit: "0123456".into(),
        dirty: Some(false),
        rustc: "rustc 1.93.0".into(),
        cargo: "cargo 1.93.0".into(),
        host: "x86_64-pc-windows-msvc".into(),
        started_unix: 1_700_000_000,
        duration: Duration::from_millis(1500),
        steps: vec![
            step("fmt", Status::Pass, "", 12),
            step(
                "w0-check-x86_64",
                Status::Skipped,
                "windows-target-missing",
                1,
            ),
        ],
    };
    let json = receipt.to_json();
    for needle in [
        "\"leg\": \"windows-x86_64\",",
        "\"os\": \"windows\",",
        "\"arch\": \"x86_64\",",
        "\"target\": \"x86_64-pc-windows-msvc\",",
        "\"reason\": \"windows-target-missing\"",
        "\"groups\": [\"checks\", \"test\"],",
    ] {
        assert!(json.contains(needle), "missing {needle} in\n{json}");
    }
    assert!(!json.contains("\"image\""));
    // A receipt from another host carries its leg in its name.
    assert_eq!(
        receipt.file_name(),
        "t0-0123456-20231114T221320Z-windows-x86_64-checks+test.json"
    );
}

/// The gating Playwright rows read `web/tests/browserCheck.ts`, and a missing browser is NOT_RUN
/// with the install hint, never a pass; anything else it prints is a failure of the check itself.
#[test]
fn a_browser_check_is_present_missing_with_its_hint_or_broken() {
    use super::tiers::{BrowserPresence, browser_presence};
    assert_eq!(browser_presence("PRESENT\n"), BrowserPresence::Present);
    assert_eq!(
        browser_presence(
            "$ bun tests/browserCheck.ts webkit\nMISSING install it with `bunx playwright install webkit`\n"
        ),
        BrowserPresence::Missing("install it with `bunx playwright install webkit`".to_string())
    );
    assert!(matches!(browser_presence(""), BrowserPresence::Broken(_)));
    assert!(matches!(
        browser_presence("PRESENTLY\n"),
        BrowserPresence::Broken(_)
    ));
}

/// End to end: a planted crate whose first test prints a `blocked: ` `SKIP` line, whose corpus
/// test prints the no-data-root line of `corpus_or_skip`, and whose third test passes, run through
/// `Ctx::test_step` with real `cargo test -- --show-output` output, lands in the receipt as
/// BLOCKED with its reason, SKIPPED-CORPUS and PASS, counted apart.
#[test]
fn a_planted_crate_run_by_a_test_step_is_blocked_skipped_corpus_and_pass_in_the_receipt() {
    let (root, mut ctx) = planted(
        "blocked",
        r#"
fn skip(test: &str, reason: &str) {
    println!("SKIP {test}: {reason}");
}
#[test]
fn t1_m4_pk_boot_console() {
    skip("t1_m4_pk_boot_console", "blocked: not written yet");
}
#[test]
fn t1_m4_needs_the_corpus() {
    skip(
        "t1_m4_needs_the_corpus",
        "corpus id `pk` unavailable: no data root: set PASSPORTSIM_DATA_ROOT to an absolute path",
    );
}
#[test]
fn t1_m4_passes() {
    assert_eq!(1 + 1, 2);
}
"#,
    );
    run_planted(&mut ctx, &root, "3 t1_ tests");

    let found: Vec<(&str, Status, &str)> = ctx
        .steps
        .iter()
        .map(|s| (s.name.as_str(), s.status, s.reason.as_str()))
        .collect();
    assert_eq!(found.len(), 3, "{found:?}");
    assert_eq!(found[0].0, "t1-tests");
    assert_eq!(found[0].1, Status::Pass, "{found:?}");
    assert!(
        found[0].2.contains("1 blocked, 1 skipped-corpus"),
        "{found:?}"
    );
    assert!(found.contains(&(
        "t1-tests.t1_m4_pk_boot_console",
        Status::Blocked,
        "blocked: not written yet"
    )));
    assert!(
        found
            .iter()
            .any(|s| s.0 == "t1-tests.t1_m4_needs_the_corpus" && s.1 == Status::SkippedCorpus)
    );
    let passed = ctx.tests.iter().find(|t| t.name == "t1_m4_passes");
    assert_eq!(
        passed.map(|t| t.status),
        Some(Status::Pass),
        "{:?}",
        ctx.tests
    );

    let receipt = Receipt {
        tier: "t1".into(),
        groups: vec![],
        leg: "macos-aarch64".into(),
        os: "macos".into(),
        arch: "aarch64".into(),
        target: "aarch64-apple-darwin".into(),
        commit: "0".into(),
        short_commit: "0".into(),
        dirty: None,
        rustc: String::new(),
        cargo: String::new(),
        host: String::new(),
        started_unix: 0,
        duration: Duration::ZERO,
        steps: ctx.steps.clone(),
    };
    let json = receipt.to_json();
    assert!(
        json.contains(
            "\"counts\": {\"pass\": 1, \"fail\": 0, \"skipped\": 0, \"not_run\": 0, \"blocked\": 1, \"skipped_corpus\": 1}"
        ),
        "{json}"
    );
    assert!(
        json.contains(
            "{\"name\": \"t1-tests.t1_m4_pk_boot_console\", \"status\": \"BLOCKED\", \"reason\": \"blocked: not written yet\""
        ),
        "{json}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A temporary crate whose `tests/m4.rs` is `source`, and a T1 context rooted in it.
fn planted(tag: &str, source: &str) -> (PathBuf, Ctx) {
    let root = std::env::temp_dir().join(format!(
        "pemu-xtask-ci-planted-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"planted\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(root.join("tests/m4.rs"), source).unwrap();
    let ctx = Ctx {
        root: root.clone(),
        tier: "t1".into(),
        log_dir: root.join("logs"),
        steps: Vec::new(),
        tests: Vec::new(),
    };
    (root, ctx)
}

/// `cargo test -- --show-output` of a planted crate as the step `t1-tests`.
fn run_planted(ctx: &mut Ctx, root: &Path, note: &str) {
    let mut cmd = ctx.command("cargo", &["test", "--offline", "--", "--show-output"]);
    cmd.env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("RUSTFLAGS")
        .env("CARGO_INCREMENTAL", "0");
    ctx.test_step("t1-tests", cmd, note);
}

/// End to end: a planted test that prints its marker under another test's name is
/// an orphan. The step fails and the orphan is a FAIL sub-step, although the harness passed every
/// test.
#[test]
fn a_planted_marker_for_another_name_fails_the_step() {
    let (root, mut ctx) = planted(
        "orphan",
        r#"
#[test]
fn t1_m4_pk_boot_console() {
    println!("SKIP t1_m4_pk_boot_consol: blocked: not written yet");
}
#[test]
fn t1_m4_passes() {}
"#,
    );
    run_planted(&mut ctx, &root, "2 t1_ tests");
    let step = &ctx.steps[0];
    assert_eq!(step.status, Status::Fail, "{:?}", ctx.steps);
    let orphan = ctx
        .steps
        .iter()
        .find(|s| s.name == "t1-tests.orphan-marker.t1_m4_pk_boot_consol")
        .expect("an orphan sub-step");
    assert_eq!(orphan.status, Status::Fail);
    assert!(orphan.reason.contains("printed by `t1_m4_pk_boot_console`"));
    let _ = std::fs::remove_dir_all(&root);
}

/// A Playwright JSON report in the shape `--reporter=json` writes: a passing run with `NOT_RUN`
/// annotations on its result, a `fixme` test, a skipped one, a failed one and a passing and a
/// failing test outside any `describe`.
const PLAYWRIGHT_JSON: &str = r#"{
  "config": {}, "errors": [], "stats": {"expected": 1},
  "suites": [
    {"title": "m9.spec.ts", "file": "m9.spec.ts", "line": 0, "column": 0, "specs": [], "suites": [
      {"title": "M9 on the real core", "file": "m9.spec.ts", "line": 452, "specs": [
        {"title": "official boots Wall-paced on SAB", "file": "m9.spec.ts", "line": 455, "tests": [
          {"projectName": "chromium", "status": "expected", "annotations": [
            {"type": "sab real-time factor (recorded)", "description": "0.98"}
          ], "results": [{"status": "passed", "annotations": [
            {"type": "NOT_RUN", "description": "sab s_sel-sab: no walkers"},
            {"type": "NOT_RUN", "description": "unpaced-webkit webkit: Playwright WebKit is not installed"}
          ]}]}
        ]}
      ]}
    ]},
    {"title": "cards.spec.ts", "file": "cards.spec.ts", "line": 0, "column": 0, "suites": [
      {"title": "environment cards", "file": "cards.spec.ts", "line": 19, "specs": [
        {"title": "pk reaches `pk_app: ready`", "file": "cards.spec.ts", "line": 20, "tests": [
          {"projectName": "chromium", "status": "skipped", "annotations": [{"type": "skip", "description": "no wasm core in the bundle"}], "results": []}
        ]},
        {"title": "the BLE card", "file": "cards.spec.ts", "line": 29, "tests": [
          {"projectName": "chromium", "status": "skipped", "annotations": [{"type": "fixme", "description": "waits on BLE"}], "results": []}
        ]},
        {"title": "at 400 px every card fits", "file": "cards.spec.ts", "line": 108, "tests": [
          {"projectName": "chromium", "status": "unexpected", "annotations": [], "results": [{"status": "failed"}]}
        ]}
      ]},
      {"title": "no describe", "file": "cards.spec.ts", "line": 1, "specs": [
        {"title": "WebGL1 draws a frame", "file": "gl.spec.ts", "line": 99, "tests": [
          {"projectName": "chromium", "status": "expected", "annotations": [], "results": []}
        ]},
        {"title": "playback over the shared ring plays a tone", "file": "audio.spec.ts", "line": 126, "tests": [
          {"projectName": "chromium", "status": "unexpected", "annotations": [], "results": [{"status": "failed"}]}
        ]}
      ]}
    ]}
  ]
}"#;

#[test]
fn a_playwright_report_lists_each_test_that_did_not_pass_and_the_not_run_legs() {
    let tests = super::web::read_report(PLAYWRIGHT_JSON).expect("the report reads");
    assert_eq!(tests.len(), 6);

    let subs = super::web::sub_steps("playwright-chromium-smoke", "chromium", &tests);
    let find = |name: &str| {
        subs.iter()
            .find(|s| s.name == format!("playwright-chromium-smoke.chromium.{name}"))
            .unwrap_or_else(|| panic!("no sub-step {name} in {subs:#?}"))
    };
    assert_eq!(find("sab.s_sel-sab").status, Status::NotRun);
    assert_eq!(find("sab.s_sel-sab").reason, "no walkers");
    assert_eq!(find("unpaced-webkit.webkit").status, Status::NotRun);
    assert_eq!(find("cards.spec.ts:20").status, Status::Skipped);
    assert_eq!(
        find("cards.spec.ts:20").reason,
        "pk reaches `pk_app: ready`; no wasm core in the bundle"
    );
    assert_eq!(find("cards.spec.ts:29").status, Status::Blocked);
    assert!(
        find("cards.spec.ts:29")
            .reason
            .ends_with("fixme: waits on BLE")
    );
    assert_eq!(find("cards.spec.ts:108").status, Status::Fail);
    let audio = find("audio.spec.ts:126");
    assert_eq!(
        (audio.status, audio.reason.as_str()),
        (
            Status::Fail,
            "playback over the shared ring plays a tone; the test failed (unexpected)"
        )
    );
    assert_eq!(
        subs.len(),
        6,
        "the passing tests are not listed, only their NOT_RUN legs: {subs:#?}"
    );

    assert!(
        super::web::summary(&tests).starts_with("6 tests: 2 pass, 2 fail, 1 blocked, 1 skipped")
    );
    assert!(super::web::read_report("{}").is_err());
}

/// On a host with no corpus the image rows are not BLOCKED on "no `pk` image given":
/// `web/tests/preconditions.ts` words the no-corpus case like the Rust corpus skips, so it reads
/// SKIPPED-CORPUS, and BLOCKED where the corpus is and the variable alone is unset.
#[test]
fn an_image_row_with_no_corpus_on_the_host_is_skipped_corpus_and_one_with_a_corpus_is_blocked() {
    let report = r#"{
  "config": {}, "errors": [], "stats": {"skipped": 2},
  "suites": [
    {"title": "cards.spec.ts", "file": "cards.spec.ts", "line": 0, "column": 0, "suites": [
      {"title": "cards", "file": "cards.spec.ts", "line": 40, "specs": [
        {"title": "pk reaches `pk_app: ready`", "file": "cards.spec.ts", "line": 51, "tests": [
          {"projectName": "chromium", "status": "skipped", "annotations": [{"type": "skip", "description": "corpus id `pk` unavailable: no data root: set PASSPORTSIM_DATA_ROOT to an absolute path; so no `pk` image can be given (PEMU_E2E_IMAGE_PK) and the pk boot does not run on this host (the corpus is macOS-only)"}], "results": []}
        ]},
        {"title": "the NFC counter", "file": "cards.spec.ts", "line": 109, "tests": [
          {"projectName": "chromium", "status": "skipped", "annotations": [{"type": "skip", "description": "blocked: the NFC counter: no `pk` image given (set PEMU_E2E_IMAGE_PK to a merged bin), and the firmware result waits on the NFC tap"}], "results": []}
        ]}
      ]}
    ]}
  ]
}"#;
    let tests = super::web::read_report(report).expect("the report reads");
    let subs = super::web::sub_steps("playwright-smoke", "chromium", &tests);
    let status = |line: &str| {
        subs.iter()
            .find(|s| s.name == format!("playwright-smoke.chromium.cards.spec.ts:{line}"))
            .unwrap_or_else(|| panic!("no row at line {line} in {subs:#?}"))
            .status
    };
    assert_eq!(status("51"), Status::SkippedCorpus);
    assert_eq!(status("109"), Status::Blocked);
}

#[test]
fn t1_and_t2_tests_run_in_release_and_t0_stays_debug() {
    use super::tiers::{REPLAY_TEST, prefixed_test_args, replay_args, test_profile};
    assert!(test_profile("t0").is_empty());
    assert_eq!(test_profile("t1"), ["--profile", "ci-test"]);
    assert_eq!(test_profile("t2"), ["--profile", "ci-test"]);
    let manifest = include_str!("../../../Cargo.toml");
    let profile = &manifest[manifest
        .find("[profile.ci-test]")
        .expect("the ci-test profile")..];
    assert!(profile.contains("inherits = \"release\"") && profile.contains("lto = false"));
    let t1 = prefixed_test_args("t1");
    assert_eq!(
        t1[..6],
        ["test", "--workspace", "--profile", "ci-test", "--", "t1_"]
    );
    let skipped: Vec<&str> = t1
        .windows(2)
        .filter(|w| w[0] == "--skip")
        .map(|w| w[1].as_str())
        .collect();
    assert!(skipped.contains(&REPLAY_TEST), "{t1:?}");
    assert!(skipped.contains(&"t1_m1_determinism_at_entry"), "{t1:?}");
    let t2 = prefixed_test_args("t2");
    assert!(t2.contains(&"ci-test".to_string()) && !t2.contains(&REPLAY_TEST.to_string()));
    let e94 = replay_args().join(" ");
    assert_eq!(
        e94,
        format!(
            "test --profile ci-test --workspace --test m9 -- --exact {REPLAY_TEST} --show-output"
        )
    );
    // Every determinism row uses the same profile and the workspace feature set, so the tier
    // builds its tests once.
    for row in super::determinism::ROWS {
        assert_eq!(
            super::determinism::test_args(row)[1..4],
            ["--profile", "ci-test", "--workspace"],
            "{}",
            row.step
        );
    }
}

#[test]
fn the_host_budget_tests_leave_t1_tests_and_run_alone_in_series() {
    use super::tiers::{HOST_BUDGET_TESTS, host_budget_args, prefixed_test_args};
    // The native perf tests of M5 and M6, and nothing else: the two absolute host-time budgets.
    assert_eq!(
        HOST_BUDGET_TESTS,
        ["t1_m5_native_perf", "t1_m6_native_perf_f6"]
    );
    // `t1-tests` skips each, so the tier's parallel run is never the load they measure...
    let t1 = prefixed_test_args("t1");
    let skipped: Vec<&str> = t1
        .windows(2)
        .filter(|w| w[0] == "--skip")
        .map(|w| w[1].as_str())
        .collect();
    for test in HOST_BUDGET_TESTS {
        assert!(skipped.contains(test), "t1-tests must skip {test}: {t1:?}");
    }
    // ...and `t2-tests` has no reason to know them.
    let t2 = prefixed_test_args("t2");
    assert!(
        HOST_BUDGET_TESTS
            .iter()
            .all(|t| !t2.contains(&t.to_string()))
    );
    // Their own step runs exactly those two, one thread at a time, in the tier's profile and
    // feature set so it reuses the `t1-tests` build.
    assert_eq!(
        host_budget_args().join(" "),
        "test --profile ci-test --workspace --test m5 --test m6 -- --exact \
         t1_m5_native_perf t1_m6_native_perf_f6 --show-output --test-threads=1"
    );
}

#[test]
fn the_host_budget_step_settles_until_the_load_is_half_the_cores_or_three_minutes_pass() {
    use super::tiers::{SETTLE_LOAD_PER_CORE, SETTLE_MAX, Settled, settle};
    use std::time::Duration;
    assert_eq!(SETTLE_LOAD_PER_CORE, 0.5);
    assert_eq!(SETTLE_MAX, Duration::from_secs(180));
    let poll = Duration::from_secs(5);
    // A load that decays from what `t1-tests` leaves behind: waited out, then run.
    let mut loads = [14.0, 11.0, 8.5, 6.5, 5.9, 3.0].into_iter();
    let mut slept = Vec::new();
    let got = settle(|| loads.next(), |d| slept.push(d), 6.0, SETTLE_MAX, poll);
    assert_eq!(
        got.load,
        Some(5.9),
        "stops at the first reading at or below the target"
    );
    assert_eq!(got.waited, Duration::from_secs(20));
    assert_eq!(slept, [poll; 4]);
    assert!(got.quiet());
    assert!(
        got.note().starts_with("settled 20 s after t1-tests"),
        "{}",
        got.note()
    );
    // At the target is quiet: the rule is "at most", like the contention rule's "above".
    let at = settle(|| Some(6.0), |_| panic!("no wait"), 6.0, SETTLE_MAX, poll);
    assert_eq!(at.waited, Duration::ZERO);
    // A host busy for its own reasons: bounded by SETTLE_MAX, never a wait without end, and the
    // note says the tests' contention rule judges it.
    let mut total = Duration::ZERO;
    let busy = settle(|| Some(13.0), |d| total += d, 6.0, SETTLE_MAX, poll);
    assert_eq!(total, SETTLE_MAX);
    assert_eq!(busy.waited, SETTLE_MAX);
    assert!(!busy.quiet());
    assert!(
        busy.note().contains("still 13.0 after 180 s"),
        "{}",
        busy.note()
    );
    assert!(busy.note().contains("contention rule"), "{}", busy.note());
    // A last poll shorter than the period: the wait never overshoots the bound.
    let mut steps = Vec::new();
    settle(
        || Some(13.0),
        |d| steps.push(d),
        6.0,
        Duration::from_secs(12),
        poll,
    );
    assert_eq!(steps, [poll, poll, Duration::from_secs(2)]);
    // A host with no load average cannot be waited on at all.
    let none = settle(|| None, |_| panic!("no wait"), 6.0, SETTLE_MAX, poll);
    assert_eq!(
        none,
        Settled {
            load: None,
            target: 6.0,
            waited: Duration::ZERO
        }
    );
    assert!(none.note().contains("no load average"));
}

#[test]
fn the_playwright_run_gets_a_port_the_core_the_record_and_a_json_report() {
    use super::tiers::{free_port, playwright_env};
    let port = free_port().expect("a loopback port");
    let env = playwright_env(
        Path::new("/logs/smoke-chromium.json"),
        Some(port),
        Some(Path::new("/target/pemu_wasm.wasm")),
        Some(Path::new("/logs/browser-record.json")),
    );
    let get = |key: &str| {
        env.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    };
    assert_eq!(get("PEMU_E2E_REQUIRE_BROWSERS").as_deref(), Some("1"));
    assert_eq!(get("PEMU_WEB_PORT"), Some(port.to_string()));
    assert_ne!(port, 4173, "not the config's fixed default");
    assert_eq!(
        get("PEMU_E2E_CORE").as_deref(),
        Some("/target/pemu_wasm.wasm")
    );
    assert_eq!(
        get("PEMU_BROWSER_RECORD").as_deref(),
        Some("/logs/browser-record.json")
    );
    assert_eq!(
        get("PLAYWRIGHT_JSON_OUTPUT_FILE").as_deref(),
        Some("/logs/smoke-chromium.json")
    );
    // T2 writes no record and a host with no core leaves the spec to skip with its reason.
    let bare = playwright_env(Path::new("/r.json"), None, None, None);
    assert!(
        !bare.iter().any(|(k, _)| *k == "PASSPORTSIM_DATA_ROOT"),
        "the data root is set per command by with_data_root, never here"
    );
    // The two the bare call always carries, plus the corpus images, which are the only entry
    // whose presence depends on the host. Asserting a length here instead would make the
    // test pass or fail on whether this machine has a corpus.
    let extra: Vec<&str> = bare
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| *k != "PEMU_E2E_REQUIRE_BROWSERS" && *k != "PLAYWRIGHT_JSON_OUTPUT_FILE")
        .collect();
    assert!(
        extra.iter().all(|k| k.starts_with("PEMU_E2E_IMAGE_")),
        "a bare call carries nothing but the two base variables and the corpus images: {extra:?}"
    );
}

/// The E14 rows need a firmware, not only a core.
///
/// Every variable this emits must name a directory that exists, because `preconditions.ts` throws
/// for one that does not. A host with no corpus emits none and the rows skip, which is the one
/// absence that may still skip a CI row.
#[test]
fn the_corpus_images_are_passed_only_for_directories_that_are_there() {
    use super::tiers::playwright_env;
    let env = playwright_env(Path::new("/r.json"), None, None, None);
    let images: Vec<(&str, &std::ffi::OsString)> = env
        .iter()
        .filter(|(k, _)| k.starts_with("PEMU_E2E_IMAGE_"))
        .map(|(k, v)| (*k, v))
        .collect();
    for (key, value) in &images {
        let dir = Path::new(value.as_os_str());
        assert!(
            dir.is_dir(),
            "{key} names {dir:?}, which is not a directory"
        );
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            match *key {
                "PEMU_E2E_IMAGE_PK" => Some("pk"),
                "PEMU_E2E_IMAGE_DEMO" => Some("demo"),
                other => panic!("unexpected image variable {other}"),
            },
            "the variable names the corpus id it is for"
        );
    }
    // On a host with the corpus both ids resolve; on one without, neither does. A run that found
    // one of the two would mean the corpus is half installed, which is worth knowing.
    assert!(
        images.len() == 2 || images.is_empty(),
        "expected both corpus images or neither, got {:?}",
        images.iter().map(|(k, _)| k).collect::<Vec<_>>()
    );
}

/// The order that makes the replay mean something: the smoke writes the record before the replay
/// reads it.
#[test]
fn t1_replays_the_browser_record_after_the_smoke() {
    let source = include_str!("tiers.rs");
    let body = |name: &str| {
        let start = source.find(&format!("\nfn {name}(ctx")).expect("tier fn");
        let end = source[start + 1..].find("\n}\n").unwrap() + start + 1;
        &source[start..end]
    };
    let t1 = body("t1");
    let at = |needle: &str| t1.find(needle).unwrap_or_else(|| panic!("{needle} in t1"));
    assert!(at("prefixed_tests(ctx, \"t1\")") < at("playwright("));
    assert!(at("browser_record_file(\"chromium\")") < at("browser_journal_replay("));
    assert!(at("playwright(") < at("browser_journal_replay("));
    assert!(!t1.contains("\"webkit\""), "the WebKit row is T2's");
    // The Firefox row runs in T1, after the Chromium smoke whose record the replay reads.
    assert_eq!(super::tiers::FIREFOX_STEP, "playwright-firefox");
    assert!(at("browser_journal_replay(") < at("playwright(ctx, FIREFOX_STEP, &[\"firefox\"]"));
    assert_eq!(super::tiers::host_gap("firefox", "macos"), None);
    assert_eq!(super::tiers::host_gap("firefox", "windows"), None);
    let t2 = body("t2");
    assert!(t2.contains("&[\"chromium\", \"webkit\"]"));
    // The strict-receipts step names the two `--strict` exit-code tests, not every `strict`.
    assert!(!t1.contains("&[\"strict\"]"));
    let receipt_rs = include_str!("../../../crates/pemu-api/src/receipt.rs");
    for test in [
        "fn a_class_u_touch_is_a_caveat_in_both_modes_and_exits_7_under_strict()",
        "fn an_unmodeled_first_touch_is_a_lenient_caveat_and_exits_7_under_strict()",
    ] {
        assert!(receipt_rs.contains(test), "{test}");
        assert!(t1.contains(test.trim_start_matches("fn ").trim_end_matches("()")));
    }
    assert_eq!(super::tiers::host_gap("chromium", "windows"), None);
    assert_eq!(super::tiers::host_gap("webkit", "macos"), None);
    assert!(
        super::tiers::host_gap("webkit", "windows")
            .unwrap()
            .contains("macOS-only")
    );
    // The installed Chrome and Edge rows are Windows-only, and a Windows T0 runs them with the
    // Chromium row that gates everywhere and the Firefox row.
    for row in [super::tiers::WINDOWS_CHROME, super::tiers::WINDOWS_MSEDGE] {
        assert_eq!(super::tiers::host_gap(row, "windows"), None);
        assert!(
            super::tiers::host_gap(row, "macos")
                .unwrap()
                .contains("Windows-only")
        );
    }
    assert_eq!(super::tiers::host_gap("firefox", "windows"), None);
    assert_eq!(
        super::tiers::WINDOWS_ROWS,
        ["chromium", "windows-chrome", "windows-msedge", "firefox"]
    );
    let t0 = body("t0");
    // The Windows rows run in T0 on the Windows host only, without the corpus, and T1
    // and T2 keep theirs.
    let windows = t0
        .find("std::env::consts::OS == \"windows\"")
        .expect("the Windows browser rows in t0");
    // Compared without whitespace, so rustfmt's line breaking cannot change the verdict.
    let leg: String = t0[windows..].split_whitespace().collect();
    assert!(
        leg.contains("playwright(ctx,WINDOWS_BROWSERS_STEP,&WINDOWS_ROWS,None,Corpus::Withheld,)"),
        "{leg}"
    );
    assert!(t1.contains("Corpus::Given") && t2.contains("Corpus::Given"));
}

#[test]
fn a_covered_step_carries_the_status_its_tests_recorded() {
    use super::tiers::covered_status;
    let outcome = |name: &str, status| TestOutcome {
        name: name.into(),
        status,
        reason: String::new(),
        partial: Vec::new(),
    };
    let found = ["m5::t1_m5_smoke", "m7::t1_m7_suite"];
    // Tests of this tier that no step recorded are not run, never a pass.
    assert_eq!(
        covered_status("t1", &found, &[]),
        (
            Status::NotRun,
            "; t1 tests: t1_m5_smoke not run by this tier, t1_m7_suite not run by this tier".into()
        )
    );
    let both = [
        outcome("t1_m5_smoke", Status::Pass),
        outcome("t1_m7_suite", Status::Blocked),
    ];
    let (status, why) = covered_status("t1", &found, &both);
    assert_eq!(status, Status::Blocked);
    assert_eq!(why, "; t1 tests: t1_m5_smoke PASS, t1_m7_suite BLOCKED");
    let passed = [
        outcome("t1_m5_smoke", Status::Pass),
        outcome("t1_m7_suite", Status::Pass),
        outcome("other", Status::Fail),
    ];
    assert_eq!(covered_status("t1", &found, &passed).0, Status::Pass);
    // Tests of other tiers only prove that the coverage exists.
    let unit = ["pemu_api::receipt::tests::exits_7_under_strict"];
    assert_eq!(
        covered_status("t1", &unit, &[]),
        (Status::Pass, "; no t1 test among them".into())
    );
}

#[test]
fn a_playwright_row_whose_core_did_not_build_fails() {
    use super::determinism::CoreBuild;
    use super::runner::Exec;
    let root = Path::new("/ws");
    let failed = CoreBuild::Failed(Exec {
        success: false,
        failure: "exit status: 101".into(),
        duration: Duration::ZERO,
        log: PathBuf::from("/ws/target/xtask-ci/t1/smoke-core-build.log"),
        stdout: String::new(),
    });
    assert_eq!(
        super::tiers::core_failure(&failed, root),
        (
            Status::Fail,
            "the wasm core did not build (exit status: 101); log target/xtask-ci/t1/smoke-core-build.log"
                .into()
        )
    );
    assert_eq!(
        super::tiers::core_failure(&CoreBuild::NoTarget, root).0,
        Status::NotRun
    );
}

#[test]
fn a_not_run_leg_naming_an_engine_that_ran_is_dropped() {
    let tests = super::web::read_report(PLAYWRIGHT_JSON).unwrap();
    let subs = super::web::sub_steps("playwright-chromium-webkit", "chromium", &tests);
    let has = |subs: &[StepResult], name: &str| subs.iter().any(|s| s.name == name);
    let leg = "playwright-chromium-webkit.chromium.unpaced-webkit.webkit";
    assert!(has(&subs, leg));
    let kept = super::web::drop_stale_engine_legs(subs.clone(), &["chromium"]);
    assert!(has(&kept, leg), "WebKit did not run: the leg stays NOT_RUN");
    let dropped = super::web::drop_stale_engine_legs(subs.clone(), &["chromium", "webkit"]);
    assert!(
        !has(&dropped, leg),
        "WebKit ran: the spec's fixed line is stale"
    );
    assert!(has(
        &dropped,
        "playwright-chromium-webkit.chromium.sab.s_sel-sab"
    ));
    assert_eq!(dropped.len(), subs.len() - 1);
}

#[test]
fn each_engine_writes_its_own_browser_record() {
    use super::tiers::browser_record_file;
    assert_eq!(
        browser_record_file("chromium"),
        "browser-record-chromium.json"
    );
    assert_ne!(
        browser_record_file("webkit"),
        browser_record_file("chromium")
    );
    let source = include_str!("tiers.rs");
    let body = &source[source.find("\nfn playwright(").unwrap()..];
    let body = &body[..body.find("\n}\n").unwrap()];
    let record = body
        .find("browser_record_file(engine)")
        .expect("per-engine record");
    assert!(record < body.find("remove_file(&record)").unwrap());
    assert!(body.find("remove_file(&record)").unwrap() < body.find("ctx.exec(&run_name").unwrap());
}

#[test]
fn a_declared_failure_is_blocked_and_report_errors_fail_the_row() {
    let json = r#"{"errors": [{"message": "Error: cannot load cards.spec.ts\n  at x"}], "suites": [
      {"title": "a.spec.ts", "file": "a.spec.ts", "specs": [
        {"title": "s_sel reads back", "file": "a.spec.ts", "line": 4, "tests": [
          {"expectedStatus": "failed", "status": "expected", "annotations": [{"type": "fail", "description": "walkers missing"}], "results": []}
        ]},
        {"title": "page focus", "file": "a.spec.ts", "line": 9, "tests": [
          {"expectedStatus": "failed", "status": "unexpected", "annotations": [{"type": "fail"}], "results": []}
        ]},
        {"title": "plain", "file": "a.spec.ts", "line": 12, "tests": [
          {"expectedStatus": "passed", "status": "expected", "annotations": [], "results": []}
        ]}
      ]}
    ]}"#;
    let tests = super::web::read_report(json).unwrap();
    assert_eq!(
        (tests[0].status, tests[0].reason.as_str()),
        (
            Status::Blocked,
            "expected to fail (test.fail): walkers missing"
        )
    );
    assert_eq!(tests[1].status, Status::Fail);
    assert!(tests[1].reason.contains("remove the marker"));
    assert_eq!(tests[2].status, Status::Pass);
    assert_eq!(
        super::web::report_errors(json),
        ["Error: cannot load cards.spec.ts"]
    );
    assert!(super::web::report_errors(PLAYWRIGHT_JSON).is_empty());
}

/// A passing Rust test that printed `NOT_RUN` lines, or ran some legs and skipped others, is
/// partial; its step status stays PASS.
#[test]
fn a_rust_test_with_legs_not_run_is_partial() {
    let stdout = "\
running 3 tests
test t1_m5_frame ... ok
test t1_m5_browser ... ok
test t1_m9b_replay ... ok

successes:

---- t1_m5_browser stdout ----
RAN t1_m5_browser golden
NOT_RUN t1_m5_browser s_sel: no walkers

---- t1_m9b_replay stdout ----
RAN t1_m9b_replay frames
SKIP t1_m9b_replay: the pcm digest waits on the audio path
";
    let report = outcome::read(stdout);
    let outcomes = report.outcomes();
    let find = |name: &str| outcomes.iter().find(|o| o.name == name).unwrap().clone();
    assert_eq!(find("t1_m5_frame").partial, Vec::<String>::new());
    let browser = find("t1_m5_browser");
    assert_eq!(
        (browser.status, browser.partial.clone()),
        (Status::Pass, vec!["s_sel: no walkers".to_string()])
    );
    let replay = find("t1_m9b_replay");
    assert_eq!(replay.status, Status::Pass);
    assert_eq!(
        replay.partial,
        vec!["skipped: the pcm digest waits on the audio path".to_string()]
    );
}

/// T0 clears the data-root override on its workspace `test` step, so the tier reports the
/// same thing whoever runs it: the command inherits the operator's environment, and a shell that
/// exports `PASSPORTSIM_DATA_ROOT` would run the corpus tests for real and report rows BLOCKED
/// that are SKIPPED-CORPUS on every other machine. T1 owns the corpus and injects the root per
/// step (`with_data_root`).
#[test]
fn the_t0_workspace_test_step_clears_the_data_root_override() {
    let (root, ctx) = planted("t0-data-root", "");
    let cmd = super::tiers::workspace_test_command(&ctx);
    let removed = cmd
        .get_envs()
        .any(|(key, value)| key == crate::hostdirs::DATA_ROOT_ENV && value.is_none());
    assert!(
        removed,
        "T0's workspace test step must remove {}, not merely leave it unset",
        crate::hostdirs::DATA_ROOT_ENV
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The cross-host parity test runs once, in its own T0 step. The workspace step skips
/// it by name and the parity step runs its binary, so a failing binary that sorts before
/// `cross_host` cannot keep the parity from being proved on a host.
#[test]
fn the_parity_test_runs_in_its_own_t0_step_and_the_workspace_step_skips_it() {
    let (root, ctx) = planted("t0-parity", "");
    let args = |cmd: &std::process::Command| -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    };
    let workspace = args(&super::tiers::workspace_test_command(&ctx));
    let skip = workspace.iter().position(|a| a == "--skip");
    assert_eq!(
        skip.and_then(|i| workspace.get(i + 1)).map(String::as_str),
        Some(super::tiers::PARITY_TEST),
        "{workspace:?}"
    );
    let parity = args(&super::tiers::parity_test_command(&ctx));
    assert!(
        parity.windows(2).any(|w| w == ["--test", "cross_host"]),
        "{parity:?}"
    );
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/milestones/cross_host/main.rs"
    ))
    .expect("the parity test's source");
    assert!(
        source.contains(&format!("fn {}()", super::tiers::PARITY_TEST)),
        "the skipped name is the parity test's"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The package tests run once, in the `package-tests` step: the workspace step skips them by the
/// filter that step runs, and the filter names the module `cargo xtask package`'s tests live in.
#[test]
fn the_package_tests_run_in_their_own_t0_step_and_the_workspace_step_skips_them() {
    let (root, ctx) = planted("t0-package", "");
    let args = |cmd: &std::process::Command| -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    };
    let workspace = args(&super::tiers::workspace_test_command(&ctx));
    assert!(
        workspace
            .windows(2)
            .any(|w| w == ["--skip", super::tiers::PACKAGE_TESTS]),
        "{workspace:?}"
    );
    let package = args(&super::tiers::package_test_command(&ctx));
    assert!(
        package.windows(2).any(|w| w == ["-p", "xtask"]),
        "{package:?}"
    );
    assert!(
        package.iter().any(|a| a == super::tiers::PACKAGE_TESTS),
        "{package:?}"
    );
    assert!(
        include_str!("../package.rs").contains("\nmod tests;")
            && include_str!("../package/tests.rs").contains("#[test]"),
        "the filter is the path of xtask's package::tests module"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A line that announces itself as a marker and then does not parse is reported, not dropped:
/// `SKIP <test> (esptool leg): ...` has whitespace in its name, which `test_name` rejects, and read
/// as ordinary output it would hide the esptool leg from the receipt.
#[test]
fn a_line_that_claims_to_be_a_marker_and_does_not_parse_is_reported() {
    let stdout = "\
running 1 test
SKIP t1_m2_x (esptool leg): no esptool on this host
test t1_m2_x ... ok
";
    let report = super::outcome::read(stdout);
    assert!(
        report.has_malformed(),
        "the whitespace-named SKIP line must be reported, not dropped: {:?}",
        report.malformed
    );
    assert_eq!(
        report.malformed,
        vec!["SKIP t1_m2_x (esptool leg): no esptool on this host"]
    );
    assert!(!report.has_orphans(), "it is malformed, not an orphan");

    // The documented form still parses and is not reported as malformed.
    let good = "\
running 1 test
NOT_RUN t1_m2_x esptool-leg: no esptool on this host
test t1_m2_x ... ok
";
    let report = super::outcome::read(good);
    assert!(!report.has_malformed(), "{:?}", report.malformed);
    assert_eq!(report.markers.len(), 1);
}

// ------------------------------------------------------------------------------------------------
// Rows proved by CI steps, and a row's tier read alike on every tier
// ------------------------------------------------------------------------------------------------

#[test]
fn unmeasured_lines_are_the_not_measured_and_not_run_ones() {
    let log = "fw-Og 0.97 of blockx, PASS\n      NOT MEASURED, load average 9.1 on 12 cores: F5\n\
               NOT_RUN windows-phase: macos-phase-incomplete\nnot measured in prose\n\
               the NOT_RUN word inside a line\n";
    assert_eq!(
        super::tiers::unmeasured(log),
        [
            "NOT MEASURED, load average 9.1 on 12 cores: F5",
            "NOT_RUN windows-phase: macos-phase-incomplete"
        ]
    );
}

/// The browser half of the clean-environment first run: T1 builds the package before its
/// Playwright smoke and hands the
/// smoke its directory; T2's run of the same spec gets none, because the row is T1's.
#[test]
fn t1_builds_the_package_before_the_smoke_and_names_it() {
    use super::tiers::{PACKAGE_ENV, package_dir_of, smoke_package_args};
    let version = env!("CARGO_PKG_VERSION");
    let printed = format!(
        "package: no demo embedded: the build host has no `official` corpus entry\n\
         package: /tmp/out/passportsim-{version}-macos-arm64\n\
         package: /tmp/out/passportsim-{version}-web\n\
         package: /tmp/out/passportsim-{version}-macos-arm64.tar.gz\n\
         package: secrets-check clean over 3 file(s)\n"
    );
    assert_eq!(
        package_dir_of(&printed),
        Some(PathBuf::from(format!(
            "/tmp/out/passportsim-{version}-macos-arm64"
        )))
    );
    assert_eq!(
        package_dir_of(&format!("package: /tmp/out/passportsim-{version}-web\n")),
        None
    );
    assert_eq!(package_dir_of("no package line\n"), None);
    let args = smoke_package_args("/tmp/out");
    assert!(args.ends_with(&[
        "package",
        "--target",
        "aarch64-apple-darwin",
        "--out",
        "/tmp/out",
        "--no-archive"
    ]));
    assert_eq!(PACKAGE_ENV, "PEMU_E2E_PACKAGE");
    let spec = include_str!("../../../web/tests/firstRun.spec.ts");
    assert!(spec.contains(&format!("\"{PACKAGE_ENV}\"")));
    let source = include_str!("tiers.rs");
    let t1 = &source
        [source.find("\nfn t1(ctx").unwrap()..source.find("\nfn browser_journal_replay").unwrap()];
    assert!(t1.find("smoke_package(ctx)").unwrap() < t1.find("playwright(").unwrap());
    assert!(t1.contains("package.as_deref()"));
    assert!(source.contains(
        "\"playwright-chromium-webkit\",\n        &[\"chromium\", \"webkit\"],\n        None,"
    ));
}
