//! Pins the paths the tooling resolves per directory role on macOS and Windows, as literal
//! segments, so a change of resolver implementation cannot move a receipt store, a data root or
//! the secrets guard's inputs unnoticed.

use std::path::PathBuf;

use crate::hostdirs::{Base, HostDirs, KnownFolders, Overrides, Role, data_root_of, host_base};
use crate::secrets::device::Env;

fn path(segments: &[&str]) -> PathBuf {
    segments.iter().collect()
}

fn folders() -> KnownFolders {
    KnownFolders {
        profile: PathBuf::from(r"C:\Users\u"),
        roaming_app_data: PathBuf::from(r"C:\Users\u\AppData\Roaming"),
        local_app_data: PathBuf::from(r"C:\Users\u\AppData\Local"),
    }
}

fn mac(overrides: Overrides) -> HostDirs {
    HostDirs::new(Ok(Base::Home(PathBuf::from("/Users/u"))), overrides)
}

fn win(overrides: Overrides) -> HostDirs {
    HostDirs::new(Ok(Base::Windows(folders())), overrides)
}

fn table(dirs: &HostDirs) -> Vec<(Role, PathBuf)> {
    Role::ALL
        .iter()
        .map(|&role| (role, dirs.role(role).expect("resolves")))
        .collect()
}

#[test]
fn the_macos_column_is_pinned() {
    let home = "/Users/u";
    let data = [home, "Library", "Application Support", "passportsim"];
    assert_eq!(
        table(&mac(Overrides::default())),
        vec![
            (Role::Home, path(&[home])),
            (Role::Config, path(&[home, ".config", "passportsim"])),
            (Role::DataRoot, path(&data)),
            (
                Role::Cache,
                path(&[home, "Library", "Caches", "passportsim"])
            ),
            (Role::Runtime, path(&[home, ".passportsim"])),
            (Role::Logs, path(&[home, ".passportsim", "logs"])),
            (Role::Artifacts, path(&data).join("artifacts")),
        ]
    );
}

#[test]
fn the_windows_column_is_pinned() {
    let local = r"C:\Users\u\AppData\Local";
    assert_eq!(
        table(&win(Overrides::default())),
        vec![
            (Role::Home, path(&[r"C:\Users\u"])),
            (
                Role::Config,
                path(&[r"C:\Users\u\AppData\Roaming", "passportsim"])
            ),
            (Role::DataRoot, path(&[local, "passportsim", "data"])),
            (Role::Cache, path(&[local, "passportsim", "cache"])),
            (Role::Runtime, path(&[local, "passportsim", "run"])),
            (Role::Logs, path(&[local, "passportsim", "logs"])),
            (
                Role::Artifacts,
                path(&[local, "passportsim", "data", "artifacts"])
            ),
        ]
    );
}

#[test]
fn the_overrides_are_pinned_on_both_columns() {
    let relocated = Overrides {
        home: Some("/srv/emu".to_string()),
        ..Overrides::default()
    };
    let expected = vec![
        (Role::Home, path(&["/srv/emu", "home"])),
        (Role::Config, path(&["/srv/emu", "config"])),
        (Role::DataRoot, path(&["/srv/emu", "data"])),
        (Role::Cache, path(&["/srv/emu", "cache"])),
        (Role::Runtime, path(&["/srv/emu", "run"])),
        (Role::Logs, path(&["/srv/emu", "logs"])),
        (Role::Artifacts, path(&["/srv/emu", "data", "artifacts"])),
    ];
    assert_eq!(table(&mac(relocated.clone())), expected);
    assert_eq!(table(&win(relocated)), expected);

    let narrow = Overrides {
        config_dir: Some("~/cfg".to_string()),
        data_root: Some("~\\data".to_string()),
        ..Overrides::default()
    };
    let m = mac(narrow.clone());
    assert_eq!(m.role(Role::Config), Ok(path(&["/Users/u", "cfg"])));
    assert_eq!(m.role(Role::DataRoot), Ok(path(&["/Users/u", "data"])));
    assert_eq!(
        m.role(Role::Artifacts),
        Ok(path(&["/Users/u", "data", "artifacts"]))
    );
    assert_eq!(
        m.role(Role::Cache),
        Ok(path(&["/Users/u", "Library", "Caches", "passportsim"]))
    );
    let w = win(narrow);
    assert_eq!(w.role(Role::Config), Ok(path(&[r"C:\Users\u", "cfg"])));
    assert_eq!(w.role(Role::DataRoot), Ok(path(&[r"C:\Users\u", "data"])));
}

#[test]
fn the_tooling_data_root_is_pinned() {
    let config = Some("[paths]\ndata_root = \"~/corpus-root\"\n");
    let silent = Some("[efuse]\ndefault = \"synth\"\n");
    let m = mac(Overrides::default());
    assert_eq!(
        data_root_of(&m, None),
        Ok(path(&[
            "/Users/u",
            "Library",
            "Application Support",
            "passportsim"
        ]))
    );
    assert_eq!(data_root_of(&m, silent), data_root_of(&m, None));
    assert_eq!(
        data_root_of(&m, config),
        Ok(path(&["/Users/u", "corpus-root"]))
    );
    let w = win(Overrides::default());
    assert_eq!(
        data_root_of(&w, None),
        Ok(path(&[r"C:\Users\u\AppData\Local", "passportsim", "data"]))
    );
    assert_eq!(
        data_root_of(&w, config),
        Ok(path(&[r"C:\Users\u", "corpus-root"]))
    );
    let claimed = mac(Overrides {
        data_root: Some("/data/pemu".to_string()),
        ..Overrides::default()
    });
    assert_eq!(data_root_of(&claimed, config), Ok(path(&["/data/pemu"])));
    let config_moved = mac(Overrides {
        config_dir: Some("/etc/pemu".to_string()),
        ..Overrides::default()
    });
    assert_eq!(
        data_root_of(&config_moved, config),
        Ok(path(&["/Users/u", "corpus-root"]))
    );
    assert!(data_root_of(&m, Some("[paths]\ndata_root = 3\n")).is_err());
}

/// The secrets guard's inputs per column, with no `config.toml` below these fixture bases.
#[test]
fn the_secrets_guard_paths_are_pinned() {
    let home = "/nonexistent-pemu-pin/u";
    let env = Env::from_base(&Base::Home(PathBuf::from(home))).expect("macOS env");
    assert_eq!(
        env.hash_file,
        path(&[home, ".config", "passportsim", "secrets-check.toml"])
    );
    assert_eq!(
        env.device_dir,
        path(&[
            home,
            "Library",
            "Application Support",
            "passportsim",
            "device"
        ])
    );
    assert_eq!(env.backups_dir, path(&[home, "esp", "passport-backups"]));

    let env = Env::from_base(&Base::Windows(KnownFolders {
        profile: PathBuf::from(r"Z:\nonexistent-pemu-pin\u"),
        roaming_app_data: PathBuf::from(r"Z:\nonexistent-pemu-pin\Roaming"),
        local_app_data: PathBuf::from(r"Z:\nonexistent-pemu-pin\Local"),
    }))
    .expect("Windows env");
    assert_eq!(
        env.hash_file,
        path(&[
            r"Z:\nonexistent-pemu-pin\Roaming",
            "passportsim",
            "secrets-check.toml"
        ])
    );
    assert_eq!(
        env.device_dir,
        path(&[
            r"Z:\nonexistent-pemu-pin\Local",
            "passportsim",
            "data",
            "device"
        ])
    );
    assert_eq!(
        env.backups_dir,
        path(&[r"Z:\nonexistent-pemu-pin\u", "esp", "passport-backups"])
    );
}

/// A test build resolves no real directory and reads no override.
#[test]
fn a_test_build_resolves_nothing_from_the_process() {
    assert!(host_base().is_err());
    let dirs = HostDirs::from_process();
    for role in Role::ALL {
        assert!(dirs.role(role).is_err(), "{role} resolved in a test build");
    }
    assert!(crate::hostdirs::home().is_err());
    assert!(crate::hostdirs::receipts_dir().is_err());
}
