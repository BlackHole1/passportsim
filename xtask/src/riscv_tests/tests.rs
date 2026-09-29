//! Tests of the manifest rendering, parsing and verification, and of the argument parser.

use std::path::Path;

use super::manifest::{Entry, Excluded, Manifest, digest_file, hex};
use super::{build, parse_fetch_build, verify};

/// A two-entry manifest whose files a test writes.
fn manifest() -> Manifest {
    Manifest {
        commit: build::PINNED_COMMIT.to_string(),
        env_commit: "0".repeat(40),
        toolchain: "riscv32-esp-elf-gcc (crosstool-NG esp-14.2.0_20251107) 14.2.0".to_string(),
        build: "riscv32-esp-elf-gcc -march=rv32imc_zicsr_zifencei -o \"x\"".to_string(),
        // The real shim hash: `Manifest::verify` compares this line with the committed file.
        env_shim_sha256: hex(&pemu_loader::sha256(build::ENV_SHIM.as_bytes())),
        suites: build::SUITES.iter().map(|s| s.to_string()).collect(),
        tests: Vec::new(),
        excluded: Vec::new(),
    }
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// A scratch directory that removes itself.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let path = std::env::temp_dir().join(format!("pemu-riscv-tests-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch directory");
        Dir(path)
    }

    /// Writes one file and returns its manifest entry.
    fn write(&self, name: &str, bytes: &[u8]) -> Entry {
        std::fs::write(self.0.join(name), bytes).expect("write");
        let (sha256, size) = digest_file(&self.0.join(name)).expect("digest");
        Entry {
            name: name.to_string(),
            sha256,
            size,
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn render_round_trips_through_parse() {
    let dir = Dir::new("round-trip");
    let mut m = manifest();
    m.tests = vec![
        dir.write("rv32ui-p-add", b"one"),
        dir.write("rv32um-p-mul", b"two"),
    ];
    m.excluded = vec![Excluded {
        name: "rv32ui-p-nope".to_string(),
        reason: "needs a CSR the C3 has not".to_string(),
    }];
    assert_eq!(Manifest::parse(&m.render()).expect("parse"), m);
}

#[test]
fn verify_accepts_the_files_it_digested() {
    let dir = Dir::new("accept");
    let mut m = manifest();
    m.tests = vec![dir.write("rv32ui-p-add", b"one")];
    std::fs::write(dir.path().join("MANIFEST.toml"), m.render()).expect("write manifest");
    assert_eq!(m.verify(dir.path()), Ok(1));
    assert!(
        verify(dir.path())
            .expect("verify")
            .contains("1 ELF(s) verified")
    );
}

#[test]
fn verify_reports_a_changed_file() {
    let dir = Dir::new("changed");
    let mut m = manifest();
    m.tests = vec![dir.write("rv32ui-p-add", b"one")];
    std::fs::write(dir.path().join("rv32ui-p-add"), b"ONE").expect("overwrite");
    let err = m.verify(dir.path()).expect_err("must fail");
    assert!(err.contains("SHA-256"), "{err}");
}

/// The shim is the one environment change these ELFs carry, so a manifest whose `env_shim_sha256`
/// no longer matches the committed `c3_env_p.h` must fail even when every ELF still digests.
#[test]
fn verify_reports_a_shim_that_changed_since_the_build() {
    let dir = Dir::new("shim");
    let mut m = manifest();
    m.tests = vec![dir.write("rv32ui-p-add", b"one")];
    m.env_shim_sha256 = "f".repeat(64);
    let err = m.verify(dir.path()).expect_err("must fail");
    assert!(err.contains("c3_env_p.h"), "{err}");
    assert!(err.contains("fetch-build"), "{err}");
}

#[test]
fn verify_reports_a_missing_and_an_unlisted_file() {
    let dir = Dir::new("missing");
    let mut m = manifest();
    m.tests = vec![dir.write("rv32ui-p-add", b"one")];
    std::fs::remove_file(dir.path().join("rv32ui-p-add")).expect("remove");
    dir.write("rv32ui-p-stray", b"two");
    let err = m.verify(dir.path()).expect_err("must fail");
    assert!(err.contains("cannot read"), "{err}");
    assert!(err.contains("not in the manifest"), "{err}");
}

#[test]
fn verify_reports_a_size_that_does_not_match() {
    let dir = Dir::new("size");
    let mut m = manifest();
    m.tests = vec![dir.write("rv32ui-p-add", b"one")];
    m.tests[0].size = 99;
    let err = m.verify(dir.path()).expect_err("must fail");
    assert!(err.contains("manifest says 99"), "{err}");
}

#[test]
fn parse_rejects_a_foreign_schema_and_an_empty_exclusion_reason() {
    let dir = Dir::new("schema");
    let mut m = manifest();
    m.tests = vec![dir.write("rv32ui-p-add", b"one")];
    let text = m.render().replace("riscv-tests-manifest/1", "something/2");
    assert!(
        Manifest::parse(&text)
            .expect_err("must fail")
            .contains("schema")
    );

    m.excluded = vec![Excluded {
        name: "rv32ui-p-nope".to_string(),
        reason: "   ".to_string(),
    }];
    let err = Manifest::parse(&m.render()).expect_err("must fail");
    assert!(err.contains("empty reason"), "{err}");
}

#[test]
fn the_committed_manifest_verifies_against_the_committed_elfs() {
    let line = verify(&build::data_dir()).expect("the committed data directory verifies");
    assert!(line.contains("riscv-software-src/riscv-tests"), "{line}");
}

#[test]
fn fetch_build_arguments_are_parsed_and_checked() {
    let opts = parse_fetch_build(&[]).expect("defaults");
    assert_eq!(opts.commit, build::PINNED_COMMIT);
    assert!(!opts.offline);

    let opts = parse_fetch_build(&args(&["--offline", "--commit", &"A".repeat(40)])).expect("ok");
    assert!(opts.offline);
    assert_eq!(opts.commit, "a".repeat(40));

    assert!(parse_fetch_build(&args(&["--commit"])).is_err());
    assert!(parse_fetch_build(&args(&["--commit", "abc"])).is_err());
    assert!(parse_fetch_build(&args(&["--what"])).is_err());
}

#[test]
fn hex_renders_lowercase_bytes() {
    assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
}
