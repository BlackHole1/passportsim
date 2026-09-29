//! `cargo xtask test <filter>`: runs one test under a tier's profile and data-root rule.
//!
//! A corpus test needs `PASSPORTSIM_DATA_ROOT`, which only a tier sets
//! (`ci::tiers::with_data_root`). Exporting it by hand is refused by `secrets-check` and makes the
//! result depend on the operator; a full `ci t1` per edit is slow. This command applies the tier's
//! own rule to one filter. It is not a tier and writes no receipt.
//!
//! `--no-fail-fast` is the default: `cargo test` stops at the first failing binary, and the
//! milestone binaries sort by number, so a break that `m1` also covers would hide `m10` entirely.

use std::process::Command;

/// The tiers a filter can be run under, and the default.
const TIERS: [&str; 3] = ["t0", "t1", "t2"];
const DEFAULT_TIER: &str = "t1";

fn usage() -> String {
    format!(
        "usage: cargo xtask test <filter> [--tier {}] [--fail-fast]\n\
         \n\
         Runs the workspace tests whose name contains <filter>, with the tier's cargo profile and\n\
         the tier's data-root rule (t1 and t2 inject it, t0 clears it). Not a tier: it writes no\n\
         receipt and no milestone claim may cite it.",
        TIERS.join("|")
    )
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut filter: Option<&str> = None;
    let mut tier = DEFAULT_TIER;
    let mut fail_fast = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--tier" => {
                let value = rest
                    .next()
                    .ok_or_else(|| format!("--tier needs a value\n{}", usage()))?;
                tier = TIERS
                    .iter()
                    .find(|t| *t == value)
                    .ok_or_else(|| format!("unknown tier `{value}`\n{}", usage()))?;
            }
            "--fail-fast" => fail_fast = true,
            "-h" | "--help" => {
                println!("{}", usage());
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option `{other}`\n{}", usage()));
            }
            other => {
                if filter.replace(other).is_some() {
                    return Err(format!("one filter at a time\n{}", usage()));
                }
            }
        }
    }
    let filter = filter.ok_or_else(usage)?;

    let mut cmd = Command::new("cargo");
    cmd.arg("test").arg("--workspace");
    cmd.args(crate::ci::tiers::test_profile(tier));
    // `--no-fail-fast` is cargo's, so it goes before the `--`: passed after it, libtest sees it
    // and every run ends with "Unrecognized option: 'no-fail-fast'" before a test starts.
    if !fail_fast {
        cmd.arg("--no-fail-fast");
    }
    cmd.arg("--");
    cmd.arg(filter);
    cmd.arg("--show-output");

    // The tier's own rule, not this process's environment. T1 and T2 inject the data root so the
    // corpus tests run; T0 clears it, because a T0 result that depends on who ran it is not a T0
    // result.
    if tier == "t0" {
        cmd.env_remove(crate::hostdirs::DATA_ROOT_ENV);
    } else {
        crate::ci::tiers::with_data_root(&mut cmd);
    }

    let root_note = match cmd
        .get_envs()
        .find(|(k, _)| *k == crate::hostdirs::DATA_ROOT_ENV)
    {
        Some((_, Some(_))) => "data root injected",
        Some((_, None)) => "data root cleared",
        None => "data root untouched (none resolved)",
    };
    println!(
        "xtask test: filter `{filter}`, tier {tier}, {root_note}, \
         {}.\nThis is a developer loop, not a tier: it writes no receipt and no claim may cite it.",
        if fail_fast {
            "fail-fast"
        } else {
            "--no-fail-fast"
        }
    );

    let status = cmd
        .status()
        .map_err(|e| format!("could not run cargo test: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo test exited with {status}; run `cargo xtask ci {tier}` before claiming anything"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A filter is required: the command must never degenerate into a whole-workspace run that
    /// looks like a tier.
    #[test]
    fn a_missing_filter_is_a_usage_error() {
        let err = run(&args(&[])).expect_err("no filter");
        assert!(err.contains("usage:"), "{err}");
    }

    #[test]
    fn an_unknown_tier_is_refused_by_name() {
        let err = run(&args(&["t1_m10_monitor", "--tier", "t3"])).expect_err("t3 is no tier");
        assert!(err.contains("unknown tier `t3`"), "{err}");
    }

    #[test]
    fn two_filters_are_refused_rather_than_silently_dropping_one() {
        let err = run(&args(&["a", "b"])).expect_err("two filters");
        assert!(err.contains("one filter at a time"), "{err}");
    }

    /// T0 clears the override and T1 injects it, so a T0 answer never depends on the caller's
    /// environment.
    #[test]
    fn the_tier_decides_the_data_root_rule() {
        assert!(crate::ci::tiers::test_profile("t0").is_empty());
        assert_eq!(
            crate::ci::tiers::test_profile("t1"),
            &["--profile", "ci-test"]
        );
    }
}
