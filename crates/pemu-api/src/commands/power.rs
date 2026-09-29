//! `passportsim power`: the power button and the battery, the first command of the opt-in `power`
//! caps group.
//!
//! The group adds no input of its own. Every `power` call is spelled as the `input` or `env` call
//! that journals the same `InputEvent`s ([`crate::commands::input::input_on`],
//! [`crate::commands::env::env_on`]), so either spelling replays the same way:
//!
//! | `power` | journals as |
//! |---|---|
//! | `press` (`duration`, `then_run`) | `input {button: power, action: click}` |
//! | `hold` | `input {button: power, action: press}` |
//! | `release` | `input {button: power, action: release}` |
//! | `battery` (`mv`, `soc`, `present`, `temp_c`) | `env {battery: {...}}` |
//!
//! A `press` with no `duration` is 600 ms when off and 2200 ms when on, as `input` reads it.
//! Battery fresh and disconnect are not offered: neither has an `InputEvent`. The arguments are
//! flat, so each is a CLI flag, and they are checked here before anything is journaled.

use crate::error::{
    ApiError, E_DEADLOCK, E_GUEST_PANIC, E_HLE, E_INTERNAL, E_LEASE, E_STATE, E_STUCK, E_TRIPWIRE,
    E_USAGE,
};
use crate::output::Output;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

use super::env::{BATTERY_MV_MAX, BATTERY_MV_MIN, BATTERY_TEMP_C_MAX, BATTERY_TEMP_C_MIN};
use crate::args::{duration_schema, enum_of, instance_schema, object, only, opt_str, usage};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PowerOp {
    /// Press and release the power button.
    Press,
    /// Press the power button and leave it down.
    Hold,
    Release,
    /// Change what the fuel gauge measures.
    Battery,
}

impl PowerOp {
    pub const fn as_str(self) -> &'static str {
        match self {
            PowerOp::Press => "press",
            PowerOp::Hold => "hold",
            PowerOp::Release => "release",
            PowerOp::Battery => "battery",
        }
    }

    pub fn parse(text: &str) -> Option<PowerOp> {
        match text {
            "press" => Some(PowerOp::Press),
            "hold" => Some(PowerOp::Hold),
            "release" => Some(PowerOp::Release),
            "battery" => Some(PowerOp::Battery),
            _ => None,
        }
    }
}

/// Only `op: battery` takes them.
const BATTERY_KEYS: [&str; 4] = ["mv", "soc", "present", "temp_c"];

/// Only `op: press` takes them (`then_run` also `hold` and `release`).
const PRESS_KEYS: [&str; 2] = ["duration", "then_run"];

/// A field the op does not take is `E_USAGE`.
pub fn translate(value: &serde_json::Value) -> Result<(PowerOp, serde_json::Value), ApiError> {
    let args = object(value)?;
    let mut known = vec!["instance", "op"];
    known.extend(PRESS_KEYS);
    known.extend(BATTERY_KEYS);
    only(args, &known)?;
    let op = enum_of(args, "op", PowerOp::parse, "press, hold, release, battery")?
        .unwrap_or(PowerOp::Press);
    let instance = opt_str(args, "instance")?;
    let named = |keys: &[&'static str]| keys.iter().copied().find(|k| args.contains_key(*k));

    let mut out = serde_json::Map::new();
    if let Some(instance) = instance {
        out.insert("instance".into(), instance.into());
    }
    match op {
        PowerOp::Battery => {
            if let Some(key) = named(&PRESS_KEYS) {
                return Err(usage(key, "belongs to `op: press`, not `op: battery`"));
            }
            let battery: serde_json::Map<_, _> = BATTERY_KEYS
                .iter()
                .filter_map(|k| args.get(*k).map(|v| ((*k).to_owned(), v.clone())))
                .collect();
            if battery.is_empty() {
                return Err(usage(
                    "op",
                    "`battery` needs at least one of mv, soc, present, temp_c",
                ));
            }
            out.insert("battery".into(), serde_json::Value::Object(battery));
        }
        PowerOp::Press | PowerOp::Hold | PowerOp::Release => {
            if let Some(key) = named(&BATTERY_KEYS) {
                return Err(usage(key, "belongs to `op: battery`"));
            }
            if op != PowerOp::Press && args.contains_key("duration") {
                return Err(usage(
                    "duration",
                    "a raw edge has no length; `op: press` takes one",
                ));
            }
            out.insert("button".into(), "power".into());
            let action = match op {
                PowerOp::Press => "click",
                PowerOp::Hold => "press",
                _ => "release",
            };
            out.insert("action".into(), action.into());
            for key in PRESS_KEYS {
                if let Some(v) = args.get(key) {
                    out.insert(key.to_owned(), v.clone());
                }
            }
        }
    }
    Ok((op, serde_json::Value::Object(out)))
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`power` arguments.",
        "properties": {
            "instance": instance_schema(),
            "op": { "type": "string", "enum": ["press", "hold", "release", "battery"], "description": "What to do (press)." },
            "duration": duration_schema("Press length; 600 ms when off, 2200 ms when on."),
            "then_run": duration_schema("Run after the edge."),
            "mv": { "type": "integer", "minimum": BATTERY_MV_MIN, "maximum": BATTERY_MV_MAX, "description": "Cell voltage." },
            "soc": { "type": "integer", "minimum": 0, "maximum": 100, "description": "Charge percent." },
            // An omitted `present` leaves the battery as it is and reaches `env` only when named.
            // The CLI offers `--no-present` for it; a `default: true` would tell every schema
            // consumer something false.
            "present": { "type": "boolean", "x-tri-state": true, "description": "Battery connected; omitted leaves it alone, `--no-present` disconnects it." },
            "temp_c": { "type": "integer", "minimum": BATTERY_TEMP_C_MIN, "maximum": BATTERY_TEMP_C_MAX, "description": "Cell temperature." }
        }
    })
}

/// `input`'s for an edge, `env`'s for a state.
pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "vt_us": { "type": "integer" },
            "elapsed_vt_us": { "type": "integer" },
            "input": { "type": "object" },
            "serial": { "type": "object" },
            "stream": { "type": "string" },
            "applied": { "type": "object" },
            "effects": { "type": "array", "items": { "type": "string" } },
            "notes": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// Press or hold the power button, or set the battery.
#[command(
    api_crate = crate,
    name = "power",
    group = power,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(advances_time, needs_instance),
    cli(positional = ["op"]),
    errors(E_USAGE, E_STATE, E_LEASE, E_GUEST_PANIC, E_DEADLOCK, E_STUCK, E_TRIPWIRE, E_HLE, E_INTERNAL),
    example(
        title = "Press the power button for the length the rail needs",
        args = r#"{"op":"press"}"#,
    ),
    example(
        title = "Hold the power button down",
        args = r#"{"op":"hold"}"#,
    ),
    example(
        title = "Run the cell down to 3500 mV at 15 percent",
        args = r#"{"op":"battery","mv":3500,"soc":15}"#,
    ),
)]
pub fn power(cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    match translate(&args)? {
        (PowerOp::Battery, env) => super::env::env(cx, env),
        (_, input) => super::input::input(cx, input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    use crate::commands::env::EnvArgs;
    use crate::commands::input::{Action, Button, InputArgs};

    #[test]
    fn a_press_is_the_power_click_of_input() {
        let (op, input) = translate(&json!({"op":"press","duration":"600ms"})).expect("a press");
        assert_eq!(op, PowerOp::Press);
        let args = InputArgs::from_json(&input).expect("input takes it");
        assert_eq!((args.button, args.action), (Button::Power, Action::Click));
        assert_eq!(
            args.duration,
            Some(pemu_core::time::VTime::from_ms(600)),
            "the length is passed through"
        );
        // No `op` is a press, whose length `input` reads from the rail.
        let (op, input) = translate(&json!({})).expect("the default");
        assert_eq!(op, PowerOp::Press);
        assert_eq!(InputArgs::from_json(&input).expect("input").duration, None);
    }

    #[test]
    fn hold_and_release_are_the_raw_edges_of_input() {
        for (op, action) in [("hold", Action::Press), ("release", Action::Release)] {
            let (_, input) = translate(&json!({"op": op, "instance": "p2"})).expect("an edge");
            let args = InputArgs::from_json(&input).expect("input takes it");
            assert_eq!((args.button, args.action), (Button::Power, action));
            assert_eq!(args.instance.as_deref(), Some("p2"));
        }
    }

    #[test]
    fn battery_is_the_battery_of_env() {
        let (op, env) =
            translate(&json!({"op":"battery","mv":3500,"soc":15,"temp_c":-5})).expect("a set");
        assert_eq!(op, PowerOp::Battery);
        let args = EnvArgs::from_json(&env).expect("env takes it");
        let battery = args.battery.expect("the battery part");
        assert_eq!(
            (battery.mv, battery.soc, battery.temp_c_deci),
            (Some(3500), Some(15), Some(-50))
        );
        assert!(args.usb.is_none() && args.mic.is_none() && args.nfc_card.is_none());
    }

    #[test]
    fn a_field_of_another_op_is_refused_before_anything_is_journaled() {
        for bad in [
            json!({"op":"press","mv":3500}),
            json!({"op":"battery","duration":"1s"}),
            json!({"op":"hold","duration":"1s"}),
            json!({"op":"battery"}),
            json!({"op":"toggle"}),
            json!({"button":"power"}),
        ] {
            let error = translate(&bad).expect_err("refused");
            assert_eq!(error.code, E_USAGE, "{bad}");
        }
    }

    #[test]
    fn the_command_is_in_the_power_group_and_advertises_its_ops() {
        let spec = crate::registry::find("power").expect("#[command] registered power");
        assert_eq!(spec.group, crate::spec::CapsGroup::Power);
        assert_eq!(spec.cli.positional, &["op"]);
        // Without the tri-state marker the CLI has no `--no-present`. It must not become a
        // `default`: a consumer of the MCP `inputSchema` that applies defaults would journal a
        // battery state nobody asked for.
        let schema = input_schema().to_value();
        assert_eq!(
            schema["properties"]["present"]["x-tri-state"],
            serde_json::json!(true)
        );
        assert_eq!(
            schema["properties"]["present"]["default"],
            serde_json::Value::Null,
            "an omitted `present` means `do not touch`, not `true`"
        );
        for example in spec.examples {
            let args: serde_json::Value =
                serde_json::from_str(example.args).expect("the example is JSON");
            translate(&args).expect("every example translates");
        }
    }
}
