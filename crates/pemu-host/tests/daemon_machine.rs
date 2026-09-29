//! The daemon over a real `pemu_machine::Machine` on loopback: `start`, `run`, the `serial`
//! alias, pushed `event.serial` and `event.state`, and the artifacts flushed on `stop` and on
//! `POST /v1/shutdown`.
//!
//! No corpus is needed: on an erased flash the bundled ROM prints its banner and retries the
//! flash boot. A process of its own because `backend::install` and the session pool are
//! process-wide.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pemu_api::spec::CapsGroup;
use pemu_host::auth::{Auth, Token};
use pemu_host::daemon::{self, Shutdown};
use pemu_host::http::Server;
use pemu_host::pool::Pool;
use pemu_loader::bundle::FlashImage;

fn token() -> Token {
    Token::from_bytes([0x5A; 32])
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pemu-daemon-machine-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("a temp directory");
    dir
}

struct Daemon {
    addr: SocketAddr,
    server: Arc<Server>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Daemon {
    fn start(artifacts: &Path) -> Daemon {
        pemu_host::backend::install(
            Arc::new(|fw: &str| match fw {
                "erased" => Ok(FlashImage::erased()),
                // An absolute path in the host's own form: `\` separates on Windows.
                path if Path::new(path).is_absolute()
                    && Path::new(path).file_name() == Some("erased.bin".as_ref()) =>
                {
                    Ok(FlashImage::erased())
                }
                other => Err(pemu_api::commands::start::firmware_not_found(other)),
            }),
            Some("<artifacts>".to_string()),
            pemu_host::audio_root::AudioRoot::new(artifacts.join("audio-root")),
        );
        let bound = daemon::bind(0).expect("port 0 always binds");
        let port = bound.port;
        let server = Arc::new(
            Server::new(
                Auth::new(token(), port),
                Arc::new(Pool::new(4)),
                Shutdown::new(),
                BTreeSet::from([CapsGroup::Core]),
            )
            .with_artifacts(artifacts),
        );
        let served = Arc::clone(&server);
        let listener = bound.listener;
        let thread = std::thread::spawn(move || {
            pemu_host::http::serve_blocking(listener, served).expect("serve");
        });
        Daemon {
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            server,
            thread: Some(thread),
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<serde_json::Value>) -> (u16, Vec<u8>) {
        let body = body.map(|b| b.to_string().into_bytes());
        let response = daemon::request_with_timeout(
            self.addr,
            method,
            path,
            &token(),
            body.as_deref(),
            Duration::from_secs(120),
        )
        .expect("the daemon answers");
        (response.status, response.body)
    }

    fn json(&self, method: &str, path: &str, body: serde_json::Value) -> serde_json::Value {
        let (status, bytes) = self.call(method, path, Some(body));
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        serde_json::from_slice(&bytes).expect("a JSON envelope")
    }

    fn join(mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("serve returned");
        }
    }
}

fn ws_connect(
    addr: SocketAddr,
    instance: &str,
) -> tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>> {
    use tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{addr}/v1/instances/{instance}/ws")
        .into_client_request()
        .expect("a client request");
    let headers = request.headers_mut();
    headers.insert(
        "authorization",
        format!("Bearer {}", token().to_hex())
            .parse()
            .expect("a header"),
    );
    headers.insert(
        "origin",
        format!("http://{addr}").parse().expect("a header"),
    );
    headers.insert(
        "sec-websocket-protocol",
        pemu_host::ws::SUBPROTOCOL.parse().expect("a header"),
    );
    let (socket, _) = tungstenite::connect(request).expect("the upgrade is accepted");
    if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("a read timeout");
    }
    socket
}

fn read_until(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    mut done: impl FnMut(&serde_json::Value) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match socket.read().expect("a message before the deadline") {
            tungstenite::Message::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(text.as_str()).expect("JSON");
                let stop = done(&value);
                seen.push(value);
                if stop {
                    return seen;
                }
            }
            tungstenite::Message::Binary(_) => {}
            _ => {}
        }
    }
    let methods: Vec<&serde_json::Value> = seen.iter().map(|v| &v["method"]).collect();
    panic!("the awaited message never came; saw {methods:?}");
}

type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

fn subscribe_serial(addr: SocketAddr, instance: &str) -> Socket {
    let mut socket = ws_connect(addr, instance);
    socket
        .send(tungstenite::Message::text(
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "subscribe", "params": { "topics": ["serial"] } })
                .to_string(),
        ))
        .expect("send");
    read_until(&mut socket, |v| v["id"] == 1);
    socket
}

/// A run of a virtual minute with an hour of host budget: only a shutdown ends it, so a test
/// orders events around it instead of guessing how much wall time a virtual second takes.
fn spawn_endless_run(
    addr: SocketAddr,
    instance: &str,
) -> std::thread::JoinHandle<std::io::Result<daemon::ClientResponse>> {
    let path = format!("/v1/instances/{instance}/commands/run");
    std::thread::spawn(move || {
        let body = serde_json::json!({ "for": "60s", "wall_budget_ms": 3_600_000 }).to_string();
        daemon::request_with_timeout(
            addr,
            "POST",
            &path,
            &token(),
            Some(body.as_bytes()),
            Duration::from_secs(600),
        )
    })
}

/// Waits for the ROM banner, printed at the start of the first run: once pushed, that run is
/// under way on the worker.
fn await_banner(socket: &mut Socket) {
    read_until(socket, |v| {
        v["method"] == "event.serial"
            && v["params"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("ESP-ROM:esp32c3-eco7"))
    });
}

/// The `summary.json` of `instance`, asserted to record a shutdown short of the virtual minute.
fn assert_ended_by_shutdown(run_dir: &Path, instance: &str, why: &str) {
    let summary: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run_dir.join(format!("{instance}/summary.json"))).expect("summary.json"),
    )
    .expect("JSON");
    assert_eq!(summary["reason"], "shutdown", "{summary}");
    assert_eq!(summary["instance"], instance, "{summary}");
    assert!(
        summary["final_vt_us"]
            .as_u64()
            .expect("the session was ended")
            < 60_000_000,
        "the run on {instance} reached its minute: {why} ({summary})"
    );
}

#[test]
fn a_daemon_instance_runs_the_real_machine_pushes_its_streams_and_flushes_its_artifacts() {
    let artifacts = temp("daemon");
    let daemon = Daemon::start(&artifacts);
    let run_dir = artifacts.join(daemon.server.run_id());

    let started = daemon.json(
        "POST",
        "/v1/instances",
        serde_json::json!({ "fw": "erased", "boot": "none" }),
    );
    assert_eq!(started["ok"], true, "{started}");
    let p1 = started["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_string();
    assert!(
        run_dir.join(&p1).is_dir(),
        "the instance owns its directory"
    );

    // A subscriber is pushed what the machine prints during a later `run`.
    let mut socket = ws_connect(daemon.addr, &p1);
    socket
        .send(tungstenite::Message::text(
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "subscribe", "params": { "topics": ["serial", "state"] } })
                .to_string(),
        ))
        .expect("send");
    let reply = read_until(&mut socket, |v| v["id"] == 1);
    assert_eq!(
        reply.last().expect("a reply")["result"]["topics"],
        serde_json::json!(["serial", "state"])
    );

    let ran = daemon.json(
        "POST",
        &format!("/v1/instances/{p1}/commands/run"),
        serde_json::json!({ "for": "60ms" }),
    );
    assert_eq!(ran["ok"], true, "{ran}");
    assert_eq!(
        ran["result"]["instance"], p1,
        "the route's instance reached the handler"
    );

    let pushed = read_until(&mut socket, |v| {
        v["method"] == "event.serial"
            && v["params"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("ESP-ROM:esp32c3-eco7"))
    });
    let methods: Vec<&serde_json::Value> = reply
        .iter()
        .chain(pushed.iter())
        .map(|v| &v["method"])
        .collect();
    assert!(
        methods.iter().any(|m| *m == "event.state"),
        "the lifecycle is sent on subscribing to `state`: {methods:?}"
    );

    // `run` already moved the session cursor past the banner, so the read names cursor 0.
    let (status, body) = daemon.call("GET", &format!("/v1/instances/{p1}/serial?cursor=0"), None);
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("ESP-ROM:esp32c3-eco7"));

    let stopped = daemon.json(
        "DELETE",
        &format!("/v1/instances/{p1}"),
        serde_json::json!({}),
    );
    assert_eq!(stopped["ok"], true, "{stopped}");
    let serial_log = std::fs::read(run_dir.join(format!("{p1}/serial.log"))).expect("serial.log");
    assert!(String::from_utf8_lossy(&serial_log).contains("ESP-ROM:esp32c3-eco7"));
    let summary: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run_dir.join(format!("{p1}/summary.json"))).expect("summary.json"),
    )
    .expect("JSON");
    assert_eq!(summary["reason"], "stop");
    assert_eq!(summary["instance"], p1.as_str());
    assert!(
        !daemon
            .server
            .pool()
            .live()
            .iter()
            .any(|id| id.to_string() == p1)
    );
    drop(socket);

    // An instance still live at `POST /v1/shutdown` is flushed on that path too.
    let p2 = daemon.json(
        "POST",
        "/v1/instances",
        serde_json::json!({ "fw": "erased", "boot": "none" }),
    )["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_string();
    let ran = daemon.json(
        "POST",
        &format!("/v1/instances/{p2}/commands/run"),
        serde_json::json!({ "for": "10ms" }),
    );
    assert_eq!(ran["ok"], true, "{ran}");
    let (status, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(status, 200);
    daemon.join();
    let summary: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run_dir.join(format!("{p2}/summary.json"))).expect("summary.json"),
    )
    .expect("JSON");
    assert_eq!(summary["reason"], "shutdown");
    assert!(summary["final_vt_us"].as_u64().expect("a final instant") >= 10_000);
    assert!(run_dir.join(format!("{p2}/serial.log")).is_file());
}

#[test]
fn a_firmware_the_source_does_not_know_is_refused_and_leaves_no_instance() {
    let artifacts = temp("refused");
    let daemon = Daemon::start(&artifacts);
    let (status, body) = daemon.call(
        "POST",
        "/v1/instances",
        Some(serde_json::json!({ "fw": "nonexistent", "boot": "none" })),
    );
    assert_eq!(status, 404, "a missing asset is a 404");
    let refused: serde_json::Value = serde_json::from_slice(&body).expect("an envelope");
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["code"], "E_ASSET_MISSING");
    assert!(daemon.server.pool().live().is_empty());
    let (status, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(status, 200);
    daemon.join();
}

#[test]
fn a_long_run_on_one_instance_does_not_hold_up_status_on_another() {
    // Only calls to one instance serialize. The run on p1 cannot end while the test lives, so
    // the order of events is the property: status on p2 answers while that run is in flight,
    // and only then does the shutdown end the run short of its minute.
    let artifacts = temp("parallel");
    let daemon = Daemon::start(&artifacts);
    let run_dir = artifacts.join(daemon.server.run_id());
    let start = || {
        daemon.json(
            "POST",
            "/v1/instances",
            serde_json::json!({ "fw": "erased", "boot": "none" }),
        )["result"]["instance"]
            .as_str()
            .expect("an id")
            .to_string()
    };
    let (p1, p2) = (start(), start());
    let mut socket = subscribe_serial(daemon.addr, &p1);
    let long = spawn_endless_run(daemon.addr, &p1);
    await_banner(&mut socket);

    let asked = Instant::now();
    let status = daemon.json(
        "POST",
        &format!("/v1/instances/{p2}/commands/status"),
        serde_json::json!({}),
    );
    eprintln!(
        "status on {p2} took {:?} during the run on {p1}",
        asked.elapsed()
    );
    assert_eq!(status["ok"], true, "{status}");
    assert_eq!(status["result"]["instances"][0]["instance"], p2.as_str());
    assert!(
        !long.is_finished(),
        "the run on {p1} ended before status on {p2} answered"
    );

    let (code, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(code, 200);
    daemon.join();
    let _ = long.join().expect("the run thread");
    assert_ended_by_shutdown(&run_dir, &p1, &format!("status on {p2} waited for it"));
}

#[test]
fn a_shutdown_during_a_long_run_ends_it_promptly_and_flushes_the_directory() {
    let artifacts = temp("cancel");
    let daemon = Daemon::start(&artifacts);
    let run_dir = artifacts.join(daemon.server.run_id());
    let p1 = daemon.json(
        "POST",
        "/v1/instances",
        serde_json::json!({ "fw": "erased", "boot": "none" }),
    )["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_string();

    // The shutdown is sent once the banner shows the run under way, not after a guessed sleep.
    let mut socket = subscribe_serial(daemon.addr, &p1);
    let long = spawn_endless_run(daemon.addr, &p1);
    await_banner(&mut socket);
    assert!(!long.is_finished(), "the run is in flight");

    let asked = Instant::now();
    let (status, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(status, 200);
    daemon.join();
    let took = asked.elapsed();
    eprintln!("shutdown during the run took {took:?}");
    assert!(
        took < Duration::from_secs(10),
        "the shutdown waited on the run: {took:?}"
    );

    if let Ok(response) = long.join().expect("the run thread") {
        let body: serde_json::Value = serde_json::from_slice(&response.body).expect("an envelope");
        assert_eq!(
            body["ok"], false,
            "the run did not reach its minute: {body}"
        );
        assert_eq!(body["error"]["code"], "E_STATE", "{body}");
    }
    assert_ended_by_shutdown(&run_dir, &p1, "the shutdown did not cancel it");
    assert!(run_dir.join(format!("{p1}/serial.log")).is_file());
}

#[test]
fn a_subscriber_is_pushed_serial_while_a_long_run_is_still_going() {
    // Serial is pushed after every slice, not once at the end: the run cannot end on its own, so
    // the banner arriving at all proves it came from a slice.
    let artifacts = temp("slices");
    let daemon = Daemon::start(&artifacts);
    let run_dir = artifacts.join(daemon.server.run_id());
    let p1 = daemon.json(
        "POST",
        "/v1/instances",
        serde_json::json!({ "fw": "erased", "boot": "none" }),
    )["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_string();
    let mut socket = subscribe_serial(daemon.addr, &p1);
    let long = spawn_endless_run(daemon.addr, &p1);
    await_banner(&mut socket);
    assert!(
        !long.is_finished(),
        "the banner was pushed only after the run on {p1} returned"
    );
    let (status, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(status, 200);
    daemon.join();
    let _ = long.join().expect("the run thread");
    assert_ended_by_shutdown(&run_dir, &p1, "the banner waited for the run to return");
}

#[test]
fn a_body_instance_that_contradicts_the_route_is_refused() {
    let artifacts = temp("refusals");
    let daemon = Daemon::start(&artifacts);
    let p1 = daemon.json(
        "POST",
        "/v1/instances",
        serde_json::json!({ "fw": "erased", "boot": "none" }),
    )["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_string();

    // S1: the route names p1, the body another instance.
    let (status, body) = daemon.call(
        "POST",
        &format!("/v1/instances/{p1}/commands/status"),
        Some(serde_json::json!({ "instance": "p999" })),
    );
    let refused: serde_json::Value = serde_json::from_slice(&body).expect("an envelope");
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"]["code"], "E_USAGE", "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains(&p1) && m.contains("p999")),
        "{refused}"
    );
    let same = daemon.json(
        "POST",
        &format!("/v1/instances/{p1}/commands/status"),
        serde_json::json!({ "instance": p1 }),
    );
    assert_eq!(same["ok"], true, "{same}");

    let (status, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(status, 200);
    daemon.join();
}

#[test]
fn an_instance_command_to_a_session_no_worker_serves_is_refused() {
    let artifacts = temp("unhosted");
    let daemon = Daemon::start(&artifacts);
    // A session in the process pool that no worker of this daemon serves.
    let machine = pemu_host::backend::build_machine(FlashImage::erased(), 0).expect("a machine");
    let args = pemu_api::commands::start::StartArgs {
        fw: "erased".to_owned(),
        ..pemu_api::commands::start::StartArgs::default()
    };
    let stray = pemu_api::commands::start::with_pool(|pool| pool.attach(&args, Box::new(machine)))
        .to_string();
    let (status, body) = daemon.call(
        "POST",
        &format!("/v1/instances/{stray}/commands/run"),
        Some(serde_json::json!({ "for": "1ms" })),
    );
    let refused: serde_json::Value = serde_json::from_slice(&body).expect("an envelope");
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["code"], "E_STATE", "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("not hosted by this daemon")),
        "{refused}"
    );
    assert_eq!(status, 409, "{refused}");
    let untouched = pemu_api::commands::start::with_pool(|pool| {
        pool.session(pemu_api::instance::InstanceId::parse(&stray).expect("an id"))
            .map(|s| s.now())
    });
    assert_eq!(
        untouched,
        Some(pemu_core::time::VTime(0)),
        "the stray session never ran"
    );

    let (status, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(status, 200);
    daemon.join();
}

#[test]
fn a_firmware_path_leaves_the_daemon_as_its_file_name_only() {
    let artifacts = temp("fwname");
    let daemon = Daemon::start(&artifacts);
    let run_dir = artifacts.join(daemon.server.run_id());
    let fw_path = artifacts.join("private-dir").join("erased.bin");
    let fw_text = fw_path.to_str().expect("UTF-8").to_string();
    let started = daemon.json(
        "POST",
        "/v1/instances",
        serde_json::json!({ "fw": fw_text, "boot": "none" }),
    );
    assert_eq!(started["ok"], true, "{started}");
    assert_eq!(started["result"]["image"]["fw"], "erased.bin", "{started}");
    let p1 = started["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_string();
    let status = daemon.json(
        "POST",
        &format!("/v1/instances/{p1}/commands/status"),
        serde_json::json!({}),
    );
    assert_eq!(status["result"]["instances"][0]["fw"], "erased.bin");
    let stopped = daemon.json(
        "DELETE",
        &format!("/v1/instances/{p1}"),
        serde_json::json!({}),
    );
    assert_eq!(stopped["ok"], true, "{stopped}");
    let (code, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(code, 200);
    daemon.join();

    let summary =
        std::fs::read_to_string(run_dir.join(format!("{p1}/summary.json"))).expect("summary.json");
    let private = artifacts.join("private-dir");
    for text in [
        started.to_string(),
        status.to_string(),
        stopped.to_string(),
        summary.clone(),
    ] {
        assert!(
            !text.contains(private.to_str().expect("UTF-8")) && !text.contains("private-dir"),
            "an absolute firmware path leaked: {text}"
        );
    }
    let summary: serde_json::Value = serde_json::from_str(&summary).expect("JSON");
    assert_eq!(summary["fw"], "erased.bin", "{summary}");
}

#[test]
fn a_socket_keeps_streaming_during_its_own_run_and_closes_after_its_stop() {
    let artifacts = temp("wscall");
    let daemon = Daemon::start(&artifacts);
    let p1 = daemon.json(
        "POST",
        "/v1/instances",
        serde_json::json!({ "fw": "erased", "boot": "none" }),
    )["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_string();
    let mut socket = ws_connect(daemon.addr, &p1);
    let send = |socket: &mut tungstenite::WebSocket<_>, value: serde_json::Value| {
        socket
            .send(tungstenite::Message::text(value.to_string()))
            .expect("send");
    };
    send(
        &mut socket,
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "subscribe", "params": { "topics": ["serial", "state"] } }),
    );
    read_until(&mut socket, |v| v["id"] == 1);

    // `wall_budget_ms` is stated rather than left to the 30,000 default: 1 s of virtual time
    // takes about 27 s of host time in a debug build, so a loaded host would hit `E_WALL_BUDGET`.
    send(
        &mut socket,
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "passport_run", "params": { "for": "1s", "wall_budget_ms": 300_000 } }),
    );
    let seen = read_until(&mut socket, |v| v["id"] == 2);
    let banner = seen.iter().position(|v| {
        v["method"] == "event.serial"
            && v["params"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("ESP-ROM:esp32c3-eco7"))
    });
    let reply = seen.len() - 1;
    assert!(
        banner.is_some_and(|b| b < reply),
        "the stream kept flowing while the run was awaited: {:?}",
        seen.iter().map(|v| &v["method"]).collect::<Vec<_>>()
    );
    assert_eq!(
        seen[reply]["result"]["status"], "elapsed",
        "{}",
        seen[reply]
    );

    send(
        &mut socket,
        serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "passport_stop", "params": {} }),
    );
    let seen = read_until(&mut socket, |v| {
        v["method"] == "event.state" && v["params"]["state"] == "stopped"
    });
    assert!(
        seen.iter()
            .any(|v| v["id"] == 3 && v.get("result").is_some()),
        "{seen:?}"
    );
    let closed = loop {
        match socket.read() {
            Ok(tungstenite::Message::Close(_)) | Err(_) => break true,
            Ok(_) => {}
        }
    };
    assert!(closed);
    let (status, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(status, 200);
    daemon.join();
}

/// A fork that does not fit keeps none of its copies and says so retryably, instead of reporting
/// ids no thread serves (sixteen forks on a twelve-instance daemon did).
#[test]
fn a_fork_past_the_capacity_keeps_no_copy_and_is_refused_retryably() {
    let artifacts = temp("fork-capacity");
    let daemon = Daemon::start(&artifacts);
    let p1 = daemon.json(
        "POST",
        "/v1/instances",
        serde_json::json!({ "fw": "erased", "boot": "none" }),
    )["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_string();
    let (status, body) = daemon.call(
        "POST",
        &format!("/v1/instances/{p1}/commands/snapshot"),
        Some(serde_json::json!({ "op": "fork", "name": "copy", "count": 4 })),
    );
    let refused: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(
        (status, &refused["ok"]),
        (409, &serde_json::Value::Bool(false)),
        "{refused}"
    );
    assert_eq!(refused["error"]["code"], "E_STATE", "{refused}");
    assert_eq!(refused["error"]["retryable"], true, "{refused}");
    let pool = daemon.server.pool();
    let live: Vec<String> = pool.live().iter().map(|id| id.to_string()).collect();
    assert_eq!(
        live,
        std::slice::from_ref(&p1),
        "no copy of the refused fork is left"
    );
    let listed = daemon.json("GET", "/v1/instances", serde_json::json!({}));
    assert_eq!(
        listed["result"]["instances"].as_array().map(Vec::len),
        Some(1),
        "{listed}"
    );
    let forked = daemon.json(
        "POST",
        &format!("/v1/instances/{p1}/commands/snapshot"),
        serde_json::json!({ "op": "fork", "name": "copy", "count": 3 }),
    );
    assert_eq!(forked["ok"], true, "{forked}");
    for id in forked["result"]["instances"].as_array().expect("ids") {
        let id = id.as_str().expect("an id");
        let status = daemon.json(
            "POST",
            &format!("/v1/instances/{id}/commands/status"),
            serde_json::json!({}),
        );
        assert_eq!(status["ok"], true, "{id}: {status}");
    }
    let (status, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(status, 200);
    daemon.join();
}

#[test]
fn a_fork_over_http_gets_its_own_worker_runs_there_and_counts_as_live() {
    let artifacts = temp("fork");
    let daemon = Daemon::start(&artifacts);
    let run_dir = artifacts.join(daemon.server.run_id());
    let p1 = daemon.json(
        "POST",
        "/v1/instances",
        serde_json::json!({ "fw": "erased", "boot": "none" }),
    )["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_string();
    let ran = daemon.json(
        "POST",
        &format!("/v1/instances/{p1}/commands/run"),
        serde_json::json!({ "for": "5ms" }),
    );
    assert_eq!(ran["ok"], true, "{ran}");

    let forked = daemon.json(
        "POST",
        &format!("/v1/instances/{p1}/commands/snapshot"),
        serde_json::json!({ "op": "fork", "name": "copy", "count": 1 }),
    );
    assert_eq!(forked["ok"], true, "{forked}");
    let p2 = forked["result"]["instances"][0]
        .as_str()
        .expect("a fork id")
        .to_string();
    let pool = daemon.server.pool();
    let live: Vec<String> = pool.live().iter().map(|id| id.to_string()).collect();
    assert!(live.contains(&p1) && live.contains(&p2), "{live:?}");
    let fork_id = pemu_api::instance::InstanceId::parse(&p2).expect("an id");
    assert!(
        pool.worker(fork_id).is_some(),
        "the fork has a registry worker"
    );
    let thread = pool
        .run_on(fork_id, || {
            std::thread::current().name().map(str::to_string)
        })
        .expect("the fork's mailbox answers");
    assert_eq!(
        thread.as_deref(),
        Some("pemu-instance"),
        "the thread `start_thread` names, created with INSTANCE_STACK_BYTES = {}",
        pemu_host::pool::INSTANCE_STACK_BYTES
    );
    assert_eq!(pemu_host::pool::INSTANCE_STACK_BYTES, 8 * 1024 * 1024);

    // A run on the fork runs on that worker, and the parent's virtual time does not move.
    let parent_vt = ran["result"]["vt_us"].as_u64().expect("vt");
    let fork_run = daemon.json(
        "POST",
        &format!("/v1/instances/{p2}/commands/run"),
        serde_json::json!({ "for": "60ms" }),
    );
    assert_eq!(fork_run["ok"], true, "{fork_run}");
    assert_eq!(fork_run["result"]["instance"], p2.as_str());
    // The receipt names the class of the worker that ran the slices, read back there.
    #[cfg(target_os = "macos")]
    assert_eq!(
        fork_run["result"]["receipt"]["host_qos"], "user-interactive",
        "{fork_run}"
    );
    assert!(fork_run["result"]["vt_us"].as_u64().expect("vt") >= parent_vt + 60_000);
    let status = daemon.json(
        "POST",
        &format!("/v1/instances/{p1}/commands/status"),
        serde_json::json!({}),
    );
    assert_eq!(status["result"]["instances"][0]["vt_us"], parent_vt);

    let stopped = daemon.json(
        "DELETE",
        &format!("/v1/instances/{p1}"),
        serde_json::json!({}),
    );
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert!(pool.idle_for().is_none(), "the fork is still live");
    let stopped = daemon.json(
        "DELETE",
        &format!("/v1/instances/{p2}"),
        serde_json::json!({}),
    );
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert!(pool.idle_for().is_some(), "no instance is left");
    let serial_log = std::fs::read(run_dir.join(format!("{p2}/serial.log"))).expect("serial.log");
    // The copy was taken in the ROM's flash-boot retry loop, so its run prints that loop's line.
    assert!(
        String::from_utf8_lossy(&serial_log).contains("invalid header: 0xffffffff"),
        "the fork's directory holds what its run printed ({} bytes)",
        serial_log.len()
    );
    let summary: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run_dir.join(format!("{p2}/summary.json"))).expect("summary.json"),
    )
    .expect("JSON");
    assert_eq!(summary["reason"], "stop");
    assert_eq!(summary["fw"], "erased");

    let (code, _) = daemon.call("POST", "/v1/shutdown", Some(serde_json::json!({})));
    assert_eq!(code, 200);
    daemon.join();
}
