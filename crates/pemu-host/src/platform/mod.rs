//! Everything that differs per host lives behind the traits of this module, with one
//! implementation per host and a fake for tests, so no other crate carries a `cfg`: owner-only
//! create and check, the confirmation dialog, interactive-console detection, serial enumeration
//! without opening, crash-report opt-out, detached daemon spawn and the machine-thread class.
//!
//! [`host`] returns [`macos::MacOs`] on macOS, `windows::Windows` on Windows, and on any other
//! host a value that refuses every call with [`PlatformError::Unsupported`]. Tests take
//! [`fake::FakeHost`] instead, which is why every service is a trait. `windows-sys` is used only
//! from `platform::windows` and `paths`.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

pub mod fake;
#[cfg(target_os = "macos")]
pub mod macos;
// Serial enumeration without opening a port. Its own module doc carries the rule; an outer doc
// line here would be resolved in this module's scope and break its links.
#[cfg(feature = "device")]
pub mod serial;
#[cfg(windows)]
pub mod windows;

#[derive(Debug)]
pub enum PlatformError {
    /// The path exists but is not owner-only. `detail` says how: group or other mode bits on
    /// macOS, a foreign ACE or inheritance on Windows.
    NotOwnerOnly {
        path: PathBuf,
        /// How it is wider than owner-only.
        detail: String,
    },
    /// A host call failed. The path is carried because `io::Error` does not hold one.
    Io {
        /// The path the call was made on, or the empty path when the call names none.
        path: PathBuf,
        source: io::Error,
    },
    Unsupported(&'static str),
    /// The service exists on this host but its body is not written. Distinct from
    /// [`PlatformError::Unsupported`] so a caller maps it to `E_INTERNAL`, not
    /// `E_HOST_UNSUPPORTED`, which would send the person to another host.
    NotImplementedYet(&'static str),
}

impl PlatformError {
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> PlatformError {
        PlatformError::Io {
            path: path.into(),
            source,
        }
    }

    pub fn wider(path: impl Into<PathBuf>, detail: impl Into<String>) -> PlatformError {
        PlatformError::NotOwnerOnly {
            path: path.into(),
            detail: detail.into(),
        }
    }

    pub fn not_implemented_yet(what: &'static str) -> PlatformError {
        PlatformError::NotImplementedYet(what)
    }

    /// Whether this is the "no implementation on this host" arm, which callers map to
    /// `E_HOST_UNSUPPORTED`. [`PlatformError::NotImplementedYet`] is excluded.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, PlatformError::Unsupported(_))
    }

    pub fn is_not_implemented_yet(&self) -> bool {
        matches!(self, PlatformError::NotImplementedYet(_))
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlatformError::NotOwnerOnly { path, detail } => {
                write!(f, "`{}` is not owner-only: {detail}", path.display())
            }
            PlatformError::Io { path, source } => {
                if path.as_os_str().is_empty() {
                    write!(f, "{source}")
                } else {
                    write!(f, "`{}`: {source}", path.display())
                }
            }
            PlatformError::Unsupported(what) => {
                write!(f, "this host has no implementation: {what}")
            }
            PlatformError::NotImplementedYet(what) => {
                write!(f, "this build has no implementation yet: {what}")
            }
        }
    }
}

impl std::error::Error for PlatformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlatformError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Opens an existing file for reading without following a symlink at its last component, and
/// only when the opened file is the regular file `expected` (its prior `symlink_metadata`)
/// described, so a path swapped between the check and the open is refused:
///
/// - macOS: `O_NOFOLLOW | O_NONBLOCK` (a FIFO does not block the open), then the handle's
///   `(dev, ino)` must equal `expected`'s and be a regular file;
/// - Windows: `FILE_FLAG_OPEN_REPARSE_POINT`, so a link is opened as itself, then the handle must
///   be a regular file of `expected`'s length. Symlinks and junctions are refused; a swap for
///   another regular file of the same length is not caught, since stable std exposes no file
///   index;
/// - any other host: a plain open and the regular-file check.
///
/// A refusal is an `InvalidInput` error whose text names no path.
pub fn open_regular_no_follow(path: &Path, expected: &std::fs::Metadata) -> io::Result<File> {
    let refused = || io::Error::new(io::ErrorKind::InvalidInput, "not the regular file checked");
    if !expected.file_type().is_file() {
        return Err(refused());
    }
    #[cfg(target_os = "macos")]
    let file = macos::open_read_no_follow(path, expected)?;
    #[cfg(windows)]
    let file = windows::open_read_no_follow(path, expected)?;
    #[cfg(not(any(target_os = "macos", windows)))]
    let file = File::open(path)?;
    let opened = file.metadata()?;
    if !opened.file_type().is_file() || opened.len() != expected.len() {
        return Err(refused());
    }
    Ok(file)
}

/// Connects to a loopback `addr` within `timeout`, with a closed port refused at once on both
/// hosts. Windows retransmits the SYN after the reset for about 2 s, which would report a refusal
/// as a timeout, so its arm turns SYN retransmission off first (`windows::net`).
pub fn connect_loopback(
    addr: &std::net::SocketAddr,
    timeout: std::time::Duration,
) -> io::Result<std::net::TcpStream> {
    #[cfg(windows)]
    {
        windows::net::connect_loopback(addr, timeout)
    }
    #[cfg(not(windows))]
    {
        std::net::TcpStream::connect_timeout(addr, timeout)
    }
}

/// A loopback listener no other socket can bind over. On macOS std's `SO_REUSEADDR` still refuses
/// a second listener without `SO_REUSEPORT`; Windows sets `SO_EXCLUSIVEADDRUSE` before the bind
/// and never `SO_REUSEADDR`.
pub fn listen_loopback(addr: &std::net::SocketAddr) -> io::Result<std::net::TcpListener> {
    #[cfg(windows)]
    {
        windows::net::listen_exclusive(addr)
    }
    #[cfg(not(windows))]
    {
        std::net::TcpListener::bind(addr)
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
const UNSUPPORTED_HOST: &str = "the platform services are defined for macOS and Windows only";

/// A fresh, unique directory under the host temporary directory, for a test that needs real files.
/// Deliberately not a directory role: a `cfg(test)` build must never reach a real role directory.
#[cfg(test)]
pub(crate) fn scratch_dir(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("pemu-host-test-{name}-{}-{n}", std::process::id()))
}

/// Owner-only creation and checking. Every private file and directory (daemon token, discovery
/// file, logs, boot cache, planner backups) is created owner-only, never made private afterwards,
/// and re-checked on every read.
///
/// | Host | Creation | Read check |
/// |---|---|---|
/// | macOS | files 0600 through `OpenOptionsExt::mode`, directories 0700 through `DirBuilderExt::mode` | owner uid equals the current uid and `mode & 0o077 == 0` |
/// | Windows | a protected DACL with inheritance disabled granting the current user SID and SYSTEM, applied at creation | `GetNamedSecurityInfoW`: no ACE other than the current user and SYSTEM, and inheritance disabled |
///
/// On Windows, Administrators and holders of `SeBackupPrivilege` can still read the file, the same
/// class of exposure as root on macOS.
pub trait OwnerOnly: Send + Sync {
    /// Creates `path` as an owner-only directory. An existing one that passes
    /// [`OwnerOnly::check`] is fine; a widened one refuses.
    fn create_dir(&self, path: &Path) -> Result<(), PlatformError>;

    /// Creates one new owner-only directory, exclusively and non-recursively: an `AlreadyExists`
    /// [`PlatformError::Io`] when anything is there, so a caller wanting a fresh directory per run
    /// can try the next name.
    fn create_new_dir(&self, path: &Path) -> Result<(), PlatformError>;

    /// Creates or truncates `path` as an owner-only file, opened for writing. The protection is
    /// part of the create call; an existing file is re-checked and refused if widened, never
    /// repaired.
    fn create_file(&self, path: &Path) -> Result<File, PlatformError>;

    /// Creates `path` as a new owner-only file and refuses when anything is already there, so two
    /// writers never share one temporary file.
    fn create_new_file(&self, path: &Path) -> Result<File, PlatformError>;

    fn check(&self, path: &Path) -> Result<(), PlatformError>;
}

/// The detached daemon spawn. An auto-spawned `serve --headless` must survive the shell, terminal
/// and agent harness that started it.
///
/// | Host | Detachment |
/// |---|---|
/// | macOS | a new session: `setsid` in `pre_exec` |
/// | Windows | `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, one attempt with `CREATE_BREAKAWAY_FROM_JOB` and a retry without it, never a service; a raw `CreateProcessW` whose `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` names the three null-device handles and nothing else |
///
/// All three stdio handles go to the null device and the daemon inherits no other handle, so it
/// never holds an MCP client's stdout or a shell's output pipe open (on Windows
/// `std::process::Command` passes every inheritable handle through). The daemon writes its own log.
pub trait DaemonSpawn: Send + Sync {
    fn spawn_detached(&self, command: &mut Command) -> Result<DetachedChild, PlatformError>;
}

/// A child started by [`DaemonSpawn::spawn_detached`]. The parent may drop it and exit; a parent
/// that stays alive calls [`DetachedChild::wait`] so no zombie entry lingers.
#[derive(Debug)]
pub struct DetachedChild {
    pid: u32,
    child: Spawned,
}

#[derive(Debug)]
enum Spawned {
    Std(std::process::Child),
    #[cfg(windows)]
    Handle(std::os::windows::io::OwnedHandle),
}

impl DetachedChild {
    pub(crate) fn new(child: std::process::Child) -> DetachedChild {
        DetachedChild {
            pid: child.id(),
            child: Spawned::Std(child),
        }
    }

    #[cfg(windows)]
    pub(crate) fn from_handle(
        pid: u32,
        process: std::os::windows::io::OwnedHandle,
    ) -> DetachedChild {
        DetachedChild {
            pid,
            child: Spawned::Handle(process),
        }
    }

    /// For a log only: staleness is decided by connecting, since pids are recycled.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn wait(&mut self) -> Result<std::process::ExitStatus, PlatformError> {
        match &mut self.child {
            Spawned::Std(child) => child.wait(),
            #[cfg(windows)]
            Spawned::Handle(process) => windows::process::wait(process),
        }
        .map_err(|e| PlatformError::io(PathBuf::new(), e))
    }

    /// Ends the child (a test, or a parent giving up on a daemon it just started); an ordinary
    /// shutdown uses `POST /v1/shutdown`.
    pub fn kill(&mut self) -> Result<(), PlatformError> {
        match &mut self.child {
            Spawned::Std(child) => child.kill(),
            #[cfg(windows)]
            Spawned::Handle(process) => windows::process::kill(process),
        }
        .map_err(|e| PlatformError::io(PathBuf::new(), e))
    }
}

/// A shutdown signal, named host-neutrally so a caller can log which one arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownSignal {
    /// `SIGINT` on macOS, `CTRL_C_EVENT` on Windows: the interrupt a person types.
    Interrupt,
    /// `SIGTERM` on macOS. Windows has none, so `POST /v1/shutdown` is the portable path.
    Terminate,
    Break,
    /// `CTRL_CLOSE_EVENT` on Windows: the console window was closed.
    Close,
    /// `CTRL_SHUTDOWN_EVENT` on Windows: the system is shutting down.
    SystemShutdown,
}

/// Set when a shutdown signal arrives. A flag, because a signal handler may only store to one
/// lock-free atomic; `'static` because a POSIX handler can reach only a `static`.
#[derive(Clone, Copy, Debug)]
pub struct ShutdownFlag(&'static AtomicBool);

impl ShutdownFlag {
    pub fn from_static(cell: &'static AtomicBool) -> ShutdownFlag {
        ShutdownFlag(cell)
    }

    /// A fresh cell with a deliberately leaked backing store (one `bool`), for a fake: the cell
    /// must outlive every handler that could reach it.
    pub(crate) fn leaked() -> ShutdownFlag {
        ShutdownFlag(Box::leak(Box::new(AtomicBool::new(false))))
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The signal and shutdown paths. A foreground `serve` stops on its host's signals; the daemon
/// stops over `POST /v1/shutdown`, the path both hosts share (Windows has no `SIGTERM` or `kill`).
pub trait Signals: Send + Sync {
    fn shutdown_signals(&self) -> &'static [ShutdownSignal];

    /// Installs a handler for every signal of [`Signals::shutdown_signals`] and returns the flag
    /// it sets. Installing twice replaces the handler, as the OS does.
    fn install_shutdown(&self) -> Result<ShutdownFlag, PlatformError>;
}

/// Interactive-console detection.
///
/// | Host | Test |
/// |---|---|
/// | macOS | the process has a controlling tty and is in its foreground process group |
/// | Windows | `GetConsoleMode` succeeds on both `CONIN$` and `CONOUT$` |
///
/// A heuristic: a wrapper that allocates a pseudo-terminal (`script`, node-pty, a ConPTY host)
/// looks like a person, so the one-time code path stops mistakes, not a same-user agent.
pub trait Console: Send + Sync {
    fn is_interactive(&self) -> bool;
}

/// Whether the confirmation dialog can be raised at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogAvailability {
    /// An interactive desktop exists: a window-server session on macOS, a session other than
    /// service session 0 on Windows.
    Available,
    /// No dialog can be raised; the command falls through to the next confirmation path or fails,
    /// never proceeding unconfirmed.
    Unavailable(&'static str),
}

/// What a person answered. Deliberately no `Default`, `From<bool>` or "assume yes" constructor:
/// a confirmation that can appear without a person is worthless.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmation {
    Confirmed,
    Declined,
}

/// The confirmation dialog, the only path of a console-less `serve --headless` without MCP
/// elicitation. An implementation reports [`DialogAvailability::Unavailable`] when no interactive
/// desktop exists. It is never auto-answered: not on timeout, not when it cannot be shown, and
/// not in a fake unless the test set the answer. A human gate, not a security boundary.
pub trait Dialog: Send + Sync {
    fn availability(&self) -> DialogAvailability;

    /// Raises the dialog and blocks until the person answers. `title` and `body` are already
    /// redacted.
    fn ask(&self, title: &str, body: &str) -> Result<Confirmation, PlatformError>;
}

/// What the crash-report opt-out did on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashReportOptOut {
    /// Nothing was needed: macOS ReportCrash records stack and registers rather than heap, so a
    /// report does not carry guest RAM (unverified on macOS 27).
    NotNeeded,
    OptedOut,
}

/// The crash-report opt-out, called by the daemon and the CLI at start-up. Guest RAM and flash
/// live in this process's heap, and crash reports must never carry them unless the user asks.
pub trait CrashReports: Send + Sync {
    /// Opts this process out of the host crash reporter, or reports that nothing was needed. The
    /// machine-wide Windows `LocalDumps` policy can still force a dump
    /// ([`CrashReports::forced_dumps`]).
    fn opt_out(&self) -> Result<CrashReportOptOut, PlatformError>;

    /// The machine policy that writes a local dump whatever [`Self::opt_out`] did. Only its
    /// presence and scope are read, so no dump folder reaches a report.
    fn forced_dumps(&self) -> Result<ForcedDumps, PlatformError> {
        Ok(ForcedDumps::NotApplicable)
    }
}

/// The Windows `LocalDumps` policy as far as it concerns this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForcedDumps {
    NotApplicable,
    Absent,
    /// The key exists: its values apply to every executable (Microsoft Learn, "Collecting
    /// User-Mode Dumps").
    EveryExecutable,
    ThisExecutable,
}

impl ForcedDumps {
    pub fn describe(self) -> Option<&'static str> {
        match self {
            ForcedDumps::NotApplicable => None,
            ForcedDumps::Absent => Some("absent"),
            ForcedDumps::EveryExecutable => Some("set for every executable"),
            ForcedDumps::ThisExecutable => {
                Some("set for every executable, with a subkey naming this executable")
            }
        }
    }

    pub fn is_set(self) -> bool {
        matches!(
            self,
            ForcedDumps::EveryExecutable | ForcedDumps::ThisExecutable
        )
    }
}

/// The scheduling class a thread runs at, named after macOS's `qos_class_t` (`sys/qos.h`).
/// [`QosClass::None`] is a host with no such classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QosClass {
    /// `QOS_CLASS_USER_INTERACTIVE`: what a machine thread asks for.
    UserInteractive,
    UserInitiated,
    /// `QOS_CLASS_DEFAULT`: what a spawned macOS thread reads when it asked for nothing, whatever
    /// class its spawner runs at.
    Default,
    Utility,
    /// `QOS_CLASS_BACKGROUND`, which keeps a thread on the efficiency cores of Apple Silicon.
    Background,
    Unspecified,
    Unknown,
    None,
}

impl QosClass {
    pub fn as_str(self) -> &'static str {
        match self {
            QosClass::UserInteractive => "user-interactive",
            QosClass::UserInitiated => "user-initiated",
            QosClass::Default => "default",
            QosClass::Utility => "utility",
            QosClass::Background => "background",
            QosClass::Unspecified => "unspecified",
            QosClass::Unknown => "unknown",
            QosClass::None => "none",
        }
    }
}

/// The scheduling class of threads that run a machine or feed one in real time.
///
/// | Host | Request | Read back |
/// |---|---|---|
/// | macOS | `pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0)` | `qos_class_self()` |
/// | Windows | HighQoS: power throttling off (`SetThreadInformation`) | the throttling state |
/// | any other | nothing (a no-op) | [`QosClass::None`] |
///
/// The read-back is the class granted, not the core used: `taskpolicy -b` or `-c utility` clamps a
/// whole process while its threads still read `user-interactive`, and macOS spreads threads over
/// both clusters once the performance cluster is oversubscribed.
pub trait ThreadQos: Send + Sync {
    fn request_interactive(&self) -> Result<QosClass, PlatformError>;

    fn current(&self) -> Result<QosClass, PlatformError>;
}

pub trait Host: Send + Sync {
    fn owner_only(&self) -> &dyn OwnerOnly;
    fn daemon_spawn(&self) -> &dyn DaemonSpawn;
    fn signals(&self) -> &dyn Signals;
    fn console(&self) -> &dyn Console;
    fn dialog(&self) -> &dyn Dialog;
    fn crash_reports(&self) -> &dyn CrashReports;
    fn thread_qos(&self) -> &dyn ThreadQos;
    /// The OS name and version for `doctor` (`macOS 27.2`, `Windows 10.0.26200`). The default
    /// is the honest answer for a host without an arm.
    fn os_version(&self) -> Result<String, PlatformError> {
        Err(PlatformError::not_implemented_yet(
            "the OS version on this host",
        ))
    }
}

/// The services of the running host. This and the `cfg` arms below are the only place in the
/// workspace that selects a host implementation.
pub fn host() -> &'static dyn Host {
    #[cfg(target_os = "macos")]
    {
        &macos::MacOs
    }
    #[cfg(windows)]
    {
        &windows::Windows
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        &UnsupportedHost
    }
}

/// Whether `path` (followed) belongs to the user running this process: owner uid on macOS; on
/// Windows the owner SID is the token's user or default owner. Any other host answers `false`.
pub fn owned_by_current_user(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::owned_by_current_user(path)
    }
    #[cfg(windows)]
    {
        windows::acl::owned_by_current_user(path)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = path;
        false
    }
}

pub fn owner_only() -> &'static dyn OwnerOnly {
    host().owner_only()
}

pub fn daemon_spawn() -> &'static dyn DaemonSpawn {
    host().daemon_spawn()
}

pub fn signals() -> &'static dyn Signals {
    host().signals()
}

pub fn console() -> &'static dyn Console {
    host().console()
}

pub fn dialog() -> &'static dyn Dialog {
    host().dialog()
}

pub fn crash_reports() -> &'static dyn CrashReports {
    host().crash_reports()
}

pub fn thread_qos() -> &'static dyn ThreadQos {
    host().thread_qos()
}

/// The first call of a thread that runs a machine or feeds one in real time: asks for the
/// user-interactive class and returns the class read back, by name. A refusal does not stop the
/// thread.
pub fn machine_thread() -> &'static str {
    qos_report(thread_qos().request_interactive())
}

pub fn thread_qos_name() -> &'static str {
    qos_report(thread_qos().current())
}

pub fn qos_report(answer: Result<QosClass, PlatformError>) -> &'static str {
    match answer {
        Ok(class) => class.as_str(),
        Err(e) if e.is_not_implemented_yet() => "not-implemented",
        Err(_) => "unreadable",
    }
}

/// The services of a host that is neither macOS nor Windows: every call refuses, so `cargo check`
/// there reports one honest error instead of failing to compile.
#[cfg(not(any(target_os = "macos", windows)))]
struct UnsupportedHost;

#[cfg(not(any(target_os = "macos", windows)))]
impl Host for UnsupportedHost {
    fn owner_only(&self) -> &dyn OwnerOnly {
        self
    }

    fn daemon_spawn(&self) -> &dyn DaemonSpawn {
        self
    }

    fn signals(&self) -> &dyn Signals {
        self
    }

    fn console(&self) -> &dyn Console {
        self
    }

    fn dialog(&self) -> &dyn Dialog {
        self
    }

    fn crash_reports(&self) -> &dyn CrashReports {
        self
    }

    fn thread_qos(&self) -> &dyn ThreadQos {
        self
    }
}

/// A host without QoS classes: the request is a no-op and reads [`QosClass::None`], since a machine
/// thread must still run.
#[cfg(not(any(target_os = "macos", windows)))]
impl ThreadQos for UnsupportedHost {
    fn request_interactive(&self) -> Result<QosClass, PlatformError> {
        Ok(QosClass::None)
    }

    fn current(&self) -> Result<QosClass, PlatformError> {
        Ok(QosClass::None)
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
impl DaemonSpawn for UnsupportedHost {
    fn spawn_detached(&self, _command: &mut Command) -> Result<DetachedChild, PlatformError> {
        Err(PlatformError::Unsupported(UNSUPPORTED_HOST))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
impl Signals for UnsupportedHost {
    fn shutdown_signals(&self) -> &'static [ShutdownSignal] {
        &[]
    }

    fn install_shutdown(&self) -> Result<ShutdownFlag, PlatformError> {
        Err(PlatformError::Unsupported(UNSUPPORTED_HOST))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
impl Console for UnsupportedHost {
    /// Never interactive: without console detection no one-time code may be printed.
    fn is_interactive(&self) -> bool {
        false
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
impl Dialog for UnsupportedHost {
    fn availability(&self) -> DialogAvailability {
        DialogAvailability::Unavailable(UNSUPPORTED_HOST)
    }

    fn ask(&self, _title: &str, _body: &str) -> Result<Confirmation, PlatformError> {
        Err(PlatformError::Unsupported(UNSUPPORTED_HOST))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
impl CrashReports for UnsupportedHost {
    fn opt_out(&self) -> Result<CrashReportOptOut, PlatformError> {
        Err(PlatformError::Unsupported(UNSUPPORTED_HOST))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
impl OwnerOnly for UnsupportedHost {
    fn create_dir(&self, _path: &Path) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(UNSUPPORTED_HOST))
    }

    fn create_new_dir(&self, _path: &Path) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(UNSUPPORTED_HOST))
    }

    fn create_file(&self, _path: &Path) -> Result<File, PlatformError> {
        Err(PlatformError::Unsupported(UNSUPPORTED_HOST))
    }

    fn create_new_file(&self, _path: &Path) -> Result<File, PlatformError> {
        Err(PlatformError::Unsupported(UNSUPPORTED_HOST))
    }

    fn check(&self, _path: &Path) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(UNSUPPORTED_HOST))
    }
}
