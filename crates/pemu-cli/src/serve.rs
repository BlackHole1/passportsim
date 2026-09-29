//! `passportsim serve`: the daemon, wiring what `pemu-host` built.
//!
//! Start-up, in order:
//!
//! 1. The directory roles are resolved (`PASSPORTSIM_HOME` and the other overrides apply).
//! 2. The lifetime lock `serve.lock` of the runtime directory is taken. A process that cannot take
//!    it, or finds a daemon already answering, exits 8 (`E_DAEMON`), so two daemons never share a
//!    runtime directory. Only then is the headless log opened, for appending.
//! 3. A fresh 256-bit token and the loopback bind: the default port first, port 0 on any bind
//!    error, with the Windows WSAEACCES hint.
//! 4. The discovery and token files are published owner-only; the listener is already bound, so a
//!    client that sees the file can connect at once.
//! 5. The machine factory, host clock, artifacts root and audio root are installed.
//! 6. Shutdown signals, `serve --stop` and the ten-minute idle exit set the shutdown flag.
//! 7. `serve_blocking` returns once every instance stopped and flushed its artifacts; the discovery
//!    and token files are removed if still this daemon's.
//!
//! A foreground `serve` prints the URL, the token path (never the token) and a UI URL with a
//! single-use 60-second launch code, and logs to stderr. `--headless` is what an auto-spawn runs:
//! its stdio is the null device, its log is the owner-only `serve.log`, and it prints no UI line,
//! because a credential never reaches a log.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use clap::ArgMatches;
use pemu_api::spec::CapsGroup;
use pemu_host::auth::{Auth, Token};
use pemu_host::daemon::{self, DaemonState, Discovery, DiscoveryStore, HttpProbe, Shutdown};
use pemu_host::http::Server;
use pemu_host::paths::{HostPaths, OwnerOnlyFiles};
use pemu_host::pool::Pool;

use crate::render::{self, Mode, Stream};
use crate::{Outcome, exit, forward, paths, tree};

/// In the logs role.
pub const LOG_FILE: &str = "serve.log";

/// Stopping flushes every instance's artifacts first, so this is generous.
const STOP_WAIT: Duration = Duration::from_secs(60);

enum Log {
    /// Standard error.
    Stderr,
    /// `serve.log` in the logs role, or nowhere when it could not be opened (the daemon still
    /// serves).
    File(Option<std::fs::File>),
}

impl Log {
    fn line(&mut self, text: &str) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let line = format!("[{stamp}] serve: {text}\n");
        match self {
            Log::Stderr => render::write(&line, Stream::Stderr),
            Log::File(Some(file)) => {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
            Log::File(None) => {}
        }
    }
}

pub fn run(matches: &ArgMatches) -> Outcome {
    let artifacts = matches.get_one::<String>(tree::ARTIFACTS_ARG);
    let paths = paths::resolver(artifacts.map(String::as_str));
    let store = match forward::store(&paths) {
        Ok(store) => store,
        Err(why) => return forward::infra(&why, Mode::Text),
    };
    if matches.get_flag(tree::STOP_ARG) {
        return stop(&store);
    }
    let caps = match matches.get_one::<String>(tree::CAPS_ARG) {
        None => std::collections::BTreeSet::from([CapsGroup::Core]),
        Some(list) => match pemu_host::mcp_stdio::parse_caps(list) {
            // `--caps device` needs `--allow-device` on the same command line. Each flash still
            // needs a person; the flag only decides whether the group exists in this process.
            Ok(caps)
                if caps.contains(&CapsGroup::Device)
                    && !matches.get_flag(tree::ALLOW_DEVICE_ARG) =>
            {
                return usage(
                    "`--caps device` needs `--allow-device`; every flash still passes the checks and \
                     a person's confirmation",
                );
            }
            Ok(caps) => caps,
            Err(why) => return usage(&format!("`--caps {list}`: {why}")),
        },
    };
    // The lock comes before everything that could collide with another daemon: the bind, the token
    // and discovery files, and the log. The loser exits 8, and a client that spawned it keeps
    // polling the discovery file, where the winner appears.
    let _lock = match store.lock_lifetime() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return forward::infra(
                &format!(
                    "another daemon holds {}: one is running or starting in this runtime \
                     directory; `passportsim serve --stop` stops it",
                    daemon::LOCK_FILE
                ),
                Mode::Text,
            );
        }
        Err(e) => return forward::infra(&format!("the lifetime lock: {e}"), Mode::Text),
    };
    let headless = matches.get_flag(tree::HEADLESS_ARG);
    if let Some(running) = match forward::running(&store) {
        Ok(running) => running,
        Err(why) => return forward::infra(&why, Mode::Text),
    } {
        return forward::infra(
            &format!(
                "a daemon already answers on port {} (pid {}); `passportsim serve --stop` stops \
                 it",
                running.port, running.pid
            ),
            Mode::Text,
        );
    }
    // Holding the lock with no daemon answering, the log is this process's to write; appending
    // keeps an earlier daemon's lines.
    let mut log = if headless {
        Log::File(open_log(&paths))
    } else {
        Log::Stderr
    };
    match serve(matches, &paths, &store, caps, headless, &mut log) {
        Ok(()) => Outcome::default(),
        Err(why) => {
            if headless {
                log.line(&format!("not serving: {why}"));
            }
            forward::infra(&why, Mode::Text)
        }
    }
}

/// Steps 2 to 7 of the module header.
fn serve(
    matches: &ArgMatches,
    paths: &HostPaths,
    store: &DiscoveryStore<'_>,
    caps: std::collections::BTreeSet<CapsGroup>,
    headless: bool,
    log: &mut Log,
) -> Result<(), String> {
    let token = Token::generate().map_err(|e| format!("no token: {e}"))?;
    let port = matches
        .get_one::<u16>(tree::PORT_ARG)
        .copied()
        .unwrap_or(daemon::DEFAULT_PORT);
    let bound = daemon::bind(port).map_err(|e| format!("loopback did not bind: {e}"))?;
    if let Some(first) = &bound.first_error {
        log.line(&format!(
            "port {port} did not bind ({first}); serving on port {} instead",
            bound.port
        ));
        // WSAEACCES, which only Windows reports.
        if first.raw_os_error() == Some(10013) {
            log.line(daemon::wsaeacces_hint());
        }
    }
    let discovery = Discovery::new(bound.port, token.clone(), std::process::id());
    store
        .publish(&discovery)
        .map_err(|e| format!("the discovery file was not written: {e}"))?;

    let artifacts_dir = paths
        .artifacts()
        .map_err(|e| format!("the artifacts directory is not resolvable: {e}"))?;
    let env = pemu_host::assets::HostEnv::from_paths(paths);
    let corpus_env = env.clone();
    let audio = pemu_host::audio_root::AudioRoot::from_paths(paths)
        .map_err(|e| format!("the data root is not resolvable: {e}"))?;
    // The demo this binary ships is a firmware source, so a clean host with no corpus boots it by
    // name and a `start` with no `fw` boots it. It is the same lookup the web UI's firmware route
    // uses.
    let payload = crate::payload::this_process();
    pemu_host::backend::install(
        pemu_host::backend::product_source_with_bundles(
            env.clone(),
            crate::payload::firmware_bundles(payload.clone()),
        ),
        paths::artifacts_root(paths),
        audio,
    );
    // A daemon shares no working directory with its callers, so scenario paths must arrive
    // absolute; it reads them under its config role and the workspace a forwarding CLI names per
    // request.
    pemu_host::hooks::install(
        pemu_host::hooks::HostHooks::product(
            env,
            None,
            paths.config().ok().into_iter().collect(),
            paths.config().ok(),
        )
        .with_payload_bundles(crate::payload::firmware_bundles(payload.clone())),
    );
    pemu_host::boot_cache::install(paths.cache().ok());
    // The daemon answers the Device group as an agent caller: `erase_nvs`, `backup` and `no_backup`
    // stay out of reach, and a flash still needs a person. Not being interactive, it asks through
    // the native dialog; where none can be raised, `availability()` reports `Unavailable` and the
    // run ends `E_PLAN_REFUSED` before the port is opened. MCP elicitation is not wired into the
    // planner hook.
    let allow_device = matches.get_flag(tree::ALLOW_DEVICE_ARG);
    if allow_device && let Ok(data_root) = paths.data_root() {
        pemu_api::commands::plan_flash::install(
            pemu_api::commands::plan_flash::DeviceCaller::Agent,
            Box::new(pemu_host::device::HostPlanner::new(
                &data_root,
                &crate::device_repository(),
                matches
                    .get_one::<String>(tree::ESPTOOL_ARG)
                    .map(String::as_str),
            )),
        );
    }
    let max = matches
        .get_one::<usize>(tree::MAX_INSTANCES_ARG)
        .copied()
        .unwrap_or_else(Pool::default_max_instances);
    let shutdown = Shutdown::new();
    // The static web UI from this binary's payload, and `/<corpus-id>.pebundle` built on request
    // from the corpus map. A development build installs the seam anyway and answers 404, and
    // `describe` says why in one line that names no host path.
    log.line(&crate::payload::Payload::describe(&payload));
    // The corpus first and the shipped bundle second, so an owner's own image wins and a host with
    // no corpus still boots the demo. Neither half ever joins a request to a path.
    let from_corpus = pemu_host::webui::corpus_bundles(corpus_env);
    let from_payload = crate::payload::payload_bundles(payload.clone());
    let web_ui = pemu_host::webui::WebUi::new(
        crate::payload::web_assets(payload),
        Box::new(move |id: &str| from_corpus(id).or_else(|| from_payload(id))),
    );
    let server = Arc::new(
        Server::new(
            Auth::new(token.clone(), bound.port),
            Arc::new(Pool::new(max)),
            shutdown.clone(),
            forward::daemon_groups_with_device(allow_device),
        )
        .with_mcp_caps(&caps)
        .with_artifacts(&artifacts_dir)
        .with_web_ui(web_ui),
    );
    match daemon::watch_signals(pemu_host::platform::signals(), shutdown) {
        Ok(signals) => log.line(&format!("stops on {signals:?}")),
        Err(e) => log.line(&format!(
            "no signal handler ({e}); `serve --stop` still works"
        )),
    }
    let token_path = paths::redact(
        &store.dir().join(pemu_host::auth::TOKEN_FILE),
        paths.home().ok().as_deref(),
    );
    let groups: Vec<&str> = caps.iter().map(|g| g.caps_name()).collect();
    log.line(&format!(
        "pid {} on http://127.0.0.1:{} mcp caps {} run {}",
        std::process::id(),
        bound.port,
        groups.join(","),
        server.run_id()
    ));
    if !headless {
        // The launch code is 32 bytes of OS entropy in the URL fragment, so it never reaches the
        // daemon's request line, its log or a `Referer`. It is single use and lives 60 seconds, and
        // it is written to this terminal only: the headless daemon prints no UI line.
        let ui = match server.mint_launch_code() {
            Ok(code) => format!("ui:    http://127.0.0.1:{}/#lc={code}\n", bound.port),
            Err(e) => format!("ui:    no launch code ({e}); the UI cannot be opened\n"),
        };
        render::write(
            &format!(
                "serving on http://127.0.0.1:{}\ntoken: {token_path}\n{ui}",
                bound.port
            ),
            Stream::Stdout,
        );
    }
    let served = pemu_host::http::serve_blocking(bound.listener, server);
    // Removed only while it still names this daemon, so a later daemon keeps its own.
    if let Ok(Some(current)) = store.read()
        && current == discovery
    {
        let _ = store.remove();
    }
    // A token file left behind would name a key nothing answers to. Same "still this daemon's"
    // rule.
    let _ = store.remove_token_if(&token);
    match served {
        Ok(()) => {
            log.line("stopped; every instance was stopped and its artifacts flushed");
            Ok(())
        }
        Err(e) => Err(format!("the server ended with an error: {e}")),
    }
}

/// The authenticated `POST /v1/shutdown`, then a wait until the port stops answering. Stopping a
/// daemon that is not running is not an error.
fn stop(store: &DiscoveryStore<'_>) -> Outcome {
    let state = match daemon::resolve(store, &HttpProbe) {
        Ok(state) => state,
        Err(e) => return forward::infra(&format!("the discovery file: {e}"), Mode::Text),
    };
    let text = match state {
        DaemonState::Running(discovery) => {
            if let Err(e) = daemon::stop(&discovery) {
                return forward::infra(&format!("the shutdown request failed: {e}"), Mode::Text);
            }
            if !daemon::wait_gone(&discovery, &HttpProbe, STOP_WAIT) {
                return forward::infra(
                    &format!(
                        "the daemon on port {} accepted the shutdown but still answers",
                        discovery.port
                    ),
                    Mode::Text,
                );
            }
            format!("stopped the daemon on port {}\n", discovery.port)
        }
        DaemonState::Stale(_) => {
            let _ = store.remove();
            "no daemon is running (a stale discovery file was removed)\n".to_string()
        }
        DaemonState::NotRunning => "no daemon is running\n".to_string(),
    };
    Outcome {
        code: exit::PASS,
        stdout: text,
        stderr: String::new(),
    }
}

/// A missing file is created owner-only first ([`OwnerOnlyFiles::create_file`]); an existing one
/// must pass the owner-only check and is appended to, never truncated.
fn open_log(paths: &HostPaths) -> Option<std::fs::File> {
    let dir = paths.logs().ok()?;
    let files = OwnerOnlyFiles::host();
    files.create_dir(&dir).ok()?;
    let path = dir.join(LOG_FILE);
    if !path.exists() {
        drop(files.create_file(&path).ok()?);
    }
    files.check(&path).ok()?;
    std::fs::OpenOptions::new().append(true).open(&path).ok()
}

/// Exit 2 on stderr.
pub fn usage(why: &str) -> Outcome {
    Outcome {
        code: exit::USAGE,
        stdout: String::new(),
        stderr: format!("error: E_USAGE: {why}\n"),
    }
}
