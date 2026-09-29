//! Building the probe firmware with the local ESP-IDF.
//!
//! The IDF is located only through `IDF_PATH` and `IDF_TOOLS_PATH` or an explicit `--idf <dir>`,
//! never through a hard-coded version. The environment a build needs is the one
//! `$IDF_PATH/tools/idf_tools.py export` prints, which is the supported way to get it without
//! sourcing `export.sh`; the only thing that has to be found first is the IDF's own Python.
//!
//! The compiler binaries are then **spawned by absolute path**: the
//! directories of the exported `PATH` are searched for `riscv32-esp-elf-gcc` with this host's
//! executable suffix and the file that is found is the one run, so the recorded `toolchain` line
//! of the manifest cannot describe some other gcc that happened to be first on the inherited
//! `PATH`.
//!
//! Nothing here opens a serial port. `idf.py build` is the only target ever invoked: no `flash`,
//! no `monitor`, no `esptool`. The merged 8 MB image is assembled here from the build's own
//! `flash_args` file rather than by calling `esptool merge_bin`, so the device-facing tool is
//! never run at all.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::manifest::{Artifact, FLASH_SIZE_BYTES, Manifest, Probe, Source, digest_file};

/// Byte an erased flash cell reads as, and therefore the pad of a merged image.
const ERASED: u8 = 0xff;

/// Stem of the C compiler of the RISC-V toolchain. The host's executable suffix is appended,
/// so this is `riscv32-esp-elf-gcc` on macOS and `riscv32-esp-elf-gcc.exe` on Windows.
const GCC_STEM: &str = "riscv32-esp-elf-gcc";

/// Stem of the toolchain's `strip`, which makes the committed ELF.
const STRIP_STEM: &str = "riscv32-esp-elf-strip";

/// The located ESP-IDF and the environment a build of it needs.
pub struct Env {
    /// `IDF_PATH`.
    pub idf_path: PathBuf,
    /// The IDF's Python interpreter.
    pub python: PathBuf,
    /// Variables to set for every child process, `PATH` included.
    pub vars: BTreeMap<String, OsString>,
    /// `version.txt` of the checkout.
    pub idf_version: String,
    /// The checkout's commit, or `unknown`.
    pub idf_commit: String,
    /// First line of `riscv32-esp-elf-gcc --version`.
    pub toolchain: String,
    /// Absolute path of `riscv32-esp-elf-strip`, found beside the compiler in the exported
    /// `PATH`.
    pub strip: PathBuf,
}

/// Finds ESP-IDF and works out the build environment.
///
/// `idf_override` is `--idf <dir>`; without it `IDF_PATH` is used.
pub fn discover(idf_override: Option<&Path>) -> Result<Env, String> {
    let idf_path = match idf_override {
        Some(dir) => dir.to_path_buf(),
        None => PathBuf::from(
            std::env::var_os("IDF_PATH")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "IDF_PATH is not set; probe builds locate ESP-IDF only through IDF_PATH and \
                     IDF_TOOLS_PATH, or an explicit `--idf <dir>`"
                        .to_string()
                })?,
        ),
    };
    if !idf_path.join("tools/idf.py").is_file() {
        return Err(format!(
            "{} does not look like an ESP-IDF checkout (no tools/idf.py)",
            idf_path.display()
        ));
    }
    let idf_version = idf_version(&idf_path)?;
    let tools_path = tools_path()?;
    let python = find_python(&tools_path, &idf_version)?;

    let mut vars = BTreeMap::new();
    vars.insert("IDF_PATH".to_string(), idf_path.clone().into_os_string());
    vars.insert(
        "IDF_TOOLS_PATH".to_string(),
        tools_path.clone().into_os_string(),
    );
    for (key, value) in export_vars(&python, &idf_path, &tools_path)? {
        vars.insert(key, value);
    }

    let gcc = toolchain_binary(&vars, GCC_STEM)?;
    let strip = toolchain_binary(&vars, STRIP_STEM)?;
    let toolchain = first_line(&run(
        Command::new(&gcc).arg("--version"),
        &vars,
        "riscv32-esp-elf-gcc --version",
    )?);
    let idf_commit = match run(
        Command::new("git")
            .arg("-C")
            .arg(&idf_path)
            .args(["rev-parse", "HEAD"]),
        &vars,
        "git rev-parse HEAD",
    ) {
        Ok(out) => first_line(&out),
        Err(_) => "unknown".to_string(),
    };

    Ok(Env {
        idf_path,
        python,
        vars,
        idf_version,
        idf_commit,
        toolchain,
        strip,
    })
}

/// The absolute path of one toolchain binary, searched for in the `PATH` that
/// `idf_tools.py export` produced.
///
/// The compiler binaries are named with the host's executable suffix and **spawned by absolute
/// path**. Letting the operating system search `PATH` would resolve whichever
/// `riscv32-esp-elf-gcc` came first in the inherited environment, and the manifest's `toolchain`
/// line, which the whole reproducibility argument rests on, would then describe a compiler the
/// build may not have used.
pub(super) fn toolchain_binary(
    vars: &BTreeMap<String, OsString>,
    stem: &str,
) -> Result<PathBuf, String> {
    let name = format!("{stem}{}", std::env::consts::EXE_SUFFIX);
    let path = vars
        .get("PATH")
        .ok_or_else(|| "idf_tools.py export printed no PATH".to_string())?;
    let mut searched = Vec::new();
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return Ok(candidate);
        }
        searched.push(dir);
    }
    let shown: Vec<String> = searched
        .iter()
        .take(8)
        .map(|dir| dir.display().to_string())
        .collect();
    Err(format!(
        "{name} is in none of the {} directories `idf_tools.py export` named; run the ESP-IDF \
         install script for the riscv32-esp-elf toolchain. Searched (first {}): {}",
        searched.len(),
        shown.len(),
        shown.join(", ")
    ))
}

/// The IDF's version string.
///
/// A release tarball carries `version.txt`; a git checkout of a release tag, which is what the
/// install script produces, does not and is described by its tag instead. Both forms give the
/// same `v5.5.3`, and `idf.py --version` uses the same two sources in the same order.
fn idf_version(idf_path: &Path) -> Result<String, String> {
    if let Ok(text) = std::fs::read_to_string(idf_path.join("version.txt")) {
        let version = text.trim().to_string();
        if !version.is_empty() {
            return Ok(version);
        }
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(idf_path)
        .args(["describe", "--tags", "--dirty"])
        .output()
        .map_err(|err| {
            format!(
                "cannot run git describe on the IDF checkout: {}",
                err.kind()
            )
        })?;
    let version = first_line(&String::from_utf8_lossy(&output.stdout));
    if !output.status.success() || version.is_empty() {
        return Err(format!(
            "cannot determine the ESP-IDF version of {}: it has no version.txt and git describe \
             said nothing",
            idf_path.display()
        ));
    }
    Ok(version)
}

/// `IDF_TOOLS_PATH`, or `~/.espressif` which is where the IDF installer puts it.
fn tools_path() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("IDF_TOOLS_PATH").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    Ok(crate::hostdirs::home()?.join(".espressif"))
}

/// The IDF's own Python: `IDF_PYTHON_ENV_PATH` when it is set, otherwise the virtual environment
/// under `IDF_TOOLS_PATH/python_env` whose name carries this IDF's major and minor version
/// (`idf5.5_py3.10_env`).
fn find_python(tools_path: &Path, idf_version: &str) -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("IDF_PYTHON_ENV_PATH").filter(|value| !value.is_empty()) {
        let python = PathBuf::from(value).join("bin/python");
        if python.is_file() {
            return Ok(python);
        }
        return Err(format!(
            "IDF_PYTHON_ENV_PATH is set but {} is not a file",
            python.display()
        ));
    }
    let short = short_version(idf_version);
    let envs = tools_path.join("python_env");
    let read = std::fs::read_dir(&envs)
        .map_err(|err| format!("cannot list {}: {}", envs.display(), err.kind()))?;
    let mut candidates = Vec::new();
    for item in read {
        let item = item.map_err(|err| format!("cannot list {}: {}", envs.display(), err.kind()))?;
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with(&format!("idf{short}_")) && item.path().join("bin/python").is_file() {
            candidates.push(item.path().join("bin/python"));
        }
    }
    candidates.sort();
    candidates.pop().ok_or_else(|| {
        format!(
            "no ESP-IDF Python environment for {idf_version} under {}; run the IDF install script \
             or set IDF_PYTHON_ENV_PATH",
            envs.display()
        )
    })
}

/// `v5.5.3` to `5.5`.
fn short_version(version: &str) -> String {
    let trimmed = version.trim_start_matches('v');
    let mut parts = trimmed.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => format!("{major}.{minor}"),
        _ => trimmed.to_string(),
    }
}

/// The variables `idf_tools.py export` prints, with `$PATH` in its `PATH` line expanded against
/// this process's `PATH`.
fn export_vars(
    python: &Path,
    idf_path: &Path,
    tools_path: &Path,
) -> Result<Vec<(String, OsString)>, String> {
    let mut command = Command::new(python);
    command
        .arg(idf_path.join("tools/idf_tools.py"))
        .args(["export", "--format", "key-value"])
        .env("IDF_PATH", idf_path)
        .env("IDF_TOOLS_PATH", tools_path);
    let output = command
        .output()
        .map_err(|err| format!("cannot run idf_tools.py export: {}", err.kind()))?;
    if !output.status.success() {
        return Err(format!(
            "idf_tools.py export failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let current = std::env::var("PATH").unwrap_or_default();
    let mut vars = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        // `IDF_DEACTIVATE_FILE_PATH` names a temporary file that only a shell that sourced
        // `export.sh` would use; passing it on would leave a stale path in the environment.
        if key == "IDF_DEACTIVATE_FILE_PATH" {
            continue;
        }
        let value = value.replace("$PATH", &current);
        vars.push((key.to_string(), OsString::from(value)));
    }
    if !vars.iter().any(|(key, _)| key == "PATH") {
        return Err("idf_tools.py export printed no PATH".to_string());
    }
    Ok(vars)
}

/// Names of the probes in `probes/`, sorted: every directory with a `CMakeLists.txt`.
pub fn probe_names(repo_root: &Path) -> Result<Vec<String>, String> {
    let dir = repo_root.join("probes");
    let read = std::fs::read_dir(&dir)
        .map_err(|err| format!("cannot list {}: {}", dir.display(), err.kind()))?;
    let mut names = Vec::new();
    for item in read {
        let item = item.map_err(|err| format!("cannot list {}: {}", dir.display(), err.kind()))?;
        if item.path().join("CMakeLists.txt").is_file() {
            names.push(item.file_name().to_string_lossy().to_string());
        }
    }
    names.sort();
    if names.is_empty() {
        return Err(format!("{} holds no probe projects", dir.display()));
    }
    Ok(names)
}

/// Builds every probe and returns the manifest that describes the result.
///
/// `build_root` is the canonical build directory recorded in the manifest; `image_root` is where
/// the merged 8 MB images are written. Both live outside the repository, under the data root.
pub fn build_all(
    env: &Env,
    repo_root: &Path,
    build_root: &Path,
    image_root: &Path,
) -> Result<Manifest, String> {
    let _lock = BuildLock::acquire(build_root)?;
    let mut probes = Vec::new();
    for name in probe_names(repo_root)? {
        println!("probes: building {name}");
        probes.push(build_one(env, repo_root, &name, build_root, image_root)?);
    }
    let (common_sdkconfig_sha256, _) =
        digest_file(&repo_root.join("probes/common/sdkconfig.defaults"))?;
    let (probe_line_h_sha256, _) = digest_file(&repo_root.join("probes/common/probe_line.h"))?;
    Ok(Manifest {
        idf_version: env.idf_version.clone(),
        idf_commit: env.idf_commit.clone(),
        toolchain: env.toolchain.clone(),
        strip_command: super::manifest::STRIP_COMMAND.to_string(),
        build_dir: home_relative(build_root, crate::hostdirs::home().ok().as_deref()),
        common_sdkconfig_sha256,
        probe_line_h_sha256,
        probes,
    })
}

/// `path` with a leading `home` written as `~`, so the manifest names no user.
pub(super) fn home_relative(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn build_one(
    env: &Env,
    repo_root: &Path,
    name: &str,
    build_root: &Path,
    image_root: &Path,
) -> Result<Probe, String> {
    let project = format!("probes/{name}");
    let project_dir = repo_root.join(&project);
    let build_dir = build_root.join(name);
    discard_foreign_cache(&build_dir, &project_dir)?;
    std::fs::create_dir_all(&build_dir)
        .map_err(|err| format!("cannot create {}: {}", build_dir.display(), err.kind()))?;

    let defaults = format!(
        "{};{}",
        repo_root.join("probes/common/sdkconfig.defaults").display(),
        project_dir.join("sdkconfig.defaults").display()
    );
    // ESP-IDF applies `sdkconfig.defaults` only when it has no `sdkconfig` yet, and keeps an
    // existing one otherwise. Leaving last run's file in place would mean an edited defaults file
    // silently did nothing, so the generated configuration is discarded before every build and
    // rebuilt from the two defaults files. `sdkconfig.old` is the backup idf.py writes beside it.
    for stale in ["sdkconfig", "sdkconfig.old"] {
        let path = build_dir.join(stale);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!("cannot remove {}: {}", path.display(), err.kind()));
            }
        }
    }

    let mut command = Command::new(&env.python);
    command
        .arg(env.idf_path.join("tools/idf.py"))
        .arg("-C")
        .arg(&project_dir)
        .arg("-B")
        .arg(&build_dir)
        .arg("-D")
        .arg(format!("SDKCONFIG_DEFAULTS={defaults}"))
        .arg("-D")
        .arg(format!(
            "SDKCONFIG={}",
            build_dir.join("sdkconfig").display()
        ))
        .arg("build");
    run(&mut command, &env.vars, &format!("idf.py build ({name})"))?;

    let merged = assemble_merged(&build_dir)?;
    std::fs::create_dir_all(image_root)
        .map_err(|err| format!("cannot create {}: {}", image_root.display(), err.kind()))?;
    let image_path = image_root.join(format!("{name}-8MB.bin"));
    std::fs::write(&image_path, &merged)
        .map_err(|err| format!("cannot write {}: {}", image_path.display(), err.kind()))?;

    let mut sources: Vec<Source> = Vec::new();
    for path in super::manifest::project_files(&project_dir, &project)? {
        let (sha256, _) = digest_file(&repo_root.join(&path))?;
        sources.push(Source { path, sha256 });
    }

    let elf_path = build_dir.join(format!("{name}.elf"));
    let stripped_path = strip_elf(env, &elf_path, &build_dir.join(STRIPPED_DIR), name)?;
    let read = |path: &Path| {
        std::fs::read(path).map_err(|err| format!("cannot read {}: {}", path.display(), err.kind()))
    };
    super::loaded::same_loaded_image(&read(&elf_path)?, &read(&stripped_path)?)
        .map_err(|err| format!("{name}: {err}"))?;

    Ok(Probe {
        name: name.to_string(),
        project,
        sources,
        sdkconfig_sha256: digest_file(&build_dir.join("sdkconfig"))?.0,
        strip: super::manifest::strip_mode(name).to_string(),
        elf: artifact(&elf_path)?,
        elf_stripped_path: super::manifest::stripped_elf_path(name),
        elf_stripped: artifact(&stripped_path)?,
        app: artifact(&build_dir.join(format!("{name}.bin")))?,
        bootloader: artifact(&build_dir.join("bootloader/bootloader.bin"))?,
        partition_table: artifact(&build_dir.join("partition_table/partition-table.bin"))?,
        merged: artifact(&image_path)?,
    })
}

/// The source directory a CMake cache was generated for, read from its `CMAKE_HOME_DIRECTORY`
/// entry, or `None` when the text has no such entry.
pub(super) fn cache_home_directory(cache: &str) -> Option<&str> {
    cache.lines().find_map(|line| {
        line.strip_prefix("CMAKE_HOME_DIRECTORY:")
            .and_then(|rest| rest.split_once('='))
            .map(|(_, value)| value.trim())
    })
}

/// Name of the lock file in the shared build root.
pub(super) const LOCK_FILE: &str = ".probes.lock";

/// An exclusive lock on the shared build root, held for a whole `cargo xtask probes` run.
///
/// Every checkout builds in the same canonical directory, so two runs at once would delete each
/// other's build; the second waits here. An OS file lock leaves no stale lock after a crash.
///
/// Drop unlocks explicitly rather than only closing the file: a child being spawned on another
/// thread can hold a copy of the descriptor (measured on macOS: a close-only guard stayed locked
/// in about one release in seven with eight threads spawning `/usr/bin/true`).
pub(super) struct BuildLock {
    file: std::fs::File,
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        // An unlock that fails leaves the close, and at worst the process exit, to release it.
        let _ = self.file.unlock();
    }
}

impl BuildLock {
    /// Waits for the lock, printing once if another run holds it.
    pub(super) fn acquire(build_root: &Path) -> Result<BuildLock, String> {
        if let Some(lock) = Self::try_acquire(build_root)? {
            return Ok(lock);
        }
        println!(
            "probes: another `cargo xtask probes` run holds {}; waiting",
            build_root.join(LOCK_FILE).display()
        );
        let file = Self::open(build_root)?;
        file.lock().map_err(|err| Self::error(build_root, &err))?;
        Ok(BuildLock { file })
    }

    /// Takes the lock only if it is free.
    pub(super) fn try_acquire(build_root: &Path) -> Result<Option<BuildLock>, String> {
        let file = Self::open(build_root)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(BuildLock { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(err)) => Err(Self::error(build_root, &err)),
        }
    }

    fn open(build_root: &Path) -> Result<std::fs::File, String> {
        std::fs::create_dir_all(build_root)
            .map_err(|err| format!("cannot create {}: {}", build_root.display(), err.kind()))?;
        let path = build_root.join(LOCK_FILE);
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|err| format!("cannot open {}: {}", path.display(), err.kind()))
    }

    /// A second descriptor for the same open file description, as a spawned child inherits one.
    #[cfg(test)]
    pub(super) fn descriptor_copy(&self) -> std::fs::File {
        self.file
            .try_clone()
            .expect("the lock file descriptor duplicates")
    }

    fn error(build_root: &Path, err: &std::io::Error) -> String {
        format!(
            "cannot lock {}: {}",
            build_root.join(LOCK_FILE).display(),
            err.kind()
        )
    }
}

/// Removes a probe's build directory unless its CMake cache provably belongs to this checkout.
///
/// The build directory is shared by every checkout (`output_dirs` in `probes.rs`), but CMake
/// refuses a cache generated for another source directory. The directory holds only derived
/// files, and CONFIG_APP_REPRODUCIBLE_BUILD maps the source prefix out, so a fresh configure
/// gives the same artifacts. The cache is kept only when its `CMAKE_HOME_DIRECTORY` names this
/// probe's project directory; no cache at all is left alone. [`BuildLock`] guards the removal.
pub(super) fn discard_foreign_cache(build_dir: &Path, project_dir: &Path) -> Result<(), String> {
    let cache_path = build_dir.join("CMakeCache.txt");
    let bytes = match std::fs::read(&cache_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(format!(
                "cannot read {}: {}",
                cache_path.display(),
                err.kind()
            ));
        }
    };
    // Both sides are canonicalized: the cache may name `/tmp/...` where this process sees
    // `/private/tmp/...`, and the repository root arrives as `xtask/..`.
    let same = String::from_utf8(bytes)
        .ok()
        .as_deref()
        .and_then(cache_home_directory)
        .map(|home| {
            matches!(
                (std::fs::canonicalize(home), std::fs::canonicalize(project_dir)),
                (Ok(cached), Ok(project)) if cached == project
            )
        })
        .unwrap_or(false);
    if same {
        return Ok(());
    }
    println!(
        "probes: {} was not configured for this checkout; discarding it",
        build_dir.display()
    );
    std::fs::remove_dir_all(build_dir)
        .map_err(|err| format!("cannot remove {}: {}", build_dir.display(), err.kind()))
}

/// Directory of the build in which the stripped ELF is made, before it is copied into the
/// repository. Stripping in place would destroy the ELF the app image's SHA-256 refers to.
const STRIPPED_DIR: &str = "stripped";

/// Strips one probe ELF into `dir`, and returns where it landed.
///
/// This is the file committed to the repository: 350 KB rather than the
/// 3.3 MB of the unstripped ELF, and enough to load, disassemble and run. The placeholder
/// sections of [`super::loaded::DUMMY_SECTIONS`] are removed too, which the caller then proves
/// changed nothing loaded. The command is recorded in the manifest as the provenance note, so a
/// reader can reproduce it.
pub fn strip_elf(env: &Env, elf: &Path, dir: &Path, name: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("cannot create {}: {}", dir.display(), err.kind()))?;
    let out = dir.join(format!("{name}.elf"));
    std::fs::copy(elf, &out).map_err(|err| {
        format!(
            "cannot copy {} to {}: {}",
            elf.display(),
            out.display(),
            err.kind()
        )
    })?;
    run(
        Command::new(&env.strip)
            .arg(super::manifest::strip_mode(name))
            .args(
                super::loaded::DUMMY_SECTIONS
                    .iter()
                    .flat_map(|section| ["-R", section]),
            )
            .arg(&out),
        &env.vars,
        &format!("riscv32-esp-elf-strip ({name})"),
    )?;
    Ok(out)
}

/// Where `build_one` left the stripped ELF of `name`.
pub fn stripped_build_path(build_root: &Path, name: &str) -> PathBuf {
    build_root
        .join(name)
        .join(STRIPPED_DIR)
        .join(format!("{name}.elf"))
}

fn artifact(path: &Path) -> Result<Artifact, String> {
    let (sha256, size) = digest_file(path)?;
    Ok(Artifact { sha256, size })
}

/// Assembles the merged 8 MB flash image from the build's `flash_args`.
///
/// `flash_args` is the offset-and-file list ESP-IDF writes for the flasher; reading it keeps the
/// offsets tied to the build's own partition table instead of being hard-coded here. The image is
/// padded with the erased-cell byte, which is what a real 8 MB part reads outside a written
/// region, so the merged image of a probe has the same shape as the corpus images.
pub fn assemble_merged(build_dir: &Path) -> Result<Vec<u8>, String> {
    let path = build_dir.join("flash_args");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {}", path.display(), err.kind()))?;
    let mut image = vec![ERASED; FLASH_SIZE_BYTES as usize];
    let mut placed = 0;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(offset), Some(file)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some(hex) = offset.strip_prefix("0x") else {
            continue; // the leading `--flash_mode ...` line
        };
        let offset = usize::from_str_radix(hex, 16)
            .map_err(|_| format!("{}: `{offset}` is not an offset", path.display()))?;
        let bytes = std::fs::read(build_dir.join(file))
            .map_err(|err| format!("cannot read {file} of the build: {}", err.kind()))?;
        let end = offset + bytes.len();
        if end > image.len() {
            return Err(format!(
                "{file} at {offset:#x} runs past the end of an {FLASH_SIZE_BYTES} byte image"
            ));
        }
        image[offset..end].copy_from_slice(&bytes);
        placed += 1;
    }
    if placed < 3 {
        return Err(format!(
            "{} placed only {placed} image(s); a probe image needs the bootloader, the partition \
             table and the app",
            path.display()
        ));
    }
    Ok(image)
}

/// Runs a command with the IDF environment, returning its standard output.
fn run(
    command: &mut Command,
    vars: &BTreeMap<String, OsString>,
    what: &str,
) -> Result<String, String> {
    for (key, value) in vars {
        command.env(key, value);
    }
    let output = command
        .output()
        .map_err(|err| format!("cannot run {what}: {}", err.kind()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let tail: Vec<&str> = stdout.lines().rev().take(20).collect();
        return Err(format!(
            "{what} failed\n  stderr: {}\n  stdout tail:\n    {}",
            stderr.trim(),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n    ")
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}
