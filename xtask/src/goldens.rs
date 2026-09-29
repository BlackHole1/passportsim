//! `cargo xtask oracle goldens --derive`: the operator entry point of the golden derivation.
//!
//! **This step is run by an operator, not by CI.** It reads a raw console capture, masks it with
//! the `pemu_verify` console normalizer and writes the masked text to a directory the operator
//! names, normally `$ROOT/goldens/` under the preserved data root. It never writes into
//! `tests/golden/` and never copies the raw capture: promoting a derived file to a committed
//! golden is a separate, deliberate act, gated on `xtask secrets-check` passing on the derived
//! text (`docs/secrets.md`).
//!
//! It is macOS-only because golden derivation from device logs happens on the oracle host.
//! `oracle.rs` applies the refusal before calling in here.
//!
//! Two safety properties matter more than convenience here, so both are enforced:
//!
//! 1. the derivation refuses to write when a MAC-shaped string survived masking
//!    ([`pemu_verify::goldens::DerivationReport::is_clean`]);
//! 2. it refuses to write outside the directory the operator named, and refuses a directory
//!    inside the repository unless `--allow-in-repo` is given, so a slip of the hand cannot put
//!    device-derived text into the tree without the operator saying so.

use std::fs;
use std::path::{Path, PathBuf};

use pemu_verify::goldens::{Golden, Header, Kind, derive};
use pemu_verify::normalize::BootSelect;

pub(crate) const USAGE: &str = "\
usage: cargo xtask oracle goldens --derive --input <capture> --out <dir> --image <id> \\
         --kind <device|oracle|self> --source <name> --command <line> \\
         --binary-sha256 <hex> --rom-sha256 <hex> --efuse-sha256 <hex> --strap <value> \\
         [--name <file stem>] [--all-boots] [--allow-in-repo]

Writes <out>/<name>.console.txt: the normalized, identity-masked text with a
complete golden header. The raw capture is never copied and `tests/golden/` is never
written to; promote a derived file by hand once `cargo xtask secrets-check` passes on it.";

/// Entry point, called by `oracle.rs` after the macOS check.
pub fn run(root: &Path, flags: &[(String, String)]) -> Result<(), String> {
    let get = |key: &str| -> Option<&str> {
        flags
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    let has = |key: &str| flags.iter().any(|(name, _)| name == key);
    if !has("derive") {
        return Err(format!("`oracle goldens` needs --derive\n{USAGE}"));
    }
    let need = |key: &str| -> Result<String, String> {
        get(key)
            .map(str::to_string)
            .ok_or_else(|| format!("`--{key}` is required\n{USAGE}"))
    };

    let input = PathBuf::from(need("input")?);
    let out = PathBuf::from(need("out")?);
    let image = need("image")?;
    let kind = Kind::parse(&need("kind")?)
        .ok_or_else(|| "`--kind` is `device`, `oracle` or `self`".to_string())?;
    let name = get("name").unwrap_or(&image).to_string();

    if !has("allow-in-repo") && inside_repository(root, &out) {
        return Err(format!(
            "{} is inside the repository. Derived text goes to the preserved data root \
             ($ROOT/goldens/); pass --allow-in-repo only when you mean to write here, and run \
             `cargo xtask secrets-check` before committing anything (docs/secrets.md)",
            out.display()
        ));
    }

    let raw = fs::read(&input).map_err(|err| format!("{}: {err}", input.display()))?;
    let header = Header {
        kind: Some(kind),
        source: need("source")?,
        image: image.clone(),
        command: need("command")?,
        binary_sha256: need("binary-sha256")?,
        rom_sha256: need("rom-sha256")?,
        efuse_sha256: need("efuse-sha256")?,
        strap: need("strap")?,
        provisional: kind.is_provisional(),
        extra: Default::default(),
    };
    let missing = header.missing_fields();
    if !missing.is_empty() {
        return Err(format!(
            "the golden header is incomplete: {}",
            missing.join(", ")
        ));
    }
    let select = if has("all-boots") {
        BootSelect::AllBoots
    } else {
        BootSelect::LastBoot
    };

    let derived = derive(&raw, header, select);
    report(&derived.report);
    if !derived.report.is_clean() {
        return Err(format!(
            "{} MAC-shaped string(s) survived masking; nothing was written",
            derived.report.residual_macs
        ));
    }
    fs::create_dir_all(&out).map_err(|err| format!("{}: {err}", out.display()))?;
    let target = out.join(format!("{name}.console.txt"));
    fs::write(&target, derived.golden.to_bytes())
        .map_err(|err| format!("{}: {err}", target.display()))?;
    // Read it back through the golden parser, so a file that cannot be consumed is caught here
    // rather than in a milestone test.
    let written = fs::read(&target).map_err(|err| format!("{}: {err}", target.display()))?;
    Golden::parse(&written).map_err(|err| format!("{} {err}", target.display()))?;
    println!("oracle goldens: wrote {}", target.display());
    println!(
        "oracle goldens: run `cargo xtask secrets-check --path {}` before committing it \
         (docs/secrets.md)",
        target.display()
    );
    Ok(())
}

/// Whether `out` lands inside the repository.
///
/// `--out` is what the operator typed, so it is usually relative, and a relative path compared
/// against an absolute root never matches. Both sides are therefore resolved first ([`resolve`]).
fn inside_repository(root: &Path, out: &Path) -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve(&cwd, out).starts_with(resolve(&cwd, root))
}

/// An absolute, `.`- and `..`-free path, with symbolic links resolved as far as the path exists.
///
/// The output directory normally does not exist yet (the derivation creates it), so
/// `canonicalize` alone cannot be used: the longest existing prefix is canonicalized, which is
/// what resolves `/tmp` to `/private/tmp` on macOS, and the rest is appended lexically.
fn resolve(cwd: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut lexical = PathBuf::new();
    for part in absolute.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                lexical.pop();
            }
            other => lexical.push(other),
        }
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut existing = lexical.as_path();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            _ => return lexical,
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or_else(|_| existing.into());
    for name in tail.iter().rev() {
        resolved.push(name);
    }
    resolved
}

/// Prints what the derivation masked, so the operator sees the evidence.
fn report(report: &pemu_verify::goldens::DerivationReport) {
    println!(
        "oracle goldens: {} bytes in, {} lines kept, {} timestamped",
        report.input_bytes, report.lines_kept, report.timestamped
    );
    println!(
        "oracle goldens: MAC addresses masked: {}",
        report.macs_masked
    );
    for (name, count) in &report.tails_masked {
        println!("oracle goldens: mask `{name}` applied {count} time(s)");
    }
    println!(
        "oracle goldens: MAC shapes left after masking: {} (must be 0)",
        report.residual_macs
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn root() -> PathBuf {
        crate::util::workspace_root()
    }

    fn full(out: &Path, input: &Path) -> Vec<(String, String)> {
        let mut list = flags(&[
            ("derive", ""),
            ("image", "pk"),
            ("kind", "device"),
            ("source", "device"),
            ("command", "cat capture"),
            ("binary-sha256", "0000"),
            ("rom-sha256", "1111"),
            ("efuse-sha256", "2222"),
            ("strap", "0x0a"),
            ("allow-in-repo", ""),
        ]);
        list.push(("input".to_string(), input.display().to_string()));
        list.push(("out".to_string(), out.display().to_string()));
        list
    }

    #[test]
    fn derive_writes_a_masked_golden_that_parses_back() {
        let root = root();
        let out = root.join("target/oracle-goldens");
        let input = root.join("tools/oracle/fixtures/coverage-reference.console");
        let _ = fs::remove_dir_all(&out);
        run(&root, &full(&out, &input)).expect("the derivation runs");
        let written = fs::read(out.join("pk.console.txt")).expect("the file is written");
        let golden = Golden::parse(&written).expect("it parses as a golden");
        assert_eq!(golden.header.kind, Some(Kind::Device));
        assert_eq!(golden.header.strap, "0x0a");
        assert!(!golden.body.contains("02:00:00:c3:00:01"));
        assert!(golden.body.contains("<MAC>"));
    }

    #[test]
    fn derive_refuses_to_write_into_the_repository_by_default() {
        let root = root();
        let out = root.join("target/oracle-goldens-refused");
        let input = root.join("tools/oracle/fixtures/coverage-reference.console");
        let refused: Vec<(String, String)> = full(&out, &input)
            .into_iter()
            .filter(|(key, _)| key != "allow-in-repo")
            .collect();
        let err = run(&root, &refused).expect_err("a path inside the repository is refused");
        assert!(err.contains("inside the repository"), "{err}");
        assert!(!out.exists(), "nothing was written");
    }

    #[test]
    fn derive_needs_every_header_field() {
        let root = root();
        let out = root.join("target/oracle-goldens-incomplete");
        let input = root.join("tools/oracle/fixtures/coverage-reference.console");
        let partial: Vec<(String, String)> = full(&out, &input)
            .into_iter()
            .filter(|(key, _)| key != "strap")
            .collect();
        let err = run(&root, &partial).expect_err("a missing header field is refused");
        assert!(err.contains("`--strap` is required"), "{err}");
    }

    #[test]
    fn derive_needs_the_derive_switch() {
        let root = root();
        let err = run(&root, &[]).expect_err("refused");
        assert!(err.contains("--derive"), "{err}");
    }

    #[test]
    fn a_relative_out_inside_the_repository_is_refused_too() {
        // Cargo runs this test with the crate directory as the current directory, which is
        // inside the repository, so a relative `--out` must be caught too.
        let root = root();
        let input = root.join("tools/oracle/fixtures/coverage-reference.console");
        let relative = Path::new("oracle-relative-goldens");
        let refused: Vec<(String, String)> = full(relative, &input)
            .into_iter()
            .filter(|(key, _)| key != "allow-in-repo")
            .collect();
        let err = run(&root, &refused).expect_err("a relative path inside the repository");
        assert!(err.contains("inside the repository"), "{err}");
        assert!(
            !std::env::current_dir()
                .expect("a current directory")
                .join(relative)
                .exists(),
            "nothing was written"
        );
    }

    #[test]
    fn resolving_an_out_path_survives_relative_and_dotted_forms() {
        let root = root();
        let cwd = root.join("xtask");
        assert!(inside(&cwd, &root, Path::new("goldens")));
        assert!(inside(&cwd, &root, Path::new("./src/../goldens")));
        assert!(inside(&cwd, &root, &root.join("target/goldens")));
        assert!(!inside(&cwd, &root, Path::new("../../outside-the-tree")));
        assert!(!inside(&cwd, &root, Path::new("/tmp/pemu-goldens")));

        /// [`inside_repository`] with the current directory supplied, so the test needs no
        /// process-wide `set_current_dir`.
        fn inside(cwd: &Path, root: &Path, out: &Path) -> bool {
            resolve(cwd, out).starts_with(resolve(cwd, root))
        }
    }

    #[test]
    fn the_documented_command_line_parses_and_runs() {
        // Every other test here hands `run` a flag vector built by hand; this one goes through
        // the real parser (`oracle::split_args`) with the command line goldens.rs documents.
        let root = root();
        let out = root.join("target/oracle-goldens-parsed");
        let input = root.join("tools/oracle/fixtures/coverage-reference.console");
        let _ = fs::remove_dir_all(&out);
        let command: Vec<String> = [
            "goldens",
            "--derive",
            "--input",
            &input.display().to_string(),
            "--out",
            &out.display().to_string(),
            "--image",
            "pk",
            "--kind",
            "device",
            "--source",
            "device",
            "--command",
            "cat capture",
            "--binary-sha256",
            "0000",
            "--rom-sha256",
            "1111",
            "--efuse-sha256",
            "2222",
            "--strap",
            "0x0a",
            "--all-boots",
            "--allow-in-repo",
        ]
        .iter()
        .map(|value| (*value).to_string())
        .collect();
        let (flags, positional) =
            crate::oracle::split_args(&command).expect("the documented line parses");
        assert_eq!(positional, vec!["goldens".to_string()]);
        run(&root, &flags).expect("and runs");
        let written = fs::read(out.join("pk.console.txt")).expect("the file is written");
        Golden::parse(&written).expect("it parses as a golden");
    }
}
