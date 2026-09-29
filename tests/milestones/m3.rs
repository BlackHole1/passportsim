//! Milestone M3 tests: the `pk`, `official` and `goldminer` boots, the M3 probes, determinism and
//! restore equivalence, the agent loop, the USJ console of `pk`, and the console TX pacing.
//! Names use the prefix `t<tier>_m3_` so `xtask ci` can count them.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;
use common::{build_args, image_machine, passportsim, workspace};
// The host legs of the determinism harness, shared with m1.rs.
#[allow(dead_code)]
mod determinism;
// The QEMU oracle side of the T2 write-stream comparison, shared with m1.rs.
mod oracle;

use std::collections::BTreeSet;
use std::io::Cursor;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pemu_api::spec::CapsGroup;
use pemu_core::hostio::{EventKind, SerialStream};
use pemu_core::input::InputEvent;
use pemu_core::snap::{SnapOpts, Snapshot};
use pemu_core::time::VTime;
use pemu_host::auth::{Auth, Token};
use pemu_host::daemon::{self, Shutdown};
use pemu_host::http::Server;
use pemu_host::mcp_stdio::{self, Mcp};
use pemu_host::pool::Pool;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::elf::ElfInfo;
use pemu_machine::config::{Assets, MachineConfig, TimingProfileId};
use pemu_machine::determinism::{Since, Variant, hang_digest, hex, report, report_since};
use pemu_machine::executor::Executor;
use pemu_machine::hang::StuckKind;
use pemu_machine::machine::{At, Machine};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet, Watch};
use pemu_soc_c3::periph::BLOCKS;
use serde_json::Value;

/// The merged image of the `pk` corpus id.
const PK_IMAGE: &str = "FoloToy-AI-Passport-8MB.bin";

/// The app ELF of the `pk` corpus id, and by the same name of `official`: it names pcs, and it is
/// run identity (the HLE binding) of the machines that load it.
const PK_APP_ELF: &str = "FoloToy-AI-Passport.elf";

const LOOP: [&str; 5] = ["start", "run", "serial", "status", "stop"];

/// Virtual budget of the `run`. The device prints `bsp_i2c` at 196 ms to 211 ms (dev:L61) and
/// `entry 0x` at about 24 ms (dev:L14).
const RUN_TIMEOUT: &str = "1s";

/// Host budget of the `run`: generous, so a slow debug build fails on the guest, not the host.
const WALL_BUDGET_MS: u64 = 900_000;

fn token() -> Token {
    Token::from_bytes([0x3E; 32])
}

// ---------------------------------------------------------------------------------------------
// Schema validation
// ---------------------------------------------------------------------------------------------

fn output_schema(command: &str) -> Value {
    let path = workspace().join(format!("docs/schema/commands/{command}.output.json"));
    let committed: Value = serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|e| panic!("{command}.output.json: {e}")),
    )
    .expect("a generated schema is JSON");
    let spec = pemu_api::registry::find(command).expect("a registered command");
    let live: Value = (spec.output_schema)().as_value().clone();
    assert_eq!(
        committed, live,
        "docs/schema/commands/{command}.output.json is stale; run `cargo xtask docs`"
    );
    committed
}

fn error_schema() -> Value {
    serde_json::from_slice(
        &std::fs::read(workspace().join("docs/schema/error@1.json")).expect("error@1.json"),
    )
    .expect("JSON")
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        _ => false,
    }
}

/// Every problem of `value` against `schema`, as `pointer: reason` lines. It covers the keywords
/// the generated schemas use; `pattern` is not checked, as in `xtask docs`.
fn validate(schema: &Value, value: &Value, at: &str, problems: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else {
        if schema == &Value::Bool(false) {
            problems.push(format!("{at}: the schema is `false`"));
        }
        return;
    };
    if let Some(ty) = schema.get("type") {
        let ok = match ty {
            Value::String(name) => type_matches(name, value),
            Value::Array(names) => names
                .iter()
                .filter_map(Value::as_str)
                .any(|n| type_matches(n, value)),
            _ => true,
        };
        if !ok {
            problems.push(format!("{at}: expected type {ty}, got {value}"));
            return;
        }
    }
    if let Some(Value::Array(options)) = schema.get("enum")
        && !options.contains(value)
    {
        problems.push(format!("{at}: {value} is not one of {options:?}"));
    }
    if let (Some(min), Some(n)) = (
        schema.get("minimum").and_then(Value::as_f64),
        value.as_f64(),
    ) && n < min
    {
        problems.push(format!("{at}: {n} is below {min}"));
    }
    if let (Some(max), Some(n)) = (
        schema.get("maximum").and_then(Value::as_f64),
        value.as_f64(),
    ) && n > max
    {
        problems.push(format!("{at}: {n} is above {max}"));
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(branches)) = schema.get(key) {
            let passing = branches
                .iter()
                .filter(|b| {
                    let mut p = Vec::new();
                    validate(b, value, at, &mut p);
                    p.is_empty()
                })
                .count();
            let ok = if key == "oneOf" {
                passing == 1
            } else {
                passing >= 1
            };
            if !ok {
                problems.push(format!("{at}: {passing} branches of {key} match"));
            }
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(Value::Array(required)) = schema.get("required") {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    problems.push(format!("{at}: `{key}` is required"));
                }
            }
        }
        for (key, item) in object {
            match properties.and_then(|p| p.get(key)) {
                Some(sub) => validate(sub, item, &format!("{at}/{key}"), problems),
                None => match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        problems.push(format!("{at}: `{key}` is not allowed"));
                    }
                    Some(sub @ Value::Object(_)) => {
                        validate(sub, item, &format!("{at}/{key}"), problems);
                    }
                    _ => {}
                },
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        for (i, item) in array.iter().enumerate() {
            validate(items, item, &format!("{at}/{i}"), problems);
        }
    }
}

#[track_caller]
fn assert_valid(what: &str, schema: &Value, value: &Value) {
    let mut problems = Vec::new();
    validate(schema, value, "", &mut problems);
    assert!(
        problems.is_empty(),
        "{what} does not validate against its generated schema: {problems:?}"
    );
}

#[track_caller]
fn check_outcome(command: &str, ok: bool, result: &Value, error: &Value) {
    if ok {
        assert_valid(command, &output_schema(command), result);
    } else {
        assert_valid(&format!("{command} error"), &error_schema(), error);
    }
}

// ---------------------------------------------------------------------------------------------
// The daemon
// ---------------------------------------------------------------------------------------------

fn pk_image(test: &str) -> Option<Arc<Vec<u8>>> {
    let path = common::corpus_file_or_skip(test, common::PK, PK_IMAGE)?;
    static IMAGE: OnceLock<Arc<Vec<u8>>> = OnceLock::new();
    Some(Arc::clone(IMAGE.get_or_init(|| {
        Arc::new(std::fs::read(path).expect("the verified corpus file is readable"))
    })))
}

struct Daemon {
    addr: SocketAddr,
    server: Arc<Server>,
    artifacts: PathBuf,
}

impl Daemon {
    fn start(image: Arc<Vec<u8>>) -> Daemon {
        let artifacts = std::env::temp_dir().join(format!(
            "pemu-agent-loop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&artifacts).expect("a temp artifacts root");
        pemu_host::backend::install(
            Arc::new(move |fw: &str| match fw {
                common::PK => pemu_host::backend::merged_image(&image),
                other => Err(pemu_api::commands::start::firmware_not_found(other)),
            }),
            Some("<artifacts>".to_string()),
            pemu_host::audio_root::AudioRoot::new(artifacts.join("audio")),
        );
        let bound = daemon::bind(0).expect("port 0 always binds");
        let port = bound.port;
        let server = Arc::new(
            Server::new(
                Auth::new(token(), port),
                Arc::new(Pool::new(8)),
                Shutdown::new(),
                BTreeSet::from([CapsGroup::Core]),
            )
            .with_artifacts(&artifacts),
        );
        let served = Arc::clone(&server);
        let listener = bound.listener;
        std::thread::spawn(move || pemu_host::http::serve_blocking(listener, served));
        Daemon {
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            server,
            artifacts,
        }
    }

    fn http(&self, method: &str, path: &str, body: &Value) -> Value {
        let response = daemon::request_with_timeout(
            self.addr,
            method,
            path,
            &token(),
            Some(body.to_string().as_bytes()),
            Duration::from_secs(1_800),
        )
        .expect("the daemon answers");
        serde_json::from_slice(&response.body).unwrap_or_else(|e| {
            panic!(
                "{method} {path} answered {} with no JSON envelope: {e}",
                response.status
            )
        })
    }
}

struct LoopRun {
    matched: bool,
    run: Value,
}

fn check_loop_fields(
    id: &str,
    start: &Value,
    run: Option<&Value>,
    serial: &Value,
    status: &Value,
    stop: &Value,
) {
    assert!(
        id.strip_prefix('p')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        "a server-minted id: {id}"
    );
    assert_eq!(start["state"], "paused", "start: {start}");
    assert_eq!(start["image"]["fw"], common::PK, "start: {start}");
    assert_eq!(start["boot"]["status"], "skipped", "boot none: {start}");
    assert_eq!(start["vt_us"], 0, "start: {start}");
    if let Some(run) = run {
        assert_eq!(run["instance"], id, "run: {run}");
        assert_eq!(run["result"], "pass", "run: {run}");
        assert_eq!(run["match"]["source"], "serial", "run: {run}");
        assert!(run["vt_us"].as_u64().is_some_and(|vt| vt > 0), "run: {run}");
        assert_eq!(
            status["instances"][0]["vt_us"], run["vt_us"],
            "status: {status}"
        );
    }
    assert_eq!(serial["instance"], id, "serial: {serial}");
    assert_eq!(serial["op"], "read", "serial: {serial}");
    assert_eq!(serial["cursor"], 0, "serial: {serial}");
    assert!(
        serial["next_cursor"].as_u64().is_some_and(|c| c > 0),
        "the ROM banner was read: {serial}"
    );
    let row = &status["instances"][0];
    assert_eq!(row["instance"], id, "status: {status}");
    assert_eq!(row["fw"], common::PK, "status: {status}");
    assert_eq!(row["state"], "paused", "status: {status}");
    assert_eq!(stop["instance"], id, "stop: {stop}");
    assert_eq!(stop["state"], "stopped", "stop: {stop}");
}

fn http_loop(daemon: &Daemon, until: &str) -> LoopRun {
    let start = daemon.http(
        "POST",
        "/v1/instances",
        &serde_json::json!({ "fw": common::PK, "boot": "none" }),
    );
    assert_eq!(start["ok"], true, "start: {start}");
    check_outcome("start", true, &start["result"], &Value::Null);
    let id = start["result"]["instance"]
        .as_str()
        .expect("an instance id")
        .to_string();

    let run = daemon.http(
        "POST",
        &format!("/v1/instances/{id}/commands/run"),
        &serde_json::json!({ "until": until, "timeout": RUN_TIMEOUT, "wall_budget_ms": WALL_BUDGET_MS }),
    );
    let run_ok = run["ok"] == true;
    check_outcome("run", run_ok, &run["result"], &run["error"]);
    let matched = run_ok && run["result"]["status"] == "matched";

    let serial = daemon.http(
        "POST",
        &format!("/v1/instances/{id}/commands/serial"),
        &serde_json::json!({ "op": "read", "cursor": 0 }),
    );
    assert_eq!(serial["ok"], true, "serial: {serial}");
    check_outcome("serial", true, &serial["result"], &Value::Null);

    let status = daemon.http(
        "POST",
        &format!("/v1/instances/{id}/commands/status"),
        &serde_json::json!({}),
    );
    assert_eq!(status["ok"], true, "status: {status}");
    check_outcome("status", true, &status["result"], &Value::Null);

    let stop = daemon.http(
        "DELETE",
        &format!("/v1/instances/{id}"),
        &serde_json::json!({}),
    );
    assert_eq!(stop["ok"], true, "stop: {stop}");
    check_outcome("stop", true, &stop["result"], &Value::Null);
    check_loop_fields(
        &id,
        &start["result"],
        matched.then_some(&run["result"]),
        &serial["result"],
        &status["result"],
        &stop["result"],
    );

    // The instance's artifacts were finished on `stop`.
    let dir = daemon.artifacts.join(daemon.server.run_id()).join(&id);
    assert!(dir.join("summary.json").is_file(), "summary.json of {id}");
    LoopRun {
        matched,
        run: if run_ok {
            run["result"].clone()
        } else {
            run["error"].clone()
        },
    }
}

/// One JSON-RPC exchange over the MCP stdio framing: one object per line in, one per line out.
fn mcp_call(mcp: &mut Mcp, request: &Value) -> Value {
    let mut out = Vec::new();
    mcp_stdio::serve(mcp, Cursor::new(format!("{request}\n")), &mut out).expect("stdio framing");
    let text = String::from_utf8(out).expect("UTF-8");
    let mut lines = text.lines();
    let answer = serde_json::from_str(lines.next().expect("one answer")).expect("JSON-RPC");
    assert!(lines.next().is_none(), "exactly one line per request");
    answer
}

/// A tool call; returns `(ok, structuredContent)` after validating it.
fn mcp_tool(mcp: &mut Mcp, rpc_id: u64, command: &str, arguments: Value) -> (bool, Value) {
    let answer = mcp_call(
        mcp,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": rpc_id, "method": "tools/call",
            "params": { "name": format!("passport_{command}"), "arguments": arguments },
        }),
    );
    assert_eq!(answer["id"], rpc_id);
    let result = &answer["result"];
    let ok = result["isError"] == false;
    let content = result["structuredContent"].clone();
    check_outcome(command, ok, &content, &content["error"]);
    assert!(
        result["content"][0]["text"].is_string(),
        "{command}: the shaped text travels beside the JSON"
    );
    (ok, content)
}

fn mcp_loop(daemon: &Daemon, until: &str) -> LoopRun {
    let mut mcp = Mcp::new(Arc::clone(&daemon.server));
    let init = mcp_call(
        &mut mcp,
        &serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let tools = mcp_call(
        &mut mcp,
        &serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    );
    for command in LOOP {
        let tool = tools["result"]["tools"]
            .as_array()
            .expect("a tool list")
            .iter()
            .find(|t| t["name"] == format!("passport_{command}"))
            .unwrap_or_else(|| panic!("tools/list offers passport_{command}"));
        assert_eq!(
            tool["outputSchema"],
            output_schema(command),
            "the tool's outputSchema is the generated one"
        );
    }

    let (ok, start) = mcp_tool(
        &mut mcp,
        3,
        "start",
        serde_json::json!({ "fw": common::PK, "boot": "none" }),
    );
    assert!(ok, "start: {start}");
    let id = start["instance"].as_str().expect("an id").to_string();
    let (run_ok, run) = mcp_tool(
        &mut mcp,
        4,
        "run",
        serde_json::json!({ "instance": id, "until": until, "timeout": RUN_TIMEOUT, "wall_budget_ms": WALL_BUDGET_MS }),
    );
    let matched = run_ok && run["status"] == "matched";
    let (ok, serial) = mcp_tool(
        &mut mcp,
        5,
        "serial",
        serde_json::json!({ "instance": id, "op": "read", "cursor": 0 }),
    );
    assert!(ok, "serial: {serial}");
    let (ok, status) = mcp_tool(&mut mcp, 6, "status", serde_json::json!({ "instance": id }));
    assert!(ok, "status: {status}");
    let (ok, stop) = mcp_tool(&mut mcp, 7, "stop", serde_json::json!({ "instance": id }));
    assert!(ok, "stop: {stop}");
    check_loop_fields(
        &id,
        &start,
        matched.then_some(&run),
        &serial,
        &status,
        &stop,
    );
    LoopRun { matched, run }
}

// ---------------------------------------------------------------------------------------------
// Where a boot stops
// ---------------------------------------------------------------------------------------------

const DIAG_SLICE_INSNS: u64 = 200_000;

/// Runs `pk` on a bare machine until `needle`, a non-limit stop, or [`RUN_TIMEOUT`], and describes
/// where it ended: stop reason, pc and symbol, the last slices' pcs, the last MMIO read, virtual
/// time, console tail and the first-touch ledger.
fn where_boot_stops(image: &[u8], app_elf: Option<&[u8]>, needle: &str) -> String {
    let flash = pemu_host::backend::merged_image(image).expect("the pk image");
    let Ok(mut m) = pemu_host::backend::build_machine(flash, 1) else {
        return "the diagnostic machine was not built".to_string();
    };
    let deadline = VTime::from_ms(1_000);
    let matcher = MatcherId(8);
    let mut reason = StopReason::Until;
    let mut last_pcs: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
    while m.now() < deadline {
        let outcome = m.run(RunLimits {
            until: Some(deadline),
            max_insns: Some(DIAG_SLICE_INSNS),
            stops: StopSet {
                matchers: vec![(
                    matcher,
                    Matcher::Serial {
                        stream: SerialStream::UsjTx,
                        pattern: LinePattern::Contains(needle.into()),
                    },
                )],
                ..StopSet::default()
            },
        });
        reason = outcome.reason;
        if last_pcs.len() == 6 {
            last_pcs.pop_front();
        }
        last_pcs.push_back(m.hart().pc);
        if !matches!(reason, StopReason::MaxInsns | StopReason::Until) {
            break;
        }
        if outcome.insns == 0 {
            break;
        }
    }
    let pc = m.hart().pc;
    let app = app_elf.and_then(|bytes| pemu_loader::elf::ElfInfo::parse(bytes).ok());
    let symbol_of = |pc: u32| {
        m.assets()
            .rom
            .symbols()
            .func_at(pc)
            .or_else(|| app.as_ref().and_then(|elf| elf.symbols.func_at(pc)))
            .map_or_else(|| "<no symbol>".to_string(), |s| s.name.clone())
    };
    let symbol = symbol_of(pc);
    let slice_pcs: Vec<String> = last_pcs
        .iter()
        .map(|pc| format!("0x{pc:08x} {}", symbol_of(*pc)))
        .collect();
    let last_read = m.last_mmio_read().map_or_else(
        || "none".to_string(),
        |r| {
            format!(
                "0x{:08x} from pc 0x{:08x} ({}), {} repeats",
                r.addr,
                r.pc,
                symbol_of(r.pc),
                r.repeats
            )
        },
    );
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
    let console = String::from_utf8_lossy(&bytes).into_owned();
    let tail: Vec<&str> = console
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let touches = m.ledger().first_touches();
    let row = |t: &pemu_core::fidelity::FirstTouch| {
        let block = BLOCKS
            .iter()
            .find(|b| b.id == t.periph)
            .map_or("<not a block>", |b| b.name);
        format!("{block} +0x{:03x} {:?} at {:?}", t.off, t.access, t.now)
    };
    format!(
        "stop {reason:?} at vt {:?}, pc 0x{pc:08x} ({symbol}); last slice pcs {slice_pcs:?}; \
         last MMIO read {last_read}; console tail {tail:?}; first-touch ledger {} rows, \
         first [{}], last [{}]",
        m.now(),
        touches.len(),
        touches.first().map_or_else(String::new, row),
        touches.last().map_or_else(String::new, row),
    )
}

// ---------------------------------------------------------------------------------------------
// The agent loop
// ---------------------------------------------------------------------------------------------

/// The agent loop to the ROM's hand-over to the bootloader (`entry 0x`), both transports,
/// schema-validated.
#[test]
fn t1_daemon_agent_loop_rom_entry() {
    let test = "t1_daemon_agent_loop_rom_entry";
    let Some(image) = pk_image(test) else {
        return;
    };
    let daemon = Daemon::start(image);
    let until = "serial:/^entry 0x/";

    let http = http_loop(&daemon, until);
    assert!(
        http.matched,
        "the ROM reaches `entry 0x` on `pk`; run: {}",
        http.run
    );
    assert_eq!(http.run["match"]["source"], "serial", "{}", http.run);

    let mcp = mcp_loop(&daemon, until);
    assert!(mcp.matched, "over MCP too; run: {}", mcp.run);
    assert_eq!(
        http.run["match"]["vt_us"], mcp.run["match"]["vt_us"],
        "two instances of one image and seed match at one instant"
    );
    assert!(daemon.server.pool().live().is_empty(), "both loops stopped");
}

/// `start`, `run --until serial:/bsp_i2c/`, `serial`, `status` and `stop` over the HTTP command
/// routes, then over MCP by a scripted client on the same server and pool, each output validated
/// against its generated schema. Both loops start with `boot: "none"`; an unmatched run fails and
/// names where the boot stops.
#[test]
fn t1_m3_agent_loop_bsp_i2c() {
    let test = "t1_m3_agent_loop_bsp_i2c";
    let exit = test.to_string();
    let Some(image) = pk_image(test) else {
        return;
    };
    let daemon = Daemon::start(Arc::clone(&image));
    let until = "serial:/bsp_i2c/";
    // The app ELF only names the pc of the report; its absence is no reason to skip.
    let app_elf = common::corpus_or_skip(test, common::PK).and_then(|files| {
        files
            .iter()
            .find(|f| f.file == PK_APP_ELF)
            .and_then(|f| std::fs::read(&f.path).ok())
    });

    // The loop runs in full either way.
    let http = http_loop(&daemon, until);
    if !http.matched {
        let code = http.run["code"].as_str().unwrap_or("none");
        panic!(
            "{}",
            format!(
                "{exit} failed: `run --until {until}` on pk did not match within {RUN_TIMEOUT} \
                 virtual (daemon run: {}, code {code}); the loop, both surfaces' schemas \
                 and stop are otherwise exercised by t1_daemon_agent_loop_rom_entry; bare machine: \
                 {}; the boot needs intc delivery and the systimer, and \
                 the cause of the stop is UNVERIFIED",
                http.run["status"].as_str().map_or_else(
                    || "failed with an error".to_string(),
                    |status| format!("status {status}")
                ),
                where_boot_stops(&image, app_elf.as_deref(), "bsp_i2c"),
            )
        );
    }
    let mcp = mcp_loop(&daemon, until);
    assert!(
        mcp.matched,
        "the MCP loop matches as the HTTP loop did: {}",
        mcp.run
    );
    assert!(daemon.server.pool().live().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------------------------

/// The `pk` boot line of the M3 gate, after its `I (<ms>) ` prefix.
const BSP_I2C: &str = "bsp_i2c: I2C 就绪 SDA=GPIO10 SCL=GPIO7";

/// Lines `pk` must print before [`BSP_I2C`], in order and without their prefix: the last line
/// before the scheduler starts, the two `main_task` lines only the running scheduler prints, and
/// the four heap regions.
const BEFORE_BSP_I2C: [&str; 7] = [
    "heap_init: At 3FCA2AC0 len 0001D540 (117 KiB): RAM",
    "heap_init: At 3FCC0000 len 0001C710 (113 KiB): Retention RAM",
    "heap_init: At 3FCDC710 len 0000294C (10 KiB): Retention RAM",
    "heap_init: At 50000020 len 00001FC8 (7 KiB): RTCRAM",
    "sleep_gpio: Enable automatic switching of GPIO sleep configuration",
    "main_task: Started on CPU0",
    "main_task: Calling app_main()",
];

/// Instructions `pk` gets from power-on to [`BSP_I2C`]; it needs about 7 million, so a regression
/// fails on the missing line rather than on a hang.
const BSP_I2C_INSNS: u64 = 200_000_000;

const TICK_WINDOW_PS: u64 = 100_000_000_000;

/// FreeRTOS ticks [`TICK_WINDOW_PS`] is worth at `CONFIG_FREERTOS_HZ` 1000.
const TICKS_IN_WINDOW: u32 = 100;

const BSP_I2C_MATCHER: MatcherId = MatcherId(1);

/// A log line without its `I (<ms>) ` prefix.
fn untimed(line: &str) -> &str {
    let line = line.trim_end_matches('\r');
    match line.find(") ") {
        Some(at) if line.starts_with(['I', 'W', 'E']) && line[1..].starts_with(" (") => {
            &line[at + 2..]
        }
        _ => line,
    }
}

/// The bring-up gate for `pk` from power-on to `bsp_i2c`: the scheduler has to start.
///
/// SYSTEM `CPU_INTR_FROM_CPU_0` has to raise IRQ source 50, or the first `vPortYield` returns and
/// `start_cpu0` parks after `sleep_gpio`; SYSTIMER comparator 0 has to honour `PERIOD_MODE` set
/// after COMP0_LOAD, or the tick fires once and the idle task waits for good after `main_task`.
#[test]
fn t1_m3_v0_pk_boots_to_bsp_i2c() {
    let test = "t1_m3_v0_pk_boots_to_bsp_i2c";
    let Some(path) = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")
    else {
        return;
    };
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses as a merged image");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    let mut m = Machine::new(MachineConfig::default(), assets).expect("the image fits the flash");

    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BSP_I2C_INSNS),
        stops: StopSet {
            matchers: vec![(
                BSP_I2C_MATCHER,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Contains(BSP_I2C.into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
    let console = String::from_utf8_lossy(&bytes).into_owned();
    let lines: Vec<&str> = console.lines().map(untimed).collect();
    let tail = lines[lines.len().saturating_sub(8)..].join("\n");

    let mut at = 0;
    for want in BEFORE_BSP_I2C {
        let found = lines[at..].iter().position(|l| *l == want);
        let Some(found) = found else {
            let hint = if want.starts_with("main_task") {
                "the scheduler never started: SYSTEM FROM_CPU_0 must raise IRQ source 50"
            } else {
                "the app did not get this far"
            };
            panic!(
                "`{want}` missing ({hint}); pc {:#010x}; console tail:\n{tail}",
                m.hart().pc
            );
        };
        at += found + 1;
    }
    assert_eq!(
        out.reason,
        StopReason::Matcher(BSP_I2C_MATCHER),
        "`pk` stopped at pc {:#010x} before `{BSP_I2C}` (a FreeRTOS tick that stops after one \
         fire is SYSTIMER PERIOD_MODE); console tail:\n{tail}",
        m.hart().pc
    );
    assert_eq!(
        lines[at..].iter().filter(|l| **l == BSP_I2C).count(),
        1,
        "`{BSP_I2C}` follows `main_task` once; console tail:\n{tail}"
    );

    // `xTickCount` advances one per millisecond past `bsp_i2c`. `pk` reaches the line without
    // waiting on a tick, so this is the check that the periodic comparator keeps ticking.
    let Some(elf_path) = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport.elf")
    else {
        return;
    };
    let elf = ElfInfo::parse(&std::fs::read(&elf_path).expect("the verified ELF is readable"))
        .expect("the pinned app ELF parses");
    let tick_count = elf
        .symbols
        .lookup("xTickCount")
        .expect("the app ELF names the FreeRTOS tick counter")
        .addr;
    let ticks = |m: &mut Machine| {
        m.guest_mem()
            .load(tick_count, 4)
            .expect("xTickCount is in SRAM")
    };
    let before = ticks(&mut m);
    let until = VTime(m.now().0 + TICK_WINDOW_PS);
    let out = m.run(RunLimits {
        until: Some(until),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(out.reason, StopReason::Until, "pc {:#010x}", m.hart().pc);
    let advanced = ticks(&mut m).wrapping_sub(before);
    assert!(
        advanced.abs_diff(TICKS_IN_WINDOW) <= 1,
        "xTickCount advanced {advanced} over 100 ms, not {TICKS_IN_WINDOW}: the SYSTIMER \
         comparator 0 tick stopped (PERIOD_MODE written after COMP0_LOAD); pc {:#010x}",
        m.hart().pc
    );
}

/// Instructions the ROM is given to reach `entry`; it needs about 150 thousand.
const ENTRY_INSNS: u64 = 50_000_000;

const ENTRY: MatcherId = MatcherId(1);

fn to_entry(mut stops: StopSet) -> RunLimits {
    stops.matchers.push((
        ENTRY,
        Matcher::Serial {
            stream: SerialStream::UsjTx,
            pattern: LinePattern::Prefix("entry 0x".into()),
        },
    ));
    RunLimits {
        until: None,
        max_insns: Some(ENTRY_INSNS),
        stops,
    }
}

fn console_from(m: &mut Machine, from: u64) -> Vec<u8> {
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    ring.slices(from).iter().copied().collect()
}

/// Instruction counts from a fixed LCG in `1..below`, the same on every host.
fn points(n: usize, below: u64) -> Vec<u64> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            1 + (x >> 11) % (below - 1)
        })
        .collect()
}

/// Saves `m`, restores into a fresh machine over the same image, and runs it to `entry`,
/// asserting it ends where `straight` ended.
fn resume_equals(test: &str, what: &str, m: &mut Machine, image: &[u8], straight: &mut Machine) {
    let snap = m.snapshot(SnapOpts::default());
    let head = m.io().usj_tx.head();
    let bytes = snap.to_bytes().expect("a machine snapshot serializes");
    let mut fresh = image_machine(image);
    fresh
        .restore(&Snapshot::from_bytes(&bytes).expect("parses"))
        .expect("a snapshot of the same run identity restores");
    let out = fresh.run(to_entry(StopSet::default()));
    assert_eq!(out.reason, StopReason::Matcher(ENTRY), "{test}: {what}");
    assert_eq!(fresh.now(), straight.now(), "{test}: {what}: virtual time");
    assert_eq!(fresh.hart().insns, straight.hart().insns, "{test}: {what}");
    assert_eq!(fresh.state_hash(), straight.state_hash(), "{test}: {what}");
    assert_eq!(
        console_from(&mut fresh, head),
        console_from(straight, head),
        "{test}: {what}: console after the snapshot"
    );
}

/// Snapshot anywhere over the ROM boot to `entry`: snapshots at six instruction counts and at a
/// breakpoint, each restored in a fresh machine, end where the straight run ends.
#[test]
fn t1_m3_snapshot_anywhere_rom_boot() {
    let test = "t1_m3_snapshot_anywhere_rom_boot";
    let Some(path) = common::corpus_file_or_skip(test, common::PK, PK_IMAGE) else {
        return;
    };
    let image = std::fs::read(&path).expect("the verified corpus file is readable");

    let mut straight = image_machine(&image);
    let done = straight.run(to_entry(StopSet::default()));
    assert_eq!(
        done.reason,
        StopReason::Matcher(ENTRY),
        "the ROM reaches `entry`"
    );
    let total = straight.hart().insns;

    for k in points(6, total) {
        let mut m = image_machine(&image);
        let out = m.run(RunLimits::insns(k));
        assert_eq!(out.reason, StopReason::MaxInsns);
        resume_equals(
            test,
            &format!("instruction {k}"),
            &mut m,
            &image,
            &mut straight,
        );
    }

    let mut probe = image_machine(&image);
    probe.run(RunLimits::insns(total / 2));
    let bp = probe.hart().pc;
    let mut m = image_machine(&image);
    let stop = m.run(to_entry(StopSet {
        breakpoints: vec![bp],
        ..StopSet::default()
    }));
    assert_eq!(stop.reason, StopReason::Breakpoint(bp));
    resume_equals(test, "breakpoint", &mut m, &image, &mut straight);
}

// ---------------------------------------------------------------------------------------------
// Hang negative
// ---------------------------------------------------------------------------------------------

/// Scenario `hang-negative`: `pk` with `--disable-model spi2` stops with `E_STUCK` naming
/// busy-wait row `spi2.cmd_update` within 3 s of virtual time. The mechanism is covered without
/// the corpus by `pemu_machine::ff_tests`.
#[test]
fn t1_m3_hang_negative() {
    let test = "t1_m3_hang_negative";
    let Some(path) = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")
    else {
        return;
    };
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses as a merged image");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned");
    let mut m = Machine::new(MachineConfig::default(), assets).expect("the image fits");
    assert!(m.disable_model("spi2"));
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(3_000)),
        max_insns: None,
        stops: StopSet::default(),
    });
    let StopReason::Stuck(report) = out.reason else {
        panic!(
            "`pk` without spi2 did not stop as stuck within 3 s: {:?} at {:?}, pc {:#x}",
            out.reason,
            out.vt,
            m.hart().pc
        );
    };
    assert_eq!(report.wait_row, Some("spi2.cmd_update"), "{report:?}");
    assert_eq!(report.block, "spi2");
    assert_ne!(report.kind, StuckKind::Fallback, "{report:?}");
    assert!(out.vt <= VTime::from_ms(3_000));
}

// ---------------------------------------------------------------------------------------------
// Determinism and restore equivalence
// ---------------------------------------------------------------------------------------------

/// The merged images of the three corpus ids.
const IMAGES: [(&str, &str); 3] = [
    (common::PK, PK_IMAGE),
    (common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin"),
    (common::GOLDMINER, "goldminer-sanitized-8MB.bin"),
];

fn bsp_i2c_stops() -> StopSet {
    StopSet {
        matchers: vec![(
            BSP_I2C_MATCHER,
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern: LinePattern::Contains(BSP_I2C.into()),
            },
        )],
        ..StopSet::default()
    }
}

/// The `pk` line the determinism span runs on to (dev:L65). `pk` has presented 16 frames by then,
/// so the frame digests compare pictures.
const LVGL_READY: &str = "bsp_lvgl: LVGL 就绪";

const LVGL_READY_MATCHER: MatcherId = MatcherId(2);

fn lvgl_ready_stops() -> StopSet {
    StopSet {
        matchers: vec![(
            LVGL_READY_MATCHER,
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern: LinePattern::Contains(LVGL_READY.into()),
            },
        )],
        ..StopSet::default()
    }
}

fn variant_machine(flash: &[u8], variant: &Variant) -> Machine {
    app_machine(flash, None, variant)
}

/// A machine over the bundled ROM, `flash` and the app ELF `elf` under the D-3 choices of
/// `variant`. The ELF is run identity: with it `pk` binds its RadioModule and runs past BLE init,
/// and a restore needs the same ELF.
fn app_machine(flash: &[u8], elf: Option<Arc<ElfInfo>>, variant: &Variant) -> Machine {
    let flash = FlashImage::from_merged(flash).expect("a corpus image parses as a merged image");
    let assets = Assets::with_bundled_rom(flash, elf, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    let mut m = Machine::new(variant.config(MachineConfig::default()), assets)
        .expect("the image fits the 8 MB flash");
    variant.apply(&mut m);
    m
}

/// The app ELF of corpus id `id`, parsed once per process: `Some(None)` for `goldminer`, which
/// has none, and `None` after the corpus skip line.
fn app_elf(test: &str, id: &str) -> Option<Option<Arc<ElfInfo>>> {
    type Cache = std::sync::Mutex<std::collections::BTreeMap<String, Arc<ElfInfo>>>;
    static ELVES: OnceLock<Cache> = OnceLock::new();
    if id == common::GOLDMINER {
        return Some(None);
    }
    let cache = ELVES.get_or_init(Cache::default);
    if let Some(elf) = cache.lock().expect("not poisoned").get(id) {
        return Some(Some(Arc::clone(elf)));
    }
    let path = common::corpus_file_or_skip(test, id, PK_APP_ELF)?;
    let bytes = std::fs::read(path).expect("the verified corpus ELF is readable");
    let elf = Arc::new(ElfInfo::parse(&bytes).expect("the pinned ELF parses"));
    cache
        .lock()
        .expect("not poisoned")
        .insert(id.to_string(), Arc::clone(&elf));
    Some(Some(elf))
}

/// Length of one ladder click the scripts below journal, the `input` default.
const CLICK_MS: u64 = 80;

/// The microphone rate and the Audio demo's recording rate.
const MIC_FS: u32 = 16_000;

/// The inputs that take `official` from its settled menu into the Audio demo and play its tone,
/// as `m6.rs` `e64_inputs` scripts them: DOWN at 1.5 s and 2.0 s, OK at 2.5 s opens the demo, and
/// OK at 3.0 s plays its 1 kHz square (at the codec from 3.39 s to 4.39 s). With `record`, UP at
/// 4.5 s records 3 s and plays it back, with a -6 dBFS 440 Hz microphone tone from 4.0 s for 5 s.
fn official_script(record: bool) -> Vec<(VTime, InputEvent)> {
    use pemu_api::commands::mic_set::{self, CHUNK_FRAMES};
    use pemu_core::input::{ButtonId, MicSource};

    let mut clicks = vec![
        (1_500, ButtonId::Down),
        (2_000, ButtonId::Down),
        (2_500, ButtonId::Ok),
        (3_000, ButtonId::Ok),
    ];
    if record {
        clicks.push((4_500, ButtonId::Up));
    }
    let mut out = Vec::new();
    for (ms, id) in clicks {
        out.push((VTime::from_ms(ms), InputEvent::Button { id, down: true }));
        out.push((
            VTime::from_ms(ms + CLICK_MS),
            InputEvent::Button { id, down: false },
        ));
    }
    if record {
        let source = MicSource::Tone {
            hz: 440,
            amplitude: 16_384,
        };
        let mut tone = vec![0i16; 5 * MIC_FS as usize];
        assert!(mic_set::render(&source, MIC_FS, 0, &mut tone));
        let from = VTime::from_ms(4_000);
        for (k, chunk) in tone.chunks(CHUNK_FRAMES).enumerate() {
            out.push((
                pemu_core::time::frame_time(from, (k * CHUNK_FRAMES) as u64, MIC_FS),
                InputEvent::MicChunk {
                    seq: k as u64,
                    samples: mic_set::interleave(chunk, 1),
                },
            ));
        }
    }
    out.sort_by_key(|(at, _)| *at);
    out
}

fn scripted_official(
    image: &[u8],
    elf: &Arc<ElfInfo>,
    variant: &Variant,
    script: Vec<(VTime, InputEvent)>,
) -> Machine {
    let mut m = app_machine(image, Some(Arc::clone(elf)), variant);
    for (at, ev) in script {
        m.input(At::Vt(at), ev)
            .expect("an input at a future instant is journaled");
    }
    m
}

fn official_files(test: &str) -> Option<(Vec<u8>, Arc<ElfInfo>)> {
    let path = common::corpus_file_or_skip(test, common::OFFICIAL, PK_IMAGE)?;
    let image = std::fs::read(path).expect("the verified corpus file is readable");
    let elf = app_elf(test, common::OFFICIAL)?.expect("`official` has an app ELF");
    Some((image, elf))
}

fn to_bsp_i2c(image: &[u8], variant: &Variant) -> (Machine, String) {
    let mut m = variant_machine(image, variant);
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BSP_I2C_INSNS),
        stops: bsp_i2c_stops(),
    });
    let text = report(&out.reason, &m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(BSP_I2C_MATCHER),
        "`pk` reaches `bsp_i2c` under {}: {text}",
        variant.label()
    );
    (m, text)
}

/// The `pk` determinism span under `variant`: power-on to `bsp_i2c` and on to [`LVGL_READY`].
/// Returns the machine at LVGL ready, the instruction count and report at `bsp_i2c`, and both
/// reports as one text.
fn to_lvgl_ready(image: &[u8], variant: &Variant) -> (Machine, u64, String, String) {
    let (mut m, at_bsp_i2c) = to_bsp_i2c(image, variant);
    let total = m.hart().insns;
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BSP_I2C_INSNS),
        stops: lvgl_ready_stops(),
    });
    let text = report(&out.reason, &m);
    assert_eq!(
        out.reason,
        StopReason::Matcher(LVGL_READY_MATCHER),
        "`pk` reaches LVGL ready under {}: {text}",
        variant.label()
    );
    let both = format!("at bsp_i2c: {at_bsp_i2c}\nat LVGL ready: {text}");
    (m, total, at_bsp_i2c, both)
}

fn pk_image_file(test: &str) -> Option<(PathBuf, Vec<u8>)> {
    let path = common::corpus_file_or_skip(test, common::PK, PK_IMAGE)?;
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    Some((path, bytes))
}

/// `pk` under `variant` as a paused instance of a `pemu_api` command pool, advanced by `clock step`
/// to each count of `at`, then run on to `bsp_i2c`: its `state_hash`, instruction count and time.
fn clock_step_run(image: &[u8], variant: &Variant, at: &[u64]) -> ([u8; 32], u64, VTime) {
    use pemu_api::commands::clock::{ClockArgs, Op, clock_on};
    use pemu_api::commands::start::{Boot, Pool, StartArgs};

    let machine = variant_machine(image, variant);
    let mut pool = Pool::new();
    let start = StartArgs {
        fw: common::PK.to_owned(),
        boot: Boot::None,
        ..StartArgs::default()
    };
    let id = pool.attach(&start, Box::new(machine));
    pool.table_mut()
        .get_mut(id)
        .expect("the instance was just attached")
        .transition(pemu_api::instance::Lifecycle::Paused, VTime(0))
        .expect("starting -> paused");
    let mut sorted = at.to_vec();
    sorted.sort_unstable();
    let mut done = 0;
    for k in sorted {
        if k <= done {
            continue;
        }
        let step = ClockArgs::from_json(&serde_json::json!({"op": "step", "insns": k - done}))
            .expect("`clock step` parses");
        assert_eq!(step.op, Op::Step);
        let out = clock_on(&mut pool, id, &step).expect("a paused instance steps");
        done = out.json["insns"]
            .as_u64()
            .expect("the step reports the instruction count");
    }
    let session = pool.session_mut(id).expect("the instance");
    let m = session.snapshot_machine();
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BSP_I2C_INSNS),
        stops: bsp_i2c_stops(),
    });
    assert_eq!(
        out.reason,
        StopReason::Matcher(BSP_I2C_MATCHER),
        "`pk` reaches `bsp_i2c` after `clock step`"
    );
    (m.state_hash(), done + out.insns, m.now())
}

/// Every determinism variant (block size, poll and ROM delay fast-forward, trace, stop
/// invariance) gives the default run's whole report, under `fast` and `device`.
///
/// Two spans, so the frame and PCM digests compare pictures and sound: `pk` to LVGL ready
/// ([`determinism_matrix`]) and `official` into the Audio demo's tone ([`determinism_audio`]).
/// Under `device` the `lru16k` line account stalls misses, so every variant could also move the
/// stall points.
/// Debugger stops never move virtual time (a QEMU icount run is known to fail this).
#[test]
fn t1_m3_determinism_bsp_i2c() {
    let test = "t1_m3_determinism_bsp_i2c";
    let Some((_, image)) = pk_image_file(test) else {
        return;
    };
    let official = official_files(test);
    let mut coverage = determinism::Coverage::default();
    for profile in [TimingProfileId::Fast, TimingProfileId::Device] {
        determinism_matrix(test, &image, profile, &mut coverage);
        if let Some((image, elf)) = &official {
            determinism_audio(test, image, elf, profile, &mut coverage);
        }
    }
    coverage.report(test);
}

/// The `pk` span under one profile: block sizes 1 and 3 and `ref_step`, ROM delay fast-forward
/// off, trace on, and poll fast-forward on and off with equal canonical traces. Stop invariance
/// arms breakpoints, write watches, matchers and pauses; at least one breakpoint, watch and pause
/// must fire.
fn determinism_matrix(
    test: &str,
    image: &[u8],
    profile: TimingProfileId,
    coverage: &mut determinism::Coverage,
) {
    let image = image.to_vec();
    let base = Variant {
        profile,
        ..Variant::default()
    };
    let (mut straight, total, want, want_both) = to_lvgl_ready(&image, &base);
    println!("RAN {test} pk {profile:?}");
    coverage.output(
        &format!("`pk` to LVGL ready under {profile:?}"),
        &mut straight,
        Since::START,
    );

    let mut variants = vec![
        Variant {
            max_block_insns: 1,
            ..base
        },
        Variant {
            max_block_insns: 3,
            ..base
        },
        Variant {
            executor: Executor::Reference,
            ..base
        },
        Variant {
            rom_delay_ff: !base.rom_delay_ff,
            ..base
        },
        Variant {
            trace: !base.trace,
            ..base
        },
    ];
    variants.push(Variant {
        poll_ff: !base.poll_ff,
        ..base
    });
    for variant in &variants {
        let (_, _, _, got) = to_lvgl_ready(&image, variant);
        assert_eq!(
            got,
            want_both,
            "{test}: {} differs at `bsp_i2c` or LVGL ready",
            variant.label()
        );
    }

    // The ROM delay fast-forward is off in both, so what "on" skipped is the poll fast-forward's.
    let traced = |poll_ff| {
        let variant = Variant {
            poll_ff,
            trace: true,
            rom_delay_ff: false,
            ..base
        };
        let mut m = variant_machine(&image, &variant);
        let mut skipped = 0;
        let mut reports = Vec::new();
        for stops in [bsp_i2c_stops(), lvgl_ready_stops()] {
            let out = m.run(RunLimits {
                until: None,
                max_insns: Some(BSP_I2C_INSNS),
                stops,
            });
            skipped += out.ff_insns;
            reports.push(report(&out.reason, &m));
        }
        let both = format!("at bsp_i2c: {}\nat LVGL ready: {}", reports[0], reports[1]);
        (both, m.trace_digest(), skipped)
    };
    let (on, off) = (traced(true), traced(false));
    assert_eq!(on.0, want_both, "{test}: poll_ff on with the trace on");
    assert_eq!(off.0, want_both, "{test}: poll_ff off with the trace on");
    assert_eq!(
        on.1, off.1,
        "{test}: the canonical trace differs with poll fast-forward on and off"
    );
    assert_eq!(
        off.2, 0,
        "{test}: nothing is skipped with both shortcuts off"
    );
    // Under `fast` nothing is skipped; under `device` the chains of the clocked SPI1, SPI2 and I2C0
    // waits.
    coverage.poll_ff(&format!("`pk` to LVGL ready under {profile:?}"), on.2);

    let straight = {
        let mut m = variant_machine(&image, &base);
        m.run(RunLimits {
            until: None,
            max_insns: Some(BSP_I2C_INSNS),
            stops: bsp_i2c_stops(),
        });
        (m.state_hash(), m.hart().insns, m.now())
    };
    let mut probe = variant_machine(&image, &base);
    let mut breakpoints = Vec::new();
    for k in points(5, total) {
        let at = k - probe.hart().insns.min(k);
        probe.run(RunLimits {
            until: None,
            max_insns: Some(at),
            stops: StopSet::default(),
        });
        breakpoints.push(probe.hart().pc);
    }
    // A stack slot of the task running halfway to `bsp_i2c`: written again before the line.
    let mut half = variant_machine(&image, &base);
    half.run(RunLimits::insns(total / 2));
    let stack = half.hart().x[2] - 32;
    breakpoints.sort_unstable();
    breakpoints.dedup();
    // Never reached: RV32IMC instructions start on even addresses.
    breakpoints.push(0x4038_0001);
    let extra = StopSet {
        breakpoints,
        watches: vec![
            Watch {
                addr: stack,
                len: 32,
            },
            Watch {
                addr: 0x3FCA_2AC0,
                len: 64,
            },
            Watch {
                addr: 0x3FCC_8000,
                len: 16,
            },
        ],
        matchers: vec![
            (
                MatcherId(100),
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Exact("no such line in pk".into()),
                },
            ),
            (MatcherId(101), Matcher::Event(EventKind::Sleep)),
        ],
    };
    assert!(extra.check().is_ok(), "the watches are watchable");
    let mut m = variant_machine(&image, &base);
    let pauses: Vec<u64> = points(7, total).into_iter().map(|k| k ^ 0x55).collect();
    let (reason, taken) =
        determinism::run_through_stops(&mut m, &bsp_i2c_stops(), BSP_I2C_INSNS, extra, &pauses);
    assert_eq!(
        report(&reason, &m),
        want,
        "{test}: armed stops and pauses changed the run ({taken:?})"
    );
    assert!(
        taken.breakpoints >= 1,
        "{test}: no breakpoint fired: {taken:?}"
    );
    assert!(taken.watches >= 1, "{test}: no watch fired: {taken:?}");
    assert!(taken.pauses >= 1, "{test}: no pause was taken: {taken:?}");
    // User observe hooks at three more random instants add their own fire counts to the
    // `hle.machine` section, so `state_hash` differs by exactly that and nothing else may.
    let mut hooked = variant_machine(&image, &base);
    let mut observed = Vec::new();
    let mut probe = variant_machine(&image, &base);
    for k in points(3, total).into_iter().map(|k| k ^ 0x3333) {
        let at = k - probe.hart().insns.min(k);
        probe.run(RunLimits::insns(at));
        let pc = probe.hart().pc;
        if hooked.add_observe_hook(pc, "determinism") {
            observed.push(pc);
        }
    }
    let out = hooked.run(RunLimits {
        until: None,
        max_insns: Some(BSP_I2C_INSNS),
        stops: bsp_i2c_stops(),
    });
    let without_state = |line: &str| {
        line.split(' ')
            .filter(|field| !field.starts_with("state="))
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(
        without_state(&report(&out.reason, &hooked)),
        without_state(&want),
        "{test}: observe hooks changed the run"
    );
    for pc in &observed {
        assert!(
            hooked.observe_fires(*pc).is_some_and(|(n, _)| n >= 1),
            "{test}: the observe hook at {pc:#010x} never fired"
        );
    }
    println!("RAN {test} observe-hooks");
    // `clock step` at random points through the real command, then on to `bsp_i2c`: it must end with
    // the straight run's `state_hash`, instruction count and virtual time.
    let steps = clock_step_run(&image, &base, &points(4, total));
    assert_eq!(
        steps.0, straight.0,
        "{test}: `clock step` changed the state_hash"
    );
    assert_eq!(
        (steps.1, steps.2),
        (straight.1, straight.2),
        "{test}: `clock step` moved the instruction count or virtual time"
    );
    println!("RAN {test} clock-step {profile:?}");
    // In-process restores at three counts reach `bsp_i2c` and LVGL ready with equal reports, the
    // output compared from each machine's own mark (a snapshot carries no guest-to-host output).
    // Under `device` a restore that lost the line account's state would miss where the straight run
    // hits.
    let on_to_lvgl_ready = |m: &mut Machine| {
        let since = Since::now(m);
        let mut reports = Vec::new();
        for stops in [bsp_i2c_stops(), lvgl_ready_stops()] {
            let out = m.run(RunLimits {
                until: None,
                max_insns: Some(BSP_I2C_INSNS),
                stops,
            });
            reports.push(report_since(&out.reason, m, since));
        }
        reports.join("\n")
    };
    for k in points(3, total).into_iter().map(|k| k ^ 0x0F0F) {
        let mut m = variant_machine(&image, &base);
        m.run(RunLimits::insns(k));
        let bytes = m
            .snapshot(SnapOpts::default())
            .to_bytes()
            .expect("a machine snapshot serializes");
        let straight_on = on_to_lvgl_ready(&mut m);
        let mut again = variant_machine(&image, &base);
        again
            .restore(&Snapshot::from_bytes(&bytes).expect("parses"))
            .expect("a snapshot of the same run identity restores");
        assert_eq!(
            on_to_lvgl_ready(&mut again),
            straight_on,
            "{test}: {profile:?}: a restore at {k} instructions changed the run"
        );
    }
    println!("RAN {test} snapshot-restore {profile:?}");
    println!(
        "determinism `pk` to `bsp_i2c` and LVGL ready under {profile:?}: {} variants equal; poll \
         fast-forward skipped {} instructions with the same trace; stops taken {taken:?}",
        variants.len() + 2,
        on.2
    );
}

/// Where the `official` span ends: past the Audio demo's tone.
const AUDIO_SPAN_END_MS: u64 = 4_500;

/// The `official` span under one profile, from power-on into the Audio demo's tone to 4.5 s: the
/// same variants as the `pk` span, pauses at seven counts, and in-process restores at three
/// instants, one inside the tone.
fn determinism_audio(
    test: &str,
    image: &[u8],
    elf: &Arc<ElfInfo>,
    profile: TimingProfileId,
    coverage: &mut determinism::Coverage,
) {
    let base = Variant {
        profile,
        ..Variant::default()
    };
    let end = VTime::from_ms(AUDIO_SPAN_END_MS);
    let to_end = |m: &mut Machine| {
        m.run(RunLimits {
            until: Some(end),
            max_insns: None,
            stops: StopSet::default(),
        })
    };
    let run = |variant: &Variant| {
        let mut m = scripted_official(image, elf, variant, official_script(false));
        let out = to_end(&mut m);
        assert_eq!(
            out.reason,
            StopReason::Until,
            "{test}: `official` under {} stops short of {AUDIO_SPAN_END_MS} ms",
            variant.label()
        );
        (report(&out.reason, &m), out.ff_insns, m)
    };
    let (want, _, mut straight) = run(&base);
    let total = straight.hart().insns;
    println!("RAN {test} official {profile:?}");
    coverage.output(
        &format!("`official` Audio demo under {profile:?}"),
        &mut straight,
        Since::START,
    );

    let variants = [
        Variant {
            max_block_insns: 1,
            ..base
        },
        Variant {
            max_block_insns: 3,
            ..base
        },
        Variant {
            executor: Executor::Reference,
            ..base
        },
        Variant {
            rom_delay_ff: !base.rom_delay_ff,
            ..base
        },
        Variant {
            trace: !base.trace,
            ..base
        },
        Variant {
            poll_ff: !base.poll_ff,
            ..base
        },
    ];
    for variant in &variants {
        assert_eq!(
            run(variant).0,
            want,
            "{test}: `official` under {} differs at {AUDIO_SPAN_END_MS} ms",
            variant.label()
        );
    }

    let traced = |poll_ff| {
        let (text, skipped, m) = run(&Variant {
            poll_ff,
            trace: true,
            rom_delay_ff: false,
            ..base
        });
        (text, m.trace_digest(), skipped)
    };
    let (on, off) = (traced(true), traced(false));
    assert_eq!(
        on.0, want,
        "{test}: `official` poll_ff on with the trace on"
    );
    assert_eq!(
        off.0, want,
        "{test}: `official` poll_ff off with the trace on"
    );
    assert_eq!(
        on.1, off.1,
        "{test}: `official`: the canonical trace differs with poll fast-forward on and off"
    );
    assert_eq!(
        off.2, 0,
        "{test}: `official`: nothing is skipped with both shortcuts off"
    );
    coverage.poll_ff(&format!("`official` Audio demo under {profile:?}"), on.2);

    let mut m = scripted_official(image, elf, &base, official_script(false));
    let mut pauses = points(7, total);
    pauses.sort_unstable();
    let mut taken = 0;
    for p in pauses {
        let now = m.hart().insns;
        if p <= now {
            continue;
        }
        let out = m.run(RunLimits {
            until: Some(end),
            max_insns: Some(p - now),
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::MaxInsns, "{test}: pause at {p}");
        taken += 1;
    }
    let out = to_end(&mut m);
    assert_eq!(
        report(&out.reason, &m),
        want,
        "{test}: `official`: {taken} pauses changed the run"
    );
    assert!(taken >= 1, "{test}: `official`: no pause was taken");

    // Restores at 2.2 s, 3.5 s (inside the tone) and 4.2 s into machines with nothing journaled: the
    // snapshot carries the journal of the clicks still to come.
    for ms in [2_200, 3_500, 4_200] {
        let mut m = scripted_official(image, elf, &base, official_script(false));
        m.run(RunLimits {
            until: Some(VTime::from_ms(ms)),
            max_insns: None,
            stops: StopSet::default(),
        });
        let bytes = m
            .snapshot(SnapOpts::default())
            .to_bytes()
            .expect("a machine snapshot serializes");
        let since = Since::now(&m);
        let out = to_end(&mut m);
        let straight_on = report_since(&out.reason, &m, since);
        let mut again = app_machine(image, Some(Arc::clone(elf)), &base);
        again
            .restore(&Snapshot::from_bytes(&bytes).expect("parses"))
            .expect("a snapshot of the same run identity restores");
        let own = Since::now(&again);
        let out = to_end(&mut again);
        assert_eq!(
            report_since(&out.reason, &again, own),
            straight_on,
            "{test}: `official` {profile:?}: a restore at {ms} ms changed the run"
        );
    }
    println!(
        "determinism `official` Audio demo to {AUDIO_SPAN_END_MS} ms under {profile:?}: {} variants \
         equal; poll fast-forward skipped {} instructions with the same trace; {taken} pauses; 3 \
         in-process restores",
        variants.len() + 2,
        on.2
    );
}

/// `pk` to `bsp_i2c` from the wasm32 build under Node, and under `jsc` where present, gives the
/// native report.
#[test]
fn t1_m3_native_equals_node_and_jsc() {
    let test = "t1_m3_native_equals_node_and_jsc";
    let Some((path, image)) = pk_image_file(test) else {
        return;
    };
    let base = Variant::default();
    let (_, native) = to_bsp_i2c(&image, &base);
    let legs = determinism::wasm_legs(&determinism::WasmBoot {
        image: &path,
        pattern: BSP_I2C,
        prefix: false,
        max_insns: BSP_I2C_INSNS,
        max_block_insns: base.max_block_insns,
        max_slice: base.max_slice,
        poll_ff: base.poll_ff,
    });
    determinism::assert_legs(test, "`pk` at `bsp_i2c`", &native, legs);
}

/// Slice invariance (nightly): `max_slice` in {1, 37, 4096, 10^6} gives one report at `bsp_i2c`
/// under `fast` and under `device`, each compared with itself. Each matrix also varies the
/// executor and `poll_ff`, so the fast-forward surface the `device` events add is walked both ways.
#[test]
fn t2_m3_slice_invariance_both_profiles() {
    let test = "t2_m3_slice_invariance_both_profiles";
    let Some((_, image)) = pk_image_file(test) else {
        return;
    };
    println!("RAN {test} pk");
    println!(
        "RAN {test} device-profile: `device` selects the device column of \
         specs/timing-profiles.toml, so the matrix below runs it"
    );
    for profile in [TimingProfileId::Fast, TimingProfileId::Device] {
        let base = Variant {
            max_slice: 1_000_000,
            profile,
            ..Variant::default()
        };
        let (_, want) = to_bsp_i2c(&image, &base);
        let mut variants: Vec<Variant> = [1, 37, 4_096]
            .into_iter()
            .map(|max_slice| Variant { max_slice, ..base })
            .collect();
        // Console TX pacing makes the UART0 status registers and `EP1_CONF` `UntilNextEvent` while a
        // drain is pending, so `ref_step` and `poll_ff` off are in the matrix under this profile too.
        variants.push(Variant {
            executor: Executor::Reference,
            ..base
        });
        variants.push(Variant {
            poll_ff: !base.poll_ff,
            ..base
        });
        for variant in &variants {
            let (_, got) = to_bsp_i2c(&image, variant);
            assert_eq!(got, want, "{test}: {} differs", variant.label());
        }
    }
}

/// Snapshot anywhere (nightly). Every snapshot is restored in a fresh process, and every report
/// must equal the uninterrupted run over the output after the instant.
/// - **Anywhere:** `pk` saved at 20 pseudo-random counts before `bsp_i2c`, run on to `bsp_i2c`.
/// - **The special states** (an outstanding HLE continuation, DMA in flight, unconsumed `usj_rx`
///   and `audio_in` data, a future-stamped journaled input, a TCB reused after `vTaskDelete`):
///   [`SPECIALS`] names four instants that hold them. A state no instant holds prints
///   `NOT_RUN <test> special-states`.
#[test]
fn t2_m3_snapshot_anywhere_fresh_process() {
    let test = "t2_m3_snapshot_anywhere_fresh_process";
    let Some((_, image)) = pk_image_file(test) else {
        return;
    };
    let base = Variant::default();
    if let Some(file) = determinism::child_snapshot() {
        let tag = determinism::child_tag(&file);
        if let Some(special) = tag.strip_prefix("special-") {
            special_child(test, special, &file);
            return;
        }
        let bytes = std::fs::read(&file).expect("the parent wrote the snapshot");
        let mut m = variant_machine(&image, &base);
        m.restore(&Snapshot::from_bytes(&bytes).expect("parses"))
            .expect("a snapshot of the same run identity restores");
        // The output a restored machine produces starts at the snapshot's cursors.
        let since = Since::now(&m);
        let out = m.run(RunLimits {
            until: None,
            max_insns: Some(BSP_I2C_INSNS),
            stops: bsp_i2c_stops(),
        });
        determinism::child_answer(&file, &report_since(&out.reason, &m, since));
        return;
    }
    let (straight, _) = to_bsp_i2c(&image, &base);
    let total = straight.hart().insns;
    let at = points(20, total);
    let mut wants = Vec::new();
    let mut snaps: Vec<(String, Vec<u8>)> = at
        .iter()
        .map(|&k| {
            let mut m = variant_machine(&image, &base);
            let out = m.run(RunLimits::insns(k));
            assert_eq!(out.reason, StopReason::MaxInsns, "{test}: instruction {k}");
            let since = Since::now(&m);
            wants.push((
                format!("instruction {k}"),
                report_since(&StopReason::Matcher(BSP_I2C_MATCHER), &straight, since),
            ));
            let bytes = m
                .snapshot(SnapOpts::default())
                .to_bytes()
                .expect("a machine snapshot serializes");
            (format!("insn-{k}"), bytes)
        })
        .collect();

    let mut held: Vec<(&str, Vec<SpecialState>)> = Vec::new();
    for special in &SPECIALS {
        let Some((image, elf)) = special_files(test, special.id) else {
            continue;
        };
        let (mut m, detail) = special_machine(test, special.tag, &image, &elf);
        let snap = m.snapshot(SnapOpts::default());
        let bytes = snap.to_bytes().expect("a machine snapshot serializes");
        let mut states = special_states(&mut m, &snap);
        let deleted: BTreeSet<u32> = m.hle_section().task_generations.keys().copied().collect();
        let since = Since::now(&m);
        let out = to_special_end(&mut m, special);
        wants.push((
            format!("{} ({detail})", special.tag),
            report_since(&out.reason, &m, since),
        ));
        states.push(reused_tcb(&image, &elf, &bytes, &deleted, special));
        held.push((special.tag, states));
        snaps.push((format!("special-{}", special.tag), bytes));
    }

    let answers = determinism::fresh_process(test, &snaps);
    for ((what, want), got) in wants.iter().zip(&answers) {
        assert_eq!(got, want, "{test}: restored from {what} in a fresh process");
    }
    println!("RAN {test} pk");
    println!(
        "snapshot anywhere: 20 fresh-process restores between 1 and {total} equal the \
         straight run"
    );
    report_special_states(test, &held);
}

/// One special instant: its tag, its corpus id, and the virtual time both runs run to.
struct Special {
    tag: &'static str,
    id: &'static str,
    end_ms: u64,
}

const SPECIALS: [Special; 4] = [
    Special {
        tag: "pk-nested-hle",
        id: common::PK,
        end_ms: 1_000,
    },
    Special {
        tag: "pk-usj-rx",
        id: common::PK,
        end_ms: 1_000,
    },
    Special {
        tag: "official-2800ms",
        id: common::OFFICIAL,
        end_ms: 8_000,
    },
    Special {
        tag: "official-4700ms",
        id: common::OFFICIAL,
        end_ms: 8_000,
    },
];

/// The hooked function `pk`'s nested-HLE instants stop in; its handler calls back into the guest.
const NESTED_HLE_HOOK: &str = "esp_bt_controller_init";

/// Instructions [`to_nested_hle`] steps past the hook entry before it gives up.
const NESTED_HLE_STEPS: u32 = 10_000;

fn special_files(test: &str, id: &str) -> Option<(Vec<u8>, Arc<ElfInfo>)> {
    let path = common::corpus_file_or_skip(test, id, PK_IMAGE)?;
    let image = std::fs::read(path).expect("the verified corpus file is readable");
    let elf = app_elf(test, id)?.expect("`pk` and `official` have an app ELF");
    Some((image, elf))
}

/// The guest calls HLE handlers have outstanding in `m`, leaving out a parked worker's waits
/// (`Continuation::wait`).
fn nested_calls(m: &Machine) -> Vec<(String, u32)> {
    m.hle_section()
        .continuations
        .iter()
        .filter(|(_, c)| !c.wait)
        .map(|(_, c)| (c.handler.handler.to_string(), c.func))
        .collect()
}

/// Runs `m` to the entry of hooked `symbol`, then one instruction at a time until a handler has a
/// guest call outstanding. Returns the calls.
fn to_nested_hle(test: &str, m: &mut Machine, elf: &ElfInfo, symbol: &str) -> String {
    let entry = elf
        .symbols
        .addr_of(symbol)
        .unwrap_or_else(|| panic!("{test}: `{symbol}` is linked"));
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(3_000)),
        max_insns: None,
        stops: StopSet {
            breakpoints: vec![entry],
            ..StopSet::default()
        },
    });
    assert_eq!(
        out.reason,
        StopReason::Breakpoint(entry),
        "{test}: the boot reaches `{symbol}`"
    );
    for _ in 0..NESTED_HLE_STEPS {
        if !nested_calls(m).is_empty() {
            break;
        }
        m.run(RunLimits::insns(1));
    }
    let calls = nested_calls(m);
    assert!(
        !calls.is_empty(),
        "{test}: no handler called into the guest within {NESTED_HLE_STEPS} instructions of \
         `{symbol}`"
    );
    calls
        .iter()
        .map(|(handler, func)| format!("`{handler}` calls {func:#010x}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A special instant's machine, stopped at the instant: `pk` inside the nested call of its
/// `esp_bt_controller_init` handler; `pk` 1000 instructions after 300 bytes were written to its
/// USB console at `bsp_i2c` (236 still in `usj_rx`); `official` at 2.8 s of the recording script,
/// with inputs still journaled and the TCB at 0x3FCAEA78 freed at 453 ms and not yet reused by the
/// tone task at 3.30 s; and `official` at 4.7 s, recording, with both I2S0 directions in flight
/// and samples in `audio_in`.
fn special_machine(test: &str, tag: &str, image: &[u8], elf: &Arc<ElfInfo>) -> (Machine, String) {
    let base = Variant::default();
    match tag {
        "pk-nested-hle" => {
            let mut m = app_machine(image, Some(Arc::clone(elf)), &base);
            let calls = to_nested_hle(test, &mut m, elf, NESTED_HLE_HOOK);
            (m, calls)
        }
        "pk-usj-rx" => {
            let mut m = app_machine(image, Some(Arc::clone(elf)), &base);
            let out = m.run(RunLimits {
                until: None,
                max_insns: Some(BSP_I2C_INSNS),
                stops: bsp_i2c_stops(),
            });
            assert_eq!(out.reason, StopReason::Matcher(BSP_I2C_MATCHER), "{test}");
            m.input(
                At::Now,
                InputEvent::SerialIn {
                    chan: pemu_core::input::SerialChan::USJ,
                    data: vec![b'x'; 300],
                },
            )
            .expect("now is not in the past");
            m.run(RunLimits::insns(1_000));
            (m, "300 bytes written at `bsp_i2c`".to_string())
        }
        "official-2800ms" | "official-4700ms" => {
            let ms = if tag == "official-2800ms" {
                2_800
            } else {
                4_700
            };
            let mut m = scripted_official(image, elf, &base, official_script(true));
            let out = m.run(RunLimits {
                until: Some(VTime::from_ms(ms)),
                max_insns: None,
                stops: StopSet::default(),
            });
            assert_eq!(
                out.reason,
                StopReason::Until,
                "{test}: `official` to {ms} ms"
            );
            if tag == "official-4700ms" {
                // A microphone chunk waits in `audio_in` until the RX period that reads it ends, and 4.7 s falls
                // between that end and the next chunk; step a millisecond at a time to the next chunk's wait.
                for _ in 0..15 {
                    if !m.io().audio_in.is_empty() {
                        break;
                    }
                    let next = VTime(m.now().0 + 1_000_000_000);
                    m.run(RunLimits {
                        until: Some(next),
                        max_insns: None,
                        stops: StopSet::default(),
                    });
                }
            }
            let at = m.now().as_us();
            (m, format!("{at} us of the recording script"))
        }
        other => panic!("{test}: no special instant `{other}`"),
    }
}

fn to_special_end(m: &mut Machine, special: &Special) -> pemu_machine::run::RunOutcome {
    m.run(RunLimits {
        until: Some(VTime::from_ms(special.end_ms)),
        max_insns: None,
        stops: StopSet::default(),
    })
}

/// The child half of a special instant: restore with nothing journaled, run to the end, answer.
fn special_child(test: &str, tag: &str, file: &std::path::Path) {
    let special = SPECIALS
        .iter()
        .find(|s| s.tag == tag)
        .expect("the tag names a special instant");
    let (image, elf) = special_files(test, special.id).expect("the parent read them");
    let bytes = std::fs::read(file).expect("the parent wrote the snapshot");
    let mut m = app_machine(&image, Some(elf), &Variant::default());
    m.restore(&Snapshot::from_bytes(&bytes).expect("parses"))
        .expect("a snapshot of the same run identity restores");
    let since = Since::now(&m);
    let out = to_special_end(&mut m, special);
    determinism::child_answer(file, &report_since(&out.reason, &m, since));
}

/// One special state at one instant: its name, how much the snapshot holds, and what that is.
struct SpecialState {
    state: &'static str,
    count: u64,
    detail: String,
}

/// The special states `m` holds at its snapshot `snap`: nested HLE calls, DMA-driven events in
/// flight, unconsumed `usj_rx` bytes and `audio_in` samples, and journaled inputs stamped after
/// the instant. The reused TCB needs the run after the instant and is [`reused_tcb`].
fn special_states(m: &mut Machine, snap: &pemu_core::snap::Snapshot) -> Vec<SpecialState> {
    use pemu_core::sched::{Owner, Scheduler};
    use pemu_soc_c3::periph::{Peripheral, gdma, i2s0, spi2};

    let calls = nested_calls(m);
    let sched: Scheduler = snap.get().expect("a machine snapshot has the scheduler");
    let dma_blocks = [
        ("I2S0", <i2s0::Model as Peripheral>::ID),
        ("SPI2", <spi2::Master as Peripheral>::ID),
        ("GDMA", <gdma::Engine as Peripheral>::ID),
    ];
    let pending = sched.pending();
    let dma: Vec<(&str, usize)> = dma_blocks
        .iter()
        .map(|(name, id)| {
            let n = pending
                .iter()
                .filter(|(_, _, key)| key.owner == Owner::Periph(*id))
                .count();
            (*name, n)
        })
        .collect();
    let now = m.now();
    let journaled = m.journal().pending().iter().filter(|e| e.at > now).count() as u64;
    let usj_rx = m.io().usj_rx.len() as u64;
    let audio_in = m.io().audio_in.len() as u64;
    vec![
        SpecialState {
            state: "hle-continuation",
            count: calls.len() as u64,
            detail: calls
                .iter()
                .map(|(handler, func)| format!("`{handler}` calls {func:#010x}"))
                .collect::<Vec<_>>()
                .join(", "),
        },
        SpecialState {
            state: "dma",
            count: dma.iter().map(|(_, n)| *n as u64).sum(),
            detail: dma
                .iter()
                .map(|(name, n)| format!("{name} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
                + " pending events",
        },
        SpecialState {
            state: "usj-rx",
            count: usj_rx,
            detail: format!("{usj_rx} bytes the guest has not read"),
        },
        SpecialState {
            state: "audio-in",
            count: audio_in,
            detail: format!("{audio_in} microphone samples the guest has not read"),
        },
        SpecialState {
            state: "journaled-input",
            count: journaled,
            detail: format!("{journaled} inputs stamped after {} us", now.as_us()),
        },
    ]
}

/// The TCBs a `vTaskDelete` freed before the instant (`task_generations`) that a task runs from
/// again before the end, read through `pxCurrentTCBs` every millisecond of a restore of `bytes`.
fn reused_tcb(
    image: &[u8],
    elf: &Arc<ElfInfo>,
    bytes: &[u8],
    deleted: &BTreeSet<u32>,
    special: &Special,
) -> SpecialState {
    let mut reused = Vec::new();
    if !deleted.is_empty() {
        let current = elf
            .symbols
            .addr_of("pxCurrentTCBs")
            .expect("the FreeRTOS of these images links `pxCurrentTCBs`");
        let mut m = app_machine(image, Some(Arc::clone(elf)), &Variant::default());
        m.restore(&Snapshot::from_bytes(bytes).expect("parses"))
            .expect("restores");
        let end = VTime::from_ms(special.end_ms);
        while m.now() < end {
            m.run(RunLimits {
                until: Some(VTime(end.0.min(m.now().0 + 1_000_000_000))),
                max_insns: None,
                stops: StopSet::default(),
            });
            let tcb = m.guest_mem().load(current, 4).unwrap_or(0);
            if deleted.contains(&tcb) && !reused.iter().any(|(t, _)| *t == tcb) {
                reused.push((tcb, m.now()));
            }
        }
    }
    SpecialState {
        state: "reused-tcb",
        count: reused.len() as u64,
        detail: if reused.is_empty() {
            format!(
                "{} TCBs freed before the instant, none current again",
                deleted.len()
            )
        } else {
            reused
                .iter()
                .map(|(tcb, at)| {
                    format!(
                        "TCB {tcb:#010x} freed before the instant, current again at {} us",
                        at.as_us()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        },
    }
}

/// Prints one `RAN` line per special state some instant held, and `NOT_RUN <test>
/// special-states` for a state none held.
fn report_special_states(test: &str, held: &[(&str, Vec<SpecialState>)]) {
    let names = [
        "hle-continuation",
        "dma",
        "usj-rx",
        "audio-in",
        "journaled-input",
        "reused-tcb",
    ];
    for name in names {
        let at: Vec<(&str, &SpecialState)> = held
            .iter()
            .flat_map(|(tag, states)| {
                states
                    .iter()
                    .filter(|s| s.state == name)
                    .map(move |s| (*tag, s))
            })
            .collect();
        let with: Vec<String> = at
            .iter()
            .filter(|(_, s)| s.count > 0)
            .map(|(tag, s)| format!("{tag}: {}", s.detail))
            .collect();
        if with.is_empty() {
            let counts: Vec<String> = at
                .iter()
                .map(|(tag, s)| format!("{tag}: {}", s.detail))
                .collect();
            println!(
                "NOT_RUN {test} special-states: {name}: no instant holds it ({})",
                counts.join("; ")
            );
        } else {
            println!(
                "RAN {test} {name}: a snapshot holding it restores in a fresh process and \
                 continues equally: {}",
                with.join("; ")
            );
        }
    }
}

/// Virtual time restore equivalence runs past its instant: 2 s.
const RESTORE_WINDOW_PS: u64 = 2_000_000_000_000;

enum Instant {
    Line(&'static str),
    At(VTime),
    /// Inside a nested HLE call, stopped in the hooked function named ([`to_nested_hle`]).
    NestedHle(&'static str),
}

/// One restore-equivalence instant: corpus id, tag, where, and whether `official`'s clicks into
/// the Audio demo are journaled before power-on.
struct Case {
    id: &'static str,
    tag: &'static str,
    instant: Instant,
    clicks: bool,
}

/// The settled `official` menu with key presses: 1.9 s, after the first DOWN, with DOWN, OK and
/// OK still journaled, so the next 2 s open the Audio demo and play its tone.
const MENU_MS: u64 = 1_900;

/// One instant per image: `pk` at `bsp_i2c`, `official` on its menu with key presses, and
/// `goldminer` at 300 ms, booting.
fn short_cases() -> Vec<Case> {
    vec![
        Case {
            id: common::PK,
            tag: "short",
            instant: Instant::Line(BSP_I2C),
            clicks: false,
        },
        Case {
            id: common::OFFICIAL,
            tag: "short",
            instant: Instant::At(VTime::from_ms(MENU_MS)),
            clicks: true,
        },
        Case {
            id: common::GOLDMINER,
            tag: "short",
            instant: Instant::At(VTime::from_ms(300)),
            clicks: false,
        },
    ]
}

/// Every instant of the T2 variant: `pk` at 20 ms, `bsp_i2c` and 250 ms; `pk` inside the nested
/// call of its BLE init hook; `official` at 1 s and on its menu with key presses; `goldminer` at
/// 1 s.
fn all_cases() -> Vec<Case> {
    let at = |ms| Instant::At(VTime::from_ms(ms));
    let case = |id, tag, instant, clicks| Case {
        id,
        tag,
        instant,
        clicks,
    };
    vec![
        case(common::PK, "rom", at(20), false),
        case(common::PK, "bsp-i2c", Instant::Line(BSP_I2C), false),
        case(common::PK, "display", at(250), false),
        case(
            common::PK,
            "nested-hle",
            Instant::NestedHle(NESTED_HLE_HOOK),
            false,
        ),
        case(common::OFFICIAL, "boot", at(1_000), false),
        case(common::OFFICIAL, "menu", at(MENU_MS), true),
        case(common::GOLDMINER, "boot", at(1_000), false),
    ]
}

fn case_image(test: &str, id: &str) -> Option<Vec<u8>> {
    let name = IMAGES
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, n)| *n)
        .expect("a ROM banner image");
    let path = common::corpus_file_or_skip(test, id, name)?;
    Some(std::fs::read(path).expect("the verified corpus file is readable"))
}

/// A machine over `image` with the app ELF of `case`'s corpus id, run from power-on to the
/// instant, with the clicks journaled when the case has them. Returns what it stopped on.
fn at_instant(
    test: &str,
    case: &Case,
    image: &[u8],
    elf: Option<Arc<ElfInfo>>,
) -> (Machine, String) {
    let base = Variant::default();
    let mut m = match (&elf, case.clicks) {
        (Some(elf), true) => scripted_official(image, elf, &base, official_script(false)),
        _ => app_machine(image, elf.clone(), &base),
    };
    let lim = match case.instant {
        Instant::Line(text) => RunLimits {
            until: None,
            max_insns: Some(BSP_I2C_INSNS),
            stops: StopSet {
                matchers: vec![(
                    BSP_I2C_MATCHER,
                    Matcher::Serial {
                        stream: SerialStream::UsjTx,
                        pattern: LinePattern::Contains(text.into()),
                    },
                )],
                ..StopSet::default()
            },
        },
        Instant::At(t) => RunLimits {
            until: Some(t),
            max_insns: None,
            stops: StopSet::default(),
        },
        Instant::NestedHle(symbol) => {
            let elf = elf.expect("a nested HLE instant needs the app ELF");
            let calls = to_nested_hle(test, &mut m, &elf, symbol);
            return (m, calls);
        }
    };
    let out = m.run(lim);
    assert!(
        matches!(out.reason, StopReason::Matcher(_) | StopReason::Until),
        "{test}: the instant is reached: {:?} at {:?}",
        out.reason,
        out.vt
    );
    let at = format!("{} us", m.now().as_us());
    (m, at)
}

/// Runs `m` 2 s past `from` and reports over the output since `since`, with the digest of the
/// `hang` section; also returns the stop reason.
fn window(m: &mut Machine, from: VTime, since: Since) -> (String, StopReason) {
    let out = m.run(RunLimits {
        until: Some(VTime(from.0 + RESTORE_WINDOW_PS)),
        max_insns: None,
        stops: StopSet::default(),
    });
    // Restore equivalence also compares the `hang` section `state_hash` leaves out.
    let text = format!(
        "{} hang={}",
        report_since(&out.reason, m, since),
        hex(&hang_digest(m))
    );
    (text, out.reason)
}

/// Records what a window compared: a run that stopped early prints `NOT_RUN window-short`, and the
/// frames and PCM after the instant go to `coverage`.
fn note_window(
    test: &str,
    what: &str,
    m: &mut Machine,
    reason: &StopReason,
    (from, since): (VTime, Since),
    coverage: &mut determinism::Coverage,
) {
    let end = VTime(from.0 + RESTORE_WINDOW_PS);
    if m.now() < end {
        println!(
            "NOT_RUN {test} window-short: {what}: the run stopped {} ms after the instant, before \
             the 2 s window ended: {reason:?}",
            (m.now().0 - from.0) / 1_000_000_000
        );
    }
    coverage.output(what, m, since);
}

/// The child half: restore the snapshot tagged `<id>-<tag>` with nothing journaled, run the
/// window, answer.
fn restore_equivalence_child(test: &str, file: &std::path::Path) {
    let tag = determinism::child_tag(file);
    let (id, _) = IMAGES
        .iter()
        .find(|(id, _)| tag.starts_with(id))
        .expect("the tag names a corpus id");
    let image = case_image(test, id).expect("the parent read it");
    let elf = app_elf(test, id).expect("the parent read it");
    let bytes = std::fs::read(file).expect("the parent wrote the snapshot");
    let mut m = app_machine(&image, elf, &Variant::default());
    m.restore(&Snapshot::from_bytes(&bytes).expect("parses"))
        .expect("a snapshot of the same run identity restores");
    let since = Since::now(&m);
    let from = m.now();
    determinism::child_answer(file, &window(&mut m, from, since).0);
}

/// Three runs from one instant: straight, restored in this process, and restored in a fresh
/// process. Returns the two in-process reports and the snapshot, or `None` after the skip line.
fn three_runs(
    test: &str,
    case: &Case,
    coverage: &mut determinism::Coverage,
) -> Option<(String, String, Vec<u8>)> {
    let image = case_image(test, case.id)?;
    let elf = app_elf(test, case.id)?;
    let (mut straight, at) = at_instant(test, case, &image, elf.clone());
    let bytes = straight
        .snapshot(SnapOpts::default())
        .to_bytes()
        .expect("a machine snapshot serializes");
    let since = Since::now(&straight);
    let from = straight.now();
    let (want, reason) = window(&mut straight, from, since);
    let what = format!("`{}` {} ({at})", case.id, case.tag);
    note_window(test, &what, &mut straight, &reason, (from, since), coverage);

    let mut again = app_machine(&image, elf, &Variant::default());
    again
        .restore(&Snapshot::from_bytes(&bytes).expect("parses"))
        .expect("a snapshot of the same run identity restores");
    // Each machine takes its own mark: the frame generation restarts at a restore.
    let own = Since::now(&again);
    let (in_process, _) = window(&mut again, from, own);
    Some((want, in_process, bytes))
}

/// The three runs of every case, the fresh-process leg for all at once, and the coverage lines.
fn restore_equivalence(test: &str, cases: &[Case]) {
    let mut coverage = determinism::Coverage::default();
    let mut snaps = Vec::new();
    let mut wants = Vec::new();
    for case in cases {
        let Some((want, in_process, bytes)) = three_runs(test, case, &mut coverage) else {
            continue;
        };
        let name = format!("{}-{}", case.id, case.tag);
        assert_eq!(in_process, want, "{test}: `{name}`: in-process restore");
        snaps.push((name.clone(), bytes));
        wants.push((name, want));
    }
    let answers = determinism::fresh_process(test, &snaps);
    for ((name, want), got) in wants.iter().zip(&answers) {
        assert_eq!(got, want, "{test}: `{name}`: fresh-process restore");
        println!("RAN {test} {name}");
        println!("restore equivalence `{name}`: three runs equal: {want}");
    }
    coverage.report(test);
}

/// From one instant per image, the straight run and both restores have equal `state_hash`,
/// console, frame and PCM hashes after 2 s more, and equal `hang` sections.
///
/// The digests cover the output after the instant, from each machine's own mark. The machines
/// carry the app ELF, so `pk` runs past BLE init and `official`'s window plays the tone.
#[test]
fn t1_m3_restore_equivalence_short() {
    let test = "t1_m3_restore_equivalence_short";
    if let Some(file) = determinism::child_snapshot() {
        restore_equivalence_child(test, &file);
        return;
    }
    restore_equivalence(test, &short_cases());
}

/// The same at every instant of [`all_cases`] (nightly), among them the `official` menu with key
/// presses and `pk` paused inside the guest call of its `ble.init` handler. The same instant
/// inside the U4 magic ISR is `t1_m8_u4_gate_4`.
#[test]
fn t2_m3_restore_equivalence_all_instants() {
    let test = "t2_m3_restore_equivalence_all_instants";
    if let Some(file) = determinism::child_snapshot() {
        restore_equivalence_child(test, &file);
        return;
    }
    restore_equivalence(test, &all_cases());
}

// ---------------------------------------------------------------------------------------------
// The magic range and the radio pages
// ---------------------------------------------------------------------------------------------

/// A machine over corpus image `id` with its app ELF when the id has one, or `None` after a
/// printed skip.
fn hle_machine(test: &str, id: &str, image: &str, elf: Option<&str>) -> Option<Machine> {
    let path = common::corpus_file_or_skip(test, id, image)?;
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses");
    let elf = match elf {
        Some(name) => {
            let path = common::corpus_file_or_skip(test, id, name)?;
            let bytes = std::fs::read(&path).expect("the verified ELF is readable");
            Some(Arc::new(
                ElfInfo::parse(&bytes).expect("the pinned ELF parses"),
            ))
        }
        None => None,
    };
    let assets = Assets::with_bundled_rom(flash, elf, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    Some(Machine::new(MachineConfig::default(), assets).expect("the image fits"))
}

/// Zero fetches in the magic range 0x4005ECC0-0x4005EE3B over the `pk`, `official` and
/// `goldminer` boots, and the radio pages see exactly the 14 `rtc_sleep_pu` accesses.
///
/// Every unallocated pc of the magic range is a `MagicRangeFetch` tripwire and every CPU access to
/// a radio window is checked against the boot allowance, so a stray access ends the run with
/// `StopReason::Tripwire`. Each boot runs 2 s. `pk` may also end at the `DisabledFeature` tripwire
/// at `esp_bt_controller_init`; `goldminer` has no ELF, so only the symbol-free stops apply.
#[test]
fn t1_m3_magic_range_and_radio_pages() {
    let test = "t1_m3_magic_range_and_radio_pages";
    let id = test.to_string();
    let boots = [
        (common::PK, PK_IMAGE, Some(PK_APP_ELF), "bsp_lvgl"),
        (
            common::OFFICIAL,
            "FoloToy-AI-Passport-8MB.bin",
            Some("FoloToy-AI-Passport.elf"),
            "Returned from app_main()",
        ),
        (
            common::GOLDMINER,
            "goldminer-sanitized-8MB.bin",
            None,
            "Returned from app_main()",
        ),
    ];
    for (image_id, image, elf, reached) in boots {
        let Some(mut m) = hle_machine(test, image_id, image, elf) else {
            return;
        };
        assert!(m.magic_range_hooks() >= 185, "{id} {image_id}");
        let out = m.run(RunLimits {
            until: Some(VTime::from_ms(2_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        let console = String::from_utf8_lossy(&console_from(&mut m, 0)).into_owned();
        match (&out.reason, image_id) {
            (StopReason::Until, _) => {}
            (StopReason::Tripwire(report), common::PK)
                if report.kind == pemu_machine::hle::HleTripKind::DisabledFeature => {}
            (other, _) => panic!(
                "{id} {image_id}: the boot ended {other:?} at pc {:#010x}, console tail:\n{}",
                m.hart().pc,
                console.lines().rev().take(6).collect::<Vec<_>>().join("\n")
            ),
        }
        assert!(
            console.contains(reached),
            "{id} {image_id}: `{reached}` missing"
        );
        assert_eq!(
            m.radio_accesses(),
            14,
            "{id} {image_id}: the radio pages saw another number of boot accesses"
        );
    }
}

/// `hle_probe` with HLE bound: the BLE module finds none of its VHCI functions linked, no binding
/// covers the probe's `hle_probe_hook_*` symbols, and the lines from `PROBE` to `DONE` equal
/// `tests/fw/captures/hle_probe.txt`, as on silicon. Binding arms nothing an ordinary IDF app
/// reaches, and the magic range and radio pages stay quiet.
#[test]
fn t1_hle_probe_passes_with_hle_bound_and_native_bodies() {
    let test = "t1_hle_probe_passes_with_hle_bound_and_native_bodies";
    // Probe images are not corpus ids: `xtask probes` builds them into `corpus/probes/` of the data
    // root the `pk` corpus id resolves in, and pins each in `tests/fw/manifest.toml`.
    let Some(pk) = common::corpus_file_or_skip(test, common::PK, PK_IMAGE) else {
        return;
    };
    let fw = workspace().join("tests/fw");
    let path = pk
        .ancestors()
        .nth(2)
        .expect("a corpus file sits under corpus/<id>/")
        .join("probes/hle_probe-8MB.bin");
    let Ok(bytes) = std::fs::read(&path) else {
        common::skip(
            test,
            "corpus/probes/hle_probe-8MB.bin is not built (xtask probes)",
        );
        return;
    };
    let manifest = std::fs::read_to_string(fw.join("manifest.toml")).expect("the probe manifest");
    let pinned = manifest
        .split("[[probe]]")
        .find(|block| block.contains("name = \"hle_probe\""))
        .and_then(|block| {
            block
                .lines()
                .find_map(|l| l.strip_prefix("merged_sha256 = \""))
                .map(|v| v.trim_end_matches('"').to_string())
        })
        .expect("tests/fw/manifest.toml pins the hle_probe merged image");
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&bytes),
        pinned,
        "{test}: corpus/probes/hle_probe-8MB.bin is not the pinned build"
    );
    let elf = ElfInfo::parse(&std::fs::read(fw.join("hle_probe.elf")).expect("tests/fw ELF"))
        .expect("the probe ELF parses");
    let flash = FlashImage::from_merged(&bytes).expect("a probe image parses");
    let assets = Assets::with_bundled_rom(flash, Some(Arc::new(elf)), None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    let mut m = Machine::new(MachineConfig::default(), assets).expect("the image fits");
    let done = MatcherId(0xD0);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(3_000)),
        max_insns: None,
        stops: StopSet {
            matchers: vec![(
                done,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Prefix("DONE|".into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let mut console = String::from_utf8_lossy(&console_from(&mut m, 0)).into_owned();
    assert_eq!(out.reason, StopReason::Matcher(done), "{test}:\n{console}");
    let after = m.now().as_us() + 500_000;
    m.run(RunLimits {
        until: Some(VTime::from_us(after)),
        max_insns: None,
        stops: StopSet::default(),
    });
    console.push_str(&String::from_utf8_lossy(&console_from(
        &mut m,
        console.len() as u64,
    )));
    let probe_lines = |text: &str| -> Vec<String> {
        text.lines()
            .map(|l| l.trim_end_matches('\r'))
            .skip_while(|l| !l.starts_with("PROBE|"))
            .take_while(|l| !l.starts_with("I ("))
            .map(str::to_string)
            .collect()
    };
    let capture = std::fs::read_to_string(fw.join("captures/hle_probe.txt")).expect("capture");
    assert_eq!(probe_lines(&console), probe_lines(&capture), "{test}");
    assert_eq!(
        m.hle_state().observed[3],
        5,
        "{test}: vTaskDelete observe hook fires"
    );
    assert_eq!(m.radio_accesses(), 14, "{test}");
}

// ---------------------------------------------------------------------------------------------
// The agent loop through the real binary
// ---------------------------------------------------------------------------------------------
//
// The `passportsim` executable runs as child processes, so the daemon is the auto-spawned
// `serve --headless` and MCP is the stdio adapter. `fw` is positional: `start pk`.
//
// Every child runs with a fresh `PASSPORTSIM_HOME` and without `PASSPORTSIM_DATA_ROOT`,
// `PASSPORTSIM_CONFIG_DIR` and any `PASSPORTSIM_CORPUS_<ID>`, so a daemon the user runs is never
// found, stopped or reused; a guard runs `serve --stop` however the test ends.

#[test]
fn t0_m3_binary_build_mirrors_the_test_target_layout() {
    let root = std::env::temp_dir().join(format!("pemu-agent-loop-layout-{}", std::process::id()));
    std::fs::create_dir_all(root.join("aarch64-apple-darwin").join("release")).expect("dirs");
    std::fs::create_dir_all(root.join("debug")).expect("dirs");
    std::fs::write(
        root.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55",
    )
    .expect("tag");
    let text = |args: Vec<std::ffi::OsString>| {
        args.iter()
            .map(|arg| {
                arg.to_string_lossy()
                    .replace(&*root.to_string_lossy(), "<t>")
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        text(build_args(&root.join("debug"))),
        ["--target-dir", "<t>"]
    );
    assert_eq!(
        text(build_args(
            &root.join("aarch64-apple-darwin").join("release")
        )),
        [
            "--release",
            "--target-dir",
            "<t>",
            "--target",
            "aarch64-apple-darwin"
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}

struct IsolatedHome {
    bin: PathBuf,
    home: PathBuf,
}

impl IsolatedHome {
    fn new(image: &std::path::Path) -> IsolatedHome {
        let home = std::env::temp_dir().join(format!(
            "pemu-agent-loop-bin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        let config = home.join("config");
        std::fs::create_dir_all(&config).expect("a temporary home");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for dir in [&home, &config] {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .expect("owner-only");
            }
        }
        let image = image.to_str().expect("a UTF-8 corpus path");
        assert!(
            !image.contains(['"', ',']),
            "the corpus map reader takes no quote or comma"
        );
        std::fs::write(
            config.join("corpus.toml"),
            format!("[{}]\nbin = \"{image}\"\n", common::PK),
        )
        .expect("the corpus map");
        IsolatedHome {
            bin: passportsim(),
            home,
        }
    }

    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(&self.bin);
        command.args(args).env("PASSPORTSIM_HOME", &self.home);
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|key| {
                key == "PASSPORTSIM_DATA_ROOT"
                    || key == "PASSPORTSIM_CONFIG_DIR"
                    || key.starts_with("PASSPORTSIM_CORPUS_")
            }) {
                command.env_remove(&key);
            }
        }
        command
    }

    fn cli(&self, args: &[&str]) -> (i32, String, String) {
        let out = self
            .command(args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("passportsim runs");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8(out.stdout).expect("UTF-8 stdout"),
            String::from_utf8(out.stderr).expect("UTF-8 stderr"),
        )
    }

    /// One `--output json` call that must exit `code`, validated against its generated schema.
    #[track_caller]
    fn json(&self, command: &str, args: &[&str], code: i32) -> Value {
        let mut argv = vec![command];
        argv.extend_from_slice(args);
        argv.extend_from_slice(&["--output", "json"]);
        let (exit, stdout, stderr) = self.cli(&argv);
        common::assert_cli_exit(
            exit,
            code,
            &stdout,
            &format!("passportsim {argv:?}: {stderr}"),
        );
        let value: Value = serde_json::from_str(stdout.trim_end())
            .unwrap_or_else(|e| panic!("passportsim {argv:?} printed no JSON: {e}: {stdout}"));
        check_outcome(command, code == 0, &value, &value);
        value
    }

    fn discovery(&self) -> PathBuf {
        self.home.join("run").join(daemon::DISCOVERY_FILE)
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        // Idempotent: a daemon that already stopped answers "no daemon is running".
        let _ = self.cli(&["serve", "--stop"]);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn binary_cli_loop(home: &IsolatedHome, until: &str) {
    let wall = WALL_BUDGET_MS.to_string();
    let start = home.json("start", &[common::PK, "--boot", "none"], 0);
    let id = start["instance"]
        .as_str()
        .expect("an instance id")
        .to_string();
    assert!(
        home.discovery().is_file(),
        "`start` auto-spawned the daemon and it published its discovery file"
    );
    let run = home.json(
        "run",
        &[until, "--timeout", RUN_TIMEOUT, "--wall-budget-ms", &wall],
        0,
    );
    assert_eq!(run["status"], "matched", "run: {run}");
    let serial = home.json("serial", &["read", "--cursor", "0"], 0);
    let status = home.json("status", &[], 0);
    let stop = home.json("stop", &[&id], 0);
    check_loop_fields(&id, &start, Some(&run), &serial, &status, &stop);

    // The instance's artifacts were finished on `stop`, under this home's data root.
    let artifacts = home.home.join("data").join("artifacts");
    let finished = std::fs::read_dir(&artifacts)
        .expect("the daemon's artifacts root")
        .filter_map(Result::ok)
        .any(|run_dir| run_dir.path().join(&id).join("summary.json").is_file());
    assert!(
        finished,
        "summary.json of {id} under {}",
        artifacts.display()
    );

    // The daemon keeps running with no instance, and the text rendering says so.
    let (code, text, stderr) = home.cli(&["status"]);
    assert_eq!(code, 0, "{text}{stderr}");
    assert!(text.starts_with("no instance is running\n"), "{text}");
}

struct McpChild {
    child: std::process::Child,
    input: Option<std::process::ChildStdin>,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl McpChild {
    fn spawn(home: &IsolatedHome) -> McpChild {
        let mut child = home
            .command(&["mcp"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("passportsim mcp runs");
        let input = child.stdin.take().expect("piped stdin");
        let output = std::io::BufReader::new(child.stdout.take().expect("piped stdout"));
        McpChild {
            child,
            input: Some(input),
            output,
        }
    }

    /// Writes one JSON-RPC line; reads one answer line unless it is a notification.
    fn send(&mut self, message: &Value) -> Option<Value> {
        use std::io::{BufRead as _, Write as _};
        let input = self.input.as_mut().expect("the session is open");
        writeln!(input, "{message}").expect("the adapter reads stdin");
        input.flush().expect("flush");
        message.get("id")?;
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .expect("the adapter answers on stdout");
        Some(serde_json::from_str(&line).unwrap_or_else(|e| {
            panic!("the adapter's stdout carries JSON-RPC only: {e}: {line:?}")
        }))
    }

    fn tool(&mut self, rpc_id: u64, command: &str, arguments: Value) -> (bool, Value) {
        let answer = self
            .send(&serde_json::json!({
                "jsonrpc": "2.0", "id": rpc_id, "method": "tools/call",
                "params": { "name": format!("passport_{command}"), "arguments": arguments },
            }))
            .expect("a request is answered");
        assert_eq!(answer["id"], rpc_id, "{answer}");
        let result = &answer["result"];
        let ok = result["isError"] == false;
        let content = result["structuredContent"].clone();
        check_outcome(command, ok, &content, &content["error"]);
        assert!(result["content"][0]["text"].is_string(), "{answer}");
        (ok, content)
    }

    /// Closes stdin, which ends the session; the adapter exits 0.
    fn finish(mut self) {
        drop(self.input.take());
        let status = self.child.wait().expect("the adapter exits");
        assert!(status.success(), "`passportsim mcp` exited {status}");
    }
}

impl Drop for McpChild {
    /// A session a failed assertion left open: the adapter is killed and reaped.
    fn drop(&mut self) {
        drop(self.input.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn binary_mcp_loop(home: &IsolatedHome, until: &str) {
    let mut mcp = McpChild::spawn(home);
    let init = mcp
        .send(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-11-25" } }))
        .expect("initialize is answered");
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
    assert!(
        mcp.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .is_none()
    );
    assert!(home.discovery().is_file(), "`mcp` auto-spawned the daemon");
    let tools = mcp
        .send(&serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .expect("tools/list is answered");
    for command in LOOP {
        let tool = tools["result"]["tools"]
            .as_array()
            .expect("a tool list")
            .iter()
            .find(|t| t["name"] == format!("passport_{command}"))
            .unwrap_or_else(|| panic!("tools/list offers passport_{command}"));
        assert_eq!(tool["outputSchema"], output_schema(command));
    }
    let (ok, start) = mcp.tool(
        3,
        "start",
        serde_json::json!({ "fw": common::PK, "boot": "none" }),
    );
    assert!(ok, "start: {start}");
    let id = start["instance"].as_str().expect("an id").to_string();
    let (ok, run) = mcp.tool(
        4,
        "run",
        serde_json::json!({ "instance": id, "until": until, "timeout": RUN_TIMEOUT, "wall_budget_ms": WALL_BUDGET_MS }),
    );
    assert!(ok && run["status"] == "matched", "run: {run}");
    let (ok, serial) = mcp.tool(
        5,
        "serial",
        serde_json::json!({ "instance": id, "op": "read", "cursor": 0 }),
    );
    assert!(ok, "serial: {serial}");
    let (ok, status) = mcp.tool(6, "status", serde_json::json!({ "instance": id }));
    assert!(ok, "status: {status}");
    let (ok, stop) = mcp.tool(7, "stop", serde_json::json!({ "instance": id }));
    assert!(ok, "stop: {stop}");
    check_loop_fields(&id, &start, Some(&run), &serial, &status, &stop);
    mcp.finish();
}

/// The agent loop through the `passportsim` binary: the CLI loop against the daemon `start`
/// spawns, `serve --stop`, then the MCP loop against the daemon `mcp` spawns.
#[test]
fn t1_m3_agent_loop_bsp_i2c_binary() {
    let test = "t1_m3_agent_loop_bsp_i2c_binary";
    let Some(image) = common::corpus_file_or_skip(test, common::PK, PK_IMAGE) else {
        return;
    };
    let home = IsolatedHome::new(&image);
    let until = "serial:/bsp_i2c/";

    let (code, text, stderr) = home.cli(&["serve", "--stop"]);
    assert_eq!(
        (code, text.as_str()),
        (0, "no daemon is running\n"),
        "a fresh home has no daemon: {stderr}"
    );
    binary_cli_loop(&home, until);

    let (code, text, stderr) = home.cli(&["serve", "--stop"]);
    assert_eq!(code, 0, "{text}{stderr}");
    assert!(text.starts_with("stopped the daemon on port "), "{text}");
    assert!(
        !home.discovery().exists(),
        "the stopped daemon removed its discovery file"
    );

    binary_mcp_loop(&home, until);
    let (code, text, stderr) = home.cli(&["serve", "--stop"]);
    assert_eq!(code, 0, "{text}{stderr}");
    assert!(text.starts_with("stopped the daemon on port "), "{text}");
}

// ---------------------------------------------------------------------------------------------
// The boots of M3
// ---------------------------------------------------------------------------------------------

/// Virtual time the device ran before esptool reset it into the captured boot.
const DEVICE_LIKE_PS: u64 = 300_000_000_000;

/// Lines of the derived device golden the `pk` boot claims: dev:L4 to dev:L61.
const PK_BOOT_LINES: usize = 58;

/// Lines of the device-like `rom-banner` variant: dev:L4 to dev:L13.
const ROM_BANNER_LINES: usize = 10;

const BOOT_LINE: MatcherId = MatcherId(0x31);

fn line_stop(pattern: LinePattern) -> StopSet {
    StopSet {
        matchers: vec![(
            BOOT_LINE,
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern,
            },
        )],
        ..StopSet::default()
    }
}

/// The device-like line reset: power on, run 300 ms, apply `UsbLine {rts: 1, dtr: 0}`, the esptool
/// hard reset that made the captured boot `rst:0x15`.
fn device_like_reset(test: &str, m: &mut Machine) {
    let out = m.run(RunLimits {
        until: Some(VTime(DEVICE_LIKE_PS)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{test}: the first boot ends before 300 ms at pc {:#010x}",
        m.hart().pc
    );
    m.input(
        pemu_machine::machine::At::Now,
        pemu_core::input::InputEvent::UsbLine {
            dtr: false,
            rts: true,
        },
    )
    .expect("now is not in the past");
}

fn console_tail(console: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(console);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// `pk`: the normalized last boot of scenario `pk-boot-device` equals dev:L4 to dev:L61 of the
/// derived device golden (58 lines, `Saved PC:` and the Passport Keys version masked), and the
/// `rom-banner` variant's last banner equals dev:L4 to dev:L13.
///
/// Both run 300 ms and then the esptool line reset, so the compared boot is the captured
/// `rst:0x15` one. The golden is derived on this host and never committed, so the comparison
/// skips with the loader's reason where it is absent.
#[test]
fn t1_m3_pk_boot_device_to_bsp_i2c() {
    let test = "t1_m3_pk_boot_device_to_bsp_i2c";
    let id = test.to_string();
    let Some((_, image)) = pk_image_file(test) else {
        return;
    };
    let Some(golden) = common::derived_golden_or_skip(test, "pk.console.txt") else {
        return;
    };

    let mut m = image_machine(&image);
    device_like_reset(test, &mut m);
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(BSP_I2C_INSNS),
        stops: line_stop(LinePattern::Contains(BSP_I2C.into())),
    });
    let console = console_from(&mut m, 0);
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: the reset boot never printed `{BSP_I2C}`; tail:\n{}",
        console_tail(&console, 8)
    );
    let compared =
        common::assert_console_prefix("pk.console.txt", &golden, &console, Some(PK_BOOT_LINES));
    assert_eq!(compared, PK_BOOT_LINES, "{id}");

    let mut m = image_machine(&image);
    device_like_reset(test, &mut m);
    let out = m.run(to_entry(StopSet::default()));
    let console = console_from(&mut m, 0);
    assert_eq!(
        out.reason,
        StopReason::Matcher(ENTRY),
        "{id}: the reset boot never printed `entry`; tail:\n{}",
        console_tail(&console, 8)
    );
    common::assert_console_prefix("pk.console.txt", &golden, &console, Some(ROM_BANNER_LINES));
    println!(
        "RAN {test} pk-boot-device: {PK_BOOT_LINES} lines; rom-banner: {ROM_BANNER_LINES} lines"
    );
}

/// A corpus boot from power-on: the machine and its device console, or `None` after the skip line.
fn boot(
    test: &str,
    id: &str,
    file: &str,
    limits: RunLimits,
) -> Option<(Machine, StopReason, String)> {
    let path = common::corpus_file_or_skip(test, id, file)?;
    let image = std::fs::read(&path).expect("the verified corpus file is readable");
    let mut m = image_machine(&image);
    let out = m.run(limits);
    let console = String::from_utf8_lossy(&console_from(&mut m, 0)).into_owned();
    Some((m, out.reason, console))
}

/// The lines `official` must print, without their prefix: the demo banner, the `bsp_i2c` line and
/// the two heap region lines that differ from `pk`'s.
const OFFICIAL_LINES: [&str; 4] = [
    "heap_init: At 3FCABCC0 len 00014340 ",
    "heap_init: At 50000044 len 00001FA4 ",
    "main: FoloToy AI Passport BSP demo 启动",
    BSP_I2C,
];

/// `official` prints its demo banner and the `bsp_i2c` line, and its heap region lines include
/// `3FCABCC0 len 00014340` and RTC RAM `50000044 len 00001FA4`.
#[test]
fn t1_m3_official_reaches_bsp_i2c() {
    let test = "t1_m3_official_reaches_bsp_i2c";
    let id = test.to_string();
    let Some((m, reason, console)) = boot(
        test,
        common::OFFICIAL,
        "FoloToy-AI-Passport-8MB.bin",
        RunLimits {
            until: None,
            max_insns: Some(BSP_I2C_INSNS),
            stops: line_stop(LinePattern::Contains(BSP_I2C.into())),
        },
    ) else {
        return;
    };
    let tail = console_tail(console.as_bytes(), 8);
    assert_eq!(
        reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: `official` stopped at pc {:#010x} before `{BSP_I2C}`; tail:\n{tail}",
        m.hart().pc
    );
    let lines: Vec<&str> = console.lines().map(untimed).collect();
    let mut at = 0;
    for want in OFFICIAL_LINES {
        let Some(found) = lines[at..].iter().position(|l| l.starts_with(want)) else {
            panic!("{id}: `{want}` missing or out of order; tail:\n{tail}");
        };
        at += found + 1;
    }
}

/// The blocks of the M3 strict set (`spi_mem` as `spi0` and `spi1`, `gpio`/`iomux` as both,
/// `timg` as `timg0` and `timg1`).
const M3_STRICT_BLOCKS: [&str; 21] = [
    "efuse",
    "rtc_cntl",
    "regi2c",
    "system",
    "apb_ctrl",
    "uart0",
    "usj",
    "gpio",
    "iomux",
    "spi0",
    "spi1",
    "flash_xmc",
    "intc",
    "sha",
    "extmem",
    "mmu",
    "timg0",
    "timg1",
    "systimer",
    "sensitive",
    "assist_debug",
];

/// The single registers the M3 strict set adds as store (`uart1` +0x010).
const M3_STRICT_REGISTERS: [(&str, u32); 1] = [("uart1", 0x010)];

/// Virtual time `goldminer` runs for.
const GOLDMINER_RUN_PS: u64 = 5_000_000_000_000;

fn row_name(id: pemu_core::sched::PeriphId) -> &'static str {
    BLOCKS
        .iter()
        .find(|b| b.id == id)
        .map_or("<not a block>", |b| b.name)
}

/// The class the model of block `periph` claims for the register at `off`
/// (`Peripheral::fidelity`).
///
/// `FidelityLedger::unmodeled` reads class notes, which only the I2C0, SARADC and I2S0 models
/// write, so the models answer here instead. A class is a property of the register, so a default
/// instance answers for the running one.
fn model_class(
    devices: &mut pemu_soc_c3::periph::Devices,
    periph: pemu_core::sched::PeriphId,
    off: u32,
) -> pemu_core::fidelity::Fidelity {
    struct Class {
        off: u32,
        class: pemu_core::fidelity::Fidelity,
    }
    impl pemu_soc_c3::periph::DeviceVisitor for Class {
        fn visit<P: pemu_soc_c3::periph::Peripheral>(&mut self, dev: &mut P) {
            self.class = dev.fidelity(self.off);
        }
    }
    let mut v = Class {
        off,
        class: pemu_core::fidelity::Fidelity::U,
    };
    devices.visit(periph, &mut v);
    v.class
}

/// Offsets of the M3 strict set that are not registers: the model reads 0, drops the write and
/// ledgers the offset.
///
/// - `efuse` +0x18C, the reserved word between `EFUSE_RD_REPEAT_ERR3` and `ERR4`, is absent from
///   the IDF v5.5.3 header and `specs/c3-registers.csv`; a driver loop steps across it. What the
///   silicon answers there is UNVERIFIED until a probe and a device capture settle it.
const GOLDMINER_ALLOWED_OFFSETS: [(&str, u32, &str); 1] = [(
    "efuse",
    0x18C,
    "the reserved word between EFUSE_RD_REPEAT_ERR3 and ERR4: no register, no RegSpec, no class",
)];

/// The first-touched strict-set registers allowed to answer `Fidelity::U`: empty. An entry turns
/// [`t1_m3_goldminer_runs_5_s_strict`] into a `SKIP` that names it, because no class covers a
/// wrong answer.
const GOLDMINER_STILL_U: [(&str, u32, &str); 0] = [];

/// `goldminer` prints `main_task: Calling app_main()` and runs 5 s without `E_UNMODELED` inside
/// the M3 strict set.
///
/// `Strictness` is still a placeholder, so the check is on the record an `Unmodeled` stop is made
/// from: no first touch in the set may be of a register whose model claims no class above U
/// ([`model_class`]). The run first-touches 503 registers of the set, each with a class row in
/// `specs/blocks/<block>.toml`. The one unclassed touch is [`GOLDMINER_ALLOWED_OFFSETS`].
#[test]
fn t1_m3_goldminer_runs_5_s_strict() {
    let test = "t1_m3_goldminer_runs_5_s_strict";
    let id = test.to_string();
    let Some((m, reason, console)) = boot(
        test,
        common::GOLDMINER,
        "goldminer-sanitized-8MB.bin",
        RunLimits {
            until: Some(VTime(GOLDMINER_RUN_PS)),
            max_insns: None,
            stops: StopSet::default(),
        },
    ) else {
        return;
    };
    let tail = console_tail(console.as_bytes(), 8);
    assert_eq!(
        reason,
        StopReason::Until,
        "{id}: `goldminer` stopped at pc {:#010x} before 5 s; tail:\n{tail}",
        m.hart().pc
    );
    assert!(
        console
            .lines()
            .map(untimed)
            .any(|l| l == "main_task: Calling app_main()"),
        "{id}: `main_task: Calling app_main()` missing; tail:\n{tail}"
    );
    let mut classes = pemu_soc_c3::periph::Devices::default();
    // Every first touch the strict set covers: the denominator, printed below.
    let mut checked: Vec<(&str, u32)> = m
        .ledger()
        .first_touches()
        .iter()
        .filter(|t| !t.allowlisted)
        .map(|t| (row_name(t.periph), t.off))
        .filter(|(block, off)| {
            M3_STRICT_BLOCKS.contains(block) || M3_STRICT_REGISTERS.contains(&(*block, *off))
        })
        .filter(|(block, off)| {
            !GOLDMINER_ALLOWED_OFFSETS
                .iter()
                .any(|(b, o, _)| (b, o) == (block, off))
        })
        .collect();
    checked.sort_unstable();
    checked.dedup();
    let mut unmodeled: Vec<(&str, u32)> = m
        .ledger()
        .first_touches()
        .iter()
        .filter(|t| !t.allowlisted && !model_class(&mut classes, t.periph, t.off).is_claimed())
        .map(|t| (row_name(t.periph), t.off))
        .filter(|(block, off)| {
            M3_STRICT_BLOCKS.contains(block) || M3_STRICT_REGISTERS.contains(&(*block, *off))
        })
        // Offsets that are not registers ([`GOLDMINER_ALLOWED_OFFSETS`]).
        .filter(|(block, off)| {
            !GOLDMINER_ALLOWED_OFFSETS
                .iter()
                .any(|(b, o, _)| (b, o) == (block, off))
        })
        .collect();
    unmodeled.sort_unstable();
    unmodeled.dedup();
    if unmodeled.is_empty() {
        println!(
            "{id} `goldminer` reaches app_main and runs 5 s with no unmodeled first touch inside \
             the M3 strict set: {} register(s) of the set were first-touched and every one of \
             them carries a class above U, plus {} allowlisted offset(s) that are not registers",
            checked.len(),
            GOLDMINER_ALLOWED_OFFSETS.len()
        );
        return;
    }
    let mut per_block: Vec<(&str, usize)> = Vec::new();
    for (block, _) in &unmodeled {
        match per_block.iter_mut().find(|(b, _)| b == block) {
            Some((_, n)) => *n += 1,
            None => per_block.push((block, 1)),
        }
    }
    let counts: Vec<String> = per_block.iter().map(|(b, n)| format!("{b} {n}")).collect();
    let unexpected: Vec<String> = unmodeled
        .iter()
        .filter(|row| {
            !GOLDMINER_STILL_U
                .iter()
                .any(|(b, off, _)| (*b, *off) == **row)
        })
        .map(|(block, off)| format!("{block} +{off:#05X}"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "{id}: {} first-touched register(s) of the strict set answer Fidelity::U and are not in \
         GOLDMINER_STILL_U: {}. Either the class row of specs/blocks/<block>.toml is missing, or one \
         that existed was removed; per block, the whole unclassed set is {}",
        unexpected.len(),
        unexpected.join(", "),
        counts.join(", ")
    );
    let left: Vec<String> = GOLDMINER_STILL_U
        .iter()
        .filter(|(b, off, _)| unmodeled.contains(&(*b, *off)))
        .map(|(b, off, why)| format!("{b} +{off:#05X} ({why})"))
        .collect();
    common::skip(
        test,
        &format!(
            "{id} the 5 s run passes and every first-touched register of the strict set carries a \
             class except {}: {}",
            left.len(),
            left.join("; ")
        ),
    );
}

// ---------------------------------------------------------------------------------------------
// The M3 probes
// ---------------------------------------------------------------------------------------------

/// The merged image of probe `name` and its stripped ELF, or `None` after the skip line.
///
/// Found in `corpus/probes/` beside the verified `pk` corpus id and checked against its pin in
/// `tests/fw/manifest.toml`; a present image that is not the pinned build fails.
fn probe_image(test: &str, name: &str) -> Option<(Vec<u8>, ElfInfo)> {
    let pk = common::corpus_file_or_skip(test, common::PK, PK_IMAGE)?;
    let path = pk
        .ancestors()
        .nth(2)
        .expect("a corpus file sits under corpus/<id>/")
        .join(format!("probes/{name}-8MB.bin"));
    let Ok(bytes) = std::fs::read(&path) else {
        common::skip(
            test,
            &format!("corpus/probes/{name}-8MB.bin is not built (xtask probes)"),
        );
        return None;
    };
    let fw = workspace().join("tests/fw");
    let manifest = std::fs::read_to_string(fw.join("manifest.toml")).expect("the probe manifest");
    let pinned = manifest
        .split("[[probe]]")
        .find(|block| block.contains(&format!("name = \"{name}\"")))
        .and_then(|block| {
            block
                .lines()
                .find_map(|l| l.strip_prefix("merged_sha256 = \""))
                .map(|v| v.trim_end_matches('"').to_string())
        })
        .unwrap_or_else(|| panic!("tests/fw/manifest.toml pins no merged image for `{name}`"));
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&bytes),
        pinned,
        "{test}: corpus/probes/{name}-8MB.bin is not the pinned build"
    );
    let elf = ElfInfo::parse(
        &std::fs::read(fw.join(format!("{name}.elf"))).expect("the committed probe ELF"),
    )
    .expect("the probe ELF parses");
    Some((bytes, elf))
}

fn probe_machine(image: &[u8], elf: ElfInfo, cfg: MachineConfig) -> Machine {
    let flash = FlashImage::from_merged(image).expect("a probe image parses");
    let assets = Assets::with_bundled_rom(flash, Some(Arc::new(elf)), None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    Machine::new(cfg, assets).expect("the image fits")
}

/// Virtual time between two console reads in [`run_collecting`], short enough that the ring never
/// wraps between reads.
const COLLECT_SLICE_PS: u64 = 50_000_000_000;

/// Runs `m` until `until` or a stop of `stops`, reading the device console as it goes so no byte
/// is lost to the `HostIo` ring. Returns the stop and the console bytes.
fn run_collecting(m: &mut Machine, until: VTime, stops: &StopSet) -> (StopReason, Vec<u8>) {
    let mut cursor = m.io().serial_ring(SerialStream::UsjTx).head();
    let mut console = Vec::new();
    loop {
        let next = VTime((m.now().0 + COLLECT_SLICE_PS).min(until.0));
        let out = m.run(RunLimits {
            until: Some(next),
            max_insns: None,
            stops: stops.clone(),
        });
        let ring = m.io().serial_ring(SerialStream::UsjTx);
        assert!(
            ring.tail() <= cursor,
            "the console ring wrapped between two reads; shorten COLLECT_SLICE_PS"
        );
        console.extend(ring.slices(cursor).iter().copied());
        cursor = ring.head();
        if out.reason != StopReason::Until || next >= until {
            return (out.reason, console);
        }
    }
}

const PROBE_RESET_RESTARTS: usize = 20;

/// Virtual time `probe_reset` gets; it prints `DONE` at about 5 s.
const PROBE_RESET_RUN_MS: u64 = 20_000;

const PROBE_RESET_DONE: &str = "DONE|name=probe_reset|status=ok";

/// The reset causes of the banners after the power-on one and the twenty restarts: deep-sleep wake
/// (0x5), task watchdog abort (0xc), the interrupt-watchdog stage (0xc) and the RTC watchdog (0x9).
const PROBE_RESET_AFTER_RESTARTS: [&str; 4] = [
    "0x5 (DSLEEP)",
    "0xc (RTC_SW_CPU_RST)",
    "0xc (RTC_SW_CPU_RST)",
    "0x9 (RTCWDT_SYS_RST)",
];

/// The probe's summary lines as silicon prints them
/// (`device-probe_reset-20260917T162425Z.notes.md` below the data root's `captures/`).
const PROBE_RESET_SUMMARY: [&str; 5] = [
    "SUMMARY|restarts=20|restart_reasons_ok=20|target=20",
    "SUMMARY|stage=after_deep_sleep|reason=8",
    "SUMMARY|stage=after_twdt|reason=6",
    "SUMMARY|stage=after_iwdt|reason=0",
    "SUMMARY|stage=after_rwdt|reason=7",
];

/// `probe_reset` calls `esp_restart` 20 times, each following banner shows `rst:0xc`, and no
/// `Memprot feature locked` line appears.
///
/// The rest of the sequence (deep sleep and timer wake, task watchdog, interrupt-watchdog stage,
/// RTC watchdog, `DONE`) is checked against the device capture: 22 `rst:0xc` and one `rst:0x9`,
/// and the summary lines, including `after_iwdt reason=0` with `FAIL|what=iwdt` (the interrupt
/// watchdog does not reset the chip during the spin on silicon either). The device console loses
/// the `rst:0x5` banner because USJ drops across deep sleep; the emulator's keeps it.
#[test]
fn t1_m3_probe_reset_restarts_20_times() {
    let test = "t1_m3_probe_reset_restarts_20_times";
    let id = test.to_string();
    let Some((image, elf)) = probe_image(test, "probe_reset") else {
        return;
    };
    let mut m = probe_machine(&image, elf, MachineConfig::default());
    let (reason, console) = run_collecting(
        &mut m,
        VTime::from_ms(PROBE_RESET_RUN_MS),
        &line_stop(LinePattern::Exact(PROBE_RESET_DONE.into())),
    );
    let console = String::from_utf8_lossy(&console).into_owned();
    let tail = console_tail(console.as_bytes(), 8);
    assert_eq!(
        reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: the probe never printed `{PROBE_RESET_DONE}`; tail:\n{tail}"
    );
    let lines: Vec<&str> = console.lines().map(|l| l.trim_end_matches('\r')).collect();
    let causes: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("rst:"))
        .map(|l| l.split(',').next().unwrap_or(l))
        .collect();
    assert_eq!(
        causes.len(),
        1 + PROBE_RESET_RESTARTS + PROBE_RESET_AFTER_RESTARTS.len(),
        "{id}: the power-on banner, one per restart and one per later stage: {causes:?}"
    );
    assert_eq!(causes[0], "0x1 (POWERON)", "{id}");
    assert!(
        causes[1..=PROBE_RESET_RESTARTS]
            .iter()
            .all(|c| *c == "0xc (RTC_SW_CPU_RST)"),
        "{id}: every banner after a restart shows rst:0xc: {causes:?}"
    );
    assert_eq!(
        causes[1 + PROBE_RESET_RESTARTS..],
        PROBE_RESET_AFTER_RESTARTS,
        "{id}: the banners after the restarts"
    );
    assert_eq!(
        causes.iter().filter(|c| c.starts_with("0xc")).count(),
        22,
        "{id}"
    );
    assert_eq!(
        causes.iter().filter(|c| c.starts_with("0x9")).count(),
        1,
        "{id}"
    );
    for index in 1..=PROBE_RESET_RESTARTS {
        let want = format!("RESTART|index={index}|reason=3|raw=0x0c");
        assert!(
            lines.contains(&want.as_str()),
            "{id}: `{want}` missing; tail:\n{tail}"
        );
    }
    let summary: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.starts_with("SUMMARY|"))
        .collect();
    assert_eq!(
        summary, PROBE_RESET_SUMMARY,
        "{id}: the probe's summary against silicon"
    );
    assert!(
        lines
            .contains(&"FAIL|what=iwdt|detail=spinning with interrupts off did not reset the chip"),
        "{id}: the interrupt-watchdog stage's FAIL line, which silicon prints too, is missing"
    );
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("FAIL|")).count(),
        1,
        "{id}: a FAIL line other than the iwdt one; tail:\n{tail}"
    );
    assert!(
        !console.contains("Memprot feature locked"),
        "{id}: a `Memprot feature locked` line appeared"
    );
}

/// One `CLK` line of `probe_clocks`: the phase and the deltas of its four time sources.
#[derive(Debug)]
struct ClkLine {
    phase: String,
    timer_us: i64,
    ticks: i64,
    cycles: i64,
    rtc_us: i64,
}

impl ClkLine {
    fn parse(line: &str) -> Option<ClkLine> {
        let mut fields = line.strip_prefix("CLK|")?.split('|');
        let mut next = |key: &str| -> Option<String> {
            fields
                .next()?
                .strip_prefix(key)?
                .strip_prefix('=')
                .map(str::to_string)
        };
        let phase = next("phase")?;
        next("nominal_us")?;
        let timer_us = next("timer_us")?.parse().ok()?;
        let ticks = next("ticks")?.parse().ok()?;
        let cycles = next("cycles")?.parse().ok()?;
        let rtc_us = next("rtc_us")?.parse().ok()?;
        Some(ClkLine {
            phase,
            timer_us,
            ticks,
            cycles,
            rtc_us,
        })
    }
}

/// One FreeRTOS tick of `probe_clocks` in microseconds, the tolerance of every clock rule.
const CLOCKS_TICK_US: i64 = 1_000;

const CLOCKS_CPU_MHZ: i64 = 160;

/// The phases before the light sleep, each busy (the cycle count agrees with the wall clocks) or
/// WFI (it advances by `cpu_mhz x (dvt - idle - uncounted stall)`).
const CLOCKS_PHASES: [(&str, bool); 4] = [
    ("busy_loop", true),
    ("rom_delay_us", true),
    ("task_delay", false),
    ("timer_poll", false),
];

const CLOCKS_LIGHT_SLEEP_US: i64 = 20_000;

const CLOCKS_AFTER_SLEEP: [&str; 2] = [
    "SLEEP|phase=light_sleep|err=0|cause=4",
    "DONE|name=probe_clocks|status=ok",
];

/// The `idle` of one `probe_clocks` phase: its window's idle plus the carry, capped at the phase's
/// own duration, with the rest carried on.
///
/// With console pacing a committed USJ IN packet waits for a host poll and light sleep postpones
/// it, so under `device` the `timer_poll` window holds the 20 ms sleep's idle. The shift only moves
/// idle earlier, so the cap gives each phase its own idle back; under `fast` the carry is 0.
fn take_idle(carry: &mut i64, out: &pemu_machine::run::RunOutcome, clk: &ClkLine) -> i64 {
    let available = (out.idle_ps / 1_000_000) as i64 + *carry;
    let mine = available.min(clk.timer_us);
    *carry = available - mine;
    mine
}

/// [`take_idle`] for a phase that never waits: all its window's idle goes to the carry.
fn defer_idle(carry: &mut i64, out: &pemu_machine::run::RunOutcome) {
    *carry += (out.idle_ps / 1_000_000) as i64;
}

#[track_caller]
fn within_tick(id: &str, profile: &str, phase: &str, pair: &str, a: i64, b: i64) {
    assert!(
        (a - b).abs() <= CLOCKS_TICK_US,
        "{id} {profile} {phase}: {pair} are {} us apart, over one tick",
        (a - b).abs()
    );
}

/// `probe_clocks` under `fast` and `device`: esp_timer, the tick count and RTC time agree within
/// one tick across the busy loop, the ROM delay, `vTaskDelay` and the esp_timer poll; the cycle
/// count agrees within one tick across the busy phases and across the WFI phases advances by
/// `cpu_mhz x (dvt - idle - uncounted stall)`.
///
/// The uncounted stall is 0 under both profiles: `fast` charges no fill, and `device` counts the
/// fill stalls (`cache_stall_counts_cycles` 1). Across the light sleep the tick count stays
/// constant, esp_timer and RTC time advance by 20 ms, and the probe prints its timer wake and
/// `DONE`.
#[test]
fn t1_m3_probe_clocks_one_clock() {
    let test = "t1_m3_probe_clocks_one_clock";
    let id = test.to_string();
    let Some((image, elf)) = probe_image(test, "probe_clocks") else {
        return;
    };
    for profile in [TimingProfileId::Fast, TimingProfileId::Device] {
        let label = format!("{profile:?}").to_lowercase();
        let cfg = MachineConfig {
            profile,
            ..MachineConfig::default()
        };
        let mut m = probe_machine(&image, elf.clone(), cfg);
        let stops = StopSet {
            matchers: vec![
                (
                    MatcherId(0x36),
                    Matcher::Serial {
                        stream: SerialStream::UsjTx,
                        pattern: LinePattern::Prefix("CLK|".into()),
                    },
                ),
                (
                    MatcherId(0x37),
                    Matcher::Serial {
                        stream: SerialStream::UsjTx,
                        pattern: LinePattern::Prefix("FAIL|".into()),
                    },
                ),
            ],
            ..StopSet::default()
        };
        let mut from = 0;
        let mut idle_carry = 0i64;
        for (phase, busy) in CLOCKS_PHASES {
            let out = m.run(RunLimits {
                until: Some(VTime::from_ms(3_000)),
                max_insns: None,
                stops: stops.clone(),
            });
            let console = console_from(&mut m, from);
            from = m.io().serial_ring(SerialStream::UsjTx).head();
            let text = String::from_utf8_lossy(&console).into_owned();
            assert_eq!(
                out.reason,
                StopReason::Matcher(MatcherId(0x36)),
                "{id} {label}: no `CLK` line for {phase} (a `FAIL` line stops too); tail:\n{}",
                console_tail(&console, 8)
            );
            let clk = text
                .lines()
                .rev()
                .find_map(|l| ClkLine::parse(l.trim_end_matches('\r')))
                .expect("the stop was on a CLK line");
            assert_eq!(clk.phase, phase, "{id} {label}: phases in probe order");
            let tick_us = clk.ticks * CLOCKS_TICK_US;
            within_tick(
                &id,
                &label,
                phase,
                "esp_timer and rtc",
                clk.timer_us,
                clk.rtc_us,
            );
            within_tick(
                &id,
                &label,
                phase,
                "esp_timer and tick",
                clk.timer_us,
                tick_us,
            );
            within_tick(&id, &label, phase, "tick and rtc", tick_us, clk.rtc_us);
            let cycles_us = clk.cycles / CLOCKS_CPU_MHZ;
            if busy {
                defer_idle(&mut idle_carry, &out);
                within_tick(
                    &id,
                    &label,
                    phase,
                    "esp_timer and cycles",
                    clk.timer_us,
                    cycles_us,
                );
            } else {
                let idle_us = take_idle(&mut idle_carry, &out, &clk);
                assert!(
                    idle_us > 0,
                    "{id} {label} {phase}: a WFI phase that never idled"
                );
                within_tick(
                    &id,
                    &label,
                    phase,
                    "cycles and cpu_mhz x (dvt - idle)",
                    cycles_us,
                    clk.timer_us - idle_us,
                );
            }
        }
        // The FreeRTOS tick count does not advance across a light sleep: the device capture
        // `device-sleep_lat-20260917T141559Z.notes.md` (second run) shows 0 ticks across each of five
        // 500 ms light sleeps with no tickless idle, while esp_timer and RTC time advance.
        let out = m.run(RunLimits {
            until: Some(VTime::from_ms(3_000)),
            max_insns: None,
            stops: stops.clone(),
        });
        let console = console_from(&mut m, from);
        from = m.io().serial_ring(SerialStream::UsjTx).head();
        assert_eq!(
            out.reason,
            StopReason::Matcher(MatcherId(0x36)),
            "{id} {label}: no `CLK` line for light_sleep; tail:\n{}",
            console_tail(&console, 8)
        );
        let clk = String::from_utf8_lossy(&console)
            .lines()
            .rev()
            .find_map(|l| ClkLine::parse(l.trim_end_matches('\r')))
            .expect("the stop was on a CLK line");
        let phase = "light_sleep";
        assert_eq!(clk.phase, phase, "{id} {label}: phases in probe order");
        assert_eq!(
            clk.ticks, 0,
            "{id} {label} light_sleep: the tick count advanced across the light sleep, which it \
             does not on silicon"
        );
        within_tick(
            &id,
            &label,
            phase,
            "esp_timer and rtc",
            clk.timer_us,
            clk.rtc_us,
        );
        for (source, us) in [("esp_timer", clk.timer_us), ("rtc", clk.rtc_us)] {
            assert!(
                (CLOCKS_LIGHT_SLEEP_US..=CLOCKS_LIGHT_SLEEP_US + CLOCKS_TICK_US).contains(&us),
                "{id} {label} light_sleep: {source} advanced {us} us across a {CLOCKS_LIGHT_SLEEP_US} \
                 us sleep, not the sleep within one tick"
            );
        }
        let idle_us = take_idle(&mut idle_carry, &out, &clk);
        within_tick(
            &id,
            &label,
            phase,
            "cycles and cpu_mhz x (dvt - idle)",
            clk.cycles / CLOCKS_CPU_MHZ,
            clk.timer_us - idle_us,
        );
        // Idle left in the carry would be idle no phase claimed.
        assert!(
            idle_carry < CLOCKS_TICK_US,
            "{id} {label}: {idle_carry} us of idle belongs to no phase, over one tick"
        );
        let out = m.run(RunLimits {
            until: Some(VTime::from_ms(3_000)),
            max_insns: None,
            stops: line_stop(LinePattern::Exact(CLOCKS_AFTER_SLEEP[1].into())),
        });
        let console = console_from(&mut m, from);
        assert_eq!(
            out.reason,
            StopReason::Matcher(BOOT_LINE),
            "{id} {label}: no `DONE` after the light sleep; tail:\n{}",
            console_tail(&console, 8)
        );
        let text = String::from_utf8_lossy(&console).into_owned();
        let lines: Vec<&str> = text
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter(|l| l.starts_with("SLEEP|") || l.starts_with("DONE|") || l.starts_with("FAIL|"))
            .collect();
        assert_eq!(
            lines, CLOCKS_AFTER_SLEEP,
            "{id} {label}: the lines after the light sleep"
        );
        println!(
            "RAN {test} {label}: busy_loop rom_delay_us task_delay timer_poll light_sleep \
             (ticks 0, esp_timer {} us, rtc {} us)",
            clk.timer_us, clk.rtc_us
        );
    }
    println!(
        "RAN {test} device-profile: `device` selects the device column of \
         specs/timing-profiles.toml, whose pacing, CPI and RTC slow clock this probe \
         exercises"
    );
}

/// The lines `probe_intc` prints on silicon before its EDGE round, without the `LAT` samples:
/// `device-probe_intc-20260917T120257Z.log` below the data root (no identity in them).
const INTC_SILICON_LINES: [&str; 8] = [
    "PROBE|name=probe_intc|schema=passport-emu/probe-line/1",
    "INTC|from_cpu_2|source=52|line=7|map=7|pri=1|type=0|enable=1|thresh=1",
    "INTC|from_cpu_3|source=53|line=8|map=8|pri=1|type=0|enable=1|thresh=1",
    "THRESH|pri=1|blocked_at=2|restored_to=1|ran_while_blocked=0|ran_after_restore=1",
    "INTC|from_cpu_2_before|source=52|line=7|map=7|pri=1|type=0|enable=1|thresh=1",
    "INTC|from_cpu_2_rerouted|source=52|line=8|map=8|pri=1|type=0|enable=1|thresh=1",
    "MAP|from_line=7|to_line=8|map=8|delivered=1|isr_a=0|isr_b=1",
    "INTC|from_cpu_2_restored|source=52|line=7|map=7|pri=1|type=0|enable=1|thresh=1",
];

/// Virtual time after the MAP round: the EDGE round needs 8 triggers a few milliseconds apart when
/// the latch clears, so 300 ms without an EDGE line is a storm.
const INTC_EDGE_WINDOW_PS: u64 = 300_000_000_000;

/// Past the first task watchdog report, which silicon prints at 5071 ms and this build at about
/// 5030 ms.
const INTC_WDT_RUN_MS: u64 = 5_500;

const INTC_IRQ_RECORDS: usize = 4_096;

/// `probe_intc` to the end of its MAP round with the IRQ trace on, its lines checked against the
/// silicon capture on the way, or `None` after the skip line.
fn intc_to_edge_round(test: &str) -> Option<Machine> {
    let (image, elf) = probe_image(test, "probe_intc")?;
    let cfg = MachineConfig {
        trace: pemu_machine::config::TraceCfg {
            kinds: Some(pemu_core::trace::TraceKinds::IRQ),
            recent: INTC_IRQ_RECORDS,
        },
        ..MachineConfig::default()
    };
    let mut m = probe_machine(&image, elf, cfg);
    let restored = INTC_SILICON_LINES[INTC_SILICON_LINES.len() - 1];
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(3_000)),
        max_insns: None,
        stops: line_stop(LinePattern::Exact(restored.into())),
    });
    let console = console_from(&mut m, 0);
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{test}: the MAP round did not finish; tail:\n{}",
        console_tail(&console, 12)
    );
    let text = String::from_utf8_lossy(&console).into_owned();
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let probe: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.contains('|') && !l.starts_with("LAT|") && !l.starts_with("LATSUM|"))
        .collect();
    assert_eq!(
        probe, INTC_SILICON_LINES,
        "{test}: the probe lines before the EDGE round"
    );
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("LAT|index=")).count(),
        16,
        "{test}: sixteen latency samples"
    );
    Some(m)
}

/// `probe_intc` storms in its EDGE round as silicon does, not like the QEMU oracle's `isr=8`
/// (`console.probe-intc-edge-round` in `specs/oracle-known-diffs.toml`).
///
/// IDF's `rv_utils_intr_edge_ack` writes the line number 7 into `CPU_INT_CLEAR` rather than bit 7,
/// so line 7's edge latch never clears: no `EDGE` line comes in 300 ms, and at least nine in ten
/// IRQ records are line 7 again. The `LAT` samples are only counted here; the `device` comparison
/// is `t1_cycle_counts_against_the_device_captures` in `m11.rs`.
#[test]
fn t1_probe_intc_edge_round_storms_like_silicon() {
    let test = "t1_probe_intc_edge_round_storms_like_silicon";
    let Some(mut m) = intc_to_edge_round(test) else {
        return;
    };
    let from = m.io().serial_ring(SerialStream::UsjTx).head();
    let until = VTime(m.now().0 + INTC_EDGE_WINDOW_PS);
    let out = m.run(RunLimits {
        until: Some(until),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{test}: pc {:#010x}",
        m.hart().pc
    );
    let after = String::from_utf8_lossy(&console_from(&mut m, from)).into_owned();
    assert!(
        !after.contains("EDGE|"),
        "{test}: the EDGE round completed, so the edge latch cleared:\n{after}"
    );
    let irq: Vec<pemu_core::trace::IrqEvent> = m
        .trace()
        .records()
        .filter_map(|r| match r.ev {
            pemu_core::trace::TraceEvent::Irq(ev) => Some(ev),
            _ => None,
        })
        .collect();
    assert_eq!(
        irq.len(),
        INTC_IRQ_RECORDS,
        "{test}: the replay window is full"
    );
    let mut per_line = [0usize; 32];
    for ev in &irq {
        if let pemu_core::trace::IrqEvent::Take { line, .. } = ev {
            per_line[usize::from(*line) & 31] += 1;
        }
    }
    let span_insns = {
        let recs: Vec<u64> = m.trace().records().map(|r| r.insns).collect();
        recs.last().copied().unwrap_or(0) - recs.first().copied().unwrap_or(0)
    };
    assert!(
        per_line[7] * 10 >= INTC_IRQ_RECORDS * 9,
        "{test}: line 7 is not storming: takes per line {per_line:?} over {span_insns} instructions"
    );
}

/// The task-watchdog half (nightly, the storm has to last 5 s): silicon's task watchdog reports
/// IDLE starved with `main` running at 5071 ms, and so must this build.
#[test]
fn t2_probe_intc_task_watchdog_reports_idle_starved() {
    let test = "t2_probe_intc_task_watchdog_reports_idle_starved";
    let Some(mut m) = intc_to_edge_round(test) else {
        return;
    };
    let from = m.io().serial_ring(SerialStream::UsjTx).head();
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(INTC_WDT_RUN_MS)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{test}: pc {:#010x}",
        m.hart().pc
    );
    let after = console_from(&mut m, from);
    let text = String::from_utf8_lossy(&after).into_owned();
    let lines: Vec<&str> = text.lines().map(untimed).collect();
    assert!(!text.contains("EDGE|"), "{test}: the EDGE round completed");
    for want in [
        "task_wdt: Task watchdog got triggered. The following tasks/users did not reset the \
         watchdog in time:",
        "task_wdt:  - IDLE (CPU 0)",
        "task_wdt: CPU 0: main",
    ] {
        assert!(
            lines.contains(&want),
            "{test}: `{want}` missing; tail:\n{}",
            console_tail(&after, 12)
        );
    }
}

/// The trap vector base of the IDF app: `MTVEC` reads `0x40380001` in the `probe_panic` dump
/// (vectored mode); an exception enters at the base, interrupt `n` at `base + 4n`.
const APP_TRAP_BASE: u32 = 0x4038_0000;

/// `mcause` of a load access fault (RISC-V privileged specification, exception code 5).
const MCAUSE_LOAD_ACCESS_FAULT: u32 = 5;

/// `limits-bus`: an access to an unmapped address faults with the silicon `mcause` and `mtval`.
///
/// The access is `probe_panic`'s NULL read: address 0 is unmapped (TRM System and Memory chapter),
/// and IDF's locked PMP entries make it a load access fault, `mcause` 5 with `mtval` the address
/// (RISC-V privileged specification). The run breaks at the trap vector base, which only an
/// exception enters, and IDF's panic text must print the same values.
///
/// **Evidence class: specification, not silicon.** The values come from the TRM memory map
/// and the RISC-V privileged specification; no device capture of `probe_panic` exists.
#[test]
fn t1_m3_limits_bus_unmapped_read_faults() {
    let test = "t1_m3_limits_bus_unmapped_read_faults";
    let id = test.to_string();
    let Some((image, elf)) = probe_image(test, "probe_panic") else {
        return;
    };
    let mut m = probe_machine(&image, elf, MachineConfig::default());
    let armed = "ARMED|task=panic_task|addr=0x00000000";
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(3_000)),
        max_insns: None,
        stops: line_stop(LinePattern::Exact(armed.into())),
    });
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: `{armed}` never printed; tail:\n{}",
        console_tail(&console_from(&mut m, 0), 8)
    );
    let from = m.io().serial_ring(SerialStream::UsjTx).head();
    let out = m.run(RunLimits {
        until: Some(VTime(m.now().0 + VTime::from_ms(500).0)),
        max_insns: None,
        stops: StopSet {
            breakpoints: vec![APP_TRAP_BASE],
            ..StopSet::default()
        },
    });
    assert_eq!(
        out.reason,
        StopReason::Breakpoint(APP_TRAP_BASE),
        "{id}: the NULL read did not trap"
    );
    let csr = &m.hart().csr;
    assert_eq!(csr.mcause, MCAUSE_LOAD_ACCESS_FAULT, "{id}: mcause");
    assert_eq!(csr.mtval, 0, "{id}: mtval is the unmapped data address");
    let mepc = csr.mepc;
    assert!(
        (0x4200_0000..0x4280_0000).contains(&mepc),
        "{id}: mepc {mepc:#010x} is not in the app's flash text"
    );

    // The `esp_panic_handler` observe hook stops the run first; IDF's report follows on resume.
    let until = VTime(m.now().0 + VTime::from_ms(500).0);
    let dump_stop = || RunLimits {
        until: Some(until),
        max_insns: None,
        stops: line_stop(LinePattern::Prefix("MHARTID".into())),
    };
    let mut out = m.run(dump_stop());
    let mut panics = 0;
    while matches!(out.reason, StopReason::GuestPanic(_)) && panics < 1 {
        panics += 1;
        out = m.run(dump_stop());
    }
    let dump = String::from_utf8_lossy(&console_from(&mut m, from)).into_owned();
    assert_eq!(
        out.reason,
        StopReason::Matcher(BOOT_LINE),
        "{id}: no register dump:\n{dump}"
    );
    assert!(
        dump.contains(
            "Guru Meditation Error: Core  0 panic'ed (Load access fault). Exception was unhandled."
        ),
        "{id}: IDF did not report a load access fault:\n{dump}"
    );
    assert!(
        dump.contains(&format!("MEPC    : {mepc:#010x}")),
        "{id}: the dump's MEPC differs from the trap's:\n{dump}"
    );
    assert!(
        dump.contains("MCAUSE  : 0x00000005  MTVAL   : 0x00000000"),
        "{id}: the dump's MCAUSE and MTVAL:\n{dump}"
    );
}

// ---------------------------------------------------------------------------------------------
// No spurious stack guard
// ---------------------------------------------------------------------------------------------

/// `ASSIST_DEBUG_CORE_0_INTR_ENA`, the stack-guard monitor enable (IDF
/// `soc/esp32c3/register/soc/assist_debug_reg.h`).
const ASSIST_DEBUG_INTR_ENA: u32 = 0x600C_E000;

/// Records of the MMIO-write and IRQ trace a run keeps between two reads.
const GUARD_TRACE_RECORDS: usize = 1 << 16;

/// Interrupt entries the runs must take together for the toggle check to mean something; most
/// come from `goldminer`'s 5 s.
const GUARD_MIN_TAKES: usize = 100;

fn guard_trace() -> pemu_machine::config::TraceCfg {
    pemu_machine::config::TraceCfg {
        kinds: Some(
            pemu_core::trace::TraceKinds::MMIO_WRITE.union(pemu_core::trace::TraceKinds::IRQ),
        ),
        recent: GUARD_TRACE_RECORDS,
    }
}

/// The interrupt-entry toggle check, fed record by record across a whole run.
#[derive(Default)]
struct GuardToggle {
    takes: usize,
    toggled: usize,
    /// `Some(false)`: an entry was taken and the monitor stop is due; `Some(true)`: the stop was
    /// seen and the start is due.
    expect: Option<bool>,
    broken: Option<String>,
}

impl GuardToggle {
    fn feed(&mut self, rec: &pemu_core::trace::TraceRecord) {
        match rec.ev {
            pemu_core::trace::TraceEvent::Irq(pemu_core::trace::IrqEvent::Take { .. }) => {
                self.takes += 1;
                // `rtos_int_enter` stops the monitor before it moves the bounds to the ISR stack.
                self.expect = Some(false);
            }
            pemu_core::trace::TraceEvent::MmioWrite { addr, val, .. }
                if addr == ASSIST_DEBUG_INTR_ENA =>
            {
                let bits = val & 0x300;
                match self.expect {
                    Some(false) if bits == 0 => self.expect = Some(true),
                    Some(true) if bits == 0x300 => {
                        self.toggled += 1;
                        self.expect = None;
                    }
                    Some(_) => {
                        self.broken.get_or_insert_with(|| {
                            format!("INTR_ENA {val:#x} at instruction {}", rec.insns)
                        });
                        self.expect = None;
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }
}

/// Runs `m` in slices short enough that the trace window never drops a record, feeding every
/// record to `toggle`.
fn run_with_guard_trace(
    m: &mut Machine,
    limits: RunLimits,
    toggle: &mut GuardToggle,
) -> StopReason {
    const SLICE: u64 = 20_000;
    let mut cursor = m.trace().head();
    let mut left = limits.max_insns;
    loop {
        let slice = left.map_or(SLICE, |l| l.min(SLICE));
        let out = m.run(RunLimits {
            until: limits.until,
            max_insns: Some(slice),
            stops: limits.stops.clone(),
        });
        let (tail, head) = (m.trace().tail(), m.trace().head());
        assert!(
            tail <= cursor,
            "the stack-guard trace window dropped records"
        );
        for rec in m.trace().records().skip((cursor - tail) as usize) {
            toggle.feed(&rec);
        }
        cursor = head;
        if let Some(l) = left.as_mut() {
            *l = l.saturating_sub(out.insns);
        }
        if out.reason != StopReason::MaxInsns || left == Some(0) {
            return out.reason;
        }
    }
}

/// Checks one run: no stack-guard violation, no stack protection panic, and the monitor switched
/// off and on after every interrupt entry.
///
/// The SP spill RAW bits (8 and 9) are latched only from a violation the engine reports, and this
/// build counts every such report in `Machine::unrecorded_spills` instead, so a count of 0 means
/// neither bit was ever set.
fn guard_check(id: &str, what: &str, m: &Machine, console: &str, toggle: &GuardToggle) {
    assert_eq!(
        m.unrecorded_spills(),
        0,
        "{id} {what}: a stack-guard violation"
    );
    assert!(
        !console.contains("Stack protection fault") && !console.contains("Guru Meditation"),
        "{id} {what}: a panic on the console"
    );
    assert_eq!(
        toggle.broken, None,
        "{id} {what}: the monitor toggle pattern broke"
    );
    assert!(toggle.takes > 0, "{id} {what}: no interrupt entry at all");
    assert!(
        toggle.toggled + 1 >= toggle.takes,
        "{id} {what}: the monitor toggled around {} of {} interrupt entries",
        toggle.toggled,
        toggle.takes
    );
}

/// Over the M3 boots and the block-size determinism variants, the ASSIST_DEBUG SP spill RAW bits
/// are never set and no stack protection panic occurs, while `INTR_ENA` toggles around interrupts
/// as `rtos_int_enter` and `rtos_int_exit` do.
///
/// The variants (block sizes 1 and 3, `ref_step`) change where an `sp` write ends a block. After
/// every interrupt entry the next `INTR_ENA` write clears bits 8 and 9 and the one after sets them
/// ([`GuardToggle`]).
#[test]
fn t1_m3_no_spurious_stack_guard() {
    let test = "t1_m3_no_spurious_stack_guard";
    let id = test.to_string();
    let traced = || MachineConfig {
        trace: guard_trace(),
        ..MachineConfig::default()
    };
    let machine = |image: &[u8], cfg: MachineConfig| {
        let flash = FlashImage::from_merged(image).expect("a corpus image parses");
        let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
            .expect("the bundled ROM is pinned");
        Machine::new(cfg, assets).expect("the image fits")
    };

    let Some((_, pk)) = pk_image_file(test) else {
        return;
    };
    // The reset boot of `pk` to `bsp_i2c`.
    let mut m = machine(&pk, traced());
    let mut toggle = GuardToggle::default();
    device_like_reset(test, &mut m);
    let reason = run_with_guard_trace(
        &mut m,
        RunLimits {
            until: None,
            max_insns: Some(BSP_I2C_INSNS),
            stops: bsp_i2c_stops(),
        },
        &mut toggle,
    );
    assert_eq!(reason, StopReason::Matcher(BSP_I2C_MATCHER), "{id} pk boot");
    let console = String::from_utf8_lossy(&console_from(&mut m, 0)).into_owned();
    guard_check(&id, "pk boot", &m, &console, &toggle);
    let mut total_takes = toggle.takes;

    // `pk` to `bsp_i2c` where the block boundaries move.
    for variant in [
        Variant {
            max_block_insns: 1,
            ..Variant::default()
        },
        Variant {
            max_block_insns: 3,
            ..Variant::default()
        },
        Variant {
            executor: Executor::Reference,
            ..Variant::default()
        },
    ] {
        let mut cfg = variant.config(MachineConfig::default());
        cfg.trace = guard_trace();
        let mut m = machine(&pk, cfg);
        variant.apply(&mut m);
        let mut toggle = GuardToggle::default();
        let reason = run_with_guard_trace(
            &mut m,
            RunLimits {
                until: None,
                max_insns: Some(BSP_I2C_INSNS),
                stops: bsp_i2c_stops(),
            },
            &mut toggle,
        );
        let what = format!("determinism {}", variant.label());
        assert_eq!(reason, StopReason::Matcher(BSP_I2C_MATCHER), "{id} {what}");
        let console = String::from_utf8_lossy(&console_from(&mut m, 0)).into_owned();
        guard_check(&id, &what, &m, &console, &toggle);
        total_takes += toggle.takes;
    }

    // `official` and `goldminer`.
    for (image_id, file, limits) in [
        (
            common::OFFICIAL,
            "FoloToy-AI-Passport-8MB.bin",
            RunLimits {
                until: None,
                max_insns: Some(BSP_I2C_INSNS),
                stops: bsp_i2c_stops(),
            },
        ),
        (
            common::GOLDMINER,
            "goldminer-sanitized-8MB.bin",
            RunLimits {
                until: Some(VTime(GOLDMINER_RUN_PS)),
                max_insns: None,
                stops: StopSet::default(),
            },
        ),
    ] {
        let Some(path) = common::corpus_file_or_skip(test, image_id, file) else {
            return;
        };
        let image = std::fs::read(&path).expect("the verified corpus file is readable");
        let mut m = machine(&image, traced());
        let mut toggle = GuardToggle::default();
        let reason = run_with_guard_trace(&mut m, limits, &mut toggle);
        assert!(
            matches!(reason, StopReason::Matcher(_) | StopReason::Until),
            "{id} {image_id}: {reason:?}"
        );
        let console = String::from_utf8_lossy(&console_from(&mut m, 0)).into_owned();
        guard_check(&id, image_id, &m, &console, &toggle);
        total_takes += toggle.takes;
    }
    assert!(
        total_takes >= GUARD_MIN_TAKES,
        "{id}: only {total_takes} interrupt entries over the boots"
    );
}

// ---------------------------------------------------------------------------------------------
// The USJ console of `pk`
// ---------------------------------------------------------------------------------------------

/// Virtual time the console is kept going after the scheduler starts.
const USJ_SOF_WINDOW_PS: u64 = 10_000_000_000_000;

/// `pk` with its app ELF, bound for HLE, and the ELF's symbol table.
fn pk_with_elf(test: &str) -> Option<(Machine, Arc<ElfInfo>)> {
    let image = common::corpus_file_or_skip(test, common::PK, PK_IMAGE)?;
    let elf_path = common::corpus_file_or_skip(test, common::PK, PK_APP_ELF)?;
    let bytes = std::fs::read(&image).expect("the verified corpus file is readable");
    let elf = Arc::new(
        ElfInfo::parse(&std::fs::read(&elf_path).expect("the verified ELF is readable"))
            .expect("the pinned ELF parses"),
    );
    let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses");
    let assets = Assets::with_bundled_rom(flash, Some(elf.clone()), None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    Some((
        Machine::new(MachineConfig::default(), assets).expect("the image fits"),
        elf,
    ))
}

fn pk_symbol(elf: &ElfInfo, name: &str) -> u32 {
    elf.symbols
        .addr_of(name)
        .unwrap_or_else(|| panic!("the pk app ELF defines `{name}`"))
}

fn run_to_symbol(id: &str, m: &mut Machine, elf: &ElfInfo, name: &str) {
    let pc = pk_symbol(elf, name);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(2_000)),
        max_insns: None,
        stops: StopSet {
            breakpoints: vec![pc],
            ..StopSet::default()
        },
    });
    assert_eq!(
        out.reason,
        StopReason::Breakpoint(pc),
        "{id}: `pk` never reached `{name}`"
    );
}

/// The `pk` console continues for 10 s virtual after the scheduler starts with a client open (U3),
/// and the IDF connection monitor never reports a disconnect.
///
/// The monitor writes `s_usb_serial_jtag_conn_status` only when its verdict changes
/// (`usb_serial_jtag_connection_monitor.c`, ESP-IDF v5.5.3), so a write watch on that byte from
/// `xPortStartScheduler` on is the disconnect check. Any stop before 10 s fails.
#[test]
fn t1_m3_usj_sof_keeps_the_pk_console() {
    let test = "t1_m3_usj_sof_keeps_the_pk_console";
    let id = test.to_string();
    let Some((mut m, elf)) = pk_with_elf(test) else {
        return;
    };
    run_to_symbol(&id, &mut m, &elf, "xPortStartScheduler");
    let started = m.now();
    let from = m.io().serial_ring(SerialStream::UsjTx).head();
    let status = pk_symbol(&elf, "s_usb_serial_jtag_conn_status");
    let out = m.run(RunLimits {
        until: Some(VTime(started.0 + USJ_SOF_WINDOW_PS)),
        max_insns: None,
        stops: StopSet {
            watches: vec![Watch {
                addr: status,
                len: 1,
            }],
            ..StopSet::default()
        },
    });
    let after = console_from(&mut m, from);
    assert!(
        !matches!(out.reason, StopReason::Watchpoint { .. }),
        "{id}: the connection monitor changed its verdict at {:?}; tail:\n{}",
        m.now(),
        console_tail(&after, 8)
    );
    assert_eq!(
        m.guest_mem().load(status, 1),
        Some(1),
        "{id}: the monitor does not hold `connected`"
    );
    let lines = String::from_utf8_lossy(&after).lines().count();
    assert!(
        lines > 0,
        "{id}: nothing printed after the scheduler started"
    );
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{id}: `pk` stopped {} ms after the scheduler started, before the 10 s window; tail:\n{}",
        (m.now().0 - started.0) / 1_000_000_000,
        console_tail(&after, 8)
    );
    println!("{id}: {lines} console lines over 10 s after the scheduler started, no disconnect");
}

/// Virtual time watched from the driver install: 3 s, which holds the boot-time burst (under
/// 200 ms in both states) and two seconds of idle after it.
const USJ_BURST_WINDOW_MS: u64 = 3_000;

/// USJ interrupt entries allowed in any virtual second of the U2 window. In U2 neither enabled
/// interrupt can pend more than once, and a storm is at least 1000 a second; 10 separates the two
/// and admits the U3-to-U2 entry.
const USJ_U2_ISR_PER_S: u64 = 10;

struct UsjBurst {
    written: u64,
    writes: u64,
    /// Bytes the host drained into the console after the install.
    drained: u64,
    /// Entries of `usb_serial_jtag_isr_handler_default`, per virtual second of the window.
    isr_per_s: Vec<u64>,
    console: Vec<u8>,
}

/// Boots `pk` with the device-like reset, stops at `usb_serial_jtag_driver_install`, puts the host
/// in U2 when `open` is false, and counts the driver's writes and interrupt entries.
fn usj_burst_run(test: &str, open: bool) -> Option<UsjBurst> {
    let (mut m, elf) = pk_with_elf(test)?;
    device_like_reset(test, &mut m);
    run_to_symbol(test, &mut m, &elf, "bsp_i2c_init");
    run_to_symbol(test, &mut m, &elf, "usb_serial_jtag_driver_install");
    if !open {
        m.input(
            pemu_machine::machine::At::Now,
            pemu_core::input::InputEvent::UsbClient { open: false },
        )
        .expect("now is not in the past");
    }
    let isr = pk_symbol(&elf, "usb_serial_jtag_isr_handler_default");
    let write = pk_symbol(&elf, "usb_serial_jtag_write_bytes");
    let head = m.io().serial_ring(SerialStream::UsjTx).head();
    let start = m.now();
    let second = VTime::from_ms(1_000).0;
    let mut burst = UsjBurst {
        written: 0,
        writes: 0,
        drained: 0,
        isr_per_s: vec![0; (USJ_BURST_WINDOW_MS / 1_000) as usize],
        console: Vec::new(),
    };
    loop {
        let out = m.run(RunLimits {
            until: Some(VTime(start.0 + VTime::from_ms(USJ_BURST_WINDOW_MS).0)),
            max_insns: None,
            stops: StopSet {
                breakpoints: vec![isr, write],
                ..StopSet::default()
            },
        });
        match out.reason {
            StopReason::Breakpoint(pc) => {
                if pc == isr {
                    let at = ((m.now().0 - start.0) / second) as usize;
                    let last = burst.isr_per_s.len() - 1;
                    burst.isr_per_s[at.min(last)] += 1;
                    let total: u64 = burst.isr_per_s.iter().sum();
                    assert!(
                        open || total <= USJ_U2_ISR_PER_S * USJ_BURST_WINDOW_MS / 1_000,
                        "{test}: U2 interrupt storm, {total} USJ interrupt entries by {} ms",
                        (m.now().0 - start.0) / VTime::from_ms(1).0
                    );
                } else {
                    // `usb_serial_jtag_write_bytes(const void *src, size_t size, TickType_t)`.
                    burst.writes += 1;
                    burst.written += u64::from(m.hart().x[11]);
                }
                m.run(RunLimits {
                    until: None,
                    max_insns: Some(1),
                    stops: StopSet::default(),
                });
            }
            StopReason::Until => break,
            other => panic!("{test}: `pk` stopped with {other:?} during the USJ burst window"),
        }
    }
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    burst.drained = ring.head() - head;
    burst.console = ring.slices(head).iter().copied().collect();
    Some(burst)
}

/// A driver-mode TX burst through the interrupt-driven USJ driver completes in U3, and in U2
/// blocks without an interrupt storm. The burst is every line `pk` writes after
/// `usb_serial_jtag_driver_install`, up to `pk_app: ready` and the BLE init lines.
/// - **U3**: drained bytes equal written bytes, the burst reaches `pk_app: ready`, and the
///   interrupt handler ran. `periph/usj.rs` drains synchronously, so the U3 latency is not tested.
/// - **U2**: the same writes happen, nothing drains, and the handler runs at most
///   [`USJ_U2_ISR_PER_S`] times in any virtual second.
#[test]
fn t1_m3_usj_driver_mode_tx_burst() {
    let test = "t1_m3_usj_driver_mode_tx_burst";
    let id = test.to_string();
    let Some(u3) = usj_burst_run(test, true) else {
        return;
    };
    let Some(u2) = usj_burst_run(test, false) else {
        return;
    };
    let u3_text = String::from_utf8_lossy(&u3.console).into_owned();
    assert!(
        u3.writes > 0 && u3.written > 0,
        "{id}: `pk` wrote nothing through the USJ driver"
    );
    assert_eq!(
        u3.drained,
        u3.written,
        "{id} U3: {} bytes written through the driver, {} drained; tail:\n{}",
        u3.written,
        u3.drained,
        console_tail(&u3.console, 8)
    );
    assert!(
        u3_text.contains("pk_app: ready"),
        "{id} U3: the burst does not reach `pk_app: ready`; tail:\n{}",
        console_tail(&u3.console, 8)
    );
    assert!(
        u3.isr_per_s.iter().sum::<u64>() > 0,
        "{id} U3: TX completed without the driver's interrupt handler"
    );

    assert_eq!(
        (u2.writes, u2.written),
        (u3.writes, u3.written),
        "{id} U2: the app's writes differ from U3's, so it is stuck behind the blocked endpoint"
    );
    assert_eq!(u2.drained, 0, "{id} U2: bytes drained with no client open");
    for (s, count) in u2.isr_per_s.iter().enumerate() {
        assert!(
            *count <= USJ_U2_ISR_PER_S,
            "{id} U2: {count} USJ interrupt entries in virtual second {s}, above {USJ_U2_ISR_PER_S}"
        );
    }
    println!(
        "{id}: U3 {} bytes in {} writes drained, USJ interrupts per second {:?}; U2 the same writes, \
         0 drained, USJ interrupts per second {:?}",
        u3.written, u3.writes, u3.isr_per_s, u2.isr_per_s
    );
}

// ---------------------------------------------------------------------------------------------
// The T2 QEMU write-stream diff over the app phase
// ---------------------------------------------------------------------------------------------

/// The blocks diffed, by their `specs/oracle-qemu-regions.toml` names.
const APP_PHASE_BLOCKS: [&str; 4] = ["systimer", "intc", "timg0", "timg1"];

/// The bootloader's last line before it jumps to the app, where the app phase starts.
const APP_PHASE_START: &str = "boot: Disabling RNG early entropy source";

/// Over `pk`'s app phase up to `bsp_i2c`, the per-block write-stream diff against QEMU for
/// SYSTIMER, INTMATRIX and TIMG is clean or explained in `specs/oracle-known-diffs.toml`.
#[test]
fn t2_m3_app_phase_write_streams_match_oracle() {
    let test = "t2_m3_app_phase_write_streams_match_oracle";
    let Some(trace_path) = oracle::oracle_file_or_skip(test, "pk.trace") else {
        return;
    };
    let Some(path) = common::corpus_file_or_skip(test, common::PK, PK_IMAGE) else {
        return;
    };
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses as a merged image");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    let cfg = MachineConfig {
        trace: oracle::write_trace(),
        ..MachineConfig::default()
    };
    let mut m = Machine::new(cfg, assets).expect("the image fits the 8 MB flash");
    let (stop, writes) = oracle::run_collecting_writes(
        &mut m,
        RunLimits {
            until: None,
            max_insns: Some(BSP_I2C_INSNS),
            stops: bsp_i2c_stops(),
        },
    );
    assert_eq!(
        stop,
        StopReason::Matcher(BSP_I2C_MATCHER),
        "`pk` reaches `bsp_i2c`"
    );

    let map = oracle::regions();
    let (ours, unmapped) = oracle::our_streams(&map, &writes, Some(APP_PHASE_START), "bsp_i2c:")
        .expect("our USJ write stream spells the app phase");
    let trace = std::fs::read_to_string(&trace_path).expect("the oracle trace is readable");
    let theirs = oracle::oracle_streams(&map, &trace, Some(APP_PHASE_START), "bsp_i2c:")
        .expect("the oracle USJ write stream spells the app phase");
    println!("`pk` app phase: {unmapped} of our writes outside every block");
    let failures = oracle::diff_blocks("`pk` app phase", &APP_PHASE_BLOCKS, &ours, &theirs);
    assert!(
        failures.is_empty(),
        "app phase: unexplained divergences:\n{failures}"
    );
}

// ---------------------------------------------------------------------------------------------
// Console TX pacing
// ---------------------------------------------------------------------------------------------

/// The three boot anchors the timing bands are checked on.
const PACING_ANCHORS: [&str; 3] = [
    "boot: ESP-IDF",
    "boot: Loaded app from partition",
    "cpu_start: Pro cpu start user code",
];

/// Virtual time a paced boot needs to print all three anchors.
const PACING_RUN_MS: u64 = 250;

/// `I (n)` milliseconds of [`PACING_ANCHORS`] in the device captures
/// (`device-probe_intc-20260917T120257Z.log` and `device-probe_clocks-20260917T120350Z.log`;
/// these three numbers carry no identity).
const PACING_DEVICE: [(&str, [u64; 3]); 2] =
    [("probe_intc", [24, 58, 67]), ("probe_clocks", [24, 60, 69])];

/// `|a - b| <= max(floor, share x b)` with the share as a percentage, in integers.
fn within_band(emu: u64, dev: u64, floor_ms: u64, share_pct: u64) -> bool {
    emu.abs_diff(dev) <= floor_ms.max(dev * share_pct / 100)
}

fn anchor_stamps(test: &str, text: &str) -> [u64; 3] {
    PACING_ANCHORS.map(|needle| {
        let line = text
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{test}: no `{needle}` line in:\n{text}"));
        line.trim_start_matches("I (")
            .split(')')
            .next()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or_else(|| panic!("{test}: `{line}` has no ESP_LOG timestamp"))
    })
}

fn pacing_console(test: &str, name: &str, profile: TimingProfileId) -> Option<String> {
    let (image, elf) = probe_image(test, name)?;
    let cfg = MachineConfig {
        profile,
        ..MachineConfig::default()
    };
    let mut m = probe_machine(&image, elf, cfg);
    let (_, console) = run_collecting(
        &mut m,
        VTime::from_ms(PACING_RUN_MS),
        &line_stop(LinePattern::Contains(PACING_ANCHORS[2].into())),
    );
    Some(String::from_utf8_lossy(&console).into_owned())
}

/// The ESP_LOG timestamps rewritten to `(T)` so only the text is left.
fn mask_stamps(text: &str) -> String {
    text.lines()
        .map(|l| {
            let l = l.trim_end_matches('\r');
            match (l.starts_with("I ("), l.find(") ")) {
                (true, Some(j)) => format!("I (T){}", &l[j + 1..]),
                _ => l.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What modelling the console TX pacing does to the three boot anchors, against the two device
/// captures with the band arithmetic.
///
/// The ROM and the bootloader wait for the console to drain, so each line costs its transmission
/// time on silicon. First the text: both profiles must print equal normalized consoles, which bites
/// because the ROM's `usb_uart_tx_one_char` drops a character after 5000 polls (rev101 ROM ELF).
/// Then the bands: with the `lru16k` line account all three anchors and both deltas land inside
/// them. The reset CPU rate is documented at `Machine::reset_cpu_hz`.
#[test]
fn t1_console_pacing_moves_the_boot_anchors_toward_the_device() {
    let test = "t1_console_pacing_moves_the_boot_anchors_toward_the_device";
    for (name, device) in PACING_DEVICE {
        let Some(fast) = pacing_console(test, name, TimingProfileId::Fast) else {
            return;
        };
        let paced = pacing_console(test, name, TimingProfileId::Device)
            .expect("the corpus that answered once answers twice");
        assert_eq!(
            mask_stamps(&fast),
            mask_stamps(&paced),
            "{test} {name}: the pacing changed the console text"
        );

        let before = anchor_stamps(test, &fast);
        let after = anchor_stamps(test, &paced);
        println!("{test} {name}: anchors {before:?} -> {after:?} against device {device:?}");

        // Absolute band, |t_emu - t_dev| <= max(10 ms, 20 % of t_dev), at all three anchors.
        for i in 0..3 {
            assert!(
                within_band(after[i], device[i], 10, 20),
                "{test} {name}: anchor {i} at {} ms is outside the band of {} ms",
                after[i],
                device[i]
            );
        }
        // Delta band, |dt_emu - dt_dev| <= max(5 ms, 20 % of dt_dev), both deltas.
        let d_emu = [after[1] - after[0], after[2] - after[1]];
        let d_dev = [device[1] - device[0], device[2] - device[1]];
        println!("{test} {name}: deltas {d_emu:?} against device {d_dev:?}");
        for i in 0..2 {
            assert!(
                within_band(d_emu[i], d_dev[i], 5, 20),
                "{test} {name}: delta {} of {} ms is outside the band of {} ms",
                i + 1,
                d_emu[i],
                d_dev[i]
            );
        }
    }
}
