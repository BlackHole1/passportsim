//! `passportsim status`: lifecycle, virtual time, lease, cursors and pacing of every instance.
//!
//! It is not `needs_instance`: omitting `instance` lists every instance, and it is the one command
//! an agent can call before anything exists.
//!
//! `artifacts_root` is the one output allowed an absolute path, in native form after home-directory
//! redaction. The process installs it on the pool ([`crate::pool::Pool::set_artifacts_root`]); no
//! caller can set it.
//!
//! The row carries lifecycle, virtual time, lease, determinism class, fidelity summary
//! ([`Receipt::fidelity`]), cursors and the last event (`null` while nothing has been emitted).

use std::fmt::Write as _;

use crate::error::{ApiError, E_STATE, E_USAGE};
use crate::instance::InstanceId;
use crate::lease::LeaseHolder;
use crate::output::Output;
use crate::receipt::Receipt;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

use super::run::{event_name, stream_name};
use crate::args::{instance_schema, object, only, opt_str};
use crate::pool::{Pool, with_pool};
use crate::session::Session;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusArgs {
    /// One instance, or every live one when absent.
    pub instance: Option<String>,
}

impl StatusArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<StatusArgs, ApiError> {
        let args = object(value)?;
        only(args, &["instance"])?;
        Ok(StatusArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
        })
    }
}

pub fn status_on(pool: &mut Pool, args: &StatusArgs) -> Result<Output, ApiError> {
    let ids: Vec<InstanceId> = match &args.instance {
        None => pool.live_ids(),
        Some(text) => {
            let id = InstanceId::parse(text)?;
            if pool.session(id).is_none() && !pool.is_busy(id) {
                return Err(unknown(id, pool));
            }
            vec![id]
        }
    };
    let artifacts_root = pool.artifacts_root().map(str::to_owned);
    let mut rows = Vec::with_capacity(ids.len());
    let mut latest = Receipt::default();
    for id in ids {
        let lease = pool
            .table()
            .get(id)
            .map(|state| (state.lifecycle, state.lease.clone(), state.since));
        let Some((lifecycle, lease, since)) = lease else {
            continue;
        };
        // `null` while no endpoint is open.
        let endpoint = super::endpoint::status_json(pool, id);
        let Some(session) = pool.session_mut(id) else {
            // Checked out by a call in flight: its machine is unreachable until the call returns,
            // and `status` never waits for it.
            if pool.is_busy(id) {
                rows.push(serde_json::json!({
                    "instance": id.to_string(),
                    "state": lifecycle.as_str(),
                    "since_vt_us": since.as_us(),
                    "busy": true,
                    "owner": lease.holder(since).map_or("none", LeaseHolder::as_str),
                    "endpoint": endpoint,
                }));
            }
            continue;
        };
        let receipt = session.receipt();
        let now = session.now();
        let usb_open = session.machine().io().usj_ctrl.client_open();
        let last = last_event(session);
        let build = build(session);
        let fidelity = fidelity(&receipt);
        let cursors: Vec<serde_json::Value> = pemu_core::hostio::SerialStream::ALL
            .iter()
            .map(|stream| {
                serde_json::json!({
                    "stream": stream_name(*stream),
                    "cursor": session.cursor(*stream).0,
                })
            })
            .collect();
        rows.push(serde_json::json!({
            "instance": id.to_string(),
            "label": session.label,
            "state": lifecycle.as_str(),
            "since_vt_us": since.as_us(),
            "vt_us": receipt.vt_us,
            "insns": receipt.insns,
            "fw": session.fw,
            "build": build,
            "seed": session.seed,
            "usb": if usb_open { "open" } else { "closed" },
            "clock": {
                "mode": session.mode.as_str(),
                "speed": session.speed.to_json(),
                "idle_skip": session.idle_skip,
                "owner": lease.holder(now).map_or("none", LeaseHolder::as_str),
                "deterministic_so_far": session.deterministic_so_far,
            },
            "fidelity": fidelity,
            // The data-path faults the run absorbed.
            "fault_counters": receipt.extra.get("fault_counters").cloned().unwrap_or(serde_json::Value::Null),
            // The last run parked in WFI with nothing but an input to wake it.
            "waiting_for_input": session.waiting_for_input,
            "last_event": last,
            "serial_cursors": cursors,
            "endpoint": endpoint,
        }));
        latest = receipt;
    }
    let json = serde_json::json!({
        "instances": rows,
        "artifacts_root": artifacts_root,
    });
    Ok(Output::new(json.clone(), render(&json), latest))
}

/// In the one-line receipt's order; empty rather than absent, since the shape is what an agent
/// binds to.
fn fidelity(receipt: &Receipt) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = receipt
        .fidelity
        .iter()
        .map(|entry| {
            serde_json::json!({
                "subsystem": entry.subsystem,
                "class": entry.class.name(),
            })
        })
        .collect();
    serde_json::Value::Array(entries)
}

/// From the firmware's `esp_app_desc_t`, or `null`. `elf_sha256` is the hash whose prefix the
/// firmware prints at start, so the page header and the console name a build the same way; compile
/// time and date are volatile and left out.
fn build(session: &mut Session) -> serde_json::Value {
    let Some(desc) = session.machine().app_desc() else {
        return serde_json::Value::Null;
    };
    let sha = desc.app_elf_sha256.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    serde_json::json!({
        "project": desc.project_name,
        "version": desc.version,
        "idf_ver": desc.idf_ver,
        "elf_sha256": sha,
    })
}

/// `null` while nothing has been emitted.
fn last_event(session: &mut Session) -> serde_json::Value {
    let events = &session.machine().io().events;
    let head = events.head();
    if head == 0 {
        return serde_json::Value::Null;
    }
    let from = head.saturating_sub(1).max(events.tail());
    match events.slices(from).iter().last() {
        None => serde_json::Value::Null,
        Some(event) => serde_json::json!({
            "seq": head - 1,
            "kind": event_name(event.kind),
            "vt_us": event.vt.as_us(),
            "arg": event.arg,
        }),
    }
}

/// A stopped or never minted id.
fn unknown(id: InstanceId, pool: &Pool) -> ApiError {
    let message = match pool.table().get(id) {
        Some(_) => format!("instance `{id}` is stopped"),
        None => format!("no instance `{id}`"),
    };
    ApiError::new(E_STATE, message).with_hint("`status` lists the live instances")
}

/// Then the artifact root when the caller supplied one.
fn render(json: &serde_json::Value) -> String {
    let mut text = String::new();
    let rows = json["instances"].as_array().map_or(&[][..], Vec::as_slice);
    if rows.is_empty() {
        text.push_str("no instance is running\n");
    }
    for row in rows {
        if row["busy"] == true {
            let why = if row["owner"] == "endpoint" {
                "live on its endpoint"
            } else {
                "a call is in flight"
            };
            let _ = writeln!(
                text,
                "{} {} busy ({why})",
                row["instance"].as_str().unwrap_or("?"),
                row["state"].as_str().unwrap_or("?"),
            );
            render_endpoint(&mut text, &row["endpoint"]);
            continue;
        }
        let _ = writeln!(
            text,
            "{} {} vt={}us fw={} clock={} {} {}",
            row["instance"].as_str().unwrap_or("?"),
            row["state"].as_str().unwrap_or("?"),
            row["vt_us"].as_u64().unwrap_or(0),
            row["fw"].as_str().unwrap_or(""),
            row["clock"]["mode"].as_str().unwrap_or("?"),
            row["clock"]["owner"].as_str().unwrap_or("none"),
            row["label"].as_str().unwrap_or(""),
        );
        if row["waiting_for_input"] == true {
            text.truncate(text.trim_end().len());
            text.push_str(" waiting-for-input\n");
        }
        render_endpoint(&mut text, &row["endpoint"]);
    }
    if let Some(root) = json["artifacts_root"].as_str() {
        let _ = write!(text, "artifacts: {root}");
    }
    text.trim_end().to_owned()
}

/// The flashing URL and the pty path.
fn render_endpoint(text: &mut String, endpoint: &serde_json::Value) {
    if endpoint.is_null() {
        return;
    }
    let _ = write!(
        text,
        "  endpoint clock={}",
        endpoint["clock"].as_str().unwrap_or("?")
    );
    if let Some(url) = endpoint["tcp"]["rfc2217"].as_str() {
        let _ = write!(text, " {url}");
    }
    if let Some(path) = endpoint["pty"]["path"].as_str() {
        let _ = write!(text, " pty={path}");
    }
    text.push('\n');
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`status` arguments.",
        "properties": {
            "instance": instance_schema()
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instances": { "type": "array", "items": { "type": "object" } },
            "artifacts_root": { "type": ["string", "null"] }
        }
    })
}

/// Report every instance's lifecycle, virtual time and clock.
#[command(
    api_crate = crate,
    name = "status",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(read_only, idempotent),
    cli(positional = ["instance"]),
    errors(E_USAGE, E_STATE),
    example(
        title = "List every running instance",
        args = r#"{}"#,
    ),
    example(
        title = "Report one instance",
        args = r#"{"instance":"p1"}"#,
    ),
)]
pub fn status(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = StatusArgs::from_json(&args)?;
    with_pool(|pool| status_on(pool, &args))
}

/// Reused by `clock` and by the CLI banner.
pub fn summary_line(session: &mut Session, lifecycle: &str) -> String {
    let receipt = session.receipt();
    format!(
        "{} {lifecycle} vt={}us fw={}",
        session.id, receipt.vt_us, session.fw
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commands::start::tests::{TestMachine, started};

    #[test]
    fn a_busy_instance_answers_a_short_row_without_waiting_for_its_machine() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.checkout(id).expect("in the pool");
        let args = StatusArgs {
            instance: Some("p1".to_owned()),
        };
        let out = status_on(&mut pool, &args).expect("a busy instance is still live");
        assert_eq!(out.json["instances"][0]["busy"], true);
        assert_eq!(out.json["instances"][0]["state"], "paused");
        assert_eq!(out.text, "p1 paused busy (a call is in flight)");
        pool.checkin(session);
        let out = status_on(&mut pool, &args).expect("back");
        assert!(out.json["instances"][0].get("busy").is_none());
    }

    #[test]
    fn an_empty_pool_says_so_instead_of_failing() {
        let mut pool = Pool::new();
        let out = status_on(&mut pool, &StatusArgs::default()).expect("status always answers");
        assert_eq!(out.json["instances"], serde_json::json!([]));
        assert_eq!(out.text, "no instance is running");
    }

    #[test]
    fn one_instance_reports_its_lifecycle_clock_and_cursors() {
        let (mut pool, _) = started(TestMachine::new());
        let out = status_on(&mut pool, &StatusArgs::default()).expect("status always answers");
        let row = &out.json["instances"][0];
        assert_eq!(row["instance"], "p1");
        assert_eq!(row["state"], "paused");
        assert_eq!(row["fw"], "official");
        assert_eq!(row["usb"], "open");
        assert_eq!(row["clock"]["mode"], "deterministic");
        assert_eq!(row["clock"]["owner"], "none");
        assert_eq!(row["serial_cursors"][0]["stream"], "usj");
        assert_eq!(row["serial_cursors"][0]["cursor"], 0);
        assert_eq!(
            row["fidelity"],
            serde_json::json!([]),
            "the scripted machine's table is empty, but the shape is reported"
        );
        assert_eq!(row["last_event"], serde_json::Value::Null);
        assert!(
            out.text.starts_with("p1 paused vt=0us fw=official"),
            "{}",
            out.text
        );
    }

    #[test]
    fn an_explicit_id_reports_only_that_instance() {
        let (mut pool, _) = started(TestMachine::new());
        let args = StatusArgs {
            instance: Some("p1".to_owned()),
        };
        let out = status_on(&mut pool, &args).expect("p1 is live");
        assert_eq!(out.json["instances"].as_array().expect("rows").len(), 1);
    }

    #[test]
    fn a_stopped_or_unknown_id_is_state_not_an_empty_list() {
        let (mut pool, id) = started(TestMachine::new());
        let args = StatusArgs {
            instance: Some("p9".to_owned()),
        };
        let error = status_on(&mut pool, &args).expect_err("p9 was never minted");
        assert_eq!(error.code, E_STATE);
        assert!(
            error.message.contains("no instance `p9`"),
            "{}",
            error.message
        );

        pool.destroy(id).expect("p1 is live");
        let args = StatusArgs {
            instance: Some("p1".to_owned()),
        };
        let error = status_on(&mut pool, &args).expect_err("p1 is gone");
        assert!(error.message.contains("is stopped"), "{}", error.message);
    }

    #[test]
    fn the_last_event_of_the_ring_is_reported() {
        use pemu_core::hostio::EventKind;

        let (mut pool, id) = started(
            TestMachine::new()
                .event(10, EventKind::Reset)
                .event(20, EventKind::UiSettled),
        );
        pool.session_mut(id)
            .expect("the scripted instance")
            .run_until(pemu_core::time::VTime::from_ms(50));
        let out = status_on(&mut pool, &StatusArgs::default()).expect("status always answers");
        let event = &out.json["instances"][0]["last_event"];
        assert_eq!(event["kind"], "ui_settled");
        assert_eq!(event["vt_us"], 20_000);
        assert_eq!(event["seq"], 1);
    }

    /// It comes from the process that resolved it; no argument can set it.
    #[test]
    fn the_artifact_root_is_reported_from_the_pool_and_is_no_argument_of_this_command() {
        let (mut pool, _) = started(TestMachine::new());
        pool.set_artifacts_root(Some(
            "~/Library/Application Support/passportsim/runs".to_owned(),
        ));
        let out = status_on(&mut pool, &StatusArgs::default()).expect("status always answers");
        assert_eq!(
            out.json["artifacts_root"],
            "~/Library/Application Support/passportsim/runs"
        );
        assert!(out.text.contains("artifacts: ~/Library"), "{}", out.text);

        let error = StatusArgs::from_json(&serde_json::json!({ "artifacts_root": "/anywhere" }))
            .expect_err("the root is an output, not an input");
        assert_eq!(error.code, E_USAGE);
    }

    #[test]
    fn a_malformed_id_is_usage_and_an_unknown_key_too() {
        let args = StatusArgs {
            instance: Some("q1".to_owned()),
        };
        let mut pool = Pool::new();
        assert_eq!(
            status_on(&mut pool, &args)
                .expect_err("`q` is no instance kind")
                .code,
            E_USAGE
        );
        assert_eq!(
            StatusArgs::from_json(&serde_json::json!({ "nonsense": 1 }))
                .expect_err("outside the schema")
                .code,
            E_USAGE
        );
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("status").expect("#[command] registered status");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            StatusArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.read_only && spec.annotations.idempotent);
        assert!(!spec.annotations.needs_instance, "see the module header");
    }
}
