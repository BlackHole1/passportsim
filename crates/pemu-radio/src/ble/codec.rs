//! Snapshot helpers for the BLE module state, which lives in the `hle.machine` section.

use pemu_core::snap::SnapError;

/// The error of an enum tag this build does not define.
pub fn bad_tag() -> SnapError {
    SnapError::Malformed {
        at: "hle.machine",
        reason: "ble module state: unknown enum tag",
    }
}
