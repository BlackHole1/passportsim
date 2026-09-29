//! Corpus check of `xtask ci t1`.
//!
//! Reads `~/.config/passportsim/corpus.toml` (one table per corpus id, file paths plus an
//! inline `sha256` table keyed like the paths) and hashes every listed file. It prints only
//! ids, file kinds and match results: never paths, hashes or contents.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

use sha2::{Digest, Sha256};

use super::model::{Status, StepResult};
use super::runner::Ctx;
use crate::hostdirs::{self, expand_home};

/// Corpus manifest below `HOME` (`docs/quickstart.md`).
pub const CORPUS_FILE: &str = ".config/passportsim/corpus.toml";

/// Result for one listed file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Match,
    Mismatch,
    /// The file cannot be read.
    Missing,
    /// The manifest lists the file without a hash.
    NoHash,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Match => "match",
            Outcome::Mismatch => "MISMATCH",
            Outcome::Missing => "missing",
            Outcome::NoHash => "no hash listed",
        }
    }
}

/// One listed file: corpus id, file kind (`bin`, `elf`, ...) and outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub kind: String,
    pub outcome: Outcome,
}

/// Checks every file of a `corpus.toml` text. `hash` returns the hex SHA-256 of a file, or
/// `None` when it cannot be read.
pub fn check(
    text: &str,
    home: &Path,
    hash: &dyn Fn(&Path) -> Option<String>,
) -> Result<Vec<Entry>, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|_| "corpus.toml does not parse as TOML".to_string())?;
    let mut entries = Vec::new();
    for (id, value) in &table {
        let Some(files) = value.as_table() else {
            continue;
        };
        let hashes = files.get("sha256").and_then(toml::Value::as_table);
        for (kind, path) in files {
            let Some(path) = path.as_str().filter(|_| kind != "sha256") else {
                continue;
            };
            let expected = hashes
                .and_then(|hashes| hashes.get(kind))
                .and_then(toml::Value::as_str);
            let outcome = match expected {
                None => Outcome::NoHash,
                Some(expected) => match hash(&expand_home(path, home)) {
                    None => Outcome::Missing,
                    Some(actual) if actual.eq_ignore_ascii_case(expected) => Outcome::Match,
                    Some(_) => Outcome::Mismatch,
                },
            };
            entries.push(Entry {
                id: id.clone(),
                kind: kind.clone(),
                outcome,
            });
        }
    }
    Ok(entries)
}

/// Lowercase hex SHA-256 of a file, read in chunks.
pub fn sha256_file(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// The `corpus` step of T1.
pub fn step(ctx: &mut Ctx) {
    let started = Instant::now();
    let result = hostdirs::home().and_then(|home| {
        let text = match std::fs::read_to_string(home.join(CORPUS_FILE)) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "~/{CORPUS_FILE} not found (see docs/quickstart.md)"
                ));
            }
            Err(err) => return Err(format!("cannot read ~/{CORPUS_FILE}: {}", err.kind())),
        };
        check(&text, &home, &sha256_file)
    });
    let (status, reason) = match result {
        Err(reason) => (Status::Fail, reason),
        Ok(entries) => {
            for entry in &entries {
                println!(
                    "[ci {}] corpus {} {}: {}",
                    ctx.tier,
                    entry.id,
                    entry.kind,
                    entry.outcome.as_str()
                );
            }
            let bad = entries
                .iter()
                .filter(|e| e.outcome != Outcome::Match)
                .count();
            match (entries.len(), bad) {
                (0, _) => (Status::Fail, "corpus.toml lists no files".to_string()),
                (n, 0) => (Status::Pass, format!("{n} files match")),
                (n, bad) => (Status::Fail, format!("{bad} of {n} files do not match")),
            }
        }
    };
    ctx.record(StepResult {
        duration: started.elapsed(),
        ..StepResult::instant("corpus", status, reason)
    });
}
