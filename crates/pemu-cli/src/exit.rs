//! Exit codes of `passportsim`.
//!
//! | Code | Name | When |
//! |---|---|---|
//! | 0 | PASS | the command succeeded with no caveat |
//! | 1 | FAIL | the command refused what it was asked to do |
//! | 2 | USAGE | bad flags, or arguments outside the input schema |
//! | 3 | IMAGE_REJECTED | the image failed validation; only that |
//! | 4 | GUEST_FAULT | panic, watchdog, deadlock, boot loop |
//! | 5 | TIMEOUT | a virtual timeout (`E_TIMEOUT`) |
//! | 6 | WALL_TIMEOUT | the host budget ran out (`E_WALL_BUDGET`); retrying is meaningful |
//! | 7 | UNMODELED_HW | `--strict` and the firmware used unmodeled hardware |
//! | 8 | INFRA | an asset is missing or has the wrong digest, a path is not usable, or the daemon cannot be reached (`E_DAEMON`) |
//! | 9 | DEVICE_REFUSED | the device planner refused |
//! | 10 | PASS_WITH_CAVEATS | the run passed with caveats |
//! | 70 | INTERNAL | an emulator bug |
//!
//! The success side is [`pemu_api::receipt::exit_code`], including the `--strict` rule. This module
//! maps an [`ApiError`] to its code. Two mappings are deliberate:
//! - asset errors (`E_ASSET_MISSING`, `E_ASSET_HASH`) exit 8 INFRA, not 3: code 3 is only for the
//!   image the caller passed, and a missing bundled ROM or a moved corpus digest is the machine's
//!   state;
//! - `E_HOST_UNSUPPORTED` exits 2 USAGE: the invocation names something this host cannot do, which
//!   the caller has to change.

use pemu_api::error::{
    ApiError, E_ASSET_HASH, E_ASSET_MISSING, E_CARDID_CHANGED, E_DAEMON, E_DEADLOCK, E_DEVICE_BUSY,
    E_GUEST_PANIC, E_HOST_UNSUPPORTED, E_INTERNAL, E_PLAN_REFUSED, E_STUCK, E_TIMEOUT, E_UNMODELED,
    E_USAGE, E_WALL_BUDGET, ErrorCode,
};

pub const PASS: u8 = 0;
pub const FAIL: u8 = 1;
pub const USAGE: u8 = 2;
/// Reserved: no core command returns it yet, so only the table test names it.
#[allow(dead_code, reason = "code 3 is reserved: no command returns it yet")]
pub const IMAGE_REJECTED: u8 = 3;
pub const GUEST_FAULT: u8 = 4;
pub const TIMEOUT: u8 = 5;
pub const WALL_TIMEOUT: u8 = 6;
pub const UNMODELED_HW: u8 = 7;
/// An asset, a path or a server.
pub const INFRA: u8 = 8;
pub const DEVICE_REFUSED: u8 = 9;
/// Produced by [`pemu_api::receipt::exit_code`], not [`of_code`]: no error carries it.
#[allow(dead_code, reason = "the success side computes it in pemu-api")]
pub const PASS_WITH_CAVEATS: u8 = 10;
pub const INTERNAL: u8 = 70;

/// A scenario judges its steps against the receipt of the instance it ran on and reports its code
/// plus the runner's 6 and 8 (`pemu_api::scenario::exit_code`); the receipt at the top belongs to
/// the process. `--strict` still turns a pass with caveats into 7, as for `run`.
#[must_use]
pub fn of_scenario_payload(
    command: &str,
    json: &crate::json::Value,
    strict: pemu_api::receipt::Strictness,
) -> Option<u8> {
    if command != "scenario" {
        return None;
    }
    let code = u8::try_from(json.get("exit_code")?.as_u64()?).ok()?;
    Some(
        if strict == pemu_api::receipt::Strictness::Strict && code == PASS_WITH_CAVEATS {
            UNMODELED_HW
        } else {
            code
        },
    )
}

/// Only `run` and `scenario`, which judge something. Any other command's receipt caveats belong to
/// the instance's history, not the call; exiting 10 would make `--strict` (the CI default) fail a
/// `stop`. A CLI rule only: MCP and HTTP carry the caveats in the receipt.
#[must_use]
pub fn carries_a_verdict(command: &str) -> bool {
    matches!(command, "run" | "scenario")
}

#[must_use]
pub fn of_error(error: &ApiError) -> u8 {
    of_code(error.code)
}

#[must_use]
pub fn of_code(code: ErrorCode) -> u8 {
    match code.name {
        name if name == E_USAGE.name || name == E_HOST_UNSUPPORTED.name => USAGE,
        name if name == E_TIMEOUT.name => TIMEOUT,
        name if name == E_WALL_BUDGET.name => WALL_TIMEOUT,
        name if name == E_UNMODELED.name => UNMODELED_HW,
        name if name == E_GUEST_PANIC.name || name == E_DEADLOCK.name || name == E_STUCK.name => {
            GUEST_FAULT
        }
        name if name == E_ASSET_MISSING.name
            || name == E_ASSET_HASH.name
            || name == E_DAEMON.name =>
        {
            INFRA
        }
        name if name == E_PLAN_REFUSED.name
            || name == E_DEVICE_BUSY.name
            || name == E_CARDID_CHANGED.name =>
        {
            DEVICE_REFUSED
        }
        name if name == E_INTERNAL.name => INTERNAL,
        // Everything else is the command refusing what it was asked to do.
        _ => FAIL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_api::error::{E_LEASE, E_STATE, registered_error_codes};
    use pemu_api::receipt::{Caveat, CaveatKind, Strictness, Verdict, exit_code};

    #[test]
    fn the_exit_code_table_is_the_one_this_maps_to() {
        assert_eq!(of_code(E_USAGE), 2);
        assert_eq!(of_code(E_TIMEOUT), 5);
        assert_eq!(of_code(E_WALL_BUDGET), 6);
        assert_eq!(of_code(E_UNMODELED), 7);
        assert_eq!(of_code(E_GUEST_PANIC), 4);
        assert_eq!(of_code(E_ASSET_MISSING), 8);
        assert_eq!(of_code(E_DAEMON), 8, "an unreachable daemon is INFRA");
        assert_eq!(of_code(E_PLAN_REFUSED), 9);
        assert_eq!(of_code(E_INTERNAL), 70);
        assert_eq!(of_code(E_STATE), 1);
        assert_eq!(of_code(E_LEASE), 1);
    }

    #[test]
    fn every_registered_code_has_a_code_in_the_table() {
        let allowed = [
            PASS,
            FAIL,
            USAGE,
            IMAGE_REJECTED,
            GUEST_FAULT,
            TIMEOUT,
            WALL_TIMEOUT,
            UNMODELED_HW,
            INFRA,
            DEVICE_REFUSED,
            PASS_WITH_CAVEATS,
            INTERNAL,
        ];
        for code in registered_error_codes() {
            let exit = of_code(code);
            assert!(allowed.contains(&exit), "{}: {exit}", code.name);
            assert_ne!(exit, PASS, "{} must not exit 0", code.name);
        }
    }

    #[test]
    fn a_clean_success_is_zero_and_caveats_follow_the_strictness() {
        assert_eq!(exit_code(Verdict::Pass, &[], Strictness::Lenient), PASS);
        let caveat = [Caveat {
            kind: CaveatKind::ClassU,
            detail: "rmt.conf0".to_owned(),
        }];
        assert_eq!(
            exit_code(Verdict::PassWithCaveats, &caveat, Strictness::Lenient),
            PASS_WITH_CAVEATS
        );
        assert_eq!(
            exit_code(Verdict::PassWithCaveats, &caveat, Strictness::Strict),
            UNMODELED_HW
        );
    }
}
