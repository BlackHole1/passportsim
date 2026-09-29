//! Hashed rules of `cargo xtask secrets-check` and the local hash file.
//!
//! The hash file `secrets-check.toml` of the config directory (`~/.config/passportsim/` on
//! macOS, `%APPDATA%\passportsim\` on Windows; owner-only, never committed, never
//! leaves the host whose device data it was derived from) holds one salted secret set built by
//! `--init`:
//!
//! ```toml
//! version = 1
//! salt = "<64 hex digits>"
//! window_lens = [4, 6, 17]
//!
//! [canary]
//! value = "pemu-secrets-canary-<32 hex digits>"
//!
//! [[member]]
//! kind = "mac"
//! len = 17
//! text_only = false
//! hash = "<64 hex digits of sha256(salt || member)>"
//! ```
//!
//! The canary is the only plaintext value: random, no identity, and hashed into the set, so a
//! staged file containing it proves that the hooks refuse a hashed member.
//! The loader recomputes `window_lens` and rejects a file where the two disagree. Reports name
//! the rule, file and offset only; parse errors never quote the file (salt and canary).

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::{ErrorKind, Write as _};
use std::path::Path;

use pemu_api::secret_set::{self, MemberKind, SaltedEntry, SaltedSet};

use super::patterns;

/// Format version written to and required from the hash file.
pub const HASH_FILE_VERSION: i64 = 1;
/// Prefix of every canary value.
pub const CANARY_PREFIX: &str = "pemu-secrets-canary-";
/// Salt length written by `--init`.
pub const SALT_LEN: usize = 32;
/// Random bytes after [`CANARY_PREFIX`], rendered as hex.
const CANARY_RANDOM_LEN: usize = 16;
/// Printed hit lines per file; the rest is summarized as a count.
const MAX_LINES_PER_FILE: usize = 20;

/// A loaded hash file. `Debug` prints counts only.
pub struct HashFile {
    /// The salted members, canary included.
    pub salted: SaltedSet,
    /// The plaintext canary of `[canary] value`.
    pub canary: String,
}

impl fmt::Debug for HashFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashFile")
            .field("entries", &self.salted.len())
            .field("window_lens", &self.salted.window_lens().len())
            .finish()
    }
}

/// Why a hash file could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// No file at the path.
    Missing,
    /// The file exists but cannot be read, parsed or trusted. The reason never quotes content.
    Unreadable(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Missing => f.write_str("the hash file is missing"),
            LoadError::Unreadable(why) => write!(f, "the hash file is unreadable: {why}"),
        }
    }
}

/// `n` bytes from the operating system random source.
pub fn random_bytes(n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| format!("cannot get random bytes: {e}"))?;
    Ok(buf)
}

/// A fresh random salt of [`SALT_LEN`] bytes.
pub fn new_salt() -> Result<Vec<u8>, String> {
    random_bytes(SALT_LEN)
}

/// A fresh random canary: [`CANARY_PREFIX`] followed by 32 lowercase hex digits.
pub fn new_canary() -> Result<String, String> {
    let random = random_bytes(CANARY_RANDOM_LEN)?;
    Ok(format!(
        "{CANARY_PREFIX}{}",
        secret_set::hex_string(&random)
    ))
}

/// Whether `value` has the canary shape written by [`new_canary`].
pub fn is_canary_shape(value: &str) -> bool {
    value.strip_prefix(CANARY_PREFIX).is_some_and(|hex| {
        hex.len() == 2 * CANARY_RANDOM_LEN && hex.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// Renders the hash file text (see module docs). Entries keep the order of `salted`, which is
/// sorted by hash, so the file reveals nothing about member order.
pub fn render(salted: &SaltedSet, canary: &str) -> String {
    let mut out = String::new();
    out.push_str(
        "# Salted hashes of the local device secret set.\n\
         # Written by `cargo xtask secrets-check --init`. Never commit, copy or share this file.\n",
    );
    let _ = writeln!(out, "version = {HASH_FILE_VERSION}");
    let _ = writeln!(out, "salt = \"{}\"", secret_set::hex_string(salted.salt()));
    let lens: Vec<String> = salted.window_lens().iter().map(usize::to_string).collect();
    let _ = writeln!(out, "window_lens = [{}]", lens.join(", "));
    let _ = writeln!(out, "\n[canary]\nvalue = \"{canary}\"");
    for entry in salted.entries() {
        let _ = writeln!(
            out,
            "\n[[member]]\nkind = \"{}\"\nlen = {}\ntext_only = {}\nhash = \"{}\"",
            entry.kind.name(),
            entry.len,
            entry.text_only,
            secret_set::hex_string(&entry.hash)
        );
    }
    out
}

/// Parses hash file text. Errors name the problem, never the content.
pub fn parse(text: &str) -> Result<HashFile, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|_| "it does not parse as TOML".to_string())?;
    if table.get("version").and_then(toml::Value::as_integer) != Some(HASH_FILE_VERSION) {
        return Err(format!("it is not format version {HASH_FILE_VERSION}"));
    }
    let salt = table
        .get("salt")
        .and_then(toml::Value::as_str)
        .and_then(secret_set::decode_hex)
        .ok_or("its salt is not hex text")?;
    let canary = table
        .get("canary")
        .and_then(|c| c.get("value"))
        .and_then(toml::Value::as_str)
        .filter(|v| is_canary_shape(v))
        .ok_or("its [canary] value is missing or malformed")?
        .to_string();
    let members = table
        .get("member")
        .and_then(toml::Value::as_array)
        .ok_or("it has no [[member]] entries")?;
    let mut entries = Vec::with_capacity(members.len());
    for (i, member) in members.iter().enumerate() {
        let entry = parse_member(member)
            .ok_or_else(|| format!("[[member]] entry {} is malformed", i + 1))?;
        entries.push(entry);
    }
    let salted = SaltedSet::from_parts(salt, entries).map_err(|e| e.to_string())?;
    let lens: Option<std::collections::BTreeSet<usize>> = table
        .get("window_lens")
        .and_then(toml::Value::as_array)
        .and_then(|lens| {
            lens.iter()
                .map(|v| v.as_integer().and_then(|n| usize::try_from(n).ok()))
                .collect()
        });
    if lens.as_ref() != Some(salted.window_lens()) {
        return Err("its window_lens disagree with its members".to_string());
    }
    let canary_hashed = salted
        .scan_as(canary.as_bytes(), true)
        .iter()
        .any(|h| h.kind == MemberKind::Canary && h.offset == 0 && h.len == canary.len());
    if !canary_hashed {
        return Err("its canary is not hashed into the set".to_string());
    }
    Ok(HashFile { salted, canary })
}

fn parse_member(value: &toml::Value) -> Option<SaltedEntry> {
    let kind = MemberKind::from_name(value.get("kind")?.as_str()?)?;
    let len = usize::try_from(value.get("len")?.as_integer()?)
        .ok()
        .filter(|&n| n > 0)?;
    let text_only = value.get("text_only")?.as_bool()?;
    let hash: [u8; 32] = secret_set::decode_hex(value.get("hash")?.as_str()?)?
        .try_into()
        .ok()?;
    Some(SaltedEntry {
        hash,
        kind,
        len,
        text_only,
    })
}

/// Loads the hash file at `path`. A file that is not owner-only (a mode with group or other bits
/// on macOS, a DACL wider than the user and SYSTEM on Windows) is refused, because its salt makes
/// guessing members cheap. The check is `pemu_host::platform`'s, the one the product holds its
/// own private files to.
pub fn load(path: &Path) -> Result<HashFile, LoadError> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Err(LoadError::Missing),
        Err(err) => {
            return Err(LoadError::Unreadable(format!(
                "cannot inspect it: {}",
                err.kind()
            )));
        }
    };
    if !meta.is_file() {
        return Err(LoadError::Unreadable(
            "it is not a regular file".to_string(),
        ));
    }
    match pemu_host::platform::owner_only().check(path) {
        Ok(()) => {}
        Err(pemu_host::platform::PlatformError::NotOwnerOnly { detail, .. }) => {
            return Err(LoadError::Unreadable(format!(
                "it is not owner-only ({detail}); {WIDER_HINT}"
            )));
        }
        Err(err) => {
            return Err(LoadError::Unreadable(format!(
                "its owner-only check failed: {err}"
            )));
        }
    }
    let text = std::fs::read_to_string(path)
        .map_err(|err| LoadError::Unreadable(format!("cannot read it: {}", err.kind())))?;
    parse(&text).map_err(LoadError::Unreadable)
}

/// What a refused hash file tells the user to do, per host.
#[cfg(not(windows))]
const WIDER_HINT: &str = "run chmod 600 on it, or rerun cargo xtask secrets-check --init";
/// What a refused hash file tells the user to do, per host. Windows has no `chmod`; `--init`
/// writes a new file with the protected DACL.
#[cfg(windows)]
const WIDER_HINT: &str = "rerun cargo xtask secrets-check --init, which writes it owner-only";

/// Writes `text` to `path` atomically and owner-only: the parent directory is created
/// owner-only when it is missing, and the content goes to a new owner-only temporary file (mode
/// 0600 on macOS, the protected DACL on Windows, applied at creation by `pemu_host::platform`)
/// that is then renamed over `path`. A rename keeps the temporary file's protection, so the hash
/// file is never readable by another user, not even between the write and the rename.
///
/// An existing parent is used as it is: the config directory also holds `config.toml`, which other
/// tools write, and refusing a wider directory would make `--init` fail where the file it writes
/// is still owner-only.
pub fn write(path: &Path, text: &str) -> Result<(), String> {
    let guard = pemu_host::platform::owner_only();
    let dir = path
        .parent()
        .ok_or("the hash file path has no parent directory")?;
    if !dir.is_dir() {
        guard
            .create_dir(dir)
            .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    }
    let tmp = dir.join(format!(".secrets-check.toml.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut file = guard
        .create_new_file(&tmp)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            std::fs::rename(&tmp, path)
        });
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {}", path.display(), err.kind()));
    }
    Ok(())
}

/// One hashed member found in one file: kind, printable path and offset, never content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedHit {
    pub kind: MemberKind,
    /// Repository-relative path, or withheld as in the pattern report.
    pub path: String,
    pub offset: usize,
}

/// Outcome of the hashed rules over a list of files.
#[derive(Debug, Default)]
pub struct HashedReport {
    pub files_scanned: usize,
    pub hits: Vec<HashedHit>,
}

impl HashedReport {
    /// Scans one file's bytes (text and binary alike) and records its hits.
    pub fn scan_bytes(&mut self, salted: &SaltedSet, shown: &str, bytes: &[u8]) {
        self.files_scanned += 1;
        for hit in salted.scan(bytes) {
            self.hits.push(HashedHit {
                kind: hit.kind,
                path: shown.to_string(),
                offset: hit.offset,
            });
        }
    }

    /// Adds the files and hits of `other`.
    pub fn merge(&mut self, other: HashedReport) {
        self.files_scanned += other.files_scanned;
        self.hits.extend(other.hits);
    }

    /// Hit count per kind.
    pub fn count_by_kind(&self) -> BTreeMap<MemberKind, usize> {
        let mut counts = BTreeMap::new();
        for hit in &self.hits {
            *counts.entry(hit.kind).or_insert(0) += 1;
        }
        counts
    }

    /// Hit lines (`secrets-check: hashed:<kind> <file>:0x<offset>`) and one summary line.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut i = 0;
        while i < self.hits.len() {
            let path = &self.hits[i].path;
            let run = self.hits[i..]
                .iter()
                .take_while(|hit| &hit.path == path)
                .count();
            for hit in &self.hits[i..i + run.min(MAX_LINES_PER_FILE)] {
                let _ = writeln!(
                    out,
                    "secrets-check: hashed:{} {}:0x{:x}",
                    hit.kind.name(),
                    hit.path,
                    hit.offset
                );
            }
            if run > MAX_LINES_PER_FILE {
                let more = run - MAX_LINES_PER_FILE;
                let _ = writeln!(out, "secrets-check: hashed {path} ({more} more)");
            }
            i += run;
        }
        let counts: Vec<String> = self
            .count_by_kind()
            .iter()
            .map(|(kind, n)| format!("hashed:{} {n}", kind.name()))
            .collect();
        let _ = writeln!(
            out,
            "secrets-check: hashed rules: {} file(s) scanned; {} hit(s){}{}",
            self.files_scanned,
            self.hits.len(),
            if counts.is_empty() { "" } else { ": " },
            counts.join(", ")
        );
        out
    }
}

/// Runs the hashed rules over `rel_paths` under `root`, reading files the way the pattern scan
/// does: missing paths, symlinks and non-regular files are skipped, other I/O errors fail.
pub fn scan_files(
    root: &Path,
    rel_paths: &[String],
    salted: &SaltedSet,
) -> Result<HashedReport, String> {
    let mut report = HashedReport::default();
    for rel in rel_paths {
        let shown = if patterns::backup_name(rel) {
            patterns::withheld_path(rel)
        } else {
            rel.clone()
        };
        let full = root.join(rel);
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("cannot inspect {shown}: {}", err.kind())),
        };
        if !meta.is_file() {
            continue;
        }
        let bytes =
            std::fs::read(&full).map_err(|err| format!("cannot read {shown}: {}", err.kind()))?;
        report.scan_bytes(salted, &shown, &bytes);
    }
    Ok(report)
}
