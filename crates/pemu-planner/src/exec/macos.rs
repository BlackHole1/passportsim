//! The macOS half of [`super`]: POSIX modes for the runner's private files, and the call-out port
//! read with its modem-control reset pulse.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use crate::flow::SessionError;

pub(super) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// A directory, no group or other bit, owned by this process's user.
pub(super) fn check_private_dir(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("scratch directory: {e}"))?;
    if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
        return Err("the scratch directory is not an owner-only directory".to_owned());
    }
    // The current uid, without libc: the owner of a file this process just created.
    let probe = path.join(".owner-probe");
    let _ = fs::remove_file(&probe);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
        .map_err(|e| format!("scratch directory: {e}"))?;
    let uid = fs::metadata(&probe).map(|m| m.uid());
    let _ = fs::remove_file(&probe);
    if uid.map_err(|e| format!("scratch directory: {e}"))? != meta.uid() {
        return Err("the scratch directory belongs to another user".to_owned());
    }
    Ok(())
}

pub(super) fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Narrows a file esptool wrote with its own umask (0644) to 0600.
pub(super) fn narrow_private_file(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("scratch file: {e}"))
}

/// Opens the confirmed call-out port read-only for the boot console.
///
/// `O_NOCTTY` (0x20000) and `O_NONBLOCK` (0x4) of `sys/fcntl.h`: the port must not become this
/// process's controlling terminal, and the open must not wait for carrier.
pub(super) fn open_console(path: &str) -> Result<fs::File, SessionError> {
    const O_NOCTTY: i32 = 0x0002_0000;
    const O_NONBLOCK: i32 = 0x0000_0004;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOCTTY | O_NONBLOCK)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::ResourceBusy | std::io::ErrorKind::WouldBlock => SessionError::Busy,
            _ => SessionError::Failed(format!("the boot console did not open: {}", e.kind())),
        })
}

// Declared directly, so the modem-control pulse needs no dependency.
unsafe extern "C" {
    /// Variadic; 0, or -1 with `errno` set.
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

/// `TIOCMGET`, `_IOR('t', 106, int)` (`sys/ttycom.h`), derived per `sys/ioccom.h` as
/// `IOC_OUT | (4 << 16) | (b't' << 8) | 106`.
const TIOCMGET: u64 = 0x4004_746A;
/// `TIOCMSET`, `_IOW('t', 109, int)`: the same derivation with `IOC_IN`.
const TIOCMSET: u64 = 0x8004_746D;
const TIOCM_DTR: i32 = 0x0002;
const TIOCM_RTS: i32 = 0x0004;

/// The three states of [`super::RESET_STATES`], applied to the lines the port already has.
///
/// The first state must clear both lines: on open the CDC driver reports DTR and RTS already
/// asserted (`TIOCMGET` = 0x006), so asserting RTS first creates no edge and the device never
/// resets.
fn reset_line_sequence(current: i32) -> [i32; 3] {
    super::RESET_STATES.map(|state| {
        let mut lines = current & !(TIOCM_DTR | TIOCM_RTS);
        if state.dtr {
            lines |= TIOCM_DTR;
        }
        if state.rts {
            lines |= TIOCM_RTS;
        }
        lines
    })
}

/// Restarts the device on an already-open, read-only descriptor by pulsing its reset line.
///
/// Measured on the device: `TIOCMGET` and `TIOCMSET` work on an `O_RDONLY` descriptor, so no byte
/// is written; and the USB Serial/JTAG endpoint survives a chip reset, so the boot console arrives
/// on the same open fd with no re-enumeration.
pub(super) fn pulse_reset(file: &fs::File, hold: Duration) -> Result<(), SessionError> {
    use std::os::fd::AsRawFd as _;

    let fd = file.as_raw_fd();
    let mut lines: i32 = 0;
    // SAFETY: `fd` is open for the whole call, and `TIOCMGET` writes one `int` through the
    // pointer, which is valid and aligned for the duration.
    let rc = unsafe { ioctl(fd, TIOCMGET, &raw mut lines) };
    if rc != 0 {
        return Err(SessionError::Failed(format!(
            "the reset line could not be read: {}",
            std::io::Error::last_os_error().kind()
        )));
    }
    let sequence = reset_line_sequence(lines);
    let last = sequence.len() - 1;
    for (index, state) in sequence.into_iter().enumerate() {
        // SAFETY: `fd` is open for the whole call, and `TIOCMSET` reads one `int` through the
        // pointer, which is valid and aligned for the duration.
        let rc = unsafe { ioctl(fd, TIOCMSET, &raw const state) };
        if rc != 0 {
            return Err(SessionError::Failed(format!(
                "the reset line could not be driven: {}",
                std::io::Error::last_os_error().kind()
            )));
        }
        // The last state needs no hold: the read window starts right after it.
        if index < last {
            std::thread::sleep(hold);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reset_pulse_deasserts_before_it_asserts() {
        // The state the driver reports on open.
        let [quiet, pulled, released] = reset_line_sequence(TIOCM_DTR | TIOCM_RTS);

        assert_eq!(
            quiet & (TIOCM_DTR | TIOCM_RTS),
            0,
            "the pulse must deassert both lines first or it creates no edge at all"
        );
        assert_eq!(
            pulled & TIOCM_RTS,
            TIOCM_RTS,
            "the second state asserts RTS, which is the edge that pulls the reset line"
        );
        assert_eq!(
            pulled & TIOCM_DTR,
            0,
            "DTR stays deasserted through the pulse"
        );
        assert_eq!(released, quiet, "the pulse ends with both lines released");
        assert_ne!(
            quiet, pulled,
            "there is an edge between the first two states"
        );

        // Whatever the port's other bits are, they are left alone and only DTR and RTS move.
        let carrier = 0x0040;
        for state in reset_line_sequence(carrier | TIOCM_DTR | TIOCM_RTS) {
            assert_eq!(
                state & carrier,
                carrier,
                "unrelated line bits are preserved"
            );
        }
    }
}
