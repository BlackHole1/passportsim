//! The whole `var:` chain on the real `official` image: the name resolved from DWARF, the watch
//! armed over its address, and the comparison decided against guest memory. Each layer is
//! unit-tested over synthetic images; only here do the four agree on one firmware.
//!
//! A process of its own because `backend::install`, the session pool and the walker seam are
//! process-wide. Skips, with the reason, when the corpus image or app ELF is absent.

// Test-only file and env access.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::PathBuf;
use std::sync::Arc;

use pemu_api::error::{ApiError, E_TIMEOUT, E_USAGE};
use pemu_api::output::Output;

const OFFICIAL: &str = "official";

fn corpus_path(id: &str, key: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let text = std::fs::read_to_string(home.join(".config/passportsim/corpus.toml")).ok()?;
    let mut in_entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == format!("[{id}]");
        } else if in_entry
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            let raw = v.trim().trim_matches('"');
            let path = match raw.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(raw),
            };
            return path.exists().then_some(path);
        }
    }
    None
}

/// Installs the backend and walkers over the corpus `official` build, or `false` after printing
/// why the test skips.
fn hooked() -> bool {
    let (Some(bin), Some(elf)) = (corpus_path(OFFICIAL, "bin"), corpus_path(OFFICIAL, "elf"))
    else {
        eprintln!("skip: the official corpus image or app ELF is absent");
        return false;
    };
    let image = Arc::new(std::fs::read(&bin).expect("the corpus image is readable"));
    let elf = std::fs::read(&elf).expect("the corpus ELF is readable");
    let context = Arc::new(pemu_host::hooks::ElfContext::parse(&elf).expect("the ELF parses"));
    pemu_host::backend::install(
        Arc::new(move |fw: &str| match fw {
            OFFICIAL => pemu_host::backend::merged_image(&image),
            other => Err(pemu_api::commands::start::firmware_not_found(other)),
        }),
        None,
        pemu_host::audio_root::AudioRoot::new(std::env::temp_dir().join("pemu-var-matcher-audio")),
    );
    pemu_host::hooks::install(pemu_host::hooks::HostHooks {
        elves: Arc::new(move |fw: &str| (fw == OFFICIAL).then(|| Arc::clone(&context))),
        scenario_root: pemu_host::hooks::ScenarioRoot::new(None, Vec::new()),
        salt_dir: None,
    });
    pemu_host::boot_cache::install(None);
    true
}

fn call(name: &str, args: serde_json::Value) -> Result<Output, ApiError> {
    let spec = pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"));
    (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args)
}

fn settled_official() -> String {
    let out = call(
        "start",
        serde_json::json!({ "fw": OFFICIAL, "boot": "until_ui_settled" }),
    )
    .expect("official starts");
    out.json["instance"].as_str().expect("an id").to_owned()
}

fn inspect_vars(inst: &str, names: &[&str]) -> Vec<serde_json::Value> {
    let out = call(
        "inspect",
        serde_json::json!({ "instance": inst, "what": ["vars"], "vars": names }),
    )
    .expect("`inspect vars` answers on a real instance");
    out.json["vars"]
        .as_array()
        .expect("an array of globals")
        .clone()
}

/// The settled `official` menu selects Display (`s_sel == 0`, `s_active == -1`); `click DOWN`
/// makes it `s_sel == 1`.
#[test]
fn the_var_chain_holds_on_the_official_corpus_build() {
    if !hooked() {
        return;
    }
    let inst = settled_official();

    // 1. The section reads name, unit, address, C type and value from the guest itself.
    let rows = inspect_vars(&inst, &["s_sel", "s_active"]);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["name"], "s_sel");
    assert_eq!(rows[0]["unit"], "main.c");
    assert_eq!(rows[0]["type"], "int32");
    assert_eq!(rows[0]["bytes"], 4);
    assert_eq!(rows[0]["value"], 0, "the settled menu selects Display");
    assert_eq!(rows[1]["name"], "s_active");
    assert_eq!(rows[1]["value"], -1, "no page is open");
    let addr = rows[0]["addr"].as_str().expect("an address").to_owned();

    // 2. A matcher already true fires at once: the value reaching it is the guest's.
    let matched = call(
        "run",
        serde_json::json!({ "instance": inst, "until": "var:s_sel == 0", "timeout": "1s" }),
    )
    .expect("`s_sel` is already 0");
    assert_eq!(matched.json["status"], "matched", "{}", matched.json);
    assert_eq!(matched.json["match"]["source"], "var");
    assert_eq!(matched.json["match"]["text"], "s_sel = 0");

    // 3. A false matcher does not fire, or the one above would pass on a leaf that always fires.
    let error = call(
        "run",
        serde_json::json!({ "instance": inst, "until": "var:s_sel == 1", "timeout": "200ms" }),
    )
    .expect_err("nothing pressed DOWN");
    assert_eq!(error.code, E_TIMEOUT, "{error:?}");

    // 4. After the press it fires: the machine accepted the watch (a refused one is silently
    //    dropped and would only ever poll).
    call(
        "input",
        serde_json::json!({ "instance": inst, "button": "down", "action": "click" }),
    )
    .expect("the press is applied");
    let matched = call(
        "run",
        serde_json::json!({ "instance": inst, "until": "var:s_sel == 1", "timeout": "2s" }),
    )
    .expect("`click DOWN` gives s_sel == 1");
    assert_eq!(matched.json["status"], "matched", "{}", matched.json);
    assert_eq!(matched.json["match"]["text"], "s_sel = 1");

    // 5. The section agrees with the matcher, at the same address.
    let rows = inspect_vars(&inst, &["s_sel"]);
    assert_eq!(rows[0]["value"], 1);
    assert_eq!(rows[0]["addr"], addr, "the same object, not another");

    // 6. A compilation-unit qualifier resolves the same object; an index on a scalar is refused.
    let rows = inspect_vars(&inst, &["main.c::s_sel"]);
    assert_eq!(rows[0]["addr"], addr);
    assert_eq!(rows[0]["value"], 1);
    let rows = inspect_vars(&inst, &["s_sel[0]"]);
    assert!(
        rows[0]["unreadable"]
            .as_str()
            .is_some_and(|m| m.contains("not an array")),
        "{:?}",
        rows[0]
    );

    // 7. A missing name is reported against that name; a malformed one is refused before a walk.
    let rows = inspect_vars(&inst, &["s_sel", "no_such_global_here"]);
    assert_eq!(rows[0]["value"], 1, "the good name still answers");
    assert!(
        rows[1]["unreadable"]
            .as_str()
            .is_some_and(|m| m.contains("no_such_global_here")),
        "{:?}",
        rows[1]
    );
    let refused = call(
        "inspect",
        serde_json::json!({ "instance": inst, "what": ["vars"], "vars": ["s sel"] }),
    )
    .expect_err("not a global");
    assert_eq!(refused.code, E_USAGE);

    println!(
        "RAN the_var_chain_holds_on_the_official_corpus_build official: s_sel int32 at {addr} \
         in main.c reads 0 on the settled menu and 1 after `click DOWN`; `run --until \
         var:s_sel == 1` times out before the press and matches after it"
    );
    call("stop", serde_json::json!({ "instance": inst })).expect("the instance stops");
}
