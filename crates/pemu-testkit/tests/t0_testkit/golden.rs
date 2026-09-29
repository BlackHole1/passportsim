//! Golden prefix runner self-tests: a match passes, a planted difference is found at the right
//! line and rendered with context, and a missing golden is a reason rather than a panic.

use pemu_testkit::golden::{self, CONTEXT_LINES, GoldenError, compare_prefix_bytes};
use pemu_verify::goldens::{Golden, Header, Kind, TextMismatch};
use pemu_verify::normalize::BootSelect;

/// A boot console, as a device or an oracle would print it. Six lines is enough to have context
/// on both sides of a planted difference ([`CONTEXT_LINES`] is 3).
const CONSOLE: &[&str] = &[
    "ESP-ROM:esp32c3-api1-20210207",
    "I (24) boot: ESP-IDF v5.5.3",
    "I (27) boot: Partition Table:",
    "I (196) esp_image: segment 0: paddr=00010020",
    "I (205) cpu_start: Pro cpu start user code",
    "I (210) main: Passport Keys v1.4.2",
];

fn console_bytes(lines: &[&str]) -> Vec<u8> {
    let mut text = lines.join("\n");
    text.push('\n');
    text.into_bytes()
}

fn golden_of(lines: &[&str]) -> Golden {
    let header = Header {
        kind: Some(Kind::SelfReviewed),
        source: "pemu".to_string(),
        image: "toy".to_string(),
        command: "cargo test -p pemu-testkit".to_string(),
        binary_sha256: "0".repeat(64),
        rom_sha256: "1".repeat(64),
        efuse_sha256: "2".repeat(64),
        strap: "0xa".to_string(),
        provisional: true,
        extra: Default::default(),
    };
    let body = pemu_verify::normalize::normalize(&console_bytes(lines), BootSelect::LastBoot);
    Golden {
        header,
        body: body.to_text(),
    }
}

#[test]
fn t0_a_run_that_matches_the_golden_passes_with_the_line_count() {
    let golden = golden_of(CONSOLE);
    let lines = compare_prefix_bytes(
        "toy.console.txt",
        &golden,
        &console_bytes(CONSOLE),
        BootSelect::LastBoot,
        None,
    )
    .expect("the same console matches its own golden");
    assert_eq!(lines, CONSOLE.len());
}

#[test]
fn t0_a_planted_difference_is_reported_at_its_line_with_context() {
    let golden = golden_of(CONSOLE);
    let mut drifted: Vec<&str> = CONSOLE.to_vec();
    drifted[4] = "I (205) cpu_start: Pro cpu start user code (drifted)";

    let failure = compare_prefix_bytes(
        "toy.console.txt",
        &golden,
        &console_bytes(&drifted),
        BootSelect::LastBoot,
        None,
    )
    .expect_err("line 5 differs");

    assert_eq!(
        failure.mismatch,
        TextMismatch::Line {
            at: 4,
            // The normalizer rewrote the timestamp to `(T)` before the comparison.
            expected: "I (T) cpu_start: Pro cpu start user code".to_string(),
            actual: "I (T) cpu_start: Pro cpu start user code (drifted)".to_string(),
        },
        "the index is the first differing line, 0-based"
    );

    let report = &failure.report;
    assert!(report.starts_with("line 5 differs:"), "{report}");
    assert!(
        report.contains("   5 - I (T) cpu_start: Pro cpu start user code"),
        "the golden line is printed with `-`: {report}"
    );
    assert!(
        report.contains("   5 + I (T) cpu_start: Pro cpu start user code (drifted)"),
        "the run's line is printed with `+`: {report}"
    );
    assert!(
        report.contains("   2   I (T) boot: ESP-IDF v5.5.3"),
        "{CONTEXT_LINES} lines of context precede it: {report}"
    );
    assert!(
        report.contains("   6   I (T) main: Passport Keys <masked>"),
        "and follow it, with the version tail masked: {report}"
    );
    assert!(failure.to_string().contains("toy.console.txt"));
}

#[test]
fn t0_a_run_that_stops_early_reports_the_first_missing_line() {
    let golden = golden_of(CONSOLE);
    let failure = compare_prefix_bytes(
        "toy.console.txt",
        &golden,
        &console_bytes(&CONSOLE[..3]),
        BootSelect::LastBoot,
        Some(5),
    )
    .expect_err("the run produced 3 of the 5 claimed lines");

    assert_eq!(
        failure.mismatch,
        TextMismatch::TooShort {
            claimed: 5,
            produced: 3,
        }
    );
    assert!(failure.report.contains("the first missing line is 4"));
    assert!(failure.report.contains("   4 + <end of run>"));
}

#[test]
fn t0_claiming_more_lines_than_the_golden_holds_blames_the_claim() {
    let golden = golden_of(&CONSOLE[..2]);
    let failure = compare_prefix_bytes(
        "toy.console.txt",
        &golden,
        &console_bytes(CONSOLE),
        BootSelect::LastBoot,
        Some(6),
    )
    .expect_err("the golden holds 2 lines");
    assert_eq!(
        failure.mismatch,
        TextMismatch::OverClaimed {
            claimed: 6,
            golden: 2,
        }
    );
    assert!(failure.report.contains("the claim is wrong, not the run"));
}

#[test]
fn t0_a_golden_that_is_not_there_is_a_reason_not_a_panic() {
    let err = golden::committed("no-such-image/no-such.console.txt")
        .expect_err("nothing is committed under that name");
    assert_eq!(
        err,
        GoldenError::NotFound {
            name: "no-such-image/no-such.console.txt".to_string(),
            committed: true,
        }
    );
    assert!(err.to_string().contains("tests/golden"), "{err}");
}

#[test]
fn t0_the_repository_root_is_the_workspace_that_holds_this_crate() {
    let root = golden::repo_root();
    assert!(
        root.join("crates/pemu-testkit/Cargo.toml").is_file(),
        "the golden runner resolves the tree from its own manifest directory"
    );
    assert!(root.join("docs/ARCHITECTURE.md").is_file());
}
