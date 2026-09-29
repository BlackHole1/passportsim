//! Running CI steps: captured logs, progress lines and stub detection.
//!
//! Each command step writes its combined output to `<target>/xtask-ci/<tier>-<stamp>/<step>.log`
//! (`<target>` is `CARGO_TARGET_DIR` or the workspace `target/`), so the terminal shows one
//! progress line per step and the log tail of a failed step only.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::model::{self, Status, StepResult};
use super::outcome::{self, TestOutcome};

/// What an unimplemented xtask subcommand prints; [`Ctx::xtask_step`] reports its step SKIPPED.
const NOT_IMPLEMENTED: &str = "not implemented yet";
/// Lines of a failed step's log printed to the terminal.
const TAIL_LINES: usize = 25;

/// State of one tier run.
pub struct Ctx {
    /// Workspace root; every command runs there.
    pub root: PathBuf,
    /// `t0`, `t1` or `t2`.
    pub tier: String,
    /// Directory of the step logs.
    pub log_dir: PathBuf,
    /// Steps recorded so far, in order.
    pub steps: Vec<StepResult>,
    /// Outcome of each test run by a test step so far.
    pub tests: Vec<TestOutcome>,
}

/// Outcome of one command.
pub struct Exec {
    pub success: bool,
    /// Short description of a failure (`exit status 101`), empty on success.
    pub failure: String,
    pub duration: Duration,
    pub log: PathBuf,
    /// Captured stdout, when requested; otherwise stdout goes to the log.
    pub stdout: String,
}

impl Ctx {
    /// A context for `tier`, with logs under a directory named by `stamp`.
    pub fn new(tier: &str, stamp: &str) -> Ctx {
        let root = crate::util::workspace_root();
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target"));
        let log_dir = root
            .join(target)
            .join("xtask-ci")
            .join(format!("{tier}-{stamp}"));
        Ctx {
            root,
            tier: tier.to_string(),
            log_dir,
            steps: Vec::new(),
            tests: Vec::new(),
        }
    }

    /// `program args` run from the workspace root, with colors off.
    ///
    /// The program is resolved by [`resolve`] first, so a step never spawns a `.cmd` or `.bat`
    /// shim. An unresolved name is spawned as given, which fails with a clear "cannot spawn"
    /// instead of silently doing something else.
    pub fn command(&self, program: &str, args: &[&str]) -> Command {
        command_in(&self.root, program, args)
    }

    /// Stdout of a short query command, trimmed; `None` when it cannot run or fails.
    pub fn query(&self, program: &str, args: &[&str]) -> Option<String> {
        let output = self
            .command(program, args)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Records a finished step: a progress line, then the list.
    pub fn record(&mut self, step: StepResult) {
        let reason = match step.reason.as_str() {
            "" => String::new(),
            reason => format!(": {}", model::one_line(reason)),
        };
        let line = format!(
            "[ci {}] {} {} ({}){reason}",
            self.tier,
            step.name,
            step.status.as_str(),
            model::seconds(step.duration)
        );
        println!("{line}");
        self.steps.push(step);
    }

    /// Runs `cmd` with its output in the step log; stdout is captured instead when
    /// `capture_stdout` is set.
    pub fn exec(&self, name: &str, mut cmd: Command, capture_stdout: bool) -> Exec {
        let log = self.log_dir.join(format!("{name}.log"));
        let started = Instant::now();
        let spawned = std::fs::create_dir_all(&self.log_dir)
            .and_then(|()| File::create(&log))
            .and_then(|file| {
                let stdout = if capture_stdout {
                    Stdio::piped()
                } else {
                    Stdio::from(file.try_clone()?)
                };
                cmd.stdin(Stdio::null())
                    .stdout(stdout)
                    .stderr(Stdio::from(file))
                    .spawn()
            });
        let mut bytes = Vec::new();
        let result = spawned.and_then(|mut child| {
            // Read the bytes and decode them lossily, and wait for the child whatever the read
            // gave, so a test printing invalid UTF-8 neither loses the output nor leaves a zombie.
            let read = child
                .stdout
                .take()
                .map_or(Ok(0), |mut pipe| pipe.read_to_end(&mut bytes));
            let status = child.wait();
            read.and(status)
        });
        let stdout = String::from_utf8_lossy(&bytes).into_owned();
        let (success, failure) = match result {
            Ok(status) if status.success() => (true, String::new()),
            Ok(status) => (false, status.to_string()),
            Err(err) => (false, format!("cannot run: {}", err.kind())),
        };
        Exec {
            success,
            failure,
            duration: started.elapsed(),
            log,
            stdout,
        }
    }

    /// Runs `cmd` as step `name`, once more if the first run failed for a network reason.
    ///
    /// `cargo deny check` fetches the RustSec advisory database over the network, and a proxy can
    /// drop that connection (`Connection closed by <proxy> port 443`) where an immediate re-run
    /// passes. Retrying once keeps a dropped fetch from reading as a failed audit, and the note
    /// says the retry happened: only a second failure is reported, with its own log.
    pub fn run_step_retrying_network(
        &mut self,
        name: &str,
        program: &str,
        args: &[&str],
        note: &str,
    ) {
        let exec = self.exec(name, self.command(program, args), false);
        let exec = if !exec.success && network_failure(&read_log(&exec.log)) {
            eprintln!("[ci] {name}: retrying once, the first run failed to reach the network");
            self.exec(name, self.command(program, args), false)
        } else {
            return_step(self, name, &exec, note);
            return;
        };
        let note = if note.is_empty() {
            "retried once after a network failure".to_string()
        } else {
            format!("{note}; retried once after a network failure")
        };
        return_step(self, name, &exec, &note);
    }

    /// Runs `cmd` as step `name`: PASS with `note`, or FAIL with the exit status and log path.
    pub fn run_step(&mut self, name: &str, cmd: Command, note: &str) {
        let exec = self.exec(name, cmd, false);
        let step = self.finish(name, &exec, note);
        self.record(step);
    }

    /// Runs a `cargo test` command as step `name` and reads what its tests printed (`outcome.rs`):
    /// the step is PASS or FAIL by the command, each blocked, corpus-skipped or skipped test and
    /// each not-run leg follows it as a sub-step, and the test outcomes are kept for later steps. The command must pass `--show-output` or `--nocapture`, so passing tests' lines are
    /// on stdout; that stdout is kept beside the step log as `<step>.stdout.log`.
    pub fn test_step(&mut self, name: &str, cmd: Command, note: &str) {
        let exec = self.exec(name, cmd, true);
        let stdout_log = exec.log.with_extension("stdout.log");
        let _ = std::fs::write(&stdout_log, &exec.stdout);
        let report = outcome::read(&exec.stdout);
        if !exec.success {
            eprintln!("---- tail of {} ----", stdout_log.display());
            eprintln!("{}", tail(&stdout_log, TAIL_LINES));
        }
        let subs = report.sub_steps(name);
        let note = match (note, subs.is_empty()) {
            (_, true) => note.to_string(),
            ("", false) => report.note(),
            (note, false) => format!("{note}; {}", report.note()),
        };
        let mut step = self.finish(name, &exec, &note);
        // An orphan SKIP or PENDING line cannot be attributed to a test, so the run proves less
        // than its exit status says.
        if report.has_orphans() && step.status == Status::Pass {
            step.status = Status::Fail;
            step.reason = format!("{note}; a SKIP or PENDING line names no test of this run");
        }
        // A line that announced itself as a marker and did not parse is the same failure: whatever
        // it reported is missing from the receipt (`outcome::Report::malformed`).
        if report.has_malformed() && step.status == Status::Pass {
            step.status = Status::Fail;
            step.reason = format!(
                "{note}; {} line(s) begin with a marker word and do not parse as a marker, \
                 first: {}",
                report.malformed.len(),
                report.malformed.first().copied().unwrap_or_default()
            );
        }
        self.record(step);
        for sub in subs {
            self.record(sub);
        }
        self.tests.extend(report.outcomes());
    }

    /// Runs `xtask <args>` as step `name`; SKIPPED when the subcommand says it is unimplemented.
    ///
    /// The subcommand runs from this process's own executable ([`xtask_command`]), not through
    /// `cargo xtask`: `cargo run` would rebuild `xtask` whenever it judged it stale (a changed
    /// source, a changed `.cargo/config.toml`), and on Windows that relink fails on the image of
    /// the running runner (`os error 5`). It also means every step runs the very code whose name
    /// the receipt carries.
    pub fn xtask_step(&mut self, name: &str, args: &[&str], note: &str) {
        let exec = self.exec(name, xtask_command(&self.root, args), false);
        let log_text = std::fs::read_to_string(&exec.log).unwrap_or_default();
        let stub_line = log_text.lines().find(|line| line.contains(NOT_IMPLEMENTED));
        let step = match stub_line {
            Some(line) if !exec.success => StepResult {
                name: name.to_string(),
                status: Status::Skipped,
                reason: line.trim().to_string(),
                duration: exec.duration,
            },
            _ => self.finish(name, &exec, note),
        };
        self.record(step);
    }

    /// The step result of a finished command, printing the log tail of a failure.
    pub fn finish(&self, name: &str, exec: &Exec, note: &str) -> StepResult {
        let (status, reason) = if exec.success {
            (Status::Pass, note.to_string())
        } else {
            let log = exec.log.strip_prefix(&self.root).unwrap_or(&exec.log);
            eprintln!("---- tail of {} ----", log.display());
            eprintln!("{}", tail(&exec.log, TAIL_LINES));
            (
                Status::Fail,
                format!("{}; log {}", exec.failure, log.display()),
            )
        };
        StepResult {
            name: name.to_string(),
            status,
            reason,
            duration: exec.duration,
        }
    }
}

/// `program args` run from `dir`, with colors off: [`Ctx::command`] for a caller that holds no
/// context.
pub fn command_in(dir: &Path, program: &str, args: &[&str]) -> Command {
    let mut cmd = match resolve(program) {
        Tool::Found(path) => Command::new(path),
        Tool::ShimOnly | Tool::Missing => Command::new(program),
    };
    cmd.args(args)
        .current_dir(dir)
        .env("CARGO_TERM_COLOR", "never");
    cmd
}

/// `xtask <args>` run from `dir` by this process's own executable, or through `cargo xtask` when
/// the executable cannot be named (`std::env::current_exe` failed).
pub fn xtask_command(dir: &Path, args: &[&str]) -> Command {
    match std::env::current_exe() {
        Ok(exe) => {
            let mut cmd = Command::new(exe);
            cmd.args(args)
                .current_dir(dir)
                .env("CARGO_TERM_COLOR", "never");
            cmd
        }
        Err(_) => {
            let mut full = vec!["xtask"];
            full.extend_from_slice(args);
            command_in(dir, "cargo", &full)
        }
    }
}

/// How an external program of xtask resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tool {
    /// The executable, by absolute path.
    Found(PathBuf),
    /// Only a `.cmd` or `.bat` shim exists. It is never spawned: `Command` would run it through
    /// `cmd.exe`, whose own argument splitting is the BatBadBut class (CVE-2024-24576).
    ShimOnly,
    /// Nothing of that name on `PATH`.
    Missing,
}

/// Extensions a resolved program may have on Windows: `PATHEXT` is deliberately **not** read, and
/// `.cmd`, `.bat` and `.ps1` are never executed.
const WINDOWS_EXEC: &[&str] = &["exe", "com"];
/// Shim extensions that turn a lookup into [`Tool::ShimOnly`].
const WINDOWS_SHIM: &[&str] = &["cmd", "bat"];

/// Looks `program` up on `PATH` once, without a shell.
pub fn resolve(program: &str) -> Tool {
    if Path::new(program).components().count() > 1 {
        let path = PathBuf::from(program);
        return if path.is_file() {
            Tool::Found(path)
        } else {
            Tool::Missing
        };
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return Tool::Missing;
    };
    let windows = cfg!(windows);
    let mut shim = false;
    for dir in std::env::split_paths(&path_var) {
        if !windows {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return Tool::Found(candidate);
            }
            continue;
        }
        for ext in WINDOWS_EXEC {
            let candidate = dir.join(format!("{program}.{ext}"));
            if candidate.is_file() {
                return Tool::Found(candidate);
            }
        }
        shim = shim
            || WINDOWS_SHIM
                .iter()
                .any(|ext| dir.join(format!("{program}.{ext}")).is_file());
    }
    if shim { Tool::ShimOnly } else { Tool::Missing }
}

/// Last `lines` lines of a text file.
pub fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The step log of a run that could not reach the network, rather than one that judged its input.
fn network_failure(log: &str) -> bool {
    const SIGNS: [&str; 5] = [
        "failed to fetch advisory database",
        "Could not read from remote repository",
        "Connection closed by",
        "Could not resolve host",
        "Connection timed out",
    ];
    SIGNS.iter().any(|sign| log.contains(sign))
}

fn read_log(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn return_step(ctx: &mut Ctx, name: &str, exec: &Exec, note: &str) {
    let step = ctx.finish(name, exec, note);
    ctx.record(step);
}

#[cfg(test)]
mod tests {
    use super::network_failure;

    /// A dropped fetch is retried; a judgement is not. `cargo deny check` prints its verdict and
    /// exits non-zero when an advisory matches, and that must stay red on the first run.
    #[test]
    fn only_a_run_that_could_not_reach_the_network_is_retried() {
        let dropped = "\
2026-09-22 18:02:51 [ERROR] failed to fetch advisory database https://github.com/RustSec/advisory-db \
with cli: failed to fetch latest changes\nConnection closed by 198.18.29.97 port 443\n\
fatal: Could not read from remote repository.\n";
        assert!(network_failure(dropped));
        // Each sign on its own, so dropping one from the list fails this test rather than being
        // covered by another sign in the same sample.
        for line in [
            "[ERROR] failed to fetch advisory database https://github.com/RustSec/advisory-db",
            "fatal: Could not read from remote repository.",
            "Connection closed by 198.18.29.97 port 443",
            "fatal: Could not resolve host: github.com",
            "ssh: connect to host github.com port 22: Connection timed out",
        ] {
            assert!(network_failure(line), "{line}");
        }

        let judged = "\
error[vulnerability]: a crate with a security vulnerability was detected\n\
advisories FAILED, bans ok, licenses ok, sources ok\n";
        assert!(!network_failure(judged));
        assert!(!network_failure(""));
        // A licence refusal names a registry without being a network failure.
        assert!(!network_failure(
            "error[rejected]: failed to satisfy license requirements for crates-io source"
        ));
    }
}
