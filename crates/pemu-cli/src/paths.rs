//! The one host path the CLI resolves for a command, and its redaction.
//!
//! `pemu-api` may not read a path, so a command that reports one is handed it resolved. Exactly one
//! output carries an absolute path: `status`'s artifact root, with the home directory redacted.
//! When a command's input schema has an `artifacts_root` property the caller did not set, the CLI
//! fills it in. The root is `Role::Artifacts` (`<data root>/artifacts/`) unless `--artifacts <dir>`
//! moves it.

use std::path::Path;

use pemu_host::paths::{Env, HostPaths, Overrides, Role};

#[must_use]
pub fn resolver(artifacts: Option<&str>) -> HostPaths {
    let env = Env::from_process();
    let mut overrides: Overrides = env.overrides().clone();
    overrides.artifacts = artifacts
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty());
    HostPaths::new(Env::new(env.base().cloned(), overrides))
}

/// `None` when this host has no base to resolve it against, which is a real answer, not a panic.
#[must_use]
pub fn artifacts_root(paths: &HostPaths) -> Option<String> {
    let root = paths.role(Role::Artifacts).ok()?;
    let home = paths.home().ok();
    Some(redact(&root, home.as_deref()))
}

/// A leading `home` becomes `~`, in native form: a Windows root reads
/// `~\AppData\Local\passportsim\data\artifacts`. Every other output uses relative forward-slashed
/// paths. `Path::strip_prefix` matches whole components, so `/Users/alice2` is never a child of
/// `/Users/alice`, and a Windows drive or UNC prefix must match whole.
#[must_use]
pub fn redact(path: &Path, home: Option<&Path>) -> String {
    let text = path.to_string_lossy().into_owned();
    let Some(home) = home.filter(|home| !home.as_os_str().is_empty()) else {
        return text;
    };
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.to_string_lossy()),
        Err(_) => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    /// So no test reads the process environment.
    fn injected(home: &str, artifacts: Option<&str>) -> HostPaths {
        HostPaths::new(Env::macos(
            PathBuf::from(home),
            Overrides {
                artifacts: artifacts.map(str::to_owned),
                ..Overrides::default()
            },
        ))
    }

    #[test]
    fn the_root_is_reported_with_the_home_directory_redacted() {
        let paths = injected("/Users/example", None);
        let root = artifacts_root(&paths).expect("macOS resolves every role");
        // The running host's own separator.
        let sep = std::path::MAIN_SEPARATOR;
        assert!(root.starts_with(&format!("~{sep}")), "{root}");
        assert!(!root.contains("example"), "{root}");
        assert!(root.ends_with("artifacts"), "{root}");
    }

    #[test]
    fn the_artifacts_flag_moves_the_root() {
        let paths = injected("/Users/example", Some("/tmp/runs"));
        assert_eq!(
            artifacts_root(&paths).as_deref(),
            Some("/tmp/runs"),
            "a root outside home is not redacted"
        );
    }

    #[test]
    fn redaction_is_a_whole_component_prefix() {
        let home = PathBuf::from("/Users/alice");
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            redact(Path::new("/Users/alice/x"), Some(&home)),
            format!("~{sep}x")
        );
        assert_eq!(redact(Path::new("/Users/alice"), Some(&home)), "~");
        assert_eq!(
            redact(Path::new("/Users/alice2/x"), Some(&home)),
            "/Users/alice2/x",
            "a sibling whose name starts the same is not the home directory"
        );
        assert_eq!(redact(Path::new("/opt/x"), None), "/opt/x");
    }

    /// Windows paths parse only on Windows, so each host asserts its own spelling; neither arm is
    /// skipped.
    #[test]
    fn the_root_is_reported_in_the_hosts_native_form() {
        if cfg!(windows) {
            let home = Path::new(r"C:\Users\example");
            assert_eq!(
                redact(
                    Path::new(r"C:\Users\example\AppData\Local\passportsim\data\artifacts"),
                    Some(home)
                ),
                r"~\AppData\Local\passportsim\data\artifacts"
            );
            assert_eq!(
                redact(Path::new(r"D:\Users\example\x"), Some(home)),
                r"D:\Users\example\x",
                "another drive is not the home directory"
            );
            assert_eq!(
                redact(Path::new(r"C:\Users\example2\x"), Some(home)),
                r"C:\Users\example2\x"
            );
        } else {
            assert_eq!(
                redact(
                    Path::new("/Users/example/Library/Application Support/passportsim/artifacts"),
                    Some(Path::new("/Users/example/"))
                ),
                "~/Library/Application Support/passportsim/artifacts"
            );
        }
    }
}
