//! Small helpers every xtask module shares: the workspace root and running git.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Workspace root: the parent of this `xtask` crate.
pub fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest).to_path_buf()
}

/// Runs `git -C dir <args>` with stdin closed and returns its stdout. The error names the git
/// subcommand, the directory and what git printed on stderr.
pub fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if !out.status.success() {
        let sub = args.iter().find(|a| !a.starts_with('-')).unwrap_or(&"");
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "git {sub} failed in {}: {}",
            dir.display(),
            stderr.trim()
        ));
    }
    Ok(out.stdout)
}

/// [`git`] with its stdout as trimmed text.
pub fn git_text(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git(dir, args)?;
    let text = String::from_utf8(out).map_err(|_| format!("git {} printed non-UTF-8", args[0]))?;
    Ok(text.trim().to_string())
}
