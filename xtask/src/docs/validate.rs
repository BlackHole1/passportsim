//! A JSON Schema validator over the subset `schemars` emits for our input types.
//!
//! Every `Example` is executed in CI, and the generated schema is the contract of every surface;
//! an example that does not satisfy its own schema would ship a lie in `docs/commands/<name>.md`.
//! Rather than take a third-party validator into `[workspace.dependencies]` for that one check,
//! this validates the keywords
//! `schemars` 1.2 actually produces for the argument types of `pemu-api`:
//!
//! | Keyword | Covered |
//! |---|---|
//! | `type` (string or array of strings), `enum`, `const` | yes |
//! | `properties`, `required`, `additionalProperties` | yes |
//! | `items` (single schema), `prefixItems` | yes |
//! | `anyOf`, `oneOf`, `allOf`, `not` | yes |
//! | `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `multipleOf` | yes |
//! | `minLength`, `maxLength`, `minItems`, `maxItems`, `uniqueItems` | yes |
//! | `$ref` into `$defs`, boolean schemas (`true`, `false`) | yes |
//! | `pattern`, `patternProperties`, `format` | **UNVERIFIED: not checked** |
//!
//! `pattern` is deliberately not implemented: a regular-expression engine is exactly the
//! dependency this module exists to avoid, and a hand-written one would be a second unverified
//! surface. [`unchecked_keywords`] reports every schema that leans on an unchecked keyword, so a
//! command that starts using one is visible instead of silently unvalidated, and
//! `xtask docs` prints those as warnings.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// Keywords this module recognises but does not check (the UNVERIFIED row above).
pub const UNCHECKED: &[&str] = &["pattern", "patternProperties", "propertyNames", "format"];

/// One reason an instance does not satisfy a schema, with the JSON pointer of the failing value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// JSON pointer into the instance, `""` for the whole document (RFC 6901 spelling).
    pub at: String,
    /// What is wrong, one line.
    pub why: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.at.is_empty() {
            write!(f, "{}", self.why)
        } else {
            write!(f, "{}: {}", self.at, self.why)
        }
    }
}

/// Validates `instance` against `schema`, returning every problem found.
///
/// `schema` is the whole document: its `$defs` resolve the `$ref`s inside it.
pub fn validate(schema: &Value, instance: &Value) -> Result<(), Vec<Problem>> {
    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut problems = Vec::new();
    check(&defs, schema, instance, "", &mut problems);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Keywords whose value is one subschema.
const SUBSCHEMA: &[&str] = &[
    "items",
    "additionalItems",
    "contains",
    "additionalProperties",
    "propertyNames",
    "unevaluatedItems",
    "unevaluatedProperties",
    "not",
    "if",
    "then",
    "else",
];

/// Keywords whose value is an array of subschemas.
const SUBSCHEMA_LIST: &[&str] = &["allOf", "anyOf", "oneOf", "prefixItems"];

/// Keywords whose value is a map of *names* to subschemas. The keys there are property or
/// definition names, never schema keywords, which is why the walk below has to know the
/// difference: a command with a property called `format` is not a command that uses `format`.
const SUBSCHEMA_MAP: &[&str] = &[
    "properties",
    "patternProperties",
    "dependentSchemas",
    "$defs",
    "definitions",
];

/// Every unchecked keyword (of [`UNCHECKED`]) that appears anywhere in `schema`, in name order.
///
/// Only keywords in schema position count. The walk descends through the applicator keywords
/// above and never treats a `properties` map, an `enum` value or a `default` as a schema, so a
/// property *named* `format` or `pattern` does not raise a warning about a keyword the command
/// does not use.
pub fn unchecked_keywords(schema: &Value) -> BTreeSet<&'static str> {
    let mut found = BTreeSet::new();
    walk(schema, &mut found);
    found
}

/// Walks `value` **in schema position**.
fn walk(value: &Value, found: &mut BTreeSet<&'static str>) {
    let Value::Object(map) = value else {
        // A boolean schema (`true`, `false`) constrains nothing by keyword; anything else is not a
        // schema at all.
        return;
    };
    for key in UNCHECKED {
        if map.contains_key(*key) {
            found.insert(key);
        }
    }
    for (key, child) in map {
        let key = key.as_str();
        if SUBSCHEMA.contains(&key) {
            walk(child, found);
        } else if SUBSCHEMA_LIST.contains(&key) {
            if let Some(items) = child.as_array() {
                items.iter().for_each(|item| walk(item, found));
            }
        } else if SUBSCHEMA_MAP.contains(&key)
            && let Some(members) = child.as_object()
        {
            members.values().for_each(|member| walk(member, found));
        }
    }
}

fn push(problems: &mut Vec<Problem>, at: &str, why: impl Into<String>) {
    problems.push(Problem {
        at: at.to_string(),
        why: why.into(),
    });
}

/// The subschema `$ref` points at, resolving only `#/$defs/<name>` (the one form `schemars` 1.2
/// emits for our types).
fn resolve<'a>(defs: &'a Map<String, Value>, reference: &str) -> Option<&'a Value> {
    let name = reference.strip_prefix("#/$defs/")?;
    defs.get(name)
}

fn check(
    defs: &Map<String, Value>,
    schema: &Value,
    instance: &Value,
    at: &str,
    problems: &mut Vec<Problem>,
) {
    let map = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            push(problems, at, "schema `false` accepts no value");
            return;
        }
        Value::Object(map) => map,
        other => {
            push(problems, at, format!("schema is not an object: {other}"));
            return;
        }
    };

    if let Some(Value::String(reference)) = map.get("$ref") {
        match resolve(defs, reference) {
            Some(target) => check(defs, target, instance, at, problems),
            None => push(problems, at, format!("unresolvable `$ref` {reference:?}")),
        }
        // `schemars` emits `$ref` alone or beside description-only keywords, so nothing else of
        // this object constrains the instance.
        return;
    }

    check_type(map, instance, at, problems);
    check_values(map, instance, at, problems);
    check_numbers(map, instance, at, problems);
    check_strings(map, instance, at, problems);
    check_object(defs, map, instance, at, problems);
    check_array(defs, map, instance, at, problems);
    check_combinators(defs, map, instance, at, problems);
}

/// The JSON Schema type name of a value, `"integer"` reported as `"number"` by the caller when a
/// schema asks for a number.
fn type_of(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0) {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(expected: &str, instance: &Value) -> bool {
    let actual = type_of(instance);
    expected == actual || (expected == "number" && actual == "integer")
}

fn check_type(map: &Map<String, Value>, instance: &Value, at: &str, problems: &mut Vec<Problem>) {
    let Some(ty) = map.get("type") else {
        return;
    };
    let ok = match ty {
        Value::String(one) => type_matches(one, instance),
        Value::Array(any) => any
            .iter()
            .filter_map(Value::as_str)
            .any(|one| type_matches(one, instance)),
        _ => true,
    };
    if !ok {
        push(
            problems,
            at,
            format!("expected type {ty}, found {}", type_of(instance)),
        );
    }
}

fn check_values(map: &Map<String, Value>, instance: &Value, at: &str, problems: &mut Vec<Problem>) {
    if let Some(Value::Array(allowed)) = map.get("enum")
        && !allowed.contains(instance)
    {
        push(problems, at, format!("value {instance} is not in `enum`"));
    }
    if let Some(expected) = map.get("const")
        && expected != instance
    {
        push(
            problems,
            at,
            format!("value {instance} is not the `const` {expected}"),
        );
    }
}

fn check_numbers(
    map: &Map<String, Value>,
    instance: &Value,
    at: &str,
    problems: &mut Vec<Problem>,
) {
    let Some(value) = instance.as_f64() else {
        return;
    };
    let bound = |key: &str| map.get(key).and_then(Value::as_f64);
    if let Some(min) = bound("minimum")
        && value < min
    {
        push(problems, at, format!("{value} is below `minimum` {min}"));
    }
    if let Some(max) = bound("maximum")
        && value > max
    {
        push(problems, at, format!("{value} is above `maximum` {max}"));
    }
    if let Some(min) = bound("exclusiveMinimum")
        && value <= min
    {
        push(
            problems,
            at,
            format!("{value} is not above `exclusiveMinimum` {min}"),
        );
    }
    if let Some(max) = bound("exclusiveMaximum")
        && value >= max
    {
        push(
            problems,
            at,
            format!("{value} is not below `exclusiveMaximum` {max}"),
        );
    }
    if let Some(step) = bound("multipleOf")
        && step > 0.0
        && (value / step).fract().abs() > f64::EPSILON
    {
        push(problems, at, format!("{value} is no multiple of {step}"));
    }
}

fn check_strings(
    map: &Map<String, Value>,
    instance: &Value,
    at: &str,
    problems: &mut Vec<Problem>,
) {
    let Some(text) = instance.as_str() else {
        return;
    };
    // JSON Schema counts characters, not bytes.
    let length = text.chars().count() as u64;
    if let Some(min) = map.get("minLength").and_then(Value::as_u64)
        && length < min
    {
        push(
            problems,
            at,
            format!("string of {length} characters is shorter than `minLength` {min}"),
        );
    }
    if let Some(max) = map.get("maxLength").and_then(Value::as_u64)
        && length > max
    {
        push(
            problems,
            at,
            format!("string of {length} characters is longer than `maxLength` {max}"),
        );
    }
}

/// `at` extended by one object member or array index, RFC 6901 escaping included.
fn child(at: &str, token: &str) -> String {
    format!("{at}/{}", token.replace('~', "~0").replace('/', "~1"))
}

fn check_object(
    defs: &Map<String, Value>,
    map: &Map<String, Value>,
    instance: &Value,
    at: &str,
    problems: &mut Vec<Problem>,
) {
    let Some(fields) = instance.as_object() else {
        return;
    };
    let properties = map.get("properties").and_then(Value::as_object);
    if let Some(Value::Array(required)) = map.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            if !fields.contains_key(name) {
                push(problems, at, format!("missing required property {name:?}"));
            }
        }
    }
    for (name, value) in fields {
        match properties.and_then(|p| p.get(name)) {
            Some(subschema) => check(defs, subschema, value, &child(at, name), problems),
            None => match map.get("additionalProperties") {
                Some(Value::Bool(false)) => push(
                    problems,
                    &child(at, name),
                    "property is not allowed (`additionalProperties: false`)",
                ),
                Some(subschema) => check(defs, subschema, value, &child(at, name), problems),
                None => {}
            },
        }
    }
}

fn check_array(
    defs: &Map<String, Value>,
    map: &Map<String, Value>,
    instance: &Value,
    at: &str,
    problems: &mut Vec<Problem>,
) {
    let Some(items) = instance.as_array() else {
        return;
    };
    let count = items.len() as u64;
    if let Some(min) = map.get("minItems").and_then(Value::as_u64)
        && count < min
    {
        push(
            problems,
            at,
            format!("{count} items is below `minItems` {min}"),
        );
    }
    if let Some(max) = map.get("maxItems").and_then(Value::as_u64)
        && count > max
    {
        push(
            problems,
            at,
            format!("{count} items is above `maxItems` {max}"),
        );
    }
    if map.get("uniqueItems") == Some(&Value::Bool(true)) {
        for (index, item) in items.iter().enumerate() {
            if items[..index].contains(item) {
                push(problems, at, "items are not unique (`uniqueItems`)");
                break;
            }
        }
    }
    let prefix = map.get("prefixItems").and_then(Value::as_array);
    for (index, item) in items.iter().enumerate() {
        let at = child(at, &index.to_string());
        match prefix.and_then(|p| p.get(index)) {
            Some(subschema) => check(defs, subschema, item, &at, problems),
            None => {
                if let Some(subschema) = map.get("items") {
                    check(defs, subschema, item, &at, problems);
                }
            }
        }
    }
}

fn check_combinators(
    defs: &Map<String, Value>,
    map: &Map<String, Value>,
    instance: &Value,
    at: &str,
    problems: &mut Vec<Problem>,
) {
    if let Some(Value::Array(all)) = map.get("allOf") {
        for subschema in all {
            check(defs, subschema, instance, at, problems);
        }
    }
    if let Some(Value::Array(any)) = map.get("anyOf") {
        let matched = any
            .iter()
            .any(|subschema| passes(defs, subschema, instance));
        if !matched {
            push(problems, at, "value matches no branch of `anyOf`");
        }
    }
    if let Some(Value::Array(one)) = map.get("oneOf") {
        let matched = one
            .iter()
            .filter(|subschema| passes(defs, subschema, instance))
            .count();
        if matched != 1 {
            push(
                problems,
                at,
                format!("value matches {matched} branches of `oneOf`, expected exactly 1"),
            );
        }
    }
    if let Some(subschema) = map.get("not")
        && passes(defs, subschema, instance)
    {
        push(problems, at, "value matches `not`");
    }
}

fn passes(defs: &Map<String, Value>, schema: &Value, instance: &Value) -> bool {
    let mut problems = Vec::new();
    check(defs, schema, instance, "", &mut problems);
    problems.is_empty()
}
