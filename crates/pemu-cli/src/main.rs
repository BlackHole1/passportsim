//! `passportsim`: one binary whose subcommands are the command registry. Nothing here knows a
//! command by name: the clap tree, flags, arguments, text and exit code all come from the
//! `CommandSpec`, so a new `#[command]` is a new subcommand with no edit here.
//!
//! One invocation:
//!
//! 1. argv is refused with `E_USAGE` if any argument is not UTF-8, before clap sees it, so a lossy
//!    conversion never names a file the user did not mean;
//! 2. clap parses the generated tree;
//! 3. [`args::assemble`] turns the matches and the `--json` document into one JSON object;
//! 4. [`install_host_facts`] hands the pool the artifact root and a clock;
//! 5. the handler runs against the process-wide instance pool, unless [`forward`] sends the call to
//!    the daemon (spawning one for `start`; `--ephemeral` keeps it here);
//! 6. [`render`] writes UTF-8 bytes and picks the exit code.
//!
//! `passportsim serve` ([`serve`]) is the daemon and `passportsim mcp` ([`mcp`]) its MCP stdio
//! adapter. Neither is a registry command, so neither has an input schema or a page under
//! `docs/commands/`.

mod args;
mod doctor;
mod exit;
mod forward;
mod glob;
mod json;
mod mcp;
mod paths;
mod payload;
mod render;
mod serve;
mod tree;

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::Instant;

use pemu_api::error::{ApiError, E_HOST_UNSUPPORTED, E_USAGE};
use pemu_api::host_support::{self, Host};
use pemu_api::receipt::{Receipt, Strictness, Verdict, exit_code};
use pemu_api::registry;
use pemu_api::spec::{CommandSpec, HandlerCx};

use crate::args::{HostJson, JsonSource};
use crate::json::Value;
use crate::render::{Mode, Stream};

/// A pure function of argv and the JSON source, so a test can drive the whole binary without a
/// process.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub code: u8,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    fn refused(error: &ApiError, mode: Mode) -> Outcome {
        let (text, stream) = render::failure(error, mode);
        let code = exit::of_error(error);
        match stream {
            Stream::Stdout => Outcome {
                code,
                stdout: text,
                stderr: String::new(),
            },
            Stream::Stderr => Outcome {
                code,
                stdout: String::new(),
                stderr: text,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// The product: forward to the daemon, install the real machine factory for what runs here, and
    /// run `serve` and `mcp`.
    Host,
    /// No machine factory and no daemon: the CLI's own tests, which must never reach a user's
    /// daemon.
    InProcess,
}

fn main() -> ExitCode {
    let outcome = run(
        &std::env::args_os().collect::<Vec<OsString>>(),
        &mut HostJson,
        Policy::Host,
    );
    if !outcome.stdout.is_empty() {
        render::write(&outcome.stdout, Stream::Stdout);
    }
    if !outcome.stderr.is_empty() {
        render::write(&outcome.stderr, Stream::Stderr);
    }
    ExitCode::from(outcome.code)
}

fn run(argv: &[OsString], source: &mut dyn JsonSource, policy: Policy) -> Outcome {
    // Read before the arguments parse, so a refusal of argv itself is still a JSON envelope when
    // asked for.
    let mode = mode_of(argv);
    let text = match utf8(argv) {
        Ok(text) => text,
        Err(error) => return Outcome::refused(&error, mode),
    };
    let matches = match tree::build().try_get_matches_from(&text) {
        Ok(matches) => matches,
        Err(error) => return clap_outcome(&error),
    };
    let Some((name, sub)) = matches.subcommand() else {
        // Unreachable with `subcommand_required`; a refusal rather than a panic.
        return Outcome::refused(
            &ApiError::new(E_USAGE, "no command; `passportsim --help` lists them"),
            mode,
        );
    };
    match (name, policy) {
        (tree::SERVE, Policy::Host) => return serve::run(sub),
        (tree::MCP, Policy::Host) => return mcp::run(sub),
        (tree::SERVE | tree::MCP, Policy::InProcess) => {
            return Outcome::refused(
                &ApiError::new(E_USAGE, format!("`{name}` runs only in the real binary")),
                mode,
            );
        }
        _ => {}
    }
    let Some(spec) = registry::find_with_alias(name) else {
        return Outcome::refused(
            &ApiError::new(E_USAGE, format!("`{name}` is no command of this build")),
            mode,
        );
    };
    // A flag after the command name wins over the same flag before it.
    let mode = sub
        .get_one::<String>(tree::OUTPUT_ARG)
        .or_else(|| matches.get_one::<String>(tree::OUTPUT_ARG))
        .and_then(|text| Mode::parse(text))
        .unwrap_or(mode);
    let strict = if sub.get_flag(tree::STRICT_ARG) || matches.get_flag(tree::STRICT_ARG) {
        Strictness::Strict
    } else {
        Strictness::Lenient
    };
    if let Err(error) = host_check(spec) {
        return Outcome::refused(&error, mode);
    }
    let mut call = match args::assemble(spec, sub, source) {
        Ok(call) => call,
        Err(error) => return Outcome::refused(&error, mode),
    };
    if spec.name == "start"
        && let Ok(cwd) = std::env::current_dir()
    {
        absolutize_fw(&mut call, &cwd);
    }
    // Discovery stays out of `pemu-api`, so the CLI runs it and hands the report in. A person types
    // `passportsim doctor` and nothing else.
    if spec.name == "doctor" {
        doctor::fill_report(&mut call, &paths::resolver(None), &payload::this_process());
    }
    let artifacts = sub
        .get_one::<String>(tree::ARTIFACTS_ARG)
        .or_else(|| matches.get_one::<String>(tree::ARTIFACTS_ARG));
    let ephemeral = sub.get_flag(tree::EPHEMERAL_ARG) || matches.get_flag(tree::EPHEMERAL_ARG);

    // Checked before a call can leave the process, so a malformed id is the same `E_USAGE` envelope
    // here and in the daemon.
    if let Some(text) = call.get("instance").and_then(Value::as_str)
        && let Err(error) = pemu_api::instance::InstanceId::parse(text)
    {
        return Outcome::refused(&error, mode);
    }
    if policy == Policy::Host && !ephemeral {
        if let Some(error) = tainted_start_needs_ephemeral(spec.name, &call) {
            return Outcome::refused(&error, mode);
        }
        let is_start = spec.name == "start";
        // `start` is the one command that spawns a daemon, so a usage error must not leave one
        // behind.
        if is_start && let Err(error) = pemu_api::commands::start::StartArgs::from_json(&call) {
            return Outcome::refused(&error, mode);
        }
        if is_start || forward::addresses_instance(spec) {
            let paths = paths::resolver(None);
            let store = match forward::store(&paths) {
                Ok(store) => store,
                Err(why) => return forward::infra(&why, mode),
            };
            let daemon = if is_start {
                forward::ensure(&store).map(Some)
            } else {
                forward::running(&store)
            };
            match daemon {
                Err(why) => return forward::infra(&why, mode),
                Ok(Some(discovery)) => {
                    if artifacts.is_some() {
                        return Outcome::refused(
                            &ApiError::new(
                                E_USAGE,
                                "`--artifacts` moves the artifacts of this process, and this \
                                 command runs in the daemon",
                            )
                            .with_hint(
                                "start the daemon with `passportsim serve --artifacts <dir>`, or \
                                 add `--ephemeral` to run this command here",
                            ),
                            mode,
                        );
                    }
                    // A daemon resolves no relative scenario path; here the working directory does.
                    if spec.name == "scenario"
                        && let Ok(cwd) = std::env::current_dir()
                    {
                        absolutize_scenario(&mut call, &cwd);
                    }
                    return match forward::call(&discovery, spec.name, &call) {
                        Ok(result) => forward::outcome(spec.name, &result, mode, strict),
                        Err(why) => forward::infra(&why, mode),
                    };
                }
                // No daemon, so this process answers.
                Ok(None) => {}
            }
        }
    }
    // A flag after the command name wins, as `--output` does above.
    let allow_device = (sub.get_flag(tree::ALLOW_DEVICE_ARG)
        || matches.get_flag(tree::ALLOW_DEVICE_ARG))
    .then_some(pemu_api::commands::plan_flash::DeviceCaller::Cli);
    // The same rule for `--esptool`, so the refusal telling a person to pass it can be answered
    // where they stand.
    let esptool = sub
        .get_one::<String>(tree::ESPTOOL_ARG)
        .or_else(|| matches.get_one::<String>(tree::ESPTOOL_ARG));
    let warning = install_host_facts(
        artifacts.map(String::as_str),
        policy,
        allow_device,
        esptool.map(String::as_str),
    );

    // A command running here runs its instance on this thread, so it asks for the machine threads'
    // class.
    if policy == Policy::Host {
        pemu_host::platform::machine_thread();
    }
    let mut outcome = match (spec.handler)(&mut HandlerCx {}, call) {
        Ok(output) => Outcome {
            code: success_code(spec.name, &output.receipt, &output.json, strict),
            stdout: render::success(&output, mode),
            stderr: String::new(),
        },
        Err(error) => Outcome::refused(&error, mode),
    };
    if let Some(warning) = warning {
        outcome.stderr.insert_str(0, &warning);
    }
    outcome
}

/// A tainted instance must not live in the shared daemon that `/mcp` and HTTP reach, so a tainting
/// `start` (`efuse_dump` or `allow_tainted`) is refused here with the `--ephemeral` hint. The test
/// below keeps the list complete.
fn tainted_start_needs_ephemeral(command: &str, call: &Value) -> Option<ApiError> {
    if command != "start" {
        return None;
    }
    let asks_for_taint = call.get("efuse_dump").is_some()
        || call.get("allow_tainted").and_then(Value::as_bool) == Some(true);
    asks_for_taint.then(|| {
        ApiError::new(
            E_USAGE,
            "this load taints the instance, and a tainted instance does not live in the shared \
             daemon, which MCP and HTTP reach",
        )
        .with_hint("add `--ephemeral`, which runs the instance in this process alone")
    })
}

/// Neither a Windows (UTF-16) nor a macOS (bytes) command line guarantees UTF-8; refusing here
/// keeps a half-converted path from reaching a file system call.
fn utf8(argv: &[OsString]) -> Result<Vec<String>, ApiError> {
    let mut out = Vec::with_capacity(argv.len().max(1));
    if argv.is_empty() {
        out.push("passportsim".to_owned());
    }
    for (index, arg) in argv.iter().enumerate() {
        let Some(text) = arg.to_str() else {
            return Err(ApiError::new(
                E_USAGE,
                format!(
                    "argument {index} is not valid UTF-8: `{}`",
                    arg.to_string_lossy()
                ),
            )
            .with_hint(
                "passportsim reads and writes UTF-8 only; rename the file, or pass \
                 its path in a `--json` document written as UTF-8",
            ));
        };
        out.push(text.to_owned());
    }
    Ok(out)
}

/// Both clap spellings are read, so `--output=json` and `--output json` behave alike. A non-UTF-8
/// argument cannot be the mode and is left for [`utf8`] to refuse.
fn mode_of(argv: &[OsString]) -> Mode {
    let mut mode = Mode::default();
    let mut expect_value = false;
    for arg in argv {
        let Some(text) = arg.to_str() else {
            expect_value = false;
            continue;
        };
        if expect_value {
            mode = Mode::parse(text).unwrap_or(mode);
            expect_value = false;
        } else if let Some(value) = text.strip_prefix("--output=") {
            mode = Mode::parse(value).unwrap_or(mode);
        } else if text == "--output" {
            expect_value = true;
        }
    }
    mode
}

/// `--help` and `--version` go to stdout with exit 0, because a caller pipes them. Everything else
/// is `E_USAGE`, exit 2, on stderr, with clap's message naming the argument.
fn clap_outcome(error: &clap::Error) -> Outcome {
    use clap::error::ErrorKind;
    let text = error.render().to_string();
    let text = if text.ends_with('\n') {
        text
    } else {
        format!("{text}\n")
    };
    match error.kind() {
        ErrorKind::DisplayHelp => Outcome {
            code: exit::PASS,
            stdout: text,
            stderr: String::new(),
        },
        // `--version` also says where the payload comes from: embedded, the verified directory
        // beside the binary, or none and why. Resolved here because a build without an embedded
        // copy hashes the directory, which no other invocation should pay for.
        ErrorKind::DisplayVersion => Outcome {
            code: exit::PASS,
            stdout: format!(
                "{text}{}\n",
                payload::Payload::describe(&payload::this_process())
            ),
            stderr: String::new(),
        },
        // No command: the help is what the caller needs, but it is still a usage error.
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => Outcome {
            code: exit::USAGE,
            stdout: text,
            stderr: String::new(),
        },
        _ => Outcome {
            code: exit::USAGE,
            stdout: String::new(),
            stderr: text,
        },
    }
}

/// Applies when `fw` holds a path separator or starts with `.`. The daemon refuses a relative path,
/// but the CLI knows the caller's working directory, so `passportsim start ./build/merged.bin`
/// means what it says either way. A bare word such as `pk` is a corpus id and is left alone.
fn absolutize_fw(call: &mut Value, cwd: &std::path::Path) {
    let Some(fw) = call.get("fw").and_then(Value::as_str) else {
        return;
    };
    let path = std::path::Path::new(fw);
    let pathlike = fw.contains(['/', '\\']) || fw.starts_with('.');
    if !pathlike || path.is_absolute() {
        return;
    }
    if let Some(text) = std::path::absolute(cwd.join(path))
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
    {
        call["fw"] = Value::String(text);
    }
}

/// A daemon resolves no relative path, so a forwarded pattern is joined to the directory it was
/// typed in. `Path::join` applies each host's rule (a Windows root-relative `\abs\a.yaml` takes the
/// working directory's drive), and on Windows the result is forward-slashed because the pattern
/// grammar uses `/`.
fn absolutize_scenario(call: &mut Value, cwd: &std::path::Path) {
    let Some(file) = call.get("file").and_then(Value::as_str) else {
        return;
    };
    if std::path::Path::new(file).is_absolute() {
        return;
    }
    if let Some(joined) = cwd.join(file).to_str() {
        let joined = if cfg!(windows) {
            joined.replace('\\', "/")
        } else {
            joined.to_owned()
        };
        call["file"] = Value::String(joined);
    }
}

/// By the host support table.
fn host_check(spec: &CommandSpec) -> Result<(), ApiError> {
    let Some(host) = Host::current() else {
        // A host outside the support matrix: the table cannot speak for it, so nothing is refused.
        return Ok(());
    };
    if host_support::hosts(spec.name).has(host) {
        return Ok(());
    }
    let error = ApiError::new(
        E_HOST_UNSUPPORTED,
        format!("`{}` does not run on {}", spec.name, host.as_str()),
    );
    Err(match host_support::hint(spec.name) {
        Some(hint) => error.with_hint(hint),
        None => error,
    })
}

/// The workspace the command was typed in, or the directory itself outside one. Never the data
/// root: backups live under it, so every flash would fail after the device was already opened and
/// reset. `serve` uses this too.
fn device_repository() -> std::path::PathBuf {
    repository_of(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
}

/// So a test can pin the rule without changing the working directory.
fn repository_of(cwd: std::path::PathBuf) -> std::path::PathBuf {
    pemu_host::hooks::workspace_root(&cwd).unwrap_or(cwd)
}

/// The home-redacted artifact root and a monotonic host clock (which makes `wall_budget_ms`
/// enforceable). Neither can be a command argument, and a core crate stays off the file system and
/// the clock. Under [`Policy::Host`] it also installs the machine factory; when the data root
/// cannot be resolved, the returned warning says why.
fn install_host_facts(
    artifacts: Option<&str>,
    policy: Policy,
    allow_device: Option<pemu_api::commands::plan_flash::DeviceCaller>,
    esptool: Option<&str>,
) -> Option<String> {
    let paths = paths::resolver(artifacts);
    // This process is a person at a terminal, so it may answer a tainted load. `serve` and `mcp`
    // return before this function, so a daemon and the stdio relay never allow it, and neither does
    // the browser.
    pemu_api::commands::start::allow_tainted_loads();
    // Without `--allow-device` nothing is installed and every Device command refuses, so no build
    // can reach an attached Passport by accident.
    if let Some(caller) = allow_device
        && let Ok(data_root) = paths.data_root()
    {
        pemu_api::commands::plan_flash::install(
            caller,
            Box::new(pemu_host::device::HostPlanner::new(
                &data_root,
                &device_repository(),
                esptool,
            )),
        );
    }
    let root = paths::artifacts_root(&paths);
    let mut warning = None;
    if policy == Policy::Host {
        // For a command that runs here: `--ephemeral`, or no daemon to address.
        let env = pemu_host::assets::HostEnv::from_paths(&paths);
        match pemu_host::audio_root::AudioRoot::from_paths(&paths) {
            Ok(audio) => {
                pemu_host::backend::install(
                    // This binary's payload is a firmware source too, so an in-process `start`
                    // boots the bundled demo as the daemon does.
                    pemu_host::backend::product_source_with_bundles(
                        env.clone(),
                        payload::firmware_bundles(payload::this_process()),
                    ),
                    root,
                    audio,
                );
                // Scenario paths are relative to the directory the command was typed in and read
                // under its workspace.
                let cwd = std::env::current_dir().ok();
                // Outside any workspace that directory stands in for one: this process reads files
                // for the person at the keyboard only.
                let roots = cwd
                    .as_deref()
                    .map(|cwd| pemu_host::hooks::workspace_root(cwd).unwrap_or(cwd.to_path_buf()))
                    .into_iter()
                    .collect();
                pemu_host::hooks::install(
                    pemu_host::hooks::HostHooks::product(env, cwd, roots, paths.config().ok())
                        // When no corpus names an app ELF, the walkers and the settle point read
                        // the bundled one.
                        .with_payload_bundles(payload::firmware_bundles(payload::this_process())),
                );
                pemu_host::boot_cache::install(paths.cache().ok());
                return None;
            }
            Err(error) => warning = Some(no_factory_warning(&error)),
        }
    }
    pemu_api::commands::start::with_pool(|pool| {
        pool.set_artifacts_root(root);
        pool.set_host_clock(Some(host_ms));
    });
    warning
}

fn no_factory_warning(error: &pemu_host::paths::PathError) -> String {
    format!(
        "warning: the data root is not resolvable ({error}), so this process builds no machine; \
         set PASSPORTSIM_HOME or PASSPORTSIM_DATA_ROOT\n"
    )
}

/// Since this process started. A duration past 584 million years saturates.
fn host_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// `fail` fails every command. Caveats count only for [`exit::carries_a_verdict`]: a `status` or a
/// `serial read` carries the same caveats, and exiting 10 for reading a console tells a shell
/// nothing it can act on.
fn success_code(command: &str, receipt: &Receipt, json: &Value, strict: Strictness) -> u8 {
    let holds = assertions_hold(json);
    if let Some(code) = exit::of_scenario_payload(command, json, strict) {
        return code;
    }
    if exit::carries_a_verdict(command) {
        return exit_code(receipt.verdict(holds), &receipt.caveats(), strict);
    }
    exit_code(
        if holds { Verdict::Pass } else { Verdict::Fail },
        &[],
        strict,
    )
}

/// Only a command that judges something reports a `result`.
fn assertions_hold(json: &Value) -> bool {
    match json.get("result").and_then(Value::as_str) {
        Some(result) => result != "fail",
        None => true,
    }
}

#[cfg(test)]
fn argv(list: &[&str]) -> Vec<OsString> {
    std::iter::once(OsString::from("passportsim"))
        .chain(list.iter().map(OsString::from))
        .collect()
}

/// So no CLI test writes a file.
#[cfg(test)]
#[derive(Default)]
struct Memory {
    stdin: String,
}

#[cfg(test)]
impl JsonSource for Memory {
    fn stdin(&mut self) -> Result<String, String> {
        Ok(self.stdin.clone())
    }

    fn file(&mut self, path: &std::path::Path) -> Result<String, String> {
        Err(format!("{}: no such file", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_repository_comes_from_the_working_directory() {
        let temp =
            std::env::temp_dir().join(format!("pemu-cli-repo-{}-{}", std::process::id(), line!()));
        let data_root = temp.join("data-root");
        let work = temp.join("work");
        std::fs::create_dir_all(&data_root).expect("data root");
        std::fs::create_dir_all(&work).expect("work");
        // Outside a workspace the directory itself stands in, and it is not the data root.
        let repository = repository_of(work.clone());
        assert_eq!(repository, work);
        assert!(!data_root.starts_with(&repository));
        // Inside a workspace, its root: still not the data root, nor an ancestor of it.
        std::fs::write(work.join(pemu_host::hooks::WORKSPACE_MARKER), "").expect("marker");
        let inner = work.join("crates").join("thing");
        std::fs::create_dir_all(&inner).expect("inner");
        assert_eq!(repository_of(inner), work);
        assert!(!data_root.starts_with(repository_of(work.clone())));
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// `status` is the one command that answers before anything is started; everything else needs a
    /// machine, which a plain build refuses.
    fn status(extra: &[&str]) -> Outcome {
        let mut list = vec!["status"];
        list.extend_from_slice(extra);
        run(&argv(&list), &mut Memory::default(), Policy::InProcess)
    }

    /// The pool is process-wide and this binary installs no machine factory, so it stays empty for
    /// every test and the empty-pool rendering is exact. If a factory is ever installed here, the
    /// tests need a pool of their own.
    #[test]
    fn a_command_prints_its_text_with_the_one_line_receipt_and_exits_zero() {
        let outcome = status(&[]);
        assert_eq!(outcome.code, exit::PASS, "{outcome:?}");
        assert!(outcome.stderr.is_empty(), "{}", outcome.stderr);
        let lines: Vec<&str> = outcome.stdout.trim_end().split('\n').collect();
        assert_eq!(lines[0], "no instance is running", "{outcome:?}");
        assert!(lines.last().expect("a receipt line").contains("profile"));
    }

    #[test]
    fn output_json_is_one_object_with_the_receipt_and_the_artifact_root() {
        // Per process, so parallel copies of this test never collide.
        let root = std::env::temp_dir().join(format!(
            "pemu-cli-artifacts-{}-{}",
            std::process::id(),
            line!()
        ));
        let root = root.to_str().expect("a UTF-8 temp path");
        let outcome = status(&["--output", "json", "--artifacts", root]);
        assert_eq!(outcome.code, exit::PASS, "{outcome:?}");
        let value: Value =
            serde_json::from_str(outcome.stdout.trim_end()).expect("one JSON object");
        assert!(value["instances"].is_array());
        assert!(value["receipt"].is_object());
        // Native form after home redaction: the temp directory is usually outside `HOME` on macOS,
        // so it comes back as given, and under the profile on Windows, so it comes back as
        // `~\AppData\...`.
        let home = pemu_host::paths::HostPaths::from_process().home().ok();
        let expected = paths::redact(std::path::Path::new(root), home.as_deref());
        assert_eq!(
            value["artifacts_root"], expected,
            "`status` reports the root the CLI resolved"
        );
    }

    #[test]
    fn an_unknown_flag_is_usage_on_stderr() {
        let outcome = run(
            &argv(&["status", "--nonsense"]),
            &mut Memory::default(),
            Policy::InProcess,
        );
        assert_eq!(outcome.code, exit::USAGE);
        assert!(outcome.stdout.is_empty(), "{}", outcome.stdout);
        assert!(outcome.stderr.contains("--nonsense"), "{}", outcome.stderr);
    }

    #[test]
    fn a_refusal_is_a_sentence_in_text_and_an_envelope_in_json() {
        let outcome = status(&["q1"]);
        assert_eq!(outcome.code, exit::USAGE, "{outcome:?}");
        assert!(outcome.stderr.starts_with("error: E_USAGE"), "{outcome:?}");

        let outcome = status(&["q1", "--output", "json"]);
        assert_eq!(outcome.code, exit::USAGE);
        let value: Value =
            serde_json::from_str(outcome.stdout.trim_end()).expect("the error envelope");
        assert_eq!(value["code"], "E_USAGE");
    }

    #[test]
    fn the_json_document_is_read_from_standard_input() {
        let mut source = Memory {
            stdin: r#"{"instance":"p1"}"#.to_owned(),
        };
        let outcome = run(
            &argv(&["status", "--json", "-"]),
            &mut source,
            Policy::InProcess,
        );
        // No `p1` exists in a fresh process, so the unknown-instance refusal proves the document
        // reached the handler.
        assert_eq!(outcome.code, exit::FAIL, "{outcome:?}");
        assert!(outcome.stderr.contains("p1"), "{}", outcome.stderr);
    }

    /// A lone surrogate in a Windows command line has the same shape. Only the Unix byte spelling
    /// can be built here, so the Windows side is unverified.
    #[cfg(unix)]
    #[test]
    fn an_argument_that_is_not_utf8_is_refused_with_e_usage() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let bad = OsStr::from_bytes(&[0x2d, 0x2d, 0x6a, 0x73, 0x6f, 0x6e, 0xff]).to_owned();
        let outcome = run(
            &[OsString::from("passportsim"), OsString::from("status"), bad],
            &mut Memory::default(),
            Policy::InProcess,
        );
        assert_eq!(outcome.code, exit::USAGE, "{outcome:?}");
        assert!(
            outcome.stderr.contains("not valid UTF-8"),
            "{}",
            outcome.stderr
        );
        assert!(std::str::from_utf8(outcome.stderr.as_bytes()).is_ok());
    }

    #[test]
    fn help_is_stdout_and_exits_zero_while_no_command_is_usage() {
        let outcome = run(
            &argv(&["--help"]),
            &mut Memory::default(),
            Policy::InProcess,
        );
        assert_eq!(outcome.code, exit::PASS);
        assert!(outcome.stdout.contains("passportsim"), "{outcome:?}");
        assert!(!outcome.stdout.contains(".exe"), "{outcome:?}");

        let outcome = run(&argv(&[]), &mut Memory::default(), Policy::InProcess);
        assert_eq!(outcome.code, exit::USAGE, "{outcome:?}");
        assert!(
            !outcome.stdout.is_empty(),
            "the help is what a caller needs"
        );
    }

    #[test]
    fn every_registered_command_is_reachable_by_name_and_by_alias() {
        for spec in registry::commands() {
            let outcome = run(
                &argv(&[spec.name, "--help"]),
                &mut Memory::default(),
                Policy::InProcess,
            );
            assert_eq!(outcome.code, exit::PASS, "{}: {outcome:?}", spec.name);
            for alias in spec.cli.aliases {
                let outcome = run(
                    &argv(&[alias, "--help"]),
                    &mut Memory::default(),
                    Policy::InProcess,
                );
                assert_eq!(outcome.code, exit::PASS, "{alias}: {outcome:?}");
            }
        }
    }

    /// Every example is a JSON object, so this runs them all through the real tree with `--json -`.
    /// An example either succeeds or fails for a reason this build has: no instance (`E_STATE`) or
    /// no machine factory (`E_INTERNAL`). A third shape is a success that exits non-zero: a
    /// `scenario` batch whose scenarios could not run returns `result: "fail"` and exits 1.
    #[test]
    fn every_documented_example_is_accepted_by_the_command_line() {
        for spec in registry::commands() {
            for example in spec.examples {
                let mut source = Memory {
                    stdin: example.args.to_owned(),
                };
                let outcome = run(
                    &argv(&[spec.name, "--json", "-", "--output", "json"]),
                    &mut source,
                    Policy::InProcess,
                );
                let where_ = format!("{} example `{}`", spec.name, example.title);
                if outcome.code == exit::PASS {
                    continue;
                }
                let value: Value = serde_json::from_str(outcome.stdout.trim_end())
                    .unwrap_or_else(|e| panic!("{where_}: {e}: {outcome:?}"));
                if value.get("code").is_none() {
                    // Succeeded with a verdict of `fail`.
                    assert!(
                        !assertions_hold(&value),
                        "{where_} exited {} with neither a refusal nor a failing result: \
                         {outcome:?}",
                        outcome.code
                    );
                    continue;
                }
                let code = value["code"].as_str().unwrap_or_default();
                // The Device group refuses without `--allow-device`, which these examples never
                // pass.
                if spec.group == pemu_api::spec::CapsGroup::Device && code == "E_PLAN_REFUSED" {
                    continue;
                }
                assert!(
                    code == "E_STATE" || code == "E_INTERNAL",
                    "{where_} failed with {code}, which is not one of the two refusals a \
                     machineless build has: {outcome:?}"
                );
            }
        }
    }

    /// The refusal names the flag that answers it.
    #[test]
    fn a_tainted_start_is_not_forwarded_to_the_shared_daemon() {
        let dump = serde_json::json!({"fw": "official", "efuse_dump": "/tmp/dump"});
        let error = tainted_start_needs_ephemeral("start", &dump).expect("refused");
        assert_eq!(error.code, E_USAGE);
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("--ephemeral")),
            "{:?}",
            error.hint
        );
        assert!(
            tainted_start_needs_ephemeral("start", &serde_json::json!({"fw": "official"}))
                .is_none(),
            "an ordinary start still reaches the daemon"
        );
        assert!(
            tainted_start_needs_ephemeral("status", &dump).is_none(),
            "no other command takes an eFuse dump"
        );

        // `allow_tainted: false` must go on as an ordinary start; a check that only asked whether
        // the key exists would strand every caller that spells the default out.
        let flash = serde_json::json!({
            "fw": "/tmp/backup.bin",
            "allow_tainted": true,
            "confirm": "a person typed this",
        });
        let error = tainted_start_needs_ephemeral("start", &flash).expect("refused");
        assert_eq!(error.code, E_USAGE);
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("--ephemeral")),
            "{:?}",
            error.hint
        );
        assert!(
            tainted_start_needs_ephemeral(
                "start",
                &serde_json::json!({"fw": "official", "allow_tainted": false}),
            )
            .is_none(),
            "`allow_tainted: false` is an ordinary start and still reaches the daemon"
        );
    }

    /// What is absolute differs per host: `/abs/a.yaml` is absolute on macOS and root-relative on
    /// Windows, where it takes the working directory's drive.
    #[test]
    fn a_forwarded_scenario_pattern_is_joined_to_the_working_directory() {
        let cwd = std::path::Path::new(if cfg!(windows) {
            r"C:\work\proj"
        } else {
            "/work/proj"
        });
        let file = |value: &str| {
            let mut call = serde_json::json!({ "file": value });
            absolutize_scenario(&mut call, cwd);
            call["file"].as_str().expect("a string").to_owned()
        };
        if cfg!(windows) {
            for pattern in ["tests/scenarios/*.yaml", r"tests\scenarios\*.yaml"] {
                assert_eq!(file(pattern), "C:/work/proj/tests/scenarios/*.yaml");
            }
            assert_eq!(file("/abs/a.yaml"), "C:/abs/a.yaml", "root-relative");
            assert_eq!(file(r"D:\abs\a.yaml"), r"D:\abs\a.yaml", "already absolute");
        } else {
            assert_eq!(
                file("tests/scenarios/*.yaml"),
                "/work/proj/tests/scenarios/*.yaml"
            );
            assert_eq!(file("/abs/a.yaml"), "/abs/a.yaml", "already absolute");
        }
        let mut inline = serde_json::json!({ "inline": "schema: x" });
        absolutize_scenario(&mut inline, cwd);
        assert!(
            inline.get("file").is_none(),
            "an inline scenario has no path"
        );
    }

    #[test]
    fn a_firmware_written_as_a_relative_path_is_made_absolute_and_a_corpus_id_is_not() {
        let cwd = std::path::Path::new(if cfg!(windows) {
            r"C:\work\proj"
        } else {
            "/work/proj"
        });
        let fw = |text: &str| {
            let mut call = serde_json::json!({ "fw": text });
            absolutize_fw(&mut call, cwd);
            call["fw"].as_str().expect("a string").to_owned()
        };
        assert_eq!(fw("pk"), "pk", "a corpus id");
        assert_eq!(
            fw("demo-merged.bin"),
            "demo-merged.bin",
            "no separator, no dot"
        );
        // The host's own rule: lexical on macOS, where `..` stays; `GetFullPathNameW` on Windows,
        // which folds `.` and `..` and turns `/` into `\`.
        if cfg!(windows) {
            assert_eq!(fw("build/merged.bin"), r"C:\work\proj\build\merged.bin");
            assert_eq!(fw(r"build\merged.bin"), r"C:\work\proj\build\merged.bin");
            assert_eq!(fw("./merged.bin"), r"C:\work\proj\merged.bin");
            assert_eq!(fw("../other/merged.bin"), r"C:\work\other\merged.bin");
            assert_eq!(fw(".hidden.bin"), r"C:\work\proj\.hidden.bin");
            assert_eq!(fw("/abs/merged.bin"), r"C:\abs\merged.bin", "root-relative");
            assert_eq!(
                fw(r"D:\abs\merged.bin"),
                r"D:\abs\merged.bin",
                "already absolute"
            );
        } else {
            assert_eq!(fw("build/merged.bin"), "/work/proj/build/merged.bin");
            assert_eq!(fw("./merged.bin"), "/work/proj/merged.bin");
            assert_eq!(fw("../other/merged.bin"), "/work/proj/../other/merged.bin");
            assert_eq!(fw(".hidden.bin"), "/work/proj/.hidden.bin");
            assert_eq!(fw("/abs/merged.bin"), "/abs/merged.bin", "already absolute");
        }
    }

    #[test]
    fn a_process_without_a_data_root_says_it_builds_no_machine() {
        let line = no_factory_warning(&pemu_host::paths::PathError::HomeNotSet);
        assert!(
            line.starts_with("warning: the data root is not resolvable ("),
            "{line}"
        );
        assert!(line.ends_with("PASSPORTSIM_DATA_ROOT\n"), "{line}");
        assert!(!line.contains("  "), "{line}");
        assert_eq!(
            install_host_facts(None, Policy::InProcess, None, None),
            None,
            "the test policy installs no factory and warns about none"
        );
    }

    #[test]
    fn a_verdict_rests_on_the_result_when_a_command_reports_one() {
        assert!(assertions_hold(&serde_json::json!({})));
        assert!(assertions_hold(&serde_json::json!({"result":"pass"})));
        assert!(assertions_hold(
            &serde_json::json!({"result":"pass_with_caveats"})
        ));
        assert!(!assertions_hold(&serde_json::json!({"result":"fail"})));
    }

    #[test]
    fn the_output_mode_is_known_before_the_arguments_parse() {
        assert_eq!(mode_of(&argv(&["status"])), Mode::Text);
        assert_eq!(mode_of(&argv(&["--output", "json", "status"])), Mode::Json);
        assert_eq!(mode_of(&argv(&["status", "--output=json"])), Mode::Json);
        assert_eq!(
            mode_of(&argv(&["status", "--output=json", "--output", "text"])),
            Mode::Text,
            "the last spelling wins, as clap resolves it"
        );
    }

    /// `status` and `stop` carry the same instance receipt, and `--strict` (the CI default) must
    /// not fail a `stop` because an earlier run touched a class-U register. A scenario result's own
    /// code is the process's code; `--strict` still makes 10 a 7.
    #[test]
    fn a_scenario_result_exits_with_its_own_code() {
        let receipt = pemu_api::receipt::Receipt {
            unmodeled_first_touch: vec!["gdma.GDMA_IN_LINK_CH0".to_string()],
            ..pemu_api::receipt::Receipt::default()
        };
        for (payload, lenient, strict) in [(0, 0, 0), (10, 10, 7), (1, 1, 1), (6, 6, 6), (8, 8, 8)]
        {
            let json = serde_json::json!({"result": "pass", "exit_code": payload});
            assert_eq!(
                success_code("scenario", &receipt, &json, Strictness::Lenient),
                lenient,
                "payload {payload} lenient"
            );
            assert_eq!(
                success_code("scenario", &receipt, &json, Strictness::Strict),
                strict,
                "payload {payload} strict"
            );
        }
    }

    #[test]
    fn a_caveat_moves_the_exit_code_of_a_command_that_judges_and_of_no_other() {
        let receipt = pemu_api::receipt::Receipt {
            unmodeled_first_touch: vec!["twai.0x0000".to_string()],
            ..pemu_api::receipt::Receipt::default()
        };
        let clean = pemu_api::receipt::Receipt::default();
        let ok = serde_json::json!({"result": "pass"});
        let bad = serde_json::json!({"result": "fail"});
        for judging in ["run", "scenario"] {
            assert_eq!(
                success_code(judging, &receipt, &ok, Strictness::Lenient),
                exit::PASS_WITH_CAVEATS,
                "{judging} lenient"
            );
            assert_eq!(
                success_code(judging, &receipt, &ok, Strictness::Strict),
                exit::UNMODELED_HW,
                "{judging} strict"
            );
            assert_eq!(
                success_code(judging, &clean, &ok, Strictness::Strict),
                exit::PASS,
                "{judging} with no caveat"
            );
            assert_eq!(
                success_code(judging, &receipt, &bad, Strictness::Lenient),
                exit::FAIL,
                "{judging} failing assertions outrank a caveat"
            );
        }
        for reporting in ["status", "stop", "serial read", "screenshot", "start"] {
            assert_eq!(
                success_code(reporting, &receipt, &ok, Strictness::Strict),
                exit::PASS,
                "{reporting} reports the caveat in the receipt and exits 0"
            );
            assert_eq!(
                success_code(reporting, &receipt, &bad, Strictness::Lenient),
                exit::FAIL,
                "{reporting} still fails when it says it failed"
            );
        }
    }
}
