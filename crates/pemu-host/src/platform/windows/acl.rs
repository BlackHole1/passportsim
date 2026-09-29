//! Windows owner-only protection: one protected descriptor per process, carried in the
//! `SECURITY_ATTRIBUTES` of every create call, and a read check that walks the DACL as a whitelist.
//!
//! The descriptor is the SDDL `O:<user>D:P(A;;FA;;;<user>)(A;;FA;;;SY)` (Microsoft Learn,
//! "Security Descriptor String Format", "ACE Strings"): owner the current user, a protected DACL
//! (nothing inherited) with two non-inheritable full-access allow ACEs, the user and `SYSTEM`.
//! As a self-relative descriptor it is one allocation with no internal pointers, so it lives in a
//! `static`. The owner is explicit because an elevated token's default owner is
//! `BUILTIN\Administrators`, not the user.
//!
//! It applies only when a call creates the object; an existing object keeps its descriptor and
//! the read check decides. Nothing here calls `SetNamedSecurityInfoW` outside a test.
//!
//! The read check accepts only a present, protected DACL whose ACEs are all non-inherited allow or
//! deny ACEs naming the current user or `SYSTEM`, owned by the current user. It is a whitelist
//! because a "no SID but the user" check would refuse the `SYSTEM` ACE the descriptor grants.
//!
//! Weaker than macOS: Administrators and `SeBackupPrivilege` holders can still read the file, and
//! the existing ancestors of a role directory are not inspected (the macOS arm's ancestor rule).

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
    GetLengthSid, GetSecurityDescriptorControl, GetTokenInformation, INHERITED_ACE, IsValidSid,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    SECURITY_MAX_SID_SIZE, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
    WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
    READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::super::PlatformError;

/// `ACCESS_ALLOWED_ACE_TYPE` (`winnt.h`): an allow ACE whose SID follows the access mask.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// `ACCESS_DENIED_ACE_TYPE` (`winnt.h`): a deny ACE with the same layout as an allow ACE.
const ACCESS_DENIED_ACE_TYPE: u8 = 1;

/// A path as the NUL-terminated UTF-16 a `W` call takes. An interior NUL would silently cut the
/// path short, so it is refused.
pub(crate) fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut text: Vec<u16> = path.as_os_str().encode_wide().collect();
    if text.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path holds a NUL character",
        ));
    }
    text.push(0);
    Ok(text)
}

pub(crate) fn wide_str(text: &str) -> io::Result<Vec<u16>> {
    wide(Path::new(text))
}

/// An owned copy of a SID, in a `u32` buffer because a SID's sub-authorities are 32-bit words and
/// the Win32 calls expect them aligned.
pub(crate) struct Sid(Box<[u32]>);

impl Sid {
    /// Copies the SID `psid` points to.
    /// # Safety
    ///
    /// `psid` must point to a valid SID for the duration of the call.
    unsafe fn copy_of(psid: PSID) -> io::Result<Sid> {
        // SAFETY: the caller guarantees `psid` is a valid SID for this call.
        if unsafe { IsValidSid(psid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a valid SID",
            ));
        }
        // SAFETY: as above; `GetLengthSid` only reads the SID's header.
        let len = unsafe { GetLengthSid(psid) } as usize;
        let mut words = vec![0u32; len.div_ceil(4)].into_boxed_slice();
        // SAFETY: `words` holds at least `len` bytes, and `psid` is valid for `len` bytes.
        unsafe { std::ptr::copy_nonoverlapping(psid as *const u8, words.as_mut_ptr().cast(), len) };
        Ok(Sid(words))
    }

    fn well_known(kind: i32) -> io::Result<Sid> {
        let mut words = vec![0u32; SECURITY_MAX_SID_SIZE as usize / 4].into_boxed_slice();
        let mut size = SECURITY_MAX_SID_SIZE;
        // SAFETY: `words` holds `size` bytes, and a null domain SID is what a well-known SID with
        // no domain part takes.
        let ok = unsafe {
            CreateWellKnownSid(
                kind,
                std::ptr::null_mut(),
                words.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Sid(words))
    }

    pub(crate) fn as_psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }

    /// Whether `psid` names the same account.
    ///
    /// # Safety
    ///
    /// `psid` must point to a valid SID for the duration of the call.
    unsafe fn equals(&self, psid: PSID) -> bool {
        // SAFETY: both SIDs are valid for the call; `EqualSid` only reads them.
        unsafe { EqualSid(self.as_psid(), psid) != 0 }
    }

    /// The `S-1-5-...` form, for SDDL and for a refusal that names the account it saw.
    pub(crate) fn to_string_form(&self) -> io::Result<String> {
        // SAFETY: `self` is a valid SID copied from the system.
        unsafe { sid_string(self.as_psid()) }
    }
}

///
/// # Safety
///
/// `psid` must point to a valid SID for the duration of the call.
unsafe fn sid_string(psid: PSID) -> io::Result<String> {
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: `psid` is valid per the caller; `raw` is a valid out-pointer that receives a
    // `LocalAlloc` string on success, freed below.
    if unsafe { ConvertSidToStringSidW(psid, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `raw` is a NUL-terminated UTF-16 string.
    let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
    // SAFETY: `len` units before the terminator were just read.
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) });
    // SAFETY: `raw` is the `LocalAlloc` allocation of the call above, freed exactly once.
    unsafe { LocalFree(raw.cast()) };
    Ok(text)
}

pub(crate) struct Identity {
    pub(crate) user: Sid,
    system: Sid,
    /// The default owner of objects this token creates (`TokenOwner`): the user, or
    /// `BUILTIN\Administrators` under an elevated administrator token.
    pub(crate) token_owner: Sid,
}

/// One token query, into a buffer aligned for the pointer-holding structure it returns.
fn token_information(class: i32) -> io::Result<Box<[u64]>> {
    let mut raw: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` is a pseudo-handle that needs no closing, and `raw` is a valid
    // out-pointer that receives a token handle owned below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is the token handle just opened, owned and closed exactly once by `token`.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut needed = 0u32;
    // SAFETY: a null buffer with length 0 is the documented way to ask for the size.
    let sized = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    let error = io::Error::last_os_error();
    if sized != 0 || error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(error);
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)].into_boxed_slice();
    // SAFETY: `buffer` holds at least `needed` bytes and is 8-byte aligned.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buffer)
}

fn load_identity() -> io::Result<Identity> {
    let user = token_information(TokenUser)?;
    // SAFETY: the buffer holds a `TOKEN_USER` whose SID points into the same buffer, which is
    // alive for the copy.
    let user = unsafe { Sid::copy_of((*(user.as_ptr() as *const TOKEN_USER)).User.Sid)? };
    let owner = token_information(TokenOwner)?;
    // SAFETY: as above, for `TOKEN_OWNER`.
    let token_owner = unsafe { Sid::copy_of((*(owner.as_ptr() as *const TOKEN_OWNER)).Owner)? };
    Ok(Identity {
        user,
        system: Sid::well_known(WinLocalSystemSid)?,
        token_owner,
    })
}

/// The identity of this process, read once. A failure is kept as its OS error code so every
/// later call reports the same cause.
pub(crate) fn identity() -> Result<&'static Identity, PlatformError> {
    static IDENTITY: OnceLock<Result<Identity, i32>> = OnceLock::new();
    IDENTITY
        .get_or_init(|| load_identity().map_err(|e| e.raw_os_error().unwrap_or(0)))
        .as_ref()
        .map_err(|&code| PlatformError::io(PathBuf::new(), io::Error::from_raw_os_error(code)))
}

/// The self-relative protected descriptor of this process.
struct Descriptor(PSECURITY_DESCRIPTOR);

// SAFETY: written once by `ConvertStringSecurityDescriptorToSecurityDescriptorW`, never freed or
// written again; the Win32 calls that receive it only read it.
unsafe impl Send for Descriptor {}
// SAFETY: as above; shared reads of an immutable allocation.
unsafe impl Sync for Descriptor {}

pub(crate) fn owner_only_sddl(user: &str) -> String {
    format!("O:{user}D:P(A;;FA;;;{user})(A;;FA;;;SY)")
}

/// A descriptor built from SDDL, owned by the caller and freed with `LocalFree`.
pub(crate) fn descriptor_from_sddl(sddl: &str) -> io::Result<PSECURITY_DESCRIPTOR> {
    let text = wide_str(sddl)?;
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `text` is NUL-terminated and alive for the call; `sd` receives a `LocalAlloc`
    // allocation on success that the caller owns; the size out-parameter is optional.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sd)
}

fn descriptor() -> Result<&'static Descriptor, PlatformError> {
    static DESCRIPTOR: OnceLock<Result<Descriptor, i32>> = OnceLock::new();
    let id = identity()?;
    DESCRIPTOR
        .get_or_init(|| {
            id.user
                .to_string_form()
                .and_then(|user| descriptor_from_sddl(&owner_only_sddl(&user)))
                .map(Descriptor)
                .map_err(|e| e.raw_os_error().unwrap_or(0))
        })
        .as_ref()
        .map_err(|&code| PlatformError::io(PathBuf::new(), io::Error::from_raw_os_error(code)))
}

/// `SECURITY_ATTRIBUTES` carrying the protected descriptor, with no handle inheritance.
fn attributes(descriptor: &'static Descriptor) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    }
}

/// One `CreateDirectoryW` carrying the protected descriptor. `AlreadyExists` when the name is
/// taken, so a caller that wants a fresh directory can try the next name.
pub(crate) fn create_one_dir(path: &Path) -> Result<(), PlatformError> {
    let sa = attributes(descriptor()?);
    let name = wide(path).map_err(|e| PlatformError::io(path, e))?;
    // SAFETY: `name` is NUL-terminated and `sa` points to a live descriptor; both outlive the call.
    if unsafe { CreateDirectoryW(name.as_ptr(), &sa) } == 0 {
        let error = io::Error::last_os_error();
        let error = match error.raw_os_error() {
            Some(code) if code == ERROR_ALREADY_EXISTS as i32 => {
                io::Error::from(io::ErrorKind::AlreadyExists)
            }
            _ => error,
        };
        return Err(PlatformError::io(path, error));
    }
    Ok(())
}

/// Creates `path` and every missing parent with the protected descriptor, then checks every
/// directory it created and the leaf. Pre-existing ancestors are not checked, and an existing
/// leaf is re-checked rather than repaired.
pub(crate) fn create_dir(path: &Path) -> Result<(), PlatformError> {
    let existing = path
        .ancestors()
        .find(|a| !a.as_os_str().is_empty() && a.exists())
        .map(Path::to_path_buf);
    let mut levels: Vec<PathBuf> = path
        .ancestors()
        .take_while(|a| !a.as_os_str().is_empty() && Some(*a) != existing.as_deref())
        .map(Path::to_path_buf)
        .collect();
    if levels.is_empty() {
        levels.push(path.to_path_buf());
    }
    levels.reverse();
    for level in &levels {
        match create_one_dir(level) {
            Ok(()) => {}
            // A second creator raced this one, or the leaf already existed: the check decides.
            Err(PlatformError::Io { source, .. })
                if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    for level in &levels {
        let meta = std::fs::metadata(level).map_err(|e| PlatformError::io(level, e))?;
        if !meta.is_dir() {
            return Err(PlatformError::io(
                level,
                io::Error::new(io::ErrorKind::AlreadyExists, "a file is in the way"),
            ));
        }
        check(level)?;
    }
    Ok(())
}

/// Opens `path` for writing with the protected descriptor: `CREATE_NEW` when `new`, else
/// `OPEN_ALWAYS`, the check, and only then a truncation.
///
/// `FILE_FLAG_OPEN_REPARSE_POINT` opens a link or junction as itself, so the write can never be
/// redirected out of the role directory; a reparse point is refused before a byte is written. The
/// check runs on the **handle**, so what is checked is what was opened, and before the
/// truncation, so a refusal leaves an existing file as it was.
pub(crate) fn create_file(path: &Path, new: bool) -> Result<File, PlatformError> {
    let sa = attributes(descriptor()?);
    let name = wide(path).map_err(|e| PlatformError::io(path, e))?;
    // SAFETY: `name` is NUL-terminated and `sa` points to a live descriptor; both outlive the
    // call, and a null template handle is allowed.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE | READ_CONTROL | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &sa,
            if new { CREATE_NEW } else { OPEN_ALWAYS },
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(PlatformError::io(path, io::Error::last_os_error()));
    }
    // SAFETY: `handle` is a valid file handle just opened, owned from here by `file`.
    let file = unsafe { File::from_raw_handle(handle) };
    let meta = file.metadata().map_err(|e| PlatformError::io(path, e))?;
    if !meta.is_file() {
        return Err(PlatformError::io(
            path,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a link or junction at the target is not a file this call may create",
            ),
        ));
    }
    check_handle(path, &file)?;
    if !new {
        file.set_len(0).map_err(|e| PlatformError::io(path, e))?;
    }
    Ok(file)
}

/// The read check on a path, following links as an open would.
pub(crate) fn check(path: &Path) -> Result<(), PlatformError> {
    let name = wide(path).map_err(|e| PlatformError::io(path, e))?;
    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `name` is NUL-terminated; the out-pointers are valid, and on success `sd` is a
    // `LocalAlloc` allocation that `owner` and `dacl` point into, freed by `verify_and_free`.
    let status = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if status != 0 {
        return Err(PlatformError::io(
            path,
            io::Error::from_raw_os_error(status as i32),
        ));
    }
    // SAFETY: the three pointers are the successful result of the call above.
    unsafe { verify_and_free(path, owner, dacl, sd) }
}

/// The read check on an open handle (`GetSecurityInfo`); the handle needs `READ_CONTROL`.
fn check_handle(path: &Path, file: &File) -> Result<(), PlatformError> {
    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `file` is an open handle with `READ_CONTROL`; the out-pointers are as in `check`.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if status != 0 {
        return Err(PlatformError::io(
            path,
            io::Error::from_raw_os_error(status as i32),
        ));
    }
    // SAFETY: the three pointers are the successful result of the call above.
    unsafe { verify_and_free(path, owner, dacl, sd) }
}

/// [`verify`], then frees the descriptor whatever the answer.
///
/// # Safety
///
/// `owner` and `dacl` must point into `sd`, a `LocalAlloc` descriptor returned by a
/// `Get*SecurityInfo` call, which this function frees.
unsafe fn verify_and_free(
    path: &Path,
    owner: PSID,
    dacl: *mut ACL,
    sd: PSECURITY_DESCRIPTOR,
) -> Result<(), PlatformError> {
    // SAFETY: the caller's guarantee.
    let answer = unsafe { verify(path, owner, dacl, sd) };
    // SAFETY: `sd` is the caller's `LocalAlloc` descriptor, freed exactly once, after its last use.
    unsafe { LocalFree(sd) };
    answer
}

/// The whitelist of the module doc over a descriptor's owner, control bits and DACL.
///
/// # Safety
///
/// `owner` and `dacl` must point into the live descriptor `sd` (or be null).
unsafe fn verify(
    path: &Path,
    owner: PSID,
    dacl: *mut ACL,
    sd: PSECURITY_DESCRIPTOR,
) -> Result<(), PlatformError> {
    let id = identity()?;
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `sd` is a live descriptor; both out-pointers are valid.
    if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
        return Err(PlatformError::io(path, io::Error::last_os_error()));
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(PlatformError::wider(
            path,
            "its DACL inherits from the parent directory (SE_DACL_PROTECTED is not set)",
        ));
    }
    // SAFETY: `owner` is null or a SID inside `sd`.
    if owner.is_null() || !unsafe { id.user.equals(owner) } {
        let who = match owner.is_null() {
            true => "no owner".to_string(),
            // SAFETY: `owner` is a SID inside the live `sd`.
            false => unsafe { sid_string(owner) }.unwrap_or_else(|_| "an unreadable SID".into()),
        };
        return Err(PlatformError::wider(
            path,
            format!("it is owned by {who}, not by the current user"),
        ));
    }
    if dacl.is_null() {
        return Err(PlatformError::wider(
            path,
            "it has a NULL DACL, which grants everyone full access",
        ));
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` is a live ACL and `size` is the structure `AclSizeInformation` fills.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(PlatformError::io(path, io::Error::last_os_error()));
    }
    for index in 0..size.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: `index` is below the ACL's ACE count, and `ace` receives a pointer into `dacl`.
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(PlatformError::io(path, io::Error::last_os_error()));
        }
        // SAFETY: every ACE starts with an `ACE_HEADER`.
        let header = unsafe { *(ace as *const ACE_HEADER) };
        if u32::from(header.AceFlags) & INHERITED_ACE != 0 {
            return Err(PlatformError::wider(
                path,
                format!("ACE {index} is inherited from a parent directory"),
            ));
        }
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE && header.AceType != ACCESS_DENIED_ACE_TYPE {
            return Err(PlatformError::wider(
                path,
                format!(
                    "ACE {index} is of type {}, which the owner-only check does not accept",
                    header.AceType
                ),
            ));
        }
        // SAFETY: an allow and a deny ACE share `ACCESS_ALLOWED_ACE`'s layout, whose SID starts
        // at `SidStart`; the SID lies inside the ACE, which lies inside `dacl`.
        let sid =
            unsafe { std::ptr::addr_of!((*(ace as *const ACCESS_ALLOWED_ACE)).SidStart) } as PSID;
        // SAFETY: `sid` is a SID inside the live ACL.
        let known = unsafe { id.user.equals(sid) || id.system.equals(sid) };
        if !known {
            // SAFETY: as above.
            let who = unsafe { sid_string(sid) }.unwrap_or_else(|_| "an unreadable SID".into());
            return Err(PlatformError::wider(
                path,
                format!("ACE {index} names {who}, which is neither the current user nor SYSTEM"),
            ));
        }
    }
    Ok(())
}

/// The owner SID of `path` (followed), or `None` when it cannot be read.
fn owner_of(path: &Path) -> Option<Sid> {
    let name = wide(path).ok()?;
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: as in `check`, with only the owner requested.
    let status = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: `owner` is null or a SID inside `sd`, which is alive until the free below.
    let sid = (!owner.is_null())
        .then(|| unsafe { Sid::copy_of(owner) }.ok())
        .flatten();
    // SAFETY: `sd` is the call's `LocalAlloc` descriptor, freed exactly once.
    unsafe { LocalFree(sd) };
    sid
}

/// Whether `path` (followed) belongs to this user: its owner is the token's user SID, or the
/// token's default owner (`BUILTIN\Administrators` for what an elevated session of this user
/// created, such as a `git clone` run as administrator).
pub(crate) fn owned_by_current_user(path: &Path) -> bool {
    let (Ok(id), Some(owner)) = (identity(), owner_of(path)) else {
        return false;
    };
    // SAFETY: `owner` is a valid SID copied from the system.
    unsafe { id.user.equals(owner.as_psid()) || id.token_owner.equals(owner.as_psid()) }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write as _;

    use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    use super::super::super::scratch_dir;
    use super::*;

    /// Replaces the DACL of `path` with `dacl_sddl`, the way another tool could have left it.
    /// Test-only: the product never rewrites a descriptor.
    pub(crate) fn set_dacl(path: &Path, dacl_sddl: &str, protected: bool) {
        let sd = descriptor_from_sddl(dacl_sddl).expect("the test SDDL parses");
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `sd` is the live descriptor just built; the out-pointers are valid.
        let ok = unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) };
        assert!(ok != 0 && present != 0, "the test SDDL carries a DACL");
        let name = wide(path).expect("a path");
        let flag = match protected {
            true => PROTECTED_DACL_SECURITY_INFORMATION,
            false => UNPROTECTED_DACL_SECURITY_INFORMATION,
        };
        // SAFETY: `name` is NUL-terminated and `dacl` points into the live `sd`.
        let status = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | flag,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        // SAFETY: `sd` is the `LocalAlloc` descriptor built above, freed once.
        unsafe { LocalFree(sd) };
        assert_eq!(status, 0, "SetNamedSecurityInfoW");
    }

    pub(crate) fn user() -> String {
        identity()
            .expect("identity")
            .user
            .to_string_form()
            .expect("sid")
    }

    /// Adds a read ACE for Everyone (`WD`): the foreign ACE the owner-only tests refuse.
    pub(crate) fn widen(path: &Path) {
        let u = user();
        set_dacl(
            path,
            &format!("D:(A;;FA;;;{u})(A;;FA;;;SY)(A;;FR;;;WD)"),
            true,
        );
    }

    #[test]
    fn a_created_file_carries_the_protected_two_ace_dacl_and_a_foreign_ace_is_refused() {
        let dir = scratch_dir("win-acl-file");
        create_dir(&dir).expect("owner-only directory");
        let file = dir.join("token");
        drop(create_file(&file, false).expect("owner-only file"));
        check(&file).expect("the file it just created is owner-only");
        assert!(owned_by_current_user(&file));

        widen(&file);
        let refused = check(&file)
            .expect_err("a foreign ACE is refused")
            .to_string();
        assert!(
            refused.contains("not owner-only") && refused.contains("S-1-1-0"),
            "the refusal names the foreign SID: {refused}"
        );

        let u = user();
        set_dacl(&file, &format!("D:(A;;FA;;;{u})(A;;FA;;;SY)"), false);
        let refused = check(&file)
            .expect_err("inheritance enabled is refused")
            .to_string();
        assert!(refused.contains("SE_DACL_PROTECTED"), "{refused}");

        set_dacl(&file, &format!("D:(A;;FA;;;{u})(A;;FA;;;SY)"), true);
        check(&file).expect("back to the two ACEs, protected");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_created_without_the_descriptor_is_refused() {
        let dir = std::env::temp_dir().join(format!("pemu-win-acl-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("plain dir");
        let file = dir.join("plain.txt");
        std::fs::write(&file, b"x").expect("plain file");
        let refused = check(&file).expect_err("an inherited ACL is not owner-only");
        assert!(refused.to_string().contains("not owner-only"), "{refused}");
        assert!(
            create_dir(&dir).is_err(),
            "an existing plain directory is refused"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parents_are_created_owner_only_and_a_widened_directory_is_refused() {
        let root = scratch_dir("win-acl-parents");
        let leaf = root.join("boot-cache").join("entries");
        create_dir(&leaf).expect("owner-only tree");
        for dir in [&root, &root.join("boot-cache"), &leaf] {
            check(dir).expect("every created level is owner-only");
        }
        create_dir(&leaf).expect("an existing owner-only directory is not an error");
        widen(&leaf);
        create_dir(&leaf).expect_err("a widened directory is refused, not repaired");
        check(&leaf).expect_err("and the refusal changed nothing");
        let u = user();
        set_dacl(&leaf, &format!("D:(A;;FA;;;{u})(A;;FA;;;SY)"), true);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_refusal_keeps_the_content_and_an_acceptance_truncates() {
        let dir = scratch_dir("win-acl-truncate");
        create_dir(&dir).expect("dir");
        let file = dir.join("serve.json");
        create_file(&file, false)
            .expect("create")
            .write_all(b"the first, longer content")
            .expect("write");
        create_file(&file, false)
            .expect("reopen")
            .write_all(b"short")
            .expect("write");
        assert_eq!(std::fs::read(&file).expect("read"), b"short");

        widen(&file);
        create_file(&file, false).expect_err("a widened file is refused");
        assert_eq!(
            std::fs::read(&file).expect("read"),
            b"short",
            "nothing destroyed"
        );
        create_file(&file, true).expect_err("create_new refuses an existing file");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Creating the link needs `SeCreateSymbolicLinkPrivilege` or Developer Mode; the test fails
    /// rather than skips without it, because a skipped security test reports ok.
    #[test]
    fn a_symlink_at_the_target_is_refused() {
        let dir = scratch_dir("win-acl-symlink");
        create_dir(&dir).expect("dir");
        let elsewhere = dir.join("notes.txt");
        create_file(&elsewhere, true)
            .expect("target")
            .write_all(b"the user's own file")
            .expect("write");
        let planted = dir.join("serve.json");
        std::os::windows::fs::symlink_file(&elsewhere, &planted)
            .expect("symlink creation (needs Developer Mode or the symlink privilege)");
        let refused = create_file(&planted, false).expect_err("a link is not a file to create");
        assert!(refused.to_string().contains("link"), "{refused}");
        assert_eq!(
            std::fs::read(&elsewhere).expect("read"),
            b"the user's own file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A path that was a regular file when checked and is a link or junction when opened is
    /// refused: the link opens as itself and is not a regular file.
    #[test]
    fn open_regular_no_follow_refuses_a_symlink_and_a_junction() {
        use super::super::super::open_regular_no_follow;
        let dir = scratch_dir("win-no-follow");
        create_dir(&dir).expect("dir");
        let real = dir.join("scenario.yaml");
        create_file(&real, true)
            .expect("file")
            .write_all(b"steps: []\n")
            .expect("write");
        let expected = std::fs::symlink_metadata(&real).expect("the checked file");
        open_regular_no_follow(&real, &expected).expect("the regular file itself opens");

        let target = dir.join("elsewhere.yaml");
        create_file(&target, true)
            .expect("file")
            .write_all(b"steps: []\n")
            .expect("write");
        let swapped = dir.join("swapped.yaml");
        std::os::windows::fs::symlink_file(&target, &swapped)
            .expect("symlink creation (needs Developer Mode or the symlink privilege)");
        let refused = open_regular_no_follow(&swapped, &expected).expect_err("a symlink");
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{refused}");

        let junction = dir.join("junction.yaml");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&dir)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("cmd runs");
        assert!(status.success(), "mklink /J");
        open_regular_no_follow(&junction, &expected).expect_err("a junction is not the file");
        std::fs::remove_dir(&junction).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_new_directory_is_owner_only_and_exclusive() {
        use super::super::super::OwnerOnly as _;
        let root = scratch_dir("win-new-dir");
        create_dir(&root).expect("dir");
        let run = root.join("run-1");
        super::super::Windows
            .create_new_dir(&run)
            .expect("a fresh name");
        check(&run).expect("owner-only");
        let taken = super::super::Windows
            .create_new_dir(&run)
            .expect_err("taken");
        assert!(
            matches!(&taken, PlatformError::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists),
            "{taken}"
        );
        super::super::Windows
            .create_new_dir(&root.join("missing").join("run"))
            .expect_err("non-recursive");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_descriptor_sddl_is_protected_with_two_full_access_aces() {
        assert_eq!(
            owner_only_sddl("S-1-5-21-1-2-3-1001"),
            "O:S-1-5-21-1-2-3-1001D:P(A;;FA;;;S-1-5-21-1-2-3-1001)(A;;FA;;;SY)"
        );
    }
}
