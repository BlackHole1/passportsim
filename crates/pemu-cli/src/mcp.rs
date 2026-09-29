//! `passportsim mcp`: the MCP stdio adapter to the daemon, spawning it if needed.
//!
//! Each line of standard input is one JSON-RPC message, posted unchanged to the daemon's `/mcp`
//! mount with the bearer token; the answer is one line on standard output, and a notification (HTTP
//! 202) writes nothing. So MCP, the CLI and the web UI share one pool. The version `initialize`
//! agreed on travels in `MCP-Protocol-Version` on every later request, because streamable HTTP is
//! stateless per request. A spawned daemon's handles all go to the null device: standard output
//! carries JSON-RPC only, and diagnostics go to standard error.
//!
//! The daemon exits after ten idle minutes, which a paused session can outlive. A request is resent
//! only when it provably reached no daemon: a refused connection, or a 401/403 from a new daemon
//! with a new token. Any other failure may come after the daemon started the call, and a
//! `tools/call` such as `run` must never run twice, so it is answered with a JSON-RPC internal
//! error.
//!
//! Each request is relayed on its own thread, so a `ping` or a cancel is not held behind a long
//! `run`; answers are written whole under a lock. `--caps` travels in the `/mcp` caps header on
//! every request, so clients with different `--caps` share one daemon.

use std::io::{BufRead, Write};
use std::sync::Mutex;
use std::time::Duration;

use clap::ArgMatches;
use pemu_api::spec::CapsGroup;
use pemu_host::daemon::{self, ClientResponse, Discovery, DiscoveryStore};

use crate::json::Value;
use crate::render::Mode;
use crate::{Outcome, exit, forward, paths, serve, tree};

/// For a request that is not a tool call (`initialize`, `tools/list`).
const RPC_TIMEOUT: Duration = Duration::from_secs(60);

pub fn run(matches: &ArgMatches) -> Outcome {
    let list = matches
        .get_one::<String>(tree::CAPS_ARG)
        .cloned()
        .unwrap_or_default();
    let caps = match pemu_host::mcp_stdio::parse_caps(&list) {
        // Only when the person who started the relay passed `--allow-device`; the daemon decides
        // again.
        Ok(caps)
            if caps.contains(&CapsGroup::Device) && !matches.get_flag(tree::ALLOW_DEVICE_ARG) =>
        {
            return serve::usage(
                "`--caps device` needs `--allow-device`; every flash still passes the checks and a \
                 person's confirmation",
            );
        }
        Ok(caps) => caps,
        Err(why) => return serve::usage(&format!("`--caps {list}`: {why}")),
    };
    let paths = paths::resolver(None);
    let store = match forward::store(&paths) {
        Ok(store) => store,
        Err(why) => return forward::infra(&why, Mode::Text),
    };
    let link = DaemonLink { store };
    let discovery = match link.connect() {
        Ok(discovery) => discovery,
        Err(why) => return forward::infra(&why, Mode::Text),
    };
    // So the client's `scenario` calls can name the files of the workspace it started in.
    let scenario_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| pemu_host::hooks::workspace_root(&cwd))
        .and_then(|root| root.to_str().map(str::to_owned));
    let relay =
        Relay::new(link, forward::caps_list(&caps), discovery).with_scenario_root(scenario_root);
    let stdin = std::io::stdin();
    match relay.pump(stdin.lock(), std::io::stdout()) {
        Ok(()) => Outcome::default(),
        // The client closed standard output: the session is over.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Outcome::default(),
        Err(e) => Outcome {
            code: exit::INFRA,
            stdout: String::new(),
            stderr: format!("error: mcp: the stdio channel failed: {e}\n"),
        },
    }
}

/// The real daemon, or a script in the tests.
pub trait Link: Sync {
    /// Spawned when none answers. Errors when no daemon could be found or started.
    fn connect(&self) -> Result<Discovery, String>;

    /// Errors when the transport failed; the kind says whether any byte could have been sent.
    fn post(
        &self,
        discovery: &Discovery,
        headers: &[(&str, &str)],
        body: &[u8],
        timeout: Duration,
    ) -> std::io::Result<ClientResponse>;
}

struct DaemonLink {
    store: DiscoveryStore<'static>,
}

impl Link for DaemonLink {
    fn connect(&self) -> Result<Discovery, String> {
        forward::ensure(&self.store)
    }

    fn post(
        &self,
        discovery: &Discovery,
        headers: &[(&str, &str)],
        body: &[u8],
        timeout: Duration,
    ) -> std::io::Result<ClientResponse> {
        daemon::request_with_headers(
            discovery.addr(),
            "POST",
            "/mcp",
            &discovery.token,
            headers,
            Some(body),
            timeout,
        )
    }
}

pub struct Relay<L: Link> {
    link: L,
    caps: String,
    scenario_root: Option<String>,
    discovery: Mutex<Discovery>,
    version: Mutex<Option<String>>,
}

enum Failure {
    /// Nothing reached a daemon: it may be sent again.
    NotDelivered(String),
    /// It may have reached a daemon: it must not be sent again.
    Unknown(String),
}

impl<L: Link> Relay<L> {
    /// Already connected to `discovery`.
    pub fn new(link: L, caps: String, discovery: Discovery) -> Relay<L> {
        Relay {
            link,
            caps,
            scenario_root: None,
            discovery: Mutex::new(discovery),
            version: Mutex::new(None),
        }
    }

    /// So the MCP client's `scenario` calls read files under `root`.
    pub fn with_scenario_root(mut self, root: Option<String>) -> Relay<L> {
        self.scenario_root = root;
        self
    }

    /// Until end of file, one thread per request. Errors when standard input or output failed.
    pub fn pump<R: BufRead, W: Write + Send>(&self, input: R, output: W) -> std::io::Result<()> {
        let output = Mutex::new(output);
        let failed: Mutex<Option<std::io::Error>> = Mutex::new(None);
        std::thread::scope(|scope| -> std::io::Result<()> {
            for line in input.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let (output, failed) = (&output, &failed);
                scope.spawn(move || {
                    let Some(answer) = self.relay(&line) else {
                        return;
                    };
                    let mut out = output.lock().unwrap_or_else(|e| e.into_inner());
                    if let Err(e) = writeln!(out, "{answer}").and_then(|()| out.flush()) {
                        failed
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get_or_insert(e);
                    }
                });
            }
            Ok(())
        })?;
        match failed.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// `None` for a notification.
    pub fn relay(&self, line: &str) -> Option<Value> {
        let parsed: Option<Value> = serde_json::from_str(line).ok();
        let method = parsed
            .as_ref()
            .and_then(|v| v.get("method"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let timeout = match (&parsed, method.as_deref()) {
            (Some(value), Some("tools/call")) => {
                forward::call_timeout(&value["params"]["arguments"])
            }
            _ => RPC_TIMEOUT,
        };
        let result = match self.send(line, timeout) {
            // Nothing was delivered: find or spawn the daemon again and send once more.
            Err(Failure::NotDelivered(first)) => match self.reconnect() {
                Ok(()) => self.send(line, timeout),
                Err(why) => Err(Failure::Unknown(format!("{first}; {why}"))),
            },
            other => other,
        };
        match result {
            Ok(Some(answer)) => {
                if method.as_deref() == Some("initialize")
                    && let Some(version) = answer["result"]["protocolVersion"].as_str()
                {
                    *self.version.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(version.to_string());
                }
                Some(answer)
            }
            Ok(None) => None,
            Err(Failure::NotDelivered(why) | Failure::Unknown(why)) => {
                // A notification has no id and must not be answered.
                let id = parsed.as_ref().and_then(|v| v.get("id")).cloned()?;
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32603, "message": format!("passportsim daemon: {why}") },
                }))
            }
        }
    }

    fn reconnect(&self) -> Result<(), String> {
        let discovery = self.link.connect()?;
        *self.discovery.lock().unwrap_or_else(|e| e.into_inner()) = discovery;
        Ok(())
    }

    /// The answer, `None` for 202, or why it failed.
    fn send(&self, line: &str, timeout: Duration) -> Result<Option<Value>, Failure> {
        let discovery = self
            .discovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let version = self
            .version
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut headers: Vec<(&str, &str)> = vec![(pemu_host::mcp_http::CAPS_HEADER, &self.caps)];
        if let Some(version) = version.as_deref() {
            headers.push((pemu_host::mcp_http::VERSION_HEADER, version));
        }
        if let Some(root) = self.scenario_root.as_deref() {
            headers.push((pemu_host::hooks::SCENARIO_ROOT_HEADER, root));
        }
        let response = match self
            .link
            .post(&discovery, &headers, line.as_bytes(), timeout)
        {
            Ok(response) => response,
            // The only transport error that proves no byte was written: the refusal answers the
            // connect, before the request exists on the wire.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                return Err(Failure::NotDelivered(format!(
                    "port {} refused the connection",
                    discovery.port
                )));
            }
            Err(e) => {
                return Err(Failure::Unknown(format!(
                    "port {} failed during the call: {e}",
                    discovery.port
                )));
            }
        };
        match response.status {
            202 => Ok(None),
            // The token is checked before any route runs, so a refusal ran nothing.
            401 | 403 => Err(Failure::NotDelivered(format!(
                "port {} refused the token (HTTP {})",
                discovery.port, response.status
            ))),
            // 200, and the 400 of a non-JSON body, both carry a JSON-RPC message.
            _ => serde_json::from_slice::<Value>(&response.body)
                .map(Some)
                .map_err(|e| {
                    Failure::Unknown(format!(
                        "port {} answered HTTP {} without JSON-RPC: {e}",
                        discovery.port, response.status
                    ))
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pemu_host::auth::Token;

    /// Answers from a script, counting posts and connects.
    struct Scripted {
        answers: Mutex<Vec<std::io::Result<ClientResponse>>>,
        posts: AtomicUsize,
        connects: AtomicUsize,
    }

    impl Scripted {
        fn new(answers: Vec<std::io::Result<ClientResponse>>) -> Scripted {
            Scripted {
                answers: Mutex::new(answers),
                posts: AtomicUsize::new(0),
                connects: AtomicUsize::new(0),
            }
        }
    }

    impl Link for Scripted {
        fn connect(&self) -> Result<Discovery, String> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(discovery(9002))
        }

        fn post(
            &self,
            _discovery: &Discovery,
            _headers: &[(&str, &str)],
            _body: &[u8],
            _timeout: Duration,
        ) -> std::io::Result<ClientResponse> {
            self.posts.fetch_add(1, Ordering::SeqCst);
            self.answers.lock().expect("lock").remove(0)
        }
    }

    fn discovery(port: u16) -> Discovery {
        Discovery::new(port, Token::from_bytes([7; 32]), 1)
    }

    fn ok(body: &str) -> std::io::Result<ClientResponse> {
        Ok(ClientResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
        })
    }

    fn io(kind: std::io::ErrorKind) -> std::io::Result<ClientResponse> {
        Err(std::io::Error::new(kind, "scripted"))
    }

    const CALL: &str = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"passport_run","arguments":{}}}"#;
    const ANSWER: &str = r#"{"jsonrpc":"2.0","id":5,"result":{"isError":false}}"#;

    fn relay(answers: Vec<std::io::Result<ClientResponse>>) -> Relay<Scripted> {
        Relay::new(Scripted::new(answers), "core".to_string(), discovery(9001))
    }

    #[test]
    fn a_request_that_may_have_been_written_is_never_sent_again() {
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::BrokenPipe,
        ] {
            let relay = relay(vec![io(kind), ok(ANSWER)]);
            let answer = relay.relay(CALL).expect("a request is answered");
            assert_eq!(answer["error"]["code"], -32603, "{kind:?}: {answer}");
            assert_eq!(answer["id"], 5);
            assert_eq!(relay.link.posts.load(Ordering::SeqCst), 1, "{kind:?}");
            assert_eq!(relay.link.connects.load(Ordering::SeqCst), 0, "{kind:?}");
        }
    }

    #[test]
    fn a_refused_connection_or_token_reconnects_and_sends_once_more() {
        for first in [
            io(std::io::ErrorKind::ConnectionRefused),
            Ok(ClientResponse {
                status: 401,
                body: Vec::new(),
            }),
            Ok(ClientResponse {
                status: 403,
                body: Vec::new(),
            }),
        ] {
            let relay = relay(vec![first, ok(ANSWER)]);
            let answer = relay.relay(CALL).expect("answered");
            assert_eq!(answer["result"]["isError"], false, "{answer}");
            assert_eq!(relay.link.posts.load(Ordering::SeqCst), 2);
            assert_eq!(relay.link.connects.load(Ordering::SeqCst), 1);
            assert_eq!(
                relay.discovery.lock().expect("lock").port,
                9002,
                "the new daemon"
            );
        }
        // Only once: a second refusal is an answer, not a loop.
        let relay = relay(vec![
            io(std::io::ErrorKind::ConnectionRefused),
            io(std::io::ErrorKind::ConnectionRefused),
            ok(ANSWER),
        ]);
        let answer = relay.relay(CALL).expect("answered");
        assert_eq!(answer["error"]["code"], -32603, "{answer}");
        assert_eq!(relay.link.posts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_notification_is_never_answered_even_when_it_fails() {
        let relay = relay(vec![io(std::io::ErrorKind::ConnectionReset)]);
        assert!(
            relay
                .relay(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
    }

    #[test]
    fn the_pump_answers_every_request_as_one_line_and_waits_at_eof() {
        let relay = relay(vec![
            ok(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}"#),
            Ok(ClientResponse {
                status: 202,
                body: Vec::new(),
            }),
        ]);
        let input = Cursor::new(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n\n\
             {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        );
        let mut out = Vec::new();
        relay.pump(input, &mut out).expect("pump");
        let text = String::from_utf8(out).expect("UTF-8");
        assert_eq!(text.lines().count(), 1, "{text}");
        assert_eq!(relay.link.posts.load(Ordering::SeqCst), 2);
    }
}
