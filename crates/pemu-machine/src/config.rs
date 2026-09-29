//! Machine configuration and the assets a machine is built from.

use std::fmt;
use std::sync::{Arc, OnceLock};

use pemu_board::ladder::LadderConfig;
use pemu_board::passport::BoardConfig;
use pemu_board::power::PowerConfig;
use pemu_board::usb_plug::UsbConfig;
use pemu_core::clock::TimingProfile;
use pemu_core::trace::{DEFAULT_RECENT_RECORDS, TraceKinds, TraceSink};
use pemu_hle::binding::MachineConfigFragment;
use pemu_hle::image_symbols::Recovered;
use pemu_hle::worker::WakeMode;
use pemu_loader::app_desc::AppDesc;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::elf::ElfInfo;
#[cfg(feature = "bundled-rom")]
use pemu_loader::rom::PinError;
use pemu_loader::rom::RomImage;
use pemu_rv32::engine::EngineCfg;
use pemu_soc_c3::SocError;

use crate::hang::HangCfg;

pub struct MachineConfig {
    pub board: BoardConfig,
    pub profile: TimingProfileId,
    /// Explicit profile values replacing the `specs/timing-profiles.toml` column `profile` names.
    /// `None` except in a calibration; the resolved values are run identity either way.
    pub timing: Option<TimingProfile>,
    pub seed: u64,
    pub efuse: EfuseSource,
    pub hle: HleConfig,
    pub engine: EngineCfg,
    pub poll_ff: bool,
    pub trace: TraceCfg,
    pub console_capture: CaptureMode,
    pub hang: HangCfg,
    /// Blocks whose model is replaced by a register store (`--disable-model`).
    pub disabled_models: Vec<String>,
}

impl Clone for MachineConfig {
    /// Field by field: `EngineCfg` is not `Clone`, because its fuser is a `&'static` reference the
    /// machine sets.
    fn clone(&self) -> MachineConfig {
        MachineConfig {
            board: self.board.clone(),
            profile: self.profile,
            timing: self.timing.clone(),
            seed: self.seed,
            efuse: self.efuse,
            hle: self.hle.clone(),
            engine: EngineCfg {
                max_block_insns: self.engine.max_block_insns,
                strict_csr: self.engine.strict_csr,
                fuser: self.engine.fuser,
            },
            poll_ff: self.poll_ff,
            trace: self.trace.clone(),
            console_capture: self.console_capture,
            hang: self.hang,
            disabled_models: self.disabled_models.clone(),
        }
    }
}

impl MachineConfig {
    /// [`MachineConfig::timing`] when set, otherwise the `specs/timing-profiles.toml` column that
    /// [`MachineConfig::profile`] names.
    pub fn timing(&self) -> TimingProfile {
        match &self.timing {
            Some(explicit) => explicit.clone(),
            None => self.profile.table_column().clone(),
        }
    }

    /// The run identity hash the snapshot header carries as `config_hash`: SHA-256 over
    /// [`MachineConfig::identity_bytes`], the parsed fields, never a file's bytes.
    pub fn identity_hash(&self) -> [u8; 32] {
        pemu_loader::sha256(&self.identity_bytes())
    }

    /// The canonical encoding behind [`MachineConfig::identity_hash`]: a version tag, then every
    /// field that can change what the guest observes, little-endian in declaration order (sets are
    /// sorted, since their order changes nothing). `engine.max_block_insns`, `engine.fuser`,
    /// `poll_ff` and `trace` are left out because they cannot change a result. Every struct is
    /// destructured without `..`, so a new field does not compile until it is classified here.
    pub fn identity_bytes(&self) -> Vec<u8> {
        let MachineConfig {
            board,
            profile,
            timing: _,
            seed,
            efuse,
            hle,
            engine,
            poll_ff: _,
            trace: _,
            console_capture,
            hang,
            disabled_models,
        } = self;
        let EngineCfg {
            max_block_insns: _,
            strict_csr,
            fuser: _,
        } = engine;
        let BoardConfig {
            xtal_hz,
            slow_clk_hz,
            flash_size_mb,
            flash_jedec,
            strap,
            display_width,
            display_height,
            display_cs_gpio,
            display_dc_gpio,
            display_hz,
            invon_shows_ram,
            backlight_gpio,
            backlight_channel,
            backlight_active_high,
            i2c_sda_gpio,
            i2c_scl_gpio,
            codec_addr,
            gauge_addr,
            i2s_gpio,
            buttons,
            power,
            battery_capacity_mah,
            battery_charge_ma,
            battery_term_ma,
            battery_cv_mv,
            gauge_version_reg,
            usb,
        } = board;
        let LadderConfig {
            adc_unit,
            adc_channel,
            gpio,
            up_mv,
            down_mv,
            ok_mv,
            released_mv,
            raw_code,
            digital_vil_mv,
            pullup_held_in_deep_sleep,
        } = buttons;
        let PowerConfig {
            on_hold_ms,
            off_hold_ms,
            usb_powers_mcu,
            cutoff_mv,
        } = power;
        let UsbConfig {
            enumerate_ms,
            download_strap,
        } = usb;
        let HleConfig {
            radios,
            disabled: disabled_radios,
            wake,
        } = hle;

        let mut out = b"pemu.machine-config.identity.v4".to_vec();
        let mut put = |bytes: &[u8]| out.extend_from_slice(bytes);
        for v in [*xtal_hz, *slow_clk_hz, *flash_size_mb] {
            put(&v.to_le_bytes());
        }
        put(flash_jedec);
        put(&[*strap]);
        put(&display_width.to_le_bytes());
        put(&display_height.to_le_bytes());
        put(&[*display_cs_gpio, *display_dc_gpio]);
        put(&display_hz.to_le_bytes());
        put(&[
            u8::from(*invon_shows_ram),
            *backlight_gpio,
            *backlight_channel,
            u8::from(*backlight_active_high),
            *i2c_sda_gpio,
            *i2c_scl_gpio,
            *codec_addr,
            *gauge_addr,
        ]);
        put(i2s_gpio);
        put(&[*adc_unit, *adc_channel, *gpio]);
        for v in [*up_mv, *down_mv, *ok_mv, *released_mv] {
            put(&v.to_le_bytes());
        }
        for v in raw_code {
            put(&v.to_le_bytes());
        }
        put(&digital_vil_mv.to_le_bytes());
        put(&[u8::from(*pullup_held_in_deep_sleep)]);
        put(&on_hold_ms.to_le_bytes());
        put(&off_hold_ms.to_le_bytes());
        put(&[u8::from(*usb_powers_mcu)]);
        put(&cutoff_mv.to_le_bytes());
        for v in [
            *battery_capacity_mah,
            *battery_charge_ma,
            *battery_term_ma,
            *battery_cv_mv,
        ] {
            put(&v.to_le_bytes());
        }
        put(&[*gauge_version_reg]);
        put(&enumerate_ms.to_le_bytes());
        put(&[*download_strap]);
        put(&[match profile {
            TimingProfileId::Fast => 0,
            TimingProfileId::Device => 1,
        }]);
        // The resolved profile values, so two tables or a calibration's explicit values are
        // two identities.
        let timing = self.timing().identity_bytes();
        put(&(timing.len() as u32).to_le_bytes());
        put(&timing);
        put(&seed.to_le_bytes());
        put(&[match efuse {
            EfuseSource::Synth => 0,
            EfuseSource::Dump => 1,
        }]);
        // `MachineConfigFragment` has no fields yet; the count still separates one registered
        // module from none.
        put(&(radios.len() as u32).to_le_bytes());
        put(&[
            match console_capture {
                CaptureMode::Wire => 0,
                CaptureMode::Fifo => 1,
            },
            u8::from(*strict_csr),
        ]);
        let HangCfg { enabled, stuck_ms } = hang;
        put(&[u8::from(*enabled)]);
        put(&stuck_ms.to_le_bytes());
        let models: std::collections::BTreeSet<&str> =
            disabled_models.iter().map(String::as_str).collect();
        put(&(models.len() as u32).to_le_bytes());
        for name in models {
            put(&(name.len() as u32).to_le_bytes());
            put(name.as_bytes());
        }
        let radios_off: std::collections::BTreeSet<&str> =
            disabled_radios.iter().map(String::as_str).collect();
        put(&(radios_off.len() as u32).to_le_bytes());
        for name in radios_off {
            put(&(name.len() as u32).to_le_bytes());
            put(name.as_bytes());
        }
        put(&[match wake {
            WakeMode::U5Polling => 0,
            WakeMode::U4MagicIsr => 1,
        }]);
        out
    }
}

/// Everything a machine is built from besides its configuration. The hashes of
/// [`Assets::identity`] are computed on first use and kept; a machine holds its assets behind an
/// `Arc` and never changes them.
pub struct Assets {
    pub rom: RomImage,
    pub flash: FlashImage,
    pub app_elf: Option<Arc<ElfInfo>>,
    pub boot_elf: Option<Arc<ElfInfo>>,
    pub efuse: EfuseImage,
    ids: OnceLock<AssetIdentity>,
    /// Computed once: every `Machine::new`, fork and restore binds, and the scan reads the whole
    /// image.
    recovered: OnceLock<Option<Recovered>>,
}

/// The asset half of the run identity that a snapshot header carries.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct AssetIdentity {
    pub rom_sha256: [u8; 32],
    pub image_sha256: [u8; 32],
    pub efuse_sha256: [u8; 32],
    /// SHA-256 of the app ELF: HLE binding, observe hooks and tripwires derive from its symbols, so
    /// the same flash image with another ELF is another run.
    pub app_elf_sha256: Option<[u8; 32]>,
}

impl Assets {
    pub fn new(
        rom: RomImage,
        flash: FlashImage,
        app_elf: Option<Arc<ElfInfo>>,
        boot_elf: Option<Arc<ElfInfo>>,
        efuse: EfuseImage,
    ) -> Assets {
        Assets {
            rom,
            flash,
            app_elf,
            boot_elf,
            efuse,
            ids: OnceLock::new(),
            recovered: OnceLock::new(),
        }
    }

    pub fn recovered_symbols(&self) -> Option<&Recovered> {
        if self.app_elf.is_some() {
            return None;
        }
        self.recovered
            .get_or_init(|| {
                let flash = self.flash.bytes();
                let merged = pemu_loader::esp_image::MergedImage::parse(flash).ok()?;
                let (_, app) = merged.app?;
                let segments = crate::hle::app_segments(flash, &app);
                Some(pemu_hle::image_symbols::recover(
                    pemu_hle::image_symbols::ImageRules::load(),
                    &segments,
                    app.app_desc(flash).ok().flatten(),
                    app.header.entry_addr,
                    Some(self.rom.symbols()),
                ))
            })
            .as_ref()
    }

    /// The `esp_app_desc_t` of the app the flash boots, else the app ELF's: the build identity the
    /// firmware prints at start (`ELF file SHA256`).
    pub fn app_desc(&self) -> Option<AppDesc> {
        let flash = self.flash.bytes();
        pemu_loader::esp_image::MergedImage::parse(flash)
            .ok()
            .and_then(|merged| merged.app)
            .and_then(|(_, app)| app.app_desc(flash).ok().flatten())
            .or_else(|| self.app_elf.as_ref().and_then(|elf| elf.app_desc.clone()))
    }

    pub fn binding_elf(&self) -> Option<&ElfInfo> {
        self.app_elf
            .as_deref()
            .or_else(|| self.recovered_symbols().map(|r| &r.elf))
    }

    pub fn identity(&self) -> &AssetIdentity {
        self.ids.get_or_init(|| {
            let words: Vec<u8> = self
                .efuse
                .dump_words()
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect();
            AssetIdentity {
                rom_sha256: pemu_loader::sha256(self.rom.bytes()),
                image_sha256: pemu_loader::sha256(self.flash.bytes()),
                efuse_sha256: pemu_loader::sha256(&words),
                app_elf_sha256: self.app_elf.as_ref().map(|elf| elf.sha256),
            }
        })
    }

    /// Assets over the bundled ROM that `efuse`'s chip revision selects, checked against its pin.
    #[cfg(feature = "bundled-rom")]
    pub fn with_bundled_rom(
        flash: FlashImage,
        app_elf: Option<Arc<ElfInfo>>,
        boot_elf: Option<Arc<ElfInfo>>,
        efuse: EfuseImage,
    ) -> Result<Assets, PinError> {
        use pemu_loader::rom::{RomRev, bundled, check_pin};

        let rom = check_pin(bundled(RomRev::for_efuse(&efuse)?))?;
        Ok(Assets::new(rom, flash, app_elf, boot_elf, efuse))
    }
}

const DEFAULT_MAX_BLOCK_INSNS: u16 = 64;

impl Default for MachineConfig {
    /// The bring-up configuration: the board of `boards/ai-passport.toml`, the `fast` profile, seed
    /// 0, a synthesized eFuse, poll fast-forward on (it changes speed only), no trace and wire
    /// console capture. UNVERIFIED as a product default: each value keeps a run dependent on as
    /// little emulator machinery as possible.
    fn default() -> MachineConfig {
        MachineConfig {
            board: BoardConfig::default(),
            profile: TimingProfileId::Fast,
            timing: None,
            seed: 0,
            efuse: EfuseSource::Synth,
            hle: HleConfig::default(),
            engine: EngineCfg {
                max_block_insns: DEFAULT_MAX_BLOCK_INSNS,
                strict_csr: false,
                fuser: None,
            },
            poll_ff: true,
            trace: TraceCfg::default(),
            console_capture: CaptureMode::Wire,
            hang: HangCfg::default(),
            disabled_models: Vec::new(),
        }
    }
}

/// Timing profile selected by `MachineConfig::profile`, a column of `specs/timing-profiles.toml`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TimingProfileId {
    Fast,
    Device,
}

impl TimingProfileId {
    pub const ALL: &'static [TimingProfileId] = &[TimingProfileId::Fast, TimingProfileId::Device];

    /// The one spelling of this profile: the wasm config `profile` value, the `start` argument and
    /// the word the receipt uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            TimingProfileId::Fast => "fast",
            TimingProfileId::Device => "device",
        }
    }

    /// `None` for a word outside the vocabulary, which callers refuse rather than default.
    pub fn parse(text: &str) -> Option<TimingProfileId> {
        TimingProfileId::ALL
            .iter()
            .copied()
            .find(|id| id.as_str() == text)
    }

    pub fn table_column(self) -> &'static TimingProfile {
        match self {
            TimingProfileId::Fast => TimingProfile::fast(),
            TimingProfileId::Device => TimingProfile::device(),
        }
    }

    pub fn vocabulary() -> String {
        let names: Vec<&str> = TimingProfileId::ALL.iter().map(|id| id.as_str()).collect();
        names.join(", ")
    }
}

/// Where the eFuse image comes from. Defined here rather than in `pemu-loader`, which does not
/// use serde.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EfuseSource {
    Synth,
    Dump,
}

/// HLE part of the machine configuration: one fragment per registered `pemu_hle::RadioModule`, in
/// registration order. `disabled` and `wake` change what the guest runs, so both are run identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HleConfig {
    pub radios: Vec<MachineConfigFragment>,
    /// Registered radio modules left unbound, by `RadioModule::name`: no hooks are bound and the
    /// `DisabledFeature` tripwire is armed at their driver init. An unknown name disables nothing.
    pub disabled: Vec<String>,
    /// How workers of bound modules are woken: the U4 magic ISR by default (it reproduces the
    /// device's HCI reply timing), or U5 polling.
    pub wake: WakeMode,
}

/// Whether the canonical MMIO and IRQ trace is recorded, which kinds, and how many recent records
/// stay replayable. The default records nothing, so a machine pays one branch per traced access.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceCfg {
    pub kinds: Option<TraceKinds>,
    /// Records kept for replay; 0 selects `pemu_core::trace::DEFAULT_RECENT_RECORDS`.
    pub recent: usize,
}

impl TraceCfg {
    pub fn all() -> TraceCfg {
        TraceCfg {
            kinds: Some(TraceKinds::ALL),
            recent: 0,
        }
    }

    pub fn sink(&self) -> TraceSink {
        match self.kinds {
            None => TraceSink::default(),
            Some(kinds) => {
                let recent = if self.recent == 0 {
                    DEFAULT_RECENT_RECORDS
                } else {
                    self.recent
                };
                TraceSink::new(kinds, recent)
            }
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CaptureMode {
    Wire,
    Fifo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The SoC refused an asset: a ROM longer than the ROM window, or a flash image longer than the
    /// 8 MB part.
    Soc(SocError),
    /// [`MachineConfig::efuse`] and [`Assets::efuse`] disagree. The image is the authority: it
    /// carries device bytes and answers `EfuseImage::tainted`, so a mismatch would misreport taint.
    EfuseMismatch {
        config: EfuseSource,
        tainted: bool,
    },
    UnknownModel(String),
    /// The resolved timing profile has a value the guest cannot run under.
    Timing(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Soc(e) => write!(f, "{e}"),
            ConfigError::EfuseMismatch { config, tainted } => write!(
                f,
                "the configuration asks for a {config:?} eFuse and the image {} a device dump",
                if *tainted { "is" } else { "is not" }
            ),
            ConfigError::Timing(why) => write!(f, "timing profile: {why}"),
            ConfigError::UnknownModel(name) => {
                write!(
                    f,
                    "no block `{name}` to disable: it is not a c3_devices! row"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<SocError> for ConfigError {
    fn from(e: SocError) -> ConfigError {
        ConfigError::Soc(e)
    }
}

#[cfg(all(test, feature = "bundled-rom"))]
mod tests {
    use super::*;

    /// Pinned: a change to the encoding or to a default moves every snapshot's `config_hash`, so
    /// every earlier snapshot is refused, and must be deliberate. A profile row added to the table
    /// moves it even at 0 in the `fast` column.
    #[test]
    fn the_default_configuration_identity_hash_is_pinned() {
        let hex: String = MachineConfig::default()
            .identity_hash()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "8c6709487f811a5d9fef9a021b2d62f31ee39c7b0176faa6d1389ee27feacb1d"
        );
    }

    #[test]
    fn identity_covers_the_fields_that_change_the_guest_and_only_those() {
        let base = MachineConfig::default().identity_hash();
        let changed = |f: fn(&mut MachineConfig)| {
            let mut cfg = MachineConfig::default();
            f(&mut cfg);
            cfg.identity_hash()
        };
        assert_ne!(changed(|c| c.engine.strict_csr = true), base);
        assert_ne!(changed(|c| c.seed = 1), base);
        assert_ne!(changed(|c| c.board.power.on_hold_ms += 1), base);
        assert_ne!(changed(|c| c.board.buttons.raw_code[3] ^= 1), base);
        assert_ne!(changed(|c| c.console_capture = CaptureMode::Fifo), base);
        assert_ne!(changed(|c| c.profile = TimingProfileId::Device), base);
        assert_eq!(changed(|c| c.engine.max_block_insns = 1), base);
        assert_eq!(changed(|c| c.poll_ff = !c.poll_ff), base);
        // The hang detector and the disabled models are identity; model order and repeats are not.
        assert_ne!(changed(|c| c.hang.enabled = false), base);
        assert_ne!(changed(|c| c.hang.stuck_ms = 1), base);
        assert_ne!(changed(|c| c.hle.wake = WakeMode::U5Polling), base);
        let ble = changed(|c| c.hle.disabled = vec!["ble".into()]);
        assert_ne!(ble, base);
        assert_eq!(
            changed(|c| c.hle.disabled = vec!["ble".into(), "ble".into()]),
            ble
        );
        let spi2 = changed(|c| c.disabled_models = vec!["spi2".into()]);
        assert_ne!(spi2, base);
        assert_ne!(changed(|c| c.disabled_models = vec!["spi2x".into()]), spi2);
        assert_eq!(
            changed(|c| c.disabled_models = vec!["sha".into(), "spi2".into()]),
            changed(|c| c.disabled_models = vec!["spi2".into(), "sha".into(), "spi2".into()])
        );
    }

    #[test]
    fn the_asset_identity_is_computed_once_and_matches_the_bytes() {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .unwrap();
        let first: *const AssetIdentity = assets.identity();
        assert!(
            std::ptr::eq(first, assets.identity()),
            "the same cached value"
        );
        assert_eq!(
            assets.identity().rom_sha256,
            pemu_loader::sha256(assets.rom.bytes())
        );
        assert_eq!(
            assets.identity().image_sha256,
            pemu_loader::sha256(assets.flash.bytes())
        );
    }

    #[test]
    fn with_bundled_rom_selects_a_pinned_rom_for_a_synthesized_efuse() {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .unwrap();
        assert!(assets.app_elf.is_none());
        assert!(assets.boot_elf.is_none());
    }
}
