//! Milestone M10 tests: esptool and the pty over the USB Serial/JTAG host endpoints, monitor
//! resets, sleep, power, NFC and the USB link, the planner rehearsal and refusal matrix, and
//! `limits-flash`. Names use the prefix `t<tier>_m10_` so `xtask ci` can count them.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;

// ------------------------------------------------------------------------------------------------
// The refusal matrix without a port
// ------------------------------------------------------------------------------------------------

use pemu_loader::partitions::{Partition, ptype, subtype};
use pemu_planner::flow::{
    BackupStore, Confirmed, DevicePaths, DeviceSession, Discovery, Effects, ElicitationConfirmer,
    ElicitationPort, Elicited, FlowError, Identity, PortCandidate, SessionError, SessionOpener,
    flash_device,
};
use pemu_planner::plan::{
    ImageSource, Origin, PlanOutcome, PlanRequest, PlannedWrite, dry_run, encode_partition_table,
    plan_flash,
};
use pemu_planner::rules::{BackupEvidence, ChipRevision, DeviceFacts, Rule};

/// The committed device facts fixture: non-identity facts only.
const DEVICE_FACTS: &str = include_str!("../fixtures/device-facts.toml");

fn refusal_facts() -> DeviceFacts {
    DeviceFacts::from_toml(DEVICE_FACTS).expect("tests/fixtures/device-facts.toml parses")
}

/// A merged image with the fixture's own layout: a synthetic bootloader and app, 0xFF elsewhere,
/// padded to 8 MB so it spans nvs and the cardid window as the padded corpus image does.
fn refusal_image(layout: &[Partition]) -> Vec<u8> {
    let mut image = vec![0xFFu8; 0x80_0000];
    image[..0x100].fill(0x11);
    image[0] = 0xE9;
    let table = encode_partition_table(layout);
    image[0x8000..0x8000 + table.len()].copy_from_slice(&table);
    // An app descriptor gives the boot check an ELF SHA-256 to compare (a placeholder digest).
    let app = 0x1_0000;
    image[app..app + 0x1000].fill(0x22);
    image[app..app + 24].fill(0);
    image[app] = 0xE9;
    image[app + 1] = 1;
    image[app + 2] = 2;
    image[app + 12..app + 14].copy_from_slice(&5u16.to_le_bytes());
    image[app + 24..app + 28].copy_from_slice(&0x3C00_0020u32.to_le_bytes());
    image[app + 28..app + 32].copy_from_slice(&256u32.to_le_bytes());
    image[app + 32..app + 288].fill(0);
    image[app + 32..app + 36].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
    image[app + 32 + 144..app + 32 + 176].fill(0x5A);
    image
}

fn refusal_backup() -> BackupEvidence {
    BackupEvidence {
        verified: true,
        owner_only: true,
        inside_repository: false,
    }
}

/// Held by every test of this binary that installs the process-wide `pemu_host` hooks. One lock
/// for the whole file: per-module locks do not serialize against one another, and a walker then
/// runs under the other firmware's `elves` closure (`inspect vars` answers `no symbol`).
static HOOKED: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Held by every test that opens a device other than the null device or counts character
/// devices, so the refusal matrix cannot count the pty test's pty as its own.
static CHARACTER_DEVICES: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Character devices this process holds open, as `(descriptor, device number)` above stdio,
/// leaving out the null device: nothing the planner does may add one. On a host without
/// `/dev/fd` the list is empty and the flow's opener record carries the assertion.
///
/// The null device is left out by device number, not name, because every concurrent
/// `Stdio::null()` spawn opens it; a port under any name still counts.
fn open_character_devices() -> Vec<(u32, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        // Not `ok()`: without a readable `/dev/null` every concurrent `Stdio::null()` would read as a
        // port.
        let null = std::fs::metadata("/dev/null")
            .expect("/dev/null is a character device on every unix host")
            .rdev();
        let Ok(dir) = std::fs::read_dir("/dev/fd") else {
            return Vec::new();
        };
        let mut open: Vec<(u32, u64)> = dir
            .filter_map(Result::ok)
            .filter_map(|e| Some((e.file_name().to_str()?.parse::<u32>().ok()?, e.path())))
            .filter(|(fd, _)| *fd > 2)
            .filter_map(|(fd, path)| {
                let meta = std::fs::metadata(path).ok()?;
                meta.file_type()
                    .is_char_device()
                    .then_some((fd, meta.rdev()))
            })
            .filter(|(_, rdev)| *rdev != null)
            .collect();
        open.sort_unstable();
        open
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

/// Every host serial device spelling.
fn names_a_serial_device(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["/dev/cu.", "/dev/tty.", r"\\.\com", r"\\?\usb#"]
        .iter()
        .any(|p| lower.contains(p))
        || lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
            w.len() > 3 && w.starts_with("com") && w[3..].bytes().all(|b| b.is_ascii_digit())
        })
}

struct Candidates;

impl Discovery for Candidates {
    fn enumerate(&mut self) -> Result<Vec<PortCandidate>, String> {
        Ok(["/dev/cu.usbmodem-e10", "/dev/tty.usbmodem-e10", "COM7"]
            .iter()
            .map(|p| PortCandidate {
                path: (*p).to_owned(),
                vid: 0x303A,
                pid: 0x1001,
            })
            .collect())
    }
}

struct AnyPort;

impl DevicePaths for AnyPort {
    fn is_flash_port(&self, path: &str) -> bool {
        path == "/dev/cu.usbmodem-e10"
    }
    fn case_insensitive(&self) -> bool {
        false
    }
}

/// The emulated rehearsal target: an in-memory 8 MB flash with the fixture's layout and a
/// synthetic cardid pattern, so the flow gets past the rehearsal and reaches the person.
struct MemTarget(Vec<u8>);

impl MemTarget {
    fn new(layout: &[Partition]) -> MemTarget {
        let mut flash = vec![0xFFu8; 0x80_0000];
        let table = encode_partition_table(layout);
        flash[0x8000..0x8000 + table.len()].copy_from_slice(&table);
        for (i, b) in flash[0x35_6000..0x35_A000].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        MemTarget(flash)
    }
}

impl DeviceSession for MemTarget {
    fn identify(&mut self) -> Result<Identity, SessionError> {
        Ok(Identity {
            chip: "ESP32-C3".to_owned(),
            revision: ChipRevision { major: 1, minor: 1 },
            flash_manufacturer: 0x20,
            flash_device: 0x4017,
            device_key: None,
        })
    }
    fn read_partition_table(&mut self) -> Result<Vec<u8>, SessionError> {
        Ok(self.0[0x8000..0x8C00].to_vec())
    }
    fn region_md5(&mut self, offset: u32, size: u32) -> Result<[u8; 16], SessionError> {
        let digest = pemu_loader::sha256(&self.0[offset as usize..(offset + size) as usize]);
        Ok(digest[..16].try_into().expect("16 bytes"))
    }
    fn read_region(&mut self, offset: u32, size: u32) -> Result<Vec<u8>, SessionError> {
        Ok(self.0[offset as usize..(offset + size) as usize].to_vec())
    }
    fn write(&mut self, writes: &[PlannedWrite]) -> Result<(), SessionError> {
        for w in writes {
            let (s, e) = w.sectors();
            self.0[s as usize..e as usize].fill(0xFF);
            self.0[w.offset as usize..w.offset as usize + w.data.len()].copy_from_slice(&w.data);
        }
        Ok(())
    }
    fn erase_region(&mut self, _: u32, _: u32) -> Result<(), SessionError> {
        unreachable!("the refusal matrix asks for no erase")
    }
    fn verify(&mut self, _: &[PlannedWrite]) -> Result<(), SessionError> {
        Ok(())
    }
    fn hard_reset(&mut self) -> Result<(), SessionError> {
        Ok(())
    }
    fn boot_log(&mut self) -> Result<String, SessionError> {
        let prefix = pemu_loader::hex(&[0x5A; 5]);
        Ok(format!(
            "rst:0x15 (USB_UART_CHIP_RESET),boot:0xa (SPI_FAST_FLASH_BOOT)\nI (117) app_init: ELF file SHA256:  {}...\n",
            &prefix[..9]
        ))
    }
}

/// A person who reads the prompt and declines; the prompt is kept to be searched.
struct Decline(Vec<String>);

impl ElicitationPort for Decline {
    fn elicit(&mut self, message: &str) -> Elicited {
        self.0.push(message.to_owned());
        Elicited::Declined
    }
}

struct RecordingOpener(usize);

impl SessionOpener for RecordingOpener {
    fn open(&mut self, _: &Confirmed) -> Result<Box<dyn DeviceSession + '_>, SessionError> {
        self.0 += 1;
        Err(SessionError::Unsupported(
            "the refusal matrix never opens a port".to_owned(),
        ))
    }
}

struct NoBackups;

impl BackupStore for NoBackups {
    fn store(
        &mut self,
        _: &[u8; 32],
        _: Option<&[u8; 16]>,
        _: &[(u32, Vec<u8>)],
    ) -> Result<BackupEvidence, String> {
        unreachable!()
    }
}

/// `plan_flash` and `flash_device --dry-run` against the committed device facts: each refusal
/// rule fires on its mutated fact, the unmodified facts give an accepted plan, and no host serial
/// device is opened (no port path in any output, the opener never reached, no character device
/// descriptor added).
#[test]
fn t0_m10_refusal_matrix_without_a_port() {
    let _devices = CHARACTER_DEVICES.lock().unwrap_or_else(|e| e.into_inner());
    let devices_before = open_character_devices();

    let facts = refusal_facts();
    assert_eq!(facts.revision, ChipRevision { major: 1, minor: 1 });
    assert_eq!(
        (facts.flash_manufacturer, facts.flash_device),
        (0x20, 0x4017)
    );
    let cardid = facts
        .partitions
        .iter()
        .find(|p| p.name == "cardid")
        .expect("cardid");
    assert_eq!((cardid.offset, cardid.size), (0x35_6000, 0x4000));
    let layout = facts.partitions.clone();
    let image = refusal_image(&layout);
    let request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);

    let mut outputs: Vec<String> = Vec::new();
    let record = |outcome: &PlanOutcome, outputs: &mut Vec<String>| {
        outputs.extend(outcome.refusals.iter().map(ToString::to_string));
        outputs.push(format!("{:?}", outcome.plan));
    };

    let planned = plan_flash(&request, Some(&facts));
    assert!(
        planned.accepted().is_some(),
        "plan_flash: {:?}",
        planned.refusals
    );
    let dry = dry_run(&request, &facts, Some(&refusal_backup()));
    assert!(dry.accepted().is_some(), "dry run: {:?}", dry.refusals);
    record(&planned, &mut outputs);
    record(&dry, &mut outputs);

    type Mutation = fn(&mut DeviceFacts, &mut Vec<u8>, &mut BackupEvidence, &mut bool);
    let matrix: [(&str, Rule, Mutation); 9] = [
        ("other chip revision", Rule::ChipRevision, |f, _, _, _| {
            f.revision = ChipRevision { major: 0, minor: 4 }
        }),
        ("other flash id", Rule::FlashId, |f, _, _, _| {
            f.flash_device = 0x4018
        }),
        ("missing cardid", Rule::CardidMissing, |f, _, _, _| {
            f.partitions.retain(|p| p.name != "cardid")
        }),
        ("moved cardid", Rule::CardidMoved, |f, _, _, _| {
            if let Some(p) = f.partitions.iter_mut().find(|p| p.name == "cardid") {
                p.offset = 0x35_8000;
            }
        }),
        (
            "plan overlapping [0x356000, 0x35A000)",
            Rule::CardidOverlap,
            |f, img, _, _| {
                // An app slot that runs into cardid, filled 0x800 bytes into the window.
                let mut layout = f.partitions.clone();
                layout.push(Partition {
                    name: "ota_0".to_owned(),
                    ptype: ptype::APP,
                    subtype: subtype::OTA_0,
                    offset: 0x35_2000,
                    size: 0x6000,
                    flags: 0,
                });
                let table = encode_partition_table(&layout);
                img[0x8000..0x8000 + table.len()].copy_from_slice(&table);
                img[0x35_2000..0x35_6800].fill(0x33);
                img[0x35_2000] = 0xE9;
            },
        ),
        (
            "no verified backup",
            Rule::NoVerifiedBackup,
            |_, _, b, present| {
                b.verified = false;
                *present = true;
            },
        ),
        (
            "absent backup",
            Rule::NoVerifiedBackup,
            |_, _, _, present| *present = false,
        ),
        (
            "backup not owner-only",
            Rule::BackupNotOwnerOnly,
            |_, _, b, _| b.owner_only = false,
        ),
        (
            "backup inside the repository",
            Rule::BackupInsideRepository,
            |_, _, b, _| b.inside_repository = true,
        ),
    ];
    for (what, rule, mutate) in matrix {
        let (mut f, mut img, mut backup, mut present) =
            (facts.clone(), image.clone(), refusal_backup(), true);
        mutate(&mut f, &mut img, &mut backup, &mut present);
        let request = PlanRequest::write(ImageSource::Merged(&img), Origin::HumanCli);
        let outcome = dry_run(&request, &f, present.then_some(&backup));
        assert!(
            outcome.refused_by(rule) && outcome.accepted().is_none(),
            "{what}: expected {}, got {:?}",
            rule.id(),
            outcome.refusals
        );
        record(&outcome, &mut outputs);
        if !matches!(
            rule,
            Rule::NoVerifiedBackup | Rule::BackupNotOwnerOnly | Rule::BackupInsideRepository
        ) {
            let planned = plan_flash(&request, Some(&f));
            assert!(planned.refused_by(rule), "plan_flash {what}");
            record(&planned, &mut outputs);
        }
    }

    // The rehearsal passes on the emulated target, the person declines, and the opener is never
    // reached.
    let mut rehearsal = MemTarget::new(&layout);
    let mut opener = RecordingOpener(0);
    let mut person = Decline(Vec::new());
    let (report, result) = flash_device(
        &request,
        None,
        Effects {
            discovery: &mut Candidates,
            paths: &AnyPort,
            rehearsal: &mut rehearsal,
            confirmer: &mut ElicitationConfirmer::new(&mut person),
            opener: &mut opener,
            backups: &mut NoBackups,
            // The ordinary run backs up only the sectors the plan writes; the full 8 MB backup is the
            // `--backup` a person types.
            full_backup: None,
        },
    );
    assert_eq!(result, Err(FlowError::Declined));
    assert_eq!(opener.0, 0, "the device opener is never reached");
    assert_eq!(person.0.len(), 1, "the person was asked once");
    outputs.extend(person.0.iter().cloned());
    outputs.push(format!("{:?}", report));

    // The same matrix through the registry with the production `HostPlanner`. A command takes no
    // device facts (the identity, device-table and `nvs` rules run at Identify, which needs a
    // port), so only the image half and the offline label are judged here.
    outputs.extend(registry_leg(&image, &layout));

    for output in &outputs {
        assert!(
            !names_a_serial_device(output),
            "a port path in an output: {output}"
        );
    }
    assert_eq!(
        open_character_devices(),
        devices_before,
        "a character device was opened"
    );
}

fn refusal_temp(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pemu-refusal-{name}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    dir
}

fn registry_call(
    name: &str,
    args: serde_json::Value,
) -> Result<pemu_api::output::Output, pemu_api::error::ApiError> {
    let spec = pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"));
    (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args)
}

/// The matrix through the registry. Returns every text it produced, for the port-path scan.
fn registry_leg(image: &[u8], layout: &[Partition]) -> Vec<String> {
    use pemu_api::commands::plan_flash::{
        CHECKS_PENDING, DeviceCaller, OFFLINE_LABEL, install, uninstall,
    };
    use pemu_host::device::HostPlanner;

    let mut outputs: Vec<String> = Vec::new();
    let root = refusal_temp("root");
    let repo = refusal_temp("repo");
    let images = refusal_temp("images");
    let accepted_path = images.join("accepted-8MB.bin");
    std::fs::write(&accepted_path, image).expect("the accepted image");
    let mut overlapping = image.to_vec();
    let mut over_layout = layout.to_vec();
    over_layout.push(Partition {
        name: "ota_0".to_owned(),
        ptype: ptype::APP,
        subtype: subtype::OTA_0,
        offset: 0x35_2000,
        size: 0x6000,
        flags: 0,
    });
    let table = encode_partition_table(&over_layout);
    overlapping[0x8000..0x8000 + table.len()].copy_from_slice(&table);
    overlapping[0x35_2000..0x35_6800].fill(0x33);
    overlapping[0x35_2000] = 0xE9;
    let overlap_path = images.join("overlapping-8MB.bin");
    std::fs::write(&overlap_path, &overlapping).expect("the overlapping image");

    // Without `--allow-device` no planner is installed and every Device command refuses by naming
    // the flag.
    uninstall();
    for name in ["plan_flash", "flash_device", "device_boot_check"] {
        let refused = registry_call(name, serde_json::json!({ "image": accepted_path }))
            .expect_err("no planner is installed");
        assert_eq!(refused.code, pemu_api::error::E_PLAN_REFUSED, "{name}");
        assert!(refused.message.contains("--allow-device"), "{name}");
        outputs.push(refused.message.clone());
    }

    install(
        DeviceCaller::Cli,
        Box::new(HostPlanner::new(&root, &repo, None)),
    );

    // The unmodified image: accepted, and labelled offline in both the text and the JSON.
    let planned = registry_call("plan_flash", serde_json::json!({ "image": accepted_path }))
        .expect("an offline plan is answered");
    assert_eq!(planned.json["accepted"], true, "{}", planned.json);
    assert_eq!(planned.json["device_checks"], CHECKS_PENDING);
    assert!(planned.text.contains(OFFLINE_LABEL), "{}", planned.text);
    outputs.push(planned.text.clone());
    outputs.push(planned.json.to_string());

    let refused = registry_call("plan_flash", serde_json::json!({ "image": overlap_path }))
        .expect("a refused plan is still an answer, not an error");
    assert_eq!(refused.json["accepted"], false, "{}", refused.json);
    let rules: Vec<String> = refused.json["refusals"]
        .as_array()
        .expect("refusals")
        .iter()
        .map(|r| r["rule"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        rules
            .iter()
            .any(|r| r == &format!("{:?}", Rule::CardidOverlap)),
        "{rules:?}"
    );
    outputs.push(refused.text.clone());
    outputs.push(refused.json.to_string());

    // `--dry-run` plans and reports, opens and enumerates nothing, and says so. Its backup evidence
    // is unverified, because nothing was read or written back.
    let dry = registry_call(
        "flash_device",
        serde_json::json!({ "image": accepted_path, "dry_run": true }),
    )
    .expect("a dry run is answered");
    assert_eq!(dry.json["dry_run"], true, "{}", dry.json);
    assert_eq!(dry.json["steps"][0]["ok"], true, "{}", dry.json);
    assert_eq!(dry.json["backup"]["verified"], false);
    assert_eq!(dry.json["boot_check"], serde_json::Value::Null);
    assert_eq!(dry.json["plan"]["device_checks"], CHECKS_PENDING);
    outputs.push(dry.text.clone());
    outputs.push(dry.json.to_string());

    let dry_refused = registry_call(
        "flash_device",
        serde_json::json!({ "image": overlap_path, "dry_run": true }),
    )
    .expect("a refused dry run is answered");
    assert_eq!(dry_refused.json["steps"][0]["ok"], false);
    assert_eq!(dry_refused.json["plan"]["accepted"], false);
    outputs.push(dry_refused.text.clone());
    outputs.push(dry_refused.json.to_string());

    // A data root whose backups sit inside the repository is refused before anything is opened.
    uninstall();
    install(
        DeviceCaller::Cli,
        Box::new(HostPlanner::new(&root, &root, None)),
    );
    let inside = registry_call(
        "flash_device",
        serde_json::json!({ "image": accepted_path, "dry_run": true }),
    );
    match inside {
        Ok(out) => {
            assert_eq!(
                out.json["backup"]["inside_repository"], true,
                "{}",
                out.json
            );
            outputs.push(out.text.clone());
            outputs.push(out.json.to_string());
        }
        Err(e) => {
            assert_eq!(e.code, pemu_api::error::E_PLAN_REFUSED, "{e:?}");
            outputs.push(e.message.clone());
        }
    }
    uninstall();

    for dir in [root, repo, images] {
        let _ = std::fs::remove_dir_all(dir);
    }
    outputs
}

// ------------------------------------------------------------------------------------------------
// The USB Serial/JTAG host endpoints
// ------------------------------------------------------------------------------------------------

mod usj_endpoints {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use pemu_core::time::VTime;
    use pemu_host::endpoints::live::{LiveRunner, Pacing};
    use pemu_host::endpoints::tcp::{TcpEndpoint, TcpOptions};
    use pemu_loader::bundle::FlashImage;
    use pemu_machine::machine::Machine;

    use super::common;

    /// The merged image of `official`.
    pub const OFFICIAL_IMAGE: &str = "FoloToy-AI-Passport-8MB.bin";

    /// The app partition of `official.pt` and the cardid window.
    pub const APP_OFFSET: usize = 0x1_0000;
    pub const APP_END: usize = 0x31_0000;
    pub const CARDID: std::ops::Range<u32> = 0x35_6000..0x35_A000;

    const ESPTOOL_TIMEOUT: Duration = Duration::from_secs(300);

    /// An esptool resolved from `$IDF_PYTHON_ENV_PATH`, then the IDF tools directory, as the
    /// interpreter that runs `-m esptool`, or `None` after a printed skip.
    pub fn esptool_or_skip(test: &str) -> Option<PathBuf> {
        let mut envs: Vec<PathBuf> = Vec::new();
        if let Some(env) = std::env::var_os("IDF_PYTHON_ENV_PATH") {
            envs.push(PathBuf::from(env));
        }
        let tools = std::env::var_os("IDF_TOOLS_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(|h| Path::new(&h).join(".espressif"))
            });
        if let Some(dir) = tools.map(|t| t.join("python_env"))
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            let mut found: Vec<PathBuf> =
                entries.filter_map(Result::ok).map(|e| e.path()).collect();
            // Newest by name.
            found.sort();
            envs.extend(found.into_iter().rev());
        }
        for env in envs {
            // `bin/python` on macOS, `Scripts\python.exe` on Windows; looking only for the first would skip
            // every esptool row on Windows.
            let python = if cfg!(windows) {
                env.join("Scripts").join("python.exe")
            } else {
                env.join("bin").join("python")
            };
            if !python.is_file() {
                continue;
            }
            let ok = Command::new(&python)
                .args(["-I", "-m", "esptool", "version"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success());
            if ok {
                return Some(python);
            }
        }
        common::skip(test, "no esptool in an IDF Python environment");
        None
    }

    pub fn official_or_skip(test: &str) -> Option<Vec<u8>> {
        let path = common::corpus_file_or_skip(test, common::OFFICIAL, OFFICIAL_IMAGE)?;
        Some(std::fs::read(path).expect("the located corpus file reads"))
    }

    /// `official`'s app bytes: the factory partition with its trailing erased bytes trimmed.
    pub fn app_bytes(image: &[u8]) -> Vec<u8> {
        let app = &image[APP_OFFSET..APP_END];
        let len = app.iter().rposition(|&b| b != 0xFF).map_or(0, |i| i + 1);
        let mut out = app[..len.next_multiple_of(4)].to_vec();
        out.resize(len.next_multiple_of(4), 0xFF);
        out
    }

    /// The console line the boot run of [`Live::start`] waits for: `official` has left `app_main`
    /// and the ROM and bootloader watchdogs are off.
    pub const BOOTED: &str = "main_task: Returned from app_main()";

    /// Virtual time the boot may take; `official` prints [`BOOTED`] at about 451 ms.
    pub const BOOT_BUDGET_MS: u64 = 3_000;

    /// How long the live thread is held back while the first esptool client connects, the host lag
    /// that made this row flake (see [`Live::start`]).
    pub const LAG: Duration = Duration::from_millis(2_500);

    /// Connect attempts of the command that runs under [`LAG`], spelled out: the lag exercises the
    /// row only if the client is still retrying its sync when the live thread starts again.
    pub const CONNECT_ATTEMPTS: &str = "7";

    /// What the command under [`LAG`] may take beyond the lag itself, on a loaded host.
    pub const LAG_SLACK: Duration = Duration::from_secs(60);

    pub struct Live {
        pub runner: LiveRunner<Machine>,
        pub endpoint: TcpEndpoint,
        /// `usj_tx` cursor at the start, so the console of this run can be read back.
        pub cursor: u64,
    }

    impl Live {
        pub fn start(image: &[u8], opts: TcpOptions) -> Live {
            let flash = FlashImage::from_merged(image).expect("official is a merged image");
            let machine = pemu_host::backend::build_machine(flash, 1).expect("machine");
            Live::start_machine(machine, opts)
        }

        /// [`Live::start`] over a machine the caller built, for a firmware that needs its app ELF bound
        /// before it boots (`pk` stops at the BLE tripwire without one).
        pub fn start_machine(mut machine: Machine, opts: TcpOptions) -> Live {
            let cursor = pemu_machine::machine::MachineApi::io(&mut machine)
                .usj_tx
                .head();
            // Booting before the endpoint opens is a correctness requirement. On a fresh machine
            // the client's reset lands wherever the live thread got to, on a loaded host early in
            // the boot. A download reset inside the ROM's flash-boot window (0 to 3 ms) or the
            // bootloader's (116 to 122 ms) leaves that stage's RTC watchdog armed, because reset
            // cause 0x15 keeps the RTC domain (IDF `soc/reset_reasons.h`), and the ROM's download
            // loop feeds neither. The watchdog then prints its banner into the SLIP stream
            // (`Invalid head of packet (0x45)`), before esptool's `_post_connect` disables it. Past
            // `app_main` the bootloader watchdog is off, whatever the host load.
            boot_past_app_main(&mut machine);
            // A debug build runs far slower than real time, so it runs unpaced; a release build at real
            // time.
            let pacing = if cfg!(debug_assertions) {
                Pacing::Max
            } else {
                Pacing::Wall(1.0)
            };
            let runner = LiveRunner::start(machine, pacing)
                .map_err(|(_, e)| e)
                .expect("spawn");
            let endpoint = TcpEndpoint::bind(runner.link(), opts).expect("bind loopback");
            assert!(endpoint.addr().ip().is_loopback(), "loopback only");
            Live {
                runner,
                endpoint,
                cursor,
            }
        }

        /// Lets `ms` more virtual milliseconds run, then stops and returns the machine and the console
        /// since the start. With no client connected the endpoint leaves the instance idle, so the last
        /// milliseconds (the boot after esptool's hard reset) run on the machine directly.
        pub fn finish(self, ms: u64) -> (Machine, Vec<u8>) {
            self.endpoint.close();
            let mut machine = self
                .runner
                .stop()
                .expect("the live thread returned the machine");
            let until = VTime(machine.now().0 + VTime::from_ms(ms).0);
            pemu_machine::machine::MachineApi::run(
                &mut machine,
                pemu_machine::run::RunLimits {
                    until: Some(until),
                    max_insns: None,
                    stops: pemu_machine::stops::StopSet::default(),
                },
            );
            let console = pemu_machine::machine::MachineApi::io(&mut machine)
                .usj_tx
                .slices(self.cursor)
                .iter()
                .copied()
                .collect();
            (machine, console)
        }
    }

    /// Runs the machine until it has left `app_main` ([`Live::start`]).
    fn boot_past_app_main(machine: &mut Machine) {
        use pemu_machine::machine::MachineApi;
        let from = MachineApi::io(machine).usj_tx.head();
        let deadline = VTime(machine.now().0 + VTime::from_ms(BOOT_BUDGET_MS).0);
        while machine.now().0 < deadline.0 {
            let until = VTime((machine.now().0 + VTime::from_ms(50).0).min(deadline.0));
            MachineApi::run(
                machine,
                pemu_machine::run::RunLimits {
                    until: Some(until),
                    max_insns: None,
                    stops: pemu_machine::stops::StopSet::default(),
                },
            );
            let console: Vec<u8> = MachineApi::io(machine)
                .usj_tx
                .slices(from)
                .iter()
                .copied()
                .collect();
            if String::from_utf8_lossy(&console).contains(BOOTED) {
                return;
            }
        }
        let console: Vec<u8> = MachineApi::io(machine)
            .usj_tx
            .slices(from)
            .iter()
            .copied()
            .collect();
        panic!(
            "the instance did not reach `{BOOTED}` in {BOOT_BUDGET_MS} ms of virtual time:\n{}",
            String::from_utf8_lossy(&console)
        );
    }

    /// One esptool run against an emulator URL, its joined output and its wall time. Only
    /// `rfc2217://127.0.0.1:` and `socket://127.0.0.1:` reach esptool, so no test can name a host
    /// serial device.
    pub fn esptool(
        python: &Path,
        port: &str,
        args: &[&str],
        cwd: &Path,
    ) -> (bool, String, Duration) {
        assert!(
            port.starts_with("rfc2217://127.0.0.1:") || port.starts_with("socket://127.0.0.1:"),
            "an emulator URL, never a device: {port}"
        );
        let started = Instant::now();
        let mut child = Command::new(python)
            .args(["-I", "-m", "esptool", "--chip", "esp32c3", "--port", port])
            .args(args)
            .current_dir(cwd)
            .env_remove("PYTHONPATH")
            .env_remove("PYTHONHOME")
            .env_remove("ESPTOOL_PORT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("esptool spawns");
        let out = {
            let mut stdout = child.stdout.take().expect("piped");
            let mut stderr = child.stderr.take().expect("piped");
            let err = std::thread::spawn(move || {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
                s
            });
            let mut s = String::new();
            let reader = std::thread::spawn(move || {
                let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
                s
            });
            loop {
                if child.try_wait().expect("wait").is_some() {
                    break;
                }
                if started.elapsed() > ESPTOOL_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let mut text = reader.join().unwrap_or_default();
            text.push_str(&err.join().unwrap_or_default());
            text
        };
        let ok = child.try_wait().ok().flatten().is_some_and(|s| s.success());
        // The command line and wall time of every run, and the whole output of one that failed: that
        // run is the evidence.
        println!(
            "esptool {} -> {} in {:.1} s{}",
            args.join(" "),
            if ok { "ok" } else { "FAILED" },
            started.elapsed().as_secs_f64(),
            if ok {
                String::new()
            } else {
                format!(":\n{out}")
            }
        );
        (ok, out, started.elapsed())
    }

    pub fn scratch(test: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("pemu-{test}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    pub fn after_last<'a>(console: &'a str, marker: &str) -> Option<&'a str> {
        console.rfind(marker).map(|i| &console[i..])
    }
}

/// esptool's USB-JTAG reset reaches ROM download mode over RFC 2217, `write_flash` writes
/// `official`'s app at 0x10000, and the hard reset boots `boot:0xa` into the app banner.
#[test]
fn t1_m10_esptool_usb_jtag_reset_write_and_hard_reset() {
    let test = "t1_m10_esptool_usb_jtag_reset_write_and_hard_reset";
    if cfg!(debug_assertions) {
        // A debug build answers chip_id and flash_id, but the stub's deflate is too slow for esptool's
        // FLASH_DEFL_DATA timeout.
        common::skip(
            test,
            "write_flash needs a release build (cargo test --release)",
        );
        return;
    }
    let Some(python) = usj_endpoints::esptool_or_skip(test) else {
        return;
    };
    let Some(image) = usj_endpoints::official_or_skip(test) else {
        return;
    };
    let dir = usj_endpoints::scratch(test);
    let app = usj_endpoints::app_bytes(&image);
    std::fs::write(dir.join("app.bin"), &app).expect("scratch app");

    let live = usj_endpoints::Live::start(&image, pemu_host::endpoints::tcp::TcpOptions::default());
    let url = live.endpoint.rfc2217_url();
    let (ok, out, wall) = usj_endpoints::esptool(
        &python,
        &url,
        &[
            "--before",
            "usb_reset",
            "--after",
            "hard_reset",
            "write_flash",
            "0x10000",
            "app.bin",
        ],
        &dir,
    );
    println!(
        "usb-jtag esptool wall: write_flash {:.1} s",
        wall.as_secs_f64()
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ok, "esptool failed:\n{out}");
    assert!(out.contains("Hash of data verified"), "{out}");
    assert!(out.contains("Hard resetting via RTS pin"), "{out}");

    let (machine, console) = live.finish(3_000);
    let console = String::from_utf8_lossy(&console);
    assert!(
        console.contains("rst:0x15 (USB_UART_CHIP_RESET),boot:0x6 (DOWNLOAD(USB/UART0))"),
        "{console}"
    );
    let boot =
        usj_endpoints::after_last(&console, "rst:0x15 (USB_UART_CHIP_RESET)").expect("a USB reset");
    assert!(
        boot.starts_with("rst:0x15 (USB_UART_CHIP_RESET),boot:0xa (SPI_FAST_FLASH_BOOT)"),
        "{boot}"
    );
    assert!(
        boot.contains("app_init: Project name:     FoloToy-AI-Passport"),
        "{boot}"
    );
    let mut written = vec![0u8; app.len()];
    machine.flash_read(usj_endpoints::APP_OFFSET as u32, &mut written);
    assert!(written == app, "the flash holds the app esptool wrote");
}

/// The esptool round trip over RFC 2217 with the default (classic) reset, and the `socket://`
/// form, where esptool forces `no_reset` and only monitors. A debug build runs the read-only half.
#[test]
fn t1_m10_esptool_round_trip_over_rfc2217_and_socket() {
    let test = "t1_m10_esptool_round_trip_over_rfc2217_and_socket";
    let Some(python) = usj_endpoints::esptool_or_skip(test) else {
        return;
    };
    let Some(image) = usj_endpoints::official_or_skip(test) else {
        return;
    };
    let dir = usj_endpoints::scratch(test);
    let live = usj_endpoints::Live::start(&image, pemu_host::endpoints::tcp::TcpOptions::default());
    let url = live.endpoint.rfc2217_url();
    let run_timed = |args: &[&str], what: &str| {
        let (ok, out, wall) = usj_endpoints::esptool(&python, &url, args, &dir);
        println!(
            "esptool round trip wall: {what} {:.1} s",
            wall.as_secs_f64()
        );
        assert!(ok, "esptool {what} failed:\n{out}");
        (out, wall)
    };
    let run = |args: &[&str], what: &str| run_timed(args, what).0;
    let stay = ["--before", "default_reset", "--after", "no_reset"];

    // The first command runs while the live thread is held back for `usj_endpoints::LAG`, the state
    // this row flaked in (`Live::start`), so it is exercised on every run. Both intervals are
    // measured: python's startup and the negotiation alone exceed `LAG`, and the hold can start
    // late.
    type Span = (std::time::Instant, std::time::Instant);
    // `LiveRunner::call` takes a `'static` job, so the span comes back through a shared cell.
    let held: std::sync::Arc<std::sync::Mutex<Option<Span>>> = std::sync::Arc::default();
    let (out, wall, command) = std::thread::scope(|scope| {
        let held = std::sync::Arc::clone(&held);
        let runner = &live.runner;
        scope.spawn(move || {
            runner.call(move |_: &mut pemu_machine::machine::Machine| {
                let from = std::time::Instant::now();
                std::thread::sleep(usj_endpoints::LAG);
                *held.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((from, std::time::Instant::now()));
            });
        });
        let from = std::time::Instant::now();
        let (out, wall) = run_timed(
            &[
                &stay[..],
                &[
                    "--connect-attempts",
                    usj_endpoints::CONNECT_ATTEMPTS,
                    "chip_id",
                ],
            ]
            .concat(),
            "chip_id under the lag",
        );
        (out, wall, (from, std::time::Instant::now()))
    });
    let held =
        (*held.lock().unwrap_or_else(|e| e.into_inner())).expect("the live thread was held back");
    // The command was running while the guest could not answer.
    let overlap = held
        .1
        .min(command.1)
        .saturating_duration_since(held.0.max(command.0));
    assert!(
        overlap >= usj_endpoints::LAG * 9 / 10,
        "the command ran {command:?} and the live thread was held back {held:?}: they overlap for \
         {overlap:?}, not the {:?} this row is about",
        usj_endpoints::LAG
    );
    assert!(
        wall <= usj_endpoints::LAG + usj_endpoints::LAG_SLACK,
        "the first command took {wall:?}, more than the lag plus {:?}: {} connect attempts are no \
         longer enough to cover the lag",
        usj_endpoints::LAG_SLACK,
        usj_endpoints::CONNECT_ATTEMPTS
    );
    assert!(
        out.contains("Chip is ESP32-C3 (QFN32) (revision v1.1)"),
        "{out}"
    );
    assert!(out.contains("USB mode: USB-Serial/JTAG"), "{out}");
    let out = run(&[&stay[..], &["flash_id"]].concat(), "flash_id");
    assert!(
        out.contains("Manufacturer: 20") && out.contains("Device: 4017"),
        "{out}"
    );
    // A non-sensitive range: the partition table.
    run(
        &[&stay[..], &["read_flash", "0x8000", "0xc00", "pt.bin"]].concat(),
        "read_flash",
    );
    let table = std::fs::read(dir.join("pt.bin")).expect("esptool wrote the read");
    assert!(
        table == image[0x8000..0x8C00],
        "read_flash returns the image's partition table"
    );

    let written_ok = if cfg!(debug_assertions) {
        common::skip(test, "write_flash and verify_flash need a release build");
        None
    } else {
        let app = usj_endpoints::app_bytes(&image);
        std::fs::write(dir.join("app.bin"), &app).expect("scratch app");
        let out = run(
            &[&stay[..], &["write_flash", "0x10000", "app.bin"]].concat(),
            "write_flash",
        );
        assert!(out.contains("Hash of data verified"), "{out}");
        let out = run(
            &[&stay[..], &["verify_flash", "0x10000", "app.bin"]].concat(),
            "verify_flash",
        );
        assert!(out.contains("-- verify OK"), "{out}");
        Some(app)
    };

    // `socket://`: esptool prints its no-reset note and cannot reach the ROM of a running app.
    let socket = live.endpoint.socket_url();
    let (_, out, _) = usj_endpoints::esptool(
        &python,
        &socket,
        &["--connect-attempts", "1", "chip_id"],
        &dir,
    );
    assert!(
        out.contains("It's not possible to reset the chip over a TCP socket"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);

    let (machine, console) = live.finish(10);
    let console = String::from_utf8_lossy(&console);
    // The class A download banner, as the device prints it.
    assert!(
        console.contains("rst:0x15 (USB_UART_CHIP_RESET),boot:0x6 (DOWNLOAD(USB/UART0))"),
        "esptool's reset latched the download strap:\n{console}"
    );
    if let Some(app) = written_ok {
        let mut written = vec![0u8; app.len()];
        machine.flash_read(usj_endpoints::APP_OFFSET as u32, &mut written);
        assert!(
            written == app,
            "the flash MD5 equals the image: the bytes are the app"
        );
        // The cardid window is compared in memory against the loaded image and never printed.
        let mut cardid =
            vec![0u8; (usj_endpoints::CARDID.end - usj_endpoints::CARDID.start) as usize];
        machine.flash_read(usj_endpoints::CARDID.start, &mut cardid);
        assert!(
            cardid
                == image[usj_endpoints::CARDID.start as usize..usj_endpoints::CARDID.end as usize],
            "the cardid window is unchanged"
        );
    }
}

/// `macos-only`: bytes cross the pty endpoint both ways against the real ROM, and the pty has no
/// modem-control lines. The ROM is reset into USB download mode first, so an esptool `SYNC`
/// written into the slave comes back as the ROM's own response.
#[cfg(target_os = "macos")]
#[test]
fn t0_m10_pty_data_echo_and_no_modem_lines() {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    use pemu_core::input::InputEvent;
    use pemu_host::endpoints::live::{LiveRunner, Pacing};
    use pemu_host::endpoints::pty::{LIMITATION, PtyEndpoint, modem_lines_supported};
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_machine::config::{Assets, MachineConfig};
    use pemu_machine::machine::{At, Machine};

    // The refusal matrix counts open character devices; the pty pair must not land in its window.
    let _devices = CHARACTER_DEVICES.lock().unwrap_or_else(|e| e.into_inner());
    let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(3))
        .expect("the bundled ROM is pinned");
    let mut machine = Machine::new(MachineConfig::default(), assets).expect("machine");
    for (dtr, rts) in [(true, false), (false, true), (false, false)] {
        machine
            .input(At::Now, InputEvent::UsbLine { dtr, rts })
            .expect("now");
    }
    let runner = LiveRunner::start(machine, Pacing::Max)
        .map_err(|(_, e)| e)
        .expect("spawn");
    let pty = PtyEndpoint::open(runner.link()).expect("a pty pair");
    let mut client = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.path())
        .expect("the pty slave opens");

    let mut sync = vec![
        0xC0, 0x00, 0x08, 0x24, 0x00, 0, 0, 0, 0, 0x07, 0x07, 0x12, 0x20,
    ];
    sync.extend([0x55; 32]);
    sync.push(0xC0);
    let response = [0xC0u8, 0x01, 0x08, 0x04, 0x00, 0x07, 0x07, 0x12, 0x20];
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut reader = client.try_clone().expect("clone");
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut got = Vec::new();
    let mut answered = false;
    while !answered && Instant::now() < deadline {
        // esptool repeats SYNC until the ROM is listening; so does this client.
        client.write_all(&sync).expect("write into the slave");
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(200)) {
            got.extend(chunk);
        }
        while let Ok(chunk) = rx.try_recv() {
            got.extend(chunk);
        }
        answered = got.windows(response.len()).any(|w| w == response);
    }
    assert!(
        answered,
        "the ROM's SYNC response came back through the pty: {}",
        String::from_utf8_lossy(&got)
    );
    assert!(
        String::from_utf8_lossy(&got)
            .contains("rst:0x15 (USB_UART_CHIP_RESET),boot:0x6 (DOWNLOAD(USB/UART0))"),
        "{}",
        String::from_utf8_lossy(&got)
    );
    assert!(!modem_lines_supported(pty.path()).expect("the modem-line query runs"));
    assert!(LIMITATION.contains("no modem-control lines"));
    drop(client);
    pty.close();
    runner.stop();
}

// ------------------------------------------------------------------------------------------------
// Sleep, power and NFC
// ------------------------------------------------------------------------------------------------

mod sleep_power_nfc {
    use std::sync::Arc;

    use pemu_core::hostio::SerialStream;
    use pemu_core::time::VTime;
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_loader::elf::ElfInfo;
    use pemu_machine::config::{Assets, MachineConfig};
    use pemu_machine::machine::Machine;
    use pemu_machine::run::RunLimits;
    use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};

    use super::common;

    pub use super::common::workspace;

    /// A probe machine: the merged image from `corpus/probes/`, pinned by `tests/fw/manifest.toml`,
    /// with its stripped ELF bound. `None` after a printed skip.
    pub fn probe_or_skip(test: &str, name: &str) -> Option<Machine> {
        let official = common::corpus_file_or_skip(
            test,
            common::OFFICIAL,
            super::usj_endpoints::OFFICIAL_IMAGE,
        )?;
        let path = official
            .ancestors()
            .nth(2)
            .expect("a corpus file sits under corpus/<id>/")
            .join(format!("probes/{name}-8MB.bin"));
        let Ok(bytes) = std::fs::read(&path) else {
            common::skip(
                test,
                &format!("corpus/probes/{name}-8MB.bin is not built (xtask probes)"),
            );
            return None;
        };
        let fw = workspace().join("tests/fw");
        let manifest =
            std::fs::read_to_string(fw.join("manifest.toml")).expect("the probe manifest");
        let pinned = manifest
            .split("[[probe]]")
            .find(|block| block.contains(&format!("name = \"{name}\"")))
            .and_then(|block| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix("merged_sha256 = \""))
                    .map(|v| v.trim_end_matches('"').to_string())
            })
            .expect("tests/fw/manifest.toml pins the probe's merged image");
        assert_eq!(
            pemu_testkit::corpus::sha256_hex(&bytes),
            pinned,
            "{test}: corpus/probes/{name}-8MB.bin is not the pinned build"
        );
        let elf =
            ElfInfo::parse(&std::fs::read(fw.join(format!("{name}.elf"))).expect("tests/fw ELF"))
                .expect("the probe ELF parses");
        let flash = FlashImage::from_merged(&bytes).expect("a probe image parses");
        let assets =
            Assets::with_bundled_rom(flash, Some(Arc::new(elf)), None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        Some(Machine::new(MachineConfig::default(), assets).expect("the image fits"))
    }

    pub fn console(m: &mut Machine) -> String {
        let ring = m.io().serial_ring(SerialStream::UsjTx);
        String::from_utf8_lossy(&ring.slices(0).iter().copied().collect::<Vec<u8>>()).into_owned()
    }

    pub fn field<'a>(line: &'a str, key: &str) -> &'a str {
        line.split('|')
            .find_map(|f| f.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')))
            .unwrap_or_else(|| panic!("`{key}` in `{line}`"))
    }

    /// `sleep_timer` through both boots: three 500 ms light sleeps return `ESP_OK` by the timer
    /// (cause 4) with esp_timer and RTC time agreeing and at least 500 ms (the wake latency of
    /// `device-sleep_lat-20260917T141559Z.notes.md` is modeled), then a 1 s deep sleep comes back as
    /// `rst:0x5 (DSLEEP)` with reset reason 8, the TIMER wake cause, the RTC data counter kept and at
    /// least a second of RTC time.
    ///
    /// It also prints the emulator's counterparts of the capture's deep-sleep numbers (device
    /// 1102849 us from entry to boot 2's `BOOT` line, 26334 us uptime at app_main); boot time is not
    /// calibrated, so the gap is printed.
    #[test]
    fn t1_m10_sleep_timer_probe_light_and_deep_sleep() {
        let test = "t1_m10_sleep_timer_probe_light_and_deep_sleep";
        let Some(mut m) = probe_or_skip(test, "sleep_timer") else {
            return;
        };
        let boot2 = run_to_line(&mut m, 10_000, "BOOT|stage=1");
        assert_eq!(
            boot2.reason,
            StopReason::Matcher(MatcherId(0x10)),
            "{test}:\n{}",
            console(&mut m)
        );
        let events: Vec<pemu_core::hostio::HostEvent> =
            m.io().events.slices(0).iter().copied().collect();
        let deep_entry = events
            .iter()
            .rev()
            .find(|e| e.kind == pemu_core::hostio::EventKind::Sleep && e.arg == 2)
            .expect("the deep-sleep entry event")
            .vt;
        let deep_reset = events
            .iter()
            .rev()
            .find(|e| e.kind == pemu_core::hostio::EventKind::Reset && e.arg == 0x05)
            .expect("the DEEPSLEEP reset event")
            .vt;

        let done = MatcherId(0xD0);
        let out = m.run(RunLimits {
            until: Some(VTime::from_ms(10_000)),
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
        assert_eq!(out.reason, StopReason::Matcher(done), "{test}:\n{text}");
        let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();

        let boots: Vec<&&str> = lines.iter().filter(|l| l.starts_with("BOOT|")).collect();
        assert_eq!(boots.len(), 2, "{text}");
        assert_eq!(field(boots[0], "raw"), "0x01");
        assert_eq!(field(boots[1], "stage"), "1");
        assert_eq!(field(boots[1], "reason"), "8", "ESP_RST_DEEPSLEEP");
        assert_eq!(field(boots[1], "raw"), "0x05");
        assert_eq!(field(boots[1], "wake"), "4", "ESP_SLEEP_WAKEUP_TIMER");
        assert!(text.contains("rst:0x5 (DSLEEP),boot:0xa (SPI_FAST_FLASH_BOOT)"));

        let light: Vec<&&str> = lines.iter().filter(|l| l.starts_with("LIGHT|")).collect();
        assert_eq!(light.len(), 3, "{text}");
        for line in light {
            assert_eq!(field(line, "rc"), "0", "{line}");
            assert_eq!(field(line, "cause"), "4", "{line}");
            let timer: u64 = field(line, "timer_ms").parse().expect("a number");
            let rtc: u64 = field(line, "rtc_ms").parse().expect("a number");
            assert_eq!(timer, 500, "the device reads 500.083 to 500.131 ms: {line}");
            assert_eq!(timer, rtc, "esp_timer and the RTC agree: {line}");
        }
        let deep = lines
            .iter()
            .find(|l| l.starts_with("DEEP|"))
            .expect("the second boot's DEEP line");
        assert_eq!(field(deep, "counter"), "1", "RTC data memory survived");
        let slept: u64 = field(deep, "rtc_ms").parse().expect("a number");
        assert!((1_000..1_200).contains(&slept), "{deep}");
        let fails: Vec<&&str> = lines.iter().filter(|l| l.starts_with("FAIL|")).collect();
        assert!(fails.is_empty(), "{fails:?}");
        assert!(lines.contains(&"DONE|name=sleep_timer|status=ok"), "{text}");

        let rtc_us = (boot2.vt.0 - deep_entry.0) / 1_000_000;
        let uptime_us = (boot2.vt.0 - deep_reset.0) / 1_000_000;
        println!(
            "GAP {test}: deep-sleep entry to boot 2 BOOT line {rtc_us} us (device 1102849 us, \
             probe rtc_ms={slept}); DEEPSLEEP reset to boot 2 BOOT line {uptime_us} us (device \
             esp_timer uptime at app_main 26334 us); boot time is the timing profile's"
        );
    }

    /// Absolute count of console bytes the USJ ring has received.
    pub fn console_head(m: &mut Machine) -> u64 {
        m.io().serial_ring(SerialStream::UsjTx).head()
    }

    pub fn run_to_line(m: &mut Machine, ms: u64, text: &str) -> pemu_machine::run::RunOutcome {
        m.run(RunLimits {
            until: Some(VTime::from_ms(ms)),
            max_insns: None,
            stops: StopSet {
                matchers: vec![(
                    MatcherId(0x10),
                    Matcher::Serial {
                        stream: SerialStream::UsjTx,
                        pattern: LinePattern::Contains(text.into()),
                    },
                )],
                ..StopSet::default()
            },
        })
    }

    /// Over the bundled ROM: holding power 2.2 s turns the rail off and the console stops; a 0.6 s
    /// press turns it on with a `rst:0x1` banner. The hold lengths are the `input` command's,
    /// and the rail edges land 2000 ms and 500 ms into the hold.
    #[test]
    fn t0_m10_power_hold_off_and_press_on() {
        use pemu_api::commands::input::{POWER_OFF_MS, POWER_ON_MS};
        use pemu_core::hostio::EventKind;
        use pemu_core::input::InputEvent;
        use pemu_machine::machine::At;

        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let mut m = Machine::new(MachineConfig::default(), assets).expect("machine");
        let run_to = |m: &mut Machine, ms: u64| {
            m.run(RunLimits {
                until: Some(VTime::from_ms(ms)),
                max_insns: None,
                stops: StopSet::default(),
            })
        };
        let banner = "rst:0x1 (POWERON),boot:0xa (SPI_FAST_FLASH_BOOT)";
        let first = run_to_line(&mut m, 300, banner);
        assert!(
            matches!(first.reason, StopReason::Matcher(_)),
            "{:?}",
            first.reason
        );
        run_to(&mut m, 300);

        // Hold 2.2 s from t = 0.3 s: the rail drops at 2.3 s.
        let ms = |v: u64| VTime::from_ms(v);
        m.input(At::Vt(ms(300)), InputEvent::Power { down: true })
            .expect("future");
        m.input(
            At::Vt(ms(300 + POWER_OFF_MS)),
            InputEvent::Power { down: false },
        )
        .expect("future");
        run_to(&mut m, 2_300);
        assert!(!m.mcu_powered(), "the rail is off");
        let at_off = console_head(&mut m);
        run_to(&mut m, 3_000);
        assert_eq!(
            console_head(&mut m),
            at_off,
            "the console stopped with the rail"
        );
        let power: Vec<(u64, VTime)> = m
            .io()
            .events
            .slices(0)
            .iter()
            .filter(|e| e.kind == EventKind::Power)
            .map(|e| (e.arg, e.vt))
            .collect();
        assert_eq!(power, vec![(0, ms(2_300))], "power.off at the 2000 ms mark");

        // A 0.6 s press from t = 3 s: the rail rises 500 ms in with a power-on reset.
        m.input(At::Vt(ms(3_000)), InputEvent::Power { down: true })
            .expect("future");
        m.input(
            At::Vt(ms(3_000 + POWER_ON_MS)),
            InputEvent::Power { down: false },
        )
        .expect("future");
        let second = run_to_line(&mut m, 4_000, banner);
        assert!(
            matches!(second.reason, StopReason::Matcher(_)),
            "{:?}",
            second.reason
        );
        assert!(second.vt > ms(3_500), "the banner follows the rail edge");
        assert!(m.mcu_powered());
        let resets: Vec<(u64, VTime)> = m
            .io()
            .events
            .slices(0)
            .iter()
            .filter(|e| e.kind == EventKind::Reset)
            .map(|e| (e.arg, e.vt))
            .collect();
        assert_eq!(resets.last(), Some(&(1, ms(3_500))), "{resets:?}");
    }

    /// `nfc.ndef.write` of a URI then `nfc.tap` reads it back, the NFC counter increments, and
    /// `nfc.dump` redacts the UID. The card has no MCU connection, so the bundled ROM is enough.
    #[test]
    fn t0_m10_nfc_uri_write_tap_counter_and_redacted_dump() {
        use pemu_api::commands::start::{Boot, StartArgs, with_pool};

        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let machine = Machine::new(MachineConfig::default(), assets).expect("the machine builds");
        let id = with_pool(|pool| {
            let start = StartArgs {
                fw: common::OFFICIAL.to_owned(),
                boot: Boot::None,
                ..StartArgs::default()
            };
            let id = pool.attach(&start, Box::new(machine));
            pool.table_mut()
                .get_mut(id)
                .expect("the instance was just created")
                .transition(pemu_api::instance::Lifecycle::Paused, VTime(0))
                .expect("starting -> paused");
            id.to_string()
        });
        let call = |name: &str, args: serde_json::Value| {
            let spec = pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}`"));
            assert_eq!(
                spec.group,
                pemu_api::spec::CapsGroup::Nfc,
                "the `nfc` group"
            );
            (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args)
                .unwrap_or_else(|e| panic!("`{name}` failed: {e:?}"))
        };
        const URI: &str = "https://example.com/p";

        let loaded = call(
            "nfc_tag",
            serde_json::json!({"instance": id, "ndef": [{"type": "uri", "uri": URI}], "counter": true}),
        );
        assert!(
            loaded.json["frames"].as_u64().unwrap_or(0) > 0,
            "{}",
            loaded.json
        );

        let tap = |ops: serde_json::Value| {
            call("nfc_tap", serde_json::json!({"instance": id, "ops": ops}))
        };
        let first = tap(serde_json::json!([{"op": "readNdef"}]));
        assert_eq!(
            first.json["ops"][0]["records"][0]["uri"], URI,
            "{}",
            first.json
        );
        assert_eq!(first.json["counter"], 1);
        let second = tap(serde_json::json!([{"op": "readNdef"}]));
        assert_eq!(second.json["counter"], 2, "one increment per tap");

        let dump = call("nfc_tag", serde_json::json!({"instance": id, "dump": true}));
        assert_eq!(dump.json["tag"]["uid"], "<SECRET>");
        assert_eq!(dump.json["pages"][0], "<SECRET>");
        assert_eq!(dump.json["pages"][1], "<SECRET>");
        // The UID's bytes, read through a raw READ 00 as a phone would, never appear either.
        let raw = tap(serde_json::json!([{"op": "raw", "frames": ["3000"]}]));
        let response = raw.json["ops"][0]["responses"][0]
            .as_str()
            .expect("a response");
        assert!(response.starts_with("<SECRET>"), "{response}");
        assert_eq!(
            response.len(),
            "<SECRET>".len() + 2 * 7,
            "BCC1 masked, 7 bytes kept"
        );

        let vt = call("nfc_tag", serde_json::json!({"instance": id}));
        assert_eq!(vt.json["vt_us"], 0, "no NFC call advances virtual time");
        with_pool(|pool| {
            let id = pemu_api::instance::InstanceId::parse(&id).expect("an id");
            pool.destroy(id).expect("the instance ends");
        });
    }

    /// The official image and ELF installed as the in-process backend, or `None` after the SKIP line.
    /// The guard serializes the process-wide hooks against every other test that installs them.
    fn hooked_official(test: &str) -> Option<std::sync::MutexGuard<'static, ()>> {
        use super::HOOKED;
        let bin = common::corpus_file_or_skip(
            test,
            common::OFFICIAL,
            super::usj_endpoints::OFFICIAL_IMAGE,
        )?;
        let elf = common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport.elf")?;
        let guard = HOOKED.lock().unwrap_or_else(|e| e.into_inner());
        let image = Arc::new(std::fs::read(bin).expect("the verified corpus image is readable"));
        let context = Arc::new(
            pemu_host::hooks::ElfContext::parse(&std::fs::read(elf).expect("the ELF reads"))
                .expect("the ELF parses"),
        );
        pemu_host::backend::install(
            Arc::new(move |fw: &str| match fw {
                common::OFFICIAL => pemu_host::backend::merged_image(&image),
                other => Err(pemu_api::commands::start::firmware_not_found(other)),
            }),
            None,
            pemu_host::audio_root::AudioRoot::new(std::env::temp_dir().join("pemu-m10-no-audio")),
        );
        pemu_host::hooks::install(pemu_host::hooks::HostHooks {
            elves: Arc::new(move |fw: &str| (fw == common::OFFICIAL).then(|| Arc::clone(&context))),
            scenario_root: pemu_host::hooks::ScenarioRoot::new(
                Some(workspace()),
                vec![workspace()],
            ),
            salt_dir: None,
        });
        pemu_host::boot_cache::install(None);
        Some(guard)
    }

    fn call(name: &str, args: serde_json::Value) -> serde_json::Value {
        let spec =
            pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"));
        (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args.clone())
            .unwrap_or_else(|e| panic!("`{name}` {args} failed: {e:?}"))
            .json
    }

    /// The `official` Low Power card: light sleep 2 s resumes; deep sleep 5 s gives a banner with
    /// `rst:0x5` and the timer wake cause; the RTC RAM counter survives.
    ///
    /// The host event ring has no USB kind, so the USJ detach and attach are claimed with the
    /// deep-sleep `Sleep` events (arg 2 detach, arg 3 attach) plus the USJ U-state from the `soc.usj`
    /// snapshot section: U1 `ChargeOnly` mid-sleep and U3 `AttachedOpen` after the wake. The link
    /// drops for a light sleep too, so that leg reads the U-state inside the 2 s sleep.
    #[test]
    fn t1_m10_official_low_power_light_and_deep_sleep() {
        use pemu_core::hostio::{EventKind, HostEvent};

        let test = "t1_m10_official_low_power_light_and_deep_sleep";
        let Some(_hooks) = hooked_official(test) else {
            return;
        };
        let start = call(
            "start",
            serde_json::json!({"fw": common::OFFICIAL, "boot_cache": "ui-settled"}),
        );
        assert_eq!(start["boot"]["status"], "matched", "{start}");
        let id = start["instance"].as_str().expect("an id").to_owned();
        let press = |button: &str| {
            call(
                "input",
                serde_json::json!({"instance": id, "button": button, "action": "click"}),
            )
        };
        let tree = || {
            call("ui", serde_json::json!({"instance": id}))["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        let run_until = |until: &str, timeout: &str| {
            let out = call(
                "run",
                serde_json::json!({"instance": id, "until": until, "timeout": timeout}),
            );
            assert_eq!(out["status"], "matched", "{until}: {out}");
            out
        };
        let events = || -> Vec<HostEvent> {
            let parsed = pemu_api::instance::InstanceId::parse(&id).expect("an id");
            pemu_api::commands::start::with_pool(|pool| {
                let session = pool.session_mut(parsed).expect("a session");
                session
                    .machine()
                    .io()
                    .events
                    .slices(0)
                    .iter()
                    .copied()
                    .collect()
            })
        };
        let usj_state = || {
            use pemu_core::snap::{SectionId, SnapOpts, serde_from_section};
            let parsed = pemu_api::instance::InstanceId::parse(&id).expect("an id");
            pemu_api::commands::start::with_pool(|pool| {
                let session = pool.session_mut(parsed).expect("a session");
                let snapshot = session
                    .snapshot_machine()
                    .snapshot(SnapOpts::default())
                    .expect("a snapshot");
                let section = SectionId::soc("usj");
                let model: pemu_soc_c3::periph::usj::UsjModel = serde_from_section(
                    snapshot.section(&section).expect("the usj section"),
                    section,
                    pemu_machine::snapshot::SECTION_VERSION,
                    "soc.usj",
                )
                .expect("the usj section decodes");
                model.link().state
            })
        };
        let sleeps = |from: VTime| -> Vec<(u64, VTime)> {
            events()
                .into_iter()
                .filter(|e| e.kind == EventKind::Sleep && e.vt >= from)
                .map(|e| (e.arg, e.vt))
                .collect()
        };
        let now = || {
            VTime(
                call("status", serde_json::json!({"instance": id}))["instances"][0]["vt_us"]
                    .as_u64()
                    .unwrap_or(0)
                    * 1_000_000,
            )
        };

        // UP wraps from Display to Low Power (main.c DEMOS[6]), OK enters it.
        press("up");
        press("ok");
        call("run", serde_json::json!({"instance": id, "for": "500ms"}));
        let page = tree();
        assert!(page.contains("RTC TIMER WAKE ONLY"), "{page}");

        // OK runs the selected first card (demo_low_power.c SLEEP_COMMAND_LIGHT).
        let t0 = now();
        press("ok");
        call("run", serde_json::json!({"instance": id, "for": "1s"}));
        let light_asleep_state = usj_state();
        assert_eq!(
            light_asleep_state,
            pemu_core::hostio::UsbHostState::ChargeOnly,
            "U1 while the light sleep holds the PHY unpowered"
        );
        assert_eq!(
            sleeps(t0).iter().map(|e| e.0).collect::<Vec<_>>(),
            vec![0],
            "mid-sleep: the entry only"
        );
        call("run", serde_json::json!({"instance": id, "for": "2s"}));
        let light_woke_state = usj_state();
        assert_eq!(
            light_woke_state,
            pemu_core::hostio::UsbHostState::AttachedOpen,
            "U3 after the light-sleep wake"
        );
        let light = sleeps(t0);
        assert_eq!(
            light.iter().map(|e| e.0).collect::<Vec<_>>(),
            vec![0, 1],
            "{light:?}"
        );
        let slept = light[1].1.0 - light[0].1.0;
        println!(
            "{test}: light sleep entry {:?}, gated {} ps",
            light[0].1, slept
        );
        // The modeled wake latency (`LIGHT_SLEEP_WAKE_LATENCY_PS`): the device returns 83 to 131 us past
        // the request (`device-sleep_lat-20260917T141559Z.notes.md`).
        assert!(
            (1_990_000_000_000..=2_000_200_000_000).contains(&slept),
            "{slept} ps"
        );
        let woke = tree();
        assert!(woke.contains("LIGHT WAKE: TIMER"), "{woke}");
        println!(
            "RAN {test} light: {}",
            woke.lines()
                .filter(|l| l.contains("Slept"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // The card's line is written after the wake and reaches the host. On the device the host's port
        // does not survive the detach, so its console stops instead
        // (`device-sleep_timer-20260917T141314Z.log`).
        println!(
            "RAN {test} usj light: Sleep arg 0 (detach) then arg 1 (attach); U-state \
             {light_asleep_state:?} asleep, {light_woke_state:?} after the wake"
        );

        // Deep sleep: DOWN selects the second card, OK runs it; 5 s later the chip resets.
        press("down");
        let t1 = now();
        press("ok");
        call("run", serde_json::json!({"instance": id, "for": "2s"}));
        assert_eq!(
            sleeps(t1).iter().map(|e| e.0).collect::<Vec<_>>(),
            vec![2],
            "asleep: the detach"
        );
        let asleep_state = usj_state();
        assert_eq!(
            asleep_state,
            pemu_core::hostio::UsbHostState::ChargeOnly,
            "U1 while the PHY is unpowered"
        );
        run_until("serial:~\"rst:0x5 (DSLEEP)\"", "10s");
        let woke_state = usj_state();
        assert_eq!(
            woke_state,
            pemu_core::hostio::UsbHostState::AttachedOpen,
            "U3 after the wake"
        );
        let deep = sleeps(t1);
        assert_eq!(
            deep.iter().map(|e| e.0).collect::<Vec<_>>(),
            vec![2, 3],
            "{deep:?}"
        );
        let asleep = deep[1].1.0 - deep[0].1.0;
        assert!(
            (4_990_000_000_000..=5_000_100_000_000).contains(&asleep),
            "{asleep} ps"
        );
        // main.c:110 logs the wake cause last; 4 is `ESP_SLEEP_WAKEUP_TIMER`.
        run_until("serial:/: 4$/", "5s");
        let resets: Vec<u64> = events()
            .into_iter()
            .filter(|e| e.kind == EventKind::Reset && e.vt >= t1)
            .map(|e| e.arg)
            .collect();
        assert_eq!(resets, vec![5], "one DEEPSLEEP reset");

        // Back in the menu the card reports `s_deep_sleep_count` (RTC_DATA_ATTR) as #1
        // (demo_low_power.c:114-118).
        run_until("serial:/Display=1 Button=1/", "5s");
        press("up");
        press("ok");
        call("run", serde_json::json!({"instance": id, "for": "500ms"}));
        let back = tree();
        assert!(back.contains("DEEP TIMER WAKE  #1"), "{back}");
        println!(
            "RAN {test} deep: sleep {deep:?}, reset cause 5, wake cause 4, DEEP TIMER WAKE #1"
        );
        println!(
            "RAN {test} usj: Sleep arg 2 (detach) then arg 3 (attach); U-state {asleep_state:?} \
             asleep, {woke_state:?} after the wake"
        );
        call("stop", serde_json::json!({"instance": id}));
    }
}

// ------------------------------------------------------------------------------------------------
// The `power` and `usb` commands of the opt-in caps group
// ------------------------------------------------------------------------------------------------

mod power_usb_commands {
    use std::sync::{Arc, Mutex};

    use pemu_core::input::InputEvent;
    use pemu_core::journal::Origin;
    use pemu_core::snap::{LivePolicy, SnapError, SnapHeader, SnapOpts, Snapshot};
    use pemu_core::time::VTime;
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_machine::SnapshotMachine;
    use pemu_machine::config::{Assets, MachineConfig};
    use pemu_machine::machine::{At, GuestMem, InputError, Machine, MachineApi, Receipt};
    use pemu_machine::run::{RunLimits, RunOutcome};
    use pemu_machine::snapshot::{FlashView, Redaction, SecretSources};

    use super::common;

    type Journaled = Arc<Mutex<Vec<(At, Origin, InputEvent)>>>;

    /// A real machine that also keeps every journaled input: the pool holds a
    /// `Box<dyn SnapshotMachine + Send>`, so `Machine::journal` is out of reach.
    ///
    /// **Every other method forwards, including the defaulted ones.** A defaulted `secret_sources` is
    /// empty, so an instance behind this wrapper would export with nothing to redact: harmless on the
    /// erased flash here, a leak for the next test that wraps a corpus image.
    struct Recording {
        inner: Machine,
        journal: Journaled,
    }

    impl Recording {
        fn note(&self, at: At, origin: Origin, ev: &InputEvent) {
            self.journal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((at, origin, ev.clone()));
        }
    }

    impl MachineApi for Recording {
        fn run(&mut self, lim: RunLimits) -> RunOutcome {
            self.inner.run(lim)
        }
        fn input(&mut self, at: At, ev: InputEvent) -> Result<u64, InputError> {
            self.note(at, Origin::default(), &ev);
            MachineApi::input(&mut self.inner, at, ev)
        }
        fn input_from(
            &mut self,
            at: At,
            origin: Origin,
            ev: InputEvent,
        ) -> Result<u64, InputError> {
            self.note(at, origin, &ev);
            MachineApi::input_from(&mut self.inner, at, origin, ev)
        }
        fn io(&mut self) -> &mut pemu_core::hostio::HostIo {
            MachineApi::io(&mut self.inner)
        }
        fn now(&self) -> VTime {
            MachineApi::now(&self.inner)
        }
        fn guest_mem(&mut self) -> GuestMem<'_> {
            MachineApi::guest_mem(&mut self.inner)
        }
        fn receipt(&mut self) -> Receipt {
            MachineApi::receipt(&mut self.inner)
        }
        fn is_tainted(&self) -> bool {
            MachineApi::is_tainted(&self.inner)
        }
    }

    impl SnapshotMachine for Recording {
        fn snapshot(&self, opts: SnapOpts) -> Result<Snapshot, SnapError> {
            SnapshotMachine::snapshot(&self.inner, opts)
        }
        fn redact(&self, snapshot: &mut Snapshot) -> Result<Redaction, SnapError> {
            SnapshotMachine::redact(&self.inner, snapshot)
        }
        fn restore(&mut self, snapshot: &Snapshot) -> Result<(), SnapError> {
            SnapshotMachine::restore(&mut self.inner, snapshot)
        }
        /// A fork is the machine alone: a copy recording into the same journal would mix instances.
        fn fork(&self, live: LivePolicy) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
            SnapshotMachine::fork(&self.inner, live)
        }
        fn state_hash(&self) -> [u8; 32] {
            SnapshotMachine::state_hash(&self.inner)
        }
        fn snapshot_header(&self, opts: SnapOpts) -> Result<SnapHeader, SnapError> {
            SnapshotMachine::snapshot_header(&self.inner, opts)
        }
        fn secret_generation(&self) -> u64 {
            SnapshotMachine::secret_generation(&self.inner)
        }
        fn secret_sources(&self, view: FlashView) -> SecretSources {
            SnapshotMachine::secret_sources(&self.inner, view)
        }
        fn interrupt_can_wake(&self) -> Option<bool> {
            SnapshotMachine::interrupt_can_wake(&self.inner)
        }
    }

    /// The seed both instances are built and started with.
    const SEED: u64 = 0x5EED_0007;

    fn machine() -> Machine {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let config = MachineConfig {
            seed: SEED,
            ..MachineConfig::default()
        };
        Machine::new(config, assets).expect("the machine builds")
    }

    fn instance() -> (String, Journaled) {
        use pemu_api::commands::start::{Boot, StartArgs, with_pool};

        let journal: Journaled = Journaled::default();
        let backend = Recording {
            inner: machine(),
            journal: Arc::clone(&journal),
        };
        let id = with_pool(|pool| {
            let start = StartArgs {
                fw: common::OFFICIAL.to_owned(),
                boot: Boot::None,
                seed: SEED,
                ..StartArgs::default()
            };
            let id = pool.attach(&start, Box::new(backend));
            pool.table_mut()
                .get_mut(id)
                .expect("the instance was just created")
                .transition(pemu_api::instance::Lifecycle::Paused, VTime(0))
                .expect("starting -> paused");
            id.to_string()
        });
        (id, journal)
    }

    fn call(name: &str, args: serde_json::Value) -> serde_json::Value {
        let spec =
            pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"));
        (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args.clone())
            .unwrap_or_else(|e| panic!("`{name}` {args} failed: {e:?}"))
            .json
    }

    /// An output object without the instance id, the one field the two spellings cannot share.
    fn without_instance(mut value: serde_json::Value) -> serde_json::Value {
        if let Some(object) = value.as_object_mut() {
            object.remove("instance");
        }
        value
    }

    fn state_hash(id: &str) -> [u8; 32] {
        pemu_api::commands::start::with_pool(|pool| {
            let id = pemu_api::instance::InstanceId::parse(id).expect("an id");
            pool.session_mut(id)
                .expect("the instance is not checked out")
                .snapshot_machine()
                .state_hash()
        })
    }

    fn destroy(id: &str) {
        pemu_api::commands::start::with_pool(|pool| {
            let id = pemu_api::instance::InstanceId::parse(id).expect("an id");
            pool.destroy(id).expect("the instance ends");
        });
    }

    /// The `power` caps group adds no stimulus of its own, so a journal recorded with `power` and
    /// `usb` replays exactly like one recorded with `input` and `env`.
    ///
    /// Two instances with the same image and seed are driven one with each spelling. Each pair must
    /// answer with the same output (the instance id aside), both must journal the same inputs at the
    /// same instants with the same `Origin`, and they must end on the same `state_hash`.
    #[test]
    fn t0_m10_power_and_usb_journal_what_input_and_env_journal() {
        let test = "t0_m10_power_and_usb_journal_what_input_and_env_journal";
        let (group, group_journal) = instance();
        let (plain, plain_journal) = instance();
        assert_ne!(group, plain, "two instances");

        // `duration` is named on both sides and kept short: this is about the spellings agreeing.
        let pairs: Vec<(&str, serde_json::Value, &str, serde_json::Value)> = vec![
            (
                "power",
                serde_json::json!({"op": "press", "duration": "10ms"}),
                "input",
                serde_json::json!({"button": "power", "action": "click", "duration": "10ms"}),
            ),
            (
                "power",
                serde_json::json!({"op": "hold"}),
                "input",
                serde_json::json!({"button": "power", "action": "press"}),
            ),
            (
                "power",
                serde_json::json!({"op": "release"}),
                "input",
                serde_json::json!({"button": "power", "action": "release"}),
            ),
            (
                "power",
                serde_json::json!({"op": "battery", "mv": 3500, "soc": 15, "temp_c": -5}),
                "env",
                serde_json::json!({"battery": {"mv": 3500, "soc": 15, "temp_c": -5}}),
            ),
            (
                "power",
                serde_json::json!({"op": "battery", "present": false}),
                "env",
                serde_json::json!({"battery": {"present": false}}),
            ),
            (
                "usb",
                serde_json::json!({"state": "host"}),
                "env",
                serde_json::json!({"usb": "host"}),
            ),
            (
                "usb",
                serde_json::json!({"cable": "out"}),
                "input",
                serde_json::json!({"button": "usb", "action": "unplug"}),
            ),
            (
                "usb",
                serde_json::json!({"cable": "in"}),
                "input",
                serde_json::json!({"button": "usb", "action": "plug"}),
            ),
            (
                "usb",
                serde_json::json!({"client": "open"}),
                "input",
                serde_json::json!({"button": "usb", "action": "open"}),
            ),
            (
                "usb",
                serde_json::json!({"client": "closed"}),
                "input",
                serde_json::json!({"button": "usb", "action": "close"}),
            ),
        ];

        for (group_name, group_args, plain_name, plain_args) in &pairs {
            let mut group_args = group_args.clone();
            group_args["instance"] = group.as_str().into();
            let mut plain_args = plain_args.clone();
            plain_args["instance"] = plain.as_str().into();
            let got = without_instance(call(group_name, group_args.clone()));
            let want = without_instance(call(plain_name, plain_args.clone()));
            assert_eq!(
                got, want,
                "`{group_name}` {group_args} and `{plain_name}` {plain_args} answer differently"
            );
        }

        let group_journal = group_journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let plain_journal = plain_journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert!(
            group_journal.len() >= pairs.len(),
            "every call journaled something: {group_journal:?}"
        );
        assert_eq!(
            group_journal, plain_journal,
            "the two spellings journaled different inputs"
        );
        assert_eq!(
            state_hash(&group),
            state_hash(&plain),
            "the two spellings left the machines in different states"
        );
        println!(
            "RAN {test}: {} calls, {} journaled inputs, one state hash",
            pairs.len(),
            group_journal.len()
        );

        destroy(&group);
        destroy(&plain);
    }

    /// The wrapper answers for the machine on the whole `SnapshotMachine` surface, defaulted methods
    /// included. `Pool::attach` builds the instance's `SecretSet` from `secret_sources` once, so a
    /// wrapper that took the empty default would export with nothing to redact.
    #[test]
    fn t0_m10_the_recording_backend_answers_for_the_machine_it_wraps() {
        let bare = machine();
        let wrapped = Recording {
            inner: machine(),
            journal: Journaled::default(),
        };
        for view in [FlashView::Image, FlashView::Current] {
            let sources = SnapshotMachine::secret_sources(&wrapped, view);
            assert_eq!(
                sources,
                SnapshotMachine::secret_sources(&bare, view),
                "{view:?}"
            );
            assert!(
                !sources.cardid_window.is_empty(),
                "{view:?}: the default would have been empty"
            );
        }
        assert_eq!(
            SnapshotMachine::secret_generation(&wrapped),
            SnapshotMachine::secret_generation(&bare)
        );
        assert_eq!(
            SnapshotMachine::interrupt_can_wake(&wrapped),
            SnapshotMachine::interrupt_can_wake(&bare)
        );
        let opts = pemu_core::snap::SnapOpts::default();
        assert_eq!(
            SnapshotMachine::snapshot_header(&wrapped, opts).expect("a header"),
            SnapshotMachine::snapshot_header(&bare, opts).expect("a header")
        );
        assert_eq!(
            SnapshotMachine::state_hash(&wrapped),
            SnapshotMachine::state_hash(&bare)
        );
    }

    /// `power` and `usb` are `input` or `env` under another name, so they carry the annotations of
    /// the stricter of the two; `advances_time` also decides which lifecycle states accept a call.
    #[test]
    fn t0_m10_power_and_usb_carry_the_annotations_of_input_and_env() {
        use pemu_api::spec::CapsGroup;

        let find = |name: &str| {
            pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"))
        };
        let (input, env) = (find("input"), find("env"));
        // The two halves differ in exactly one annotation, which is why the group takes `input`'s.
        assert!(
            input.annotations.advances_time && !env.annotations.advances_time,
            "`input` is the stricter half: {:?} vs {:?}",
            input.annotations,
            env.annotations
        );
        assert_eq!(
            pemu_api::spec::Annotations {
                advances_time: false,
                ..input.annotations
            },
            env.annotations,
            "`input` and `env` agree on every other annotation"
        );
        for name in ["power", "usb"] {
            let spec = find(name);
            assert_eq!(spec.group, CapsGroup::Power, "the `power` group");
            assert_eq!(
                spec.annotations, input.annotations,
                "`{name}` becomes an `input` or an `env` call and must carry the stricter of the \
                 two commands' annotations"
            );
        }
    }
}

// ------------------------------------------------------------------------------------------------
// The planner rehearsal against an emulated instance
// ------------------------------------------------------------------------------------------------

mod planner_rehearsal {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use pemu_host::endpoints::live::LiveRunner;
    use pemu_host::endpoints::tcp::TcpOptions;
    use pemu_machine::machine::{Machine, MachineApi};
    use pemu_planner::flow::{BootCheck, FlowReport, SessionError, Step, rehearse};
    use pemu_planner::plan::{ImageSource, Origin, PlanRequest, plan_flash};
    use pemu_planner::rehearse::{
        BootConsole, EsptoolCommand, EsptoolSession, EsptoolSources, FileProbe, HostOs, Invocation,
        ProcessOutput, ProcessRunner, RegionMd5, check_invocation, resolve_and_check_esptool,
    };
    use pemu_planner::rules::{CARDID_OFFSET, CARDID_SIZE, DeviceFacts, Rule};

    use super::{DEVICE_FACTS, common, names_a_serial_device, usj_endpoints};

    const PROCESS_TIMEOUT: Duration = Duration::from_secs(300);
    const BOOT_MS: u64 = 3_000;

    /// The planner's own cardid pattern, which `pemu_host::device::run_flash` seeds the rehearsal
    /// instance with; the target must keep it bit-identical.
    fn synthetic_cardid() -> Vec<u8> {
        pemu_planner::rules::synthetic_cardid()
    }

    struct HostFiles;

    impl FileProbe for HostFiles {
        fn is_file(&self, path: &str) -> bool {
            Path::new(path).is_file()
        }
    }

    /// The rehearsal's process runner: the resolved esptool with an argument vector and no shell, in
    /// a scratch directory, every vector through the planner's own gate first. It asserts only an
    /// emulator URL is ever spawned against.
    struct RehearsalRunner {
        command: EsptoolCommand,
        scratch: PathBuf,
        counter: u32,
        wall: Vec<(String, Duration)>,
        transcripts: Vec<String>,
    }

    impl RehearsalRunner {
        fn new(command: &EsptoolCommand, scratch: &Path) -> RehearsalRunner {
            RehearsalRunner {
                command: command.clone(),
                scratch: scratch.to_path_buf(),
                counter: 0,
                wall: Vec::new(),
                transcripts: Vec::new(),
            }
        }
    }

    impl ProcessRunner for RehearsalRunner {
        fn run(&mut self, invocation: &Invocation) -> Result<ProcessOutput, String> {
            let scratch = self.scratch.clone();
            let file_len = |path: &str| {
                let path = Path::new(path);
                path.starts_with(&scratch)
                    .then(|| std::fs::metadata(path).ok().map(|m| m.len()))
                    .flatten()
            };
            check_invocation(&self.command, invocation, &file_len).map_err(|r| r.to_string())?;
            let port = invocation
                .args
                .iter()
                .position(|a| a == "--port")
                .and_then(|i| invocation.args.get(i + 1))
                .cloned()
                .unwrap_or_default();
            assert!(
                port.is_empty() || port.starts_with("rfc2217://127.0.0.1:"),
                "an emulator URL, never a device: {port}"
            );
            let started = Instant::now();
            let output = run_vector(invocation, &self.scratch);
            // The label is the subcommand, never an argument: a test log carries no host path.
            let label = invocation
                .args
                .iter()
                .position(|a| a == "--after")
                .and_then(|i| invocation.args.get(i + 2))
                .cloned()
                .unwrap_or_else(|| "region-md5".to_owned());
            self.wall.push((label, started.elapsed()));
            let out = output?;
            self.transcripts.push(out.output.clone());
            Ok(out)
        }

        fn scratch_path(&mut self, name: &str) -> String {
            self.counter += 1;
            self.scratch
                .join(format!("{}-{name}", self.counter))
                .to_string_lossy()
                .into_owned()
        }

        fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
            std::fs::write(path, bytes).map_err(|e| format!("scratch file: {e}"))
        }

        fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
            std::fs::read(path).map_err(|e| format!("scratch file: {e}"))
        }

        fn remove_file(&mut self, path: &str) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// One process, drained on its own threads and killed at [`PROCESS_TIMEOUT`].
    fn run_vector(invocation: &Invocation, cwd: &Path) -> Result<ProcessOutput, String> {
        let started = Instant::now();
        let mut child = Command::new(&invocation.program)
            .args(&invocation.args)
            .current_dir(cwd)
            .env_remove("PYTHONPATH")
            .env_remove("PYTHONHOME")
            .env_remove("ESPTOOL_PORT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn: {e}"))?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let err = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
            s
        });
        let out = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
            s
        });
        loop {
            if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                break;
            }
            if started.elapsed() > PROCESS_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the process passed its deadline and was stopped".to_owned());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut text = out.join().unwrap_or_default();
        text.push_str(&err.join().unwrap_or_default());
        let success = child.try_wait().ok().flatten().is_some_and(|s| s.success());
        Ok(ProcessOutput {
            success,
            output: text,
        })
    }

    /// The instance supplies the region digest over its own flash, as the stub does on the device.
    struct InstanceMd5<'a>(&'a LiveRunner<Machine>);

    impl RegionMd5 for InstanceMd5<'_> {
        fn region_md5(&mut self, offset: u32, size: u32) -> Result<[u8; 16], SessionError> {
            let size = size as usize;
            self.0
                .call(move |machine: &mut Machine| {
                    let mut region = vec![0u8; size];
                    machine.flash_read(offset, &mut region);
                    pemu_planner::md5::digest(&region)
                })
                .ok_or_else(|| SessionError::Failed("the instance stopped".to_owned()))
        }
    }

    /// After esptool's hard reset no host tool is connected, so the endpoint leaves the instance
    /// idle; the job gives it the boot it needs and returns the console since the cursor.
    struct InstanceConsole<'a> {
        runner: &'a LiveRunner<Machine>,
        cursor: u64,
    }

    impl BootConsole for InstanceConsole<'_> {
        fn boot_log(&mut self) -> Result<String, SessionError> {
            let cursor = self.cursor;
            let bytes = self
                .runner
                .call(move |machine: &mut Machine| {
                    let until = pemu_core::time::VTime(
                        machine.now().0 + pemu_core::time::VTime::from_ms(BOOT_MS).0,
                    );
                    MachineApi::run(
                        machine,
                        pemu_machine::run::RunLimits {
                            until: Some(until),
                            max_insns: None,
                            stops: pemu_machine::stops::StopSet::default(),
                        },
                    );
                    MachineApi::io(machine)
                        .usj_tx
                        .slices(cursor)
                        .iter()
                        .copied()
                        .collect::<Vec<u8>>()
                })
                .ok_or_else(|| SessionError::Failed("the instance stopped".to_owned()))?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            // The boot after the last reset, as the device reader returns it.
            Ok(usj_endpoints::after_last(&text, "rst:0x")
                .map_or(text.clone(), |boot| format!("rst:0x{}", &boot[6..])))
        }
    }

    /// The planner against an emulated `official` over RFC 2217: an image whose cardid window
    /// carries data is refused; the accepted plan is rehearsed through the identical esptool
    /// invocation, its boot check passes, and the synthetic cardid stays bit-identical.
    #[test]
    fn t1_m10_planner_rehearsal_on_an_emulated_official_instance() {
        let test = "t1_m10_planner_rehearsal_on_an_emulated_official_instance";
        let Some(image) = usj_endpoints::official_or_skip(test) else {
            return;
        };
        let facts = DeviceFacts::from_toml(DEVICE_FACTS).expect("the committed fixture parses");

        // No process, no port: a non-erased cardid window would be dropped or overwritten.
        let mut overlapping = image.clone();
        overlapping[CARDID_OFFSET as usize..(CARDID_OFFSET + CARDID_SIZE) as usize]
            .copy_from_slice(&synthetic_cardid());
        let refused = plan_flash(
            &PlanRequest::write(ImageSource::Merged(&overlapping), Origin::HumanCli),
            Some(&facts),
        );
        let rules: Vec<Rule> = refused.refusals.iter().map(|r| r.rule).collect();
        assert!(
            rules.contains(&Rule::DataDropped) || rules.contains(&Rule::CardidOverlap),
            "a fixture overlapping [0x356000, 0x35A000) is refused: {rules:?}"
        );
        assert!(refused.accepted().is_none());

        if cfg!(debug_assertions) {
            // The stub's deflate is too slow in a debug build, so this leg needs a release build.
            common::skip(
                test,
                "the rehearsal write needs a release build (cargo test --release)",
            );
            return;
        }
        let Some(python) = usj_endpoints::esptool_or_skip(test) else {
            return;
        };

        let accepted = plan_flash(
            &PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli),
            Some(&facts),
        );
        let plan = accepted
            .accepted()
            .unwrap_or_else(|| panic!("the corpus image plans: {:?}", accepted.refusals))
            .clone();
        assert!(
            plan.writes.iter().any(|w| w.offset == 0x1_0000),
            "the plan writes the app"
        );

        // Exactly what the product builds its rehearsal instance from (`pemu_host::device`).
        let target_image = pemu_planner::rules::with_synthetic_cardid(&image);
        let dir = usj_endpoints::scratch(test);
        let live = usj_endpoints::Live::start(&target_image, TcpOptions::default());
        let url = live.endpoint.rfc2217_url();

        let sources = EsptoolSources {
            explicit: Some(python.to_string_lossy().into_owned()),
            ..EsptoolSources::default()
        };
        let mut resolver = RehearsalRunner::new(
            &EsptoolCommand {
                program: python.to_string_lossy().into_owned(),
                prefix: vec!["-I".to_owned(), "-m".to_owned(), "esptool".to_owned()],
                major: 4,
            },
            &dir,
        );
        let command = resolve_and_check_esptool(&sources, HostOs::MacOs, &HostFiles, &mut resolver)
            .expect("an IDF esptool of at least 4.12");

        let mut runner = RehearsalRunner::new(&command, &dir);
        let mut md5 = InstanceMd5(&live.runner);
        let mut console = InstanceConsole {
            runner: &live.runner,
            cursor: live.cursor,
        };
        let mut report = FlowReport::default();
        let request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
        let result = {
            let mut session = EsptoolSession::emulator(
                command.clone(),
                &url,
                &mut runner,
                &mut md5,
                &mut console,
            )
            .expect("an emulator URL is allowed");
            rehearse(&request, &plan, &mut session, &mut report)
        };
        for (what, wall) in &runner.wall {
            println!("planner rehearsal wall: {what} {:.1} s", wall.as_secs_f64());
        }
        // The device's digest path, `SPI_FLASH_MD5` through the flasher stub, over the same endpoint,
        // must agree with the instance's own digest.
        let stub_digest = {
            let script = runner.scratch_path(pemu_planner::stub_md5::HELPER_NAME);
            runner
                .write_file(&script, pemu_planner::stub_md5::HELPER_SOURCE.as_bytes())
                .expect("the helper is written");
            let invocation = pemu_planner::stub_md5::helper_invocation(
                &command,
                &script,
                &url,
                CARDID_OFFSET,
                CARDID_SIZE,
            );
            let out = runner.run(&invocation).expect("the helper runs");
            runner.remove_file(&script);
            assert!(out.success, "the region-MD5 helper failed:\n{}", out.output);
            pemu_planner::stub_md5::parse_digest(&out.output).expect("a digest")
        };
        let instance_digest = md5
            .region_md5(CARDID_OFFSET, CARDID_SIZE)
            .expect("instance");
        assert_eq!(
            stub_digest, instance_digest,
            "the flasher stub's SPI_FLASH_MD5 and the instance's own digest agree"
        );

        let transcripts = runner.transcripts.join("\n");
        let (machine, _) = live.finish(0);
        let _ = std::fs::remove_dir_all(&dir);

        result.unwrap_or_else(|e| panic!("the rehearsal failed: {e:?}\n{transcripts}"));
        assert_eq!(
            report.rehearsal.step_order(),
            [Step::Guard, Step::Write, Step::Verify, Step::BootCheck]
        );
        assert_eq!(
            report.rehearsal.cardid_unchanged,
            Some(true),
            "the synthetic cardid digest is unchanged"
        );
        assert_eq!(
            report.rehearsal.boot,
            Some(BootCheck {
                banner: true,
                elf_sha256_matches: true
            })
        );
        assert_eq!(report.device.steps, [(Step::Rehearse, true)]);

        // The write happened, and the cardid window is bit-identical, not only equal by digest.
        let app = usj_endpoints::app_bytes(&image);
        let mut written = vec![0u8; app.len()];
        machine.flash_read(usj_endpoints::APP_OFFSET as u32, &mut written);
        assert!(written == app, "the flash holds the app the plan wrote");
        let mut cardid = vec![0u8; CARDID_SIZE as usize];
        machine.flash_read(CARDID_OFFSET, &mut cardid);
        assert!(cardid == synthetic_cardid(), "cardid is bit-identical");

        // No output names a host serial device, and none carries a cardid byte.
        assert!(!names_a_serial_device(&transcripts), "{transcripts}");
        assert!(!names_a_serial_device(&format!("{report:?}")));
        let window = pemu_loader::hex(&cardid[..16]);
        assert!(
            !transcripts.contains(&window),
            "the cardid window's first bytes in an output"
        );
    }
}

// ------------------------------------------------------------------------------------------------
// `limits-flash`
// ------------------------------------------------------------------------------------------------

/// `limits-flash`, both halves in one test:
///
/// 1. **The planner and loader refuse an app above 0x300000** ([`pemu_planner::rules::APP_MAX`]).
/// 2. **A guest flash access beyond 0x800000 reaches the cell 8 MB below it**: class A for a read,
///    UNVERIFIED for program and erase.
#[test]
fn t0_m10_limits_flash_app_size_refusal_and_beyond_the_part() {
    let test = "t0_m10_limits_flash_app_size_refusal_and_beyond_the_part";

    flash_limits::the_planner_refuses_an_app_above_the_bound();
    flash_limits::a_guest_access_beyond_the_part_reaches_the_mirror_below_it();

    println!(
        "RAN {test} app-size-and-beyond-the-part: an app above {:#x} is refused in both input \
         forms; a guest SPI1 read, program and erase at or above {:#x} reach the cell 8 MB below \
         (the read class A from device-probe_campaign_regs-20260924T164141Z; program and erase \
         UNVERIFIED)",
        pemu_planner::rules::APP_MAX,
        pemu_soc_c3::mem::FLASH_LEN,
    );
}

mod flash_limits {
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;
    use pemu_core::sched::Scheduler;
    use pemu_core::time::VTime;
    use pemu_loader::partitions::Partition;
    use pemu_planner::plan::{
        ImageSource, InputSegment, Origin, PlanRequest, encode_partition_table, plan_flash,
    };
    use pemu_planner::rules::{APP_MAX, Rule};
    use pemu_soc_c3::flash_store::{ERASED, FlashStore};
    use pemu_soc_c3::r#gen::regs_spi1::{REGS, idx};
    use pemu_soc_c3::mem::FLASH_LEN;
    use pemu_soc_c3::periph::flash_xmc::{Written, op};
    use pemu_soc_c3::periph::spi_mem::{Spi1Model, Started, cmd};

    use super::{refusal_facts, refusal_image};

    /// Block offsets from the generated register table.
    fn off(index: usize) -> u32 {
        u32::from(REGS[index].off)
    }

    /// The `factory` partition of the device facts, grown to `size` so an app above [`APP_MAX`] still
    /// ends before the cardid window at 0x356000.
    fn layout_with_factory(size: u32) -> Vec<Partition> {
        let mut layout = refusal_facts().partitions;
        let factory = layout
            .iter_mut()
            .find(|p| p.name == "factory")
            .expect("the fixture has a `factory` app partition");
        assert_eq!(factory.offset, 0x1_0000);
        assert_eq!(factory.size, APP_MAX, "the fixture app slot is the bound");
        factory.size = size;
        assert!(
            0x1_0000 + size <= 0x35_6000,
            "the grown slot still ends before the cardid window"
        );
        layout
    }

    /// An app above [`APP_MAX`] is refused in both input forms (a merged image and a segment list),
    /// and an app inside the bound is not.
    pub fn the_planner_refuses_an_app_above_the_bound() {
        let facts = refusal_facts();

        let layout = layout_with_factory(0x34_0000);
        let mut image = refusal_image(&layout);
        let app = 0x1_0000usize;
        image[app..app + APP_MAX as usize + 1].fill(0x22);
        image[app] = 0xE9;
        let merged = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
        let outcome = plan_flash(&merged, Some(&facts));
        assert!(
            outcome.refused_by(Rule::AppTooLarge) && outcome.accepted().is_none(),
            "a merged image with an app of {:#x} bytes must fire {}: {:?}",
            APP_MAX + 1,
            Rule::AppTooLarge.id(),
            outcome.refusals
        );

        let table = encode_partition_table(&facts.partitions);
        let mut big = vec![0x22u8; APP_MAX as usize + 1];
        big[0] = 0xE9;
        let segments = [
            InputSegment {
                name: "partition-table.bin",
                offset: 0x8000,
                data: &table,
            },
            InputSegment {
                name: "app.bin",
                offset: 0x1_0000,
                data: &big,
            },
        ];
        let request = PlanRequest::write(ImageSource::Segments(&segments), Origin::HumanCli);
        let outcome = plan_flash(&request, Some(&facts));
        assert!(
            outcome.refused_by(Rule::AppTooLarge) && outcome.accepted().is_none(),
            "a segment list with an app of {:#x} bytes must fire {}: {:?}",
            APP_MAX + 1,
            Rule::AppTooLarge.id(),
            outcome.refusals
        );

        // The fixture's own image, whose app sits in the unmodified `factory` slot, still plans.
        let ok = refusal_image(&facts.partitions);
        let request = PlanRequest::write(ImageSource::Merged(&ok), Origin::HumanCli);
        let outcome = plan_flash(&request, Some(&facts));
        assert!(
            outcome.accepted().is_some() && !outcome.refused_by(Rule::AppTooLarge),
            "an app inside the bound is not refused: {:?}",
            outcome.refusals
        );
    }

    /// The SPI1 command host, its part and the array behind it, driven as the machine drives them:
    /// a register write, the array half, then the events that came due. No machine needed.
    struct Guest {
        spi1: Spi1Model,
        flash: FlashStore,
        sched: Scheduler,
        ledger: FidelityLedger,
        now: VTime,
        last: Started,
    }

    impl Guest {
        fn new() -> Guest {
            Guest {
                spi1: Spi1Model::default(),
                flash: FlashStore::erased(),
                sched: Scheduler::default(),
                ledger: FidelityLedger::default(),
                now: VTime(0),
                last: Started::default(),
            }
        }

        fn read(&mut self, at: u32) -> u32 {
            self.spi1.load(at, Size::B4, self.now, &mut self.ledger)
        }

        fn write(&mut self, at: u32, val: u32) -> Option<Written> {
            self.last = self.spi1.store(
                at,
                Size::B4,
                val,
                self.now,
                &mut self.ledger,
                &mut self.sched,
            );
            if self.last.stop {
                return self.spi1.service(&mut self.flash);
            }
            None
        }

        /// Delivers the `WIP` completion the run loop would deliver between instructions.
        fn settle(&mut self) {
            if let Some(t) = self.sched.next_time() {
                self.now = t;
            }
            while self.sched.pop_due(self.now).is_some() {
                self.spi1.complete();
            }
        }

        fn write_enable(&mut self) {
            self.write(off(idx::SPI_MEM_CMD), cmd::FLASH_WREN);
        }

        fn buffer(&mut self, len: usize) -> Vec<u8> {
            (0..len)
                .map(|i| {
                    let word = self.read(off(idx::SPI_MEM_W0) + 4 * (i as u32 / 4));
                    (word >> (8 * (i % 4))) as u8
                })
                .collect()
        }

        /// A `READ` user transaction of `len` bytes at `addr`, as IDF spells one.
        fn usr_read(&mut self, addr: u32, len: u32) -> Vec<u8> {
            self.write(off(idx::SPI_MEM_USER), 1 << 31 | 1 << 30 | 1 << 28);
            self.write(off(idx::SPI_MEM_USER1), 23 << 26);
            self.write(off(idx::SPI_MEM_USER2), 7 << 28 | u32::from(op::READ));
            self.write(off(idx::SPI_MEM_MISO_DLEN), len * 8 - 1);
            self.write(off(idx::SPI_MEM_ADDR), addr);
            self.write(off(idx::SPI_MEM_CMD), cmd::USR);
            self.buffer(len as usize)
        }
    }

    /// A guest transaction at or above [`FLASH_LEN`] through the SPI1 registers reaches
    /// the cell 8 MB below: the part decodes the low 23 bits, so a program or erase changes and
    /// reports the mirrored page.
    ///
    /// The read is class A (`device-probe_campaign_regs-20260924T164141Z`, `FLASH|read_0x800000`,
    /// `same_as_0x000000=1`, `ff_bytes=0`); program and erase take the same decoder and are
    /// UNVERIFIED, because no device run writes the flash.
    pub fn a_guest_access_beyond_the_part_reaches_the_mirror_below_it() {
        let (cmd_reg, addr_reg, w0) = (
            off(idx::SPI_MEM_CMD),
            off(idx::SPI_MEM_ADDR),
            off(idx::SPI_MEM_W0),
        );
        let mut g = Guest::new();

        // A witness at the start of the part, which a read above mirrors, and one in its last page,
        // which nothing below may change.
        g.flash.program(0x10, &[0x5A; 4]);
        let last_page = FLASH_LEN - 0x1000;
        g.flash.program(last_page, &[0xA5; 4]);
        assert_eq!(
            g.usr_read(FLASH_LEN + 0x10, 4),
            vec![0x5A; 4],
            "a read at {:#x} gives what 0x10 holds",
            FLASH_LEN + 0x10
        );
        assert_eq!(
            g.usr_read(last_page, 4),
            vec![0xA5; 4],
            "the last page of the part reads what was programmed"
        );

        // A page program at the first address the part does not have lands on address 0.
        g.write(w0, 0);
        g.write_enable();
        g.write(addr_reg, 0x0400_0000 | FLASH_LEN);
        assert_eq!(
            g.write(cmd_reg, cmd::FLASH_PP),
            Some(Written {
                first_page: 0,
                pages: 1
            }),
            "a program at {FLASH_LEN:#x} writes the mirrored page 0"
        );
        g.settle();
        assert_eq!(g.flash.read_byte(0), 0x00, "the program landed at 0");

        // An erase above the part erases the mirrored sector, and reports that page.
        g.write_enable();
        g.write(addr_reg, FLASH_LEN + 0x10);
        assert_eq!(
            g.write(cmd_reg, cmd::FLASH_SE),
            Some(Written {
                first_page: 0,
                pages: 1
            }),
            "an erase at {:#x} erases sector 0",
            FLASH_LEN + 0x10
        );
        g.settle();
        assert_eq!(g.usr_read(0x10, 4), vec![ERASED; 4], "sector 0 is erased");
        assert_eq!(
            g.usr_read(last_page, 4),
            vec![0xA5; 4],
            "the last page of the part is still what it was"
        );
    }
}

// ------------------------------------------------------------------------------------------------
// The USB link state of a running `pk`
// ------------------------------------------------------------------------------------------------

/// Moving a running `pk` from U3 to U0 for five seconds and back produces its link-state change
/// lines and no panic; see `usb_link_state::run`.
#[test]
fn t1_m10_usb_u3_to_u0_and_back_moves_the_pk_link_state_without_a_panic() {
    let test = "t1_m10_usb_u3_to_u0_and_back_moves_the_pk_link_state_without_a_panic";
    usb_link_state::run(test);
}

mod usb_link_state {
    use pemu_core::hostio::SerialStream;

    use super::common;

    /// `pk_ui.h` `pk_ui_link_t`, and the `-1` `pk_app.c` starts `s_ui_link` at.
    const LINK_UNSET: i64 = -1;
    const LINK_WAITING: i64 = 0;
    const LINK_USB: i64 = 2;

    /// The file static of `pk_app.c`, named with its compilation unit.
    const LINK_VAR: &str = "pk_app.c::s_ui_link";

    /// A run that hits the panic handler fails rather than running on.
    const NO_PANIC: &str = "serial:/Guru Meditation/";

    /// The app's own handshake, as `pk_protocol.c` parses it off the USJ RX line.
    const HELLO: &str = "{\"cmd\":\"hello\"}";

    use crate::common::workspace;

    /// Installs the host's real hooks over the corpus `pk` image and ELF, whose DWARF lets
    /// `inspect vars` read `s_ui_link`. `None` after a printed skip.
    fn hooked_pk(test: &str) -> Option<std::sync::MutexGuard<'static, ()>> {
        use std::sync::Arc;

        use super::HOOKED;

        let bin = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")?;
        let elf = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport.elf")?;
        let guard = HOOKED.lock().unwrap_or_else(|e| e.into_inner());
        let image = Arc::new(std::fs::read(bin).expect("the verified corpus image is readable"));
        let context = Arc::new(
            pemu_host::hooks::ElfContext::parse(
                &std::fs::read(elf).expect("the verified corpus ELF is readable"),
            )
            .expect("the ELF parses"),
        );
        pemu_host::backend::install(
            Arc::new(move |fw: &str| match fw {
                common::PK => pemu_host::backend::merged_image(&image),
                other => Err(pemu_api::commands::start::firmware_not_found(other)),
            }),
            None,
            pemu_host::audio_root::AudioRoot::new(std::env::temp_dir().join("pemu-m10-no-audio")),
        );
        pemu_host::hooks::install(pemu_host::hooks::HostHooks {
            elves: Arc::new(move |fw: &str| (fw == common::PK).then(|| Arc::clone(&context))),
            scenario_root: pemu_host::hooks::ScenarioRoot::new(
                Some(workspace()),
                vec![workspace()],
            ),
            salt_dir: None,
        });
        pemu_host::boot_cache::install(None);
        Some(guard)
    }

    fn call(name: &str, args: serde_json::Value) -> serde_json::Value {
        let spec =
            pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"));
        (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args.clone())
            .unwrap_or_else(|e| panic!("`{name}` {args} failed: {e:?}"))
            .json
    }

    fn with_session<R>(
        inst: &str,
        f: impl FnOnce(&mut pemu_api::commands::start::Session) -> R,
    ) -> R {
        let id = pemu_api::instance::InstanceId::parse(inst).expect("an id");
        pemu_api::commands::start::with_pool(|pool| {
            f(pool
                .session_mut(id)
                .expect("the instance is not checked out"))
        })
    }

    fn console_from(inst: &str, from: u64) -> String {
        with_session(inst, |s| {
            let ring = s.machine().io().serial_ring(SerialStream::UsjTx);
            let bytes: Vec<u8> = ring.slices(from).iter().copied().collect();
            String::from_utf8_lossy(&bytes).into_owned()
        })
    }

    fn console_head(inst: &str) -> u64 {
        with_session(inst, |s| {
            s.machine().io().serial_ring(SerialStream::UsjTx).head()
        })
    }

    fn now_us(inst: &str) -> u64 {
        with_session(inst, |s| s.now().as_us())
    }

    fn link_state(inst: &str) -> i64 {
        let out = call(
            "inspect",
            serde_json::json!({"instance": inst, "what": ["vars"], "vars": [LINK_VAR]}),
        );
        let read = out["vars"]
            .as_array()
            .and_then(|a| a.first().cloned())
            .unwrap_or_else(|| panic!("`inspect vars` answered one row: {out}"));
        assert!(
            read.get("unreadable").is_none(),
            "`{LINK_VAR}` is not readable: {read}. The corpus ELF must carry the symbol and its \
             DWARF; if it is ever stripped, this row needs another witness."
        );
        assert_eq!(read["name"], "s_ui_link", "{read}");
        assert_eq!(read["bytes"], 4, "`s_ui_link` is an `int`: {read}");
        read["value"]
            .as_i64()
            .unwrap_or_else(|| panic!("an integer value: {read}"))
    }

    /// The app's hello over the USJ RX line, which makes `host_alive(PK_LINK_USB)` hold.
    fn say_hello(inst: &str) {
        call(
            "serial",
            serde_json::json!({
                "instance": inst, "op": "write", "text": HELLO, "newline": true
            }),
        );
    }

    fn run_until(inst: &str, until: &str, timeout: &str) -> serde_json::Value {
        call(
            "run",
            serde_json::json!({
                "instance": inst, "until": until, "timeout": timeout, "fail_if": [NO_PANIC]
            }),
        )
    }

    fn run_for(inst: &str, duration: &str) -> serde_json::Value {
        call(
            "run",
            serde_json::json!({"instance": inst, "for": duration, "fail_if": [NO_PANIC]}),
        )
    }

    /// The witness is the guest's own `s_ui_link` through `inspect vars`, not the console: unplugged,
    /// the guest runs on battery but the USJ discards text while no host is attached, so a missing
    /// line proves nothing.
    ///
    /// The asserted values are the `pk_ui_link_t` transitions of `refresh_link_state`: 0 `WAITING` at
    /// boot, 2 `USB` after a hello, 0 after 5 s unplugged, 2 after the link and a hello come back.
    pub fn run(test: &str) {
        let Some(_hooked) = hooked_pk(test) else {
            return;
        };
        let started = call(
            "start",
            serde_json::json!({"fw": common::PK, "boot": "none"}),
        );
        let inst = started["instance"]
            .as_str()
            .expect("an instance id")
            .to_owned();

        // 1. Boot until `refresh_link_state` has run (around 662 ms; the wait is generous).
        //    `serial:~"..."` is the contains form; the line carries an `I (662) ` prefix.
        let boot = run_until(&inst, "serial:~\"pk_app: link state -1 -> 0\"", "20s");
        assert_eq!(boot["status"], "matched", "the boot anchor: {boot}");
        // `refresh_link_state` logs the change and then assigns `s_ui_link`; one loop period retires
        // the store.
        run_for(&inst, "250ms");
        let anchor_us = now_us(&inst);
        assert_eq!(
            link_state(&inst),
            LINK_WAITING,
            "after the boot line `link state {LINK_UNSET} -> {LINK_WAITING}`"
        );

        // 2. U3 with an app talking: the hello makes `refresh_link_state` pick PK_UI_LINK_USB.
        say_hello(&inst);
        let up = run_until(&inst, &format!("var:{LINK_VAR} == {LINK_USB}"), "5s");
        assert_eq!(up["status"], "matched", "the USB link comes up: {up}");
        assert_eq!(link_state(&inst), LINK_USB);
        let before_unplug = console_head(&inst);

        // 3. U3 to U0, held for five seconds, through the `usb` command.
        call(
            "usb",
            serde_json::json!({"instance": &inst, "state": "unplugged"}),
        );
        let held = run_for(&inst, "5s");
        assert_eq!(held["status"], "elapsed", "the five seconds ran: {held}");

        // 4. No USB link, no BLE host alive or subscribed: the chain falls to PK_UI_LINK_WAITING.
        //    PK_UI_LINK_BLE_PENDING (1) and PK_UI_LINK_BLE (3) would also differ and be wrong.
        assert_eq!(
            link_state(&inst),
            LINK_WAITING,
            "unplugged, `refresh_link_state` falls to PK_UI_LINK_WAITING"
        );
        // What the guest printed in that window did not reach the host.
        let dropped_window = console_head(&inst) - before_unplug;

        // 5. Back to U3 with a client open, and the app handshakes again. Now the line lands.
        call(
            "usb",
            serde_json::json!({"instance": &inst, "state": "open"}),
        );
        run_for(&inst, "500ms");
        say_hello(&inst);
        let back = run_until(&inst, &format!("var:{LINK_VAR} == {LINK_USB}"), "5s");
        assert_eq!(back["status"], "matched", "the link comes back: {back}");
        assert_eq!(link_state(&inst), LINK_USB);
        // From the unplug on, so the first handshake's line cannot stand in.
        let since_unplug = console_from(&inst, before_unplug);
        assert!(
            since_unplug.contains(&format!("pk_app: link state {LINK_WAITING} -> {LINK_USB}")),
            "the console carries the line that lands after the link is back:\n{since_unplug}"
        );
        let text = console_from(&inst, 0);

        // 6. No panic, and the guest still schedules: `inspect tasks` walks a consistent table with the
        //    app task in it, and virtual time advanced.
        assert!(
            !text.contains("Guru Meditation") && !text.contains("abort() was called"),
            "no panic in the whole run"
        );
        let tasks = call(
            "inspect",
            serde_json::json!({"instance": &inst, "what": ["tasks"]}),
        );
        assert_eq!(tasks["tasks"]["consistent"], true, "{tasks}");
        let names: Vec<String> = tasks["tasks"]["tasks"]
            .as_array()
            .expect("a task list")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(
            names.iter().any(|n| n == "pk_app"),
            "the app task is still scheduled: {names:?}"
        );
        let end_us = now_us(&inst);
        assert!(
            end_us > anchor_us + 5_000_000,
            "virtual time advanced past the five-second window: {anchor_us} to {end_us}"
        );

        println!(
            "RAN {test} link-state: s_ui_link {LINK_UNSET} -> {LINK_WAITING} -> {LINK_USB} -> \
             {LINK_WAITING} (5 s unplugged) -> {LINK_USB}; {dropped_window} console byte(s) \
             reached the host while unplugged; {} task(s) still scheduled at {end_us} us",
            names.len()
        );
        call("stop", serde_json::json!({"instance": &inst}));
    }
}

// ------------------------------------------------------------------------------------------------
// What a monitor reset of a running `pk` really produces
// ------------------------------------------------------------------------------------------------

/// Against the device capture `$ROOT/device/e10.2-double-banner-2026-09-21.md` (class A, modem
/// lines and reads only):
///
/// - one entry into (RTS, DTR) = (1, 0) is one chip reset and one banner, so one `idf.py monitor`
///   reset of a running `pk` gives **one** `boot:0xa` banner, not two;
/// - the two banners of `boot_log.bin` lines 1 to 7 are two chip resets close enough together that
///   the second caught the ROM still running, which is why its `Saved PC` is a ROM address;
/// - an entry into (1, 0) whose last flag write was (0, 1) boots `boot:0x6 (DOWNLOAD(USB/UART0))`,
///   one whose last flag write was (0, 0) boots `boot:0xa (SPI_FAST_FLASH_BOOT)`.
///
/// So `UsjModel::line_state` firing on **entry** into (1, 0) is what silicon does.
#[test]
fn t1_m10_monitor_reset_banners_of_a_running_pk() {
    let test = "t1_m10_monitor_reset_banners_of_a_running_pk";
    monitor_reset::run(test);
}

mod monitor_reset {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Duration;

    use pemu_host::endpoints::rfc2217::{IAC, OPT_COM_PORT, SB, SE, WILL, cmd, control};
    use pemu_host::endpoints::tcp::{TcpEndpoint, TcpOptions};
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::elf::ElfInfo;

    use super::{common, usj_endpoints};

    /// The banner shapes of the reset causes, as the ROM prints them.
    const RESET: &str = "rst:0x15 (USB_UART_CHIP_RESET)";
    const FLASH_BOOT: &str = "rst:0x15 (USB_UART_CHIP_RESET),boot:0xa (SPI_FAST_FLASH_BOOT)";
    const DOWNLOAD: &str = "rst:0x15 (USB_UART_CHIP_RESET),boot:0x6 (DOWNLOAD(USB/UART0))";

    /// Mask ROM of the C3. A `Saved PC` inside it means the reset caught the ROM running.
    const ROM: std::ops::Range<u32> =
        pemu_soc_c3::mem::ROM_BASE..pemu_soc_c3::mem::ROM_BASE + pemu_soc_c3::mem::ROM_LEN;

    /// How long the host waits after a line change, so the live guest prints what the change caused.
    const SETTLE: Duration = Duration::from_millis(250);

    /// Virtual milliseconds each instance may run after the endpoint closes while it waits for
    /// [`READY`]: a bound on a wait, not a budget (`pk` prints it about 412 ms after a reset).
    const TAIL_MS: u64 = 20_000;

    /// The line `pk`'s application phase ends with.
    const READY: &str = "pk_app: ready";

    /// A `pk` instance running live behind an RFC 2217 endpoint, with its app ELF bound so the boot
    /// passes BLE init. `None` after a printed skip.
    fn live_pk(test: &str) -> Option<usj_endpoints::Live> {
        let bin = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")?;
        let elf = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport.elf")?;
        let flash = FlashImage::from_merged(&std::fs::read(bin).expect("the corpus image reads"))
            .expect("`pk` is a merged image");
        let elf = Arc::new(
            ElfInfo::parse(&std::fs::read(elf).expect("the corpus ELF reads"))
                .expect("the ELF parses"),
        );
        // `Fast` explicitly: the timing profile is run identity, and this test's anchors assume `fast`.
        let machine = pemu_host::backend::build_machine_with_elf(
            flash,
            Some(elf),
            1,
            pemu_machine::config::TimingProfileId::Fast,
        )
        .expect("the machine builds");
        Some(usj_endpoints::Live::start_machine(
            machine,
            TcpOptions::default(),
        ))
    }

    /// An RFC 2217 client on the endpoint, with a thread draining what the server sends.
    struct Client {
        stream: TcpStream,
    }

    impl Client {
        fn connect(endpoint: &TcpEndpoint) -> Client {
            assert!(endpoint.addr().ip().is_loopback(), "loopback only");
            let mut stream = TcpStream::connect(endpoint.addr()).expect("the endpoint accepts");
            stream
                .write_all(&[IAC, WILL, OPT_COM_PORT])
                .expect("the COM-PORT-OPTION offer");
            let mut drain = stream.try_clone().expect("clone");
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while matches!(drain.read(&mut buf), Ok(n) if n > 0) {}
            });
            Client { stream }
        }

        /// One `SET-CONTROL`, which the endpoint turns into an `InputEvent::UsbLine`.
        fn set(&mut self, value: u8) {
            self.stream
                .write_all(&[IAC, SB, OPT_COM_PORT, cmd::SET_CONTROL, value, IAC, SE])
                .expect("the line state reaches the endpoint");
            std::thread::sleep(SETTLE);
        }

        fn rts(&mut self, on: bool) {
            self.set(if on {
                control::RTS_ON
            } else {
                control::RTS_OFF
            });
        }

        fn dtr(&mut self, on: bool) {
            self.set(if on {
                control::DTR_ON
            } else {
                control::DTR_OFF
            });
        }
    }

    fn banners(console: &str) -> (usize, usize, usize) {
        (
            console.matches(RESET).count(),
            console.matches(FLASH_BOOT).count(),
            console.matches(DOWNLOAD).count(),
        )
    }

    fn saved_pcs(console: &str) -> Vec<u32> {
        console
            .match_indices("Saved PC:0x")
            .map(|(at, marker)| {
                let rest = &console[at + marker.len()..];
                let hex: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
                u32::from_str_radix(&hex, 16).expect("a hex address")
            })
            .collect()
    }

    /// The console of one instance after `drive` walked its line states and the boot after the last
    /// reset reached [`READY`]. It polls instead of arming a matcher, because the boot usually
    /// finishes while the live thread still runs.
    fn console_after(test: &str, drive: impl FnOnce(&mut Client)) -> Option<String> {
        use pemu_machine::machine::MachineApi;

        let live = live_pk(test)?;
        let cursor = live.cursor;
        let mut client = Client::connect(&live.endpoint);
        // The starting pair is (RTS, DTR) = (0, 0): no line has been set on this endpoint.
        drive(&mut client);
        drop(client);
        let (mut machine, _) = live.finish(0);

        let ms = pemu_core::time::VTime::from_ms;
        let deadline = pemu_core::time::VTime(machine.now().0 + ms(TAIL_MS).0);
        let read = |m: &mut pemu_machine::machine::Machine| -> String {
            let bytes: Vec<u8> = MachineApi::io(m)
                .usj_tx
                .slices(cursor)
                .iter()
                .copied()
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        loop {
            let console = read(&mut machine);
            // After the last banner, so an earlier reset's boot cannot stand in.
            if usj_endpoints::after_last(&console, RESET).is_some_and(|tail| tail.contains(READY)) {
                return Some(console);
            }
            if machine.now().0 >= deadline.0 {
                panic!(
                    "the boot after the last reset did not reach `{READY}` in {TAIL_MS} ms of \
                     virtual time:\n{console}"
                );
            }
            let until = pemu_core::time::VTime((machine.now().0 + ms(50).0).min(deadline.0));
            MachineApi::run(
                &mut machine,
                pemu_machine::run::RunLimits {
                    until: Some(until),
                    max_insns: None,
                    stops: pemu_machine::stops::StopSet::default(),
                },
            );
        }
    }

    /// Opens the emulator's RFC 2217 port directly (no `idf.py`, so any host) and walks the capture's
    /// three (RTS, DTR) sequences, each on its own `pk`: the monitor startup (one `boot:0xa` banner,
    /// `Saved PC` outside the ROM), a bootloader reset then a hard reset, and two hard resets a short
    /// gap apart.
    ///
    /// Not asserted: the capture's `Saved PC` in the ROM after a 0.030 s gap and not after 0.150 s.
    /// That is wall-clock, and the virtual gap here is whatever the host gave the live thread.
    pub fn run(test: &str) {
        // 1. esp-idf-monitor startup: `open_serial(reset=True)` leaves (1, 1), walks to (0, 1) and
        //    (0, 0), and `Reset.hard()` enters (1, 0) once. One line moves per step, as pyserial does.
        let Some(console) = console_after(test, |c| {
            c.dtr(true); // (0, 1)
            c.rts(true); // (1, 1) - through DTR first, so (1, 0) is not entered here
            c.rts(false); // (0, 1): download flag set
            c.dtr(false); // (0, 0): download flag cleared
            c.rts(true); // (1, 0): the one reset
            c.rts(false); // (0, 0)
        }) else {
            return;
        };
        let (resets, flash_boot, download) = banners(&console);
        assert_eq!(
            (resets, flash_boot, download),
            (1, 1, 0),
            "one monitor startup reset gives exactly one `boot:0xa` banner and no download \
             banner (device capture, lead 2026-09-21):\n{console}"
        );
        let pcs = saved_pcs(&console);
        assert_eq!(pcs.len(), 1, "one `Saved PC:` line: {pcs:#x?}\n{console}");
        assert!(
            !ROM.contains(&pcs[0]),
            "the reset caught the running app, so `Saved PC:{:#010x}` is not a ROM address",
            pcs[0]
        );
        let after = usj_endpoints::after_last(&console, FLASH_BOOT).expect("the banner");
        assert!(
            after.contains("app_init: Project name:     FoloToy-AI-Passport")
                && after.contains(READY),
            "the `pk` boot follows the banner:\n{after}"
        );
        println!(
            "RAN {test} monitor-startup: 1 banner ({FLASH_BOOT}), Saved PC {:#010x} outside the \
             ROM, then the `pk` boot",
            pcs[0]
        );

        // 2. `usb_jtag_bootloader_reset` then `hard_reset`: the first reset is entered from (1, 1) with
        //    the flag set, so it downloads; the second after (0, 0), so it boots from flash.
        let Some(console) = console_after(test, |c| {
            c.dtr(true); // (0, 1): download flag set
            c.rts(true); // (1, 1)
            c.dtr(false); // (1, 0): reset, download
            c.rts(false); // (0, 0): flag cleared while the ROM waits
            c.rts(true); // (1, 0): reset, flash boot
            c.rts(false); // (0, 0)
        }) else {
            return;
        };
        let (resets, flash_boot, download) = banners(&console);
        assert_eq!(
            (resets, flash_boot, download),
            (2, 1, 1),
            "a bootloader reset then a hard reset gives `boot:0x6` then `boot:0xa`:\n{console}"
        );
        let first = console.find(DOWNLOAD).expect("the download banner");
        let second = console.find(FLASH_BOOT).expect("the flash-boot banner");
        assert!(
            first < second,
            "the download banner comes first:\n{console}"
        );
        assert!(
            console[first..second].contains("waiting for download"),
            "the ROM waited for a download between the two banners:\n{console}"
        );
        println!("RAN {test} bootloader-then-hard: {DOWNLOAD} then {FLASH_BOOT}");

        // 3. Two hard resets a short gap apart: the `boot_log.bin` shape, two `boot:0xa` banners.
        let Some(console) = console_after(test, |c| {
            // The gap is whatever [`SETTLE`] gives the live thread, not the capture's 0.030 s.
            c.rts(true); // (1, 0): reset 1
            c.rts(false); // (0, 0)
            c.rts(true); // (1, 0): reset 2
            c.rts(false); // (0, 0)
        }) else {
            return;
        };
        let (resets, flash_boot, download) = banners(&console);
        assert_eq!(
            (resets, flash_boot, download),
            (2, 2, 0),
            "two hard resets give two `boot:0xa` banners, which is the `boot_log.bin` shape \
             (lines 1 to 7):\n{console}"
        );
        let pcs = saved_pcs(&console);
        assert_eq!(pcs.len(), 2, "one `Saved PC:` per banner: {pcs:#x?}");
        println!(
            "RAN {test} double-hard: 2 banners, both {FLASH_BOOT}; Saved PC {:#010x} and \
             {:#010x} (the capture's ROM-range result for a 0.030 s gap is wall-clock and is not \
             asserted here)",
            pcs[0], pcs[1]
        );
    }
}
