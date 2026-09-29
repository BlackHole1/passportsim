//! Windows implementation of the platform traits. Each service with a Win32 body has its own
//! submodule citing the Microsoft Learn pages its choices depend on: [`acl`] (owner-only
//! descriptor and read check), [`desktop`] (console and dialog), [`wer`] (crash-report opt-out),
//! `process` (detached spawn, console control events, thread class) and `net` (loopback connect).
//! This file holds the trait impls that delegate to them.
//!
//! The owner-only guarantee is weaker than the macOS one: Administrators and any holder of
//! `SeBackupPrivilege` can still read the file, like root on macOS.

use std::fs::File;
use std::path::Path;

use super::{
    Confirmation, Console, CrashReportOptOut, CrashReports, DaemonSpawn, Dialog,
    DialogAvailability, ForcedDumps, Host, OwnerOnly, PlatformError, Signals, ThreadQos,
};

pub(crate) mod acl;
mod desktop;
pub(crate) mod net;
pub(crate) mod process;
mod wer;

/// `FILE_FLAG_OPEN_REPARSE_POINT`: open a symbolic link or junction as itself rather than its
/// target, so the handle's file type says "link" and the caller refuses it.
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// The Windows arm of [`super::open_regular_no_follow`]. A symbolic link or a junction at `path`
/// opens as itself and is then refused by the caller.
pub(crate) fn open_read_no_follow(
    path: &Path,
    _expected: &std::fs::Metadata,
) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

pub struct Windows;

impl Host for Windows {
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

    /// For example `Windows 10.0.26200` on Windows 11 25H2.
    fn os_version(&self) -> Result<String, PlatformError> {
        os_version()
    }
}

/// The running system's version from `RtlGetVersion`
/// (<https://learn.microsoft.com/windows-hardware/drivers/ddi/wdm/nf-wdm-rtlgetversion>).
///
/// Not `GetVersionExW`, which reports the version the manifest declares support for (Windows 8
/// without one), and not the file version of `kernel32.dll`: an enablement-package release ships
/// the previous release's binaries, so build 26200 reads 10.0.26100 there. `windows-sys` carries
/// the declaration only in its driver-kit features, so it is written here and `raw-dylib` links
/// it from `ntdll.dll`.
fn os_version() -> Result<String, PlatformError> {
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32;
    }

    // SAFETY: `OSVERSIONINFOW` is plain data, so all zeroes is valid; the size field is set below.
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    // SAFETY: `info` is writable with its size set; `RtlGetVersion` always returns 0.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status != 0 {
        return Err(PlatformError::Io {
            path: std::path::PathBuf::new(),
            source: std::io::Error::other(format!(
                "RtlGetVersion returned NTSTATUS 0x{status:08x}"
            )),
        });
    }
    Ok(format!(
        "Windows {}.{}.{}",
        info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
    ))
}

impl OwnerOnly for Windows {
    /// Creates every missing level with the protected descriptor in `CreateDirectoryW`, then
    /// checks each created level and the leaf. Never `std::fs::create_dir_all`, whose directory
    /// would inherit its parent's ACL.
    fn create_dir(&self, path: &Path) -> Result<(), PlatformError> {
        acl::create_dir(path)
    }

    /// One exclusive `CreateDirectoryW` with the protected descriptor: the name must be free and
    /// the parent must exist.
    fn create_new_dir(&self, path: &Path) -> Result<(), PlatformError> {
        acl::create_one_dir(path)?;
        acl::check(path)
    }

    /// `CreateFileW` with `OPEN_ALWAYS`, the protected descriptor and
    /// `FILE_FLAG_OPEN_REPARSE_POINT`; the handle is checked before truncation, so a refusal leaves
    /// an existing file as it was.
    fn create_file(&self, path: &Path) -> Result<File, PlatformError> {
        acl::create_file(path, false)
    }

    fn create_new_file(&self, path: &Path) -> Result<File, PlatformError> {
        acl::create_file(path, true)
    }

    /// A protected DACL naming only the current user and `SYSTEM`, owned by the current user.
    fn check(&self, path: &Path) -> Result<(), PlatformError> {
        acl::check(path)
    }
}

impl Console for Windows {
    /// `GetConsoleMode` succeeds on both `CONIN$` and `CONOUT$`, and the process is outside
    /// session 0 on `WinSta0`: a process started without a console gets an invisible one that
    /// passes the first test alone. Any failure reads as "not interactive", so the one-time code
    /// is never printed where no person can see it.
    fn is_interactive(&self) -> bool {
        desktop::is_interactive()
    }
}

impl Dialog for Windows {
    /// Available only when this session is not session 0, equals the active console session, and
    /// its window station is `WinSta0`.
    fn availability(&self) -> DialogAvailability {
        desktop::availability()
    }

    /// `MessageBoxW`, gated on [`Dialog::availability`]. Only the Yes button is a confirmation,
    /// and a dialog that cannot be shown is never one.
    fn ask(&self, title: &str, body: &str) -> Result<Confirmation, PlatformError> {
        desktop::gated(self.availability(), || desktop::raise(title, body))
    }
}

impl CrashReports for Windows {
    /// `SetErrorMode` with `SEM_NOGPFAULTERRORBOX` and `WerSetFlags` with no UI and no heap, both
    /// read back. The machine-wide `LocalDumps` policy can still force a dump.
    fn opt_out(&self) -> Result<CrashReportOptOut, PlatformError> {
        wer::opt_out()
    }

    /// Whether the `LocalDumps` key exists and has a subkey named after this executable; no value
    /// under it is read.
    fn forced_dumps(&self) -> Result<ForcedDumps, PlatformError> {
        wer::forced_dumps()
    }
}
