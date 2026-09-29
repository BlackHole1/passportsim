//! Daemon lifecycle: discovery, staleness, auto-spawn, the bind policy and shutdown.
//!
//! A running daemon publishes `serve.json` (owner-only, in the runtime role) with its port and
//! token. A client judges staleness by connecting to that port with that token, never by pid, since
//! pids are recycled; readiness after [`spawn`] is the same probe, never a sleep.
//!
//! [`bind`] tries the default port and falls back to port 0 on any error, so a reserved or occupied
//! range never blocks start-up. Every shutdown path (`serve --stop`, signals, idle exit) sets one
//! [`Shutdown`] flag, and `Server::shutdown_now` then flushes every instance's artifacts.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::auth::{Token, TokenError};
use crate::paths::{GuardError, OwnerOnlyFiles};
use crate::platform::{DaemonSpawn, DetachedChild, PlatformError};

pub const DEFAULT_PORT: u16 = 8765;

/// The discovery file in the runtime role.
pub const DISCOVERY_FILE: &str = "serve.json";

/// The lifetime lock in the runtime role, held from before the bind until exit, so two `serve`
/// processes racing on one runtime directory cannot both bind and publish.
pub const LOCK_FILE: &str = "serve.lock";

pub const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);

/// The loopback address the daemon binds. Non-loopback is refused: the API runs arbitrary firmware
/// and, with device caps, touches hardware.
pub const BIND_ADDR: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// The health route a probe calls, the cheapest authenticated route.
pub const HEALTH_PATH: &str = "/v1/health";

pub const SHUTDOWN_PATH: &str = "/v1/shutdown";

/// How long a probe waits to connect and for the answer: a live loopback daemon replies in
/// microseconds, and a closed port refuses at once.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);

pub const READY_TIMEOUT: Duration = Duration::from_secs(20);

const READY_POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    /// The port really bound (the fallback port when the default was taken).
    pub port: u16,
    pub token: Token,
    /// Informational only: staleness is decided by connecting, because pids are recycled.
    pub pid: u32,
    pub version: String,
}

impl Discovery {
    pub fn new(port: u16, token: Token, pid: u32) -> Discovery {
        Discovery {
            port,
            token,
            pid,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from((BIND_ADDR, self.port))
    }

    pub fn to_json_text(&self) -> String {
        serde_json::json!({
            "port": self.port,
            "token": self.token.to_hex(),
            "pid": self.pid,
            "version": self.version,
        })
        .to_string()
    }

    /// Parses the discovery file. Every field is required: a partial file is truncated or from
    /// another build, and guessing would send a token to the wrong port.
    pub fn parse(text: &str) -> Result<Discovery, DiscoveryError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|_| DiscoveryError::Malformed("not JSON"))?;
        let port = value
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .ok_or(DiscoveryError::Malformed("`port`"))?;
        let token = value
            .get("token")
            .and_then(serde_json::Value::as_str)
            .ok_or(DiscoveryError::Malformed("`token`"))?;
        let token = Token::parse_hex(token).map_err(DiscoveryError::Token)?;
        let pid = value
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
            .ok_or(DiscoveryError::Malformed("`pid`"))?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_str)
            .ok_or(DiscoveryError::Malformed("`version`"))?
            .to_string();
        Ok(Discovery {
            port,
            token,
            pid,
            version,
        })
    }
}

#[derive(Debug)]
pub enum DiscoveryError {
    Malformed(&'static str),
    Token(TokenError),
    Guard(GuardError),
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoveryError::Malformed(what) => {
                write!(f, "the discovery file has no usable {what}")
            }
            DiscoveryError::Token(e) => write!(f, "the discovery file's token is unusable: {e}"),
            DiscoveryError::Guard(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

/// The discovery and token files in one runtime directory, all created owner-only; every read
/// re-checks, so a file another user could write is refused rather than followed to their port.
pub struct DiscoveryStore<'a> {
    dir: PathBuf,
    files: OwnerOnlyFiles<'a>,
}

impl<'a> DiscoveryStore<'a> {
    pub fn new(dir: impl Into<PathBuf>, files: OwnerOnlyFiles<'a>) -> DiscoveryStore<'a> {
        DiscoveryStore {
            dir: dir.into(),
            files,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(DISCOVERY_FILE)
    }

    /// Publishes `discovery`, creating the runtime directory owner-only first. The token also gets
    /// its own file, which is enough for a client to reach the daemon.
    pub fn publish(&self, discovery: &Discovery) -> Result<(), DiscoveryError> {
        self.files
            .create_dir(&self.dir)
            .map_err(DiscoveryError::Guard)?;
        discovery
            .token
            .store(&self.files, &self.dir)
            .map_err(DiscoveryError::Guard)?;
        // Written beside the final name and renamed over it, so a CLI polling for a daemon it just
        // spawned never reads a half-written file as malformed.
        let path = self.path();
        let partial = path.with_extension(format!("json.tmp.{}", std::process::id()));
        self.files
            .write(&partial, discovery.to_json_text().as_bytes())
            .map_err(DiscoveryError::Guard)?;
        std::fs::rename(&partial, &path).map_err(|e| {
            let _ = std::fs::remove_file(&partial);
            DiscoveryError::Guard(GuardError::Platform(crate::platform::PlatformError::Io {
                path: path.clone(),
                source: e,
            }))
        })
    }

    /// Reads the discovery file, or `Ok(None)` when there is none. A file failing the owner-only
    /// check is an error, so another user cannot hide a running daemon and get a second started.
    pub fn read(&self) -> Result<Option<Discovery>, DiscoveryError> {
        let path = self.path();
        if !path.exists() {
            return Ok(None);
        }
        let bytes = self.files.read(&path).map_err(DiscoveryError::Guard)?;
        let text = String::from_utf8(bytes).map_err(|_| DiscoveryError::Malformed("UTF-8"))?;
        Discovery::parse(&text).map(Some)
    }

    /// Takes the exclusive lifetime lock, creating the directory and file owner-only first.
    /// `Ok(None)` means a daemon is running or starting here. The lock is `File::try_lock`
    /// (`flock` on macOS, `LockFileEx` on Windows), released by the OS however the process exits.
    /// The file is never removed: that would let a third process lock a new inode while the
    /// second still holds the old one.
    pub fn lock_lifetime(&self) -> Result<Option<LifetimeLock>, DiscoveryError> {
        self.files
            .create_dir(&self.dir)
            .map_err(DiscoveryError::Guard)?;
        let path = self.dir.join(LOCK_FILE);
        let file = if path.exists() {
            self.files.check(&path).map_err(DiscoveryError::Guard)?;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| {
                    DiscoveryError::Guard(GuardError::Platform(PlatformError::io(&path, e)))
                })?
        } else {
            self.files
                .create_file(&path)
                .map_err(DiscoveryError::Guard)?
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(LifetimeLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(DiscoveryError::Guard(
                GuardError::Platform(PlatformError::io(&path, e)),
            )),
        }
    }

    /// Removes the token file on exit, only while it still holds `token`, so a daemon that
    /// published later keeps its own.
    pub fn remove_token_if(&self, token: &Token) -> io::Result<()> {
        let path = self.dir.join(crate::auth::TOKEN_FILE);
        match Token::load(&self.files, &self.dir) {
            Ok(current) if current.matches(token) => match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    pub fn remove(&self) -> io::Result<()> {
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[derive(Debug)]
pub struct LifetimeLock {
    _file: std::fs::File,
}

/// What a probe found at a port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Running,
    Gone,
    /// Something answered but refused the token, so it is not our daemon: stale, like a recycled
    /// pid would be.
    Foreign,
}

/// Connecting to a recorded port with a recorded token. A trait so the staleness rule is testable
/// without a listener and reusable by the CLI.
pub trait Probe {
    fn liveness(&self, addr: SocketAddr, token: &Token) -> Liveness;
}

/// The real probe: an authenticated `GET /v1/health` over loopback.
#[derive(Clone, Copy, Debug, Default)]
pub struct HttpProbe;

impl Probe for HttpProbe {
    fn liveness(&self, addr: SocketAddr, token: &Token) -> Liveness {
        match request(addr, "GET", HEALTH_PATH, token, None) {
            Ok(response) if response.status == 200 => Liveness::Running,
            Ok(_) => Liveness::Foreign,
            Err(_) => Liveness::Gone,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonState {
    Running(Discovery),
    /// A discovery file exists but its port does not answer with its token.
    Stale(Discovery),
    NotRunning,
}

pub fn resolve(
    store: &DiscoveryStore<'_>,
    probe: &dyn Probe,
) -> Result<DaemonState, DiscoveryError> {
    let Some(discovery) = store.read()? else {
        return Ok(DaemonState::NotRunning);
    };
    match probe.liveness(discovery.addr(), &discovery.token) {
        Liveness::Running => Ok(DaemonState::Running(discovery)),
        Liveness::Gone | Liveness::Foreign => Ok(DaemonState::Stale(discovery)),
    }
}

#[derive(Debug)]
pub struct Bound {
    pub listener: TcpListener,
    pub port: u16,
    pub fell_back: bool,
    pub first_error: Option<io::Error>,
}

/// Binds loopback, preferring `port` and falling back to port 0 on any error (a reserved range,
/// another daemon, a Hyper-V/WSL/WinNAT reservation). Only a failure of port 0 is fatal.
///
/// On Windows the listener sets `SO_EXCLUSIVEADDRUSE` and never `SO_REUSEADDR`; macOS keeps std's
/// bind, whose `SO_REUSEADDR` still refuses a second listener without `SO_REUSEPORT`.
pub fn bind(port: u16) -> io::Result<Bound> {
    let preferred = SocketAddr::from((BIND_ADDR, port));
    match crate::platform::listen_loopback(&preferred) {
        Ok(listener) => {
            let port = listener.local_addr()?.port();
            // The daemon's own port is never a bridge destination.
            crate::relay_wisp::deny_port(port);
            Ok(Bound {
                listener,
                port,
                fell_back: false,
                first_error: None,
            })
        }
        Err(first) => {
            let listener = crate::platform::listen_loopback(&SocketAddr::from((BIND_ADDR, 0)))?;
            let port = listener.local_addr()?.port();
            crate::relay_wisp::deny_port(port);
            Ok(Bound {
                listener,
                port,
                fell_back: true,
                first_error: Some(first),
            })
        }
    }
}

/// The hint for Windows bind error 10013 (WSAEACCES), usually an excluded port range rather than a
/// permission problem.
pub fn wsaeacces_hint() -> &'static str {
    "Windows refused the port with WSAEACCES (10013). A Hyper-V, WSL or WinNAT reservation \
     usually owns it; list the excluded ranges with `netsh int ipv4 show excludedportrange \
     protocol=tcp`. The daemon has fallen back to a port the system chose."
}

/// The shutdown flag every stop path ends in. A flag, because a signal handler may only store to
/// an atomic, and the daemon's loop observing it is where every flush happens.
#[derive(Clone, Debug, Default)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
}

impl Shutdown {
    pub fn new() -> Shutdown {
        Shutdown::default()
    }

    /// Requests shutdown. Idempotent: two signals and a `POST /v1/shutdown` racing is one stop.
    pub fn request(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

const SIGNAL_POLL: Duration = Duration::from_millis(50);

/// Installs the platform's shutdown signals (`SIGINT`/`SIGTERM`, or the Windows console events)
/// and bridges them into `shutdown` through a watcher thread, since a handler may only store to an
/// atomic. Returns the installed signals for the log.
pub fn watch_signals(
    signals: &'static dyn crate::platform::Signals,
    shutdown: Shutdown,
) -> Result<&'static [crate::platform::ShutdownSignal], PlatformError> {
    let flag = signals.install_shutdown()?;
    std::thread::Builder::new()
        .name("pemu-signals".to_string())
        .spawn(move || {
            while !shutdown.requested() {
                if flag.is_set() {
                    shutdown.request();
                    return;
                }
                std::thread::sleep(SIGNAL_POLL);
            }
        })
        .map_err(|e| PlatformError::io(PathBuf::new(), e))?;
    Ok(signals.shutdown_signals())
}

/// Sends the authenticated `POST /v1/shutdown` of `serve --stop`. A 200 means accepted, not exited;
/// use [`wait_gone`] to observe the exit.
pub fn stop(discovery: &Discovery) -> io::Result<()> {
    let response = request(
        discovery.addr(),
        "POST",
        SHUTDOWN_PATH,
        &discovery.token,
        Some(b""),
    )?;
    if response.status == 200 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "the daemon answered {} to {SHUTDOWN_PATH}",
            response.status
        )))
    }
}

pub fn wait_gone(discovery: &Discovery, probe: &dyn Probe, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if probe.liveness(discovery.addr(), &discovery.token) != Liveness::Running {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(READY_POLL);
    }
}

/// The `serve --headless` command line of an auto-spawn, from the caller's own executable so a
/// checkout build spawns itself. Stdio is not set here: [`crate::platform::DaemonSpawn`] nulls all
/// three handles, so the daemon never inherits an MCP client's stdout.
pub fn spawn_command(exe: &Path) -> Command {
    let mut command = Command::new(exe);
    command.arg("serve").arg("--headless");
    command
}

#[derive(Debug)]
pub enum SpawnError {
    Platform(PlatformError),
    NotReady,
    Discovery(DiscoveryError),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnError::Platform(e) => write!(f, "{e}"),
            SpawnError::NotReady => f.write_str(
                "the daemon was started but no port answered with its token; its log in the logs \
                 directory says why",
            ),
            SpawnError::Discovery(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Auto-spawns `serve --headless` and waits until it answers, re-reading the discovery file since
/// the port is unknown until the daemon publishes it.
pub fn spawn(
    exe: &Path,
    spawner: &dyn DaemonSpawn,
    store: &DiscoveryStore<'_>,
    probe: &dyn Probe,
    timeout: Duration,
) -> Result<(DetachedChild, Discovery), SpawnError> {
    let mut command = spawn_command(exe);
    let child = spawner
        .spawn_detached(&mut command)
        .map_err(SpawnError::Platform)?;
    let deadline = Instant::now() + timeout;
    loop {
        match resolve(store, probe).map_err(SpawnError::Discovery)? {
            DaemonState::Running(discovery) => return Ok((child, discovery)),
            DaemonState::Stale(_) | DaemonState::NotRunning => {}
        }
        if Instant::now() >= deadline {
            return Err(SpawnError::NotReady);
        }
        std::thread::sleep(READY_POLL);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// One authenticated HTTP/1.1 request to a loopback daemon: a deliberately tiny client, with
/// `Connection: close` and a read to the announced length. `None` sends no body; `Some(&[])` an
/// empty one.
pub fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: &Token,
    body: Option<&[u8]>,
) -> io::Result<ClientResponse> {
    request_with_timeout(addr, method, path, token, body, PROBE_TIMEOUT)
}

/// [`request`] with a longer read timeout, since a `run` holds its answer for as long as the guest
/// runs. The connect keeps [`PROBE_TIMEOUT`].
pub fn request_with_timeout(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: &Token,
    body: Option<&[u8]>,
    timeout: Duration,
) -> io::Result<ClientResponse> {
    request_with_headers(addr, method, path, token, &[], body, timeout)
}

/// [`request_with_timeout`] with extra headers (the `mcp` relay's `MCP-Protocol-Version`). A name
/// or value with a line break is refused, so it can never split the request.
pub fn request_with_headers(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: &Token,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> io::Result<ClientResponse> {
    if headers
        .iter()
        .any(|(name, value)| name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a request header holds a line break",
        ));
    }
    let mut stream = crate::platform::connect_loopback(&addr, PROBE_TIMEOUT)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(PROBE_TIMEOUT))?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Connection: close\r\n",
        port = addr.port(),
        token = token.to_hex(),
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body)?;
    }
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other("no HTTP status line"))?;
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = Vec::new();
    match length {
        Some(n) => {
            body.resize(n, 0);
            reader.read_exact(&mut body)?;
        }
        None => {
            reader.read_to_end(&mut body)?;
        }
    }
    Ok(ClientResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::OwnerOnlyFiles;
    use crate::platform::fake::FakeOwnerOnly;
    use std::cell::RefCell;

    fn token() -> Token {
        Token::from_bytes([0x5a; 32])
    }

    struct ScriptedProbe {
        answer: Liveness,
        asked: RefCell<Vec<(SocketAddr, String)>>,
    }

    impl ScriptedProbe {
        fn new(answer: Liveness) -> ScriptedProbe {
            ScriptedProbe {
                answer,
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl Probe for ScriptedProbe {
        fn liveness(&self, addr: SocketAddr, token: &Token) -> Liveness {
            self.asked.borrow_mut().push((addr, token.to_hex()));
            self.answer
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-daemon-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        dir
    }

    #[test]
    fn a_header_cannot_split_a_request() {
        let addr = SocketAddr::from((BIND_ADDR, 9));
        for headers in [[("x-a", "1\r\nx-b: 2")], [("x-a\n", "1")]] {
            let refused = request_with_headers(
                addr,
                "POST",
                "/mcp",
                &token(),
                &headers,
                None,
                PROBE_TIMEOUT,
            )
            .expect_err("refused before any connection");
            assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn a_discovery_record_round_trips_through_its_file_text() {
        let d = Discovery::new(49152, token(), 4242);
        let parsed = Discovery::parse(&d.to_json_text()).expect("its own text parses");
        assert_eq!(parsed, d);
        assert_eq!(parsed.addr().port(), 49152);
        assert_eq!(parsed.addr().ip().to_string(), "127.0.0.1");
    }

    #[test]
    fn a_partial_or_foreign_discovery_file_is_refused() {
        for text in [
            "{}",
            "not json",
            r#"{"token":"00","pid":1,"version":"0.1.0"}"#,
            r#"{"port":8765,"pid":1,"version":"0.1.0"}"#,
            r#"{"port":8765,"token":"zz","pid":1,"version":"0.1.0"}"#,
            r#"{"port":99999,"token":"00","pid":1,"version":"0.1.0"}"#,
        ] {
            assert!(
                Discovery::parse(text).is_err(),
                "{text} must not parse as a discovery file"
            );
        }
    }

    #[test]
    fn publish_then_read_returns_the_same_record() {
        let dir = temp_dir("publish");
        let guard = FakeOwnerOnly::new();
        let store = DiscoveryStore::new(dir.join("run"), OwnerOnlyFiles::new(&guard));
        assert_eq!(store.read().expect("no file yet"), None);
        let d = Discovery::new(8765, token(), 7);
        store.publish(&d).expect("publish");
        assert_eq!(store.read().expect("read back"), Some(d));
        let loaded = Token::load(&OwnerOnlyFiles::new(&guard), store.dir()).expect("token file");
        assert!(loaded.matches(&token()));
        assert!(guard.checked(&store.path()));
        store.remove().expect("remove");
        assert_eq!(store.read().expect("gone"), None);
        store.remove().expect("removing twice is fine");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_reader_never_sees_a_partial_discovery_file_and_no_temporary_file_is_left() {
        let dir = temp_dir("publish-race");
        let guard = FakeOwnerOnly::new();
        let files = OwnerOnlyFiles::new(&guard);
        let store = DiscoveryStore::new(dir.join("run"), files);
        store
            .publish(&Discovery::new(8765, token(), 7))
            .expect("publish");
        let leftover = store
            .path()
            .with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&leftover, b"{\"port\":").expect("a crashed publish's leftover");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for port in 0..300u16 {
                    store
                        .publish(&Discovery::new(10_000 + port, token(), 7))
                        .expect("publish");
                }
            });
            for _ in 0..300 {
                match store.read() {
                    Ok(Some(_)) => {}
                    other => panic!("a reader saw {other:?} while the file was republished"),
                }
            }
        });
        let names: Vec<String> = std::fs::read_dir(store.dir())
            .expect("the runtime directory lists")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.contains(".tmp.")),
            "no temporary file is left: {names:?}"
        );
        assert_eq!(
            store.read().expect("read").map(|d| d.port),
            Some(10_299),
            "the last publish wins"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_discovery_file_that_fails_the_owner_only_guard_is_an_error_not_an_absence() {
        let dir = temp_dir("widened");
        let guard = FakeOwnerOnly::new();
        let store = DiscoveryStore::new(dir.join("run"), OwnerOnlyFiles::new(&guard));
        store
            .publish(&Discovery::new(8765, token(), 7))
            .expect("publish");
        guard.refuse(store.path());
        assert!(
            matches!(store.read(), Err(DiscoveryError::Guard(_))),
            "a widened discovery file must refuse, not read as `no daemon`"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn staleness_is_decided_by_connecting_with_the_token_not_by_the_pid() {
        let dir = temp_dir("stale");
        let guard = FakeOwnerOnly::new();
        let store = DiscoveryStore::new(dir.join("run"), OwnerOnlyFiles::new(&guard));
        let discovery = Discovery::new(8765, token(), std::process::id());
        store.publish(&discovery).expect("publish");

        let gone = ScriptedProbe::new(Liveness::Gone);
        assert_eq!(
            resolve(&store, &gone).expect("resolve"),
            DaemonState::Stale(discovery.clone()),
            "a live pid does not make a discovery file fresh"
        );
        assert_eq!(
            gone.asked.borrow().as_slice(),
            &[(discovery.addr(), token().to_hex())],
            "the probe is asked with the recorded port and token"
        );

        let foreign = ScriptedProbe::new(Liveness::Foreign);
        assert_eq!(
            resolve(&store, &foreign).expect("resolve"),
            DaemonState::Stale(discovery.clone()),
            "a server that refuses our token is not our daemon"
        );

        let running = ScriptedProbe::new(Liveness::Running);
        assert_eq!(
            resolve(&store, &running).expect("resolve"),
            DaemonState::Running(discovery)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn with_no_discovery_file_the_daemon_is_not_running() {
        let dir = temp_dir("absent");
        let guard = FakeOwnerOnly::new();
        let store = DiscoveryStore::new(dir.join("run"), OwnerOnlyFiles::new(&guard));
        let probe = ScriptedProbe::new(Liveness::Running);
        assert_eq!(
            resolve(&store, &probe).expect("resolve"),
            DaemonState::NotRunning
        );
        assert!(
            probe.asked.borrow().is_empty(),
            "with no file there is no port to probe"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_bind_policy_takes_the_free_port_and_falls_back_when_it_is_taken() {
        // The "preferred" port is one the kernel just handed out, never a fixed port.
        let first = bind(0).expect("port 0 always binds");
        assert!(!first.fell_back);
        assert!(first.first_error.is_none());
        assert_ne!(first.port, 0, "the real port is recorded, never 0");
        assert_eq!(
            first.listener.local_addr().expect("addr").ip().to_string(),
            "127.0.0.1",
            "non-loopback binding is refused"
        );

        let taken = first.port;
        TcpListener::bind(SocketAddr::from((BIND_ADDR, taken)))
            .map(|_| ())
            .expect_err("another socket cannot bind the daemon's port");
        let second = bind(taken).expect("the fallback always binds");
        assert!(
            second.fell_back,
            "a taken preferred port falls back instead of failing"
        );
        assert!(
            second.first_error.is_some(),
            "the first error is kept for the log"
        );
        assert_ne!(second.port, taken);
        assert_ne!(second.port, 0);
    }

    #[test]
    fn the_wsaeacces_hint_names_the_command_that_shows_the_reservation() {
        let hint = wsaeacces_hint();
        assert!(hint.contains("10013"));
        assert!(hint.contains("netsh int ipv4 show excludedportrange protocol=tcp"));
        assert!(hint.contains("Hyper-V"));
    }

    #[test]
    fn the_spawn_command_asks_for_a_headless_serve_and_sets_no_stdio() {
        let command = spawn_command(Path::new("/opt/passportsim"));
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, ["serve", "--headless"]);
        assert_eq!(command.get_program().to_string_lossy(), "/opt/passportsim");
    }

    #[test]
    fn a_signal_reaches_the_shutdown_flag_the_daemon_thread_observes() {
        let host: &'static crate::platform::fake::FakeHost =
            Box::leak(Box::new(crate::platform::fake::FakeHost::new()));
        let shutdown = Shutdown::new();
        let installed =
            watch_signals(&host.signals, shutdown.clone()).expect("the fake installs handlers");
        assert!(
            !installed.is_empty(),
            "a host lists the signals it listens on"
        );
        assert!(!shutdown.requested());
        host.signals.deliver_shutdown();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !shutdown.requested() {
            assert!(
                Instant::now() < deadline,
                "the signal never reached the flag"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_stop_request_is_idempotent_on_the_flag() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.requested());
        shutdown.request();
        let clone = shutdown.clone();
        clone.request();
        assert!(shutdown.requested(), "two stop paths are one stop");
    }
}
