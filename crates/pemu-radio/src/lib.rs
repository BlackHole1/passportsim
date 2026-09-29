//! Radio: BLE controller, HCI, virtual air and central, btsnoop; Wi-Fi driver model and scan;
//! virtual LAN; heap ledger.

pub mod ble;
pub mod heap_ledger;
pub(crate) mod hle_common;
pub mod lan;
pub mod log_lines;
pub mod wifi;

/// Defined in `pemu-hle` and re-exported so `pemu-machine` names radio modules through this crate.
pub use pemu_hle::RadioModule;
pub use pemu_hle::binding::NoRadio;

/// The radio modules compiled into this build, in registration order (BLE, then Wi-Fi).
pub fn modules() -> Vec<&'static dyn RadioModule> {
    [ble::hle::module(), wifi::hle::module()]
        .into_iter()
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn ble_is_the_first_registered_module_and_wifi_the_second() {
        let names: Vec<&str> = super::modules().iter().map(|m| m.name()).collect();
        assert_eq!(names, ["ble", "wifi"]);
        // Each module allocates its `HookId` space from its index, so two modules bound against one
        // image never collide.
        assert_ne!(
            super::ble::hle::BLE_MODULE,
            super::wifi::hle::WIFI_MODULE,
            "the registered modules take distinct indices"
        );
    }
}
