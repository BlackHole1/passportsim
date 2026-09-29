//! Checks the clippy half of layering rule 1 (`docs/ARCHITECTURE.md`, Layering).
//!
//! Clippy reads the `clippy.toml` nearest to each crate's manifest directory (walking up to the
//! workspace root) and does not merge files. Each core crate therefore carries its own
//! `clippy.toml` with `disallowed-types` and `disallowed-methods`, while the root file (read by
//! host crates, `xtask` and `tests/milestones`) lists none of them. This module keeps that
//! arrangement from drifting: every core crate file must list every required path and repeat
//! every other setting of the root file, and the root file must not list a required path.
//!
//! `pemu-planner` with feature `device` is not a core crate, but clippy cannot
//! scope a `clippy.toml` to a feature. The planner therefore keeps the core copy, and its device
//! module (`#[cfg(feature = "device")] pub mod exec;` in `lib.rs`) allows
//! `clippy::disallowed_methods` and `clippy::disallowed_types`. Device-feature code outside that
//! module needs the same allow, or clippy rejects what the core-std-api rule accepts.

use std::collections::BTreeSet;

/// Types every core crate's `clippy.toml` must disallow.
pub const REQUIRED_TYPES: &[&str] = &[
    "std::time::Instant",
    "std::time::SystemTime",
    "std::thread::JoinHandle",
    "std::thread::Thread",
    "std::thread::Builder",
    "std::thread::Scope",
    "std::thread::LocalKey",
    "std::fs::File",
    "std::fs::OpenOptions",
    "std::fs::DirEntry",
    "std::fs::ReadDir",
    "std::fs::Metadata",
    "std::net::TcpStream",
    "std::net::TcpListener",
    "std::net::UdpSocket",
    "std::process::Command",
    "std::process::Child",
    "std::process::Stdio",
    "std::env::Args",
    "std::env::Vars",
    "std::collections::HashMap",
    "std::collections::HashSet",
    "std::hash::RandomState",
];

/// Functions and methods every core crate's `clippy.toml` must disallow.
pub const REQUIRED_METHODS: &[&str] = &[
    "std::time::Instant::now",
    "std::time::Instant::elapsed",
    "std::time::SystemTime::now",
    "std::time::SystemTime::elapsed",
    "std::thread::spawn",
    "std::thread::sleep",
    "std::thread::scope",
    "std::thread::current",
    "std::thread::yield_now",
    "std::thread::park",
    "std::thread::available_parallelism",
    "std::thread::Builder::new",
    "std::fs::read",
    "std::fs::read_to_string",
    "std::fs::write",
    "std::fs::read_dir",
    "std::fs::create_dir",
    "std::fs::create_dir_all",
    "std::fs::remove_file",
    "std::fs::remove_dir",
    "std::fs::remove_dir_all",
    "std::fs::metadata",
    "std::fs::copy",
    "std::fs::rename",
    "std::fs::canonicalize",
    "std::fs::File::open",
    "std::fs::File::create",
    "std::fs::OpenOptions::new",
    "std::env::var",
    "std::env::var_os",
    "std::env::vars",
    "std::env::vars_os",
    "std::env::args",
    "std::env::args_os",
    "std::env::current_dir",
    "std::env::set_current_dir",
    "std::env::temp_dir",
    "std::env::current_exe",
    "std::env::home_dir",
    "std::env::set_var",
    "std::env::remove_var",
    "std::net::TcpStream::connect",
    "std::net::TcpListener::bind",
    "std::net::UdpSocket::bind",
    "std::net::ToSocketAddrs::to_socket_addrs",
    "std::process::exit",
    "std::process::abort",
    "std::process::id",
    "std::process::Command::new",
    "std::collections::HashMap::new",
    "std::collections::HashMap::with_capacity",
    "std::collections::HashSet::new",
    "std::collections::HashSet::with_capacity",
    // The platform libm differs per host; sqrt and powi do not go through it and stay allowed.
    "f32::sin",
    "f32::cos",
    "f32::tan",
    "f32::asin",
    "f32::acos",
    "f32::atan",
    "f32::atan2",
    "f32::exp",
    "f32::exp2",
    "f32::ln",
    "f32::log",
    "f32::log2",
    "f32::log10",
    "f32::sinh",
    "f32::cosh",
    "f32::tanh",
    "f32::cbrt",
    "f32::hypot",
    "f32::powf",
    "f32::mul_add",
    "f64::sin",
    "f64::cos",
    "f64::tan",
    "f64::asin",
    "f64::acos",
    "f64::atan",
    "f64::atan2",
    "f64::exp",
    "f64::exp2",
    "f64::ln",
    "f64::log",
    "f64::log2",
    "f64::log10",
    "f64::sinh",
    "f64::cosh",
    "f64::tanh",
    "f64::cbrt",
    "f64::hypot",
    "f64::powf",
    "f64::mul_add",
];

const TYPES_KEY: &str = "disallowed-types";
const METHODS_KEY: &str = "disallowed-methods";

/// Problems with a core crate's `clippy.toml` (`None` when the file is missing).
pub fn check_core(text: Option<&str>, root: Option<&str>) -> Vec<String> {
    let Some(text) = text else {
        return vec![
            "core crate has no clippy.toml with the layering rule 1 disallowed APIs".to_string(),
        ];
    };
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => return vec![format!("invalid clippy.toml: {e}")],
    };
    let mut problems = Vec::new();
    for (key, required) in [(TYPES_KEY, REQUIRED_TYPES), (METHODS_KEY, REQUIRED_METHODS)] {
        let listed = listed_paths(&table, key);
        for path in required.iter().filter(|p| !listed.contains(**p)) {
            problems.push(format!("{key} is missing `{path}`"));
        }
    }
    let root_table = root
        .and_then(|r| r.parse::<toml::Table>().ok())
        .unwrap_or_default();
    for (key, value) in &root_table {
        if key != TYPES_KEY && key != METHODS_KEY && table.get(key) != Some(value) {
            problems.push(format!("does not repeat root clippy.toml setting `{key}`"));
        }
    }
    problems
}

/// Problems with the root `clippy.toml`: it must not disallow a required path, or host crates,
/// `xtask` and `tests/milestones` would be restricted too.
pub fn check_root(text: Option<&str>) -> Vec<String> {
    let Some(text) = text else {
        return Vec::new();
    };
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => return vec![format!("invalid clippy.toml: {e}")],
    };
    let mut problems = Vec::new();
    for (key, required) in [(TYPES_KEY, REQUIRED_TYPES), (METHODS_KEY, REQUIRED_METHODS)] {
        let listed = listed_paths(&table, key);
        for path in required.iter().filter(|p| listed.contains(**p)) {
            problems.push(format!(
                "root {key} lists `{path}`, which would restrict host crates; keep it in core crate files"
            ));
        }
    }
    problems
}

/// Paths of a disallowed list; entries are strings or tables with a `path` key.
fn listed_paths(table: &toml::Table, key: &str) -> BTreeSet<String> {
    let entries = table.get(key).and_then(toml::Value::as_array);
    entries
        .into_iter()
        .flatten()
        .filter_map(|e| {
            e.as_str()
                .or_else(|| e.get("path").and_then(toml::Value::as_str))
        })
        .map(str::to_string)
        .collect()
}
