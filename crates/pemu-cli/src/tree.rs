//! The clap tree, generated from the command registry: one subcommand per command,
//! `bin_name("passportsim")` so a usage line never reads `passportsim.exe`, and one flag per
//! top-level input-schema property.
//!
//! `xtask docs` writes each synopsis from the same schema with [`synopsis`], and every subcommand
//! overrides its usage line with it, so clap cannot drift from the documents.
//!
//! | Schema property | argv |
//! |---|---|
//! | listed in `CommandSpec::cli.positional` | a bare argument, in the declared order |
//! | boolean | `--flag`, taking no value |
//! | boolean whose schema `default` is `true`, or which carries `x-tri-state` | also `--no-flag`, the only way to reach `false` |
//! | any other scalar | `--flag <VALUE>` |
//! | object or array | no flag: it travels in the `--json` document |
//!
//! A nested value has no portable argv spelling (`cmd.exe` has no single quotes and Windows
//! PowerShell 5.1 strips embedded double quotes), so it travels in `--json`.

use clap::{Arg, ArgAction, Command};
use pemu_api::registry;
use pemu_api::spec::CommandSpec;

use crate::json::{Map, Value};

/// So a help snapshot does not depend on the window the test ran in.
pub const TERM_WIDTH: usize = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    Positional,
    Switch,
    Value,
    /// Carried in the `--json` document.
    Json,
}

/// Resolving `$ref` into the document's `$defs`.
fn form_of(schema: &Value, defs: Option<&Map<String, Value>>) -> Form {
    form_at(schema, defs, 0)
}

/// The hop budget makes a recursive `$defs` chain terminate.
fn form_at(schema: &Value, defs: Option<&Map<String, Value>>, hops: usize) -> Form {
    if let Some(Value::String(reference)) = schema.get("$ref") {
        if hops >= 8 {
            return Form::Json;
        }
        let target = reference
            .strip_prefix("#/$defs/")
            .and_then(|name| defs.and_then(|defs| defs.get(name)));
        return match target {
            Some(target) => form_at(target, defs, hops + 1),
            None => Form::Json,
        };
    }
    if let Some(Value::Array(values)) = schema.get("enum") {
        return if values
            .iter()
            .all(|value| !value.is_object() && !value.is_array())
        {
            Form::Value
        } else {
            Form::Json
        };
    }
    if let Some(value) = schema.get("const") {
        return if value.is_object() || value.is_array() {
            Form::Json
        } else {
            Form::Value
        };
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(branches)) = schema.get(key) {
            let forms: Vec<Form> = branches
                .iter()
                .map(|branch| form_at(branch, defs, hops + 1))
                .collect();
            if forms.contains(&Form::Json) {
                return Form::Json;
            }
            return if forms.iter().all(|form| *form == Form::Switch) {
                Form::Switch
            } else {
                Form::Value
            };
        }
    }
    let names: Vec<&str> = match schema.get("type") {
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => return Form::Json,
    };
    let names: Vec<&str> = names.into_iter().filter(|name| *name != "null").collect();
    if names
        .iter()
        .any(|name| *name == "object" || *name == "array")
    {
        return Form::Json;
    }
    if !names.is_empty() && names.iter().all(|name| *name == "boolean") {
        return Form::Switch;
    }
    if names.is_empty() {
        Form::Json
    } else {
        Form::Value
    }
}

#[derive(Clone, Debug)]
pub struct Property {
    /// With underscores.
    pub name: String,
    pub form: Form,
    pub required: bool,
    /// For the value conversion of [`crate::args`].
    pub kind: Option<String>,
    /// See [`Property::negated_flag`].
    pub negated: bool,
    /// Becomes the flag's help.
    pub help: String,
}

impl Property {
    /// Underscores become dashes.
    pub fn flag(&self) -> String {
        self.name.replace('_', "-")
    }

    pub fn negated_name(&self) -> String {
        format!("no_{}", self.name)
    }

    pub fn negated_flag(&self) -> String {
        format!("no-{}", self.flag())
    }
}

/// A nullable scalar is `["string", "null"]`; reading only the plain form would leave it untyped,
/// and [`crate::args::assemble`] would turn the text `7` into the number 7 for a string property.
fn kind_of(schema: &Value) -> Option<String> {
    match schema.get("type") {
        Some(Value::String(one)) => Some(one.clone()),
        Some(Value::Array(many)) => {
            let names: Vec<&str> = many
                .iter()
                .filter_map(Value::as_str)
                .filter(|name| *name != "null")
                .collect();
            match names.as_slice() {
                [one] => Some((*one).to_owned()),
                // A genuine union (`Duration` is "integer milliseconds or `1.5s`"): read as the
                // narrowest scalar its text parses as.
                _ => None,
            }
        }
        _ => None,
    }
}

/// A switch can only set a flag, so `false` is unreachable from argv for two kinds of property:
///
/// 1. a boolean whose `default` is `true`, where the flag alone would be a no-op;
/// 2. a boolean marked `x-tri-state`, where omitting it means "leave it alone" (`power`'s
///    `present`). It must not get a `default` just to earn the spelling: the same schema is the MCP
///    `inputSchema`, and a consumer that applies defaults would send a value nobody asked for.
fn negated_of(form: Form, schema: &Value) -> bool {
    form == Form::Switch
        && (schema.get("default") == Some(&Value::Bool(true))
            || schema.get("x-tri-state") == Some(&Value::Bool(true)))
}

/// In schema order (which `serde_json` keeps sorted).
pub fn properties(spec: &CommandSpec) -> Vec<Property> {
    let schema = (spec.input_schema)().to_value();
    let required: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let defs = schema.get("$defs").and_then(Value::as_object);
    let Some(map) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter()
        .map(|(name, property)| {
            let form = if spec.cli.positional.contains(&name.as_str()) {
                Form::Positional
            } else {
                form_of(property, defs)
            };
            Property {
                name: name.clone(),
                form,
                required: required.iter().any(|one| one == name),
                kind: kind_of(property),
                negated: negated_of(form, property),
                help: property
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            }
        })
        .collect()
}

/// Byte for byte the one `xtask docs` writes into `docs/commands/<name>.md`.
pub fn synopsis(spec: &CommandSpec) -> String {
    let props = properties(spec);
    let required_of = |wanted: &str| {
        props
            .iter()
            .find(|p| p.name == wanted)
            .is_some_and(|p| p.required)
    };
    let mut line = format!("passportsim {}", spec.name);
    for name in spec.cli.positional {
        if required_of(name) {
            line.push_str(&format!(" <{name}>"));
        } else {
            line.push_str(&format!(" [{name}]"));
        }
    }
    let mut json_required = false;
    for property in &props {
        let flag = match property.form {
            Form::Positional => continue,
            Form::Json => {
                json_required |= property.required;
                continue;
            }
            Form::Switch => format!("--{}", property.flag()),
            Form::Value => format!("--{} <{}>", property.flag(), property.name.to_uppercase()),
        };
        if property.required {
            line.push_str(&format!(" {flag}"));
        } else {
            line.push_str(&format!(" [{flag}]"));
        }
    }
    if json_required {
        line.push_str(" --json <@file|->");
    } else {
        line.push_str(" [--json <@file|->]");
    }
    line
}

pub const JSON_ARG: &str = "json";

/// `--json` is the input document, so the rendering is chosen by `--output`.
pub const OUTPUT_ARG: &str = "output";

/// The CI default, which turns an unmodeled-hardware caveat into exit 7.
pub const STRICT_ARG: &str = "strict";

/// The one thing that moves the artifact root.
pub const ARTIFACTS_ARG: &str = "artifacts";

/// Run in this process instead of forwarding to the daemon.
pub const EPHEMERAL_ARG: &str = "ephemeral";

/// Not a registry command: it hosts the registry.
pub const SERVE: &str = "serve";

/// Not a registry command either.
pub const MCP: &str = "mcp";

/// No console output; the log goes to the logs role.
pub const HEADLESS_ARG: &str = "headless";

/// The authenticated `POST /v1/shutdown`.
pub const STOP_ARG: &str = "stop";

/// The preferred port of the bind policy.
pub const PORT_ARG: &str = "port";

/// Of `serve` and `mcp`.
pub const CAPS_ARG: &str = "caps";

pub const MAX_INSTANCES_ARG: &str = "max-instances";

/// Without it no planner is installed and every Device command refuses, so nothing this binary runs
/// can reach an attached Passport.
pub const ALLOW_DEVICE_ARG: &str = "allow-device";

/// The esptool the Device group runs.
pub const ESPTOOL_ARG: &str = "esptool";

/// On the root and on every subcommand, so each may be written before or after the command name.
/// Not part of [`synopsis`]: they belong to no command.
fn globals(mut command: Command, taken: &[String]) -> Command {
    // A schema property of the same name owns the spelling; two arguments with one id is a clap
    // panic.
    if !taken.iter().any(|name| name == OUTPUT_ARG) {
        command = command.arg(
            Arg::new(OUTPUT_ARG)
                .long("output")
                .value_name("MODE")
                .value_parser(["text", "json"])
                .help("Render the result as text (default) or as one JSON object."),
        );
    }
    if !taken.iter().any(|name| name == STRICT_ARG) {
        command = command.arg(
            Arg::new(STRICT_ARG)
                .long("strict")
                .action(ArgAction::SetTrue)
                .help("Fail an unmodeled-hardware caveat with exit 7."),
        );
    }
    if !taken.iter().any(|name| name == EPHEMERAL_ARG) {
        command = command.arg(
            Arg::new(EPHEMERAL_ARG)
                .long("ephemeral")
                .action(ArgAction::SetTrue)
                .help("Run in this process instead of the shared daemon."),
        );
    }
    if !taken.iter().any(|name| name == ALLOW_DEVICE_ARG) {
        command = command.arg(
            Arg::new(ALLOW_DEVICE_ARG)
                .long("allow-device")
                .action(ArgAction::SetTrue)
                .help("Turn on the device group, which can flash the attached Passport."),
        );
    }
    // The esptool resolution starts with what a person named, so "pass `--esptool`" is actionable.
    if !taken.iter().any(|name| name == ESPTOOL_ARG) {
        command = command.arg(
            Arg::new(ESPTOOL_ARG)
                .long("esptool")
                .value_name("PATH")
                .help("The esptool to run for the device group, instead of the discovered one."),
        );
    }
    if !taken.iter().any(|name| name == ARTIFACTS_ARG) {
        command = command.arg(
            Arg::new(ARTIFACTS_ARG)
                .long("artifacts")
                .value_name("DIR")
                .help("Artifact root, instead of the host default."),
        );
    }
    command
}

pub fn build() -> Command {
    let mut root = globals(
        Command::new("passportsim")
            .bin_name("passportsim")
            // Carries the payload line of `payload::Payload::describe`.
            .version(env!("CARGO_PKG_VERSION"))
            .term_width(TERM_WIDTH)
            .disable_help_subcommand(true)
            .subcommand_required(true)
            .arg_required_else_help(true)
            .about("Emulator for the FoloToy AI Passport (ESP32-C3)."),
        &[],
    );
    let mut names: Vec<&CommandSpec> = registry::commands().iter().collect();
    names.sort_by_key(|spec| spec.name);
    for spec in names {
        root = root.subcommand(subcommand(spec));
    }
    root.subcommand(serve()).subcommand(mcp())
}

/// Not generated from the registry: it serves the registry, so it has no input schema, no MCP tool
/// and no page under `docs/commands/`.
pub fn serve() -> Command {
    Command::new(SERVE)
        .about("Run the daemon: HTTP, WebSocket and MCP over loopback.")
        .term_width(TERM_WIDTH)
        .arg(
            Arg::new(HEADLESS_ARG)
                .long("headless")
                .action(ArgAction::SetTrue)
                .help("Print nothing; log to the logs directory. What an auto-spawn runs."),
        )
        .arg(
            Arg::new(STOP_ARG)
                .long("stop")
                .action(ArgAction::SetTrue)
                .conflicts_with_all([HEADLESS_ARG, PORT_ARG, CAPS_ARG, MAX_INSTANCES_ARG])
                .help("Stop the running daemon over its authenticated shutdown route."),
        )
        .arg(
            Arg::new(PORT_ARG)
                .long("port")
                .value_name("PORT")
                .value_parser(clap::value_parser!(u16))
                .help("Preferred loopback port (8765); a port the system picks if it is taken."),
        )
        .arg(
            Arg::new(CAPS_ARG)
                .long("caps")
                .value_name("LIST")
                .help("Comma-separated caps groups besides core: audio, radio, nfc, debug, power."),
        )
        .arg(
            Arg::new(MAX_INSTANCES_ARG)
                .long("max-instances")
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help("Most instances at once (twice the host parallelism)."),
        )
        .arg(
            Arg::new(ARTIFACTS_ARG)
                .long("artifacts")
                .value_name("DIR")
                .help("Artifact root, instead of the host default."),
        )
        .arg(
            Arg::new(ALLOW_DEVICE_ARG)
                .long("allow-device")
                .action(ArgAction::SetTrue)
                .help("Turn on the device group, which can flash the attached Passport."),
        )
        .arg(
            Arg::new(ESPTOOL_ARG)
                .long("esptool")
                .value_name("PATH")
                .help("The esptool to run for the device group, instead of the discovered one."),
        )
}

/// The stdio adapter to the daemon.
pub fn mcp() -> Command {
    Command::new(MCP)
        .about("Serve MCP on standard input and output, through the daemon.")
        .term_width(TERM_WIDTH)
        .arg(
            Arg::new(CAPS_ARG)
                .long("caps")
                .value_name("LIST")
                .help("Comma-separated caps groups besides core: audio, radio, nfc, debug, power."),
        )
        .arg(
            Arg::new(ALLOW_DEVICE_ARG)
                .long("allow-device")
                .action(ArgAction::SetTrue)
                .help("Turn on the device group, which can flash the attached Passport."),
        )
}

/// With the usage line the documents carry.
pub fn subcommand(spec: &CommandSpec) -> Command {
    let props = properties(spec);
    let taken: Vec<String> = props.iter().map(|p| p.name.clone()).collect();
    // The command owns its long help, so the list lives once, next to the flow it describes.
    let long_help = pemu_api::commands::flash_device::long_help(spec.name);
    let mut command = globals(
        Command::new(spec.name)
            .about(spec.summary)
            .after_long_help(long_help.clone().unwrap_or_default())
            .after_help(long_help.unwrap_or_default())
            .term_width(TERM_WIDTH)
            .override_usage(synopsis(spec))
            .arg(
                Arg::new(JSON_ARG)
                    .long("json")
                    .value_name("@file|-")
                    .help("Read the arguments from a JSON document: `@<path>` or `-` for stdin."),
            ),
        &taken,
    );
    for alias in spec.cli.aliases {
        command = command.alias(*alias);
    }
    // Positionals first, in the declared order.
    for (index, name) in spec.cli.positional.iter().enumerate() {
        let property = props.iter().find(|p| p.name == *name);
        let help = property.map_or(String::new(), |p| p.help.clone());
        let required = property.is_some_and(|p| p.required);
        command = command.arg(
            Arg::new((*name).to_owned())
                .index(index + 1)
                .required(false)
                .help(if required {
                    format!("{help} (required)")
                } else {
                    help
                }),
        );
    }
    for property in &props {
        match property.form {
            Form::Positional | Form::Json => continue,
            Form::Switch => {
                command = command.arg(
                    Arg::new(property.name.clone())
                        .long(property.flag())
                        .action(ArgAction::SetTrue)
                        .help(property.help.clone()),
                );
                if property.negated {
                    command = command.arg(
                        Arg::new(property.negated_name())
                            .long(property.negated_flag())
                            .action(ArgAction::SetTrue)
                            .conflicts_with(property.name.clone())
                            .help(format!("The opposite of `--{}`.", property.flag())),
                    );
                }
            }
            Form::Value => {
                command = command.arg(
                    Arg::new(property.name.clone())
                        .long(property.flag())
                        .value_name(property.name.to_uppercase())
                        .help(property.help.clone()),
                );
            }
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read from the fenced block under its `## Synopsis` heading. `xtask docs --check` keeps the
    /// documents current, so they are the snapshot, not a second copy under `tests/`.
    fn documented_synopsis(name: &str) -> String {
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/commands")
            .join(format!("{name}.md"));
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{}: {e}", file.display()))
            .replace("\r\n", "\n");
        let after = text
            .split_once("## Synopsis")
            .unwrap_or_else(|| panic!("{name}.md has a synopsis section"))
            .1;
        let block = after
            .split_once("```text\n")
            .expect("a fenced synopsis")
            .1
            .split_once("```")
            .expect("a closed fence")
            .0;
        block.trim_end().to_owned()
    }

    #[test]
    fn every_command_renders_the_usage_line_the_documents_carry() {
        for spec in registry::commands() {
            let help = subcommand(spec)
                .render_usage()
                .to_string()
                .replace("\r\n", "\n");
            let line = help
                .strip_prefix("Usage: ")
                .unwrap_or(&help)
                .trim_end()
                .to_owned();
            assert_eq!(
                line,
                documented_synopsis(spec.name),
                "`passportsim {}` drifted from docs/commands/{}.md; run `cargo run -p xtask -- docs`",
                spec.name,
                spec.name
            );
        }
    }

    #[test]
    fn every_help_is_utf8_without_crlf_and_names_every_argv_property() {
        for spec in registry::commands() {
            let help = subcommand(spec).render_long_help().to_string();
            assert!(!help.contains('\r'), "{}: CRLF found", spec.name);
            assert!(!help.contains(".exe"), "{}: {help}", spec.name);
            assert!(help.contains("--json"), "{}: {help}", spec.name);
            assert!(help.contains("--output"), "{}: {help}", spec.name);
            for property in properties(spec) {
                match property.form {
                    // Carried by `--json`, so deliberately absent from the flag list.
                    Form::Json => assert!(
                        !help.contains(&format!("--{} ", property.flag())),
                        "{}: {} is nested and must not be a flag",
                        spec.name,
                        property.name
                    ),
                    Form::Positional => assert!(
                        help.contains(&format!("[{}]", property.name))
                            || help.contains(&format!("<{}>", property.name)),
                        "{}: {} is positional and missing from the help",
                        spec.name,
                        property.name
                    ),
                    Form::Switch | Form::Value => assert!(
                        help.contains(&format!("--{}", property.flag())),
                        "{}: {} is missing from the help",
                        spec.name,
                        property.name
                    ),
                }
            }
        }
    }

    #[test]
    fn the_root_is_named_passportsim_and_never_the_exe() {
        let root = build();
        assert_eq!(root.get_name(), "passportsim");
        assert_eq!(root.get_bin_name(), Some("passportsim"));
        let help = root.clone().render_long_help().to_string();
        assert!(!help.contains(".exe"), "{help}");
    }

    #[test]
    fn every_registered_command_is_a_subcommand_in_name_order() {
        let root = build();
        let names: Vec<&str> = root.get_subcommands().map(Command::get_name).collect();
        let mut expected: Vec<&str> = registry::commands().iter().map(|s| s.name).collect();
        expected.sort_unstable();
        // The two host subcommands follow the registry, and no command registers their names.
        expected.extend([SERVE, MCP]);
        assert_eq!(names, expected);
        assert!(names.contains(&"start") && names.contains(&"clock"));
        assert!(registry::find_with_alias(SERVE).is_none());
        assert!(registry::find_with_alias(MCP).is_none());
    }

    #[test]
    fn serve_and_mcp_parse_their_documented_flags() {
        let root = build();
        let matches = root
            .clone()
            .try_get_matches_from([
                "passportsim",
                "serve",
                "--headless",
                "--port",
                "0",
                "--caps",
                "audio",
                "--max-instances",
                "3",
            ])
            .expect("serve flags");
        let (name, sub) = matches.subcommand().expect("a subcommand");
        assert_eq!(name, SERVE);
        assert!(sub.get_flag(HEADLESS_ARG));
        assert_eq!(sub.get_one::<u16>(PORT_ARG), Some(&0));
        assert_eq!(sub.get_one::<usize>(MAX_INSTANCES_ARG), Some(&3));
        assert!(
            root.clone()
                .try_get_matches_from(["passportsim", "serve", "--stop", "--headless"])
                .is_err(),
            "`--stop` stops a daemon and starts none"
        );
        let matches = root
            .try_get_matches_from(["passportsim", "mcp", "--caps", "audio,nfc"])
            .expect("mcp flags");
        let (name, sub) = matches.subcommand().expect("a subcommand");
        assert_eq!(name, MCP);
        assert_eq!(
            sub.get_one::<String>(CAPS_ARG).map(String::as_str),
            Some("audio,nfc")
        );
    }

    #[test]
    fn ephemeral_is_a_global_flag_of_every_registered_command() {
        for spec in registry::commands() {
            let matches = build()
                .try_get_matches_from(["passportsim", spec.name, "--ephemeral", "--help"])
                .err()
                .map(|e| e.kind());
            assert_eq!(
                matches,
                Some(clap::error::ErrorKind::DisplayHelp),
                "{}",
                spec.name
            );
        }
        let matches = build()
            .try_get_matches_from(["passportsim", "--ephemeral", "status"])
            .expect("before the command name");
        assert!(matches.get_flag(EPHEMERAL_ARG));
    }

    #[test]
    fn a_nested_property_never_becomes_a_flag() {
        let spec = registry::find("doctor").expect("doctor is registered");
        let props = properties(spec);
        let report = props
            .iter()
            .find(|p| p.name == "report")
            .expect("doctor takes a report object");
        assert_eq!(report.form, Form::Json);
        assert!(synopsis(spec).contains("--json <@file|->"));
    }

    /// An untyped property goes down the union branch of [`crate::args`], where the text `7`
    /// becomes the number 7.
    #[test]
    fn a_nullable_scalar_keeps_the_type_it_declares() {
        assert_eq!(
            kind_of(&serde_json::json!({ "type": ["string", "null"] })).as_deref(),
            Some("string")
        );
        assert_eq!(
            kind_of(&serde_json::json!({ "type": "integer" })).as_deref(),
            Some("integer")
        );
        assert_eq!(
            kind_of(&serde_json::json!({ "type": ["integer", "string"] })),
            None,
            "a genuine union stays untyped, and is read as the narrowest scalar it parses as"
        );
        assert_eq!(kind_of(&serde_json::json!({})), None);
    }

    #[test]
    fn a_boolean_that_defaults_to_true_gets_a_negated_spelling() {
        let spec = registry::find("stop").expect("stop is registered");
        let keep = properties(spec)
            .into_iter()
            .find(|p| p.name == "keep_artifacts")
            .expect("stop takes keep_artifacts");
        assert!(keep.negated);
        assert_eq!(keep.negated_flag(), "no-keep-artifacts");
        let help = subcommand(spec).render_long_help().to_string();
        assert!(help.contains("--no-keep-artifacts"), "{help}");
        assert!(
            !synopsis(spec).contains("no-keep-artifacts"),
            "the synopsis is the documented one, and the negated flag is not a schema property"
        );
    }

    #[test]
    fn a_tri_state_boolean_gets_a_negated_spelling_without_a_default() {
        let spec = registry::find("power").expect("power is registered");
        let present = properties(spec)
            .into_iter()
            .find(|p| p.name == "present")
            .expect("power takes present");
        assert!(present.negated);
        assert_eq!(present.negated_flag(), "no-present");
        let help = subcommand(spec).render_long_help().to_string();
        assert!(help.contains("--no-present"), "{help}");
        let schema = (spec.input_schema)().to_value();
        assert_eq!(
            schema["properties"]["present"].get("default"),
            None,
            "the spelling comes from `x-tri-state`, never from a `default` that is not true"
        );
    }

    #[test]
    fn a_boolean_is_a_switch_and_a_scalar_takes_a_value() {
        let spec = registry::find("stop").expect("stop is registered");
        let props = properties(spec);
        assert_eq!(
            props
                .iter()
                .find(|p| p.name == "keep_artifacts")
                .map(|p| p.form),
            Some(Form::Switch)
        );
        assert_eq!(
            synopsis(spec),
            "passportsim stop [instance] [--keep-artifacts] [--json <@file|->]"
        );
    }
}
