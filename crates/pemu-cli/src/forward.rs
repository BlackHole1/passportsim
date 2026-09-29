//! Reaching the daemon that shares instances between invocations.
//!
//! When a command goes to the daemon:
//!
//! 1. `--ephemeral` never forwards: the command runs in this process, instances and all.
//! 2. `start` forwards, spawning the daemon first when none answers: it creates what later calls
//!    share.
//! 3. A command whose input schema has an `instance` property forwards when a daemon answers; with
//!    none there is no shared instance, so it runs here against an empty pool, answering as a
//!    daemon with no instance would.
//! 4. Every other command (`doctor`) runs in this process.
//!
//! A forwarded command is one `tools/call` on the daemon's `/mcp` mount, not a REST route: the MCP
//! result carries both renderings, so a forwarded command prints the same bytes as in process.
//! Roles resolve from this process, so `PASSPORTSIM_HOME=<dir>` gives a test a daemon of its own.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use pemu_api::error::{ApiError, E_DAEMON, ErrorCode};
use pemu_api::receipt::{Receipt, Strictness, exit_code};
use pemu_api::spec::{CapsGroup, CommandSpec};
use pemu_host::daemon::{self, DaemonState, Discovery, DiscoveryStore, HttpProbe};
use pemu_host::paths::{HostPaths, OwnerOnlyFiles};

use crate::exit;
use crate::json::Value;
use crate::render::{self, Mode, Stream};
use crate::{Outcome, assertions_hold};

/// Errors when the runtime role could not be resolved.
pub fn store(paths: &HostPaths) -> Result<DiscoveryStore<'static>, String> {
    let dir = paths
        .runtime()
        .map_err(|e| format!("the runtime directory is not resolvable: {e}"))?;
    Ok(DiscoveryStore::new(dir, OwnerOnlyFiles::host()))
}

/// Staleness is decided by connecting, never by a process id. A discovery file that exists but is
/// refused (not owner-only, malformed) is an error, never silently ignored.
pub fn running(store: &DiscoveryStore<'_>) -> Result<Option<Discovery>, String> {
    match daemon::resolve(store, &HttpProbe) {
        Ok(DaemonState::Running(discovery)) => Ok(Some(discovery)),
        Ok(DaemonState::Stale(_) | DaemonState::NotRunning) => Ok(None),
        Err(e) => Err(format!(
            "the discovery file {}: {e}",
            store.path().display()
        )),
    }
}

/// Every caps group but `device`, which needs `--allow-device` and human confirmation. `--caps`
/// narrows only a client's MCP tool surface, so a CLI command is never refused for its group.
pub fn daemon_groups() -> BTreeSet<CapsGroup> {
    daemon_groups_with_device(false)
}

/// Every flash still needs human confirmation.
pub fn daemon_groups_with_device(allow_device: bool) -> BTreeSet<CapsGroup> {
    CapsGroup::ALL
        .into_iter()
        .filter(|group| allow_device || *group != CapsGroup::Device)
        .collect()
}

/// As `--caps` and the `/mcp` caps header take it.
pub fn caps_list(caps: &BTreeSet<CapsGroup>) -> String {
    caps.iter()
        .map(|group| group.caps_name())
        .collect::<Vec<_>>()
        .join(",")
}

/// Errors when the daemon could not be spawned or did not become ready.
pub fn ensure(store: &DiscoveryStore<'_>) -> Result<Discovery, String> {
    if let Some(discovery) = running(store)? {
        return Ok(discovery);
    }
    let exe = this_exe()?;
    let (child, discovery) = daemon::spawn(
        &exe,
        pemu_host::platform::daemon_spawn(),
        store,
        &HttpProbe,
        daemon::READY_TIMEOUT,
    )
    .map_err(|e| format!("the daemon did not start: {e}"))?;
    // The daemon runs in a session of its own and outlives this process, so the handle is dropped.
    drop(child);
    Ok(discovery)
}

/// So a checkout build spawns itself, not the first `passportsim` on `PATH`.
fn this_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("this executable's path is unknown: {e}"))
}

/// Rule 3 of the module header.
pub fn addresses_instance(spec: &CommandSpec) -> bool {
    (spec.input_schema)()
        .as_value()
        .get("properties")
        .and_then(|p| p.get("instance"))
        .is_some()
}

/// A `run` answers when the guest stops, bounded by its `wall_budget_ms` (default 30 s), so this is
/// that budget plus a minute, and never under ten minutes, because `scenario` has no single budget.
pub fn call_timeout(args: &Value) -> Duration {
    let budget = args
        .get("wall_budget_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Duration::from_millis(budget.saturating_add(60_000)).max(Duration::from_secs(600))
}

/// Returns the JSON-RPC `result`; errors when the daemon did not answer with an MCP tool result.
pub fn call(discovery: &Discovery, name: &str, args: &Value) -> Result<Value, String> {
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": format!("{}{name}", pemu_host::mcp_stdio::TOOL_PREFIX), "arguments": args },
    });
    // Every group the daemon runs: a CLI command is never caps-gated.
    let groups = caps_list(&daemon_groups());
    let mut headers = vec![(pemu_host::mcp_http::CAPS_HEADER, groups)];
    // The daemon reads `scenario` files under its roots, and this command's workspace is one of
    // them for this call.
    if name == "scenario"
        && let Some(root) = std::env::current_dir()
            .ok()
            .and_then(|cwd| pemu_host::hooks::workspace_root(&cwd))
            .and_then(|root| root.to_str().map(str::to_owned))
    {
        headers.push((pemu_host::hooks::SCENARIO_ROOT_HEADER, root));
    }
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let response = daemon::request_with_headers(
        discovery.addr(),
        "POST",
        "/mcp",
        &discovery.token,
        &headers,
        Some(request.to_string().as_bytes()),
        call_timeout(args),
    )
    .map_err(|e| format!("the daemon on port {} did not answer: {e}", discovery.port))?;
    if response.status != 200 {
        return Err(format!(
            "the daemon on port {} answered HTTP {}",
            discovery.port, response.status
        ));
    }
    let mut value: Value = serde_json::from_slice(&response.body)
        .map_err(|e| format!("the daemon's answer is not JSON: {e}"))?;
    if let Some(error) = value.get("error") {
        // The MCP adapter refusing the arguments themselves, which in process is the handler's
        // `E_USAGE`.
        if error["code"] == -32602 {
            let refusal = ApiError::new(
                pemu_api::error::E_USAGE,
                error["message"].as_str().unwrap_or_default(),
            );
            return Ok(serde_json::json!({
                "isError": true,
                "structuredContent": { "error": refusal.to_json() },
            }));
        }
        return Err(format!("the daemon refused the call: {error}"));
    }
    match value.get_mut("result").map(Value::take) {
        Some(result) if result.get("isError").is_some() => Ok(result),
        _ => Err("the daemon's answer is not an MCP tool result".to_string()),
    }
}

/// Rendered exactly as the same call in process.
pub fn outcome(command: &str, result: &Value, mode: Mode, strict: Strictness) -> Outcome {
    let structured = result.get("structuredContent").cloned().unwrap_or_default();
    if result.get("isError") == Some(&Value::Bool(true)) {
        return refused(&structured["error"], mode);
    }
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let receipt = structured
        .get("receipt")
        .and_then(Receipt::from_json)
        .unwrap_or_default();
    // The same rule as `main::success_code`, so a forwarded call exits exactly as in process.
    let holds = assertions_hold(&structured);
    let code = if let Some(code) = crate::exit::of_scenario_payload(command, &structured, strict) {
        code
    } else if crate::exit::carries_a_verdict(command) {
        exit_code(receipt.verdict(holds), &receipt.caveats(), strict)
    } else {
        exit_code(
            if holds {
                pemu_api::receipt::Verdict::Pass
            } else {
                pemu_api::receipt::Verdict::Fail
            },
            &[],
            strict,
        )
    };
    Outcome {
        code,
        stdout: match mode {
            Mode::Text => format!("{text}\n"),
            Mode::Json => format!("{structured}\n"),
        },
        stderr: String::new(),
    }
}

fn refused(envelope: &Value, mode: Mode) -> Outcome {
    let name = envelope["code"].as_str().unwrap_or_default();
    let code = ErrorCode::lookup(name).map_or(exit::INTERNAL, exit::of_code);
    let (text, stream) = match mode {
        Mode::Json => (format!("{envelope}\n"), Stream::Stdout),
        Mode::Text => {
            let known = ErrorCode::lookup(name);
            let mut error = ApiError::new(
                known.unwrap_or(pemu_api::error::E_INTERNAL),
                envelope["message"].as_str().unwrap_or_default(),
            );
            if let Some(hint) = envelope["hint"].as_str() {
                error = error.with_hint(hint);
            }
            if let Some(lines) = envelope["serial_tail"].as_array() {
                error = error.with_serial_tail(
                    lines
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                );
            }
            render::failure(&error, Mode::Text)
        }
    };
    match stream {
        Stream::Stdout => Outcome {
            code,
            stdout: text,
            stderr: String::new(),
        },
        Stream::Stderr => Outcome {
            code,
            stdout: String::new(),
            stderr: text,
        },
    }
}

/// Retryable, exit 8 INFRA.
pub fn daemon_error(why: &str) -> ApiError {
    ApiError::new(E_DAEMON, why.to_string())
        .retryable()
        .with_hint(
            "`passportsim serve` runs the daemon in the foreground and prints why it stops; \
             `--ephemeral` runs a command without it",
        )
}

/// The typed envelope on stdout in JSON mode, the sentence and hint on stderr in text mode.
pub fn infra(why: &str, mode: Mode) -> Outcome {
    crate::Outcome::refused(&daemon_error(why), mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_api::error::{E_TIMEOUT, E_USAGE};
    use pemu_api::output::Output;

    /// Rebuilt from `pemu_host::mcp_stdio`'s two public halves, so the test pins the shape the CLI
    /// reads.
    fn success(output: &Output) -> Value {
        serde_json::json!({
            "content": [{ "type": "text", "text": output.to_text() }],
            "structuredContent": output.to_json(),
            "isError": false,
        })
    }

    fn failure(error: &ApiError) -> Value {
        serde_json::json!({
            "content": [{ "type": "text", "text": error.message.clone() }],
            "structuredContent": { "error": error.to_json() },
            "isError": true,
        })
    }

    /// So an agent session not opened for a device flash cannot see or call one.
    #[test]
    fn the_device_group_is_absent_unless_allow_device() {
        assert!(!daemon_groups().contains(&CapsGroup::Device));
        assert!(!daemon_groups_with_device(false).contains(&CapsGroup::Device));
        assert!(daemon_groups_with_device(true).contains(&CapsGroup::Device));
        assert_eq!(
            daemon_groups_with_device(true).len(),
            daemon_groups().len() + 1,
            "only the Device group is added"
        );
        let mut device: Vec<&str> = pemu_api::registry::commands_in(CapsGroup::Device)
            .map(|spec| spec.name)
            .collect();
        device.sort_unstable();
        assert_eq!(device, ["device_boot_check", "flash_device", "plan_flash"]);
        // The default caps list never carries `device`, so its tools are not listed either.
        assert!(!caps_list(&daemon_groups()).contains("device"));
        assert!(caps_list(&daemon_groups_with_device(true)).contains("device"));
    }

    #[test]
    fn a_forwarded_success_prints_what_the_same_output_prints_in_process() {
        let output = Output::new(
            serde_json::json!({ "instance": "p1", "state": "paused" }),
            "p1 paused",
            Receipt::default(),
        );
        for mode in [Mode::Text, Mode::Json] {
            let outcome = outcome("status", &success(&output), mode, Strictness::Lenient);
            assert_eq!(outcome.stdout, render::success(&output, mode), "{mode:?}");
            assert_eq!(outcome.code, exit::PASS);
            assert!(outcome.stderr.is_empty());
        }
        let failed = Output::new(
            serde_json::json!({ "result": "fail" }),
            "",
            Receipt::default(),
        );
        assert_eq!(
            outcome("run", &success(&failed), Mode::Json, Strictness::Lenient).code,
            exit::FAIL,
            "a `result: fail` exits 1 exactly as in process"
        );
    }

    #[test]
    fn a_forwarded_refusal_prints_what_the_same_refusal_prints_in_process() {
        let error = ApiError::new(E_TIMEOUT, "until not met")
            .with_hint("call inspect")
            .with_serial_tail(vec!["I (91) main: tail".to_string()]);
        for mode in [Mode::Text, Mode::Json] {
            let forwarded = outcome("run", &failure(&error), mode, Strictness::Lenient);
            let local = crate::Outcome::refused(&error, mode);
            assert_eq!(forwarded, local, "{mode:?}");
        }
        assert_eq!(
            outcome(
                "run",
                &failure(&ApiError::new(E_USAGE, "bad")),
                Mode::Text,
                Strictness::Lenient
            )
            .code,
            exit::USAGE
        );
    }

    #[test]
    fn a_run_may_take_its_whole_wall_budget_and_a_minute_more() {
        assert_eq!(
            call_timeout(&serde_json::json!({})),
            Duration::from_secs(600)
        );
        assert_eq!(
            call_timeout(&serde_json::json!({ "wall_budget_ms": 900_000 })),
            Duration::from_secs(960)
        );
    }

    #[test]
    fn instance_commands_are_the_ones_with_an_instance_property() {
        let find = |name| pemu_api::registry::find(name).expect("registered");
        assert!(addresses_instance(find("run")));
        assert!(addresses_instance(find("status")));
        assert!(!addresses_instance(find("start")));
        assert!(!addresses_instance(find("doctor")));
    }

    #[test]
    fn the_daemon_runs_every_group_but_device() {
        let groups = daemon_groups();
        assert!(!groups.contains(&CapsGroup::Device));
        assert_eq!(groups.len(), CapsGroup::ALL.len() - 1);
        assert_eq!(
            caps_list(&BTreeSet::from([CapsGroup::Core, CapsGroup::Nfc])),
            "core,nfc"
        );
    }

    #[test]
    fn an_unreachable_daemon_is_e_daemon_exit_8_in_both_modes() {
        let outcome = infra("port 1 did not answer", Mode::Text);
        assert_eq!(outcome.code, exit::INFRA);
        assert!(outcome.stdout.is_empty());
        assert!(
            outcome
                .stderr
                .starts_with("error: E_DAEMON: port 1 did not answer\nhint: "),
            "{}",
            outcome.stderr
        );
        let outcome = infra("port 1 did not answer", Mode::Json);
        assert_eq!(outcome.code, exit::INFRA);
        let envelope: Value = serde_json::from_str(outcome.stdout.trim_end()).expect("JSON");
        assert_eq!(envelope["code"], "E_DAEMON");
        assert_eq!(envelope["number"], 20);
        assert_eq!(envelope["retryable"], true, "E_DAEMON is retryable");
    }
}
