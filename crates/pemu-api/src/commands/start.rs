//! `passportsim start`: create an instance from a firmware. The pool, [`Session`] and argument
//! helpers are re-exported here so their public paths stay.

use std::fmt::Write as _;

use pemu_core::time::VTime;
use pemu_machine::config::TimingProfileId;
use pemu_machine::machine::At;
use pemu_machine::stops::StopReason;

use crate::args::{
    duration_schema, enum_of, object, only, opt_bool, opt_duration, opt_str, opt_u64, usage,
};
use crate::error::{
    ApiError, E_ASSET_MISSING, E_DEADLOCK, E_INTERNAL, E_SECRET_REFUSED, E_STATE, E_TIMEOUT,
    E_USAGE,
};
use crate::instance::{InstanceId, Lifecycle};
use crate::matchers::str_enum;
use crate::output::Output;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

pub use crate::pool::*;
pub use crate::session::*;

str_enum! {
    pub enum Boot {
        /// Return immediately; the caller drives the boot with `run`.
        None = "none",
        /// Until the IDF main task prints `Calling app_main()`.
        UntilAppMain = "until_app_main",
        /// To the host's settle point ([`UiSettleHook`]), or, without one for the firmware, the
        /// display's first `ui-settled` event.
        UntilUiSettled = "until_ui_settled",
    }
}

str_enum! {
    pub enum BootCachePoint {
        /// The first LVGL safe point after `app_main` returns.
        UiSettled = "ui-settled",
    }
}

const APP_MAIN_LINE: &str = "Calling app_main()";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartArgs {
    /// Corpus id or path.
    pub fw: String,
    /// Shown by `status`.
    pub label: String,
    pub power_on: bool,
    pub mode: ClockMode,
    /// Part of run identity.
    pub seed: u64,
    /// Part of run identity, and the profile every later receipt names.
    pub profile: TimingProfileId,
    pub boot: Boot,
    pub boot_timeout: VTime,
    /// Journaled at instant 0 before the boot; `None` leaves the board's power-on default.
    pub usb: Option<super::env::UsbWorld>,
    pub boot_cache: Option<BootCachePoint>,
    /// A directory of `efuse_blk<N>.bin` files, or one dump file, imported instead of the
    /// synthesized eFuse. It taints the instance, so it needs [`StartArgs::confirm`] and a process
    /// that called [`allow_tainted_loads`]; MCP and HTTP never expose it.
    pub efuse_dump: Option<String>,
    /// Load [`StartArgs::fw`] even when its bytes are secret-bearing, which is otherwise refused.
    /// Same two gates as `efuse_dump`; the taint is read from the bytes, not the flag.
    pub allow_tainted: bool,
    /// A non-empty code a person supplies.
    pub confirm: Option<String>,
}

impl Default for StartArgs {
    fn default() -> StartArgs {
        StartArgs {
            fw: String::new(),
            label: String::new(),
            power_on: true,
            mode: ClockMode::Deterministic,
            seed: 1,
            profile: TimingProfileId::Fast,
            boot: Boot::UntilUiSettled,
            boot_timeout: VTime::from_ms(10_000),
            usb: None,
            boot_cache: None,
            efuse_dump: None,
            allow_tainted: false,
            confirm: None,
        }
    }
}

/// Off until the native CLI turns it on for a command it answers in process; a daemon, its mounts
/// and the browser never do. Atomic, because it is read inside calls that hold other locks.
static TAINTED_LOADS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The native CLI calls it; nothing that serves a transport does.
pub fn allow_tainted_loads() {
    TAINTED_LOADS.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// For a host that stops serving a person, and for tests.
pub fn refuse_tainted_loads() {
    TAINTED_LOADS.store(false, std::sync::atomic::Ordering::SeqCst);
}

pub fn tainted_loads_allowed() -> bool {
    TAINTED_LOADS.load(std::sync::atomic::Ordering::SeqCst)
}

fn tainted_load_refused(why: &str, hint: &str) -> ApiError {
    ApiError::new(E_SECRET_REFUSED, why.to_owned()).with_hint(hint.to_owned())
}

impl StartArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<StartArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "fw",
                "label",
                "power",
                "mode",
                "seed",
                "profile",
                "boot",
                "boot_timeout",
                "usb",
                "boot_cache",
                "efuse_dump",
                "allow_tainted",
                "confirm",
            ],
        )?;
        let usb = match opt_str(args, "usb")? {
            None => None,
            Some(text) => Some(super::env::UsbWorld::parse(text).ok_or_else(|| {
                usage(
                    "usb",
                    &format!("`{text}` is not one of unplugged, charger, host, open"),
                )
            })?),
        };
        let boot_cache = enum_of(
            args,
            "boot_cache",
            BootCachePoint::parse,
            &BootCachePoint::vocabulary(),
        )?;
        if boot_cache.is_some() {
            // The cache is the boot, so a second boot marker or a powered-off start contradicts it.
            if opt_str(args, "power")? == Some("off") {
                return Err(usage("boot_cache", "a powered-off instance does not boot"));
            }
            if let Some(boot) = opt_str(args, "boot")?
                && boot != Boot::UntilUiSettled.as_str()
            {
                return Err(usage(
                    "boot_cache",
                    &format!("`boot: {boot}` stops somewhere else than the cached point"),
                ));
            }
        }
        // The confirmation is read here, so a dump without one is refused before a path reaches the
        // host. The transport rule is the handler's ([`tainted_loads_allowed`]), which keeps this
        // function pure.
        let efuse_dump = match opt_str(args, "efuse_dump")? {
            None => None,
            Some("") => {
                return Err(usage(
                    "efuse_dump",
                    "the path of a dump, not an empty string",
                ));
            }
            Some(path) => Some(path.to_owned()),
        };
        // The same gates for the other tainting load.
        let allow_tainted = opt_bool(args, "allow_tainted")?.unwrap_or(false);
        let confirm = opt_str(args, "confirm")?.map(str::to_owned);
        let asks_for_taint = efuse_dump.is_some() || allow_tainted;
        if asks_for_taint && confirm.as_deref().is_none_or(|code| code.is_empty()) {
            let (why, hint) = if efuse_dump.is_some() {
                (
                    "an eFuse dump taints the instance, and a tainted load is confirmed by a \
                     person",
                    "pass the confirmation code in `confirm`; the emulator's own eFuse is \
                     synthesized and needs none",
                )
            } else {
                (
                    "`allow_tainted` loads a flash image whose own bytes may be secret-bearing, \
                     and a tainted load is confirmed by a person",
                    "pass the confirmation code in `confirm`; an image with an erased cardid \
                     window and no NVS credential key loads without it",
                )
            };
            return Err(tainted_load_refused(why, hint));
        }
        if !asks_for_taint && confirm.is_some() {
            return Err(usage(
                "confirm",
                "answers a tainted load; `start` asks for one only with `efuse_dump` or \
                 `allow_tainted`",
            ));
        }
        let power = opt_str(args, "power")?.unwrap_or("on");
        let power_on = match power {
            "on" => true,
            "off" => false,
            other => return Err(usage("power", &format!("`{other}` is not `on` or `off`"))),
        };
        Ok(StartArgs {
            fw: opt_str(args, "fw")?.unwrap_or(DEMO_FW).to_owned(),
            label: opt_str(args, "label")?.unwrap_or_default().to_owned(),
            power_on,
            mode: enum_of(args, "mode", ClockMode::parse, &ClockMode::vocabulary())?
                .unwrap_or(ClockMode::Deterministic),
            seed: opt_u64(args, "seed")?.unwrap_or(1),
            profile: enum_of(
                args,
                "profile",
                TimingProfileId::parse,
                &TimingProfileId::vocabulary(),
            )?
            .unwrap_or(TimingProfileId::Fast),
            boot: enum_of(args, "boot", Boot::parse, &Boot::vocabulary())?
                .unwrap_or(Boot::UntilUiSettled),
            boot_timeout: opt_duration(args, "boot_timeout")?.unwrap_or(VTime::from_ms(10_000)),
            usb,
            boot_cache,
            efuse_dump,
            allow_tainted,
            confirm,
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BootStatus {
    Matched,
    Timeout,
    Skipped,
    /// `until_ui_settled` on a firmware with no settle point (no ELF): the budget ran in full and
    /// whether the UI settled cannot be observed, so it is not reported as a failed boot.
    Unobservable,
}

impl BootStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            BootStatus::Matched => "matched",
            BootStatus::Timeout => "timeout",
            BootStatus::Skipped => "skipped",
            BootStatus::Unobservable => "unobservable",
        }
    }
}

/// The instance goes `Starting` -> `Paused` when the boot ends, or `Starting` -> `PoweredOff` for
/// `power: "off"`, since nothing runs while the rail is down.
pub fn start_on(pool: &mut Pool, args: &StartArgs) -> Result<Output, ApiError> {
    let id = pool.create(args)?;
    let (hook, settle) = (pool.boot_cache, pool.ui_settle);
    let booted = match pool.session_mut(id) {
        Some(session) => prepare(session, args, hook).and_then(|prepared| match prepared {
            Prepared::Cached(cache) => Ok((cached_status(&cache), Some(cache))),
            Prepared::Boot => match (args.power_on, args.boot) {
                (false, _) | (_, Boot::None) => Ok((BootStatus::Skipped, None)),
                (true, kind) => {
                    boot_session(session, kind, args.boot_timeout, settle).map(|b| (b, None))
                }
            },
        }),
        None => Err(ApiError::new(E_INTERNAL, "the new session vanished")),
    };
    let (boot, cache) = match booted {
        Ok(booted) => booted,
        Err(error) => {
            let _ = pool.destroy(id);
            return Err(error);
        }
    };
    finish_start(pool, id, args, boot, cache.as_ref())
}

enum Prepared {
    Boot,
    Cached(CachedBoot),
}

/// The USB world is an input at instant 0, so it is part of the boot the cache keys, never a change
/// after it.
fn prepare(
    session: &mut Session,
    args: &StartArgs,
    hook: Option<BootCacheHook>,
) -> Result<Prepared, ApiError> {
    if let Some(usb) = args.usb {
        for event in usb.events() {
            session.machine().input(At::Now, event).map_err(|_| {
                ApiError::new(
                    E_STATE,
                    "the machine refused the initial USB state at instant 0",
                )
            })?;
        }
    }
    if args.boot_cache.is_none() || !args.power_on {
        return Ok(Prepared::Boot);
    }
    let hook = hook.ok_or_else(|| {
        ApiError::new(
            E_STATE,
            "this process has no boot cache, so `boot_cache` cannot be honoured",
        )
        .with_hint("the daemon and the CLI install one; drop `boot_cache` to boot")
    })?;
    hook(args, session).map(Prepared::Cached)
}

fn cached_status(cache: &CachedBoot) -> BootStatus {
    if cache.settled {
        BootStatus::Matched
    } else {
        BootStatus::Timeout
    }
}

fn finish_start(
    pool: &mut Pool,
    id: InstanceId,
    args: &StartArgs,
    boot: BootStatus,
    cache: Option<&CachedBoot>,
) -> Result<Output, ApiError> {
    let lifecycle = if args.power_on {
        Lifecycle::Paused
    } else {
        Lifecycle::PoweredOff
    };
    let now = pool.session(id).map_or(VTime(0), Session::now);
    if let Some(state) = pool.table_mut().get_mut(id) {
        state.transition(lifecycle, now)?;
    }
    let session = pool
        .session_mut(id)
        .ok_or_else(|| ApiError::new(E_INTERNAL, "the new session vanished"))?;
    let receipt = session.receipt();
    let mut json = serde_json::json!({
        "instance": id.to_string(),
        "state": lifecycle.as_str(),
        "vt_us": receipt.vt_us,
        "mode": session.mode.as_str(),
        "image": {
            "fw": session.fw,
            "label": session.label,
            "seed": session.seed,
        },
        "boot": { "status": boot.as_str(), "kind": args.boot.as_str() },
    });
    if let Some(usb) = args.usb {
        json["usb"] = usb.as_str().into();
    }
    if let (Some(cache), Some(point)) = (cache, args.boot_cache) {
        json["boot"]["cache"] = serde_json::json!({
            "point": point.as_str(),
            "hit": cache.hit,
            "key": cache.key,
            "store": cache.store,
        });
    }
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{id} {} vt={}us fw={}",
        lifecycle.as_str(),
        receipt.vt_us,
        session.fw
    );
    let _ = write!(text, "boot: {} {}", boot.as_str(), args.boot.as_str());
    if let Some(cache) = cache {
        let _ = write!(
            text,
            " cache {} ({})",
            if cache.hit { "hit" } else { "miss" },
            cache.store
        );
    }
    Ok(Output::new(json, text, receipt))
}

/// A boot that misses its marker in time is reported in `boot.status`, never refused. A guest that
/// parks waiting for input first is `E_DEADLOCK`, since no budget would reach the marker.
fn boot_session(
    session: &mut Session,
    kind: Boot,
    budget: VTime,
    settle: Option<UiSettleHook>,
) -> Result<BootStatus, ApiError> {
    let deadline = VTime(session.now().0.saturating_add(budget.0));
    let matcher = match kind {
        Boot::None => return Ok(BootStatus::Skipped),
        Boot::UntilAppMain => crate::commands::run::Wait::serial_contains(APP_MAIN_LINE),
        Boot::UntilUiSettled => {
            if let Some(settled) = settle
                .map(|hook| hook(session, budget))
                .transpose()?
                .flatten()
            {
                return Ok(if settled {
                    BootStatus::Matched
                } else {
                    BootStatus::Timeout
                });
            }
            // Nothing posts `ui-settled` outside unit tests, so running out the budget says nothing
            // about the UI: `Unobservable`, not `Timeout`.
            return boot_poll(
                session,
                &crate::commands::run::Wait::ui_settled(),
                deadline,
                BootStatus::Unobservable,
            );
        }
    };
    boot_poll(session, &matcher, deadline, BootStatus::Timeout)
}

/// `at_deadline` is what a boot that reached the deadline without the marker reports.
fn boot_poll(
    session: &mut Session,
    matcher: &crate::commands::run::Wait,
    deadline: VTime,
    at_deadline: BootStatus,
) -> Result<BootStatus, ApiError> {
    use crate::commands::run::{Stop, WallBudget, poll};
    Ok(
        match poll(
            session,
            Some(matcher),
            &[],
            deadline,
            &WallBudget::unlimited(),
        ) {
            Ok(Stop::Matched(_)) => BootStatus::Matched,
            Ok(Stop::Waiting) => {
                let at = session.now().as_us();
                let error = fault_of(&StopReason::Deadlock)
                    .expect("a deadlock is a fault")
                    .at_vt_us(at);
                return Err(crate::commands::inspect::deadlock_envelope(session, error));
            }
            Ok(Stop::Deadlocked(report)) => {
                let at = session.now().as_us();
                let error = crate::commands::inspect::task_deadlock_error(&report).at_vt_us(at);
                return Err(crate::commands::inspect::deadlock_envelope(session, error));
            }
            Ok(Stop::Deadline) => at_deadline,
            // Any other fault is a boot that did not get there, with the fault in its envelope.
            Ok(Stop::Failed(_)) | Err(_) => BootStatus::Timeout,
        },
    )
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`start` arguments.",
        "properties": {
            "fw": { "type": "string", "description": "Corpus id or path; the bundled demo (`official`) when omitted." },
            "label": { "type": "string", "description": "Label for `status`." },
            "power": { "type": "string", "enum": ["on", "off"], "description": "Power rail at start (on)." },
            "mode": { "type": "string", "enum": ["deterministic", "realtime"], "description": "Pacing mode (deterministic)." },
            "seed": { "type": "integer", "minimum": 0, "description": "RNG seed (1)." },
            "profile": { "type": "string", "enum": ["fast", "device"], "description": "Timing profile (fast)." },
            "boot": { "type": "string", "enum": ["none", "until_app_main", "until_ui_settled"], "description": "Boot marker (until_ui_settled)." },
            "boot_timeout": duration_schema("Boot budget, 10s."),
            "usb": { "type": "string", "enum": ["unplugged", "charger", "host", "open"], "description": "Initial USB world (board default)." },
            "boot_cache": { "type": "string", "enum": ["ui-settled"], "description": "Restore or fill the boot cache." },
            "efuse_dump": { "type": "string", "description": "Directory of `efuse_blk<N>.bin` files, or one dump file, to import instead of the synthesized eFuse. Taints the instance: CLI only, with `confirm`." },
            "allow_tainted": { "type": "boolean", "description": "Load `fw` even when its cardid window is not erased or its NVS holds credential keys, which is otherwise refused. Taints the instance: CLI only, with `confirm`." },
            "confirm": { "type": "string", "description": "Human confirmation code for a tainted load, `efuse_dump` or `allow_tainted`." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "state": { "type": "string" },
            "vt_us": { "type": "integer" },
            "mode": { "type": "string" },
            "image": { "type": "object" },
            "boot": { "type": "object" }
        }
    })
}

/// Create an instance from a firmware and boot it.
#[command(
    api_crate = crate,
    name = "start",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    cli(positional = ["fw"]),
    errors(E_USAGE, E_STATE, E_ASSET_MISSING, E_TIMEOUT, E_DEADLOCK, E_INTERNAL, E_SECRET_REFUSED),
    example(
        title = "Start the official demo and wait for the UI",
        args = r#"{"fw":"official"}"#,
    ),
    example(
        title = "Start powered off, to drive the power button by hand",
        args = r#"{"fw":"official","power":"off","boot":"none","label":"cold"}"#,
    ),
)]
pub fn start(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = StartArgs::from_json(&args)?;
    // The confirmation says a person asked; this says the ask reached a process a person is at. A
    // daemon, its mounts and the browser never allow it.
    if (args.efuse_dump.is_some() || args.allow_tainted) && !tainted_loads_allowed() {
        let hint = if args.efuse_dump.is_some() {
            "run it as `passportsim start --efuse-dump <dir> --confirm <code> --ephemeral` in a \
             terminal"
        } else {
            "run it as `passportsim start <path> --allow-tainted --confirm <code> --ephemeral` \
             in a terminal"
        };
        return Err(tainted_load_refused(
            "this load taints the instance, and this process does not answer tainted loads: they \
             are a native CLI operation, and MCP and HTTP never expose them",
            hint,
        ));
    }
    // The machine is built and booted outside the pool lock, so a slow boot holds up no other
    // instance.
    let backend = {
        let factory = with_pool(|pool| pool.factory);
        factory(&args)?
    };
    let (id, hook, settle) =
        with_pool(|pool| (pool.attach(&args, backend), pool.boot_cache, pool.ui_settle));
    // A refusal ends the instance it created rather than leaving a half-started one.
    let booted = with_session(
        |_| Ok(id),
        |session| match prepare(session, &args, hook)? {
            Prepared::Cached(cache) => Ok((cached_status(&cache), Some(cache))),
            Prepared::Boot => match (args.power_on, args.boot) {
                (false, _) | (_, Boot::None) => Ok((BootStatus::Skipped, None)),
                (true, kind) => {
                    boot_session(session, kind, args.boot_timeout, settle).map(|b| (b, None))
                }
            },
        },
    );
    let (boot, cache) = match booted {
        Ok(booted) => booted,
        Err(error) => {
            with_pool(|pool| {
                let _ = pool.destroy(id);
            });
            return Err(error);
        }
    };
    with_pool(|pool| finish_start(pool, id, &args, boot, cache.as_ref()))
}

/// The prebuilt official demo a packaged binary carries. A name, not a path: the payload answers it
/// on a clean host, the corpus where there is one, and a plain `cargo build` answers
/// `E_ASSET_MISSING` naming `xtask package`.
pub const DEMO_FW: &str = "official";

/// Built here so `start` owns the message; `pemu-api` never looks a path up itself.
pub fn firmware_not_found(fw: &str) -> ApiError {
    ApiError::new(E_ASSET_MISSING, format!("no firmware `{fw}`")).with_hint(
        "`doctor` lists the corpus entries this machine has; `start <path>` takes an `idf.py` \
         build directory, a merged image or a `.pebundle`",
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use pemu_core::hostio::{EventKind, HostEvent, HostIo, SerialStream};
    use pemu_machine::MachineApi;
    use pemu_machine::run::{RunLimits, RunOutcome};

    use crate::pool::Reach;
    use crate::receipt::Receipt;
    use crate::session::{binding_into_receipt, fault_counters_into_receipt};
    use pemu_core::input::InputEvent;
    use pemu_machine::machine::{GuestMem, InputError, Receipt as LedgerReceipt};

    /// For a scripted backend: taking, restoring and forking refuse, redaction is the header stamp,
    /// and the state hash is constant.
    macro_rules! refuse_snapshots {
        ($ty:ty) => {
            impl pemu_machine::SnapshotMachine for $ty {
                fn snapshot(
                    &self,
                    _opts: pemu_core::snap::SnapOpts,
                ) -> Result<pemu_core::snap::Snapshot, pemu_core::snap::SnapError> {
                    Err($crate::commands::start::tests::no_snapshot())
                }

                fn redact(
                    &self,
                    snapshot: &mut pemu_core::snap::Snapshot,
                ) -> Result<pemu_machine::snapshot::Redaction, pemu_core::snap::SnapError> {
                    snapshot.header.exported = true;
                    snapshot.header.redacted = true;
                    Ok(pemu_machine::snapshot::Redaction::default())
                }

                fn restore(
                    &mut self,
                    _snapshot: &pemu_core::snap::Snapshot,
                ) -> Result<(), pemu_core::snap::SnapError> {
                    Err($crate::commands::start::tests::no_snapshot())
                }

                fn fork(
                    &self,
                    _live: pemu_core::snap::LivePolicy,
                ) -> Result<
                    Box<dyn pemu_machine::SnapshotMachine + Send>,
                    pemu_core::snap::SnapError,
                > {
                    Err($crate::commands::start::tests::no_snapshot())
                }

                fn state_hash(&self) -> [u8; 32] {
                    [0; 32]
                }
            }
        };
    }
    pub(crate) use refuse_snapshots;

    pub(crate) fn no_snapshot() -> pemu_core::snap::SnapError {
        pemu_core::snap::SnapError::Malformed {
            at: "machine",
            reason: "this test backend cannot snapshot",
        }
    }

    refuse_snapshots!(TestMachine);

    /// Not `pemu_testkit::MockMachine`, which `pemu-api` may not depend on; unlike it, this
    /// releases into the real `HostIo` rings the commands read.
    pub(crate) struct TestMachine {
        vt: VTime,
        io: HostIo,
        pending: Vec<(VTime, Scripted)>,
        insns_per_us: u64,
        pub(crate) journal: Vec<InputEvent>,
        /// While one is pending the hart is not reported as waiting before it.
        pending_inputs: Vec<VTime>,
        /// From this instant a run stops with `StopReason::Deadlock`, until an input arrives.
        waits_from: Option<VTime>,
        /// `None` wakes it for good.
        rewaits_from: Option<VTime>,
        tainted: bool,
        /// The fidelity ledger, panel and clock fields a real machine fills.
        drained: LedgerReceipt,
    }

    #[derive(Clone, Debug)]
    pub(crate) enum Scripted {
        /// The newline is added.
        Line(SerialStream, String),
        Event(EventKind),
    }

    impl TestMachine {
        pub(crate) fn new() -> TestMachine {
            TestMachine {
                vt: VTime(0),
                io: HostIo::new(8192),
                pending: Vec::new(),
                insns_per_us: 160,
                journal: Vec::new(),
                pending_inputs: Vec::new(),
                waits_from: None,
                rewaits_from: None,
                tainted: false,
                drained: LedgerReceipt::default(),
            }
        }

        pub(crate) fn with_drained(mut self, drained: LedgerReceipt) -> TestMachine {
            self.drained = drained;
            self
        }

        pub(crate) fn built_on_a_secret_input(mut self) -> TestMachine {
            self.tainted = true;
            self
        }

        pub(crate) fn line(mut self, ms: u64, text: &str) -> TestMachine {
            self.pending.push((
                VTime::from_ms(ms),
                Scripted::Line(SerialStream::UsjTx, text.to_owned()),
            ));
            self
        }

        pub(crate) fn waits_for_input_at(mut self, ms: u64) -> TestMachine {
            self.waits_from = Some(VTime::from_ms(ms));
            self
        }

        pub(crate) fn waits_again_at(mut self, ms: u64) -> TestMachine {
            self.rewaits_from = Some(VTime::from_ms(ms));
            self
        }

        pub(crate) fn event(mut self, ms: u64, kind: EventKind) -> TestMachine {
            self.pending
                .push((VTime::from_ms(ms), Scripted::Event(kind)));
            self
        }

        /// In instant, then insertion order.
        fn release(&mut self, at: VTime) {
            let mut due = Vec::new();
            let mut keep = Vec::new();
            for (vt, out) in self.pending.drain(..) {
                if vt.0 <= at.0 {
                    due.push((vt, out));
                } else {
                    keep.push((vt, out));
                }
            }
            self.pending = keep;
            due.sort_by_key(|(vt, _)| *vt);
            for (vt, out) in due {
                match out {
                    Scripted::Line(stream, text) => {
                        let mut bytes = text.into_bytes();
                        bytes.push(b'\n');
                        self.io.serial_write(stream, &bytes, vt);
                    }
                    Scripted::Event(kind) => self.io.events.emit(HostEvent { kind, vt, arg: 0 }),
                }
            }
        }
    }

    impl MachineApi for TestMachine {
        fn run(&mut self, lim: RunLimits) -> RunOutcome {
            let until = match (lim.until, lim.max_insns) {
                (Some(t), _) => VTime(t.0.max(self.vt.0)),
                (None, Some(n)) => VTime(
                    self.vt
                        .0
                        .saturating_add(VTime::from_us(n.div_ceil(self.insns_per_us.max(1))).0),
                ),
                (None, None) => self.vt,
            };
            // A pending input moves the wait past its instant.
            let beyond = self.pending_inputs.iter().any(|t| t.0 > until.0);
            let last_due = self
                .pending_inputs
                .iter()
                .filter(|t| t.0 <= until.0)
                .max()
                .copied();
            self.pending_inputs.retain(|t| t.0 > until.0);
            let parked = self
                .waits_from
                .filter(|_| !beyond)
                .map(|from| last_due.map_or(from, |t| VTime(t.0.max(from.0))))
                .filter(|from| until.0 > from.0);
            let until = parked.map_or(until, |from| VTime(from.0.max(self.vt.0)));
            // So a line scheduled at the deadline is visible when `run` returns.
            self.release(until);
            let insns = (until.0.saturating_sub(self.vt.0) / VTime::from_us(1).0)
                .saturating_mul(self.insns_per_us);
            self.vt = until;
            RunOutcome {
                reason: if parked.is_some() {
                    StopReason::Deadlock
                } else if lim.until.is_some() {
                    StopReason::Until
                } else {
                    StopReason::MaxInsns
                },
                vt: self.vt,
                insns,
                ff_insns: 0,
                idle_ps: 0,
            }
        }

        fn input(&mut self, at: At, ev: InputEvent) -> Result<u64, InputError> {
            self.waits_from = self.rewaits_from;
            if let At::Vt(t) = at
                && t.0 > self.vt.0
            {
                self.pending_inputs.push(t);
            }
            match &ev {
                InputEvent::UsbCable { plugged } => self.io.usj_ctrl.set_cable(*plugged),
                InputEvent::UsbClient { open } => self.io.usj_ctrl.set_client_open(*open),
                InputEvent::UsbLine { dtr, rts } => self.io.usj_ctrl.set_line_state(*rts, *dtr),
                InputEvent::SerialIn { data, .. } => {
                    self.io.usj_rx.push(data);
                }
                _ => {}
            }
            self.journal.push(ev);
            Ok(self.journal.len() as u64 - 1)
        }

        fn io(&mut self) -> &mut HostIo {
            &mut self.io
        }

        fn now(&self) -> VTime {
            self.vt
        }

        fn guest_mem(&mut self) -> GuestMem<'_> {
            unreachable!("no core command reads guest memory")
        }

        fn is_tainted(&self) -> bool {
            false
        }
        fn receipt(&mut self) -> LedgerReceipt {
            LedgerReceipt {
                tainted: self.tainted,
                journal_len: Some(self.journal.len() as u64),
                ..self.drained.clone()
            }
        }
    }

    pub(crate) fn pool_of(machine: TestMachine) -> (Pool, InstanceId) {
        let mut pool = Pool::new();
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&args, Box::new(machine));
        (pool, id)
    }

    pub(crate) fn started(machine: TestMachine) -> (Pool, InstanceId) {
        let (mut pool, id) = pool_of(machine);
        pool.table_mut()
            .get_mut(id)
            .expect("the instance was just created")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        (pool, id)
    }

    #[test]
    fn pool_tables_are_one_per_type_shared_by_clones_and_by_no_other_pool() {
        #[derive(Default)]
        struct Counts(u32);
        #[derive(Default)]
        struct Names(Vec<&'static str>);
        let first = PoolTables::default();
        let shared = first.clone();
        let other = PoolTables::default();
        first.with(|counts: &mut Counts| counts.0 += 1);
        shared.with(|counts: &mut Counts| counts.0 += 1);
        shared.with(|names: &mut Names| {
            names.0.push("inner");
            first.with(|counts: &mut Counts| counts.0 += 1);
        });
        assert_eq!(first.with(|counts: &mut Counts| counts.0), 3);
        assert_eq!(first.with(|names: &mut Names| names.0.clone()), ["inner"]);
        assert_eq!(other.with(|counts: &mut Counts| counts.0), 0);
        assert!(other.with(|names: &mut Names| names.0.is_empty()));
    }

    #[test]
    fn a_pool_reach_reaches_its_own_pool_and_never_waits() {
        assert!(Pool::new().reach().is_none());
        let pool = Pool::new().into_shared();
        let reach = pool
            .lock()
            .expect("free")
            .reach()
            .expect("a shared pool is reachable");
        let id = pool
            .lock()
            .expect("free")
            .attach(&StartArgs::default(), Box::new(TestMachine::new()));
        assert_eq!(
            reach.try_with(|p| p.session(id).map(|s| s.id)),
            Reached::Ran(Some(id))
        );
        let held = pool.lock().expect("free");
        assert_eq!(reach.try_with(|_| ()), Reached::Busy);
        drop(held);
        drop(pool);
        assert_eq!(reach.try_with(|_| ()), Reached::Gone);
        assert!(matches!(
            with_pool(|p| p.reach()).map(|r| r.0),
            Some(Reach::Process)
        ));
    }

    #[test]
    fn the_receipt_names_the_class_the_running_thread_read_back() {
        let _world = crate::commands::snapshot::tests::world();
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let mut pool = Pool::new();
        pool.set_thread_qos(Some(|| "user-interactive"));
        let id = pool.attach(&args, Box::new(TestMachine::new()));
        let session = pool.session_mut(id).expect("the session");
        assert!(session.receipt().to_json().get("host_qos").is_none());
        session.run_until(VTime(1_000_000));
        assert_eq!(session.host_qos(), Some("user-interactive"));
        let receipt = session.receipt();
        assert_eq!(receipt.to_json()["host_qos"], "user-interactive");
        assert_eq!(
            Receipt::from_json(&receipt.to_json()).expect("round trips"),
            receipt
        );
        session.run_insns(1);
        assert_eq!(session.host_qos(), Some("user-interactive"));
        // A thread outside the run methods records its own.
        session.note_host_qos("default");
        assert_eq!(session.receipt().to_json()["host_qos"], "default");

        let (mut plain, id) = started(TestMachine::new());
        let session = plain.session_mut(id).expect("the session");
        session.run_until(VTime(1_000_000));
        assert_eq!(session.host_qos(), None);
        assert!(session.receipt().to_json().get("host_qos").is_none());
    }

    #[test]
    fn the_fault_counters_reach_the_receipt() {
        let mut receipt = Receipt::default();
        fault_counters_into_receipt(
            pemu_machine::machine::FaultCounters {
                dma_faults: 3,
                pcm_width_faults: 1,
            },
            &mut receipt,
        );
        let json = receipt.to_json();
        assert_eq!(json["fault_counters"]["dma"], 3);
        assert_eq!(json["fault_counters"]["pcm_width"], 1);
        assert_eq!(Receipt::from_json(&json).expect("round trips"), receipt);
    }

    /// The machine half is `pemu_machine::machine`'s own test; this is the seam between the two
    /// receipts.
    #[test]
    fn the_four_caveat_sources_reach_the_receipt_and_the_exit_code() {
        use crate::receipt::{CaveatKind, Strictness, Verdict, exit_code};
        // The secret store is process-wide and every pool's first instance is `p1`, so this takes
        // the store's test guard.
        let _world = crate::commands::snapshot::tests::world();
        let drained = LedgerReceipt {
            classes_touched: pemu_machine::machine::ClassesTouched {
                c: vec!["battery".to_string()],
                u: vec!["twai".to_string()],
            },
            unmodeled_first_touch: vec!["twai.0x0000".to_string()],
            timing_lint: vec!["st7789.sleep_cycle_too_fast.0x11".to_string()],
            ..LedgerReceipt::default()
        };
        let (mut pool, id) = started(TestMachine::new().with_drained(drained));
        let receipt = pool.session_mut(id).expect("the session").receipt();
        assert_eq!(receipt.classes_touched.c, vec!["battery".to_string()]);
        assert_eq!(receipt.classes_touched.u, vec!["twai".to_string()]);
        assert_eq!(
            receipt.unmodeled_first_touch,
            vec!["twai.0x0000".to_string()]
        );
        assert_eq!(
            receipt.timing_lint,
            vec!["st7789.sleep_cycle_too_fast.0x11".to_string()]
        );
        let caveats = receipt.caveats();
        let kinds: Vec<CaveatKind> = caveats.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![
                CaveatKind::ClassU,
                CaveatKind::Unmodeled,
                CaveatKind::TimingLint
            ],
            "{caveats:?}"
        );
        let verdict = receipt.verdict(true);
        assert_eq!(verdict, Verdict::PassWithCaveats);
        assert_eq!(exit_code(verdict, &caveats, Strictness::Strict), 7);
        assert_eq!(exit_code(verdict, &caveats, Strictness::Lenient), 10);

        // A chain that caveated every run would pass the assertions above while being useless.
        let (mut pool, id) = started(TestMachine::new());
        let clean = pool.session_mut(id).expect("the session").receipt();
        assert!(clean.caveats().is_empty());
        assert_eq!(clean.verdict(true), Verdict::Pass);
        assert_eq!(
            exit_code(clean.verdict(true), &clean.caveats(), Strictness::Strict),
            0
        );
    }

    /// Recorded once per tripwire: a caveat list that grew on every resume would count retries, not
    /// what the guest did.
    #[test]
    fn a_tripwire_hit_reaches_the_receipt_once() {
        use crate::receipt::CaveatKind;
        let _world = crate::commands::snapshot::tests::world();
        use pemu_machine::hle::HleTripKind;
        use pemu_machine::stops::TripReport;
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the session");
        assert!(session.receipt().hle.tripwires_hit.is_empty());
        let stop = StopReason::Tripwire(TripReport {
            kind: HleTripKind::Rwip,
            pc: 0x4000_0000,
            detail: "r_rwip_isr".to_string(),
            caller: 0x4200_0000,
            feature: None,
        });
        session.note_tripwire(&stop);
        session.note_tripwire(&stop);
        session.note_tripwire(&StopReason::Until);
        let receipt = session.receipt();
        assert_eq!(receipt.hle.tripwires_hit, vec!["rwip:r_rwip_isr"]);
        let caveats = receipt.caveats();
        assert_eq!(caveats.len(), 1, "{caveats:?}");
        assert_eq!(caveats[0].kind, CaveatKind::Tripwire);
        assert_eq!(caveats[0].detail, "rwip:r_rwip_isr");
    }

    #[test]
    fn the_binding_record_reaches_the_receipt() {
        use pemu_machine::hle::{HleBindingRecord, HleFeatureStatus};
        let mut receipt = Receipt::default();
        let record = HleBindingRecord {
            profile_id: "idf-5.5.3".to_string(),
            app_elf_sha256: [0xAB; 32],
            features: [
                ("ble".to_string(), HleFeatureStatus::Bound),
                ("wifi".to_string(), HleFeatureStatus::Disabled),
            ]
            .into_iter()
            .collect(),
            log_lines: [(
                "ble".to_string(),
                pemu_machine::hle::HleRadioLogLines {
                    synthesized: 5,
                    verified: false,
                },
            )]
            .into_iter()
            .collect(),
        };
        binding_into_receipt(&record, &[], None, &mut receipt);
        assert_eq!(receipt.hle.bound.as_deref(), Some("idf-5.5.3/ble"));
        // Not written a second time under `binding`.
        assert_eq!(receipt.hle.synthesized_log_lines, 5);
        assert_eq!(receipt.to_json()["hle"]["synthesized_log_lines"], 5);
        assert_eq!(
            receipt.to_json()["binding"]["log_lines"]["ble"],
            "unverified"
        );
        assert_eq!(
            receipt.to_json()["binding"]["log_lines"]["ble"]["synthesized_log_lines"],
            serde_json::Value::Null,
            "the retired second spelling is gone"
        );
        let json = receipt.to_json();
        assert_eq!(json["binding"]["features"]["wifi"], "disabled");
        assert_eq!(json["binding"]["app_elf_sha256"], "ab".repeat(32));
        assert_eq!(json.get("radio"), None);
        let mut receipt = Receipt::default();
        let record = HleBindingRecord {
            features: [("ble".to_string(), HleFeatureStatus::Disabled)]
                .into_iter()
                .collect(),
            ..HleBindingRecord::default()
        };
        binding_into_receipt(&record, &[], None, &mut receipt);
        let json = receipt.to_json();
        assert_eq!(json["hle"]["bound"], serde_json::Value::Null);
        assert_eq!(json["binding"]["features"]["ble"], "disabled");
        assert_eq!(json["binding"]["app_elf_sha256"], serde_json::Value::Null);
        assert_eq!(json["radio"], "unbound (no ELF)");
        assert_eq!(Receipt::from_json(&json).expect("round trips"), receipt);
        // The symbols were recovered from the image, so the radio is bound, not unbound.
        let mut receipt = Receipt::default();
        let record = HleBindingRecord {
            profile_id: "idf-5.5.3".to_string(),
            features: [("ble".to_string(), HleFeatureStatus::Bound)]
                .into_iter()
                .collect(),
            ..HleBindingRecord::default()
        };
        binding_into_receipt(&record, &[], None, &mut receipt);
        let json = receipt.to_json();
        assert_eq!(json["hle"]["bound"], "idf-5.5.3/ble");
        assert_eq!(json["binding"]["app_elf_sha256"], serde_json::Value::Null);
        assert_eq!(json["radio"], "bound from the image (no ELF)");
        assert_eq!(
            json["binding"].get("mismatches"),
            None,
            "nothing was refused"
        );
        assert_eq!(
            json["binding"].get("idf_ver"),
            None,
            "no descriptor, no version"
        );
    }

    #[test]
    fn a_refused_module_says_which_checks_refused_it() {
        use pemu_machine::hle::{HleBindingMismatch, HleBindingRecord, HleFeatureStatus};
        let record = HleBindingRecord {
            features: [("wifi".to_string(), HleFeatureStatus::UnsupportedImage)]
                .into_iter()
                .collect(),
            ..HleBindingRecord::default()
        };
        let why = vec![(
            "wifi".to_string(),
            vec![HleBindingMismatch {
                symbol: "esp_wifi_scan_stop".to_string(),
                field: pemu_machine::hle::HleMismatchField::Missing,
                expected: "found in the image".to_string(),
                found: "no place in the image has a pinned shape of it".to_string(),
            }],
        )];
        let mut receipt = Receipt::default();
        binding_into_receipt(&record, &why, Some("v5.5.3-dirty"), &mut receipt);
        let json = receipt.to_json();
        assert_eq!(json["binding"]["features"]["wifi"], "unsupported image");
        assert_eq!(json["binding"]["idf_ver"], "v5.5.3-dirty");
        assert_eq!(
            json["binding"]["mismatches"]["wifi"],
            serde_json::json!([{
                "symbol": "esp_wifi_scan_stop",
                "field": "missing",
                "expected": "found in the image",
                "found": "no place in the image has a pinned shape of it",
            }])
        );
        assert_eq!(Receipt::from_json(&json).expect("round trips"), receipt);
    }

    #[test]
    fn a_checked_out_session_is_busy_for_bind_destroy_and_a_second_checkout_until_checkin() {
        let (mut pool, id) = started(TestMachine::new());
        let annotations = super::super::run::SPEC_RUN.annotations;
        let session = pool.checkout(id).expect("the session is in the pool");
        assert!(pool.is_busy(id));
        assert!(pool.session(id).is_none());
        for error in [
            pool.checkout(id).map(|_| ()).expect_err("already out"),
            pool.bind(annotations, Some("p1"))
                .map(|_| ())
                .expect_err("busy"),
            pool.destroy(id).map(|_| ()).expect_err("busy"),
        ] {
            assert_eq!(error.code, E_STATE);
            assert!(error.retryable, "{error:?}");
            assert!(error.message.contains("busy"), "{}", error.message);
        }
        pool.checkin(session);
        assert!(!pool.is_busy(id));
        assert_eq!(pool.bind(annotations, Some("p1")).expect("back"), id);
        pool.destroy(id).expect("no longer busy");
    }

    #[test]
    fn the_default_factory_says_how_a_host_installs_one() {
        let error = Pool::new()
            .create(&StartArgs::default())
            .expect_err("no core is built yet");
        assert_eq!(error.code, E_INTERNAL);
        assert!(
            error
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("Pool::with_factory"),
            "the refusal says how a host installs a factory"
        );
    }

    #[test]
    fn start_boots_until_the_app_main_line_and_reports_it() {
        let mut pool = Pool::with_factory(|_| {
            Ok(Box::new(
                TestMachine::new().line(300, "I (300) main_task: Calling app_main()"),
            ))
        });
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::UntilAppMain,
            ..StartArgs::default()
        };
        let out = start_on(&mut pool, &args).expect("the marker appears inside the budget");
        assert_eq!(out.json["instance"], "p1");
        assert_eq!(out.json["state"], "paused");
        assert_eq!(out.json["boot"]["status"], "matched");
        assert_eq!(out.json["image"]["fw"], "official");
        assert!(out.json["vt_us"].as_u64().expect("vt") >= 300_000);
    }

    #[test]
    fn a_boot_cache_start_reports_the_hook_and_skips_the_marker() {
        fn hit(args: &StartArgs, session: &mut Session) -> Result<CachedBoot, ApiError> {
            assert_eq!(args.boot_cache, Some(BootCachePoint::UiSettled));
            assert_eq!(session.fw, "official");
            Ok(CachedBoot {
                hit: true,
                key: "ab".repeat(32),
                store: "disk",
                settled: true,
            })
        }
        let mut pool = Pool::with_factory(|_| Ok(Box::new(TestMachine::new())));
        let args = StartArgs::from_json(&serde_json::json!({
            "fw": "official",
            "boot_cache": "ui-settled",
        }))
        .expect("inside the schema");
        let refused = start_on(&mut pool, &args).expect_err("no cache installed");
        assert_eq!(refused.code, E_STATE);
        assert!(
            pool.live_ids().is_empty(),
            "a refused start leaves no instance"
        );

        pool.set_boot_cache(Some(hit));
        let out = start_on(&mut pool, &args).expect("the hook answers");
        assert_eq!(out.json["boot"]["status"], "matched");
        assert_eq!(out.json["boot"]["cache"]["hit"], true);
        assert_eq!(out.json["boot"]["cache"]["store"], "disk");
        assert_eq!(
            out.json["vt_us"], 0,
            "the marker did not run on top of the cache"
        );
        assert!(out.text.contains("cache hit (disk)"), "{}", out.text);
    }

    #[test]
    fn until_ui_settled_goes_through_the_settle_hook_and_falls_back_to_the_event() {
        fn settles(session: &mut Session, budget: VTime) -> Result<Option<bool>, ApiError> {
            assert_eq!(budget, VTime::from_ms(10_000));
            if session.fw != "official" {
                return Ok(None);
            }
            session.run_until(VTime::from_ms(450));
            Ok(Some(true))
        }
        let mut pool = Pool::with_factory(|args| {
            Ok(Box::new(if args.fw == "official" {
                TestMachine::new()
            } else {
                TestMachine::new().event(12, EventKind::UiSettled)
            }))
        });
        pool.set_ui_settle(Some(settles));
        let args = |fw: &str| StartArgs {
            fw: fw.to_owned(),
            ..StartArgs::default()
        };
        let out = start_on(&mut pool, &args("official")).expect("settles");
        assert_eq!(
            (
                out.json["boot"]["status"].as_str(),
                out.json["vt_us"].as_u64()
            ),
            (Some("matched"), Some(450_000)),
            "the hook's instant, with no event emitted"
        );
        let out = start_on(&mut pool, &args("pk")).expect("the event");
        assert_eq!(out.json["boot"]["status"], "matched");
        assert!(out.json["vt_us"].as_u64().expect("vt") < 450_000);

        fn refuses(_: &mut Session, _: VTime) -> Result<Option<bool>, ApiError> {
            Err(ApiError::new(E_STATE, "cancelled"))
        }
        pool.set_ui_settle(Some(refuses));
        let before = pool.live_ids().len();
        assert!(start_on(&mut pool, &args("official")).is_err());
        assert_eq!(
            pool.live_ids().len(),
            before,
            "a failed boot leaves no instance"
        );
    }

    #[test]
    fn a_boot_cache_contradicted_by_power_or_boot_is_usage() {
        for extra in [
            serde_json::json!({"power": "off"}),
            serde_json::json!({"boot": "none"}),
        ] {
            let mut json = serde_json::json!({"fw": "official", "boot_cache": "ui-settled"});
            json.as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            let error = StartArgs::from_json(&json).expect_err("contradiction");
            assert_eq!(error.code, E_USAGE, "{json}");
        }
        assert_eq!(
            StartArgs::from_json(&serde_json::json!({"fw": "pk", "usb": "open"}))
                .expect("usb")
                .usb,
            Some(super::super::env::UsbWorld::Open)
        );
        assert!(StartArgs::from_json(&serde_json::json!({"fw": "pk", "usb": "u3"})).is_err());
    }

    #[test]
    fn the_initial_usb_world_is_journaled_before_the_boot() {
        let (machine, journal) = crate::commands::env::tests::JournalMachine::new();
        let mut pool = Pool::new();
        let args = StartArgs {
            fw: "official".to_owned(),
            usb: Some(super::super::env::UsbWorld::Unplugged),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&args, Box::new(machine));
        let session = pool.session_mut(id).expect("attached");
        assert!(matches!(prepare(session, &args, None), Ok(Prepared::Boot)));
        assert_eq!(
            *journal.lock().expect("journal"),
            [
                InputEvent::UsbCable { plugged: false },
                InputEvent::UsbClient { open: false }
            ]
        );
    }

    #[test]
    fn a_boot_marker_that_never_appears_is_reported_not_refused() {
        let mut pool = Pool::with_factory(|_| Ok(Box::new(TestMachine::new())));
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::UntilAppMain,
            boot_timeout: VTime::from_ms(50),
            ..StartArgs::default()
        };
        let out = start_on(&mut pool, &args).expect("a slow boot is reported, never refused");
        assert_eq!(out.json["boot"]["status"], "timeout");
        assert_eq!(out.json["vt_us"], 50_000);
    }

    #[test]
    fn a_settle_marker_this_host_cannot_see_is_unobservable_not_timeout() {
        let mut pool = Pool::with_factory(|_| Ok(Box::new(TestMachine::new())));
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::UntilUiSettled,
            boot_timeout: VTime::from_ms(50),
            ..StartArgs::default()
        };
        let out = start_on(&mut pool, &args).expect("an unobservable marker is reported");
        assert_eq!(out.json["boot"]["status"], "unobservable");
        assert_eq!(out.json["vt_us"], 50_000);
    }

    #[test]
    fn a_powered_off_start_skips_the_boot_and_stays_powered_off() {
        let mut pool = Pool::with_factory(|_| Ok(Box::new(TestMachine::new())));
        let args = StartArgs {
            fw: "official".to_owned(),
            power_on: false,
            ..StartArgs::default()
        };
        let out = start_on(&mut pool, &args).expect("a cold instance starts");
        assert_eq!(out.json["state"], "powered_off");
        assert_eq!(out.json["boot"]["status"], "skipped");
        assert_eq!(out.json["vt_us"], 0);
    }

    #[test]
    fn instance_ids_count_up_and_are_never_reused() {
        let mut pool = Pool::with_factory(|_| Ok(Box::new(TestMachine::new())));
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let first = pool.create(&args).expect("p1");
        let second = pool.create(&args).expect("p2");
        assert_eq!(first.to_string(), "p1");
        assert_eq!(second.to_string(), "p2");
        pool.destroy(first).expect("p1 is live");
        let third = pool.create(&args).expect("p3");
        assert_eq!(third.to_string(), "p3");
    }

    #[test]
    fn the_arguments_follow_the_input_schema() {
        let args = StartArgs::from_json(&serde_json::json!({
            "fw": "official",
            "label": "smoke",
            "power": "off",
            "mode": "realtime",
            "seed": 7,
            "boot": "none",
            "boot_timeout": "1.5s",
            "profile": "device",
        }))
        .expect("every field is in the schema");
        assert_eq!(args.fw, "official");
        assert_eq!(args.label, "smoke");
        assert!(!args.power_on);
        assert_eq!(args.mode, ClockMode::Realtime);
        assert_eq!(args.seed, 7);
        assert_eq!(args.boot, Boot::None);
        assert_eq!(args.boot_timeout, VTime::from_ms(1_500));
        assert_eq!(args.profile, TimingProfileId::Device);
    }

    /// Both words, so a receipt that defaulted to the right one would not pass.
    #[test]
    fn a_run_reports_the_timing_profile_its_machine_was_built_on() {
        use pemu_loader::bundle::FlashImage;
        use pemu_loader::efuse_image::EfuseImage;
        use pemu_machine::config::{Assets, MachineConfig};

        for (profile, word) in [
            (TimingProfileId::Fast, "fast"),
            (TimingProfileId::Device, "device"),
        ] {
            let assets =
                Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                    .expect("the bundled ROM is pinned");
            let machine = pemu_machine::Machine::new(
                MachineConfig {
                    profile,
                    ..MachineConfig::default()
                },
                assets,
            )
            .expect("the ROM fits the ROM window");
            let mut pool = Pool::new();
            let id = pool.attach(
                &StartArgs {
                    fw: "official".to_owned(),
                    profile,
                    boot: Boot::None,
                    ..StartArgs::default()
                },
                Box::new(machine),
            );
            let session = pool.session_mut(id).expect("attached");
            session
                .machine()
                .run(pemu_machine::run::RunLimits::insns(20_000));
            let receipt = session.receipt();
            assert_eq!(receipt.profile, word, "{profile:?}");
            assert!(
                receipt.one_line().contains(&format!("profile {word}")),
                "the one line says it too: {}",
                receipt.one_line()
            );
            assert_eq!(receipt.to_json()["profile"], word);
        }
    }

    /// The secret store knows nothing about this instance, so only the backend's own answer can
    /// make the field `true`.
    #[test]
    fn a_receipt_reports_the_taint_its_backend_reports() {
        // Attaching writes the process secret store, so this holds the shared world lock.
        let _world = crate::commands::snapshot::tests::world();
        let (mut pool, id) = started(TestMachine::new());
        let clean = pool.session_mut(id).expect("attached").receipt();
        assert!(!clean.tainted, "a scripted backend loaded no secret");
        assert_eq!(clean.to_json()["tainted"], false);

        let (mut pool, id) = started(TestMachine::new().built_on_a_secret_input());
        let receipt = pool.session_mut(id).expect("attached").receipt();
        assert!(
            receipt.tainted,
            "the backend's taint reaches the instance's receipt"
        );
        assert_eq!(receipt.to_json()["tainted"], true);
        assert!(
            receipt.one_line().contains("tainted"),
            "the one line says it too: {}",
            receipt.one_line()
        );
    }

    /// Both refusals come before a path reaches the host.
    #[test]
    fn a_tainted_load_needs_a_confirmation_and_a_process_that_answers_one() {
        let _world = crate::commands::snapshot::tests::world();
        let dump = serde_json::json!({"fw": "official", "efuse_dump": "/tmp/dump"});
        let refused = StartArgs::from_json(&dump).expect_err("no confirmation");
        assert_eq!(refused.code, E_SECRET_REFUSED);
        assert!(
            refused
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("confirm")),
            "{:?}",
            refused.hint
        );
        let confirmed = serde_json::json!({
            "fw": "official",
            "efuse_dump": "/tmp/dump",
            "confirm": "a person typed this",
        });
        let args = StartArgs::from_json(&confirmed).expect("a confirmed dump parses");
        assert_eq!(args.efuse_dump.as_deref(), Some("/tmp/dump"));

        // A confirmation is not a transport: with no allowance, which is every daemon and mount,
        // the handler refuses before asking for a machine.
        let spec = crate::registry::find("start").expect("start is registered");
        refuse_tainted_loads();
        let over_a_transport = (spec.handler)(&mut crate::spec::HandlerCx {}, confirmed.clone())
            .expect_err("a transport does not answer a tainted load");
        assert_eq!(over_a_transport.code, E_SECRET_REFUSED);
        assert!(
            over_a_transport.message.contains("tainted"),
            "{}",
            over_a_transport.message
        );

        // With the allowance, what refuses is the missing firmware source, further down.
        allow_tainted_loads();
        let at_a_terminal = (spec.handler)(&mut crate::spec::HandlerCx {}, confirmed)
            .expect_err("this test installs no machine factory");
        assert_ne!(
            at_a_terminal.code, E_SECRET_REFUSED,
            "the tainted load passed the gate: {}",
            at_a_terminal.message
        );
        refuse_tainted_loads();

        // `confirm` without a dump is a usage error rather than a silent acceptance.
        let stray = StartArgs::from_json(&serde_json::json!({"fw": "official", "confirm": "x"}))
            .expect_err("nothing to confirm");
        assert_eq!(stray.code, E_USAGE);
    }

    /// Either gate alone is not the rule, so both are asserted.
    #[test]
    fn allow_tainted_needs_a_confirmation_and_a_process_that_answers_one() {
        let _world = crate::commands::snapshot::tests::world();
        let asked = serde_json::json!({"fw": "/tmp/backup.bin", "allow_tainted": true});
        let refused = StartArgs::from_json(&asked).expect_err("no confirmation");
        assert_eq!(refused.code, E_SECRET_REFUSED);
        assert!(
            refused
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("confirm")),
            "{:?}",
            refused.hint
        );
        let confirmed = serde_json::json!({
            "fw": "/tmp/backup.bin",
            "allow_tainted": true,
            "confirm": "a person typed this",
        });
        let args = StartArgs::from_json(&confirmed).expect("a confirmed load parses");
        assert!(args.allow_tainted);

        let spec = crate::registry::find("start").expect("start is registered");
        refuse_tainted_loads();
        let over_a_transport = (spec.handler)(&mut crate::spec::HandlerCx {}, confirmed.clone())
            .expect_err("a transport does not answer a tainted load");
        assert_eq!(over_a_transport.code, E_SECRET_REFUSED);
        assert!(
            over_a_transport.message.contains("tainted"),
            "{}",
            over_a_transport.message
        );

        // With the allowance, what refuses is the missing firmware source, further down.
        allow_tainted_loads();
        let at_a_terminal = (spec.handler)(&mut crate::spec::HandlerCx {}, confirmed)
            .expect_err("this test installs no machine factory");
        assert_ne!(
            at_a_terminal.code, E_SECRET_REFUSED,
            "the tainted load passed the gate: {}",
            at_a_terminal.message
        );
        refuse_tainted_loads();

        // `allow_tainted: false` asks for no confirmation.
        let plain =
            StartArgs::from_json(&serde_json::json!({"fw": "official", "allow_tainted": false}))
                .expect("an untainted start parses");
        assert!(!plain.allow_tainted);
    }

    #[test]
    fn start_selects_the_timing_profile_and_refuses_an_unknown_name() {
        let args = StartArgs::from_json(&serde_json::json!({
            "fw": "official",
            "profile": "device",
        }))
        .expect("`device` is a profile");
        assert_eq!(args.profile, TimingProfileId::Device);
        assert_eq!(
            StartArgs::from_json(&serde_json::json!({"fw": "official"}))
                .expect("the default")
                .profile,
            TimingProfileId::Fast,
        );

        let error = StartArgs::from_json(&serde_json::json!({
            "fw": "official",
            "profile": "slow",
        }))
        .expect_err("`slow` is no profile");
        assert_eq!(error.code, E_USAGE);
        assert!(error.message.contains("slow"), "{}", error.message);
        assert!(error.message.contains("device"), "{}", error.message);
        // `pemu-wasm` parses through the same vocabulary.
        assert!(TimingProfileId::parse("slow").is_none());
    }

    #[test]
    fn malformed_arguments_are_usage() {
        let cases = [
            serde_json::json!([]),
            serde_json::json!({ "fw": 7 }),
            serde_json::json!({ "fw": "official", "power": "maybe" }),
            serde_json::json!({ "fw": "official", "mode": "fast" }),
            serde_json::json!({ "fw": "official", "boot": "until_led" }),
            serde_json::json!({ "fw": "official", "profile": "slow" }),
            serde_json::json!({ "fw": "official", "boot_timeout": ".5s" }),
            serde_json::json!({ "fw": "official", "nonsense": 1 }),
        ];
        for case in cases {
            let error = StartArgs::from_json(&case).expect_err("outside the schema");
            assert_eq!(error.code, E_USAGE, "{case}");
        }
    }

    /// Nothing is resolved here; a host that cannot answer the name refuses with `E_ASSET_MISSING`.
    #[test]
    fn no_firmware_argument_is_the_bundled_demo() {
        let default = StartArgs::from_json(&serde_json::json!({})).expect("no `fw` is allowed");
        assert_eq!(default.fw, DEMO_FW);
        assert_eq!(default.fw, "official", "the corpus id of the demo");
        let named = StartArgs::from_json(&serde_json::json!({ "fw": "pk" })).expect("a name");
        assert_eq!(named.fw, "pk", "a named firmware still wins");
        let schema = serde_json::to_value(input_schema()).expect("the input schema");
        assert!(
            schema.get("required").is_none(),
            "`fw` is no longer a required property: {schema}"
        );
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("start").expect("#[command] registered start");
        for example in spec.examples {
            let args = example.args_json().expect("an example is JSON");
            StartArgs::from_json(&args).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
    }

    #[test]
    fn the_command_is_registered_with_its_shape() {
        let spec = crate::registry::find("start").expect("#[command] registered start");
        assert_eq!(spec.group, crate::spec::CapsGroup::Core);
        assert!(!spec.annotations.needs_instance && !spec.annotations.read_only);
        assert_eq!(spec.cli.positional, ["fw"]);
        assert!(spec.errors.contains(&E_ASSET_MISSING) && spec.errors.contains(&E_TIMEOUT));
        assert_eq!(spec.examples.len(), 2);
    }

    #[test]
    fn a_speed_is_a_factor_or_max() {
        assert_eq!(
            Speed::parse(&serde_json::json!("max")).expect("max"),
            Speed::Max
        );
        assert_eq!(
            Speed::parse(&serde_json::json!(2.5)).expect("2.5x"),
            Speed::Milli(2_500)
        );
        assert_eq!(Speed::Milli(2_500).as_text(), "2.500x");
        for bad in [
            serde_json::json!(0.01),
            serde_json::json!(65),
            serde_json::json!("fast"),
        ] {
            assert_eq!(
                Speed::parse(&bad).expect_err("outside the range").code,
                E_USAGE
            );
        }
    }

    /// Every example of the core group, run against a scripted machine. Commands come from the
    /// registry, so one not dispatched here fails rather than being skipped, and every example must
    /// succeed.
    #[test]
    fn every_core_example_runs_against_a_scripted_instance() {
        use crate::commands::{clock, input, run, serial, status, stop};
        use crate::spec::CapsGroup;

        let mut ran = 0;
        for spec in crate::registry::commands() {
            // Commands whose examples run elsewhere: `doctor` in its own tests, the rest through
            // `agent_budget::tests::every_seam_command_example_runs_against_the_scripted_instance`.
            // A core command on neither list fails.
            const ELSEWHERE: &[&str] = &[
                "doctor",
                "env",
                "snapshot",
                "ui",
                "screenshot",
                "inspect",
                "scenario",
            ];
            if spec.group != CapsGroup::Core || ELSEWHERE.contains(&spec.name) {
                continue;
            }
            for example in spec.examples {
                let json = example.args_json().expect("an example is JSON");
                let (mut pool, id) = started(
                    TestMachine::new()
                        .line(5, "I (5) main_task: Calling app_main()")
                        .line(9, "I (9) pk_app: ready")
                        .event(12, EventKind::UiSettled),
                );
                let outcome = match spec.name {
                    "start" => {
                        let mut pool = Pool::with_factory(|_| Ok(Box::new(TestMachine::new())));
                        let args = StartArgs::from_json(&json).expect("the example parses");
                        start_on(&mut pool, &args).map(|_| ())
                    }
                    "stop" => {
                        let args = stop::StopArgs::from_json(&json).expect("the example parses");
                        stop::stop_on(&mut pool, id, &args).map(|_| ())
                    }
                    "status" => {
                        let args =
                            status::StatusArgs::from_json(&json).expect("the example parses");
                        // The example names `p1`, which is the id `started` minted.
                        status::status_on(&mut pool, &args).map(|_| ())
                    }
                    "run" => {
                        let args = run::RunArgs::from_json(&json).expect("the example parses");
                        let session = pool.session_mut(id).expect("the scripted instance");
                        run::run_on(session, &args).map(|_| ())
                    }
                    "serial" => {
                        let args =
                            serial::SerialArgs::from_json(&json).expect("the example parses");
                        let session = pool.session_mut(id).expect("the scripted instance");
                        serial::serial_on(session, &args).map(|_| ())
                    }
                    "input" => {
                        let args = input::InputArgs::from_json(&json).expect("the example parses");
                        let session = pool.session_mut(id).expect("the scripted instance");
                        input::input_on(session, &args, true).map(|_| ())
                    }
                    "clock" => {
                        let args = clock::ClockArgs::from_json(&json).expect("the example parses");
                        clock::clock_on(&mut pool, id, &args).map(|_| ())
                    }
                    other => panic!(
                        "`{other}` is a core command with no case here: add one, or say in this \
                         test where its examples run instead"
                    ),
                };
                outcome.unwrap_or_else(|error| {
                    panic!(
                        "{}: the documented example {:?} does not work: {error:?}",
                        spec.name, example.title
                    )
                });
                ran += 1;
            }
        }
        assert!(ran >= 14, "every core command registers examples: {ran}");
    }

    #[test]
    fn a_boot_that_parks_before_its_marker_is_e_deadlock_with_the_envelope() {
        let mut pool = Pool::with_factory(|_| {
            Ok(Box::new(
                TestMachine::new()
                    .line(5, "boot: parking")
                    .waits_for_input_at(30),
            ))
        });
        let args = StartArgs::from_json(&serde_json::json!({
            "fw": "official",
            "boot": "until_app_main",
        }))
        .expect("inside the schema");
        let error = start_on(&mut pool, &args).expect_err("the guest parked");
        assert_eq!(error.code, E_DEADLOCK);
        assert_eq!(error.vt_us, 30_000);
        assert!(error.detail.get("fault").is_some(), "{}", error.detail);
        assert!(
            error.detail.get("wake_inputs").is_some(),
            "{}",
            error.detail
        );
        assert!(
            pool.live_ids().is_empty(),
            "a refused start leaves no instance"
        );
    }

    #[test]
    fn a_deadlock_is_e_deadlock_and_the_session_records_the_wait() {
        let deadlock = fault_of(&StopReason::Deadlock).expect("a deadlock is reported");
        assert_eq!(deadlock.code, crate::error::E_DEADLOCK);
        assert!(fault_of(&StopReason::MaxInsns).is_none());
        assert!(fault_of(&StopReason::Until).is_none());
        let (mut pool, id) = started(TestMachine::new().waits_for_input_at(40));
        let session = pool.session_mut(id).expect("live");
        let outcome = session.run_until(VTime::from_ms(100));
        assert_eq!(outcome.reason, StopReason::Deadlock);
        assert_eq!(outcome.vt, VTime::from_ms(40));
        assert!(session.waiting_for_input);
        session
            .machine()
            .input(NOW, InputEvent::UsbClient { open: true })
            .expect("journaled");
        let outcome = session.run_until(VTime::from_ms(100));
        assert_eq!(outcome.reason, StopReason::Until);
        assert!(!session.waiting_for_input, "a run that moved on clears it");
    }

    #[test]
    fn a_firmware_path_is_shown_by_its_file_name_only() {
        assert_eq!(fw_display("official"), "official");
        assert_eq!(
            fw_display("/Users/someone/fw/pk-merged.bin"),
            "pk-merged.bin"
        );
        assert_eq!(fw_display("C:\\data\\pk.bin"), "pk.bin");
        assert_eq!(fw_display("rel/dir/"), "<path>");
        let mut pool = Pool::new();
        let args = StartArgs {
            fw: "/private/tmp/secret-dir/pk.bin".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&args, Box::new(TestMachine::new()));
        assert_eq!(pool.session(id).expect("live").fw, "pk.bin");
    }

    /// A reset stop is one the caller asked for.
    #[test]
    fn a_reset_stop_is_not_a_fault() {
        use pemu_core::reset::{ResetCause, ResetKind};
        let kind = ResetKind::of(ResetCause::RTC_SW_SYS).expect("0x03 is a reset cause");
        assert!(fault_of(&StopReason::ChipReset(kind)).is_none());
        assert!(fault_of(&StopReason::Matcher(pemu_machine::stops::MatcherId(1))).is_none());
    }

    /// `status` cannot fail and `start` cannot succeed without a factory, whatever else the pool
    /// holds.
    #[test]
    fn the_registered_handlers_reach_the_process_pool() {
        let out = (crate::registry::find("status").expect("status").handler)(
            &mut HandlerCx {},
            serde_json::json!({}),
        )
        .expect("`status` answers even with no instance");
        assert!(out.json["instances"].is_array());

        let error = (crate::registry::find("start").expect("start").handler)(
            &mut HandlerCx {},
            serde_json::json!({ "fw": "official" }),
        )
        .expect_err("this build has no emulator core");
        assert_eq!(error.code, E_INTERNAL);
    }
}
