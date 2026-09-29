//! Unit tests of `xtask layering` over in-memory manifests and source strings.

use super::clippy::{REQUIRED_METHODS, REQUIRED_TYPES};
use super::lexer::{Tok, lex};
use super::{CrateInput, Rule, WorkspaceInput, check};

fn clippy_full() -> String {
    let list = |paths: &[&str]| {
        paths
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "disallowed-types = [{}]\ndisallowed-methods = [{}]\n",
        list(REQUIRED_TYPES),
        list(REQUIRED_METHODS)
    )
}

fn manifest(name: &str, body: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n{body}")
}

/// A crate in `crates/<name>` with a complete clippy.toml.
fn krate(name: &str, body: &str, files: &[(&str, &str)]) -> CrateInput {
    CrateInput {
        dir: format!("crates/{name}"),
        manifest: manifest(name, body),
        files: files
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect(),
        clippy_toml: Some(clippy_full()),
    }
}

/// Violation lines of a workspace made of `crates`.
fn violations(crates: Vec<CrateInput>) -> Vec<(Rule, String)> {
    let ws = WorkspaceInput {
        root_manifest: "[workspace]\nmembers = []\n".to_string(),
        root_clippy_toml: None,
        crates,
    };
    check(&ws)
        .expect("check runs")
        .violations
        .into_iter()
        .map(|v| (v.rule, v.to_string()))
        .collect()
}

fn only_rule(found: &[(Rule, String)], rule: Rule) -> bool {
    !found.is_empty() && found.iter().all(|(r, _)| *r == rule)
}

// Crate edges (`crate-edge`).

#[test]
fn allowed_transitive_edge_passes() {
    // The graph is transitive: pemu-wasm reaches pemu-machine, pemu-loader and pemu-core.
    let wasm = krate(
        "pemu-wasm",
        "[dependencies]\npemu-api.workspace = true\npemu-machine.workspace = true\n\
         pemu-loader = { path = \"../pemu-loader\" }\n\n[dev-dependencies]\npemu-core.workspace = true\n",
        &[("src/lib.rs", "pub fn f() {}\n")],
    );
    // The explicit extra edge.
    let soc = krate(
        "pemu-soc-c3",
        "[dependencies]\npemu-board.workspace = true\npemu-rv32.workspace = true\n",
        &[],
    );
    let found = violations(vec![wasm, soc]);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn forbidden_edge_fails() {
    let core = krate(
        "pemu-core",
        "[dependencies]\npemu-machine.workspace = true\n",
        &[],
    );
    let found = violations(vec![core]);
    assert!(only_rule(&found, Rule::CrateEdge), "{found:?}");
    assert_eq!(found.len(), 1);
    assert!(
        found[0]
            .1
            .starts_with("crate-edge pemu-core crates/pemu-core/Cargo.toml:6: `pemu-machine`"),
        "{found:?}"
    );
}

#[test]
fn forbidden_dev_and_target_edges_fail() {
    let board = krate(
        "pemu-board",
        "[dev-dependencies]\npemu-soc-c3.workspace = true\n\n\
         [target.'cfg(unix)'.dependencies]\npemu-host.workspace = true\n",
        &[],
    );
    let found = violations(vec![board]);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(only_rule(&found, Rule::CrateEdge), "{found:?}");
    assert!(
        found
            .iter()
            .any(|(_, l)| l.contains("Cargo.toml:6: `pemu-soc-c3` (dev-dependencies)"))
    );
    assert!(
        found
            .iter()
            .any(|(_, l)| l.contains("Cargo.toml:9: `pemu-host` (dependencies)"))
    );
}

#[test]
fn milestones_may_dev_depend_on_testkit() {
    let mut milestones = krate(
        "pemu-milestones",
        "autotests = false\n\n[dev-dependencies]\npemu-testkit.workspace = true\n\n\
         [[test]]\nname = \"m0\"\npath = \"m0.rs\"\n",
        &[("m0.rs", "use std::time::Instant;\n#[test]\nfn m0() {}\n")],
    );
    milestones.dir = "tests/milestones".to_string();
    milestones.clippy_toml = None;
    let found = violations(vec![milestones]);
    assert!(found.is_empty(), "{found:?}");
}

// Third-party dependencies (`third-party`).

#[test]
fn third_party_follows_the_crate_table() {
    let board = krate(
        "pemu-board",
        "[dependencies]\nserde.workspace = true\n",
        &[],
    );
    let api = krate(
        "pemu-api",
        "[dependencies]\nsha2.workspace = true\nserde_json.workspace = true\n\n\
         [target.'cfg(not(target_arch = \"wasm32\"))'.dependencies]\nlinkme.workspace = true\n",
        &[],
    );
    let mut xtask = krate("xtask", "[dependencies]\nanything = \"1\"\n", &[]);
    xtask.dir = "xtask".to_string();
    assert!(violations(vec![board, api, xtask]).is_empty());

    let rv32 = krate(
        "pemu-rv32",
        "[dependencies]\npemu-core.workspace = true\nserde.workspace = true\n",
        &[],
    );
    let found = violations(vec![rv32]);
    assert!(only_rule(&found, Rule::ThirdParty), "{found:?}");
    assert!(
        found[0].1.contains("Cargo.toml:7: third-party `serde`"),
        "{found:?}"
    );
}

#[test]
fn renamed_dependency_uses_its_package_name() {
    let rv32 = krate(
        "pemu-rv32",
        "[dependencies]\nfast = { package = \"pemu-machine\", path = \"../pemu-machine\" }\n",
        &[],
    );
    let found = violations(vec![rv32]);
    assert!(only_rule(&found, Rule::CrateEdge), "{found:?}");
}

#[test]
fn crate_missing_from_the_table_fails() {
    let found = violations(vec![krate("pemu-extra", "", &[])]);
    assert!(only_rule(&found, Rule::CrateTable), "{found:?}");
}

// Core std APIs (`core-std-api`).

/// Violations of a `pemu-core` crate with `src/lib.rs` declaring `pub mod time;` plus `files`.
fn core_with(files: &[(&str, &str)]) -> Vec<(Rule, String)> {
    let mut all = vec![("src/lib.rs", "pub mod time;\n")];
    all.extend_from_slice(files);
    violations(vec![krate("pemu-core", "", &all)])
}

#[test]
fn planted_instant_in_pemu_core_fails() {
    let time = "//! Time.\n\nuse std::time::Instant;\n\npub fn now() -> Instant {\n    Instant::now()\n}\n";
    let found = core_with(&[("src/time.rs", time)]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0]
            .1
            .starts_with("core-std-api pemu-core crates/pemu-core/src/time.rs:3: `std::time`"),
        "{found:?}"
    );
}

#[test]
fn same_use_inside_cfg_test_module_passes() {
    let time = "pub fn f() {}\n\n#[cfg(test)]\nmod tests {\n    use std::time::Instant;\n\n    \
                #[test]\n    fn t() {\n        let _ = Instant::now();\n    }\n}\n";
    assert!(core_with(&[("src/time.rs", time)]).is_empty());

    // The gate ends with the module: a use after it is flagged on its own line.
    let after = format!("{time}\nuse std::fs;\n");
    let found = core_with(&[("src/time.rs", after.as_str())]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].1.contains("time.rs:13: `std::fs`"), "{found:?}");
}

#[test]
fn test_gates_are_recognized() {
    let passing = [
        "#[cfg(test)]\nmod tests;\n",
        "#[cfg(all(test, feature = \"x\"))]\nmod t { use std::thread; }\n",
        "#[cfg(any(test, all(test, unix)))]\nuse std::env;\n",
        "#[test]\nfn t() { std::thread::sleep(core::time::Duration::ZERO); }\n",
        "#[tokio::test]\nasync fn t() { let _ = std::env::var(\"X\"); }\n",
        "mod inner {\n    #![cfg(test)]\n    use std::net::TcpStream;\n}\n",
    ];
    for time in passing {
        // `mod tests;` in the non-mod-rs file `src/time.rs` resolves to `src/time/tests.rs`.
        let files = [
            ("src/time.rs", time),
            ("src/time/tests.rs", "use std::collections::HashMap;\n"),
        ];
        let found = core_with(&files[..if time.contains("mod tests;") { 2 } else { 1 }]);
        assert!(found.is_empty(), "{time}: {found:?}");
    }
    let failing = [
        "#[cfg(any(test, feature = \"x\"))]\nuse std::fs;\n",
        "#[cfg(not(test))]\nuse std::fs;\n",
        "#[cfg_attr(test, derive(Debug))]\nstruct S(std::fs::File);\n",
    ];
    for time in failing {
        let found = core_with(&[("src/time.rs", time)]);
        assert!(only_rule(&found, Rule::CoreStdApi), "{time}: {found:?}");
    }
}

#[test]
fn gated_module_file_passes_and_ungated_module_file_fails() {
    let lib =
        "#[cfg(test)]\nmod checks;\nmod util;\n#[path = \"extra/other.rs\"]\npub mod other;\n";
    let files = [
        ("src/lib.rs", lib),
        ("src/checks.rs", "use std::collections::HashSet;\n"),
        (
            "src/util.rs",
            "pub type M = std::collections::HashMap<u8, u8>;\n",
        ),
        (
            "src/extra/other.rs",
            "#![cfg(test)]\nuse std::process::Command;\n",
        ),
    ];
    let found = violations(vec![krate("pemu-core", "", &files)]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].1.contains("src/util.rs:1: `HashMap`"), "{found:?}");
}

#[test]
fn std_paths_are_found_in_groups_and_ignored_in_comments_and_strings() {
    let time = "use std::{collections::{BTreeMap, HashMap}, fs, io};\n\
                // std::time::Instant is fine in a comment\n\
                const S: &str = \"std::time\";\n\
                use std::collections::hash_map::Entry;\n\
                use std::*;\n\
                fn f() { let _ = ::std::env::var(\"X\"); }\n\
                fn g(d: core::time::Duration) -> std::io::Result<()> { Ok(()) }\n";
    let found = core_with(&[("src/time.rs", time)]);
    let lines: Vec<&str> = found.iter().map(|(_, l)| l.as_str()).collect();
    assert_eq!(found.len(), 5, "{lines:#?}");
    for expected in [
        "time.rs:1: `HashMap`",
        "time.rs:1: `std::fs`",
        "time.rs:4: `std::collections::hash_map`",
        "time.rs:5: `std::*`",
        "time.rs:6: `std::env`",
    ] {
        assert!(
            lines.iter().any(|l| l.contains(expected)),
            "{expected}: {lines:#?}"
        );
    }
}

#[test]
fn host_crates_may_use_std_apis() {
    let mut host = krate(
        "pemu-host",
        "",
        &[(
            "src/lib.rs",
            "use std::time::Instant;\nuse std::collections::HashMap;\n",
        )],
    );
    host.clippy_toml = None;
    assert!(violations(vec![host]).is_empty());
}

// Serial devices (`serial-device`).

#[test]
fn dev_cu_string_outside_the_planner_fails() {
    let host_src = "// Never opens /dev/cu.* (comments are ignored).\n\
                    pub fn port() -> &'static str {\n    \"/dev/cu.usbserial\"\n}\n\
                    pub const TTY: &str = r#\"/dev/tty.x\"#;\n\
                    #[cfg(test)]\nmod tests { const P: &str = \"/dev/cu.test\"; }\n";
    let mut host = krate("pemu-host", "", &[("src/lib.rs", host_src)]);
    host.clippy_toml = None;
    let found = violations(vec![host]);
    let lines: Vec<&str> = found.iter().map(|(_, l)| l.as_str()).collect();
    assert!(only_rule(&found, Rule::SerialDevice), "{lines:#?}");
    assert_eq!(found.len(), 2, "{lines:#?}");
    assert!(lines[0].starts_with("serial-device pemu-host crates/pemu-host/src/lib.rs:3:"));
    assert!(lines[1].contains("src/lib.rs:5:"), "{lines:#?}");
}

/// Layering rule 5 covers the Windows spellings too: the device namespaces in either literal
/// form, and a reserved port name in any case.
#[test]
fn windows_device_paths_outside_the_planner_fail() {
    let host_src = "pub const A: &str = \"\\\\\\\\.\\\\COM3\";\n\
                    pub const B: &str = r\"\\\\?\\\\COM12\";\n\
                    pub const C: &str = \"com9:\";\n\
                    pub const D: &str = \"LPT1\";\n\
                    pub const OK1: &str = \"COM0\";\n\
                    pub const OK2: &str = \"command line\";\n\
                    pub const OK3: &str = \"com12\";\n";
    let mut host = krate("pemu-host", "", &[("src/lib.rs", host_src)]);
    host.clippy_toml = None;
    let found = violations(vec![host]);
    let lines: Vec<&str> = found.iter().map(|(_, l)| l.as_str()).collect();
    assert!(only_rule(&found, Rule::SerialDevice), "{lines:#?}");
    assert_eq!(found.len(), 4, "{lines:#?}");
    for (index, line) in (1..=4).zip(&lines) {
        assert!(line.contains(&format!("src/lib.rs:{index}:")), "{lines:#?}");
    }
}

/// A port open is caught at the call site, not only when a port name appears as a literal. A
/// path from OS enumeration never appears as a literal, so the literal rule alone sees nothing.
#[test]
fn a_planted_port_open_outside_the_planner_fails() {
    let host_src = "use std::os::unix::fs::OpenOptionsExt as _;\n                    const O_NOCTTY: i32 = 0x0002_0000;\n                    pub fn read(port: &str) -> std::io::Result<std::fs::File> {\n                    std::fs::OpenOptions::new().read(true).custom_flags(O_NOCTTY).open(port)\n}\n";
    let mut host = krate(
        "pemu-host",
        "",
        &[
            ("src/lib.rs", "mod console;\n"),
            ("src/console.rs", host_src),
        ],
    );
    host.clippy_toml = None;
    let found = violations(vec![host]);
    let lines: Vec<&str> = found.iter().map(|(_, l)| l.as_str()).collect();
    assert!(only_rule(&found, Rule::SerialDevice), "{lines:#?}");
    assert!(
        lines.iter().any(|l| l.contains("src/console.rs:2:")),
        "the declaration is caught: {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("src/console.rs:4:")),
        "so is the open: {lines:#?}"
    );
    assert!(
        lines.iter().all(|l| l.contains("terminal device")),
        "{lines:#?}"
    );

    // The same code in the planner's device feature is what rule 5 allows.
    let planner = krate(
        "pemu-planner",
        "[features]\ndevice = []\n",
        &[
            ("src/lib.rs", "#[cfg(feature = \"device\")]\nmod exec;\n"),
            ("src/exec.rs", host_src),
        ],
    );
    assert!(violations(vec![planner]).is_empty());

    // And a termios call in a test is not a product port open.
    let mut gated = krate(
        "pemu-host",
        "",
        &[(
            "src/lib.rs",
            "#[cfg(test)]\nmod tests { const O_NOCTTY: i32 = 0; }\n",
        )],
    );
    gated.clippy_toml = None;
    assert!(violations(vec![gated]).is_empty());
}

/// A reset toggles DTR and RTS, which opens no port and writes no byte, so none of the
/// open-shaped identifiers sees it; the modem-control pair must catch it.
#[test]
fn a_planted_modem_control_pulse_outside_the_planner_fails() {
    // The shape `pemu_planner::exec::pulse_reset` has: no open at all, just two ioctls on a
    // descriptor someone already holds.
    let pulse = "const TIOCMSET: u64 = 0x8004_746D;\n                  const TIOCMGET: u64 = 0x4004_746A;\n                  pub fn reset(fd: i32, lines: *mut i32) {\n                  unsafe { ioctl(fd, TIOCMGET, lines) };\n                  unsafe { ioctl(fd, TIOCMSET, lines) };\n}\n";
    let mut host = krate(
        "pemu-host",
        "",
        &[("src/lib.rs", "mod reset;\n"), ("src/reset.rs", pulse)],
    );
    host.clippy_toml = None;
    let found = violations(vec![host]);
    let lines: Vec<&str> = found.iter().map(|(_, l)| l.as_str()).collect();
    assert!(only_rule(&found, Rule::SerialDevice), "{lines:#?}");
    assert!(
        lines.iter().any(|l| l.contains("src/reset.rs:1:")),
        "the `TIOCMSET` declaration is caught: {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("src/reset.rs:2:")),
        "and the `TIOCMGET` one: {lines:#?}"
    );
    assert!(
        lines.iter().all(|l| l.contains("terminal device")),
        "{lines:#?}"
    );

    // The same pulse inside the planner's device feature is what rule 5 allows, and it
    // is where `device_boot_check --reset` really lives.
    let planner = krate(
        "pemu-planner",
        "[features]\ndevice = []\n",
        &[
            ("src/lib.rs", "#[cfg(feature = \"device\")]\nmod exec;\n"),
            ("src/exec.rs", pulse),
        ],
    );
    assert!(violations(vec![planner]).is_empty());

    // The pty endpoint keeps its whole-file exemption: it answers the same ioctls about a
    // pseudo-terminal it created itself, and the real file names `TIOCMGET` several times.
    let mut exempt = krate(
        "pemu-host",
        "",
        &[
            ("src/lib.rs", "mod endpoints;\n"),
            ("src/endpoints/pty.rs", pulse),
        ],
    );
    exempt.clippy_toml = None;
    assert!(
        violations(vec![exempt]).is_empty(),
        "`src/endpoints/pty.rs` is the audited exception (PORT_OPEN_EXEMPT)"
    );
}

#[test]
fn planner_device_feature_may_reach_a_serial_device() {
    let lib = "#[cfg(feature = \"device\")]\nmod device;\n";
    let device = "use std::fs::OpenOptions;\nconst PORT: &str = \"/dev/cu.usbmodem1\";\n";
    let planner = krate(
        "pemu-planner",
        "[features]\ndevice = []\n",
        &[("src/lib.rs", lib), ("src/device.rs", device)],
    );
    assert!(violations(vec![planner]).is_empty());

    let ungated = krate("pemu-planner", "", &[("src/lib.rs", device)]);
    let rules: Vec<Rule> = violations(vec![ungated])
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(rules, vec![Rule::CoreStdApi, Rule::SerialDevice]);
}

#[test]
fn serial_port_crates_only_behind_the_planner_device_feature() {
    let mut host = krate("pemu-host", "[dependencies]\nserialport = \"4\"\n", &[]);
    host.clippy_toml = None;
    let rules: Vec<Rule> = violations(vec![host]).into_iter().map(|(r, _)| r).collect();
    assert_eq!(rules, vec![Rule::ThirdParty, Rule::SerialDevice]);

    // Optional and enabled only by `device`: only the third-party table is left.
    let planner = krate(
        "pemu-planner",
        "[features]\ndevice = [\"dep:serialport\"]\n\n[dependencies]\n\
         serialport = { version = \"4\", optional = true }\n",
        &[],
    );
    let rules: Vec<Rule> = violations(vec![planner])
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(rules, vec![Rule::ThirdParty]);

    let always = krate(
        "pemu-planner",
        "[dependencies]\ntokio-serial = \"5\"\n",
        &[],
    );
    let rules: Vec<Rule> = violations(vec![always])
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(rules, vec![Rule::ThirdParty, Rule::SerialDevice]);
}

// Clippy configuration.

#[test]
fn clippy_config_is_checked() {
    let mut missing = krate("pemu-rv32", "", &[]);
    missing.clippy_toml = None;
    let mut partial = krate("pemu-board", "", &[]);
    partial.clippy_toml = Some(clippy_full().replace("\"std::fs::File\", ", ""));
    let found = violations(vec![missing, partial]);
    assert!(only_rule(&found, Rule::ClippyConfig), "{found:?}");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .any(|(_, l)| l.contains("disallowed-types is missing `std::fs::File`"))
    );

    let ws = WorkspaceInput {
        root_manifest: "[workspace]\n".to_string(),
        root_clippy_toml: Some(
            "msrv = \"1.93\"\ndisallowed-types = [\"std::fs::File\"]\n".to_string(),
        ),
        crates: vec![krate("pemu-core", "", &[])],
    };
    let found = check(&ws).expect("check runs").violations;
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().all(|v| v.rule == Rule::ClippyConfig));
    assert!(
        found
            .iter()
            .any(|v| v.krate == "workspace" && v.detail.contains("std::fs::File"))
    );
    assert!(
        found
            .iter()
            .any(|v| v.krate == "pemu-core" && v.detail.contains("`msrv`"))
    );
}

// Lexer.

#[test]
fn lexer_handles_literals_comments_and_lifetimes() {
    let src = "/* outer /* nested\n */ */ fn f<'a>(x: &'a u8) -> char {\n    \
               let _ = (b\"b\\\"s\", r##\"raw \"# still\"##, 1.5, 0..2);\n    '\\''\n}\n";
    let toks = lex(src);
    let strings: Vec<&str> = toks
        .iter()
        .filter_map(|t| match &t.tok {
            Tok::Str(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(strings, vec!["b\\\"s", "raw \"# still"]);
    let f = toks.iter().find(|t| t.is_ident("fn")).expect("fn token");
    assert_eq!(f.line, 2);
    assert!(!toks.iter().any(|t| t.is_ident("a") || t.is_ident("nested")));
    assert_eq!(
        toks.last().map(|t| (t.tok.clone(), t.line)),
        Some((Tok::Punct('}'), 5))
    );
}

// The Windows twins of the serial-device rules.

/// The Windows port calls are caught outside the planner's device feature, as the `TIOCMSET`
/// pulse is: `SetCommState`, `SetCommTimeouts` and the `EscapeCommFunction` pulse of
/// `exec/windows.rs` have one purpose.
#[test]
fn a_planted_windows_port_call_outside_the_planner_fails() {
    let pulse = "pub fn reset(h: isize) {\n    unsafe { EscapeCommFunction(h, SETRTS) };\n}\n\
                 pub fn configure(h: isize, dcb: *const u8) {\n    unsafe { SetCommState(h, dcb) };\n}\n";
    let mut host = krate(
        "pemu-host",
        "",
        &[("src/lib.rs", "mod port;\n"), ("src/port.rs", pulse)],
    );
    host.clippy_toml = None;
    let found = violations(vec![host]);
    let lines: Vec<&str> = found.iter().map(|(_, l)| l.as_str()).collect();
    assert!(only_rule(&found, Rule::SerialDevice), "{lines:#?}");
    for name in ["EscapeCommFunction", "SETRTS", "SetCommState"] {
        assert!(
            lines.iter().any(|l| l.contains(name)),
            "`{name}` is caught: {lines:#?}"
        );
    }

    let planner = krate(
        "pemu-planner",
        "[features]\ndevice = []\n",
        &[
            ("src/lib.rs", "#[cfg(feature = \"device\")]\nmod exec;\n"),
            ("src/exec.rs", "mod windows;\n"),
            ("src/exec/windows.rs", pulse),
        ],
    );
    assert!(violations(vec![planner]).is_empty());
}

/// `windows-sys` is allowed in the planner only as an optional dependency that feature `device`
/// alone enables, so the pure planner stays a core crate with no third-party dependency.
#[test]
fn the_planner_takes_windows_sys_only_behind_its_device_feature() {
    let ok = krate(
        "pemu-planner",
        "[features]\ndevice = [\"dep:windows-sys\", \"windows-sys/Win32_Devices_Communication\"]\n\n\
         [target.'cfg(windows)'.dependencies]\nwindows-sys = { version = \"0.61\", optional = true }\n",
        &[],
    );
    let found = violations(vec![ok]);
    assert!(found.is_empty(), "{found:?}");

    let always = krate(
        "pemu-planner",
        "[target.'cfg(windows)'.dependencies]\nwindows-sys = \"0.61\"\n",
        &[],
    );
    let found = violations(vec![always]);
    assert!(only_rule(&found, Rule::SerialDevice), "{found:?}");

    let other_feature = krate(
        "pemu-planner",
        "[features]\ndevice = [\"dep:windows-sys\"]\nextra = [\"dep:windows-sys\"]\n\n\
         [target.'cfg(windows)'.dependencies]\nwindows-sys = { version = \"0.61\", optional = true }\n",
        &[],
    );
    let found = violations(vec![other_feature]);
    assert!(only_rule(&found, Rule::SerialDevice), "{found:?}");
}

/// The SetupAPI and communications features of `windows-sys` are enabled only through feature
/// `device`, SetupAPI in the host or the planner and communications in the planner alone; never in
/// a dependency's own list and never in the workspace entry.
#[test]
fn windows_device_features_only_through_feature_device() {
    let host_body = |features: &str, inline: &str| {
        format!(
            "[features]\n{features}\n\n[target.'cfg(windows)'.dependencies]\n\
             windows-sys = {{ version = \"0.61\"{inline} }}\n"
        )
    };
    let host = |body: String| {
        let mut host = krate("pemu-host", &body, &[]);
        host.clippy_toml = None;
        violations(vec![host])
    };

    // The host's enumeration: SetupAPI behind its `device` feature is allowed.
    let found = host(host_body(
        "device = [\"windows-sys/Win32_Devices_DeviceAndDriverInstallation\"]",
        "",
    ));
    assert!(found.is_empty(), "{found:?}");
    // The communications feature is the planner's alone.
    let found = host(host_body(
        "device = [\"windows-sys/Win32_Devices_Communication\"]",
        "",
    ));
    assert!(only_rule(&found, Rule::SerialDevice), "{found:?}");
    // Another feature name turns it on for builds that never asked for the device group.
    let found = host(host_body(
        "extra = [\"windows-sys/Win32_Devices_DeviceAndDriverInstallation\"]",
        "",
    ));
    assert!(only_rule(&found, Rule::SerialDevice), "{found:?}");
    // The dependency's own list turns it on for every build.
    let found = host(host_body(
        "device = []",
        ", features = [\"Win32_Devices_DeviceAndDriverInstallation\"]",
    ));
    assert!(only_rule(&found, Rule::SerialDevice), "{found:?}");

    // The workspace entry, which every crate naming the binding inherits.
    let ws = WorkspaceInput {
        root_manifest: "[workspace]\nmembers = []\n\n[workspace.dependencies]\n\
                        windows-sys = { version = \"0.61\", features = [\"Win32_Foundation\", \
                        \"Win32_Devices_Communication\"] }\n"
            .to_string(),
        root_clippy_toml: None,
        crates: Vec::new(),
    };
    let found: Vec<String> = check(&ws)
        .expect("check runs")
        .violations
        .into_iter()
        .filter(|v| v.rule == Rule::SerialDevice)
        .map(|v| v.to_string())
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("Win32_Devices_Communication"),
        "{found:?}"
    );
}
