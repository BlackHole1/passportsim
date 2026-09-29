//! The registry view every generated surface is rendered from.
//!
//! One place turns a [`CommandSpec`] into the names the surfaces use (MCP tool, HTTP route,
//! TypeScript type) and into the **full** host matrix, so no generator can accidentally emit a
//! host-filtered subset: the committed text must be identical on every host, and
//! `Host::current()` is never consulted here.

use pemu_api::host_support::{self, Host, Hosts};
use pemu_api::spec::CommandSpec;

/// Every supported host, in the order every generated column uses.
pub const HOSTS: [Host; 3] = [Host::MacOs, Host::Windows, Host::Browser];

/// One registered command, with everything the generators need beside its spec.
#[derive(Clone, Copy)]
pub struct Command {
    /// The registered spec.
    pub spec: &'static CommandSpec,
    /// The full host matrix of the command: `host_support::TABLE` with the `native_only`
    /// annotation applied, never filtered by the generating host.
    pub hosts: Hosts,
}

impl Command {
    /// The MCP tool name `passport_<name>`.
    pub fn mcp_tool(&self) -> String {
        format!("passport_{}", self.spec.name)
    }

    /// The HTTP route `POST /v1/instances/{id}/commands/<name>`.
    ///
    /// The server has exactly this route (plus the GET aliases `screen.png`, `serial.log` and
    /// `events.ndjson`) and does not split by `needs_instance`. An instance-less form would be an
    /// invented route that a generated table makes binding; the server has to add it first.
    pub fn http_route(&self) -> String {
        format!("POST /v1/instances/{{id}}/commands/{}", self.spec.name)
    }

    /// The TypeScript type name of the arguments, `doctor` becoming `DoctorArgs`.
    pub fn ts_args(&self) -> String {
        format!("{}Args", pascal_case(self.spec.name))
    }

    /// The TypeScript type name of the success payload, `doctor` becoming `DoctorResult`.
    pub fn ts_result(&self) -> String {
        format!("{}Result", pascal_case(self.spec.name))
    }

    /// Whether `host` can run the command.
    pub fn runs_on(&self, host: Host) -> bool {
        self.hosts.has(host)
    }
}

/// The full host matrix of one command: the `host_support::TABLE` row, if any, with `native_only`
/// applied (the annotation is the browser/native split, the table is the two native hosts).
/// `check_table` already rejects a row that claims the browser for a `native_only` command, so
/// the two never disagree in the other direction.
pub fn hosts_of(spec: &CommandSpec) -> Hosts {
    let mut hosts = host_support::hosts(spec.name);
    if spec.annotations.native_only {
        hosts.browser = false;
    }
    hosts
}

/// Every registered command in name order, which is the order every generated surface uses:
/// `tools/list` comes back in a fixed order, so client prompt caches stay valid, and the documents
/// stay stable.
///
/// Fails when the registry does not pass its own checks, so no surface is written from a registry
/// `pemu_api::registry::check` rejects.
pub fn commands() -> Result<Vec<Command>, String> {
    let specs = pemu_api::registry::commands();
    from_specs(specs)
}

/// [`commands`] over an explicit list, so a test can render a synthetic registry.
pub fn from_specs(specs: &'static [CommandSpec]) -> Result<Vec<Command>, String> {
    from_specs_with_rows(specs, host_support::TABLE)
}

/// [`from_specs`] against explicit host-support rows: every row must name a command of `specs`,
/// so a synthetic registry is checked against the rows written for it.
pub fn from_specs_with_rows(
    specs: &'static [CommandSpec],
    rows: &[host_support::Support],
) -> Result<Vec<Command>, String> {
    pemu_api::registry::check(specs).map_err(|e| format!("command registry: {e}"))?;
    host_support::check_rows(rows, specs).map_err(|e| format!("host support table: {e}"))?;
    let mut commands: Vec<Command> = specs
        .iter()
        .map(|spec| Command {
            spec,
            hosts: hosts_of(spec),
        })
        .collect();
    commands.sort_by_key(|c| c.spec.name);
    Ok(commands)
}

/// `doctor` to `Doctor`, `net_http` to `NetHttp`: the TypeScript and Markdown spelling of a
/// registry name (names are `[a-z][a-z0-9_]*`, so only `_` splits words).
pub fn pascal_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for word in name.split('_') {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// The annotation names of one command, in declaration order, for the documents and the CLI help.
pub fn annotation_names(spec: &CommandSpec) -> Vec<&'static str> {
    let a = spec.annotations;
    let all = [
        ("read_only", a.read_only),
        ("destructive", a.destructive),
        ("idempotent", a.idempotent),
        ("advances_time", a.advances_time),
        ("needs_instance", a.needs_instance),
        ("native_only", a.native_only),
        ("human_confirm", a.human_confirm),
    ];
    all.iter()
        .filter(|(_, set)| *set)
        .map(|(name, _)| *name)
        .collect()
}
