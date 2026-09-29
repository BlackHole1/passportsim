//! macOS implementation of the platform traits: mode bits for owner-only files, a native alert in
//! a window-server session, tty plus foreground process group for console detection, IOKit serial
//! enumeration, `setsid` for the detached daemon, and the QoS class machine threads ask for.
//!
//! Only this module may call raw OS process APIs. The libSystem functions are declared by hand,
//! since std already links them, so there is no `libc` dependency or build script.

use std::fs::{DirBuilder, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    Confirmation, Console, CrashReportOptOut, CrashReports, DaemonSpawn, DetachedChild, Dialog,
    DialogAvailability, Host, OwnerOnly, PlatformError, QosClass, ShutdownFlag, ShutdownSignal,
    Signals, ThreadQos,
};

pub const FILE_MODE: u32 = 0o600;
pub const DIR_MODE: u32 = 0o700;
/// The permission bits that must all be clear: group and other.
pub const FOREIGN_BITS: u32 = 0o077;
/// `O_NOFOLLOW` (`sys/fcntl.h`): the open fails with `ELOOP` when the final component is a symlink.
/// A symlink planted at `<runtime role>/serve.json` would otherwise redirect the daemon token, and
/// the read check, which follows links, would pass the same user's 0600 target.
const O_NOFOLLOW: i32 = 0x0100;
/// `O_NONBLOCK` (`sys/fcntl.h`): opening a FIFO returns at once instead of waiting for a writer, so
/// [`super::open_regular_no_follow`] can refuse it after the open.
const O_NONBLOCK: i32 = 0x0004;

/// The macOS arm of [`super::open_regular_no_follow`]: `O_NOFOLLOW | O_NONBLOCK`, and the opened
/// handle must be the `(dev, ino)` that `expected` describes.
pub(crate) fn open_read_no_follow(
    path: &Path,
    expected: &std::fs::Metadata,
) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    if opened.dev() != expected.dev() || opened.ino() != expected.ino() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the file changed between the check and the open",
        ));
    }
    Ok(file)
}

// SAFETY of the declarations: libSystem entry points with a stable ABI that std links; they take
// no arguments and cannot fail.
unsafe extern "C" {
    fn getuid() -> u32;
    fn geteuid() -> u32;
}

/// Makes a FIFO at `path` with `mkfifo(3)` for tests. In process rather than spawning `mkfifo(1)`:
/// a child forked while another test holds an `flock` inherits that lock until it execs.
#[cfg(test)]
pub(crate) fn make_fifo(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    unsafe extern "C" {
        /// POSIX `mkfifo(3)`; `mode_t` is 16 bits on macOS.
        fn mkfifo(path: *const std::ffi::c_char, mode: u16) -> i32;
    }
    let text = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `text` is a NUL-terminated string that outlives the call, which only reads it.
    if unsafe { mkfifo(text.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn owned_by_current_user(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (std::fs::metadata(path), current_uid()) {
        (Ok(meta), Some(uid)) => meta.uid() == uid,
        _ => false,
    }
}

/// The uid an owner-only file must belong to. Real and effective ids must match, so a set-user-id
/// run cannot accept a file that belongs to neither.
fn current_uid() -> Option<u32> {
    // SAFETY: neither call has a failure mode or a side effect.
    let (real, effective) = unsafe { (getuid(), geteuid()) };
    (real == effective).then_some(real)
}

pub struct MacOs;

impl Host for MacOs {
    fn owner_only(&self) -> &dyn OwnerOnly {
        self
    }

    /// `ProductName` and `ProductVersion` from the system version file (what `sw_vers` prints),
    /// read directly so `doctor` spawns no process.
    fn os_version(&self) -> Result<String, PlatformError> {
        const PLIST: &str = "/System/Library/CoreServices/SystemVersion.plist";
        let text = std::fs::read_to_string(PLIST).map_err(|e| PlatformError::io(PLIST, e))?;
        let value = |key: &str| {
            let at = text.find(&format!("<key>{key}</key>"))?;
            let rest = &text[at..];
            let open = rest.find("<string>")? + "<string>".len();
            let close = rest[open..].find("</string>")?;
            Some(rest[open..open + close].trim().to_owned())
        };
        match (value("ProductName"), value("ProductVersion")) {
            (Some(name), Some(version)) => Ok(format!("{name} {version}")),
            _ => Err(PlatformError::io(
                PLIST,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "no ProductName or ProductVersion",
                ),
            )),
        }
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

const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

unsafe extern "C" {
    /// `pthread/qos.h`: sets the calling thread's QoS class; 0 on success, an errno otherwise.
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    fn qos_class_self() -> u32;
}

fn qos_class(raw: u32) -> QosClass {
    match raw {
        0x21 => QosClass::UserInteractive,
        0x19 => QosClass::UserInitiated,
        0x15 => QosClass::Default,
        0x11 => QosClass::Utility,
        0x09 => QosClass::Background,
        0x00 => QosClass::Unspecified,
        _ => QosClass::Unknown,
    }
}

impl ThreadQos for MacOs {
    /// A failed request is an error, not a class, so a report never claims a class not granted.
    fn request_interactive(&self) -> Result<QosClass, PlatformError> {
        // SAFETY: no pointers; changes only the calling thread's class and reports failure by
        // return value.
        let status = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
        if status != 0 {
            return Err(PlatformError::io(
                PathBuf::new(),
                std::io::Error::from_raw_os_error(status),
            ));
        }
        self.current()
    }

    fn current(&self) -> Result<QosClass, PlatformError> {
        // SAFETY: no arguments; it reads the calling thread's class.
        Ok(qos_class(unsafe { qos_class_self() }))
    }
}

impl OwnerOnly for MacOs {
    /// Creates the directory with mode 0700 in the `mkdir` call itself, parents too; the umask can
    /// only take bits away. `DirBuilder::recursive` applies the mode only to directories it
    /// creates, so the leaf and every created level must pass [`OwnerOnly::check`], and every
    /// pre-existing one [`check_ancestor`], which accepts a stock 0750 home or 0755 `/Users`.
    fn create_dir(&self, path: &Path) -> Result<(), PlatformError> {
        let existing = deepest_existing(path);
        if let Some(ancestor) = existing.as_deref() {
            for level in ancestor.ancestors().filter(|a| !a.as_os_str().is_empty()) {
                check_ancestor(level)?;
            }
        }
        if let Err(e) = DirBuilder::new()
            .mode(DIR_MODE)
            .recursive(true)
            .create(path)
        {
            return Err(PlatformError::io(path, e));
        }
        for dir in created_levels(existing.as_deref(), path) {
            self.check(&dir)?;
        }
        Ok(())
    }

    /// One non-recursive `mkdir(2)` with mode 0700, failing with `EEXIST` on anything already at
    /// `path`. The new directory is then held to [`OwnerOnly::check`].
    fn create_new_dir(&self, path: &Path) -> Result<(), PlatformError> {
        DirBuilder::new()
            .mode(DIR_MODE)
            .create(path)
            .map_err(|e| PlatformError::io(path, e))?;
        self.check(path)
    }

    /// Opens `path` for writing, creating it with mode 0600. An existing file keeps its mode and
    /// may not be made private afterwards, so it is opened without `O_TRUNC`, the descriptor is
    /// checked, and only then `set_len(0)`: a refusal leaves the token or discovery file exactly
    /// as found. [`O_NOFOLLOW`] refuses a symlink at `path` before any write.
    fn create_file(&self, path: &Path) -> Result<File, PlatformError> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .custom_flags(O_NOFOLLOW)
            .mode(FILE_MODE)
            .open(path)
            .map_err(|e| PlatformError::io(path, e))?;
        check_metadata(
            path,
            &file.metadata().map_err(|e| PlatformError::io(path, e))?,
        )?;
        file.set_len(0).map_err(|e| PlatformError::io(path, e))?;
        Ok(file)
    }

    /// Creates `path` with mode 0600 and `O_EXCL`, failing on anything already there, a symlink
    /// included. The metadata check still runs, for the umask.
    fn create_new_file(&self, path: &Path) -> Result<File, PlatformError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(O_NOFOLLOW)
            .mode(FILE_MODE)
            .open(path)
            .map_err(|e| PlatformError::io(path, e))?;
        check_metadata(
            path,
            &file.metadata().map_err(|e| PlatformError::io(path, e))?,
        )?;
        Ok(file)
    }

    /// Owner uid equals the current uid and `mode & 0o077 == 0`. `metadata` follows symlinks on
    /// purpose: it describes what an open reaches, and a symlink's own mode is always 0777.
    fn check(&self, path: &Path) -> Result<(), PlatformError> {
        let meta = std::fs::metadata(path).map_err(|e| PlatformError::io(path, e))?;
        check_metadata(path, &meta)
    }
}

// SAFETY of the declarations below: POSIX entry points of libSystem, which std already links, with
// the signatures written beside them. None takes a pointer, and each fails with a negative return.
unsafe extern "C" {
    /// POSIX `setsid(2)`: makes the caller a session leader with no controlling terminal; -1 only
    /// for a process-group leader, which a freshly forked child never is.
    fn setsid() -> i32;
    /// POSIX `getsid(2)`: the session id of `pid`, or -1.
    #[cfg(test)]
    fn getsid(pid: i32) -> i32;
    fn getpgrp() -> i32;
    /// POSIX `tcgetpgrp(3)`: the foreground process group of the terminal on `fd`, or -1.
    fn tcgetpgrp(fd: i32) -> i32;
    fn isatty(fd: i32) -> i32;
    /// POSIX `signal(3)`: installs `handler` (an `extern "C" fn(i32)` cast to `usize`) for `sig`
    /// and returns the previous one, or `SIG_ERR`.
    fn signal(sig: i32, handler: usize) -> usize;
    /// POSIX `raise(3)`: sends `sig` to the calling thread.
    #[cfg(test)]
    fn raise(sig: i32) -> i32;
}

const CALLER_SECURITY_SESSION: i32 = -1;
/// `sessionHasGraphicAccess`: the session can draw, which a dialog needs.
const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;

// SAFETY: `SessionGetInfo` is a documented Security.framework entry point with a stable ABI. Both
// pointers are out-parameters written before returning 0; an error leaves them untouched, which is
// why they are initialized first.
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    /// `OSStatus SessionGetInfo(SecuritySessionId session, SecuritySessionId *sessionId,
    /// SessionAttributeBits *attributes)`.
    #[allow(non_snake_case)]
    fn SessionGetInfo(session: i32, session_id: *mut i32, attributes: *mut u32) -> i32;
}

// The native confirmation alert.

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kCFUserNotificationCautionAlertLevel` from the SDK's `CFUserNotification.h`: the levels run
/// stop 0, note 1, caution 2, plain 3. A write to real hardware is a caution.
const CF_USER_NOTIFICATION_CAUTION_ALERT_LEVEL: usize = 2;
/// `kCFUserNotificationDefaultResponse`: the default button, which is the confirm button here.
const CF_USER_NOTIFICATION_DEFAULT_RESPONSE: usize = 0;
/// `kCFUserNotificationAlternateResponse`: the decline button. Also the out-parameter's seed, so a
/// call that leaves it untouched reads as a decline, not as the 0 of the default button.
const CF_USER_NOTIFICATION_ALTERNATE_RESPONSE: usize = 1;
/// `kCFUserNotificationCancelResponse`: the window went away without a button. On macOS 27.2 this
/// is what a timeout returns (status 0 after the timeout, not the non-zero status the header
/// suggests); [`dialog_answer`] declines both readings.
const CF_USER_NOTIFICATION_CANCEL_RESPONSE: usize = 3;
/// The response bits that must equal [`CF_USER_NOTIFICATION_DEFAULT_RESPONSE`] for a confirmation:
/// the two button bits and the six reserved above them. Two bits would be a trap: the default
/// response is 0, so a reserved value such as 4 would read as Confirm.
const CF_USER_NOTIFICATION_RESPONSE_MASK: usize = 0xFF;
/// How long the alert waits before [`dialog_answer`] reads a decline, so a wedged or headless
/// window server cannot block the command for ever. Long enough to read a plan.
const DIALOG_TIMEOUT_SECONDS: f64 = 120.0;
const DIALOG_CONFIRM_BUTTON: &str = "Confirm";
/// The decline button. Closing the window, the timeout and every other outcome also decline.
const DIALOG_DECLINE_BUTTON: &str = "Cancel";

// SAFETY of the declarations below: documented CoreFoundation entry points with a stable ABI and
// the C signatures written beside them. `CFTimeInterval` is `double`, `CFOptionFlags` and
// `CFIndex` are pointer-sized on both macOS targets, `CFStringEncoding` is `UInt32` and `Boolean`
// is `unsigned char`.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    /// `CFStringRef CFStringCreateWithBytes(CFAllocatorRef alloc, const UInt8 *bytes, CFIndex
    /// numBytes, CFStringEncoding encoding, Boolean isExternalRepresentation)`. A null
    /// allocator is `kCFAllocatorDefault`. The caller owns the result.
    #[allow(non_snake_case)]
    fn CFStringCreateWithBytes(
        alloc: *const std::ffi::c_void,
        bytes: *const u8,
        num_bytes: isize,
        encoding: u32,
        is_external_representation: u8,
    ) -> *const std::ffi::c_void;

    /// `void CFRelease(CFTypeRef cf)`: `cf` must be a non-null reference this process owns.
    #[allow(non_snake_case)]
    fn CFRelease(cf: *const std::ffi::c_void);

    /// `SInt32 CFUserNotificationDisplayAlert(CFTimeInterval timeout, CFOptionFlags flags,
    /// CFURLRef iconURL, CFURLRef soundURL, CFURLRef localizationURL, CFStringRef alertHeader,
    /// CFStringRef alertMessage, CFStringRef defaultButtonTitle, CFStringRef
    /// alternateButtonTitle, CFStringRef otherButtonTitle, CFOptionFlags *responseFlags)`.
    /// Blocks until a person answers or `timeout` elapses; a non-zero status means the alert
    /// was not raised.
    #[allow(non_snake_case)]
    fn CFUserNotificationDisplayAlert(
        timeout: f64,
        flags: usize,
        icon_url: *const std::ffi::c_void,
        sound_url: *const std::ffi::c_void,
        localization_url: *const std::ffi::c_void,
        alert_header: *const std::ffi::c_void,
        alert_message: *const std::ffi::c_void,
        default_button_title: *const std::ffi::c_void,
        alternate_button_title: *const std::ffi::c_void,
        other_button_title: *const std::ffi::c_void,
        response_flags: *mut usize,
    ) -> i32;
}

/// A CoreFoundation string this process owns, released on drop, so [`raise_alert`] cannot leak one
/// when a later string fails.
struct CfString(*const std::ffi::c_void);

impl CfString {
    /// The UTF-8 bytes of `text` as a CoreFoundation string, copied as written: nothing shortens
    /// or rewrites what a person is asked to confirm.
    fn new(text: &str) -> Option<CfString> {
        let bytes = text.as_bytes();
        let len = isize::try_from(bytes.len()).ok()?;
        // SAFETY: `bytes` is valid for `len` bytes during the call, which copies them; a null
        // allocator is the default; the bytes are UTF-8 from a `&str`; there is no BOM.
        let raw = unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                bytes.as_ptr(),
                len,
                CF_STRING_ENCODING_UTF8,
                0,
            )
        };
        (!raw.is_null()).then_some(CfString(raw))
    }

    fn as_ptr(&self) -> *const std::ffi::c_void {
        self.0
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        // SAFETY: `self.0` is non-null (checked in `new`) and owned; `CfString` is neither `Copy`
        // nor `Clone`, so this runs once.
        unsafe { CFRelease(self.0) };
    }
}

/// What `CFUserNotificationDisplayAlert` answered. Only the default button is a yes: the alternate
/// button, a cancel (closed window, timeout), any unknown response and a non-zero status decline.
/// A pure function so the mapping is testable without a window server.
fn dialog_answer(status: i32, response: usize) -> Confirmation {
    // A non-zero status means no answer was received, so `response` is not read: it is treated as
    // the cancel response, like a timeout.
    let button = match status {
        0 => response & CF_USER_NOTIFICATION_RESPONSE_MASK,
        _ => CF_USER_NOTIFICATION_CANCEL_RESPONSE,
    };
    match button {
        CF_USER_NOTIFICATION_DEFAULT_RESPONSE => Confirmation::Confirmed,
        _ => Confirmation::Declined,
    }
}

/// Raises the alert and blocks until a person answers or [`DIALOG_TIMEOUT_SECONDS`] elapses. Only
/// called from [`Dialog::ask`] after [`Dialog::availability`] said `Available`.
fn raise_alert(title: &str, body: &str) -> Result<Confirmation, PlatformError> {
    let (Some(header), Some(message), Some(confirm), Some(decline)) = (
        CfString::new(title),
        CfString::new(body),
        CfString::new(DIALOG_CONFIRM_BUTTON),
        CfString::new(DIALOG_DECLINE_BUTTON),
    ) else {
        return Err(PlatformError::io(
            PathBuf::new(),
            std::io::Error::other(
                "the confirmation text could not be handed to CoreFoundation, so no dialog was \
                 raised",
            ),
        ));
    };
    // Seeded with the alternate response, never 0 (the default response), so an unwritten
    // out-parameter cannot read as a confirmation.
    let mut response: usize = CF_USER_NOTIFICATION_ALTERNATE_RESPONSE;
    // SAFETY: the four string references live until the end of this statement and are only read;
    // the three `CFURLRef` arguments and the other-button title are null ("omit"); `response` is a
    // live, initialized `CFOptionFlags` the call may write once.
    let status = unsafe {
        CFUserNotificationDisplayAlert(
            DIALOG_TIMEOUT_SECONDS,
            CF_USER_NOTIFICATION_CAUTION_ALERT_LEVEL,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            header.as_ptr(),
            message.as_ptr(),
            confirm.as_ptr(),
            decline.as_ptr(),
            std::ptr::null(),
            &mut response,
        )
    };
    Ok(dialog_answer(status, response))
}

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
const SIG_ERR: usize = usize::MAX;
#[cfg(test)]
const SIG_DFL: usize = 0;

const MACOS_SHUTDOWN_SIGNALS: &[ShutdownSignal] =
    &[ShutdownSignal::Interrupt, ShutdownSignal::Terminate];

/// The flag every installed handler sets. A handler runs between two arbitrary instructions, so it
/// stores one lock-free atomic and returns; the daemon's thread does the flushing.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn shutdown_handler(_sig: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

impl DaemonSpawn for MacOs {
    /// Starts a new session in the child (`setsid` in `pre_exec`) and sends all three stdio handles
    /// to the null device. Out of the shell's session and foreground group, the child gets no
    /// `SIGHUP` when the terminal closes and no `Ctrl-C`. It writes its own log, so the parent can
    /// exit at once.
    fn spawn_detached(&self, command: &mut Command) -> Result<DetachedChild, PlatformError> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the closure runs between `fork` and `exec`, where only async-signal-safe calls
        // are allowed; it calls `setsid` and nothing else.
        unsafe {
            command.pre_exec(|| match setsid() {
                -1 => Err(std::io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        command
            .spawn()
            .map(DetachedChild::new)
            .map_err(|e| PlatformError::io(std::path::PathBuf::new(), e))
    }
}

impl Signals for MacOs {
    fn shutdown_signals(&self) -> &'static [ShutdownSignal] {
        MACOS_SHUTDOWN_SIGNALS
    }

    /// Installs the handler for `SIGINT` and `SIGTERM`. The flag is one process-wide cell, because
    /// signal dispositions are process-wide.
    fn install_shutdown(&self) -> Result<ShutdownFlag, PlatformError> {
        let flag = ShutdownFlag::from_static(&SHUTDOWN);
        for sig in [SIGINT, SIGTERM] {
            // SAFETY: `shutdown_handler` has the `extern "C"` signature `signal(3)` requires and
            // only stores to a lock-free atomic.
            let previous = unsafe { signal(sig, shutdown_handler as *const () as usize) };
            if previous == SIG_ERR {
                return Err(PlatformError::io(
                    std::path::PathBuf::new(),
                    std::io::Error::last_os_error(),
                ));
            }
        }
        Ok(flag)
    }
}

impl Console for MacOs {
    /// The process has a controlling terminal and is in its foreground process group. The
    /// terminal is opened rather than fd 0 inspected, because a redirected stdin says nothing
    /// about a person; the foreground test rules out a background job, which `SIGTTOU` would stop
    /// at the prompt. A heuristic: a `script` or node-pty pseudo-terminal passes. The open goes
    /// through [`crate::paths::allow_controlling_terminal`], the one exemption of
    /// `refuse_device`.
    fn is_interactive(&self) -> bool {
        let path = crate::paths::allow_controlling_terminal();
        if crate::paths::refuse_device(path).is_err() {
            return false;
        }
        let Ok(tty) = std::fs::File::open(path) else {
            return false;
        };
        let fd = tty.as_raw_fd();
        // SAFETY: `fd` is open for the whole expression, and both calls only read terminal state.
        unsafe { isatty(fd) == 1 && tcgetpgrp(fd) == getpgrp() && getpgrp() != -1 }
    }
}

impl Dialog for MacOs {
    /// Whether this process is in a session with a window server: `sessionHasGraphicAccess` of
    /// `SessionGetInfo`. An `ssh` login, a system `launchd` job and a build agent all lack it.
    fn availability(&self) -> DialogAvailability {
        let mut id: i32 = 0;
        let mut attributes: u32 = 0;
        // SAFETY: both out-parameters are valid for the call and written only on success;
        // `CALLER_SECURITY_SESSION` means "my own session".
        let status = unsafe { SessionGetInfo(CALLER_SECURITY_SESSION, &mut id, &mut attributes) };
        if status != 0 {
            return DialogAvailability::Unavailable(
                "this process is in no security session that could own a window",
            );
        }
        match attributes & SESSION_HAS_GRAPHIC_ACCESS != 0 {
            true => DialogAvailability::Available,
            false => DialogAvailability::Unavailable(
                "this session has no window server, so no dialog can be raised",
            ),
        }
    }

    /// The native alert, with no shell and no other process. Without a session that can draw it
    /// refuses with an error, raising nothing. Only the default button is a yes. `title` and
    /// `body` go in as written (already redacted), never the terminal path's one-time code. A human
    /// gate, not a security boundary: Accessibility rights can click it.
    fn ask(&self, title: &str, body: &str) -> Result<Confirmation, PlatformError> {
        gated_on_availability(self.availability(), || raise_alert(title, body))
    }
}

/// Runs `raise` only when an interactive desktop exists, else refuses with the availability reason
/// as [`PlatformError::Unsupported`], so callers fall through to the next path. The decision is a
/// parameter so a test can gate out a panicking `raise` on a Mac that has a window server.
fn gated_on_availability(
    availability: DialogAvailability,
    raise: impl FnOnce() -> Result<Confirmation, PlatformError>,
) -> Result<Confirmation, PlatformError> {
    match availability {
        DialogAvailability::Available => raise(),
        DialogAvailability::Unavailable(why) => Err(PlatformError::Unsupported(why)),
    }
}

impl CrashReports for MacOs {
    /// Nothing is needed: ReportCrash records stack and registers rather than heap, so a report
    /// does not carry guest RAM or flash (unverified on macOS 27). The opt-out mechanism is
    /// Windows-only.
    fn opt_out(&self) -> Result<CrashReportOptOut, PlatformError> {
        Ok(CrashReportOptOut::NotNeeded)
    }
}

/// The deepest ancestor of `path` that already exists, `path` included. Everything below it is
/// what a recursive create makes and the emulator owns.
fn deepest_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|a| !a.as_os_str().is_empty() && a.exists())
        .map(Path::to_path_buf)
}

/// The directories a recursive create of `leaf` is responsible for, outermost first. If `leaf`
/// already existed the list is `leaf` alone, so it is still re-checked.
fn created_levels(existing: Option<&Path>, leaf: &Path) -> Vec<PathBuf> {
    let mut levels: Vec<PathBuf> = leaf
        .ancestors()
        .take_while(|a| !a.as_os_str().is_empty() && Some(*a) != existing)
        .map(Path::to_path_buf)
        .collect();
    if levels.is_empty() {
        levels.push(leaf.to_path_buf());
    }
    levels.reverse();
    levels
}

/// Root's uid, the one other owner an ancestor may have (`/`, `/Users`, `/var/folders`).
const ROOT_UID: u32 = 0;
/// Group and other write: the bits that let somebody else rename or replace entries.
const FOREIGN_WRITE: u32 = 0o022;
/// The sticky bit: only an entry's owner may rename or delete it, which makes `/tmp` safe above
/// the tree.
const STICKY: u32 = 0o1000;

/// The rule for a directory the emulator did not create but passes through. Weaker than
/// [`OwnerOnly::check`] (a stock home is 0750 and `/Users` 0755), it demands what protects the tree
/// below: nobody else may rename or replace an entry (no group or other write unless sticky), and
/// the owner is this user or root.
fn check_ancestor(path: &Path) -> Result<(), PlatformError> {
    let meta = std::fs::metadata(path).map_err(|e| PlatformError::io(path, e))?;
    let Some(uid) = current_uid() else {
        return Err(PlatformError::wider(
            path,
            "the real and effective user ids differ, so there is no single \"current uid\" to \
             compare the owner against",
        ));
    };
    if meta.uid() != uid && meta.uid() != ROOT_UID {
        return Err(PlatformError::wider(
            path,
            format!(
                "an ancestor of the tree belongs to uid {}, which is neither uid {uid} nor root",
                meta.uid()
            ),
        ));
    }
    let mode = meta.mode() & 0o7777;
    if mode & FOREIGN_WRITE != 0 && mode & STICKY == 0 {
        return Err(PlatformError::wider(
            path,
            format!(
                "an ancestor of the tree has mode {mode:04o}, so another user may rename or \
                 replace what the emulator creates below it"
            ),
        ));
    }
    Ok(())
}

fn check_metadata(path: &Path, meta: &std::fs::Metadata) -> Result<(), PlatformError> {
    let Some(uid) = current_uid() else {
        return Err(PlatformError::wider(
            path,
            "the real and effective user ids differ, so there is no single \"current uid\" to \
             compare the owner against",
        ));
    };
    if meta.uid() != uid {
        return Err(PlatformError::wider(
            path,
            format!("it belongs to uid {}, not to uid {uid}", meta.uid()),
        ));
    }
    let mode = meta.mode() & 0o7777;
    if mode & FOREIGN_BITS != 0 {
        return Err(PlatformError::wider(
            path,
            format!("its mode is {mode:04o}, which grants group or other access"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::super::scratch_dir;
    use super::*;

    #[test]
    fn qos_classes_follow_sys_qos_h() {
        let named: Vec<&str> = [0x21, 0x19, 0x15, 0x11, 0x09, 0x00, 0x42]
            .into_iter()
            .map(|raw| qos_class(raw).as_str())
            .collect();
        assert_eq!(
            named,
            [
                "user-interactive",
                "user-initiated",
                "default",
                "utility",
                "background",
                "unspecified",
                "unknown"
            ]
        );
        assert_eq!(QOS_CLASS_USER_INTERACTIVE, 0x21);
    }

    /// A spawned thread reads `default` whatever its parent runs at, which is why every machine
    /// thread asks, and then reads back the class it asked for.
    #[test]
    fn a_spawned_thread_starts_at_default_and_reads_back_what_it_asked_for() {
        let (before, after) = std::thread::spawn(|| {
            let before = MacOs.current().expect("reads");
            let after = MacOs.request_interactive().expect("granted");
            (before, after)
        })
        .join()
        .expect("joins");
        assert_eq!(before, QosClass::Default);
        assert_eq!(after, QosClass::UserInteractive);
        assert_eq!(super::super::qos_report(Ok(after)), "user-interactive");
    }

    #[test]
    fn created_file_is_accepted_and_a_wider_one_is_refused() {
        let dir = scratch_dir("macos-owner-file");
        MacOs.create_dir(&dir).expect("owner-only directory");
        let file = dir.join("token");
        {
            let handle = MacOs.create_file(&file).expect("owner-only file");
            drop(handle);
        }
        let mode = std::fs::metadata(&file).expect("metadata").mode() & 0o7777;
        assert_eq!(mode, FILE_MODE, "created with the macOS file mode");
        MacOs
            .check(&file)
            .expect("the file it just created is owner-only");

        for wider in [0o640, 0o604, 0o666, 0o700] {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(wider))
                .expect("widen the mode");
            let refused = MacOs.check(&file);
            if wider & FOREIGN_BITS == 0 {
                refused.expect("0700 grants nothing to group or other");
            } else {
                let detail = refused.expect_err("a wider mode is refused").to_string();
                assert!(
                    detail.contains("not owner-only") && detail.contains("group or other"),
                    "the refusal names the rule it broke: {detail}"
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn created_directory_is_0700_and_a_wider_one_is_refused() {
        let dir = scratch_dir("macos-owner-dir");
        MacOs.create_dir(&dir).expect("owner-only directory");
        let mode = std::fs::metadata(&dir).expect("metadata").mode() & 0o7777;
        assert_eq!(mode, DIR_MODE, "created with the macOS directory mode");

        MacOs
            .create_dir(&dir)
            .expect("an existing owner-only directory is not an error");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("widen the mode");
        let refused = MacOs
            .create_dir(&dir)
            .expect_err("a widened directory is refused, not repaired");
        assert_eq!(
            std::fs::metadata(&dir).expect("metadata").mode() & 0o7777,
            0o755,
            "the refusal changed nothing on disk"
        );
        assert!(refused.to_string().contains("0755"), "{refused}");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(DIR_MODE)).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parents_are_created_owner_only() {
        let root = scratch_dir("macos-owner-parents");
        let leaf = root.join("boot-cache").join("entries");
        MacOs.create_dir(&leaf).expect("owner-only directory tree");
        for dir in [&root, &root.join("boot-cache"), &leaf] {
            assert_eq!(
                std::fs::metadata(dir).expect("metadata").mode() & 0o7777,
                DIR_MODE,
                "every level of `{}` is owner-only",
                dir.display()
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// Regression: a `create_file` passing `truncate(true)` emptied the file before the owner-only
    /// check, so the caller got an error and a destroyed token or discovery file.
    #[test]
    fn a_refusal_leaves_the_file_it_refuses_untouched() {
        let dir = scratch_dir("macos-refusal-keeps-content");
        MacOs.create_dir(&dir).expect("owner-only directory");
        let file = dir.join("serve.json");
        let content = b"{\"token\":\"a-real-daemon-token\"}";
        std::fs::write(&file, content).expect("a file an earlier run left behind");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
            .expect("widened by an earlier run or a different umask");

        let refused = MacOs
            .create_file(&file)
            .expect_err("a widened file is refused");
        assert!(refused.to_string().contains("0644"), "{refused}");
        assert_eq!(
            std::fs::read(&file).expect("the file is still there"),
            content,
            "the refusal destroyed nothing, so the caller can still remove and recreate"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_accepted_file_is_still_truncated() {
        let dir = scratch_dir("macos-accepted-truncates");
        MacOs.create_dir(&dir).expect("owner-only directory");
        let file = dir.join("token");
        {
            use std::io::Write;
            let mut handle = MacOs.create_file(&file).expect("owner-only file");
            handle
                .write_all(b"the first, longer content")
                .expect("write");
        }
        {
            use std::io::Write;
            let mut handle = MacOs.create_file(&file).expect("owner-only file");
            handle.write_all(b"short").expect("write");
        }
        assert_eq!(
            std::fs::read(&file).expect("read back"),
            b"short",
            "a create that is accepted starts the file empty"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_symlink_at_the_target_is_refused() {
        let dir = scratch_dir("macos-symlink-target");
        MacOs.create_dir(&dir).expect("owner-only directory");
        let elsewhere = dir.join("notes.txt");
        let content = b"a file of the user's that is not the emulator's to write";
        std::fs::write(&elsewhere, content).expect("the link target");
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(FILE_MODE))
            .expect("owned by the same user, at the same mode, so `check` would accept it");
        let planted = dir.join("serve.json");
        std::os::unix::fs::symlink(&elsewhere, &planted).expect("plant the symlink");

        MacOs
            .create_file(&planted)
            .expect_err("a symlink at the target is not a file this call may create");
        assert_eq!(
            std::fs::read(&elsewhere).expect("the target is still there"),
            content,
            "nothing was written through the link"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The second half matters: an owner-only rule on ancestors would refuse a stock 0750 home
    /// or 0755 `/Users`.
    #[test]
    fn a_foreign_writable_ancestor_is_refused_and_a_readable_one_is_not() {
        let root = scratch_dir("macos-ancestor");
        MacOs.create_dir(&root).expect("owner-only directory");

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).expect("chmod");
        MacOs
            .create_dir(&root.join("run"))
            .expect("a group-readable ancestor is not the emulator's to refuse");

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        let refused = MacOs
            .create_dir(&root.join("logs"))
            .expect_err("a foreign-writable ancestor is refused");
        assert!(
            refused.to_string().contains("0777") && refused.to_string().contains("rename"),
            "the refusal names the ancestor and the power it grants: {refused}"
        );
        assert!(
            !root.join("logs").exists() || MacOs.check(&root.join("logs")).is_ok(),
            "the refusal is about the ancestor, not about what was created below it"
        );

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o1777)).expect("chmod");
        MacOs
            .create_dir(&root.join("sticky"))
            .expect("a sticky world-writable ancestor lets only an entry's owner rename it");

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(DIR_MODE)).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_new_directory_is_0700_and_exclusive() {
        let root = scratch_dir("macos-new-dir");
        MacOs.create_dir(&root).expect("owner-only directory");
        let run = root.join("run-1");
        MacOs.create_new_dir(&run).expect("a fresh name");
        assert_eq!(
            std::fs::metadata(&run).expect("metadata").mode() & 0o7777,
            DIR_MODE
        );
        let taken = MacOs.create_new_dir(&run).expect_err("the name is taken");
        assert!(
            matches!(&taken, PlatformError::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists),
            "{taken}"
        );
        std::os::unix::fs::symlink(&run, root.join("link")).expect("symlink");
        MacOs
            .create_new_dir(&root.join("link"))
            .expect_err("a symlink is something already there");
        MacOs
            .create_new_dir(&root.join("missing").join("run"))
            .expect_err("non-recursive: the parent must exist");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_current_uid_is_the_owner_of_a_file_this_process_creates() {
        let dir = scratch_dir("macos-owner-uid");
        MacOs.create_dir(&dir).expect("owner-only directory");
        let uid = current_uid().expect("an ordinary run has one current uid");
        assert_eq!(
            std::fs::metadata(&dir).expect("metadata").uid(),
            uid,
            "the owner uid equals the current uid"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod service_tests {
    use std::time::{Duration, Instant};

    use super::super::scratch_dir;
    use super::*;

    /// Waits up to two seconds for `ready`, polling rather than sleeping a guessed amount.
    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if ready() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn a_detached_child_runs_in_its_own_session() {
        let dir = scratch_dir("spawn-detached");
        MacOs.create_dir(&dir).expect("owner-only directory");
        let marker = dir.join("started");
        let script = format!("printf started > '{}'; sleep 3", marker.to_string_lossy());
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(&script);
        let mut child = MacOs
            .spawn_detached(&mut command)
            .expect("the detached spawn");

        wait_for("the detached child to run", || marker.exists());
        let pid = child.pid() as i32;
        // SAFETY: `getsid` only reads the session id of a live process.
        let (child_session, own_session) = unsafe { (getsid(pid), getsid(0)) };
        assert_eq!(
            child_session, pid,
            "`setsid` in `pre_exec` makes the child its own session leader"
        );
        assert_ne!(
            child_session, own_session,
            "the child left the session of the process that started it"
        );

        child.kill().expect("end the child");
        child.wait().expect("reap the child");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Names the marker file for [`spawn_helper_parent`], and by being set tells it that it is the
    /// intermediate parent of [`a_spawned_child_outlives_its_parent`].
    const OUTLIVE_MARKER: &str = "PEMU_TEST_OUTLIVE_MARKER";

    const OUTLIVE_HELPER: &str = "platform::macos::service_tests::spawn_helper_parent";

    /// Not an assertion: the intermediate parent [`a_spawned_child_outlives_its_parent`] re-runs
    /// this binary for. It spawns detached and returns, so its process exits while the child runs.
    /// Without [`OUTLIVE_MARKER`] it does nothing.
    #[test]
    fn spawn_helper_parent() {
        let Some(marker) = std::env::var_os(OUTLIVE_MARKER) else {
            return;
        };
        let marker = PathBuf::from(marker);
        // The grandchild appends to the marker about twenty times a second and stops on its own
        // after roughly three seconds, so the test needs no kill and leaves nothing behind.
        let script = format!(
            "i=0; while [ $i -lt 60 ]; do printf x >> '{}'; i=$((i+1)); sleep 0.05; done",
            marker.to_string_lossy()
        );
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(&script);
        MacOs
            .spawn_detached(&mut command)
            .expect("the detached spawn");
    }

    /// A parent that stays alive can never show this, so the parent is a second copy of this
    /// binary filtered to [`spawn_helper_parent`]: it spawns and exits, `status()` reaps it, and
    /// the grandchild's marker must keep growing after that.
    #[test]
    fn a_spawned_child_outlives_its_parent() {
        let dir = scratch_dir("spawn-outlives");
        MacOs.create_dir(&dir).expect("owner-only directory");
        let marker = dir.join("alive");

        let status = Command::new(std::env::current_exe().expect("this test binary"))
            .args(["--exact", OUTLIVE_HELPER])
            .env(OUTLIVE_MARKER, &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run the intermediate parent");
        assert!(
            status.success(),
            "the intermediate parent ran the detached spawn: {status}"
        );

        wait_for("the detached grandchild to start", || marker.exists());
        let seen = std::fs::metadata(&marker).map_or(0, |m| m.len());
        wait_for("the child to go on running after its parent exited", || {
            std::fs::metadata(&marker).is_ok_and(|m| m.len() > seen)
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_spawn_that_cannot_start_reports_the_error() {
        let missing = scratch_dir("spawn-missing").join("no-such-program");
        let mut command = Command::new(&missing);
        let e = MacOs
            .spawn_detached(&mut command)
            .expect_err("a program that does not exist");
        assert!(
            !e.is_unsupported(),
            "a missing program is not an unsupported host: {e}"
        );
    }

    /// `SIGINT` is raised on this thread and the default disposition put back afterwards.
    #[test]
    fn the_shutdown_handler_catches_the_macos_signals() {
        assert_eq!(
            MacOs.shutdown_signals(),
            &[ShutdownSignal::Interrupt, ShutdownSignal::Terminate],
            "SIGINT and SIGTERM under `cfg(target_os = \"macos\")`"
        );
        let flag = MacOs.install_shutdown().expect("install the handler");
        assert!(!flag.is_set(), "nothing has been delivered yet");
        // SAFETY: `raise` delivers to the calling thread, where the handler installed above
        // stores to one atomic and returns.
        let delivered = unsafe { raise(SIGINT) };
        assert_eq!(delivered, 0, "raise(SIGINT) succeeded");
        assert!(flag.is_set(), "the handler set the shutdown flag");
        // SAFETY: restoring the default disposition of SIGINT for the rest of the test run.
        unsafe {
            signal(SIGINT, SIG_DFL);
            signal(SIGTERM, SIG_DFL);
        }
    }

    /// The value depends on how the test was started (it is a heuristic), so only stability and
    /// agreement with the terminal's existence are asserted.
    #[test]
    fn console_detection_is_consistent_with_the_controlling_terminal() {
        let interactive = MacOs.is_interactive();
        assert_eq!(interactive, MacOs.is_interactive(), "a stable answer");
        if std::fs::File::open(crate::paths::allow_controlling_terminal()).is_err() {
            assert!(
                !interactive,
                "no controlling terminal means no interactive console"
            );
        }
    }

    /// Never calls `ask` in a graphic session: that would put a real alert on the developer's
    /// screen for two minutes. The gate and the answer mapping have their own tests.
    #[test]
    fn the_dialog_never_answers_by_itself() {
        let availability = MacOs.availability();
        assert_eq!(
            availability,
            MacOs.availability(),
            "the availability of a session does not flicker"
        );
        if let DialogAvailability::Unavailable(why) = availability {
            let refused = MacOs
                .ask("Flash the device?", "This writes to the attached hardware.")
                .expect_err("no confirmation exists without a desktop to show one on");
            assert!(
                refused.to_string().contains(why),
                "the refusal carries the reason `availability` gave: {refused}"
            );
        }
    }

    /// Driven through `gated_on_availability` so it holds on a Mac whose session is available.
    #[test]
    fn the_availability_gate_refuses_without_raising_anything() {
        let refused = gated_on_availability(
            DialogAvailability::Unavailable("this session has no window server"),
            || panic!("the alert must not be raised when no desktop exists"),
        )
        .expect_err("a dialog that cannot be shown is never a confirmation");
        assert!(
            refused.to_string().contains("no window server"),
            "the reason travels to the caller, which falls through to another path: {refused}"
        );
    }

    #[test]
    fn an_available_desktop_is_the_only_case_that_raises_the_alert() {
        for answer in [Confirmation::Confirmed, Confirmation::Declined] {
            let raised = std::cell::Cell::new(false);
            let got = gated_on_availability(DialogAvailability::Available, || {
                raised.set(true);
                Ok(answer)
            })
            .expect("an available desktop raises the alert");
            assert!(raised.get(), "the alert was raised");
            assert_eq!(
                got, answer,
                "the person's answer is passed through as given"
            );
        }
    }

    #[test]
    fn only_the_default_button_is_a_confirmation() {
        assert_eq!(
            dialog_answer(0, CF_USER_NOTIFICATION_DEFAULT_RESPONSE),
            Confirmation::Confirmed,
            "the default button is the confirm button"
        );
        for response in [
            CF_USER_NOTIFICATION_ALTERNATE_RESPONSE,
            2, // kCFUserNotificationOtherResponse, a button this alert does not offer
            CF_USER_NOTIFICATION_CANCEL_RESPONSE,
            // A reserved value: 4 & 3 is 0, the default response, which is why the mask is not
            // two bits wide.
            4,
            0xFE,
        ] {
            assert_eq!(
                dialog_answer(0, response),
                Confirmation::Declined,
                "response {response} is not the default button, so it is a decline"
            );
        }
        for status in [-1, 1, i32::MIN, i32::MAX] {
            assert_eq!(
                dialog_answer(status, CF_USER_NOTIFICATION_DEFAULT_RESPONSE),
                Confirmation::Declined,
                "status {status} means the alert was never raised or received: the untouched \
                 out-parameter must not read as a yes"
            );
        }
    }

    /// The measured macOS 27.2 timeout (status 0, cancel response) is a decline.
    #[test]
    fn a_timed_out_alert_is_a_decline_as_this_host_reports_it() {
        assert_eq!(
            dialog_answer(0, CF_USER_NOTIFICATION_CANCEL_RESPONSE),
            Confirmation::Declined,
            "the timeout this host actually returns"
        );
        assert_eq!(
            dialog_answer(-1, CF_USER_NOTIFICATION_ALTERNATE_RESPONSE),
            Confirmation::Declined,
            "and the non-zero status a host that reported it the other way would return"
        );
    }

    /// The check-box and text-field bits (from bit 8) never change the answer; this alert offers
    /// neither, and the mask keeps that an assumption the code does not rely on.
    #[test]
    fn the_response_flags_are_read_by_their_button_bits_only() {
        let check_box_checked = 1usize << 8;
        assert_eq!(
            dialog_answer(0, CF_USER_NOTIFICATION_DEFAULT_RESPONSE | check_box_checked),
            Confirmation::Confirmed
        );
        assert_eq!(
            dialog_answer(
                0,
                CF_USER_NOTIFICATION_ALTERNATE_RESPONSE | check_box_checked
            ),
            Confirmation::Declined
        );
    }

    /// CoreFoundation accepts every shape a redacted prompt can take (empty, long, non-ASCII), so
    /// a prompt is never cut short on the way to the alert. The alert itself needs a window server.
    #[test]
    fn the_callers_text_reaches_corefoundation_whole() {
        let long = "Flash 3 segments to /dev/cu.usbmodem1101. ".repeat(200);
        for text in ["", "Flash the device?", "确认写入设备？ \u{1f9ff}", &long] {
            let string =
                CfString::new(text).expect("UTF-8 bytes are always a CoreFoundation string");
            assert!(!string.as_ptr().is_null(), "a usable reference");
        }
    }

    #[test]
    fn the_macos_crash_report_opt_out_is_not_needed() {
        assert_eq!(
            MacOs.opt_out().expect("no host call to fail"),
            CrashReportOptOut::NotNeeded
        );
    }
}
