//! Radio workers: guest tasks entered at `MagicPcs::wifi_worker` and `MagicPcs::bt_worker` that
//! park (`HleAction::Park`) until a radio event wakes them.
//!
//! Two wake modes. U5 polls a semaphore; U4 (the magic ISR) raises the radio interrupt and gives
//! the semaphore. [`WakeMode::default`] is U4 because it matches silicon: the
//! `probe_campaign_radio` capture times HCI Reset and Read Local Version at about 520/80 us, which
//! U4 reproduces, while U5 answers only at its next 20 ms poll and wakes a priority-23 task 50
//! times a second. The base profiles ([`bt_controller_profile`], [`wifi_profile`]) name no
//! interrupt source, so they stay U5 (`crate::core::check_worker` refuses U4 without one).
//!
//! The module's init handler creates the semaphore and the task through [`WorkerCalls`]
//! (IDF FreeRTOS `xTaskCreatePinnedToCore`), with the semaphore as `pvParameters`; the task
//! enters at the magic worker PC, where the core records it ([`WorkerState`]). A park becomes a
//! nested `xQueueSemaphoreTake`: the poll interval in U5, `portMAX_DELAY` in U4, 0 while an event
//! it has not been woken for is queued. In U4 the magic ISR gives through `xQueueGiveFromISR` and
//! yields when `pxHigherPriorityTaskWoken` comes back set; the core lowers the level at the magic
//! ISR return.

use std::collections::VecDeque;

use pemu_core::irq_source::IrqSource;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::snap::{SectionId, SnapError, SnapReader, SnapValue, snap_struct};

use crate::guest_call::{Arg, CallRequest};
use crate::magic::{MagicKind, MagicPcs};

/// Why a parked worker was woken, carried by `Resume::Woken`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub enum WakeReason {
    #[default]
    Event,
    /// The wait timed out: the U5 poll interval, or a driver deadline.
    Timeout,
    /// The feature is being torn down and the worker must exit.
    Deinit,
}

/// Which wake-up mechanism a profile uses.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub enum WakeMode {
    /// The worker blocks on its semaphore with a timeout and re-checks the queue.
    U5Polling,
    /// A raised radio source reaches the magic ISR, which gives the worker semaphore.
    #[default]
    U4MagicIsr,
}

/// The driver task a radio HLE creates for itself. Every value comes from the per-IDF profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerProfile {
    /// `wifi` or `btController`.
    pub task_name: &'static str,
    /// `configMAX_PRIORITIES - 2` = 23 for both drivers.
    pub priority: u32,
    /// Bytes.
    pub stack_bytes: u32,
    /// The C3 has one core.
    pub core_id: u32,
    pub entry: MagicKind,
    pub isr_entry: MagicKind,
    /// `None` until a profile sets it: UNVERIFIED for BLE. A U4 event on a profile without one
    /// is refused, never raised on a guess.
    pub isr_source: Option<IrqSource>,
    /// U5 poll interval in FreeRTOS ticks, used when [`WorkerProfile::poll_us`] is `None` or the
    /// guest's tick rate cannot be read.
    pub poll_ticks: u32,
    /// U5 poll interval in microseconds. 20 ms is 20 ticks only at a 1 kHz tick; at
    /// `CONFIG_FREERTOS_HZ` 100 the same 20 ticks are 200 ms. When set, a park waits
    /// [`WorkerProfile::poll_ticks_for`] the guest's own tick rate.
    pub poll_us: Option<u32>,
    pub wake: WakeMode,
    /// Largest scratch block a nested call from this worker's task may ask for, `None` for
    /// [`crate::guest_call::GuardProfile::max_scratch`]. The BLE worker hands whole H4 packets (up
    /// to 259 bytes) to `notify_host_recv` through scratch, so its profile raises the limit.
    pub max_scratch: Option<u16>,
}

/// One asynchronous radio event (air packet, LAN frame, scan-done timer), owned by `pemu-radio`.
/// `pemu-hle` never looks inside the payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct RadioEvent {
    pub tag: u16,
    pub payload: Vec<u8>,
}

/// The wake-up half of a worker, snapshot state in `HleSection::workers`. The core lowers the
/// radio source level exactly once per event, at the magic ISR return; an event that arrives while
/// the ISR runs is a separate raise, so no event is swallowed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct WakeEngine {
    mode: WakeMode,
    /// Events waiting for the worker. The worker drains this in task context after the interrupt
    /// has exited, so it is not what the interrupt level tracks.
    queue: VecDeque<RadioEvent>,
    level: bool,
    /// Events raised but not yet taken into a magic ISR: the device's pending flag.
    pending: u32,
    /// Events taken into an ISR but not yet acknowledged by its return.
    in_flight: u32,
    lowered: u32,
    /// Set while the last `xQueueGiveFromISR` reported a higher-priority task woken.
    yield_requested: bool,
    /// True when an event was posted since the worker was last woken for one. The module drains
    /// the queue at its own pace, so "the queue is not empty" cannot be the test, or a worker that
    /// has not drained yet would never sleep.
    unseen: bool,
}

impl WakeEngine {
    pub fn new(mode: WakeMode) -> WakeEngine {
        WakeEngine {
            mode,
            ..WakeEngine::default()
        }
    }

    pub fn mode(&self) -> WakeMode {
        self.mode
    }

    /// In U4 the radio source level goes up and the guest takes the interrupt when it next can, so
    /// an event raised inside a critical section is delivered when the section ends. In U5 nothing
    /// is raised: the polling worker finds the event on its next wake.
    pub fn post(&mut self, event: RadioEvent) {
        self.queue.push_back(event);
        self.unseen = true;
        if self.mode == WakeMode::U4MagicIsr {
            self.pending += 1;
            self.level = true;
        }
    }

    pub fn level(&self) -> bool {
        self.level
    }

    /// The magic ISR was entered: one raised event moves from pending to in flight.
    pub fn on_isr_entry(&mut self) {
        if self.pending > 0 {
            self.pending -= 1;
            self.in_flight += 1;
        }
    }

    /// The magic ISR returned: the core lowers the level, consuming the device's pending flag. It
    /// goes back up at once when an event arrived while the ISR ran.
    pub fn on_isr_return(&mut self) {
        if self.in_flight > 0 {
            self.in_flight -= 1;
            self.lowered += 1;
        }
        self.level = self.pending > 0;
    }

    pub fn lowered(&self) -> u32 {
        self.lowered
    }

    /// Carries the lowering count over to a fresh engine, so a worker deleted and created again
    /// keeps counting (the count is a diagnostic of the whole run).
    pub fn restore_lowered(&mut self, lowered: u32) {
        self.lowered = lowered;
    }

    /// Records the `&woken` out-parameter of `xQueueGiveFromISR`.
    pub fn set_woken(&mut self, woken: bool) {
        self.yield_requested = woken;
    }

    /// Whether the last give asked for a switch at interrupt exit.
    pub fn yield_requested(&self) -> bool {
        self.yield_requested
    }

    pub fn take_yield(&mut self) -> bool {
        std::mem::take(&mut self.yield_requested)
    }

    /// The worker loop drains these before parking again.
    pub fn next_event(&mut self) -> Option<RadioEvent> {
        self.queue.pop_front()
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn unseen(&self) -> bool {
        self.unseen
    }

    pub fn mark_seen(&mut self) {
        self.unseen = false;
    }

    /// `None` to stay parked.
    pub fn wake_reason(&self) -> Option<WakeReason> {
        if self.queue.is_empty() {
            None
        } else {
            Some(WakeReason::Event)
        }
    }
}

/// The runtime state of one worker, in `HleSection::workers` under its magic entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct WorkerState {
    /// `pxCurrentTCBs` when it first entered the magic worker PC.
    pub task: u32,
    /// The worker's binary semaphore, the task's `pvParameters`.
    pub semaphore: u32,
    pub wake: WakeEngine,
    /// True once a teardown was requested: the next wake is `WakeReason::Deinit`.
    pub deinit: bool,
}

/// Registration of one worker with the core. Configuration derived from binding, not snapshot
/// state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerConfig {
    pub profile: WorkerProfile,
    pub calls: WorkerCalls,
}

/// `portMAX_DELAY`: the U4 wait blocks until the magic ISR gives.
pub const PORT_MAX_DELAY: u32 = u32::MAX;

/// `pdTRUE`: `xQueueSemaphoreTake` took the semaphore.
pub const PD_TRUE: u32 = 1;

impl SnapValue for WakeMode {
    fn snap_write(&self, out: &mut Vec<u8>) {
        (*self as u8).snap_write(out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<WakeMode, SnapError> {
        match u8::snap_read(r)? {
            0 => Ok(WakeMode::U5Polling),
            1 => Ok(WakeMode::U4MagicIsr),
            _ => Err(SnapError::Malformed {
                at: SectionId::HLE,
                reason: "WakeMode is not U5Polling or U4MagicIsr",
            }),
        }
    }
}

snap_struct!(RadioEvent { tag, payload });

impl SnapValue for WakeEngine {
    fn snap_write(&self, out: &mut Vec<u8>) {
        self.mode.snap_write(out);
        (self.queue.len() as u64).snap_write(out);
        for event in &self.queue {
            event.snap_write(out);
        }
        self.level.snap_write(out);
        self.pending.snap_write(out);
        self.in_flight.snap_write(out);
        self.lowered.snap_write(out);
        self.yield_requested.snap_write(out);
        self.unseen.snap_write(out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<WakeEngine, SnapError> {
        let mode = WakeMode::snap_read(r)?;
        let len = u64::snap_read(r)?;
        let mut queue = VecDeque::new();
        for _ in 0..len {
            queue.push_back(RadioEvent::snap_read(r)?);
        }
        Ok(WakeEngine {
            mode,
            queue,
            level: bool::snap_read(r)?,
            pending: u32::snap_read(r)?,
            in_flight: u32::snap_read(r)?,
            lowered: u32::snap_read(r)?,
            yield_requested: bool::snap_read(r)?,
            unseen: bool::snap_read(r)?,
        })
    }
}

snap_struct!(WorkerState {
    task,
    semaphore,
    wake,
    deinit
});

/// The nested calls a radio HLE makes to build its worker, at addresses the binding resolved.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WorkerCalls {
    pub task_create: u32,
    pub queue_create: u32,
    /// The U5 wait.
    pub semaphore_take: u32,
    /// The U4 give.
    pub give_from_isr: u32,
    pub yield_from_isr: u32,
}

impl WorkerCalls {
    /// `xTaskCreatePinnedToCore(entry, name, stack, semaphore, prio, &handle, core)`.
    ///
    /// The name and `&handle` live in the scratch block, on the caller's stack, so the headroom
    /// guard counts them; FreeRTOS copies the name into the TCB. The semaphore is `pvParameters`,
    /// so it reaches the magic worker entry in `a0` and the core learns it there.
    pub fn create_worker(
        &self,
        pcs: &MagicPcs,
        profile: &WorkerProfile,
        semaphore: u32,
    ) -> CallRequest {
        let name = profile.task_name.as_bytes();
        // [0..4) the `&handle` out-parameter, then the NUL-terminated task name.
        let mut scratch = vec![0u8; 4];
        scratch.extend_from_slice(name);
        scratch.push(0);
        CallRequest::new(
            "xTaskCreatePinnedToCore",
            self.task_create,
            &[
                Arg::Val(pcs.pc_of(profile.entry)),
                Arg::Scratch(4),
                Arg::Val(profile.stack_bytes),
                Arg::Val(semaphore),
                Arg::Val(profile.priority),
                Arg::Scratch(0),
                Arg::Val(profile.core_id),
            ],
        )
        .with_scratch(scratch)
    }

    /// `xQueueGenericCreate(1, 0, queueQUEUE_TYPE_BINARY_SEMAPHORE)`.
    pub fn create_semaphore(&self) -> CallRequest {
        CallRequest::new(
            "xQueueGenericCreate",
            self.queue_create,
            &[
                Arg::Val(1),
                Arg::Val(0),
                Arg::Val(QUEUE_TYPE_BINARY_SEMAPHORE),
            ],
        )
    }

    /// `xQueueSemaphoreTake(sem, ticks)`: what `HleAction::Park` becomes for a registered worker.
    pub fn park(&self, semaphore: u32, ticks: u32) -> CallRequest {
        CallRequest::new(
            "xQueueSemaphoreTake",
            self.semaphore_take,
            &[Arg::Val(semaphore), Arg::Val(ticks)],
        )
    }

    /// `xQueueGiveFromISR(sem, &woken)`, with `&woken` in the ISR frame. Non-blocking, which is
    /// what makes it legal inside an ISR.
    pub fn give_from_isr(&self, semaphore: u32) -> CallRequest {
        CallRequest::new(
            "xQueueGiveFromISR",
            self.give_from_isr,
            &[Arg::Val(semaphore), Arg::Scratch(0)],
        )
        .with_scratch(vec![0u8; 4])
        .from_isr()
    }

    /// The port's yield-from-ISR function, called when `&woken` came back set.
    pub fn request_yield(&self) -> CallRequest {
        CallRequest::new("vPortYieldFromISR", self.yield_from_isr, &[]).from_isr()
    }
}

impl WorkerProfile {
    /// [`WorkerProfile::poll_us`] at `tick_hz`, rounded up so the wait is never shorter than the
    /// interval and never 0 (a 0-tick `xQueueSemaphoreTake` does not block), or
    /// [`WorkerProfile::poll_ticks`] when the profile has no time or the rate is unknown.
    pub fn poll_ticks_for(&self, tick_hz: Option<u32>) -> u32 {
        match (self.poll_us, tick_hz) {
            (Some(us), Some(hz)) if hz != 0 => {
                let ticks = (u64::from(us) * u64::from(hz)).div_ceil(1_000_000);
                u32::try_from(ticks.max(1)).unwrap_or(u32::MAX)
            }
            _ => self.poll_ticks,
        }
    }
}

pub const QUEUE_TYPE_BINARY_SEMAPHORE: u32 = 3;

/// The `btController` profile: priority 23, a 4,096 B stack (3,584 + 512 of `esp_task.h`) and a
/// 20 ms U5 poll. The interrupt source is UNVERIFIED and stays `None`.
pub fn bt_controller_profile() -> WorkerProfile {
    WorkerProfile {
        task_name: "btController",
        priority: 23,
        stack_bytes: 4096,
        core_id: 0,
        entry: MagicKind::BtWorker,
        isr_entry: MagicKind::BtIsr,
        isr_source: None,
        poll_ticks: 20,
        poll_us: None,
        // No interrupt source, so U5: a U4 profile needs one.
        wake: WakeMode::U5Polling,
        max_scratch: None,
    }
}

/// The `wifi` profile: priority 23 and the stack the `feature_caps` formula gives, 6,656 B for the
/// probe sdkconfig.
pub fn wifi_profile() -> WorkerProfile {
    WorkerProfile {
        task_name: "wifi",
        priority: 23,
        stack_bytes: 6656,
        core_id: 0,
        entry: MagicKind::WifiWorker,
        isr_entry: MagicKind::WifiIsr,
        isr_source: None,
        poll_ticks: 20,
        poll_us: None,
        // No interrupt source, so U5: a U4 profile needs one.
        wake: WakeMode::U5Polling,
        max_scratch: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poll_in_time_is_the_guests_own_ticks_rounded_up() {
        // 20 ms is 20 ticks at the corpus's 1 kHz, so every pinned corpus timing is
        // unchanged, and 2 ticks at `probe_wifi_http`'s 100 Hz rather than the 200 ms that 20
        // ticks would be there.
        let timed = WorkerProfile {
            poll_us: Some(20_000),
            ..wifi_profile()
        };
        assert_eq!(timed.poll_ticks_for(Some(1_000)), 20);
        assert_eq!(timed.poll_ticks_for(Some(100)), 2);
        // Rounded up and never 0, since a 0-tick take does not block.
        assert_eq!(timed.poll_ticks_for(Some(30)), 1);
        let short = WorkerProfile {
            poll_us: Some(1),
            ..wifi_profile()
        };
        assert_eq!(short.poll_ticks_for(Some(1_000)), 1);
        // An unknown rate, or a profile with no time, keeps the tick count.
        assert_eq!(timed.poll_ticks_for(None), timed.poll_ticks);
        assert_eq!(bt_controller_profile().poll_ticks_for(Some(100)), 20);
    }

    fn event(tag: u16) -> RadioEvent {
        RadioEvent {
            tag,
            payload: vec![tag as u8, 0xAA],
        }
    }

    #[test]
    fn u4_magic_isr_is_the_default_wake_mode() {
        // The source-less base profiles stay U5, since U4 without a source is refused.
        assert_eq!(WakeMode::default(), WakeMode::U4MagicIsr);
        assert_eq!(bt_controller_profile().wake, WakeMode::U5Polling);
        assert_eq!(bt_controller_profile().isr_source, None);
        assert_eq!(wifi_profile().wake, WakeMode::U5Polling);
        assert_eq!(wifi_profile().isr_source, None);
    }

    #[test]
    fn the_worker_profiles_are_the_arch_values() {
        let bt = bt_controller_profile();
        assert_eq!(bt.task_name, "btController");
        assert_eq!(bt.priority, 23);
        assert_eq!(bt.stack_bytes, 4096);
        assert_eq!(bt.isr_source, None, "UNVERIFIED for BLE");
        let wifi = wifi_profile();
        assert_eq!(wifi.task_name, "wifi");
        assert_eq!(wifi.priority, 23);
        assert_eq!(wifi.stack_bytes, 6656);
    }

    #[test]
    fn u5_queues_an_event_without_raising_anything() {
        let mut engine = WakeEngine::new(WakeMode::U5Polling);
        engine.post(event(1));
        assert!(!engine.level(), "U5 raises no interrupt line");
        assert_eq!(engine.wake_reason(), Some(WakeReason::Event));
    }

    #[test]
    fn u4_raises_the_level_and_the_core_lowers_it_once_per_event() {
        let mut engine = WakeEngine::new(WakeMode::U4MagicIsr);
        engine.post(event(1));
        assert!(engine.level());
        engine.on_isr_entry();
        assert_eq!(engine.next_event().map(|e| e.tag), Some(1));
        engine.on_isr_return();
        assert!(
            !engine.level(),
            "the level goes down at the magic ISR return"
        );
        assert_eq!(engine.lowered(), 1);
        engine.on_isr_return();
        assert_eq!(engine.lowered(), 1);
    }

    #[test]
    fn an_event_that_arrives_while_the_isr_runs_raises_the_level_again() {
        let mut engine = WakeEngine::new(WakeMode::U4MagicIsr);
        engine.post(event(1));
        engine.on_isr_entry();
        engine.next_event();
        engine.post(event(2)); // arrives while the ISR is still running
        engine.on_isr_return();
        assert!(engine.level(), "the second event is not swallowed");
        assert_eq!(engine.lowered(), 1);
        engine.on_isr_entry();
        engine.next_event();
        engine.on_isr_return();
        assert_eq!(engine.lowered(), 2, "one lowering per event");
        assert!(!engine.level());
    }

    #[test]
    fn a_woken_out_parameter_asks_for_a_switch_at_interrupt_exit() {
        let mut engine = WakeEngine::new(WakeMode::U4MagicIsr);
        assert!(!engine.yield_requested());
        engine.set_woken(true);
        assert!(engine.yield_requested());
        assert!(engine.take_yield());
        assert!(!engine.yield_requested(), "the request is consumed once");
    }

    #[test]
    fn worker_state_round_trips_through_its_snapshot_codec() {
        let mut wake = WakeEngine::new(WakeMode::U4MagicIsr);
        wake.post(event(3));
        wake.post(event(4));
        wake.on_isr_entry();
        wake.set_woken(true);
        let state = WorkerState {
            task: 0x3FC8_6000,
            semaphore: 0x3FCA_9000,
            wake,
            deinit: true,
        };
        let mut bytes = Vec::new();
        state.snap_write(&mut bytes);
        let mut r = SnapReader::new(&bytes, SectionId::HLE);
        assert_eq!(WorkerState::snap_read(&mut r).expect("decode"), state);
        assert!(r.is_empty());
    }

    #[test]
    fn the_worker_creation_call_carries_the_profile_values_and_the_semaphore() {
        let pcs = MagicPcs::from_spec().expect("spec");
        let calls = WorkerCalls {
            task_create: 0x4039_10F0,
            queue_create: 0x4039_2000,
            semaphore_take: 0x4039_3000,
            give_from_isr: 0x4039_0F6A,
            yield_from_isr: 0x4038_B87C,
        };
        let profile = bt_controller_profile();
        let call = calls.create_worker(&pcs, &profile, 0x3FCA_9000);
        assert_eq!(call.func, calls.task_create);
        assert_eq!(call.nargs, 7);
        assert_eq!(call.args[0], Arg::Val(pcs.bt_worker));
        assert_eq!(call.args[1], Arg::Scratch(4), "the task name is in scratch");
        assert_eq!(call.args[2], Arg::Val(4096));
        assert_eq!(call.args[3], Arg::Val(0x3FCA_9000), "pvParameters");
        assert_eq!(call.args[4], Arg::Val(23));
        assert_eq!(call.args[5], Arg::Scratch(0), "&handle is in scratch");
        assert_eq!(&call.scratch[4..], b"btController\0");
        assert!(call.blocking, "task creation runs in task context");
    }

    #[test]
    fn the_isr_calls_are_from_isr_calls() {
        let calls = WorkerCalls {
            task_create: 0,
            queue_create: 0,
            semaphore_take: 0x4039_3000,
            give_from_isr: 0x4039_0F6A,
            yield_from_isr: 0x4038_B87C,
        };
        let give = calls.give_from_isr(0x1234);
        assert!(!give.blocking, "only FromISR calls are legal inside an ISR");
        assert_eq!(give.scratch.len(), 4, "&woken lives in the ISR frame");
        assert_eq!(give.args[1], Arg::Scratch(0));
        assert!(!calls.request_yield().blocking);
        let park = calls.park(0x1234, 20);
        assert!(park.blocking);
        assert_eq!(park.args[1], Arg::Val(20));
    }
}
