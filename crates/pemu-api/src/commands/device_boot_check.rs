//! `device_boot_check`: read the boot console of the connected AI Passport and judge it against an
//! image.
//!
//! Standalone it resets nothing unless asked, and an idle, booted Passport is silent (0 bytes over
//! 6 s), so a bare listen sees no banner. `reset` restarts the device on the port the command
//! already holds and reads what that restart prints. The host redacts the console first; the result
//! carries the verdict and counts, never the log. `console_empty` and `console_bytes` say whether
//! the device said anything, which `lines_omitted` (what redaction dropped) cannot.
//!
//! It is `human_confirm` and `native_only`, and neither `read_only` nor `idempotent`: `reset`
//! reboots the owner's hardware, and a too-permissive hint costs an unattended retry that reboots a
//! device. `destructive` stays false: no flash content changes. The `reset` prompt's digest is
//! domain-separated from the plain read's.

use crate::error::{ApiError, E_DEVICE_BUSY, E_HOST_UNSUPPORTED, E_PLAN_REFUSED, E_USAGE};
use crate::output::Output;
use crate::receipt::Receipt;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

use super::plan_flash::{from_cli, request_of_with, with_planner};

/// `plan_flash` and `flash_device` refuse `reset` as an unknown field rather than ignore it.
const OWN_KEYS: &[&str] = &["reset"];

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "Arguments of `device_boot_check`: the image whose app ELF SHA-256 the boot console must name.",
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
            "reset": {
                "type": "boolean",
                "default": false,
                "description": "Restart the device first, then read the boot console the restart produces. Without it an already-booted Passport is silent and no banner can be seen. The restart is confirmed in its own words; no byte is written to the device."
            }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "The boot check: a ROM banner was seen, and the printed ELF SHA-256 prefix starts the image's. The console text itself is never returned, only verdicts and counts.",
        "required": ["banner", "elf_sha256_matches", "passed", "console_empty", "console_bytes", "reset"],
        "properties": {
            "banner": { "type": "boolean", "description": "A `rst:0x.. ,boot:0x..` line was read." },
            "elf_sha256_matches": { "type": "boolean" },
            "passed": { "type": "boolean" },
            "console_empty": {
                "type": "boolean",
                "description": "The device sent nothing at all in the read window. It is a different outcome from a console that carried lines and did not match, and it is what an idle, already-booted Passport does: ask for `reset` to make it speak."
            },
            "console_bytes": {
                "type": "integer",
                "minimum": 0,
                "description": "How many raw bytes arrived from the port before the redaction. A count only: no console text is ever returned."
            },
            "reset": {
                "type": "boolean",
                "description": "Whether this call restarted the device before reading."
            },
            "lines_omitted": {
                "type": ["integer", "null"],
                "minimum": 0,
                "description": "How many console lines the *redaction* dropped. Not a measure of silence: `0` also means everything that arrived was kept, so read it with `console_empty`."
            }
        }
    })
}

/// An empty console renders as silence and nothing else, because "not spoken" and "booted the wrong
/// image" call for opposite actions; the `lines_omitted` line is left out, since redacting nothing
/// says nothing.
pub fn render_check(check: &serde_json::Value) -> String {
    let flag = |name: &str| {
        check
            .get(name)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    let mut text = format!(
        "boot check: banner {}, ELF SHA-256 prefix {}\n",
        flag("banner"),
        flag("elf_sha256_matches")
    );
    if flag("console_empty") {
        text.push_str(
            "console: the device sent nothing (0 bytes in the read window), which is not the same \
             as a console that did not match\n",
        );
        if !flag("reset") {
            text.push_str(
                "an already-booted Passport is silent; pass `reset` to restart it and read the \
                 boot console that restart produces\n",
            );
        }
    } else {
        if let Some(bytes) = check
            .get("console_bytes")
            .and_then(serde_json::Value::as_u64)
        {
            text.push_str(&format!(
                "console: {bytes} byte(s) of device output arrived\n"
            ));
        }
        if let Some(omitted) = check
            .get("lines_omitted")
            .and_then(serde_json::Value::as_u64)
        {
            text.push_str(&format!(
                "{omitted} line(s) of device output were dropped by the redaction\n"
            ));
        }
    }
    text.push_str(if flag("passed") {
        "passed\n"
    } else {
        "failed\n"
    });
    text
}

/// Read the AI Passport's boot console against an image, optionally restarting it first.
#[command(
    api_crate = crate,
    name = "device_boot_check",
    group = device,
    input_schema = input_schema,
    output_schema = output_schema,
    cli(positional = ["image"]),
    annotations(human_confirm, native_only),
    errors(E_USAGE, E_PLAN_REFUSED, E_DEVICE_BUSY, E_HOST_UNSUPPORTED),
    example(
        title = "Restart the Passport and check that it booted the official image",
        args = r#"{"image":"FoloToy-AI-Passport-8MB.bin","reset":true}"#,
    ),
)]
pub fn device_boot_check(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let request = request_of_with(&args, from_cli(), OWN_KEYS)?;
    let check = with_planner(|planner| planner.boot_check(&request))?;
    let text = render_check(&check);
    Ok(Output::new(check, text, Receipt::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::find;
    use crate::spec::CapsGroup;

    /// A flash that accepted `reset` without acting on it would answer a person expecting a reset
    /// with silence.
    #[test]
    fn only_the_command_that_declares_reset_accepts_it() {
        use super::super::plan_flash::{DeviceCaller, install, uninstall};

        let mut declared = Vec::new();
        for name in ["plan_flash", "flash_device", "device_boot_check"] {
            let spec = find(name).expect("registered");
            let schema = serde_json::to_value((spec.input_schema)()).expect("schema is JSON");
            let offers = schema["properties"].get("reset").is_some();
            declared.push((name, offers));
        }
        assert_eq!(
            declared,
            vec![
                ("plan_flash", false),
                ("flash_device", false),
                ("device_boot_check", true)
            ],
            "only `device_boot_check` may declare `reset`"
        );

        // A planner is installed so an accepted argument reaches one; only which arguments get that
        // far is asserted.
        let _gate = gate();
        install(DeviceCaller::Cli, Box::new(AcceptAll));
        for (name, offers) in declared {
            let spec = find(name).expect("registered");
            let args = serde_json::json!({"image": "x.bin", "reset": true});
            let outcome = (spec.handler)(&mut HandlerCx {}, args);
            if offers {
                let out = outcome.expect("the command that declares `reset` accepts it");
                assert_eq!(
                    out.to_json()["reset"],
                    true,
                    "`{name}` must pass the flag through to its planner"
                );
            } else {
                let error = outcome.expect_err("a command that ignores `reset` must refuse it");
                assert_eq!(error.code, E_USAGE, "`{name}`");
                assert!(
                    error.message.contains("unknown field `reset`"),
                    "`{name}`: {}",
                    error.message
                );
            }
            // `flash_device` may still refuse the call for its own reason, such as the backup
            // question; this asserts only that no shared key became unknown.
            let shared = (spec.handler)(&mut HandlerCx {}, serde_json::json!({"image": "x.bin"}));
            if let Err(e) = &shared {
                assert!(
                    !e.message.contains("unknown field"),
                    "`{name}` lost a shared argument: {}",
                    e.message
                );
            }
            // So the list is a filter and not an open door.
            let bogus = (spec.handler)(
                &mut HandlerCx {},
                serde_json::json!({"image": "x.bin", "nope": 1}),
            )
            .expect_err("an undeclared key is refused");
            assert_eq!(bogus.code, E_USAGE, "`{name}`");
            assert!(
                bogus.message.contains("unknown field `nope`"),
                "`{name}`: {}",
                bogus.message
            );
        }
        uninstall();
    }

    /// Shared with `plan_flash`'s tests: two private gates would guard one global with two locks.
    use super::super::plan_flash::test_gate as gate;

    /// Echoes the `reset` it was handed, so the test sees an accepted flag reach it.
    struct AcceptAll;

    impl super::super::plan_flash::DevicePlanner for AcceptAll {
        fn plan(
            &mut self,
            request: &super::super::plan_flash::DeviceRequest,
        ) -> Result<serde_json::Value, ApiError> {
            Ok(serde_json::json!({ "reset": request.reset }))
        }
        fn flash(
            &mut self,
            request: &super::super::plan_flash::DeviceRequest,
        ) -> Result<serde_json::Value, ApiError> {
            self.plan(request)
        }
        fn boot_check(
            &mut self,
            request: &super::super::plan_flash::DeviceRequest,
        ) -> Result<serde_json::Value, ApiError> {
            self.plan(request)
        }
    }

    /// The annotations describe what the command can do, and `reset` can pull EN.
    #[test]
    fn the_command_can_reset_so_it_is_not_read_only_and_is_native_only() {
        let spec = find("device_boot_check").expect("registered");
        assert_eq!(spec.group, CapsGroup::Device);
        assert!(spec.annotations.native_only);
        assert!(
            !spec.annotations.read_only,
            "a command that can pull EN must not claim `readOnlyHint: true`"
        );
        assert!(
            !spec.annotations.idempotent,
            "a second call can reboot the device again"
        );
        assert!(
            !spec.annotations.destructive,
            "no byte of flash is written or erased"
        );
        assert!(
            spec.annotations.human_confirm,
            "a port open, and a restart, are a person's decision"
        );
    }

    #[test]
    fn the_verdict_is_rendered_without_the_console() {
        let check = serde_json::json!({
            "banner": true, "elf_sha256_matches": true, "passed": true,
            "console_empty": false, "console_bytes": 3984, "reset": true, "lines_omitted": 12
        });
        let text = render_check(&check);
        assert!(
            text.starts_with("boot check: banner true, ELF SHA-256 prefix true"),
            "{text}"
        );
        assert!(
            text.contains("3984 byte(s) of device output arrived"),
            "{text}"
        );
        assert!(
            text.contains("12 line(s) of device output were dropped"),
            "{text}"
        );
        assert!(text.ends_with("passed\n"), "{text}");
        let failed = serde_json::json!({
            "banner": true, "elf_sha256_matches": false, "passed": false,
            "console_empty": false, "console_bytes": 90, "reset": true
        });
        assert!(render_check(&failed).ends_with("failed\n"));
    }

    /// An idle, booted device sends zero bytes, which "banner false, ELF SHA-256 prefix false"
    /// would misreport as a wrong image.
    #[test]
    fn silence_and_mismatch_render_differently() {
        let silent = serde_json::json!({
            "banner": false, "elf_sha256_matches": false, "passed": false,
            "console_empty": true, "console_bytes": 0, "reset": false, "lines_omitted": 0
        });
        let mismatch = serde_json::json!({
            "banner": true, "elf_sha256_matches": false, "passed": false,
            "console_empty": false, "console_bytes": 3984, "reset": true, "lines_omitted": 0
        });
        let silent_text = render_check(&silent);
        let mismatch_text = render_check(&mismatch);
        assert_ne!(silent_text, mismatch_text);

        assert!(
            silent_text.contains("the device sent nothing"),
            "{silent_text}"
        );
        // The line that caused the confusion is not shown when nothing arrived.
        assert!(
            !silent_text.contains("dropped by the redaction"),
            "a redaction that dropped nothing from nothing says nothing: {silent_text}"
        );
        // Without a reset, the render says what to do about it.
        assert!(silent_text.contains("pass `reset`"), "{silent_text}");
        // And with one, it does not repeat advice already taken.
        let silent_after_reset = render_check(&serde_json::json!({
            "banner": false, "elf_sha256_matches": false, "passed": false,
            "console_empty": true, "console_bytes": 0, "reset": true
        }));
        assert!(silent_after_reset.contains("the device sent nothing"));
        assert!(
            !silent_after_reset.contains("pass `reset`"),
            "{silent_after_reset}"
        );

        assert!(!mismatch_text.contains("sent nothing"), "{mismatch_text}");
        assert!(
            mismatch_text.contains("0 line(s) of device output were dropped"),
            "{mismatch_text}"
        );
        assert!(silent_text.ends_with("failed\n") && mismatch_text.ends_with("failed\n"));
    }

    /// So a client reading JSON alone can make the distinction.
    #[test]
    fn the_schemas_carry_the_reset_argument_and_the_empty_verdict() {
        let input = serde_json::to_value(input_schema()).expect("input schema is JSON");
        let reset = &input["properties"]["reset"];
        assert_eq!(reset["type"], "boolean");
        assert_eq!(reset["default"], false);
        assert_eq!(input["required"], serde_json::json!(["image"]));

        let output = serde_json::to_value(output_schema()).expect("output schema is JSON");
        let required = output["required"].as_array().expect("a required list");
        for name in ["console_empty", "console_bytes", "reset"] {
            assert!(
                required.iter().any(|r| r == name),
                "`{name}` must always be present, or a reader cannot rely on it"
            );
            assert!(
                output["properties"][name].is_object(),
                "{name} is undescribed"
            );
        }
        assert_eq!(output["properties"]["console_empty"]["type"], "boolean");
        assert_eq!(output["properties"]["console_bytes"]["type"], "integer");
        // `lines_omitted` keeps its own, different meaning.
        let omitted = output["properties"]["lines_omitted"]["description"]
            .as_str()
            .expect("a description");
        assert!(omitted.contains("redaction"), "{omitted}");
    }
}
