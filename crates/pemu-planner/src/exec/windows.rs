//! The Windows half of [`super`]: owner-only scratch files by a protected DACL, and the confirmed
//! `COM<n>` port read with a DCB, polled read timeouts and an `EscapeCommFunction` reset pulse.
//!
//! The planner cannot depend on `pemu-host`, and needs what its owner-only service does not offer:
//! narrowing a file esptool wrote. The descriptor mirrors the host's (current user and `SYSTEM`,
//! full access, `SE_DACL_PROTECTED`), except that the scratch directory's ACEs are inheritable:
//! on Windows a directory's DACL does not guard its children the way 0700 does on macOS (traverse
//! checking is bypassed by default). Administrators and `SeBackupPrivilege` holders can still read
//! the files, like root on macOS.
//!
//! No test opens a port, so what a real port does with these calls is UNVERIFIED outside a manual
//! device run.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::Path;
use std::time::Duration;

use windows_sys::Win32::Devices::Communication::{
    CLRDTR, CLRRTS, COMMTIMEOUTS, DCB, EscapeCommFunction, GetCommState, NOPARITY, ONESTOPBIT,
    SETDTR, SETRTS, SetCommState, SetCommTimeouts,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_OPERATION_ABORTED, ERROR_SHARING_VIOLATION,
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, EqualSid,
    GetAce, GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    GetTokenInformation, IsWellKnownSid, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{ModemControl, ModemEscape};
use crate::flow::SessionError;

/// The `AceType` of an allow ACE (Microsoft Learn, "ACE_HEADER structure (winnt.h)").
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned open by `OpenProcessToken` and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

struct LocalMemory(*mut c_void);

impl Drop for LocalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: every constructor wraps a pointer a documented `LocalAlloc` producer handed
            // this module (`ConvertSidToStringSidW`, the SDDL conversion, `GetNamedSecurityInfoW`),
            // freed exactly once.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// The current user's SID from the process token (`GetTokenInformation`, `TokenUser`). The buffer
/// is kept because the SID pointer points into it.
struct UserSid {
    buffer: Vec<u64>,
}

impl UserSid {
    fn of_process() -> std::io::Result<UserSid> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no close; the token
        // out-parameter is a live local.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(last_error());
        }
        let token = OwnedHandle(token);
        let mut needed = 0u32;
        // SAFETY: a size query with a null buffer; the call writes the size it needs.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &raw mut needed)
        };
        // A `u64` buffer keeps the `TOKEN_USER` it holds aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        let len = u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX);
        // SAFETY: the buffer is `len` bytes, live and aligned for `TOKEN_USER`.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                len,
                &raw mut needed,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(UserSid { buffer })
    }

    fn sid(&self) -> PSID {
        // SAFETY: the buffer holds the `TOKEN_USER` `GetTokenInformation` wrote.
        unsafe { (*self.buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn to_sddl(&self) -> std::io::Result<String> {
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: the SID is valid while `self` lives; the call allocates the string, which is
        // freed by `LocalMemory`.
        if unsafe { ConvertSidToStringSidW(self.sid(), &raw mut text) } == 0 {
            return Err(last_error());
        }
        let owned = LocalMemory(text.cast());
        // SAFETY: a NUL-terminated UTF-16 string the call just wrote.
        let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
        // SAFETY: `len` code units precede the terminator.
        let units = unsafe { std::slice::from_raw_parts(text, len) };
        let string = String::from_utf16_lossy(units);
        drop(owned);
        Ok(string)
    }
}

/// A self-relative security descriptor built from SDDL, freed on drop.
struct Descriptor(LocalMemory);

impl Descriptor {
    /// `D:P` (a protected DACL) with one `FA` (file all access) allow ACE for the current user and
    /// one for `SY`, with `OICI` inheritance when `inherit` (Microsoft Learn, "ACE Strings").
    fn owner_only(inherit: bool) -> std::io::Result<Descriptor> {
        let user = UserSid::of_process()?.to_sddl()?;
        let flags = if inherit { "OICI" } else { "" };
        let sddl = format!("D:P(A;{flags};FA;;;{user})(A;{flags};FA;;;SY)");
        let text: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `text` is NUL-terminated and outlives the call; the descriptor is allocated by
        // the call and freed by `LocalMemory`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(Descriptor(LocalMemory(descriptor)))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: 0,
        }
    }

    fn dacl(&self) -> std::io::Result<*mut ACL> {
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: the descriptor is live; the out-parameters are live locals.
        let ok = unsafe {
            GetSecurityDescriptorDacl(
                self.0.0,
                &raw mut present,
                &raw mut dacl,
                &raw mut defaulted,
            )
        };
        if ok == 0 || present == 0 || dacl.is_null() {
            return Err(std::io::Error::other(
                "the owner-only descriptor has no DACL",
            ));
        }
        Ok(dacl)
    }
}

/// Creates one directory with the owner-only descriptor at creation. Fails with `AlreadyExists`
/// when the name is taken, so it is also an exclusive claim.
fn create_dir_with(path: &Path, inherit: bool) -> std::io::Result<()> {
    let descriptor = Descriptor::owner_only(inherit)?;
    let attributes = descriptor.attributes();
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated; the attributes and the descriptor they point to outlive
    // the call.
    if unsafe { CreateDirectoryW(name.as_ptr(), &raw const attributes) } == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// Creates the runner's scratch directory: missing parents as ordinary directories (the host
/// already made them private), the leaf owner-only with inheritable ACEs.
pub(super) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match create_dir_with(path, true) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// A protected DACL whose every ACE allows the current user or `SYSTEM` and nothing else.
pub(super) fn check_owner_only(path: &Path) -> Result<(), String> {
    let user = UserSid::of_process().map_err(|e| format!("the current user: {e}"))?;
    let name = wide(path);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `name` is NUL-terminated; the DACL pointer points into the descriptor, which the
    // call allocates and `LocalMemory` frees after the last use of the DACL below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != 0 {
        return Err(format!(
            "its security could not be read ({})",
            std::io::Error::from_raw_os_error(status as i32).kind()
        ));
    }
    let _descriptor = LocalMemory(descriptor);
    if dacl.is_null() {
        return Err("it has a null DACL, which grants everyone access".to_owned());
    }
    let (mut control, mut revision) = (0u16, 0u32);
    // SAFETY: the descriptor is live until `_descriptor` drops.
    if unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) } == 0
        || control & SE_DACL_PROTECTED == 0
    {
        return Err("its DACL is not protected, so it inherits from its parent".to_owned());
    }
    let mut size = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: the DACL is live; the out-structure is the size the call is told.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err("its DACL could not be read".to_owned());
    }
    for index in 0..size.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: `index` is below the ACE count of the live DACL.
        if unsafe { GetAce(dacl, index, &raw mut ace) } == 0 {
            return Err("an ACE of its DACL could not be read".to_owned());
        }
        // SAFETY: every ACE begins with an `ACE_HEADER`.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            return Err(format!("its DACL has an ACE of type {}", header.AceType));
        }
        // The SID of an allow ACE starts at `SidStart`, after the header and the mask
        // (Microsoft Learn, "ACCESS_ALLOWED_ACE structure").
        // SAFETY: the offset is inside the ACE, whose size the header vouches for.
        let sid: PSID =
            unsafe { ace.cast::<u8>().add(std::mem::size_of::<ACE_HEADER>() + 4) }.cast::<c_void>();
        // SAFETY: both SIDs are live for the calls.
        let allowed = unsafe {
            EqualSid(sid, user.sid()) != 0 || IsWellKnownSid(sid, WinLocalSystemSid) != 0
        };
        if !allowed {
            return Err("its DACL allows a SID other than this user and SYSTEM".to_owned());
        }
    }
    Ok(())
}

/// A directory, with the owner-only DACL. No owner test as on macOS: the DACL already names who can
/// open it, and an elevated token makes `BUILTIN\Administrators` the owner of what it creates.
pub(super) fn check_private_dir(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("scratch directory: {e}"))?;
    if !meta.is_dir() {
        return Err("the scratch directory is not an owner-only directory".to_owned());
    }
    check_owner_only(path)
        .map_err(|why| format!("the scratch directory is not an owner-only directory: {why}"))
}

/// Creates a new scratch file with the owner-only descriptor, refusing one that exists.
pub(super) fn create_private_file(path: &Path) -> std::io::Result<File> {
    let descriptor = Descriptor::owner_only(false)?;
    let attributes = descriptor.attributes();
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated; the attributes outlive the call. The handle returned is
    // owned by the `File` below.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            0,
            &raw const attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    // SAFETY: a valid, open handle nothing else owns.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Narrows a file esptool wrote to the owner-only protected DACL, then re-runs the check on it.
pub(super) fn narrow_private_file(path: &Path) -> Result<(), String> {
    let descriptor =
        Descriptor::owner_only(false).map_err(|e| format!("scratch file: {}", e.kind()))?;
    let dacl = descriptor
        .dacl()
        .map_err(|e| format!("scratch file: {}", e.kind()))?;
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated; the DACL points into `descriptor`, live for the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if status != 0 {
        return Err(format!(
            "scratch file: {}",
            std::io::Error::from_raw_os_error(status as i32).kind()
        ));
    }
    check_owner_only(path).map_err(|why| format!("scratch file: {why}"))
}

/// The DCB of the boot-console read, from the one the port has: 8N1, binary, no flow control (the
/// USB Serial/JTAG bridge drives no handshake line). DTR and RTS keep the driver's state, so a
/// plain read moves no line. The rate is kept: the bridge ignores it.
fn console_dcb(current: DCB) -> DCB {
    DCB {
        _bitfield: super::console_dcb_bits(current._bitfield),
        BaudRate: if current.BaudRate == 0 {
            115_200
        } else {
            current.BaudRate
        },
        ByteSize: 8,
        Parity: NOPARITY,
        StopBits: ONESTOPBIT,
        ..current
    }
}

/// Opens the confirmed port (`\\.\COM<n>`) for the boot console: `CreateFileW` with read access
/// only, no sharing and `OPEN_EXISTING`, as Microsoft Learn requires of a communications resource.
/// No byte is ever written. UNVERIFIED on the device: that the USB CDC driver accepts
/// `SetCommState` and `EscapeCommFunction` on a read-only handle (`ntddser.h` declares them
/// any-access).
///
/// A port another program holds answers `ERROR_ACCESS_DENIED` or a sharing violation:
/// [`SessionError::Busy`].
pub(super) fn open_console(path: &str) -> Result<File, SessionError> {
    let name: Vec<u16> = path.encode_utf16().chain([0]).collect();
    // SAFETY: `name` is NUL-terminated; no security attributes and no template. The handle is
    // owned by the `File` below.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = last_error();
        return Err(match error.raw_os_error().map(|c| c as u32) {
            Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION) => SessionError::Busy,
            _ => SessionError::Failed(format!(
                "the boot console did not open: {} (Windows error {})",
                error.kind(),
                error.raw_os_error().unwrap_or(0)
            )),
        });
    }
    // SAFETY: a valid, open handle nothing else owns.
    let file = unsafe { File::from_raw_handle(handle) };
    configure(&file)?;
    Ok(file)
}

fn configure(file: &File) -> Result<(), SessionError> {
    let handle = file.as_raw_handle();
    let failed = |what: &str| {
        let error = last_error();
        SessionError::Failed(format!(
            "the boot console could not be configured: {what} failed with {} (Windows error {})",
            error.kind(),
            error.raw_os_error().unwrap_or(0)
        ))
    };
    let mut dcb = DCB {
        DCBlength: std::mem::size_of::<DCB>() as u32,
        ..DCB::default()
    };
    // SAFETY: the handle is open; `dcb` is a live DCB with its length set, as the call requires.
    if unsafe { GetCommState(handle, &raw mut dcb) } == 0 {
        return Err(failed("GetCommState"));
    }
    let dcb = console_dcb(dcb);
    let set = || {
        // SAFETY: as above; the call reads the DCB.
        if unsafe { SetCommState(handle, &raw const dcb) } == 0 {
            Err(last_error().raw_os_error().unwrap_or(0))
        } else {
            Ok(())
        }
    };
    if retry_once_on_abort(set).is_err() {
        return Err(failed("SetCommState"));
    }
    let (interval, multiplier, constant) = super::POLLED_READ_TIMEOUTS;
    let timeouts = COMMTIMEOUTS {
        ReadIntervalTimeout: interval,
        ReadTotalTimeoutMultiplier: multiplier,
        ReadTotalTimeoutConstant: constant,
        WriteTotalTimeoutMultiplier: 0,
        WriteTotalTimeoutConstant: 0,
    };
    // SAFETY: as above; the call reads the structure.
    if unsafe { SetCommTimeouts(handle, &raw const timeouts) } == 0 {
        return Err(failed("SetCommTimeouts"));
    }
    Ok(())
}

/// Runs `call` and, if it fails with `ERROR_OPERATION_ABORTED` (995), runs it once more.
///
/// On the device, `SetCommState` failed once with 995 on the first open of the port and succeeded
/// on the next: the driver cancelled the request rather than refusing the settings (why
/// `usbser.sys` does so is UNVERIFIED). One retry, not a loop, so a port that keeps aborting is
/// still reported.
fn retry_once_on_abort(mut call: impl FnMut() -> Result<(), i32>) -> Result<(), i32> {
    match call() {
        Err(code) if code == ERROR_OPERATION_ABORTED as i32 => call(),
        other => other,
    }
}

/// The modem lines of an open port, driven by `EscapeCommFunction`.
struct PortLines<'a>(&'a File);

impl ModemControl for PortLines<'_> {
    fn escape(&mut self, function: ModemEscape) -> std::io::Result<()> {
        let code = match function {
            ModemEscape::SetDtr => SETDTR,
            ModemEscape::ClearDtr => CLRDTR,
            ModemEscape::SetRts => SETRTS,
            ModemEscape::ClearRts => CLRRTS,
        };
        // SAFETY: the handle is open for the life of the borrow.
        if unsafe { EscapeCommFunction(self.0.as_raw_handle(), code) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }
}

/// Restarts the device on the open, read-only handle by pulsing its reset line
/// ([`super::RESET_STATES`]); no byte is written.
pub(super) fn pulse_reset(file: &File, hold: Duration) -> Result<(), SessionError> {
    super::drive_reset(&mut PortLines(file), hold, &mut std::thread::sleep)
}

/// An exclusive, non-inheriting owner-only directory, the shape `pemu-host`'s `create_new_dir`
/// gives a backup run directory, so the tests can pin that the descriptor is the same.
#[cfg(test)]
fn create_owner_only_dir(path: &Path) -> std::io::Result<()> {
    create_dir_with(path, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_comm_state_retries_once_on_operation_aborted_only() {
        let abort = ERROR_OPERATION_ABORTED as i32;
        let run = |results: &[Result<(), i32>]| {
            let mut calls = 0;
            let out = retry_once_on_abort(|| {
                calls += 1;
                results[calls - 1]
            });
            (out, calls)
        };
        assert_eq!(run(&[Ok(())]), (Ok(()), 1));
        assert_eq!(run(&[Err(abort), Ok(())]), (Ok(()), 2));
        assert_eq!(run(&[Err(abort), Err(abort)]), (Err(abort), 2));
        assert_eq!(run(&[Err(5)]), (Err(5), 1), "access denied is not retried");
    }

    fn fresh(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "pemu-planner-win-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn set_sddl(path: &Path, sddl: &str) {
        let text: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: as in `Descriptor::owner_only`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "SDDL {sddl}");
        let descriptor = Descriptor(LocalMemory(descriptor));
        let dacl = descriptor.dacl().expect("a DACL");
        let name = wide(path);
        // SAFETY: as in `narrow_private_file`.
        let status = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        assert_eq!(status, 0, "SetNamedSecurityInfoW");
    }

    #[test]
    fn the_owner_only_descriptor_passes_and_an_inherited_acl_does_not() {
        let dir = fresh("dacl");
        create_private_dir(&dir).expect("create");
        check_private_dir(&dir).expect("owner-only at creation");
        let file = dir.join("f.bin");
        drop(create_private_file(&file).expect("create file"));
        check_owner_only(&file).expect("the file is owner-only at creation");
        create_private_file(&file).expect_err("CREATE_NEW refuses an existing file");

        let plain = fresh("plain");
        std::fs::create_dir(&plain).expect("plain dir");
        let why = check_private_dir(&plain).expect_err("an inherited ACL is refused");
        assert!(why.contains("not protected"), "{why}");

        // A foreign ACE (Everyone, `WD`) on a protected DACL is refused by the whitelist.
        let foreign = dir.join("foreign.bin");
        std::fs::write(&foreign, b"x").expect("write");
        set_sddl(&foreign, "D:P(A;;FA;;;SY)(A;;FR;;;WD)");
        let why = check_owner_only(&foreign).expect_err("a foreign ACE is refused");
        assert!(why.contains("other than"), "{why}");
        narrow_private_file(&foreign).expect("narrowed");
        check_owner_only(&foreign).expect("owner-only after narrowing");

        let claimed = dir.join("run1");
        create_owner_only_dir(&claimed).expect("claim");
        check_owner_only(&claimed).expect("owner-only");
        let again = create_owner_only_dir(&claimed).expect_err("taken");
        assert_eq!(again.kind(), std::io::ErrorKind::AlreadyExists);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir(&plain);
    }

    #[test]
    fn the_console_dcb_is_8n1_without_flow_control() {
        let current = DCB {
            DCBlength: std::mem::size_of::<DCB>() as u32,
            BaudRate: 0,
            _bitfield: u32::MAX,
            ByteSize: 7,
            Parity: 2,
            StopBits: 2,
            ..DCB::default()
        };
        let dcb = console_dcb(current);
        assert_eq!(
            (dcb.ByteSize, dcb.Parity, dcb.StopBits),
            (8, NOPARITY, ONESTOPBIT)
        );
        assert_eq!(dcb.BaudRate, 115_200);
        assert_eq!(dcb._bitfield, super::super::console_dcb_bits(u32::MAX));
    }
}
