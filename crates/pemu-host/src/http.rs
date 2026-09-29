//! The HTTP surface of `serve`: the route table, the status mapping and the request handler.
//!
//! [`Request`] and [`Response`] are plain values and [`Server::handle`] is an ordinary function,
//! so routing, auth and error mapping are testable without a socket; the axum adapter in [`serve`]
//! only translates.
//!
//! Every registered command is reachable at `POST /v1/instances/{id}/commands/{name}`, so the UI,
//! CLI and MCP cannot drift apart. The GET aliases answer their own media type for a browser or
//! `curl`; `screen.png`, `events.ndjson` and `artifacts/{path}` are refused with `E_STATE` until an
//! artifact reader serves them.
//!
//! A guest failure (timeout, panic, assertion) is a 200 with `ok: false`, so the full envelope is
//! always in the body; only a protocol failure gets a 4xx or 5xx ([`status_for`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use pemu_api::error::{ApiError, E_INTERNAL, E_STATE, E_USAGE, ErrorCode};
use pemu_api::instance::InstanceId;
use pemu_api::spec::{CapsGroup, CommandSpec, HandlerCx};
use pemu_core::time::VTime;

use crate::auth::{Auth, AuthError};
use crate::daemon::Shutdown;
use crate::pool::Pool;

/// Version of the HTTP surface itself, the `protocol` field of `/v1/health`.
pub const PROTOCOL: u32 = 1;

pub const JSON: &str = "application/json";

/// Media type of the `serial` and `ui.txt` GET aliases. The charset is explicit because the
/// bodies are guest text, which a browser guessing a locale encoding would garble.
pub const TEXT: &str = "text/plain; charset=utf-8";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Method {
    Get,
    Post,
    Delete,
}

impl Method {
    pub fn parse(text: &str) -> Option<Method> {
        match text {
            "GET" => Some(Method::Get),
            "POST" => Some(Method::Post),
            "DELETE" => Some(Method::Delete),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
        }
    }
}

/// Request headers, compared case-insensitively as HTTP requires.
#[derive(Clone, Debug, Default)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn new() -> Headers {
        Headers::default()
    }

    pub fn with(mut self, name: &str, value: impl Into<String>) -> Headers {
        self.0.push((name.to_ascii_lowercase(), value.into()));
        self
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: Method, path: impl Into<String>) -> Request {
        Request {
            method,
            path: path.into(),
            query: String::new(),
            headers: Headers::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Request {
        self.headers = self.headers.with(name, value);
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Request {
        self.body = body.into();
        self
    }

    /// The body as JSON, or `null` when empty.
    pub fn json(&self) -> Result<serde_json::Value, ApiError> {
        if self.body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&self.body)
            .map_err(|e| ApiError::new(E_USAGE, format!("the request body is not JSON: {e}")))
    }

    /// The value of a query parameter, not percent-decoded: every parameter is a number or a bare
    /// word.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then_some(v)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            headers: vec![("content-type".to_string(), content_type.to_string())],
            body: body.into(),
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Response {
        Response::new(status, JSON, value.to_string())
    }

    /// A JSON error envelope, at the status [`status_for`] gives its code.
    pub fn error(error: &ApiError) -> Response {
        Response::json(
            status_for(error.code),
            &serde_json::json!({ "ok": false, "result": serde_json::Value::Null, "error": error.to_json() }),
        )
    }

    /// The envelope of an auth refusal, at the 401 or 403 [`AuthError::status`] decided.
    ///
    /// It carries the registered `E_USAGE` code: an unregistered `E_UNAUTHORIZED` would be a code
    /// an agent finds in neither `GET /v1/schema` nor `docs/errors.md`.
    pub fn unauthorized(error: &AuthError) -> Response {
        let api = ApiError::new(E_USAGE, error.to_string()).with_hint(
            "send the daemon token of the discovery file as `Authorization: Bearer <token>`, \
             with a `Host` and any `Origin` naming the loopback address the daemon bound",
        );
        Response::json(
            error.status(),
            &serde_json::json!({ "ok": false, "result": serde_json::Value::Null, "error": api.to_json() }),
        )
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Response {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Health,
    Schema,
    /// `GET /v1/instances`: the `status` output.
    Instances,
    /// `POST /v1/instances`: the `start` input.
    Start,
    /// `DELETE /v1/instances/{id}`: the `stop` input.
    Stop(InstanceId),
    /// `POST /v1/instances/{id}/commands/{name}`: the one generic command route.
    Command {
        instance: InstanceId,
        name: String,
    },
    /// `GET /v1/instances/{id}/serial`, the `serial.log` alias included.
    Serial(InstanceId),
    Screen(InstanceId),
    Ui(InstanceId),
    Events(InstanceId),
    Artifact {
        instance: InstanceId,
        /// The forward-slash path below the instance artifact root.
        path: String,
    },
    Lease(InstanceId),
    /// `GET /v1/instances/{id}/ws`: the WebSocket upgrade.
    Ws(InstanceId),
    ScenarioRun,
    /// `POST /v1/shutdown`: the authenticated daemon stop.
    Shutdown,
    /// `GET /v1/attach`: a browser-hosted instance registering itself.
    Attach,
    /// `/v1/relay`: the WISP relay.
    Relay,
    /// `POST /mcp`: MCP over streamable HTTP.
    Mcp,
    /// `POST /v1/session`: a launch code redeemed for the session cookie.
    Session,
    /// `GET /`: the UI shell, the one document served without a credential.
    Shell,
    /// `GET /<name>`: one file of the static web UI.
    Asset {
        /// Already one safe segment ([`crate::webui::safe_name`]).
        name: String,
    },
    /// `GET /<corpus-id>.pebundle`: the bundle built on request.
    Bundle {
        /// Already one safe segment, matched against the host's corpus ids and never used as a
        /// path.
        id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteError {
    NotFound,
    /// The path is a route, but not with this method; the value lists the methods it has.
    MethodNotAllowed(&'static str),
    BadInstance(String),
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteError::NotFound => f.write_str("no such route"),
            RouteError::MethodNotAllowed(allowed) => {
                write!(f, "this route accepts {allowed}")
            }
            RouteError::BadInstance(text) => {
                write!(f, "`{text}` is not an instance id such as `p1`")
            }
        }
    }
}

/// Matches a method and path against the route table. A path that exists under another method is
/// `MethodNotAllowed`, not `NotFound`.
pub fn route(method: Method, path: &str) -> Result<Route, RouteError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let only = |m: Method, route: Route, allowed: &'static str| {
        if method == m {
            Ok(route)
        } else {
            Err(RouteError::MethodNotAllowed(allowed))
        }
    };
    match segments.as_slice() {
        // The static routes. `[]` and `[name]` cannot collide with a `/v1/...` path, which always
        // has at least two segments.
        [] => only(Method::Get, Route::Shell, "GET"),
        ["mcp"] => only(Method::Post, Route::Mcp, "POST"),
        ["v1", "health"] => only(Method::Get, Route::Health, "GET"),
        ["v1", "session"] => only(Method::Post, Route::Session, "POST"),
        ["v1", "schema"] => only(Method::Get, Route::Schema, "GET"),
        ["v1", "shutdown"] => only(Method::Post, Route::Shutdown, "POST"),
        ["v1", "attach"] => only(Method::Get, Route::Attach, "GET"),
        ["v1", "relay"] => only(Method::Get, Route::Relay, "GET"),
        ["v1", "scenarios:run"] => only(Method::Post, Route::ScenarioRun, "POST"),
        ["v1", "instances"] => match method {
            Method::Get => Ok(Route::Instances),
            Method::Post => Ok(Route::Start),
            Method::Delete => Err(RouteError::MethodNotAllowed("GET, POST")),
        },
        ["v1", "instances", id, rest @ ..] => {
            let instance =
                InstanceId::parse(id).map_err(|_| RouteError::BadInstance((*id).to_string()))?;
            match rest {
                [] => only(Method::Delete, Route::Stop(instance), "DELETE"),
                ["commands", name] => only(
                    Method::Post,
                    Route::Command {
                        instance,
                        name: (*name).to_string(),
                    },
                    "POST",
                ),
                ["serial"] | ["serial.log"] => only(Method::Get, Route::Serial(instance), "GET"),
                ["screen.png"] => only(Method::Get, Route::Screen(instance), "GET"),
                ["ui.txt"] => only(Method::Get, Route::Ui(instance), "GET"),
                ["events.ndjson"] => only(Method::Get, Route::Events(instance), "GET"),
                ["lease"] => only(Method::Post, Route::Lease(instance), "POST"),
                ["ws"] => only(Method::Get, Route::Ws(instance), "GET"),
                ["artifacts", tail @ ..] if !tail.is_empty() => only(
                    Method::Get,
                    Route::Artifact {
                        instance,
                        path: tail.join("/"),
                    },
                    "GET",
                ),
                _ => Err(RouteError::NotFound),
            }
        }
        // A `.pebundle` is a corpus id to resolve; anything else is a web UI file.
        [name] => match crate::webui::safe_name(name) {
            Some(name) => match name.strip_suffix(crate::webui::BUNDLE_SUFFIX) {
                Some(id) if crate::webui::safe_name(id).is_some() => {
                    only(Method::Get, Route::Bundle { id: id.to_string() }, "GET")
                }
                _ => only(
                    Method::Get,
                    Route::Asset {
                        name: name.to_string(),
                    },
                    "GET",
                ),
            },
            None => Err(RouteError::NotFound),
        },
        _ => Err(RouteError::NotFound),
    }
}

/// The HTTP status of an error code. Protocol problems get a status; guest outcomes (timeout,
/// panic, watchdog, tripwire) are results an agent must read in full, so they are 200 with
/// `ok: false`.
///
/// `E_STATE` covers both "no such instance" and "wrong state"; it maps to 409 because the second
/// is more common and a 404 would suggest the route is wrong.
pub fn status_for(code: ErrorCode) -> u16 {
    match code.name {
        "E_USAGE" | "E_STALE_REF" => 400,
        "E_SECRET_REFUSED" | "E_HOST_UNSUPPORTED" | "E_PLAN_REFUSED" | "E_DEVICE_BUSY"
        | "E_CARDID_CHANGED" => 403,
        "E_ASSET_MISSING" => 404,
        "E_STATE" | "E_LEASE" => 409,
        "E_ASSET_HASH" => 422,
        "E_INTERNAL" => 500,
        // The daemon itself is unavailable.
        "E_DAEMON" => 503,
        _ => 200,
    }
}

/// Everything a request is served against: one pool behind every transport.
pub struct Server {
    auth: Auth,
    pool: Arc<Pool>,
    shutdown: Shutdown,
    caps: BTreeSet<CapsGroup>,
    mcp_caps: BTreeSet<CapsGroup>,
    commands: &'static [CommandSpec],
    idle_exit: std::time::Duration,
    artifacts: Option<std::path::PathBuf>,
    run_id: String,
    web_ui: Option<crate::webui::WebUi>,
    sessions: crate::webui::Sessions,
    relay: Arc<crate::attach::Relay>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("port", &self.auth.port())
            .field("caps", &self.caps)
            .field("commands", &self.commands.len())
            .finish_non_exhaustive()
    }
}

impl Server {
    pub fn new(
        auth: Auth,
        pool: Arc<Pool>,
        shutdown: Shutdown,
        caps: BTreeSet<CapsGroup>,
    ) -> Server {
        Server {
            auth,
            pool,
            shutdown,
            mcp_caps: caps.clone(),
            caps,
            commands: pemu_api::registry::commands(),
            idle_exit: crate::daemon::IDLE_EXIT,
            artifacts: None,
            run_id: run_id(),
            web_ui: None,
            sessions: crate::webui::Sessions::new(),
            relay: Arc::new(crate::attach::Relay::new()),
        }
    }

    pub fn relay(&self) -> &Arc<crate::attach::Relay> {
        &self.relay
    }

    /// The same server serving the static web UI through `ui`. Without it every static path is
    /// 404, as in a development build with no payload.
    pub fn with_web_ui(mut self, ui: crate::webui::WebUi) -> Server {
        self.web_ui = Some(ui);
        self
    }

    /// Mints a single-use launch code that expires after [`crate::webui::LAUNCH_CODE_TTL`]. The
    /// caller puts it only in the URL fragment of the page it opens: never a query string, a file
    /// or a log.
    pub fn mint_launch_code(&self) -> Result<String, crate::auth::TokenError> {
        self.sessions.mint()
    }

    pub fn sessions(&self) -> &crate::webui::Sessions {
        &self.sessions
    }

    /// The same server with an artifacts root: every instance `start` creates gets
    /// `<root>/<run id>/<instance>/`, flushed on `stop` and on every shutdown path.
    pub fn with_artifacts(mut self, root: impl Into<std::path::PathBuf>) -> Server {
        self.artifacts = Some(root.into());
        self
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Tests only; the product value is ten minutes.
    pub fn with_idle_exit(mut self, idle_exit: std::time::Duration) -> Server {
        self.idle_exit = idle_exit;
        self
    }

    pub fn idle_exit(&self) -> std::time::Duration {
        self.idle_exit
    }

    /// A test cannot add to the `linkme` registry, so it passes its own command list.
    pub fn with_commands(mut self, commands: &'static [CommandSpec]) -> Server {
        self.commands = commands;
        self
    }

    /// The caps groups this server runs commands of. The product daemon enables every group but
    /// `device`; only the MCP tool surface is narrowed per client.
    pub fn caps(&self) -> &BTreeSet<CapsGroup> {
        &self.caps
    }

    /// The same server with the default MCP tool groups for a client that names none (`serve
    /// --caps`), always within [`Server::caps`] and with Core.
    pub fn with_mcp_caps(mut self, caps: &BTreeSet<CapsGroup>) -> Server {
        self.mcp_caps = narrow_caps(&self.caps, caps);
        self
    }

    pub fn mcp_caps(&self) -> &BTreeSet<CapsGroup> {
        &self.mcp_caps
    }

    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    pub fn commands(&self) -> &'static [CommandSpec] {
        self.commands
    }

    pub fn shutdown(&self) -> &Shutdown {
        &self.shutdown
    }

    /// The auth checks over three header values, for the WebSocket upgrade, which never becomes a
    /// [`Request`].
    pub fn authorize_headers(
        &self,
        host: Option<&str>,
        origin: Option<&str>,
        authorization: Option<&str>,
    ) -> Result<(), AuthError> {
        self.auth.check(host, origin, authorization)
    }

    /// The auth checks. The credential is the bearer token or a live session cookie; the cookie is
    /// `SameSite=Strict` and the `Origin` check has already refused a foreign page.
    pub fn authorize(&self, req: &Request) -> Result<(), AuthError> {
        self.auth.check_host(req.headers.get("host"))?;
        self.auth.check_origin(req.headers.get("origin"))?;
        self.credential(req)
    }

    /// The bearer token or a live session cookie. A request with an `Authorization` header is
    /// judged on it alone, so a wrong token is not quietly rescued by a cookie.
    fn credential(&self, req: &Request) -> Result<(), AuthError> {
        if let Some(header) = req.headers.get("authorization") {
            return self.auth.check_bearer(Some(header));
        }
        match crate::webui::cookie_value(req.headers.get("cookie"), crate::webui::SESSION_COOKIE) {
            // The refusal does not say whether the cookie was absent, stale or invented.
            Some(value) if self.sessions.holds(value) => Ok(()),
            _ => Err(AuthError::MissingCredential),
        }
    }

    /// Serves one request: `Host` and `Origin`, then the route table, then the credential, then
    /// the handler.
    ///
    /// The order is the security statement: a foreign `Host` or `Origin` is refused before any
    /// credential is looked at (DNS-rebinding and cross-site defense, and no timing signal about
    /// the token). The route comes before the credential because the shell needs none and
    /// `POST /v1/session` is authenticated by the launch code in its body.
    pub fn handle(&self, req: &Request) -> Response {
        if let Err(e) = self.auth.check_host(req.headers.get("host")) {
            return Response::unauthorized(&e);
        }
        if let Err(e) = self.auth.check_origin(req.headers.get("origin")) {
            return Response::unauthorized(&e);
        }
        let matched = route(req.method, &req.path);
        // Only the shell and the session route skip the credential. Every other outcome, routing
        // errors included, checks it first, so an unauthorized caller learns nothing of the routes.
        if !matches!(matched, Ok(Route::Shell) | Ok(Route::Session))
            && let Err(e) = self.credential(req)
        {
            // A static route's caller is a browser that cannot send a bearer token, so it is told
            // differently what to do.
            return match matched {
                Ok(Route::Asset { .. } | Route::Bundle { .. }) => crate::webui::no_session(),
                _ => Response::unauthorized(&e),
            };
        }
        let route = match matched {
            Ok(route) => route,
            Err(RouteError::MethodNotAllowed(allowed)) => {
                return Response::json(
                    405,
                    &serde_json::json!({ "ok": false, "error": { "code": "E_USAGE", "message": format!("{} {} is not served; this route accepts {allowed}", req.method.as_str(), req.path) } }),
                )
                .header("allow", allowed);
            }
            Err(e) => {
                return Response::json(
                    404,
                    &serde_json::json!({ "ok": false, "error": { "code": "E_USAGE", "message": e.to_string() } }),
                );
            }
        };
        match self.dispatch(&route, req) {
            Ok(response) => response,
            Err(e) => Response::error(&e),
        }
    }

    fn dispatch(&self, route: &Route, req: &Request) -> Result<Response, ApiError> {
        match route {
            Route::Health => Ok(Response::json(200, &self.health())),
            Route::Schema => Ok(Response::json(200, &self.schema())),
            Route::Shutdown => {
                self.shutdown.request();
                Ok(Response::json(
                    200,
                    &serde_json::json!({ "ok": true, "result": { "stopping": true } }),
                ))
            }
            Route::Instances => {
                // Native instances, then attached pages, each in id order.
                let mut ids = self.pool.live();
                ids.extend(self.relay.attached());
                Ok(Response::json(
                    200,
                    &serde_json::json!({
                        "ok": true,
                        "result": {
                            "instances": ids
                                .iter()
                                .map(|id| serde_json::Value::from(id.to_string()))
                                .collect::<Vec<_>>()
                        }
                    }),
                ))
            }
            Route::Start => self.command("start", None, req.json()?),
            Route::Stop(id) => self.command("stop", Some(*id), req.json()?),
            Route::Command { instance, name } => self.command(name, Some(*instance), req.json()?),
            Route::Lease(id) => self.command("clock", Some(*id), req.json()?),
            Route::ScenarioRun => self.command("scenario", None, req.json()?),
            Route::Serial(id) => self.serial_alias(*id, req),
            Route::Ui(id) => self.ui_alias(*id),
            // Serving the picture needs an artifact reader; JSON at a `.png` URL would be worse
            // than saying plainly that the alias is not wired.
            Route::Screen(id) => Err(ApiError::new(
                E_STATE,
                format!(
                    "instance `{id}` cannot serve `screen.png` yet: the alias returns the PNG an \
                     artifact holds, and this daemon has no artifact reader over the directory \
                     of `artifacts.rs`"
                ),
            )
            .with_hint(
                "`POST /v1/instances/{id}/commands/screenshot` returns the artifact reference \
                 today",
            )),
            Route::Events(id) | Route::Artifact { instance: id, .. } => Err(ApiError::new(
                E_STATE,
                format!(
                    "instance `{id}` has no artifact stream yet: this daemon has no artifact \
                     reader over the directory of `artifacts.rs`"
                ),
            )),
            Route::Ws(_) => Err(ApiError::new(
                E_USAGE,
                "this route is a WebSocket upgrade; connect with `Upgrade: websocket` and the \
                 `passportsim.v1` subprotocol",
            )),
            // Reached only without an upgrade: the upgrade itself is `attach::route`'s.
            Route::Attach => Err(ApiError::new(
                E_USAGE,
                "`/v1/attach` is a WebSocket upgrade; a page attaches with `new WebSocket(..)`",
            )),
            // Reached only without an upgrade: the upgrade itself is `relay_wisp::route`'s.
            Route::Relay => Err(ApiError::new(
                E_USAGE,
                "`/v1/relay` is a WebSocket upgrade; a page carries the bridge with \
                 `new WebSocket(..)`",
            )),
            // The axum adapter sends `/mcp` to `mcp_http::serve`; only a direct `handle` call gets
            // here.
            Route::Mcp => Err(ApiError::new(
                E_INTERNAL,
                "`/mcp` is served by `mcp_http::serve`, not by the REST dispatcher",
            )),
            Route::Shell => Ok(crate::webui::shell()),
            Route::Session => Ok(self.session(req)),
            Route::Asset { name } => {
                let asset = self
                    .web_ui
                    .as_ref()
                    .and_then(|ui| ui.asset(name))
                    .ok_or_else(crate::webui::not_served)?;
                Ok(crate::webui::static_response(
                    asset.content_type,
                    asset.bytes,
                ))
            }
            Route::Bundle { id } => {
                let bytes = self
                    .web_ui
                    .as_ref()
                    .and_then(|ui| ui.bundle(id))
                    .ok_or_else(crate::webui::not_served)?;
                Ok(crate::webui::static_response(
                    crate::webui::OCTET_STREAM,
                    bytes,
                ))
            }
        }
    }

    /// `POST /v1/session`: a launch code redeemed for the session cookie. The code is the
    /// credential. Every failure (not JSON, no code, used, expired, unknown) is the same 401, and
    /// the response echoes nothing of the request.
    fn session(&self, req: &Request) -> Response {
        let offered = req
            .json()
            .ok()
            .and_then(|body| body.get("code")?.as_str().map(str::to_owned));
        match offered.and_then(|code| self.sessions.redeem(&code)) {
            Some(cookie) => Response::json(
                200,
                &serde_json::json!({ "ok": true, "result": { "session": true } }),
            )
            .header("set-cookie", crate::webui::set_cookie(&cookie))
            .header("cache-control", "no-store"),
            None => crate::webui::no_session(),
        }
    }

    pub fn health(&self) -> serde_json::Value {
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "core": "pemu",
            "core_version": env!("CARGO_PKG_VERSION"),
            "caps": self.caps.iter().map(|g| g.caps_name()).collect::<Vec<_>>(),
            "protocol": PROTOCOL,
        })
    }

    /// `GET /v1/schema`: every command of an enabled group with its schemas.
    pub fn schema(&self) -> serde_json::Value {
        let commands: Vec<serde_json::Value> = self
            .commands
            .iter()
            .filter(|c| self.caps.contains(&c.group))
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "caps": c.group.caps_name(),
                    "summary": c.summary,
                    "input_schema": c.agent_input_schema(),
                    "output_schema": (c.output_schema)(),
                    "step_alias": c.scenario_step,
                    "annotations": {
                        "read_only": c.annotations.read_only,
                        "destructive": c.annotations.destructive,
                        "idempotent": c.annotations.idempotent,
                        "advances_time": c.annotations.advances_time,
                        "needs_instance": c.annotations.needs_instance,
                        "native_only": c.annotations.native_only,
                        "human_confirm": c.annotations.human_confirm,
                    },
                    "cli": { "positional": c.cli.positional, "aliases": c.cli.aliases },
                })
            })
            .collect();
        serde_json::json!({ "commands": commands })
    }

    /// Runs one registry command, on the instance thread when it addresses an instance, so HTTP
    /// and MCP calls to one instance serialize like CLI calls.
    pub fn command(
        &self,
        name: &str,
        instance: Option<InstanceId>,
        args: serde_json::Value,
    ) -> Result<Response, ApiError> {
        Ok(match self.command_outcome(name, instance, args)? {
            Ok(output) => Response::json(
                200,
                &serde_json::json!({ "ok": true, "result": output.to_json(), "error": serde_json::Value::Null }),
            ),
            Err(e) => Response::error(&e),
        })
    }

    /// Runs one registry command and returns the handler's own outcome, unshaped. The outer
    /// `Result` is a protocol problem (unknown command, group not served, instance gone), the
    /// inner one the guest outcome; every surface shapes this one value.
    ///
    /// `start` runs on a new registry worker (8 MiB stack). A command addressed to a hosted
    /// instance runs on that instance's thread with its artifact directory bound and the stream
    /// producer after it; an instance this pool does not host is `E_STATE`; anything else runs on
    /// the calling thread. The route's instance is put into the arguments when the schema has
    /// `instance` and the caller named none, because a handler learns its instance only there.
    pub fn command_outcome(
        &self,
        name: &str,
        instance: Option<InstanceId>,
        args: serde_json::Value,
    ) -> Result<Result<pemu_api::output::Output, ApiError>, ApiError> {
        let spec = self
            .commands
            .iter()
            .find(|c| c.name == name || c.cli.aliases.contains(&name))
            .ok_or_else(|| {
                ApiError::new(E_USAGE, format!("no command `{name}`"))
                    .with_hint("`GET /v1/schema` lists every command this daemon serves")
            })?;
        if !self.caps.contains(&spec.group) {
            return Err(ApiError::new(
                E_STATE,
                format!(
                    "command `{name}` is in the `{}` group, which this daemon was not started with",
                    spec.group.caps_name()
                ),
            )
            .with_hint("restart the daemon with `--caps` naming the group"));
        }
        let mut args = if args.is_null() {
            serde_json::json!({})
        } else {
            args
        };
        if let (Some(id), Some(map)) = (instance, args.as_object_mut())
            && takes_instance(spec)
        {
            match map.get("instance") {
                None => {
                    map.insert(
                        "instance".to_string(),
                        serde_json::Value::from(id.to_string()),
                    );
                }
                // A body naming the route's instance is fine; naming another is a contradiction,
                // never silently resolved.
                Some(named) if named.as_str() == Some(id.to_string().as_str()) => {}
                Some(named) => {
                    return Err(ApiError::new(
                        E_USAGE,
                        format!(
                            "the route addresses instance `{id}` but the arguments name `{}`",
                            named
                                .as_str()
                                .map_or_else(|| named.to_string(), str::to_string)
                        ),
                    )
                    .with_hint(
                        "drop `instance` from the body, or call the route of that instance",
                    ));
                }
            }
        }
        let handler = spec.handler;
        if spec.name == "start" {
            return self.start_on_worker(handler, args);
        }
        let named = instance.or_else(|| {
            args.get("instance")
                .and_then(serde_json::Value::as_str)
                .and_then(|text| InstanceId::parse(text).ok())
        });
        let bound = match named {
            Some(id) => Some(id),
            None if spec.annotations.needs_instance => {
                pemu_api::commands::start::with_pool(|pool| pool.bind(spec.annotations, None)).ok()
            }
            None => None,
        };
        // A browser-hosted instance is answered only by its page, through the relay.
        if let Some(id) = bound
            && crate::attach::is_browser(id)
        {
            let outcome = self.relay.call(id, spec, args)?;
            if spec.name == "stop" && outcome.is_ok() {
                self.relay.end(id, "the instance was stopped");
            }
            return Ok(outcome);
        }
        let worker = bound.and_then(|id| self.pool.worker(id).map(|w| (id, w)));
        // An instance command whose instance has no thread here must not run on the calling
        // thread: it would bypass the 8 MiB worker, the mailbox that serializes its calls, the
        // producer and the artifact directory.
        if worker.is_none()
            && let Some(id) = bound
            && spec.annotations.needs_instance
            && !self.pool.hosts(id)
        {
            return Err(not_hosted(id));
        }
        let call_root = crate::hooks::call_root();
        Ok(match (worker, instance) {
            (Some((id, worker)), _) => {
                // `stop` drops the session, so its firmware display form is read first.
                let fw = (spec.name == "stop")
                    .then(|| {
                        pemu_api::commands::start::with_pool(|pool| {
                            pool.session(id).map(|session| session.fw.clone())
                        })
                    })
                    .flatten();
                let outcome = self.run_bound(id, worker, call_root, move || {
                    handler(&mut HandlerCx {}, args)
                })?;
                if spec.name == "stop"
                    && let Ok(output) = &outcome
                {
                    let mut summary = output.json.clone();
                    if let Some(map) = summary.as_object_mut() {
                        map.insert("reason".to_string(), serde_json::Value::from("stop"));
                        if let Some(fw) = fw {
                            map.insert("fw".to_string(), serde_json::Value::from(fw));
                        }
                    }
                    self.pool.retire(id, &summary)?;
                }
                if let Ok(output) = &outcome
                    && let Err(refused) = self.adopt_forks(spec, output)
                {
                    return Ok(Err(refused));
                }
                outcome
            }
            (None, Some(id)) => return Err(crate::pool::no_instance(id)),
            (None, None) => {
                // A parallel `scenario` batch runs here, off any instance; its JUnit file goes to
                // a directory of its own beside the instances'.
                let dir = (spec.name == "scenario"
                    && args.get("jobs").and_then(serde_json::Value::as_u64) > Some(1))
                .then(|| self.batch_dir())
                .flatten();
                crate::artifacts::bind_current(dir, || handler(&mut HandlerCx {}, args))
            }
        })
    }

    fn batch_dir(&self) -> Option<crate::artifacts::SharedDir> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let root = self.artifacts.clone()?;
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::artifacts::ArtifactDir::create(&root, &self.run_id, &format!("batch-{n}"))
            .ok()
            .map(|dir| Arc::new(std::sync::Mutex::new(dir)))
    }

    /// Runs `job` on a registry worker's thread with the instance's artifact directory, stream
    /// producer and the caller's scenario root bound; the producer publishes after it.
    fn run_bound<T, F>(
        &self,
        id: InstanceId,
        worker: crate::pool::Worker,
        call_root: Option<std::path::PathBuf>,
        job: F,
    ) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let artifacts = worker.artifacts.clone();
        let hub = Arc::clone(&worker.hub);
        self.pool.run_on(id, move || {
            let slice_hub = Arc::clone(&hub);
            let outcome = crate::artifacts::bind_current(artifacts.clone(), || {
                crate::hub::bind_current(id, slice_hub, artifacts.clone(), || {
                    crate::hooks::rebind_call_root(call_root, job)
                })
            });
            publish(id, &hub, artifacts.as_ref());
            outcome
        })
    }

    /// Hosts the copies a `snapshot fork` minted, each on its own registry worker so it counts in
    /// `live`, the capacity check and the idle exit. All or nothing: if the pool refuses one copy,
    /// every copy is ended and the pool's retryable refusal is returned.
    fn adopt_forks(
        &self,
        spec: &CommandSpec,
        output: &pemu_api::output::Output,
    ) -> Result<(), ApiError> {
        if spec.name != "snapshot" || output.json.get("op").and_then(|v| v.as_str()) != Some("fork")
        {
            return Ok(());
        }
        let ids: Vec<InstanceId> = output
            .json
            .get("instances")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .filter_map(|text| InstanceId::parse(text).ok())
            .collect();
        for (at, id) in ids.iter().enumerate() {
            if let Err(refused) = self.pool.adopt_worker(*id, self.worker_resources(*id)) {
                for hosted in &ids[..at] {
                    let _ = self.pool.stop(*hosted, VTime::default());
                }
                for unhosted in &ids[at..] {
                    crate::pool::end_unhosted(*unhosted);
                }
                let asked = ids.len();
                return Err(ApiError::new(
                    refused.code,
                    format!(
                        "{}; none of the {asked} forks was kept, because only {at} fit",
                        refused.message
                    ),
                )
                .retryable()
                .with_hint(
                    "`snapshot fork` with a smaller `count`, `stop` an instance, or start the \
                     daemon with a larger `--max-instances`",
                ));
            }
        }
        Ok(())
    }

    fn worker_resources(&self, id: InstanceId) -> crate::pool::Worker {
        crate::pool::Worker {
            hub: crate::hub::Hub::new(id),
            // A directory that cannot be created leaves the instance without artifacts rather than
            // refusing a running machine; `audio_capture` then reports it.
            artifacts: self.artifacts.clone().and_then(|root| {
                crate::artifacts::ArtifactDir::create(&root, &self.run_id, &id.to_string())
                    .ok()
                    .map(|dir| Arc::new(std::sync::Mutex::new(dir)))
            }),
        }
    }

    fn start_on_worker(
        &self,
        handler: pemu_api::spec::Handler,
        args: serde_json::Value,
    ) -> Result<Result<pemu_api::output::Output, ApiError>, ApiError> {
        let outcome = self.pool.spawn_worker(
            move || {
                let outcome = handler(&mut HandlerCx {}, args);
                let id = outcome.as_ref().ok().and_then(output_instance);
                (id, outcome)
            },
            |id| self.worker_resources(id),
        )?;
        if let Some(id) = outcome.as_ref().ok().and_then(output_instance)
            && let Some(worker) = self.pool.worker(id)
        {
            let _ = self.pool.run_on(id, move || {
                publish(id, &worker.hub, worker.artifacts.as_ref());
            });
        }
        Ok(outcome)
    }

    /// `GET /v1/instances/{id}/serial`: `text/plain` with `X-Next-Cursor` and `X-VT-Ms`. An absent
    /// query parameter is left out, not sent as `null`, because the schema accepts a missing
    /// optional key but rejects an explicit `null`.
    fn serial_alias(&self, id: InstanceId, req: &Request) -> Result<Response, ApiError> {
        let mut args = serde_json::Map::new();
        args.insert("op".to_string(), serde_json::Value::from("read"));
        if let Some(stream) = req.param("stream") {
            args.insert("stream".to_string(), serde_json::Value::from(stream));
        }
        for name in ["cursor", "max_bytes"] {
            if let Some(text) = req.param(name) {
                // Non-numbers pass through so the command's schema reports the type error.
                let value = match text.parse::<u64>() {
                    Ok(n) => serde_json::Value::from(n),
                    Err(_) => serde_json::Value::from(text),
                };
                args.insert(name.to_string(), value);
            }
        }
        Ok(
            match self.command_outcome("serial", Some(id), serde_json::Value::Object(args))? {
                Ok(output) => {
                    let response = Response::new(200, TEXT, alias_text(&output))
                        .header("x-vt-ms", vt_ms(&output));
                    alias_header(response, "x-next-cursor", &output, "next_cursor")
                }
                Err(e) => Response::error(&e),
            },
        )
    }

    /// `GET /v1/instances/{id}/ui.txt`: the pruned tree as `text/plain`, with `X-UI-Rev`.
    fn ui_alias(&self, id: InstanceId) -> Result<Response, ApiError> {
        Ok(
            match self.command_outcome("ui", Some(id), serde_json::Value::Null)? {
                Ok(output) => {
                    let response = Response::new(200, TEXT, alias_text(&output))
                        .header("x-vt-ms", vt_ms(&output));
                    alias_header(response, "x-ui-rev", &output, "ui_rev")
                }
                Err(e) => Response::error(&e),
            },
        )
    }

    /// Stops every instance, joining each thread, then sets the shutdown flag. The signal path,
    /// `POST /v1/shutdown` and the idle exit all end here, so every path flushes artifacts first.
    pub fn shutdown_now(&self, now: VTime) {
        self.relay.close_all();
        self.pool.shutdown(now);
        self.shutdown.request();
    }
}

// `scenario run --jobs`: the batch that forks from the boot cache.

fn batch_server() -> &'static std::sync::Mutex<std::sync::Weak<Server>> {
    static SERVER: std::sync::OnceLock<std::sync::Mutex<std::sync::Weak<Server>>> =
        std::sync::OnceLock::new();
    SERVER.get_or_init(|| std::sync::Mutex::new(std::sync::Weak::new()))
}

/// Makes `server` the one `scenario run --jobs N` forks on, and installs [`run_batch`] as the
/// `pemu-api` batch runner. Without it a batch runs in order on the bound instance.
pub fn install_batch(server: &Arc<Server>) {
    *batch_server().lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(server);
    pemu_api::commands::scenario::set_batch_runner(Some(run_batch));
}

const BATCH_BOOT_CACHE: &str = "ui-settled";

const BATCH_SNAPSHOT: &str = "batch-boot";

trait BatchHost: Sync {
    fn room(&self) -> usize;
    fn start(&self, args: serde_json::Value) -> Result<(InstanceId, serde_json::Value), ApiError>;
    fn save(&self, id: InstanceId) -> Result<(), ApiError>;
    fn fork(&self, template: InstanceId) -> Result<InstanceId, ApiError>;
    fn run(
        &self,
        fork: InstanceId,
        item: pemu_api::commands::scenario::BatchItem,
    ) -> Result<pemu_api::scenario::Report, ApiError>;
    fn stop(&self, id: InstanceId);
}

struct ServerBatch<'a> {
    server: &'a Server,
    call_root: Option<std::path::PathBuf>,
}

impl BatchHost for ServerBatch<'_> {
    fn room(&self) -> usize {
        self.server
            .pool
            .max_instances()
            .saturating_sub(self.server.pool.live().len())
    }

    fn start(&self, args: serde_json::Value) -> Result<(InstanceId, serde_json::Value), ApiError> {
        let output = self.server.command_outcome("start", None, args)??;
        let id = output_instance(&output)
            .ok_or_else(|| ApiError::new(E_INTERNAL, "`start` named no instance"))?;
        Ok((id, output.json["boot"]["cache"].clone()))
    }

    fn save(&self, id: InstanceId) -> Result<(), ApiError> {
        self.server
            .command_outcome(
                "snapshot",
                Some(id),
                serde_json::json!({ "op": "save", "name": BATCH_SNAPSHOT }),
            )?
            .map(|_| ())
    }

    fn fork(&self, template: InstanceId) -> Result<InstanceId, ApiError> {
        let output = self.server.command_outcome(
            "snapshot",
            Some(template),
            serde_json::json!({ "op": "fork", "name": BATCH_SNAPSHOT, "count": 1 }),
        )??;
        output.json["instances"][0]
            .as_str()
            .and_then(|text| InstanceId::parse(text).ok())
            .ok_or_else(|| ApiError::new(E_INTERNAL, "`snapshot fork` named no instance"))
    }

    fn run(
        &self,
        fork: InstanceId,
        item: pemu_api::commands::scenario::BatchItem,
    ) -> Result<pemu_api::scenario::Report, ApiError> {
        let worker = self
            .server
            .pool
            .worker(fork)
            .ok_or_else(|| not_hosted(fork))?;
        self.server
            .run_bound(fork, worker, self.call_root.clone(), move || {
                // A panicking step must not take the fork's thread: the fork still has to be
                // stopped and the batch still has to answer.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    pemu_api::commands::scenario::run_scenario(&item.scenario, &item.options)
                }))
                .map_err(|_| panicked("the scenario"))
            })?
    }

    fn stop(&self, id: InstanceId) {
        let _ = self
            .server
            .command_outcome("stop", Some(id), serde_json::json!({}));
    }
}

fn panicked(what: &str) -> ApiError {
    ApiError::new(
        E_INTERNAL,
        format!("{what} panicked inside the batch runner"),
    )
    .with_hint("the instances the batch made were stopped; the daemon log has the panic")
}

fn run_batch(
    items: Vec<pemu_api::commands::scenario::BatchItem>,
    jobs: usize,
) -> Result<pemu_api::commands::scenario::BatchOutcome, ApiError> {
    let server = batch_server()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upgrade()
        .ok_or_else(|| ApiError::new(E_STATE, "no daemon hosts this batch"))?;
    let host = ServerBatch {
        server: &server,
        call_root: crate::hooks::call_root(),
    };
    batch_on(&host, items, jobs)
}

/// The batch: one template instance per distinct boot, started with `boot_cache: ui-settled` and
/// saved as [`BATCH_SNAPSHOT`]; then per scenario a fresh fork of its template, run on the fork's
/// worker and stopped. At most `jobs` run at once. Every instance made is stopped whatever
/// happens, and a panicking scenario is its own `E_INTERNAL`.
///
/// # Errors
///
/// `E_STATE` (retryable) once for the whole batch when there is no room for the templates and one
/// fork.
fn batch_on(
    host: &dyn BatchHost,
    items: Vec<pemu_api::commands::scenario::BatchItem>,
    jobs: usize,
) -> Result<pemu_api::commands::scenario::BatchOutcome, ApiError> {
    use pemu_api::commands::scenario::{BatchOutcome, batch_start};
    use pemu_api::scenario::Report;

    let starts: Vec<_> = items
        .iter()
        .map(|item| batch_start(&item.scenario))
        .collect();
    let mut wanted: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for start in starts.iter().flatten() {
        wanted
            .entry(start.args.to_string())
            .or_insert_with(|| start.args.clone());
    }
    let room = host.room();
    if !wanted.is_empty() && room < wanted.len() + 1 {
        return Err(ApiError::new(
            E_STATE,
            format!(
                "a parallel batch needs room for {} template instance(s) and one fork, and the \
                 daemon has room for {room}",
                wanted.len()
            ),
        )
        .retryable()
        .with_hint(
            "`stop` an instance, start the daemon with a larger `--max-instances`, or run with \
             `--jobs 1` on a started instance",
        ));
    }

    let mut templates: BTreeMap<String, Result<(InstanceId, serde_json::Value), ApiError>> =
        BTreeMap::new();
    for (text, args) in &wanted {
        let mut args = args.clone();
        args["boot_cache"] = BATCH_BOOT_CACHE.into();
        let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (id, cache) = host.start(args)?;
            // The id is kept before the save, so a failed save still stops the instance.
            match host.save(id) {
                Ok(()) => Ok((id, cache)),
                Err(refused) => {
                    host.stop(id);
                    Err(refused)
                }
            }
        }))
        .unwrap_or_else(|_| Err(panicked("starting a template")));
        templates.insert(text.clone(), started);
    }
    let workers = jobs
        .min(room.saturating_sub(templates.len()))
        .min(items.len())
        .max(1);

    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Report>>> =
        items.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(item) = items.get(at) else {
                        break;
                    };
                    let not_run = |refused: &ApiError| {
                        Report::not_run(&item.scenario.name, &item.options.source, refused)
                    };
                    let report = match &starts[at] {
                        Err(refused) => not_run(refused),
                        Ok(start) => match templates.get(&start.args.to_string()) {
                            Some(Ok((template, _))) => {
                                batch_one(host, *template, item, &start.keys)
                            }
                            Some(Err(refused)) => not_run(refused),
                            None => not_run(&ApiError::new(
                                E_INTERNAL,
                                "the batch started no template for this scenario",
                            )),
                        },
                    };
                    *results[at].lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
                }
            });
        }
    });

    let mut from = Vec::new();
    for (text, started) in &templates {
        let args = &wanted[text];
        match started {
            Ok((id, cache)) => {
                from.push(serde_json::json!({
                    "image": args["fw"],
                    "start": args,
                    "instance": id.to_string(),
                    "boot_cache": cache,
                }));
                host.stop(*id);
            }
            Err(refused) => from.push(serde_json::json!({
                "image": args["fw"],
                "start": args,
                "error": refused.to_json(),
            })),
        }
    }
    let reports = results
        .into_iter()
        .zip(&items)
        .map(|(slot, item)| {
            slot.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or_else(|| {
                    Report::not_run(
                        &item.scenario.name,
                        &item.options.source,
                        &ApiError::new(E_INTERNAL, "no batch worker reached this scenario"),
                    )
                })
        })
        .collect();
    Ok(BatchOutcome {
        reports,
        detail: serde_json::json!({
            "forked_from": from,
            "parallel": workers,
            "wall_budget": "per_scenario",
        }),
    })
}

/// One scenario of [`batch_on`]: fork `template`, run on the fork's worker with the template's
/// `setup` keys marked as applied, stop the fork. A panic is this scenario's `E_INTERNAL`.
fn batch_one(
    host: &dyn BatchHost,
    template: InstanceId,
    item: &pemu_api::commands::scenario::BatchItem,
    started_with: &[String],
) -> pemu_api::scenario::Report {
    use pemu_api::scenario::Report;
    let not_run =
        |refused: &ApiError| Report::not_run(&item.scenario.name, &item.options.source, refused);
    let fork = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.fork(template)))
    {
        Ok(Ok(fork)) => fork,
        Ok(Err(refused)) => return not_run(&refused),
        Err(_) => return not_run(&panicked("forking")),
    };
    let mut item = item.clone();
    item.options.instance = fork.to_string();
    item.options.started_with = started_with.to_vec();
    let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.run(fork, item)))
        .unwrap_or_else(|_| Err(panicked("the scenario")))
        .unwrap_or_else(|refused| not_run(&refused));
    host.stop(fork);
    report
}

fn not_hosted(id: InstanceId) -> ApiError {
    ApiError::new(
        E_STATE,
        format!("instance `{id}` is not hosted by this daemon"),
    )
    .with_hint("`GET /v1/instances` lists the instances this daemon hosts; `start` makes one")
}

/// Whether a command's input schema has an `instance` property, cached per command name because
/// building a schema allocates its whole JSON tree on every call.
fn takes_instance(spec: &CommandSpec) -> bool {
    static KNOWN: std::sync::OnceLock<std::sync::Mutex<BTreeMap<&'static str, bool>>> =
        std::sync::OnceLock::new();
    let known = KNOWN.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()));
    if let Some(answer) = known
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(spec.name)
    {
        return *answer;
    }
    let answer = (spec.input_schema)()
        .as_value()
        .get("properties")
        .and_then(|p| p.get("instance"))
        .is_some();
    known
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(spec.name, answer);
    answer
}

fn output_instance(output: &pemu_api::output::Output) -> Option<InstanceId> {
    output
        .json
        .get("instance")
        .and_then(serde_json::Value::as_str)
        .and_then(|text| InstanceId::parse(text).ok())
}

/// The run id of a daemon process: `run-<unix seconds>-<pid>`, unique per host.
fn run_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("run-{secs}-{}", std::process::id())
}

fn publish(id: InstanceId, hub: &crate::hub::Hub, artifacts: Option<&crate::artifacts::SharedDir>) {
    pemu_api::commands::start::with_pool(|pool| {
        let lifecycle = pool
            .table()
            .get(id)
            .map(|state| state.lifecycle.as_str().to_string());
        if let Some(session) = pool.session_mut(id) {
            let now = session.now();
            let waiting = session.waiting_for_input;
            hub.collect(
                session.machine().io(),
                now,
                lifecycle.as_deref(),
                Some(waiting),
                artifacts,
            );
        }
    });
}

/// The body of a `text/plain` GET alias: the output's own `text` field when it has one, the shaped
/// text otherwise.
fn alias_text(output: &pemu_api::output::Output) -> String {
    output
        .json
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| output.text.clone())
}

fn vt_ms(output: &pemu_api::output::Output) -> String {
    (output.vt_us / 1_000).to_string()
}

fn alias_header(
    response: Response,
    header: &str,
    output: &pemu_api::output::Output,
    field: &str,
) -> Response {
    match output.json.get(field) {
        None | Some(serde_json::Value::Null) => response,
        Some(serde_json::Value::String(text)) => response.header(header, text.clone()),
        Some(other) => response.header(header, other.to_string()),
    }
}

pub fn narrow_caps(
    enabled: &BTreeSet<CapsGroup>,
    asked: &BTreeSet<CapsGroup>,
) -> BTreeSet<CapsGroup> {
    let mut caps: BTreeSet<CapsGroup> = asked.intersection(enabled).copied().collect();
    caps.insert(CapsGroup::Core);
    caps
}

/// Largest request body the daemon reads: generous for scenario files and flash overlays, bounded
/// because a peer can start a body before the token is checked.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

const SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Serves HTTP and WebSocket on `listener` until the shutdown flag is set. The listener is the one
/// [`crate::daemon::bind`] produced, so the discovery file's port is the served port.
///
/// axum only parses the request; [`Server::handle`] does the rest. The WebSocket upgrade must be
/// answered by the transport, so [`crate::ws::upgrade`] runs the same auth checks itself.
pub async fn serve(listener: std::net::TcpListener, server: Arc<Server>) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    install_batch(&server);
    let shutdown = server.shutdown().clone();
    let idle = IdleExit {
        pool: Arc::clone(server.pool()),
        relay: Arc::clone(server.relay()),
        shutdown: shutdown.clone(),
        after: server.idle_exit(),
    };
    let app = crate::relay_wisp::route(crate::attach::route(crate::ws::route(axum::Router::new())))
        .fallback(axum::routing::any(adapt))
        .with_state(Arc::clone(&server));
    let served = Arc::clone(&server);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while !shutdown.requested() {
                if idle.expired() {
                    idle.shutdown.request();
                    break;
                }
                tokio::time::sleep(SHUTDOWN_POLL).await;
            }
            // axum drains in-flight connections before `serve` returns, and a long `run` is one:
            // cancel every session first so the drain ends at the next slice.
            idle.pool.cancel_all();
        })
        .await;
    served.shutdown_now(VTime::default());
    result
}

/// [`serve`] on a two-worker tokio runtime of its own, blocking until shutdown. Two workers are
/// enough because commands run on instance threads and the blocking pool.
pub fn serve_blocking(listener: std::net::TcpListener, server: Arc<Server>) -> std::io::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("pemu-serve")
        .enable_all()
        .build()?
        .block_on(serve(listener, server))
}

/// The ten-minute idle exit: the daemon stops once it has held no instance that long, so a
/// forgotten `passportsim start` does not leave a server running for days.
struct IdleExit {
    pool: Arc<Pool>,
    /// An attached page counts as an instance, so a daemon a tab drives does not idle out.
    relay: Arc<crate::attach::Relay>,
    shutdown: Shutdown,
    after: std::time::Duration,
}

impl IdleExit {
    fn expired(&self) -> bool {
        self.relay.attached().is_empty()
            && self.pool.idle_for().is_some_and(|idle| idle >= self.after)
    }
}

async fn adapt(
    axum::extract::State(server): axum::extract::State<Arc<Server>>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let Some(method) = Method::parse(parts.method.as_str()) else {
        return into_axum(Response::json(
            405,
            &serde_json::json!({ "ok": false, "error": { "code": "E_USAGE", "message": format!("`{}` is not served", parts.method) } }),
        ));
    };
    let body = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes.to_vec(),
        Err(e) => {
            return into_axum(Response::json(
                413,
                &serde_json::json!({ "ok": false, "error": { "code": "E_USAGE", "message": format!("the request body was not read: {e}") } }),
            ));
        }
    };
    let mut headers = Headers::new();
    for (name, value) in parts.headers.iter() {
        if let Ok(value) = value.to_str() {
            headers = headers.with(name.as_str(), value);
        }
    }
    let request = Request {
        method,
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or_default().to_string(),
        headers,
        body,
    };
    // `Server::handle` blocks for a whole `run`; on a current-thread runtime it would stall the
    // health probe, `POST /v1/shutdown` and the idle-exit watcher, so it runs off the executor.
    let response = tokio::task::spawn_blocking(move || {
        // A forwarding CLI names its workspace root for `scenario` files, for this request only.
        let root = request
            .headers
            .get(crate::hooks::SCENARIO_ROOT_HEADER)
            .map(str::to_owned);
        crate::hooks::bind_call_root(root.as_deref(), || {
            // `/mcp` goes to the MCP transport: it is stateless per request, and the protocol
            // version travels in a header.
            if matches!(route(request.method, &request.path), Ok(Route::Mcp)) {
                let mcp = crate::mcp_stdio::Mcp::new(Arc::clone(&server))
                    .with_version(request.headers.get("mcp-protocol-version"));
                let mut mcp = match crate::mcp_http::with_caps_header(mcp, &request) {
                    Ok(mcp) => mcp,
                    Err(response) => return response,
                };
                return crate::mcp_http::serve(&server, &mut mcp, &request);
            }
            server.handle(&request)
        })
    })
    .await;
    match response {
        Ok(response) => into_axum(response),
        // A handler panic is a daemon bug, not a guest outcome, so it is `E_INTERNAL` rather than
        // a 200 with `ok: false`.
        Err(e) => into_axum(Response::error(&ApiError::new(
            E_INTERNAL,
            format!("the request handler did not finish: {e}"),
        ))),
    }
}

pub(crate) fn into_axum(response: Response) -> axum::response::Response {
    let mut builder = axum::http::Response::builder().status(
        axum::http::StatusCode::from_u16(response.status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
    );
    for (name, value) in &response.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(axum::body::Body::from(response.body))
        .unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::empty())
                .expect("an empty 500 always builds")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Token;
    use crate::daemon::{HEALTH_PATH, SHUTDOWN_PATH};
    use pemu_api::error::{E_TIMEOUT, E_USAGE as USAGE};
    use pemu_api::spec::{Annotations, CliShape, Example, any_schema};

    fn id(text: &str) -> InstanceId {
        InstanceId::parse(text).expect("an instance id")
    }

    fn token() -> Token {
        Token::from_bytes([0x33; 32])
    }

    fn ok_handler(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
        Ok(Output::new(
            serde_json::json!({ "echo": args }),
            "ok",
            pemu_api::receipt::Receipt::default(),
        ))
    }

    fn failing_handler(_cx: &mut HandlerCx, _args: serde_json::Value) -> Result<Output, ApiError> {
        Err(ApiError::new(E_TIMEOUT, "the wait was not met"))
    }

    /// How long `sleeps` holds its instance thread; under `daemon::PROBE_TIMEOUT`, because the
    /// test reads the answer back over the probe's own small client.
    const SLEEP_HANDLER: std::time::Duration = std::time::Duration::from_millis(900);

    static SLEEP_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn serial_handler(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
        Ok(Output {
            json: serde_json::json!({ "text": args.to_string(), "next_cursor": 42 }),
            text: "the shaped text, which the alias uses only without a `text` field".to_string(),
            artifacts: Vec::new(),
            receipt: pemu_api::receipt::Receipt::default(),
            vt_us: 12_345_000,
        })
    }

    fn ui_handler(_cx: &mut HandlerCx, _args: serde_json::Value) -> Result<Output, ApiError> {
        Ok(Output {
            json: serde_json::json!({ "text": "screen\n  label \"hello\"", "ui_rev": 7 }),
            text: String::new(),
            artifacts: Vec::new(),
            receipt: pemu_api::receipt::Receipt::default(),
            vt_us: 12_345_000,
        })
    }

    fn sleeping_handler(_cx: &mut HandlerCx, _args: serde_json::Value) -> Result<Output, ApiError> {
        SLEEP_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(SLEEP_HANDLER);
        Ok(Output::new(
            serde_json::json!({ "slept": true }),
            "slept",
            pemu_api::receipt::Receipt::default(),
        ))
    }

    use pemu_api::output::Output;

    static SPECS: &[CommandSpec] = &[
        CommandSpec {
            name: "probe",
            group: CapsGroup::Core,
            summary: "A command the route tests dispatch to.",
            input_schema: any_schema,
            output_schema: any_schema,
            annotations: Annotations {
                needs_instance: true,
                ..Annotations::EMPTY
            },
            cli: CliShape::EMPTY,
            scenario_step: None,
            examples: &[Example {
                title: "probe",
                args: "{}",
            }],
            errors: &[],
            handler: ok_handler,
        },
        CommandSpec {
            name: "waits",
            group: CapsGroup::Core,
            summary: "A command that fails for a guest reason.",
            input_schema: any_schema,
            output_schema: any_schema,
            annotations: Annotations::EMPTY,
            cli: CliShape::EMPTY,
            scenario_step: None,
            examples: &[Example {
                title: "waits",
                args: "{}",
            }],
            errors: &[],
            handler: failing_handler,
        },
        CommandSpec {
            name: "sleeps",
            group: CapsGroup::Core,
            summary: "A command that holds its instance thread, standing in for a long `run`.",
            input_schema: any_schema,
            output_schema: any_schema,
            annotations: Annotations {
                needs_instance: true,
                ..Annotations::EMPTY
            },
            cli: CliShape::EMPTY,
            scenario_step: None,
            examples: &[Example {
                title: "sleeps",
                args: "{}",
            }],
            errors: &[],
            handler: sleeping_handler,
        },
        CommandSpec {
            name: "serial",
            group: CapsGroup::Core,
            summary: "The command behind the `serial` and `serial.log` GET aliases.",
            input_schema: any_schema,
            output_schema: any_schema,
            annotations: Annotations {
                needs_instance: true,
                ..Annotations::EMPTY
            },
            cli: CliShape::EMPTY,
            scenario_step: None,
            examples: &[Example {
                title: "serial",
                args: "{}",
            }],
            errors: &[],
            handler: serial_handler,
        },
        CommandSpec {
            name: "ui",
            group: CapsGroup::Core,
            summary: "The command behind the `ui.txt` GET alias.",
            input_schema: any_schema,
            output_schema: any_schema,
            annotations: Annotations {
                needs_instance: true,
                ..Annotations::EMPTY
            },
            cli: CliShape::EMPTY,
            scenario_step: None,
            examples: &[Example {
                title: "ui",
                args: "{}",
            }],
            errors: &[],
            handler: ui_handler,
        },
        CommandSpec {
            name: "listen",
            group: CapsGroup::Radio,
            summary: "A command of a group the test server does not enable.",
            input_schema: any_schema,
            output_schema: any_schema,
            annotations: Annotations::EMPTY,
            cli: CliShape::EMPTY,
            scenario_step: None,
            examples: &[Example {
                title: "listen",
                args: "{}",
            }],
            errors: &[],
            handler: ok_handler,
        },
    ];

    fn server() -> Server {
        let pool = Arc::new(Pool::new(4));
        Server::new(
            Auth::new(token(), 8765),
            pool,
            Shutdown::new(),
            BTreeSet::from([CapsGroup::Core]),
        )
        .with_commands(SPECS)
    }

    fn authorized(method: Method, path: &str) -> Request {
        Request::new(method, path)
            .header("host", "127.0.0.1:8765")
            .header("authorization", format!("Bearer {}", token().to_hex()))
    }

    #[test]
    fn the_route_table_matches_every_row_of_the_http_surface() {
        use Method::*;
        assert_eq!(route(Get, "/v1/health"), Ok(Route::Health));
        assert_eq!(route(Get, "/v1/schema"), Ok(Route::Schema));
        assert_eq!(route(Get, "/v1/instances"), Ok(Route::Instances));
        assert_eq!(route(Post, "/v1/instances"), Ok(Route::Start));
        assert_eq!(route(Delete, "/v1/instances/p1"), Ok(Route::Stop(id("p1"))));
        assert_eq!(
            route(Post, "/v1/instances/p1/commands/run"),
            Ok(Route::Command {
                instance: id("p1"),
                name: "run".to_string()
            })
        );
        assert_eq!(
            route(Get, "/v1/instances/p1/serial"),
            Ok(Route::Serial(id("p1")))
        );
        assert_eq!(
            route(Get, "/v1/instances/p1/serial.log"),
            Ok(Route::Serial(id("p1"))),
            "the GET alias reaches the same route"
        );
        assert_eq!(
            route(Get, "/v1/instances/p1/screen.png"),
            Ok(Route::Screen(id("p1")))
        );
        assert_eq!(
            route(Get, "/v1/instances/p1/ui.txt"),
            Ok(Route::Ui(id("p1")))
        );
        assert_eq!(
            route(Get, "/v1/instances/p1/events.ndjson"),
            Ok(Route::Events(id("p1")))
        );
        assert_eq!(
            route(Get, "/v1/instances/b2/artifacts/screens/0001-boot.png"),
            Ok(Route::Artifact {
                instance: id("b2"),
                path: "screens/0001-boot.png".to_string()
            })
        );
        assert_eq!(
            route(Post, "/v1/instances/p1/lease"),
            Ok(Route::Lease(id("p1")))
        );
        assert_eq!(route(Get, "/v1/instances/p1/ws"), Ok(Route::Ws(id("p1"))));
        assert_eq!(route(Post, "/v1/scenarios:run"), Ok(Route::ScenarioRun));
        assert_eq!(route(Post, SHUTDOWN_PATH), Ok(Route::Shutdown));
        assert_eq!(route(Get, HEALTH_PATH), Ok(Route::Health));
        assert_eq!(route(Get, "/v1/attach"), Ok(Route::Attach));
        assert_eq!(route(Post, "/mcp"), Ok(Route::Mcp));
    }

    #[test]
    fn the_route_table_separates_a_wrong_method_from_a_wrong_path() {
        assert_eq!(
            route(Method::Get, "/v1/shutdown"),
            Err(RouteError::MethodNotAllowed("POST"))
        );
        assert_eq!(
            route(Method::Post, "/v1/health"),
            Err(RouteError::MethodNotAllowed("GET"))
        );
        assert_eq!(
            route(Method::Delete, "/v1/instances"),
            Err(RouteError::MethodNotAllowed("GET, POST"))
        );
        assert_eq!(route(Method::Get, "/v1/nothing"), Err(RouteError::NotFound));
        assert_eq!(route(Method::Get, "/"), Ok(Route::Shell));
        assert_eq!(
            route(Method::Post, "/"),
            Err(RouteError::MethodNotAllowed("GET"))
        );
        assert_eq!(
            route(Method::Get, "/v1/instances/p1/artifacts"),
            Err(RouteError::NotFound),
            "an artifact route needs a path below the root"
        );
        assert_eq!(
            route(Method::Delete, "/v1/instances/nope"),
            Err(RouteError::BadInstance("nope".to_string()))
        );
    }

    #[test]
    fn a_guest_outcome_is_a_200_and_a_protocol_problem_is_not() {
        assert_eq!(status_for(E_TIMEOUT), 200);
        assert_eq!(status_for(pemu_api::error::E_GUEST_PANIC), 200);
        assert_eq!(status_for(pemu_api::error::E_STUCK), 200);
        assert_eq!(status_for(USAGE), 400);
        assert_eq!(status_for(pemu_api::error::E_STALE_REF), 400);
        assert_eq!(status_for(pemu_api::error::E_SECRET_REFUSED), 403);
        assert_eq!(status_for(pemu_api::error::E_HOST_UNSUPPORTED), 403);
        assert_eq!(
            status_for(pemu_api::error::E_DAEMON),
            503,
            "the daemon error is a 503"
        );
        assert_eq!(status_for(pemu_api::error::E_PLAN_REFUSED), 403);
        assert_eq!(status_for(pemu_api::error::E_ASSET_MISSING), 404);
        assert_eq!(status_for(E_STATE), 409);
        assert_eq!(status_for(pemu_api::error::E_LEASE), 409);
        assert_eq!(status_for(pemu_api::error::E_ASSET_HASH), 422);
        assert_eq!(status_for(E_INTERNAL), 500);
    }

    #[test]
    fn an_unauthorized_request_never_reaches_the_route_table() {
        let server = server();
        let response = server
            .handle(&Request::new(Method::Post, SHUTDOWN_PATH).header("host", "127.0.0.1:8765"));
        assert_eq!(response.status, 401);
        assert!(
            !server.shutdown().requested(),
            "an unauthenticated POST must not stop the daemon"
        );
        let response = server.handle(
            &authorized(Method::Post, SHUTDOWN_PATH).header("origin", "http://evil.example"),
        );
        assert_eq!(response.status, 403);
        assert!(!server.shutdown().requested());
        // A foreign `Host` with the right token: the DNS-rebinding shape.
        let response = server.handle(
            &Request::new(Method::Post, SHUTDOWN_PATH)
                .header("host", "evil.example:8765")
                .header("authorization", format!("Bearer {}", token().to_hex())),
        );
        assert_eq!(response.status, 403);
        assert!(!server.shutdown().requested());
    }

    #[test]
    fn an_authenticated_shutdown_sets_the_flag_and_answers_first() {
        let server = server();
        let response = server.handle(&authorized(Method::Post, SHUTDOWN_PATH));
        assert_eq!(response.status, 200);
        assert!(response.text().contains("\"stopping\":true"));
        assert!(server.shutdown().requested());
    }

    #[test]
    fn health_names_the_protocol_and_the_enabled_caps() {
        let server = server();
        let response = server.handle(&authorized(Method::Get, HEALTH_PATH));
        assert_eq!(response.status, 200);
        assert_eq!(response.get("content-type"), Some(JSON));
        let body: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
        assert_eq!(body["protocol"], PROTOCOL);
        assert_eq!(body["caps"], serde_json::json!(["core"]));
    }

    #[test]
    fn the_schema_route_lists_only_commands_of_an_enabled_group() {
        let server = server();
        let body: serde_json::Value =
            serde_json::from_slice(&server.handle(&authorized(Method::Get, "/v1/schema")).body)
                .expect("JSON");
        let names: Vec<&str> = body["commands"]
            .as_array()
            .expect("array")
            .iter()
            .map(|c| c["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            ["probe", "waits", "sleeps", "serial", "ui"],
            "the Radio command is not listed"
        );
    }

    #[test]
    fn a_command_of_a_disabled_group_is_refused_even_when_its_route_is_known() {
        let server = server();
        let pool = Arc::clone(server.pool());
        let id = pool.host_for_test("p901");
        let response = server.handle(
            &authorized(Method::Post, &format!("/v1/instances/{id}/commands/listen")).body("{}"),
        );
        assert_eq!(response.status, 409);
        assert!(response.text().contains("radio"));
        pool.stop(id, VTime::default()).expect("stop");
    }

    #[test]
    fn a_command_runs_on_its_instance_thread_and_a_guest_failure_is_a_200() {
        let server = server();
        let pool = Arc::clone(server.pool());
        let id = pool.host_for_test("p901");

        let response = server.handle(
            &authorized(Method::Post, &format!("/v1/instances/{id}/commands/probe"))
                .body(r#"{"button":"ok"}"#),
        );
        assert_eq!(response.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
        assert_eq!(body["ok"], true);
        assert_eq!(body["result"]["echo"]["button"], "ok");

        let response = server.handle(&authorized(Method::Post, "/v1/instances").body("{}"));
        assert_eq!(
            response.status, 400,
            "there is no `start` in the test registry"
        );

        let response = server
            .command("waits", None, serde_json::Value::Null)
            .expect("dispatch");
        assert_eq!(response.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "E_TIMEOUT");
        pool.stop(id, VTime::default()).expect("stop");
    }

    fn server_with_instance() -> (Server, InstanceId) {
        let server = server();
        let id = server.pool().host_for_test("p901");
        (server, id)
    }

    #[test]
    fn the_text_aliases_answer_their_own_media_type_and_headers() {
        let (server, id) = server_with_instance();
        let mut req = authorized(Method::Get, &format!("/v1/instances/{id}/serial.log"));
        req.query = "cursor=120&max_bytes=4096".to_string();
        let response = server.handle(&req);
        assert_eq!(response.status, 200);
        assert_eq!(response.get("content-type"), Some(TEXT));
        assert_eq!(response.get("x-next-cursor"), Some("42"));
        assert_eq!(response.get("x-vt-ms"), Some("12345"));
        assert_eq!(
            response.text(),
            r#"{"cursor":120,"max_bytes":4096,"op":"read"}"#,
            "a number reaches the command as a number, not as the raw query string, and the GET \
             is the registry's `serial` read"
        );

        let response = server.handle(&authorized(
            Method::Get,
            &format!("/v1/instances/{id}/ui.txt"),
        ));
        assert_eq!(response.status, 200);
        assert_eq!(response.get("content-type"), Some(TEXT));
        assert_eq!(response.get("x-ui-rev"), Some("7"));
        assert_eq!(response.text(), "screen\n  label \"hello\"");
        server.pool().stop(id, VTime::default()).expect("stop");
    }

    #[test]
    fn an_absent_query_parameter_is_left_out_and_never_sent_as_null() {
        let (server, id) = server_with_instance();
        let response = server.handle(&authorized(
            Method::Get,
            &format!("/v1/instances/{id}/serial"),
        ));
        assert_eq!(response.text(), r#"{"op":"read"}"#);

        let mut req = authorized(Method::Get, &format!("/v1/instances/{id}/serial"));
        req.query = "max_bytes=4096".to_string();
        assert_eq!(
            server.handle(&req).text(),
            r#"{"max_bytes":4096,"op":"read"}"#
        );
        server.pool().stop(id, VTime::default()).expect("stop");
    }

    #[test]
    fn the_picture_alias_says_plainly_that_it_has_no_reader() {
        let (server, id) = server_with_instance();
        let response = server.handle(&authorized(
            Method::Get,
            &format!("/v1/instances/{id}/screen.png"),
        ));
        assert_eq!(response.status, 409);
        let body: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
        assert_eq!(body["error"]["code"], "E_STATE");
        assert!(response.text().contains("no artifact reader"));
        server.pool().stop(id, VTime::default()).expect("stop");
    }

    #[test]
    fn an_auth_refusal_carries_a_registered_code_and_the_usual_envelope() {
        // A code name invented here would reach an agent that could never look it up.
        let server = server();
        let response =
            server.handle(&Request::new(Method::Get, HEALTH_PATH).header("host", "127.0.0.1:8765"));
        assert_eq!(response.status, 401);
        let body: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
        assert_eq!(body["ok"], false);
        assert!(
            body["result"].is_null() && body.get("result").is_some(),
            "the same three members as every other error envelope"
        );
        let code = body["error"]["code"].as_str().expect("a code");
        assert!(
            pemu_api::error::PRE_REGISTERED
                .iter()
                .any(|registered| registered.name == code),
            "`{code}` is not a registered error code"
        );
        assert_eq!(body["error"]["number"], u64::from(USAGE.number));

        let response = server
            .handle(&authorized(Method::Get, HEALTH_PATH).header("origin", "http://evil.example"));
        assert_eq!(response.status, 403);
        let body: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
        assert_eq!(body["error"]["code"], "E_USAGE");
    }

    #[test]
    fn a_wrong_method_answers_405_with_an_allow_header() {
        let server = server();
        let response = server.handle(&authorized(Method::Get, SHUTDOWN_PATH));
        assert_eq!(response.status, 405);
        assert_eq!(response.get("allow"), Some("POST"));
        assert!(!server.shutdown().requested());
    }

    #[test]
    fn an_unknown_route_is_404_and_an_unknown_command_is_400() {
        let server = server();
        assert_eq!(
            server.handle(&authorized(Method::Get, "/v1/nope")).status,
            404
        );
        let pool = Arc::clone(server.pool());
        let id = pool.host_for_test("p901");
        let response = server.handle(
            &authorized(Method::Post, &format!("/v1/instances/{id}/commands/nope")).body("{}"),
        );
        assert_eq!(response.status, 400);
        assert!(response.text().contains("/v1/schema"));
        pool.stop(id, VTime::default()).expect("stop");
    }

    #[test]
    fn a_body_that_is_not_json_is_a_usage_error() {
        let server = server();
        let response = server.handle(&authorized(Method::Post, "/v1/scenarios:run").body("{oops"));
        assert_eq!(response.status, 400);
        assert!(response.text().contains("not JSON"));
    }

    #[test]
    fn the_served_daemon_answers_the_probe_and_stops_on_an_authenticated_shutdown() {
        use crate::daemon::{self, Liveness, Probe};

        let bound = daemon::bind(0).expect("port 0 always binds");
        let port = bound.port;
        // The `Host` and `Origin` checks follow the port really bound, so the server is built
        // after the bind.
        let server = Arc::new(
            Server::new(
                Auth::new(token(), port),
                Arc::new(Pool::new(2)),
                Shutdown::new(),
                BTreeSet::from([CapsGroup::Core]),
            )
            .with_commands(SPECS),
        );
        let listener = bound.listener;
        let served = Arc::clone(&server);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a tokio runtime");
            runtime.block_on(serve(listener, served)).expect("serve");
        });

        let discovery = daemon::Discovery::new(port, token(), std::process::id());
        let addr = discovery.addr();
        let probe = daemon::HttpProbe;
        // Readiness is the port answering with the token, never a sleep.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while probe.liveness(addr, &token()) != Liveness::Running {
            assert!(
                std::time::Instant::now() < deadline,
                "the daemon never answered its own probe"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // A foreign token is refused, which tells a live daemon from a stranger on a recycled port.
        let wrong = Token::from_bytes([0x44; 32]);
        assert_eq!(
            daemon::request(addr, "GET", daemon::HEALTH_PATH, &wrong, None)
                .expect("the port answered")
                .status,
            401
        );
        assert_eq!(probe.liveness(addr, &wrong), Liveness::Foreign);

        daemon::stop(&discovery).expect("the shutdown was accepted");
        assert!(
            daemon::wait_gone(&discovery, &probe, std::time::Duration::from_secs(10)),
            "the daemon must stop answering after `POST /v1/shutdown`"
        );
        assert!(server.shutdown().requested());
        thread.join().expect("the server thread ends");
    }

    #[test]
    fn a_command_in_flight_does_not_stall_the_health_probe() {
        use crate::daemon::{self, Liveness, Probe};

        // Regression: a handler that awaited `Pool::run_on` on the executor held a current-thread
        // runtime's only worker, so `GET /v1/health`, `POST /v1/shutdown` and the idle-exit
        // watcher all waited out the whole command.
        let bound = daemon::bind(0).expect("port 0 always binds");
        let port = bound.port;
        let server = Arc::new(
            Server::new(
                Auth::new(token(), port),
                Arc::new(Pool::new(2)),
                Shutdown::new(),
                BTreeSet::from([CapsGroup::Core]),
            )
            .with_commands(SPECS),
        );
        let instance = server.pool().host_for_test("p901");
        let listener = bound.listener;
        let served = Arc::clone(&server);
        let thread = std::thread::spawn(move || {
            // One worker, as the product's `serve` has when a machine is busy: the bug is
            // invisible with an idle worker to spare.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a tokio runtime");
            runtime.block_on(serve(listener, served)).expect("serve");
        });

        let discovery = daemon::Discovery::new(port, token(), std::process::id());
        let addr = discovery.addr();
        let probe = daemon::HttpProbe;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while probe.liveness(addr, &token()) != Liveness::Running {
            assert!(
                std::time::Instant::now() < deadline,
                "the daemon never answered its own probe"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        SLEEP_STARTED.store(false, std::sync::atomic::Ordering::SeqCst);
        let caller = std::thread::spawn(move || {
            daemon::request(
                addr,
                "POST",
                &format!("/v1/instances/{instance}/commands/sleeps"),
                &token(),
                Some(b"{}"),
            )
            .expect("the command answered")
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !SLEEP_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "the blocking command never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let start = std::time::Instant::now();
        let health = daemon::request(addr, "GET", daemon::HEALTH_PATH, &token(), None)
            .expect("the health route answered");
        let waited = start.elapsed();
        assert_eq!(health.status, 200);
        assert!(
            waited < SLEEP_HANDLER / 3,
            "the health probe waited {waited:?} behind a command that holds an instance thread \
             for {SLEEP_HANDLER:?}; the executor must never be blocked on `Pool::run_on`"
        );

        let answer = caller.join().expect("the command thread ends");
        assert_eq!(answer.status, 200);
        server.shutdown_now(VTime::default());
        thread.join().expect("the server thread ends");
    }

    #[test]
    fn the_daemon_exits_once_it_has_held_no_instance_for_the_idle_timeout() {
        use crate::daemon::{self, Liveness, Probe};

        let bound = daemon::bind(0).expect("port 0 always binds");
        let port = bound.port;
        let server = Arc::new(
            Server::new(
                Auth::new(token(), port),
                Arc::new(Pool::new(2)),
                Shutdown::new(),
                BTreeSet::from([CapsGroup::Core]),
            )
            .with_commands(SPECS)
            .with_idle_exit(std::time::Duration::from_millis(150)),
        );
        assert_eq!(
            Server::new(
                Auth::new(token(), 0),
                Arc::new(Pool::new(1)),
                Shutdown::new(),
                BTreeSet::new()
            )
            .idle_exit(),
            daemon::IDLE_EXIT,
            "the product value is ten minutes"
        );
        let listener = bound.listener;
        let served = Arc::clone(&server);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a tokio runtime");
            runtime.block_on(serve(listener, served)).expect("serve");
        });

        let discovery = daemon::Discovery::new(port, token(), std::process::id());
        let probe = daemon::HttpProbe;
        assert!(
            daemon::wait_gone(&discovery, &probe, std::time::Duration::from_secs(10)),
            "a daemon holding no instance stops on its own"
        );
        assert_ne!(
            probe.liveness(discovery.addr(), &token()),
            Liveness::Running
        );
        thread.join().expect("the server thread ends");
    }

    #[test]
    fn query_parameters_are_read_off_the_raw_query_string() {
        let mut req = Request::new(Method::Get, "/v1/instances/p1/serial");
        req.query = "cursor=120&max_bytes=4096".to_string();
        assert_eq!(req.param("cursor"), Some("120"));
        assert_eq!(req.param("max_bytes"), Some("4096"));
        assert_eq!(req.param("missing"), None);
    }

    /// Records what the batch made and stopped, and fails or panics where a test asks.
    #[derive(Default)]
    struct FakeBatch {
        room: usize,
        failing_save: Vec<&'static str>,
        panicking_run: Vec<&'static str>,
        panicking_fork: bool,
        state: std::sync::Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        next: u32,
        started: Vec<(InstanceId, serde_json::Value)>,
        forks: Vec<InstanceId>,
        stopped: Vec<InstanceId>,
        ran_with: Vec<Vec<String>>,
    }

    impl FakeBatch {
        fn mint(&self) -> InstanceId {
            let mut state = self.state.lock().expect("unpoisoned");
            state.next += 1;
            id(&format!("p{}", state.next))
        }

        fn leaks(&self) -> Vec<InstanceId> {
            let state = self.state.lock().expect("unpoisoned");
            state
                .started
                .iter()
                .map(|(id, _)| *id)
                .chain(state.forks.iter().copied())
                .filter(|made| state.stopped.iter().filter(|s| *s == made).count() != 1)
                .collect()
        }
    }

    impl BatchHost for FakeBatch {
        fn room(&self) -> usize {
            self.room
        }

        fn start(
            &self,
            args: serde_json::Value,
        ) -> Result<(InstanceId, serde_json::Value), ApiError> {
            let made = self.mint();
            self.state
                .lock()
                .expect("unpoisoned")
                .started
                .push((made, args));
            Ok((
                made,
                serde_json::json!({"hit": false, "point": "ui-settled"}),
            ))
        }

        fn save(&self, saved: InstanceId) -> Result<(), ApiError> {
            let state = self.state.lock().expect("unpoisoned");
            let fw = state
                .started
                .iter()
                .find(|(made, _)| *made == saved)
                .map(|(_, args)| args["fw"].as_str().unwrap_or_default().to_owned())
                .unwrap_or_default();
            if self.failing_save.contains(&fw.as_str()) {
                return Err(ApiError::new(E_STATE, "the save failed"));
            }
            Ok(())
        }

        fn fork(&self, _template: InstanceId) -> Result<InstanceId, ApiError> {
            assert!(!self.panicking_fork, "a fork step panicked");
            let made = self.mint();
            self.state.lock().expect("unpoisoned").forks.push(made);
            Ok(made)
        }

        fn run(
            &self,
            fork: InstanceId,
            item: pemu_api::commands::scenario::BatchItem,
        ) -> Result<pemu_api::scenario::Report, ApiError> {
            assert!(
                !self.panicking_run.contains(&item.scenario.name.as_str()),
                "a scenario step panicked"
            );
            self.state
                .lock()
                .expect("unpoisoned")
                .ran_with
                .push(item.options.started_with.clone());
            Ok(pemu_api::scenario::Report {
                name: item.scenario.name.clone(),
                source: item.options.source.clone(),
                instance: fork.to_string(),
                status: pemu_api::scenario::RunStatus::Pass,
                caveats: Vec::new(),
                vt_us: 0,
                steps: Vec::new(),
            })
        }

        fn stop(&self, stopped: InstanceId) {
            self.state.lock().expect("unpoisoned").stopped.push(stopped);
        }
    }

    fn batch_item(name: &str, image: &str, setup: &str) -> pemu_api::commands::scenario::BatchItem {
        let text = format!(
            "schema: passportsim/scenario@1\nname: {name}\nimage: {image}\n{setup}steps:\n  - delay: 1ms\n"
        );
        pemu_api::commands::scenario::BatchItem {
            scenario: pemu_api::scenario::Scenario::parse(&text).expect("the scenario reads"),
            options: pemu_api::commands::scenario::RunOptions {
                source: format!("tests/scenarios/{name}.yaml"),
                ..Default::default()
            },
        }
    }

    #[test]
    fn a_failing_save_and_a_panicking_scenario_leave_no_instance_running() {
        let host = FakeBatch {
            room: 8,
            failing_save: vec!["broken"],
            panicking_run: vec!["boom"],
            ..FakeBatch::default()
        };
        let items = vec![
            batch_item("ok", "official", ""),
            batch_item("boom", "official", ""),
            batch_item("unsaved", "broken", ""),
        ];
        let outcome = batch_on(&host, items, 4).expect("the batch answers");
        let statuses: Vec<_> = outcome.reports.iter().map(|r| r.status).collect();
        use pemu_api::scenario::RunStatus::{Error, Pass};
        assert_eq!(statuses, [Pass, Error, Error], "{:?}", outcome.reports);
        let message = |at: usize| {
            outcome.reports[at].steps[0]
                .error
                .as_ref()
                .expect("an error")["message"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        assert!(message(1).contains("panicked"), "{}", message(1));
        assert_eq!(message(2), "the save failed");
        assert_eq!(
            host.leaks(),
            Vec::<InstanceId>::new(),
            "every instance stopped once"
        );
        assert_eq!(host.state.lock().expect("unpoisoned").started.len(), 2);

        let host = FakeBatch {
            room: 8,
            panicking_fork: true,
            ..FakeBatch::default()
        };
        let outcome = batch_on(&host, vec![batch_item("ok", "official", "")], 2).expect("answers");
        assert_eq!(outcome.reports[0].status, Error);
        assert_eq!(
            host.leaks(),
            Vec::<InstanceId>::new(),
            "the template is stopped"
        );
    }

    #[test]
    fn a_batch_starts_one_template_per_boot_and_marks_the_keys_it_applied() {
        let host = FakeBatch {
            room: 16,
            ..FakeBatch::default()
        };
        let items = vec![
            batch_item("a", "official", ""),
            batch_item("b", "official", "setup: {usb: unplugged}\n"),
            batch_item(
                "c",
                "official",
                "setup: {usb: unplugged, battery: {mv: 3300}}\n",
            ),
            batch_item("d", "official", "setup: {seed: 9}\n"),
            batch_item("e", "official", "setup: {flash_seed: nvs.bin}\n"),
        ];
        let outcome = batch_on(&host, items, 8).expect("the batch answers");
        let state = host.state.lock().expect("unpoisoned");
        let started: Vec<String> = state.started.iter().map(|(_, a)| a.to_string()).collect();
        assert_eq!(started.len(), 3, "{started:?}");
        assert!(
            started
                .iter()
                .all(|a| a.contains("\"boot_cache\":\"ui-settled\""))
        );
        assert!(started.iter().any(|a| a.contains("\"usb\":\"unplugged\"")));
        assert!(started.iter().any(|a| a.contains("\"seed\":9")));
        assert_eq!(
            outcome.detail["forked_from"].as_array().map(Vec::len),
            Some(3)
        );
        let mut ran = state.ran_with.clone();
        ran.sort();
        assert_eq!(
            ran,
            [
                vec![],
                vec!["seed".to_owned()],
                vec!["usb".to_owned()],
                vec!["usb".to_owned()]
            ]
        );
        assert_eq!(
            outcome.reports[4].status,
            pemu_api::scenario::RunStatus::Error,
            "`flash_seed` has no boot to fork from"
        );
    }

    #[test]
    fn a_batch_without_room_is_refused_once() {
        let host = FakeBatch {
            room: 1,
            ..FakeBatch::default()
        };
        let refused = batch_on(&host, vec![batch_item("a", "official", "")], 8)
            .expect_err("no room for a fork beside the template");
        assert_eq!(refused.code, E_STATE);
        assert!(refused.retryable, "{refused:?}");
        assert!(host.state.lock().expect("unpoisoned").started.is_empty());
    }
    // The static routes, `/<corpus-id>.pebundle` and the launch code. One fixture serves two files
    // and one corpus id, with no host directory or real corpus behind it.

    const UI_FILES: [(&str, &str); 3] = [
        ("index.html", "<!doctype html><title>UI</title>"),
        ("main.js", "export const ui = 1;"),
        ("pemu_wasm.wasm", "\0asm not really"),
    ];

    const FIXTURE_ID: &str = "official";
    const FIXTURE_FLASH: &[u8] = b"flash bytes of the fixture";

    fn ui_server() -> Server {
        server().with_web_ui(crate::webui::WebUi::new(
            Box::new(|name: &str| {
                UI_FILES
                    .iter()
                    .find(|(file, _)| *file == name)
                    .map(|(file, body)| crate::webui::Asset {
                        content_type: crate::webui::content_type(file),
                        bytes: body.as_bytes().to_vec(),
                    })
            }),
            Box::new(|id: &str| {
                (id == FIXTURE_ID).then(|| {
                    pemu_loader::bundle::build(
                        Some(id),
                        None,
                        &[pemu_loader::bundle::BundleInput {
                            role: pemu_loader::bundle::BUNDLE_FLASH,
                            name: "official.bin",
                            bytes: FIXTURE_FLASH,
                        }],
                    )
                })
            }),
        ))
    }

    fn anonymous(method: Method, path: &str) -> Request {
        Request::new(method, path).header("host", "127.0.0.1:8765")
    }

    fn session_of(server: &Server) -> String {
        let code = server.mint_launch_code().expect("the OS has entropy");
        let response = server.handle(
            &anonymous(Method::Post, "/v1/session")
                .header("content-type", "application/json")
                .body(serde_json::json!({ "code": code }).to_string()),
        );
        assert_eq!(response.status, 200, "{}", response.text());
        let set = response.get("set-cookie").expect("a cookie is set");
        let value = set
            .strip_prefix("pemu_session=")
            .and_then(|rest| rest.split(';').next())
            .expect("the cookie names its value first");
        format!("pemu_session={value}")
    }

    fn names_nothing_private(body: &str) {
        for forbidden in [
            "/Users/",
            "/home/",
            "Application Support",
            "AppData",
            "corpus.toml",
            "data root",
            ".bin",
            ".elf",
        ] {
            assert!(
                !body.contains(forbidden),
                "a refusal must not name `{forbidden}`: {body}"
            );
        }
    }

    #[test]
    fn a_static_get_with_no_credential_is_401_and_the_bearer_token_serves_it() {
        let server = ui_server();
        for path in ["/index.html", "/main.js", "/official.pebundle"] {
            let refused = server.handle(&anonymous(Method::Get, path));
            assert_eq!(refused.status, 401, "{path} must need a credential");
            assert!(refused.body.is_empty() || refused.get("content-type") == Some(JSON));
            names_nothing_private(&refused.text());
            let served = server.handle(&authorized(Method::Get, path));
            assert_eq!(served.status, 200, "{path} with the bearer token");
        }
        let wrong = server.handle(&anonymous(Method::Get, "/index.html").header(
            "authorization",
            format!("Bearer {}", "11".repeat(31) + "12"),
        ));
        assert_eq!(wrong.status, 401);
    }

    #[test]
    fn a_launch_code_redeems_exactly_once_and_a_second_redemption_is_401() {
        let server = ui_server();
        let code = server.mint_launch_code().expect("the OS has entropy");
        let post = || {
            server.handle(
                &anonymous(Method::Post, "/v1/session")
                    .body(serde_json::json!({ "code": code }).to_string()),
            )
        };
        let first = post();
        assert_eq!(first.status, 200);
        let cookie = first.get("set-cookie").expect("the cookie of a redemption");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        assert!(!cookie.contains("Secure"), "the origin is plain http");
        assert!(!first.text().contains(&code));
        assert!(!cookie.contains(&code), "the cookie is a second secret");
        let second = post();
        assert_eq!(second.status, 401, "a launch code is single use");
        names_nothing_private(&second.text());
        assert_eq!(
            server
                .handle(
                    &anonymous(Method::Post, "/v1/session")
                        .body(serde_json::json!({ "code": "not a code" }).to_string())
                )
                .status,
            401
        );
        assert_eq!(
            server
                .handle(&anonymous(Method::Post, "/v1/session"))
                .status,
            401
        );
    }

    #[test]
    fn an_expired_launch_code_is_401() {
        let server = ui_server();
        let old = std::time::Instant::now()
            .checked_sub(crate::webui::LAUNCH_CODE_TTL + std::time::Duration::from_secs(1))
            .expect("this process has run for a minute");
        let code = server.sessions().mint_at(old).expect("the OS has entropy");
        let refused = server.handle(
            &anonymous(Method::Post, "/v1/session")
                .body(serde_json::json!({ "code": code }).to_string()),
        );
        assert_eq!(
            refused.status, 401,
            "60 seconds is the whole life of a code"
        );
        assert!(refused.get("set-cookie").is_none());
    }

    #[test]
    fn the_session_cookie_serves_a_static_get_and_a_foreign_one_does_not() {
        let server = ui_server();
        let cookie = session_of(&server);
        let served =
            server.handle(&anonymous(Method::Get, "/index.html").header("cookie", &cookie));
        assert_eq!(served.status, 200);
        assert_eq!(
            server
                .handle(&anonymous(Method::Get, "/official.pebundle").header("cookie", &cookie))
                .status,
            200,
            "the cookie is a credential for the firmware route too"
        );
        for foreign in ["pemu_session=AAAA", "other=1", "pemu_session="] {
            assert_eq!(
                server
                    .handle(&anonymous(Method::Get, "/index.html").header("cookie", foreign))
                    .status,
                401,
                "{foreign}"
            );
        }
    }

    #[test]
    fn a_foreign_host_or_origin_is_403_on_a_static_get_before_any_credential() {
        let server = ui_server();
        let cookie = session_of(&server);
        let rebound = server.handle(
            &Request::new(Method::Get, "/index.html")
                .header("host", "evil.example:8765")
                .header("authorization", format!("Bearer {}", token().to_hex())),
        );
        assert_eq!(rebound.status, 403);
        let cross = server.handle(
            &anonymous(Method::Get, "/official.pebundle")
                .header("origin", "http://evil.example")
                .header("cookie", &cookie),
        );
        assert_eq!(cross.status, 403);
        // The shell is not an exception: `Host` is checked before the route is read.
        assert_eq!(
            server
                .handle(&Request::new(Method::Get, "/").header("host", "evil.example:8765"))
                .status,
            403
        );
        assert_eq!(
            server
                .handle(
                    &anonymous(Method::Post, "/v1/session")
                        .header("origin", "http://evil.example")
                        .body("{}")
                )
                .status,
            403
        );
    }

    #[test]
    fn an_unknown_id_and_an_unknown_file_are_404_with_a_body_naming_no_host_path() {
        let server = ui_server();
        for path in [
            "/nothing.html",
            "/unknown.pebundle",
            "/pk.pebundle",
            "/receipt.json",
        ] {
            let response = server.handle(&authorized(Method::Get, path));
            assert_eq!(response.status, 404, "{path}");
            let body = response.text();
            names_nothing_private(&body);
            assert!(
                !body.contains("pk") && !body.contains(FIXTURE_ID),
                "a refusal names no corpus id the request did not carry: {body}"
            );
        }
    }

    #[test]
    fn a_path_that_tries_to_escape_is_404_and_never_reaches_the_seam() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let asks = Arc::clone(&seen);
        let bundles = Arc::clone(&seen);
        let server = server().with_web_ui(crate::webui::WebUi::new(
            Box::new(move |name: &str| {
                asks.lock().expect("unpoisoned").push(name.to_owned());
                None
            }),
            Box::new(move |id: &str| {
                bundles.lock().expect("unpoisoned").push(id.to_owned());
                None
            }),
        ));
        for path in [
            "/../../etc/passwd",
            "/..",
            "/./index.html",
            "/%2e%2e%2ftoken",
            "/..%2ftoken",
            "/payload/web/index.html",
            "/.hidden",
            "/a%00.html",
            "/token",
            "/..pebundle.pebundle",
        ] {
            let response = server.handle(&authorized(Method::Get, path));
            assert_eq!(response.status, 404, "{path} must be refused");
            names_nothing_private(&response.text());
        }
        // `/token` is a plain name, so the seam is asked and answers nothing; the other shapes
        // never get past the route table.
        assert_eq!(
            &*seen.lock().expect("unpoisoned"),
            &["token".to_owned()],
            "no escaping name reaches the host's asset or bundle function"
        );
    }

    #[test]
    fn the_shell_is_the_only_document_served_without_a_credential() {
        let server = ui_server();
        let shell = server.handle(&anonymous(Method::Get, "/"));
        assert_eq!(shell.status, 200);
        assert_eq!(shell.get("content-type"), Some("text/html; charset=utf-8"));
        let body = shell.text();
        names_nothing_private(&body);
        assert!(!body.contains(FIXTURE_ID));
        assert!(!body.contains(&token().to_hex()));
        for state in ["p1", "b1", "instance", "vt_ms", "Bearer"] {
            assert!(!body.contains(state), "the shell must not carry `{state}`");
        }
        for path in [
            "/index.html",
            "/main.js",
            "/pemu_wasm.wasm",
            "/official.pebundle",
        ] {
            assert_eq!(
                server.handle(&anonymous(Method::Get, path)).status,
                401,
                "{path} is not served without a credential"
            );
        }
        assert_eq!(
            server.handle(&anonymous(Method::Get, "/v1/health")).status,
            401
        );
        assert_eq!(
            server
                .handle(&anonymous(Method::Get, "/v1/instances"))
                .status,
            401
        );
    }

    #[test]
    fn every_static_body_is_cross_origin_isolated_and_sniff_proof() {
        let server = ui_server();
        for path in [
            "/",
            "/index.html",
            "/main.js",
            "/pemu_wasm.wasm",
            "/official.pebundle",
        ] {
            let response = if path == "/" {
                server.handle(&anonymous(Method::Get, path))
            } else {
                server.handle(&authorized(Method::Get, path))
            };
            assert_eq!(response.status, 200, "{path}");
            assert_eq!(
                response.get("cross-origin-opener-policy"),
                Some("same-origin"),
                "{path}"
            );
            assert_eq!(
                response.get("cross-origin-embedder-policy"),
                Some("require-corp"),
                "{path}"
            );
            assert_eq!(
                response.get("x-content-type-options"),
                Some("nosniff"),
                "{path}"
            );
            assert_eq!(
                response.get("cross-origin-resource-policy"),
                Some("same-origin"),
                "{path}"
            );
        }
    }

    /// `STATIC_HEADERS` is what another host copies, so it must be exactly the headers a static
    /// body carries apart from the type and `cache-control`.
    #[test]
    fn static_headers_is_the_whole_header_set_of_a_static_body() {
        let server = ui_server();
        for path in ["/", "/main.js", "/pemu_wasm.wasm", "/official.pebundle"] {
            let response = if path == "/" {
                server.handle(&anonymous(Method::Get, path))
            } else {
                server.handle(&authorized(Method::Get, path))
            };
            assert_eq!(response.status, 200, "{path}");
            let mut sent: Vec<(String, String)> = response
                .headers
                .iter()
                .filter(|(name, _)| name != "content-type" && name != "cache-control")
                .cloned()
                .collect();
            sent.sort();
            let mut listed: Vec<(String, String)> = crate::webui::STATIC_HEADERS
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
            listed.sort();
            assert_eq!(sent, listed, "{path}");
        }
    }

    #[test]
    fn a_pebundle_is_the_bundle_the_page_boots_and_its_type_is_not_guessed() {
        let server = ui_server();
        let response = server.handle(&authorized(Method::Get, "/official.pebundle"));
        assert_eq!(response.status, 200);
        assert_eq!(
            response.get("content-type"),
            Some(crate::webui::OCTET_STREAM)
        );
        let bundle = pemu_loader::bundle::Bundle::parse(&response.body)
            .expect("the route answers a `.pebundle` the loader parses");
        assert_eq!(bundle.id(), Some(FIXTURE_ID));
        assert_eq!(
            bundle.role_data(pemu_loader::bundle::BUNDLE_FLASH),
            Some(FIXTURE_FLASH)
        );
        assert_eq!(
            server
                .handle(&authorized(Method::Get, "/index.html"))
                .status,
            200
        );
    }

    #[test]
    fn a_daemon_with_no_web_ui_serves_the_shell_and_nothing_else() {
        // With no payload the shell still answers, so the person is told what happened, and every
        // file is a 404.
        let server = server();
        assert_eq!(server.handle(&anonymous(Method::Get, "/")).status, 200);
        for path in ["/index.html", "/official.pebundle"] {
            assert_eq!(server.handle(&authorized(Method::Get, path)).status, 404);
        }
    }
    /// One raw HTTP/1.1 request over loopback. `crate::daemon::request` cannot be used: it always
    /// sends the bearer token and drops the response headers (`Set-Cookie`, isolation headers).
    fn raw(
        addr: std::net::SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let mut stream = std::net::TcpStream::connect(addr).expect("the daemon accepts");
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
            addr.port()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(body) = body {
            head.push_str(&format!(
                "Content-Length: {}\r\nContent-Type: application/json\r\n",
                body.len()
            ));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .expect("the request is sent");
        if let Some(body) = body {
            stream.write_all(body.as_bytes()).expect("the body is sent");
        }
        stream.flush().expect("flushed");
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).expect("a status line");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("a status code");
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("a header line") == 0 {
                break;
            }
            let line = line.trim_end().to_owned();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
        let length: Option<usize> = headers
            .iter()
            .find(|(n, _)| n == "content-length")
            .and_then(|(_, v)| v.parse().ok());
        let mut body = Vec::new();
        match length {
            Some(n) => {
                body.resize(n, 0);
                reader.read_exact(&mut body).expect("the announced body");
            }
            None => {
                reader.read_to_end(&mut body).expect("the body");
            }
        }
        (status, headers, body)
    }

    /// The launch-code exchange over a real socket, proving the axum adapter keeps `Set-Cookie` and
    /// the isolation headers.
    #[test]
    fn the_served_daemon_hands_a_browser_a_session_and_then_the_ui() {
        let bound = crate::daemon::bind(0).expect("port 0 always binds");
        let port = bound.port;
        let server = Arc::new(
            Server::new(
                Auth::new(token(), port),
                Arc::new(Pool::new(2)),
                Shutdown::new(),
                BTreeSet::from([CapsGroup::Core]),
            )
            .with_commands(SPECS)
            .with_web_ui(crate::webui::WebUi::new(
                Box::new(|name: &str| {
                    UI_FILES
                        .iter()
                        .find(|(file, _)| *file == name)
                        .map(|(file, body)| crate::webui::Asset {
                            content_type: crate::webui::content_type(file),
                            bytes: body.as_bytes().to_vec(),
                        })
                }),
                Box::new(|id: &str| {
                    (id == FIXTURE_ID).then(|| {
                        pemu_loader::bundle::build(
                            Some(id),
                            None,
                            &[pemu_loader::bundle::BundleInput {
                                role: pemu_loader::bundle::BUNDLE_FLASH,
                                name: "official.bin",
                                bytes: FIXTURE_FLASH,
                            }],
                        )
                    })
                }),
            )),
        );
        let listener = bound.listener;
        let served = Arc::clone(&server);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a tokio runtime");
            runtime.block_on(serve(listener, served)).expect("serve");
        });
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
        let discovery = crate::daemon::Discovery::new(port, token(), std::process::id());
        let probe = crate::daemon::HttpProbe;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while {
            use crate::daemon::Probe;
            probe.liveness(discovery.addr(), &token()) != crate::daemon::Liveness::Running
        } {
            assert!(
                std::time::Instant::now() < deadline,
                "the daemon never answered its own probe"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // 1. The shell: no credential, cross-origin isolated.
        let (status, headers, body) = raw(addr, "GET", "/", &[], None);
        assert_eq!(status, 200);
        let header = |headers: &[(String, String)], name: &str| {
            headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            header(&headers, "cross-origin-opener-policy").as_deref(),
            Some("same-origin")
        );
        assert_eq!(
            header(&headers, "cross-origin-embedder-policy").as_deref(),
            Some("require-corp")
        );
        assert!(String::from_utf8_lossy(&body).contains("/v1/session"));

        let (status, _, _) = raw(addr, "GET", "/index.html", &[], None);
        assert_eq!(status, 401);

        let code = server.mint_launch_code().expect("the OS has entropy");
        let (status, headers, body) = raw(
            addr,
            "POST",
            "/v1/session",
            &[("Origin", &format!("http://127.0.0.1:{port}"))],
            Some(&serde_json::json!({ "code": code }).to_string()),
        );
        let text = String::from_utf8_lossy(&body).into_owned();
        assert_eq!(status, 200, "{text}");
        let set = header(&headers, "set-cookie").expect("the transport carries `Set-Cookie`");
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict"),
            "{set}"
        );
        assert!(!text.contains(&code), "the answer never echoes the code");
        let cookie = set.split(';').next().expect("a cookie pair").to_owned();

        let (status, headers, body) = raw(addr, "GET", "/index.html", &[("Cookie", &cookie)], None);
        assert_eq!(status, 200);
        assert_eq!(body, UI_FILES[0].1.as_bytes());
        assert_eq!(
            header(&headers, "content-type").as_deref(),
            Some("text/html; charset=utf-8")
        );
        let (status, _, body) = raw(
            addr,
            "GET",
            "/official.pebundle",
            &[("Cookie", &cookie)],
            None,
        );
        assert_eq!(status, 200);
        let bundle = pemu_loader::bundle::Bundle::parse(&body)
            .expect("the bytes on the wire are the bundle the loader parses");
        assert_eq!(bundle.id(), Some(FIXTURE_ID));
        assert_eq!(
            bundle.role_data(pemu_loader::bundle::BUNDLE_FLASH),
            Some(FIXTURE_FLASH)
        );

        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/session",
            &[],
            Some(&serde_json::json!({ "code": code }).to_string()),
        );
        assert_eq!(status, 401);

        crate::daemon::stop(&discovery).expect("the shutdown was accepted");
        assert!(crate::daemon::wait_gone(
            &discovery,
            &probe,
            std::time::Duration::from_secs(10)
        ));
        thread.join().expect("the server thread ends");
    }
}
