//! The secret-pattern scan of a written package, run before packaging reports success.
//!
//! A package leaves the machine, so both written trees get the rules `cargo xtask secrets-check`
//! applies to a commit, in-process. The pattern rules always run; the hashed rules run only when
//! the packaging host has `~/.config/passportsim/secrets-check.toml`, and the receipt records
//! which ran. Test builds resolve an empty home, so tests exercise the pattern rules only.

use std::path::{Path, PathBuf};

use crate::secrets::device::Env;
use crate::secrets::hooks::{self, Strictness};
use crate::secrets::patterns;
use crate::secrets::scan::Rule;

/// What the scan of one package run found. A hit fails the packaging instead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Regular files read and checked across every scanned tree.
    pub files_scanned: usize,
    /// Always true: the pattern rules need no local state.
    pub pattern_rules: bool,
    /// Whether the packaging host had the hash file, so the hashed rules ran too.
    pub hashed_rules: bool,
}

/// The local paths the gate resolves. Test builds never see the real ones.
#[cfg(not(test))]
pub fn env() -> Result<Env, String> {
    Env::from_process()
}

#[cfg(test)]
pub fn env() -> Result<Env, String> {
    Env::from_home(Path::new("/nonexistent/passportsim-test-home"))
}

/// Scans the binary `xtask package` has just built and put at `rel` below `root`.
///
/// A `cardid-window` hit on it is dropped when it is a structurally valid executable
/// ([`crate::secrets::patterns::executable_structure`]): offset 0x356000 of a linked program
/// longer than 0x35A000 bytes is code, not a flash partition. Every other rule applies.
pub fn scan_built_binary(root: &Path, rel: &str, env: &Env) -> Result<(), String> {
    let hash_file = hooks::load_hash_file(env, Strictness::Tree)?;
    scan_tree_with(
        root,
        &[rel.to_string()],
        hash_file.as_ref().map(|f| &f.salted),
        &[rel],
    )?;
    Ok(())
}

/// Scans every file of every directory in `dirs` and refuses on the first rule hit.
///
/// A refusal names package-relative paths, never a host path. The executables listed with each
/// directory (the native binary, the wasm core) get the [`scan_built_binary`] exception.
pub fn scan(dirs: &[(&Path, &[&str])], env: &Env) -> Result<Verdict, String> {
    let hash_file = hooks::load_hash_file(env, Strictness::Tree)?;
    let salted = hash_file.as_ref().map(|file| &file.salted);
    let mut verdict = Verdict {
        files_scanned: 0,
        pattern_rules: true,
        hashed_rules: salted.is_some(),
    };
    for (dir, built) in dirs {
        verdict.files_scanned += scan_tree_with(dir, &list(dir)?, salted, built)?;
    }
    Ok(verdict)
}

/// Scans one file of a written tree. Used for `receipt.json`, which [`scan`] writes after it runs.
pub fn scan_one(root: &Path, rel: &str, env: &Env) -> Result<(), String> {
    let hash_file = hooks::load_hash_file(env, Strictness::Tree)?;
    scan_tree(
        root,
        &[rel.to_string()],
        hash_file.as_ref().map(|f| &f.salted),
    )?;
    Ok(())
}

/// Scans `files` of one tree and returns how many were read, or the refusal.
fn scan_tree(
    root: &Path,
    files: &[String],
    salted: Option<&pemu_api::secret_set::SaltedSet>,
) -> Result<usize, String> {
    scan_tree_with(root, files, salted, &[])
}

/// [`scan_tree`], with the [`scan_built_binary`] exception for each file of `built`.
fn scan_tree_with(
    root: &Path,
    files: &[String],
    salted: Option<&pemu_api::secret_set::SaltedSet>,
    built: &[&str],
) -> Result<usize, String> {
    let mut outcome = hooks::scan_worktree(root, files, salted)?;
    for &built in built {
        if !outcome
            .patterns
            .hits
            .iter()
            .any(|hit| hit.rule == Rule::CardidWindow && hit.path == built)
        {
            continue;
        }
        let bytes = std::fs::read(root.join(built)).map_err(|e| format!("{built}: {e}"))?;
        if patterns::executable_structure(&bytes) {
            outcome
                .patterns
                .hits
                .retain(|hit| !(hit.rule == Rule::CardidWindow && hit.path == built));
        }
    }
    if let Err(counts) = outcome.verdict("nothing is packaged") {
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "the package".to_string());
        return Err(format!(
            "the secret guard refused {name}:\n{}{counts}\n\
             A package is the one artifact that leaves this machine, so it is scanned with the \
             same rules `cargo xtask secrets-check` applies to a commit.",
            outcome.render()
        ));
    }
    Ok(outcome.patterns.files_scanned)
}

/// Every file below `dir`, as `/`-separated paths relative to it, sorted.
fn list(dir: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path: PathBuf = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            walk(base, &path, out)?;
            continue;
        }
        let relative = path
            .strip_prefix(base)
            .map_err(|_| format!("{} is not below {}", path.display(), base.display()))?;
        out.push(
            relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::tests::{macho64_le, planted, wasm_module};

    fn tree(name: &str, files: &[(&str, Vec<u8>)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-guard-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        for (rel, bytes) in files {
            std::fs::write(dir.join(rel), bytes).expect("write");
        }
        dir
    }

    #[test]
    fn only_the_built_binary_with_a_valid_structure_is_exempt_from_cardid_window() {
        let env = env().expect("the test env");
        let dir = tree(
            "exempt",
            &[
                ("passportsim", planted(&macho64_le())),
                ("other", planted(&macho64_le())),
            ],
        );
        scan_built_binary(&dir, "passportsim", &env).expect("the built binary is exempt");
        assert!(
            scan_one(&dir, "other", &env).is_err(),
            "the same bytes under another name are scanned strictly"
        );
        let refused = scan(&[(&dir, &["passportsim"])], &env).expect_err("`other` still fires");
        assert!(refused.contains("other"), "{refused}");
        assert!(!refused.contains("passportsim:"), "{refused}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_built_wasm_core_is_exempt_in_every_tree_but_a_forged_one_fires() {
        let env = env().expect("the test env");
        let wasm = planted(&wasm_module());
        let package = tree("wasm-package", &[("pemu_wasm.wasm", wasm.clone())]);
        let web = tree("wasm-web", &[("pemu_wasm.wasm", wasm)]);
        scan(
            &[(&package, &["pemu_wasm.wasm"]), (&web, &["pemu_wasm.wasm"])],
            &env,
        )
        .expect("the built wasm core is exempt in both trees");
        let refused = scan(&[(&package, &["pemu_wasm.wasm"]), (&web, &[])], &env)
            .expect_err("an unnamed copy is scanned strictly");
        assert!(refused.contains("cardid-window"), "{refused}");
        // A version 2 header, and a version 1 header whose sections do not tile the file (a flash
        // image behind it), are no module.
        for (name, head) in [
            ("wasm-v2", &b"\0asm\x02\0\0\0"[..]),
            ("wasm-forged", &b"\0asm\x01\0\0\0"[..]),
        ] {
            let forged = tree(name, &[("pemu_wasm.wasm", planted(head))]);
            let refused = scan(&[(&forged, &["pemu_wasm.wasm"])], &env).expect_err(name);
            assert!(refused.contains("cardid-window"), "{name}: {refused}");
            std::fs::remove_dir_all(&forged).ok();
        }
        for dir in [package, web] {
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn a_built_binary_with_a_forged_header_still_fires() {
        let env = env().expect("the test env");
        let mut mz = b"MZ".to_vec();
        mz.resize(0x40, 0);
        mz[0x3C..0x40].copy_from_slice(&0x2000u32.to_le_bytes());
        for (name, head) in [("mz", mz), ("elf", b"\x7fELF".to_vec())] {
            let dir = tree(name, &[("passportsim", planted(&head))]);
            let refused = scan_built_binary(&dir, "passportsim", &env).expect_err(name);
            assert!(refused.contains("cardid-window"), "{name}: {refused}");
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
