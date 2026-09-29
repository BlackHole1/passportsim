//! Milestone M2 tests: the second-stage bootloader. Names use the prefix `t<tier>_m2_` so
//! `xtask ci` can count them.
//!
//! Every test but `t2_m2_bootloader_phase_matches_oracle` is T1. The oracle comparisons read
//! the consoles `xtask oracle consoles` wrote below the data root, and the T2 test reads the phase
//! records `xtask oracle boot-trace` wrote. No test here starts QEMU.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;
use common::{console_bytes, image_machine};

// The QEMU oracle side of the comparisons, shared with m1.rs and m3.rs.
mod oracle;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use pemu_core::hostio::SerialStream;
use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::elf::ElfInfo;
use pemu_loader::esp_image::MergedImage;
use pemu_machine::config::{Assets, MachineConfig};
use pemu_machine::machine::Machine;
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};
use pemu_verify::normalize::{BootSelect, normalize};

/// The `pk` merged image and its app ELF.
const PK_IMAGE: &str = "FoloToy-AI-Passport-8MB.bin";
const PK_APP_ELF: &str = "FoloToy-AI-Passport.elf";
/// The `official` and `goldminer` merged images.
const OFFICIAL_IMAGE: &str = "FoloToy-AI-Passport-8MB.bin";
const GOLDMINER_IMAGE: &str = "goldminer-sanitized-8MB.bin";

/// Base of the DROM and IROM flash windows and the span one MMU table serves.
const DROM: u32 = 0x3C00_0000;
const IROM: u32 = 0x4200_0000;
const WINDOW: u32 = 0x0080_0000;
const PAGE: u32 = 0x1_0000;
/// Bytes of `esp_image_segment_header_t`: load address and length.
const SEGMENT_HEADER: u32 = 8;

/// One flash-window app segment: load address, length and the offset of its data in the app
/// image. `image_info`'s "File offs" is the offset of the 8-byte segment header, so the data
/// starts 8 bytes later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seg {
    load: u32,
    len: u32,
    file_off: u32,
}

/// Whether a path is a regular file, for the esptool resolution order.
struct HostFiles;

impl pemu_planner::rehearse::FileProbe for HostFiles {
    fn is_file(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }
}

/// The resolved esptool command: the product's own resolver
/// (`pemu_planner::rehearse::resolve_esptool`) over this host's sources. `Err` carries the
/// resolver's reason, which a test prints as its skipped leg.
///
/// No port, ever: only `image_info`, which reads a file, runs from here, its argument vector is
/// checked ([`no_port_argument`]), and `pemu-planner`'s `device` feature is off in this build.
fn esptool() -> Result<(PathBuf, Vec<String>), String> {
    let sources = pemu_host::device::esptool_sources(None);
    let os = if cfg!(target_os = "windows") {
        pemu_planner::rehearse::HostOs::Windows
    } else {
        pemu_planner::rehearse::HostOs::MacOs
    };
    match pemu_planner::rehearse::resolve_esptool(&sources, os, &HostFiles) {
        Ok(command) => Ok((PathBuf::from(command.program), command.prefix)),
        Err(err) => Err(match err {
            pemu_planner::rehearse::ResolveError::NotFound(why) => why,
            other => format!("{other:?}"),
        }),
    }
}

/// Panics when an esptool argument vector names a port: nothing runs esptool against the Passport
/// before a human confirms, so the check is on the vector about to be spawned.
#[track_caller]
fn no_port_argument(args: &[String]) {
    for arg in args {
        assert!(
            arg != "--port" && arg != "-p" && !arg.starts_with("--port="),
            "an `image_info` invocation must never name a port: {args:?}"
        );
    }
}

/// Every segment `image_info --version 2` reports for the app image `app`, in image order. It
/// reads the file written under `CARGO_TARGET_TMPDIR` and nothing else.
fn image_info_all_segments(tool: &(PathBuf, Vec<String>), app: &[u8], test: &str) -> Vec<Seg> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    std::fs::create_dir_all(&dir).expect("the test scratch directory");
    let file = dir.join("app.bin");
    std::fs::write(&file, app).expect("the app image copy");
    let mut args = tool.1.clone();
    args.extend(
        ["--chip", "esp32c3", "image_info", "--version", "2"]
            .iter()
            .map(|a| (*a).to_string()),
    );
    args.push(file.to_string_lossy().into_owned());
    no_port_argument(&args);
    let out = Command::new(&tool.0)
        .args(&args)
        .output()
        .expect("the resolved esptool command runs");
    let _ = std::fs::remove_file(&file);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{test}: image_info failed:\n{text}");
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok();
    text.lines()
        .skip_while(|l| !l.starts_with("Segment   Length"))
        .skip(2)
        .map_while(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            (cols.len() >= 4 && cols[0].parse::<u32>().is_ok()).then(|| Seg {
                len: hex(cols[1]).expect("a hex length"),
                load: hex(cols[2]).expect("a hex load address"),
                file_off: hex(cols[3]).expect("a hex file offset") + SEGMENT_HEADER,
            })
        })
        .collect()
}

fn image_info_segments(tool: &(PathBuf, Vec<String>), app: &[u8], test: &str) -> Vec<Seg> {
    image_info_all_segments(tool, app, test)
        .into_iter()
        .filter(|s| in_flash_window(s.load))
        .collect()
}

fn in_flash_window(addr: u32) -> bool {
    (DROM..DROM + WINDOW).contains(&addr) || (IROM..IROM + WINDOW).contains(&addr)
}

/// An observe hook on `call_start_cpu0` (from `pk.elf`) fires once, and at that instant the IROM
/// and DROM MMU entries match the app segment mapping `image_info` reports.
///
/// The run goes on to 1 s of virtual time, past the BLE init, so the observe count covers the
/// whole boot. Entry `(load & 0x7FFFFF) >> 16` must be
/// `(app offset + file offset + page delta) >> 16`, and the bytes the guest reads through the
/// window must equal the image's. With no esptool the segment table comes from
/// `pemu_loader::esp_image` and the esptool leg is `NOT_RUN`.
#[test]
fn t1_m2_mmu_handoff_matches_image_info() {
    let test = "t1_m2_mmu_handoff_matches_image_info";
    let Some(image_path) = common::corpus_file_or_skip(test, common::PK, PK_IMAGE) else {
        return;
    };
    let Some(elf_path) = common::corpus_file_or_skip(test, common::PK, PK_APP_ELF) else {
        return;
    };
    let bytes = std::fs::read(&image_path).expect("the verified corpus file is readable");
    let merged = MergedImage::parse(&bytes).expect("the pk image parses");
    let (part, app) = merged.app.expect("pk has a boot app");
    let app_off = part.offset;

    let loader_segs: Vec<Seg> = app
        .segments
        .iter()
        .map(|s| Seg {
            load: s.load_addr,
            len: s.len,
            file_off: (s.data_offset as u32) - app_off,
        })
        .filter(|s| in_flash_window(s.load))
        .collect();
    assert!(
        loader_segs.iter().any(|s| s.load >= IROM) && loader_segs.iter().any(|s| s.load < IROM),
        "{test}: pk maps both a DROM and an IROM segment: {loader_segs:?}"
    );
    let segs = match esptool() {
        Ok(tool) => {
            let end = app_off as usize + app.len;
            let reported = image_info_segments(&tool, &bytes[app_off as usize..end], test);
            assert_eq!(
                reported, loader_segs,
                "{test}: esptool image_info and the loader disagree on the flash segments"
            );
            reported
        }
        Err(why) => {
            // The row is partial rather than met, because the esptool half was not proved here.
            println!("NOT_RUN {test} esptool-leg: {why}; the loader's segment table stands in");
            loader_segs
        }
    };

    let elf = ElfInfo::parse(&std::fs::read(&elf_path).expect("the verified ELF is readable"))
        .expect("the pinned ELF parses");
    let entry = elf
        .symbols
        .addr_of("call_start_cpu0")
        .expect("pk.elf names call_start_cpu0");
    let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses");
    let assets = Assets::with_bundled_rom(flash, Some(Arc::new(elf)), None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    let mut m = Machine::new(MachineConfig::default(), assets).expect("the image fits");
    assert!(m.add_observe_hook(entry, "call_start_cpu0"), "{test}");

    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(1_000)),
        max_insns: None,
        stops: StopSet {
            breakpoints: vec![entry],
            ..StopSet::default()
        },
    });
    assert_eq!(out.reason, StopReason::Breakpoint(entry), "{test}");
    assert_eq!(m.hart().pc, entry, "{test}");

    for seg in &segs {
        // The page delta of the load address equals the page delta of the flash offset: that is what
        // makes a 64 KB mapping possible, and `esp_image_format` relies on it.
        let paddr = app_off + seg.file_off;
        assert_eq!(
            seg.load % PAGE,
            paddr % PAGE,
            "{test}: segment {seg:x?} is not page aligned with its flash offset"
        );
        let first_va = seg.load & !(PAGE - 1);
        let first_pa = paddr & !(PAGE - 1);
        let mut va = first_va;
        while va < seg.load + seg.len {
            let index = (va & (WINDOW - 1)) >> 16;
            let want = (first_pa + (va - first_va)) >> 16;
            assert_eq!(
                m.mmu_entry(index),
                want,
                "{test}: MMU entry {index} for {va:#010x} (segment {seg:x?})"
            );
            va += PAGE;
        }
        let mem = m.guest_mem();
        for probe in [seg.load, seg.load + seg.len - 4] {
            let off = (app_off + seg.file_off + (probe - seg.load)) as usize;
            let want = u32::from_le_bytes(bytes[off..off + 4].try_into().expect("4 bytes"));
            assert_eq!(
                mem.load(probe, 4),
                Some(want),
                "{test}: the page table maps {probe:#010x} to flash {off:#x}"
            );
        }
    }

    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(1_000)),
        max_insns: None,
        stops: StopSet::default(),
    });
    // The BLE HLE binds on `pk`, so the boot runs past the BLE init instead of stopping at the
    // tripwire.
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{test}: the pk boot runs its 1 s with BLE bound"
    );
    assert_eq!(
        m.observe_fires(entry).map(|(n, _)| n),
        Some(1),
        "{test}: the observe hook on call_start_cpu0 fires once"
    );
}

/// The last bootloader line, dev:L36, without its `I (<ms>) ` prefix.
const DISABLING_RNG: &str = "boot: Disabling RNG early entropy source";
/// dev:L35.
const LOADED_APP: &str = "boot: Loaded app from partition at offset";
/// The line the ROM prints last, where the bootloader phase begins (dev:L13).
const ENTRY_PREFIX: &str = "entry 0x";
/// The first line the app prints; an image the bootloader rejected must never reach it.
const APP_FIRST: &str = "cpu_start:";
/// The first of the lines the bootloader prints when an image checksum fails.
const CHECKSUM_FAILED: &str = "esp_image: Checksum failed.";
const NOT_BOOTABLE: &str = "boot: Factory app partition is not bootable";
const NO_BOOTABLE: &str = "boot: No bootable app partitions in the partition table";

/// Lines of the derived device golden claimed: dev:L4 to dev:L36. The text rule compares a
/// contiguous prefix, so the claim includes dev:L7, the `Saved PC:` line whose value the
/// normalizer masks.
const PK_BOOTLOADER_LINES: usize = 33;

/// Virtual time the device-like scenario `pk-boot-device` runs before the line reset.
const DEVICE_LIKE_PS: u64 = 300_000_000_000;

/// Instructions a boot gets to reach its line: far above what it uses (each `RAN` line prints it),
/// so a regression fails on a missing line rather than on a hang.
const BOOT_INSNS: u64 = 200_000_000;

const BOOT_LINE: MatcherId = MatcherId(0x21);

fn corpus_image(test: &str, id: &str, file: &str) -> Option<Vec<u8>> {
    let path = common::corpus_file_or_skip(test, id, file)?;
    Some(std::fs::read(&path).expect("the verified corpus file is readable"))
}

/// A console line stop on the USJ console, which is where this firmware prints.
fn line_stop(contains: &str) -> StopSet {
    StopSet {
        matchers: vec![(
            BOOT_LINE,
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern: LinePattern::Contains(contains.into()),
            },
        )],
        ..StopSet::default()
    }
}

fn tail(console: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(console);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The device-like line reset of scenario `pk-boot-device`: power on, run 300 ms of virtual time,
/// then apply `UsbLine {rts: 1, dtr: 0}`, the esptool hard reset that made the captured boot
/// `rst:0x15`.
fn device_like_reset(id: &str, m: &mut Machine) {
    let out = m.run(RunLimits {
        until: Some(VTime(DEVICE_LIKE_PS)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{id}: the first boot ends before 300 ms at pc {:#010x}",
        m.hart().pc
    );
    m.input(
        pemu_machine::machine::At::Now,
        pemu_core::input::InputEvent::UsbLine {
            dtr: false,
            rts: true,
        },
    )
    .expect("now is not in the past");
}

/// The normalized lines of a console, with `select` keeping the last boot or every boot.
fn normalized_lines(bytes: &[u8], select: BootSelect) -> Vec<String> {
    normalize(bytes, select)
        .lines
        .into_iter()
        .map(|line| line.text)
        .collect()
}

/// The bootloader phase of a normalized console: the lines after the first `entry 0x` up to and
/// including the first line that contains `last`. The first, because the flipped image resets in
/// a loop. `None` when either end is missing.
fn bootloader_phase(lines: &[String], last: &str) -> Option<Vec<String>> {
    let start = lines.iter().position(|l| l.starts_with(ENTRY_PREFIX))? + 1;
    let end = lines[start..].iter().position(|l| l.contains(last))? + start;
    Some(lines[start..=end].to_vec())
}

/// Compares our bootloader-phase lines with the oracle's, excusing an index a `console` entry of
/// `specs/oracle-known-diffs.toml` lists. A failure names the index and lengths, never oracle text.
fn assert_equals_oracle(what: &str, ours: &[String], theirs: &[String]) {
    let known = oracle::known_diffs();
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{what}: {} lines here, {} in the oracle console",
        ours.len(),
        theirs.len()
    );
    for (at, (a, b)) in ours.iter().zip(theirs).enumerate() {
        if a == b {
            continue;
        }
        if let Some(entry) = known.suppresses_console(oracle::QEMU, b) {
            println!("{what}: line {at} excused by `{}`", entry.id);
            continue;
        }
        panic!(
            "{what}: line {at} differs from the oracle console ({} bytes here, {} there)",
            a.len(),
            b.len()
        );
    }
}

/// One `esp_image: segment <n>: paddr=<p> vaddr=<v> size=<s>h (...) map|load` line of the
/// bootloader, as the three numbers it reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ConsoleSeg {
    paddr: u32,
    vaddr: u32,
    size: u32,
}

fn console_segments(lines: &[String]) -> Vec<ConsoleSeg> {
    let field = |line: &str, name: &str| -> Option<u32> {
        let rest = line.split_once(name)?.1;
        let value: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        u32::from_str_radix(&value, 16).ok()
    };
    lines
        .iter()
        .filter(|l| l.contains("esp_image: segment "))
        .filter_map(|l| {
            Some(ConsoleSeg {
                paddr: field(l, "paddr=")?,
                vaddr: field(l, "vaddr=")?,
                size: field(l, "size=")?,
            })
        })
        .collect()
}

fn app_partition(image: &[u8]) -> (u32, Vec<u8>) {
    let merged = MergedImage::parse(image).expect("a corpus image parses");
    let (part, app) = merged.app.expect("the image has a boot app");
    let off = part.offset as usize;
    (part.offset, image[off..off + app.len].to_vec())
}

/// Asserts that the segment lines the bootloader printed are the segments `image_info` reports
/// for the same app partition, and returns how many were compared. The bootloader's `paddr` is
/// `app offset + File offs + 8`, its `vaddr` the `Load addr` and its `size` the `Length`.
fn assert_segments_match_image_info(
    id: &str,
    test: &str,
    tool: &(PathBuf, Vec<String>),
    image: &[u8],
    printed: &[ConsoleSeg],
) -> usize {
    let (app_off, app) = app_partition(image);
    let reported: Vec<ConsoleSeg> = image_info_all_segments(tool, &app, test)
        .into_iter()
        .map(|s| ConsoleSeg {
            paddr: app_off + s.file_off,
            vaddr: s.load,
            size: s.len,
        })
        .collect();
    assert!(!reported.is_empty(), "{id}: image_info reported no segment");
    assert_eq!(
        printed, reported,
        "{id}: the bootloader's segment lines and `image_info` disagree"
    );
    reported.len()
}

/// The normalized last boot of scenario `pk-boot-device` equals dev:L4 to dev:L36 of the derived
/// device golden: the bootloader banner, chip and eFuse revisions, SPI and flash settings, the
/// partition table, the segment lines, `Loaded app` and `Disabling RNG early entropy source...`.
///
/// Timestamps become `(T)` and the `Saved PC:` value and the bootloader compile time are masked;
/// timestamp bands are an M11 check. The golden is derived on this host and never committed.
#[test]
fn t1_m2_pk_bootloader_phase_console() {
    let test = "t1_m2_pk_bootloader_phase_console";
    let id = test.to_string();
    let Some(image) = corpus_image(test, common::PK, PK_IMAGE) else {
        return;
    };
    let Some(golden) = common::derived_golden_or_skip(test, "pk.console.txt") else {
        return;
    };
    let mut m = image_machine(&image);
    device_like_reset(&id, &mut m);
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BOOT_INSNS),
        stops: line_stop(DISABLING_RNG),
    });
    let console = console_bytes(&mut m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: the reset boot never printed `{DISABLING_RNG}`; tail:\n{}",
        tail(&console, 8)
    );
    let compared = common::assert_console_prefix(
        "pk.console.txt",
        &golden,
        &console,
        Some(PK_BOOTLOADER_LINES),
    );
    assert_eq!(compared, PK_BOOTLOADER_LINES, "{id}");
    println!(
        "RAN {test} pk-boot-device: {compared} lines (dev:L4-L36), {} instructions",
        out.insns
    );
}

/// `official`'s bootloader-phase text equals the QEMU oracle console, and its segment lines equal
/// `image_info` of its app partition.
///
/// Both sides are normalized alike and cut after `entry 0x` through `Disabling RNG early entropy
/// source...`. A line the oracle cannot reproduce is excused only by a listed `console` entry of
/// `specs/oracle-known-diffs.toml`, never by a mask.
#[test]
fn t1_m2_official_bootloader_phase() {
    let test = "t1_m2_official_bootloader_phase";
    let id = test.to_string();
    let Some(oracle_path) = oracle::oracle_file_or_skip(test, "official.usj.console") else {
        return;
    };
    let Some(image) = corpus_image(test, common::OFFICIAL, OFFICIAL_IMAGE) else {
        return;
    };
    let tool = match esptool() {
        Ok(tool) => tool,
        Err(why) => {
            common::skip(test, &format!("the resolved esptool command: {why}"));
            return;
        }
    };

    let mut m = image_machine(&image);
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BOOT_INSNS),
        stops: line_stop(DISABLING_RNG),
    });
    let console = console_bytes(&mut m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: `official` never printed `{DISABLING_RNG}`; tail:\n{}",
        tail(&console, 8)
    );
    let ours = bootloader_phase(
        &normalized_lines(&console, BootSelect::AllBoots),
        DISABLING_RNG,
    )
    .expect("our console holds the bootloader phase the run stopped at the end of");
    let oracle_bytes = std::fs::read(&oracle_path).expect("the oracle console is readable");
    let theirs = bootloader_phase(
        &normalized_lines(&oracle_bytes, BootSelect::AllBoots),
        DISABLING_RNG,
    )
    .expect("the oracle console holds the bootloader phase");
    assert_equals_oracle(&format!("{id} `official` bootloader phase"), &ours, &theirs);

    let segments =
        assert_segments_match_image_info(&id, test, &tool, &image, &console_segments(&ours));
    println!(
        "RAN {test} official: {} bootloader lines equal the oracle, {segments} segment lines \
         equal image_info, {} instructions",
        ours.len(),
        out.insns
    );
}

/// `goldminer` prints `Loaded app`, and its segment lines are the segments `image_info` reports
/// for its own app partition.
#[test]
fn t1_m2_goldminer_loaded_app_segments() {
    let test = "t1_m2_goldminer_loaded_app_segments";
    let id = test.to_string();
    let Some(image) = corpus_image(test, common::GOLDMINER, GOLDMINER_IMAGE) else {
        return;
    };
    let tool = match esptool() {
        Ok(tool) => tool,
        Err(why) => {
            common::skip(test, &format!("the resolved esptool command: {why}"));
            return;
        }
    };
    let mut m = image_machine(&image);
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BOOT_INSNS),
        stops: line_stop(LOADED_APP),
    });
    let console = console_bytes(&mut m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: `goldminer` never printed `{LOADED_APP}`; tail:\n{}",
        tail(&console, 8)
    );
    let lines = normalized_lines(&console, BootSelect::AllBoots);
    let phase =
        bootloader_phase(&lines, LOADED_APP).expect("the bootloader phase is on the console");
    let segments =
        assert_segments_match_image_info(&id, test, &tool, &image, &console_segments(&phase));
    println!(
        "RAN {test} goldminer: `Loaded app` printed, {segments} segment lines equal image_info, \
         {} instructions",
        out.insns
    );
}

/// The byte the negative test inverts, inside the app image rather than its header, so the
/// bootloader refuses on the image check. `xtask oracle consoles` flips the same byte
/// (`xtask/src/oracle.rs`, `FLIP_OFFSET`).
const FLIP_OFFSET: usize = 0x0002_0000;

/// The SHA negative: a copy of `pk` with one byte flipped inside the app image makes the
/// bootloader reject it, with text equal to the QEMU oracle's for the same copy, and the run never
/// jumps to the app.
///
/// The copy lives under the artifacts directory, never in the repository or beside the corpus, and
/// is removed at the end. The image resets in a loop, so the compared text is the first boot,
/// after `entry 0x` through `No bootable app partitions in the partition table`.
#[test]
fn t1_m2_flipped_app_is_rejected() {
    let test = "t1_m2_flipped_app_is_rejected";
    let id = test.to_string();
    let Some(oracle_path) = oracle::oracle_file_or_skip(test, "pk-e2.4-flip.usj.console") else {
        return;
    };
    let Some(image) = corpus_image(test, common::PK, PK_IMAGE) else {
        return;
    };
    let root = match pemu_testkit::corpus::data_root_from_env() {
        Ok(root) => root,
        Err(err) => {
            common::skip(
                test,
                &format!("the flipped-image copy needs a data root: {err}"),
            );
            return;
        }
    };

    let mut flipped = image.clone();
    assert!(
        flipped.len() > FLIP_OFFSET,
        "{id}: `pk` is shorter than the flip offset {FLIP_OFFSET:#x}"
    );
    flipped[FLIP_OFFSET] ^= 0xFF;
    let dir = root.join("artifacts").join(test);
    std::fs::create_dir_all(&dir).expect("the artifacts directory of this test");
    assert!(
        !dir.starts_with(pemu_testkit::golden::repo_root()),
        "{id}: the flipped copy must never be written inside the repository"
    );
    let copy = dir.join("pk-e2.4-flip.bin");
    std::fs::write(&copy, &flipped).expect("the flipped copy is written under artifacts");
    let oracle_copy = root
        .join("artifacts")
        .join("oracle")
        .join("pk-e2.4-flip.bin");
    if oracle_copy.is_file() {
        let theirs = std::fs::read(&oracle_copy).expect("the oracle's copy is readable");
        assert_eq!(
            theirs.len(),
            flipped.len(),
            "{id}: the oracle ran a copy of another size"
        );
        assert!(
            theirs == flipped,
            "{id}: the oracle's copy is not the one this test makes"
        );
    } else {
        println!("RAN {test} same-copy-check: the oracle's copy is no longer under artifacts");
    }

    let mut m = image_machine(&flipped);
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BOOT_INSNS),
        stops: line_stop(NO_BOOTABLE),
    });
    let console = console_bytes(&mut m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: the flipped image never printed `{NO_BOOTABLE}`; tail:\n{}",
        tail(&console, 8)
    );
    let ours = bootloader_phase(
        &normalized_lines(&console, BootSelect::AllBoots),
        NO_BOOTABLE,
    )
    .expect("our console holds the refusal the run stopped on");
    assert!(
        ours.iter().any(|l| l.contains(CHECKSUM_FAILED)),
        "{id}: the bootloader refused without the image check line"
    );
    assert!(
        ours.iter().any(|l| l.contains(NOT_BOOTABLE)),
        "{id}: the bootloader refused without the factory-partition line"
    );
    let oracle_bytes = std::fs::read(&oracle_path).expect("the oracle console is readable");
    let theirs = bootloader_phase(
        &normalized_lines(&oracle_bytes, BootSelect::AllBoots),
        NO_BOOTABLE,
    )
    .expect("the oracle console holds the refusal");
    assert_equals_oracle(&format!("{id} `pk` flipped, first boot"), &ours, &theirs);

    // It resets instead, and the reset boot refuses again: the run reaches a second `entry 0x`, and
    // no app line is ever printed.
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BOOT_INSNS),
        stops: line_stop(CHECKSUM_FAILED),
    });
    let console = console_bytes(&mut m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: the refused image did not come round to a second refusal; tail:\n{}",
        tail(&console, 8)
    );
    let all = normalized_lines(&console, BootSelect::AllBoots);
    assert!(
        !all.iter().any(|l| l.contains(APP_FIRST)),
        "{id}: the run reached the app although the bootloader refused the image"
    );
    let boots = all.iter().filter(|l| l.starts_with("ESP-ROM:")).count();
    assert!(
        boots >= 2,
        "{id}: the refusal did not reset the chip ({boots} boot banners)"
    );
    let _ = std::fs::remove_file(&copy);
    let _ = std::fs::remove_dir(&dir);
    println!(
        "RAN {test} pk-e2.4-flip: {} lines equal the oracle, {boots} boot banners, no app line",
        ours.len()
    );
}

/// Informational: the instructions retired from power-on to `Loaded app` (dev:L35) for `pk` and
/// `official`, recorded on the `RAN` line. It sets no budget.
#[test]
fn t1_m2_insns_from_reset_to_loaded_app() {
    let test = "t1_m2_insns_from_reset_to_loaded_app";
    let id = test.to_string();
    let mut recorded = 0;
    for (corpus_id, file) in [(common::PK, PK_IMAGE), (common::OFFICIAL, OFFICIAL_IMAGE)] {
        let Some(image) = corpus_image(test, corpus_id, file) else {
            continue;
        };
        let mut m = image_machine(&image);
        let out = m.run(RunLimits {
            until: None,
            max_insns: Some(BOOT_INSNS),
            stops: line_stop(LOADED_APP),
        });
        let console = console_bytes(&mut m);
        assert_eq!(
            out.reason,
            StopReason::Matcher(BOOT_LINE),
            "{id}: `{corpus_id}` never printed `{LOADED_APP}`; tail:\n{}",
            tail(&console, 8)
        );
        recorded += 1;
        println!(
            "RAN {test} {corpus_id}: {} instructions from reset to `Loaded app`, {} us virtual",
            out.insns,
            m.now().0 / 1_000_000
        );
    }
    if recorded == 0 {
        println!("NOT_RUN {test} both-images: neither `pk` nor `official` is on this host");
    }
}

/// The images compared: the corpus ids with a bootloader ELF, with the merged image and the app
/// ELF whose `call_start_cpu0` ends the phase. `pk` is required; `official` is a second leg.
const ORACLE_IMAGES: [(&str, &str, &str); 2] = [
    (common::PK, PK_IMAGE, PK_APP_ELF),
    (common::OFFICIAL, OFFICIAL_IMAGE, PK_APP_ELF),
];

const ORACLE_INSNS: u64 = BOOT_INSNS;

/// One leg: our bootloader phase of `id` against the oracle record `record`, or why the leg cannot
/// run (a reason for a `SKIP` or `NOT_RUN` line).
fn oracle_leg(
    test: &str,
    id: &str,
    image: &str,
    app: &str,
    record: &Path,
) -> Result<pemu_verify::phase::BootPhaseReport, String> {
    let located = pemu_testkit::corpus::locate_id(id);
    if let Some(failure) = located.failure_reason() {
        panic!("{test}: {failure}");
    }
    if let Some(reason) = located.skip_reason() {
        return Err(reason);
    }
    let file = |name: &str| {
        located
            .file(name)
            .map(|f| f.path.clone())
            .ok_or_else(|| format!("corpus id `{id}` has no file `{name}`"))
    };
    let read = |path: PathBuf| std::fs::read(path).expect("a verified corpus file is readable");
    let boot = ElfInfo::parse(&read(file("bootloader.elf")?)).expect("the pinned ELF parses");
    let app = ElfInfo::parse(&read(file(app)?)).expect("the pinned ELF parses");
    let flash = FlashImage::from_merged(&read(file(image)?)).expect("a corpus image parses");
    let theirs = pemu_verify::phase::PhaseRecord::parse(
        oracle::QEMU,
        &std::fs::read_to_string(record).expect("the oracle record is readable"),
    )
    .unwrap_or_else(|e| panic!("{test}: `{id}` oracle record: {e}"));

    let (watch, end) = pemu_verify::phase::boot_watch(&boot, &app)
        .unwrap_or_else(|e| panic!("{test}: `{id}`: {e}"));
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    let cfg = MachineConfig {
        trace: pemu_testkit::oracle_run::phase_trace(),
        ..MachineConfig::default()
    };
    let mut m = Machine::new(cfg, assets).expect("the image fits the 8 MB flash");
    let ours = pemu_testkit::oracle_run::record_phase(&mut m, &watch, end, ORACLE_INSNS)
        .unwrap_or_else(|e| panic!("{test}: `{id}` never reaches the app entry: {e}"));
    let symbolize = |pc: u32| boot.symbols.func_at(pc).map(|s| s.name.clone());
    Ok(pemu_verify::phase::diff_boot_phase(
        &ours,
        &theirs,
        &oracle::regions(),
        &oracle::known_diffs(),
        oracle::QEMU,
        &symbolize,
    )
    .unwrap_or_else(|e| panic!("{test}: `{id}`: {e}")))
}

/// Over the bootloader phase (the bootloader's `call_start_cpu0` to the app's), the per-block write
/// streams of SHA, TIMG, EXTMEM, MMU, SPI1, eFuse and RTC_CNTL and the order of entries into every
/// bootloader function equal the QEMU oracle's, or differ only as
/// `specs/oracle-known-diffs.toml` lists. The eFuse leg is its read-offset sequence.
///
/// The oracle side is `<data root>/oracles/phase/<id>.boot.qemu` from `cargo xtask oracle
/// boot-trace`; the T2 `oracle-diffs` step runs the same comparison through `xtask oracle diff`.
#[test]
fn t2_m2_bootloader_phase_matches_oracle() {
    let test = "t2_m2_bootloader_phase_matches_oracle";
    let id = test.to_string();
    let root = match pemu_testkit::corpus::data_root_from_env() {
        Ok(root) => root,
        Err(e) => {
            common::skip(test, &format!("oracle phase records: {e}"));
            return;
        }
    };
    let mut failures = String::new();
    for (corpus_id, image, app) in ORACLE_IMAGES {
        let record = root
            .join("oracles/phase")
            .join(format!("{corpus_id}.boot.qemu"));
        let leg = if record.is_file() {
            oracle_leg(test, corpus_id, image, app, &record)
        } else {
            Err(format!(
                "the oracle record `{corpus_id}.boot.qemu` is absent below the data root; \
                 `cargo xtask oracle boot-trace` records it"
            ))
        };
        match leg {
            Ok(report) => {
                print!("{}", report.render(&format!("{id} `{corpus_id}`")));
                if !report.is_clean() {
                    failures.push_str(&report.render(&format!("{id} `{corpus_id}`")));
                }
            }
            Err(reason) if corpus_id == common::PK => {
                common::skip(test, &reason);
                return;
            }
            Err(reason) => println!("NOT_RUN {test} {corpus_id}: {reason}"),
        }
    }
    assert!(
        failures.is_empty(),
        "{id}: unexplained divergences:\n{failures}"
    );
}
