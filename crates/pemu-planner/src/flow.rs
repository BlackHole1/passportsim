//! The device flash flow in its fixed order (the ten [`Step`]s: Discover, Plan, Rehearse, Confirm,
//! Identify, Guard, Back up, Write, Verify, Boot check). Every host effect is behind a trait, so
//! the order and every refusal are tested with fakes and no port.
//!
//! Nothing opens the device before a confirmation: [`SessionOpener::open`] takes a [`Confirmed`],
//! which only [`flash_device`] and [`confirm_console_read`] can make, after an
//! [`Answer::Accepted`] naming the digest being executed.

use pemu_loader::partitions::PartitionTable;
use pemu_loader::{hex, sha256};

use crate::plan::{Plan, PlanRequest, PlannedWrite, plan_flash};
use crate::rules::{
    BackupEvidence, CARDID_OFFSET, CARDID_SIZE, ChipRevision, DeviceFacts, LEFTOVER_END,
    LEFTOVER_START, Refusal, Rule, TABLE_LEN, USB_PID, USB_VID, check_backup, check_identity,
    round_to_sectors, touches_cardid,
};

/// One serial device found by OS enumeration. Carries nothing that identifies a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortCandidate {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
}

/// OS serial enumeration (IOKit on macOS, SetupAPI on Windows). Must not open any port.
pub trait Discovery {
    fn enumerate(&mut self) -> Result<Vec<PortCandidate>, String>;
}

/// The host's device-path predicate: outside feature `device` no code names a host serial device.
pub trait DevicePaths {
    /// Whether `path` is a port the planner may flash through: a serial device, and on macOS a
    /// call-out device (the `tty` twin is refused).
    fn is_flash_port(&self, path: &str) -> bool;
    fn case_insensitive(&self) -> bool;
    /// The path an open of an accepted `port` goes through: the port itself on macOS, the
    /// `\\.\` namespace form on Windows, because `COM10` and above are not reserved names and only
    /// the namespace form says "device" for every n.
    fn open_path(&self, port: &str) -> String {
        port.to_owned()
    }
}

/// Step 1: picks the port among the 303A:1001 candidates. Matching is on the identifiers, never on
/// a name; more than one needs `--port`, and `--port` must equal a discovered port.
pub fn select_port(
    candidates: &[PortCandidate],
    requested: Option<&str>,
    paths: &dyn DevicePaths,
) -> Result<String, Refusal> {
    let matching: Vec<&PortCandidate> = candidates
        .iter()
        .filter(|c| c.vid == USB_VID && c.pid == USB_PID && paths.is_flash_port(&c.path))
        .collect();
    let same = |a: &str, b: &str| {
        if paths.case_insensitive() {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    match requested {
        Some(port) => matching
            .iter()
            .find(|c| same(&c.path, port))
            .map(|c| c.path.clone())
            .ok_or_else(|| {
                Refusal::new(
                    Rule::PortNotAllowed,
                    "`--port` is not a discovered 303A:1001 flashing port",
                )
            }),
        None => match matching.as_slice() {
            [] => Err(Refusal::new(
                Rule::NoDevice,
                "no 303A:1001 device found; power it on by holding the power button 0.5 s",
            )),
            [one] => Ok(one.path.clone()),
            _ => Err(Refusal::new(
                Rule::AmbiguousPort,
                format!("{} devices found; name one with `--port`", matching.len()),
            )),
        },
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    Flash,
    ReadConsole,
    /// As [`Intent::ReadConsole`], after restarting the device by a reset-line pulse
    /// (`device_boot_check --reset`). Rebooting is a different thing to say yes to, so its digest
    /// is domain-separated in [`console_read_digest`].
    ResetAndReadConsole,
}

impl Intent {
    pub fn resets(self) -> bool {
        matches!(self, Intent::ResetAndReadConsole)
    }
}

/// What a person is asked to confirm. It names the digest and every write, and never a host port
/// path, a device identity value or the confirmation code: the same text goes to an MCP client and
/// a dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmPrompt {
    pub intent: Intent,
    /// Lower-case hex of the digest the person confirms: the plan digest for a flash, and
    /// SHA-256 over the intent and the port for a console read.
    pub plan_sha256: String,
    pub writes: Vec<(String, u32, usize)>,
    pub erase_nvs: Option<(u32, u32)>,
    /// Whether a write or the `nvs` erase lands in [0x310000, 0x356000), right below cardid.
    pub writes_leftover: bool,
    /// Whether the whole 8 MB part, cardid and `nvs` included, is backed up first (`--backup`).
    pub full_backup: bool,
}

impl ConfirmPrompt {
    pub fn of(plan: &Plan, full_backup: bool) -> ConfirmPrompt {
        ConfirmPrompt {
            full_backup,
            intent: Intent::Flash,
            plan_sha256: plan.plan_sha256_hex(),
            writes: plan
                .writes
                .iter()
                .map(|w| (w.name.clone(), w.offset, w.data.len()))
                .collect(),
            erase_nvs: plan.erase_nvs,
            writes_leftover: plan
                .writes
                .iter()
                .map(PlannedWrite::sectors)
                .chain(plan.erase_nvs.map(|(offset, size)| {
                    round_to_sectors(u64::from(offset), u64::from(offset) + u64::from(size))
                }))
                .any(|(s, e)| s < u64::from(LEFTOVER_END) && e > u64::from(LEFTOVER_START)),
        }
    }

    /// The prompt for a read-only open of `port`. The digest covers the port and the intent; the
    /// text names no host path.
    pub fn console_read(port: &str) -> ConfirmPrompt {
        ConfirmPrompt::console(port, Intent::ReadConsole)
    }

    /// The prompt for a read-only open of `port` that restarts the device first.
    pub fn console_reset(port: &str) -> ConfirmPrompt {
        ConfirmPrompt::console(port, Intent::ResetAndReadConsole)
    }

    fn console(port: &str, intent: Intent) -> ConfirmPrompt {
        ConfirmPrompt {
            intent,
            plan_sha256: hex(&console_read_digest(port, intent)),
            writes: Vec::new(),
            erase_nvs: None,
            writes_leftover: false,
            full_backup: false,
        }
    }

    pub fn title(&self) -> &'static str {
        match self.intent {
            Intent::Flash => "Flash the AI Passport",
            Intent::ReadConsole => "Read the AI Passport's boot console",
            Intent::ResetAndReadConsole => "Restart the AI Passport and read its boot console",
        }
    }

    pub fn verb(&self) -> &'static str {
        match self.intent {
            Intent::Flash => "flash",
            Intent::ReadConsole => "read the console",
            Intent::ResetAndReadConsole => "restart it and read the console",
        }
    }

    pub fn render(&self) -> String {
        if self.intent == Intent::ReadConsole {
            return format!(
                "Open the connected AI Passport read-only and read its boot console for up to 10 \
                 s, under confirmation {}.\nNothing is written, erased or reset, and the console \
                 is redacted to the ROM banner and the app's ELF digest before anything sees \
                 it.\n",
                self.plan_sha256
            );
        }
        if self.intent == Intent::ResetAndReadConsole {
            return format!(
                "Restart the connected AI Passport and read the boot console the restart \
                 produces, for up to 10 s, under confirmation {}.\nThe device will be restarted: \
                 whatever it is running stops at once and it boots again from flash. Nothing is \
                 written or erased, no byte is sent to the device, and the console is redacted to \
                 the ROM banner and the app's ELF digest before anything sees it.\n",
                self.plan_sha256
            );
        }
        let mut text = format!(
            "Flash the connected AI Passport with plan {}:\n",
            self.plan_sha256
        );
        for (name, offset, len) in &self.writes {
            text.push_str(&format!("  write {name} at {offset:#x}, {len} bytes\n"));
        }
        if let Some((offset, size)) = self.erase_nvs {
            text.push_str(&format!("  erase nvs at {offset:#x}, {size:#x} bytes\n"));
        }
        if self.writes_leftover {
            text.push_str(
                "note: a write or erase lands in [0x310000, 0x356000), the unused flash right below cardid; the official layout keeps nothing there.\n",
            );
        }
        text.push_str(
            "cardid [0x356000, 0x35A000) is never written; a backup of every sector written is taken first.\n",
        );
        text.push_str(
            "after the write the device is restarted and its boot console read for up to 10 s.\n",
        );
        if self.full_backup {
            text.push_str(
                "backup: the whole 8 MB part, cardid and nvs included, is read twice and stored owner-only outside the repository before anything is written.\n",
            );
        }
        text
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Accepted {
        plan_sha256: String,
    },
    Declined,
    /// This confirmation path cannot run here, with a hint.
    Unavailable(String),
}

mod sealed {
    pub trait Sealed {}
}

/// A confirmation path. Sealed: the only implementations are [`ElicitationConfirmer`] (MCP
/// elicitation), [`DialogConfirmer`] (a native dialog), [`TerminalConfirmer`] (a one-time code) and
/// [`StandingGrantConfirmer`] (the owner's standing grant). A host plugs in only the transport; the
/// planner renders the prompt and judges the answer, so a host cannot skip the prompt or accept a
/// different plan. No AI tool call may answer a confirmation.
pub trait Confirmer: sealed::Sealed {
    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Answer;
}

/// The digest a person confirms before a console open: SHA-256 over a per-[`Intent`] domain tag and
/// the port, so it never equals a plan digest, never covers another port, and a plain read's yes
/// cannot be replayed for a restart.
///
/// # Panics
///
/// Panics on [`Intent::Flash`], which has a plan digest of its own and never comes here.
fn console_read_digest(port: &str, intent: Intent) -> [u8; 32] {
    let tag: &[u8] = match intent {
        Intent::ReadConsole => b"passport-emu device boot console read-only\0",
        Intent::ResetAndReadConsole => b"passport-emu device boot console reset and read\0",
        Intent::Flash => unreachable!("a flash is confirmed by its plan digest"),
    };
    let mut message = tag.to_vec();
    message.extend_from_slice(port.as_bytes());
    sha256(&message)
}

/// Proof that a person accepted an operation on a port. Only [`flash_device`] and
/// [`confirm_console_read`] make one; its `Debug` omits the port path.
#[derive(Clone)]
pub struct Confirmed {
    port: String,
    plan_sha256: [u8; 32],
    /// Whether the yes covers a restart, so a plain-read token can never pulse a reset line.
    reset: bool,
}

impl core::fmt::Debug for Confirmed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Confirmed")
            .field("plan_sha256", &hex(&self.plan_sha256))
            .finish_non_exhaustive()
    }
}

impl Confirmed {
    /// A token for this crate's unit tests only; no other crate or binary can forge one.
    #[cfg(test)]
    pub(crate) fn for_crate_tests(port: &str, plan_sha256: [u8; 32]) -> Confirmed {
        Confirmed {
            port: port.to_owned(),
            plan_sha256,
            reset: false,
        }
    }

    #[cfg(all(test, feature = "device"))]
    pub(crate) fn for_crate_tests_with_reset(port: &str, plan_sha256: [u8; 32]) -> Confirmed {
        Confirmed {
            reset: true,
            ..Confirmed::for_crate_tests(port, plan_sha256)
        }
    }

    pub fn port(&self) -> &str {
        &self.port
    }

    pub fn plan_sha256(&self) -> &[u8; 32] {
        &self.plan_sha256
    }

    /// Whether the person confirmed a restart, not only a read: `true` for a flash (its prompt
    /// names the boot-check restart) and for a console read with `reset`. The one authority
    /// `exec::PortConsole` accepts for pulsing the reset line.
    pub fn reset_confirmed(&self) -> bool {
        self.reset
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Elicited {
    Typed(String),
    Declined,
    Unavailable(String),
}

/// The MCP elicitation transport. It returns what the person typed and never sees the expected
/// digest; [`ElicitationConfirmer`] compares.
pub trait ElicitationPort {
    fn elicit(&mut self, message: &str) -> Elicited;
}

pub struct ElicitationConfirmer<'a> {
    port: &'a mut dyn ElicitationPort,
}

impl<'a> ElicitationConfirmer<'a> {
    pub fn new(port: &'a mut dyn ElicitationPort) -> ElicitationConfirmer<'a> {
        ElicitationConfirmer { port }
    }
}

impl sealed::Sealed for ElicitationConfirmer<'_> {}

impl Confirmer for ElicitationConfirmer<'_> {
    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Answer {
        let message = format!(
            "{}Type the digest above to {}; leave it empty to cancel.\n",
            prompt.render(),
            prompt.verb()
        );
        match self.port.elicit(&message) {
            Elicited::Typed(text) if text.trim().is_empty() => Answer::Declined,
            Elicited::Typed(text) => Answer::Accepted {
                plan_sha256: text.trim().to_ascii_lowercase(),
            },
            Elicited::Declined => Answer::Declined,
            Elicited::Unavailable(hint) => Answer::Unavailable(hint),
        }
    }
}

/// The native dialog transport (`pemu_host::platform::Dialog`).
pub trait DialogPort {
    fn availability(&self) -> Result<(), String>;
    fn ask(&self, title: &str, body: &str) -> Result<bool, String>;
}

/// Confirmation by native dialog. An unavailable desktop or a dialog error is
/// [`Answer::Unavailable`], never a yes.
pub struct DialogConfirmer<'a> {
    port: &'a dyn DialogPort,
}

impl<'a> DialogConfirmer<'a> {
    pub fn new(port: &'a dyn DialogPort) -> DialogConfirmer<'a> {
        DialogConfirmer { port }
    }
}

impl sealed::Sealed for DialogConfirmer<'_> {}

impl Confirmer for DialogConfirmer<'_> {
    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Answer {
        if let Err(why) = self.port.availability() {
            return Answer::Unavailable(why);
        }
        match self.port.ask(prompt.title(), &prompt.render()) {
            Ok(true) => Answer::Accepted {
                plan_sha256: prompt.plan_sha256.clone(),
            },
            Ok(false) => Answer::Declined,
            Err(why) => Answer::Unavailable(format!("the dialog failed: {why}")),
        }
    }
}

/// The owner's standing grant, answered by no one: accepts every prompt with that prompt's own
/// digest.
///
/// Confirmation is a human gate, not a security boundary, and the owner cannot judge a partition
/// layout or a digest anyway. Every machine-checkable rule still runs (identity guard, cardid
/// refusal, rehearsal, backup, verify, boot check). Constructed only in `pemu_host::device` for
/// the flash and the boot-console read; secret-revealing confirmations keep the other paths.
#[derive(Debug, Default)]
pub struct StandingGrantConfirmer;

impl StandingGrantConfirmer {
    pub fn new() -> StandingGrantConfirmer {
        StandingGrantConfirmer
    }
}

impl sealed::Sealed for StandingGrantConfirmer {}

impl Confirmer for StandingGrantConfirmer {
    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Answer {
        // What is granted is the yes, never which plan it is a yes to.
        Answer::Accepted {
            plan_sha256: prompt.plan_sha256.clone(),
        }
    }
}

/// The controlling terminal a one-time code is shown on and read from; the code goes nowhere else.
pub trait Terminal {
    fn show(&mut self, text: &str);
    fn read_line(&mut self) -> Option<String>;
}

/// Letters and digits a person does not confuse (no 0/O, 1/I/L).
const CODE_ALPHABET: &[u8; 31] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// The largest multiple of 31 that fits a byte: bytes at or above it are rejected so every code
/// letter is equally likely.
const CODE_BYTE_LIMIT: u8 = 248;

/// An 8-letter one-time confirmation code drawn from host entropy `next_byte` (a core crate has
/// none). Rejection sampling below 248 keeps the 31 letters uniform.
pub fn terminal_code(next_byte: &mut dyn FnMut() -> u8) -> String {
    let mut code = String::with_capacity(8);
    while code.len() < 8 {
        let b = next_byte();
        if b < CODE_BYTE_LIMIT {
            code.push(CODE_ALPHABET[usize::from(b % 31)] as char);
        }
    }
    code
}

/// Shows the prompt and a one-time code on the terminal, and accepts only that code typed back.
/// The code is never returned, logged or put in any output.
pub struct TerminalConfirmer<'a> {
    terminal: &'a mut dyn Terminal,
    entropy: &'a mut dyn FnMut() -> u8,
}

impl<'a> TerminalConfirmer<'a> {
    pub fn new(
        terminal: &'a mut dyn Terminal,
        entropy: &'a mut dyn FnMut() -> u8,
    ) -> TerminalConfirmer<'a> {
        TerminalConfirmer { terminal, entropy }
    }
}

impl sealed::Sealed for TerminalConfirmer<'_> {}

impl Confirmer for TerminalConfirmer<'_> {
    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Answer {
        let code = terminal_code(self.entropy);
        self.terminal.show(&prompt.render());
        self.terminal.show(&format!(
            "Type {code} and press Return to {}, anything else cancels: ",
            prompt.verb()
        ));
        match self.terminal.read_line() {
            Some(line) if line.trim() == code => Answer::Accepted {
                plan_sha256: prompt.plan_sha256.clone(),
            },
            _ => Answer::Declined,
        }
    }
}

/// What a device (or the emulator standing in for it) reports about itself. The MAC is never
/// kept, only a salted SHA-256 prefix that keys the backup directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub chip: String,
    pub revision: ChipRevision,
    pub flash_manufacturer: u8,
    pub flash_device: u16,
    /// The first 16 bytes of SHA-256(salt, MAC), when the session had a salt and saw a MAC.
    pub device_key: Option<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// The port is held by another process, such as Passport Keys.
    Busy,
    Unsupported(String),
    Failed(String),
}

/// The flasher operations the flow needs. There is deliberately no whole-chip erase.
pub trait DeviceSession {
    fn identify(&mut self) -> Result<Identity, SessionError>;
    /// The 0xC00 bytes of the partition table at 0x8000 (read-only; never cardid or nvs).
    fn read_partition_table(&mut self) -> Result<Vec<u8>, SessionError>;
    fn region_md5(&mut self, offset: u32, size: u32) -> Result<[u8; 16], SessionError>;
    /// Reads a region for the backup; the flow only asks for sectors it will write.
    fn read_region(&mut self, offset: u32, size: u32) -> Result<Vec<u8>, SessionError>;
    /// The whole 8 MB part, for `--backup`: the one read that covers cardid and `nvs`. Its bytes
    /// are never printed, logged or put in a receipt. A session that cannot serve it says so, and
    /// the backup is refused rather than skipped.
    fn read_full_flash(&mut self) -> Result<Vec<u8>, SessionError> {
        Err(SessionError::Unsupported(
            "this session takes no full-part backup".to_owned(),
        ))
    }
    /// Writes exactly these segments with the flash size kept, never erasing all (step 8).
    fn write(&mut self, writes: &[PlannedWrite]) -> Result<(), SessionError>;
    /// Erases one region; the flow calls it only for a person's `--erase-nvs`.
    fn erase_region(&mut self, offset: u32, size: u32) -> Result<(), SessionError>;
    /// Device-side MD5 verification of every written segment (step 9).
    fn verify(&mut self, writes: &[PlannedWrite]) -> Result<(), SessionError>;
    fn hard_reset(&mut self) -> Result<(), SessionError>;
    /// The boot console after the reset, redacted by the host (step 10).
    fn boot_log(&mut self) -> Result<String, SessionError>;
}

/// Opens the real-device session. Only a [`Confirmed`] opens it.
pub trait SessionOpener {
    fn open(&mut self, confirmed: &Confirmed) -> Result<Box<dyn DeviceSession + '_>, SessionError>;
}

/// Stores a backup owner-only under the data root and reports what it found.
pub trait BackupStore {
    /// Stores regions in a directory unique to this run, reads them back, and reports. Never
    /// truncates or replaces an earlier backup.
    fn store(
        &mut self,
        plan_sha256: &[u8; 32],
        device_key: Option<&[u8; 16]>,
        regions: &[(u32, Vec<u8>)],
    ) -> Result<BackupEvidence, String>;
}

/// What a full backup produced: booleans and a length, never a path, a stem or a digest.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FullBackupFacts {
    pub evidence: BackupEvidence,
    pub bytes: u64,
}

/// The optional full-part backup (`--backup`, CLI only). Separate from [`BackupStore`] because it
/// reads what the plan never touches (cardid and `nvs`); its evidence goes through the same
/// [`check_backup`] rules.
pub trait FullBackup {
    /// Reads the whole part twice and stores it owner-only outside the repository. The bytes never
    /// reach the caller.
    fn store_full(
        &mut self,
        session: &mut dyn DeviceSession,
        plan_sha256: &[u8; 32],
        device_key: Option<&[u8; 16]>,
    ) -> Result<FullBackupFacts, String>;
}

/// What a person does after `E_CARDID_CHANGED`. No tool writes cardid back: recovery is human-only.
pub const CARDID_CHANGED_INSTRUCTIONS: &str = "The cardid partition [0x356000, 0x35A000) changed during the flash. Stop: do not flash again and do not unplug the Passport. No tool will write cardid back. Keep the backup directory of this run and the flow report, and contact the Passport vendor to restore the card identity; recovery is a human-only step.";

/// What a person does when the write or the verify failed on the device. It rests on two facts:
/// `apply` always resets once the guard digest exists, and the backup was checked before the first
/// write.
pub const WRITE_FAILED_INSTRUCTIONS: &str = "The flash did not finish, so the app partition may hold part of the new firmware. Two things are already true: the Passport was reset back to the app it has, so unplugging it is not urgent, and the backup of this run was taken and checked before anything was written. It is kept under the data root's `device/backups` directory, in the run directory of this plan; do not delete it. Run the same flash again with the same image, and if it fails the same way, keep that directory and the flow report.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowError {
    Refused(Vec<Refusal>),
    Declined,
    ConfirmationUnavailable(String),
    DeviceBusy,
    /// The cardid MD5 differs after the write. Recovery is human-only; the payload is the
    /// instruction text shown to the person.
    CardidChanged(&'static str),
    CheckFailed(String),
    Session(String),
}

impl FlowError {
    pub fn code(&self) -> &'static str {
        match self {
            FlowError::Refused(_) | FlowError::Declined => "E_PLAN_REFUSED",
            FlowError::ConfirmationUnavailable(_) => "E_PLAN_REFUSED",
            FlowError::DeviceBusy => "E_DEVICE_BUSY",
            FlowError::CardidChanged(_) => "E_CARDID_CHANGED",
            FlowError::CheckFailed(_) | FlowError::Session(_) => "E_PLAN_REFUSED",
        }
    }
}

impl From<SessionError> for FlowError {
    fn from(e: SessionError) -> FlowError {
        match e {
            SessionError::Busy => FlowError::DeviceBusy,
            SessionError::Unsupported(why) | SessionError::Failed(why) => FlowError::Session(why),
        }
    }
}

/// The ten steps of a device flash, in order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Discover,
    Plan,
    Rehearse,
    Confirm,
    /// Read-only identity; the first open of the port.
    Identify,
    /// cardid MD5 before.
    Guard,
    /// Backup of every sector to be written.
    Backup,
    Write,
    /// Verify and cardid MD5 after.
    Verify,
    BootCheck,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepLog {
    pub steps: Vec<(Step, bool)>,
    /// Whether the cardid MD5 was equal before and after; `None` when the guard never ran or the
    /// second digest could not be taken.
    pub cardid_unchanged: Option<bool>,
    pub boot: Option<BootCheck>,
    /// The whole-part backup, once stored; `None` without `--backup` or when it failed.
    pub full_backup: Option<FullBackupFacts>,
    /// What the host found out about the per-sector backup, once stored; `None` before step 7 or
    /// when storing it failed.
    pub backup: Option<crate::rules::BackupEvidence>,
}

impl StepLog {
    fn record(&mut self, step: Step, ok: bool) {
        self.steps.push((step, ok));
    }

    pub fn step_order(&self) -> Vec<Step> {
        self.steps.iter().map(|(s, _)| *s).collect()
    }
}

/// What the flow did: step outcomes, the plan digest and booleans only, never a device value, boot
/// log text or confirmation code. Rehearsal and device run are kept apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlowReport {
    pub plan_sha256: Option<String>,
    pub device: StepLog,
    pub rehearsal: StepLog,
}

/// Confirms a read-only console open on its own. Even a read-only open claims a port another
/// program may be using, so it asks the same confirmation paths a flash does. `reset` asks for
/// [`Intent::ResetAndReadConsole`] instead.
///
/// # Errors
///
/// [`FlowError::Declined`] when the person said no, [`FlowError::ConfirmationUnavailable`] when no
/// path could ask, and [`FlowError::Refused`] with [`Rule::ConfirmationMismatch`] when the answer
/// named something else.
pub fn confirm_console_read(
    port: &str,
    reset: bool,
    confirmer: &mut dyn Confirmer,
) -> Result<Confirmed, FlowError> {
    let intent = if reset {
        Intent::ResetAndReadConsole
    } else {
        Intent::ReadConsole
    };
    let prompt = ConfirmPrompt::console(port, intent);
    match confirmer.confirm(&prompt) {
        Answer::Accepted { plan_sha256 } if plan_sha256 == prompt.plan_sha256 => Ok(Confirmed {
            port: port.to_owned(),
            plan_sha256: console_read_digest(port, intent),
            reset,
        }),
        Answer::Accepted { .. } => Err(FlowError::Refused(vec![Refusal::new(
            Rule::ConfirmationMismatch,
            "the confirmation named a different port or a different operation",
        )])),
        Answer::Declined => Err(FlowError::Declined),
        Answer::Unavailable(hint) => Err(FlowError::ConfirmationUnavailable(hint)),
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BootCheck {
    pub banner: bool,
    pub elf_sha256_matches: bool,
}

impl BootCheck {
    pub fn passed(&self) -> bool {
        self.banner && self.elf_sha256_matches
    }
}

/// Judges a boot console: a ROM banner, and an `ELF file SHA256:` line whose hex prefix (9 digits
/// with `CONFIG_APP_RETRIEVE_LEN_ELF_SHA=9`) starts the expected digest.
pub fn boot_check(log: &str, expected_elf_sha256: Option<&[u8; 32]>) -> BootCheck {
    let banner = log.lines().any(crate::console::is_rom_banner);
    let expected = expected_elf_sha256.map(|d| hex(d));
    let elf_sha256_matches = expected.is_some_and(|expected| {
        log.lines().any(|line| {
            let Some((_, rest)) = line.split_once("ELF file SHA256:") else {
                return false;
            };
            let prefix: String = rest
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect::<String>()
                .to_ascii_lowercase();
            prefix.len() >= 8 && expected.starts_with(&prefix)
        })
    });
    BootCheck {
        banner,
        elf_sha256_matches,
    }
}

/// Step 5: reads the device table and judges identity plus the image-side cardid rule.
fn identify(
    session: &mut dyn DeviceSession,
    plan_request: &PlanRequest<'_>,
    plan: &Plan,
) -> Result<Option<[u8; 16]>, FlowError> {
    let identity = session.identify()?;
    let device_key = identity.device_key;
    let table_bytes = session.read_partition_table()?;
    let partitions =
        match PartitionTable::parse(&table_bytes[..table_bytes.len().min(TABLE_LEN as usize)]) {
            Ok(table) => table.entries,
            Err(_) => Vec::new(),
        };
    let facts = DeviceFacts {
        chip: identity.chip,
        revision: identity.revision,
        flash_manufacturer: identity.flash_manufacturer,
        flash_device: identity.flash_device,
        partitions,
    };
    let mut refusals = check_identity(&facts);
    let with_device = plan_flash(plan_request, Some(&facts));
    refusals.extend(with_device.refusals);
    // The device's facts decide ranges such as the `nvs` erase; what runs must still be exactly
    // the plan the person confirmed.
    if refusals.is_empty() && with_device.plan.plan_sha256 != plan.plan_sha256 {
        refusals.push(Refusal::new(
            Rule::ConfirmationMismatch,
            "with the device's partition table the plan differs from the one confirmed",
        ));
    }
    refusals.sort_by(|a, b| (a.rule, &a.detail).cmp(&(b.rule, &b.detail)));
    refusals.dedup();
    if refusals.is_empty() {
        Ok(device_key)
    } else {
        Err(FlowError::Refused(refusals))
    }
}

/// Steps 6 to 10 on an open session. `backups` is `None` only for the rehearsal.
///
/// Once the guard digest exists, the second cardid digest is always attempted and the device is
/// always reset, so a changed cardid is reported even when the write itself failed.
fn apply(
    session: &mut dyn DeviceSession,
    plan: &Plan,
    backups: Option<Backups<'_>>,
    log: &mut StepLog,
) -> Result<(), FlowError> {
    let before = match session.region_md5(CARDID_OFFSET, CARDID_SIZE) {
        Ok(digest) => {
            log.record(Step::Guard, true);
            digest
        }
        Err(e) => {
            log.record(Step::Guard, false);
            let _ = session.hard_reset();
            return Err(e.into());
        }
    };
    let written = backup_write_verify(session, plan, backups, log);
    let unchanged = session
        .region_md5(CARDID_OFFSET, CARDID_SIZE)
        .ok()
        .map(|after| after == before);
    log.cardid_unchanged = unchanged;
    let reset = session.hard_reset();
    if unchanged == Some(false) {
        return Err(FlowError::CardidChanged(CARDID_CHANGED_INSTRUCTIONS));
    }
    written?;
    if unchanged.is_none() {
        return Err(FlowError::Session(
            "the cardid digest after the write could not be taken".to_owned(),
        ));
    }
    reset?;
    let boot = match session.boot_log() {
        Ok(text) => boot_check(&text, plan.app_elf_sha256.as_ref()),
        Err(e) => {
            log.record(Step::BootCheck, false);
            return Err(e.into());
        }
    };
    log.boot = Some(boot);
    log.record(Step::BootCheck, boot.passed());
    if boot.passed() {
        Ok(())
    } else {
        Err(FlowError::CheckFailed(format!(
            "boot check: banner {}, ELF SHA-256 prefix {}",
            boot.banner, boot.elf_sha256_matches
        )))
    }
}

struct Backups<'a> {
    store: &'a mut dyn BackupStore,
    full: Option<&'a mut dyn FullBackup>,
    device_key: Option<[u8; 16]>,
}

fn backup_write_verify(
    session: &mut dyn DeviceSession,
    plan: &Plan,
    backups: Option<Backups<'_>>,
    log: &mut StepLog,
) -> Result<(), FlowError> {
    let backed_up = backups.is_some();
    if let Some(Backups {
        store,
        full,
        device_key,
    }) = backups
    {
        // The whole-part backup is the only recovery source for cardid and `nvs`.
        if let Some(full) = full {
            let facts = match full.store_full(session, &plan.plan_sha256, device_key.as_ref()) {
                Ok(facts) => facts,
                Err(why) => {
                    log.record(Step::Backup, false);
                    return Err(FlowError::Session(why));
                }
            };
            let refusals = check_backup(Some(&facts.evidence));
            if !refusals.is_empty() {
                log.record(Step::Backup, false);
                return Err(FlowError::Refused(refusals));
            }
            // Recorded only once the rules passed, so the report never shows an untrusted backup.
            log.full_backup = Some(facts);
        }
        let mut ranges: Vec<(u64, u64)> = plan.writes.iter().map(PlannedWrite::sectors).collect();
        if let Some((offset, size)) = plan.erase_nvs {
            ranges.push(round_to_sectors(
                u64::from(offset),
                u64::from(offset) + u64::from(size),
            ));
        }
        let mut regions = Vec::new();
        for (s, e) in ranges {
            if touches_cardid(s, e) {
                log.record(Step::Backup, false);
                return Err(FlowError::Refused(vec![Refusal::new(
                    Rule::CardidOverlap,
                    "a backup range touches cardid; the plan is inconsistent",
                )]));
            }
            match session.read_region(s as u32, (e - s) as u32) {
                Ok(bytes) => regions.push((s as u32, bytes)),
                Err(err) => {
                    log.record(Step::Backup, false);
                    return Err(err.into());
                }
            }
        }
        let evidence = match store.store(&plan.plan_sha256, device_key.as_ref(), &regions) {
            Ok(evidence) => evidence,
            Err(why) => {
                log.record(Step::Backup, false);
                return Err(FlowError::Session(why));
            }
        };
        log.backup = Some(evidence);
        let refusals = check_backup(Some(&evidence));
        log.record(Step::Backup, refusals.is_empty());
        if !refusals.is_empty() {
            return Err(FlowError::Refused(refusals));
        }
    }
    let wrote = plan
        .erase_nvs
        .map_or(Ok(()), |(offset, size)| session.erase_region(offset, size))
        .and_then(|()| session.write(&plan.writes));
    log.record(Step::Write, wrote.is_ok());
    if let Err(e) = wrote {
        return Err(write_failed(e, backed_up));
    }
    let verified = session.verify(&plan.writes);
    log.record(Step::Verify, verified.is_ok());
    if let Err(e) = verified {
        return Err(write_failed(e, backed_up));
    }
    Ok(())
}

/// A failed write or verify, with [`WRITE_FAILED_INSTRUCTIONS`] when a backup of this run exists.
/// A busy port keeps its own code: the flash never started.
fn write_failed(error: SessionError, backed_up: bool) -> FlowError {
    match FlowError::from(error) {
        FlowError::Session(why) if backed_up => {
            FlowError::Session(format!("{why}. {WRITE_FAILED_INSTRUCTIONS}"))
        }
        other => other,
    }
}

/// Step 3: applies `plan` to an emulator instance with the real partition layout and a synthetic
/// cardid, through the same esptool invocation as the device. Identity must be the Passport's, the
/// boot check must pass, and the synthetic cardid must stay bit-identical.
pub fn rehearse(
    request: &PlanRequest<'_>,
    plan: &Plan,
    target: &mut dyn DeviceSession,
    report: &mut FlowReport,
) -> Result<(), FlowError> {
    let result = identify(target, request, plan)
        .and_then(|_| apply(target, plan, None, &mut report.rehearsal));
    report.device.record(Step::Rehearse, result.is_ok());
    result.map_err(|e| match e {
        FlowError::Refused(r) => FlowError::Refused(r),
        other => FlowError::CheckFailed(format!("rehearsal: {other:?}{}", debug_build_hint())),
    })
}

/// A hint for a failed rehearsal in a debug build: the emulated stub's deflate is too slow there
/// for esptool's `FLASH_DEFL_DATA` timeout on a large app ("The chip stopped responding"), while
/// a small app rehearses fine. Empty in a release build.
fn debug_build_hint() -> &'static str {
    if cfg!(debug_assertions) {
        ". This is a debug build: the emulated flasher stub's deflate runs too slowly for \
         esptool's FLASH_DEFL_DATA timeout on an app of any size, and a large one fails where a \
         probe-sized one succeeds. Rebuild with `--release` before reading this as a device or \
         image fault"
    } else {
        ""
    }
}

pub struct Effects<'a> {
    pub discovery: &'a mut dyn Discovery,
    pub paths: &'a dyn DevicePaths,
    pub rehearsal: &'a mut dyn DeviceSession,
    pub confirmer: &'a mut dyn Confirmer,
    pub opener: &'a mut dyn SessionOpener,
    pub backups: &'a mut dyn BackupStore,
    /// Step 7, `--backup`. `None` backs up only the sectors the plan writes.
    pub full_backup: Option<&'a mut dyn FullBackup>,
}

pub fn flash_device(
    request: &PlanRequest<'_>,
    requested_port: Option<&str>,
    fx: Effects<'_>,
) -> (FlowReport, Result<(), FlowError>) {
    let mut report = FlowReport::default();
    let result = run(request, requested_port, fx, &mut report);
    (report, result)
}

fn run(
    request: &PlanRequest<'_>,
    requested_port: Option<&str>,
    fx: Effects<'_>,
    report: &mut FlowReport,
) -> Result<(), FlowError> {
    let candidates = fx.discovery.enumerate().map_err(FlowError::Session)?;
    let port = select_port(&candidates, requested_port, fx.paths);
    report.device.record(Step::Discover, port.is_ok());
    let port = port.map_err(|r| FlowError::Refused(vec![r]))?;

    let outcome = plan_flash(request, None);
    report.plan_sha256 = Some(outcome.plan.plan_sha256_hex());
    report
        .device
        .record(Step::Plan, outcome.refusals.is_empty());
    let plan = match outcome.accepted() {
        Some(plan) => plan.clone(),
        None => return Err(FlowError::Refused(outcome.refusals)),
    };

    rehearse(request, &plan, fx.rehearsal, report)?;

    let prompt = ConfirmPrompt::of(&plan, fx.full_backup.is_some());
    let answer = fx.confirmer.confirm(&prompt);
    let confirmed = match answer {
        Answer::Accepted { plan_sha256 } if plan_sha256 == prompt.plan_sha256 => Confirmed {
            port,
            plan_sha256: plan.plan_sha256,
            // Step 10 pulses the reset line on the port it already has open: after esptool's own
            // `--after hard_reset` the ROM banner is gone before the reader attaches.
            reset: true,
        },
        Answer::Accepted { .. } => {
            report.device.record(Step::Confirm, false);
            return Err(FlowError::Refused(vec![Refusal::new(
                Rule::ConfirmationMismatch,
                "the confirmation named a different plan",
            )]));
        }
        Answer::Declined => {
            report.device.record(Step::Confirm, false);
            return Err(FlowError::Declined);
        }
        Answer::Unavailable(hint) => {
            report.device.record(Step::Confirm, false);
            return Err(FlowError::ConfirmationUnavailable(hint));
        }
    };
    report.device.record(Step::Confirm, true);

    let mut session = match fx.opener.open(&confirmed) {
        Ok(session) => session,
        Err(e) => {
            report.device.record(Step::Identify, false);
            return Err(e.into());
        }
    };
    let identified = identify(session.as_mut(), request, &plan);
    report.device.record(Step::Identify, identified.is_ok());
    let device_key = match identified {
        Ok(key) => key,
        Err(e) => {
            let _ = session.hard_reset();
            return Err(e);
        }
    };
    apply(
        session.as_mut(),
        &plan,
        Some(Backups {
            store: fx.backups,
            full: fx.full_backup,
            device_key,
        }),
        &mut report.device,
    )
}
