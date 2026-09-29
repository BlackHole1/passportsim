//! `passportsim usb`: the USB cable and the host client of the USB Serial/JTAG port, the second
//! command of the opt-in `power` caps group.
//!
//! Like `power`, every call is spelled as the `env` or `input` call that journals the same
//! `UsbCable` and `UsbClient` inputs, so either spelling replays the same way:
//!
//! | `usb` | journals as |
//! |---|---|
//! | `state` (`unplugged`, `charger`, `host`, `open`) | `env {usb: state}`: the cable, then the client, at one instant |
//! | `cable: in` / `cable: out` | `input {button: usb, action: plug / unplug}` |
//! | `client: open` / `client: closed` | `input {button: usb, action: open / close}` |
//!
//! `state` names a whole host state and reports what the firmware can see of it; `cable` and
//! `client` move one fact each, followed by `then_run` and no settle run, so a caller orders two
//! edges itself. U2 ATTACHED_IDLE is reachable both ways. One call names exactly one of the three,
//! so it never journals a half-applied pair. `cable` and `client` are words because a CLI switch
//! can only set `true`.

use crate::error::{
    ApiError, E_DEADLOCK, E_GUEST_PANIC, E_HLE, E_INTERNAL, E_LEASE, E_STATE, E_STUCK, E_TRIPWIRE,
    E_USAGE,
};
use crate::output::Output;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

use super::env::UsbWorld;
use crate::args::{duration_schema, enum_of, instance_schema, object, only, opt_str, usage};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UsbVia {
    /// Through `env`.
    Env,
    /// Through `input`.
    Input,
}

/// Anything else is `E_USAGE`.
pub fn translate(value: &serde_json::Value) -> Result<(UsbVia, serde_json::Value), ApiError> {
    let args = object(value)?;
    only(args, &["instance", "state", "cable", "client", "then_run"])?;
    let state = match opt_str(args, "state")? {
        None => None,
        Some(text) => Some(UsbWorld::parse(text).ok_or_else(|| {
            usage(
                "state",
                &format!("`{text}` is not one of unplugged, charger, host, open"),
            )
        })?),
    };
    let in_out = |t: &str| match t {
        "in" => Some(true),
        "out" => Some(false),
        _ => None,
    };
    let open_closed = |t: &str| match t {
        "open" => Some(true),
        "closed" => Some(false),
        _ => None,
    };
    let cable = enum_of(args, "cable", in_out, "in, out")?;
    let client = enum_of(args, "client", open_closed, "open, closed")?;

    let mut out = serde_json::Map::new();
    if let Some(instance) = opt_str(args, "instance")? {
        out.insert("instance".into(), instance.into());
    }
    let via = match (state, cable, client) {
        (Some(state), None, None) => {
            if args.contains_key("then_run") {
                return Err(usage(
                    "then_run",
                    "`state` journals without running; `run` lets the guest see it",
                ));
            }
            out.insert("usb".into(), state.as_str().into());
            UsbVia::Env
        }
        (None, Some(plugged), None) => {
            out.insert("button".into(), "usb".into());
            out.insert(
                "action".into(),
                if plugged { "plug" } else { "unplug" }.into(),
            );
            UsbVia::Input
        }
        (None, None, Some(open)) => {
            out.insert("button".into(), "usb".into());
            out.insert("action".into(), if open { "open" } else { "close" }.into());
            UsbVia::Input
        }
        _ => {
            return Err(
                usage("state", "name exactly one of `state`, `cable` or `client`")
                    .with_hint("`usb unplugged`, `usb --cable out` or `usb --client open`"),
            );
        }
    };
    if via == UsbVia::Input
        && let Some(run) = args.get("then_run")
    {
        out.insert("then_run".into(), run.clone());
    }
    Ok((via, serde_json::Value::Object(out)))
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`usb` arguments; exactly one of state, cable, client.",
        "properties": {
            "instance": instance_schema(),
            "state": { "type": "string", "enum": ["unplugged", "charger", "host", "open"], "description": "Whole USB host state." },
            "cable": { "type": "string", "enum": ["in", "out"], "description": "Plug or unplug the cable." },
            "client": { "type": "string", "enum": ["open", "closed"], "description": "Open or close the host console." },
            "then_run": duration_schema("Run after a cable or client edge.")
        }
    })
}

/// `env`'s for a state, `input`'s for an edge, as `power` reports.
pub use super::power::output_schema;

/// Plug or unplug the USB cable, or open or close the host console.
#[command(
    api_crate = crate,
    name = "usb",
    group = power,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(advances_time, needs_instance),
    cli(positional = ["state"]),
    errors(E_USAGE, E_STATE, E_LEASE, E_GUEST_PANIC, E_DEADLOCK, E_STUCK, E_TRIPWIRE, E_HLE, E_INTERNAL),
    example(
        title = "Unplug the cable (U0)",
        args = r#"{"state":"unplugged"}"#,
    ),
    example(
        title = "Plug back in with a host console open (U3)",
        args = r#"{"state":"open"}"#,
    ),
    example(
        title = "Close the host console and leave the cable in (U2)",
        args = r#"{"client":"closed"}"#,
    ),
)]
pub fn usb(cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    match translate(&args)? {
        (UsbVia::Env, env) => super::env::env(cx, env),
        (UsbVia::Input, input) => super::input::input(cx, input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_core::input::InputEvent;
    use serde_json::json;

    use crate::commands::env::EnvArgs;
    use crate::commands::input::{Action, Button, InputArgs};

    #[test]
    fn a_state_is_the_usb_world_of_env() {
        for (name, world) in [
            ("unplugged", UsbWorld::Unplugged),
            ("charger", UsbWorld::Charger),
            ("host", UsbWorld::Host),
            ("open", UsbWorld::Open),
        ] {
            let (via, env) = translate(&json!({ "state": name })).expect("a state");
            assert_eq!(via, UsbVia::Env);
            let args = EnvArgs::from_json(&env).expect("env takes it");
            assert_eq!(args.usb, Some(world));
            assert!(args.battery.is_none() && args.mic.is_none());
        }
        // U3 -> U0 -> U3 journals the cable and client facts.
        assert_eq!(
            UsbWorld::Unplugged.events(),
            [
                InputEvent::UsbCable { plugged: false },
                InputEvent::UsbClient { open: false }
            ]
        );
    }

    #[test]
    fn cable_and_client_are_the_usb_edges_of_input() {
        for (args, action) in [
            (json!({"cable": "in"}), Action::Plug),
            (json!({"cable": "out"}), Action::Unplug),
            (json!({"client": "open"}), Action::Open),
            (
                json!({"client": "closed", "then_run": "0ms"}),
                Action::Close,
            ),
        ] {
            let (via, input) = translate(&args).expect("an edge");
            assert_eq!(via, UsbVia::Input);
            let parsed = InputArgs::from_json(&input).expect("input takes it");
            assert_eq!((parsed.button, parsed.action), (Button::Usb, action));
        }
    }

    #[test]
    fn exactly_one_of_state_cable_and_client() {
        for bad in [
            json!({}),
            json!({"state":"open","cable":"in"}),
            json!({"cable":"in","client":"open"}),
            json!({"state":"docked"}),
            json!({"state":"open","then_run":"1s"}),
            json!({"cable":true}),
            json!({"vbus":true}),
        ] {
            let error = translate(&bad).expect_err("refused");
            assert_eq!(error.code, E_USAGE, "{bad}");
        }
    }

    #[test]
    fn the_command_is_in_the_power_group_and_its_examples_translate() {
        let spec = crate::registry::find("usb").expect("#[command] registered usb");
        assert_eq!(spec.group, crate::spec::CapsGroup::Power);
        assert_eq!(spec.cli.positional, &["state"]);
        for example in spec.examples {
            let args: serde_json::Value =
                serde_json::from_str(example.args).expect("the example is JSON");
            translate(&args).expect("every example translates");
        }
    }
}
