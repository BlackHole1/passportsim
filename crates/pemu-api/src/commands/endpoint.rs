//! `passportsim endpoint`: open, describe or close the USB Serial/JTAG host endpoints of one
//! instance, so `esptool`, `idf.py` and a raw console reach it.
//!
//! - `--tcp` binds 127.0.0.1 on an auto-assigned port: `rfc2217://` flashes and resets, `socket://`
//!   monitors. `--auto-download` resets a raw esptool client's instance into download mode on its
//!   first `SYNC`.
//! - `--pty` opens a macOS pseudo-terminal, data only; elsewhere it is `E_HOST_UNSUPPORTED` with
//!   the TCP form as the hint.
//! - `--close` stops the endpoints, hands the instance back and reports its final virtual time.
//!
//! The clock: by default (`--clock endpoint`) the endpoint takes the lease and runs the session on
//! the host's endpoint thread, at 1x wall time while a host tool is connected (a host tool waits in
//! wall time) and idle otherwise; other calls are `E_LEASE` until `--close`. With `--clock agent`
//! it takes no lease and runs nothing: client input is journaled at the start of each slice an
//! agent call runs. Everything a client sends is journaled, so the run stays replayable.
//!
//! The endpoint itself is native work: the host installs an [`EndpointHost`] with [`set_host`], and
//! without one the command is `E_HOST_UNSUPPORTED`.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{ApiError, E_HOST_UNSUPPORTED, E_LEASE, E_STATE, E_USAGE};
use crate::host_support::{self, Host};
use crate::instance::InstanceId;
use crate::lease::LeaseHolder;
use crate::matchers::str_enum;
use crate::output::Output;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

use crate::args::{enum_of, instance_schema, object, only, opt_bool, opt_str, usage};
use crate::pool::{Pool, shared};
use crate::session::Session;

str_enum! {
    pub enum EndpointClock {
        /// Holds the lease and runs the instance in wall time while a tool is connected.
        Endpoint = "endpoint",
        /// The agent keeps the instance; client input is journaled as agent calls run it.
        Agent = "agent",
    }
}

// `str_enum!` writes the derives, so `#[default]` cannot be attached to a variant there.
#[allow(clippy::derivable_impls)]
impl Default for EndpointClock {
    fn default() -> Self {
        EndpointClock::Endpoint
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndpointArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub tcp: bool,
    pub pty: bool,
    /// TCP only.
    pub auto_download: bool,
    pub clock: EndpointClock,
    pub close: bool,
}

impl EndpointArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<EndpointArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &["instance", "tcp", "pty", "auto_download", "clock", "close"],
        )?;
        let clock = enum_of(args, "clock", EndpointClock::parse, "endpoint, agent")?;
        let out = EndpointArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            tcp: opt_bool(args, "tcp")?.unwrap_or(false),
            pty: opt_bool(args, "pty")?.unwrap_or(false),
            auto_download: opt_bool(args, "auto_download")?.unwrap_or(false),
            clock: clock.unwrap_or_default(),
            close: opt_bool(args, "close")?.unwrap_or(false),
        };
        if out.close && (out.tcp || out.pty || out.auto_download || clock.is_some()) {
            return Err(usage("close", "`close` takes no other option"));
        }
        if out.auto_download && !out.tcp {
            return Err(usage("auto_download", "`auto_download` needs `tcp`"));
        }
        if clock.is_some() && !out.opens() {
            return Err(usage(
                "clock",
                "`clock` applies to an endpoint being opened",
            ));
        }
        Ok(out)
    }

    pub fn opens(&self) -> bool {
        self.tcp || self.pty
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndpointInfo {
    /// On 127.0.0.1.
    pub tcp_port: Option<u16>,
    pub pty_path: Option<String>,
    pub pty_limitation: Option<String>,
    pub auto_download: bool,
    pub clock: EndpointClock,
    /// A TCP client, the open pty.
    pub clients: usize,
    pub connections: u64,
    /// `rfc2217`, `slip` or `console`.
    pub last_mode: Option<String>,
    pub vt_us: u64,
    /// Read back on the live thread; `None` under `--clock agent`.
    pub qos: Option<&'static str>,
    /// Times the live pacer fell behind its lag bound and re-anchored: each is a stretch the guest
    /// ran slower than real time.
    pub reanchors: u64,
}

/// With the session to return to the pool.
pub struct OpenFailed {
    /// `None` when the host lost it.
    pub session: Option<Session>,
    pub error: ApiError,
}

pub struct Opened {
    pub info: EndpointInfo,
    /// `Some` under `--clock agent`, for the caller to return to the pool; `None` when the host
    /// runs it live.
    pub session: Option<Session>,
}

pub struct Closed {
    pub clock: EndpointClock,
    /// `None` under `--clock agent` (it never left the pool) or when the host's thread panicked.
    pub session: Option<Session>,
}

/// Installed by `pemu-host`.
pub trait EndpointHost: Send + Sync {
    /// Under `--clock endpoint` the host keeps the session and runs it live; under `--clock agent`
    /// it hands it back. On failure the session comes back with the error, unless the host's thread
    /// panicked.
    fn open(&self, session: Session, args: &EndpointArgs) -> Result<Opened, Box<OpenFailed>>;
    /// `None` when none is open.
    fn close(&self, id: InstanceId) -> Option<Closed>;
    fn describe(&self, id: InstanceId) -> Option<EndpointInfo>;
}

pub fn set_host(host: Option<Arc<dyn EndpointHost>>) {
    crate::pool::with_pool(|pool| pool.set_endpoint_host(host));
}

/// As `status` reports them; `null` when none is open or no host is installed.
pub fn status_json(pool: &Pool, id: InstanceId) -> serde_json::Value {
    match pool.endpoint_host().and_then(|host| host.describe(id)) {
        Some(info) => info_json(&info),
        None => serde_json::Value::Null,
    }
}

pub fn is_open(pool: &Pool, id: InstanceId) -> bool {
    pool.endpoint_host()
        .is_some_and(|host| host.describe(id).is_some())
}

/// The first step of `stop` and every host shutdown, so ending an instance never finds it held by
/// an endpoint.
pub fn close_for_stop(pool: &mut Pool, id: InstanceId) {
    let Some(host) = pool.endpoint_host() else {
        return;
    };
    if let Some(closed) = host.close(id)
        && let Some(session) = closed.session
    {
        return_session(pool, session);
    }
}

/// Releases the endpoint lease it carries.
fn return_session(pool: &mut Pool, mut session: Session) {
    let id = session.id;
    let now = session.now();
    if let Some(ticket) = session.ticket.take() {
        if ticket.holder() == LeaseHolder::Endpoint {
            if let Some(state) = pool.table_mut().get_mut(id) {
                let _ = state.lease.release(ticket, now);
            }
        } else {
            session.ticket = Some(ticket);
        }
    }
    pool.checkin(session);
}

fn no_host() -> ApiError {
    ApiError::new(
        E_HOST_UNSUPPORTED,
        "USB Serial/JTAG endpoints run in the native host (`passportsim serve` or the CLI)",
    )
    .with_hint("start the instance through the native daemon, then call `endpoint --tcp`")
}

/// By the host support table.
fn check_host(option: &str) -> Result<(), ApiError> {
    let name = format!("endpoint --{option}");
    let supported = Host::current().is_some_and(|h| host_support::hosts(&name).has(h));
    if supported {
        return Ok(());
    }
    let mut error = ApiError::new(
        E_HOST_UNSUPPORTED,
        format!("`{name}` does not run on this host"),
    );
    if let Some(hint) = host_support::hint(&name) {
        error = error.with_hint(hint);
    }
    Err(error)
}

fn info_json(i: &EndpointInfo) -> serde_json::Value {
    let tcp = i.tcp_port.map(|port| {
        serde_json::json!({
            "port": port,
            "rfc2217": format!("rfc2217://127.0.0.1:{port}"),
            "socket": format!("socket://127.0.0.1:{port}"),
        })
    });
    let pty = i
        .pty_path
        .as_ref()
        .map(|path| serde_json::json!({ "path": path, "limitation": i.pty_limitation }));
    serde_json::json!({
        "tcp": tcp,
        "pty": pty,
        "auto_download": i.auto_download,
        "clock": i.clock.as_str(),
        "clients": i.clients,
        "connections": i.connections,
        "last_mode": i.last_mode,
        "vt_us": i.vt_us,
        "qos": i.qos,
        "reanchors": i.reanchors,
    })
}

fn info_output(id: InstanceId, info: Option<&EndpointInfo>) -> Output {
    let mut json = info.map_or_else(
        || {
            serde_json::json!({
                "tcp": null, "pty": null, "auto_download": false, "clock": null, "clients": 0,
                "connections": 0, "last_mode": null, "vt_us": 0, "qos": null, "reanchors": 0,
            })
        },
        info_json,
    );
    json["instance"] = serde_json::json!(id.to_string());
    json["open"] = serde_json::json!(info.is_some());
    json["closed"] = serde_json::json!(false);
    let mut text = String::new();
    match info {
        None => {
            let _ = write!(text, "{id}: no endpoint open");
        }
        Some(i) => {
            let running = match (i.clock, i.clients) {
                (EndpointClock::Agent, _) => "clock=agent",
                (EndpointClock::Endpoint, 0) => "clock=endpoint idle",
                (EndpointClock::Endpoint, _) => "clock=endpoint live",
            };
            let _ = write!(text, "{id}: {running}");
            if let Some(qos) = i.qos {
                let _ = write!(text, " qos={qos} reanchors={}", i.reanchors);
            }
            if let Some(port) = i.tcp_port {
                let _ = write!(
                    text,
                    "\n  flash:   rfc2217://127.0.0.1:{port}\n  monitor: socket://127.0.0.1:{port}"
                );
            }
            if let Some(path) = &i.pty_path {
                let _ = write!(text, "\n  pty:     {path} (data only)");
            }
        }
    }
    // `Output::to_json` writes this over the object's own `vt_us`; without it every reply would
    // read `vt_us: 0` while the instance ran live.
    let mut out = Output::new(json, text, crate::receipt::Receipt::default());
    out.vt_us = info.map_or(0, |i| i.vt_us);
    out
}

/// The final virtual time and instruction count.
fn closed_output(id: InstanceId, receipt: crate::receipt::Receipt) -> Output {
    let json = serde_json::json!({
        "instance": id.to_string(),
        "open": false,
        "closed": true,
        "final_vt_us": receipt.vt_us,
        "insns": receipt.insns,
    });
    let text = format!(
        "{id}: endpoints closed vt={}us insns={}",
        receipt.vt_us, receipt.insns
    );
    Output::new(json, text, receipt)
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`endpoint` arguments.",
        "properties": {
            "instance": instance_schema(),
            "tcp": { "type": "boolean", "description": "Open the TCP endpoint on 127.0.0.1." },
            "pty": { "type": "boolean", "description": "Open a pty (macOS, data only)." },
            "auto_download": { "type": "boolean", "description": "Reset to download on a raw SYNC." },
            "clock": { "type": "string", "enum": ["endpoint", "agent"], "description": "Who drives time (endpoint)." },
            "close": { "type": "boolean", "description": "Close the endpoints." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "open": { "type": "boolean" },
            "closed": { "type": "boolean" },
            "tcp": { "type": ["object", "null"] },
            "pty": { "type": ["object", "null"] },
            "auto_download": { "type": "boolean" },
            "clock": { "type": ["string", "null"] },
            "clients": { "type": "integer" },
            "connections": { "type": "integer" },
            "last_mode": { "type": ["string", "null"] },
            "vt_us": { "type": "integer" },
            "qos": { "type": ["string", "null"] },
            "reanchors": { "type": "integer" },
            "final_vt_us": { "type": "integer" },
            "insns": { "type": "integer" }
        }
    })
}

/// Open, describe or close an instance's USB Serial/JTAG endpoints for esptool and idf.py.
#[command(
    api_crate = crate,
    name = "endpoint",
    group = debug,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance, native_only),
    cli(positional = ["instance"]),
    errors(E_USAGE, E_STATE, E_LEASE, E_HOST_UNSUPPORTED),
    example(
        title = "Serve the only instance for esptool over RFC 2217",
        args = r#"{"tcp":true}"#,
    ),
    example(
        title = "Close the endpoints of p1",
        args = r#"{"instance":"p1","close":true}"#,
    ),
)]
pub fn endpoint(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = EndpointArgs::from_json(&args)?;
    let host = lock(shared()).endpoint_host();
    endpoint_on(host, &args, shared())
}

fn lock(pool: &Mutex<Pool>) -> MutexGuard<'_, Pool> {
    pool.lock().unwrap_or_else(|e| e.into_inner())
}

/// The testable core of the handler.
pub fn endpoint_on(
    host: Option<Arc<dyn EndpointHost>>,
    args: &EndpointArgs,
    pool: &Mutex<Pool>,
) -> Result<Output, ApiError> {
    if args.pty {
        check_host("pty")?;
    }
    // No busy check: an instance with open endpoints is checked out, and it is the one `close` and
    // a describe address. An absent instance is `E_STATE` on every host, so the host refusal comes
    // second.
    let id = lock(pool)
        .table()
        .bind(SPEC_ENDPOINT.annotations, args.instance.as_deref())?
        .ok_or_else(|| ApiError::new(E_STATE, "no instance to serve"))?;
    let host = host.ok_or_else(no_host)?;
    if args.close {
        let closed = host.close(id).ok_or_else(|| {
            ApiError::new(E_STATE, format!("instance `{id}` has no endpoint open"))
        })?;
        let mut pool = lock(pool);
        if let Some(session) = closed.session {
            return_session(&mut pool, session);
        }
        let receipt = match pool.session_mut(id) {
            Some(session) => session.receipt(),
            None => {
                return Err(ApiError::new(
                    E_STATE,
                    format!("instance `{id}` lost its session while its endpoints were open"),
                ));
            }
        };
        return Ok(closed_output(id, receipt));
    }
    if !args.opens() {
        return Ok(info_output(id, host.describe(id).as_ref()));
    }
    if host.describe(id).is_some() {
        return Err(ApiError::new(
            E_STATE,
            format!("instance `{id}` already has endpoints open"),
        )
        .with_hint("`endpoint --close` first, then open the set you want"));
    }
    let session = {
        let mut pool = lock(pool);
        let id = pool.bind(SPEC_ENDPOINT.annotations, Some(&id.to_string()))?;
        // A live bridge has already given the clock to a peer that answers in host time, so
        // `--clock agent` cannot take it. The reader is `clock`'s, covering the BLE and Wi-Fi
        // bridges alike.
        if args.clock == EndpointClock::Agent {
            super::clock::refuse_while_bridged(
                &mut pool,
                id,
                "hand the clock to the agent driving",
            )?;
        }
        let ticket = if args.clock == EndpointClock::Endpoint {
            let now = pool
                .session(id)
                .map(Session::now)
                .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
            let state = pool
                .table_mut()
                .get_mut(id)
                .ok_or_else(|| ApiError::new(E_STATE, format!("no instance `{id}`")))?;
            Some(state.lease.acquire(LeaseHolder::Endpoint, now, None)?)
        } else {
            None
        };
        match pool.checkout(id) {
            Ok(mut session) => {
                if ticket.is_some() {
                    session.ticket = ticket;
                }
                session
            }
            Err(error) => {
                if let (Some(ticket), Some(state)) = (ticket, pool.table_mut().get_mut(id)) {
                    let _ = state.lease.release(ticket, state.since);
                }
                return Err(error);
            }
        }
    };
    match host.open(session, args) {
        Ok(Opened { info, session }) => {
            if let Some(session) = session {
                return_session(&mut lock(pool), session);
            }
            Ok(info_output(id, Some(&info)))
        }
        Err(failed) => {
            let OpenFailed { session, error } = *failed;
            // A lost session leaves the instance busy, the honest state of an instance whose thread
            // died.
            if let Some(session) = session {
                return_session(&mut lock(pool), session);
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use crate::commands::start::tests::{TestMachine, started};

    /// Holds the session (`--clock endpoint`) or hands it back (`--clock agent`).
    #[derive(Default)]
    struct FakeHost {
        held: Mutex<BTreeMap<InstanceId, Option<Session>>>,
    }

    impl EndpointHost for FakeHost {
        fn open(&self, session: Session, args: &EndpointArgs) -> Result<Opened, Box<OpenFailed>> {
            let info = EndpointInfo {
                tcp_port: args.tcp.then_some(4242),
                auto_download: args.auto_download,
                clock: args.clock,
                ..EndpointInfo::default()
            };
            let id = session.id;
            let (kept, back) = match args.clock {
                EndpointClock::Endpoint => (Some(session), None),
                EndpointClock::Agent => (None, Some(session)),
            };
            self.held.lock().expect("held").insert(id, kept);
            Ok(Opened {
                info,
                session: back,
            })
        }
        fn close(&self, id: InstanceId) -> Option<Closed> {
            let session = self.held.lock().expect("held").remove(&id)?;
            Some(Closed {
                clock: if session.is_some() {
                    EndpointClock::Endpoint
                } else {
                    EndpointClock::Agent
                },
                session,
            })
        }
        fn describe(&self, id: InstanceId) -> Option<EndpointInfo> {
            self.held
                .lock()
                .expect("held")
                .contains_key(&id)
                .then(|| EndpointInfo {
                    tcp_port: Some(4242),
                    vt_us: 1_234,
                    qos: Some("user-interactive"),
                    reanchors: 2,
                    ..EndpointInfo::default()
                })
        }
    }

    #[test]
    fn open_checks_the_session_out_and_close_returns_it() {
        let (pool, id) = started(TestMachine::new());
        let pool = Mutex::new(pool);
        let host: Arc<dyn EndpointHost> = Arc::new(FakeHost::default());
        let open = EndpointArgs {
            instance: Some(id.to_string()),
            tcp: true,
            ..EndpointArgs::default()
        };
        let out = endpoint_on(Some(Arc::clone(&host)), &open, &pool).expect("opens");
        assert_eq!(out.json["tcp"]["rfc2217"], "rfc2217://127.0.0.1:4242");
        assert!(lock(&pool).is_busy(id), "agent calls see a busy instance");
        // The endpoint holds the lease, so an agent call is `E_LEASE` naming it.
        assert_eq!(
            lock(&pool)
                .table()
                .get(id)
                .and_then(|s| s.lease.holder(s.since)),
            Some(LeaseHolder::Endpoint)
        );
        let refused = lock(&pool)
            .bind(
                crate::commands::run::SPEC_RUN.annotations,
                Some(&id.to_string()),
            )
            .expect_err("the endpoint drives the clock");
        assert_eq!(refused.code, E_LEASE);
        assert!(
            refused.message.contains("`endpoint`"),
            "{}",
            refused.message
        );
        let again = endpoint_on(Some(Arc::clone(&host)), &open, &pool).expect_err("already open");
        assert_eq!(again.code, E_STATE);

        // The live virtual time, not the 0 of the default receipt, plus the thread's class and
        // re-anchors.
        let describe = EndpointArgs {
            instance: Some(id.to_string()),
            ..EndpointArgs::default()
        };
        let out = endpoint_on(Some(Arc::clone(&host)), &describe, &pool).expect("describes");
        let reply = out.to_json();
        assert_eq!(reply["vt_us"], 1_234, "{reply}");
        assert_eq!(reply["qos"], "user-interactive");
        assert_eq!(reply["reanchors"], 2);
        assert!(
            out.text.contains("qos=user-interactive reanchors=2"),
            "{}",
            out.text
        );

        let close = EndpointArgs {
            instance: Some(id.to_string()),
            close: true,
            ..EndpointArgs::default()
        };
        let out = endpoint_on(Some(Arc::clone(&host)), &close, &pool).expect("closes");
        assert_eq!(out.json["closed"], true);
        assert!(out.json["final_vt_us"].is_u64() && out.json["insns"].is_u64());
        assert!(!lock(&pool).is_busy(id));
        assert_eq!(
            lock(&pool)
                .table()
                .get(id)
                .and_then(|s| s.lease.holder(s.since)),
            None,
            "close releases the endpoint lease"
        );
        let error = endpoint_on(Some(host), &close, &pool).expect_err("nothing open");
        assert_eq!(error.code, E_STATE);
    }

    #[test]
    fn a_lease_held_by_another_party_refuses_the_endpoint_clock() {
        let (mut pool, id) = started(TestMachine::new());
        pool.table_mut()
            .get_mut(id)
            .expect("p1")
            .lease
            .acquire(LeaseHolder::Ui, pemu_core::time::VTime(0), None)
            .expect("the UI takes the lease");
        let pool = Mutex::new(pool);
        let host: Arc<dyn EndpointHost> = Arc::new(FakeHost::default());
        let open = EndpointArgs {
            instance: Some(id.to_string()),
            tcp: true,
            ..EndpointArgs::default()
        };
        let error = endpoint_on(Some(Arc::clone(&host)), &open, &pool).expect_err("held by ui");
        assert_eq!(error.code, E_LEASE);
        assert!(error.message.contains("`ui`"), "{}", error.message);
        assert!(!lock(&pool).is_busy(id));
        assert!(host.describe(id).is_none());

        // No lease, so it opens and the instance stays with the agent.
        let agent = EndpointArgs {
            clock: EndpointClock::Agent,
            ..open
        };
        let out = endpoint_on(Some(Arc::clone(&host)), &agent, &pool).expect("opens");
        assert_eq!(out.json["clock"], "agent");
        assert!(!lock(&pool).is_busy(id), "the agent keeps the instance");
        let close = EndpointArgs {
            instance: Some(id.to_string()),
            close: true,
            ..EndpointArgs::default()
        };
        endpoint_on(Some(host), &close, &pool).expect("closes");
        assert_eq!(
            lock(&pool)
                .table()
                .get(id)
                .and_then(|s| s.lease.holder(s.since)),
            Some(LeaseHolder::Ui),
            "the agent-clock endpoint never touched the UI's lease"
        );
    }

    #[test]
    fn without_a_native_host_the_command_is_host_unsupported() {
        let args = EndpointArgs {
            tcp: true,
            ..EndpointArgs::default()
        };
        // No instance is a state error on every build, host or not.
        assert_eq!(
            endpoint_on(None, &args, &Mutex::new(Pool::new()))
                .expect_err("no instance")
                .code,
            E_STATE
        );
        let (pool, id) = started(TestMachine::new());
        let args = EndpointArgs {
            instance: Some(id.to_string()),
            ..args
        };
        assert_eq!(
            endpoint_on(None, &args, &Mutex::new(pool))
                .expect_err("no host")
                .code,
            E_HOST_UNSUPPORTED
        );
    }

    #[test]
    fn pty_is_refused_off_macos_with_the_tcp_hint() {
        let result = check_host("pty");
        if cfg!(target_os = "macos") {
            assert!(result.is_ok());
        } else {
            let error = result.expect_err("not macOS");
            assert_eq!(error.code, E_HOST_UNSUPPORTED);
            assert!(error.hint.unwrap_or_default().contains("tcp"));
        }
        assert!(host_support::hosts("endpoint --pty").has(Host::MacOs));
        assert!(!host_support::hosts("endpoint --pty").has(Host::Windows));
    }

    #[test]
    fn malformed_arguments_are_usage() {
        for case in [
            serde_json::json!({ "close": true, "tcp": true }),
            serde_json::json!({ "auto_download": true }),
            serde_json::json!({ "port": 1 }),
            serde_json::json!({ "tcp": true, "clock": "wall" }),
            serde_json::json!({ "clock": "agent" }),
            serde_json::json!({ "close": true, "clock": "agent" }),
        ] {
            let error = EndpointArgs::from_json(&case).expect_err("outside the schema");
            assert_eq!(error.code, E_USAGE, "{case}");
        }
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("endpoint").expect("#[command] registered endpoint");
        for example in spec.examples {
            let args = example.args_json().expect("an example is JSON");
            EndpointArgs::from_json(&args).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.needs_instance && spec.annotations.native_only);
    }
}
