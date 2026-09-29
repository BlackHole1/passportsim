//! A reader for the small TOML subset the oracle spec files use. `pemu-verify` may depend on no
//! third-party crate, so the spec files and a golden's `bands.toml` are read here. The subset
//! refuses anything it does not understand, so a drifting file fails loudly instead of being
//! half-read:
//!
//! - `# comment` lines and blank lines;
//! - `key = value` pairs, in the document root or in the current table;
//! - `[table]` and `[[array.of.tables]]` headers;
//! - values: a basic string `"..."` with `\\`, `\"` and `\n` escapes, a decimal or `0x`
//!   hexadecimal integer with `_` separators, `true`, `false`, and a single-line array of any
//!   of those, arrays included (`[[1, 2], [3, 4]]`).
//!
//! Every error carries the 1-based line number.

use std::collections::BTreeMap;
use std::fmt;

/// A parsed value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Str(String),
    /// A decimal or hexadecimal integer.
    Int(i64),
    Bool(bool),
    /// A single-line array.
    Array(Vec<Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(v) => Some(v),
            _ => None,
        }
    }
}

/// A table: its key-value pairs and the 1-based line its header sits on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    /// Line of the `[header]`, or 1 for the document root.
    pub line: usize,
    pub pairs: BTreeMap<String, Value>,
}

impl Table {
    pub fn str_field(&self, key: &str) -> Result<&str, Error> {
        match self.pairs.get(key) {
            Some(Value::Str(s)) => Ok(s),
            Some(_) => Err(Error::new(self.line, format!("`{key}` is not a string"))),
            None => Err(Error::new(self.line, format!("missing `{key}`"))),
        }
    }

    /// A required integer field, as `u64`.
    pub fn u64_field(&self, key: &str) -> Result<u64, Error> {
        match self.pairs.get(key) {
            Some(Value::Int(i)) if *i >= 0 => Ok(*i as u64),
            Some(Value::Int(_)) => Err(Error::new(self.line, format!("`{key}` is negative"))),
            Some(_) => Err(Error::new(self.line, format!("`{key}` is not an integer"))),
            None => Err(Error::new(self.line, format!("missing `{key}`"))),
        }
    }

    /// An optional boolean field, `fallback` when absent.
    pub fn bool_field(&self, key: &str, fallback: bool) -> Result<bool, Error> {
        match self.pairs.get(key) {
            None => Ok(fallback),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(Error::new(self.line, format!("`{key}` is not a boolean"))),
        }
    }

    pub fn opt_str(&self, key: &str) -> Result<Option<&str>, Error> {
        match self.pairs.get(key) {
            None => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s)),
            Some(_) => Err(Error::new(self.line, format!("`{key}` is not a string"))),
        }
    }
}

/// A parsed document: the root table and the arrays of tables, by header name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub root: Table,
    /// `[[name]]` entries, in file order, by header name.
    pub arrays: BTreeMap<String, Vec<Table>>,
    pub tables: BTreeMap<String, Table>,
}

impl Document {
    pub fn array(&self, name: &str) -> &[Table] {
        self.arrays.get(name).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// A parse or validation error with its 1-based line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub detail: String,
}

impl Error {
    pub fn new(line: usize, detail: impl Into<String>) -> Self {
        Error {
            line,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.detail)
    }
}

impl std::error::Error for Error {}

/// Parses the subset. CR is stripped first, so a checkout on either host parses the same.
pub fn parse(text: &str) -> Result<Document, Error> {
    let mut doc = Document::default();
    let mut current: Option<(String, bool)> = None;
    for (index, raw) in text.replace('\r', "").split('\n').enumerate() {
        let line = index + 1;
        let trimmed = strip_comment(raw).trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(header) = trimmed.strip_prefix("[[") {
            let name = header
                .strip_suffix("]]")
                .ok_or_else(|| Error::new(line, "unterminated `[[` header"))?
                .trim()
                .to_string();
            check_header(&name, line)?;
            doc.arrays.entry(name.clone()).or_default().push(Table {
                line,
                pairs: BTreeMap::new(),
            });
            current = Some((name, true));
            continue;
        }
        if let Some(header) = trimmed.strip_prefix('[') {
            let name = header
                .strip_suffix(']')
                .ok_or_else(|| Error::new(line, "unterminated `[` header"))?
                .trim()
                .to_string();
            check_header(&name, line)?;
            if doc
                .tables
                .insert(
                    name.clone(),
                    Table {
                        line,
                        pairs: BTreeMap::new(),
                    },
                )
                .is_some()
            {
                return Err(Error::new(line, format!("duplicate table `{name}`")));
            }
            current = Some((name, false));
            continue;
        }
        let (key, value) = split_pair(trimmed, line)?;
        let target = match &current {
            None => &mut doc.root,
            Some((name, true)) => doc
                .arrays
                .get_mut(name)
                .and_then(|entries| entries.last_mut())
                .expect("the array entry was pushed with its header"),
            Some((name, false)) => doc
                .tables
                .get_mut(name)
                .expect("the table was inserted with its header"),
        };
        if target.pairs.insert(key.clone(), value).is_some() {
            return Err(Error::new(line, format!("duplicate key `{key}`")));
        }
    }
    Ok(doc)
}

/// Rejects a header name outside `[A-Za-z0-9_.-]`.
fn check_header(name: &str, line: usize) -> Result<(), Error> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(Error::new(line, format!("bad header name `{name}`")));
    }
    Ok(())
}

/// Drops a `#` comment that starts outside a string.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    for (at, byte) in bytes.iter().enumerate() {
        match byte {
            _ if escaped => escaped = false,
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'#' if !in_string => return &line[..at],
            _ => {}
        }
    }
    line
}

fn split_pair(line: &str, at: usize) -> Result<(String, Value), Error> {
    let (key, rest) = line
        .split_once('=')
        .ok_or_else(|| Error::new(at, "expected `key = value`"))?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(Error::new(at, format!("bad key `{key}`")));
    }
    Ok((key.to_string(), parse_value(rest.trim(), at)?))
}

fn parse_value(text: &str, at: usize) -> Result<Value, Error> {
    if let Some(inner) = text.strip_prefix('[') {
        let inner = inner
            .strip_suffix(']')
            .ok_or_else(|| Error::new(at, "arrays must close on their own line"))?;
        let mut items = Vec::new();
        for item in split_array(inner, at)? {
            if !item.trim().is_empty() {
                items.push(parse_value(item.trim(), at)?);
            }
        }
        return Ok(Value::Array(items));
    }
    if text.starts_with('"') {
        return Ok(Value::Str(parse_string(text, at)?));
    }
    match text {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        _ => {}
    }
    parse_int(text, at).map(Value::Int)
}

/// Splits an array body on commas that sit outside a string and outside a nested array, so a
/// single-line array of arrays (`[[1, 2], [3, 4]]`) reads as two items.
fn split_array(inner: &str, at: usize) -> Result<Vec<&str>, Error> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut depth = 0usize;
    for (index, byte) in inner.bytes().enumerate() {
        match byte {
            _ if escaped => escaped = false,
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'[' if !in_string => depth += 1,
            b']' if !in_string => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| Error::new(at, "unbalanced `]` in an array"))?;
            }
            b',' if !in_string && depth == 0 => {
                parts.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if in_string {
        return Err(Error::new(at, "unterminated string in an array"));
    }
    if depth != 0 {
        return Err(Error::new(at, "unbalanced `[` in an array"));
    }
    parts.push(&inner[start..]);
    Ok(parts)
}

/// Parses a basic string with the three escapes the subset allows.
fn parse_string(text: &str, at: usize) -> Result<String, Error> {
    let body = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(|| Error::new(at, "unterminated string"))?;
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            if ch == '"' {
                return Err(Error::new(at, "unescaped `\"` inside a string"));
            }
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            other => {
                return Err(Error::new(
                    at,
                    format!("unsupported escape `\\{}`", other.unwrap_or(' ')),
                ));
            }
        }
    }
    Ok(out)
}

/// Parses a decimal or `0x` hexadecimal integer with `_` separators.
fn parse_int(text: &str, at: usize) -> Result<i64, Error> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let clean = digits.replace('_', "");
    let value = match clean
        .strip_prefix("0x")
        .or_else(|| clean.strip_prefix("0X"))
    {
        Some(hex) => i64::from_str_radix(hex, 16),
        None => clean.parse::<i64>(),
    }
    .map_err(|_| Error::new(at, format!("`{text}` is not a value of the subset")))?;
    Ok(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_root_pairs_tables_and_arrays() {
        let doc = parse(concat!(
            "schema = 1\r\n",
            "name = \"regions\"  # trailing comment\n",
            "\n",
            "[limits]\n",
            "base = 0x6000_0000\n",
            "wrap = true\n",
            "\n",
            "[[region]]\n",
            "qemu = \"misc.esp.sha\"\n",
            "block = \"sha\"\n",
            "tags = [\"a\", \"b\"]\n",
            "\n",
            "[[region]]\n",
            "qemu = \"timer.esp.timg\"\n",
            "block = \"timg0\"\n",
        ))
        .expect("parses");
        assert_eq!(doc.root.pairs["schema"], Value::Int(1));
        assert_eq!(doc.root.pairs["name"].as_str(), Some("regions"));
        assert_eq!(doc.tables["limits"].pairs["base"], Value::Int(0x6000_0000));
        assert_eq!(doc.tables["limits"].pairs["wrap"], Value::Bool(true));
        assert_eq!(doc.array("region").len(), 2);
        assert_eq!(doc.array("region")[0].str_field("block").unwrap(), "sha");
        assert_eq!(
            doc.array("region")[0].pairs["tags"].as_array().unwrap(),
            &[Value::Str("a".into()), Value::Str("b".into())]
        );
        assert_eq!(doc.array("region")[1].line, 13);
    }

    #[test]
    fn a_single_line_array_of_arrays_reads_as_nested_arrays() {
        let doc = parse("values = [[0x1, 2], [3, \"a,]\"]]\n").expect("parses");
        assert_eq!(
            doc.root.pairs["values"],
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Str("a,]".into())]),
            ])
        );
        assert!(parse("values = [[1, 2]\n").is_err());
    }

    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        let doc = parse("reason = \"regi2c reads 0xFFFFFF # not a comment\"\n").expect("parses");
        assert_eq!(
            doc.root.str_field("reason").unwrap(),
            "regi2c reads 0xFFFFFF # not a comment"
        );
    }

    #[test]
    fn refuses_what_the_subset_does_not_cover() {
        for (text, at) in [
            ("a = 1.5\n", 1),
            ("a = { b = 1 }\n", 1),
            ("[unterminated\n", 1),
            ("a = \"open\n", 1),
            ("a = 1\na = 2\n", 2),
            ("[t]\n[t]\n", 2),
            ("bare line\n", 1),
        ] {
            let err = parse(text).expect_err("must be refused");
            assert_eq!(err.line, at, "{text:?} reported {err}");
        }
    }

    #[test]
    fn field_helpers_report_the_header_line() {
        let doc = parse("[[row]]\nblock = \"sha\"\n").expect("parses");
        let row = &doc.array("row")[0];
        assert_eq!(row.str_field("block").unwrap(), "sha");
        assert_eq!(row.opt_str("reason").unwrap(), None);
        assert_eq!(row.u64_field("offset").unwrap_err().line, 1);
    }
}
