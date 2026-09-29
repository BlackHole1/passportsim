//! Local paths, secret set derivation, `--init` and `--self-test` of `cargo xtask
//! secrets-check`.
//!
//! Every function takes an [`Env`]; only [`Env::from_process`] reads the host, so tests point an
//! `Env` at temporary directories, never at real device data.
//!
//! Inputs, which are counted and never printed:
//! - `$ROOT/device/efuse_blk0.bin` to `efuse_blk10.bin` (espefuse dumps, little-endian 32-bit
//!   words): BLK1 yields the MAC members, BLK2 the unique ID and calibration words, and the
//!   other blocks are length-checked by `pemu-api::secret_set`;
//! - `<home>/esp/passport-backups/*.bin` (full flash backups): each file stem, and the non-0xFF
//!   content of the cardid window `[0x356000, 0x35A000)` of each backup long enough to hold it.
//!
//! The guard runs on whichever host holds `$ROOT/device/`, so every path comes from the
//! directory roles of the running host (`hostdirs.rs`):
//!
//! | Path | macOS | Windows |
//! |---|---|---|
//! | hash file | `~/.config/passportsim/secrets-check.toml` | `%APPDATA%\passportsim\secrets-check.toml` |
//! | `$ROOT` (no config key) | `~/Library/Application Support/passportsim` | `%LOCALAPPDATA%\passportsim\data` |
//! | backups | `~/esp/passport-backups` | `%USERPROFILE%\esp\passport-backups` |
//!
//! `$ROOT` is `[paths] data_root` of the config directory's `config.toml` when that key is set.
//! The Windows folders are the known folders (`FOLDERID_RoamingAppData`,
//! `FOLDERID_LocalAppData`, `FOLDERID_Profile`), never the environment variables of the same
//! name.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::PathBuf;

use pemu_api::secret_set::{self, MemberKind, SecretSet, SecretSetBuilder};

use super::{hashed, patterns};
use crate::hostdirs::{self, Base, HostDirs, Overrides};

/// Number of eFuse blocks of the ESP32-C3 (BLK0 to BLK10).
pub const EFUSE_BLOCKS: u8 = 11;
/// Local flash backup directory below the home directory, on both hosts.
const BACKUP_DIR: [&str; 2] = ["esp", "passport-backups"];
/// The hash file's name in the config directory.
const HASH_FILE: &str = "secrets-check.toml";

/// Where the guard looks for its inputs and its hash file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env {
    /// `<config>/secrets-check.toml`.
    pub hash_file: PathBuf,
    /// `$ROOT/device`.
    pub device_dir: PathBuf,
    /// `<home>/esp/passport-backups`.
    pub backups_dir: PathBuf,
}

impl Env {
    /// The paths of the current user, from the host base (`HOME` on macOS, the known folders on
    /// Windows). Test builds never call it (see `secrets::process_env`).
    ///
    /// The directory-role overrides are **not** honored here: `PASSPORTSIM_HOME` pointing at an
    /// empty directory would hide the device directory and turn the fail-closed rule into an
    /// environment variable. A run that really wants other paths passes `--root`.
    #[cfg_attr(test, allow(dead_code))]
    pub fn from_process() -> Result<Env, String> {
        if let Some(name) = overridden() {
            return Err(format!(
                "{name} is set; the secret guard resolves its paths from HOME only. \
                 Unset it, or pass --root for a scan of another tree"
            ));
        }
        Env::from_base(&hostdirs::host_base()?)
    }

    /// The paths of one host base, with no override: the config directory holds the hash file,
    /// `$ROOT` is `[paths] data_root` of its `config.toml` or the data-root role, and the
    /// backups sit below the home role.
    pub fn from_base(base: &Base) -> Result<Env, String> {
        let dirs = HostDirs::new(Ok(base.clone()), Overrides::default());
        let config = hostdirs::read_config(&dirs)?;
        let home = dirs.home()?;
        Ok(Env {
            hash_file: dirs.config()?.join(HASH_FILE),
            device_dir: hostdirs::data_root_of(&dirs, config.as_deref())?.join("device"),
            backups_dir: home.join(BACKUP_DIR[0]).join(BACKUP_DIR[1]),
        })
    }

    /// The paths below a macOS `home`.
    #[cfg(test)]
    pub fn from_home(home: &std::path::Path) -> Result<Env, String> {
        Env::from_base(&Base::Home(home.to_path_buf()))
    }

    /// The paths of the Windows column: the config directory roams (`FOLDERID_RoamingAppData`),
    /// the data root does not (`FOLDERID_LocalAppData`), and the backups sit below the profile
    /// (`FOLDERID_Profile`), where `~\` in the config also expands.
    #[cfg(test)]
    pub fn from_folders(folders: &hostdirs::KnownFolders) -> Result<Env, String> {
        Env::from_base(&Base::Windows(folders.clone()))
    }

    /// Whether the device directory exists, which makes a missing hash file refuse the hooks.
    pub fn has_device_dir(&self) -> bool {
        self.device_dir.exists()
    }
}

/// Directory-role overrides, which the guard refuses to run under.
const OVERRIDES: &[&str] = &[
    "PASSPORTSIM_HOME",
    "PASSPORTSIM_CONFIG_DIR",
    "PASSPORTSIM_DATA_ROOT",
];

/// The first override that is set and not empty.
fn overridden() -> Option<&'static str> {
    OVERRIDES
        .iter()
        .copied()
        .find(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// What one derivation read: counts only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sources {
    /// eFuse block files read.
    pub efuse_blocks: usize,
    /// Backup files whose stem was added.
    pub backups: usize,
    /// Backups whose cardid window was read.
    pub cardid_windows: usize,
    /// `*.bin` entries of the backup directory that were not regular files or had a
    /// non-UTF-8 name.
    pub backups_skipped: usize,
}

/// Builds the secret set of this host: eFuse members, backup stems, cardid windows and
/// `canary`. Fails when the device directory is missing or holds no eFuse block, and on any
/// read error, so a partial set is never hashed.
pub fn derive(env: &Env, canary: &str) -> Result<(SecretSet, Sources), String> {
    let mut builder = SecretSetBuilder::new();
    let mut sources = Sources::default();
    match std::fs::metadata(&env.device_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err("the device path is not a directory".to_string()),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(format!(
                "no device directory at {}; nothing to hash",
                env.device_dir.display()
            ));
        }
        Err(err) => {
            return Err(format!(
                "cannot inspect the device directory: {}",
                err.kind()
            ));
        }
    }
    for index in 0..EFUSE_BLOCKS {
        let path = env.device_dir.join(format!("efuse_blk{index}.bin"));
        match std::fs::read(&path) {
            Ok(bytes) => {
                builder
                    .efuse_block(index, &bytes)
                    .map_err(|err| err.to_string())?;
                sources.efuse_blocks += 1;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(format!("cannot read eFuse block {index}: {}", err.kind())),
        }
    }
    if sources.efuse_blocks == 0 {
        return Err("the device directory holds no efuse_blk<N>.bin file".to_string());
    }
    add_backups(env, &mut builder, &mut sources)?;
    builder.canary(canary.as_bytes());
    Ok((builder.build(), sources))
}

/// Adds the stem and cardid window of every `*.bin` file of the backup directory, in name
/// order. A missing directory adds nothing. Errors number the backup and never name it.
fn add_backups(
    env: &Env,
    builder: &mut SecretSetBuilder,
    sources: &mut Sources,
) -> Result<(), String> {
    let listing = match std::fs::read_dir(&env.backups_dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("cannot list the backup directory: {}", err.kind())),
    };
    let mut paths = Vec::new();
    for entry in listing {
        let entry =
            entry.map_err(|err| format!("cannot list the backup directory: {}", err.kind()))?;
        paths.push(entry.path());
    }
    paths.sort();
    let window_range = patterns::CARDID_WINDOW;
    for (number, path) in paths.iter().enumerate().map(|(i, p)| (i + 1, p)) {
        let Some(name) = path.file_name() else {
            continue;
        };
        if !name
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".bin")
        {
            continue;
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                sources.backups_skipped += 1;
                continue;
            }
            Err(err) => {
                return Err(format!(
                    "cannot inspect backup entry {number}: {}",
                    err.kind()
                ));
            }
        };
        let (true, Some(name)) = (meta.is_file(), name.to_str()) else {
            sources.backups_skipped += 1;
            continue;
        };
        builder.backup_stem(&name[..name.len() - ".bin".len()]);
        sources.backups += 1;
        if meta.len() < window_range.end as u64 {
            continue;
        }
        let mut window = vec![0u8; window_range.len()];
        File::open(path)
            .and_then(|mut file| {
                file.seek(SeekFrom::Start(window_range.start as u64))?;
                file.read_exact(&mut window)
            })
            .map_err(|err| format!("cannot read backup entry {number}: {}", err.kind()))?;
        builder.cardid_window(&window);
        sources.cardid_windows += 1;
    }
    Ok(())
}

/// `mac 12, unique_id 6` style list of a count map.
fn kinds_summary(counts: &std::collections::BTreeMap<MemberKind, usize>) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    counts
        .iter()
        .map(|(kind, n)| format!("{} {n}", kind.name()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sources_line(mode: &str, sources: &Sources) -> String {
    format!(
        "secrets-check {mode}: read {} of {EFUSE_BLOCKS} eFuse block file(s), {} backup(s) ({} cardid window(s), {} skipped)\n",
        sources.efuse_blocks, sources.backups, sources.cardid_windows, sources.backups_skipped
    )
}

/// `--init`: derives the set, hashes it with a fresh salt and canary, writes the hash file
/// (owner-only) and checks that the written file detects every member. Returns the
/// printed summary, which holds counts only.
pub fn init(env: &Env) -> Result<String, String> {
    let canary = hashed::new_canary()?;
    let (set, sources) = derive(env, &canary)?;
    let salt = hashed::new_salt()?;
    let salted = set.salted(&salt).map_err(|err| err.to_string())?;
    let replaced = env.hash_file.exists();
    hashed::write(&env.hash_file, &hashed::render(&salted, &canary))?;
    let loaded = hashed::load(&env.hash_file).map_err(|err| err.to_string())?;
    let check = secret_set::self_test(&set, &loaded.salted);
    let mut out = sources_line("--init", &sources);
    let _ = writeln!(
        out,
        "secrets-check --init: {} member(s): {}; {} form(s) dropped by the false-positive guards; {} cardid chunk(s) skipped as NVS page headers; {} window length(s)",
        set.len(),
        kinds_summary(&set.count_by_kind()),
        set.dropped(),
        set.nvs_headers_skipped(),
        salted.window_lens().len()
    );
    let _ = writeln!(
        out,
        "secrets-check --init: {} {} (owner-only, new salt and canary); {} of {} member(s) detected through it",
        if replaced { "replaced" } else { "wrote" },
        env.hash_file.display(),
        check.detected,
        check.members
    );
    if !check.passed() {
        return Err(format!(
            "{out}the written hash file misses member(s): {}",
            kinds_summary(&check.missed)
        ));
    }
    out.push_str(
        "secrets-check --init: next run `cargo xtask hooks install` and `cargo xtask secrets-check --self-test`\n",
    );
    Ok(out)
}

/// `--self-test`: re-derives the set in memory with the stored canary, renders every member
/// form and checks that the hash file detects each. Counts only.
pub fn self_test(env: &Env) -> Result<String, String> {
    let file = hashed::load(&env.hash_file)
        .map_err(|err| format!("{err}; run cargo xtask secrets-check --init"))?;
    let (set, sources) = derive(env, &file.canary)?;
    let check = secret_set::self_test(&set, &file.salted);
    let mut probe = hashed::HashedReport::default();
    let line = format!("scratch: {}\n", file.canary);
    probe.scan_bytes(&file.salted, "<canary probe>", line.as_bytes());
    let canary_found = probe.hits.iter().any(|hit| hit.kind == MemberKind::Canary);
    let mut out = sources_line("--self-test", &sources);
    let _ = writeln!(
        out,
        "secrets-check --self-test: {} of {} rendered member form(s) detected ({}); canary probe {}; hash file entries {}",
        check.detected,
        check.members,
        kinds_summary(&set.count_by_kind()),
        if canary_found { "detected" } else { "missed" },
        file.salted.len()
    );
    if file.salted.len() != set.len() {
        let _ = writeln!(
            out,
            "secrets-check --self-test: note: the hash file and the re-derived set differ in size; device data changed since --init"
        );
    }
    if check.passed() && canary_found {
        out.push_str("secrets-check --self-test: passed\n");
        Ok(out)
    } else {
        Err(format!(
            "{out}self-test failed: missed {}; run cargo xtask secrets-check --init",
            kinds_summary(&check.missed)
        ))
    }
}
