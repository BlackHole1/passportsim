//! Tests of the hash file, the hashed rules, `--init` and `--self-test`.
//!
//! Every test runs on synthetic data under a fresh temporary HOME: the MAC uses the
//! `02:00:00` placeholder prefix, the backup stems are invented, and MAC-shaped text is built
//! at run time. No test reads the real device directory, backups or hash file. The helpers
//! are shared with `hook_tests.rs`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use pemu_api::secret_set::{self, MemberKind, SecretSet};

use super::device::{self, Env};
use super::hashed::{self, HashedHit, LoadError};
use super::patterns::CARDID_WINDOW;

/// Synthetic base MAC with the placeholder prefix.
pub(super) const MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x5e, 0x7a, 0x91];
/// Synthetic unique ID.
const UID: [u8; 16] = [
    0x3c, 0x91, 0x0e, 0x57, 0xa4, 0x28, 0x6b, 0xd3, 0x15, 0xf2, 0x88, 0x4e, 0xc7, 0x09, 0x6a, 0xb5,
];
/// Synthetic BLK2 words 4 to 7.
const CALIB: [u32; 4] = [0x2d3c_4b5a, 0x0000_7e81, 0x1357_9bdf, 0x0000_0000];
/// Invented backup stems.
pub(super) const STEMS: [&str; 2] = ["synthetic-backup-alpha", "synthetic-backup-beta"];

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A temporary directory, removed on drop.
pub(super) struct TempDir {
    pub(super) path: PathBuf,
}

impl TempDir {
    pub(super) fn new(tag: &str) -> TempDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("guard3-{tag}-{}-{unique}-{nanos}", std::process::id());
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).expect("create temp dir");
        // Canonical form, so `/var` and `/private/var` style aliases never differ.
        let path = path.canonicalize().expect("canonicalize temp dir");
        TempDir { path }
    }

    pub(super) fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, bytes).expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(super) fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// Lowercase two-digit hex octets of `bytes` joined by `sep`.
pub(super) fn hex_text(bytes: &[u8], sep: &str) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(sep)
}

/// The first 32-byte chunk of the synthetic cardid window.
pub(super) fn cardid_chunk() -> Vec<u8> {
    (0..32u8).map(|i| i.wrapping_mul(37) ^ 0x5a).collect()
}

fn block_bytes(index: u8) -> Vec<u8> {
    match index {
        1 => {
            let mut b = vec![0u8; 24];
            for (i, byte) in b.iter_mut().take(6).enumerate() {
                *byte = MAC[5 - i];
            }
            b[8] = 0x11;
            b
        }
        2 => {
            let mut b = vec![0u8; 32];
            b[..16].copy_from_slice(&UID);
            for (w, word) in CALIB.iter().enumerate() {
                b[16 + 4 * w..20 + 4 * w].copy_from_slice(&word.to_le_bytes());
            }
            b
        }
        0 => vec![0u8; 24],
        _ => vec![0u8; 32],
    }
}

/// A temporary HOME with a synthetic device directory (11 eFuse blocks) and two synthetic
/// backups, the first long enough to hold a cardid window.
pub(super) struct Home {
    pub(super) dir: TempDir,
    pub(super) env: Env,
}

pub(super) fn synthetic_home() -> Home {
    let dir = TempDir::new("home");
    let env = Env::from_home(&dir.path).expect("env");
    populate(&env);
    Home { dir, env }
}

/// Writes the synthetic eFuse blocks and backups where `env` looks for them.
fn populate(env: &Env) {
    for index in 0..device::EFUSE_BLOCKS {
        let path = env.device_dir.join(format!("efuse_blk{index}.bin"));
        std::fs::create_dir_all(&env.device_dir).expect("device dir");
        std::fs::write(path, block_bytes(index)).expect("write block");
    }
    let mut image = vec![0xFFu8; CARDID_WINDOW.end + 16];
    let start = CARDID_WINDOW.start;
    image[start..start + 32].copy_from_slice(&cardid_chunk());
    image[start + 64..start + 70].copy_from_slice(b"card-7");
    // A generic NVS page header on the window's second page is skipped, not hashed.
    let page = start + secret_set::NVS_PAGE_LEN;
    image[page..page + 32].copy_from_slice(&super::nvs::build_page(&[])[..32]);
    std::fs::create_dir_all(&env.backups_dir).expect("backups dir");
    std::fs::write(env.backups_dir.join(format!("{}.bin", STEMS[0])), image).expect("backup");
    std::fs::write(
        env.backups_dir.join(format!("{}.bin", STEMS[1])),
        [0x5au8; 64],
    )
    .expect("backup");
}

/// The Windows paths of the guard: the hash file in the roaming config directory, `$ROOT` in
/// the local data directory, the backups below the profile, and `--init` then `--self-test`
/// over a device directory there. Resolved from injected known folders under a fixture tree, so
/// it runs on both hosts and never reads a real folder.
#[test]
fn the_windows_column_runs_init_and_self_test_over_a_fixture_root() {
    let dir = TempDir::new("known-folders");
    let folders = crate::hostdirs::KnownFolders {
        profile: dir.path.join("Users").join("someone"),
        roaming_app_data: dir.path.join("Roaming"),
        local_app_data: dir.path.join("Local"),
    };
    let env = Env::from_folders(&folders).expect("env");
    assert_eq!(
        env.hash_file,
        dir.path
            .join("Roaming")
            .join("passportsim")
            .join("secrets-check.toml")
    );
    assert_eq!(
        env.device_dir,
        dir.path
            .join("Local")
            .join("passportsim")
            .join("data")
            .join("device")
    );
    assert_eq!(
        env.backups_dir,
        folders.profile.join("esp").join("passport-backups")
    );
    assert_eq!(
        Env::from_base(&crate::hostdirs::Base::Windows(folders.clone())).expect("env"),
        env
    );

    populate(&env);
    let out = device::init(&env).expect("init over the fixture root");
    assert!(out.contains("owner-only"), "{out}");
    pemu_host::platform::owner_only()
        .check(&env.hash_file)
        .expect("the hash file is owner-only");
    let out = device::self_test(&env).expect("self-test");
    assert!(out.contains("passed"), "{out}");

    let config = dir
        .path
        .join("Roaming")
        .join("passportsim")
        .join("config.toml");
    std::fs::write(&config, "[paths]\ndata_root = '~\\elsewhere'\n").expect("config");
    let moved = Env::from_folders(&folders).expect("env");
    assert_eq!(
        moved.device_dir,
        folders.profile.join("elsewhere").join("device")
    );
}

/// Fails when `output` contains a synthetic identity value or the canary.
pub(super) fn assert_no_identity(output: &str, canary: Option<&str>) {
    let mut needles = vec![
        hex_text(&MAC, ":"),
        hex_text(&MAC, "-"),
        hex_text(&MAC, ""),
        hex_text(&MAC[3..], ":"),
        hex_text(&UID, ""),
        STEMS[0].to_string(),
        STEMS[1].to_string(),
    ];
    needles.extend(canary.map(str::to_string));
    for (i, needle) in needles.iter().enumerate() {
        let upper = needle.to_uppercase();
        assert!(
            !output.contains(needle.as_str()) && !output.contains(upper.as_str()),
            "output leaks synthetic identity value number {i}"
        );
    }
}

#[test]
fn render_and_parse_round_trip() {
    let canary = hashed::new_canary().expect("canary");
    assert!(hashed::is_canary_shape(&canary));
    let mut builder = SecretSet::builder();
    builder.base_mac(MAC).canary(canary.as_bytes());
    let salted = builder.build().salted(&[0x42; 32]).expect("salted");
    let file = hashed::parse(&hashed::render(&salted, &canary)).expect("parse");
    assert_eq!(file.canary, canary);
    assert_eq!(file.salted, salted);
    let debug = format!("{file:?}");
    assert!(!debug.contains(&canary));
    assert!(!debug.contains(&secret_set::hex_string(&[0x42; 32])));
}

#[test]
fn parse_rejects_bad_files_without_quoting_them() {
    let canary = hashed::new_canary().expect("canary");
    let mut builder = SecretSet::builder();
    builder.base_mac(MAC).canary(canary.as_bytes());
    let good = hashed::render(&builder.build().salted(&[7; 32]).expect("salted"), &canary);
    let broken = format!("salt = \"{canary}\n[[member");
    let err = hashed::parse(&broken).expect_err("not TOML");
    assert!(!err.contains(&canary));
    let err = hashed::parse(&good.replace("version = 1", "version = 2")).expect_err("version");
    assert!(err.contains("version"), "{err}");
    let tampered = good.replacen("window_lens = [", "window_lens = [999, ", 1);
    assert!(
        hashed::parse(&tampered)
            .expect_err("lens")
            .contains("window_lens")
    );
    let mut no_canary = SecretSet::builder();
    no_canary.base_mac(MAC);
    let salted = no_canary.build().salted(&[7; 32]).expect("salted");
    let err = hashed::parse(&hashed::render(&salted, &canary)).expect_err("canary not hashed");
    assert!(err.contains("canary") && !err.contains(&canary), "{err}");
}

#[test]
fn init_writes_a_private_hash_file_and_prints_counts_only() {
    let home = synthetic_home();
    let out = device::init(&home.env).expect("init");
    let file = hashed::load(&home.env.hash_file).expect("load");
    assert_no_identity(&out, Some(&file.canary));
    assert!(!out.contains(&secret_set::hex_string(file.salted.salt())));
    assert!(
        out.contains(
            "read 11 of 11 eFuse block file(s), 2 backup(s) (1 cardid window(s), 0 skipped)"
        ),
        "{out}"
    );
    let kinds = file.salted.count_by_kind();
    for kind in [
        MemberKind::Mac,
        MemberKind::MacSuffix,
        MemberKind::UniqueId,
        MemberKind::CalibWord,
        MemberKind::BackupStem,
        MemberKind::CardId,
        MemberKind::Canary,
    ] {
        assert!(kinds.contains_key(&kind), "missing kind {kind}");
    }
    assert_eq!(kinds[&MemberKind::CardId], 2);
    assert!(
        out.contains("1 cardid chunk(s) skipped as NVS page headers"),
        "{out}"
    );
    // Owner-only on both hosts, through the product's own check; the mode bits are the macOS
    // spelling of it.
    pemu_host::platform::owner_only()
        .check(&home.env.hash_file)
        .expect("the hash file is owner-only");
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).expect("meta").permissions().mode();
        assert_eq!(mode(&home.env.hash_file) & 0o777, 0o600);
        assert_eq!(
            mode(home.env.hash_file.parent().expect("dir")) & 0o777,
            0o700
        );
    }
    let text = std::fs::read_to_string(&home.env.hash_file).expect("read");
    assert!(text.contains("[canary]") && text.contains(&file.canary));
    assert_no_identity(&text, None);

    let again = device::init(&home.env).expect("re-init");
    assert!(again.contains("replaced"), "{again}");
    let second = hashed::load(&home.env.hash_file).expect("load");
    assert_ne!(second.canary, file.canary);
    assert_ne!(second.salted.salt(), file.salted.salt());
}

#[test]
fn init_fails_on_missing_or_malformed_device_data() {
    let dir = TempDir::new("home-empty");
    let env = Env::from_home(&dir.path).expect("env");
    assert!(
        device::init(&env)
            .expect_err("no dir")
            .contains("no device directory")
    );
    std::fs::create_dir_all(&env.device_dir).expect("device dir");
    assert!(
        device::init(&env)
            .expect_err("no blocks")
            .contains("no efuse_blk")
    );
    std::fs::write(env.device_dir.join("efuse_blk1.bin"), [0u8; 23]).expect("short block");
    let err = device::init(&env).expect_err("bad length");
    assert!(err.contains("expected 24"), "{err}");
    assert!(!env.hash_file.exists());
}

#[test]
fn derive_counts_backups_and_skips_non_files() {
    let home = synthetic_home();
    std::fs::create_dir_all(home.env.backups_dir.join("folder.bin")).expect("dir");
    std::fs::write(home.env.backups_dir.join("notes.txt"), b"not a backup").expect("txt");
    let canary = "pemu-secrets-canary-00112233445566778899aabbccddeeff";
    let (set, sources) = device::derive(&home.env, canary).expect("derive");
    let expected = device::Sources {
        efuse_blocks: 11,
        backups: 2,
        cardid_windows: 1,
        backups_skipped: 1,
    };
    assert_eq!(sources, expected);
    assert_eq!(set.count_by_kind()[&MemberKind::BackupStem], 2);
    assert!(home.dir.path.join("esp/passport-backups").is_dir());
}

#[test]
fn self_test_passes_after_init_and_detects_changed_device_data() {
    let home = synthetic_home();
    let err = device::self_test(&home.env).expect_err("no hash file");
    assert!(err.contains("--init"), "{err}");
    device::init(&home.env).expect("init");
    let canary = hashed::load(&home.env.hash_file).expect("load").canary;
    let out = device::self_test(&home.env).expect("self-test");
    assert_no_identity(&out, Some(&canary));
    assert!(
        out.contains("canary probe detected") && out.contains("passed"),
        "{out}"
    );

    let blk1 = home.env.device_dir.join("efuse_blk1.bin");
    let mut bytes = std::fs::read(&blk1).expect("read blk1");
    bytes[0] ^= 0x0f;
    std::fs::write(&blk1, bytes).expect("write blk1");
    let err = device::self_test(&home.env).expect_err("stale hash file");
    assert!(err.contains("missed mac"), "{err}");
    assert_no_identity(&err, Some(&canary));
}

#[test]
fn hashed_scan_reports_kind_file_and_offset_only() {
    let home = synthetic_home();
    device::init(&home.env).expect("init");
    let file = hashed::load(&home.env.hash_file).expect("load");
    let tree = TempDir::new("tree");
    let line = format!("peer {}\n", hex_text(&MAC, "-").to_uppercase());
    tree.write("docs/notes.md", line.as_bytes());
    let mut blob = vec![0u8, 1, 2, 3, 4];
    blob.extend(cardid_chunk());
    blob.push(0);
    tree.write("assets/sample.dat", &blob);
    tree.write("clean.txt", b"nothing here\n");
    let rels = args(&[
        "docs/notes.md",
        "assets/sample.dat",
        "clean.txt",
        "missing.txt",
    ]);
    let report = hashed::scan_files(&tree.path, &rels, &file.salted).expect("scan");
    assert_eq!(report.files_scanned, 3);
    let hit = |kind, path: &str, offset| HashedHit {
        kind,
        path: path.to_string(),
        offset,
    };
    assert!(
        report
            .hits
            .contains(&hit(MemberKind::Mac, "docs/notes.md", 5))
    );
    assert!(
        report
            .hits
            .contains(&hit(MemberKind::MacSuffix, "docs/notes.md", 14))
    );
    assert!(
        report
            .hits
            .contains(&hit(MemberKind::CardId, "assets/sample.dat", 5))
    );
    assert!(report.hits.iter().all(|h| h.path != "clean.txt"));
    let text = report.render();
    assert!(
        text.contains("secrets-check: hashed:mac docs/notes.md:0x5\n"),
        "{text}"
    );
    assert!(
        text.contains("hashed:cardid assets/sample.dat:0x5"),
        "{text}"
    );
    assert_no_identity(&text, Some(&file.canary));
}

/// A missing file, a file wider than owner-only and a garbled one are each refused, on both hosts.
#[test]
fn load_refuses_missing_shared_or_garbled_files() {
    let home = synthetic_home();
    assert_eq!(
        hashed::load(&home.env.hash_file).expect_err("missing"),
        LoadError::Missing
    );
    device::init(&home.env).expect("init");
    let text = std::fs::read(&home.env.hash_file).expect("read");
    widen(&home.env.hash_file, &text);
    let err = hashed::load(&home.env.hash_file).expect_err("shared");
    assert!(err.to_string().contains("not owner-only"), "{err}");
    #[cfg(target_os = "macos")]
    assert!(err.to_string().contains("chmod 600"), "{err}");
    #[cfg(windows)]
    assert!(err.to_string().contains("--init"), "{err}");
    hashed::write(&home.env.hash_file, "salt = [[[\n").expect("garble, owner-only");
    let err = hashed::load(&home.env.hash_file).expect_err("garbled");
    assert!(
        matches!(&err, LoadError::Unreadable(why) if why.contains("TOML")),
        "{err}"
    );
}

/// Makes `path` hold `text` and be wider than owner-only, the way a copy or another tool leaves
/// it: group and other readable on macOS; on Windows a file created without the protected
/// descriptor, which carries its creator's default DACL and no `SE_DACL_PROTECTED`.
fn widen(path: &std::path::Path, text: &[u8]) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, text).expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::remove_file(path).expect("remove");
        std::fs::write(path, text).expect("a plain file");
    }
}

#[test]
fn env_honors_the_config_data_root() {
    let dir = TempDir::new("home-config");
    let plain = Env::from_home(&dir.path).expect("env");
    let default_root = "Library/Application Support/passportsim/device";
    assert_eq!(plain.device_dir, dir.path.join(default_root));
    assert_eq!(
        plain.hash_file,
        dir.path.join(".config/passportsim/secrets-check.toml")
    );
    assert_eq!(plain.backups_dir, dir.path.join("esp/passport-backups"));
    let config = ".config/passportsim/config.toml";
    dir.write(config, b"[paths]\ndata_root = \"~/custom/root\"\n");
    let custom = Env::from_home(&dir.path).expect("env");
    assert_eq!(custom.device_dir, dir.path.join("custom/root/device"));
    dir.write(config, b"[paths\n");
    assert!(Env::from_home(&dir.path).is_err());
}

#[test]
fn parse_args_and_run_with_dispatch_the_new_modes() {
    let mode = |list: &[&str]| super::parse_args(&args(list)).map(|opts| opts.mode);
    assert_eq!(mode(&["--init"]), Ok(super::Mode::Init));
    assert_eq!(mode(&["--self-test"]), Ok(super::Mode::SelfTest));
    assert_eq!(mode(&["--staged"]), Ok(super::Mode::Staged));
    assert_eq!(mode(&["--hook", "pre-commit"]), Ok(super::Mode::Staged));
    let push = mode(&["--hook", "pre-push", "origin", "/some/url"]);
    assert_eq!(push, Ok(super::Mode::PrePush));
    assert!(mode(&["--hook"]).is_err());
    assert!(mode(&["--hook", "post-merge"]).is_err());

    let home = synthetic_home();
    let opts = super::parse_args(&args(&["--init"])).expect("parse");
    super::run_with(opts, &home.env, None).expect("init");
    let opts = super::parse_args(&args(&["--self-test"])).expect("parse");
    super::run_with(opts, &home.env, None).expect("self-test");
}
