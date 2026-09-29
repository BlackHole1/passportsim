//! Rule `notes-path`: a `specs/notes/` file carries no source path of a restricted reference.
//!
//! The notes state behavior in prose, with no source path, file name, line number or code shape of
//! a restricted reference. The patterns:
//!
//! - `.rs:` a Rust file with a line number, the shape of an esp32sim source citation;
//! - `hw/` a QEMU device model directory;
//! - `esp32sim/` and `esp32sim-` a path inside an esp32sim tree or one of the `esp32sim-*`
//!   reference directories.
//!
//! The bare word `esp32sim` is allowed: a note may say that a fact was learned from an esp32sim
//! run, which is a black-box observation. Only the path forms are findings.

use super::{Finding, Rule, SourceFile};

/// The forbidden patterns, each with the reason printed in a finding.
pub const PATTERNS: &[(&str, &str)] = &[
    (".rs:", "a Rust source file with a line number"),
    ("hw/", "a QEMU device model directory"),
    ("esp32sim/", "a path inside an esp32sim tree"),
    ("esp32sim-", "an `esp32sim-*` reference directory"),
];

/// Checks the `specs/notes/` files, in the order given.
pub fn scan(files: &[SourceFile]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for file in files {
        for (number, line) in file.text.lines().enumerate() {
            for (pattern, reason) in PATTERNS {
                if line.contains(pattern) {
                    findings.push(Finding {
                        rule: Rule::NotesPath,
                        file: file.path.clone(),
                        line: number + 1,
                        detail: format!(
                            "`{pattern}` names {reason}; a note states behavior only \
                             (`CONTRIBUTING.md#clean-room`)"
                        ),
                    });
                }
            }
        }
    }
    findings
}
