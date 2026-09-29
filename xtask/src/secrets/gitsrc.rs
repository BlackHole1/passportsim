//! Git blob sources of the hook modes.
//!
//! The hooks scan what git records or sends, not the working tree:
//! - `--staged` (pre-commit): the blobs of the index that differ from `HEAD` (added, copied,
//!   modified or type-changed), read from the index git will commit (`GIT_INDEX_FILE` is
//!   honored because git commands inherit the hook environment);
//! - `--hook pre-push`: for each pushed ref whose remote tip is known locally, every blob
//!   introduced by the commits of `remote..local`; when the remote tip is new or unknown, the
//!   tree of the pushed tip plus the blobs introduced by its commits that are on no
//!   remote-tracking ref. Run by hand (no ref lines on stdin), it scans `@{upstream}..HEAD`
//!   when an upstream exists and the tracked tree of `HEAD` otherwise.
//!
//! Blobs are written into a private, owner-only temporary directory under their
//! repository paths, so the pattern scan and the hashed rules run unchanged; the directory is
//! removed when the scan ends.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::rom;
pub use crate::util::git;

/// One blob to scan under its repository path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlobEntry {
    /// `/`-separated repository path.
    pub path: String,
    /// Full object name of the blob.
    pub blob: String,
}

/// ROM support files that `rom::RomDir` reads next to a scanned ROM ELF.
const ROM_SUPPORT: [&str; 3] = ["pins.toml", "LICENSE", "NOTICE"];

/// Whether `git -C root <args>` exits with success (its output is discarded).
pub fn git_ok(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// True for an all-zero object name (a created or deleted ref in pre-push input).
pub fn is_zero_sha(sha: &str) -> bool {
    !sha.is_empty() && sha.bytes().all(|b| b == b'0')
}

/// Regular files (any `100xxx` mode) and symlinks carry blobs; gitlinks do not.
fn is_blob_mode(mode: &str) -> bool {
    mode.starts_with("100") || mode == "120000"
}

/// Parses `--raw -z` output of `git diff` or `git diff-tree`:
/// `:<old mode> <new mode> <old sha> <new sha> <status>\0<path>\0`, with a second path for
/// renames and copies. Keeps entries whose new side is a blob; drops deletions and gitlinks.
pub fn parse_raw(out: &[u8]) -> Vec<BlobEntry> {
    let mut entries = Vec::new();
    let mut fields = out.split(|&b| b == 0);
    while let Some(header) = fields.next() {
        let header = String::from_utf8_lossy(header);
        let Some(header) = header.strip_prefix(':') else {
            continue;
        };
        let parts: Vec<&str> = header.split(' ').collect();
        let Some(mut path) = fields.next() else {
            break;
        };
        let status = parts.get(4).copied().unwrap_or("");
        if status.starts_with('R') || status.starts_with('C') {
            match fields.next() {
                Some(target) => path = target,
                None => break,
            }
        }
        if parts.len() < 5 || status.starts_with('D') {
            continue;
        }
        let (new_mode, new_sha) = (parts[1], parts[3]);
        if !is_blob_mode(new_mode) || is_zero_sha(new_sha) {
            continue;
        }
        entries.push(BlobEntry {
            path: String::from_utf8_lossy(path).into_owned(),
            blob: new_sha.to_string(),
        });
    }
    entries
}

/// Parses `git ls-tree -z` output (`<mode> <type> <sha>\t<path>\0`) and `git ls-files -s -z`
/// output (`<mode> <sha> <stage>\t<path>\0`), keeping blob entries.
pub fn parse_listing(out: &[u8]) -> Vec<BlobEntry> {
    let mut entries = Vec::new();
    for record in out.split(|&b| b == 0) {
        let Some(tab) = record.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let meta = String::from_utf8_lossy(&record[..tab]);
        let parts: Vec<&str> = meta.split(' ').collect();
        let sha = match parts.as_slice() {
            [mode, "blob", sha] if is_blob_mode(mode) => *sha,
            [mode, sha, _stage] if is_blob_mode(mode) && sha.len() >= 40 => *sha,
            _ => continue,
        };
        entries.push(BlobEntry {
            path: String::from_utf8_lossy(&record[tab + 1..]).into_owned(),
            blob: sha.to_string(),
        });
    }
    entries
}

/// The blobs of one hook scan.
#[derive(Debug, Default)]
pub struct BlobPlan {
    /// Blobs to scan. In pre-push history one path may carry several blobs.
    pub entries: Vec<BlobEntry>,
    /// `assets/rom/` support files, written next to scanned ROM ELFs but not scanned.
    pub support: Vec<BlobEntry>,
    /// What was selected, as counts.
    pub scope: String,
}

fn rom_support_paths() -> Vec<String> {
    ROM_SUPPORT
        .iter()
        .map(|name| format!("{}{name}", rom::ROM_DIR))
        .collect()
}

/// The pre-commit plan: staged blobs that differ from `HEAD` (or every staged blob before the
/// first commit), plus the index versions of the ROM support files.
pub fn staged_plan(root: &Path) -> Result<BlobPlan, String> {
    let diff = git(
        root,
        &[
            "diff",
            "--cached",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-renames",
            "--diff-filter=ACMT",
        ],
    )?;
    let entries = parse_raw(&diff);
    let support_paths = rom_support_paths();
    let mut args = vec!["ls-files", "-s", "-z", "--"];
    args.extend(support_paths.iter().map(String::as_str));
    let support = parse_listing(&git(root, &args)?);
    Ok(BlobPlan {
        scope: format!("{} staged blob(s)", entries.len()),
        entries,
        support,
    })
}

/// One ref line of pre-push stdin: `<local ref> <local sha> <remote ref> <remote sha>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    pub local_sha: String,
    pub remote_sha: String,
}

/// The well-formed ref lines of pre-push stdin.
pub fn parse_push_lines(text: &str) -> Vec<RefUpdate> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            (fields.len() == 4).then(|| RefUpdate {
                local_sha: fields[1].to_string(),
                remote_sha: fields[3].to_string(),
            })
        })
        .collect()
}

fn rev_list(root: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let mut all = vec!["rev-list"];
    all.extend_from_slice(args);
    let out = git(root, &all)?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// The pre-push plan for `updates` (see module docs); an empty list means a manual run.
pub fn push_plan(root: &Path, updates: &[RefUpdate]) -> Result<BlobPlan, String> {
    let mut commits = BTreeSet::new();
    let mut trees = Vec::new();
    let mut tip = None;
    if updates.is_empty() {
        if git_ok(root, &["rev-parse", "--verify", "-q", "@{upstream}"]) {
            commits.extend(rev_list(root, &["HEAD", "^@{upstream}"])?);
            tip = Some("HEAD".to_string());
        } else if git_ok(root, &["rev-parse", "--verify", "-q", "HEAD^{commit}"]) {
            trees.push("HEAD".to_string());
            tip = Some("HEAD".to_string());
        }
    }
    for update in updates {
        if is_zero_sha(&update.local_sha) {
            continue;
        }
        tip.get_or_insert_with(|| update.local_sha.clone());
        let remote_commit = format!("{}^{{commit}}", update.remote_sha);
        if !is_zero_sha(&update.remote_sha) && git_ok(root, &["cat-file", "-e", &remote_commit]) {
            let exclude = format!("^{}", update.remote_sha);
            commits.extend(rev_list(root, &[&update.local_sha, &exclude])?);
        } else {
            trees.push(update.local_sha.clone());
            commits.extend(rev_list(root, &[&update.local_sha, "--not", "--remotes"])?);
        }
    }
    let mut blobs = BTreeSet::new();
    for tree in &trees {
        blobs.extend(parse_listing(&git(
            root,
            &["ls-tree", "-r", "-z", "--full-tree", tree],
        )?));
    }
    for commit in &commits {
        blobs.extend(parse_raw(&git(
            root,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--raw",
                "--no-abbrev",
                "--no-renames",
                "--no-commit-id",
                "--root",
                "-m",
                "--diff-filter=ACMT",
                commit,
            ],
        )?));
    }
    let support = match &tip {
        Some(tip) => {
            let support_paths = rom_support_paths();
            let mut args = vec!["ls-tree", "-z", "--full-tree", tip.as_str(), "--"];
            args.extend(support_paths.iter().map(String::as_str));
            parse_listing(&git(root, &args)?)
        }
        None => Vec::new(),
    };
    Ok(BlobPlan {
        scope: format!(
            "{} commit(s) and {} tree(s), {} distinct blob(s)",
            commits.len(),
            trees.len(),
            blobs.len()
        ),
        entries: blobs.into_iter().collect(),
        support,
    })
}

static SCRATCH_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A private, owner-only temporary directory, removed on drop.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn new() -> Result<ScratchDir, String> {
        let unique = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!("pemu-secrets-{}-{unique}-{nanos}", std::process::id());
        let path = std::env::temp_dir().join(name);
        // Owner-only at creation on both hosts: mode 0700 on macOS, the protected
        // DACL on Windows, and exclusive, so a name another process planted is refused rather
        // than reused. The blobs written below inherit nothing wider: on macOS the directory's
        // mode keeps other users out, and on Windows a file created in it without a descriptor
        // gets the creator's default DACL, not an ACE of the temporary directory's.
        pemu_host::platform::owner_only()
            .create_new_dir(&path)
            .map_err(|err| format!("cannot create a private temporary directory: {err}"))?;
        Ok(ScratchDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Splits entries into batches in which each path appears once: the k-th distinct blob of a
/// path goes into batch k.
pub fn batches(entries: &[BlobEntry]) -> Vec<Vec<BlobEntry>> {
    let mut seen: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut out: Vec<Vec<BlobEntry>> = Vec::new();
    for entry in entries {
        let blobs = seen.entry(entry.path.as_str()).or_default();
        if !blobs.insert(entry.blob.as_str()) {
            continue;
        }
        let k = blobs.len() - 1;
        if out.len() <= k {
            out.push(Vec::new());
        }
        out[k].push(entry.clone());
    }
    out
}

/// Writes every blob of `batch` into `dir` under its repository path, and, when the batch
/// touches `assets/rom/`, the `support` files it does not already hold.
pub fn materialize(
    root: &Path,
    dir: &Path,
    batch: &[BlobEntry],
    support: &[BlobEntry],
) -> Result<(), String> {
    let held: BTreeSet<&str> = batch.iter().map(|e| e.path.as_str()).collect();
    let touches_rom = batch.iter().any(|e| rom::is_under_rom_dir(&e.path));
    let extra = support
        .iter()
        .filter(|e| touches_rom && !held.contains(e.path.as_str()));
    for entry in batch.iter().chain(extra) {
        let target = safe_join(dir, &entry.path)
            .ok_or_else(|| "git listed a path that is not repository-relative".to_string())?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                format!("cannot prepare a temporary copy of a blob: {}", err.kind())
            })?;
        }
        let bytes = git(root, &["cat-file", "blob", &entry.blob])?;
        std::fs::write(&target, bytes)
            .map_err(|err| format!("cannot write a temporary copy of a blob: {}", err.kind()))?;
    }
    Ok(())
}

fn safe_join(dir: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    let normal = rel_path
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    (!rel.is_empty() && normal).then(|| dir.join(rel_path))
}
