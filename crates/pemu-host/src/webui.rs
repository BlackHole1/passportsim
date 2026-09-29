//! The static half of `serve`: the web UI asset seam, the `.pebundle` built on request from a
//! corpus id, the UI shell, and the launch-code session.
//!
//! No request contributes a path component: an asset name must pass [`safe_name`], and a bundle
//! id is matched against the ids the corpus map resolves ([`corpus_bundles`]).
//!
//! A static `GET` takes the bearer token or the session cookie minted here. The cookie is minted
//! only for a launch code: 32 bytes of OS entropy, single use, good for [`LAUNCH_CODE_TTL`], never
//! written to a file or logged, and passed in the URL fragment, which browsers never send. The
//! only credential-free document is [`shell`], which redeems that fragment. Both secrets live in
//! [`Sessions`], in daemon memory only.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pemu_api::error::{ApiError, E_ASSET_MISSING, E_USAGE};

use crate::auth::TokenError;
use crate::http::Response;

pub const SECRET_BYTES: usize = 32;

pub const LAUNCH_CODE_TTL: Duration = Duration::from_secs(60);

pub const SESSION_COOKIE: &str = "pemu_session";

/// The fragment key the CLI writes the launch code under: `http://127.0.0.1:<port>/#lc=<code>`.
pub const LAUNCH_FRAGMENT_KEY: &str = "lc";

/// The suffix of the firmware route, `GET /<corpus-id>.pebundle`.
pub const BUNDLE_SUFFIX: &str = ".pebundle";

/// Where the shell sends the browser once a session exists. Like every asset except [`shell`], it
/// needs a credential.
pub const UI_DOCUMENT: &str = "/index.html";

/// The longest asset name served, so a pathological URL is refused before any lookup.
const MAX_NAME: usize = 64;

/// How many launch codes may be pending at once. Every page reload mints a session, so at the
/// bound the oldest is dropped rather than the daemon growing.
const MAX_PENDING: usize = 16;
/// How many sessions may be live; see [`MAX_PENDING`].
const MAX_SESSIONS: usize = 64;

/// The two headers that make a document cross-origin isolated, so the page may use
/// SharedArrayBuffer.
pub const ISOLATION_HEADERS: [(&str, &str); 2] = [
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-embedder-policy", "require-corp"),
];

/// The headers every static body carries apart from `cache-control`. `xtask package` copies this
/// list into the bundle's Cloudflare `_headers` file for hosts that serve the UI elsewhere.
pub const STATIC_HEADERS: [(&str, &str); 4] = [
    ISOLATION_HEADERS[0],
    ISOLATION_HEADERS[1],
    ("cross-origin-resource-policy", "same-origin"),
    ("x-content-type-options", "nosniff"),
];

pub const OCTET_STREAM: &str = "application/octet-stream";

#[derive(Clone, PartialEq, Eq)]
pub struct Asset {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Asset {
    /// Prints the type and the length, never the bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Asset")
            .field("content_type", &self.content_type)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// The host's map from an asset name to its bytes (the binary embeds the web UI).
pub type AssetFn = Box<dyn Fn(&str) -> Option<Asset> + Send + Sync>;

pub type BundleFn = Box<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

/// What the daemon serves and where the bytes come from: the host installs an asset map over its
/// embedded payload and a bundle builder over the corpus map. Without one, every static path is
/// 404, as in a development build with no payload.
pub struct WebUi {
    assets: AssetFn,
    bundles: BundleFn,
}

impl fmt::Debug for WebUi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebUi { .. }")
    }
}

impl WebUi {
    pub fn new(assets: AssetFn, bundles: BundleFn) -> WebUi {
        WebUi { assets, bundles }
    }

    /// The bytes of one asset. `name` has already passed [`safe_name`].
    pub fn asset(&self, name: &str) -> Option<Asset> {
        safe_name(name)?;
        (self.assets)(name)
    }

    /// The `.pebundle` of one corpus id, built on request. `id` has already passed [`safe_name`].
    pub fn bundle(&self, id: &str) -> Option<Vec<u8>> {
        safe_name(id)?;
        (self.bundles)(id)
    }
}

/// `name` when it is one servable file name: a whitelist of one segment of ASCII letters, digits,
/// `.`, `_` and `-`, at most [`MAX_NAME`] long, with no leading dot and no `..`. Nothing that
/// passes can name a directory, a parent or a hidden file.
pub fn safe_name(name: &str) -> Option<&str> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
    ok.then_some(name)
}

/// The media type of an asset name, by extension. An unknown one is [`OCTET_STREAM`], and with
/// `nosniff` the browser never guesses another.
pub fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/vnd.microsoft.icon",
        Some("woff2") => "font/woff2",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        _ => OCTET_STREAM,
    }
}

/// Adds [`STATIC_HEADERS`] and `no-store`: the shell and bundle are built per request, and a cached
/// UI file from a daemon that has since restarted is one no session can re-fetch.
fn static_headers(response: Response) -> Response {
    let mut response = response.header("cache-control", "no-store");
    for (name, value) in STATIC_HEADERS {
        response = response.header(name, value);
    }
    response
}

pub fn static_response(content_type: &str, bytes: Vec<u8>) -> Response {
    static_headers(Response::new(200, content_type, bytes))
}

/// The 404 of an unknown file or corpus id. It names nothing, so a caller learns only that this
/// daemon does not serve it.
pub fn not_served() -> ApiError {
    ApiError::new(E_ASSET_MISSING, "this daemon serves no such file").with_hint(
        "the static routes of `serve` are the web UI files this build carries and \
         `/<corpus-id>.pebundle` for a corpus id this host resolves",
    )
}

/// The 401 of a static `GET` with no credential or a refused launch code. Used, expired, unknown
/// and missing codes all answer the same way.
pub fn no_session() -> Response {
    let error = ApiError::new(E_USAGE, "not authorized").with_hint(
        "a static request needs the daemon token as `Authorization: Bearer <token>`, or the \
         session cookie the UI gets when the launch code the CLI minted is redeemed; a launch \
         code is single use and expires 60 seconds after it is minted",
    );
    Response::json(
        401,
        &serde_json::json!({ "ok": false, "result": serde_json::Value::Null, "error": error.to_json() }),
    )
    .header("cache-control", "no-store")
}

/// The UI shell, the one document served without a credential. It carries no instance state,
/// corpus id, host path or token. Its script is inline because an external one would be a second
/// credential-free fetch. The script clears the code from the address bar first, posts it to
/// `/v1/session`, and on a 200 replaces itself with the UI.
pub fn shell() -> Response {
    static_headers(Response::new(200, content_type("shell.html"), SHELL_HTML))
}

const SHELL_HTML: &str = r##"<!doctype html>
<!-- The UI shell: it redeems a launch code and loads nothing else. -->
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>PassportSim</title>
  </head>
  <body>
    <p id="state">Starting the emulator UI.</p>
    <script>
      (function () {
        var state = document.getElementById("state");
        var hash = window.location.hash || "";
        var code = hash.indexOf("#lc=") === 0 ? hash.slice(4) : "";
        // Only the page's own preferences (`?mode=advanced`, `?lang=ja`) follow the code to the
        // UI; nothing else in the query does.
        var keep = [];
        window.location.search.slice(1).split("&").forEach(function (pair) {
          if (/^(mode=(simple|advanced)|lang=[A-Za-z-]{2,10})$/.test(pair)) {
            keep.push(pair);
          }
        });
        var target = "/index.html" + (keep.length ? "?" + keep.join("&") : "");
        // Before anything else: the address bar, this history entry and every later `Referer`
        // lose the code.
        window.history.replaceState(null, "", window.location.pathname);
        if (!code) {
          state.textContent =
            "This page needs a launch code. Open the emulator UI from the command line: a link opened by hand carries none.";
          return;
        }
        window
          .fetch("/v1/session", {
            method: "POST",
            credentials: "same-origin",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ code: code }),
          })
          .then(function (response) {
            if (response.ok) {
              window.location.replace(target);
              return;
            }
            state.textContent =
              "This launch code was refused: a code is single use and lasts 60 seconds. Open the UI from the command line again.";
          })
          .catch(function () {
            state.textContent = "The emulator daemon did not answer.";
          });
      })();
    </script>
  </body>
</html>
"##;

/// A 32-byte secret: a launch code or a session cookie value. The bytes never reach `Debug`, a log
/// or an error body, and [`Secret::matches`] compares in constant time.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret([u8; SECRET_BYTES]);

impl Secret {
    /// A fresh secret from the OS generator, never derived from the daemon token.
    pub fn generate() -> Result<Secret, TokenError> {
        let mut bytes = [0u8; SECRET_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| TokenError::NoEntropy)?;
        Ok(Secret(bytes))
    }

    pub fn from_bytes(bytes: [u8; SECRET_BYTES]) -> Secret {
        Secret(bytes)
    }

    /// Unpadded base64url, safe in a URL fragment and a cookie without encoding.
    pub fn to_base64url(&self) -> String {
        base64url(&self.0)
    }

    /// Constant-time comparison with the offered text; wrong length or alphabet is a mismatch.
    pub fn matches(&self, offered: &str) -> bool {
        let want = self.to_base64url();
        let mut diff = u8::from(want.len() != offered.len());
        for (a, b) in want.bytes().zip(offered.bytes()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// base64url with no padding (RFC 4648 section 5).
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let word = (b0 << 16) | (b1 << 8) | b2;
        let digits = chunk.len() + 1;
        for i in 0..digits {
            let index = (word >> (18 - 6 * i)) & 0x3f;
            out.push(char::from(ALPHABET[index as usize]));
        }
    }
    out
}

/// Pending launch codes and live sessions, in memory only: a session survives a page reload and
/// dies with the daemon.
#[derive(Debug)]
pub struct Sessions {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    codes: VecDeque<(Secret, Instant)>,
    cookies: VecDeque<Secret>,
}

impl Default for Sessions {
    fn default() -> Sessions {
        Sessions::new()
    }
}

impl Sessions {
    pub fn new() -> Sessions {
        Sessions {
            state: Mutex::new(State::default()),
        }
    }

    /// Mints a launch code and returns its text, the only time that text exists outside this
    /// store. The caller puts it in the URL fragment and nowhere else.
    pub fn mint(&self) -> Result<String, TokenError> {
        self.mint_at(Instant::now())
    }

    pub fn mint_at(&self, now: Instant) -> Result<String, TokenError> {
        let secret = Secret::generate()?;
        let text = secret.to_base64url();
        let mut state = self.lock();
        state.codes.retain(|(_, until)| *until > now);
        while state.codes.len() >= MAX_PENDING {
            state.codes.pop_front();
        }
        state
            .codes
            .push_back((secret, now.checked_add(LAUNCH_CODE_TTL).unwrap_or(now)));
        Ok(text)
    }

    /// Redeems a launch code for a session cookie value. The code is removed before the cookie is
    /// minted, so two requests racing on one code cannot both win.
    pub fn redeem(&self, offered: &str) -> Option<String> {
        self.redeem_at(offered, Instant::now())
    }

    pub fn redeem_at(&self, offered: &str, now: Instant) -> Option<String> {
        let mut state = self.lock();
        state.codes.retain(|(_, until)| *until > now);
        let found = state
            .codes
            .iter()
            .position(|(code, _)| code.matches(offered))?;
        state.codes.remove(found);
        let secret = Secret::generate().ok()?;
        let text = secret.to_base64url();
        while state.cookies.len() >= MAX_SESSIONS {
            state.cookies.pop_front();
        }
        state.cookies.push_back(secret);
        Some(text)
    }

    pub fn holds(&self, offered: &str) -> bool {
        let state = self.lock();
        state.cookies.iter().any(|c| c.matches(offered))
    }

    pub fn pending(&self) -> usize {
        self.lock().codes.len()
    }

    /// Recovers a poisoned lock: refusing every request after one panicking handler would be
    /// worse than serving the sessions that exist.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The `Set-Cookie` value of a redeemed launch code. No `Secure`, because the origin is plain
/// `http://127.0.0.1` where a `Secure` cookie is never stored; `SameSite=Strict` and `HttpOnly`
/// keep foreign pages and page script off it.
pub fn set_cookie(value: &str) -> String {
    format!("{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict")
}

/// The value of one cookie of a `Cookie` header. A value this daemon minted holds no `;`, `=` or
/// space.
pub fn cookie_value<'a>(header: Option<&'a str>, name: &str) -> Option<&'a str> {
    header?.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.trim())
    })
}

/// The bundle half of the seam, over a host's corpus map. The id is compared with the ids
/// [`crate::assets::Corpus`] parsed and the bytes come from the map's paths. An unknown id, a
/// missing file and a digest mismatch are all `None`. The bundle carries `flash` and `app_elf`
/// under names derived from the id alone, as `xtask/src/package/demo.rs` writes them.
pub fn corpus_bundles(env: crate::assets::HostEnv) -> BundleFn {
    Box::new(move |id: &str| {
        let id = safe_name(id)?;
        let corpus = crate::assets::Corpus::load(&env).ok()?;
        corpus.get(id)?;
        let flash = corpus.read(id, pemu_loader::bundle::CORPUS_BIN).ok()?;
        let elf = corpus.read(id, pemu_loader::bundle::CORPUS_ELF).ok();
        let flash_name = format!("{id}.bin");
        let elf_name = format!("{id}.elf");
        let mut files = vec![pemu_loader::bundle::BundleInput {
            role: pemu_loader::bundle::BUNDLE_FLASH,
            name: &flash_name,
            bytes: &flash,
        }];
        if let Some(elf) = elf.as_deref() {
            files.push(pemu_loader::bundle::BundleInput {
                role: pemu_loader::bundle::BUNDLE_APP_ELF,
                name: &elf_name,
                bytes: elf,
            });
        }
        Some(pemu_loader::bundle::build(Some(id), None, &files))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_one_plain_segment_and_every_escape_is_refused() {
        for good in [
            "index.html",
            "main.js",
            "pemu_wasm.wasm",
            "official.pebundle",
            "a-b.c",
        ] {
            assert_eq!(safe_name(good), Some(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "../secrets",
            "a/b",
            "a\\b",
            "%2e%2e",
            "a..b",
            "a b",
            "a\0b",
            "ünicode.js",
            "~/token",
            "a:b",
        ] {
            assert_eq!(safe_name(bad), None, "{bad} must be refused");
        }
        assert_eq!(
            safe_name(&"a".repeat(MAX_NAME)),
            Some("a".repeat(MAX_NAME).as_str())
        );
        assert_eq!(safe_name(&"a".repeat(MAX_NAME + 1)), None);
    }

    #[test]
    fn every_known_extension_has_its_own_type_and_the_rest_are_octets() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("main.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("styles.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("pemu_wasm.wasm"), "application/wasm");
        assert_eq!(content_type("official.pebundle"), OCTET_STREAM);
        assert_eq!(content_type("no-extension"), OCTET_STREAM);
    }

    #[test]
    fn base64url_matches_rfc_4648_section_5_with_no_padding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        // No `+` and no `/`: safe in a URL fragment and a cookie value unencoded.
        assert_eq!(base64url(&[0xfb, 0xff, 0xbe]), "-_--");
        let text = Secret::from_bytes([0x7f; SECRET_BYTES]).to_base64url();
        assert_eq!(text.len(), 43, "32 bytes are 43 base64url characters");
        assert!(
            text.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn a_secret_prints_no_byte_of_itself_and_compares_whole() {
        let secret = Secret::from_bytes([0x11; SECRET_BYTES]);
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert!(secret.matches(&secret.to_base64url()));
        assert!(!secret.matches(""));
        assert!(!secret.matches(&secret.to_base64url()[..42]));
        let mut other = [0x11; SECRET_BYTES];
        other[SECRET_BYTES - 1] = 0x12;
        assert!(!secret.matches(&Secret::from_bytes(other).to_base64url()));
        assert_ne!(
            Secret::generate().expect("entropy").to_base64url(),
            Secret::generate().expect("entropy").to_base64url()
        );
    }

    #[test]
    fn a_launch_code_is_redeemed_exactly_once() {
        let sessions = Sessions::new();
        let code = sessions.mint().expect("entropy");
        assert_eq!(sessions.pending(), 1);
        let cookie = sessions.redeem(&code).expect("the code is good once");
        assert!(sessions.holds(&cookie));
        assert_eq!(sessions.pending(), 0);
        assert_eq!(sessions.redeem(&code), None, "a code is single use");
    }

    #[test]
    fn a_code_older_than_the_ttl_is_gone() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let old = now
            .checked_sub(LAUNCH_CODE_TTL + Duration::from_secs(1))
            .expect("the clock has run for a minute");
        let code = sessions.mint_at(old).expect("entropy");
        assert_eq!(
            sessions.redeem_at(&code, now),
            None,
            "60 s is the whole life of a code"
        );
        let edge = sessions
            .mint_at(now.checked_sub(LAUNCH_CODE_TTL).expect("clock"))
            .expect("entropy");
        assert_eq!(sessions.redeem_at(&edge, now), None);
        let fresh = sessions.mint_at(now).expect("entropy");
        assert!(sessions.redeem_at(&fresh, now).is_some());
    }

    #[test]
    fn a_code_of_one_store_is_not_a_code_of_another() {
        // The store is the daemon instance: nothing a second daemon could reproduce.
        let a = Sessions::new();
        let b = Sessions::new();
        let code = a.mint().expect("entropy");
        assert_eq!(b.redeem(&code), None);
        let cookie = a.redeem(&code).expect("its own store");
        assert!(!b.holds(&cookie));
    }

    #[test]
    fn a_cookie_header_is_read_pair_by_pair() {
        assert_eq!(cookie_value(None, SESSION_COOKIE), None);
        assert_eq!(
            cookie_value(Some("pemu_session=abc"), SESSION_COOKIE),
            Some("abc")
        );
        assert_eq!(
            cookie_value(Some("other=1; pemu_session=abc; third=2"), SESSION_COOKIE),
            Some("abc")
        );
        assert_eq!(
            cookie_value(Some("pemu_sessionx=abc"), SESSION_COOKIE),
            None
        );
        assert_eq!(cookie_value(Some("nonsense"), SESSION_COOKIE), None);
    }

    #[test]
    fn the_cookie_attributes_are_the_ones_the_adr_fixes() {
        let cookie = set_cookie("value");
        assert_eq!(
            cookie,
            "pemu_session=value; Path=/; HttpOnly; SameSite=Strict"
        );
        assert!(
            !cookie.contains("Secure"),
            "the origin is plain http://127.0.0.1"
        );
        assert!(!cookie.to_lowercase().contains("samesite=lax"));
    }

    #[test]
    fn the_shell_carries_no_state_no_path_and_no_second_fetch() {
        let shell = shell();
        assert_eq!(shell.status, 200);
        assert_eq!(shell.get("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(shell.get("cross-origin-opener-policy"), Some("same-origin"));
        assert_eq!(
            shell.get("cross-origin-embedder-policy"),
            Some("require-corp")
        );
        let text = shell.text();
        for forbidden in [
            "official", "pebundle", "token", "Bearer", "/Users/", "corpus", "p1",
        ] {
            assert!(
                !text.contains(forbidden),
                "the shell must not name `{forbidden}`"
            );
        }
        assert!(
            !text.contains("<script src"),
            "the shell loads no external script"
        );
        assert!(
            !text.contains("<link"),
            "the shell loads no external stylesheet"
        );
        assert!(
            text.contains("history.replaceState"),
            "the code leaves the address bar"
        );
        assert!(text.contains("/v1/session"));
    }

    /// A corpus map written for this test, so nothing reads the host's real corpus.
    fn corpus_env(dir: &std::path::Path, bin: &[u8], elf: &[u8]) -> crate::assets::HostEnv {
        let bin_path = dir.join("image.bin");
        let elf_path = dir.join("app.elf");
        std::fs::write(&bin_path, bin).expect("the temporary directory is writable");
        std::fs::write(&elf_path, elf).expect("the temporary directory is writable");
        let text = format!(
            "[demo]\nbin = \"{}\"\nelf = \"{}\"\nsha256 = {{ bin = \"{}\", elf = \"{}\" }}\n",
            bin_path.display(),
            elf_path.display(),
            pemu_loader::hex(&pemu_loader::sha256(bin)),
            pemu_loader::hex(&pemu_loader::sha256(elf)),
        );
        std::fs::write(dir.join("corpus.toml"), text).expect("the temporary directory is writable");
        crate::assets::HostEnv {
            config_dir: dir.to_path_buf(),
            ..crate::assets::HostEnv::default()
        }
    }

    #[test]
    fn a_bundle_is_built_from_the_map_and_an_unresolvable_id_is_nothing() {
        let dir = std::env::temp_dir().join(format!("pemu-webui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        let bundles = corpus_bundles(corpus_env(&dir, b"flash bytes", b"elf bytes"));
        let bytes = bundles("demo").expect("the map resolves `demo`");
        let bundle = pemu_loader::bundle::Bundle::parse(&bytes).expect("a bundle it wrote itself");
        assert_eq!(bundle.id(), Some("demo"));
        assert_eq!(
            bundle.role_data(pemu_loader::bundle::BUNDLE_FLASH),
            Some(&b"flash bytes"[..])
        );
        assert_eq!(
            bundle.role_data(pemu_loader::bundle::BUNDLE_APP_ELF),
            Some(&b"elf bytes"[..])
        );
        let names: Vec<&str> = bundle.files().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["demo.bin", "demo.elf"]);
        for id in ["official", "..", "../demo", "demo/../demo", "%2e%2e", ""] {
            assert!(bundles(id).is_none(), "{id} must resolve to nothing");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_seam_refuses_a_name_before_the_host_function_sees_it() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let asset_log = std::sync::Arc::clone(&seen);
        let bundle_log = std::sync::Arc::clone(&seen);
        let ui = WebUi::new(
            Box::new(move |name| {
                asset_log.lock().expect("no panic").push(name.to_owned());
                None
            }),
            Box::new(move |id| {
                bundle_log.lock().expect("no panic").push(id.to_owned());
                None
            }),
        );
        for bad in ["..", "../etc", "a/b", ".hidden"] {
            assert!(ui.asset(bad).is_none());
            assert!(ui.bundle(bad).is_none());
        }
        assert!(
            seen.lock().expect("no panic").is_empty(),
            "no unsafe name reaches the host's function"
        );
        assert!(ui.asset("index.html").is_none());
        assert_eq!(&*seen.lock().expect("no panic"), &["index.html".to_owned()]);
    }
}
