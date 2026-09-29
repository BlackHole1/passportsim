//! `cargo xtask`: developer and local CI task runner. Each subcommand lives in its own module
//! as `run(args) -> Result<(), String>`; an error exits with status 2.

mod agent_budget;
mod bench;
mod bench_k;
mod blob_symbols;
mod ci;
mod codegen;
mod comment_check;
mod docs;
mod goldens;
mod hostdirs;
#[cfg(test)]
mod hostdirs_pins;
mod layering;
mod manifest_util;
mod mcp_size;
mod onetest;
mod oracle;
mod package;
mod portable;
mod probes;
mod provenance;
mod riscv_tests;
mod secrets;
mod util;
mod wasm;

use std::process::ExitCode;

/// Subcommands accepted by `cargo xtask`, in usage order.
const SUBCOMMANDS: &[&str] = &[
    "codegen",
    "comment-check",
    "layering",
    "provenance",
    "secrets-check",
    "hooks",
    "oracle",
    "probes",
    "riscv-tests",
    "bench",
    "bench-k",
    "bench-browser",
    "wasm",
    "docs",
    "mcp-size",
    "agent-budget",
    "ci",
    "package",
    "portable",
    "test",
];

fn usage() -> String {
    format!(
        "usage: cargo xtask <subcommand> [args]\nsubcommands: {}",
        SUBCOMMANDS.join(", ")
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((name, rest)) = args.split_first() else {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };
    let result = match name.as_str() {
        "codegen" => codegen::run(rest),
        "comment-check" => comment_check::run(rest),
        "layering" => layering::run(rest),
        "provenance" => provenance::run(rest),
        "secrets-check" => secrets::run(rest),
        "hooks" => secrets::run_hooks(rest),
        "oracle" => oracle::run(rest),
        "probes" => probes::run(rest),
        "riscv-tests" => riscv_tests::run(rest),
        "bench" => bench::run(rest),
        "bench-k" => bench_k::run(rest),
        "bench-browser" => bench::browser::run(rest),
        "wasm" => wasm::run(rest),
        "docs" => docs::run(rest),
        "mcp-size" => mcp_size::run(rest),
        "agent-budget" => agent_budget::run(rest),
        "ci" => ci::run(rest),
        "package" => package::run(rest),
        "portable" => portable::run(rest),
        "test" => onetest::run(rest),
        "help" | "-h" | "--help" => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        other => {
            eprintln!("xtask: unknown subcommand `{other}`\n{}", usage());
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask {name}: {err}");
            ExitCode::from(2)
        }
    }
}
