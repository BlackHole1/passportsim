//! The prebuilt official demo bundle a package carries so the emulator boots with no setup.
//!
//! The image is the `official` corpus entry, the FoloToy AI Passport BSP demo built from upstream
//! commit `f75873f` (MIT). It is never committed, so only a host with the corpus embeds it from
//! there, after checking the digests `corpus.toml` pins against the compiled-in
//! [`PINNED_PREFIXES`], the secret-pattern rules, the MIT text and the upstream commit. Other hosts
//! take it from a package (`--payload-from`, [`from_package`]) or a directory (`--demo`,
//! [`from_dir`]) under the same checks. No receipt field or refusal names a path.

use std::path::{Path, PathBuf};

use pemu_loader::bundle::{BundleInput, build as build_bundle};

use super::elf_paths;
use crate::ci::corpus::sha256_file;
use crate::hostdirs::{self, expand_home};
use crate::secrets::patterns;

pub const DEMO_ID: &str = "official";

/// The SHA-256 prefixes pinned for the `official` corpus entry, by file kind: the pin the
/// packaging host cannot move, since `corpus.toml` supplies both path and digest.
const PINNED_PREFIXES: [(&str, &str); 3] = [
    ("bin", "580285887df4e163"),
    ("elf", "dd63252a5675a2de"),
    ("boot_elf", "5fcf29a5d25417a8"),
];

/// Upstream commit the `official` image was built from.
pub const UPSTREAM_COMMIT: &str = "f75873f";

pub const BUNDLE_FILE: &str = "official.pebundle";

/// The upstream checkout the demo was built from, below the data root. Only its `LICENSE` is read.
const UPSTREAM_DIR: &str = "builds/official";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Demo {
    /// `.pebundle` bytes (`pemu_loader::bundle`), carrying the merged image and the app ELF.
    pub bundle: Vec<u8>,
    pub license: String,
    /// SHA-256 of the merged flash image, as `corpus.toml` pins it.
    pub bin_sha256: String,
    /// SHA-256 of the app ELF, as `corpus.toml` pins it.
    pub elf_sha256: String,
    /// SHA-256 of the app ELF the bundle carries: the pinned one with its build paths blanked
    /// ([`super::elf_paths`]).
    pub elf_shipped_sha256: String,
    pub elf_paths_blanked: usize,
    /// How [`UPSTREAM_COMMIT`] was established on the host that built the bundle.
    pub commit: CommitCheck,
    pub source: Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// This host's firmware corpus, through `corpus.toml`.
    Corpus,
    /// The `payload/firmware/` of a package built on another host (`--payload-from`).
    Package,
    /// A directory holding those three files, such as a verified download (`--demo`).
    Dir,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Corpus => "corpus",
            Source::Package => "package",
            Source::Dir => "demo-dir",
        }
    }
}

/// How the commit id the NOTICE asserts was established on the packaging host. A checkout at
/// another commit is not a variant: [`from_map`] refuses the demo, because the NOTICE would state a
/// falsehood.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitCheck {
    /// `git rev-parse --short HEAD` in the upstream checkout printed [`UPSTREAM_COMMIT`].
    Rechecked,
    /// It could not be asked. The clause says why, and goes into the NOTICE verbatim.
    NotRechecked(&'static str),
}

impl CommitCheck {
    pub fn rechecked(&self) -> bool {
        matches!(self, CommitCheck::Rechecked)
    }

    /// The parenthesized clause the NOTICE carries after the commit id.
    fn clause(&self) -> String {
        match self {
            CommitCheck::Rechecked => RECHECKED.to_string(),
            CommitCheck::NotRechecked(why) => {
                format!(
                    "the pinned id; {why}, and a checkout that disagreed would have refused the \
                     demo rather than shipped this line"
                )
            }
        }
    }
}

impl Demo {
    /// The notice shipped beside the image: what it is, where it came from, and its digests.
    pub fn notice(&self) -> String {
        let checked = self.commit.clause();
        format!(
            "FoloToy AI Passport BSP demo firmware\n\
             \n\
             This package carries a prebuilt demo image so that the emulator boots with no setup.\n\
             It is not part of the emulator and is not covered by the emulator's own LICENSE.\n\
             \n\
             Copyright (c) FoloToy. Licensed under the MIT License; the full text is in\n\
             official-demo.LICENSE beside this file, copied verbatim from the upstream repository.\n\
             \n\
             Upstream commit: {commit} ({checked})\n\
             Corpus id:       {id}\n\
             SHA-256, merged flash image: {bin}\n\
             SHA-256, application ELF:    {elf}\n\
             SHA-256, application ELF as shipped: {shipped}\n\
             Build paths blanked in it:   {blanked}\n\
             \n\
             The merged flash image is redistributed unmodified. In the application ELF, the\n\
             account part of each build path in its debug sections (for example `Users/<name>`)\n\
             is overwritten with `_`, in place and byte for byte; every other byte is the ELF\n\
             above. Neither is ever committed to the emulator repository; `cargo xtask package`\n\
             embeds them only after checking them against pinned digests.\n",
            commit = UPSTREAM_COMMIT,
            checked = checked,
            id = DEMO_ID,
            bin = self.bin_sha256,
            elf = self.elf_sha256,
            shipped = self.elf_shipped_sha256,
            blanked = self.elf_paths_blanked,
        )
    }
}

const COMMIT_LINE: &str = "Upstream commit:";
const BIN_LINE: &str = "SHA-256, merged flash image:";
const ELF_LINE: &str = "SHA-256, application ELF:";
const SHIPPED_LINE: &str = "SHA-256, application ELF as shipped:";
const BLANKED_LINE: &str = "Build paths blanked in it:";

const RECHECKED: &str = "re-checked against the upstream checkout on the packaging host";
const NO_GIT: &str = "`git` could not be run here";
const NO_CHECKOUT: &str = "the upstream checkout is not readable here";

/// Why no demo was embedded. Carries no path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Absent {
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    Embedded(Box<Demo>),
    Absent(Absent),
}

impl Found {
    pub fn absent_reason(&self) -> Option<&str> {
        match self {
            Found::Embedded(_) => None,
            Found::Absent(absent) => Some(&absent.reason),
        }
    }
}

fn absent(reason: impl Into<String>) -> Found {
    Found::Absent(Absent {
        reason: reason.into(),
    })
}

/// Looks for the demo on this host. A host without the corpus gets [`Found::Absent`]. The corpus is
/// resolved from the home role, never from the repository at `_root`.
pub fn find(_root: &Path) -> Result<Found, String> {
    if !cfg!(target_os = "macos") {
        return Ok(absent(
            "the firmware corpus is macOS-only; a Windows package takes \
             the demo of a macOS package with `--payload-from <package directory>`",
        ));
    }
    let Ok(home) = hostdirs::home() else {
        return Ok(absent("the home role does not resolve on this host"));
    };
    let map = home.join(crate::ci::corpus::CORPUS_FILE);
    let Ok(text) = std::fs::read_to_string(&map) else {
        return Ok(absent(
            "no firmware corpus map in the config role; a checkout without the \
             corpus packages without the demo",
        ));
    };
    Ok(from_map(&text, &home))
}

/// One file of the demo, read and checked against both pins and the secret-pattern rules.
struct Checked {
    kind: &'static str,
    flash: bool,
    bytes: Vec<u8>,
    sha256: String,
    dir: PathBuf,
}

struct FileKind {
    /// Corpus file kind, as `corpus.toml` and [`PINNED_PREFIXES`] spell it.
    kind: &'static str,
    role: &'static str,
    name: &'static str,
    /// Whether the bytes are the merged 8 MB flash image, so flash-offset rules (the cardid window)
    /// apply.
    flash: bool,
}

const FILES: [FileKind; 2] = [
    FileKind {
        kind: "bin",
        role: pemu_loader::bundle::BUNDLE_FLASH,
        name: "FoloToy-AI-Passport-8MB.bin",
        flash: true,
    },
    FileKind {
        kind: "elf",
        role: pemu_loader::bundle::BUNDLE_APP_ELF,
        name: "FoloToy-AI-Passport.elf",
        flash: false,
    },
];

/// [`find`] with the corpus text and home role given, so a test needs no host corpus.
pub fn from_map(text: &str, home: &Path) -> Found {
    from_map_with(text, home, &PINNED_PREFIXES)
}

/// [`from_map`] with the prefixes given: a synthetic test file cannot match the real prefix.
pub fn from_map_with(text: &str, home: &Path, pins: &[(&str, &str)]) -> Found {
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(_) => return absent("the firmware corpus map does not parse as TOML"),
    };
    let Some(entry) = table.get(DEMO_ID).and_then(toml::Value::as_table) else {
        return absent(format!("the firmware corpus map has no `{DEMO_ID}` entry"));
    };
    let hashes = entry.get("sha256").and_then(toml::Value::as_table);
    let mut files: Vec<Checked> = Vec::new();
    for file in &FILES {
        match check_file(entry, hashes, file, pins, home) {
            Ok(checked) => files.push(checked),
            Err(found) => return found,
        }
    }
    for file in &files {
        if let Some(reason) = secrets_hit(file) {
            return absent(reason);
        }
    }
    let (Some(bin), Some(elf)) = (files.first(), files.get(1)) else {
        return absent("the demo bundle needs both the merged image and the application ELF");
    };
    let Some(license) = upstream_license(&bin.dir, home) else {
        return absent(
            "the upstream MIT license text of the demo is not on this host, and a binary is \
             never shipped without its license",
        );
    };
    let commit = match upstream_head(&upstream_dir(home)) {
        Head::At(head) if head == UPSTREAM_COMMIT => CommitCheck::Rechecked,
        Head::At(head) => {
            return absent(format!(
                "the upstream checkout on this host is at `{head}`, not the pinned \
                 `{UPSTREAM_COMMIT}`; the NOTICE beside the image asserts that commit id, so a \
                 checkout that disagrees drops the demo instead of shipping a false licence line"
            ));
        }
        Head::NoGit => CommitCheck::NotRechecked(NO_GIT),
        Head::NotACheckout => CommitCheck::NotRechecked(NO_CHECKOUT),
    };
    let shipped = match elf_paths::blank(&elf.bytes) {
        Ok(shipped) => shipped,
        Err(why) => {
            return absent(format!(
                "the demo's application ELF cannot have its build paths blanked, and a package \
                 never names the account that built its firmware: {why}"
            ));
        }
    };
    let bundle = bundle_of(&bin.bytes, &shipped.bytes);
    Found::Embedded(Box::new(Demo {
        bundle,
        license,
        bin_sha256: bin.sha256.clone(),
        elf_sha256: elf.sha256.clone(),
        elf_shipped_sha256: sha256_hex(&shipped.bytes),
        elf_paths_blanked: shipped.paths,
        commit,
        source: Source::Corpus,
    }))
}

/// The `.pebundle` of the merged image and the application ELF, the one writer both demo sources
/// share, so a package's bundle can be rebuilt from the two files it holds.
fn bundle_of(bin: &[u8], elf: &[u8]) -> Vec<u8> {
    let [image, app] = &FILES;
    build_bundle(
        Some(DEMO_ID),
        Some("FoloToy AI Passport BSP demo"),
        &[
            BundleInput {
                role: image.role,
                name: image.name,
                bytes: bin,
            },
            BundleInput {
                role: app.role,
                name: app.name,
                bytes: elf,
            },
        ],
    )
}

fn notice_field<'a>(notice: &'a str, label: &str) -> Option<&'a str> {
    notice
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .map(str::trim)
}

const PACKAGE_FIRMWARE: &str = "payload/firmware";

const LICENSE_FILE: &str = "official-demo.LICENSE";
const NOTICE_FILE: &str = "official-demo.NOTICE";

/// The demo of the package directory `dir` (`--payload-from`). Unlike [`find`], a package with no
/// demo or one that fails a check is an error: the caller named it.
pub fn from_package(dir: &Path) -> Result<Found, String> {
    from_package_with(dir, &PINNED_PREFIXES)
}

/// [`from_package`] with the prefixes given. Both packages come from the same tree, so the notice
/// must also be the one this commit writes, byte for byte.
pub fn from_package_with(dir: &Path, pins: &[(&str, &str)]) -> Result<Found, String> {
    let flag = "--payload-from";
    let (demo, notice) = from_files(&dir.join(PACKAGE_FIRMWARE), pins, flag, Source::Package)?;
    if demo.notice() != notice {
        return Err(format!(
            "{flag}: the demo is refused: `{NOTICE_FILE}` is not a notice this commit writes for \
             the bundle's two digests"
        ));
    }
    Ok(Found::Embedded(Box::new(demo)))
}

/// The demo in `dir`, which holds the three files of a package's `payload/firmware/` (`--demo`),
/// such as a CI runner's pinned download. The notice may come from an older commit, so it only has
/// to state the digests of the files beside it; the package gets the notice this commit writes.
pub fn from_dir(dir: &Path) -> Result<Found, String> {
    from_dir_with(dir, &PINNED_PREFIXES)
}

/// [`from_dir`] with the prefixes given.
pub fn from_dir_with(dir: &Path, pins: &[(&str, &str)]) -> Result<Found, String> {
    let (demo, _) = from_files(dir, pins, "--demo", Source::Dir)?;
    Ok(Found::Embedded(Box::new(demo)))
}

/// Reads and checks the bundle, licence and notice in `firmware`. `flag` names the option in every
/// refusal.
fn from_files(
    firmware: &Path,
    pins: &[(&str, &str)],
    flag: &str,
    source: Source,
) -> Result<(Demo, String), String> {
    let read = |name: &str| {
        std::fs::read(firmware.join(name)).map_err(|e| {
            let place = match source {
                Source::Package => format!("`{PACKAGE_FIRMWARE}/{name}` of the named package"),
                _ => format!("`{name}` in the named directory"),
            };
            format!(
                "{flag}: {place} is not readable ({e}); it must hold the demo files that `xtask \
                 package` writes to `{PACKAGE_FIRMWARE}/`"
            )
        })
    };
    let bundle = read(BUNDLE_FILE)?;
    let license = String::from_utf8(read(LICENSE_FILE)?)
        .map_err(|_| format!("{flag}: `{LICENSE_FILE}` is not UTF-8 text"))?;
    let notice = String::from_utf8(read(NOTICE_FILE)?)
        .map_err(|_| format!("{flag}: `{NOTICE_FILE}` is not UTF-8 text"))?;
    let refused = |why: String| format!("{flag}: the demo is refused: {why}");

    let parsed = pemu_loader::bundle::Bundle::parse(&bundle)
        .map_err(|e| refused(format!("`{BUNDLE_FILE}` does not parse ({e:?})")))?;
    if parsed.id() != Some(DEMO_ID) {
        return Err(refused(format!(
            "`{BUNDLE_FILE}` is not the `{DEMO_ID}` bundle"
        )));
    }
    // The shipped ELF has its build paths blanked, so the pin applies to the source ELF the notice
    // names, and the bundle's ELF must hold no profile path.
    let field = |label: &str| {
        notice_field(&notice, label)
            .map(str::to_string)
            .ok_or_else(|| refused(format!("`{NOTICE_FILE}` has no `{label}` line")))
    };
    let source_elf = field(ELF_LINE)?.to_ascii_lowercase();
    let blanked: usize = field(BLANKED_LINE)?
        .parse()
        .map_err(|_| refused(format!("`{NOTICE_FILE}` states no blanked-path count")))?;
    let commit = commit_of(&field(COMMIT_LINE)?).ok_or_else(|| {
        refused(format!(
            "`{NOTICE_FILE}` does not name commit {UPSTREAM_COMMIT}"
        ))
    })?;
    let mut files: Vec<Checked> = Vec::new();
    for file in &FILES {
        let bytes = parsed
            .role_data(file.role)
            .ok_or_else(|| refused(format!("the bundle has no `{}` role", file.role)))?
            .to_vec();
        let sha256 = sha256_hex(&bytes);
        let prefix = pins
            .iter()
            .find_map(|(name, prefix)| (*name == file.kind).then_some(*prefix))
            .ok_or_else(|| refused(format!("no SHA-256 prefix is pinned for `{}`", file.kind)))?;
        let pinned = if file.kind == "elf" {
            if elf_paths::blank(&bytes).map(|again| again.paths) != Ok(0) {
                return Err(refused(
                    "its application ELF still holds a build path, or is not one this commit \
                     can check"
                        .into(),
                ));
            }
            &source_elf
        } else {
            &sha256
        };
        if !pinned.starts_with(prefix) {
            return Err(refused(format!(
                "its `{}` does not start with the pinned SHA-256 prefix for it",
                file.kind
            )));
        }
        files.push(Checked {
            kind: file.kind,
            flash: file.flash,
            bytes,
            sha256,
            dir: firmware.to_path_buf(),
        });
    }
    for file in &files {
        if let Some(reason) = secrets_hit(file) {
            return Err(refused(reason));
        }
    }
    let (Some(bin), Some(elf)) = (files.first(), files.get(1)) else {
        return Err(refused("the bundle needs the image and the ELF".into()));
    };
    if bundle_of(&bin.bytes, &elf.bytes) != bundle {
        return Err(refused(format!(
            "`{BUNDLE_FILE}` is not the bundle this commit writes from the two files it holds"
        )));
    }
    // The notice's other two digests must be the files' own, or it describes other files.
    for (label, actual) in [(BIN_LINE, &bin.sha256), (SHIPPED_LINE, &elf.sha256)] {
        if !field(label)?.eq_ignore_ascii_case(actual) {
            return Err(refused(format!(
                "`{NOTICE_FILE}` states another `{label}` digest than the bundle's"
            )));
        }
    }
    if !is_mit(&license) {
        return Err(refused(format!(
            "`{LICENSE_FILE}` is not the upstream MIT licence text"
        )));
    }
    let demo = Demo {
        bundle,
        license,
        bin_sha256: bin.sha256.clone(),
        elf_sha256: source_elf,
        elf_shipped_sha256: elf.sha256.clone(),
        elf_paths_blanked: blanked,
        commit,
        source,
    };
    Ok((demo, notice))
}

/// The commit verdict of a notice's `Upstream commit:` value, or `None` when it names another
/// commit or no verdict this module writes.
fn commit_of(value: &str) -> Option<CommitCheck> {
    let clause = value.strip_prefix(UPSTREAM_COMMIT)?;
    [NO_GIT, NO_CHECKOUT]
        .into_iter()
        .find(|why| clause.contains(why))
        .map(CommitCheck::NotRechecked)
        .or_else(|| clause.contains(RECHECKED).then_some(CommitCheck::Rechecked))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The secret-pattern rule that refuses one corpus file, if any: the predicates of
/// `secrets/patterns.rs`. The reason names the rule, the kind and the offset, never a path.
fn secrets_hit(file: &Checked) -> Option<String> {
    let kind = file.kind;
    let refused = |rule: &str, at: String| {
        Some(format!(
            "`{DEMO_ID}` `{kind}` is refused by the secret-pattern `{rule}` rule ({at}); a package never \
             ships bytes that the secret guard would refuse to commit"
        ))
    };
    if let Some(&offset) = patterns::mac_shape_offsets(&file.bytes).first() {
        return refused("mac-shape", format!("first at 0x{offset:x}"));
    }
    if patterns::efuse_dump_shape(&file.bytes) {
        return refused("efuse-dump", format!("{} bytes", file.bytes.len()));
    }
    if file.flash
        && let Some((offset, count)) = patterns::cardid_window(&file.bytes)
    {
        return refused(
            "cardid-window",
            format!("{count} non-0xFF byte(s), first at 0x{offset:x}"),
        );
    }
    None
}

/// Reads and checks one corpus file, or the reason the demo cannot be embedded.
fn check_file(
    entry: &toml::Table,
    hashes: Option<&toml::Table>,
    file: &'static FileKind,
    pins: &[(&str, &str)],
    home: &Path,
) -> Result<Checked, Found> {
    let kind = file.kind;
    let Some(path) = entry.get(kind).and_then(toml::Value::as_str) else {
        return Err(absent(format!(
            "the firmware corpus map lists no `{kind}` for `{DEMO_ID}`"
        )));
    };
    let path = expand_home(path, home);
    let Some(pinned) = hashes
        .and_then(|h| h.get(kind))
        .and_then(toml::Value::as_str)
    else {
        return Err(absent(format!(
            "the firmware corpus map pins no SHA-256 for `{DEMO_ID}` `{kind}`; an unpinned image \
             is never shipped"
        )));
    };
    let Some(actual) = sha256_file(&path) else {
        return Err(absent(format!(
            "`{DEMO_ID}` `{kind}` is not readable on this host"
        )));
    };
    if !actual.eq_ignore_ascii_case(pinned) {
        return Err(absent(format!(
            "`{DEMO_ID}` `{kind}` does not match the SHA-256 the corpus map pins for it; a package \
             never carries an image the corpus no longer vouches for"
        )));
    }
    let Some(prefix) = pins
        .iter()
        .find_map(|(name, prefix)| (*name == kind).then_some(*prefix))
    else {
        return Err(absent(format!(
            "no SHA-256 prefix is pinned for `{DEMO_ID}` `{kind}`, so nothing outside the \
             corpus map vouches for it"
        )));
    };
    if !actual.to_ascii_lowercase().starts_with(prefix) {
        return Err(absent(format!(
            "`{DEMO_ID}` `{kind}` does not start with the pinned SHA-256 prefix for it; the \
             corpus map supplies both the path and its own digest, so only the compiled-in \
             prefix can say which file `{DEMO_ID}` is"
        )));
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return Err(absent(format!(
            "`{DEMO_ID}` `{kind}` became unreadable while packaging"
        )));
    };
    Ok(Checked {
        kind,
        flash: file.flash,
        bytes,
        sha256: actual.to_ascii_lowercase(),
        dir: path.parent().unwrap_or(home).to_path_buf(),
    })
}

fn upstream_dir(home: &Path) -> PathBuf {
    home.join(hostdirs::DEFAULT_DATA_ROOT).join(UPSTREAM_DIR)
}

/// Sentences every copy of the MIT licence carries. The header alone is not enough, because the
/// NOTICE claims the file beside it is the MIT licence.
const MIT_MARKERS: [&str; 3] = [
    "MIT License",
    "Permission is hereby granted, free of charge",
    "THE SOFTWARE IS PROVIDED \"AS IS\"",
];

/// The upstream MIT license text: beside the image first, then in the checkout it was built from.
fn upstream_license(image_dir: &Path, home: &Path) -> Option<String> {
    [
        image_dir.join("LICENSE"),
        upstream_dir(home).join("LICENSE"),
    ]
    .into_iter()
    .find_map(|path| std::fs::read_to_string(path).ok())
    .filter(|text| is_mit(text))
}

fn is_mit(text: &str) -> bool {
    text.starts_with(MIT_MARKERS[0]) && MIT_MARKERS[1..].iter().all(|m| text.contains(m))
}

/// What `git rev-parse --short HEAD` said about the upstream checkout. The NOTICE words the causes
/// apart.
enum Head {
    /// `git` ran and printed a short commit id.
    At(String),
    /// `git` could not be run at all on this host.
    NoGit,
    /// `git` ran and failed: no checkout at that path, or no `HEAD` in it.
    NotACheckout,
}

fn upstream_head(dir: &Path) -> Head {
    // A missing working directory fails the spawn like a missing `git`, so it is decided first.
    if !dir.is_dir() {
        return Head::NotACheckout;
    }
    let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
    else {
        return Head::NoGit;
    };
    if !output.status.success() {
        return Head::NotACheckout;
    }
    Head::At(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
