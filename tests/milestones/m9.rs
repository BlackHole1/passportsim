//! Milestone M9 tests. Names use the prefix `t<tier>_m9a_` or `t<tier>_m9b_` so `xtask ci` can
//! count them.
//!
//! [`t1_m9b_browser_journal_replays_natively`] replays the journal a `Wall`-paced browser
//! session exported on the native machine, and the determinism report the browser's core printed
//! must come out again. A replay that presented no frame or wrote no sample prints `NOT_RUN` for
//! that sub-leg, so a match of empty output is never counted. The other M9 checks live in
//! `web/tests/*.spec.ts` and `cargo xtask bench-browser`.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;

use pemu_core::input::{ButtonId, InputEvent};
use pemu_core::time::VTime;
use std::sync::Arc;

use pemu_loader::bundle::{
    BUNDLE_APP_ELF, BUNDLE_BOOT_ELF, BUNDLE_EFUSE, BUNDLE_FLASH, Bundle, BundleInput, FlashImage,
    build as build_bundle,
};
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::elf::ElfInfo;
use pemu_machine::config::{Assets, EfuseSource, MachineConfig};
use pemu_machine::determinism::report;
use pemu_machine::machine::{At, Machine};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::StopSet;

/// The variable naming the record `web/tests/m9.spec.ts` writes of its isolated run: `{image,
/// core: {sha256, abiVersion}, config, bundle, roles, journal, report}`, where `roles` are the
/// asset roles the core reported loading.
const RECORD_ENV: &str = "PEMU_BROWSER_RECORD";

/// The wasm core the spec serves unless `PEMU_E2E_CORE` names another (`web/tests/m9.spec.ts`).
const CORE_ENV: &str = "PEMU_E2E_CORE";
const DEFAULT_CORE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32-unknown-unknown/wasm-release/pemu_wasm.wasm"
);

/// `pemu_wasm::layout::ABI_VERSION`, read from its source line: this package does not link
/// `pemu-wasm`.
fn abi_version() -> u64 {
    include_str!("../../crates/pemu-wasm/src/layout.rs")
        .lines()
        .find_map(|line| line.strip_prefix("pub const ABI_VERSION: u32 = "))
        .and_then(|rest| rest.trim_end_matches(';').parse().ok())
        .expect("layout.rs declares `pub const ABI_VERSION: u32 = N;`")
}

/// Why a record cannot be replayed against this tree, or `None`: it must name the SHA-256 of the
/// wasm core built here and this tree's ABI version. A stale record from another build is skipped.
fn stale_record(record: &serde_json::Value) -> Option<String> {
    let Some(sha) = record["core"]["sha256"].as_str() else {
        return Some("the browser record names no wasm core SHA-256; rerun the spec".to_string());
    };
    let Some(abi) = record["core"]["abiVersion"].as_u64() else {
        return Some("the browser record names no ABI version; rerun the spec".to_string());
    };
    if abi != abi_version() {
        return Some(format!(
            "the browser record is of ABI version {abi}, this tree is {}; rerun the spec",
            abi_version()
        ));
    }
    let core =
        std::env::var_os(CORE_ENV).map_or_else(|| DEFAULT_CORE.into(), std::path::PathBuf::from);
    let Ok(bytes) = std::fs::read(&core) else {
        return Some(format!(
            "no wasm core at {} to check the record against; build it (`cargo build -p pemu-wasm --lib --target wasm32-unknown-unknown --profile wasm-release`) or set {CORE_ENV}",
            core.display()
        ));
    };
    let built = pemu_loader::hex(&pemu_loader::sha256(&bytes));
    (built != sha).then(|| {
        format!(
            "the browser record is of wasm core {sha}, the core built here is {built}; rerun the spec"
        )
    })
}

/// The corpus files of the published demo bundle by the name the bundle keeps them under
/// (`xtask/src/package/demo.rs` `FILES`).
const DEMO_CORPUS_FILES: [&str; 2] = ["FoloToy-AI-Passport-8MB.bin", "FoloToy-AI-Passport.elf"];

/// The configuration and assets the browser's core was built from: the bundle is rebuilt from the
/// corpus files the record names and must hash to the one the browser served. `None` (after a
/// skip) when a corpus file is not on this host.
fn browser_machine(test: &str, record: &serde_json::Value) -> Option<(MachineConfig, Assets)> {
    let bundle = &record["bundle"];
    let files = bundle["files"]
        .as_array()
        .expect("the record names the bundle's files");
    let mut payloads = Vec::new();
    for file in files {
        let name = file["name"].as_str().expect("a bundle file has a name");
        assert!(
            DEMO_CORPUS_FILES.contains(&name),
            "{test}: the bundle file `{name}` is not one the demo bundle carries"
        );
        let path = common::corpus_file_or_skip(test, common::OFFICIAL, name)?;
        let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
        assert_eq!(
            Some(pemu_loader::hex(&pemu_loader::sha256(&bytes)).as_str()),
            file["sha256"].as_str(),
            "{test}: `{name}` is not the file the browser served"
        );
        payloads.push((
            file["role"].as_str().expect("a role").to_string(),
            name,
            bytes,
        ));
    }
    let inputs: Vec<BundleInput<'_>> = payloads
        .iter()
        .map(|(role, name, bytes)| BundleInput { role, name, bytes })
        .collect();
    let rebuilt = build_bundle(bundle["id"].as_str(), bundle["name"].as_str(), &inputs);
    assert_eq!(
        Some(pemu_loader::hex(&pemu_loader::sha256(&rebuilt)).as_str()),
        bundle["sha256"].as_str(),
        "{test}: `pemu_loader::bundle::build` does not reproduce the bundle the browser served"
    );
    let parsed = Bundle::parse(&rebuilt).expect("the rebuilt bundle parses");
    let data = |role: &str| {
        parsed
            .role_data(role)
            .unwrap_or_else(|| panic!("{test}: the core loaded `{role}`, which the bundle lacks"))
    };
    let roles: Vec<&str> = record["roles"]
        .as_array()
        .expect("the record names the roles the core loaded")
        .iter()
        .map(|role| role.as_str().expect("a role name"))
        .collect();
    assert!(roles.contains(&BUNDLE_FLASH), "{test}: no flash was loaded");
    let mut config = MachineConfig::default();
    let mut flash = None;
    let mut app_elf = None;
    let mut boot_elf = None;
    let mut efuse = EfuseImage::synth(config.seed);
    for role in &roles {
        match *role {
            BUNDLE_FLASH => {
                flash = Some(FlashImage::from_merged(data(role)).expect("the flash parses"));
            }
            BUNDLE_APP_ELF => {
                app_elf = Some(Arc::new(
                    ElfInfo::parse(data(role)).expect("the app ELF parses"),
                ));
            }
            BUNDLE_BOOT_ELF => {
                boot_elf = Some(Arc::new(
                    ElfInfo::parse(data(role)).expect("the bootloader ELF parses"),
                ));
            }
            BUNDLE_EFUSE => {
                efuse = EfuseImage::from_dump(data(role)).expect("the eFuse dump parses");
                config.efuse = EfuseSource::Dump;
            }
            other => {
                panic!("{test}: the core loaded role `{other}`, which a replay cannot rebuild")
            }
        }
    }
    let assets = Assets::with_bundled_rom(flash.expect("flash"), app_elf, boot_elf, efuse)
        .expect("the bundled ROM is pinned");
    Some((config, assets))
}

/// One journal entry as `web/src/worker/input.ts` `JournalExport` writes it.
fn event_of(entry: &serde_json::Value) -> (VTime, InputEvent) {
    let at: u64 = entry["atPs"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("a journal entry has a decimal `atPs`");
    // The event in the serde form of `InputEvent`, whichever door it came through.
    let event: InputEvent = serde_json::from_value(entry["event"].clone())
        .expect("a journal entry's `event` is an InputEvent");
    (VTime(at), event)
}

/// The field of a report line that starts with `name` (`state=`, `vt=`, ...).
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split(' ').find(|f| f.starts_with(name))
}

/// The session is `web/tests/m9.spec.ts`'s isolated run of `official` with an Ok press; its record
/// names the report the browser's core gave after the pause.
#[test]
fn t1_m9b_browser_journal_replays_natively() {
    let test = "t1_m9b_browser_journal_replays_natively";
    let Some(record) = std::env::var_os(RECORD_ENV) else {
        common::skip(
            test,
            &format!(
                "no browser record: run `bun run e2e tests/m9.spec.ts` in web/ with {RECORD_ENV} set, then this test with the same variable"
            ),
        );
        return;
    };
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).expect("the browser record is readable"))
            .expect("the browser record is JSON");
    if let Some(reason) = stale_record(&record) {
        common::skip(test, &reason);
        return;
    }
    assert_eq!(
        record["image"], "official",
        "the record is of the `official` demo"
    );
    let browser = record["report"]
        .as_str()
        .expect("the record carries the browser's report");
    let vt: u64 = field(browser, "vt=")
        .and_then(|f| f[3..].parse().ok())
        .expect("the report names its virtual time");
    // An export that dropped live chunks says so; replaying it would compare a different run.
    assert_eq!(
        record["journal"]["format"], 1,
        "the record's journal is export format 1"
    );
    if record["journal"]["replayable"] != true {
        common::skip(
            test,
            &format!(
                "the browser journal dropped live chunks {}; export it with includeSecrets to replay",
                record["journal"]["dropped"]
            ),
        );
        return;
    }
    let entries = record["journal"]["entries"]
        .as_array()
        .expect("the record carries the journal");
    assert!(!entries.is_empty(), "the session journaled its press");
    // A press shorter than the firmware's debounce changes nothing, and the negative control below
    // would then fail for the probe's timing rather than for the replay.
    let ok_edge = |down: bool| {
        entries.iter().map(event_of).find(|(_, event)| {
            *event
                == InputEvent::Button {
                    id: ButtonId::Ok,
                    down,
                }
        })
    };
    if let (Some((pressed, _)), Some((released, _))) = (ok_edge(true), ok_edge(false)) {
        let held = released.0.saturating_sub(pressed.0);
        assert!(
            held >= 100_000_000_000,
            "{test}: the browser held the press {held} ps, under 100 ms; rerun the spec"
        );
    }
    // The export is the machine's journal, so the registry click the session made through
    // `pemu_call` is in it beside the Worker's own press.
    let doors: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry["door"].as_str())
        .collect();
    println!("{test}: journal doors {doors:?}");

    // `pemu_new` with `{fw: "official"}` is `MachineConfig::default()` over the bundled ROM and the
    // synthesized eFuse of seed 0, with the assets of the bundle it loaded.
    let Some((config, assets)) = browser_machine(test, &record) else {
        return;
    };
    let roles = record["roles"].clone();
    let mut m = Machine::new(config, assets).expect("the machine composes");
    for entry in entries {
        let (at, event) = event_of(entry);
        m.input(At::Vt(at), event)
            .expect("a journaled input is not in the past of a fresh machine");
    }
    let out = m.run(RunLimits {
        until: Some(VTime(vt)),
        max_insns: None,
        stops: StopSet::default(),
    });
    let native = report(&out.reason, &m);
    // The browser's last slice ended wherever its pacing put it; this run ends at the same instant
    // with `Until`. Everything else must be equal.
    for name in [
        "state=", "insns=", "vt=", "usj=", "uart0=", "lines=", "frame=", "pcm=",
    ] {
        assert_eq!(
            field(&native, name),
            field(browser, name),
            "{test}: `{name}` of the native replay differs from the browser session\nnative:  {native}\nbrowser: {browser}"
        );
    }
    // Without the journal the same run ends elsewhere, so the comparison is not of two idle machines.
    let (idle_config, idle_assets) =
        browser_machine(test, &record).expect("the corpus files were there a moment ago");
    let mut idle = Machine::new(idle_config, idle_assets).expect("the machine composes");
    let idle_out = idle.run(RunLimits {
        until: Some(VTime(vt)),
        max_insns: None,
        stops: StopSet::default(),
    });
    let idle_report = report(&idle_out.reason, &idle);
    assert_ne!(
        field(&idle_report, "state="),
        field(browser, "state="),
        "{test}: the run without the journal ends in the browser's state, so the press compared nothing"
    );
    // Without the registry's inputs the run ends elsewhere too.
    if doors.contains(&"registry") {
        let (config, assets) =
            browser_machine(test, &record).expect("the corpus files were there a moment ago");
        let mut partial = Machine::new(config, assets).expect("the machine composes");
        for entry in entries.iter().filter(|entry| entry["door"] != "registry") {
            let (at, event) = event_of(entry);
            partial
                .input(At::Vt(at), event)
                .expect("a journaled input is not in the past of a fresh machine");
        }
        let partial_out = partial.run(RunLimits {
            until: Some(VTime(vt)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_ne!(
            field(&report(&partial_out.reason, &partial), "state="),
            field(browser, "state="),
            "{test}: the run without the registry's inputs ends in the browser's state, so they compared nothing"
        );
        println!("RAN {test} registry-inputs: the replay without them ends in another state");
    }
    // With frames flowing, the glass without the press differs, so the journal shaped the frames.
    if idle.io().frame.generation() > 0 {
        assert_ne!(
            field(&idle_report, "frame="),
            field(browser, "frame="),
            "{test}: the frames without the journal equal the browser's, so the frame leg compared nothing the press did"
        );
    }
    println!(
        "RAN {test} official: {} journaled inputs ({} from registry commands) replayed to vt={vt}ps over roles {roles} equal the browser",
        entries.len(),
        doors.iter().filter(|door| **door == "registry").count()
    );
    let frames = m.io().frame.generation();
    if frames == 0 {
        println!(
            "NOT_RUN {test} frame: the replay presented no frame, the frame digests compare nothing"
        );
    } else {
        println!("RAN {test} frame: {frames} frames presented, digest equal the browser");
    }
    let samples = m.io().audio_out.head();
    if samples == 0 {
        println!(
            "NOT_RUN {test} pcm: the replay wrote no PCM sample, the PCM digests compare nothing"
        );
    } else {
        println!("RAN {test} pcm: {samples} PCM samples written, digest equal the browser");
    }
}
