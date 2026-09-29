//! The packaging account kept out of the shipped artifacts.
//!
//! rustc writes the source path of every panic location into what it compiles, so an unremapped
//! package carries the builder's home. [`Remap`] passes one `--remap-path-prefix` per prefix,
//! widest first because rustc applies the last match, as `target.<triple>.rustflags` (never
//! `RUSTFLAGS`). The host's separator survives after the token, so the wasm core still differs by
//! host and `--payload-from` stays ([`super::source`]).
//!
//! [`audit`] then searches every shipped artifact, ignoring ASCII case, for this account and for
//! any account's profile path; one hit fails the packaging. The receipt records tokens and counts,
//! never the strings.

use std::path::{Path, PathBuf};

/// The token the workspace root is remapped to.
pub const WORKSPACE_TOKEN: &str = "/pemu";

/// The token the cargo home is remapped to.
pub const CARGO_HOME_TOKEN: &str = "/cargo";

/// The token a target directory outside the workspace root is remapped to.
pub const TARGET_DIR_TOKEN: &str = "/pemu-target";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub home: PathBuf,
    pub user: Option<String>,
}

impl Account {
    /// The account of this process: `HOME` (`USERPROFILE` first on Windows) and `USER` or `USERNAME`.
    pub fn of_process() -> Result<Account, String> {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let home = if cfg!(windows) {
            var("USERPROFILE").or_else(|| var("HOME"))
        } else {
            var("HOME")
        }
        .ok_or_else(|| {
            "the packaging account has no home directory in the environment (HOME, or USERPROFILE \
             on Windows), so neither the cargo home to remap nor the path to audit for is known"
                .to_string()
        })?;
        Ok(Account {
            home: PathBuf::from(home),
            user: var("USER").or_else(|| var("USERNAME")),
        })
    }

    /// The byte strings of this account the audit refuses, lowercase.
    pub fn needles(&self) -> Vec<Vec<u8>> {
        let home = self
            .home
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string();
        let mut needles = Vec::new();
        if home.len() > 1 {
            needles.push(home.replace('\\', "/"));
            needles.push(home.replace('/', "\\"));
        }
        if let Some(user) = &self.user {
            for parent in ["users/", "users\\", "home/"] {
                needles.push(format!("{parent}{user}"));
            }
        }
        let mut needles: Vec<Vec<u8>> = needles
            .into_iter()
            .map(|needle| needle.to_ascii_lowercase().into_bytes())
            .collect();
        needles.sort();
        needles.dedup();
        needles
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remap {
    /// (what the prefix is, the prefix, its token), widest first.
    pub prefixes: Vec<(&'static str, PathBuf, String)>,
}

impl Remap {
    /// The remapping for a build of the workspace at `root` into `target_dir` by `account`, with
    /// the toolchain `rustc` in `root` reports.
    pub fn new(root: &Path, target_dir: &Path, account: &Account) -> Result<Remap, String> {
        let (sysroot, commit) = toolchain(root)?;
        Ok(Remap::from_parts(
            root,
            target_dir,
            &cargo_home(account),
            &sysroot,
            &commit,
        ))
    }

    pub fn from_parts(
        root: &Path,
        target_dir: &Path,
        cargo_home: &Path,
        sysroot: &Path,
        commit: &str,
    ) -> Remap {
        let mut prefixes = vec![
            (
                "cargo_home",
                cargo_home.to_path_buf(),
                CARGO_HOME_TOKEN.to_string(),
            ),
            (
                "toolchain_source",
                sysroot.join("lib").join("rustlib").join("src").join("rust"),
                format!("/rustc/{commit}"),
            ),
            ("workspace", root.to_path_buf(), WORKSPACE_TOKEN.to_string()),
        ];
        if !target_dir.starts_with(root) {
            prefixes.push((
                "target_dir",
                target_dir.to_path_buf(),
                TARGET_DIR_TOKEN.to_string(),
            ));
        }
        Remap { prefixes }
    }

    pub fn rustflags(&self) -> Vec<String> {
        self.prefixes
            .iter()
            .map(|(_, from, to)| format!("--remap-path-prefix={}={to}", from.display()))
            .collect()
    }
}

/// `target.<target>.rustflags=[...]` for `cargo --config`, as TOML literal strings so a Windows
/// path goes in as written. A flag holding `'` or a line break is refused.
pub fn rustflags_config(target: &str, flags: &[String]) -> Result<String, String> {
    let mut items = Vec::new();
    for flag in flags {
        if flag.contains(['\'', '\n', '\r']) {
            return Err(format!(
                "the rustc flag {flag:?} cannot be written as a TOML literal string for \
                 `cargo --config`; move the checkout or the cargo home to a path without `'`"
            ));
        }
        items.push(format!("'{flag}'"));
    }
    Ok(format!("target.{target}.rustflags=[{}]", items.join(", ")))
}

/// The cargo home: `CARGO_HOME`, else `<home>/.cargo` (the cargo book, "Cargo Home").
fn cargo_home(account: &Account) -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| account.home.join(".cargo"))
}

/// The sysroot and commit hash of the `rustc` cargo would run in `root` (`RUSTC`, else `rustc`,
/// which the toolchain file of the workspace pins).
fn toolchain(root: &Path) -> Result<(PathBuf, String), String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let run = |args: &[&str]| -> Result<String, String> {
        let output = std::process::Command::new(&rustc)
            .args(args)
            .current_dir(root)
            .output()
            .map_err(|e| format!("cannot run `{rustc} {}`: {e}", args.join(" ")))?;
        if !output.status.success() {
            return Err(format!(
                "`{rustc} {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let sysroot = PathBuf::from(run(&["--print", "sysroot"])?);
    let version = run(&["-vV"])?;
    let commit = version
        .lines()
        .find_map(|line| line.strip_prefix("commit-hash: "))
        .map(str::trim)
        .filter(|hash| !hash.is_empty() && *hash != "unknown")
        .ok_or_else(|| format!("`{rustc} -vV` names no commit-hash:\n{version}"))?;
    Ok((sysroot, commit.to_string()))
}

/// The offsets in `bytes` where one of `needles` (lowercase) starts, ignoring ASCII case, and not
/// inside one of `skip`. Overlapping matches are one hit (`\users\name` and `users\name`).
pub fn hits(bytes: &[u8], needles: &[Vec<u8>], skip: &[std::ops::Range<usize>]) -> Vec<usize> {
    let mut found: Vec<std::ops::Range<usize>> = Vec::new();
    for needle in needles {
        let Some(&first) = needle.first() else {
            continue;
        };
        let mut at = 0;
        while at + needle.len() <= bytes.len() {
            if bytes[at].to_ascii_lowercase() == first
                && bytes[at..at + needle.len()]
                    .iter()
                    .zip(needle)
                    .all(|(byte, want)| byte.to_ascii_lowercase() == *want)
                && !skip.iter().any(|span| span.contains(&at))
            {
                found.push(at..at + needle.len());
            }
            at += 1;
        }
    }
    found.sort_unstable_by_key(|span| (span.start, span.end));
    let mut starts = Vec::new();
    let mut reach = 0;
    for span in found {
        if starts.is_empty() || span.start >= reach {
            starts.push(span.start);
        }
        reach = reach.max(span.end);
    }
    starts
}

/// The profile directories of any account, lowercase, on either host.
const PROFILE_PARENTS: [&[u8]; 3] = [b"/users/", b"\\users\\", b"/home/"];

fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'$')
}

/// The profile paths of any account in `bytes`: `Users/<name>` in `/Users/<name>/...`, without the
/// leading separator.
pub fn profile_path_spans(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
    let needles: Vec<Vec<u8>> = PROFILE_PARENTS.iter().map(|p| p.to_vec()).collect();
    hits(bytes, &needles, &[])
        .into_iter()
        .filter_map(|at| {
            let parent = PROFILE_PARENTS.iter().find(|p| {
                bytes
                    .get(at..at + p.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(p))
            })?;
            let name = at + parent.len();
            let end = bytes[name..]
                .iter()
                .position(|&b| !name_byte(b))
                .map_or(bytes.len(), |n| name + n);
            (end > name).then_some(at + 1..end)
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audit {
    /// (what, token) for every remapped prefix.
    pub remapped: Vec<(String, String)>,
    pub searched: Vec<String>,
}

pub struct Artifact<'a> {
    pub label: String,
    pub bytes: &'a [u8],
}

/// Searches `artifacts` for `account` and for any account's profile path. The first artifact with
/// a hit fails it.
pub fn audit(
    account: &Account,
    remap: &Remap,
    artifacts: &[Artifact<'_>],
) -> Result<Audit, String> {
    let needles = account.needles();
    let mut searched = Vec::new();
    for artifact in artifacts {
        // One path is one hit, whichever rules found it: `/users/<name>` of this account is also a
        // profile path, one byte further in.
        let shortest = needles.iter().map(Vec::len).min().unwrap_or(1);
        let mut spans: Vec<std::ops::Range<usize>> = hits(artifact.bytes, &needles, &[])
            .into_iter()
            .map(|at| at..at + shortest)
            .chain(
                profile_path_spans(artifact.bytes)
                    .into_iter()
                    .map(|span| span.start.saturating_sub(1)..span.end),
            )
            .collect();
        spans.sort_unstable_by_key(|span| (span.start, span.end));
        let mut found: Vec<usize> = Vec::new();
        let mut reach = 0;
        for span in spans {
            if found.is_empty() || span.start >= reach {
                found.push(span.start);
            }
            reach = reach.max(span.end);
        }
        if let Some(&at) = found.first() {
            let end = (at + 96).min(artifact.bytes.len());
            let context = String::from_utf8_lossy(&artifact.bytes[at..end]);
            let context = context.split(['\0', '\n']).next().unwrap_or_default();
            return Err(format!(
                "{} names an account {} time(s), first at byte {at}: {context:?}. A shipped \
                 artifact must not name who built it or its inputs; the remapped \
                 prefixes are {}, and the demo ELF's profile paths are blanked \
                 (`package/elf_paths.rs`), so this path came from somewhere else",
                artifact.label,
                found.len(),
                remap
                    .prefixes
                    .iter()
                    .map(|(what, _, _)| *what)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        searched.push(artifact.label.clone());
    }
    Ok(Audit {
        remapped: remap
            .prefixes
            .iter()
            .map(|(what, _, to)| (what.to_string(), to.clone()))
            .collect(),
        searched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account {
            home: PathBuf::from("/Users/someone"),
            user: Some("someone".to_string()),
        }
    }

    #[test]
    fn the_needles_are_the_home_both_ways_and_the_name_below_a_profile_directory() {
        let needles: Vec<String> = account()
            .needles()
            .into_iter()
            .map(|n| String::from_utf8(n).expect("utf-8"))
            .collect();
        assert_eq!(
            needles,
            [
                "/users/someone",
                "\\users\\someone",
                "home/someone",
                "users/someone",
                "users\\someone"
            ]
        );
        let windows = Account {
            home: PathBuf::from("C:\\Users\\Live\\"),
            user: None,
        };
        let needles = windows.needles();
        assert!(needles.contains(&b"c:/users/live".to_vec()));
        assert!(needles.contains(&b"c:\\users\\live".to_vec()));
    }

    #[test]
    fn hits_ignore_case_and_skip_the_spans_given() {
        let bytes = b"xx C:\\USERS\\Someone\\.cargo yy /users/someone/ zz";
        let needles = account().needles();
        let all = hits(bytes, &needles, &[]);
        assert_eq!(all, [5, 30]);
        let skip = [0..10, 100..110];
        assert_eq!(hits(bytes, &needles, &skip), [30]);
        assert!(hits(b"src/someone.rs /Users/someones", &needles, &[]).len() == 1);
    }

    #[test]
    fn profile_path_spans_cover_the_parent_and_the_name_of_any_account() {
        let bytes = b"/Users/a.b/x C:\\Users\\b-c\\y /home/c /Users/ /users/<name>";
        let spans: Vec<&[u8]> = profile_path_spans(bytes)
            .into_iter()
            .map(|span| &bytes[span])
            .collect();
        assert_eq!(
            spans,
            [
                b"Users/a.b".as_slice(),
                b"Users\\b-c".as_slice(),
                b"home/c".as_slice()
            ]
        );
    }

    #[test]
    fn the_remap_runs_widest_first_and_adds_an_outside_target_dir() {
        let remap = Remap::from_parts(
            Path::new("/w/pemu"),
            Path::new("/w/pemu/target"),
            Path::new("/h/.cargo"),
            Path::new("/h/.rustup/toolchains/t"),
            "abc123",
        );
        // The toolchain prefix is joined the host's way, as rustc will see it.
        let rust_src = Path::new("/h/.rustup/toolchains/t")
            .join("lib")
            .join("rustlib")
            .join("src")
            .join("rust");
        assert_eq!(
            remap.rustflags(),
            [
                "--remap-path-prefix=/h/.cargo=/cargo".to_string(),
                format!("--remap-path-prefix={}=/rustc/abc123", rust_src.display()),
                "--remap-path-prefix=/w/pemu=/pemu".to_string(),
            ]
        );
        let outside = Remap::from_parts(
            Path::new("/w/pemu"),
            Path::new("/t"),
            Path::new("/h/.cargo"),
            Path::new("/s"),
            "abc123",
        );
        assert_eq!(
            outside.rustflags().last().map(String::as_str),
            Some("--remap-path-prefix=/t=/pemu-target")
        );
    }

    #[test]
    fn the_config_is_literal_strings_for_one_target() {
        let flags = vec![
            "-C".to_string(),
            "--remap-path-prefix=C:\\src\\x=/src".to_string(),
        ];
        assert_eq!(
            rustflags_config("x86_64-pc-windows-msvc", &flags).expect("config"),
            "target.x86_64-pc-windows-msvc.rustflags=['-C', \
             '--remap-path-prefix=C:\\src\\x=/src']"
        );
        assert!(rustflags_config("t", &["a'b".to_string()]).is_err());
    }

    #[test]
    fn the_audit_refuses_this_account_and_any_profile_path_in_any_artifact() {
        let remap = Remap::from_parts(
            Path::new("/w"),
            Path::new("/w/target"),
            Path::new("/c"),
            Path::new("/s"),
            "h",
        );
        let clean = b"code /pemu/crates/x.rs /cargo/registry/src/y.rs /_________/esp/z.c".to_vec();
        let ok = audit(
            &account(),
            &remap,
            &[
                Artifact {
                    label: "passportsim".to_string(),
                    bytes: &clean,
                },
                Artifact {
                    label: "web/main.js".to_string(),
                    bytes: b"console.log(1)",
                },
            ],
        )
        .expect("nothing to find");
        assert_eq!(ok.searched, ["passportsim", "web/main.js"]);

        let mine = b"panic at /users/SOMEONE/.cargo/registry/src/x.rs".to_vec();
        let refused = audit(
            &account(),
            &remap,
            &[Artifact {
                label: "passportsim".to_string(),
                bytes: &mine,
            }],
        )
        .expect_err("this account");
        assert!(
            refused.contains("passportsim names an account 1 time(s)"),
            "{refused}"
        );

        let theirs = b"DW_AT_comp_dir /Users/builder/esp/idf".to_vec();
        let refused = audit(
            &account(),
            &remap,
            &[Artifact {
                label: "payload/firmware/official.pebundle".to_string(),
                bytes: &theirs,
            }],
        )
        .expect_err("another account's profile path");
        assert!(
            refused.contains("official.pebundle names an account"),
            "{refused}"
        );
    }
}
