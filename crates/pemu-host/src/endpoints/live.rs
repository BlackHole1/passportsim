//! The live side every USB Serial/JTAG endpoint shares: one instance running on its own thread,
//! paced to the host clock, with a [`UsjLink`] between it and the client's transport.
//!
//! esptool, `idf.py monitor` and a raw console work against wall-clock timeouts (a 100 ms SYNC
//! timeout, a 3 s command timeout), so while a client is attached the guest must keep running
//! without anyone calling `run`. [`LiveRunner`] runs it in short slices with the sleep-and-correct
//! [`Pacer`]. Everything a client sends is journaled at a slice boundary (`SerialIn`, `UsbLine`),
//! and the real ROM and stub answer the download protocol.
//!
//! A `SerialIn` is sized to what `HostIo::usj_rx` can take; the rest waits in the link, and a
//! client that outruns the guest blocks once [`TO_GUEST_LIMIT`] bytes wait, like the NAK of a full
//! USB OUT endpoint.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pemu_core::input::{InputEvent, SerialChan};
use pemu_core::journal::Origin;
use pemu_core::snap::{LivePolicy, SnapError, SnapOpts, Snapshot};
use pemu_core::time::VTime;
use pemu_machine::SnapshotMachine;
use pemu_machine::machine::{At, MachineApi};
use pemu_machine::run::{RunLimits, RunOutcome};
use pemu_machine::stops::{StopReason, StopSet};

use super::rfc2217::LineState;
use crate::pacing::{Pace, Pacer};

pub const TO_GUEST_LIMIT: usize = 256 * 1024;

/// Guest bytes kept for one slow transport before its oldest are dropped and counted.
pub const FROM_GUEST_LIMIT: usize = 1024 * 1024;

/// The virtual length of one live slice: a reply leaves within a millisecond of wall time at 1x,
/// the SOF period that moves the USJ endpoints.
pub const SLICE: VTime = VTime::from_ms(1);

const IDLE_POLL: Duration = Duration::from_millis(2);

#[derive(Clone, Debug, PartialEq, Eq)]
enum HostCmd {
    Bytes(VecDeque<u8>),
    Line(LineState),
}

/// One transport's own output queue, so a TCP client and a pty on one instance each get every
/// byte, and one leaving or reading slowly never touches the other.
#[derive(Debug, Default)]
struct Tapped {
    from_guest: VecDeque<u8>,
}

/// The name of one subscription, for a transport whose decoder thread must name the queue its
/// writer thread holds ([`UsjLink::purge_tap`]). A detached tap's id names nothing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TapId(u64);

/// A transport's subscription to the guest output. Not `Clone`: the attaching transport is the
/// reader, and dropping it detaches on every exit path.
#[derive(Debug)]
pub struct Tap {
    id: TapId,
    link: Arc<UsjLink>,
}

impl Tap {
    pub fn id(&self) -> TapId {
        self.id
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.link.lock().taps.remove(&self.id);
    }
}

#[derive(Debug, Default)]
struct LinkInner {
    to_guest: VecDeque<HostCmd>,
    to_guest_bytes: usize,
    taps: BTreeMap<TapId, Tapped>,
    next_tap: u64,
    /// The line state last handed to the machine, so a new session starts from it.
    line: LineState,
    dropped_from_guest: u64,
    /// Bytes a client `PURGE-DATA` discarded, kept apart from `dropped_from_guest` because a purge
    /// was asked for.
    purged_to_guest: u64,
    purged_from_guest: u64,
    stopped: bool,
    vt: VTime,
    last_stop: Option<String>,
    qos: Option<&'static str>,
    reanchors: u64,
}

#[derive(Debug, Default)]
pub struct UsjLink {
    inner: Mutex<LinkInner>,
    changed: Condvar,
}

impl UsjLink {
    fn lock(&self) -> MutexGuard<'_, LinkInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues bytes for the guest, blocking while [`TO_GUEST_LIMIT`] bytes already wait. `false`
    /// once the runner has stopped.
    pub fn send_bytes(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return !self.lock().stopped;
        }
        let mut inner = self.lock();
        while inner.to_guest_bytes >= TO_GUEST_LIMIT && !inner.stopped {
            inner = self
                .changed
                .wait_timeout(inner, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if inner.stopped {
            return false;
        }
        inner.to_guest_bytes += bytes.len();
        match inner.to_guest.back_mut() {
            Some(HostCmd::Bytes(tail)) => tail.extend(bytes),
            _ => inner
                .to_guest
                .push_back(HostCmd::Bytes(bytes.iter().copied().collect())),
        }
        true
    }

    /// Queues a line-state change, after every byte already queued.
    pub fn set_line(&self, line: LineState) {
        self.lock().to_guest.push_back(HostCmd::Line(line));
    }

    /// Drops the bytes still waiting for the guest (`PURGE-DATA` 2 or 3) and returns the count.
    /// Queued line changes stay: a `SET-CONTROL` already sent is a reset the client still expects.
    pub fn purge_to_guest(&self) -> usize {
        let mut inner = self.lock();
        let dropped = inner.to_guest_bytes;
        inner.to_guest.retain(|c| matches!(c, HostCmd::Line(_)));
        inner.to_guest_bytes = 0;
        inner.purged_to_guest += dropped as u64;
        self.changed.notify_all();
        dropped
    }

    /// Drops the guest bytes `tap` has not read (`PURGE-DATA` 1 or 3) and returns the count. Only
    /// the asking transport's queue, so `idf.py monitor` keeps its boot log while esptool purges.
    /// Bytes not yet fanned out stay, as with a client-side flush. An unknown id drops nothing.
    pub fn purge_tap(&self, tap: TapId) -> usize {
        let mut inner = self.lock();
        let Some(tapped) = inner.taps.get_mut(&tap) else {
            return 0;
        };
        let dropped = tapped.from_guest.len();
        tapped.from_guest.clear();
        inner.purged_from_guest += dropped as u64;
        self.changed.notify_all();
        dropped
    }

    /// Bytes [`UsjLink::purge_to_guest`] has discarded, for tests: nothing reports it.
    pub fn purged_to_guest(&self) -> u64 {
        self.lock().purged_to_guest
    }

    /// Bytes [`UsjLink::purge_tap`] has discarded over every tap, for tests.
    pub fn purged_from_guest(&self) -> u64 {
        self.lock().purged_from_guest
    }

    /// Starts buffering guest output for one transport, from the next byte. An attached transport
    /// is a connected host tool; with none, a [`LiveRunner`] does not advance the guest.
    pub fn attach(self: &Arc<Self>) -> Tap {
        let mut inner = self.lock();
        let id = TapId(inner.next_tap);
        inner.next_tap += 1;
        inner.taps.insert(id, Tapped::default());
        Tap {
            id,
            link: Arc::clone(self),
        }
    }

    pub fn detach(&self, tap: Tap) {
        drop(tap);
    }

    pub fn attached(&self) -> usize {
        self.lock().taps.len()
    }

    /// Moves up to `out.len()` of `tap`'s bytes into `out`, waiting at most `timeout` for the
    /// first. `None` once the runner has stopped and nothing is left.
    pub fn read_output(&self, tap: &Tap, out: &mut [u8], timeout: Duration) -> Option<usize> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            let queue = &mut inner.taps.get_mut(&tap.id)?.from_guest;
            if !queue.is_empty() {
                let n = out.len().min(queue.len());
                for (slot, b) in out.iter_mut().zip(queue.drain(..n)) {
                    *slot = b;
                }
                return Some(n);
            }
            if inner.stopped {
                return None;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Some(0);
            };
            inner = self
                .changed
                .wait_timeout(inner, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub fn line(&self) -> LineState {
        self.lock().line
    }

    pub fn stopped(&self) -> bool {
        self.lock().stopped
    }

    pub fn vt(&self) -> VTime {
        self.lock().vt
    }

    pub fn dropped_from_guest(&self) -> u64 {
        self.lock().dropped_from_guest
    }

    /// The last stop other than the slice limit (a panic, a tripwire, a deadlock), so `status` can
    /// say why a guest stopped answering.
    pub fn last_stop(&self) -> Option<String> {
        self.lock().last_stop.clone()
    }

    /// The live thread's scheduling class; `None` before it started, and for an agent-driven link.
    pub fn qos(&self) -> Option<&'static str> {
        self.lock().qos
    }

    /// How often the live pacer fell more than [`crate::pacing::MAX_LAG`] behind and re-anchored.
    /// Re-anchors that forget an idle stretch (no client, a halted guest) are not counted.
    pub fn reanchors(&self) -> u64 {
        self.lock().reanchors
    }

    pub fn to_guest_drained(&self) -> bool {
        self.lock().to_guest.is_empty()
    }
}

pub trait LiveTarget: Send + 'static {
    fn api(&mut self) -> &mut dyn MachineApi;
    /// Whether the host asked the target to end (a daemon shutdown); the runner stops at its next
    /// slice boundary.
    fn cancelled(&self) -> bool {
        false
    }
    /// Counts `insns` retired by one live slice, where the target keeps a count.
    fn account(&mut self, insns: u64) {
        let _ = insns;
    }
    /// Records the live thread's class, where the target keeps one.
    fn note_qos(&mut self, class: &'static str) {
        let _ = class;
    }
}

impl LiveTarget for pemu_machine::machine::Machine {
    fn api(&mut self) -> &mut dyn MachineApi {
        self
    }
}

impl LiveTarget for Box<dyn MachineApi + Send> {
    fn api(&mut self) -> &mut dyn MachineApi {
        self.as_mut()
    }
}

impl LiveTarget for pemu_api::commands::start::Session {
    fn api(&mut self) -> &mut dyn MachineApi {
        self.machine()
    }
    fn cancelled(&self) -> bool {
        pemu_api::commands::start::Session::cancelled(self)
    }
    fn account(&mut self, insns: u64) {
        self.insns = self.insns.saturating_add(insns);
    }
    fn note_qos(&mut self, class: &'static str) {
        self.note_host_qos(class);
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Pacing {
    /// `Wall { rate }`: 1.0 is real time.
    Wall(f64),
    /// As fast as the host runs it.
    Max,
}

type Job<T> = Box<dyn FnOnce(&mut T) + Send>;

type Jobs<T> = Arc<Mutex<Vec<Job<T>>>>;

pub struct LiveRunner<T: LiveTarget> {
    link: Arc<UsjLink>,
    stop: Arc<AtomicBool>,
    jobs: Jobs<T>,
    thread: Option<JoinHandle<Option<T>>>,
}

impl<T: LiveTarget> std::fmt::Debug for LiveRunner<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveRunner")
            .field("link", &self.link)
            .field("stopped", &self.stop.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl<T: LiveTarget> LiveRunner<T> {
    /// Starts `target` running at `pacing`. The link starts from the machine's line state, so a
    /// first `SET-CONTROL` repeating it is not an event. The thread is spawned before the target
    /// moves into it, so a spawn failure hands the target back.
    pub fn start(mut target: T, pacing: Pacing) -> Result<LiveRunner<T>, (T, std::io::Error)> {
        let link = UsjLink::starting_at(target.api());
        let stop = Arc::new(AtomicBool::new(false));
        let jobs: Jobs<T> = Arc::default();
        let (send, receive) = std::sync::mpsc::sync_channel::<T>(1);
        let spawned = std::thread::Builder::new()
            .name("usj-live".to_owned())
            .stack_size(crate::pool::INSTANCE_STACK_BYTES)
            .spawn({
                let link = Arc::clone(&link);
                let stop = Arc::clone(&stop);
                let jobs = Arc::clone(&jobs);
                move || {
                    let target = receive.recv().ok()?;
                    Some(run_live(target, &link, &stop, &jobs, pacing))
                }
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => return Err((target, e)),
        };
        if let Err(std::sync::mpsc::SendError(target)) = send.send(target) {
            return Err((
                target,
                std::io::Error::other("the live thread ended at once"),
            ));
        }
        Ok(LiveRunner {
            link,
            stop,
            jobs,
            thread: Some(thread),
        })
    }

    /// Runs `job` against the target between two slices and returns its result; `None` once the
    /// runner has stopped. This reads a running instance without stopping it (a region digest's
    /// flash bytes, the console since a cursor); the job's reads are not guest input.
    pub fn call<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut T) -> R + Send + 'static,
    ) -> Option<R> {
        let (send, receive) = std::sync::mpsc::sync_channel::<R>(1);
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Box::new(move |target: &mut T| {
                let _ = send.send(job(target));
            }));
        loop {
            match receive.recv_timeout(Duration::from_millis(20)) {
                Ok(result) => return Some(result),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if self.link.stopped() => {
                    return None;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    pub fn link(&self) -> Arc<UsjLink> {
        Arc::clone(&self.link)
    }

    /// Stops the thread at its next slice boundary and returns the target, `None` if it panicked.
    pub fn stop(mut self) -> Option<T> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().and_then(|t| t.join().ok().flatten())
    }
}

impl<T: LiveTarget> Drop for LiveRunner<T> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Hands the queued host actions to the machine at its current instant, sized to `usj_rx`, and
/// says whether it journaled anything.
fn journal_host_actions(m: &mut dyn MachineApi, link: &UsjLink) -> bool {
    let mut inner = link.lock();
    let mut free = m.io().usj_rx.free();
    let mut moved = false;
    let mut journaled = false;
    while let Some(cmd) = inner.to_guest.front_mut() {
        match cmd {
            HostCmd::Line(line) => {
                let line = *line;
                inner.to_guest.pop_front();
                journaled = true;
                // An endpoint client's input makes the run `replayable`.
                if m.input_from(
                    At::Now,
                    Origin::Endpoint,
                    InputEvent::UsbLine {
                        dtr: line.dtr,
                        rts: line.rts,
                    },
                )
                .is_ok()
                {
                    inner.line = line;
                }
            }
            HostCmd::Bytes(bytes) => {
                let n = bytes.len().min(free);
                if n == 0 {
                    break;
                }
                let data: Vec<u8> = bytes.drain(..n).collect();
                let empty = bytes.is_empty();
                if empty {
                    inner.to_guest.pop_front();
                }
                inner.to_guest_bytes -= n;
                free -= n;
                moved = true;
                journaled = true;
                let _ = m.input_from(
                    At::Now,
                    Origin::Endpoint,
                    InputEvent::SerialIn {
                        chan: SerialChan::USJ,
                        data,
                    },
                );
                if !empty {
                    break;
                }
            }
        }
    }
    if moved {
        link.changed.notify_all();
    }
    journaled
}

fn collect_output(m: &mut dyn MachineApi, link: &UsjLink, cursor: &mut u64) {
    let ring = &m.io().usj_tx;
    let head = ring.head();
    // A restore moves the ring back; output resumes from where it now stands.
    if head < *cursor {
        *cursor = head;
    }
    if head == *cursor {
        return;
    }
    let mut inner = link.lock();
    if !inner.taps.is_empty() {
        let slices = ring.slices(*cursor);
        let mut dropped = 0u64;
        for tapped in inner.taps.values_mut() {
            tapped.from_guest.extend(slices.iter().copied());
            let over = tapped.from_guest.len().saturating_sub(FROM_GUEST_LIMIT);
            if over > 0 {
                tapped.from_guest.drain(..over);
                dropped += over as u64;
            }
        }
        inner.dropped_from_guest += dropped;
        link.changed.notify_all();
    }
    *cursor = head;
}

fn run_live<T: LiveTarget>(
    mut target: T,
    link: &UsjLink,
    stop: &AtomicBool,
    jobs: &Mutex<Vec<Job<T>>>,
    pacing: Pacing,
) -> T {
    let qos = crate::platform::machine_thread();
    target.note_qos(qos);
    link.lock().qos = Some(qos);
    let mut cursor = target.api().io().usj_tx.head();
    let to_duration = |t: VTime| Duration::from_nanos(t.0 / 1_000);
    let mut pacer = match pacing {
        Pacing::Wall(rate) => Some(Pacer::new(
            rate,
            Instant::now(),
            to_duration(target.api().now()),
        )),
        Pacing::Max => None,
    };
    while !stop.load(Ordering::SeqCst) && !target.cancelled() {
        let due = std::mem::take(&mut *jobs.lock().unwrap_or_else(|e| e.into_inner()));
        for job in due {
            job(&mut target);
        }
        let m = target.api();
        let journaled = journal_host_actions(m, link);
        if link.attached() == 0 && !journaled {
            // Wall time only while a host tool is connected. With none the guest waits (a
            // disconnect's last line changes still get a slice), and the pacer forgets the pause
            // so the next client does not see the guest sprint.
            std::thread::sleep(IDLE_POLL);
            if let Some(p) = pacer.as_mut() {
                p.reanchor(Instant::now(), to_duration(m.now()));
            }
            continue;
        }
        let before = m.now();
        let outcome = m.run(RunLimits {
            until: Some(VTime(before.0 + SLICE.0)),
            max_insns: None,
            stops: StopSet::default(),
        });
        collect_output(m, link, &mut cursor);
        let now = m.now();
        target.account(outcome.insns);
        {
            let mut inner = link.lock();
            inner.vt = now;
            if outcome.reason != StopReason::Until {
                inner.last_stop = Some(format!("{:?}", outcome.reason));
            }
        }
        if now == before {
            // A deadlock or halt does not move virtual time: wait on the host clock instead of
            // spinning, and forget the lag.
            std::thread::sleep(Duration::from_millis(1));
            if let Some(p) = pacer.as_mut() {
                p.reanchor(Instant::now(), to_duration(now));
            }
            continue;
        }
        if let Some(p) = pacer.as_mut()
            && let Pace::Reanchored { .. } = p.wait(to_duration(now))
        {
            link.lock().reanchors += 1;
        }
    }
    let mut inner = link.lock();
    inner.stopped = true;
    link.changed.notify_all();
    drop(inner);
    target
}

/// The link of an `endpoint --clock agent` instance: set while its endpoints are open, and kept
/// for the instance's life so a second open reuses the wrapper.
#[derive(Debug, Default)]
pub struct AgentSlot {
    link: Mutex<Option<Arc<UsjLink>>>,
    cursor: Mutex<Option<u64>>,
}

impl AgentSlot {
    pub fn connect(&self, link: Arc<UsjLink>) {
        *self.cursor.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.link.lock().unwrap_or_else(|e| e.into_inner()) = Some(link);
    }

    pub fn disconnect(&self) {
        if let Some(link) = self.link.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let mut inner = link.lock();
            inner.stopped = true;
            link.changed.notify_all();
        }
    }
}

impl UsjLink {
    /// A link starting from the line state `m` holds, so a first `SET-CONTROL` repeating it is not
    /// an event.
    pub fn starting_at(m: &mut dyn MachineApi) -> Arc<UsjLink> {
        let link = Arc::new(UsjLink::default());
        let ctrl = m.io().usj_ctrl;
        link.lock().line = LineState {
            dtr: ctrl.dtr(),
            rts: ctrl.rts(),
        };
        link.lock().vt = m.now();
        link
    }
}

/// The machine of an `endpoint --clock agent` instance: every agent `run` first journals what the
/// clients sent, then copies the slice's output to them. Nothing runs between calls, and every
/// other call passes through.
pub struct AgentLinked {
    inner: Box<dyn SnapshotMachine + Send>,
    slot: Arc<AgentSlot>,
}

impl AgentLinked {
    pub fn new(inner: Box<dyn SnapshotMachine + Send>, slot: Arc<AgentSlot>) -> AgentLinked {
        AgentLinked { inner, slot }
    }
}

impl MachineApi for AgentLinked {
    fn run(&mut self, lim: RunLimits) -> RunOutcome {
        let link = self
            .slot
            .link
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(link) = link else {
            return self.inner.run(lim);
        };
        let mut cursor = self.slot.cursor.lock().unwrap_or_else(|e| e.into_inner());
        let m: &mut dyn MachineApi = self.inner.as_mut();
        let from = *cursor.get_or_insert_with(|| m.io().usj_tx.head());
        let mut at = from;
        journal_host_actions(m, link.as_ref());
        let outcome = m.run(lim);
        collect_output(m, link.as_ref(), &mut at);
        *cursor = Some(at);
        link.lock().vt = m.now();
        outcome
    }
    fn input(&mut self, at: At, ev: InputEvent) -> Result<u64, pemu_machine::machine::InputError> {
        self.inner.input(at, ev)
    }
    fn input_from(
        &mut self,
        at: At,
        origin: Origin,
        ev: InputEvent,
    ) -> Result<u64, pemu_machine::machine::InputError> {
        self.inner.input_from(at, origin, ev)
    }
    fn io(&mut self) -> &mut pemu_core::hostio::HostIo {
        self.inner.io()
    }
    fn now(&self) -> VTime {
        self.inner.now()
    }
    fn guest_mem(&mut self) -> pemu_machine::machine::GuestMem<'_> {
        self.inner.guest_mem()
    }
    fn receipt(&mut self) -> pemu_machine::machine::Receipt {
        self.inner.receipt()
    }
    fn is_tainted(&self) -> bool {
        self.inner.is_tainted()
    }

    /// Forwarded so an endpoint-linked session labels its ledger blocks like an unlinked one.
    fn heap_ledger(&self) -> Vec<pemu_machine::hle::HleHeapBlock> {
        self.inner.heap_ledger()
    }

    /// Forwarded so a live bridge fixes this instance's pacing too.
    fn live_bridges(&self) -> u32 {
        self.inner.live_bridges()
    }

    /// Forwarded so an endpoint-linked session reads the same scripted central.
    fn radio_module_state(&self, module: &str) -> Option<&[u8]> {
        self.inner.radio_module_state(module)
    }

    fn app_desc(&self) -> Option<pemu_loader::app_desc::AppDesc> {
        self.inner.app_desc()
    }
}

impl SnapshotMachine for AgentLinked {
    fn snapshot(&self, opts: SnapOpts) -> Result<Snapshot, SnapError> {
        self.inner.snapshot(opts)
    }
    fn redact(
        &self,
        snapshot: &mut Snapshot,
    ) -> Result<pemu_machine::snapshot::Redaction, SnapError> {
        self.inner.redact(snapshot)
    }
    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), SnapError> {
        self.inner.restore(snapshot)
    }
    /// A fork is a new instance with no endpoint, so it gets the bare machine.
    fn fork(&self, live: LivePolicy) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
        self.inner.fork(live)
    }
    fn state_hash(&self) -> [u8; 32] {
        self.inner.state_hash()
    }
    fn secret_generation(&self) -> u64 {
        self.inner.secret_generation()
    }
    fn secret_sources(
        &self,
        view: pemu_machine::snapshot::FlashView,
    ) -> pemu_machine::snapshot::SecretSources {
        self.inner.secret_sources(view)
    }
    fn interrupt_can_wake(&self) -> Option<bool> {
        self.inner.interrupt_can_wake()
    }
    fn watchdog_fired(&self) -> Option<pemu_machine::stops::WatchdogFire> {
        self.inner.watchdog_fired()
    }
}

/// A test double: a guest that echoes every host byte back upper-cased and records every line
/// state it was given.
#[cfg(test)]
pub(crate) mod echo {
    use pemu_core::hostio::{HostIo, SerialStream};
    use pemu_core::input::InputEvent;
    use pemu_core::time::VTime;
    use pemu_machine::machine::{At, GuestMem, InputError, MachineApi, Receipt};
    use pemu_machine::run::{RunLimits, RunOutcome};
    use pemu_machine::stops::StopReason;
    use std::sync::{Arc, Mutex};

    pub struct EchoMachine {
        pub io: HostIo,
        pub now: VTime,
        pub pending: Vec<InputEvent>,
        pub lines: Arc<Mutex<Vec<(bool, bool)>>>,
        /// Bytes of console output written every run, for a client that cannot keep up.
        pub flood: usize,
        pub origins: Arc<Mutex<Vec<pemu_core::journal::Origin>>>,
        /// Wall time every run takes, in milliseconds: a host that cannot keep up.
        pub slow_ms: Arc<std::sync::atomic::AtomicU64>,
        pub noted_qos: Arc<Mutex<Option<&'static str>>>,
    }

    impl EchoMachine {
        pub fn new() -> EchoMachine {
            EchoMachine {
                io: HostIo::new(4096),
                now: VTime(0),
                pending: Vec::new(),
                lines: Arc::default(),
                flood: 0,
                origins: Arc::default(),
                slow_ms: Arc::default(),
                noted_qos: Arc::default(),
            }
        }
    }

    impl MachineApi for EchoMachine {
        fn run(&mut self, lim: RunLimits) -> RunOutcome {
            for ev in std::mem::take(&mut self.pending) {
                match ev {
                    InputEvent::SerialIn { data, .. } => {
                        let upper: Vec<u8> = data.iter().map(u8::to_ascii_uppercase).collect();
                        self.io.serial_write(SerialStream::UsjTx, &upper, self.now);
                    }
                    InputEvent::UsbLine { dtr, rts } => {
                        self.lines.lock().expect("lines").push((dtr, rts));
                    }
                    _ => {}
                }
            }
            if self.flood > 0 {
                let bytes = vec![b'x'; self.flood];
                self.io.serial_write(SerialStream::UsjTx, &bytes, self.now);
            }
            let slow = self.slow_ms.load(std::sync::atomic::Ordering::SeqCst);
            if slow > 0 {
                std::thread::sleep(std::time::Duration::from_millis(slow));
            }
            self.now = lim.until.unwrap_or(self.now);
            RunOutcome {
                reason: StopReason::Until,
                vt: self.now,
                insns: 0,
                ff_insns: 0,
                idle_ps: 0,
            }
        }
        fn input(&mut self, at: At, ev: InputEvent) -> Result<u64, InputError> {
            self.input_from(at, pemu_core::journal::Origin::Agent, ev)
        }
        fn input_from(
            &mut self,
            _at: At,
            origin: pemu_core::journal::Origin,
            ev: InputEvent,
        ) -> Result<u64, InputError> {
            self.origins.lock().expect("origins").push(origin);
            self.pending.push(ev);
            Ok(0)
        }
        fn io(&mut self) -> &mut HostIo {
            &mut self.io
        }
        fn now(&self) -> VTime {
            self.now
        }
        fn guest_mem(&mut self) -> GuestMem<'_> {
            GuestMem::empty()
        }
        fn is_tainted(&self) -> bool {
            false
        }
        fn receipt(&mut self) -> Receipt {
            Receipt::default()
        }
    }

    fn no_snapshot() -> pemu_core::snap::SnapError {
        pemu_core::snap::SnapError::Malformed {
            at: "machine",
            reason: "the echo test guest cannot snapshot",
        }
    }

    impl pemu_machine::SnapshotMachine for EchoMachine {
        fn snapshot(
            &self,
            _opts: pemu_core::snap::SnapOpts,
        ) -> Result<pemu_core::snap::Snapshot, pemu_core::snap::SnapError> {
            Err(no_snapshot())
        }
        fn redact(
            &self,
            _snapshot: &mut pemu_core::snap::Snapshot,
        ) -> Result<pemu_machine::snapshot::Redaction, pemu_core::snap::SnapError> {
            Err(no_snapshot())
        }
        fn restore(
            &mut self,
            _snapshot: &pemu_core::snap::Snapshot,
        ) -> Result<(), pemu_core::snap::SnapError> {
            Err(no_snapshot())
        }
        fn fork(
            &self,
            _live: pemu_core::snap::LivePolicy,
        ) -> Result<Box<dyn pemu_machine::SnapshotMachine + Send>, pemu_core::snap::SnapError>
        {
            Err(no_snapshot())
        }
        fn state_hash(&self) -> [u8; 32] {
            [0; 32]
        }
    }

    impl super::LiveTarget for EchoMachine {
        fn api(&mut self) -> &mut dyn MachineApi {
            self
        }
        fn note_qos(&mut self, class: &'static str) {
            *self.noted_qos.lock().expect("noted") = Some(class);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::echo::EchoMachine;

    #[test]
    fn host_bytes_and_lines_reach_the_guest_in_order_and_output_comes_back() {
        let guest = EchoMachine::new();
        let lines = std::sync::Arc::clone(&guest.lines);
        let origins = std::sync::Arc::clone(&guest.origins);
        let runner = LiveRunner::start(guest, Pacing::Wall(1.0))
            .map_err(|(_, e)| e)
            .expect("spawn");
        let link = runner.link();
        let tap = link.attach();
        link.set_line(LineState {
            dtr: true,
            rts: false,
        });
        assert!(link.send_bytes(b"hello"));
        let mut got = Vec::new();
        let mut buf = [0u8; 16];
        while got.len() < 5 {
            let n = link
                .read_output(&tap, &mut buf, Duration::from_secs(5))
                .expect("running");
            assert!(n > 0, "the echo arrived within the timeout");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"HELLO");
        assert_eq!(*lines.lock().expect("lines"), vec![(true, false)]);
        // An endpoint client's input is journaled as `Endpoint`, making the run `replayable`.
        let seen = origins.lock().expect("origins").clone();
        assert!(!seen.is_empty());
        assert!(
            seen.iter()
                .all(|o| *o == pemu_core::journal::Origin::Endpoint),
            "{seen:?}"
        );
        assert_eq!(
            link.line(),
            LineState {
                dtr: true,
                rts: false
            }
        );
        let vt = runner
            .call(|guest| guest.now)
            .expect("a job runs between slices");
        assert!(vt > VTime(0));
        let back = runner.stop().expect("the thread returned the target");
        assert!(back.now > VTime(0), "virtual time moved while live");
    }

    #[test]
    fn purging_to_the_guest_drops_the_queued_bytes_and_keeps_the_line_change() {
        let link = std::sync::Arc::new(UsjLink::default());
        assert!(link.send_bytes(b"hello"));
        link.set_line(LineState {
            dtr: false,
            rts: true,
        });
        assert!(link.send_bytes(b"world"));
        assert_eq!(link.purge_to_guest(), 10, "both runs of bytes");
        assert_eq!(link.purged_to_guest(), 10);
        assert_eq!(
            link.purged_from_guest(),
            0,
            "the other direction is untouched"
        );
        assert!(
            !link.to_guest_drained(),
            "the queued line change is not data and stays"
        );
        assert_eq!(link.purge_to_guest(), 0, "nothing is left to drop");
    }

    #[test]
    fn purging_one_tap_leaves_every_other_tap_alone() {
        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let link = runner.link();
        let asking = link.attach();
        let other = link.attach();
        assert!(link.send_bytes(b"hello"));
        // The runner fans the echo out at a slice boundary, so a purge can arrive first: drop until
        // the asking tap has given up all five bytes.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut dropped = 0usize;
        while dropped < 5 {
            dropped += link.purge_tap(asking.id());
            assert!(Instant::now() < deadline, "the echo reached the tap");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(dropped, 5, "the five echoed bytes of the asking transport");
        assert_eq!(link.purged_from_guest(), 5);
        assert_eq!(
            link.purged_to_guest(),
            0,
            "the other direction is untouched"
        );
        let mut buf = [0u8; 16];
        let mut kept = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while kept.len() < 5 {
            let n = link
                .read_output(&other, &mut buf, Duration::from_millis(100))
                .expect("running");
            kept.extend_from_slice(&buf[..n]);
            assert!(Instant::now() < deadline, "the other tap kept its bytes");
        }
        assert_eq!(kept, b"HELLO", "the purge was not this transport's");
        let gone = asking.id();
        drop(asking);
        assert_eq!(link.purge_tap(gone), 0);
        runner.stop();
    }

    #[test]
    fn a_stopped_runner_returns_its_target_and_wakes_readers() {
        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let link = runner.link();
        let tap = link.attach();
        // Polled to a deadline: when the runner thread is first scheduled is the host's business,
        // and a loaded Windows host can take more than 20 ms.
        let deadline = Instant::now() + Duration::from_secs(5);
        while link.vt() == VTime(0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(link.vt() > VTime(0), "the mock advanced while live");
        assert!(runner.stop().is_some());
        assert!(link.stopped());
        let mut buf = [0u8; 4];
        assert_eq!(
            link.read_output(&tap, &mut buf, Duration::from_secs(1)),
            None
        );
        assert!(!link.send_bytes(b"x"));
    }

    #[test]
    fn a_live_guest_that_falls_behind_counts_its_reanchors_and_names_its_class() {
        use std::sync::atomic::Ordering;
        let guest = EchoMachine::new();
        let slow = std::sync::Arc::clone(&guest.slow_ms);
        let noted = std::sync::Arc::clone(&guest.noted_qos);
        let runner = LiveRunner::start(guest, Pacing::Wall(1.0))
            .map_err(|(_, e)| e)
            .expect("spawn");
        let link = runner.link();
        // No client: the runner idles and forgets the time, which is not falling behind.
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(link.reanchors(), 0, "an idle stretch is not a re-anchor");
        let tap = link.attach();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            link.vt() > VTime(0),
            "the guest ran while a client was attached"
        );
        assert_eq!(link.reanchors(), 0, "a guest the host keeps up with");

        let lag = u64::try_from(crate::pacing::MAX_LAG.as_millis()).expect("small");
        slow.store(lag + 50, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while link.reanchors() < 2 {
            assert!(std::time::Instant::now() < deadline, "no re-anchor counted");
            std::thread::sleep(Duration::from_millis(20));
        }
        slow.store(0, Ordering::SeqCst);

        let class = link.qos().expect("the live thread read its class");
        assert_eq!(*noted.lock().expect("noted"), Some(class));
        #[cfg(target_os = "macos")]
        assert_eq!(class, "user-interactive", "the live thread asked for it");
        #[cfg(not(target_os = "macos"))]
        assert_ne!(class, "unreadable");
        drop(tap);
        assert!(runner.stop().is_some());
    }
}
