//! `cargo xtask provenance`: text checks of the clean-room rules (`CONTRIBUTING.md#clean-room`).
//!
//! - `model-no-citation` (`provenance/models.rs`): a model file header cites at least one public
//!   source or says `UNVERIFIED`.
//! - `notes-path` (`provenance/notes.rs`): no file under `specs/notes/` carries a source path of a
//!   restricted reference.
//! - `spec-provenance` (`provenance/specs.rs`): every `specs/blocks/*.toml` header and row has a
//!   non-empty `provenance` field that names no restricted source path.
//!
//! Each finding prints as `<rule> <file>:<line>: <detail>`, followed by counts; any finding fails
//! the command. It reads text, so it catches a citation, not a transcription: judging that code
//! does not mirror another emulator's structure stays with the reviewer.

mod models;
mod notes;
mod specs;

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};

/// Directory of the behavior notes.
const NOTES: &str = "specs/notes";
/// Directory of the per-block spec files.
const BLOCKS: &str = "specs/blocks";

/// A provenance rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// A model file header cites no permitted source.
    ModelNoCitation,
    /// A `specs/notes/` file carries a restricted source path pattern.
    NotesPath,
    /// A `specs/blocks/*.toml` header or row has no `provenance` field.
    SpecProvenance,
}

impl Rule {
    /// All rules, in report order.
    pub const ALL: [Rule; 3] = [Rule::ModelNoCitation, Rule::NotesPath, Rule::SpecProvenance];

    /// Name printed in reports.
    pub fn name(self) -> &'static str {
        match self {
            Rule::ModelNoCitation => "model-no-citation",
            Rule::NotesPath => "notes-path",
            Rule::SpecProvenance => "spec-provenance",
        }
    }
}

/// One finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Rule broken.
    pub rule: Rule,
    /// Repository-relative file.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// Explanation, including the matched citation or pattern.
    pub detail: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Finding {
            rule,
            file,
            line,
            detail,
        } = self;
        write!(f, "{} {file}:{line}: {detail}", rule.name())
    }
}

/// One input file, as text.
#[derive(Clone, Debug)]
pub struct SourceFile {
    /// Repository-relative path, `/`-separated.
    pub path: String,
    /// File text.
    pub text: String,
}

/// The repository, as text: what the rules read.
#[derive(Clone, Debug, Default)]
pub struct Input {
    /// Model files (`provenance/models.rs`).
    pub models: Vec<SourceFile>,
    /// Files under `specs/notes/`.
    pub notes: Vec<SourceFile>,
    /// Files under `specs/blocks/`.
    pub blocks: Vec<SourceFile>,
}

/// Result of a check.
#[derive(Clone, Debug)]
pub struct Report {
    /// Findings, in rule order.
    pub findings: Vec<Finding>,
    /// Model files checked.
    pub models: usize,
    /// `specs/notes/` files checked.
    pub notes: usize,
    /// Block spec files checked.
    pub blocks: usize,
}

impl Report {
    /// `rule count` for every rule.
    pub fn counts(&self) -> String {
        Rule::ALL
            .iter()
            .map(|rule| {
                let n = self.findings.iter().filter(|f| f.rule == *rule).count();
                format!("{} {n}", rule.name())
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Entry point of `cargo xtask provenance`.
pub fn run(args: &[String]) -> Result<(), String> {
    let root = match args {
        [] => crate::util::workspace_root(),
        [flag, dir] if flag == "--root" => PathBuf::from(dir),
        [flag] if flag == "-h" || flag == "--help" => {
            println!("usage: cargo xtask provenance [--root <dir>]");
            return Ok(());
        }
        _ => return Err("usage: cargo xtask provenance [--root <dir>]".to_string()),
    };
    let report = check(&load(&root)?);
    for finding in &report.findings {
        println!("{finding}");
    }
    let n = report.findings.len();
    println!(
        "provenance: {} model files, {} notes, {} block specs, {n} findings ({})",
        report.models,
        report.notes,
        report.blocks,
        report.counts()
    );
    match n {
        0 => Ok(()),
        n => Err(format!("{n} provenance findings")),
    }
}

/// Runs the rules over `input`.
pub fn check(input: &Input) -> Report {
    let mut findings = models::scan(&input.models);
    findings.extend(notes::scan(&input.notes));
    findings.extend(specs::scan(&input.blocks));
    findings.sort_by(|a, b| {
        (a.rule, &a.file, a.line, &a.detail).cmp(&(b.rule, &b.file, b.line, &b.detail))
    });
    Report {
        findings,
        models: input.models.len(),
        notes: input.notes.len(),
        blocks: input.blocks.len(),
    }
}

/// Reads the model files, `specs/notes/` and `specs/blocks/` under `root`.
fn load(root: &Path) -> Result<Input, String> {
    let mut models = Vec::new();
    for scope in models::SCOPES {
        for file in files(root, scope, ".rs")? {
            if models::is_model(&file.path, &file.text) {
                models.push(file);
            }
        }
    }
    Ok(Input {
        models,
        notes: files(root, NOTES, "")?,
        blocks: files(root, BLOCKS, ".toml")?,
    })
}

/// Files below `dir` (repository-relative) whose name ends with `suffix`, sorted by path. A
/// directory that does not exist yields nothing, so a tree without `specs/notes/` still checks.
fn files(root: &Path, dir: &str, suffix: &str) -> Result<Vec<SourceFile>, String> {
    let mut out = Vec::new();
    collect(&root.join(dir), dir, suffix, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn collect(dir: &Path, rel: &str, suffix: &str, out: &mut Vec<SourceFile>) -> Result<(), String> {
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("cannot list {}: {err}", dir.display())),
    };
    for entry in listing {
        let entry = entry.map_err(|err| format!("cannot list {}: {err}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let rel_path = format!("{rel}/{name}");
        let kind = entry
            .file_type()
            .map_err(|err| format!("cannot stat {}: {err}", path.display()))?;
        if kind.is_dir() && !name.starts_with('.') {
            collect(&path, &rel_path, suffix, out)?;
        } else if kind.is_file() && name.ends_with(suffix) && !name.starts_with('.') {
            out.push(SourceFile {
                path: rel_path,
                text: read(&path)?,
            });
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))
}
