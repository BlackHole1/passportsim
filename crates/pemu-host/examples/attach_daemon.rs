//! The daemon `web/tests/attach.spec.ts` attaches a page to.
//!
//! Not `passportsim serve`: that serves the web UI only from an embedded package payload, which a
//! development build lacks, and the attach socket needs the page served by the same daemon (its
//! session cookie and `Origin`). This is [`pemu_host::http::Server`] with the static seam filled
//! from a directory on disk instead.
//!
//! ```text
//! cargo run -p pemu-host --example attach_daemon -- --web <web/dist> --core <pemu_wasm.wasm> \
//!     [--bundle <id>=<file.pebundle>]...
//! ```
//!
//! It prints one JSON line, `{"port", "token", "launch"}`, and serves until stdin closes or
//! `POST /v1/shutdown` arrives. The token is printed because the test is the MCP client and has
//! no runtime directory; it never reaches a file.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use pemu_api::spec::CapsGroup;
use pemu_host::auth::{Auth, Token};
use pemu_host::daemon::Shutdown;
use pemu_host::http::Server;
use pemu_host::pool::Pool;
use pemu_host::webui::{Asset, WebUi};

/// The file name the page fetches the wasm core under (`web/src/worker/worker.ts` `CORE_FILE`).
const CORE_FILE: &str = "pemu_wasm.wasm";

fn main() {
    if let Err(why) = run() {
        eprintln!("attach_daemon: {why}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let mut web: Option<PathBuf> = None;
    let mut core: Option<PathBuf> = None;
    let mut bundles: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("`{flag}` needs a value"))?;
        match flag.as_str() {
            "--web" => web = Some(PathBuf::from(value)),
            "--core" => core = Some(PathBuf::from(value)),
            "--bundle" => {
                let (id, path) = value
                    .split_once('=')
                    .ok_or("`--bundle` takes `<id>=<file>`")?;
                bundles.insert(id.to_string(), PathBuf::from(path));
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    let web = web.ok_or("`--web <dir>` is required")?;
    let core = std::fs::read(core.ok_or("`--core <file>` is required")?)
        .map_err(|e| format!("the wasm core: {e}"))?;
    let bundles: BTreeMap<String, Vec<u8>> = bundles
        .into_iter()
        .map(|(id, path)| {
            std::fs::read(&path)
                .map(|bytes| (id.clone(), bytes))
                .map_err(|e| format!("bundle `{id}`: {e}"))
        })
        .collect::<Result<_, _>>()?;

    let token = Token::generate().map_err(|e| format!("no token: {e}"))?;
    let bound = pemu_host::daemon::bind(0).map_err(|e| format!("bind: {e}"))?;
    let port = bound.port;
    let shutdown = Shutdown::new();
    // `WebUi::asset` has already refused any name that is not one safe segment.
    let assets = Box::new(move |name: &str| {
        let bytes = if name == CORE_FILE {
            core.clone()
        } else {
            std::fs::read(web.join(name)).ok()?
        };
        Some(Asset {
            content_type: pemu_host::webui::content_type(name),
            bytes,
        })
    });
    let server = Arc::new(
        Server::new(
            Auth::new(token.clone(), port),
            Arc::new(Pool::new(4)),
            shutdown.clone(),
            BTreeSet::from([CapsGroup::Core]),
        )
        .with_web_ui(WebUi::new(
            assets,
            Box::new(move |id: &str| bundles.get(id).cloned()),
        )),
    );
    let code = server
        .mint_launch_code()
        .map_err(|e| format!("no launch code: {e}"))?;
    let line = serde_json::json!({
        "port": port,
        "token": token.to_hex(),
        "launch": format!("http://127.0.0.1:{port}/#lc={code}"),
    });
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{line}").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    // The test's end of the pipe closing is the stop signal, so a killed test leaves no daemon.
    let watcher = shutdown.clone();
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
        watcher.request();
    });
    pemu_host::http::serve_blocking(bound.listener, server).map_err(|e| format!("serve: {e}"))
}
