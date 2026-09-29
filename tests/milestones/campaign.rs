//! The silicon evidence campaign (`specs/notes/silicon-campaign.md`): the campaign probes run in
//! the emulator, their committed records `tests/fw/campaign/<probe>.emu.txt`, and the device
//! captures below the data root's `captures/`.
//!
//! Tests are named `t<tier>_campaign_...`, with the note's step in the name where one step owns
//! the test; the `#[ignore]`d ones print rather than assert and run by hand.

#[allow(dead_code)]
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pemu_core::clock::{CacheVariant, TimingProfile};
use pemu_core::fidelity::TouchAccess;
use pemu_core::hostio::SerialStream;
use pemu_core::input::{ButtonId, InputEvent};
use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::elf::ElfInfo;
use pemu_machine::config::{Assets, MachineConfig};
use pemu_machine::machine::{At, Machine};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};
use pemu_soc_c3::periph::BLOCKS;

/// The data root's `corpus/` directory, found through the pinned `pk` image, or `None` after a
/// printed skip.
fn corpus_dir(test: &str) -> Option<PathBuf> {
    let pk = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")?;
    Some(
        pk.ancestors()
            .nth(2)
            .expect("a corpus file sits under corpus/<id>/")
            .to_path_buf(),
    )
}

fn machine_with(flash: &[u8], elf: Option<&[u8]>, cfg: MachineConfig) -> Machine {
    let flash = FlashImage::from_merged(flash).expect("a corpus image parses");
    let elf = elf.map(|e| Arc::new(ElfInfo::parse(e).expect("the ELF parses")));
    let assets = Assets::with_bundled_rom(flash, elf, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    Machine::new(cfg, assets).expect("the image fits")
}

fn machine(flash: &[u8], elf: Option<&[u8]>) -> Machine {
    machine_with(flash, elf, MachineConfig::default())
}

const CAMPAIGN_BUDGET_MS: u64 = 60_000;

const PRESS_NOTE: &str = "NOTE|press Up";

/// The presses scripted into a campaign run when [`PRESS_NOTE`] appears, as made on the device:
/// Up, then Down, then OK, each held 300 ms, from 1 s after the note.
const PRESSES: [(u64, ButtonId); 3] = [
    (1_000, ButtonId::Up),
    (2_500, ButtonId::Down),
    (4_000, ButtonId::Ok),
];

/// Runs a campaign probe from power-on under the `device` timing profile until its final `DONE|`
/// line or [`CAMPAIGN_BUDGET_MS`]; returns why it stopped and the whole USJ console. A guest panic
/// or reset is run through, and [`PRESSES`] are scripted when the probe asks for them.
fn campaign_console(flash: &[u8], elf: Option<&[u8]>) -> (StopReason, String) {
    campaign_console_with(
        flash,
        elf,
        MachineConfig {
            profile: pemu_machine::config::TimingProfileId::Device,
            ..MachineConfig::default()
        },
    )
}

fn campaign_console_with(
    flash: &[u8],
    elf: Option<&[u8]>,
    cfg: MachineConfig,
) -> (StopReason, String) {
    let mut m = machine_with(flash, elf, cfg);
    let done = MatcherId(0xCA);
    let press = MatcherId(0xCB);
    let serial = |prefix: &str| Matcher::Serial {
        stream: SerialStream::UsjTx,
        pattern: LinePattern::Prefix(prefix.into()),
    };
    let stops = StopSet {
        matchers: vec![(done, serial("DONE|")), (press, serial(PRESS_NOTE))],
        ..StopSet::default()
    };
    let until = VTime::from_ms(CAMPAIGN_BUDGET_MS);
    loop {
        let reason = m
            .run(RunLimits {
                until: Some(until),
                max_insns: None,
                stops: stops.clone(),
            })
            .reason;
        if m.now() >= until {
            return (reason, console(&mut m));
        }
        match reason {
            StopReason::GuestPanic(_) => {}
            StopReason::Matcher(id) if id == press => {
                let now_ms = m.now().as_us() / 1000;
                for (after, id) in PRESSES {
                    for (at, down) in [(now_ms + after, true), (now_ms + after + 300, false)] {
                        m.input(At::Vt(VTime::from_ms(at)), InputEvent::Button { id, down })
                            .expect("a future button input is journaled");
                    }
                }
            }
            _ => return (reason, console(&mut m)),
        }
    }
}

fn console(m: &mut Machine) -> String {
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn block_name(id: pemu_core::sched::PeriphId) -> &'static str {
    BLOCKS
        .get(usize::from(id.0))
        .map_or("<not a block>", |b| b.name)
}

fn register_name(block: &str, off: u32) -> String {
    match pemu_machine::hang::register_at(block, off) {
        Some(reg) => reg.name.to_string(),
        None => format!("{:#06x}", off & !3),
    }
}

const BUDGET_MS: u64 = 15_000;

fn run_for(m: &mut Machine, until_ms: u64, probe: bool) -> StopReason {
    let stop = MatcherId(0xCA);
    let stops = if probe {
        StopSet {
            matchers: vec![(
                stop,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Prefix("DONE|".into()),
                },
            )],
            ..StopSet::default()
        }
    } else {
        StopSet::default()
    };
    // The probes that panic on purpose reboot from the panic handler's entry, so the run goes on
    // until the budget or the matcher ends it.
    loop {
        let reason = m
            .run(RunLimits {
                until: Some(VTime::from_ms(until_ms)),
                max_insns: None,
                stops: stops.clone(),
            })
            .reason;
        if !matches!(reason, StopReason::GuestPanic(_)) || m.now() >= VTime::from_ms(until_ms) {
            return reason;
        }
    }
}

struct Image {
    name: String,
    flash: PathBuf,
    elf: Option<PathBuf>,
    /// A product image, which gets the button walk after its boot.
    product: bool,
}

fn images(corpus: &Path) -> Vec<Image> {
    let mut out = Vec::new();
    let fixed: [(&str, &str, Option<&str>, bool); 8] = [
        (
            "pk",
            "pk/FoloToy-AI-Passport-8MB.bin",
            Some("pk/FoloToy-AI-Passport.elf"),
            true,
        ),
        (
            "official",
            "official/FoloToy-AI-Passport-8MB.bin",
            Some("official/FoloToy-AI-Passport.elf"),
            true,
        ),
        (
            "demo",
            "demo/demo-merged.bin",
            Some("demo/FoloToy-AI-Passport.elf"),
            true,
        ),
        (
            "goldminer",
            "goldminer/goldminer-sanitized-8MB.bin",
            None,
            true,
        ),
        ("probe-long", "probe-long/probe-long-8MB.bin", None, false),
        (
            "probe2",
            "probe2/probe2-merged.bin",
            Some("probe2/radio_heapprobe.elf"),
            false,
        ),
        (
            "pkgatt",
            "pkgatt/merged-binary.bin",
            Some("pkgatt/radio_pkgatt.elf"),
            false,
        ),
        (
            "scan3",
            "scan3/merged-binary.bin",
            Some("scan3/radio_scan3probe.elf"),
            false,
        ),
    ];
    for (name, flash, elf, product) in fixed {
        out.push(Image {
            name: name.to_string(),
            flash: corpus.join(flash),
            elf: elf.map(|e| corpus.join(e)),
            product,
        });
    }
    let mut probes: Vec<String> = std::fs::read_dir(corpus.join("probes"))
        .map(|d| {
            d.filter_map(Result::ok)
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|n| n.strip_suffix("-8MB.bin"))
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    probes.sort();
    for name in probes {
        let elf = corpus.join(format!("probes/build/{name}/{name}.elf"));
        out.push(Image {
            flash: corpus.join(format!("probes/{name}-8MB.bin")),
            elf: elf.is_file().then_some(elf),
            name: format!("probes/{name}"),
            product: false,
        });
    }
    out
}

/// Prints the first-touch ledger of every corpus and probe image: one
/// `TOUCH|<image>|<block>|<register>|<offset>|<access>` line per register the run reached.
///
/// Each run is [`BUDGET_MS`] from power-on under `fast`, HLE bound. A product image also gets the
/// Audio demo button script, so the I2S, GDMA and codec paths are in its ledger; a probe stops at
/// `DONE|`. `CAMPAIGN_IMAGES`, a comma-separated list of image names, limits the run.
#[test]
#[ignore = "prints the campaign's first-touch ledger rather than asserting; run with --ignored --nocapture"]
fn campaign_first_touch_ledger() {
    let test = "campaign_first_touch_ledger";
    let Some(corpus) = corpus_dir(test) else {
        return;
    };
    let only: Option<Vec<String>> = std::env::var("CAMPAIGN_IMAGES")
        .ok()
        .map(|v| v.split(',').map(str::to_string).collect());
    for image in images(&corpus) {
        if only.as_ref().is_some_and(|o| !o.contains(&image.name)) {
            continue;
        }
        let Ok(flash) = std::fs::read(&image.flash) else {
            println!("IMAGE|{}|missing={}", image.name, image.flash.display());
            continue;
        };
        let elf = image.elf.as_ref().and_then(|p| std::fs::read(p).ok());
        let mut m = machine(&flash, elf.as_deref());
        if image.product {
            // DOWN, DOWN and OK open `official`'s Audio demo, OK plays the tone and UP records and plays
            // back (tests/milestones/m6.rs).
            for (ms, id) in [
                (1_500, ButtonId::Down),
                (2_000, ButtonId::Down),
                (2_500, ButtonId::Ok),
                (3_000, ButtonId::Ok),
                (4_500, ButtonId::Up),
            ] {
                for (at, down) in [(ms, true), (ms + 80, false)] {
                    m.input(At::Vt(VTime::from_ms(at)), InputEvent::Button { id, down })
                        .expect("a future button input is journaled");
                }
            }
        }
        let reason = run_for(&mut m, BUDGET_MS, !image.product);
        let text = console(&mut m);
        let lines = text.lines().count();
        if std::env::var_os("CAMPAIGN_CONSOLE").is_some() {
            for l in text.lines() {
                println!("CONSOLE|{}|{}", image.name, l.trim_end());
            }
        }
        println!(
            "IMAGE|{}|elf={}|stop={reason:?}|vt_ms={}|console_lines={lines}|touches={}",
            image.name,
            image.elf.is_some() && elf.is_some(),
            m.now().as_us() / 1000,
            m.ledger().first_touches().len()
        );
        let mut rows: BTreeMap<(String, u32), String> = BTreeMap::new();
        for t in m.ledger().first_touches() {
            let block = block_name(t.periph);
            let access = match t.access {
                TouchAccess::Read => "R",
                TouchAccess::Write => "W",
            };
            rows.entry((block.to_string(), t.off & !3))
                .or_insert_with(|| access.to_string());
        }
        for ((block, off), access) in rows {
            println!(
                "TOUCH|{}|{block}|{}|{off:#05x}|{access}",
                image.name,
                register_name(&block, off)
            );
        }
    }
}

/// Runs one image through [`campaign_console`] and prints its console: the development loop of a
/// campaign probe before `cargo xtask probes` pins it. `CAMPAIGN_RUN_IMAGE` names the merged
/// image and `CAMPAIGN_RUN_ELF`, optionally, its app ELF.
#[test]
#[ignore = "runs an image named by CAMPAIGN_RUN_IMAGE and prints its console; run with --ignored --nocapture"]
fn campaign_run_image() {
    let Some(path) = std::env::var_os("CAMPAIGN_RUN_IMAGE") else {
        println!("SKIP campaign_run_image: CAMPAIGN_RUN_IMAGE is not set");
        return;
    };
    let flash = std::fs::read(&path).expect("CAMPAIGN_RUN_IMAGE is readable");
    let elf = std::env::var_os("CAMPAIGN_RUN_ELF")
        .map(|p| std::fs::read(p).expect("CAMPAIGN_RUN_ELF is readable"));
    let (reason, text) = campaign_console(&flash, elf.as_deref());
    print!("{text}");
    println!("STOP {reason:?}");
}

/// Runs one image as [`campaign_run_image`] does and prints the virtual time of every pass
/// through the pcs `CAMPAIGN_RUN_BREAKS` names (`label=0xaddr,...`), then the probe lines. A label
/// passed more than 20 times prints its first 20 passes and a count.
#[test]
#[ignore = "runs an image named by CAMPAIGN_RUN_IMAGE with breakpoints from CAMPAIGN_RUN_BREAKS"]
fn campaign_time_points() {
    let (Some(path), Some(breaks)) = (
        std::env::var_os("CAMPAIGN_RUN_IMAGE"),
        std::env::var("CAMPAIGN_RUN_BREAKS").ok(),
    ) else {
        println!("SKIP campaign_time_points: CAMPAIGN_RUN_IMAGE or CAMPAIGN_RUN_BREAKS is not set");
        return;
    };
    let flash = std::fs::read(&path).expect("CAMPAIGN_RUN_IMAGE is readable");
    let elf = std::env::var_os("CAMPAIGN_RUN_ELF")
        .map(|p| std::fs::read(p).expect("CAMPAIGN_RUN_ELF is readable"));
    let points: Vec<(String, u32)> = breaks
        .split(',')
        .filter_map(|kv| {
            let (label, addr) = kv.split_once('=')?;
            let addr = u32::from_str_radix(addr.trim().trim_start_matches("0x"), 16).ok()?;
            Some((label.trim().to_string(), addr))
        })
        .collect();
    let mut m = machine_with(
        &flash,
        elf.as_deref(),
        MachineConfig {
            profile: pemu_machine::config::TimingProfileId::Device,
            ..MachineConfig::default()
        },
    );
    let done = MatcherId(0xCA);
    let stops = StopSet {
        breakpoints: points.iter().map(|(_, a)| *a).collect(),
        matchers: vec![(
            done,
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern: LinePattern::Prefix("DONE|".into()),
            },
        )],
        ..StopSet::default()
    };
    let until = VTime::from_ms(CAMPAIGN_BUDGET_MS);
    let mut hits: BTreeMap<u32, u32> = BTreeMap::new();
    let reason = loop {
        let reason = m
            .run(RunLimits {
                until: Some(until),
                max_insns: None,
                stops: stops.clone(),
            })
            .reason;
        if m.now() >= until {
            break reason;
        }
        match reason {
            StopReason::Breakpoint(pc) => {
                let n = hits.entry(pc).or_default();
                *n += 1;
                if *n <= 20 {
                    let label = points
                        .iter()
                        .find(|(_, a)| *a == pc)
                        .map_or("?", |(l, _)| l.as_str());
                    println!("AT {label} pc={pc:#010x} vt_ps={}", m.now().0);
                }
            }
            StopReason::GuestPanic(_) => {}
            _ => break reason,
        }
    };
    for (pc, n) in hits {
        println!("HITS pc={pc:#010x} n={n}");
    }
    print!("{}", probe_lines(&console(&mut m)));
    println!("STOP {reason:?}");
}

const CAMPAIGN_PROBES: [&str; 4] = [
    "probe_campaign_radio",
    "probe_campaign_regs",
    "probe_campaign_reset",
    "probe_campaign_timing",
];

/// The quoted value of `key` in the `[[probe]]` table of `tests/fw/manifest.toml` whose `name` is
/// `probe`. The manifest is one `key = value` per line, so a line scan reads it.
fn manifest_field(manifest: &str, probe: &str, key: &str) -> Option<String> {
    let name = format!("name = \"{probe}\"");
    let mut inside = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line == "[[probe]]" {
            inside = false;
        } else if line == name {
            inside = true;
        } else if inside
            && let Some(value) = line.strip_prefix(key).and_then(|r| r.strip_prefix(" = \""))
        {
            return value.strip_suffix('"').map(str::to_string);
        }
    }
    None
}

use pemu_testkit::corpus::sha256_hex;

/// The probe lines of a console (`TAG|...`, `probes/common/probe_line.h`), one per line with a
/// final newline, without the ROM and ESP-IDF log lines around them.
fn probe_lines(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some((tag, _)) = line.split_once('|') else {
            continue;
        };
        let is_tag = tag.starts_with(|c: char| c.is_ascii_uppercase())
            && tag
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if is_tag {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Each campaign probe, built as `tests/fw/manifest.toml` pins it and run under the `device`
/// profile, prints exactly the probe lines recorded in `tests/fw/campaign/<probe>.emu.txt`, the
/// emulator side of `cargo xtask probes compare`. A model change that moves a campaign fact shows
/// up here (`CAMPAIGN_RECORD=1` writes the files instead of comparing).
///
/// An absent probe image is skipped; one that is present but not the pinned build fails.
#[test]
fn t1_campaign_probes_print_the_recorded_emulator_lines() {
    let test = "t1_campaign_probes_print_the_recorded_emulator_lines";
    let Some(corpus) = corpus_dir(test) else {
        return;
    };
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = std::fs::read_to_string(repo.join("tests/fw/manifest.toml"))
        .expect("the probe manifest is committed");
    let record = std::env::var_os("CAMPAIGN_RECORD").is_some();
    for name in CAMPAIGN_PROBES {
        let Ok(flash) = std::fs::read(corpus.join(format!("probes/{name}-8MB.bin"))) else {
            common::skip(test, &format!("no probe image {name}-8MB.bin"));
            continue;
        };
        let pinned = |key: &str| {
            manifest_field(&manifest, name, key)
                .unwrap_or_else(|| panic!("tests/fw/manifest.toml has no {key} for {name}"))
        };
        assert_eq!(
            sha256_hex(&flash),
            pinned("merged_sha256"),
            "{name}-8MB.bin is not the build tests/fw/manifest.toml pins: run `cargo xtask probes`"
        );
        let Ok(elf) = std::fs::read(corpus.join(format!("probes/build/{name}/{name}.elf"))) else {
            common::skip(test, &format!("no unstripped ELF for {name}"));
            continue;
        };
        assert_eq!(
            sha256_hex(&elf),
            pinned("elf_sha256"),
            "the unstripped {name}.elf is not the pinned build: run `cargo xtask probes`"
        );
        let (reason, text) = campaign_console(&flash, Some(&elf));
        let lines = probe_lines(&text);
        let path = repo.join(format!("tests/fw/campaign/{name}.emu.txt"));
        if record {
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("tests/fw/campaign");
            std::fs::write(&path, &lines).expect("the record is writable");
            println!(
                "RECORDED {name}: {} line(s), stop {reason:?}",
                lines.lines().count()
            );
            continue;
        }
        assert!(
            matches!(reason, StopReason::Matcher(_)) && lines.contains("\nDONE|"),
            "{name} did not reach its DONE line: {reason:?}\n{text}"
        );
        let want = std::fs::read_to_string(&path).expect("the emulator record is committed");
        assert_eq!(
            lines, want,
            "{name} no longer prints its recorded lines; if the model change is intended, renew \
             tests/fw/campaign/ with CAMPAIGN_RECORD=1"
        );
        println!("RAN {test} {name}");
    }
}

/// The device captures of `probe_campaign_regs` and `probe_campaign_timing`, below the data root's
/// `captures/`.
const REGS_CAPTURE: &str = "device-probe_campaign_regs-20260924T164141Z-run1.clean.log";
const TIMING_CAPTURES: [&str; 2] = [
    "device-probe_campaign_timing-20260924T155139Z-run1.log",
    "device-probe_campaign_timing-20260924T155139Z-run2.log",
];

/// Whether `tag_fact`, the start of a probe line of `probe`, is a register row settled from those
/// captures (the note's "Step 3: the register rows"). Of the timing probe only
/// `rsa.RSA_M_PRIME` is a register row.
fn regs_row(probe: &str, tag_fact: &str) -> bool {
    let Some((tag, fact)) = tag_fact.split_once('|') else {
        return false;
    };
    if probe == "probe_campaign_timing" {
        return tag == "MPI" && fact.starts_with("modmult_mprime_");
    }
    match tag {
        "FLASH" => fact == "read_0x800000",
        "GATE" => fact.starts_with("i2s0_") || fact.starts_with("aes_"),
        "REG" => {
            fact.starts_with("regi2c.")
                || fact.starts_with("iomux.")
                || fact.starts_with("uart0.")
                || fact.starts_with("system.SYSTEM_PERIP_")
                || (fact.starts_with("gpio.GPIO_FUNC") && fact.ends_with("_IN_SEL_CFG"))
                || [
                    "spi0.SPI_MEM_USER",
                    "spi0.SPI_MEM_MOSI_DLEN",
                    "spi0.SPI_MEM_MISO_DLEN",
                    "spi0.SPI_MEM_MISC",
                    "spi1.SPI_MEM_CTRL2",
                    "timg0.TIMG_RTCCALICFG",
                    "timg0.TIMG_RTCCALICFG2",
                    "i2s0.I2S_TX_PCM2PDM_CONF.reset",
                ]
                .contains(&fact)
        }
        _ => false,
    }
}

fn facts(text: &str) -> BTreeMap<String, String> {
    probe_lines(text)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            let key = format!("{}|{}", parts.next()?, parts.next()?);
            Some((key, line.to_string()))
        })
        .collect()
}

fn fields(line: &str) -> BTreeMap<&str, &str> {
    line.split('|')
        .skip(2)
        .filter_map(|kv| kv.split_once('='))
        .collect()
}

/// Every step 3 register row prints on the device what the committed record prints, line for
/// line. The class A rows of `specs/blocks/*.toml` citing these captures name this test.
///
/// Field by field, where part of the line is not a model fact:
/// - `USJ|fram_num_width`: `max` follows the host's SOF phase (2046 on the device, 2041 here).
/// - `ADC|press_<n>`: the median only. `press_2` is pinned one code below the device, 393 against
///   394: under the synthesized eFuse calibration the curve reads 274 mV at both, so the SAR ADC
///   model's inverse never lands on 394 (`boards/ai-passport.toml` `[buttons]`).
#[test]
fn t1_campaign_regs_rows_match_the_device_capture() {
    let test = "t1_campaign_regs_rows_match_the_device_capture";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let read = |name: &str| std::fs::read_to_string(root.join("captures").join(name)).ok();
    let Some(regs) = read(REGS_CAPTURE) else {
        common::skip(test, "no probe_campaign_regs capture");
        return;
    };
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let record = |name: &str| {
        std::fs::read_to_string(repo.join(format!("tests/fw/campaign/{name}.emu.txt")))
            .expect("the emulator record is committed")
    };
    let mut pairs = vec![(
        REGS_CAPTURE,
        "probe_campaign_regs",
        facts(&regs),
        facts(&record("probe_campaign_regs")),
    )];
    for name in TIMING_CAPTURES {
        let Some(text) = read(name) else {
            common::skip(test, &format!("no {name}"));
            return;
        };
        pairs.push((
            name,
            "probe_campaign_timing",
            facts(&text),
            facts(&record("probe_campaign_timing")),
        ));
    }

    let mut compared = 0;
    for (capture, probe, dev, emu) in &pairs {
        for (key, line) in dev.iter().filter(|(k, _)| regs_row(probe, k)) {
            let got = emu
                .get(key)
                .unwrap_or_else(|| panic!("{test}: the record prints no {key} ({capture})"));
            assert_eq!(got, line, "{test}: {key} ({capture})");
            compared += 1;
        }
    }
    // 1 FLASH, 3 regi2c, 22 IO_MUX pads, 128 input selectors, 5 SPI_MEM, 4 SYSTEM gates, 4 UART0,
    // 2 TIMG0, 8 GATE, 1 PCM2PDM, and 2 MPI lines in each of the two timing runs.
    assert_eq!(
        compared,
        1 + 3 + 22 + 128 + 5 + 4 + 4 + 2 + 8 + 1 + 2 * 2,
        "{test}: rows compared"
    );

    let (dev, emu) = (&pairs[0].2, &pairs[0].3);
    let field = |map: &BTreeMap<String, String>, key: &str, f: &str| -> String {
        let line = map.get(key).unwrap_or_else(|| panic!("{test}: no {key}"));
        fields(line)
            .get(f)
            .unwrap_or_else(|| panic!("{test}: {key} has no {f}"))
            .to_string()
    };
    for f in ["samples", "period_ms", "over_2047", "wraps", "moved"] {
        assert_eq!(
            field(emu, "USJ|fram_num_width", f),
            field(dev, "USJ|fram_num_width", f),
            "{test}: USJ|fram_num_width {f}"
        );
    }
    for press in ["ADC|press_1", "ADC|press_3"] {
        assert_eq!(
            field(emu, press, "median"),
            field(dev, press, "median"),
            "{test}: {press} median"
        );
    }
    let down: u32 = field(dev, "ADC|press_2", "median").parse().expect("a code");
    assert_eq!(
        field(emu, "ADC|press_2", "median"),
        (down - 1).to_string(),
        "{test}: ADC|press_2 median, pinned one code below the device (see the documentation)"
    );
    println!("RAN {test}: {compared} rows equal to the device");
}

/// The device capture of `probe_campaign_reset`, host-wait build.
const RESET_CAPTURE: &str = "device-probe_campaign_reset-20260924T164812Z-run1.log";

/// The step 3 reset rows (the note's "Step 3: reset and deep sleep") print on the device what the
/// committed record prints: `SWD|armed`, `SWD|reset`, `SLEEP|armed`, `WAKE|deep_sleep`, `REG` and
/// `WAIT`.
///
/// `BOOT|boot2` and `BOOT|boot3` compare `step`, `reason`, `raw` and `rtc_magic`, and of
/// `rtc_time_ms` only that it restarts at the super-watchdog reset (boot2 under a second, the
/// device's boot1 2852258). `BOOT|boot1` differs by capture method; this capture has no
/// `SWD|timeout`.
#[test]
fn t1_campaign_reset_rows_match_the_device_capture() {
    let test = "t1_campaign_reset_rows_match_the_device_capture";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let Ok(capture) = std::fs::read_to_string(root.join("captures").join(RESET_CAPTURE)) else {
        common::skip(test, "no probe_campaign_reset capture");
        return;
    };
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let record =
        std::fs::read_to_string(repo.join("tests/fw/campaign/probe_campaign_reset.emu.txt"))
            .expect("the emulator record is committed");
    let (dev, emu) = (facts(&capture), facts(&record));

    let exact = [
        "WAIT|boot1",
        "WAIT|boot2",
        "WAIT|boot3",
        "SWD|armed",
        "SWD|reset",
        "SLEEP|armed",
        "WAKE|deep_sleep",
        "REG|rtc_cntl.RTC_CNTL_DIG_ISO.after_deep_sleep",
        "REG|rtc_cntl.RTC_CNTL_PWC.after_deep_sleep",
        "REG|rtc_cntl.RTC_CNTL_DIG_PAD_HOLD.after_deep_sleep",
    ];
    for key in exact {
        let want = dev
            .get(key)
            .unwrap_or_else(|| panic!("{test}: the capture prints no {key}"));
        let got = emu
            .get(key)
            .unwrap_or_else(|| panic!("{test}: the record prints no {key}"));
        assert_eq!(got, want, "{test}: {key}");
    }
    let field = |map: &BTreeMap<String, String>, key: &str, f: &str| -> String {
        let line = map.get(key).unwrap_or_else(|| panic!("{test}: no {key}"));
        fields(line)
            .get(f)
            .unwrap_or_else(|| panic!("{test}: {key} has no {f}"))
            .to_string()
    };
    for key in ["BOOT|boot2", "BOOT|boot3"] {
        for f in ["step", "reason", "raw", "rtc_magic"] {
            assert_eq!(
                field(&emu, key, f),
                field(&dev, key, f),
                "{test}: {key} {f}"
            );
        }
    }
    let ms = |map: &BTreeMap<String, String>, key: &str| -> u64 {
        field(map, key, "rtc_time_ms").parse().expect("a count")
    };
    assert!(
        ms(&dev, "BOOT|boot1") > 1_000_000,
        "{test}: the device ran long before boot1"
    );
    for (side, map) in [("device", &dev), ("emulator", &emu)] {
        assert!(
            ms(map, "BOOT|boot2") < 1_000,
            "{test}: the RTC counter restarts at the super-watchdog reset ({side})"
        );
    }
    println!(
        "RAN {test}: {} lines and two BOOT lines equal to the device",
        exact.len()
    );
}

/// `rtc_cntl.RTC_CNTL_STORE1` at a `SYS_` reset: the reset probe with its first printed boot an
/// esptool reset (`rst:0x15`) with the RTC counter already running, as
/// `device-probe_campaign_reset-20260924T201202Z` run1 to run3 (boot1 `rtc_time_ms` 12122679,
/// then 51 after the super-watchdog reset).
///
/// IDF's `esp_rtc_get_time_us` (`esp_hw_support/esp_clk.c`) adds `(ticks - rtc_last_ticks) x
/// STORE1` to a time kept in RTC memory and restarts from 0 only when STORE1 reads 0; kept across
/// a restarted counter the difference wraps, so the super-watchdog reset clears STORE1. The
/// committed record cannot show it: its boot1 is a power-on.
#[test]
fn t1_campaign_rtc_time_restarts_after_a_super_watchdog_reset() {
    let test = "t1_campaign_rtc_time_restarts_after_a_super_watchdog_reset";
    let Some(corpus) = corpus_dir(test) else {
        return;
    };
    let Ok(flash) = std::fs::read(corpus.join("probes/probe_campaign_reset-8MB.bin")) else {
        common::skip(test, "no probe image probe_campaign_reset-8MB.bin");
        return;
    };
    let mut m = machine_with(
        &flash,
        None,
        MachineConfig {
            profile: pemu_machine::config::TimingProfileId::Device,
            ..MachineConfig::default()
        },
    );
    // The power-on boot is still in its host wait at 300 ms; the esptool reset then starts boot1
    // with the counter running, as on the device.
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(300)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(out.reason, StopReason::Until, "{test}: the power-on boot");
    m.input(
        At::Now,
        InputEvent::UsbLine {
            dtr: false,
            rts: true,
        },
    )
    .expect("now is not in the past");
    let done = MatcherId(0xCA);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(CAMPAIGN_BUDGET_MS)),
        max_insns: None,
        stops: StopSet {
            matchers: vec![(
                done,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Prefix("DONE|".into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let text = console(&mut m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(done),
        "{test}: no DONE line:\n{text}"
    );
    let emu = facts(&text);
    let field = |key: &str, f: &str| -> String {
        let line = emu
            .get(key)
            .unwrap_or_else(|| panic!("{test}: no {key}:\n{text}"));
        fields(line)
            .get(f)
            .unwrap_or_else(|| panic!("{test}: {key} has no {f}"))
            .to_string()
    };
    let ms = |key: &str| -> u64 { field(key, "rtc_time_ms").parse().expect("a count") };
    assert_eq!(
        field("BOOT|boot1", "raw"),
        "0x15",
        "{test}: boot1 is the esptool reset"
    );
    assert_eq!(
        field("BOOT|boot2", "raw"),
        "0x12",
        "{test}: boot2 follows the super watchdog"
    );
    assert!(
        ms("BOOT|boot2") < 1_000,
        "{test}: IDF's RTC time restarts at the super-watchdog reset, as the device's 51 ms; \
         boot2 read {} ms",
        ms("BOOT|boot2")
    );
    assert!(
        ms("BOOT|boot1") > ms("BOOT|boot2"),
        "{test}: the counter at boot1 is past the one at boot2, the device's case"
    );
    // The deep sleep keeps STORE1, so boot3 continues boot2's time.
    assert!(
        (1_000..3_000).contains(&ms("BOOT|boot3")),
        "{test}: boot3 continues boot2's time across the deep sleep"
    );
    println!(
        "RAN {test}: boot1 {} ms, boot2 {} ms, boot3 {} ms",
        ms("BOOT|boot1"),
        ms("BOOT|boot2"),
        ms("BOOT|boot3")
    );
}

/// `hle.ble.reply_us` (`ble.toml` `[controller]`): under U4 the radio probe times HCI Reset and
/// Read Local Version inside the two device runs of `device-probe_campaign_radio-20260924T155729Z`
/// (run1 L69 and L70: 523 and 76 us; run2: 516 and 82 us). The test sets U4 so it keeps pinning
/// the magic-ISR path whatever the default; under U5 the worker's 20 ms poll decides the time.
#[test]
fn t1_campaign_radio_under_u4_answers_hci_inside_the_device_runs() {
    let test = "t1_campaign_radio_under_u4_answers_hci_inside_the_device_runs";
    let Some(corpus) = corpus_dir(test) else {
        return;
    };
    let name = "probe_campaign_radio";
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = std::fs::read_to_string(repo.join("tests/fw/manifest.toml"))
        .expect("the probe manifest is committed");
    let Ok(flash) = std::fs::read(corpus.join(format!("probes/{name}-8MB.bin"))) else {
        common::skip(test, &format!("no probe image {name}-8MB.bin"));
        return;
    };
    let Ok(elf) = std::fs::read(corpus.join(format!("probes/build/{name}/{name}.elf"))) else {
        common::skip(test, &format!("no unstripped ELF for {name}"));
        return;
    };
    for (bytes, key) in [(&flash, "merged_sha256"), (&elf, "elf_sha256")] {
        assert_eq!(
            Some(sha256_hex(bytes)),
            manifest_field(&manifest, name, key),
            "{name}: not the build tests/fw/manifest.toml pins ({key}): run `cargo xtask probes`"
        );
    }
    let mut cfg = MachineConfig {
        profile: pemu_machine::config::TimingProfileId::Device,
        ..MachineConfig::default()
    };
    cfg.hle.wake = pemu_hle::worker::WakeMode::U4MagicIsr;
    let (reason, text) = campaign_console_with(&flash, Some(&elf), cfg);
    let lines = probe_lines(&text);
    assert!(
        matches!(reason, StopReason::Matcher(_)) && lines.contains("\nDONE|"),
        "{name} did not reach its DONE line: {reason:?}\n{text}"
    );
    let us = |prefix: &str| -> u32 {
        let line = lines
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} line:\n{lines}"));
        line.split('|')
            .find_map(|f| f.strip_prefix("us="))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no us= in {line}"))
    };
    let (reset, version) = (us("HCI|reset|"), us("HCI|read_local_version|"));
    assert!(
        (516..=523).contains(&reset),
        "{test}: HCI Reset answered in {reset} us, the device runs 516 and 523"
    );
    assert!(
        (76..=82).contains(&version),
        "{test}: Read Local Version answered in {version} us, the device runs 76 and 82"
    );
    println!("RAN {test}: reset {reset} us, read_local_version {version} us");
}

/// The capture the per-class cycle costs are derived from.
const CPI_CAPTURE: &str = "device-probe_campaign_timing-20260924T234712Z-run1.log";

/// The committed record of `probe_campaign_timing` prints the device's `TIME|cpi_<class>` cycles
/// for the twelve kernels the class table reproduces, and the three class C kernels short by their
/// residue (the load-use pair beside an IRAM fetch the bank rule leaves, and the GPIO read's bus
/// phase). `cpi_load_dram` and `cpi_store_dram` are not compared: their SRAM Block 1 bank cost
/// follows the operand's address bit 3, clear in this capture's build (`ram_word` at
/// `0x3fc8de20`, where the rule gives the device's 3583 and 4094) and set in the recorded one.
///
/// `TIME|sha256_64k_prefilled`, which `sha_block_ps` is derived from, reads within 0.6 % of
/// [`FETCH_CAPTURE`] (171806 on silicon; three builds read 171079, 169843 and 171806).
#[test]
fn t1_campaign_cpi_rows_match_the_device_capture() {
    let test = "t1_campaign_cpi_rows_match_the_device_capture";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let Ok(capture) = std::fs::read_to_string(root.join("captures").join(CPI_CAPTURE)) else {
        common::skip(test, "no probe_campaign_timing capture");
        return;
    };
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let record =
        std::fs::read_to_string(repo.join("tests/fw/campaign/probe_campaign_timing.emu.txt"))
            .expect("the emulator record is committed");
    let (dev, emu) = (facts(&capture), facts(&record));
    let cycles = |map: &BTreeMap<String, String>, key: &str| -> i64 {
        let line = map.get(key).unwrap_or_else(|| panic!("{test}: no {key}"));
        fields(line)
            .get("cycles")
            .unwrap_or_else(|| panic!("{test}: {key} has no cycles"))
            .parse()
            .expect("a count")
    };
    // (kernel, emulator minus device): 0 for the classes the table reproduces, the class C residue
    // for the rest.
    let rows: [(&str, i64); 15] = [
        ("empty", 0),
        ("alu", 0),
        ("branch_not_taken", 0),
        ("branch_taken", 0),
        ("jump", 0),
        ("call_ret", 0),
        ("load_dram_use", -257),
        ("load_dram_use_gap1", -1024),
        ("load_flash", 0),
        ("load_flash_use", 0),
        ("mul", 0),
        ("div", 0),
        ("csr_read", 0),
        ("mmio_read", -255),
        ("mmio_write", 0),
    ];
    for (kernel, residue) in rows {
        let key = format!("TIME|cpi_{kernel}");
        let (d, e) = (cycles(&dev, &key), cycles(&emu, &key));
        assert_eq!(
            e - d,
            residue,
            "{test}: {key} reads {e} cycles against the device's {d}"
        );
    }
    let key = "TIME|sha256_64k_prefilled";
    let Ok(built) = std::fs::read_to_string(root.join("captures").join(FETCH_CAPTURE)) else {
        common::skip(
            test,
            "no capture of the recorded probe_campaign_timing build",
        );
        return;
    };
    let (d, e) = (cycles(&facts(&built), key), cycles(&emu, key));
    assert!(
        (e - d).abs() * 1000 <= 6 * d,
        "{test}: {key} reads {e} cycles against the device's {d}"
    );
    println!(
        "RAN {test}: 12 cpi kernels equal to the device, 3 at their class C residue, \
         sha256_64k_prefilled {e} against {d}"
    );
}

/// The capture of the `probe_campaign_timing` build at main b2a016e1: run1 and run2 are identical
/// on every cycle row.
const FILL_CAPTURE: &str = "device-probe_campaign_timing-20260925T042926Z-run1.log";

/// The `TIME` lines of a console keyed by fact and, for the placement kernels, by where their
/// operand was (`data=`), since those print one fact several times.
fn placed_facts(text: &str) -> BTreeMap<String, String> {
    probe_lines(text)
        .lines()
        .filter(|line| line.starts_with("TIME|"))
        .map(|line| {
            let fact = line.split('|').nth(1).unwrap_or_default();
            let key = match fields(line).get("data") {
                Some(data) => format!("{fact}@{data}"),
                None => fact.to_string(),
            };
            (key, line.to_string())
        })
        .collect()
}

/// The committed record of `probe_campaign_timing` against [`FILL_CAPTURE`] (the note's
/// "Step 3i").
///
/// To the cycle: the divides of a zero quotient and by 1, `mulhu`, the EXTMEM read and the GPIO
/// read and write at CPU 80 MHz. The six `fill_work` lines, whose cold minus warm falls from 40068
/// to 13312 cycles as the work between misses grows, within 150 cycles cold and equal warm.
///
/// Class C, pinned at its residue: the GPIO and SYSTIMER reads at 160 MHz, one cycle an iteration
/// (the APB clock edge); the DRAM kernels whose operand is in SRAM Block 1, where the IRAM code is
/// fetched from (TRM table 16.3-1): the plain load and store one cycle short, the load-use pairs
/// at their residue.
#[test]
fn t1_campaign_fill_overlap_divide_and_mmio_rows_match_the_device_capture() {
    let test = "t1_campaign_fill_overlap_divide_and_mmio_rows_match_the_device_capture";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let Ok(capture) = std::fs::read_to_string(root.join("captures").join(FILL_CAPTURE)) else {
        common::skip(
            test,
            "no probe_campaign_timing capture of the recorded build",
        );
        return;
    };
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let record =
        std::fs::read_to_string(repo.join("tests/fw/campaign/probe_campaign_timing.emu.txt"))
            .expect("the emulator record is committed");
    let (dev, emu) = (placed_facts(&capture), placed_facts(&record));
    let field = |map: &BTreeMap<String, String>, key: &str, name: &str| -> i64 {
        let line = map.get(key).unwrap_or_else(|| panic!("{test}: no {key}"));
        fields(line)
            .get(name)
            .unwrap_or_else(|| panic!("{test}: {key} has no {name}"))
            .parse()
            .expect("a count")
    };
    let rows: [(&str, i64); 21] = [
        ("cpi_x_div_q0@static", 0),
        ("cpi_x_div_by1@static", 0),
        ("cpi_x_mulhu@static", 0),
        ("cpi_x_csr_write@static", 0),
        ("cpi_mmio_read_extmem_icache_ctrl", 0),
        ("cpi80_empty", 0),
        ("cpi80_alu", 0),
        ("cpi80_mmio_read", 0),
        ("cpi80_mmio_write", 0),
        ("cpi_mmio_read_gpio_in", -255),
        ("cpi_mmio_read_systimer_conf", -255),
        ("cpi_at_load_dram@static", -1),
        ("cpi_at_load_dram_use@static", -257),
        ("cpi_at_load_dram_use_gap1@static", -1024),
        ("cpi_at_store_dram@static", -1),
        ("cpi_at_load_dram@heap_high", 0),
        ("cpi_at_load_dram_use_gap1@heap_high", 0),
        ("cpi_at_store_dram@heap_high", 0),
        ("cpi_at_load_dram@heap_mid", 0),
        ("cpi80_load_dram", -1),
        ("cpi80_store_dram", -1),
    ];
    for (kernel, residue) in rows {
        let (d, e) = (field(&dev, kernel, "cycles"), field(&emu, kernel, "cycles"));
        assert_eq!(
            e - d,
            residue,
            "{test}: {kernel} reads {e} cycles against the device's {d}"
        );
    }
    let mut worst = 0;
    for run in [
        "fill_work_use_k0",
        "fill_work_use_k32",
        "fill_work_use_k96",
        "fill_work_nouse_k96",
        "fill_work80_use_k0",
        "fill_work80_use_k48",
    ] {
        let warm = (field(&dev, run, "warm"), field(&emu, run, "warm"));
        assert_eq!(warm.1, warm.0, "{test}: {run} warm");
        let (d, e) = (field(&dev, run, "cold"), field(&emu, run, "cold"));
        assert!(
            (e - d).abs() <= 150,
            "{test}: {run} reads {e} cycles cold against the device's {d}"
        );
        worst = worst.max((e - d).abs());
    }
    println!(
        "RAN {test}: 13 rows to the cycle (4 with the operand in SRAM Block 2), 4 within one cycle, 4 at their class C residue, the six fill_work lines \
         within {worst} cycles"
    );
}

/// The capture of the build the record is taken from (main 7c94feca, ELF `222fdc5f...`): run1 and
/// run2 are identical on every row compared.
const FETCH_CAPTURE: &str = "device-probe_campaign_timing-20260925T100555Z-run1.log";

/// The committed record of `probe_campaign_timing` against [`FETCH_CAPTURE`] (the note's
/// "Step 3k").
///
/// The streaming fetch-ahead (`cold::CacheModel::fetch`): the cold `fetch_end` lines stay flat
/// from 0 to 128 work cycles a line and rise only at 256, within 0.5 % (1.5 % at 256); the
/// `fetch_mid` and `fetchf_mid` lines within 0.15 %; `fill_workf` and `cache_code_cold` within
/// 0.7 %.
///
/// The SRAM Block 1 bank rule (`cost::bank_step`): the 162 `dres` cells read 3583 or 3839 by the
/// parity of (code offset + data offset) / 8, equal where the device reads 3583 and one cycle
/// short where it reads 3839; `cpi_load_dram`, `cpi_store_dram` and the gap 0 kernels to the
/// cycle; the static and low-heap plain kernels one cycle short. Class C at its residue: the
/// load-use pair and the loads with 1 or 3 instructions after them. The `wline` lines (line
/// entry, refuted) within 4 cycles.
#[test]
fn t1_campaign_fetch_ahead_and_block_1_bank_rows_match_the_device_capture() {
    let test = "t1_campaign_fetch_ahead_and_block_1_bank_rows_match_the_device_capture";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let Ok(capture) = std::fs::read_to_string(root.join("captures").join(FETCH_CAPTURE)) else {
        common::skip(
            test,
            "no probe_campaign_timing capture of the recorded build",
        );
        return;
    };
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let record =
        std::fs::read_to_string(repo.join("tests/fw/campaign/probe_campaign_timing.emu.txt"))
            .expect("the emulator record is committed");
    let (dev, emu) = (placed_facts(&capture), placed_facts(&record));
    let field = |map: &BTreeMap<String, String>, key: &str, name: &str| -> i64 {
        let line = map.get(key).unwrap_or_else(|| panic!("{test}: no {key}"));
        fields(line)
            .get(name)
            .unwrap_or_else(|| panic!("{test}: {key} has no {name}"))
            .parse()
            .expect("a count")
    };
    // (line, field, band in parts per ten thousand of the device's count)
    let mut bands: Vec<(String, &str, i64)> = Vec::new();
    for k in [0, 8, 16, 32, 64, 128] {
        bands.push((format!("fetch_end_k{k}"), "cold", 50));
    }
    bands.push(("fetch_end_k256".into(), "cold", 150));
    for k in [0, 8, 16, 32, 64, 128, 256] {
        bands.push((format!("fetch_mid_k{k}"), "cold", 15));
    }
    for k in [0, 64, 256] {
        bands.push((format!("fetchf_mid_k{k}"), "cold", 15));
    }
    bands.push(("fill_workf_use_k32".into(), "cold", 70));
    bands.push(("fill_workf_use_k96".into(), "cold", 70));
    bands.push(("cache_code_cold".into(), "cycles", 70));
    let mut worst = 0.0f64;
    for (line, name, band) in &bands {
        let (d, e) = (field(&dev, line, name), field(&emu, line, name));
        assert!(
            (e - d).abs() * 10_000 <= band * d,
            "{test}: {line} reads {e} cycles {name} against the device's {d}"
        );
        worst = worst.max((e - d).abs() as f64 * 100.0 / d as f64);
    }
    let mut cells = 0;
    for kind in ["load", "store"] {
        for c in (0..=64).step_by(8) {
            let line = format!("dres_{kind}_c{c:02}");
            for d_off in (0..=64).step_by(8) {
                let name = format!("d{d_off:02}");
                let (d, e) = (field(&dev, &line, &name), field(&emu, &line, &name));
                let odd = (c + d_off) / 8 % 2 == 1;
                assert_eq!(
                    d,
                    if odd { 3839 } else { 3583 },
                    "{test}: {line} {name}: the device's parity pattern"
                );
                assert_eq!(
                    e - d,
                    if odd { -1 } else { 0 },
                    "{test}: {line} {name} reads {e} cycles against the device's {d}"
                );
                cells += 1;
            }
        }
    }
    let rows: [(&str, i64); 21] = [
        ("cpi_load_dram", 0),
        ("cpi_store_dram", 0),
        ("cpi_gap_load_gap0@static", 0),
        ("cpi_gap_store_gap0@static", 0),
        ("cpi_at_load_dram@static", -1),
        ("cpi_at_store_dram@static", -1),
        ("cpi_at_load_dram@heap_low", -1),
        ("cpi_at_store_dram@heap_low", -1),
        ("cpi80_load_dram", -1),
        ("cpi80_store_dram", -1),
        ("cpi_at_load_dram_use@static", -257),
        ("cpi_at_load_dram_use@heap_low", -257),
        ("cpi_at_load_dram_use_gap1@static", -1024),
        ("cpi_at_load_dram_use_gap1@heap_low", -1024),
        ("cpi_gap_load_gap3@static", -256),
        ("cpi_gap_store_gap3@static", -256),
        ("cpi_x_load_store_data@static", -256),
        ("wline_seq", 0),
        ("wline_seq_iram", 0),
        ("wline_jump", -3),
        ("wline_jump_in", -4),
    ];
    for (kernel, residue) in rows {
        let (d, e) = (field(&dev, kernel, "cycles"), field(&emu, kernel, "cycles"));
        assert_eq!(
            e - d,
            residue,
            "{test}: {kernel} reads {e} cycles against the device's {d}"
        );
    }
    println!(
        "RAN {test}: {} fetch and fill lines within {worst:.2} %, {cells} dres cells equal or one \
         cycle short, 10 Block 1 kernels within one cycle, 7 at their class C residue, the wline \
         lines within 4 cycles",
        bands.len()
    );
}

/// The three captures of the `probe_campaign_reset` build with the `TIMEBASE` lines.
const TIMEBASE_CAPTURES: [&str; 3] = [
    "device-probe_campaign_reset-20260924T232751Z-run1.log",
    "device-probe_campaign_reset-20260924T232751Z-run2.log",
    "device-probe_campaign_reset-20260924T232751Z-run3.log",
];

/// The boot time base of `probe_campaign_reset`, from the committed record, against the three
/// device runs (the note's "Step 3h"); the bands are in `rows`.
///
/// Modelled:
/// - `esp_timer` counts from its own init, where IDF pulses `SYSTIMER_RST` (`wiring::gates`).
///   boot1 `timer_us` (lines 59, 2631 to 2645) reads 2478, 5.8 to 6.3 % short: the cold flash
///   code of the app start (class C, as `probe_campaign_timing` `aes_cbc_16`).
/// - The IDF console's 50 ms flush timeout runs from `esp_timer` 0 while the USB link is down
///   after the super-watchdog reset: boot2 reaches app_main at `timer_us` 50400 on all three runs
///   (line 66), `rtc_time_ms` 51 on both sides.
/// - The SLEEP line's `timer_us` (line 69): boot2's app_main plus the host wait and prints.
///
/// Class C, pinned at its measured residue (emulator minus device, over the three runs):
/// - boot2 `rtc_counter_us`: +4.3 ms, all before `esp_timer` starts, where the model's ROM waits
///   16 ms for the undrained USB packet with the link down;
/// - boot3 `timer_us`: +6.2 to +8.5 ms; the model waits the whole flush timeout again after the
///   wake (50341 us), the device reaches app_main at 41.9 to 44.1 ms, mechanism not identified;
/// - boot3 `rtc_counter_us`: +15.4 to +17.3 ms, the two above plus the rest to boot3's
///   `esp_timer` init, less the device's RC slow clock running 0.1 % fast against its calibration.
#[test]
fn t1_campaign_the_boot_time_base_against_the_device_capture() {
    let test = "t1_campaign_the_boot_time_base_against_the_device_capture";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let mut runs = Vec::new();
    for name in TIMEBASE_CAPTURES {
        let Ok(text) = std::fs::read_to_string(root.join("captures").join(name)) else {
            common::skip(test, &format!("no {name}"));
            return;
        };
        runs.push(facts(&text));
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let emu = facts(
        &std::fs::read_to_string(repo.join("tests/fw/campaign/probe_campaign_reset.emu.txt"))
            .expect("the emulator record is committed"),
    );
    let value = |map: &BTreeMap<String, String>, key: &str, f: &str| -> i64 {
        let line = map.get(key).unwrap_or_else(|| panic!("{test}: no {key}"));
        fields(line)
            .get(f)
            .unwrap_or_else(|| panic!("{test}: {key} has no {f}"))
            .parse()
            .unwrap_or_else(|_| panic!("{test}: {key} {f} is not a number"))
    };
    // (key, field, lowest and highest emulator-minus-device difference allowed, in us)
    let rows: [(&str, &str, i64, i64); 8] = [
        ("TIMEBASE|boot1", "timer_us", -185, 132),
        ("TIMEBASE|boot2", "timer_us", -100, 100),
        ("TIMEBASE|boot2", "rtc_time_us", -110, 100),
        ("TIMEBASE|sleep", "timer_us", -260, 200),
        ("BOOT|boot2", "rtc_time_ms", 0, 0),
        ("TIMEBASE|boot2", "rtc_counter_us", 3_500, 5_500),
        ("TIMEBASE|boot3", "timer_us", 5_500, 9_000),
        ("TIMEBASE|boot3", "rtc_counter_us", 14_500, 18_500),
    ];
    for (key, f, lo, hi) in rows {
        let e = value(&emu, key, f);
        for (n, dev) in runs.iter().enumerate() {
            let d = value(dev, key, f);
            assert!(
                (lo..=hi).contains(&(e - d)),
                "{test}: {key} {f} reads {e} against run{}'s {d}, outside {lo}..={hi}",
                n + 1
            );
        }
    }
    println!(
        "RAN {test}: {} time-base fields against 3 device runs",
        rows.len()
    );
}

const WAYS_N: [u32; 8] = [4, 6, 7, 8, 9, 10, 12, 16];
const WAYS_PASSES: usize = 64;
/// Ways of a set (IDF `esp32c3/rom/cache.h` `MAX_ICACHE_WAYS`).
const WAYS: usize = 8;
/// One pass of `ways_retouch_n10`: lines 0 to 7, 0 and 1 again, then 8 and 9.
const WAYS_RETOUCH: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 8, 9];
const WAYS_RANDOM_TRIALS: u64 = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WaysKind {
    Cyclic,
    Pseudo,
    Mixed,
    Retouch,
}

/// The probe's access order (`ways_order` in `probe_campaign_timing.c`): the line indices of the
/// cold pass and the [`WAYS_PASSES`] after it, and one pass's length. Pseudo shuffles 0 to n-1
/// from the identity each pass (Fisher-Yates, from i = n-1 down) with one xorshift32 stream
/// seeded with 0x3A5E0000 + n.
fn ways_order(kind: WaysKind, n: u32) -> (Vec<u8>, usize) {
    let pass = if kind == WaysKind::Retouch {
        WAYS_RETOUCH.len()
    } else {
        n as usize
    };
    let mut rng: u32 = 0x3A5E_0000 + n;
    let mut idx = Vec::with_capacity(pass * (WAYS_PASSES + 1));
    for _ in 0..=WAYS_PASSES {
        let mut order: Vec<u8> = if kind == WaysKind::Retouch {
            WAYS_RETOUCH.to_vec()
        } else {
            (0..n as u8).collect()
        };
        if kind == WaysKind::Pseudo {
            for i in (1..n as usize).rev() {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                order.swap(i, (rng % (i as u32 + 1)) as usize);
            }
        }
        idx.extend(order);
    }
    (idx, pass)
}

/// FNV-1a 64, the probe's `order` digest.
fn ways_fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
    })
}

#[derive(Clone, Copy, Debug)]
enum WaysPolicy {
    Lru,
    /// Tree pseudo-LRU (7 bits, each pointing at the half to replace next), from the given initial
    /// bits: every access points the bits on its way's path away from it.
    TreePlru(u8),
    /// First in, first out: a hit changes nothing.
    Fifo,
    /// A uniformly random way on a miss, from a SplitMix64 stream with the given seed.
    Random(u64),
}

fn ways_tree_victim(tree: u8) -> usize {
    let (mut node, mut way) = (0usize, 0usize);
    for _ in 0..3 {
        let bit = usize::from((tree >> node) & 1);
        way = way * 2 + bit;
        node = 2 * node + 1 + bit;
    }
    way
}

fn ways_tree_touch(tree: &mut u8, way: usize) {
    let mut node = 0usize;
    for level in 0..3 {
        let bit = (way >> (2 - level)) & 1;
        if bit == 0 {
            *tree |= 1 << node;
        } else {
            *tree &= !(1 << node);
        }
        node = 2 * node + 1 + bit;
    }
}

/// Whether each access of a run hits, for a set that starts full of eight lines the run never
/// touches. The mixed runs' data lines share the ways with the code lines (one cache behind both
/// buses, `SOC_SHARED_IDCACHE_SUPPORTED`), so every run is one sequence of line indices.
fn ways_hits(policy: WaysPolicy, idx: &[u8]) -> Vec<bool> {
    const FOREIGN: u32 = 1 << 16;
    let mut tags: [u32; WAYS] = std::array::from_fn(|w| FOREIGN + w as u32);
    // Recency rank of each way (0 = most recent), for LRU.
    let mut rank: [usize; WAYS] = std::array::from_fn(|w| w);
    let mut next = 0usize;
    let (mut tree, mut rng) = match policy {
        WaysPolicy::TreePlru(bits) => (bits, 0),
        WaysPolicy::Random(seed) => (0, seed),
        WaysPolicy::Lru | WaysPolicy::Fifo => (0, 0),
    };
    let mut hits = Vec::with_capacity(idx.len());
    for &line in idx {
        let tag = u32::from(line);
        let found = tags.iter().position(|t| *t == tag);
        hits.push(found.is_some());
        let way = found.unwrap_or_else(|| {
            let victim = match policy {
                WaysPolicy::Lru => (0..WAYS).max_by_key(|w| rank[*w]).expect("8 ways"),
                WaysPolicy::Fifo => {
                    let v = next;
                    next = (next + 1) % WAYS;
                    v
                }
                WaysPolicy::TreePlru(_) => ways_tree_victim(tree),
                WaysPolicy::Random(_) => {
                    rng = rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
                    let mut z = rng;
                    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                    ((z ^ (z >> 31)) % WAYS as u64) as usize
                }
            };
            tags[victim] = tag;
            victim
        });
        let r = rank[way];
        for other in &mut rank {
            if *other < r {
                *other += 1;
            }
        }
        rank[way] = 0;
        ways_tree_touch(&mut tree, way);
    }
    hits
}

/// The cycles of a run's passes after the cold one, from its hits and the `device` fill timing
/// as `cold::CacheAccount` charges it: a hit costs its loop iteration, `hit.0` for a call and
/// `hit.1` for a data read (a hit on the line in transfer also waits for its word); a miss asks
/// for its line `cache_miss_cycles` after it and not before the previous transfer ended, and
/// waits for word 1 (a call: its `ret` in word 0 and the read-ahead) or word 0 (a data read), the
/// words arriving evenly from `cache_first_word_ps` to `cache_fill_ps`. In the mixed runs an odd
/// line is read.
fn ways_cycles(hits: &[bool], idx: &[u8], pass: usize, mixed: bool, hit: (f64, f64)) -> f64 {
    let profile = pemu_core::clock::TimingProfile::device();
    let cycle_ps = 6250.0; // CPU 160 MHz
    let fill = profile.cache_fill_ps as f64 / cycle_ps;
    let first = profile.cache_first_word_ps as f64 / cycle_ps;
    let per_word = (fill - first) / 7.0;
    let asked = f64::from(profile.cache_miss_cycles);
    let (mut t, mut busy, mut warm_start) = (0.0f64, 0.0f64, 0.0f64);
    let (mut fill_line, mut fill_start) = (None, 0.0f64);
    for (k, (&h, &line)) in hits.iter().zip(idx).enumerate() {
        if k == pass {
            warm_start = t;
        }
        let data = mixed && line % 2 == 1;
        let word = if data { 0.0 } else { 1.0 };
        let stall = if h {
            if fill_line == Some(line) {
                (fill_start + first + per_word * word - t).max(0.0)
            } else {
                0.0
            }
        } else {
            let start = t.max(busy) + asked;
            fill_line = Some(line);
            fill_start = start;
            busy = start + fill;
            start + first + per_word * word - t
        };
        t += if data { hit.1 } else { hit.0 } + stall;
    }
    t - warm_start
}

/// What one policy predicts for one line over its initial states or seeds: the least, most and
/// mean warm misses, the least and most cold ones, and the mean warm cycles when the costs of a
/// hit are known.
struct WaysPrediction {
    warm: (u32, u32, f64),
    cold: (u32, u32),
    cycles: Option<f64>,
}

fn ways_predict(
    policies: &[WaysPolicy],
    idx: &[u8],
    pass: usize,
    mixed: bool,
    hit: Option<(f64, f64)>,
) -> WaysPrediction {
    let (mut warm, mut cold, mut cycles) = (Vec::new(), Vec::new(), 0.0);
    for policy in policies {
        let hits = ways_hits(*policy, idx);
        let misses = |from: usize, to: usize| -> u32 {
            u32::try_from(hits[from..to].iter().filter(|h| !**h).count()).expect("a count")
        };
        cold.push(misses(0, pass));
        warm.push(misses(pass, hits.len()));
        if let Some(hit) = hit {
            cycles += ways_cycles(&hits, idx, pass, mixed, hit);
        }
    }
    let n = policies.len() as f64;
    WaysPrediction {
        warm: (
            *warm.iter().min().expect("a policy"),
            *warm.iter().max().expect("a policy"),
            f64::from(warm.iter().sum::<u32>()) / n,
        ),
        cold: (
            *cold.iter().min().expect("a policy"),
            *cold.iter().max().expect("a policy"),
        ),
        cycles: hit.map(|_| cycles / n),
    }
}

/// What true LRU (`lru16k`), tree pseudo-LRU, FIFO (`fifo16k`, the `device` profile's model) and
/// random replacement predict for each `TIME|ways_*` line, from the probe's own access order
/// (checked against the line's `order` digest). Tree pseudo-LRU runs from all 128 initial states,
/// random over [`WAYS_RANDOM_TRIALS`] seeds. Each policy's misses become cycles ([`ways_cycles`]),
/// and the `device` policy must give the record's cycles within 0.5 %. True LRU never misses warm
/// at n <= 8 and always at n >= 9; LRU, FIFO and tree pseudo-LRU miss each fresh line once cold;
/// on the retouch line the three give distinct counts and random's range lies apart.
#[test]
fn t0_campaign_replacement_policies_predict_the_ways_lines() {
    let test = "t0_campaign_replacement_policies_predict_the_ways_lines";
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let record =
        std::fs::read_to_string(repo.join("tests/fw/campaign/probe_campaign_timing.emu.txt"))
            .expect("the emulator record is committed");
    let emu = facts(&record);
    let mut lines: Vec<(String, WaysKind, u32)> = Vec::new();
    for (kind, name) in [
        (WaysKind::Cyclic, "cyclic"),
        (WaysKind::Pseudo, "pseudo"),
        (WaysKind::Mixed, "mixed"),
    ] {
        for n in WAYS_N {
            lines.push((format!("ways_{name}_n{n}"), kind, n));
        }
    }
    lines.push(("ways_retouch_n10".into(), WaysKind::Retouch, 10));

    let plru: Vec<WaysPolicy> = (0..=127u8).map(WaysPolicy::TreePlru).collect();
    let random: Vec<WaysPolicy> = (0..WAYS_RANDOM_TRIALS).map(WaysPolicy::Random).collect();
    let emu_cycles = |line: &str| -> Option<f64> {
        fields(emu.get(&format!("TIME|{line}"))?)
            .get("cycles")
            .and_then(|v| v.parse().ok())
    };
    // A call's and a data read's loop iteration when they hit: ways_cyclic_n4 is calls only and
    // ways_mixed_n4 half each.
    let per_access = 4.0 * WAYS_PASSES as f64;
    let hit = emu_cycles("ways_cyclic_n4").and_then(|code| {
        let mixed = emu_cycles("ways_mixed_n4")?;
        let call = code / per_access;
        Some((call, 2.0 * mixed / per_access - call))
    });
    if hit.is_none() {
        println!("NOTE {test}: the record has no ways lines yet; misses only");
    }
    let (mut digests, mut worst) = (0, 0.0f64);
    for (name, kind, n) in &lines {
        let (idx, pass) = ways_order(*kind, *n);
        let accesses = pass * WAYS_PASSES;
        let mixed = *kind == WaysKind::Mixed;
        let lru = ways_predict(&[WaysPolicy::Lru], &idx, pass, mixed, hit);
        let fifo = ways_predict(&[WaysPolicy::Fifo], &idx, pass, mixed, hit);
        let tree = ways_predict(&plru, &idx, pass, mixed, hit);
        let rnd = ways_predict(&random, &idx, pass, mixed, hit);
        let distinct = if *kind == WaysKind::Retouch { 10 } else { *n };
        for (policy, p) in [("lru", &lru), ("fifo", &fifo), ("plru", &tree)] {
            assert_eq!(
                p.cold,
                (distinct, distinct),
                "{test}: {name}: {policy} misses each fresh line once in the cold pass"
            );
        }
        if matches!(kind, WaysKind::Cyclic | WaysKind::Mixed) {
            let want = if *n as usize <= WAYS {
                0
            } else {
                u32::try_from(accesses).expect("a count")
            };
            assert_eq!(lru.warm.0, want, "{test}: {name}: true LRU");
        }
        if *kind == WaysKind::Retouch {
            let (l, f, t) = (lru.warm.0, fifo.warm.0, tree.warm);
            assert!(
                t.0 == t.1 && l != f && l != t.0 && f != t.0,
                "{test}: {name}: LRU {l}, FIFO {f} and tree pseudo-LRU {t:?} are not distinct"
            );
            let (lo, hi) = (rnd.warm.0, rnd.warm.1);
            assert!(
                [l, f, t.0].iter().all(|d| *d < lo || *d > hi),
                "{test}: {name}: random's {lo}..={hi} overlaps LRU {l}, FIFO {f} or PLRU {}",
                t.0
            );
        }
        let recorded = emu_cycles(name);
        if let Some(line) = emu.get(&format!("TIME|{name}")) {
            let want = format!("{:016x}", ways_fnv(&idx));
            assert_eq!(
                fields(line).get("order").copied(),
                Some(want.as_str()),
                "{test}: {name}: the model's access order is not the probe's"
            );
            digests += 1;
        }
        let committed = match TimingProfile::device().cache_model {
            CacheVariant::Fifo16k => &fifo,
            CacheVariant::Lru16k => &lru,
            CacheVariant::ColdPage => panic!("{test}: the device profile has no line cache"),
        };
        if let (Some(model), Some(rec)) = (committed.cycles, recorded) {
            let off = (model - rec) / rec;
            worst = worst.max(off.abs());
            assert!(
                off.abs() <= 0.005,
                "{test}: {name}: the committed policy predicts {model:.0} cycles, the record reads \
                 {rec:.0}"
            );
        }
        let show = |p: &WaysPrediction| -> String {
            let cycles = p.cycles.map_or_else(String::new, |c| format!(" = {c:.0}"));
            if p.warm.0 == p.warm.1 {
                format!("{}{cycles}", p.warm.0)
            } else {
                format!("{}..{} mean {:.1}{cycles}", p.warm.0, p.warm.1, p.warm.2)
            }
        };
        println!(
            "WAYS {name}: {accesses} accesses, warm misses{}: lru {} | plru {} | fifo {} | \
             random {}{}",
            if hit.is_some() { " = cycles" } else { "" },
            show(&lru),
            show(&tree),
            show(&fifo),
            show(&rnd),
            recorded.map_or_else(String::new, |c| format!(" | record {c:.0}")),
        );
    }
    println!(
        "RAN {test}: {} lines, 4 policies, {digests} access orders checked against the record, \
         the committed {} within {:.2} % of its cycles",
        lines.len(),
        TimingProfile::device().cache_model.as_str(),
        worst * 100.0
    );
}

/// The captures of the build with the `ways` lines (main 5591a120), run1 and run2 identical on
/// every `ways` line.
const WAYS_CAPTURES: [&str; 2] = [
    "device-probe_campaign_timing-20260925T133649Z-run1.log",
    "device-probe_campaign_timing-20260925T133649Z-run2.log",
];

/// The cache replaces first in, first out. On every one of the 25 `TIME|ways_*` lines of
/// [`WAYS_CAPTURES`], the device's EXTMEM IBUS and DBUS miss counters over the 64 passes are
/// exactly what FIFO predicts ([`ways_hits`]), split between fetches and data reads as FIFO
/// splits them, and each cold pass misses each line once. True LRU and every initial state of tree
/// pseudo-LRU miss the retouch line's count, and random's whole range lies below it. The record,
/// taken under `fifo16k`, reads each line's cycles within 0.2 % of the device (cold passes not
/// compared: the first run's reads 1322 on the device against 1145, the other 24 within 1 %).
#[test]
fn t1_campaign_fifo_replacement_matches_the_device_capture() {
    let test = "t1_campaign_fifo_replacement_matches_the_device_capture";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let mut runs = Vec::new();
    for name in WAYS_CAPTURES {
        let Ok(text) = std::fs::read_to_string(root.join("captures").join(name)) else {
            common::skip(test, &format!("no {name}"));
            return;
        };
        runs.push(facts(&text));
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let emu = facts(
        &std::fs::read_to_string(repo.join("tests/fw/campaign/probe_campaign_timing.emu.txt"))
            .expect("the emulator record is committed"),
    );
    let count = |line: &str, f: &str| -> u64 {
        fields(line)
            .get(f)
            .unwrap_or_else(|| panic!("{test}: no {f} in {line}"))
            .parse()
            .unwrap_or_else(|_| panic!("{test}: {f} is not a count in {line}"))
    };
    let mut lines: Vec<(String, WaysKind, u32)> = Vec::new();
    for (kind, name) in [
        (WaysKind::Cyclic, "cyclic"),
        (WaysKind::Pseudo, "pseudo"),
        (WaysKind::Mixed, "mixed"),
    ] {
        for n in WAYS_N {
            lines.push((format!("ways_{name}_n{n}"), kind, n));
        }
    }
    lines.push(("ways_retouch_n10".into(), WaysKind::Retouch, 10));
    let mut worst = 0.0f64;
    for (name, kind, n) in &lines {
        let key = format!("TIME|{name}");
        let (idx, pass) = ways_order(*kind, *n);
        let hits = ways_hits(WaysPolicy::Fifo, &idx);
        // FIFO's misses after the cold pass, on fetches and on data reads, and in the cold pass.
        let (mut fetch, mut data, mut cold) = (0u64, 0u64, 0u64);
        for (k, (hit, line)) in hits.iter().zip(&idx).enumerate() {
            if *hit {
                continue;
            }
            if k < pass {
                cold += 1;
            } else if *kind == WaysKind::Mixed && line % 2 == 1 {
                data += 1;
            } else {
                fetch += 1;
            }
        }
        let rec = emu
            .get(&key)
            .unwrap_or_else(|| panic!("{test}: the record has no {name}"));
        for (r, dev) in runs.iter().enumerate() {
            let line = dev
                .get(&key)
                .unwrap_or_else(|| panic!("{test}: run{} has no {name}", r + 1));
            assert_eq!(
                fields(line).get("order"),
                fields(rec).get("order"),
                "{test}: {name}: the capture's access order is not the record's"
            );
            assert_eq!(
                (count(line, "ibus_miss"), count(line, "dbus_miss")),
                (fetch, data),
                "{test}: {name} run{}: the device's warm misses are not FIFO's",
                r + 1
            );
            assert_eq!(
                count(line, "cold_ibus_miss") + count(line, "cold_dbus_miss"),
                cold,
                "{test}: {name} run{}: the cold pass",
                r + 1
            );
            let (d, e) = (count(line, "cycles") as f64, count(rec, "cycles") as f64);
            let off = (e - d) / d;
            worst = worst.max(off.abs());
            assert!(
                off.abs() <= 0.002,
                "{test}: {name} run{}: the record reads {e} cycles against the device's {d}",
                r + 1
            );
        }
        if *kind == WaysKind::Retouch {
            let device = count(&runs[0][&key], "ibus_miss");
            let misses =
                |p: WaysPolicy| ways_hits(p, &idx)[pass..].iter().filter(|h| !**h).count() as u64;
            assert_ne!(misses(WaysPolicy::Lru), device, "{test}: true LRU");
            assert!(
                (0..=127u8).all(|b| misses(WaysPolicy::TreePlru(b)) != device),
                "{test}: tree pseudo-LRU"
            );
            assert!(
                (0..WAYS_RANDOM_TRIALS).all(|s| misses(WaysPolicy::Random(s)) < device),
                "{test}: random"
            );
        }
    }
    println!(
        "RAN {test}: {} lines x {} runs: the EXTMEM miss counters equal FIFO's misses, fetches \
         and data reads apart; the record's cycles within {:.2} %",
        lines.len(),
        runs.len(),
        worst * 100.0
    );
}

const SWEEP_NOTE: &str = "specs/notes/silicon-campaign.md";

/// The number of rows in the inventory's six tables.
const SWEEP_ROWS: usize = 213;

/// The cells of a Markdown table row, split on the pipes a backslash does not escape, with the
/// escapes of the others removed.
fn table_cells(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cell).trim().to_string()),
            _ => cell.push(c),
        }
    }
    // The text before the first pipe is not a cell.
    if !cells.is_empty() {
        cells.remove(0);
    }
    cells
}

/// One capture line an inventory row cites: `<alias>:<line> <TAG|fact>[ ~<field>:<pct>%|*,...]`.
#[derive(Debug)]
struct Citation {
    alias: String,
    line: usize,
    /// `TAG|fact`, with `@<data>` for a placement kernel.
    key: String,
    /// Fields compared within a relative tolerance (`Some(pct)`) or not at all (`None`); every
    /// other field must be equal.
    tolerance: BTreeMap<String, Option<f64>>,
}

fn citation(span: &str) -> Option<Citation> {
    let (head, rest) = span.split_once(' ')?;
    let (alias, line) = head.split_once(':')?;
    if alias.is_empty() || !alias.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    let line = line.parse().ok()?;
    let (key, tol) = match rest.split_once(" ~") {
        Some((key, tol)) => (key, Some(tol)),
        None => (rest, None),
    };
    let (tag, _) = key.split_once('|')?;
    if !tag.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    let mut tolerance = BTreeMap::new();
    for item in tol.into_iter().flat_map(|t| t.split(',')) {
        let (field, pct) = item.split_once(':')?;
        let pct = match pct {
            "*" => None,
            p => Some(p.strip_suffix('%')?.parse().ok()?),
        };
        tolerance.insert(field.to_string(), pct);
    }
    Some(Citation {
        alias: alias.to_string(),
        line,
        key: key.to_string(),
        tolerance,
    })
}

fn citations(cell: &str) -> Vec<Citation> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .filter_map(citation)
        .collect()
}

fn placed_key(line: &str) -> Option<String> {
    let mut parts = line.splitn(3, '|');
    let key = format!("{}|{}", parts.next()?, parts.next()?);
    Some(match fields(line).get("data") {
        Some(data) => format!("{key}@{data}"),
        None => key,
    })
}

/// The sweep of the campaign inventory is checkable. Every row of the inventory's tables
/// (sections 1 to 6) carries a step 4 disposition (none still reads **probe**), every **open** row
/// names its Part B probe line, and the Total row of the last count table is what the rows say.
/// Every **settled** row cites at least one capture line (`<alias>:<line> <TAG|fact>`, aliases
/// from the note's "Step 4" and "Step 5" tables); with the data root, each cited line is at that
/// line of that capture and the committed record prints the same fact with every field equal but
/// those listed after `~`: within its percentage of the device, or not compared (`*`, a field
/// that measures the capture rather than the chip). Without the data root only the note half runs.
#[test]
fn t1_campaign_every_settled_row_names_a_capture_line() {
    let test = "t1_campaign_every_settled_row_names_a_capture_line";
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let note = std::fs::read_to_string(repo.join(SWEEP_NOTE)).expect("the inventory is committed");

    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut total: Option<Vec<u32>> = None;
    for line in note.lines().filter(|l| l.starts_with('|')) {
        let cells = table_cells(line);
        let code = |s: &str| s.strip_prefix('`')?.strip_suffix('`').map(str::to_string);
        if cells.len() >= 2
            && let (Some(alias), Some(file)) = (code(&cells[0]), code(&cells[1]))
            && file.starts_with("device-")
            && file.ends_with(".log")
        {
            aliases.insert(alias, file);
            continue;
        }
        if cells.first().map(String::as_str) == Some("**Total**") && cells.len() == 8 {
            total = Some(
                cells[1..]
                    .iter()
                    .map(|c| c.trim_matches('*').parse().expect("a count"))
                    .collect(),
            );
            continue;
        }
        let id = cells.first().cloned().unwrap_or_default();
        let is_row = id.len() >= 2
            && id.starts_with(['B', 'T', 'K', 'H', 'V', 'S'])
            && id[1..].bytes().all(|b| b.is_ascii_digit());
        if is_row {
            rows.push((id, cells.last().cloned().unwrap_or_default()));
        }
    }
    assert_eq!(rows.len(), SWEEP_ROWS, "{test}: inventory rows");
    assert!(
        !aliases.is_empty(),
        "{test}: the note has no capture alias table"
    );

    // Items, then settled A, settled B, settled with C kept, open, cannot, untouched.
    let mut counts = [rows.len() as u32, 0, 0, 0, 0, 0, 0];
    let mut cited: Vec<(String, Citation)> = Vec::new();
    for (id, cell) in &rows {
        let slot = if let Some(rest) = cell.strip_prefix("**settled** (") {
            let found = citations(cell);
            assert!(
                !found.is_empty(),
                "{test}: {id} is settled and cites no capture line"
            );
            for c in found {
                assert!(
                    aliases.contains_key(&c.alias),
                    "{test}: {id} cites `{}`, which the alias table does not name",
                    c.alias
                );
                cited.push((id.clone(), c));
            }
            if rest.starts_with("C kept") {
                3
            } else if rest.starts_with('A') {
                1
            } else if rest.starts_with('B') {
                2
            } else {
                panic!("{test}: {id}: a settled row names A, B or C kept")
            }
        } else if cell.starts_with("**open**") {
            assert!(
                cell.contains("Part B"),
                "{test}: {id} is open and names no Part B line"
            );
            4
        } else if cell.starts_with("**cannot**") {
            5
        } else if cell.starts_with("**untouched**") {
            6
        } else {
            panic!("{test}: {id} has no step 4 disposition: {cell}");
        };
        counts[slot] += 1;
    }
    assert_eq!(
        total.as_deref(),
        Some(&counts[..]),
        "{test}: the last count table's Total row (items, A, B, C kept, open, cannot, untouched)"
    );

    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(
            test,
            "no data root: the cited capture lines are not checked",
        );
        return;
    };
    let mut captures: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut records: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (alias, file) in &aliases {
        let Ok(text) = std::fs::read_to_string(root.join("captures").join(file)) else {
            common::skip(test, &format!("no {file}"));
            return;
        };
        captures.insert(
            alias.clone(),
            text.lines()
                .map(|l| l.trim_end_matches('\r').to_string())
                .collect(),
        );
        let probe = file
            .strip_prefix("device-")
            .and_then(|f| f.split('-').next())
            .expect("a capture is named device-<probe>-<stamp>");
        let record =
            std::fs::read_to_string(repo.join(format!("tests/fw/campaign/{probe}.emu.txt")))
                .unwrap_or_else(|_| panic!("{test}: no emulator record for {probe}"));
        let facts = probe_lines(&record)
            .lines()
            .filter_map(|l| Some((placed_key(l)?, l.to_string())))
            .collect();
        records.insert(alias.clone(), facts);
    }
    for (id, c) in &cited {
        let line = captures[&c.alias]
            .get(c.line - 1)
            .unwrap_or_else(|| panic!("{test}: {id}: {} has no line {}", c.alias, c.line));
        assert_eq!(
            placed_key(line).as_deref(),
            Some(c.key.as_str()),
            "{test}: {id}: line {} of {} is not {}: {line}",
            c.line,
            c.alias,
            c.key
        );
        let rec = records[&c.alias]
            .get(&c.key)
            .unwrap_or_else(|| panic!("{test}: {id}: the record prints no {}", c.key));
        let (dev, emu) = (fields(line), fields(rec));
        let names: std::collections::BTreeSet<&str> = dev
            .keys()
            .chain(emu.keys())
            .copied()
            .filter(|k| *k != "row")
            .collect();
        for name in names {
            let (d, e) = (dev.get(name), emu.get(name));
            match c.tolerance.get(name) {
                Some(None) => {}
                Some(Some(pct)) => {
                    let num = |v: Option<&&str>| -> f64 {
                        v.and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                            panic!("{test}: {id}: {} {name} is not a number", c.key)
                        })
                    };
                    let (d, e) = (num(d), num(e));
                    assert!(
                        (e - d).abs() <= d.abs() * pct / 100.0,
                        "{test}: {id}: {} {name}: the record reads {e} against the device's {d} \
                         (cited within {pct} %)",
                        c.key
                    );
                }
                None => assert_eq!(e, d, "{test}: {id}: {} {name} (record, device)", c.key),
            }
        }
    }
    println!(
        "RAN {test}: {} rows, {} settled citing {} capture lines, each at its line and matched by \
         the record within the cited tolerances",
        rows.len(),
        counts[1] + counts[2] + counts[3],
        cited.len()
    );
}

/// The captures of the Part B probe builds (main 723d08f5): `probe_campaign_regs` with the gate,
/// TIMG_REGCLK and ADC lines, and `probe_campaign_reset` with the RTC_CNTL words after the
/// super-watchdog reset and the wake.
const LANE4_REGS_CAPTURE: &str = "device-probe_campaign_regs-20260926T024930Z-run1.clean.log";
const LANE4_RESET_CAPTURE: &str = "device-probe_campaign_reset-20260925T174549Z-run1.log";

/// The step 5 rows settled from the Part B captures print on the device what the committed
/// records print (the note's "Step 5").
///
/// Line for line: every `GATE` line of LEDC, I2C0, SPI2 and SHA (`wiring::gates`), the I2S0 and
/// AES latch lines, `GATE|timg<n>_regclk`, `GATE|timg1_regclk_count`, and the 38
/// `REG|rtc_cntl.<word>.boot2` and `.boot3` lines of the reset probe.
///
/// Field by field, where part of the line is CPU timing rather than the gate:
/// - `GATE|timg0_regclk_count`: `on`, `off` and `later` within 1 us (device 201, 406 and 608,
///   model 202, 405 and 607); `off - on`, whether a clear CLK_EN stops the counter, is 200 us
///   within 10 us on both sides for both groups.
/// - `TIME|adc_oneshot_read` (`adc_conversion_ps`): `us` and `cycles` within 0.5 % (3029 us and
///   484360 cycles on the device, 3026 and 484026 here: the poll loop moves in 13-cycle steps).
#[test]
fn t1_campaign_gate_and_adc_rows_match_the_device_capture() {
    let test = "t1_campaign_gate_and_adc_rows_match_the_device_capture";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let read = |name: &str| std::fs::read_to_string(root.join("captures").join(name)).ok();
    let (Some(regs), Some(reset)) = (read(LANE4_REGS_CAPTURE), read(LANE4_RESET_CAPTURE)) else {
        common::skip(
            test,
            "no Part B capture of probe_campaign_regs or probe_campaign_reset",
        );
        return;
    };
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let record = |name: &str| {
        facts(
            &std::fs::read_to_string(repo.join(format!("tests/fw/campaign/{name}.emu.txt")))
                .expect("the emulator record is committed"),
        )
    };
    let (dev_regs, emu_regs) = (facts(&regs), record("probe_campaign_regs"));
    let (dev_reset, emu_reset) = (facts(&reset), record("probe_campaign_reset"));

    let exact = |key: &str| {
        let Some((tag, fact)) = key.split_once('|') else {
            return false;
        };
        match tag {
            "GATE" => {
                ["ledc_", "i2c0_", "spi2_", "sha_"]
                    .iter()
                    .any(|p| fact.starts_with(p))
                    || [
                        "i2s0_latch",
                        "aes_latch",
                        "timg0_regclk",
                        "timg1_regclk",
                        "timg1_regclk_count",
                    ]
                    .contains(&fact)
            }
            "REG" => {
                fact.starts_with("rtc_cntl.")
                    && (fact.ends_with(".boot2") || fact.ends_with(".boot3"))
            }
            _ => false,
        }
    };
    let mut compared = 0;
    for (capture, dev, emu) in [
        (LANE4_REGS_CAPTURE, &dev_regs, &emu_regs),
        (LANE4_RESET_CAPTURE, &dev_reset, &emu_reset),
    ] {
        for (key, line) in dev.iter().filter(|(k, _)| exact(k)) {
            let got = emu
                .get(key)
                .unwrap_or_else(|| panic!("{test}: the record prints no {key} ({capture})"));
            assert_eq!(got, line, "{test}: {key} ({capture})");
            compared += 1;
        }
    }
    // 4 blocks of 5 GATE lines, 2 latch lines, 2 regclk, 1 regclk count; 19 words at 2 boots.
    assert_eq!(
        compared,
        4 * 5 + 2 + 2 + 1 + 19 * 2,
        "{test}: lines compared"
    );

    let num = |map: &BTreeMap<String, String>, key: &str, f: &str| -> f64 {
        let line = map.get(key).unwrap_or_else(|| panic!("{test}: no {key}"));
        fields(line)
            .get(f)
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{test}: {key} has no numeric {f}"))
    };
    let key = "GATE|timg0_regclk_count";
    for f in ["step_us", "back", "latched"] {
        assert_eq!(
            num(&emu_regs, key, f),
            num(&dev_regs, key, f),
            "{test}: {key} {f}"
        );
    }
    for f in ["on", "off", "later"] {
        let (d, e) = (num(&dev_regs, key, f), num(&emu_regs, key, f));
        assert!((e - d).abs() <= 1.0, "{test}: {key} {f}: {e} against {d}");
    }
    for key in ["GATE|timg0_regclk_count", "GATE|timg1_regclk_count"] {
        for (side, map) in [("device", &dev_regs), ("record", &emu_regs)] {
            let ran = num(map, key, "off") - num(map, key, "on");
            assert!(
                (ran - 200.0).abs() <= 10.0,
                "{test}: {key} ({side}) counted {ran} us of the 200 us with CLK_EN clear"
            );
        }
    }
    let key = "TIME|adc_oneshot_read";
    for f in ["reads", "ok"] {
        assert_eq!(
            num(&emu_regs, key, f),
            num(&dev_regs, key, f),
            "{test}: {key} {f}"
        );
    }
    for f in ["us", "cycles"] {
        let (d, e) = (num(&dev_regs, key, f), num(&emu_regs, key, f));
        assert!(
            (e - d).abs() <= d * 0.005,
            "{test}: {key} {f}: the record reads {e} against the device's {d}"
        );
    }
    println!(
        "RAN {test}: {compared} lines equal to the device, TIMG_REGCLK and the ADC time within \
         their tolerances"
    );
}
