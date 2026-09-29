//! Guest access for HLE handlers (`GuestView`) and nested guest calls (`HleAction::Call`), with
//! the nested-call guards.
//!
//! A nested call borrows the interrupted stack:
//!
//! ```text
//! sp1 = (sp0 - scratch.len() - 16) & !0xF      scratch written at sp1, Arg::Scratch(off) -> sp1 + off
//! a0..a7 = args ; ra = MagicPcs::ret ; pc = func
//! ```
//!
//! The engine, not the handler, lays the frame out, which makes a headroom check possible. The
//! risk is real (`specs/notes/g3-behavior.md`, `g3-nested-call-stack`): a hook placed where the
//! stack was nearly full overwrote memory below it silently, and it is largest in an interrupt,
//! on the 1,536 B ISR stack (IDF `CONFIG_ESP_SYSTEM_ISR_STACK_SIZE`).
//!
//! The guards, each failing with `E_HLE` naming the task or the ISR stack:
//!
//! 1. a blocking nested call is refused inside an ISR or before the scheduler runs;
//! 2. a continuation key collision is refused;
//! 3. the frame, scratch included, must leave the profile's headroom above the stack in use:
//!    `sp1 >= xIsrStackBottom + headroom` in an ISR, `sp1 >= pxCurrentTCBs->pxStack + headroom`
//!    otherwise;
//! 4. on return the current task and its `task_generations` entry must match the recorded ones;
//!    an ISR continuation instead requires `isr_generation` to match;
//! 5. the scratch block stays within the profile's cap (128 B; the largest event struct is 48 B).

use std::fmt;

use pemu_core::irq_source::IrqSource;
use pemu_core::sched::{EventHandle, EventKey};
use pemu_core::time::VTime;
use pemu_rv32::trap::Trap;

use crate::continuation::{ContKey, Continuations};
use crate::magic::MagicPcs;

/// `x2`, the stack pointer.
pub const SP: u8 = 2;
pub const RA: u8 = 1;
pub const A0: u8 = 10;

/// Bytes an `Arg::Scratch` pointer must have inside the scratch block: one word, the size of every
/// out-parameter the HLE passes (`&woken`, `&handle`).
pub const SCRATCH_WORD: usize = 4;

/// Argument of a nested guest call.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Arg {
    Val(u32),
    /// Byte offset into the scratch block; resolves to `sp1 + off`.
    Scratch(u16 /* byte offset into the scratch block */),
}

/// What one handler step asks the engine to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HleAction {
    /// Nested guest call with `ra = MagicPcs::ret`, framed as in the module header.
    /// Out-parameters such as `&woken` for `xQueueGiveFromISR` are `Arg::Scratch`.
    Call {
        func: u32,
        /// Only the first `nargs` are used.
        args: [Arg; 8],
        nargs: u8,
        /// Written at `sp1` and read back into `Resume::Returned::scratch`.
        scratch: Vec<u8>,
    },
    /// Restore the hook frame, pc = saved ra.
    Return {
        /// The results left in `a0` and `a1`.
        a0: u32,
        a1: u32,
    },
    /// Worker waits for a radio event (via a nested FreeRTOS wait).
    Park,
    /// Stops the run with E_HLE.
    Fail(HleError),
}

/// Guest state an HLE handler may read and change.
pub trait GuestView {
    fn reg(&self, r: u8) -> u32;
    fn set_reg(&mut self, r: u8, v: u32);
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Trap>;
    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), Trap>;
    /// Sets the level of interrupt source `s`.
    fn raise(&mut self, s: IrqSource, level: bool);
    fn schedule(&mut self, at: VTime, key: EventKey) -> EventHandle;
    fn now(&self) -> VTime;
    /// Functions, data and local symbols (btdm_controller_status, s_wifi_inited).
    fn symbol(&self, name: &str) -> Option<u32>;
    /// pxCurrentTCBs.
    fn current_task(&mut self) -> u32;
    /// port_uxInterruptNesting != 0.
    fn in_isr(&mut self) -> bool;
    fn scheduler_running(&mut self) -> bool;
    /// Fills `out` from the machine's `DetRng` stream `stream`, so a radio model's randomness
    /// follows the seed and is in the `rng` snapshot section.
    fn draw_entropy(&mut self, stream: pemu_core::rng::RngStream, out: &mut [u8]);
    /// The guest's FreeRTOS tick rate, or `None` while it has not configured one, so a worker's U5
    /// poll can be a time rather than a tick count.
    fn tick_hz(&mut self) -> Option<u32> {
        None
    }
}

pub fn read_u32(g: &mut dyn GuestView, addr: u32) -> Result<u32, Trap> {
    let mut word = [0u8; 4];
    g.read(addr, &mut word)?;
    Ok(u32::from_le_bytes(word))
}

/// Design parameters of the nested-call guards.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GuardProfile {
    /// Bytes that must stay free below `sp1` (default 512).
    pub headroom: u32,
    /// Largest scratch block a handler may ask for (default 128).
    pub max_scratch: u16,
    /// Byte offset of `pxStack` inside `tskTaskControlBlock`.
    ///
    /// UNVERIFIED for an arbitrary build: 48 is the offset in the corpus ELFs' DWARF
    /// (`crates/pemu-introspect/src/freertos.rs` pins it). A build with another FreeRTOS
    /// configuration overrides it here rather than having the guard guess.
    pub tcb_px_stack: u32,
}

impl Default for GuardProfile {
    fn default() -> GuardProfile {
        GuardProfile {
            headroom: 512,
            max_scratch: 128,
            tcb_px_stack: 48,
        }
    }
}

/// The two ISR-stack symbols of the IDF FreeRTOS RISC-V port, resolved at bind time. Both are
/// pointer variables, so the guard reads the word at the symbol: interrupt entry at nesting level 1
/// loads `xIsrStackTop` into `sp` and sets `SP_MIN = xIsrStackBottom`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StackSymbols {
    /// Address of the `xIsrStackBottom` variable, not its value.
    pub isr_stack_bottom: Option<u32>,
    pub isr_stack_top: Option<u32>,
}

impl StackSymbols {
    pub fn resolve(g: &dyn GuestView) -> StackSymbols {
        StackSymbols {
            isr_stack_bottom: g.symbol("xIsrStackBottom"),
            isr_stack_top: g.symbol("xIsrStackTop"),
        }
    }
}

/// Why an HLE handler stopped the run with `E_HLE`, via `HleAction::Fail` or a failing guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HleError {
    pub kind: HleErrorKind,
    /// For a guard, names the task or the ISR stack.
    pub detail: String,
}

impl HleError {
    pub fn new(kind: HleErrorKind, detail: impl Into<String>) -> HleError {
        HleError {
            kind,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for HleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.detail)
    }
}

/// Which nested-call guard or handler step produced an [`HleError`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HleErrorKind {
    BlockingInIsr,
    SchedulerNotRunning,
    /// Another continuation already holds this `(sp1, generation)` key.
    KeyCollision,
    /// The frame would leave less than the profile's headroom above the stack in use.
    StackHeadroom,
    ScratchTooLarge,
    /// An `Arg::Scratch(off)` names a pointer whose 4-byte target does not lie inside the scratch
    /// block, so a callee writing through it would spill into the caller's frame above `sp1`.
    ScratchOutOfBounds,
    /// The magic return found no continuation under `(sp, generation)`.
    UnknownContinuation,
    TaskMismatch,
    /// The task or ISR generation moved while the call was outstanding.
    GenerationMismatch,
    GuestFault,
    /// The handler itself returned `HleAction::Fail`.
    Handler,
}

/// One nested guest call as the engine executes it: `HleAction::Call` plus whether it can block
/// and the callee name for the diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRequest {
    pub func: u32,
    /// Only the first `nargs` are used.
    pub args: [Arg; 8],
    pub nargs: u8,
    pub scratch: Vec<u8>,
    /// False only for a `FromISR` call, the only kind legal inside an ISR.
    pub blocking: bool,
    /// For the diagnostic of a failing guard.
    pub name: &'static str,
}

impl CallRequest {
    /// A blocking call of `func` with `args[..nargs]` and no scratch.
    pub fn new(name: &'static str, func: u32, args: &[Arg]) -> CallRequest {
        let mut slots = [Arg::Val(0); 8];
        slots[..args.len()].copy_from_slice(args);
        CallRequest {
            func,
            args: slots,
            nargs: args.len() as u8,
            scratch: Vec::new(),
            blocking: true,
            name,
        }
    }

    /// The same call with a scratch block; `Arg::Scratch(off)` resolves to `sp1 + off`.
    pub fn with_scratch(mut self, scratch: Vec<u8>) -> CallRequest {
        self.scratch = scratch;
        self
    }

    /// Marks the call non-blocking, which makes it legal inside an ISR.
    pub fn from_isr(mut self) -> CallRequest {
        self.blocking = false;
        self
    }
}

impl From<CallRequest> for HleAction {
    /// The nested call a handler asks for. `blocking` and `name` are not carried: the engine
    /// takes them from `HandlerHost::describe` when it carries the call out.
    fn from(call: CallRequest) -> HleAction {
        HleAction::Call {
            func: call.func,
            args: call.args,
            nargs: call.nargs,
            scratch: call.scratch,
        }
    }
}

/// The frame the engine picked for one nested call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallFrame {
    pub sp0: u32,
    /// `sp` during the call, and the key of the continuation.
    pub sp1: u32,
    pub a: [u32; 8],
    pub key: ContKey,
    pub in_isr: bool,
    /// The current task, which in an ISR still names the interrupted task.
    pub task: u32,
}

/// Which stack a nested call borrowed, and where its floor is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum StackFloor {
    /// `xIsrStackBottom`.
    Isr(u32),
    /// `pxCurrentTCBs->pxStack` of the named task.
    Task { tcb: u32, base: u32 },
}

impl StackFloor {
    fn address(self) -> u32 {
        match self {
            StackFloor::Isr(bottom) => bottom,
            StackFloor::Task { base, .. } => base,
        }
    }

    /// How a failing headroom check names the stack.
    fn name(self) -> String {
        match self {
            StackFloor::Isr(bottom) => format!("the ISR stack (xIsrStackBottom {bottom:#010x})"),
            StackFloor::Task { tcb, base } => {
                format!("task {tcb:#010x} (pxStack {base:#010x})")
            }
        }
    }
}

/// The nested-call engine: the magic PCs it returns through, the guard parameters and the two
/// ISR-stack symbols.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CallEngine {
    /// `ret` above all.
    pub pcs: MagicPcs,
    pub guards: GuardProfile,
    pub stack: StackSymbols,
}

impl CallEngine {
    /// `sp1 = (sp0 - scratch.len() - 16) & !0xF`, saturating, so a tiny `sp0` produces a frame the
    /// headroom guard refuses rather than one that wraps around.
    pub fn frame_base(sp0: u32, scratch_len: usize) -> u32 {
        sp0.saturating_sub(scratch_len as u32).saturating_sub(16) & !0xF
    }

    /// Runs every guard and returns the frame, or the `E_HLE` that refuses the call. A refusal
    /// leaves the guest unchanged.
    pub fn prepare(
        &self,
        g: &mut dyn GuestView,
        conts: &Continuations,
        generation: u64,
        call: &CallRequest,
    ) -> Result<CallFrame, HleError> {
        self.prepare_with_scratch_limit(g, conts, generation, call, self.guards.max_scratch)
    }

    /// [`CallEngine::prepare`] with the scratch limit of the calling context: a worker's profile
    /// may set its own (`WorkerProfile::max_scratch`).
    pub fn prepare_with_scratch_limit(
        &self,
        g: &mut dyn GuestView,
        conts: &Continuations,
        generation: u64,
        call: &CallRequest,
        max_scratch: u16,
    ) -> Result<CallFrame, HleError> {
        if call.scratch.len() > usize::from(max_scratch) {
            return Err(HleError::new(
                HleErrorKind::ScratchTooLarge,
                format!(
                    "nested call {} asks for {} scratch bytes, more than the profile's {}",
                    call.name,
                    call.scratch.len(),
                    max_scratch
                ),
            ));
        }
        // Every pointer into the scratch block must have a whole word behind it: the caller's
        // frame starts at most 16 bytes above the block's end, so an offset whose word runs past
        // the block lets an out-parameter write land in memory the interrupted code still owns.
        for arg in call.args.iter().take(call.nargs.into()) {
            if let Arg::Scratch(off) = *arg
                && usize::from(off) + SCRATCH_WORD > call.scratch.len()
            {
                return Err(HleError::new(
                    HleErrorKind::ScratchOutOfBounds,
                    format!(
                        "nested call {} passes Arg::Scratch({off}), but its scratch block is {} \
                         byte(s); a word at that offset would spill into the caller's frame",
                        call.name,
                        call.scratch.len()
                    ),
                ));
            }
        }
        let in_isr = g.in_isr();
        let task = g.current_task();
        if call.blocking {
            if in_isr {
                return Err(HleError::new(
                    HleErrorKind::BlockingInIsr,
                    format!(
                        "blocking nested call {} inside an ISR (interrupted task {task:#010x}); \
                         only FromISR calls are legal there",
                        call.name
                    ),
                ));
            }
            if !g.scheduler_running() {
                return Err(HleError::new(
                    HleErrorKind::SchedulerNotRunning,
                    format!(
                        "blocking nested call {} before the scheduler runs (task {task:#010x})",
                        call.name
                    ),
                ));
            }
        }

        let sp0 = g.reg(SP);
        let sp1 = CallEngine::frame_base(sp0, call.scratch.len());
        let floor = self.floor(g, in_isr, task)?;
        let limit = floor.address().saturating_add(self.guards.headroom);
        if sp1 < limit {
            return Err(HleError::new(
                HleErrorKind::StackHeadroom,
                format!(
                    "nested call {} would put sp at {sp1:#010x} on {}, inside the {} B headroom \
                     above {:#010x}; {} scratch byte(s) included",
                    call.name,
                    floor.name(),
                    self.guards.headroom,
                    floor.address(),
                    call.scratch.len(),
                ),
            ));
        }

        let key = ContKey {
            sp: sp1,
            generation,
        };
        if conts.contains(key) {
            return Err(HleError::new(
                HleErrorKind::KeyCollision,
                format!(
                    "a continuation already holds sp {sp1:#010x} generation {generation}; \
                     nested call {} refused",
                    call.name
                ),
            ));
        }

        let mut a = [0u32; 8];
        for (slot, arg) in a.iter_mut().zip(call.args.iter()).take(call.nargs.into()) {
            *slot = match *arg {
                Arg::Val(v) => v,
                Arg::Scratch(off) => sp1.wrapping_add(u32::from(off)),
            };
        }
        Ok(CallFrame {
            sp0,
            sp1,
            a,
            key,
            in_isr,
            task,
        })
    }

    /// `xIsrStackBottom` inside an ISR, `pxCurrentTCBs->pxStack` otherwise.
    fn floor(
        &self,
        g: &mut dyn GuestView,
        in_isr: bool,
        task: u32,
    ) -> Result<StackFloor, HleError> {
        if in_isr {
            let at = self.stack.isr_stack_bottom.ok_or_else(|| {
                HleError::new(
                    HleErrorKind::StackHeadroom,
                    "the ISR stack cannot be checked: xIsrStackBottom is not a bound symbol of \
                     the profile",
                )
            })?;
            let bottom = read_u32(g, at).map_err(|trap| {
                HleError::new(
                    HleErrorKind::GuestFault,
                    format!("reading xIsrStackBottom at {at:#010x} faulted: {trap:?}"),
                )
            })?;
            return Ok(StackFloor::Isr(bottom));
        }
        let at = task.wrapping_add(self.guards.tcb_px_stack);
        let base = read_u32(g, at).map_err(|trap| {
            HleError::new(
                HleErrorKind::GuestFault,
                format!("reading pxStack of task {task:#010x} at {at:#010x} faulted: {trap:?}"),
            )
        })?;
        Ok(StackFloor::Task { tcb: task, base })
    }

    /// Applies a prepared frame: the scratch block at `sp1`, `a0`..`a7`, `ra = MagicPcs::ret`,
    /// `sp = sp1`. The caller stores the continuation under [`CallFrame::key`] before resuming.
    pub fn start(
        &self,
        g: &mut dyn GuestView,
        frame: &CallFrame,
        call: &CallRequest,
    ) -> Result<u32, HleError> {
        if !call.scratch.is_empty() {
            g.write(frame.sp1, &call.scratch).map_err(|trap| {
                HleError::new(
                    HleErrorKind::GuestFault,
                    format!(
                        "writing {} scratch byte(s) at {:#010x} faulted: {trap:?}",
                        call.scratch.len(),
                        frame.sp1
                    ),
                )
            })?;
        }
        for (index, value) in frame.a.iter().enumerate().take(call.nargs.into()) {
            g.set_reg(A0 + index as u8, *value);
        }
        g.set_reg(RA, self.pcs.ret);
        g.set_reg(SP, frame.sp1);
        Ok(call.func)
    }

    /// Reads the scratch block back from `sp1` after the call returned.
    pub fn read_scratch(
        &self,
        g: &mut dyn GuestView,
        sp1: u32,
        len: usize,
    ) -> Result<Vec<u8>, HleError> {
        let mut out = vec![0u8; len];
        if len > 0 {
            g.read(sp1, &mut out).map_err(|trap| {
                HleError::new(
                    HleErrorKind::GuestFault,
                    format!("reading {len} scratch byte(s) back at {sp1:#010x} faulted: {trap:?}"),
                )
            })?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_guest::{DATA, ISR_STACK_BYTES, SynthGuest};

    /// `xQueueGiveFromISR(queue, &woken)`: the magic ISR's scratch call.
    const GIVE_FROM_ISR: u32 = 0x4039_0F6A;
    /// A blocking nested call, `xQueueSemaphoreTake`.
    const SEM_TAKE: u32 = 0x4039_1000;

    const TCB: u32 = DATA + 0x1000;
    const TASK_STACK: u32 = DATA + 0x2000;

    fn engine(g: &SynthGuest) -> CallEngine {
        CallEngine {
            pcs: crate::magic::MagicPcs::from_spec().expect("specs/magic-pcs.toml"),
            guards: GuardProfile::default(),
            stack: g.stack_symbols(),
        }
    }

    fn give_from_isr() -> CallRequest {
        CallRequest::new(
            "xQueueGiveFromISR",
            GIVE_FROM_ISR,
            &[Arg::Val(0x1234), Arg::Scratch(0)],
        )
        .with_scratch(vec![0u8; 4])
        .from_isr()
    }

    #[test]
    fn the_frame_is_the_documented_formula() {
        assert_eq!(CallEngine::frame_base(0x3FCA_EA20, 0), 0x3FCA_EA10);
        assert_eq!(CallEngine::frame_base(0x3FCA_EA20, 4), 0x3FCA_EA00);
        assert_eq!(CallEngine::frame_base(0x3FCA_EA20, 48), 0x3FCA_E9E0);
        assert!(CallEngine::frame_base(0x3FCA_EA20, 4).is_multiple_of(16));
        // A tiny sp0 saturates instead of wrapping into high memory.
        assert_eq!(CallEngine::frame_base(8, 0), 0);
    }

    #[test]
    fn a_blocking_call_is_refused_inside_an_isr() {
        // Only FromISR calls are legal there.
        let mut g = SynthGuest::new();
        g.with_task(
            &GuardProfile::default(),
            TCB,
            TASK_STACK,
            TASK_STACK + 0x1000,
        );
        g.with_isr_stack(DATA + 0x4000, DATA + 0x4000 + ISR_STACK_BYTES);
        let engine = engine(&g);
        let call = CallRequest::new("xQueueSemaphoreTake", SEM_TAKE, &[Arg::Val(0)]);
        let err = engine
            .prepare(&mut g, &Continuations::default(), 0, &call)
            .expect_err("a blocking call in an ISR is an emulator bug");
        assert_eq!(err.kind, HleErrorKind::BlockingInIsr);
        assert!(err.detail.contains("xQueueSemaphoreTake"), "{err}");
        // current_task() still names the interrupted task there.
        assert!(err.detail.contains(&format!("{TCB:#010x}")), "{err}");
    }

    #[test]
    fn a_from_isr_call_is_allowed_inside_an_isr() {
        let mut g = SynthGuest::new();
        g.with_task(
            &GuardProfile::default(),
            TCB,
            TASK_STACK,
            TASK_STACK + 0x1000,
        );
        let bottom = DATA + 0x4000;
        g.with_isr_stack(bottom, bottom + ISR_STACK_BYTES);
        let engine = engine(&g);
        let frame = engine
            .prepare(&mut g, &Continuations::default(), 7, &give_from_isr())
            .expect("a FromISR call with the whole ISR stack free");
        assert!(frame.in_isr);
        assert_eq!(frame.task, TCB);
        assert_eq!(frame.key.generation, 7);
    }

    #[test]
    fn a_blocking_call_is_refused_before_the_scheduler_runs() {
        // No blocking nested calls before the scheduler starts.
        let mut g = SynthGuest::new();
        g.scheduler_running = false;
        g.with_task(
            &GuardProfile::default(),
            TCB,
            TASK_STACK,
            TASK_STACK + 0x1000,
        );
        let engine = engine(&g);
        let call = CallRequest::new("xQueueSemaphoreTake", SEM_TAKE, &[Arg::Val(0)]);
        let err = engine
            .prepare(&mut g, &Continuations::default(), 0, &call)
            .expect_err("the scheduler is not running");
        assert_eq!(err.kind, HleErrorKind::SchedulerNotRunning);
        assert!(err.detail.contains(&format!("{TCB:#010x}")), "{err}");
    }

    #[test]
    fn the_headroom_check_refuses_a_task_frame_that_reaches_into_the_stack_floor() {
        let guards = GuardProfile::default();
        let mut g = SynthGuest::new();
        // 8 bytes of slack above pxStack + headroom: the 16-byte frame does not fit.
        let sp = TASK_STACK + guards.headroom + 8;
        g.with_task(&guards, TCB, TASK_STACK, sp);
        let engine = engine(&g);
        let call = CallRequest::new("xQueueSemaphoreTake", SEM_TAKE, &[Arg::Val(0)]);
        let err = engine
            .prepare(&mut g, &Continuations::default(), 0, &call)
            .expect_err("the frame reaches into the headroom");
        assert_eq!(err.kind, HleErrorKind::StackHeadroom);
        assert!(err.detail.contains(&format!("task {TCB:#010x}")), "{err}");
        assert!(err.detail.contains("pxStack"), "{err}");

        // The same call one 16-byte frame higher fits.
        g.set_reg(SP, sp + 16);
        assert!(
            engine
                .prepare(&mut g, &Continuations::default(), 0, &call)
                .is_ok()
        );
    }

    #[test]
    fn the_headroom_check_refuses_an_isr_frame_that_reaches_into_the_isr_stack_floor() {
        // The 512 B default headroom leaves 1,024 B of the 1,536 B ISR stack usable.
        let guards = GuardProfile::default();
        let bottom = DATA + 0x4000;
        let mut g = SynthGuest::new();
        g.with_task(&guards, TCB, TASK_STACK, 0);
        g.with_isr_stack(bottom, bottom + guards.headroom + 16);
        let engine = engine(&g);
        let err = engine
            .prepare(&mut g, &Continuations::default(), 3, &give_from_isr())
            .expect_err("the &woken scratch pushes the frame into the headroom");
        assert_eq!(err.kind, HleErrorKind::StackHeadroom);
        assert!(err.detail.contains("ISR stack"), "{err}");
        assert!(
            err.detail
                .contains(&format!("xIsrStackBottom {bottom:#010x}")),
            "{err}"
        );
        assert!(err.detail.contains("4 scratch byte(s) included"), "{err}");
    }

    #[test]
    fn the_scratch_bytes_count_towards_the_headroom() {
        // The same sp, the same callee: only the 4 scratch bytes of `&woken` differ, and they
        // push the frame below the limit (`g3-nested-call-stack`).
        let guards = GuardProfile::default();
        let bottom = DATA + 0x4000;
        let mut g = SynthGuest::new();
        g.with_task(&guards, TCB, TASK_STACK, 0);
        g.with_isr_stack(bottom, bottom + guards.headroom + 16);
        let engine = engine(&g);
        let without =
            CallRequest::new("xQueueGiveFromISR", GIVE_FROM_ISR, &[Arg::Val(0x1234)]).from_isr();
        assert!(
            engine
                .prepare(&mut g, &Continuations::default(), 3, &without)
                .is_ok(),
            "without scratch the 16-byte frame lands exactly on the limit"
        );
        assert_eq!(
            engine
                .prepare(&mut g, &Continuations::default(), 3, &give_from_isr())
                .expect_err("with scratch it does not")
                .kind,
            HleErrorKind::StackHeadroom
        );
    }

    #[test]
    fn the_isr_path_fails_closed_when_the_profile_did_not_bind_the_symbol() {
        let guards = GuardProfile::default();
        let mut g = SynthGuest::new();
        g.with_task(&guards, TCB, TASK_STACK, DATA + 0x5000);
        g.in_isr = true;
        let engine = CallEngine {
            pcs: crate::magic::MagicPcs::from_spec().expect("spec"),
            guards,
            stack: StackSymbols::default(),
        };
        let err = engine
            .prepare(&mut g, &Continuations::default(), 0, &give_from_isr())
            .expect_err("no xIsrStackBottom, no check, no call");
        assert_eq!(err.kind, HleErrorKind::StackHeadroom);
        assert!(err.detail.contains("xIsrStackBottom"), "{err}");
    }

    #[test]
    fn a_key_collision_is_refused() {
        let guards = GuardProfile::default();
        let mut g = SynthGuest::new();
        let sp = TASK_STACK + 0x1000;
        g.with_task(&guards, TCB, TASK_STACK, sp);
        let engine = engine(&g);
        let call = CallRequest::new("xQueueSemaphoreTake", SEM_TAKE, &[Arg::Val(0)]);
        let frame = engine
            .prepare(&mut g, &Continuations::default(), 0, &call)
            .expect("first call");
        let mut conts = Continuations::default();
        conts.insert(frame.key, crate::continuation::Continuation::default());
        let err = engine
            .prepare(&mut g, &conts, 0, &call)
            .expect_err("the same key twice");
        assert_eq!(err.kind, HleErrorKind::KeyCollision);
        assert!(
            err.detail.contains(&format!("{:#010x}", frame.sp1)),
            "{err}"
        );
    }

    #[test]
    fn a_scratch_block_over_the_profile_cap_is_refused() {
        let guards = GuardProfile::default();
        let mut g = SynthGuest::new();
        g.with_task(&guards, TCB, TASK_STACK, TASK_STACK + 0x2000);
        let engine = engine(&g);
        let call = CallRequest::new("esp_event_post", SEM_TAKE, &[Arg::Scratch(0)])
            .with_scratch(vec![0u8; usize::from(guards.max_scratch) + 1]);
        let err = engine
            .prepare(&mut g, &Continuations::default(), 0, &call)
            .expect_err("over the cap");
        assert_eq!(err.kind, HleErrorKind::ScratchTooLarge);
    }

    #[test]
    fn scratch_arguments_resolve_to_the_frame_and_come_back_after_the_call() {
        let guards = GuardProfile::default();
        let bottom = DATA + 0x4000;
        let mut g = SynthGuest::new();
        g.with_task(&guards, TCB, TASK_STACK, 0);
        g.with_isr_stack(bottom, bottom + ISR_STACK_BYTES);
        let engine = engine(&g);
        let call = give_from_isr();
        let frame = engine
            .prepare(&mut g, &Continuations::default(), 0, &call)
            .expect("prepare");
        assert_eq!(frame.a[0], 0x1234, "Arg::Val passes through");
        assert_eq!(frame.a[1], frame.sp1, "Arg::Scratch(0) is sp1 + 0");

        let pc = engine.start(&mut g, &frame, &call).expect("start");
        assert_eq!(pc, GIVE_FROM_ISR);
        assert_eq!(g.reg(RA), engine.pcs.ret, "ra is the magic return");
        assert_eq!(g.reg(SP), frame.sp1);
        assert_eq!(g.reg(A0), 0x1234);
        assert_eq!(g.reg(A0 + 1), frame.sp1);

        // The callee sets *pxHigherPriorityTaskWoken; the engine reads it back.
        g.poke(frame.sp1, 1);
        let scratch = engine
            .read_scratch(&mut g, frame.sp1, 4)
            .expect("read back");
        assert_eq!(scratch, 1u32.to_le_bytes());
    }

    #[test]
    fn a_scratch_pointer_past_the_block_is_refused() {
        // A 4-byte block with `Arg::Scratch(4)` would hand the callee a pointer to the first byte
        // above the block, and a word written there lands between sp1 + 4 and sp0.
        let guards = GuardProfile::default();
        let mut g = SynthGuest::new();
        g.with_task(&guards, TCB, TASK_STACK, TASK_STACK + 0x1000);
        let engine = engine(&g);
        for off in [1u16, 4, 0x100] {
            let call = CallRequest::new("xQueueGiveFromISR", GIVE_FROM_ISR, &[Arg::Scratch(off)])
                .with_scratch(vec![0u8; 4])
                .from_isr();
            let err = engine
                .prepare(&mut g, &Continuations::default(), 0, &call)
                .expect_err("the word at that offset is not inside the block");
            assert_eq!(err.kind, HleErrorKind::ScratchOutOfBounds, "{err}");
            assert!(
                err.detail.contains(&format!("Arg::Scratch({off})")),
                "{err}"
            );
        }
        // No scratch block at all, and a pointer into it.
        let call = CallRequest::new("xQueueGiveFromISR", GIVE_FROM_ISR, &[Arg::Scratch(0)]);
        assert_eq!(
            engine
                .prepare(&mut g, &Continuations::default(), 0, &call)
                .expect_err("empty block")
                .kind,
            HleErrorKind::ScratchOutOfBounds
        );
    }

    #[test]
    fn an_out_parameter_write_stays_below_the_callers_frame() {
        // Whatever the block length and alignment, the word behind every accepted scratch pointer
        // ends at or below sp0, so the callee cannot overwrite the interrupted frame.
        let guards = GuardProfile::default();
        let engine_guest = {
            let mut g = SynthGuest::new();
            g.with_task(&guards, TCB, TASK_STACK, TASK_STACK + 0x1000);
            g
        };
        let engine = engine(&engine_guest);
        for sp_low in 0..16u32 {
            for len in 4..=24usize {
                let mut g = engine_guest.clone();
                let sp0 = TASK_STACK + 0x1000 + sp_low;
                g.set_reg(SP, sp0);
                // Mark the caller's frame, then let the callee fill the last legal word.
                g.poke(sp0, 0xCAFE_F00D);
                let off = (len - SCRATCH_WORD) as u16;
                let call =
                    CallRequest::new("xQueueGiveFromISR", GIVE_FROM_ISR, &[Arg::Scratch(off)])
                        .with_scratch(vec![0u8; len]);
                let frame = engine
                    .prepare(&mut g, &Continuations::default(), 0, &call)
                    .expect("in bounds");
                engine.start(&mut g, &frame, &call).expect("start");
                let target = frame.a[0];
                assert!(u64::from(target) + SCRATCH_WORD as u64 <= u64::from(sp0));
                g.write(target, &[0xFF; SCRATCH_WORD])
                    .expect("callee write");
                assert_eq!(g.peek(sp0), 0xCAFE_F00D, "sp0 {sp0:#x} len {len}");
            }
        }
    }

    #[test]
    fn a_refused_call_changes_nothing_in_the_guest() {
        let guards = GuardProfile::default();
        let mut g = SynthGuest::new();
        let sp = TASK_STACK + guards.headroom;
        g.with_task(&guards, TCB, TASK_STACK, sp);
        g.set_reg(RA, 0xDEAD_BEEF);
        let engine = engine(&g);
        let call = CallRequest::new("xQueueSemaphoreTake", SEM_TAKE, &[Arg::Val(0)]);
        assert!(
            engine
                .prepare(&mut g, &Continuations::default(), 0, &call)
                .is_err()
        );
        assert_eq!(g.reg(SP), sp);
        assert_eq!(g.reg(RA), 0xDEAD_BEEF);
    }
}
