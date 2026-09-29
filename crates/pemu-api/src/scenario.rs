//! `passportsim/scenario@1`: the document, its reader and its JUnit report.
//!
//! The workspace has no YAML crate (a core crate must build for wasm32), so this module reads a
//! YAML subset and refuses the rest with a line number:
//!
//! - block mappings and sequences, nested by indentation (spaces only);
//! - flow mappings `{a: b}` and flow sequences `[a, b]`, nested;
//! - plain, single-quoted and double-quoted scalars, `true`, `false`, `null`, integers;
//! - block literals (`key: |`);
//! - `#` comments outside a quoted scalar.
//!
//! Anchors, aliases, tags, multiple documents, folded scalars and complex keys are refused rather
//! than ignored. A step's command key resolves against `CommandSpec::scenario_step`, so a step
//! alias exists exactly when a command claims it. [`junit`] renders reports deterministically, so
//! the same reports give the same bytes on every host.

use std::fmt;
use std::fmt::Write as _;

use crate::error::{ApiError, E_USAGE};
use crate::receipt::{Caveat, CaveatKind, Strictness, Verdict};
use pemu_introspect::lvgl::UiTree;

pub const SCHEMA: &str = "passportsim/scenario@1";

/// Numbers are `i64` only: a float would not survive a JSON round trip identically on two hosts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Yaml {
    /// `null`, `~`, or an empty value.
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    /// Any other scalar, quoted or plain.
    Str(String),
    Seq(Vec<Yaml>),
    /// In written order.
    Map(Vec<(String, Yaml)>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScenarioError {
    /// 0 when the problem is the document as a whole.
    pub line: usize,
    pub message: String,
}

impl ScenarioError {
    pub fn at(line: usize, message: impl Into<String>) -> ScenarioError {
        ScenarioError {
            line,
            message: message.into(),
        }
    }

    pub fn whole(message: impl Into<String>) -> ScenarioError {
        ScenarioError {
            line: 0,
            message: message.into(),
        }
    }
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

impl From<ScenarioError> for ApiError {
    /// `E_USAGE`, with the line in the message so the author need not search for it.
    fn from(err: ScenarioError) -> ApiError {
        ApiError::new(E_USAGE, format!("scenario: {err}")).with_hint(
            "`scenario validate <file>` reads a file without running it; scenario@1 is a \
             restricted YAML",
        )
    }
}

/// Content has comments and trailing spaces removed.
#[derive(Clone, Debug)]
struct Line {
    indent: usize,
    text: String,
    number: usize,
}

impl Yaml {
    pub fn parse(text: &str) -> Result<Yaml, ScenarioError> {
        let lines = significant_lines(text)?;
        if lines.is_empty() {
            return Ok(Yaml::Null);
        }
        let mut at = 0;
        let value = parse_block(&lines, &mut at, lines[0].indent)?;
        if at < lines.len() {
            return Err(ScenarioError::at(
                lines[at].number,
                "this line is indented less than the document it belongs to",
            ));
        }
        Ok(value)
    }

    pub fn as_map(&self) -> Option<&[(String, Yaml)]> {
        match self {
            Yaml::Map(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn as_seq(&self) -> Option<&[Yaml]> {
        match self {
            Yaml::Seq(items) => Some(items),
            _ => None,
        }
    }

    /// `true`, `false` and integers render back to their written form, so `press: ok` and `press:
    /// "ok"` agree.
    pub fn as_str(&self) -> Option<String> {
        match self {
            Yaml::Str(text) => Some(text.clone()),
            Yaml::Bool(value) => Some(value.to_string()),
            Yaml::Int(value) => Some(value.to_string()),
            Yaml::Null => None,
            Yaml::Seq(_) | Yaml::Map(_) => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Yaml> {
        self.as_map()
            .and_then(|entries| entries.iter().find(|(k, _)| k == key).map(|(_, v)| v))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Yaml::Null)
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Yaml::Null => "null",
            Yaml::Bool(_) => "a boolean",
            Yaml::Int(_) => "an integer",
            Yaml::Str(_) => "a string",
            Yaml::Seq(_) => "a sequence",
            Yaml::Map(_) => "a mapping",
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Yaml::Null => serde_json::Value::Null,
            Yaml::Bool(value) => serde_json::Value::Bool(*value),
            Yaml::Int(value) => serde_json::Value::Number((*value).into()),
            Yaml::Str(text) => serde_json::Value::String(text.clone()),
            Yaml::Seq(items) => serde_json::Value::Array(items.iter().map(Yaml::to_json).collect()),
            Yaml::Map(entries) => {
                let mut map = serde_json::Map::new();
                for (key, value) in entries {
                    map.insert(key.clone(), value.to_json());
                }
                serde_json::Value::Object(map)
            }
        }
    }
}

/// Refuses a tab in the indentation, which would nest the same file differently by tab width.
/// Everything under `key: |` is content: `#` and blank lines are kept. A blank line takes the next
/// content line's indentation, since [`parse_literal`] pads by `indent - child`; blank lines
/// pending when the literal ends are trailing, which YAML strips.
fn significant_lines(text: &str) -> Result<Vec<Line>, ScenarioError> {
    let mut lines = Vec::new();
    // Indentation of the `key: |` line while a block literal is open.
    let mut literal_at: Option<usize> = None;
    // Line numbers of blank lines seen inside the open literal, not yet placed.
    let mut pending_blanks: Vec<usize> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let leading = raw.len() - raw.trim_start().len();
        if let Some(open) = literal_at {
            if raw.trim().is_empty() {
                pending_blanks.push(number);
                continue;
            }
            if indent > open {
                if raw[..leading].contains('\t') {
                    return Err(ScenarioError::at(
                        number,
                        "a tab is not indentation in scenario@1; use spaces",
                    ));
                }
                for blank in pending_blanks.drain(..) {
                    lines.push(Line {
                        indent,
                        text: String::new(),
                        number: blank,
                    });
                }
                lines.push(Line {
                    indent,
                    text: raw[indent..].trim_end().to_owned(),
                    number,
                });
                continue;
            }
            // The indentation fell back: the literal ended, and its trailing blank lines go.
            literal_at = None;
            pending_blanks.clear();
        }
        if raw[..leading].contains('\t') {
            return Err(ScenarioError::at(
                number,
                "a tab is not indentation in scenario@1; use spaces",
            ));
        }
        let text = strip_comment(&raw[indent..]).trim_end().to_owned();
        if text.is_empty() {
            continue;
        }
        if text == "---" || text == "..." {
            return Err(ScenarioError::at(
                number,
                "scenario@1 is one document; `---` and `...` are not part of it",
            ));
        }
        if text.ends_with(": |") || text.ends_with(": |-") {
            literal_at = Some(indent);
        }
        lines.push(Line {
            indent,
            text,
            number,
        });
    }
    Ok(lines)
}

fn strip_comment(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(q) => {
                if byte == b'\\' && q == b'"' {
                    index += 1;
                } else if byte == q {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                // Only at the start of the line or after a space, so `bg=#ffd928` in a plain scalar
                // survives.
                b'#' if index == 0 || bytes[index - 1] == b' ' => return &text[..index],
                _ => {}
            },
        }
        index += 1;
    }
    text
}

fn parse_block(lines: &[Line], at: &mut usize, indent: usize) -> Result<Yaml, ScenarioError> {
    let line = &lines[*at];
    if line.text == "-" || line.text.starts_with("- ") {
        return parse_seq(lines, at, indent);
    }
    if let Some(split) = key_end(&line.text) {
        let _ = split;
        return parse_map(lines, at, indent);
    }
    let value = parse_flow(&line.text, line.number)?;
    *at += 1;
    Ok(value)
}

fn parse_seq(lines: &[Line], at: &mut usize, indent: usize) -> Result<Yaml, ScenarioError> {
    let mut items = Vec::new();
    while *at < lines.len() && lines[*at].indent == indent {
        let line = lines[*at].clone();
        if !(line.text == "-" || line.text.starts_with("- ")) {
            break;
        }
        *at += 1;
        let rest = line.text[1..].trim_start().to_owned();
        let child_indent = indent + (line.text.len() - line.text[1..].trim_start().len());
        if rest.is_empty() {
            items.push(parse_child(lines, at, indent)?);
        } else if key_end(&rest).is_some() {
            // `- key: value` starts a mapping whose later keys line up under `key`.
            items.push(parse_inline_map(
                lines,
                at,
                &rest,
                line.number,
                child_indent,
            )?);
        } else {
            items.push(parse_flow(&rest, line.number)?);
        }
    }
    Ok(Yaml::Seq(items))
}

fn parse_map(lines: &[Line], at: &mut usize, indent: usize) -> Result<Yaml, ScenarioError> {
    let mut entries: Vec<(String, Yaml)> = Vec::new();
    while *at < lines.len() && lines[*at].indent == indent {
        let line = lines[*at].clone();
        let Some(split) = key_end(&line.text) else {
            break;
        };
        *at += 1;
        let (key, value) = read_entry(lines, at, &line.text, split, line.number, indent)?;
        if entries.iter().any(|(existing, _)| *existing == key) {
            return Err(ScenarioError::at(
                line.number,
                format!("`{key}` appears twice in the same mapping"),
            ));
        }
        entries.push((key, value));
    }
    Ok(Yaml::Map(entries))
}

fn parse_inline_map(
    lines: &[Line],
    at: &mut usize,
    first: &str,
    number: usize,
    indent: usize,
) -> Result<Yaml, ScenarioError> {
    let split = key_end(first).ok_or_else(|| ScenarioError::at(number, "expected `key: value`"))?;
    let (key, value) = read_entry(lines, at, first, split, number, indent)?;
    let mut entries = vec![(key, value)];
    if *at < lines.len() && lines[*at].indent == indent {
        let Yaml::Map(rest) = parse_map(lines, at, indent)? else {
            return Err(ScenarioError::at(number, "expected a mapping"));
        };
        for (key, value) in rest {
            if entries.iter().any(|(existing, _)| *existing == key) {
                return Err(ScenarioError::at(
                    number,
                    format!("`{key}` appears twice in the same mapping"),
                ));
            }
            entries.push((key, value));
        }
    }
    Ok(Yaml::Map(entries))
}

fn read_entry(
    lines: &[Line],
    at: &mut usize,
    text: &str,
    split: usize,
    number: usize,
    indent: usize,
) -> Result<(String, Yaml), ScenarioError> {
    let key = scalar_key(&text[..split], number)?;
    let rest = text[split + 1..].trim();
    let value = if rest.is_empty() {
        parse_child(lines, at, indent)?
    } else if rest == "|" || rest == "|-" {
        parse_literal(lines, at, indent, rest == "|")?
    } else {
        parse_flow(rest, number)?
    };
    Ok((key, value))
}

fn parse_child(lines: &[Line], at: &mut usize, indent: usize) -> Result<Yaml, ScenarioError> {
    if *at < lines.len() && lines[*at].indent > indent {
        let child = lines[*at].indent;
        parse_block(lines, at, child)
    } else {
        Ok(Yaml::Null)
    }
}

fn parse_literal(
    lines: &[Line],
    at: &mut usize,
    indent: usize,
    keep_final_newline: bool,
) -> Result<Yaml, ScenarioError> {
    let mut body = String::new();
    let child = match lines.get(*at) {
        Some(line) if line.indent > indent => line.indent,
        _ => return Ok(Yaml::Str(String::new())),
    };
    while *at < lines.len() && lines[*at].indent >= child {
        let line = &lines[*at];
        body.push_str(&" ".repeat(line.indent - child));
        body.push_str(&line.text);
        body.push('\n');
        *at += 1;
    }
    if !keep_final_newline {
        while body.ends_with('\n') {
            body.pop();
        }
    }
    Ok(Yaml::Str(body))
}

/// Skips quoted scalars and flow collections, so `re: "main_task: app_main"` has one key and `{a:
/// 1, b: 2}` has none at the top level.
fn key_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut depth = 0usize;
    for (index, &byte) in bytes.iter().enumerate() {
        match quote {
            Some(q) => {
                if byte == b'\\' && q == b'"' {
                    continue;
                }
                if byte == q {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'{' | b'[' => depth += 1,
                b'}' | b']' => depth = depth.saturating_sub(1),
                b':' if depth == 0 => {
                    let next = bytes.get(index + 1);
                    if next.is_none() || next == Some(&b' ') {
                        return Some(index);
                    }
                }
                _ => {}
            },
        }
    }
    None
}

fn scalar_key(text: &str, number: usize) -> Result<String, ScenarioError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ScenarioError::at(number, "a mapping key cannot be empty"));
    }
    if text.starts_with('?') || text.starts_with('&') || text.starts_with('*') {
        return Err(ScenarioError::at(
            number,
            "complex keys, anchors and aliases are not part of scenario@1",
        ));
    }
    match parse_flow(text, number)? {
        Yaml::Str(key) => Ok(key),
        Yaml::Bool(value) => Ok(value.to_string()),
        Yaml::Int(value) => Ok(value.to_string()),
        other => Err(ScenarioError::at(
            number,
            format!("a mapping key must be a scalar, saw {}", other.kind()),
        )),
    }
}

fn parse_flow(text: &str, number: usize) -> Result<Yaml, ScenarioError> {
    let mut cursor = Cursor {
        bytes: text.as_bytes(),
        at: 0,
        number,
        depth: 0,
    };
    cursor.skip_spaces();
    let value = cursor.value()?;
    cursor.skip_spaces();
    if cursor.at < cursor.bytes.len() {
        return Err(ScenarioError::at(
            number,
            "unexpected text after a complete value",
        ));
    }
    Ok(value)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
    number: usize,
    /// A plain scalar ends at `,`, `}`, `]` or `: ` only inside a flow collection, so a top-level
    /// `Boot, check, navigate` is one scalar.
    depth: usize,
}

impl Cursor<'_> {
    fn skip_spaces(&mut self) {
        while self.bytes.get(self.at) == Some(&b' ') {
            self.at += 1;
        }
    }

    fn error(&self, message: impl Into<String>) -> ScenarioError {
        ScenarioError::at(self.number, message)
    }

    fn value(&mut self) -> Result<Yaml, ScenarioError> {
        match self.bytes.get(self.at) {
            None => Ok(Yaml::Null),
            Some(b'{') => self.flow_map(),
            Some(b'[') => self.flow_seq(),
            Some(b'"') => self.double_quoted(),
            Some(b'\'') => self.single_quoted(),
            Some(b'&' | b'*' | b'!') => {
                Err(self.error("anchors, aliases and tags are not part of scenario@1"))
            }
            Some(_) => Ok(self.plain()),
        }
    }

    fn flow_map(&mut self) -> Result<Yaml, ScenarioError> {
        self.at += 1;
        self.depth += 1;
        let mut entries: Vec<(String, Yaml)> = Vec::new();
        loop {
            self.skip_spaces();
            match self.bytes.get(self.at) {
                None => return Err(self.error("a flow mapping is not closed")),
                Some(b'}') => {
                    self.at += 1;
                    self.depth -= 1;
                    return Ok(Yaml::Map(entries));
                }
                Some(b',') if !entries.is_empty() => {
                    self.at += 1;
                    continue;
                }
                _ => {}
            }
            let key = match self.value()? {
                Yaml::Str(key) => key,
                Yaml::Bool(value) => value.to_string(),
                Yaml::Int(value) => value.to_string(),
                other => {
                    return Err(self.error(format!(
                        "a mapping key must be a scalar, saw {}",
                        other.kind()
                    )));
                }
            };
            self.skip_spaces();
            if self.bytes.get(self.at) != Some(&b':') {
                return Err(self.error(format!("`{key}` has no `:`")));
            }
            self.at += 1;
            self.skip_spaces();
            let value = if matches!(self.bytes.get(self.at), Some(b',' | b'}')) {
                Yaml::Null
            } else {
                self.value()?
            };
            if entries.iter().any(|(existing, _)| *existing == key) {
                return Err(self.error(format!("`{key}` appears twice in the same mapping")));
            }
            entries.push((key, value));
        }
    }

    fn flow_seq(&mut self) -> Result<Yaml, ScenarioError> {
        self.at += 1;
        self.depth += 1;
        let mut items = Vec::new();
        loop {
            self.skip_spaces();
            match self.bytes.get(self.at) {
                None => return Err(self.error("a flow sequence is not closed")),
                Some(b']') => {
                    self.at += 1;
                    self.depth -= 1;
                    return Ok(Yaml::Seq(items));
                }
                Some(b',') if !items.is_empty() => {
                    self.at += 1;
                    continue;
                }
                _ => {}
            }
            items.push(self.value()?);
        }
    }

    fn double_quoted(&mut self) -> Result<Yaml, ScenarioError> {
        self.at += 1;
        let mut out = String::new();
        while let Some(&byte) = self.bytes.get(self.at) {
            self.at += 1;
            match byte {
                b'"' => return Ok(Yaml::Str(out)),
                b'\\' => {
                    let escape = *self
                        .bytes
                        .get(self.at)
                        .ok_or_else(|| self.error("a string ends inside an escape"))?;
                    self.at += 1;
                    out.push(match escape {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'0' => '\0',
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        _ => {
                            return Err(self.error(
                                "a string has a `\\` escape scenario@1 does not know \
                                 (\\n, \\t, \\r, \\0, \\\", \\\\, \\/)",
                            ));
                        }
                    });
                }
                _ => push_utf8(&mut out, self.bytes, &mut self.at, byte),
            }
        }
        Err(self.error("a double-quoted string is not closed"))
    }

    fn single_quoted(&mut self) -> Result<Yaml, ScenarioError> {
        self.at += 1;
        let mut out = String::new();
        while let Some(&byte) = self.bytes.get(self.at) {
            self.at += 1;
            if byte == b'\'' {
                // YAML doubles a single quote to escape it.
                if self.bytes.get(self.at) == Some(&b'\'') {
                    self.at += 1;
                    out.push('\'');
                    continue;
                }
                return Ok(Yaml::Str(out));
            }
            push_utf8(&mut out, self.bytes, &mut self.at, byte);
        }
        Err(self.error("a single-quoted string is not closed"))
    }

    /// At the top level it runs to the end of the line; inside a flow collection it ends at the
    /// closing `,`, `}` or `]`, or at a key's `:`.
    fn plain(&mut self) -> Yaml {
        let start = self.at;
        while let Some(&byte) = self.bytes.get(self.at) {
            if self.depth > 0 {
                if matches!(byte, b',' | b'}' | b']') {
                    break;
                }
                if byte == b':'
                    && matches!(
                        self.bytes.get(self.at + 1),
                        None | Some(b' ' | b',' | b'}' | b']')
                    )
                {
                    break;
                }
            }
            self.at += 1;
        }
        let text = String::from_utf8_lossy(&self.bytes[start..self.at])
            .trim()
            .to_owned();
        scalar_of(&text)
    }
}

fn push_utf8(out: &mut String, bytes: &[u8], at: &mut usize, first: u8) {
    if first.is_ascii() {
        out.push(char::from(first));
        return;
    }
    let start = *at - 1;
    while *at < bytes.len() && (bytes[*at] & 0xC0) == 0x80 {
        *at += 1;
    }
    out.push_str(&String::from_utf8_lossy(&bytes[start..*at]));
}

fn scalar_of(text: &str) -> Yaml {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => Yaml::Null,
        "true" | "True" | "TRUE" => Yaml::Bool(true),
        "false" | "False" | "FALSE" => Yaml::Bool(false),
        // `on` and `off` stay strings, not YAML 1.1 booleans: `power: on` is the power step's
        // vocabulary.
        _ => match text.parse::<i64>() {
            Ok(value) => Yaml::Int(value),
            Err(_) => Yaml::Str(text.to_owned()),
        },
    }
}

/// Anything else is `E_USAGE`.
pub const TOP_LEVEL_KEYS: &[&str] = &[
    "schema",
    "name",
    "description",
    "image",
    "tags",
    "setup",
    "defaults",
    "fail_on",
    "steps",
];

/// Fields every step may carry beside its one command key. `expect` asserts on the command's answer
/// ([`answer_failures`]), so a scenario can say which advertiser it found, not just that the call
/// succeeded.
pub const STEP_FIELDS: &[&str] = &["name", "id", "timeout", "continue_on_error", "expect"];

/// `repeat` and `set` are control flow, `delay` and `expect_not` are `run` the other way round, and
/// `ui.expect` is `ui` plus [`UiExpect`].
pub const BUILT_IN_STEPS: &[&str] = &["repeat", "set", "delay", "expect_not", "ui.expect"];

/// Defaults for every waiting step; `None` means the command's own default. `settle` and
/// `total_timeout` are refused rather than dropped: a scenario that thinks it settles between UI
/// reads would pass on a half-drawn screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Defaults {
    pub timeout: Option<String>,
    /// Overrides the command's `wall_budget_ms`.
    pub wall_budget: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// Empty when none was written.
    pub name: String,
    /// Referenced by a later step; empty when none was written.
    pub id: String,
    pub key: String,
    pub value: Yaml,
    pub timeout: Option<String>,
    pub continue_on_error: bool,
    /// A mapping the command's JSON answer must hold ([`answer_failures`]); `None` when the step
    /// asserts only that the call succeeded.
    pub expect: Option<Yaml>,
    pub line: usize,
}

impl Step {
    pub fn is_known(key: &str, aliases: &[&str]) -> bool {
        BUILT_IN_STEPS.contains(&key) || aliases.contains(&key)
    }

    /// Names the step index and the closest valid key.
    pub fn unknown(&self, index: usize, aliases: &[&str]) -> ScenarioError {
        let mut known: Vec<&str> = BUILT_IN_STEPS.to_vec();
        known.extend_from_slice(aliases);
        known.sort_unstable();
        let closest = known
            .iter()
            .min_by_key(|candidate| edit_distance(&self.key, candidate))
            .copied()
            .unwrap_or("wait");
        ScenarioError::at(
            self.line,
            format!(
                "step {}: `{}` is not a scenario@1 step key; did you mean `{closest}`? \
                 (known: {})",
                index + 1,
                self.key,
                known.join(", ")
            ),
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scenario {
    /// Also the JUnit test-suite name.
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    /// The firmware the scenario was written against, checked against the bound instance's
    /// firmware.
    pub image: String,
    /// The runner applies the world-state keys (`usb`, `battery`, `nfc`, `mic`) through `env`,
    /// checks `power` against the lifecycle, and refuses start-time keys by name.
    pub setup: Yaml,
    pub defaults: Defaults,
    /// The runner arms them on every waiting step.
    pub fail_on: Vec<Yaml>,
    pub steps: Vec<Step>,
}

impl Scenario {
    /// Refuses a wrong or missing `schema`, a missing `name`, an unknown top-level key, and a step
    /// that is not a mapping with exactly one command key.
    pub fn parse(text: &str) -> Result<Scenario, ScenarioError> {
        let document = Yaml::parse(text)?;
        let entries = document.as_map().ok_or_else(|| {
            ScenarioError::whole(format!(
                "a scenario is a mapping with a `schema` key, saw {}",
                document.kind()
            ))
        })?;
        for (key, _) in entries {
            if !TOP_LEVEL_KEYS.contains(&key.as_str()) {
                return Err(ScenarioError::whole(format!(
                    "`{key}` is not a scenario@1 top-level key (known: {})",
                    TOP_LEVEL_KEYS.join(", ")
                )));
            }
        }
        match document.get("schema").and_then(Yaml::as_str).as_deref() {
            Some(SCHEMA) => {}
            Some(_) => {
                return Err(ScenarioError::whole(format!("`schema` must be `{SCHEMA}`")));
            }
            None => {
                return Err(ScenarioError::whole(format!(
                    "a scenario starts with `schema: {SCHEMA}`"
                )));
            }
        }
        let name = document
            .get("name")
            .and_then(Yaml::as_str)
            .ok_or_else(|| ScenarioError::whole("a scenario needs a `name`"))?;
        let tags = match document.get("tags") {
            None | Some(Yaml::Null) => Vec::new(),
            Some(Yaml::Seq(items)) => items.iter().filter_map(Yaml::as_str).collect(),
            Some(other) => {
                return Err(ScenarioError::whole(format!(
                    "`tags` is a sequence, saw {}",
                    other.kind()
                )));
            }
        };
        let fail_on = match document.get("fail_on") {
            None | Some(Yaml::Null) => Vec::new(),
            Some(Yaml::Seq(items)) => items.clone(),
            Some(other) => {
                return Err(ScenarioError::whole(format!(
                    "`fail_on` is a sequence of matchers, saw {}",
                    other.kind()
                )));
            }
        };
        Ok(Scenario {
            name,
            description: document
                .get("description")
                .and_then(Yaml::as_str)
                .unwrap_or_default(),
            tags,
            image: document
                .get("image")
                .and_then(Yaml::as_str)
                .unwrap_or_default(),
            setup: document.get("setup").cloned().unwrap_or(Yaml::Null),
            defaults: defaults_of(document.get("defaults"))?,
            fail_on,
            steps: steps_of(document.get("steps"), &step_lines(text))?,
        })
    }

    /// Checks every step key before the scenario starts, rather than at the step that has it.
    pub fn check_steps(&self, aliases: &[&str]) -> Result<(), ScenarioError> {
        // A malformed `ui.expect` is a validation error rather than a failure halfway through a
        // boot.
        let expect_reads = |step: &Step, line: usize| {
            if step.key == "ui.expect" {
                UiExpect::parse(&step.value).map_err(|err| ScenarioError::at(line, err.message))?;
            }
            Ok::<(), ScenarioError>(())
        };
        for (index, step) in self.steps.iter().enumerate() {
            if !Step::is_known(&step.key, aliases) {
                return Err(step.unknown(index, aliases));
            }
            expect_reads(step, step.line)?;
            if step.key == "repeat" {
                for mut inner in repeat_steps(&step.value)? {
                    if !Step::is_known(&inner.key, aliases) {
                        // A nested body has no line of its own; use the `repeat`'s.
                        inner.line = step.line;
                        return Err(inner.unknown(index, aliases));
                    }
                    expect_reads(&inner, step.line)?;
                }
            }
        }
        Ok(())
    }
}

pub fn repeat_steps(value: &Yaml) -> Result<Vec<Step>, ScenarioError> {
    let inner = value.get("steps").ok_or_else(|| {
        ScenarioError::whole("`repeat` needs `steps`, a sequence of steps to repeat")
    })?;
    steps_of(Some(inner), &[])
}

/// At least 1.
pub fn repeat_times(value: &Yaml) -> Result<u32, ScenarioError> {
    match value.get("times") {
        Some(Yaml::Int(times)) if *times >= 1 => u32::try_from(*times)
            .map_err(|_| ScenarioError::whole("`repeat.times` is larger than this build repeats")),
        _ => Err(ScenarioError::whole(
            "`repeat` needs `times`, an integer of at least 1",
        )),
    }
}

fn defaults_of(value: Option<&Yaml>) -> Result<Defaults, ScenarioError> {
    let Some(value) = value else {
        return Ok(Defaults::default());
    };
    if value.is_null() {
        return Ok(Defaults::default());
    }
    let entries = value.as_map().ok_or_else(|| {
        ScenarioError::whole(format!("`defaults` is a mapping, saw {}", value.kind()))
    })?;
    let mut defaults = Defaults::default();
    for (key, value) in entries {
        match key.as_str() {
            "timeout" => defaults.timeout = scalar_default(key, value)?,
            "wall_budget" => defaults.wall_budget = scalar_default(key, value)?,
            "settle" => {
                return Err(ScenarioError::whole(
                    "`defaults.settle` is not supported: `run` takes no `settle`, and a `ui:*` \
                     wait settles at the next LVGL safe point by itself",
                ));
            }
            "total_timeout" => {
                return Err(ScenarioError::whole(
                    "`defaults.total_timeout` is a whole-scenario virtual bound nothing enforces \
                     yet; bound each step with `defaults.timeout`, or the whole run with \
                     `--wall-budget-ms`, which is checked between steps",
                ));
            }
            other => {
                return Err(ScenarioError::whole(format!(
                    "`defaults.{other}` is not a scenario@1 default \
                     (known: timeout, wall_budget; settle and total_timeout are refused by name)"
                )));
            }
        }
    }
    Ok(defaults)
}

/// The 1-based source line of each top-level step, found the way [`parse_seq`] finds the sequence,
/// since [`Yaml`] carries no positions. Steps inside a `repeat` take the `repeat`'s line.
fn step_lines(text: &str) -> Vec<usize> {
    let Ok(lines) = significant_lines(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut at = match lines
        .iter()
        .position(|line| line.indent == 0 && line.text == "steps:")
    {
        Some(index) => index + 1,
        None => return out,
    };
    let Some(indent) = lines.get(at).map(|line| line.indent) else {
        return out;
    };
    while let Some(line) = lines.get(at) {
        if line.indent < indent {
            break;
        }
        if line.indent == indent && (line.text == "-" || line.text.starts_with("- ")) {
            out.push(line.number);
        }
        at += 1;
    }
    out
}

/// Refuses a value that is not a scalar rather than dropping it.
fn scalar_default(key: &str, value: &Yaml) -> Result<Option<String>, ScenarioError> {
    match value {
        Yaml::Null => Ok(None),
        other => other.as_str().map(Some).ok_or_else(|| {
            ScenarioError::whole(format!(
                "`defaults.{key}` is a duration such as `5s`, saw {}",
                other.kind()
            ))
        }),
    }
}

/// `lines` holds each item's source line; it is empty for a nested `repeat` body.
fn steps_of(value: Option<&Yaml>, lines: &[usize]) -> Result<Vec<Step>, ScenarioError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let items = value.as_seq().ok_or_else(|| {
        ScenarioError::whole(format!("`steps` is a sequence, saw {}", value.kind()))
    })?;
    let mut steps = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let entries = item.as_map().ok_or_else(|| {
            ScenarioError::whole(format!(
                "step {}: a step is a mapping with one command key, saw {}",
                index + 1,
                item.kind()
            ))
        })?;
        let mut step = Step {
            name: String::new(),
            id: String::new(),
            key: String::new(),
            value: Yaml::Null,
            timeout: None,
            continue_on_error: false,
            expect: None,
            line: lines.get(index).copied().unwrap_or(0),
        };
        let mut command_keys = Vec::new();
        for (key, value) in entries {
            match key.as_str() {
                "name" => step.name = value.as_str().unwrap_or_default(),
                "id" => step.id = value.as_str().unwrap_or_default(),
                "timeout" => step.timeout = value.as_str(),
                "continue_on_error" => step.continue_on_error = value == &Yaml::Bool(true),
                "expect" => {
                    if value.as_map().is_none() {
                        return Err(ScenarioError::at(
                            step.line,
                            format!(
                                "step {}: `expect` is a mapping of the answer's fields, saw {}",
                                index + 1,
                                value.kind()
                            ),
                        ));
                    }
                    step.expect = Some(value.clone());
                }
                other => {
                    command_keys.push(other.to_owned());
                    step.key = other.to_owned();
                    step.value = value.clone();
                }
            }
        }
        if command_keys.len() != 1 {
            return Err(ScenarioError::whole(format!(
                "step {}: a step has exactly one command key beside {:?}, saw {}",
                index + 1,
                STEP_FIELDS,
                if command_keys.is_empty() {
                    "none".to_owned()
                } else {
                    command_keys.join(", ")
                }
            )));
        }
        // A built-in step has no command answer to hold `expect` against, so it would be ignored.
        if step.expect.is_some() && BUILT_IN_STEPS.contains(&step.key.as_str()) {
            return Err(ScenarioError::at(
                step.line,
                format!(
                    "step {}: `expect` asserts a command's answer, and `{}` is not a command step",
                    index + 1,
                    step.key
                ),
            ));
        }
        steps.push(step);
    }
    Ok(steps)
}

/// Two-row Levenshtein distance, for the "did you mean" of an unknown step key.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, left) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, right) in b.iter().enumerate() {
            let cost = usize::from(left != right);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        previous.clone_from(&current);
    }
    previous[b.len()]
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StepStatus {
    Pass,
    /// An assertion did not hold, or a `fail_on` matcher fired.
    Fail,
    /// An unknown key, a bad argument, or an error envelope from the command.
    Error,
    /// An earlier step ended the scenario.
    Skipped,
}

impl StepStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            StepStatus::Pass => "pass",
            StepStatus::Fail => "fail",
            StepStatus::Error => "error",
            StepStatus::Skipped => "skipped",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepReport {
    /// 0-based, `repeat` bodies counted once per pass.
    pub index: usize,
    pub name: String,
    pub key: String,
    pub status: StepStatus,
    /// Microseconds.
    pub vt_us: u64,
    /// Microseconds.
    pub elapsed_vt_us: u64,
    pub error: Option<serde_json::Value>,
}

impl StepReport {
    pub fn line(&self) -> String {
        let mut text = format!("{:>3}. {}", self.index + 1, self.key);
        if !self.name.is_empty() {
            let _ = write!(text, " ({})", self.name);
        }
        let _ = write!(
            text,
            " {} vt={}us (+{}us)",
            self.status.as_str(),
            self.vt_us,
            self.elapsed_vt_us
        );
        if let Some(error) = &self.error
            && let Some(message) = error.get("message").and_then(serde_json::Value::as_str)
        {
            let _ = write!(text, ": {message}");
        }
        text
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Pass,
    PassWithCaveats,
    Fail,
    Error,
    WallBudget,
}

impl RunStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            RunStatus::Pass => "pass",
            RunStatus::PassWithCaveats => "pass_with_caveats",
            RunStatus::Fail => "fail",
            RunStatus::Error => "error",
            RunStatus::WallBudget => "wall_budget",
        }
    }

    /// `error` and `wall_budget` are `Fail` as a verdict; [`exit_code`] gives them their own codes.
    pub const fn verdict(self) -> Verdict {
        match self {
            RunStatus::Pass => Verdict::Pass,
            RunStatus::PassWithCaveats => Verdict::PassWithCaveats,
            RunStatus::Fail | RunStatus::Error | RunStatus::WallBudget => Verdict::Fail,
        }
    }

    pub const fn assertions_hold(self) -> bool {
        matches!(self, RunStatus::Pass | RunStatus::PassWithCaveats)
    }

    /// For picking the worst of a batch, worst last. Not an exit code.
    pub const fn severity(self) -> u8 {
        match self {
            RunStatus::Pass => 0,
            RunStatus::PassWithCaveats => 1,
            RunStatus::Fail => 2,
            RunStatus::WallBudget => 3,
            RunStatus::Error => 4,
        }
    }
}

/// Pass, `pass_with_caveats` and fail go through [`crate::receipt::exit_code`], so nothing
/// disagrees about `--strict`. `wall_budget` exits 6 WALL_TIMEOUT (retrying is meaningful) and
/// `error` exits 8 INFRA, which CI must not confuse with a real failure.
#[must_use]
pub fn exit_code(status: RunStatus, caveats: &[Caveat], strictness: Strictness) -> u8 {
    match status {
        RunStatus::WallBudget => 6,
        RunStatus::Error => 8,
        other => crate::receipt::exit_code(other.verdict(), caveats, strictness),
    }
}

/// Named after the receipt fields each caveat comes from. If a second surface needs the list, this
/// belongs beside [`crate::receipt::CaveatKind`].
const fn caveat_kind(kind: CaveatKind) -> &'static str {
    match kind {
        CaveatKind::ClassU => "class_u",
        CaveatKind::Unmodeled => "unmodeled_first_touch",
        CaveatKind::Tripwire => "tripwire",
        CaveatKind::TimingLint => "timing_lint",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub name: String,
    /// Relative and forward-slashed; empty for an inline scenario.
    pub source: String,
    pub instance: String,
    pub status: RunStatus,
    /// Empty for a scenario that never bound an instance.
    pub caveats: Vec<Caveat>,
    pub vt_us: u64,
    pub steps: Vec<StepReport>,
}

impl Report {
    /// So the file still appears in the JUnit output as one errored case.
    pub fn unreadable(source: &str, error: &ScenarioError) -> Report {
        Report {
            name: source.to_owned(),
            source: source.to_owned(),
            instance: String::new(),
            status: RunStatus::Error,
            caveats: Vec::new(),
            vt_us: 0,
            steps: vec![StepReport {
                index: 0,
                name: "read the scenario".to_owned(),
                key: "scenario".to_owned(),
                status: StepStatus::Error,
                vt_us: 0,
                elapsed_vt_us: 0,
                error: Some(serde_json::json!({
                    "code": "E_USAGE",
                    "message": error.to_string(),
                })),
            }],
        }
    }

    /// A batch's start or fork refused: one errored step carrying the refusal.
    pub fn not_run(name: &str, source: &str, error: &crate::error::ApiError) -> Report {
        Report {
            name: name.to_owned(),
            source: source.to_owned(),
            instance: String::new(),
            status: RunStatus::Error,
            caveats: Vec::new(),
            vt_us: 0,
            steps: vec![StepReport {
                index: 0,
                name: "get an instance".to_owned(),
                key: "setup".to_owned(),
                status: StepStatus::Error,
                vt_us: 0,
                elapsed_vt_us: 0,
                error: Some(error.to_json()),
            }],
        }
    }

    pub fn failed_step(&self) -> Option<usize> {
        self.steps
            .iter()
            .find(|step| matches!(step.status, StepStatus::Fail | StepStatus::Error))
            .map(|step| step.index)
    }

    /// `strictness` changes the answer: the same caveats exit 7 under `--strict` and 10 without it.
    pub fn to_json(&self, strictness: Strictness) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "source": self.source,
            "instance": self.instance,
            "status": self.status.as_str(),
            "exit_code": exit_code(self.status, &self.caveats, strictness),
            "caveats": self.caveats.iter().map(|caveat| serde_json::json!({
                "kind": caveat_kind(caveat.kind),
                "detail": caveat.detail,
            })).collect::<Vec<_>>(),
            "vt_us": self.vt_us,
            "failed_step": self.failed_step(),
            "steps": self.steps.iter().map(|step| serde_json::json!({
                "index": step.index,
                "name": step.name,
                "key": step.key,
                "status": step.status.as_str(),
                "vt_us": step.vt_us,
                "elapsed_vt_us": step.elapsed_vt_us,
                "error": step.error,
            })).collect::<Vec<_>>(),
        })
    }

    /// The summary line, every failed step and the last few, so a 200-step scenario costs about
    /// what a 10-step one does.
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "scenario {} {} ({} of {} steps, vt={}us)",
            self.name,
            self.status.as_str(),
            self.steps
                .iter()
                .filter(|step| step.status != StepStatus::Skipped)
                .count(),
            self.steps.len(),
            self.vt_us
        );
        let tail_from = self.steps.len().saturating_sub(TEXT_TAIL_STEPS);
        let mut shown = 0usize;
        for (position, step) in self.steps.iter().enumerate() {
            let failed = matches!(step.status, StepStatus::Fail | StepStatus::Error);
            if failed || position >= tail_from {
                let _ = write!(text, "\n{}", step.line());
                shown += 1;
            }
        }
        if shown < self.steps.len() {
            let _ = write!(text, "\n({} step(s) not shown)", self.steps.len() - shown);
        }
        text
    }
}

pub const TEXT_TAIL_STEPS: usize = 5;

/// Renders scenario reports as one JUnit XML file: one `<testsuite>` per scenario and one
/// `<testcase>` per step, so CI points at the failing step. Times are virtual seconds; wall time is
/// host noise.
#[must_use]
pub fn junit(reports: &[Report]) -> String {
    let mut tests = 0usize;
    let mut failures = 0usize;
    let mut errors = 0usize;
    let mut skipped = 0usize;
    for report in reports {
        for step in &report.steps {
            tests += 1;
            match step.status {
                StepStatus::Pass => {}
                StepStatus::Fail => failures += 1,
                StepStatus::Error => errors += 1,
                StepStatus::Skipped => skipped += 1,
            }
        }
    }
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        out,
        "<testsuites name=\"passportsim\" tests=\"{tests}\" failures=\"{failures}\" \
         errors=\"{errors}\" skipped=\"{skipped}\">"
    );
    for report in reports {
        let suite_tests = report.steps.len();
        let suite_failures = count(report, StepStatus::Fail);
        let suite_errors = count(report, StepStatus::Error);
        let suite_skipped = count(report, StepStatus::Skipped);
        let _ = writeln!(
            out,
            "  <testsuite name=\"{}\" tests=\"{suite_tests}\" failures=\"{suite_failures}\" \
             errors=\"{suite_errors}\" skipped=\"{suite_skipped}\" time=\"{}\">",
            escape(&report.name),
            seconds(report.vt_us)
        );
        if !report.source.is_empty() {
            let _ = writeln!(
                out,
                "    <properties><property name=\"source\" value=\"{}\"/></properties>",
                escape(&report.source)
            );
        }
        for step in &report.steps {
            let name = if step.name.is_empty() {
                format!("{}. {}", step.index + 1, step.key)
            } else {
                format!("{}. {} ({})", step.index + 1, step.key, step.name)
            };
            let _ = write!(
                out,
                "    <testcase classname=\"{}\" name=\"{}\" time=\"{}\"",
                escape(&report.name),
                escape(&name),
                seconds(step.elapsed_vt_us)
            );
            match step.status {
                StepStatus::Pass => {
                    let _ = writeln!(out, "/>");
                }
                StepStatus::Skipped => {
                    let _ = writeln!(out, ">\n      <skipped/>\n    </testcase>");
                }
                StepStatus::Fail | StepStatus::Error => {
                    let tag = if step.status == StepStatus::Fail {
                        "failure"
                    } else {
                        "error"
                    };
                    let error = step.error.clone().unwrap_or(serde_json::Value::Null);
                    let code = error
                        .get("code")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let message = error
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("the step did not pass");
                    let _ = writeln!(
                        out,
                        ">\n      <{tag} type=\"{}\" message=\"{}\">{}</{tag}>\n    </testcase>",
                        escape(code),
                        escape(message),
                        escape(&step.line())
                    );
                }
            }
        }
        let _ = writeln!(out, "  </testsuite>");
    }
    out.push_str("</testsuites>\n");
    out
}

fn count(report: &Report, status: StepStatus) -> usize {
    report
        .steps
        .iter()
        .filter(|step| step.status == status)
        .count()
}

/// Built from integers, never a float, so the same report renders the same bytes on every host.
fn seconds(vt_us: u64) -> String {
    format!("{}.{:06}", vt_us / 1_000_000, vt_us % 1_000_000)
}

/// Also drops the control characters XML 1.0 cannot represent.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// So one failed match over a long list does not bury the rest of the report.
const ANSWER_QUOTE_MAX: usize = 240;

/// What in a command's JSON answer does not hold a step's `expect`; empty when everything holds.
/// The match is partial: a mapping holds when every key it names holds, a sequence when each item
/// holds for some element, and a scalar when it equals a value of the same JSON type (`"247"` never
/// equals `247`). Each failure is one line naming the path, the expectation and the answer.
pub fn answer_failures(expected: &Yaml, actual: &serde_json::Value) -> Vec<String> {
    let mut failures = Vec::new();
    answer_at("", expected, actual, &mut failures);
    failures
}

fn answer_at(path: &str, expected: &Yaml, actual: &serde_json::Value, out: &mut Vec<String>) {
    let shown = if path.is_empty() { "the answer" } else { path };
    match expected {
        Yaml::Map(entries) => {
            let Some(object) = actual.as_object() else {
                out.push(format!(
                    "`{shown}`: expected an object, saw {}",
                    quote(actual)
                ));
                return;
            };
            for (key, want) in entries {
                let at = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match object.get(key) {
                    Some(have) => answer_at(&at, want, have, out),
                    None => out.push(format!("`{at}`: the answer has no such field")),
                }
            }
        }
        Yaml::Seq(items) => {
            let Some(array) = actual.as_array() else {
                out.push(format!("`{shown}`: expected a list, saw {}", quote(actual)));
                return;
            };
            for want in items {
                let held = array.iter().any(|have| {
                    let mut probe = Vec::new();
                    answer_at("", want, have, &mut probe);
                    probe.is_empty()
                });
                if !held {
                    out.push(format!(
                        "`{shown}`: no element holds {}; the list is {}",
                        want.to_json(),
                        quote(actual)
                    ));
                }
            }
        }
        scalar => {
            let held = match (scalar, actual) {
                (Yaml::Null, serde_json::Value::Null) => true,
                (Yaml::Bool(want), serde_json::Value::Bool(have)) => want == have,
                (Yaml::Int(want), serde_json::Value::Number(have)) => {
                    have.as_i64() == Some(*want)
                        || (*want >= 0 && have.as_u64() == u64::try_from(*want).ok())
                }
                (Yaml::Str(want), serde_json::Value::String(have)) => want == have,
                _ => false,
            };
            if !held {
                out.push(format!(
                    "`{shown}`: expected {}, saw {}",
                    scalar.to_json(),
                    quote(actual)
                ));
            }
        }
    }
}

fn quote(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= ANSWER_QUOTE_MAX {
        return text;
    }
    let cut: String = text.chars().take(ANSWER_QUOTE_MAX).collect();
    format!("{cut}...")
}

/// A `ui.expect` query: every field must hold for one node. `text` is exact, `text_re` uses the
/// matcher grammar, `class` may omit `lv_`, `visible` defaults to `true`, and `bg` and `border` are
/// `#rrggbb` because the official firmware marks its selection by style only. `role` and `state:
/// selected` need `setup.hints`, which this build does not read, and are refused by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiQuery {
    pub text: Option<String>,
    pub text_re: Option<(String, crate::matchers::TextPattern)>,
    pub class: Option<String>,
    pub state: Option<String>,
    /// An `eN` of the tree being checked.
    pub reference: Option<String>,
    pub bg: Option<String>,
    pub border: Option<String>,
    /// Some ancestor matches this query.
    pub within: Option<Box<UiQuery>>,
    pub visible: bool,
}

fn lvgl_state(name: &str) -> bool {
    pemu_introspect::lvgl::STATES
        .iter()
        .any(|(_, state)| *state == name)
}

fn colour_of(value: &Yaml, key: &str) -> Result<String, ScenarioError> {
    let text = value
        .as_str()
        .ok_or_else(|| ScenarioError::whole(format!("`{key}` is a colour, `#rrggbb`")))?
        .to_ascii_lowercase();
    let hex = text.strip_prefix('#').unwrap_or("");
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ScenarioError::whole(format!(
            "`{key}` is not a `#rrggbb` colour"
        )));
    }
    Ok(text)
}

impl UiQuery {
    pub fn parse(value: &Yaml) -> Result<UiQuery, ScenarioError> {
        let entries = value
            .as_map()
            .ok_or_else(|| ScenarioError::whole("a UI query is a mapping such as `{text: OK}`"))?;
        let mut query = UiQuery {
            text: None,
            text_re: None,
            class: None,
            state: None,
            reference: None,
            bg: None,
            border: None,
            within: None,
            visible: true,
        };
        let text = |value: &Yaml, key: &str| {
            value
                .as_str()
                .ok_or_else(|| ScenarioError::whole(format!("`{key}` is text")))
        };
        for (key, value) in entries {
            match key.as_str() {
                "text" => query.text = Some(text(value, key)?),
                "text_re" => {
                    let body = text(value, key)?;
                    // The compiler's message quotes file content, so the error names the key only.
                    let pattern = crate::matchers::TextPattern::compile(&body).map_err(|_| {
                        ScenarioError::whole(
                            "`text_re` is not a pattern this build compiles (see the text classes of `/.../` \
                                 matchers)",
                        )
                    })?;
                    query.text_re = Some((body, pattern));
                }
                "class" => {
                    let class = text(value, key)?;
                    query.class = Some(class.strip_prefix("lv_").unwrap_or(&class).to_owned());
                }
                "state" => {
                    let state = text(value, key)?;
                    if state == "selected" {
                        return Err(ScenarioError::whole(
                            "`state: selected` comes from `setup.hints`, which \
                             this build does not read; the official selection is a local style, so \
                             ask for it with `within: {bg: \"#ffd928\"}`",
                        ));
                    }
                    // `hidden` is an LVGL flag written as a state.
                    if state != "hidden" && !lvgl_state(&state) {
                        return Err(ScenarioError::whole(
                            "`state` is not an LVGL state (checked, focused, edited, pressed, \
                             disabled, hidden)",
                        ));
                    }
                    query.state = Some(state);
                }
                "ref" => query.reference = Some(text(value, key)?),
                "bg" => query.bg = Some(colour_of(value, key)?),
                "border" => query.border = Some(colour_of(value, key)?),
                "within" => query.within = Some(Box::new(UiQuery::parse(value)?)),
                "visible" => {
                    query.visible = match value {
                        Yaml::Bool(visible) => *visible,
                        _ => return Err(ScenarioError::whole("`visible` is true or false")),
                    }
                }
                "role" => {
                    return Err(ScenarioError::whole(
                        "`role` comes from `setup.hints`, which this build does \
                         not read; query `class`, `text` and style instead",
                    ));
                }
                other => {
                    return Err(ScenarioError::whole(format!(
                        "`{other}` is not a UI query field (text, text_re, class, state, ref, \
                         within, visible, bg, border)"
                    )));
                }
            }
        }
        Ok(query)
    }

    #[must_use]
    pub fn matches(&self, tree: &UiTree, index: usize) -> bool {
        let Some(node) = tree.nodes.get(index) else {
            return false;
        };
        let colour =
            |c: Option<pemu_introspect::lvgl::Color>| c.map(pemu_introspect::lvgl::Color::render);
        self.text
            .as_ref()
            .is_none_or(|t| node.text.as_ref() == Some(t))
            && self
                .text_re
                .as_ref()
                .is_none_or(|(_, p)| node.text.as_deref().is_some_and(|t| p.matches(t)))
            && self.class.as_ref().is_none_or(|c| &node.class == c)
            && self.state.as_ref().is_none_or(|s| match s.as_str() {
                "hidden" => node.is_hidden(),
                s => node.states().contains(&s),
            })
            && self.reference.as_ref().is_none_or(|r| &node.reference == r)
            && self
                .bg
                .as_ref()
                .is_none_or(|bg| colour(node.bg).as_ref() == Some(bg))
            && self
                .border
                .as_ref()
                .is_none_or(|b| colour(node.border).as_ref() == Some(b))
            && (!self.visible || visible(tree, index))
            && self.within.as_ref().is_none_or(|outer| {
                ancestors(tree, index).any(|ancestor| outer.matches(tree, ancestor))
            })
    }

    /// In pre-order.
    #[must_use]
    pub fn find(&self, tree: &UiTree) -> Vec<usize> {
        (0..tree.nodes.len())
            .filter(|&index| self.matches(tree, index))
            .collect()
    }
}

/// Nearest first.
fn ancestors(tree: &UiTree, index: usize) -> impl Iterator<Item = usize> + '_ {
    let mut parent = tree.nodes.get(index).map_or(0, |node| node.parent);
    std::iter::from_fn(move || {
        if parent == 0 {
            return None;
        }
        let at = tree.nodes.iter().position(|node| node.obj == parent)?;
        parent = tree.nodes[at].parent;
        Some(at)
    })
}

/// Neither the node nor an ancestor is hidden, and its box has an area inside the display.
fn visible(tree: &UiTree, index: usize) -> bool {
    let node = &tree.nodes[index];
    let on_screen = node.w > 0
        && node.h > 0
        && node.x < tree.hor_res
        && node.y < tree.ver_res
        && node.x.saturating_add(node.w) > 0
        && node.y.saturating_add(node.h) > 0;
    on_screen
        && !node.is_hidden()
        && ancestors(tree, index).all(|ancestor| !tree.nodes[ancestor].is_hidden())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CountCmp {
    Eq,
    Gte,
    Lte,
}

/// One line of a partial `tree:`: a class, an optional exact text, and the lines indented below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreePattern {
    pub class: String,
    pub text: Option<String>,
    pub children: Vec<TreePattern>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiExpect {
    /// Each query matches at least one node.
    pub contains: Vec<UiQuery>,
    /// No query matches any node.
    pub absent: Vec<UiQuery>,
    pub count: Option<(UiQuery, CountCmp, u64)>,
    /// Must appear with nesting and order kept; extra nodes are allowed.
    pub tree: Vec<TreePattern>,
}

fn queries(value: &Yaml, key: &str) -> Result<Vec<UiQuery>, ScenarioError> {
    match value {
        Yaml::Seq(items) => items.iter().map(UiQuery::parse).collect(),
        Yaml::Map(_) => Ok(vec![UiQuery::parse(value)?]),
        _ => Err(ScenarioError::whole(format!(
            "`{key}` is a list of UI queries"
        ))),
    }
}

/// `- <class> ["<text>"]`, nested by indentation.
fn tree_patterns(text: &str) -> Result<Vec<TreePattern>, ScenarioError> {
    let mut lines = Vec::new();
    for raw in text.lines().filter(|line| !line.trim().is_empty()) {
        let indent = raw.len() - raw.trim_start().len();
        let body = raw
            .trim()
            .strip_prefix("- ")
            .ok_or_else(|| ScenarioError::whole("a `tree` line does not start with `- `"))?;
        let (class, rest) = body.split_once(' ').unwrap_or((body, ""));
        let rest = rest.trim();
        let text = if rest.is_empty() {
            None
        } else {
            Some(
                rest.strip_prefix('"')
                    .and_then(|r| r.strip_suffix('"'))
                    .ok_or_else(|| {
                        ScenarioError::whole(
                            "in a `tree` line, the text after the class is a quoted string",
                        )
                    })?
                    .to_owned(),
            )
        };
        let class = class.strip_prefix("lv_").unwrap_or(class).to_owned();
        lines.push((
            indent,
            TreePattern {
                class,
                text,
                children: Vec::new(),
            },
        ));
    }
    fn build(lines: &[(usize, TreePattern)], at: &mut usize, indent: usize) -> Vec<TreePattern> {
        let mut out = Vec::new();
        while let Some((here, pattern)) = lines.get(*at) {
            if *here < indent {
                break;
            }
            let mut pattern = pattern.clone();
            *at += 1;
            if let Some((next, _)) = lines.get(*at)
                && *next > *here
            {
                pattern.children = build(lines, at, *next);
            }
            out.push(pattern);
        }
        out
    }
    let first = lines.first().map_or(0, |(indent, _)| *indent);
    let mut at = 0;
    let forest = build(&lines, &mut at, first);
    if at != lines.len() {
        return Err(ScenarioError::whole(
            "`tree` is indented less than its first line",
        ));
    }
    Ok(forest)
}

impl UiExpect {
    pub fn parse(value: &Yaml) -> Result<UiExpect, ScenarioError> {
        let entries = value.as_map().ok_or_else(|| {
            ScenarioError::whole("`ui.expect` is a mapping of contains, absent, count and tree")
        })?;
        let mut expect = UiExpect {
            contains: Vec::new(),
            absent: Vec::new(),
            count: None,
            tree: Vec::new(),
        };
        for (key, value) in entries {
            match key.as_str() {
                "contains" => expect.contains = queries(value, key)?,
                "absent" => expect.absent = queries(value, key)?,
                "count" => {
                    let entries = value.as_map().ok_or_else(|| {
                        ScenarioError::whole("`count` is a mapping of `query` and one bound")
                    })?;
                    if let Some((other, _)) = entries
                        .iter()
                        .find(|(k, _)| !matches!(k.as_str(), "query" | "eq" | "gte" | "lte"))
                    {
                        return Err(ScenarioError::whole(format!(
                            "`count.{other}` is not a `count` field (query, eq, gte, lte)"
                        )));
                    }
                    let query = UiQuery::parse(value.get("query").ok_or_else(|| {
                        ScenarioError::whole("`count` needs `query` and one of eq, gte, lte")
                    })?)?;
                    let bounds: Vec<(&str, CountCmp)> = [
                        ("eq", CountCmp::Eq),
                        ("gte", CountCmp::Gte),
                        ("lte", CountCmp::Lte),
                    ]
                    .into_iter()
                    .filter(|(name, _)| value.get(name).is_some())
                    .collect();
                    let [(name, cmp)] = bounds[..] else {
                        return Err(ScenarioError::whole(
                            "`count` needs exactly one of eq, gte, lte",
                        ));
                    };
                    let Some(Yaml::Int(n)) = value
                        .get(name)
                        .filter(|v| matches!(v, Yaml::Int(n) if *n >= 0))
                    else {
                        return Err(ScenarioError::whole(format!(
                            "`count.{name}` is a whole number"
                        )));
                    };
                    expect.count = Some((query, cmp, *n as u64));
                }
                "tree" => {
                    let text = value.as_str().ok_or_else(|| {
                        ScenarioError::whole("`tree` is a block of `- class` lines")
                    })?;
                    expect.tree = tree_patterns(&text)?;
                }
                "selected" => {
                    return Err(ScenarioError::whole(
                        "`selected` is `state: selected`, which comes from `setup.hints`, which \
                         this build does not read; the official selection is a local \
                         style, so write `contains: [{text: T, within: {bg: \"#ffd928\"}}]`",
                    ));
                }
                other => {
                    return Err(ScenarioError::whole(format!(
                        "`{other}` is not a `ui.expect` field (contains, absent, count, tree)"
                    )));
                }
            }
        }
        if expect.contains.is_empty()
            && expect.absent.is_empty()
            && expect.count.is_none()
            && expect.tree.is_empty()
        {
            return Err(ScenarioError::whole(
                "`ui.expect` asserts nothing: give contains, absent, count or tree",
            ));
        }
        Ok(expect)
    }

    /// One line per assertion that does not hold.
    #[must_use]
    pub fn failures(&self, tree: &UiTree) -> Vec<String> {
        let mut out = Vec::new();
        for (i, query) in self.contains.iter().enumerate() {
            if query.find(tree).is_empty() {
                out.push(format!(
                    "contains[{i}]: no node matches {}",
                    describe(query)
                ));
            }
        }
        for (i, query) in self.absent.iter().enumerate() {
            let hits = query.find(tree);
            if let Some(first) = hits.first() {
                out.push(format!(
                    "absent[{i}]: {} matches {} node(s), first {}",
                    describe(query),
                    hits.len(),
                    tree.nodes[*first].reference
                ));
            }
        }
        if let Some((query, cmp, want)) = &self.count {
            let got = query.find(tree).len() as u64;
            let holds = match cmp {
                CountCmp::Eq => got == *want,
                CountCmp::Gte => got >= *want,
                CountCmp::Lte => got <= *want,
            };
            if !holds {
                out.push(format!(
                    "count: {} matches {got} node(s), expected {cmp:?} {want}",
                    describe(query)
                ));
            }
        }
        if !self.tree.is_empty() && !forest_matches(tree, &self.tree) {
            out.push("tree: the partial tree does not appear in the walked tree".to_owned());
        }
        out
    }
}

fn describe(query: &UiQuery) -> String {
    let mut parts = Vec::new();
    if let Some(text) = &query.text {
        parts.push(format!("text {text:?}"));
    }
    if let Some((source, _)) = &query.text_re {
        parts.push(format!("text_re /{source}/"));
    }
    if let Some(class) = &query.class {
        parts.push(format!("class {class}"));
    }
    if let Some(state) = &query.state {
        parts.push(format!("state {state}"));
    }
    if let Some(reference) = &query.reference {
        parts.push(format!("ref {reference}"));
    }
    if let Some(bg) = &query.bg {
        parts.push(format!("bg {bg}"));
    }
    if let Some(border) = &query.border {
        parts.push(format!("border {border}"));
    }
    if let Some(within) = &query.within {
        parts.push(format!("within {}", describe(within)));
    }
    format!("{{{}}}", parts.join(", "))
}

/// Whether `patterns` match, in order, nodes of the pre-order range `lo..hi`, each pattern's
/// children among that node's descendants. The search backtracks, so it is memoized on (pattern
/// list, range); without the memo a near match costs exponential time in its depth.
fn forest_matches(tree: &UiTree, patterns: &[TreePattern]) -> bool {
    // `ends[i]`: one past the last pre-order index of `i`'s subtree.
    let mut ends = vec![0usize; tree.nodes.len()];
    for index in (0..tree.nodes.len()).rev() {
        let depth = tree.nodes[index].depth;
        let mut end = index + 1;
        while end < tree.nodes.len() && tree.nodes[end].depth > depth {
            end = ends[end];
        }
        ends[index] = end;
    }
    let mut memo = std::collections::BTreeMap::new();
    forest_in(tree, &ends, patterns, 0, tree.nodes.len(), &mut memo)
}

/// A pattern list (a suffix of some `children` vector, by address and length) and a node range.
type ForestKey = (usize, usize, usize, usize);

fn forest_in(
    tree: &UiTree,
    ends: &[usize],
    patterns: &[TreePattern],
    lo: usize,
    hi: usize,
    memo: &mut std::collections::BTreeMap<ForestKey, bool>,
) -> bool {
    let Some((first, rest)) = patterns.split_first() else {
        return true;
    };
    let key = (patterns.as_ptr() as usize, patterns.len(), lo, hi);
    if let Some(known) = memo.get(&key) {
        return *known;
    }
    let found = (lo..hi).any(|index| {
        let node = &tree.nodes[index];
        node.class == first.class
            && first
                .text
                .as_ref()
                .is_none_or(|t| node.text.as_ref() == Some(t))
            && forest_in(tree, ends, &first.children, index + 1, ends[index], memo)
            && forest_in(tree, ends, rest, ends[index], hi, memo)
    });
    memo.insert(key, found);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shapes the reader must cope with: block mappings inside block sequences, flow mappings
    /// with quoted patterns, and a nested `repeat`.
    const SMOKE: &str = r#"
schema: passportsim/scenario@1
name: official-menu-smoke
description: Boot official firmware, check the menu, navigate
tags: [smoke, ui]
setup: {power: on, usb: open, battery: {mv: 3900, soc: 80}}
defaults: {timeout: 5s}
fail_on:
  - serial: {re: "Guru Meditation Error|abort\\(\\) was called"}
steps:
  - name: app starts
    wait: {serial: {re: "main_task: Calling app_main\\(\\)"}}   # the IDF line
  - press: down
  - id: before_loop
    env: {battery: {soc: 50}}
  - repeat:
      times: 2
      steps:
        - press: ok
        - delay: 500ms
"#;

    #[test]
    fn the_smoke_scenario_reads_into_the_document_it_describes() {
        let scenario = Scenario::parse(SMOKE).expect("the smoke scenario is scenario@1");
        assert_eq!(scenario.name, "official-menu-smoke");
        assert_eq!(scenario.tags, vec!["smoke", "ui"]);
        assert_eq!(scenario.defaults.timeout.as_deref(), Some("5s"));
        assert_eq!(scenario.fail_on.len(), 1);
        assert_eq!(
            scenario.fail_on[0]
                .get("serial")
                .and_then(|serial| serial.get("re"))
                .and_then(Yaml::as_str)
                .as_deref(),
            Some(r"Guru Meditation Error|abort\(\) was called"),
            "a quoted regular expression keeps its backslashes and its pipe"
        );
        assert_eq!(
            scenario
                .setup
                .get("power")
                .and_then(Yaml::as_str)
                .as_deref(),
            Some("on"),
            "`on` is the power vocabulary, not the YAML 1.1 boolean"
        );
        assert_eq!(
            scenario
                .setup
                .get("battery")
                .and_then(|b| b.get("mv"))
                .cloned(),
            Some(Yaml::Int(3900))
        );
        let keys: Vec<&str> = scenario.steps.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, vec!["wait", "press", "env", "repeat"]);
        assert_eq!(scenario.steps[0].name, "app starts");
        assert_eq!(scenario.steps[2].id, "before_loop");
        assert_eq!(
            scenario.steps[1].value,
            Yaml::Str("down".to_owned()),
            "a plain scalar step value"
        );
    }

    #[test]
    fn a_repeat_carries_its_own_steps() {
        let scenario = Scenario::parse(SMOKE).expect("reads");
        let repeat = &scenario.steps[3];
        assert_eq!(repeat_times(&repeat.value).expect("times"), 2);
        let inner = repeat_steps(&repeat.value).expect("steps");
        assert_eq!(
            inner.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
            vec!["press", "delay"]
        );
        assert_eq!(inner[1].value, Yaml::Str("500ms".to_owned()));
    }

    /// A scenario file is read over MCP and HTTP, so an error names the key and the line, never the
    /// value.
    #[test]
    fn a_schema_error_names_the_key_never_the_value() {
        let head = "schema: passportsim/scenario@1\nname: x\nsteps:\n";
        let cases = [
            "schema: SECRET-1\nname: x\nsteps: []\n".to_owned(),
            format!("{head}  - ui.expect: {{contains: [{{bg: \"SECRET-2\"}}]}}\n"),
            format!("{head}  - ui.expect: {{contains: [{{state: SECRET-3}}]}}\n"),
            format!("{head}  - ui.expect: {{contains: [{{text_re: \"x^SECRET-4\"}}]}}\n"),
            format!("{head}  - ui.expect:\n      tree: |\n        SECRET-5\n"),
            format!("{head}  - ui.expect:\n      tree: |\n        - obj SECRET-6\n"),
            format!("{head}  - wait: {{serial: \"a\\qSECRET-7\"}}\n"),
            format!("{head}  - wait: {{serial: x}} SECRET-8\n"),
        ];
        for text in &cases {
            let error = Scenario::parse(text)
                .and_then(|scenario| scenario.check_steps(&["wait"]))
                .expect_err(text);
            assert!(!error.to_string().contains("SECRET"), "{text}\n -> {error}");
        }
    }

    /// Hints files write colours as `"#ffd928"`.
    #[test]
    fn a_comment_stops_at_a_quoted_hash() {
        let value = Yaml::parse("a: \"#ffd928\" # the selected background\nb: 1").expect("reads");
        assert_eq!(value.get("a"), Some(&Yaml::Str("#ffd928".to_owned())));
        assert_eq!(value.get("b"), Some(&Yaml::Int(1)));
    }

    #[test]
    fn a_block_literal_keeps_its_indentation_and_line_breaks() {
        let value =
            Yaml::parse("tree: |\n  - obj\n    - label \"Display\"\nafter: 1").expect("reads");
        assert_eq!(
            value.get("tree").and_then(Yaml::as_str).as_deref(),
            Some("- obj\n  - label \"Display\"\n")
        );
        assert_eq!(value.get("after"), Some(&Yaml::Int(1)));
    }

    /// So a shell line a step sends to the guest keeps its tail.
    #[test]
    fn a_block_literal_keeps_a_hash_and_an_inner_blank_line() {
        let value =
            Yaml::parse("script: |\n  echo hello  # not a comment\n\n  echo bye\nafter: 1\n")
                .expect("reads");
        assert_eq!(
            value.get("script").and_then(Yaml::as_str).as_deref(),
            Some("echo hello  # not a comment\n\necho bye\n")
        );
        assert_eq!(value.get("after"), Some(&Yaml::Int(1)));
    }

    #[test]
    fn trailing_blank_lines_of_a_literal_do_not_reach_the_value() {
        let value = Yaml::parse("script: |-\n  one\n\n\nafter: 2\n").expect("reads");
        assert_eq!(
            value.get("script").and_then(Yaml::as_str).as_deref(),
            Some("one")
        );
        assert_eq!(value.get("after"), Some(&Yaml::Int(2)));
    }

    #[test]
    fn a_hash_outside_a_literal_is_still_a_comment() {
        let value = Yaml::parse("a: 1  # gone\nb: 2\n").expect("reads");
        assert_eq!(value.get("a"), Some(&Yaml::Int(1)));
        assert_eq!(value.get("b"), Some(&Yaml::Int(2)));
    }

    #[test]
    fn flow_collections_nest() {
        let value = Yaml::parse("a: {b: [1, 2, {c: d}], e: null}").expect("reads");
        assert_eq!(
            value.get("a").and_then(|a| a.get("b")).cloned(),
            Some(Yaml::Seq(vec![
                Yaml::Int(1),
                Yaml::Int(2),
                Yaml::Map(vec![("c".to_owned(), Yaml::Str("d".to_owned()))]),
            ]))
        );
        assert_eq!(value.get("a").and_then(|a| a.get("e")), Some(&Yaml::Null));
    }

    #[test]
    fn everything_outside_the_subset_is_refused_with_its_line() {
        for (text, line, needle) in [
            ("a: &anchor 1", 1, "anchors"),
            ("a: *alias", 1, "anchors"),
            ("a: !!str 1", 1, "tags"),
            ("a: 1\n---\nb: 2", 2, "one document"),
            ("a:\n\tb: 1", 2, "tab"),
            ("a: {b: 1", 1, "not closed"),
            ("a: [1, 2", 1, "not closed"),
            ("a: \"unclosed", 1, "not closed"),
            ("a: 1\na: 2", 2, "twice"),
            ("a: {b: 1, b: 2}", 1, "twice"),
            ("a: \"\\q\"", 1, "escape"),
        ] {
            let error = Yaml::parse(text).expect_err(text);
            assert_eq!(error.line, line, "{text}: {error}");
            assert!(
                error.message.contains(needle),
                "{text}: {error} should mention `{needle}`"
            );
        }
    }

    #[test]
    fn a_document_that_is_not_a_scenario_says_which_rule_it_broke() {
        for (text, needle) in [
            ("schema: other\nname: x", "must be `passportsim/scenario@1`"),
            ("name: x", "starts with `schema:"),
            ("schema: passportsim/scenario@1", "needs a `name`"),
            (
                "schema: passportsim/scenario@1\nname: x\nnonsense: 1",
                "not a scenario@1 top-level key",
            ),
            (
                "schema: passportsim/scenario@1\nname: x\nsteps: 1",
                "`steps` is a sequence",
            ),
            (
                "schema: passportsim/scenario@1\nname: x\nsteps:\n  - press: ok\n    wait: 1",
                "exactly one command key",
            ),
            (
                "schema: passportsim/scenario@1\nname: x\nsteps:\n  - name: only a name",
                "exactly one command key",
            ),
            (
                "schema: passportsim/scenario@1\nname: x\ndefaults: {nonsense: 1}",
                "not a scenario@1 default",
            ),
        ] {
            let error = Scenario::parse(text).expect_err(text);
            assert!(
                error.message.contains(needle),
                "{text}\n -> {error} should mention `{needle}`"
            );
        }
    }

    #[test]
    fn an_unknown_step_key_names_the_index_and_the_closest_valid_key() {
        let scenario =
            Scenario::parse("schema: passportsim/scenario@1\nname: x\nsteps:\n  - pres: ok\n")
                .expect("the document reads; only the key is wrong");
        let error = scenario
            .check_steps(&["press", "wait", "serial"])
            .expect_err("`pres` is not a step");
        assert!(error.message.contains("step 1"), "{error}");
        assert!(error.message.contains("did you mean `press`"), "{error}");
    }

    #[test]
    fn a_step_carries_the_source_line_it_was_written_on() {
        let text = "schema: passportsim/scenario@1\n\
                    name: x\n\
                    \n\
                    # a comment, which is not a line of the document\n\
                    steps:\n\
                    \x20 - wait: {serial: {re: ready}}\n\
                    \n\
                    \x20 - name: mistyped\n\
                    \x20   pres: ok\n";
        let scenario = Scenario::parse(text).expect("the document reads");
        assert_eq!(
            scenario
                .steps
                .iter()
                .map(|step| step.line)
                .collect::<Vec<_>>(),
            vec![6, 8],
            "each step is at the line its `- ` was written on"
        );
        let error = scenario
            .check_steps(&["press", "wait"])
            .expect_err("`pres` is not a step");
        assert_eq!(error.line, 8);
        assert!(
            error.to_string().contains("line 8"),
            "the refusal names the line: {error}"
        );
    }

    #[test]
    fn a_step_inside_a_repeat_reports_the_repeats_line() {
        let text = "schema: passportsim/scenario@1\n\
                    name: x\n\
                    steps:\n\
                    \x20 - repeat:\n\
                    \x20     times: 2\n\
                    \x20     steps:\n\
                    \x20       - pres: ok\n";
        let scenario = Scenario::parse(text).expect("the document reads");
        assert_eq!(scenario.steps[0].line, 4);
        let error = scenario
            .check_steps(&["press", "wait"])
            .expect_err("`pres` is not a step");
        assert_eq!(error.line, 4, "{error}");
    }

    #[test]
    fn a_repeat_body_is_checked_against_the_aliases() {
        let scenario = Scenario::parse(SMOKE).expect("reads");
        scenario
            .check_steps(&["wait", "press", "env"])
            .expect("`delay` is a built-in step, the other three are aliases");
        let error = scenario
            .check_steps(&["wait", "env"])
            .expect_err("`press` is inside the repeat");
        assert!(error.message.contains("press"), "{error}");
    }

    #[test]
    fn the_registry_is_where_step_aliases_come_from() {
        let aliases: Vec<&str> = crate::registry::commands()
            .iter()
            .filter_map(|spec| spec.scenario_step)
            .collect();
        assert!(
            aliases.contains(&"wait") && aliases.contains(&"press"),
            "`run` and `input` claim their aliases: {aliases:?}"
        );
        for alias in &aliases {
            assert!(
                crate::spec::scenario_step_is_valid(alias),
                "`{alias}` is not a valid step key"
            );
            assert!(
                Step::is_known(alias, &aliases),
                "`{alias}` should be a known step"
            );
        }
    }

    fn report() -> Report {
        Report {
            name: "official-menu-smoke".to_owned(),
            source: "tests/scenarios/official-menu-smoke.yaml".to_owned(),
            instance: "p1".to_owned(),
            status: RunStatus::Fail,
            caveats: Vec::new(),
            vt_us: 1_500_000,
            steps: vec![
                StepReport {
                    index: 0,
                    name: "app starts".to_owned(),
                    key: "wait".to_owned(),
                    status: StepStatus::Pass,
                    vt_us: 500_000,
                    elapsed_vt_us: 500_000,
                    error: None,
                },
                StepReport {
                    index: 1,
                    name: String::new(),
                    key: "press".to_owned(),
                    status: StepStatus::Fail,
                    vt_us: 1_500_000,
                    elapsed_vt_us: 1_000_000,
                    error: Some(serde_json::json!({
                        "code": "E_TIMEOUT",
                        "message": "no match within 5000000us of virtual time",
                    })),
                },
                StepReport {
                    index: 2,
                    name: String::new(),
                    key: "env".to_owned(),
                    status: StepStatus::Skipped,
                    vt_us: 1_500_000,
                    elapsed_vt_us: 0,
                    error: None,
                },
            ],
        }
    }

    #[test]
    fn the_report_names_the_failed_step_and_its_exit_code() {
        let report = report();
        assert_eq!(report.failed_step(), Some(1));
        let lenient = Strictness::Lenient;
        assert_eq!(exit_code(report.status, &[], lenient), 1);
        assert_eq!(exit_code(RunStatus::Pass, &[], lenient), 0);
        assert_eq!(exit_code(RunStatus::WallBudget, &[], lenient), 6);
        assert_eq!(exit_code(RunStatus::Error, &[], lenient), 8);
        let json = report.to_json(lenient);
        assert_eq!(json["status"], "fail");
        assert_eq!(json["failed_step"], 1);
        assert_eq!(json["steps"][1]["error"]["code"], "E_TIMEOUT");
    }

    /// Both numbers come from [`crate::receipt::exit_code`], so this pins the delegation rather
    /// than a second copy of the table.
    #[test]
    fn pass_with_caveats_exits_10_leniently_and_7_under_strict() {
        let mut report = report();
        report.status = RunStatus::PassWithCaveats;
        report.caveats = vec![Caveat {
            kind: CaveatKind::ClassU,
            detail: "usb_serial_jtag.mem_conf".to_owned(),
        }];
        assert!(
            report.status.assertions_hold(),
            "a pass_with_caveats is a pass for `result`"
        );
        assert_eq!(
            exit_code(report.status, &report.caveats, Strictness::Lenient),
            10,
            "any other pass_with_caveats exits 10"
        );
        assert_eq!(
            exit_code(report.status, &report.caveats, Strictness::Strict),
            7,
            "under --strict a class-U caveat exits 7 UNMODELED_HW"
        );
        let json = report.to_json(Strictness::Strict);
        assert_eq!(json["status"], "pass_with_caveats");
        assert_eq!(json["exit_code"], 7);
        assert_eq!(json["caveats"][0]["kind"], "class_u");
        assert_eq!(json["caveats"][0]["detail"], "usb_serial_jtag.mem_conf");
    }

    #[test]
    fn a_non_hardware_caveat_exits_10_even_under_strict() {
        let caveats = [Caveat {
            kind: CaveatKind::TimingLint,
            detail: "spi.flash_read".to_owned(),
        }];
        assert_eq!(
            exit_code(RunStatus::PassWithCaveats, &caveats, Strictness::Strict),
            10
        );
    }

    /// A lenient `pass_with_caveats` must not outrank a `fail` just because 10 is larger than 1.
    #[test]
    fn the_batch_status_is_ordered_by_severity_not_by_exit_code() {
        let mut all = [
            RunStatus::Pass,
            RunStatus::PassWithCaveats,
            RunStatus::Fail,
            RunStatus::WallBudget,
            RunStatus::Error,
        ];
        all.sort_by_key(|status| status.severity());
        assert_eq!(
            all,
            [
                RunStatus::Pass,
                RunStatus::PassWithCaveats,
                RunStatus::Fail,
                RunStatus::WallBudget,
                RunStatus::Error,
            ]
        );
        assert!(
            RunStatus::Fail.severity() > RunStatus::PassWithCaveats.severity(),
            "a failure outranks a caveat, though 1 < 10 as exit codes"
        );
    }

    #[test]
    fn the_text_rendering_shows_the_failures_and_the_last_five_steps() {
        let mut report = report();
        report.steps = (0..200)
            .map(|index| StepReport {
                index,
                name: String::new(),
                key: "delay".to_owned(),
                status: if index == 3 {
                    StepStatus::Fail
                } else {
                    StepStatus::Pass
                },
                vt_us: index as u64 * 1000,
                elapsed_vt_us: 1000,
                error: None,
            })
            .collect();
        let text = report.to_text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            1 + 1 + TEXT_TAIL_STEPS + 1,
            "summary, the failure, the tail and the elision note:\n{text}"
        );
        assert!(lines[1].contains("4. delay fail"), "{text}");
        assert!(
            lines
                .last()
                .expect("a line")
                .contains("194 step(s) not shown"),
            "{text}"
        );
    }

    #[test]
    fn junit_carries_one_suite_per_scenario_and_one_case_per_step() {
        let xml = junit(&[report()]);
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(
            xml.contains("<testsuites name=\"passportsim\" tests=\"3\" failures=\"1\" errors=\"0\" skipped=\"1\">"),
            "{xml}"
        );
        assert!(
            xml.contains("<testsuite name=\"official-menu-smoke\" tests=\"3\""),
            "{xml}"
        );
        assert!(
            xml.contains(
                "<property name=\"source\" value=\"tests/scenarios/official-menu-smoke.yaml\"/>"
            ),
            "{xml}"
        );
        assert!(xml.contains("<failure type=\"E_TIMEOUT\""), "{xml}");
        assert!(xml.contains("<skipped/>"), "{xml}");
        assert!(xml.trim_end().ends_with("</testsuites>"), "{xml}");
    }

    #[test]
    fn junit_times_are_virtual_seconds_and_are_byte_identical_across_runs() {
        let xml = junit(&[report()]);
        assert!(xml.contains("time=\"1.500000\""), "{xml}");
        assert!(xml.contains("time=\"0.500000\""), "{xml}");
        assert_eq!(xml, junit(&[report()]));
        assert_eq!(seconds(0), "0.000000");
        assert_eq!(seconds(1), "0.000001");
    }

    /// A file that vanished from a CI report is worse than one that failed in it.
    #[test]
    fn an_unreadable_scenario_becomes_one_errored_case() {
        let error = Scenario::parse("name: x").expect_err("no schema");
        let report = Report::unreadable("tests/scenarios/broken.yaml", &error);
        assert_eq!(report.status, RunStatus::Error);
        let xml = junit(&[report]);
        assert!(xml.contains("errors=\"1\""), "{xml}");
        assert!(xml.contains("<error type=\"E_USAGE\""), "{xml}");
    }

    #[test]
    fn junit_escapes_what_xml_cannot_carry_raw() {
        let mut report = report();
        report.name = "a<b & c\u{1}".to_owned();
        let xml = junit(&[report]);
        assert!(xml.contains("name=\"a&lt;b &amp; c\""), "{xml}");
        assert!(!xml.contains('\u{1}'));
    }

    #[test]
    fn a_scenario_error_becomes_a_usage_envelope() {
        let error: ApiError = Scenario::parse("a: {b: 1").expect_err("unclosed").into();
        assert_eq!(error.code, E_USAGE);
        assert!(error.message.contains("line 1"), "{}", error.message);
    }

    /// The official menu in miniature: a screen, a selected card (`#ffd928`) holding "Display", a
    /// plain card holding "Button", and a hidden label.
    fn menu() -> UiTree {
        use pemu_introspect::lvgl::{Color, UiNode};
        let node = |obj: u32, parent: u32, depth: u32, class: &str, text: Option<&str>| UiNode {
            obj,
            reference: format!("e{}", obj & 0xff),
            class: class.to_owned(),
            parent,
            depth,
            text: text.map(str::to_owned),
            x: 10,
            y: 10,
            w: 50,
            h: 20,
            ..UiNode::default()
        };
        let mut card1 = node(2, 1, 1, "obj", None);
        card1.bg = Some(Color {
            r: 0xff,
            g: 0xd9,
            b: 0x28,
        });
        let mut hidden = node(6, 1, 1, "label", Some("secret"));
        hidden.flags = pemu_introspect::lvgl::FLAG_HIDDEN;
        let mut screen = node(1, 0, 0, "obj", None);
        screen.w = 240;
        screen.h = 320;
        UiTree {
            rev: 3,
            nodes: vec![
                screen,
                card1,
                node(3, 2, 2, "label", Some("Display")),
                node(4, 1, 1, "obj", None),
                node(5, 4, 2, "label", Some("Button")),
                hidden,
            ],
            hor_res: 240,
            ver_res: 320,
            ..UiTree::default()
        }
    }

    fn expect(text: &str) -> Result<UiExpect, ScenarioError> {
        UiExpect::parse(&Yaml::parse(text).expect("the YAML reads"))
    }

    #[test]
    fn ui_expect_contains_absent_and_count_hold_on_the_menu() {
        let tree = menu();
        let ok = expect(
            "contains: [{text: Display, within: {bg: \"#FFD928\"}}, {text_re: \"^But\", class: lv_label}]\n\
             absent: [{text: Guru}, {text: secret}]\n\
             count: {query: {class: label}, eq: 2}\n",
        )
        .expect("reads");
        assert_eq!(ok.failures(&tree), Vec::<String>::new());

        let broken = expect(
            "contains: [{text: Button, within: {bg: \"#ffd928\"}}]\n\
             absent: [{class: label}]\n\
             count: {query: {text: secret, visible: false}, gte: 2}\n",
        )
        .expect("reads");
        let failures = broken.failures(&tree);
        assert_eq!(failures.len(), 3, "{failures:?}");
        assert!(failures[0].contains("contains[0]") && failures[0].contains("#ffd928"));
        assert!(failures[1].contains("absent[0]") && failures[1].contains("2 node(s)"));
        assert!(failures[2].starts_with("count:"), "{}", failures[2]);
    }

    #[test]
    fn ui_expect_tree_matches_nesting_and_order_with_extra_nodes_allowed() {
        let tree = menu();
        let holds =
            expect("tree: |\n  - obj\n    - label \"Display\"\n  - obj\n    - label \"Button\"\n")
                .expect("reads");
        assert!(
            holds.failures(&tree).is_empty(),
            "{:?}",
            holds.failures(&tree)
        );
        let deep = expect("tree: |\n  - obj\n    - label \"Button\"\n").expect("reads");
        assert!(
            deep.failures(&tree).is_empty(),
            "a descendant, not only a child"
        );
        let order =
            expect("tree: |\n  - label \"Button\"\n  - label \"Display\"\n").expect("reads");
        assert_eq!(order.failures(&tree).len(), 1, "order is kept");
        let nesting =
            expect("tree: |\n  - label \"Display\"\n    - label \"Button\"\n").expect("reads");
        assert_eq!(nesting.failures(&tree).len(), 1, "nesting is kept");
    }

    #[test]
    fn ui_expect_accepts_state_hidden_and_count_refuses_unknown_or_conflicting_keys() {
        let tree = menu();
        let hidden =
            expect("count: {query: {state: hidden, visible: false}, eq: 1}\n").expect("reads");
        assert_eq!(hidden.failures(&tree), Vec::<String>::new());
        for (text, needle) in [
            (
                "count: {query: {class: label}, eq: 2, lte: 3}\n",
                "exactly one",
            ),
            (
                "count: {query: {class: label}, eq: 2, max: 3}\n",
                "count.max",
            ),
            ("count: {query: {class: label}}\n", "exactly one"),
            ("count: {query: {class: label}, gte: -1}\n", "whole number"),
        ] {
            let error = expect(text).expect_err(text);
            assert!(error.message.contains(needle), "{text}: {}", error.message);
        }
    }

    #[test]
    fn ui_expect_refuses_what_needs_hints_and_what_asserts_nothing() {
        for (text, needle) in [
            ("selected: {text: Display}", "setup.hints"),
            ("contains: [{role: card}]", "setup.hints"),
            ("contains: [{state: selected}]", "setup.hints"),
            ("contains: [{state: sleepy}]", "not an LVGL state"),
            ("contains: [{colour: red}]", "not a UI query field"),
            ("contains: [{bg: yellow}]", "#rrggbb"),
            ("{}", "asserts nothing"),
        ] {
            let error = expect(text).expect_err(text);
            assert!(error.message.contains(needle), "{text}: {}", error.message);
        }
        let scenario = Scenario::parse(
            "schema: passportsim/scenario@1\nname: bad\nsteps:\n  - ui.expect: {selected: {text: A}}\n",
        )
        .expect("the document reads");
        let error = scenario
            .check_steps(&[])
            .expect_err("validated before a step runs");
        assert_eq!(error.line, 4, "{}", error.message);
    }

    fn step_expect(text: &str) -> Yaml {
        let scenario = Scenario::parse(&format!(
            "schema: passportsim/scenario@1\nname: e\nsteps:\n  - ble.scan: {{}}\n    expect: {text}\n"
        ))
        .expect("the document reads");
        scenario.steps[0]
            .expect
            .clone()
            .expect("the step carries `expect`")
    }

    #[test]
    fn expect_holds_a_partial_answer_and_names_every_field_that_does_not() {
        let answer = serde_json::json!({
            "connected": true,
            "mtu": 247,
            "addr": "02:00:00:00:00:01",
            "found": [
                {"name": null, "pdu": "SCAN_RSP", "connectable": false},
                {"name": "Passport Keys", "pdu": "ADV_IND", "connectable": true},
            ],
            "tree": [{"uuid": "S", "children": [{"uuid": "A"}, {"uuid": "B"}]}],
        });
        // Unnamed fields are allowed, and a list item holds for any element.
        for held in [
            "{connected: true, mtu: 247}",
            "{found: [{name: Passport Keys, connectable: true}]}",
            "{tree: [{uuid: S, children: [{uuid: B}, {uuid: A}]}]}",
            "{found: [{name: null}]}",
        ] {
            assert_eq!(
                answer_failures(&step_expect(held), &answer),
                Vec::<String>::new(),
                "{held}"
            );
        }
        for (text, needle) in [
            ("{mtu: 23}", "`mtu`: expected 23, saw 247"),
            ("{mtu: \"247\"}", "`mtu`: expected \"247\", saw 247"),
            ("{connected: false}", "`connected`: expected false"),
            ("{rssi: -40}", "`rssi`: the answer has no such field"),
            (
                "{found: [{name: Other}]}",
                "`found`: no element holds {\"name\":\"Other\"}",
            ),
            (
                "{tree: [{uuid: S, children: [{uuid: C}]}]}",
                "`tree`: no element holds",
            ),
            ("{mtu: {value: 1}}", "`mtu`: expected an object, saw 247"),
            (
                "{connected: [true]}",
                "`connected`: expected a list, saw true",
            ),
            ("{addr: {a: 1}}", "`addr`: expected an object"),
        ] {
            let failures = answer_failures(&step_expect(text), &answer);
            assert_eq!(failures.len(), 1, "{text}: {failures:?}");
            assert!(failures[0].contains(needle), "{text}: {}", failures[0]);
        }
        let failures = answer_failures(&step_expect("{mtu: 23, connected: false}"), &answer);
        assert_eq!(failures.len(), 2, "{failures:?}");
    }

    #[test]
    fn a_long_answer_is_quoted_cut_short() {
        let long = serde_json::json!({"found": ["x".repeat(1_000)]});
        let failures = answer_failures(&step_expect("{found: [y]}"), &long);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].ends_with("..."), "{}", failures[0]);
        assert!(failures[0].len() < 400, "{}", failures[0]);
    }

    #[test]
    fn expect_is_a_mapping_on_a_command_step_only() {
        let not_a_map = Scenario::parse(
            "schema: passportsim/scenario@1\nname: e\nsteps:\n  - ble.scan: {}\n    expect: 247\n",
        )
        .expect_err("a scalar asserts no field");
        assert!(
            not_a_map.message.contains("`expect` is a mapping"),
            "{}",
            not_a_map.message
        );
        for key in BUILT_IN_STEPS {
            let text = format!(
                "schema: passportsim/scenario@1\nname: e\nsteps:\n  - {key}: {{}}\n    expect: {{a: 1}}\n"
            );
            let error = Scenario::parse(&text).expect_err(key);
            assert!(
                error.message.contains("is not a command step"),
                "{key}: {}",
                error.message
            );
        }
        assert!(STEP_FIELDS.contains(&"expect"));
    }
}
