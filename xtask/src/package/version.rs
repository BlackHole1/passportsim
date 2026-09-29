//! `--version <x.y.z>`: a release build's version, applied over a clean checkout.
//!
//! The release workflow computes the next version from the tags and commits nothing, so the
//! checkout still carries the previous `[workspace.package] version`. [`Stamp::apply`] writes the
//! release version into `Cargo.toml` for the build (cargo then updates the workspace entries of
//! `Cargo.lock` itself) and puts both files back when the stamp is dropped. The receipt records
//! the checkout as it was before the stamp ([`super::receipt::Checkout`]).

use std::path::{Path, PathBuf};

/// Checks that `text` is a plain `X.Y.Z` release version: three dot-separated decimal numbers
/// without leading zeros, no pre-release or build suffix.
pub fn parse(text: &str) -> Result<String, String> {
    let parts: Vec<&str> = text.split('.').collect();
    let numeric = |part: &&str| {
        !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_digit())
            && (*part == "0" || !part.starts_with('0'))
    };
    if parts.len() == 3 && parts.iter().all(numeric) {
        Ok(text.to_string())
    } else {
        Err(format!(
            "--version `{text}` is not a release version: expected X.Y.Z, for example 0.2.0"
        ))
    }
}

/// `manifest` with the `version` of its `[workspace.package]` table set to `version`.
pub fn stamp_manifest(manifest: &str, version: &str) -> Result<String, String> {
    let mut out = String::with_capacity(manifest.len() + 8);
    let mut in_table = false;
    let mut stamped = false;
    for line in manifest.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_table = trimmed == "[workspace.package]";
        } else if in_table && !stamped && is_version_key(trimmed) {
            let ending = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            out.push_str(&format!("version = \"{version}\"{ending}"));
            stamped = true;
            continue;
        }
        out.push_str(line);
    }
    if stamped {
        Ok(out)
    } else {
        Err("Cargo.toml has no `version` in [workspace.package] to set".to_string())
    }
}

fn is_version_key(line: &str) -> bool {
    line.strip_prefix("version")
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// The release version written into the checkout; dropping it restores the original bytes.
pub struct Stamp {
    saved: Vec<(PathBuf, Vec<u8>)>,
}

/// The files a stamp changes: the manifest it writes and the lock file cargo rewrites.
const STAMPED: [&str; 2] = ["Cargo.toml", "Cargo.lock"];

impl Stamp {
    pub fn apply(root: &Path, version: &str) -> Result<Stamp, String> {
        let mut saved = Vec::new();
        for name in STAMPED {
            let path = root.join(name);
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            saved.push((path, bytes));
        }
        let stamp = Stamp { saved };
        let (manifest_path, manifest) = &stamp.saved[0];
        let text = std::str::from_utf8(manifest)
            .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
        let stamped = stamp_manifest(text, version)?;
        std::fs::write(manifest_path, stamped)
            .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
        Ok(stamp)
    }
}

impl Drop for Stamp {
    fn drop(&mut self) {
        for (path, bytes) in &self.saved {
            if let Err(e) = std::fs::write(path, bytes) {
                eprintln!(
                    "package: could not restore {} after the --version build: {e}",
                    path.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_x_y_z_is_a_release_version() {
        for good in ["0.1.0", "1.0.0", "10.20.30", "0.0.0"] {
            assert_eq!(parse(good).unwrap(), good);
        }
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "1.2.3-rc.1",
            "1.2.3+b",
            "01.2.3",
            "1..3",
            "1.2.x",
            " 1.2.3",
        ] {
            let refused = parse(bad).unwrap_err();
            assert!(refused.contains("expected X.Y.Z"), "{bad}: {refused}");
        }
    }

    #[test]
    fn the_stamp_sets_only_the_workspace_package_version() {
        let manifest = "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"0.1.0\"\n\
                        edition = \"2024\"\n\n[workspace.dependencies]\nserde = { version = \"1\" }\n\
                        [package]\nversion = \"9.9.9\"\n";
        let stamped = stamp_manifest(manifest, "0.2.0").unwrap();
        assert_eq!(
            stamped,
            manifest.replace("version = \"0.1.0\"", "version = \"0.2.0\"")
        );
        assert!(stamped.contains("version = \"9.9.9\""), "{stamped}");
        let crlf = manifest.replace('\n', "\r\n");
        assert_eq!(
            stamp_manifest(&crlf, "0.2.0").unwrap(),
            stamped.replace('\n', "\r\n")
        );
        assert!(
            stamp_manifest("[workspace]\n[package]\nversion = \"1.0.0\"\n", "0.2.0")
                .unwrap_err()
                .contains("[workspace.package]")
        );
    }

    #[test]
    fn the_workspace_manifest_can_be_stamped() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the workspace");
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        let stamped = stamp_manifest(&manifest, "98.76.54").unwrap();
        assert!(stamped.contains("version = \"98.76.54\""));
        assert_eq!(stamped.lines().count(), manifest.lines().count());
    }

    #[test]
    fn dropping_the_stamp_restores_both_files() {
        let root = std::env::temp_dir().join(format!("pemu-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let manifest = "[workspace.package]\nversion = \"0.1.0\"\n";
        std::fs::write(root.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(root.join("Cargo.lock"), "lock\n").unwrap();
        {
            let _stamp = Stamp::apply(&root, "0.2.0").unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join("Cargo.toml")).unwrap(),
                "[workspace.package]\nversion = \"0.2.0\"\n"
            );
            std::fs::write(root.join("Cargo.lock"), "rewritten by cargo\n").unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.toml")).unwrap(),
            manifest
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock")).unwrap(),
            "lock\n"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
