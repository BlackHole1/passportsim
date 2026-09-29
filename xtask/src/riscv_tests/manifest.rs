//! `MANIFEST.toml` of the committed riscv-tests ELFs: the provenance record, the per-file
//! SHA-256 and size, and the excluded tests with their reasons.
//!
//! The manifest is the only index of the data directory. `cargo xtask riscv-tests fetch-build`
//! writes it from a fresh build and `cargo xtask riscv-tests` verifies it, so an ELF that was
//! edited, truncated or added without a rebuild fails the check. `tests/riscv_tests.rs` reads
//! the same file with its own small reader, which is why the rendering stays one plain
//! `key = "value"` per line with no inline tables.

use std::fmt::Write as _;
use std::path::Path;

pub use crate::manifest_util::digest_file;
use crate::manifest_util::{array, escape, integer, string};
pub use pemu_loader::hex;

/// The manifest file the getters' errors name.
const FILE: &str = "MANIFEST.toml";

/// Schema string of the rendered file, so a reader can refuse a shape it does not know.
pub const SCHEMA: &str = "passport-emu/riscv-tests-manifest/1";

/// Upstream repository the ELFs are built from (BSD-3-Clause).
pub const UPSTREAM: &str = "https://github.com/riscv-software-src/riscv-tests";

/// SPDX id of the upstream license.
pub const LICENSE: &str = "BSD-3-Clause";

/// Value the `p` environment writes to `tohost` when a test passes (`env/p/riscv_test.h`
/// `RVTEST_PASS` sets `gp` to 1; `RVTEST_FAIL` sets an odd value above 1).
pub const PASS_VALUE: u32 = 1;

/// One committed ELF.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// File name in the data directory, which is also the test name (`rv32ui-p-add`).
    pub name: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
    /// Size of the file in bytes.
    pub size: u64,
}

/// One test that is built upstream but not run here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Excluded {
    pub name: String,
    /// Why it cannot apply to the C3. Required: an exclusion without a reason is a bug.
    pub reason: String,
}

/// Everything `MANIFEST.toml` records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Pinned upstream commit of `riscv-tests`.
    pub commit: String,
    /// Pinned commit of its `env` submodule (`riscv-software-src/riscv-test-env`).
    pub env_commit: String,
    /// `riscv32-esp-elf-gcc --version` first line.
    pub toolchain: String,
    /// The compile command with `<>` placeholders, one line.
    pub build: String,
    /// SHA-256 of `xtask/src/riscv_tests/c3_env_p.h`, the committed C3 shim over the upstream
    /// `p` prologue. That header states what it adjusts and why; recording its hash makes the
    /// adjustment part of the provenance, and [`Manifest::verify`] compares it with the
    /// committed file so an edited shim cannot pass unnoticed.
    pub env_shim_sha256: String,
    /// Suites built, in order (`rv32ui`, `rv32um`, `rv32uc`).
    pub suites: Vec<String>,
    pub tests: Vec<Entry>,
    pub excluded: Vec<Excluded>,
}

/// Largest ELF the manifest accepts: reviewed test ELFs are committed only under 1 MB each. The
/// built `p` tests are all well under 32 KB, so a file anywhere near the limit means the build
/// picked up the wrong environment.
pub const MAX_ELF_BYTES: u64 = 1 << 20;

impl Manifest {
    /// Renders the file, header comments included.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# Provenance of the committed riscv-tests ELFs.\n");
        out.push_str("#\n");
        out.push_str("# Written by `cargo xtask riscv-tests fetch-build`; never edited by hand.\n");
        out.push_str("# `cargo xtask riscv-tests` verifies every hash and size below and then\n");
        out.push_str("# runs `cargo test -p pemu-rv32 --test riscv_tests`.\n");
        out.push_str("#\n");
        out.push_str("# Upstream is BSD-3-Clause; the attribution lives in THIRD_PARTY.md.\n");
        out.push_str("#\n");
        out.push_str(
            "# `env_shim` is the committed C3 shim over the upstream `p` prologue; that\n",
        );
        out.push_str(
            "# header states what it adjusts (two prologue hooks: a CSR write the C3 cannot\n",
        );
        out.push_str(
            "# take, and the vectored mtvec base the C3 forces) and why. Test bodies, the link\n",
        );
        out.push_str("# script, the trap vector and the pass/fail protocol are upstream's.\n");
        let _ = writeln!(out, "\nschema = \"{SCHEMA}\"");
        let _ = writeln!(out, "upstream = \"{UPSTREAM}\"");
        let _ = writeln!(out, "commit = \"{}\"", self.commit);
        let _ = writeln!(
            out,
            "env_submodule = \"https://github.com/riscv-software-src/riscv-test-env\""
        );
        let _ = writeln!(out, "env_commit = \"{}\"", self.env_commit);
        let _ = writeln!(out, "license = \"{LICENSE}\"");
        let _ = writeln!(out, "toolchain = \"{}\"", self.toolchain);
        let _ = writeln!(out, "environment = \"p\"");
        let _ = writeln!(out, "env_shim = \"xtask/src/riscv_tests/c3_env_p.h\"");
        let _ = writeln!(out, "env_shim_sha256 = \"{}\"", self.env_shim_sha256);
        let _ = writeln!(out, "pass_value = {PASS_VALUE}");
        let _ = writeln!(out, "suites = [{}]", quoted_list(&self.suites));
        let _ = writeln!(out, "build = \"{}\"", escape(&self.build));
        for entry in &self.tests {
            let _ = writeln!(out, "\n[[test]]");
            let _ = writeln!(out, "name = \"{}\"", entry.name);
            let _ = writeln!(out, "sha256 = \"{}\"", entry.sha256);
            let _ = writeln!(out, "size = {}", entry.size);
        }
        for skip in &self.excluded {
            let _ = writeln!(out, "\n[[excluded]]");
            let _ = writeln!(out, "name = \"{}\"", skip.name);
            let _ = writeln!(out, "reason = \"{}\"", escape(&skip.reason));
        }
        out
    }

    /// Parses a rendered manifest.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let table: toml::Table = text
            .parse()
            .map_err(|err| format!("MANIFEST.toml does not parse as TOML: {err}"))?;
        let schema = string(&table, "schema", FILE)?;
        if schema != SCHEMA {
            return Err(format!(
                "MANIFEST.toml schema is `{schema}`, not `{SCHEMA}`"
            ));
        }
        let upstream = string(&table, "upstream", FILE)?;
        if upstream != UPSTREAM {
            return Err(format!("MANIFEST.toml upstream is `{upstream}`"));
        }
        let license = string(&table, "license", FILE)?;
        if license != LICENSE {
            return Err(format!("MANIFEST.toml license is `{license}`"));
        }
        let mut tests = Vec::new();
        for item in array(&table, "test", FILE)? {
            tests.push(Entry {
                name: string(item, "name", FILE)?,
                sha256: string(item, "sha256", FILE)?,
                size: integer(item, "size", FILE)?,
            });
        }
        if tests.is_empty() {
            return Err("MANIFEST.toml lists no [[test]]".to_string());
        }
        let mut excluded = Vec::new();
        for item in array(&table, "excluded", FILE).unwrap_or_default() {
            let reason = string(item, "reason", FILE)?;
            if reason.trim().is_empty() {
                return Err(format!(
                    "excluded test `{}` has an empty reason",
                    string(item, "name", FILE)?
                ));
            }
            excluded.push(Excluded {
                name: string(item, "name", FILE)?,
                reason,
            });
        }
        Ok(Manifest {
            commit: string(&table, "commit", FILE)?,
            env_commit: string(&table, "env_commit", FILE)?,
            toolchain: string(&table, "toolchain", FILE)?,
            env_shim_sha256: string(&table, "env_shim_sha256", FILE)?,
            build: string(&table, "build", FILE)?,
            suites: table
                .get("suites")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "MANIFEST.toml has no `suites` array".to_string())?
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect(),
            tests,
            excluded,
        })
    }

    /// Reads every listed file from `dir` and checks its size and SHA-256, and checks the
    /// recorded shim hash against the committed shim.
    ///
    /// Returns the number of files checked. Every mismatch is reported, not just the first,
    /// and an ELF in `dir` that the manifest does not list is a failure too: the manifest is
    /// the index the test harness walks.
    ///
    /// The shim is the only place where these ELFs leave the upstream environment, so editing
    /// `c3_env_p.h` without rebuilding must fail here; otherwise `env_shim_sha256` would be a
    /// provenance line no check ever ties to a file.
    pub fn verify(&self, dir: &Path) -> Result<usize, String> {
        let mut problems = Vec::new();
        let shim = hex(&pemu_loader::sha256(super::build::ENV_SHIM.as_bytes()));
        if shim != self.env_shim_sha256 {
            problems.push(format!(
                "xtask/src/riscv_tests/c3_env_p.h has SHA-256 {shim}, manifest says {}: \
                 the shim changed since the last fetch-build, so rerun \
                 `cargo xtask riscv-tests fetch-build`",
                self.env_shim_sha256
            ));
        }
        for entry in &self.tests {
            let path = dir.join(&entry.name);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    problems.push(format!("{}: cannot read ({})", entry.name, err.kind()));
                    continue;
                }
            };
            if bytes.len() as u64 != entry.size {
                problems.push(format!(
                    "{}: {} bytes, manifest says {}",
                    entry.name,
                    bytes.len(),
                    entry.size
                ));
                continue;
            }
            if entry.size > MAX_ELF_BYTES {
                problems.push(format!(
                    "{}: {} bytes is over the {MAX_ELF_BYTES} byte limit for committed ELFs",
                    entry.name, entry.size
                ));
            }
            let got = hex(&pemu_loader::sha256(&bytes));
            if got != entry.sha256 {
                problems.push(format!(
                    "{}: SHA-256 {got}, manifest says {}",
                    entry.name, entry.sha256
                ));
            }
        }
        for extra in unlisted(dir, &self.tests)? {
            problems.push(format!(
                "{extra}: in the data directory but not in the manifest"
            ));
        }
        if problems.is_empty() {
            Ok(self.tests.len())
        } else {
            Err(format!(
                "{} manifest problem(s):\n  {}",
                problems.len(),
                problems.join("\n  ")
            ))
        }
    }
}

/// Files of `dir` that no entry names, `MANIFEST.toml` itself excepted.
fn unlisted(dir: &Path, tests: &[Entry]) -> Result<Vec<String>, String> {
    let read = std::fs::read_dir(dir)
        .map_err(|err| format!("cannot list {}: {}", dir.display(), err.kind()))?;
    let mut extra = Vec::new();
    for item in read {
        let item = item.map_err(|err| format!("cannot list {}: {}", dir.display(), err.kind()))?;
        let name = item.file_name().to_string_lossy().to_string();
        if name == "MANIFEST.toml" || name.starts_with('.') {
            continue;
        }
        if !tests.iter().any(|entry| entry.name == name) {
            extra.push(name);
        }
    }
    extra.sort();
    Ok(extra)
}

fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("\"{item}\""))
        .collect::<Vec<_>>()
        .join(", ")
}
