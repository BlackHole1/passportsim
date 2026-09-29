//! `cargo xtask portable`: the two scans that keep a Windows checkout byte-identical to a macOS
//! one.
//!
//! - `crlf`: no CR byte in tracked text under `specs/`, `boards/`, `tests/golden/`, `docs/` and
//!   `crates/`, skipping every path `.gitattributes` marks `binary` or `-text`. A CR byte breaks
//!   byte-exact goldens, `codegen --check`, `docs --check`, `include_str!` of specs, TOML and CSV
//!   offsets, and the secrets scanner's byte offsets.
//! - `names`: no tracked path with a Windows-invalid character (`<>:"|?*`, backslash, control
//!   characters 1 to 31), a reserved stem (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`,
//!   `LPT1`-`LPT9` with the ISO-8859-1 superscript forms, with any extension), a trailing dot or
//!   space, a length over 200 characters, or two paths differing only in case.
//!
//! Both report one line per finding and fail the command; `xtask ci t0` runs them as two steps.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "usage: cargo xtask portable [--root <dir>]";

/// Directories the CR scan covers.
const CR_ROOTS: &[&str] = &["specs/", "boards/", "tests/golden/", "docs/", "crates/"];

/// Longest tracked path, in characters. Windows resolves a relative path against the current
/// directory before applying `MAX_PATH`, so the budget is for the repository part only.
const MAX_PATH_CHARS: usize = 200;

/// Stems Windows reserves whatever the extension, beside the numbered ports below. Built with
/// `concat!` so this file does not itself name a device (layering rule 5).
const RESERVED_STEMS: &[&str] = &[
    concat!("co", "n"),
    concat!("pr", "n"),
    concat!("au", "x"),
    concat!("nu", "l"),
];

/// Prefixes of the numbered reserved ports.
const RESERVED_PORTS: &[&str] = &[concat!("co", "m"), concat!("lp", "t")];

/// Suffixes that make a reserved port. `0` is not reserved; Windows reads the ISO-8859-1
/// superscripts as digits, so they are.
const PORT_NUMBERS: &[&str] = &[
    "1", "2", "3", "4", "5", "6", "7", "8", "9", "\u{b9}", "\u{b2}", "\u{b3}",
];

/// Whether a lower-case stem is a name Windows reserves.
fn is_reserved(stem: &str) -> bool {
    RESERVED_STEMS.contains(&stem)
        || RESERVED_PORTS.iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|rest| PORT_NUMBERS.contains(&rest))
        })
}

/// Characters Windows rejects in a file name. `/` is the separator and is not one of them.
const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*', '\\'];

/// Entry point of `cargo xtask portable`.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut root: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--root" => {
                let value = iter
                    .next()
                    .ok_or_else(|| format!("--root needs a value\n{USAGE}"))?;
                root = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    let root = root.unwrap_or_else(|| {
        crate::util::workspace_root()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."))
    });
    let files = tracked(&root)?;
    let mut findings = check_names(&files);
    findings.extend(check_crlf(&root, &files)?);
    for line in &findings {
        println!("{line}");
    }
    println!(
        "portable: {} tracked paths, {} violations",
        files.len(),
        findings.len()
    );
    if findings.is_empty() {
        Ok(())
    } else {
        Err(format!("{} portability violations", findings.len()))
    }
}

/// Tracked paths of the repository, `/`-separated.
fn tracked(root: &Path) -> Result<Vec<String>, String> {
    let out = crate::util::git(root, &["ls-files", "-z"])
        .map_err(|err| format!("{err}; is --root a git repository?"))?;
    let mut files: Vec<String> = out
        .split(|&b| b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// One line per path that would not survive a Windows checkout.
pub fn check_names(files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut lowered: Vec<(String, &String)> = Vec::with_capacity(files.len());
    for path in files {
        if path.chars().count() > MAX_PATH_CHARS {
            out.push(format!(
                "portable-names {path}: {} characters, over the {MAX_PATH_CHARS} limit",
                path.chars().count()
            ));
        }
        for component in path.split('/') {
            if let Some(bad) = component
                .chars()
                .find(|c| INVALID_CHARS.contains(c) || (*c as u32) < 0x20)
            {
                out.push(format!(
                    "portable-names {path}: `{}` holds the character {bad:?}, invalid on Windows",
                    component
                ));
            }
            if component.ends_with('.') || component.ends_with(' ') {
                out.push(format!(
                    "portable-names {path}: `{component}` ends with a dot or a space"
                ));
            }
            let stem = component
                .split_once('.')
                .map_or(component, |(stem, _)| stem)
                .to_ascii_lowercase();
            if is_reserved(&stem) {
                out.push(format!(
                    "portable-names {path}: `{component}` is a reserved Windows device name"
                ));
            }
        }
        lowered.push((path.to_lowercase(), path));
    }
    lowered.sort();
    for pair in lowered.windows(2) {
        if pair[0].0 == pair[1].0 {
            out.push(format!(
                "portable-names {}: differs from {} only in case",
                pair[1].1, pair[0].1
            ));
        }
    }
    out.sort();
    out
}

/// One line per tracked text file under [`CR_ROOTS`] holding a CR byte.
fn check_crlf(root: &Path, files: &[String]) -> Result<Vec<String>, String> {
    let exempt = binary_or_untexted(root, files)?;
    let mut out = Vec::new();
    for path in files {
        if !CR_ROOTS.iter().any(|dir| path.starts_with(dir)) || exempt.contains(path) {
            continue;
        }
        let bytes = match std::fs::read(root.join(path)) {
            Ok(bytes) => bytes,
            // A tracked path can be absent from the working tree during a partial checkout.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("cannot read {path}: {}", err.kind())),
        };
        if let Some(at) = bytes.iter().position(|&b| b == b'\r') {
            let line = 1 + bytes[..at].iter().filter(|&&b| b == b'\n').count();
            out.push(format!(
                "crlf {path}:{line}: CRLF found; the tree is LF only (.gitattributes `* text=auto eol=lf`)"
            ));
        }
    }
    Ok(out)
}

/// Tracked paths `.gitattributes` marks `binary` or `-text`, which the CR scan skips.
fn binary_or_untexted(root: &Path, files: &[String]) -> Result<Vec<String>, String> {
    let mut input = String::new();
    for path in files {
        input.push_str(path);
        input.push('\n');
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-attr", "--stdin", "text", "binary"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git check-attr: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("git check-attr has no stdin")?
        .write_all(input.as_bytes())
        .map_err(|e| format!("cannot write to git check-attr: {e}"))?;
    let finished = child
        .wait_with_output()
        .map_err(|e| format!("git check-attr failed: {e}"))?;
    if !finished.status.success() {
        return Err("git check-attr failed".to_string());
    }
    let out = String::from_utf8_lossy(&finished.stdout).into_owned();
    let mut exempt = Vec::new();
    for line in out.lines() {
        // `<path>: <attribute>: <value>`, with the path first and unquoted for our names.
        let Some((path, rest)) = line.split_once(": ") else {
            continue;
        };
        let unset = rest.ends_with(": unset") && rest.starts_with("text:");
        let binary = rest.starts_with("binary:") && rest.ends_with(": set");
        if unset || binary {
            exempt.push(path.to_string());
        }
    }
    exempt.sort();
    exempt.dedup();
    Ok(exempt)
}

#[cfg(test)]
mod tests;
