//! The app ELF of a running firmware, and the `inspect` and `ui` walkers that read the guest
//! through it.
//!
//! Walkers need the DWARF layouts of the image the instance started from, and the machine carries
//! no ELF. A walk resolves its firmware name through the installed [`ElfSource`], the only per-host
//! part: a daemon reads `corpus.toml` or its payload, a browser holds the ELF the page loaded. The
//! walkers live here once so the page and the CLI answer the same call the same way. Layouts are
//! resolved at the first walk unless [`ElfContext::parse`] asks for them at once.
//!
//! `inspect nvs` is not answered: it needs the NVS partition in flash, and the facade gives a
//! walker guest RAM only.

use std::sync::{Arc, Mutex, OnceLock};

use pemu_introspect::layout::Layouts;
use pemu_introspect::{GuestMemory, IntrospectError, MemError};
use pemu_loader::elf::ElfInfo;
use pemu_machine::MachineApi;
use pemu_machine::machine::GuestMem;

use crate::commands::inspect::{self, Introspectors};

/// What every walk over one firmware needs from its app ELF.
#[derive(Debug)]
pub struct ElfContext {
    /// Shared with the machines built for this firmware, which bind HLE against the same `Arc`.
    pub elf: Arc<ElfInfo>,
    /// Kept for the panic decoder, which borrows them once per panic rather than once per walk.
    pub bytes: Arc<[u8]>,
    /// Remembered, so a firmware without debug information is not re-parsed per call.
    layouts: OnceLock<Result<Layouts, IntrospectError>>,
    /// Resolved at the first call that names a global, so a caller that never does never pays the
    /// unit scan.
    pub globals: pemu_introspect::vars::GlobalsCache,
}

impl ElfContext {
    /// Layouts are resolved at the first walk.
    #[must_use]
    pub fn new(elf: Arc<ElfInfo>, bytes: Arc<[u8]>) -> ElfContext {
        ElfContext {
            elf,
            bytes,
            layouts: OnceLock::new(),
            globals: pemu_introspect::vars::GlobalsCache::new(),
        }
    }

    /// Resolves the layouts at once, so a host learns at load that the ELF has no usable debug
    /// information.
    pub fn parse(bytes: &[u8]) -> Result<ElfContext, IntrospectError> {
        let elf = ElfInfo::parse(bytes)
            .map_err(|e| IntrospectError::Dwarf(format!("the app ELF does not parse: {e:?}")))?;
        let layouts = pemu_introspect::dwarf::DebugInfo::parse(&elf, bytes)?.layouts();
        let context = ElfContext::new(Arc::new(elf), Arc::from(bytes));
        let _ = context.layouts.set(Ok(layouts));
        Ok(context)
    }

    /// Resolved at the first call and remembered.
    pub fn layouts(&self) -> Result<&Layouts, IntrospectError> {
        self.layouts
            .get_or_init(|| {
                pemu_introspect::dwarf::DebugInfo::parse(&self.elf, &self.bytes)
                    .map(|debug| debug.layouts())
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Lets a test tell a lazy parse from an eager one.
    #[must_use]
    pub fn layouts_resolved(&self) -> bool {
        self.layouts.get().is_some()
    }
}

/// `fw` is a corpus id or the file name of a path.
pub type ElfResolver = Arc<dyn Fn(&str) -> Option<Arc<ElfContext>> + Send + Sync>;

/// The refusal text is part of the seam because it differs per host: a daemon asks for an `elf` in
/// `corpus.toml`, a page for the ELF as `pemu_load` kind 2.
#[derive(Clone)]
pub struct ElfSource {
    resolve: ElfResolver,
    missing: &'static str,
}

impl ElfSource {
    /// `missing` completes a refusal read as "no ...", so it names the app ELF and how this host
    /// gets one.
    #[must_use]
    pub fn new(resolve: ElfResolver, missing: &'static str) -> ElfSource {
        ElfSource { resolve, missing }
    }

    #[must_use]
    pub fn elf_of(&self, fw: &str) -> Option<Arc<ElfContext>> {
        (self.resolve)(fw)
    }

    #[must_use]
    pub const fn missing(&self) -> &'static str {
        self.missing
    }
}

impl std::fmt::Debug for ElfSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElfSource")
            .field("missing", &self.missing)
            .finish_non_exhaustive()
    }
}

/// For an embedder that never called [`install_walkers`], not a firmware without an ELF.
pub const NO_ELF_SOURCE: &str = "the app ELF of this firmware (this build installed no ELF source)";

fn installed() -> &'static Mutex<Option<ElfSource>> {
    static SOURCE: OnceLock<Mutex<Option<ElfSource>>> = OnceLock::new();
    SOURCE.get_or_init(|| Mutex::new(None))
}

/// Most hosts call [`install_walkers`] instead.
pub fn set_elf_source(source: Option<ElfSource>) {
    *installed().lock().unwrap_or_else(|e| e.into_inner()) = source;
}

#[must_use]
pub fn elf_source() -> Option<ElfSource> {
    installed()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[must_use]
pub fn elf_of(fw: &str) -> Option<Arc<ElfContext>> {
    elf_source().and_then(|source| source.elf_of(fw))
}

#[must_use]
pub fn missing_elf() -> IntrospectError {
    IntrospectError::MissingSymbol {
        name: elf_source().map_or(NO_ELF_SOURCE, |source| source.missing()),
    }
}

fn walking() -> Result<Arc<ElfContext>, IntrospectError> {
    let fw = inspect::walk_firmware().ok_or(IntrospectError::MissingSymbol {
        name: "the firmware of this walk (a walk runs inside `with_walk_firmware`)",
    })?;
    elf_of(&fw).ok_or_else(missing_elf)
}

/// Reads the arena as a guest load does and never reaches a peripheral, so a walk has no side
/// effect.
pub struct FacadeMemory<'a>(pub GuestMem<'a>);

impl GuestMemory for FacadeMemory<'_> {
    fn read(&self, addr: u32, out: &mut [u8]) -> Result<(), MemError> {
        let len = u32::try_from(out.len()).unwrap_or(u32::MAX);
        let fail = || MemError { addr, len };
        let mut at = 0usize;
        while at < out.len() {
            let here = addr.wrapping_add(u32::try_from(at).map_err(|_| fail())?);
            let size = if out.len() - at >= 4 { 4 } else { 1 };
            let value = self.0.load(here, size).ok_or_else(fail)?;
            out[at..at + usize::from(size)]
                .copy_from_slice(&value.to_le_bytes()[..usize::from(size)]);
            at += usize::from(size);
        }
        Ok(())
    }
}

fn walk_tasks(
    machine: &mut dyn MachineApi,
) -> Result<pemu_introspect::freertos::TaskSnapshot, IntrospectError> {
    let cx = walking()?;
    let layouts = cx.layouts()?;
    let mem = FacadeMemory(machine.guest_mem());
    let mut snapshot = pemu_introspect::freertos::walk_tasks(layouts, &cx.elf.symbols, &mem)?;
    pemu_introspect::freertos::resolve_mutex_waits(&mut snapshot, layouts, &mem);
    Ok(snapshot)
}

fn walk_heap(
    machine: &mut dyn MachineApi,
) -> Result<pemu_introspect::tlsf::HeapSnapshot, IntrospectError> {
    let cx = walking()?;
    let layouts = cx.layouts()?;
    let mem = FacadeMemory(machine.guest_mem());
    pemu_introspect::tlsf::walk_heaps(layouts, &cx.elf.symbols, &mem)
}

fn walk_ui(
    machine: &mut dyn MachineApi,
    rev: u64,
) -> Result<pemu_introspect::lvgl::UiTree, IntrospectError> {
    let cx = walking()?;
    let layouts = cx.layouts()?;
    let mem = FacadeMemory(machine.guest_mem());
    pemu_introspect::lvgl::walk_ui(layouts, &cx.elf.symbols, &mem, rev)
}

/// Decodes the `panic_info_t` at `info`: the exception frame through DWARF, then the stack unwound
/// over `.debug_frame`, with ROM frames named from the bundled ROM ELF.
fn walk_panic(
    machine: &mut dyn MachineApi,
    info: u32,
) -> Result<pemu_introspect::panic::PanicRecord, IntrospectError> {
    use pemu_introspect::dwarf::DebugInfo;
    use pemu_introspect::panic::PanicRecord;
    use pemu_introspect::unwind::{Symbolizer, Unwinder};
    let cx = walking()?;
    let layouts = cx.layouts()?;
    let mem = FacadeMemory(machine.guest_mem());
    let frame = layouts.require("panic_info_t")?.u32(&mem, info, "frame")?;
    let mut record = PanicRecord::from_exception_frame(layouts, &mem, frame)?;
    // `panic_info_t.reason` is the IDF's own text ("Interrupt wdt timeout on CPU0"), which the
    // exception frame alone cannot give.
    if let Ok(reason) = layouts.require("panic_info_t")?.u32(&mem, info, "reason")
        && reason != 0
    {
        let text = mem.cstr(reason, 128);
        if !text.is_empty() {
            record = record.with_detail(text, None);
        }
    }
    let debug = DebugInfo::parse(&cx.elf, &cx.bytes)?;
    let unwinder = Unwinder::from_debug_info(&debug);
    let rom = bundled_rom_symbols();
    let mut symbols = Symbolizer::new(&cx.elf.symbols).with_debug(&debug);
    if let Some(rom) = rom.as_deref() {
        symbols = symbols.with_rom(rom);
    }
    Ok(record.unwind(&unwinder, &mem, &symbols, None))
}

/// UNVERIFIED for a machine on another ROM revision; daemon and browser machines both synthesize a
/// rev101 eFuse.
fn bundled_rom_symbols() -> Option<Arc<pemu_loader::symbols::SymbolTable>> {
    static ROM: OnceLock<Option<Arc<pemu_loader::symbols::SymbolTable>>> = OnceLock::new();
    ROM.get_or_init(|| {
        let bytes = pemu_loader::rom::bundled_opt(pemu_loader::rom::RomRev::Rev101)?;
        ElfInfo::parse(bytes).ok().map(|elf| Arc::new(elf.symbols))
    })
    .clone()
}

fn walk_vars(
    machine: &mut dyn MachineApi,
    queries: &[pemu_introspect::vars::VarQuery],
) -> Result<pemu_introspect::vars::VarSnapshot, IntrospectError> {
    let cx = walking()?;
    let globals = cx.globals.get(&cx.elf, &cx.bytes)?;
    let mem = FacadeMemory(machine.guest_mem());
    Ok(pemu_introspect::vars::read_all(globals, &mem, queries))
}

fn walk_nvs(
    machine: &mut dyn MachineApi,
) -> Result<pemu_introspect::nvs::NvsListing, IntrospectError> {
    (inspect::NO_INTROSPECTORS.nvs)(machine)
}

pub const INTROSPECTORS: Introspectors = Introspectors {
    tasks: walk_tasks,
    heap: walk_heap,
    nvs: walk_nvs,
    ui: walk_ui,
    panic: walk_panic,
    vars: walk_vars,
};

/// The `tasks` walk without stack high-water marks, whose cost scales with stack size, plus the
/// deadlock verdict. The ELF is passed in, so a per-slice check does not serialize against other
/// instances' walks. Anything uncertain is `None`.
#[must_use]
pub fn deadlock_report(
    machine: &mut dyn MachineApi,
    cx: &ElfContext,
) -> Option<pemu_introspect::freertos::DeadlockReport> {
    use pemu_introspect::freertos::{WalkOptions, resolve_mutex_waits};
    let layouts = cx.layouts().ok()?;
    let mem = FacadeMemory(machine.guest_mem());
    let mut snapshot = pemu_introspect::freertos::walk_tasks_with(
        layouts,
        &cx.elf.symbols,
        &mem,
        WalkOptions {
            stack_high_water: false,
        },
    )
    .ok()?;
    resolve_mutex_waits(&mut snapshot, layouts, &mem);
    // Spelled out, because this function has the same name.
    pemu_introspect::freertos::deadlock_report(&snapshot)
}

/// Evaluates the LVGL safe point on `machine`, with the tree walked at the same instant.
/// `Ok(false)` covers every "not yet". The lock check gets pc `0` because the facade has none,
/// which can only make an instant not safe, never a wrong instant safe.
pub fn at_safe_point(fw: &str, machine: &mut dyn MachineApi) -> Result<bool, IntrospectError> {
    let cx = elf_of(fw).ok_or_else(missing_elf)?;
    let layouts = cx.layouts()?;
    let mem = FacadeMemory(machine.guest_mem());
    let Ok(tree) = pemu_introspect::lvgl::walk_ui(layouts, &cx.elf.symbols, &mem, 0) else {
        return Ok(false);
    };
    Ok(
        pemu_introspect::safepoint::evaluate(layouts, &cx.elf.symbols, &mem, 0, Some(&tree), None)
            .is_ok_and(|point| point.is_safe()),
    )
}

/// Virtual time between two safe-point checks of `ui` and `ui.expect`.
pub const SAFE_POINT_SLICE: pemu_core::time::VTime = pemu_core::time::VTime::from_ms(1);

/// Runs the session in [`SAFE_POINT_SLICE`]s until the LVGL safe point holds or `timeout` of
/// virtual time is spent. A firmware with no ELF answers `None`.
pub fn ui_safe_point(
    session: &mut crate::session::Session,
    timeout: pemu_core::time::VTime,
) -> Result<Option<bool>, crate::error::ApiError> {
    use pemu_core::time::VTime;
    let fw = session.fw.clone();
    if elf_of(&fw).is_none() {
        return Ok(None);
    }
    let deadline = VTime(session.now().0.saturating_add(timeout.0));
    loop {
        if at_safe_point(&fw, session.machine()).map_err(|e| inspect::walker_error("ui", &e))? {
            return Ok(Some(true));
        }
        if session.now() >= deadline {
            return Ok(Some(false));
        }
        if session.cancelled() {
            return Err(crate::pool::cancelled(session.id));
        }
        let until = VTime(
            session
                .now()
                .0
                .saturating_add(SAFE_POINT_SLICE.0)
                .min(deadline.0),
        );
        let outcome = session.run_until(until);
        if !matches!(
            outcome.reason,
            pemu_machine::stops::StopReason::Until | pemu_machine::stops::StopReason::MaxInsns
        ) {
            return Ok(Some(
                at_safe_point(&fw, session.machine())
                    .map_err(|e| inspect::walker_error("ui", &e))?,
            ));
        }
    }
}

/// A second call replaces the first. No DWARF is parsed here.
pub fn install_walkers(source: ElfSource) {
    set_elf_source(Some(source));
    inspect::set_introspectors(INTROSPECTORS);
    crate::commands::ui::set_safe_point(Some(ui_safe_point));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::inspect::tests::world;

    /// The bundled rev101 ROM: a real RISC-V ELF in every build, with no `.debug_info`, so its
    /// layouts resolve to nothing.
    fn an_elf() -> Option<(Arc<ElfInfo>, Arc<[u8]>)> {
        let bytes = pemu_loader::rom::bundled_opt(pemu_loader::rom::RomRev::Rev101)?;
        let elf = ElfInfo::parse(bytes).ok()?;
        Some((Arc::new(elf), Arc::from(bytes)))
    }

    fn a_source(fw: &'static str, cx: Arc<ElfContext>) -> ElfSource {
        ElfSource::new(
            Arc::new(move |asked: &str| (asked == fw).then(|| Arc::clone(&cx))),
            "the app ELF of this firmware (the way this host is given one)",
        )
    }

    fn a_machine() -> pemu_machine::Machine {
        let assets = pemu_machine::config::Assets::with_bundled_rom(
            pemu_loader::bundle::FlashImage::erased(),
            None,
            None,
            pemu_loader::efuse_image::EfuseImage::synth(0),
        )
        .expect("the bundled ROM is pinned");
        pemu_machine::Machine::new(pemu_machine::config::MachineConfig::default(), assets)
            .expect("composes")
    }

    #[test]
    fn facade_memory_reads_what_the_rom_left_in_ram_and_refuses_an_unbacked_range() {
        use pemu_machine::run::RunLimits;
        let mut machine = a_machine();
        machine.run(RunLimits {
            until: None,
            max_insns: Some(200_000),
            stops: Default::default(),
        });
        let mem = FacadeMemory(machine.guest_mem());
        // A 6-byte read crosses the reader's word-then-byte split.
        let base = 0x3fc8_0000;
        let mut wide = [0u8; 6];
        mem.read(base, &mut wide).expect("SRAM is backed");
        for (i, byte) in wide.iter().enumerate() {
            assert_eq!(Some(u32::from(*byte)), mem.0.load(base + i as u32, 1));
        }
        let err = mem
            .read(0x0000_0000, &mut [0u8; 4])
            .expect_err("no memory at 0");
        assert_eq!((err.addr, err.len), (0, 4));
    }

    /// Only the words differ per host.
    #[test]
    fn a_walk_without_an_elf_names_the_elf_and_this_host_s_way_to_supply_one() {
        let _world = world();
        set_elf_source(None);
        let outside = walking().expect_err("no walk is in progress");
        assert!(
            format!("{outside}").contains("with_walk_firmware"),
            "outside a walk the error names the gate, not a guest structure: {outside}"
        );
        let unsourced = inspect::with_walk_firmware("no-such-firmware", walking)
            .expect_err("no source is installed");
        assert!(
            format!("{unsourced}").contains("app ELF"),
            "a build with no source still names the app ELF: {unsourced}"
        );

        let Some((elf, bytes)) = an_elf() else {
            return;
        };
        let cx = Arc::new(ElfContext::new(elf, bytes));
        set_elf_source(Some(a_source("known", cx)));
        let missing =
            inspect::with_walk_firmware("other", walking).expect_err("the source knows one name");
        assert!(
            format!("{missing}").contains("the way this host is given one"),
            "the refusal is the installed host's words: {missing}"
        );
        assert!(
            inspect::with_walk_firmware("known", walking).is_ok(),
            "the firmware the source knows resolves"
        );
        set_elf_source(None);
    }

    #[test]
    fn every_walker_resolves_its_elf_through_the_installed_source() {
        let _world = world();
        let Some((elf, bytes)) = an_elf() else {
            return;
        };
        let mut machine = a_machine();
        let cx = Arc::new(ElfContext::new(elf, bytes));
        set_elf_source(Some(a_source("known", Arc::clone(&cx))));

        let refusals: Vec<String> = inspect::with_walk_firmware("other", || {
            vec![
                format!(
                    "{}",
                    (INTROSPECTORS.tasks)(&mut machine).expect_err("tasks")
                ),
                format!("{}", (INTROSPECTORS.heap)(&mut machine).expect_err("heap")),
                format!("{}", (INTROSPECTORS.ui)(&mut machine, 1).expect_err("ui")),
                format!(
                    "{}",
                    (INTROSPECTORS.panic)(&mut machine, 0).expect_err("panic")
                ),
                format!(
                    "{}",
                    (INTROSPECTORS.vars)(&mut machine, &[]).expect_err("vars")
                ),
            ]
        });
        for refusal in &refusals {
            assert!(
                refusal.contains("the way this host is given one"),
                "every walker names the missing ELF the same way: {refusal}"
            );
        }
        // The safe point runs outside the walk gate.
        let safe = at_safe_point("other", &mut machine).expect_err("no ELF for it");
        assert!(
            format!("{safe}").contains("the way this host is given one"),
            "{safe}"
        );
        assert_eq!(
            at_safe_point("known", &mut machine),
            Ok(false),
            "a firmware the source knows is a plain `not yet`, never the ELF refusal"
        );
        set_elf_source(None);
    }

    #[test]
    fn layouts_are_lazy_by_default_eager_on_parse_and_remembered_either_way() {
        let Some((elf, bytes)) = an_elf() else {
            return;
        };
        let cx = ElfContext::new(Arc::clone(&elf), Arc::clone(&bytes));
        assert!(
            !cx.layouts_resolved(),
            "a context parses no DWARF until it is walked"
        );
        let first = cx
            .layouts()
            .map(|l| l.iter().count())
            .map_err(|e| format!("{e}"));
        assert!(
            cx.layouts_resolved(),
            "the attempt is remembered either way"
        );
        let second = cx
            .layouts()
            .map(|l| l.iter().count())
            .map_err(|e| format!("{e}"));
        assert_eq!(first, second, "a second walk answers from the memo");

        // The ROM ELF has no `.debug_info`, so `parse` refuses it where `new` defers the error.
        assert_eq!(
            ElfContext::parse(&bytes)
                .map(|_| ())
                .map_err(|e| format!("{e}")),
            first.map(|_| ()),
            "`parse` and the first walk of `new` reach the same verdict"
        );
    }

    #[test]
    fn the_safe_point_loop_answers_none_without_an_elf() {
        use crate::commands::start::{Boot, StartArgs};
        use crate::pool::Pool;
        let _world = world();
        set_elf_source(None);
        let mut pool = Pool::new();
        let id = pool.attach(
            &StartArgs {
                fw: "no-elf-here".to_owned(),
                boot: Boot::None,
                ..StartArgs::default()
            },
            Box::new(a_machine()),
        );
        let session = pool.session_mut(id).expect("the instance");
        let before = session.now();
        assert_eq!(
            ui_safe_point(session, pemu_core::time::VTime::from_ms(5)),
            Ok(None),
            "no ELF: this build cannot tell, and does not run the guest"
        );
        assert_eq!(session.now(), before, "nothing ran");
    }
}
