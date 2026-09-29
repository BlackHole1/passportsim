//! Where a browser's walkers find an app ELF: what `pemu_host::hooks` answers from `corpus.toml`,
//! this module answers from the ELF the page handed the core as `pemu_load` kind 2. The walkers
//! are `pemu_api::elf`'s; this only puts an `ElfSource` behind them. Not `cfg`-gated on wasm.
//!
//! Nothing costs at boot: the DWARF walk runs at the first `inspect` or `ui` call of a firmware
//! and is remembered for the life of the machine. The ELF bytes stay alive because the panic
//! decoder borrows them to symbolize and unwind.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use pemu_api::elf::{ElfSource, install_walkers};
use pemu_loader::elf::ElfInfo;

pub use pemu_api::elf::{ElfContext, SAFE_POINT_SLICE, ui_safe_point};

/// What a page does about a firmware whose app ELF this process does not hold. The daemon's
/// counterpart names a `corpus.toml` entry instead; a page has no corpus.
pub const MISSING_ELF: &str =
    "the app ELF of this firmware (load it as `pemu_load` kind 2, or drop it beside the image)";

/// Every firmware whose app ELF this process has, weakly: the strong reference lives in the
/// [`crate::instance::Instance`], so an entry never keeps megabytes alive after `pemu_drop`.
fn registry() -> &'static Mutex<BTreeMap<String, Weak<ElfContext>>> {
    static ELVES: OnceLock<Mutex<BTreeMap<String, Weak<ElfContext>>>> = OnceLock::new();
    ELVES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Publishes `elf` as the app ELF of firmware `fw` and installs the walkers. The returned handle
/// keeps the context alive. A firmware built twice replaces the older entry, and dead entries are
/// swept here, so the map holds at most one live and one dead entry per `fw`.
pub fn register(fw: &str, elf: Arc<ElfInfo>, bytes: Arc<[u8]>) -> Arc<ElfContext> {
    let context = Arc::new(ElfContext::new(elf, bytes));
    {
        let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, weak| weak.strong_count() > 0);
        map.insert(fw.to_owned(), Arc::downgrade(&context));
    }
    install();
    context
}

/// The ELF context of `fw`, or `None` when no live machine of this process was built from one.
#[must_use]
pub fn elf_of(fw: &str) -> Option<Arc<ElfContext>> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(fw)
        .and_then(Weak::upgrade)
}

#[must_use]
pub fn elves() -> ElfSource {
    ElfSource::new(Arc::new(|fw: &str| elf_of(fw)), MISSING_ELF)
}

/// Installs the `inspect` and `ui` walkers and the `ui` safe point, once. Also called from
/// `crate::instance::build`, so a machine without an app ELF gets walkers that name the missing
/// ELF rather than a "no DWARF definition" error that reads as a firmware fault.
pub fn install() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| install_walkers(elves()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled ROM: a real RISC-V ELF with no `.debug_info`, which is all these tests need.
    fn an_elf() -> Option<(Arc<ElfInfo>, Arc<[u8]>)> {
        let bytes = pemu_loader::rom::bundled_opt(pemu_loader::rom::RomRev::Rev101)?;
        let elf = ElfInfo::parse(bytes).ok()?;
        Some((Arc::new(elf), Arc::from(bytes)))
    }

    /// The words must tell a page how to supply an ELF, never mention a corpus it cannot have, and
    /// not read as "this firmware has no LVGL".
    #[test]
    fn a_missing_elf_names_what_a_page_can_actually_do_about_it() {
        install();
        assert!(MISSING_ELF.contains("app ELF"), "{MISSING_ELF}");
        assert!(MISSING_ELF.contains("pemu_load"), "{MISSING_ELF}");
        assert!(
            !MISSING_ELF.contains("corpus"),
            "a page has no corpus: {MISSING_ELF}"
        );
        assert!(!MISSING_ELF.contains("_lv_global_t"), "{MISSING_ELF}");
    }

    /// An ELF lives exactly as long as its machine, and registering parses no DWARF.
    #[test]
    fn registering_publishes_the_context_and_dropping_the_handle_sweeps_it() {
        let Some((elf, bytes)) = an_elf() else {
            return;
        };
        let handle = register("sweep-me", elf, bytes);
        assert!(
            elf_of("sweep-me").is_some(),
            "a live machine's ELF is found"
        );
        assert!(
            elves().elf_of("sweep-me").is_some(),
            "and the source this host installs resolves it"
        );
        assert!(
            !handle.layouts_resolved(),
            "registering parses no DWARF (the install is lazy)"
        );
        drop(handle);
        assert!(
            elf_of("sweep-me").is_none(),
            "the context dies with the machine that held it"
        );
        assert!(elves().elf_of("sweep-me").is_none());
    }
}
