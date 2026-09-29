//! The flash flow's order and confirmation rules, with fakes for every host effect: no port, no
//! process, no file.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use pemu_loader::esp_image::EspImage;
use pemu_loader::partitions::{ptype, subtype};
use pemu_loader::{hex, sha256};
use pemu_planner::flow::{
    BackupStore, CARDID_CHANGED_INSTRUCTIONS, Confirmed, DevicePaths, DeviceSession, Discovery,
    Effects, ElicitationConfirmer, ElicitationPort, Elicited, FlowError, FullBackupFacts, Identity,
    PortCandidate, SessionError, SessionOpener, StandingGrantConfirmer, Step, Terminal,
    TerminalConfirmer, WRITE_FAILED_INSTRUCTIONS, boot_check, flash_device, select_port,
    terminal_code,
};
use pemu_planner::plan::{
    ImageSource, Origin, PlanRequest, PlannedWrite, encode_partition_table, plan_flash,
};
use pemu_planner::rules::{
    BackupEvidence, CARDID_OFFSET, CHIP_REVISION, ChipRevision, Refusal, Rule,
};
use support::*;

type Log = Rc<RefCell<Vec<String>>>;

/// A flash chip in memory with the Passport layout and a synthetic cardid pattern. Writes erase
/// the covering sectors first, as the flasher stub does.
struct FakeFlash {
    name: &'static str,
    log: Log,
    flash: Vec<u8>,
    identity: Identity,
    busy: bool,
    corrupt_cardid_on_write: bool,
    /// The written app comes back wrong, so the verify fails while the write succeeded.
    corrupt_app_on_write: bool,
    /// A stub or layout that reaches too far erases the cardid window as well.
    erase_cardid_on_write: bool,
    fail_write: bool,
    boot_elf_override: Option<String>,
    /// The session cannot take the device-side region MD5 at all.
    no_region_md5: bool,
    /// How many whole-part reads this session served, and whether the second differs.
    full_reads: usize,
    unstable_reads: bool,
    resets: usize,
}

impl FakeFlash {
    fn new(name: &'static str, log: &Log) -> FakeFlash {
        let mut flash = vec![0xFFu8; 0x80_0000];
        let table = encode_partition_table(&official_layout());
        flash[0x8000..0x8000 + table.len()].copy_from_slice(&table);
        for (i, b) in flash[CARDID_OFFSET as usize..CARDID_OFFSET as usize + 0x4000]
            .iter_mut()
            .enumerate()
        {
            *b = (i as u8).wrapping_mul(37) ^ 0x5C;
        }
        FakeFlash {
            name,
            log: log.clone(),
            flash,
            identity: Identity {
                chip: "ESP32-C3".to_owned(),
                revision: CHIP_REVISION,
                flash_manufacturer: 0x20,
                flash_device: 0x4017,
                device_key: None,
            },
            busy: false,
            corrupt_cardid_on_write: false,
            corrupt_app_on_write: false,
            erase_cardid_on_write: false,
            fail_write: false,
            boot_elf_override: None,
            no_region_md5: false,
            full_reads: 0,
            unstable_reads: false,
            resets: 0,
        }
    }

    fn note(&self, what: &str) {
        self.log.borrow_mut().push(format!("{}:{what}", self.name));
    }

    fn cardid(&self) -> Vec<u8> {
        self.flash[CARDID_OFFSET as usize..CARDID_OFFSET as usize + 0x4000].to_vec()
    }
}

impl DeviceSession for FakeFlash {
    fn identify(&mut self) -> Result<Identity, SessionError> {
        self.note("identify");
        if self.busy {
            return Err(SessionError::Busy);
        }
        Ok(self.identity.clone())
    }
    fn read_partition_table(&mut self) -> Result<Vec<u8>, SessionError> {
        self.note("read_table");
        Ok(self.flash[0x8000..0x8C00].to_vec())
    }
    fn region_md5(&mut self, offset: u32, size: u32) -> Result<[u8; 16], SessionError> {
        self.note("md5");
        if self.no_region_md5 {
            return Err(SessionError::Unsupported(
                "the region MD5 needs esptool as a Python library".to_owned(),
            ));
        }
        let d = sha256(&self.flash[offset as usize..(offset + size) as usize]);
        Ok(d[..16].try_into().expect("16 bytes"))
    }
    fn read_region(&mut self, offset: u32, size: u32) -> Result<Vec<u8>, SessionError> {
        self.note(&format!("read {offset:#x} {size:#x}"));
        Ok(self.flash[offset as usize..(offset + size) as usize].to_vec())
    }
    fn read_full_flash(&mut self) -> Result<Vec<u8>, SessionError> {
        self.note("read-full");
        self.full_reads += 1;
        let mut whole = self.flash.clone();
        // With `unstable_reads`, every read after the first differs by one byte.
        if self.unstable_reads && self.full_reads > 1 {
            whole[0x20] ^= 0x01;
        }
        Ok(whole)
    }
    fn write(&mut self, writes: &[PlannedWrite]) -> Result<(), SessionError> {
        self.note("write");
        for w in writes {
            let (s, e) = w.sectors();
            self.flash[s as usize..e as usize].fill(0xFF);
            self.flash[w.offset as usize..w.offset as usize + w.data.len()]
                .copy_from_slice(&w.data);
        }
        if self.corrupt_cardid_on_write {
            self.flash[CARDID_OFFSET as usize] ^= 1;
        }
        if self.corrupt_app_on_write {
            self.flash[0x1_0000] ^= 0x40;
        }
        if self.erase_cardid_on_write {
            self.flash[CARDID_OFFSET as usize..CARDID_OFFSET as usize + 0x4000].fill(0xFF);
        }
        if self.fail_write {
            return Err(SessionError::Failed("write failed half way".to_owned()));
        }
        Ok(())
    }
    fn erase_region(&mut self, offset: u32, size: u32) -> Result<(), SessionError> {
        self.note(&format!("erase {offset:#x} {size:#x}"));
        self.flash[offset as usize..(offset + size) as usize].fill(0xFF);
        Ok(())
    }
    fn verify(&mut self, writes: &[PlannedWrite]) -> Result<(), SessionError> {
        self.note("verify");
        for w in writes {
            let got = &self.flash[w.offset as usize..w.offset as usize + w.data.len()];
            if sha256(got) != w.sha256 {
                return Err(SessionError::Failed("verify mismatch".to_owned()));
            }
        }
        Ok(())
    }
    fn hard_reset(&mut self) -> Result<(), SessionError> {
        self.note("hard_reset");
        self.resets += 1;
        Ok(())
    }
    fn boot_log(&mut self) -> Result<String, SessionError> {
        self.note("boot_log");
        let app = &self.flash[0x1_0000..];
        let elf = EspImage::parse(app)
            .ok()
            .and_then(|i| i.app_desc(app).ok().flatten())
            .map(|d| hex(&d.app_elf_sha256)[..9].to_owned())
            .unwrap_or_default();
        let elf = self.boot_elf_override.clone().unwrap_or(elf);
        Ok(format!(
            "ESP-ROM:esp32c3-api1-20210207\nrst:0x15 (USB_UART_CHIP_RESET),boot:0xa (SPI_FAST_FLASH_BOOT)\n\
             I (117) app_init: ELF file SHA256:  {elf}...\n"
        ))
    }
}

struct FakeDiscovery {
    log: Log,
    ports: Vec<PortCandidate>,
}

impl Discovery for FakeDiscovery {
    fn enumerate(&mut self) -> Result<Vec<PortCandidate>, String> {
        self.log.borrow_mut().push("enumerate".to_owned());
        Ok(self.ports.clone())
    }
}

/// Fake paths: `fake-cu-*` are flashing ports, `fake-tty-*` are the refused twins.
struct FakePaths;

impl DevicePaths for FakePaths {
    fn is_flash_port(&self, path: &str) -> bool {
        path.starts_with("fake-cu-")
    }
    fn case_insensitive(&self) -> bool {
        false
    }
}

struct Scripted {
    log: Log,
    answer: Option<Elicited>,
    prompts: Vec<String>,
}

fn digest_in(message: &str) -> String {
    message
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|w| w.len() == 64)
        .expect("the prompt shows the digest")
        .to_owned()
}

impl ElicitationPort for Scripted {
    fn elicit(&mut self, message: &str) -> Elicited {
        self.log.borrow_mut().push("confirm".to_owned());
        self.prompts.push(message.to_owned());
        match &self.answer {
            Some(a) => a.clone(),
            None => Elicited::Typed(digest_in(message)),
        }
    }
}

struct Opener {
    log: Log,
    device: Option<FakeFlash>,
    opened: usize,
}

impl SessionOpener for Opener {
    fn open(&mut self, confirmed: &Confirmed) -> Result<Box<dyn DeviceSession + '_>, SessionError> {
        self.log
            .borrow_mut()
            .push(format!("open {}", confirmed.port()));
        self.log
            .borrow_mut()
            .push(format!("token reset={}", confirmed.reset_confirmed()));
        self.opened += 1;
        let device = self.device.as_mut().expect("a device");
        Ok(Box::new(Borrowed(device)))
    }
}

struct Borrowed<'a>(&'a mut FakeFlash);

impl DeviceSession for Borrowed<'_> {
    fn identify(&mut self) -> Result<Identity, SessionError> {
        self.0.identify()
    }
    fn read_partition_table(&mut self) -> Result<Vec<u8>, SessionError> {
        self.0.read_partition_table()
    }
    fn region_md5(&mut self, o: u32, s: u32) -> Result<[u8; 16], SessionError> {
        self.0.region_md5(o, s)
    }
    fn read_region(&mut self, o: u32, s: u32) -> Result<Vec<u8>, SessionError> {
        self.0.read_region(o, s)
    }
    fn read_full_flash(&mut self) -> Result<Vec<u8>, SessionError> {
        self.0.read_full_flash()
    }
    fn write(&mut self, w: &[PlannedWrite]) -> Result<(), SessionError> {
        self.0.write(w)
    }
    fn erase_region(&mut self, o: u32, s: u32) -> Result<(), SessionError> {
        self.0.erase_region(o, s)
    }
    fn verify(&mut self, w: &[PlannedWrite]) -> Result<(), SessionError> {
        self.0.verify(w)
    }
    fn hard_reset(&mut self) -> Result<(), SessionError> {
        self.0.hard_reset()
    }
    fn boot_log(&mut self) -> Result<String, SessionError> {
        self.0.boot_log()
    }
}

struct Backups {
    evidence: BackupEvidence,
    regions: Vec<(u32, usize)>,
}

impl BackupStore for Backups {
    fn store(
        &mut self,
        _: &[u8; 32],
        _: Option<&[u8; 16]>,
        regions: &[(u32, Vec<u8>)],
    ) -> Result<BackupEvidence, String> {
        self.regions = regions.iter().map(|(o, b)| (*o, b.len())).collect();
        Ok(self.evidence)
    }
}

/// A whole-part store that records what it was given and answers fixed evidence.
struct FullBackups {
    evidence: BackupEvidence,
    /// `(bytes read, whether the two reads agreed)` per call.
    calls: Vec<(usize, bool)>,
    /// The device key it was keyed by.
    keys: Vec<Option<[u8; 16]>>,
    fail: bool,
}

impl pemu_planner::flow::FullBackup for FullBackups {
    fn store_full(
        &mut self,
        session: &mut dyn DeviceSession,
        _plan_sha256: &[u8; 32],
        device_key: Option<&[u8; 16]>,
    ) -> Result<FullBackupFacts, String> {
        let first = session
            .read_full_flash()
            .map_err(|e| format!("the full backup could not be read: {e:?}"))?;
        let second = session
            .read_full_flash()
            .map_err(|e| format!("the full backup could not be read again: {e:?}"))?;
        let agreed = first == second && !self.fail;
        self.calls.push((first.len(), agreed));
        self.keys.push(device_key.copied());
        if !agreed {
            return Err(
                "the two full-part reads differ, so the backup cannot be trusted".to_owned(),
            );
        }
        Ok(FullBackupFacts {
            evidence: self.evidence,
            bytes: first.len() as u64,
        })
    }
}

struct Rig {
    log: Log,
    full_backup: Option<Box<FullBackups>>,
    discovery: FakeDiscovery,
    rehearsal: FakeFlash,
    confirmer: Scripted,
    opener: Opener,
    backups: Backups,
}

impl Rig {
    fn new() -> Rig {
        let log: Log = Rc::default();
        Rig {
            discovery: FakeDiscovery {
                log: log.clone(),
                ports: vec![PortCandidate {
                    path: "fake-cu-1".to_owned(),
                    vid: 0x303A,
                    pid: 0x1001,
                }],
            },
            rehearsal: FakeFlash::new("emu", &log),
            confirmer: Scripted {
                log: log.clone(),
                answer: None,
                prompts: Vec::new(),
            },
            opener: Opener {
                log: log.clone(),
                device: Some(FakeFlash::new("dev", &log)),
                opened: 0,
            },
            full_backup: None,
            backups: Backups {
                evidence: BackupEvidence {
                    verified: true,
                    owner_only: true,
                    inside_repository: false,
                },
                regions: Vec::new(),
            },
            log,
        }
    }

    fn run(&mut self, image: &[u8]) -> (pemu_planner::flow::FlowReport, Result<(), FlowError>) {
        let request = PlanRequest::write(ImageSource::Merged(image), Origin::HumanCli);
        flash_device(
            &request,
            None,
            Effects {
                discovery: &mut self.discovery,
                paths: &FakePaths,
                rehearsal: &mut self.rehearsal,
                confirmer: &mut ElicitationConfirmer::new(&mut self.confirmer),
                opener: &mut self.opener,
                backups: &mut self.backups,
                full_backup: self
                    .full_backup
                    .as_deref_mut()
                    .map(|f| f as &mut dyn pemu_planner::flow::FullBackup),
            },
        )
    }

    /// [`Rig::run`] with the standing grant in place of the scripted person.
    fn run_granted(
        &mut self,
        image: &[u8],
    ) -> (pemu_planner::flow::FlowReport, Result<(), FlowError>) {
        let request = PlanRequest::write(ImageSource::Merged(image), Origin::HumanCli);
        flash_device(
            &request,
            None,
            Effects {
                discovery: &mut self.discovery,
                paths: &FakePaths,
                rehearsal: &mut self.rehearsal,
                confirmer: &mut StandingGrantConfirmer::new(),
                opener: &mut self.opener,
                backups: &mut self.backups,
                full_backup: self
                    .full_backup
                    .as_deref_mut()
                    .map(|f| f as &mut dyn pemu_planner::flow::FullBackup),
            },
        )
    }

    fn device(&self) -> &FakeFlash {
        self.opener.device.as_ref().expect("device")
    }

    fn log_has(&self, prefix: &str) -> bool {
        self.log.borrow().iter().any(|l| l.starts_with(prefix))
    }
}

fn refused_rules(result: &Result<(), FlowError>) -> Vec<Rule> {
    match result {
        Err(FlowError::Refused(r)) => r.iter().map(|r| r.rule).collect(),
        _ => Vec::new(),
    }
}

#[test]
fn an_accepted_flash_runs_the_ten_steps_in_order() {
    let mut rig = Rig::new();
    let image = official_padded();
    let cardid_before = rig.device().cardid();
    let (report, result) = rig.run(&image);
    assert_eq!(result, Ok(()));
    // The rehearsal (Guard, Write, Verify, BootCheck on the emulator) runs before the person is
    // asked, and the device is first opened at Identify, after Confirm.
    assert_eq!(
        report.rehearsal.step_order(),
        [Step::Guard, Step::Write, Step::Verify, Step::BootCheck]
    );
    assert_eq!(report.rehearsal.cardid_unchanged, Some(true));
    assert_eq!(
        report.device.step_order(),
        [
            Step::Discover,
            Step::Plan,
            Step::Rehearse,
            Step::Confirm,
            Step::Identify,
            Step::Guard,
            Step::Backup,
            Step::Write,
            Step::Verify,
            Step::BootCheck,
        ]
    );
    let log = rig.log.borrow().clone();
    let confirm = log.iter().position(|l| l == "confirm").expect("confirmed");
    let open = log
        .iter()
        .position(|l| l.starts_with("open "))
        .expect("opened");
    let first_dev = log
        .iter()
        .position(|l| l.starts_with("dev:"))
        .expect("device used");
    assert!(confirm < open && open < first_dev, "{log:?}");
    assert!(log.iter().take(confirm).all(|l| !l.starts_with("dev:")));
    assert!(report.device.steps.iter().all(|(_, ok)| *ok));
    assert_eq!(report.device.cardid_unchanged, Some(true));
    let device = rig.device();
    assert_eq!(device.cardid(), cardid_before, "cardid is bit-identical");
    assert_eq!(&device.flash[..0x9000], &image[..0x9000]);
    assert_eq!(
        &device.flash[0x1_0000..0x2_0000],
        &image[0x1_0000..0x2_0000]
    );
    // The backup covers exactly the sectors written: bootloader, table and app, never nvs or cardid.
    for (offset, len) in &rig.backups.regions {
        let end = u64::from(*offset) + *len as u64;
        assert!(end <= 0x9000 || *offset >= 0x1_0000);
        assert!(end <= u64::from(CARDID_OFFSET));
    }
}

#[test]
fn a_declined_confirmation_never_opens_the_device() {
    let mut rig = Rig::new();
    rig.confirmer.answer = Some(Elicited::Declined);
    let (report, result) = rig.run(&official_unpadded());
    assert_eq!(result, Err(FlowError::Declined));
    assert_eq!(result.unwrap_err().code(), "E_PLAN_REFUSED");
    assert_eq!(rig.opener.opened, 0);
    assert!(!rig.log_has("dev:"));
    assert!(!report.device.step_order().contains(&Step::Identify));
}

#[test]
fn an_unavailable_confirmation_path_never_opens_the_device() {
    let mut rig = Rig::new();
    rig.confirmer.answer = Some(Elicited::Unavailable("no interactive console".to_owned()));
    let (_, result) = rig.run(&official_unpadded());
    assert!(matches!(result, Err(FlowError::ConfirmationUnavailable(_))));
    assert_eq!(rig.opener.opened, 0);
}

#[test]
fn rule_confirmation_mismatch() {
    let mut rig = Rig::new();
    rig.confirmer.answer = Some(Elicited::Typed("0".repeat(64)));
    let (_, result) = rig.run(&official_unpadded());
    assert_eq!(refused_rules(&result), [Rule::ConfirmationMismatch]);
    assert_eq!(rig.opener.opened, 0);
}

#[test]
fn a_refused_plan_neither_rehearses_nor_asks() {
    let mut rig = Rig::new();
    let mut image = official_padded();
    image[0x35_7000] = 0;
    let (report, result) = rig.run(&image);
    assert!(refused_rules(&result).contains(&Rule::DataDropped));
    assert!(!rig.log_has("emu:") && !rig.log_has("confirm") && !rig.log_has("open "));
    assert_eq!(report.device.step_order(), [Step::Discover, Step::Plan]);
    assert!(report.rehearsal.steps.is_empty());
}

#[test]
fn a_failed_rehearsal_never_asks_a_person() {
    let mut rig = Rig::new();
    rig.rehearsal.corrupt_cardid_on_write = true;
    let (_, result) = rig.run(&official_unpadded());
    assert!(
        matches!(result, Err(FlowError::CheckFailed(_))),
        "{result:?}"
    );
    assert!(!rig.log_has("confirm") && !rig.log_has("open "));
}

/// Only possible because the rehearsal target carries a synthetic cardid: the second half shows an
/// unseeded (0xFF) window passing the erase, which is why `pemu_host::device::run_flash` seeds it.
#[test]
fn an_erase_that_reaches_the_cardid_window_fails_the_rehearsal() {
    use pemu_planner::rules::{CARDID_END, synthetic_cardid, with_synthetic_cardid};

    let mut rig = Rig::new();
    rig.rehearsal.erase_cardid_on_write = true;
    assert_eq!(
        rig.rehearsal.cardid(),
        synthetic_cardid(),
        "the rehearsal target starts from a synthetic pattern, not from 0xFF"
    );
    let (report, result) = rig.run(&official_padded());
    assert!(
        matches!(result, Err(FlowError::CheckFailed(_))),
        "the rehearsal catches the erase: {result:?}"
    );
    assert_eq!(
        report.rehearsal.cardid_unchanged,
        Some(false),
        "the guard saw the window change"
    );
    assert!(!rig.log_has("confirm") && !rig.log_has("open "));
    assert!(!rig.log_has("dev:"));

    // Against an unseeded 0xFF window the guard reports "unchanged".
    let mut blind = Rig::new();
    blind.rehearsal.erase_cardid_on_write = true;
    blind.rehearsal.flash[CARDID_OFFSET as usize..CARDID_END as usize].fill(0xFF);
    let (blind_report, blind_result) = blind.run(&official_padded());
    assert_eq!(
        blind_result,
        Ok(()),
        "the erase is invisible on a 0xFF window"
    );
    assert_eq!(blind_report.rehearsal.cardid_unchanged, Some(true));

    let image = official_padded();
    let seeded = with_synthetic_cardid(&image);
    assert_eq!(seeded.len(), image.len());
    assert_eq!(
        seeded[CARDID_OFFSET as usize..CARDID_END as usize],
        synthetic_cardid()[..]
    );
    assert_eq!(
        seeded[..CARDID_OFFSET as usize],
        image[..CARDID_OFFSET as usize]
    );
    assert_eq!(seeded[CARDID_END as usize..], image[CARDID_END as usize..]);
    // The unpadded build output ends before the window, so it is grown to it and seeded there too.
    let short = official_unpadded();
    let grown = with_synthetic_cardid(&short);
    assert_eq!(grown.len(), CARDID_END as usize);
    assert_eq!(
        grown[CARDID_OFFSET as usize..CARDID_END as usize],
        synthetic_cardid()[..]
    );
    assert_eq!(grown[..short.len()], short[..]);
}

#[test]
fn a_rehearsal_target_with_another_flash_id_refuses() {
    let mut rig = Rig::new();
    rig.rehearsal.identity.flash_manufacturer = 0xC8;
    let (_, result) = rig.run(&official_unpadded());
    assert!(refused_rules(&result).contains(&Rule::FlashId));
    assert!(!rig.log_has("confirm"));
}

#[test]
fn rule_no_device() {
    let mut rig = Rig::new();
    rig.discovery.ports[0].pid = 0x1002;
    let (_, result) = rig.run(&official_unpadded());
    assert_eq!(refused_rules(&result), [Rule::NoDevice]);
    assert!(!rig.log_has("emu:"));
}

#[test]
fn rule_ambiguous_port() {
    let mut rig = Rig::new();
    let mut second = rig.discovery.ports[0].clone();
    second.path = "fake-cu-2".to_owned();
    rig.discovery.ports.push(second);
    let (_, result) = rig.run(&official_unpadded());
    assert_eq!(refused_rules(&result), [Rule::AmbiguousPort]);
}

#[test]
fn rule_port_not_allowed() {
    let ports = vec![
        PortCandidate {
            path: "fake-cu-1".to_owned(),
            vid: 0x303A,
            pid: 0x1001,
        },
        PortCandidate {
            path: "fake-tty-1".to_owned(),
            vid: 0x303A,
            pid: 0x1001,
        },
    ];
    for requested in ["fake-tty-1", "fake-cu-9", "FAKE-CU-1"] {
        let refusal = select_port(&ports, Some(requested), &FakePaths).expect_err(requested);
        assert_eq!(refusal.rule, Rule::PortNotAllowed, "{requested}");
    }
    assert_eq!(
        select_port(&ports, None, &FakePaths).as_deref(),
        Ok("fake-cu-1"),
        "the tty twin is not a candidate"
    );
    struct Insensitive;
    impl DevicePaths for Insensitive {
        fn is_flash_port(&self, _: &str) -> bool {
            true
        }
        fn case_insensitive(&self) -> bool {
            true
        }
    }
    assert_eq!(
        select_port(&ports, Some("FAKE-CU-1"), &Insensitive).as_deref(),
        Ok("fake-cu-1")
    );
}

#[test]
fn a_device_identity_refusal_resets_the_device_and_writes_nothing() {
    for mutate in [
        (|d: &mut FakeFlash| d.identity.revision = ChipRevision { major: 0, minor: 3 })
            as fn(&mut FakeFlash),
        |d| d.identity.flash_device = 0x4016,
        |d| {
            let mut layout = official_layout();
            layout[3].offset = 0x35_8000;
            let table = encode_partition_table(&layout);
            d.flash[0x8000..0x8C00].copy_from_slice(&table);
        },
    ] {
        let mut rig = Rig::new();
        mutate(rig.opener.device.as_mut().expect("device"));
        let (_, result) = rig.run(&official_unpadded());
        let rules = refused_rules(&result);
        assert!(
            rules
                .iter()
                .any(|r| matches!(r, Rule::ChipRevision | Rule::FlashId | Rule::CardidMoved)),
            "{rules:?}"
        );
        assert_eq!(
            rig.device().resets,
            1,
            "reset back to the app before returning"
        );
        assert!(!rig.log_has("dev:write") && !rig.log_has("dev:read "));
    }
}

#[test]
fn a_stored_sector_backup_leaves_its_evidence_in_the_report() {
    let mut rig = Rig::new();
    let (report, result) = rig.run(&official_unpadded());
    assert_eq!(result, Ok(()));
    assert_eq!(report.device.backup, Some(rig.backups.evidence));
    assert!(report.device.backup.is_some_and(|e| e.verified));
}

#[test]
fn a_backup_that_is_not_owner_only_or_inside_the_repository_stops_before_the_write() {
    for (evidence, rule) in [
        (
            BackupEvidence {
                verified: true,
                owner_only: false,
                inside_repository: false,
            },
            Rule::BackupNotOwnerOnly,
        ),
        (
            BackupEvidence {
                verified: true,
                owner_only: true,
                inside_repository: true,
            },
            Rule::BackupInsideRepository,
        ),
        (
            BackupEvidence {
                verified: false,
                owner_only: true,
                inside_repository: false,
            },
            Rule::NoVerifiedBackup,
        ),
    ] {
        let mut rig = Rig::new();
        rig.backups.evidence = evidence;
        let (report, result) = rig.run(&official_unpadded());
        assert_eq!(refused_rules(&result), [rule]);
        // The refused evidence is still reported, so the receipt says why the flow stopped.
        assert_eq!(report.device.backup, Some(evidence));
        assert!(!rig.log_has("dev:write"));
        assert_eq!(rig.device().resets, 1);
    }
}

#[test]
fn a_changed_cardid_returns_e_cardid_changed() {
    let mut rig = Rig::new();
    rig.opener
        .device
        .as_mut()
        .expect("device")
        .corrupt_cardid_on_write = true;
    let (report, result) = rig.run(&official_unpadded());
    assert_eq!(
        result,
        Err(FlowError::CardidChanged(CARDID_CHANGED_INSTRUCTIONS))
    );
    assert_eq!(
        FlowError::CardidChanged(CARDID_CHANGED_INSTRUCTIONS).code(),
        "E_CARDID_CHANGED"
    );
    assert_eq!(report.device.cardid_unchanged, Some(false));
    assert_eq!(rig.device().resets, 1);
    assert!(!rig.log_has("dev:boot_log"));
}

#[test]
fn a_busy_port_returns_e_device_busy() {
    let mut rig = Rig::new();
    rig.opener.device.as_mut().expect("device").busy = true;
    let (_, result) = rig.run(&official_unpadded());
    assert_eq!(result, Err(FlowError::DeviceBusy));
    assert_eq!(FlowError::DeviceBusy.code(), "E_DEVICE_BUSY");
}

#[test]
fn a_boot_check_with_another_elf_prefix_fails() {
    let mut rig = Rig::new();
    rig.opener
        .device
        .as_mut()
        .expect("device")
        .boot_elf_override = Some("123456789".to_owned());
    let (report, result) = rig.run(&official_unpadded());
    assert!(matches!(result, Err(FlowError::CheckFailed(_))));
    assert_eq!(
        report.device.boot.map(|b| (b.banner, b.elf_sha256_matches)),
        Some((true, false))
    );
}

#[test]
fn a_person_erasing_nvs_erases_only_nvs_after_the_backup() {
    let mut rig = Rig::new();
    let image = official_unpadded();
    let mut request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
    request.erase_nvs = true;
    let (_, result) = flash_device(
        &request,
        None,
        Effects {
            discovery: &mut rig.discovery,
            paths: &FakePaths,
            rehearsal: &mut rig.rehearsal,
            confirmer: &mut ElicitationConfirmer::new(&mut rig.confirmer),
            opener: &mut rig.opener,
            backups: &mut rig.backups,
            full_backup: None,
        },
    );
    assert_eq!(result, Ok(()));
    assert!(rig.backups.regions.contains(&(0x9000, 0x6000)));
    let log = rig.log.borrow().clone();
    let erase = log.iter().position(|l| l == "dev:erase 0x9000 0x6000");
    let backup_read = log.iter().position(|l| l == "dev:read 0x9000 0x6000");
    assert!(backup_read < erase && erase.is_some(), "{log:?}");
}

#[test]
fn boot_check_reads_the_banner_and_the_elf_prefix() {
    let digest = ELF_SHA;
    let prefix = &hex(&digest)[..9];
    let log = format!(
        "rst:0x15 (USB_UART_CHIP_RESET),boot:0xa (SPI_FAST_FLASH_BOOT)\nI (117) app_init: ELF file SHA256:  {prefix}...\n"
    );
    assert!(boot_check(&log, Some(&digest)).passed());
    assert!(!boot_check(&log, None).passed());
    assert!(
        !boot_check(
            "I (117) app_init: ELF file SHA256:  0a1122334...",
            Some(&digest)
        )
        .banner
    );
    let short = log.replace(prefix, &prefix[..4]);
    assert!(!boot_check(&short, Some(&digest)).elf_sha256_matches);
}

struct FakeTerminal {
    shown: Vec<String>,
    typed: Option<String>,
}

impl Terminal for FakeTerminal {
    fn show(&mut self, text: &str) {
        self.shown.push(text.to_owned());
    }
    fn read_line(&mut self) -> Option<String> {
        self.typed.take()
    }
}

/// Not the report, the result, the prompt or a refusal.
#[test]
fn the_confirmation_code_appears_only_on_the_terminal() {
    let bytes = |seed: u8| {
        let mut n = seed;
        move || {
            n = n.wrapping_mul(29).wrapping_add(17);
            n
        }
    };
    let code = terminal_code(&mut bytes(7));
    assert_eq!(code.len(), 8);
    assert_ne!(code, terminal_code(&mut bytes(8)));
    for typed in [code.clone(), "WRONG123".to_owned()] {
        let mut rig = Rig::new();
        let mut terminal = FakeTerminal {
            shown: Vec::new(),
            typed: Some(typed.clone()),
        };
        let mut entropy = bytes(7);
        let mut confirmer = TerminalConfirmer::new(&mut terminal, &mut entropy);
        let image = official_unpadded();
        let request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
        let (report, result) = flash_device(
            &request,
            None,
            Effects {
                discovery: &mut rig.discovery,
                paths: &FakePaths,
                rehearsal: &mut rig.rehearsal,
                confirmer: &mut confirmer,
                opener: &mut rig.opener,
                backups: &mut rig.backups,
                full_backup: None,
            },
        );
        assert!(terminal.shown.iter().any(|t| t.contains(&code)));
        if typed == code {
            assert_eq!(result, Ok(()));
        } else {
            assert_eq!(result, Err(FlowError::Declined));
            assert_eq!(rig.opener.opened, 0);
        }
        let prompt_text = terminal.shown[0].clone();
        let outputs = [
            format!("{report:?}"),
            format!("{result:?}"),
            prompt_text,
            rig.log.borrow().join("\n"),
            result
                .as_ref()
                .err()
                .map(|e| e.code().to_owned())
                .unwrap_or_default(),
        ];
        for output in outputs {
            assert!(!output.contains(&code), "the code leaked into: {output}");
        }
    }
}

#[test]
fn a_failed_write_that_changed_cardid_still_reports_e_cardid_changed() {
    let mut rig = Rig::new();
    {
        let device = rig.opener.device.as_mut().expect("device");
        device.corrupt_cardid_on_write = true;
        device.fail_write = true;
    }
    let (report, result) = rig.run(&official_unpadded());
    assert_eq!(
        result,
        Err(FlowError::CardidChanged(CARDID_CHANGED_INSTRUCTIONS))
    );
    assert_eq!(report.device.cardid_unchanged, Some(false));
    assert!(report.device.steps.contains(&(Step::Write, false)));
    assert!(!report.device.step_order().contains(&Step::Verify));
    assert_eq!(rig.device().resets, 1);

    // A failed write that left cardid alone is a session error, still reset once.
    let mut rig = Rig::new();
    rig.opener.device.as_mut().expect("device").fail_write = true;
    let (report, result) = rig.run(&official_unpadded());
    assert!(matches!(result, Err(FlowError::Session(_))), "{result:?}");
    assert_eq!(report.device.cardid_unchanged, Some(true));
    assert_eq!(rig.device().resets, 1);
}

#[test]
fn terminal_code_rejects_biased_bytes() {
    let mut stream = [255u8, 248, 0, 30, 31, 247, 1, 2, 3, 4].into_iter();
    let code = terminal_code(&mut || stream.next().expect("enough bytes"));
    // 255 and 248 are rejected; 0->A, 30->9, 31->A, 247->(247%31=30)->9, 1->B, 2->C, 3->D, 4->E.
    assert_eq!(code, "A9A9BCDE");
    let mut counts = [0u32; 31];
    for byte in 0..=255u8 {
        if byte < 248 {
            counts[usize::from(byte % 31)] += 1;
        }
    }
    assert!(counts.iter().all(|c| *c == 8), "{counts:?}");
}

/// Directly or through an alias. Every `.rs` file is scanned: files under `tests`, benches and
/// examples whole, others from their first test marker to the end; aliases (`type X = ...;`,
/// `use ... as X;`) are followed to a fixed point.
#[test]
#[allow(clippy::disallowed_methods)] // a source scan of the repository, test code only
fn no_test_constructs_the_real_device_opener() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let me = std::path::Path::new(file!())
        .file_name()
        .expect("name")
        .to_owned();
    let mut sources = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if !matches!(
                    name.to_str(),
                    Some("target" | ".git" | "node_modules" | "wt")
                ) {
                    stack.push(path);
                }
                continue;
            }
            if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                sources.push((path, text));
            }
        }
    }
    let mut needles = vec![concat!("Device", "Opener").to_owned()];
    loop {
        let mut grew = false;
        for (_, text) in &sources {
            for line in text.lines() {
                let line = line.trim();
                let alias = if let Some(rest) = line
                    .strip_prefix("pub type ")
                    .or(line.strip_prefix("type "))
                {
                    rest.split_once('=')
                        .filter(|(_, rhs)| needles.iter().any(|n| rhs.contains(n.as_str())))
                        .map(|(lhs, _)| lhs.split(['<', ' ']).next().unwrap_or("").to_owned())
                } else if line.contains(" as ") && line.contains("use ") {
                    needles
                        .iter()
                        .find_map(|n| line.split_once(&format!("{n} as ")))
                        .map(|(_, rhs)| {
                            rhs.split(|c: char| !c.is_alphanumeric() && c != '_')
                                .next()
                                .unwrap_or("")
                                .to_owned()
                        })
                } else {
                    None
                };
                if let Some(alias) = alias.filter(|a| !a.is_empty() && !needles.contains(a)) {
                    needles.push(alias);
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    let mut offenders = Vec::new();
    for (path, text) in &sources {
        if path.file_name() == Some(me.as_os_str()) {
            continue;
        }
        let whole = path.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some("tests" | "benches" | "examples")
            )
        });
        let start = if whole {
            Some(0)
        } else {
            ["#[cfg(test)]", "#[test]", "cfg(any(test"]
                .iter()
                .filter_map(|m| text.find(m))
                .min()
        };
        let Some(start) = start else { continue };
        let test_code = &text[start..];
        let is_word = |at: usize, len: usize| {
            let before = test_code[..at].chars().next_back();
            let after = test_code[at + len..].chars().next();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
        };
        for needle in &needles {
            if test_code
                .match_indices(needle.as_str())
                .any(|(at, _)| is_word(at, needle.len()))
            {
                offenders.push(format!("{} ({needle})", path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the device opener appears in test code: {offenders:?}"
    );
}

#[test]
fn the_prompt_names_no_port_path() {
    let mut rig = Rig::new();
    let (_, result) = rig.run(&official_unpadded());
    assert_eq!(result, Ok(()));
    assert_eq!(rig.confirmer.prompts.len(), 1);
    let prompt = &rig.confirmer.prompts[0];
    assert!(prompt.contains("the connected AI Passport"), "{prompt}");
    assert!(!prompt.contains("fake-cu"), "{prompt}");
}

#[test]
fn the_prompt_notes_a_write_into_the_leftover_region() {
    let mut rig = Rig::new();
    let _ = rig.run(&official_unpadded());
    assert!(!rig.confirmer.prompts[0].contains("0x310000, 0x356000"));

    let mut layout = official_layout();
    layout.push(part(
        "ota_0",
        ptype::APP,
        subtype::OTA_0,
        0x31_0000,
        0x4_6000,
    ));
    let app = app_image(ELF_SHA, 0x1000);
    let mut image = merged(&layout, &app, 0x31_0000 + app.len());
    image[0x31_0000..].copy_from_slice(&app);
    let mut rig = Rig::new();
    let _ = rig.run(&image);
    assert_eq!(rig.confirmer.prompts.len(), 1, "the plan reaches Confirm");
    let prompt = &rig.confirmer.prompts[0];
    assert!(prompt.contains("[0x310000, 0x356000)"), "{prompt}");
}

#[test]
fn sector_rounding_does_not_wrap_at_four_gigabytes() {
    let write = PlannedWrite {
        name: "far".to_owned(),
        offset: 0xFFFF_F000,
        data: vec![0; 0x2000],
        sha256: [0; 32],
    };
    assert_eq!(write.sectors(), (0xFFFF_F000, 0x1_0000_1000));
}

#[test]
fn cardid_changed_carries_human_instructions() {
    let FlowError::CardidChanged(text) = FlowError::CardidChanged(CARDID_CHANGED_INSTRUCTIONS)
    else {
        unreachable!()
    };
    assert!(text.contains("do not flash again"));
    assert!(text.contains("human-only"));
    assert!(!text.contains("/dev/"));
}

#[test]
fn the_leftover_note_covers_an_erase() {
    use pemu_planner::flow::ConfirmPrompt;
    use pemu_planner::plan::Plan;
    let plan = Plan {
        writes: Vec::new(),
        erase_nvs: Some((0x31_0000, 0x4_6000)),
        plan_sha256: [7; 32],
        app_elf_sha256: None,
    };
    assert!(
        ConfirmPrompt::of(&plan, false)
            .render()
            .contains("[0x310000, 0x356000)")
    );
    let plan = Plan {
        erase_nvs: Some((0x9000, 0x6000)),
        ..plan
    };
    assert!(
        !ConfirmPrompt::of(&plan, false)
            .render()
            .contains("[0x310000, 0x356000)")
    );
}

/// An empty answer cancels.
#[test]
fn the_planner_judges_the_typed_digest() {
    let image = official_unpadded();
    let digest = pemu_planner::plan::plan_flash(
        &PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli),
        None,
    )
    .plan
    .plan_sha256_hex();
    let mut rig = Rig::new();
    rig.confirmer.answer = Some(Elicited::Typed(format!(
        "  {}\n",
        digest.to_ascii_uppercase()
    )));
    let (_, result) = rig.run(&image);
    assert_eq!(result, Ok(()));
    assert!(rig.confirmer.prompts[0].contains("Type the digest above to flash"));

    let mut rig = Rig::new();
    rig.confirmer.answer = Some(Elicited::Typed("   ".to_owned()));
    let (_, result) = rig.run(&image);
    assert_eq!(result, Err(FlowError::Declined));
    assert_eq!(rig.opener.opened, 0);

    let mut rig = Rig::new();
    rig.confirmer.answer = Some(Elicited::Typed("yes".to_owned()));
    let (_, result) = rig.run(&image);
    assert_eq!(refused_rules(&result), [Rule::ConfirmationMismatch]);
    assert_eq!(rig.opener.opened, 0);
}

#[test]
fn a_device_without_a_region_md5_stops_at_the_guard_before_the_backup() {
    let mut rig = Rig::new();
    rig.opener.device.as_mut().expect("device").no_region_md5 = true;
    let (report, result) = rig.run(&official_padded());
    assert!(matches!(result, Err(FlowError::Session(_))), "{result:?}");
    assert_eq!(
        report.device.step_order(),
        [
            Step::Discover,
            Step::Plan,
            Step::Rehearse,
            Step::Confirm,
            Step::Identify,
            Step::Guard
        ]
    );
    assert_eq!(report.device.steps.last(), Some(&(Step::Guard, false)));
    assert_eq!(report.device.cardid_unchanged, None);
    assert!(rig.backups.regions.is_empty(), "no backup was taken");
    assert!(!rig.log_has("dev write"), "nothing was written");
    assert!(!rig.log_has("dev verify"));
    assert_eq!(
        rig.device().resets,
        1,
        "the device is reset back to the app"
    );
}

#[test]
fn a_full_backup_is_read_twice_before_the_write() {
    let mut rig = Rig::new();
    rig.full_backup = Some(Box::new(FullBackups {
        evidence: BackupEvidence {
            verified: true,
            owner_only: true,
            inside_repository: false,
        },
        calls: Vec::new(),
        keys: Vec::new(),
        fail: false,
    }));
    let (_, result) = rig.run(&official_padded());
    assert_eq!(result, Ok(()));
    let full = rig.full_backup.as_ref().expect("a full store");
    assert_eq!(
        full.calls,
        [(0x80_0000usize, true)],
        "8 MB, read twice, agreed"
    );
    assert_eq!(full.keys, [None], "this fake device reports no MAC");
    let log = rig.log.borrow().clone();
    let full_read = log.iter().position(|l| l == "dev:read-full");
    let write = log.iter().position(|l| l.starts_with("dev:write"));
    assert!(full_read.is_some() && full_read < write, "{log:?}");
}

#[test]
fn a_full_backup_leaves_evidence_in_the_report_and_in_the_prompt() {
    let evidence = BackupEvidence {
        verified: true,
        owner_only: true,
        inside_repository: false,
    };
    let mut rig = Rig::new();
    rig.full_backup = Some(Box::new(FullBackups {
        evidence,
        calls: Vec::new(),
        keys: Vec::new(),
        fail: false,
    }));
    let (report, result) = rig.run(&official_padded());
    assert_eq!(result, Ok(()));
    assert_eq!(
        report.device.full_backup,
        Some(FullBackupFacts {
            evidence,
            bytes: 0x80_0000,
        })
    );
    let prompt = rig.confirmer.prompts[0].clone();
    assert!(prompt.contains("whole 8 MB part"), "{prompt}");
    assert!(prompt.contains("cardid and nvs included"), "{prompt}");
    // Nothing of the backup's name, place or contents is in the report.
    let rendered = format!("{report:?}");
    assert!(
        !rendered.contains("full-8mb") && !rendered.contains('/'),
        "{rendered}"
    );

    // A run without `--backup` reports none, and its prompt promises none.
    let mut plain = Rig::new();
    let (plain_report, plain_result) = plain.run(&official_padded());
    assert_eq!(plain_result, Ok(()));
    assert_eq!(plain_report.device.full_backup, None);
    assert!(!plain.confirmer.prompts[0].contains("whole 8 MB part"));

    // A whole-part backup the rules refuse is not reported as one either.
    let mut refused = Rig::new();
    refused.full_backup = Some(Box::new(FullBackups {
        evidence: BackupEvidence {
            verified: false,
            owner_only: true,
            inside_repository: false,
        },
        calls: Vec::new(),
        keys: Vec::new(),
        fail: false,
    }));
    let (refused_report, refused_result) = refused.run(&official_padded());
    assert!(refused_result.is_err());
    assert_eq!(refused_report.device.full_backup, None);
}

#[test]
fn a_failed_write_says_the_device_was_reset_and_the_backup_is_kept() {
    for (what, fail_write) in [("write", true), ("verify", false)] {
        let mut rig = Rig::new();
        {
            let device = rig.opener.device.as_mut().expect("device");
            device.fail_write = fail_write;
            device.corrupt_app_on_write = !fail_write;
        }
        let (report, result) = rig.run(&official_padded());
        let Err(FlowError::Session(why)) = result else {
            panic!("{what}: {result:?}");
        };
        assert!(why.contains(WRITE_FAILED_INSTRUCTIONS), "{what}: {why}");
        assert!(why.contains("reset back to the app"), "{what}: {why}");
        assert!(why.contains("backup of this run"), "{what}: {why}");
        assert!(
            report
                .device
                .steps
                .contains(&(pemu_planner::flow::Step::Backup, true)),
            "{what}: {:?}",
            report.device.steps
        );
        assert_eq!(rig.device().resets, 1, "{what}");
        // The rehearsal has no backup to point at, so its failure carries no such promise.
        let mut emu = Rig::new();
        emu.rehearsal.fail_write = true;
        let (_, emu_result) = emu.run(&official_padded());
        assert!(
            !format!("{emu_result:?}").contains("backup of this run"),
            "{emu_result:?}"
        );
    }
}

#[test]
fn a_full_backup_that_cannot_be_trusted_stops_before_the_write() {
    let mut rig = Rig::new();
    rig.full_backup = Some(Box::new(FullBackups {
        evidence: BackupEvidence {
            verified: true,
            owner_only: true,
            inside_repository: false,
        },
        calls: Vec::new(),
        keys: Vec::new(),
        fail: false,
    }));
    rig.opener.device.as_mut().expect("device").unstable_reads = true;
    let (report, result) = rig.run(&official_padded());
    assert!(matches!(result, Err(FlowError::Session(_))), "{result:?}");
    assert_eq!(report.device.steps.last(), Some(&(Step::Backup, false)));
    assert!(!rig.log_has("dev:write"), "nothing was written");
    assert!(!rig.log_has("dev:erase"), "nothing was erased");
}

#[test]
fn a_full_backup_inside_the_repository_is_refused() {
    for evidence in [
        BackupEvidence {
            verified: true,
            owner_only: false,
            inside_repository: false,
        },
        BackupEvidence {
            verified: true,
            owner_only: true,
            inside_repository: true,
        },
        BackupEvidence {
            verified: false,
            owner_only: true,
            inside_repository: false,
        },
    ] {
        let mut rig = Rig::new();
        rig.full_backup = Some(Box::new(FullBackups {
            evidence,
            calls: Vec::new(),
            keys: Vec::new(),
            fail: false,
        }));
        let (_, result) = rig.run(&official_padded());
        let rules = refused_rules(&result);
        assert!(!rules.is_empty(), "{evidence:?} is refused");
        assert!(!rig.log_has("dev:write"), "{evidence:?} wrote nothing");
    }
}

#[test]
fn a_console_read_is_confirmed_by_a_person_for_one_port() {
    use pemu_planner::flow::{ConfirmPrompt, Intent, confirm_console_read};

    let log: Log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut person = Scripted {
        log: log.clone(),
        answer: None,
        prompts: Vec::new(),
    };
    let confirmed = confirm_console_read(
        "/dev/cu.usbmodem1101",
        false,
        &mut ElicitationConfirmer::new(&mut person),
    )
    .expect("the person typed the digest");
    assert_eq!(confirmed.port(), "/dev/cu.usbmodem1101");
    assert!(
        !confirmed.reset_confirmed(),
        "a yes to a plain read is not a yes to a restart"
    );

    let shown = person.prompts[0].clone();
    assert!(shown.contains("read-only"), "{shown}");
    assert!(
        shown.contains("Nothing is written, erased or reset"),
        "{shown}"
    );
    assert!(!shown.contains("/dev/"), "{shown}");

    let other = ConfirmPrompt::console_read("/dev/cu.usbmodem2202");
    let mine = ConfirmPrompt::console_read("/dev/cu.usbmodem1101");
    assert_eq!(mine.intent, Intent::ReadConsole);
    assert_ne!(mine.plan_sha256, other.plan_sha256);
    assert_eq!(hex(confirmed.plan_sha256()), mine.plan_sha256);
    let image = official_unpadded();
    let request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
    let plan = plan_flash(&request, None);
    assert_ne!(mine.plan_sha256, plan.plan.plan_sha256_hex());

    for (answer, expected) in [
        (
            Elicited::Typed("f".repeat(64)),
            FlowError::Refused(vec![Refusal::new(
                Rule::ConfirmationMismatch,
                "the confirmation named a different port or a different operation",
            )]),
        ),
        (Elicited::Declined, FlowError::Declined),
        (
            Elicited::Unavailable("no elicitation".to_owned()),
            FlowError::ConfirmationUnavailable("no elicitation".to_owned()),
        ),
    ] {
        let mut refuser = Scripted {
            log: log.clone(),
            answer: Some(answer),
            prompts: Vec::new(),
        };
        let error = confirm_console_read(
            "/dev/cu.usbmodem1101",
            false,
            &mut ElicitationConfirmer::new(&mut refuser),
        )
        .expect_err("no token");
        assert_eq!(error, expected);
    }
}

/// The plain read's text says "Nothing is written, erased or reset", so a restart needs its own
/// intent, words and digest domain.
#[test]
fn a_reset_is_confirmed_in_its_own_words_and_under_its_own_digest() {
    use pemu_planner::flow::{ConfirmPrompt, Intent, confirm_console_read};

    let log: Log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut person = Scripted {
        log: log.clone(),
        answer: None,
        prompts: Vec::new(),
    };
    let confirmed = confirm_console_read(
        "/dev/cu.usbmodem1101",
        true,
        &mut ElicitationConfirmer::new(&mut person),
    )
    .expect("the person typed the digest");
    assert!(
        confirmed.reset_confirmed(),
        "the restart authority travels in the token"
    );

    let shown = person.prompts[0].clone();
    assert!(
        shown.contains("Restart the connected AI Passport"),
        "{shown}"
    );
    assert!(shown.contains("The device will be restarted"), "{shown}");
    assert!(
        !shown.contains("Nothing is written, erased or reset"),
        "the plain read's promise must not be repeated over a reset: {shown}"
    );
    assert!(!shown.contains("/dev/"), "{shown}");

    // Domain separation: for the same port the two digests differ.
    let plain = ConfirmPrompt::console_read("/dev/cu.usbmodem1101");
    let restart = ConfirmPrompt::console_reset("/dev/cu.usbmodem1101");
    assert_eq!(restart.intent, Intent::ResetAndReadConsole);
    assert!(restart.intent.resets() && !plain.intent.resets());
    assert_ne!(
        plain.plan_sha256, restart.plan_sha256,
        "the same port must not yield the same digest for both intents"
    );
    assert_eq!(hex(confirmed.plan_sha256()), restart.plan_sha256);
    assert_ne!(
        restart.plan_sha256,
        ConfirmPrompt::console_reset("/dev/cu.usbmodem2202").plan_sha256
    );
    assert_ne!(restart.title(), plain.title());
    assert_ne!(restart.verb(), plain.verb());

    let mut replayer = Scripted {
        log,
        answer: Some(Elicited::Typed(plain.plan_sha256.clone())),
        prompts: Vec::new(),
    };
    let error = confirm_console_read(
        "/dev/cu.usbmodem1101",
        true,
        &mut ElicitationConfirmer::new(&mut replayer),
    )
    .expect_err("a read confirmation is not a reset confirmation");
    assert_eq!(
        error,
        FlowError::Refused(vec![Refusal::new(
            Rule::ConfirmationMismatch,
            "the confirmation named a different port or a different operation",
        )])
    );
}

// The owner's standing grant removes the person and nothing else.

#[test]
fn the_standing_grant_flashes_with_no_one_asked_and_leaves_cardid_alone() {
    let mut rig = Rig::new();
    let image = official_padded();
    let cardid_before = rig.device().cardid();
    let (report, result) = rig.run_granted(&image);
    assert_eq!(result, Ok(()));
    // No prompt reached the scripted person, yet the flow still records a Confirm step.
    assert!(rig.confirmer.prompts.is_empty());
    assert!(!rig.log_has("confirm"));
    assert!(report.device.step_order().contains(&Step::Confirm));
    assert!(report.device.steps.iter().all(|(_, ok)| *ok));
    assert_eq!(
        rig.device().cardid(),
        cardid_before,
        "cardid is bit-identical"
    );
    assert_eq!(report.device.cardid_unchanged, Some(true));
}

#[test]
fn the_standing_grant_answers_away_no_machine_rule() {
    // A plan the planner refuses is refused before any confirmer is reached, granted or not.
    let mut rig = Rig::new();
    let mut image = official_padded();
    image[0x35_7000] = 0;
    let (_, result) = rig.run_granted(&image);
    assert!(refused_rules(&result).contains(&Rule::DataDropped));
    assert_eq!(rig.opener.opened, 0);

    // A device that is not the Passport is refused at Identify and written to not at all.
    let mut rig = Rig::new();
    rig.opener
        .device
        .as_mut()
        .expect("device")
        .identity
        .flash_device = 0x4016;
    let (_, result) = rig.run_granted(&official_unpadded());
    assert!(
        refused_rules(&result).contains(&Rule::FlashId),
        "{result:?}"
    );
    assert!(!rig.log_has("dev:write"));

    // A table that moves cardid is refused the same way.
    let mut rig = Rig::new();
    let mut layout = official_layout();
    layout[3].offset = 0x35_8000;
    let table = encode_partition_table(&layout);
    rig.opener.device.as_mut().expect("device").flash[0x8000..0x8C00].copy_from_slice(&table);
    let (_, result) = rig.run_granted(&official_unpadded());
    assert!(
        refused_rules(&result).contains(&Rule::CardidMoved),
        "{result:?}"
    );
    assert!(!rig.log_has("dev:write"));
}

/// The ROM prints its banner within milliseconds of a reset, so the restart has to come from the
/// reader's own open port.
#[test]
fn a_flash_token_authorises_the_boot_check_restart_and_its_prompt_says_so() {
    let mut rig = Rig::new();
    let (_, result) = rig.run(&official_padded());
    assert_eq!(result, Ok(()));
    assert!(rig.log_has("token reset=true"), "{:?}", rig.log.borrow());
    let prompt = rig.confirmer.prompts.first().expect("the person was asked");
    assert!(
        prompt.contains("restarted and its boot console read"),
        "the prompt names the restart it authorises: {prompt}"
    );
}
