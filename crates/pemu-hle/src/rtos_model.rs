//! A synthetic FreeRTOS firmware for the worker tests.
//!
//! Every HLE decision runs through production code ([`HleCore`] with a registered worker). This
//! file models only the machine's half: the bodies of the FreeRTOS callees
//! (`xQueueGenericCreate`, `xTaskCreatePinnedToCore`, `xQueueSemaphoreTake`,
//! `xQueueGiveFromISR`, the port's yield-from-ISR), a two-task scheduler, timed waits, critical
//! sections, and interrupt entry and exit of the IDF FreeRTOS RISC-V port (`rtos_int_enter`,
//! `rtos_int_exit`). The same gates also run on the real machine (`tests/milestones/m8.rs`,
//! `t1_m8_u4_gate_*`).

use pemu_core::irq_source::IrqSource;
use pemu_core::snap::SnapSection;

use crate::binding::BoundHooks;
use crate::continuation::{HandlerState, Resume};
use crate::core::{CallInfo, HandlerHost, HleCore, Step, worker_handler};
use crate::guest_call::{A0, CallEngine, GuardProfile, GuestView, HleAction, RA, SP, read_u32};
use crate::hooks::{HandlerKind, HookKind, HookRef, ModuleIndex};
use crate::magic::{MagicKind, MagicPcs};
use crate::test_guest::{DATA, ISR_STACK_BYTES, SynthGuest, TEXT};
use crate::worker::{
    PD_TRUE, WakeMode, WakeReason, WorkerCalls, WorkerConfig, WorkerProfile, bt_controller_profile,
};

pub(crate) const TCB_MAIN: u32 = DATA + 0x1000;
pub(crate) const STACK_MAIN: u32 = DATA + 0x2000;
pub(crate) const TCB_WORKER: u32 = DATA + 0x6000;
pub(crate) const STACK_WORKER: u32 = DATA + 0x7000;
pub(crate) const ISR_STACK_BOTTOM: u32 = DATA + 0xC000;
/// Handle `xQueueGenericCreate` returns for the worker semaphore.
pub(crate) const SEMAPHORE: u32 = 0x3FCA_9000;
pub(crate) const INIT_PC: u32 = TEXT + 0x1000;
pub(crate) const INIT_RA: u32 = TEXT + 0x2000;
pub(crate) const INIT: HandlerKind = HandlerKind(1);
/// A test value for the U4 profile's interrupt source. The real BLE source is UNVERIFIED, which
/// is why the shipped profile leaves it `None`.
pub(crate) const TEST_SOURCE: IrqSource = IrqSource(1);

pub(crate) const CALLS: WorkerCalls = WorkerCalls {
    task_create: 0x4039_10F0,
    queue_create: 0x4039_2000,
    semaphore_take: 0x4039_3000,
    give_from_isr: 0x4039_0F6A,
    yield_from_isr: 0x4038_B87C,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Task {
    /// Priority 1.
    Main,
    /// The `btController` worker, priority 23.
    Worker,
}

pub(crate) struct RadioHost {
    pcs: MagicPcs,
    pub profile: WorkerProfile,
    pub wakes: Vec<WakeReason>,
    /// The `&handle` `xTaskCreatePinnedToCore` wrote back.
    pub handle: Option<u32>,
}

impl HandlerHost for RadioHost {
    fn enter(&mut self, kind: HandlerKind, _g: &mut dyn GuestView) -> (HandlerState, HleAction) {
        if kind == INIT {
            let state = HandlerState {
                handler: "ble.init".to_string(),
                bytes: vec![0],
            };
            (state, HleAction::from(CALLS.create_semaphore()))
        } else {
            assert_eq!(kind, worker_handler(MagicKind::BtWorker));
            let state = HandlerState {
                handler: "ble.worker".to_string(),
                bytes: Vec::new(),
            };
            (state, HleAction::Park)
        }
    }

    fn resume(
        &mut self,
        state: &mut HandlerState,
        _g: &mut dyn GuestView,
        resume: Resume,
    ) -> HleAction {
        match (state.handler.as_str(), resume) {
            ("ble.init", Resume::Returned { a0, .. }) if state.bytes[0] == 0 => {
                // The semaphore came back; the worker gets it as pvParameters.
                state.bytes[0] = 1;
                HleAction::from(CALLS.create_worker(&self.pcs, &self.profile, a0))
            }
            ("ble.init", Resume::Returned { scratch, .. }) => {
                self.handle = Some(u32::from_le_bytes([
                    scratch[0], scratch[1], scratch[2], scratch[3],
                ]));
                HleAction::Return { a0: 0, a1: 0 }
            }
            ("ble.worker", Resume::Woken { reason }) => {
                self.wakes.push(reason);
                if reason == WakeReason::Deinit {
                    HleAction::Return { a0: 0, a1: 0 }
                } else {
                    HleAction::Park
                }
            }
            (handler, other) => panic!("{handler} resumed with {other:?}"),
        }
    }

    fn describe(&self, func: u32) -> CallInfo {
        let name = match func {
            f if f == CALLS.queue_create => "xQueueGenericCreate",
            f if f == CALLS.task_create => "xTaskCreatePinnedToCore",
            _ => "nested call",
        };
        CallInfo {
            name,
            blocking: true,
            _func: func,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Callee {
    /// Returned to `ra`, the magic return PC.
    Returned,
    /// Blocked: the task waits and another one runs.
    Blocked,
}

/// A guest, the HLE core with a registered worker, the module's handlers and a two-task FreeRTOS
/// model.
#[derive(Clone)]
pub(crate) struct Model {
    pub running: Task,
    /// False inside `portENTER_CRITICAL`: the C3 port masks interrupts there.
    pub interrupts_enabled: bool,
    /// True while the worker is blocked in `xQueueSemaphoreTake` with a nonzero timeout.
    pub worker_blocked: bool,
    pub semaphore_count: u32,
    /// The port's switch-at-interrupt-exit flag, which the yield-from-ISR call sets.
    pub port_switch: bool,
    pub saved_sp: [u32; 2],
    pub created: Option<(u32, u32)>,
    pub woken_written: Vec<u32>,
    pub wait_ticks: Vec<u32>,
}

pub(crate) struct Firmware {
    pub g: SynthGuest,
    pub core: HleCore,
    pub host: RadioHost,
    pub m: Model,
    pub delivered: Vec<u16>,
    pub isr_top: u32,
}

impl Firmware {
    pub fn new(mode: WakeMode) -> Firmware {
        let guards = GuardProfile::default();
        let mut g = SynthGuest::new();
        g.with_task(&guards, TCB_MAIN, STACK_MAIN, STACK_MAIN + 0x800);
        g.poke(TCB_WORKER + guards.tcb_px_stack, STACK_WORKER);
        let isr_top = ISR_STACK_BOTTOM + ISR_STACK_BYTES;
        g.with_isr_stack(ISR_STACK_BOTTOM, isr_top);
        g.in_isr = false;
        g.set_reg(SP, STACK_MAIN + 0x800);
        let pcs = MagicPcs::from_spec().expect("specs/magic-pcs.toml");
        let engine = CallEngine {
            pcs,
            guards,
            stack: g.stack_symbols(),
        };
        let profile = WorkerProfile {
            wake: mode,
            isr_source: (mode == WakeMode::U4MagicIsr).then_some(TEST_SOURCE),
            ..bt_controller_profile()
        };
        let mut core = HleCore::new(engine, BoundHooks::default());
        core.add_worker(WorkerConfig {
            profile: profile.clone(),
            calls: CALLS,
        })
        .expect("a valid worker registration");
        Firmware {
            g,
            core,
            host: RadioHost {
                pcs,
                profile,
                wakes: Vec::new(),
                handle: None,
            },
            m: Model {
                running: Task::Main,
                interrupts_enabled: true,
                worker_blocked: false,
                semaphore_count: 0,
                port_switch: false,
                saved_sp: [STACK_MAIN + 0x800, STACK_WORKER + 0x800],
                created: None,
                woken_written: Vec::new(),
                wait_ticks: Vec::new(),
            },
            delivered: Vec::new(),
            isr_top,
        }
    }

    /// The app calls `esp_bt_controller_init`: the hooked init creates the semaphore and the
    /// worker, and the worker then enters at the magic worker PC and parks. Returns what the init
    /// returned to the app.
    pub fn boot_radio(&mut self) -> Step {
        self.g.set_reg(RA, INIT_RA);
        let hook = HookRef {
            kind: HookKind::Hle(INIT),
            module: ModuleIndex::FIRST_MODULE,
        };
        let step = self
            .core
            .on_hook(&mut self.g, &mut self.host, INIT_PC, hook)
            .expect("the init hook runs");
        let returned = self.drive(step);
        assert!(
            matches!(returned, Some(Step::Returned { .. })),
            "{returned:?}"
        );

        // The scheduler starts the priority-23 worker at its entry with pvParameters in a0.
        let (entry, param) = self.m.created.expect("xTaskCreatePinnedToCore ran");
        self.switch_to(Task::Worker);
        self.g.set_reg(A0, param);
        self.g.set_reg(RA, 0);
        let step = self
            .core
            .on_hook(
                &mut self.g,
                &mut self.host,
                entry,
                HookRef::core(HookKind::Magic(MagicKind::BtWorker)),
            )
            .expect("the worker enters");
        let parked = self.drive(step);
        assert_eq!(parked, None, "the worker blocks in its wait");
        self.switch_to(Task::Main);
        returned.expect("checked above")
    }

    /// Runs HLE steps until a handler returns (`Some`) or a callee blocks (`None`). Events the
    /// worker was woken for are drained after each wake, as the run loop does.
    pub fn drive(&mut self, mut step: Step) -> Option<Step> {
        loop {
            match step {
                Step::Resume { pc } => {
                    if self.run_callee(pc) == Callee::Blocked {
                        self.m.saved_sp[self.m.running as usize] = self.g.reg(SP);
                        return None;
                    }
                    let wakes = self.host.wakes.len();
                    step = self
                        .core
                        .on_magic_return(&mut self.g, &mut self.host)
                        .expect("magic return");
                    if self.host.wakes.len() > wakes {
                        while let Some(event) = self.core.next_event(MagicKind::BtWorker) {
                            self.delivered.push(event.tag);
                        }
                    }
                }
                other => return Some(other),
            }
        }
    }

    pub fn switch_to(&mut self, task: Task) {
        self.m.saved_sp[self.m.running as usize] = self.g.reg(SP);
        self.m.running = task;
        let (tcb, sp) = match task {
            Task::Main => (TCB_MAIN, self.m.saved_sp[0]),
            Task::Worker => (TCB_WORKER, self.m.saved_sp[1]),
        };
        self.g.current_task = tcb;
        self.g.set_reg(SP, sp);
    }

    pub fn run_callee(&mut self, pc: u32) -> Callee {
        assert_eq!(
            self.g.reg(RA),
            self.core.engine.pcs.ret,
            "every nested call returns through the magic PC"
        );
        let a = |g: &SynthGuest, n: u8| g.reg(A0 + n);
        match pc {
            p if p == CALLS.queue_create => {
                assert_eq!((a(&self.g, 0), a(&self.g, 1), a(&self.g, 2)), (1, 0, 3));
                self.g.set_reg(A0, SEMAPHORE);
            }
            p if p == CALLS.task_create => {
                let name = self.g.bytes(a(&self.g, 1), 13);
                assert_eq!(&name, b"btController\0");
                assert_eq!(a(&self.g, 2), 4096);
                assert_eq!(a(&self.g, 4), 23);
                self.g.poke(a(&self.g, 5), TCB_WORKER);
                self.m.created = Some((a(&self.g, 0), a(&self.g, 3)));
                self.g.set_reg(A0, PD_TRUE);
            }
            p if p == CALLS.semaphore_take => {
                assert_eq!(self.m.running, Task::Worker, "only the worker waits");
                assert_eq!(a(&self.g, 0), SEMAPHORE);
                let ticks = a(&self.g, 1);
                self.m.wait_ticks.push(ticks);
                if self.m.semaphore_count > 0 {
                    self.m.semaphore_count = 0;
                    self.g.set_reg(A0, PD_TRUE);
                } else if ticks == 0 {
                    self.g.set_reg(A0, 0);
                } else {
                    self.m.worker_blocked = true;
                    return Callee::Blocked;
                }
            }
            p if p == CALLS.give_from_isr => {
                assert!(self.g.in_isr, "the give runs in the magic ISR");
                assert_eq!(a(&self.g, 0), SEMAPHORE);
                self.m.semaphore_count = 1;
                // pdTRUE only when a higher-priority task was blocked on the semaphore, which is
                // what makes the interrupt exit switch to it.
                let woken = u32::from(self.m.worker_blocked);
                self.g.poke(a(&self.g, 1), woken);
                self.m.woken_written.push(woken);
                self.g.set_reg(A0, PD_TRUE);
            }
            p if p == CALLS.yield_from_isr => {
                assert!(self.g.in_isr);
                self.m.port_switch = true;
            }
            _ => panic!("the synthetic firmware has no callee at {pc:#010x}"),
        }
        Callee::Returned
    }

    /// The worker's blocked wait ends: it timed out (U5 poll interval) or the semaphore was
    /// given. The worker runs, is woken, parks again, and the app task runs.
    pub fn resume_worker(&mut self, took: bool) {
        assert!(self.m.worker_blocked, "the worker is blocked in its wait");
        self.m.worker_blocked = false;
        self.switch_to(Task::Worker);
        if took {
            self.m.semaphore_count = 0;
        }
        self.g.set_reg(A0, if took { PD_TRUE } else { 0 });
        let step = self
            .core
            .on_magic_return(&mut self.g, &mut self.host)
            .expect("the wait returns");
        while let Some(event) = self.core.next_event(MagicKind::BtWorker) {
            self.delivered.push(event.tag);
        }
        let result = self.drive(step);
        if result.is_none() {
            self.switch_to(Task::Main);
        }
    }

    pub fn level(&self) -> bool {
        self.core
            .worker(MagicKind::BtWorker)
            .is_some_and(|w| w.wake.level())
    }

    pub fn critical(&mut self, inside: impl FnOnce(&mut Firmware)) {
        self.m.interrupts_enabled = false;
        inside(self);
        self.m.interrupts_enabled = true;
    }

    /// A radio event arrives from `pemu-radio`.
    pub fn post(&mut self, tag: u16) {
        self.core
            .post(
                &mut self.g,
                MagicKind::BtWorker,
                crate::worker::RadioEvent {
                    tag,
                    payload: Vec::new(),
                },
            )
            .expect("a registered worker");
    }

    /// The guest takes the radio interrupt when it can and runs it to the interrupt exit.
    /// Returns false when the level is low or interrupts are masked.
    pub fn service_interrupt(&mut self) -> bool {
        match self.enter_interrupt(usize::MAX) {
            Some((task_sp, _)) => {
                self.exit_interrupt(task_sp);
                true
            }
            None => false,
        }
    }

    /// Takes the interrupt and runs at most `stop_after` HLE steps of the magic ISR. Returns the
    /// interrupted task's `sp` and the steps taken, or `None` when no interrupt is taken. The
    /// ISR has returned when fewer than `stop_after` steps were needed.
    pub fn enter_interrupt(&mut self, stop_after: usize) -> Option<(u32, usize)> {
        if !self.m.interrupts_enabled || !self.level() {
            return None;
        }
        // rtos_int_enter: nesting level 1 loads xIsrStackTop into sp.
        let task_sp = self.g.reg(SP);
        self.g.in_isr = true;
        let top_at = self.g.symbol("xIsrStackTop").expect("bound");
        let top = read_u32(&mut self.g, top_at).expect("readable");
        assert_eq!(top, self.isr_top);
        self.g.set_reg(SP, top);
        self.g.set_reg(RA, TEXT + 0x3000); // the interrupt dispatcher called the magic ISR
        let pc = self.core.engine.pcs.bt_isr;
        let mut step = self
            .core
            .on_hook(
                &mut self.g,
                &mut self.host,
                pc,
                HookRef::core(HookKind::Magic(MagicKind::BtIsr)),
            )
            .expect("magic ISR entry");
        let mut taken = 1;
        while let Step::Resume { pc } = step {
            if taken >= stop_after {
                return Some((task_sp, taken));
            }
            assert_eq!(
                self.run_callee(pc),
                Callee::Returned,
                "ISR callees never block"
            );
            step = self
                .core
                .on_magic_return(&mut self.g, &mut self.host)
                .expect("magic return");
            taken += 1;
        }
        assert!(matches!(step, Step::Returned { .. }), "{step:?}");
        Some((task_sp, taken))
    }

    /// Finishes an interrupt stopped by [`Firmware::enter_interrupt`]: runs the rest of the
    /// magic ISR, then the interrupt exit.
    pub fn finish_isr(&mut self, task_sp: u32) {
        let pending = self
            .core
            .section
            .continuations
            .iter()
            .find(|(_, c)| c.magic_isr)
            .map(|(_, c)| c.func)
            .expect("an outstanding ISR call");
        assert_eq!(self.run_callee(pending), Callee::Returned);
        let mut step = self
            .core
            .on_magic_return(&mut self.g, &mut self.host)
            .expect("magic return");
        while let Step::Resume { pc } = step {
            assert_eq!(self.run_callee(pc), Callee::Returned);
            step = self
                .core
                .on_magic_return(&mut self.g, &mut self.host)
                .expect("magic return");
        }
        self.exit_interrupt(task_sp);
    }

    /// `rtos_int_exit`: back on the task stack, and a switch to the worker when the yield flag is
    /// set. The worker's wait returns with the semaphore taken.
    pub fn exit_interrupt(&mut self, task_sp: u32) {
        self.g.in_isr = false;
        self.g.set_reg(SP, task_sp);
        if std::mem::take(&mut self.m.port_switch) {
            self.resume_worker(true);
        }
    }

    pub fn hle_bytes(&self) -> Vec<u8> {
        self.core.section.encode().expect("encode").bytes
    }
}
