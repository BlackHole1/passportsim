//! The real-device pieces, compiled only with feature `device` on macOS and Windows: the port
//! spelling rules ([`HostPorts`]), the esptool runner ([`StdRunner`]), the boot console reader
//! ([`PortConsole`]) and the flow's [`SessionOpener`] ([`DeviceOpener`]). This is the only code
//! that may name a host serial device, and it opens only a port in a [`Confirmed`]. No test or CI
//! job constructs a [`DeviceOpener`] over a real port.
//!
//! Host calls live in `exec/macos.rs` and `exec/windows.rs`, which export the same functions.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::console::{
    BOOT_CONSOLE_MAX_BYTES, BOOT_CONSOLE_POLL, boot_console_is_complete, redact_boot_console,
};
use crate::flow::{Confirmed, DevicePaths, DeviceSession, SessionError, SessionOpener};
use crate::rehearse::{
    ConsoleOpener, EsptoolCommand, EsptoolSession, HostOs, Invocation, PortBootConsole,
    ProcessOutput, ProcessRunner, check_invocation,
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

/// The call-out device prefix, the only macOS spelling a flash may go through.
const CALLOUT: &str = "/dev/cu.";
/// The dial-in twin, which is refused.
const DIALIN: &str = "/dev/tty.";
const DEVICE_NAMESPACE: &str = "\\\\.\\";
const COM_STEM: &str = "COM";

/// The macOS device-path rule: `/dev/cu.*` only, compared case-sensitively.
#[derive(Copy, Clone, Debug, Default)]
pub struct CalloutPaths;

impl DevicePaths for CalloutPaths {
    fn is_flash_port(&self, path: &str) -> bool {
        path.starts_with(CALLOUT) && !path.starts_with(DIALIN) && !path.contains("..")
    }

    fn case_insensitive(&self) -> bool {
        false
    }
}

/// The Windows port rule: `COM<n>`, case-insensitive, `n` decimal from 1 with no leading zero and
/// at most four digits. Opened as `\\.\COM<n>` for every n, because `COM10` and above resolve
/// only with the prefix (Microsoft Learn, "Naming Files, Paths, and Namespaces").
#[derive(Copy, Clone, Debug, Default)]
pub struct ComPorts;

impl ComPorts {
    pub fn number(path: &str) -> Option<u32> {
        let stem = path.get(..COM_STEM.len())?;
        if !stem.eq_ignore_ascii_case(COM_STEM) {
            return None;
        }
        let digits = &path[COM_STEM.len()..];
        let ok = (1..=4).contains(&digits.len())
            && digits.bytes().all(|b| b.is_ascii_digit())
            && !digits.starts_with('0');
        if ok { digits.parse().ok() } else { None }
    }
}

impl DevicePaths for ComPorts {
    fn is_flash_port(&self, path: &str) -> bool {
        ComPorts::number(path).is_some()
    }

    fn case_insensitive(&self) -> bool {
        true
    }

    fn open_path(&self, port: &str) -> String {
        format!("{DEVICE_NAMESPACE}{port}")
    }
}

#[cfg(target_os = "macos")]
pub type HostPorts = CalloutPaths;
#[cfg(windows)]
pub type HostPorts = ComPorts;

pub const HOST_OS: HostOs = if cfg!(windows) {
    HostOs::Windows
} else {
    HostOs::MacOs
};

/// Spawns processes with an argument vector, never a shell, each with a deadline, and keeps
/// scratch files in an owner-only directory (0700 and this user's on macOS, a protected owner-only
/// DACL on Windows). A file esptool wrote is narrowed to owner-only before it is read.
pub struct StdRunner {
    command: EsptoolCommand,
    scratch: PathBuf,
    counter: u32,
    deadlines: Deadlines,
}

impl StdRunner {
    /// A runner for the resolved `command` only. `scratch` is created owner-only if absent and
    /// refused if it is wider.
    pub fn new(
        scratch: &Path,
        command: &EsptoolCommand,
        deadlines: Deadlines,
    ) -> Result<StdRunner, String> {
        if !scratch.exists() {
            os::create_private_dir(scratch).map_err(|e| format!("scratch directory: {e}"))?;
        }
        os::check_private_dir(scratch)?;
        // Clear files left by a run that died: names restart at `0-...` and `write_file` opens
        // `create_new`, so a leftover would fail every later flash with `EEXIST`. Regular files
        // directly inside only, never a recursive removal.
        for entry in fs::read_dir(scratch).map_err(|e| format!("scratch directory: {e}"))? {
            let entry = entry.map_err(|e| format!("scratch directory: {e}"))?;
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(StdRunner {
            command: command.clone(),
            scratch: scratch.to_path_buf(),
            counter: 0,
            deadlines,
        })
    }
}

impl ProcessRunner for StdRunner {
    fn run(&mut self, invocation: &Invocation) -> Result<ProcessOutput, String> {
        let scratch = &self.scratch;
        let file_len = |path: &str| {
            let path = Path::new(path);
            if !path.starts_with(scratch) {
                return None;
            }
            fs::symlink_metadata(path)
                .ok()
                .filter(fs::Metadata::is_file)
                .map(|m| m.len())
        };
        check_invocation(&self.command, invocation, &file_len).map_err(|r| r.to_string())?;
        run_with_deadline(invocation, &self.scratch, self.deadlines.of(invocation))
    }

    fn scratch_path(&mut self, name: &str) -> String {
        self.counter += 1;
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.scratch
            .join(format!("{}-{safe}", self.counter))
            .to_string_lossy()
            .into_owned()
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let mut file =
            os::create_private_file(Path::new(path)).map_err(|e| format!("scratch file: {e}"))?;
        file.write_all(bytes)
            .map_err(|e| format!("scratch file: {e}"))
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        if !Path::new(path).starts_with(&self.scratch) {
            return Err("a scratch file outside the scratch directory".to_owned());
        }
        let meta = fs::symlink_metadata(path).map_err(|e| format!("scratch file: {e}"))?;
        if !meta.is_file() {
            return Err("a scratch file is not a regular file".to_owned());
        }
        os::narrow_private_file(Path::new(path))?;
        fs::read(path).map_err(|e| format!("scratch file: {e}"))
    }

    fn remove_file(&mut self, path: &str) {
        let _ = fs::remove_file(path);
    }
}

/// Python variables that change what an interpreter imports or runs at start. `-I` already
/// ignores `PYTHON*`; they are removed too, for an esptool executable that cannot take `-I`.
const PYTHON_IMPORT_VARIABLES: &[&str] = &[
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONSTARTUP",
    "PYTHONUSERBASE",
    "PYTHONNOUSERSITE",
    "PYTHONSAFEPATH",
    "PYTHONPLATLIBDIR",
    "PYTHONEXECUTABLE",
    "PYTHONINSPECT",
    "PYTHONWARNINGS",
    "PYTHONPYCACHEPREFIX",
    "__PYVENV_LAUNCHER__",
];

/// The `Command` for one invocation: no shell, working directory the scratch directory (so nothing
/// is imported from where the CLI started), and every `PYTHON*` variable removed. The prefix test
/// ignores case because Windows environment names do.
fn esptool_command(invocation: &Invocation, scratch: &Path) -> Command {
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .current_dir(scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in PYTHON_IMPORT_VARIABLES {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("PYTHON")
        {
            command.env_remove(name);
        }
    }
    command
}

/// Per-subcommand deadlines: flash transfers are slow (a full app write over RFC 2217 takes about
/// 36 s), identity and version queries are not.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Deadlines {
    /// `chip_id`, `flash_id`, `version`.
    pub query: Duration,
    /// `write_flash`, `read_flash`, `verify_flash`, `erase_region`.
    pub flash: Duration,
}

impl Default for Deadlines {
    fn default() -> Deadlines {
        Deadlines {
            query: Duration::from_secs(60),
            flash: Duration::from_secs(900),
        }
    }
}

impl Deadlines {
    pub fn of(&self, invocation: &Invocation) -> Duration {
        let flash = invocation.args.iter().any(|a| {
            matches!(
                a.replace('-', "_").as_str(),
                "write_flash" | "read_flash" | "verify_flash" | "erase_region"
            )
        });
        if flash { self.flash } else { self.query }
    }
}

/// Reads a pipe to its end on its own thread, so a child writing more than the pipe buffer never
/// blocks on a parent that is only polling for its exit.
fn drain(pipe: Option<impl std::io::Read + Send + 'static>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = tx.send(bytes);
        });
    }
    rx
}

fn run_with_deadline(
    invocation: &Invocation,
    scratch: &Path,
    timeout: Duration,
) -> Result<ProcessOutput, String> {
    let mut child = esptool_command(invocation, scratch)
        .spawn()
        .map_err(|e| format!("spawn esptool: {e}"))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| format!("wait for esptool: {e}"))?
        {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "esptool did not finish within {} s and was stopped",
                    timeout.as_secs()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    // The pipes close when the child exits; a grandchild holding them gets a short grace only.
    let grace = Duration::from_secs(5);
    let mut output =
        String::from_utf8_lossy(&stdout.recv_timeout(grace).unwrap_or_default()).into_owned();
    output.push_str(&String::from_utf8_lossy(
        &stderr.recv_timeout(grace).unwrap_or_default(),
    ));
    Ok(ProcessOutput {
        success: status.success(),
        output,
    })
}

/// How long each leg of the reset pulse is held, as in esptool's `usb_jtag_serial` reset.
const RESET_PULSE_HOLD: Duration = Duration::from_millis(100);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineState {
    pub(crate) dtr: bool,
    pub(crate) rts: bool,
}

/// The reset pulse: both lines quiet, RTS asserted (the edge that pulls the chip's reset), both
/// quiet again. The first state is needed because the macOS driver reports both lines asserted on
/// open.
pub(crate) const RESET_STATES: [LineState; 3] = [
    LineState {
        dtr: false,
        rts: false,
    },
    LineState {
        dtr: false,
        rts: true,
    },
    LineState {
        dtr: false,
        rts: false,
    },
];

/// One `EscapeCommFunction` code, as a value every host can test.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum ModemEscape {
    SetDtr,
    ClearDtr,
    SetRts,
    ClearRts,
}

/// The modem lines of an open port, one escape function at a time: `EscapeCommFunction` on
/// Windows, a recording fake in the tests.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) trait ModemControl {
    fn escape(&mut self, function: ModemEscape) -> std::io::Result<()>;
}

/// The escape functions of [`RESET_STATES`], two per state.
///
/// DTR is set before RTS in every state: a USB CDC driver sends both lines in one control request,
/// and on Windows a DTR change alone is not documented to reach the device before the next RTS
/// change. UNVERIFIED on the device over Windows.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn reset_escapes() -> [[ModemEscape; 2]; 3] {
    RESET_STATES.map(|state| {
        [
            if state.dtr {
                ModemEscape::SetDtr
            } else {
                ModemEscape::ClearDtr
            },
            if state.rts {
                ModemEscape::SetRts
            } else {
                ModemEscape::ClearRts
            },
        ]
    })
}

/// Drives [`reset_escapes`] on `lines`, holding every state but the last for `hold`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn drive_reset(
    lines: &mut dyn ModemControl,
    hold: Duration,
    sleep: &mut dyn FnMut(Duration),
) -> Result<(), SessionError> {
    let states = reset_escapes();
    let last = states.len() - 1;
    for (index, state) in states.into_iter().enumerate() {
        for function in state {
            lines.escape(function).map_err(|e| {
                SessionError::Failed(format!(
                    "the reset line could not be driven: {function:?} failed with {}",
                    e.kind()
                ))
            })?;
        }
        // The last state needs no hold: the read window starts right after it.
        if index < last {
            sleep(hold);
        }
    }
    Ok(())
}

/// The DCB flag word of the boot-console read on Windows, from the port's own (Microsoft Learn,
/// "DCB structure (winbase.h)": `fBinary` bit 0, `fParity` 1, `fOutxCtsFlow` 2, `fOutxDsrFlow` 3,
/// `fDtrControl` 4-5, `fDsrSensitivity` 6, `fTXContinueOnXoff` 7, `fOutX` 8, `fInX` 9,
/// `fErrorChar` 10, `fNull` 11, `fRtsControl` 12-13, `fAbortOnError` 14).
///
/// Binary on, parity and every flow control off, no character rewriting, no abort on error. DTR
/// and RTS keep the driver's control, so the open moves no line; a handshake or toggle control
/// becomes "enabled" (1).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn console_dcb_bits(current: u32) -> u32 {
    const BINARY: u32 = 1 << 0;
    const DTR_SHIFT: u32 = 4;
    const RTS_SHIFT: u32 = 12;
    const TX_CONTINUE_ON_XOFF: u32 = 1 << 7;
    let enabled = |shift: u32| u32::from((current >> shift) & 0b11 != 0);
    // Bits 15 and up are `fDummy2`, reserved.
    let reserved = current & !0x7FFF;
    reserved
        | BINARY
        | (current & TX_CONTINUE_ON_XOFF)
        | (enabled(DTR_SHIFT) << DTR_SHIFT)
        | (enabled(RTS_SHIFT) << RTS_SHIFT)
}

/// The `COMMTIMEOUTS` read fields of the boot-console read on Windows, `(ReadIntervalTimeout,
/// ReadTotalTimeoutMultiplier, ReadTotalTimeoutConstant)`: `MAXDWORD`, 0, 0 makes a read "return
/// immediately with the bytes that have already been received" (Microsoft Learn, "COMMTIMEOUTS
/// structure"), so [`read_window`] polls it like a non-blocking descriptor on macOS.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const POLLED_READ_TIMEOUTS: (u32, u32, u32) = (u32::MAX, 0, 0);

/// What one console read saw, in counts only: no console text ever leaves this type.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsoleStats {
    pub bytes: u64,
    /// Whether the device sent nothing at all, a different problem from lines that did not match.
    pub empty: bool,
}

impl ConsoleStats {
    fn of(console: &[u8]) -> ConsoleStats {
        ConsoleStats {
            bytes: console.len() as u64,
            empty: console.is_empty(),
        }
    }
}

/// The boot check's console read on the real device: reads the confirmed port for up to
/// [`crate::rehearse::BOOT_CONSOLE_TIMEOUT_MS`] and returns the redacted console. If the
/// [`Confirmed`] carries a restart, the device is first reset by a line pulse on the same handle.
///
/// The port is opened read-only (esptool owns every byte to the device; the pulse sends none). The
/// window is bounded in time and memory ([`BOOT_CONSOLE_MAX_BYTES`]) and ends early once the
/// console holds what the boot check judges.
pub struct PortConsole<'a> {
    paths: &'a dyn DevicePaths,
    timeout: Duration,
    stats: ConsoleStats,
}

impl<'a> PortConsole<'a> {
    pub fn new(paths: &'a dyn DevicePaths) -> PortConsole<'a> {
        PortConsole {
            paths,
            timeout: Duration::from_millis(u64::from(crate::rehearse::BOOT_CONSOLE_TIMEOUT_MS)),
            stats: ConsoleStats::default(),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> PortConsole<'a> {
        self.timeout = timeout;
        self
    }

    /// What the last [`ConsoleOpener::boot_log_of`] saw. Before any read, `empty` is `false`: "no
    /// read yet" is not a verdict about a device.
    pub fn stats(&self) -> ConsoleStats {
        self.stats
    }
}

impl ConsoleOpener for PortConsole<'_> {
    fn boot_log_of(&mut self, confirmed: &Confirmed) -> Result<String, SessionError> {
        let port = confirmed.port();
        if !self.paths.is_flash_port(port) {
            return Err(SessionError::Failed(
                "the boot console was asked for a path that is not a flashing port".to_owned(),
            ));
        }
        let mut file = os::open_console(&self.paths.open_path(port))?;
        if confirmed.reset_confirmed() {
            os::pulse_reset(&file, RESET_PULSE_HOLD)?;
        }
        let console = read_window(&mut file, Instant::now() + self.timeout)?;
        self.stats = ConsoleStats::of(&console);
        Ok(redact_boot_console(&console))
    }
}

/// Reads until `deadline`, until [`BOOT_CONSOLE_MAX_BYTES`], or until the console is complete.
/// Split out so a test can drive it without a port.
fn read_window(source: &mut dyn std::io::Read, deadline: Instant) -> Result<Vec<u8>, SessionError> {
    let mut console: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while Instant::now() < deadline && console.len() < BOOT_CONSOLE_MAX_BYTES {
        match source.read(&mut chunk) {
            Ok(0) => std::thread::sleep(BOOT_CONSOLE_POLL),
            Ok(n) => {
                let room = BOOT_CONSOLE_MAX_BYTES - console.len();
                console.extend_from_slice(&chunk[..n.min(room)]);
                if boot_console_is_complete(&console) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(BOOT_CONSOLE_POLL);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(SessionError::Failed(format!(
                    "the boot console read failed: {}",
                    e.kind()
                )));
            }
        }
    }
    Ok(console)
}

pub struct DeviceOpener<'r> {
    command: EsptoolCommand,
    runner: &'r mut dyn ProcessRunner,
    console: Option<&'r mut dyn ConsoleOpener>,
    mac_salt: Option<[u8; 32]>,
}

impl<'r> DeviceOpener<'r> {
    /// Without [`DeviceOpener::with_console`] the flow fails closed at the boot check.
    pub fn new(command: EsptoolCommand, runner: &'r mut dyn ProcessRunner) -> DeviceOpener<'r> {
        DeviceOpener {
            command,
            runner,
            console: None,
            mac_salt: None,
        }
    }

    /// The per-install salt that keys the backup directory by SHA-256(salt, MAC). Without it the
    /// backup directory says `unkeyed`.
    pub fn with_mac_salt(mut self, salt: [u8; 32]) -> DeviceOpener<'r> {
        self.mac_salt = Some(salt);
        self
    }

    /// The boot console. It learns the confirmed port only when the session opens.
    pub fn with_console(mut self, console: &'r mut dyn ConsoleOpener) -> DeviceOpener<'r> {
        self.console = Some(console);
        self
    }
}

impl SessionOpener for DeviceOpener<'_> {
    fn open(&mut self, confirmed: &Confirmed) -> Result<Box<dyn DeviceSession + '_>, SessionError> {
        if !HostPorts::default().is_flash_port(confirmed.port()) {
            return Err(SessionError::Failed(
                "the confirmed port is not a flashing port of this host".to_owned(),
            ));
        }
        let console = self.console.as_mut().map(|opener| {
            Box::new(PortBootConsole::new(confirmed, &mut **opener))
                as Box<dyn crate::rehearse::BootConsole + '_>
        });
        let session = EsptoolSession::device(
            self.command.clone(),
            confirmed,
            &mut *self.runner,
            None,
            console,
        );
        Ok(Box::new(match self.mac_salt {
            Some(salt) => session.with_mac_salt(salt),
            None => session,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console that answers with the same chunk forever, or never.
    struct Chatter {
        line: &'static str,
        blocks: bool,
    }

    impl std::io::Read for Chatter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.blocks {
                return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "wait"));
            }
            let bytes = self.line.as_bytes();
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            Ok(n)
        }
    }

    #[test]
    fn a_console_that_never_stops_is_capped_at_64_kb() {
        let mut source = Chatter {
            line: "I (300) pk_app: chatter chatter chatter\n",
            blocks: false,
        };
        let console = read_window(&mut source, Instant::now() + Duration::from_secs(30))
            .expect("the cap ends the read");
        assert_eq!(console.len(), BOOT_CONSOLE_MAX_BYTES);
        let redacted = redact_boot_console(&console);
        assert!(redacted.starts_with('<'), "{redacted}");
        assert!(!crate::flow::boot_check(&redacted, None).banner);
    }

    #[test]
    fn a_silent_console_returns_when_the_window_ends() {
        let mut source = Chatter {
            line: "",
            blocks: true,
        };
        let started = Instant::now();
        let console = read_window(&mut source, started + Duration::from_millis(120))
            .expect("a silent console is not an error");
        assert!(console.is_empty());
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
    }

    #[test]
    fn a_complete_boot_ends_the_window_early() {
        let mut source = Chatter {
            line: concat!(
                "ESP-ROM:esp32c3-api1-20210207\n",
                "rst:0xc (RTC_SW_CPU_RST),boot:0xa (SPI_FAST_FLASH_BOOT)\n",
                "I (117) app_init: ELF file SHA256:  f5429c0b8...\n",
            ),
            blocks: false,
        };
        let started = Instant::now();
        let console = read_window(&mut source, started + Duration::from_secs(30))
            .expect("a complete boot ends the read");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(console.len() < BOOT_CONSOLE_MAX_BYTES);
        let redacted = redact_boot_console(&console);
        assert!(
            crate::flow::boot_check(&redacted, None).banner,
            "{redacted}"
        );
    }

    #[test]
    fn only_a_token_that_confirmed_a_restart_authorises_one() {
        let plain = Confirmed::for_crate_tests("/dev/cu.usbmodem1101", [3; 32]);
        let restart = Confirmed::for_crate_tests_with_reset("/dev/cu.usbmodem1101", [3; 32]);
        assert!(
            !plain.reset_confirmed(),
            "a plain read must not reach `pulse_reset`"
        );
        assert!(restart.reset_confirmed());
        assert_eq!(plain.port(), restart.port());
    }

    /// An idle, already-booted Passport sends 0 bytes, so an empty console is the ordinary outcome
    /// of a read without a reset, not a broken device.
    #[test]
    fn an_empty_console_is_distinguishable_from_one_that_did_not_match() {
        let mut silent = Chatter {
            line: "",
            blocks: true,
        };
        let console = read_window(&mut silent, Instant::now() + Duration::from_millis(120))
            .expect("a silent console is not an error");
        let silence = ConsoleStats::of(&console);
        assert_eq!(
            silence,
            ConsoleStats {
                bytes: 0,
                empty: true
            }
        );

        // Every line is dropped, so only the byte count tells this apart from silence.
        let mut noisy = Chatter {
            line: "I (300) pk_app: nothing step 10 judges\n",
            blocks: false,
        };
        let console = read_window(&mut noisy, Instant::now() + Duration::from_secs(30))
            .expect("the cap ends the read");
        let spoke = ConsoleStats::of(&console);
        assert!(!spoke.empty);
        assert!(spoke.bytes > 0);
        assert!(!crate::flow::boot_check(&redact_boot_console(&console), None).banner);
    }

    #[test]
    fn a_broken_read_is_an_error() {
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("the cable was pulled"))
            }
        }
        let error = read_window(&mut Broken, Instant::now() + Duration::from_secs(1))
            .expect_err("a broken read stops the window");
        let SessionError::Failed(why) = error else {
            panic!("a failure, not a refusal");
        };
        assert!(why.contains("boot console read failed"), "{why}");
    }

    #[test]
    fn only_callout_devices_are_flash_ports() {
        assert!(CalloutPaths.is_flash_port("/dev/cu.usbmodem1101"));
        for path in [
            "/dev/tty.usbmodem1101",
            "/dev/cu.../disk0",
            "/tmp/cu.usbmodem",
            "rfc2217://127.0.0.1:1",
        ] {
            assert!(!CalloutPaths.is_flash_port(path), "{path}");
        }
    }

    fn test_command() -> EsptoolCommand {
        EsptoolCommand {
            program: "/nonexistent/esptool".to_owned(),
            prefix: Vec::new(),
            major: 4,
        }
    }

    fn scratch() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "pemu-planner-exec-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn a_scratch_file_left_by_a_dead_run_does_not_block_the_next_one() {
        let dir = scratch();
        let mut first =
            StdRunner::new(&dir, &test_command(), Deadlines::default()).expect("scratch");
        let path = first.scratch_path("full.bin");
        first.write_file(&path, b"").expect("write");
        // No `remove_file`: this is the run that was killed.
        drop(first);
        assert!(Path::new(&path).exists(), "the dead run left its file");

        let mut second =
            StdRunner::new(&dir, &test_command(), Deadlines::default()).expect("scratch");
        assert!(!Path::new(&path).exists(), "the new runner cleared it");
        assert_eq!(second.scratch_path("full.bin"), path);
        second
            .write_file(&path, b"xyz")
            .expect("the next run writes it");
        assert_eq!(second.read_file(&path).expect("read"), b"xyz");
        second.remove_file(&path);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scratch_files_are_private_through_the_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch();
        let mut runner =
            StdRunner::new(&dir, &test_command(), Deadlines::default()).expect("scratch");
        let path = runner.scratch_path("a b.bin");
        runner.write_file(&path, b"xyz").expect("write");
        assert_eq!(
            fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(runner.read_file(&path).expect("read"), b"xyz");
        runner.remove_file(&path);
        let esptool_file = runner.scratch_path("read.bin");
        fs::write(&esptool_file, b"table").expect("esptool-like write");
        fs::set_permissions(&esptool_file, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(runner.read_file(&esptool_file).expect("read"), b"table");
        assert_eq!(
            fs::metadata(&esptool_file)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(runner.read_file("/etc/hosts").is_err());
        let refused = runner.run(&Invocation {
            program: "/nonexistent/esptool".to_owned(),
            args: vec!["erase_flash".to_owned()],
        });
        assert!(refused.expect_err("refused").contains("erase_flash"));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("widen");
        assert!(StdRunner::new(&dir, &test_command(), Deadlines::default()).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn scratch_files_are_private_through_the_dacl() {
        let dir = scratch();
        let mut runner =
            StdRunner::new(&dir, &test_command(), Deadlines::default()).expect("scratch");
        let path = runner.scratch_path("a b.bin");
        runner.write_file(&path, b"xyz").expect("write");
        windows::check_owner_only(Path::new(&path)).expect("owner-only at creation");
        assert_eq!(runner.read_file(&path).expect("read"), b"xyz");
        runner.remove_file(&path);
        let esptool_file = runner.scratch_path("read.bin");
        fs::write(&esptool_file, b"table").expect("esptool-like write");
        assert_eq!(runner.read_file(&esptool_file).expect("read"), b"table");
        windows::check_owner_only(Path::new(&esptool_file)).expect("narrowed before the read");
        let outside = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        assert!(runner.read_file(&format!(r"{outside}\win.ini")).is_err());
        let refused = runner.run(&Invocation {
            program: "/nonexistent/esptool".to_owned(),
            args: vec!["erase_flash".to_owned()],
        });
        assert!(refused.expect_err("refused").contains("erase_flash"));
        let _ = fs::remove_dir_all(&dir);

        let wide = scratch();
        fs::create_dir_all(&wide).expect("an ordinary directory");
        let why = StdRunner::new(&wide, &test_command(), Deadlines::default())
            .err()
            .expect("an inherited ACL is refused");
        assert!(why.contains("owner-only"), "{why}");
        let _ = fs::remove_dir_all(&wide);
    }

    /// Runs for about five seconds and prints nothing.
    fn slow_program() -> Invocation {
        if cfg!(windows) {
            powershell("Start-Sleep -Seconds 5")
        } else {
            Invocation {
                program: "/bin/sleep".to_owned(),
                args: vec!["5".to_owned()],
            }
        }
    }

    /// Writes 2 MiB to stdout and exits.
    fn loud_program() -> Invocation {
        if cfg!(windows) {
            powershell("[Console]::Out.Write('x' * 2097152)")
        } else {
            Invocation {
                program: "/bin/dd".to_owned(),
                args: vec![
                    "if=/dev/zero".to_owned(),
                    "bs=1048576".to_owned(),
                    "count=2".to_owned(),
                ],
            }
        }
    }

    /// Windows PowerShell by its absolute path, never a `PATH` search.
    fn powershell(command: &str) -> Invocation {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        Invocation {
            program: format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe"),
            args: ["-NoProfile", "-NonInteractive", "-Command", command]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    #[test]
    fn a_process_past_its_deadline_is_stopped() {
        let started = Instant::now();
        let err = run_with_deadline(
            &slow_program(),
            &std::env::temp_dir(),
            Duration::from_millis(200),
        )
        .expect_err("killed");
        assert!(err.contains("did not finish"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn esptool_runs_isolated_in_the_scratch_directory() {
        let dir = std::env::temp_dir();
        let invocation = Invocation {
            program: "/env/idf/bin/python".to_owned(),
            args: ["-I", "-m", "esptool", "version"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        let command = esptool_command(&invocation, &dir);
        assert_eq!(command.get_program(), "/env/idf/bin/python");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["-I", "-m", "esptool", "version"]);
        assert_eq!(command.get_current_dir(), Some(dir.as_path()));
        let envs: Vec<_> = command.get_envs().collect();
        for name in [
            "PYTHONPATH",
            "PYTHONHOME",
            "PYTHONSTARTUP",
            "PYTHONUSERBASE",
        ] {
            assert!(
                envs.contains(&(std::ffi::OsStr::new(name), None)),
                "{name} is removed"
            );
        }
        assert!(
            envs.iter().all(|(_, value)| value.is_none()),
            "nothing is added"
        );
    }

    #[test]
    fn a_child_writing_megabytes_is_drained() {
        let started = Instant::now();
        let out = run_with_deadline(
            &loud_program(),
            &std::env::temp_dir(),
            Duration::from_secs(20),
        )
        .expect("finished");
        assert!(out.success, "{}", &out.output[..out.output.len().min(200)]);
        assert!(out.output.len() >= 2 * 1024 * 1024, "{}", out.output.len());
        assert!(started.elapsed() < Duration::from_secs(15));

        let deadlines = Deadlines {
            query: Duration::from_secs(1),
            flash: Duration::from_secs(2),
        };
        let with = |sub: &str| Invocation {
            program: "p".to_owned(),
            args: vec!["--after".to_owned(), "no_reset".to_owned(), sub.to_owned()],
        };
        for sub in ["write_flash", "read-flash", "verify_flash", "erase_region"] {
            assert_eq!(deadlines.of(&with(sub)), Duration::from_secs(2), "{sub}");
        }
        for sub in ["chip_id", "flash-id", "version"] {
            assert_eq!(deadlines.of(&with(sub)), Duration::from_secs(1), "{sub}");
        }
    }

    #[test]
    fn only_com_n_names_are_windows_flash_ports() {
        for port in ["COM1", "COM3", "com3", "Com12", "COM256", "COM4096"] {
            assert!(ComPorts.is_flash_port(port), "{port}");
        }
        for port in [
            "COM",
            "COM0",
            "COM03",
            "COM12345",
            "COM3:",
            "COM3.txt",
            r"\\.\COM3",
            r"C:\COM3",
            "COMX",
            " COM3",
            "LPT1",
            "/dev/cu.usbmodem1101",
            "rfc2217://127.0.0.1:1",
        ] {
            assert!(!ComPorts.is_flash_port(port), "{port}");
        }
        assert!(ComPorts.case_insensitive());
        assert_eq!(ComPorts::number("com12"), Some(12));
    }

    #[test]
    fn a_windows_port_is_opened_in_the_device_namespace_for_every_n() {
        assert_eq!(ComPorts.open_path("COM3"), r"\\.\COM3");
        assert_eq!(ComPorts.open_path("COM12"), r"\\.\COM12");
        assert_eq!(
            CalloutPaths.open_path("/dev/cu.usbmodem1101"),
            "/dev/cu.usbmodem1101"
        );
        assert_eq!(
            HOST_OS,
            if cfg!(windows) {
                HostOs::Windows
            } else {
                HostOs::MacOs
            }
        );
    }

    /// Records every escape function and hold, and can fail one.
    #[derive(Default)]
    struct FakeLines {
        calls: Vec<ModemEscape>,
        fail_on: Option<ModemEscape>,
    }

    impl ModemControl for FakeLines {
        fn escape(&mut self, function: ModemEscape) -> std::io::Result<()> {
            self.calls.push(function);
            match self.fail_on {
                Some(f) if f == function => Err(std::io::Error::other("the driver refused")),
                _ => Ok(()),
            }
        }
    }

    #[test]
    fn the_windows_reset_pulse_drives_the_macos_states_through_escapes() {
        use ModemEscape::{ClearDtr, ClearRts, SetRts};

        let mut lines = FakeLines::default();
        let mut holds = Vec::new();
        drive_reset(&mut lines, Duration::from_millis(100), &mut |d| {
            holds.push(d)
        })
        .expect("the pulse");
        assert_eq!(
            lines.calls,
            [ClearDtr, ClearRts, ClearDtr, SetRts, ClearDtr, ClearRts],
            "quiet, RTS pulled with DTR quiet, quiet; DTR first in every state"
        );
        assert_eq!(holds, [Duration::from_millis(100); 2]);
        assert_eq!(RESET_STATES.len(), 3);
        assert!(RESET_STATES[1].rts && !RESET_STATES[1].dtr);

        let mut refusing = FakeLines {
            fail_on: Some(SetRts),
            ..FakeLines::default()
        };
        let SessionError::Failed(why) =
            drive_reset(&mut refusing, Duration::ZERO, &mut |_| {}).expect_err("refused")
        else {
            panic!("a failure");
        };
        assert!(why.contains("SetRts"), "{why}");
        assert_eq!(refusing.calls.last(), Some(&SetRts), "nothing after it");
    }

    #[test]
    fn the_console_dcb_turns_flow_control_off_and_keeps_the_lines() {
        // Everything set: DTR handshake (2), RTS toggle (3), every flag, reserved bits.
        let all = u32::MAX;
        let bits = console_dcb_bits(all);
        assert_eq!(bits & 1, 1, "binary");
        for (bit, name) in [
            (1, "parity"),
            (2, "CTS flow"),
            (3, "DSR flow"),
            (6, "DSR sensitivity"),
            (8, "XON/XOFF out"),
            (9, "XON/XOFF in"),
            (10, "error char"),
            (11, "NUL discard"),
            (14, "abort on error"),
        ] {
            assert_eq!(bits >> bit & 1, 0, "{name} is off");
        }
        assert_eq!(bits >> 4 & 0b11, 1, "a DTR handshake becomes enabled");
        assert_eq!(bits >> 12 & 0b11, 1, "an RTS toggle becomes enabled");
        assert_eq!(bits & !0x7FFF, all & !0x7FFF, "reserved bits are kept");
        // Lines off stay off, so a plain read moves no line.
        let quiet = console_dcb_bits(0);
        assert_eq!(quiet >> 4 & 0b11, 0);
        assert_eq!(quiet >> 12 & 0b11, 0);
        assert_eq!(quiet, 1, "binary and nothing else");
        let up = console_dcb_bits(1 << 4 | 1 << 12);
        assert_eq!((up >> 4 & 0b11, up >> 12 & 0b11), (1, 1));
        assert_eq!(POLLED_READ_TIMEOUTS, (u32::MAX, 0, 0));
    }

    /// Needs `PEMU_ESPTOOL` naming a Python interpreter with esptool; says SKIP without it.
    /// `version` opens no port.
    #[test]
    fn a_real_esptool_resolves_as_python_and_reports_its_version() {
        use crate::rehearse::{EsptoolSources, FileProbe, check_esptool_version, resolve_esptool};

        let Some(explicit) = std::env::var_os("PEMU_ESPTOOL") else {
            println!("SKIP: PEMU_ESPTOOL is not set, so no real esptool is checked");
            return;
        };
        struct Probe;
        impl FileProbe for Probe {
            fn is_file(&self, path: &str) -> bool {
                Path::new(path).is_file()
            }
        }
        let sources = EsptoolSources {
            explicit: Some(explicit.to_string_lossy().into_owned()),
            ..EsptoolSources::default()
        };
        let command = resolve_esptool(&sources, HOST_OS, &Probe).expect("resolves");
        assert_eq!(
            command.prefix,
            ["-I", "-m", "esptool"],
            "a Python entry point"
        );
        let dir = scratch();
        let mut runner = StdRunner::new(&dir, &command, Deadlines::default()).expect("scratch");
        let checked = check_esptool_version(command.clone(), &mut runner).expect("4.12 or newer");
        assert!(checked.major >= 4, "{checked:?}");

        // Through `run_with_deadline` directly: the allow list admits esptool subcommands only.
        let probe = Invocation {
            program: command.program.clone(),
            args: vec![
                "-I".to_owned(),
                "-c".to_owned(),
                "import os; print(os.getcwd()); \
                 print(len([k for k in os.environ if k.upper().startswith('PYTHON')]))"
                    .to_owned(),
            ],
        };
        let out = run_with_deadline(&probe, &dir, Duration::from_secs(60)).expect("ran");
        assert!(out.success, "{}", out.output);
        let mut lines = out.output.lines().map(str::trim);
        let cwd = lines.next().expect("a working directory");
        assert_eq!(
            fs::canonicalize(cwd).expect("cwd"),
            fs::canonicalize(&dir).expect("scratch"),
            "the child starts in the scratch directory"
        );
        assert_eq!(lines.next(), Some("0"), "no PYTHON* variable reaches it");
        let _ = fs::remove_dir_all(&dir);
    }
}
