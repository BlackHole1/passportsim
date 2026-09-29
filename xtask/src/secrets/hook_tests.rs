//! Tests of the hook modes, the fail-closed rule and hook installation.
//!
//! Every repository is a fresh temporary `git init` without remotes; HOME, the device
//! directory and the hash file are synthetic (`hashed_tests.rs`). No test touches the real
//! repository's `.git`. MAC-shaped text is built at run time.
//!
//! These tests spawn hundreds of git children. Under x86_64 emulation by Rosetta (an `amd64`
//! image on an arm64 Mac) run them with `--test-threads=1`: Rosetta can deadlock the test
//! process when a child is spawned while another thread starts (orbstack/orbstack#380). No CI
//! tier runs emulated, so T0 does not set it.

use std::path::Path;
use std::process::Command;

use pemu_api::secret_set::MemberKind;

use super::device::{self, Env};
use super::hashed;
use super::hashed_tests::{TempDir, args, hex_text, synthetic_home};
use super::hooks::{self, HookState, Strictness};
use super::scan::Rule;

/// A unicast, non-placeholder address used only to plant pattern hits.
const PLANTED: [u8; 6] = [0x10, 0x20, 0x30, 0x4a, 0x5b, 0x6c];

/// Runs git in `dir` with a fixed identity, no signing and branch `main`; returns stdout.
fn git(dir: &Path, list: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=secrets-check test"])
        .args(["-c", "user.email=test@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(list)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {list:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo(tag: &str) -> TempDir {
    let dir = TempDir::new(tag);
    git(&dir.path, &["init", "-q"]);
    dir
}

fn commit_all(dir: &Path, message: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "--no-verify", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

/// A synthetic HOME with an initialized hash file, and its canary.
fn armed_home() -> (super::hashed_tests::Home, String) {
    let home = synthetic_home();
    device::init(&home.env).expect("init");
    let canary = hashed::load(&home.env.hash_file).expect("load").canary;
    (home, canary)
}

/// The hooks are written, kept and updated the same way on both hosts; the executable bit is the
/// macOS half only, because Git for Windows runs a hook by its `#!` line.
#[test]
fn hooks_install_is_executable_and_idempotent() {
    let repo = repo("install");
    let states = |force| -> Vec<HookState> {
        let done = hooks::install(&repo.path, force).expect("install");
        done.into_iter().map(|(_, state)| state).collect()
    };
    assert_eq!(states(false), [HookState::Installed, HookState::Installed]);
    let hook = |name: &str| repo.path.join(".git/hooks").join(name);
    for (name, check) in [
        ("pre-commit", "exec cargo xtask secrets-check --staged\n"),
        (
            "pre-push",
            "exec cargo xtask secrets-check --hook pre-push \"$@\"\n",
        ),
    ] {
        let text = std::fs::read_to_string(hook(name)).expect("hook file");
        assert!(text.starts_with("#!/bin/sh\n") && text.contains(hooks::HOOK_MARKER));
        assert!(text.ends_with(check), "{text}");
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(hook(name))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }
    assert_eq!(states(false), [HookState::Unchanged, HookState::Unchanged]);

    std::fs::write(
        hook("pre-commit"),
        "#!/bin/sh\n# pemu-secrets-guard: older\n",
    )
    .expect("old");
    assert_eq!(states(false), [HookState::Updated, HookState::Unchanged]);
    std::fs::write(hook("pre-push"), "#!/bin/sh\necho mine\n").expect("foreign");
    let err = hooks::install(&repo.path, false).expect_err("foreign hook kept");
    assert!(err.contains("--force"), "{err}");
    assert!(
        std::fs::read_to_string(hook("pre-push"))
            .expect("read")
            .contains("echo mine")
    );
    assert_eq!(states(true), [HookState::Unchanged, HookState::Updated]);

    let root = repo.path.to_string_lossy().into_owned();
    assert_eq!(
        super::run_hooks(&args(&["install", "--root", &root])),
        Ok(())
    );
    assert!(super::run_hooks(&[]).is_err());
    assert!(super::run_hooks(&args(&["uninstall"])).is_err());
    assert!(super::run_hooks(&args(&["install", "--root"])).is_err());
}

#[test]
fn installed_pre_commit_logic_refuses_a_staged_canary() {
    let (home, canary) = armed_home();
    let repo = repo("canary");
    hooks::install(&repo.path, false).expect("install");
    let script = std::fs::read_to_string(repo.path.join(".git/hooks/pre-commit")).expect("hook");
    let line = script
        .lines()
        .find_map(|l| l.strip_prefix("exec cargo xtask secrets-check "))
        .expect("exec line");
    let hook_args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    let hook_opts = || {
        let mut opts = super::parse_args(&hook_args).expect("parse hook args");
        opts.root = Some(repo.path.clone());
        opts
    };
    assert_eq!(hook_opts().mode, super::Mode::Staged);

    repo.write("scratch.txt", format!("{canary}\n").as_bytes());
    git(&repo.path, &["add", "scratch.txt"]);
    let err = super::run_with(hook_opts(), &home.env, None).expect_err("commit refused");
    assert!(
        err.contains("commit refused") && !err.contains(&canary),
        "{err}"
    );

    git(&repo.path, &["rm", "-q", "--cached", "scratch.txt"]);
    std::fs::remove_file(repo.path.join("scratch.txt")).expect("delete scratch");
    repo.write("README.md", b"clean\n");
    git(&repo.path, &["add", "README.md"]);
    assert_eq!(super::run_with(hook_opts(), &home.env, None), Ok(()));
}

#[test]
fn staged_rom_elf_is_checked_against_the_committed_pins() {
    let home = TempDir::new("home-rom");
    let env = Env::from_home(&home.path).expect("env");
    let repo = repo("rom");
    let elf = b"\x7fELF\x01\x01\x01\0synthetic rom image".to_vec();
    let pins = format!(
        "[[rom]]\nfile = \"a.elf\"\nsha256 = \"{}\"\n",
        super::rom::sha256_hex(&elf)
    );
    repo.write("assets/rom/pins.toml", pins.as_bytes());
    repo.write("assets/rom/LICENSE", b"license text\n");
    repo.write("assets/rom/NOTICE", b"notice text\n");
    commit_all(&repo.path, "pins");

    repo.write("assets/rom/a.elf", &elf);
    git(&repo.path, &["add", "assets/rom/a.elf"]);
    let scan = hooks::staged(&repo.path, &env).expect("scan");
    assert_eq!(scan.outcome.patterns.roms_pinned, 1);
    assert!(scan.outcome.verdict("refused").is_ok());

    repo.write("assets/rom/b.elf", b"\x7fELF\x01\x01\x01\0another image");
    git(&repo.path, &["add", "assets/rom/b.elf"]);
    let scan = hooks::staged(&repo.path, &env).expect("scan");
    assert_eq!(scan.outcome.patterns.count(Rule::RomPin), 1);
}

#[test]
fn staged_scan_refuses_the_canary_and_reads_blobs_not_the_work_tree() {
    let (home, canary) = armed_home();
    let repo = repo("staged");
    repo.write("README.md", b"clean\n");
    commit_all(&repo.path, "base");

    repo.write("scratch.txt", format!("canary {canary}\n").as_bytes());
    git(&repo.path, &["add", "scratch.txt"]);
    repo.write("scratch.txt", b"clean again\n");
    let scan = hooks::staged(&repo.path, &home.env).expect("scan");
    let hits = &scan.outcome.hashed.as_ref().expect("hashed rules ran").hits;
    assert!(
        hits.iter()
            .any(|h| h.kind == MemberKind::Canary && h.path == "scratch.txt" && h.offset == 7)
    );
    let err = scan.outcome.verdict("refused").expect_err("refused");
    assert!(err.contains("1 hashed rule hit"), "{err}");
    assert!(!scan.outcome.render().contains(&canary));

    git(&repo.path, &["add", "scratch.txt"]);
    repo.write("scratch.txt", format!("canary {canary}\n").as_bytes());
    let scan = hooks::staged(&repo.path, &home.env).expect("scan");
    assert_eq!(scan.outcome.hashed_hits(), 0);
    assert!(scan.outcome.verdict("refused").is_ok());
}

#[test]
fn staged_scan_before_the_first_commit_runs_pattern_rules_without_a_hash_file() {
    let home = TempDir::new("home-nodevice");
    let env = Env::from_home(&home.path).expect("env");
    let repo = repo("unborn");
    repo.write(
        "notes.md",
        format!("addr {}\n", hex_text(&PLANTED, ":")).as_bytes(),
    );
    repo.write("ok.md", b"placeholder 02:00:00:00:00:01\n");
    git(&repo.path, &["add", "notes.md", "ok.md"]);
    let scan = hooks::staged(&repo.path, &env).expect("no device dir: pattern rules only");
    assert!(scan.outcome.hashed.is_none());
    assert_eq!(scan.outcome.patterns.files_scanned, 2);
    assert_eq!(scan.outcome.patterns.count(Rule::MacShape), 1);
    assert!(scan.outcome.verdict("refused").is_err());
    let text = scan.outcome.render();
    assert!(
        text.contains("hashed rules: skipped (no hash file)"),
        "{text}"
    );
    assert!(!text.contains(&hex_text(&PLANTED, ":")));
}

#[test]
fn hook_modes_fail_closed_when_the_hash_file_is_missing_or_unreadable() {
    let home = synthetic_home();
    let repo = repo("failclosed");
    repo.write("a.txt", b"clean\n");
    git(&repo.path, &["add", "a.txt"]);

    let err = hooks::staged(&repo.path, &home.env).expect_err("missing hash file");
    assert!(
        err.contains("run cargo xtask secrets-check --init"),
        "{err}"
    );
    let err = hooks::pre_push(&repo.path, &home.env, None).expect_err("missing, pre-push");
    assert!(err.contains("--init"), "{err}");
    assert!(
        hooks::load_hash_file(&home.env, Strictness::Tree)
            .expect("tree mode skips")
            .is_none()
    );

    // Written owner-only on both hosts, so the refusal below is about the content.
    hashed::write(&home.env.hash_file, "version = [[[\n").expect("garbage");
    let err = hooks::staged(&repo.path, &home.env).expect_err("unreadable hash file");
    assert!(
        err.contains("unreadable") && err.contains("--init"),
        "{err}"
    );
    assert!(hooks::load_hash_file(&home.env, Strictness::Tree).is_err());
}

fn hits_canary(scan: &hooks::HookScan) -> bool {
    scan.outcome
        .hashed
        .as_ref()
        .is_some_and(|report| report.hits.iter().any(|h| h.kind == MemberKind::Canary))
}

fn zero_sha() -> String {
    "0".repeat(40)
}

#[test]
fn pre_push_scans_the_pushed_range_including_history() {
    let (home, canary) = armed_home();
    let repo = repo("push");
    repo.write("README.md", b"base\n");
    let base = commit_all(&repo.path, "base");
    repo.write("scratch.txt", format!("canary {canary}\n").as_bytes());
    let added = commit_all(&repo.path, "add");
    std::fs::remove_file(repo.path.join("scratch.txt")).expect("remove");
    let removed = commit_all(&repo.path, "remove");
    let push = |line: String| hooks::pre_push(&repo.path, &home.env, Some(&line)).expect("scan");

    let scan = push(format!(
        "refs/heads/main {removed} refs/heads/main {base}\n"
    ));
    assert!(
        scan.scope.starts_with("2 commit(s) and 0 tree(s)"),
        "{}",
        scan.scope
    );
    assert!(hits_canary(&scan) && scan.outcome.verdict("refused").is_err());

    let scan = push(format!(
        "refs/heads/main {removed} refs/heads/main {added}\n"
    ));
    assert!(scan.outcome.verdict("refused").is_ok(), "{}", scan.scope);

    let scan = push(format!(
        "(delete) {} refs/heads/old {removed}\n",
        zero_sha()
    ));
    assert!(
        scan.scope.starts_with("0 commit(s) and 0 tree(s)"),
        "{}",
        scan.scope
    );

    let scan = push(format!(
        "refs/heads/topic {removed} refs/heads/topic {}\n",
        zero_sha()
    ));
    assert!(
        scan.scope.starts_with("3 commit(s) and 1 tree(s)"),
        "{}",
        scan.scope
    );
    assert!(hits_canary(&scan));

    let unknown = "ab".repeat(20);
    let scan = push(format!(
        "refs/heads/main {removed} refs/heads/main {unknown}\n"
    ));
    assert!(
        scan.scope.starts_with("3 commit(s) and 1 tree(s)"),
        "{}",
        scan.scope
    );
    assert!(hits_canary(&scan));
}

#[test]
fn pre_push_manual_run_uses_the_upstream_range_or_the_tracked_tree() {
    let (home, canary) = armed_home();
    let repo = repo("manual");
    repo.write("README.md", b"base\n");
    commit_all(&repo.path, "base");
    repo.write("scratch.txt", format!("canary {canary}\n").as_bytes());
    let tip = commit_all(&repo.path, "add");

    let scan = hooks::pre_push(&repo.path, &home.env, None).expect("tree");
    assert!(
        scan.scope.starts_with("0 commit(s) and 1 tree(s)"),
        "{}",
        scan.scope
    );
    assert!(hits_canary(&scan));

    // A local branch stands in for the upstream, so the test needs no remote.
    git(&repo.path, &["branch", "-q", "published", &tip]);
    git(&repo.path, &["branch", "-q", "--set-upstream-to=published"]);
    let scan = hooks::pre_push(&repo.path, &home.env, Some("")).expect("empty range");
    assert!(
        scan.scope.starts_with("0 commit(s) and 0 tree(s)"),
        "{}",
        scan.scope
    );
    assert!(scan.outcome.verdict("refused").is_ok());

    repo.write("more.txt", format!("again {canary}\n").as_bytes());
    commit_all(&repo.path, "more");
    let scan = hooks::pre_push(&repo.path, &home.env, None).expect("range");
    assert!(
        scan.scope.starts_with("1 commit(s) and 0 tree(s)"),
        "{}",
        scan.scope
    );
    let hashed = scan.outcome.hashed.as_ref().expect("hashed");
    assert!(hashed.hits.iter().all(|h| h.path == "more.txt"));
    assert!(hits_canary(&scan));
}

#[test]
fn raw_and_listing_parsers_keep_blobs_only() {
    use super::gitsrc::{self, BlobEntry};
    let sha = |c: char| c.to_string().repeat(40);
    let entry = |path: &str, c: char| BlobEntry {
        path: path.to_string(),
        blob: sha(c),
    };
    let raw = format!(
        ":000000 100644 {z} {a} A\0new.txt\0:100644 000000 {b} {z} D\0gone.txt\0\
         :160000 160000 {b} {c} M\0sub\0:100644 100755 {b} {c} R087\0old.sh\0new.sh\0\
         :100644 120000 {b} {d} T\0link\0",
        z = sha('0'),
        a = sha('a'),
        b = sha('b'),
        c = sha('c'),
        d = sha('d')
    );
    assert_eq!(
        gitsrc::parse_raw(raw.as_bytes()),
        vec![
            entry("new.txt", 'a'),
            entry("new.sh", 'c'),
            entry("link", 'd')
        ]
    );
    let listing = format!(
        "100644 blob {a}\tassets/rom/pins.toml\0160000 commit {b}\tsub\0100644 {c} 0\tREADME.md\0",
        a = sha('a'),
        b = sha('b'),
        c = sha('c')
    );
    assert_eq!(
        gitsrc::parse_listing(listing.as_bytes()),
        vec![entry("assets/rom/pins.toml", 'a'), entry("README.md", 'c')]
    );
    let batches = gitsrc::batches(&[
        entry("x", 'a'),
        entry("y", 'b'),
        entry("x", 'c'),
        entry("x", 'a'),
    ]);
    assert_eq!(
        batches,
        vec![
            vec![entry("x", 'a'), entry("y", 'b')],
            vec![entry("x", 'c')]
        ]
    );
    let lines = "refs/heads/main aaa refs/heads/main bbb\n\nbogus line\n";
    assert_eq!(gitsrc::parse_push_lines(lines).len(), 1);
}
