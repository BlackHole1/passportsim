//! `cargo xtask oracle`: oracle runs and the checks on their output.
//!
//! Oracle runs are macOS-only (the oracle is a patched arm64 QEMU on Homebrew libraries); a
//! subcommand that starts one or reads the preserved device capture refuses elsewhere with
//! [`MACOS_ONLY`]. Checks of committed data run on every host:
//!
//! | Subcommand | Needs an oracle | Host |
//! |---|---|---|
//! | `help` | no | any |
//! | `regions --check` | no | any |
//! | `known-diffs --check` | no | any |
//! | `hist --check` | no | any |
//! | `counts --file <path>` | no | any |
//! | `hist --regen --trace <file>` | no, but it rewrites committed data | macOS |
//! | `timing-calib --ref <f> --emu <f>` | reads preserved logs | macOS |
//! | `goldens --derive …` | reads the preserved reference boot | macOS |
//! | `consoles` | yes | macOS |
//! | `consoles --print` | no | macOS |
//! | `boot-trace [--images L]` | yes | macOS |
//! | `diff [--images L]` | no, reads the `boot-trace` record | any host with that record and the corpus |
//!
//! A console run captures UART0, UART1 and USB Serial/JTAG: `<id>.console` holds the 9 ROM lines,
//! `<id>.usj.console` the rest, which is "the console" a milestone test compares. T0 runs
//! `oracle-regions`, `oracle-known-diffs` and `oracle-hist` (the coverage gate); T2 `oracle-diffs`
//! runs `oracle diff` over the bootloader phase (`phase.rs`).

mod phase;

pub(crate) use phase::{PHASE_DIR, oracle_record as phase_record};

/// The corpus ids a bootloader-phase record is made for (`oracle boot-trace`).
pub(crate) const PHASE_IMAGES: [&str; 2] = [phase::IMAGES[0].0, phase::IMAGES[1].0];

use std::fs;
use std::path::{Path, PathBuf};

use pemu_verify::goldens;
use pemu_verify::hist::{self, Allow, Hist};
use pemu_verify::known_diffs::KnownDiffs;
use pemu_verify::qemu_ingest::{self, RegionMap};

/// Parsed command line: `--key value` pairs and switches, then the positional arguments.
type Args = (Vec<(String, String)>, Vec<String>);

/// The refusal on a non-macOS host.
pub const MACOS_ONLY: &str = "oracle runs are macOS-only";

const USAGE: &str = "\
usage: cargo xtask oracle <subcommand> [args]

  help                                    this text (any host)
  regions --check                         parse and validate specs/oracle-qemu-regions.toml
  known-diffs --check                     parse and validate specs/oracle-known-diffs.toml
  hist --check [--trace F] [--baseline F] rebuild a histogram and run the coverage gate
  counts --file <path>                    read a stored instruction-count file
  hist --regen --trace F --baseline F     rewrite a committed histogram (macOS)
  timing-calib --ref F --emu F            timestamped-line coverage of two captures (macOS)
  goldens --derive --input F --out D      derive a golden from a capture (macOS; see goldens.rs)
  consoles [--config F] [--images L]      regenerate the oracle consoles (macOS, needs QEMU)
  consoles --print [--images L]           print the configuration a run would use (L: id,id)
  boot-trace [--images L]                 record the oracle's bootloader phase (macOS, needs QEMU)
  diff [--images L]                       our bootloader phase against that record

common: --root <dir> (default: the workspace holding this xtask crate)";

/// Committed data the checks read, relative to the repository root.
const REGIONS: &str = "specs/oracle-qemu-regions.toml";
const KNOWN_DIFFS: &str = "specs/oracle-known-diffs.toml";
const SAMPLE_TRACE: &str = "tools/oracle/fixtures/hist-sample.trace";
const SAMPLE_HIST: &str = "tools/oracle/fixtures/hist-sample.hist";

/// The real coverage baseline: every `(block, offset)` the `pk` oracle run touched, from the
/// regenerated consoles. The fixture above is the gate's own unit-test pair and covers
/// 44 touches of a synthetic trace; this one covers 505 of a real ROM and app boot, and is the
/// baseline a new touch fails CI against.
pub(crate) const PK_HIST: &str = "tools/oracle/pk-rom.hist";

/// Where the trace that baseline came from is regenerated to, below the data root.
pub(crate) const PK_TRACE: &str = "oracles/consoles/pk.trace";

/// Entry point of `cargo xtask oracle`.
pub fn run(args: &[String]) -> Result<(), String> {
    let (flags, positional) = split_args(args)?;
    let root = match flags.iter().find(|(key, _)| key == "root") {
        Some((_, value)) => PathBuf::from(value),
        None => crate::util::workspace_root(),
    };
    if has(&flags, "help") || positional.iter().any(|arg| arg == "help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    match positional.first().map(String::as_str) {
        None => {
            // With nothing to do, `xtask oracle` still refuses off macOS, so every other host
            // sees the same refusal.
            refuse_off_macos()?;
            Err(format!("no subcommand\n{USAGE}"))
        }
        Some("regions") => check_regions(&root),
        Some("known-diffs") => check_known_diffs(&root),
        Some("hist") => hist_command(&root, &flags),
        Some("counts") => counts_command(&root, &flags),
        Some("timing-calib") => timing_calib(&flags),
        Some("goldens") => {
            refuse_off_macos()?;
            crate::goldens::run(&root, &flags)
        }
        Some("consoles") => consoles(&root, &flags),
        Some("boot-trace") => phase::boot_trace(&flags),
        Some("diff") => phase::diff(&root, &flags),
        Some(other) => Err(format!("unknown subcommand `{other}`\n{USAGE}")),
    }
}

/// Refuses on any host but macOS with [`MACOS_ONLY`].
fn refuse_off_macos() -> Result<(), String> {
    if std::env::consts::OS != "macos" {
        return Err(MACOS_ONLY.to_string());
    }
    Ok(())
}

/// Every `--key` this command line takes with no value. A switch missing from this list would
/// swallow the next argument, so the list is the one place a new switch has to be added, and
/// `tests::every_switch_the_usage_texts_document_is_valueless` holds it to the USAGE strings.
pub(crate) const SWITCHES: &[&str] = &[
    "check",
    "regen",
    "record",
    "derive",
    "help",
    "all-boots",
    "allow-in-repo",
    // `oracle consoles --print` resolves and prints without starting an oracle.
    "print",
];

/// Splits `--key value` flags, `--key` switches and positional arguments.
pub(crate) fn split_args(args: &[String]) -> Result<Args, String> {
    let mut flags = Vec::new();
    let mut positional = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.strip_prefix("--") {
            None => positional.push(arg.clone()),
            Some(key) if SWITCHES.contains(&key) => {
                flags.push((key.to_string(), String::new()));
            }
            Some(key) => {
                let value = rest
                    .next()
                    .ok_or_else(|| format!("`--{key}` needs a value\n{USAGE}"))?;
                flags.push((key.to_string(), value.clone()));
            }
        }
    }
    Ok((flags, positional))
}

/// The value of a flag.
fn flag<'a>(flags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    flags
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// Whether a switch is present.
fn has(flags: &[(String, String)], key: &str) -> bool {
    flags.iter().any(|(name, _)| name == key)
}

/// Reads a file, naming it in the error.
fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))
}

/// `oracle regions --check`.
fn check_regions(root: &Path) -> Result<(), String> {
    let path = root.join(REGIONS);
    let map = RegionMap::parse(&read(&path)?).map_err(|err| format!("{REGIONS} {err}"))?;
    let trusted = map.blocks.iter().filter(|block| block.trusted).count();
    let covered: usize = map.regions.values().map(|region| region.blocks.len()).sum();
    println!(
        "oracle regions: {} blocks ({trusted} trusted), {} region names covering {covered} named \
         blocks, no overlap",
        map.blocks.len(),
        map.regions.len()
    );
    Ok(())
}

/// `oracle known-diffs --check`.
fn check_known_diffs(root: &Path) -> Result<(), String> {
    let path = root.join(KNOWN_DIFFS);
    let text = read(&path)?;
    let diffs = KnownDiffs::parse(&text).map_err(|err| format!("{KNOWN_DIFFS} {err}"))?;
    let allow = Allow::parse_list(&text).map_err(|err| format!("{KNOWN_DIFFS} {err}"))?;
    let regions =
        RegionMap::parse(&read(&root.join(REGIONS))?).map_err(|err| format!("{REGIONS} {err}"))?;
    for entry in &diffs.entries {
        if let pemu_verify::known_diffs::Scope::Mmio { block, .. } = &entry.scope
            && regions.block(block).is_none()
        {
            return Err(format!(
                "{KNOWN_DIFFS}: known diff `{}` names block `{block}`, which {REGIONS} does not",
                entry.id
            ));
        }
    }
    for entry in &allow {
        if regions.block(&entry.block).is_none() {
            return Err(format!(
                "{KNOWN_DIFFS}: allowlist entry names block `{}`, which {REGIONS} does not",
                entry.block
            ));
        }
    }
    println!(
        "oracle known-diffs: {} listed differences, {} allowlist entries, every block known",
        diffs.entries.len(),
        allow.len()
    );
    Ok(())
}

/// `oracle hist --check` and `oracle hist --regen`.
fn hist_command(root: &Path, flags: &[(String, String)]) -> Result<(), String> {
    let regen = has(flags, "regen");
    if regen {
        refuse_off_macos()?;
    }
    if !regen && !has(flags, "check") {
        return Err(format!("`oracle hist` needs --check or --regen\n{USAGE}"));
    }
    let trace_path = root.join(flag(flags, "trace").unwrap_or(SAMPLE_TRACE));
    let baseline_path = root.join(flag(flags, "baseline").unwrap_or(SAMPLE_HIST));
    // A histogram's label is what a reader has to trust when the gate fires, so it names the
    // trace it was built from rather than defaulting to the fixture's label on every run.
    let label = match flag(flags, "label") {
        Some(label) => label.to_string(),
        None => match flag(flags, "trace") {
            None => "hist-sample (synthetic trace)".to_string(),
            Some(trace) => Path::new(trace)
                .file_name()
                .map_or_else(|| trace.to_string(), |name| name.to_string_lossy().into()),
        },
    };
    let map =
        RegionMap::parse(&read(&root.join(REGIONS))?).map_err(|err| format!("{REGIONS} {err}"))?;
    let ingest = qemu_ingest::ingest(&read(&trace_path)?, &map);
    if !ingest.malformed.is_empty() {
        return Err(format!(
            "{}: {} unparsable trace line(s), first at line {}",
            trace_path.display(),
            ingest.malformed.len(),
            ingest.malformed[0]
        ));
    }
    if !ingest.unmapped.is_empty() {
        return Err(format!(
            "{}: {} access(es) outside every block window, first at {:#010x}",
            trace_path.display(),
            ingest.unmapped.len(),
            ingest.unmapped[0].1.addr
        ));
    }
    if let Some((region, block)) = ingest.region_block_mismatch.iter().next() {
        return Err(format!(
            "{}: region `{region}` reached block `{block}`, which {REGIONS} does not list it as \
             covering ({} such pair(s)). The QEMU build re-split a region: fix the `blocks` list \
             of that row, or the block windows, before trusting the histogram",
            trace_path.display(),
            ingest.region_block_mismatch.len()
        ));
    }
    if !ingest.unknown_regions.is_empty() {
        return Err(format!(
            "{}: {} region name(s) {REGIONS} does not list, first `{}`",
            trace_path.display(),
            ingest.unknown_regions.len(),
            ingest
                .unknown_regions
                .iter()
                .next()
                .expect("the set is not empty")
        ));
    }
    let observed = Hist::from_ingest(&label, &ingest);
    if regen {
        fs::write(&baseline_path, observed.to_text())
            .map_err(|err| format!("{}: {err}", baseline_path.display()))?;
        println!(
            "oracle hist: wrote {} ({} touches)",
            baseline_path.display(),
            observed.entries.len()
        );
        return Ok(());
    }
    let baseline = Hist::parse(&read(&baseline_path)?)
        .map_err(|err| format!("{} {err}", baseline_path.display()))?;
    let allow = Allow::parse_list(&read(&root.join(KNOWN_DIFFS))?)
        .map_err(|err| format!("{KNOWN_DIFFS} {err}"))?;
    let new = hist::new_touches(&baseline, &observed, &allow);
    for touch in &new {
        println!("oracle hist: new touch {touch}");
    }
    for (block, offset) in hist::vanished_touches(&baseline, &observed) {
        println!("oracle hist: touch no longer made: {block} {offset:#07x}");
    }
    if !new.is_empty() {
        return Err(format!(
            "{} new (block, offset) touch(es); each needs a RegSpec with a class other than U or \
             an allowlist entry with a reason in {KNOWN_DIFFS}",
            new.len()
        ));
    }
    println!(
        "oracle hist: {} touches over {} blocks, no new touch ({} allowlist entries)",
        observed.entries.len(),
        observed.blocks().len(),
        allow.len()
    );
    Ok(())
}

/// `oracle counts --file <path>` and `oracle counts --record`.
fn counts_command(root: &Path, flags: &[(String, String)]) -> Result<(), String> {
    if has(flags, "record") {
        refuse_off_macos()?;
        return Err(
            "recording instruction counts needs an esp32sim run; see tools/oracle/README.md \
             section `counts` for the command, then commit the file it writes"
                .to_string(),
        );
    }
    let path = flag(flags, "file")
        .map(|value| root.join(value))
        .ok_or_else(|| format!("`oracle counts` needs --file <path>\n{USAGE}"))?;
    let counts =
        goldens::parse_counts(&read(&path)?).map_err(|err| format!("{} {err}", path.display()))?;
    for (image, reference) in goldens::ESP32SIM_APP_MAIN {
        let key = ((*image).to_string(), "app_main".to_string());
        match counts.get(&key) {
            Some(stored) => {
                println!("oracle counts: {image} app_main {stored} stored, {reference} reference")
            }
            None => println!("oracle counts: {image} app_main not recorded"),
        }
    }
    println!("oracle counts: {} entries", counts.len());
    Ok(())
}

/// `oracle timing-calib --ref <file> --emu <file>`: the timestamped-line coverage of a reference
/// capture against an emulator capture. Reads the preserved logs, so macOS-only.
fn timing_calib(flags: &[(String, String)]) -> Result<(), String> {
    refuse_off_macos()?;
    let reference = flag(flags, "ref")
        .ok_or_else(|| format!("`oracle timing-calib` needs --ref <path>\n{USAGE}"))?;
    let emulated = flag(flags, "emu")
        .ok_or_else(|| format!("`oracle timing-calib` needs --emu <path>\n{USAGE}"))?;
    let reference = fs::read(reference).map_err(|err| format!("{reference}: {err}"))?;
    let emulated = fs::read(emulated).map_err(|err| format!("{emulated}: {err}"))?;
    let coverage = goldens::timestamped_coverage(&reference, &emulated);
    println!(
        "oracle timing-calib: reference timestamped lines {}, emulator {}, matched {} of {}",
        coverage.total, coverage.emulated, coverage.matched, coverage.total
    );
    Ok(())
}

/// `oracle consoles`: regenerate every oracle console with the pinned configuration.
///
/// The pinned `qemu-oracle` inputs and the images live under the preserved data root, so the
/// configuration is resolved, not committed: the oracle tree gives the binary, ROM and eFuse
/// image, `~/.config/passportsim/corpus.toml` one image per id. The values go to
/// `<data root>/oracles/consoles/qemu-oracle.env`, and `tools/oracle/regen-consoles.sh` runs on it.
/// `--config F` uses an operator's own env file verbatim; `--print` resolves and prints without
/// starting an oracle, which shows a fresh checkout which pin or image is missing.
fn consoles(root: &Path, flags: &[(String, String)]) -> Result<(), String> {
    refuse_off_macos()?;
    let driver = root.join(DRIVER);
    if !driver.exists() {
        return Err(format!("{} is missing", driver.display()));
    }
    let env_path = match flag(flags, "config") {
        Some(config) => {
            let path = root.join(config);
            if !path.exists() {
                return Err(format!(
                    "{} is missing; copy tools/oracle/qemu-oracle.env.example and fill in the \
                     paths of the pinned `qemu-oracle` inputs, or drop --config and let \
                     `oracle consoles` resolve them",
                    path.display()
                ));
            }
            path
        }
        None => {
            let wanted = flag(flags, "images").map(|list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
            let home = crate::hostdirs::home()?;
            let config = match fs::read_to_string(home.join(crate::hostdirs::CONFIG_FILE)) {
                Ok(text) => Some(text),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => {
                    return Err(format!(
                        "cannot read ~/{}: {}",
                        crate::hostdirs::CONFIG_FILE,
                        err.kind()
                    ));
                }
            };
            let data_root = crate::hostdirs::data_root(&home, config.as_deref())?;
            let manifest =
                fs::read_to_string(home.join(crate::ci::corpus::CORPUS_FILE)).map_err(|_| {
                    format!(
                        "~/{} not found; the image paths come from it",
                        crate::ci::corpus::CORPUS_FILE
                    )
                })?;
            let artifacts = data_root.join("artifacts/oracle");
            let plan = resolve(
                &data_root,
                &home,
                &artifacts,
                &manifest,
                wanted.as_deref(),
                &|path| path.exists(),
                &|path| corpus_sha_prefix(path),
                &make_flip_image,
            )?;
            if has(flags, "print") {
                print!("{}", plan.env());
                for note in &plan.notes {
                    println!("# {note}");
                }
                return Ok(());
            }
            for note in &plan.notes {
                eprintln!("oracle consoles: {note}");
            }
            let dir = data_root.join(CONSOLE_DIR);
            fs::create_dir_all(&dir)
                .map_err(|err| format!("{} cannot be created: {err}", dir.display()))?;
            let path = dir.join("qemu-oracle.env");
            fs::write(&path, plan.env())
                .map_err(|err| format!("{} cannot be written: {err}", path.display()))?;
            path
        }
    };
    let status = std::process::Command::new("bash")
        .arg(&driver)
        .arg(&env_path)
        .status()
        .map_err(|err| format!("bash {} cannot start: {err}", driver.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} exited {}; the console, trace and header it wrote are left in place for reading",
            driver.display(),
            status.code().unwrap_or(-1)
        ))
    }
}

/// The driver that starts the oracle; the only place a QEMU command line is built.
const DRIVER: &str = "tools/oracle/regen-consoles.sh";

/// Where a run writes, below the data root (never into the repository).
const CONSOLE_DIR: &str = "oracles/consoles";

/// Seconds a single oracle run is given. A flashed image boots and keeps running, so every run
/// is stopped by a bound; 20 s is past the boot phase every exit reads.
const TIMEOUT_S: u32 = 20;

/// Bytes of memory-region trace a single run may write. An unbounded `pk` run wrote 13 GB in ten
/// minutes, so the cap is the bound that matters on a full-speed host.
const TRACE_MAX_BYTES: u64 = 256 << 20;

/// The pinned `qemu-oracle` inputs: the path below the data root and the expected SHA-256
/// prefix. The driver hashes each file and refuses a mismatch, so these
/// prefixes are what it checks against, never a substitute for reading the file.
const QEMU_BIN: (&str, &str) = (
    "oracles/qemu-g2/qemu/build-g2/qemu-system-riscv32",
    "eb28ffc878f8b2d2",
);
const QEMU_ROM_DIR: &str = "oracles/qemu/rom/rev101-lma";
const QEMU_ROM: (&str, &str) = (
    "oracles/qemu/rom/rev101-lma/esp32c3-rom.bin",
    "8b41b9b114e110e3",
);
const QEMU_EFUSE: (&str, &str) = ("spikes/g2/efuse/v1.1-blk13-synth.bin", "555952c9daccdf75");

/// The launcher wrapper. `DYLD_LIBRARY_PATH` cannot be inherited through a
/// SIP-protected launcher, so the pinned binary is started through this script with `QEMU_BIN`
/// set; the pin is still checked against the binary itself, not against the wrapper.
const QEMU_LAUNCHER: &str = "oracles/qemu/bin/qemu-c3";

/// Consoles an exit uses, as corpus ids. `pk`, `official` and `goldminer` are the three images of
/// the boot exits, the next four are probe images, and [`FLIP_ID`] is the flipped-app negative,
/// which is not a corpus entry but a copy this command makes.
const CONSOLE_IDS: [&str; 8] = [
    "pk",
    "official",
    "goldminer",
    "probe-long",
    "probe2",
    "scan3",
    "pkgatt",
    FLIP_ID,
];

/// The flipped-app negative: a copy of `pk` with one byte flipped inside the app image, which the
/// bootloader must reject. It is made under the artifacts directory, never in the corpus and
/// never in the repository, because it is a derived file the exit rebuilds.
const FLIP_ID: &str = "pk-e2.4-flip";

/// Corpus id the flipped copy is made from.
const FLIP_SOURCE: &str = "pk";

/// Byte flipped in the copy. The app partition of the FoloToy image starts at 0x10000 (the IDF
/// default for a single-factory layout), and the offset is well inside the app image rather
/// than in its header, so the failure the bootloader reports is the SHA-256 check and not a
/// malformed-header refusal.
const FLIP_OFFSET: u64 = 0x0002_0000;

/// What a run will do: the env text plus the notes about what could not be resolved.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    lines: Vec<(String, String)>,
    notes: Vec<String>,
}

impl Plan {
    /// The env file the driver sources. Values are quoted, because corpus paths hold spaces
    /// ("Application Support").
    fn env(&self) -> String {
        let mut text = String::from(
            "# Generated by `cargo xtask oracle consoles`. Do not edit: a rerun\n\
             # overwrites it. The pins are the `qemu-oracle` constants of xtask/src/oracle.rs.\n",
        );
        for (key, value) in &self.lines {
            text.push_str(&format!("{key}=\"{value}\"\n"));
        }
        text
    }
}

/// Whether a path is present. Injected so [`resolve`] is testable without the oracle tree.
type Exists<'a> = &'a dyn Fn(&Path) -> bool;

/// First 16 hex characters of a file's SHA-256, or `None` when it cannot be read.
type ShaPrefix<'a> = &'a dyn Fn(&Path) -> Option<String>;

/// Makes the flipped copy (corpus entry, home, artifacts directory, presence test).
type FlipImage<'a> =
    &'a dyn Fn(Option<&toml::Value>, &Path, &Path, Exists<'_>) -> Result<PathBuf, String>;

/// Resolves the pinned configuration and the image list against a data root and a corpus
/// manifest. `exists` and `sha_prefix` are injected so the resolution is testable without the
/// oracle tree.
#[allow(clippy::too_many_arguments)]
fn resolve(
    data_root: &Path,
    home: &Path,
    artifacts: &Path,
    manifest: &str,
    wanted: Option<&[String]>,
    exists: Exists<'_>,
    sha_prefix: ShaPrefix<'_>,
    flip_image: FlipImage<'_>,
) -> Result<Plan, String> {
    let mut notes = Vec::new();
    let pinned = |what: &str, (rel, pin): (&str, &str)| -> Result<String, String> {
        let path = data_root.join(rel);
        if !exists(&path) {
            return Err(format!(
                "the pinned `qemu-oracle` {what} is not at <data root>/{rel}"
            ));
        }
        match sha_prefix(&path) {
            Some(got) if got == pin => Ok(path.display().to_string()),
            Some(got) => Err(format!(
                "<data root>/{rel} has sha256 prefix {got}, but the {what} is pinned to {pin}"
            )),
            None => Err(format!("<data root>/{rel} cannot be read")),
        }
    };
    let binary = pinned("binary", QEMU_BIN)?;
    let rom = pinned("ROM", QEMU_ROM)?;
    let efuse = pinned("eFuse image", QEMU_EFUSE)?;
    let launcher = data_root.join(QEMU_LAUNCHER);
    if !exists(&launcher) {
        return Err(format!(
            "the QEMU launcher wrapper is not at <data root>/{QEMU_LAUNCHER}"
        ));
    }

    let table: toml::Table = manifest
        .parse()
        .map_err(|_| "corpus.toml does not parse as TOML".to_string())?;
    let ids: Vec<String> = match wanted {
        Some(list) => list.to_vec(),
        None => CONSOLE_IDS.iter().map(|id| (*id).to_string()).collect(),
    };
    let mut images = Vec::new();
    for id in &ids {
        if id == FLIP_ID {
            match flip_image(table.get(FLIP_SOURCE), home, artifacts, exists) {
                Ok(path) => images.push(format!("{id}={}", path.display())),
                Err(note) => notes.push(note),
            }
            continue;
        }
        match table
            .get(id)
            .and_then(toml::Value::as_table)
            .and_then(|files| files.get("bin"))
            .and_then(toml::Value::as_str)
        {
            None => notes.push(format!(
                "corpus id `{id}` has no `bin` in corpus.toml; its console is not regenerated"
            )),
            Some(path) => {
                let path = crate::hostdirs::expand_home(path, home);
                if exists(&path) {
                    images.push(format!("{id}={}", path.display()));
                } else {
                    notes.push(format!(
                        "corpus id `{id}` is listed but its image is missing; its console is \
                         not regenerated"
                    ));
                }
            }
        }
    }
    if images.is_empty() {
        return Err(format!(
            "no corpus image of {ids:?} resolves; `xtask ci t1` reports which ones are missing"
        ));
    }

    let lines = vec![
        ("PEMU_QEMU_BIN".to_string(), binary),
        (
            "PEMU_QEMU_LAUNCHER".to_string(),
            launcher.display().to_string(),
        ),
        (
            "PEMU_QEMU_SHA256_PREFIX".to_string(),
            QEMU_BIN.1.to_string(),
        ),
        (
            "PEMU_QEMU_ROM_DIR".to_string(),
            data_root.join(QEMU_ROM_DIR).display().to_string(),
        ),
        ("PEMU_QEMU_ROM".to_string(), rom),
        (
            "PEMU_QEMU_ROM_SHA256_PREFIX".to_string(),
            QEMU_ROM.1.to_string(),
        ),
        ("PEMU_QEMU_EFUSE".to_string(), efuse),
        (
            "PEMU_QEMU_EFUSE_SHA256_PREFIX".to_string(),
            QEMU_EFUSE.1.to_string(),
        ),
        ("PEMU_QEMU_STRAP_MODE".to_string(), "0x0A".to_string()),
        (
            "PEMU_QEMU_STRAP_DRIVER".to_string(),
            "esp32c3.gpio".to_string(),
        ),
        (
            "PEMU_QEMU_STRAP_PROPERTY".to_string(),
            "strap_mode".to_string(),
        ),
        (
            "PEMU_QEMU_FLAGS".to_string(),
            "-M esp32c3 -display none -monitor none -icount shift=0,align=off,sleep=off"
                .to_string(),
        ),
        (
            "PEMU_QEMU_TRACE_FLAGS".to_string(),
            "-d trace:memory_region_ops_read,trace:memory_region_ops_write".to_string(),
        ),
        (
            "PEMU_ORACLE_OUT".to_string(),
            data_root.join(CONSOLE_DIR).display().to_string(),
        ),
        // The oracle never stops on its own, and its memory-region trace grows at tens of MB
        // per second; both bounds are recorded in every header the driver writes.
        ("PEMU_ORACLE_TIMEOUT".to_string(), TIMEOUT_S.to_string()),
        (
            "PEMU_ORACLE_TRACE_MAX".to_string(),
            TRACE_MAX_BYTES.to_string(),
        ),
        // One pair per line: the data root is under "Application Support", so every corpus
        // path holds a space and a space-separated list would split inside a path.
        ("PEMU_ORACLE_IMAGES".to_string(), images.join("\n")),
    ];
    Ok(Plan { lines, notes })
}

/// Makes the flipped copy: `pk` with the byte at [`FLIP_OFFSET`] inverted, under the artifacts
/// directory.
///
/// The copy is rewritten on every run rather than reused, because a stale one would make the
/// negative pass against a source image that has since changed.
fn make_flip_image(
    entry: Option<&toml::Value>,
    home: &Path,
    artifacts: &Path,
    exists: Exists<'_>,
) -> Result<PathBuf, String> {
    let source = entry
        .and_then(toml::Value::as_table)
        .and_then(|files| files.get("bin"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| {
            format!("corpus id `{FLIP_SOURCE}` has no `bin`, so the flipped copy cannot be made")
        })?;
    let source = crate::hostdirs::expand_home(source, home);
    if !exists(&source) {
        return Err(format!(
            "corpus id `{FLIP_SOURCE}` is listed but its image is missing, so the flipped copy \
             cannot be made"
        ));
    }
    let mut image = fs::read(&source).map_err(|err| format!("{FLIP_SOURCE}: {err}"))?;
    let offset = usize::try_from(FLIP_OFFSET).expect("the offset fits a usize");
    if image.len() <= offset {
        return Err(format!(
            "corpus id `{FLIP_SOURCE}` is {} bytes, shorter than the flip offset {FLIP_OFFSET:#x}",
            image.len()
        ));
    }
    image[offset] ^= 0xFF;
    fs::create_dir_all(artifacts)
        .map_err(|err| format!("{} cannot be created: {err}", artifacts.display()))?;
    let path = artifacts.join(format!("{FLIP_ID}.bin"));
    fs::write(&path, &image)
        .map_err(|err| format!("{} cannot be written: {err}", path.display()))?;
    Ok(path)
}

/// First 16 hex characters of a file's SHA-256, the form of the pins and of the driver's output.
fn corpus_sha_prefix(path: &Path) -> Option<String> {
    crate::ci::corpus::sha256_file(path).map(|hex| hex[..16].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        crate::util::workspace_root()
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| (*value).to_string()).collect()
    }

    /// A corpus manifest with one good id, one whose image is missing and one with no `bin`.
    fn manifest() -> &'static str {
        "[pk]\nbin = \"~/corpus/pk.bin\"\n\n[official]\nbin = \"~/corpus/gone.bin\"\n\n[goldminer]\nelf = \"~/corpus/g.elf\"\n"
    }

    fn flip_stub(
        _entry: Option<&toml::Value>,
        _home: &Path,
        artifacts: &Path,
        _exists: Exists<'_>,
    ) -> Result<PathBuf, String> {
        Ok(artifacts.join("pk-e2.4-flip.bin"))
    }

    /// Every pinned file present with the pinned hash, one image resolvable.
    fn good_plan(ids: &[&str]) -> Result<Plan, String> {
        let root = Path::new("/data");
        let home = Path::new("/home");
        let wanted: Vec<String> = ids.iter().map(|id| (*id).to_string()).collect();
        resolve(
            root,
            home,
            Path::new("/data/artifacts/oracle"),
            manifest(),
            Some(&wanted),
            &|path| path != Path::new("/home/corpus/gone.bin"),
            &|path| {
                Some(
                    match path.strip_prefix(root).map(Path::to_str) {
                        Ok(Some(QEMU_BIN_REL)) => QEMU_BIN.1,
                        Ok(Some(QEMU_ROM_REL)) => QEMU_ROM.1,
                        _ => QEMU_EFUSE.1,
                    }
                    .to_string(),
                )
            },
            &flip_stub,
        )
    }

    const QEMU_BIN_REL: &str = QEMU_BIN.0;
    const QEMU_ROM_REL: &str = QEMU_ROM.0;

    #[test]
    fn the_resolved_configuration_carries_every_pin_and_one_pair_per_line() {
        let plan = good_plan(&["pk"]).expect("the pins resolve");
        let env = plan.env();
        // The driver checks each pin against the file it hashes, so all three must reach it.
        assert!(env.contains(&format!("PEMU_QEMU_SHA256_PREFIX=\"{}\"", QEMU_BIN.1)));
        assert!(env.contains(&format!("PEMU_QEMU_ROM_SHA256_PREFIX=\"{}\"", QEMU_ROM.1)));
        assert!(env.contains(&format!(
            "PEMU_QEMU_EFUSE_SHA256_PREFIX=\"{}\"",
            QEMU_EFUSE.1
        )));
        // One `id=path` pair per line: a space-separated list splits inside "Application Support".
        // The path is compared as a path: on Windows the `~/corpus/pk.bin` of the manifest expands
        // to `/home\corpus/pk.bin`, the same path written with that host's separator.
        let image = env
            .lines()
            .find_map(|line| line.strip_prefix("PEMU_ORACLE_IMAGES=\"pk="))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("one `pk=<path>` pair");
        assert_eq!(Path::new(image), Path::new("/home/corpus/pk.bin"));
        assert!(env.contains("PEMU_QEMU_STRAP_MODE=\"0x0A\""));
        assert!(env.contains(&format!("PEMU_ORACLE_TIMEOUT=\"{TIMEOUT_S}\"")));
    }

    #[test]
    fn two_images_are_two_lines_not_two_words() {
        let plan = good_plan(&["pk", FLIP_ID]).expect("both resolve");
        let images = plan
            .lines
            .iter()
            .find(|(key, _)| key == "PEMU_ORACLE_IMAGES")
            .map(|(_, value)| value.clone())
            .expect("the list is there");
        assert_eq!(images.lines().count(), 2, "{images}");
        assert!(images.lines().all(|line| line.contains('=')));
    }

    #[test]
    fn a_missing_or_unlisted_image_is_a_note_and_not_a_failure() {
        let plan = good_plan(&["pk", "official", "goldminer"]).expect("`pk` still resolves");
        assert_eq!(plan.notes.len(), 2, "{:?}", plan.notes);
        assert!(plan.notes.iter().any(|note| note.contains("`official`")));
        assert!(plan.notes.iter().any(|note| note.contains("`goldminer`")));
    }

    #[test]
    fn no_resolvable_image_is_a_failure_rather_than_an_empty_run() {
        let err = good_plan(&["official"]).expect_err("nothing to run");
        assert!(err.contains("no corpus image"), "{err}");
    }

    #[test]
    fn a_pin_that_does_not_match_refuses_before_the_oracle_starts() {
        let err = resolve(
            Path::new("/data"),
            Path::new("/home"),
            Path::new("/data/artifacts/oracle"),
            manifest(),
            Some(&["pk".to_string()]),
            &|_| true,
            &|_| Some("0000000000000000".to_string()),
            &flip_stub,
        )
        .expect_err("a wrong hash is refused");
        assert!(err.contains("is pinned to"), "{err}");
    }

    #[test]
    fn a_missing_pinned_file_names_the_path_below_the_data_root() {
        let err = resolve(
            Path::new("/data"),
            Path::new("/home"),
            Path::new("/data/artifacts/oracle"),
            manifest(),
            Some(&["pk".to_string()]),
            &|path| path != Path::new("/data").join(QEMU_EFUSE.0),
            &|path| {
                Some(
                    match path.strip_prefix("/data").map(Path::to_str) {
                        Ok(Some(QEMU_BIN_REL)) => QEMU_BIN.1,
                        Ok(Some(QEMU_ROM_REL)) => QEMU_ROM.1,
                        _ => QEMU_EFUSE.1,
                    }
                    .to_string(),
                )
            },
            &flip_stub,
        )
        .expect_err("a missing eFuse image is refused");
        assert!(err.contains(QEMU_EFUSE.0), "{err}");
    }

    #[test]
    fn the_console_set_is_the_one_the_plan_names() {
        // pk, official, goldminer, the flipped copy and the probes.
        for id in ["pk", "official", "goldminer", FLIP_ID] {
            assert!(CONSOLE_IDS.contains(&id), "{id} is not regenerated");
        }
        assert_eq!(CONSOLE_IDS.len(), 8);
    }

    #[test]
    fn help_needs_no_oracle_and_no_host_check() {
        run(&args(&["help"])).expect("`oracle help` runs anywhere");
        run(&args(&["--help"])).expect("`oracle --help` runs anywhere");
    }

    #[test]
    fn the_committed_region_map_and_known_diffs_check_out() {
        let root = root();
        check_regions(&root).expect("the committed region map is valid");
        check_known_diffs(&root).expect("the committed known-diffs file is valid");
    }

    #[test]
    fn the_hist_gate_passes_over_the_committed_sample() {
        hist_command(&root(), &[("check".to_string(), String::new())]).expect("the gate passes");
    }

    #[test]
    fn the_hist_gate_fails_a_trace_that_touches_something_new() {
        let root = root();
        let extra = root.join("target/oracle-hist-extra.trace");
        let mut text = fs::read_to_string(root.join(SAMPLE_TRACE)).expect("the sample is there");
        text.push_str(
            "memory_region_ops_write cpu 0 mr 0x1 addr 0x6002d024 value 0x1 size 4 name \
             'esp32c3.iomem'\n",
        );
        fs::create_dir_all(extra.parent().expect("target/")).expect("target/ exists");
        fs::write(&extra, text).expect("writes the temporary trace");
        let err = hist_command(
            &root,
            &[
                ("check".to_string(), String::new()),
                (
                    "trace".to_string(),
                    extra
                        .strip_prefix(&root)
                        .expect("under the root")
                        .display()
                        .to_string(),
                ),
            ],
        )
        .expect_err("a new touch fails the gate");
        assert!(err.contains("new (block, offset) touch"), "{err}");
    }

    #[test]
    fn hist_needs_a_mode() {
        let err = hist_command(&root(), &[]).expect_err("neither --check nor --regen");
        assert!(err.contains("--check or --regen"), "{err}");
    }

    #[test]
    fn an_unknown_subcommand_is_named() {
        let err = run(&args(&["nope"])).expect_err("refused");
        assert!(err.contains("unknown subcommand `nope`"), "{err}");
    }

    #[test]
    fn the_macos_refusal_message_is_the_one_arch_fixes() {
        assert_eq!(MACOS_ONLY, "oracle runs are macOS-only");
    }

    #[test]
    fn flags_take_values_and_switches_do_not() {
        let (flags, positional) =
            split_args(&args(&["hist", "--check", "--trace", "a/b.trace"])).expect("parses");
        assert_eq!(positional, vec!["hist".to_string()]);
        assert_eq!(flag(&flags, "trace"), Some("a/b.trace"));
        assert!(has(&flags, "check"));
        assert!(split_args(&args(&["--trace"])).is_err());
    }

    #[test]
    fn the_goldens_switches_do_not_swallow_the_next_argument() {
        // `--allow-in-repo` and `--all-boots` are documented as switches in goldens.rs; a
        // switch missing from SWITCHES either eats the flag after it or fails outright.
        let (flags, positional) = split_args(&args(&[
            "goldens",
            "--derive",
            "--all-boots",
            "--allow-in-repo",
            "--strap",
            "0x0a",
        ]))
        .expect("the documented command line parses");
        assert_eq!(positional, vec!["goldens".to_string()]);
        assert!(has(&flags, "all-boots"));
        assert!(has(&flags, "allow-in-repo"));
        assert_eq!(flag(&flags, "strap"), Some("0x0a"), "{flags:?}");
        // And last on the line, where a value-taking flag would have failed.
        let (flags, _) = split_args(&args(&["goldens", "--strap", "0x0a", "--allow-in-repo"]))
            .expect("a trailing switch parses");
        assert!(has(&flags, "allow-in-repo"));
    }

    #[test]
    fn every_switch_the_usage_texts_document_is_valueless() {
        // The usage texts are what an operator types. A `--flag` they show with no placeholder
        // after it must be in SWITCHES, or the command line they document cannot be run.
        for usage in [USAGE, crate::goldens::USAGE] {
            let tokens: Vec<&str> = usage
                .split_whitespace()
                .map(|token| token.trim_matches(['[', ']', ',']))
                .collect();
            for (at, token) in tokens.iter().enumerate() {
                let Some(key) = token.strip_prefix("--") else {
                    continue;
                };
                let takes_value = tokens.get(at + 1).is_some_and(|next| {
                    next.starts_with('<')
                        || (next.len() <= 2 && next.chars().all(|c| c.is_ascii_uppercase()))
                });
                if !takes_value {
                    assert!(
                        SWITCHES.contains(&key),
                        "`--{key}` is documented as a switch but split_args would take a value \
                         for it"
                    );
                }
            }
        }
    }
}
