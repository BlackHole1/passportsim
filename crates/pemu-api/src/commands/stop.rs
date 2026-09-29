//! `passportsim stop`: destroy an instance.
//!
//! The id stays in the table after the session is dropped, so a later call says "instance `p1` is
//! stopped" rather than "no such instance", and the index is never reused. Writing artifacts is
//! file system work for `pemu_host::artifacts`; this command reports the final virtual time it
//! stamps.

use std::fmt::Write as _;

use crate::error::{ApiError, E_STATE, E_USAGE};
use crate::output::Output;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

use crate::args::{instance_schema, object, only, opt_bool, opt_str};
use crate::pool::{Pool, with_pool};
use crate::session::Session;

/// Arguments of `stop`. `keep_artifacts` defaults to `true`, so the CLI offers
/// `--no-keep-artifacts`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StopArgs {
    /// Instance to destroy, or `None` while exactly one is live.
    pub instance: Option<String>,
    pub keep_artifacts: bool,
}

impl StopArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<StopArgs, ApiError> {
        let args = object(value)?;
        only(args, &["instance", "keep_artifacts"])?;
        Ok(StopArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            keep_artifacts: opt_bool(args, "keep_artifacts")?.unwrap_or(true),
        })
    }
}

/// Destroys the instance `id` names.
pub fn stop_on(
    pool: &mut Pool,
    id: crate::instance::InstanceId,
    args: &StopArgs,
) -> Result<Output, ApiError> {
    let mut session: Session = pool.destroy(id)?;
    let receipt = session.receipt();
    let json = serde_json::json!({
        "instance": id.to_string(),
        "state": "stopped",
        "final_vt_us": receipt.vt_us,
        "insns": receipt.insns,
        "keep_artifacts": args.keep_artifacts,
    });
    let mut text = String::new();
    let _ = write!(
        text,
        "{id} stopped vt={}us insns={}",
        receipt.vt_us, receipt.insns
    );
    if !args.keep_artifacts {
        text.push_str(" (artifacts dropped)");
    }
    Ok(Output::new(json, text, receipt))
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`stop` arguments.",
        "properties": {
            "instance": instance_schema(),
            "keep_artifacts": { "type": "boolean", "default": true, "description": "Keep the artifacts (true)." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "state": { "type": "string" },
            "final_vt_us": { "type": "integer" },
            "insns": { "type": "integer" },
            "keep_artifacts": { "type": "boolean" }
        }
    })
}

/// Destroy an instance and flush its artifacts.
#[command(
    api_crate = crate,
    name = "stop",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    cli(positional = ["instance"]),
    errors(E_USAGE, E_STATE),
    example(
        title = "Stop the only running instance",
        args = r#"{}"#,
    ),
    example(
        title = "Stop one instance of several and drop its artifacts",
        args = r#"{"instance":"p1","keep_artifacts":false}"#,
    ),
)]
pub fn stop(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = StopArgs::from_json(&args)?;
    with_pool(|pool| {
        let id = pool.bind(SPEC_STOP.annotations, args.instance.as_deref())?;
        stop_on(pool, id, &args)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commands::start::tests::{TestMachine, started};
    use crate::instance::Lifecycle;

    #[test]
    fn stopping_ends_the_lifecycle_and_drops_the_session() {
        let (mut pool, id) = started(TestMachine::new());
        let out = stop_on(&mut pool, id, &StopArgs::default()).expect("p1 is live");
        assert_eq!(out.json["instance"], "p1");
        assert_eq!(out.json["state"], "stopped");
        assert_eq!(out.json["final_vt_us"], 0);
        assert!(pool.session(id).is_none());
        assert_eq!(
            pool.table().get(id).expect("the id is kept").lifecycle,
            Lifecycle::Stopped
        );
        assert!(pool.live_ids().is_empty());
    }

    #[test]
    fn a_second_stop_is_state_rather_than_a_silent_success() {
        let (mut pool, id) = started(TestMachine::new());
        stop_on(&mut pool, id, &StopArgs::default()).expect("p1 is live");
        let error = stop_on(&mut pool, id, &StopArgs::default()).expect_err("p1 is gone");
        assert_eq!(error.code, E_STATE);
    }

    #[test]
    fn dropping_the_artifacts_is_reported_in_the_text() {
        let (mut pool, id) = started(TestMachine::new());
        let args = StopArgs {
            instance: None,
            keep_artifacts: false,
        };
        let out = stop_on(&mut pool, id, &args).expect("p1 is live");
        assert_eq!(out.json["keep_artifacts"], false);
        assert!(out.text.contains("artifacts dropped"), "{}", out.text);
    }

    #[test]
    fn malformed_arguments_are_usage() {
        for case in [
            serde_json::json!([]),
            serde_json::json!({ "keep_artifacts": "yes" }),
            serde_json::json!({ "nonsense": 1 }),
        ] {
            let error = StopArgs::from_json(&case).expect_err("outside the schema");
            assert_eq!(error.code, E_USAGE, "{case}");
        }
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("stop").expect("#[command] registered stop");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            StopArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.needs_instance);
        assert_eq!(spec.cli.positional, ["instance"]);
    }
}
