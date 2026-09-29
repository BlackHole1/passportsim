//! `cargo xtask oracle boot-trace` and `cargo xtask oracle diff`: the two halves of the
//! bootloader-phase oracle comparison (`pemu_verify::phase`).
//!
//! - `boot-trace` runs the pinned QEMU oracle as a black box with its own `-d exec,nochain` and
//!   `trace:memory_region_ops_*` log output, filters the log into a phase record while it streams
//!   ([`pemu_verify::phase::ExecFilter`]) and stops the oracle at the app's `call_start_cpu0`. The
//!   record goes to `<data root>/oracles/phase/<id>.boot.qemu`, never into the repository
//!   (`CONTRIBUTING.md#clean-room`). macOS only, like every oracle run.
//! - `diff` runs our machine over the same image to the same entry
//!   (`pemu_testkit::oracle_run::record_phase`), writes our record beside the oracle's as
//!   `<id>.boot.pemu` for reading, and compares the two with `specs/oracle-known-diffs.toml`
//!   applied. It fails on any unexplained divergence. It needs the oracle record and the corpus,
//!   not the oracle, so it runs wherever both are (the T2 `oracle-diffs` step).
//!
//! The pins, the launcher and the fixed flags are resolved by the same [`super::resolve`] that
//! `oracle consoles` uses, so the two commands cannot drift apart on what "the oracle" is.

use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pemu_loader::elf::ElfInfo;
use pemu_verify::known_diffs::KnownDiffs;
use pemu_verify::phase::{self, ExecFilter, PhaseRecord};
use pemu_verify::qemu_ingest::RegionMap;

use super::{KNOWN_DIFFS, REGIONS, flag, read, refuse_off_macos};

/// Where both records live, below the data root.
pub(crate) const PHASE_DIR: &str = "oracles/phase";

/// The images a bootloader-phase comparison runs over by default: the corpus ids that carry a
/// bootloader ELF, with their merged image and app ELF file names.
pub(crate) const IMAGES: [(&str, &str, &str); 2] = [
    (
        "pk",
        "FoloToy-AI-Passport-8MB.bin",
        "FoloToy-AI-Passport.elf",
    ),
    (
        "official",
        "FoloToy-AI-Passport-8MB.bin",
        "FoloToy-AI-Passport.elf",
    ),
];

/// The bootloader ELF's file name in every corpus id that has one.
const BOOT_ELF: &str = "bootloader.elf";

/// The oracle name `specs/oracle-known-diffs.toml` entries scope to for the pinned QEMU binary.
const ORACLE: &str = "qemu";

/// Log lines a `boot-trace` run may print before the app entry. A `pk` boot prints about 3.3 M
/// by then; the bound only stops a run whose image never reaches its app.
const MAX_LOG_LINES: u64 = 200_000_000;

/// Instructions our machine is given to reach the app entry; the bootloader phase of `pk` ends
/// near 6 M (6.10 M to `Loaded app`).
const MAX_INSNS: u64 = 200_000_000;

/// The oracle record of `id`.
pub(crate) fn oracle_record(data_root: &Path, id: &str) -> PathBuf {
    data_root.join(PHASE_DIR).join(format!("{id}.boot.qemu"))
}

/// Our record of `id`.
fn our_record(data_root: &Path, id: &str) -> PathBuf {
    data_root.join(PHASE_DIR).join(format!("{id}.boot.pemu"))
}

/// The data root the way `oracle consoles` resolves it: `~/.config/passportsim/config.toml`
/// when present, else the default.
fn data_root() -> Result<(PathBuf, PathBuf), String> {
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
    let root = crate::hostdirs::data_root(&home, config.as_deref())?;
    Ok((home, root))
}

/// The images a command runs over: `--images a,b`, or every one of [`IMAGES`].
fn images(
    flags: &[(String, String)],
) -> Result<Vec<(&'static str, &'static str, &'static str)>, String> {
    let Some(list) = flag(flags, "images") else {
        return Ok(IMAGES.to_vec());
    };
    list.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| {
            IMAGES
                .iter()
                .find(|(known, ..)| *known == id)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "`{id}` has no bootloader ELF in the corpus; the bootloader phase is \
                         recorded for {:?}",
                        IMAGES.map(|(id, ..)| id)
                    )
                })
        })
        .collect()
}

/// The verified corpus files of one image: the merged image, the bootloader ELF and the app ELF.
struct Subject {
    image: PathBuf,
    image_sha: String,
    boot: ElfInfo,
    app: ElfInfo,
}

/// Locates and verifies an image's files (`pemu_testkit::corpus`: a present file that is not the
/// pinned one is an error, never a skip).
fn subject(data_root: &Path, (id, bin, app): (&str, &str, &str)) -> Result<Subject, String> {
    let located = pemu_testkit::corpus::locate_id_at(data_root, id);
    if let Some(reason) = located.failure_reason().or_else(|| located.skip_reason()) {
        return Err(reason);
    }
    let file = |name: &str| {
        located
            .file(name)
            .ok_or_else(|| format!("corpus id `{id}` has no file `{name}`"))
    };
    let elf = |name: &str| -> Result<ElfInfo, String> {
        let bytes = fs::read(&file(name)?.path).map_err(|e| format!("{id}/{name}: {e}"))?;
        ElfInfo::parse(&bytes).map_err(|e| format!("{id}/{name}: {e:?}"))
    };
    let image = file(bin)?;
    Ok(Subject {
        image: image.path.clone(),
        image_sha: image.sha256[..16].to_string(),
        boot: elf(BOOT_ELF)?,
        app: elf(app)?,
    })
}

/// `oracle boot-trace [--images L]`: record the oracle's bootloader phase.
pub(crate) fn boot_trace(flags: &[(String, String)]) -> Result<(), String> {
    refuse_off_macos()?;
    let (home, root) = data_root()?;
    let manifest = fs::read_to_string(home.join(crate::ci::corpus::CORPUS_FILE)).map_err(|_| {
        format!(
            "~/{} not found; the image paths come from it",
            crate::ci::corpus::CORPUS_FILE
        )
    })?;
    let dir = root.join(PHASE_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("{} cannot be created: {e}", dir.display()))?;
    for image in images(flags)? {
        let id = image.0;
        let wanted = [id.to_string()];
        let plan = super::resolve(
            &root,
            &home,
            &root.join("artifacts/oracle"),
            &manifest,
            Some(&wanted),
            &|path| path.exists(),
            &|path| super::corpus_sha_prefix(path),
            &super::make_flip_image,
        )?;
        let subject = subject(&root, image)?;
        let lines = record_oracle(&plan, id, &subject, &dir)?;
        println!(
            "oracle boot-trace: {id}: {lines} log lines to the app entry; wrote {}",
            oracle_record(&root, id).display()
        );
    }
    Ok(())
}

/// One oracle run of `id`, filtered into `<dir>/<id>.boot.qemu`. Returns the log lines read.
fn record_oracle(
    plan: &super::Plan,
    id: &str,
    subject: &Subject,
    dir: &Path,
) -> Result<u64, String> {
    let value = |key: &str| {
        plan.lines
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| format!("the resolved configuration has no {key}"))
    };
    let (watch, end) = phase::boot_watch(&subject.boot, &subject.app)?;
    let size = fs::metadata(&subject.image)
        .map_err(|e| format!("{id}: {e}"))?
        .len();
    if ![2u64 << 20, 4 << 20, 8 << 20, 16 << 20].contains(&size) {
        return Err(format!(
            "{id}: the image is {size} bytes, not a flash size the oracle accepts"
        ));
    }
    let mut argv: Vec<String> = value("PEMU_QEMU_FLAGS")?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let path = |p: &Path| p.display().to_string();
    argv.extend([
        "-L".into(),
        value("PEMU_QEMU_ROM_DIR")?,
        "-drive".into(),
        format!("file={},if=mtd,format=raw", path(&subject.image)),
        "-drive".into(),
        format!(
            "file={},if=none,format=raw,id=efuse",
            value("PEMU_QEMU_EFUSE")?
        ),
        "-global".into(),
        "driver=nvram.esp32c3.efuse,property=drive,value=efuse".into(),
        "-global".into(),
        format!(
            "driver={},property={},value={}",
            value("PEMU_QEMU_STRAP_DRIVER")?,
            value("PEMU_QEMU_STRAP_PROPERTY")?,
            value("PEMU_QEMU_STRAP_MODE")?
        ),
        // The oracle's own log: one line per executed translation block (`nochain`, so every
        // block execution prints) and every memory-region access, in execution order.
        "-d".into(),
        "exec,nochain,trace:memory_region_ops_read,trace:memory_region_ops_write".into(),
        "-serial".into(),
        format!("file:{}", path(&dir.join(format!("{id}.boot.uart0")))),
        "-serial".into(),
        "null".into(),
        "-serial".into(),
        format!("file:{}", path(&dir.join(format!("{id}.boot.usj")))),
    ]);
    let launcher = value("PEMU_QEMU_LAUNCHER")?;
    let command_line = std::iter::once(launcher.as_str())
        .chain(argv.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let mut child = Command::new(&launcher)
        .args(&argv)
        .env("QEMU_BIN", value("PEMU_QEMU_BIN")?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{launcher} cannot start: {e}"))?;
    let stderr = child.stderr.take().expect("stderr is piped");

    let final_path = dir.join(format!("{id}.boot.qemu"));
    let partial = dir.join(format!("{id}.boot.qemu.partial"));
    let file = fs::File::create(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
    let mut out = BufWriter::new(file);
    let header = [
        ("kind", "oracle phase record".to_string()),
        ("source", ORACLE.to_string()),
        ("image", id.to_string()),
        ("command", command_line),
        ("binary-sha256", subject.image_sha.clone()),
        ("qemu-sha256", value("PEMU_QEMU_SHA256_PREFIX")?),
        ("rom-sha256", value("PEMU_QEMU_ROM_SHA256_PREFIX")?),
        ("efuse-sha256", value("PEMU_QEMU_EFUSE_SHA256_PREFIX")?),
        ("bootloader-elf-sha256", hex8(&subject.boot.sha256)),
        ("app-elf-sha256", hex8(&subject.app.sha256)),
        ("watched", format!("{} functions", watch.len())),
        ("end", format!("{} at {end:#010x}", phase::APP_ENTRY)),
    ];
    let io = |e: std::io::Error| format!("{}: {e}", partial.display());
    out.write_all(PhaseRecord::new(ORACLE).render(&header).as_bytes())
        .map_err(io)?;

    let mut filter = ExecFilter::new(watch, end);
    let mut reader = BufReader::with_capacity(1 << 20, stderr);
    let mut buf = Vec::with_capacity(256);
    let mut lines = 0u64;
    let result = loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => {
                break Err(format!(
                    "{id}: the oracle exited after {lines} log lines without reaching {}",
                    phase::APP_ENTRY
                ));
            }
            Ok(_) => {}
            Err(e) => break Err(format!("{id}: reading the oracle log: {e}")),
        }
        lines += 1;
        if let Some(line) = filter.feed(&String::from_utf8_lossy(&buf))
            && let Err(e) = writeln!(out, "{line}")
        {
            break Err(io(e));
        }
        if filter.done() {
            break Ok(lines);
        }
        if lines >= MAX_LOG_LINES {
            break Err(format!(
                "{id}: {MAX_LOG_LINES} log lines without reaching {}",
                phase::APP_ENTRY
            ));
        }
    };
    // The oracle never stops on its own; the run ends here either way.
    let _ = child.kill();
    let _ = child.wait();
    let lines = result?;
    writeln!(
        out,
        "#!bound: stopped at {} after {lines} log lines, {} translation blocks",
        phase::APP_ENTRY,
        filter.blocks()
    )
    .map_err(io)?;
    out.flush().map_err(io)?;
    drop(out);
    fs::rename(&partial, &final_path).map_err(|e| format!("{}: {e}", final_path.display()))?;
    Ok(lines)
}

/// The first 8 bytes of a digest as the 16 hex characters every pin in this tree is written in.
fn hex8(digest: &[u8]) -> String {
    pemu_loader::hex(&digest[..digest.len().min(8)])
}

/// `oracle diff [--images L]`: our bootloader phase against the oracle record.
pub(crate) fn diff(root_dir: &Path, flags: &[(String, String)]) -> Result<(), String> {
    let (_, data_root) = data_root()?;
    let map =
        RegionMap::parse(&read(&root_dir.join(REGIONS))?).map_err(|e| format!("{REGIONS} {e}"))?;
    let known = KnownDiffs::parse(&read(&root_dir.join(KNOWN_DIFFS))?)
        .map_err(|e| format!("{KNOWN_DIFFS} {e}"))?;
    let mut failed = Vec::new();
    for image in images(flags)? {
        let id = image.0;
        let theirs_path = oracle_record(&data_root, id);
        let theirs_text = fs::read_to_string(&theirs_path).map_err(|e| {
            format!(
                "{}: {e}; `cargo xtask oracle boot-trace --images {id}` records it",
                theirs_path.display()
            )
        })?;
        let theirs = PhaseRecord::parse(ORACLE, &theirs_text)
            .map_err(|e| format!("{}: {e}", theirs_path.display()))?;
        let subject = subject(&data_root, image)?;
        let ours = our_phase(&subject)?;
        let ours_path = our_record(&data_root, id);
        let header = [
            ("kind", "emulator phase record".to_string()),
            ("source", "pemu".to_string()),
            ("image", id.to_string()),
            ("binary-sha256", subject.image_sha.clone()),
        ];
        fs::write(&ours_path, ours.render(&header))
            .map_err(|e| format!("{}: {e}", ours_path.display()))?;
        let symbolize = |pc: u32| subject.boot.symbols.func_at(pc).map(|s| s.name.clone());
        let report = phase::diff_boot_phase(&ours, &theirs, &map, &known, ORACLE, &symbolize)?;
        print!("{}", report.render(&format!("`{id}` bootloader phase")));
        if !report.is_clean() {
            failed.push(id);
        }
    }
    if failed.is_empty() {
        println!("oracle diff: the bootloader phase is clean against the oracle");
        Ok(())
    } else {
        Err(format!(
            "oracle diff: unexplained divergences in {failed:?}; each needs a fix or an entry in \
             {KNOWN_DIFFS} with its reason"
        ))
    }
}

/// Our record of an image, from power-on to the app entry, on the machine the boot tests build
/// (the bundled ROM, the synthesized eFuse, no ELF bound).
fn our_phase(subject: &Subject) -> Result<PhaseRecord, String> {
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_machine::config::{Assets, MachineConfig};
    use pemu_machine::machine::Machine;

    let (watch, end) = phase::boot_watch(&subject.boot, &subject.app)?;
    let bytes = fs::read(&subject.image).map_err(|e| format!("the image: {e}"))?;
    let flash = FlashImage::from_merged(&bytes).map_err(|e| format!("the image: {e:?}"))?;
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .map_err(|e| format!("the bundled ROM: {e:?}"))?;
    let cfg = MachineConfig {
        trace: pemu_testkit::oracle_run::phase_trace(),
        ..MachineConfig::default()
    };
    let mut m = Machine::new(cfg, assets).map_err(|e| format!("the machine: {e:?}"))?;
    pemu_testkit::oracle_run::record_phase(&mut m, &watch, end, MAX_INSNS)
}
