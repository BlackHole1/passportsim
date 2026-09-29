//! Tests of the probe line format and of the manifest reader.
//!
//! These run on any host: they touch no toolchain, no build and no device. The one test that
//! reads the tree checks that the committed manifest still describes the committed probes, which
//! is the part of `cargo xtask probes --check` that needs no ESP-IDF.

use super::line::{ProbeLine, Run, SCHEMA, parse_console};
use super::manifest::{Manifest, project_files};

/// Reads one committed QEMU oracle capture.
fn capture(name: &str) -> String {
    let path = crate::util::workspace_root().join(format!("tests/fw/captures/{name}.txt"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} is committed: {err}", path.display()))
}

/// Every tag a capture's probe lines carry, sorted and deduplicated.
fn tags(text: &str) -> Vec<String> {
    let console = parse_console(text).expect("the capture parses");
    let mut tags: Vec<String> = console
        .lines
        .iter()
        .map(|line| line.tag.to_string())
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

/// A capture in the shape a probe actually prints, used by several tests below.
const CAPTURE: &str = "\
ESP-ROM:esp32c3-api1-20210207\r\n\
I (25) boot: ESP-IDF v5.5.3 2nd stage bootloader\r\n\
PROBE|name=probe_boot_facts|schema=passport-emu/probe-line/1\r\n\
CHIP|model=5|revision=101|major=1|minor=1|cores=1|features=0x00000012\r\n\
HEAP|internal_8bit|total=280000|free=270000|largest=110000|min=269000|blocks=4\r\n\
MAC|placeholder=1|mac=02:00:00:c3:00:01\r\n\
DONE|name=probe_boot_facts|status=ok\r\n";

#[test]
fn parses_a_line_with_positional_segments_and_fields() {
    let parsed = ProbeLine::parse("HEAP|after_init|free=1024|largest=512")
        .expect("grammar")
        .expect("a probe line");
    assert_eq!(parsed.tag, "HEAP");
    assert_eq!(parsed.positional, vec!["after_init"]);
    assert_eq!(parsed.field("free"), Some("1024"));
    assert_eq!(parsed.field("largest"), Some("512"));
    assert_eq!(parsed.field("absent"), None);
}

#[test]
fn an_empty_field_value_is_allowed() {
    let parsed = ProbeLine::parse("NOTE|what=deep_sleep|why=")
        .expect("grammar")
        .expect("a probe line");
    assert_eq!(parsed.field("why"), Some(""));
}

#[test]
fn ordinary_console_lines_are_not_probe_lines() {
    for line in [
        "I (325) main_task: Calling app_main()",
        "ESP-ROM:esp32c3-eco7-20230720",
        "rst:0x15 (USB_UART_CHIP_RESET),boot:0xa (SPI_FAST_FLASH_BOOT)",
        "lowercase|free=1",
        "|free=1",
        "TOOLONGTAGTOOLONGTAG|a=1",
        "plain text with no separator",
    ] {
        assert_eq!(ProbeLine::parse(line).expect("grammar"), None, "{line}");
    }
}

#[test]
fn a_malformed_probe_line_is_an_error_not_a_skipped_line() {
    // A bad key, a positional segment after a field, a trailing separator and no segment at all
    // are the four ways a firmware format string goes wrong; none may be silently dropped.
    for line in [
        "HEAP|Free=1024",
        "HEAP|free=1024|stage",
        "HEAP|free=1024|",
        "HEAP|",
    ] {
        assert!(ProbeLine::parse(line).is_err(), "{line} should not parse");
    }
}

#[test]
fn a_value_may_not_carry_a_separator_or_a_control_character() {
    assert!(ProbeLine::parse("ECHO|hex=ab\u{7}cd").is_err());
    // A '|' inside a value is indistinguishable from a segment break, so it splits instead, and
    // the fragment after it has no '=': that is the positional-after-field error.
    assert!(ProbeLine::parse("ECHO|hex=ab|cd").is_err());
}

#[test]
fn reads_a_capture_as_a_run() {
    let run = Run::read(CAPTURE).expect("a run");
    assert_eq!(run.name, "probe_boot_facts");
    assert_eq!(run.status, "ok");
    assert!(run.failures.is_empty());
    // CHIP, HEAP and MAC are facts; PROBE and DONE are the frame.
    assert_eq!(run.facts, 3);
    assert!(run.passed());
}

#[test]
fn a_fail_line_makes_a_run_fail_even_when_done_says_ok() {
    let capture = CAPTURE.replace(
        "DONE|",
        "FAIL|what=read_mac|detail=esp_read_mac failed\r\nDONE|",
    );
    let run = Run::read(&capture).expect("a run");
    assert_eq!(run.status, "ok");
    assert_eq!(run.failures, vec!["read_mac".to_string()]);
    assert!(!run.passed());
}

#[test]
fn a_capture_with_a_foreign_schema_is_refused() {
    let capture = CAPTURE.replace(SCHEMA, "passport-emu/probe-line/99");
    let err = Run::read(&capture).expect_err("a schema mismatch");
    assert!(err.contains("probe-line/99"), "{err}");
}

#[test]
fn a_capture_without_a_footer_is_refused() {
    let capture = CAPTURE.replace("DONE|name=probe_boot_facts|status=ok\r\n", "");
    assert!(Run::read(&capture).is_err());
}

#[test]
fn the_last_footer_of_a_restarting_probe_closes_the_run() {
    // probe_reset prints one header per boot; the run is closed by the last DONE.
    let mut capture = String::new();
    for _ in 0..3 {
        capture.push_str("PROBE|name=probe_reset|schema=");
        capture.push_str(SCHEMA);
        capture.push_str("\nBOOT|stage=restart|boot=1|reason=3|raw=0x0c|wake=0\n");
    }
    capture.push_str("DONE|name=probe_reset|status=ok\n");
    let run = Run::read(&capture).expect("a run");
    assert_eq!(run.name, "probe_reset");
    assert_eq!(run.facts, 3);
    assert!(run.passed());
}

#[test]
fn a_line_a_reset_cut_short_is_skipped_not_an_error() {
    // probe_reset resets 24 times on purpose, three of them while printf output is still
    // draining out of the USJ FIFO, so a boot's last line routinely stops mid-segment. That may
    // not throw away the other 24 boots.
    let capture = format!(
        "PROBE|name=probe_reset|schema={SCHEMA}\n\
         BOOT|stage=restart|boot=1|reason=3|raw=0x0c|wake=0\n\
         STORE|store4=0x00000000|\n\
         ESP-ROM:esp32c3-api1-20210207\n\
         PROBE|name=probe_reset|schema={SCHEMA}\n\
         BOOT|stage=restart|boot=2|reason=3|raw=0x0c|wake=0\n\
         DONE|name=probe_reset|status=ok\n"
    );
    let run = Run::read(&capture).expect("a run");
    assert_eq!(run.truncated, 1);
    assert_eq!(run.facts, 2);
    assert!(run.passed());
}

#[test]
fn a_last_line_with_no_newline_is_treated_as_cut_short() {
    let capture = format!(
        "PROBE|name=probe_reset|schema={SCHEMA}\n\
         DONE|name=probe_reset|status=ok\n\
         SENS|stage=rwdt|reg=rom_table_lock|"
    );
    let run = Run::read(&capture).expect("a run");
    assert_eq!(run.truncated, 1);
    assert!(run.passed());
}

#[test]
fn a_complete_but_malformed_line_is_still_an_error() {
    // The tolerance is only for lines the capture itself shows were cut short. A firmware format
    // string with a typo, on a line that ended normally, still has to be reported.
    let capture = format!(
        "PROBE|name=probe_reset|schema={SCHEMA}\n\
         STORE|store4=0x00000000|\n\
         BOOT|stage=restart|boot=1|reason=3|raw=0x0c|wake=0\n\
         DONE|name=probe_reset|status=ok\n"
    );
    let err = Run::read(&capture).expect_err("a malformed line");
    assert!(err.contains("line 2"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// The committed QEMU oracle captures (tests/fw/captures/README.md).
//
// These are the only tests that relate the reader to what the firmware really prints: everything
// above uses text this module wrote itself.
// ---------------------------------------------------------------------------------------------

/// Probes with no oracle capture, and why. A capture is taken "where QEMU models the path"; for
/// these it does not, so a capture would record the oracle's gap rather than the
/// probe (tests/fw/captures/README.md states what each run showed).
const NO_ORACLE_CAPTURE: [(&str, &str); 12] = [
    (
        "pkgatt",
        "QEMU has no BLE controller: btdm_low_power_mode_init asserts",
    ),
    (
        "probe_campaign_radio",
        "QEMU has no BLE controller, and the campaign's reference is the device \
         (specs/notes/silicon-campaign.md)",
    ),
    (
        "probe_campaign_regs",
        "a silicon campaign probe: every fact is a class C or UNVERIFIED row the device settles \
         and QEMU's own register model does not (specs/notes/silicon-campaign.md)",
    ),
    (
        "probe_campaign_reset",
        "a silicon campaign probe: the super-watchdog and deep-sleep facts are compared with the \
         device, not the oracle (specs/notes/silicon-campaign.md)",
    ),
    (
        "probe_campaign_timing",
        "a silicon campaign probe of timings, and QEMU's icount clock is no timing reference \
         (specs/notes/silicon-campaign.md)",
    ),
    (
        "probe_stack",
        "QEMU has no hardware stack guard: the run goes silent after ARMED",
    ),
    (
        "probe_timing",
        "QEMU maps no SPI2 or I2C0, and its icount clock is no timing reference",
    ),
    (
        "probe_wifi_conn",
        "QEMU has no Wi-Fi: esp_phy_enable asserts on the missing modem clock",
    ),
    (
        "probe_wifi_assoc",
        "the committed build has no credential, so its radio path is compiled out, and a \
         credentialled build meets QEMU's missing modem clock",
    ),
    (
        "probe_wifi_http",
        "QEMU has no Wi-Fi: esp_phy_enable asserts on the missing modem clock",
    ),
    (
        "scan3",
        "QEMU has no Wi-Fi: esp_phy_enable asserts on the missing modem clock",
    ),
    (
        "sleep_timer",
        "QEMU never wakes from light sleep: the interrupt watchdog resets the chip",
    ),
];

#[test]
fn every_probe_has_a_committed_oracle_capture_or_a_reason() {
    let root = crate::util::workspace_root();
    let names = super::build::probe_names(&root).expect("the probes directory");
    for name in &names {
        let path = root.join(format!("tests/fw/captures/{name}.txt"));
        match NO_ORACLE_CAPTURE.iter().find(|(probe, _)| probe == name) {
            None => assert!(path.is_file(), "{} has no oracle capture", path.display()),
            Some((_, why)) => assert!(
                !path.exists(),
                "{} is committed, but `{name}` is listed as not modelled by QEMU: {why}",
                path.display()
            ),
        }
    }
    for (probe, _) in NO_ORACLE_CAPTURE {
        assert!(
            names.iter().any(|name| name == probe),
            "`{probe}` is listed as uncaptured but is not a probe"
        );
    }
}

#[test]
fn the_wave_3b_oracle_captures_are_passing_runs() {
    // Taken with `xtask oracle consoles` and the pinned `qemu-oracle`, whose USJ device
    // raises SOF, so these run to their footer (tests/fw/captures/README.md).
    for (name, stages) in [
        (
            "flash_stress",
            vec![
                "PART", "ERASE", "TIME", "PATTERN", "AND", "SECTOR", "STRADDLE", "CYCLES",
                "PERSIST",
            ],
        ),
        ("hle_probe", vec!["STATE", "CALL", "ORDER", "DELETE", "ISR"]),
        ("probe_deadlock", vec!["DEADLOCK", "TASK"]),
        (
            "probe_limits",
            vec!["HEAPREG", "LIMIT", "LIMREG", "AUDIO", "ABOVE"],
        ),
        ("probe_panic", vec!["ARMED", "AFTER"]),
        ("probe_wdt", vec!["BOOT", "TWDT", "IWDT", "SUMMARY"]),
        // The result lines are also compared, line for line, with the host's by
        // `t1_m8_probe_crypto_matches_the_host_computed_values`.
        ("probe_crypto", vec!["AES", "RSA", "MPI"]),
    ] {
        let run = Run::read(&capture(name)).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(run.name, name);
        assert!(run.passed(), "{name}: {run:?}");
        let found = tags(&capture(name));
        for stage in stages {
            assert!(
                found.iter().any(|tag| tag == stage),
                "{name}: no {stage} in {found:?}"
            );
        }
    }
}

#[test]
fn the_panic_capture_shows_the_load_access_fault_of_a_null_read() {
    // A NULL read in a task. mcause 5 is a load access fault and MTVAL is the address.
    let text = capture("probe_panic");
    assert!(text.contains("panic'ed (Load access fault)"), "{text}");
    assert!(
        text.contains("MCAUSE  : 0x00000005  MTVAL   : 0x00000000"),
        "{text}"
    );
    assert!(text.contains("AFTER|reason=4|raw=0x0c"), "{text}");
}

#[test]
fn the_limits_capture_meets_the_audio_limit() {
    // malloc(96000) succeeds; a request one byte above the largest free block fails.
    let text = capture("probe_limits");
    assert!(text.contains("AUDIO|size=96000|"), "{text}");
    assert!(
        text.lines()
            .any(|line| line.starts_with("ABOVE|") && line.ends_with("|at=1|above=0")),
        "{text}"
    );
}

#[test]
fn the_boot_facts_oracle_capture_is_a_passing_run() {
    let run = Run::read(&capture("probe_boot_facts")).expect("a run");
    assert_eq!(run.name, "probe_boot_facts");
    assert!(run.passed(), "{run:?}");
    assert_eq!(
        tags(&capture("probe_boot_facts")),
        [
            "CHIP", "DONE", "EFUSE", "FLASH", "HEAP", "HEAPREG", "MAC", "PART", "PROBE", "RESET",
            "STRAP"
        ]
    );
}

#[test]
fn the_reset_oracle_capture_is_a_passing_run_of_twenty_restarts() {
    let text = capture("probe_reset");
    let run = Run::read(&text).expect("a run");
    assert_eq!(run.name, "probe_reset");
    assert!(run.passed(), "{run:?}");
    // The 20-restart loop of the `restart-reason` check, with the raw cause asserted on chip: the
    // probe only counts a boot when `esp_reset_reason` is ESP_RST_SW *and* the raw RTC_CNTL cause
    // is 0x0c.
    assert!(
        text.contains("SUMMARY|restarts=20|restart_reasons_ok=20|target=20"),
        "the capture does not show twenty good restarts"
    );
    let restarts: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("RESTART|"))
        .collect();
    assert_eq!(restarts.len(), 20);
    for line in &restarts {
        assert!(line.ends_with("|reason=3|raw=0x0c"), "{line}");
    }
    assert!(run.stages.contains(&"restart".to_string()), "{run:?}");
    assert!(
        run.stages.contains(&"after_deep_sleep".to_string()),
        "{run:?}"
    );
}

#[test]
fn the_intc_oracle_capture_shows_the_map_reroute() {
    // The "MAP re-route": writing the source's MAP register alone moves it to
    // another CPU line, and the handler of *that* line is the one that runs.
    let text = capture("probe_intc");
    assert!(
        text.contains("|isr_a=0|isr_b=1"),
        "the capture does not show the re-routed source reaching the other line"
    );
    let names: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("INTC|"))
        .filter_map(|line| line.split('|').nth(1))
        .collect();
    assert!(names.contains(&"from_cpu_2_before"), "{names:?}");
    assert!(names.contains(&"from_cpu_2_rerouted"), "{names:?}");
    assert!(names.contains(&"from_cpu_2_restored"), "{names:?}");
}

#[test]
fn the_incomplete_oracle_captures_still_parse() {
    // QEMU drops the console a few milliseconds in, because it raises no USJ SOF interrupt and
    // IDF's connection monitor then declares the host gone (tests/fw/captures/README.md). The
    // captures are cut short, but every probe line in them still has to parse, and the tags have
    // to be the ones the firmware prints.
    for (name, expected) in [
        (
            "probe_intc",
            vec!["EDGE", "INTC", "LAT", "LATSUM", "MAP", "PROBE", "THRESH"],
        ),
        ("probe_clocks", vec!["CLKCFG", "PROBE"]),
        ("usj_echo", vec!["BURST", "PROBE", "USJ"]),
    ] {
        let text = capture(name);
        assert_eq!(tags(&text), expected, "{name}");
        // Cut short means no footer, so the capture is not a run.
        assert!(Run::read(&text).is_err(), "{name}");
    }
}

/// A manifest with one probe, rendered and parsed back.
fn sample_manifest() -> Manifest {
    use super::manifest::{Artifact, Probe, Source};
    let artifact = |sha: &str, size: u64| Artifact {
        sha256: sha.repeat(64 / sha.len()),
        size,
    };
    Manifest {
        idf_version: "v5.5.3".to_string(),
        idf_commit: "a".repeat(40),
        toolchain: "riscv32-esp-elf-gcc (crosstool-NG esp-14.2.0_20251107) 14.2.0".to_string(),
        strip_command: super::manifest::STRIP_COMMAND.to_string(),
        build_dir: "/home/example/.local/share/passportsim/corpus/probes/build".to_string(),
        common_sdkconfig_sha256: "1".repeat(64),
        probe_line_h_sha256: "2".repeat(64),
        probes: vec![Probe {
            name: "probe_boot_facts".to_string(),
            project: "probes/probe_boot_facts".to_string(),
            sources: vec![Source {
                path: "probes/probe_boot_facts/CMakeLists.txt".to_string(),
                sha256: "3".repeat(64),
            }],
            sdkconfig_sha256: "4".repeat(64),
            strip: "--strip-all".to_string(),
            elf: artifact("5", 3_400_000),
            elf_stripped_path: super::manifest::stripped_elf_path("probe_boot_facts"),
            elf_stripped: artifact("a", 350_000),
            app: artifact("6", 160_000),
            bootloader: artifact("7", 21_000),
            partition_table: artifact("8", 3_072),
            merged: artifact("9", 8 * 1024 * 1024),
        }],
    }
}

#[test]
fn a_manifest_survives_a_render_and_parse_round_trip() {
    let manifest = sample_manifest();
    let parsed = Manifest::parse(&manifest.render()).expect("the rendered manifest parses");
    assert_eq!(parsed, manifest);
}

#[test]
fn a_manifest_with_a_foreign_schema_is_refused() {
    let text = sample_manifest()
        .render()
        .replace(super::manifest::SCHEMA, "passport-emu/probes-manifest/99");
    let err = Manifest::parse(&text).expect_err("a schema mismatch");
    assert!(err.contains("probes-manifest/99"), "{err}");
}

#[test]
fn a_manifest_with_a_truncated_hash_is_refused() {
    let text = sample_manifest().render().replace(&"5".repeat(64), "5abc");
    assert!(Manifest::parse(&text).is_err());
}

#[test]
fn comparing_a_manifest_with_a_changed_build_names_every_difference() {
    let committed = sample_manifest();
    let mut fresh = sample_manifest();
    fresh.toolchain = "riscv32-esp-elf-gcc (crosstool-NG esp-13.2.0) 13.2.0".to_string();
    fresh.probes[0].elf.sha256 = "a".repeat(64);
    let differences = committed.compare(&fresh);
    assert_eq!(differences.len(), 2);
    assert!(differences.iter().any(|item| item.what == "toolchain"));
    assert!(
        differences
            .iter()
            .any(|item| item.probe == "probe_boot_facts" && item.what == "elf")
    );
}

#[test]
fn comparing_notices_a_probe_that_was_added_or_lost() {
    let committed = sample_manifest();
    let mut fresh = sample_manifest();
    fresh.probes[0].name = "usj_echo".to_string();
    let differences = committed.compare(&fresh);
    assert_eq!(differences.len(), 2);
    assert!(
        differences.iter().all(|item| item.what == "presence"),
        "{differences:?}"
    );
}

#[test]
fn the_committed_manifest_matches_the_committed_probe_sources() {
    // The part of `cargo xtask probes --check` that needs no ESP-IDF: every source hash in
    // `tests/fw/manifest.toml` still matches the tree, and no probe file is missing from it.
    let root = crate::util::workspace_root();
    let text =
        std::fs::read_to_string(root.join(super::MANIFEST)).expect("the manifest is committed");
    let manifest = Manifest::parse(&text).expect("the committed manifest parses");
    let verified = manifest
        .verify_sources(&root)
        .expect("the committed sources match the manifest");
    assert!(
        verified.checked >= manifest.probes.len(),
        "{} file(s) checked",
        verified.checked
    );
}

#[test]
fn a_note_about_a_small_elf_is_not_a_verification_failure() {
    // The note "the unstripped ELF would fit in the repository too" is an observation, not a
    // problem: a probe built with size optimization and no debug information must not make
    // `cargo xtask probes verify` (and with it `--check` and the manifest test above) fail.
    let root = crate::util::workspace_root();
    let text =
        std::fs::read_to_string(root.join(super::MANIFEST)).expect("the manifest is committed");
    let mut manifest = Manifest::parse(&text).expect("the committed manifest parses");
    for probe in &mut manifest.probes {
        probe.elf = probe.elf_stripped.clone();
    }
    let verified = manifest
        .verify_sources(&root)
        .expect("a note may not fail the verification");
    assert_eq!(verified.notes.len(), manifest.probes.len());
    assert!(
        verified.notes[0].contains("could be committed"),
        "{verified:?}"
    );
}

#[test]
fn a_committed_elf_that_does_not_match_the_manifest_is_a_failure() {
    // The committed ELF is checked with no toolchain, like a source file: an ELF replaced by
    // hand, or left behind by an older build, has to be visible on any host.
    let root = crate::util::workspace_root();
    let text =
        std::fs::read_to_string(root.join(super::MANIFEST)).expect("the manifest is committed");
    let mut manifest = Manifest::parse(&text).expect("the committed manifest parses");
    manifest.probes[0].elf_stripped.sha256 = "0".repeat(64);
    let err = manifest
        .verify_sources(&root)
        .expect_err("a changed committed ELF is a problem");
    assert!(err.contains(&manifest.probes[0].elf_stripped_path), "{err}");
}

#[test]
fn every_committed_elf_is_under_the_appendix_b_limit() {
    // Each committed test binary is under 1 MB. The parser refuses a manifest that claims
    // otherwise, so the limit cannot be bypassed by editing the file.
    let root = crate::util::workspace_root();
    let text =
        std::fs::read_to_string(root.join(super::MANIFEST)).expect("the manifest is committed");
    let manifest = Manifest::parse(&text).expect("the committed manifest parses");
    for probe in &manifest.probes {
        assert!(
            probe.elf_stripped.size <= super::manifest::MAX_COMMITTED_ELF_BYTES,
            "{} is {} bytes",
            probe.elf_stripped_path,
            probe.elf_stripped.size
        );
        assert!(
            root.join(&probe.elf_stripped_path).is_file(),
            "{} is not committed",
            probe.elf_stripped_path
        );
    }
    let over = text.replace(
        &format!(
            "elf_stripped_size = {}",
            manifest.probes[0].elf_stripped.size
        ),
        "elf_stripped_size = 2097152",
    );
    assert!(Manifest::parse(&over).is_err());
}

#[test]
fn every_probe_in_the_tree_is_in_the_committed_manifest() {
    let root = crate::util::workspace_root();
    let text =
        std::fs::read_to_string(root.join(super::MANIFEST)).expect("the manifest is committed");
    let manifest = Manifest::parse(&text).expect("the committed manifest parses");
    let names = super::build::probe_names(&root).expect("the probes directory");
    for name in &names {
        assert!(
            manifest.probes.iter().any(|probe| &probe.name == name),
            "probe `{name}` is in the tree but not in {}",
            super::MANIFEST
        );
    }
    assert_eq!(names.len(), manifest.probes.len());
}

#[test]
fn a_probe_project_holds_only_the_files_the_manifest_lists() {
    let root = crate::util::workspace_root();
    for name in super::build::probe_names(&root).expect("the probes directory") {
        let project = format!("probes/{name}");
        let files = project_files(&root.join(&project), &project).expect("the project directory");
        // Every probe is the same four files: the project, its defaults, the component and the
        // probe itself. A fifth file is fine, but it has to be a deliberate addition that the
        // manifest then records, which the previous test enforces.
        assert!(files.len() >= 4, "{project} holds only {files:?}");
        assert!(files.contains(&format!("{project}/CMakeLists.txt")));
        assert!(files.contains(&format!("{project}/sdkconfig.defaults")));
        assert!(files.contains(&format!("{project}/main/CMakeLists.txt")));
        assert!(files.contains(&format!("{project}/main/{name}.c")));
    }
}

#[test]
fn the_firmware_header_and_the_reader_agree_on_the_schema() {
    // `probes/common/probe_line.h` is the firmware side of the line format; this module is the
    // reader. A schema bump in one and not the other would silently split them.
    let header =
        std::fs::read_to_string(crate::util::workspace_root().join("probes/common/probe_line.h"))
            .expect("the header is committed");
    assert!(
        header.contains(&format!("#define PROBE_LINE_SCHEMA \"{SCHEMA}\"")),
        "probe_line.h does not declare {SCHEMA}"
    );
}

// ---------------------------------------------------------------------------------------------
// The toolchain search: host executable suffix, spawned by absolute path.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_toolchain_is_found_by_absolute_path_in_the_exported_path() {
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    let root = crate::util::workspace_root().join("target/probe-toolchain-test");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("the scratch directory");
    let name = format!("fake-gcc{}", std::env::consts::EXE_SUFFIX);
    std::fs::write(bin.join(&name), b"#!/bin/sh\n").expect("the scratch binary");

    let mut vars = BTreeMap::new();
    let path = std::env::join_paths([root.join("empty"), bin.clone()]).expect("a PATH");
    vars.insert("PATH".to_string(), path);
    let found = super::build::toolchain_binary(&vars, "fake-gcc").expect("the binary is found");
    assert_eq!(found, bin.join(&name));
    assert!(found.is_absolute(), "{found:?}");

    // Nothing is ever resolved through the inherited PATH: a name that is not in the exported
    // PATH is an error naming what was searched, not a command the operating system looks up.
    let err = super::build::toolchain_binary(&vars, "absent-gcc").expect_err("no such binary");
    assert!(err.contains("absent-gcc"), "{err}");
    assert!(err.contains("Searched"), "{err}");

    let mut empty = BTreeMap::new();
    empty.insert("IDF_PATH".to_string(), OsString::from("/somewhere"));
    assert!(super::build::toolchain_binary(&empty, "fake-gcc").is_err());
}

// ---------------------------------------------------------------------------------------------
// Firmware guarantees the reader cannot see.
//
// The probes are C, built only on the macOS build host, so a Rust test cannot run them. These
// read the committed sources and assert the properties that a capture does not show, because the
// property is about what the firmware must *not* do or about a path the QEMU oracle never
// reaches. The manifest's source hashes make the files these read the files that were built.
// ---------------------------------------------------------------------------------------------

/// One probe's source.
fn firmware(name: &str) -> String {
    let path = crate::util::workspace_root().join(format!("probes/{name}/main/{name}.c"));
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

#[test]
fn probe_intc_acknowledges_the_cpu_edge_latch() {
    // An edge-type CPU interrupt latches in INTERRUPT_CORE0_CPU_INT_CLEAR_REG and stays pending
    // until that bit is written. IDF's dispatcher never does it, and `probe_intc` reconfigures
    // the line to edge itself, so without this the first edge delivery re-enters the handler
    // until the interrupt watchdog resets the chip.
    let source = firmware("probe_intc");
    assert!(
        source.contains("esp_cpu_intr_edge_ack"),
        "probe_intc never acknowledges the edge latch"
    );
    // Once in the handler, once after the type register is restored.
    assert_eq!(
        source.matches("esp_cpu_intr_edge_ack(").count(),
        2,
        "{source}"
    );
}

#[test]
fn usj_echo_never_writes_host_bytes_to_the_console() {
    // The console is the stream the reader grades. A host that could put bytes on it could forge
    // `DONE|name=usj_echo|status=ok` or split a probe line in two.
    let source = firmware("usj_echo");
    assert!(
        !source.contains("usb_serial_jtag_write_bytes(buffer"),
        "usj_echo writes the receive buffer back to the port"
    );
    assert!(
        source.contains("sanitize(buffer"),
        "usj_echo does not sanitize what it prints of the host's bytes"
    );
    // The rendering may not carry a separator, a newline or any other non-printable byte.
    assert!(source.contains("byte == '|'"), "{source}");
}

#[test]
fn every_stage_of_probe_reset_ends_in_a_reset() {
    // The sequence advances only by resetting. A stage whose own reset could not be set up has
    // to restart anyway: falling out of the switch returns from `app_main` with no further boot,
    // so no DONE, no SUMMARY, and the MAX_BOOTS guard can never fire.
    let source = firmware("probe_reset");
    let switch = source
        .split_once("// Cause the reset the current stage asks for")
        .expect("the second switch")
        .1;
    let mut in_stage = false;
    for line in switch.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("PROBE_FAIL(") {
            in_stage = true;
        } else if in_stage && (trimmed.starts_with("esp_restart(") || trimmed.starts_with("esp_")) {
            in_stage = false;
        } else if in_stage && trimmed == "break;" {
            panic!("a failed stage of probe_reset breaks out without resetting: {line}");
        }
    }
}

#[test]
fn probe_clocks_judges_the_one_tick_rule_itself() {
    // The probe *checks* that the sources agree within one tick. A probe that only
    // printed deltas would be graded "passed" for any clock behaviour at all, because
    // `Run::passed` looks only at the DONE status and the absence of FAIL lines.
    let source = firmware("probe_clocks");
    assert!(
        source.contains("FAIL|what=clock_consistency"),
        "probe_clocks emits no clock-consistency failure"
    );
    assert!(source.contains("1000000 / configTICK_RATE_HZ"), "{source}");
    for phase in ["busy_loop", "rom_delay_us", "task_delay", "timer_poll"] {
        assert!(
            source.contains(&format!(
                "report(\"{phase}\", PHASE_US, &before, &after, RULE_WALL"
            )),
            "{phase} is not judged"
        );
    }
}

#[test]
fn probe_reset_asserts_the_raw_restart_cause() {
    // The `restart-reason` check names the raw value: `esp_restart` 20 times gives 0x0C each
    // time. The IDF enum alone would not catch it, because `esp_reset_reason` maps several raw
    // causes onto ESP_RST_SW.
    let source = firmware("probe_reset");
    assert!(
        source.contains("#define RAW_RESET_SW_CPU 0x0cu"),
        "{source}"
    );
    assert!(
        source
            .contains("esp_reset_reason() == ESP_RST_SW && raw_reset_cause() == RAW_RESET_SW_CPU"),
        "probe_reset does not assert the raw cause of a software restart"
    );
}

#[test]
fn the_source_directory_of_a_cmake_cache_is_read_from_its_home_entry() {
    let cache = "# This is the CMakeCache file.\n\
                 CMAKE_BUILD_TYPE:STRING=\n\
                 CMAKE_HOME_DIRECTORY:INTERNAL=/tmp/one/probes/probe_intc\n";
    assert_eq!(
        super::build::cache_home_directory(cache),
        Some("/tmp/one/probes/probe_intc")
    );
    assert_eq!(
        super::build::cache_home_directory("CMAKE_BUILD_TYPE:STRING=\n"),
        None
    );
}

/// The committed ELF of `usj_echo`, which every checkout has.
fn committed_elf() -> Vec<u8> {
    let path = crate::util::workspace_root().join(super::manifest::stripped_elf_path("usj_echo"));
    std::fs::read(&path).unwrap_or_else(|err| panic!("{} is committed: {err}", path.display()))
}

/// The file offset of the first byte of section `name`.
fn section_offset(elf: &[u8], name: &str) -> usize {
    let info = pemu_loader::elf::ElfInfo::parse(elf).expect("the committed ELF parses");
    info.section(name).expect("the section exists").offset as usize
}

#[test]
fn an_elf_loads_the_same_image_as_itself() {
    let elf = committed_elf();
    super::loaded::same_loaded_image(&elf, &elf).expect("identical ELFs load the same image");
}

#[test]
fn a_changed_loaded_byte_is_not_the_same_image() {
    // A strip step that altered code would be caught, not recorded as the committed ELF.
    let elf = committed_elf();
    let mut changed = elf.clone();
    changed[section_offset(&elf, ".flash.text")] ^= 0xff;
    let err = super::loaded::same_loaded_image(&elf, &changed).expect_err("a changed byte");
    assert!(err.contains(".flash.text changed contents"), "{err}");
}

#[test]
fn a_moved_loaded_section_is_not_the_same_image() {
    let elf = committed_elf();
    let info = pemu_loader::elf::ElfInfo::parse(&elf).expect("parses");
    let index = info.section(".iram0.text").expect("exists").index;
    // Elf32_Shdr: e_shoff at 0x20, e_shentsize at 0x2e, sh_addr at 0x0c.
    let shoff = u32::from_le_bytes(elf[0x20..0x24].try_into().unwrap()) as usize;
    let entsize = u16::from_le_bytes(elf[0x2e..0x30].try_into().unwrap()) as usize;
    let at = shoff + index * entsize + 0x0c;
    let mut moved = elf.clone();
    let addr = u32::from_le_bytes(moved[at..at + 4].try_into().unwrap()) + 4;
    moved[at..at + 4].copy_from_slice(&addr.to_le_bytes());
    let err = super::loaded::same_loaded_image(&elf, &moved).expect_err("a moved section");
    assert!(err.contains(".iram0.text moved"), "{err}");
}

#[test]
fn the_committed_elfs_carry_no_placeholder_sections() {
    // The strip step removes them; a committed ELF that still has one was made another way.
    let root = crate::util::workspace_root();
    let text =
        std::fs::read_to_string(root.join(super::MANIFEST)).expect("the manifest is committed");
    let manifest = Manifest::parse(&text).expect("the committed manifest parses");
    for probe in &manifest.probes {
        let bytes = std::fs::read(root.join(&probe.elf_stripped_path)).expect("committed");
        let info = pemu_loader::elf::ElfInfo::parse(&bytes).expect("parses");
        for dummy in super::loaded::DUMMY_SECTIONS {
            assert!(info.section(dummy).is_none(), "{} has {dummy}", probe.name);
        }
    }
}

#[test]
fn a_strip_mode_other_than_the_probes_own_is_refused() {
    // `probe_boot_facts` is not in KEEP_SYMBOLS, so a manifest saying its symbols were kept lies.
    let text = sample_manifest()
        .render()
        .replace("strip = \"--strip-all\"", "strip = \"--strip-debug\"");
    let err = Manifest::parse(&text).expect_err("a wrong strip mode");
    assert!(err.contains("--strip-debug"), "{err}");
}

#[test]
fn the_hook_and_panic_symbols_are_in_the_committed_elfs() {
    // No toolchain on the reader's side: the committed ELF itself names the hook points and the
    // panic frames.
    let root = crate::util::workspace_root();
    for (name, symbols) in [
        (
            "hle_probe",
            vec![
                "hle_probe_hook_delay",
                "hle_probe_hook_malloc",
                "hle_probe_hook_free",
                "hle_probe_hook_post",
                "hle_probe_hook_isr_give",
            ],
        ),
        (
            "probe_panic",
            vec!["probe_panic_read_null", "probe_panic_outer"],
        ),
    ] {
        let bytes = std::fs::read(root.join(super::manifest::stripped_elf_path(name)))
            .expect("the ELF is committed");
        let info = pemu_loader::elf::ElfInfo::parse(&bytes).expect("parses");
        for symbol in symbols {
            assert!(
                info.symbols.addr_of(symbol).is_some(),
                "{name}: no {symbol}"
            );
        }
    }
}

/// A fresh directory under the system temporary directory, removed first if a run left it.
fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pemu-probes-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    dir
}

/// A build directory whose CMake cache holds `cache`, beside a project directory.
fn build_with_cache(name: &str, cache: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = scratch_dir(name);
    let project = root.join("probes/usj_echo");
    let build = root.join("build/usj_echo");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&build).expect("build");
    std::fs::write(build.join("CMakeCache.txt"), cache).expect("cache");
    std::fs::write(build.join("usj_echo.elf"), b"derived").expect("a derived file");
    (project, build)
}

#[test]
fn a_cache_of_this_checkout_is_kept() {
    let (project, build) = build_with_cache("same", b"");
    let cache = format!("CMAKE_HOME_DIRECTORY:INTERNAL={}\n", project.display());
    std::fs::write(build.join("CMakeCache.txt"), cache).expect("cache");
    super::build::discard_foreign_cache(&build, &project).expect("kept");
    assert!(build.join("usj_echo.elf").is_file());
}

#[test]
fn a_cache_of_another_checkout_is_discarded() {
    let (project, build) = build_with_cache("foreign", b"");
    let other = project.parent().unwrap().join("other");
    std::fs::create_dir_all(&other).expect("other checkout");
    let cache = format!("CMAKE_HOME_DIRECTORY:INTERNAL={}\n", other.display());
    std::fs::write(build.join("CMakeCache.txt"), cache).expect("cache");
    super::build::discard_foreign_cache(&build, &project).expect("discarded");
    assert!(!build.exists());
}

#[test]
fn a_cache_whose_home_is_gone_or_missing_is_discarded() {
    let (project, build) = build_with_cache(
        "gone",
        b"CMAKE_HOME_DIRECTORY:INTERNAL=/nonexistent/passportsim/probes/usj_echo\n",
    );
    super::build::discard_foreign_cache(&build, &project).expect("discarded");
    assert!(!build.exists());

    let (project, build) = build_with_cache("missing", b"CMAKE_BUILD_TYPE:STRING=\n");
    super::build::discard_foreign_cache(&build, &project).expect("discarded");
    assert!(!build.exists());
}

#[test]
fn a_cache_that_is_not_utf8_is_discarded_not_an_error() {
    let (project, build) = build_with_cache("binary", b"CMAKE_HOME_DIRECTORY:INTERNAL=\xff\xfe\n");
    super::build::discard_foreign_cache(&build, &project).expect("discarded, not an error");
    assert!(!build.exists());
}

#[test]
fn the_recorded_build_directory_names_no_user() {
    use std::path::Path;
    let home = Path::new("/home/alice");
    let build = home.join(".local/share/passportsim/corpus/probes/build");
    assert_eq!(
        super::build::home_relative(&build, Some(home)),
        "~/.local/share/passportsim/corpus/probes/build"
    );
    let elsewhere = Path::new("/data/passportsim/corpus/probes/build");
    assert_eq!(
        super::build::home_relative(elsewhere, Some(home)),
        "/data/passportsim/corpus/probes/build"
    );
    assert_eq!(
        super::build::home_relative(&build, None),
        build.display().to_string()
    );
}

#[test]
fn a_build_directory_with_no_cache_is_left_alone() {
    let root = scratch_dir("nocache");
    let build = root.join("build/usj_echo");
    std::fs::create_dir_all(&build).expect("build");
    super::build::discard_foreign_cache(&build, &root.join("probes/usj_echo")).expect("left");
    assert!(build.is_dir());
}

#[test]
fn a_second_run_cannot_take_the_build_lock_while_the_first_holds_it() {
    let root = scratch_dir("lock");
    let first = super::build::BuildLock::acquire(&root).expect("the first run locks");
    let second = super::build::BuildLock::try_acquire(&root).expect("no error");
    assert!(second.is_none(), "two runs held the build root at once");
    // A process spawned meanwhile, by this run or by a sibling test, can hold a copy of the lock's
    // descriptor while it starts; the copy must not keep the lock once the guard is dropped.
    let inherited = first.descriptor_copy();
    drop(first);
    let third = super::build::BuildLock::try_acquire(&root).expect("no error");
    assert!(third.is_some(), "the lock was not released");
    drop(inherited);
}

#[test]
fn a_hand_build_sdkconfig_is_not_a_probe_source() {
    // Both are ignored by git, so a manifest listing them would never verify on another checkout.
    let root = scratch_dir("handbuild");
    std::fs::create_dir_all(root.join("main")).expect("main");
    for name in ["CMakeLists.txt", "sdkconfig", "sdkconfig.old", "main/x.c"] {
        std::fs::write(root.join(name), b"x").expect("file");
    }
    let found = project_files(&root, "probes/x").expect("listed");
    assert_eq!(found, vec!["probes/x/CMakeLists.txt", "probes/x/main/x.c"]);
}

#[test]
fn the_hle_delay_capture_blocks_for_several_ticks_within_one_of_the_request() {
    // A 10 ms delay is one tick at 100 Hz, which cannot tell an early or a
    // late return from a correct one. The probe asks for 100 ms and checks both bounds.
    let text = capture("hle_probe");
    let line = text
        .lines()
        .find(|line| line.starts_with("CALL|delay|"))
        .expect("a CALL|delay line");
    let field = |key: &str| -> u32 {
        line.split('|')
            .find_map(|part| part.strip_prefix(&format!("{key}=")))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no {key} in {line}"))
    };
    assert_eq!(field("ms"), 100, "{line}");
    assert!(field("want") >= 2, "{line}");
    assert!(
        field("ticks") >= field("want") && field("ticks") <= field("want") + 1,
        "{line}"
    );
}

#[test]
fn the_limits_capture_carries_what_a_per_region_comparison_needs() {
    // Compared per region against a silicon capture of the same image. Each LIMREG line has
    // the region bounds and consistent used and drained numbers, none wrapped or shrunk, and the
    // IMAGE line names the ELF so the two captures can be matched.
    let text = capture("probe_limits");
    let image = text
        .lines()
        .find_map(|line| line.strip_prefix("IMAGE|elf_sha256="))
        .expect("an IMAGE line");
    assert_eq!(image.len(), 64, "{image}");
    let mut regions = 0;
    for line in text.lines().filter(|line| line.starts_with("LIMREG|")) {
        let get = |key: &str| -> u64 {
            line.split('|')
                .find_map(|part| part.strip_prefix(&format!("{key}=")))
                .map(|value| match value.strip_prefix("0x") {
                    Some(hex) => u64::from_str_radix(hex, 16).expect("hex"),
                    None => value.parse().expect("decimal"),
                })
                .unwrap_or_else(|| panic!("no {key} in {line}"))
        };
        assert_eq!(get("end") - get("start"), get("size"), "{line}");
        assert_eq!(get("shrunk"), 0, "{line}");
        assert_eq!(
            get("used_drained") - get("used_before"),
            get("drained"),
            "{line}"
        );
        assert!(get("drained") <= get("size"), "{line}");
        regions += 1;
    }
    assert!(regions >= 4, "{regions} LIMREG lines");
}

#[test]
fn the_stack_probe_task_does_not_print() {
    // QEMU has no stack guard, so no capture can show where the fault fires. The source is the
    // check: the 2048-byte task body calls no console function.
    let path = crate::util::workspace_root().join("probes/probe_stack/main/probe_stack.c");
    let source = std::fs::read_to_string(&path).expect("the probe source");
    let start = source
        .find("static void stack_task(void *arg)")
        .expect("stack_task");
    let body = &source[start..start + source[start..].find("\n}\n").expect("end of stack_task")];
    for call in ["printf", "PROBE_", "puts", "fflush", "ESP_LOG"] {
        assert!(!body.contains(call), "stack_task calls {call}: {body}");
    }
}

#[test]
fn the_scratch_partitions_stay_clear_of_the_device_layout() {
    // The device's factory partition is 0x10000-0x30FFFF, cardid is 0x356000-0x359FFF, and a read
    // of the device flash found non-blank leftovers at 0x310000-0x317000, 0x330000-0x356000,
    // 0x35A000-0x35D000 and 0x3FA000-0x41A000.
    let keep_out: [(u64, u64); 6] = [
        (0x0, 0x10000),
        (0x10000, 0x310000),
        (0x310000, 0x317000),
        (0x330000, 0x35A000),
        (0x35A000, 0x35D000),
        (0x3FA000, 0x41A000),
    ];
    let root = crate::util::workspace_root();
    for probe in ["flash_stress", "probe_timing"] {
        let path = root.join(format!("probes/{probe}/partitions.csv"));
        let text = std::fs::read_to_string(&path).expect("the table");
        let row = text
            .lines()
            .find(|line| line.starts_with("scratch,"))
            .expect("a scratch row");
        let fields: Vec<&str> = row.split(',').map(str::trim).collect();
        let offset = u64::from_str_radix(fields[3].trim_start_matches("0x"), 16).expect("offset");
        let size = u64::from_str_radix(fields[4].trim_start_matches("0x"), 16).expect("size");
        assert_eq!((offset, size), (0x500000, 0x40000), "{probe}: {row}");
        assert!(
            offset + size <= 0x700000,
            "{probe} reaches the old recovery partition"
        );
        for (start, end) in keep_out {
            assert!(
                offset + size <= start || offset >= end,
                "{probe}: scratch overlaps 0x{start:x}-0x{end:x}"
            );
        }
    }
}

#[test]
fn the_timing_probe_frames_its_spi2_bytes_as_panel_pixels() {
    // QEMU maps no SPI2, so there is no capture to check. Without DC and a RAMWR the timed bytes
    // would be commands to the panel, not a frame.
    let path = crate::util::workspace_root().join("probes/probe_timing/main/probe_timing.c");
    let source = std::fs::read_to_string(&path).expect("the probe source");
    for needle in [
        "#define PIN_LCD_DC 20",
        "#define LCD_CASET 0x2A",
        "#define LCD_RASET 0x2B",
        "#define LCD_RAMWR 0x2C",
        "gpio_set_level(PIN_LCD_DC, dc)",
        "SPI_TRANS_CS_KEEP_ACTIVE",
        "spi_device_acquire_bus",
    ] {
        assert!(source.contains(needle), "probe_timing.c has no `{needle}`");
    }
}

#[test]
fn scan3_checks_every_rc_the_readme_says_and_masks_neighbours() {
    // The README row lists what is checked; the source must agree, and an
    // access point outside the virtual air never prints its SSID or its full BSSID.
    let path = crate::util::workspace_root().join("probes/scan3/main/scan3.c");
    let source = std::fs::read_to_string(&path).expect("the probe source");
    for name in [
        "deinit_before_init",
        "esp_wifi_init",
        "esp_wifi_set_storage",
        "esp_wifi_set_mode",
        "esp_wifi_start",
        "sta_start_seen",
        "scan_start",
        "scan_done",
        "scan_num",
        "scan_records",
        "deinit_while_started",
        "esp_wifi_stop",
        "sta_stop_seen",
        "esp_wifi_deinit",
        "deinit_again",
        "nvs_flash_init",
        "nimble_port_init",
        "ble_synced",
        "adv_start",
        "nimble_port_stop",
    ] {
        assert!(
            source.contains(&format!("check_rc(\"{name}\"")),
            "no check for {name}"
        );
    }
    assert!(
        !source.contains("check_rc(\"scan_busy\""),
        "busy 12294 is UNVERIFIED"
    );
    assert!(source.contains("VIRTUAL_BSSID_PREFIX[5] = {0x02, 0x00, 0x00, 0x47, 0x32}"));
    assert!(source.contains("|(neighbour)|ch=%u|rssi=%d|auth=%u|bssid=%02x:%02x:%02x:xx:xx:xx"));
    assert_eq!(
        source.matches("printf(\"AP|").count(),
        2,
        "one full and one masked AP format"
    );
}

/// The `0x...` value after `key` in a register-dump line of a capture.
fn register(text: &str, key: &str) -> u32 {
    let at = text
        .find(key)
        .unwrap_or_else(|| panic!("no {key} in the capture"));
    let rest = text[at + key.len()..].trim_start_matches([' ', ':']);
    let hex = rest
        .strip_prefix("0x")
        .and_then(|rest| rest.get(..8))
        .unwrap_or_else(|| panic!("no hex after {key}"));
    u32::from_str_radix(hex, 16).expect("hex")
}

#[test]
fn the_panic_fault_is_inside_probe_panic_read_null() {
    // The backtrace names the frames. The committed ELF keeps its symbol table (KEEP_SYMBOLS), so
    // the fault PC resolves with no toolchain: MEPC in the NULL read, RA back in its caller.
    let text = capture("probe_panic");
    let root = crate::util::workspace_root();
    let bytes = std::fs::read(root.join(super::manifest::stripped_elf_path("probe_panic")))
        .expect("the ELF is committed");
    let info = pemu_loader::elf::ElfInfo::parse(&bytes).expect("parses");
    let within = |name: &str, pc: u32| {
        let start = info
            .symbols
            .addr_of(name)
            .unwrap_or_else(|| panic!("no {name}"));
        let size = info
            .symbols
            .lookup(name)
            .map(|sym| sym.size)
            .expect("the symbol");
        let next = start + size;
        assert!(
            pc >= start && pc < next,
            "0x{pc:08x} is not in {name} (0x{start:08x}..0x{next:08x})"
        );
    };
    within("probe_panic_read_null", register(&text, "MEPC    :"));
    within("probe_panic_outer", register(&text, "RA      :"));
}

#[test]
fn the_flash_stress_capture_passes_every_stage_at_the_new_scratch_offset() {
    let text = capture("flash_stress");
    for expected in [
        "PART|offset=0x500000|size=0x040000",
        "ERASE|bytes=262144|not_ff=0",
        "|mismatches=0|",
        "SECTOR|offset=0x08000|inside_not_ff=0|before_mismatches=0|after_mismatches=0",
        "CYCLES|cycles=16|mismatches=0|",
        "PERSIST|marker=1",
    ] {
        assert!(text.contains(expected), "no `{expected}` in {text}");
    }
    for tag in ["PATTERN|", "AND|", "STRADDLE|"] {
        let line = text
            .lines()
            .find(|line| line.starts_with(tag))
            .unwrap_or_else(|| panic!("no {tag}"));
        assert!(line.contains("|mismatches=0|"), "{line}");
    }
}

/// One row of a probe `partitions.csv`: name, type, subtype, offset, size.
fn partition_rows(text: &str) -> Vec<(String, String, String, u64, u64)> {
    let number = |field: &str| -> u64 {
        let field = field.trim();
        match field
            .strip_prefix("0x")
            .or_else(|| field.strip_prefix("0X"))
        {
            Some(hex) => u64::from_str_radix(hex, 16).expect("a hexadecimal field"),
            None => match field.strip_suffix('K') {
                Some(k) => k.parse::<u64>().expect("a decimal field") * 1024,
                None => field.parse::<u64>().expect("a decimal field"),
            },
        }
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            assert!(f.len() >= 5, "`{line}` is not a partition row");
            (
                f[0].to_owned(),
                f[1].to_owned(),
                f[2].to_owned(),
                number(f[3]),
                number(f[4]),
            )
        })
        .collect()
}

#[test]
fn probe_wifi_conn_carries_the_devices_own_partition_table() {
    // The association capture needs a probe the identity guard accepts, which means the image's
    // table must be the device's, `cardid` row included. The device's table is the committed
    // `official.pt` layout, `tests/fixtures/device-facts.toml`, and it is what the attached
    // Passport's own bootloader prints. This test fails if either side is edited without the
    // other. `probe_crypto` and the four silicon campaign probes (specs/notes/silicon-campaign.md)
    // carry the same table, because they run on the device too.
    let root = crate::util::workspace_root();
    let facts = std::fs::read_to_string(root.join("tests/fixtures/device-facts.toml"))
        .expect("the device facts are committed");
    let facts: toml::Table = facts.parse().expect("the device facts parse");
    let expected: Vec<(String, String, String, u64, u64)> = facts["partition"]
        .as_array()
        .expect("`[[partition]]` rows")
        .iter()
        .map(|p| {
            let s = |key: &str| p[key].as_str().expect("a string field").to_owned();
            let n = |key: &str| {
                let text = s(key);
                let hex = text.trim_start_matches("0x").trim_start_matches("0X");
                u64::from_str_radix(hex, 16).expect("a hexadecimal field")
            };
            (s("name"), s("type"), s("subtype"), n("offset"), n("size"))
        })
        .collect();
    for name in [
        "probe_wifi_conn",
        "probe_crypto",
        "probe_campaign_radio",
        "probe_campaign_regs",
        "probe_campaign_reset",
        "probe_campaign_timing",
    ] {
        let csv = std::fs::read_to_string(root.join(format!("probes/{name}/partitions.csv")))
            .expect("the probe's table is committed");
        assert_eq!(partition_rows(&csv), expected, "{name}");
    }
}

#[test]
fn no_other_probe_table_declares_cardid() {
    // The `cardid` row exists in exactly seven probes, and it is there to *reserve* the region: no
    // probe may put a partition of its own over [0x356000, 0x35A000), which is the one thing the
    // device rules never allow.
    let root = crate::util::workspace_root();
    for name in super::build::probe_names(&root).expect("the probes directory") {
        let path = root.join(format!("probes/{name}/partitions.csv"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (row, ptype, subtype, offset, size) in partition_rows(&text) {
            let end = offset + size;
            let overlaps = offset < 0x35_A000 && end > 0x35_6000;
            // `probe_wifi_assoc`, `probe_crypto` and the four silicon
            // campaign probes (specs/notes/silicon-campaign.md) also carry the Passport's own
            // table, because they run on the device.
            if matches!(
                name.as_str(),
                "probe_wifi_conn"
                    | "probe_wifi_assoc"
                    | "probe_crypto"
                    | "probe_campaign_radio"
                    | "probe_campaign_regs"
                    | "probe_campaign_reset"
                    | "probe_campaign_timing"
            ) && row == "cardid"
            {
                assert_eq!((ptype.as_str(), subtype.as_str()), ("data", "nvs"));
                assert_eq!((offset, size), (0x35_6000, 0x4000));
                continue;
            }
            assert!(
                !overlaps,
                "{name}: `{row}` [{offset:#x}, {end:#x}) reaches into cardid"
            );
        }
    }
}

#[test]
fn the_committed_manifest_is_exactly_what_render_writes() {
    // "never edited by hand": a header or field changed in the renderer but not re-pinned, or a
    // manifest edited by hand, shows up here on any host.
    let root = crate::util::workspace_root();
    let text =
        std::fs::read_to_string(root.join(super::MANIFEST)).expect("the manifest is committed");
    let manifest = Manifest::parse(&text).expect("the committed manifest parses");
    assert_eq!(manifest.render(), text);
}
