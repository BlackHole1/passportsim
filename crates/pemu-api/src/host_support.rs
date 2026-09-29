//! Host availability of a command or option. A table beside `CommandSpec` rather than a field: the
//! `native_only` annotation is binary, and there are two native hosts.
//!
//! Committed generated docs carry the full matrix and are identical on every host; this table is
//! read at runtime only, by MCP `tools/list`, `doctor` and the `E_HOST_UNSUPPORTED` refusal.

use crate::spec::CommandSpec;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Host {
    MacOs,
    Windows,
    Browser,
}

impl Host {
    /// `None` for a host outside the matrix.
    pub const fn current() -> Option<Host> {
        #[cfg(target_arch = "wasm32")]
        {
            Some(Host::Browser)
        }
        #[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
        {
            Some(Host::MacOs)
        }
        #[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
        {
            Some(Host::Windows)
        }
        #[cfg(all(
            not(target_arch = "wasm32"),
            not(target_os = "macos"),
            not(target_os = "windows")
        ))]
        {
            None
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Host::MacOs => "macos",
            Host::Windows => "windows",
            Host::Browser => "browser",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Hosts {
    pub macos: bool,
    pub windows: bool,
    pub browser: bool,
}

impl Hosts {
    /// The value for a name with no row.
    pub const ALL: Hosts = Hosts {
        macos: true,
        windows: true,
        browser: true,
    };
    pub const NATIVE: Hosts = Hosts {
        macos: true,
        windows: true,
        browser: false,
    };
    /// Oracles and the pty endpoint.
    pub const MACOS_ONLY: Hosts = Hosts {
        macos: true,
        windows: false,
        browser: false,
    };

    pub const fn has(self, host: Host) -> bool {
        match host {
            Host::MacOs => self.macos,
            Host::Windows => self.windows,
            Host::Browser => self.browser,
        }
    }
}

/// A command name, or `"<command> --<option>"`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Support {
    pub name: &'static str,
    pub hosts: Hosts,
    /// Named in the `E_HOST_UNSUPPORTED` message as the host-neutral way to do the same thing.
    pub hint: &'static str,
}

/// Every command or option that does not run everywhere.
pub const TABLE: &[Support] = &[
    Support {
        name: "endpoint --pty",
        hosts: Hosts::MACOS_ONLY,
        hint: "use `endpoint --tcp`: rfc2217://127.0.0.1:<port> flashes and \
               socket://127.0.0.1:<port> monitors on every host",
    },
    // Device commands run on both native hosts in a build with feature `device`; a build without it
    // answers `E_HOST_UNSUPPORTED` naming the build that can. The rows make the docs and the
    // browser refusal say "native only": the web UI never opens a serial port. `plan_flash` and
    // `flash_device --dry-run` open nothing and run on every native host.
    Support {
        name: "device_boot_check",
        hosts: Hosts::NATIVE,
        hint: "run it with the native CLI on the macOS or Windows host the Passport is attached \
               to, built with feature `device`; `plan_flash` plans on every host",
    },
    Support {
        name: "flash_device",
        hosts: Hosts::NATIVE,
        hint: "a real flash runs with the native CLI on the macOS or Windows host the Passport is \
               attached to, built with feature `device`; `flash_device --dry-run` and \
               `plan_flash` plan, refuse and report on every native host, and they open nothing",
    },
];

pub fn hosts(name: &str) -> Hosts {
    match TABLE.iter().find(|row| row.name == name) {
        Some(row) => row.hosts,
        None => Hosts::ALL,
    }
}

pub fn hint(name: &str) -> Option<&'static str> {
    TABLE
        .iter()
        .find(|row| row.name == name)
        .map(|row| row.hint)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TableProblem {
    Duplicate(&'static str),
    /// The part before a space, if any.
    Unknown(&'static str),
    NoHost(&'static str),
    BrowserOnNativeOnly(&'static str),
    /// The refusal would name no alternative.
    NoHint(&'static str),
}

impl std::fmt::Display for TableProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TableProblem::Duplicate(name) => write!(f, "host_support row {name} appears twice"),
            TableProblem::Unknown(name) => write!(f, "host_support row {name} names no command"),
            TableProblem::NoHost(name) => write!(f, "host_support row {name} runs nowhere"),
            TableProblem::BrowserOnNativeOnly(name) => {
                write!(
                    f,
                    "host_support row {name} is native_only but claims the browser"
                )
            }
            TableProblem::NoHint(name) => write!(f, "host_support row {name} has an empty hint"),
        }
    }
}

/// A separate registry test, not part of `check_registry`.
pub fn check_table(specs: &[CommandSpec]) -> Result<(), TableProblem> {
    check_rows(TABLE, specs)
}

pub fn check_rows(rows: &[Support], specs: &[CommandSpec]) -> Result<(), TableProblem> {
    for (index, row) in rows.iter().enumerate() {
        if rows[..index].iter().any(|other| other.name == row.name) {
            return Err(TableProblem::Duplicate(row.name));
        }
        let command = row.name.split(' ').next().unwrap_or(row.name);
        let Some(spec) = specs.iter().find(|spec| spec.name == command) else {
            return Err(TableProblem::Unknown(row.name));
        };
        if !row.hosts.macos && !row.hosts.windows && !row.hosts.browser {
            return Err(TableProblem::NoHost(row.name));
        }
        if spec.annotations.native_only && row.hosts.browser {
            return Err(TableProblem::BrowserOnNativeOnly(row.name));
        }
        if row.hint.is_empty() {
            return Err(TableProblem::NoHint(row.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_the_registry() {
        check_table(crate::registry::commands()).expect("host_support table");
    }

    #[test]
    fn unlisted_names_run_everywhere() {
        assert_eq!(hosts("no-such-command"), Hosts::ALL);
        assert_eq!(hint("no-such-command"), None);
        assert!(Hosts::MACOS_ONLY.has(Host::MacOs));
        assert!(!Hosts::MACOS_ONLY.has(Host::Windows));
        assert!(!Hosts::NATIVE.has(Host::Browser));
    }

    #[test]
    fn the_current_host_is_in_the_matrix() {
        assert!(Host::current().is_some(), "host outside the support matrix");
    }
}
