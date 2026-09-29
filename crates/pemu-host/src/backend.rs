//! The real machine behind every daemon instance, and [`install`], which makes this process a
//! daemon host: `pemu-api` may not read files, so its default `BackendFactory` refuses until
//! [`install`] puts in [`machine_factory`], the [`FirmwareSource`], the host clock for
//! `wall_budget_ms`, the artifacts root and the audio seams.
//!
//! A `fw` argument becomes a merged 8 MB flash image on the bundled ROM with a synthesized eFuse
//! seeded by the run seed. A firmware path must be absolute (a daemon has no working directory of
//! the caller's) and is read through [`crate::host_file::read_regular`]; every read failure is one
//! error and a malformed image is a fixed text.
//!
//! A daemon never loads secret-bearing firmware: [`refuse_secrets`] rejects a cardid window that
//! is not all 0xFF or NVS holding credential keys, since tainted loading is CLI only.
//!
//! `start {power: off}` still builds a powered machine; the instance is only reported off, since
//! the facade has no rail.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use pemu_api::commands::start::{self, StartArgs};
use pemu_api::error::{
    ApiError, E_ASSET_HASH, E_ASSET_MISSING, E_INTERNAL, E_SECRET_REFUSED, E_USAGE,
};
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_machine::SnapshotMachine;
use pemu_machine::config::{Assets, MachineConfig, TimingProfileId};
use pemu_machine::machine::Machine;

use crate::assets::{Corpus, HostEnv};

/// Bytes of the 8 MB flash part, the cap of a firmware read.
pub const FLASH_BYTES: u64 = 8 * 1024 * 1024;

/// The cardid window, `[start, end)`.
pub const CARDID_WINDOW: (usize, usize) = (0x35_6000, 0x35_A000);

/// Turns a `start` `fw` argument into the flash image the machine boots. Errors reach the caller
/// as they are, so they must not name a host path.
pub type FirmwareSource = Arc<dyn Fn(&str) -> Result<FlashImage, ApiError> + Send + Sync>;

fn installed() -> &'static Mutex<Option<FirmwareSource>> {
    static SOURCE: OnceLock<Mutex<Option<FirmwareSource>>> = OnceLock::new();
    SOURCE.get_or_init(|| Mutex::new(None))
}

/// Builds the machine of a new instance (the `pemu-api` `BackendFactory`), on the instance's own
/// thread.
pub fn machine_factory(args: &StartArgs) -> Result<Box<dyn SnapshotMachine + Send>, ApiError> {
    let source = installed()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| {
            ApiError::new(E_INTERNAL, "this daemon has no firmware source installed")
                .with_hint("the host calls `pemu_host::backend::install` before serving")
        })?;
    let flash = source(&args.fw)?;
    // `--allow-tainted` is the only way past this refusal, and `start` has already required a
    // confirmation and a process that allows tainted loads. Taint is read from the bytes at attach,
    // so the flag on an erased image taints nothing.
    if !args.allow_tainted {
        refuse_secrets(flash.bytes())?;
    }
    record_built(&args.fw, flash.bytes());
    // HLE binds against the firmware's app ELF when the host knows one, so the panic, abort and
    // assert hooks and the per-image tripwires arm as on a machine a test composes with its ELF.
    let app_elf = crate::hooks::elf_of(&args.fw).map(|cx| Arc::clone(&cx.elf));
    Ok(Box::new(build_machine_with_efuse(
        flash,
        app_elf,
        args.seed,
        args.profile,
        efuse_of(args)?,
    )?))
}

/// The eFuse image this `start` asks for: synthesized, or the tainting `--efuse-dump <dir>`.
/// `pemu-api` has already refused unconfirmed or remote dumps; the path must be absolute and
/// passes [`crate::paths::refuse_device`].
fn efuse_of(args: &StartArgs) -> Result<EfuseImage, ApiError> {
    let Some(text) = args.efuse_dump.as_deref() else {
        return Ok(EfuseImage::synth(args.seed));
    };
    let path = Path::new(text);
    if !path.is_absolute() {
        return Err(ApiError::new(
            E_USAGE,
            "`efuse_dump` is an absolute path to the dump directory or file",
        ));
    }
    crate::paths::refuse_device(path).map_err(|refusal| {
        ApiError::new(
            E_USAGE,
            format!("`efuse_dump` names a device: {}", refusal.reason),
        )
    })?;
    crate::assets::load_efuse_dump(path).map_err(|e| {
        // The detail names the dump's shape, never a byte of it.
        ApiError::new(E_ASSET_MISSING, format!("the eFuse dump did not load: {e}"))
    })
}

fn nvs_ranges(flash: &[u8]) -> Vec<std::ops::Range<usize>> {
    let Ok(table) = pemu_loader::partitions::PartitionTable::from_flash(flash) else {
        return Vec::new();
    };
    table
        .entries
        .iter()
        .filter(|p| {
            p.ptype == pemu_loader::partitions::ptype::DATA
                && p.subtype == pemu_loader::partitions::subtype::NVS
        })
        .map(|part| {
            let start = (part.offset as usize).min(flash.len());
            start..start.saturating_add(part.size as usize).min(flash.len())
        })
        .collect()
}

/// SHA-256 over every NVS data partition of `flash`, in table order, each length-prefixed: the
/// NVS seed part of the boot-cache key.
pub fn nvs_seed_sha(flash: &[u8]) -> [u8; 32] {
    let mut buf = Vec::new();
    for range in nvs_ranges(flash) {
        let bytes = &flash[range];
        buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        buf.extend_from_slice(bytes);
    }
    pemu_loader::sha256(&buf)
}

/// SHA-256 of `flash` with every NVS data partition erased, so images differing only in NVS
/// differ only in [`nvs_seed_sha`].
pub fn image_sha_without_nvs(flash: &[u8]) -> [u8; 32] {
    let mut erased = flash.to_vec();
    for range in nvs_ranges(flash) {
        erased[range].fill(0xFF);
    }
    pemu_loader::sha256(&erased)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltImage {
    pub fw: String,
    pub image_sha: [u8; 32],
    pub nvs_seed_sha: [u8; 32],
}

thread_local! {
    /// The image the factory built last on this thread. `start` builds the machine and runs its
    /// boot-cache hook on one thread, so the hook reads its own session's image here.
    static BUILT: std::cell::RefCell<Option<BuiltImage>> = const { std::cell::RefCell::new(None) };
}

fn record_built(fw: &str, flash: &[u8]) {
    let built = BuiltImage {
        fw: fw.to_owned(),
        image_sha: image_sha_without_nvs(flash),
        nvs_seed_sha: nvs_seed_sha(flash),
    };
    BUILT.with(|slot| *slot.borrow_mut() = Some(built));
}

/// Takes the factory's measurement of the image it built last on this thread, if it was for
/// `fw`. The machine facade gives no view of flash, so this is the only place with the bytes.
pub fn take_built(fw: &str) -> Option<BuiltImage> {
    BUILT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|built| built.fw == fw) {
            slot.take()
        } else {
            None
        }
    })
}

/// A merged flash image, or `E_USAGE` with a fixed text (the loader's detail describes the bytes).
pub fn merged_image(bytes: &[u8]) -> Result<FlashImage, ApiError> {
    if bytes.starts_with(&pemu_loader::bundle::BUNDLE_MAGIC) {
        return bundle_image(bytes);
    }
    if bytes.len() as u64 > FLASH_BYTES {
        return Err(not_merged());
    }
    FlashImage::from_merged(bytes).map_err(|_| not_merged())
}

/// The flash image inside a `.pebundle`. `Bundle::parse` verifies every payload digest; what boots
/// is the `flash` role.
fn bundle_image(bytes: &[u8]) -> Result<FlashImage, ApiError> {
    let bundle = pemu_loader::bundle::Bundle::parse(bytes).map_err(|_| not_a_bundle())?;
    let flash = bundle
        .role_data(pemu_loader::bundle::BUNDLE_FLASH)
        .ok_or_else(not_a_bundle)?;
    if flash.len() as u64 > FLASH_BYTES {
        return Err(not_merged());
    }
    FlashImage::from_merged(flash).map_err(|_| not_merged())
}

fn not_a_bundle() -> ApiError {
    ApiError::new(
        E_USAGE,
        "this `.pebundle` does not carry a flash image this build can read",
    )
    .with_hint(
        "a bundle is self-checking: every payload digest is verified, and the `flash` role is the \
         merged image `start` boots",
    )
}

fn not_merged() -> ApiError {
    ApiError::new(E_USAGE, "the firmware is not a merged 8 MB flash image").with_hint(
        "`start` takes a merged image as `esptool merge_bin` writes it, bootloader at 0 and \
         partition table at 0x8000",
    )
}

/// `E_SECRET_REFUSED` when the cardid window holds a byte other than 0xFF, or an NVS data partition
/// holds a credential key. The synthetic-cardid mark lives in snapshot metadata, which an image
/// does not carry, so an image is judged on its bytes alone.
pub fn refuse_secrets(flash: &[u8]) -> Result<(), ApiError> {
    let (start, end) = CARDID_WINDOW;
    if let Some(window) = flash.get(start..end.min(flash.len()))
        && window.iter().any(|&b| b != 0xFF)
    {
        return Err(secret_refused("its cardid window is not erased"));
    }
    if let Ok(table) = pemu_loader::partitions::PartitionTable::from_flash(flash) {
        for part in table.entries.iter().filter(|p| {
            p.ptype == pemu_loader::partitions::ptype::DATA
                && p.subtype == pemu_loader::partitions::subtype::NVS
        }) {
            let start = part.offset as usize;
            let end = start.saturating_add(part.size as usize).min(flash.len());
            let Some(bytes) = flash.get(start..end) else {
                continue;
            };
            if pemu_introspect::nvs::list(bytes)
                .credentials()
                .next()
                .is_some()
            {
                return Err(secret_refused("an NVS partition holds credential keys"));
            }
        }
    }
    Ok(())
}

fn secret_refused(why: &str) -> ApiError {
    ApiError::new(
        E_SECRET_REFUSED,
        format!("this firmware is refused over MCP and HTTP: {why}"),
    )
    .with_hint(
        "a secret-bearing flash taints the machine and is loaded only by a human through the CLI \
         with confirmation; start from a sanitized image",
    )
}

/// A machine over the bundled ROM, a synthesized eFuse and `flash`, on `MachineConfig`'s default
/// timing profile.
pub fn build_machine(flash: FlashImage, seed: u64) -> Result<Machine, ApiError> {
    build_machine_with_elf(flash, None, seed, MachineConfig::default().profile)
}

/// [`build_machine`] with the app ELF HLE binds against, when there is one, and the timing
/// profile `start` selected.
pub fn build_machine_with_elf(
    flash: FlashImage,
    app_elf: Option<Arc<pemu_loader::elf::ElfInfo>>,
    seed: u64,
    profile: TimingProfileId,
) -> Result<Machine, ApiError> {
    build_machine_with_efuse(flash, app_elf, seed, profile, EfuseImage::synth(seed))
}

/// [`build_machine_with_elf`] over an eFuse image the caller chose (how `--efuse-dump` reaches a
/// machine). The configuration follows the image, and `EfuseImage::tainted` is what every receipt
/// answers from.
pub fn build_machine_with_efuse(
    flash: FlashImage,
    app_elf: Option<Arc<pemu_loader::elf::ElfInfo>>,
    seed: u64,
    profile: TimingProfileId,
    efuse: EfuseImage,
) -> Result<Machine, ApiError> {
    let source = if efuse.tainted() {
        pemu_machine::config::EfuseSource::Dump
    } else {
        pemu_machine::config::EfuseSource::Synth
    };
    let assets = Assets::with_bundled_rom(flash, app_elf, None, efuse).map_err(|e| {
        ApiError::new(
            E_ASSET_MISSING,
            format!("no pinned ROM for the synthesized chip revision: {e:?}"),
        )
    })?;
    let config = MachineConfig {
        seed,
        profile,
        efuse: source,
        ..MachineConfig::default()
    };
    Machine::new(config, assets)
        .map_err(|e| ApiError::new(E_INTERNAL, format!("the machine was not built: {e:?}")))
}

/// Makes this process a daemon host: the machine factory over `source`, the host clock, the
/// artifacts root `status` reports (already home-redacted) and the audio seams. A later call
/// replaces an earlier one.
pub fn install(
    source: FirmwareSource,
    artifacts_root_text: Option<String>,
    audio_root: crate::audio_root::AudioRoot,
) {
    *installed().lock().unwrap_or_else(|e| e.into_inner()) = Some(source);
    start::with_pool(|pool| {
        pool.set_factory(machine_factory);
        pool.set_host_clock(Some(host_ms));
        // Every receipt names the class of the thread that ran the machine.
        pool.set_thread_qos(Some(crate::platform::thread_qos_name));
        pool.set_artifacts_root(artifacts_root_text);
    });
    crate::audio_root::install(audio_root);
    pemu_api::commands::run::set_slice_observer(Some(crate::hub::observe_slice));
    crate::endpoints::install();
}

fn host_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    u64::try_from(
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// The product's firmware source: a corpus id (with its `PASSPORTSIM_CORPUS_<ID>` override), read
/// with the digest the map pins, or else a path to a merged image. Errors name the argument as
/// written, never an expanded path.
pub fn product_source(env: HostEnv) -> FirmwareSource {
    product_source_with_bundles(env, Arc::new(|_| None))
}

/// A `.pebundle` this binary carries for an id, if any (the packaged payload).
pub type PayloadBundles = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

/// [`product_source`] over a binary that may carry bundles, so `passportsim run` with no image
/// boots the bundled demo. The corpus wins over the payload, as in the web UI's firmware route, so
/// an owner's own image of an id beats the shipped copy. A path is tried last.
pub fn product_source_with_bundles(env: HostEnv, bundles: PayloadBundles) -> FirmwareSource {
    Arc::new(move |fw: &str| {
        if let Ok(corpus) = Corpus::load(&env)
            && let Some(entry) = corpus.get(fw)
        {
            let file = entry
                .file(pemu_loader::bundle::CORPUS_BIN)
                .ok_or_else(|| start::firmware_not_found(fw))?;
            let bytes = read_capped(&file.path).ok_or_else(|| start::firmware_not_found(fw))?;
            if let Some(want) = file.sha256
                && pemu_loader::sha256(&bytes) != want
            {
                return Err(ApiError::new(
                    E_ASSET_HASH,
                    format!("corpus `{fw}` does not match the digest corpus.toml pins"),
                ));
            }
            return merged_image(&bytes);
        }
        if let Some(bundle) = bundles(fw) {
            return merged_image(&bundle);
        }
        if fw == start::DEMO_FW && !Path::new(fw).is_absolute() {
            // A development build has no demo; tell the person what would give them one.
            return Err(ApiError::new(
                E_ASSET_MISSING,
                format!("no firmware `{fw}`: this build carries no bundled demo image"),
            )
            .with_hint(
                "`cargo xtask package` embeds the demo; or name an `idf.py` build directory, a \
                 merged image or a `.pebundle`",
            ));
        }
        merged_image(&read_path(fw)?)
    })
}

/// A firmware named by an absolute path: an `idf.py` build directory, or a file read under
/// [`crate::host_file::read_regular`]. A directory without `flasher_args.json` falls through to
/// the file read, whose refusal is about the path the person typed.
fn read_path(fw: &str) -> Result<Vec<u8>, ApiError> {
    let path = Path::new(fw);
    if !path.is_absolute() {
        return Err(ApiError::new(
            E_USAGE,
            "argument `fw`: a firmware path must be absolute, or a corpus id",
        )
        .with_hint("the daemon does not share the caller's working directory"));
    }
    if crate::build_dir::is_build_dir(path) {
        return crate::build_dir::merged_image_bytes(path, fw, FLASH_BYTES);
    }
    read_capped(path).ok_or_else(|| start::firmware_not_found(fw))
}

fn read_capped(path: &Path) -> Option<Vec<u8>> {
    let canonical = crate::host_file::canonical(path).ok()?;
    crate::host_file::read_regular(&canonical, FLASH_BYTES).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_image_is_taken_once_on_its_own_thread_for_its_own_fw() {
        let flash = vec![0xFFu8; 0x10000];
        record_built("official", &flash);
        assert_eq!(
            take_built("pk"),
            None,
            "another firmware's start reads nothing"
        );
        let other = std::thread::spawn(|| take_built("official"))
            .join()
            .expect("no panic");
        assert_eq!(other, None, "another thread reads nothing");
        let built = take_built("official").expect("this start's image");
        assert_eq!(built.nvs_seed_sha, nvs_seed_sha(&flash));
        assert_eq!(built.image_sha, image_sha_without_nvs(&flash));
        assert_eq!(take_built("official"), None, "taken once");
    }

    #[test]
    fn an_erased_flash_builds_a_machine_that_prints_the_rom_banner() {
        use pemu_core::hostio::SerialStream;
        use pemu_machine::run::RunLimits;

        let mut machine = build_machine(FlashImage::erased(), 1).expect("the bundled ROM machine");
        machine.run(RunLimits {
            until: None,
            max_insns: Some(1_200_000),
            stops: Default::default(),
        });
        let ring = machine.io().serial_ring(SerialStream::UsjTx);
        let text: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
        assert!(
            String::from_utf8_lossy(&text).contains("ESP-ROM:esp32c3-eco7"),
            "the synthesized eFuse selects the rev101 ROM"
        );
    }

    /// A synthetic merged image: a one-segment bootloader at 0, and a partition table with an NVS
    /// partition at 0x9000 and the cardid window as a second partition, both erased. No app.
    fn synthetic_flash() -> Vec<u8> {
        let mut flash = vec![0xFFu8; FLASH_BYTES as usize];
        let mut boot = vec![0xE9, 1, 2, 0x3F];
        boot.extend_from_slice(&0x4038_02E8u32.to_le_bytes());
        boot.extend_from_slice(&[0xEE, 0, 0, 0]);
        boot.extend_from_slice(&5u16.to_le_bytes());
        boot.push(3);
        boot.extend_from_slice(&3u16.to_le_bytes());
        boot.extend_from_slice(&199u16.to_le_bytes());
        boot.extend_from_slice(&[0; 5]);
        boot.extend_from_slice(&0x3FCD_5830u32.to_le_bytes());
        boot.extend_from_slice(&16u32.to_le_bytes());
        boot.extend_from_slice(&[7; 16]);
        while boot.len() % 16 != 15 {
            boot.push(0);
        }
        boot.push(0xEF ^ (0..16).fold(0u8, |c, _| c ^ 7));
        flash[..boot.len()].copy_from_slice(&boot);
        let row = |name: &str, offset: u32, size: u32| {
            let mut row = vec![0xAA, 0x50, 0x01, 0x02];
            row.extend_from_slice(&offset.to_le_bytes());
            row.extend_from_slice(&size.to_le_bytes());
            let mut label = [0u8; 16];
            label[..name.len()].copy_from_slice(name.as_bytes());
            row.extend_from_slice(&label);
            row.extend_from_slice(&[0; 4]);
            row
        };
        let mut table = row("nvs", 0x9000, 0x4000);
        table.extend(row("cardid", CARDID_WINDOW.0 as u32, 0x4000));
        flash[0x8000..0x8000 + table.len()].copy_from_slice(&table);
        flash
    }

    fn nvs_page(flash: &mut [u8], at: usize, ns: &str, key: &str) {
        let page = &mut flash[at..at + 4096];
        page.fill(0xFF);
        page[0..4].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
        page[4..8].copy_from_slice(&1u32.to_le_bytes());
        page[8] = 0xFE;
        let entry = |page: &mut [u8], i: usize, index: u8, name: &str, value: u8| {
            let off = 64 + i * 32;
            page[off..off + 32].fill(0);
            page[off] = index;
            page[off + 1] = 0x01;
            page[off + 2] = 1;
            page[off + 8..off + 8 + name.len()].copy_from_slice(name.as_bytes());
            page[off + 24] = value;
        };
        entry(page, 0, 0, ns, 1);
        entry(page, 1, 1, key, 42);
        // Entry state bitmap at offset 32: entries 0 and 1 written (0b10), the rest empty.
        page[32] = 0b1111_1010;
    }

    #[test]
    fn a_synthetic_erased_image_is_accepted() {
        let flash = synthetic_flash();
        merged_image(&flash).expect("the synthetic image parses");
        assert_eq!(refuse_secrets(&flash), Ok(()));
        let mut plain = flash.clone();
        nvs_page(&mut plain, 0x9000, "game_prefs", "volume");
        assert_eq!(
            refuse_secrets(&plain),
            Ok(()),
            "a plain NVS key is not a credential"
        );
    }

    #[test]
    fn a_non_erased_cardid_window_is_refused_without_its_bytes() {
        let mut flash = synthetic_flash();
        flash[CARDID_WINDOW.0 + 17] = 0x5A;
        let error = refuse_secrets(&flash).expect_err("an unerased cardid window");
        assert_eq!(error.code, E_SECRET_REFUSED);
        assert!(error.message.contains("cardid"), "{}", error.message);
        assert!(!error.message.contains("5a") && !error.message.contains("0x"));
        flash[CARDID_WINDOW.0 + 17] = 0xFF;
        flash[CARDID_WINDOW.1 - 1] = 0;
        assert_eq!(
            refuse_secrets(&flash).expect_err("last byte").code,
            E_SECRET_REFUSED
        );
        flash[CARDID_WINDOW.1 - 1] = 0xFF;
        flash[CARDID_WINDOW.1] = 0;
        assert_eq!(
            refuse_secrets(&flash),
            Ok(()),
            "the byte after the window is not cardid"
        );
    }

    #[test]
    fn an_nvs_credential_key_is_refused_without_naming_it() {
        let mut flash = synthetic_flash();
        nvs_page(&mut flash, 0x9000, "nvs.net80211", "sta.ssid");
        let error = refuse_secrets(&flash).expect_err("NVS credential keys");
        assert_eq!(error.code, E_SECRET_REFUSED);
        assert!(!error.message.contains("sta.ssid"), "{}", error.message);
    }

    /// Taken by the tests that swap the process-wide firmware source.
    static SOURCE_WORLD: Mutex<()> = Mutex::new(());

    fn source_world() -> std::sync::MutexGuard<'static, ()> {
        SOURCE_WORLD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn every_firmware_the_factory_builds_passes_the_secret_check() {
        let _world = source_world();
        let mut flash = synthetic_flash();
        flash[CARDID_WINDOW.0] = 0;
        let image = merged_image(&flash).expect("parses");
        *installed().lock().expect("lock") = Some(Arc::new(move |_| Ok(image.clone())));
        let args = StartArgs {
            fw: "synthetic".to_string(),
            ..StartArgs::default()
        };
        let Err(error) = machine_factory(&args) else {
            panic!("a cardid-bearing image must not build a machine");
        };
        assert_eq!(error.code, E_SECRET_REFUSED);
    }

    /// Writes a synthesized eFuse image into `dir` as `efuse_blk<N>.bin` files. A dump is defined
    /// by its origin, not its values, so no real device dump (MAC, unique id) is needed.
    fn write_synthetic_dump(dir: &Path) {
        use pemu_loader::efuse_image::BLOCK_WORDS;

        let synth = EfuseImage::synth(3);
        for (block, &words) in BLOCK_WORDS.iter().enumerate() {
            let bytes: Vec<u8> = synth.words()[block][..words]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect();
            std::fs::write(dir.join(format!("efuse_blk{block}.bin")), bytes).expect("a block");
        }
    }

    /// Both legs are asserted: a carry that always answered `Dump` would tell every ordinary run
    /// it holds a device's eFuse.
    #[test]
    fn the_efuse_source_the_factory_used_reaches_the_machines_receipt() {
        use pemu_machine::config::EfuseSource;

        let _world = source_world();
        let image = merged_image(&synthetic_flash()).expect("parses");
        *installed().lock().expect("lock") = Some(Arc::new(move |_| Ok(image.clone())));

        let stamp = format!("{}-{:?}", std::process::id(), std::thread::current().id());
        let dir = std::env::temp_dir().join(format!("pemu-backend-efuse-{stamp}"));
        std::fs::create_dir_all(&dir).expect("a dump directory");
        write_synthetic_dump(&dir);

        let synthetic = StartArgs {
            fw: "synthetic".to_string(),
            ..StartArgs::default()
        };
        let dumped = StartArgs {
            fw: "synthetic".to_string(),
            efuse_dump: Some(dir.to_string_lossy().into_owned()),
            confirm: Some("a person typed this".to_string()),
            ..StartArgs::default()
        };
        for (args, want) in [(synthetic, EfuseSource::Synth), (dumped, EfuseSource::Dump)] {
            let mut machine = machine_factory(&args).expect("the synthetic image builds");
            assert_eq!(
                pemu_machine::MachineApi::receipt(machine.as_mut()).efuse,
                Some(want),
                "the receipt names the eFuse image the machine holds"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_start_profile_argument_reaches_the_machine_the_factory_builds() {
        let _world = source_world();
        let image = merged_image(&synthetic_flash()).expect("parses");
        *installed().lock().expect("lock") = Some(Arc::new(move |_| Ok(image.clone())));
        for profile in [TimingProfileId::Fast, TimingProfileId::Device] {
            let args = StartArgs {
                fw: "synthetic".to_string(),
                profile,
                ..StartArgs::default()
            };
            let mut machine = machine_factory(&args).expect("the synthetic image builds");
            assert_eq!(
                pemu_machine::MachineApi::receipt(machine.as_mut()).profile,
                Some(profile),
            );
        }
    }

    /// Without the taint carry, `store_for_session` would answer `disk` and a device dump would
    /// reach the on-disk cache; breaking either half fails this test.
    #[test]
    fn a_start_on_an_efuse_dump_is_tainted_and_its_entry_stays_out_of_the_cache_role() {
        use crate::boot_cache::{CACHE_DIR, store_for_session};
        use pemu_api::commands::start::with_pool;

        let _world = source_world();
        let _store = crate::boot_cache::tests::store_gate();
        let stamp = format!("{}-{:?}", std::process::id(), std::thread::current().id());
        let dir = std::env::temp_dir().join(format!("pemu-backend-{stamp}"));
        let dump = dir.join("efuse-dump");
        let role = dir.join("cache-role");
        std::fs::create_dir_all(&dump).expect("a dump directory");
        std::fs::create_dir_all(&role).expect("a cache role");
        write_synthetic_dump(&dump);

        let image = merged_image(&synthetic_flash()).expect("parses");
        *installed().lock().expect("lock") = Some(Arc::new(move |_| Ok(image.clone())));
        crate::boot_cache::install(Some(role.clone()));
        let args = StartArgs {
            fw: "synthetic".to_string(),
            efuse_dump: Some(dump.to_string_lossy().into_owned()),
            confirm: Some("a person typed this".to_string()),
            ..StartArgs::default()
        };
        let backend = machine_factory(&args).expect("the dump and the synthetic image build");
        let id = with_pool(|pool| pool.attach(&args, backend));
        let (tainted, efuse, store) = with_pool(|pool| {
            let session = pool.session_mut(id).expect("the attached session");
            let receipt = session.receipt();
            let (tainted, efuse) = (receipt.tainted, receipt.efuse);
            let (store, disk) = store_for_session(session).expect("a store");
            (tainted, efuse, (store, disk.is_none()))
        });
        let _ = with_pool(|pool| pool.destroy(id));
        crate::boot_cache::install(None);
        assert!(
            tainted,
            "an instance started on an eFuse dump is tainted in its receipt"
        );
        assert_eq!(
            efuse,
            pemu_api::receipt::EfuseSource::Dump,
            "the receipt names the eFuse image the machine was actually built from"
        );
        assert_eq!(
            store,
            ("memory", true),
            "a tainted machine's boot-cache entries stay in memory"
        );
        assert!(
            !role.join(CACHE_DIR).exists(),
            "nothing of a tainted machine was written under the cache role"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A synthetic image's ROM boot leans on class C blocks and touches no unmodeled register in
    /// 400 ms, so the verdict is `pass`. The class U path is tested in `pemu_machine::machine` and
    /// `pemu_api::commands::start`.
    #[test]
    fn a_real_runs_fidelity_ledger_and_clock_reach_its_receipt() {
        use pemu_api::commands::start::with_pool;
        use pemu_api::receipt::Verdict;
        use pemu_core::time::VTime;

        let _world = source_world();
        let image = merged_image(&synthetic_flash()).expect("parses");
        *installed().lock().expect("lock") = Some(Arc::new(move |_| Ok(image.clone())));
        let args = StartArgs {
            fw: "synthetic".to_string(),
            ..StartArgs::default()
        };
        let backend = machine_factory(&args).expect("the synthetic image builds");
        let id = with_pool(|pool| pool.attach(&args, backend));
        let fresh = with_pool(|pool| pool.session_mut(id).expect("attached").receipt());
        let receipt = with_pool(|pool| {
            let session = pool.session_mut(id).expect("attached");
            session.run_until(VTime::from_us(400_000));
            session.receipt()
        });
        let _ = with_pool(|pool| pool.destroy(id));

        assert!(
            fresh.classes_touched.is_empty(),
            "a machine that has run nothing has leaned on nothing: {:?}",
            fresh.classes_touched
        );
        assert!(
            receipt.classes_touched.c.contains(&"system".to_string())
                && receipt.classes_touched.c.contains(&"uart0".to_string()),
            "the ROM run leans on the modeled blocks it drives: {:?}",
            receipt.classes_touched
        );
        assert_eq!(
            receipt.verdict(true),
            Verdict::Pass,
            "this run reached no unmodeled register, so it has nothing to caveat: {:?}",
            receipt.caveats()
        );
        assert_eq!(receipt.cpi_milli, 1_000, "the `fast` profile's ratio");
        assert_eq!(
            receipt.journal_len, 0,
            "nothing was journaled into this run"
        );
        assert_eq!(
            receipt.host_parity.as_deref(),
            Some(pemu_api::receipt::HOST_PARITY)
        );
    }

    /// Taint from flash: `Machine::is_tainted` answers for the eFuse alone, but `Pool::attach`
    /// reads the image's secret sources (cardid, NVS credentials) and taints the instance, and
    /// `Session::receipt` ORs the two, so `boot_cache::store_for_session` picks memory.
    #[test]
    fn a_machine_built_from_a_cardid_bearing_flash_is_tainted_in_its_receipt() {
        use crate::boot_cache::{CACHE_DIR, store_for_session};
        use pemu_api::commands::start::with_pool;

        let _world = source_world();
        let _store = crate::boot_cache::tests::store_gate();
        let stamp = format!("{}-{:?}", std::process::id(), std::thread::current().id());
        let role = std::env::temp_dir().join(format!("pemu-backend-flash-{stamp}"));
        std::fs::create_dir_all(&role).expect("a cache role");

        // An image `refuse_secrets` refuses, built directly so the carry is proved before any
        // surface can produce one.
        let mut bytes = synthetic_flash();
        bytes[CARDID_WINDOW.0] = 0;
        refuse_secrets(&bytes).expect_err("this image is exactly the one MCP and HTTP refuse");
        let image = merged_image(&bytes).expect("parses");
        let machine =
            build_machine_with_elf(image, None, 1, TimingProfileId::Fast).expect("it builds");

        crate::boot_cache::install(Some(role.clone()));
        let args = StartArgs {
            fw: "synthetic".to_string(),
            ..StartArgs::default()
        };
        let id = with_pool(|pool| pool.attach(&args, Box::new(machine)));
        let (own, tainted, store) = with_pool(|pool| {
            let session = pool.session_mut(id).expect("the attached session");
            let own = pemu_machine::MachineApi::is_tainted(session.machine());
            let tainted = session.receipt().tainted;
            let (store, disk) = store_for_session(session).expect("a store");
            (own, tainted, (store, disk.is_none()))
        });
        let _ = with_pool(|pool| pool.destroy(id));
        crate::boot_cache::install(None);

        assert!(
            !own,
            "`Machine::is_tainted` answers for the eFuse alone, so it is `false` here. If this \
             ever becomes `true` the flash taint gained a second source and the two must be \
             reconciled rather than left to OR by accident"
        );
        assert!(
            tainted,
            "an instance built from a flash whose cardid window is not erased \
             is tainted in its receipt, through the `pemu-api` secret store"
        );
        assert_eq!(
            store,
            ("memory", true),
            "a flash-tainted machine's boot-cache entries stay in memory, exactly as an \
             eFuse-tainted one's do"
        );
        assert!(
            !role.join(CACHE_DIR).exists(),
            "nothing of a flash-tainted machine was written under the cache role"
        );
        std::fs::remove_dir_all(&role).ok();
    }

    /// Without `--allow-tainted` the image is refused; with it the machine is built; and with it
    /// on an erased image the instance is untainted, because taint comes from the bytes, never the
    /// flag.
    #[test]
    fn allow_tainted_is_the_only_way_a_secret_bearing_flash_builds_a_machine() {
        use pemu_api::commands::start::with_pool;

        let _world = source_world();
        let mut bytes = synthetic_flash();
        bytes[CARDID_WINDOW.0] = 0;
        let secret = merged_image(&bytes).expect("parses");
        let erased = merged_image(&synthetic_flash()).expect("parses");

        *installed().lock().expect("lock") = Some(Arc::new(move |_| Ok(secret.clone())));
        let plain = StartArgs {
            fw: "backup".to_string(),
            ..StartArgs::default()
        };
        let Err(refused) = machine_factory(&plain) else {
            panic!("a secret-bearing flash is refused without `allow_tainted`");
        };
        assert_eq!(refused.code, E_SECRET_REFUSED);

        let allowed = StartArgs {
            fw: "backup".to_string(),
            allow_tainted: true,
            confirm: Some("a person typed this".to_string()),
            ..StartArgs::default()
        };
        let backend = machine_factory(&allowed).expect("`allow_tainted` loads it");
        let id = with_pool(|pool| pool.attach(&allowed, backend));
        let tainted = with_pool(|pool| {
            pool.session_mut(id)
                .expect("the attached session")
                .receipt()
                .tainted
        });
        let _ = with_pool(|pool| pool.destroy(id));
        assert!(
            tainted,
            "the instance this surface produces is tainted in its receipt"
        );

        *installed().lock().expect("lock") = Some(Arc::new(move |_| Ok(erased.clone())));
        let backend = machine_factory(&allowed).expect("an erased image loads either way");
        let id = with_pool(|pool| pool.attach(&allowed, backend));
        let untainted = with_pool(|pool| {
            pool.session_mut(id)
                .expect("the attached session")
                .receipt()
                .tainted
        });
        let _ = with_pool(|pool| pool.destroy(id));
        assert!(
            !untainted,
            "the taint is read from the image's bytes, never from `allow_tainted`"
        );
    }

    /// Counts `screenshot` artifact writes, so a test can say none happened.
    static SCREENSHOT_WRITES: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn counting_write(path: &str, _bytes: &[u8]) -> Result<String, String> {
        SCREENSHOT_WRITES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(path.to_owned())
    }

    /// Uses a real tainted machine from an `--efuse-dump` start so taint travels the product's
    /// way. The untainted leg asserts the capture is written, so a refusal widened to every
    /// instance fails; the counter proves the writer is never reached.
    #[test]
    fn a_tainted_instance_is_refused_a_screenshot_before_anything_is_written() {
        use pemu_api::commands::screenshot::{
            ScreenshotArgs, ScreenshotCodec, screenshot_on, set_io,
        };
        use pemu_api::commands::start::with_pool;
        use std::sync::atomic::Ordering;

        let _world = source_world();
        let image = merged_image(&synthetic_flash()).expect("parses");
        *installed().lock().expect("lock") = Some(Arc::new(move |_| Ok(image.clone())));

        let stamp = format!("{}-{:?}", std::process::id(), std::thread::current().id());
        let dir = std::env::temp_dir().join(format!("pemu-backend-shot-{stamp}"));
        std::fs::create_dir_all(&dir).expect("a dump directory");
        write_synthetic_dump(&dir);

        let previous = SCREENSHOT_WRITES.swap(0, Ordering::SeqCst);
        debug_assert_eq!(previous, 0, "the counter starts at zero");
        set_io(ScreenshotCodec {
            encode: |w, h, rgb| {
                let mut out = w.to_le_bytes().to_vec();
                out.extend_from_slice(&h.to_le_bytes());
                out.extend_from_slice(rgb);
                Ok(out)
            },
            decode: |_| Err("this test never reads a golden".to_owned()),
        });
        let previous_io = pemu_api::artifact_io::replace(Some(pemu_api::artifact_io::ArtifactIo {
            write: counting_write,
            read: |_| Err("this test never reads an artifact".to_owned()),
        }));

        let tainted_args = StartArgs {
            fw: "synthetic".to_string(),
            efuse_dump: Some(dir.to_string_lossy().into_owned()),
            confirm: Some("a person typed this".to_string()),
            ..StartArgs::default()
        };
        let clean_args = StartArgs {
            fw: "synthetic".to_string(),
            ..StartArgs::default()
        };
        let shot = ScreenshotArgs::from_json(&serde_json::json!({ "view": "raw" }))
            .expect("the arguments parse");

        let backend = machine_factory(&tainted_args).expect("the dump and the image build");
        let tainted_id = with_pool(|pool| pool.attach(&tainted_args, backend));
        let refused = with_pool(|pool| {
            let session = pool.session_mut(tainted_id).expect("the attached session");
            screenshot_on(session, &shot).expect_err("a tainted capture is refused")
        });
        let _ = with_pool(|pool| pool.destroy(tainted_id));
        assert_eq!(refused.code, E_SECRET_REFUSED);
        assert_eq!(
            SCREENSHOT_WRITES.load(Ordering::SeqCst),
            0,
            "nothing of a tainted machine's panel reaches an artifact writer"
        );

        let backend = machine_factory(&clean_args).expect("the image builds");
        let clean_id = with_pool(|pool| pool.attach(&clean_args, backend));
        let captured = with_pool(|pool| {
            let session = pool.session_mut(clean_id).expect("the attached session");
            screenshot_on(session, &shot).map(|_| ())
        });
        let _ = with_pool(|pool| pool.destroy(clean_id));
        captured.expect("an untainted instance still captures its panel");
        assert_eq!(
            SCREENSHOT_WRITES.load(Ordering::SeqCst),
            1,
            "the refusal is the taint's and not the command's: an untainted capture is written"
        );

        pemu_api::artifact_io::replace(previous_io);
        SCREENSHOT_WRITES.store(0, Ordering::SeqCst);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dump_without_a_confirmation_never_reaches_the_factory() {
        let refused = StartArgs::from_json(&serde_json::json!({
            "fw": "synthetic",
            "efuse_dump": "/tmp/does-not-need-to-exist",
        }))
        .expect_err("a tainted load is confirmed by a person");
        assert_eq!(refused.code, E_SECRET_REFUSED);
        StartArgs::from_json(&serde_json::json!({
            "fw": "synthetic",
            "efuse_dump": "/tmp/does-not-need-to-exist",
            "confirm": "a person typed this",
        }))
        .expect("a confirmed dump parses");
    }

    #[test]
    fn a_path_is_absolute_capped_and_refused_with_one_error_whatever_the_cause() {
        let error = read_path("relative/fw.bin").expect_err("relative");
        assert_eq!(error.code, E_USAGE);

        let dir = std::env::temp_dir().join(format!("pemu-backend-fw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let big = dir.join("big.bin");
        std::fs::write(&big, vec![0xFFu8; FLASH_BYTES as usize + 1]).expect("write");
        let absent = dir.join("absent.bin");
        let cases = [dir.clone(), big, absent];
        let errors: Vec<ApiError> = cases
            .iter()
            .map(|p| read_path(&p.to_string_lossy()).expect_err("refused"))
            .collect();
        for (error, path) in errors.iter().zip(&cases) {
            assert_eq!(error.code, E_ASSET_MISSING);
            let generic = start::firmware_not_found(&path.to_string_lossy());
            assert_eq!(error.message, generic.message, "one error for every cause");
        }

        let Err(error) = merged_image(b"short") else {
            panic!("a five-byte firmware is not a merged image");
        };
        assert_eq!(error.code, E_USAGE);
        assert!(
            !error.message.contains("magic"),
            "no loader detail: {}",
            error.message
        );
    }
}
