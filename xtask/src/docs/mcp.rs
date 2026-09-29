//! The MCP `tools/list` payload built from the command registry.
//!
//! `tools/list` is filtered by the enabled caps groups at run time; the generated surface here is
//! never filtered by host, so the same bytes come out on macOS, Windows and in the browser. The
//! per-host availability travels as an `x-passport-hosts`
//! annotation instead, which is where the runtime refusal path (`E_HOST_UNSUPPORTED`) and the
//! generated documents read the same matrix.
//!
//! Annotation mapping:
//!
//! | `CommandSpec::annotations` | MCP |
//! |---|---|
//! | `read_only` | `readOnlyHint` |
//! | `destructive` | `destructiveHint` |
//! | `idempotent` | `idempotentHint` |
//! | group `device` | `openWorldHint` |
//!
//! `openWorldHint` is set for the three `passport_device_*` tools, which are exactly the Device
//! group. Every other tool talks to the emulator alone, so a closed world is the honest answer.

use serde_json::{Map, Value, json};

use super::model::{Command, HOSTS};

/// The MCP tool-list byte budget: the core tool list is at most 24 KB of JSON.
///
/// Every MCP session pays for the core list in its context, so this is a hard gate; raise it only
/// with a measurement of the core commands that needs it.
pub const CORE_BUDGET_BYTES: usize = 24 * 1024;

/// How deep a recursive `$defs` chain (`Matcher`, `UiQuery.within`) is inlined.
const INLINE_DEPTH: usize = 3;

/// One MCP tool object, as `tools/list` returns it.
///
/// The schemas carry no `$ref`: the `$defs` are written once in the registry and inlined per tool
/// (only the definitions a tool uses), so the served payload is larger than what `schemars` emits.
/// Inlining here is what makes [`tool_bytes`] and `xtask mcp-size` measure the bytes a client
/// actually pays for rather than a smaller shape nobody receives.
pub fn tool(command: &Command) -> Value {
    let spec = command.spec;
    let hosts: Vec<&str> = HOSTS
        .iter()
        .filter(|host| command.runs_on(**host))
        .map(|host| host.as_str())
        .collect();
    json!({
        "name": command.mcp_tool(),
        "description": spec.summary,
        // What an MCP client is offered, which drops the CLI-only arguments.
        "inputSchema": inlined(spec.agent_input_schema().to_value()),
        "outputSchema": inlined((spec.output_schema)().to_value()),
        "annotations": {
            "title": spec.summary,
            "readOnlyHint": spec.annotations.read_only,
            "destructiveHint": spec.annotations.destructive,
            "idempotentHint": spec.annotations.idempotent,
            "openWorldHint": spec.group == pemu_api::spec::CapsGroup::Device,
            "x-passport-hosts": hosts,
        },
    })
}

/// One schema document with the `$defs` it uses inlined and the `$defs` map itself dropped.
///
/// A recursive definition would not terminate, so a name is substituted at most [`INLINE_DEPTH`]
/// times along one path; past that the node becomes `true`, the schema that accepts anything,
/// which is the honest "not described further here" and leaves no dangling `$ref`. A `$ref` that
/// resolves to nothing is left alone, and then the `$defs` map stays so the document keeps
/// meaning.
fn inlined(schema: Value) -> Value {
    let Some(defs) = schema.get("$defs").and_then(Value::as_object).cloned() else {
        return schema;
    };
    let mut body = schema;
    if let Some(map) = body.as_object_mut() {
        map.remove("$defs");
    }
    let mut dangling = false;
    let mut out = substitute(body, &defs, &mut Vec::new(), &mut dangling);
    if dangling && let Some(map) = out.as_object_mut() {
        map.insert("$defs".to_string(), Value::Object(defs));
    }
    out
}

/// [`inlined`] over one node. `open` is the chain of `$defs` names being substituted above it.
fn substitute(
    node: Value,
    defs: &Map<String, Value>,
    open: &mut Vec<String>,
    dangling: &mut bool,
) -> Value {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref")
                && let Some(name) = reference.strip_prefix("#/$defs/")
            {
                let Some(target) = defs.get(name) else {
                    *dangling = true;
                    return Value::Object(map);
                };
                if open.iter().filter(|open| *open == name).count() >= INLINE_DEPTH {
                    return Value::Bool(true);
                }
                open.push(name.to_string());
                let mut body = substitute(target.clone(), defs, open, dangling);
                open.pop();
                // `schemars` emits `$ref` beside description-only keywords; they survive the
                // substitution, and the definition's own keywords win over none of them.
                if let Some(body) = body.as_object_mut() {
                    for (key, value) in map {
                        if key != "$ref" {
                            body.entry(key).or_insert(value);
                        }
                    }
                }
                return body;
            }
            let mut out = Map::new();
            for (key, value) in map {
                out.insert(key, substitute(value, defs, open, dangling));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| substitute(item, defs, open, dangling))
                .collect(),
        ),
        other => other,
    }
}

/// The `tools/list` payload of `commands`, in registry order: a fixed order keeps client prompt
/// caches valid.
pub fn tools_list(commands: &[Command]) -> Value {
    json!({ "tools": commands.iter().map(tool).collect::<Vec<_>>() })
}

/// Serialized size of one tool object, in bytes of compact JSON: what a client pays for it.
pub fn tool_bytes(command: &Command) -> usize {
    serde_json::to_string(&tool(command))
        .map(|text| text.len())
        .unwrap_or(0)
}

/// Serialized size of the whole payload, in bytes of compact JSON.
pub fn list_bytes(commands: &[Command]) -> usize {
    serde_json::to_string(&tools_list(commands))
        .map(|text| text.len())
        .unwrap_or(0)
}
