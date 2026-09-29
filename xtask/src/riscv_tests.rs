//! `cargo xtask riscv-tests`: the riscv-tests conformance suite under `ref_step`.
//!
//! ```text
//! cargo xtask riscv-tests                    verify the manifest, then run the tests (T0)
//! cargo xtask riscv-tests verify             verify the manifest only
//! cargo xtask riscv-tests fetch-build        rebuild the committed ELFs from pinned upstream
//!     [--commit <sha>] [--offline]
//! ```
//!
//! With no arguments it is the T0 step (`xtask/src/ci/tiers.rs` calls it that way): it
//! checks every SHA-256 and size in `crates/pemu-rv32/tests/data/riscv-tests/MANIFEST.toml`
//! against the committed ELFs and then runs `cargo test -p pemu-rv32 --test riscv_tests`. It
//! needs no network, no data root and no toolchain, because the ELFs are in the tree.
//!
//! `fetch-build` is the developer path and does need all three; `build.rs` documents it.

mod build;
mod manifest;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::Command;

use manifest::Manifest;

const USAGE: &str = concat!(
    "usage: cargo xtask riscv-tests                 verify the manifest and run the tests\n",
    "       cargo xtask riscv-tests verify          verify the manifest only\n",
    "       cargo xtask riscv-tests fetch-build [--commit <sha>] [--offline]"
);

/// Entry point of `cargo xtask riscv-tests`.
pub fn run(args: &[String]) -> Result<(), String> {
    match args.split_first() {
        None => {
            println!("{}", verify(&build::data_dir())?);
            cargo_test()
        }
        Some((first, rest)) => match first.as_str() {
            "verify" if rest.is_empty() => {
                println!("{}", verify(&build::data_dir())?);
                Ok(())
            }
            "fetch-build" => {
                let opts = parse_fetch_build(rest)?;
                println!("{}", build::fetch_build(&build::data_dir(), &opts)?);
                Ok(())
            }
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                Ok(())
            }
            other => Err(format!("unknown argument `{other}`\n{USAGE}")),
        },
    }
}

/// Parses the arguments after `fetch-build`.
fn parse_fetch_build(args: &[String]) -> Result<build::Options, String> {
    let mut opts = build::Options {
        commit: build::PINNED_COMMIT.to_string(),
        offline: false,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--commit" => {
                let value = iter
                    .next()
                    .ok_or_else(|| format!("--commit needs a value\n{USAGE}"))?;
                if value.len() != 40 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err("--commit needs a full 40-character commit hash".to_string());
                }
                opts.commit = value.to_lowercase();
            }
            "--offline" => opts.offline = true,
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    Ok(opts)
}

/// Verifies the manifest of `dir` and returns the line to print.
fn verify(dir: &Path) -> Result<String, String> {
    let path = dir.join("MANIFEST.toml");
    let text = std::fs::read_to_string(&path).map_err(|err| {
        format!(
            "cannot read {}: {} (run `cargo xtask riscv-tests fetch-build`)",
            path.display(),
            err.kind()
        )
    })?;
    let parsed = Manifest::parse(&text)?;
    let checked = parsed.verify(dir)?;
    let excluded = if parsed.excluded.is_empty() {
        String::new()
    } else {
        format!(", {} excluded", parsed.excluded.len())
    };
    Ok(format!(
        "riscv-tests: {checked} ELF(s) verified against {} at {}{excluded}",
        manifest::UPSTREAM,
        &parsed.commit[..parsed.commit.len().min(12)]
    ))
}

/// Runs `cargo test -p pemu-rv32 --test riscv_tests`, inheriting the output.
fn cargo_test() -> Result<(), String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .args(["test", "-p", "pemu-rv32", "--test", "riscv_tests"])
        .current_dir(crate::util::workspace_root())
        .status()
        .map_err(|err| format!("cannot run cargo test: {}", err.kind()))?;
    if status.success() {
        Ok(())
    } else {
        Err("cargo test -p pemu-rv32 --test riscv_tests failed".to_string())
    }
}

/// `<data root>/riscv-tests`: the clone and the build tree, neither of them committed.
fn work_root() -> Result<PathBuf, String> {
    Ok(crate::hostdirs::process_data_root()?.join("riscv-tests"))
}
