//! Two tests of the embedded payload against the built binary: the embedded digest equals the
//! receipt's, and a binary moved away from its directory still finds its payload.
//!
//! Both read the binary's payload from `passportsim --version`. A development build embeds nothing,
//! and that alone is the skip: the test first asserts the binary reports itself as a development
//! build, prints one `SKIP <test>: <reason>` line, and still runs every assertion that needs no
//! embedded copy. A build meant to embed that did not cannot pass as a skip: its `--version` would
//! say the variable is unset while `option_env!` saw it set.
//!
//! To run them for real: `PASSPORTSIM_EMBED_PAYLOAD=<dir> cargo test -p pemu-cli --test payload`,
//! with `<dir>` a package tree from `cargo run -q -p xtask -- package`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// As this test crate saw it at compile time; Cargo builds the binary and this test in one
/// invocation.
const EMBEDDED_FROM: Option<&str> = option_env!("PASSPORTSIM_EMBED_PAYLOAD");

const BIN: &str = env!("CARGO_BIN_EXE_passportsim");

fn binary_name() -> String {
    format!("passportsim{}", std::env::consts::EXE_SUFFIX)
}

/// In the `tests/milestones/common.rs` form.
fn skip(test: &str, reason: &str) {
    println!("SKIP {test}: {reason}");
}

/// Removed first if an earlier run left it.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pemu-cli-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Run from a neutral working directory so the answer cannot come from it.
fn payload_line(binary: &Path) -> String {
    let output = Command::new(binary)
        .arg("--version")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", binary.display()));
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("UTF-8");
    stdout
        .lines()
        .find(|line| line.starts_with("payload: "))
        .unwrap_or_else(|| panic!("no payload line in:\n{stdout}"))
        .to_owned()
}

fn copy_binary(dir: &Path) -> PathBuf {
    let to = dir.join(binary_name());
    fs::copy(BIN, &to).expect("copy the binary");
    to
}

/// Asserting the copy is not damaged.
fn embedded_digest(line: &str) -> String {
    line.strip_prefix("payload: embedded, sha256 ")
        .unwrap_or_else(|| panic!("not an intact embedded payload: {line}"))
        .to_owned()
}

/// Which is what makes a skip legitimate.
fn assert_development_build(line: &str) {
    assert!(
        line.starts_with("payload: none: development build")
            && line.contains("PASSPORTSIM_EMBED_PAYLOAD was not set"),
        "the binary embeds nothing, but not because it is a development build: {line}"
    );
}

fn receipt_digest(receipt: &Path) -> String {
    let text = fs::read_to_string(receipt).expect("receipt.json");
    let value: serde_json::Value = serde_json::from_str(&text).expect("receipt.json is JSON");
    value["payload"]["sha256"]
        .as_str()
        .expect("receipt.json records payload.sha256")
        .to_owned()
}

#[test]
fn the_embedded_digest_equals_the_receipts() {
    const TEST: &str = "the_embedded_digest_equals_the_receipts";
    let line = payload_line(Path::new(BIN));
    let Some(tree) = EMBEDDED_FROM.filter(|dir| !dir.is_empty()) else {
        assert_development_build(&line);
        skip(
            TEST,
            "this build embedded no payload (PASSPORTSIM_EMBED_PAYLOAD unset at build time), so \
             there is no embedded digest to compare with a receipt",
        );
        return;
    };
    let embedded = embedded_digest(&line);
    let receipt = Path::new(tree).join("receipt.json");
    assert!(
        receipt.is_file(),
        "PASSPORTSIM_EMBED_PAYLOAD names a tree without receipt.json; it must name the package \
         tree `xtask package` wrote"
    );
    assert_eq!(
        embedded,
        receipt_digest(&receipt),
        "the digest embedded in passportsim is not the one receipt.json records"
    );
}

/// One payload file and a `receipt.json` recording its digest.
fn package_around_binary(dir: &Path) -> (PathBuf, String) {
    let binary = copy_binary(dir);
    let body = b"<html>";
    fs::create_dir_all(dir.join("payload/web")).expect("mkdir");
    fs::write(dir.join("payload/web/index.html"), body).expect("write");
    let listing = format!(
        "{}  payload/web/index.html\n",
        pemu_loader::hex(&pemu_loader::sha256(body))
    );
    let digest = pemu_loader::hex(&pemu_loader::sha256(listing.as_bytes()));
    fs::write(
        dir.join("receipt.json"),
        format!(r#"{{"payload":{{"sha256":"{digest}"}}}}"#),
    )
    .expect("write receipt");
    (binary, digest)
}

#[test]
fn a_binary_moved_away_from_its_directory_still_finds_the_payload() {
    const TEST: &str = "a_binary_moved_away_from_its_directory_still_finds_the_payload";
    let root = scratch("moved");
    let package = root.join("package");
    let moved = root.join("elsewhere");
    fs::create_dir_all(&package).expect("mkdir");
    fs::create_dir_all(&moved).expect("mkdir");
    let (packaged, directory_digest) = package_around_binary(&package);
    let inside = payload_line(&packaged);
    // The same bytes, in a directory that holds nothing else.
    fs::rename(&packaged, moved.join(binary_name())).expect("move the binary");
    let outside = payload_line(&moved.join(binary_name()));

    if EMBEDDED_FROM.is_some_and(|dir| !dir.is_empty()) {
        let digest = embedded_digest(&inside);
        assert_eq!(
            embedded_digest(&outside),
            digest,
            "the moved binary lost its payload"
        );
        assert_ne!(
            digest, directory_digest,
            "the directory beside the binary is not the embedded payload, and must not be used"
        );
    } else {
        // A development build uses the package directory beside it when that matches its receipt,
        // and has nothing once moved away.
        assert_eq!(
            inside,
            format!("payload: package directory beside the binary, sha256 {directory_digest}")
        );
        assert_development_build(&outside);
        skip(
            TEST,
            "this build embedded no payload (PASSPORTSIM_EMBED_PAYLOAD unset at build time), so \
             the moved binary has no embedded copy to find; the directory fallback and the \
             development-build report were asserted",
        );
    }
    let _ = fs::remove_dir_all(root);
}
