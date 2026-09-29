//! Host directory roles of the tooling (`docs/ARCHITECTURE.md`, Host surfaces).
//!
//! The role table, the Windows known folders, the `PASSPORTSIM_*` overrides and tilde expansion
//! are `pemu_host::paths`, the resolver the daemon uses, so the receipt store and the daemon
//! cannot look at different roots. What is added here is tooling-only:
//!
//! - the `[paths] data_root` key of `config.toml`, which sits below the overrides;
//! - the receipts directory below that data root;
//! - **no fallback in a test build**: `pemu_host::paths` guards only its own unit tests, so this
//!   module resolves nothing from the process when xtask is compiled as a test, and a test that
//!   injects no base fails instead of reading the developer's directories.
//!
//! **The overrides are not honored by the secrets guard.** `xtask secrets-check`, the git hooks
//! and `xtask/src/secrets/device.rs` resolve the hash file, the data root and the device
//! directory from the host base only, and refuse when a `PASSPORTSIM_*` variable is set unless
//! `--root` is given. That refusal lives in `secrets/device.rs`.

use std::path::{Path, PathBuf};

#[cfg(test)]
pub use pemu_host::paths::KnownFolders;
pub use pemu_host::paths::expand_tilde as expand_home;
pub use pemu_host::paths::{Base, DATA_ROOT_ENV, Overrides, Role};
use pemu_host::paths::{Env, HostPaths, PathError};

/// Data root below `HOME` on macOS when `config.toml` sets none.
pub const DEFAULT_DATA_ROOT: &str = "Library/Application Support/passportsim";
/// Local configuration file below `HOME`, as it is written in messages.
pub const CONFIG_FILE: &str = ".config/passportsim/config.toml";
/// Name of the configuration file inside the config role.
const CONFIG_FILE_NAME: &str = "config.toml";
/// Receipts below the data root.
pub const RECEIPTS_DIR: &str = "receipts";

/// The base directories of the running host; none in a test build.
pub fn host_base() -> Result<Base, String> {
    if cfg!(test) {
        return Err(PathError::NoFallbackUnderTest.to_string());
    }
    pemu_host::paths::host_base().map_err(|err| err.to_string())
}

/// The host directory roles, with errors as text.
#[derive(Clone, Debug)]
pub struct HostDirs {
    paths: HostPaths,
}

impl HostDirs {
    /// An injected resolver: no process state is read.
    pub fn new(base: Result<Base, PathError>, overrides: Overrides) -> HostDirs {
        HostDirs {
            paths: HostPaths::new(Env::new(base, overrides)),
        }
    }

    /// The resolver of the current process; in a test build, one with no base and no override.
    pub fn from_process() -> HostDirs {
        #[cfg(test)]
        {
            HostDirs::new(Err(PathError::NoFallbackUnderTest), Overrides::default())
        }
        #[cfg(not(test))]
        {
            HostDirs {
                paths: HostPaths::from_process(),
            }
        }
    }

    /// The directory of one role.
    pub fn role(&self, role: Role) -> Result<PathBuf, String> {
        self.paths.role(role).map_err(|err| err.to_string())
    }

    /// The home role.
    pub fn home(&self) -> Result<PathBuf, String> {
        self.role(Role::Home)
    }

    /// The config role.
    pub fn config(&self) -> Result<PathBuf, String> {
        self.role(Role::Config)
    }

    /// The data-root role, before the `[paths] data_root` key of the configuration file.
    pub fn data_root_role(&self) -> Result<PathBuf, String> {
        self.role(Role::DataRoot)
    }

    /// Whether an override claims the data-root role, which makes the tooling ignore the
    /// `[paths] data_root` key. `PASSPORTSIM_CONFIG_DIR` moves another role and leaves the key in
    /// force, since each narrow variable moves one role.
    fn claims_data_root(&self) -> bool {
        let overrides = self.paths.env().overrides();
        overrides.home.is_some() || overrides.data_root.is_some()
    }
}

/// `HOME` of the current process, which is the home role.
pub fn home() -> Result<PathBuf, String> {
    HostDirs::from_process().home()
}

/// The `[paths] data_root` key of the config text, still unexpanded, or `None` when the file is
/// absent or silent about it.
fn config_data_root(config: Option<&str>) -> Result<Option<String>, String> {
    let Some(text) = config else {
        return Ok(None);
    };
    let table: toml::Table = text.parse().map_err(|_| {
        "the config.toml of the config directory does not parse as TOML".to_string()
    })?;
    let Some(value) = table.get("paths").and_then(|paths| paths.get("data_root")) else {
        return Ok(None);
    };
    let value = value.as_str().ok_or_else(|| {
        "[paths] data_root of the config.toml of the config directory is not a string".to_string()
    })?;
    Ok(Some(value.to_string()))
}

/// The data root below one macOS `home`: `[paths] data_root` of the config text (a leading `~/`
/// or `~\` expanded against `home`), or [`DEFAULT_DATA_ROOT`].
///
/// It resolves the macOS column only; a caller with a [`HostDirs`] calls [`data_root_of`].
pub fn data_root(home: &Path, config: Option<&str>) -> Result<PathBuf, String> {
    let dirs = HostDirs::new(Ok(Base::Home(home.to_path_buf())), Overrides::default());
    data_root_of(&dirs, config)
}

/// The data root of `dirs`: an override that claims the role when one is set, else the
/// `[paths] data_root` key of `config`, else the host's default data root.
pub fn data_root_of(dirs: &HostDirs, config: Option<&str>) -> Result<PathBuf, String> {
    if dirs.claims_data_root() {
        return dirs.data_root_role();
    }
    match config_data_root(config)? {
        Some(value) => Ok(expand_home(&value, &dirs.home()?)),
        None => dirs.data_root_role(),
    }
}

/// The `config.toml` text of the config role of `dirs`, or `None` when there is none.
pub fn read_config(dirs: &HostDirs) -> Result<Option<String>, String> {
    let path = dirs.config()?.join(CONFIG_FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("cannot read {}: {}", path.display(), err.kind())),
    }
}

/// The tooling data root of the current user: [`data_root_of`] over the process resolver and the
/// `config.toml` of its config role.
///
/// The `PASSPORTSIM_*` overrides are honored here so that the receipt store and the daemon look at
/// the same root.
pub fn process_data_root() -> Result<PathBuf, String> {
    let dirs = HostDirs::from_process();
    let config = read_config(&dirs)?;
    data_root_of(&dirs, config.as_deref())
}

/// `<data root>/receipts` of the current user.
pub fn receipts_dir() -> Result<PathBuf, String> {
    Ok(process_data_root()?.join(RECEIPTS_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn macos(overrides: Overrides) -> HostDirs {
        HostDirs::new(Ok(Base::Home(PathBuf::from("/home/u"))), overrides)
    }

    /// `PASSPORTSIM_HOME` claims every role, the data root included, so it replaces the key.
    #[test]
    fn the_home_override_claims_the_data_root() {
        let relocated = macos(Overrides {
            home: Some("/srv/emu".to_string()),
            ..Overrides::default()
        });
        assert_eq!(
            data_root_of(&relocated, Some("[paths]\ndata_root = \"/ignored\"\n")),
            Ok(PathBuf::from("/srv/emu/data"))
        );
    }

    /// With no key, `PASSPORTSIM_CONFIG_DIR` leaves the data root on the host row, never below
    /// the config override.
    #[test]
    fn the_config_dir_override_does_not_move_the_data_root() {
        let dirs = macos(Overrides {
            config_dir: Some("/etc/pemu".to_string()),
            ..Overrides::default()
        });
        assert_eq!(dirs.config(), Ok(PathBuf::from("/etc/pemu")));
        assert_eq!(
            data_root_of(&dirs, None),
            Ok(Path::new("/home/u").join(DEFAULT_DATA_ROOT))
        );
    }

    /// The macOS-only helper agrees with the resolver on a macOS base.
    #[test]
    fn the_macos_helper_is_the_macos_column() {
        let home = Path::new("/home/u");
        let config = Some("[paths]\ndata_root = \"~\\\\data\"\n");
        for text in [None, config] {
            assert_eq!(
                data_root(home, text),
                data_root_of(&macos(Overrides::default()), text)
            );
        }
        assert_eq!(data_root(home, config), Ok(home.join("data")));
    }
}
