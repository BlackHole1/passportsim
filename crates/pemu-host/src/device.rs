//! The host side of the flash-to-device planner. `pemu-planner` is a core crate and reads no file,
//! so the host services reach it through the traits of `pemu_planner::flow`: backup evidence and
//! the owner-only backup store, the device-path predicate, the confirmation dialog, and (feature
//! `device`) serial discovery. Nothing here opens a serial device: paths are only compared or
//! passed through `refuse_device`, and enumeration lists without opening.

use std::path::{Path, PathBuf};

use pemu_api::commands::plan_flash::{DevicePlanner, DeviceRequest};
#[cfg(not(feature = "device"))]
use pemu_api::error::E_HOST_UNSUPPORTED;
use pemu_api::error::{ApiError, E_USAGE};
#[cfg(feature = "device")]
use pemu_api::error::{E_CARDID_CHANGED, E_DEVICE_BUSY, E_PLAN_REFUSED};
use pemu_loader::{hex, sha256};
use pemu_planner::flow::{BackupStore, DevicePaths, DialogPort};
use pemu_planner::plan::{ImageSource, Origin, PlanOutcome, PlanRequest, plan_flash};
use pemu_planner::rules::BackupEvidence;

use crate::paths::{OwnerOnlyFiles, Reason, contains, refuse_device};
use crate::platform::{Confirmation, Dialog, DialogAvailability};

/// Why an owner-only operation refused, as a class a person can act on. The path is left out:
/// it is a host absolute path or a backup directory named by the salted MAC digest.
fn guard_why(error: &crate::paths::GuardError) -> String {
    use crate::platform::PlatformError;

    match error {
        crate::paths::GuardError::Device(refusal) => refusal.reason.to_string(),
        crate::paths::GuardError::Platform(PlatformError::NotOwnerOnly { detail, .. }) => {
            format!("it is not owner-only ({detail})")
        }
        crate::paths::GuardError::Platform(PlatformError::Io { source, .. }) => {
            source.kind().to_string()
        }
        crate::paths::GuardError::Platform(
            PlatformError::Unsupported(why) | PlatformError::NotImplementedYet(why),
        ) => (*why).to_owned(),
    }
}

/// The facts about a backup file, fail-closed: a check that cannot run counts against it.
/// `inside_repository` compares canonical paths case-insensitively with a `\\?\` prefix stripped.
pub fn backup_evidence(
    files: OwnerOnlyFiles<'_>,
    backup: &Path,
    repository: &Path,
    verified: bool,
) -> BackupEvidence {
    BackupEvidence {
        verified,
        owner_only: files.check(backup).is_ok(),
        inside_repository: contains(repository, backup).unwrap_or(true),
    }
}

/// The per-sector backup store: one owner-only directory per run under `root`, one file per
/// region, each read back and compared. A backup is the recovery source, so it is never replaced:
/// `<device key>-<plan digest>-run<N>` is claimed by an exclusive `mkdir`, so concurrent runs get
/// distinct directories, and each file is created exclusively. The device key is the salted MAC
/// hash prefix, never the MAC; without one it is `unkeyed`.
pub struct OwnerOnlyBackups<'a> {
    files: OwnerOnlyFiles<'a>,
    root: PathBuf,
    repository: PathBuf,
    pub last: Option<PathBuf>,
}

impl<'a> OwnerOnlyBackups<'a> {
    pub fn new(files: OwnerOnlyFiles<'a>, root: &Path, repository: &Path) -> OwnerOnlyBackups<'a> {
        OwnerOnlyBackups {
            files,
            root: root.to_path_buf(),
            repository: repository.to_path_buf(),
            last: None,
        }
    }

    /// Claims the first free run directory of this key and plan; an existing one, whoever made
    /// it, is skipped.
    fn claim_run_dir(&self, stem: &str) -> Result<PathBuf, String> {
        for n in 1..=10_000u32 {
            let dir = self.root.join(format!("{stem}-run{n}"));
            refuse_device(&dir).map_err(|r| format!("backup directory: {}", r.reason))?;
            match create_private_dir(&dir) {
                Ok(()) => {
                    self.files
                        .check(&dir)
                        .map_err(|e| format!("backup directory: {}", guard_why(&e)))?;
                    return Ok(dir);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("backup directory: {}", e.kind())),
            }
        }
        Err("no free backup run directory".to_owned())
    }
}

/// One non-recursive owner-only `mkdir` ([`OwnerOnlyFiles::create_new_dir`]), so claiming a run
/// directory and protecting it are one call; `AlreadyExists` when the name is taken.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    OwnerOnlyFiles::host().create_new_dir(dir).map_err(guard_io)
}

/// Creates `path` exclusively as an owner-only file and writes `bytes`; never truncates.
fn write_new_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    OwnerOnlyFiles::host()
        .write_new(path, bytes)
        .map_err(guard_io)
}

/// An owner-only failure as an `io::Error` carrying only its kind, so no path leaks.
fn guard_io(error: crate::paths::GuardError) -> std::io::Error {
    use crate::paths::GuardError;
    use crate::platform::PlatformError;

    match error {
        GuardError::Device(refusal) => {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, refusal.reason.to_string())
        }
        GuardError::Platform(PlatformError::Io { source, .. }) => source,
        GuardError::Platform(PlatformError::NotOwnerOnly { detail, .. }) => {
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, detail)
        }
        GuardError::Platform(
            PlatformError::Unsupported(why) | PlatformError::NotImplementedYet(why),
        ) => std::io::Error::new(std::io::ErrorKind::Unsupported, why),
    }
}

impl BackupStore for OwnerOnlyBackups<'_> {
    fn store(
        &mut self,
        plan_sha256: &[u8; 32],
        device_key: Option<&[u8; 16]>,
        regions: &[(u32, Vec<u8>)],
    ) -> Result<BackupEvidence, String> {
        let key = device_key.map_or_else(|| "unkeyed".to_owned(), |k| hex(k));
        let dir = self.claim_run_dir(&format!("{key}-{}", hex(&plan_sha256[..8])))?;
        let mut verified = true;
        let mut owner_only = true;
        for (offset, bytes) in regions {
            let path = dir.join(format!("{offset:08x}.bin"));
            refuse_device(&path).map_err(|r| format!("backup file: {}", r.reason))?;
            write_new_private_file(&path, bytes).map_err(|e| {
                format!(
                    "backup file {offset:08x}.bin: {}; a backup is never replaced",
                    e.kind()
                )
            })?;
            match self.files.read(&path) {
                Ok(back) => verified &= sha256(&back) == sha256(bytes),
                Err(_) => owner_only = false,
            }
        }
        let evidence = BackupEvidence {
            verified: verified && !regions.is_empty(),
            owner_only: owner_only && self.files.check(&dir).is_ok(),
            inside_repository: contains(&self.repository, &dir).unwrap_or(true),
        };
        self.last = Some(dir);
        Ok(evidence)
    }
}

/// The flashing-port predicate: the per-OS `spelling` rule must accept the port and
/// `refuse_device` must refuse its open form. The open form matters on Windows: `COM12` is not a
/// reserved name, but `\\.\COM12`, which the planner opens, is a device-namespace path.
pub struct HostDevicePaths<'a> {
    spelling: &'a dyn DevicePaths,
}

impl<'a> HostDevicePaths<'a> {
    pub fn new(spelling: &'a dyn DevicePaths) -> HostDevicePaths<'a> {
        HostDevicePaths { spelling }
    }
}

impl DevicePaths for HostDevicePaths<'_> {
    fn is_flash_port(&self, path: &str) -> bool {
        if !self.spelling.is_flash_port(path) {
            return false;
        }
        matches!(
            refuse_device(Path::new(&self.spelling.open_path(path))),
            Err(refusal) if matches!(
                refusal.reason,
                Reason::SerialName | Reason::DeviceNamespace | Reason::ReservedName
            )
        )
    }

    fn case_insensitive(&self) -> bool {
        self.spelling.case_insensitive()
    }

    fn open_path(&self, port: &str) -> String {
        self.spelling.open_path(port)
    }
}

/// The dialog confirmation transport over `platform::Dialog`. An unavailable desktop or a dialog
/// error is reported as such, never turned into a yes.
pub struct HostDialog<'a> {
    dialog: &'a dyn Dialog,
}

impl<'a> HostDialog<'a> {
    pub fn new(dialog: &'a dyn Dialog) -> HostDialog<'a> {
        HostDialog { dialog }
    }
}

impl DialogPort for HostDialog<'_> {
    fn availability(&self) -> Result<(), String> {
        match self.dialog.availability() {
            DialogAvailability::Available => Ok(()),
            DialogAvailability::Unavailable(why) => Err(why.to_owned()),
        }
    }

    fn ask(&self, title: &str, body: &str) -> Result<bool, String> {
        match self.dialog.ask(title, body) {
            Ok(Confirmation::Confirmed) => Ok(true),
            Ok(Confirmation::Declined) => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// OS entropy for the one-time terminal confirmation code. If the OS source fails this is `None`
/// and the terminal path is unavailable rather than predictable.
pub fn entropy_bytes() -> Option<impl FnMut() -> u8> {
    let mut probe = [0u8; 1];
    getrandom::fill(&mut probe).ok()?;
    let mut buffer = [0u8; 32];
    let mut used = buffer.len();
    Some(move || {
        if used == buffer.len() {
            // Unexpected after a successful probe; 0xFF is skipped by the code's rejection
            // sampling, so it never yields a predictable letter.
            if getrandom::fill(&mut buffer).is_err() {
                buffer = [0xFF; 32];
            }
            used = 0;
        }
        used += 1;
        buffer[used - 1]
    })
}

// The optional full 8 MB backup (`--backup`).

/// What a full backup produced: no file name and no digest, since the backup holds a real
/// Passport's cardid and NVS credentials and its directory name derives from the MAC.
pub type FullBackup = pemu_planner::flow::FullBackupFacts;

/// Reads the whole 8 MB part twice, refuses unless both reads have the same SHA-256, and stores it
/// owner-only in a fresh run directory under `root`, outside `repository`. Twice because this is
/// the only recovery source and a serial read can silently drop or duplicate a packet. The result
/// carries booleans and a length only.
pub fn full_backup(
    files: OwnerOnlyFiles<'_>,
    root: &Path,
    repository: &Path,
    device_key: Option<&[u8; 16]>,
    plan_sha256: &[u8; 32],
    session: &mut dyn pemu_planner::flow::DeviceSession,
) -> Result<FullBackup, String> {
    let first = session
        .read_full_flash()
        .map_err(|e| format!("the full backup could not be read: {e:?}"))?;
    let second = session
        .read_full_flash()
        .map_err(|e| format!("the full backup could not be read a second time: {e:?}"))?;
    if sha256(&first) != sha256(&second) {
        return Err(
            "the two full-part reads differ, so the backup cannot be trusted; nothing was \
             written and nothing was flashed"
                .to_owned(),
        );
    }
    drop(second);
    let stem = backup_stem(device_key, plan_sha256);
    let dir = claim_run_dir(&files, root, &stem)?;
    let path = dir.join("full-8mb.bin");
    files
        .write(&path, &first)
        .map_err(|e| format!("the full backup could not be written: {}", guard_why(&e)))?;
    // Read back from the file too, so "verified" means the bytes on disk are the bytes read.
    let stored = std::fs::read(&path).map_err(|e| format!("the full backup: {}", e.kind()))?;
    let verified = sha256(&stored) == sha256(&first);
    Ok(FullBackup {
        evidence: backup_evidence(files, &path, repository, verified),
        bytes: first.len() as u64,
    })
}

pub struct OwnerOnlyFullBackup<'a> {
    files: OwnerOnlyFiles<'a>,
    root: PathBuf,
    repository: PathBuf,
}

impl<'a> OwnerOnlyFullBackup<'a> {
    pub fn new(
        files: OwnerOnlyFiles<'a>,
        root: &Path,
        repository: &Path,
    ) -> OwnerOnlyFullBackup<'a> {
        OwnerOnlyFullBackup {
            files,
            root: root.to_path_buf(),
            repository: repository.to_path_buf(),
        }
    }
}

impl pemu_planner::flow::FullBackup for OwnerOnlyFullBackup<'_> {
    fn store_full(
        &mut self,
        session: &mut dyn pemu_planner::flow::DeviceSession,
        plan_sha256: &[u8; 32],
        device_key: Option<&[u8; 16]>,
    ) -> Result<pemu_planner::flow::FullBackupFacts, String> {
        full_backup(
            self.files,
            &self.root,
            &self.repository,
            device_key,
            plan_sha256,
            session,
        )
    }
}

/// The directory name of a backup: the salted MAC-hash prefix when known, the plan digest prefix
/// always. Never printed.
fn backup_stem(device_key: Option<&[u8; 16]>, plan_sha256: &[u8; 32]) -> String {
    let key = device_key.map_or_else(|| "unkeyed".to_owned(), |k| hex(&k[..8]));
    format!("{key}-{}", hex(&plan_sha256[..8]))
}

fn claim_run_dir(files: &OwnerOnlyFiles<'_>, root: &Path, stem: &str) -> Result<PathBuf, String> {
    if !root.is_dir() {
        files
            .create_dir(root)
            .map_err(|e| format!("backup directory: {}", guard_why(&e)))?;
    }
    for n in 1..=10_000u32 {
        let dir = root.join(format!("{stem}-run{n}"));
        refuse_device(&dir).map_err(|r| format!("backup directory: {}", r.reason))?;
        match create_private_dir(&dir) {
            Ok(()) => {
                files
                    .check(&dir)
                    .map_err(|e| format!("backup directory: {}", guard_why(&e)))?;
                return Ok(dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("backup directory: {}", e.kind())),
        }
    }
    Err("too many backup runs of this plan; move the older ones away".to_owned())
}

// The per-install MAC salt and the `pemu-api` planner hook.

/// The backup directory, checked against the repository before anything is enumerated or opened:
/// a repository containing the data root would otherwise refuse every plan only after the device
/// was opened, identified and reset.
#[cfg(any(feature = "device", test))]
fn backup_root_for(data_root: &Path, repository: &Path) -> Result<PathBuf, String> {
    let root = data_root.join("device").join("backups");
    if !root.is_dir() {
        std::fs::create_dir_all(&root).map_err(|e| format!("backup directory: {}", e.kind()))?;
    }
    if contains(repository, &root).unwrap_or(true) {
        return Err(
            "the backup directory is inside the repository this planner protects; run from a directory the data root is not inside, or move the \
                    data root"
                .to_owned(),
        );
    }
    Ok(root)
}

/// The per-install salt that keys backup directories: 32 bytes of OS entropy, written once
/// owner-only under `<data root>/device/`, never printed or logged. A bare SHA-256 of a MAC is
/// enumerable, so the key is SHA-256(salt, MAC), first 16 bytes. The file is created exclusively,
/// so runs share the first salt; one that is not owner-only is refused rather than repaired.
pub fn mac_salt(data_root: &Path) -> Result<[u8; 32], String> {
    let files = OwnerOnlyFiles::host();
    let dir = data_root.join("device");
    if !dir.exists() {
        files
            .create_dir(&dir)
            .map_err(|e| format!("device directory: {}", guard_why(&e)))?;
    }
    let path = dir.join("mac-salt.bin");
    if !path.exists() {
        let mut fresh = [0u8; 32];
        getrandom::fill(&mut fresh).map_err(|_| "the OS entropy source failed".to_owned())?;
        // `write_new` refuses when anything is there, so a second or racing run falls through to
        // the read below and keeps the first salt.
        match files.write_new(&path, &fresh) {
            Ok(()) => return Ok(fresh),
            Err(_) if path.exists() => {}
            Err(e) => {
                return Err(format!(
                    "the MAC salt could not be written: {}",
                    guard_why(&e)
                ));
            }
        }
    }
    files
        .check(&path)
        .map_err(|_| "the MAC salt file is not owner-only; remove it and run again".to_owned())?;
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("the MAC salt could not be read: {}", e.kind()))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| "the MAC salt file is not 32 bytes; remove it and run again".to_owned())
}

/// The esptool sources this host can see: `--esptool`, `$IDF_PYTHON_ENV_PATH`, then the Python
/// environments of the IDF tools directory (`~/.espressif`, `%USERPROFILE%\.espressif` on
/// Windows), newest name first.
pub fn esptool_sources(explicit: Option<&str>) -> pemu_planner::rehearse::EsptoolSources {
    let tools = std::env::var_os("IDF_TOOLS_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| Path::new(&home).join(".espressif"))
        });
    let mut envs: Vec<String> = Vec::new();
    if let Some(dir) = tools.map(|t| t.join("python_env"))
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        let mut found: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        found.sort();
        envs.extend(
            found
                .into_iter()
                .rev()
                .map(|p| p.to_string_lossy().into_owned()),
        );
    }
    pemu_planner::rehearse::EsptoolSources {
        explicit: explicit.map(str::to_owned),
        idf_python_env: std::env::var_os("IDF_PYTHON_ENV_PATH")
            .map(|v| v.to_string_lossy().into_owned()),
        idf_tools_envs: envs,
    }
}

/// The host half of the Device group: reads the image, runs the pure planner and, with feature
/// `device`, the whole flash flow. Only a merged image is accepted.
pub struct HostPlanner {
    data_root: PathBuf,
    repository: PathBuf,
    esptool: Option<String>,
}

impl HostPlanner {
    pub fn new(data_root: &Path, repository: &Path, esptool: Option<&str>) -> HostPlanner {
        HostPlanner {
            data_root: data_root.to_path_buf(),
            repository: repository.to_path_buf(),
            esptool: esptool.map(str::to_owned),
        }
    }

    fn image_of(request: &DeviceRequest) -> Result<Vec<u8>, ApiError> {
        let path = Path::new(&request.image);
        refuse_device(path)
            .map_err(|r| ApiError::new(E_USAGE, format!("`image`: {}", r.reason)))?;
        if path.is_dir() {
            return Err(ApiError::new(
                E_USAGE,
                "`image` is a directory; the device flow takes a merged `.bin` in v1",
            ));
        }
        std::fs::read(path)
            .map_err(|e| ApiError::new(E_USAGE, format!("`image` could not be read: {}", e.kind())))
    }

    /// The pure plan as JSON. `identified` means the flow checked the identity, device-table and
    /// `nvs` rules against the device and they passed; only then is the plan labelled `done`.
    fn plan_json(request: &DeviceRequest, image: &[u8], identified: bool) -> serde_json::Value {
        let mut plan_request = PlanRequest::write(ImageSource::Merged(image), caller_origin());
        plan_request.erase_nvs = request.erase_nvs;
        let outcome = plan_flash(&plan_request, None);
        plan_outcome_json(&outcome, if identified { "done" } else { "pending" })
    }
}

/// The receipt of a real flash: the steps, the checks, both backups' evidence and the plan with
/// its segment hashes. Built from the flow report alone, so a test needs no esptool or device.
#[cfg(feature = "device")]
fn flash_receipt_json(
    report: &pemu_planner::flow::FlowReport,
    plan: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "plan_sha256": report.plan_sha256.clone().unwrap_or_default(),
        "dry_run": false,
        "steps": step_log_json(&report.device),
        "rehearsal": step_log_json(&report.rehearsal),
        "cardid_unchanged": report.device.cardid_unchanged,
        "boot_check": report.device.boot.map(|b| serde_json::json!({
            "banner": b.banner,
            "elf_sha256_matches": b.elf_sha256_matches,
            "passed": b.passed(),
        })),
        // Booleans only, no file name or stem.
        "backup": report.device.backup.map(|e| serde_json::json!({
            "verified": e.verified,
            "owner_only": e.owner_only,
            "inside_repository": e.inside_repository,
        })),
        // Whether the whole 8 MB part was stored: booleans and a length only.
        "full_backup": full_backup_json(report.device.full_backup),
        "plan": plan,
    })
}

/// Who asked: the CLI installs the planner as `DeviceCaller::Cli`, a server as an agent, and only
/// the CLI may ask for `--erase-nvs`. `pemu-api` reads the same slot for CLI-only arguments.
fn caller_origin() -> Origin {
    if pemu_api::commands::plan_flash::from_cli() {
        Origin::HumanCli
    } else {
        Origin::Tool
    }
}

/// One `PlanOutcome` as JSON. It carries no image byte, port path or device value.
fn plan_outcome_json(outcome: &PlanOutcome, device_checks: &str) -> serde_json::Value {
    serde_json::json!({
        "plan_sha256": outcome.plan.plan_sha256_hex(),
        "writes": outcome
            .plan
            .writes
            .iter()
            .map(|w| serde_json::json!({
                "name": w.name,
                "offset": w.offset,
                "bytes": w.data.len(),
                "sha256": hex(&w.sha256),
            }))
            .collect::<Vec<_>>(),
        "refusals": outcome
            .refusals
            .iter()
            .map(|r| serde_json::json!({ "rule": format!("{:?}", r.rule), "detail": r.detail }))
            .collect::<Vec<_>>(),
        "accepted": outcome.refusals.is_empty(),
        "device_checks": device_checks,
    })
}

impl DevicePlanner for HostPlanner {
    fn plan(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
        let image = Self::image_of(request)?;
        Ok(Self::plan_json(request, &image, false))
    }

    fn flash(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
        let image = Self::image_of(request)?;
        if request.dry_run {
            // A dry run opens and enumerates nothing. The backup evidence covers the directory the
            // flow would use, so a bad data root is caught before anyone confirms; `verified` stays
            // false.
            let plan = Self::plan_json(request, &image, false);
            let backups = self.data_root.join("device").join("backups");
            let evidence =
                backup_evidence(OwnerOnlyFiles::host(), &backups, &self.repository, false);
            let accepted = plan
                .get("accepted")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            return Ok(serde_json::json!({
                "plan_sha256": plan["plan_sha256"].clone(),
                "dry_run": true,
                "steps": [{ "step": "Plan", "ok": accepted }],
                "rehearsal": [],
                "cardid_unchanged": serde_json::Value::Null,
                "boot_check": serde_json::Value::Null,
                "backup": {
                    "verified": evidence.verified,
                    "owner_only": evidence.owner_only,
                    "inside_repository": evidence.inside_repository,
                },
                "full_backup": full_backup_json(None),
                "plan": plan,
            }));
        }
        #[cfg(feature = "device")]
        {
            self.run_flash(request, &image)
        }
        #[cfg(not(feature = "device"))]
        {
            let _ = (&self.esptool, image);
            Err(unsupported_here())
        }
    }

    fn boot_check(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
        #[cfg(feature = "device")]
        {
            self.run_boot_check(request)
        }
        #[cfg(not(feature = "device"))]
        {
            let _ = request;
            Err(unsupported_here())
        }
    }
}

// The real-device flow, feature `device`.

/// The emulator instance standing in for the device during the rehearsal: it computes the region
/// digest over its own flash, as the stub does on the device.
pub struct InstanceMd5<'a>(
    pub &'a crate::endpoints::live::LiveRunner<pemu_machine::machine::Machine>,
);

impl pemu_planner::rehearse::RegionMd5 for InstanceMd5<'_> {
    fn region_md5(
        &mut self,
        offset: u32,
        size: u32,
    ) -> Result<[u8; 16], pemu_planner::flow::SessionError> {
        let size = size as usize;
        self.0
            .call(move |machine: &mut pemu_machine::machine::Machine| {
                let mut region = vec![0u8; size];
                machine.flash_read(offset, &mut region);
                pemu_planner::md5::digest(&region)
            })
            .ok_or_else(|| {
                pemu_planner::flow::SessionError::Failed("the instance stopped".to_owned())
            })
    }
}

/// The rehearsal instance's boot console. After esptool's hard reset no host tool is connected,
/// so the endpoint leaves the instance idle; the job boots it and returns the console since
/// `cursor`.
pub struct InstanceBootConsole<'a> {
    pub runner: &'a crate::endpoints::live::LiveRunner<pemu_machine::machine::Machine>,
    /// The `usj_tx` head at the start of the run.
    pub cursor: u64,
    /// Virtual milliseconds the instance is given to boot.
    pub boot_ms: u64,
}

impl pemu_planner::rehearse::BootConsole for InstanceBootConsole<'_> {
    fn boot_log(&mut self) -> Result<String, pemu_planner::flow::SessionError> {
        use pemu_machine::machine::{Machine, MachineApi};
        let (cursor, boot_ms) = (self.cursor, self.boot_ms);
        let bytes = self
            .runner
            .call(move |machine: &mut Machine| {
                let until = pemu_core::time::VTime(
                    machine.now().0 + pemu_core::time::VTime::from_ms(boot_ms).0,
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
            .ok_or_else(|| {
                pemu_planner::flow::SessionError::Failed("the instance stopped".to_owned())
            })?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(match text.rfind("rst:0x") {
            Some(i) => text[i..].to_owned(),
            None => text,
        })
    }
}

#[cfg(feature = "device")]
struct HostFileProbe;

#[cfg(feature = "device")]
impl pemu_planner::rehearse::FileProbe for HostFileProbe {
    fn is_file(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }
}

/// The whole-part backup as JSON, or `null` without `--backup`. `taken` is needed because the one
/// `Step::Backup` entry covers the sector backup and the whole part together.
fn full_backup_json(facts: Option<pemu_planner::flow::FullBackupFacts>) -> serde_json::Value {
    match facts {
        Some(full) => serde_json::json!({
            "taken": true,
            "verified": full.evidence.verified,
            "owner_only": full.evidence.owner_only,
            "inside_repository": full.evidence.inside_repository,
            "bytes": full.bytes,
        }),
        None => serde_json::Value::Null,
    }
}

#[cfg(feature = "device")]
fn step_log_json(log: &pemu_planner::flow::StepLog) -> serde_json::Value {
    serde_json::Value::Array(
        log.steps
            .iter()
            .map(|(step, ok)| serde_json::json!({ "step": format!("{step:?}"), "ok": ok }))
            .collect(),
    )
}

#[cfg(feature = "device")]
impl HostPlanner {
    /// An owner-only scratch directory of this run, under `<data root>/device/scratch/<name>`.
    fn scratch(&self, name: &str) -> Result<PathBuf, ApiError> {
        let dir = self.data_root.join("device").join("scratch").join(name);
        let files = OwnerOnlyFiles::host();
        if !dir.exists() {
            std::fs::create_dir_all(dir.parent().unwrap_or(&dir))
                .map_err(|e| ApiError::new(E_PLAN_REFUSED, format!("scratch: {}", e.kind())))?;
            files.create_dir(&dir).map_err(|e| {
                ApiError::new(E_PLAN_REFUSED, format!("scratch: {}", guard_why(&e)))
            })?;
        }
        // The directory is this run's scratch, so the refusal can safely say to remove it.
        files.check(&dir).map_err(|e| {
            ApiError::new(
                E_PLAN_REFUSED,
                format!(
                    "the scratch directory: {}; remove it and run again",
                    guard_why(&e)
                ),
            )
        })?;
        Ok(dir)
    }

    /// The whole flash flow on the connected Passport. `pemu_planner::flow::flash_device` decides
    /// every order and refusal; this assembles the host effects it needs.
    fn run_flash(
        &mut self,
        request: &DeviceRequest,
        image: &[u8],
    ) -> Result<serde_json::Value, ApiError> {
        use pemu_planner::exec::{Deadlines, DeviceOpener, HOST_OS, HostPorts, StdRunner};
        use pemu_planner::flow::{Effects, flash_device};
        use pemu_planner::rehearse::{EsptoolSession, check_esptool_version, resolve_esptool};

        let refuse = |why: String| ApiError::new(E_PLAN_REFUSED, why);
        let sources = esptool_sources(self.esptool.as_deref());
        let command = resolve_esptool(&sources, HOST_OS, &HostFileProbe)
            .map_err(|e| refuse(format!("esptool: {e:?}")))?;
        let mut device_runner = StdRunner::new(
            &self.scratch("device-session")?,
            &command,
            Deadlines::default(),
        )
        .map_err(refuse)?;
        let command = check_esptool_version(command, &mut device_runner)
            .map_err(|e| refuse(format!("esptool: {e:?}")))?;
        let mut rehearsal_runner =
            StdRunner::new(&self.scratch("rehearsal")?, &command, Deadlines::default())
                .map_err(refuse)?;

        // The rehearsal image carries a synthetic cardid pattern: an erased window would make an
        // erase of it invisible to the guard, so seeding lets the rehearsal catch that hazard.
        let rehearsal_image = pemu_planner::rules::with_synthetic_cardid(image);
        let flash = pemu_loader::bundle::FlashImage::from_merged(&rehearsal_image)
            .map_err(|e| ApiError::new(E_USAGE, format!("`image` is not a merged image: {e:?}")))?;
        let machine = crate::backend::build_machine(flash, 1)
            .map_err(|e| refuse(format!("the rehearsal instance: {e:?}")))?;
        let mut machine = machine;
        let cursor = pemu_machine::machine::MachineApi::io(&mut machine)
            .usj_tx
            .head();
        let live = crate::endpoints::live::LiveRunner::start(
            machine,
            crate::endpoints::live::Pacing::Wall(1.0),
        )
        .map_err(|(_, e)| refuse(format!("the rehearsal instance: {}", e.kind())))?;
        let endpoint = crate::endpoints::tcp::TcpEndpoint::bind(
            live.link(),
            crate::endpoints::tcp::TcpOptions::default(),
        )
        .map_err(|e| refuse(format!("the rehearsal endpoint: {}", e.kind())))?;
        let url = endpoint.rfc2217_url();

        let salt = mac_salt(&self.data_root).map_err(refuse)?;
        let spelling = HostPorts::default();
        let paths = HostDevicePaths::new(&spelling);
        let enumeration = crate::platform::serial::enumerator();
        let mut discovery = SerialDiscovery::new(enumeration);
        let backup_root = backup_root_for(&self.data_root, &self.repository).map_err(refuse)?;
        let mut backups =
            OwnerOnlyBackups::new(OwnerOnlyFiles::host(), &backup_root, &self.repository);
        // The whole-part backup only when a person typed `--backup`.
        let mut whole_part =
            OwnerOnlyFullBackup::new(OwnerOnlyFiles::host(), &backup_root, &self.repository);
        let full_store: Option<&mut dyn pemu_planner::flow::FullBackup> =
            request.backup.then_some(&mut whole_part);

        let mut plan_request = PlanRequest::write(ImageSource::Merged(image), caller_origin());
        plan_request.erase_nvs = request.erase_nvs;

        let (report, result) = {
            let mut md5 = InstanceMd5(&live);
            let mut console = InstanceBootConsole {
                runner: &live,
                cursor,
                boot_ms: 3_000,
            };
            let mut rehearsal = EsptoolSession::emulator(
                command.clone(),
                &url,
                &mut rehearsal_runner,
                &mut md5,
                &mut console,
            )
            .map_err(|r| refuse(r.to_string()))?;

            // The port-opening reader lives in the planner behind feature `device` and takes the
            // confirmation token, not a path.
            let mut boot_console = pemu_planner::exec::PortConsole::new(&paths);
            let mut opener = DeviceOpener::new(command, &mut device_runner)
                .with_console(&mut boot_console)
                .with_mac_salt(salt);

            // The owner's standing grant answers the confirmation, so a device run needs no person
            // at the machine; every machine-checked rule still runs.
            let mut standing_grant = pemu_planner::flow::StandingGrantConfirmer::new();
            let confirmer: &mut dyn pemu_planner::flow::Confirmer = &mut standing_grant;

            flash_device(
                &plan_request,
                request.port.as_deref(),
                Effects {
                    discovery: &mut discovery,
                    paths: &paths,
                    rehearsal: &mut rehearsal,
                    confirmer,
                    opener: &mut opener,
                    backups: &mut backups,
                    full_backup: full_store,
                },
            )
        };
        endpoint.close();
        let _ = live.stop();

        let identified = report
            .device
            .steps
            .contains(&(pemu_planner::flow::Step::Identify, true));
        let json = flash_receipt_json(&report, Self::plan_json(request, image, identified));
        match result {
            Ok(()) => Ok(json),
            Err(error) => {
                let code = flow_code(&error);
                Err(ApiError::new(code, flow_message(&error)).with_detail(json))
            }
        }
    }

    /// The boot check alone: enumerate, confirm, open the port read-only and judge what it
    /// printed. Nothing is reset unless `request.reset` asks, and an idle booted Passport prints
    /// nothing. It writes nothing but is still confirmed, with a digest naming port and intent.
    /// The result carries counts, never console text.
    fn run_boot_check(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
        use pemu_planner::console::lines_omitted;
        use pemu_planner::exec::{HostPorts, PortConsole};
        use pemu_planner::flow::{Discovery as _, boot_check, confirm_console_read, select_port};
        use pemu_planner::rehearse::ConsoleOpener as _;

        let image = Self::image_of(request)?;
        let mut plan_request = PlanRequest::write(ImageSource::Merged(&image), caller_origin());
        plan_request.erase_nvs = false;
        let outcome = plan_flash(&plan_request, None);
        let spelling = HostPorts::default();
        let paths = HostDevicePaths::new(&spelling);
        let enumeration = crate::platform::serial::enumerator();
        let candidates = SerialDiscovery::new(enumeration)
            .enumerate()
            .map_err(|why| ApiError::new(E_PLAN_REFUSED, why))?;
        let port = select_port(&candidates, request.port.as_deref(), &paths)
            .map_err(|r| ApiError::new(E_PLAN_REFUSED, r.to_string()))?;
        // The standing grant answers here too, as for the flash above.
        let mut standing_grant = pemu_planner::flow::StandingGrantConfirmer::new();
        let confirmer: &mut dyn pemu_planner::flow::Confirmer = &mut standing_grant;
        let confirmed =
            confirm_console_read(&port, request.reset, confirmer).map_err(flow_error)?;
        let mut console = PortConsole::new(&paths);
        let log = console.boot_log_of(&confirmed).map_err(|e| match e {
            pemu_planner::flow::SessionError::Busy => ApiError::new(
                E_DEVICE_BUSY,
                "the port is held by another process; quit Passport Keys",
            ),
            pemu_planner::flow::SessionError::Unsupported(why)
            | pemu_planner::flow::SessionError::Failed(why) => ApiError::new(E_PLAN_REFUSED, why),
        })?;
        let check = boot_check(&log, outcome.plan.app_elf_sha256.as_ref());
        let omitted = lines_omitted(&log);
        let stats = console.stats();
        Ok(serde_json::json!({
            "banner": check.banner,
            "elf_sha256_matches": check.elf_sha256_matches,
            "passed": check.passed(),
            // Counts and a verdict, never the console: silence must be told from a mismatch.
            "console_empty": stats.empty,
            "console_bytes": stats.bytes,
            "reset": request.reset,
            "lines_omitted": omitted,
        }))
    }
}

#[cfg(feature = "device")]
fn flow_code(error: &pemu_planner::flow::FlowError) -> pemu_api::error::ErrorCode {
    match error.code() {
        "E_DEVICE_BUSY" => E_DEVICE_BUSY,
        "E_CARDID_CHANGED" => E_CARDID_CHANGED,
        _ => E_PLAN_REFUSED,
    }
}

/// What a flow error says to a person, with no path and no device value in it.
#[cfg(feature = "device")]
fn flow_message(error: &pemu_planner::flow::FlowError) -> String {
    use pemu_planner::flow::FlowError;

    match error {
        FlowError::Refused(rules) => rules
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
        FlowError::Declined => "the person declined".to_owned(),
        FlowError::ConfirmationUnavailable(hint) => hint.clone(),
        FlowError::DeviceBusy => {
            "the port is held by another process; quit Passport Keys".to_owned()
        }
        FlowError::CardidChanged(instructions) => (*instructions).to_owned(),
        FlowError::CheckFailed(why) | FlowError::Session(why) => why.clone(),
    }
}

#[cfg(feature = "device")]
fn flow_error(error: pemu_planner::flow::FlowError) -> ApiError {
    ApiError::new(flow_code(&error), flow_message(&error))
}

/// The refusal of a build without feature `device`: `E_HOST_UNSUPPORTED`, naming the build that
/// can flash.
#[cfg(not(feature = "device"))]
fn unsupported_here() -> ApiError {
    ApiError::new(
        E_HOST_UNSUPPORTED,
        "this build has no device support: build the CLI with \
         `cargo build -p pemu-cli --features pemu-host/device` \
         (`plan_flash` and `flash_device --dry-run` work without it)",
    )
}

/// Serial discovery over the host's enumeration: 303A:1001 devices, listed without opening.
#[cfg(feature = "device")]
pub struct SerialDiscovery<'a> {
    enumeration: &'a dyn crate::platform::serial::SerialEnumeration,
}

#[cfg(feature = "device")]
impl<'a> SerialDiscovery<'a> {
    pub fn new(
        enumeration: &'a dyn crate::platform::serial::SerialEnumeration,
    ) -> SerialDiscovery<'a> {
        SerialDiscovery { enumeration }
    }
}

#[cfg(feature = "device")]
impl pemu_planner::flow::Discovery for SerialDiscovery<'_> {
    fn enumerate(&mut self) -> Result<Vec<pemu_planner::flow::PortCandidate>, String> {
        use crate::platform::serial::{PID, VID};
        let devices = self
            .enumeration
            .enumerate(VID, PID)
            .map_err(|e| e.message.clone())?;
        Ok(devices
            .into_iter()
            .map(|d| pemu_planner::flow::PortCandidate {
                path: d.path.to_string_lossy().into_owned(),
                vid: d.vid,
                pid: d.pid,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_planner::flow::SessionError;

    use crate::platform::fake::{FakeDialog, FakeOwnerOnly};
    use crate::platform::scratch_dir;

    /// A session whose whole-part read is a fixed 8 MB pattern with cardid-like bytes in the
    /// window, so a test can prove none of them reaches an output.
    struct FakeWholePart {
        reads: usize,
        drift: bool,
    }

    impl FakeWholePart {
        fn part(&self) -> Vec<u8> {
            let mut flash = vec![0xFFu8; 0x80_0000];
            for (i, b) in flash[0x35_6000..0x35_A000].iter_mut().enumerate() {
                *b = (i as u8).wrapping_mul(37) ^ 0x5C;
            }
            flash
        }
    }

    impl pemu_planner::flow::DeviceSession for FakeWholePart {
        fn identify(&mut self) -> Result<pemu_planner::flow::Identity, SessionError> {
            Err(SessionError::Unsupported("not used".to_owned()))
        }
        fn read_partition_table(&mut self) -> Result<Vec<u8>, SessionError> {
            Err(SessionError::Unsupported("not used".to_owned()))
        }
        fn region_md5(&mut self, _: u32, _: u32) -> Result<[u8; 16], SessionError> {
            Err(SessionError::Unsupported("not used".to_owned()))
        }
        fn read_region(&mut self, _: u32, _: u32) -> Result<Vec<u8>, SessionError> {
            Err(SessionError::Unsupported("not used".to_owned()))
        }
        fn read_full_flash(&mut self) -> Result<Vec<u8>, SessionError> {
            self.reads += 1;
            let mut part = self.part();
            if self.drift && self.reads > 1 {
                part[0x40] ^= 0x01;
            }
            Ok(part)
        }
        fn write(&mut self, _: &[pemu_planner::plan::PlannedWrite]) -> Result<(), SessionError> {
            Err(SessionError::Unsupported("not used".to_owned()))
        }
        fn erase_region(&mut self, _: u32, _: u32) -> Result<(), SessionError> {
            Err(SessionError::Unsupported("not used".to_owned()))
        }
        fn verify(&mut self, _: &[pemu_planner::plan::PlannedWrite]) -> Result<(), SessionError> {
            Err(SessionError::Unsupported("not used".to_owned()))
        }
        fn hard_reset(&mut self) -> Result<(), SessionError> {
            Err(SessionError::Unsupported("not used".to_owned()))
        }
        fn boot_log(&mut self) -> Result<String, SessionError> {
            Err(SessionError::Unsupported("not used".to_owned()))
        }
    }

    #[test]
    fn a_full_backup_lands_owner_only_outside_the_repository() {
        let root = scratch_dir("device-full-backups");
        let repo = scratch_dir("device-full-repo");
        let files = OwnerOnlyFiles::host();
        files.create_dir(&repo).expect("repo");
        let mut session = FakeWholePart {
            reads: 0,
            drift: false,
        };
        let full = full_backup(files, &root, &repo, None, &[7u8; 32], &mut session)
            .expect("a clean 8 MB backup");
        assert_eq!(session.reads, 2, "the part is read twice");
        assert_eq!(full.bytes, 0x80_0000);
        assert_eq!(
            full.evidence,
            BackupEvidence {
                verified: true,
                owner_only: true,
                inside_repository: false,
            }
        );
        let rendered = format!("{full:?}");
        assert!(!rendered.contains(&root.to_string_lossy().into_owned()));
        assert!(!rendered.contains("0x356000") && !rendered.contains("cardid"));

        let mut again = FakeWholePart {
            reads: 0,
            drift: false,
        };
        full_backup(files, &root, &repo, None, &[7u8; 32], &mut again).expect("a second run");
        let runs = std::fs::read_dir(&root).expect("read").count();
        assert_eq!(runs, 2, "each run claims its own directory");
        for entry in std::fs::read_dir(&root).expect("read").flatten() {
            files
                .check(&entry.path())
                .expect("owner-only run directory");
            let file = entry.path().join("full-8mb.bin");
            assert_eq!(
                std::fs::metadata(&file).expect("the backup file").len(),
                0x80_0000
            );
            files.check(&file).expect("owner-only backup");
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn two_full_reads_that_differ_are_refused() {
        let root = scratch_dir("device-full-drift");
        let repo = scratch_dir("device-full-drift-repo");
        let files = OwnerOnlyFiles::host();
        files.create_dir(&repo).expect("repo");
        let mut session = FakeWholePart {
            reads: 0,
            drift: true,
        };
        let error = full_backup(files, &root, &repo, None, &[7u8; 32], &mut session)
            .expect_err("the two reads differ");
        assert!(error.contains("cannot be trusted"), "{error}");
        assert!(
            std::fs::read_dir(&root).map(Iterator::count).unwrap_or(0) == 0,
            "nothing was written"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_full_backup_inside_the_repository_is_reported() {
        let repo = scratch_dir("device-full-inside");
        let files = OwnerOnlyFiles::host();
        files.create_dir(&repo).expect("repo");
        let mut session = FakeWholePart {
            reads: 0,
            drift: false,
        };
        let inside = repo.join("backups");
        let full = full_backup(files, &inside, &repo, None, &[7u8; 32], &mut session)
            .expect("it is written, and reported as unsafe");
        assert!(full.evidence.inside_repository);
        assert!(
            !pemu_planner::rules::check_backup(Some(&full.evidence)).is_empty(),
            "the flow refuses it"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The rehearsal uses a seeded copy of the image while the plan uses the image itself: a
    /// rehearsal target with a 0xFF window could never fail on an erase of the window.
    #[test]
    fn the_rehearsal_image_carries_a_synthetic_cardid_and_the_plan_does_not() {
        use pemu_planner::rules::{CARDID_END, CARDID_OFFSET, with_synthetic_cardid};

        let mut image = vec![0xFFu8; 0x80_0000];
        image[0] = 0xE9;
        let rehearsal_image = with_synthetic_cardid(&image);
        let window = CARDID_OFFSET as usize..CARDID_END as usize;
        assert_ne!(rehearsal_image[window.clone()], image[window.clone()]);
        assert!(
            rehearsal_image[window.clone()].iter().any(|b| *b != 0xFF),
            "an erase of the window is visible against this target"
        );
        assert_eq!(
            rehearsal_image[..CARDID_OFFSET as usize],
            image[..CARDID_OFFSET as usize]
        );
        assert_eq!(
            rehearsal_image[CARDID_END as usize..],
            image[CARDID_END as usize..]
        );

        // A plan is never made from the seeded copy: the planner refuses a merged image whose
        // cardid window carries data.
        assert!(
            !pemu_planner::plan::plan_flash(
                &PlanRequest::write(ImageSource::Merged(&rehearsal_image), Origin::HumanCli),
                None,
            )
            .refusals
            .is_empty(),
            "an image with a written cardid window never plans"
        );
    }

    #[test]
    fn the_report_carries_the_full_backup_facts_and_nothing_else() {
        use pemu_planner::flow::FullBackupFacts;

        let facts = FullBackupFacts {
            evidence: BackupEvidence {
                verified: true,
                owner_only: true,
                inside_repository: false,
            },
            bytes: 0x80_0000,
        };
        let json = full_backup_json(Some(facts));
        assert_eq!(json["taken"], true);
        assert_eq!(json["verified"], true);
        assert_eq!(json["owner_only"], true);
        assert_eq!(json["inside_repository"], false);
        assert_eq!(json["bytes"], 0x80_0000);
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bytes",
                "inside_repository",
                "owner_only",
                "taken",
                "verified"
            ],
            "no file name, no stem, no digest"
        );
        assert_eq!(full_backup_json(None), serde_json::Value::Null);
    }

    /// No backup or salt error carries a host absolute path or the backup stem (the salted MAC
    /// digest).
    #[test]
    fn backup_and_salt_errors_name_no_path_and_no_stem() {
        let root = scratch_dir("device-b2-backups");
        let repo = scratch_dir("device-b2-repo");
        let files = OwnerOnlyFiles::host();
        files.create_dir(&repo).expect("repo");
        let key = [0xABu8; 16];
        let plan = [0x5Au8; 32];
        let stem = backup_stem(Some(&key), &plan);
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_owned());
        let mut errors: Vec<String> = Vec::new();

        let blocked = root.join("blocked");
        std::fs::create_dir_all(&root).expect("root");
        files.write(&blocked, b"not a directory").expect("file");
        let mut session = FakeWholePart {
            reads: 0,
            drift: false,
        };
        errors.push(
            full_backup(files, &blocked, &repo, Some(&key), &plan, &mut session)
                .expect_err("a file cannot hold run directories"),
        );
        errors.push(
            claim_run_dir(&files, &blocked, &stem).expect_err("the same, through the helper"),
        );
        let mut backups = OwnerOnlyBackups::new(OwnerOnlyFiles::host(), &blocked, &repo);
        errors.push(
            pemu_planner::flow::BackupStore::store(
                &mut backups,
                &plan,
                Some(&key),
                &[(0, vec![1])],
            )
            .expect_err("no directory, no backup"),
        );
        let salt_root = scratch_dir("device-b2-salt");
        std::fs::create_dir_all(&salt_root).expect("salt root");
        files
            .write(&salt_root.join("device"), b"not a directory")
            .expect("file");
        errors.push(mac_salt(&salt_root).expect_err("the device directory is a file"));

        for error in &errors {
            assert!(!error.contains(&stem), "the stem leaked: {error}");
            assert!(!error.contains("/Users"), "a host path leaked: {error}");
            assert!(!error.contains(&home), "a host path leaked: {error}");
            assert!(
                !error.contains(&root.to_string_lossy().into_owned()),
                "a host path leaked: {error}"
            );
            assert!(!error.is_empty(), "the failure still says something");
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&salt_root);
    }

    #[test]
    fn a_repository_that_holds_the_backups_refuses_before_anything_opens() {
        let data_root = scratch_dir("device-s1-root");
        std::fs::create_dir_all(&data_root).expect("data root");
        // The data root as its own repository.
        let error = backup_root_for(&data_root, &data_root).expect_err("the backups are inside it");
        assert!(error.contains("inside the repository"), "{error}");
        assert!(!error.contains("/Users"), "no host path in it: {error}");
        let parent = data_root.parent().expect("a parent").to_path_buf();
        backup_root_for(&data_root, &parent).expect_err("an ancestor holds it too");
        let repo = scratch_dir("device-s1-repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let root = backup_root_for(&data_root, &repo).expect("a repository elsewhere");
        assert!(root.is_dir() && root.ends_with("backups"));
        let _ = std::fs::remove_dir_all(&data_root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn an_existing_salt_is_read_back_and_never_overwritten() {
        let root = scratch_dir("device-salt-keep");
        let dir = root.join("device");
        let files = OwnerOnlyFiles::host();
        files.create_dir(&root).expect("root");
        files.create_dir(&dir).expect("device dir");
        let planted = [0x3Cu8; 32];
        files
            .write_new(&dir.join("mac-salt.bin"), &planted)
            .expect("plant a salt");
        assert_eq!(mac_salt(&root).expect("read back"), planted);
        assert_eq!(
            std::fs::read(dir.join("mac-salt.bin")).expect("still there"),
            planted.to_vec(),
            "the salt on disk is untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_mac_salt_is_created_once_and_reused() {
        let root = scratch_dir("device-salt");
        std::fs::create_dir_all(&root).expect("root");
        let first = mac_salt(&root).expect("a fresh salt");
        assert_ne!(first, [0u8; 32], "the OS entropy source answered");
        assert_eq!(mac_salt(&root).expect("reused"), first);
        let path = root.join("device").join("mac-salt.bin");
        OwnerOnlyFiles::host()
            .check(&path)
            .expect("the salt is owner-only");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("widen");
            let error = mac_salt(&root).expect_err("a wider salt is refused");
            assert!(error.contains("owner-only"), "{error}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn backup_evidence_uses_the_owner_only_check_and_containment() {
        let repo = scratch_dir("device-repo");
        let data = scratch_dir("device-data");
        let files = OwnerOnlyFiles::host();
        files.create_dir(&data).expect("data dir");
        let outside = data.join("backup.bin");
        files.write(&outside, b"sectors").expect("write");
        assert_eq!(
            backup_evidence(files, &outside, &repo, true),
            BackupEvidence {
                verified: true,
                owner_only: true,
                inside_repository: false
            }
        );
        // Inside the repository, spelled with another case: still inside.
        files.create_dir(&repo).expect("repo dir");
        let inside = repo.join("Backups").join("b.bin");
        files.create_dir(&repo.join("Backups")).expect("sub");
        files.write(&inside, b"sectors").expect("write");
        let upper = PathBuf::from(repo.to_string_lossy().to_uppercase()).join("backups/b.bin");
        assert!(backup_evidence(files, &upper, &repo, true).inside_repository);
        let guard = FakeOwnerOnly::new();
        guard.refuse(&outside);
        assert!(!backup_evidence(OwnerOnlyFiles::new(&guard), &outside, &repo, true).owner_only);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn the_backup_store_writes_owner_only_and_reads_back() {
        let root = scratch_dir("device-backups");
        let repo = scratch_dir("device-repo2");
        let files = OwnerOnlyFiles::host();
        files.create_dir(&root).expect("root");
        let mut store = OwnerOnlyBackups::new(files, &root, &repo);
        let evidence = store
            .store(
                &[7; 32],
                Some(&[3; 16]),
                &[(0x1_0000, vec![1, 2, 3]), (0, vec![9; 16])],
            )
            .expect("stored");
        assert_eq!(
            evidence,
            BackupEvidence {
                verified: true,
                owner_only: true,
                inside_repository: false
            }
        );
        let dir = store.last.clone().expect("dir");
        assert_eq!(
            files.read(&dir.join("00010000.bin")).expect("read"),
            [1, 2, 3]
        );
        let mut in_repo = OwnerOnlyBackups::new(files, &repo, &repo);
        files.create_dir(&repo).expect("repo");
        assert!(
            in_repo
                .store(&[8; 32], None, &[(0, vec![1])])
                .expect("stored")
                .inside_repository
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_retry_of_the_same_plan_leaves_the_first_backup_byte_identical() {
        let root = scratch_dir("device-backups-retry");
        let repo = scratch_dir("device-repo3");
        let files = OwnerOnlyFiles::host();
        files.create_dir(&root).expect("root");
        let mut store = OwnerOnlyBackups::new(files, &root, &repo);
        store
            .store(&[5; 32], Some(&[1; 16]), &[(0, vec![0xAA; 32])])
            .expect("run 1");
        let first = store.last.clone().expect("run 1 dir");
        store
            .store(&[5; 32], Some(&[1; 16]), &[(0, vec![0xBB; 32])])
            .expect("run 2");
        let second = store.last.clone().expect("run 2 dir");
        assert_ne!(first, second);
        assert!(first.to_string_lossy().ends_with("-run1"));
        assert!(second.to_string_lossy().ends_with("-run2"));
        assert_eq!(
            files.read(&first.join("00000000.bin")).expect("read"),
            [0xAA; 32]
        );
        assert_eq!(
            files.read(&second.join("00000000.bin")).expect("read"),
            [0xBB; 32]
        );
        let name = first
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with(&hex(&[1; 16])), "{name}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_runs_of_the_same_plan_get_distinct_directories() {
        let root = scratch_dir("device-backups-race");
        let repo = scratch_dir("device-repo4");
        OwnerOnlyFiles::host().create_dir(&root).expect("root");
        let runs = 8u8;
        let barrier = std::sync::Barrier::new(usize::from(runs));
        let dirs: Vec<PathBuf> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..runs)
                .map(|i| {
                    let (root, repo, barrier) = (&root, &repo, &barrier);
                    scope.spawn(move || {
                        let mut store = OwnerOnlyBackups::new(OwnerOnlyFiles::host(), root, repo);
                        barrier.wait();
                        store
                            .store(&[9; 32], Some(&[2; 16]), &[(0, vec![i; 64])])
                            .expect("stored");
                        store.last.expect("dir")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread"))
                .collect()
        });
        let unique: std::collections::BTreeSet<_> = dirs.iter().collect();
        assert_eq!(unique.len(), usize::from(runs), "{dirs:?}");
        let files = OwnerOnlyFiles::host();
        for (i, dir) in dirs.iter().enumerate() {
            assert_eq!(
                files.read(&dir.join("00000000.bin")).expect("read"),
                vec![i as u8; 64]
            );
        }
        let taken = root.join(format!("{}-{}-run{}", hex(&[4; 16]), hex(&[6; 8]), 1));
        std::fs::create_dir(&taken).expect("pre-existing");
        let mut store = OwnerOnlyBackups::new(files, &root, &repo);
        store
            .store(&[6; 32], Some(&[4; 16]), &[(0, vec![1])])
            .expect("stored");
        assert!(
            store
                .last
                .expect("dir")
                .to_string_lossy()
                .ends_with("-run2")
        );
        assert!(std::fs::read_dir(&taken).expect("taken").next().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    struct Everything;

    impl DevicePaths for Everything {
        fn is_flash_port(&self, _: &str) -> bool {
            true
        }
        fn case_insensitive(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_flash_port_must_be_a_device_name_by_the_host_predicate() {
        let paths = HostDevicePaths::new(&Everything);
        assert!(paths.is_flash_port(r"\\.\COM12"));
        assert!(paths.is_flash_port("COM3"));
        assert!(!paths.is_flash_port("rfc2217://127.0.0.1:4000"));
        assert!(!paths.is_flash_port("backup.bin"));
        assert!(paths.case_insensitive());
    }

    /// Every `COM<n>` is a flashing port, `COM10` and above included: they are not reserved names,
    /// but their open form `\\.\COM<n>` is a device-namespace path.
    #[cfg(feature = "device")]
    #[test]
    fn every_com_port_is_a_device_by_its_open_form() {
        use pemu_planner::exec::{CalloutPaths, ComPorts};

        let windows = HostDevicePaths::new(&ComPorts);
        for port in ["COM3", "com3", "COM12", "COM256"] {
            assert!(windows.is_flash_port(port), "{port}");
            assert!(windows.open_path(port).ends_with(port));
        }
        assert!(
            refuse_device(Path::new("COM12")).is_ok(),
            "COM12 as typed is not a reserved name, which is why the open form is checked"
        );
        for not_a_port in [
            "COM0",
            "COM3.txt",
            r"\\.\COM3",
            "backup.bin",
            "rfc2217://x:1",
        ] {
            assert!(!windows.is_flash_port(not_a_port), "{not_a_port}");
        }
        assert!(windows.case_insensitive());

        let macos = HostDevicePaths::new(&CalloutPaths);
        assert!(macos.is_flash_port(concat!("/dev/", "cu.usbmodem1101")));
        assert!(!macos.is_flash_port(concat!("/dev/", "tty.usbmodem1101")));
        assert!(!macos.is_flash_port("COM3"));
        assert!(!macos.case_insensitive());
    }

    #[test]
    fn the_host_dialog_never_turns_unavailable_into_yes() {
        use pemu_planner::flow::{Answer, ConfirmPrompt, Confirmer, DialogConfirmer};
        let prompt = ConfirmPrompt {
            intent: pemu_planner::flow::Intent::Flash,
            plan_sha256: "ab".repeat(32),
            writes: vec![("factory".to_owned(), 0x1_0000, 16)],
            erase_nvs: None,
            writes_leftover: false,
            full_backup: false,
        };
        let dialog = FakeDialog::default();
        dialog.make_unavailable("no window server");
        let port = HostDialog::new(&dialog);
        assert_eq!(
            DialogConfirmer::new(&port).confirm(&prompt),
            Answer::Unavailable("no window server".to_owned())
        );
        let dialog = FakeDialog::default();
        let port = HostDialog::new(&dialog);
        assert!(matches!(
            DialogConfirmer::new(&port).confirm(&prompt),
            Answer::Unavailable(_)
        ));
        dialog.answer_with(Confirmation::Declined);
        assert_eq!(
            DialogConfirmer::new(&port).confirm(&prompt),
            Answer::Declined
        );
        dialog.answer_with(Confirmation::Confirmed);
        assert_eq!(
            DialogConfirmer::new(&port).confirm(&prompt),
            Answer::Accepted {
                plan_sha256: "ab".repeat(32)
            }
        );
        assert!(
            dialog
                .asked()
                .iter()
                .all(|(_, body)| body.contains(&"ab".repeat(32)))
        );
    }

    #[test]
    fn host_entropy_varies() {
        let mut next = entropy_bytes().expect("the OS generator");
        let a: Vec<u8> = (0..64).map(|_| next()).collect();
        let b: Vec<u8> = (0..64).map(|_| next()).collect();
        assert_ne!(a, b);
    }

    /// A real flash's receipt carries the sector backup's evidence and the plan with its segment
    /// hashes (both were once always `null`).
    #[cfg(feature = "device")]
    #[test]
    fn a_flash_receipt_carries_the_backup_evidence_and_the_plan() {
        let mut report = pemu_planner::flow::FlowReport::default();
        report.device.backup = Some(BackupEvidence {
            verified: true,
            owner_only: true,
            inside_repository: false,
        });
        let plan = serde_json::json!({"writes": [{"name": "factory", "sha256": "ab"}]});
        let receipt = flash_receipt_json(&report, plan.clone());
        assert_eq!(receipt["plan"], plan);
        assert_eq!(
            receipt["backup"],
            serde_json::json!({"verified": true, "owner_only": true, "inside_repository": false})
        );
        let before = flash_receipt_json(&pemu_planner::flow::FlowReport::default(), plan);
        assert_eq!(before["backup"], serde_json::Value::Null);
    }
}
