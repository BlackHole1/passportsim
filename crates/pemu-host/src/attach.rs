//! The daemon's browser relay: a page attaches over `ws://127.0.0.1:<port>/v1/attach`, the daemon
//! mints it a `b<n>` id, and every registry call addressed to that id is proxied into the page's
//! Worker and answered by the page's own machine, never by a native one.
//!
//! The socket is upgraded after the same checks as every route, with the bearer token or the
//! session cookie (a browser cannot set `Authorization` on a WebSocket); no credential travels
//! inside the socket. Text frames only, one JSON object each, and the page speaks first:
//!
//! | Direction | Frame | Meaning |
//! |---|---|---|
//! | page to daemon | `{"hello": "passportsim-browser", "label": ".."}` | the first frame; anything else closes the socket |
//! | daemon to page | `{"attached": "b1"}` | the id the daemon minted |
//! | daemon to page | `{"id": 7, "call": "{\"cmd\":..,\"args\":..}"}` | one registry call, the JSON `pemu_call` takes |
//! | page to daemon | `{"id": 7, "ok": "<output JSON>"}` or `{"id": 7, "err": "<ApiError JSON>"}` | its answer |
//! | page to daemon | `{"lease": "take"}` or `{"lease": "release"}` | the page's UI asks for the clock lease |
//! | daemon to page | `{"lease": "held"}`, `{"lease": "released"}` or `{"lease": "refused", "holder": ".."}` | the answer |
//!
//! Ids come from the `pemu-api` instance table and are never reused. Calls to one page run one at
//! a time. An agent call holds the `agent` lease for its duration; a page holding `ui` makes every
//! non-`read_only` call a retryable `E_LEASE`. The page calls its machine `p1`, so `instance`
//! fields and whole-word mentions in answers are rewritten to `b<n>`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pemu_api::error::{ApiError, E_INTERNAL, E_STATE};
use pemu_api::instance::{InstanceId, InstanceKind, Lifecycle};
use pemu_api::lease::{LeaseHolder, LeaseTicket};
use pemu_api::output::{ArtifactRef, Output};
use pemu_api::receipt::Receipt;
use pemu_api::spec::CommandSpec;
use pemu_core::time::VTime;

use crate::http::Server;

pub const ATTACH_PATH: &str = "/v1/attach";

pub const HELLO: &str = "passportsim-browser";

pub const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The close code for an attach the daemon ended (`stop` or shutdown): the page does not reconnect
/// after it. RFC 6455 reserves 4000 to 4999 for applications.
pub const CLOSE_ENDED: u16 = 4000;

/// Longest label a page may give itself, in characters. A label is shown, never parsed.
const MAX_LABEL: usize = 64;

#[derive(Debug)]
enum Outgoing {
    Text(String),
    End(&'static str),
}

#[derive(Debug)]
enum Reply {
    Ok(String),
    Err(String),
}

struct Link {
    out: tokio::sync::mpsc::UnboundedSender<Outgoing>,
    pending: Mutex<BTreeMap<u64, std::sync::mpsc::Sender<Reply>>>,
    next: AtomicU64,
    /// Held for the whole of one call, so calls to one page run one at a time.
    serial: Mutex<()>,
    ui_lease: Mutex<Option<LeaseTicket>>,
    label: String,
}

#[derive(Default)]
pub struct Relay {
    links: Mutex<BTreeMap<InstanceId, Arc<Link>>>,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("attached", &self.attached())
            .finish()
    }
}

pub struct Attached {
    pub id: InstanceId,
    rx: tokio::sync::mpsc::UnboundedReceiver<Outgoing>,
}

impl Relay {
    pub fn new() -> Relay {
        Relay::default()
    }

    pub fn attached(&self) -> Vec<InstanceId> {
        self.lock().keys().copied().collect()
    }

    pub fn hosts(&self, id: InstanceId) -> bool {
        self.lock().contains_key(&id)
    }

    pub fn label(&self, id: InstanceId) -> Option<String> {
        self.lock().get(&id).map(|link| link.label.clone())
    }

    pub fn register(&self, label: &str) -> Attached {
        let id = pemu_api::commands::start::with_pool(|pool| pool.attach_browser());
        let (out, rx) = tokio::sync::mpsc::unbounded_channel();
        let link = Arc::new(Link {
            out,
            pending: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            serial: Mutex::new(()),
            ui_lease: Mutex::new(None),
            label: label.chars().take(MAX_LABEL).collect(),
        });
        self.lock().insert(id, link);
        Attached { id, rx }
    }

    /// Ends a page's attach: waiting calls are answered with the detach, the id becomes `stopped`
    /// and any lease it held is dropped. Idempotent.
    pub fn detach(&self, id: InstanceId) {
        let Some(link) = self.lock().remove(&id) else {
            return;
        };
        // The table goes `stopped` before any waiter wakes, so a caller retrying the id is told it
        // is stopped rather than that no such instance exists.
        let _ = pemu_api::commands::start::with_pool(|pool| pool.detach_browser(id));
        // Dropping the senders wakes every waiter with a disconnect, reported as the detach.
        link.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn close_all(&self) {
        let links: Vec<(InstanceId, Arc<Link>)> = self
            .lock()
            .iter()
            .map(|(id, link)| (*id, Arc::clone(link)))
            .collect();
        for (id, link) in links {
            let _ = link.out.send(Outgoing::End("the daemon is shutting down"));
            self.detach(id);
        }
    }

    /// Runs one registry call on the page hosting `id`. The outer `Err` is a refusal before the
    /// page was reached (or a page that went away), the inner one the page's own answer. Blocks
    /// until the page answers or detaches, so run it off the async executor.
    pub fn call(
        &self,
        id: InstanceId,
        spec: &CommandSpec,
        mut args: serde_json::Value,
    ) -> Result<Result<Output, ApiError>, ApiError> {
        let link = self.link(id)?;
        // The page's registry refuses an `instance` that is not its own machine.
        if let Some(map) = args.as_object_mut() {
            map.remove("instance");
        }
        let _serial = link.serial.lock().unwrap_or_else(|e| e.into_inner());
        let ticket = match take_agent_lease(id, spec) {
            Ok(ticket) => ticket,
            Err(refused) => return Ok(Err(refused)),
        };
        let answer = self.exchange(id, &link, spec.name, args);
        if let Some(ticket) = ticket {
            release_lease(id, ticket);
        }
        let reply = answer?;
        Ok(match reply {
            Reply::Ok(text) => output_from_page(&text).map(|output| localize_output(output, id)),
            Reply::Err(text) => Err(localize_error(error_from_page(&text), id)),
        })
    }

    fn page_lease(&self, id: InstanceId, op: &str) -> serde_json::Value {
        let Some(link) = self.lock().get(&id).cloned() else {
            return serde_json::json!({ "lease": "refused", "holder": "none" });
        };
        let mut held = link.ui_lease.lock().unwrap_or_else(|e| e.into_inner());
        match op {
            "take" => {
                if held.is_some() {
                    return serde_json::json!({ "lease": "held" });
                }
                let taken = pemu_api::commands::start::with_pool(|pool| {
                    let state = pool.table_mut().get_mut(id)?;
                    Some(state.lease.acquire(LeaseHolder::Ui, VTime(0), None))
                });
                match taken {
                    Some(Ok(ticket)) => {
                        *held = Some(ticket);
                        serde_json::json!({ "lease": "held" })
                    }
                    Some(Err(_)) => {
                        let holder = lease_holder(id).unwrap_or("none");
                        serde_json::json!({ "lease": "refused", "holder": holder })
                    }
                    None => serde_json::json!({ "lease": "refused", "holder": "none" }),
                }
            }
            "release" => {
                if let Some(ticket) = held.take() {
                    release_lease(id, ticket);
                }
                serde_json::json!({ "lease": "released" })
            }
            other => serde_json::json!({
                "lease": "refused",
                "error": format!("`{}` is not a lease operation; `take` or `release`", clip(other)),
            }),
        }
    }

    fn exchange(
        &self,
        id: InstanceId,
        link: &Link,
        name: &str,
        args: serde_json::Value,
    ) -> Result<Reply, ApiError> {
        let seq = link.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        link.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seq, tx);
        let call = serde_json::json!({ "cmd": name, "args": args }).to_string();
        let frame = serde_json::json!({ "id": seq, "call": call }).to_string();
        if link.out.send(Outgoing::Text(frame)).is_err() {
            link.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&seq);
            return Err(detached_in_flight(id));
        }
        rx.recv().map_err(|_| detached_in_flight(id))
    }

    /// Hands a page's answer to its waiting call. An answer nobody waits for is dropped.
    fn deliver(&self, id: InstanceId, seq: u64, reply: Reply) {
        let Some(link) = self.lock().get(&id).cloned() else {
            return;
        };
        let waiter = link
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&seq);
        if let Some(waiter) = waiter {
            let _ = waiter.send(reply);
        }
    }

    pub fn end(&self, id: InstanceId, reason: &'static str) {
        if let Some(link) = self.lock().get(&id).cloned() {
            let _ = link.out.send(Outgoing::End(reason));
        }
        self.detach(id);
    }

    fn link(&self, id: InstanceId) -> Result<Arc<Link>, ApiError> {
        if let Some(link) = self.lock().get(&id) {
            return Ok(Arc::clone(link));
        }
        let lifecycle = pemu_api::commands::start::with_pool(|pool| {
            pool.table().get(id).map(|state| state.lifecycle)
        });
        Err(match lifecycle {
            Some(Lifecycle::Stopped) => ApiError::new(
                E_STATE,
                format!("instance `{id}` is stopped: its page detached from the daemon"),
            )
            .with_hint(
                "a page that attaches again is given a new id; `status` lists the live instances",
            ),
            _ => ApiError::new(E_STATE, format!("no instance `{id}`"))
                .with_hint("`status` lists the live instances"),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<InstanceId, Arc<Link>>> {
        self.links.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn is_browser(id: InstanceId) -> bool {
    id.kind() == InstanceKind::Browser
}

fn detached_in_flight(id: InstanceId) -> ApiError {
    ApiError::new(
        E_STATE,
        format!(
            "instance `{id}` detached while the call was in flight: its page closed the socket"
        ),
    )
    .with_hint("the page's machine went with it; a page that attaches again is given a new id")
}

/// Takes the `agent` lease for a non-`read_only` call, or refuses naming the holder. A `read_only`
/// call runs whoever holds the lease.
fn take_agent_lease(id: InstanceId, spec: &CommandSpec) -> Result<Option<LeaseTicket>, ApiError> {
    pemu_api::commands::start::with_pool(|pool| {
        let Some(state) = pool.table_mut().get_mut(id) else {
            return Ok(None);
        };
        state
            .lease
            .check_call(LeaseHolder::Agent, spec.annotations, VTime(0))
            .map_err(|e| {
                e.with_hint(format!(
                    "the page of `{id}` holds the clock lease for its own controls; retry once it \
                     gives it back"
                ))
            })?;
        if spec.annotations.read_only {
            return Ok(None);
        }
        state
            .lease
            .acquire(LeaseHolder::Agent, VTime(0), None)
            .map(Some)
    })
}

fn release_lease(id: InstanceId, ticket: LeaseTicket) {
    pemu_api::commands::start::with_pool(|pool| {
        if let Some(state) = pool.table_mut().get_mut(id) {
            let _ = state.lease.release(ticket, VTime(0));
        }
    });
}

fn lease_holder(id: InstanceId) -> Option<&'static str> {
    pemu_api::commands::start::with_pool(|pool| {
        let state = pool.table().get(id)?;
        state.lease.holder(VTime(0)).map(LeaseHolder::as_str)
    })
}

/// A page's `ok` answer (`{json, text, artifacts, receipt, vt_us}`) read back into an [`Output`].
fn output_from_page(text: &str) -> Result<Output, ApiError> {
    let malformed = |what: &str| {
        ApiError::new(
            E_INTERNAL,
            format!("the page answered the call with a malformed result: {what}"),
        )
    };
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| malformed("it is not JSON"))?;
    let map = value
        .as_object()
        .ok_or_else(|| malformed("it is not an object"))?;
    let json = map
        .get("json")
        .cloned()
        .ok_or_else(|| malformed("no `json`"))?;
    let shown = map
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let receipt = map
        .get("receipt")
        .and_then(Receipt::from_json)
        .ok_or_else(|| malformed("no readable `receipt`"))?;
    let mut output = Output::new(json, shown, receipt);
    if let Some(vt_us) = map.get("vt_us").and_then(serde_json::Value::as_u64) {
        output.vt_us = vt_us;
    }
    for artifact in map
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let field = |key: &str| artifact.get(key).and_then(serde_json::Value::as_str);
        let (Some(path), Some(sha256), Some(media_type)) =
            (field("path"), field("sha256"), field("media_type"))
        else {
            return Err(malformed("an artifact without its path, hash or type"));
        };
        let bytes = artifact
            .get("bytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let reference = ArtifactRef::new(path, sha256, media_type, bytes)
            .map_err(|_| malformed("an artifact path that is not a relative artifact path"))?;
        output = output
            .with_artifact(reference)
            .map_err(|_| malformed("an artifact path that is not a relative artifact path"))?;
    }
    Ok(output)
}

/// A page's `err` answer: its `pemu_call` envelope, or the plain message of a machineless Worker.
fn error_from_page(text: &str) -> ApiError {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| ApiError::from_json(&value))
        .unwrap_or_else(|| {
            ApiError::new(
                E_STATE,
                format!("the page could not run the call: {}", clip(text)),
            )
        })
}

fn clip(text: &str) -> String {
    const MAX: usize = 200;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        let head: String = text.chars().take(MAX).collect();
        format!("{head}...")
    }
}

/// The page's own ids an answer names, rewritten to `id`.
fn localize_output(mut output: Output, id: InstanceId) -> Output {
    let mut local = Vec::new();
    rewrite_instances(&mut output.json, id, &mut local);
    for from in &local {
        output.text = replace_word(&output.text, from, &id.to_string());
    }
    output
}

fn localize_error(mut error: ApiError, id: InstanceId) -> ApiError {
    let mut local = Vec::new();
    rewrite_instances(&mut error.detail, id, &mut local);
    // Only a process id can be the page's own, since the wasm pool mints nothing else.
    let target = id.to_string();
    for word in words(&error.message) {
        if let Ok(named) = InstanceId::parse(word)
            && named.kind() == InstanceKind::Process
            && !local.contains(&word.to_string())
        {
            local.push(word.to_string());
        }
    }
    for from in &local {
        error.message = replace_word(&error.message, from, &target);
        if let Some(hint) = &error.hint {
            error.hint = Some(replace_word(hint, from, &target).into());
        }
    }
    error
}

fn rewrite_instances(value: &mut serde_json::Value, id: InstanceId, seen: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, member) in map.iter_mut() {
                if key == "instance"
                    && let Some(text) = member.as_str()
                    && InstanceId::parse(text).is_ok_and(|n| n.kind() == InstanceKind::Process)
                {
                    if !seen.iter().any(|s| s == text) {
                        seen.push(text.to_string());
                    }
                    *member = serde_json::Value::from(id.to_string());
                } else {
                    rewrite_instances(member, id, seen);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                rewrite_instances(item, id, seen);
            }
        }
        _ => {}
    }
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
}

fn replace_word(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(from) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + from.len()..].chars().next();
        let bounded = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
        out.push_str(&rest[..at]);
        if bounded(before) && bounded(after) {
            out.push_str(to);
        } else {
            out.push_str(from);
        }
        rest = &rest[at + from.len()..];
    }
    out.push_str(rest);
    out
}

pub fn route(router: axum::Router<Arc<Server>>) -> axum::Router<Arc<Server>> {
    router.route(ATTACH_PATH, axum::routing::any(upgrade))
}

/// The upgrade: the auth checks with the credential a page has, then the socket task.
async fn upgrade(
    axum::extract::State(server): axum::extract::State<Arc<Server>>,
    headers: axum::http::HeaderMap,
    ws: Result<
        axum::extract::ws::WebSocketUpgrade,
        axum::extract::ws::rejection::WebSocketUpgradeRejection,
    >,
) -> axum::response::Response {
    let mut request = crate::http::Request::new(crate::http::Method::Get, ATTACH_PATH);
    for (name, value) in &headers {
        if let Ok(value) = value.to_str() {
            request = request.header(name.as_str(), value);
        }
    }
    if let Err(e) = server.authorize(&request) {
        return crate::http::into_axum(crate::http::Response::unauthorized(&e));
    }
    match ws {
        Ok(ws) => ws.on_upgrade(move |socket| pump(server, socket)),
        Err(_) => crate::http::into_axum(crate::http::Response::error(&ApiError::new(
            pemu_api::error::E_USAGE,
            "`/v1/attach` is a WebSocket upgrade; a page attaches with `new WebSocket(..)`",
        ))),
    }
}

async fn pump(server: Arc<Server>, mut socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::{CloseFrame, Message};

    let relay = Arc::clone(server.relay());
    let label = match tokio::time::timeout(HELLO_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => hello_label(text.as_str()),
        _ => None,
    };
    let Some(label) = label else {
        let refusal = serde_json::json!({
            "error": format!("the first frame must be {{\"hello\": \"{HELLO}\"}}"),
        });
        let _ = socket.send(Message::Text(refusal.to_string().into())).await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    };
    let Attached { id, mut rx } = relay.register(&label);
    let hello = serde_json::json!({ "attached": id.to_string() });
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        relay.detach(id);
        return;
    }
    loop {
        tokio::select! {
            message = socket.recv() => {
                let Some(Ok(message)) = message else { break };
                match message {
                    Message::Text(text) => {
                        if let Some(reply) = page_frame(&relay, id, text.as_str())
                            && socket.send(Message::Text(reply.to_string().into())).await.is_err()
                        {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            outgoing = rx.recv() => match outgoing {
                Some(Outgoing::Text(text)) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Some(Outgoing::End(reason)) => {
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame {
                            code: CLOSE_ENDED,
                            reason: reason.into(),
                        })))
                        .await;
                    break;
                }
                None => break,
            },
        }
    }
    relay.detach(id);
}

fn hello_label(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("hello")?.as_str()? != HELLO {
        return None;
    }
    Some(
        value
            .get("label")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("browser")
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_LABEL)
            .collect(),
    )
}

/// One page frame: an answer (handed to its waiter) or a lease request (answered).
fn page_frame(relay: &Relay, id: InstanceId, text: &str) -> Option<serde_json::Value> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Some(serde_json::json!({ "error": "a frame is one JSON object" }));
    };
    if let Some(op) = value.get("lease").and_then(serde_json::Value::as_str) {
        return Some(relay.page_lease(id, op));
    }
    let seq = value.get("id").and_then(serde_json::Value::as_u64)?;
    if let Some(ok) = value.get("ok").and_then(serde_json::Value::as_str) {
        relay.deliver(id, seq, Reply::Ok(ok.to_string()));
    } else if let Some(err) = value.get("err").and_then(serde_json::Value::as_str) {
        relay.deliver(id, seq, Reply::Err(err.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_word_id_is_replaced_and_a_longer_one_is_not() {
        assert_eq!(replace_word("p1 is running", "p1", "b3"), "b3 is running");
        assert_eq!(replace_word("`p1`, p10, xp1", "p1", "b3"), "`b3`, p10, xp1");
        assert_eq!(replace_word("", "p1", "b3"), "");
    }

    #[test]
    fn instance_members_naming_a_process_id_become_the_browser_id() {
        let id = InstanceId::parse("b2").unwrap();
        let mut value = serde_json::json!({
            "instance": "p1",
            "rows": [{ "instance": "p1", "other": "p1" }],
            "keep": { "instance": "b9" },
        });
        let mut seen = Vec::new();
        rewrite_instances(&mut value, id, &mut seen);
        assert_eq!(
            value,
            serde_json::json!({
                "instance": "b2",
                "rows": [{ "instance": "b2", "other": "p1" }],
                "keep": { "instance": "b9" },
            })
        );
        assert_eq!(seen, ["p1"]);
    }

    #[test]
    fn a_hello_frame_names_its_label_and_anything_else_is_refused() {
        assert_eq!(
            hello_label(r#"{"hello":"passportsim-browser","label":"tab\u0007 one"}"#),
            Some("tab one".to_string())
        );
        assert_eq!(
            hello_label(r#"{"hello":"passportsim-browser"}"#),
            Some("browser".to_string())
        );
        assert_eq!(hello_label(r#"{"hello":"other"}"#), None);
        assert_eq!(hello_label(r#"{"id":1,"call":"{}"}"#), None);
        assert_eq!(hello_label("not json"), None);
    }

    #[test]
    fn a_page_result_reads_back_into_its_output_and_a_malformed_one_is_internal() {
        let page = serde_json::json!({
            "json": { "instance": "p1", "state": "running" },
            "text": "p1 running",
            "artifacts": [],
            "receipt": Receipt::default().to_json(),
            "vt_us": 1234,
        });
        let output = output_from_page(&page.to_string()).unwrap();
        let output = localize_output(output, InstanceId::parse("b1").unwrap());
        assert_eq!(output.json["instance"], "b1");
        assert_eq!(output.text, "b1 running");
        assert_eq!(output.vt_us, 1234);
        for bad in ["[]", "nope", r#"{"text":"x"}"#] {
            assert_eq!(output_from_page(bad).unwrap_err().code, E_INTERNAL, "{bad}");
        }
    }

    #[test]
    fn a_page_error_keeps_its_envelope_and_a_plain_message_is_a_state_error() {
        let envelope = ApiError::new(pemu_api::error::E_TIMEOUT, "instance `p1` timed out")
            .with_hint("run `p1` longer");
        let back = localize_error(
            error_from_page(&envelope.to_json_text()),
            InstanceId::parse("b4").unwrap(),
        );
        assert_eq!(back.code, pemu_api::error::E_TIMEOUT);
        assert_eq!(back.message, "instance `b4` timed out");
        assert_eq!(back.hint.as_deref(), Some("run `b4` longer"));
        let plain = error_from_page("no machine is booted");
        assert_eq!(plain.code, E_STATE);
        assert!(plain.message.contains("no machine is booted"));
    }

    // Over loopback: a real daemon, a page played by a WebSocket client.

    use crate::auth::{Auth, Token};
    use crate::daemon::Shutdown;
    use crate::pool::Pool;
    use pemu_api::spec::{Annotations, CapsGroup, CliShape, Example, HandlerCx, any_schema};
    use std::collections::BTreeSet;
    use std::net::TcpStream;

    type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

    /// The relay must never run a registry handler natively: the page's machine answers.
    fn native(_cx: &mut HandlerCx, _args: serde_json::Value) -> Result<Output, ApiError> {
        panic!("a call addressed to a browser-hosted instance ran natively")
    }

    const fn spec(name: &'static str, annotations: Annotations) -> CommandSpec {
        CommandSpec {
            name,
            group: CapsGroup::Core,
            summary: "A command the relay tests address to a page.",
            input_schema: any_schema,
            output_schema: any_schema,
            annotations,
            cli: CliShape::EMPTY,
            scenario_step: None,
            examples: &[Example {
                title: "call",
                args: "{}",
            }],
            errors: &[],
            handler: native,
        }
    }

    static SPECS: &[CommandSpec] = &[
        spec(
            "input",
            Annotations {
                needs_instance: true,
                advances_time: true,
                ..Annotations::EMPTY
            },
        ),
        spec(
            "serial",
            Annotations {
                needs_instance: true,
                read_only: true,
                ..Annotations::EMPTY
            },
        ),
        spec(
            "stop",
            Annotations {
                needs_instance: true,
                destructive: true,
                ..Annotations::EMPTY
            },
        ),
    ];

    fn token() -> Token {
        Token::from_bytes([0x3b; 32])
    }

    fn serve() -> (u16, Arc<Server>) {
        let bound = crate::daemon::bind(0).expect("port 0 always binds");
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
        let served = Arc::clone(&server);
        let listener = bound.listener;
        std::thread::spawn(move || {
            let _ = crate::http::serve_blocking(listener, served);
        });
        (port, server)
    }

    fn request(
        port: u16,
        bearer: Option<&str>,
        origin: &str,
    ) -> tungstenite::handshake::client::Request {
        use tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://127.0.0.1:{port}{ATTACH_PATH}")
            .into_client_request()
            .expect("a client request");
        let headers = request.headers_mut();
        if let Some(bearer) = bearer {
            headers.insert(
                "authorization",
                format!("Bearer {bearer}").parse().expect("a header value"),
            );
        }
        headers.insert("origin", origin.parse().expect("a header value"));
        request
    }

    fn read_json(socket: &mut Socket) -> serde_json::Value {
        match socket.read().expect("a message") {
            tungstenite::Message::Text(text) => serde_json::from_str(text.as_str()).expect("JSON"),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    fn send_json(socket: &mut Socket, value: serde_json::Value) {
        socket
            .send(tungstenite::Message::text(value.to_string()))
            .expect("send");
    }

    fn attach(port: u16) -> (Socket, InstanceId) {
        let (mut socket, _) = tungstenite::connect(request(
            port,
            Some(&token().to_hex()),
            &format!("http://127.0.0.1:{port}"),
        ))
        .expect("the attach upgrade");
        send_json(
            &mut socket,
            serde_json::json!({ "hello": HELLO, "label": "test page" }),
        );
        let attached = read_json(&mut socket);
        let id = InstanceId::parse(attached["attached"].as_str().expect("an id")).expect("an id");
        (socket, id)
    }

    fn page_output(cmd: &str, args: &serde_json::Value) -> String {
        let receipt = Receipt {
            vt_us: 4_200,
            ..Receipt::default()
        };
        serde_json::json!({
            "json": { "instance": "p1", "answered_by": "page", "cmd": cmd, "args": args },
            "text": format!("p1 answered {cmd}"),
            "artifacts": [],
            "receipt": receipt.to_json(),
            "vt_us": 4_200,
        })
        .to_string()
    }

    fn answer_one(socket: &mut Socket) -> serde_json::Value {
        let frame = read_json(socket);
        let call: serde_json::Value =
            serde_json::from_str(frame["call"].as_str().expect("a call")).expect("call JSON");
        send_json(
            socket,
            serde_json::json!({
                "id": frame["id"],
                "ok": page_output(call["cmd"].as_str().unwrap_or(""), &call["args"]),
            }),
        );
        call
    }

    type Outcome = Result<Result<Output, ApiError>, ApiError>;

    fn in_background(
        server: &Arc<Server>,
        name: &'static str,
        id: InstanceId,
        args: serde_json::Value,
    ) -> std::thread::JoinHandle<Outcome> {
        let server = Arc::clone(server);
        std::thread::spawn(move || server.command_outcome(name, Some(id), args))
    }

    #[test]
    fn a_page_attaches_as_a_b_instance_and_its_machine_answers_http_and_mcp() {
        let (port, server) = serve();
        let (mut page, id) = attach(port);
        assert!(is_browser(id), "the daemon minted `{id}`");
        assert!(server.relay().attached().contains(&id));
        assert_eq!(server.relay().label(id).as_deref(), Some("test page"));
        let lifecycle = pemu_api::commands::start::with_pool(|pool| {
            pool.table().get(id).map(|state| state.lifecycle)
        });
        assert_eq!(lifecycle, Some(Lifecycle::Running));

        let call = in_background(
            &server,
            "input",
            id,
            serde_json::json!({ "button": "down", "action": "click" }),
        );
        let seen = answer_one(&mut page);
        assert_eq!(seen["cmd"], "input");
        assert_eq!(
            seen["args"],
            serde_json::json!({ "button": "down", "action": "click" }),
            "the daemon's `instance` is not the page's, so it is not forwarded"
        );
        let output = call.join().expect("joined").expect("reached").expect("ok");
        assert_eq!(output.json["answered_by"], "page");
        assert_eq!(output.json["instance"], id.to_string());
        assert_eq!(output.text, format!("{id} answered input"));
        assert_eq!(output.vt_us, 4_200);

        let mcp_server = Arc::clone(&server);
        let mcp = std::thread::spawn(move || {
            let mut mcp = crate::mcp_stdio::Mcp::new(mcp_server);
            let request = crate::mcp_stdio::RpcRequest::parse(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "passport_serial",
                    "arguments": { "instance": id.to_string(), "op": "read" },
                },
            }))
            .expect("a request");
            mcp.handle(&request).expect("an answer")
        });
        let seen = answer_one(&mut page);
        assert_eq!(seen["cmd"], "serial");
        assert_eq!(seen["args"], serde_json::json!({ "op": "read" }));
        let answer = mcp.join().expect("joined");
        assert_eq!(answer["result"]["isError"], false, "{answer}");
        assert_eq!(
            answer["result"]["structuredContent"]["instance"],
            id.to_string()
        );
        assert_eq!(answer["result"]["structuredContent"]["answered_by"], "page");

        let listed = server.handle(
            &crate::http::Request::new(crate::http::Method::Get, "/v1/instances")
                .header("host", format!("127.0.0.1:{port}"))
                .header("authorization", format!("Bearer {}", token().to_hex())),
        );
        let body: serde_json::Value = serde_json::from_slice(&listed.body).expect("JSON");
        assert!(
            body["result"]["instances"]
                .as_array()
                .expect("a list")
                .contains(&serde_json::Value::from(id.to_string())),
            "{body}"
        );

        let call = in_background(&server, "input", id, serde_json::json!({}));
        let frame = read_json(&mut page);
        let envelope = ApiError::new(pemu_api::error::E_USAGE, "instance `p1` needs a `button`")
            .with_hint("`p1` takes `button`");
        send_json(
            &mut page,
            serde_json::json!({ "id": frame["id"], "err": envelope.to_json_text() }),
        );
        let refused = call
            .join()
            .expect("joined")
            .expect("reached")
            .expect_err("the page refused");
        assert_eq!(refused.code, pemu_api::error::E_USAGE);
        assert_eq!(refused.message, format!("instance `{id}` needs a `button`"));

        server.relay().detach(id);
    }

    #[test]
    fn an_id_no_page_holds_is_refused_and_never_runs_natively() {
        let (_port, server) = serve();
        let never = InstanceId::new(InstanceKind::Browser, 4_000_000_000).expect("an id");
        let refused = server
            .command_outcome("input", Some(never), serde_json::json!({}))
            .expect_err("no page holds it");
        assert_eq!(refused.code, E_STATE);
        assert_eq!(refused.message, format!("no instance `{never}`"));
    }

    #[test]
    fn a_page_that_detaches_mid_call_fails_the_call_and_leaves_its_id_stopped() {
        let (port, server) = serve();
        let (mut page, id) = attach(port);
        let call = in_background(&server, "input", id, serde_json::json!({ "button": "ok" }));
        let _frame = read_json(&mut page);
        page.close(None).expect("close");
        let _ = page.flush();
        let refused = call
            .join()
            .expect("joined")
            .expect_err("the page went away");
        assert_eq!(refused.code, E_STATE);
        assert!(
            refused
                .message
                .contains("detached while the call was in flight"),
            "{}",
            refused.message
        );
        let later = server
            .command_outcome("serial", Some(id), serde_json::json!({}))
            .expect_err("the id is gone");
        assert_eq!(later.code, E_STATE);
        assert!(later.message.contains("is stopped"), "{}", later.message);
        assert!(!server.relay().hosts(id));
        let lifecycle = pemu_api::commands::start::with_pool(|pool| {
            pool.table().get(id).map(|state| state.lifecycle)
        });
        assert_eq!(lifecycle, Some(Lifecycle::Stopped));
    }

    #[test]
    fn the_ui_lease_refuses_agent_input_but_not_a_read_and_waits_for_an_agent_call() {
        let (port, server) = serve();
        let (mut page, id) = attach(port);

        send_json(&mut page, serde_json::json!({ "lease": "take" }));
        assert_eq!(read_json(&mut page), serde_json::json!({ "lease": "held" }));
        let refused = server
            .command_outcome("input", Some(id), serde_json::json!({ "button": "ok" }))
            .expect("reached")
            .expect_err("the page holds the lease");
        assert_eq!(refused.code, pemu_api::error::E_LEASE);
        assert!(refused.retryable);
        assert!(refused.message.contains("ui"), "{}", refused.message);

        let read = in_background(&server, "serial", id, serde_json::json!({ "op": "read" }));
        answer_one(&mut page);
        assert!(read.join().expect("joined").expect("reached").is_ok());

        send_json(&mut page, serde_json::json!({ "lease": "release" }));
        assert_eq!(
            read_json(&mut page),
            serde_json::json!({ "lease": "released" })
        );

        let call = in_background(&server, "input", id, serde_json::json!({ "button": "ok" }));
        let frame = read_json(&mut page);
        send_json(&mut page, serde_json::json!({ "lease": "take" }));
        assert_eq!(
            read_json(&mut page),
            serde_json::json!({ "lease": "refused", "holder": "agent" })
        );
        send_json(
            &mut page,
            serde_json::json!({ "id": frame["id"], "ok": page_output("input", &serde_json::json!({})) }),
        );
        assert!(call.join().expect("joined").expect("reached").is_ok());
        send_json(&mut page, serde_json::json!({ "lease": "take" }));
        assert_eq!(read_json(&mut page), serde_json::json!({ "lease": "held" }));

        server.relay().detach(id);
    }

    #[test]
    fn stop_ends_the_attach_with_the_close_code_the_page_does_not_reconnect_after() {
        let (port, server) = serve();
        let (mut page, id) = attach(port);
        let call = in_background(&server, "stop", id, serde_json::json!({}));
        answer_one(&mut page);
        assert!(call.join().expect("joined").expect("reached").is_ok());
        loop {
            match page.read() {
                Ok(tungstenite::Message::Close(Some(frame))) => {
                    assert_eq!(u16::from(frame.code), CLOSE_ENDED);
                    break;
                }
                Ok(tungstenite::Message::Close(None)) => panic!("closed without the code"),
                Ok(_) => {}
                Err(e) => panic!("the socket ended without a close frame: {e}"),
            }
        }
        assert!(!server.relay().hosts(id));
    }

    #[test]
    fn the_upgrade_takes_the_credential_and_origin_and_nothing_in_band() {
        let (port, _server) = serve();
        let own = format!("http://127.0.0.1:{port}");
        let wrong = "00".repeat(32);
        let right = token().to_hex();
        for (bearer, origin, want) in [
            (None, own.as_str(), 401),
            (Some(wrong.as_str()), own.as_str(), 401),
            (Some(right.as_str()), "http://evil.example", 403),
        ] {
            match tungstenite::connect(request(port, bearer, origin)) {
                Err(tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status().as_u16(), want, "{bearer:?} {origin}");
                }
                Err(other) => panic!("{bearer:?} {origin}: {other}"),
                Ok(_) => panic!("{bearer:?} {origin}: the upgrade was accepted"),
            }
        }
        // A first frame that is not a hello (a token in band, say) gets no id.
        let (mut socket, _) =
            tungstenite::connect(request(port, Some(&right), &own)).expect("upgrade");
        send_json(&mut socket, serde_json::json!({ "token": right }));
        let refusal = read_json(&mut socket);
        assert!(refusal["error"].as_str().is_some(), "{refusal}");
    }
}
