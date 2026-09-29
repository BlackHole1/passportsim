//! The BLE radio module.
//!
//! [`BleModule`] binds `specs/hle/idf-5.5.3/ble.toml` fail-closed: each of the 7 VHCI functions
//! the image links must sit in its pinned section and match an accepted (size, skeleton hash)
//! variant, the app's `idf_ver` must match the profile, and every nested-call target and data
//! symbol the handlers use must be linked. Any failure refuses the whole module, and the machine
//! arms the `DisabledFeature` tripwire at `esp_bt_controller_init`. An image that links none of
//! the 7 functions binds nothing. Without an ELF, the module binds only when every name
//! [`RadioModule::image_symbols`] lists is found in the image.

use pemu_core::snap::SectionId;
use pemu_hle::binding::{BindingMismatch, ImageView, MachineConfigFragment, ModuleSymbols};
use pemu_hle::core::ModuleHost;
use pemu_hle::hooks::{HookSet, ModuleIndex};
use pemu_loader::elf::ElfInfo;

use super::profile::BleProfile;
use super::vhci::BleHost;
use crate::RadioModule;

/// The module index BLE's hooks carry: modules are ordered BLE, then Wi-Fi.
pub const BLE_MODULE: ModuleIndex = ModuleIndex::FIRST_MODULE;

/// The binding profile id; the receipt reports a bound BLE as `idf-5.5.3/ble`.
pub const PROFILE_ID: &str = "idf-5.5.3";

#[derive(Copy, Clone, Debug, Default)]
pub struct BleModule;

static BLE: BleModule = BleModule;

pub fn module() -> Option<&'static dyn RadioModule> {
    Some(&BLE)
}

impl BleModule {
    pub fn bind_view(&self, image: &ImageView<'_>) -> Result<HookSet, Vec<BindingMismatch>> {
        let profile = BleProfile::load();
        crate::hle_common::bind_view(
            image,
            &profile.idf,
            &profile.hooks,
            &profile.calls,
            &profile.data,
            BLE_MODULE,
        )
    }
}

impl RadioModule for BleModule {
    fn name(&self) -> &'static str {
        "ble"
    }

    /// Binds from the symbols alone, which can never check the code hashes, so an image that
    /// links the controller is refused here; [`RadioModule::bind_image`] is the real binding.
    fn bind(&self, elf: &ElfInfo) -> Result<HookSet, Vec<BindingMismatch>> {
        self.bind_view(&ImageView::symbols_only(elf))
    }

    fn snapshot_sections(&self) -> &'static [SectionId] {
        // Empty on purpose: everything the module owns is written by `BleState::snap_write` into
        // the already hashed `hle.machine` section; a section of its own would duplicate those
        // bytes.
        &[]
    }

    fn config_fragment(&self) -> MachineConfigFragment {
        MachineConfigFragment::default()
    }

    fn bind_image(&self, image: &ImageView<'_>) -> Result<HookSet, Vec<BindingMismatch>> {
        self.bind_view(image)
    }

    fn profile_id(&self) -> Option<&'static str> {
        Some(PROFILE_ID)
    }

    fn host(&self, image: &ImageView<'_>) -> Option<Box<dyn ModuleHost>> {
        BleHost::new(BLE_MODULE, image).map(|host| Box::new(host) as Box<dyn ModuleHost>)
    }

    /// The profile's hooks, calls and data, and the NVS calibration marker: the enable handler's
    /// busy time depends on whether it is linked, and an image can show that it is, never that it
    /// is not, so an image-only bind needs it found.
    fn image_symbols(&self) -> ModuleSymbols {
        let profile = BleProfile::load();
        crate::hle_common::module_symbols(
            &profile.hooks,
            &profile.calls,
            &profile.data,
            &[super::vhci::NVS_CALIBRATION_SYMBOL],
        )
    }

    /// True when the btsnoop capture keeps key material. It adds no `SecretSet` member: a session
    /// key is this run's own, so the taint is its whole contribution.
    fn state_taints(&self, state: &[u8]) -> bool {
        crate::ble::vhci::capture_keeps_secrets(state)
    }
}
