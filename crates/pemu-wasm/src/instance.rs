//! The machines behind the ABI handles, reached through two doors. The raw calls (`pemu_run`,
//! `pemu_input`, `pemu_snapshot`, ...) use the [`Machine`] directly. `pemu_call` runs registry
//! commands, which reach an instance only through the process pool, so `pemu_build` also
//! registers a pool session whose backend is a [`Hosted`] pointer to the same machine. The
//! pointer is sound under three rules:
//!
//! 1. the machine is heap-allocated once ([`Box::into_raw`]) and freed only in [`Instance`]'s
//!    `Drop`, after the pool session that points at it was destroyed;
//! 2. no reference into the machine from the raw side is alive while a registry handler runs:
//!    `pemu_call` copies the instance id out, releases every borrow, and only then dispatches;
//! 3. every entry point holds the one handle-table lock ([`with_table`]) for its whole body, so
//!    two threads never reach one machine at once; [`Hosted`] asserts it in debug builds.
//!
//! No registry command arms `RunLimits::stops`, so `pemu_call` also answers reserved requests
//! starting with `@`, which no registry name can ([`reserved`]). The machine does not record
//! which door an input came through, so each instance keeps a [`DoorLog`], and
//! [`journal_input`] is the one place this crate calls `Machine::input`.

use std::collections::BTreeMap;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard};

use pemu_api::commands::start::{StartArgs, with_pool};
use pemu_api::error::{
    ApiError, E_ASSET_MISSING, E_HOST_UNSUPPORTED, E_INTERNAL, E_SNAPSHOT, E_STATE, E_USAGE,
};
use pemu_api::host_support::{self, Host};
use pemu_api::instance::{InstanceId, Lifecycle};
use pemu_api::spec::HandlerCx;
use pemu_core::hostio::HostIo;
use pemu_core::input::InputEvent;
use pemu_core::journal::{JournalEntry, LiveStream, Origin};
use pemu_core::snap::{LivePolicy, SnapError, SnapOpts, Snapshot};
use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::elf::ElfInfo;
use pemu_loader::rom::RomImage;
use pemu_machine::machine::{At, GuestMem, InputError, Receipt};
use pemu_machine::run::{RunLimits, RunOutcome};
use pemu_machine::stops::StopSet;
use pemu_machine::{Machine, MachineApi, SnapshotMachine};
use serde_json::{Value, json};

use crate::config::WasmConfig;
use crate::io_view::IoPublisher;
use crate::layout::stop_code;

/// The pool backend of a handle's machine (rules 1 to 3). The only value is the one [`build`]
/// registers under the handle-table lock, so the pool reaches the pointer only from a handler
/// [`call`] dispatches, under that lock too.
struct Hosted(NonNull<Machine>, SharedDoors);

// SAFETY: (a) the pointee is a `Send` `Machine` owned by one `Instance` and freed only after the
// pool session holding this value was destroyed (rule 1); (b) it is dereferenced only under the
// handle-table lock, checked in debug builds (rule 3), so a moved `Hosted` never gives two
// threads the machine; (c) no raw-side reference is alive meanwhile (rule 2).
unsafe impl Send for Hosted {}

/// Whether the handle-table lock is held now, by any thread: enough for a debug check, since
/// rule 3 rules out another holder while a handler runs.
fn table_held() -> bool {
    matches!(TABLE.try_lock(), Err(std::sync::TryLockError::WouldBlock))
}

impl Hosted {
    fn get(&self) -> &Machine {
        debug_assert!(
            table_held(),
            "a pool backend was reached without the handle-table lock"
        );
        // SAFETY: invariant (a) keeps the machine alive for as long as this session exists, (b)
        // holds the lock, and (c) means no `&mut` from the raw side is alive.
        unsafe { self.0.as_ref() }
    }

    fn get_mut(&mut self) -> &mut Machine {
        debug_assert!(
            table_held(),
            "a pool backend was reached without the handle-table lock"
        );
        // SAFETY: as `get`; `&mut self` is the one path to the machine for the handler's call.
        unsafe { self.0.as_mut() }
    }
}

impl MachineApi for Hosted {
    fn run(&mut self, lim: RunLimits) -> RunOutcome {
        self.get_mut().run(lim)
    }

    fn input(&mut self, at: At, ev: InputEvent) -> Result<u64, InputError> {
        let doors = Arc::clone(&self.1);
        journal_input(
            self.get_mut(),
            &doors,
            Door::Registry,
            at,
            Origin::Agent,
            ev,
        )
    }

    fn input_from(&mut self, at: At, origin: Origin, ev: InputEvent) -> Result<u64, InputError> {
        let doors = Arc::clone(&self.1);
        journal_input(self.get_mut(), &doors, Door::Registry, at, origin, ev)
    }

    fn io(&mut self) -> &mut HostIo {
        self.get_mut().io()
    }

    fn now(&self) -> VTime {
        self.get().now()
    }

    fn guest_mem(&mut self) -> GuestMem<'_> {
        self.get_mut().guest_mem()
    }

    fn receipt(&mut self) -> Receipt {
        self.get_mut().receipt()
    }

    fn is_tainted(&self) -> bool {
        self.get().is_tainted()
    }

    /// Forwarded so a browser session sees the same ledger labels as a native one.
    fn heap_ledger(&self) -> Vec<pemu_machine::hle::HleHeapBlock> {
        self.get().heap_ledger()
    }

    /// Forwarded so a live bridge fixes this instance's pacing too.
    fn live_bridges(&self) -> u32 {
        self.get().live_bridges()
    }

    /// Forwarded so the `ble_*` commands read the same scripted central as natively.
    fn radio_module_state(&self, module: &str) -> Option<&[u8]> {
        self.get().radio_module_state(module)
    }

    /// Forwarded, so the page's header names the build as the native `status` does.
    fn app_desc(&self) -> Option<pemu_loader::app_desc::AppDesc> {
        self.get().app_desc()
    }
}

impl SnapshotMachine for Hosted {
    fn snapshot(&self, opts: SnapOpts) -> Result<Snapshot, SnapError> {
        Ok(self.get().snapshot(opts))
    }

    fn redact(
        &self,
        snapshot: &mut Snapshot,
    ) -> Result<pemu_machine::snapshot::Redaction, SnapError> {
        self.get().redact(snapshot)
    }

    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), SnapError> {
        let restored = self.get_mut().restore(snapshot);
        if restored.is_ok() {
            lock_doors(&self.1).restored(self.get(), Some(snapshot.canonical_hash()));
        }
        restored
    }

    fn fork(&self, live: LivePolicy) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
        Ok(Box::new(self.get().fork(live)?))
    }

    fn state_hash(&self) -> [u8; 32] {
        self.get().state_hash()
    }

    fn secret_generation(&self) -> u64 {
        self.get().secret_generation()
    }

    fn secret_sources(
        &self,
        view: pemu_machine::snapshot::FlashView,
    ) -> pemu_machine::snapshot::SecretSources {
        self.get().secret_sources(view)
    }

    fn interrupt_can_wake(&self) -> Option<bool> {
        Some(self.get().interrupt_can_wake())
    }

    fn watchdog_fired(&self) -> Option<pemu_machine::stops::WatchdogFire> {
        self.get().watchdog_fired()
    }
}

/// Which door an input reached the machine through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Door {
    /// `pemu_input`.
    Input,
    /// A registry command through `pemu_call`.
    Registry,
}

impl Door {
    fn as_str(self) -> &'static str {
        match self {
            Door::Input => "input",
            Door::Registry => "registry",
        }
    }
}

/// Where a restore left the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestoredAt {
    pub vt: VTime,
    pub cursor: u64,
    /// The `seq` the restored journal hands out next: entries below it came with the snapshot.
    pub next_seq: u64,
    /// `Snapshot::canonical_hash` of the snapshot restored, so a replay can check it; `None` when
    /// the restore path did not hand the snapshot over.
    pub snapshot: Option<[u8; 32]>,
}

/// The doors of a handle's journal entries, and its last restore.
#[derive(Default, Debug)]
pub struct DoorLog {
    /// `seq` to its instant and door. Matched on both, because a restore to an earlier instant
    /// hands the same `seq` out again.
    doors: BTreeMap<u64, (VTime, Door)>,
    /// The last restore, which makes `@journal` answer from the snapshot's instant.
    restored: Option<RestoredAt>,
}

impl DoorLog {
    /// The door `entry` came through; an entry this handle did not journal reads as
    /// [`Door::Input`]. A label only: dropping a payload is decided from the journal origin.
    fn door(&self, entry: &JournalEntry) -> Door {
        match self.doors.get(&entry.seq) {
            Some((at, door)) if *at == entry.at => *door,
            _ => Door::Input,
        }
    }

    /// Records a restore: the doors of every `seq` the journal will hand out again, or whose
    /// instant no longer matches, are forgotten. A second record of the same restore without a
    /// hash (the registry path records after `Hosted::restore` did) keeps the first one's.
    fn restored(&mut self, machine: &Machine, snapshot: Option<[u8; 32]>) {
        let journal = machine.journal();
        let next_seq = journal.next_seq();
        self.doors.retain(|seq, _| *seq < next_seq);
        let kept: BTreeMap<u64, VTime> = journal.entries().iter().map(|e| (e.seq, e.at)).collect();
        self.doors
            .retain(|seq, (at, _)| kept.get(seq).is_none_or(|k| k == at));
        let (vt, cursor) = (machine.now(), journal.cursor());
        let snapshot = snapshot.or_else(|| {
            self.restored
                .filter(|r| (r.vt, r.cursor, r.next_seq) == (vt, cursor, next_seq))
                .and_then(|r| r.snapshot)
        });
        self.restored = Some(RestoredAt {
            vt,
            cursor,
            next_seq,
            snapshot,
        });
    }

    pub fn restored_at(&self) -> Option<RestoredAt> {
        self.restored
    }
}

type SharedDoors = Arc<Mutex<DoorLog>>;

fn lock_doors(doors: &SharedDoors) -> MutexGuard<'_, DoorLog> {
    doors.lock().unwrap_or_else(|e| e.into_inner())
}

/// Journals `event` and records its door when the machine accepted it.
fn journal_input(
    machine: &mut Machine,
    doors: &SharedDoors,
    door: Door,
    at: At,
    origin: Origin,
    event: InputEvent,
) -> Result<u64, InputError> {
    let when = match at {
        At::Now => machine.now(),
        At::Vt(t) => t,
    };
    let seq = machine.input_from(at, origin, event)?;
    lock_doors(doors).doors.insert(seq, (when, door));
    Ok(seq)
}

/// What `pemu_new` starts and `pemu_load` fills. A configuration that did not parse is kept as
/// its reason, so `pemu_build` reports it as `E_USAGE` rather than `pemu_new` answering null.
pub struct MachineBuilder {
    pub config: Result<WasmConfig, String>,
    /// Kind 0. `None` makes `pemu_build` use `Assets::with_bundled_rom`.
    pub rom: Option<RomImage>,
    /// Kind 1.
    pub flash: Option<FlashImage>,
    /// Kind 2, or the app ELF of a `.pebundle` loaded as kind 1.
    pub app_elf: Option<Arc<ElfInfo>>,
    /// The bytes `app_elf` was parsed from, for the DWARF walk of `crate::introspect`
    /// (`pemu-api` may not load an ELF itself). Parsed lazily.
    pub app_elf_bytes: Option<Arc<[u8]>>,
    /// Kind 3, or the bootloader ELF of a `.pebundle` loaded as kind 1.
    pub boot_elf: Option<Arc<ElfInfo>>,
    /// Kind 4. `None` makes `pemu_build` use `EfuseImage::synth(seed)`.
    pub efuse: Option<EfuseImage>,
    /// The roles loaded so far, in `.pebundle` role names (`rom` for kind 0), each once.
    pub roles: Vec<&'static str>,
}

pub struct Instance {
    machine: NonNull<Machine>,
    pub id: InstanceId,
    publisher: IoPublisher,
    stops: StopSet,
    last: Option<RunOutcome>,
    /// [`MachineBuilder::roles`] at `pemu_build`, which `@report` names.
    roles: Vec<&'static str>,
    doors: SharedDoors,
    /// The only strong reference to the app ELF the walkers resolve through; the registry holds
    /// a weak one, so the bytes are freed with the machine.
    _elf: Option<Arc<crate::introspect::ElfContext>>,
}

// SAFETY: the instance owns its `Send` machine (rule 1), reached only under the handle-table
// lock (rule 3).
unsafe impl Send for Instance {}

impl Instance {
    /// The machine, for a raw call (rule 2).
    pub fn machine(&mut self) -> &mut Machine {
        // SAFETY: rule 1 keeps it alive; `&mut self` under the table lock is the only path.
        unsafe { self.machine.as_mut() }
    }

    pub fn publisher(&self) -> &IoPublisher {
        &self.publisher
    }

    pub fn refresh(&mut self) {
        // SAFETY: as `machine`; the publisher is a separate field, so the borrows do not overlap.
        let m = unsafe { self.machine.as_mut() };
        let now = m.now();
        self.publisher.refresh(m.io(), now);
    }

    pub fn restored_at(&self) -> Option<RestoredAt> {
        lock_doors(&self.doors).restored_at()
    }

    /// Re-wires the published view after a restore and bumps its generation.
    pub fn rebuilt(&mut self) {
        // SAFETY: as `refresh`.
        let m = unsafe { self.machine.as_mut() };
        self.publisher.rebuilt(m.io());
        let now = m.now();
        self.publisher.refresh(m.io(), now);
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let id = self.id;
        // Rule 1: the session goes first. One a registry `stop` already ended is gone, and
        // `destroy` then refuses harmlessly.
        let _ = with_pool(|pool| pool.destroy(id));
        debug_assert!(with_pool(
            |pool| pool.session(id).is_none() && !pool.is_busy(id)
        ));
        // SAFETY: the pointer came from `Box::into_raw` in `build` and nothing else frees it.
        drop(unsafe { Box::from_raw(self.machine.as_ptr()) });
    }
}

#[derive(Default)]
pub struct Table {
    instances: BTreeMap<u32, Instance>,
    next: u32,
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    instances: BTreeMap::new(),
    next: 0,
});

/// Runs `f` with the handle table locked (rule 3). A poisoned lock is recovered: one panicked
/// call must not refuse every later one.
pub fn with_table<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    let mut guard: MutexGuard<'_, Table> = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// The `E_STATE` of a handle that names no machine.
pub fn no_handle(handle: u32) -> ApiError {
    ApiError::new(E_STATE, format!("handle {handle} names no machine"))
        .with_hint("`pemu_build` returns the handle; `pemu_drop` ends it")
}

impl Table {
    pub fn get(&mut self, handle: u32) -> Option<&mut Instance> {
        self.instances.get_mut(&handle)
    }

    pub fn drop_handle(&mut self, handle: u32) -> bool {
        self.instances.remove(&handle).is_some()
    }
}

impl MachineBuilder {
    pub fn new(cfg: &[u8]) -> MachineBuilder {
        MachineBuilder {
            config: crate::config::parse(cfg),
            rom: None,
            flash: None,
            app_elf: None,
            app_elf_bytes: None,
            boot_elf: None,
            efuse: None,
            roles: Vec::new(),
        }
    }

    fn note(&mut self, role: &'static str) {
        if !self.roles.contains(&role) {
            self.roles.push(role);
        }
    }

    /// Parses one asset into the builder; a later load of a kind replaces an earlier one.
    ///
    /// Kind 1 also takes a `.pebundle` (recognized by its magic), the form the web build ships
    /// the demo in: its `flash` payload is the merged image, and its `app_elf`, `boot_elf` and
    /// `efuse` payloads fill those kinds.
    pub fn load(&mut self, kind: u32, bytes: &[u8]) -> Result<(), ApiError> {
        use crate::layout::LoadKind;
        use pemu_loader::bundle::{
            BUNDLE_APP_ELF, BUNDLE_BOOT_ELF, BUNDLE_EFUSE, BUNDLE_FLASH, BUNDLE_MAGIC, Bundle,
        };

        let bad = |what: &str, e: &dyn std::fmt::Display| {
            ApiError::new(E_USAGE, format!("the {what} does not parse: {e}"))
        };
        let Some(kind) = LoadKind::from_u32(kind) else {
            return Err(ApiError::new(
                E_USAGE,
                format!("`pemu_load` kind {kind} is not 0 to 4"),
            ));
        };
        match kind {
            LoadKind::RomElf => {
                self.rom = Some(RomImage::from_elf(bytes).map_err(|e| bad("ROM ELF", &e))?);
                self.note("rom");
            }
            LoadKind::MergedFlash if bytes.starts_with(&BUNDLE_MAGIC) => {
                let bundle = Bundle::parse(bytes).map_err(|e| bad(".pebundle", &e))?;
                let flash = bundle.role_data(BUNDLE_FLASH).ok_or_else(|| {
                    ApiError::new(E_ASSET_MISSING, "the .pebundle carries no `flash` image")
                })?;
                self.flash =
                    Some(FlashImage::from_merged(flash).map_err(|e| bad("bundled flash", &e))?);
                self.note(BUNDLE_FLASH);
                if let Some(elf) = bundle.role_data(BUNDLE_APP_ELF) {
                    self.app_elf = Some(Arc::new(
                        ElfInfo::parse(elf).map_err(|e| bad("bundled app ELF", &e))?,
                    ));
                    self.app_elf_bytes = Some(Arc::from(elf));
                    self.note(BUNDLE_APP_ELF);
                }
                if let Some(elf) = bundle.role_data(BUNDLE_BOOT_ELF) {
                    self.boot_elf = Some(Arc::new(
                        ElfInfo::parse(elf).map_err(|e| bad("bundled bootloader ELF", &e))?,
                    ));
                    self.note(BUNDLE_BOOT_ELF);
                }
                if let Some(dump) = bundle.role_data(BUNDLE_EFUSE) {
                    self.efuse =
                        Some(EfuseImage::from_dump(dump).map_err(|e| bad("bundled eFuse", &e))?);
                    self.note(BUNDLE_EFUSE);
                }
            }
            LoadKind::MergedFlash => {
                self.flash = Some(
                    FlashImage::from_merged(bytes).map_err(|e| bad("merged flash image", &e))?,
                );
                self.note(BUNDLE_FLASH);
            }
            LoadKind::AppElf => {
                self.app_elf = Some(Arc::new(
                    ElfInfo::parse(bytes).map_err(|e| bad("app ELF", &e))?,
                ));
                self.app_elf_bytes = Some(Arc::from(bytes));
                self.note(BUNDLE_APP_ELF);
            }
            LoadKind::BootloaderElf => {
                self.boot_elf = Some(Arc::new(
                    ElfInfo::parse(bytes).map_err(|e| bad("bootloader ELF", &e))?,
                ));
                self.note(BUNDLE_BOOT_ELF);
            }
            LoadKind::Efuse => {
                self.efuse =
                    Some(EfuseImage::from_dump(bytes).map_err(|e| bad("eFuse image", &e))?);
                self.note(BUNDLE_EFUSE);
            }
        }
        Ok(())
    }
}

/// Builds the machine, registers it in the process pool and returns its handle.
pub fn build(builder: MachineBuilder) -> Result<u32, ApiError> {
    use pemu_machine::config::{Assets, EfuseSource};

    let roles = builder.roles;
    let config = builder.config.map_err(|why| ApiError::new(E_USAGE, why))?;
    // The walkers answer in this process from here on: with no app ELF they name the ELF they
    // are missing, instead of `pemu-api`'s "no DWARF definition" error.
    crate::introspect::install();
    let flash = builder.flash.ok_or_else(|| {
        ApiError::new(E_ASSET_MISSING, "no firmware image was loaded").with_hint(
            "`pemu_load` kind 1 takes a merged flash image or a .pebundle before `pemu_build`",
        )
    })?;
    let mut machine_cfg = config.machine.clone();
    machine_cfg.efuse = if builder.efuse.is_some() {
        EfuseSource::Dump
    } else {
        EfuseSource::Synth
    };
    let efuse = builder
        .efuse
        .unwrap_or_else(|| EfuseImage::synth(machine_cfg.seed));
    let assets_app_elf = builder.app_elf.clone();
    let assets = match builder.rom {
        Some(rom) => Assets::new(rom, flash, builder.app_elf, builder.boot_elf, efuse),
        None => Assets::with_bundled_rom(flash, builder.app_elf, builder.boot_elf, efuse).map_err(
            |e| {
                ApiError::new(
                    E_ASSET_MISSING,
                    format!("no pinned ROM for this chip revision: {e}"),
                )
            },
        )?,
    };
    let mut machine = Machine::new(machine_cfg, assets)
        .map_err(|e| ApiError::new(E_USAGE, format!("the machine was not built: {e}")))?;
    if let Some(executor) = config.executor {
        machine.set_executor(executor);
    }
    if let Some(slice) = config.max_slice {
        machine.set_max_slice(slice);
    }
    if let Some(on) = config.rom_delay_ff {
        machine.set_rom_delay_ff(on);
    }
    // Published under the firmware name `inspect::walk_firmware` will hand a walker. No DWARF is
    // parsed here.
    let elf = match (builder.app_elf_bytes, &assets_app_elf) {
        (Some(bytes), Some(elf)) => Some(crate::introspect::register(
            &config.fw,
            Arc::clone(elf),
            bytes,
        )),
        _ => None,
    };
    let publisher = IoPublisher::new(machine.io());
    let machine = NonNull::from(Box::leak(Box::new(machine)));
    let doors = SharedDoors::default();
    let args = StartArgs {
        fw: config.fw.clone(),
        label: config.label.clone(),
        seed: config.machine.seed,
        ..StartArgs::default()
    };
    // Registered under the handle-table lock, like every later use ([`Hosted`]'s invariant).
    with_table(|table| {
        let id = with_pool(|pool| {
            let id = pool.attach(&args, Box::new(Hosted(machine, Arc::clone(&doors))));
            if let Some(state) = pool.table_mut().get_mut(id) {
                // `start` leaves a booted instance `Paused` (`pemu_api` `finish_start`); a built
                // machine has not run, and every time-advancing command accepts `Paused`.
                let _ = state.transition(Lifecycle::Paused, VTime(0));
            }
            id
        });
        table.next = table.next.checked_add(1).unwrap_or(1).max(1);
        let handle = table.next;
        let mut instance = Instance {
            machine,
            id,
            publisher,
            stops: StopSet::default(),
            last: None,
            roles,
            doors,
            _elf: elf,
        };
        instance.refresh();
        table.instances.insert(handle, instance);
        Ok(handle)
    })
}

/// A negative `until_ps` or `max_insns` is no limit of that kind, so a run can be bounded by one
/// alone.
pub fn limits(until_ps: i64, max_insns: i64, stops: StopSet) -> RunLimits {
    RunLimits {
        until: u64::try_from(until_ps).ok().map(VTime),
        max_insns: u64::try_from(max_insns).ok(),
        stops,
    }
}

impl Instance {
    /// Runs the machine with the armed stops and returns the stop code.
    pub fn run(&mut self, until_ps: i64, max_insns: i64) -> u32 {
        let lim = limits(until_ps, max_insns, self.stops.clone());
        let out = self.machine().run(lim);
        let code = stop_code(&out.reason) as u32;
        // Counted as the pool session counts its own commands, so `status` and every receipt
        // agree with the machine.
        let (id, insns) = (self.id, out.insns);
        let deadlock = out.reason == pemu_machine::stops::StopReason::Deadlock;
        with_pool(|pool| {
            if let Some(session) = pool.session_mut(id) {
                session.insns = session.insns.saturating_add(insns);
                session.waiting_for_input = deadlock;
            }
        });
        self.last = Some(out);
        self.refresh();
        code
    }

    /// The `pemu_last_stop` JSON: the stop's name, code and payload, and the `RunOutcome` fields.
    /// Every 64-bit count is a decimal string: virtual picoseconds pass 53 bits after about 2.5 h.
    pub fn last_stop_json(&self) -> Value {
        let Some(out) = &self.last else {
            return json!({ "reason": null });
        };
        let code = stop_code(&out.reason);
        let mut map = serde_json::Map::new();
        map.insert("reason".into(), code.name().into());
        map.insert("code".into(), (code as u32).into());
        map.insert("detail".into(), format!("{:?}", out.reason).into());
        use pemu_machine::stops::StopReason;
        match &out.reason {
            StopReason::Matcher(id) => {
                map.insert("matcher".into(), id.0.into());
            }
            StopReason::Breakpoint(pc) => {
                map.insert("pc".into(), (*pc).into());
            }
            StopReason::Watchpoint { addr, pc } => {
                map.insert("addr".into(), (*addr).into());
                map.insert("pc".into(), (*pc).into());
            }
            _ => {}
        }
        map.insert("vt_ps".into(), out.vt.0.to_string().into());
        map.insert("insns".into(), out.insns.to_string().into());
        map.insert("ff_insns".into(), out.ff_insns.to_string().into());
        map.insert("idle_ps".into(), out.idle_ps.to_string().into());
        Value::Object(map)
    }

    /// Journals a `pemu_input` buffer, fixed-layout records or a JSON array of `{at, event}`.
    /// Everything is decoded first, so a malformed buffer changes nothing; if the machine refuses
    /// an entry (an instant in the past), earlier entries stay and the error names the index.
    pub fn input(&mut self, bytes: &[u8]) -> Result<Value, ApiError> {
        let entries: Vec<(At, InputEvent)> = if crate::input_batch::is_fixed_layout(bytes) {
            crate::input_batch::decode(bytes)
                .map_err(|e| {
                    ApiError::new(E_USAGE, format!("the input batch is malformed: {e:?}"))
                })?
                .into_iter()
                .map(|d| (d.at, d.event))
                .collect()
        } else {
            json_inputs(bytes)?
        };
        let count = entries.len();
        let doors = Arc::clone(&self.doors);
        for (index, (at, event)) in entries.into_iter().enumerate() {
            // A live microphone chunk is journaled in full, so the run becomes `replayable`; a
            // network frame or HCI packet is a bridged peer, so the run becomes `live`; every
            // other event keeps the scripted origin.
            let origin = match event {
                InputEvent::MicChunk { .. } => Origin::UiLive,
                InputEvent::NetFrame { .. } | InputEvent::HciPacket { .. } => Origin::Bridge,
                _ => Origin::Agent,
            };
            journal_input(self.machine(), &doors, Door::Input, at, origin, event).map_err(
                |_| {
                    ApiError::new(
                        E_USAGE,
                        format!("input {index} of {count} was refused by the machine"),
                    )
                    .with_hint(
                        "an input stamped before the machine's virtual time cannot be journaled",
                    )
                },
            )?;
        }
        self.refresh();
        Ok(json!({ "journaled": count }))
    }

    /// Local snapshot bytes for a rewind ring; `flags` must be 0. An export needs the redaction
    /// and secret refusal of the command layer, so it goes through the registry `snapshot`.
    pub fn snapshot(&mut self, flags: u32) -> Result<Vec<u8>, ApiError> {
        if flags != 0 {
            return Err(ApiError::new(
                E_USAGE,
                format!("`pemu_snapshot` flags {flags:#x}: only 0 (a local snapshot) is accepted"),
            )
            .with_hint(
                "export through `pemu_call` with `{\"cmd\":\"snapshot\",...}`, which redacts",
            ));
        }
        self.machine()
            .snapshot(SnapOpts::default())
            .to_bytes()
            .map_err(|e| ApiError::new(E_SNAPSHOT, format!("the snapshot did not encode: {e}")))
    }

    /// Restores snapshot bytes, then re-wires the published view.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), ApiError> {
        let snap = Snapshot::from_bytes(bytes)
            .map_err(|e| ApiError::new(E_SNAPSHOT, format!("the snapshot does not decode: {e}")))?;
        let result = self.machine().restore(&snap);
        if result.is_ok() {
            let doors = Arc::clone(&self.doors);
            lock_doors(&doors).restored(self.machine(), Some(snap.canonical_hash()));
        }
        self.last = None;
        self.rebuilt();
        result.map_err(|e| restore_refusal(&e))
    }
}

/// A live host peer is the instance's state, not the snapshot's, so `SnapError::LiveBridge` is
/// `E_STATE` as the registry maps it; every other refusal is `E_SNAPSHOT`.
fn restore_refusal(e: &SnapError) -> ApiError {
    match e {
        SnapError::LiveBridge { bridge } => ApiError::new(
            E_STATE,
            format!("the snapshot was not restored: {bridge} is attached"),
        )
        .with_hint("detach the live bridge, then restore"),
        other => ApiError::new(E_SNAPSHOT, format!("the snapshot was refused: {other}")),
    }
}

/// Reads the JSON form of `pemu_input`: `[{"at": "now" | <picoseconds>, "event": <InputEvent>}]`.
/// Picoseconds may be a number or a decimal string, since a JavaScript number holds 53 bits.
fn json_inputs(bytes: &[u8]) -> Result<Vec<(At, InputEvent)>, ApiError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| ApiError::new(E_USAGE, format!("the input is not JSON: {e}")))?;
    let items = match value {
        Value::Array(items) => items,
        single @ Value::Object(_) => vec![single],
        _ => {
            return Err(ApiError::new(
                E_USAGE,
                "the input must be an array of {at, event}",
            ));
        }
    };
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let bad = |why: &str| ApiError::new(E_USAGE, format!("input {index}: {why}"));
            let at = match item.get("at") {
                None => At::Now,
                Some(Value::String(s)) if s == "now" => At::Now,
                Some(Value::String(s)) => At::Vt(VTime(
                    s.parse()
                        .map_err(|_| bad("`at` is not \"now\" or picoseconds"))?,
                )),
                Some(Value::Number(n)) => At::Vt(VTime(
                    n.as_u64()
                        .ok_or_else(|| bad("`at` is not a non-negative integer"))?,
                )),
                Some(_) => return Err(bad("`at` is not \"now\" or picoseconds")),
            };
            let event = item.get("event").ok_or_else(|| bad("`event` is missing"))?;
            let event: InputEvent = serde_json::from_value(event.clone())
                .map_err(|e| bad(&format!("`event` is not an InputEvent: {e}")))?;
            Ok((at, event))
        })
        .collect()
}

/// A `pemu_call` request: `{"cmd": <name>, "args": {..}}`; the Worker protocol carries the id
/// outside it (`web/src/api/envelope.ts`).
pub struct Request {
    /// Registry command name, or a reserved `@` name.
    pub cmd: String,
    /// The arguments object; `null` and absent are `{}`.
    pub args: Value,
}

impl Request {
    pub fn parse(bytes: &[u8]) -> Result<Request, ApiError> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|e| ApiError::new(E_USAGE, format!("the call is not JSON: {e}")))?;
        let cmd = value
            .get("cmd")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::new(E_USAGE, "the call needs `cmd`, a command name"))?
            .to_string();
        let args = match value.get("args") {
            None | Some(Value::Null) => json!({}),
            Some(args @ Value::Object(_)) => args.clone(),
            Some(_) => return Err(ApiError::new(E_USAGE, "`args` must be a JSON object")),
        };
        Ok(Request { cmd, args })
    }
}

/// The `CommandOutput` the page decodes (`web/src/api/envelope.ts`).
fn output_json(output: &pemu_api::output::Output) -> Value {
    json!({
        "json": output.json,
        "text": output.text,
        "artifacts": output.artifacts.iter().map(|a| a.to_json()).collect::<Vec<_>>(),
        "receipt": output.receipt.to_json(),
        "vt_us": output.vt_us,
    })
}

/// Runs a registry command on the pool session of `id`, as the daemon's route does: the handle's
/// instance is added to a command that takes one, and a body naming another is refused.
pub fn dispatch(id: InstanceId, request: Request) -> Result<Value, ApiError> {
    let spec = crate::commands::find(&request.cmd).ok_or_else(|| {
        ApiError::new(E_USAGE, format!("`{}` is not a command", request.cmd))
            .with_hint("`status` and the generated command reference list the commands")
    })?;
    if let Some(host) = Host::current()
        && (!host_support::hosts(spec.name).has(host)
            || (host == Host::Browser && spec.annotations.native_only))
    {
        let mut error = ApiError::new(
            E_HOST_UNSUPPORTED,
            format!(
                "`{}` does not run in the {} build",
                spec.name,
                host.as_str()
            ),
        );
        if let Some(hint) = host_support::hint(spec.name) {
            error = error.with_hint(hint);
        }
        return Err(error);
    }
    let mut args = request.args;
    let takes_instance = (spec.input_schema)()
        .as_value()
        .get("properties")
        .and_then(|p| p.get("instance"))
        .is_some();
    if takes_instance && let Some(map) = args.as_object_mut() {
        let named = id.to_string();
        match map.get("instance") {
            None => {
                map.insert("instance".into(), Value::from(named));
            }
            Some(v) if v.as_str() == Some(named.as_str()) => {}
            Some(v) => {
                return Err(ApiError::new(
                    E_USAGE,
                    format!("the handle addresses instance `{named}` but the arguments name {v}"),
                )
                .with_hint("drop `instance` from the arguments; the handle names the instance"));
            }
        }
    }
    let output = (spec.handler)(&mut HandlerCx {}, args)?;
    Ok(output_json(&output))
}

/// Whether `request` puts a snapshot back (`snapshot` op `restore` or its `load` alias). That
/// rewrites every ring in place, so the published view is rebuilt even when the cursors match.
fn restores(request: &Request) -> bool {
    request.cmd == "snapshot"
        && matches!(
            request.args.get("op").and_then(Value::as_str),
            Some("restore" | "load")
        )
}

/// The output cursors a restore can move backwards: the fallback that catches a command which
/// rewinds the machine without being [`restores`].
fn marks(m: &mut Machine) -> [u64; 4] {
    let now = m.now().0;
    let io = m.io();
    [now, io.usj_tx.head(), io.uart0_tx.head(), io.events.head()]
}

/// The reserved request a browser carrier reads the Wi-Fi bridge's outbound window with.
pub const RELAY_REQUEST: &str = "@relay";

/// base64 of `bytes`, standard alphabet with padding, as `atob` reads it. A third the size of a
/// JSON number array; the crate has no base64 dependency and this is 20 lines.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let word = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(word >> (18 - 6 * i)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `@relay` `{cursor?: "<decimal>"}`: the Wi-Fi bridge's outbound window for a carrier outside
/// this process, the read half of the seam `pemu_host::relay_wisp::tick` uses natively. The
/// write half is `pemu_input`'s `InputEvent::NetFrame` with `Origin::Bridge`.
///
/// Answers `{attached, routes, packets, cursor, dropped}`. With no `cursor` it answers no packets
/// and the window's current end: earlier packets belong to no server.
fn relay_window(id: InstanceId, args: &Value) -> Result<Value, ApiError> {
    let cursor = match args.get("cursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.parse::<u64>().map_err(|_| {
            ApiError::new(E_USAGE, "@relay: `cursor` is not a decimal window position")
        })?),
        Some(Value::Number(n)) => Some(n.as_u64().ok_or_else(|| {
            ApiError::new(E_USAGE, "@relay: `cursor` is not a non-negative integer")
        })?),
        Some(_) => {
            return Err(ApiError::new(
                E_USAGE,
                "@relay: `cursor` is a decimal string or a non-negative integer",
            ));
        }
    };
    with_pool(|pool| {
        let session = pool.session_mut(id).ok_or_else(|| {
            ApiError::new(E_STATE, format!("this handle's instance `{id}` is gone"))
        })?;
        let state = pemu_api::commands::net_http::wifi_state(session)?.unwrap_or_default();
        let bridge = &state.lan.bridge;
        let (packets, next) = match cursor {
            Some(cursor) => bridge.since(cursor),
            None => (Vec::new(), bridge.since(u64::MAX).1),
        };
        Ok(json!({
            "attached": bridge.attached,
            "routes": bridge.routes.iter().map(|r| json!({
                "port": r.port,
                "host_port": r.host_port,
            })).collect::<Vec<_>>(),
            "packets": packets.iter().map(|p| Value::from(base64(p))).collect::<Vec<_>>(),
            "cursor": next.to_string(),
            "dropped": bridge.dropped_out.to_string(),
        }))
    })
}

/// Answers `pemu_call` on `handle`: a reserved `@` request, or a registry command (rule 2).
pub fn call(table: &mut Table, handle: u32, bytes: &[u8]) -> Result<Value, ApiError> {
    let request = Request::parse(bytes)?;
    let instance = table.get(handle).ok_or_else(|| no_handle(handle))?;
    // `@relay` reads through the pool session, so it obeys rule 2: the instance borrow ends here.
    if request.cmd == RELAY_REQUEST {
        let id = instance.id;
        return relay_window(id, &request.args);
    }
    if request.cmd.starts_with('@') {
        return instance.reserved(&request);
    }
    let id = instance.id;
    let before = marks(instance.machine());
    let restoring = restores(&request);
    let mic_next = instance.machine().next_live_chunk(LiveStream::Mic);
    // Live microphone chunks reach the journal through `pemu_input`, unseen by the pool session,
    // so its count is raised first: one `seq` sequence per stream, never reused.
    with_pool(|pool| {
        if let Some(session) = pool.session_mut(id) {
            session.mic.next_seq = session.mic.next_seq.max(mic_next);
        }
    });
    // Rule 2: no borrow of the instance survives into the handler.
    let answer = dispatch(id, request);
    // A registry `stop` ends the session; the handle stays until `pemu_drop`.
    if let Some(instance) = table.get(handle) {
        let after = marks(instance.machine());
        let rewound = after.iter().zip(before.iter()).any(|(a, b)| a < b);
        if (restoring && answer.is_ok()) || rewound {
            let doors = Arc::clone(&instance.doors);
            lock_doors(&doors).restored(instance.machine(), None);
            instance.last = None;
            instance.rebuilt();
        } else {
            instance.refresh();
        }
    }
    answer
}

impl Instance {
    /// The reserved `@` requests:
    ///
    /// - `@stops` `{breakpoints: [pc], watches: [{addr, len}], matchers: [{id, serial: {stream,
    ///   contains | prefix | exact}} | {id, event}]}` replaces the stops every later `pemu_run`
    ///   arms; `{}` disarms all.
    /// - `@report` answers `{report, roles}`: the determinism report of the last stop and the
    ///   asset roles `pemu_load` supplied, so a replay can rebuild them.
    /// - `@live` answers `{mic_next_seq, net_next_seq, hci_next_seq}`, the `seq` each live
    ///   stream's next chunk must carry.
    /// - `@journal` `{include_secrets?}` answers the input journal; see [`journal_answer`].
    pub fn reserved(&mut self, request: &Request) -> Result<Value, ApiError> {
        match request.cmd.as_str() {
            "@stops" => {
                let stops = stop_set(&request.args)?;
                stops
                    .check()
                    .map_err(|e| ApiError::new(E_USAGE, e.to_string()))?;
                let armed = json!({
                    "breakpoints": stops.breakpoints.len(),
                    "watches": stops.watches.len(),
                    "matchers": stops.matchers.len(),
                });
                self.stops = stops;
                Ok(json!({ "armed": armed }))
            }
            "@report" => {
                let reason = self
                    .last
                    .as_ref()
                    .map(|out| out.reason.clone())
                    .ok_or_else(|| {
                        ApiError::new(
                            E_STATE,
                            "no run has stopped since the machine was built or restored",
                        )
                    })?;
                let report = pemu_machine::determinism::report(&reason, self.machine());
                Ok(json!({ "report": report, "roles": self.roles }))
            }
            "@live" => {
                let m = self.machine();
                Ok(json!({
                    "mic_next_seq": m.next_live_chunk(LiveStream::Mic).to_string(),
                    "net_next_seq": m.next_live_chunk(LiveStream::Net).to_string(),
                    "hci_next_seq": m.next_live_chunk(LiveStream::Hci).to_string(),
                }))
            }
            "@journal" => {
                let doors = Arc::clone(&self.doors);
                let doors = lock_doors(&doors);
                journal_answer(self.machine(), &doors, &request.args)
            }
            other => Err(ApiError::new(
                E_USAGE,
                format!(
                    "`{other}` is not a reserved request; expected @stops, @report, @live or @journal"
                ),
            )),
        }
    }
}

/// The `@journal` format of a machine never restored; a reader refuses a format it does not know.
pub const JOURNAL_FORMAT: u32 = 1;

/// The `@journal` format after a restore: format 1 plus `from`. Its replay starts from the
/// snapshot, so a format-1 reader refuses it.
pub const JOURNAL_FORMAT_RESTORED: u32 = 2;

/// Whether `entry` is live-bridge data an export drops by default: a microphone, network or HCI
/// chunk whose journal origin is not scripted. The origin is the machine's own record, so a `seq`
/// reused after a restore cannot mislead it; a registry `mic_set` is `Agent` and stays.
fn live_payload(entry: &JournalEntry) -> Option<(&'static str, u64, usize)> {
    if matches!(entry.origin, Origin::Agent | Origin::Scenario) {
        return None;
    }
    match &entry.ev {
        InputEvent::MicChunk { seq, samples } => Some(("MicChunk", *seq, samples.len())),
        InputEvent::NetFrame { seq, data } => Some(("NetFrame", *seq, data.len())),
        InputEvent::HciPacket { seq, data } => Some(("HciPacket", *seq, data.len())),
        _ => None,
    }
}

/// Whether `entry` carries a secret input an export drops whole: the scripted Wi-Fi keys of
/// `EnvChange::WifiAps` and the reader frames of `InputEvent::NfcTap`. Structural, because both
/// render as number arrays the JSON string masker cannot see
/// (`secret_bytes_are_invisible_to_the_value_masker`). Unlike [`live_payload`] the origin does
/// not matter.
fn secret_payload(entry: &JournalEntry) -> Option<(&'static str, u64, usize)> {
    use pemu_core::input::{EnvChange, NfcOp};
    match &entry.ev {
        InputEvent::Env(EnvChange::WifiAps(aps)) if aps.iter().any(|ap| !ap.psk.is_empty()) => {
            Some(("WifiKeys", entry.seq, aps.len()))
        }
        InputEvent::NfcTap { ops } => {
            let frames = ops.iter().filter(|op| matches!(op, NfcOp::Cmd(_))).count();
            (frames > 0).then_some(("NfcFrames", entry.seq, frames))
        }
        _ => None,
    }
}

/// The `@journal` answer: `{format, replayable, dropped: [{kind, seq}], entries: [{at_ps, seq,
/// door, event, dropped?}]}` in `seq` order, `at_ps` and `seq` as decimal strings.
///
/// Live-bridge payloads and secret inputs become a marker unless `include_secrets`: `event` is
/// `null`, `dropped` names `{kind, seq, len}`, and `replayable: false`. After a restore it is
/// format 2 with `from: {vt_ps, cursor, next_seq, snapshot}`, and a replay restores that snapshot
/// and journals only entries at or above `next_seq`.
fn journal_answer(machine: &Machine, doors: &DoorLog, args: &Value) -> Result<Value, ApiError> {
    if let Some(map) = args.as_object()
        && let Some(key) = map.keys().find(|k| k.as_str() != "include_secrets")
    {
        return Err(ApiError::new(
            E_USAGE,
            format!("@journal: `{key}` is not include_secrets"),
        ));
    }
    let include_secrets = match args.get("include_secrets") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(ApiError::new(
                E_USAGE,
                "@journal: `include_secrets` must be a boolean",
            ));
        }
    };
    let journal = machine.journal();
    // `Journal::entries` is delivery order, `(at, seq)`; the export is acceptance order.
    let mut held: Vec<&JournalEntry> = journal.entries().iter().collect();
    held.sort_by_key(|e| e.seq);
    let mut dropped = Vec::new();
    let mut entries = Vec::with_capacity(held.len());
    for entry in held {
        let door = doors.door(entry);
        let mut item = json!({
            "at_ps": entry.at.0.to_string(),
            "seq": entry.seq.to_string(),
            "door": door.as_str(),
        });
        match live_payload(entry)
            .or_else(|| secret_payload(entry))
            .filter(|_| !include_secrets)
        {
            Some((kind, seq, len)) => {
                item["event"] = Value::Null;
                item["dropped"] = json!({ "kind": kind, "seq": seq.to_string(), "len": len });
                dropped.push(json!({ "kind": kind, "seq": seq.to_string() }));
            }
            None => {
                item["event"] = serde_json::to_value(&entry.ev).map_err(|e| {
                    ApiError::new(E_INTERNAL, format!("an input did not encode: {e}"))
                })?;
            }
        }
        entries.push(item);
    }
    let mut answer = json!({
        "format": JOURNAL_FORMAT,
        "replayable": dropped.is_empty(),
        "dropped": dropped,
        "entries": entries,
    });
    if let Some(from) = doors.restored_at() {
        answer["format"] = JOURNAL_FORMAT_RESTORED.into();
        answer["from"] = json!({
            "vt_ps": from.vt.0.to_string(),
            "cursor": from.cursor.to_string(),
            "next_seq": from.next_seq.to_string(),
            "snapshot": from
                .snapshot
                .map(|hash| hash.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        });
    }
    Ok(answer)
}

fn stop_set(args: &Value) -> Result<StopSet, ApiError> {
    use pemu_core::hostio::{EventKind, SerialStream};
    use pemu_machine::stops::{LinePattern, Matcher, MatcherId, Watch};

    let bad = |why: String| ApiError::new(E_USAGE, format!("@stops: {why}"));
    let u32_of = |v: &Value, what: &str| {
        v.as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| bad(format!("`{what}` must be a 32-bit unsigned integer")))
    };
    let list = |key: &str| -> Result<Vec<Value>, ApiError> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => Ok(items.clone()),
            Some(_) => Err(bad(format!("`{key}` must be an array"))),
        }
    };
    if let Some(map) = args.as_object()
        && let Some(key) = map
            .keys()
            .find(|k| !matches!(k.as_str(), "breakpoints" | "watches" | "matchers"))
    {
        return Err(bad(format!(
            "`{key}` is not breakpoints, watches or matchers"
        )));
    }
    let mut set = StopSet::default();
    for pc in list("breakpoints")? {
        set.breakpoints.push(u32_of(&pc, "breakpoints[]")?);
    }
    for w in list("watches")? {
        set.watches.push(Watch {
            addr: u32_of(w.get("addr").unwrap_or(&Value::Null), "watches[].addr")?,
            len: u32_of(w.get("len").unwrap_or(&Value::Null), "watches[].len")?,
        });
    }
    for m in list("matchers")? {
        let id = MatcherId(u32_of(
            m.get("id").unwrap_or(&Value::Null),
            "matchers[].id",
        )?);
        let matcher = if let Some(serial) = m.get("serial") {
            let stream = match serial.get("stream").and_then(Value::as_str) {
                Some("usj") => SerialStream::UsjTx,
                Some("uart0") => SerialStream::Uart0Tx,
                _ => return Err(bad("`serial.stream` must be `usj` or `uart0`".into())),
            };
            let text = |key: &str| serial.get(key).and_then(Value::as_str).map(str::to_string);
            let pattern = if let Some(t) = text("contains") {
                LinePattern::Contains(t)
            } else if let Some(t) = text("prefix") {
                LinePattern::Prefix(t)
            } else if let Some(t) = text("exact") {
                LinePattern::Exact(t)
            } else {
                return Err(bad("`serial` needs `contains`, `prefix` or `exact`".into()));
            };
            Matcher::Serial { stream, pattern }
        } else if let Some(event) = m.get("event") {
            let kind: EventKind = serde_json::from_value(event.clone())
                .map_err(|e| bad(format!("`event` is not an EventKind: {e}")))?;
            Matcher::Event(kind)
        } else {
            return Err(bad("a matcher needs `serial` or `event`".into()));
        };
        set.matchers.push((id, matcher));
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restore_refused_for_a_live_bridge_is_a_state_error_and_the_rest_are_snapshot_errors() {
        let live = restore_refusal(&SnapError::LiveBridge {
            bridge: "live microphone capture".into(),
        });
        assert_eq!(live.code, E_STATE);
        assert!(
            live.message.contains("live microphone capture"),
            "{}",
            live.message
        );
        assert_eq!(restore_refusal(&SnapError::BadMagic).code, E_SNAPSHOT);
    }

    #[test]
    fn relay_packets_are_rfc_4648_base64_with_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // The two characters that separate base64 from base64url.
        assert_eq!(base64(&[0xfb, 0xff, 0xbe]), "+/++");
        // A WISP packet's own header, which is what actually travels.
        assert_eq!(base64(&[0x01, 0x02, 0x00, 0x00, 0x00]), "AQIAAAA=");
    }

    #[test]
    fn a_door_is_matched_on_seq_and_instant_and_unknown_doors_are_input() {
        let entry = |seq: u64, at: u64| JournalEntry {
            at: VTime(at),
            seq,
            origin: pemu_core::journal::Origin::Agent,
            ev: InputEvent::Button {
                id: pemu_core::input::ButtonId::Ok,
                down: true,
            },
        };
        let mut log = DoorLog::default();
        log.doors.insert(4, (VTime(100), Door::Registry));
        assert_eq!(log.door(&entry(4, 100)), Door::Registry);
        assert_eq!(
            log.door(&entry(4, 200)),
            Door::Input,
            "same seq, other instant"
        );
        assert_eq!(
            log.door(&entry(5, 100)),
            Door::Input,
            "a seq this handle never saw"
        );
    }
}
