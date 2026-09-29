//! `Machine::input_from` must stay the one place this crate appends to the input journal: hosts
//! export the journal through `Machine::journal`, and a second append site would journal inputs
//! no host door saw.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::Path;

fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("the crate's source directory is readable")
        .map(|e| e.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).expect("a source file is UTF-8");
            out.push((path.display().to_string(), text));
        }
    }
}

/// The `.append(` calls on a receiver ending in `journal` plus `Journal::append` paths, ignoring
/// line comments and whitespace (so a split call still counts).
fn append_sites(text: &str) -> usize {
    let code: String = text
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect();
    let flat: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let lower = flat.to_ascii_lowercase();
    lower.matches("journal.append(").count() + flat.matches("Journal::append").count()
}

#[test]
fn machine_input_is_the_only_journal_append_site_in_the_crate() {
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(
        files.iter().any(|(p, _)| p.ends_with("machine.rs")),
        "the scan found the crate's sources"
    );
    let sites: Vec<(&str, usize)> = files
        .iter()
        .map(|(p, t)| (p.as_str(), append_sites(t)))
        .filter(|(_, n)| *n > 0)
        .collect();
    assert_eq!(
        sites.len(),
        1,
        "one file appends to the journal, `machine.rs` `Machine::input_from`: {sites:?}; a second site \
         journals inputs no host door saw, so move the export onto `Machine::journal` first"
    );
    assert!(
        sites[0].0.ends_with("machine.rs") && sites[0].1 == 1,
        "{sites:?}"
    );
}

#[test]
fn the_guard_counts_a_split_call_and_a_path_call() {
    assert_eq!(append_sites("self.journal\n    .append(now, at, o, ev)"), 1);
    assert_eq!(append_sites("Journal::append(&mut j, now, at, o, ev)"), 1);
    assert_eq!(append_sites("self.journal.pop_due(now)"), 0);
    assert_eq!(append_sites("/// a second `Journal::append` site"), 0);
}
