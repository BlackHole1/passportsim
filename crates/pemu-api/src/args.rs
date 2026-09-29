//! JSON argument helpers for command handlers: typed getters that answer `E_USAGE` naming the key,
//! and shared schema fragments.

use pemu_core::time::VTime;

use crate::error::{ApiError, E_USAGE};

pub(crate) type JsonMap = serde_json::Map<String, serde_json::Value>;

/// An argument that does not fit its schema.
pub(crate) fn usage(what: &str, detail: &str) -> ApiError {
    ApiError::new(E_USAGE, format!("argument `{what}`: {detail}"))
}

pub(crate) fn object(value: &serde_json::Value) -> Result<&JsonMap, ApiError> {
    value
        .as_object()
        .ok_or_else(|| usage("arguments", "expected a JSON object"))
}

/// Absent and `null` are both `None`.
pub(crate) fn opt_str<'a>(args: &'a JsonMap, key: &str) -> Result<Option<&'a str>, ApiError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(usage(key, "expected a string")),
    }
}

pub(crate) fn req_str<'a>(args: &'a JsonMap, key: &str) -> Result<&'a str, ApiError> {
    opt_str(args, key)?.ok_or_else(|| usage(key, "is required"))
}

pub(crate) fn opt_u64(args: &JsonMap, key: &str) -> Result<Option<u64>, ApiError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| usage(key, "expected a non-negative integer")),
        Some(_) => Err(usage(key, "expected a non-negative integer")),
    }
}

pub(crate) fn opt_bool(args: &JsonMap, key: &str) -> Result<Option<bool>, ApiError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(usage(key, "expected a boolean")),
    }
}

/// Refuses keys outside `known`, so a typo is reported instead of ignored.
pub(crate) fn only(args: &JsonMap, known: &[&str]) -> Result<(), ApiError> {
    for key in args.keys() {
        if !known.contains(&key.as_str()) {
            return Err(usage(key, &format!("unknown; expected one of {known:?}")));
        }
    }
    Ok(())
}

/// A bare integer in milliseconds, or `800us`, `250ms`, `1.5s`, `2m`.
pub(crate) fn opt_duration(args: &JsonMap, key: &str) -> Result<Option<VTime>, ApiError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => crate::matchers::parse_duration(s).map(Some),
        Some(serde_json::Value::Number(n)) => match n.as_u64() {
            Some(ms) => Ok(Some(VTime::from_ms(ms))),
            None => Err(usage(
                key,
                "expected whole milliseconds or a duration string",
            )),
        },
        Some(_) => Err(usage(key, "expected a duration")),
    }
}

pub(crate) fn enum_of<T>(
    args: &JsonMap,
    key: &str,
    parse: fn(&str) -> Option<T>,
    vocabulary: &str,
) -> Result<Option<T>, ApiError> {
    match opt_str(args, key)? {
        None => Ok(None),
        Some(text) => match parse(text) {
            Some(value) => Ok(Some(value)),
            None => Err(usage(key, &format!("`{text}` is not one of {vocabulary}"))),
        },
    }
}

/// Inlined per command because some MCP clients do not resolve `$ref`.
pub(crate) fn duration_schema(description: &str) -> serde_json::Value {
    serde_json::json!({
        "description": description,
        "oneOf": [
            { "type": "integer", "minimum": 0 },
            { "type": "string", "pattern": "^[0-9]+(\\.[0-9]+)?(us|ms|s|m)$" }
        ]
    })
}

/// Says nothing about being optional (the `required` list does): this fragment repeats in every
/// tool, and each word counts against the tool-list size budget.
pub(crate) fn instance_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "pattern": "^[pb][0-9]+$",
        "description": "Instance id."
    })
}
