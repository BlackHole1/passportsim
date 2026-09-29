//! The layering policy as data: the crate table, the dependency graph with its extra edges, and
//! the std APIs core crates must not use (`docs/ARCHITECTURE.md`, Layering).

use std::collections::{BTreeMap, BTreeSet};

/// Third-party crates a workspace crate may depend on (normal, dev and build dependencies).
#[derive(Clone, Copy, Debug)]
pub enum ThirdParty {
    /// Only the listed crates.
    Only(&'static [&'static str]),
    /// Any crate (`xtask`).
    Any,
}

/// One row of the crate table.
#[derive(Clone, Copy, Debug)]
pub struct CratePolicy {
    /// Package name.
    pub name: &'static str,
    /// Core crate in the sense of layering rule 1.
    pub core: bool,
    /// Allowed third-party dependencies.
    pub third_party: ThirdParty,
}

/// The crate table: the third-party crates each workspace crate may use.
///
/// - `pemu-core` re-exports `pub use serde;`, so crates below `pemu-machine` derive through
///   `pemu_core::serde` without a serde dependency of their own.
/// - `pemu-board`: serde, for `Chip: Serialize + DeserializeOwned`.
/// - `pemu-api`: linkme for `#[command]` registration and sha2 for the `secret_set` builder shared
///   with `xtask secrets-check`.
/// - `pemu-cli`: clap, for the clap tree generated from the registry (the workspace manifest lists
///   clap as native only), and serde_json, because it parses the `--json` document and prints
///   `Output::to_json`, whose types are `pemu-api`'s public API.
///
/// `core` follows layering rule 1: every crate except `pemu-host`, `pemu-cli`, `pemu-testkit`,
/// `pemu-verify`, `pemu-milestones` and `xtask`. `pemu-planner` is core; only its code behind
/// `feature = "device"` is exempt (see `DEVICE_CRATE`).
pub const CRATES: &[CratePolicy] = &[
    core("pemu-core", &["serde", "postcard", "blake3"]),
    core("pemu-rv32", &[]),
    core("pemu-macros", &["syn", "quote", "proc-macro2"]),
    core("pemu-soc-c3", &[]),
    core("pemu-board", &["serde"]),
    core("pemu-loader", &["object", "sha2", "md-5"]),
    core("pemu-hle", &[]),
    core("pemu-radio", &["smoltcp"]),
    core("pemu-machine", &[]),
    core("pemu-introspect", &["gimli", "addr2line"]),
    core("pemu-api", &["serde_json", "schemars", "linkme", "sha2"]),
    // `windows-sys`: the port read and the owner-only scratch files of `exec/windows.rs`. It is
    // optional, in the `cfg(windows)` target table, and enabled by feature `device` alone
    // (`DEVICE_ONLY_DEPS`), so the pure planner stays a core crate with no third-party dependency.
    core("pemu-planner", &["windows-sys"]),
    // `serde_json`: `pemu_new`, `pemu_input` and `pemu_call` are a JSON cold path, and
    // `pemu_api::output::Output` and `ApiError` already speak `serde_json::Value`.
    core("pemu-wasm", &["serde_json"]),
    host(
        "pemu-host",
        // `serde_json` and `getrandom` are real dependencies of the daemon: the discovery file,
        // the HTTP and WebSocket bodies and MCP JSON-RPC are JSON, and the 256-bit daemon token
        // needs OS entropy from the one portable source that adds no `cfg` outside `platform`.
        // A `pub use serde_json;` in `pemu-api` would only hide the edge.
        &[
            "tokio",
            "axum",
            "tungstenite",
            "png",
            "serde_json",
            "getrandom",
            // Win32 platform calls and known folders, in the `cfg(windows)` target table only.
            "windows-sys",
        ],
    ),
    host("pemu-cli", &["clap", "serde_json"]),
    host("pemu-testkit", &[]),
    host("pemu-verify", &[]),
    // `serde_json`: a milestone test that claims a command claims it through the registry, and
    // `pemu_api::registry` hands a handler a `serde_json::Value`. A test that built
    // its arguments any other way would be claiming a different entry point than an agent uses.
    host("pemu-milestones", &["serde_json"]),
    CratePolicy {
        name: "xtask",
        core: false,
        third_party: ThirdParty::Any,
    },
];

const fn core(name: &'static str, deps: &'static [&'static str]) -> CratePolicy {
    CratePolicy {
        name,
        core: true,
        third_party: ThirdParty::Only(deps),
    }
}

const fn host(name: &'static str, deps: &'static [&'static str]) -> CratePolicy {
    CratePolicy {
        name,
        core: false,
        third_party: ThirdParty::Only(deps),
    }
}

/// Direct edges of the layering mermaid graph, `(from, to)`.
pub const GRAPH_EDGES: &[(&str, &str)] = &[
    ("pemu-rv32", "pemu-core"),
    ("pemu-soc-c3", "pemu-rv32"),
    ("pemu-soc-c3", "pemu-core"),
    ("pemu-board", "pemu-core"),
    ("pemu-loader", "pemu-core"),
    ("pemu-hle", "pemu-rv32"),
    ("pemu-hle", "pemu-loader"),
    ("pemu-radio", "pemu-hle"),
    ("pemu-machine", "pemu-soc-c3"),
    ("pemu-machine", "pemu-board"),
    ("pemu-machine", "pemu-radio"),
    ("pemu-introspect", "pemu-loader"),
    ("pemu-api", "pemu-machine"),
    ("pemu-api", "pemu-introspect"),
    ("pemu-api", "pemu-macros"),
    ("pemu-planner", "pemu-loader"),
    ("pemu-host", "pemu-api"),
    ("pemu-host", "pemu-planner"),
    ("pemu-cli", "pemu-host"),
    ("pemu-wasm", "pemu-api"),
    ("pemu-verify", "pemu-loader"),
    ("pemu-testkit", "pemu-machine"),
    ("pemu-testkit", "pemu-verify"),
    ("xtask", "pemu-testkit"),
    ("xtask", "pemu-api"),
    ("xtask", "pemu-host"),
];

/// Edges added to the graph before the transitive closure is taken.
///
/// - `pemu-soc-c3 -> pemu-board`: `SocBus::board: &mut dyn BoardPorts` (layering rule 3).
/// - `pemu-milestones -> pemu-testkit`: a dev-dependency of `tests/milestones/`; the mermaid graph
///   has no node for it.
/// - `pemu-milestones -> pemu-host`: a dev-dependency, for the tests that claim host behavior.
///   `passportsim doctor` (ROM hashes, corpus, roles) discovers through `pemu_host::assets`; a
///   test with its own discovery would claim what the product does not do.
/// - `pemu-host -> pemu-testkit`: a dev-dependency only. The endpoint, HCI and relay unit tests
///   attach a `MockMachine` where a real machine would need a firmware image, and the mermaid
///   graph draws no dev edges.
pub const EXTRA_EDGES: &[(&str, &str)] = &[
    ("pemu-soc-c3", "pemu-board"),
    ("pemu-milestones", "pemu-testkit"),
    ("pemu-milestones", "pemu-host"),
    ("pemu-host", "pemu-testkit"),
];

/// The only crate that may reach a serial device, and only behind this feature (layering rule 5).
pub const DEVICE_CRATE: &str = "pemu-planner";
/// The feature of `DEVICE_CRATE` that gates serial-device code.
pub const DEVICE_FEATURE: &str = "device";

/// Third-party dependencies a crate may have **only** as an optional normal dependency that feature
/// `DEVICE_FEATURE` enables and no other feature does, `(crate, package)`.
pub const DEVICE_ONLY_DEPS: &[(&str, &str)] = &[("pemu-planner", "windows-sys")];

/// The `windows-sys` features that reach a serial device, and the crates that may enable each
/// one, only through their feature `DEVICE_FEATURE`.
///
/// - `Win32_Devices_Communication` (`CreateFileW` on a port is ordinary, but `SetCommState`,
///   `SetCommTimeouts` and `EscapeCommFunction` are what a port read and a reset need) belongs to
///   [`DEVICE_CRATE`] alone, the only code that may open a host serial device.
/// - `Win32_Devices_DeviceAndDriverInstallation` (SetupAPI) is the discovery step of flashing,
///   which opens nothing and lives in `pemu-host`'s `platform::serial` behind that crate's feature
///   `device`; the planner may enable it too.
///
/// Neither may appear in a dependency's own `features` list, in the workspace entry, or in any
/// other feature: those would turn it on for every build.
pub const DEVICE_WINDOWS_FEATURES: &[(&str, &[&str])] = &[
    ("Win32_Devices_Communication", &[DEVICE_CRATE]),
    (
        "Win32_Devices_DeviceAndDriverInstallation",
        &[DEVICE_CRATE, "pemu-host"],
    ),
];

/// The crates whose features [`DEVICE_WINDOWS_FEATURES`] are.
pub const WINDOWS_BINDINGS: &[&str] = &["windows-sys", "windows"];

/// `std` modules core crates must not use (layering rule 1). `core::time::Duration` stays
/// available; the `std::time` path is rejected as a whole.
pub const FORBIDDEN_STD_MODULES: &[&str] = &["time", "thread", "fs", "env", "net", "process"];

/// Identifiers core crates must not name: `HashMap`/`HashSet` iteration order is not
/// deterministic.
pub const FORBIDDEN_IDENTS: &[&str] = &["HashMap", "HashSet"];

/// Modules under `std::collections` core crates must not name.
pub const FORBIDDEN_COLLECTION_MODULES: &[&str] = &["hash_map", "hash_set"];

/// Device path prefixes of layering rule 5, in the collapsed form [`collapse_slashes`] produces.
/// Built with `concat!` so this file does not itself contain the literal prefixes.
pub const DEVICE_PATH_PREFIXES: &[&str] = &[
    concat!("/dev/", "cu."),
    concat!("/dev/", "tty."),
    // The Windows device namespaces, after runs of backslashes collapse to one: `\\.\COM3` and
    // `\\?\COM3` both reach this as `\.\`.
    concat!("\\", ".", "\\"),
    concat!("\\", "?", "\\"),
];

/// Reserved Windows device stems that name a port. `COM0` and `LPT0` are not reserved and are
/// not listed.
const RESERVED_PORT_STEMS: &[&str] = &[concat!("co", "m"), concat!("lp", "t")];

/// Identifiers that only code which **opens or configures a terminal device** needs: the termios,
/// modem-control and serial-crate call shapes, because a port path can come from enumeration at
/// run time and never appear as a literal. The modem-control pair is here
/// because a reset toggles DTR and RTS through `TIOCMSET` without opening a port.
///
/// Not complete: a plain `std::fs::File::open(port)` on a runtime path is invisible to it. It
/// raises the cost of opening a device outside [`DEVICE_CRATE`]; review is the proof.
/// `custom_flags` and `OpenOptions` are left out on purpose, since owner-only file opens use them
/// all over `pemu-host`. Spelled with `concat!` so this file does not match its own scan.
pub const PORT_OPEN_IDENTS: &[&str] = &[
    concat!("O_NO", "CTTY"),
    concat!("TIOC", "EXCL"),
    concat!("TIOC", "MGET"),
    concat!("TIOC", "MSET"),
    concat!("tc", "getattr"),
    concat!("tc", "setattr"),
    concat!("tc", "flush"),
    concat!("tc", "drain"),
    concat!("cf", "makeraw"),
    concat!("cf", "setspeed"),
    concat!("cf", "setispeed"),
    concat!("cf", "setospeed"),
    concat!("serial", "port"),
    concat!("serial", "2"),
    concat!("tokio_", "serial"),
    concat!("mio_", "serial"),
    // The Windows twins: configuring a port and driving its modem lines.
    // `CreateFileW` is not here, for the reason `OpenOptions` is not: every owner-only file of
    // the Windows arm is created with it.
    concat!("EscapeComm", "Function"),
    concat!("GetComm", "State"),
    concat!("SetComm", "State"),
    concat!("GetComm", "Timeouts"),
    concat!("SetComm", "Timeouts"),
    concat!("SetComm", "Mask"),
    concat!("WaitComm", "Event"),
    concat!("Purge", "Comm"),
    concat!("SetComm", "Break"),
    concat!("ClearComm", "Break"),
    concat!("GetCommModem", "Status"),
    concat!("SET", "DTR"),
    concat!("CLR", "DTR"),
    concat!("SET", "RTS"),
    concat!("CLR", "RTS"),
];

/// Files outside [`DEVICE_CRATE`] that may still name a [`PORT_OPEN_IDENTS`] identifier, with the
/// reason each one is not a host serial device. The list is the audit trail: adding a row is a
/// review decision, and it is short on purpose.
pub const PORT_OPEN_EXEMPT: &[(&str, &str, &str)] = &[(
    "pemu-host",
    "src/endpoints/pty.rs",
    "the USB Serial/JTAG endpoint opens the pseudo-terminal pair it created itself \
     (`posix_openpt`), never a device the OS enumerated",
)];

/// The finding text for an identifier that opens or configures a terminal device.
pub fn port_open_ident(name: &str) -> Option<String> {
    PORT_OPEN_IDENTS
        .contains(&name)
        .then(|| format!("`{name}`, which opens or configures a terminal device,"))
}

/// Whether `file` (relative to its crate directory, with `/` separators) is an audited exception
/// to [`PORT_OPEN_IDENTS`] for `crate_name`.
pub fn port_open_exempt(crate_name: &str, file: &str) -> bool {
    PORT_OPEN_EXEMPT
        .iter()
        .any(|(c, path, _)| *c == crate_name && *path == file)
}

/// Runs of backslashes collapsed to one, so an escaped literal and a raw literal of the same
/// path reach the device lint in the same form.
pub fn collapse_slashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_slash = false;
    for c in text.chars() {
        let slash = c == '\\';
        if !(slash && previous_slash) {
            out.push(c);
        }
        previous_slash = slash;
    }
    out
}

/// Why a string literal names a host serial device, or `None`.
///
/// Matching is case-insensitive because both supported hosts are, and it sees the collapsed form
/// of the literal, so `COM3`, `com3:`, `\\.\COM3` and `r"\\?\COM12"` all match.
pub fn device_literal(text: &str) -> Option<String> {
    let collapsed = collapse_slashes(text);
    let lower = collapsed.to_ascii_lowercase();
    for prefix in DEVICE_PATH_PREFIXES {
        if lower.contains(&prefix.to_ascii_lowercase()) {
            return Some(format!("string literal containing `{prefix}`"));
        }
    }
    let bytes = lower.as_bytes();
    for stem in RESERVED_PORT_STEMS {
        let mut from = 0;
        while let Some(at) = lower[from..].find(stem).map(|at| at + from) {
            from = at + stem.len();
            let before_is_name = at
                .checked_sub(1)
                .is_some_and(|i| bytes[i].is_ascii_alphanumeric());
            let digits = lower[from..].bytes().take_while(u8::is_ascii_digit).count();
            let after = lower[from + digits..].bytes().next();
            let number_ok = digits == 1 && bytes[from] != b'0';
            let after_is_name = after.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'.');
            if !before_is_name && number_ok && !after_is_name {
                return Some(format!(
                    "string literal naming the reserved port `{}`",
                    &collapsed[at..from + digits]
                ));
            }
        }
    }
    None
}

/// Whether a dependency name is a serial-port crate, for example
/// `serialport`, `tokio-serial`, `mio-serial` or `serial2`.
pub fn is_serial_crate(name: &str) -> bool {
    let n = name.replace('_', "-");
    n.contains("serialport")
        || n == "serial"
        || n.starts_with("serial-")
        || n.ends_with("-serial")
        || n.starts_with("serial2")
}

/// Looks up the policy row of a crate.
pub fn crate_policy(name: &str) -> Option<&'static CratePolicy> {
    CRATES.iter().find(|c| c.name == name)
}

/// Transitive closure of `GRAPH_EDGES` plus `EXTRA_EDGES`: for each crate, every crate it
/// reaches.
pub fn reachable() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    let mut direct: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for &(from, to) in GRAPH_EDGES.iter().chain(EXTRA_EDGES) {
        direct.entry(from).or_default().push(to);
    }
    let mut out = BTreeMap::new();
    for c in CRATES {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&str> = direct.get(c.name).cloned().unwrap_or_default();
        while let Some(next) = stack.pop() {
            if seen.insert(next) {
                stack.extend(direct.get(next).into_iter().flatten().copied());
            }
        }
        out.insert(c.name, seen);
    }
    out
}
