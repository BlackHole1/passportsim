//! The host half of the `inspect`, `ui`, `screenshot` and `scenario` seams. `pemu-api` may not read
//! a file, name the `png` crate or load an ELF, so each command declares a seam of `fn` pointers
//! and [`install`] fills them.
//!
//! The walkers are `pemu_api::elf`'s, shared with the browser; this module supplies where an app
//! ELF comes from ([`corpus_elves`], [`payload_elves`]). A firmware started from a path has no ELF,
//! and its walks say so in [`MISSING_ELF`]'s words.
//!
//! The taint refusal of screenshots lives in `pemu_api::commands::screenshot::screenshot_on`, not
//! here: the taint is `Session::receipt`'s answer, which a writer cannot see, and refusing at the
//! write would already have encoded the capture.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use pemu_api::commands::inspect;
use pemu_api::elf::ElfSource;
use pemu_machine::MachineApi;

pub use pemu_api::elf::{
    ElfContext, ElfResolver, FacadeMemory, INTROSPECTORS, SAFE_POINT_SLICE, at_safe_point, elf_of,
    ui_safe_point,
};

use crate::assets::{Corpus, HostEnv};

// Where this host's ELFs come from.

/// What a caller does about a firmware whose app ELF no source knows. The browser's counterpart
/// (`pemu_wasm::introspect::MISSING_ELF`) names `pemu_load` kind 2 instead.
pub const MISSING_ELF: &str =
    "the app ELF of this firmware (a corpus id with an `elf` file in corpus.toml)";

/// The ELF of a `corpus.toml` id, digest-checked, parsed once per process and kept. A failed read
/// or parse is retried on the next walk, so an ELF added while the daemon runs is picked up.
pub fn corpus_elves(env: HostEnv) -> ElfResolver {
    let cache: Mutex<BTreeMap<String, Arc<ElfContext>>> = Mutex::new(BTreeMap::new());
    Arc::new(move |fw: &str| {
        if let Some(known) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(fw) {
            return Some(Arc::clone(known));
        }
        // Read and parsed outside the lock, so a slow parse holds up no other firmware's walk.
        let context = Corpus::load(&env)
            .ok()
            .and_then(|corpus| corpus.read(fw, pemu_loader::bundle::CORPUS_ELF).ok())
            .and_then(|bytes| ElfContext::parse(&bytes).ok())
            .map(Arc::new)?;
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        Some(Arc::clone(cache.entry(fw.to_owned()).or_insert(context)))
    })
}

/// The `app_elf` role of the `.pebundle` this binary carries. Settle points, `inspect`, `ui` and
/// `--boot-cache` all need the app ELF's DWARF, so this gives a host with no corpus the same
/// walkers as one with it. Parsed once per firmware and kept.
pub fn payload_elves(bundles: crate::backend::PayloadBundles) -> ElfResolver {
    let cache: Mutex<BTreeMap<String, Arc<ElfContext>>> = Mutex::new(BTreeMap::new());
    Arc::new(move |fw: &str| {
        if let Some(known) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(fw) {
            return Some(Arc::clone(known));
        }
        let bytes = bundles(fw)?;
        let bundle = pemu_loader::bundle::Bundle::parse(&bytes).ok()?;
        let elf = bundle.role_data(pemu_loader::bundle::BUNDLE_APP_ELF)?;
        let context = Arc::new(ElfContext::parse(elf).ok()?);
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        Some(Arc::clone(cache.entry(fw.to_owned()).or_insert(context)))
    })
}

// The deadlock watch.

/// The task-level deadlock watch, run after every slice of every command that advances time. It
/// uses [`deadlock_elf`], not the walk gate, so a per-slice check never serializes against other
/// instances' walks. Anything uncertain is `None`.
fn watch_deadlock(
    machine: &mut dyn MachineApi,
    fw: &str,
) -> Option<pemu_introspect::freertos::DeadlockReport> {
    let cx = deadlock_elf(fw)?;
    pemu_api::elf::deadlock_report(machine, &cx)
}

/// The ELF the deadlock watch uses for `fw`, remembered including `None`. Unlike [`elf_of`], a
/// miss is remembered, because a check at every slice boundary must not read the corpus manifest a
/// thousand times a second; an ELF added later is watched only from the next process.
fn deadlock_elf(fw: &str) -> Option<Arc<ElfContext>> {
    static MEMO: OnceLock<Mutex<BTreeMap<String, Option<Arc<ElfContext>>>>> = OnceLock::new();
    let memo = MEMO.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(known) = memo.lock().unwrap_or_else(|e| e.into_inner()).get(fw) {
        return known.clone();
    }
    let context = elf_of(fw);
    memo.lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(fw.to_owned())
        .or_insert(context)
        .clone()
}

// Screenshots.

fn encode(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    crate::png::encode_rgb888(width, height, rgb).map_err(|e| e.to_string())
}

pub const SCREENSHOT_CODEC: pemu_api::commands::screenshot::ScreenshotCodec =
    pemu_api::commands::screenshot::ScreenshotCodec {
        encode,
        decode: crate::png::decode_rgb888,
    };

pub const ARTIFACT_IO: pemu_api::artifact_io::ArtifactIo = pemu_api::artifact_io::ArtifactIo {
    write: crate::artifacts::write_current,
    read: crate::artifacts::read_current,
};

// Scenario files.

/// Largest scenario file read, in bytes; scenarios are a few kilobytes.
pub const MAX_SCENARIO_BYTES: u64 = 1024 * 1024;

/// Most files a glob listing returns.
pub const MAX_LISTED: usize = 10_000;
/// Deepest directory level a glob listing descends to.
pub const MAX_DEPTH: usize = 16;
/// Most directory entries a glob listing looks at, so a pattern over a huge tree costs a bounded
/// walk even when nothing matches.
pub const MAX_VISITED: usize = 50_000;

/// The header a forwarding CLI names its workspace root in, added to that request's scenario
/// roots ([`bind_call_root`]).
pub const SCENARIO_ROOT_HEADER: &str = "passportsim-scenario-root";

/// Where `scenario` reads its files. The CLI resolves a relative path against its working
/// directory; a daemon shares none, so it takes absolute paths only (the CLI absolutizes first).
/// Since `scenario` is reachable over MCP and HTTP, the resolved file must lie under the CLI's
/// workspace or, for a daemon, its config role plus the workspace a forwarding CLI names; a
/// refusal does not say whether the file exists. Only regular `.yaml`/`.yml` files up to
/// [`MAX_SCENARIO_BYTES`] are read, and a glob listing follows no link and stays bounded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScenarioRoot {
    base: Option<PathBuf>,
    roots: Vec<PathBuf>,
}

thread_local! {
    static CALL_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// A caller's root, canonical, when it is an absolute directory other than the file system root.
fn usable_root(root: &std::path::Path) -> Option<PathBuf> {
    if !root.is_absolute() {
        return None;
    }
    let canonical = crate::host_file::canonical(root).ok()?;
    (canonical.is_dir() && canonical.parent().is_some()).then_some(canonical)
}

/// The file marking a directory as a workspace besides a `.git`.
pub const WORKSPACE_MARKER: &str = "passportsim.toml";

fn is_workspace(dir: &std::path::Path) -> bool {
    dir.join(".git").exists() || dir.join(WORKSPACE_MARKER).exists()
}

/// A root from [`SCENARIO_ROOT_HEADER`], canonical, when it is an existing directory other than
/// `/`, owned by the daemon's user, and the top of a workspace.
fn usable_call_root(root: &std::path::Path) -> Option<PathBuf> {
    let canonical = usable_root(root)?;
    (is_workspace(&canonical) && crate::platform::owned_by_current_user(&canonical))
        .then_some(canonical)
}

/// Runs `f` with `root` as an extra scenario root of this thread, if [`usable_call_root`] accepts
/// it. Any token holder can send the header, but gains nothing: the token already lets it name
/// any firmware path to `start`.
pub fn bind_call_root<R>(root: Option<&str>, f: impl FnOnce() -> R) -> R {
    rebind_call_root(
        root.and_then(|text| usable_call_root(std::path::Path::new(text))),
        f,
    )
}

pub fn call_root() -> Option<PathBuf> {
    CALL_ROOT.with(|slot| slot.borrow().clone())
}

/// Runs `f` with a `root` already checked by [`bind_call_root`] on another thread.
pub fn rebind_call_root<R>(root: Option<PathBuf>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CALL_ROOT.with(|slot| *slot.borrow_mut() = previous);
        }
    }
    let previous = CALL_ROOT.with(|slot| slot.replace(root));
    let _restore = Restore(previous);
    f()
}

/// The workspace a CLI runs in: the nearest ancestor of `cwd` holding a `.git` or a
/// [`WORKSPACE_MARKER`].
pub fn workspace_root(cwd: &std::path::Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| is_workspace(dir))
        .map(std::path::Path::to_path_buf)
}

/// The refusal of a path outside the roots or one that does not exist: the same words for both.
const NOT_UNDER_ROOTS: &str =
    "not a scenario file under the scenario roots (the workspace, or the daemon's config role)";

impl ScenarioRoot {
    /// Relative paths resolve against `base` (`None` for a daemon: absolute only), and only files
    /// under `roots` are read. A root that is not a directory is dropped.
    pub fn new(base: Option<PathBuf>, roots: Vec<PathBuf>) -> ScenarioRoot {
        ScenarioRoot {
            base,
            roots: roots.iter().filter_map(|root| usable_root(root)).collect(),
        }
    }

    fn under_roots(&self, path: &std::path::Path) -> bool {
        self.roots
            .iter()
            .cloned()
            .chain(call_root())
            .any(|root| crate::paths::contains(&root, path).unwrap_or(false))
    }

    fn native(&self, path: &str) -> Result<PathBuf, String> {
        let as_path = std::path::Path::new(path);
        if as_path.is_absolute() {
            return Ok(as_path.to_path_buf());
        }
        if path.split('/').any(|segment| segment == "..") {
            return Err("a scenario path has no `..` segment".to_owned());
        }
        match &self.base {
            Some(base) => Ok(base.join(path)),
            None => Err(
                "this daemon shares no working directory with its caller, so a scenario path \
                 must be absolute (the CLI makes it absolute before it forwards)"
                    .to_owned(),
            ),
        }
    }

    pub fn read_text(&self, path: &str) -> Result<String, String> {
        if !(path.ends_with(".yaml") || path.ends_with(".yml")) {
            return Err("a scenario file name ends in `.yaml` or `.yml`".to_owned());
        }
        let native = self.native(path)?;
        let canonical =
            crate::host_file::canonical(&native).map_err(|_| NOT_UNDER_ROOTS.to_owned())?;
        if !self.under_roots(&canonical) {
            return Err(NOT_UNDER_ROOTS.to_owned());
        }
        // The name that counts is the one the links lead to, not the one the caller typed.
        if !matches!(
            canonical.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml")
        ) {
            return Err("a scenario file name ends in `.yaml` or `.yml`".to_owned());
        }
        let bytes = crate::host_file::read_regular(&canonical, MAX_SCENARIO_BYTES)
            .map_err(|_| "not a readable scenario file".to_owned())?;
        String::from_utf8(bytes).map_err(|_| "the file is not UTF-8".to_owned())
    }

    /// `path` as a result shows it: forward-slashed and relative to the deepest scenario root
    /// holding it. An absolute path under no root shows its file name alone, so no host path
    /// leaves. A Windows rooted path without a drive (`\Users\x\a.yaml`) counts as absolute.
    pub fn display(&self, path: &str) -> String {
        let as_path = std::path::Path::new(path);
        if !as_path.is_absolute() && !as_path.has_root() {
            return path.replace('\\', "/");
        }
        // The roots are canonical and the path may not be (`/tmp` vs `/private/tmp`), so its
        // nearest existing ancestor is canonicalized and the rest appended.
        let canonical = as_path.ancestors().find_map(|ancestor| {
            let rest = as_path.strip_prefix(ancestor).ok()?;
            crate::host_file::canonical(ancestor)
                .ok()
                .map(|real| real.join(rest))
        });
        let mut best: Option<PathBuf> = None;
        for root in self
            .roots
            .iter()
            .cloned()
            .chain(call_root())
            .chain(self.base.clone())
        {
            let relative = [Some(as_path), canonical.as_deref()]
                .into_iter()
                .flatten()
                .find_map(|candidate| candidate.strip_prefix(&root).ok().map(PathBuf::from));
            if let Some(relative) = relative
                && best
                    .as_ref()
                    .is_none_or(|b| relative.components().count() < b.components().count())
            {
                best = Some(relative);
            }
        }
        match best {
            Some(relative) => relative
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/"),
            None => as_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    /// Every regular file under `root`, as `root` joined with its forward-slashed relative path
    /// (what `pemu_api::commands::scenario::expand` matches against).
    pub fn list_files(&self, root: &str) -> Result<Vec<String>, String> {
        let dir = if root.is_empty() {
            self.base
                .clone()
                .ok_or_else(|| "a daemon lists absolute directories only".to_owned())?
        } else {
            self.native(root)?
        };
        let canonical =
            crate::host_file::canonical(&dir).map_err(|_| NOT_UNDER_ROOTS.to_owned())?;
        if !self.under_roots(&canonical) {
            return Err(NOT_UNDER_ROOTS.to_owned());
        }
        let prefix = root.trim_end_matches('/');
        let mut out = Vec::new();
        let mut visited = 0usize;
        let mut pending = vec![(canonical, String::new(), 0usize)];
        while let Some((native, relative, depth)) = pending.pop() {
            let entries = std::fs::read_dir(&native).map_err(|e| format!("{}", e.kind()))?;
            for entry in entries.flatten() {
                visited += 1;
                if visited > MAX_VISITED {
                    return Err(format!(
                        "more than {MAX_VISITED} entries under the pattern root"
                    ));
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let rel = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                // `file_type` does not follow a symlink, so a link loop cannot recurse and a link
                // out of the roots is never listed.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() && depth < MAX_DEPTH {
                    pending.push((entry.path(), rel, depth + 1));
                } else if kind.is_file() {
                    out.push(if prefix.is_empty() {
                        rel
                    } else {
                        format!("{prefix}/{rel}")
                    });
                }
                if out.len() > MAX_LISTED {
                    return Err(format!(
                        "more than {MAX_LISTED} files under the pattern root"
                    ));
                }
            }
        }
        out.sort_unstable();
        Ok(out)
    }
}

fn installed_scenario_root() -> &'static Mutex<ScenarioRoot> {
    static ROOT: OnceLock<Mutex<ScenarioRoot>> = OnceLock::new();
    ROOT.get_or_init(|| Mutex::new(ScenarioRoot::default()))
}

fn scenario_root() -> ScenarioRoot {
    installed_scenario_root()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn read_text(path: &str) -> Result<String, String> {
    scenario_root().read_text(path)
}

fn list_files(root: &str) -> Result<Vec<String>, String> {
    scenario_root().list_files(root)
}

fn display_path(path: &str) -> String {
    scenario_root().display(path)
}

fn write_text(path: &str, text: &str) -> Result<String, String> {
    crate::artifacts::write_current(path, text.as_bytes())
}

// Install.

#[derive(Clone)]
pub struct HostHooks {
    pub elves: ElfResolver,
    pub scenario_root: ScenarioRoot,
    /// The directory of this installation's export salt; `None` for a per-process salt.
    pub salt_dir: Option<PathBuf>,
}

impl HostHooks {
    pub fn product(
        env: HostEnv,
        base: Option<PathBuf>,
        roots: Vec<PathBuf>,
        salt_dir: Option<PathBuf>,
    ) -> HostHooks {
        HostHooks {
            elves: corpus_elves(env),
            scenario_root: ScenarioRoot::new(base, roots),
            salt_dir,
        }
    }

    /// The same hooks, with the bundles this binary carries behind the corpus: an owner's own ELF
    /// wins over the shipped copy.
    #[must_use]
    pub fn with_payload_bundles(mut self, bundles: crate::backend::PayloadBundles) -> HostHooks {
        let corpus = Arc::clone(&self.elves);
        let payload = payload_elves(bundles);
        self.elves = Arc::new(move |fw: &str| corpus(fw).or_else(|| payload(fw)));
        self
    }
}

/// The export salt file, below the host's config role.
pub const EXPORT_SALT_FILE: &str = "export-salt";

pub const EXPORT_SALT_BYTES: usize = 32;

/// The export salt of this installation: `snapshot export` salts its `Redacted{<sha256>}` labels
/// with it, so two exports of one installation compare and nobody without the salt can test a
/// guess. Read from `<dir>/export-salt`, else made from OS entropy and created exclusively; a
/// process that lost the race reads the winner's.
///
/// # Errors
///
/// The directory or file is not owner-only, the file has another length, or there is no entropy.
pub fn export_salt(
    dir: &std::path::Path,
    files: crate::paths::OwnerOnlyFiles<'_>,
) -> Result<[u8; EXPORT_SALT_BYTES], String> {
    let path = dir.join(EXPORT_SALT_FILE);
    let read = |files: &crate::paths::OwnerOnlyFiles<'_>| {
        let bytes = files.read(&path).map_err(|e| e.to_string())?;
        <[u8; EXPORT_SALT_BYTES]>::try_from(bytes.as_slice())
            .map_err(|_| format!("{EXPORT_SALT_FILE} is not {EXPORT_SALT_BYTES} bytes"))
    };
    if path.exists() {
        return read(&files);
    }
    files.create_dir(dir).map_err(|e| e.to_string())?;
    let mut salt = [0u8; EXPORT_SALT_BYTES];
    getrandom::fill(&mut salt).map_err(|_| "the OS has no entropy for a salt".to_owned())?;
    match files.write_new(&path, &salt) {
        Ok(()) => Ok(salt),
        Err(_) if path.exists() => read(&files),
        Err(e) => Err(e.to_string()),
    }
}

fn installed_salt_dir() -> &'static Mutex<Option<PathBuf>> {
    static DIR: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    DIR.get_or_init(|| Mutex::new(None))
}

/// The salt `snapshot export` asks for: the installation's, or a per-process one when there is no
/// usable salt file.
fn installed_export_salt() -> Option<Vec<u8>> {
    let dir = installed_salt_dir()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    dir.and_then(|dir| export_salt(&dir, crate::paths::OwnerOnlyFiles::host()).ok())
        .or_else(process_salt)
        .map(|salt| salt.to_vec())
}

/// A never-persisted per-process salt, so labels still say nothing to anyone else.
fn process_salt() -> Option<[u8; EXPORT_SALT_BYTES]> {
    let mut salt = [0u8; EXPORT_SALT_BYTES];
    getrandom::fill(&mut salt).ok().map(|()| salt)
}

/// Installs the walkers, the settle point, the `screenshot` codec, artifact access, the export salt
/// and `scenario` file access of this process, beside [`crate::backend::install`]. A second call
/// replaces the first.
pub fn install(hooks: HostHooks) {
    *installed_scenario_root()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = hooks.scenario_root;
    // Exports, screenshots, btsnoop and pcap captures go through the instance's artifact
    // directory, already redacted.
    pemu_api::artifact_io::set(ARTIFACT_IO);
    // The salt file is made at the first export, so a run that exports nothing creates nothing in
    // the config role.
    *installed_salt_dir()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = hooks.salt_dir;
    pemu_api::commands::snapshot::with_seams(|seams| {
        seams.set_salt(Vec::new());
        seams.set_salt_source(Some(installed_export_salt));
    });
    pemu_api::elf::install_walkers(ElfSource::new(hooks.elves, MISSING_ELF));
    // The task-level deadlock, checked at every slice boundary.
    inspect::set_deadlock_watch(Some(watch_deadlock));
    // The ELF of a walk is loaded before the walk gate is taken.
    inspect::set_walk_prepare(Some(|fw| {
        let _ = elf_of(fw);
    }));
    pemu_api::commands::start::with_pool(|pool| {
        pool.set_ui_settle(Some(crate::boot_cache::ui_settle_hook));
    });
    pemu_api::commands::screenshot::set_io(SCREENSHOT_CODEC);
    // `env --ble-bridge attach` opens the external HCI transport (an H4 stream on loopback).
    crate::endpoints::hci::install();
    // `net_http --op bridge` opens the Wi-Fi bridge, a WISP server over loopback sockets.
    crate::relay_wisp::install();
    pemu_api::commands::scenario::set_io(pemu_api::commands::scenario::ScenarioIo {
        read_text,
        list_files,
        write_text,
        display_path,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-hooks-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        dir
    }

    /// The walkers are tested in `pemu_api::elf`; this checks the ELF source is installed and that
    /// an unknown firmware is refused in this host's words.
    #[test]
    fn a_walk_without_an_elf_names_the_corpus_entry_this_host_wants() {
        let mut machine =
            crate::backend::build_machine(pemu_loader::bundle::FlashImage::erased(), 1)
                .expect("the bundled ROM machine");
        install(HostHooks {
            elves: Arc::new(|_| None),
            // Only the ELF source changes: the other seams are process-wide and other tests read
            // them.
            scenario_root: scenario_root(),
            salt_dir: None,
        });
        let unknown =
            inspect::with_walk_firmware("merged.bin", || (INTROSPECTORS.ui)(&mut machine, 1))
                .expect_err("no ELF for a path firmware");
        assert_eq!(unknown.to_string(), format!("no symbol {MISSING_ELF}"));
        assert!(MISSING_ELF.contains("corpus.toml"), "{MISSING_ELF}");
        let settle = at_safe_point("merged.bin", &mut machine).expect_err("no ELF for it");
        assert_eq!(settle.to_string(), unknown.to_string());
    }

    #[test]
    fn a_scenario_path_is_a_yaml_file_resolved_against_the_base_or_absolute() {
        let base = temp("scenario");
        std::fs::create_dir_all(base.join("tests/scenarios/deep")).expect("mkdir");
        std::fs::write(base.join("tests/scenarios/a.yaml"), "name: a\n").expect("write");
        std::fs::write(base.join("tests/scenarios/deep/b.yml"), "name: b\n").expect("write");
        std::fs::write(base.join("secret.txt"), "token").expect("write");

        let cli = ScenarioRoot::new(Some(base.clone()), vec![base.clone()]);
        assert_eq!(
            cli.read_text("tests/scenarios/a.yaml").as_deref(),
            Ok("name: a\n")
        );
        assert!(cli.read_text("secret.txt").is_err(), "not a scenario file");
        assert!(cli.read_text("tests/../tests/scenarios/a.yaml").is_err());
        assert_eq!(
            cli.list_files("tests/scenarios").expect("lists"),
            ["tests/scenarios/a.yaml", "tests/scenarios/deep/b.yml"]
        );

        let daemon = ScenarioRoot::new(None, vec![base.clone()]);
        let relative = daemon
            .read_text("tests/scenarios/a.yaml")
            .expect_err("no base");
        assert!(relative.contains("absolute"), "{relative}");
        let absolute = base.join("tests/scenarios/a.yaml");
        assert_eq!(
            daemon.read_text(&absolute.to_string_lossy()).as_deref(),
            Ok("name: a\n")
        );
        let root = base.join("tests/scenarios").to_string_lossy().into_owned();
        let listed = daemon.list_files(&root).expect("an absolute root lists");
        assert_eq!(listed[0], format!("{root}/a.yaml"));
        let hits = pemu_api::commands::scenario::expand(&format!("{root}/*.yaml"), &listed);
        assert_eq!(hits, [format!("{root}/a.yaml")], "`*` stays in one segment");
        assert_eq!(daemon.display(&hits[0]), "tests/scenarios/a.yaml");
        assert_eq!(
            daemon.display(&base.join("tests/scenarios/gone.yaml").to_string_lossy()),
            "tests/scenarios/gone.yaml",
            "a path that does not exist is shown relative too"
        );
        assert_eq!(
            daemon.display("/elsewhere/private/x.yaml"),
            "x.yaml",
            "a path under no root shows no host directory"
        );
        assert_eq!(
            cli.display("tests\\scenarios\\a.yaml"),
            "tests/scenarios/a.yaml"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// 21 nested `ui.expect` lines failing only at the leaf took exponential time before the memo
    /// in `pemu_api::scenario`. Timed here, because a core crate reads no clock.
    #[test]
    fn a_ui_expect_tree_of_depth_twenty_is_decided_in_under_50_ms() {
        use pemu_introspect::lvgl::{UiNode, UiTree};
        const K: usize = 20;
        let tree = UiTree {
            nodes: (0..=K)
                .map(|depth| UiNode {
                    obj: depth as u32 + 1,
                    class: "obj".to_owned(),
                    depth: depth as u32,
                    text: (depth == K).then(|| "leaf".to_owned()),
                    ..UiNode::default()
                })
                .collect(),
            ..UiTree::default()
        };
        for (leaf, holds) in [("absent", false), ("leaf", true)] {
            let mut text = String::from("tree: |\n");
            for depth in 0..K {
                text.push_str(&format!("{}- obj\n", "  ".repeat(depth + 1)));
            }
            text.push_str(&format!("{}- obj \"{leaf}\"\n", "  ".repeat(K + 1)));
            let yaml = pemu_api::scenario::Yaml::parse(&text).expect("reads");
            let expect = pemu_api::scenario::UiExpect::parse(&yaml).expect("parses");
            let started = std::time::Instant::now();
            let failures = expect.failures(&tree);
            let took = started.elapsed();
            assert_eq!(failures.is_empty(), holds, "{leaf}: {failures:?}");
            assert!(
                took < std::time::Duration::from_millis(50),
                "{leaf}: took {took:?}"
            );
        }
    }

    /// Made with `mkfifo(3)` in process, never a spawned `mkfifo`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_fifo_named_like_a_scenario_is_refused_without_blocking() {
        let root = temp("fifo");
        let fifo = root.join("pipe.yaml");
        crate::platform::macos::make_fifo(&fifo).expect("mkfifo");
        let reader = ScenarioRoot::new(None, vec![root.clone()]);
        assert!(reader.read_text(&fifo.to_string_lossy()).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_export_salt_is_created_once_owner_only_and_read_back() {
        use crate::paths::OwnerOnlyFiles;
        use crate::platform::fake::{FakeOwnerOnly, OwnerOnlyCall};
        let dir = temp("salt").join("config");
        let guard = FakeOwnerOnly::new();
        let first = export_salt(&dir, OwnerOnlyFiles::new(&guard)).expect("created");
        let file = dir.join(EXPORT_SALT_FILE);
        assert!(
            guard
                .calls()
                .contains(&OwnerOnlyCall::CreateNewFile(file.clone()))
        );
        assert_ne!(first, [0; EXPORT_SALT_BYTES]);
        assert_eq!(export_salt(&dir, OwnerOnlyFiles::new(&guard)), Ok(first));
        assert!(guard.checked(&file), "re-checked on read");
        guard.refuse(&file);
        assert!(export_salt(&dir, OwnerOnlyFiles::new(&FakeOwnerOnly::new())).is_ok());
        assert!(export_salt(&dir, OwnerOnlyFiles::new(&guard)).is_err());
        std::fs::write(&file, b"short").expect("write");
        assert!(export_salt(&dir, OwnerOnlyFiles::new(&FakeOwnerOnly::new())).is_err());
        std::fs::remove_dir_all(dir.parent().expect("temp")).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scenario_files_are_read_only_under_the_roots_and_links_do_not_escape() {
        let root = temp("roots");
        let outside = temp("outside");
        std::fs::create_dir_all(root.join("s")).expect("mkdir");
        std::fs::write(root.join("s/a.yaml"), "name: a\n").expect("write");
        std::fs::write(outside.join("b.yaml"), "name: b\n").expect("write");
        std::fs::write(outside.join("secret.txt"), "token").expect("write");
        std::os::unix::fs::symlink(outside.join("b.yaml"), root.join("s/link.yaml")).expect("ln");
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("s/renamed.yaml"))
            .expect("ln");
        std::os::unix::fs::symlink(&outside, root.join("s/dir")).expect("ln");
        let path = |p: &std::path::Path| p.to_string_lossy().into_owned();

        let daemon = ScenarioRoot::new(None, vec![root.clone()]);
        assert_eq!(
            daemon.read_text(&path(&root.join("s/a.yaml"))).as_deref(),
            Ok("name: a\n")
        );
        let refused = daemon
            .read_text(&path(&outside.join("b.yaml")))
            .expect_err("outside");
        let missing = daemon
            .read_text(&path(&outside.join("none.yaml")))
            .expect_err("missing");
        assert_eq!(
            refused, missing,
            "a refusal does not say whether the file exists"
        );
        assert!(
            daemon.read_text(&path(&root.join("s/link.yaml"))).is_err(),
            "link out"
        );
        assert!(
            daemon
                .read_text(&path(&root.join("s/renamed.yaml")))
                .is_err()
        );
        assert!(daemon.list_files(&path(&outside)).is_err());
        assert_eq!(
            daemon.list_files(&path(&root.join("s"))).expect("lists"),
            [path(&root.join("s/a.yaml"))],
            "links are not listed"
        );

        let named = path(&outside);
        bind_call_root(Some(&named), || {
            assert_eq!(call_root(), None, "not a workspace: no `.git` or marker");
        });
        std::fs::write(outside.join(WORKSPACE_MARKER), "").expect("marker");
        bind_call_root(Some(&named), || {
            assert_eq!(
                daemon.read_text(&path(&outside.join("b.yaml"))).as_deref(),
                Ok("name: b\n")
            );
            assert!(
                daemon
                    .read_text(&path(&root.join("s/renamed.yaml")))
                    .is_err()
            );
            let moved = call_root();
            let seen = std::thread::spawn(move || rebind_call_root(moved, call_root))
                .join()
                .expect("no panic");
            assert_eq!(seen, call_root(), "a worker thread sees the request's root");
        });
        assert!(
            daemon.read_text(&path(&outside.join("b.yaml"))).is_err(),
            "unbound again"
        );
        for useless in ["/", "relative/dir", "/no/such/dir"] {
            bind_call_root(Some(useless), || assert_eq!(call_root(), None, "{useless}"));
        }
        std::fs::create_dir_all(root.join("ws/.git")).expect("mkdir");
        std::fs::create_dir_all(root.join("ws/a/b")).expect("mkdir");
        assert_eq!(workspace_root(&root.join("ws/a/b")), Some(root.join("ws")));
        assert_eq!(
            workspace_root(&root.join("s")),
            None,
            "no workspace above a temp dir"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
