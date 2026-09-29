//! The HLE dispatcher: hook entry, nested calls, magic returns and observe hooks for the
//! IDF FreeRTOS functions the profiles bind.
//!
//! A handler's state is the module's own type, carried as an opaque [`HandlerState`] and stepped
//! by the module through the object-safe [`HandlerHost`]. This file owns what is common: the
//! guards, the frame arithmetic, the continuation table and the generations.
//! [`HleCore::on_hook`] dispatches in order: magic return, hook PCs, worker entries, tripwires.

use std::collections::BTreeMap;

use crate::binding::BoundHooks;
use crate::continuation::{ContKey, Continuation, Continuations, HandlerState, HleSection, Resume};
use crate::guest_call::{
    A0, CallEngine, CallRequest, GuestView, HleAction, HleError, HleErrorKind, RA, SP,
};
use crate::hooks::{HandlerKind, HookKind, HookRef};
use crate::magic::MagicKind;
use crate::observe::{Observation, ObserveKind};
use crate::tripwire::{TripKind, TripwireSet};
use crate::worker::{
    PD_TRUE, PORT_MAX_DELAY, RadioEvent, WakeEngine, WakeMode, WakeReason, WorkerConfig,
    WorkerState,
};

/// `HandlerState::handler` of the magic ISR handler the core runs itself for a registered U4
/// worker: `bytes` is `[step, worker entry]`.
pub const MAGIC_ISR_HANDLER: &str = "core.magic_isr";

/// Magic ISR step: the `xQueueGiveFromISR` call is outstanding.
const ISR_GIVE: u8 = 0;
/// Magic ISR step: the yield-from-ISR call is outstanding.
const ISR_YIELD: u8 = 1;

/// How a `RadioModule` runs its own handlers. Object safe: [`HleCore`] holds
/// `&mut dyn HandlerHost` and never knows a module's handler types.
pub trait HandlerHost {
    /// Starts the handler that replaces the function hooked with `kind`. Returns its state and
    /// first action.
    fn enter(&mut self, kind: HandlerKind, g: &mut dyn GuestView) -> (HandlerState, HleAction);

    fn resume(
        &mut self,
        state: &mut HandlerState,
        g: &mut dyn GuestView,
        resume: Resume,
    ) -> HleAction;

    /// How a [`HleAction::Call`] should be classified and named, since the action carries
    /// neither. The default treats every call as blocking, the safe side: a blocking call inside
    /// an ISR is refused.
    fn describe(&self, func: u32) -> CallInfo {
        CallInfo {
            name: "nested call",
            blocking: true,
            _func: func,
        }
    }

    /// True when the host wants the queued radio events of a worker handed over at each wake
    /// ([`HandlerHost::deliver`]) rather than drained by its caller through
    /// [`HleCore::next_event`]: a handler resumed with `Resume::Woken` has to make its upcall
    /// with the event in hand, and a run loop only sees the step after it.
    fn takes_events(&self) -> bool {
        false
    }

    /// The events queued for the worker entered at `entry`, moved out just before its handler is
    /// resumed with `Resume::Woken`. Called only when [`HandlerHost::takes_events`] is true.
    fn deliver(&mut self, _entry: MagicKind, _events: Vec<RadioEvent>) {}
}

/// Busy time in microseconds the timing profile (`specs/timing-profiles.toml`) gives a module's
/// handlers: the replaced blob's own work, otherwise done in zero virtual time. All zero under
/// `fast`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BusyDelays {
    pub init_us: u32,
    pub enable_us: u32,
    /// Added to the enable handler of an image that keeps its PHY calibration in NVS.
    pub enable_nvs_cal_us: u32,
    pub disable_us: u32,
    pub deinit_us: u32,
}

/// How one registered `RadioModule` runs its handlers in one machine; `pemu-machine` routes a hook
/// to it by module index, magic entry or continuation handler name. It keeps no guest state
/// between calls: a snapshot carries only the `state` bytes the machine hands in.
pub trait ModuleHost: Send {
    fn module(&self) -> crate::hooks::ModuleIndex;
    /// `RadioModule::name`, the prefix of every `HandlerState::handler` it writes (`ble.worker`).
    fn name(&self) -> &'static str;
    fn magic_entries(&self) -> Vec<MagicKind>;
    /// The workers the core registers for this module, resolved against the bound image, in wake
    /// mode `wake`. The BLE module's handlers allocate the radio interrupt in both modes, as the
    /// controller does on silicon (`ble.toml` `[worker]`).
    fn workers(&mut self, wake: WakeMode) -> Vec<WorkerConfig>;
    /// A module spends it through a nested guest busy-wait, so the guest's own clock sees it; 0
    /// adds no call. The default spends nothing.
    fn set_busy_delays(&mut self, delays: BusyDelays) {
        let _ = delays;
    }
    fn enter(
        &mut self,
        state: &mut Vec<u8>,
        kind: HandlerKind,
        g: &mut dyn GuestView,
    ) -> (HandlerState, HleAction);
    fn resume(
        &mut self,
        state: &mut Vec<u8>,
        handler: &mut HandlerState,
        g: &mut dyn GuestView,
        resume: Resume,
    ) -> HleAction;
    fn describe(&self, func: u32) -> Option<CallInfo>;
    fn deliver(&mut self, state: &mut Vec<u8>, entry: MagicKind, events: Vec<RadioEvent>);
    /// What the receipt reports about the lines this module synthesized.
    fn log_lines(&self, _state: &[u8]) -> Option<crate::binding::RadioLogLines> {
        None
    }
    /// The guest-heap blocks this module holds for the blob it replaced (`inspect heap`, receipt
    /// counts). A module that keeps no ledger answers with nothing.
    fn heap_ledger(&self, _state: &[u8]) -> Vec<crate::binding::HeapBlock> {
        Vec::new()
    }
    /// How many live host bridges this module holds open. A bridged peer answers in host time, so
    /// while this is not 0 pacing is fixed at `Wall { rate: 1 }` and a pause is refused.
    fn bridges_live(&self, _state: &[u8]) -> u32 {
        0
    }
    /// The module-initiated post. A handler that wants to raise an event later schedules a
    /// virtual-time event with the key [`module_timer`] gives it; when it is due the machine calls
    /// this and posts every returned event to the module's worker through [`HleCore::post`]. The
    /// default posts nothing.
    fn on_timer(
        &mut self,
        _state: &mut Vec<u8>,
        _tag: u16,
        _g: &mut dyn GuestView,
    ) -> Vec<RadioEvent> {
        Vec::new()
    }
    /// A journaled input for this module, applied at its journal instant; returned events are
    /// posted as for [`ModuleHost::on_timer`]. An `Err` is a refused input, counted as unapplied.
    /// The default refuses every input.
    fn on_input(
        &mut self,
        _state: &mut Vec<u8>,
        _payload: &[u8],
        _g: &mut dyn GuestView,
    ) -> Result<Vec<RadioEvent>, HleError> {
        Err(HleError::new(
            HleErrorKind::Handler,
            format!("the {} module takes no input", self.name()),
        ))
    }
    /// A journaled event of the external HCI bridge, handled like [`ModuleHost::on_input`]. A door
    /// of its own because it carries a host peer that is not part of the run's identity, so a
    /// journal viewer can tell a scripted input from a bridged one without decoding either. The
    /// default refuses.
    fn on_hci(
        &mut self,
        _state: &mut Vec<u8>,
        _ev: HciInput<'_>,
        _g: &mut dyn GuestView,
    ) -> Result<Vec<RadioEvent>, HleError> {
        Err(HleError::new(
            HleErrorKind::Handler,
            format!("the {} module has no external HCI bridge", self.name()),
        ))
    }
    /// A journaled event of the Wi-Fi bridge, kept apart from [`ModuleHost::on_input`] for the
    /// same reason as [`ModuleHost::on_hci`]. The default refuses.
    fn on_net(
        &mut self,
        _state: &mut Vec<u8>,
        _ev: NetInput<'_>,
        _g: &mut dyn GuestView,
    ) -> Result<Vec<RadioEvent>, HleError> {
        Err(HleError::new(
            HleErrorKind::Handler,
            format!("the {} module has no network bridge", self.name()),
        ))
    }
}

/// One journaled event of the Wi-Fi bridge ([`ModuleHost::on_net`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NetInput<'a> {
    /// Attached with these allowlisted routes (`EnvChange::WifiBridge`).
    Attach {
        routes: &'a [pemu_core::input::NetRoute],
    },
    /// The bridge detached.
    Detach,
    /// One packet from the relay's server side (`InputEvent::NetFrame`); `seq` lets the module
    /// count a gap.
    Packet {
        /// Position in the stream.
        seq: u64,
        data: &'a [u8],
    },
}

/// One journaled event of the external HCI bridge ([`ModuleHost::on_hci`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HciInput<'a> {
    /// From now on the guest's host-to-controller packets go to the external controller and the
    /// virtual one stops answering (`EnvChange::BleHciBridge`).
    Attach,
    Detach,
    /// One packet from the external controller (`InputEvent::HciPacket`). `seq` counts from 0 per
    /// session, so the module can see that the stream dropped data.
    Packet {
        seq: u64,
        data: &'a [u8],
    },
}

/// The scheduler key of a module timer with `tag`, owned by the radio whose id is the module index.
pub fn module_timer(module: crate::hooks::ModuleIndex, tag: u16) -> pemu_core::sched::EventKey {
    pemu_core::sched::EventKey {
        owner: pemu_core::sched::Owner::Radio(pemu_core::sched::RadioId(u16::from(module.0))),
        tag,
    }
}

/// Whether [`HleCore::add_worker`] accepts `config`, so a machine can refuse the module before it
/// builds the core: a U4 profile without an interrupt source is refused.
pub fn check_worker(config: &WorkerConfig) -> Result<(), HleError> {
    if config.profile.wake == WakeMode::U4MagicIsr && config.profile.isr_source.is_none() {
        return Err(HleError::new(
            HleErrorKind::Handler,
            format!(
                "worker {} is U4 but its profile names no interrupt source",
                config.profile.task_name
            ),
        ));
    }
    Ok(())
}

/// What the host says about a nested-call target.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CallInfo {
    pub name: &'static str,
    /// False only for a `FromISR` call, the only kind legal inside an ISR.
    pub blocking: bool,
    pub _func: u32,
}

/// What the run loop does next after the HLE handled a hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Resume the guest at `pc`; the registers are already set.
    Resume {
        /// Where to resume.
        pc: u32,
    },
    /// The handler returned: the hook frame is restored and `pc` is the saved `ra`.
    Returned {
        /// The saved `ra`.
        pc: u32,
        a0: u32,
        a1: u32,
    },
    /// A handler parked with no registered worker behind it; its continuation waits for
    /// [`HleCore::wake`]. A registered worker's park is a nested wait call instead.
    Parked,
    /// An observe hook ran; the engine must now execute the original instruction once
    /// (`Engine::run_hooked_once`).
    Observed(Observation),
    /// A tripwire fired: the run stops with `E_TRIPWIRE` naming the symbol.
    Tripped {
        kind: TripKind,
        /// The symbol and caller the diagnostic names.
        detail: String,
    },
}

/// The HLE state of one machine: the nested-call engine, the snapshot section, the bound hooks
/// and the armed tripwires.
pub struct HleCore {
    pub engine: CallEngine,
    pub section: HleSection,
    pub bound: BoundHooks,
    pub tripwires: TripwireSet,
    /// Registered workers by magic entry: configuration derived from binding, never snapshotted.
    workers: BTreeMap<u8, WorkerConfig>,
    /// The continuation of the running handler. It moves into `section.continuations` for the
    /// duration of each nested call.
    active: Option<(ContKey, Continuation)>,
}

impl HleCore {
    pub fn new(engine: CallEngine, bound: BoundHooks) -> HleCore {
        let section = HleSection {
            binding: bound.record.clone(),
            ..HleSection::default()
        };
        HleCore {
            engine,
            section,
            tripwires: bound.tripwires.clone(),
            bound,
            workers: BTreeMap::new(),
            active: None,
        }
    }

    /// Registers a worker: its park becomes the profile's semaphore wait and, in U4, the core runs
    /// its magic ISR. A U4 profile without an interrupt source is refused: which source BLE uses
    /// is UNVERIFIED, and the core never raises one on a guess.
    pub fn add_worker(&mut self, config: WorkerConfig) -> Result<(), HleError> {
        check_worker(&config)?;
        self.workers.insert(config.profile.entry as u8, config);
        Ok(())
    }

    /// The registration of the worker entered at `entry`, where the machine reads the interrupt
    /// source a post raises.
    pub fn worker_config(&self, entry: MagicKind) -> Option<&WorkerConfig> {
        self.workers.get(&(entry as u8))
    }

    pub fn worker(&self, entry: MagicKind) -> Option<&WorkerState> {
        self.section.workers.get(&(entry as u8))
    }

    /// In U4 the core raises the profile's interrupt source; in U5 the next poll finds the event.
    pub fn post(
        &mut self,
        g: &mut dyn GuestView,
        entry: MagicKind,
        event: RadioEvent,
    ) -> Result<(), HleError> {
        let config = self.workers.get(&(entry as u8)).ok_or_else(|| {
            HleError::new(
                HleErrorKind::Handler,
                format!("an event was posted to {entry:?}, which has no registered worker"),
            )
        })?;
        let state = self
            .section
            .workers
            .entry(entry as u8)
            .or_insert_with(|| WorkerState {
                wake: WakeEngine::new(config.profile.wake),
                ..WorkerState::default()
            });
        state.wake.post(event);
        if let (WakeMode::U4MagicIsr, Some(source)) = (state.wake.mode(), config.profile.isr_source)
        {
            g.raise(source, state.wake.level());
        }
        Ok(())
    }

    /// The next queued event of the worker entered at `entry`, drained by its handler after a
    /// `Resume::Woken`.
    pub fn next_event(&mut self, entry: MagicKind) -> Option<RadioEvent> {
        self.section
            .workers
            .get_mut(&(entry as u8))?
            .wake
            .next_event()
    }

    /// Asks the worker entered at `entry` to exit: its next wake is `WakeReason::Deinit`, and its
    /// next wait does not sleep.
    pub fn request_deinit(&mut self, entry: MagicKind) {
        if let Some(state) = self.section.workers.get_mut(&(entry as u8)) {
            state.deinit = true;
        }
    }

    pub fn on_hook(
        &mut self,
        g: &mut dyn GuestView,
        host: &mut dyn HandlerHost,
        pc: u32,
        hook: HookRef,
    ) -> Result<Step, HleError> {
        match hook.kind {
            HookKind::Magic(MagicKind::Return) => self.on_magic_return(g, host),
            HookKind::Magic(kind) if kind.is_isr() => self.on_magic_isr(g, host, kind),
            HookKind::Magic(kind) => self.on_magic_worker(g, host, kind),
            HookKind::Hle(kind) => self.on_hle_hook(g, host, kind),
            HookKind::Observe(kind) => Ok(Step::Observed(self.on_observe(g, kind))),
            HookKind::Tripwire(kind) => Ok(self.tripped(pc, kind)),
            HookKind::FastForward(_) | HookKind::Breakpoint => Ok(Step::Resume { pc }),
        }
    }

    /// The `E_TRIPWIRE` stop, naming the symbol when one is armed at `pc`.
    fn tripped(&self, pc: u32, kind: TripKind) -> Step {
        let detail = match self.tripwires.at(pc) {
            Some((_, symbol)) => format!("{symbol} at {pc:#010x}"),
            None => match kind {
                TripKind::MagicRangeFetch => {
                    format!("fetch of the unallocated magic PC {pc:#010x}")
                }
                _ => format!("{pc:#010x}"),
            },
        };
        Step::Tripped { kind, detail }
    }

    /// Makes a handler entered at the current hook the active one: its frame, return address and
    /// key.
    fn activate(
        &mut self,
        g: &mut dyn GuestView,
        frame: [u32; 32],
        handler: HandlerState,
        magic_isr: bool,
    ) {
        let cont = Continuation {
            handler,
            frame,
            ra: frame[usize::from(RA)],
            step: 0,
            task: g.current_task(),
            in_isr: g.in_isr(),
            scratch_len: 0,
            func: 0,
            sp0: frame[usize::from(SP)],
            wait: false,
            magic_isr,
        };
        let key = ContKey {
            sp: frame[usize::from(SP)],
            generation: self.section.generation_for(cont.task, cont.in_isr),
        };
        self.active = Some((key, cont));
    }

    /// A hooked function entry: the handler starts and its first action is carried out.
    fn on_hle_hook(
        &mut self,
        g: &mut dyn GuestView,
        host: &mut dyn HandlerHost,
        kind: HandlerKind,
    ) -> Result<Step, HleError> {
        let frame = save_frame(g);
        let (state, action) = host.enter(kind, g);
        self.activate(g, frame, state, false);
        self.carry_out(g, host, action)
    }

    /// A magic worker entry: a task created with a magic entry starts here on its own stack. For a
    /// registered worker the core records the task and the semaphore, which arrives in `a0` as
    /// `pvParameters`. The handler then runs as it does at a hook.
    fn on_magic_worker(
        &mut self,
        g: &mut dyn GuestView,
        host: &mut dyn HandlerHost,
        kind: MagicKind,
    ) -> Result<Step, HleError> {
        if let Some(config) = self.workers.get(&(kind as u8)) {
            let task = g.current_task();
            let semaphore = g.reg(A0);
            let state = self
                .section
                .workers
                .entry(kind as u8)
                .or_insert_with(|| WorkerState {
                    wake: WakeEngine::new(config.profile.wake),
                    ..WorkerState::default()
                });
            state.task = task;
            state.semaphore = semaphore;
        }
        self.on_hle_hook(g, host, worker_handler(kind))
    }

    /// A magic ISR entry: `isr_generation` is bumped first and becomes the innermost nesting
    /// level, so a continuation started here is never taken for one of the interrupted task.
    ///
    /// For a registered U4 worker the core runs the ISR itself: the event moves in flight,
    /// `xQueueGiveFromISR(semaphore, &woken)` is called, then the port's yield-from-ISR function
    /// when `woken` came back set, then the ISR returns and the core lowers the level. Any other
    /// magic ISR is the module's handler.
    fn on_magic_isr(
        &mut self,
        g: &mut dyn GuestView,
        host: &mut dyn HandlerHost,
        kind: MagicKind,
    ) -> Result<Step, HleError> {
        self.section.enter_isr();
        let frame = save_frame(g);
        let registered = self
            .workers
            .iter()
            .find(|(_, config)| config.profile.isr_entry == kind)
            .map(|(entry, config)| (*entry, config.clone()));
        let Some((entry, config)) = registered else {
            let (state, action) = host.enter(worker_handler(kind), g);
            self.activate(g, frame, state, true);
            return self.carry_out(g, host, action);
        };
        let state = self
            .section
            .workers
            .entry(entry)
            .or_insert_with(|| WorkerState {
                wake: WakeEngine::new(config.profile.wake),
                ..WorkerState::default()
            });
        state.wake.on_isr_entry();
        let semaphore = state.semaphore;
        let handler = HandlerState {
            handler: MAGIC_ISR_HANDLER.to_string(),
            bytes: vec![ISR_GIVE, entry],
        };
        self.activate(g, frame, handler, true);
        if semaphore == 0 {
            // The worker has not started, so there is nothing to give. The event stays queued,
            // and the worker's first park does not sleep while one is (`crate::worker`).
            return Ok(self.finish(g, 0, 0));
        }
        self.start_call(g, config.calls.give_from_isr(semaphore), false)
    }

    fn magic_isr_step(&mut self, g: &mut dyn GuestView, scratch: &[u8]) -> Result<Step, HleError> {
        let (key, mut cont) = self.take_active()?;
        let (step, entry) = match cont.handler.bytes[..] {
            [step, entry] => (step, entry),
            _ => {
                return Err(HleError::new(
                    HleErrorKind::Handler,
                    "the magic ISR handler state is not [step, entry]",
                ));
            }
        };
        let config = self.workers.get(&entry).cloned();
        if step == ISR_GIVE {
            let woken = scratch
                .get(..4)
                .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
                .unwrap_or(0)
                != 0;
            if let Some(state) = self.section.workers.get_mut(&entry) {
                state.wake.set_woken(woken);
            }
            if let (true, Some(config)) = (woken, config) {
                cont.handler.bytes[0] = ISR_YIELD;
                self.active = Some((key, cont));
                return self.start_call(g, config.calls.request_yield(), false);
            }
        }
        self.active = Some((key, cont));
        Ok(self.finish(g, 0, 0))
    }

    /// An observe hook: it runs, then the engine executes the original instruction.
    /// `vTaskDelete` drops the deleted task's continuations, bumps its generation and forgets it
    /// as a worker, dropping its queued events and, in U4, lowering its radio source, so a later
    /// init's worker neither receives them nor takes an interrupt for them.
    pub fn on_observe(&mut self, g: &mut dyn GuestView, kind: ObserveKind) -> Observation {
        let task = g.current_task();
        if kind != ObserveKind::TaskDelete {
            return Observation {
                kind,
                task,
                dropped: 0,
                generation: None,
            };
        }
        // vTaskDelete(NULL) deletes the calling task (FreeRTOS), which is what a worker's own
        // deinit path uses; a0 otherwise carries the handle of the task to delete.
        let handle = g.reg(A0);
        let deleted = if handle == 0 { task } else { handle };
        let (generation, dropped) = self.section.delete_task(deleted);
        for (entry, state) in &mut self.section.workers {
            if state.task == deleted {
                state.task = 0;
                state.semaphore = 0;
                let lowered = state.wake.lowered();
                state.wake = WakeEngine::new(state.wake.mode());
                state.wake.restore_lowered(lowered);
                if state.wake.mode() == WakeMode::U4MagicIsr
                    && let Some(source) = self
                        .workers
                        .get(entry)
                        .and_then(|config| config.profile.isr_source)
                {
                    g.raise(source, false);
                }
            }
        }
        Observation {
            kind,
            task: deleted,
            dropped,
            generation: Some(generation),
        }
    }

    /// The magic return: the continuation under `(sp, generation)` is found, its guards are
    /// checked and its handler is resumed. The return of a park wait resumes with
    /// `Resume::Woken`; a call of the core's magic ISR handler is stepped by the core.
    pub fn on_magic_return(
        &mut self,
        g: &mut dyn GuestView,
        host: &mut dyn HandlerHost,
    ) -> Result<Step, HleError> {
        let sp = g.reg(SP);
        let in_isr = g.in_isr();
        let task = g.current_task();
        let generation = self.section.generation_for(task, in_isr);
        let key = ContKey { sp, generation };
        let Some(mut cont) = self.section.continuations.take(key) else {
            return Err(self.explain_missing(sp, generation, task, in_isr));
        };
        if cont.in_isr != in_isr {
            return Err(HleError::new(
                HleErrorKind::TaskMismatch,
                format!(
                    "the continuation at sp {sp:#010x} was started {}, and the magic return is {}",
                    context_word(cont.in_isr),
                    context_word(in_isr)
                ),
            ));
        }
        if !in_isr && cont.task != task {
            return Err(HleError::new(
                HleErrorKind::TaskMismatch,
                format!(
                    "the continuation at sp {sp:#010x} belongs to task {:#010x}, and task \
                     {task:#010x} returned into it",
                    cont.task
                ),
            ));
        }
        let a0 = g.reg(A0);
        let a1 = g.reg(A0 + 1);
        let scratch = self
            .engine
            .read_scratch(g, sp, usize::from(cont.scratch_len))?;
        g.set_reg(SP, cont.sp0);
        cont.step += 1;
        if cont.magic_isr && cont.handler.handler == MAGIC_ISR_HANDLER {
            self.active = Some((key, cont));
            return self.magic_isr_step(g, &scratch);
        }
        let resume = if std::mem::take(&mut cont.wait) {
            let reason = self.wake_reason(cont.task, a0);
            if host.takes_events() {
                self.hand_over_events(host, cont.task);
            }
            Resume::Woken { reason }
        } else {
            Resume::Returned { a0, a1, scratch }
        };
        let action = host.resume(&mut cont.handler, g, resume);
        self.active = Some((key, cont));
        self.carry_out(g, host, action)
    }

    /// Moves the queued events of the worker whose task is `task` to the host
    /// ([`HandlerHost::takes_events`]).
    fn hand_over_events(&mut self, host: &mut dyn HandlerHost, task: u32) {
        let Some((entry, state)) = self
            .section
            .workers
            .iter_mut()
            .find(|(_, state)| state.task == task)
        else {
            return;
        };
        let mut events = Vec::new();
        while let Some(event) = state.wake.next_event() {
            events.push(event);
        }
        if events.is_empty() {
            return;
        }
        let entry = MagicKind::ALL
            .into_iter()
            .find(|kind| *kind as u8 == *entry);
        if let Some(entry) = entry {
            host.deliver(entry, events);
        }
    }

    fn wake_reason(&mut self, task: u32, a0: u32) -> WakeReason {
        match self.section.workers.values_mut().find(|s| s.task == task) {
            Some(state) if state.deinit => WakeReason::Deinit,
            Some(state) if a0 == PD_TRUE || state.wake.unseen() => {
                state.wake.mark_seen();
                WakeReason::Event
            }
            _ if a0 == PD_TRUE => WakeReason::Event,
            _ => WakeReason::Timeout,
        }
    }

    /// Why `(sp, generation)` held no continuation: something is there under another generation
    /// (the generation guard firing), or nothing is.
    fn explain_missing(&self, sp: u32, generation: u64, task: u32, in_isr: bool) -> HleError {
        match self.section.continuations.find_by_sp(sp) {
            Some((key, cont)) => HleError::new(
                HleErrorKind::GenerationMismatch,
                format!(
                    "the continuation at sp {sp:#010x} keys on generation {}, and {} is now at \
                     generation {generation}",
                    key.generation,
                    if cont.in_isr {
                        "the ISR".to_string()
                    } else {
                        format!("task {:#010x}", cont.task)
                    }
                ),
            ),
            None => HleError::new(
                HleErrorKind::UnknownContinuation,
                format!(
                    "magic return {} at sp {sp:#010x} generation {generation} has no \
                     continuation (task {task:#010x})",
                    context_word(in_isr)
                ),
            ),
        }
    }

    /// Wakes a handler parked with no registered worker. A registered worker is woken by its own
    /// wait instead.
    pub fn wake(
        &mut self,
        g: &mut dyn GuestView,
        host: &mut dyn HandlerHost,
        key: ContKey,
        reason: WakeReason,
    ) -> Result<Step, HleError> {
        let Some(mut cont) = self.section.continuations.take(key) else {
            return Err(HleError::new(
                HleErrorKind::UnknownContinuation,
                format!(
                    "no parked handler at sp {:#010x} generation {}",
                    key.sp, key.generation
                ),
            ));
        };
        cont.step += 1;
        let action = host.resume(&mut cont.handler, g, Resume::Woken { reason });
        self.active = Some((key, cont));
        self.carry_out(g, host, action)
    }

    fn carry_out(
        &mut self,
        g: &mut dyn GuestView,
        host: &mut dyn HandlerHost,
        action: HleAction,
    ) -> Result<Step, HleError> {
        match action {
            HleAction::Call {
                func,
                args,
                nargs,
                scratch,
            } => {
                let info = host.describe(func);
                let call = CallRequest {
                    func,
                    args,
                    nargs,
                    scratch,
                    blocking: info.blocking,
                    name: info.name,
                };
                self.start_call(g, call, false)
            }
            HleAction::Return { a0, a1 } => Ok(self.finish(g, a0, a1)),
            HleAction::Park => {
                let (key, cont) = self.take_active()?;
                match self.park_call(&cont, g) {
                    Some(call) => {
                        self.active = Some((key, cont));
                        self.start_call(g, call, true)
                    }
                    None => {
                        self.section.continuations.insert(key, cont);
                        Ok(Step::Parked)
                    }
                }
            }
            HleAction::Fail(err) => {
                self.active = None;
                Err(err)
            }
        }
    }

    /// The wait a registered worker's park becomes, or `None` when the parking task is no started
    /// registered worker. The wait is blocking, so a park from ISR context is refused.
    fn park_call(&self, cont: &Continuation, g: &mut dyn GuestView) -> Option<CallRequest> {
        let (entry, state) = self
            .section
            .workers
            .iter()
            .find(|(_, state)| state.task == cont.task && state.semaphore != 0)?;
        let config = self.workers.get(entry)?;
        let ticks = if state.deinit || state.wake.unseen() {
            0
        } else {
            match state.wake.mode() {
                WakeMode::U5Polling => config.profile.poll_ticks_for(g.tick_hz()),
                WakeMode::U4MagicIsr => PORT_MAX_DELAY,
            }
        };
        Some(config.calls.park(state.semaphore, ticks))
    }

    /// The scratch limit of a call made by `cont`: its worker's own when the task is a registered
    /// worker with one and the call is from task context, the guard profile's otherwise.
    fn scratch_limit(&self, cont: &Continuation) -> u16 {
        let default = self.engine.guards.max_scratch;
        if cont.in_isr || cont.task == 0 {
            return default;
        }
        self.section
            .workers
            .iter()
            .find(|(_, state)| state.task == cont.task)
            .and_then(|(entry, _)| self.workers.get(entry))
            .and_then(|config| config.profile.max_scratch)
            .unwrap_or(default)
    }

    /// Sets up one nested guest call: every guard of `CallEngine::prepare`, then the frame.
    /// `wait` marks the core's park wait (`Continuation::wait`).
    fn start_call(
        &mut self,
        g: &mut dyn GuestView,
        call: CallRequest,
        wait: bool,
    ) -> Result<Step, HleError> {
        let (key, mut cont) = self.take_active()?;
        // The frame is taken from the hook frame's `sp`, not the guest's current one: a hook's
        // calls run one after another and a hook does not fire while a call is outstanding
        // (`specs/notes/g3-behavior.md` g3-hook-api).
        g.set_reg(SP, cont.sp0);
        let max_scratch = self.scratch_limit(&cont);
        let frame = match self.engine.prepare_with_scratch_limit(
            g,
            &self.section.continuations,
            key.generation,
            &call,
            max_scratch,
        ) {
            Ok(frame) => frame,
            Err(err) => {
                // A refused call leaves the guest as it was.
                g.set_reg(SP, cont.sp0);
                self.active = None;
                return Err(err);
            }
        };
        let pc = self.engine.start(g, &frame, &call)?;
        cont.func = call.func;
        cont.scratch_len = call.scratch.len() as u16;
        cont.wait = wait;
        self.section.continuations.insert(frame.key, cont);
        Ok(Step::Resume { pc })
    }

    /// The handler returned: the 31 saved GPRs go back, `a0` and `a1` carry the result and the
    /// guest resumes at the saved `ra`. The return of a magic ISR also leaves its nesting level,
    /// and for the core's own magic ISR handler it is where the core lowers the radio source
    /// level, once per event.
    fn finish(&mut self, g: &mut dyn GuestView, a0: u32, a1: u32) -> Step {
        let Some((_, cont)) = self.active.take() else {
            return Step::Returned { pc: 0, a0, a1 };
        };
        for (r, value) in cont.frame.iter().enumerate().skip(1) {
            g.set_reg(r as u8, *value);
        }
        g.set_reg(A0, a0);
        g.set_reg(A0 + 1, a1);
        if cont.magic_isr {
            self.section.leave_isr();
            if let (MAGIC_ISR_HANDLER, &[_, entry]) =
                (cont.handler.handler.as_str(), &cont.handler.bytes[..])
                && let Some(state) = self.section.workers.get_mut(&entry)
            {
                state.wake.on_isr_return();
                if let Some(source) = self
                    .workers
                    .get(&entry)
                    .and_then(|config| config.profile.isr_source)
                {
                    g.raise(source, state.wake.level());
                }
            }
        }
        Step::Returned {
            pc: cont.ra,
            a0,
            a1,
        }
    }

    /// The handler that is running right now, or the internal error of a run loop that dispatched
    /// an action with none active.
    fn take_active(&mut self) -> Result<(ContKey, Continuation), HleError> {
        self.active.take().ok_or_else(|| {
            HleError::new(
                HleErrorKind::UnknownContinuation,
                "an HLE action was carried out with no handler active",
            )
        })
    }

    pub fn outstanding(&self) -> usize {
        self.section.continuations.len()
    }

    /// Drops everything that names a guest stack frame or a task identity, which a chip reset
    /// destroys. The binding record, tripwires, worker configuration and user hooks are image and
    /// host state and stay.
    ///
    /// A stale continuation would refuse the next boot's first blocking nested call at the same
    /// stack pointer with [`HleErrorKind::KeyCollision`]: on the corpus `pk` a monitor reset then
    /// died inside BLE init (`t1_m10_monitor_reset_banners_of_a_running_pk`).
    pub fn on_chip_reset(&mut self) {
        self.active = None;
        self.section.continuations = Continuations::default();
        self.section.task_generations.clear();
        self.section.isr_generation = 0;
        self.section.isr_nesting.clear();
    }
}

fn context_word(in_isr: bool) -> &'static str {
    if in_isr {
        "in an ISR"
    } else {
        "in task context"
    }
}

/// Handler number reserved for each magic entry, so a worker and an ISR are ordinary handlers of
/// their module. The top of the `HandlerKind` space, which a module numbers upward from 0.
pub fn worker_handler(kind: MagicKind) -> HandlerKind {
    HandlerKind(u16::MAX - kind as u16)
}

/// The 32 integer registers at hook entry. `x0` is always 0.
fn save_frame(g: &dyn GuestView) -> [u32; 32] {
    let mut frame = [0u32; 32];
    for (r, slot) in frame.iter_mut().enumerate().skip(1) {
        *slot = g.reg(r as u8);
    }
    frame
}

#[cfg(test)]
pub(crate) mod script {
    //! A `HandlerHost` that replays a fixed list of actions per handler, so a test can write a
    //! synthetic guest program without a radio module.

    use super::*;
    use std::collections::BTreeMap;

    /// One scripted handler: the actions it returns, in order.
    pub(crate) struct ScriptHost {
        pub scripts: BTreeMap<u16, Vec<HleAction>>,
        pub calls: BTreeMap<u32, CallInfo>,
        /// Every `Resume` the host saw, newest last.
        pub resumes: Vec<Resume>,
    }

    impl ScriptHost {
        pub fn new(kind: HandlerKind, actions: Vec<HleAction>) -> ScriptHost {
            ScriptHost {
                scripts: BTreeMap::from([(kind.0, actions)]),
                calls: BTreeMap::new(),
                resumes: Vec::new(),
            }
        }

        pub fn with_call(mut self, func: u32, name: &'static str, blocking: bool) -> ScriptHost {
            self.calls.insert(
                func,
                CallInfo {
                    name,
                    blocking,
                    _func: func,
                },
            );
            self
        }

        fn action(&self, state: &HandlerState) -> HleAction {
            let kind: u16 = state.handler.parse().expect("handler number");
            let step = usize::from(state.bytes[0]);
            self.scripts[&kind]
                .get(step)
                .cloned()
                .unwrap_or(HleAction::Return { a0: 0, a1: 0 })
        }
    }

    impl HandlerHost for ScriptHost {
        fn enter(
            &mut self,
            kind: HandlerKind,
            _g: &mut dyn GuestView,
        ) -> (HandlerState, HleAction) {
            let state = HandlerState {
                handler: kind.0.to_string(),
                bytes: vec![0],
            };
            self.resumes.push(Resume::Entry);
            let action = self.action(&state);
            (state, action)
        }

        fn resume(
            &mut self,
            state: &mut HandlerState,
            _g: &mut dyn GuestView,
            resume: Resume,
        ) -> HleAction {
            self.resumes.push(resume);
            state.bytes[0] += 1;
            self.action(state)
        }

        fn describe(&self, func: u32) -> CallInfo {
            self.calls.get(&func).copied().unwrap_or(CallInfo {
                name: "nested call",
                blocking: true,
                _func: func,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::script::ScriptHost;
    use super::*;
    use crate::binding::{BindingRecord, BoundHooks};
    use crate::guest_call::{Arg, CallEngine, GuardProfile};
    use crate::magic::MagicPcs;
    use crate::test_guest::{DATA, ISR_STACK_BYTES, SynthGuest};
    use pemu_core::snap::SnapSection;

    const HOOK_PC: u32 = 0x4201_71D6;
    const HOOK_RA: u32 = 0x4201_7000;
    const SEM_TAKE: u32 = 0x4039_1000;
    const GIVE_FROM_ISR: u32 = 0x4039_0F6A;
    const TCB_A: u32 = DATA + 0x1000;
    const TCB_B: u32 = DATA + 0x1800;
    const STACK_A: u32 = DATA + 0x4000;
    const STACK_B: u32 = DATA + 0x8000;
    const HANDLER: HandlerKind = HandlerKind(1);

    fn guest() -> SynthGuest {
        let mut g = SynthGuest::new();
        g.with_task(&GuardProfile::default(), TCB_A, STACK_A, STACK_A + 0x2000);
        g.set_reg(RA, HOOK_RA);
        g.set_reg(8, 0xF00D_1234); // s0, so the frame restore is visible
        g
    }

    fn core(g: &SynthGuest) -> HleCore {
        let engine = CallEngine {
            pcs: MagicPcs::from_spec().expect("spec"),
            guards: GuardProfile::default(),
            stack: g.stack_symbols(),
        };
        HleCore::new(engine, BoundHooks::default())
    }

    fn hook() -> HookRef {
        HookRef {
            kind: HookKind::Hle(HANDLER),
            module: crate::hooks::ModuleIndex::FIRST_MODULE,
        }
    }

    fn call(func: u32, args: &[Arg], scratch: Vec<u8>) -> HleAction {
        let mut slots = [Arg::Val(0); 8];
        slots[..args.len()].copy_from_slice(args);
        HleAction::Call {
            func,
            args: slots,
            nargs: args.len() as u8,
            scratch,
        }
    }

    #[test]
    fn a_nested_call_runs_and_its_return_finishes_the_handler() {
        let mut g = guest();
        let mut core = core(&g);
        let mut host = ScriptHost::new(
            HANDLER,
            vec![
                call(SEM_TAKE, &[Arg::Val(0x99)], Vec::new()),
                HleAction::Return { a0: 7, a1: 8 },
            ],
        )
        .with_call(SEM_TAKE, "xQueueSemaphoreTake", true);

        let sp0 = g.reg(SP);
        let step = core
            .on_hook(&mut g, &mut host, HOOK_PC, hook())
            .expect("hook");
        assert_eq!(step, Step::Resume { pc: SEM_TAKE });
        assert_eq!(core.outstanding(), 1, "the call is outstanding");
        assert_eq!(g.reg(RA), core.engine.pcs.ret);
        assert_eq!(g.reg(A0), 0x99);
        assert_eq!(g.reg(SP), CallEngine::frame_base(sp0, 0));
        g.set_reg(A0, 0);
        g.set_reg(8, 0xDEAD_DEAD); // the callee clobbers s0

        let step = core.on_magic_return(&mut g, &mut host).expect("return");
        assert_eq!(
            step,
            Step::Returned {
                pc: HOOK_RA,
                a0: 7,
                a1: 8
            }
        );
        assert_eq!(core.outstanding(), 0, "0 outstanding at the end");
        assert_eq!(g.reg(8), 0xF00D_1234, "the hook frame is restored");
        assert_eq!(g.reg(SP), sp0);
        assert_eq!(g.reg(A0), 7);
        assert!(matches!(host.resumes[1], Resume::Returned { a0: 0, .. }));
    }

    #[test]
    fn every_call_of_one_hook_takes_its_frame_from_the_hook_frame() {
        // A hook runs an ordered list of guest calls and does not fire while one is outstanding
        // (`specs/notes/g3-behavior.md` g3-hook-api), so the second call starts from the same
        // sp0 as the first rather than stacking below it.
        let mut g = guest();
        let mut core = core(&g);
        let mut host = ScriptHost::new(
            HANDLER,
            vec![
                call(SEM_TAKE, &[], Vec::new()),
                call(SEM_TAKE, &[], Vec::new()),
                HleAction::Return { a0: 0, a1: 0 },
            ],
        );
        let sp0 = g.reg(SP);
        core.on_hook(&mut g, &mut host, HOOK_PC, hook())
            .expect("hook");
        let first = g.reg(SP);
        core.on_magic_return(&mut g, &mut host).expect("return 1");
        assert_eq!(g.reg(SP), first, "the same frame, not one below it");
        assert_eq!(core.outstanding(), 1);
        core.on_magic_return(&mut g, &mut host).expect("return 2");
        assert_eq!(g.reg(SP), sp0);
        assert_eq!(core.outstanding(), 0);
    }

    #[test]
    fn a_scratch_call_from_the_magic_isr_reads_its_out_parameter_back() {
        // The magic ISR gives the worker semaphore through xQueueGiveFromISR, whose
        // pxHigherPriorityTaskWoken out-parameter lives in the ISR frame.
        let mut g = guest();
        let bottom = DATA + 0x9000;
        g.with_isr_stack(bottom, bottom + ISR_STACK_BYTES);
        let mut core = core(&g);
        let isr = worker_handler(MagicKind::BtIsr);
        let mut host = ScriptHost::new(
            isr,
            vec![
                call(
                    GIVE_FROM_ISR,
                    &[Arg::Val(0x1234), Arg::Scratch(0)],
                    vec![0u8; 4],
                ),
                HleAction::Return { a0: 0, a1: 0 },
            ],
        )
        .with_call(GIVE_FROM_ISR, "xQueueGiveFromISR", false);

        let step = core
            .on_hook(
                &mut g,
                &mut host,
                core.engine.pcs.bt_isr,
                HookRef::core(HookKind::Magic(MagicKind::BtIsr)),
            )
            .expect("magic ISR entry");
        assert_eq!(step, Step::Resume { pc: GIVE_FROM_ISR });
        assert_eq!(core.section.isr_generation, 1, "every ISR entry bumps it");
        let sp1 = g.reg(SP);
        g.poke(sp1, 1); // the callee sets *pxHigherPriorityTaskWoken

        core.on_magic_return(&mut g, &mut host).expect("return");
        match &host.resumes[1] {
            Resume::Returned { scratch, .. } => assert_eq!(scratch, &1u32.to_le_bytes()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_deleted_task_loses_its_continuations() {
        let mut g = guest();
        let mut core = core(&g);
        let mut host = ScriptHost::new(HANDLER, vec![call(SEM_TAKE, &[], Vec::new())]);
        core.on_hook(&mut g, &mut host, HOOK_PC, hook())
            .expect("hook");
        assert_eq!(core.outstanding(), 1);

        g.set_reg(A0, TCB_A);
        let observed = core.on_observe(&mut g, ObserveKind::TaskDelete);
        assert_eq!(observed.dropped, 1);
        assert_eq!(observed.task, TCB_A);
        assert_eq!(observed.generation, Some(1));
        assert_eq!(core.outstanding(), 0);
        // A new task at the reused TCB address starts above the bumped value.
        assert_eq!(core.section.generation_for(TCB_A, false), 1);
    }

    #[test]
    fn a_magic_return_from_another_task_is_refused() {
        let mut g = guest();
        let mut core = core(&g);
        let mut host = ScriptHost::new(HANDLER, vec![call(SEM_TAKE, &[], Vec::new())]);
        core.on_hook(&mut g, &mut host, HOOK_PC, hook())
            .expect("hook");
        // Another task happens to return to the magic PC with the same sp.
        g.current_task = TCB_B;
        g.poke(TCB_B + GuardProfile::default().tcb_px_stack, STACK_B);
        let err = core
            .on_magic_return(&mut g, &mut host)
            .expect_err("the sp match alone must not be enough");
        assert_eq!(err.kind, HleErrorKind::TaskMismatch);
        assert!(err.detail.contains(&format!("{TCB_A:#010x}")), "{err}");
    }

    #[test]
    fn a_magic_return_after_the_generation_moved_is_refused() {
        let mut g = guest();
        let mut core = core(&g);
        let mut host = ScriptHost::new(HANDLER, vec![call(SEM_TAKE, &[], Vec::new())]);
        core.on_hook(&mut g, &mut host, HOOK_PC, hook())
            .expect("hook");
        // A TCB address reused by a new task: the generation moved, the continuation did not.
        core.section.task_generations.insert(TCB_A, 9);
        let err = core
            .on_magic_return(&mut g, &mut host)
            .expect_err("the generation guard");
        assert_eq!(err.kind, HleErrorKind::GenerationMismatch);
        assert!(err.detail.contains("generation 9"), "{err}");
    }

    #[test]
    fn a_magic_return_with_no_continuation_is_refused() {
        let mut g = guest();
        let mut core = core(&g);
        let mut host = ScriptHost::new(HANDLER, Vec::new());
        let err = core
            .on_magic_return(&mut g, &mut host)
            .expect_err("nothing is outstanding");
        assert_eq!(err.kind, HleErrorKind::UnknownContinuation);
    }

    #[test]
    fn a_failing_guard_stops_the_run_and_leaves_no_continuation() {
        let guards = GuardProfile::default();
        let mut g = SynthGuest::new();
        g.with_task(&guards, TCB_A, STACK_A, STACK_A + guards.headroom);
        let mut core = core(&g);
        let mut host = ScriptHost::new(HANDLER, vec![call(SEM_TAKE, &[], Vec::new())]);
        let err = core
            .on_hook(&mut g, &mut host, HOOK_PC, hook())
            .expect_err("no headroom");
        assert_eq!(err.kind, HleErrorKind::StackHeadroom);
        assert_eq!(core.outstanding(), 0);
    }

    #[test]
    fn a_snapshot_with_an_outstanding_continuation_restores_identically() {
        let mut g = guest();
        let mut core = core(&g);
        let mut host = ScriptHost::new(
            HANDLER,
            vec![
                call(SEM_TAKE, &[Arg::Val(3)], vec![1, 2, 3, 4]),
                HleAction::Return { a0: 5, a1: 6 },
            ],
        );
        core.section.binding = BindingRecord {
            profile_id: "5.5.3".to_string(),
            app_elf_sha256: [9u8; 32],
            ..BindingRecord::default()
        };
        core.on_hook(&mut g, &mut host, HOOK_PC, hook())
            .expect("hook");
        assert_eq!(core.outstanding(), 1);

        let section = core.section.encode().expect("encode");
        let restored = HleSection::decode(&section).expect("decode");
        assert_eq!(restored, core.section, "the whole hle section round-trips");

        // The restored machine completes the outstanding call at the same place.
        let mut core2 = HleCore::new(core.engine, BoundHooks::default());
        core2.section = restored;
        let step = core2.on_magic_return(&mut g, &mut host).expect("finish");
        assert_eq!(
            step,
            Step::Returned {
                pc: HOOK_RA,
                a0: 5,
                a1: 6
            }
        );
        assert_eq!(core2.outstanding(), 0);
    }

    /// A chip reset destroys every guest stack, so the continuations, task generations and ISR
    /// nesting go with it, while the binding, which is image state, survives.
    #[test]
    fn a_chip_reset_drops_the_continuations_and_generations_of_the_boot_that_ended() {
        let mut g = guest();
        let mut core = core(&g);
        let mut host = ScriptHost::new(
            HANDLER,
            vec![
                call(SEM_TAKE, &[Arg::Val(0x99)], Vec::new()),
                HleAction::Return { a0: 0, a1: 0 },
            ],
        )
        .with_call(SEM_TAKE, "xQueueSemaphoreTake", true);
        core.section.binding = BindingRecord {
            profile_id: "5.5.3".to_string(),
            app_elf_sha256: [9u8; 32],
            ..BindingRecord::default()
        };
        core.section.task_generations.insert(TCB_A, 3);
        core.section.isr_generation = 7;
        core.on_hook(&mut g, &mut host, HOOK_PC, hook())
            .expect("hook");
        assert_eq!(core.outstanding(), 1, "a call is suspended");

        core.on_chip_reset();
        assert_eq!(
            core.outstanding(),
            0,
            "the frame it would return to is gone"
        );
        assert!(core.section.task_generations.is_empty());
        assert_eq!(core.section.isr_generation, 0);
        assert!(core.section.isr_nesting.is_empty());
        assert_eq!(
            core.section.binding.app_elf_sha256, [9u8; 32],
            "the binding is image state and survives a reset"
        );
    }

    #[test]
    fn a_restore_against_another_app_elf_is_refused() {
        // Restore returns E_SNAPSHOT when the loaded app ELF SHA-256 differs.
        let mut section = HleSection::default();
        section.binding.app_elf_sha256 = [1u8; 32];
        assert!(section.check_app_elf(&[1u8; 32]).is_ok());
        assert!(section.check_app_elf(&[2u8; 32]).is_err());
    }

    #[test]
    fn a_fetch_of_an_unallocated_magic_pc_trips() {
        let g = guest();
        let core = core(&g);
        let pcs = core.engine.pcs;
        match core.tripped(pcs.bt_isr + 4, TripKind::MagicRangeFetch) {
            Step::Tripped { kind, detail } => {
                assert_eq!(kind, TripKind::MagicRangeFetch);
                assert!(detail.contains("magic PC"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_armed_tripwire_names_its_symbol() {
        let g = guest();
        let mut core = core(&g);
        core.tripwires
            .arm(0x4200_9000, TripKind::BlobInternal, "ppTxPkt");
        match core.tripped(0x4200_9000, TripKind::BlobInternal) {
            Step::Tripped { kind, detail } => {
                assert_eq!(kind, TripKind::BlobInternal);
                assert_eq!(detail, "ppTxPkt at 0x42009000");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_fatal_observe_hooks_report_the_running_task() {
        let mut g = guest();
        let mut core = core(&g);
        for kind in [
            ObserveKind::PanicHandler,
            ObserveKind::Abort,
            ObserveKind::AssertFunc,
        ] {
            let observed = core.on_observe(&mut g, kind);
            assert_eq!(observed.task, TCB_A);
            assert_eq!(observed.dropped, 0);
            assert_eq!(observed.generation, None);
            assert!(kind.is_fatal());
        }
    }

    #[test]
    fn a_worker_profile_raises_the_scratch_limit_for_its_own_task_context_only() {
        // The BLE worker hands 259-byte H4 packets through scratch; hook handlers and ISR context
        // keep the guard profile's 128 B.
        let g = guest();
        let mut core = core(&g);
        core.add_worker(crate::worker::WorkerConfig {
            profile: crate::worker::WorkerProfile {
                max_scratch: Some(264),
                ..crate::worker::bt_controller_profile()
            },
            calls: crate::rtos_model::CALLS,
        })
        .expect("a U5 worker");
        core.section.workers.insert(
            MagicKind::BtWorker as u8,
            crate::worker::WorkerState {
                task: TCB_A,
                semaphore: 1,
                wake: crate::worker::WakeEngine::new(WakeMode::U5Polling),
                deinit: false,
            },
        );
        let cont = |task, in_isr| Continuation {
            task,
            in_isr,
            ..Continuation::default()
        };
        assert_eq!(core.scratch_limit(&cont(TCB_A, false)), 264);
        assert_eq!(core.scratch_limit(&cont(TCB_A, true)), 128, "ISR context");
        assert_eq!(
            core.scratch_limit(&cont(TCB_A + 0x100, false)),
            128,
            "another task"
        );
        assert_eq!(
            core.scratch_limit(&cont(0, false)),
            128,
            "before the scheduler"
        );
    }

    #[test]
    fn v_task_delete_null_deletes_the_calling_task() {
        let mut g = guest();
        let mut core = core(&g);
        g.set_reg(A0, 0);
        let observed = core.on_observe(&mut g, ObserveKind::TaskDelete);
        assert_eq!(observed.task, TCB_A);
    }

    #[test]
    fn a_parked_handler_waits_until_it_is_woken() {
        let mut g = guest();
        let mut core = core(&g);
        let mut host = ScriptHost::new(
            HANDLER,
            vec![HleAction::Park, HleAction::Return { a0: 1, a1: 0 }],
        );
        let step = core
            .on_hook(&mut g, &mut host, HOOK_PC, hook())
            .expect("hook");
        assert_eq!(step, Step::Parked);
        assert_eq!(core.outstanding(), 1);
        let key = core.section.continuations.iter().next().expect("parked").0;
        let step = core
            .wake(&mut g, &mut host, key, WakeReason::default())
            .expect("wake");
        assert_eq!(
            step,
            Step::Returned {
                pc: HOOK_RA,
                a0: 1,
                a1: 0
            }
        );
    }

    #[test]
    fn a_module_timer_is_keyed_by_the_module_index() {
        use crate::hooks::ModuleIndex;
        use pemu_core::sched::{Owner, RadioId};
        let key = module_timer(ModuleIndex::FIRST_MODULE, 7);
        assert_eq!((key.owner, key.tag), (Owner::Radio(RadioId(1)), 7));
        assert_ne!(module_timer(ModuleIndex(2), 7), key);
    }
}
