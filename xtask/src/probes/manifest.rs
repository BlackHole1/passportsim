//! `tests/fw/manifest.toml`: what `cargo xtask probes` built, and from what.
//!
//! The **stripped** ELF of every probe is committed under `tests/fw/` (each under 1 MB, with the
//! strip command as its provenance note), so a checkout with no ESP-IDF still has a runnable
//! probe. Stripping takes a probe ELF from about 3.3 MB to under 250 KB, and a radio
//! probe from about 10 MB to about 1 MB; it also drops ESP-IDF's two `NOBITS` placeholder
//! sections, which `loaded.rs` proves changes nothing loaded.
//!
//! Three kinds of hash:
//!
//!  - **source** hashes of every probe file and the two shared files, checkable with no
//!    toolchain: they expose an edited probe that was never rebuilt;
//!  - the **committed-ELF** hash of `tests/fw/<name>.elf`, also checkable with no toolchain;
//!  - **artifact** hashes (unstripped ELF, app image, bootloader, partition table, merged 8 MB
//!    image), checkable only by `cargo xtask probes --check`.
//!
//! Artifact hashes reproduce only in the recorded `build_dir`: under
//! `CONFIG_APP_REPRODUCIBLE_BUILD` the app description still carries the ELF's own SHA-256, so
//! `xtask probes` always builds in one canonical directory and records it here.

use std::fmt::Write as _;
use std::path::Path;

pub use crate::manifest_util::digest_file;
use crate::manifest_util::{array, escape, integer, string};

/// The manifest file the getters' errors name.
const FILE: &str = "manifest.toml";

/// Schema string of the rendered file, so a reader can refuse a shape it does not know.
pub const SCHEMA: &str = "passport-emu/probes-manifest/2";

/// Chip every probe targets.
pub const TARGET: &str = "esp32c3";

/// Flash size the shared defaults pin, and the size of a merged image: the board carries an 8 MB
/// XMC part.
pub const FLASH_SIZE_BYTES: u64 = 8 * 1024 * 1024;

/// Largest binary the repository accepts.
pub const MAX_COMMITTED_ELF_BYTES: u64 = 1 << 20;

/// Directory of the committed probe ELFs, relative to the repository root.
pub const COMMITTED_ELF_DIR: &str = "tests/fw";

/// Provenance note of every committed ELF: how it was made from the build's own ELF.
/// `<strip>` is the probe's own `strip` field, [`strip_mode`].
pub const STRIP_COMMAND: &str =
    "riscv32-esp-elf-strip <strip> -R .flash_rodata_dummy -R .dram0.dummy <name>.elf";

/// Probes whose committed ELF keeps its symbol table, because a consumer resolves names in it
/// with no toolchain: the hook points `hle_probe_hook_*` that HLE binds by name, and the panic
/// backtrace frames `probe_panic_read_null` and `probe_panic_outer`. Debug information is still
/// removed, which keeps both far under [`MAX_COMMITTED_ELF_BYTES`].
pub const KEEP_SYMBOLS: [&str; 2] = ["hle_probe", "probe_panic"];

/// The strip option of probe `name`: `--strip-debug` for [`KEEP_SYMBOLS`], else `--strip-all`.
pub fn strip_mode(name: &str) -> &'static str {
    if KEEP_SYMBOLS.contains(&name) {
        "--strip-debug"
    } else {
        "--strip-all"
    }
}

/// Where the committed stripped ELF of `name` lives, relative to the repository root.
pub fn stripped_elf_path(name: &str) -> String {
    format!("{COMMITTED_ELF_DIR}/{name}.elf")
}

/// One built file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    /// Lowercase hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

/// One committed source file of a probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// Path relative to the repository root.
    pub path: String,
    pub sha256: String,
}

/// One probe: where its sources are, and what a build of them produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// Directory name under `probes/`, which is also the IDF project name.
    pub name: String,
    /// Project directory relative to the repository root.
    pub project: String,
    /// Every committed file of the project, sorted by path.
    pub sources: Vec<Source>,
    /// SHA-256 of the `sdkconfig` the build generated, which fixes every option of the build.
    pub sdkconfig_sha256: String,
    /// The strip option the committed ELF was made with ([`strip_mode`]).
    pub strip: String,
    /// The unstripped ELF, which stays outside the repository.
    pub elf: Artifact,
    /// Where the stripped ELF is committed, relative to the repository root.
    pub elf_stripped_path: String,
    /// The committed stripped ELF.
    pub elf_stripped: Artifact,
    pub app: Artifact,
    pub bootloader: Artifact,
    pub partition_table: Artifact,
    /// The 8 MB flash image assembled from the three above.
    pub merged: Artifact,
}

/// Everything the manifest records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// `esp-idf/version.txt`, for example `v5.5.3`.
    pub idf_version: String,
    /// The IDF checkout's commit, or `unknown` when it is not a git checkout.
    pub idf_commit: String,
    /// First line of `riscv32-esp-elf-gcc --version`.
    pub toolchain: String,
    /// How each committed ELF was made from the build's own ELF: its provenance note.
    pub strip_command: String,
    /// Directory every build ran in, with the home directory written as `~`. Artifact hashes
    /// reproduce only there.
    pub build_dir: String,
    /// SHA-256 of `probes/common/sdkconfig.defaults`.
    pub common_sdkconfig_sha256: String,
    /// SHA-256 of `probes/common/probe_line.h`, the line-format contract.
    pub probe_line_h_sha256: String,
    pub probes: Vec<Probe>,
}

/// What [`Manifest::verify_sources`] found: how many files it hashed, and any observation that
/// is worth printing but is not a failure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verified {
    /// Files hashed against the tree.
    pub checked: usize,
    /// Observations to print on the success path. Never a reason to fail.
    pub notes: Vec<String>,
}

/// One difference between the manifest and a fresh build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference {
    /// Probe the difference belongs to, or `""` for a header field.
    pub probe: String,
    /// What differs (`elf`, `merged`, `toolchain`).
    pub what: String,
    /// What the manifest says.
    pub expected: String,
    /// What the fresh build produced.
    pub actual: String,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.probe.is_empty() {
            write!(
                f,
                "{}: manifest {}, build {}",
                self.what, self.expected, self.actual
            )
        } else {
            write!(
                f,
                "{} {}: manifest {}, build {}",
                self.probe, self.what, self.expected, self.actual
            )
        }
    }
}

impl Manifest {
    /// Renders the file, header comments included.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "# What `cargo xtask probes` built, and from what. Written by that command;\n",
        );
        out.push_str("# `cargo xtask probes --check` rebuilds every probe and compares.\n");
        out.push_str("#\n");
        out.push_str("# `source` and `elf_stripped` hashes are checkable with no toolchain. The\n");
        out.push_str("# other artifacts stay outside the repository, and their hashes reproduce\n");
        out.push_str(
            "# only in `build_dir` (`~` is the builder's home): the app image carries the\n",
        );
        out.push_str(
            "# ELF's SHA-256, and the ELF's debug information depends on the directory.\n",
        );
        let _ = writeln!(out, "\nschema = \"{SCHEMA}\"");
        let _ = writeln!(out, "target = \"{TARGET}\"");
        let _ = writeln!(out, "flash_size_bytes = {FLASH_SIZE_BYTES}");
        let _ = writeln!(out, "idf_version = \"{}\"", escape(&self.idf_version));
        let _ = writeln!(out, "idf_commit = \"{}\"", escape(&self.idf_commit));
        let _ = writeln!(out, "toolchain = \"{}\"", escape(&self.toolchain));
        let _ = writeln!(out, "strip_command = \"{}\"", escape(&self.strip_command));
        let _ = writeln!(out, "build_dir = \"{}\"", escape(&self.build_dir));
        let _ = writeln!(
            out,
            "common_sdkconfig = \"probes/common/sdkconfig.defaults\""
        );
        let _ = writeln!(
            out,
            "common_sdkconfig_sha256 = \"{}\"",
            self.common_sdkconfig_sha256
        );
        let _ = writeln!(out, "probe_line_h = \"probes/common/probe_line.h\"");
        let _ = writeln!(
            out,
            "probe_line_h_sha256 = \"{}\"",
            self.probe_line_h_sha256
        );
        let _ = writeln!(out, "elf_committed = true");
        let _ = writeln!(
            out,
            "elf_committed_note = \"the stripped ELF of each probe is committed at \
             `elf_stripped`, under the {MAX_COMMITTED_ELF_BYTES} byte limit for committed \
             binaries; the unstripped ELF and the merged image stay outside the repository\""
        );
        for probe in &self.probes {
            let _ = writeln!(out, "\n[[probe]]");
            let _ = writeln!(out, "name = \"{}\"", probe.name);
            let _ = writeln!(out, "project = \"{}\"", probe.project);
            let _ = writeln!(out, "sdkconfig_sha256 = \"{}\"", probe.sdkconfig_sha256);
            let _ = writeln!(out, "strip = \"{}\"", probe.strip);
            let _ = writeln!(out, "elf_stripped = \"{}\"", probe.elf_stripped_path);
            render_artifact(&mut out, "elf_stripped", &probe.elf_stripped);
            render_artifact(&mut out, "elf", &probe.elf);
            render_artifact(&mut out, "app", &probe.app);
            render_artifact(&mut out, "bootloader", &probe.bootloader);
            render_artifact(&mut out, "partition_table", &probe.partition_table);
            render_artifact(&mut out, "merged", &probe.merged);
            for source in &probe.sources {
                let _ = writeln!(out, "\n[[probe.source]]");
                let _ = writeln!(out, "path = \"{}\"", source.path);
                let _ = writeln!(out, "sha256 = \"{}\"", source.sha256);
            }
        }
        out
    }

    /// Parses a rendered manifest.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let table: toml::Table = text
            .parse()
            .map_err(|err| format!("manifest.toml does not parse as TOML: {err}"))?;
        let schema = string(&table, "schema", FILE)?;
        if schema != SCHEMA {
            return Err(format!(
                "manifest.toml schema is `{schema}`, not `{SCHEMA}`"
            ));
        }
        let target = string(&table, "target", FILE)?;
        if target != TARGET {
            return Err(format!(
                "manifest.toml target is `{target}`, not `{TARGET}`"
            ));
        }
        let flash = integer(&table, "flash_size_bytes", FILE)?;
        if flash != FLASH_SIZE_BYTES {
            return Err(format!(
                "manifest.toml flash_size_bytes is {flash}, not {FLASH_SIZE_BYTES}"
            ));
        }
        let mut probes = Vec::new();
        for item in array(&table, "probe", FILE)? {
            let name = string(item, "name", FILE)?;
            let mut sources = Vec::new();
            for source in array(item, "source", FILE)? {
                sources.push(Source {
                    path: string(source, "path", FILE)?,
                    sha256: hash(source, "sha256")?,
                });
            }
            if sources.is_empty() {
                return Err(format!("probe `{name}` lists no [[probe.source]]"));
            }
            let elf_stripped_path = string(item, "elf_stripped", FILE)?;
            if elf_stripped_path != stripped_elf_path(&name) {
                return Err(format!(
                    "probe `{name}` commits its ELF at `{elf_stripped_path}`, not at `{}`",
                    stripped_elf_path(&name)
                ));
            }
            let elf_stripped = artifact(item, "elf_stripped")?;
            if elf_stripped.size > MAX_COMMITTED_ELF_BYTES {
                return Err(format!(
                    "probe `{name}` commits a {} byte ELF, over the {MAX_COMMITTED_ELF_BYTES} \
                     byte limit for committed binaries",
                    elf_stripped.size
                ));
            }
            let strip = string(item, "strip", FILE)?;
            if strip != strip_mode(&name) {
                return Err(format!(
                    "probe `{name}` was stripped with `{strip}`, not `{}`",
                    strip_mode(&name)
                ));
            }
            probes.push(Probe {
                strip,
                project: string(item, "project", FILE)?,
                sdkconfig_sha256: hash(item, "sdkconfig_sha256")?,
                elf: artifact(item, "elf")?,
                elf_stripped_path,
                elf_stripped,
                app: artifact(item, "app")?,
                bootloader: artifact(item, "bootloader")?,
                partition_table: artifact(item, "partition_table")?,
                merged: artifact(item, "merged")?,
                name,
                sources,
            });
        }
        if probes.is_empty() {
            return Err("manifest.toml lists no [[probe]]".to_string());
        }
        Ok(Manifest {
            idf_version: string(&table, "idf_version", FILE)?,
            idf_commit: string(&table, "idf_commit", FILE)?,
            toolchain: string(&table, "toolchain", FILE)?,
            strip_command: string(&table, "strip_command", FILE)?,
            build_dir: string(&table, "build_dir", FILE)?,
            common_sdkconfig_sha256: hash(&table, "common_sdkconfig_sha256")?,
            probe_line_h_sha256: hash(&table, "probe_line_h_sha256")?,
            probes,
        })
    }

    /// Checks every recorded source hash, and every committed stripped ELF, against the tree at
    /// `repo_root`, with no toolchain and no build.
    ///
    /// Every mismatch is reported, not just the first, and a file inside a probe project that the
    /// manifest does not list is a failure too: the manifest is the index of what was built.
    /// Observations that are not failures go to [`Verified::notes`], never to the error, so a
    /// note cannot fail a build.
    pub fn verify_sources(&self, repo_root: &Path) -> Result<Verified, String> {
        let mut problems = Vec::new();
        let mut notes = Vec::new();
        let mut checked = 0;
        for (path, expected) in [
            (
                "probes/common/sdkconfig.defaults",
                &self.common_sdkconfig_sha256,
            ),
            ("probes/common/probe_line.h", &self.probe_line_h_sha256),
        ] {
            checked += 1;
            check_file(repo_root, path, expected, &mut problems);
        }
        for probe in &self.probes {
            for source in &probe.sources {
                checked += 1;
                check_file(repo_root, &source.path, &source.sha256, &mut problems);
            }
            match project_files(&repo_root.join(&probe.project), &probe.project) {
                Ok(found) => {
                    for path in found {
                        if !probe.sources.iter().any(|source| source.path == path) {
                            problems.push(format!(
                                "{path}: in {} but not in the manifest",
                                probe.project
                            ));
                        }
                    }
                }
                Err(err) => problems.push(err),
            }
            // The committed ELF is checked like a source: bytes and size, with no toolchain.
            checked += 1;
            match digest_file(&repo_root.join(&probe.elf_stripped_path)) {
                Ok((got, size))
                    if got == probe.elf_stripped.sha256 && size == probe.elf_stripped.size => {}
                Ok((got, size)) => problems.push(format!(
                    "{}: SHA-256 {got} ({size} bytes), manifest says {} ({} bytes)",
                    probe.elf_stripped_path, probe.elf_stripped.sha256, probe.elf_stripped.size
                )),
                Err(err) => problems.push(format!("{err} (run `cargo xtask probes`)")),
            }
            if probe.elf.size <= MAX_COMMITTED_ELF_BYTES {
                // Not a failure: the unstripped ELF now fits the limit too, so it could be
                // committed in place of the stripped one.
                notes.push(format!(
                    "{}: the unstripped ELF is {} bytes, under the {MAX_COMMITTED_ELF_BYTES} byte \
                     limit for committed binaries, so it could be committed in place of the \
                     stripped one",
                    probe.name, probe.elf.size
                ));
            }
        }
        if problems.is_empty() {
            Ok(Verified { checked, notes })
        } else {
            Err(format!(
                "{} manifest problem(s):\n  {}",
                problems.len(),
                problems.join("\n  ")
            ))
        }
    }

    /// Every difference between this manifest and one written from a fresh build.
    pub fn compare(&self, fresh: &Manifest) -> Vec<Difference> {
        let mut out = Vec::new();
        header_diff(
            &mut out,
            "idf_version",
            &self.idf_version,
            &fresh.idf_version,
        );
        header_diff(&mut out, "idf_commit", &self.idf_commit, &fresh.idf_commit);
        header_diff(&mut out, "toolchain", &self.toolchain, &fresh.toolchain);
        header_diff(
            &mut out,
            "strip_command",
            &self.strip_command,
            &fresh.strip_command,
        );
        header_diff(&mut out, "build_dir", &self.build_dir, &fresh.build_dir);
        header_diff(
            &mut out,
            "probe_line_h_sha256",
            &self.probe_line_h_sha256,
            &fresh.probe_line_h_sha256,
        );
        header_diff(
            &mut out,
            "common_sdkconfig_sha256",
            &self.common_sdkconfig_sha256,
            &fresh.common_sdkconfig_sha256,
        );
        for probe in &self.probes {
            let Some(other) = fresh.probes.iter().find(|item| item.name == probe.name) else {
                out.push(Difference {
                    probe: probe.name.clone(),
                    what: "presence".to_string(),
                    expected: "built".to_string(),
                    actual: "missing".to_string(),
                });
                continue;
            };
            probe_diff(
                &mut out,
                &probe.name,
                "sdkconfig_sha256",
                &probe.sdkconfig_sha256,
                &other.sdkconfig_sha256,
            );
            probe_diff(&mut out, &probe.name, "strip", &probe.strip, &other.strip);
            artifact_diff(&mut out, &probe.name, "elf", &probe.elf, &other.elf);
            artifact_diff(
                &mut out,
                &probe.name,
                "elf_stripped",
                &probe.elf_stripped,
                &other.elf_stripped,
            );
            artifact_diff(&mut out, &probe.name, "app", &probe.app, &other.app);
            artifact_diff(
                &mut out,
                &probe.name,
                "bootloader",
                &probe.bootloader,
                &other.bootloader,
            );
            artifact_diff(
                &mut out,
                &probe.name,
                "partition_table",
                &probe.partition_table,
                &other.partition_table,
            );
            artifact_diff(
                &mut out,
                &probe.name,
                "merged",
                &probe.merged,
                &other.merged,
            );
        }
        for probe in &fresh.probes {
            if !self.probes.iter().any(|item| item.name == probe.name) {
                out.push(Difference {
                    probe: probe.name.clone(),
                    what: "presence".to_string(),
                    expected: "missing".to_string(),
                    actual: "built".to_string(),
                });
            }
        }
        out
    }
}

fn check_file(repo_root: &Path, path: &str, expected: &str, problems: &mut Vec<String>) {
    match digest_file(&repo_root.join(path)) {
        Ok((got, _)) if got == expected => {}
        Ok((got, _)) => problems.push(format!("{path}: SHA-256 {got}, manifest says {expected}")),
        Err(err) => problems.push(err),
    }
}

/// Every committed file under a probe project, as repository-relative paths, sorted.
pub fn project_files(dir: &Path, prefix: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), prefix.to_string())];
    while let Some((path, rel)) = stack.pop() {
        let read = std::fs::read_dir(&path)
            .map_err(|err| format!("cannot list {}: {}", path.display(), err.kind()))?;
        for item in read {
            let item =
                item.map_err(|err| format!("cannot list {}: {}", path.display(), err.kind()))?;
            let name = item.file_name().to_string_lossy().to_string();
            // A stray local build directory or editor file is not a source; `build` is what
            // `idf.py` leaves behind when someone builds a probe by hand, as are `sdkconfig` and
            // `sdkconfig.old`, which `.gitignore` keeps out of the tree.
            if name.starts_with('.')
                || name == "build"
                || name == "sdkconfig"
                || name == "sdkconfig.old"
            {
                continue;
            }
            let child = format!("{rel}/{name}");
            if item.path().is_dir() {
                stack.push((item.path(), child));
            } else {
                out.push(child);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn header_diff(out: &mut Vec<Difference>, what: &str, expected: &str, actual: &str) {
    if expected != actual {
        out.push(Difference {
            probe: String::new(),
            what: what.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
}

fn probe_diff(out: &mut Vec<Difference>, probe: &str, what: &str, expected: &str, actual: &str) {
    if expected != actual {
        out.push(Difference {
            probe: probe.to_string(),
            what: what.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
}

fn artifact_diff(
    out: &mut Vec<Difference>,
    probe: &str,
    what: &str,
    expected: &Artifact,
    actual: &Artifact,
) {
    if expected != actual {
        out.push(Difference {
            probe: probe.to_string(),
            what: what.to_string(),
            expected: format!("{} ({} bytes)", expected.sha256, expected.size),
            actual: format!("{} ({} bytes)", actual.sha256, actual.size),
        });
    }
}

fn render_artifact(out: &mut String, name: &str, artifact: &Artifact) {
    let _ = writeln!(out, "{name}_sha256 = \"{}\"", artifact.sha256);
    let _ = writeln!(out, "{name}_size = {}", artifact.size);
}

fn artifact(table: &toml::Table, name: &str) -> Result<Artifact, String> {
    Ok(Artifact {
        sha256: hash(table, &format!("{name}_sha256"))?,
        size: integer(table, &format!("{name}_size"), FILE)?,
    })
}

/// A string that must be a lowercase hex SHA-256.
fn hash(table: &toml::Table, key: &str) -> Result<String, String> {
    let value = string(table, key, FILE)?;
    if value.len() != 64
        || !value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    {
        return Err(format!(
            "`{key}` of manifest.toml is not a lowercase hex SHA-256"
        ));
    }
    Ok(value)
}
