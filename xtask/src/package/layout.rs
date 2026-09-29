//! The package tree, the web bundle and the payload digest.
//!
//! ```text
//! passportsim-<ver>-<os-arch>/       passportsim-<ver>-web/
//!   passportsim[.exe]                  index.html  styles.css  main.js  worker.js  worklet.js
//!   receipt.json                       favicon.svg  favicon-32.png  apple-touch-icon.png
//!   LICENSE  THIRD_PARTY.md            pemu_wasm.wasm
//!   docs/quickstart.md                 official.pebundle          (when embedded)
//!   docs/deploy-cloudflare.md          LICENSE  THIRD_PARTY.md
//!   docs/commands/*.md  docs/errors.md licenses/esp-rom-elfs.LICENSE
//!   skills/passportsim/**              licenses/esp-rom-elfs.NOTICE
//!   assets/rom/{LICENSE,NOTICE,pins.toml}  licenses/official-demo.LICENSE  (when embedded)
//!   payload/schema/**                  licenses/official-demo.NOTICE  (when embedded)
//!   payload/web/**                     install.sh  install.ps1
//!   payload/firmware/**   (when embedded)  wrangler.jsonc  _headers  .assetsignore
//!   docs/secrets.md
//!   docs/i18n/{zh-CN,ja,fr}/{quickstart,secrets,deploy-cloudflare}.md
//! ```
//!
//! Shipped documents keep their repository paths, so links resolve as in a checkout. The payload
//! is every file except the binary and `receipt.json`, digested host-independently as SHA-256 over
//! `"<file sha256 hex>  <path>\n"` of each file, sorted by `/`-separated path.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::secrets::device::Env;

use super::demo::{self, Found};
use super::{Built, archive, cloudflare, guard, receipt, windows};

pub struct Inputs<'a> {
    pub binary: &'a Path,
    /// The `pemu_wasm.wasm` core.
    pub wasm: &'a Path,
    /// `web/dist`, as `bun run build` left it.
    pub web: &'a Path,
    /// Whether this host contributed the demo.
    pub demo: &'a Found,
    pub target: &'a str,
    /// Where the secret guard ([`guard`]) looks for the local hash file.
    pub secrets_env: &'a Env,
    /// The version in the package names and the receipt.
    pub version: &'a str,
    /// The checkout the receipt records.
    pub checkout: &'a receipt::Checkout,
}

/// Package directory suffix and binary file name of one target. Nothing else hardcodes either, so
/// the Windows package cannot put `passportsim.exe` into the payload, which must be byte-identical
/// across hosts.
pub fn target_shape(target: &str) -> Result<(&'static str, &'static str), String> {
    match target {
        "aarch64-apple-darwin" => Ok(("macos-arm64", "passportsim")),
        "x86_64-pc-windows-msvc" => Ok(("windows-x64", "passportsim.exe")),
        "aarch64-pc-windows-msvc" => Ok(("windows-arm64", "passportsim.exe")),
        other => Err(format!(
            "target `{other}` has no package shape; the supported hosts are macOS on Apple \
             Silicon and Windows"
        )),
    }
}

/// Files `bun run build` publishes (`web/package.json`). `worklet.js` is the audio worklet
/// `main.js` loads; without it a packaged page has no audio. The three icons are the ones
/// `index.html` links.
const WEB_FILES: [&str; 8] = [
    "index.html",
    "styles.css",
    "main.js",
    "worker.js",
    "worklet.js",
    "favicon.svg",
    "favicon-32.png",
    "apple-touch-icon.png",
];

/// The wasm core's name, which `web/src/worker/worker.ts` asks for beside `worker.js`.
const WASM_FILE: &str = "pemu_wasm.wasm";

/// The CLI installers, from `scripts/`, which the web bundle serves at its root so the one-line
/// install commands of the README fetch them from the site.
pub const INSTALLERS: [&str; 2] = ["install.sh", "install.ps1"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadFile {
    /// Path inside the package, `/` separated.
    pub path: String,
    pub size: u64,
    /// Lowercase hex SHA-256.
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadDigest {
    pub sha256: String,
    pub files: Vec<PayloadFile>,
}

/// The receipt is not payload: it records the digest, so it cannot be inside it.
const RECEIPT_FILE: &str = "receipt.json";

const ROM_DIR: &str = "assets/rom";

/// What `assets/rom/` contributes; the ROM ELFs themselves are compiled into the binary.
const ROM_LICENCE_FILES: [&str; 3] = ["LICENSE", "NOTICE", "pins.toml"];

/// Documents the package carries at their repository paths, with the translations under `i18n/`
/// that their language lines link. `errors.md` is generated.
const DOCS: [&str; 13] = [
    "quickstart.md",
    "errors.md",
    "secrets.md",
    "deploy-cloudflare.md",
    "i18n/zh-CN/quickstart.md",
    "i18n/zh-CN/secrets.md",
    "i18n/zh-CN/deploy-cloudflare.md",
    "i18n/ja/quickstart.md",
    "i18n/ja/secrets.md",
    "i18n/ja/deploy-cloudflare.md",
    "i18n/fr/quickstart.md",
    "i18n/fr/secrets.md",
    "i18n/fr/deploy-cloudflare.md",
];

pub fn write(root: &Path, out: &Path, inputs: &Inputs<'_>) -> Result<Built, String> {
    let version = inputs.version;
    let (suffix, binary_name) = target_shape(inputs.target)?;
    let package_dir = out.join(format!("passportsim-{version}-{suffix}"));
    let web_dir = out.join(format!("passportsim-{version}-web"));
    for dir in [&package_dir, &web_dir] {
        if dir.exists() {
            fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
    }
    let web_assets = write_web_bundle(root, &web_dir, inputs)?;
    write_package(root, &package_dir, inputs)?;
    let payload = digest(&package_dir, binary_name)?;
    // The secret guard runs before the receipt is written, so the receipt can record which rule sets
    // ran; the receipt is then scanned on its own.
    let package_wasm = format!("payload/web/{WASM_FILE}");
    let secrets = guard::scan(
        &[
            (&package_dir, &[binary_name, &package_wasm]),
            (&web_dir, &[WASM_FILE]),
        ],
        inputs.secrets_env,
    )?;
    let receipt = receipt::build(root, inputs, &payload, secrets)?;
    write_file(
        &package_dir.join(RECEIPT_FILE),
        receipt.to_json_text().as_bytes(),
    )?;
    guard::scan_one(&package_dir, RECEIPT_FILE, inputs.secrets_env)?;
    Ok(Built {
        package_archive: None,
        web_archive: None,
        package_dir,
        web_dir,
        web_assets,
        receipt,
    })
}

/// [`write`] plus the two archives, for tests that need a package without a CLI build.
#[cfg(test)]
pub fn write_all(
    root: &Path,
    out: &Path,
    inputs: &Inputs<'_>,
    archives: bool,
) -> Result<Built, String> {
    let mut built = write(root, out, inputs)?;
    if archives {
        archive_both(&mut built)?;
    }
    Ok(built)
}

pub fn archive_format(target: &str) -> archive::Format {
    if target.contains("-windows-") {
        archive::Format::Zip
    } else {
        archive::Format::TarGz
    }
}

/// Writes the two archives of a finished package in its target's format, the web bundle included.
pub fn archive_both(built: &mut Built) -> Result<(), String> {
    let target = built.receipt.target.clone();
    let format = archive_format(&target);
    let (_, binary_name) = target_shape(&target)?;
    built.package_archive = Some(archive::write(&built.package_dir, format, &[binary_name])?);
    built.web_archive = Some(archive::write(&built.web_dir, format, &[])?);
    Ok(())
}

/// Puts `binary`, built with the package's own payload embedded, into a package [`write`]
/// produced, and rewrites the receipt. The ROM measurement is retaken on it, both new files are
/// scanned, and a Windows binary is audited ([`windows::audit`]), all before any archive exists.
pub fn embed_binary(
    root: &Path,
    inputs: &Inputs<'_>,
    mut built: Built,
    binary: &Path,
) -> Result<Built, String> {
    let (_, binary_name) = target_shape(inputs.target)?;
    copy(binary, &built.package_dir.join(binary_name))?;
    let payload = digest(&built.package_dir, binary_name)?;
    if payload != built.receipt.payload {
        return Err(format!(
            "the payload digest moved from {} to {} when the embedding binary was put in; the \
             binary must be the only file that changed",
            built.receipt.payload.sha256, payload.sha256
        ));
    }
    let shipped = Inputs {
        binary,
        wasm: inputs.wasm,
        web: inputs.web,
        demo: inputs.demo,
        target: inputs.target,
        secrets_env: inputs.secrets_env,
        version: inputs.version,
        checkout: inputs.checkout,
    };
    built.receipt.roms_embedded = receipt::rom_report(root, &shipped)?;
    built.receipt.payload_embedded = true;
    if inputs.target.contains("-windows-") {
        let bytes = fs::read(binary).map_err(|e| format!("{}: {e}", binary.display()))?;
        built.receipt.windows = Some(windows::audit(&bytes, inputs.target)?);
    }
    write_file(
        &built.package_dir.join(RECEIPT_FILE),
        built.receipt.to_json_text().as_bytes(),
    )?;
    guard::scan_built_binary(&built.package_dir, binary_name, inputs.secrets_env)?;
    guard::scan_one(&built.package_dir, RECEIPT_FILE, inputs.secrets_env)?;
    Ok(built)
}

/// The static web bundle, servable at any path and deployable as a Cloudflare Workers project.
fn write_web_bundle(
    root: &Path,
    dir: &Path,
    inputs: &Inputs<'_>,
) -> Result<cloudflare::Assets, String> {
    for name in WEB_FILES {
        copy(&inputs.web.join(name), &dir.join(name))?;
    }
    copy(inputs.wasm, &dir.join(WASM_FILE))?;
    copy(&root.join("LICENSE"), &dir.join("LICENSE"))?;
    copy(&root.join("THIRD_PARTY.md"), &dir.join("THIRD_PARTY.md"))?;
    for name in INSTALLERS {
        copy(&root.join("scripts").join(name), &dir.join(name))?;
    }
    let rom = root.join("assets/rom");
    copy(
        &rom.join("LICENSE"),
        &dir.join("licenses/esp-rom-elfs.LICENSE"),
    )?;
    copy(
        &rom.join("NOTICE"),
        &dir.join("licenses/esp-rom-elfs.NOTICE"),
    )?;
    if let Found::Embedded(embedded) = inputs.demo {
        write_file(&dir.join(demo::BUNDLE_FILE), &embedded.bundle)?;
        write_file(
            &dir.join("licenses/official-demo.LICENSE"),
            embedded.license.as_bytes(),
        )?;
        write_file(
            &dir.join("licenses/official-demo.NOTICE"),
            embedded.notice().as_bytes(),
        )?;
    }
    cloudflare::write(dir)?;
    cloudflare::check(dir)
}

fn write_package(root: &Path, dir: &Path, inputs: &Inputs<'_>) -> Result<(), String> {
    let (_, binary_name) = target_shape(inputs.target)?;
    copy(inputs.binary, &dir.join(binary_name))?;
    copy(&root.join("LICENSE"), &dir.join("LICENSE"))?;
    copy(&root.join("THIRD_PARTY.md"), &dir.join("THIRD_PARTY.md"))?;
    // ARCHITECTURE.md stays out: it is for contributors.
    for name in DOCS {
        copy(&root.join("docs").join(name), &dir.join("docs").join(name))?;
    }
    copy_dir(
        &root.join("docs/commands"),
        &dir.join("docs/commands"),
        "md",
    )?;
    copy_tree(
        &root.join("skills/passportsim"),
        &dir.join("skills/passportsim"),
    )?;
    let rom = root.join(ROM_DIR);
    for name in ROM_LICENCE_FILES {
        copy(&rom.join(name), &dir.join(ROM_DIR).join(name))?;
    }
    copy(
        &root.join("docs/schema/error@1.json"),
        &dir.join("payload/schema/error@1.json"),
    )?;
    copy_dir(
        &root.join("docs/schema/commands"),
        &dir.join("payload/schema/commands"),
        "json",
    )?;
    for name in WEB_FILES {
        copy(&inputs.web.join(name), &dir.join("payload/web").join(name))?;
    }
    copy(inputs.wasm, &dir.join("payload/web").join(WASM_FILE))?;
    if let Found::Embedded(embedded) = inputs.demo {
        let firmware = dir.join("payload/firmware");
        write_file(&firmware.join(demo::BUNDLE_FILE), &embedded.bundle)?;
        write_file(
            &firmware.join("official-demo.LICENSE"),
            embedded.license.as_bytes(),
        )?;
        write_file(
            &firmware.join("official-demo.NOTICE"),
            embedded.notice().as_bytes(),
        )?;
    }
    Ok(())
}

/// The payload digest of a written package directory. `binary_name` comes from [`target_shape`]:
/// excluding the wrong spelling would put the executable into the payload.
pub fn digest(package_dir: &Path, binary_name: &str) -> Result<PayloadDigest, String> {
    let not_payload = [binary_name, RECEIPT_FILE];
    let mut files = Vec::new();
    collect(package_dir, package_dir, &mut files)?;
    files.retain(|file| !not_payload.contains(&file.path.as_str()));
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hasher = Sha256::new();
    for file in &files {
        hasher.update(format!("{}  {}\n", file.sha256, file.path).as_bytes());
    }
    Ok(PayloadDigest {
        sha256: hex(&hasher.finalize()),
        files,
    })
}

fn collect(base: &Path, dir: &Path, out: &mut Vec<PayloadFile>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect(base, &path, out)?;
            continue;
        }
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let relative = path
            .strip_prefix(base)
            .map_err(|_| format!("{} is not below {}", path.display(), base.display()))?;
        out.push(PayloadFile {
            path: relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
            size: bytes.len() as u64,
            sha256: hex(&Sha256::digest(&bytes)),
        });
    }
    Ok(())
}

/// Window taken from the middle of a ROM ELF, away from the header every ELF of the toolchain
/// shares.
const ROM_WINDOW: usize = 256;

/// Whether a produced artifact carries the bytes of a bundled ROM ELF. The linker keeps
/// `pemu-loader`'s `include_bytes!` only when a caller reaches `pemu_loader::rom::bundled`, so the
/// receipt records this measurement. An unreadable file is an error, not `false`.
pub fn roms_embedded(artifact: &Path, rom: &Path) -> Result<bool, String> {
    let bytes = fs::read(artifact).map_err(|e| format!("{}: {e}", artifact.display()))?;
    let elf = fs::read(rom).map_err(|e| format!("{}: {e}", rom.display()))?;
    let start = elf.len() / 2;
    let window = elf.get(start..start + ROM_WINDOW).ok_or_else(|| {
        format!(
            "{} is {} bytes, too short to take a {ROM_WINDOW}-byte window from its middle; it is \
             not a ROM ELF",
            rom.display(),
            elf.len()
        )
    })?;
    Ok(bytes.windows(window.len()).any(|slice| slice == window))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))
}

fn write_file(to: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(to, bytes).map_err(|e| format!("{}: {e}", to.display()))
}

/// Copies every file of `from` with the given extension, without recursing.
fn copy_dir(from: &Path, to: &Path, extension: &str) -> Result<(), String> {
    let entries = fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let mut copied = 0usize;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("{}: {e}", from.display()))?
            .path();
        if path.extension().is_some_and(|ext| ext == extension) {
            let Some(name) = path.file_name() else {
                continue;
            };
            copy(&path, &to.join(name))?;
            copied += 1;
        }
    }
    if copied == 0 {
        return Err(format!(
            "{} holds no .{extension} file; run `cargo run -q -p xtask -- docs` first",
            from.display()
        ));
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let entries = fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("{}: {e}", from.display()))?
            .path();
        let Some(name) = path.file_name() else {
            continue;
        };
        if path.is_dir() {
            copy_tree(&path, &to.join(name))?;
        } else {
            copy(&path, &to.join(name))?;
        }
    }
    Ok(())
}
