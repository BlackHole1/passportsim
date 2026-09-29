//! Unit tests of `xtask portable`.

use super::check_names;

fn names(paths: &[&str]) -> Vec<String> {
    check_names(&paths.iter().map(|p| p.to_string()).collect::<Vec<_>>())
}

#[test]
fn portable_paths_pass() {
    assert!(
        names(&[
            "crates/pemu-core/src/lib.rs",
            "docs/ARCHITECTURE.md",
            "assets/rom/esp32c3_rev101_rom.elf",
            "tests/golden/official-boot.txt",
            "COM0.md",
            "com10.rs",
            "console.rs",
        ])
        .is_empty()
    );
}

#[test]
fn windows_invalid_characters_and_reserved_names_fail() {
    let found = names(&[
        "docs/a:b.md",
        "docs/q?.md",
        "crates/x/src/aux.rs",
        "crates/x/src/COM1.toml",
        "crates/x/src/lpt\u{b2}",
        "docs/trailing.",
        "docs/space ",
    ]);
    assert_eq!(found.len(), 7, "{found:#?}");
    assert!(found.iter().any(|l| l.contains("invalid on Windows")));
    assert!(
        found
            .iter()
            .filter(|l| l.contains("reserved Windows device name"))
            .count()
            == 3,
        "{found:#?}"
    );
    assert!(
        found
            .iter()
            .filter(|l| l.contains("ends with a dot or a space"))
            .count()
            == 2,
        "{found:#?}"
    );
}

#[test]
fn paths_differing_only_in_case_fail() {
    let found = names(&["docs/Plan.md", "docs/plan.md", "docs/PLAN.md"]);
    assert_eq!(found.len(), 2, "{found:#?}");
    assert!(
        found.iter().all(|l| l.contains("only in case")),
        "{found:#?}"
    );
}

#[test]
fn an_overlong_path_fails() {
    let long = format!("crates/{}/src/lib.rs", "a".repeat(200));
    let found = names(&[long.as_str()]);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(found[0].contains("over the 200 limit"), "{found:#?}");
}
