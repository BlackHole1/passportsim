//! `passportsim net_http`: the virtual LAN's HTTP side, and the bridge that takes a guest's HTTP
//! request to a server on the host.
//!
//! | `op` | journals | answers with |
//! |---|---|---|
//! | `status` (default) | nothing | the LAN: leases, open connections, the scripted service, the bridge |
//! | `bridge` | `EnvChange::WifiBridge { attached: true, routes }` | the same, with the bridge attached |
//! | `unbridge` | `EnvChange::WifiBridge { attached: false }` | the same, with the bridge detached |
//!
//! A route allowlists one port: a guest TCP connection to `host.emu.internal` (the gateway) on
//! `port` is proxied to `127.0.0.1:host_port`. Nothing else is bridged, and the routes are
//! journaled with the attach. The machine side is `pemu_radio::lan::bridge`; the host side is a
//! transport ([`NetBridgeIo`]), and a build with none (the wasm core) journals the attach alone for
//! another carrier, such as the browser's relay.
//!
//! A bridged peer answers in host time, so while attached the instance runs at `Wall { rate: 1 }`
//! and other pacings are `E_LEASE`. At every slice boundary [`live_bridge_tick`] hands the session
//! to the host, which moves packets and sleeps until wall time catches up, so a 300 ms server
//! answers in 300 ms of guest time.

use pemu_core::input::{EnvChange, InputEvent, NET_MAX_ROUTES, NetRoute};
use pemu_core::journal::Origin;
use pemu_machine::machine::At;
use pemu_radio::lan::bridge::HOST_NAME;
use pemu_radio::lan::gateway::{GATEWAY_IP, tcp_state};
use pemu_radio::lan::services;
use pemu_radio::wifi::driver::WifiState;

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_STATE, E_USAGE};
use crate::output::Output;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::nfc_tap::bind_checked;
use crate::args::{instance_schema, object, only, opt_str, opt_u64, usage};
use crate::session::Session;

pub const MODULE: &str = "wifi";

/// Re-exported so a host that carries the bridge need not name `pemu-radio`.
pub use pemu_radio::lan::bridge as wisp;

/// Installed by a host that can open a socket. Both take the session, not its id: the host keeps
/// its transports in the session's pool, and an id names an instance only within its pool.
#[derive(Copy, Clone)]
pub struct NetBridgeIo {
    /// `Err` says why, such as a route to a port the host denies.
    pub attach: fn(&Session, &[NetRoute]) -> Result<(), String>,
    /// Closing one that is not open is fine: the journal is the truth about whether it is attached.
    pub detach: fn(&Session),
}

static NET_BRIDGE_IO: std::sync::OnceLock<NetBridgeIo> = std::sync::OnceLock::new();

/// The first installation wins.
pub fn set_bridge_io(io: NetBridgeIo) {
    let _ = NET_BRIDGE_IO.set(io);
}

/// `None` in a build that cannot open a socket.
pub fn installed_bridge_io() -> Option<NetBridgeIo> {
    NET_BRIDGE_IO.get().copied()
}

/// Moves the bridges' packets and paces at `Wall { rate: 1 }`. `first` marks a call's first
/// boundary, where the host re-anchors, because the time between calls is the agent's, not the
/// guest's.
pub type BridgeTick = fn(&mut Session, bool);

static BRIDGE_TICK: std::sync::OnceLock<BridgeTick> = std::sync::OnceLock::new();

/// The first installation wins.
pub fn set_bridge_tick(tick: BridgeTick) {
    let _ = BRIDGE_TICK.set(tick);
}

/// Called by `run` at every slice boundary. Answers whether it handed off, so the caller keeps its
/// slices short while a bridge is live.
pub fn live_bridge_tick(session: &mut Session, first: bool) -> bool {
    if session.machine().live_bridges() == 0 {
        return false;
    }
    if let Some(tick) = BRIDGE_TICK.get() {
        tick(session, first);
    }
    true
}

/// `Ok(None)` when the machine holds none: no Wi-Fi module, or one not reached yet.
pub fn wifi_state(session: &mut Session) -> Result<Option<WifiState>, ApiError> {
    let Some(bytes) = session.machine().radio_module_state(MODULE) else {
        return Ok(None);
    };
    WifiState::decode(bytes).map(Some).map_err(|err| {
        ApiError::new(
            E_INTERNAL,
            format!("the Wi-Fi module state does not decode: {err:?}"),
        )
    })
}

pub fn bridge_journal(
    session: &mut Session,
    attached: bool,
    routes: &[NetRoute],
) -> Result<(), ApiError> {
    session
        .machine()
        .input(
            At::Now,
            InputEvent::Env(EnvChange::WifiBridge {
                attached,
                routes: routes.to_vec(),
            }),
        )
        .map(|_| ())
        .map_err(|_| {
            ApiError::new(
                E_STATE,
                "the machine refused the Wi-Fi bridge change at the current instant",
            )
            .with_hint("`status` shows the instance's lifecycle state")
        })
}

/// Also the next cursor and how many the window dropped before any reader took them. Reading takes
/// nothing out of the machine.
pub fn bridge_outbound(
    session: &mut Session,
    cursor: u64,
) -> Result<(Vec<Vec<u8>>, u64, u64), ApiError> {
    let state = wifi_state(session)?.unwrap_or_default();
    let (packets, next) = state.lan.bridge.since(cursor);
    Ok((packets, next, state.lan.bridge.dropped_out))
}

/// So a transport that reopens continues the stream rather than looking like one that lost packets.
pub fn bridge_next_seq(session: &mut Session) -> u64 {
    wifi_state(session)
        .ok()
        .flatten()
        .map_or(0, |st| st.lan.bridge.next_seq)
}

/// With [`Origin::Bridge`]: a live host peer, so the run is `Replayable` and every receipt says so.
pub fn bridge_inbound(session: &mut Session, seq: u64, data: Vec<u8>) -> Result<(), ApiError> {
    session
        .machine()
        .input_from(At::Now, Origin::Bridge, InputEvent::NetFrame { seq, data })
        .map(|_| ())
        .map_err(|_| {
            ApiError::new(
                E_STATE,
                "the machine refused the bridged packet at the current instant",
            )
        })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Status,
    Bridge,
    Unbridge,
}

impl Op {
    fn parse(text: &str) -> Option<Op> {
        match text {
            "status" => Some(Op::Status),
            "bridge" => Some(Op::Bridge),
            "unbridge" => Some(Op::Unbridge),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Status => "status",
            Op::Bridge => "bridge",
            Op::Unbridge => "unbridge",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetHttpArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub op: Op,
    /// `bridge`: the allowlisted routes.
    pub routes: Vec<NetRoute>,
}

fn port_of(value: &serde_json::Value, what: &str) -> Result<u16, ApiError> {
    value
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
        .ok_or_else(|| usage(what, "expected a TCP port in 1..=65535"))
}

impl NetHttpArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<NetHttpArgs, ApiError> {
        let args = object(value)?;
        only(args, &["instance", "op", "port", "host_port", "routes"])?;
        let op = match opt_str(args, "op")? {
            None => Op::Status,
            Some(text) => Op::parse(text).ok_or_else(|| {
                usage(
                    "op",
                    &format!("`{text}` is not one of status, bridge, unbridge"),
                )
            })?,
        };
        let mut routes = Vec::new();
        if let Some(port) = args.get("port").filter(|v| !v.is_null()) {
            let port = port_of(port, "port")?;
            let host_port = match opt_u64(args, "host_port")? {
                None => port,
                Some(_) => port_of(&args["host_port"], "host_port")?,
            };
            routes.push(NetRoute { port, host_port });
        } else if args.get("host_port").is_some_and(|v| !v.is_null()) {
            return Err(usage("host_port", "names the host side of a `port`"));
        }
        if let Some(list) = args.get("routes").filter(|v| !v.is_null()) {
            let list = list
                .as_array()
                .ok_or_else(|| usage("routes", "expected an array of {port, host_port}"))?;
            for (i, route) in list.iter().enumerate() {
                let at = |f: &str| format!("routes[{i}].{f}");
                let route = object(route)?;
                only(route, &["port", "host_port"])?;
                let port = port_of(
                    route
                        .get("port")
                        .ok_or_else(|| usage(&at("port"), "is required"))?,
                    &at("port"),
                )?;
                let host_port = match route.get("host_port").filter(|v| !v.is_null()) {
                    None => port,
                    Some(v) => port_of(v, &at("host_port"))?,
                };
                routes.push(NetRoute { port, host_port });
            }
        }
        match op {
            Op::Bridge if routes.is_empty() => {
                return Err(usage(
                    "port",
                    "`bridge` needs a `port` (or `routes`) to allowlist",
                ));
            }
            Op::Status | Op::Unbridge if !routes.is_empty() => {
                return Err(usage("port", "only `bridge` takes routes"));
            }
            _ => {}
        }
        if !NetRoute::check_all(&routes) {
            return Err(usage(
                "routes",
                &format!("at most {NET_MAX_ROUTES} routes, each guest port once"),
            ));
        }
        Ok(NetHttpArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            op,
            routes,
        })
    }
}

fn dotted(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

fn tcp_state_name(state: u8) -> &'static str {
    match state {
        tcp_state::SYN_RECEIVED => "syn-received",
        tcp_state::ESTABLISHED => "established",
        tcp_state::LAST_ACK => "last-ack",
        tcp_state::SYN_PENDING => "connecting",
        _ => "unknown",
    }
}

/// The `status` answer of every operation.
pub fn lan_json(state: &WifiState) -> serde_json::Value {
    let lan = &state.lan;
    let b = &lan.bridge;
    serde_json::json!({
        "gateway": dotted(GATEWAY_IP),
        "leases": lan.leases.iter().map(|l| serde_json::json!({
            "ip": dotted(l.ip),
            "bound": l.bound,
        })).collect::<Vec<_>>(),
        "service": {
            "port": services::HTTP_PORT,
            "path": services::PROBE_PATH,
            "body_bytes": services::PROBE_BODY.len(),
            "answers": lan.counters.http_answers,
        },
        "connections": lan.tcp.iter().map(|c| serde_json::json!({
            "peer": format!("{}:{}", dotted(c.peer_ip), c.peer_port),
            "port": c.local_port,
            "state": tcp_state_name(c.state),
            "bridged": c.stream != 0,
        })).collect::<Vec<_>>(),
        "bridge": {
            "attached": b.attached,
            "host": HOST_NAME,
            "routes": b.routes.iter().map(|r| serde_json::json!({
                "port": r.port,
                "host_port": r.host_port,
            })).collect::<Vec<_>>(),
            "relay_ready": b.credit0 != 0,
            "streams": b.counters.connects,
            "refused": b.counters.refused,
            "bytes_out": b.counters.bytes_out,
            "bytes_in": b.counters.bytes_in,
            "inbound": b.inbound,
            "lost_in": b.lost_in,
            "dropped_out": b.dropped_out,
        },
    })
}

fn no_module() -> ApiError {
    ApiError::new(
        E_STATE,
        "this instance has no bound Wi-Fi module, so it has no virtual LAN",
    )
    .with_hint(
        "`inspect fidelity` reports the binding: an image that links no Wi-Fi driver, a refused \
         binding and a disabled model all leave the LAN absent",
    )
}

pub fn net_http_on(session: &mut Session, args: &NetHttpArgs) -> Result<Output, ApiError> {
    let instance = session.id.to_string();
    let mut notes: Vec<String> = Vec::new();
    match args.op {
        Op::Status => {}
        Op::Bridge => {
            // Up before the journal entry, so the machine never reports a bridge the host cannot
            // carry.
            let io = installed_bridge_io();
            if let Some(io) = io {
                (io.attach)(session, &args.routes).map_err(|err| {
                    ApiError::new(
                        E_STATE,
                        format!("the Wi-Fi bridge could not be opened: {err}"),
                    )
                    .with_hint(
                        "a route reaches 127.0.0.1 only, and never the daemon's own port or a \
                         USB endpoint's",
                    )
                })?;
            } else {
                notes.push(
                    "no host transport in this build: the bridge is attached in the journal and \
                     another carrier moves its packets"
                        .to_owned(),
                );
            }
            let applied = bridge_journal(session, true, &args.routes).and_then(|()| {
                let now = session.now();
                session.run_until(now);
                match wifi_state(session)? {
                    Some(st) if st.lan.bridge.attached => Ok(()),
                    _ => Err(no_module()),
                }
            });
            if let Err(error) = applied {
                if let Some(io) = io {
                    (io.detach)(session);
                }
                return Err(error);
            }
            notes.push(
                "the bridge is live: the run is paced at realtime 1.000x and is replayable from \
                 its journal, not deterministic"
                    .to_owned(),
            );
        }
        Op::Unbridge => {
            bridge_journal(session, false, &[])?;
            let now = session.now();
            session.run_until(now);
            if let Some(io) = installed_bridge_io() {
                (io.detach)(session);
            }
        }
    }
    let state = wifi_state(session)?.ok_or_else(no_module)?;
    let receipt = session.receipt();
    let lan = lan_json(&state);
    let bridge = &state.lan.bridge;
    let text = format!(
        "{instance} net_http {}: gateway {} leases {} connections {}; bridge {}{}",
        args.op.as_str(),
        dotted(GATEWAY_IP),
        state.lan.leases.len(),
        state.lan.tcp.len(),
        if bridge.attached {
            "attached"
        } else {
            "detached"
        },
        bridge
            .routes
            .iter()
            .map(|r| format!(" {HOST_NAME}:{} -> 127.0.0.1:{}", r.port, r.host_port))
            .collect::<String>(),
    );
    let json = serde_json::json!({
        "instance": instance,
        "vt_us": receipt.vt_us,
        "op": args.op.as_str(),
        "lan": lan,
        "notes": notes,
    });
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

pub fn input_schema() -> Schema {
    let route = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["port"],
        "properties": {
            "port": { "type": "integer", "minimum": 1, "maximum": 65535 },
            "host_port": { "type": "integer", "minimum": 1, "maximum": 65535 }
        }
    });
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "`net_http` arguments.",
        "properties": {
            "instance": instance_schema(),
            "op": { "enum": ["status", "bridge", "unbridge"], "description": "Default status." },
            "port": {
                "type": "integer", "minimum": 1, "maximum": 65535,
                "description": "bridge: the guest port on host.emu.internal."
            },
            "host_port": {
                "type": "integer", "minimum": 1, "maximum": 65535,
                "description": "bridge: the 127.0.0.1 port it reaches (default: port)."
            },
            "routes": { "type": "array", "maxItems": NET_MAX_ROUTES, "items": route }
        }
    });
    Schema::try_from(schema).unwrap_or_default()
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "vt_us": { "type": "integer" },
            "op": { "type": "string" },
            "lan": { "type": "object" },
            "notes": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// Report the virtual LAN, or bridge an allowlisted port to a server on the host.
#[command(
    api_crate = crate,
    name = "net_http",
    group = radio,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    scenario_step = "net.http",
    errors(E_USAGE, E_STATE, E_LEASE, E_INTERNAL),
    example(
        title = "Show the LAN, its HTTP service and the bridge",
        args = r#"{}"#,
    ),
    example(
        title = "Bridge the guest's port 80 on host.emu.internal to a host server on 8080",
        args = r#"{"op":"bridge","port":80,"host_port":8080}"#,
    ),
    example(
        title = "Detach the bridge",
        args = r#"{"op":"unbridge"}"#,
    ),
)]
pub fn net_http(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = NetHttpArgs::from_json(&args)?;
    crate::pool::with_session(
        |pool| bind_checked(pool, SPEC_NET_HTTP.annotations, args.instance.as_deref()),
        |session| net_http_on(session, &args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_parse_and_refuse_by_name() {
        let args = NetHttpArgs::from_json(&serde_json::json!({})).unwrap();
        assert_eq!(args.op, Op::Status);
        let args =
            NetHttpArgs::from_json(&serde_json::json!({"op":"bridge","port":80,"host_port":18080}))
                .unwrap();
        assert_eq!(
            args.routes,
            vec![NetRoute {
                port: 80,
                host_port: 18_080
            }]
        );
        let args =
            NetHttpArgs::from_json(&serde_json::json!({"op":"bridge","routes":[{"port":8080}]}))
                .unwrap();
        assert_eq!(args.routes[0].host_port, 8080, "host_port defaults to port");
        for bad in [
            serde_json::json!({"op":"bridge"}),
            serde_json::json!({"op":"status","port":80}),
            serde_json::json!({"op":"bridge","port":0}),
            serde_json::json!({"op":"bridge","port":70000}),
            serde_json::json!({"op":"bridge","routes":[{"port":80},{"port":80}]}),
            serde_json::json!({"op":"bridge","host_port":80}),
            serde_json::json!({"op":"open"}),
            serde_json::json!({"url":"http://x"}),
        ] {
            let err = NetHttpArgs::from_json(&bad).expect_err("refused");
            assert_eq!(err.code, E_USAGE, "{bad}");
        }
    }

    #[test]
    fn every_example_parses_and_the_command_is_in_the_radio_group() {
        let spec = crate::registry::find("net_http").expect("#[command] registered it");
        assert_eq!(spec.group, crate::spec::CapsGroup::Radio);
        for example in spec.examples {
            let value = example.args_json().expect("the example is JSON");
            NetHttpArgs::from_json(&value).expect("every example parses");
        }
    }
}
