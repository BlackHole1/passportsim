//! `cargo xtask ci t0|t1|t2`: the CI tiers.
//!
//! - T0 needs no corpus or device data and runs on every host.
//! - T1 needs the firmware corpus and local goldens; T2 adds the long determinism runs, the oracle
//!   diffs and the benchmarks. Both run on macOS only, because they need the corpus, `jsc`, QEMU
//!   and WebKit.
//!
//! Tests join a tier by name: `t1_*` and `t2_*` tests run in their tier, and T0 runs the whole
//! workspace. Every step runs even after a failure, and the command fails when any step failed.
//! Each run prints a summary table and writes a JSON receipt under `<data root>/receipts/`.

pub mod corpus;
mod determinism;
mod model;
mod outcome;
mod receipt;
mod runner;
#[cfg(test)]
mod tests;
pub mod tiers;
mod web;

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use model::StepResult;
use receipt::Receipt;
use runner::Ctx;

const USAGE: &str = "usage: cargo xtask ci t0|t1|t2";

/// Parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// `t0`, `t1` or `t2`.
    pub tier: String,
    pub help: bool,
}

/// Parses the arguments after `ci`.
pub fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut opts = Options {
        tier: String::new(),
        help: false,
    };
    for arg in args {
        match arg.as_str() {
            "t0" | "t1" | "t2" if opts.tier.is_empty() => opts.tier = arg.clone(),
            "-h" | "--help" => opts.help = true,
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    if opts.help {
        return Ok(opts);
    }
    if opts.tier.is_empty() {
        return Err(format!("missing tier\n{USAGE}"));
    }
    if opts.tier != "t0" && std::env::consts::OS != "macos" {
        return Err(format!(
            "{} runs on macOS only; this host is {}",
            opts.tier,
            std::env::consts::OS
        ));
    }
    Ok(opts)
}

/// Entry point of `cargo xtask ci`.
pub fn run(args: &[String]) -> Result<(), String> {
    let opts = parse_args(args)?;
    if opts.help {
        println!("{USAGE}");
        return Ok(());
    }
    let started = Instant::now();
    let started_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let mut ctx = Ctx::new(&opts.tier, &receipt::stamp_utc(started_unix));
    let rustc = ctx
        .query("rustc", &["--version"])
        .unwrap_or_else(|| "unknown".into());
    let cargo = ctx
        .query("cargo", &["--version"])
        .unwrap_or_else(|| "unknown".into());
    let host = ctx
        .query("rustc", &["-vV"])
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("host: ").map(String::from))
        })
        .unwrap_or_else(|| "unknown".into());

    let commit = ctx
        .query("git", &["rev-parse", "HEAD"])
        .unwrap_or_else(|| "unknown".into());
    let short_commit = ctx
        .query("git", &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".into());
    let dirty = ctx
        .query("git", &["status", "--porcelain"])
        .map(|s| !s.is_empty());
    let leg = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    tiers::run(&mut ctx, &opts.tier);

    let receipt = Receipt {
        tier: opts.tier.clone(),
        leg,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        target: host.clone(),
        commit,
        short_commit,
        dirty,
        rustc,
        cargo,
        host,
        started_unix,
        duration: started.elapsed(),
        steps: std::mem::take(&mut ctx.steps),
    };
    println!(
        "\n{}\ntotal: {}",
        model::summary_table(&receipt.steps),
        model::seconds(receipt.duration)
    );
    let written = crate::hostdirs::receipts_dir().and_then(|dir| receipt.write(&dir));
    match &written {
        Ok(path) => println!("receipt: {}", path.display()),
        Err(err) => eprintln!("receipt not written: {err}"),
    }
    verdict(&opts.tier, &receipt.steps, written.err())
}

/// `Err` when a step failed or the receipt could not be written.
fn verdict(tier: &str, steps: &[StepResult], receipt_error: Option<String>) -> Result<(), String> {
    let failed = model::counts(steps).fail;
    if failed > 0 {
        return Err(format!("{tier}: {failed} of {} steps failed", steps.len()));
    }
    receipt_error.map_or(Ok(()), |err| {
        Err(format!("{tier}: receipt not written: {err}"))
    })
}
