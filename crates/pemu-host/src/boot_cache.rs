//! The boot cache: a snapshot at the first LVGL safe point after `app_main` returns, so a suite of
//! scenarios on one build boots once.
//!
//! The key is a hash of [`KeyParts`], never a path, so a cache is portable and leaks no user
//! name. An entry is written to `<key>.tmp.<pid>.<n>` and renamed into place (the only atomic
//! publish both file systems offer), with a bounded retry because a Windows rename fails while the
//! target is open. The first writer wins, except over an entry that no longer restores
//! ([`BootCache::replace`]). Each file is [`ENTRY_MAGIC`], the blake3 of the snapshot, then the
//! snapshot; a mismatch is [`Error::Damaged`]. Entries are read fully, never memory-mapped, since
//! an open mapping would block the next rename on Windows.
//!
//! On-disk entries are owner-only because a boot snapshot is guest RAM, and a tainted machine's
//! entries stay in memory ([`BootCache::in_memory`]).

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use crate::paths::{GuardError, OwnerOnlyFiles};

pub const CACHE_DIR: &str = "boot-cache";

/// How many times a rename is retried. A Windows sharing violation clears in milliseconds when a
/// reader closes and never clears when something else holds the target, so giving up (one cold
/// boot) beats hanging the daemon.
pub const RENAME_RETRIES: u32 = 8;

pub const ENTRY_MAGIC: [u8; 8] = *b"PEMUBOOT";

fn frame_entry(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENTRY_MAGIC.len() + 32 + bytes.len());
    out.extend_from_slice(&ENTRY_MAGIC);
    out.extend_from_slice(&pemu_core::snap::stored_digest(bytes));
    out.extend_from_slice(bytes);
    out
}

fn unframe_entry(file: &[u8]) -> Option<&[u8]> {
    let rest = file.strip_prefix(&ENTRY_MAGIC)?;
    let (digest, bytes) = rest.split_at_checked(32)?;
    (digest == pemu_core::snap::stored_digest(bytes)).then_some(bytes)
}

/// The temporary name of one publish: `<key>.tmp.<pid>.<n>`, unique across processes by the pid
/// and within one by the counter.
fn temporary_name(key: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{key}.tmp.{}.{n}", std::process::id())
}

/// Most entries an in-memory store keeps: each is a whole machine snapshot of several MiB.
pub const MEMORY_ENTRIES: usize = 8;

const RENAME_BACKOFF: Duration = Duration::from_millis(20);

/// The emulator build id ([`crate::build_id`], computed by `build.rs`). Content-only, so any edit
/// to what boots a machine gives another key.
pub const BUILD_ID: &str = env!("PEMU_BUILD_ID");

/// What a boot-cache key is taken over. Every field is a hash or short token the caller computed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyParts {
    /// SHA-256 of every flashed byte after the flash overlay, plus the app and bootloader ELF
    /// hashes.
    pub bundle_id: String,
    pub rom_sha: String,
    pub efuse_kind: String,
    pub efuse_sha: String,
    pub config_hash: String,
    pub profile: String,
    /// Hash of the NVS seed: a different seed must miss.
    pub nvs_seed_sha: String,
    /// The boot-relevant initial environment (USB U-state, battery, NFC card, RTC epoch), in a
    /// stable order.
    pub environment: String,
    pub format_version: String,
    pub build_id: String,
    pub settle_point: String,
}

impl KeyParts {
    /// The key: lower-case hex SHA-256 over the parts, each length-prefixed so `("ab", "c")` and
    /// `("a", "bc")` differ.
    pub fn key(&self) -> String {
        let mut buf = Vec::new();
        for part in [
            &self.bundle_id,
            &self.rom_sha,
            &self.efuse_kind,
            &self.efuse_sha,
            &self.config_hash,
            &self.profile,
            &self.nvs_seed_sha,
            &self.environment,
            &self.format_version,
            &self.build_id,
            &self.settle_point,
        ] {
            buf.extend_from_slice(&(part.len() as u64).to_le_bytes());
            buf.extend_from_slice(part.as_bytes());
        }
        pemu_loader::hex(&pemu_loader::sha256(&buf))
    }
}

/// Whether `key` is 64 lower-case hex characters, so it can never be a path or a device name.
pub fn key_is_valid(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug)]
pub enum Error {
    BadKey(String),
    Io(PathBuf, io::Error),
    Guard(GuardError),
    /// The entry's magic or blake3 digest does not match; it is never handed to a restore.
    Damaged(String),
    /// The rename kept failing; the entry already there wins.
    RenameGaveUp {
        key: String,
        attempts: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadKey(key) => write!(
                f,
                "`{key}` is not a boot-cache key: a key is 64 lower-case hex characters"
            ),
            Error::Io(path, e) => write!(f, "{}: {e}", path.display()),
            Error::Guard(e) => write!(f, "{e}"),
            Error::Damaged(key) => write!(
                f,
                "the boot-cache entry `{key}` does not match its digest; it is not restored"
            ),
            Error::RenameGaveUp { key, attempts } => write!(
                f,
                "the boot-cache entry `{key}` could not be published after {attempts} attempts; \
                 the entry already in place is kept and this boot is not cached"
            ),
        }
    }
}

impl std::error::Error for Error {}

type MemoryEntries = (
    BTreeMap<String, Vec<u8>>,
    std::collections::VecDeque<String>,
);

enum Store<'a> {
    /// On disk, owner-only, under the cache role.
    Disk {
        dir: PathBuf,
        files: OwnerOnlyFiles<'a>,
    },
    /// In memory only (tainted machines): at most [`MEMORY_ENTRIES`], oldest dropped first.
    Memory(Mutex<MemoryEntries>),
}

pub struct BootCache<'a> {
    store: Store<'a>,
}

impl fmt::Debug for BootCache<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.store {
            Store::Disk { dir, .. } => f.debug_struct("BootCache").field("dir", dir).finish(),
            Store::Memory(_) => f.write_str("BootCache(in memory: tainted)"),
        }
    }
}

impl<'a> BootCache<'a> {
    /// The on-disk cache under `cache_role/boot-cache/`, created owner-only.
    pub fn on_disk(cache_role: &Path, files: OwnerOnlyFiles<'a>) -> Result<BootCache<'a>, Error> {
        let dir = cache_role.join(CACHE_DIR);
        files.create_dir(&dir).map_err(Error::Guard)?;
        Ok(BootCache {
            store: Store::Disk { dir, files },
        })
    }

    /// The cache a tainted machine gets: same API, nothing on disk.
    pub fn in_memory() -> BootCache<'static> {
        BootCache {
            store: Store::Memory(Mutex::new((
                BTreeMap::new(),
                std::collections::VecDeque::new(),
            ))),
        }
    }

    pub fn is_on_disk(&self) -> bool {
        matches!(self.store, Store::Disk { .. })
    }

    /// The entry for `key`, read fully into memory. A missing entry is `Ok(None)`; a present but
    /// unreadable one is an error, since a file another user could write must not become guest RAM.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        if !key_is_valid(key) {
            return Err(Error::BadKey(key.to_string()));
        }
        match &self.store {
            Store::Memory(map) => Ok(map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
                .get(key)
                .cloned()),
            Store::Disk { dir, files } => {
                let path = dir.join(key);
                if !path.exists() {
                    return Ok(None);
                }
                let file = files.read(&path).map_err(Error::Guard)?;
                unframe_entry(&file)
                    .map(|bytes| Some(bytes.to_vec()))
                    .ok_or_else(|| Error::Damaged(key.to_string()))
            }
        }
    }

    /// Publishes an entry. Returns whether this call published it; `false` means another writer
    /// got there first, which is fine since both hold the same bytes.
    pub fn put(&self, key: &str, bytes: &[u8]) -> Result<bool, Error> {
        self.publish(key, bytes, false)
    }

    /// Publishes over an existing entry that no longer restores, through the same atomic rename,
    /// so a reader sees the old entry or the new one.
    pub fn replace(&self, key: &str, bytes: &[u8]) -> Result<bool, Error> {
        self.publish(key, bytes, true)
    }

    fn publish(&self, key: &str, bytes: &[u8], replace: bool) -> Result<bool, Error> {
        if !key_is_valid(key) {
            return Err(Error::BadKey(key.to_string()));
        }
        match &self.store {
            Store::Memory(map) => {
                let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
                let (entries, order) = &mut *guard;
                if entries.contains_key(key) {
                    if !replace {
                        return Ok(false);
                    }
                } else {
                    while entries.len() >= MEMORY_ENTRIES {
                        let Some(oldest) = order.pop_front() else {
                            break;
                        };
                        entries.remove(&oldest);
                    }
                    order.push_back(key.to_string());
                }
                entries.insert(key.to_string(), bytes.to_vec());
                Ok(true)
            }
            Store::Disk { dir, files } => {
                let final_path = dir.join(key);
                if !replace && final_path.exists() {
                    return Ok(false);
                }
                let temp = dir.join(temporary_name(key));
                files
                    .write_new(&temp, &frame_entry(bytes))
                    .map_err(Error::Guard)?;
                let published = rename_with_retry(&temp, &final_path, RENAME_RETRIES);
                match published {
                    Ok(()) => Ok(true),
                    Err(_) if !replace && final_path.exists() => {
                        // Another writer published first; it wins and the temporary goes away.
                        let _ = std::fs::remove_file(&temp);
                        Ok(false)
                    }
                    Err(_) => {
                        let _ = std::fs::remove_file(&temp);
                        Err(Error::RenameGaveUp {
                            key: key.to_string(),
                            attempts: RENAME_RETRIES,
                        })
                    }
                }
            }
        }
    }

    pub fn keys(&self) -> Result<Vec<String>, Error> {
        match &self.store {
            Store::Memory(map) => Ok(map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
                .keys()
                .cloned()
                .collect()),
            Store::Disk { dir, .. } => {
                let mut keys = Vec::new();
                let entries = std::fs::read_dir(dir).map_err(|e| Error::Io(dir.clone(), e))?;
                for entry in entries {
                    let entry = entry.map_err(|e| Error::Io(dir.clone(), e))?;
                    if let Some(name) = entry.file_name().to_str()
                        && key_is_valid(name)
                    {
                        keys.push(name.to_string());
                    }
                }
                keys.sort();
                Ok(keys)
            }
        }
    }
}

/// Renames `from` to `to`, retrying a bounded number of times. A Windows rename can fail while
/// another process has the target open; on macOS the retry never triggers.
pub fn rename_with_retry(from: &Path, to: &Path, attempts: u32) -> io::Result<()> {
    let mut last = None;
    for attempt in 0..attempts.max(1) {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                if attempt + 1 < attempts {
                    std::thread::sleep(RENAME_BACKOFF);
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("the rename was never attempted")))
}

// `start --boot-cache ui-settled`.

/// Virtual time between two settle checks of a cold boot. Slices are fixed from instant 0, so a
/// cold boot settles at the same instruction every time and a restored entry equals it.
pub const SETTLE_SLICE: pemu_core::time::VTime = pemu_core::time::VTime::from_ms(10);

/// The line ESP-IDF's main task prints when `app_main` returns (`app_startup.c`).
pub const APP_MAIN_RETURNED: &str = "Returned from app_main()";

fn installed_dir() -> &'static Mutex<Option<PathBuf>> {
    static DIR: std::sync::OnceLock<Mutex<Option<PathBuf>>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| Mutex::new(None))
}

/// The in-memory store: tainted machines, and every entry of a host with no cache role.
fn memory() -> &'static BootCache<'static> {
    static MEMORY: std::sync::OnceLock<BootCache<'static>> = std::sync::OnceLock::new();
    MEMORY.get_or_init(BootCache::in_memory)
}

/// Makes `start --boot-cache` answer in this process: untainted entries go to
/// `<cache_role>/boot-cache/`, or stay in memory with no cache role. A second call replaces it.
pub fn install(cache_role: Option<PathBuf>) {
    *installed_dir().lock().unwrap_or_else(|e| e.into_inner()) = cache_role;
    pemu_api::commands::start::with_pool(|pool| pool.set_boot_cache(Some(start_hook)));
}

/// The store for a machine: memory when `tainted` or when no directory is installed, else disk.
///
/// # Errors
///
/// The owner-only cache directory cannot be created.
pub fn store_for(tainted: bool) -> Result<(&'static str, Option<BootCache<'static>>), Error> {
    let dir = installed_dir()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match (tainted, dir) {
        (false, Some(dir)) => Ok((
            "disk",
            Some(BootCache::on_disk(&dir, OwnerOnlyFiles::host())?),
        )),
        _ => Ok(("memory", None)),
    }
}

/// [`store_for`] of the taint this session's receipt reports, so the choice always reads it.
///
/// # Errors
///
/// The owner-only cache directory cannot be created.
pub fn store_for_session(
    session: &mut pemu_api::commands::start::Session,
) -> Result<(&'static str, Option<BootCache<'static>>), Error> {
    store_for(session.receipt().tainted)
}

/// The definition of the `ui-settled` instant as it enters the key: point name, awaited line and
/// slice. Changing any of them misses.
pub fn settle_point(point: pemu_api::commands::start::BootCachePoint) -> String {
    format!(
        "{}: first LVGL safe point (lock free, layout valid, not rendering) checked every {} us \
         of virtual time from instant 0, after the console printed {APP_MAIN_RETURNED:?}",
        point.as_str(),
        SETTLE_SLICE.as_us()
    )
}

/// The boot-relevant environment of the key from the `start` arguments: initial USB U-state, RNG
/// seed and pacing. `start` takes no battery, NFC or RTC input yet, so those are written as the
/// board's reset state.
pub fn environment(args: &pemu_api::commands::start::StartArgs) -> String {
    format!(
        "usb={};seed={};mode={};battery=reset;nfc=reset;rtc=reset",
        args.usb.map_or("board-default", |usb| usb.as_str()),
        args.seed,
        args.mode.as_str(),
    )
}

/// The key parts of a session that has not run yet. The ROM, image, eFuse and configuration hashes
/// come from a snapshot header of the fresh machine; the snapshot format, core version and
/// [`BUILD_ID`] are parts of their own so a rebuild misses. The bootloader ELF is not loaded and
/// contributes nothing.
pub fn key_parts(
    args: &pemu_api::commands::start::StartArgs,
    session: &mut pemu_api::commands::start::Session,
    app_elf_sha: &[u8; 32],
) -> Result<KeyParts, pemu_api::error::ApiError> {
    use pemu_api::error::{ApiError, E_INTERNAL};
    let header = session
        .snapshot_machine()
        .snapshot_header(pemu_core::snap::SnapOpts::default())
        .map_err(|e| {
            ApiError::new(
                E_INTERNAL,
                format!("the fresh machine has no snapshot header: {e:?}"),
            )
        })?;
    let built = crate::backend::take_built(&args.fw).ok_or_else(|| {
        ApiError::new(
            E_INTERNAL,
            "the boot cache has no image measurement for this session; its machine was not built \
             by this host's factory on this thread",
        )
    })?;
    let mut bundle = built.image_sha.to_vec();
    bundle.extend_from_slice(app_elf_sha);
    let nvs = built.nvs_seed_sha;
    Ok(KeyParts {
        bundle_id: pemu_loader::hex(&pemu_loader::sha256(&bundle)),
        rom_sha: pemu_loader::hex(&header.rom_sha256),
        efuse_kind: format!("{:?}", header.efuse_kind).to_ascii_lowercase(),
        efuse_sha: pemu_loader::hex(&header.efuse_hash),
        config_hash: format!(
            "{}+core-{}",
            pemu_loader::hex(&header.config_hash),
            header.core_version
        ),
        format_version: header.format.to_string(),
        build_id: BUILD_ID.to_string(),
        settle_point: settle_point(
            args.boot_cache
                .unwrap_or(pemu_api::commands::start::BootCachePoint::UiSettled),
        ),
        profile: session.receipt().profile,
        nvs_seed_sha: pemu_loader::hex(&nvs),
        environment: environment(args),
    })
}

fn console_has(
    session: &mut pemu_api::commands::start::Session,
    from: u64,
    needle: &str,
) -> (bool, u64) {
    let ring = session
        .machine()
        .io()
        .serial_ring(pemu_core::hostio::SerialStream::UsjTx);
    let bytes: Vec<u8> = ring.slices(from).iter().copied().collect();
    let found = bytes
        .windows(needle.len())
        .any(|window| window == needle.as_bytes());
    // Resume a needle's length back, so a line split across two slices is still found.
    let next = ring.head().saturating_sub(needle.len() as u64).max(from);
    (found, next)
}

/// Runs a fresh session in fixed slices to the first LVGL safe point after `app_main` returned.
/// `Ok(false)` when `budget` ran out or the guest stopped.
fn settle(
    session: &mut pemu_api::commands::start::Session,
    budget: pemu_core::time::VTime,
) -> Result<bool, pemu_api::error::ApiError> {
    use pemu_core::time::VTime;
    use pemu_machine::stops::StopReason;
    let fw = session.fw.clone();
    let deadline = VTime(session.now().0.saturating_add(budget.0));
    let mut returned = false;
    let mut cursor = 0u64;
    while session.now() < deadline {
        if session.cancelled() {
            return Err(pemu_api::commands::start::cancelled(session.id));
        }
        let until = VTime(
            session
                .now()
                .0
                .saturating_add(SETTLE_SLICE.0)
                .min(deadline.0),
        );
        let outcome = session.run_until(until);
        if !returned {
            let (found, next) = console_has(session, cursor, APP_MAIN_RETURNED);
            returned = found;
            cursor = next;
        }
        if returned {
            let safe = crate::hooks::at_safe_point(&fw, session.machine())
                .map_err(|e| pemu_api::commands::inspect::walker_error("boot_cache", &e))?;
            if safe {
                return Ok(true);
            }
        }
        if !matches!(outcome.reason, StopReason::Until | StopReason::MaxInsns) {
            return Ok(false);
        }
    }
    Ok(false)
}

/// The `UiSettleHook` of this host: `boot: until_ui_settled` reaches the same instant a cold
/// `--boot-cache` start stores. `None` without an app ELF, so `start` waits for a `ui-settled`
/// event instead.
///
/// # Errors
///
/// The start was cancelled, or the safe-point walk could not read the guest.
pub fn ui_settle_hook(
    session: &mut pemu_api::commands::start::Session,
    budget: pemu_core::time::VTime,
) -> Result<Option<bool>, pemu_api::error::ApiError> {
    if crate::hooks::elf_of(&session.fw).is_none() {
        return Ok(None);
    }
    settle(session, budget).map(Some)
}

/// The `BootCacheHook` of this host. A hit restores the fresh session from the entry; a miss boots
/// cold to the settled point and stores it, replacing an entry that no longer restores.
pub fn start_hook(
    args: &pemu_api::commands::start::StartArgs,
    session: &mut pemu_api::commands::start::Session,
) -> Result<pemu_api::commands::start::CachedBoot, pemu_api::error::ApiError> {
    use pemu_api::commands::start::CachedBoot;
    use pemu_api::error::{ApiError, E_INTERNAL, E_STATE};
    let context = crate::hooks::elf_of(&session.fw).ok_or_else(|| {
        ApiError::new(
            E_STATE,
            format!(
                "`{}` has no app ELF this host can find, and the ui-settled point is an LVGL safe \
                 point read through its DWARF",
                session.fw
            ),
        )
        .with_hint("start a corpus id whose corpus.toml entry lists an `elf`, or drop `boot_cache`")
    })?;
    let key = key_parts(args, session, &context.elf.sha256)?.key();
    let (store, disk) = store_for_session(session)
        .map_err(|e| ApiError::new(E_STATE, format!("the boot cache is not usable: {e}")))?;
    let cache = disk.as_ref().unwrap_or_else(|| memory());
    let (entry, damaged) = match cache.get(&key) {
        Ok(entry) => (entry, false),
        Err(Error::Damaged(_)) => (None, true),
        Err(e) => {
            return Err(ApiError::new(
                E_STATE,
                format!("the boot cache is not readable: {e}"),
            ));
        }
    };
    let stale = damaged || entry.is_some();
    if let Some(bytes) = entry
        && let Ok(snapshot) = pemu_core::snap::Snapshot::from_bytes(&bytes)
        && session.snapshot_machine().restore(&snapshot).is_ok()
    {
        return Ok(CachedBoot {
            hit: true,
            key,
            store,
            settled: true,
        });
    }
    let settled = settle(session, args.boot_timeout)?;
    if !settled {
        return Ok(CachedBoot {
            hit: false,
            key,
            store: "none",
            settled,
        });
    }
    let bytes = session
        .snapshot_machine()
        .snapshot(pemu_core::snap::SnapOpts::default())
        .and_then(|snapshot| snapshot.to_bytes())
        .map_err(|e| {
            ApiError::new(
                E_INTERNAL,
                format!("the settled machine did not snapshot: {e:?}"),
            )
        })?;
    if stale {
        cache.replace(&key, &bytes)
    } else {
        cache.put(&key, &bytes)
    }
    .map_err(|e| ApiError::new(E_STATE, format!("the boot cache entry was not stored: {e}")))?;
    Ok(CachedBoot {
        hit: false,
        key,
        store,
        settled,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::platform::fake::FakeOwnerOnly;

    fn parts() -> KeyParts {
        KeyParts {
            bundle_id: "a".repeat(64),
            rom_sha: "b".repeat(64),
            efuse_kind: "synthesized".to_string(),
            efuse_sha: "c".repeat(64),
            config_hash: "d".repeat(64),
            profile: "default".to_string(),
            nvs_seed_sha: "e".repeat(64),
            environment: "usb=u0;battery=80;nfc=none;rtc=0".to_string(),
            format_version: "3".to_string(),
            build_id: "1".repeat(64),
            settle_point: "ui-settled".to_string(),
        }
    }

    /// Serializes the tests that install a cache role into the process-wide [`installed_dir`].
    pub(crate) fn store_gate() -> std::sync::MutexGuard<'static, ()> {
        static GATE: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        GATE.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// An eFuse dump directory as `--efuse-dump <dir>` reads it, synthesized. Taint follows the
    /// image's origin, never a word's value, so no real device dump (MAC, unique id) is needed.
    fn write_synthetic_efuse_dump(dir: &Path) {
        use pemu_loader::efuse_image::{BLOCK_WORDS, EfuseImage};
        let img = EfuseImage::synth(7);
        for (block, &words) in BLOCK_WORDS.iter().enumerate() {
            let bytes: Vec<u8> = img.words()[block][..words]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect();
            std::fs::write(dir.join(format!("efuse_blk{block}.bin")), bytes).expect("a block file");
        }
    }

    fn machine_on(efuse: pemu_loader::efuse_image::EfuseImage) -> pemu_machine::machine::Machine {
        use pemu_machine::config::{Assets, EfuseSource, MachineConfig};
        let cfg = pemu_machine::config::MachineConfig {
            efuse: if efuse.tainted() {
                EfuseSource::Dump
            } else {
                EfuseSource::Synth
            },
            ..MachineConfig::default()
        };
        let assets =
            Assets::with_bundled_rom(pemu_loader::bundle::FlashImage::erased(), None, None, efuse)
                .expect("the bundled ROM is pinned");
        pemu_machine::machine::Machine::new(cfg, assets).expect("the ROM fits the ROM window")
    }

    /// Attaches `machine` as a `start` would, reports its receipt's taint and the store
    /// [`store_for_session`] picks, writes `key` there, and stops the instance.
    fn taint_and_store_of(
        machine: pemu_machine::machine::Machine,
        key: &str,
    ) -> (bool, &'static str) {
        use pemu_api::commands::start::{StartArgs, with_pool};
        let args = StartArgs::default();
        let id = with_pool(|pool| pool.attach(&args, Box::new(machine)));
        let answer = with_pool(|pool| {
            let session = pool.session_mut(id).expect("the attached session");
            let tainted = session.receipt().tainted;
            let (store, disk) = store_for_session(session).expect("a store");
            let cache = disk.as_ref().unwrap_or_else(|| memory());
            cache.put(key, b"boot").expect("put");
            (tainted, store)
        });
        let _ = with_pool(|pool| pool.destroy(id));
        answer
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-boot-cache-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        dir
    }

    #[test]
    fn a_key_is_hex_and_holds_no_host_path() {
        let key = parts().key();
        assert!(key_is_valid(&key), "{key}");
        assert!(
            !key.contains(std::path::MAIN_SEPARATOR),
            "a key never carries a path"
        );
    }

    #[test]
    fn a_different_nvs_seed_misses_the_cache() {
        // A variant differing only in the NVS seed must not hit.
        let base = parts();
        let mut other = base.clone();
        other.nvs_seed_sha = "f".repeat(64);
        assert_ne!(base.key(), other.key());
    }

    #[test]
    fn every_part_of_the_key_changes_it_and_no_two_parts_can_be_confused() {
        let base = parts();
        let mut seen = std::collections::BTreeSet::from([base.key()]);
        type Change = Box<dyn Fn(&mut KeyParts)>;
        let variants: [Change; 11] = [
            Box::new(|p| p.bundle_id = "0".repeat(64)),
            Box::new(|p| p.rom_sha = "0".repeat(64)),
            Box::new(|p| p.efuse_kind = "device".to_string()),
            Box::new(|p| p.efuse_sha = "0".repeat(64)),
            Box::new(|p| p.config_hash = "0".repeat(64)),
            Box::new(|p| p.profile = "fast".to_string()),
            Box::new(|p| p.nvs_seed_sha = "0".repeat(64)),
            Box::new(|p| p.environment = "usb=u1".to_string()),
            Box::new(|p| p.format_version = "4".to_string()),
            Box::new(|p| p.build_id = "2".repeat(64)),
            Box::new(|p| p.settle_point = "ui-settled-2".to_string()),
        ];
        for change in &variants {
            let mut p = base.clone();
            change(&mut p);
            assert!(seen.insert(p.key()), "a part of the key did not change it");
        }

        let a = KeyParts {
            efuse_kind: "ab".to_string(),
            efuse_sha: "c".to_string(),
            ..KeyParts::default()
        };
        let b = KeyParts {
            efuse_kind: "a".to_string(),
            efuse_sha: "bc".to_string(),
            ..KeyParts::default()
        };
        assert_ne!(a.key(), b.key(), "the parts must not be confusable");
    }

    #[test]
    fn a_key_that_is_not_hex_is_refused_before_any_file_is_touched() {
        let cache = BootCache::in_memory();
        for bad in ["", "../escape", "A".repeat(64).as_str(), "abc"] {
            assert!(matches!(cache.get(bad), Err(Error::BadKey(_))), "{bad}");
            assert!(
                matches!(cache.put(bad, b"x"), Err(Error::BadKey(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn an_entry_round_trips_through_a_temporary_name_and_a_rename() {
        let dir = temp_dir("roundtrip");
        let guard = FakeOwnerOnly::new();
        let cache = BootCache::on_disk(&dir, OwnerOnlyFiles::new(&guard)).expect("create");
        assert!(cache.is_on_disk());
        let key = parts().key();
        assert_eq!(cache.get(&key).expect("miss"), None);
        assert!(cache.put(&key, b"snapshot bytes").expect("put"));
        assert_eq!(
            cache.get(&key).expect("hit"),
            Some(b"snapshot bytes".to_vec())
        );
        let names: Vec<String> = std::fs::read_dir(dir.join(CACHE_DIR))
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, std::slice::from_ref(&key));
        assert!(!names.iter().any(|n| n.contains(".tmp.")));
        assert_eq!(cache.keys().expect("keys"), std::slice::from_ref(&key));
        assert!(guard.checked(&dir.join(CACHE_DIR).join(&key)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_temporary_name_carries_this_process_id_and_a_counter_and_is_created_new() {
        let dir = temp_dir("tempname");
        let guard = FakeOwnerOnly::new();
        let cache = BootCache::on_disk(&dir, OwnerOnlyFiles::new(&guard)).expect("create");
        let key = parts().key();
        cache.replace(&key, b"x").expect("put");
        cache.replace(&key, b"y").expect("put again");
        let wrote: Vec<String> = guard
            .calls()
            .iter()
            .filter_map(|call| match call {
                crate::platform::fake::OwnerOnlyCall::CreateNewFile(path) => {
                    Some(path.file_name()?.to_string_lossy().into_owned())
                }
                crate::platform::fake::OwnerOnlyCall::CreateFile(path) => {
                    panic!(
                        "a temporary file is never truncated open: {}",
                        path.display()
                    )
                }
                _ => None,
            })
            .collect();
        let prefix = format!("{key}.tmp.{}.", std::process::id());
        assert_eq!(wrote.len(), 2);
        assert!(
            wrote.iter().all(|name| name.starts_with(&prefix)),
            "{wrote:?}"
        );
        assert_ne!(
            wrote[0], wrote[1],
            "each publish has its own temporary name"
        );

        let taken = dir.join(CACHE_DIR).join(format!("{key}.tmp.taken"));
        std::fs::write(&taken, b"other").expect("a file");
        assert!(
            OwnerOnlyFiles::new(&guard)
                .write_new(&taken, b"mine")
                .is_err()
        );
        assert_eq!(std::fs::read(&taken).expect("read"), b"other");

        std::thread::scope(|scope| {
            for n in 0..8u8 {
                let (cache, key) = (&cache, &key);
                scope.spawn(move || cache.replace(key, &[n; 4096]).expect("publishes"));
            }
        });
        let entry = cache.get(&key).expect("a whole entry").expect("present");
        assert!(entry.len() == 4096 && entry.iter().all(|b| *b == entry[0]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_entry_whose_digest_does_not_match_is_damaged_never_restored() {
        let dir = temp_dir("digest");
        let guard = FakeOwnerOnly::new();
        let cache = BootCache::on_disk(&dir, OwnerOnlyFiles::new(&guard)).expect("create");
        let key = parts().key();
        cache.put(&key, b"snapshot bytes").expect("put");
        let path = dir.join(CACHE_DIR).join(&key);
        let mut file = std::fs::read(&path).expect("read");
        assert!(file.starts_with(&ENTRY_MAGIC));
        let last = file.len() - 1;
        file[last] ^= 1;
        std::fs::write(&path, &file).expect("damage it");
        assert!(matches!(cache.get(&key), Err(Error::Damaged(k)) if k == key));
        std::fs::write(&path, b"snapshot bytes").expect("an unframed file");
        assert!(matches!(cache.get(&key), Err(Error::Damaged(_))));
        cache.replace(&key, b"snapshot bytes").expect("replaced");
        assert_eq!(
            cache.get(&key).expect("whole"),
            Some(b"snapshot bytes".to_vec())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn another_emulator_build_or_snapshot_format_gives_another_key() {
        assert_eq!(BUILD_ID.len(), 64, "a SHA-256 in hex: {BUILD_ID}");
        assert!(key_is_valid(BUILD_ID));
        let this = parts();
        let other = KeyParts {
            build_id: "0".repeat(64),
            ..this.clone()
        };
        assert_ne!(this.key(), other.key());
        let older = KeyParts {
            format_version: "2".to_string(),
            ..this.clone()
        };
        assert_ne!(this.key(), older.key());
        assert!(
            settle_point(pemu_api::commands::start::BootCachePoint::UiSettled)
                .contains(APP_MAIN_RETURNED)
        );
    }

    #[test]
    fn the_build_id_moves_with_any_crate_source_board_or_manifest_and_not_with_tests() {
        use crate::build_id::build_id;
        let ws = temp_dir("buildid");
        let write = |path: &str, text: &str| {
            let file = ws.join(path);
            std::fs::create_dir_all(file.parent().expect("a parent")).expect("mkdir");
            std::fs::write(file, text).expect("write");
        };
        write("Cargo.toml", "[workspace]\n");
        write("crates/pemu-api/src/lib.rs", "pub fn a() {}\n");
        write("crates/pemu-api/tests/t.rs", "#[test] fn t() {}\n");
        write("boards/passport.toml", "lcd = 1\n");
        let id = |ws: &std::path::Path| build_id(ws, pemu_loader::sha256);
        let base = id(&ws);
        write("crates/pemu-api/tests/t.rs", "#[test] fn u() {}\n");
        assert_eq!(id(&ws), base, "a test tree is not part of the build");
        let mut seen = std::collections::BTreeSet::from([base]);
        for (path, text) in [
            ("crates/pemu-api/src/lib.rs", "pub fn b() {}\n"),
            ("boards/passport.toml", "lcd = 2\n"),
            ("Cargo.toml", "[workspace]\n[profile.release]\n"),
            ("crates/pemu-host/src/backend.rs", "\n"),
        ] {
            write(path, text);
            assert!(
                seen.insert(id(&ws)),
                "editing {path} did not change the build id"
            );
        }
        std::fs::remove_dir_all(&ws).ok();

        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("the workspace");
        assert_eq!(BUILD_ID, build_id(workspace, pemu_loader::sha256));
    }

    #[test]
    fn a_corrupt_entry_is_replaced_atomically_and_put_still_keeps_the_first_writer() {
        let dir = temp_dir("replace");
        let guard = FakeOwnerOnly::new();
        let cache = BootCache::on_disk(&dir, OwnerOnlyFiles::new(&guard)).expect("create");
        let key = parts().key();
        assert!(cache.put(&key, b"corrupt").expect("put"));
        assert!(!cache.put(&key, b"good").expect("put again"));
        assert_eq!(cache.get(&key).expect("read"), Some(b"corrupt".to_vec()));
        assert!(cache.replace(&key, b"good").expect("replace"));
        assert_eq!(cache.get(&key).expect("read"), Some(b"good".to_vec()));
        let names: Vec<String> = std::fs::read_dir(dir.join(CACHE_DIR))
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            std::slice::from_ref(&key),
            "no temporary file is left"
        );

        let memory = BootCache::in_memory();
        assert!(memory.put(&key, b"corrupt").expect("put"));
        assert!(!memory.put(&key, b"good").expect("put again"));
        assert!(memory.replace(&key, b"good").expect("replace"));
        assert_eq!(memory.get(&key).expect("read"), Some(b"good".to_vec()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_memory_store_is_bounded_and_the_environment_follows_the_arguments() {
        let memory = BootCache::in_memory();
        let key = |n: usize| format!("{n:064x}");
        for n in 0..MEMORY_ENTRIES + 3 {
            assert!(memory.put(&key(n), &[n as u8]).expect("put"));
        }
        let keys = memory.keys().expect("keys");
        assert_eq!(keys.len(), MEMORY_ENTRIES);
        assert!(!keys.contains(&key(0)) && keys.contains(&key(MEMORY_ENTRIES + 2)));

        use pemu_api::commands::start::StartArgs;
        let base = StartArgs::default();
        let seeded = StartArgs {
            seed: base.seed + 1,
            ..StartArgs::default()
        };
        let usb = StartArgs {
            usb: Some(pemu_api::commands::env::UsbWorld::Unplugged),
            ..StartArgs::default()
        };
        assert_ne!(environment(&base), environment(&seeded));
        assert_ne!(environment(&base), environment(&usb));
        assert!(
            environment(&usb).starts_with("usb=unplugged;"),
            "{}",
            environment(&usb)
        );
    }

    #[test]
    fn the_first_writer_wins_and_the_second_is_not_an_error() {
        let dir = temp_dir("first");
        let guard = FakeOwnerOnly::new();
        let cache = BootCache::on_disk(&dir, OwnerOnlyFiles::new(&guard)).expect("create");
        let key = parts().key();
        assert!(cache.put(&key, b"first").expect("put"));
        assert!(
            !cache.put(&key, b"second").expect("put again"),
            "the second writer reports that it did not publish"
        );
        assert_eq!(cache.get(&key).expect("hit"), Some(b"first".to_vec()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_tainted_machine_keeps_its_entries_in_memory_only() {
        let cache = BootCache::in_memory();
        assert!(!cache.is_on_disk());
        let key = parts().key();
        assert!(cache.put(&key, b"tainted boot").expect("put"));
        assert_eq!(
            cache.get(&key).expect("hit"),
            Some(b"tainted boot".to_vec())
        );
        assert_eq!(cache.keys().expect("keys"), [key]);
    }

    #[test]
    fn the_rename_retry_is_bounded_and_reports_giving_up() {
        let dir = temp_dir("rename");
        let missing = dir.join("never-written");
        let target = dir.join("target");
        let started = std::time::Instant::now();
        let err = rename_with_retry(&missing, &target, 3).expect_err("nothing to rename");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the retry is bounded, not a hang"
        );
        assert!(!target.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Asserts on the filesystem, not on `store_for`: `store_for(true)` answers `memory` whatever
    /// the receipt says, so it cannot catch a receipt that drops the taint.
    #[test]
    fn a_tainted_sessions_entry_never_appears_under_the_cache_role() {
        use pemu_loader::efuse_image::EfuseImage;
        let _gate = store_gate();
        let role = temp_dir("tainted-session");
        let dump = temp_dir("tainted-session-dump");
        write_synthetic_efuse_dump(&dump);
        *installed_dir().lock().unwrap_or_else(|e| e.into_inner()) = Some(role.clone());

        let imported = crate::assets::load_efuse_dump(&dump).expect("the synthetic dump imports");
        assert!(imported.tainted(), "an imported dump taints");
        let tainted_key = KeyParts {
            bundle_id: "f".repeat(64),
            ..parts()
        }
        .key();
        assert_eq!(
            taint_and_store_of(machine_on(imported), &tainted_key),
            (true, "memory"),
            "the receipt of an eFuse-dump machine says `tainted`, so its entries stay in memory"
        );
        assert!(
            !role.join(CACHE_DIR).join(&tainted_key).exists(),
            "a tainted machine's entry reached the cache role on disk"
        );

        // The control: an untainted machine writes its entry, so the assertion above is about
        // taint and not a cache that never writes.
        let clean_key = parts().key();
        assert_eq!(
            taint_and_store_of(machine_on(EfuseImage::synth(7)), &clean_key),
            (false, "disk"),
            "a synthesized eFuse is no device's, so the entry goes to the cache role"
        );
        assert!(
            role.join(CACHE_DIR).join(&clean_key).is_file(),
            "an untainted entry is on disk under its key"
        );

        *installed_dir().lock().unwrap_or_else(|e| e.into_inner()) = None;
        std::fs::remove_dir_all(&role).ok();
        std::fs::remove_dir_all(&dump).ok();
    }

    #[test]
    fn a_tainted_machine_is_given_the_memory_store_even_with_a_cache_role() {
        let _gate = store_gate();
        let dir = temp_dir("store-for");
        *installed_dir().lock().expect("lock") = Some(dir.clone());
        let (tainted, none) = store_for(true).expect("memory");
        assert_eq!((tainted, none.is_none()), ("memory", true));
        let (clean, disk) = store_for(false).expect("disk");
        assert_eq!(clean, "disk");
        assert!(disk.expect("an on-disk cache").is_on_disk());
        *installed_dir().lock().expect("lock") = None;
        assert_eq!(store_for(false).expect("memory").0, "memory");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_nvs_seed_hash_covers_the_nvs_partitions_and_nothing_else() {
        use crate::backend::nvs_seed_sha;
        let mut flash = vec![0xFFu8; 0x10000];
        // One NVS data partition at 0x9000, 0x1000 long.
        let mut row = vec![0xAA, 0x50, 0x01, 0x02];
        row.extend_from_slice(&0x9000u32.to_le_bytes());
        row.extend_from_slice(&0x1000u32.to_le_bytes());
        row.extend_from_slice(b"nvs\0\0\0\0\0\0\0\0\0\0\0\0\0");
        row.extend_from_slice(&[0; 4]);
        flash[0x8000..0x8000 + row.len()].copy_from_slice(&row);
        let base = nvs_seed_sha(&flash);
        let mut other_nvs = flash.clone();
        other_nvs[0x9010] = 0x00;
        assert_ne!(
            nvs_seed_sha(&other_nvs),
            base,
            "a different NVS seed misses"
        );
        let mut outside = flash.clone();
        outside[0xC000] = 0x00;
        assert_eq!(
            nvs_seed_sha(&outside),
            base,
            "bytes outside NVS are the bundle id's"
        );

        use crate::backend::image_sha_without_nvs;
        assert_eq!(
            image_sha_without_nvs(&other_nvs),
            image_sha_without_nvs(&flash)
        );
        assert_ne!(
            image_sha_without_nvs(&outside),
            image_sha_without_nvs(&flash)
        );
    }
}
