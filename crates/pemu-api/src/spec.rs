//! `CommandSpec`, the description of one registered command. `#[command]` builds one from a handler
//! function; see `crate::registry` for the attribute grammar.

pub use schemars::Schema;

/// Re-exported so `#[command(input = T)]` expansions can name it without their own dependency.
#[doc(hidden)]
pub use schemars;

use std::collections::BTreeSet;

use crate::error::{ApiError, ErrorCode};
use crate::output::Output;

/// Accepts anything; `#[command]` uses it when a command declares no `input` or `output` type.
pub fn any_schema() -> Schema {
    Schema::from(true)
}

/// Lowercase ASCII letters, digits and `_`, starting with a letter. The MCP tool name and HTTP
/// route are built from it.
pub fn name_is_valid(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A registry name, optionally with `.` separated parts, as in `serial.write` and `ui.expect`.
pub fn scenario_step_is_valid(key: &str) -> bool {
    !key.is_empty() && key.split('.').all(name_is_valid)
}

/// One registered command. The registry generates the CLI, MCP tools, HTTP and WS routes, scenario
/// steps, TypeScript types and docs from it. Every field is `Copy`, so a test can copy a spec and
/// change one field.
#[derive(Clone, Copy, Debug)]
pub struct CommandSpec {
    /// The MCP tool is `passport_<name>`, the HTTP route `POST /v1/instances/{id}/commands/{name}`.
    pub name: &'static str,
    /// MCP lists a tool only when its group is enabled; the group also fixes the range of the
    /// command's own error codes.
    pub group: CapsGroup,
    /// Reused verbatim by CLI help, the MCP tool description and the generated docs.
    pub summary: &'static str,
    /// Top-level properties become CLI flags; nested objects use `--json`.
    pub input_schema: fn() -> Schema,
    /// The MCP `outputSchema`. Errors use the envelope of `ApiError::schema` instead.
    pub output_schema: fn() -> Schema,
    pub annotations: Annotations,
    pub cli: CliShape,
    pub scenario_step: Option<&'static str>,
    /// Executed in CI against a fixture instance; at least one is required.
    pub examples: &'static [Example],
    /// Shared Core codes and codes in the command's own group range.
    pub errors: &'static [ErrorCode],
    pub handler: Handler,
}

impl CommandSpec {
    /// The input schema an agent surface publishes: the command's schema without the
    /// [`CliShape::cli_only`] properties. The CLI and generated docs keep the whole schema.
    pub fn agent_input_schema(&self) -> Schema {
        let schema = (self.input_schema)();
        if self.cli.cli_only.is_empty() {
            return schema;
        }
        let mut value = schema.to_value();
        if let Some(object) = value.as_object_mut() {
            if let Some(properties) = object.get_mut("properties").and_then(|p| p.as_object_mut()) {
                for name in self.cli.cli_only {
                    properties.remove(*name);
                }
            }
            if let Some(required) = object.get_mut("required").and_then(|r| r.as_array_mut()) {
                required.retain(|name| {
                    !name
                        .as_str()
                        .is_some_and(|name| self.cli.cli_only.contains(&name))
                });
            }
        }
        Schema::try_from(value).unwrap_or_else(|_| (self.input_schema)())
    }
}

/// Each group reserves one error-code number range (`CapsGroup::error_range`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CapsGroup {
    Core,
    Audio,
    Radio,
    Nfc,
    Debug,
    Device,
    /// `power` and `usb`: the power button, battery, USB cable and host client.
    Power,
}

impl CapsGroup {
    pub const ALL: [CapsGroup; 7] = [
        CapsGroup::Core,
        CapsGroup::Audio,
        CapsGroup::Radio,
        CapsGroup::Nfc,
        CapsGroup::Debug,
        CapsGroup::Device,
        CapsGroup::Power,
    ];

    pub const fn caps_name(self) -> &'static str {
        match self {
            CapsGroup::Core => "core",
            CapsGroup::Audio => "audio",
            CapsGroup::Radio => "radio",
            CapsGroup::Nfc => "nfc",
            CapsGroup::Debug => "debug",
            CapsGroup::Device => "device",
            CapsGroup::Power => "power",
        }
    }

    /// The groups a `--caps` list names, always including [`CapsGroup::Core`]. An unknown name is
    /// refused with a message listing every group. Shared by `passportsim mcp --caps` and `xtask
    /// mcp-size --caps` so they cannot disagree.
    pub fn parse_list(list: &str) -> Result<BTreeSet<CapsGroup>, String> {
        let mut groups = BTreeSet::from([CapsGroup::Core]);
        for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let group = CapsGroup::ALL
                .into_iter()
                .find(|g| g.caps_name() == name)
                .ok_or_else(|| {
                    let names: Vec<&str> = CapsGroup::ALL.iter().map(|g| g.caps_name()).collect();
                    format!(
                        "`{name}` is not a caps group; the groups are {}",
                        names.join(", ")
                    )
                })?;
            groups.insert(group);
        }
        Ok(groups)
    }
}

/// Behavior annotations, mapped to the MCP `readOnlyHint`, `destructiveHint` and `idempotentHint`.
/// Which annotation sets the MCP open-world hint is not decided.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Annotations {
    pub read_only: bool,
    /// Can destroy data outside the emulator; only device commands.
    pub destructive: bool,
    pub idempotent: bool,
    pub advances_time: bool,
    pub needs_instance: bool,
    /// Absent from the wasm build and browser-attached instances.
    pub native_only: bool,
    /// Every call needs a human confirmation an agent cannot give.
    pub human_confirm: bool,
}

impl Annotations {
    /// The `..` base of a `#[command]` expansion (`Default::default()` is not usable in a `const`).
    pub const EMPTY: Annotations = Annotations {
        read_only: false,
        destructive: false,
        idempotent: false,
        advances_time: false,
        needs_instance: false,
        native_only: false,
        human_confirm: false,
    };

    /// Returns the violated rule; `#[command]` makes the same checks at compile time.
    pub const fn check(self, group: CapsGroup) -> Result<(), &'static str> {
        if self.read_only && self.destructive {
            return Err("`read_only` and `destructive` exclude each other");
        }
        if self.destructive && !self.human_confirm {
            return Err("`destructive` needs `human_confirm`");
        }
        if matches!(group, CapsGroup::Device) && !self.native_only {
            return Err("the Device group is `native_only`");
        }
        Ok(())
    }
}

/// Shape of the command in the generated clap tree. Top-level input properties not in `positional`
/// become `--flags`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CliShape {
    pub positional: &'static [&'static str],
    pub aliases: &'static [&'static str],
    /// Input properties only a person on the native CLI may use (such as `--erase-nvs`); MCP and
    /// HTTP publish the schema without them. The handler still refuses them, because a surface is
    /// not a validator.
    pub cli_only: &'static [&'static str],
}

impl CliShape {
    pub const EMPTY: CliShape = CliShape {
        positional: &[],
        aliases: &[],
        cli_only: &[],
    };
}

/// One example invocation, executed in CI against a fixture instance.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Example {
    pub title: &'static str,
    /// The arguments as JSON object text. Only the object shape is checked; full schema validation
    /// would need a JSON Schema validator the workspace does not depend on.
    pub args: &'static str,
}

impl Example {
    pub fn args_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(self.args)
    }
}

/// Arguments failing the input schema return `E_USAGE`.
pub type Handler = fn(&mut HandlerCx, serde_json::Value) -> Result<Output, ApiError>;

/// What a handler runs against. Still empty.
pub struct HandlerCx {}
