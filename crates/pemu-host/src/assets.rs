//! Native asset overrides and discovery. The bundled ROM and synthesized eFuse need no discovery,
//! so everything here is optional: the ROM overrides (`--rom <path>`, `--rom idf`,
//! `PASSPORTSIM_ROM`, config keys `rom.rev101`/`rom.rev3`, in that order, plus
//! `--allow-unpinned-rom`), the tainting `--efuse-dump`, and the corpus map `corpus.toml`.
//! `~/.espressif` and `$IDF_TOOLS_PATH` are read only for `--rom idf` and the `doctor` warning.
//!
//! This is the only module that reads the file system and environment for assets; it hands bytes
//! to `pemu_loader`, whose pin check is a pure function of the bytes.

use std::env;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use pemu_api::commands::doctor;
use pemu_loader::bundle::{CORPUS_IDS, CorpusEntry, CorpusMap, TomlLite};
use pemu_loader::efuse_image::{BLOCK_WORDS, EfuseImage};
use pemu_loader::rom::{PinError, RomImage, RomRev, check_pin, pinned_rev, pins};
use pemu_loader::{hex, sha256};

pub const ROM_ENV: &str = "PASSPORTSIM_ROM";
/// Prefix of the per-id corpus override variables `PASSPORTSIM_CORPUS_<ID>`.
pub const CORPUS_ENV_PREFIX: &str = "PASSPORTSIM_CORPUS_";
pub const ROM_ARG_IDF: &str = "idf";
/// Directory of an esp-rom-elfs install, relative to `$IDF_TOOLS_PATH` or `~/.espressif`.
pub const ESP_ROM_ELFS_SUBDIR: &str = "tools/esp-rom-elfs";
pub const CONFIG_FILE: &str = "config.toml";
pub const CORPUS_FILE: &str = "corpus.toml";
pub const ROM_CONFIG_TABLE: &str = "rom";

/// Failure of an optional asset, carrying the error code the command layer reports.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AssetError {
    /// `E_ASSET_MISSING`: nothing supplies the asset.
    Missing {
        what: String,
        path: Option<PathBuf>,
        detail: String,
    },
    /// `E_ASSET_HASH`: the bytes are not pinned (without `--allow-unpinned-rom`) or do not match
    /// the corpus map's digest.
    Hash {
        what: String,
        path: PathBuf,
        sha256: String,
    },
    /// `E_USAGE`: the bytes are present but unusable, such as an override that is not an ELF.
    Invalid {
        what: String,
        path: Option<PathBuf>,
        detail: String,
    },
    /// `E_INTERNAL`: pinned bytes failed to assemble (a loader defect).
    Internal { detail: String },
}

impl AssetError {
    pub fn code_name(&self) -> &'static str {
        match self {
            AssetError::Missing { .. } => "E_ASSET_MISSING",
            AssetError::Hash { .. } => "E_ASSET_HASH",
            AssetError::Invalid { .. } => "E_USAGE",
            AssetError::Internal { .. } => "E_INTERNAL",
        }
    }

    fn from_pin(error: PinError, path: Option<PathBuf>, sha256: String) -> AssetError {
        match error {
            PinError::NoBundledRom => AssetError::Missing {
                what: "ROM".to_owned(),
                path,
                detail: "no bundled ROM for this chip revision".to_owned(),
            },
            PinError::Unpinned => match path {
                Some(path) => AssetError::Hash {
                    what: "ROM".to_owned(),
                    path,
                    sha256,
                },
                None => AssetError::Internal {
                    detail: "bundled ROM is not in assets/rom/pins.toml".to_owned(),
                },
            },
            PinError::Assembly(e) => AssetError::Internal {
                detail: e.to_string(),
            },
            // The magic-PC range must be provably unused in a pinned ROM.
            ref e @ PinError::MagicRange(_) => AssetError::Internal {
                detail: e.to_string(),
            },
        }
    }
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AssetError::Missing {
                what,
                path: Some(path),
                detail,
            }
            | AssetError::Invalid {
                what,
                path: Some(path),
                detail,
            } => write!(f, "{what} {}: {detail}", path.display()),
            AssetError::Missing { what, detail, .. } | AssetError::Invalid { what, detail, .. } => {
                write!(f, "{what}: {detail}")
            }
            AssetError::Hash { what, path, sha256 } => write!(
                f,
                "{what} {} has SHA-256 {sha256}, which is not pinned",
                path.display()
            ),
            AssetError::Internal { detail } => write!(f, "internal asset error: {detail}"),
        }
    }
}

impl std::error::Error for AssetError {}

/// The parts of the environment asset discovery may read. Tests build one by hand so they never
/// touch the real home directory.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HostEnv {
    pub home: Option<PathBuf>,
    pub config_dir: PathBuf,
    pub espressif_dir: Option<PathBuf>,
    pub idf_tools_path: Option<PathBuf>,
    pub rom_env: Option<String>,
    /// `PASSPORTSIM_CORPUS_<ID>` overrides: the id (lower case, `_` as `-`) and the path.
    pub corpus_env: Vec<(String, PathBuf)>,
    /// The data root a relative `corpus.toml` path resolves against. Only
    /// [`HostEnv::from_paths`] fills it, since only `HostPaths` resolves directory roles.
    pub data_root: Option<PathBuf>,
}

impl HostEnv {
    /// Reads the process environment. A scrubbed environment gives an empty result, the
    /// zero-setup case.
    pub fn from_process() -> HostEnv {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        let corpus_env = env::vars()
            .filter_map(|(key, value)| {
                let id = key.strip_prefix(CORPUS_ENV_PREFIX)?;
                Some((id.to_lowercase().replace('_', "-"), PathBuf::from(value)))
            })
            .collect();
        HostEnv {
            config_dir: home
                .clone()
                .unwrap_or_default()
                .join(".config")
                .join("passportsim"),
            espressif_dir: home.clone().map(|h| h.join(".espressif")),
            idf_tools_path: env::var_os("IDF_TOOLS_PATH")
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty()),
            rom_env: env::var(ROM_ENV).ok().filter(|v| !v.is_empty()),
            home,
            corpus_env,
            data_root: None,
        }
    }

    /// [`HostEnv::from_process`] with the config and data-root roles of `paths`, so
    /// `PASSPORTSIM_HOME` and the per-role overrides move `corpus.toml` and `config.toml` too.
    pub fn from_paths(paths: &crate::paths::HostPaths) -> HostEnv {
        let mut env = HostEnv::from_process();
        if let Ok(config) = paths.config() {
            env.config_dir = config;
        }
        env.data_root = paths.data_root().ok();
        env
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE)
    }

    pub fn corpus_file(&self) -> PathBuf {
        self.config_dir.join(CORPUS_FILE)
    }

    /// Resolves a path written in `corpus.toml` or `config.toml`: absolute as written (a leading
    /// `/` counts on every host); `~`, `~/...` or `~\...` against the home role; anything else
    /// relative to the data root, never the working directory, so the file is portable between
    /// hosts. A relative path that climbs out of the data root, or one with no data root, is
    /// refused. The error is the reason.
    pub fn expand(&self, path: &str) -> Result<PathBuf, String> {
        if path.starts_with('~') {
            return match self.home.as_ref() {
                Some(home) => Ok(crate::paths::expand_tilde(path, home)),
                None => Err(format!(
                    "`{path}` starts with `~` and this process resolved no home directory"
                )),
            };
        }
        let written = Path::new(path);
        // `is_absolute` is false for a leading `/` on Windows, which a map written on macOS holds;
        // `has_root` catches it.
        if written.is_absolute() || written.has_root() {
            return Ok(written.to_path_buf());
        }
        let root = self.data_root.as_ref().ok_or_else(|| {
            format!(
                "`{path}` is relative, so it resolves against the data root, and this \
                 process resolved no data root; write it absolute or with a leading `~/`"
            )
        })?;
        // Lexical, not `canonicalize`: the file may not exist yet, and `doctor` still names it.
        let mut resolved = root.clone();
        let mut depth = 0usize;
        for part in written.components() {
            match part {
                Component::CurDir => {}
                Component::Normal(name) => {
                    resolved.push(name);
                    depth += 1;
                }
                Component::ParentDir => {
                    if depth == 0 {
                        return Err(format!(
                            "`{path}` climbs out of the data root; a path in this file is \
                             absolute, `~`-relative, or relative to the data root without \
                             leaving it"
                        ));
                    }
                    resolved.pop();
                    depth -= 1;
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!(
                        "`{path}` is neither absolute nor relative to the data root"
                    ));
                }
            }
        }
        Ok(resolved)
    }
}

/// The ROM flags of a run.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RomOptions {
    pub rom: Option<RomArg>,
    /// `--allow-unpinned-rom`: accept a ROM outside `assets/rom/pins.toml`. The ROM-hash-keyed
    /// hooks are then off and the receipt says `rom: unpinned`.
    pub allow_unpinned_rom: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RomArg {
    /// `--rom idf`: the newest local esp-rom-elfs copy.
    Idf,
    Path(PathBuf),
}

impl RomArg {
    pub fn parse(value: &str) -> RomArg {
        if value == ROM_ARG_IDF {
            RomArg::Idf
        } else {
            RomArg::Path(PathBuf::from(value))
        }
    }
}

/// Where the ROM of a run came from, as the receipt and `doctor` print it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RomOrigin {
    Bundled,
    Cli,
    CliIdf,
    Env,
    Config,
}

impl RomOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            RomOrigin::Bundled => "bundled",
            RomOrigin::Cli => "--rom",
            RomOrigin::CliIdf => "--rom idf",
            RomOrigin::Env => ROM_ENV,
            RomOrigin::Config => "config rom.*",
        }
    }
}

pub struct ResolvedRom {
    pub rev: RomRev,
    pub image: RomImage,
    /// SHA-256 of the ELF bytes, lower-case hex.
    pub sha256: String,
    /// Whether the bytes are in `assets/rom/pins.toml`; `false` only with `--allow-unpinned-rom`.
    pub pinned: bool,
    pub pinned_rev: Option<RomRev>,
    pub origin: RomOrigin,
    pub path: Option<PathBuf>,
}

impl fmt::Debug for ResolvedRom {
    /// Without the ROM bytes, which are hundreds of kilobytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedRom")
            .field("rev", &self.rev)
            .field("sha256", &self.sha256)
            .field("pinned", &self.pinned)
            .field("pinned_rev", &self.pinned_rev)
            .field("origin", &self.origin)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// The ROM for this run: the first override that names a file wins, otherwise the bundled ELF of
/// `rev` (from `RomRev::for_efuse(&efuse)`).
pub fn resolve_rom(
    env: &HostEnv,
    opts: &RomOptions,
    rev: RomRev,
) -> Result<ResolvedRom, AssetError> {
    let Some((path, origin)) = rom_override(env, opts, rev)? else {
        return bundled_rom(rev);
    };
    let bytes = read_file("ROM", &path)?;
    let sha = hex(&sha256(&bytes));
    let pinned = pinned_rev(&bytes);
    match check_pin(&bytes) {
        Ok(image) => Ok(ResolvedRom {
            rev,
            image,
            sha256: sha,
            pinned: true,
            pinned_rev: pinned,
            origin,
            path: Some(path),
        }),
        Err(PinError::Unpinned) if opts.allow_unpinned_rom => {
            let image = RomImage::from_elf(&bytes).map_err(|e| AssetError::Invalid {
                what: "ROM".to_owned(),
                path: Some(path.clone()),
                detail: e.to_string(),
            })?;
            Ok(ResolvedRom {
                rev,
                image,
                sha256: sha,
                pinned: false,
                pinned_rev: None,
                origin,
                path: Some(path),
            })
        }
        Err(e) => Err(AssetError::from_pin(e, Some(path), sha)),
    }
}

/// The bundled ROM of a revision. Only `pemu-loader` knows whether the binary carries the bytes:
/// cargo unifies features, so a `pemu-host` built `--no-default-features` can still link a
/// `pemu-loader` with `bundled-rom`. Without the bytes a run needs an override (`E_ASSET_MISSING`).
pub fn bundled_rom(rev: RomRev) -> Result<ResolvedRom, AssetError> {
    let Some(bytes) = pemu_loader::rom::bundled_opt(rev) else {
        return Err(no_embedded_rom(rev));
    };
    let sha = hex(&sha256(bytes));
    let image = check_pin(bytes).map_err(|e| AssetError::from_pin(e, None, sha.clone()))?;
    Ok(ResolvedRom {
        rev,
        image,
        sha256: sha,
        pinned: true,
        pinned_rev: Some(rev),
        origin: RomOrigin::Bundled,
        path: None,
    })
}

/// The `E_ASSET_MISSING` of a build without ROM bytes, separate so a build with them can test it.
fn no_embedded_rom(rev: RomRev) -> AssetError {
    AssetError::Missing {
        what: "ROM".to_owned(),
        path: None,
        detail: format!(
            "this build has no bundled {}: pass --rom <path>",
            rev.file_name()
        ),
    }
}

/// The first override set, or `None` for the bundled ROM. An override naming no readable file is
/// an error, never a silent fallback.
fn rom_override(
    env: &HostEnv,
    opts: &RomOptions,
    rev: RomRev,
) -> Result<Option<(PathBuf, RomOrigin)>, AssetError> {
    if let Some(arg) = &opts.rom {
        return match arg {
            RomArg::Path(path) => Ok(Some((path.clone(), RomOrigin::Cli))),
            RomArg::Idf => idf_rom_file(env, rev).map(|path| Some((path, RomOrigin::CliIdf))),
        };
    }
    if let Some(value) = &env.rom_env {
        return Ok(Some((PathBuf::from(value), RomOrigin::Env)));
    }
    let config = match fs::read_to_string(env.config_file()) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let key = rom_config_key(rev);
    let Some(written) = TomlLite::parse(&config)
        .string(ROM_CONFIG_TABLE, key)
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    // A bad value is `E_USAGE`: the bytes are reachable, the configuration is not usable.
    let path = env.expand(&written).map_err(|detail| AssetError::Invalid {
        what: format!("`{ROM_CONFIG_TABLE}.{key}` of {CONFIG_FILE}"),
        path: None,
        detail,
    })?;
    Ok(Some((path, RomOrigin::Config)))
}

pub fn rom_config_key(rev: RomRev) -> &'static str {
    match rev {
        RomRev::Rev101 => "rev101",
        RomRev::Rev3 => "rev3",
    }
}

/// `--rom idf`: the ROM file of the newest esp-rom-elfs directory, `$IDF_TOOLS_PATH` first, then
/// `~/.espressif`.
fn idf_rom_file(env: &HostEnv, rev: RomRev) -> Result<PathBuf, AssetError> {
    let searched: Vec<PathBuf> = esp_rom_elfs_dirs(env);
    for dir in &searched {
        let file = dir.join(rev.file_name());
        if file.is_file() {
            return Ok(file);
        }
    }
    Err(AssetError::Missing {
        what: "ROM".to_owned(),
        path: None,
        detail: format!(
            "--rom idf found no {} in {}",
            rev.file_name(),
            if searched.is_empty() {
                "any esp-rom-elfs directory ($IDF_TOOLS_PATH and ~/.espressif are unset or empty)"
                    .to_owned()
            } else {
                searched
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
    })
}

/// The newest esp-rom-elfs directory under `$IDF_TOOLS_PATH` and under `~/.espressif`, in order.
pub fn esp_rom_elfs_dirs(env: &HostEnv) -> Vec<PathBuf> {
    [env.idf_tools_path.as_ref(), env.espressif_dir.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|root| newest_child(&root.join(ESP_ROM_ELFS_SUBDIR)))
        .collect()
}

/// The child directory with the greatest name, which for esp-rom-elfs is the newest release date.
fn newest_child(dir: &Path) -> Option<PathBuf> {
    let mut names: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry.file_type().ok()?.is_dir().then(|| entry.path())
        })
        .collect();
    names.sort();
    names.pop()
}

fn read_file(what: &str, path: &Path) -> Result<Vec<u8>, AssetError> {
    fs::read(path).map_err(|e| AssetError::Missing {
        what: what.to_owned(),
        path: Some(path.to_owned()),
        detail: e.to_string(),
    })
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CorpusPath {
    pub kind: String,
    pub path: PathBuf,
    pub sha256: Option<[u8; 32]>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CorpusId {
    pub id: String,
    pub files: Vec<CorpusPath>,
}

impl CorpusId {
    pub fn file(&self, kind: &str) -> Option<&CorpusPath> {
        self.files.iter().find(|f| f.kind == kind)
    }
}

/// The corpus map with `~` expanded and `PASSPORTSIM_CORPUS_<ID>` applied. The corpus is absent
/// on a fresh machine, so a caller that needs it reports a skip rather than a failure.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Corpus {
    file: PathBuf,
    entries: Vec<CorpusId>,
    warnings: Vec<String>,
}

impl Corpus {
    pub fn load(env: &HostEnv) -> Result<Corpus, AssetError> {
        let file = env.corpus_file();
        let text = fs::read_to_string(&file).map_err(|e| AssetError::Missing {
            what: "corpus map".to_owned(),
            path: Some(file.clone()),
            detail: e.to_string(),
        })?;
        Ok(Corpus::from_text(env, file, &text))
    }

    /// Reads a map from text. A file whose path [`HostEnv::expand`] refuses is dropped from its
    /// entry with the reason kept in [`Corpus::warnings`], so one mistyped line does not take
    /// every other id down.
    pub fn from_text(env: &HostEnv, file: PathBuf, text: &str) -> Corpus {
        let mut warnings = Vec::new();
        let mut entries: Vec<CorpusId> = CorpusMap::parse(text)
            .entries()
            .iter()
            .map(|entry: &CorpusEntry| CorpusId {
                id: entry.id.clone(),
                files: entry
                    .files
                    .iter()
                    .filter_map(|f| match env.expand(&f.path) {
                        Ok(path) => Some(CorpusPath {
                            kind: f.kind.clone(),
                            path,
                            sha256: f.sha256,
                        }),
                        Err(detail) => {
                            warnings.push(format!(
                                "corpus id `{}`, key `{}`: {detail}",
                                entry.id, f.kind
                            ));
                            None
                        }
                    })
                    .collect(),
            })
            .collect();
        // `PASSPORTSIM_CORPUS_<ID>` replaces the image of one id, digest included.
        for (id, path) in &env.corpus_env {
            let file = CorpusPath {
                kind: pemu_loader::bundle::CORPUS_BIN.to_owned(),
                path: path.clone(),
                sha256: None,
            };
            match entries.iter_mut().find(|e| &e.id == id) {
                Some(entry) => match entry.files.iter_mut().find(|f| f.kind == file.kind) {
                    Some(existing) => *existing = file,
                    None => entry.files.push(file),
                },
                None => entries.push(CorpusId {
                    id: id.clone(),
                    files: vec![file],
                }),
            }
        }
        Corpus {
            file,
            entries,
            warnings,
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn entries(&self) -> &[CorpusId] {
        &self.entries
    }

    pub fn get(&self, id: &str) -> Option<&CorpusId> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn read(&self, id: &str, kind: &str) -> Result<Vec<u8>, AssetError> {
        let entry = self.get(id).ok_or_else(|| AssetError::Missing {
            what: format!("corpus id {id}"),
            path: Some(self.file.clone()),
            detail: "not in the corpus map".to_owned(),
        })?;
        let file = entry.file(kind).ok_or_else(|| AssetError::Missing {
            what: format!("corpus {id}.{kind}"),
            path: Some(self.file.clone()),
            detail: "not in the corpus map".to_owned(),
        })?;
        let bytes = read_file(&format!("corpus {id}.{kind}"), &file.path)?;
        let digest = sha256(&bytes);
        match file.sha256 {
            Some(want) if want != digest => Err(AssetError::Hash {
                what: format!("corpus {id}.{kind}"),
                path: file.path.clone(),
                sha256: hex(&digest),
            }),
            _ => Ok(bytes),
        }
    }
}

/// `--efuse-dump <path>`: a directory of `efuse_blk<N>.bin` files as espefuse writes them, or one
/// whole-image file. The image is tainted, so every export needs `--include-secrets`. CLI only,
/// never exposed over MCP or HTTP.
pub fn load_efuse_dump(path: &Path) -> Result<EfuseImage, AssetError> {
    if path.is_dir() {
        let mut blocks: Vec<(u8, Vec<u8>)> = Vec::new();
        for block in 0..BLOCK_WORDS.len() {
            let file = path.join(format!("efuse_blk{block}.bin"));
            if file.is_file() {
                blocks.push((block as u8, read_file("eFuse dump", &file)?));
            }
        }
        if blocks.is_empty() {
            return Err(AssetError::Missing {
                what: "eFuse dump".to_owned(),
                path: Some(path.to_owned()),
                detail: "no efuse_blk<N>.bin file in this directory".to_owned(),
            });
        }
        let borrowed: Vec<(u8, &[u8])> = blocks.iter().map(|(b, v)| (*b, v.as_slice())).collect();
        EfuseImage::from_blocks(&borrowed)
    } else {
        EfuseImage::from_dump(&read_file("eFuse dump", path)?)
    }
    .map_err(|e| AssetError::Invalid {
        what: "eFuse dump".to_owned(),
        path: Some(path.to_owned()),
        detail: e.to_string(),
    })
}

/// One bundled ROM as `doctor` reports it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BundledRomReport {
    pub file: String,
    pub rev: RomRev,
    pub pinned_sha256: String,
    pub embedded_sha256: Option<String>,
    pub matches: bool,
    pub chip_revisions: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RomOverrideReport {
    pub origin: RomOrigin,
    pub path: Option<PathBuf>,
    pub sha256: String,
    pub pinned: bool,
    pub pinned_rev: Option<RomRev>,
}

/// A local esp-rom-elfs directory, read only to warn.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EspRomElfsReport {
    pub dir: PathBuf,
    /// File name, its SHA-256 and whether `assets/rom/pins.toml` pins that digest.
    pub files: Vec<(String, String, bool)>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CorpusFileReport {
    pub kind: String,
    pub path: PathBuf,
    pub found: bool,
    pub expected_sha256: Option<String>,
    /// The digest of the file on disk, only when the report was asked to hash (`verify`).
    pub sha256: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CorpusReport {
    pub id: String,
    pub files: Vec<CorpusFileReport>,
}

/// A discovery failure. `doctor` fails with the first one; a warning never fails.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AssetProblem {
    pub code: &'static str,
    pub message: String,
}

/// What discovery found, for `passportsim doctor` (built here because `pemu-api` may not depend on
/// `pemu-host`). A local esp-rom-elfs copy that differs from the pins is a warning only.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiscoveryReport {
    pub bundled_roms: Vec<BundledRomReport>,
    pub rom_override: Option<RomOverrideReport>,
    pub esp_rom_elfs: Vec<EspRomElfsReport>,
    pub config_file: PathBuf,
    pub corpus_file: PathBuf,
    pub corpus: Vec<CorpusReport>,
    pub warnings: Vec<String>,
    pub problems: Vec<AssetProblem>,
}

/// The host part of the report: target triple, OS version and every directory role as this
/// process resolves it. Anything unreadable is reported with its reason, never left out.
pub fn host_facts(paths: &crate::paths::HostPaths) -> doctor::HostFacts {
    use crate::paths::Role;
    use doctor::{HostFacts, HostRole};
    HostFacts {
        target: env!("PEMU_TARGET_TRIPLE").to_owned(),
        os_version: crate::platform::host()
            .os_version()
            .map_err(|e| e.to_string()),
        roles: Role::ALL
            .iter()
            .map(|role| HostRole {
                role: role.slug().to_owned(),
                path: paths
                    .role(*role)
                    .map(|p| p.display().to_string())
                    .map_err(|e| e.to_string()),
            })
            .collect(),
        local_dumps: local_dumps_fact(),
    }
}

/// The `LocalDumps` fact for [`host_facts`]: `None` on a host with no such policy.
fn local_dumps_fact() -> Option<Result<String, String>> {
    match crate::platform::crash_reports().forced_dumps() {
        Ok(policy) => policy.describe().map(|fact| Ok(fact.to_owned())),
        Err(e) => Some(Err(e.to_string())),
    }
}

/// A `doctor` warning when the machine's `LocalDumps` policy is set: it writes a crash dump even
/// though the process opted out of crash reporting.
pub fn local_dumps_warning() -> Option<String> {
    let policy = crate::platform::crash_reports().forced_dumps().ok()?;
    policy.is_set().then(|| {
        format!(
            "the machine-wide WER LocalDumps policy is {}: a crash of this process can still be \
             written to a local dump, which may carry guest RAM and flash",
            policy.describe().unwrap_or_default()
        )
    })
}

/// Runs discovery for a report: the bundled pins, the active override, any local esp-rom-elfs
/// copy and the corpus map. With `verify` (as `passportsim doctor` does) every present corpus
/// file is hashed and compared.
pub fn discovery_report(
    env: &HostEnv,
    opts: &RomOptions,
    rev: RomRev,
    verify: bool,
) -> DiscoveryReport {
    let mut warnings = Vec::new();
    let mut problems: Vec<AssetProblem> = Vec::new();
    let bundled_roms = pins()
        .into_iter()
        .map(|pin| {
            let pinned_sha256 = hex(&pin.sha256);
            let embedded_sha256 = embedded_rom_sha256(pin.rev);
            let matches = embedded_sha256.as_deref() == Some(pinned_sha256.as_str());
            if let Some(embedded) = &embedded_sha256
                && !matches
            {
                warnings.push(format!(
                    "embedded {} has SHA-256 {embedded}, but assets/rom/pins.toml pins {pinned_sha256}",
                    pin.file
                ));
            }
            BundledRomReport {
                file: pin.file.to_owned(),
                rev: pin.rev,
                pinned_sha256,
                embedded_sha256,
                matches,
                chip_revisions: pin.chip_revisions.iter().map(|c| (*c).to_owned()).collect(),
            }
        })
        .collect();

    let rom_override = match resolve_rom(env, opts, rev) {
        Ok(rom) if rom.origin != RomOrigin::Bundled => {
            if !rom.pinned {
                warnings.push(format!(
                    "ROM {} is not pinned: --allow-unpinned-rom is in effect, so the ROM-hash hooks are off and the receipt says rom: unpinned",
                    rom.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
                ));
            }
            if let Some(pinned) = rom.pinned_rev
                && pinned != rev
            {
                warnings.push(format!(
                    "ROM override is pinned to {} but the eFuse chip revision selects {}",
                    pinned.file_name(),
                    rev.file_name()
                ));
            }
            Some(RomOverrideReport {
                origin: rom.origin,
                path: rom.path,
                sha256: rom.sha256,
                pinned: rom.pinned,
                pinned_rev: rom.pinned_rev,
            })
        }
        Ok(_) => None,
        Err(e) => {
            problems.push(AssetProblem {
                code: e.code_name(),
                message: e.to_string(),
            });
            None
        }
    };

    let esp_rom_elfs = esp_rom_elfs_dirs(env)
        .into_iter()
        .map(|dir| {
            let files = RomRev::ALL
                .into_iter()
                .filter_map(|rev| {
                    let path = dir.join(rev.file_name());
                    let bytes = fs::read(&path).ok()?;
                    let digest = hex(&sha256(&bytes));
                    let pinned = pinned_rev(&bytes).is_some();
                    if !pinned {
                        warnings.push(format!(
                            "local {} differs from assets/rom/pins.toml (SHA-256 {digest})",
                            path.display()
                        ));
                    }
                    Some((rev.file_name().to_owned(), digest, pinned))
                })
                .collect();
            EspRomElfsReport { dir, files }
        })
        .collect();

    let mut corpus: Vec<CorpusReport> = match Corpus::load(env) {
        Ok(corpus) => {
            warnings.extend(corpus.warnings().iter().cloned());
            corpus
                .entries()
                .iter()
                .map(|entry| CorpusReport {
                    id: entry.id.clone(),
                    files: entry
                        .files
                        .iter()
                        .map(|f| {
                            let found = f.path.is_file();
                            CorpusFileReport {
                                kind: f.kind.clone(),
                                path: f.path.clone(),
                                found,
                                expected_sha256: f.sha256.as_ref().map(|d| hex(d)),
                                sha256: (verify && found)
                                    .then(|| fs::read(&f.path).ok().map(|b| hex(&sha256(&b))))
                                    .flatten(),
                            }
                        })
                        .collect(),
                })
                .collect()
        }
        Err(e) => {
            warnings.push(format!("{e}"));
            Vec::new()
        }
    };
    // Every known corpus id is reported; one the map leaves out reads as missing, which is not a
    // failure on a fresh machine. `rom101` and `rom3` are bundled.
    let absent: Vec<&str> = CORPUS_IDS
        .into_iter()
        .filter(|id| !corpus.iter().any(|e| e.id == *id))
        .collect();
    corpus.extend(absent.into_iter().map(|id| CorpusReport {
        id: id.to_owned(),
        files: Vec::new(),
    }));

    DiscoveryReport {
        bundled_roms,
        rom_override,
        esp_rom_elfs,
        config_file: env.config_file(),
        corpus_file: env.corpus_file(),
        corpus,
        warnings,
        problems,
    }
}

impl DiscoveryReport {
    /// The report in the shape [`pemu_api::commands::doctor::run_report`] takes.
    pub fn to_doctor_report(&self) -> doctor::Report {
        doctor::Report {
            host: None,
            bundled_roms: self
                .bundled_roms
                .iter()
                .map(|rom| doctor::BundledRom {
                    file: rom.file.clone(),
                    rev: rom.rev.corpus_id().to_owned(),
                    pinned_sha256: rom.pinned_sha256.clone(),
                    embedded_sha256: rom.embedded_sha256.clone(),
                    chip_revisions: rom.chip_revisions.clone(),
                })
                .collect(),
            rom_override: self.rom_override.as_ref().map(|over| doctor::RomOverride {
                origin: over.origin.as_str().to_owned(),
                path: over.path.as_ref().map(|p| p.display().to_string()),
                sha256: over.sha256.clone(),
                pinned: over.pinned,
                pinned_rev: over.pinned_rev.map(|r| r.corpus_id().to_owned()),
            }),
            esp_rom_elfs: self
                .esp_rom_elfs
                .iter()
                .map(|dir| doctor::EspRomElfs {
                    dir: dir.dir.display().to_string(),
                    files: dir
                        .files
                        .iter()
                        .map(|(name, sha256, pinned)| doctor::EspRomElfFile {
                            name: name.clone(),
                            sha256: sha256.clone(),
                            pinned: *pinned,
                        })
                        .collect(),
                })
                .collect(),
            // A packaged binary carries the demo; the CLI fills it in.
            demo: None,
            config_file: self.config_file.display().to_string(),
            corpus_file: self.corpus_file.display().to_string(),
            corpus: self
                .corpus
                .iter()
                .map(|entry| doctor::CorpusEntry {
                    id: entry.id.clone(),
                    files: entry
                        .files
                        .iter()
                        .map(|f| doctor::CorpusFile {
                            kind: f.kind.clone(),
                            path: f.path.display().to_string(),
                            found: f.found,
                            expected_sha256: f.expected_sha256.clone(),
                            sha256: f.sha256.clone(),
                        })
                        .collect(),
                })
                .collect(),
            warnings: self.warnings.clone(),
            problems: self
                .problems
                .iter()
                .map(|p| doctor::Problem {
                    code: p.code.to_owned(),
                    message: p.message.clone(),
                })
                .collect(),
        }
    }
}

/// The digest of the ROM bytes this binary carries, asked of `pemu-loader` (see [`bundled_rom`]).
fn embedded_rom_sha256(rev: RomRev) -> Option<String> {
    pemu_loader::rom::bundled_opt(rev).map(|bytes| hex(&sha256(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test run; every dump the tests import is synthesized.
    fn scratch(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("pemu-assets-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    #[test]
    fn from_paths_reads_the_corpus_map_of_the_config_role() {
        use crate::paths::{Env, HostPaths, Overrides};
        let paths = HostPaths::new(Env::macos(
            PathBuf::from("/Users/example"),
            Overrides {
                home: Some("/tmp/pemu-home".to_string()),
                ..Overrides::default()
            },
        ));
        let env = HostEnv::from_paths(&paths);
        assert_eq!(env.config_dir, PathBuf::from("/tmp/pemu-home/config"));
        assert_eq!(
            env.corpus_file(),
            PathBuf::from("/tmp/pemu-home/config/corpus.toml")
        );
    }

    fn empty_env(home: &Path) -> HostEnv {
        HostEnv {
            home: Some(home.to_owned()),
            config_dir: home.join(".config").join("passportsim"),
            espressif_dir: None,
            idf_tools_path: None,
            rom_env: None,
            corpus_env: Vec::new(),
            data_root: Some(home.join("data")),
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory");
        }
        fs::write(path, bytes).expect("write");
    }

    #[test]
    fn an_absolute_path_is_used_as_written() {
        let env = empty_env(Path::new("/tmp/pemu-home"));
        assert_eq!(
            env.expand("/opt/images/official.bin").expect("absolute"),
            PathBuf::from("/opt/images/official.bin")
        );
    }

    /// Both separators, because one `corpus.toml` is read on macOS and on Windows.
    #[test]
    fn both_tilde_forms_expand_to_the_home_role() {
        let home = Path::new("/tmp/pemu-home");
        let env = empty_env(home);
        assert_eq!(
            env.expand("~/corpus/official.bin").expect("unix tilde"),
            home.join("corpus").join("official.bin")
        );
        assert_eq!(
            env.expand("~\\corpus\\official.bin")
                .expect("windows tilde"),
            home.join("corpus\\official.bin")
        );
        assert_eq!(env.expand("~").expect("bare tilde"), home);
    }

    /// A relative entry names the same file whatever the working directory. The directory is put
    /// back before asserting, and `expand` never reads it.
    #[test]
    fn a_relative_path_resolves_under_the_data_root_and_not_the_working_directory() {
        let home = Path::new("/tmp/pemu-home");
        let env = empty_env(home);
        let elsewhere = scratch("cwd-elsewhere");

        let before = env::current_dir().expect("a working directory");
        env::set_current_dir(&elsewhere).expect("move the working directory");
        let resolved = env.expand("corpus/official/image.bin");
        env::set_current_dir(&before).expect("restore the working directory");

        let resolved = resolved.expect("relative to the data root");
        assert_eq!(
            resolved,
            home.join("data")
                .join("corpus")
                .join("official")
                .join("image.bin"),
            "a relative entry resolves against the data root"
        );
        assert!(
            !resolved.starts_with(&elsewhere),
            "{} was resolved against the working directory",
            resolved.display()
        );
        let _ = fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn a_parent_component_that_stays_inside_the_data_root_resolves() {
        let env = empty_env(Path::new("/tmp/pemu-home"));
        assert_eq!(
            env.expand("corpus/pk/../official/image.bin")
                .expect("inside"),
            PathBuf::from("/tmp/pemu-home/data/corpus/official/image.bin")
        );
        assert_eq!(
            env.expand("./corpus/image.bin")
                .expect("current directory component"),
            PathBuf::from("/tmp/pemu-home/data/corpus/image.bin")
        );
    }

    #[test]
    fn a_relative_path_that_climbs_out_of_the_data_root_is_refused() {
        let env = empty_env(Path::new("/tmp/pemu-home"));
        for written in [
            "../outside.bin",
            "corpus/../../outside.bin",
            "../../../etc/passwd",
        ] {
            let error = env
                .expand(written)
                .expect_err("a path that leaves the data root is refused");
            assert!(error.contains("climbs out of the data root"), "{error}");
            assert!(
                error.contains(written),
                "the reason names the path: {error}"
            );
        }
    }

    #[test]
    fn a_relative_path_without_a_data_root_is_refused() {
        let mut env = empty_env(Path::new("/tmp/pemu-home"));
        env.data_root = None;
        let error = env
            .expand("corpus/image.bin")
            .expect_err("no base to resolve against");
        assert!(error.contains("resolved no data root"), "{error}");
    }

    #[test]
    fn a_refused_corpus_path_is_dropped_with_a_warning_and_the_rest_of_the_map_survives() {
        let home = Path::new("/tmp/pemu-home");
        let env = empty_env(home);
        let text = "\
[bad]
bin = \"../../outside.bin\"

[official]
bin = \"corpus/official/image.bin\"
";
        let corpus = Corpus::from_text(&env, PathBuf::from("corpus.toml"), text);
        let good = corpus.get("official").expect("the good entry survives");
        assert_eq!(
            good.file("bin").expect("its bin").path,
            home.join("data")
                .join("corpus")
                .join("official")
                .join("image.bin")
        );
        assert!(
            corpus.get("bad").is_none_or(|e| e.file("bin").is_none()),
            "the refused file is not in the map"
        );
        assert_eq!(corpus.warnings().len(), 1, "{:?}", corpus.warnings());
        let warning = &corpus.warnings()[0];
        assert!(warning.contains("corpus id `bad`"), "{warning}");
        assert!(warning.contains("key `bin`"), "{warning}");
        assert!(warning.contains("climbs out of the data root"), "{warning}");
    }

    #[test]
    fn error_codes_are_the_documented_codes() {
        let missing = AssetError::Missing {
            what: "ROM".to_owned(),
            path: None,
            detail: "none".to_owned(),
        };
        assert_eq!(missing.code_name(), "E_ASSET_MISSING");
        assert_eq!(
            AssetError::Hash {
                what: "ROM".to_owned(),
                path: PathBuf::from("/tmp/x.elf"),
                sha256: "ab".to_owned(),
            }
            .code_name(),
            "E_ASSET_HASH"
        );
        assert_eq!(
            AssetError::Invalid {
                what: "ROM".to_owned(),
                path: None,
                detail: "not an ELF".to_owned()
            }
            .code_name(),
            "E_USAGE"
        );
        assert_eq!(
            AssetError::Internal {
                detail: "x".to_owned()
            }
            .code_name(),
            "E_INTERNAL"
        );
        assert_eq!(rom_config_key(RomRev::Rev101), "rev101");
        assert_eq!(rom_config_key(RomRev::Rev3), "rev3");
        assert_eq!(RomArg::parse("idf"), RomArg::Idf);
        assert_eq!(
            RomArg::parse("/tmp/a.elf"),
            RomArg::Path(PathBuf::from("/tmp/a.elf"))
        );
    }

    /// Run `cargo test -p pemu-loader --no-default-features` for the other side of this
    /// assertion (see [`bundled_rom`] for why this crate's feature is not the evidence).
    #[test]
    fn a_build_with_no_embedded_rom_needs_an_override() {
        let home = scratch("no-embedded-rom");
        let missing = no_embedded_rom(RomRev::Rev101);
        assert_eq!(missing.code_name(), "E_ASSET_MISSING");
        assert!(missing.to_string().contains("esp32c3_rev101_rom.elf"));
        let resolved = resolve_rom(&empty_env(&home), &RomOptions::default(), RomRev::Rev101);
        match pemu_loader::rom::bundled_opt(RomRev::Rev101) {
            None => assert_eq!(
                resolved.expect_err("no bundled ROM").code_name(),
                "E_ASSET_MISSING"
            ),
            Some(bytes) => {
                let rom = resolved.expect("the embedded ROM");
                assert_eq!((rom.origin, rom.pinned), (RomOrigin::Bundled, true));
                assert_eq!(rom.sha256, hex(&sha256(bytes)));
            }
        }
        let _ = fs::remove_dir_all(&home);
    }

    /// These tests need the embedded ROM bytes; `bundled_bytes` asserts they are really there,
    /// because a feature flag is not evidence of embedding.
    #[cfg(feature = "bundled-rom")]
    mod bundled {
        use super::*;

        fn bundled_bytes(rev: RomRev) -> &'static [u8] {
            pemu_loader::rom::bundled_opt(rev).expect("this build embeds the ROM")
        }

        #[test]
        fn the_default_is_the_bundled_pinned_rom_with_no_discovery() {
            let home = scratch("default");
            let rom = resolve_rom(&empty_env(&home), &RomOptions::default(), RomRev::Rev101)
                .expect("bundled ROM");
            assert_eq!(rom.origin, RomOrigin::Bundled);
            assert!(rom.pinned);
            assert_eq!(rom.pinned_rev, Some(RomRev::Rev101));
            assert_eq!(rom.path, None);
            assert_eq!(rom.sha256, hex(&sha256(bundled_bytes(RomRev::Rev101))));
            assert!(rom.sha256.starts_with("9495e1453f36f7ee"));
            let rev3 = resolve_rom(&empty_env(&home), &RomOptions::default(), RomRev::Rev3)
                .expect("bundled rev3 ROM");
            assert!(rev3.sha256.starts_with("19ac22e08707df92"));
            let _ = fs::remove_dir_all(&home);
        }

        #[test]
        fn an_override_is_pin_checked() {
            let home = scratch("override");
            let env = empty_env(&home);
            let bytes = bundled_bytes(RomRev::Rev101);
            let copy = home.join("copy-of-rom101.elf");
            write(&copy, bytes);
            let opts = RomOptions {
                rom: Some(RomArg::Path(copy.clone())),
                allow_unpinned_rom: false,
            };
            let rom = resolve_rom(&env, &opts, RomRev::Rev101).expect("identical copy");
            assert_eq!(rom.origin, RomOrigin::Cli);
            assert!(rom.pinned);
            assert_eq!(rom.pinned_rev, Some(RomRev::Rev101));
            assert_eq!(rom.path.as_deref(), Some(copy.as_path()));
            let bundled =
                resolve_rom(&env, &RomOptions::default(), RomRev::Rev101).expect("bundled");
            assert_eq!(rom.sha256, bundled.sha256);
            assert_eq!(rom.image.image_sha256(), bundled.image.image_sha256());

            let changed = home.join("changed-rom101.elf");
            let mut edited = bytes.to_vec();
            edited[8] ^= 1;
            write(&changed, &edited);
            let opts = RomOptions {
                rom: Some(RomArg::Path(changed.clone())),
                allow_unpinned_rom: false,
            };
            let error = resolve_rom(&env, &opts, RomRev::Rev101).expect_err("one byte changed");
            assert_eq!(error.code_name(), "E_ASSET_HASH");
            assert!(matches!(error, AssetError::Hash { ref path, .. } if path == &changed));

            let opts = RomOptions {
                rom: Some(RomArg::Path(changed.clone())),
                allow_unpinned_rom: true,
            };
            let rom = resolve_rom(&env, &opts, RomRev::Rev101).expect("--allow-unpinned-rom");
            assert!(!rom.pinned);
            assert_eq!(rom.pinned_rev, None);
            assert_ne!(rom.sha256, bundled.sha256);
            let report = discovery_report(&env, &opts, RomRev::Rev101, false);
            let over = report.rom_override.expect("override reported");
            assert!(!over.pinned);
            assert_eq!(over.origin, RomOrigin::Cli);
            assert!(
                report.warnings.iter().any(|w| w.contains("not pinned")),
                "{:?}",
                report.warnings
            );
            let _ = fs::remove_dir_all(&home);
        }

        #[test]
        fn a_missing_override_file_is_asset_missing() {
            let home = scratch("missing-override");
            let opts = RomOptions {
                rom: Some(RomArg::Path(home.join("nope.elf"))),
                allow_unpinned_rom: false,
            };
            let error =
                resolve_rom(&empty_env(&home), &opts, RomRev::Rev101).expect_err("no such file");
            assert_eq!(error.code_name(), "E_ASSET_MISSING");
            let _ = fs::remove_dir_all(&home);
        }
    }

    #[cfg(feature = "bundled-rom")]
    mod precedence {
        use super::*;

        #[test]
        fn the_first_override_wins_and_nothing_is_searched_implicitly() {
            let home = scratch("precedence");
            let bytes = pemu_loader::rom::bundled_opt(RomRev::Rev101)
                .expect("this build embeds the ROM")
                .to_vec();
            let cli = home.join("cli.elf");
            let from_env = home.join("env.elf");
            let from_config = home.join("config.elf");
            for path in [&cli, &from_env, &from_config] {
                write(path, &bytes);
            }
            let idf_dir = home
                .join(".espressif")
                .join(ESP_ROM_ELFS_SUBDIR)
                .join("20241011");
            write(&idf_dir.join(RomRev::Rev101.file_name()), &bytes);
            let mut env = empty_env(&home);
            env.espressif_dir = Some(home.join(".espressif"));
            write(
                &env.config_file(),
                format!(
                    "[rom]\nrev101 = \"{}\"\nrev3 = \"/nope/rev3.elf\"\n",
                    from_config.display()
                )
                .as_bytes(),
            );

            // 3: the config keys, when no flag and no environment variable is set.
            let rom =
                resolve_rom(&env, &RomOptions::default(), RomRev::Rev101).expect("config key");
            assert_eq!(rom.origin, RomOrigin::Config);
            assert_eq!(rom.path.as_deref(), Some(from_config.as_path()));
            // 2: the environment variable.
            env.rom_env = Some(from_env.display().to_string());
            let rom = resolve_rom(&env, &RomOptions::default(), RomRev::Rev101).expect("env");
            assert_eq!(rom.origin, RomOrigin::Env);
            assert_eq!(rom.path.as_deref(), Some(from_env.as_path()));
            // 1: the flag.
            let opts = RomOptions {
                rom: Some(RomArg::Path(cli.clone())),
                allow_unpinned_rom: false,
            };
            let rom = resolve_rom(&env, &opts, RomRev::Rev101).expect("flag");
            assert_eq!(rom.origin, RomOrigin::Cli);
            let opts = RomOptions {
                rom: Some(RomArg::Idf),
                allow_unpinned_rom: false,
            };
            let rom = resolve_rom(&env, &opts, RomRev::Rev101).expect("--rom idf");
            assert_eq!(rom.origin, RomOrigin::CliIdf);
            assert_eq!(
                rom.path.as_deref(),
                Some(idf_dir.join(RomRev::Rev101.file_name()).as_path())
            );
            env.rom_env = None;
            let _ = fs::remove_file(env.config_file());
            let rom = resolve_rom(&env, &RomOptions::default(), RomRev::Rev101).expect("bundled");
            assert_eq!(rom.origin, RomOrigin::Bundled);
            assert_eq!(rom.path, None);
            let _ = fs::remove_dir_all(&home);
        }

        #[test]
        fn rom_idf_takes_the_newest_directory_and_reports_a_miss() {
            let home = scratch("idf");
            let bytes = pemu_loader::rom::bundled(RomRev::Rev3).to_vec();
            let tools = home.join("idf-tools").join(ESP_ROM_ELFS_SUBDIR);
            write(
                &tools.join("20240101").join(RomRev::Rev3.file_name()),
                b"old",
            );
            write(
                &tools.join("20241011").join(RomRev::Rev3.file_name()),
                &bytes,
            );
            let mut env = empty_env(&home);
            env.idf_tools_path = Some(home.join("idf-tools"));
            let opts = RomOptions {
                rom: Some(RomArg::Idf),
                allow_unpinned_rom: false,
            };
            let rom = resolve_rom(&env, &opts, RomRev::Rev3).expect("newest esp-rom-elfs");
            assert!(rom.pinned);
            assert!(
                rom.path
                    .as_ref()
                    .expect("path")
                    .to_string_lossy()
                    .contains("20241011")
            );
            let error = resolve_rom(&env, &opts, RomRev::Rev101).expect_err("no rev101 copy");
            assert_eq!(error.code_name(), "E_ASSET_MISSING");
            let report = discovery_report(&env, &RomOptions::default(), RomRev::Rev101, false);
            assert_eq!(report.bundled_roms.len(), 2);
            assert!(report.bundled_roms.iter().all(|r| r.matches));
            assert_eq!(report.rom_override, None);
            assert!(report.warnings.iter().any(|w| w.contains("corpus map")));
            let _ = fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn the_corpus_map_expands_paths_and_checks_digests() {
        let home = scratch("corpus");
        let env = empty_env(&home);
        let image = home.join("corpus").join("demo").join("demo-merged.bin");
        write(&image, b"merged image");
        let digest = hex(&sha256(b"merged image"));
        write(
            &env.corpus_file(),
            format!(
                "[demo]\nbin = \"~/corpus/demo/demo-merged.bin\"\nsha256 = {{ bin = \"{digest}\" }}\n\n[probe-long]\nbin = \"~/corpus/probe-long/probe-long-8MB.bin\"\n"
            )
            .as_bytes(),
        );
        let corpus = Corpus::load(&env).expect("corpus map");
        assert_eq!(
            corpus
                .entries()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["demo", "probe-long"]
        );
        let demo = corpus.get("demo").expect("demo");
        assert_eq!(
            demo.file("bin").map(|f| f.path.clone()),
            Some(image.clone())
        );
        assert_eq!(corpus.read("demo", "bin").expect("read"), b"merged image");
        assert_eq!(
            corpus
                .read("probe-long", "bin")
                .expect_err("absent")
                .code_name(),
            "E_ASSET_MISSING"
        );
        assert_eq!(
            corpus.read("nope", "bin").expect_err("no id").code_name(),
            "E_ASSET_MISSING"
        );
        assert_eq!(
            corpus.read("demo", "elf").expect_err("no kind").code_name(),
            "E_ASSET_MISSING"
        );

        write(&image, b"other bytes");
        assert_eq!(
            corpus.read("demo", "bin").expect_err("digest").code_name(),
            "E_ASSET_HASH"
        );
        let other = home.join("elsewhere.bin");
        write(&other, b"env image");
        let mut env = env.clone();
        env.corpus_env = vec![
            ("demo".to_owned(), other.clone()),
            ("scan3".to_owned(), other.clone()),
        ];
        let corpus = Corpus::load(&env).expect("corpus map");
        assert_eq!(
            corpus
                .get("demo")
                .and_then(|e| e.file("bin"))
                .map(|f| f.path.clone()),
            Some(other)
        );
        assert_eq!(
            corpus.read("demo", "bin").expect("env override"),
            b"env image"
        );
        assert!(corpus.get("scan3").is_some());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_missing_corpus_map_is_asset_missing() {
        let home = scratch("no-corpus");
        let error = Corpus::load(&empty_env(&home)).expect_err("no corpus.toml");
        assert_eq!(error.code_name(), "E_ASSET_MISSING");
        assert!(error.to_string().contains("corpus.toml"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn an_efuse_dump_directory_imports_and_taints() {
        let home = scratch("efuse");
        let dir = home.join("efuse-dump");
        let synth = EfuseImage::synth(11);
        for (block, &words) in BLOCK_WORDS.iter().enumerate() {
            let bytes: Vec<u8> = synth.words()[block][..words]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect();
            write(&dir.join(format!("efuse_blk{block}.bin")), &bytes);
        }
        let imported = load_efuse_dump(&dir).expect("block files");
        assert!(imported.tainted());
        assert_eq!(imported.words(), synth.words());
        assert_eq!(imported.chip_revision().full(), 101);

        let file = home.join("efuse.bin");
        let bytes: Vec<u8> = synth
            .dump_words()
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        write(&file, &bytes);
        let imported = load_efuse_dump(&file).expect("dump file");
        assert!(imported.tainted());
        assert_eq!(imported.words(), synth.words());
        assert_eq!(
            load_efuse_dump(&home.join("empty-dir-does-not-exist"))
                .expect_err("no file")
                .code_name(),
            "E_ASSET_MISSING"
        );
        let empty = home.join("empty");
        fs::create_dir_all(&empty).expect("dir");
        assert_eq!(
            load_efuse_dump(&empty)
                .expect_err("no block files")
                .code_name(),
            "E_ASSET_MISSING"
        );
        write(&file, b"short");
        assert_eq!(
            load_efuse_dump(&file).expect_err("short dump").code_name(),
            "E_USAGE"
        );
        let _ = fs::remove_dir_all(&home);
    }
    /// `PASSPORTSIM_ROM=/nonexistent` makes `doctor` exit with `E_ASSET_MISSING`. The scratch
    /// corpus also shows the `found`, `missing` and `mismatched` statuses.
    #[test]
    fn a_report_reaches_doctor_and_a_missing_rom_env_is_asset_missing() {
        let home = scratch("doctor");
        let image = home.join("corpus").join("demo").join("demo-merged.bin");
        write(&image, b"merged image");
        let elf = home
            .join("corpus")
            .join("demo")
            .join("FoloToy-AI-Passport.elf");
        write(&elf, b"changed since corpus.toml was written");
        let mut env = empty_env(&home);
        write(
            &env.corpus_file(),
            format!(
                "[demo]\nbin = \"{bin}\"\nelf = \"{elf}\"\nsha256 = {{ bin = \"{good}\", elf = \"{bad}\" }}\n\n\
                 [goldminer]\nbin = \"~/corpus/goldminer/goldminer-sanitized-8MB.bin\"\n",
                bin = image.display(),
                elf = elf.display(),
                good = hex(&sha256(b"merged image")),
                bad = hex(&sha256(b"what corpus.toml pins")),
            )
            .as_bytes(),
        );

        let report = discovery_report(&env, &RomOptions::default(), RomRev::Rev101, true);
        let doctor_report = report.to_doctor_report();
        let demo = doctor_report
            .corpus
            .iter()
            .find(|e| e.id == "demo")
            .expect("demo");
        assert_eq!(demo.status(), doctor::Status::Mismatched);
        assert_eq!(demo.files[0].status(), doctor::Status::Found);
        assert_eq!(demo.files[1].status(), doctor::Status::Mismatched);
        let goldminer = doctor_report
            .corpus
            .iter()
            .find(|e| e.id == "goldminer")
            .expect("goldminer");
        assert_eq!(goldminer.status(), doctor::Status::Missing);
        for id in CORPUS_IDS {
            let entry = doctor_report
                .corpus
                .iter()
                .find(|e| e.id == id)
                .unwrap_or_else(|| panic!("{id} is not reported"));
            if id == "demo" {
                continue;
            }
            assert_eq!(entry.status(), doctor::Status::Missing, "{id}");
        }
        if pemu_loader::rom::bundled_opt(RomRev::Rev101).is_some() {
            assert!(report.problems.is_empty(), "{:?}", report.problems);
            assert_eq!(doctor_report.bundled_roms.len(), 2);
            assert_eq!(doctor_report.bundled_roms[0].rev, "rom101");
            assert_eq!(doctor_report.bundled_roms[1].rev, "rom3");
            assert!(
                doctor_report
                    .bundled_roms
                    .iter()
                    .all(|r| r.status() == doctor::Status::Found)
            );
            let error = doctor::run_report(&doctor_report).expect_err("one corpus digest differs");
            assert_eq!(error.code.name, "E_ASSET_HASH");
            let text = error.detail["text"].as_str().expect("rendered report");
            assert!(text.contains("demo.elf mismatched"), "{text}");
            assert!(!text.contains("merged image"), "{text}");
        } else {
            assert_eq!(report.problems.len(), 1);
            assert_eq!(report.problems[0].code, "E_ASSET_MISSING");
            let error = doctor::run_report(&doctor_report).expect_err("no ROM in this build");
            assert_eq!(error.code.name, "E_ASSET_MISSING");
            assert!(
                doctor_report
                    .bundled_roms
                    .iter()
                    .all(|r| r.status() == doctor::Status::Missing)
            );
        }

        env.rom_env = Some(home.join("nonexistent").display().to_string());
        let report = discovery_report(&env, &RomOptions::default(), RomRev::Rev101, false);
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].code, "E_ASSET_MISSING");
        assert_eq!(report.rom_override, None);
        let error = doctor::run_report(&report.to_doctor_report()).expect_err("no such ROM file");
        assert_eq!(error.code.name, "E_ASSET_MISSING");
        assert_eq!(error.code, pemu_api::error::E_ASSET_MISSING);
        assert!(error.message.contains("nonexistent"), "{}", error.message);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn doctor_reports_a_corpus_path_it_refused_to_resolve() {
        let home = scratch("doctor-refused-path");
        let mut env = empty_env(&home);
        env.config_dir = home.join(".config").join("passportsim");
        write(
            &env.corpus_file(),
            b"[official]\nbin = \"../../outside.bin\"\n",
        );

        let report = discovery_report(&env, &RomOptions::default(), RomRev::Rev101, false);
        let named: Vec<&String> = report
            .warnings
            .iter()
            .filter(|w| w.contains("climbs out of the data root"))
            .collect();
        assert_eq!(named.len(), 1, "{:?}", report.warnings);
        assert!(named[0].contains("corpus id `official`"), "{}", named[0]);
        let _ = fs::remove_dir_all(&home);
    }
}
