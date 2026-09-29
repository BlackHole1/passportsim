//! MCP over stdio, and the transport-independent MCP core ([`Mcp`]) that [`crate::mcp_http`]
//! also uses.
//!
//! Supported protocol versions are `2025-06-18`, `2025-11-25` and `2026-07-28`. Sampling, Roots and
//! Logging are never relied on; instance state is addressed by server-minted ids (`p1`, `p2`)
//! passed as ordinary tool arguments, as `2026-07-28` prescribes after removing sessions.
//!
//! The daemon never inherits the client's stdout: the adapter talks to the client on its own
//! stdio and reaches the daemon over loopback, and [`spawn_daemon`] starts it with all three
//! stdio handles on the null device.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pemu_api::error::ApiError;
use pemu_api::output::Output;
use pemu_api::spec::{CapsGroup, CommandSpec};

use crate::daemon::{Discovery, DiscoveryStore, Probe, SpawnError};
use crate::http::Server;
use crate::platform::DaemonSpawn;

/// MCP protocol versions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2026-07-28", "2025-11-25", "2025-06-18"];

/// The version answered to a client that named none: the oldest, because a client that did not
/// negotiate is least likely to understand a newer shape.
pub const FALLBACK_VERSION: &str = "2025-06-18";

pub const DISCOVER_VERSION: &str = "2026-07-28";

pub const TOOL_PREFIX: &str = "passport_";

mod rpc {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcRequest {
    /// The id, absent for a notification.
    pub id: Option<serde_json::Value>,
    pub method: String,
    pub params: serde_json::Value,
    /// The `_meta` object, where a `2026-07-28` client carries the per-request protocol version.
    pub meta: serde_json::Value,
}

impl RpcRequest {
    pub fn parse(value: &serde_json::Value) -> Result<RpcRequest, &'static str> {
        if value.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
            return Err("a request must carry `jsonrpc: \"2.0\"`");
        }
        let method = value
            .get("method")
            .and_then(serde_json::Value::as_str)
            .ok_or("a request must carry a `method` string")?
            .to_string();
        let params = value
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let meta = params
            .get("_meta")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        Ok(RpcRequest {
            id: value.get("id").cloned(),
            method,
            params,
            meta,
        })
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// The MCP server over one command registry, independent of transport.
pub struct Mcp {
    server: Arc<Server>,
    negotiated: String,
    caps: std::collections::BTreeSet<CapsGroup>,
    client_elicitation: bool,
}

impl std::fmt::Debug for Mcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mcp")
            .field("negotiated", &self.negotiated)
            .finish_non_exhaustive()
    }
}

impl Mcp {
    pub fn new(server: Arc<Server>) -> Mcp {
        Mcp {
            caps: server.mcp_caps().clone(),
            server,
            negotiated: FALLBACK_VERSION.to_string(),
            client_elicitation: false,
        }
    }

    /// Whether the client declared the `elicitation` capability in `initialize`.
    pub fn client_elicitation(&self) -> bool {
        self.client_elicitation
    }

    /// The same session narrowed to the tool groups a client asked for (always with Core, never
    /// beyond what the server runs). Only the MCP surface is filtered.
    pub fn with_caps(mut self, caps: &std::collections::BTreeSet<CapsGroup>) -> Mcp {
        self.caps = crate::http::narrow_caps(self.server.caps(), caps);
        self
    }

    pub fn caps(&self) -> &std::collections::BTreeSet<CapsGroup> {
        &self.caps
    }

    /// The same server with the version from a streamable-HTTP `MCP-Protocol-Version` header,
    /// the only way a negotiated version reaches a later request. Absent or unknown keeps
    /// [`FALLBACK_VERSION`].
    pub fn with_version(mut self, version: Option<&str>) -> Mcp {
        if let Some(version) = version
            && PROTOCOL_VERSIONS.contains(&version)
        {
            self.negotiated = version.to_string();
        }
        self
    }

    pub fn negotiated(&self) -> &str {
        &self.negotiated
    }

    /// Answers one request, or `None` for a notification.
    pub fn handle(&mut self, request: &RpcRequest) -> Option<serde_json::Value> {
        if let Some(version) = request
            .meta
            .get("protocolVersion")
            .and_then(serde_json::Value::as_str)
            && PROTOCOL_VERSIONS.contains(&version)
        {
            self.negotiated = version.to_string();
        }
        if request.is_notification() {
            return None;
        }
        let id = request.id.clone().unwrap_or(serde_json::Value::Null);
        let result = match request.method.as_str() {
            "initialize" => Ok(self.initialize(&request.params)),
            "ping" => Ok(serde_json::json!({})),
            "tools/list" => Ok(serde_json::json!({ "tools": self.tools() })),
            "tools/call" => self.call(&request.params),
            "server/discover" if self.negotiated == DISCOVER_VERSION => Ok(self.discover()),
            "server/discover" => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                format!("`server/discover` needs protocol {DISCOVER_VERSION}"),
            )),
            other => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                format!("no MCP method `{other}`"),
            )),
        };
        Some(match result {
            Ok(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(e) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() }),
        })
    }

    /// The `initialize` result. Only `tools` is announced.
    fn initialize(&mut self, params: &serde_json::Value) -> serde_json::Value {
        let asked = params
            .get("protocolVersion")
            .and_then(serde_json::Value::as_str);
        self.negotiated = match asked {
            Some(v) if PROTOCOL_VERSIONS.contains(&v) => v.to_string(),
            _ => FALLBACK_VERSION.to_string(),
        };
        self.client_elicitation = params
            .get("capabilities")
            .and_then(|caps| caps.get("elicitation"))
            .is_some_and(serde_json::Value::is_object);
        serde_json::json!({
            "protocolVersion": self.negotiated,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "passportsim", "version": env!("CARGO_PKG_VERSION") },
        })
    }

    fn discover(&self) -> serde_json::Value {
        serde_json::json!({
            "serverInfo": { "name": "passportsim", "version": env!("CARGO_PKG_VERSION") },
            "protocolVersions": PROTOCOL_VERSIONS,
            "capabilities": { "tools": { "listChanged": false } },
            "tools": self.tools(),
        })
    }

    pub fn tools(&self) -> Vec<serde_json::Value> {
        self.server
            .commands()
            .iter()
            .filter(|c| self.caps.contains(&c.group))
            .map(tool_json)
            .collect()
    }

    /// `tools/call`. A failed command is `isError: true` with the typed envelope in
    /// `structuredContent`, never a JSON-RPC error, so the agent can read `hint` and `serial_tail`.
    fn call(&self, params: &serde_json::Value) -> Result<serde_json::Value, RpcError> {
        let name = params
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| RpcError::new(rpc::INVALID_PARAMS, "`tools/call` needs a `name`"))?;
        let command = name.strip_prefix(TOOL_PREFIX).ok_or_else(|| {
            RpcError::new(
                rpc::INVALID_PARAMS,
                format!(
                    "`{name}` is not a tool of this server; every tool is `{TOOL_PREFIX}<command>`"
                ),
            )
        })?;
        if let Some(spec) = self
            .server
            .commands()
            .iter()
            .find(|c| c.name == command || c.cli.aliases.contains(&command))
            && !self.caps.contains(&spec.group)
        {
            let group = spec.group.caps_name();
            return Ok(error_result(
                &pemu_api::error::ApiError::new(
                    pemu_api::error::E_STATE,
                    format!("tool `{name}` is in the `{group}` group, which this MCP session did not enable"),
                )
                .with_hint(format!("start the MCP client with `passportsim mcp --caps {group}`")),
            ));
        }
        let mut arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let instance = arguments
            .get("instance")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if let Some(map) = arguments.as_object_mut() {
            map.remove("instance");
        }
        let instance = match instance {
            Some(text) => Some(
                pemu_api::instance::InstanceId::parse(&text)
                    .map_err(|e| RpcError::new(rpc::INVALID_PARAMS, e.message))?,
            ),
            None => None,
        };
        let outcome = match self.server.command_outcome(command, instance, arguments) {
            Ok(outcome) => outcome,
            Err(e) => return Ok(error_result(&e)),
        };
        Ok(mcp_result(&outcome))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError {
            code,
            message: message.into(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "code": self.code, "message": self.message })
    }
}

/// One tool description; annotations map to the MCP hints.
fn tool_json(spec: &CommandSpec) -> serde_json::Value {
    serde_json::json!({
        "name": format!("{TOOL_PREFIX}{}", spec.name),
        "description": spec.summary,
        // The agent schema carries no CLI-only argument.
        "inputSchema": spec.agent_input_schema(),
        "outputSchema": (spec.output_schema)(),
        "annotations": {
            "readOnlyHint": spec.annotations.read_only,
            "destructiveHint": spec.annotations.destructive,
            "idempotentHint": spec.annotations.idempotent,
            // A command reaching outside the emulator is a device command: it writes the
            // Passport or opens its port after a person says yes. `device_boot_check` writes no
            // flash yet can pull EN, so it is not `read_only`.
            "openWorldHint": spec.annotations.destructive || spec.annotations.human_confirm,
        },
    })
}

/// The MCP result of one command outcome: `content[0].text` is `Output::to_text` and
/// `structuredContent` is `Output::to_json`. The JSON has no `text` member, so the text must not
/// be read out of it (the old fallback sent the whole body twice).
fn mcp_result(outcome: &Result<Output, ApiError>) -> serde_json::Value {
    match outcome {
        Ok(output) => serde_json::json!({
            "content": [{ "type": "text", "text": output.to_text() }],
            "structuredContent": output.to_json(),
            "isError": false,
        }),
        Err(e) => error_result(e),
    }
}

fn error_result(error: &pemu_api::error::ApiError) -> serde_json::Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": error.message.clone() }],
        "structuredContent": { "error": error.to_json() },
        "isError": true,
    })
}

/// Pumps [`Mcp`] over a newline-delimited JSON-RPC stream. Returns at end of input, so a client
/// closing the pipe sees a clean EOF.
pub fn serve<R: BufRead, W: Write>(mcp: &mut Mcp, input: R, mut output: W) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(value) => match RpcRequest::parse(&value) {
                Ok(request) => mcp.handle(&request),
                Err(why) => Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": value.get("id").cloned().unwrap_or(serde_json::Value::Null),
                    "error": RpcError::new(rpc::INVALID_REQUEST, why).to_json(),
                })),
            },
            Err(e) => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": serde_json::Value::Null,
                "error": RpcError::new(rpc::PARSE_ERROR, e.to_string()).to_json(),
            })),
        };
        if let Some(response) = response {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// MCP elicitation over a stdio session: one `elicitation/create`, then input read until the
/// client answers. It returns what the person typed and decides nothing; the planner compares it
/// with the plan digest. Never a yes by default: no capability, an error, a closed stream or a
/// timeout is `Unavailable`, and `decline`/`cancel` is `Declined`. A client request sent meanwhile
/// gets a JSON-RPC error.
pub struct StdioElicitation<'a, L: LineSource, W: Write> {
    input: &'a mut L,
    output: &'a mut W,
    supported: bool,
    next: u64,
    timeout: std::time::Duration,
}

/// How long a stdio elicitation waits: long enough to read a plan and type its digest, short
/// enough that a client that dropped the question does not hold the session forever.
pub const ELICIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NextLine {
    Line(String),
    Closed,
    TimedOut,
}

/// The input of a stdio session, a line at a time, with a bounded wait where the source allows.
pub trait LineSource {
    fn next_line(&mut self, wait: std::time::Duration) -> NextLine;
}

impl<R: BufRead> LineSource for R {
    fn next_line(&mut self, _wait: std::time::Duration) -> NextLine {
        let mut line = String::new();
        match self.read_line(&mut line) {
            Ok(0) | Err(_) => NextLine::Closed,
            Ok(_) => NextLine::Line(line),
        }
    }
}

/// Lines a reader thread forwards ([`TimedLines::spawn`]), which a wait can bound.
pub struct TimedLines(pub std::sync::mpsc::Receiver<String>);

impl TimedLines {
    pub fn spawn<R: BufRead + Send + 'static>(mut input: R) -> TimedLines {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                match input.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        TimedLines(rx)
    }
}

impl LineSource for TimedLines {
    fn next_line(&mut self, wait: std::time::Duration) -> NextLine {
        match self.0.recv_timeout(wait) {
            Ok(line) => NextLine::Line(line),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => NextLine::TimedOut,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => NextLine::Closed,
        }
    }
}

pub const ELICIT_FIELD: &str = "plan_sha256";

impl<'a, L: LineSource, W: Write> StdioElicitation<'a, L, W> {
    pub fn new(input: &'a mut L, output: &'a mut W, supported: bool) -> StdioElicitation<'a, L, W> {
        StdioElicitation {
            input,
            output,
            supported,
            next: 1,
            timeout: ELICIT_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> StdioElicitation<'a, L, W> {
        self.timeout = timeout;
        self
    }
}

pub fn elicitation_request(id: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "elicitation/create",
        "params": {
            "message": message,
            "requestedSchema": {
                "type": "object",
                "properties": {
                    ELICIT_FIELD: {
                        "type": "string",
                        "title": "Plan digest",
                        "description": "Type the plan digest shown above to flash; leave it empty to cancel."
                    }
                },
                "required": [ELICIT_FIELD]
            }
        }
    })
}

/// What a client's answer means: the typed text of an `accept`, `Declined` for `decline` or
/// `cancel`, anything else unavailable.
pub fn elicited_from(answer: &serde_json::Value) -> pemu_planner::flow::Elicited {
    use pemu_planner::flow::Elicited;
    if let Some(error) = answer.get("error") {
        return Elicited::Unavailable(format!(
            "the MCP client refused the elicitation: {}",
            error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("no message")
        ));
    }
    let result = &answer["result"];
    match result.get("action").and_then(serde_json::Value::as_str) {
        Some("accept") => Elicited::Typed(
            result["content"][ELICIT_FIELD]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ),
        Some("decline" | "cancel") => Elicited::Declined,
        _ => Elicited::Unavailable(
            "the MCP client's answer to the elicitation named no action".to_owned(),
        ),
    }
}

impl<L: LineSource, W: Write> pemu_planner::flow::ElicitationPort for StdioElicitation<'_, L, W> {
    fn elicit(&mut self, message: &str) -> pemu_planner::flow::Elicited {
        use pemu_planner::flow::Elicited;
        if !self.supported {
            return Elicited::Unavailable(
                "the MCP client declared no `elicitation` capability; confirm with the dialog or \
                 the terminal path instead"
                    .to_owned(),
            );
        }
        let id = format!("passportsim-elicit-{}", self.next);
        self.next += 1;
        let request = elicitation_request(&id, message);
        if writeln!(self.output, "{request}")
            .and_then(|()| self.output.flush())
            .is_err()
        {
            return Elicited::Unavailable("the MCP session's output is closed".to_owned());
        }
        let deadline = std::time::Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            // A client that keeps talking without answering does not extend the wait.
            let next = if left.is_zero() {
                NextLine::TimedOut
            } else {
                self.input.next_line(left)
            };
            let line = match next {
                NextLine::Line(line) => line,
                NextLine::Closed => {
                    return Elicited::Unavailable(
                        "the MCP client closed the session before answering".to_owned(),
                    );
                }
                NextLine::TimedOut => {
                    return Elicited::Unavailable(format!(
                        "the MCP client did not answer the confirmation within {} s",
                        self.timeout.as_secs_f64()
                    ));
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                return Elicited::Unavailable(
                    "the MCP client answered the elicitation with something that is not JSON"
                        .to_owned(),
                );
            };
            if value.get("method").is_some() {
                if let Some(other) = value.get("id") {
                    let busy = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": other,
                        "error": RpcError::new(
                            -32603,
                            "this session is waiting for the answer to a confirmation",
                        )
                        .to_json(),
                    });
                    let _ = writeln!(self.output, "{busy}").and_then(|()| self.output.flush());
                }
                continue;
            }
            if value.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str()) {
                return elicited_from(&value);
            }
        }
    }
}

/// Starts the daemon for an MCP client. The adapter's stdout is the MCP channel: a daemon holding
/// a copy would keep the client waiting for an EOF and could interleave logs into the protocol, so
/// the command sets no stdio and [`crate::platform::DaemonSpawn`] nulls all three handles.
pub fn spawn_daemon(
    exe: &Path,
    spawner: &dyn DaemonSpawn,
    store: &DiscoveryStore<'_>,
    probe: &dyn Probe,
    timeout: Duration,
) -> Result<Discovery, SpawnError> {
    let (child, discovery) = crate::daemon::spawn(exe, spawner, store, probe, timeout)?;
    // The daemon outlives this adapter: it is detached and reparented when this process exits.
    drop(child);
    Ok(discovery)
}

/// Every caps group an `--caps` list names, or Core alone ([`CapsGroup::parse_list`]).
pub fn parse_caps(list: &str) -> Result<std::collections::BTreeSet<CapsGroup>, String> {
    CapsGroup::parse_list(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Auth, Token};
    use crate::daemon::{DiscoveryStore, Liveness, Shutdown};
    use crate::http::Server;
    use crate::paths::OwnerOnlyFiles;
    use crate::platform::fake::{FakeDaemonSpawn, FakeOwnerOnly};
    use pemu_api::error::ApiError;
    use pemu_api::output::Output;
    use pemu_api::receipt::Receipt;
    use pemu_api::spec::{Annotations, CliShape, Example, HandlerCx, any_schema};
    use std::collections::BTreeSet;
    use std::io::Cursor;
    use std::net::SocketAddr;

    fn echo(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
        Ok(Output::new(
            serde_json::json!({ "echo": args }),
            "echoed",
            Receipt::default(),
        ))
    }

    static SPECS: &[CommandSpec] = &[CommandSpec {
        name: "status",
        group: CapsGroup::Core,
        summary: "A command the stdio tests dispatch to.",
        input_schema: any_schema,
        output_schema: any_schema,
        annotations: Annotations {
            read_only: true,
            ..Annotations::EMPTY
        },
        cli: CliShape::EMPTY,
        scenario_step: None,
        examples: &[Example {
            title: "status",
            args: "{}",
        }],
        errors: &[],
        handler: echo,
    }];

    fn mcp() -> Mcp {
        let server = Arc::new(
            Server::new(
                Auth::new(Token::from_bytes([9; 32]), 8765),
                Arc::new(crate::pool::Pool::new(2)),
                Shutdown::new(),
                BTreeSet::from([CapsGroup::Core]),
            )
            .with_commands(SPECS),
        );
        Mcp::new(server)
    }

    fn session(lines: &[&str]) -> Vec<serde_json::Value> {
        let mut mcp = mcp();
        let input = Cursor::new(lines.join("\n").into_bytes());
        let mut output = Vec::new();
        serve(&mut mcp, input, &mut output).expect("the pump reaches EOF cleanly");
        String::from_utf8(output)
            .expect("UTF-8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("one JSON value per line"))
            .collect()
    }

    #[test]
    fn the_stdio_pump_answers_one_json_value_per_line_and_ends_at_eof() {
        let answers = session(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        ]);
        assert_eq!(
            answers.len(),
            2,
            "a notification and a blank line answer nothing"
        );
        assert_eq!(answers[0]["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(answers[1]["id"], 2);
        assert_eq!(
            answers[1]["result"]["tools"][0]["name"],
            format!("{TOOL_PREFIX}status")
        );
    }

    #[test]
    fn a_malformed_line_is_reported_without_ending_the_session() {
        let answers = session(&[
            "not json at all",
            r#"{"id":3,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"ping","params":{}}"#,
        ]);
        assert_eq!(answers.len(), 3);
        assert_eq!(answers[0]["error"]["code"], -32700);
        assert_eq!(
            answers[1]["error"]["code"], -32600,
            "a request with no `jsonrpc` is an invalid request, not a parse error"
        );
        assert_eq!(
            answers[1]["id"], 3,
            "the id is echoed so the client can match it"
        );
        assert_eq!(answers[2]["result"], serde_json::json!({}));
    }

    #[test]
    fn a_read_only_command_is_announced_with_the_read_only_hint() {
        let answers = session(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#]);
        let tool = &answers[0]["result"]["tools"][0];
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert_eq!(tool["annotations"]["openWorldHint"], false);
    }

    struct ReadyProbe;

    impl Probe for ReadyProbe {
        fn liveness(&self, _addr: SocketAddr, _token: &Token) -> Liveness {
            Liveness::Running
        }
    }

    /// A program the fake spawner really starts: `/usr/bin/true`, or `cmd.exe` on Windows.
    #[cfg(unix)]
    const TRIVIAL_PROGRAM: &str = "/usr/bin/true";
    #[cfg(not(unix))]
    const TRIVIAL_PROGRAM: &str = "cmd.exe";

    #[test]
    fn the_spawned_daemon_gets_no_stdio_of_this_adapter() {
        let command = crate::daemon::spawn_command(Path::new("/opt/passportsim"));
        assert!(
            command.get_current_dir().is_none(),
            "the daemon inherits no working directory choice either"
        );
        let printed = format!("{command:?}");
        assert!(
            !printed.contains("Stdio"),
            "the MCP adapter sets no stdio; the platform spawner does: {printed}"
        );

        let dir = std::env::temp_dir().join(format!("pemu-mcp-stdio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        let guard = FakeOwnerOnly::new();
        let store = DiscoveryStore::new(dir.join("run"), OwnerOnlyFiles::new(&guard));
        store
            .publish(&Discovery::new(8765, Token::from_bytes([3; 32]), 11))
            .expect("publish");
        let spawner = FakeDaemonSpawn::default();
        let discovery = spawn_daemon(
            Path::new(TRIVIAL_PROGRAM),
            &spawner,
            &store,
            &ReadyProbe,
            Duration::from_secs(5),
        )
        .expect("the fake host spawns");
        assert_eq!(discovery.port, 8765);
        assert_eq!(
            spawner.spawns(),
            vec![vec![
                TRIVIAL_PROGRAM.to_string(),
                "serve".to_string(),
                "--headless".to_string()
            ]],
            "the adapter asks for a headless serve and nothing else"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_caps_always_keeps_core_and_names_the_groups_it_refuses() {
        let caps = parse_caps("").expect("an empty list is Core alone");
        assert_eq!(caps, std::collections::BTreeSet::from([CapsGroup::Core]));
        let caps = parse_caps("audio, nfc").expect("two groups");
        assert!(caps.contains(&CapsGroup::Core), "Core is always on");
        assert!(caps.contains(&CapsGroup::Audio));
        assert!(caps.contains(&CapsGroup::Nfc));
        let refused = parse_caps("wifi").expect_err("no such group");
        assert!(
            refused.contains("radio"),
            "the message lists the real groups"
        );
    }

    fn prompt() -> pemu_planner::flow::ConfirmPrompt {
        pemu_planner::flow::ConfirmPrompt {
            intent: pemu_planner::flow::Intent::Flash,
            plan_sha256: "ab".repeat(32),
            writes: vec![("factory".to_owned(), 0x10000, 4096)],
            erase_nvs: None,
            writes_leftover: false,
            full_backup: false,
        }
    }

    fn accepted(
        answer: &pemu_planner::flow::Answer,
        prompt: &pemu_planner::flow::ConfirmPrompt,
    ) -> bool {
        matches!(answer, pemu_planner::flow::Answer::Accepted { plan_sha256 } if *plan_sha256 == prompt.plan_sha256)
    }

    #[test]
    fn a_client_that_never_answers_times_out_as_unavailable() {
        use pemu_planner::flow::{ElicitationPort, Elicited};
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let mut input = TimedLines(rx);
        let mut output = Vec::new();
        let started = std::time::Instant::now();
        let answer = StdioElicitation::new(&mut input, &mut output, true)
            .with_timeout(std::time::Duration::from_millis(50))
            .elicit("Flash plan ab..?");
        match answer {
            Elicited::Unavailable(why) => assert!(why.contains("did not answer"), "{why}"),
            other => panic!("a silent client is not {other:?}"),
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(
            String::from_utf8(output)
                .expect("UTF-8")
                .contains("elicitation/create"),
            "the question was asked"
        );

        for _ in 0..1_000 {
            tx.send("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n".to_owned())
                .expect("the receiver is alive");
        }
        let mut output = Vec::new();
        let answer = StdioElicitation::new(&mut input, &mut output, true)
            .with_timeout(std::time::Duration::ZERO)
            .elicit("Flash plan ab..?");
        assert!(matches!(answer, Elicited::Unavailable(_)), "{answer:?}");
        drop(tx);
        let answer = StdioElicitation::new(&mut input, &mut output, true).elicit("again?");
        assert!(
            matches!(&answer, Elicited::Unavailable(why) if why.contains("closed") || why.contains("did not")),
            "{answer:?}"
        );
    }

    fn ask(
        supported: bool,
        client_lines: &[String],
    ) -> (pemu_planner::flow::Answer, Vec<serde_json::Value>) {
        use pemu_planner::flow::{Confirmer, ElicitationConfirmer};
        let mut input = Cursor::new(client_lines.join("\n").into_bytes());
        let mut output = Vec::new();
        let answer = {
            let mut port = StdioElicitation::new(&mut input, &mut output, supported);
            ElicitationConfirmer::new(&mut port).confirm(&prompt())
        };
        let written = String::from_utf8(output)
            .expect("UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("one JSON value per line"))
            .collect();
        (answer, written)
    }

    const FIRST: &str = "passportsim-elicit-1";

    #[test]
    fn an_elicitation_the_client_declines_is_refused() {
        let reply =
            serde_json::json!({"jsonrpc": "2.0", "id": FIRST, "result": {"action": "decline"}});
        let (answer, written) = ask(true, &[reply.to_string()]);
        assert_eq!(answer, pemu_planner::flow::Answer::Declined);
        assert!(!accepted(&answer, &prompt()));
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(written[0]["method"], "elicitation/create");
        assert_eq!(written[0]["id"], FIRST);
        let message = written[0]["params"]["message"].as_str().expect("a message");
        assert!(message.contains(&prompt().plan_sha256), "{message}");
        assert_eq!(
            written[0]["params"]["requestedSchema"]["required"][0],
            ELICIT_FIELD
        );
        let cancel =
            serde_json::json!({"jsonrpc": "2.0", "id": FIRST, "result": {"action": "cancel"}});
        assert_eq!(
            ask(true, &[cancel.to_string()]).0,
            pemu_planner::flow::Answer::Declined
        );
        let empty = serde_json::json!({"jsonrpc": "2.0", "id": FIRST, "result": {"action": "accept", "content": {ELICIT_FIELD: " "}}});
        assert_eq!(
            ask(true, &[empty.to_string()]).0,
            pemu_planner::flow::Answer::Declined
        );
    }

    #[test]
    fn a_client_without_elicitation_is_unavailable_and_never_a_yes() {
        let (answer, written) = ask(false, &[]);
        assert!(
            matches!(&answer, pemu_planner::flow::Answer::Unavailable(hint) if hint.contains("elicitation")),
            "{answer:?}"
        );
        assert!(written.is_empty(), "nothing is asked: {written:?}");
        let (closed, _) = ask(true, &[]);
        assert!(
            matches!(closed, pemu_planner::flow::Answer::Unavailable(_)),
            "{closed:?}"
        );
        let error = serde_json::json!({"jsonrpc": "2.0", "id": FIRST, "error": {"code": -32601, "message": "no elicitation"}});
        let (refused, _) = ask(true, &[error.to_string()]);
        assert!(
            matches!(refused, pemu_planner::flow::Answer::Unavailable(_)),
            "{refused:?}"
        );
    }

    #[test]
    fn an_answer_with_a_different_digest_is_refused() {
        let other = "cd".repeat(32);
        let lines = [
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {}}).to_string(),
            serde_json::json!({"jsonrpc": "2.0", "id": "someone-else", "result": {"action": "accept", "content": {ELICIT_FIELD: prompt().plan_sha256}}}).to_string(),
            serde_json::json!({"jsonrpc": "2.0", "id": FIRST, "result": {"action": "accept", "content": {ELICIT_FIELD: other.to_ascii_uppercase()}}}).to_string(),
        ];
        let (answer, written) = ask(true, &lines);
        assert_eq!(
            answer,
            pemu_planner::flow::Answer::Accepted { plan_sha256: other },
            "the typed digest reaches the flow unchanged but for case"
        );
        assert!(
            !accepted(&answer, &prompt()),
            "a different digest is no yes"
        );
        assert_eq!(written.len(), 2, "{written:?}");
        assert_eq!(written[1]["id"], 7);
        assert!(written[1]["error"].is_object(), "{written:?}");
        let right = serde_json::json!({"jsonrpc": "2.0", "id": FIRST, "result": {"action": "accept", "content": {ELICIT_FIELD: prompt().plan_sha256}}});
        assert!(accepted(&ask(true, &[right.to_string()]).0, &prompt()));
    }

    #[test]
    fn initialize_records_whether_the_client_can_elicit() {
        let mut with = mcp();
        let init = |caps: serde_json::Value| RpcRequest {
            id: Some(serde_json::json!(1)),
            method: "initialize".to_owned(),
            params: serde_json::json!({"protocolVersion": "2025-06-18", "capabilities": caps}),
            meta: serde_json::Value::Null,
        };
        with.handle(&init(serde_json::json!({"elicitation": {}})));
        assert!(with.client_elicitation());
        let mut without = mcp();
        without.handle(&init(serde_json::json!({})));
        assert!(!without.client_elicitation());
    }
}
