//! `cargo xtask riscv-tests fetch-build`: clone the pinned upstream into the data root, build
//! the RV32 `p` tests with the ESP toolchain and refresh the committed ELFs and manifest.
//!
//! # What it does
//!
//! 1. Clones `riscv-software-src/riscv-tests` with its `env` submodule into
//!    `<data root>/riscv-tests/src` (a fetch when the clone is there) and checks out
//!    [`PINNED_COMMIT`], so the build is reproducible and the manifest records what it built.
//! 2. Reads the test names out of `isa/<suite>/Makefrag`, which is the upstream list, so a
//!    later commit that adds a test is picked up instead of silently skipped.
//! 3. Compiles each one with [`GCC_ARGS`], the upstream `isa/Makefile` recipe plus the C3 arch
//!    and ABI, into `<data root>/riscv-tests/build`.
//! 4. Copies the ELFs into the committed data directory and writes `MANIFEST.toml`.
//!
//! Nothing in the data root is committed; only step 4's output is. The clone stays so a rebuild
//! needs no network.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::manifest::{Entry, Excluded, MAX_ELF_BYTES, Manifest, digest_file, hex};

/// The C3 shim over the upstream `p` prologue, committed next to this module. It states in its
/// own header what it adjusts and why; `MANIFEST.toml` records its SHA-256.
pub(super) const ENV_SHIM: &str = include_str!("c3_env_p.h");

/// Placeholder in [`ENV_SHIM`] that becomes the absolute path of the upstream header.
const SHIM_PLACEHOLDER: &str = "@UPSTREAM_RISCV_TEST_H@";

/// Upstream commit the committed ELFs are built from (`master` when pinned, verified with
/// `git ls-remote`). Bumping it means a rebuild and a new manifest.
pub const PINNED_COMMIT: &str = "2ebecad997fa58cd9e5724340ba75aa4b59bd1d0";

/// Suites built, in manifest order, in the `p` environment: physical single core,
/// machine mode, no virtual memory (`env/p/riscv_test.h`).
pub const SUITES: [&str; 3] = ["rv32ui", "rv32um", "rv32uc"];

/// The ESP toolchain, below `$HOME`.
const GCC: &str =
    ".espressif/tools/riscv32-esp-elf/esp-14.2.0_20251107/riscv32-esp-elf/bin/riscv32-esp-elf-gcc";

/// Compiler arguments: the C3 arch and ABI, then the upstream `RISCV_GCC_OPTS` of
/// `isa/Makefile`. `-nostdlib -nostartfiles` is why the linker script alone places the image.
const GCC_ARGS: [&str; 7] = [
    "-march=rv32imc_zicsr_zifencei",
    "-mabi=ilp32",
    "-static",
    "-mcmodel=medany",
    "-fvisibility=hidden",
    "-nostdlib",
    "-nostartfiles",
];

/// Tests excluded from the committed set, with the reason the manifest records.
///
/// Empty: every `rv32ui`, `rv32um` and `rv32uc` `p` test applies to the C3. The C3 takes no
/// misaligned-access exception (mcause 4 and 6 never occur), so
/// `rv32ui-p-ma_data` runs as written, and `fence.i` is implemented (Zifencei), so
/// `rv32ui-p-fence_i` runs too.
const EXCLUDED: [Excluded2; 0] = [];

/// A compile-time exclusion row. Mirrors [`Excluded`] with `&'static str` fields.
struct Excluded2 {
    #[allow(dead_code)]
    name: &'static str,
    #[allow(dead_code)]
    reason: &'static str,
}

/// Options of `fetch-build`.
pub struct Options {
    /// Upstream commit to build; [`PINNED_COMMIT`] unless overridden.
    pub commit: String,
    /// Skip the network: use the clone as it is (it must already be at `commit`).
    pub offline: bool,
}

/// Runs the whole pipeline and returns the report to print.
pub fn fetch_build(data_dir: &Path, opts: &Options) -> Result<String, String> {
    let home = crate::hostdirs::home()?;
    let gcc = home.join(GCC);
    if !gcc.exists() {
        return Err(format!(
            "the ESP toolchain is not installed at ~/{GCC} (wave 1 brief)"
        ));
    }
    let root = super::work_root()?;
    let src = root.join("src");
    if !opts.offline {
        clone_or_fetch(&src, &opts.commit)?;
    } else if !src.join("isa").is_dir() {
        return Err(format!(
            "--offline needs an existing clone at {}",
            src.display()
        ));
    }
    let head = git(&src, &["rev-parse", "HEAD"])?;
    if head != opts.commit {
        return Err(format!(
            "{} is at {head}, not the requested {}",
            src.display(),
            opts.commit
        ));
    }
    let env_commit = submodule_commit(&src)?;
    let toolchain = first_line(&run(&gcc, &["--version"], &src)?);

    let build = root.join("build");
    std::fs::create_dir_all(&build)
        .map_err(|err| format!("cannot create {}: {}", build.display(), err.kind()))?;
    let shim = write_shim(&root, &src)?;
    let mut built = Vec::new();
    for suite in SUITES {
        for name in test_names(&src, suite)? {
            let target = format!("{suite}-p-{name}");
            compile(&gcc, &src, &build, &shim, suite, &name, &target)?;
            built.push(target);
        }
    }
    let report = install(data_dir, &build, &built)?;

    let manifest = Manifest {
        commit: opts.commit.clone(),
        env_commit,
        toolchain,
        build: build_command(),
        env_shim_sha256: hex(&pemu_loader::sha256(ENV_SHIM.as_bytes())),
        suites: SUITES.iter().map(|s| s.to_string()).collect(),
        tests: report.entries,
        excluded: EXCLUDED
            .iter()
            .map(|row| Excluded {
                name: row.name.to_string(),
                reason: row.reason.to_string(),
            })
            .collect(),
    };
    let path = data_dir.join("MANIFEST.toml");
    std::fs::write(&path, manifest.render())
        .map_err(|err| format!("cannot write {}: {}", path.display(), err.kind()))?;
    Ok(format!(
        "riscv-tests fetch-build: {} ELF(s) from {} at {}\n  \
         largest {} bytes, total {} bytes, {} removed\n  \
         manifest: {}",
        manifest.tests.len(),
        super::manifest::UPSTREAM,
        &opts.commit[..12],
        report.largest,
        report.total,
        report.removed,
        path.display()
    ))
}

/// The build recipe the manifest records, with `<>` placeholders for the paths. Two steps, so
/// that no temporary object name reaches the output (see [`compile`]); rerunning both lines at
/// the recorded commit reproduces the recorded SHA-256 byte for byte.
fn build_command() -> String {
    let args = GCC_ARGS.join(" ");
    format!(
        "riscv32-esp-elf-gcc {args} -I<c3 env shim> -I<env/p> -I<isa/macros/scalar> \
         -c <isa/SUITE/NAME.S> -o SUITE-p-NAME.o \
         && riscv32-esp-elf-gcc {args} -T<env/p/link.ld> SUITE-p-NAME.o -o SUITE-p-NAME"
    )
}

/// What [`install`] did.
struct Installed {
    entries: Vec<Entry>,
    largest: u64,
    total: u64,
    removed: usize,
}

/// Copies the built ELFs into the committed data directory, dropping files a previous build left
/// behind, and digests each one.
fn install(data_dir: &Path, build: &Path, built: &[String]) -> Result<Installed, String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|err| format!("cannot create {}: {}", data_dir.display(), err.kind()))?;
    let mut removed = 0;
    if let Ok(read) = std::fs::read_dir(data_dir) {
        for item in read.flatten() {
            let name = item.file_name().to_string_lossy().to_string();
            if name == "MANIFEST.toml" || name.starts_with('.') {
                continue;
            }
            if !built.contains(&name) {
                std::fs::remove_file(item.path())
                    .map_err(|err| format!("cannot remove {name}: {}", err.kind()))?;
                removed += 1;
            }
        }
    }
    let mut entries = Vec::new();
    let mut largest = 0;
    let mut total = 0;
    for name in built {
        let from = build.join(name);
        let to = data_dir.join(name);
        std::fs::copy(&from, &to)
            .map_err(|err| format!("cannot copy {name} into the data directory: {}", err.kind()))?;
        let (sha256, size) = digest_file(&to)?;
        if size > MAX_ELF_BYTES {
            return Err(format!(
                "{name} is {size} bytes; committed test ELFs must be under {MAX_ELF_BYTES}"
            ));
        }
        largest = largest.max(size);
        total += size;
        entries.push(Entry {
            name: name.clone(),
            sha256,
            size,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Installed {
        entries,
        largest,
        total,
        removed,
    })
}

/// Compiles one test, the upstream `isa/Makefile` recipe with [`GCC_ARGS`], in two steps.
///
/// Assemble to `<build>/<target>.o` first, then link that object. A single `gcc a.S -o elf`
/// invocation assembles into a randomly named temporary object (`ccXXXXXX.o`) and the linker
/// copies that name into the `STT_FILE` symbol of the output, so the same sources and the same
/// toolchain produce a different ELF on every run and no manifest hash can ever be recomputed.
/// Naming the object after the target removes the only non-deterministic input, and a rebuild at
/// the same commit reproduces the committed bytes exactly.
fn compile(
    gcc: &Path,
    src: &Path,
    build: &Path,
    shim: &Path,
    suite: &str,
    name: &str,
    target: &str,
) -> Result<(), String> {
    let isa = src.join("isa");
    let obj = build.join(format!("{target}.o"));
    let mut args: Vec<String> = GCC_ARGS.iter().map(|arg| arg.to_string()).collect();
    // The shim directory comes first, so its `riscv_test.h` wins and includes upstream's.
    args.push(format!("-I{}", shim.display()));
    args.push(format!("-I{}", src.join("env/p").display()));
    args.push(format!("-I{}", isa.join("macros/scalar").display()));
    args.push("-c".to_string());
    args.push(
        isa.join(suite)
            .join(format!("{name}.S"))
            .display()
            .to_string(),
    );
    args.push("-o".to_string());
    args.push(obj.display().to_string());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run(gcc, &refs, src)?;

    let mut args: Vec<String> = GCC_ARGS.iter().map(|arg| arg.to_string()).collect();
    args.push(format!("-T{}", src.join("env/p/link.ld").display()));
    args.push(obj.display().to_string());
    args.push("-o".to_string());
    args.push(build.join(target).display().to_string());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run(gcc, &refs, src).map(|_| ())
}

/// Test names of `isa/<suite>/Makefrag`: the `<suite>_sc_tests` list, one or more words per
/// continued line. Reading the upstream list keeps the built set honest across a commit bump.
fn test_names(src: &Path, suite: &str) -> Result<Vec<String>, String> {
    let path = src.join("isa").join(suite).join("Makefrag");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {}", path.display(), err.kind()))?;
    let start = format!("{suite}_sc_tests");
    let mut names = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if !inside {
            let Some(rest) = line.strip_prefix(&start) else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            inside = true;
            if !push_words(&mut names, rest) {
                break;
            }
            continue;
        }
        if !push_words(&mut names, line) {
            break;
        }
    }
    if names.is_empty() {
        return Err(format!("{} lists no {start}", path.display()));
    }
    Ok(names)
}

/// Adds the words of one `Makefrag` line; `false` when the list ended (no trailing backslash).
fn push_words(names: &mut Vec<String>, line: &str) -> bool {
    let body = line.trim();
    let (body, more) = match body.strip_suffix('\\') {
        Some(body) => (body, true),
        None => (body, false),
    };
    for word in body.split_whitespace() {
        names.push(word.to_string());
    }
    more
}

/// Clones the repository with its submodules, or fetches into an existing clone, then checks out
/// `commit` and updates the submodules to what that commit pins.
fn clone_or_fetch(src: &Path, commit: &str) -> Result<(), String> {
    if src.join(".git").exists() {
        git(src, &["fetch", "--quiet", "origin"])?;
    } else {
        if let Some(parent) = src.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {}", parent.display(), err.kind()))?;
        }
        let target = src.display().to_string();
        run(
            Path::new("git"),
            &[
                "clone",
                "--quiet",
                "--recurse-submodules",
                super::manifest::UPSTREAM,
                &target,
            ],
            Path::new("."),
        )?;
    }
    git(src, &["checkout", "--quiet", commit])?;
    git(src, &["submodule", "update", "--init", "--quiet"])?;
    Ok(())
}

/// Commit of the `env` submodule, from `git submodule status`.
fn submodule_commit(src: &Path) -> Result<String, String> {
    let text = git(src, &["submodule", "status", "env"])?;
    text.split_whitespace()
        .next()
        .map(|word| word.trim_start_matches(['+', '-', 'U']).to_string())
        .filter(|word| word.len() == 40)
        .ok_or_else(|| "cannot read the commit of the `env` submodule".to_string())
}

/// `git -C <dir> <args>`, trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    crate::util::git_text(dir, args)
}

/// Runs a program in `dir` and returns its trimmed stdout, or its stderr as the error.
fn run(program: &Path, args: &[&str], dir: &Path) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|err| format!("cannot run {}: {}", program.display(), err.kind()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "{} {} failed: {}",
            program.display(),
            args.join(" "),
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// First line of a multi-line output.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// Where the ELFs are copied to, relative to the repository root.
pub const DATA_DIR: &str = "crates/pemu-rv32/tests/data/riscv-tests";

/// Absolute data directory of this checkout.
pub fn data_dir() -> PathBuf {
    crate::util::workspace_root().join(DATA_DIR)
}

/// Writes [`ENV_SHIM`] into `<root>/env-shim/riscv_test.h` with the upstream path substituted,
/// and returns that directory so [`compile`] can put it first on the include path.
fn write_shim(root: &Path, src: &Path) -> Result<PathBuf, String> {
    let upstream = src.join("env/p/riscv_test.h");
    if !upstream.is_file() {
        return Err(format!("{} is missing", upstream.display()));
    }
    let dir = root.join("env-shim");
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("cannot create {}: {}", dir.display(), err.kind()))?;
    let text = ENV_SHIM.replace(SHIM_PLACEHOLDER, &upstream.display().to_string());
    if text == ENV_SHIM {
        return Err(format!("c3_env_p.h no longer contains {SHIM_PLACEHOLDER}"));
    }
    let path = dir.join("riscv_test.h");
    std::fs::write(&path, text)
        .map_err(|err| format!("cannot write {}: {}", path.display(), err.kind()))?;
    Ok(dir)
}
