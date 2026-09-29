//! `cargo xtask wasm`: the generated boundary between the wasm core and the web worker.
//!
//! # Subcommands
//!
//! - `cargo xtask wasm [--check]` writes `web/src/worker/layout.ts` from the Rust constants of
//!   `crates/pemu-wasm/src/layout.rs`. With `--check` nothing is written and the command fails
//!   when the committed file differs from what the constants produce. That form is the
//!   `wasm-layout-check` step of `xtask ci t0`, which is where the drift gate lives: it shells
//!   out to cargo, so it must not run from inside `cargo test`.
//!
//! # Where the numbers come from
//!
//! `pemu_wasm::tsgen::layout_ts` renders the file, and this module runs it through the
//! `pemu-layout-ts` binary of that crate rather than linking it. The crate graph has no edge from
//! `xtask` to `pemu-wasm`, and restating a single offset here would give the boundary two
//! sources of truth.

use std::path::Path;
use std::process::Command;

/// Path of the generated module, relative to the repository root.
pub const LAYOUT_TS: &str = "web/src/worker/layout.ts";

/// Entry point of `cargo xtask wasm`.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut check = false;
    for arg in args {
        match arg.as_str() {
            "--check" => check = true,
            other => {
                return Err(format!(
                    "unknown argument `{other}`\nusage: cargo xtask wasm [--check]"
                ));
            }
        }
    }
    let root = crate::util::workspace_root();
    let generated = generate(&root)?;
    let path = root.join(LAYOUT_TS);

    if check {
        let found = std::fs::read_to_string(&path)
            .map_err(|err| format!("{LAYOUT_TS} cannot be read ({err}); run `cargo xtask wasm`"))?;
        if found == generated {
            println!("wasm: {LAYOUT_TS} matches crates/pemu-wasm/src/layout.rs");
            return Ok(());
        }
        return Err(format!(
            "{LAYOUT_TS} is stale: {}\nrun `cargo xtask wasm` and commit the result",
            first_difference(&found, &generated)
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("{} cannot be created: {err}", parent.display()))?;
    }
    let unchanged = std::fs::read_to_string(&path).is_ok_and(|found| found == generated);
    if unchanged {
        println!("wasm: {LAYOUT_TS} is up to date");
        return Ok(());
    }
    std::fs::write(&path, &generated)
        .map_err(|err| format!("{LAYOUT_TS} cannot be written: {err}"))?;
    println!("wasm: wrote {LAYOUT_TS} ({} bytes)", generated.len());
    Ok(())
}

/// Runs the `pemu-layout-ts` binary of `pemu-wasm` and returns what it printed.
fn generate(root: &Path) -> Result<String, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let out = Command::new(&cargo)
        .current_dir(root)
        .args(["run", "-q", "-p", "pemu-wasm", "--bin", "pemu-layout-ts"])
        .output()
        .map_err(|err| format!("`{cargo} run -p pemu-wasm --bin pemu-layout-ts` failed: {err}"))?;
    if !out.status.success() {
        return Err(format!(
            "`{cargo} run -p pemu-wasm --bin pemu-layout-ts` exited with {}:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout)
        .map_err(|err| format!("pemu-layout-ts printed invalid UTF-8: {err}"))
}

/// A one-line description of where two versions of the file first differ, so a failing `--check`
/// says what changed instead of only that something did.
fn first_difference(found: &str, generated: &str) -> String {
    for (number, (a, b)) in found.lines().zip(generated.lines()).enumerate() {
        if a != b {
            return format!("line {} is `{a}`, the constants give `{b}`", number + 1);
        }
    }
    let (found_lines, generated_lines) = (found.lines().count(), generated.lines().count());
    if found_lines == generated_lines {
        return "the lines match but the trailing bytes differ".to_string();
    }
    format!("it has {found_lines} line(s), the constants give {generated_lines}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_argument_is_refused_with_the_usage() {
        let err = run(&["--force".to_string()]).expect_err("unknown arguments are refused");
        assert!(err.contains("--force"), "{err}");
        assert!(err.contains("usage: cargo xtask wasm"), "{err}");
    }

    #[test]
    fn the_first_difference_names_the_line_and_both_sides() {
        let message = first_difference("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(message, "line 2 is `b`, the constants give `B`");
    }

    #[test]
    fn a_length_difference_is_reported_as_a_line_count() {
        let message = first_difference("a\n", "a\nb\n");
        assert_eq!(message, "it has 1 line(s), the constants give 2");
    }

    // The drift gate itself is the `wasm-layout-check` step of `xtask ci t0`, not a test here:
    // running `cargo run -p pemu-wasm` from inside `cargo test` blocks on the build lock, needs a
    // writable target directory, and fails offline, which would make the only drift gate flaky.
}
