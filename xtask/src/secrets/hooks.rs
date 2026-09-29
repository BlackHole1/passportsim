//! Scan orchestration of every `secrets-check` scan mode, the fail-closed rule of the hooks,
//! and `cargo xtask hooks install`.
//!
//! Pattern rules run in every mode. Hashed rules run whenever the hash file loads. A hash
//! file that exists but cannot be used always refuses the scan. A missing hash file refuses
//! the hook modes only on a host that has a device directory, macOS or Windows alike;
//! elsewhere the hashed rules are skipped with a note.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use pemu_api::secret_set::SaltedSet;

use super::device::Env;
use super::gitsrc::{self, BlobPlan};
use super::hashed::{self, HashFile, HashedReport, LoadError};
use super::scan::{self, Report};

/// What the user is told to run when the hash file is missing or unusable.
pub const INIT_HINT: &str = "run cargo xtask secrets-check --init";

/// How a missing hash file is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strictness {
    /// Tree and path scans: skip the hashed rules.
    Tree,
    /// Hook modes: refuse when the device directory exists.
    Hook,
}

/// Loads the hash file for a scan (see module docs for the fail-closed rule).
pub fn load_hash_file(env: &Env, strictness: Strictness) -> Result<Option<HashFile>, String> {
    match hashed::load(&env.hash_file) {
        Ok(file) => Ok(Some(file)),
        Err(LoadError::Missing) if strictness == Strictness::Hook && env.has_device_dir() => {
            Err(format!(
                "refused: this host has a device directory but no hash file at {}; {INIT_HINT}",
                env.hash_file.display()
            ))
        }
        Err(LoadError::Missing) => Ok(None),
        Err(err) => Err(format!(
            "refused: {err} ({}); {INIT_HINT}",
            env.hash_file.display()
        )),
    }
}

/// Pattern and hashed results of one scan.
#[derive(Debug, Default)]
pub struct Outcome {
    pub patterns: Report,
    /// `None` when the hashed rules were skipped.
    pub hashed: Option<HashedReport>,
}

impl Outcome {
    /// Total hashed hits (0 when skipped).
    pub fn hashed_hits(&self) -> usize {
        self.hashed.as_ref().map_or(0, |report| report.hits.len())
    }

    /// Pattern report, then hashed report or a skip note.
    pub fn render(&self) -> String {
        let mut out = self.patterns.render();
        match &self.hashed {
            Some(report) => out.push_str(&report.render()),
            None => out.push_str("secrets-check: hashed rules: skipped (no hash file)\n"),
        }
        out
    }

    /// `Ok` when no rule fired; otherwise the counts followed by `refusal`.
    pub fn verdict(&self, refusal: &str) -> Result<(), String> {
        let (patterns, hashed) = (self.patterns.hits.len(), self.hashed_hits());
        if patterns == 0 && hashed == 0 {
            Ok(())
        } else {
            Err(format!(
                "{patterns} pattern rule hit(s) and {hashed} hashed rule hit(s); {refusal}"
            ))
        }
    }
}

/// Prints the scope line and the report, then returns the verdict.
pub fn finish(scope: Option<&str>, outcome: &Outcome, refusal: &str) -> Result<(), String> {
    if let Some(scope) = scope {
        println!("secrets-check: scope: {scope}");
    }
    print!("{}", outcome.render());
    outcome.verdict(refusal)
}

/// Scans files of the working tree (tree and `--paths` modes).
pub fn scan_worktree(
    root: &Path,
    rel_paths: &[String],
    salted: Option<&SaltedSet>,
) -> Result<Outcome, String> {
    let patterns = scan::scan_files(root, rel_paths)?;
    let hashed = salted
        .map(|salted| hashed::scan_files(root, rel_paths, salted))
        .transpose()?;
    Ok(Outcome { patterns, hashed })
}

/// Scans the blobs of a plan through private temporary copies, one batch per blob version.
pub fn scan_plan(
    root: &Path,
    plan: &BlobPlan,
    salted: Option<&SaltedSet>,
) -> Result<Outcome, String> {
    let mut outcome = Outcome {
        patterns: Report::default(),
        hashed: salted.map(|_| HashedReport::default()),
    };
    for batch in gitsrc::batches(&plan.entries) {
        let dir = gitsrc::ScratchDir::new()?;
        gitsrc::materialize(root, dir.path(), &batch, &plan.support)?;
        let rels: Vec<String> = batch.iter().map(|entry| entry.path.clone()).collect();
        let part = scan::scan_files(dir.path(), &rels)?;
        outcome.patterns.files_scanned += part.files_scanned;
        outcome.patterns.files_skipped += part.files_skipped;
        outcome.patterns.roms_pinned += part.roms_pinned;
        outcome.patterns.hits.extend(part.hits);
        if let (Some(total), Some(salted)) = (outcome.hashed.as_mut(), salted) {
            total.merge(hashed::scan_files(dir.path(), &rels, salted)?);
        }
    }
    Ok(outcome)
}

/// Result of a hook-mode scan.
#[derive(Debug)]
pub struct HookScan {
    /// What was scanned, as counts.
    pub scope: String,
    pub outcome: Outcome,
}

/// `--staged` (pre-commit): the staged blobs of the repository at `root`.
pub fn staged(root: &Path, env: &Env) -> Result<HookScan, String> {
    let file = load_hash_file(env, Strictness::Hook)?;
    let plan = gitsrc::staged_plan(root)?;
    let outcome = scan_plan(root, &plan, file.as_ref().map(|f| &f.salted))?;
    Ok(HookScan {
        scope: plan.scope,
        outcome,
    })
}

/// `--hook pre-push`: the pushed blobs. `stdin` holds git's ref lines; `None` or no ref line
/// means a manual run.
pub fn pre_push(root: &Path, env: &Env, stdin: Option<&str>) -> Result<HookScan, String> {
    let file = load_hash_file(env, Strictness::Hook)?;
    let updates = gitsrc::parse_push_lines(stdin.unwrap_or(""));
    let plan = gitsrc::push_plan(root, &updates)?;
    let outcome = scan_plan(root, &plan, file.as_ref().map(|f| &f.salted))?;
    Ok(HookScan {
        scope: plan.scope,
        outcome,
    })
}

/// Marker that identifies hook files written by [`install`].
pub const HOOK_MARKER: &str = "# pemu-secrets-guard";

/// The installed hooks: file name, `secrets-check` arguments, refused action.
pub const HOOKS: [(&str, &str, &str); 2] = [
    ("pre-commit", "--staged", "commit"),
    ("pre-push", "--hook pre-push \"$@\"", "push"),
];

const HOOKS_USAGE: &str = "usage: cargo xtask hooks install [--root <dir>] [--force]";

/// The script of one hook. It runs from the top of the work tree; when cargo, the build or
/// the check fails, the script exits non-zero and git refuses the action (fail closed).
// The script text is compared byte for byte by `presence`, and `.git/hooks` is shared by every
// worktree of a clone, so changing it forces a reinstall everywhere.
pub fn hook_script(name: &str, check_args: &str, action: &str) -> String {
    format!(
        "#!/bin/sh\n\
         {HOOK_MARKER}: {name} hook written by `cargo xtask hooks install`.\n\
         # It refuses the {action} when `cargo xtask secrets-check` reports a hit.\n\
         # Rerun the installer to update this file; manual edits are overwritten.\n\
         set -e\n\
         cd \"$(git rev-parse --show-toplevel)\"\n\
         exec cargo xtask secrets-check {check_args}\n"
    )
}

/// What [`install`] did to one hook file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookState {
    Installed,
    Updated,
    Unchanged,
}

/// The hooks directory git uses for the repository at `root` (`.git/hooks` unless
/// `core.hooksPath` says otherwise).
pub fn hooks_dir(root: &Path) -> Result<PathBuf, String> {
    let out = gitsrc::git(root, &["rev-parse", "--git-path", "hooks"])?;
    let text = String::from_utf8(out).map_err(|_| "git printed a non-UTF-8 hooks path")?;
    let path = PathBuf::from(text.trim_end_matches(['\n', '\r']));
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

/// What `xtask ci` reports about the installed hooks.
///
/// Read-only: it never writes, so a T0 run cannot arm the guard by accident. The executable bit
/// is not part of the answer, because Git for Windows runs a hook by its `#!` line.
pub fn presence(root: &Path) -> Result<Result<String, String>, String> {
    let dir = hooks_dir(root)?;
    let mut missing = Vec::new();
    let mut foreign = Vec::new();
    for (name, check_args, action) in HOOKS {
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(existing) if existing == hook_script(name, check_args, action).as_bytes() => {}
            Ok(existing) if String::from_utf8_lossy(&existing).contains(HOOK_MARKER) => {
                foreign.push(format!("{name} is out of date"));
            }
            Ok(_) => foreign.push(format!("{name} was not written by the installer")),
            Err(err) if err.kind() == ErrorKind::NotFound => missing.push(name),
            Err(err) => return Err(format!("cannot read {}: {}", path.display(), err.kind())),
        }
    }
    if missing.is_empty() && foreign.is_empty() {
        return Ok(Ok(format!("{} hooks armed", HOOKS.len())));
    }
    let mut why = Vec::new();
    if !missing.is_empty() {
        why.push(format!("missing: {}", missing.join(", ")));
    }
    why.extend(foreign);
    Ok(Err(format!(
        "{}; run `cargo xtask hooks install`",
        why.join("; ")
    )))
}

/// Writes the pre-commit and pre-push hooks, executable (0755). Idempotent: identical files
/// stay untouched, files carrying [`HOOK_MARKER`] are updated, and any other existing hook is
/// kept unless `force` is set.
pub fn install(root: &Path, force: bool) -> Result<Vec<(PathBuf, HookState)>, String> {
    let dir = hooks_dir(root)?;
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("cannot create {}: {}", dir.display(), err.kind()))?;
    let mut done = Vec::with_capacity(HOOKS.len());
    for (name, check_args, action) in HOOKS {
        let path = dir.join(name);
        let script = hook_script(name, check_args, action);
        let write = |path: &Path| {
            std::fs::write(path, &script)
                .map_err(|err| format!("cannot write {}: {}", path.display(), err.kind()))
        };
        let state = match std::fs::read(&path) {
            Ok(existing) if existing == script.as_bytes() => HookState::Unchanged,
            Ok(existing) if force || String::from_utf8_lossy(&existing).contains(HOOK_MARKER) => {
                write(&path)?;
                HookState::Updated
            }
            Ok(_) => {
                return Err(format!(
                    "{} exists and was not written by cargo xtask hooks install; move it away or rerun with --force",
                    path.display()
                ));
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                write(&path)?;
                HookState::Installed
            }
            Err(err) => return Err(format!("cannot read {}: {}", path.display(), err.kind())),
        };
        make_executable(&path)?;
        done.push((path, state));
    }
    Ok(done)
}

#[cfg(target_os = "macos")]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .map_err(|err| format!("cannot inspect {}: {}", path.display(), err.kind()))?;
    if meta.permissions().mode() & 0o777 == 0o755 {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|err| format!("cannot make {} executable: {}", path.display(), err.kind()))
}

/// Windows has no executable bit; Git for Windows runs a hook by its `#!` line instead.
#[cfg(not(target_os = "macos"))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Entry point of `cargo xtask hooks`.
pub fn run_hooks(args: &[String]) -> Result<(), String> {
    let (mut root, mut force, mut install_requested) = (None, false, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "install" => install_requested = true,
            "--force" => force = true,
            "--root" => {
                let dir = args.get(i + 1).ok_or("--root needs a directory")?;
                root = Some(PathBuf::from(dir));
                i += 1;
            }
            "-h" | "--help" => {
                println!("{HOOKS_USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n{HOOKS_USAGE}")),
        }
        i += 1;
    }
    if !install_requested {
        return Err(HOOKS_USAGE.to_string());
    }
    let root = match root {
        Some(root) => root,
        None => super::default_root()?,
    };
    for (path, state) in install(&root, force)? {
        let label = match state {
            HookState::Installed => "installed",
            HookState::Updated => "updated",
            HookState::Unchanged => "unchanged",
        };
        println!("hooks: {label} {}", path.display());
    }
    println!(
        "hooks: next run `cargo xtask secrets-check --self-test`, then stage a scratch file holding the [canary] value and confirm that git commit is refused"
    );
    Ok(())
}
