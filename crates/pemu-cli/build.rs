//! Embeds the package payload in `passportsim`.
//!
//! `PASSPORTSIM_EMBED_PAYLOAD=<dir>` names a tree `xtask package` produced: every file except the
//! root-level binary of this target and `receipt.json` is payload. The path must be absolute, and
//! the tree's `receipt.json` must record the digest computed here, or the build fails. The variable
//! is the only input on purpose: looking into the data root would be another directory resolver and
//! would embed whatever an earlier packaging run left. Nothing is read from the repository.
//!
//! Without the variable nothing is embedded and `passportsim --version` says why. A variable that
//! names no directory fails the build: that is a packaging run about to ship without its payload.
//!
//! The digest, as `xtask/src/package/layout.rs` computes it:
//!
//! ```text
//! SHA-256( concat over files, sorted by `/`-separated relative path, of "<file sha256 hex>  <path>\n" )
//! ```
//!
//! `src/payload.rs` recomputes it over the embedded bytes in a test.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const PAYLOAD_ENV: &str = "PASSPORTSIM_EMBED_PAYLOAD";

const RECEIPT_FILE: &str = "receipt.json";

/// The binary of this target and the receipt, as the receipt's `payload.definition` says.
/// `CARGO_CFG_TARGET_OS` names the target, not the host running this script.
fn not_payload(path: &str) -> bool {
    let binary = if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        "passportsim.exe"
    } else {
        "passportsim"
    };
    path == binary || path == RECEIPT_FILE
}

fn main() {
    println!("cargo:rerun-if-env-changed={PAYLOAD_ENV}");
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let blob_path = out.join("payload.bin");
    let module = match env::var_os(PAYLOAD_ENV).filter(|value| !value.is_empty()) {
        None => {
            write(&blob_path, &[]);
            absent_module(&format!(
                "development build: no payload was embedded ({PAYLOAD_ENV} was not set when this \
                 binary was built); set it to a package tree that `cargo run -q -p xtask -- package` \
                 wrote to embed one"
            ))
        }
        Some(dir) => {
            let dir = Path::new(&dir);
            if !dir.is_absolute() {
                panic!(
                    "{PAYLOAD_ENV}={} is relative; it must be an absolute path, because a build \
                     script runs from crates/pemu-cli and not from where cargo was invoked",
                    dir.display()
                );
            }
            embed(dir, &blob_path)
        }
    };
    write(&out.join("payload.rs"), module.as_bytes());
}

/// Returns the generated module.
fn embed(dir: &Path, blob_path: &Path) -> String {
    if !dir.is_dir() {
        panic!(
            "{PAYLOAD_ENV}={} names no directory; unset it for a development build",
            dir.display()
        );
    }
    let mut files = Vec::new();
    collect(dir, dir, &mut files);
    files.retain(|(path, _)| !not_payload(path));
    files.sort_by(|a, b| a.0.cmp(&b.0));
    if files.is_empty() {
        panic!(
            "{PAYLOAD_ENV}={} holds no payload file; it must name a package tree",
            dir.display()
        );
    }
    let mut blob = Vec::new();
    let mut index = String::new();
    let mut listing = String::new();
    for (path, file) in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let bytes = fs::read(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let sha = pemu_loader::hex(&pemu_loader::sha256(&bytes));
        let _ = writeln!(
            index,
            "    EmbeddedFile {{ path: {path:?}, offset: {}, len: {}, sha256: {sha:?} }},",
            blob.len(),
            bytes.len()
        );
        let _ = writeln!(listing, "{sha}  {path}");
        blob.extend_from_slice(&bytes);
    }
    let digest = pemu_loader::hex(&pemu_loader::sha256(listing.as_bytes()));
    check_receipt(dir, &digest);
    write(blob_path, &blob);
    format!(
        "/// The payload digest, computed by `build.rs`.\n\
         pub const DIGEST: Option<&str> = Some({digest:?});\n\
         /// Why nothing is embedded, when nothing is.\n\
         pub const ABSENT: Option<&str> = None;\n\
         /// Every payload file, concatenated in path order.\n\
         pub static BLOB: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/payload.bin\"));\n\
         /// One row per file of [`BLOB`], sorted by path.\n\
         pub static FILES: &[EmbeddedFile] = &[\n{index}];\n"
    )
}

/// A variable pointed at the wrong directory would otherwise embed a tree `xtask package` never
/// secret-scanned.
fn check_receipt(dir: &Path, digest: &str) {
    let file = dir.join(RECEIPT_FILE);
    println!("cargo:rerun-if-changed={}", file.display());
    let text = fs::read_to_string(&file).unwrap_or_else(|e| {
        panic!(
            "{PAYLOAD_ENV}={} has no readable {RECEIPT_FILE} ({e}); it must name a package tree \
             that `cargo run -q -p xtask -- package` wrote",
            dir.display()
        )
    });
    let recorded = receipt_payload_sha256(&text).unwrap_or_else(|| {
        panic!(
            "{} records no payload.sha256; it is not a package receipt",
            file.display()
        )
    });
    if recorded != digest {
        panic!(
            "{PAYLOAD_ENV}={}: the files hash to payload digest {digest}, but {RECEIPT_FILE} \
             records {recorded}; the tree changed after it was packaged",
            dir.display()
        );
    }
}

/// A build script may not depend on `serde_json`, so this small reader follows the two keys and
/// skips everything else, escaped strings included.
fn receipt_payload_sha256(text: &str) -> Option<String> {
    let mut json = Json {
        bytes: text.as_bytes(),
        at: 0,
    };
    let mut found = None;
    json.object(&mut |json, key| {
        if key != "payload" {
            return json.skip();
        }
        json.object(&mut |json, key| {
            if key == "sha256" {
                found = Some(json.string()?);
                Some(())
            } else {
                json.skip()
            }
        })
    })?;
    found
}

struct Json<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Json<'_> {
    fn space(&mut self) {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        self.space();
        (self.bytes.get(self.at) == Some(&byte)).then(|| self.at += 1)
    }

    /// Calls `field` with the cursor on each value.
    fn object(&mut self, field: &mut dyn FnMut(&mut Self, String) -> Option<()>) -> Option<()> {
        self.eat(b'{')?;
        if self.eat(b'}').is_some() {
            return Some(());
        }
        loop {
            let key = self.string()?;
            self.eat(b':')?;
            field(self, key)?;
            if self.eat(b',').is_none() {
                return self.eat(b'}');
            }
        }
    }

    /// Escapes other than `\uXXXX` are decoded, which is all a digest or a key needs.
    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        loop {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let escaped = *self.bytes.get(self.at)?;
                    self.at += 1;
                    match escaped {
                        b'u' => self.at += 4,
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn skip(&mut self) -> Option<()> {
        self.space();
        match *self.bytes.get(self.at)? {
            b'"' => self.string().map(|_| ()),
            b'{' => self.object(&mut |json, _| json.skip()),
            b'[' => {
                self.at += 1;
                if self.eat(b']').is_some() {
                    return Some(());
                }
                loop {
                    self.skip()?;
                    if self.eat(b',').is_none() {
                        return self.eat(b']');
                    }
                }
            }
            _ => {
                while self
                    .bytes
                    .get(self.at)
                    .is_some_and(|b| !matches!(b, b',' | b'}' | b']') && !b.is_ascii_whitespace())
                {
                    self.at += 1;
                }
                Some(())
            }
        }
    }
}

fn absent_module(reason: &str) -> String {
    format!(
        "/// The payload digest; `None` because nothing is embedded.\n\
         pub const DIGEST: Option<&str> = None;\n\
         /// Why nothing is embedded.\n\
         pub const ABSENT: Option<&str> = Some({reason:?});\n\
         /// Empty: nothing is embedded.\n\
         pub static BLOB: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/payload.bin\"));\n\
         /// Empty: nothing is embedded.\n\
         pub static FILES: &[EmbeddedFile] = &[];\n"
    )
}

/// As (`/`-separated path relative to `base`, absolute path).
fn collect(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .path();
        if path.is_dir() {
            collect(base, &path, out);
            continue;
        }
        let relative = path
            .strip_prefix(base)
            .expect("walked below the base")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push((relative, path));
    }
}

/// Only when they differ, so an unchanged payload relinks nothing.
fn write(path: &Path, bytes: &[u8]) {
    if fs::read(path).is_ok_and(|old| old == bytes) {
        return;
    }
    fs::write(path, bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}
