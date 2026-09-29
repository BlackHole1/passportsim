//! The two Windows confirmation paths that need a person at this machine: the interactive-console
//! test and the native dialog.
//!
//! **Console.** `CONIN$` and `CONOUT$` are the buffers of the attached console whatever stdio was
//! redirected to (Microsoft Learn, "Console Handles"), and `GetConsoleMode` must succeed on both.
//! That is not enough on Windows: a console program started without one gets a new console, so
//! under an OpenSSH session with no pty both buffers answer, yet the console is invisible, in
//! session 0 on a non-interactive window station (measured: `echo x>CON` reached no client). So
//! the rule adds that the process is not in session 0 and its window station is `WinSta0`
//! ([`console_rule`]). A ConPTY hosted inside a desktop session passes it like a person's
//! terminal; an `ssh -t` session also runs in session 0 and reads "not interactive"
//! (fail-closed).
//!
//! **Dialog.** A dialog reaches a person only from `WinSta0` of the session the physical console
//! shows: session 0 cannot show UI, and a remote logon is another session than
//! `WTSGetActiveConsoleSessionId`. The box is `MessageBoxW`, system-modal and topmost, Yes/No
//! with No the default (`MB_DEFBUTTON2`) so a stray Enter declines.

use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::GetConsoleMode;
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetUserObjectInformationW, UOI_NAME,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_SYSTEMMODAL, MB_TOPMOST, MB_YESNO,
    MessageBoxW,
};

use super::super::{Confirmation, DialogAvailability, PlatformError};
use super::acl::wide_str;

/// `GetConsoleMode` succeeds on both `CONIN$` and `CONOUT$`. Both names pass
/// [`crate::paths::refuse_device`] only as the exact-match exemption; a refusal reads as "not
/// interactive".
pub(crate) fn is_interactive() -> bool {
    let buffers = crate::paths::allow_console_buffers()
        .iter()
        .all(|name| crate::paths::refuse_device(name).is_ok() && has_console_mode(name));
    console_rule(buffers, session(), window_station().as_deref())
}

/// The console rule over its facts. Pure, so every branch is tested on any host.
pub(crate) fn console_rule(buffers: bool, session: Option<u32>, station: Option<&str>) -> bool {
    buffers
        && session.is_some_and(|s| s != 0)
        && station.is_some_and(|name| name.eq_ignore_ascii_case("WinSta0"))
}

fn session() -> Option<u32> {
    let mut session = 0u32;
    // SAFETY: `session` is a valid out-pointer; the process id is this process's own.
    let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    (ok != 0).then_some(session)
}

fn has_console_mode(name: &Path) -> bool {
    let Ok(text) = super::acl::wide(name) else {
        return false;
    };
    // SAFETY: `text` is NUL-terminated and outlives the call; no security attributes and no
    // template are passed.
    let handle = unsafe {
        CreateFileW(
            text.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    // SAFETY: `handle` was just opened and is owned, and closed, by `owned` alone.
    let owned = unsafe {
        <std::os::windows::io::OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(
            handle,
        )
    };
    let mut mode = 0u32;
    // SAFETY: `owned` is an open handle for the call and `mode` a valid out-pointer.
    let ok = unsafe {
        GetConsoleMode(
            std::os::windows::io::AsRawHandle::as_raw_handle(&owned),
            &mut mode,
        )
    };
    ok != 0
}

/// `WTSGetActiveConsoleSessionId`'s answer when no session is attached to the physical console.
const NO_CONSOLE_SESSION: u32 = 0xFFFF_FFFF;

pub(crate) fn availability() -> DialogAvailability {
    let Some(session) = session() else {
        return DialogAvailability::Unavailable(
            "the session of this process cannot be read, so no dialog can be shown to a person",
        );
    };
    // SAFETY: takes nothing and cannot fail; it answers `NO_CONSOLE_SESSION` instead.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    decide(session, console, window_station().as_deref())
}

fn window_station() -> Option<String> {
    // SAFETY: returns a handle this process does not own and must not close, or null.
    let station = unsafe { GetProcessWindowStation() };
    if station.is_null() {
        return None;
    }
    let mut name = [0u16; 256];
    let mut needed = 0u32;
    // SAFETY: `name` holds `size_of_val(&name)` bytes and `needed` is a valid out-pointer.
    let ok = unsafe {
        GetUserObjectInformationW(
            station,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            std::mem::size_of_val(&name) as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}

/// The dialog rule: the session is not 0, equals the active console session, and the window
/// station is `WinSta0`. Pure, so every branch is tested without a desktop.
pub(crate) fn decide(session: u32, console: u32, station: Option<&str>) -> DialogAvailability {
    if session == 0 {
        return DialogAvailability::Unavailable(
            "this process runs in session 0, where services run and no window reaches a person",
        );
    }
    if console == NO_CONSOLE_SESSION {
        return DialogAvailability::Unavailable(
            "no session is attached to the physical console, so no dialog can reach a person",
        );
    }
    if session != console {
        return DialogAvailability::Unavailable(
            "this process is not in the active console session (an ssh or remote logon), so a \
             dialog would reach no one at this machine",
        );
    }
    match station {
        Some(name) if name.eq_ignore_ascii_case("WinSta0") => DialogAvailability::Available,
        _ => DialogAvailability::Unavailable(
            "this process's window station is not WinSta0, the interactive one, so a dialog would \
             be drawn where no one sees it",
        ),
    }
}

/// Runs `raise` only when a person can see the dialog, else refuses with the reason
/// [`availability`] gave, as `Unsupported`, so callers fall through to the next path.
pub(crate) fn gated(
    availability: DialogAvailability,
    raise: impl FnOnce() -> Result<Confirmation, PlatformError>,
) -> Result<Confirmation, PlatformError> {
    match availability {
        DialogAvailability::Available => raise(),
        DialogAvailability::Unavailable(why) => Err(PlatformError::Unsupported(why)),
    }
}

pub(crate) const DIALOG_STYLE: u32 =
    MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SYSTEMMODAL | MB_SETFOREGROUND | MB_TOPMOST;

/// Only the Yes button is a confirmation; No, a closed window and a failure to show (0) decline.
pub(crate) fn answer(result: i32) -> Confirmation {
    match result == IDYES {
        true => Confirmation::Confirmed,
        false => Confirmation::Declined,
    }
}

/// Raises the dialog and blocks until the person answers. An interior NUL in `title` or `body`
/// is refused instead of shown truncated.
///
/// `MessageBoxW` has no timeout, unlike the macOS alert. UNVERIFIED whether a desktop that never
/// draws the box exists once [`availability`] has passed.
pub(crate) fn raise(title: &str, body: &str) -> Result<Confirmation, PlatformError> {
    let invalid = |e| PlatformError::io(std::path::PathBuf::new(), e);
    let title = wide_str(title).map_err(invalid)?;
    let body = wide_str(body).map_err(invalid)?;
    // SAFETY: both strings are NUL-terminated and outlive the call; a null owner window is
    // allowed and makes the box ownerless.
    let result = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            DIALOG_STYLE,
        )
    };
    Ok(answer(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_console_session_on_winsta0_is_available() {
        assert_eq!(decide(1, 1, Some("WinSta0")), DialogAvailability::Available);
        assert_eq!(decide(1, 1, Some("winsta0")), DialogAvailability::Available);
        for (session, console, station) in [
            (0, 0, Some("WinSta0")),
            (1, NO_CONSOLE_SESSION, Some("WinSta0")),
            (2, 1, Some("WinSta0")),
            (1, 1, Some("Service-0x0-3e7$")),
            (1, 1, None),
        ] {
            assert!(
                matches!(
                    decide(session, console, station),
                    DialogAvailability::Unavailable(_)
                ),
                "{session} {console} {station:?}"
            );
        }
    }

    /// From a remote shell the reason says why there is no interactive desktop. Never raises a
    /// dialog. In a PowerShell window of the logged-on user it printed `Available`.
    #[test]
    fn availability_follows_the_rule_and_is_unavailable_over_ssh() {
        let got = availability();
        if std::env::var_os("SSH_CONNECTION").is_some() {
            let DialogAvailability::Unavailable(why) = got else {
                panic!("an ssh session has no interactive desktop");
            };
            assert!(why.contains("session"), "{why}");
        }
        println!("dialog availability here: {got:?}");
    }

    #[test]
    fn the_gate_refuses_without_raising_anything() {
        let refused = gated(DialogAvailability::Unavailable("no desktop"), || {
            panic!("the dialog must not be raised without a desktop")
        })
        .expect_err("refused");
        assert!(refused.is_unsupported() && refused.to_string().contains("no desktop"));
        assert_eq!(
            gated(DialogAvailability::Available, || Ok(Confirmation::Declined)).expect("raised"),
            Confirmation::Declined
        );
    }

    #[test]
    fn only_yes_is_a_confirmation() {
        assert_eq!(answer(IDYES), Confirmation::Confirmed);
        for other in [0, 1, 2, 7] {
            assert_eq!(answer(other), Confirmation::Declined, "{other}");
        }
        assert_eq!(
            DIALOG_STYLE & MB_DEFBUTTON2,
            MB_DEFBUTTON2,
            "No is the default"
        );
    }

    #[test]
    fn only_a_console_in_an_interactive_session_counts() {
        assert!(console_rule(true, Some(1), Some("WinSta0")));
        assert!(
            console_rule(true, Some(3), Some("winsta0")),
            "a remote desktop session"
        );
        for (buffers, session, station) in [
            (false, Some(1), Some("WinSta0")),
            (true, Some(0), Some("WinSta0")),
            (true, None, Some("WinSta0")),
            (true, Some(1), Some("Service-0x0-3e7$")),
            (true, Some(1), None),
        ] {
            assert!(
                !console_rule(buffers, session, station),
                "{buffers} {session:?} {station:?}"
            );
        }
    }

    /// From a remote shell the process has a console answering `GetConsoleMode` on both buffers
    /// and is still not interactive. In a PowerShell window of the logged-on user it printed
    /// `interactive true, input true, output true, session Some(1), station Some("WinSta0")`.
    #[test]
    fn console_detection_is_false_over_ssh() {
        let interactive = is_interactive();
        let [input, output] = crate::paths::allow_console_buffers();
        println!(
            "console: interactive {interactive}, input {}, output {}, session {:?}, station {:?}",
            has_console_mode(input),
            has_console_mode(output),
            session(),
            window_station()
        );
        if std::env::var_os("SSH_CONNECTION").is_some() {
            assert!(
                !interactive,
                "an ssh session is not an interactive console here"
            );
        }
        for name in [input, output] {
            assert!(
                crate::paths::refuse_device(name).is_ok(),
                "{}",
                name.display()
            );
        }
    }
}
