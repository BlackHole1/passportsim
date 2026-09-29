//! Plans for the corpus images, found through `~/.config/passportsim/corpus.toml` and checked
//! against its SHA-256; each test skips when the manifest or a file is absent. No assertion names a
//! byte of an image.

// Test-only file and env access; the core-crate clippy.toml bans target non-test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod support;

use std::path::PathBuf;

use pemu_loader::bundle::CorpusMap;
use pemu_loader::sha256;
use pemu_planner::plan::{ImageSource, Origin, PlanRequest, dry_run};
use pemu_planner::rules::{BackupEvidence, CARDID_END, CARDID_OFFSET, Rule};

/// Prints the skip line `xtask ci` reads as SKIPPED-CORPUS (`xtask/src/ci/outcome.rs`).
fn skip(test: &str, id: &str, why: &str) {
    println!("SKIP {test}: corpus id `{id}` unavailable: {why}");
}

/// The merged image of corpus id `id`, or `None` after printing the skip. A found image prints
/// `RAN <test> <id>`, so a test that skips some ids reads as a partial skip.
fn corpus_bin(test: &str, id: &str) -> Option<Vec<u8>> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        skip(test, id, "HOME is not set");
        return None;
    };
    let Ok(text) = std::fs::read_to_string(home.join(".config/passportsim/corpus.toml")) else {
        skip(test, id, "no corpus manifest");
        return None;
    };
    let map = CorpusMap::parse(&text);
    let Some(file) = map.get(id).and_then(|entry| entry.file("bin")).cloned() else {
        skip(test, id, "corpus.toml lists no `bin` file for it");
        return None;
    };
    let path = match file.path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(&file.path),
    };
    let Ok(bytes) = std::fs::read(&path) else {
        skip(test, id, "its `bin` file is absent");
        return None;
    };
    if let Some(pinned) = file.sha256 {
        assert_eq!(sha256(&bytes), pinned, "{id}.bin differs from corpus.toml");
    }
    println!("RAN {test} {id}");
    Some(bytes)
}

/// Covers padded (8,388,608 B) and unpadded (1,579,296 B) images.
#[test]
fn corpus_passport_images_plan_to_three_writes_clear_of_nvs_and_cardid() {
    let test = "corpus_passport_images_plan_to_three_writes_clear_of_nvs_and_cardid";
    let backup = BackupEvidence {
        verified: true,
        owner_only: true,
        inside_repository: false,
    };
    for id in ["official", "pk", "demo", "goldminer", "probe-long"] {
        let Some(image) = corpus_bin(test, id) else {
            continue;
        };
        let request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
        let outcome = dry_run(&request, &support::facts(), Some(&backup));
        let rules: Vec<&str> = outcome.refusals.iter().map(|r| r.rule.id()).collect();
        let plan = outcome
            .accepted()
            .unwrap_or_else(|| panic!("{id} ({} B) refused: {rules:?}", image.len()));
        let offsets: Vec<u32> = plan.writes.iter().map(|w| w.offset).collect();
        assert_eq!(offsets, [0x0, 0x8000, 0x1_0000], "{id}");
        for w in &plan.writes {
            let (s, e) = w.sectors();
            assert!(e <= 0x9000 || s >= 0x1_0000, "{id} {} touches nvs", w.name);
            assert!(e <= u64::from(CARDID_OFFSET) || s >= u64::from(CARDID_END));
        }
        assert!(plan.app_elf_sha256.is_some(), "{id} app descriptor");
    }
}

/// Probe builds with the default IDF table carry no `cardid` entry; writing their table would drop
/// the identity partition.
#[test]
fn corpus_probe_images_without_cardid_are_refused() {
    let test = "corpus_probe_images_without_cardid_are_refused";
    for id in ["probe2", "scan3", "pkgatt"] {
        let Some(image) = corpus_bin(test, id) else {
            continue;
        };
        let request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
        let outcome = dry_run(&request, &support::facts(), None);
        assert!(outcome.refused_by(Rule::ImageMovesCardid), "{id}");
        assert!(outcome.accepted().is_none(), "{id}");
    }
}
