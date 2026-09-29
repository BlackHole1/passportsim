//! The emulator build id of the boot-cache key, shared by `build.rs` and this crate's tests.
//!
//! A boot-cache entry is only valid for the build that booted it, so the id is SHA-256 over the
//! sorted `"<file sha256 hex>  <path>\n"` lines of every file below [`ROOTS`] (workspace-relative,
//! `/`-separated, skipping [`SKIPPED_DIRS`] and dot files). It depends on file contents only, so
//! two checkouts of one tree agree.

use std::path::{Path, PathBuf};

/// What a build is made from, relative to the workspace.
pub const ROOTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "assets/rom/pins.toml",
    "boards",
    "specs",
    "crates",
];

/// Directory names skipped at any depth: nothing a product build reads.
pub const SKIPPED_DIRS: &[&str] = &["tests", "benches", "docs", "target", "node_modules"];

/// Every file the id covers below `workspace`, as (workspace-relative `/` path, path), sorted.
pub fn files(workspace: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for root in ROOTS {
        collect(workspace, &workspace.join(root), &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// The build id of `workspace` with `sha256` as the hash.
pub fn build_id(workspace: &Path, sha256: impl Fn(&[u8]) -> [u8; 32]) -> String {
    let hex = |bytes: &[u8; 32]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut listing = String::new();
    for (relative, path) in files(workspace) {
        let bytes = std::fs::read(&path).unwrap_or_default();
        listing.push_str(&hex(&sha256(&bytes)));
        listing.push_str("  ");
        listing.push_str(&relative);
        listing.push('\n');
    }
    hex(&sha256(listing.as_bytes()))
}

fn collect(workspace: &Path, path: &Path, out: &mut Vec<(String, PathBuf)>) {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.starts_with('.') {
        return;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        if SKIPPED_DIRS.contains(&name) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect(workspace, &entry.path(), out);
        }
    } else if meta.is_file() {
        let Ok(relative) = path.strip_prefix(workspace) else {
            return;
        };
        let relative = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push((relative, path.to_path_buf()));
    }
}
