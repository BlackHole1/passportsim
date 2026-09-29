//! `cargo xtask secrets-check` and `cargo xtask hooks` (see `docs/secrets.md`).
//!
//! The pattern rules need no local hash file, so they run in T0 on every host and in the hooks:
//!
//! | Rule | Rejects | Details |
//! |---|---|---|
//! | `mac-shape` | MAC-shaped strings other than the `02:00:00` placeholder prefix | `secrets/patterns.rs` |
//! | `efuse-dump` | files whose size and structure match raw eFuse block dumps | `secrets/patterns.rs` |
//! | `nvs-credential` | NVS partitions with credential keys | `secrets/nvs.rs` |
//! | `cardid-window` | non-0xFF bytes in the cardid window `[0x356000, 0x35A000)` | `secrets/patterns.rs` |
//! | `backup-name` | file names matching device backup naming patterns | `secrets/patterns.rs` |
//! | `rom-pin` | any binary under `assets/rom/` except a pinned, licensed ROM ELF | `secrets/rom.rs` |
//!
//! Output never contains matched content, member values or hashes: only rule names,
//! `file:offset` and counts. A file whose name trips `backup-name` is printed with its name
//! withheld, because the name itself may identify a device backup.
//!
//! Usage is [`USAGE`]. With no `--paths` the scan covers `git ls-files -co --exclude-standard`;
//! `--root` defaults to `git rev-parse --show-toplevel` of the current directory.
//!
//! The hashed rules (`hashed:<kind>`, `secrets/hashed.rs`) run in every scan mode whenever
//! `~/.config/passportsim/secrets-check.toml` exists; `--init` and `--self-test` live in
//! `secrets/device.rs`, the hook modes and the fail-closed rule in `secrets/hooks.rs`, and the
//! git blob sources in `secrets/gitsrc.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

// Submodules live in `secrets/`. Explicit paths keep them resolvable when this file is
// included through `#[path]` by a throwaway development crate.
#[path = "secrets/device.rs"]
// `pub(crate)`: `xtask package` resolves the same local paths for its own secret gate over
// the written package (`package/guard.rs`).
pub(crate) mod device;
#[path = "secrets/gitsrc.rs"]
mod gitsrc;
#[path = "secrets/hashed.rs"]
mod hashed;
#[cfg(test)]
#[path = "secrets/hashed_tests.rs"]
mod hashed_tests;
#[cfg(test)]
#[path = "secrets/hook_tests.rs"]
mod hook_tests;
#[path = "secrets/hooks.rs"]
pub mod hooks;
#[path = "secrets/nvs.rs"]
mod nvs;
#[path = "secrets/patterns.rs"]
// `pub(crate)`: `xtask package` runs these pure predicates over the demo image before it embeds
// it (`package/demo.rs`).
pub(crate) mod patterns;
#[path = "secrets/rom.rs"]
mod rom;
#[path = "secrets/scan.rs"]
// `pub(crate)`: the package guard drops one `cardid-window` hit by its `Rule`
// (`package/guard.rs`).
pub(crate) mod scan;
#[cfg(test)]
#[path = "secrets/tests.rs"]
pub(crate) mod tests;

const USAGE: &str = concat!(
    "usage: cargo xtask secrets-check [--root <dir>] [--paths <files...>]\n",
    "         relative --paths resolve against --root when given, else the current directory\n",
    "       cargo xtask secrets-check --init | --self-test\n",
    "       cargo xtask secrets-check [--root <dir>] --staged | --hook pre-commit\n",
    "       cargo xtask secrets-check [--root <dir>] --hook pre-push [<remote> <url>]\n",
    "       cargo xtask hooks install [--root <dir>] [--force]"
);

/// What to scan.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Every tracked or untracked, not ignored file of the repository.
    Tree,
    /// The given files: absolute, or relative to `--root` when it was given and to the
    /// current directory otherwise.
    Paths(Vec<String>),
    /// `--init`: write the hash file from the local device data.
    Init,
    /// `--self-test`: check the hash file against the local device data.
    SelfTest,
    /// `--staged` or `--hook pre-commit`: the staged blobs.
    Staged,
    /// `--hook pre-push`: the pushed blobs; git's ref lines arrive on stdin.
    PrePush,
    /// Print usage.
    Help,
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    root: Option<PathBuf>,
    mode: Mode,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut root = None;
    let mut mode = Mode::Tree;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                let dir = args.get(i + 1).ok_or("--root needs a directory")?;
                root = Some(PathBuf::from(dir));
                i += 2;
            }
            "--paths" => {
                mode = Mode::Paths(args[i + 1..].to_vec());
                break;
            }
            "--init" => {
                mode = Mode::Init;
                i += 1;
            }
            "--self-test" => {
                mode = Mode::SelfTest;
                i += 1;
            }
            "--staged" => {
                mode = Mode::Staged;
                i += 1;
            }
            "--hook" => match args.get(i + 1).map(String::as_str) {
                Some("pre-commit") => {
                    mode = Mode::Staged;
                    i += 2;
                }
                // git passes the remote name and URL, which the scan does not need.
                Some("pre-push") => {
                    mode = Mode::PrePush;
                    break;
                }
                _ => return Err(format!("--hook needs pre-commit or pre-push\n{USAGE}")),
            },
            "-h" | "--help" => {
                mode = Mode::Help;
                i += 1;
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    Ok(Options { root, mode })
}

/// Entry point of `cargo xtask secrets-check`.
pub fn run(args: &[String]) -> Result<(), String> {
    let opts = parse_args(args)?;
    if opts.mode == Mode::Help {
        println!("{USAGE}");
        return Ok(());
    }
    let env = process_env(&opts.mode)?;
    let stdin = if opts.mode == Mode::PrePush {
        read_hook_stdin()?
    } else {
        None
    };
    run_with(opts, &env, stdin.as_deref())
}

/// [`run`] with injected local paths and pre-push stdin (tests use temporary directories).
fn run_with(opts: Options, env: &device::Env, stdin: Option<&str>) -> Result<(), String> {
    match opts.mode {
        Mode::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        Mode::Init => {
            print!("{}", device::init(env)?);
            return Ok(());
        }
        Mode::SelfTest => {
            print!("{}", device::self_test(env)?);
            return Ok(());
        }
        _ => {}
    }
    let explicit_root = opts.root.is_some();
    let root = match opts.root {
        Some(root) => root,
        None => default_root()?,
    };
    match opts.mode {
        Mode::Staged => {
            let scan = hooks::staged(&root, env)?;
            let refusal = "commit refused; fix or unstage the files listed above";
            hooks::finish(Some(&scan.scope), &scan.outcome, refusal)
        }
        Mode::PrePush => {
            let scan = hooks::pre_push(&root, env, stdin)?;
            let refusal = "push refused; rewrite the commits that add the files listed above";
            hooks::finish(Some(&scan.scope), &scan.outcome, refusal)
        }
        Mode::Paths(paths) => {
            let base = paths_base(explicit_root.then_some(root.as_path()))?;
            let files: Vec<String> = paths.iter().map(|p| relativize(&root, &base, p)).collect();
            scan_worktree_mode(&root, &files, env)
        }
        _ => {
            let files = list_tree(&root)?;
            scan_worktree_mode(&root, &files, env)
        }
    }
}

/// Tree and `--paths` modes: pattern rules, plus hashed rules when the hash file exists.
fn scan_worktree_mode(root: &Path, files: &[String], env: &device::Env) -> Result<(), String> {
    let file = hooks::load_hash_file(env, hooks::Strictness::Tree)?;
    let outcome = hooks::scan_worktree(root, files, file.as_ref().map(|f| &f.salted))?;
    hooks::finish(None, &outcome, "fix or remove the files listed above")
}

/// Pre-push ref lines from stdin; `None` when stdin is a terminal (a manual run).
fn read_hook_stdin() -> Result<Option<String>, String> {
    use std::io::{IsTerminal, Read};
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    let mut text = String::new();
    stdin
        .read_to_string(&mut text)
        .map_err(|err| format!("cannot read the pre-push ref lines: {}", err.kind()))?;
    Ok(Some(text))
}

/// The local paths used by [`run`]: those of `HOME` in normal builds.
///
/// Test builds never reach real device data, the real hash file or the index of the repository
/// that runs the tests through [`run`]: the device and hook modes are refused, and the scan
/// modes see an empty HOME. Tests inject temporary paths through [`run_with`] instead.
#[cfg(not(test))]
fn process_env(_mode: &Mode) -> Result<device::Env, String> {
    device::Env::from_process()
}

#[cfg(test)]
fn process_env(mode: &Mode) -> Result<device::Env, String> {
    match mode {
        Mode::Init | Mode::SelfTest | Mode::Staged | Mode::PrePush => Err(
            "test builds refuse the device and hook modes in run(); tests call run_with with temporary paths"
                .to_string(),
        ),
        _ => device::Env::from_home(Path::new("/nonexistent/passportsim-test-home")),
    }
}

/// The default `--root`: the enclosing work tree in normal builds. Test builds have no default,
/// so a test can never scan, or install hooks into, the repository that runs it.
#[cfg(not(test))]
fn default_root() -> Result<PathBuf, String> {
    git_toplevel()
}

#[cfg(test)]
fn default_root() -> Result<PathBuf, String> {
    Err("test builds require --root".to_string())
}

/// Entry point of `cargo xtask hooks`.
pub fn run_hooks(args: &[String]) -> Result<(), String> {
    hooks::run_hooks(args)
}

/// `git rev-parse --show-toplevel` of the current directory.
#[cfg_attr(test, allow(dead_code))]
fn git_toplevel() -> Result<PathBuf, String> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err("not inside a git repository; pass --root <dir>".to_string());
    }
    let text = String::from_utf8(out.stdout).map_err(|_| "git printed a non-UTF-8 path")?;
    Ok(PathBuf::from(text.trim_end_matches(['\n', '\r'])))
}

/// Files of the tree: `git ls-files -co --exclude-standard`, relative to `root`, `/`-separated.
fn list_tree(root: &Path) -> Result<Vec<String>, String> {
    let out = crate::util::git(root, &["ls-files", "-z", "-c", "-o", "--exclude-standard"])
        .map_err(|err| format!("{err}; is --root a git repository?"))?;
    let mut files: Vec<String> = out
        .split(|&b| b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// The directory that relative `--paths` arguments resolve against: `--root` when it was
/// given on the command line, the current directory otherwise.
fn paths_base(explicit_root: Option<&Path>) -> Result<PathBuf, String> {
    match explicit_root {
        Some(root) => Ok(root.to_path_buf()),
        None => std::env::current_dir()
            .map_err(|err| format!("cannot read the current directory: {}", err.kind())),
    }
}

/// Turns a user-supplied path into a `/`-separated path relative to `root` when it lies
/// inside the root, and keeps it as given (made absolute against `base`) otherwise. A
/// relative path is joined to `base` (see [`paths_base`]); an absolute path ignores it.
fn relativize(root: &Path, base: &Path, given: &str) -> String {
    let abs = base.join(given);
    // Canonicalize both sides so `/tmp` and `/private/tmp` style aliases compare equal.
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let canon = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    match canon.strip_prefix(&canon_root) {
        Ok(rel) => to_slash(rel),
        Err(_) => to_slash(&abs),
    }
}

fn to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
