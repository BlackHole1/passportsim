//! The Windows Error Reporting opt-out: guest RAM and flash live in this process's heap, so a
//! crash report carrying the heap could carry them off the machine. Two per-process calls, no
//! registry write:
//!
//! 1. `SetErrorMode(GetErrorMode() | SEM_NOGPFAULTERRORBOX)`: "The system does not invoke Windows
//!    Error Reporting". Process-wide and inherited by children; existing bits are kept, because
//!    threads setting different modes make error handling inconsistent.
//! 2. `WerSetFlags(WER_FAULT_REPORTING_NO_UI | WER_FAULT_REPORTING_FLAG_NOHEAP)`, for report paths
//!    that ignore the error mode: whatever WER still reports carries no heap. Which fail-fast
//!    paths bypass the error mode is UNVERIFIED; the heap flag holds either way.
//!
//! Not `WerAddExcludedApplication`: it writes the file name into the user's registry, where it
//! outlives the process and covers every program of that name.
//!
//! Neither call overrides the machine policy `LocalDumps`, which works "even if WER is disabled"
//! (Microsoft Learn, "Collecting User-Mode Dumps"). `doctor` reports it ([`forced_dumps`]): the
//! key means every executable, a subkey named after this one means it by name. Only the keys are
//! opened, never a value, so the dump folder never reaches a report.

use windows_sys::Win32::System::Diagnostics::Debug::{
    GetErrorMode, SEM_NOGPFAULTERRORBOX, SetErrorMode,
};
use windows_sys::Win32::System::ErrorReporting::{
    WER_FAULT_REPORTING_FLAG_NOHEAP, WER_FAULT_REPORTING_NO_UI, WerGetFlags, WerSetFlags,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegCloseKey, RegOpenKeyExW,
};

use super::super::{CrashReportOptOut, ForcedDumps, PlatformError};

/// The machine policy key of "Collecting User-Mode Dumps" (Microsoft Learn).
const LOCAL_DUMPS: &str = r"SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps";

pub(crate) fn forced_dumps() -> Result<ForcedDumps, PlatformError> {
    let exe =
        std::env::current_exe().map_err(|e| PlatformError::io(std::path::PathBuf::new(), e))?;
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    policy(HKEY_LOCAL_MACHINE, LOCAL_DUMPS, &name)
        .map_err(|e| PlatformError::io(std::path::PathBuf::new(), e))
}

fn policy(root: HKEY, path: &str, exe: &str) -> std::io::Result<ForcedDumps> {
    let Some(key) = open(root, path)? else {
        return Ok(ForcedDumps::Absent);
    };
    let named = open(key.0, exe)?;
    Ok(match named {
        Some(_) => ForcedDumps::ThisExecutable,
        None => ForcedDumps::EveryExecutable,
    })
}

/// An open registry key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: a key this process opened and has not closed.
        unsafe { RegCloseKey(self.0) };
    }
}

/// Opens `path` under `root` for reading in the 64-bit view; `None` when the key does not exist.
fn open(root: HKEY, path: &str) -> std::io::Result<Option<Key>> {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: a NUL-terminated name and an out parameter this function owns.
    let status =
        unsafe { RegOpenKeyExW(root, wide.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut key) };
    match status {
        ERROR_SUCCESS => Ok(Some(Key(key))),
        ERROR_FILE_NOT_FOUND => Ok(None),
        other => Err(std::io::Error::from_raw_os_error(other as i32)),
    }
}

pub(crate) const WER_FLAGS: u32 = WER_FAULT_REPORTING_NO_UI | WER_FAULT_REPORTING_FLAG_NOHEAP;

/// Applies both calls and reads both back, so a success is what the system holds.
pub(crate) fn opt_out() -> Result<CrashReportOptOut, PlatformError> {
    // SAFETY: both calls take and return plain integers and have no failure mode.
    unsafe { SetErrorMode(GetErrorMode() | SEM_NOGPFAULTERRORBOX) };
    // SAFETY: takes a flag word only.
    let set = unsafe { WerSetFlags(WER_FLAGS) };
    if set < 0 {
        return Err(PlatformError::io(
            std::path::PathBuf::new(),
            std::io::Error::from_raw_os_error(set),
        ));
    }
    let (mode, flags) = read_back()?;
    if mode & SEM_NOGPFAULTERRORBOX == 0 || flags & WER_FLAGS != WER_FLAGS {
        return Err(PlatformError::io(
            std::path::PathBuf::new(),
            std::io::Error::other(format!(
                "the WER opt-out did not hold: error mode {mode:#x}, WER flags {flags:#x}"
            )),
        ));
    }
    Ok(CrashReportOptOut::OptedOut)
}

pub(crate) fn read_back() -> Result<(u32, u32), PlatformError> {
    let mut flags = 0u32;
    // SAFETY: the pseudo-handle of this process needs no closing, and `flags` is a valid
    // out-pointer.
    let got = unsafe { WerGetFlags(GetCurrentProcess(), &mut flags) };
    if got < 0 {
        return Err(PlatformError::io(
            std::path::PathBuf::new(),
            std::io::Error::from_raw_os_error(got),
        ));
    }
    // SAFETY: no arguments, no failure mode.
    Ok((unsafe { GetErrorMode() }, flags))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing is written to the registry.
    #[test]
    fn the_opt_out_holds_as_read_back_from_the_system() {
        assert_eq!(opt_out().expect("opted out"), CrashReportOptOut::OptedOut);
        let (mode, flags) = read_back().expect("read back");
        assert_ne!(mode & SEM_NOGPFAULTERRORBOX, 0, "error mode {mode:#x}");
        assert_eq!(flags & WER_FLAGS, WER_FLAGS, "WER flags {flags:#x}");
        assert_eq!(opt_out().expect("idempotent"), CrashReportOptOut::OptedOut);
    }

    /// On a scratch copy of the key's shape under `HKEY_CURRENT_USER` (`HKEY_LOCAL_MACHINE` needs
    /// an administrator), removed at the end.
    #[test]
    fn the_policy_read_tells_absent_every_executable_and_this_one_apart() {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, RegCreateKeyExW, RegDeleteTreeW,
        };
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let create = |path: &str| {
            let mut key: HKEY = std::ptr::null_mut();
            // SAFETY: a NUL-terminated name and out parameters this test owns.
            let status = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(path).as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_WRITE,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(status, ERROR_SUCCESS, "create {path}");
            drop(Key(key));
        };
        let root = format!(r"Software\passportsim-test-wer-{}", std::process::id());
        let dumps = format!(r"{root}\LocalDumps");
        let answer = || policy(HKEY_CURRENT_USER, &dumps, "passportsim.exe").expect("read");

        assert_eq!(answer(), ForcedDumps::Absent);
        create(&format!(r"{dumps}\other.exe"));
        assert_eq!(answer(), ForcedDumps::EveryExecutable);
        create(&format!(r"{dumps}\PASSPORTSIM.EXE"));
        assert_eq!(
            answer(),
            ForcedDumps::ThisExecutable,
            "registry key names compare without case"
        );

        // SAFETY: a NUL-terminated name of the scratch tree this test created.
        let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&root).as_ptr()) };
        assert_eq!(status, ERROR_SUCCESS, "remove the scratch tree");
        assert_eq!(answer(), ForcedDumps::Absent);
    }

    #[test]
    fn the_machine_policy_reads() {
        let answer = forced_dumps().expect("HKLM is readable by every user");
        assert_ne!(answer, ForcedDumps::NotApplicable);
        eprintln!("LocalDumps on this host: {answer:?}");
    }
}
