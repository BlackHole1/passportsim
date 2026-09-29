//! `cargo xtask mcp-size`: the MCP tool-list size budget.
//!
//! Every core tool is in the default `tools/list` payload of every session, so the payload is a
//! fixed cost every agent pays; this catches a schema that grows unnoticed (budget UNVERIFIED). The
//! measurement is the compact JSON of [`crate::docs::tools_list`], in bytes. It is the same on
//! every host, because the tool objects carry the host matrix as an annotation instead of being
//! filtered.
//!
//! Over budget, the failure names the largest tool, which is the one to shrink:
//!
//! ```text
//! xtask mcp-size: core tool list is 25289 bytes, over the 24576-byte budget by 713;
//! the largest tool is `passport_start` at 4102 bytes
//! ```
//!
//! `--json` prints the payload itself, so an MCP server or a test can diff what is measured.
//!
//! `--caps LIST` (the `passportsim mcp --caps` spelling) also measures the list a client with those
//! opt-in groups receives, core included, and prints it on a second line. Only the core list is
//! gated: the budget covers the always-on payload every session pays, and a caps group is what a
//! client opts into. With `--json` the printed payload is that
//! enabled list.

use std::collections::BTreeSet;

use pemu_api::spec::CapsGroup;

use crate::docs::{self, DocCommand};

const USAGE: &str = "usage: cargo xtask mcp-size [--json] [--caps LIST]";

/// Entry point of `cargo xtask mcp-size`.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut print_json = false;
    let mut caps: Option<BTreeSet<CapsGroup>> = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => print_json = true,
            "--caps" => {
                let list = args
                    .next()
                    .ok_or_else(|| format!("`--caps` needs a list\n{USAGE}"))?;
                caps = Some(parse_caps(list)?);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    let commands = docs::registry_commands()?;
    let enabled = |groups: &BTreeSet<CapsGroup>| -> Vec<DocCommand> {
        commands
            .iter()
            .copied()
            .filter(|command| groups.contains(&command.spec.group))
            .collect()
    };
    let core = enabled(&BTreeSet::from([CapsGroup::Core]));
    let with_caps = caps.as_ref().map(&enabled);
    if print_json {
        let payload = docs::tools_list(with_caps.as_deref().unwrap_or(&core));
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|e| format!("cannot serialize the tool list: {e}"))?
        );
    }
    let report = measure(&core, docs::CORE_BUDGET_BYTES);
    println!("{}", report.line());
    if let (Some(groups), Some(list)) = (&caps, &with_caps) {
        println!("{}", caps_line(groups, list, report.bytes));
    }
    report.into_result()
}

/// The groups a `--caps` list names, Core always among them (the `passportsim mcp --caps` rule).
///
/// The reader itself is [`CapsGroup::parse_list`], shared with `pemu_host::mcp_stdio::parse_caps`,
/// so this report and the served tool list can never read a list differently.
pub fn parse_caps(list: &str) -> Result<BTreeSet<CapsGroup>, String> {
    CapsGroup::parse_list(list)
}

/// The informational line of `--caps`: the enabled list's size and what the groups add to core.
pub fn caps_line(groups: &BTreeSet<CapsGroup>, list: &[DocCommand], core_bytes: usize) -> String {
    let names: Vec<&str> = groups.iter().map(|g| g.caps_name()).collect();
    let bytes = docs::list_bytes(list);
    format!(
        "tool list with --caps {} is {bytes} bytes in {} tool(s), {} bytes over core (opt-in, not gated)",
        names.join(","),
        list.len(),
        bytes.saturating_sub(core_bytes)
    )
}

/// What one measurement found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Tools measured.
    pub tools: usize,
    /// Compact JSON bytes of the whole `tools/list` payload.
    pub bytes: usize,
    /// The budget it was compared against.
    pub budget: usize,
    /// The largest tool and its own compact JSON size, when there is one.
    pub largest: Option<(String, usize)>,
}

impl Report {
    /// The one line `mcp-size` prints, over budget or not.
    pub fn line(&self) -> String {
        let largest = match &self.largest {
            Some((name, bytes)) => format!("; the largest tool is `{name}` at {bytes} bytes"),
            None => String::new(),
        };
        if self.bytes > self.budget {
            format!(
                "core tool list is {} bytes in {} tool(s), over the {}-byte budget by {}{largest}",
                self.bytes,
                self.tools,
                self.budget,
                self.bytes - self.budget
            )
        } else {
            format!(
                "core tool list is {} bytes in {} tool(s), within the {}-byte budget ({} left){largest}",
                self.bytes,
                self.tools,
                self.budget,
                self.budget - self.bytes
            )
        }
    }

    /// Whether the list fits.
    pub fn within_budget(&self) -> bool {
        self.bytes <= self.budget
    }

    /// `Ok` inside the budget, the failure line otherwise.
    pub fn into_result(self) -> Result<(), String> {
        if self.within_budget() {
            Ok(())
        } else {
            Err(self.line())
        }
    }
}

/// Measures `commands` as one `tools/list` payload against `budget`.
///
/// Taking the list and the budget as arguments is what lets a test measure a synthetic
/// over-budget registry without registering commands.
pub fn measure(commands: &[DocCommand], budget: usize) -> Report {
    let largest = commands
        .iter()
        .map(|command| (command.mcp_tool(), docs::tool_bytes(command)))
        .max_by_key(|(_, bytes)| *bytes);
    Report {
        tools: commands.len(),
        bytes: docs::list_bytes(commands),
        budget,
        largest,
    }
}
