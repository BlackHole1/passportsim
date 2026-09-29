//! The corpus locator: finds the firmware images and ELFs, and answers per id whether each file
//! is there and is the file the manifest pins.
//!
//! The corpus is not in the repository: it lives in `corpus/` under the data root, with a
//! `MANIFEST.json` recording each file's id, name, path, size and SHA-256.
//!
//! **A missing corpus is a skip; a wrong one is a failure.** T0 needs no corpus, so an absent
//! file is a typed [`Located`] answer to skip on; a file with the wrong size or hash is
//! [`Located::Corrupt`] and fails the test.
//!
//! This crate resolves no host directory role: it reads only `PASSPORTSIM_DATA_ROOT` (unset:
//! [`RootError::NotConfigured`], a skip), or takes a root through [`Corpus::locate_at`]. Reports
//! name ids, never paths.

mod json;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use self::json::Scalar;

/// Overrides the data-root role; the only variable this crate reads. Composing
/// `PASSPORTSIM_HOME` is the host resolvers' job, not a test crate's.
pub const DATA_ROOT_ENV: &str = "PASSPORTSIM_DATA_ROOT";
/// The corpus below the data root.
pub const CORPUS_DIR: &str = "corpus";
/// The manifest inside the corpus directory.
pub const MANIFEST_FILE: &str = "MANIFEST.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootError {
    /// `PASSPORTSIM_DATA_ROOT` is not set, so this process has no root it may read.
    NotConfigured,
    /// The override is not an absolute path; this crate expands neither `~` nor a role
    /// composition.
    NotAbsolute {
        /// The variable that was set.
        var: &'static str,
    },
}

impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootError::NotConfigured => write!(
                f,
                "no data root: set {DATA_ROOT_ENV} to an absolute path; \
                 pemu-testkit resolves no host directory role of its own"
            ),
            RootError::NotAbsolute { var } => {
                write!(
                    f,
                    "{var} is not an absolute path; a role must be absolute, and this crate \
                     expands no `~` (only pemu_host::paths and xtask/src/hostdirs.rs do)"
                )
            }
        }
    }
}

/// The data root from `PASSPORTSIM_DATA_ROOT` only. A caller that already has the root passes it
/// to [`Corpus::locate_at`] instead.
pub fn data_root_from_env() -> Result<PathBuf, RootError> {
    let Some(dir) = non_empty_var(DATA_ROOT_ENV) else {
        return Err(RootError::NotConfigured);
    };
    let path = PathBuf::from(dir);
    if !path.is_absolute() {
        return Err(RootError::NotAbsolute { var: DATA_ROOT_ENV });
    }
    Ok(path)
}

/// The value of an environment variable, a blank one counting as unset, as
/// `pemu_host::paths::Env::from_process` counts it.
fn non_empty_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Why the corpus as a whole is unavailable, as opposed to one file being absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CorpusError {
    Root(RootError),
    NoManifest,
    ManifestUnreadable {
        /// The `std::io::ErrorKind` as text; the path is deliberately left out.
        kind: String,
    },
    ManifestInvalid {
        detail: String,
    },
}

impl fmt::Display for CorpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorpusError::Root(err) => write!(f, "{err}"),
            CorpusError::NoManifest => write!(
                f,
                "no corpus: {CORPUS_DIR}/{MANIFEST_FILE} is not below the data root"
            ),
            CorpusError::ManifestUnreadable { kind } => {
                write!(f, "the corpus manifest cannot be read ({kind})")
            }
            CorpusError::ManifestInvalid { detail } => {
                write!(f, "the corpus manifest is invalid: {detail}")
            }
        }
    }
}

/// One manifest record. An id can list several files (`pk` has the merged image and two ELFs), so
/// `(id, file)` is the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorpusFile {
    /// Corpus id, for example `pk` or `rom101`.
    pub id: String,
    pub file: String,
    /// Absolute path as the manifest records it. Never printed by this module.
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

impl CorpusFile {
    /// `<id>/<file>`, the name a report uses.
    pub fn name(&self) -> String {
        format!("{}/{}", self.id, self.file)
    }
}

/// What checking one file found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Verified,
    /// The size matches; the hash was not computed.
    SizeOk,
    Missing,
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    HashMismatch {
        /// The first 16 hex digits of the manifest's hash.
        expected_prefix: String,
        actual_prefix: String,
    },
    Unreadable {
        kind: String,
    },
}

impl Outcome {
    pub fn is_usable(&self) -> bool {
        matches!(self, Outcome::Verified | Outcome::SizeOk)
    }

    /// True when the file is on disk but is not the pinned file. An unreadable file counts: it is
    /// present, so the host has the corpus and something is wrong with it.
    pub fn is_corrupt(&self) -> bool {
        matches!(
            self,
            Outcome::SizeMismatch { .. }
                | Outcome::HashMismatch { .. }
                | Outcome::Unreadable { .. }
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Verified => "verified",
            Outcome::SizeOk => "size ok",
            Outcome::Missing => "missing",
            Outcome::SizeMismatch { .. } => "SIZE MISMATCH",
            Outcome::HashMismatch { .. } => "HASH MISMATCH",
            Outcome::Unreadable { .. } => "unreadable",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::SizeMismatch { expected, actual } => {
                write!(f, "SIZE MISMATCH (pinned {expected} bytes, found {actual})")
            }
            Outcome::HashMismatch {
                expected_prefix,
                actual_prefix,
            } => write!(
                f,
                "HASH MISMATCH (pinned {expected_prefix}, found {actual_prefix})"
            ),
            Outcome::Unreadable { kind } => write!(f, "unreadable ({kind})"),
            other => f.write_str(other.as_str()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStatus {
    pub file: CorpusFile,
    pub outcome: Outcome,
}

impl fmt::Display for FileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.file.name(), self.outcome)
    }
}

/// Found, absent, or present and wrong, which are not interchangeable: `Missing` **skips** with
/// [`Located::skip_reason`], `Corrupt` **fails** with [`Located::failure_reason`], since a green
/// exit against an unpinned image would be worse than none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Located {
    Found(Vec<CorpusFile>),
    /// A file is absent, the manifest does not list the id, or the whole corpus is unavailable.
    Missing {
        id: String,
        /// Why, in one line and without a path.
        reason: String,
        files: Vec<FileStatus>,
    },
    /// A file of the id is on disk and wrong (size, SHA-256, or unreadable).
    Corrupt {
        id: String,
        /// What is wrong, in one line and without a path.
        reason: String,
        files: Vec<FileStatus>,
    },
}

impl Located {
    pub fn found(&self) -> Option<&[CorpusFile]> {
        match self {
            Located::Found(files) => Some(files),
            Located::Missing { .. } | Located::Corrupt { .. } => None,
        }
    }

    pub fn file(&self, file: &str) -> Option<&CorpusFile> {
        self.found()?.iter().find(|f| f.file == file)
    }

    /// The reason to **skip** with, naming ids and files, never a host path.
    pub fn skip_reason(&self) -> Option<String> {
        match self {
            Located::Missing { id, reason, files } => {
                Some(report(id, "unavailable", reason, files))
            }
            Located::Found(_) | Located::Corrupt { .. } => None,
        }
    }

    /// The reason to **fail** with; a present-but-wrong file is never a skip.
    pub fn failure_reason(&self) -> Option<String> {
        match self {
            Located::Corrupt { id, reason, files } => {
                Some(report(id, "is not the pinned corpus", reason, files))
            }
            Located::Found(_) | Located::Missing { .. } => None,
        }
    }
}

/// One verdict line per file under a heading that names the id, never a host path.
fn report(id: &str, verdict: &str, reason: &str, files: &[FileStatus]) -> String {
    let mut text = format!("corpus id `{id}` {verdict}: {reason}");
    for status in files {
        text.push_str(&format!("\n  {status}"));
    }
    text
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Corpus {
    root: PathBuf,
    files: Vec<CorpusFile>,
}

impl Corpus {
    /// Reads the manifest below the data root from the environment.
    pub fn locate() -> Result<Corpus, CorpusError> {
        let root = data_root_from_env().map_err(CorpusError::Root)?;
        Corpus::locate_at(&root)
    }

    pub fn locate_at(data_root: &Path) -> Result<Corpus, CorpusError> {
        let dir = data_root.join(CORPUS_DIR);
        let manifest = dir.join(MANIFEST_FILE);
        let text = match std::fs::read_to_string(&manifest) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(CorpusError::NoManifest);
            }
            Err(err) => {
                return Err(CorpusError::ManifestUnreadable {
                    kind: format!("{:?}", err.kind()),
                });
            }
        };
        let files = parse_manifest(&text, &dir)?;
        Ok(Corpus { root: dir, files })
    }

    /// The corpus directory the manifest was read from. Data, not for a report.
    pub fn dir(&self) -> &Path {
        &self.root
    }

    pub fn files(&self) -> &[CorpusFile] {
        &self.files
    }

    pub fn ids(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for file in &self.files {
            if !out.contains(&file.id.as_str()) {
                out.push(&file.id);
            }
        }
        out
    }

    pub fn records(&self, id: &str) -> Vec<&CorpusFile> {
        self.files.iter().filter(|f| f.id == id).collect()
    }

    /// Checks existence and size, and the SHA-256 when `verify` is set (tens of milliseconds for
    /// an 8 MB image).
    pub fn check(&self, file: &CorpusFile, verify: bool) -> FileStatus {
        FileStatus {
            file: file.clone(),
            outcome: check_outcome(file, verify),
        }
    }

    /// The answer for one id, hashes verified. An id the manifest does not list is `Missing`
    /// with that reason, so a typo does not look like an absent file.
    pub fn locate_id(&self, id: &str) -> Located {
        self.locate_id_with(id, true)
    }

    pub fn locate_id_with(&self, id: &str, verify: bool) -> Located {
        let records = self.records(id);
        if records.is_empty() {
            return Located::Missing {
                id: id.to_string(),
                reason: "the manifest lists no such id".to_string(),
                files: Vec::new(),
            };
        }
        let statuses: Vec<FileStatus> = records
            .into_iter()
            .map(|file| self.check(file, verify))
            .collect();
        if statuses.iter().all(|s| s.outcome.is_usable()) {
            return Located::Found(statuses.into_iter().map(|s| s.file).collect());
        }
        let total = statuses.len();
        // A present-but-wrong file decides the verdict even when another file of the id is
        // absent: the host has this corpus, so it is a failure.
        let corrupt = statuses.iter().filter(|s| s.outcome.is_corrupt()).count();
        if corrupt > 0 {
            return Located::Corrupt {
                id: id.to_string(),
                reason: format!(
                    "{corrupt} of {total} files are on disk and are not the pinned file"
                ),
                files: statuses,
            };
        }
        let bad = statuses.iter().filter(|s| !s.outcome.is_usable()).count();
        Located::Missing {
            id: id.to_string(),
            reason: format!("{bad} of {total} files did not check out"),
            files: statuses,
        }
    }
}

/// The answer for one id, from `PASSPORTSIM_DATA_ROOT`. Whole-corpus errors become
/// [`Located::Missing`], never [`Located::Corrupt`]: this host does not have the corpus.
pub fn locate_id(id: &str) -> Located {
    located_or_missing(Corpus::locate(), id)
}

/// [`locate_id`] against an explicit data root, for a test that must not read whatever the
/// environment points at.
pub fn locate_id_at(data_root: &Path, id: &str) -> Located {
    located_or_missing(Corpus::locate_at(data_root), id)
}

fn located_or_missing(corpus: Result<Corpus, CorpusError>, id: &str) -> Located {
    match corpus {
        Ok(corpus) => corpus.locate_id(id),
        Err(err) => Located::Missing {
            id: id.to_string(),
            reason: err.to_string(),
            files: Vec::new(),
        },
    }
}

fn check_outcome(file: &CorpusFile, verify: bool) -> Outcome {
    let meta = match std::fs::metadata(&file.path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Outcome::Missing,
        Err(err) => {
            return Outcome::Unreadable {
                kind: format!("{:?}", err.kind()),
            };
        }
    };
    if !meta.is_file() {
        return Outcome::Missing;
    }
    if meta.len() != file.size {
        return Outcome::SizeMismatch {
            expected: file.size,
            actual: meta.len(),
        };
    }
    if !verify {
        return Outcome::SizeOk;
    }
    let bytes = match std::fs::read(&file.path) {
        Ok(bytes) => bytes,
        Err(err) => {
            return Outcome::Unreadable {
                kind: format!("{:?}", err.kind()),
            };
        }
    };
    let actual = sha256_hex(&bytes);
    if actual.eq_ignore_ascii_case(&file.sha256) {
        Outcome::Verified
    } else {
        Outcome::HashMismatch {
            expected_prefix: prefix16(&file.sha256),
            actual_prefix: prefix16(&actual),
        }
    }
}

/// The lower-case hex SHA-256 of `bytes`, as the manifest pins it. Public for the milestone
/// self-checks, whose only dependency is this crate.
pub fn sha256_hex(bytes: &[u8]) -> String {
    pemu_loader::hex(&pemu_loader::sha256(bytes))
}

fn prefix16(hash: &str) -> String {
    hash.chars().take(16).collect()
}

/// Reads the manifest records. A relative `path` resolves against the corpus directory.
fn parse_manifest(text: &str, dir: &Path) -> Result<Vec<CorpusFile>, CorpusError> {
    let records = json::parse_object_array(text).map_err(|err| CorpusError::ManifestInvalid {
        detail: err.to_string(),
    })?;
    let mut out = Vec::with_capacity(records.len());
    for (n, record) in records.iter().enumerate() {
        out.push(
            record_to_file(record, dir).map_err(|detail| CorpusError::ManifestInvalid {
                detail: format!("record {n}: {detail}"),
            })?,
        );
    }
    Ok(out)
}

fn record_to_file(record: &BTreeMap<String, Scalar>, dir: &Path) -> Result<CorpusFile, String> {
    let id = string_field(record, "id")?;
    let file = string_field(record, "file")?;
    let path = PathBuf::from(string_field(record, "path")?);
    let path = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    let size = record
        .get("size")
        .and_then(Scalar::as_u64)
        .ok_or("field `size` is missing or not an integer")?;
    let sha256 = string_field(record, "sha256")?;
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("field `sha256` is not 64 hex digits".to_string());
    }
    Ok(CorpusFile {
        id,
        file,
        path,
        size,
        sha256: sha256.to_ascii_lowercase(),
    })
}

fn string_field(record: &BTreeMap<String, Scalar>, name: &str) -> Result<String, String> {
    record
        .get(name)
        .and_then(Scalar::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("field `{name}` is missing or not a string"))
}
