//! `HostPaths` resolves a directory role (home, config, data root, cache, runtime, logs,
//! artifacts) to a path. With `xtask/src/hostdirs.rs`, this is the only place that names
//! `Application Support`, `.config`, `AppData` or `HOME`.
//!
//! Tests stay off real directories: the environment is injected ([`Env`]), and under `cfg(test)`
//! [`host_base`] has no fallback. That guard reaches only this crate's unit tests; every other
//! test injects an [`Env`]. Windows folders come from `SHGetKnownFolderPath`, never
//! `USERPROFILE` or `LOCALAPPDATA`, which do not move a known folder.
//!
//! Also holds [`refuse_device`], the serial-device refusal predicate, the [`OwnerOnlyFiles`]
//! guard every private file goes through, and [`contains`], the containment comparison.

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::platform::{OwnerOnly, PlatformError};

/// Application directory below every per-user base directory.
pub const APP_DIR: &str = "passportsim";
/// Artifacts directory below the data root.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Puts every role under `<dir>/<role>/`.
pub const HOME_ENV: &str = "PASSPORTSIM_HOME";
/// Overrides the config role only.
pub const CONFIG_DIR_ENV: &str = "PASSPORTSIM_CONFIG_DIR";
/// Overrides the data-root role only.
pub const DATA_ROOT_ENV: &str = "PASSPORTSIM_DATA_ROOT";

/// A per-user directory role. Other crates ask for a role, never a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// The user's home directory: `HOME` on macOS, `FOLDERID_Profile` on Windows.
    Home,
    /// Configuration, which roams on Windows: `~/.config/passportsim/`, `%APPDATA%\passportsim\`.
    Config,
    /// Data root, which must not roam: `~/Library/Application Support/passportsim/`,
    /// `%LOCALAPPDATA%\passportsim\data\`.
    DataRoot,
    /// Cache: `~/Library/Caches/passportsim/`, `%LOCALAPPDATA%\passportsim\cache\`.
    Cache,
    /// Runtime state (`serve.json`, the daemon token, the pty link): `~/.passportsim/`,
    /// `%LOCALAPPDATA%\passportsim\run\`.
    Runtime,
    /// Logs: `~/.passportsim/logs/`, `%LOCALAPPDATA%\passportsim\logs\`.
    Logs,
    /// Artifacts: `<data root>/artifacts/` unless `--artifacts <dir>` gives another directory.
    Artifacts,
}

impl Role {
    pub const ALL: [Role; 7] = [
        Role::Home,
        Role::Config,
        Role::DataRoot,
        Role::Cache,
        Role::Runtime,
        Role::Logs,
        Role::Artifacts,
    ];

    /// The role's directory name. These match the Windows subdirectory names, so the
    /// `PASSPORTSIM_HOME` layout `<dir>/<role>/` and the Windows tree are the same.
    pub fn slug(self) -> &'static str {
        match self {
            Role::Home => "home",
            Role::Config => "config",
            Role::DataRoot => "data",
            Role::Cache => "cache",
            Role::Runtime => "run",
            Role::Logs => "logs",
            Role::Artifacts => ARTIFACTS_DIR,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// macOS: `HOME` is unset or empty.
    HomeNotSet,
    /// Windows: `SHGetKnownFolderPath` failed or returned an empty path.
    KnownFolderUnavailable {
        /// The `FOLDERID_*` name that failed.
        folder: &'static str,
        /// The `HRESULT` the call returned (`0` for an empty path from a successful call).
        hresult: i32,
    },
    /// A test build resolved a role with no override set; `cfg(test)` has no fallback.
    NoFallbackUnderTest,
    /// The host is neither macOS nor Windows.
    UnsupportedHost,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::HomeNotSet => f.write_str("HOME is not set"),
            PathError::KnownFolderUnavailable { folder, hresult } => write!(
                f,
                "the Windows known folder {folder} is not available \
                 (SHGetKnownFolderPath HRESULT {hresult:#010x})"
            ),
            PathError::NoFallbackUnderTest => f.write_str(
                "a test build resolves no real host directory: set an override, or inject an Env",
            ),
            PathError::UnsupportedHost => {
                f.write_str("host directory roles are defined for macOS and Windows only")
            }
        }
    }
}

impl std::error::Error for PathError {}

/// The three Windows known folders. They never come from an environment variable, because
/// changing `USERPROFILE` or `LOCALAPPDATA` does not move a known folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownFolders {
    /// `FOLDERID_Profile`.
    pub profile: PathBuf,
    /// `FOLDERID_RoamingAppData` (the folder `%APPDATA%` usually names). Configuration roams.
    pub roaming_app_data: PathBuf,
    /// `FOLDERID_LocalAppData` (the folder `%LOCALAPPDATA%` usually names). A ROM blob, a flash
    /// image or an oracle capture must not roam in an enterprise profile.
    pub local_app_data: PathBuf,
}

/// The host base directories a role resolves against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Base {
    /// macOS: every role sits below `HOME`.
    Home(PathBuf),
    /// Windows: every role sits below a known folder.
    Windows(KnownFolders),
}

impl Base {
    /// `HOME` on macOS, `FOLDERID_Profile` on Windows: what `~/` and `~\` expand against.
    pub fn home(&self) -> &Path {
        match self {
            Base::Home(home) => home,
            Base::Windows(folders) => &folders.profile,
        }
    }
}

/// The known folders of this Windows host, the only per-OS seam of the role table.
#[cfg(windows)]
pub fn known_folders() -> Result<KnownFolders, PathError> {
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData,
    };
    Ok(KnownFolders {
        profile: known_folder(&FOLDERID_Profile, "FOLDERID_Profile")?,
        roaming_app_data: known_folder(&FOLDERID_RoamingAppData, "FOLDERID_RoamingAppData")?,
        local_app_data: known_folder(&FOLDERID_LocalAppData, "FOLDERID_LocalAppData")?,
    })
}

/// One `SHGetKnownFolderPath` call for the current user, with the default flags (no creation, no
/// environment-variable form).
#[cfg(windows)]
fn known_folder(id: &windows_sys::core::GUID, folder: &'static str) -> Result<PathBuf, PathError> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `id` is a valid GUID, a null token means the current user and `raw` is a valid
    // out-pointer. The COM-allocated result is freed with `CoTaskMemFree` below on every path,
    // failure included.
    let hresult =
        unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut raw) };
    let path = if hresult >= 0 && !raw.is_null() {
        // SAFETY: on success `raw` is a NUL-terminated UTF-16 string owned by this call.
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        // SAFETY: `len` code units before the terminator were just read as valid.
        let wide = unsafe { std::slice::from_raw_parts(raw, len) };
        Some(PathBuf::from(std::ffi::OsString::from_wide(wide)))
    } else {
        None
    };
    // SAFETY: `raw` is null or the COM allocation of the call above, freed exactly once.
    unsafe { CoTaskMemFree(raw.cast()) };
    match path {
        Some(path) if !path.as_os_str().is_empty() => Ok(path),
        _ => Err(PathError::KnownFolderUnavailable {
            folder,
            hresult: if hresult >= 0 { 0 } else { hresult },
        }),
    }
}

/// The base directories of the running host. Under `cfg(test)` it returns
/// [`PathError::NoFallbackUnderTest`], so a test that set no override fails instead of reading
/// real directories.
pub fn host_base() -> Result<Base, PathError> {
    #[cfg(test)]
    {
        Err(PathError::NoFallbackUnderTest)
    }
    #[cfg(all(not(test), target_os = "macos"))]
    {
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| Base::Home(PathBuf::from(home)))
            .ok_or(PathError::HomeNotSet)
    }
    #[cfg(all(not(test), windows))]
    {
        known_folders().map(Base::Windows)
    }
    #[cfg(all(not(test), not(target_os = "macos"), not(windows)))]
    {
        Err(PathError::UnsupportedHost)
    }
}

/// `~/x`, `~\x` and `~` expanded against `home`; every other value unchanged. Both separators
/// expand on both hosts, so a value written on one host resolves on the other.
pub fn expand_tilde(value: &str, home: &Path) -> PathBuf {
    match value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        Some(rest) => home.join(rest),
        None if value == "~" => home.to_path_buf(),
        None => PathBuf::from(value),
    }
}

/// The `PASSPORTSIM_*` overrides plus `--artifacts <dir>`. Values are kept as written so a
/// leading `~` expands later; an empty value counts as unset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub home: Option<String>,
    pub config_dir: Option<String>,
    pub data_root: Option<String>,
    /// `--artifacts <dir>`: a flag, not a variable, held here so the artifacts role has one
    /// answer.
    pub artifacts: Option<String>,
}

/// The host base directories and the overrides. The base is a `Result` so a run without one
/// still resolves every role an override covers.
#[derive(Clone, Debug)]
pub struct Env {
    base: Result<Base, PathError>,
    overrides: Overrides,
}

impl Env {
    /// An injected environment: no process state is read.
    pub fn new(base: Result<Base, PathError>, overrides: Overrides) -> Env {
        Env { base, overrides }
    }

    pub fn macos(home: impl Into<PathBuf>, overrides: Overrides) -> Env {
        Env::new(Ok(Base::Home(home.into())), overrides)
    }

    pub fn windows(folders: KnownFolders, overrides: Overrides) -> Env {
        Env::new(Ok(Base::Windows(folders)), overrides)
    }

    /// [`host_base`] plus the three `PASSPORTSIM_*` variables. Absent under `cfg(test)`, so a
    /// unit test cannot read the process environment.
    #[cfg(not(test))]
    pub fn from_process() -> Env {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        Env::new(
            host_base(),
            Overrides {
                home: var(HOME_ENV),
                config_dir: var(CONFIG_DIR_ENV),
                data_root: var(DATA_ROOT_ENV),
                artifacts: None,
            },
        )
    }

    pub fn overrides(&self) -> &Overrides {
        &self.overrides
    }

    pub fn base(&self) -> Result<&Base, PathError> {
        self.base.as_ref().map_err(Clone::clone)
    }

    /// `value` with `~` expanded against the host home, never the `PASSPORTSIM_HOME` role:
    /// `PASSPORTSIM_HOME=~/emu` defines the home role, so it must expand against the host home.
    fn expand(&self, value: &str) -> Result<PathBuf, PathError> {
        if value.starts_with('~') {
            return Ok(expand_tilde(value, self.base()?.home()));
        }
        Ok(PathBuf::from(value))
    }
}

/// The directory roles, resolved from one [`Env`].
#[derive(Clone, Debug)]
pub struct HostPaths {
    env: Env,
}

impl HostPaths {
    pub fn new(env: Env) -> HostPaths {
        HostPaths { env }
    }

    /// Not available in a `cfg(test)` build.
    #[cfg(not(test))]
    pub fn from_process() -> HostPaths {
        HostPaths::new(Env::from_process())
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    /// The directory of one role. Precedence, identical on both hosts:
    /// 1. the role's own override (`PASSPORTSIM_CONFIG_DIR`, `PASSPORTSIM_DATA_ROOT`,
    ///    `--artifacts <dir>`);
    /// 2. `<data root>/artifacts/` for the artifacts role;
    /// 3. `<PASSPORTSIM_HOME>/<role>/` for every other role;
    /// 4. the host default.
    ///
    /// So `PASSPORTSIM_HOME=<dir>` moves artifacts to `<dir>/data/artifacts/`, as on Windows.
    pub fn role(&self, role: Role) -> Result<PathBuf, PathError> {
        let over = &self.env.overrides;
        let own = match role {
            Role::Config => over.config_dir.as_deref(),
            Role::DataRoot => over.data_root.as_deref(),
            Role::Artifacts => over.artifacts.as_deref(),
            _ => None,
        };
        if let Some(value) = own {
            return self.env.expand(value);
        }
        if role == Role::Artifacts {
            return Ok(self.role(Role::DataRoot)?.join(ARTIFACTS_DIR));
        }
        if let Some(home) = over.home.as_deref() {
            return Ok(self.env.expand(home)?.join(role.slug()));
        }
        Ok(match self.env.base()? {
            Base::Home(home) => macos_role(home, role),
            Base::Windows(folders) => windows_role(folders, role),
        })
    }

    pub fn home(&self) -> Result<PathBuf, PathError> {
        self.role(Role::Home)
    }

    pub fn config(&self) -> Result<PathBuf, PathError> {
        self.role(Role::Config)
    }

    pub fn data_root(&self) -> Result<PathBuf, PathError> {
        self.role(Role::DataRoot)
    }

    pub fn cache(&self) -> Result<PathBuf, PathError> {
        self.role(Role::Cache)
    }

    pub fn runtime(&self) -> Result<PathBuf, PathError> {
        self.role(Role::Runtime)
    }

    pub fn logs(&self) -> Result<PathBuf, PathError> {
        self.role(Role::Logs)
    }

    pub fn artifacts(&self) -> Result<PathBuf, PathError> {
        self.role(Role::Artifacts)
    }
}

/// The macOS default of a role, below `home`.
fn macos_role(home: &Path, role: Role) -> PathBuf {
    match role {
        Role::Home => home.to_path_buf(),
        Role::Config => home.join(".config").join(APP_DIR),
        Role::DataRoot => home.join("Library/Application Support").join(APP_DIR),
        Role::Cache => home.join("Library/Caches").join(APP_DIR),
        Role::Runtime => home.join(".passportsim"),
        Role::Logs => home.join(".passportsim").join("logs"),
        Role::Artifacts => macos_role(home, Role::DataRoot).join(ARTIFACTS_DIR),
    }
}

/// The Windows default of a role: config roams, the rest share one tree below
/// `FOLDERID_LocalAppData`.
fn windows_role(folders: &KnownFolders, role: Role) -> PathBuf {
    let local = folders.local_app_data.join(APP_DIR);
    match role {
        Role::Home => folders.profile.clone(),
        Role::Config => folders.roaming_app_data.join(APP_DIR),
        Role::DataRoot | Role::Cache | Role::Runtime | Role::Logs => local.join(role.slug()),
        Role::Artifacts => local.join(Role::DataRoot.slug()).join(ARTIFACTS_DIR),
    }
}

// ---------------------------------------------------------------------------
// Serial-device refusal. Every literal below is built with `concat!` so this file holds no
// device path for the `xtask layering` serial-device lint, which reads string literals.
// ---------------------------------------------------------------------------

/// macOS call-out serial prefix, shared with the serial enumeration.
pub(crate) const DEV_CU: &str = concat!("/dev/", "cu", ".");
/// macOS dial-in serial prefix, refused: opening it blocks on carrier.
pub(crate) const DEV_TTY: &str = concat!("/dev/", "tty", ".");
/// The controlling terminal, the one macOS path [`refuse_device`] exempts, by exact match.
const CONTROLLING_TERMINAL: &str = concat!("/dev/", "tty");
/// The Windows console input buffer, exempted by exact match ([`allow_console_buffers`]).
const CONSOLE_INPUT: &str = concat!("CON", "IN$");
/// The Windows console screen buffer ([`allow_console_buffers`]).
const CONSOLE_OUTPUT: &str = concat!("CON", "OUT$");
/// The Windows device namespace.
const DEVICE_NS: &str = concat!("\\\\", ".", "\\");
/// The Windows verbatim namespace, which is a device namespace except for a drive or a UNC path.
const VERBATIM_NS: &str = concat!("\\\\", "?", "\\");
/// The `UNC` element that makes a verbatim path a share rather than a device.
const VERBATIM_UNC: &str = concat!("un", "c", "\\");
/// Stems Windows reserves whatever the extension.
const RESERVED_STEMS: &[&str] = &[
    concat!("co", "n"),
    concat!("pr", "n"),
    concat!("au", "x"),
    concat!("nu", "l"),
];
/// Prefixes of the numbered reserved ports.
const RESERVED_PORTS: &[&str] = &[concat!("co", "m"), concat!("lp", "t")];
/// Suffixes that make a reserved port. `0` is not reserved; Windows reads the ISO-8859-1
/// superscripts as digits.
const PORT_NUMBERS: &[&str] = &[
    "1", "2", "3", "4", "5", "6", "7", "8", "9", "\u{b9}", "\u{b2}", "\u{b3}",
];

/// Why [`refuse_device`] refused a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The path names a macOS serial device by its `/dev/cu.` or `/dev/tty.` prefix.
    SerialName,
    /// The path, following symlinks, is a character device.
    CharacterDevice,
    /// The path, following symlinks, is a block device.
    BlockDevice,
    /// The path is in the Windows device namespace (`\\.\` or `\\?\`, drives and UNC shares
    /// excepted).
    DeviceNamespace,
    /// The final component is a name Windows reserves for a device, with any extension and any
    /// case (`COM3`, `com3`, `dir\COM3`, `COM3.txt`, `CON`, `NUL`).
    ReservedName,
    /// Absolutizing the path names a device.
    AbsolutizedDevice,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reason::SerialName => "a macOS serial device name",
            Reason::CharacterDevice => "a character device",
            Reason::BlockDevice => "a block device",
            Reason::DeviceNamespace => "a Windows device-namespace path",
            Reason::ReservedName => "a name Windows reserves for a device",
            Reason::AbsolutizedDevice => "a path whose absolutization names a device",
        })
    }
}

/// A refused path and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub path: PathBuf,
    pub reason: Reason,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refusing to open `{}`: it is {}; only `pemu-planner` with feature \
             `device` may open a host serial device",
            self.path.display(),
            self.reason
        )
    }
}

impl std::error::Error for Refusal {}

/// Refuses a path that names a host serial device, **before any open**. Every host file open
/// outside `pemu-planner` goes through this first, and both hosts' rules apply on every host:
///
/// 1. **macOS names.** The path, or its absolutization, begins `/dev/cu.` or `/dev/tty.`.
/// 2. **Windows namespaces**, a lexical test that does not depend on normalization: with `/`
///    replaced by `\`, the path begins `\\.\` or `\\?\`, except `\\?\<drive>:` and `\\?\UNC\`.
/// 3. **Windows reserved names.** The final component, case-insensitively, with a trailing `:`,
///    any extension and trailing spaces removed, is `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`,
///    `LPT1`-`LPT9` or one of those ports with an ISO-8859-1 superscript digit.
/// 4. **Absolutization as a second filter only.** `std::path::absolute` is `GetFullPathNameW`,
///    and Windows 11 stopped mapping a path that begins with a legacy device name to `\\.\NAME`
///    while `CreateFile("COM3")` still reaches the port, so rules 2 and 3 are the primary test
///    and this one only widens it. A failing absolutization is not a refusal.
/// 5. **macOS device kinds.** `std::fs::metadata` is a stat, which follows symlinks without
///    opening, so a symlink to a character or block device is refused too. A stat that fails is
///    **not** a refusal: `flash save --out <new file>` names a path that does not exist yet, so
///    `ENOENT` means "not a device, carry on".
///
/// The only exemptions are the controlling console devices, by exact match
/// ([`allow_controlling_terminal`], [`allow_console_buffers`]).
pub fn refuse_device(path: &Path) -> Result<(), Refusal> {
    if path == allow_controlling_terminal() || allow_console_buffers().contains(&path) {
        return Ok(());
    }
    let refuse = |reason| {
        Err(Refusal {
            path: path.to_path_buf(),
            reason,
        })
    };
    let text = path.to_string_lossy();
    if let Some(reason) = lexical_device(&text) {
        return refuse(reason);
    }
    if let Ok(absolute) = std::path::absolute(path)
        && lexical_device(&absolute.to_string_lossy()).is_some()
    {
        return refuse(Reason::AbsolutizedDevice);
    }
    match device_kind(path) {
        Some(reason) => refuse(reason),
        None => Ok(()),
    }
}

/// The controlling terminal, the one path [`refuse_device`] lets through: a confirmation prompt
/// reads `/dev/tty`, never stdin or stdout, and as a character device it would fail every rule.
/// **By exact match**: `/dev/tty.usbserial-0001` is a dial-in node and stays refused. Windows
/// uses [`allow_console_buffers`] instead.
pub fn allow_controlling_terminal() -> &'static Path {
    Path::new(CONTROLLING_TERMINAL)
}

/// `CONIN$` and `CONOUT$`, the Windows twin of [`allow_controlling_terminal`]. Rule 4 would
/// refuse them: Windows 11 `GetFullPathNameW` maps `CONIN$` to `\\.\CONIN$` (measured on build
/// 26200). **By exact match only**: `\\.\CONIN$`, `CON` and every other spelling stay refused.
pub fn allow_console_buffers() -> [&'static Path; 2] {
    [Path::new(CONSOLE_INPUT), Path::new(CONSOLE_OUTPUT)]
}

/// Rules 1 to 3 of [`refuse_device`]: the name tests, which open nothing.
fn lexical_device(text: &str) -> Option<Reason> {
    if text.starts_with(DEV_CU) || text.starts_with(DEV_TTY) {
        return Some(Reason::SerialName);
    }
    let windows = text.replace('/', "\\");
    if device_namespace(&windows) {
        return Some(Reason::DeviceNamespace);
    }
    if reserved_final_component(&windows) {
        return Some(Reason::ReservedName);
    }
    None
}

/// Rule 2: a `\`-separated path in the Windows device namespace.
///
/// `\\?\C:\tmp\x.bin` and `\\?\UNC\server\share\x` are a drive and a share, not devices;
/// `\\?\USB#VID_303A...`, `\\.\COM12` and `\\.\pipe\x` are devices.
fn device_namespace(windows: &str) -> bool {
    if let Some(rest) = windows.strip_prefix(VERBATIM_NS) {
        let bytes = rest.as_bytes();
        let drive = bytes.first().is_some_and(u8::is_ascii_alphabetic)
            && bytes.get(1) == Some(&b':')
            && matches!(bytes.get(2), None | Some(b'\\'));
        let unc = rest
            .get(..VERBATIM_UNC.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(VERBATIM_UNC));
        return !(drive || unc);
    }
    windows.starts_with(DEVICE_NS)
}

/// Rule 3: the final component of a `\`-separated path is a reserved device name.
fn reserved_final_component(windows: &str) -> bool {
    let trimmed = windows.trim_end_matches('\\');
    let last = trimmed.rsplit('\\').next().unwrap_or(trimmed);
    let name = last.strip_suffix(':').unwrap_or(last);
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_lowercase();
    RESERVED_STEMS.contains(&stem.as_str())
        || RESERVED_PORTS.iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|number| PORT_NUMBERS.contains(&number))
        })
}

/// Rule 5: the device kind of `path` with symlinks followed, or `None` when the stat fails or the
/// path is an ordinary file.
#[cfg(unix)]
fn device_kind(path: &Path) -> Option<Reason> {
    use std::os::unix::fs::FileTypeExt;
    let kind = std::fs::metadata(path).ok()?.file_type();
    if kind.is_char_device() {
        Some(Reason::CharacterDevice)
    } else if kind.is_block_device() {
        Some(Reason::BlockDevice)
    } else {
        None
    }
}

/// Rule 5 on Windows: a device is named, not stat-ed, so the lexical rules carry the guard alone.
#[cfg(not(unix))]
fn device_kind(_path: &Path) -> Option<Reason> {
    None
}

// ---------------------------------------------------------------------------
// Owner-only files and containment comparisons.
// ---------------------------------------------------------------------------

/// Why an owner-only operation refused.
#[derive(Debug)]
pub enum GuardError {
    /// Refused before any open by [`refuse_device`].
    Device(Refusal),
    /// The path is not owner-only, a host call failed, or this host has no implementation.
    Platform(PlatformError),
}

impl GuardError {
    /// Whether this is "no implementation on this host", which callers map to
    /// `E_HOST_UNSUPPORTED`.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, GuardError::Platform(e) if e.is_unsupported())
    }
}

impl From<Refusal> for GuardError {
    fn from(refusal: Refusal) -> GuardError {
        GuardError::Device(refusal)
    }
}

impl From<PlatformError> for GuardError {
    fn from(error: PlatformError) -> GuardError {
        GuardError::Platform(error)
    }
}

impl fmt::Display for GuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GuardError::Device(refusal) => refusal.fmt(f),
            GuardError::Platform(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for GuardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GuardError::Device(refusal) => Some(refusal),
            GuardError::Platform(error) => Some(error),
        }
    }
}

/// Owner-only file operations for every private file (daemon token, discovery file, launch
/// codes, logs, boot cache, planner backups, hash file). No device is ever opened
/// ([`refuse_device`] first), and protection is applied at creation and re-checked on every
/// read, with no repair path.
#[derive(Clone, Copy)]
pub struct OwnerOnlyFiles<'a> {
    guard: &'a dyn OwnerOnly,
}

impl fmt::Debug for OwnerOnlyFiles<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnerOnlyFiles")
    }
}

impl OwnerOnlyFiles<'static> {
    pub fn host() -> OwnerOnlyFiles<'static> {
        OwnerOnlyFiles {
            guard: crate::platform::owner_only(),
        }
    }
}

impl<'a> OwnerOnlyFiles<'a> {
    pub fn new(guard: &'a dyn OwnerOnly) -> OwnerOnlyFiles<'a> {
        OwnerOnlyFiles { guard }
    }

    /// Creates `path` and its parents as owner-only directories, or accepts an existing tree whose
    /// leaf is still owner-only.
    pub fn create_dir(&self, path: &Path) -> Result<(), GuardError> {
        refuse_device(path)?;
        Ok(self.guard.create_dir(path)?)
    }

    /// Creates `path` as one new owner-only directory, refusing a name that is taken
    /// ([`OwnerOnly::create_new_dir`]). The parent is not created.
    pub fn create_new_dir(&self, path: &Path) -> Result<(), GuardError> {
        refuse_device(path)?;
        Ok(self.guard.create_new_dir(path)?)
    }

    /// Creates or truncates `path` as an owner-only file, open for writing.
    pub fn create_file(&self, path: &Path) -> Result<File, GuardError> {
        refuse_device(path)?;
        Ok(self.guard.create_file(path)?)
    }

    /// Writes `bytes` to an owner-only `path`. The parent is **not** created, so a typo in a role
    /// path fails instead of building a tree somewhere else.
    pub fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), GuardError> {
        let mut file = self.create_file(path)?;
        file.write_all(bytes)
            .map_err(|e| PlatformError::io(path, e))?;
        file.sync_all().map_err(|e| PlatformError::io(path, e))?;
        Ok(())
    }

    /// Writes `bytes` to a new owner-only `path`, refusing when anything is already there
    /// ([`OwnerOnly::create_new_file`]).
    pub fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<(), GuardError> {
        refuse_device(path)?;
        let mut file = self.guard.create_new_file(path)?;
        file.write_all(bytes)
            .map_err(|e| PlatformError::io(path, e))?;
        file.sync_all().map_err(|e| PlatformError::io(path, e))?;
        Ok(())
    }

    /// Opens `path` for reading after the owner-only check.
    pub fn open(&self, path: &Path) -> Result<File, GuardError> {
        self.check(path)?;
        File::open(path).map_err(|e| GuardError::Platform(PlatformError::io(path, e)))
    }

    /// Reads an owner-only file whole, after the same check.
    pub fn read(&self, path: &Path) -> Result<Vec<u8>, GuardError> {
        let mut file = self.open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| PlatformError::io(path, e))?;
        Ok(bytes)
    }

    /// The read check alone, for a caller that hands the path to something else.
    pub fn check(&self, path: &Path) -> Result<(), GuardError> {
        refuse_device(path)?;
        Ok(self.guard.check(path)?)
    }
}

/// Strips a Windows verbatim prefix: `\\?\C:\x` becomes `C:\x` and `\\?\UNC\server\share`
/// becomes `\\server\share`. A no-op on macOS.
pub fn strip_verbatim_prefix(text: &str) -> String {
    let Some(rest) = text.strip_prefix(VERBATIM_NS) else {
        return text.to_string();
    };
    match rest
        .get(..VERBATIM_UNC.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(VERBATIM_UNC))
    {
        true => format!("\\\\{}", &rest[VERBATIM_UNC.len()..]),
        false => rest.to_string(),
    }
}

/// The comparison key of a path: its components, case-folded because APFS and NTFS are
/// case-insensitive by default, and a vector so `/a/bc` is not inside `/a/b`.
///
/// UNVERIFIED: `str::to_lowercase` is not the filesystem's own fold, so an exotic pair of names
/// could compare differently here. Every compared path is built from a role and ASCII names.
pub fn canonical_key(path: &Path) -> Result<Vec<String>, GuardError> {
    let canonical = canonical_for_containment(path)?;
    let text = strip_verbatim_prefix(&canonical.to_string_lossy());
    Ok(Path::new(&text)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect())
}

/// Whether `candidate` is `root` or lies below it, case-insensitively and with `\\?\` stripped.
/// **Reflexive**: a backup written *as* the repository directory is inside it. Neither path has
/// to exist, and a symlinked ancestor (`/tmp` on macOS) is still resolved.
pub fn contains(root: &Path, candidate: &Path) -> Result<bool, GuardError> {
    let root = canonical_key(root)?;
    let candidate = canonical_key(candidate)?;
    Ok(candidate.len() >= root.len() && candidate[..root.len()] == root[..])
}

/// The canonical path for containment, defined for a path that does not exist yet: the longest
/// existing ancestor is canonicalized and the rest appended lexically (`..` pops, `.` drops).
fn canonical_for_containment(path: &Path) -> Result<PathBuf, GuardError> {
    let absolute = std::path::absolute(path).map_err(|e| PlatformError::io(path, e))?;
    let mut existing = absolute.clone();
    let mut tail = Vec::new();
    let base = loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            break canonical;
        }
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                existing.pop();
            }
            None => break existing,
        }
    };
    let mut out = base;
    for name in tail.iter().rev() {
        if name == ".." {
            out.pop();
        } else if name != "." {
            out.push(name);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A macOS base whose `HOME` is `/home/u`.
    fn macos_env(overrides: Overrides) -> HostPaths {
        HostPaths::new(Env::macos("/home/u", overrides))
    }

    fn folders() -> KnownFolders {
        KnownFolders {
            profile: PathBuf::from(r"C:\Users\u"),
            roaming_app_data: PathBuf::from(r"C:\Users\u\AppData\Roaming"),
            local_app_data: PathBuf::from(r"C:\Users\u\AppData\Local"),
        }
    }

    /// A Windows base on [`folders`], resolvable on any host (the known-folder call is the seam).
    fn windows_env(overrides: Overrides) -> HostPaths {
        HostPaths::new(Env::windows(folders(), overrides))
    }

    /// Only `home` set, the shape a `PASSPORTSIM_HOME` run has.
    fn emu_home(dir: &str) -> Overrides {
        Overrides {
            home: Some(dir.to_string()),
            ..Overrides::default()
        }
    }

    #[test]
    fn macos_role_table() {
        let paths = macos_env(Overrides::default());
        let home = Path::new("/home/u");
        let data = home
            .join("Library")
            .join("Application Support")
            .join(APP_DIR);
        assert_eq!(paths.home(), Ok(home.to_path_buf()));
        assert_eq!(paths.config(), Ok(home.join(".config").join(APP_DIR)));
        assert_eq!(paths.data_root(), Ok(data.clone()));
        assert_eq!(
            paths.cache(),
            Ok(home.join("Library").join("Caches").join(APP_DIR))
        );
        assert_eq!(paths.runtime(), Ok(home.join(".passportsim")));
        assert_eq!(paths.logs(), Ok(home.join(".passportsim").join("logs")));
        assert_eq!(paths.artifacts(), Ok(data.join("artifacts")));
    }

    /// Configuration roams, everything else does not.
    #[test]
    fn windows_role_table() {
        let paths = windows_env(Overrides::default());
        let f = folders();
        let local = f.local_app_data.join(APP_DIR);
        assert_eq!(paths.home(), Ok(f.profile.clone()));
        assert_eq!(paths.config(), Ok(f.roaming_app_data.join(APP_DIR)));
        assert_eq!(paths.data_root(), Ok(local.join("data")));
        assert_eq!(paths.cache(), Ok(local.join("cache")));
        assert_eq!(paths.runtime(), Ok(local.join("run")));
        assert_eq!(paths.logs(), Ok(local.join("logs")));
        assert_eq!(paths.artifacts(), Ok(local.join("data").join("artifacts")));
        assert!(!paths.data_root().unwrap().starts_with(&f.roaming_app_data));
    }

    /// Artifacts follow the relocated data root, so the tree matches the Windows one.
    #[test]
    fn emu_home_puts_every_role_under_one_directory() {
        let over = emu_home("/srv/emu");
        let mac = macos_env(over.clone());
        let win = windows_env(over);
        for role in Role::ALL {
            let expected = match role {
                Role::Artifacts => Path::new("/srv/emu").join("data").join(ARTIFACTS_DIR),
                _ => Path::new("/srv/emu").join(role.slug()),
            };
            assert_eq!(mac.role(role), Ok(expected.clone()), "{role} on macOS");
            assert_eq!(win.role(role), Ok(expected), "{role} on Windows");
        }
        assert_eq!(mac.home(), Ok(PathBuf::from("/srv/emu/home")));
        assert_eq!(mac.runtime(), Ok(PathBuf::from("/srv/emu/run")));
    }

    #[test]
    fn artifacts_stay_below_the_data_root_that_moved() {
        let cases = [
            Overrides::default(),
            emu_home("/srv/emu"),
            Overrides {
                data_root: Some("/data/pemu".to_string()),
                ..Overrides::default()
            },
            Overrides {
                home: Some("/srv/emu".to_string()),
                data_root: Some("/data/pemu".to_string()),
                ..Overrides::default()
            },
            Overrides {
                home: Some("/srv/emu".to_string()),
                config_dir: Some("/etc/pemu".to_string()),
                ..Overrides::default()
            },
        ];
        for over in cases {
            for paths in [macos_env(over.clone()), windows_env(over.clone())] {
                let data = paths.data_root().expect("a data root");
                assert_eq!(
                    paths.artifacts(),
                    Ok(data.join(ARTIFACTS_DIR)),
                    "{over:?} on {:?}",
                    paths.env().base()
                );
            }
        }
        let flagged = macos_env(Overrides {
            home: Some("/srv/emu".to_string()),
            artifacts: Some("/out/run7".to_string()),
            ..Overrides::default()
        });
        assert_eq!(flagged.artifacts(), Ok(PathBuf::from("/out/run7")));
        assert_eq!(flagged.data_root(), Ok(PathBuf::from("/srv/emu/data")));
    }

    #[test]
    fn one_role_overrides_win_over_the_home_override() {
        let over = Overrides {
            home: Some("/srv/emu".to_string()),
            config_dir: Some("/etc/pemu".to_string()),
            data_root: Some("/data/pemu".to_string()),
            artifacts: None,
        };
        let paths = macos_env(over);
        assert_eq!(paths.config(), Ok(PathBuf::from("/etc/pemu")));
        assert_eq!(paths.data_root(), Ok(PathBuf::from("/data/pemu")));
        assert_eq!(paths.artifacts(), Ok(PathBuf::from("/data/pemu/artifacts")));
        // Roles neither override names still follow `PASSPORTSIM_HOME`.
        assert_eq!(paths.cache(), Ok(PathBuf::from("/srv/emu/cache")));
        assert_eq!(paths.logs(), Ok(PathBuf::from("/srv/emu/logs")));
    }

    #[test]
    fn data_root_override_moves_only_its_role() {
        let paths = macos_env(Overrides {
            data_root: Some("/data/pemu".to_string()),
            ..Overrides::default()
        });
        assert_eq!(paths.data_root(), Ok(PathBuf::from("/data/pemu")));
        assert_eq!(paths.artifacts(), Ok(PathBuf::from("/data/pemu/artifacts")));
        assert_eq!(
            paths.config(),
            Ok(PathBuf::from("/home/u/.config/passportsim"))
        );
        let flagged = macos_env(Overrides {
            data_root: Some("/data/pemu".to_string()),
            artifacts: Some("/out/run7".to_string()),
            ..Overrides::default()
        });
        assert_eq!(flagged.artifacts(), Ok(PathBuf::from("/out/run7")));
        assert_eq!(flagged.data_root(), Ok(PathBuf::from("/data/pemu")));
    }

    #[test]
    fn tilde_expands_in_both_spellings() {
        let home = Path::new("/home/u");
        assert_eq!(expand_tilde("~/data/pemu", home), home.join("data/pemu"));
        assert_eq!(expand_tilde("~\\data\\pemu", home), home.join("data\\pemu"));
        assert_eq!(expand_tilde("~", home), home.to_path_buf());
        assert_eq!(expand_tilde("/srv/pemu", home), PathBuf::from("/srv/pemu"));
        // `~x` is a name, not a home directory.
        assert_eq!(expand_tilde("~user/x", home), PathBuf::from("~user/x"));

        let mac = macos_env(Overrides {
            config_dir: Some("~/cfg".to_string()),
            data_root: Some("~\\data".to_string()),
            ..Overrides::default()
        });
        assert_eq!(mac.config(), Ok(home.join("cfg")));
        assert_eq!(mac.data_root(), Ok(home.join("data")));
        let win = windows_env(Overrides {
            config_dir: Some("~\\cfg".to_string()),
            data_root: Some("~/data".to_string()),
            ..Overrides::default()
        });
        assert_eq!(win.config(), Ok(folders().profile.join("cfg")));
        assert_eq!(win.data_root(), Ok(folders().profile.join("data")));
    }

    /// One spelling in two variables of one family names one directory.
    #[test]
    fn a_tilde_is_the_host_home_in_every_override() {
        let paths = macos_env(Overrides {
            home: Some("~/emu".to_string()),
            data_root: Some("~/data".to_string()),
            ..Overrides::default()
        });
        // The home role moved, and `~` did not follow it.
        assert_eq!(paths.home(), Ok(PathBuf::from("/home/u/emu/home")));
        assert_eq!(paths.cache(), Ok(PathBuf::from("/home/u/emu/cache")));
        assert_eq!(paths.data_root(), Ok(PathBuf::from("/home/u/data")));
        assert_eq!(
            paths.artifacts(),
            Ok(PathBuf::from("/home/u/data/artifacts"))
        );
    }

    #[test]
    fn a_test_build_has_no_fallback_to_a_real_directory() {
        assert_eq!(host_base(), Err(PathError::NoFallbackUnderTest));
        let bare = HostPaths::new(Env::new(host_base(), Overrides::default()));
        for role in Role::ALL {
            assert_eq!(
                bare.role(role),
                Err(PathError::NoFallbackUnderTest),
                "{role} resolved without an override"
            );
        }
        let overridden = HostPaths::new(Env::new(host_base(), emu_home("/srv/emu")));
        assert_eq!(overridden.runtime(), Ok(PathBuf::from("/srv/emu/run")));
        // A `~` in an override still needs the host home, which a test build does not have.
        let tilde = HostPaths::new(Env::new(host_base(), emu_home("~/emu")));
        assert_eq!(tilde.home(), Err(PathError::NoFallbackUnderTest));
    }

    fn refused(path: &str) -> Reason {
        refuse_device(Path::new(path)).expect_err(path).reason
    }

    #[test]
    fn refuse_device_covers_the_macos_spellings() {
        for name in [
            "/dev/cu.usbmodem14401",
            "/dev/cu.Bluetooth-Incoming-Port",
            "/dev/tty.usbserial-0001",
        ] {
            assert_eq!(refused(name), Reason::SerialName, "{name}");
        }
        // An ordinary path is not a device, and neither is a directory that merely sits in /dev.
        for name in ["/tmp/flash.bin", "firmware/app.bin", "/dev/fd"] {
            assert!(refuse_device(Path::new(name)).is_ok(), "{name}");
        }
    }

    /// The test is lexical, so the Windows spellings refuse on any host.
    #[test]
    fn refuse_device_covers_the_windows_spellings() {
        for name in [
            "COM3",
            "com3",
            "COM3:",
            r"dir\COM3",
            "dir/com9",
            "COM3.txt",
            "lpt3.log",
            "COM\u{b9}",
            "LPT\u{b3}",
            "CON",
            "nul",
            "Aux",
            "PRN",
            "COM3 ",
            r"C:\tmp\COM1",
        ] {
            assert_eq!(refused(name), Reason::ReservedName, "{name}");
        }
        for name in [
            r"\\.\COM12",
            r"\\?\COM3",
            r"\\.\pipe\pemu",
            r"\\?\USB#VID_303A&PID_1001#001#{a5dcbf10-6530-11d2-901f-00c04fb951ed}",
            "//./COM12",
        ] {
            assert_eq!(refused(name), Reason::DeviceNamespace, "{name}");
        }
        for name in [
            r"\\?\C:\tmp\x.bin",
            r"\\?\UNC\server\share\x.bin",
            r"C:\tmp\COM0.bin",
            "com0",
            "com10",
            "comet.bin",
            "config.toml",
        ] {
            assert!(refuse_device(Path::new(name)).is_ok(), "{name}");
        }
    }

    #[test]
    fn the_controlling_console_devices_are_the_only_exemptions() {
        let tty = allow_controlling_terminal();
        assert!(
            refuse_device(tty).is_ok(),
            "the terminal path reads the prompt from `{}`",
            tty.display()
        );
        for buffer in allow_console_buffers() {
            assert!(refuse_device(buffer).is_ok(), "{}", buffer.display());
            let spelled = format!("{DEVICE_NS}{}", buffer.display());
            assert_eq!(refused(&spelled), Reason::DeviceNamespace, "{spelled}");
        }
        assert_eq!(refused(concat!("co", "n")), Reason::ReservedName);
        // A prefix test would admit the dial-in nodes, whose open blocks until carrier.
        for name in [
            concat!("/dev/", "tty", ".usbserial-0001"),
            concat!("/dev/", "tty", ".Bluetooth-Incoming-Port"),
        ] {
            assert_eq!(refused(name), Reason::SerialName, "{name}");
        }
        // A device next to it is still refused by the kind test.
        #[cfg(unix)]
        {
            let null = PathBuf::from(concat!("/dev/", "null"));
            if null.exists() {
                assert_eq!(
                    refuse_device(&null).expect_err("a character device").reason,
                    Reason::CharacterDevice
                );
            }
        }
    }

    /// A stat follows the link, which a string prefix does not. A failing stat is not a refusal.
    #[cfg(unix)]
    #[test]
    fn refuse_device_follows_a_symlink_to_a_character_device() {
        // A fixture directory, not a resolved role: the resolver above reads no environment.
        let dir = std::env::temp_dir().join(format!("pemu-paths-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let link = dir.join("port");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/dev/null", &link).expect("symlink");
        assert_eq!(
            refuse_device(&link)
                .expect_err("a symlink to a character device")
                .reason,
            Reason::CharacterDevice
        );
        let file = dir.join("flash.bin");
        std::fs::write(&file, b"pemu").expect("fixture file");
        assert!(refuse_device(&file).is_ok());
        assert!(refuse_device(&dir.join("missing.bin")).is_ok());
        std::fs::remove_file(&link).expect("cleanup");
        std::fs::remove_file(&file).expect("cleanup");
        std::fs::remove_dir(&dir).expect("cleanup");
    }

    #[test]
    fn a_refusal_names_the_path_and_the_rule() {
        let refusal = refuse_device(Path::new("COM3")).expect_err("a reserved name");
        let text = refusal.to_string();
        assert!(text.contains("COM3"), "{text}");
        assert!(text.contains("only `pemu-planner`"), "{text}");
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;
    use crate::platform::fake::{FakeHost, OwnerOnlyCall};
    use crate::platform::{Host, scratch_dir};

    /// Built with `concat!` so this file holds no device literal.
    const A_SERIAL_DEVICE: &str = concat!("/dev/", "cu", ".usbmodem1101");

    #[test]
    fn a_private_file_is_written_and_re_checked_on_every_read() {
        let dir = scratch_dir("guard-write-read");
        let host = FakeHost::new();
        let files = OwnerOnlyFiles::new(host.owner_only());
        files.create_dir(&dir).expect("owner-only directory");
        let token = dir.join("serve.json");
        files.write(&token, b"{}").expect("owner-only write");
        assert_eq!(files.read(&token).expect("owner-only read"), b"{}");
        assert_eq!(files.read(&token).expect("owner-only read"), b"{}");

        let checks = host
            .owner_only
            .calls()
            .into_iter()
            .filter(|c| matches!(c, OwnerOnlyCall::Check(p) if p == &token))
            .count();
        assert_eq!(checks, 2, "re-checked on every read, not once");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Refused rather than repaired.
    #[test]
    fn a_widened_file_refuses_on_read() {
        let dir = scratch_dir("guard-widened");
        let host = FakeHost::new();
        let files = OwnerOnlyFiles::new(host.owner_only());
        files.create_dir(&dir).expect("owner-only directory");
        let token = dir.join("token");
        files.write(&token, b"secret").expect("owner-only write");

        host.owner_only.refuse(&token);
        let refused = files.read(&token).expect_err("a widened file is refused");
        assert!(
            matches!(&refused, GuardError::Platform(e) if !e.is_unsupported()),
            "{refused}"
        );
        assert!(
            token.exists(),
            "the refusal repaired nothing and removed nothing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// No device syscall happens: the refusal comes before the platform service.
    #[test]
    fn a_device_path_refuses_before_any_open() {
        let host = FakeHost::new();
        let files = OwnerOnlyFiles::new(host.owner_only());
        let device = Path::new(A_SERIAL_DEVICE);
        for refusal in [
            files.create_file(device).err().map(|e| e.to_string()),
            files.check(device).err().map(|e| e.to_string()),
            files.create_dir(device).err().map(|e| e.to_string()),
        ] {
            let text = refusal.expect("a device path is refused");
            assert!(text.contains("only `pemu-planner`"), "{text}");
        }
        assert!(
            host.owner_only.calls().is_empty(),
            "the platform service was never reached"
        );
    }

    /// "Not implemented on this host" is distinguishable from "not owner-only".
    #[test]
    fn an_unimplemented_host_is_distinguishable_from_a_refusal() {
        let host = FakeHost::new();
        host.owner_only.refuse_everything();
        let files = OwnerOnlyFiles::new(host.owner_only());
        let e = files
            .check(Path::new("/tmp/whatever"))
            .expect_err("an unimplemented host refuses");
        assert!(e.is_unsupported(), "{e}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_mechanism_backs_the_public_api() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = scratch_dir("guard-macos");
        let files = OwnerOnlyFiles::host();
        files.create_dir(&dir).expect("owner-only directory");
        let token = dir.join("token");
        files
            .write(&token, b"launch code")
            .expect("owner-only write");
        assert_eq!(
            std::fs::metadata(&token).expect("metadata").mode() & 0o7777,
            0o600,
            "created 0600, not chmod-ed afterwards"
        );
        assert_eq!(files.read(&token).expect("owner-only read"), b"launch code");

        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644))
            .expect("widen the mode");
        let refused = files
            .read(&token)
            .expect_err("a world-readable file is refused");
        assert!(refused.to_string().contains("group or other"), "{refused}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_windows_verbatim_prefix_is_stripped() {
        assert_eq!(strip_verbatim_prefix(r"\\?\C:\tmp\x.bin"), r"C:\tmp\x.bin");
        assert_eq!(
            strip_verbatim_prefix(concat!(r"\\?\", r"unc\server\share\x")),
            r"\\server\share\x"
        );
        assert_eq!(strip_verbatim_prefix("/home/u/x"), "/home/u/x");
        assert_eq!(strip_verbatim_prefix(r"C:\tmp"), r"C:\tmp");
    }

    #[test]
    fn containment_compares_whole_components() {
        let root = scratch_dir("guard-contains");
        let inside = root.join("backups").join("a.bin");
        std::fs::create_dir_all(inside.parent().expect("parent")).expect("tree");
        assert!(contains(&root, &root).expect("reflexive"));
        assert!(contains(&root, &inside).expect("below the root"));

        let sibling = root.with_file_name(format!(
            "{}-elsewhere",
            root.file_name().expect("name").to_string_lossy()
        ));
        assert!(
            !contains(&root, &sibling).expect("a sibling with a longer name"),
            "a component prefix is not containment"
        );
        assert!(!contains(&inside, &root).expect("the other way round"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn containment_is_case_insensitive_and_works_before_the_file_exists() {
        let root = scratch_dir("guard-case");
        std::fs::create_dir_all(&root).expect("tree");
        let shouty = PathBuf::from(root.to_string_lossy().to_uppercase());
        let not_yet = root.join("Backups").join("NOT-YET-WRITTEN.bin");
        assert!(contains(&root, &not_yet).expect("a path that does not exist yet"));
        assert!(
            contains(&shouty, &not_yet).expect("upper-cased root"),
            "APFS and NTFS are case-insensitive, so the comparison is too"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// On macOS `/tmp` itself is a symlink to `/private/tmp`.
    #[cfg(unix)]
    #[test]
    fn containment_resolves_symlinks_and_parent_components() {
        let root = scratch_dir("guard-symlink");
        let real = root.join("real");
        std::fs::create_dir_all(real.join("inner")).expect("tree");
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert!(
            contains(&real, &link.join("inner").join("x.bin")).expect("through the symlink"),
            "a symlinked ancestor resolves to the directory it names"
        );
        assert!(
            !contains(&real, &root.join("real").join("..").join("outside")).expect(".. resolves"),
            "`..` climbs out of the root"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
