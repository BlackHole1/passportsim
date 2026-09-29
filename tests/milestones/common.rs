//! Shared helpers for the milestone tests, included by the `mod common;` line of each file.
//!
//! - [`corpus_or_skip`] returns the corpus files or prints a skip: T0 hosts have no corpus. A file
//!   that is present but not the pinned one panics, so a corrupted corpus never reads green.
//! - [`committed_golden_or_skip`] and [`derived_golden_or_skip`] load goldens; a device-derived
//!   golden lives under the data root, never in the tree.
//! - [`assert_console_prefix`] compares a normalized console with a golden prefix.
//! - [`assert_cli_exit`] accepts both success exit codes.
//!
//! The test harness has no "skipped" outcome, so a skip prints one `SKIP <test>: <reason>` line,
//! which `xtask ci` reports. Nothing here copies an image into the tree or prints a host path.

use std::path::{Path, PathBuf};

use pemu_testkit::corpus::{self, CorpusFile, Located};
use pemu_testkit::golden::{self, BootSelect, Golden};

/// Asserts that a CLI call that must "succeed" did: 0 PASS, or 10 PASS_WITH_CAVEATS when the
/// printed receipt carries at least one caveat source. A lenient run exits 10 almost always, since
/// a first touch of an unmodeled register is a caveat. Other expected codes are compared exactly.
#[track_caller]
pub fn assert_cli_exit(exit: i32, code: i32, stdout: &str, context: &str) {
    if code == 0 && exit == 10 {
        let receipt: serde_json::Value = serde_json::from_str(stdout.trim_end())
            .unwrap_or_else(|e| panic!("{context}: exit 10 printed no JSON: {e}: {stdout}"));
        let receipt = &receipt["receipt"];
        let listed = |key: &str| receipt[key].as_array().is_some_and(|a| !a.is_empty());
        assert!(
            listed("unmodeled_first_touch")
                || listed("timing_lint")
                || receipt["classes_touched"]["U"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty())
                || receipt["hle"]["tripwires_hit"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty()),
            "{context}: exit 10 PASS_WITH_CAVEATS with no caveat in the receipt: {stdout}"
        );
        return;
    }
    assert_eq!(exit, code, "{context}: {stdout}");
}

/// Corpus id of the Passport Keys image.
pub const PK: &str = "pk";
/// Corpus id of the official FoloToy image.
pub const OFFICIAL: &str = "official";
/// Corpus id of the sanitized factory image.
pub const GOLDMINER: &str = "goldminer";
/// Corpus id of the bundled ECO7 ROM ELF, the default ROM of every run.
pub const ROM101: &str = "rom101";

/// Prints the one `SKIP` line an absent input produces. The reason names ids, file names and
/// outcomes only, never a host path.
pub fn skip(test: &str, reason: &str) {
    println!("SKIP {test}: {reason}");
}

/// The verified files of a corpus id, or `None` after printing why the test is skipped.
///
/// # Panics
///
/// When a file of the id is on disk and is not the pinned file.
#[track_caller]
pub fn corpus_or_skip(test: &str, id: &str) -> Option<Vec<CorpusFile>> {
    verdict_or_skip(test, corpus::locate_id(id))
}

/// [`corpus_or_skip`] against an explicit data root, which the self-checks use so they never read
/// this host's real corpus.
#[track_caller]
pub fn corpus_at_or_skip(test: &str, data_root: &Path, id: &str) -> Option<Vec<CorpusFile>> {
    verdict_or_skip(test, corpus::locate_id_at(data_root, id))
}

#[track_caller]
fn verdict_or_skip(test: &str, located: Located) -> Option<Vec<CorpusFile>> {
    if let Some(failure) = located.failure_reason() {
        panic!("{failure}");
    }
    match located.skip_reason() {
        None => located.found().map(<[CorpusFile]>::to_vec),
        Some(reason) => {
            skip(test, &reason);
            None
        }
    }
}

/// The path of one file of a corpus id (`FoloToy-AI-Passport-8MB.bin`), or `None` after printing
/// why the test is skipped. Panics as [`corpus_or_skip`].
#[track_caller]
pub fn corpus_file_or_skip(test: &str, id: &str, file: &str) -> Option<PathBuf> {
    pick_file(test, corpus_or_skip(test, id)?, id, file)
}

/// [`corpus_file_or_skip`] against an explicit data root.
#[track_caller]
pub fn corpus_file_at_or_skip(
    test: &str,
    data_root: &Path,
    id: &str,
    file: &str,
) -> Option<PathBuf> {
    pick_file(test, corpus_at_or_skip(test, data_root, id)?, id, file)
}

fn pick_file(test: &str, files: Vec<CorpusFile>, id: &str, file: &str) -> Option<PathBuf> {
    match files.iter().find(|f| f.file == file) {
        Some(found) => Some(found.path.clone()),
        None => {
            skip(test, &format!("corpus id `{id}` has no file `{file}`"));
            None
        }
    }
}

pub fn committed_golden_or_skip(test: &str, name: &str) -> Option<Golden> {
    match golden::committed(name) {
        Ok(golden) => Some(golden),
        Err(err) => {
            skip(test, &err.to_string());
            None
        }
    }
}

/// A golden derived on this host under `<data root>/goldens/`, or `None` after the skip line. A
/// device golden is never committed: its header carries an eFuse hash of the one device.
pub fn derived_golden_or_skip(test: &str, name: &str) -> Option<Golden> {
    match golden::derived(name) {
        Ok(golden) => Some(golden),
        Err(err) => {
            skip(test, &err.to_string());
            None
        }
    }
}

/// Asserts that a run's raw console, normalized here, matches the golden prefix; `claimed` is a
/// line count, `None` the whole golden. A mismatch shows the first differing line in context.
#[track_caller]
pub fn assert_console_prefix(
    name: &str,
    golden: &Golden,
    console: &[u8],
    claimed: Option<usize>,
) -> usize {
    golden::assert_prefix_bytes(name, golden, console, BootSelect::LastBoot, claimed)
}

pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

/// `<target dir>/[<triple>/]<profile>`, the directory of this test executable's build.
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable's path");
    exe.parent()
        .and_then(Path::parent)
        .expect("a target profile directory")
        .to_path_buf()
}

/// The `cargo build` arguments that put a binary in `profile`: its profile flag, the target
/// directory and, for a cross build, the triple.
pub fn build_args(profile: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    match profile.file_name().and_then(|name| name.to_str()) {
        Some("debug") | None => {}
        Some("release") => args.push("--release".into()),
        Some(custom) => args.extend(["--profile".into(), custom.into()]),
    }
    let Some(parent) = profile.parent() else {
        return args;
    };
    let (target_dir, triple) = if parent.join("CACHEDIR.TAG").is_file() {
        (parent, None)
    } else {
        (parent.parent().unwrap_or(parent), parent.file_name())
    };
    args.extend(["--target-dir".into(), target_dir.as_os_str().to_owned()]);
    if let Some(triple) = triple {
        args.extend(["--target".into(), triple.to_owned()]);
    }
    args
}

fn cargo_bin(package: &str, bin: &str, profile: &Path) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args(["build", "-q", "-p", package, "--bin", bin])
        .args(build_args(profile))
        .current_dir(workspace())
        .status()
        .expect("cargo runs");
    assert!(status.success(), "`cargo build -p {package}` failed");
    let path = profile.join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
    assert!(path.is_file(), "{} was built", path.display());
    path
}

/// The `passportsim` binary of this build, built on first use into the test's own profile, so
/// the process a test drives is the code the test was compiled from.
pub fn passportsim() -> PathBuf {
    static BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BIN.get_or_init(|| cargo_bin("pemu-cli", "passportsim", &profile_dir()))
        .clone()
}

/// The `xtask` binary, built on first use. A debug test run builds it in release, because
/// `xtask bench` refuses to measure a debug build.
pub fn xtask_bin() -> PathBuf {
    static BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        let profile = profile_dir();
        let measures = profile
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name != "debug" && !name.is_empty());
        let target = if measures {
            profile
        } else {
            profile.with_file_name("release")
        };
        cargo_bin("xtask", "xtask", &target)
    })
    .clone()
}

/// Runs `xtask bench <args>` from the workspace root; its stdout, or both streams on failure.
pub fn xtask_bench(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(xtask_bin())
        .arg("bench")
        .args(args)
        .current_dir(workspace())
        .output()
        .expect("xtask runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "`xtask bench {}` failed:\n{stdout}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// The one record of a `xtask bench --json` run that asked for one workload.
pub fn one_bench_record(stdout: &str) -> serde_json::Value {
    let from = stdout.find("[\n").expect("a --json array");
    let end = stdout[from..].rfind("\n]").expect("a --json array") + from + 2;
    let array: Vec<serde_json::Value> =
        serde_json::from_str(&stdout[from..end]).expect("the --json array parses");
    assert_eq!(array.len(), 1, "one workload was asked for: {stdout}");
    array.into_iter().next().expect("the record")
}

pub fn pk_files(test: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let image = corpus_file_or_skip(test, PK, "FoloToy-AI-Passport-8MB.bin")?;
    let elf = corpus_file_or_skip(test, PK, "FoloToy-AI-Passport.elf")?;
    Some((
        std::fs::read(image).expect("the verified image is readable"),
        std::fs::read(elf).expect("the verified ELF is readable"),
    ))
}

pub fn machine(
    flash: &[u8],
    elf: &[u8],
    cfg: pemu_machine::config::MachineConfig,
) -> pemu_machine::machine::Machine {
    use pemu_loader::{bundle::FlashImage, efuse_image::EfuseImage, elf::ElfInfo};
    let flash = FlashImage::from_merged(flash).expect("a corpus image parses");
    let elf = std::sync::Arc::new(ElfInfo::parse(elf).expect("the ELF parses"));
    let assets = pemu_machine::config::Assets::with_bundled_rom(
        flash,
        Some(elf),
        None,
        EfuseImage::synth(0),
    )
    .expect("the bundled ROM is pinned");
    pemu_machine::machine::Machine::new(cfg, assets).expect("the image fits")
}

pub fn image_machine(flash: &[u8]) -> pemu_machine::machine::Machine {
    use pemu_loader::{bundle::FlashImage, efuse_image::EfuseImage};
    let flash = FlashImage::from_merged(flash).expect("a corpus image parses as a merged image");
    let assets =
        pemu_machine::config::Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
            .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    pemu_machine::machine::Machine::new(pemu_machine::config::MachineConfig::default(), assets)
        .expect("the image fits the 8 MB flash")
}

pub fn console_bytes(m: &mut pemu_machine::machine::Machine) -> Vec<u8> {
    let ring = m.io().serial_ring(pemu_core::hostio::SerialStream::UsjTx);
    ring.slices(0).iter().copied().collect()
}

/// Self-checks of the helpers above. They are compiled into every milestone binary; their names
/// carry no `t<tier>_m<N>_` prefix, so `xtask ci` never counts them as milestone tests.
#[cfg(test)]
mod self_checks {
    use super::*;

    /// `tests/milestones/` has one dependency, so the hash comes from the testkit.
    use pemu_testkit::corpus::sha256_hex as sha256_of;

    /// A corpus root built here: the self-checks run at T0, which needs no corpus.
    struct TempCorpus {
        root: PathBuf,
    }

    impl TempCorpus {
        fn new(tag: &str) -> TempCorpus {
            let root = std::env::temp_dir().join(format!(
                "pemu-milestones-common-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("corpus").join(PK)).expect("temporary corpus");
            TempCorpus { root }
        }

        /// Writes `bytes` as one file of `PK` and returns a manifest record pinning `pinned`, which may
        /// differ from the bytes on disk.
        fn record(&self, name: &str, bytes: &[u8], pinned: &str) -> String {
            std::fs::write(self.root.join("corpus").join(PK).join(name), bytes)
                .expect("corpus file");
            format!(
                "{{\"id\": \"{PK}\", \"file\": \"{name}\", \"path\": \"{PK}/{name}\", \
                 \"size\": {}, \"sha256\": \"{pinned}\"}}",
                bytes.len()
            )
        }

        fn write_manifest(&self, records: &[String]) {
            std::fs::write(
                self.root.join("corpus").join("MANIFEST.json"),
                format!("[{}]", records.join(",")),
            )
            .expect("corpus manifest");
        }
    }

    impl Drop for TempCorpus {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn common_corpus_helpers_return_the_files_of_a_verified_id() {
        let test = "common_corpus_helpers_return_the_files_of_a_verified_id";
        let corpus = TempCorpus::new("found");
        let bytes = b"a merged image stand-in".to_vec();
        let record = corpus.record("image.bin", &bytes, &sha256_of(&bytes));
        corpus.write_manifest(&[record]);

        let files = corpus_at_or_skip(test, &corpus.root, PK).expect("the id verifies");
        assert_eq!(files.len(), 1);
        assert!(files.iter().all(|f| f.id == PK));
        assert_eq!(
            files[0].path,
            corpus.root.join("corpus").join(PK).join("image.bin")
        );
    }

    #[test]
    fn common_corpus_helpers_skip_when_the_files_are_not_here() {
        let test = "common_corpus_helpers_skip_when_the_files_are_not_here";
        let corpus = TempCorpus::new("missing");
        let bytes = b"never written to disk".to_vec();
        let record = corpus.record("image.bin", &bytes, &sha256_of(&bytes));
        corpus.write_manifest(&[record]);
        std::fs::remove_file(corpus.root.join("corpus").join(PK).join("image.bin"))
            .expect("remove the file the manifest lists");

        assert_eq!(
            corpus_at_or_skip(test, &corpus.root, PK),
            None,
            "an absent corpus is a skip, never a failure"
        );

        let bare = TempCorpus::new("bare");
        std::fs::remove_file(bare.root.join("corpus").join("MANIFEST.json")).ok();
        assert_eq!(corpus_at_or_skip(test, &bare.root, PK), None);
    }

    /// A test skips when the corpus is absent and fails when a present file's hash mismatches:
    /// otherwise one flipped byte would turn every exit that uses the image into a printed SKIP.
    #[test]
    fn common_corpus_helpers_fail_on_a_file_that_is_present_and_not_the_pinned_one() {
        let test = "common_corpus_helpers_fail_on_a_file_that_is_present_and_not_the_pinned_one";
        let corpus = TempCorpus::new("corrupt");
        let good = b"the pinned bytes".to_vec();
        let flipped = b"the flipped bytes".to_vec();
        // One file verifies; the other is on disk with the right size and the wrong hash.
        let ok = corpus.record("app.elf", &good, &sha256_of(&good));
        let wrong = corpus.record("image.bin", &flipped, &sha256_of(&vec![0u8; flipped.len()]));
        corpus.write_manifest(&[ok, wrong]);

        let root = corpus.root.clone();
        let panic = std::panic::catch_unwind(move || corpus_at_or_skip(test, &root, PK))
            .expect_err("a corpus that is present and wrong fails the test");
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "<not a string>".to_string());
        assert!(message.contains("HASH MISMATCH"), "{message}");
        assert!(message.contains(PK), "{message}");
        assert!(
            !message.contains(corpus.root.to_string_lossy().as_ref()),
            "a failure names ids and outcomes, never a host path: {message}"
        );
    }

    #[test]
    fn common_corpus_file_helper_picks_one_file_and_reports_one_the_id_does_not_carry() {
        let test = "common_corpus_file_helper_picks_one_file_and_reports_one_the_id_does_not_carry";
        let corpus = TempCorpus::new("file");
        let bytes = b"one file of the id".to_vec();
        let record = corpus.record("image.bin", &bytes, &sha256_of(&bytes));
        corpus.write_manifest(&[record]);

        assert_eq!(
            corpus_file_at_or_skip(test, &corpus.root, PK, "image.bin"),
            Some(corpus.root.join("corpus").join(PK).join("image.bin"))
        );
        assert_eq!(
            corpus_file_at_or_skip(test, &corpus.root, PK, "no-such-file.bin"),
            None,
            "a file the id does not carry is never a path"
        );
    }
}
