//! `passportsim doctor`: what the emulator found, and whether it is what the pins say.
//!
//! Discovery reads the file system, so it lives in `pemu-host::assets`; the CLI runs it and hands
//! the result in as a [`Report`], typed ([`run_report`]) or as the `report` argument. Only paths,
//! digests and statuses are printed, never file contents.
//!
//! Failures (everything else is reported and exits 0):
//!
//! 1. a discovery problem the CLI recorded, such as `PASSPORTSIM_ROM` naming a missing file, with
//!    its code (`E_ASSET_MISSING`, `E_ASSET_HASH`);
//! 2. a bundled ROM or corpus file whose digest differs from its pin: `E_ASSET_HASH`;
//! 3. no bundled ROM and no override, as in a `--no-default-features` build: `E_ASSET_MISSING`.

use core::fmt::Write as _;

use crate::error::{ApiError, E_ASSET_HASH, E_ASSET_MISSING, E_INTERNAL, E_USAGE, ErrorCode};
use crate::output::Output;
use crate::receipt::Receipt;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The digest matches the pin, or none is pinned.
    Found,
    Missing,
    Mismatched,
}

impl Status {
    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Found => "found",
            Status::Missing => "missing",
            Status::Mismatched => "mismatched",
        }
    }
}

/// `assets/rom/pins.toml` pins both ELFs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BundledRom {
    pub file: String,
    pub rev: String,
    /// Lower-case hex.
    pub pinned_sha256: String,
    /// Absent when the binary carries none.
    pub embedded_sha256: Option<String>,
    pub chip_revisions: Vec<String>,
}

impl BundledRom {
    /// Mismatched compares the embedded bytes against the pin.
    pub fn status(&self) -> Status {
        match &self.embedded_sha256 {
            None => Status::Missing,
            Some(sha) if *sha == self.pinned_sha256 => Status::Found,
            Some(_) => Status::Mismatched,
        }
    }
}

/// From `--rom`, `--rom idf`, `PASSPORTSIM_ROM`, or config `rom.rev101` and `rom.rev3`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RomOverride {
    /// As `pemu_host::assets::RomOrigin::as_str` names it.
    pub origin: String,
    pub path: Option<String>,
    pub sha256: String,
    /// False only with `--allow-unpinned-rom`; the receipt then says `rom: unpinned` and the
    /// ROM-hash-keyed hooks stay off.
    pub pinned: bool,
    pub pinned_rev: Option<String>,
}

/// Read only so `doctor` can warn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EspRomElfFile {
    pub name: String,
    pub sha256: String,
    pub pinned: bool,
}

/// Never searched for a run: only `--rom idf` and this report look at it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EspRomElfs {
    pub dir: String,
    pub files: Vec<EspRomElfFile>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CorpusFile {
    /// `bin`, `elf`, `boot_elf` or `pt`.
    pub kind: String,
    pub path: String,
    pub found: bool,
    /// Pinned in `corpus.toml`.
    pub expected_sha256: Option<String>,
    /// `None` when discovery did not hash it.
    pub sha256: Option<String>,
}

impl CorpusFile {
    /// A file with no pinned digest, or one discovery did not hash, counts as found: `Corpus::read`
    /// checks the digest when the bytes are used.
    pub fn status(&self) -> Status {
        if !self.found {
            return Status::Missing;
        }
        match (&self.expected_sha256, &self.sha256) {
            (Some(want), Some(have)) if want != have => Status::Mismatched,
            _ => Status::Found,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CorpusEntry {
    pub id: String,
    pub files: Vec<CorpusFile>,
}

impl CorpusEntry {
    /// Mismatched if any file is, else missing if any file is, else found. No files at all is
    /// missing.
    pub fn status(&self) -> Status {
        if self.files.is_empty() {
            return Status::Missing;
        }
        let mut status = Status::Found;
        for file in &self.files {
            match file.status() {
                Status::Mismatched => return Status::Mismatched,
                Status::Missing => status = Status::Missing,
                Status::Found => {}
            }
        }
        status
    }
}

/// The prebuilt official BSP demo (corpus id `official`), which release binaries and the web bundle
/// carry and a plain `cargo build` does not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmbeddedDemo {
    pub id: String,
    pub sha256: String,
    /// 0 when the caller does not report it.
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Problem {
    /// Such as `E_ASSET_MISSING`.
    pub code: String,
    pub message: String,
}

/// What a bug report needs first and no asset status says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFacts {
    /// Such as `aarch64-apple-darwin`.
    pub target: String,
    /// `Err` says why it could not be read.
    pub os_version: Result<String, String>,
    /// In the directory-role table's order.
    pub roles: Vec<HostRole>,
    /// The Windows `LocalDumps` policy, which can still write a crash dump of this process. `None`
    /// on a host with no such policy.
    pub local_dumps: Option<Result<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRole {
    /// `home`, `config`, `data`, `cache`, `run`, `logs` or `artifacts`.
    pub role: String,
    /// `Err` says why the role did not resolve.
    pub path: Result<String, String>,
}

/// What discovery found; `pemu-api` never runs discovery itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// `None` from a caller that did not collect it.
    pub host: Option<HostFacts>,
    pub bundled_roms: Vec<BundledRom>,
    pub rom_override: Option<RomOverride>,
    /// Absent in a plain `cargo build`.
    pub demo: Option<EmbeddedDemo>,
    pub esp_rom_elfs: Vec<EspRomElfs>,
    /// Whether or not it exists.
    pub config_file: String,
    pub corpus_file: String,
    pub corpus: Vec<CorpusEntry>,
    /// Reported, never a failure: a stale esp-rom-elfs copy, an unpinned override, a missing corpus
    /// map.
    pub warnings: Vec<String>,
    /// `doctor` returns the first one's code.
    pub problems: Vec<Problem>,
}

fn usage(what: &str, detail: &str) -> ApiError {
    ApiError::new(E_USAGE, format!("doctor argument `{what}`: {detail}"))
        .with_hint("the CLI builds `report` from pemu_host::assets::discovery_report")
}

fn object<'a>(value: &'a serde_json::Value, at: &str) -> Result<&'a JsonMap, ApiError> {
    value
        .as_object()
        .ok_or_else(|| usage(at, "expected an object"))
}

fn req_str(obj: &JsonMap, key: &str, at: &str) -> Result<String, ApiError> {
    match obj.get(key) {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(usage(&format!("{at}.{key}"), "expected a string")),
        None => Err(usage(at, &format!("missing `{key}`"))),
    }
}

/// Absent or `null` gives `None`.
fn opt_str(obj: &JsonMap, key: &str, at: &str) -> Result<Option<String>, ApiError> {
    match obj.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(usage(&format!("{at}.{key}"), "expected a string or null")),
    }
}

fn str_or_empty(obj: &JsonMap, key: &str, at: &str) -> Result<String, ApiError> {
    Ok(opt_str(obj, key, at)?.unwrap_or_default())
}

fn req_bool(obj: &JsonMap, key: &str, at: &str) -> Result<bool, ApiError> {
    match obj.get(key) {
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(_) => Err(usage(&format!("{at}.{key}"), "expected a boolean")),
        None => Err(usage(at, &format!("missing `{key}`"))),
    }
}

/// Absent or `null` is empty.
fn array<'a>(obj: &'a JsonMap, key: &str, at: &str) -> Result<&'a [serde_json::Value], ApiError> {
    match obj.get(key) {
        None | Some(serde_json::Value::Null) => Ok(&[]),
        Some(serde_json::Value::Array(a)) => Ok(a),
        Some(_) => Err(usage(&format!("{at}.{key}"), "expected an array")),
    }
}

fn str_vec(obj: &JsonMap, key: &str, at: &str) -> Result<Vec<String>, ApiError> {
    array(obj, key, at)?
        .iter()
        .map(|v| match v {
            serde_json::Value::String(s) => Ok(s.clone()),
            _ => Err(usage(&format!("{at}.{key}"), "expected strings")),
        })
        .collect()
}

type JsonMap = serde_json::Map<String, serde_json::Value>;

/// One shape for the `report` argument and the output, so a field cannot be carried one way and
/// dropped the other.
fn host_json(host: &HostFacts) -> serde_json::Value {
    let either = |r: &Result<String, String>, ok: &str, err: &str| match r {
        Ok(v) => serde_json::json!({ ok: v }),
        Err(e) => serde_json::json!({ err: e }),
    };
    let mut os = either(&host.os_version, "os_version", "os_version_error");
    let map = os.as_object_mut().expect("an object");
    map.insert(
        "target".to_owned(),
        serde_json::Value::String(host.target.clone()),
    );
    if let Some(dumps) = &host.local_dumps {
        let fact = either(dumps, "local_dumps", "local_dumps_error");
        map.extend(fact.as_object().expect("an object").clone());
    }
    map.insert(
        "roles".to_owned(),
        host.roles
            .iter()
            .map(|r| {
                let mut v = either(&r.path, "path", "error");
                v.as_object_mut()
                    .expect("an object")
                    .insert("role".to_owned(), serde_json::Value::String(r.role.clone()));
                v
            })
            .collect(),
    );
    os
}

fn host_from_json(value: &serde_json::Value) -> Result<HostFacts, ApiError> {
    let at = "report.host";
    let obj = object(value, at)?;
    let either = |obj: &JsonMap,
                  ok: &str,
                  err: &str,
                  at: &str|
     -> Result<Result<String, String>, ApiError> {
        match (opt_str(obj, ok, at)?, opt_str(obj, err, at)?) {
            (Some(v), None) => Ok(Ok(v)),
            (None, Some(e)) => Ok(Err(e)),
            _ => Err(usage(at, &format!("exactly one of `{ok}` and `{err}`"))),
        }
    };
    let mut roles = Vec::new();
    for (i, role) in array(obj, "roles", at)?.iter().enumerate() {
        let at = format!("{at}.roles[{i}]");
        let role = object(role, &at)?;
        roles.push(HostRole {
            role: req_str(role, "role", &at)?,
            path: either(role, "path", "error", &at)?,
        });
    }
    let local_dumps = match (obj.get("local_dumps"), obj.get("local_dumps_error")) {
        (None, None) => None,
        _ => Some(either(obj, "local_dumps", "local_dumps_error", at)?),
    };
    Ok(HostFacts {
        target: req_str(obj, "target", at)?,
        os_version: either(obj, "os_version", "os_version_error", at)?,
        roles,
        local_dumps,
    })
}

impl Report {
    /// Every array may be absent and is then empty, so a synthetic report stays short.
    pub fn from_json(value: &serde_json::Value) -> Result<Report, ApiError> {
        let at = "report";
        let obj = object(value, at)?;
        let mut report = Report {
            config_file: str_or_empty(obj, "config_file", at)?,
            corpus_file: str_or_empty(obj, "corpus_file", at)?,
            warnings: str_vec(obj, "warnings", at)?,
            ..Report::default()
        };
        report.host = match obj.get("host") {
            None | Some(serde_json::Value::Null) => None,
            Some(host) => Some(host_from_json(host)?),
        };
        for (i, rom) in array(obj, "bundled_roms", at)?.iter().enumerate() {
            let at = format!("report.bundled_roms[{i}]");
            let rom = object(rom, &at)?;
            report.bundled_roms.push(BundledRom {
                file: req_str(rom, "file", &at)?,
                rev: req_str(rom, "rev", &at)?,
                pinned_sha256: req_str(rom, "pinned_sha256", &at)?,
                embedded_sha256: opt_str(rom, "embedded_sha256", &at)?,
                chip_revisions: str_vec(rom, "chip_revisions", &at)?,
            });
        }
        report.rom_override = match obj.get("rom_override") {
            None | Some(serde_json::Value::Null) => None,
            Some(over) => {
                let at = "report.rom_override";
                let over = object(over, at)?;
                Some(RomOverride {
                    origin: req_str(over, "origin", at)?,
                    path: opt_str(over, "path", at)?,
                    sha256: req_str(over, "sha256", at)?,
                    pinned: req_bool(over, "pinned", at)?,
                    pinned_rev: opt_str(over, "pinned_rev", at)?,
                })
            }
        };
        report.demo = match obj.get("demo") {
            None | Some(serde_json::Value::Null) => None,
            Some(demo) => {
                let at = "report.demo";
                let demo = object(demo, at)?;
                Some(EmbeddedDemo {
                    id: req_str(demo, "id", at)?,
                    sha256: req_str(demo, "sha256", at)?,
                    bytes: match demo.get("bytes") {
                        None | Some(serde_json::Value::Null) => 0,
                        Some(serde_json::Value::Number(n)) => n
                            .as_u64()
                            .ok_or_else(|| usage("report.demo.bytes", "expected a byte count"))?,
                        Some(_) => return Err(usage("report.demo.bytes", "expected a number")),
                    },
                })
            }
        };
        for (i, dir) in array(obj, "esp_rom_elfs", at)?.iter().enumerate() {
            let at = format!("report.esp_rom_elfs[{i}]");
            let dir = object(dir, &at)?;
            let mut files = Vec::new();
            for (j, file) in array(dir, "files", &at)?.iter().enumerate() {
                let at = format!("{at}.files[{j}]");
                let file = object(file, &at)?;
                files.push(EspRomElfFile {
                    name: req_str(file, "name", &at)?,
                    sha256: req_str(file, "sha256", &at)?,
                    pinned: req_bool(file, "pinned", &at)?,
                });
            }
            report.esp_rom_elfs.push(EspRomElfs {
                dir: req_str(dir, "dir", &at)?,
                files,
            });
        }
        for (i, entry) in array(obj, "corpus", at)?.iter().enumerate() {
            let at = format!("report.corpus[{i}]");
            let entry = object(entry, &at)?;
            let mut files = Vec::new();
            for (j, file) in array(entry, "files", &at)?.iter().enumerate() {
                let at = format!("{at}.files[{j}]");
                let file = object(file, &at)?;
                files.push(CorpusFile {
                    kind: req_str(file, "kind", &at)?,
                    path: req_str(file, "path", &at)?,
                    found: req_bool(file, "found", &at)?,
                    expected_sha256: opt_str(file, "expected_sha256", &at)?,
                    sha256: opt_str(file, "sha256", &at)?,
                });
            }
            report.corpus.push(CorpusEntry {
                id: req_str(entry, "id", &at)?,
                files,
            });
        }
        for (i, problem) in array(obj, "problems", at)?.iter().enumerate() {
            let at = format!("report.problems[{i}]");
            let problem = object(problem, &at)?;
            report.problems.push(Problem {
                code: req_str(problem, "code", &at)?,
                message: req_str(problem, "message", &at)?,
            });
        }
        Ok(report)
    }

    /// The exact inverse of [`Report::from_json`]. It sits beside the reader because the two are
    /// one format; `a_report_survives_the_argument_round_trip` catches a field written by one and
    /// not the other.
    pub fn to_input_json(&self) -> serde_json::Value {
        serde_json::json!({
            "host": self.host.as_ref().map_or(serde_json::Value::Null, host_json),
            "bundled_roms": self
                .bundled_roms
                .iter()
                .map(|rom| serde_json::json!({
                    "file": rom.file,
                    "rev": rom.rev,
                    "pinned_sha256": rom.pinned_sha256,
                    "embedded_sha256": rom.embedded_sha256,
                    "chip_revisions": rom.chip_revisions,
                }))
                .collect::<Vec<_>>(),
            "rom_override": match &self.rom_override {
                None => serde_json::Value::Null,
                Some(over) => serde_json::json!({
                    "origin": over.origin,
                    "path": over.path,
                    "sha256": over.sha256,
                    "pinned": over.pinned,
                    "pinned_rev": over.pinned_rev,
                }),
            },
            "demo": match &self.demo {
                None => serde_json::Value::Null,
                Some(demo) => serde_json::json!({
                    "id": demo.id,
                    "sha256": demo.sha256,
                    "bytes": demo.bytes,
                }),
            },
            "esp_rom_elfs": self
                .esp_rom_elfs
                .iter()
                .map(|dir| serde_json::json!({
                    "dir": dir.dir,
                    "files": dir
                        .files
                        .iter()
                        .map(|file| serde_json::json!({
                            "name": file.name,
                            "sha256": file.sha256,
                            "pinned": file.pinned,
                        }))
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
            "config_file": self.config_file,
            "corpus_file": self.corpus_file,
            "corpus": self
                .corpus
                .iter()
                .map(|entry| serde_json::json!({
                    "id": entry.id,
                    "files": entry
                        .files
                        .iter()
                        .map(|file| serde_json::json!({
                            "kind": file.kind,
                            "path": file.path,
                            "found": file.found,
                            "expected_sha256": file.expected_sha256,
                            "sha256": file.sha256,
                        }))
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
            "warnings": self.warnings,
            "problems": self
                .problems
                .iter()
                .map(|problem| serde_json::json!({
                    "code": problem.code,
                    "message": problem.message,
                }))
                .collect::<Vec<_>>(),
        })
    }
}

impl Report {
    /// `ok` is false when a failure rule fires (see the module doc).
    pub fn to_output_json(&self) -> serde_json::Value {
        let roms: Vec<serde_json::Value> = self
            .bundled_roms
            .iter()
            .map(|rom| {
                serde_json::json!({
                    "file": rom.file,
                    "rev": rom.rev,
                    "status": rom.status().as_str(),
                    "pinned_sha256": rom.pinned_sha256,
                    "embedded_sha256": rom.embedded_sha256,
                    "chip_revisions": rom.chip_revisions,
                })
            })
            .collect();
        let over = match &self.rom_override {
            None => serde_json::Value::Null,
            Some(over) => serde_json::json!({
                "origin": over.origin,
                "path": over.path,
                "sha256": over.sha256,
                "pinned": over.pinned,
                "pinned_rev": over.pinned_rev,
            }),
        };
        let elfs: Vec<serde_json::Value> = self
            .esp_rom_elfs
            .iter()
            .map(|dir| {
                serde_json::json!({
                    "dir": dir.dir,
                    "files": dir.files.iter().map(|f| serde_json::json!({
                        "name": f.name,
                        "sha256": f.sha256,
                        "pinned": f.pinned,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let corpus: Vec<serde_json::Value> = self
            .corpus
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "id": entry.id,
                    "status": entry.status().as_str(),
                    "files": entry.files.iter().map(|f| serde_json::json!({
                        "kind": f.kind,
                        "path": f.path,
                        "status": f.status().as_str(),
                        "expected_sha256": f.expected_sha256,
                        "sha256": f.sha256,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let demo = match &self.demo {
            None => serde_json::Value::Null,
            Some(demo) => serde_json::json!({
                "id": demo.id,
                "sha256": demo.sha256,
                "bytes": demo.bytes,
            }),
        };
        serde_json::json!({
            "ok": self.failure().is_none(),
            "host": self.host.as_ref().map_or(serde_json::Value::Null, host_json),
            "bundled_roms": roms,
            "rom_override": over,
            "demo": demo,
            "esp_rom_elfs": elfs,
            "config_file": self.config_file,
            "corpus_file": self.corpus_file,
            "corpus": corpus,
            "warnings": self.warnings,
            "problems": self.problems.iter().map(|p| serde_json::json!({
                "code": p.code,
                "message": p.message,
            })).collect::<Vec<_>>(),
            "summary": {
                "bundled_roms": self.bundled_roms.len(),
                "bundled_roms_found": self.count_bundled(Status::Found),
                "bundled_roms_missing": self.count_bundled(Status::Missing),
                "bundled_roms_mismatched": self.count_bundled(Status::Mismatched),
                "corpus": self.corpus.len(),
                "corpus_found": self.count_corpus(Status::Found),
                "corpus_missing": self.count_corpus(Status::Missing),
                "corpus_mismatched": self.count_corpus(Status::Mismatched),
                "warnings": self.warnings.len(),
                "problems": self.problems.len(),
            },
        })
    }

    pub fn count_bundled(&self, status: Status) -> usize {
        self.bundled_roms
            .iter()
            .filter(|r| r.status() == status)
            .count()
    }

    pub fn count_corpus(&self, status: Status) -> usize {
        self.corpus.iter().filter(|e| e.status() == status).count()
    }

    /// The first recorded problem, then a digest that differs from its pin, then a build with no
    /// bundled ROM and no override. A missing corpus file and a stale esp-rom-elfs copy are not
    /// failures: a fresh machine has no corpus.
    pub fn failure(&self) -> Option<(ErrorCode, String)> {
        if let Some(problem) = self.problems.first() {
            let code = ErrorCode::lookup(&problem.code).unwrap_or(E_INTERNAL);
            return Some((code, problem.message.clone()));
        }
        let mismatched: Vec<String> = self
            .bundled_roms
            .iter()
            .filter(|r| r.status() == Status::Mismatched)
            .map(|r| format!("bundled {}", r.file))
            .chain(
                self.corpus
                    .iter()
                    .flat_map(|e| e.files.iter().map(move |f| (&e.id, f)))
                    .filter(|(_, f)| f.status() == Status::Mismatched)
                    .map(|(id, f)| format!("corpus {id}.{}", f.kind)),
            )
            .collect();
        if !mismatched.is_empty() {
            return Some((
                E_ASSET_HASH,
                format!(
                    "{} asset(s) differ from the pinned SHA-256: {}",
                    mismatched.len(),
                    mismatched.join(", ")
                ),
            ));
        }
        let no_bundled = !self.bundled_roms.is_empty()
            && self.count_bundled(Status::Missing) == self.bundled_roms.len();
        if no_bundled && self.rom_override.is_none() {
            return Some((
                E_ASSET_MISSING,
                "no ROM: this build embeds none and no override supplies one".to_owned(),
            ));
        }
        None
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("bundled ROM (assets/rom/pins.toml)\n");
        if self.bundled_roms.is_empty() {
            out.push_str("  (none reported)\n");
        }
        for rom in &self.bundled_roms {
            let status = rom.status();
            let _ = writeln!(
                out,
                "  {:<24} {:<7} {:<10} pinned {}",
                rom.file,
                rom.rev,
                status.as_str(),
                rom.pinned_sha256
            );
            match (&rom.embedded_sha256, status) {
                (Some(sha), Status::Mismatched) => {
                    let _ = writeln!(out, "  {:<24} {:<7} embedded {sha}", "", "");
                }
                (None, _) => {
                    let _ = writeln!(out, "  {:<24} {:<7} not embedded in this build", "", "");
                }
                _ => {}
            }
            if !rom.chip_revisions.is_empty() {
                let _ = writeln!(
                    out,
                    "  {:<24} {:<7} chip revisions {}",
                    "",
                    "",
                    rom.chip_revisions.join(", ")
                );
            }
        }
        match &self.rom_override {
            None => out.push_str("ROM override: none\n"),
            Some(over) => {
                let pinned = if over.pinned { "pinned" } else { "unpinned" };
                let _ = writeln!(
                    out,
                    "ROM override: {} {} {} sha256 {}",
                    over.origin,
                    over.path.as_deref().unwrap_or("(no path)"),
                    pinned,
                    over.sha256
                );
                if let Some(rev) = &over.pinned_rev {
                    let _ = writeln!(out, "  pinned as {rev}");
                }
            }
        }
        match &self.demo {
            None => {
                out.push_str("embedded demo: none (a plain cargo build embeds no demo image)\n")
            }
            Some(demo) => {
                let _ = writeln!(
                    out,
                    "embedded demo: {} sha256 {} ({} bytes)",
                    demo.id, demo.sha256, demo.bytes
                );
            }
        }
        for dir in &self.esp_rom_elfs {
            let _ = writeln!(out, "local esp-rom-elfs: {}", dir.dir);
            for file in &dir.files {
                let pinned = if file.pinned { "pinned" } else { "not pinned" };
                let _ = writeln!(out, "  {} {} {}", file.name, pinned, file.sha256);
            }
        }
        // Only the CLI collects the host; other callers print nothing here, which keeps their
        // report unchanged.
        match &self.host {
            None => {}
            Some(host) => {
                let os = match &host.os_version {
                    Ok(v) => v.clone(),
                    Err(e) => format!("unknown ({e})"),
                };
                let _ = writeln!(out, "host: {} on {}", host.target, os);
                match &host.local_dumps {
                    None => {}
                    Some(Ok(fact)) => {
                        let _ = writeln!(out, "WER LocalDumps policy: {fact}");
                    }
                    Some(Err(e)) => {
                        let _ = writeln!(out, "WER LocalDumps policy: unknown ({e})");
                    }
                }
                out.push_str("directory roles\n");
                for role in &host.roles {
                    match &role.path {
                        Ok(path) => {
                            let _ = writeln!(out, "  {:<10} {}", role.role, path);
                        }
                        Err(e) => {
                            let _ = writeln!(out, "  {:<10} unresolved: {}", role.role, e);
                        }
                    }
                }
            }
        }
        let _ = writeln!(out, "config file: {}", self.config_file);
        let _ = writeln!(out, "corpus map:  {}", self.corpus_file);
        out.push_str("corpus\n");
        if self.corpus.is_empty() {
            out.push_str("  (no corpus map)\n");
        }
        for entry in &self.corpus {
            let _ = writeln!(
                out,
                "  {:<12} {:<10} {} file(s)",
                entry.id,
                entry.status().as_str(),
                entry.files.len()
            );
            for file in &entry.files {
                if file.status() != Status::Found {
                    let _ = writeln!(
                        out,
                        "    {}.{} {} {}",
                        entry.id,
                        file.kind,
                        file.status().as_str(),
                        file.path
                    );
                }
            }
        }
        for warning in &self.warnings {
            let _ = writeln!(out, "warning: {warning}");
        }
        for problem in &self.problems {
            let _ = writeln!(out, "problem: {}: {}", problem.code, problem.message);
        }
        match self.failure() {
            None => {
                let _ = writeln!(
                    out,
                    "ok: {} bundled ROM(s) pinned, {} corpus entry(s) found, {} missing, {} \
                     mismatched, {} warning(s)",
                    self.count_bundled(Status::Found),
                    self.count_corpus(Status::Found),
                    self.count_corpus(Status::Missing),
                    self.count_corpus(Status::Mismatched),
                    self.warnings.len()
                );
            }
            Some((code, message)) => {
                let _ = writeln!(out, "failed: {}: {message}", code.name);
            }
        }
        out
    }
}

/// The path the CLI takes: it owns discovery and builds the [`Report`].
pub fn run_report(report: &Report) -> Result<Output, ApiError> {
    let json = report.to_output_json();
    match report.failure() {
        None => Ok(Output {
            json,
            text: report.to_text(),
            artifacts: Vec::new(),
            receipt: Receipt::default(),
            vt_us: 0,
        }),
        Some((code, message)) => {
            let detail = serde_json::json!({ "report": json, "text": report.to_text() });
            let error = ApiError::new(code, message).with_detail(detail);
            Err(match code.name {
                "E_ASSET_MISSING" => error.with_hint(
                    "check the override path, or unset PASSPORTSIM_ROM and --rom to use the \
                     bundled ROM",
                ),
                "E_ASSET_HASH" => error.with_hint(
                    "restore the pinned file, or pass --allow-unpinned-rom to accept an unpinned \
                     ROM (the receipt then says rom: unpinned)",
                ),
                _ => error,
            })
        }
    }
}

/// Not a user flag: `passportsim doctor` takes no arguments and the CLI fills `report` in.
pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "Arguments of `doctor`: the discovery report the CLI builds.",
        "required": ["report"],
        "properties": {
            "report": {
                "type": "object",
                "description": "What native discovery found (pemu_host::assets::discovery_report).",
                "properties": {
                    "bundled_roms": {
                        "type": "array",
                        "description": "The bundled ROM ELFs and their pins.",
                        "items": {
                            "type": "object",
                            "required": ["file", "rev", "pinned_sha256"],
                            "properties": {
                                "file": { "type": "string" },
                                "rev": { "type": "string" },
                                "pinned_sha256": { "type": "string" },
                                "embedded_sha256": { "type": ["string", "null"] },
                                "chip_revisions": { "type": "array", "items": { "type": "string" } }
                            }
                        }
                    },
                    "host": {
                        "type": ["object", "null"],
                        "description": "Host triple, OS, roles.",
                        "required": ["target", "roles"],
                        "properties": {
                            "target": { "type": "string" },
                            "os_version": { "type": "string" },
                            "os_version_error": { "type": "string" },
                            "local_dumps": { "type": "string" },
                            "local_dumps_error": { "type": "string" },
                            "roles": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "required": ["role"],
                                    "properties": {
                                        "role": { "type": "string" },
                                        "path": { "type": "string" },
                                        "error": { "type": "string" }
                                    }
                                }
                            }
                        }
                    },
                    "rom_override": {
                        "type": ["object", "null"],
                        "description": "The active ROM override, if any.",
                        "required": ["origin", "sha256", "pinned"],
                        "properties": {
                            "origin": { "type": "string" },
                            "path": { "type": ["string", "null"] },
                            "sha256": { "type": "string" },
                            "pinned": { "type": "boolean" },
                            "pinned_rev": { "type": ["string", "null"] }
                        }
                    },
                    "demo": {
                        "type": ["object", "null"],
                        "description": "The embedded demo image, if any.",
                        "required": ["id", "sha256"],
                        "properties": {
                            "id": { "type": "string" },
                            "sha256": { "type": "string" },
                            "bytes": { "type": "integer", "minimum": 0 }
                        }
                    },
                    "esp_rom_elfs": {
                        "type": "array",
                        "description": "Local esp-rom-elfs copies, to warn.",
                        "items": {
                            "type": "object",
                            "required": ["dir"],
                            "properties": {
                                "dir": { "type": "string" },
                                "files": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": ["name", "sha256", "pinned"],
                                        "properties": {
                                            "name": { "type": "string" },
                                            "sha256": { "type": "string" },
                                            "pinned": { "type": "boolean" }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "config_file": { "type": "string" },
                    "corpus_file": { "type": "string" },
                    "corpus": {
                        "type": "array",
                        "description": "Every corpus entry.",
                        "items": {
                            "type": "object",
                            "required": ["id"],
                            "properties": {
                                "id": { "type": "string" },
                                "files": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": ["kind", "path", "found"],
                                        "properties": {
                                            "kind": { "type": "string" },
                                            "path": { "type": "string" },
                                            "found": { "type": "boolean" },
                                            "expected_sha256": { "type": ["string", "null"] },
                                            "sha256": { "type": ["string", "null"] }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "warnings": { "type": "array", "items": { "type": "string" } },
                    "problems": {
                        "type": "array",
                        "description": "Discovery failures; `doctor` returns the first code.",
                        "items": {
                            "type": "object",
                            "required": ["code", "message"],
                            "properties": {
                                "code": { "type": "string" },
                                "message": { "type": "string" }
                            }
                        }
                    }
                }
            }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "What `doctor` found, with a `found`, `missing` or `mismatched` status per asset. No file contents.",
        "required": ["ok", "bundled_roms", "rom_override", "corpus", "warnings", "problems", "summary"],
        "properties": {
            "ok": { "type": "boolean", "description": "False when a failure rule fired." },
            "host": { "type": ["object", "null"], "description": "Host triple, OS, directory roles." },
            "bundled_roms": { "type": "array", "items": { "type": "object" } },
            "rom_override": { "type": ["object", "null"] },
            "demo": { "type": ["object", "null"] },
            "esp_rom_elfs": { "type": "array", "items": { "type": "object" } },
            "config_file": { "type": "string" },
            "corpus_file": { "type": "string" },
            "corpus": { "type": "array", "items": { "type": "object" } },
            "warnings": { "type": "array", "items": { "type": "string" } },
            "problems": { "type": "array", "items": { "type": "object" } },
            "summary": { "type": "object", "description": "Counts per status." }
        }
    })
}

/// Report the bundled ROM pins, any ROM override and every corpus entry.
#[command(
    api_crate = crate,
    name = "doctor",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(read_only, idempotent, native_only),
    errors(E_USAGE, E_ASSET_MISSING, E_ASSET_HASH, E_INTERNAL),
    example(
        title = "Check the bundled ROM pins and the corpus",
        args = r#"{"report":{"bundled_roms":[],"corpus":[]}}"#,
    ),
)]
pub fn doctor(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let fields = object(&args, "arguments")?;
    let report = fields
        .get("report")
        .ok_or_else(|| usage("arguments", "missing `report`"))?;
    run_report(&Report::from_json(report)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Placeholders with no relation to a real file; the real pins are checked in `pemu-loader` and
    /// the `pemu-host` discovery tests.
    const SHA_A: &str = "9495e1453f36f7ee00000000000000000000000000000000000000000000aaaa";
    const SHA_B: &str = "19ac22e08707df9200000000000000000000000000000000000000000000bbbb";
    const SHA_OTHER: &str = "0000000000000000000000000000000000000000000000000000000000000042";

    /// Both bundled ROMs pinned, no override, one corpus entry found.
    fn healthy() -> Report {
        Report {
            bundled_roms: vec![
                BundledRom {
                    file: "esp32c3_rev101_rom.elf".to_owned(),
                    rev: "rom101".to_owned(),
                    pinned_sha256: SHA_A.to_owned(),
                    embedded_sha256: Some(SHA_A.to_owned()),
                    chip_revisions: vec!["v1.x".to_owned()],
                },
                BundledRom {
                    file: "esp32c3_rev3_rom.elf".to_owned(),
                    rev: "rom3".to_owned(),
                    pinned_sha256: SHA_B.to_owned(),
                    embedded_sha256: Some(SHA_B.to_owned()),
                    chip_revisions: vec!["v0.3".to_owned(), "v0.4".to_owned()],
                },
            ],
            config_file: "/tmp/home/.config/passportsim/config.toml".to_owned(),
            corpus_file: "/tmp/home/.config/passportsim/corpus.toml".to_owned(),
            corpus: vec![CorpusEntry {
                id: "pk".to_owned(),
                files: vec![CorpusFile {
                    kind: "bin".to_owned(),
                    path: "/tmp/home/corpus/pk/FoloToy-AI-Passport-8MB.bin".to_owned(),
                    found: true,
                    expected_sha256: Some(SHA_OTHER.to_owned()),
                    sha256: Some(SHA_OTHER.to_owned()),
                }],
            }],
            ..Report::default()
        }
    }

    fn args_of(report: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "report": report })
    }

    #[test]
    fn a_healthy_report_lists_both_bundled_roms_as_found() {
        let report = healthy();
        let out = run_report(&report).expect("nothing is wrong");
        assert_eq!(out.json["ok"], true);
        assert_eq!(out.json["bundled_roms"][0]["status"], "found");
        assert_eq!(out.json["bundled_roms"][0]["pinned_sha256"], SHA_A);
        assert_eq!(out.json["bundled_roms"][1]["status"], "found");
        assert_eq!(out.json["rom_override"], serde_json::Value::Null);
        assert_eq!(out.json["corpus"][0]["status"], "found");
        assert_eq!(out.json["summary"]["bundled_roms_found"], 2);
        assert_eq!(out.json["summary"]["corpus_found"], 1);
        assert_eq!(out.vt_us, 0);
        assert!(out.artifacts.is_empty());
    }

    /// `pemu-api` cannot read a file at all; this pins the format the CLI prints.
    #[test]
    fn the_text_is_paths_digests_and_statuses_only() {
        let text = healthy().to_text();
        let expected = format!(
            "bundled ROM (assets/rom/pins.toml)\n\
             \x20 esp32c3_rev101_rom.elf   rom101  found      pinned {SHA_A}\n\
             \x20                                  chip revisions v1.x\n\
             \x20 esp32c3_rev3_rom.elf     rom3    found      pinned {SHA_B}\n\
             \x20                                  chip revisions v0.3, v0.4\n\
             ROM override: none\n\
             embedded demo: none (a plain cargo build embeds no demo image)\n\
             config file: /tmp/home/.config/passportsim/config.toml\n\
             corpus map:  /tmp/home/.config/passportsim/corpus.toml\n\
             corpus\n\
             \x20 pk           found      1 file(s)\n\
             ok: 2 bundled ROM(s) pinned, 1 corpus entry(s) found, 0 missing, 0 mismatched, 0 warning(s)\n"
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn a_missing_corpus_entry_is_reported_and_does_not_fail() {
        let mut report = healthy();
        report.corpus.push(CorpusEntry {
            id: "goldminer".to_owned(),
            files: vec![CorpusFile {
                kind: "bin".to_owned(),
                path: "/tmp/home/corpus/goldminer/goldminer-sanitized-8MB.bin".to_owned(),
                found: false,
                expected_sha256: Some(SHA_OTHER.to_owned()),
                sha256: None,
            }],
        });
        let out = run_report(&report).expect("a fresh machine has no corpus");
        assert_eq!(out.json["ok"], true);
        assert_eq!(out.json["corpus"][1]["status"], "missing");
        assert_eq!(out.json["corpus"][1]["files"][0]["status"], "missing");
        assert_eq!(out.json["summary"]["corpus_missing"], 1);
        assert!(out.text.contains("goldminer    missing"), "{}", out.text);
    }

    #[test]
    fn a_mismatched_corpus_file_fails_with_asset_hash() {
        let mut report = healthy();
        report.corpus[0].files[0].sha256 = Some(SHA_A.to_owned());
        let error = run_report(&report).expect_err("the digest differs from corpus.toml");
        assert_eq!(error.code, E_ASSET_HASH);
        assert!(error.message.contains("corpus pk.bin"), "{}", error.message);
        assert_eq!(error.detail["report"]["ok"], false);
        assert_eq!(error.detail["report"]["corpus"][0]["status"], "mismatched");
        assert!(
            error.detail["text"]
                .as_str()
                .expect("the rendered report travels with the error")
                .contains("pk.bin mismatched")
        );
    }

    #[test]
    fn a_mismatched_bundled_rom_fails_with_asset_hash() {
        let mut report = healthy();
        report.bundled_roms[0].embedded_sha256 = Some(SHA_OTHER.to_owned());
        let error = run_report(&report).expect_err("the embedded bytes are not the pinned ones");
        assert_eq!(error.code, E_ASSET_HASH);
        assert!(
            error.message.contains("bundled esp32c3_rev101_rom.elf"),
            "{}",
            error.message
        );
        assert_eq!(
            error.detail["report"]["bundled_roms"][0]["status"],
            "mismatched"
        );
    }

    #[test]
    fn an_override_is_reported_with_its_origin_and_pin() {
        let mut report = healthy();
        report.rom_override = Some(RomOverride {
            origin: "PASSPORTSIM_ROM".to_owned(),
            path: Some("/tmp/home/roms/changed-rom101.elf".to_owned()),
            sha256: SHA_OTHER.to_owned(),
            pinned: false,
            pinned_rev: None,
        });
        report
            .warnings
            .push("ROM /tmp/home/roms/changed-rom101.elf is not pinned".to_owned());
        let out = run_report(&report).expect("--allow-unpinned-rom is the caller's decision");
        let over = &out.json["rom_override"];
        assert_eq!(over["origin"], "PASSPORTSIM_ROM");
        assert_eq!(over["path"], "/tmp/home/roms/changed-rom101.elf");
        assert_eq!(over["sha256"], SHA_OTHER);
        assert_eq!(over["pinned"], false);
        assert!(
            out.text.contains(
                "ROM override: PASSPORTSIM_ROM /tmp/home/roms/changed-rom101.elf unpinned"
            ),
            "{}",
            out.text
        );
        assert!(out.text.contains("warning: ROM "), "{}", out.text);
    }

    #[test]
    fn a_stale_local_esp_rom_elfs_copy_only_warns() {
        let mut report = healthy();
        report.esp_rom_elfs.push(EspRomElfs {
            dir: "/tmp/home/.espressif/tools/esp-rom-elfs/20241011".to_owned(),
            files: vec![EspRomElfFile {
                name: "esp32c3_rev101_rom.elf".to_owned(),
                sha256: SHA_OTHER.to_owned(),
                pinned: false,
            }],
        });
        report.warnings.push(
            "local /tmp/home/.espressif/tools/esp-rom-elfs/20241011/esp32c3_rev101_rom.elf \
             differs from assets/rom/pins.toml"
                .to_owned(),
        );
        let out = run_report(&report).expect("a stale local copy is never a failure");
        assert_eq!(out.json["ok"], true);
        assert_eq!(out.json["esp_rom_elfs"][0]["files"][0]["pinned"], false);
        assert_eq!(out.json["summary"]["warnings"], 1);
        assert!(out.text.contains("not pinned"), "{}", out.text);
    }

    #[test]
    fn a_build_without_the_bundled_rom_and_without_an_override_is_asset_missing() {
        let mut report = healthy();
        for rom in &mut report.bundled_roms {
            rom.embedded_sha256 = None;
        }
        let error = run_report(&report).expect_err("no ROM at all");
        assert_eq!(error.code, E_ASSET_MISSING);
        assert_eq!(
            error.detail["report"]["bundled_roms"][0]["status"],
            "missing"
        );
        assert!(
            error
                .hint
                .as_deref()
                .expect("a hint names the next step")
                .contains("PASSPORTSIM_ROM")
        );
        // An override supplies one, so the same build is fine.
        report.rom_override = Some(RomOverride {
            origin: "--rom".to_owned(),
            path: Some("/tmp/home/roms/esp32c3_rev101_rom.elf".to_owned()),
            sha256: SHA_A.to_owned(),
            pinned: true,
            pinned_rev: Some("rom101".to_owned()),
        });
        let out = run_report(&report).expect("the override supplies the ROM");
        assert_eq!(out.json["ok"], true);
        assert!(
            out.text.contains("not embedded in this build"),
            "{}",
            out.text
        );
    }

    /// Discovery records the problem, and `doctor` returns its code.
    #[test]
    fn a_rom_env_pointing_nowhere_maps_to_asset_missing() {
        let report = serde_json::json!({
            "bundled_roms": [{
                "file": "esp32c3_rev101_rom.elf",
                "rev": "rom101",
                "pinned_sha256": SHA_A,
                "embedded_sha256": SHA_A,
            }],
            "problems": [{
                "code": "E_ASSET_MISSING",
                "message": "ROM /nonexistent: No such file or directory (PASSPORTSIM_ROM)",
            }],
        });
        let error = doctor(&mut HandlerCx {}, args_of(&report)).expect_err("the file is not there");
        assert_eq!(error.code, E_ASSET_MISSING);
        assert_eq!(error.status(), u32::from(E_ASSET_MISSING.number));
        assert!(error.message.contains("/nonexistent"), "{}", error.message);
        assert_eq!(error.detail["report"]["ok"], false);
        assert_eq!(
            error.detail["report"]["problems"][0]["code"],
            "E_ASSET_MISSING"
        );
    }

    #[test]
    fn an_unregistered_problem_code_becomes_internal() {
        let report = serde_json::json!({
            "problems": [{ "code": "E_NOT_REGISTERED", "message": "?" }],
        });
        let error =
            doctor(&mut HandlerCx {}, args_of(&report)).expect_err("a problem is a failure");
        assert_eq!(error.code, E_INTERNAL);
    }

    #[test]
    fn the_json_and_the_typed_report_agree() {
        let report = healthy();
        let json = serde_json::json!({
            "bundled_roms": [
                {
                    "file": "esp32c3_rev101_rom.elf",
                    "rev": "rom101",
                    "pinned_sha256": SHA_A,
                    "embedded_sha256": SHA_A,
                    "chip_revisions": ["v1.x"],
                },
                {
                    "file": "esp32c3_rev3_rom.elf",
                    "rev": "rom3",
                    "pinned_sha256": SHA_B,
                    "embedded_sha256": SHA_B,
                    "chip_revisions": ["v0.3", "v0.4"],
                },
            ],
            "config_file": report.config_file,
            "corpus_file": report.corpus_file,
            "corpus": [{
                "id": "pk",
                "files": [{
                    "kind": "bin",
                    "path": "/tmp/home/corpus/pk/FoloToy-AI-Passport-8MB.bin",
                    "found": true,
                    "expected_sha256": SHA_OTHER,
                    "sha256": SHA_OTHER,
                }],
            }],
        });
        assert_eq!(Report::from_json(&json).expect("well formed"), report);
        let out = doctor(&mut HandlerCx {}, args_of(&json)).expect("healthy");
        assert_eq!(out.text, report.to_text());
    }

    /// Every field is filled, including those `None` or empty in [`healthy`], so a field
    /// [`Report::to_input_json`] forgot cannot survive.
    #[test]
    fn a_report_survives_the_argument_round_trip() {
        let mut report = healthy();
        report.host = Some(HostFacts {
            target: "aarch64-apple-darwin".to_owned(),
            os_version: Ok("27.2".to_owned()),
            local_dumps: Some(Ok("set for every executable".to_owned())),
            roles: vec![
                HostRole {
                    role: "config".to_owned(),
                    path: Ok("/tmp/home/.config/passportsim".to_owned()),
                },
                HostRole {
                    role: "cache".to_owned(),
                    path: Err("no home directory".to_owned()),
                },
            ],
        });
        report.rom_override = Some(RomOverride {
            origin: "PASSPORTSIM_ROM".to_owned(),
            path: Some("/tmp/rom.elf".to_owned()),
            sha256: SHA_OTHER.to_owned(),
            pinned: false,
            pinned_rev: Some("rom3".to_owned()),
        });
        report.demo = Some(EmbeddedDemo {
            id: "official".to_owned(),
            sha256: SHA_OTHER.to_owned(),
            bytes: 8 * 1024 * 1024,
        });
        report.esp_rom_elfs = vec![EspRomElfs {
            dir: "/tmp/home/.espressif/tools/esp-rom-elfs/20241011".to_owned(),
            files: vec![EspRomElfFile {
                name: "esp32c3_rev3_rom.elf".to_owned(),
                sha256: SHA_B.to_owned(),
                pinned: true,
            }],
        }];
        report.warnings = vec!["a local copy differs from the pins".to_owned()];
        report.problems = vec![Problem {
            code: "E_ASSET_MISSING".to_owned(),
            message: "PASSPORTSIM_ROM names a file that is not there".to_owned(),
        }];
        let json = report.to_input_json();
        assert_eq!(
            Report::from_json(&json).expect("the inverse of from_json"),
            report,
            "every field of the report reaches the command"
        );
        // Through the registered command too: the answer is the problem's code, not a parse
        // refusal.
        let error = doctor(&mut HandlerCx {}, args_of(&json)).expect_err("a problem is a failure");
        assert_eq!(error.code, E_ASSET_MISSING, "{}", error.message);
    }

    #[test]
    fn malformed_arguments_are_usage() {
        let cases = [
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({ "report": 7 }),
            serde_json::json!({ "report": { "bundled_roms": [{ "file": "a" }] } }),
            serde_json::json!({ "report": { "warnings": [7] } }),
            serde_json::json!({ "report": { "corpus": [{ "id": "pk", "files": [{}] }] } }),
            serde_json::json!({ "report": { "rom_override": { "origin": "--rom" } } }),
        ];
        for case in cases {
            let error = doctor(&mut HandlerCx {}, case.clone()).expect_err("malformed");
            assert_eq!(error.code, E_USAGE, "{case}");
        }
    }

    /// A plain `cargo build` embeds none, and that is not a failure.
    #[test]
    fn the_embedded_demo_is_reported_when_the_build_carries_one() {
        let mut report = healthy();
        assert_eq!(report.to_output_json()["demo"], serde_json::Value::Null);
        report.demo = Some(EmbeddedDemo {
            id: "official".to_owned(),
            sha256: SHA_OTHER.to_owned(),
            bytes: 8 * 1024 * 1024,
        });
        let out = run_report(&report).expect("an embedded demo is good news");
        assert_eq!(out.json["demo"]["id"], "official");
        assert_eq!(out.json["demo"]["sha256"], SHA_OTHER);
        assert_eq!(out.json["demo"]["bytes"], 8 * 1024 * 1024);
        assert!(
            out.text.contains(&format!(
                "embedded demo: official sha256 {SHA_OTHER} (8388608 bytes)"
            )),
            "{}",
            out.text
        );
        let json = serde_json::json!({
            "demo": { "id": "official", "sha256": SHA_OTHER, "bytes": 8 * 1024 * 1024 },
        });
        assert_eq!(
            Report::from_json(&json).expect("well formed").demo,
            report.demo
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_command_is_registered_read_only_and_native_only() {
        let spec = crate::registry::find("doctor").expect("#[command] registered doctor");
        assert_eq!(spec.group, crate::spec::CapsGroup::Core);
        assert_eq!(
            spec.summary,
            "Report the bundled ROM pins, any ROM override and every corpus entry."
        );
        assert!(spec.annotations.read_only && spec.annotations.idempotent);
        assert!(spec.annotations.native_only && !spec.annotations.needs_instance);
        assert!(spec.errors.contains(&E_ASSET_MISSING) && spec.errors.contains(&E_ASSET_HASH));
        assert_eq!(spec.examples.len(), 1);
    }

    #[test]
    fn the_host_facts_are_reported_with_their_errors() {
        let mut report = healthy();
        report.host = Some(HostFacts {
            target: "aarch64-apple-darwin".to_owned(),
            os_version: Err("not implemented yet on this host".to_owned()),
            roles: vec![HostRole {
                role: "data".to_owned(),
                path: Ok("/tmp/home/data".to_owned()),
            }],
            local_dumps: Some(Err("access denied".to_owned())),
        });
        let text = report.to_text();
        assert!(
            text.contains(
                "host: aarch64-apple-darwin on unknown (not implemented yet on this host)"
            ),
            "{text}"
        );
        assert!(text.contains("data       /tmp/home/data"), "{text}");
        let json = report.to_output_json();
        assert_eq!(json["host"]["target"], "aarch64-apple-darwin");
        assert_eq!(
            json["host"]["os_version_error"],
            "not implemented yet on this host"
        );
        assert!(json["host"].get("os_version").is_none());
        assert_eq!(json["host"]["roles"][0]["path"], "/tmp/home/data");
        assert!(
            text.contains("WER LocalDumps policy: unknown (access denied)"),
            "{text}"
        );
        assert_eq!(json["host"]["local_dumps_error"], "access denied");
        assert!(json["host"].get("local_dumps").is_none());

        // A host with no such policy prints no line and carries neither key.
        report.host.as_mut().expect("a host").local_dumps = None;
        assert!(!report.to_text().contains("LocalDumps"));
        let json = report.to_output_json();
        assert!(json["host"].get("local_dumps").is_none());
        assert!(json["host"].get("local_dumps_error").is_none());
    }
}
