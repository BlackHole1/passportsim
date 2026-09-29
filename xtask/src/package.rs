//! `cargo xtask package`: the release package and the static web bundle.
//!
//! ```text
//! cargo xtask package --target <triple> [--out <dir>] [--no-archive] [--version <x.y.z>]
//!                     [--payload-from <package dir> | --demo <dir>]
//! ```
//!
//! Writes `passportsim-<ver>-<os-arch>/` ([`layout`]) and `passportsim-<ver>-web/` under `--out`
//! (default `<target dir>/package`), each also archived unless `--no-archive` ([`archive`]).
//!
//! The binary embeds the payload: the tree is written around a payload-less binary, `pemu-cli` is
//! rebuilt with `PASSPORTSIM_EMBED_PAYLOAD` naming it (its `build.rs` checks `payload.sha256`),
//! and the rebuilt binary replaces the first. Each target is built on its own host. A Windows
//! package takes the wasm core and the demo from a macOS package (`--payload-from`, [`source`]),
//! because the corpus is macOS-only and rustc writes host paths into the wasm core.
//!
//! `--version` is how the release workflow names a version it has not committed ([`version`]).

mod account;
mod archive;
mod cloudflare;
mod demo;
mod elf_paths;
mod guard;
mod layout;
mod pe;
mod receipt;
mod source;
#[cfg(test)]
mod tests;
mod version;
mod windows;

use std::path::{Path, PathBuf};

pub use demo::Found;
pub use receipt::Receipt;

const USAGE: &str = concat!(
    "usage: cargo xtask package --target <triple> [--out <dir>] [--no-archive] [--version <x.y.z>]\n",
    "                           [--payload-from <package dir> | --demo <dir>]\n",
    "       targets: aarch64-apple-darwin (built on macOS), x86_64-pc-windows-msvc (built on\n",
    "                Windows), aarch64-pc-windows-msvc (NOT_RUN: no Arm Windows host)\n",
    "       --payload-from takes the wasm core and the official demo out of a package of the\n",
    "                same clean tree, which is how a Windows package gets the payload of a macOS one\n",
    "       --demo   takes the official demo from a directory holding the three files of a\n",
    "                package's payload/firmware/ (official.pebundle, official-demo.LICENSE,\n",
    "                official-demo.NOTICE), for a host without the firmware corpus\n",
    "       --version builds a clean checkout as that version: it is written into Cargo.toml for\n",
    "                the build and put back afterwards; the release workflow passes it"
);

pub const MACOS_TARGET: &str = "aarch64-apple-darwin";

pub const WINDOWS_TARGETS: &[&str] = &[windows::X64_TARGET, windows::ARM64_TARGET];

/// Why `aarch64-pc-windows-msvc` is NOT_RUN: nothing could run or audit what it produced.
pub const ARM64_WINDOWS_REASON: &str = "no-arm-windows-host";

/// The target this host packages, if it packages any: the macOS target on macOS arm64, the x64
/// Windows target on Windows x64.
pub fn host_target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(MACOS_TARGET)
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        Some(windows::X64_TARGET)
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub target: String,
    /// Output directory; `None` means `<target dir>/package`.
    pub out: Option<PathBuf>,
    pub archive: bool,
    /// A package directory to take the wasm core and the demo from ([`source`]).
    pub payload_from: Option<PathBuf>,
    /// A directory to take the demo from ([`demo::from_dir`]).
    pub demo: Option<PathBuf>,
    /// The release version to build as, instead of the workspace version ([`version`]).
    pub version: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Built {
    pub package_dir: PathBuf,
    pub web_dir: PathBuf,
    /// What a Cloudflare Workers deploy of the web bundle would upload.
    pub web_assets: cloudflare::Assets,
    pub package_archive: Option<PathBuf>,
    pub web_archive: Option<PathBuf>,
    pub receipt: Receipt,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let Some(options) = parse(args)? else {
        return Ok(());
    };
    if options.target == windows::ARM64_TARGET {
        print_arm64_not_run();
        return Ok(());
    }
    let root = crate::codegen::workspace_root();
    let built = build(&root, &options)?;
    println!("package: {}", built.package_dir.display());
    println!("package: {}", built.web_dir.display());
    for archive in [&built.package_archive, &built.web_archive]
        .into_iter()
        .flatten()
    {
        println!("package: {}", archive.display());
    }
    // After the directory lines: `xtask ci` takes the first `package: ` line as the package.
    println!("package: {}", built.web_assets.summary());
    let secrets = &built.receipt.secrets;
    println!(
        "package: secrets-check clean over {} file(s): pattern rules yes, hashed rules {}",
        secrets.files_scanned,
        if secrets.hashed_rules {
            "yes"
        } else {
            "skipped (this host has no hash file)"
        }
    );
    println!(
        "package: payload {} ({} file(s)), demo {}",
        built.receipt.payload.sha256,
        built.receipt.payload.files.len(),
        built.receipt.demo.summary()
    );
    if let Some(audit) = &built.receipt.account {
        println!(
            "package: account audit: {} prefix(es) remapped ({}), {} artifact(s) name no \
             account",
            audit.remapped.len(),
            audit
                .remapped
                .iter()
                .map(|(what, _)| what.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            audit.searched.len()
        );
    }
    if let receipt::DemoRecord::Embedded {
        elf_paths_blanked, ..
    } = &built.receipt.demo
    {
        println!(
            "package: demo ELF: {elf_paths_blanked} build path(s) blanked in its debug sections"
        );
    }
    if let Some(audit) = &built.receipt.windows {
        println!(
            "package: windows audit: +crt-static, no redistributable import ({} DLL(s): {}), \
             manifest embedded ({})",
            audit.imports.len() + audit.delay_imports.len(),
            audit
                .imports
                .iter()
                .chain(&audit.delay_imports)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            &audit.manifest_sha256[..16]
        );
    }
    Ok(())
}

/// Parses the arguments; `Ok(None)` means `--help` was printed and there is nothing to do.
fn parse(args: &[String]) -> Result<Option<Options>, String> {
    let mut target: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut payload_from: Option<PathBuf> = None;
    let mut demo: Option<PathBuf> = None;
    let mut release_version: Option<String> = None;
    let mut archive = true;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--no-archive" => archive = false,
            "--target" => {
                let value = rest
                    .next()
                    .ok_or(format!("--target needs a triple\n{USAGE}"))?;
                target = Some(value.clone());
            }
            "--out" => {
                let value = rest
                    .next()
                    .ok_or(format!("--out needs a directory\n{USAGE}"))?;
                out = Some(PathBuf::from(value));
            }
            "--payload-from" => {
                let value = rest
                    .next()
                    .ok_or(format!("--payload-from needs a package directory\n{USAGE}"))?;
                payload_from = Some(PathBuf::from(value));
            }
            "--demo" => {
                let value = rest
                    .next()
                    .ok_or(format!("--demo needs a directory\n{USAGE}"))?;
                demo = Some(PathBuf::from(value));
            }
            "--version" => {
                let value = rest
                    .next()
                    .ok_or(format!("--version needs a version such as 0.2.0\n{USAGE}"))?;
                release_version = Some(version::parse(value)?);
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    let target = target.ok_or(format!("--target is required\n{USAGE}"))?;
    if payload_from.is_some() && demo.is_some() {
        return Err(format!(
            "--payload-from already supplies the demo, so it cannot be combined with --demo\n{USAGE}"
        ));
    }
    if target != MACOS_TARGET && !WINDOWS_TARGETS.contains(&target.as_str()) {
        return Err(format!(
            "target `{target}` is not a supported host; Linux is not supported at all \
            \n{USAGE}"
        ));
    }
    Ok(Some(Options {
        target,
        out,
        archive,
        payload_from,
        demo,
        version: release_version,
    }))
}

/// The NOT_RUN line of `aarch64-pc-windows-msvc`, worded as in `xtask ci` receipts.
fn print_arm64_not_run() {
    println!("package windows-arm64-package: NOT_RUN ({ARM64_WINDOWS_REASON})");
    println!(
        "package: `{}` is not built: this project has no Arm64 Windows host, so the package \
         could be linked (with the Visual Studio Arm64 build tools) but never run, audited under \
         load or put through the Windows end-to-end check, and an unrun package is not shipped. \
         No package was written.",
        windows::ARM64_TARGET
    );
}

/// Why `target` is not built on this host, or `None` when it is.
fn wrong_host(target: &str) -> Option<String> {
    if host_target() == Some(target) {
        return None;
    }
    let builder = match target {
        MACOS_TARGET => "macOS on Apple Silicon",
        windows::X64_TARGET => {
            "the Windows x64 host with the MSVC build tools (Windows binaries are linked on \
             Windows, and cargo-xwin is not used)"
        }
        _ => "no host of this project",
    };
    Some(format!(
        "target `{target}` is packaged on {builder}, not on this {}-{} host",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

/// Builds the package and the web bundle under `root`.
pub fn build(root: &Path, options: &Options) -> Result<Built, String> {
    if let Some(why) = wrong_host(&options.target) {
        return Err(why);
    }
    let source = open_source(root, options)?;
    let demo = match (&source, &options.demo) {
        (Some(source), _) => source.demo()?,
        (None, Some(dir)) => demo::from_dir(dir)?,
        (None, None) => demo::find(root)?,
    };
    build_from(root, options, demo, source.as_ref())
}

/// The `--payload-from` package of `options`, opened and checked ([`source::Source::open`]).
fn open_source(root: &Path, options: &Options) -> Result<Option<source::Source>, String> {
    options
        .payload_from
        .as_deref()
        .map(|dir| source::Source::open(dir, root, package_version(options)))
        .transpose()
}

/// The version this build names its package with: `--version`, else the workspace version.
fn package_version(options: &Options) -> &str {
    options
        .version
        .as_deref()
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// [`build`] with the demo already resolved: a `cfg(test)` build resolves no host directory
/// role, so `demo::find` finds nothing there.
#[cfg(test)]
pub fn build_with_demo(root: &Path, options: &Options, demo: Found) -> Result<Built, String> {
    if let Some(why) = wrong_host(&options.target) {
        return Err(why);
    }
    let source = open_source(root, options)?;
    build_from(root, options, demo, source.as_ref())
}

fn build_from(
    root: &Path,
    options: &Options,
    demo: Found,
    source: Option<&source::Source>,
) -> Result<Built, String> {
    crate::docs::run(&[])?;
    // Read before the stamp, which only a clean checkout gets: the receipt names the commit.
    let checkout = receipt::Checkout::read(root);
    let _stamp = match &options.version {
        Some(release) => {
            if checkout.dirty != Some(false) {
                return Err(format!(
                    "--version {release}: the checkout has uncommitted changes (or git could not \
                     read it), and a release is built from a clean commit only"
                ));
            }
            Some(version::Stamp::apply(root, release)?)
        }
        None => None,
    };
    let target_dir = target_dir(root);
    let account = account::Account::of_process()?;
    let remap = account::Remap::new(root, &target_dir, &account)?;
    let binary = build_cli(root, &options.target, &target_dir, &remap)?;
    let wasm = match source {
        Some(source) => source.wasm(),
        None => build_wasm(root, &target_dir, &remap)?,
    };
    let web = build_web(root)?;
    if let Some(reason) = demo.absent_reason() {
        println!("package: no demo embedded: {reason}");
    }
    let out = options
        .out
        .clone()
        .unwrap_or_else(|| target_dir.join("package"));
    let secrets_env = guard::env()?;
    let inputs = layout::Inputs {
        binary: &binary,
        wasm: &wasm,
        web: &web,
        demo: &demo,
        target: &options.target,
        secrets_env: &secrets_env,
        version: package_version(options),
        checkout: &checkout,
    };
    // The binary above embeds nothing yet: the tree is written with it, the CLI is rebuilt with
    // that tree embedded (checked against the receipt `write` leaves), and the rebuilt binary
    // replaces it. Only then are the archives made.
    let mut built = layout::write(root, &out, &inputs)?;
    // `--payload-from`: the whole payload must be the source's before a binary embeds it.
    if let Some(source) = source {
        built.receipt.payload_origin = Some(source.check(&built.receipt.payload)?);
    }
    let tree = std::fs::canonicalize(&built.package_dir)
        .map_err(|e| format!("{}: {e}", built.package_dir.display()))?;
    let embedding = build_cli_embedding(root, &options.target, &target_dir, &tree, &remap)?;
    built.receipt.account = Some(audit_account(&account, &remap, &built, &embedding, &wasm)?);
    let mut built = layout::embed_binary(root, &inputs, built, &embedding)?;
    if options.archive {
        layout::archive_both(&mut built)?;
    }
    Ok(built)
}

/// [`account::audit`] over the binary that ships, the wasm core, the demo bundle and every file of
/// the static web bundle.
fn audit_account(
    account: &account::Account,
    remap: &account::Remap,
    built: &Built,
    binary: &Path,
    wasm: &Path,
) -> Result<account::Audit, String> {
    let read = |path: &Path| std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()));
    let (_, binary_name) = layout::target_shape(&built.receipt.target)?;
    let mut files = vec![
        (binary_name.to_string(), read(binary)?),
        ("payload/web/pemu_wasm.wasm".to_string(), read(wasm)?),
    ];
    let demo = built
        .package_dir
        .join("payload/firmware")
        .join(demo::BUNDLE_FILE);
    if demo.is_file() {
        files.push((
            format!("payload/firmware/{}", demo::BUNDLE_FILE),
            read(&demo)?,
        ));
    }
    let web_name = built
        .web_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut web = Vec::new();
    list_files(&built.web_dir, &mut web)?;
    web.sort();
    for path in web {
        let relative = path
            .strip_prefix(&built.web_dir)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        files.push((format!("{web_name}/{relative}"), read(&path)?));
    }
    let artifacts: Vec<account::Artifact<'_>> = files
        .iter()
        .map(|(label, bytes)| account::Artifact {
            label: label.clone(),
            bytes,
        })
        .collect();
    account::audit(account, remap, &artifacts)
}

fn list_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            list_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// `<CARGO_TARGET_DIR>` when it is set, else `<root>/target`.
fn target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => root.join("target"),
    }
}

/// The variable `crates/pemu-cli/build.rs` embeds a package tree from.
pub const EMBED_PAYLOAD_ENV: &str = "PASSPORTSIM_EMBED_PAYLOAD";

/// Runs cargo with `args` in `root`, failing with the child's stderr. [`EMBED_PAYLOAD_ENV`] is set
/// to `embed` or removed, so the caller's environment never decides what a build embeds; the
/// variables in `remove` are removed too.
fn cargo_with(
    root: &Path,
    args: &[&str],
    embed: Option<&Path>,
    remove: &[&str],
) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = std::process::Command::new(&cargo);
    match embed {
        Some(tree) => command.env(EMBED_PAYLOAD_ENV, tree),
        None => command.env_remove(EMBED_PAYLOAD_ENV),
    };
    for name in remove {
        command.env_remove(name);
    }
    let output = command
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run `{cargo} {}`: {e}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "`cargo {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn build_cli(
    root: &Path,
    target: &str,
    target_dir: &Path,
    remap: &account::Remap,
) -> Result<PathBuf, String> {
    build_cli_with(root, target, target_dir, remap, None)
}

/// [`build_cli`] again, with the absolute package `tree` embedded. [`layout::embed_binary`] copies
/// it out at once, because the next payload-less build writes to the same path.
fn build_cli_embedding(
    root: &Path,
    target: &str,
    target_dir: &Path,
    tree: &Path,
    remap: &account::Remap,
) -> Result<PathBuf, String> {
    build_cli_with(root, target, target_dir, remap, Some(tree))
}

/// The one cargo invocation both CLI builds share, so the two binaries differ only in what the
/// build script embeds. `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` are removed because either would
/// replace the `target.<triple>.rustflags` that carry [`account::Remap`] and [`windows::cargo_args`].
fn build_cli_with(
    root: &Path,
    target: &str,
    target_dir: &Path,
    remap: &account::Remap,
    embed: Option<&Path>,
) -> Result<PathBuf, String> {
    let (_, binary_name) = layout::target_shape(target)?;
    if WINDOWS_TARGETS.contains(&target) {
        let res = windows::res_file(windows::MANIFEST.as_bytes());
        let dir = target_dir.join("package-windows");
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let res_path = dir.join(windows::res_file_name(&res));
        std::fs::write(&res_path, &res).map_err(|e| format!("{}: {e}", res_path.display()))?;
        let res_arg = res_path
            .to_str()
            .ok_or_else(|| format!("{} is not UTF-8", res_path.display()))?;
        let args = windows::cargo_args(target, res_arg, &remap.rustflags())?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        cargo_with(root, &args, embed, RUSTFLAGS_VARS)?;
    } else {
        let config = account::rustflags_config(target, &remap.rustflags())?;
        cargo_with(
            root,
            &[
                "build",
                "--release",
                "--target",
                target,
                "-p",
                "pemu-cli",
                "--config",
                &config,
            ],
            embed,
            RUSTFLAGS_VARS,
        )?;
    }
    let binary = target_dir.join(target).join("release").join(binary_name);
    if binary.is_file() {
        Ok(binary)
    } else {
        Err(format!("cargo wrote no binary at {}", binary.display()))
    }
}

const RUSTFLAGS_VARS: &[&str] = &["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"];

const WASM_TARGET: &str = "wasm32-unknown-unknown";

/// The wasm core the web bundle loads. It builds into `<target dir>/package-wasm`, because its
/// rustflags carry the [`account::Remap`] and the CI tiers' do not: sharing a directory would make
/// each rebuild the other's module.
fn build_wasm(root: &Path, target_dir: &Path, remap: &account::Remap) -> Result<PathBuf, String> {
    let dir = target_dir.join("package-wasm");
    let dir_arg = dir
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", dir.display()))?;
    let config = account::rustflags_config(WASM_TARGET, &remap.rustflags())?;
    cargo_with(
        root,
        &[
            "build",
            "-p",
            "pemu-wasm",
            "--target",
            WASM_TARGET,
            "--profile",
            "wasm-release",
            "--target-dir",
            dir_arg,
            "--config",
            &config,
        ],
        None,
        RUSTFLAGS_VARS,
    )?;
    let wasm = dir
        .join(WASM_TARGET)
        .join("wasm-release")
        .join("pemu_wasm.wasm");
    if wasm.is_file() {
        Ok(wasm)
    } else {
        Err(format!("cargo wrote no wasm core at {}", wasm.display()))
    }
}

/// `bun run build` in `web/`, which writes `web/dist/` (`web/package.json`).
fn build_web(root: &Path) -> Result<PathBuf, String> {
    let web = root.join("web");
    // A checkout that never installed `web/node_modules` fails here with the fix named.
    if !web.join("node_modules").is_dir() {
        return Err(format!(
            "{} has no node_modules: run `bun install --frozen-lockfile` there first",
            web.display()
        ));
    }
    let output = std::process::Command::new("bun")
        .args(["run", "build"])
        .current_dir(&web)
        .output()
        .map_err(|e| {
            format!(
                "cannot run `bun run build` in {}: {e}. The web assets are part of every package, \
                 so bun is a build tool of `xtask package`, not of the emulator: a \
                 packaged binary needs no Bun, Node, Python or ESP-IDF at run time.",
                web.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "`bun run build` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let dist = web.join("dist");
    if dist.is_dir() {
        Ok(dist)
    } else {
        Err(format!("bun wrote no bundle at {}", dist.display()))
    }
}
