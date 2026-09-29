//! `cargo xtask layering`: the crate layering rules of `docs/ARCHITECTURE.md` (Layering).
//!
//! Rules, each reported as one line `<rule> <crate> <file>:<line>: <detail>` followed by counts;
//! any violation makes the command fail:
//!
//! - `crate-table`: every workspace crate has a row in the crate table (`layering/policy.rs`).
//! - `crate-edge`: every workspace dependency (normal, dev and build, target-specific tables
//!   included) is an edge of the transitive closure of the layering graph plus
//!   `policy::EXTRA_EDGES`.
//! - `third-party`: every other dependency is allowed by `policy::CRATES`.
//! - `core-std-api`: core crates (layering rule 1) do not name `std::time`, `std::thread`,
//!   `std::fs`, `std::env`, `std::net`, `std::process`, a `std::*` glob or alias, `HashMap`,
//!   `HashSet`, or `std::collections::{hash_map, hash_set}` in non-test code (test code is
//!   recognized as `layering/source.rs` and `layering/walk.rs` say). In `pemu-planner`, code
//!   behind `feature = "device"` is exempt too.
//! - `serial-device` (layering rule 5): outside test code and outside `pemu-planner`'s `device`
//!   feature, no string literal names a serial device, and no crate depends on a serial-port
//!   crate unless it is an optional `pemu-planner` dependency enabled only by that feature.
//! - `clippy-config`: every core crate has a `clippy.toml` with the disallowed types and methods
//!   of rule 1, and the root `clippy.toml` lists none of them (`layering/clippy.rs`).
//!
//! Usage: `cargo xtask layering [--root <dir>]`; the default root is the workspace containing
//! this `xtask` crate.
mod check;
mod clippy;
mod lexer;
mod manifest;
mod policy;
mod source;
mod walk;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub use check::check;

/// A layering rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// Crate missing from the crate table.
    CrateTable,
    /// Workspace dependency outside the layering graph.
    CrateEdge,
    /// Third-party dependency outside the "May use" column.
    ThirdParty,
    /// Forbidden std API in a core crate.
    CoreStdApi,
    /// Serial device reference outside the planner's device feature.
    SerialDevice,
    /// Missing or incomplete clippy configuration.
    ClippyConfig,
}

impl Rule {
    /// All rules, in report order.
    pub const ALL: [Rule; 6] = [
        Rule::CrateTable,
        Rule::CrateEdge,
        Rule::ThirdParty,
        Rule::CoreStdApi,
        Rule::SerialDevice,
        Rule::ClippyConfig,
    ];

    /// Name printed in reports.
    pub fn name(self) -> &'static str {
        match self {
            Rule::CrateTable => "crate-table",
            Rule::CrateEdge => "crate-edge",
            Rule::ThirdParty => "third-party",
            Rule::CoreStdApi => "core-std-api",
            Rule::SerialDevice => "serial-device",
            Rule::ClippyConfig => "clippy-config",
        }
    }
}

/// One violation.
#[derive(Clone, Debug)]
pub struct Violation {
    /// Rule broken.
    pub rule: Rule,
    /// Package name of the crate (`workspace` for the root `clippy.toml`).
    pub krate: String,
    /// Repository-relative file.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// Explanation.
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Violation {
            rule,
            krate,
            file,
            line,
            detail,
        } = self;
        write!(f, "{} {krate} {file}:{line}: {detail}", rule.name())
    }
}

/// One workspace crate, as text.
#[derive(Clone, Debug, Default)]
pub struct CrateInput {
    /// Repository-relative crate directory, for example `crates/pemu-core`.
    pub dir: String,
    /// `Cargo.toml` text.
    pub manifest: String,
    /// `.rs` files, keyed by crate-relative `/`-separated path.
    pub files: BTreeMap<String, String>,
    /// `clippy.toml` in the crate directory, if any.
    pub clippy_toml: Option<String>,
}

/// The workspace, as text.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceInput {
    /// Root `Cargo.toml` text.
    pub root_manifest: String,
    /// Root `clippy.toml` text, if any.
    pub root_clippy_toml: Option<String>,
    /// Member crates.
    pub crates: Vec<CrateInput>,
}

/// Result of a check.
#[derive(Debug)]
pub struct Report {
    /// Violations, sorted by rule, file and line.
    pub violations: Vec<Violation>,
    /// Crates checked.
    pub crates: usize,
    /// Source files scanned.
    pub files: usize,
}

impl Report {
    /// `rule: count` for every rule.
    pub fn counts(&self) -> String {
        Rule::ALL
            .iter()
            .map(|r| {
                let n = self.violations.iter().filter(|v| v.rule == *r).count();
                format!("{} {n}", r.name())
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Entry point of `cargo xtask layering`.
pub fn run(args: &[String]) -> Result<(), String> {
    let root = match args {
        [] => crate::util::workspace_root(),
        [flag, dir] if flag == "--root" => PathBuf::from(dir),
        _ => return Err("usage: cargo xtask layering [--root <dir>]".to_string()),
    };
    let report = check(&load(&root)?)?;
    for v in &report.violations {
        println!("{v}");
    }
    let n = report.violations.len();
    println!(
        "layering: {} crates, {} files, {n} violations ({})",
        report.crates,
        report.files,
        report.counts()
    );
    if n == 0 {
        Ok(())
    } else {
        Err(format!("{n} layering violations"))
    }
}

/// Reads the workspace under `root`: the root manifest and `clippy.toml`, and for every
/// `workspace.members` entry its manifest, `clippy.toml` and `.rs` files.
fn load(root: &Path) -> Result<WorkspaceInput, String> {
    let root_manifest = read(&root.join("Cargo.toml"))?;
    let table: toml::Table = root_manifest
        .parse()
        .map_err(|e| format!("root Cargo.toml: invalid TOML: {e}"))?;
    let members = table
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .ok_or("root Cargo.toml has no workspace.members")?;
    let mut crates = Vec::new();
    for member in members {
        let dir = member
            .as_str()
            .ok_or("workspace.members entry is not a string")?;
        if dir.contains(['*', '?', '[']) {
            return Err(format!("workspace member glob `{dir}` is not supported"));
        }
        let abs = root.join(dir);
        let mut files = BTreeMap::new();
        collect_rs(&abs, "", &mut files)?;
        crates.push(CrateInput {
            dir: dir.trim_end_matches('/').to_string(),
            manifest: read(&abs.join("Cargo.toml"))?,
            files,
            clippy_toml: read_optional(&abs.join("clippy.toml"))?,
        });
    }
    Ok(WorkspaceInput {
        root_manifest,
        root_clippy_toml: read_optional(&root.join("clippy.toml"))?,
        crates,
    })
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Collects the `.rs` files below `dir` into `out`, keyed by `/`-separated paths relative to the
/// crate. Skips hidden, `target` and `node_modules` directories, nested crates and symlinks.
fn collect_rs(dir: &Path, rel: &str, out: &mut BTreeMap<String, String>) -> Result<(), String> {
    let listing =
        std::fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    let mut entries = listing
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let rel_path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if kind.is_dir() {
            let skip = name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || path.join("Cargo.toml").exists();
            if !skip {
                collect_rs(&path, &rel_path, out)?;
            }
        } else if kind.is_file() && name.ends_with(".rs") {
            out.insert(rel_path, read(&path)?);
        }
    }
    Ok(())
}
