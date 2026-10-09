//! The Wi-Fi radio module: binds `specs/hle/idf-5.5.3/wifi.toml` fail-closed.
//!
//! Every hooked function the image links must sit in its pinned section and match an accepted
//! (size, first-32-code-byte hash) variant, `idf_ver` must be the profile's IDF version, and every
//! nested-call target and data symbol must be linked. One failed check refuses the whole module,
//! and the machine then arms the `DisabledFeature` tripwire at `esp_wifi_init`: an image that
//! starts Wi-Fi without the HLE loops on `assert failed: esp_phy_enable phy_init.c:327`. An image
//! that links none of the functions binds nothing and needs nothing. Without an ELF, the module
//! binds only when every name [`RadioModule::image_symbols`] lists is found in the image, or is a
//! hook whose guard is (`wifi.toml`, "Guards").

use pemu_core::snap::SectionId;
use pemu_hle::binding::{BindingMismatch, ImageView, MachineConfigFragment, ModuleSymbols};
use pemu_hle::core::ModuleHost;
use pemu_hle::hooks::{HookSet, ModuleIndex};
use pemu_loader::elf::ElfInfo;

use super::driver::WifiHost;
use super::profile::WifiProfile;
use crate::RadioModule;

/// Module index of the Wi-Fi hooks: second in the fixed BLE, then Wi-Fi order.
pub const WIFI_MODULE: ModuleIndex = ModuleIndex(2);

pub const PROFILE_ID: &str = "idf-5.5.3";

#[derive(Copy, Clone, Debug, Default)]
pub struct WifiModule;

static WIFI: WifiModule = WifiModule;

pub fn module() -> Option<&'static dyn RadioModule> {
    Some(&WIFI)
}

impl WifiModule {
    pub fn bind_view(&self, image: &ImageView<'_>) -> Result<HookSet, Vec<BindingMismatch>> {
        let profile = WifiProfile::load();
        crate::hle_common::bind_view(
            image,
            &profile.idf,
            &profile.hooks,
            &profile.calls,
            &profile.data,
            WIFI_MODULE,
        )
    }
}

impl RadioModule for WifiModule {
    fn name(&self) -> &'static str {
        "wifi"
    }

    /// Binds from the symbols alone, which can never check the code hashes, so an image that
    /// links the driver is refused here; [`RadioModule::bind_image`] is the real binding.
    fn bind(&self, elf: &ElfInfo) -> Result<HookSet, Vec<BindingMismatch>> {
        self.bind_view(&ImageView::symbols_only(elf))
    }

    fn snapshot_sections(&self) -> &'static [SectionId] {
        // The module's state, the virtual LAN included, rides in the machine's `hle.machine`
        // section.
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
        WifiHost::new(WIFI_MODULE, image).map(|host| Box::new(host) as Box<dyn ModuleHost>)
    }

    fn image_symbols(&self) -> ModuleSymbols {
        let profile = WifiProfile::load();
        crate::hle_common::module_symbols(&profile.hooks, &profile.calls, &profile.data, &[])
    }

    /// The pre-shared keys of the scripted access points and of the last sweep's records, each
    /// once. State bytes this build cannot decode yield none.
    fn secret_values(&self, state: &[u8]) -> Vec<Vec<u8>> {
        let Ok(st) = crate::wifi::driver::WifiState::decode(state) else {
            return Vec::new();
        };
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for ap in st.aps.iter().chain(st.records.iter()) {
            if !ap.psk.is_empty() && !keys.contains(&ap.psk) {
                keys.push(ap.psk.clone());
            }
        }
        keys
    }
}
