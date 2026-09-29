//! Walks a crate's module tree from its target roots and scans every file with the
//! gate it is compiled under.
//!
//! Roots: `src/lib.rs`, `src/main.rs`, `src/bin/*.rs` and `src/bin/*/main.rs` (not gated);
//! `build.rs`, `tests/`, `benches/` and `examples/` roots (test gate, since they are not part of
//! the library build); explicit manifest target paths with the gate of their kind. A module file
//! takes the gate of its `mod name;` declaration. A file reached on several paths keeps only the
//! gates all paths agree on. Files not reached from any root are scanned without a gate, so an
//! unusual include cannot hide a violation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::manifest::Target;
use super::source::{self, FileScan, Gate, ModDecl};

/// Scans every `.rs` file of a crate. `files` maps crate-relative `/`-separated paths to text.
/// Returns each file's scan, keyed by path.
pub fn scan_crate(
    targets: &[Target],
    files: &BTreeMap<String, String>,
) -> BTreeMap<String, FileScan> {
    let test = Gate {
        test: true,
        device: false,
    };
    let mut roots: BTreeMap<String, Gate> = BTreeMap::new();
    for path in files.keys() {
        let gate = match path.split('/').collect::<Vec<_>>().as_slice() {
            ["src", "lib.rs"] | ["src", "main.rs"] => Gate::default(),
            ["src", "bin", f] if f.ends_with(".rs") => Gate::default(),
            ["src", "bin", _, "main.rs"] => Gate::default(),
            ["build.rs"] => test,
            [dir, f] if is_aux_dir(dir) && f.ends_with(".rs") => test,
            [dir, _, "main.rs"] if is_aux_dir(dir) => test,
            _ => continue,
        };
        roots.insert(path.clone(), gate);
    }
    for t in targets {
        let path = normalize(&t.path);
        let gate = if t.test { test } else { Gate::default() };
        let merged = roots.get(&path).map_or(gate, |old| old.and(gate));
        roots.insert(path, merged);
    }

    let root_set: BTreeSet<String> = roots.keys().cloned().collect();
    let mut gates: BTreeMap<String, Gate> = BTreeMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for (path, gate) in roots {
        merge(&mut gates, &mut queue, path, gate);
    }
    let mut scans = BTreeMap::new();
    while let Some(path) = queue.pop_front() {
        let Some(text) = files.get(&path) else {
            continue;
        };
        let scan = source::scan(text, gates[&path]);
        let mod_rs = root_set.contains(&path) || path.ends_with("/mod.rs") || path == "mod.rs";
        for decl in &scan.mods {
            let found = candidates(&path, mod_rs, decl)
                .into_iter()
                .find(|c| files.contains_key(c));
            if let Some(child) = found {
                merge(&mut gates, &mut queue, child, decl.gate);
            }
        }
        scans.insert(path, scan);
    }
    for (path, text) in files {
        if !scans.contains_key(path) {
            scans.insert(path.clone(), source::scan(text, Gate::default()));
        }
    }
    scans
}

fn is_aux_dir(dir: &str) -> bool {
    matches!(dir, "tests" | "benches" | "examples")
}

fn merge(
    gates: &mut BTreeMap<String, Gate>,
    queue: &mut VecDeque<String>,
    path: String,
    gate: Gate,
) {
    let merged = gates.get(&path).map_or(gate, |old| old.and(gate));
    if gates.get(&path) != Some(&merged) {
        gates.insert(path.clone(), merged);
        queue.push_back(path);
    }
}

/// Candidate files of an out-of-line module declared in `parent`.
fn candidates(parent: &str, mod_rs: bool, decl: &ModDecl) -> Vec<String> {
    let parent_dir = match parent.rfind('/') {
        Some(i) => &parent[..i],
        None => "",
    };
    let own_dir = if mod_rs {
        parent_dir.to_string()
    } else {
        join(
            parent_dir,
            parent
                .rsplit('/')
                .next()
                .unwrap_or(parent)
                .trim_end_matches(".rs"),
        )
    };
    let inline = decl.inline_parents.join("/");
    if let Some(p) = &decl.path_attr {
        let base = if inline.is_empty() {
            parent_dir.to_string()
        } else {
            join(&own_dir, &inline)
        };
        return vec![normalize(&join(&base, p))];
    }
    let dir = join(&own_dir, &inline);
    vec![
        normalize(&join(&dir, &format!("{}.rs", decl.name))),
        normalize(&join(&dir, &format!("{}/mod.rs", decl.name))),
    ]
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

/// Resolves `.` and `..` segments of a `/`-separated relative path.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}
