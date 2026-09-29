//! `flash_device`: the whole flash flow, in its fixed order, on the connected AI Passport.
//!
//! The installed `DevicePlanner` runs Discover, Plan, Rehearse, Confirm, Identify, Guard, Back
//! up, Write, Verify and Boot check; this file owns the argument shape, the `--dry-run` split, the
//! report, the `--help` text and the error mapping.
//!
//! Every run passes the confirmation step. The command is `destructive` and `human_confirm`: the
//! flow stops at Confirm and cannot go on without an `Accepted` naming this plan's digest. Which
//! path answers is the host's choice (`pemu_host::device`); a run whose path is unavailable is
//! `E_PLAN_REFUSED`. No tool call answers it, and the machine-checked rules run whatever answered.
//!
//! `--dry-run` never opens the device and enumerates nothing: it runs Plan and reports what the
//! host can say about the backup directory, so the refusal matrix runs with no port.

use crate::error::{
    ApiError, E_CARDID_CHANGED, E_DEVICE_BUSY, E_HOST_UNSUPPORTED, E_PLAN_REFUSED, E_USAGE,
};
use crate::output::Output;
use crate::receipt::Receipt;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

use super::plan_flash::{
    DeviceRequest, declined_backup, from_cli, render_plan, request_of, with_planner,
};

/// The steps a person watches during a real-device run, printed by `passportsim flash_device
/// --help`.
pub const REAL_DEVICE_STEPS: [&str; 6] = [
    "1. a full 8 MB backup is read twice with equal SHA-256 and written owner-only under the data \
     root, outside the repository (`--backup`, CLI only; a real flash takes it or `--no-backup`)",
    "2. the planner refuses unless the chip reports ESP32-C3 revision v1.1, flash manufacturer \
     0x20 and device 0x4017, and the device partition table has `cardid` at 0x356000 size 0x4000",
    "3. exactly the accepted plan is written, with `--flash_size keep` and never an erase-all",
    "4. `verify_flash` passes for every written region",
    "5. the cardid window MD5 before and after the write is equal",
    "6. the boot console shows the ROM banner and the flashed app's ELF SHA-256 prefix",
];

/// `None` for a command that adds none. The CLI asks for this when it builds the subcommand, so no
/// other crate repeats the list.
pub fn long_help(name: &str) -> Option<String> {
    if name != "flash_device" {
        return None;
    }
    let mut text = String::from(
        "The real-device flash, in its fixed order: Discover, Plan, Rehearse, Confirm, Identify, \
         Guard, Back up, Write, Verify, Boot check.\n\nConfirm must be answered before the port is \
         opened, and cardid [0x356000, 0x35A000) is never written or erased.\n\nA real-device run \
         checks:\n",
    );
    for step in REAL_DEVICE_STEPS {
        text.push_str("  ");
        text.push_str(step);
        text.push('\n');
    }
    text.push_str(
        "\nThis command is absent unless the CLI or the server was started with `--allow-device`.\n",
    );
    Some(text)
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "Arguments of `flash_device`: the image, the optional port, and `dry_run`, which stops before the device is opened.",
        "required": ["image"],
        "properties": {
            "image": {
                "type": "string",
                "description": "A merged `.bin` of the whole 8 MB part. A build directory is refused in v1."
            },
            "port": {
                "type": ["string", "null"],
                "description": "A discovered 303A:1001 port; required when several are attached."
            },
            "dry_run": {
                "type": "boolean",
                "description": "Plan only: nothing is enumerated, opened or rehearsed."
            },
            "erase_nvs": {
                "type": "boolean",
                "description": "CLI only, refused over MCP and HTTP: erase the `nvs` partition as well."
            },
            "backup": {
                "type": "boolean",
                "description": "CLI only, refused over MCP and HTTP: read the whole 8 MB part first and store it owner-only outside the repository."
            },
            "no_backup": {
                "type": "boolean",
                "description": "CLI only: flash without the whole 8 MB backup. A real flash needs one of `--backup` or `--no-backup`, so the backup is never skipped by forgetting a flag."
            }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "What the flow did: the plan digest, pass or fail per step, whether cardid was unchanged, and the boot check. No device value.",
        "required": ["plan_sha256", "steps", "dry_run"],
        "properties": {
            "plan_sha256": { "type": "string" },
            "dry_run": { "type": "boolean" },
            "steps": {
                "type": "array",
                "description": "The steps that ran, in order, each pass or fail.",
                "items": {
                    "type": "object",
                    "required": ["step", "ok"],
                    "properties": {
                        "step": { "type": "string" },
                        "ok": { "type": "boolean" }
                    }
                }
            },
            "rehearsal": {
                "type": "array",
                "description": "The same for the emulator rehearsal.",
                "items": { "type": "object" }
            },
            "cardid_unchanged": { "type": ["boolean", "null"] },
            "boot_check": { "type": ["object", "null"] },
            "backup": {
                "type": ["object", "null"],
                "description": "Backup evidence: verified, owner-only, outside the repository. Never a file name or a directory stem. A dry run reports `verified: false`, because nothing was read back."
            },
            "full_backup": {
                "type": ["object", "null"],
                "description": "The whole 8 MB backup, or `null` when none was asked for: `taken`, `verified` (two reads and the stored file agreed), `owner_only`, `inside_repository` and `bytes`. Never a file name, a directory stem or a digest.",
                "properties": {
                    "taken": { "type": "boolean" },
                    "verified": { "type": "boolean" },
                    "owner_only": { "type": "boolean" },
                    "inside_repository": { "type": "boolean" },
                    "bytes": { "type": "integer" }
                }
            },
            "plan": { "type": ["object", "null"] }
        }
    })
}

/// The steps in order, the cardid verdict and the boot check. No port path, backup file name, MAC
/// or cardid byte, because the report carries none.
pub fn render_report(report: &serde_json::Value) -> String {
    let mut text = String::new();
    if report
        .get("dry_run")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        text.push_str("dry run: nothing was opened, written or erased\n");
    }
    if let Some(plan) = report.get("plan") {
        text.push_str(&render_plan(plan));
    }
    let steps = |key: &str, label: &str, text: &mut String| {
        if let Some(list) = report.get(key).and_then(serde_json::Value::as_array) {
            for entry in list {
                let step = entry
                    .get("step")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let ok = entry
                    .get("ok")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                text.push_str(&format!(
                    "  {label} {step}: {}\n",
                    if ok { "pass" } else { "fail" }
                ));
            }
        }
    };
    steps("rehearsal", "rehearsal", &mut text);
    steps("steps", "device", &mut text);
    // Whether the whole part was stored. A run without `--backup` says so too, so the line is never
    // silently absent.
    match report
        .get("full_backup")
        .and_then(serde_json::Value::as_object)
    {
        Some(full) => {
            let flag = |name: &str| {
                full.get(name)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            };
            let bytes = full
                .get("bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            text.push_str(&format!(
                "full backup: {bytes} bytes, verified {}, owner-only {}, outside the repository \
                 {}\n",
                flag("verified"),
                flag("owner_only"),
                !flag("inside_repository"),
            ));
        }
        None if report.get("full_backup").is_some() => {
            text.push_str("full backup: none (no `--backup`)\n");
        }
        None => {}
    }
    match report
        .get("cardid_unchanged")
        .and_then(serde_json::Value::as_bool)
    {
        Some(true) => text.push_str("cardid unchanged\n"),
        Some(false) => text.push_str("cardid CHANGED\n"),
        None => {}
    }
    if let Some(boot) = report
        .get("boot_check")
        .and_then(serde_json::Value::as_object)
    {
        let flag = |name: &str| {
            boot.get(name)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        };
        text.push_str(&format!(
            "boot check: banner {}, ELF SHA-256 prefix {}\n",
            flag("banner"),
            flag("elf_sha256_matches")
        ));
    }
    text
}

/// A real flash typed on the CLI must answer the backup question: `--backup` takes it,
/// `--no-backup` declines it, and neither is `E_USAGE`. A dry run reads nothing, and an agent
/// transport cannot reach either flag, so neither is asked.
fn backup_decided(
    args: &serde_json::Value,
    request: &DeviceRequest,
    from_cli: bool,
) -> Result<(), ApiError> {
    if !from_cli || request.dry_run || request.backup || declined_backup(args) {
        return Ok(());
    }
    Err(ApiError::new(
        E_USAGE,
        "a real flash needs a backup decision: type `--backup` to store the whole 8 MB part \
         first, or `--no-backup` to flash without one. Only the sectors the \
         plan writes are backed up otherwise, so cardid and `nvs` would have no copy.",
    ))
}

/// Flash the connected AI Passport with a planned, confirmed and rehearsed image.
#[command(
    api_crate = crate,
    name = "flash_device",
    group = device,
    input_schema = input_schema,
    output_schema = output_schema,
    cli(positional = ["image"], cli_only = ["erase_nvs", "backup", "no_backup"]),
    annotations(destructive, human_confirm, native_only),
    errors(
        E_USAGE,
        E_PLAN_REFUSED,
        E_DEVICE_BUSY,
        E_CARDID_CHANGED,
        E_HOST_UNSUPPORTED
    ),
    example(
        title = "Plan and rehearse without opening the device",
        args = r#"{"image":"FoloToy-AI-Passport-8MB.bin","dry_run":true}"#,
    ),
)]
pub fn flash_device(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let request = request_of(&args, from_cli())?;
    backup_decided(&args, &request, from_cli())?;
    let report = with_planner(|planner| planner.flash(&request))?;
    let text = render_report(&report);
    Ok(Output::new(report, text, Receipt::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::find;
    use crate::spec::CapsGroup;

    #[test]
    fn the_command_is_destructive_native_only_and_needs_a_human() {
        let spec = find("flash_device").expect("registered");
        assert_eq!(spec.group, CapsGroup::Device);
        assert!(spec.annotations.native_only);
        assert!(spec.annotations.destructive);
        assert!(spec.annotations.human_confirm);
        assert!(!spec.annotations.read_only);
        for code in [E_PLAN_REFUSED, E_DEVICE_BUSY, E_CARDID_CHANGED] {
            assert!(spec.errors.contains(&code), "{}", code.name);
        }
    }

    #[test]
    fn the_help_names_the_six_real_device_steps() {
        let help = long_help("flash_device").expect("flash_device has long help");
        for step in REAL_DEVICE_STEPS {
            let head = step.split(',').next().expect("a step");
            assert!(help.contains(head), "{head}");
        }
        for (n, needle) in [
            (1, "8 MB backup"),
            (2, "0x4017"),
            (3, "accepted plan"),
            (4, "verify_flash"),
            (5, "cardid window MD5"),
            (6, "ELF SHA-256 prefix"),
        ] {
            assert!(help.contains(needle), "step {n}: {needle}");
        }
        assert!(help.contains("--allow-device"));
        assert!(long_help("plan_flash").is_none());
        assert!(long_help("device_boot_check").is_none());
    }

    /// The CLI schema still offers them, and the handler still refuses them from a tool call.
    #[test]
    fn the_agent_schema_offers_no_cli_only_argument() {
        for name in ["flash_device", "plan_flash"] {
            let spec = find(name).expect("registered");
            let full = (spec.input_schema)().to_value();
            let agent = spec.agent_input_schema().to_value();
            let properties = |schema: &serde_json::Value| -> Vec<String> {
                schema["properties"]
                    .as_object()
                    .expect("an object schema")
                    .keys()
                    .cloned()
                    .collect()
            };
            assert!(!spec.cli.cli_only.is_empty(), "{name}");
            for cli_only in spec.cli.cli_only {
                assert!(
                    properties(&full).iter().any(|p| p == cli_only),
                    "{name}: a person can still type `{cli_only}`"
                );
                assert!(
                    !properties(&agent).iter().any(|p| p == cli_only),
                    "{name}: `{cli_only}` is offered to an agent"
                );
            }
            // Everything else survives, and `image` is still required.
            assert!(properties(&agent).iter().any(|p| p == "image"), "{name}");
            assert_eq!(
                properties(&agent).len() + spec.cli.cli_only.len(),
                properties(&full).len(),
                "{name}: only the CLI-only properties were dropped"
            );
            assert_eq!(agent["required"], full["required"]);
            assert!(
                !serde_json::to_string(&agent)
                    .expect("json")
                    .contains("erase_nvs"),
                "{name}: not in a description either"
            );
        }
    }

    #[test]
    fn a_report_renders_the_steps_the_cardid_verdict_and_the_boot_check() {
        let report = serde_json::json!({
            "plan_sha256": "c".repeat(64),
            "dry_run": false,
            "rehearsal": [{"step": "Guard", "ok": true}, {"step": "Write", "ok": true}],
            "steps": [{"step": "Discover", "ok": true}, {"step": "Write", "ok": true}],
            "cardid_unchanged": true,
            "boot_check": {"banner": true, "elf_sha256_matches": true},
        });
        let text = render_report(&report);
        assert!(text.contains("  rehearsal Guard: pass"), "{text}");
        assert!(text.contains("  device Discover: pass"), "{text}");
        assert!(text.contains("cardid unchanged"), "{text}");
        assert!(
            text.contains("boot check: banner true, ELF SHA-256 prefix true"),
            "{text}"
        );
        assert!(!text.contains("dry run"), "{text}");
    }

    #[test]
    fn the_full_backup_is_rendered_or_reported_absent() {
        let with_backup = serde_json::json!({
            "plan_sha256": "c".repeat(64),
            "dry_run": false,
            "steps": [{"step": "Backup", "ok": true}],
            "full_backup": {
                "taken": true,
                "verified": true,
                "owner_only": true,
                "inside_repository": false,
                "bytes": 8_388_608u64,
            },
        });
        let text = render_report(&with_backup);
        assert!(
            text.contains(
                "full backup: 8388608 bytes, verified true, owner-only true, outside the \
                 repository true"
            ),
            "{text}"
        );
        // No path and no digest, because the report carries none.
        assert!(!text.contains('/') && !text.contains("sha256"), "{text}");

        let without = serde_json::json!({
            "plan_sha256": "", "dry_run": false, "steps": [], "full_backup": serde_json::Value::Null
        });
        assert!(
            render_report(&without).contains("full backup: none (no `--backup`)"),
            "a run without one says so"
        );
        // An older planner's report with no such member prints no line.
        let silent = serde_json::json!({"plan_sha256": "", "dry_run": false, "steps": []});
        assert!(!render_report(&silent).contains("full backup"));
    }

    #[test]
    fn a_real_cli_flash_needs_a_backup_decision() {
        let request = |args: &serde_json::Value| request_of(args, true).expect("a valid request");
        let real = serde_json::json!({"image": "i.bin"});
        let error = backup_decided(&real, &request(&real), true).expect_err("neither flag");
        assert_eq!(error.code, E_USAGE);
        assert!(error.message.contains("--backup"), "{}", error.message);
        assert!(error.message.contains("--no-backup"), "{}", error.message);

        for args in [
            serde_json::json!({"image": "i.bin", "backup": true}),
            serde_json::json!({"image": "i.bin", "no_backup": true}),
            serde_json::json!({"image": "i.bin", "dry_run": true}),
        ] {
            backup_decided(&args, &request(&args), true).expect("answered, or a dry run");
        }
        // An agent transport cannot type either flag, so it is never asked.
        let agent = serde_json::json!({"image": "i.bin"});
        backup_decided(&agent, &request(&agent), false).expect("no question for an agent");

        // Both flags at once is not an answer.
        let both = serde_json::json!({"image": "i.bin", "backup": true, "no_backup": true});
        let error = request_of(&both, true).expect_err("contradiction");
        assert_eq!(error.code, E_USAGE);
        // `no_backup` is CLI-only, like `backup`.
        let from_agent = serde_json::json!({"image": "i.bin", "no_backup": true});
        assert_eq!(
            request_of(&from_agent, false).expect_err("CLI only").code,
            E_PLAN_REFUSED
        );
    }

    #[test]
    fn a_dry_run_and_a_changed_cardid_are_visible() {
        let dry = serde_json::json!({"plan_sha256": "", "dry_run": true, "steps": []});
        assert!(render_report(&dry).starts_with("dry run: nothing was opened"));
        let changed = serde_json::json!({
            "plan_sha256": "", "dry_run": false, "steps": [], "cardid_unchanged": false
        });
        assert!(render_report(&changed).contains("cardid CHANGED"));
    }
}
