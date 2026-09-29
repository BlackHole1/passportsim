//! The package payload inside the binary: where this process takes it from and what its digest is.
//! `build.rs` embeds the tree `PASSPORTSIM_EMBED_PAYLOAD` names.
//!
//! Precedence:
//!
//! 1. the embedded copy, whenever this build has one;
//! 2. the package directory beside the binary, only in a build with nothing embedded, and only when
//!    every payload file hashes to the digest its `receipt.json` records;
//! 3. otherwise no payload, with the reason.
//!
//! A directory matching the embedded digest holds the bytes already in the executable, and checking
//! it would hash tens of MB on every start, so it is not preferred. A directory that does not match
//! is never used, so a stale or edited tree cannot win.
//!
//! A plain `cargo build` embeds nothing and compiles anyway; [`this_process`] reports why, and
//! `passportsim --version` prints it.

use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

use crate::json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbeddedFile {
    pub path: &'static str,
    pub offset: usize,
    pub len: usize,
    /// Lowercase hex.
    pub sha256: &'static str,
}

/// `DIGEST`, `ABSENT`, `BLOB` and `FILES`.
mod generated {
    use super::EmbeddedFile;
    include!(concat!(env!("OUT_DIR"), "/payload.rs"));
}

/// Written beside the binary by `xtask package`.
pub const RECEIPT_FILE: &str = "receipt.json";

/// See `xtask/src/package/layout.rs`.
pub const WEB_DIR: &str = "payload/web";

/// Present when the packaging host had the corpus to build it from.
pub const FIRMWARE_DIR: &str = "payload/firmware";

/// The asset half of the daemon's static seam. The name has already passed
/// `pemu_host::webui::safe_name`, so the join can only name a file directly inside [`WEB_DIR`]. A
/// build with no payload installs this too and answers nothing, which the route turns into a 404.
pub fn web_assets(payload: Result<Payload, String>) -> pemu_host::webui::AssetFn {
    Box::new(move |name: &str| {
        let payload = payload.as_ref().ok()?;
        let bytes = payload.read(&format!("{WEB_DIR}/{name}"))?;
        Some(pemu_host::webui::Asset {
            content_type: pemu_host::webui::content_type(name),
            bytes: bytes.into_owned(),
        })
    })
}

/// Opening the page boots the bundled demo. `serve` asks the corpus first and this second, so an
/// owner's own image wins over the shipped copy, and a machine with no corpus still serves the
/// demo.
pub fn payload_bundles(payload: Result<Payload, String>) -> pemu_host::webui::BundleFn {
    Box::new(move |id: &str| bundle_of(&payload, id))
}

/// The same seam for `start` with no image. One lookup for the page and the CLI, so they boot the
/// same bytes.
pub fn firmware_bundles(payload: Result<Payload, String>) -> pemu_host::backend::PayloadBundles {
    std::sync::Arc::new(move |id: &str| bundle_of(&payload, id))
}

/// `id` is checked here: `start` takes whatever a person or an agent typed, and the join must name
/// one file directly inside [`FIRMWARE_DIR`].
fn bundle_of(payload: &Result<Payload, String>, id: &str) -> Option<Vec<u8>> {
    let id = pemu_host::webui::safe_name(id)?;
    let payload = payload.as_ref().ok()?;
    let bytes = payload.read(&format!("{FIRMWARE_DIR}/{id}.pebundle"))?;
    Some(bytes.into_owned())
}

/// `passportsim`, or `passportsim.exe` on Windows.
pub fn binary_name() -> String {
    format!("passportsim{}", std::env::consts::EXE_SUFFIX)
}

/// Exactly the binary of this target and the receipt. A `passportsim.exe` in a macOS package is
/// payload, as it is to `xtask`.
pub fn not_payload(path: &str) -> bool {
    path == binary_name() || path == RECEIPT_FILE
}

#[derive(Clone, Copy, Debug)]
pub struct Embedded {
    digest: &'static str,
    blob: &'static [u8],
    files: &'static [EmbeddedFile],
}

impl Embedded {
    pub fn of_this_build() -> Result<Embedded, &'static str> {
        match generated::DIGEST {
            Some(digest) => Ok(Embedded {
                digest,
                blob: generated::BLOB,
                files: generated::FILES,
            }),
            None => Err(generated::ABSENT.unwrap_or("no payload was embedded")),
        }
    }

    /// For tests of the resolution order.
    #[cfg(test)]
    pub fn from_parts(
        digest: &'static str,
        blob: &'static [u8],
        files: &'static [EmbeddedFile],
    ) -> Embedded {
        Embedded {
            digest,
            blob,
            files,
        }
    }

    pub fn digest(&self) -> &'static str {
        self.digest
    }

    pub fn file(&self, path: &str) -> Option<&'static [u8]> {
        let index = self
            .files
            .binary_search_by(|file| file.path.cmp(path))
            .ok()?;
        let file = self.files[index];
        self.blob.get(file.offset..file.offset + file.len)
    }

    /// `None` when a file's bytes do not hash to the row `build.rs` wrote for it.
    pub fn recomputed_digest(&self) -> Option<String> {
        let mut rows = Vec::with_capacity(self.files.len());
        for file in self.files {
            let bytes = self.blob.get(file.offset..file.offset + file.len)?;
            let sha = pemu_loader::hex(&pemu_loader::sha256(bytes));
            if sha != file.sha256 {
                return None;
            }
            rows.push((sha, file.path.to_owned()));
        }
        Some(digest_of(rows))
    }
}

/// As `xtask`'s `package::layout::digest` defines it: SHA-256 over `"<sha>  <path>\n"` rows sorted
/// by path.
pub fn digest_of(mut rows: Vec<(String, String)>) -> String {
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    let mut listing = String::new();
    for (sha, path) in &rows {
        listing.push_str(sha);
        listing.push_str("  ");
        listing.push_str(path);
        listing.push('\n');
    }
    pemu_loader::hex(&pemu_loader::sha256(listing.as_bytes()))
}

/// Every file except those [`not_payload`] names at the root, sorted by path.
pub fn directory_rows(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut rows = Vec::new();
    walk(dir, dir, &mut rows)?;
    rows.retain(|(_, path)| !not_payload(path));
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(rows)
}

#[cfg(test)]
pub fn directory_digest(dir: &Path) -> Result<String, String> {
    Ok(digest_of(directory_rows(dir)?))
}

/// Errors name the path relative to `base`, never the absolute one: the message reaches
/// `passportsim --version`, which names no host path.
fn walk(base: &Path, dir: &Path, rows: &mut Vec<(String, String)>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", relative(base, dir)))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("{}: {e}", relative(base, dir)))?
            .path();
        if path.is_dir() {
            walk(base, &path, rows)?;
            continue;
        }
        let name = relative(base, &path);
        let bytes = fs::read(&path).map_err(|e| format!("{name}: {e}"))?;
        rows.push((pemu_loader::hex(&pemu_loader::sha256(&bytes)), name));
    }
    Ok(())
}

/// `.` for `base` itself. A path not below `base` is named by its file name alone.
fn relative(base: &Path, path: &Path) -> String {
    let Ok(rest) = path.strip_prefix(base) else {
        return path
            .file_name()
            .map_or_else(|| "?".to_owned(), |n| n.to_string_lossy().into_owned());
    };
    let joined = rest
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

/// `Ok(None)` without a receipt, an error for one without a digest.
pub fn receipt_digest(dir: &Path) -> Result<Option<String>, String> {
    let file = dir.join(RECEIPT_FILE);
    let text = match fs::read(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{RECEIPT_FILE}: {e}")),
    };
    let value: Value = serde_json::from_slice(&text).map_err(|e| format!("{RECEIPT_FILE}: {e}"))?;
    value["payload"]["sha256"]
        .as_str()
        .map(|digest| Some(digest.to_owned()))
        .ok_or_else(|| format!("{RECEIPT_FILE} records no payload.sha256"))
}

#[allow(
    dead_code,
    reason = "read by the consumers of a payload file: serving `payload/web` and the embedded \
              demo of `run` with no image"
)]
#[derive(Clone, Debug)]
pub enum Source {
    Embedded(Embedded),
    /// Beside the binary, verified against its receipt when resolved.
    Directory {
        /// The package directory.
        dir: PathBuf,
        /// A read serves only these rows, and only bytes that still hash to them: the directory can
        /// change after [`resolve`] looked at it.
        files: Vec<(String, String)>,
    },
}

#[derive(Clone, Debug)]
pub struct Payload {
    pub source: Source,
    /// Equal to the receipt's.
    pub digest: String,
}

impl Payload {
    /// Such as `payload/web/index.html`.
    pub fn read(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        match &self.source {
            Source::Embedded(embedded) => embedded.file(path).map(Cow::Borrowed),
            Source::Directory { dir, files } => {
                let index = files
                    .binary_search_by(|(_, row)| row.as_str().cmp(path))
                    .ok()?;
                let (sha, row) = &files[index];
                let bytes = fs::read(dir.join(row)).ok()?;
                (pemu_loader::hex(&pemu_loader::sha256(&bytes)) == *sha)
                    .then_some(Cow::Owned(bytes))
            }
        }
    }

    /// The line `passportsim --version` prints; it names no host path. An embedded copy is
    /// re-hashed here and only here, which is where a damaged executable is found out and what
    /// keeps the linker from dropping the bytes.
    pub fn describe(resolved: &Result<Payload, String>) -> String {
        match resolved {
            Ok(Payload {
                source: Source::Embedded(embedded),
                digest,
            }) => match embedded.recomputed_digest() {
                Some(bytes) if bytes == *digest => format!("payload: embedded, sha256 {digest}"),
                _ => format!(
                    "payload: embedded, damaged: the embedded bytes do not hash to the recorded \
                     sha256 {digest}"
                ),
            },
            Ok(Payload {
                source: Source::Directory { .. },
                digest,
            }) => format!("payload: package directory beside the binary, sha256 {digest}"),
            Err(reason) => format!("payload: none: {reason}"),
        }
    }
}

/// `beside` is the executable's directory, when it could be determined.
pub fn resolve(embedded: Result<Embedded, &str>, beside: Option<&Path>) -> Result<Payload, String> {
    let absent = match embedded {
        Ok(embedded) => {
            return Ok(Payload {
                digest: embedded.digest().to_owned(),
                source: Source::Embedded(embedded),
            });
        }
        Err(reason) => reason,
    };
    let Some(dir) = beside else {
        return Err(format!(
            "{absent}; the directory of this executable could not be determined"
        ));
    };
    let Some(want) = receipt_digest(dir).map_err(|e| format!("{absent}; {e}"))? else {
        return Err(format!(
            "{absent}; and no package {RECEIPT_FILE} is beside the binary"
        ));
    };
    let files = directory_rows(dir).map_err(|e| format!("{absent}; {e}"))?;
    let got = digest_of(files.clone());
    if got != want {
        return Err(format!(
            "{absent}; the package files beside the binary do not match its {RECEIPT_FILE} \
             (receipt {want}, files {got}), so they are not used"
        ));
    }
    Ok(Payload {
        source: Source::Directory {
            dir: dir.to_path_buf(),
            files,
        },
        digest: got,
    })
}

/// Symbolic links resolved, so a link to a packaged binary finds its package.
pub fn this_process() -> Result<Payload, String> {
    let beside = std::env::current_exe()
        .and_then(fs::canonicalize)
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    resolve(Embedded::of_this_build(), beside.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removed first if a previous run left it.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pemu-cli-payload-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn put(dir: &Path, path: &str, bytes: &[u8]) {
        let file = dir.join(path);
        fs::create_dir_all(file.parent().expect("has a parent")).expect("mkdir");
        fs::write(file, bytes).expect("write");
    }

    /// Two payload files, a binary and a receipt.
    fn package(name: &str, receipt: &str) -> PathBuf {
        let dir = scratch(name);
        put(&dir, "payload/web/index.html", b"<html>");
        put(&dir, "LICENSE", b"MIT");
        put(&dir, &binary_name(), b"not payload");
        put(
            &dir,
            RECEIPT_FILE,
            format!(r#"{{"payload":{{"sha256":"{receipt}"}}}}"#).as_bytes(),
        );
        dir
    }

    /// Computed outside this code, so the definition is pinned and not only self-consistent.
    const TWO_FILE_DIGEST: &str =
        "fffe4d1f82f127af57397f21857ce64baeeb906a3cd036d054d770d8684c2bb7";

    #[test]
    fn the_static_seam_serves_payload_files_and_no_other_path() {
        let dir = scratch("seam");
        put(&dir, "payload/web/index.html", b"<html>ui</html>");
        put(&dir, "payload/web/pemu_wasm.wasm", b"\0asm");
        put(&dir, "payload/firmware/official.pebundle", b"PEBUNDLE-ish");
        put(&dir, "LICENSE", b"MIT");
        let rows = directory_rows(&dir).expect("rows");
        let payload = Ok(Payload {
            digest: digest_of(rows.clone()),
            source: Source::Directory {
                dir: dir.clone(),
                files: rows,
            },
        });

        let assets = web_assets(payload.clone());
        let index = assets("index.html").expect("the payload carries the UI document");
        assert_eq!(index.bytes, b"<html>ui</html>");
        assert_eq!(index.content_type, "text/html; charset=utf-8");
        assert_eq!(
            assets("pemu_wasm.wasm").expect("the core").content_type,
            "application/wasm"
        );
        // Only `payload/web` is reachable.
        for name in [
            "LICENSE",
            "receipt.json",
            "official.pebundle",
            "payload",
            "nothing.js",
        ] {
            assert!(assets(name).is_none(), "{name} is not a web asset");
        }

        let bundles = payload_bundles(payload);
        assert_eq!(
            bundles("official").expect("the packaged demo"),
            b"PEBUNDLE-ish".to_vec()
        );
        for id in ["pk", "index.html", "LICENSE"] {
            assert!(bundles(id).is_none(), "{id} is no packaged firmware");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_build_without_a_payload_serves_no_asset_and_no_firmware() {
        let absent: Result<Payload, String> = Err("no payload was embedded".to_string());
        assert!(web_assets(absent.clone())("index.html").is_none());
        assert!(payload_bundles(absent)("official").is_none());
    }

    #[test]
    fn the_digest_is_the_receipt_definition() {
        let dir = package("definition", "unused");
        assert_eq!(directory_digest(&dir).expect("digest"), TWO_FILE_DIGEST);
        // Not payload, so changing them changes nothing.
        put(&dir, &binary_name(), b"another build");
        put(&dir, RECEIPT_FILE, b"{}");
        assert_eq!(directory_digest(&dir).expect("digest"), TWO_FILE_DIGEST);
        // The other host's binary name is payload, as it is to `xtask`.
        let other = if cfg!(windows) {
            "passportsim"
        } else {
            "passportsim.exe"
        };
        put(&dir, other, b"x");
        assert_ne!(directory_digest(&dir).expect("digest"), TWO_FILE_DIGEST);
        fs::remove_file(dir.join(other)).expect("rm");
        // A payload file is.
        put(&dir, "LICENSE", b"MIT2");
        assert_ne!(directory_digest(&dir).expect("digest"), TWO_FILE_DIGEST);
        let _ = fs::remove_dir_all(dir);
    }

    /// Laid out the way `build.rs` does it.
    static TWO_FILES: [EmbeddedFile; 2] = [
        EmbeddedFile {
            path: "LICENSE",
            offset: 0,
            len: 3,
            sha256: "e5dcffe836b6ec8a58e492419b550e65fb8cbdc308503979e5dacb33ac7ea3b7",
        },
        EmbeddedFile {
            path: "payload/web/index.html",
            offset: 3,
            len: 6,
            sha256: "b7d082ee12e91b756ea22e8513b8594eebcf5d39fab813da3cb55794dc888ad7",
        },
    ];

    fn two_files() -> Embedded {
        Embedded::from_parts(TWO_FILE_DIGEST, b"MIT<html>", &TWO_FILES)
    }

    #[test]
    fn an_embedded_payload_recomputes_to_its_digest_and_serves_its_files() {
        let embedded = two_files();
        assert_eq!(
            embedded.recomputed_digest().as_deref(),
            Some(TWO_FILE_DIGEST)
        );
        assert_eq!(
            embedded.file("payload/web/index.html"),
            Some(&b"<html>"[..])
        );
        assert_eq!(embedded.file("LICENSE"), Some(&b"MIT"[..]));
        assert_eq!(embedded.file("receipt.json"), None);
    }

    #[test]
    fn an_embedded_row_whose_bytes_do_not_hash_to_it_is_caught() {
        let embedded = Embedded::from_parts(TWO_FILE_DIGEST, b"MIX<html>", &TWO_FILES);
        assert_eq!(embedded.recomputed_digest(), None);
    }

    #[test]
    fn this_builds_embedded_payload_recomputes_to_its_recorded_digest() {
        match Embedded::of_this_build() {
            Ok(embedded) => assert_eq!(
                embedded.recomputed_digest().as_deref(),
                Some(embedded.digest()),
                "build.rs and the run-time digest disagree"
            ),
            // Only a build with PASSPORTSIM_EMBED_PAYLOAD unset has nothing to recompute.
            Err(reason) => {
                assert!(reason.contains("PASSPORTSIM_EMBED_PAYLOAD"), "{reason}");
                println!(
                    "SKIP this_builds_embedded_payload_recomputes_to_its_recorded_digest: {reason}"
                );
            }
        }
    }

    #[test]
    fn a_moved_binary_uses_its_embedded_copy() {
        let resolved = resolve(Ok(two_files()), None).expect("embedded");
        assert!(matches!(resolved.source, Source::Embedded(_)));
        assert_eq!(resolved.digest, TWO_FILE_DIGEST);
        assert_eq!(
            resolved.read("payload/web/index.html").as_deref(),
            Some(&b"<html>"[..])
        );
    }

    #[test]
    fn the_embedded_copy_wins_over_any_directory_beside_the_binary() {
        // Neither a matching nor an edited directory is read.
        for (name, edit) in [("matching", false), ("edited", true)] {
            let dir = package(name, TWO_FILE_DIGEST);
            if edit {
                put(&dir, "payload/web/index.html", b"<script>");
            }
            let resolved = resolve(Ok(two_files()), Some(&dir)).expect("embedded");
            assert!(matches!(resolved.source, Source::Embedded(_)), "{name}");
            assert_eq!(
                resolved.read("payload/web/index.html").as_deref(),
                Some(&b"<html>"[..]),
                "{name}"
            );
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn without_an_embedded_copy_a_directory_is_used_only_when_it_matches_its_receipt() {
        let absent = "development build: PASSPORTSIM_EMBED_PAYLOAD unset";
        let dir = package("fallback", TWO_FILE_DIGEST);
        let resolved = resolve(Err(absent), Some(&dir)).expect("matches its receipt");
        assert!(matches!(resolved.source, Source::Directory { .. }));
        assert_eq!(resolved.digest, TWO_FILE_DIGEST);
        assert_eq!(resolved.read("LICENSE").as_deref(), Some(&b"MIT"[..]));
        assert_eq!(resolved.read("receipt.json"), None, "not payload");
        assert_eq!(resolved.read("../x"), None);
        assert_eq!(
            resolved.read(&dir.join("LICENSE").to_string_lossy()),
            None,
            "absolute"
        );
        assert_eq!(resolved.read("C:\\x"), None);

        // Neither a new nor an edited file is served; an unchanged one still is.
        put(&dir, "payload/web/added.js", b"added");
        assert_eq!(
            resolved.read("payload/web/added.js"),
            None,
            "added after resolve"
        );
        put(&dir, "payload/web/index.html", b"<script>");
        assert_eq!(
            resolved.read("payload/web/index.html"),
            None,
            "edited after resolve"
        );
        assert_eq!(resolved.read("LICENSE").as_deref(), Some(&b"MIT"[..]));
        put(&dir, "payload/web/index.html", b"<html>");
        fs::remove_file(dir.join("payload/web/added.js")).expect("rm");

        put(&dir, "LICENSE", b"edited");
        let refused = resolve(Err(absent), Some(&dir)).expect_err("edited");
        assert!(refused.contains("do not match"), "{refused}");
        assert!(refused.contains(absent), "{refused}");

        fs::remove_file(dir.join(RECEIPT_FILE)).expect("rm receipt");
        let refused = resolve(Err(absent), Some(&dir)).expect_err("no receipt");
        assert!(refused.contains("no package receipt.json"), "{refused}");
        let _ = fs::remove_dir_all(dir);

        let refused = resolve(Err(absent), None).expect_err("no directory");
        assert!(refused.starts_with(absent), "{refused}");
    }

    /// The refusal keeps the development-build reason and names the file relative to the package.
    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_read_is_refused_without_a_host_path() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let absent = "development build: PASSPORTSIM_EMBED_PAYLOAD unset";
        let dir = package("unreadable", TWO_FILE_DIGEST);
        let host = dir.to_string_lossy().into_owned();

        let locked = dir.join("payload/web/index.html");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        let refused = resolve(Err(absent), Some(&dir)).expect_err("unreadable");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(refused.starts_with(&format!("{absent}; ")), "{refused}");
        assert!(refused.contains("payload/web/index.html: "), "{refused}");
        assert!(!refused.contains(&host), "{refused}");

        symlink("loop", dir.join("loop")).expect("symlink");
        let refused = resolve(Err(absent), Some(&dir)).expect_err("loop");
        assert!(
            refused.starts_with(&format!("{absent}; loop: ")),
            "{refused}"
        );
        assert!(!refused.contains(&host), "{refused}");

        fs::remove_file(dir.join("loop")).expect("rm loop");
        fs::write(dir.join(RECEIPT_FILE), b"not json").expect("write");
        let refused = resolve(Err(absent), Some(&dir)).expect_err("bad receipt");
        assert!(
            refused.starts_with(&format!("{absent}; receipt.json: ")),
            "{refused}"
        );
        assert!(!refused.contains(&host), "{refused}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_version_line_names_the_source_and_no_path() {
        let dir = package("describe", TWO_FILE_DIGEST);
        let line = Payload::describe(&resolve(Err("x"), Some(&dir)));
        assert_eq!(
            line,
            format!("payload: package directory beside the binary, sha256 {TWO_FILE_DIGEST}")
        );
        let _ = fs::remove_dir_all(dir);
        let line = Payload::describe(&resolve(Ok(two_files()), None));
        assert_eq!(line, format!("payload: embedded, sha256 {TWO_FILE_DIGEST}"));
        let damaged = Embedded::from_parts(TWO_FILE_DIGEST, b"MIX<html>", &TWO_FILES);
        let line = Payload::describe(&resolve(Ok(damaged), None));
        assert!(line.starts_with("payload: embedded, damaged"), "{line}");
    }
}
