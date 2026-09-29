//! `cargo xtask probes`: build the probe firmware and keep `tests/fw/manifest.toml` honest.
//!
//! ```text
//! cargo xtask probes                 build every probe and write the manifest
//! cargo xtask probes --check         rebuild every probe and compare it with the manifest
//! cargo xtask probes verify          check the manifest's source hashes, with no toolchain
//! cargo xtask probes list            list the probes in the tree
//! cargo xtask probes read <file>     read a console capture as a probe run
//! cargo xtask probes compare <device> <emulator>
//!                                    a device capture of a campaign probe against the emulator's
//!                                    record, row by row (specs/notes/silicon-campaign.md)
//!
//! --idf <dir>                        use this ESP-IDF instead of IDF_PATH
//! ```
//!
//! **Builds are macOS only.** They need the local ESP-IDF, which only the macOS build host has, so
//! `probes` and `probes --check` refuse on any other host; `verify`, `list` and `read` need no
//! toolchain and run anywhere. No tier builds firmware.
//!
//! **The device is never touched.** Only `idf.py build` is ever run: no `flash`, no `monitor`, no
//! `esptool`, no serial port. The merged 8 MB image is assembled in `build.rs` from the build's
//! own offset list.

mod build;
mod compare;
pub mod line;
mod loaded;
mod manifest;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use manifest::Manifest;

const USAGE: &str = concat!(
    "usage: cargo xtask probes                 build every probe and write the manifest\n",
    "       cargo xtask probes --check         rebuild and compare with the manifest\n",
    "       cargo xtask probes verify          check the manifest's source hashes only\n",
    "       cargo xtask probes list            list the probes in the tree\n",
    "       cargo xtask probes read <file>     read a console capture as a probe run\n",
    "       cargo xtask probes compare <device-capture> <emulator-record>\n",
    "                                          compare a campaign probe's device capture with the\n",
    "                                          emulator's record, row by row\n",
    "       --idf <dir>                        this ESP-IDF instead of IDF_PATH"
);

/// Path of the manifest, relative to the repository root.
const MANIFEST: &str = "tests/fw/manifest.toml";

/// Entry point of `cargo xtask probes`.
pub fn run(args: &[String]) -> Result<(), String> {
    if let Some(first) = args.first()
        && matches!(first.as_str(), "-h" | "--help" | "help")
    {
        println!("{USAGE}");
        return Ok(());
    }
    let (idf, rest) = split_idf(args)?;
    let idf = idf.as_deref();
    let root = crate::util::workspace_root();
    match rest.split_first() {
        None => {
            println!("{}", build_and_write(&root, idf)?);
            Ok(())
        }
        Some((first, [])) => match first.as_str() {
            "--check" => {
                println!("{}", check(&root, idf)?);
                Ok(())
            }
            "verify" => {
                println!("{}", verify(&root)?);
                Ok(())
            }
            "list" => {
                for name in build::probe_names(&root)? {
                    println!("{name}");
                }
                Ok(())
            }
            other => Err(format!("unknown argument `{other}`\n{USAGE}")),
        },
        Some((first, tail)) if first == "read" && tail.len() == 1 => {
            println!("{}", read_capture(Path::new(&tail[0]))?);
            Ok(())
        }
        Some((first, tail)) if first == "compare" && tail.len() == 2 => {
            let read = |path: &str| {
                std::fs::read(path)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .map_err(|err| format!("cannot read {path}: {}", err.kind()))
            };
            let report = compare::compare(&read(&tail[0])?, &read(&tail[1])?)?;
            println!("{}", report.render());
            Ok(())
        }
        Some((first, _)) => Err(format!("unknown argument `{first}`\n{USAGE}")),
    }
}

/// Takes `--idf <dir>` out of the arguments, wherever it appears.
fn split_idf(args: &[String]) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut idf = None;
    let mut rest = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--idf" => {
                let dir = iter
                    .next()
                    .ok_or_else(|| format!("--idf needs a directory\n{USAGE}"))?;
                idf = Some(PathBuf::from(dir));
            }
            other => match other.strip_prefix("--idf=") {
                Some(dir) => idf = Some(PathBuf::from(dir)),
                None => rest.push(other.to_string()),
            },
        }
    }
    Ok((idf, rest))
}

/// Refuses a build on a host that has no ESP-IDF by design.
///
/// The firmware toolchain is installed on the macOS build host only. The refusal comes before
/// anything a build would do, so a Windows checkout sees it rather than a confusing "IDF_PATH is
/// not set"; it is not applied to `verify`, `list` or `read`, which never build.
fn refuse_off_macos() -> Result<(), String> {
    if std::env::consts::OS != "macos" {
        return Err("probe builds are macOS-only".to_string());
    }
    Ok(())
}

/// Reads a probe console capture and reports the run it holds.
///
/// This is how a QEMU oracle console capture is turned into a pass or fail without a golden file:
/// the probe itself states what it checked, on its `FAIL` lines and its `DONE` status.
fn read_capture(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read {}: {}", path.display(), err.kind()))?;
    let run = line::Run::read(&text)?;
    let truncated = if run.truncated == 0 {
        String::new()
    } else {
        format!("; {} line(s) cut short by a reset", run.truncated)
    };
    if run.passed() {
        return Ok(format!(
            "probes: {} passed with {} fact line(s) over stage(s) [{}]{truncated}",
            run.name,
            run.facts,
            run.stages.join(", ")
        ));
    }
    Err(format!(
        "probe {} did not pass: status `{}`, {} failure(s){truncated}{}",
        run.name,
        run.status,
        run.failures.len(),
        if run.failures.is_empty() {
            String::new()
        } else {
            format!(":\n  {}", run.failures.join("\n  "))
        }
    ))
}

/// Builds every probe, commits the stripped ELFs and writes the manifest.
fn build_and_write(root: &Path, idf: Option<&Path>) -> Result<String, String> {
    refuse_off_macos()?;
    let env = build::discover(idf)?;
    let (build_root, image_root) = output_dirs()?;
    let fresh = build::build_all(&env, root, &build_root, &image_root)?;
    for probe in &fresh.probes {
        let from = build::stripped_build_path(&build_root, &probe.name);
        let to = root.join(&probe.elf_stripped_path);
        write_committed_elf(&from, &to)?;
    }
    let path = root.join(MANIFEST);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {}", parent.display(), err.kind()))?;
    }
    std::fs::write(&path, fresh.render())
        .map_err(|err| format!("cannot write {}: {}", path.display(), err.kind()))?;
    Ok(format!(
        "probes: built {} probe(s) with ESP-IDF {}, committed their stripped ELFs under {}, \
         wrote {MANIFEST}; merged images under {}",
        fresh.probes.len(),
        fresh.idf_version,
        manifest::COMMITTED_ELF_DIR,
        image_root.display()
    ))
}

/// Copies one stripped ELF into the repository.
fn write_committed_elf(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {}", parent.display(), err.kind()))?;
    }
    std::fs::copy(from, to).map_err(|err| {
        format!(
            "cannot copy {} to {}: {}",
            from.display(),
            to.display(),
            err.kind()
        )
    })?;
    Ok(())
}

/// Rebuilds every probe and compares the result with the committed manifest.
fn check(root: &Path, idf: Option<&Path>) -> Result<String, String> {
    refuse_off_macos()?;
    let committed = read_manifest(root)?;
    let verified = committed.verify_sources(root)?;
    let env = build::discover(idf)?;
    let (build_root, image_root) = output_dirs()?;
    let fresh = build::build_all(&env, root, &build_root, &image_root)?;
    let differences = committed.compare(&fresh);
    if differences.is_empty() {
        return Ok(format!(
            "probes: {} probe(s) rebuilt and identical to {MANIFEST} ({} file(s) checked){}",
            fresh.probes.len(),
            verified.checked,
            render_notes(&verified.notes)
        ));
    }
    Err(format!(
        "{} difference(s) between {MANIFEST} and a fresh build:\n  {}",
        differences.len(),
        differences
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ")
    ))
}

/// Checks the manifest's source and committed-ELF hashes, which needs no toolchain and no build.
fn verify(root: &Path) -> Result<String, String> {
    let committed = read_manifest(root)?;
    let verified = committed.verify_sources(root)?;
    Ok(format!(
        "probes: {MANIFEST} lists {} probe(s); {} file(s) verified against the tree{}",
        committed.probes.len(),
        verified.checked,
        render_notes(&verified.notes)
    ))
}

/// Notes of a successful verification, printed with the result rather than raised as an error.
fn render_notes(notes: &[String]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    format!("\n  note: {}", notes.join("\n  note: "))
}

fn read_manifest(root: &Path) -> Result<Manifest, String> {
    let path = root.join(MANIFEST);
    let text = std::fs::read_to_string(&path).map_err(|err| {
        format!(
            "cannot read {}: {} (run `cargo xtask probes`)",
            path.display(),
            err.kind()
        )
    })?;
    Manifest::parse(&text)
}

/// Where builds and merged images go: `CORPUS/probes/build` and `CORPUS/probes` of the data root,
/// both outside the repository.
///
/// The build directory is canonical, not a temporary one, because the app image carries the
/// ELF's own SHA-256 and the ELF's debug information is prefix-mapped against the build
/// directory: a build somewhere else produces different hashes, which would make `--check`
/// useless.
fn output_dirs() -> Result<(PathBuf, PathBuf), String> {
    let home = crate::hostdirs::home()?;
    let config = match std::fs::read_to_string(home.join(crate::hostdirs::CONFIG_FILE)) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(format!(
                "cannot read ~/{}: {}",
                crate::hostdirs::CONFIG_FILE,
                err.kind()
            ));
        }
    };
    let images = crate::hostdirs::data_root(&home, config.as_deref())?.join("corpus/probes");
    let builds = images.join("build");
    Ok((builds, images))
}
