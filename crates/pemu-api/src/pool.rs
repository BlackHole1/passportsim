//! The session pool every core command works on, and the host hooks it carries.
//!
//! `HandlerCx` is empty, so a handler reaches its instance only through its arguments: [`shared`]
//! is the one pool per process (CLI, MCP, HTTP and the web UI), and [`Pool`] is the same as a value
//! so a test, a second daemon or a wasm page can hold its own. `pemu-api` reads no environment or
//! file system; the host installs the machine factory, the home-redacted artifact root and its
//! other hooks.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use pemu_core::hostio::SerialStream;
use pemu_core::time::VTime;
use pemu_machine::SnapshotMachine;

use crate::commands::start::StartArgs;
use crate::error::{ApiError, E_INTERNAL, E_STATE};
use crate::instance::{InstanceId, InstanceKind, InstanceTable, Lifecycle};
use crate::session::{ClockMode, Session, Speed};
use crate::shape::Cursor;
use crate::spec::Annotations;

use crate::commands::snapshot::{Store, StoreHandle};

pub type BackendFactory = fn(&StartArgs) -> Result<Box<dyn SnapshotMachine + Send>, ApiError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedBoot {
    pub hit: bool,
    /// 64 hex characters; carries no host path.
    pub key: String,
    /// `disk`, `memory` (a tainted machine), or `none` when a cold boot did not settle.
    pub store: &'static str,
    /// The first LVGL safe point after `app_main` returned.
    pub settled: bool,
}

/// Restores the session from a cache entry, or boots it cold to the settled point and stores one.
/// Installed by the host, since `pemu-api` holds no file.
pub type BootCacheHook = fn(&StartArgs, &mut Session) -> Result<CachedBoot, ApiError>;

/// Runs a fresh session to the first LVGL safe point after `app_main` returned, within `budget`.
/// `Ok(None)` when the host has no app ELF for the firmware, so `start` falls back to waiting for a
/// `ui-settled` event. The boot cache settles through the same function, so both land at the same
/// instant.
pub type UiSettleHook = fn(&mut Session, VTime) -> Result<Option<bool>, ApiError>;

/// A monotonic host clock in milliseconds for `wall_budget_ms`, which a core crate may not read.
/// With none, a `run` is bounded by its virtual timeout alone. Never part of run identity.
pub type HostClock = fn() -> u64;

/// Reads the calling thread's scheduling class, so the receipt's `host_qos` is read, not assumed.
/// It does not say where the thread ran: under `taskpolicy -b` it reads `user-interactive` on
/// efficiency cores.
pub type ThreadQosReader = fn() -> &'static str;

/// The default factory builds no machine; a host installs its own with [`Pool::with_factory`].
fn no_machine(_args: &StartArgs) -> Result<Box<dyn SnapshotMachine + Send>, ApiError> {
    Err(
        ApiError::new(E_INTERNAL, "this build has no emulator core to start").with_hint(
            "this pool has no machine factory; a host installs one with `Pool::with_factory`",
        ),
    )
}

/// Per-instance tables kept beside one [`Pool`], one per type. Per pool because ids are unique only
/// within the pool that minted them. Each table has its own mutex, taken only for `f`, so `f` may
/// reach another table. Values owning a thread or socket close them in `Drop`.
#[derive(Clone, Default)]
pub struct PoolTables(Arc<Mutex<BTreeMap<std::any::TypeId, PoolTable>>>);

type PoolTable = Arc<dyn std::any::Any + Send + Sync>;

impl PoolTables {
    /// Creates the table on first use and recovers a poisoned lock, so one panicking command does
    /// not take later calls down.
    pub fn with<T, R>(&self, f: impl FnOnce(&mut T) -> R) -> R
    where
        T: Default + Send + 'static,
    {
        let table = {
            let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(
                map.entry(std::any::TypeId::of::<T>())
                    .or_insert_with(|| Arc::new(Mutex::new(T::default()))),
            )
        };
        let table = table
            .downcast::<Mutex<T>>()
            .unwrap_or_else(|_| unreachable!("a table is stored under its own TypeId"));
        let mut guard = table.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }
}

/// How a host thread (such as the HCI transport) reaches the pool that opened it, without waiting
/// for its lock: the holder may be waiting for that thread (`env --ble-bridge detach` joins it). A
/// plain [`Pool::new`] has no reach, and the transport refuses.
#[derive(Clone, Debug)]
pub struct PoolReach(pub(crate) Reach);

#[derive(Clone, Debug)]
pub(crate) enum Reach {
    Process,
    Shared(std::sync::Weak<Mutex<Pool>>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reached<R> {
    Ran(R),
    Busy,
    Gone,
}

impl PoolReach {
    pub fn try_with<R>(&self, f: impl FnOnce(&mut Pool) -> R) -> Reached<R> {
        fn locked<R>(pool: &Mutex<Pool>, f: impl FnOnce(&mut Pool) -> R) -> Reached<R> {
            match pool.try_lock() {
                Ok(mut guard) => Reached::Ran(f(&mut guard)),
                Err(std::sync::TryLockError::Poisoned(e)) => Reached::Ran(f(&mut e.into_inner())),
                Err(std::sync::TryLockError::WouldBlock) => Reached::Busy,
            }
        }
        match &self.0 {
            Reach::Process => locked(shared(), f),
            Reach::Shared(weak) => match weak.upgrade() {
                Some(pool) => locked(&pool, f),
                None => Reached::Gone,
            },
        }
    }
}

#[derive(Default)]
pub(crate) struct ReachSlot(pub(crate) Option<PoolReach>);

pub struct Pool {
    table: InstanceTable,
    sessions: BTreeMap<InstanceId, Session>,
    /// Sessions checked out by [`with_session`] and running outside the pool lock.
    busy: BTreeSet<InstanceId>,
    cancels: BTreeMap<InstanceId, Arc<AtomicBool>>,
    pub(crate) factory: BackendFactory,
    host_clock: Option<HostClock>,
    thread_qos: Option<ThreadQosReader>,
    artifacts_root: Option<String>,
    endpoint_host: Option<Arc<dyn crate::commands::endpoint::EndpointHost>>,
    pub(crate) boot_cache: Option<BootCacheHook>,
    pub(crate) ui_settle: Option<UiSettleHook>,
    /// Per pool, not per process: two pools both hand out `p1`, and a shared table would let one
    /// pool's taint answer for the other's instance.
    store: StoreHandle,
    tables: PoolTables,
}

impl Default for Pool {
    fn default() -> Pool {
        Pool::new()
    }
}

impl Pool {
    pub fn new() -> Pool {
        Pool {
            table: InstanceTable::new(),
            sessions: BTreeMap::new(),
            busy: BTreeSet::new(),
            cancels: BTreeMap::new(),
            factory: no_machine,
            host_clock: None,
            thread_qos: None,
            artifacts_root: None,
            endpoint_host: None,
            boot_cache: None,
            ui_settle: None,
            store: StoreHandle::default(),
            tables: PoolTables::default(),
        }
    }

    pub fn with_factory(factory: BackendFactory) -> Pool {
        Pool {
            factory,
            ..Pool::new()
        }
    }

    pub fn set_factory(&mut self, factory: BackendFactory) -> BackendFactory {
        std::mem::replace(&mut self.factory, factory)
    }

    pub fn set_host_clock(&mut self, clock: Option<HostClock>) {
        self.host_clock = clock;
    }

    pub fn host_clock(&self) -> Option<HostClock> {
        self.host_clock
    }

    pub fn set_thread_qos(&mut self, reader: Option<ThreadQosReader>) {
        self.thread_qos = reader;
    }

    /// The root is something `status` reports, never an argument, so no agent can set it.
    pub fn set_artifacts_root(&mut self, root: Option<String>) {
        self.artifacts_root = root;
    }

    /// Per pool, so `stop` and `status` never reach endpoints another pool opened for the same id.
    pub fn set_endpoint_host(
        &mut self,
        host: Option<Arc<dyn crate::commands::endpoint::EndpointHost>>,
    ) {
        self.endpoint_host = host;
    }

    pub fn endpoint_host(&self) -> Option<Arc<dyn crate::commands::endpoint::EndpointHost>> {
        self.endpoint_host.clone()
    }

    pub fn set_boot_cache(&mut self, hook: Option<BootCacheHook>) {
        self.boot_cache = hook;
    }

    pub fn set_ui_settle(&mut self, hook: Option<UiSettleHook>) {
        self.ui_settle = hook;
    }

    pub fn ui_settle(&self) -> Option<UiSettleHook> {
        self.ui_settle
    }

    pub fn boot_cache(&self) -> Option<BootCacheHook> {
        self.boot_cache
    }

    pub fn artifacts_root(&self) -> Option<&str> {
        self.artifacts_root.as_deref()
    }

    pub fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        self.store.with(f)
    }

    pub fn with_table<T, R>(&self, f: impl FnOnce(&mut T) -> R) -> R
    where
        T: Default + Send + 'static,
    {
        self.tables.with(f)
    }

    pub fn tables(&self) -> PoolTables {
        self.tables.clone()
    }

    pub fn into_shared(self) -> Arc<Mutex<Pool>> {
        Arc::new_cyclic(|weak| {
            self.set_reach(PoolReach(Reach::Shared(weak.clone())));
            Mutex::new(self)
        })
    }

    fn set_reach(&self, reach: PoolReach) {
        self.tables
            .with(|slot: &mut ReachSlot| slot.0 = Some(reach));
    }

    pub fn reach(&self) -> Option<PoolReach> {
        self.tables.with(|slot: &mut ReachSlot| slot.0.clone())
    }

    pub fn table(&self) -> &InstanceTable {
        &self.table
    }

    pub fn table_mut(&mut self) -> &mut InstanceTable {
        &mut self.table
    }

    pub fn live_ids(&self) -> Vec<InstanceId> {
        self.table.live_ids()
    }

    pub fn session(&self, id: InstanceId) -> Option<&Session> {
        self.sessions.get(&id)
    }

    pub fn session_mut(&mut self, id: InstanceId) -> Option<&mut Session> {
        self.sessions.get_mut(&id)
    }

    pub fn bind(
        &self,
        annotations: Annotations,
        requested: Option<&str>,
    ) -> Result<InstanceId, ApiError> {
        let id = self.table.bind(annotations, requested)?.ok_or_else(|| {
            ApiError::new(
                E_INTERNAL,
                "this command has no `needs_instance` annotation, so it cannot bind an instance",
            )
        })?;
        // A checked-out instance is refused retryably, never answered as if the session were gone.
        if self.busy.contains(&id) {
            return Err(self.busy_error(id));
        }
        Ok(id)
    }

    /// An open endpoint holding the lease is `E_LEASE` naming it; a call in flight is [`busy`].
    fn busy_error(&self, id: InstanceId) -> ApiError {
        let holder = self
            .table
            .get(id)
            .and_then(|state| state.lease.holder(state.since));
        if holder == Some(crate::lease::LeaseHolder::Endpoint) {
            return ApiError::new(
                crate::error::E_LEASE,
                format!(
                    "the clock lease of `{id}` is held by `endpoint`: a host tool endpoint runs it live"
                ),
            )
            .retryable()
            .with_hint("`endpoint --close` hands the instance back to agent calls");
        }
        busy(id)
    }

    pub fn create(&mut self, args: &StartArgs) -> Result<InstanceId, ApiError> {
        let backend = (self.factory)(args)?;
        Ok(self.attach(args, backend))
    }

    /// Registers an instance around a machine the caller already built. A browser-hosted instance
    /// uses [`Pool::attach_browser`] instead.
    pub fn attach(
        &mut self,
        args: &StartArgs,
        backend: Box<dyn SnapshotMachine + Send>,
    ) -> InstanceId {
        let id = self.table.create(InstanceKind::Process, VTime(0));
        // The `SecretSet` is built from the loaded image and eFuse before anything runs, so later
        // guest writes never taint it.
        self.store
            .with(|store| store.load_secret_set(id, backend.as_ref()));
        let cancel = Arc::new(AtomicBool::new(false));
        self.sessions.insert(
            id,
            Session {
                id,
                label: args.label.clone(),
                fw: fw_display(&args.fw),
                seed: args.seed,
                mode: args.mode,
                speed: Speed::Milli(1000),
                idle_skip: matches!(args.mode, ClockMode::Deterministic),
                deterministic_so_far: matches!(args.mode, ClockMode::Deterministic),
                ticket: None,
                insns: 0,
                host_clock: self.host_clock,
                thread_qos: self.thread_qos,
                host_qos: None,
                cursors: [Cursor(0); SerialStream::ALL.len()],
                event_cursor: 0,
                usj_journaled: (VTime(0), 0),
                mic: crate::commands::mic_set::MicState::default(),
                waiting_for_input: false,
                task_deadlock: None,
                tripwires_hit: Vec::new(),
                redacted: false,
                store: self.store.clone(),
                tables: self.tables.clone(),
                cancel: Arc::clone(&cancel),
                backend,
            },
        );
        self.cancels.insert(id, cancel);
        id
    }

    /// Registers a browser-hosted instance and returns its `b<n>` id. The machine runs as wasm in
    /// the page, so the pool holds only the table entry; the daemon's relay proxies calls into the
    /// page. The entry is `running` from the start because the page paces its own machine.
    pub fn attach_browser(&mut self) -> InstanceId {
        let id = self.table.create(InstanceKind::Browser, VTime(0));
        if let Some(state) = self.table.get_mut(id) {
            let _ = state.transition(Lifecycle::Paused, VTime(0));
            let _ = state.transition(Lifecycle::Running, VTime(0));
        }
        id
    }

    /// The entry stays, so a later call is told the id is stopped, and the index is never reused.
    pub fn detach_browser(&mut self, id: InstanceId) -> Result<(), ApiError> {
        if id.kind() != InstanceKind::Browser {
            return Err(ApiError::new(
                E_STATE,
                format!("instance `{id}` is not browser-hosted"),
            ));
        }
        let state = self
            .table
            .get_mut(id)
            .ok_or_else(|| ApiError::new(E_STATE, format!("no instance `{id}`")))?;
        state.transition(Lifecycle::Stopped, state.since)
    }

    /// Takes the session out of the pool so it runs without the pool lock. Until [`Pool::checkin`]
    /// the instance is busy, and a second checkout or a destroy is the retryable [`busy`] error.
    pub fn checkout(&mut self, id: InstanceId) -> Result<Session, ApiError> {
        if self.busy.contains(&id) {
            return Err(busy(id));
        }
        let session = self
            .sessions
            .remove(&id)
            .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
        self.busy.insert(id);
        Ok(session)
    }

    pub fn checkin(&mut self, session: Session) {
        self.busy.remove(&session.id);
        self.sessions.insert(session.id, session);
    }

    /// Asks the call running on `id` to return at its next slice, and later ones to refuse. Set
    /// before a shutdown ends the session, so it never waits on a long `run`.
    pub fn cancel(&self, id: InstanceId) -> bool {
        match self.cancels.get(&id) {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    pub fn is_busy(&self, id: InstanceId) -> bool {
        self.busy.contains(&id)
    }

    /// Destroys an instance, closing its endpoints first. The table keeps the id so a later call
    /// says "stopped" rather than "no such instance".
    pub fn destroy(&mut self, id: InstanceId) -> Result<Session, ApiError> {
        // Closing the endpoints hands a live session back, so `stop` ends an instance an endpoint
        // was running instead of refusing it as busy.
        crate::commands::endpoint::close_for_stop(self, id);
        if self.busy.contains(&id) {
            return Err(busy(id));
        }
        let now = self
            .sessions
            .get(&id)
            .map_or(VTime(0), |session| session.now());
        let state = self
            .table
            .get_mut(id)
            .ok_or_else(|| ApiError::new(E_STATE, format!("no instance `{id}`")))?;
        state.transition(Lifecycle::Stopped, now)?;
        self.cancels.remove(&id);
        self.sessions
            .remove(&id)
            .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))
    }
}

/// Every core command handler works on it, because `HandlerCx` carries nothing.
pub fn shared() -> &'static Mutex<Pool> {
    static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let pool = Pool::new();
        pool.set_reach(PoolReach(Reach::Process));
        Mutex::new(pool)
    })
}

/// The call on a checked-out session does not hold the pool, so the flag reaches it mid-run.
pub fn cancel(id: InstanceId) -> bool {
    with_pool(|pool| pool.cancel(id))
}

/// A corpus id unchanged, a path reduced to its file name on either separator, so an absolute path
/// never reaches an output.
pub fn fw_display(fw: &str) -> String {
    match fw.rsplit(['/', '\\']).next() {
        Some(name) if name.len() != fw.len() && !name.is_empty() => name.to_owned(),
        Some(_) if fw.ends_with(['/', '\\']) => "<path>".to_owned(),
        _ => fw.to_owned(),
    }
}

pub fn cancelled(id: InstanceId) -> ApiError {
    ApiError::new(
        E_STATE,
        format!("instance `{id}` is being stopped by its host, so the call ended early"),
    )
    .with_hint("the daemon is shutting down or stopping this instance; start a new one")
}

pub fn busy(id: InstanceId) -> ApiError {
    ApiError::new(
        E_STATE,
        format!("instance `{id}` is busy with another call"),
    )
    .retryable()
    .with_hint("calls to one instance serialize; repeat the call when the other returns")
}

/// Runs `f` on one session outside the pool lock, so a long `run` on `p1` holds no lock a `status`
/// on `p2` needs. The session is checked back in on a panic too.
pub fn with_session<R>(
    bind: impl FnOnce(&mut Pool) -> Result<InstanceId, ApiError>,
    f: impl FnOnce(&mut Session) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    struct Checkin(Option<Session>);
    impl Drop for Checkin {
        fn drop(&mut self) {
            if let Some(session) = self.0.take() {
                with_pool(|pool| pool.checkin(session));
            }
        }
    }
    let session = with_pool(|pool| {
        let id = bind(pool)?;
        pool.checkout(id)
    })?;
    let mut held = Checkin(Some(session));
    let session = held.0.as_mut().expect("checked out above");
    f(session)
}

/// A poisoned mutex is recovered: every mutation here is one insert or removal, so the pool stays
/// consistent, and one panicking command must not kill the daemon.
pub fn with_pool<R>(f: impl FnOnce(&mut Pool) -> R) -> R {
    let mut guard: MutexGuard<'_, Pool> = shared().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// [`with_pool`] for a host thread a command may be waiting for while it holds the pool (the HCI
/// transport, which `env --ble-bridge detach` joins); a blocking lock there would deadlock. `None`
/// means try again later.
pub fn try_with_pool<R>(f: impl FnOnce(&mut Pool) -> R) -> Option<R> {
    match shared().try_lock() {
        Ok(mut guard) => Some(f(&mut guard)),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(f(&mut e.into_inner())),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}
