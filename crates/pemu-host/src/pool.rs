//! The instance pool: one OS thread with a one-message mailbox per instance, so calls to one
//! instance serialize with no lock around the machine while instances run in parallel. Every
//! transport shares this pool.
//!
//! Every instance is a registry worker ([`Pool::spawn_worker`]). The machine itself lives in the
//! `pemu-api` session pool, which every command reaches it through; the worker thread is where
//! those commands run, so `start` builds the machine on the 8 MiB stack and every later call lands
//! there too. A mailbox message is a closure, so the pool knows no commands.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pemu_api::error::{ApiError, E_INTERNAL, E_STATE};
use pemu_api::instance::{InstanceId, InstanceState};
use pemu_core::time::VTime;

use crate::artifacts::SharedDir;
use crate::hub::Hub;

/// Stack of an instance thread: the macOS main thread's 8 MiB, which emulator structures were
/// sized against. Set explicitly because spawned threads get 2 MiB on macOS and a Windows `.exe`
/// gets 1 MiB from its PE header.
pub const INSTANCE_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Mailbox capacity. One: a call is a round trip, so a queue would only hide that the instance is
/// busy.
const MAILBOX_DEPTH: usize = 1;

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Clone, Debug)]
pub struct Worker {
    pub hub: Arc<Hub>,
    /// The instance's artifact directory, when the daemon has an artifacts root.
    pub artifacts: Option<SharedDir>,
}

enum Envelope {
    Work(Job),
    /// Leave the loop; the thread then drops the machine.
    Stop,
}

struct Thread {
    mailbox: SyncSender<Envelope>,
    handle: JoinHandle<()>,
}

struct Slot {
    thread: Thread,
    worker: Worker,
}

/// Instances of one daemon. The table mutex is held only while the table is read or written,
/// never while an instance runs, so a long `run` on `p1` does not block a `status` on `p2`.
pub struct Pool {
    inner: Mutex<Inner>,
    max_instances: usize,
    live: AtomicUsize,
    idle_since: Mutex<Option<Instant>>,
}

struct Inner {
    slots: BTreeMap<InstanceId, Slot>,
    /// Workers started but not registered yet, counted against `max_instances`.
    reserved: usize,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("max_instances", &self.max_instances)
            .field("live", &self.live.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Pool {
    pub fn new(max_instances: usize) -> Pool {
        Pool {
            inner: Mutex::new(Inner {
                slots: BTreeMap::new(),
                reserved: 0,
            }),
            max_instances: max_instances.max(1),
            live: AtomicUsize::new(0),
            idle_since: Mutex::new(Some(Instant::now())),
        }
    }

    /// The `--max-instances` default: twice the performance-core count, because the practical
    /// limit is host CPU. Falls back to reported parallelism where [`performance_cores`] has no
    /// answer (Intel Mac, Windows).
    pub fn default_max_instances() -> usize {
        let cores = performance_cores().or_else(|| {
            std::thread::available_parallelism()
                .ok()
                .map(std::num::NonZeroUsize::get)
        });
        cores.map_or(2, |n| n * 2)
    }

    pub fn max_instances(&self) -> usize {
        self.max_instances
    }

    /// Runs `job` on the instance's thread and waits for its value. A caller cancelled after the
    /// job started still lets it finish, so nothing is left half-applied to the machine.
    pub fn run_on<T, F>(&self, id: InstanceId, job: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let mailbox = {
            let inner = self.lock()?;
            inner
                .slots
                .get(&id)
                .ok_or_else(|| no_instance(id))?
                .thread
                .mailbox
                .clone()
        };
        let (tx, rx) = sync_channel::<T>(1);
        let envelope = Envelope::Work(Box::new(move || {
            // The caller may have given up; the job still ran, keeping the machine consistent.
            let _ = tx.send(job());
        }));
        mailbox.send(envelope).map_err(|_| instance_gone(id))?;
        rx.recv().map_err(|_| instance_gone(id))
    }

    /// Starts a registry worker and runs `first` (where `start` runs) on it. `first` returns the
    /// id `pemu-api` minted, or `None` for a refused `start`; the worker is then registered with
    /// `resources` or stopped. Workers still starting count against capacity, so two concurrent
    /// starts cannot both take the last place.
    pub fn spawn_worker<T, F, R>(&self, first: F, resources: R) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce() -> (Option<InstanceId>, T) + Send + 'static,
        R: FnOnce(InstanceId) -> Worker,
    {
        {
            let mut inner = self.lock()?;
            self.check_capacity(&inner)?;
            inner.reserved += 1;
        }
        let release = |pool: &Pool| {
            if let Ok(mut inner) = pool.lock() {
                inner.reserved = inner.reserved.saturating_sub(1);
            }
        };
        let thread = match start_thread() {
            Ok(started) => started,
            Err(e) => {
                release(self);
                return Err(e);
            }
        };
        let (tx, rx) = sync_channel::<(Option<InstanceId>, T)>(1);
        let sent = thread.mailbox.send(Envelope::Work(Box::new(move || {
            let _ = tx.send(first());
        })));
        let answer = sent.ok().and_then(|()| rx.recv().ok());
        let Some((id, value)) = answer else {
            release(self);
            join(thread);
            return Err(ApiError::new(
                E_INTERNAL,
                "the instance thread stopped before `start` answered",
            ));
        };
        let Some(id) = id else {
            release(self);
            join(thread);
            return Ok(value);
        };
        let slot = Slot {
            thread,
            worker: resources(id),
        };
        // A poisoned table must not leak the reservation or leave the thread unjoined: the
        // instance exists in the `pemu-api` pool by now, so it is registered regardless.
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.reserved = inner.reserved.saturating_sub(1);
        if inner.slots.contains_key(&id) {
            drop(inner);
            join(slot.thread);
            return Err(ApiError::new(
                E_INTERNAL,
                format!("instance `{id}` is already a slot of this pool"),
            ));
        }
        inner.slots.insert(id, slot);
        self.live.store(inner.slots.len(), Ordering::SeqCst);
        drop(inner);
        *self.idle_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(value)
    }

    /// Registers a worker for an instance minted without a `start` (a `snapshot fork` copy), which
    /// then counts like any started instance. Refused at capacity or for a duplicate id; the caller
    /// then ends the copy's session ([`end_unhosted`]).
    pub fn adopt_worker(&self, id: InstanceId, worker: Worker) -> Result<(), ApiError> {
        let mut inner = self.lock()?;
        self.check_capacity(&inner)?;
        if inner.slots.contains_key(&id) {
            return Err(ApiError::new(
                E_INTERNAL,
                format!("instance `{id}` is already a slot of this pool"),
            ));
        }
        let thread = start_thread()?;
        inner.slots.insert(id, Slot { thread, worker });
        self.live.store(inner.slots.len(), Ordering::SeqCst);
        drop(inner);
        *self.idle_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }

    pub fn hosts(&self, id: InstanceId) -> bool {
        self.lock()
            .map(|inner| inner.slots.contains_key(&id))
            .unwrap_or(false)
    }

    pub fn worker(&self, id: InstanceId) -> Option<Worker> {
        self.lock()
            .ok()
            .and_then(|inner| inner.slots.get(&id).map(|slot| slot.worker.clone()))
    }

    /// Removes the worker of an instance `pemu-api` already stopped, joins its thread and finishes
    /// its artifact directory. A flush error is returned after the slot is gone, since the
    /// instance has ended either way.
    pub fn retire(&self, id: InstanceId, summary: &serde_json::Value) -> Result<(), ApiError> {
        let slot = {
            let mut inner = self.lock()?;
            let slot = inner.slots.remove(&id).ok_or_else(|| no_instance(id))?;
            self.live.store(inner.slots.len(), Ordering::SeqCst);
            slot
        };
        let finished = finish_worker(slot, summary);
        self.mark_idle_if_empty();
        finished
    }

    fn check_capacity(&self, inner: &Inner) -> Result<(), ApiError> {
        if inner.slots.len() + inner.reserved >= self.max_instances {
            return Err(ApiError::new(
                E_STATE,
                format!(
                    "the daemon is at its limit of {} instances",
                    self.max_instances
                ),
            )
            .retryable()
            .with_hint("`stop` an instance, or start the daemon with a larger `--max-instances`"));
        }
        Ok(())
    }

    /// The lifecycle state of one hosted instance, or `E_STATE` when the id names none.
    pub fn state(&self, id: InstanceId) -> Result<InstanceState, ApiError> {
        if !self.hosts(id) {
            return Err(no_instance(id));
        }
        pemu_api::commands::start::with_pool(|pool| pool.table().get(id).cloned())
            .ok_or_else(|| no_instance(id))
    }

    pub fn live(&self) -> Vec<InstanceId> {
        match self.lock() {
            Ok(inner) => inner.slots.keys().copied().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Stops one instance: its session ends on its own thread, the thread is joined, then its
    /// artifact directory is finished. The join comes first because a thread still inside a
    /// command could be writing to the directory. Every shutdown path comes through here.
    pub fn stop(&self, id: InstanceId, _now: VTime) -> Result<(), ApiError> {
        if !self.hosts(id) {
            return Err(no_instance(id));
        }
        // Cancel first: the end is queued behind the running call, and a cancelled run returns at
        // its next slice instead of at the end of its budget.
        pemu_api::commands::start::cancel(id);
        let summary = self
            .run_on(id, move || end_session(id, "shutdown"))
            .unwrap_or_else(|e| serde_json::json!({ "instance": id.to_string(), "reason": "shutdown", "error": e.to_json() }));
        self.retire(id, &summary)
    }

    fn mark_idle_if_empty(&self) {
        if self.live.load(Ordering::SeqCst) == 0 {
            *self.idle_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
    }

    /// Cancels the running and every later call on every worker's session, the first step of every
    /// shutdown path, so nothing waits on a long `run`.
    pub fn cancel_all(&self) {
        for id in self.live() {
            pemu_api::commands::start::cancel(id);
        }
    }

    /// Stops every instance and joins every thread, the last step of every shutdown path.
    pub fn shutdown(&self, now: VTime) {
        let ids = match self.lock() {
            Ok(inner) => inner.slots.keys().copied().collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        for id in ids {
            if self.stop(id, now).is_err() {
                // A refused lifecycle move (already `Stopped`) must not leave a thread running
                // past the daemon, so the slot is taken out and joined anyway.
                let orphan = self.lock().ok().and_then(|mut inner| {
                    let slot = inner.slots.remove(&id);
                    self.live.store(inner.slots.len(), Ordering::SeqCst);
                    slot
                });
                if let Some(slot) = orphan {
                    join(slot.thread);
                }
            }
        }
        self.mark_idle_if_empty();
    }

    /// How long the pool has held no instance, or `None` while one is live (the idle exit's input).
    pub fn idle_for(&self) -> Option<Duration> {
        self.idle_since
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|since| since.elapsed())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, ApiError> {
        self.inner.lock().map_err(|_| {
            ApiError::new(
                E_INTERNAL,
                "the instance table is poisoned: an instance thread panicked",
            )
        })
    }
}

impl Drop for Pool {
    /// Joins every instance thread, so a dropped pool leaves no thread writing artifacts.
    fn drop(&mut self) {
        let slots = match self.inner.get_mut() {
            Ok(inner) => std::mem::take(&mut inner.slots),
            Err(poisoned) => std::mem::take(&mut poisoned.into_inner().slots),
        };
        for (id, slot) in slots {
            // The session lives in the `pemu-api` pool, which outlives this one: cancel, join,
            // then end the session so no machine is left behind, and flush the directory.
            pemu_api::commands::start::cancel(id);
            join(slot.thread);
            let summary = end_session(id, "drop");
            let _ = finish_dir(slot.worker, &summary);
        }
    }
}

fn start_thread() -> Result<Thread, ApiError> {
    let (mailbox, rx) = sync_channel(MAILBOX_DEPTH);
    let handle = std::thread::Builder::new()
        .name("pemu-instance".to_string())
        .stack_size(INSTANCE_STACK_BYTES)
        .spawn(move || instance_loop(rx))
        .map_err(|e| {
            ApiError::new(
                E_INTERNAL,
                format!("the instance thread did not start: {e}"),
            )
        })?;
    Ok(Thread { mailbox, handle })
}

/// The instance thread: request the machine-thread class, then serve the mailbox until it closes.
/// The class is requested, not inherited, because a spawned macOS thread reads `default`, which
/// the scheduler moves to the efficiency cores first when the performance cluster is busy.
fn instance_loop(rx: Receiver<Envelope>) {
    crate::platform::machine_thread();
    while let Ok(envelope) = rx.recv() {
        match envelope {
            Envelope::Work(job) => job(),
            Envelope::Stop => break,
        }
    }
}

/// Ends an instance's `pemu-api` session if still live and returns the summary its artifact
/// directory keeps. Also used for an instance this pool could not host (a fork refused at
/// capacity).
pub fn end_unhosted(id: InstanceId) -> serde_json::Value {
    pemu_api::commands::start::cancel(id);
    end_session(id, "not hosted")
}

fn end_session(id: InstanceId, reason: &str) -> serde_json::Value {
    pemu_api::commands::start::with_pool(|pool| match pool.destroy(id) {
        Ok(mut session) => {
            let receipt = session.receipt();
            serde_json::json!({
                "instance": id.to_string(),
                "reason": reason,
                "fw": session.fw,
                "final_vt_us": receipt.vt_us,
                "insns": receipt.insns,
            })
        }
        Err(_) => serde_json::json!({ "instance": id.to_string(), "reason": reason }),
    })
}

fn finish_worker(slot: Slot, summary: &serde_json::Value) -> Result<(), ApiError> {
    join(slot.thread);
    finish_dir(slot.worker, summary)
}

fn finish_dir(worker: Worker, summary: &serde_json::Value) -> Result<(), ApiError> {
    match worker.artifacts {
        Some(dir) => dir
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish(summary)
            .map_err(|e| {
                ApiError::new(
                    E_INTERNAL,
                    format!(
                        "the artifact directory was not flushed: {}",
                        flush_reason(&e)
                    ),
                )
            }),
        None => Ok(()),
    }
}

/// An artifact error without the native path an I/O error's text carries.
fn flush_reason(error: &crate::artifacts::Error) -> String {
    match error {
        crate::artifacts::Error::Io(_, e) => e.kind().to_string(),
        other => other.to_string(),
    }
}

fn join(thread: Thread) {
    // Dropping the sender would also end the loop; `Stop` makes the intent readable and lets a
    // queued job run first.
    let _ = thread.mailbox.send(Envelope::Stop);
    let _ = thread.handle.join();
}

/// The performance-core count from `sysctl -n hw.perflevel0.logicalcpu` (Apple Silicon), or
/// `None`. Run by absolute path, never through a shell.
pub fn performance_cores() -> Option<usize> {
    let out = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.perflevel0.logicalcpu"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_core_count(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_core_count(text: &str) -> Option<usize> {
    text.trim().parse::<usize>().ok().filter(|n| *n > 0)
}

pub(crate) fn no_instance(id: InstanceId) -> ApiError {
    ApiError::new(E_STATE, format!("no instance `{id}`"))
        // The same hint as `InstanceId::parse` and `status`, so forwarded and in-process calls
        // print the same bytes.
        .with_hint("`status` lists the live instances")
}

fn instance_gone(id: InstanceId) -> ApiError {
    ApiError::new(
        E_INTERNAL,
        format!("instance `{id}` stopped while the call was in flight"),
    )
}

#[cfg(test)]
impl Pool {
    /// Hosts a worker under `id` with no `pemu-api` session behind it, for a test. Use an id the
    /// `pemu-api` pool never mints in a test run, since `stop` ends that id's session.
    pub(crate) fn host_for_test(&self, id: &str) -> InstanceId {
        let id = InstanceId::parse(id).expect("an instance id");
        self.spawn_worker(
            move || (Some(id), ()),
            |id| Worker {
                hub: Hub::new(id),
                artifacts: None,
            },
        )
        .expect("a free place in the pool");
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::channel;

    fn pool() -> Pool {
        Pool::new(4)
    }

    const PROBE_FRAME_BYTES: usize = 256 * 1024;
    /// Frames [`burn_stack`] recurses through: 3 MiB, over a spawned macOS thread's 2 MiB and a
    /// Windows `.exe`'s 1 MiB, far under 8 MiB.
    const PROBE_FRAMES: usize = 12;

    const _PROBE_IS_BIGGER_THAN_A_DEFAULT_STACK: () =
        assert!(PROBE_FRAME_BYTES * PROBE_FRAMES > 2 * 1024 * 1024);
    const _PROBE_FITS_THE_INSTANCE_STACK: () =
        assert!(PROBE_FRAME_BYTES * PROBE_FRAMES < INSTANCE_STACK_BYTES / 2);

    /// Uses `PROBE_FRAME_BYTES * (depth + 1)` bytes of stack, touching both ends of every frame.
    /// A heap `Vec` would prove nothing; recursion over a fixed array overflows if
    /// `stack_size(INSTANCE_STACK_BYTES)` is removed, and `black_box` keeps it in release builds.
    #[inline(never)]
    fn burn_stack(depth: usize) -> usize {
        let mut frame = [0u8; PROBE_FRAME_BYTES];
        frame[0] = 1;
        frame[PROBE_FRAME_BYTES - 1] = 1;
        let frame = std::hint::black_box(&frame);
        let here = usize::from(frame[0]) + usize::from(frame[PROBE_FRAME_BYTES - 1]);
        match depth {
            0 => here,
            _ => here + burn_stack(depth - 1),
        }
    }

    #[test]
    fn an_instance_thread_is_created_with_an_explicit_8_mib_stack() {
        assert_eq!(
            INSTANCE_STACK_BYTES,
            8 * 1024 * 1024,
            "an instance thread gets an 8 MiB stack"
        );
        let pool = pool();
        let id = pool.host_for_test("p901");
        // A `JoinHandle` cannot report its stack size, so the job proves it by using more than a
        // default thread has: without `.stack_size(..)` the process aborts.
        let used = pool
            .run_on(id, || burn_stack(PROBE_FRAMES - 1))
            .expect("the job ran on a thread with room for it");
        assert_eq!(used, 2 * PROBE_FRAMES);
        pool.stop(id, VTime::default()).expect("stop");
    }

    #[test]
    fn an_instance_thread_runs_at_the_class_it_asked_for() {
        let pool = pool();
        let id = pool.host_for_test("p901");
        let class = pool
            .run_on(id, crate::platform::thread_qos_name)
            .expect("run_on");
        #[cfg(target_os = "macos")]
        assert_eq!(class, "user-interactive");
        #[cfg(not(target_os = "macos"))]
        assert_ne!(class, "unreadable");
        pool.stop(id, VTime::default()).expect("stop");
    }

    #[test]
    fn the_job_runs_on_the_instance_thread_not_the_caller() {
        let pool = pool();
        let id = pool.host_for_test("p901");
        let caller = std::thread::current().id();
        let (thread_id, name) = pool
            .run_on(id, move || {
                (
                    std::thread::current().id(),
                    std::thread::current().name().map(str::to_string),
                )
            })
            .expect("run_on");
        assert_ne!(thread_id, caller);
        assert_eq!(
            name.as_deref(),
            Some("pemu-instance"),
            "a stack trace says the frame is on an instance thread"
        );
        pool.stop(id, VTime::default()).expect("stop");
    }

    #[test]
    fn calls_to_one_instance_serialize_on_one_thread() {
        let pool = Arc::new(pool());
        let id = pool.host_for_test("p901");
        let inside = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();

        let mut handles = Vec::new();
        for _ in 0..2 {
            let pool = Arc::clone(&pool);
            let inside = Arc::clone(&inside);
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..8 {
                    let inside = Arc::clone(&inside);
                    let (overlapped, thread) = pool
                        .run_on(id, move || {
                            let overlapped = inside.swap(true, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(1));
                            inside.store(false, Ordering::SeqCst);
                            (overlapped, std::thread::current().id())
                        })
                        .expect("run_on");
                    tx.send((overlapped, thread)).expect("collect");
                }
            }));
        }
        drop(tx);
        for h in handles {
            h.join().expect("caller thread");
        }
        let seen: Vec<_> = rx.iter().collect();
        assert_eq!(seen.len(), 16);
        assert!(
            seen.iter().all(|(overlapped, _)| !overlapped),
            "a job started while another job of the same instance was still inside"
        );
        assert!(
            seen.iter().all(|(_, thread)| *thread == seen[0].1),
            "every call ran on the one instance thread"
        );
        pool.stop(id, VTime::default()).expect("stop");
    }

    #[test]
    fn instances_run_in_parallel_while_one_of_them_is_busy() {
        let pool = Arc::new(pool());
        let busy = pool.host_for_test("p901");
        let other = pool.host_for_test("p902");

        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel::<()>();
        let busy_pool = Arc::clone(&pool);
        let blocker = std::thread::spawn(move || {
            busy_pool
                .run_on(busy, move || {
                    started_tx.send(()).expect("announce");
                    release_rx.recv().expect("release");
                })
                .expect("run_on")
        });
        started_rx.recv().expect("the busy instance started");

        // `other` answers while `busy` is still inside its job.
        let answer = pool.run_on(other, || 7).expect("run_on other");
        assert_eq!(answer, 7);

        release_tx.send(()).expect("release");
        blocker.join().expect("blocked caller");
        pool.stop(busy, VTime::default()).expect("stop");
        pool.stop(other, VTime::default()).expect("stop");
    }

    #[test]
    fn the_pool_refuses_past_its_capacity_and_says_so_retryably() {
        let pool = Pool::new(2);
        let a = pool.host_for_test("p901");
        let b = pool.host_for_test("p902");
        let refused = pool
            .spawn_worker(|| (None, ()), |id| worker_for(None, id))
            .expect_err("full");
        assert_eq!(refused.code.name, "E_STATE");
        assert!(
            refused.retryable,
            "capacity frees up when an instance stops"
        );
        assert!(refused.message.contains('2'));
        pool.stop(a, VTime::default()).expect("stop");
        let c = pool.host_for_test("p903");
        pool.stop(b, VTime::default()).expect("stop");
        pool.stop(c, VTime::default()).expect("stop");
    }

    #[test]
    fn a_stopped_instance_leaves_the_pool_and_refuses_further_calls() {
        let pool = pool();
        let id = pool.host_for_test("p901");
        assert_eq!(pool.live(), [id]);
        pool.stop(id, VTime::default()).expect("stop");
        assert!(pool.live().is_empty());
        assert!(!pool.hosts(id));
        let err = pool.run_on(id, || 7).expect_err("no thread any more");
        assert_eq!(err.code.name, "E_STATE");
        assert_eq!(pool.state(id).expect_err("not hosted").code, E_STATE);
        assert!(pool.stop(id, VTime::default()).is_err());
    }

    #[test]
    fn the_idle_clock_starts_only_when_the_last_instance_goes() {
        let pool = pool();
        assert!(
            pool.idle_for().is_some(),
            "a daemon with no instance is idle from the start"
        );
        let a = pool.host_for_test("p901");
        let b = pool.host_for_test("p902");
        assert_eq!(pool.idle_for(), None);
        pool.stop(a, VTime::default()).expect("stop");
        assert_eq!(pool.idle_for(), None, "one instance is still live");
        pool.stop(b, VTime::default()).expect("stop");
        assert!(pool.idle_for().is_some());
    }

    #[test]
    fn shutdown_joins_every_instance_thread() {
        let pool = pool();
        for id in ["p901", "p902", "p903"] {
            pool.host_for_test(id);
        }
        pool.shutdown(VTime::default());
        assert!(pool.live().is_empty());
        assert_eq!(pool.live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_sysctl_core_count_parses_and_anything_else_is_none() {
        assert_eq!(parse_core_count("6\n"), Some(6));
        assert_eq!(parse_core_count("  10 "), Some(10));
        assert_eq!(parse_core_count("0\n"), None, "zero cores is no answer");
        assert_eq!(parse_core_count(""), None);
        assert_eq!(parse_core_count("unknown oid"), None);
        assert_eq!(parse_core_count("-4"), None);
    }

    #[test]
    fn the_default_capacity_is_twice_the_performance_cores() {
        let cores = performance_cores().unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1)
        });
        assert_eq!(Pool::default_max_instances(), cores * 2);
        assert!(Pool::default_max_instances() >= 2);
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-pool-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        dir
    }

    fn worker_for(root: Option<&std::path::Path>, id: InstanceId) -> Worker {
        Worker {
            hub: Hub::new(id),
            artifacts: root.map(|root| {
                Arc::new(std::sync::Mutex::new(
                    crate::artifacts::ArtifactDir::create(root, "run-1", &id.to_string())
                        .expect("create"),
                ))
            }),
        }
    }

    #[test]
    fn a_registry_worker_runs_start_and_every_later_call_on_one_8_mib_thread() {
        let pool = pool();
        let id = InstanceId::parse("p7").expect("an id");
        let started_on = pool
            .spawn_worker(
                move || (Some(id), std::thread::current().id()),
                |id| worker_for(None, id),
            )
            .expect("start");
        assert_eq!(pool.live(), [id]);
        assert!(pool.worker(id).is_some());
        let (later_on, used) = pool
            .run_on(id, || {
                (std::thread::current().id(), burn_stack(PROBE_FRAMES - 1))
            })
            .expect("run_on");
        assert_eq!(started_on, later_on, "one thread per instance");
        assert_eq!(used, 2 * PROBE_FRAMES);
        pool.shutdown(VTime::default());
        assert!(pool.live().is_empty());
    }

    #[test]
    fn an_adopted_fork_counts_in_live_and_capacity_and_retires_like_a_started_instance() {
        let pool = Pool::new(2);
        let (a, b, c) = (
            InstanceId::parse("p1").expect("id"),
            InstanceId::parse("p2").expect("id"),
            InstanceId::parse("p3").expect("id"),
        );
        pool.spawn_worker(move || (Some(a), ()), |id| worker_for(None, id))
            .expect("start");
        pool.adopt_worker(b, worker_for(None, b)).expect("the fork");
        assert_eq!(pool.live(), [a, b]);
        assert!(pool.hosts(b) && pool.worker(b).is_some());
        assert!(pool.idle_for().is_none());
        let full = pool
            .adopt_worker(c, worker_for(None, c))
            .expect_err("at the limit");
        assert_eq!(full.code, E_STATE);
        assert!(!pool.hosts(c));
        let again = pool
            .adopt_worker(b, worker_for(None, b))
            .expect_err("already a slot");
        assert_eq!(
            again.code, E_STATE,
            "capacity is checked first at the limit"
        );
        let on = pool
            .run_on(b, || std::thread::current().name().map(str::to_string))
            .expect("the fork has its own thread");
        assert_eq!(on.as_deref(), Some("pemu-instance"));
        pool.retire(
            b,
            &serde_json::json!({ "instance": "p2", "reason": "stop" }),
        )
        .expect("retire");
        pool.shutdown(VTime::default());
        assert!(pool.live().is_empty());
    }

    #[test]
    fn a_refused_start_leaves_no_slot_and_releases_its_reservation() {
        let pool = Pool::new(1);
        for _ in 0..3 {
            let value = pool
                .spawn_worker(|| (None, "refused"), |id| worker_for(None, id))
                .expect("the start itself answered");
            assert_eq!(value, "refused");
        }
        assert!(pool.live().is_empty());
        let id = InstanceId::parse("p1").expect("an id");
        pool.spawn_worker(move || (Some(id), ()), |id| worker_for(None, id))
            .expect("the one place is still free");
        let full = pool
            .spawn_worker(|| (None, ()), |id| worker_for(None, id))
            .expect_err("at the limit");
        assert_eq!(full.code, E_STATE);
        pool.shutdown(VTime::default());
    }

    #[test]
    fn retire_and_shutdown_join_the_worker_and_finish_its_artifacts() {
        let root = temp_dir("retire");
        let pool = pool();
        let (a, b) = (
            InstanceId::parse("p1").expect("id"),
            InstanceId::parse("p2").expect("id"),
        );
        for id in [a, b] {
            let root = root.clone();
            pool.spawn_worker(
                move || (Some(id), ()),
                move |id| worker_for(Some(&root), id),
            )
            .expect("start");
        }
        let dir = pool
            .worker(a)
            .and_then(|w| w.artifacts)
            .expect("a directory");
        pool.run_on(a, move || {
            dir.lock()
                .expect("lock")
                .append(crate::artifacts::SERIAL_FILE, b"boot\n")
                .expect("append");
        })
        .expect("run_on");

        pool.retire(
            a,
            &serde_json::json!({ "instance": "p1", "reason": "stop" }),
        )
        .expect("retire");
        let summary = std::fs::read_to_string(root.join("run-1/p1/summary.json")).expect("p1");
        assert!(summary.contains("\"stop\""), "{summary}");
        assert_eq!(
            std::fs::read(root.join("run-1/p1/serial.log")).expect("serial.log"),
            b"boot\n"
        );
        assert_eq!(
            pool.retire(a, &serde_json::json!({}))
                .expect_err("gone")
                .code,
            E_STATE
        );

        pool.shutdown(VTime::default());
        let summary = std::fs::read_to_string(root.join("run-1/p2/summary.json")).expect("p2");
        assert!(summary.contains("\"shutdown\""), "{summary}");
        assert!(pool.live().is_empty());
        assert!(pool.idle_for().is_some(), "the idle clock runs again");
    }
}
