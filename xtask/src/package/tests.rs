//! Tests of `cargo xtask package`, including the first run of a package on a clean machine.
//!
//! The shared package ([`packaged`]) is the one this host builds. The Windows-only mechanics are
//! tested over synthetic images in `windows/tests.rs`, and the archive formats in `archive.rs`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::demo::{self, Found};
use super::{Options, guard, layout};

/// The package both slow tests share, built once for this test binary.
fn packaged() -> &'static super::Built {
    static BUILT: OnceLock<super::Built> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = repo_root();
        let out = super::target_dir(&root)
            .join("package-test")
            .join(std::process::id().to_string());
        let payload_from = std::env::var_os(PAYLOAD_FROM_ENV)
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from);
        let options = Options {
            target: host_target().to_string(),
            out: Some(out),
            // The archives have their own test; skipping them here avoids compressing the demo twice.
            archive: false,
            payload_from,
            demo: None,
            version: None,
        };
        // `demo::find` resolves nothing in a test build, so the demo can only come from the source
        // package.
        let built = if options.payload_from.is_some() {
            super::build(&root, &options)
        } else {
            super::build_with_demo(&root, &options, corpus_demo_of_this_host())
        };
        built.unwrap_or_else(|e| panic!("xtask package: {e}"))
    })
}

/// The bytes a gzip member holds.
fn gunzip(packed: &[u8]) -> Vec<u8> {
    let mut plain = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(packed), &mut plain)
        .expect("a gzip member");
    plain
}

fn host_target() -> &'static str {
    super::host_target()
        .expect("the package tests run on a host that packages: macOS arm64 or Windows x64")
}

fn binary_name() -> &'static str {
    layout::target_shape(host_target())
        .expect("a package shape")
        .1
}

/// A package directory [`packaged`] takes the wasm core and the demo from, as `--payload-from`
/// does: how the Windows host, which has no corpus, gets a macOS payload.
const PAYLOAD_FROM_ENV: &str = "PEMU_PAYLOAD_FROM";

/// The demo for [`packaged`], from the corpus of the data root the tier injected, or
/// [`Found::Absent`] when this run has none.
///
/// A `cfg(test)` build resolves no host directory role, so this reads `PASSPORTSIM_DATA_ROOT` and
/// writes a map pinning the bytes on disk; `demo::from_map` still checks the compiled-in prefixes,
/// so only the pinned image can come out.
#[cfg(not(unix))]
fn corpus_demo_of_this_host() -> Found {
    absent_demo(
        "no PEMU_PAYLOAD_FROM: the firmware corpus is macOS-only, so a \
         Windows test package takes the demo of a macOS package named by that variable, and this \
         run named none",
    )
}

fn absent_demo(reason: &str) -> Found {
    Found::Absent(super::demo::Absent {
        reason: reason.into(),
    })
}

#[cfg(unix)]
fn corpus_demo_of_this_host() -> Found {
    let absent = absent_demo;
    let Some(root) = std::env::var_os("PASSPORTSIM_DATA_ROOT").map(PathBuf::from) else {
        return absent(
            "no PASSPORTSIM_DATA_ROOT: a plain `cargo test` has no data root, so this package is \
             built without the demo",
        );
    };
    // A home whose data-root role is this run's data root, so the upstream checkout the NOTICE
    // commit re-check reads is the real one.
    let home = tempdir("demo-home");
    let base = home.join("Library").join("Application Support");
    std::fs::create_dir_all(&base).expect("the data-root role of the temporary home");
    if std::os::unix::fs::symlink(&root, base.join("passportsim")).is_err() {
        return absent("the data root could not be linked into a temporary home");
    }

    let corpus = PathBuf::from(crate::hostdirs::DEFAULT_DATA_ROOT)
        .join("corpus")
        .join(demo::DEMO_ID);
    let (bin, elf) = ("FoloToy-AI-Passport-8MB.bin", "FoloToy-AI-Passport.elf");
    let named = |file: &str| format!("~/{}/{file}", corpus.display());
    let on_disk = root.join("corpus").join(demo::DEMO_ID);
    let (Some(bin_sha), Some(elf_sha)) = (
        crate::ci::corpus::sha256_file(&on_disk.join(bin)),
        crate::ci::corpus::sha256_file(&on_disk.join(elf)),
    ) else {
        return absent("the data root of this run has no `official` corpus image and ELF");
    };
    let map = format!(
        "[{id}]\nbin = \"{bin}\"\nelf = \"{elf}\"\n\
         sha256 = {{ bin = \"{bin_sha}\", elf = \"{elf_sha}\" }}\n",
        id = demo::DEMO_ID,
        bin = named(bin),
        elf = named(elf),
    );
    demo::from_map(&map, &home)
}

/// The workspace root with its generated documentation up to date: `layout::write` copies
/// `docs/errors.md`, which git ignores, so a fresh checkout lacks it.
fn root_with_generated_docs() -> PathBuf {
    static GENERATED: OnceLock<()> = OnceLock::new();
    // This writes the real workspace root: `docs::run` with no arguments regenerates the files the
    // `docs-check` step of `ci t0` compares.
    GENERATED.get_or_init(|| crate::docs::run(&[]).expect("xtask docs"));
    repo_root()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent")
        .to_path_buf()
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_target_is_required_and_only_the_supported_hosts_are_accepted() {
    assert!(
        super::parse(&args(&[]))
            .unwrap_err()
            .contains("--target is required")
    );
    let linux = super::parse(&args(&["--target", "x86_64-unknown-linux-gnu"])).unwrap_err();
    assert!(linux.contains("Linux is not supported"), "{linux}");
    let macos = super::parse(&args(&["--target", super::MACOS_TARGET]))
        .unwrap()
        .unwrap();
    assert_eq!(macos.target, super::MACOS_TARGET);
    assert!(macos.archive);
    assert!(macos.out.is_none());
    let no_archive = super::parse(&args(&[
        "--target",
        super::MACOS_TARGET,
        "--no-archive",
        "--out",
        "/tmp/x",
    ]))
    .unwrap()
    .unwrap();
    assert!(!no_archive.archive);
    assert_eq!(no_archive.out, Some(PathBuf::from("/tmp/x")));
    assert_eq!(no_archive.payload_from, None);
    let payload_from = super::parse(&args(&[
        "--target",
        super::WINDOWS_TARGETS[0],
        "--payload-from",
        "pkg",
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(payload_from.payload_from, Some(PathBuf::from("pkg")));
    assert!(
        super::parse(&args(&["--target", super::MACOS_TARGET, "--payload-from"]))
            .unwrap_err()
            .contains("--payload-from needs")
    );
    let demo = super::parse(&args(&["--target", super::MACOS_TARGET, "--demo", "dir"]))
        .unwrap()
        .unwrap();
    assert_eq!(demo.demo, Some(PathBuf::from("dir")));
    assert_eq!(demo.payload_from, None);
    let both = super::parse(&args(&[
        "--target",
        super::MACOS_TARGET,
        "--demo",
        "dir",
        "--payload-from",
        "pkg",
    ]))
    .unwrap_err();
    assert!(both.contains("cannot be combined"), "{both}");
    assert_eq!(macos.version, None);
    let release = super::parse(&args(&[
        "--target",
        super::MACOS_TARGET,
        "--version",
        "0.2.0",
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(release.version.as_deref(), Some("0.2.0"));
    for bad in [
        &["--version"][..],
        &["--version", "v0.2.0"],
        &["--version", "0.2"],
    ] {
        let mut list = vec!["--target", super::MACOS_TARGET];
        list.extend_from_slice(bad);
        let refused = super::parse(&args(&list)).unwrap_err();
        assert!(refused.contains("--version"), "{refused}");
    }
}

/// Each target is packaged on its own host, and the Arm64 Windows target is NOT_RUN with its
/// reason rather than failed.
#[test]
fn a_target_is_packaged_on_its_own_host_and_arm64_windows_is_not_run() {
    let out = tempdir("foreign-target");
    super::run(&args(&[
        "--target",
        "aarch64-pc-windows-msvc",
        "--out",
        out.to_str().unwrap(),
    ]))
    .unwrap();
    assert_eq!(super::ARM64_WINDOWS_REASON, "no-arm-windows-host");
    // The other host's target is refused by name before anything is built.
    let foreign = [super::MACOS_TARGET, super::WINDOWS_TARGETS[0]]
        .into_iter()
        .find(|target| *target != host_target())
        .expect("two package hosts");
    let options = Options {
        target: foreign.to_string(),
        out: Some(out.clone()),
        archive: false,
        payload_from: None,
        demo: None,
        version: None,
    };
    let refused = super::build(&repo_root(), &options).unwrap_err();
    assert!(
        refused.contains(foreign) && refused.contains("is packaged on"),
        "{refused}"
    );
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        0,
        "nothing written"
    );
}

/// A corpus map whose `official` entry points into `home`. The paths are TOML literal strings, so
/// a Windows backslash is not an escape.
fn corpus_map(home: &Path, bin_sha: &str, elf_sha: &str) -> String {
    let dir = home.join("corpus/official");
    format!(
        "[official]\nbin = '{dir}/image.bin'\nelf = '{dir}/app.elf'\n\
         sha256 = {{ bin = \"{bin_sha}\", elf = \"{elf_sha}\" }}\n",
        dir = dir.display()
    )
}

/// A MAC-shaped string the `mac-shape` rule refuses, assembled at run time because the pre-commit
/// hook applies the same rule to this file.
fn planted_mac() -> String {
    ["de", "ad", "be", "ef", "00", "01"].join(":")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn a_host_without_the_corpus_packages_without_the_demo_and_says_why() {
    let home = tempdir("demo-absent");
    let found = demo::from_map("", &home);
    let reason = found.absent_reason().expect("absent");
    assert!(reason.contains("no `official` entry"), "{reason}");
    assert!(
        !reason.contains('/'),
        "a reason never carries a path: {reason}"
    );

    let bad = demo::from_map("this is not toml = ", &home);
    assert!(bad.absent_reason().unwrap().contains("does not parse"));
}

#[test]
fn an_unpinned_or_mismatched_demo_image_is_never_embedded() {
    let home = tempdir("demo-pins");
    let dir = home.join("corpus/official");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("image.bin"), b"merged image").unwrap();
    std::fs::write(dir.join("app.elf"), b"application elf").unwrap();

    let unpinned = format!(
        "[official]\nbin = '{dir}/image.bin'\nelf = '{dir}/app.elf'\n",
        dir = dir.display()
    );
    let reason = demo::from_map(&unpinned, &home)
        .absent_reason()
        .unwrap()
        .to_string();
    assert!(reason.contains("pins no SHA-256"), "{reason}");

    let wrong = corpus_map(&home, &"0".repeat(64), &"1".repeat(64));
    let reason = demo::from_map(&wrong, &home)
        .absent_reason()
        .unwrap()
        .to_string();
    assert!(reason.contains("does not match the SHA-256"), "{reason}");
}

/// The canonical MIT text, abridged to the three sentences `demo::MIT_MARKERS` requires.
const MIT_TEXT: &str = "MIT License\n\nCopyright (c) 2026 FoloToy\n\n\
     Permission is hereby granted, free of charge, to any person obtaining a copy of this \
     software and associated documentation files (the \"Software\"), to deal in the Software \
     without restriction.\n\nThe above copyright notice and this permission notice shall be \
     included in all copies.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY \
     KIND.\n";

/// A demo corpus under `home`: the two files, their map, and pins made of the leading 16 hex
/// digits of these files' digests, since a synthetic file cannot match the real prefix.
fn demo_corpus(home: &Path, bin: &[u8], elf: &[u8]) -> (String, Vec<(String, String)>) {
    let dir = home.join("corpus/official");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("image.bin"), bin).unwrap();
    std::fs::write(dir.join("app.elf"), elf).unwrap();
    let (bin_sha, elf_sha) = (sha256_hex(bin), sha256_hex(elf));
    let pins = vec![
        ("bin".to_string(), bin_sha[..16].to_string()),
        ("elf".to_string(), elf_sha[..16].to_string()),
    ];
    (corpus_map(home, &bin_sha, &elf_sha), pins)
}

fn from_corpus(map: &str, home: &Path, pins: &[(String, String)]) -> Found {
    let borrowed: Vec<(&str, &str)> = pins
        .iter()
        .map(|(kind, prefix)| (kind.as_str(), prefix.as_str()))
        .collect();
    demo::from_map_with(map, home, &borrowed)
}

#[test]
fn a_demo_image_is_never_shipped_without_its_upstream_license() {
    let home = tempdir("demo-license");
    let dir = home.join("corpus/official");
    let bin = b"merged image".to_vec();
    let elf = b"application elf".to_vec();
    let (map, pins) = demo_corpus(&home, &bin, &elf);

    let reason = from_corpus(&map, &home, &pins)
        .absent_reason()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("never shipped without its license"),
        "{reason}"
    );

    std::fs::write(dir.join("LICENSE"), "GNU General Public License\n").unwrap();
    assert!(from_corpus(&map, &home, &pins).absent_reason().is_some());

    // The NOTICE says the file is the MIT licence, so the grant and the disclaimer must be there too.
    std::fs::write(
        dir.join("LICENSE"),
        "MIT License\n\nYou may not use this software.\n",
    )
    .unwrap();
    assert!(from_corpus(&map, &home, &pins).absent_reason().is_some());

    std::fs::write(dir.join("LICENSE"), MIT_TEXT).unwrap();
    let Found::Embedded(embedded) = from_corpus(&map, &home, &pins) else {
        panic!("the demo should be embedded once its license is there");
    };
    assert_eq!(embedded.bin_sha256, sha256_hex(&bin));
    let bundle = pemu_loader::bundle::Bundle::parse(&embedded.bundle).expect("parses");
    assert_eq!(bundle.id(), Some(demo::DEMO_ID));
    assert_eq!(
        bundle.role_data(pemu_loader::bundle::BUNDLE_FLASH),
        Some(bin.as_slice())
    );
    assert_eq!(
        bundle.role_data(pemu_loader::bundle::BUNDLE_APP_ELF),
        Some(elf.as_slice())
    );
    // No upstream checkout under this temporary home, so the commit line says the pin was not
    // re-checked, and why.
    let notice = embedded.notice();
    assert!(notice.contains(demo::UPSTREAM_COMMIT));
    assert!(notice.contains(&embedded.elf_sha256));
    assert!(!notice.contains(home.to_str().unwrap()));
    assert_eq!(
        embedded.commit,
        demo::CommitCheck::NotRechecked("the upstream checkout is not readable here")
    );
    assert!(!embedded.commit.rechecked());
    assert!(
        notice.contains("the upstream checkout is not readable here"),
        "{notice}"
    );
}

/// The corpus map cannot vouch for itself: it supplies path and digest, so the compiled-in prefix
/// is the pin that says which file `official` is.
#[test]
fn the_pinned_prefix_refuses_a_file_the_corpus_map_vouches_for() {
    let home = tempdir("demo-plan-pin");
    let dir = home.join("corpus/official");
    let bin = b"any file at all".to_vec();
    let elf = b"any other file".to_vec();
    let (map, pins) = demo_corpus(&home, &bin, &elf);
    std::fs::write(dir.join("LICENSE"), MIT_TEXT).unwrap();

    // Every check before the prefix passes: with the prefix substituted, this corpus embeds.
    assert!(matches!(
        from_corpus(&map, &home, &pins),
        Found::Embedded(_)
    ));

    let reason = demo::from_map(&map, &home)
        .absent_reason()
        .expect("a file the corpus map merely agrees with is not `official`")
        .to_string();
    assert!(
        reason.contains("pinned SHA-256 prefix"),
        "the refusal names the pin it failed: {reason}"
    );
    assert!(!reason.contains('/'), "a reason never carries a path");
}

/// An image the secret guard would refuse to commit is never put into a distributable. The raw
/// corpus bytes are scanned, because inside the `.pebundle` the header shifts every flash offset.
#[test]
fn an_image_the_secret_guard_would_refuse_is_never_embedded() {
    let home = tempdir("demo-secrets");
    let dir = home.join("corpus/official");
    let elf = b"application elf".to_vec();

    // A flash image with one programmed byte in the cardid window [0x356000, 0x35A000).
    let mut bin = vec![0xFFu8; 0x35_A000];
    bin[0x35_7000] = 0x42;
    let (map, pins) = demo_corpus(&home, &bin, &elf);
    std::fs::write(dir.join("LICENSE"), MIT_TEXT).unwrap();
    let reason = from_corpus(&map, &home, &pins)
        .absent_reason()
        .expect("the cardid window is not erased")
        .to_string();
    assert!(reason.contains("`cardid-window`"), "{reason}");
    assert!(
        reason.contains("0x357000"),
        "the offset is reported: {reason}"
    );
    assert!(!reason.contains('/'), "a reason never carries a path");

    // The same image with the window erased passes, so the window is what refused it.
    let erased = vec![0xFFu8; 0x35_A000];
    let (map, pins) = demo_corpus(&home, &erased, &elf);
    assert!(matches!(
        from_corpus(&map, &home, &pins),
        Found::Embedded(_)
    ));

    // The ELF is scanned by the rules that are not flash offsets: a big ELF does not trip the cardid
    // window (the real `official.elf` is 16 MB), but a MAC does.
    let mac_elf = format!("listening on {} today", planted_mac()).into_bytes();
    let (map, pins) = demo_corpus(&home, &erased, &mac_elf);
    let reason = from_corpus(&map, &home, &pins)
        .absent_reason()
        .expect("a MAC-shaped string in the ELF")
        .to_string();
    assert!(reason.contains("`mac-shape`"), "{reason}");

    let big_elf = vec![0x41u8; 0x35_A000];
    let (map, pins) = demo_corpus(&home, &erased, &big_elf);
    assert!(
        matches!(from_corpus(&map, &home, &pins), Found::Embedded(_)),
        "the cardid window is a flash offset, not an offset into every binary"
    );
}

/// A NOTICE never asserts a commit id the packaging host contradicts: a checkout at another commit
/// drops the demo; no checkout keeps it, and the NOTICE says the pin was not re-checked.
#[test]
fn an_upstream_checkout_at_another_commit_refuses_the_demo() {
    let home = tempdir("demo-commit");
    let dir = home.join("corpus/official");
    let bin = b"merged image".to_vec();
    let elf = b"application elf".to_vec();
    let (map, pins) = demo_corpus(&home, &bin, &elf);
    std::fs::write(dir.join("LICENSE"), MIT_TEXT).unwrap();

    let Found::Embedded(embedded) = from_corpus(&map, &home, &pins) else {
        panic!("no upstream checkout is not a reason to drop the demo");
    };
    assert!(!embedded.commit.rechecked());

    let upstream = home
        .join(crate::hostdirs::DEFAULT_DATA_ROOT)
        .join("builds/official");
    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "--quiet"]);
    std::fs::write(upstream.join("README"), "upstream").unwrap();
    git(&upstream, &["add", "README"]);
    git(
        &upstream,
        &[
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "user.name=test",
            // A global config that signs commits through an unreachable agent must not decide whether this
            // fixture commit can be made.
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "upstream",
        ],
    );
    let reason = from_corpus(&map, &home, &pins)
        .absent_reason()
        .expect("a checkout at another commit refuses the demo")
        .to_string();
    assert!(
        reason.contains(demo::UPSTREAM_COMMIT) && reason.contains("not the"),
        "the refusal names both commits: {reason}"
    );
    assert!(!reason.contains(home.to_str().unwrap()), "no host path");
}

/// A package directory holding the demo [`from_corpus`] builds from `bin` and `elf`, laid out as
/// `layout::write` does, with the pins `demo::from_package_with` accepts.
fn demo_package(name: &str, bin: &[u8], elf: &[u8]) -> (PathBuf, Vec<(String, String)>) {
    let home = tempdir(&format!("{name}-home"));
    let (map, pins) = demo_corpus(&home, bin, elf);
    std::fs::write(home.join("corpus/official/LICENSE"), MIT_TEXT).unwrap();
    let Found::Embedded(embedded) = from_corpus(&map, &home, &pins) else {
        panic!("the synthetic corpus embeds");
    };
    let package = tempdir(&format!("{name}-package"));
    let firmware = package.join("payload/firmware");
    std::fs::create_dir_all(&firmware).unwrap();
    std::fs::write(firmware.join(demo::BUNDLE_FILE), &embedded.bundle).unwrap();
    std::fs::write(firmware.join("official-demo.LICENSE"), &embedded.license).unwrap();
    std::fs::write(firmware.join("official-demo.NOTICE"), embedded.notice()).unwrap();
    (package, pins)
}

/// `--payload-from` takes another package's demo only when it is the demo this commit would have
/// written: the same bundle bytes, the licence and a notice for those digests.
#[test]
fn a_demo_from_another_package_is_the_same_three_files_or_refused() {
    let bin = b"merged image".to_vec();
    let elf = b"application elf".to_vec();
    let (package, pins) = demo_package("demo-from", &bin, &elf);
    let borrowed: Vec<(&str, &str)> = pins.iter().map(|(k, p)| (k.as_str(), p.as_str())).collect();
    let firmware = package.join("payload/firmware");

    let Found::Embedded(taken) = demo::from_package_with(&package, &borrowed).unwrap() else {
        panic!("a package's own demo is taken");
    };
    assert_eq!(taken.source, demo::Source::Package);
    assert_eq!(taken.bin_sha256, sha256_hex(&bin));
    assert_eq!(
        taken.bundle,
        std::fs::read(firmware.join(demo::BUNDLE_FILE)).unwrap()
    );
    assert_eq!(
        taken.notice(),
        std::fs::read_to_string(firmware.join("official-demo.NOTICE")).unwrap(),
        "the notice this host writes is the one it was given, byte for byte"
    );
    assert_eq!(
        taken.commit,
        demo::CommitCheck::NotRechecked("the upstream checkout is not readable here"),
        "the commit verdict is the one the notice states"
    );

    let refused = demo::from_package(&package).unwrap_err();
    assert!(refused.contains("pinned SHA-256 prefix"), "{refused}");

    let notice = firmware.join("official-demo.NOTICE");
    let original = std::fs::read_to_string(&notice).unwrap();
    std::fs::write(&notice, original.replace("f75873f", "0000000")).unwrap();
    let refused = demo::from_package_with(&package, &borrowed).unwrap_err();
    assert!(refused.contains("NOTICE"), "{refused}");
    std::fs::write(&notice, &original).unwrap();

    let license = firmware.join("official-demo.LICENSE");
    std::fs::write(&license, "GNU General Public License\n").unwrap();
    let refused = demo::from_package_with(&package, &borrowed).unwrap_err();
    assert!(refused.contains("MIT"), "{refused}");
    std::fs::write(&license, MIT_TEXT).unwrap();

    let bundle = firmware.join(demo::BUNDLE_FILE);
    let good = std::fs::read(&bundle).unwrap();
    let other = pemu_loader::bundle::build(
        Some(demo::DEMO_ID),
        Some("another name"),
        &[
            pemu_loader::bundle::BundleInput {
                role: pemu_loader::bundle::BUNDLE_FLASH,
                name: "FoloToy-AI-Passport-8MB.bin",
                bytes: &bin,
            },
            pemu_loader::bundle::BundleInput {
                role: pemu_loader::bundle::BUNDLE_APP_ELF,
                name: "FoloToy-AI-Passport.elf",
                bytes: &elf,
            },
        ],
    );
    std::fs::write(&bundle, other).unwrap();
    let refused = demo::from_package_with(&package, &borrowed).unwrap_err();
    assert!(
        refused.contains("not the bundle this commit writes"),
        "{refused}"
    );
    std::fs::write(&bundle, good).unwrap();

    // A package without a demo is an error: the caller asked for one.
    let empty = tempdir("demo-from-empty");
    let refused = demo::from_package(&empty).unwrap_err();
    assert!(refused.contains("not readable"), "{refused}");
}

/// `--demo` takes the three files from a plain directory. Its notice may be another commit's
/// wording but must state the files' own digests and the pinned commit.
#[test]
fn a_demo_directory_needs_its_own_digests_in_the_notice_but_not_its_wording() {
    let bin = b"merged image".to_vec();
    let elf = b"application elf".to_vec();
    let (package, pins) = demo_package("demo-dir", &bin, &elf);
    let borrowed: Vec<(&str, &str)> = pins.iter().map(|(k, p)| (k.as_str(), p.as_str())).collect();
    let dir = package.join("payload/firmware");
    let notice = dir.join("official-demo.NOTICE");
    let current = std::fs::read_to_string(&notice).unwrap();

    let Found::Embedded(taken) = demo::from_dir_with(&dir, &borrowed).unwrap() else {
        panic!("the directory's demo is taken");
    };
    assert_eq!(taken.source, demo::Source::Dir);
    assert_eq!(taken.source.as_str(), "demo-dir");
    assert_eq!(taken.notice(), current);

    // Another wording of the same facts: accepted by `--demo`, refused by `--payload-from`, whose
    // source is the same tree and so writes the same notice.
    let reworded = format!("An older notice.\n\n{current}").replace(
        "It is not part of the emulator",
        "It is no part of the emulator",
    );
    std::fs::write(&notice, &reworded).unwrap();
    let Found::Embedded(taken) = demo::from_dir_with(&dir, &borrowed).unwrap() else {
        panic!("a reworded notice with the same digests is taken");
    };
    assert_eq!(
        taken.notice(),
        current,
        "the package gets this commit's notice"
    );
    let refused = demo::from_package_with(&package, &borrowed).unwrap_err();
    assert!(
        refused.contains("not a notice this commit writes"),
        "{refused}"
    );

    let other_bin = current.replace(&sha256_hex(&bin), &"0".repeat(64));
    std::fs::write(&notice, other_bin).unwrap();
    let refused = demo::from_dir_with(&dir, &borrowed).unwrap_err();
    assert!(
        refused.starts_with("--demo:") && refused.contains("merged flash image"),
        "{refused}"
    );
    std::fs::write(&notice, current.replace(demo::UPSTREAM_COMMIT, "0000000")).unwrap();
    let refused = demo::from_dir_with(&dir, &borrowed).unwrap_err();
    assert!(refused.contains("does not name commit"), "{refused}");
    std::fs::write(&notice, &current).unwrap();

    let refused = demo::from_dir(&dir).unwrap_err();
    assert!(refused.contains("pinned SHA-256 prefix"), "{refused}");
    let refused = demo::from_dir(&tempdir("demo-dir-empty")).unwrap_err();
    assert!(
        refused.contains("`official.pebundle` in the named directory"),
        "{refused}"
    );
}

/// The demo's application ELF ships with its build paths blanked, and the NOTICE, the receipt and
/// `--payload-from` name both ELFs: the pinned one and the shipped one.
#[test]
fn the_demo_elf_ships_with_its_build_paths_blanked_and_both_digests_named() {
    let bin = b"merged image".to_vec();
    let elf = super::elf_paths::tests::elf32(&[
        (".flash.text", 0x6, b"\x13\x00\x00\x00"),
        (".debug_str", 0x30, b"/Users/builder/esp/idf/x.c\0main\0"),
    ]);
    let home = tempdir("demo-blank-home");
    let (map, pins) = demo_corpus(&home, &bin, &elf);
    std::fs::write(home.join("corpus/official/LICENSE"), MIT_TEXT).unwrap();
    let Found::Embedded(embedded) = from_corpus(&map, &home, &pins) else {
        panic!("the synthetic corpus embeds");
    };
    assert_eq!(
        embedded.elf_sha256,
        sha256_hex(&elf),
        "the pin is the corpus ELF"
    );
    assert_eq!(embedded.elf_paths_blanked, 1);
    let bundle = pemu_loader::bundle::Bundle::parse(&embedded.bundle).expect("parses");
    let shipped = bundle
        .role_data(pemu_loader::bundle::BUNDLE_APP_ELF)
        .expect("the ELF")
        .to_vec();
    assert_eq!(shipped.len(), elf.len());
    assert_ne!(shipped, elf);
    assert_eq!(embedded.elf_shipped_sha256, sha256_hex(&shipped));
    assert!(super::account::profile_path_spans(&shipped).is_empty());
    assert_eq!(
        bundle.role_data(pemu_loader::bundle::BUNDLE_FLASH),
        Some(bin.as_slice()),
        "the flash image is the pinned one"
    );
    let notice = embedded.notice();
    for line in [
        format!("SHA-256, application ELF:    {}", embedded.elf_sha256),
        format!(
            "SHA-256, application ELF as shipped: {}",
            embedded.elf_shipped_sha256
        ),
        "Build paths blanked in it:   1".to_string(),
    ] {
        assert!(notice.contains(&line), "{notice}");
    }

    let package = tempdir("demo-blank-package");
    let firmware = package.join("payload/firmware");
    std::fs::create_dir_all(&firmware).unwrap();
    std::fs::write(firmware.join(demo::BUNDLE_FILE), &embedded.bundle).unwrap();
    std::fs::write(firmware.join("official-demo.LICENSE"), &embedded.license).unwrap();
    std::fs::write(firmware.join("official-demo.NOTICE"), &notice).unwrap();
    let borrowed: Vec<(&str, &str)> = pins.iter().map(|(k, p)| (k.as_str(), p.as_str())).collect();
    let Found::Embedded(taken) = demo::from_package_with(&package, &borrowed).unwrap() else {
        panic!("the package's demo is taken");
    };
    assert_eq!(taken.elf_sha256, embedded.elf_sha256);
    assert_eq!(taken.elf_shipped_sha256, embedded.elf_shipped_sha256);
    assert_eq!(taken.elf_paths_blanked, 1);
    assert_eq!(taken.bundle, embedded.bundle);

    let unblanked = pemu_loader::bundle::build(
        Some(demo::DEMO_ID),
        Some("FoloToy AI Passport BSP demo"),
        &[
            pemu_loader::bundle::BundleInput {
                role: pemu_loader::bundle::BUNDLE_FLASH,
                name: "FoloToy-AI-Passport-8MB.bin",
                bytes: &bin,
            },
            pemu_loader::bundle::BundleInput {
                role: pemu_loader::bundle::BUNDLE_APP_ELF,
                name: "FoloToy-AI-Passport.elf",
                bytes: &elf,
            },
        ],
    );
    std::fs::write(firmware.join(demo::BUNDLE_FILE), unblanked).unwrap();
    let refused = demo::from_package_with(&package, &borrowed).unwrap_err();
    assert!(refused.contains("still holds a build path"), "{refused}");
}

/// The embedded payload is byte-identical across hosts: a macOS-shaped and a Windows-shaped
/// package of the same inputs have the same payload digest, and only the binary's name differs.
#[test]
fn a_windows_package_with_the_demo_of_a_macos_one_has_the_same_payload_digest() {
    let root = root_with_generated_docs();
    let bin = b"merged image".to_vec();
    let elf = b"application elf".to_vec();
    let (source, pins) = demo_package("cross-host", &bin, &elf);
    let borrowed: Vec<(&str, &str)> = pins.iter().map(|(k, p)| (k.as_str(), p.as_str())).collect();
    let home = tempdir("cross-host-corpus");
    let (map, pins) = demo_corpus(&home, &bin, &elf);
    std::fs::write(home.join("corpus/official/LICENSE"), MIT_TEXT).unwrap();
    let from_corpus = from_corpus(&map, &home, &pins);
    let from_package = demo::from_package_with(&source, &borrowed).unwrap();

    let env = guard::env().unwrap();
    let mut digests = Vec::new();
    for (target, found) in [
        (super::MACOS_TARGET, &from_corpus),
        (super::WINDOWS_TARGETS[0], &from_package),
    ] {
        let out = tempdir(&format!("cross-host-{target}"));
        let src = synthetic_inputs(&out.join("src"));
        let inputs = layout::Inputs {
            binary: &src.join("passportsim"),
            wasm: &src.join("pemu_wasm.wasm"),
            web: &src,
            demo: found,
            target,
            secrets_env: &env,
            version: env!("CARGO_PKG_VERSION"),
            checkout: &super::receipt::Checkout::read(&root),
        };
        let built = layout::write(&root, &out, &inputs).unwrap();
        let (_, binary) = layout::target_shape(target).unwrap();
        assert!(built.package_dir.join(binary).is_file());
        assert!(
            built
                .package_dir
                .join("payload/firmware")
                .join(demo::BUNDLE_FILE)
                .is_file()
        );
        digests.push(built.receipt.payload.clone());
    }
    assert_eq!(
        digests[0], digests[1],
        "the payload of the two targets is one payload, file for file"
    );
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_payload_digest_covers_content_and_paths_and_skips_the_binary_and_the_receipt() {
    let dir = tempdir("digest");
    std::fs::create_dir_all(dir.join("payload")).unwrap();
    std::fs::write(dir.join("passportsim"), b"the binary").unwrap();
    std::fs::write(dir.join("receipt.json"), b"{}").unwrap();
    std::fs::write(dir.join("payload/a"), b"one").unwrap();
    std::fs::write(dir.join("payload/b"), b"two").unwrap();
    let first = layout::digest(&dir, "passportsim").unwrap();
    assert_eq!(
        first
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["payload/a", "payload/b"],
        "the binary and the receipt are not payload, and files are sorted by path"
    );

    std::fs::write(dir.join("passportsim"), b"a different binary").unwrap();
    std::fs::write(dir.join("receipt.json"), b"{\"x\":1}").unwrap();
    assert_eq!(
        layout::digest(&dir, "passportsim").unwrap().sha256,
        first.sha256
    );

    std::fs::write(dir.join("payload/b"), b"three").unwrap();
    assert_ne!(
        layout::digest(&dir, "passportsim").unwrap().sha256,
        first.sha256
    );

    // Moving a file changes it too: the path is hashed with the content.
    std::fs::write(dir.join("payload/b"), b"two").unwrap();
    std::fs::rename(dir.join("payload/a"), dir.join("payload/c")).unwrap();
    assert_ne!(
        layout::digest(&dir, "passportsim").unwrap().sha256,
        first.sha256
    );
}

/// The payload excludes the binary of this target, not the literal `passportsim`.
#[test]
fn the_package_shape_follows_the_target_and_never_the_macos_literal() {
    assert_eq!(
        layout::target_shape(super::MACOS_TARGET).unwrap(),
        ("macos-arm64", "passportsim")
    );
    for target in super::WINDOWS_TARGETS {
        let (suffix, binary) = layout::target_shape(target).unwrap();
        assert!(suffix.starts_with("windows-"), "{target}: {suffix}");
        assert_eq!(binary, "passportsim.exe");
    }
    assert!(layout::target_shape("x86_64-unknown-linux-gnu").is_err());

    let dir = tempdir("digest-windows");
    std::fs::create_dir_all(dir.join("payload")).unwrap();
    std::fs::write(dir.join("passportsim.exe"), b"the binary").unwrap();
    std::fs::write(dir.join("receipt.json"), b"{}").unwrap();
    std::fs::write(dir.join("payload/a"), b"one").unwrap();
    let payload = layout::digest(&dir, "passportsim.exe").unwrap();
    assert_eq!(
        payload
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["payload/a"],
        "`passportsim.exe` is the binary of that target, not payload"
    );
}

/// `roms_embedded` is the receipt's one measured fact, so an unreadable file is an error rather
/// than `false`.
#[test]
fn roms_embedded_measures_the_artifact_and_fails_when_it_cannot() {
    let dir = tempdir("roms");
    let mut rom = vec![0u8; 1024];
    for (i, byte) in rom.iter_mut().enumerate() {
        *byte = (i % 251) as u8;
    }
    let window = rom[rom.len() / 2..rom.len() / 2 + 256].to_vec();
    std::fs::write(dir.join("rom.elf"), &rom).unwrap();

    let mut carrying = b"header bytes".to_vec();
    carrying.extend_from_slice(&window);
    carrying.extend_from_slice(b"trailer bytes");
    std::fs::write(dir.join("with.wasm"), &carrying).unwrap();
    std::fs::write(dir.join("without.wasm"), vec![0xAAu8; 4096]).unwrap();

    assert!(layout::roms_embedded(&dir.join("with.wasm"), &dir.join("rom.elf")).unwrap());
    assert!(!layout::roms_embedded(&dir.join("without.wasm"), &dir.join("rom.elf")).unwrap());

    assert!(layout::roms_embedded(&dir.join("gone.wasm"), &dir.join("rom.elf")).is_err());
    assert!(layout::roms_embedded(&dir.join("with.wasm"), &dir.join("gone.elf")).is_err());
    std::fs::write(dir.join("tiny.elf"), b"\x7fELF").unwrap();
    assert!(layout::roms_embedded(&dir.join("with.wasm"), &dir.join("tiny.elf")).is_err());
}

#[test]
fn the_package_carries_everything_it_documents() {
    let built = packaged();
    let dir = &built.package_dir;
    let mut required = vec![
        binary_name(),
        "receipt.json",
        "LICENSE",
        "THIRD_PARTY.md",
        "docs/quickstart.md",
        "docs/errors.md",
        "docs/commands/index.md",
        "docs/i18n/zh-CN/quickstart.md",
        "docs/i18n/ja/quickstart.md",
        "docs/i18n/fr/quickstart.md",
        "skills/passportsim/SKILL.md",
        "skills/passportsim/device-deny.json",
        "assets/rom/LICENSE",
        "assets/rom/NOTICE",
        "assets/rom/pins.toml",
        "payload/schema/error@1.json",
        "payload/web/index.html",
        "payload/web/worker.js",
        "payload/web/main.js",
        "payload/web/worklet.js",
        "payload/web/styles.css",
        "payload/web/favicon.svg",
        "payload/web/favicon-32.png",
        "payload/web/apple-touch-icon.png",
        "payload/web/pemu_wasm.wasm",
    ];
    if matches!(
        built.receipt.demo,
        super::receipt::DemoRecord::Embedded { .. }
    ) {
        required.push("payload/firmware/official.pebundle");
        required.push("payload/firmware/official-demo.LICENSE");
        required.push("payload/firmware/official-demo.NOTICE");
    }
    for path in required {
        assert!(dir.join(path).is_file(), "missing from the package: {path}");
    }

    let root = repo_root();
    for entry in std::fs::read_dir(root.join("docs/commands")).unwrap() {
        let source = entry.unwrap().path();
        let name = source.file_name().unwrap();
        let shipped = dir.join("docs/commands").join(name);
        assert_eq!(
            std::fs::read(&source).unwrap(),
            std::fs::read(&shipped).unwrap(),
            "{} drifted from the generated file",
            shipped.display()
        );
    }

    for entry in std::fs::read_dir(root.join("docs/schema/commands")).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(dir.join("payload/schema/commands").join(&name).is_file());
    }

    let meta = std::fs::metadata(dir.join(binary_name())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(
            meta.permissions().mode() & 0o111 != 0,
            "the binary is executable"
        );
    }
    assert!(meta.len() > 1_000_000, "the binary is a real build");
}

/// Every relative link of the package's entry documents resolves inside the package.
/// `docs/secrets.md` links `.gitignore`, which a package does not carry, so it has its own test.
#[test]
fn every_relative_link_of_the_quickstart_and_the_skill_resolves_inside_the_package() {
    let built = packaged();
    let mut entries = vec![
        built.package_dir.join("skills/passportsim/SKILL.md"),
        built.package_dir.join("docs/quickstart.md"),
        built.package_dir.join("docs/deploy-cloudflare.md"),
    ];
    for lang in ["zh-CN", "ja", "fr"] {
        for name in ["quickstart.md", "deploy-cloudflare.md"] {
            entries.push(built.package_dir.join("docs/i18n").join(lang).join(name));
        }
    }
    let mut checked = 0usize;
    for file in entries {
        let dir = file.parent().expect("a parent").to_path_buf();
        let text = std::fs::read_to_string(&file).unwrap();
        let mut fenced = false;
        for line in text.lines() {
            if line.starts_with("```") {
                fenced = !fenced;
                continue;
            }
            // A link inside a fenced block is an illustration, such as the upstream index row the
            // skill shows, and does not have to resolve here.
            if fenced {
                continue;
            }
            for part in line.split("](").skip(1) {
                let Some(target) = part.split(')').next() else {
                    continue;
                };
                if target.starts_with("http") || target.starts_with('#') {
                    continue;
                }
                assert!(
                    normalize(&dir.join(target)).is_file(),
                    "{}: `{target}` does not resolve in the package",
                    file.display()
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked >= 8,
        "both documents link to what the package ships"
    );
}

/// The shipped reference document resolves every link to a file the package carries; its
/// repository-only targets are pinned.
#[test]
fn the_two_reference_documents_resolve_their_shipped_links() {
    const REPOSITORY_ONLY: [&str; 1] = ["../.gitignore"];
    let built = packaged();
    let mut resolved = 0usize;
    for name in ["secrets.md"] {
        let file = built.package_dir.join("docs").join(name);
        let dir = file.parent().expect("a parent").to_path_buf();
        let text = std::fs::read_to_string(&file).unwrap();
        for part in text.split("](").skip(1) {
            let Some(target) = part.split(')').next() else {
                continue;
            };
            if target.starts_with("http") || target.starts_with('#') {
                continue;
            }
            if REPOSITORY_ONLY.contains(&target) {
                assert!(
                    !normalize(&dir.join(target)).is_file(),
                    "{name}: `{target}` is listed as repository-only but the package carries it"
                );
                continue;
            }
            assert!(
                normalize(&dir.join(target)).is_file(),
                "{name}: `{target}` does not resolve in the package, and is not a \
                 repository-only document"
            );
            resolved += 1;
        }
    }
    assert!(
        resolved >= 5,
        "secrets.md links to what the package ships, {resolved} links"
    );
    for name in ["LICENSE", "NOTICE", "pins.toml"] {
        assert!(built.package_dir.join("assets/rom").join(name).is_file());
    }
}

/// Every command `SKILL.md` names as `passport_<name>` or `passportsim <name>` is in the registry.
#[test]
fn the_skill_names_no_command_the_registry_does_not_have() {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("skills/passportsim/SKILL.md")).unwrap();
    let registered: Vec<String> = crate::docs::registry_commands()
        .expect("the registry")
        .iter()
        .map(|command| command.spec.name.to_string())
        .collect();
    assert!(!registered.is_empty(), "the registry has commands");

    fn word(rest: &str) -> Option<String> {
        let mut chars = rest.chars();
        if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
            return None;
        }
        let end = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(rest.len());
        Some(rest[..end].to_owned())
    }

    // `passportsim <command>` in the prose is a placeholder and does not start with a lowercase
    // letter, so `word` passes it by. `serve` and `mcp` are the CLI's own subcommands
    // (`pemu-cli/src/tree.rs`), which host the registry rather than belong to it.
    const CLI_ONLY: [&str; 2] = ["serve", "mcp"];
    let mut named: Vec<String> = Vec::new();
    for (prefix, sep) in [("passportsim", " "), ("passport_", "")] {
        let needle = format!("{prefix}{sep}");
        let mut at = 0;
        while let Some(found) = text[at..].find(&needle) {
            let start = at + found + needle.len();
            if let Some(name) = word(&text[start..]) {
                named.push(name);
            }
            at = start;
        }
    }
    named.sort();
    named.dedup();
    // The floor proves the scan found something to check.
    assert!(
        !named.is_empty(),
        "SKILL.md names no command in either spelling, so this check found nothing to verify"
    );
    for name in named {
        assert!(
            registered.contains(&name) || CLI_ONLY.contains(&name.as_str()),
            "SKILL.md names `{name}`, which the command registry does not have"
        );
    }
}

/// A skill is installed by copying `skills/passportsim/` alone (`npx skills add`, or by hand), so
/// no link or relative path in any of its files may lead out of that directory, and every link
/// must name a file that is there. Checked on the repository's tree, which is what an install
/// copies.
#[test]
fn the_skill_links_nothing_outside_its_own_directory() {
    let skill = repo_root().join("skills/passportsim");
    let mut files = Vec::new();
    let mut pending = vec![skill.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    assert!(
        files.iter().any(|file| file.ends_with("SKILL.md")),
        "the walk found the skill"
    );
    let mut links = 0usize;
    for file in &files {
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("{}: every skill file is text: {e}", file.display()));
        let dir = file.parent().expect("a parent");
        let shown = file.strip_prefix(&skill).unwrap_or(file).display();
        for part in text.split("](").skip(1) {
            let target = part.split(')').next().unwrap_or_default();
            let target = target.split('#').next().unwrap_or_default();
            if target.is_empty() || target.starts_with("https://") {
                continue;
            }
            assert!(
                !target.contains("://"),
                "{shown}: `{target}` is a link, but not https"
            );
            let resolved = normalize(&dir.join(target));
            assert!(
                resolved.starts_with(&skill) && resolved.is_file(),
                "{shown}: `{target}` does not name a file inside skills/passportsim/"
            );
            links += 1;
        }
        // A relative path in prose or code leaves the directory the same way a link does.
        for (at, _) in text.match_indices("../") {
            let start = text[..at]
                .rfind(|c: char| c.is_whitespace() || "`'\"(".contains(c))
                .map_or(0, |i| i + 1);
            let end = text[at..]
                .find(|c: char| c.is_whitespace() || "`'\")".contains(c))
                .map_or(text.len(), |i| at + i);
            let token = &text[start..end];
            assert!(
                normalize(&dir.join(token)).starts_with(&skill),
                "{shown}: `{token}` leads out of skills/passportsim/"
            );
        }
    }
    assert!(links >= 4, "the skill links its references, {links} links");
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[test]
fn the_web_bundle_is_servable_and_carries_the_same_demo_as_the_package() {
    let built = packaged();
    for name in [
        "index.html",
        "styles.css",
        "main.js",
        "worker.js",
        "worklet.js",
        "favicon.svg",
        "favicon-32.png",
        "apple-touch-icon.png",
        "pemu_wasm.wasm",
    ] {
        assert!(
            built.web_dir.join(name).is_file(),
            "web bundle needs {name}"
        );
    }
    for name in [
        "LICENSE",
        "THIRD_PARTY.md",
        "licenses/esp-rom-elfs.LICENSE",
        "licenses/esp-rom-elfs.NOTICE",
    ] {
        assert!(
            built.web_dir.join(name).is_file(),
            "web bundle needs {name}"
        );
    }
    // The worker fetches `pemu_wasm.wasm` beside `worker.js`, and it is the core the package ships.
    let worker = std::fs::read_to_string(built.web_dir.join("worker.js")).unwrap();
    assert!(
        worker.contains("pemu_wasm.wasm"),
        "the worker names the core"
    );
    assert_eq!(
        std::fs::read(built.web_dir.join("pemu_wasm.wasm")).unwrap(),
        std::fs::read(built.package_dir.join("payload/web/pemu_wasm.wasm")).unwrap()
    );
    if let super::receipt::DemoRecord::Embedded { bundle_sha256, .. } = &built.receipt.demo {
        // The web bundle carries it gzip-compressed, for the wire (`layout::write_web_bundle`).
        let from_web = gunzip(&std::fs::read(built.web_dir.join(demo::BUNDLE_FILE)).unwrap());
        let from_package = std::fs::read(
            built
                .package_dir
                .join("payload/firmware")
                .join(demo::BUNDLE_FILE),
        )
        .unwrap();
        assert_eq!(from_web, from_package, "the same demo, byte for byte");
        assert_eq!(&sha256_hex(&from_web), bundle_sha256);
        assert!(
            built
                .web_dir
                .join("licenses/official-demo.LICENSE")
                .is_file()
        );
        pemu_loader::bundle::Bundle::parse(&from_web).expect("the shipped bundle parses");
    }
    for (name, text) in [
        (
            super::cloudflare::CONFIG_FILE,
            super::cloudflare::config_text(),
        ),
        (
            super::cloudflare::HEADERS_FILE,
            super::cloudflare::headers_text(),
        ),
        (
            super::cloudflare::IGNORE_FILE,
            super::cloudflare::ignore_text(),
        ),
    ] {
        assert_eq!(
            std::fs::read_to_string(built.web_dir.join(name))
                .unwrap_or_else(|e| panic!("web bundle needs {name}: {e}")),
            text
        );
    }
    let root = repo_root();
    for name in layout::INSTALLERS {
        assert_eq!(
            std::fs::read(built.web_dir.join(name))
                .unwrap_or_else(|e| panic!("web bundle needs {name}: {e}")),
            std::fs::read(root.join("scripts").join(name)).unwrap(),
            "the served {name} is the one in scripts/"
        );
    }
    let assets = super::cloudflare::check(&built.web_dir).expect("deployable to Workers");
    assert!(assets.largest_bytes <= super::cloudflare::MAX_ASSET_BYTES);
    println!("the packaged web bundle: {}", assets.summary());
}

#[test]
fn the_packaged_binary_embeds_the_payload_its_receipt_records() {
    let built = packaged();
    let expected = format!("payload: embedded, sha256 {}", built.receipt.payload.sha256);
    let version = |binary: &Path| {
        let output = std::process::Command::new(binary)
            .arg("--version")
            .output()
            .expect("run the packaged binary");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout)
            .expect("UTF-8")
            .lines()
            .find(|line| line.starts_with("payload: "))
            .expect("a payload line")
            .to_string()
    };
    assert_eq!(version(&built.package_dir.join(binary_name())), expected);

    let alone = built
        .package_dir
        .parent()
        .expect("the package has an output directory")
        .join("moved-binary");
    let _ = std::fs::remove_dir_all(&alone);
    std::fs::create_dir_all(&alone).unwrap();
    std::fs::copy(
        built.package_dir.join(binary_name()),
        alone.join(binary_name()),
    )
    .unwrap();
    assert_eq!(version(&alone.join(binary_name())), expected);
    let _ = std::fs::remove_dir_all(&alone);
}

#[test]
fn the_receipt_is_host_free_and_its_payload_digest_recomputes() {
    let built = packaged();
    let recomputed = layout::digest(&built.package_dir, binary_name()).unwrap();
    // `receipt.json` is not payload, so the finished directory's digest equals the receipt's.
    assert_eq!(recomputed.sha256, built.receipt.payload.sha256);
    assert_eq!(recomputed.files.len(), built.receipt.payload.files.len());

    let text = std::fs::read_to_string(built.package_dir.join("receipt.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["target"], host_target());
    assert_eq!(json["distribution"]["signed"], false);
    assert_eq!(json["payload"]["sha256"], built.receipt.payload.sha256);
    // A Windows receipt names what the executable imports and embeds; a macOS one has no such block.
    if host_target().contains("-windows-") {
        let windows = &json["windows"];
        assert_eq!(windows["crt_static"], true, "{windows}");
        assert_eq!(windows["manifest"]["embedded"], true, "{windows}");
        assert_eq!(
            windows["manifest"]["active_code_page"], "UTF-8",
            "{windows}"
        );
        let imports = windows["imports"].as_array().expect("imports");
        assert!(imports.iter().any(|dll| dll == "kernel32.dll"), "{windows}");
        for dll in imports
            .iter()
            .chain(windows["delay_imports"].as_array().expect("delay"))
        {
            let dll = dll.as_str().expect("a DLL name");
            assert!(
                !super::windows::is_redistributable(dll),
                "the receipt of a package that passed the audit lists no redistributable: {dll}"
            );
        }
    } else {
        assert!(json.get("windows").is_none(), "{text}");
    }
    match &built.receipt.payload_origin {
        Some(origin) => {
            assert_eq!(origin.payload_sha256, built.receipt.payload.sha256);
            assert_eq!(
                json["payload"]["from_package"]["payload_sha256"],
                built.receipt.payload.sha256
            );
            println!(
                "{} payload {} is the payload of the {} package of commit {}",
                host_target(),
                built.receipt.payload.sha256,
                origin.target,
                origin.commit
            );
        }
        None => assert!(json["payload"]["from_package"].is_null(), "{text}"),
    }
    assert!(
        json["tree"].is_string(),
        "the receipt names its tree: {text}"
    );
    assert_eq!(
        json["payload"]["embedded"], true,
        "the packaged binary carries its payload"
    );

    // Both artifacts are measured, not only the wasm core.
    let roms = json["roms"]["embedded"].as_array().expect("rom rows");
    assert_eq!(roms.len(), 4, "two artifacts times two bundled ROM ELFs");
    for artifact in ["passportsim", "pemu_wasm.wasm"] {
        assert_eq!(
            roms.iter()
                .filter(|row| row["artifact"] == artifact)
                .count(),
            2,
            "{artifact} is measured against both ROMs"
        );
    }

    // Every prefix is remapped to its token and both artifacts were searched.
    let account = &json["account_paths"];
    let remapped: Vec<&str> = account["remapped"]
        .as_array()
        .expect("remapped prefixes")
        .iter()
        .filter_map(|row| row["prefix"].as_str())
        .collect();
    for prefix in ["cargo_home", "toolchain_source", "workspace"] {
        assert!(
            remapped.contains(&prefix),
            "{prefix} is remapped: {account}"
        );
    }
    let searched: Vec<&str> = account["searched"]
        .as_array()
        .expect("searched artifacts")
        .iter()
        .filter_map(|row| row.as_str())
        .collect();
    assert_eq!(
        &searched[..2],
        [binary_name(), "payload/web/pemu_wasm.wasm"]
    );
    let demo_bundle = built
        .package_dir
        .join("payload/firmware")
        .join(demo::BUNDLE_FILE);
    let web_name = built
        .web_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut shipped = vec![
        built.package_dir.join(binary_name()),
        built.package_dir.join("payload/web/pemu_wasm.wasm"),
    ];
    if demo_bundle.is_file() {
        assert!(
            searched.contains(&"payload/firmware/official.pebundle"),
            "{account}"
        );
        shipped.push(demo_bundle);
    }
    for name in ["index.html", "main.js", "worker.js", "pemu_wasm.wasm"] {
        assert!(
            searched.contains(&format!("{web_name}/{name}").as_str()),
            "the web bundle's {name} is searched: {account}"
        );
        shipped.push(built.web_dir.join(name));
    }
    // Read again from the files that ship: none names this account or any profile path.
    let needles = super::account::Account::of_process()
        .expect("the packaging account")
        .needles();
    for path in shipped {
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            super::account::hits(&bytes, &needles, &[]).is_empty()
                && super::account::profile_path_spans(&bytes).is_empty(),
            "{} names an account",
            path.display()
        );
    }

    assert_eq!(json["secrets"]["pattern_rules"], true);
    assert_eq!(
        json["secrets"]["hashed_rules"], built.receipt.secrets.hashed_rules,
        "a package never claims a rule set that did not run on its host"
    );
    assert!(json["secrets"]["files_scanned"].as_u64().unwrap() > 20);

    // JSON escapes a backslash, so a Windows home is looked for in the unescaped text too.
    let home =
        std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
    assert!(!home.is_empty());
    let unescaped = text.replace("\\\\", "\\");
    for host_path in [
        home.as_str(),
        &repo_root().display().to_string(),
        "/Users/",
        "\\Users\\",
    ] {
        assert!(
            !text.contains(host_path) && !unescaped.contains(host_path),
            "the receipt names no host path: `{host_path}`"
        );
    }
    // A receipt carries digests, never a MAC (docs/secrets.md).
    assert!(!text.contains("efuse_blk"));
}

/// Synthetic [`layout::Inputs`] sources under `dir`, so a test can write a package tree without
/// building the emulator or having the firmware corpus.
fn synthetic_inputs(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    for name in [
        "index.html",
        "styles.css",
        "main.js",
        "worker.js",
        "worklet.js",
        "favicon.svg",
        "favicon-32.png",
        "apple-touch-icon.png",
    ] {
        std::fs::write(dir.join(name), name).unwrap();
    }
    std::fs::write(dir.join("pemu_wasm.wasm"), b"\0asm").unwrap();
    std::fs::write(dir.join("passportsim"), b"binary").unwrap();
    dir.to_path_buf()
}

fn synthetic_demo() -> Found {
    Found::Embedded(Box::new(demo::Demo {
        bundle: b"PEBUNDL1 synthetic".to_vec(),
        license: MIT_TEXT.to_string(),
        bin_sha256: "0".repeat(64),
        elf_sha256: "1".repeat(64),
        elf_shipped_sha256: "1".repeat(64),
        elf_paths_blanked: 0,
        commit: demo::CommitCheck::NotRechecked("a test package"),
        source: demo::Source::Corpus,
    }))
}

/// The three demo files land in both trees on a host with no firmware corpus.
#[test]
fn an_embedded_demo_is_written_into_both_trees_with_its_licence_and_notice() {
    let root = root_with_generated_docs();
    let out = tempdir("demo-layout");
    let src = synthetic_inputs(&out.join("src"));
    let demo_found = synthetic_demo();
    let env = guard::env().unwrap();
    let inputs = layout::Inputs {
        binary: &src.join("passportsim"),
        wasm: &src.join("pemu_wasm.wasm"),
        web: &src,
        demo: &demo_found,
        target: super::MACOS_TARGET,
        secrets_env: &env,
        version: env!("CARGO_PKG_VERSION"),
        checkout: &super::receipt::Checkout::read(&root),
    };
    let built = layout::write(&root, &out, &inputs).unwrap();

    let package = [
        "payload/firmware/official.pebundle",
        "payload/firmware/official-demo.LICENSE",
        "payload/firmware/official-demo.NOTICE",
    ];
    let web = [
        "official.pebundle",
        "licenses/official-demo.LICENSE",
        "licenses/official-demo.NOTICE",
    ];
    for (in_package, in_web) in package.iter().zip(web) {
        let a = built.package_dir.join(in_package);
        let b = built.web_dir.join(in_web);
        assert!(a.is_file(), "missing from the package: {in_package}");
        assert!(b.is_file(), "missing from the web bundle: {in_web}");
        // The web bundle's copy of the bundle is gzip-compressed (`layout::write_web_bundle`).
        let web_bytes = std::fs::read(&b).unwrap();
        let web_bytes = if in_web == demo::BUNDLE_FILE {
            gunzip(&web_bytes)
        } else {
            web_bytes
        };
        assert_eq!(
            std::fs::read(&a).unwrap(),
            web_bytes,
            "{in_package} and {in_web} are the same bytes"
        );
    }
    assert_eq!(
        std::fs::read_to_string(
            built
                .package_dir
                .join("payload/firmware/official-demo.LICENSE")
        )
        .unwrap(),
        MIT_TEXT,
        "the upstream licence is copied verbatim"
    );
    let notice = std::fs::read_to_string(
        built
            .package_dir
            .join("payload/firmware/official-demo.NOTICE"),
    )
    .unwrap();
    assert!(notice.contains(demo::UPSTREAM_COMMIT) && notice.contains("a test package"));
}

/// A secret-rule hit in the package stops it before anything is archived.
#[test]
fn a_secret_in_a_payload_file_fails_the_packaging_and_writes_no_archive() {
    let root = root_with_generated_docs();
    let out = tempdir("guard");
    let src = synthetic_inputs(&out.join("src"));
    // A MAC outside the `02:00:00` placeholder prefix.
    std::fs::write(
        src.join("main.js"),
        format!("const mac = \"{}\";\n", planted_mac()),
    )
    .unwrap();
    let absent = Found::Absent(demo::Absent {
        reason: "a test package".to_string(),
    });
    let env = guard::env().unwrap();
    let inputs = layout::Inputs {
        binary: &src.join("passportsim"),
        wasm: &src.join("pemu_wasm.wasm"),
        web: &src,
        demo: &absent,
        target: super::MACOS_TARGET,
        secrets_env: &env,
        version: env!("CARGO_PKG_VERSION"),
        checkout: &super::receipt::Checkout::read(&root),
    };
    let error = layout::write_all(&root, &out, &inputs, true).unwrap_err();
    assert!(error.contains("mac-shape"), "{error}");
    assert!(error.contains("secret guard"), "{error}");
    let archives: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tar.gz"))
        .collect();
    assert!(archives.is_empty(), "no archive is written: {archives:?}");

    // The same tree without the planted string packages, so the rule is what refused it.
    std::fs::write(src.join("main.js"), "const mac = \"02:00:00:11:22:33\";\n").unwrap();
    let built = layout::write_all(&root, &out, &inputs, false).unwrap();
    assert!(built.receipt.secrets.pattern_rules);
    assert!(built.receipt.secrets.files_scanned > 0);
}

#[test]
fn the_archives_are_written_beside_the_directories() {
    // Its own small packages, so the shared build stays archive-free.
    let root = root_with_generated_docs();
    for (target, package_suffix, web_suffix) in [
        (super::MACOS_TARGET, "-macos-arm64.tar.gz", "-web.tar.gz"),
        (super::WINDOWS_TARGETS[0], "-windows-x64.zip", "-web.zip"),
    ] {
        let out = tempdir(&format!("archive-{target}"));
        let inputs_dir = synthetic_inputs(&out.join("src"));
        let absent = Found::Absent(demo::Absent {
            reason: "a test package".to_string(),
        });
        let env = guard::env().unwrap();
        let inputs = layout::Inputs {
            binary: &inputs_dir.join("passportsim"),
            wasm: &inputs_dir.join("pemu_wasm.wasm"),
            web: &inputs_dir,
            demo: &absent,
            target,
            secrets_env: &env,
            version: env!("CARGO_PKG_VERSION"),
            checkout: &super::receipt::Checkout::read(&root),
        };
        let built = layout::write_all(&root, &out, &inputs, true).unwrap();
        let package = built.package_archive.expect("package archive");
        let web = built.web_archive.expect("web archive");
        assert!(package.is_file() && web.is_file());
        assert!(
            package.to_string_lossy().ends_with(package_suffix),
            "{}",
            package.display()
        );
        assert!(
            web.to_string_lossy().ends_with(web_suffix),
            "{}",
            web.display()
        );
        assert!(std::fs::metadata(&package).unwrap().len() > 0);
        let reason = match &built.receipt.demo {
            super::receipt::DemoRecord::Absent { reason } => reason.clone(),
            super::receipt::DemoRecord::Embedded { .. } => panic!("no demo was given"),
        };
        assert_eq!(reason, "a test package");
    }
}

#[derive(Debug)]
struct Example {
    language: String,
    marker: String,
    body: String,
    line: usize,
}

/// Every fenced `sh` or `powershell` block of a document, with the marker before it:
/// `<!-- quickstart: run exit=<n> -->` or `<!-- quickstart: not-run - <reason> -->`. A block with
/// no unconsumed marker is an error, which is how the marker rule is enforced.
fn examples(text: &str) -> Result<Vec<Example>, String> {
    let mut out = Vec::new();
    let mut pending: Option<(String, usize)> = None;
    let mut open: Option<(String, String, usize)> = None;
    let mut body = String::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if let Some(rest) = line.trim().strip_prefix("<!-- quickstart:") {
            let marker = rest.trim_end_matches("-->").trim().to_string();
            pending = Some((marker, number));
            continue;
        }
        if let Some((language, marker, start)) = open.clone() {
            if line.starts_with("```") {
                out.push(Example {
                    language,
                    marker,
                    body: std::mem::take(&mut body),
                    line: start,
                });
                open = None;
            } else {
                body.push_str(line);
                body.push('\n');
            }
            continue;
        }
        let Some(language) = line.strip_prefix("```") else {
            continue;
        };
        let language = language.trim();
        if language != "sh" && language != "powershell" {
            // A `text` block is output, not a command, and needs no marker.
            if !language.is_empty() {
                continue;
            }
            continue;
        }
        let Some((marker, _)) = pending.take() else {
            return Err(format!(
                "line {number}: a `{language}` block with no `<!-- quickstart: ... -->` marker. \
                 Every documentation example that is a command is either executed by the \
                 clean-environment test or marked `not-run - <reason>`."
            ));
        };
        open = Some((language.to_string(), marker, number));
    }
    if open.is_some() {
        return Err("an unterminated fenced block".to_string());
    }
    Ok(out)
}

fn quickstart() -> Vec<Example> {
    let text = std::fs::read_to_string(repo_root().join("docs/quickstart.md")).unwrap();
    examples(&text).expect("docs/quickstart.md examples")
}

#[test]
fn the_example_scanner_finds_unmarked_blocks() {
    let good = "<!-- quickstart: run exit=0 -->\n\n```sh\ntrue\n```\n";
    let found = examples(good).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].marker, "run exit=0");
    assert_eq!(found[0].body, "true\n");
    assert_eq!(found[0].language, "sh");

    let bad = "```sh\ntrue\n```\n";
    assert!(examples(bad).unwrap_err().contains("no `<!-- quickstart"));

    let two = "<!-- quickstart: run exit=0 -->\n```sh\ntrue\n```\n```sh\nfalse\n```\n";
    assert!(examples(two).is_err());

    assert!(examples("```text\nhello\n```\n").unwrap().is_empty());
}

/// The quickstart language this host's clean-environment test runs.
fn host_shell_language() -> &'static str {
    if cfg!(windows) { "powershell" } else { "sh" }
}

#[test]
fn every_quickstart_example_is_marked() {
    let found = quickstart();
    assert!(found.len() >= 6, "the quickstart shows real commands");
    let mut runnable = 0usize;
    let mut runnable_powershell = 0usize;
    for example in &found {
        if let Some(rest) = example.marker.strip_prefix("run exit=") {
            rest.parse::<i32>().unwrap_or_else(|_| {
                panic!(
                    "line {}: `{}` has no exit code",
                    example.line, example.marker
                )
            });
            match example.language.as_str() {
                "sh" => runnable += 1,
                "powershell" => runnable_powershell += 1,
                other => panic!("line {}: a runnable `{other}` block", example.line),
            }
        } else {
            let reason = example
                .marker
                .strip_prefix("not-run - ")
                .unwrap_or_else(|| {
                    panic!("line {}: unknown marker `{}`", example.line, example.marker)
                });
            assert!(
                reason.len() > 20,
                "line {}: `not-run` needs a real reason, not `{reason}`",
                example.line
            );
        }
    }
    assert!(
        runnable >= 4,
        "most of the quickstart is executed, not asserted"
    );
    assert!(
        runnable_powershell >= 3,
        "section 8's Windows first run is executed on the Windows host, not asserted"
    );
}

/// Every runnable quickstart example for this host's shell, run in the clean environment
/// ([`Clean`]), exits as its marker says and leaves the scrubbed home empty.
///
/// Windows blocks run in Windows PowerShell 5.1 with `-NoProfile` and
/// `$ErrorActionPreference = 'Stop'`. There the temporary directory is a sibling of the home,
/// because PowerShell writes its own script-policy probes into it.
#[test]
fn the_quickstart_examples_run_on_a_clean_environment() {
    let built = packaged();
    let clean = Clean::new(built, tempdir("package-quickstart-home"));
    let language = host_shell_language();
    let mut ran = 0usize;
    for example in quickstart() {
        let Some(expected) = example.marker.strip_prefix("run exit=") else {
            continue;
        };
        if example.language != language {
            continue;
        }
        let expected: i32 = expected.parse().unwrap();
        let output = clean
            .shell(&example.body)
            .output()
            .unwrap_or_else(|e| panic!("line {}: cannot run: {e}", example.line));
        assert_eq!(
            output.status.code(),
            Some(expected),
            "docs/quickstart.md line {}: `{}`\nstdout: {}\nstderr: {}",
            example.line,
            example.body.trim(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        ran += 1;
    }
    let floor = if cfg!(windows) { 3 } else { 4 };
    assert!(
        ran >= floor,
        "the quickstart's executable examples all ran: {ran}"
    );

    // A fresh machine with no ESP-IDF and no passportsim config runs the emulator, and nothing may be
    // created in the empty home along the way.
    let left: Vec<_> = std::fs::read_dir(&clean.home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        left.is_empty(),
        "the scrubbed home must stay empty, found {left:?}"
    );
}

const READY_LINE: &str = "main: 就绪:Display=1 Button=1 Audio=1 Battery=1";

/// The merged image an `idf.py` build directory of `official` holds.
const MERGED_IMAGE: &str = "FoloToy-AI-Passport-8MB.bin";

const WEB_FILES: [&str; 9] = [
    "index.html",
    "styles.css",
    "main.js",
    "worker.js",
    "worklet.js",
    "favicon.svg",
    "favicon-32.png",
    "apple-touch-icon.png",
    "pemu_wasm.wasm",
];

/// The demo bundle comes from `payload/firmware` through the corpus-or-payload seam `serve`
/// installs, which lets a host with no corpus boot the demo in a browser.
const DEMO_BUNDLE: &str = "official.pebundle";

/// First run on a clean environment, on macOS and natively on Windows.
///
/// Every command runs after [`std::process::Command::env_clear`] with only this host's recipe
/// ([`Clean`]), proved from inside the child. A step this tree cannot do records a `NOT_RUN` leg,
/// never a relaxed environment. Part 2 needs the `idf.py` build directory of a corpus host.
///
/// - (1) `start` with no image boots the bundled demo inside 2 s of virtual time, and `doctor`
///   reports the bundled ROMs and the demo.
/// - (2) A copy of `$ROOT/builds/official/build` boots the same image.
/// - (3) `mcp` answers `initialize`, `tools/list` and a `passport_run` call over stdio.
/// - (4) The daemon serves the packaged web bundle after the launch-code exchange; the browser
///   half is `web/tests/firstRun.spec.ts` in the same T1 run.
#[test]
fn t1_m9a_first_run_on_a_clean_environment() {
    const TEST: &str = "t1_m9a_first_run_on_a_clean_environment";
    let built = packaged();
    let clean = Clean::new(built, tempdir("package-home"));
    let _stop = DaemonStop(&clean);

    clean_environment(&clean, built);
    let demo_booted = part_one(&clean, TEST, built);
    let running = match part_two(&clean, TEST) {
        Some(vt) => Some(vt),
        None if demo_booted => Some(start_the_demo_for_part_three(&clean)),
        None => None,
    };
    part_three(&clean, TEST, running);
    part_four(&clean, TEST, built);

    // The first run must leave nothing unexpected in the empty home: a tools directory would mean the
    // packaged binary went looking for a toolchain.
    for absent in [
        ".espressif",
        ".config",
        "esp-idf",
        ".cargo",
        ".rustup",
        "Espressif",
    ] {
        assert!(
            !clean.home.join(absent).exists(),
            "a first run created `{absent}` in the home, which it never needs"
        );
    }
    assert!(
        clean.home.join(RUNTIME_DIR).is_dir(),
        "the daemon publishes its discovery file under the runtime role `{RUNTIME_DIR}`"
    );
    let mut left: Vec<String> = std::fs::read_dir(&clean.home)
        .expect("the scrubbed HOME is readable")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into()
        })
        .collect();
    left.sort();
    println!("{} first-run footprint in an empty home: {left:?}", row());
}

fn row() -> &'static str {
    if cfg!(windows) {
        "first run on Windows"
    } else {
        "first run on macOS"
    }
}

/// The runtime role inside the clean home: `~/.passportsim` under `HOME` on macOS, `run` under
/// `PASSPORTSIM_HOME` on Windows.
const RUNTIME_DIR: &str = if cfg!(windows) { "run" } else { ".passportsim" };

/// Asserts the child's environment is the recipe's, read from inside the child (`/usr/bin/env`,
/// `cmd.exe /c set`), and on Windows that the executable needs no Visual C++ redistributable. It
/// is not vacuous under `cargo xtask ci t1`, which exports `PASSPORTSIM_DATA_ROOT` to this process.
fn clean_environment(clean: &Clean, built: &super::Built) {
    let env = if cfg!(windows) {
        let cmd = Path::new(&clean.system_root)
            .join("System32")
            .join("cmd.exe");
        clean.tool(&cmd, &["/d", "/c", "set"])
    } else {
        clean.tool(Path::new("/usr/bin/env"), &[])
    };
    assert_eq!(env.code, 0, "the environment is readable: {}", env.text());
    let mut seen: Vec<String> = env
        .out
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty())
        // `cmd.exe` defines `COMSPEC`, `PATHEXT` and `PROMPT` for itself when they are missing, so `set`
        // prints them although the packaged binary never sees them.
        .filter(|line| {
            !(cfg!(windows)
                && ["COMSPEC=", "PATHEXT=", "PROMPT="]
                    .iter()
                    .any(|own| line.to_ascii_uppercase().starts_with(own)))
        })
        .map(str::to_owned)
        .collect();
    seen.sort();
    let mut want: Vec<String> = clean
        .vars()
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    want.sort();
    assert_eq!(
        seen,
        want,
        "{}'s environment is these variables and nothing else",
        row()
    );
    assert!(
        !env.out.contains("PASSPORTSIM_DATA_ROOT") && !env.out.contains("IDF_"),
        "this process may have a data root or an IDF, and the child must not"
    );
    for absent in [".espressif", ".config", "esp-idf"] {
        assert!(
            !clean.home.join(absent).exists(),
            "the clean home starts without `{absent}`"
        );
    }
    if cfg!(windows) {
        // The executable imports no Visual C++ runtime, and no DLL of the package's own stands in for one.
        let bytes = std::fs::read(&clean.bin).expect("the packaged executable");
        let image = super::pe::parse(&bytes).expect("the packaged executable is a PE image");
        for dll in image.imports.iter().chain(&image.delay_imports) {
            assert!(
                !super::windows::is_redistributable(dll),
                "the packaged executable imports `{dll}`, a Visual C++ redistributable DLL"
            );
        }
        assert_eq!(
            image.manifest.as_deref(),
            Some(super::windows::MANIFEST.as_bytes())
        );
        let dlls: Vec<_> = std::fs::read_dir(&built.package_dir)
            .expect("the package directory")
            .filter_map(|entry| entry.ok().map(|e| e.file_name()))
            .filter(|name| {
                name.to_string_lossy()
                    .to_ascii_lowercase()
                    .ends_with(".dll")
            })
            .collect();
        assert!(dlls.is_empty(), "the package ships no DLL: {dlls:?}");
        println!(
            "{}: the packaged executable imports {} DLL(s), none of them vcruntime*.dll or \
             msvcp*.dll: {:?}",
            row(),
            image.imports.len() + image.delay_imports.len(),
            image.imports
        );
    }
}

/// Part 1: the bundled demo with no image, `run`, `start official` by corpus id, and `doctor`.
/// Returns whether the demo booted.
fn part_one(clean: &Clean, test: &str, built: &super::Built) -> bool {
    // `doctor` first: it starts nothing, so the discovery report is built in a still-empty home.
    let doctor = clean.run(&["--output", "json", "doctor"]);
    assert_eq!(doctor.code, 0, "`doctor` answers: {}", doctor.text());
    let report: serde_json::Value =
        serde_json::from_str(&doctor.out).expect("`--output json` answers one JSON object");
    assert_eq!(report["ok"], true, "{}", doctor.text());
    assert_eq!(
        report["rom_override"],
        serde_json::Value::Null,
        "a clean host runs the bundled ROM with no override: {}",
        doctor.text()
    );
    let roms = report["bundled_roms"]
        .as_array()
        .expect("`doctor` reports the bundled ROMs");
    assert_eq!(roms.len(), 2, "both pinned ROMs: {}", doctor.text());
    for rom in roms {
        assert_eq!(rom["status"], "found", "{}", doctor.text());
        assert_eq!(
            rom["embedded_sha256"],
            rom["pinned_sha256"],
            "the embedded ROM is the one assets/rom/pins.toml pins: {}",
            doctor.text()
        );
    }
    for entry in report["corpus"]
        .as_array()
        .expect("`doctor` reports every corpus id")
    {
        assert_eq!(
            entry["status"],
            "missing",
            "a clean host has no corpus at all, and that is not a failure: {}",
            doctor.text()
        );
    }

    let demo_sha = match &built.receipt.demo {
        super::receipt::DemoRecord::Embedded { bin_sha256, .. } => bin_sha256.clone(),
        super::receipt::DemoRecord::Absent { reason } => {
            assert_eq!(
                report["demo"],
                serde_json::Value::Null,
                "a package with no demo reports none: {}",
                doctor.text()
            );
            println!(
                "NOT_RUN {test} part1-bundled-demo: this package carries no demo, so there was \
                 nothing for `start` with no `fw` to boot and nothing for `doctor` to report: \
                 {reason}"
            );
            return false;
        }
    };
    assert_eq!(report["demo"]["id"], demo::DEMO_ID, "{}", doctor.text());
    assert_eq!(
        report["demo"]["sha256"],
        demo_sha,
        "`doctor` reports the digest of the image the package's own receipt records: {}",
        doctor.text()
    );
    assert_eq!(
        report["demo"]["bytes"],
        8 * 1024 * 1024,
        "the demo is a merged 8 MB flash image: {}",
        doctor.text()
    );

    // With no image `start` boots the bundled demo; the ready line must arrive inside 2 s virtual.
    let started = clean.run(&["--output", "json", "start", "--boot-timeout", "2s"]);
    assert_eq!(
        started.code,
        0,
        "`start` with no `fw` boots the bundled demo: {}",
        started.text()
    );
    let answer: serde_json::Value =
        serde_json::from_str(&started.out).expect("`--output json` answers one JSON object");
    let instance = answer["instance"]
        .as_str()
        .expect("the new instance")
        .to_owned();
    assert_eq!(answer["image"]["fw"], demo::DEMO_ID, "{}", started.text());
    assert!(
        answer["vt_us"].as_u64().expect("virtual time") <= 2_000_000,
        "the demo booted inside the 2 s of virtual time the row allows: {}",
        started.text()
    );
    // The settle point is read through the app ELF, which on a host with no corpus can only come
    // from the bundle: a `timeout` here means the payload's `app_elf` was not found.
    assert_eq!(
        answer["boot"]["status"],
        "matched",
        "the bundled demo reaches `until_ui_settled` with no corpus and no ESP-IDF: {}",
        started.text()
    );
    let receipt = &answer["receipt"];
    assert_eq!(receipt["tainted"], false, "{}", started.text());
    assert_eq!(receipt["efuse"], "synthetic", "{}", started.text());

    let serial = clean.run(&["serial", "read", "--cursor", "0", "--instance", &instance]);
    assert_eq!(serial.code, 0, "the console is readable: {}", serial.text());
    assert!(
        serial.out.contains("ESP-ROM:esp32c3-eco7-20230720"),
        "the bundled ROM ran, with no `~/.espressif` on this host: {}",
        serial.text()
    );
    assert!(
        serial.out.contains(READY_LINE),
        "the ready line of the bundled demo: {}",
        serial.text()
    );

    let artifacts = instance_artifacts(&clean.home, &instance);
    std::fs::copy(
        repo_root().join("tests/golden/official/menu.png"),
        artifacts.join("menu.png"),
    )
    .expect("the golden is placed in the instance's artifact directory");
    let shot = clean.run(&[
        "screenshot",
        "raw",
        "--compare-with",
        "menu.png",
        "--instance",
        &instance,
    ]);
    assert_eq!(shot.code, 0, "the frame compares: {}", shot.text());
    assert!(
        shot.out.contains("compare pass diff_pixels=0"),
        "the bundled demo draws the settled menu of `tests/golden/official/menu.png`: {}",
        shot.text()
    );

    let run = clean.run(&[
        "--output",
        "json",
        "run",
        "--for",
        "2s",
        "--instance",
        &instance,
    ]);
    // `run`'s exit code carries the verdict, so a lenient pass over an unmodeled register is 10.
    assert!(
        matches!(run.code, 0 | 10),
        "`run --for 2s` advances it: {}",
        run.text()
    );
    let ran: serde_json::Value =
        serde_json::from_str(&run.out).expect("`--output json` answers one JSON object");
    assert!(
        ran["vt_us"].as_u64().expect("virtual time") > answer["vt_us"].as_u64().expect("start"),
        "virtual time moved: {}",
        run.text()
    );

    // The same demo by its corpus id; a clean host has no corpus, so only the payload can answer.
    let by_id = clean.run(&[
        "--output",
        "json",
        "start",
        demo::DEMO_ID,
        "--boot-timeout",
        "2s",
    ]);
    assert_eq!(
        by_id.code,
        0,
        "`start {}` boots the packaged bundle by its id: {}",
        demo::DEMO_ID,
        by_id.text()
    );
    let second: serde_json::Value =
        serde_json::from_str(&by_id.out).expect("`--output json` answers one JSON object");
    assert_eq!(second["image"]["fw"], demo::DEMO_ID, "{}", by_id.text());
    assert_ne!(
        second["instance"],
        answer["instance"],
        "a second instance, not the first one again: {}",
        by_id.text()
    );
    for id in [
        second["instance"].as_str().expect("the second instance"),
        &instance,
    ] {
        let stopped = clean.run(&["stop", id]);
        assert_eq!(stopped.code, 0, "`{id}` stops again: {}", stopped.text());
    }
    true
}

/// Part 2: a copied `idf.py` build directory in the clean environment. Returns the virtual time of
/// the instance it leaves booted, for part 3.
fn part_two(clean: &Clean, test: &str) -> Option<u64> {
    let Some(source) = idf_build_dir() else {
        println!(
            "NOT_RUN {test} part2-build-directory: no `builds/official/build` under \
             `PASSPORTSIM_DATA_ROOT`, so no `idf.py` build directory could be copied; the tier \
             injects the data root at t1 and t2 on macOS, a plain `cargo test` has none, and the \
             firmware builds are macOS-only"
        );
        return None;
    };
    // A copy, so nothing can resolve against the corpus. `cp -c` clones on APFS, so the 270 MB
    // directory costs no disk.
    let copy = tempdir("package-build").join("build");
    let cloned = cfg!(target_os = "macos")
        && std::process::Command::new("/bin/cp")
            .args(["-Rc".as_ref(), source.as_os_str(), copy.as_os_str()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    if !cloned {
        copy_tree(&source, &copy);
    }
    let image = copy.join(MERGED_IMAGE);
    assert!(
        image.is_file(),
        "the copy holds the merged image `idf.py` wrote: {}",
        image.display()
    );

    // The build directory itself as the argument, read through `flasher_args.json`: no ESP-IDF, no
    // esptool and no corpus.
    let dir = clean.run(&[
        "--output",
        "json",
        "start",
        copy.to_str().expect("a UTF-8 path"),
        "--boot-timeout",
        "2s",
    ]);
    assert_eq!(dir.code, 0, "a build directory boots: {}", dir.text());
    let from_dir: serde_json::Value =
        serde_json::from_str(&dir.out).expect("`--output json` answers one JSON object");
    let dir_instance = from_dir["instance"]
        .as_str()
        .expect("the instance")
        .to_owned();
    assert_eq!(from_dir["image"]["fw"], "build", "{}", dir.text());
    // No settle point exists without an app ELF, so the marker says the UI could not be seen.
    assert_eq!(
        from_dir["boot"]["status"],
        "unobservable",
        "the build directory's settle marker is reported as unobservable: {}",
        dir.text()
    );
    assert!(
        from_dir["vt_us"].as_u64().expect("virtual time") <= 2_000_000,
        "the build directory booted inside the 2 s of virtual time the row allows: {}",
        dir.text()
    );
    let dir_serial = clean.run(&[
        "serial",
        "read",
        "--cursor",
        "0",
        "--instance",
        &dir_instance,
    ]);
    assert_eq!(dir_serial.code, 0, "{}", dir_serial.text());
    assert!(
        dir_serial.out.contains(READY_LINE),
        "the image the build directory holds is the one that prints the ready line: {}",
        dir_serial.text()
    );
    let dir_artifacts = instance_artifacts(&clean.home, &dir_instance);
    std::fs::copy(
        repo_root().join("tests/golden/official/menu.png"),
        dir_artifacts.join("menu.png"),
    )
    .expect("the golden is placed in the instance's artifact directory");
    let dir_shot = clean.run(&[
        "screenshot",
        "raw",
        "--compare-with",
        "menu.png",
        "--instance",
        &dir_instance,
    ]);
    assert_eq!(dir_shot.code, 0, "{}", dir_shot.text());
    assert!(
        dir_shot.out.contains("compare pass diff_pixels=0"),
        "the build directory draws the settled menu of the goldens: {}",
        dir_shot.text()
    );
    let stopped = clean.run(&["stop", &dir_instance]);
    assert_eq!(stopped.code, 0, "{}", stopped.text());

    // `--boot-timeout 2s` proves the ready line arrives inside 2 s of virtual time.
    let started = clean.run(&[
        "--output",
        "json",
        "start",
        image.to_str().expect("a UTF-8 path"),
        "--boot-timeout",
        "2s",
    ]);
    assert_eq!(started.code, 0, "the image boots: {}", started.text());
    let answer: serde_json::Value =
        serde_json::from_str(&started.out).expect("`--output json` answers one JSON object");
    let instance = answer["instance"]
        .as_str()
        .expect("the instance")
        .to_owned();
    assert_eq!(answer["image"]["fw"], MERGED_IMAGE, "{}", started.text());
    assert_eq!(
        answer["boot"]["status"],
        "unobservable",
        "a merged image named by path has no settle point here, and says so: {}",
        started.text()
    );
    assert_eq!(
        answer["vt_us"],
        2_000_000,
        "the boot took the 2 s of virtual time the row allows: {}",
        started.text()
    );

    // The receipt lists only bundled assets or the dropped image: nothing of this host.
    let receipt = &answer["receipt"];
    assert_eq!(receipt["tainted"], false, "{}", started.text());
    assert_eq!(receipt["redacted"], false, "{}", started.text());
    assert_eq!(receipt["efuse"], "synthetic", "{}", started.text());
    assert!(
        receipt["binding"]["app_elf_sha256"].is_null(),
        "no host ELF was bound: {}",
        started.text()
    );

    let serial = clean.run(&["serial", "read", "--cursor", "0", "--instance", &instance]);
    assert_eq!(serial.code, 0, "the console is readable: {}", serial.text());
    assert!(
        serial.out.contains("ESP-ROM:esp32c3-eco7-20230720"),
        "the bundled ROM ran, with no `~/.espressif` on this host: {}",
        serial.text()
    );
    assert!(
        serial.out.contains(READY_LINE),
        "the ready line arrived inside 2 s of virtual time: {}",
        serial.text()
    );

    // `--compare-with` reads its expected image from the instance's artifact directory.
    let artifacts = instance_artifacts(&clean.home, &instance);
    std::fs::copy(
        repo_root().join("tests/golden/official/menu.png"),
        artifacts.join("menu.png"),
    )
    .expect("the golden is placed in the instance's artifact directory");
    let shot = clean.run(&[
        "screenshot",
        "raw",
        "--compare-with",
        "menu.png",
        "--instance",
        &instance,
    ]);
    assert_eq!(shot.code, 0, "the frame compares: {}", shot.text());
    assert!(
        shot.out.contains("compare pass diff_pixels=0"),
        "the `raw` frame of the clean-environment boot is the golden menu: {}",
        shot.text()
    );
    answer["vt_us"].as_u64()
}

/// For part 3 when part 2 had no build directory: the bundled demo again. Returns its virtual time.
fn start_the_demo_for_part_three(clean: &Clean) -> u64 {
    let started = clean.run(&["--output", "json", "start", "--boot-timeout", "2s"]);
    assert_eq!(
        started.code,
        0,
        "the bundled demo boots: {}",
        started.text()
    );
    let answer: serde_json::Value =
        serde_json::from_str(&started.out).expect("`--output json` answers one JSON object");
    answer["vt_us"].as_u64().expect("virtual time")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("the copy's directory");
    for entry in std::fs::read_dir(from).expect("the build directory") {
        let path = entry.expect("an entry").path();
        let target = to.join(path.file_name().expect("a name"));
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            std::fs::copy(&path, &target).expect("a file of the build directory");
        }
    }
}

/// Part 3: `mcp` over stdio, with a `run` tool call against the instance booted at `running`.
fn part_three(clean: &Clean, test: &str, running: Option<u64>) {
    let booted = running.is_some();
    let mut asked = vec![
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "first-run", "version": "0"},
            },
        }),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    ];
    if booted {
        asked.push(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "passport_run", "arguments": {"for": "500ms"}},
        }));
    }
    let answers = clean.mcp(&asked);
    let by_id = |id: u64| {
        answers
            .iter()
            .find(|value| value["id"] == id)
            .unwrap_or_else(|| panic!("no answer to id {id} among {answers:?}"))
    };

    let hello = by_id(1);
    assert_eq!(hello["result"]["protocolVersion"], "2025-06-18", "{hello}");
    assert_eq!(
        hello["result"]["serverInfo"]["name"], "passportsim",
        "{hello}"
    );
    assert!(
        hello["result"]["capabilities"]["tools"].is_object(),
        "the server offers tools: {hello}"
    );

    let listed = by_id(2);
    let tools: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools/list answers an array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for name in ["passport_run", "passport_start", "passport_doctor"] {
        assert!(
            tools.contains(&name),
            "`{name}` is offered, among {tools:?}"
        );
    }

    let Some(vt) = running else {
        println!(
            "NOT_RUN {test} part3-run-tool-call: `initialize` and `tools/list` were answered, but \
             no instance exists to run: part 2 found no build directory to boot, and this package \
             has no demo to start instead"
        );
        return;
    };
    let ran = by_id(3);
    let text = ran["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("the tool answers text: {ran}"));
    assert!(
        ran["result"]["isError"] != serde_json::Value::Bool(true),
        "the run tool call succeeded: {ran}"
    );
    let expected = format!("elapsed vt={}us (+500000us)", vt + 500_000);
    assert!(
        text.contains(&expected),
        "the tool call advanced the machine by the 500 ms it asked for: {text}"
    );
}

/// Part 4: the web bundle, served by the packaged binary to a credential-carrying client.
fn part_four(clean: &Clean, test: &str, built: &super::Built) {
    // The auto-started daemon is headless and mints no launch code; the browser's daemon is the
    // foreground one, so this test starts that.
    let _ = clean.run(&["serve", "--stop"]);
    let out_path = clean.home.join("serve.out");
    let log = std::fs::File::create(&out_path).expect("the daemon's output file");
    let errs = log.try_clone().expect("one file for both streams");
    let mut child = clean
        .command(&clean.bin)
        .arg("serve")
        .stdout(log)
        .stderr(errs)
        .spawn()
        .expect("the packaged daemon starts");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut ui = None;
    while std::time::Instant::now() < deadline && ui.is_none() {
        let text = std::fs::read_to_string(&out_path).unwrap_or_default();
        ui = text
            .lines()
            .find_map(|line| line.strip_prefix("ui:"))
            .map(|rest| rest.trim().to_owned());
        if ui.is_some() {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let banner = std::fs::read_to_string(&out_path).unwrap_or_default();
            panic!("the packaged daemon exited {status} before printing its UI line:\n{banner}");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let banner = std::fs::read_to_string(&out_path).unwrap_or_default();
    let ui = ui.unwrap_or_else(|| panic!("the daemon printed no UI line:\n{banner}"));
    let (base, fragment) = ui
        .split_once('#')
        .unwrap_or_else(|| panic!("the UI line carries a launch code fragment: `{ui}`"));
    let code = fragment
        .strip_prefix("lc=")
        .unwrap_or_else(|| panic!("the fragment is a launch code: `{ui}`"));
    let addr = base
        .trim_end_matches('/')
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("the UI line is a loopback URL: `{ui}`"))
        .to_owned();
    assert!(
        banner.contains("payload: embedded"),
        "the packaged daemon serves its own embedded payload:\n{banner}"
    );

    let (status, _, body) = http(&addr, "GET", "/", None, None);
    assert_eq!(status, 200, "the shell needs no credential");
    let shell = String::from_utf8_lossy(&body);
    assert!(
        shell.contains("/v1/session") && shell.contains("#lc="),
        "the shell is the document that redeems the fragment:\n{shell}"
    );
    let (status, _, _) = http(&addr, "GET", "/index.html", None, None);
    assert_eq!(status, 401, "the UI itself is not served without one");

    let (status, headers, _) = http(
        &addr,
        "POST",
        "/v1/session",
        None,
        Some(format!("{{\"code\":\"{code}\"}}").as_bytes()),
    );
    assert_eq!(status, 200, "the launch code is redeemed");
    let cookie = header(&headers, "set-cookie").expect("the redemption sets a session cookie");
    assert!(
        cookie.starts_with("pemu_session=") && cookie.contains("HttpOnly"),
        "the session cookie is an HttpOnly `pemu_session`: `{cookie}`"
    );
    let cookie = cookie.split(';').next().expect("a cookie pair").to_owned();

    // Every packaged web file, byte for byte, with the two headers that allow SharedArrayBuffer.
    for name in WEB_FILES {
        let (status, headers, body) = http(&addr, "GET", &format!("/{name}"), Some(&cookie), None);
        assert_eq!(status, 200, "`{name}` is served to a session");
        assert_eq!(
            header(&headers, "cross-origin-opener-policy").as_deref(),
            Some("same-origin"),
            "`{name}` is cross-origin isolated"
        );
        assert_eq!(
            header(&headers, "cross-origin-embedder-policy").as_deref(),
            Some("require-corp"),
            "`{name}` is cross-origin isolated"
        );
        let packaged = std::fs::read(built.web_dir.join(name))
            .unwrap_or_else(|e| panic!("the package's own `{name}`: {e}"));
        assert_eq!(
            body.len(),
            packaged.len(),
            "`{name}` is the packaged file, not another copy"
        );
        assert!(body == packaged, "`{name}` is served byte for byte");
    }
    // The demo route: a corpus id this host resolves, else the payload's copy. Both outcomes are
    // asserted against the receipt.
    let (status, _, body) = http(
        &addr,
        "GET",
        &format!("/{DEMO_BUNDLE}"),
        Some(&cookie),
        None,
    );
    match std::fs::read(built.web_dir.join(DEMO_BUNDLE)) {
        Ok(packaged) => {
            assert!(
                matches!(
                    built.receipt.demo,
                    super::receipt::DemoRecord::Embedded { .. }
                ),
                "the package carries the demo, so its receipt says so: {}",
                built.receipt.demo.summary()
            );
            assert_eq!(status, 200, "the packaged demo is served to a session");
            assert_eq!(
                body.len(),
                packaged.len(),
                "`{DEMO_BUNDLE}` is the packaged demo"
            );
            assert!(body == packaged, "`{DEMO_BUNDLE}` is served byte for byte");
        }
        Err(_) => {
            assert!(
                matches!(
                    built.receipt.demo,
                    super::receipt::DemoRecord::Absent { .. }
                ),
                "a package with no demo file says so in its receipt: {}",
                built.receipt.demo.summary()
            );
            assert_eq!(
                status, 404,
                "a package with no demo answers the demo route with 404 and nothing else"
            );
            println!(
                "NOT_RUN {test} part4-demo-bundle: this package carries no demo, so the daemon \
                 answered `/{DEMO_BUNDLE}` with 404, which is what a build without one \
                 does. The demo is never committed and \
                 `xtask package` reads the corpus map through a host directory role, which a \
                 `cfg(test)` build resolves for nobody, so a package built \
                 inside a test binary is always demo-less: {}",
                built.receipt.demo.summary()
            );
        }
    }

    println!(
        "{}: the packaged daemon served {} files of the web bundle, cross-origin \
         isolated, after one launch code",
        row(),
        WEB_FILES.len()
    );
    println!(
        "{}: the browser half is web/tests/firstRun.spec.ts, which runs in the same tier",
        row()
    );

    let _ = clean.run(&["serve", "--stop"]);
    let _ = child.wait();
}

/// `$PASSPORTSIM_DATA_ROOT/builds/official/build`, when this host has one.
fn idf_build_dir() -> Option<PathBuf> {
    let root = std::env::var_os("PASSPORTSIM_DATA_ROOT")?;
    let dir = PathBuf::from(root)
        .join("builds")
        .join("official")
        .join("build");
    dir.is_dir().then_some(dir)
}

/// The artifact directory of instance `id`. The run id is minted by the daemon, so the single
/// `run-*` directory of the once-empty home is found rather than assumed.
fn instance_artifacts(home: &Path, id: &str) -> PathBuf {
    let data_root = if cfg!(windows) {
        home.join("data")
    } else {
        home.join("Library")
            .join("Application Support")
            .join("passportsim")
    };
    let root = data_root.join("artifacts");
    let mut runs: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("the artifact root {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("run-"))
        })
        .collect();
    runs.sort();
    assert_eq!(
        runs.len(),
        1,
        "one daemon ran in this clean HOME, found {runs:?}"
    );
    let dir = runs.remove(0).join(id);
    assert!(
        dir.is_dir(),
        "the instance wrote artifacts: {}",
        dir.display()
    );
    dir
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

/// One raw HTTP/1.1 request over loopback, with the headers a browser sends. The daemon's own
/// client always sends the bearer token, which would skip the launch-code exchange under test.
fn http(
    addr: &str,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&[u8]>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    use std::io::{BufRead, BufReader, Read, Write};

    let mut stream = std::net::TcpStream::connect(addr)
        .unwrap_or_else(|e| panic!("the packaged daemon accepts on {addr}: {e}"));
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(60)))
        .expect("a read timeout");
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(cookie) = cookie {
        head.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).expect("the request head");
    if let Some(body) = body {
        stream.write_all(body).expect("the request body");
    }
    stream.flush().expect("flushed");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("a status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("a status code in `{}`", status_line.trim()));
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("a header line") == 0 {
            break;
        }
        let line = line.trim_end().to_owned();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let length: Option<usize> = header(&headers, "content-length").and_then(|v| v.parse().ok());
    let mut answer = Vec::new();
    match length {
        Some(n) => {
            answer.resize(n, 0);
            reader.read_exact(&mut answer).expect("the announced body");
        }
        None => {
            reader.read_to_end(&mut answer).expect("the body");
        }
    }
    (status, headers, answer)
}

/// The clean environment of a first run: one home, one `PATH`, one package.
struct Clean {
    /// A directory created empty for this run: `HOME` on macOS, `PASSPORTSIM_HOME` on Windows.
    home: PathBuf,
    bin: PathBuf,
    /// `/usr/bin:/bin:<package>` on macOS, `%SystemRoot%\System32;<package>` on Windows.
    path: String,
    package: PathBuf,
    /// `TMPDIR` on macOS (the home itself), `TEMP` and `TMP` on Windows (a sibling of the home).
    temp: PathBuf,
    /// Windows: `%SystemRoot%` of this machine; empty on macOS.
    system_root: String,
}

impl Clean {
    fn new(built: &super::Built, home: PathBuf) -> Clean {
        assert_eq!(
            std::fs::read_dir(&home).expect("the new home").count(),
            0,
            "{}'s home is a new empty directory",
            row()
        );
        let package = built.package_dir.clone();
        let (path, temp, system_root) = if cfg!(windows) {
            let system_root = std::env::var("SystemRoot").expect("Windows sets SystemRoot");
            let temp = home.with_file_name(format!(
                "{}-temp",
                home.file_name().expect("a name").to_string_lossy()
            ));
            let _ = std::fs::remove_dir_all(&temp);
            std::fs::create_dir_all(&temp).expect("the temporary directory");
            let path = format!("{system_root}\\System32;{}", package.display());
            (path, temp, system_root)
        } else {
            let path = format!("/usr/bin:/bin:{}", package.display());
            (path, home.clone(), String::new())
        };
        Clean {
            bin: package.join(binary_name()),
            home,
            path,
            package,
            temp,
            system_root,
        }
    }

    fn vars(&self) -> Vec<(&'static str, String)> {
        let path = |p: &Path| p.display().to_string();
        if cfg!(windows) {
            vec![
                ("PASSPORTSIM_HOME", path(&self.home)),
                ("PATH", self.path.clone()),
                ("SystemRoot", self.system_root.clone()),
                ("TEMP", path(&self.temp)),
                ("TMP", path(&self.temp)),
            ]
        } else {
            vec![
                ("HOME", path(&self.home)),
                ("PATH", self.path.clone()),
                ("TMPDIR", path(&self.temp)),
            ]
        }
    }

    fn command(&self, program: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(program);
        command.current_dir(&self.package).env_clear();
        for (name, value) in self.vars() {
            command.env(name, value);
        }
        command
    }

    fn shell(&self, body: &str) -> std::process::Command {
        if cfg!(windows) {
            let powershell = Path::new(&self.system_root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe");
            let mut command = self.command(&powershell);
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &format!("$ErrorActionPreference = 'Stop'; {body}; exit $LASTEXITCODE"),
            ]);
            command
        } else {
            let mut command = self.command(Path::new("/bin/sh"));
            command.arg("-c").arg(body);
            command
        }
    }

    fn run(&self, args: &[&str]) -> Ran {
        self.tool(&self.bin.clone(), args)
    }

    fn tool(&self, program: &Path, args: &[&str]) -> Ran {
        let output = self
            .command(program)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("{} {args:?}: {e}", program.display()));
        Ran {
            what: format!("{} {}", program.display(), args.join(" ")),
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// `passportsim mcp` over stdio: `asked` in, one JSON value per answer line out.
    fn mcp(&self, asked: &[serde_json::Value]) -> Vec<serde_json::Value> {
        use std::io::Write;

        let mut child = self
            .command(&self.bin)
            .arg("mcp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("`passportsim mcp` starts");
        {
            let mut stdin = child.stdin.take().expect("the MCP client writes to stdin");
            for message in asked {
                writeln!(stdin, "{message}").expect("one message per line");
            }
        }
        let output = child
            .wait_with_output()
            .expect("the MCP server ends at EOF");
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "`mcp` ended cleanly: {}\n{text}",
            String::from_utf8_lossy(&output.stderr)
        );
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).unwrap_or_else(|e| panic!("an MCP answer: {e}: {line}"))
            })
            .collect()
    }
}

struct DaemonStop<'a>(&'a Clean);

impl Drop for DaemonStop<'_> {
    fn drop(&mut self) {
        let _ = self
            .0
            .command(&self.0.bin.clone())
            .args(["serve", "--stop"])
            .output();
    }
}

struct Ran {
    what: String,
    code: i32,
    out: String,
    err: String,
}

impl Ran {
    /// The whole output, for an assertion message: a refusal is the finding here.
    fn text(&self) -> String {
        format!(
            "`{}` exited {}\nstdout: {}\nstderr: {}",
            self.what, self.code, self.out, self.err
        )
    }
}

/// A fresh directory below the target directory, per process, so two test runs sharing a target
/// directory never remove each other's trees.
fn tempdir(name: &str) -> PathBuf {
    let dir = super::target_dir(&repo_root())
        .join("package-test-tmp")
        .join(std::process::id().to_string())
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temporary directory");
    dir
}
