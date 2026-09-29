//! The artifact seam for commands that write or read an artifact (`snapshot` export and import,
//! `screenshot`, `ble_gatt --op capture`, `net_capture --op save`).
//!
//! `pemu-api` opens no file, so a host installs an [`ArtifactIo`] once per process
//! (`pemu_host::hooks::install`). With none installed those commands refuse with `E_STATE`.

use std::sync::{Mutex, MutexGuard};

/// Paths are relative to the instance's artifact root; `write` returns the forward-slashed path it
/// wrote. The browser installs a pair over its own storage.
#[derive(Copy, Clone)]
pub struct ArtifactIo {
    pub write: fn(&str, &[u8]) -> Result<String, String>,
    pub read: fn(&str) -> Result<Vec<u8>, String>,
}

fn slot() -> MutexGuard<'static, Option<ArtifactIo>> {
    static IO: Mutex<Option<ArtifactIo>> = Mutex::new(None);
    match IO.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Installs the process-wide artifact access, replacing any earlier one.
pub fn set(io: ArtifactIo) {
    *slot() = Some(io);
}

/// Swaps the installed access and returns the previous one, so a test can restore it.
pub fn replace(io: Option<ArtifactIo>) -> Option<ArtifactIo> {
    std::mem::replace(&mut *slot(), io)
}

pub fn installed() -> Option<ArtifactIo> {
    *slot()
}
