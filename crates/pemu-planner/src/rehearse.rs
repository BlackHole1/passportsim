//! The one esptool invocation the rehearsal and the real device share. The command is resolved
//! once ([`resolve_esptool`]), its argument vectors are built by [`EsptoolSession`], and only the
//! port differs: `rfc2217://127.0.0.1:<port>` for the emulator, the confirmed device path for the
//! Passport. A host [`ProcessRunner`] spawns the processes; this crate spawns nothing.
//!
//! The esptool command line is taken from esptool's documentation and observed output, never from
//! its source.

use pemu_loader::{hex, sha256};

use crate::flow::{Confirmed, DeviceSession, Identity, SessionError};
use crate::plan::PlannedWrite;
use crate::rules::{
    ChipRevision, FLASH_SIZE, PortUse, Refusal, Rule, TABLE_LEN, TABLE_OFFSET, check_emulator_port,
    is_refused_script, round_to_sectors, touches_cardid,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HostOs {
    /// A Python environment keeps its interpreter in `bin/python`.
    MacOs,
    /// `Scripts\python.exe`.
    Windows,
}

/// The inputs of the esptool resolution order, gathered by the host.
#[derive(Clone, Debug, Default)]
pub struct EsptoolSources {
    pub explicit: Option<String>,
    pub idf_python_env: Option<String>,
    /// Python environment directories under the IDF tools directory, newest first.
    pub idf_tools_envs: Vec<String>,
}

pub trait FileProbe {
    fn is_file(&self, path: &str) -> bool;
}

/// A resolved esptool: a program and the arguments that come before esptool's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EsptoolCommand {
    pub program: String,
    /// `["-I", "-m", "esptool"]` for a Python interpreter (isolated mode), empty for an esptool
    /// executable.
    pub prefix: Vec<String>,
    /// The esptool major version, once [`check_version`] ran; it selects the spelling.
    pub major: u32,
}

/// The baud every session asks esptool for. USB Serial/JTAG ignores baud, but the flasher stub
/// paces reads to it: on the device a 256 KB `read_flash` took 22.8 s at 115200 and 3.0 s at
/// 921600, and a whole-part read drops from about 730 s to about 95 s, inside
/// `Deadlines::flash`.
pub const ESPTOOL_BAUD: &str = "921600";

impl EsptoolCommand {
    /// The v4 underscore or v5 dashed spelling of a subcommand or option.
    /// UNVERIFIED: that v5 also dashes option names and reset values.
    pub fn spell(&self, word: &str) -> String {
        if self.major >= 5 {
            word.replace('_', "-")
        } else {
            word.to_owned()
        }
    }

    /// The one argument-vector builder of the rehearsal and the device; the port is its only
    /// session-dependent input.
    ///
    /// `--before usb_reset` is named rather than detected: on the Passport's port esptool reads
    /// PID 0x1001 and sends `USBJTAGSerialReset` anyway (esptool docs, "Advanced Options"), but on
    /// an `rfc2217://` URL it cannot read a PID and falls back to `ClassicReset`, which fails with
    /// `Wrong boot mode detected (0xa)`.
    pub fn invocation(&self, port: &str, after: &str, sub: &[String]) -> Invocation {
        let mut args = self.prefix.clone();
        args.extend([
            "--chip".to_owned(),
            "esp32c3".to_owned(),
            "--port".to_owned(),
            port.to_owned(),
            "--baud".to_owned(),
            ESPTOOL_BAUD.to_owned(),
            "--before".to_owned(),
            self.spell("usb_reset"),
            "--after".to_owned(),
            self.spell(after),
        ]);
        args.extend(sub.iter().cloned());
        Invocation {
            program: self.program.clone(),
            args,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    Refused(Refusal),
    NotFound(String),
    /// `esptool version` failed, printed no version, or printed one older than 4.12.
    Version(String),
}

fn join(dir: &str, parts: &[&str], os: HostOs) -> String {
    let sep = match os {
        HostOs::MacOs => '/',
        HostOs::Windows => '\\',
    };
    let mut path = dir.trim_end_matches(['/', '\\']).to_owned();
    for part in parts {
        path.push(sep);
        path.push_str(part);
    }
    path
}

fn python_of(env: &str, os: HostOs) -> String {
    match os {
        HostOs::MacOs => join(env, &["bin", "python"], os),
        HostOs::Windows => join(env, &["Scripts", "python.exe"], os),
    }
}

fn is_python(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.to_ascii_lowercase().starts_with("python")
}

/// Resolves esptool in a fixed order: `--esptool`, then `$IDF_PYTHON_ENV_PATH`,
/// then the IDF tools directory. A `.bat`, `.cmd` or `.ps1` target is refused; a Python
/// interpreter runs `-m esptool`. The major version is 4 until [`check_version`] says otherwise.
pub fn resolve_esptool(
    sources: &EsptoolSources,
    os: HostOs,
    probe: &dyn FileProbe,
) -> Result<EsptoolCommand, ResolveError> {
    let command = |program: String| {
        if is_refused_script(&program) {
            return Err(ResolveError::Refused(Refusal::new(
                Rule::EsptoolScript,
                "esptool resolved to a .bat, .cmd or .ps1 file, which is never spawned",
            )));
        }
        let prefix = if is_python(&program) {
            vec!["-I".to_owned(), "-m".to_owned(), "esptool".to_owned()]
        } else {
            Vec::new()
        };
        Ok(EsptoolCommand {
            program,
            prefix,
            major: 4,
        })
    };
    if let Some(explicit) = &sources.explicit {
        if is_refused_script(explicit) {
            return command(explicit.clone());
        }
        if !probe.is_file(explicit) {
            return Err(ResolveError::NotFound(
                "`--esptool` names no file".to_owned(),
            ));
        }
        return command(explicit.clone());
    }
    let envs = sources
        .idf_python_env
        .iter()
        .chain(sources.idf_tools_envs.iter());
    for env in envs {
        let python = python_of(env, os);
        if probe.is_file(&python) {
            return command(python);
        }
    }
    Err(ResolveError::NotFound(
        "no esptool: pass `--esptool`, set IDF_PYTHON_ENV_PATH, or install ESP-IDF tools"
            .to_owned(),
    ))
}

/// Reads the version esptool prints and requires at least 4.12. The first `v<major>.<minor>` or
/// `<major>.<minor>` token counts. UNVERIFIED: the exact wording of the version line.
pub fn check_version(output: &str) -> Result<(u32, u32), String> {
    for token in output.split(|c: char| c.is_whitespace() || c == ',') {
        let token = token.strip_prefix('v').unwrap_or(token);
        let mut parts = token.split('.');
        let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
            continue;
        };
        return if (major, minor) >= (4, 12) {
            Ok((major, minor))
        } else {
            Err(format!("esptool {major}.{minor} is older than 4.12"))
        };
    }
    Err("no esptool version in the output".to_owned())
}

/// [`resolve_esptool`], then [`check_esptool_version`].
pub fn resolve_and_check_esptool(
    sources: &EsptoolSources,
    os: HostOs,
    probe: &dyn FileProbe,
    runner: &mut dyn ProcessRunner,
) -> Result<EsptoolCommand, ResolveError> {
    let command = resolve_esptool(sources, os, probe)?;
    check_esptool_version(command, runner)
}

/// Requires `<esptool> version` of at least 4.12 and takes [`EsptoolCommand::major`] from it.
pub fn check_esptool_version(
    mut command: EsptoolCommand,
    runner: &mut dyn ProcessRunner,
) -> Result<EsptoolCommand, ResolveError> {
    let mut args = command.prefix.clone();
    args.push("version".to_owned());
    let invocation = Invocation {
        program: command.program.clone(),
        args,
    };
    let out = runner.run(&invocation).map_err(ResolveError::Version)?;
    if !out.success {
        return Err(ResolveError::Version("`esptool version` failed".to_owned()));
    }
    let (major, _) = check_version(&out.output).map_err(ResolveError::Version)?;
    command.major = major;
    Ok(command)
}

/// One process to spawn: a program and its argument vector, never a shell line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
}

/// What a process returned. The output can carry the MAC, so it is parsed in memory and never
/// stored or printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessOutput {
    pub success: bool,
    pub output: String,
}

/// Spawns processes and handles the scratch files esptool reads and writes. Implemented by the
/// host; scratch files are owner-only and removed after use.
pub trait ProcessRunner {
    fn run(&mut self, invocation: &Invocation) -> Result<ProcessOutput, String>;
    fn scratch_path(&mut self, name: &str) -> String;
    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String>;
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String>;
    fn remove_file(&mut self, path: &str);
}

/// A region MD5 source. The emulator instance computes it over its own flash for the rehearsal; a
/// session with none takes the digest from the device through [`crate::stub_md5`].
pub trait RegionMd5 {
    fn region_md5(&mut self, offset: u32, size: u32) -> Result<[u8; 16], SessionError>;
}

/// The console read after a hard reset, redacted by the host: the emulator's USJ console for the
/// rehearsal, the device port for up to 10 s for the device.
pub trait BootConsole {
    fn boot_log(&mut self) -> Result<String, SessionError>;
}

impl<T: BootConsole + ?Sized> BootConsole for &mut T {
    fn boot_log(&mut self) -> Result<String, SessionError> {
        (**self).boot_log()
    }
}

/// The device half of [`BootConsole`]. It is handed a [`Confirmed`], never a bare port, and
/// `Confirmed` has no public constructor, so only a port a person agreed to is ever opened.
pub trait ConsoleOpener {
    fn boot_log_of(&mut self, confirmed: &Confirmed) -> Result<String, SessionError>;
}

pub const BOOT_CONSOLE_TIMEOUT_MS: u32 = 10_000;

pub struct PortBootConsole<'a> {
    confirmed: Confirmed,
    opener: &'a mut dyn ConsoleOpener,
}

impl<'a> PortBootConsole<'a> {
    pub fn new(confirmed: &Confirmed, opener: &'a mut dyn ConsoleOpener) -> PortBootConsole<'a> {
        PortBootConsole {
            confirmed: confirmed.clone(),
            opener,
        }
    }
}

impl BootConsole for PortBootConsole<'_> {
    fn boot_log(&mut self) -> Result<String, SessionError> {
        self.opener.boot_log_of(&self.confirmed)
    }
}

/// The only esptool subcommands the planner runs, in the v4 spelling. Everything else
/// (`erase_flash`, `write_mem`, `load_ram`, eFuse and secure-boot tools, ...) is refused by
/// omission.
const ALLOWED_SUBCOMMANDS: &[&str] = &[
    "chip_id",
    "flash_id",
    "read_flash",
    "write_flash",
    "verify_flash",
    "erase_region",
    "version",
];

fn normalize(word: &str) -> String {
    word.replace('-', "_")
}

/// A strict flash address or length: `0x` and one to eight hex digits.
fn parse_u32_hex(word: &str) -> Option<u32> {
    let digits = word.strip_prefix("0x")?;
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// A port value: printable ASCII with no spaces, not empty and not an option, so a look-alike
/// dash is refused.
fn is_port_value(word: &str) -> bool {
    !word.is_empty() && !word.starts_with('-') && word.bytes().all(|b| b.is_ascii_graphic())
}

fn is_file_operand(word: &str) -> bool {
    !word.is_empty() && !word.starts_with('-') && !word.chars().any(char::is_control)
}

fn check_flash_range(what: &str, start: u64, end: u64) -> Result<(), String> {
    let (s, e) = round_to_sectors(start, end);
    if touches_cardid(s, e) {
        return Err(format!("{what} [{s:#x}, {e:#x}) touches cardid"));
    }
    if e > u64::from(FLASH_SIZE) {
        return Err(format!("{what} [{s:#x}, {e:#x}) passes 8 MB"));
    }
    Ok(())
}

/// Checks an argument vector against the allow list, as the last gate before a spawn:
/// - the program is exactly the resolved esptool or Python interpreter `command.program`, and the
///   arguments start with exactly its prefix (`-m esptool` for Python);
/// - the fixed global options `--chip esp32c3`, `--port <port>`, `--before usb_reset` and
///   `--after no_reset|hard_reset`, each once and all required unless the subcommand is `version`,
///   and optionally `--baud` with exactly [`ESPTOOL_BAUD`], once;
/// - exactly one allowed subcommand with its fixed shape: `write_flash --flash_size keep
///   (<addr> <file>)+`, `verify_flash (<addr> <file>)+`, `read_flash <addr> <len> <file>`,
///   `erase_region <addr> <len>`, nothing after `chip_id`, `flash_id` or `version`;
/// - every address and length is strict `u32` hex, and every range, rounded to sectors, stays
///   below 8 MB and off cardid [0x356000, 0x35A000). A write or verify range is the offset plus
///   the file's length from `file_len`;
/// - every file operand is a file of this run: `file_len` answers only for the runner's own
///   owner-only scratch directory, so a `read_flash` destination is never a path the vector chose.
pub fn check_invocation(
    command: &EsptoolCommand,
    invocation: &Invocation,
    file_len: &dyn Fn(&str) -> Option<u64>,
) -> Result<(), Refusal> {
    // The region-MD5 helper is the one other vector a session may spawn, with its own gate.
    if crate::stub_md5::is_helper_invocation(invocation) {
        return crate::stub_md5::check_md5_invocation(command, invocation, file_len);
    }
    let refuse = |why: String| {
        Err(Refusal::new(
            Rule::EsptoolNotAllowed,
            format!("esptool argument vector refused: {why}"),
        ))
    };
    if invocation.program != command.program {
        return refuse("the program is not the resolved esptool".to_owned());
    }
    let name = command
        .program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.contains("espefuse") || name.contains("espsecure") {
        return refuse(format!("`{name}` is not esptool"));
    }
    let Some(args) = invocation.args.strip_prefix(command.prefix.as_slice()) else {
        return refuse("the arguments do not start with the resolved esptool prefix".to_owned());
    };
    let mut seen = [false; 5];
    let mut i = 0;
    while i < args.len() && args[i].starts_with("--") {
        let option = normalize(&args[i]);
        let Some(value) = args.get(i + 1).map(String::as_str) else {
            return refuse(format!("`{}` has no value", args[i]));
        };
        let ok = match option.as_str() {
            "__chip" => value == "esp32c3" && !std::mem::replace(&mut seen[0], true),
            "__port" => is_port_value(value) && !std::mem::replace(&mut seen[1], true),
            "__before" => normalize(value) == "usb_reset" && !std::mem::replace(&mut seen[2], true),
            "__after" => {
                matches!(normalize(value).as_str(), "no_reset" | "hard_reset")
                    && !std::mem::replace(&mut seen[3], true)
            }
            "__baud" => value == ESPTOOL_BAUD && !std::mem::replace(&mut seen[4], true),
            _ => false,
        };
        if !ok {
            // The option's name only: `--port`'s value is a host device path, and an agent reads
            // refusals.
            return refuse(format!("option `{}`", args[i]));
        }
        i += 2;
    }
    let Some(sub) = args.get(i) else {
        return refuse("no subcommand".to_owned());
    };
    let sub = normalize(sub);
    if !ALLOWED_SUBCOMMANDS.contains(&sub.as_str()) {
        return refuse(format!("subcommand `{}`", args[i]));
    }
    if sub != "version" && seen[..4] != [true; 4] {
        return refuse("`--chip`, `--port`, `--before` and `--after` are all required".to_owned());
    }
    let rest = &args[i + 1..];
    let bad_shape = || refuse(format!("the arguments of `{}`", args[i]));
    let pairs = |rest: &[String]| -> Result<(), Refusal> {
        if rest.is_empty() || !rest.len().is_multiple_of(2) {
            return bad_shape();
        }
        for pair in rest.chunks(2) {
            let (Some(offset), true) = (parse_u32_hex(&pair[0]), is_file_operand(&pair[1])) else {
                return bad_shape();
            };
            let start = u64::from(offset);
            // Checking the offset's own sector alone would let a file of any size past the cardid
            // rule, so the length must be known.
            let Some(len) = file_len(&pair[1]) else {
                return refuse(format!(
                    "the file operand of `{sub}` is not a scratch file of this run, so its length \
                     is unknown"
                ));
            };
            let end = start + len.max(1);
            if let Err(why) = check_flash_range(&sub, start, end) {
                return refuse(why);
            }
        }
        Ok(())
    };
    match sub.as_str() {
        "chip_id" | "flash_id" | "version" if rest.is_empty() => Ok(()),
        "write_flash"
            if rest.len() > 2 && normalize(&rest[0]) == "__flash_size" && rest[1] == "keep" =>
        {
            pairs(&rest[2..])
        }
        "verify_flash" => pairs(rest),
        "read_flash" | "erase_region" => {
            let operands = if sub == "read_flash" { 3 } else { 2 };
            if rest.len() != operands || (operands == 3 && !is_file_operand(&rest[2])) {
                return bad_shape();
            }
            // A full read holds cardid and the `nvs` credentials, so it must land in a file the
            // session created in its 0700 scratch directory.
            if sub == "read_flash" && file_len(&rest[2]).is_none() {
                return refuse(
                    "the destination of `read_flash` is not a scratch file of this run".to_owned(),
                );
            }
            let (Some(offset), Some(size)) = (parse_u32_hex(&rest[0]), parse_u32_hex(&rest[1]))
            else {
                return bad_shape();
            };
            if size == 0 {
                return bad_shape();
            }
            // The one read that may cover cardid: exactly the whole part, for `--backup`, into an
            // owner-only file outside the repository.
            if sub == "read_flash" && offset == 0 && size == FLASH_SIZE {
                return Ok(());
            }
            let start = u64::from(offset);
            check_flash_range(&sub, start, start + u64::from(size)).or_else(refuse)
        }
        _ => bad_shape(),
    }
}

pub struct EsptoolSession<'r> {
    command: EsptoolCommand,
    port: String,
    runner: &'r mut dyn ProcessRunner,
    md5: Option<&'r mut dyn RegionMd5>,
    console: Option<Box<dyn BootConsole + 'r>>,
    mac_salt: Option<[u8; 32]>,
    files: Vec<(String, u64)>,
    /// Every argument vector run, for the rehearsal receipt.
    pub ran: Vec<Invocation>,
}

impl<'r> EsptoolSession<'r> {
    /// A session on an emulator instance: only `rfc2217://127.0.0.1:<port>` is accepted.
    pub fn emulator(
        command: EsptoolCommand,
        url: &str,
        runner: &'r mut dyn ProcessRunner,
        md5: &'r mut dyn RegionMd5,
        console: &'r mut dyn BootConsole,
    ) -> Result<EsptoolSession<'r>, Refusal> {
        check_emulator_port(url, PortUse::Flash)?;
        Ok(EsptoolSession {
            command,
            port: url.to_owned(),
            runner,
            md5: Some(md5),
            console: Some(Box::new(console)),
            mac_salt: None,
            files: Vec::new(),
            ran: Vec::new(),
        })
    }

    pub fn device(
        command: EsptoolCommand,
        confirmed: &Confirmed,
        runner: &'r mut dyn ProcessRunner,
        md5: Option<&'r mut dyn RegionMd5>,
        console: Option<Box<dyn BootConsole + 'r>>,
    ) -> EsptoolSession<'r> {
        EsptoolSession {
            command,
            port: confirmed.port().to_owned(),
            runner,
            md5,
            console,
            mac_salt: None,
            files: Vec::new(),
            ran: Vec::new(),
        }
    }

    /// Keys the backup directory by SHA-256(salt, MAC). The salt is per installation; the MAC is
    /// never kept.
    pub fn with_mac_salt(mut self, salt: [u8; 32]) -> EsptoolSession<'r> {
        self.mac_salt = Some(salt);
        self
    }

    fn spell(&self, word: &str) -> String {
        self.command.spell(word)
    }

    fn invocation(&self, after: &str, sub: &[String]) -> Invocation {
        self.command.invocation(&self.port, after, sub)
    }

    fn run(&mut self, after: &str, sub: &[String]) -> Result<String, SessionError> {
        let invocation = self.invocation(after, sub);
        let files = &self.files;
        let file_len = |path: &str| files.iter().find(|(p, _)| p == path).map(|(_, n)| *n);
        check_invocation(&self.command, &invocation, &file_len)
            .map_err(|r| SessionError::Failed(r.to_string()))?;
        self.ran.push(invocation.clone());
        let out = self.runner.run(&invocation).map_err(SessionError::Failed)?;
        if !out.success {
            let lower = out.output.to_ascii_lowercase();
            if lower.contains("busy") || lower.contains("resource temporarily unavailable") {
                return Err(SessionError::Busy);
            }
            // esptool's own last lines tell a too-large app from a bad port. The tail only, since
            // esptool prints a progress line per block, and masked, since the output can carry
            // the MAC.
            let tail: String = out
                .output
                .lines()
                .map(crate::console::mask_mac_shapes)
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("; ");
            return Err(SessionError::Failed(format!(
                "esptool {} failed: {tail}",
                sub.first().map_or("", String::as_str)
            )));
        }
        Ok(out.output)
    }

    /// One region digest on the device through [`crate::stub_md5::HELPER_SOURCE`], written to
    /// scratch and registered in `files` so the gate recognizes it by length.
    fn stub_region_md5(&mut self, offset: u32, size: u32) -> Result<[u8; 16], SessionError> {
        if self.command.prefix.first().map(String::as_str) != Some("-I") {
            return Err(SessionError::Unsupported(
                "the region MD5 needs esptool as a Python library; resolve a \
                 Python interpreter with `--esptool <python>`"
                    .to_owned(),
            ));
        }
        let path = self.runner.scratch_path(crate::stub_md5::HELPER_NAME);
        self.runner
            .write_file(&path, crate::stub_md5::HELPER_SOURCE.as_bytes())
            .map_err(SessionError::Failed)?;
        self.files
            .push((path.clone(), crate::stub_md5::HELPER_SOURCE.len() as u64));
        let invocation =
            crate::stub_md5::helper_invocation(&self.command, &path, &self.port, offset, size);
        let files = &self.files;
        let file_len = |path: &str| files.iter().find(|(p, _)| p == path).map(|(_, n)| *n);
        let result = crate::stub_md5::check_md5_invocation(&self.command, &invocation, &file_len)
            .map_err(|r| SessionError::Failed(r.to_string()))
            .and_then(|()| {
                self.ran.push(invocation.clone());
                self.runner
                    .run(&invocation)
                    .map_err(SessionError::Failed)
                    .and_then(|out| {
                        if out.success {
                            crate::stub_md5::parse_digest(&out.output)
                        } else {
                            Err(SessionError::Failed(
                                "the region-MD5 helper failed".to_owned(),
                            ))
                        }
                    })
            });
        self.files.retain(|(p, _)| p != &path);
        self.runner.remove_file(&path);
        result
    }

    /// Claims the destination of a `read_flash`: an empty scratch file registered in `files`, so
    /// the gate sees the read lands in a file of this run. esptool truncates it anyway.
    fn claim_destination(&mut self, name: &str) -> Result<String, SessionError> {
        let path = self.runner.scratch_path(name);
        self.runner
            .write_file(&path, &[])
            .map_err(SessionError::Failed)?;
        self.files.push((path.clone(), 0));
        Ok(path)
    }

    fn release_destination(&mut self, path: &str) {
        self.files.retain(|(p, _)| p != path);
        self.runner.remove_file(path);
    }

    fn refuse_cardid(offset: u32, size: u32) -> Result<(), SessionError> {
        let (s, e) = round_to_sectors(u64::from(offset), u64::from(offset) + u64::from(size));
        if touches_cardid(s, e) {
            return Err(SessionError::Failed(
                "a read, write or erase touching cardid is never run".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Reads the chip name and revision from `chip_id` output (`ESP32-C3 (QFN32) (revision v1.1)`).
pub fn parse_chip(output: &str) -> Option<(String, ChipRevision)> {
    output.lines().find_map(|line| {
        if !line.contains("ESP32-C3") {
            return None;
        }
        let (_, rest) = line.split_once("(revision ")?;
        let token: String = rest
            .chars()
            .take_while(|c| *c == 'v' || *c == '.' || c.is_ascii_digit())
            .collect();
        Some(("ESP32-C3".to_owned(), ChipRevision::parse(&token)?))
    })
}

/// Reads the six MAC bytes from a `MAC: xx:xx:xx:xx:xx:xx` line. The bytes are hashed at once and
/// never stored.
pub fn parse_mac(output: &str) -> Option<[u8; 6]> {
    let line = output.lines().find_map(|l| l.trim().strip_prefix("MAC:"))?;
    let mut mac = [0u8; 6];
    let mut parts = line.trim().split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// Reads manufacturer and device from `flash_id` lines that start with `Manufacturer:` and
/// `Device:`, so the RFC 2217 warning "Device PID identification is only supported on ..." is not
/// read as the device id. UNVERIFIED: the line wording.
pub fn parse_flash_id(output: &str) -> Option<(u8, u16)> {
    let field = |key: &str| -> Option<u64> {
        let mut found = output
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix(key));
        let value = found.next()?.trim();
        if found.next().is_some() {
            return None;
        }
        let digits = value.strip_prefix("0x").unwrap_or(value);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u64::from_str_radix(digits, 16).ok()
    };
    Some((
        u8::try_from(field("Manufacturer:")?).ok()?,
        u16::try_from(field("Device:")?).ok()?,
    ))
}

impl DeviceSession for EsptoolSession<'_> {
    fn identify(&mut self) -> Result<Identity, SessionError> {
        let chip_output = self.run("no_reset", &[self.spell("chip_id")])?;
        let (chip, revision) = parse_chip(&chip_output)
            .ok_or_else(|| SessionError::Failed("no chip revision in chip_id".to_owned()))?;
        let flash = self.run("no_reset", &[self.spell("flash_id")])?;
        let (flash_manufacturer, flash_device) = parse_flash_id(&flash)
            .ok_or_else(|| SessionError::Failed("no flash id in flash_id".to_owned()))?;
        let device_key = match (self.mac_salt, parse_mac(&chip_output)) {
            (Some(salt), Some(mac)) => {
                let mut input = salt.to_vec();
                input.extend_from_slice(&mac);
                let digest = sha256(&input);
                let mut key = [0u8; 16];
                key.copy_from_slice(&digest[..16]);
                Some(key)
            }
            _ => None,
        };
        Ok(Identity {
            chip,
            revision,
            flash_manufacturer,
            flash_device,
            device_key,
        })
    }

    fn read_partition_table(&mut self) -> Result<Vec<u8>, SessionError> {
        self.read_region(TABLE_OFFSET, TABLE_LEN)
    }

    fn region_md5(&mut self, offset: u32, size: u32) -> Result<[u8; 16], SessionError> {
        match self.md5.as_mut() {
            Some(source) => source.region_md5(offset, size),
            None => self.stub_region_md5(offset, size),
        }
    }

    fn read_region(&mut self, offset: u32, size: u32) -> Result<Vec<u8>, SessionError> {
        Self::refuse_cardid(offset, size)?;
        let path = self.claim_destination(&format!("read-{offset:x}.bin"))?;
        let sub = [
            self.spell("read_flash"),
            format!("{offset:#x}"),
            format!("{size:#x}"),
            path.clone(),
        ];
        let result = self
            .run("no_reset", &sub)
            .and_then(|_| self.runner.read_file(&path).map_err(SessionError::Failed));
        self.release_destination(&path);
        result
    }

    fn read_full_flash(&mut self) -> Result<Vec<u8>, SessionError> {
        let path = self.claim_destination("full.bin")?;
        let sub = [
            self.spell("read_flash"),
            "0x0".to_owned(),
            format!("{FLASH_SIZE:#x}"),
            path.clone(),
        ];
        let result = self
            .run("no_reset", &sub)
            .and_then(|_| self.runner.read_file(&path).map_err(SessionError::Failed));
        self.release_destination(&path);
        let bytes = result?;
        if bytes.len() != FLASH_SIZE as usize {
            return Err(SessionError::Failed(format!(
                "the full backup read {} bytes instead of {FLASH_SIZE}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    fn write(&mut self, writes: &[PlannedWrite]) -> Result<(), SessionError> {
        let mut sub = vec![
            self.spell("write_flash"),
            self.spell("--flash_size"),
            "keep".to_owned(),
        ];
        let mut paths = Vec::new();
        for w in writes {
            Self::refuse_cardid(w.offset, w.data.len() as u32)?;
            let path = self
                .runner
                .scratch_path(&format!("{}.bin", hex(&w.sha256[..8])));
            self.runner
                .write_file(&path, &w.data)
                .map_err(SessionError::Failed)?;
            sub.push(format!("{:#x}", w.offset));
            sub.push(path.clone());
            self.files.push((path.clone(), w.data.len() as u64));
            paths.push(path);
        }
        let result = self.run("no_reset", &sub).map(|_| ());
        self.files.clear();
        for path in &paths {
            self.runner.remove_file(path);
        }
        result
    }

    fn erase_region(&mut self, offset: u32, size: u32) -> Result<(), SessionError> {
        Self::refuse_cardid(offset, size)?;
        let sub = [
            self.spell("erase_region"),
            format!("{offset:#x}"),
            format!("{size:#x}"),
        ];
        self.run("no_reset", &sub).map(|_| ())
    }

    fn verify(&mut self, writes: &[PlannedWrite]) -> Result<(), SessionError> {
        let mut sub = vec![self.spell("verify_flash")];
        let mut paths = Vec::new();
        for w in writes {
            let path = self
                .runner
                .scratch_path(&format!("verify-{}.bin", hex(&w.sha256[..8])));
            self.runner
                .write_file(&path, &w.data)
                .map_err(SessionError::Failed)?;
            sub.push(format!("{:#x}", w.offset));
            sub.push(path.clone());
            self.files.push((path.clone(), w.data.len() as u64));
            paths.push(path);
        }
        let result = self.run("no_reset", &sub).map(|_| ());
        self.files.clear();
        for path in &paths {
            self.runner.remove_file(path);
        }
        result
    }

    fn hard_reset(&mut self) -> Result<(), SessionError> {
        // `--after hard_reset chip_id` returns the chip to a normal boot.
        self.run("hard_reset", &[self.spell("chip_id")]).map(|_| ())
    }

    fn boot_log(&mut self) -> Result<String, SessionError> {
        match self.console.as_mut() {
            Some(console) => console.boot_log(),
            None => Err(SessionError::Unsupported(
                "no boot console reader for this session".to_owned(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_failed_esptool_run_quotes_its_own_last_lines_with_the_mac_masked() {
        // Assembled at run time: `xtask secrets-check` refuses any MAC-shaped literal.
        let fake_mac = ["de", "ad", "be", "ef", "00", "01"].join(":");
        let raw = format!(
            "Connecting...\nChip is ESP32-C3\nMAC: {fake_mac}\n\nA fatal error occurred: Packet content transfer stopped"
        );
        let tail: String = raw
            .lines()
            .map(crate::console::mask_mac_shapes)
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("; ");
        assert!(tail.contains("A fatal error occurred"), "{tail}");
        assert!(tail.contains("<MAC>"), "the MAC is masked: {tail}");
        assert!(
            !tail.contains(&fake_mac),
            "no MAC digit reaches the message: {tail}"
        );
        assert!(
            !tail.contains(";;"),
            "blank lines are dropped, not joined: {tail}"
        );
    }

    use super::*;
    use crate::flow::Confirmed;

    struct Recorder;

    impl ProcessRunner for Recorder {
        fn run(&mut self, invocation: &Invocation) -> Result<ProcessOutput, String> {
            // By name rather than by index, so added global options cannot shift it.
            let sub = invocation
                .args
                .iter()
                .map(|w| w.replace('-', "_"))
                .find(|w| w == "chip_id" || w == "flash_id")
                .unwrap_or_default();
            let output = match sub.as_str() {
                "chip_id" => "Chip is ESP32-C3 (QFN32) (revision v1.1)\n",
                "flash_id" => "Manufacturer: 20\nDevice: 4017\n",
                _ => "",
            };
            Ok(ProcessOutput {
                success: true,
                output: output.to_owned(),
            })
        }
        fn scratch_path(&mut self, name: &str) -> String {
            format!("scratch/{name}")
        }
        fn write_file(&mut self, _: &str, _: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn read_file(&mut self, _: &str) -> Result<Vec<u8>, String> {
            Ok(vec![0xFF; 0xC00])
        }
        fn remove_file(&mut self, _: &str) {}
    }

    struct NoMd5;
    impl RegionMd5 for NoMd5 {
        fn region_md5(&mut self, _: u32, _: u32) -> Result<[u8; 16], SessionError> {
            Ok([0; 16])
        }
    }
    struct NoConsole;
    impl BootConsole for NoConsole {
        fn boot_log(&mut self) -> Result<String, SessionError> {
            Ok(String::new())
        }
    }

    fn every_operation(session: &mut dyn DeviceSession, writes: &[PlannedWrite]) {
        session.identify().expect("identify");
        session.read_partition_table().expect("table");
        session.read_region(0x1_0000, 0x1000).expect("backup read");
        session.write(writes).expect("write");
        session.verify(writes).expect("verify");
        session.erase_region(0x9000, 0x6000).expect("erase nvs");
        session.hard_reset().expect("reset");
    }

    /// A runner that answers only the region-MD5 helper with a real stub transcript, and keeps the
    /// scratch file so a test can see the helper written and removed.
    #[derive(Default)]
    struct StubMd5Runner {
        written: Vec<(String, usize)>,
        removed: Vec<String>,
        digest: String,
    }

    impl ProcessRunner for StubMd5Runner {
        fn run(&mut self, invocation: &Invocation) -> Result<ProcessOutput, String> {
            assert!(crate::stub_md5::is_helper_invocation(invocation));
            Ok(ProcessOutput {
                success: true,
                output: format!(
                    "esptool.py v4.12.0\nChip is ESP32-C3 (QFN32) (revision v1.1)\n\
                     Uploading stub...\nRunning stub...\nStub running...\n\
                     SPI_FLASH_MD5 {}\n",
                    self.digest
                ),
            })
        }
        fn scratch_path(&mut self, name: &str) -> String {
            format!("/scratch/1-{name}")
        }
        fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
            self.written.push((path.to_owned(), bytes.len()));
            Ok(())
        }
        fn read_file(&mut self, _: &str) -> Result<Vec<u8>, String> {
            Err("the helper reads no file".to_owned())
        }
        fn remove_file(&mut self, path: &str) {
            self.removed.push(path.to_owned());
        }
    }

    fn python_command() -> EsptoolCommand {
        EsptoolCommand {
            program: "/env/idf/bin/python".to_owned(),
            prefix: vec!["-I".to_owned(), "-m".to_owned(), "esptool".to_owned()],
            major: 4,
        }
    }

    #[test]
    fn a_device_session_takes_the_cardid_digest_from_the_stub_transcript() {
        let mut runner = StubMd5Runner {
            digest: "9e107d9d372bb6826bd81d3542a419d6".to_owned(),
            ..StubMd5Runner::default()
        };
        let confirmed = Confirmed::for_crate_tests("device-port-under-test", [7; 32]);
        let mut session =
            EsptoolSession::device(python_command(), &confirmed, &mut runner, None, None);
        let digest = session
            .region_md5(crate::rules::CARDID_OFFSET, crate::rules::CARDID_SIZE)
            .expect("the stub answered");
        assert_eq!(hex(&digest), "9e107d9d372bb6826bd81d3542a419d6");
        assert_eq!(session.ran.len(), 1);
        let ran = session.ran.clone();
        assert_eq!(
            ran[0].args,
            [
                "-I",
                "/scratch/1-region-md5.py",
                "--port",
                "device-port-under-test",
                "--addr",
                "0x356000",
                "--size",
                "0x4000"
            ]
        );
        drop(session);
        assert_eq!(
            runner.written,
            [(
                "/scratch/1-region-md5.py".to_owned(),
                crate::stub_md5::HELPER_SOURCE.len()
            )]
        );
        assert_eq!(runner.removed, ["/scratch/1-region-md5.py"]);
    }

    #[test]
    fn an_esptool_executable_cannot_take_the_digest_and_spawns_nothing() {
        let mut runner = StubMd5Runner::default();
        let confirmed = Confirmed::for_crate_tests("device-port-under-test", [7; 32]);
        let mut session = EsptoolSession::device(
            EsptoolCommand {
                program: "/env/idf/bin/esptool".to_owned(),
                prefix: Vec::new(),
                major: 5,
            },
            &confirmed,
            &mut runner,
            None,
            None,
        );
        let error = session
            .region_md5(crate::rules::CARDID_OFFSET, crate::rules::CARDID_SIZE)
            .expect_err("no interpreter");
        assert!(
            matches!(&error, SessionError::Unsupported(why) if why.contains("--esptool")),
            "{error:?}"
        );
        assert!(session.ran.is_empty());
        drop(session);
        assert!(runner.written.is_empty());
    }

    #[test]
    fn the_device_and_the_rehearsal_run_identical_vectors_but_the_port() {
        let writes = vec![PlannedWrite {
            name: "factory".to_owned(),
            offset: 0x1_0000,
            data: vec![0xE9, 1, 2, 3],
            sha256: [9; 32],
        }];
        for major in [4, 5] {
            let command = EsptoolCommand {
                program: "/env/idf/bin/python".to_owned(),
                prefix: vec!["-m".to_owned(), "esptool".to_owned()],
                major,
            };
            let emulator_port = "rfc2217://127.0.0.1:4242";
            let device_port = "device-port-under-test";
            let (mut r1, mut m1, mut c1) = (Recorder, NoMd5, NoConsole);
            let mut emulator =
                EsptoolSession::emulator(command.clone(), emulator_port, &mut r1, &mut m1, &mut c1)
                    .expect("allowed");
            every_operation(&mut emulator, &writes);
            let confirmed = Confirmed::for_crate_tests(device_port, [1; 32]);
            let mut r2 = Recorder;
            let mut device = EsptoolSession::device(command, &confirmed, &mut r2, None, None);
            every_operation(&mut device, &writes);
            assert_eq!(emulator.ran.len(), device.ran.len());
            assert!(emulator.ran.len() >= 8);
            for (e, d) in emulator.ran.iter().zip(&device.ran) {
                let swapped: Vec<String> = e
                    .args
                    .iter()
                    .map(|a| {
                        if a == emulator_port {
                            device_port.to_owned()
                        } else {
                            a.clone()
                        }
                    })
                    .collect();
                assert_eq!(e.program, d.program);
                assert_eq!(swapped, d.args, "major {major}");
            }
        }
    }
}
