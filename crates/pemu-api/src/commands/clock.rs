//! `passportsim clock`: show or take the clock lease, set the pacing, and step instructions.
//!
//! Exactly one party drives an instance's virtual time. `take` and `release` are that lease;
//! `pause`, `resume`, `set_speed` and `set_mode` change the pacing; `step` runs a fixed number of
//! instructions, the one operation here that advances time.
//!
//! In deterministic mode, the agent default, `pause` is a no-op because time only moves inside a
//! call, and `resume` is `E_STATE` naming `run`.
//!
//! `clock` is not annotated `advances_time`: that would gate every call on a runnable lifecycle,
//! and `clock status` must answer for a `powered_off` or `faulted` instance too. `step` makes the
//! check itself.

use std::fmt::Write as _;

use crate::error::{
    ApiError, E_DEADLOCK, E_GUEST_PANIC, E_HLE, E_INTERNAL, E_LEASE, E_STATE, E_STUCK, E_TRIPWIRE,
    E_UNMODELED, E_USAGE,
};
use crate::instance::{InstanceId, Lifecycle};
use crate::lease::{Lease, LeaseHolder};
use crate::output::Output;
use crate::registry::command;
use crate::spec::{Annotations, HandlerCx, Schema};

use crate::args::{instance_schema, object, only, opt_bool, opt_str, opt_u64, usage};
use crate::pool::{Pool, with_pool};
use crate::session::{ClockMode, Session, Speed};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Status,
    /// At an instruction boundary.
    Pause,
    /// In realtime mode.
    Resume,
    SetSpeed,
    SetMode,
    Take,
    Release,
    Step,
}

impl Op {
    pub const fn as_str(self) -> &'static str {
        match self {
            Op::Status => "status",
            Op::Pause => "pause",
            Op::Resume => "resume",
            Op::SetSpeed => "set_speed",
            Op::SetMode => "set_mode",
            Op::Take => "take",
            Op::Release => "release",
            Op::Step => "step",
        }
    }

    pub fn parse(text: &str) -> Option<Op> {
        Op::ALL.iter().copied().find(|op| op.as_str() == text)
    }

    /// In grammar order.
    pub const ALL: [Op; 8] = [
        Op::Status,
        Op::Pause,
        Op::Resume,
        Op::SetSpeed,
        Op::SetMode,
        Op::Take,
        Op::Release,
        Op::Step,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub op: Op,
    /// For `set_speed`.
    pub speed: Option<Speed>,
    /// For `set_mode`.
    pub mode: Option<ClockMode>,
    /// Skip idle time to the next scheduled event.
    pub idle_skip: Option<bool>,
    /// For `take`.
    pub owner: LeaseHolder,
    /// Take the lease over from its current holder.
    pub force: bool,
    /// For `step`.
    pub insns: u64,
}

impl ClockArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<ClockArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "op",
                "speed",
                "mode",
                "idle_skip",
                "owner",
                "force",
                "insns",
            ],
        )?;
        let op = match opt_str(args, "op")? {
            None => return Err(usage("op", "is required")),
            Some(text) => Op::parse(text)
                .ok_or_else(|| usage("op", &format!("`{text}` is not a clock operation")))?,
        };
        let speed = match args.get("speed") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(Speed::parse(value)?),
        };
        let mode = match opt_str(args, "mode")? {
            None => None,
            Some(text) => Some(
                ClockMode::parse(text)
                    .ok_or_else(|| usage("mode", "expected `deterministic` or `realtime`"))?,
            ),
        };
        let owner = match opt_str(args, "owner")? {
            None => LeaseHolder::Agent,
            Some(text) => LeaseHolder::parse(text)
                .ok_or_else(|| usage("owner", &format!("`{text}` is not a lease holder")))?,
        };
        if op == Op::SetSpeed && speed.is_none() {
            return Err(usage("speed", "`set_speed` needs a `speed`"));
        }
        if op == Op::SetMode && mode.is_none() {
            return Err(usage("mode", "`set_mode` needs a `mode`"));
        }
        let force = opt_bool(args, "force")?.unwrap_or(false);
        let insns = opt_u64(args, "insns")?.unwrap_or(0);
        if op == Op::Step && insns == 0 {
            return Err(usage("insns", "`step` needs a positive `insns`"));
        }
        Ok(ClockArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            op,
            speed,
            mode,
            idle_skip: opt_bool(args, "idle_skip")?,
            owner,
            force,
            insns,
        })
    }
}

/// The pacing of an instance with a live bridge: `Wall { rate: 1 }`, which is `realtime` at 1.000x.
pub const BRIDGE_MODE: ClockMode = ClockMode::Realtime;
pub const BRIDGE_SPEED: Speed = Speed::Milli(1_000);

/// A bridged peer answers in host time, so virtual time runs at host time to agree with it. Any
/// other pacing looks to the guest like a protocol failure: under `max` a 300 ms answer arrives
/// after seconds of virtual time and the host stack's 2,000 ms HCI command timeout fires; under
/// `deterministic` virtual time stops between calls while the peer keeps talking. Both are
/// `E_LEASE` naming the bridge.
pub fn bridge_holds_the_clock(bridged: u32, what: &str) -> Result<(), ApiError> {
    if bridged == 0 {
        return Ok(());
    }
    Err(ApiError::new(
        E_LEASE,
        format!(
            "{bridged} live bridge(s) hold the clock: a bridged peer answers in host time, so \
             the instance runs at realtime 1.000x and nothing may {what} it"
        ),
    )
    .retryable()
    .with_hint("detach the bridge first, or leave the pacing at `realtime` 1.000x"))
}

/// 0 when it has no session. `Machine::live_bridges` sums every bound module, so this covers the
/// BLE and Wi-Fi bridges alike, and a third radio joins with no change here.
pub fn live_bridges_of(pool: &mut Pool, id: InstanceId) -> u32 {
    pool.session_mut(id)
        .map(|session| session.machine().live_bridges())
        .unwrap_or(0)
}

/// For a surface outside this command: `endpoint --clock agent` takes the clock the same way
/// `set_speed` and `pause` do.
pub fn refuse_while_bridged(pool: &mut Pool, id: InstanceId, what: &str) -> Result<(), ApiError> {
    bridge_holds_the_clock(live_bridges_of(pool, id), what)
}

pub fn clock_on(pool: &mut Pool, id: InstanceId, args: &ClockArgs) -> Result<Output, ApiError> {
    let now = pool
        .session(id)
        .map(Session::now)
        .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
    let lifecycle = pool
        .table()
        .get(id)
        .map_or(Lifecycle::Stopped, |state| state.lifecycle);

    // A live bridge's pacing is not the caller's to choose. The machine is the source of truth
    // because the bridge went up through the journal, so a restored or replayed instance knows it
    // too.
    let bridged = live_bridges_of(pool, id);

    match args.op {
        Op::Status => {}
        Op::Pause => {
            let lease = lease_of(pool, id)?;
            lease.check_pause(false)?;
            bridge_holds_the_clock(bridged, "pause")?;
        }
        Op::Resume => {
            let mode = pool.session(id).map(|s| s.mode);
            if mode == Some(ClockMode::Deterministic) {
                return Err(ApiError::new(
                    E_STATE,
                    "a deterministic instance has no clock to resume: time only moves inside a call",
                )
                .with_hint("`run --for 250ms` advances virtual time"));
            }
        }
        Op::SetSpeed => {
            if args.speed.is_some_and(|speed| speed != BRIDGE_SPEED) {
                bridge_holds_the_clock(bridged, "change the speed of")?;
            }
            if let (Some(session), Some(speed)) = (pool.session_mut(id), args.speed) {
                session.speed = speed;
            }
        }
        Op::SetMode => {
            let Some(mode) = args.mode else {
                return Err(usage("mode", "`set_mode` needs a `mode`"));
            };
            if mode != BRIDGE_MODE {
                bridge_holds_the_clock(bridged, "change the mode of")?;
            }
            // A mode switch needs the lease.
            let holder = lease_of(pool, id)?.holder(now);
            if let Some(other) = holder
                && other != args.owner
            {
                return Err(taken(other));
            }
            if let Some(session) = pool.session_mut(id) {
                session.mode = mode;
                if mode == ClockMode::Realtime {
                    session.deterministic_so_far = false;
                }
            }
        }
        Op::Take => {
            let ticket = {
                let lease = lease_mut(pool, id)?;
                if args.force {
                    lease.force_acquire(args.owner, now, Some(Lease::DEFAULT_TTL))
                } else {
                    lease.acquire(args.owner, now, Some(Lease::DEFAULT_TTL))?
                }
            };
            if let Some(session) = pool.session_mut(id) {
                session.ticket = Some(ticket);
            }
        }
        Op::Release => {
            let ticket = pool
                .session(id)
                .and_then(|session| session.ticket)
                .ok_or_else(|| {
                    ApiError::new(E_LEASE, "this session holds no clock lease")
                        .with_hint("`clock take` acquires one")
                })?;
            lease_mut(pool, id)?.release(ticket, now)?;
            if let Some(session) = pool.session_mut(id) {
                session.ticket = None;
            }
        }
        Op::Step => {
            if !lifecycle.accepts_time_advance() {
                return Err(ApiError::new(
                    E_STATE,
                    format!(
                        "instance `{id}` is `{}`, so virtual time cannot advance",
                        lifecycle.as_str()
                    ),
                ));
            }
            // The lease gate `advances_time` would give every call, applied to the one operation
            // that advances time.
            let annotations = Annotations {
                advances_time: true,
                needs_instance: true,
                ..Annotations::EMPTY
            };
            lease_of(pool, id)?.check_call(args.owner, annotations, now)?;
            let session = pool
                .session_mut(id)
                .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
            let outcome = session.run_insns(args.insns);
            if let Some(error) = crate::session::fault_of(&outcome.reason) {
                // A deadlock carries its envelope, as `run` reports it.
                let error = error.at_vt_us(outcome.vt.as_us());
                return Err(crate::commands::inspect::deadlock_envelope(session, error));
            }
            // So does the task-level one this step's slice found.
            if let Some(error) = crate::session::task_deadlock_fault(session) {
                return Err(crate::commands::inspect::deadlock_envelope(session, error));
            }
        }
    }

    if let Some(skip) = args.idle_skip
        && let Some(session) = pool.session_mut(id)
    {
        session.idle_skip = skip;
    }
    report(pool, id, args.op)
}

fn lease_of(pool: &Pool, id: InstanceId) -> Result<&Lease, ApiError> {
    pool.table()
        .get(id)
        .map(|state| &state.lease)
        .ok_or_else(|| ApiError::new(E_STATE, format!("no instance `{id}`")))
}

fn lease_mut(pool: &mut Pool, id: InstanceId) -> Result<&mut Lease, ApiError> {
    pool.table_mut()
        .get_mut(id)
        .map(|state| &mut state.lease)
        .ok_or_else(|| ApiError::new(E_STATE, format!("no instance `{id}`")))
}

fn taken(holder: LeaseHolder) -> ApiError {
    ApiError::new(
        E_LEASE,
        format!("the clock lease is held by `{}`", holder.as_str()),
    )
    .retryable()
    .with_hint("`clock take --force` takes it over")
}

/// As it stands after the operation.
fn report(pool: &mut Pool, id: InstanceId, op: Op) -> Result<Output, ApiError> {
    let lifecycle = pool
        .table()
        .get(id)
        .map_or(Lifecycle::Stopped, |state| state.lifecycle);
    let now = pool
        .session(id)
        .map(Session::now)
        .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
    let owner = lease_of(pool, id)?
        .holder(now)
        .map_or("none", LeaseHolder::as_str);
    let session = pool
        .session_mut(id)
        .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
    let receipt = session.receipt();
    let bridged = session.machine().live_bridges();
    // While a bridge is live the pacing is `Wall { rate: 1 }` whatever it was set to before.
    let (mode, speed) = match bridged {
        0 => (session.mode, session.speed),
        _ => (BRIDGE_MODE, BRIDGE_SPEED),
    };
    let json = serde_json::json!({
        "instance": id.to_string(),
        "op": op.as_str(),
        "state": lifecycle.as_str(),
        "mode": mode.as_str(),
        "speed": speed.to_json(),
        "live_bridges": bridged,
        "idle_skip": session.idle_skip,
        "owner": owner,
        "vt_us": receipt.vt_us,
        "insns": receipt.insns,
        "deterministic_so_far": session.deterministic_so_far,
    });
    let mut text = String::new();
    let _ = write!(
        text,
        "{id} {} clock={} speed={} idle_skip={} owner={} vt={}us",
        lifecycle.as_str(),
        mode.as_str(),
        speed.as_text(),
        session.idle_skip,
        owner,
        receipt.vt_us
    );
    Ok(Output::new(json, text, receipt))
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["op"],
        "description": "`clock` arguments.",
        "properties": {
            "instance": instance_schema(),
            "op": { "type": "string", "enum": ["status", "pause", "resume", "set_speed", "set_mode", "take", "release", "step"], "description": "What to do." },
            "speed": {
                "description": "Pacing factor, or `max`.",
                "oneOf": [ { "type": "number", "minimum": 0.05, "maximum": 64 }, { "const": "max" } ]
            },
            "mode": { "type": "string", "enum": ["deterministic", "realtime"], "description": "Pacing mode." },
            "idle_skip": { "type": "boolean", "description": "Skip idle time." },
            "owner": { "type": "string", "enum": ["agent", "ui", "endpoint", "scenario"], "description": "Lease holder (agent)." },
            "force": { "type": "boolean", "description": "Take the lease over (false)." },
            "insns": { "type": "integer", "minimum": 0, "description": "`step` instructions." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "op": { "type": "string" },
            "state": { "type": "string" },
            "mode": { "type": "string" },
            "speed": {},
            "idle_skip": { "type": "boolean" },
            "owner": { "type": "string" },
            "vt_us": { "type": "integer" },
            "insns": { "type": "integer" },
            "deterministic_so_far": { "type": "boolean" },
            "live_bridges": { "type": "integer" }
        }
    })
}

/// Show or take the clock lease, pace it, or step instructions.
#[command(
    api_crate = crate,
    name = "clock",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance, idempotent),
    cli(positional = ["op"]),
    errors(E_USAGE, E_STATE, E_LEASE, E_GUEST_PANIC, E_DEADLOCK, E_STUCK, E_TRIPWIRE, E_UNMODELED, E_HLE, E_INTERNAL),
    example(
        title = "Show the clock and who holds its lease",
        args = r#"{"op":"status"}"#,
    ),
    example(
        title = "Take the lease before driving the instance",
        args = r#"{"op":"take","owner":"agent"}"#,
    ),
    example(
        title = "Step a thousand instructions",
        args = r#"{"op":"step","insns":1000}"#,
    ),
)]
pub fn clock(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = ClockArgs::from_json(&args)?;
    with_pool(|pool| {
        let id = pool.bind(SPEC_CLOCK.annotations, args.instance.as_deref())?;
        clock_on(pool, id, &args)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_core::time::VTime;

    use crate::commands::start::tests::{TestMachine, started};

    fn args(op: Op) -> ClockArgs {
        ClockArgs {
            instance: None,
            op,
            speed: None,
            mode: None,
            idle_skip: None,
            owner: LeaseHolder::Agent,
            force: false,
            insns: 0,
        }
    }

    #[test]
    fn status_reports_the_pacing_and_a_free_lease() {
        let (mut pool, id) = started(TestMachine::new());
        let out = clock_on(&mut pool, id, &args(Op::Status)).expect("status always answers");
        assert_eq!(out.json["op"], "status");
        assert_eq!(out.json["mode"], "deterministic");
        assert_eq!(out.json["owner"], "none");
        assert_eq!(out.json["idle_skip"], true);
        assert_eq!(out.json["speed"], 1.0);
        assert!(out.text.contains("speed=1.000x"), "{}", out.text);
    }

    #[test]
    fn taking_the_lease_names_the_holder_and_refuses_a_second_party() {
        let (mut pool, id) = started(TestMachine::new());
        clock_on(&mut pool, id, &args(Op::Take)).expect("the lease is free");
        let out = clock_on(&mut pool, id, &args(Op::Status)).expect("status");
        assert_eq!(out.json["owner"], "agent");

        let mut ui = args(Op::Take);
        ui.owner = LeaseHolder::Ui;
        let error = clock_on(&mut pool, id, &ui).expect_err("the agent holds it");
        assert_eq!(error.code, E_LEASE);
        assert!(error.retryable);
        assert!(error.message.contains("agent"), "{}", error.message);

        ui.force = true;
        let out = clock_on(&mut pool, id, &ui).expect("a forced take-over");
        assert_eq!(out.json["owner"], "ui");
    }

    #[test]
    fn releasing_without_holding_the_lease_is_a_lease_error() {
        let (mut pool, id) = started(TestMachine::new());
        let error = clock_on(&mut pool, id, &args(Op::Release)).expect_err("nothing to release");
        assert_eq!(error.code, E_LEASE);
        clock_on(&mut pool, id, &args(Op::Take)).expect("the lease is free");
        let out =
            clock_on(&mut pool, id, &args(Op::Release)).expect("the ticket is this session's");
        assert_eq!(out.json["owner"], "none");
    }

    #[test]
    fn resume_in_deterministic_mode_names_the_command_that_advances_time() {
        let (mut pool, id) = started(TestMachine::new());
        let error = clock_on(&mut pool, id, &args(Op::Resume)).expect_err("nothing to resume");
        assert_eq!(error.code, E_STATE);
        assert!(error.hint.as_deref().unwrap_or_default().contains("run"));
    }

    #[test]
    fn set_mode_to_realtime_marks_the_run_no_longer_deterministic() {
        let (mut pool, id) = started(TestMachine::new());
        let mut set = args(Op::SetMode);
        set.mode = Some(ClockMode::Realtime);
        let out = clock_on(&mut pool, id, &set).expect("a free lease allows the switch");
        assert_eq!(out.json["mode"], "realtime");
        assert_eq!(out.json["deterministic_so_far"], false);
    }

    #[test]
    fn set_speed_and_idle_skip_are_reported_back() {
        let (mut pool, id) = started(TestMachine::new());
        let mut set = args(Op::SetSpeed);
        set.speed = Some(Speed::Max);
        set.idle_skip = Some(false);
        let out = clock_on(&mut pool, id, &set).expect("pacing is the caller's choice");
        assert_eq!(out.json["speed"], "max");
        assert_eq!(out.json["idle_skip"], false);
    }

    #[test]
    fn step_runs_instructions_and_advances_virtual_time() {
        let (mut pool, id) = started(TestMachine::new());
        let mut step = args(Op::Step);
        step.insns = 16_000;
        let out = clock_on(&mut pool, id, &step).expect("a paused instance may step");
        assert!(out.json["vt_us"].as_u64().expect("vt") > 0);
        assert!(out.json["insns"].as_u64().expect("insns") >= 16_000);
    }

    #[test]
    fn step_into_a_deadlock_is_e_deadlock_with_the_task_table() {
        let _world = crate::commands::inspect::tests::world();
        crate::commands::inspect::set_introspectors(crate::commands::inspect::tests::SCRIPTED);
        let (mut pool, id) = started(TestMachine::new().waits_for_input_at(1));
        let mut step = args(Op::Step);
        step.insns = 16_000_000;
        let error = clock_on(&mut pool, id, &step).expect_err("the guest parks");
        assert_eq!(error.code, E_DEADLOCK);
        assert_eq!(error.vt_us, 1_000, "where it parked");
        assert_eq!(error.detail["fault"], "deadlock");
        assert_eq!(
            error.detail["tasks"]["tasks"][1]["name"], "IDLE",
            "{}",
            error.detail
        );
        assert_eq!(error.detail["blocked"][0]["blocked_on"], "lvgl_port mutex");
        assert!(
            error.detail["wake_inputs"]
                .as_array()
                .is_some_and(|w| !w.is_empty())
        );
    }

    #[test]
    fn step_on_a_powered_off_instance_is_state() {
        let (mut pool, id) = started(TestMachine::new());
        pool.table_mut()
            .get_mut(id)
            .expect("the instance")
            .transition(Lifecycle::PoweredOff, VTime(0))
            .expect("paused -> powered_off");
        let mut step = args(Op::Step);
        step.insns = 10;
        let error = clock_on(&mut pool, id, &step).expect_err("no clock while the rail is down");
        assert_eq!(error.code, E_STATE);
    }

    #[test]
    fn step_is_refused_while_another_party_holds_the_lease() {
        let (mut pool, id) = started(TestMachine::new());
        let mut ui = args(Op::Take);
        ui.owner = LeaseHolder::Ui;
        clock_on(&mut pool, id, &ui).expect("the lease is free");
        let mut step = args(Op::Step);
        step.insns = 10;
        let error = clock_on(&mut pool, id, &step).expect_err("the UI holds the clock");
        assert_eq!(error.code, E_LEASE);
    }

    #[test]
    fn malformed_arguments_are_usage() {
        let cases = [
            serde_json::json!({}),
            serde_json::json!({ "op": "rewind" }),
            serde_json::json!({ "op": "set_speed" }),
            serde_json::json!({ "op": "set_speed", "speed": 100 }),
            serde_json::json!({ "op": "set_mode" }),
            serde_json::json!({ "op": "set_mode", "mode": "turbo" }),
            serde_json::json!({ "op": "step" }),
            serde_json::json!({ "op": "take", "owner": "nobody" }),
            serde_json::json!({ "op": "status", "nonsense": 1 }),
        ];
        for case in cases {
            let error = ClockArgs::from_json(&case).expect_err("outside the schema");
            assert_eq!(error.code, E_USAGE, "{case}");
        }
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("clock").expect("#[command] registered clock");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            ClockArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.needs_instance && spec.annotations.idempotent);
        assert!(!spec.annotations.advances_time, "see the module header");
    }
}
