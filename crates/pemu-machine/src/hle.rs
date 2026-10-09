//! The `pemu-hle` core bound into a machine: binding, hook exit dispatch, the radio MMIO watch
//! and the `hle.machine` state.
//!
//! Binding runs at composition, so `new`, `fork` and a restore derive the same `HookSet`; it is
//! never serialized. Beside the modules' hooks it arms a tripwire on the driver init of each radio
//! no module binds (so `pk` stops instead of asserting inside the controller) and on every even
//! pc of the magic range outside the five allocations. An ELF-less image is also guarded by the
//! `Rwip`, `RadioMmio` and `ResetLoop` tripwires.

use std::collections::BTreeMap;

use pemu_core::input::{EnvChange, InputEvent};
use pemu_core::irq_source::IrqSource;
use pemu_core::journal::Origin;
use pemu_core::sched::{EventHandle, EventKey, Scheduler};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::snap::SnapError;
use pemu_core::time::VTime;
use pemu_hle::binding::{
    BindingMismatch, BoundHooks, FeatureStatus, ImageView, LoadedSegment, MismatchField, bind_all,
};
use pemu_hle::continuation::{HandlerState, HleSection, Resume};
use pemu_hle::core::{
    CallInfo, HandlerHost, HciInput, HleCore, ModuleHost, NetInput, Step, check_worker,
    worker_handler,
};
use pemu_hle::guest_call::{
    CallEngine, GuardProfile, GuestView, HleAction, HleError, HleErrorKind, StackSymbols,
};
use pemu_hle::hooks::{HandlerKind, HookId, HookKind, HookRef, ModuleIndex};
use pemu_hle::image_symbols::ModuleCheck;
use pemu_hle::magic::{MagicKind, MagicPcs};
use pemu_hle::observe::{ObserveKind, UserHook};
use pemu_hle::tripwire::{RadioMmioWatch, TripKind};
use pemu_hle::worker::RadioEvent;
use pemu_loader::elf::ElfInfo;
use pemu_loader::symbols::SymbolTable;
use pemu_rv32::exec::Hart;
use pemu_rv32::trap::Trap;
use pemu_soc_c3::intc::IrqFabric;
use pemu_soc_c3::{Soc, Stored};

use crate::config::{Assets, HleConfig};

use crate::machine::{At, Machine};
use crate::stops::{PanicCapture, StopReason, TripReport};
pub use pemu_hle::binding::BindingMismatch as HleBindingMismatch;
pub use pemu_hle::binding::BindingRecord as HleBindingRecord;
/// The HLE types a machine's reports carry, re-exported so a caller needs no `pemu-hle` edge.
pub use pemu_hle::binding::FeatureStatus as HleFeatureStatus;
pub use pemu_hle::binding::HeapBlock as HleHeapBlock;
pub use pemu_hle::binding::MismatchField as HleMismatchField;
pub use pemu_hle::binding::RadioLogLines as HleRadioLogLines;
pub use pemu_hle::core::HciInput as HleHciInput;
pub use pemu_hle::core::NetInput as HleNetInput;
pub use pemu_hle::guest_call::HleError as HleErrorReport;
pub use pemu_hle::magic::MagicKind as HleMagicKind;
pub use pemu_hle::observe::ObserveKind as HleObserveKind;
pub use pemu_hle::tripwire::TripKind as HleTripKind;
pub use pemu_hle::worker::RadioEvent as HleRadioEvent;
pub use pemu_hle::worker::WakeMode as HleWakeMode;
pub use pemu_hle::worker::WorkerState as HleWorkerState;

/// The radio features whose driver init is a `DisabledFeature` tripwire while no module binds
/// them, keyed by `RadioModule::name`. A bound hook on the same pc wins over the tripwire.
pub const FEATURE_ENTRIES: [(&str, &str); 2] =
    [("ble", "esp_bt_controller_init"), ("wifi", "esp_wifi_init")];

/// The id of a user observe hook: a `Breakpoint` kind under the last module index, which
/// `bind_all` never hands out, so it is distinct from every bound hook and `BREAKPOINT_HOOK`.
pub fn user_observe_hook_id() -> HookId {
    HookRef {
        kind: HookKind::Breakpoint,
        module: ModuleIndex(u8::MAX),
    }
    .to_id()
}

pub(crate) struct MachineHle {
    pub(crate) core: HleCore,
    pub(crate) radio: RadioMmioWatch,
    /// Disabled-feature tripwires: pc to feature name.
    pub(crate) features: BTreeMap<u32, &'static str>,
    pub(crate) state: HleMachineSection,
    /// `false` for an ELF-less image.
    pub(crate) has_elf: bool,
    /// The `idf_ver` binding read, from the app ELF or the image's descriptor. Kept from the
    /// bind: a receipt is drawn per command, and reading the descriptor again parses and hashes
    /// the whole boot app.
    pub(crate) idf_ver: Option<String>,
    /// Per bound module of an image without an ELF, the hooks it bound without: not found in the
    /// image, each left to its guard's tripwire. Empty with an ELF, which says what is linked.
    pub(crate) guarded: Vec<(String, Vec<String>)>,
    /// The handler host of every bound module that has handlers; their state is `state.modules`.
    pub(crate) hosts: Vec<Box<dyn ModuleHost>>,
}

/// The `hle.machine` section: what this file adds to `HleSection`. All guest state and part of
/// `state_hash`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct HleMachineSection {
    /// Radio MMIO boot accesses spent since the last reset, filled when the section is written.
    pub radio_used: u32,
    /// The last `abort` or `__assert_func` entry: `ObserveKind` encoding index and `a0` to `a3`.
    pub first_capture: Option<(u8, [u32; 4])>,
    /// User observe hooks that fired: pc to (fires, instruction count of the first fire).
    pub fires: BTreeMap<u32, (u64, u64)>,
    /// Core observe hooks that fired, by `ObserveKind` encoding index.
    pub observed: [u64; 4],
    /// The pc whose hook already ran in the `esp_panic_handler` stop that left the resume marker
    /// there. A breakpoint's resume marker has not run its hook, so the resume dispatches it.
    pub hook_ran_at: Option<u32>,
    /// The pc whose hook the next slice runs past exactly once. Guest state, so a machine saved
    /// between the hook exit and that slice neither dispatches the hook again nor skips it.
    pub run_past_at: Option<u32>,
    /// The reset-loop stop of an ELF-less image.
    pub reset_loop: ResetLoop,
    /// A radio MMIO access that tripped and is not yet reported. Guest state, so a machine saved
    /// before the report still reports it after a restore.
    pub radio_trip: Option<PendingRadioTrip>,
    /// Each bound module's handler state by `RadioModule::name`, in the module's own encoding. A
    /// reset clears it: a new boot starts with the controller idle.
    pub modules: BTreeMap<String, Vec<u8>>,
}

/// A radio MMIO access past the boot allowance, waiting for the run loop.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct PendingRadioTrip {
    pub pc: u32,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct ResetLoop {
    pub text: Option<String>,
    pub repeats: u32,
    pub cursors: [u64; 2],
    /// The raised stop not yet reported, with the pc the reset left; guest state like `radio_trip`.
    pub pending_pc: Option<u32>,
}

/// Consecutive identical panics that stop an ELF-less image.
pub const RESET_LOOP_REPEATS: u32 = 3;

/// The line prefixes an ESP-IDF panic prints (`esp_system` panic output and `assert.c`); the last
/// such line of a boot is its panic text.
const PANIC_MARKERS: [&str; 4] = [
    "assert failed:",
    "abort() was called",
    "Guru Meditation Error",
    "***ERROR***",
];

pub const HLE_MACHINE: &str = "hle.machine";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HookAction {
    RunPast,
    Moved,
    Stop(StopReason),
}

/// The loaded segments of the boot app `app` of `flash`: the bytes the guest executes.
pub(crate) fn app_segments<'a>(
    flash: &'a [u8],
    app: &pemu_loader::esp_image::EspImage,
) -> Vec<LoadedSegment<'a>> {
    (0..app.segments.len())
        .filter_map(|i| {
            Some(LoadedSegment {
                addr: app.segments[i].load_addr,
                data: app.segment_data(flash, i)?,
            })
        })
        .collect()
}

/// An empty app ELF, for an image with no ELF and no boot app to recover symbols from.
fn no_elf() -> ElfInfo {
    ElfInfo {
        sha256: [0; 32],
        entry: 0,
        sections: Vec::new(),
        segments: Vec::new(),
        symbols: SymbolTable::new(Vec::new()),
        app_desc: None,
    }
}

impl MachineHle {
    /// Binds the loaded images; modules `cfg.disabled` names are recorded `disabled`. The code-byte
    /// checks read the boot app's segments from flash, since `Assets` keeps only the parsed ELF.
    pub(crate) fn bind(
        assets: &Assets,
        cfg: &HleConfig,
        timing: &pemu_core::clock::TimingProfile,
    ) -> MachineHle {
        let empty = no_elf();
        let recovered = assets.recovered_symbols();
        let elf: &ElfInfo = assets.binding_elf().unwrap_or(&empty);
        let mut refused = Vec::new();
        let modules: Vec<_> = pemu_radio::modules()
            .into_iter()
            .filter(|m| !cfg.disabled.iter().any(|name| name == m.name()))
            // Without an ELF, a module binds only when everything it reads was recovered, or
            // what was not is a hook whose guard was; any other partial recovery is refused
            // here, fail closed.
            .filter(|m| match recovered.map(|r| r.check(&m.image_symbols())) {
                Some(ModuleCheck::Refuse(why)) => {
                    refused.push((m.name(), why));
                    false
                }
                _ => true,
            })
            .collect();
        let flash = assets.flash.bytes();
        let segments: Vec<LoadedSegment<'_>> = pemu_loader::esp_image::MergedImage::parse(flash)
            .ok()
            .and_then(|merged| merged.app)
            .map(|(_, app)| app_segments(flash, &app))
            .unwrap_or_default();
        let image = ImageView::symbols_only(elf)
            .with_rom(assets.rom.symbols())
            .with_segments(&segments);
        let mut bound = bind_all(&modules, &image);
        for (name, why) in refused {
            bound
                .record
                .features
                .insert(name.to_string(), FeatureStatus::UnsupportedImage);
            bound.mismatches.push((name, why));
        }
        let mut hosts: Vec<Box<dyn ModuleHost>> = modules
            .iter()
            .filter(|m| bound.record.features.get(m.name()) == Some(&FeatureStatus::Bound))
            .filter_map(|m| m.host(&image))
            .collect();
        // A worker the core would refuse refuses its whole module.
        let mut workers = Vec::new();
        let us = |ps: u64| u32::try_from(ps / 1_000_000).unwrap_or(u32::MAX);
        for host in &mut hosts {
            host.set_busy_delays(pemu_hle::core::BusyDelays {
                init_us: us(timing.ble_init_ps),
                enable_us: us(timing.ble_enable_ps),
                enable_nvs_cal_us: us(timing.ble_enable_nvs_cal_ps),
                disable_us: us(timing.ble_disable_ps),
                deinit_us: us(timing.ble_deinit_ps),
            });
        }
        hosts.retain_mut(|host| {
            let configs = host.workers(cfg.wake);
            match configs
                .iter()
                .find_map(|c| check_worker(c).err().map(|e| (c, e)))
            {
                None => {
                    workers.extend(configs);
                    true
                }
                Some((config, err)) => {
                    refuse_module(
                        &mut bound,
                        host.module(),
                        host.name(),
                        BindingMismatch {
                            symbol: config.profile.task_name.to_string(),
                            field: MismatchField::Worker,
                            expected: "a worker the HLE core registers".to_string(),
                            found: err.detail,
                        },
                    );
                    false
                }
            }
        });
        let guarded: Vec<(String, Vec<String>)> = modules
            .iter()
            .filter(|_| recovered.is_some())
            .filter(|m| bound.record.features.get(m.name()) == Some(&FeatureStatus::Bound))
            .map(|m| {
                let absent = m
                    .image_symbols()
                    .guards
                    .iter()
                    .filter(|(hook, _)| elf.symbols.lookup(hook).is_none())
                    .map(|(hook, _)| hook.to_string())
                    .collect::<Vec<_>>();
                (m.name().to_string(), absent)
            })
            .filter(|(_, absent)| !absent.is_empty())
            .collect();
        let features = arm_disabled_features(&mut bound, elf);
        let pcs = MagicPcs::from_spec()
            .expect("specs/magic-pcs.toml names all five allocations (pemu-loader proves it)");
        arm_magic_range(&mut bound, &pcs);
        let engine = CallEngine {
            pcs,
            guards: GuardProfile::default(),
            stack: StackSymbols {
                isr_stack_bottom: elf.symbols.addr_of("xIsrStackBottom"),
                isr_stack_top: elf.symbols.addr_of("xIsrStackTop"),
            },
        };
        let mut core = HleCore::new(engine, bound);
        for worker in workers {
            core.add_worker(worker)
                .expect("check_worker accepted every registered worker above");
        }
        MachineHle {
            core,
            radio: RadioMmioWatch::new(),
            features,
            state: HleMachineSection::default(),
            has_elf: assets.app_elf.is_some(),
            idf_ver: elf.app_desc.as_ref().map(|desc| desc.idf_ver.clone()),
            guarded,
            hosts,
        }
    }

    pub(crate) fn bound_hooks(&self) -> &BoundHooks {
        &self.core.bound
    }

    /// A reset starts a new boot, whose `rtc_init` spends the radio allowance again.
    pub(crate) fn on_reset(&mut self) {
        self.radio = RadioMmioWatch::new();
        self.state.radio_used = 0;
        // The previous boot's continuations and generations name stack frames and TCBs the reset
        // destroyed; kept, a reused stack pointer would be a `KeyCollision`.
        self.core.on_chip_reset();
        // An abort of the previous boot is not the cause of a panic in this one.
        self.state.first_capture = None;
        self.state.hook_ran_at = None;
        self.state.run_past_at = None;
        self.state.modules.clear();
    }

    pub(crate) fn restore_radio_used(&mut self, used: u32) {
        self.radio = RadioMmioWatch::new();
        let Some(&addr) = self.radio.spec().boot_addresses.first() else {
            return;
        };
        // `RadioMmioWatch` counts only through `access`, so the allowance is spent as the guest
        // spent it; a restored count is never above the total.
        for _ in 0..used.min(self.radio.spec().boot_max_accesses) {
            let _ = self.radio.access(addr);
        }
        self.state.radio_used = self.radio.used();
    }
}

/// Refuses a module that bound but cannot run: its hooks leave the merged set and its feature is
/// `unsupported image`, with `mismatch` saying why.
fn refuse_module(
    bound: &mut BoundHooks,
    module: ModuleIndex,
    name: &'static str,
    mismatch: BindingMismatch,
) {
    let pcs: Vec<u32> = bound
        .set
        .iter()
        .filter(|(_, id)| HookRef::from_id(*id).is_some_and(|r| r.module == module))
        .map(|(pc, _)| pc)
        .collect();
    for pc in pcs {
        bound.set.remove(pc);
    }
    bound
        .record
        .features
        .insert(name.to_string(), FeatureStatus::UnsupportedImage);
    bound.mismatches.push((name, vec![mismatch]));
}

fn arm_disabled_features(bound: &mut BoundHooks, elf: &ElfInfo) -> BTreeMap<u32, &'static str> {
    let mut out = BTreeMap::new();
    for (feature, entry) in FEATURE_ENTRIES {
        if bound.record.features.get(feature) == Some(&FeatureStatus::Bound) {
            continue;
        }
        let Some(pc) = elf.symbols.addr_of(entry) else {
            continue;
        };
        let id = HookRef::core(HookKind::Tripwire(TripKind::DisabledFeature)).to_id();
        match bound.set.get(pc).and_then(HookRef::from_id) {
            // A hook a module bound serves the entry and replaces a core tripwire there.
            Some(HookRef {
                kind: HookKind::Tripwire(_),
                module: ModuleIndex::CORE,
            })
            | None => {}
            Some(_) => continue,
        }
        bound.set.insert(pc, id);
        bound.tripwires.arm(
            pc,
            TripKind::DisabledFeature,
            &format!("{entry} ({feature} disabled)"),
        );
        bound
            .record
            .features
            .entry(feature.to_string())
            .or_insert(FeatureStatus::Disabled);
        out.insert(pc, feature);
    }
    out
}

fn arm_magic_range(bound: &mut BoundHooks, pcs: &MagicPcs) {
    let Some((lo, hi)) = MagicPcs::range() else {
        return;
    };
    let id = HookRef::core(HookKind::Tripwire(TripKind::MagicRangeFetch)).to_id();
    for pc in (lo..=hi).step_by(2) {
        if pcs.kind_at(pc).is_none() && bound.set.get(pc).is_none() {
            bound.set.insert(pc, id);
        }
    }
}

/// The guest as the HLE core sees it, over one machine's hart, SoC, interrupt fabric and
/// scheduler.
pub(crate) struct MachineGuest<'a> {
    pub(crate) hart: &'a mut Hart,
    pub(crate) soc: &'a mut Soc,
    pub(crate) irq: &'a mut IrqFabric,
    pub(crate) sched: &'a mut Scheduler,
    pub(crate) rng: &'a mut pemu_core::rng::DetRng,
    pub(crate) now: VTime,
    pub(crate) symbols: Option<&'a SymbolTable>,
}

impl MachineGuest<'_> {
    fn word_at(&self, name: &str) -> u32 {
        self.symbols
            .and_then(|s| s.addr_of(name))
            .and_then(|addr| self.soc.load_mem(addr, 4))
            .unwrap_or(0)
    }
}

impl GuestView for MachineGuest<'_> {
    fn draw_entropy(&mut self, stream: pemu_core::rng::RngStream, out: &mut [u8]) {
        self.rng.stream(stream).fill_bytes(out);
    }

    fn reg(&self, r: u8) -> u32 {
        self.hart.x.get(usize::from(r)).copied().unwrap_or(0)
    }

    fn set_reg(&mut self, r: u8, v: u32) {
        if r != 0
            && let Some(slot) = self.hart.x.get_mut(usize::from(r))
        {
            *slot = v;
        }
    }

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Trap> {
        for (i, byte) in buf.iter_mut().enumerate() {
            let at = addr.wrapping_add(i as u32);
            *byte = self
                .soc
                .load_mem(at, 1)
                .ok_or(Trap::load_access_fault(at))? as u8;
        }
        Ok(())
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), Trap> {
        for (i, byte) in data.iter().enumerate() {
            let at = addr.wrapping_add(i as u32);
            match self.soc.store_mem(at, 1, u32::from(*byte)) {
                Stored::Wrote { .. } => {}
                _ => return Err(Trap::store_access_fault(at)),
            }
        }
        Ok(())
    }

    fn raise(&mut self, s: IrqSource, level: bool) {
        self.irq.set_source(s, level);
    }

    fn schedule(&mut self, at: VTime, key: EventKey) -> EventHandle {
        self.sched.schedule(self.now, at, key)
    }

    fn now(&self) -> VTime {
        self.now
    }

    fn symbol(&self, name: &str) -> Option<u32> {
        self.symbols?.addr_of(name)
    }

    fn current_task(&mut self) -> u32 {
        // The per-core array of the IDF FreeRTOS kernel; the C3 has one core.
        self.word_at("pxCurrentTCBs")
    }

    fn in_isr(&mut self) -> bool {
        self.word_at("port_uxInterruptNesting") != 0
    }

    fn scheduler_running(&mut self) -> bool {
        self.word_at("xSchedulerRunning") != 0
    }

    /// The guest's FreeRTOS tick rate, from SYSTIMER comparator 0's period (`vSystimerSetup` sets
    /// it to `1000000 / CONFIG_FREERTOS_HZ` us at 16 ticks per us). `None` before the port set it.
    fn tick_hz(&mut self) -> Option<u32> {
        const COUNTER_HZ: u64 = 1_000_000_000_000 / pemu_core::clock::SYSTIMER_TICK_PS;
        let period = self.soc.devices.systimer.alarm_period(0)?;
        u32::try_from(COUNTER_HZ / u64::from(period)).ok()
    }
}

/// Routes each core call of one hook exit to the bound module that owns it, with its state bytes.
/// Anything no bound module owns fails with `E_HLE` naming the handler.
struct ModuleHosts<'a> {
    hosts: &'a mut [Box<dyn ModuleHost>],
    states: &'a mut BTreeMap<String, Vec<u8>>,
    module: ModuleIndex,
}

impl ModuleHosts<'_> {
    fn by_magic(&self, kind: HandlerKind) -> Option<usize> {
        let magic = MagicKind::ALL
            .into_iter()
            .find(|m| worker_handler(*m) == kind)?;
        self.hosts
            .iter()
            .position(|h| h.magic_entries().contains(&magic))
    }

    fn by_name(&self, handler: &str) -> Option<usize> {
        self.hosts.iter().position(|h| {
            handler
                .strip_prefix(h.name())
                .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    fn fail(what: String) -> HleAction {
        HleAction::Fail(HleError::new(HleErrorKind::Handler, what))
    }
}

impl HandlerHost for ModuleHosts<'_> {
    fn enter(&mut self, kind: HandlerKind, g: &mut dyn GuestView) -> (HandlerState, HleAction) {
        let found = self
            .by_magic(kind)
            .or_else(|| self.hosts.iter().position(|h| h.module() == self.module));
        let Some(i) = found else {
            return (
                HandlerState::default(),
                ModuleHosts::fail(format!("handler {} has no registered radio module", kind.0)),
            );
        };
        let host = &mut self.hosts[i];
        let state = self.states.entry(host.name().to_string()).or_default();
        host.enter(state, kind, g)
    }

    fn resume(
        &mut self,
        state: &mut HandlerState,
        g: &mut dyn GuestView,
        resume: Resume,
    ) -> HleAction {
        let Some(i) = self.by_name(&state.handler) else {
            return ModuleHosts::fail(format!(
                "handler `{}` has no registered radio module",
                state.handler
            ));
        };
        let host = &mut self.hosts[i];
        let bytes = self.states.entry(host.name().to_string()).or_default();
        host.resume(bytes, state, g, resume)
    }

    fn describe(&self, func: u32) -> CallInfo {
        self.hosts
            .iter()
            .find_map(|h| h.describe(func))
            .unwrap_or(CallInfo {
                name: "nested call",
                blocking: true,
                _func: func,
            })
    }

    fn takes_events(&self) -> bool {
        true
    }

    fn deliver(&mut self, entry: MagicKind, events: Vec<RadioEvent>) {
        if let Some(host) = self
            .hosts
            .iter_mut()
            .find(|h| h.magic_entries().contains(&entry))
        {
            let state = self.states.entry(host.name().to_string()).or_default();
            host.deliver(state, entry, events);
        }
    }
}

impl Machine {
    /// Called by every reset: for an ELF-less image, the third consecutive boot ending on the same
    /// panic line raises `Tripwire(ResetLoop)`. A boot with no panic line resets the count.
    pub(crate) fn note_reset_panic(&mut self, pc: u32) {
        if self.hle.has_elf {
            return;
        }
        let mut text = None;
        let mut cursors = [0u64; 2];
        for (i, stream) in pemu_core::hostio::SerialStream::ALL.iter().enumerate() {
            let ring = self.io.serial_ring(*stream);
            let from = self.hle.state.reset_loop.cursors[i];
            let bytes: Vec<u8> = ring.slices(from).iter().copied().collect();
            cursors[i] = ring.head();
            let lossy = String::from_utf8_lossy(&bytes);
            if let Some(line) = lossy
                .lines()
                .map(|l| l.trim_end_matches('\r').trim())
                .rfind(|l| PANIC_MARKERS.iter().any(|m| l.starts_with(m)))
            {
                text = Some(line.to_string());
            }
        }
        let state = &mut self.hle.state.reset_loop;
        state.cursors = cursors;
        match text {
            Some(line) if state.text.as_deref() == Some(line.as_str()) => state.repeats += 1,
            Some(line) => {
                state.text = Some(line);
                state.repeats = 1;
            }
            None => {
                state.text = None;
                state.repeats = 0;
            }
        }
        if state.repeats >= RESET_LOOP_REPEATS {
            state.pending_pc = Some(pc);
        }
    }

    /// Tripwire stops raised outside a hook exit and not yet reported. Asked before the hang
    /// check, so a `Stuck` in the same slice does not hide them.
    pub(crate) fn take_pending_trip(&mut self) -> Option<StopReason> {
        if let Some(trip) = self.hle.state.radio_trip.take() {
            return Some(StopReason::Tripwire(TripReport {
                kind: TripKind::RadioMmio,
                pc: trip.pc,
                detail: trip.detail,
                caller: self.hart.x[1],
                feature: None,
            }));
        }
        self.take_reset_loop_trip().map(StopReason::Tripwire)
    }

    pub(crate) fn take_reset_loop_trip(&mut self) -> Option<TripReport> {
        let state = &mut self.hle.state.reset_loop;
        let pc = state.pending_pc.take()?;
        Some(TripReport {
            kind: TripKind::ResetLoop,
            pc,
            detail: format!(
                "{} consecutive resets with identical panic text: {}",
                state.repeats,
                state.text.as_deref().unwrap_or_default()
            ),
            caller: 0,
            feature: None,
        })
    }
}

fn args(hart: &Hart) -> [u32; 4] {
    [hart.x[10], hart.x[11], hart.x[12], hart.x[13]]
}

impl Machine {
    /// Dispatches a hook exit to `HleCore::on_hook`. A user observe hook counts a fire only when
    /// its instruction runs, so a run stopping at one tripwire repeatedly does not inflate it.
    pub(crate) fn on_hle_hook(&mut self, id: HookId, pc: u32) -> HookAction {
        let action = self.dispatch_hle_hook(id, pc);
        if action == HookAction::RunPast {
            self.count_user_fire(pc);
        }
        action
    }

    pub(crate) fn count_user_fire(&mut self, pc: u32) {
        if self.hle.core.section.user_hooks.contains_key(&pc) {
            let insns = self.hart.insns;
            let fire = self.hle.state.fires.entry(pc).or_insert((0, insns));
            fire.0 += 1;
        }
    }

    fn dispatch_hle_hook(&mut self, id: HookId, pc: u32) -> HookAction {
        if id == user_observe_hook_id() {
            return HookAction::RunPast;
        }
        let Some(hook) = HookRef::from_id(id) else {
            // A hook no binding produced (a test's) has no handler, so the instruction runs.
            return HookAction::RunPast;
        };
        let now = self.now();
        let Machine {
            hart,
            soc,
            irq,
            sched,
            assets,
            hle,
            rng,
            ..
        } = self;
        let mut guest = MachineGuest {
            hart,
            soc,
            irq,
            sched,
            rng,
            now,
            symbols: assets.binding_elf().map(|elf| &elf.symbols),
        };
        let crate::hle::MachineHle {
            core, hosts, state, ..
        } = hle;
        let mut host = ModuleHosts {
            hosts,
            states: &mut state.modules,
            module: hook.module,
        };
        let step = core.on_hook(&mut guest, &mut host, pc, hook);
        match step {
            Err(err) => HookAction::Stop(StopReason::Hle(err)),
            Ok(Step::Resume { pc: to }) | Ok(Step::Returned { pc: to, .. }) => {
                self.hart.pc = to;
                self.poll.invalidate();
                HookAction::Moved
            }
            Ok(Step::Parked) => HookAction::Stop(StopReason::Hle(HleError::new(
                HleErrorKind::Handler,
                format!("a handler parked at {pc:#010x} with no worker to wake it"),
            ))),
            Ok(Step::Observed(obs)) => {
                let index = ObserveKind::ALL
                    .iter()
                    .position(|k| *k == obs.kind)
                    .unwrap_or(0);
                self.hle.state.observed[index] += 1;
                match obs.kind {
                    ObserveKind::Abort | ObserveKind::AssertFunc => {
                        self.hle.state.first_capture = Some((index as u8, args(&self.hart)));
                        HookAction::RunPast
                    }
                    ObserveKind::TaskDelete => HookAction::RunPast,
                    // Stop before the IDF reboots, state intact; the next run executes the handler.
                    ObserveKind::PanicHandler => {
                        self.resume_breakpoint = Some(pc);
                        self.hle.state.hook_ran_at = Some(pc);
                        let first = self.hle.state.first_capture.map(|(k, a)| {
                            (ObserveKind::ALL[usize::from(k) % ObserveKind::ALL.len()], a)
                        });
                        HookAction::Stop(StopReason::GuestPanic(PanicCapture {
                            pc,
                            args: args(&self.hart),
                            first,
                        }))
                    }
                }
            }
            Ok(Step::Tripped { kind, detail }) => {
                HookAction::Stop(StopReason::Tripwire(TripReport {
                    kind,
                    pc,
                    detail,
                    caller: self.hart.x[1],
                    feature: self.hle.features.get(&pc).copied(),
                }))
            }
        }
    }

    /// A module timer came due: posts each event the module queued for now and returns how many.
    /// A timer of an unbound module, or of one with no state entry, is dropped.
    pub(crate) fn on_radio_timer(&mut self, radio: pemu_core::sched::RadioId, tag: u16) -> usize {
        let now = self.now();
        let Machine {
            hart,
            soc,
            irq,
            sched,
            assets,
            hle,
            rng,
            ..
        } = self;
        let crate::hle::MachineHle {
            core, hosts, state, ..
        } = hle;
        let Some(host) = hosts
            .iter_mut()
            .find(|h| u16::from(h.module().0) == radio.0)
        else {
            return 0;
        };
        let Some(entry) = host.magic_entries().into_iter().find(|k| k.is_worker()) else {
            return 0;
        };
        let Some(module_state) = state.modules.get_mut(host.name()) else {
            return 0;
        };
        let mut guest = MachineGuest {
            hart,
            soc,
            irq,
            sched,
            rng,
            now,
            symbols: assets.binding_elf().map(|elf| &elf.symbols),
        };
        let events = host.on_timer(module_state, tag, &mut guest);
        let mut posted = 0;
        for event in events {
            if core.post(&mut guest, entry, event).is_ok() {
                posted += 1;
            }
        }
        self.poll.invalidate();
        posted
    }

    /// A journaled input for module `module` came due. Returns false, leaving the state as it
    /// was, when no bound module has that name or the module refused it.
    pub(crate) fn on_radio_input(&mut self, module: &str, payload: &[u8]) -> bool {
        self.with_radio_module(module, |host, state, guest| {
            host.on_input(state, payload, guest)
        })
    }

    /// A journaled external-HCI event, always for the `ble` module.
    pub(crate) fn on_radio_hci(&mut self, ev: HciInput<'_>) -> bool {
        self.with_radio_module("ble", |host, state, guest| host.on_hci(state, ev, guest))
    }

    /// A journaled Wi-Fi bridge event, always for the `wifi` module.
    pub(crate) fn on_radio_net(&mut self, ev: NetInput<'_>) -> bool {
        self.with_radio_module("wifi", |host, state, guest| host.on_net(state, ev, guest))
    }

    /// The one place a journaled input becomes module work.
    fn with_radio_module(
        &mut self,
        module: &str,
        f: impl FnOnce(
            &mut dyn ModuleHost,
            &mut Vec<u8>,
            &mut dyn pemu_hle::guest_call::GuestView,
        ) -> Result<Vec<RadioEvent>, HleError>,
    ) -> bool {
        let now = self.now();
        let Machine {
            hart,
            soc,
            irq,
            sched,
            assets,
            hle,
            rng,
            ..
        } = self;
        let crate::hle::MachineHle {
            core, hosts, state, ..
        } = hle;
        let Some(host) = hosts.iter_mut().find(|h| h.name() == module) else {
            return false;
        };
        let mut module_state = state.modules.get(module).cloned().unwrap_or_default();
        let mut guest = MachineGuest {
            hart,
            soc,
            irq,
            sched,
            rng,
            now,
            symbols: assets.binding_elf().map(|elf| &elf.symbols),
        };
        let Ok(events) = f(host.as_mut(), &mut module_state, &mut guest) else {
            return false;
        };
        state.modules.insert(module.to_string(), module_state);
        if let Some(entry) = host.magic_entries().into_iter().find(|k| k.is_worker()) {
            for event in events {
                let _ = core.post(&mut guest, entry, event);
            }
        }
        self.poll.invalidate();
        true
    }

    /// Guest-heap blocks the bound radio modules hold for the blobs they replaced.
    pub fn heap_ledger(&self) -> Vec<HleHeapBlock> {
        self.hle
            .hosts
            .iter()
            .flat_map(|host| {
                let state = self
                    .hle
                    .state
                    .modules
                    .get(host.name())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                host.heap_ledger(state)
            })
            .collect()
    }

    /// Live host bridges the bound radio modules hold open. Read from module state, so a restored
    /// machine knows its bridge was up.
    pub fn live_bridges(&self) -> u32 {
        self.hle
            .hosts
            .iter()
            .map(|host| {
                let state = self
                    .hle
                    .state
                    .modules
                    .get(host.name())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                host.bridges_live(state)
            })
            .sum()
    }

    fn bridged_modules(&self) -> Vec<&'static str> {
        self.hle
            .hosts
            .iter()
            .filter(|host| {
                let state = self.hle.state.modules.get(host.name());
                host.bridges_live(state.map(Vec::as_slice).unwrap_or_default()) > 0
            })
            .map(|host| host.name())
            .collect()
    }

    /// `Err(SnapError::LiveBridge)` naming the first live bridge: its peer is in no snapshot.
    pub(crate) fn refuse_live_bridge(&self) -> Result<(), SnapError> {
        let Some(module) = self.bridged_modules().first().copied() else {
            return Ok(());
        };
        let bridge = match module {
            "ble" => "the external HCI bridge of the `ble` module".to_string(),
            "wifi" => "the LAN bridge of the `wifi` module".to_string(),
            other => format!("the bridge of the `{other}` module"),
        };
        Err(SnapError::LiveBridge { bridge })
    }

    /// Journals and applies the detach of every live bridge now: a TCP RST per bridged socket
    /// (`wifi`) or the hand-back to the virtual controller (`ble`).
    pub(crate) fn link_down(&mut self) {
        for module in self.bridged_modules() {
            let change = match module {
                "ble" => EnvChange::BleHciBridge { attached: false },
                "wifi" => EnvChange::WifiBridge {
                    attached: false,
                    routes: Vec::new(),
                },
                _ => continue,
            };
            let journaled = self.input_from(At::Now, Origin::Bridge, InputEvent::Env(change));
            debug_assert!(journaled.is_ok());
        }
        self.apply_due_journal();
    }

    /// The radio worker entered at `entry`: its task, semaphore and wake engine.
    pub fn radio_worker(&self, entry: MagicKind) -> Option<&HleWorkerState> {
        self.hle.core.worker(entry)
    }

    pub fn irq_source_level(&self, s: IrqSource) -> bool {
        self.irq.source(s)
    }

    pub fn radio_module_state(&self, module: &str) -> Option<&[u8]> {
        self.hle.state.modules.get(module).map(Vec::as_slice)
    }

    /// Adds a user observe hook at `pc` that counts arrivals and lets the instruction run. Returns
    /// `false` when one is already there.
    pub fn add_observe_hook(&mut self, pc: u32, label: &str) -> bool {
        if self.hle.core.section.user_hooks.contains_key(&pc) {
            return false;
        }
        self.hle.core.section.user_hooks.insert(
            pc,
            UserHook {
                breakpoint: false,
                label: label.to_string(),
            },
        );
        self.install_user_hooks();
        true
    }

    pub(crate) fn rebuild_hooks(&mut self) {
        let delay = crate::rom_delay::delay_hook_id();
        let kept: Vec<(u32, HookId)> = self
            .hooks
            .iter()
            .filter(|(_, id)| *id == crate::run::BREAKPOINT_HOOK || *id == delay)
            .collect();
        let mut hooks = self.hle.core.bound.set.clone();
        for (pc, id) in kept {
            if hooks.get(pc).is_none() {
                hooks.insert(pc, id);
            }
        }
        self.hooks = hooks;
        self.install_user_hooks();
    }

    pub fn observe_fires(&self, pc: u32) -> Option<(u64, u64)> {
        self.hle.state.fires.get(&pc).copied()
    }

    /// Installs the user hooks on every pc no bound hook holds, taking over the breakpoint hook's
    /// pcs: the run loop stops on any hook exit at an armed breakpoint pc.
    pub(crate) fn install_user_hooks(&mut self) {
        let pcs: Vec<u32> = self.hle.core.section.user_hooks.keys().copied().collect();
        for pc in pcs {
            match self.hooks.get(pc) {
                None => {}
                // The ROM delay shortcut stands aside while a hook sits on the loop.
                Some(id)
                    if id == crate::run::BREAKPOINT_HOOK
                        || id == crate::rom_delay::delay_hook_id() =>
                {
                    self.hooks.remove(pc);
                }
                Some(_) => continue,
            }
            self.hooks.insert(pc, user_observe_hook_id());
        }
    }

    /// The binding: per-feature status, tripwire count and mismatches.
    /// Per bound module of an image without an ELF, the hooks it was bound without, as the
    /// receipt lists them (`binding.guarded`).
    pub fn guarded_hooks(&self) -> &[(String, Vec<String>)] {
        &self.hle.guarded
    }

    pub fn hle_binding(&self) -> &BoundHooks {
        &self.hle.core.bound
    }

    pub fn tripwires(&self) -> &pemu_hle::tripwire::TripwireSet {
        &self.hle.core.tripwires
    }

    pub fn hle_section(&self) -> &HleSection {
        &self.hle.core.section
    }

    pub fn hle_state(&self) -> HleMachineSection {
        HleMachineSection {
            radio_used: self.hle.radio.used(),
            ..self.hle.state.clone()
        }
    }

    pub fn radio_accesses(&self) -> u32 {
        self.hle.radio.used()
    }

    pub fn magic_range_hooks(&self) -> usize {
        let id = HookRef::core(HookKind::Tripwire(TripKind::MagicRangeFetch)).to_id();
        self.hooks.iter().filter(|(_, h)| *h == id).count()
    }

    pub fn magic_pc(&self, kind: MagicKind) -> u32 {
        self.hle.core.engine.pcs.pc_of(kind)
    }
}

/// HLE dispatch over the bundled ROM with hand-assembled code and a synthetic symbol table.
#[cfg(all(test, feature = "bundled-rom"))]
mod tests {
    use std::sync::Arc;

    use pemu_core::snap::SnapOpts;
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_loader::symbols::{SymBind, SymKind, SymSection, Symbol};

    use super::*;
    use crate::config::MachineConfig;
    use crate::executor::Executor;
    use crate::run::RunLimits;

    const PROG: u32 = pemu_soc_c3::mem::SRAM1_IRAM_BASE;
    /// `addi a0, a0, 1` four times, then `j .`.
    const COUNT_FOUR: [u32; 5] = [
        0x0015_0513,
        0x0015_0513,
        0x0015_0513,
        0x0015_0513,
        0x0000_006F,
    ];
    const BOTH: [Executor; 2] = [Executor::Engine, Executor::Reference];

    fn elf(sha: u8, syms: &[(&str, u32)]) -> Arc<ElfInfo> {
        Arc::new(ElfInfo {
            sha256: [sha; 32],
            symbols: SymbolTable::new(
                syms.iter()
                    .map(|(name, addr)| Symbol {
                        name: (*name).to_string(),
                        addr: *addr,
                        size: 4,
                        kind: SymKind::Func,
                        bind: SymBind::Global,
                        section: SymSection::Index(1),
                    })
                    .collect(),
            ),
            ..no_elf()
        })
    }

    fn machine(app: Option<Arc<ElfInfo>>, executor: Executor, words: &[u32]) -> Machine {
        machine_with(MachineConfig::default(), app, executor, words)
    }

    fn machine_with(
        cfg: MachineConfig,
        app: Option<Arc<ElfInfo>>,
        executor: Executor,
        words: &[u32],
    ) -> Machine {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), app, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let mut m = Machine::new(cfg, assets).expect("composes");
        m.set_executor(executor);
        for (i, w) in words.iter().enumerate() {
            let stored = m.soc.store_mem(PROG + 4 * i as u32, 4, *w);
            assert!(matches!(stored, Stored::Wrote { .. }));
        }
        m.hart.pc = PROG;
        m
    }

    fn trip(reason: &StopReason) -> &TripReport {
        match reason {
            StopReason::Tripwire(report) => report,
            other => panic!("expected a tripwire, got {other:?}"),
        }
    }

    #[test]
    fn a_disabled_radio_init_stops_at_its_entry_naming_the_feature() {
        // The init entry is a tripwire whether the configuration disables BLE or the image fails
        // the fail-closed binding (this synthetic ELF pins no code bytes).
        let disabled = || {
            let mut cfg = MachineConfig::default();
            cfg.hle.disabled = vec!["ble".to_string()];
            cfg
        };
        let cases = [
            (disabled as fn() -> MachineConfig, FeatureStatus::Disabled),
            (MachineConfig::default, FeatureStatus::UnsupportedImage),
        ];
        for executor in BOTH {
            for (cfg, status) in cases {
                let app = elf(7, &[("esp_bt_controller_init", PROG + 8)]);
                let mut m = machine_with(cfg(), Some(app), executor, &COUNT_FOUR);
                assert_eq!(m.hle_binding().record.features.get("ble"), Some(&status));
                let out = m.run(RunLimits::insns(100));
                let report = trip(&out.reason);
                assert_eq!(report.kind, TripKind::DisabledFeature, "{executor:?}");
                assert_eq!((report.pc, report.feature), (PROG + 8, Some("ble")));
                assert!(report.detail.contains("esp_bt_controller_init"));
                assert_eq!((out.insns, m.hart.x[10], m.hart.pc), (2, 2, PROG + 8));
                // A tripwire is not a breakpoint: running on stops there again.
                let again = m.run(RunLimits::insns(100));
                assert_eq!(again.reason, out.reason, "{executor:?}");
                assert_eq!(again.insns, 0);
            }
        }
    }

    #[test]
    fn a_fetch_in_the_magic_range_outside_the_allocations_is_a_tripwire() {
        // `lui t0, 0x4005F; addi t0, t0, -0x32C; jr t0`: jumps to 0x4005ECD4, one past BT_ISR.
        let words = [0x4005_F2B7, 0xCD42_8293, 0x0002_8067];
        for executor in BOTH {
            let mut m = machine(None, executor, &words);
            assert!(m.magic_range_hooks() > 180, "{}", m.magic_range_hooks());
            let out = m.run(RunLimits::insns(100));
            let report = trip(&out.reason);
            assert_eq!(report.kind, TripKind::MagicRangeFetch, "{executor:?}");
            assert_eq!((report.pc, out.insns), (0x4005_ECD4, 3));
        }
    }

    #[test]
    fn an_allocated_magic_pc_with_no_continuation_is_an_hle_error() {
        // `lui t0, 0x4005F; addi t0, t0, -0x340; jr t0`: the RETURN allocation 0x4005ECC0.
        let words = [0x4005_F2B7, 0xCC02_8293, 0x0002_8067];
        for executor in BOTH {
            let mut m = machine(None, executor, &words);
            assert_eq!(m.magic_pc(MagicKind::Return), 0x4005_ECC0);
            let out = m.run(RunLimits::insns(100));
            let StopReason::Hle(err) = out.reason else {
                panic!("{executor:?}: {:?}", out.reason);
            };
            assert_eq!(err.kind, HleErrorKind::UnknownContinuation);
        }
    }

    #[test]
    fn radio_mmio_accesses_trip_once_the_boot_allowance_is_spent() {
        // `lui a0, 0x6001D; loop: sw zero, 0x54(a0); j loop`: `rtc_sleep_pu` at 0x6001D054, a
        // boot-allowance register.
        let words = [0x6001_D537, 0x0405_2A23, 0xFFDF_F06F];
        for executor in BOTH {
            let mut m = machine(None, executor, &words);
            let out = m.run(RunLimits::insns(1_000));
            let report = trip(&out.reason);
            assert_eq!(report.kind, TripKind::RadioMmio, "{executor:?}");
            assert_eq!(report.pc, PROG + 4);
            assert_eq!(out.insns, 1 + 2 * 14 + 1);
            assert_eq!(m.radio_accesses(), 14);
        }
        // `lui a0, 0x60031; sw zero, 0x204(a0); j .`: a BLE baseband register trips at once.
        let words = [0x6003_1537, 0x2005_2223, 0x0000_006F];
        for executor in BOTH {
            let mut m = machine(None, executor, &words);
            let out = m.run(RunLimits::insns(100));
            let report = trip(&out.reason);
            assert_eq!((report.pc, out.insns), (PROG + 4, 2), "{executor:?}");
            assert!(report.detail.contains("radio_ble"), "{}", report.detail);
        }
    }

    #[test]
    fn the_panic_handler_stops_with_the_abort_capture_and_the_next_run_goes_on() {
        // `abort` is captured and runs on; `esp_panic_handler` stops the run.
        for executor in BOTH {
            let app = elf(7, &[("abort", PROG + 4), ("esp_panic_handler", PROG + 12)]);
            let mut m = machine(Some(app), executor, &COUNT_FOUR);
            let out = m.run(RunLimits::insns(100));
            let StopReason::GuestPanic(capture) = out.reason else {
                panic!("{executor:?}: {:?}", out.reason);
            };
            assert_eq!(capture.pc, PROG + 12);
            assert_eq!(capture.args[0], 3);
            assert_eq!(capture.first, Some((ObserveKind::Abort, [1, 0, 0, 0])));
            assert_eq!(m.hle_state().observed, [1, 1, 0, 0]);
            let out = m.run(RunLimits::insns(100));
            assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
            assert_eq!(m.hart.x[10], 4);
        }
    }

    #[test]
    fn a_breakpoint_on_a_hooked_pc_stops_first_and_the_resume_runs_the_hook_once() {
        // An observe hook and a breakpoint on one pc, and a panic handler breakpoint whose resume
        // must not stop twice.
        for executor in BOTH {
            let app = elf(7, &[("esp_panic_handler", PROG + 12)]);
            let mut m = machine(Some(app), executor, &COUNT_FOUR);
            assert!(m.add_observe_hook(PROG + 8, "third"));
            let stops = || crate::stops::StopSet {
                breakpoints: vec![PROG + 8, PROG + 12],
                ..crate::stops::StopSet::default()
            };
            let lim = || RunLimits {
                until: None,
                max_insns: Some(100),
                stops: stops(),
            };
            let out = m.run(lim());
            assert_eq!(out.reason, StopReason::Breakpoint(PROG + 8), "{executor:?}");
            assert_eq!(m.observe_fires(PROG + 8), None, "{executor:?}");
            let out = m.run(lim());
            assert_eq!(
                out.reason,
                StopReason::Breakpoint(PROG + 12),
                "{executor:?}"
            );
            assert_eq!(m.observe_fires(PROG + 8), Some((1, 2)), "{executor:?}");
            let out = m.run(lim());
            assert!(
                matches!(out.reason, StopReason::GuestPanic(_)),
                "{executor:?}: {:?}",
                out.reason
            );
            let out = m.run(lim());
            assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
            assert_eq!(m.hart.x[10], 4, "{executor:?}");
            assert_eq!(m.hle_state().observed[0], 1, "{executor:?}");
        }
    }

    #[test]
    fn task_delete_bumps_the_generation_and_the_hle_state_survives_a_restore() {
        let app = || elf(7, &[("vTaskDelete", PROG + 4)]);
        let mut m = machine(Some(app()), Executor::Engine, &COUNT_FOUR);
        assert!(m.add_observe_hook(PROG + 8, "third"));
        assert!(!m.add_observe_hook(PROG + 8, "again"));
        let out = m.run(RunLimits::insns(100));
        assert_eq!(out.reason, StopReason::MaxInsns);
        assert_eq!(m.hart.x[10], 4, "observe hooks run the instruction");
        // `a0` is 1 at the `vTaskDelete` entry: the handle of the deleted task.
        assert_eq!(m.hle_section().task_generations.get(&1), Some(&1));
        assert_eq!(m.observe_fires(PROG + 8), Some((1, 2)));

        let snap = m.snapshot(SnapOpts::default());
        let mut fresh = machine(Some(app()), Executor::Reference, &[]);
        fresh.restore(&snap).expect("same app ELF restores");
        assert_eq!(fresh.state_hash(), m.state_hash());
        assert_eq!(fresh.hle_section(), m.hle_section());
        assert_eq!(fresh.hooks.get(PROG + 8), Some(user_observe_hook_id()));

        // A snapshot bound against another app ELF, or restored without one, is another run
        // identity.
        assert_eq!(snap.header.image_sha256.len(), 2);
        assert_eq!(snap.header.image_sha256[1], [7; 32]);
        let mismatch = Err(pemu_core::snap::SnapError::IdentityMismatch {
            field: pemu_core::snap::IdentityField::Image,
        });
        let mut other = machine(Some(elf(8, &[])), Executor::Engine, &[]);
        assert_eq!(other.restore(&snap), mismatch);
        let mut bare = machine(None, Executor::Engine, &[]);
        assert_eq!(bare.restore(&snap), mismatch);
        assert_eq!(
            bare.snapshot(SnapOpts::default()).header.image_sha256.len(),
            1
        );
    }

    #[test]
    fn a_reset_restores_the_radio_allowance_and_the_count_survives_a_restore() {
        let words = [0x6001_D537, 0x0405_2A23, 0xFFDF_F06F];
        let mut m = machine(None, Executor::Engine, &words);
        m.run(RunLimits::insns(1 + 2 * 5));
        assert_eq!(m.radio_accesses(), 5);
        let snap = m.snapshot(SnapOpts::default());
        let mut fresh = machine(None, Executor::Engine, &[]);
        fresh.restore(&snap).expect("restores");
        assert_eq!(fresh.radio_accesses(), 5);
        assert_eq!(fresh.state_hash(), m.state_hash());
        m.chip_reset(pemu_core::reset::ResetKind::of(pemu_core::reset::ResetCause(0x03)).unwrap());
        assert_eq!(m.radio_accesses(), 0);
    }

    #[test]
    fn a_reset_clears_the_abort_capture_a_later_panic_would_report() {
        for executor in BOTH {
            let app = elf(7, &[("abort", PROG + 4), ("esp_panic_handler", PROG + 12)]);
            let mut m = machine(Some(app), executor, &COUNT_FOUR);
            let out = m.run(RunLimits::insns(3));
            assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
            assert!(m.hle_state().first_capture.is_some(), "{executor:?}");
            m.chip_reset(
                pemu_core::reset::ResetKind::of(pemu_core::reset::ResetCause(0x03)).unwrap(),
            );
            assert_eq!(m.hle_state().first_capture, None, "{executor:?}");
            for (i, w) in COUNT_FOUR.iter().enumerate() {
                m.soc.store_mem(PROG + 4 * i as u32, 4, *w);
            }
            m.hart.pc = PROG + 8;
            let out = m.run(RunLimits::insns(100));
            let StopReason::GuestPanic(capture) = out.reason else {
                panic!("{executor:?}: {:?}", out.reason);
            };
            assert_eq!(capture.first, None, "{executor:?}");
        }
    }

    #[test]
    fn a_radio_event_for_a_module_that_is_not_bound_is_refused() {
        // This synthetic ELF fails the BLE binding, so an HCI packet is counted unapplied.
        let app = elf(7, &[("esp_bt_controller_init", PROG + 8)]);
        let mut m = machine(Some(app), Executor::Engine, &COUNT_FOUR);
        assert!(!m.on_radio_hci(HciInput::Packet { seq: 0, data: &[4] }));
        assert!(!m.on_radio_hci(HciInput::Attach));
        assert!(m.radio_worker(MagicKind::BtWorker).is_none());
        assert!(!m.irq_source_level(IrqSource(8)));
        assert_eq!(m.radio_module_state("ble"), None);
    }

    struct Timed(Arc<std::sync::atomic::AtomicUsize>);

    impl ModuleHost for Timed {
        fn module(&self) -> ModuleIndex {
            ModuleIndex::FIRST_MODULE
        }
        fn name(&self) -> &'static str {
            "timed"
        }
        fn magic_entries(&self) -> Vec<MagicKind> {
            vec![MagicKind::BtWorker]
        }
        fn workers(
            &mut self,
            _wake: pemu_hle::worker::WakeMode,
        ) -> Vec<pemu_hle::worker::WorkerConfig> {
            Vec::new()
        }
        fn enter(
            &mut self,
            _state: &mut Vec<u8>,
            _kind: HandlerKind,
            _g: &mut dyn GuestView,
        ) -> (HandlerState, HleAction) {
            unreachable!("no hook is bound")
        }
        fn resume(
            &mut self,
            _state: &mut Vec<u8>,
            _handler: &mut HandlerState,
            _g: &mut dyn GuestView,
            _resume: Resume,
        ) -> HleAction {
            unreachable!("no hook is bound")
        }
        fn describe(&self, _func: u32) -> Option<CallInfo> {
            None
        }
        fn deliver(&mut self, _state: &mut Vec<u8>, _entry: MagicKind, _events: Vec<RadioEvent>) {}
        fn on_timer(
            &mut self,
            _state: &mut Vec<u8>,
            _tag: u16,
            _g: &mut dyn GuestView,
        ) -> Vec<RadioEvent> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Vec::new()
        }
    }

    #[test]
    fn a_module_timer_without_state_or_power_is_dropped() {
        use std::sync::atomic::Ordering;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut m = machine(None, Executor::Engine, &COUNT_FOUR);
        m.hle.hosts.push(Box::new(Timed(calls.clone())));
        let radio = pemu_core::sched::RadioId(u16::from(ModuleIndex::FIRST_MODULE.0));
        assert_eq!(m.on_radio_timer(radio, 1), 0);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(!m.hle.state.modules.contains_key("timed"));
        m.hle.state.modules.insert("timed".to_string(), vec![1]);
        m.on_radio_timer(radio, 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let now = m.now();
        let key = pemu_hle::core::module_timer(ModuleIndex::FIRST_MODULE, 1);
        m.sched.schedule(now, now, key);
        m.mcu_powered = false;
        let dropped = m.unpowered_events;
        m.dispatch_due_events();
        assert_eq!(m.unpowered_events, dropped + 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        m.mcu_powered = true;
        m.sched.schedule(now, now, key);
        m.dispatch_due_events();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    /// A `ble` or `wifi` module host whose whole state is its bridge: `[1]` up, `[0]` down.
    struct Bridged(&'static str);

    impl ModuleHost for Bridged {
        fn module(&self) -> ModuleIndex {
            ModuleIndex::FIRST_MODULE
        }
        fn name(&self) -> &'static str {
            self.0
        }
        fn magic_entries(&self) -> Vec<MagicKind> {
            Vec::new()
        }
        fn workers(
            &mut self,
            _wake: pemu_hle::worker::WakeMode,
        ) -> Vec<pemu_hle::worker::WorkerConfig> {
            Vec::new()
        }
        fn enter(
            &mut self,
            _state: &mut Vec<u8>,
            _kind: HandlerKind,
            _g: &mut dyn GuestView,
        ) -> (HandlerState, HleAction) {
            unreachable!("no hook is bound")
        }
        fn resume(
            &mut self,
            _state: &mut Vec<u8>,
            _handler: &mut HandlerState,
            _g: &mut dyn GuestView,
            _resume: Resume,
        ) -> HleAction {
            unreachable!("no hook is bound")
        }
        fn describe(&self, _func: u32) -> Option<CallInfo> {
            None
        }
        fn deliver(&mut self, _state: &mut Vec<u8>, _entry: MagicKind, _events: Vec<RadioEvent>) {}
        fn bridges_live(&self, state: &[u8]) -> u32 {
            u32::from(state == [1])
        }
        fn on_hci(
            &mut self,
            state: &mut Vec<u8>,
            ev: HciInput<'_>,
            _g: &mut dyn GuestView,
        ) -> Result<Vec<RadioEvent>, HleError> {
            match ev {
                HciInput::Attach => *state = vec![1],
                HciInput::Detach => *state = vec![0],
                HciInput::Packet { .. } => {}
            }
            Ok(Vec::new())
        }
        fn on_net(
            &mut self,
            state: &mut Vec<u8>,
            ev: NetInput<'_>,
            _g: &mut dyn GuestView,
        ) -> Result<Vec<RadioEvent>, HleError> {
            match ev {
                NetInput::Attach { .. } => *state = vec![1],
                NetInput::Detach => *state = vec![0],
                NetInput::Packet { .. } => {}
            }
            Ok(Vec::new())
        }
    }

    /// A machine whose `ble` module holds its HCI bridge open, attached through the journal.
    fn bridged() -> Machine {
        let mut m = machine(None, Executor::Engine, &COUNT_FOUR);
        m.hle.hosts.push(Box::new(Bridged("ble")));
        attach(&mut m, EnvChange::BleHciBridge { attached: true });
        assert_eq!(m.live_bridges(), 1);
        m
    }

    fn attach(m: &mut Machine, change: EnvChange) {
        m.input_from(At::Now, Origin::Bridge, InputEvent::Env(change))
            .expect("the attach journals");
        m.apply_due_journal();
    }

    fn names_the_hci_bridge(err: &pemu_core::snap::SnapError) -> bool {
        matches!(err, pemu_core::snap::SnapError::LiveBridge { bridge } if bridge.contains("HCI"))
    }

    /// The refused restore leaves the machine as it was.
    #[test]
    fn a_live_bridge_refuses_a_fork_and_a_restore() {
        use pemu_core::snap::LivePolicy;
        let mut m = bridged();
        let snap = m.snapshot(SnapOpts::default());
        let err = m
            .fork(LivePolicy::Refuse)
            .err()
            .expect("the fork is refused");
        assert!(names_the_hci_bridge(&err), "{err:?}");
        let hash = m.state_hash();
        let seq = m.journal().next_seq();
        let err = m.restore(&snap).expect_err("the restore is refused");
        assert!(names_the_hci_bridge(&err), "{err:?}");
        assert_eq!(m.state_hash(), hash, "a refused restore changes nothing");
        assert_eq!(m.journal().next_seq(), seq);
    }

    #[test]
    fn a_link_down_journals_the_detach_of_each_live_bridge() {
        let mut m = bridged();
        let seq = m.journal().next_seq();
        let unapplied = m.unapplied_inputs();
        m.link_down();
        assert_eq!(m.live_bridges(), 0);
        assert_eq!(m.journal().next_seq(), seq + 1, "one detach per bridge");
        assert_eq!(m.unapplied_inputs(), unapplied, "the module took it");
        m.link_down();
        assert_eq!(m.journal().next_seq(), seq + 1, "nothing to detach");

        m.hle.hosts.push(Box::new(Bridged("wifi")));
        attach(&mut m, EnvChange::BleHciBridge { attached: true });
        attach(
            &mut m,
            EnvChange::WifiBridge {
                attached: true,
                routes: Vec::new(),
            },
        );
        assert_eq!(m.live_bridges(), 2);
        let seq = m.journal().next_seq();
        m.link_down();
        assert_eq!(m.live_bridges(), 0);
        assert_eq!(m.journal().next_seq(), seq + 2);
        assert_eq!(m.unapplied_inputs(), unapplied);
    }

    #[test]
    fn a_link_down_fork_leaves_the_original_attached() {
        let m = bridged();
        let copy = m
            .fork(pemu_core::snap::LivePolicy::LinkDown)
            .expect("a link-down fork forks");
        assert_eq!(copy.live_bridges(), 0);
        assert_eq!(m.live_bridges(), 1);
    }

    #[test]
    fn the_receipt_carries_the_binding_record() {
        let app = elf(7, &[("esp_bt_controller_init", PROG + 8)]);
        let mut m = machine(Some(app), Executor::Engine, &COUNT_FOUR);
        let receipt = m.receipt();
        let record = receipt.binding.expect("a machine reports its binding");
        assert_eq!(record.app_elf_sha256, [7; 32]);
        assert_eq!(
            record.features.get("ble"),
            Some(&FeatureStatus::UnsupportedImage)
        );
        assert_eq!(&record, &m.hle_binding().record);
        // The refusal carries its reasons: a receipt that says only `unsupported image` leaves
        // the reader to guess which check failed.
        let [(module, why)] = &receipt.binding_mismatches[..] else {
            panic!("one refused module: {:?}", receipt.binding_mismatches);
        };
        assert_eq!(module, "ble");
        assert!(!why.is_empty());
        assert_eq!(why, &m.hle_binding().mismatches[0].1);
    }

    #[test]
    fn a_module_whose_worker_the_core_refuses_is_refused_whole() {
        let mut bound = BoundHooks::default();
        let module = ModuleIndex::FIRST_MODULE;
        let handler = |n| HookRef {
            kind: HookKind::Hle(HandlerKind(n)),
            module,
        };
        bound.set.insert(0x4200_0000, handler(0).to_id());
        bound.set.insert(0x4200_0100, handler(1).to_id());
        let core_hook = HookRef::core(HookKind::Tripwire(TripKind::MagicRangeFetch)).to_id();
        bound.set.insert(0x4005_ECC2, core_hook);
        bound
            .record
            .features
            .insert("ble".to_string(), FeatureStatus::Bound);
        let config = pemu_hle::worker::WorkerConfig {
            profile: pemu_hle::worker::WorkerProfile {
                wake: pemu_hle::worker::WakeMode::U4MagicIsr,
                ..pemu_hle::worker::bt_controller_profile()
            },
            calls: pemu_hle::worker::WorkerCalls {
                task_create: 0,
                queue_create: 0,
                semaphore_take: 0,
                give_from_isr: 0,
                yield_from_isr: 0,
            },
        };
        let err = check_worker(&config).expect_err("U4 without a source");
        refuse_module(
            &mut bound,
            module,
            "ble",
            BindingMismatch {
                symbol: "btController".to_string(),
                field: MismatchField::Worker,
                expected: String::new(),
                found: err.detail,
            },
        );
        assert_eq!(
            bound.set.iter().collect::<Vec<_>>(),
            [(0x4005_ECC2, core_hook)]
        );
        assert_eq!(
            bound.record.features.get("ble"),
            Some(&FeatureStatus::UnsupportedImage)
        );
        assert_eq!(bound.mismatches[0].1[0].field, MismatchField::Worker);
    }

    #[test]
    fn an_image_without_the_vhci_functions_or_without_an_elf_is_not_linked_never_bound() {
        for app in [Some(elf(7, &[("app_main", PROG)])), None] {
            let has_elf = app.is_some();
            let mut m = machine(app, Executor::Engine, &COUNT_FOUR);
            let record = m.receipt().binding.expect("a machine reports its binding");
            assert_eq!(
                record.features.get("ble"),
                Some(&FeatureStatus::NotLinked),
                "ELF {has_elf}"
            );
            assert_eq!(FeatureStatus::NotLinked.receipt_word(), "not linked");
            assert_eq!(record.profile_id, "", "no module bound, no profile id");
            assert!(record.log_lines.is_empty(), "no host, no log lines");
            assert!(m.radio_worker(MagicKind::BtWorker).is_none());
        }
    }

    #[test]
    fn a_breakpoint_and_a_tripwire_on_one_pc_alternate_and_count_no_fire() {
        for executor in BOTH {
            let app = elf(7, &[("esp_bt_controller_init", PROG + 8)]);
            let mut m = machine(Some(app), executor, &COUNT_FOUR);
            assert!(m.add_observe_hook(PROG + 8, "trip"));
            let lim = || RunLimits {
                until: None,
                max_insns: Some(100),
                stops: crate::stops::StopSet {
                    breakpoints: vec![PROG + 8],
                    ..crate::stops::StopSet::default()
                },
            };
            for _ in 0..2 {
                let out = m.run(lim());
                assert_eq!(out.reason, StopReason::Breakpoint(PROG + 8), "{executor:?}");
                let out = m.run(lim());
                assert_eq!(trip(&out.reason).pc, PROG + 8, "{executor:?}");
                assert_eq!(out.insns, 0, "{executor:?}");
            }
            let out = m.run(RunLimits::insns(100));
            assert_eq!(trip(&out.reason).pc, PROG + 8, "{executor:?}");
            assert_eq!(m.observe_fires(PROG + 8), None, "{executor:?}: nothing ran");
            assert_eq!(m.hart.x[10], 2, "{executor:?}");
        }
    }

    #[test]
    fn a_user_hook_counts_the_same_whether_added_before_or_after_its_breakpoint() {
        let bp = || RunLimits {
            until: None,
            max_insns: Some(10),
            stops: crate::stops::StopSet {
                breakpoints: vec![PROG + 8],
                ..crate::stops::StopSet::default()
            },
        };
        let mut results = Vec::new();
        for executor in BOTH {
            for hook_first in [false, true] {
                let mut m = machine(None, executor, &COUNT_FOUR);
                if hook_first {
                    assert!(m.add_observe_hook(PROG + 8, "h"));
                }
                let out = m.run(RunLimits {
                    max_insns: Some(1),
                    ..bp()
                });
                assert_eq!(out.reason, StopReason::MaxInsns);
                if !hook_first {
                    assert!(m.add_observe_hook(PROG + 8, "h"));
                }
                let reasons: Vec<StopReason> = (0..3).map(|_| m.run(bp()).reason).collect();
                assert_eq!(
                    reasons,
                    [
                        StopReason::Breakpoint(PROG + 8),
                        StopReason::MaxInsns,
                        StopReason::MaxInsns
                    ],
                    "{executor:?} hook_first={hook_first}"
                );
                results.push((m.observe_fires(PROG + 8), m.state_hash()));
            }
        }
        assert_eq!(results[0].0, Some((1, 2)));
        assert!(
            results.iter().all(|r| *r == results[0]),
            "fires and state are independent of the order and the executor: {:?}",
            results.iter().map(|r| r.0).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_user_hook_added_under_a_breakpoint_is_installed_when_the_breakpoint_goes() {
        for executor in BOTH {
            let mut m = machine(None, executor, &COUNT_FOUR);
            let out = m.run(RunLimits {
                until: None,
                max_insns: Some(1),
                stops: crate::stops::StopSet {
                    breakpoints: vec![PROG + 8],
                    ..crate::stops::StopSet::default()
                },
            });
            assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
            assert!(m.add_observe_hook(PROG + 8, "third"));
            let out = m.run(RunLimits::insns(100));
            assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
            assert_eq!(m.observe_fires(PROG + 8), Some((1, 2)), "{executor:?}");
            assert_eq!(m.hooks.get(PROG + 8), Some(user_observe_hook_id()));
        }
    }

    #[test]
    fn an_elf_less_image_stops_after_three_resets_with_identical_panic_text() {
        use pemu_core::hostio::SerialStream;
        use pemu_core::reset::{ResetCause, ResetKind};
        let reset = || ResetKind::of(ResetCause(0x03)).expect("RTC_SW_SYS_RST");
        let panic = |m: &mut Machine, text: &[u8]| {
            let now = m.now();
            m.io.serial_write(SerialStream::UsjTx, b"I (12) boot: ok\r\n", now);
            m.io.serial_write(SerialStream::UsjTx, text, now);
        };
        let loop_text = b"assert failed: esp_phy_load_cal_and_init phy_init.c:327 (ret)\r\n";

        let mut m = machine(None, Executor::Engine, &[]);
        panic(&mut m, loop_text);
        m.chip_reset(reset());
        panic(&mut m, loop_text);
        m.chip_reset(reset());
        assert!(
            m.hle.state.reset_loop.pending_pc.is_none(),
            "two resets are not a loop"
        );
        let snap = m.snapshot(SnapOpts::default());
        let mut fresh = machine(None, Executor::Reference, &[]);
        fresh.restore(&snap).expect("restores");
        assert_eq!(fresh.hle_state().reset_loop.repeats, 2);
        for m in [&mut m, &mut fresh] {
            panic(m, loop_text);
            m.chip_reset(reset());
            let out = m.run(RunLimits::insns(100));
            let report = trip(&out.reason);
            assert_eq!(report.kind, TripKind::ResetLoop);
            assert!(
                report.detail.contains("phy_init.c:327"),
                "{}",
                report.detail
            );
            assert_eq!(out.insns, 0);
        }

        let mut m = machine(None, Executor::Engine, &[]);
        for text in [
            &loop_text[..],
            b"abort() was called at PC 0x42001234\r\n",
            loop_text,
        ] {
            panic(&mut m, text);
            m.chip_reset(reset());
        }
        panic(&mut m, b"I (30) main: no panic this time\r\n");
        m.chip_reset(reset());
        assert!(m.hle.state.reset_loop.pending_pc.is_none());
        assert_eq!(m.hle_state().reset_loop.repeats, 0);

        // An image with an ELF has the observe hooks instead.
        let mut m = machine(Some(elf(7, &[])), Executor::Engine, &[]);
        for _ in 0..4 {
            panic(&mut m, loop_text);
            m.chip_reset(reset());
        }
        assert!(m.hle.state.reset_loop.pending_pc.is_none());
    }

    #[test]
    fn a_raised_reset_loop_stop_survives_a_restore_before_it_is_reported() {
        use pemu_core::hostio::SerialStream;
        use pemu_core::reset::{ResetCause, ResetKind};
        for executor in BOTH {
            let mut m = machine(None, executor, &[]);
            for _ in 0..3 {
                let now = m.now();
                m.io.serial_write(SerialStream::UsjTx, b"assert failed: x.c:1 (0)\r\n", now);
                m.chip_reset(ResetKind::of(ResetCause(0x03)).expect("RTC_SW_SYS_RST"));
            }
            let snap = m.snapshot(SnapOpts::default());
            let mut fresh = machine(None, executor, &[]);
            fresh.restore(&snap).expect("restores");
            let out = fresh.run(RunLimits::insns(100));
            assert_eq!(trip(&out.reason).kind, TripKind::ResetLoop, "{executor:?}");
            assert_eq!(out.insns, 0);
            let out = fresh.run(RunLimits::insns(1));
            assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
        }
    }

    #[test]
    fn a_radio_trip_is_reported_ahead_of_a_hang_and_survives_a_restore() {
        // Reads of `rtc_sleep_pu` with 13 of 14 boot accesses spent: the second read is the
        // fifteenth access and, with `stuck_ms` 0, also the fallback hang row.
        let words = [0x6001_D537, 0x0545_2583, 0xFFDF_F06F];
        let mut hashes = Vec::new();
        for executor in BOTH {
            let mut m = machine(None, executor, &words);
            m.set_hang_detector(crate::hang::HangCfg {
                enabled: true,
                stuck_ms: 0,
            });
            m.hle.restore_radio_used(13);
            let out = m.run(RunLimits::insns(1_000));
            let report = trip(&out.reason);
            assert_eq!(
                report.kind,
                TripKind::RadioMmio,
                "{executor:?}: {:?}",
                out.reason
            );
            assert_eq!(report.pc, PROG + 4, "{executor:?}");
            assert_eq!(m.hle_state().radio_trip, None, "reported once");

            // A trip raised and not yet reported is guest state: it stops the restored machine
            // before any instruction.
            let mut m = machine(None, executor, &words);
            m.hle.state.radio_trip = Some(PendingRadioTrip {
                pc: PROG + 4,
                detail: "radio_ble 0x60031204".to_string(),
            });
            let snap = m.snapshot(SnapOpts::default());
            let mut fresh = machine(None, executor, &[]);
            fresh.restore(&snap).expect("restores");
            assert_eq!(fresh.state_hash(), m.state_hash(), "{executor:?}");
            let out = fresh.run(RunLimits::insns(1_000));
            let report = trip(&out.reason);
            assert_eq!(
                (report.kind, report.pc, out.insns),
                (TripKind::RadioMmio, PROG + 4, 0),
                "{executor:?}"
            );
            hashes.push(fresh.state_hash());
        }
        assert_eq!(hashes[0], hashes[1]);
    }

    #[test]
    fn a_limit_on_a_hooked_pc_ends_the_run_before_the_hook_on_both_executors() {
        let mut hashes = Vec::new();
        for executor in BOTH {
            let mut m = machine(None, executor, &COUNT_FOUR);
            assert!(m.add_observe_hook(PROG + 8, "third"));
            let out = m.run(RunLimits::insns(2));
            assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
            assert_eq!(m.observe_fires(PROG + 8), None, "{executor:?}");
            let snap = m.snapshot(SnapOpts::default());
            let mut fresh = machine(None, executor, &[]);
            fresh.restore(&snap).expect("restores");
            assert_eq!(fresh.state_hash(), m.state_hash(), "{executor:?}");
            for machine in [&mut m, &mut fresh] {
                let out = machine.run(RunLimits::insns(100));
                assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
                assert_eq!(
                    machine.observe_fires(PROG + 8),
                    Some((1, 2)),
                    "{executor:?}"
                );
                assert_eq!(machine.hart.x[10], 4, "{executor:?}");
            }
            assert_eq!(fresh.state_hash(), m.state_hash(), "{executor:?}");
            hashes.push(m.state_hash());
        }
        assert_eq!(hashes[0], hashes[1], "engine and reference agree");
    }

    #[test]
    fn a_limit_on_a_tripwire_pc_ends_the_run_before_it_trips_on_both_executors() {
        for executor in BOTH {
            let app = elf(7, &[("esp_bt_controller_init", PROG + 8)]);
            let mut m = machine(Some(app.clone()), executor, &COUNT_FOUR);
            let out = m.run(RunLimits::insns(2));
            assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
            let snap = m.snapshot(SnapOpts::default());
            let mut fresh = machine(Some(app), executor, &[]);
            fresh.restore(&snap).expect("restores");
            for machine in [&mut m, &mut fresh] {
                let out = machine.run(RunLimits::insns(100));
                assert_eq!(trip(&out.reason).pc, PROG + 8, "{executor:?}");
                assert_eq!(out.insns, 0, "{executor:?}");
            }
        }
    }

    #[test]
    fn a_pending_run_past_is_guest_state_a_restore_keeps() {
        // The run-past request lives in `hle.machine`, so a machine restored between the hook
        // exit and the slice runs the instruction without a second fire.
        for executor in BOTH {
            let mut m = machine(None, executor, &COUNT_FOUR);
            assert!(m.add_observe_hook(PROG + 8, "third"));
            m.run(RunLimits::insns(2));
            m.hle.state.fires.insert(PROG + 8, (1, 2));
            m.continue_past_hook(PROG + 8);
            let snap = m.snapshot(SnapOpts::default());
            let mut fresh = machine(None, executor, &[]);
            fresh.restore(&snap).expect("restores");
            assert_eq!(
                fresh.hle_state().run_past_at,
                Some(PROG + 8),
                "{executor:?}"
            );
            let out = fresh.run(RunLimits::insns(100));
            assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
            assert_eq!(fresh.observe_fires(PROG + 8), Some((1, 2)), "{executor:?}");
            assert_eq!(fresh.hart.x[10], 4, "{executor:?}");
        }
    }
}
