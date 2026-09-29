//! `cargo xtask docs`: every surface generated from the command registry.
//!
//! | File | Kind | Surface |
//! |---|---|---|
//! | `docs/commands/<name>.md` | per item, committed | docs and skill |
//! | `docs/schema/commands/<name>.input.json` | per item, committed | CLI, MCP, HTTP, scenario |
//! | `docs/schema/commands/<name>.output.json` | per item, committed | MCP `outputSchema` |
//! | `docs/schema/error@1.json` | committed | the typed error envelope |
//! | `web/src/gen/commands/<name>.d.ts` | per item, committed | browser JS |
//! | `docs/commands/index.md` | aggregate, not committed | command table, HTTP routes, MCP tools |
//! | `docs/errors.md` | aggregate, not committed | error codes |
//! | `web/src/gen/index.d.ts` | aggregate, not committed | `passportEmu.call` typing |
//! | `skills/passportsim/references/commands/<name>.md` | per item, committed | skill |
//! | `skills/passportsim/references/commands.md` | committed | skill command list |
//! | `skills/passportsim/references/errors.md` | committed | skill error codes |
//!
//! Per-item files are committed so a command's pull request touches only its own generated files;
//! aggregates are build outputs `.gitignore` lists. The skill's two lists are the exception: a
//! skill is installed straight from the repository, so everything it reads has to be committed. Every surface carries the full
//! `pemu_api::host_support::TABLE` matrix and never reads `Host::current()`, so the bytes are the
//! same on every host.
//!
//! `--check` writes nothing and fails on a differing file; a CR byte is its own "CRLF found"
//! failure with a fix hint. It checks every file, not only those a pull request touches, so a pull
//! request can fail on another command's drift. Examples are validated against their command's
//! input schema first ([`validate`]).

mod mcp;
mod model;
mod render;
mod tsgen;
mod validate;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use model::Command;

pub use mcp::{CORE_BUDGET_BYTES, list_bytes, tool_bytes, tools_list};
pub use model::{Command as DocCommand, commands as registry_commands};

const USAGE: &str = "usage: cargo xtask docs [--check]";

/// The fix hint printed with every "CRLF found" failure.
const CRLF_HINT: &str = "the repository is LF only (.gitattributes `* text=auto eol=lf`); fix with \
     `git config core.autocrlf false` then \
     `git rm --cached -r . && git reset --hard`, or `git add --renormalize .`";

/// Whether a generated file is committed or an aggregate build output.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Per-item file, committed; `--check` requires it to exist and to match.
    Committed,
    /// Aggregate build output, not committed; `--check` compares it only when it is there.
    Aggregate,
}

/// One file the generators produce.
struct Output {
    /// Path relative to the workspace root, always with forward slashes.
    path: PathBuf,
    contents: String,
    kind: Kind,
}

/// Directories whose files are all generated, scanned for files no command produces any more.
const OWNED_DIRS: &[(&str, &str)] = &[
    ("docs/commands", "md"),
    ("docs/schema/commands", "json"),
    ("web/src/gen/commands", "ts"),
    (SKILL_COMMANDS_DIR, "md"),
];

/// Where the skill's generated references live. Nothing in the skill links outside
/// `skills/passportsim/`, so it keeps its own copy of the command pages and error codes.
const SKILL_REFERENCES_DIR: &str = "skills/passportsim/references";

const SKILL_COMMANDS_DIR: &str = "skills/passportsim/references/commands";

/// Entry point of `cargo xtask docs`.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut check = false;
    for arg in args {
        match arg.as_str() {
            "--check" => check = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    let root = crate::codegen::workspace_root();
    generate(&root, check)
}

/// Renders every surface and writes it, or compares it under `--check`.
fn generate(root: &Path, check: bool) -> Result<(), String> {
    let commands = model::commands()?;
    check_examples(&commands)?;
    warn_unchecked_keywords(&commands);
    let outputs = plan(&commands)?;
    check_no_stale_files(root, &outputs)?;
    write_or_check(root, &outputs, check)
}

/// Every file the current registry produces.
fn plan(commands: &[Command]) -> Result<Vec<Output>, String> {
    let mut outputs = Vec::new();
    for command in commands {
        let name = command.spec.name;
        outputs.push(Output {
            path: PathBuf::from(format!("docs/commands/{name}.md")),
            contents: render::command_page(command),
            kind: Kind::Committed,
        });
        outputs.push(Output {
            path: PathBuf::from(format!("docs/schema/commands/{name}.input.json")),
            contents: json_file(&(command.spec.input_schema)().to_value())?,
            kind: Kind::Committed,
        });
        outputs.push(Output {
            path: PathBuf::from(format!("docs/schema/commands/{name}.output.json")),
            contents: json_file(&(command.spec.output_schema)().to_value())?,
            kind: Kind::Committed,
        });
        outputs.push(Output {
            path: PathBuf::from(format!("web/src/gen/commands/{name}.d.ts")),
            contents: tsgen::command_module(command)?,
            kind: Kind::Committed,
        });
        outputs.push(Output {
            path: PathBuf::from(format!("{SKILL_COMMANDS_DIR}/{name}.md")),
            contents: render::skill_command_page(command),
            kind: Kind::Committed,
        });
    }
    outputs.push(Output {
        path: PathBuf::from("docs/schema/error@1.json"),
        contents: json_file(&pemu_api::error::ApiError::schema().to_value())?,
        kind: Kind::Committed,
    });
    outputs.push(Output {
        path: PathBuf::from("docs/commands/index.md"),
        contents: render::index_page(commands),
        kind: Kind::Aggregate,
    });
    outputs.push(Output {
        path: PathBuf::from("docs/errors.md"),
        contents: render::errors_page(commands),
        kind: Kind::Aggregate,
    });
    outputs.push(Output {
        path: PathBuf::from("web/src/gen/index.d.ts"),
        contents: tsgen::index_module(commands),
        kind: Kind::Aggregate,
    });
    outputs.push(Output {
        path: PathBuf::from(format!("{SKILL_REFERENCES_DIR}/commands.md")),
        contents: render::skill_index_page(commands),
        kind: Kind::Committed,
    });
    outputs.push(Output {
        path: PathBuf::from(format!("{SKILL_REFERENCES_DIR}/errors.md")),
        contents: render::skill_errors_page(),
        kind: Kind::Committed,
    });
    for output in &outputs {
        if output.contents.contains('\r') {
            return Err(format!(
                "generator produced a CR byte in {}: {CRLF_HINT}",
                output.path.display()
            ));
        }
    }
    Ok(outputs)
}

/// A JSON document as a generated file: pretty printed, LF only, one trailing newline.
///
/// `serde_json` keeps object keys sorted (the workspace does not enable `preserve_order`), so two
/// runs of the same registry produce the same bytes.
fn json_file(value: &serde_json::Value) -> Result<String, String> {
    let mut text =
        serde_json::to_string_pretty(value).map_err(|e| format!("cannot serialize schema: {e}"))?;
    text.push('\n');
    Ok(text)
}

/// Validates every registered example against its own command's input schema: examples are
/// executed in CI, so a document must not show one the command would refuse.
fn check_examples(commands: &[Command]) -> Result<(), String> {
    let mut failures = Vec::new();
    for command in commands {
        let schema = (command.spec.input_schema)().to_value();
        for example in command.spec.examples {
            let args: serde_json::Value = serde_json::from_str(example.args).map_err(|e| {
                format!(
                    "command {}: example {:?} args are not JSON: {e}",
                    command.spec.name, example.title
                )
            })?;
            if let Err(problems) = validate::validate(&schema, &args) {
                for problem in problems {
                    failures.push(format!(
                        "  {} example {:?}: {problem}",
                        command.spec.name, example.title
                    ));
                }
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "example(s) do not validate against their own input schema:\n{}",
            failures.join("\n")
        ))
    }
}

/// Prints one warning per schema keyword the validator recognises but does not check, so a command
/// that starts using `pattern` is visible instead of silently unvalidated.
fn warn_unchecked_keywords(commands: &[Command]) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for command in commands {
        let schema = (command.spec.input_schema)().to_value();
        for keyword in validate::unchecked_keywords(&schema) {
            seen.insert(format!("{}: `{keyword}`", command.spec.name));
        }
    }
    for entry in seen {
        println!("docs: UNVERIFIED, the example validator does not check {entry}");
    }
}

/// Fails when a generated directory holds a file no command produces any more, so `--check` covers
/// a renamed or removed command as well as a changed one.
fn check_no_stale_files(root: &Path, outputs: &[Output]) -> Result<(), String> {
    let mut stale = Vec::new();
    for (dir, extension) in OWNED_DIRS {
        let absolute = root.join(dir);
        let Ok(entries) = fs::read_dir(&absolute) else {
            continue;
        };
        for entry in entries {
            let name = entry
                .map_err(|e| format!("{}: {e}", absolute.display()))?
                .file_name();
            let name = name.to_string_lossy().to_string();
            if !name.ends_with(&format!(".{extension}")) {
                continue;
            }
            let relative = PathBuf::from(format!("{dir}/{name}"));
            if !outputs.iter().any(|output| output.path == relative) {
                stale.push(relative.display().to_string());
            }
        }
    }
    if stale.is_empty() {
        Ok(())
    } else {
        stale.sort();
        Err(format!(
            "generated file(s) no command produces any more, delete them: {}",
            stale.join(", ")
        ))
    }
}

/// Writes every output, or under `check` compares and reports.
///
/// Returns the three failure classes separately, because "CRLF found" is its own failure with a
/// fix hint rather than a content diff.
fn write_or_check(root: &Path, outputs: &[Output], check: bool) -> Result<(), String> {
    let mut crlf = Vec::new();
    let mut differs = Vec::new();
    let mut missing = Vec::new();
    let mut stale_aggregates = Vec::new();
    let mut written = 0usize;
    let mut fresh = 0usize;

    for output in outputs {
        let path = root.join(&output.path);
        let current = fs::read(&path).ok();
        if check {
            match current {
                Some(bytes) if bytes.contains(&b'\r') => {
                    crlf.push(output.path.display().to_string());
                }
                Some(bytes) if bytes == output.contents.as_bytes() => fresh += 1,
                Some(_) => match output.kind {
                    Kind::Committed => differs.push(output.path.display().to_string()),
                    // A gitignored aggregate is left behind by every merge and no commit can
                    // bring it up to date, so a stale one is reported, not failed, exactly as a
                    // missing one is: a build output this checkout has not regenerated yet.
                    Kind::Aggregate => stale_aggregates.push(output.path.display().to_string()),
                },
                None => match output.kind {
                    Kind::Committed => missing.push(output.path.display().to_string()),
                    // A fresh checkout has no aggregate file; that is not a failure.
                    Kind::Aggregate => fresh += 1,
                },
            }
            continue;
        }
        if current.as_deref() == Some(output.contents.as_bytes()) {
            fresh += 1;
            continue;
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        replace_file(&path, output.contents.as_bytes())?;
        println!("docs: wrote {}", output.path.display());
        written += 1;
    }

    if !crlf.is_empty() {
        crlf.sort();
        return Err(format!(
            "CRLF found in generated file(s): {}\n{CRLF_HINT}",
            crlf.join(", ")
        ));
    }
    if !missing.is_empty() || !differs.is_empty() {
        let mut stale: Vec<String> = missing
            .into_iter()
            .map(|path| format!("{path} (missing)"))
            .chain(differs)
            .collect();
        stale.sort();
        return Err(format!(
            "generated file(s) out of date, run `cargo xtask docs`: {}",
            stale.join(", ")
        ));
    }
    if check {
        // Named, never failed: a person whose local `docs/errors.md` is behind should be told,
        // and `--check` still writes nothing.
        if !stale_aggregates.is_empty() {
            stale_aggregates.sort();
            println!(
                "docs: build output(s) this checkout has not regenerated, run `cargo xtask docs`: {}",
                stale_aggregates.join(", ")
            );
        }
        println!("docs: {fresh} file(s) up to date");
    } else {
        println!("docs: {written} file(s) written, {fresh} file(s) already up to date");
    }
    Ok(())
}

/// Replaces `path` with `bytes` through a sibling temporary file and a rename.
///
/// `xtask package` may copy `docs/errors.md` and `docs/schema/commands/*.json` while a `docs` or
/// `ci t0` run (or a test thread) rewrites them; a plain `fs::write` truncates first, so a package
/// could hash a half-written file. The rename is atomic, so a reader sees old or new bytes. The
/// temporary name carries the process id and a counter and ends in `.tmp`, which no scan of
/// [`OWNED_DIRS`] and no copy of `xtask package` picks up.
fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| format!("{}: no file name", path.display()))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let replaced = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
    if let Err(e) = replaced {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}
