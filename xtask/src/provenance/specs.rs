//! Rule `spec-provenance`: every `specs/blocks/*.toml` header and row records its sources in a
//! non-empty `provenance` field (`specs/README.md`).
//!
//! `xtask codegen` also rejects a row without `provenance`, but only for the tables it reads; a row
//! in a table codegen does not use yet is still a behavior claim that needs its source, so this
//! rule checks the header and all four row arrays independently of codegen.
//!
//! The value may not name a path of a GPL or unlicensed source (`CONTRIBUTING.md#clean-room`).
//! `UNVERIFIED` is a legal value.
//!
//! The missing field and the restricted value are both `spec-provenance` findings, so a reviewer
//! reads one count. The path patterns are shared with `provenance/notes.rs`.

use super::{Finding, Rule, SourceFile, notes};

/// The four row arrays of a block file (`specs/README.md`).
pub const ARRAYS: &[&str] = &["reset_domains", "wait", "overrides", "stable_read"];

/// The field every header and row carries.
const FIELD: &str = "provenance";

/// Checks the block files, in the order given.
pub fn scan(files: &[SourceFile]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for file in files {
        let table: toml::Table = match file.text.parse() {
            Ok(table) => table,
            Err(err) => {
                findings.push(Finding {
                    rule: Rule::SpecProvenance,
                    file: file.path.clone(),
                    line: 1,
                    detail: format!("invalid TOML: {}", one_line(&err.to_string())),
                });
                continue;
            }
        };
        let header = header_line(&file.text);
        check(file, "file header", table.get(FIELD), header, &mut findings);
        for array in ARRAYS {
            let rows = table.get(*array).and_then(toml::Value::as_array);
            let lines = row_lines(&file.text, array);
            for (index, row) in rows.into_iter().flatten().enumerate() {
                let line = lines.get(index).copied().unwrap_or(1);
                let where_ = format!("`[[{array}]]` row {}{}", index + 1, row_id(row));
                check(file, &where_, row.get(FIELD), line, &mut findings);
            }
        }
    }
    findings
}

/// The two checks on one `provenance` field: it is present and non-empty, and its content names
/// no restricted source path. `where_` names the header or the row in the finding,
/// and `from` is the line the field's owner starts on.
fn check(
    file: &SourceFile,
    where_: &str,
    value: Option<&toml::Value>,
    from: usize,
    findings: &mut Vec<Finding>,
) {
    if !filled(value) {
        findings.push(Finding {
            rule: Rule::SpecProvenance,
            file: file.path.clone(),
            line: from,
            detail: format!("{where_} has no `{FIELD}` field (`specs/README.md`)"),
        });
        return;
    }
    let text = value.and_then(toml::Value::as_str).unwrap_or_default();
    for (matched, reason) in restricted(text) {
        findings.push(Finding {
            rule: Rule::SpecProvenance,
            file: file.path.clone(),
            line: value_line(&file.text, from, &matched),
            detail: format!(
                "`{FIELD}` of {where_} {reason} (`specs/README.md`, `CONTRIBUTING.md#clean-room`)"
            ),
        });
    }
}

/// The restricted source path patterns of `provenance/notes.rs` that `value` names, each with the
/// matched text and the reason printed in a finding.
fn restricted(value: &str) -> Vec<(String, String)> {
    notes::PATTERNS
        .iter()
        .filter(|(pattern, _)| value.contains(pattern))
        .map(|(pattern, reason)| {
            (
                (*pattern).to_string(),
                format!("names `{pattern}`, {reason}"),
            )
        })
        .collect()
}

/// The line of the `provenance` value that carries `matched`, searched from the header or row line
/// `from` so a multi-line table still reports the offending line; `from` when it is not found.
fn value_line(text: &str, from: usize, matched: &str) -> usize {
    text.lines()
        .enumerate()
        .skip(from.saturating_sub(1))
        .find(|(_, line)| line.contains(matched))
        .map(|(index, _)| index + 1)
        .unwrap_or(from)
}

/// True when the value is a string with non-blank content.
fn filled(value: Option<&toml::Value>) -> bool {
    value
        .and_then(toml::Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

/// The line of the first header key, so the finding points into the header rather than at the
/// comment above it; line 1 when the file has no key before its first row array.
fn header_line(text: &str) -> usize {
    text.lines()
        .enumerate()
        .find(|(_, line)| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#') && !trimmed.starts_with('[') && trimmed.contains('=')
        })
        .map(|(index, _)| index + 1)
        .unwrap_or(1)
}

/// 1-based lines of the `[[<array>]]` headers, in file order, so row `n` of the parsed array
/// reports the line its header sits on.
fn row_lines(text: &str, array: &str) -> Vec<usize> {
    let header = format!("[[{array}]]");
    text.lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == header)
        .map(|(index, _)| index + 1)
        .collect()
}

/// ` (id)`, ` (register)` or ` (command)` of a row, for the finding line; empty when the row
/// names none of them.
fn row_id(row: &toml::Value) -> String {
    ["id", "register", "registers", "command"]
        .iter()
        .filter_map(|key| row.get(*key).and_then(toml::Value::as_str))
        .next()
        .map(|id| format!(" (`{id}`)"))
        .unwrap_or_default()
}

/// First line of a message, for a one-line finding.
fn one_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}
