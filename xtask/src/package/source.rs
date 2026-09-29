//! `--payload-from`: the host-independent half of a package, taken from a package another host
//! built.
//!
//! Two hosts cannot compile the same wasm core: after `--remap-path-prefix` the panic locations
//! keep the host's separator, and symbol hashes derive from host-specific `-C metadata` inputs. So
//! one host builds the core and the other ships those bytes, like the macOS-only demo.
//!
//! Checked, in order:
//!
//! 1. the source is a finished package whose binary embeds its recorded payload digest;
//! 2. it was built from a clean checkout of the same tree (`git rev-parse HEAD^{tree}`) as this
//!    clean checkout;
//! 3. it was built as the same version (the workspace version, or the same `--version`);
//! 4. the demo passes [`super::demo::from_package`];
//! 5. this host's written payload equals the source's, file for file ([`Source::check`]).

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::demo::{self, Found};
use super::layout::PayloadDigest;

const WASM: &str = "payload/web/pemu_wasm.wasm";

/// A package this run takes its wasm core and demo from.
#[derive(Clone, Debug)]
pub struct Source {
    pub dir: PathBuf,
    /// The source receipt's `commit`.
    pub commit: String,
    /// The source receipt's `target`.
    pub target: String,
    /// The source receipt's `payload.sha256`.
    pub payload_sha256: String,
    /// The source receipt's payload files, as (path, sha256).
    files: Vec<(String, String)>,
    /// Whether the source receipt records an embedded demo.
    demo_embedded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    /// The source package's target.
    pub target: String,
    /// The source package's commit.
    pub commit: String,
    /// The source package's payload digest, which equals this package's.
    pub payload_sha256: String,
}

impl Source {
    /// Opens the package at `dir` and checks steps 1 to 3 against the checkout at `root` and the
    /// `version` this run builds as.
    pub fn open(dir: &Path, root: &Path, version: &str) -> Result<Source, String> {
        let refused = |why: String| format!("--payload-from: {why}");
        let text = std::fs::read_to_string(dir.join("receipt.json")).map_err(|e| {
            refused(format!(
                "the named directory has no readable `receipt.json` ({e}); it must be a package \
                 directory that `xtask package` wrote"
            ))
        })?;
        let receipt: Value = serde_json::from_str(&text)
            .map_err(|e| refused(format!("`receipt.json` is not JSON ({e})")))?;
        let field = |value: &Value, what: &str| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| refused(format!("`receipt.json` records no `{what}`")))
        };
        if receipt["payload"]["embedded"] != Value::Bool(true) {
            return Err(refused(
                "the source package's binary does not embed its payload, so it is not a finished \
                 package"
                    .into(),
            ));
        }
        if receipt["dirty"] != Value::Bool(false) {
            return Err(refused(
                "the source package was built from a checkout with uncommitted changes, so no \
                 tree names what it was built from; package it again from a clean checkout"
                    .into(),
            ));
        }
        let tree = field(&receipt["tree"], "tree")?;
        let ours = super::receipt::git(root, &["rev-parse", "HEAD^{tree}"])
            .ok_or_else(|| refused("this checkout's tree could not be read with git".into()))?;
        let dirty = super::receipt::git(root, &["status", "--porcelain"])
            .ok_or_else(|| refused("this checkout's status could not be read with git".into()))?;
        if !dirty.is_empty() {
            return Err(refused(
                "this checkout has uncommitted changes, so its payload cannot be the source's; \
                 commit or remove them first"
                    .into(),
            ));
        }
        if ours != tree {
            return Err(refused(format!(
                "the source package was built from tree {tree}, this checkout is tree {ours}; the \
                 two payloads can only be one payload when the sources are the same"
            )));
        }
        let theirs = field(&receipt["version"], "version")?;
        if theirs != version {
            return Err(refused(format!(
                "the source package was built as version {theirs}, this run builds {version}; \
                 pass both runs the same --version"
            )));
        }
        let files = receipt["payload"]["files"]
            .as_array()
            .ok_or_else(|| refused("`receipt.json` lists no payload files".into()))?
            .iter()
            .map(|file| {
                Ok((
                    field(&file["path"], "path")?,
                    field(&file["sha256"], "sha256")?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        if !dir.join(WASM).is_file() {
            return Err(refused(format!("the source package has no `{WASM}`")));
        }
        Ok(Source {
            dir: dir.to_path_buf(),
            commit: field(&receipt["commit"], "commit")?,
            target: field(&receipt["target"], "target")?,
            payload_sha256: field(&receipt["payload"]["sha256"], "payload.sha256")?,
            files,
            demo_embedded: receipt["demo"]["embedded"] == Value::Bool(true),
        })
    }

    pub fn wasm(&self) -> PathBuf {
        self.dir.join(WASM)
    }

    /// The source package's demo, or its absence when the source has none.
    pub fn demo(&self) -> Result<Found, String> {
        if self.demo_embedded {
            demo::from_package(&self.dir)
        } else {
            Ok(Found::Absent(demo::Absent {
                reason: format!(
                    "the `{}` package this one takes its payload from carries no demo",
                    self.target
                ),
            }))
        }
    }

    /// Step 5: `payload` is the source's payload, file for file.
    pub fn check(&self, payload: &PayloadDigest) -> Result<Origin, String> {
        let ours: Vec<(String, String)> = payload
            .files
            .iter()
            .map(|file| (file.path.clone(), file.sha256.clone()))
            .collect();
        if payload.sha256 != self.payload_sha256 || ours != self.files {
            let mut differ: Vec<&str> = ours
                .iter()
                .filter(|file| !self.files.contains(file))
                .map(|(path, _)| path.as_str())
                .chain(
                    self.files
                        .iter()
                        .filter(|file| !ours.contains(file))
                        .map(|(path, _)| path.as_str()),
                )
                .collect();
            differ.sort();
            differ.dedup();
            return Err(format!(
                "--payload-from: this package's payload {} is not the source's {}; the files that \
                 differ: {differ:?}. Both hosts must ship one payload, so nothing is packaged",
                payload.sha256, self.payload_sha256
            ));
        }
        Ok(Origin {
            target: self.target.clone(),
            commit: self.commit.clone(),
            payload_sha256: self.payload_sha256.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::layout::PayloadFile;

    fn payload(files: &[(&str, &str)], sha256: &str) -> PayloadDigest {
        PayloadDigest {
            sha256: sha256.into(),
            files: files
                .iter()
                .map(|(path, sha)| PayloadFile {
                    path: (*path).into(),
                    size: 1,
                    sha256: (*sha).into(),
                })
                .collect(),
        }
    }

    fn source(files: &[(&str, &str)], sha256: &str) -> Source {
        Source {
            dir: PathBuf::from("unused"),
            commit: "c0ffee".into(),
            target: "aarch64-apple-darwin".into(),
            payload_sha256: sha256.into(),
            files: files
                .iter()
                .map(|(p, s)| ((*p).to_string(), (*s).to_string()))
                .collect(),
            demo_embedded: false,
        }
    }

    #[test]
    fn the_payload_must_be_the_sources_file_for_file() {
        let files = [
            ("payload/web/main.js", "aa"),
            ("payload/web/pemu_wasm.wasm", "bb"),
        ];
        let from = source(&files, "d1");
        let origin = from
            .check(&payload(&files, "d1"))
            .expect("the same payload");
        assert_eq!(origin.payload_sha256, "d1");
        assert_eq!(origin.target, "aarch64-apple-darwin");

        let other = [
            ("payload/web/main.js", "cc"),
            ("payload/web/pemu_wasm.wasm", "bb"),
        ];
        let refused = from.check(&payload(&other, "d2")).unwrap_err();
        assert!(refused.contains("payload/web/main.js"), "{refused}");
        assert!(
            !refused.contains("pemu_wasm.wasm"),
            "only the differing file: {refused}"
        );

        let fewer = [("payload/web/pemu_wasm.wasm", "bb")];
        let refused = from.check(&payload(&fewer, "d3")).unwrap_err();
        assert!(
            refused.contains("payload/web/main.js"),
            "a missing file differs: {refused}"
        );
    }

    /// Steps 1 to 3 refuse a package that is not finished, not of a clean tree or of another
    /// version.
    #[test]
    fn a_source_that_is_not_a_finished_package_of_a_clean_tree_is_refused() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the workspace");
        let dir = std::env::temp_dir().join(format!("pemu-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let refused = Source::open(&dir, root, "0.1.0").unwrap_err();
        assert!(refused.contains("receipt.json"), "{refused}");

        let tree = |tree: &str, embedded: bool, dirty: bool| {
            serde_json::json!({
                "version": "0.1.0",
                "commit": "c0ffee",
                "tree": tree,
                "dirty": dirty,
                "target": "aarch64-apple-darwin",
                "payload": {"sha256": "d1", "embedded": embedded, "files": []},
                "demo": {"embedded": false},
            })
            .to_string()
        };
        let receipt = |embedded: bool, dirty: bool| tree(&"0".repeat(40), embedded, dirty);
        std::fs::write(dir.join("receipt.json"), receipt(false, false)).unwrap();
        assert!(
            Source::open(&dir, root, "0.1.0")
                .unwrap_err()
                .contains("does not embed")
        );
        std::fs::write(dir.join("receipt.json"), receipt(true, true)).unwrap();
        assert!(
            Source::open(&dir, root, "0.1.0")
                .unwrap_err()
                .contains("uncommitted changes")
        );
        // A clean source of a tree that is not this checkout's: refused for the tree, or first for
        // this checkout's own uncommitted changes when it has some.
        std::fs::write(dir.join("receipt.json"), receipt(true, false)).unwrap();
        let refused = Source::open(&dir, root, "0.1.0").unwrap_err();
        assert!(
            refused.contains("built from tree") || refused.contains("this checkout has"),
            "{refused}"
        );
        // A clean source of this checkout's tree, built as another version.
        let ours = crate::package::receipt::git(root, &["rev-parse", "HEAD^{tree}"]).unwrap();
        std::fs::write(dir.join("receipt.json"), tree(&ours, true, false)).unwrap();
        let refused = Source::open(&dir, root, "0.2.0").unwrap_err();
        assert!(
            refused.contains("built as version 0.1.0, this run builds 0.2.0")
                || refused.contains("this checkout has"),
            "{refused}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
