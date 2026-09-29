//! Glob expansion, done by the program itself, because no Windows shell expands globs for a
//! program. Every path operand goes through [`expand`], so a pattern that works in `zsh` works in
//! `cmd.exe`.
//!
//! | Pattern | Matches |
//! |---|---|
//! | `?` | exactly one character inside a segment, never a separator |
//! | `*` | any run of characters inside a segment, never a separator |
//! | `**` | any number of whole segments, including none |
//!
//! Both separators are accepted on both hosts. Results are sorted by their forward-slashed
//! spelling, so a batch runs in the same order everywhere. A pattern with no metacharacter is
//! returned as itself, so the receiving command reports the missing file rather than "no match".
//! Used for `--json @<pattern>`, which must name exactly one document.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[must_use]
pub fn is_pattern(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

/// The sort key and the form every output uses.
#[must_use]
pub fn slashed(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A pattern with no metacharacter is returned unchanged; one with a metacharacter returns every
/// matching file, possibly none.
#[must_use]
pub fn expand(pattern: &str) -> Vec<PathBuf> {
    if !is_pattern(pattern) {
        return vec![PathBuf::from(pattern)];
    }
    let (root, segments) = split_root(pattern);
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    let mut found = Vec::new();
    walk(&root, &segments, &mut found);
    found.sort_by_key(|path| slashed(path));
    found.dedup();
    found
}

/// The root is whatever the host's path parser calls a prefix and root directory: `/` on macOS; on
/// Windows a drive (`C:\`), a UNC share or a verbatim prefix, so an absolute pattern never becomes
/// the drive-relative `C:Users`. `C:*.yaml` stays drive-relative. On macOS a backslash becomes `/`
/// first. A `.` segment is dropped and `..` is kept as a literal.
fn split_root(pattern: &str) -> (PathBuf, Vec<String>) {
    let native = if cfg!(windows) {
        pattern.to_owned()
    } else {
        pattern.replace('\\', "/")
    };
    let mut root = PathBuf::new();
    let mut segments = Vec::new();
    for component in Path::new(&native).components() {
        match component {
            Component::Prefix(_) | Component::RootDir => root.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => segments.push("..".to_owned()),
            Component::Normal(segment) => segments.push(segment.to_string_lossy().into_owned()),
        }
    }
    (root, segments)
}

fn walk(base: &Path, segments: &[&str], found: &mut Vec<PathBuf>) {
    let Some((head, rest)) = segments.split_first() else {
        if base.as_os_str().is_empty() || base.exists() {
            found.push(base.to_path_buf());
        }
        return;
    };
    match *head {
        // `**` matches zero or more whole segments.
        "**" => {
            walk(base, rest, found);
            for entry in children(base) {
                walk(&entry, segments, found);
            }
        }
        // Joined without touching the file system, so a deep literal path costs one `read_dir` per
        // segment that has a metacharacter.
        literal if !is_pattern(literal) => {
            let next = join(base, literal);
            if rest.is_empty() {
                if next.exists() {
                    found.push(next);
                }
            } else if next.is_dir() {
                walk(&next, rest, found);
            }
        }
        pattern => {
            for entry in children(base) {
                let name = entry
                    .file_name()
                    .map(OsStr::to_string_lossy)
                    .unwrap_or_default()
                    .to_string();
                if !matches(pattern, &name) {
                    continue;
                }
                if rest.is_empty() {
                    found.push(entry);
                } else if entry.is_dir() {
                    walk(&entry, rest, found);
                }
            }
        }
    }
}

/// Keeping a relative base relative.
fn join(base: &Path, segment: &str) -> PathBuf {
    if base.as_os_str().is_empty() {
        PathBuf::from(segment)
    } else {
        base.join(segment)
    }
}

/// Sorted by name so the walk is deterministic; the current directory when `base` is empty.
fn children(base: &Path) -> Vec<PathBuf> {
    let dir = if base.as_os_str().is_empty() {
        Path::new(".")
    } else {
        base
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| {
            if base.as_os_str().is_empty() {
                // `read_dir(".")` yields `./name`; the pattern was relative, so the result is too.
                strip_dot(&entry.path())
            } else {
                entry.path()
            }
        })
        .collect();
    out.sort();
    out
}

fn strip_dot(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The `scenario` command's matcher, so the CLI and the command select the same files.
pub use pemu_api::commands::scenario::segment_matches as matches;

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    fn tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pemu-cli-glob-{name}"));
        let _ = fs::remove_dir_all(&root);
        for dir in ["a", "a/deep", "b"] {
            fs::create_dir_all(root.join(dir)).expect("the scratch tree is writable");
        }
        for file in [
            "one.yaml",
            "two.yaml",
            "note.txt",
            "a/three.yaml",
            "a/deep/four.yaml",
            "b/five.yaml",
        ] {
            fs::write(root.join(file), b"x").expect("the scratch tree is writable");
        }
        root
    }

    /// Forward-slashed, relative to `root`.
    fn names(root: &Path, pattern: &str) -> Vec<String> {
        let full = format!("{}/{pattern}", slashed(root));
        expand(&full)
            .iter()
            .map(|path| {
                slashed(path)
                    .strip_prefix(&format!("{}/", slashed(root)))
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn a_star_stays_inside_one_segment_and_the_result_is_sorted() {
        let root = tree("star");
        assert_eq!(names(&root, "*.yaml"), ["one.yaml", "two.yaml"]);
        assert_eq!(names(&root, "*.txt"), ["note.txt"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_question_mark_matches_exactly_one_character() {
        let root = tree("question");
        assert_eq!(names(&root, "???.yaml"), ["one.yaml", "two.yaml"]);
        assert!(names(&root, "??.yaml").is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_double_star_spans_whole_segments_including_none() {
        let root = tree("deep");
        assert_eq!(
            names(&root, "**/*.yaml"),
            [
                "a/deep/four.yaml",
                "a/three.yaml",
                "b/five.yaml",
                "one.yaml",
                "two.yaml",
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn both_separators_name_the_same_files() {
        let root = tree("separators");
        let forward = expand(&format!("{}/a/*.yaml", slashed(&root)));
        let back = expand(&format!("{}\\a\\*.yaml", slashed(&root).replace('/', "\\")));
        assert_eq!(forward.len(), 1);
        assert_eq!(
            forward.iter().map(|p| slashed(p)).collect::<Vec<_>>(),
            back.iter().map(|p| slashed(p)).collect::<Vec<_>>()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A plain split would turn `C:\Users\...` into the drive-relative `C:Users\...`.
    #[test]
    fn an_absolute_pattern_keeps_the_hosts_root() {
        let parts = |pattern: &str| {
            let (root, segments) = split_root(pattern);
            (slashed(&root), segments)
        };
        let segs = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            parts("a/./b/../*.yaml"),
            (String::new(), segs(&["a", "b", "..", "*.yaml"]))
        );
        assert_eq!(
            parts(r"a\b/*.yaml"),
            (String::new(), segs(&["a", "b", "*.yaml"]))
        );
        if cfg!(windows) {
            assert_eq!(
                parts(r"C:\t\*.yaml"),
                ("C:/".into(), segs(&["t", "*.yaml"]))
            );
            assert_eq!(parts("C:/t/*.yaml"), ("C:/".into(), segs(&["t", "*.yaml"])));
            assert_eq!(
                parts(r"\\?\C:\t\*.yaml"),
                ("//?/C:/".into(), segs(&["t", "*.yaml"]))
            );
            assert_eq!(
                parts(r"\\srv\share\*.yaml"),
                ("//srv/share/".into(), segs(&["*.yaml"]))
            );
            assert_eq!(parts("C:*.yaml"), ("C:".into(), segs(&["*.yaml"])));
        } else {
            assert_eq!(parts("/t/*.yaml"), ("/".into(), segs(&["t", "*.yaml"])));
            assert_eq!(parts(r"\t\*.yaml"), ("/".into(), segs(&["t", "*.yaml"])));
        }
    }

    #[test]
    fn a_pattern_without_magic_is_returned_as_itself_even_when_it_is_missing() {
        assert_eq!(
            expand("tests/scenarios/menu.yaml"),
            vec![PathBuf::from("tests/scenarios/menu.yaml")]
        );
        assert!(!is_pattern("tests/scenarios/menu.yaml"));
    }

    #[test]
    fn a_pattern_that_matches_nothing_returns_nothing() {
        let root = tree("empty");
        assert!(names(&root, "*.pcap").is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn segment_matching_is_exact_at_both_ends() {
        assert!(matches("*.yaml", "menu.yaml"));
        assert!(!matches("*.yaml", "menu.yaml.bak"));
        assert!(matches("m*u.yaml", "menu.yaml"));
        assert!(matches("*", "anything"));
        assert!(matches("**", "anything"));
        assert!(!matches("a?c", "ac"));
        assert!(matches("a?c", "abc"));
        assert!(matches("a*b*c", "axxbyyc"));
        assert!(!matches("a*b*c", "axxbyy"));
    }
}
