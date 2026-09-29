//! The Cloudflare Workers project files of the web bundle (`docs/deploy-cloudflare.md`).
//!
//! The bundle directory is itself the project of an assets-only Worker: `_headers` carries the
//! headers `passportsim serve` sends ([`webui::STATIC_HEADERS`]) and `.assetsignore` keeps the
//! project file and `wrangler dev` state off the site. The demo `.pebundle` is close to
//! [`MAX_ASSET_BYTES`], so [`check`] fails the package before a deploy would.

use std::fs;
use std::path::Path;

use pemu_host::webui;

pub const CONFIG_FILE: &str = "wrangler.jsonc";

/// The response header rules of Workers static assets.
pub const HEADERS_FILE: &str = "_headers";

pub const IGNORE_FILE: &str = ".assetsignore";

/// The directory `wrangler dev` keeps its local state in, beside the project file.
const STATE_DIR: &str = ".wrangler";

/// Top-level names of the bundle that are not assets. Wrangler skips `.assetsignore`, `_headers`
/// and `_redirects` by itself; the other two are listed in [`IGNORE_FILE`], because an assets
/// directory of `.` would otherwise publish the project file and the local state.
const NOT_ASSETS: [&str; 5] = [
    IGNORE_FILE,
    HEADERS_FILE,
    "_redirects",
    CONFIG_FILE,
    STATE_DIR,
];

/// The largest file one Workers version may carry, on the Free and the Paid plan alike
/// (<https://developers.cloudflare.com/workers/platform/limits/#static-assets>). Wrangler refuses
/// a larger one at deploy.
pub const MAX_ASSET_BYTES: u64 = 25 * 1024 * 1024;

/// The most files one Workers version may carry on the Free plan; the Paid plan allows 100,000.
pub const MAX_ASSET_FILES: usize = 20_000;

/// The most rules a `_headers` file may hold
/// (<https://developers.cloudflare.com/workers/static-assets/headers/>).
pub const MAX_HEADER_RULES: usize = 100;

/// The longest line a `_headers` file may hold.
pub const MAX_HEADER_LINE: usize = 2_000;

/// The Worker's name, which is also its `workers.dev` subdomain; `wrangler deploy --name`
/// overrides it.
const WORKER_NAME: &str = "passportsim";

/// The runtime behaviour the Worker is pinned to. An assets-only Worker runs no code, so this
/// only has to be a date the Workers runtime accepts.
const COMPATIBILITY_DATE: &str = "2026-09-01";

/// Type of the licence texts, which have no extension a browser or Wrangler can type by: they are
/// there to be read. The installers get it too, so a browser shows them instead of downloading
/// them.
const TEXT_TYPE: &str = "text/plain; charset=utf-8";

pub fn config_text() -> String {
    format!(
        "// The Cloudflare Workers project of this web bundle, written by `cargo xtask package`.\n\
         // Deploy from this directory with `bunx wrangler deploy` (docs/deploy-cloudflare.md).\n\
         {{\n  \
           \"name\": \"{WORKER_NAME}\",\n  \
           \"compatibility_date\": \"{COMPATIBILITY_DATE}\",\n  \
           // Kept on when a deploy names a custom domain, which would otherwise turn it off.\n  \
           \"workers_dev\": true,\n  \
           // Assets only: no Worker script, so every request is a free static asset request.\n  \
           \"assets\": {{ \"directory\": \".\" }}\n\
         }}\n"
    )
}

/// The text of `_headers`: the daemon's static headers on every path, and a type for the files
/// Wrangler leaves untyped (a `.pebundle`), which with `nosniff` a browser may refuse to read.
pub fn headers_text() -> String {
    let mut text = String::from(
        "# The headers `passportsim serve` sends with every web UI file, written by\n\
         # `cargo xtask package`: the page needs the isolation pair for SharedArrayBuffer.\n\
         /*\n",
    );
    for (name, value) in webui::STATIC_HEADERS {
        text.push_str(&format!("  {name}: {value}\n"));
    }
    for (pattern, content_type) in [
        ("/*.pebundle", webui::content_type(super::demo::BUNDLE_FILE)),
        ("/LICENSE", TEXT_TYPE),
        ("/licenses/*", TEXT_TYPE),
        ("/install.sh", TEXT_TYPE),
        ("/install.ps1", TEXT_TYPE),
    ] {
        text.push_str(&format!("\n{pattern}\n  content-type: {content_type}\n"));
    }
    text
}

pub fn ignore_text() -> String {
    format!(
        "# Not assets: the Wrangler project file and the state `wrangler dev` keeps beside it.\n\
         /{CONFIG_FILE}\n\
         /{STATE_DIR}/\n"
    )
}

pub fn write(dir: &Path) -> Result<(), String> {
    let headers = headers_text();
    headers_fit(&headers)?;
    for (name, text) in [
        (CONFIG_FILE, config_text()),
        (HEADERS_FILE, headers),
        (IGNORE_FILE, ignore_text()),
    ] {
        let path = dir.join(name);
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Whether a `_headers` text is within [`MAX_HEADER_RULES`] and [`MAX_HEADER_LINE`]; Workers
/// ignores what is past either, which would drop a header silently rather than fail.
fn headers_fit(text: &str) -> Result<(), String> {
    let rules = text.lines().filter(|line| line.starts_with('/')).count();
    if rules > MAX_HEADER_RULES {
        return Err(format!(
            "{HEADERS_FILE} holds {rules} rules, over the {MAX_HEADER_RULES} Workers reads \
             (https://developers.cloudflare.com/workers/static-assets/headers/)"
        ));
    }
    if let Some(line) = text.lines().find(|line| line.len() > MAX_HEADER_LINE) {
        return Err(format!(
            "{HEADERS_FILE} has a line of {} characters, over the {MAX_HEADER_LINE} Workers reads: \
             `{line}`",
            line.len()
        ));
    }
    Ok(())
}

/// What a deploy of the bundle would upload, measured against the Workers limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assets {
    pub files: usize,
    /// The largest of them, `/`-separated relative to the bundle.
    pub largest: String,
    pub largest_bytes: u64,
}

impl Assets {
    /// One line for the package log: the largest asset and what is left under the limit.
    pub fn summary(&self) -> String {
        format!(
            "{} assets for Cloudflare Workers, the largest `{}` at {} of {MAX_ASSET_BYTES} bytes \
             ({} to spare)",
            self.files,
            self.largest,
            self.largest_bytes,
            MAX_ASSET_BYTES - self.largest_bytes
        )
    }
}

/// Every file a deploy of `dir` would upload. A file over [`MAX_ASSET_BYTES`] or a count over
/// [`MAX_ASSET_FILES`] fails with its name, its size and the limit.
pub fn check(dir: &Path) -> Result<Assets, String> {
    let mut files = Vec::new();
    walk(dir, dir, &mut files)?;
    let mut over: Vec<String> = files
        .iter()
        .filter(|(_, size)| *size > MAX_ASSET_BYTES)
        .map(|(path, size)| format!("`{path}` is {size} bytes"))
        .collect();
    over.sort();
    if !over.is_empty() {
        return Err(format!(
            "the web bundle cannot be deployed to Cloudflare Workers: {}, over the {MAX_ASSET_BYTES}-byte \
             (25 MiB) limit on one static asset \
             (https://developers.cloudflare.com/workers/platform/limits/#static-assets), which \
             `wrangler deploy` enforces; shrink the file or split it",
            over.join(", ")
        ));
    }
    if files.len() > MAX_ASSET_FILES {
        return Err(format!(
            "the web bundle cannot be deployed to Cloudflare Workers on the Free plan: {} files, \
             over the {MAX_ASSET_FILES}-file limit of one version \
             (https://developers.cloudflare.com/workers/platform/limits/#static-assets)",
            files.len()
        ));
    }
    let (largest, largest_bytes) = files
        .iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .cloned()
        .ok_or_else(|| format!("{} holds no asset", dir.display()))?;
    Ok(Assets {
        files: files.len(),
        largest,
        largest_bytes,
    })
}

/// Every file below `dir` a deploy would upload, relative to `base`, with its size.
fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, u64)>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(base)
            .map_err(|_| format!("{} is not below {}", path.display(), base.display()))?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if NOT_ASSETS.contains(&relative.as_str()) {
            continue;
        }
        let meta = entry
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            walk(base, &path, out)?;
        } else {
            out.push((relative, meta.len()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-cloudflare-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// A sparse file of `len` bytes, so a 25 MiB case costs no disk.
    fn sized(path: &Path, len: u64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::File::create(path)
            .and_then(|file| file.set_len(len))
            .expect("sized file");
    }

    /// The `(pattern, [(name, value)])` rules of a `_headers` text, `#` comments skipped.
    fn rules(text: &str) -> Vec<(String, Vec<(String, String)>)> {
        let mut rules: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            if let Some(header) = line.strip_prefix("  ") {
                let (name, value) = header.split_once(':').expect("`name: value`");
                rules
                    .last_mut()
                    .expect("a header follows a pattern")
                    .1
                    .push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            } else {
                rules.push((line.to_string(), Vec::new()));
            }
        }
        rules
    }

    #[test]
    fn every_path_gets_exactly_the_headers_the_daemon_sends() {
        let rules = rules(&headers_text());
        let all = rules
            .iter()
            .find(|(pattern, _)| pattern == "/*")
            .expect("a rule for every path");
        let mut written = all.1.clone();
        written.sort();
        let mut daemon: Vec<(String, String)> = webui::STATIC_HEADERS
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        daemon.sort();
        assert_eq!(written, daemon);
        assert!(
            written
                .iter()
                .any(|(name, value)| name == "x-content-type-options" && value == "nosniff"),
            "{written:?}"
        );
    }

    #[test]
    fn the_demo_bundle_is_typed_the_way_the_daemon_types_it() {
        let rules = rules(&headers_text());
        let pebundle = rules
            .iter()
            .find(|(pattern, _)| pattern == "/*.pebundle")
            .expect("a rule for the demo bundle");
        assert_eq!(
            pebundle.1,
            [(
                "content-type".to_string(),
                webui::content_type(super::super::demo::BUNDLE_FILE).to_string()
            )]
        );
        // The other rules only type files; none adds a header the daemon does not send.
        for (pattern, headers) in rules.iter().filter(|(pattern, _)| pattern != "/*") {
            assert!(
                headers.iter().all(|(name, _)| name == "content-type"),
                "{pattern}: {headers:?}"
            );
        }
    }

    #[test]
    fn the_installers_are_served_as_text() {
        let rules = rules(&headers_text());
        for name in super::super::layout::INSTALLERS {
            let pattern = format!("/{name}");
            let rule = rules
                .iter()
                .find(|(p, _)| *p == pattern)
                .unwrap_or_else(|| panic!("a rule for {pattern}"));
            assert_eq!(
                rule.1,
                [("content-type".to_string(), TEXT_TYPE.to_string())]
            );
        }
    }

    #[test]
    fn the_headers_file_is_within_the_workers_limits() {
        let text = headers_text();
        headers_fit(&text).expect("the written rules fit");
        assert_eq!(
            text.lines().filter(|line| line.starts_with('/')).count(),
            rules(&text).len(),
            "the limit counts every rule"
        );
        let many: String = (0..=MAX_HEADER_RULES)
            .map(|n| format!("/{n}\n  x-n: {n}\n"))
            .collect();
        assert!(headers_fit(&many).unwrap_err().contains("101 rules"));
        let long = format!("/*\n  x-long: {}\n", "a".repeat(MAX_HEADER_LINE));
        assert!(headers_fit(&long).unwrap_err().contains("line of"));
    }

    #[test]
    fn the_project_is_assets_only_with_the_bundle_as_its_directory() {
        let config = config_text();
        let json: String = config
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let value: serde_json::Value = serde_json::from_str(&json).expect("JSON once comments go");
        assert_eq!(value["assets"]["directory"], ".");
        assert_eq!(value["name"], WORKER_NAME);
        assert!(value["compatibility_date"].is_string());
        // Wrangler defaults `workers_dev` to off once a deploy names a route or custom domain.
        assert_eq!(value["workers_dev"], true);
        assert!(
            value.get("main").is_none(),
            "no Worker script: every request stays a free asset request"
        );
        let ignore = ignore_text();
        for name in [CONFIG_FILE, STATE_DIR] {
            assert!(
                ignore.lines().any(|line| line.trim_matches('/') == name),
                "{IGNORE_FILE} keeps `{name}` off the site: {ignore}"
            );
        }
    }

    #[test]
    fn a_file_at_the_limit_passes_and_one_byte_over_fails_naming_it() {
        let dir = scratch("limit");
        write(&dir).expect("project files");
        sized(&dir.join("index.html"), 100);
        sized(&dir.join("official.pebundle"), MAX_ASSET_BYTES);
        let assets = check(&dir).expect("a file of exactly 25 MiB deploys");
        assert_eq!(assets.files, 2, "the project files are not assets");
        assert_eq!(assets.largest, "official.pebundle");
        assert!(
            assets.summary().contains("(0 to spare)"),
            "{}",
            assets.summary()
        );

        sized(&dir.join("licenses/official.pebundle"), MAX_ASSET_BYTES + 1);
        let refused = check(&dir).expect_err("one byte over is refused");
        assert!(
            refused.contains("`licenses/official.pebundle` is 26214401 bytes"),
            "{refused}"
        );
        assert!(!refused.contains("`official.pebundle`"), "{refused}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrangler_state_and_the_project_files_are_not_counted_or_measured() {
        let dir = scratch("state");
        write(&dir).expect("project files");
        sized(&dir.join("index.html"), 1);
        sized(
            &dir.join(".wrangler/state/v3/cache/blob"),
            MAX_ASSET_BYTES + 1,
        );
        let assets = check(&dir).expect("local state is never uploaded");
        assert_eq!(assets.files, 1);
        assert_eq!(assets.largest, "index.html");
        fs::remove_dir_all(&dir).ok();
    }
}
