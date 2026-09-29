//! Corpus locator self-tests. They build their own corpus in a temporary directory, so they never
//! read the real one and pass on a host that has none.

use std::path::{Path, PathBuf};

use pemu_testkit::corpus::{self, Corpus, CorpusError, Located, Outcome};

/// A fresh temporary directory, removed by [`TempRoot`] on drop.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(tag: &str) -> TempRoot {
        let path =
            std::env::temp_dir().join(format!("pemu-testkit-corpus-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("corpus")).expect("temporary corpus directory");
        TempRoot { path }
    }

    fn root(&self) -> &Path {
        &self.path
    }

    /// Writes a corpus file and returns its bytes' SHA-256 as hex.
    fn write_file(&self, id: &str, name: &str, bytes: &[u8]) -> String {
        let dir = self.path.join("corpus").join(id);
        std::fs::create_dir_all(&dir).expect("corpus id directory");
        std::fs::write(dir.join(name), bytes).expect("corpus file");
        pemu_loader::hex(&pemu_loader::sha256(bytes))
    }

    fn write_manifest(&self, json: &str) {
        std::fs::write(self.path.join("corpus").join("MANIFEST.json"), json)
            .expect("corpus manifest");
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// One manifest record, in the shape the real `MANIFEST.json` uses.
fn record(id: &str, file: &str, size: usize, sha256: &str) -> String {
    format!(
        "{{\n \"id\": \"{id}\",\n \"file\": \"{file}\",\n \"path\": \"{id}/{file}\",\n \
         \"size\": {size},\n \"sha256\": \"{sha256}\",\n \"prefix_ok\": true\n}}"
    )
}

#[test]
fn t0_a_verified_id_is_found_with_its_files() {
    let root = TempRoot::new("found");
    let image = vec![0xA5u8; 64];
    let elf = b"\x7fELF toy".to_vec();
    let image_sha = root.write_file("pk", "image.bin", &image);
    let elf_sha = root.write_file("pk", "app.elf", &elf);
    root.write_manifest(&format!(
        "[\n{},\n{}\n]\n",
        record("pk", "image.bin", image.len(), &image_sha),
        record("pk", "app.elf", elf.len(), &elf_sha)
    ));

    let corpus = Corpus::locate_at(root.root()).expect("the manifest parses");
    assert_eq!(corpus.ids(), vec!["pk"]);

    let located = corpus.locate_id("pk");
    let files = located.found().expect("both files verify");
    assert_eq!(files.len(), 2);
    assert_eq!(located.skip_reason(), None);
    assert_eq!(
        located.file("app.elf").map(|f| f.path.clone()),
        Some(root.root().join("corpus").join("pk").join("app.elf")),
        "a relative manifest path resolves against the corpus directory"
    );
}

#[test]
fn t0_a_missing_id_reports_a_reason_instead_of_failing() {
    let root = TempRoot::new("missing");
    let image = vec![0x11u8; 32];
    let sha = pemu_loader::hex(&pemu_loader::sha256(&image));
    // The manifest lists the file; nothing is written to disk.
    root.write_manifest(&format!(
        "[\n{}\n]\n",
        record("goldminer", "image.bin", image.len(), &sha)
    ));

    let corpus = Corpus::locate_at(root.root()).expect("the manifest parses");
    let located = corpus.locate_id("goldminer");

    assert!(located.found().is_none());
    let reason = located.skip_reason().expect("a reason to skip with");
    assert!(reason.contains("goldminer/image.bin"), "{reason}");
    assert!(reason.contains("missing"), "{reason}");
    assert!(
        !reason.contains(root.root().to_string_lossy().as_ref()),
        "a skip reason names ids and outcomes, never a host path: {reason}"
    );

    let Located::Missing { files, .. } = &located else {
        panic!("missing");
    };
    assert_eq!(files[0].outcome, Outcome::Missing);
}

#[test]
fn t0_an_id_the_manifest_does_not_list_is_missing_with_that_reason() {
    let root = TempRoot::new("unknown");
    root.write_manifest("[]\n");
    let corpus = Corpus::locate_at(root.root()).expect("an empty manifest parses");
    assert!(corpus.ids().is_empty());

    let located = corpus.locate_id("pk");
    let reason = located.skip_reason().expect("a reason");
    assert!(reason.contains("no such id"), "{reason}");
}

#[test]
fn t0_a_changed_file_is_corrupt_not_missing() {
    let root = TempRoot::new("mismatch");
    let bytes = vec![0x22u8; 48];
    root.write_file("official", "image.bin", &bytes);
    let wrong_hash = "0".repeat(64);
    root.write_manifest(&format!(
        "[\n{}\n]\n",
        record("official", "image.bin", bytes.len(), &wrong_hash)
    ));
    let corpus = Corpus::locate_at(root.root()).expect("the manifest parses");

    let located = corpus.locate_id("official");
    let Located::Corrupt { files, .. } = &located else {
        panic!("a file whose bytes changed is present and wrong: {located:?}");
    };
    assert_eq!(
        files[0].outcome,
        Outcome::HashMismatch {
            expected_prefix: "0000000000000000".to_string(),
            actual_prefix: pemu_loader::hex(&pemu_loader::sha256(&bytes))[..16].to_string(),
        }
    );
    assert_eq!(
        located.skip_reason(),
        None,
        "a corrupt corpus is never a reason to skip"
    );
    let failure = located.failure_reason().expect("a reason to fail with");
    assert!(failure.contains("HASH MISMATCH"), "{failure}");
    assert!(
        !failure.contains(root.root().to_string_lossy().as_ref()),
        "a failure names ids and outcomes, never a host path: {failure}"
    );

    // Without the hash check the size alone is enough, and this file has the pinned size.
    let cheap = corpus.locate_id_with("official", false);
    assert_eq!(
        cheap.found().map(|f| f.len()),
        Some(1),
        "a test that only wants the path does not pay for the hash"
    );

    // A file of another size is rejected even without hashing, and is corrupt for the same
    // reason: it is on disk and it is not the pinned file.
    root.write_file("official", "image.bin", &[0x22u8; 47]);
    let corpus = Corpus::locate_at(root.root()).expect("the manifest parses");
    let Located::Corrupt { files, .. } = corpus.locate_id_with("official", false) else {
        panic!("a file of the wrong size is present and wrong");
    };
    assert_eq!(
        files[0].outcome,
        Outcome::SizeMismatch {
            expected: 48,
            actual: 47,
        }
    );
}

#[test]
fn t0_one_wrong_file_makes_the_whole_id_corrupt_even_beside_an_absent_one() {
    let root = TempRoot::new("mixed");
    let present = vec![0x33u8; 16];
    root.write_file("pk", "image.bin", &present);
    let absent_hash = "1".repeat(64);
    root.write_manifest(&format!(
        "[\n{},\n{}\n]\n",
        record("pk", "image.bin", present.len(), &"0".repeat(64)),
        record("pk", "app.elf", 8, &absent_hash)
    ));

    let corpus = Corpus::locate_at(root.root()).expect("the manifest parses");
    let located = corpus.locate_id("pk");
    let Located::Corrupt { files, reason, .. } = &located else {
        panic!("one wrong file decides the verdict: {located:?}");
    };
    assert!(files[0].outcome.is_corrupt());
    assert_eq!(files[1].outcome, Outcome::Missing);
    assert!(reason.contains("1 of 2"), "{reason}");
    assert_eq!(located.skip_reason(), None);
}

#[test]
fn t0_locating_an_id_at_an_explicit_root_needs_no_environment() {
    let root = TempRoot::new("explicit");
    let bytes = vec![0x44u8; 24];
    let sha = root.write_file("rom101", "rom.elf", &bytes);
    root.write_manifest(&format!(
        "[\n{}\n]\n",
        record("rom101", "rom.elf", bytes.len(), &sha)
    ));

    let located = corpus::locate_id_at(root.root(), "rom101");
    assert_eq!(located.found().map(|f| f.len()), Some(1));
    assert_eq!(located.skip_reason(), None);
    assert_eq!(located.failure_reason(), None);

    let bare = TempRoot::new("explicit-bare");
    let absent = corpus::locate_id_at(bare.root(), "rom101");
    assert!(
        absent.skip_reason().is_some(),
        "a root with no manifest is a skip, not a failure"
    );
    assert_eq!(absent.failure_reason(), None);
}

#[test]
fn t0_a_root_without_a_manifest_is_reported_as_no_corpus() {
    let root = TempRoot::new("bare");
    let err = Corpus::locate_at(root.root()).expect_err("there is no manifest");
    assert_eq!(err, CorpusError::NoManifest);
    assert!(err.to_string().contains("MANIFEST.json"), "{err}");
}

#[test]
fn t0_an_invalid_manifest_is_reported_with_the_record_that_is_wrong() {
    let root = TempRoot::new("invalid");
    root.write_manifest("[{\"id\": \"pk\", \"file\": \"image.bin\", \"path\": \"pk/image.bin\"}]");
    let err = Corpus::locate_at(root.root()).expect_err("the record has no size");
    let CorpusError::ManifestInvalid { detail } = &err else {
        panic!("an incomplete record is invalid, not missing: {err:?}");
    };
    assert!(detail.contains("record 0"), "{detail}");
    assert!(detail.contains("size"), "{detail}");

    root.write_manifest("not json");
    assert!(matches!(
        Corpus::locate_at(root.root()),
        Err(CorpusError::ManifestInvalid { .. })
    ));
}
