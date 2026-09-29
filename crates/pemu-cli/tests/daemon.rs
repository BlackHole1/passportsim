//! The `passportsim` binary as child processes: `serve`, `mcp` and forwarding, with no corpus.
//!
//! Every child runs with `PASSPORTSIM_HOME=<fresh temporary directory>` and no other
//! `PASSPORTSIM_*` override, so a daemon the user runs is never found, stopped or reused. [`Home`]
//! stops its daemon on drop, and the tests run one at a time ([`serial`]).
//!
//! These run on macOS and Windows. The home is created owner-only through the platform layer, whose
//! owner-only check the runtime files are held to on both hosts; macOS also asserts exact modes. A
//! daemon of one home is told from others by `ps -E` (the environment) on macOS; Windows shows no
//! other process's environment, so each home runs its own copy of the binary, found by executable
//! path.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_passportsim");

/// The daemon count is per home, but ports and CPU are not.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Its daemon is stopped and its children reaped on drop.
pub struct Home {
    pub dir: PathBuf,
    /// [`BIN`] on macOS, a copy inside the home on Windows.
    pub exe: PathBuf,
    children: Mutex<Vec<std::process::Child>>,
}

impl Home {
    pub fn new(name: &str) -> Home {
        let dir = std::env::temp_dir().join(format!(
            "pemu-cli-daemon-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        pemu_host::platform::owner_only()
            .create_dir(&dir)
            .expect("an owner-only temporary home");
        let exe = if cfg!(windows) {
            let copy = dir.join("passportsim.exe");
            std::fs::copy(BIN, &copy).expect("this home's copy of the binary");
            copy
        } else {
            PathBuf::from(BIN)
        };
        Home {
            dir,
            exe,
            children: Mutex::new(Vec::new()),
        }
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.exe);
        command
            .args(args)
            .env("PASSPORTSIM_HOME", &self.dir)
            .current_dir(&self.dir);
        for (key, _) in std::env::vars_os() {
            if key
                .to_str()
                .is_some_and(|k| k.starts_with("PASSPORTSIM_") && k != "PASSPORTSIM_HOME")
            {
                command.env_remove(&key);
            }
        }
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args)
            .stdin(Stdio::null())
            .output()
            .expect("passportsim runs")
    }

    pub fn runtime(&self) -> PathBuf {
        self.dir.join("run")
    }

    pub fn discovery(&self) -> PathBuf {
        self.runtime().join("serve.json")
    }

    pub fn port(&self) -> Option<u16> {
        let text = std::fs::read_to_string(self.discovery()).ok()?;
        let at = text.find("\"port\":")? + "\"port\":".len();
        text[at..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    }

    /// `serve --headless --port 0`, waiting until it answers.
    pub fn spawn_daemon(&self) {
        self.spawn_daemon_with(&[]);
    }

    pub fn spawn_daemon_with(&self, extra: &[&str]) {
        let mut args = vec!["serve", "--headless", "--port", "0"];
        args.extend_from_slice(extra);
        let child = self
            .command(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("serve runs");
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(child);
        assert!(
            wait_until(Duration::from_secs(20), || {
                self.port()
                    .is_some_and(|port| std::net::TcpStream::connect(("127.0.0.1", port)).is_ok())
            }),
            "the daemon answers"
        );
    }

    pub fn stop(&self) {
        let _ = self.run(&["serve", "--stop"]);
        for mut child in self
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = child.wait();
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.stop();
        wait_until(Duration::from_secs(20), || daemons(self).is_empty());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Returns whether `done` held before `timeout`.
pub fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `ps -E` appends a same-user process's environment to its command line.
#[cfg(not(windows))]
pub fn daemons(home: &Home) -> Vec<u32> {
    let out = Command::new("/bin/ps")
        .args(["-E", "-ww", "-ax", "-o", "pid=,command="])
        .output()
        .expect("ps runs");
    let marker = format!("PASSPORTSIM_HOME={}", home.dir.display());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.contains(" serve") && line.contains(BIN))
        .filter(|line| line.split_whitespace().any(|word| word == marker.as_str()))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

/// Those running this home's copy of the binary with `serve` on their command line.
#[cfg(windows)]
pub fn daemons(home: &Home) -> Vec<u32> {
    powershell(
        "Get-CimInstance Win32_Process -Filter \"Name='passportsim.exe'\" | Where-Object { \
         $_.ExecutablePath -eq $env:PEMU_TEST_EXE -and $_.CommandLine -like '* serve*' } | \
         ForEach-Object { $_.ProcessId }",
        &home.exe,
    )
}

/// Numbers only: the console code page is not UTF-8 and tool text is localized.
#[cfg(windows)]
fn powershell(script: &str, exe: &std::path::Path) -> Vec<u32> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("PEMU_TEST_EXE", exe)
        .stdin(Stdio::null())
        .output()
        .expect("PowerShell runs");
    assert!(out.status.success(), "{script}: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// A console program started without its parent's console gets its own console host, which is what
/// draws a console window.
#[cfg(windows)]
fn console_hosts_of(pid: u32, home: &Home) -> Vec<u32> {
    powershell(
        &format!(
            "Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}' | Where-Object {{ \
             $_.Name -in @('conhost.exe', 'OpenConsole.exe') }} | ForEach-Object {{ $_.ProcessId }}"
        ),
        &home.exe,
    )
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("UTF-8")
}

#[test]
fn concurrent_spawning_clients_leave_exactly_one_daemon() {
    let _serial = serial();
    let home = Home::new("race");
    let clients: Vec<std::process::Child> = (0..6)
        .map(|i| {
            if i % 2 == 0 {
                home.command(&["mcp"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("mcp runs")
            } else {
                // A missing absolute path: the daemon refuses the firmware, so the race is about
                // the daemon alone.
                home.command(&["start", "/nonexistent/passportsim-race.bin"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("start runs")
            }
        })
        .collect();
    for (i, child) in clients.into_iter().enumerate() {
        let out = child.wait_with_output().expect("the client ends");
        let stderr = text(&out.stderr);
        assert!(
            !stderr.contains("E_DAEMON"),
            "client {i} found the daemon every other client found: {stderr}"
        );
    }
    // Losers exit at the lock; give them a moment.
    assert!(
        wait_until(Duration::from_secs(20), || daemons(&home).len() == 1),
        "exactly one daemon: {:?}",
        daemons(&home)
    );
}

#[test]
fn a_second_serve_exits_8_and_leaves_the_live_log_whole() {
    let _serial = serial();
    let home = Home::new("second");
    home.spawn_daemon();
    let log = home.dir.join("logs").join("serve.log");
    let logged = |log: &PathBuf, times: usize| {
        wait_until(Duration::from_secs(20), || {
            std::fs::read_to_string(log)
                .is_ok_and(|text| text.matches(" on http://127.0.0.1:").count() == times)
        })
    };
    assert!(logged(&log, 1), "the first daemon logged its start");
    let before = std::fs::read_to_string(&log).expect("the headless log");

    for args in [
        &["serve", "--headless"][..],
        &["serve", "--headless", "--port", "0"][..],
        &["serve", "--port", "0"][..],
    ] {
        let out = home.run(args);
        assert_eq!(out.status.code(), Some(8), "{args:?}: {out:?}");
        assert!(
            text(&out.stderr).starts_with("error: E_DAEMON: "),
            "{}",
            text(&out.stderr)
        );
    }
    let after = std::fs::read_to_string(&log).expect("the headless log");
    assert_eq!(
        after, before,
        "the losers neither truncated nor wrote the live log"
    );
    assert_eq!(daemons(&home).len(), 1);

    // A restarted daemon appends to the log it finds.
    home.stop();
    home.spawn_daemon();
    assert!(logged(&log, 2), "the second daemon logged its start");
    let restarted = std::fs::read_to_string(&log).expect("the headless log");
    assert!(
        restarted.starts_with(&before) && restarted.len() > before.len(),
        "appended, not truncated:\n{restarted}"
    );
}

#[test]
fn a_forwarded_refusal_prints_the_bytes_of_the_same_refusal_in_process() {
    let _serial = serial();
    let home = Home::new("identity");
    let local: Vec<Output> = [["status", "zz"], ["status", "p9"]]
        .iter()
        .flat_map(|args| {
            ["text", "json"]
                .map(|mode| home.run(&[args[0], args[1], "--output", mode, "--ephemeral"]))
        })
        .collect();
    home.spawn_daemon();
    let forwarded: Vec<Output> = [["status", "zz"], ["status", "p9"]]
        .iter()
        .flat_map(|args| {
            ["text", "json"].map(|mode| home.run(&[args[0], args[1], "--output", mode]))
        })
        .collect();
    for (local, forwarded) in local.iter().zip(&forwarded) {
        assert_eq!(local.status.code(), forwarded.status.code());
        assert_eq!(text(&local.stdout), text(&forwarded.stdout));
        assert_eq!(text(&local.stderr), text(&forwarded.stderr));
    }
    assert_eq!(forwarded[0].status.code(), Some(2), "zz is E_USAGE");
    assert_eq!(forwarded[2].status.code(), Some(1), "p9 is E_STATE");
    assert!(text(&forwarded[2].stderr).contains("hint: `status` lists the live instances"));
}

/// The adapter's stdout reaches end of file as soon as it exits, although the daemon it started
/// runs on, because the daemon holds no copy of that pipe; on Windows no console host was created,
/// so no console window appeared.
#[test]
fn an_mcp_client_sees_eof_and_the_daemon_its_adapter_started_outlives_it() {
    use std::io::{BufRead as _, Read as _, Write as _};
    let _serial = serial();
    let home = Home::new("eof");
    let mut adapter = home
        .command(&["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("mcp runs");
    let mut input = adapter.stdin.take().expect("piped stdin");
    let mut output = std::io::BufReader::new(adapter.stdout.take().expect("piped stdout"));
    let initialize = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-11-25" } });
    writeln!(input, "{initialize}").expect("the adapter reads stdin");
    input.flush().expect("flush");
    let mut line = String::new();
    output.read_line(&mut line).expect("the adapter answers");
    assert!(line.contains("\"result\""), "{line}");
    assert_eq!(daemons(&home).len(), 1, "the adapter spawned the daemon");

    // A read that never ends is the inherited-handle failure.
    drop(input);
    let (sent, eof) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = sent.send(output.read_to_end(&mut rest).map(|_| rest));
    });
    let rest = eof
        .recv_timeout(Duration::from_secs(20))
        .expect("end of file on the adapter's stdout while the daemon still runs")
        .expect("the pipe reads to its end");
    assert!(rest.is_empty(), "{}", text(&rest));
    let status = adapter.wait().expect("the adapter ends at end of input");
    assert!(status.success(), "{status:?}");

    let pids = daemons(&home);
    assert_eq!(pids.len(), 1, "the daemon outlived its adapter: {pids:?}");
    let port = home.port().expect("the discovery file names the port");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "and still answers"
    );
    #[cfg(windows)]
    assert_eq!(
        console_hosts_of(pids[0], &home),
        Vec::<u32>::new(),
        "the detached daemon got no console, so no console window"
    );
}

#[test]
fn a_start_usage_error_spawns_no_daemon() {
    let _serial = serial();
    let home = Home::new("start-usage");
    let out = home.run(&["start", "pk", "--power", "sideways"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        text(&out.stderr).starts_with("error: E_USAGE: "),
        "{}",
        text(&out.stderr)
    );
    assert!(!home.discovery().exists(), "no daemon was spawned");
    assert!(daemons(&home).is_empty());
}

#[test]
fn a_stopped_daemon_removes_its_discovery_and_token_files() {
    let _serial = serial();
    let home = Home::new("token");
    home.spawn_daemon();
    let token = home.runtime().join("token");
    assert!(token.is_file() && home.discovery().is_file());
    home.stop();
    assert!(!home.discovery().exists(), "serve.json removed");
    assert!(!token.exists(), "the token file removed");
    assert!(
        home.runtime().join("serve.lock").is_file(),
        "the lock file stays (its inode is what the lock is on)"
    );
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("one JSON object: {e}: {}", text(bytes)))
}

/// Synthetic, never device data: a one-segment bootloader at 0 and a partition table at 0x8000 with
/// one erased NVS partition. It boots nothing, which a lifecycle test does not need.
fn synthetic_flash() -> Vec<u8> {
    let mut flash = vec![0xFFu8; 8 * 1024 * 1024];
    let mut boot = vec![0xE9, 1, 2, 0x3F];
    boot.extend_from_slice(&0x4038_02E8u32.to_le_bytes());
    boot.extend_from_slice(&[0xEE, 0, 0, 0]);
    boot.extend_from_slice(&5u16.to_le_bytes());
    boot.push(3);
    boot.extend_from_slice(&3u16.to_le_bytes());
    boot.extend_from_slice(&199u16.to_le_bytes());
    boot.extend_from_slice(&[0; 5]);
    boot.extend_from_slice(&0x3FCD_5830u32.to_le_bytes());
    boot.extend_from_slice(&16u32.to_le_bytes());
    boot.extend_from_slice(&[7; 16]);
    while boot.len() % 16 != 15 {
        boot.push(0);
    }
    boot.push(0xEF ^ (0..16).fold(0u8, |c, _| c ^ 7));
    flash[..boot.len()].copy_from_slice(&boot);
    let mut row = vec![0xAA, 0x50, 0x01, 0x02];
    row.extend_from_slice(&0x9000u32.to_le_bytes());
    row.extend_from_slice(&0x4000u32.to_le_bytes());
    let mut label = [0u8; 16];
    label[..3].copy_from_slice(b"nvs");
    row.extend_from_slice(&label);
    row.extend_from_slice(&[0; 4]);
    flash[0x8000..0x8000 + row.len()].copy_from_slice(&row);
    flash
}

/// Each request is written and its answer line read, in order.
fn mcp_session(
    home: &Home,
    extra: &[&str],
    requests: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    mcp_session_in(home, &home.dir, extra, requests)
}

fn mcp_session_in(
    home: &Home,
    cwd: &std::path::Path,
    extra: &[&str],
    requests: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    use std::io::{BufRead as _, Write as _};
    let mut args = vec!["mcp"];
    args.extend_from_slice(extra);
    let mut child = home
        .command(&args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("mcp runs");
    let mut input = child.stdin.take().expect("piped stdin");
    let mut output = std::io::BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut answers = Vec::new();
    for request in requests {
        writeln!(input, "{request}").expect("the adapter reads stdin");
        input.flush().expect("flush");
        if request.get("id").is_none() {
            continue;
        }
        let mut line = String::new();
        output.read_line(&mut line).expect("the adapter answers");
        answers.push(serde_json::from_str(&line).expect("JSON-RPC on stdout"));
    }
    drop(input);
    let status = child.wait().expect("the adapter ends at end of input");
    assert!(status.success(), "{status:?}");
    answers
}

fn tool_names(answer: &serde_json::Value) -> Vec<String> {
    answer["result"]["tools"]
        .as_array()
        .expect("tools/list result")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name").to_string())
        .collect()
}

#[test]
fn an_instance_command_with_no_daemon_runs_in_process_and_spawns_none() {
    let _serial = serial();
    let home = Home::new("no-daemon");
    let out = home.run(&["status", "--output", "json"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(json(&out.stdout)["instances"], serde_json::json!([]));
    // It addresses an instance; with no daemon this process answers it.
    let out = home.run(&["status", "p1"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        text(&out.stderr).starts_with("error: E_STATE: "),
        "{}",
        text(&out.stderr)
    );
    assert!(!home.discovery().exists(), "no discovery file");
    assert!(daemons(&home).is_empty(), "no daemon");
}

#[test]
fn ephemeral_never_forwards_and_artifacts_is_refused_when_forwarding() {
    let _serial = serial();
    let home = Home::new("ephemeral");
    // Runs here: no daemon is spawned for it.
    let out = home.run(&[
        "start",
        "/nonexistent/passportsim-ephemeral.bin",
        "--ephemeral",
    ]);
    assert_ne!(out.status.code(), Some(0), "{out:?}");
    assert!(
        !home.discovery().exists(),
        "`--ephemeral` spawned no daemon"
    );
    assert!(daemons(&home).is_empty());

    let daemon_root = home.dir.join("daemon-artifacts");
    home.spawn_daemon_with(&["--artifacts", daemon_root.to_str().expect("UTF-8")]);
    let forwarded = json(&home.run(&["status", "--output", "json"]).stdout);
    let here = json(
        &home
            .run(&["status", "--output", "json", "--ephemeral"])
            .stdout,
    );
    assert_eq!(
        forwarded["artifacts_root"],
        daemon_root.to_str().expect("UTF-8"),
        "the daemon answered: {forwarded}"
    );
    assert_ne!(
        here["artifacts_root"], forwarded["artifacts_root"],
        "`--ephemeral` answered in this process: {here}"
    );

    let out = home.run(&["status", "p1", "--artifacts", "/tmp/elsewhere"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(
        text(&out.stderr),
        "error: E_USAGE: `--artifacts` moves the artifacts of this process, and this command runs \
         in the daemon\nhint: start the daemon with `passportsim serve --artifacts <dir>`, or add \
         `--ephemeral` to run this command here\n"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn caps_narrow_the_mcp_client_but_never_the_cli_and_a_stop_flushes_and_unpublishes() {
    let _serial = serial();
    let home = Home::new("caps");
    home.spawn_daemon();

    // Owner-only by the platform layer's check on both hosts (a protected DACL naming only this
    // user and SYSTEM on Windows), and by exact modes on macOS.
    let runtime = home.runtime();
    let logs = home.dir.join("logs");
    let files = [
        runtime.join("token"),
        runtime.join("serve.json"),
        runtime.join("serve.lock"),
        logs.join("serve.log"),
    ];
    for path in files.iter().chain([&runtime, &logs]) {
        pemu_host::platform::owner_only()
            .check(path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    #[cfg(unix)]
    {
        assert_eq!(mode(&runtime), 0o700, "run/");
        assert_eq!(mode(&logs), 0o700, "logs/");
        for file in &files {
            assert_eq!(mode(file), 0o600, "{}", file.display());
        }
    }

    let image = home.dir.join("synthetic.bin");
    std::fs::write(&image, synthetic_flash()).expect("the synthetic image");
    let out = home.run(&["start", image.to_str().expect("UTF-8"), "--output", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let instance = json(&out.stdout)["instance"]
        .as_str()
        .expect("the started instance")
        .to_string();

    // The CLI forwards with every daemon group, so `mic_set` (audio) is not gated.
    let out = home.run(&["mic_set", "silence", "--instance", &instance]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    let initialize = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-11-25" } });
    let initialized =
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let list = serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
    let call = serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "passport_mic_set", "arguments": { "kind": "silence", "instance": instance } } });
    let requests = [initialize.clone(), initialized.clone(), list, call];

    // An MCP client without `--caps` has the core tools only.
    let core = mcp_session(&home, &[], &requests);
    let names = tool_names(&core[1]);
    assert!(names.iter().any(|n| n == "passport_status"), "{names:?}");
    assert!(!names.iter().any(|n| n == "passport_mic_set"), "{names:?}");
    assert_eq!(core[2]["result"]["isError"], true, "{}", core[2]);
    let refusal = core[2]["result"]["structuredContent"].to_string();
    assert!(
        refusal.contains("E_STATE") && refusal.contains("--caps audio"),
        "{refusal}"
    );

    // `--caps audio` adds the audio tools, and the same call runs.
    let audio = mcp_session(&home, &["--caps", "audio"], &requests);
    assert!(
        tool_names(&audio[1])
            .iter()
            .any(|n| n == "passport_mic_set")
    );
    assert_ne!(audio[2]["result"]["isError"], true, "{}", audio[2]);

    // The adapter names its workspace, so an MCP client can validate a scenario file there; the
    // same file outside any workspace is refused.
    let workspace = home.dir.join("ws");
    std::fs::create_dir_all(workspace.join(".git")).expect("a workspace");
    std::fs::create_dir_all(workspace.join("tests/scenarios")).expect("a scenario dir");
    let scenario = include_str!("../../../tests/scenarios/env-journal.yaml");
    let inside = workspace.join("tests/scenarios/env-journal.yaml");
    std::fs::write(&inside, scenario).expect("a workspace scenario");
    let outside = home.dir.join("env-journal.yaml");
    std::fs::write(&outside, scenario).expect("a stray scenario");
    let validate = |file: &std::path::Path| {
        let call = serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "passport_scenario", "arguments": { "op": "validate", "file": file.to_str().expect("UTF-8") } } });
        let requests = [initialize.clone(), initialized.clone(), call];
        mcp_session_in(&home, &workspace.join("tests"), &[], &requests).remove(1)
    };
    let read = validate(&inside);
    assert_ne!(read["result"]["isError"], true, "{read}");
    let refused = validate(&outside);
    assert_eq!(refused["result"]["isError"], true, "{refused}");

    // `--caps device` is refused by the adapter before it reaches the daemon.
    let out = home.run(&["mcp", "--caps", "device"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    // Stopping flushes every instance's artifacts and unpublishes the files. On macOS the stop is
    // SIGTERM; Windows has no SIGTERM and a detached daemon has no console for a control event, so
    // there it is the authenticated `serve --stop`.
    let pids = daemons(&home);
    assert_eq!(pids.len(), 1, "{pids:?}");
    #[cfg(unix)]
    {
        let killed = Command::new("/bin/kill")
            .args(["-TERM", &pids[0].to_string()])
            .status()
            .expect("kill runs");
        assert!(killed.success());
    }
    #[cfg(windows)]
    {
        let out = home.run(&["serve", "--stop"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
    }
    assert!(
        wait_until(Duration::from_secs(60), || !home.discovery().exists()
            && daemons(&home).is_empty()),
        "the daemon exits and removes serve.json"
    );
    assert!(!runtime.join("token").exists(), "the token file removed");
    let summaries: Vec<PathBuf> = std::fs::read_dir(home.dir.join("data").join("artifacts"))
        .expect("the daemon's artifact root")
        .flatten()
        .map(|run| run.path().join(&instance).join("summary.json"))
        .filter(|summary| summary.is_file())
        .collect();
    assert_eq!(summaries.len(), 1, "one flushed summary");
    let summary = json(&std::fs::read(&summaries[0]).expect("summary.json"));
    assert_eq!(summary["reason"], "shutdown", "{summary}");
    assert!(
        std::fs::read_to_string(logs.join("serve.log"))
            .expect("the log")
            .contains("every instance was stopped and its artifacts flushed")
    );
}
