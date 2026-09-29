//! `receipt.json` of a package: which commit built it, for which target, what is inside, whether
//! the demo and the ROM bytes made it in, which secret rule sets ran, and for Windows what the
//! executable imports. It is host-free: no absolute path, home, user name or device identity.

use std::path::Path;

use serde_json::{Value, json};

use super::demo::{self, Found};
use super::guard;
use super::layout::{self, PayloadDigest};
use super::{account, source, windows};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The workspace version, or the `--version` a release build applied over the checkout.
    pub version: String,
    pub target: String,
    /// Commit the package was built from, or `unknown`.
    pub commit: String,
    /// Whether the work tree had uncommitted changes, when that could be determined. A
    /// `--version` stamp is not counted: this is the checkout before it ([`Checkout`]).
    pub dirty: Option<bool>,
    /// The tree of the commit (`git rev-parse HEAD^{tree}`), or `None`: what a copy of the
    /// checkout on another host keeps, and what `--payload-from` compares ([`source`]).
    pub tree: Option<String>,
    /// The package the wasm core and the demo came from, when `--payload-from` named one.
    pub payload_origin: Option<source::Origin>,
    pub payload: PayloadDigest,
    /// Whether the packaged binary carries the payload. `false` only in the receipt [`layout::write`]
    /// leaves for the embedding build; [`layout::embed_binary`] rewrites it before any archive.
    pub payload_embedded: bool,
    pub demo: DemoRecord,
    /// Per artifact and bundled ROM ELF, whether the ROM bytes are inside it.
    pub roms_embedded: Vec<RomMeasurement>,
    /// What the secret scan of the written package found ([`super::guard`]).
    pub secrets: guard::Verdict,
    /// What the Windows audit read from a Windows executable; `None` for macOS.
    pub windows: Option<windows::Audit>,
    /// What the [`account`] audit found in the binary that ships and the wasm core; `None` only
    /// in the receipt [`layout::write`] leaves for the embedding build.
    pub account: Option<account::Audit>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RomMeasurement {
    /// The artifact measured: `passportsim` (the binary, whatever its file name on the target) or
    /// `pemu_wasm.wasm`.
    pub artifact: String,
    pub rom: String,
    pub embedded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DemoRecord {
    Embedded {
        bin_sha256: String,
        /// SHA-256 of the application ELF the corpus pins.
        elf_sha256: String,
        /// SHA-256 of the application ELF the bundle carries, its build paths blanked.
        elf_shipped_sha256: String,
        /// How many build paths were blanked in it.
        elf_paths_blanked: usize,
        bundle_sha256: String,
        /// Whether the upstream checkout confirmed the pinned commit id, on the host that built
        /// the bundle.
        commit_verified: bool,
        source: demo::Source,
    },
    Absent {
        /// One line, host-free.
        reason: String,
    },
}

impl DemoRecord {
    pub fn summary(&self) -> String {
        match self {
            DemoRecord::Embedded { bundle_sha256, .. } => {
                format!("embedded ({})", &bundle_sha256[..16])
            }
            DemoRecord::Absent { reason } => format!("not embedded: {reason}"),
        }
    }
}

/// The git state of the checkout a package is built from, read before a `--version` stamp
/// touches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// `git rev-parse HEAD`, or `unknown`.
    pub commit: String,
    /// Whether `git status --porcelain` lists anything, when git could say.
    pub dirty: Option<bool>,
    /// `git rev-parse HEAD^{tree}`.
    pub tree: Option<String>,
}

impl Checkout {
    pub fn read(root: &Path) -> Checkout {
        Checkout {
            commit: git(root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string()),
            dirty: git(root, &["status", "--porcelain"]).map(|out| !out.is_empty()),
            tree: git(root, &["rev-parse", "HEAD^{tree}"]),
        }
    }
}

pub fn build(
    root: &Path,
    inputs: &layout::Inputs<'_>,
    payload: &PayloadDigest,
    secrets: guard::Verdict,
) -> Result<Receipt, String> {
    Ok(Receipt {
        secrets,
        windows: None,
        account: None,
        version: inputs.version.to_string(),
        target: inputs.target.to_string(),
        commit: inputs.checkout.commit.clone(),
        dirty: inputs.checkout.dirty,
        tree: inputs.checkout.tree.clone(),
        payload_origin: None,
        payload: payload.clone(),
        payload_embedded: false,
        demo: match inputs.demo {
            Found::Embedded(embedded) => DemoRecord::Embedded {
                bin_sha256: embedded.bin_sha256.clone(),
                elf_sha256: embedded.elf_sha256.clone(),
                elf_shipped_sha256: embedded.elf_shipped_sha256.clone(),
                elf_paths_blanked: embedded.elf_paths_blanked,
                bundle_sha256: sha256_hex(&embedded.bundle),
                commit_verified: embedded.commit.rechecked(),
                source: embedded.source,
            },
            Found::Absent(absent) => DemoRecord::Absent {
                reason: absent.reason.clone(),
            },
        },
        roms_embedded: rom_report(root, inputs)?,
    })
}

const BUNDLED_ROMS: [&str; 2] = ["esp32c3_rev101_rom.elf", "esp32c3_rev3_rom.elf"];

/// Per artifact and bundled ROM ELF, whether the ROM bytes are really in it. An unreadable ROM or
/// artifact fails the packaging rather than reading as "the linker dropped the bytes".
pub fn rom_report(root: &Path, inputs: &layout::Inputs<'_>) -> Result<Vec<RomMeasurement>, String> {
    let dir = root.join("assets/rom");
    let artifacts = [
        ("passportsim", inputs.binary),
        ("pemu_wasm.wasm", inputs.wasm),
    ];
    let mut out = Vec::new();
    for (label, artifact) in artifacts {
        for rom in BUNDLED_ROMS {
            out.push(RomMeasurement {
                artifact: label.to_string(),
                rom: rom.to_string(),
                embedded: layout::roms_embedded(artifact, &dir.join(rom))?,
            });
        }
    }
    Ok(out)
}

impl Receipt {
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "tool": "cargo xtask package",
            "version": self.version,
            "target": self.target,
            "commit": self.commit,
            "tree": self.tree,
            "dirty": self.dirty,
            "payload": {
                "sha256": self.payload.sha256,
                "embedded": self.payload_embedded,
                "from_package": self.payload_origin.as_ref().map(|origin| json!({
                    "target": origin.target,
                    "commit": origin.commit,
                    "payload_sha256": origin.payload_sha256,
                    "taken": ["payload/web/pemu_wasm.wasm", "payload/firmware/*"],
                    "note": "the wasm core and the demo are the macOS package's bytes; every other \
                             payload file was built here, and the whole payload was checked equal \
                             to that package's before anything was packaged",
                })),
                "definition": "SHA-256 over \"<file sha256 hex>  <path>\\n\" for every payload \
                               file, sorted by path; the payload is every file of the package \
                               except the `passportsim` binary of this target and `receipt.json`",
                "files": self.payload.files.iter().map(|file| json!({
                    "path": file.path,
                    "size": file.size,
                    "sha256": file.sha256,
                })).collect::<Vec<_>>(),
            },
            "demo": self.demo_json(),
            "roms": {
                "embedded": self.roms_embedded.iter().map(|row| json!({
                    "artifact": row.artifact,
                    "file": row.rom,
                    "embedded": row.embedded,
                })).collect::<Vec<_>>(),
                "note": "measured in each produced artifact, not asserted: `pemu-loader` embeds \
                         both ROM ELFs behind its default `bundled-rom` feature, and the linker \
                         keeps them only once a reachable caller reads them",
            },
            "secrets": {
                "scan": "cargo xtask secrets-check, run in-process over every file of both \
                         written trees before this receipt was written",
                "files_scanned": self.secrets.files_scanned,
                "pattern_rules": self.secrets.pattern_rules,
                "hashed_rules": self.secrets.hashed_rules,
                "note": "the pattern rules need no local state and run on every packaging host. \
                         The hashed rules need the packaging host's own \
                         ~/.config/passportsim/secrets-check.toml; `hashed_rules` is \
                         false when that host had none, and a package never claims a rule set \
                         that did not run.",
            },
            "distribution": {
                "signed": false,
                "notarized": false,
                "package_manager": false,
                "note": "deferred on both hosts: no Developer ID \
                         signing, notarization or Homebrew tap on macOS; no Authenticode signing \
                         or winget/Scoop manifest on Windows. The per-OS first-launch step is \
                         documented in docs/quickstart.md sections 2 and 8.",
            },
        });
        if let Some(audit) = &self.account {
            value["account_paths"] = json!({
                "remapped": audit.remapped.iter().map(|(what, token)| json!({
                    "prefix": what,
                    "token": token,
                })).collect::<Vec<_>>(),
                "searched": audit.searched,
                "hits": 0,
                "note": "every build of the binary and the wasm core remapped these prefixes of the \
                         packaging account to the tokens, as target rustflags set by `xtask \
                         package` alone, and the demo's application ELF had the account part of \
                         its build paths blanked in place (`demo.elf_paths_blanked`). Every \
                         searched artifact was then read byte for byte, ignoring case, for this \
                         account's home directory (either separator), its user name below a \
                         profile directory, and any account's profile path; the packaging fails \
                         on one hit",
            });
        }
        if let Some(audit) = &self.windows {
            value["windows"] = json!({
                "crt_static": true,
                "imports": audit.imports,
                "delay_imports": audit.delay_imports,
                "redistributable_imports": [],
                "manifest": {
                    "embedded": true,
                    "sha256": audit.manifest_sha256,
                    "long_path_aware": true,
                    "active_code_page": "UTF-8",
                    "supported_os": [windows::SUPPORTED_OS_WINDOWS_10_11],
                    "execution_level": "asInvoker",
                },
                "note": "audited in the executable that ships: \
                         `+crt-static` was set for this target only, the import and delay-load \
                         tables name no vcruntime*.dll or msvcp*.dll (the packaging fails when \
                         they do), and RT_MANIFEST resource 1 is byte for byte the manifest \
                         `xtask package` links. That the loader honors each setting is not \
                         measured here",
            });
        }
        value
    }

    /// The receipt as pretty JSON text with one trailing newline.
    pub fn to_json_text(&self) -> String {
        let mut text =
            serde_json::to_string_pretty(&self.to_json()).unwrap_or_else(|_| "{}".to_string());
        text.push('\n');
        text
    }

    fn demo_json(&self) -> Value {
        match &self.demo {
            DemoRecord::Embedded {
                bin_sha256,
                elf_sha256,
                elf_shipped_sha256,
                elf_paths_blanked,
                bundle_sha256,
                commit_verified,
                source,
            } => json!({
                "embedded": true,
                "corpus_id": demo::DEMO_ID,
                "license": "MIT",
                "upstream_commit": demo::UPSTREAM_COMMIT,
                "upstream_commit_verified": commit_verified,
                "bin_sha256": bin_sha256,
                "elf_sha256": elf_sha256,
                "elf_shipped_sha256": elf_shipped_sha256,
                "elf_paths_blanked": elf_paths_blanked,
                "elf_note": "`elf_sha256` is the corpus pin; the bundle carries that ELF \
                             with the account part of its build paths overwritten with `_` in its \
                             debug sections, in place, every other byte unchanged \
                             (`elf_shipped_sha256`); the merged flash image is the pinned one",
                "bundle_sha256": bundle_sha256,
                "source": source.as_str(),
            }),
            DemoRecord::Absent { reason } => json!({
                "embedded": false,
                "corpus_id": demo::DEMO_ID,
                "reason": reason,
                "note": "the demo image is never committed, so a host \
                         without the firmware corpus packages without it rather than failing",
            }),
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Trimmed stdout of a git command in `root`, or `None`.
pub fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
