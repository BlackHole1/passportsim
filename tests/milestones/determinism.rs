//! The legs of the determinism harness that need a host: another process and another engine.
//! What is compared and what may vary is `pemu_machine::determinism`.
//!
//! [`fresh_process`] writes each snapshot to a file and reruns this test binary once per
//! snapshot, filtered to the calling test, with [`CHILD_ENV`] naming the file; the child restores
//! it over the same image and answers through [`child_answer`]. It shares nothing else with the
//! parent.
//!
//! [`wasm_legs`] runs `crates/pemu-wasm/js/abi_parity.cjs` over the wasm32 `pemu-wasm` under Node
//! and `jsc`, booting through the Worker's ABI. A test never shells out to cargo, so the module is
//! built by `cargo xtask ci t1` and named by [`WASM_ENV`]. A leg that cannot run prints
//! `NOT_RUN <test> <engine>: <reason>`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pemu_machine::Machine;
use pemu_machine::determinism::Since;
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{MatcherId, StopReason, StopSet};

/// The variable naming the snapshot file a child restores.
pub const CHILD_ENV: &str = "PEMU_DET_CHILD";

/// The variable naming the wasm32 `pemu_wasm` module.
pub const WASM_ENV: &str = "PEMU_DET_WASM";

const RUNNER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/pemu-wasm/js/abi_parity.cjs"
);

/// Where the `jsc` shell lives on macOS when it is not on `PATH` (JavaScriptCore framework).
const MACOS_JSC: &str =
    "/System/Library/Frameworks/JavaScriptCore.framework/Versions/Current/Helpers/jsc";

/// In a child process: the snapshot file to restore.
pub fn child_snapshot() -> Option<PathBuf> {
    std::env::var_os(CHILD_ENV).map(PathBuf::from)
}

/// In a child process: the answer for `snapshot`, written beside it.
pub fn child_answer(snapshot: &Path, text: &str) {
    std::fs::write(snapshot.with_extension("out"), text).expect("the answer file is writable");
}

/// The tag a child's snapshot was written under ([`fresh_process`]).
pub fn child_tag(snapshot: &Path) -> String {
    snapshot
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Restores each of `snapshots` (tag, bytes) in a process of its own and returns the children's
/// answers in order. `test` filters the child run.
pub fn fresh_process(test: &str, snapshots: &[(String, Vec<u8>)]) -> Vec<String> {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("pemu-det-{test}-{}", std::process::id())));
    let dir = &scratch.0;
    std::fs::create_dir_all(dir).expect("a scratch directory");
    let exe = std::env::current_exe().expect("the test binary knows its path");
    let files: Vec<PathBuf> = snapshots
        .iter()
        .map(|(tag, bytes)| {
            let file = dir.join(format!("{tag}.bin"));
            std::fs::write(&file, bytes).expect("the snapshot file is writable");
            file
        })
        .collect();
    let width = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut answers = Vec::new();
    for batch in files.chunks(width) {
        let children: Vec<_> = batch
            .iter()
            .map(|file| {
                Command::new(&exe)
                    .args(["--exact", test, "--nocapture", "--test-threads", "1"])
                    .env(CHILD_ENV, file)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("the test binary starts again")
            })
            .collect();
        for (file, child) in batch.iter().zip(children) {
            let out = child.wait_with_output().expect("the child runs to its end");
            let answer = std::fs::read_to_string(file.with_extension("out")).unwrap_or_else(|_| {
                panic!(
                    "{test}: the child restoring {} gave no answer ({}); its output:\n{}{}",
                    file.file_name().unwrap().to_string_lossy(),
                    out.status,
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )
            });
            answers.push(answer);
        }
    }
    answers
}

/// The scratch directory of [`fresh_process`], removed when it goes out of scope, also when a
/// child's missing answer panics the parent.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Taken {
    pub breakpoints: usize,
    pub watches: usize,
    pub matchers: usize,
    pub pauses: usize,
}

/// Runs `m` to one of `target`'s matchers or its instruction limit with `extra` stops armed too,
/// pausing at every instruction count in `pauses`. Each extra stop is taken, disarmed and resumed,
/// so the end can be compared with a run that armed nothing. Returns the reason and what was taken.
pub fn run_through_stops(
    m: &mut Machine,
    target: &StopSet,
    max_insns: u64,
    mut extra: StopSet,
    pauses: &[u64],
) -> (StopReason, Taken) {
    let targets: Vec<MatcherId> = target.matchers.iter().map(|(id, _)| *id).collect();
    let end = m.hart().insns + max_insns;
    let mut taken = Taken::default();
    loop {
        let now = m.hart().insns;
        let next_pause = pauses.iter().copied().filter(|&p| p > now).min();
        let stop_at = next_pause.map_or(end, |p| p.min(end));
        let mut stops = target.clone();
        stops.breakpoints.extend(&extra.breakpoints);
        stops.watches.extend(&extra.watches);
        stops.matchers.extend(extra.matchers.iter().cloned());
        let out = m.run(RunLimits {
            until: None,
            max_insns: Some(stop_at.saturating_sub(now)),
            stops,
        });
        match out.reason {
            StopReason::Matcher(id) if targets.contains(&id) => return (out.reason, taken),
            StopReason::MaxInsns if m.hart().insns < end => taken.pauses += 1,
            StopReason::Breakpoint(pc) if extra.breakpoints.contains(&pc) => {
                extra.breakpoints.retain(|&b| b != pc);
                taken.breakpoints += 1;
            }
            StopReason::Watchpoint { addr, .. } => {
                extra
                    .watches
                    .retain(|w| !(w.addr <= addr && addr < w.addr + w.len.max(1)));
                taken.watches += 1;
            }
            StopReason::Matcher(id) => {
                extra.matchers.retain(|(m, _)| *m != id);
                taken.matchers += 1;
            }
            other => return (other, taken),
        }
    }
}

/// What the frame, PCM and poll fast-forward comparisons of one test had to compare, span by span.
/// [`Coverage::report`] prints one `RAN` or `NOT_RUN` line per sub-check, so a vacuous comparison
/// is never silent.
#[derive(Default)]
pub struct Coverage {
    /// Frames presented after the mark, by span.
    frames: Vec<(String, u64)>,
    /// PCM samples written after the mark, and how many of them are not zero, by span.
    pcm: Vec<(String, u64, u64)>,
    /// Instructions the poll fast-forward skipped with the ROM delay shortcut off, by span.
    poll_ff: Vec<(String, u64)>,
}

impl Coverage {
    pub fn output(&mut self, what: &str, m: &mut Machine, since: Since) {
        let frames = m.io().frame.generation().wrapping_sub(since.frame);
        let ring = &m.io().audio_out;
        let from = since.pcm.max(ring.tail());
        let samples = ring.head().saturating_sub(since.pcm);
        let nonzero = ring.slices(from).iter().filter(|s| **s != 0).count() as u64;
        self.frames.push((what.to_string(), frames));
        self.pcm.push((what.to_string(), samples, nonzero));
    }

    pub fn poll_ff(&mut self, what: &str, skipped: u64) {
        self.poll_ff.push((what.to_string(), skipped));
    }

    /// Prints the `RAN` or `NOT_RUN` line of each sub-check that has a span.
    pub fn report(&self, test: &str) {
        let list = |items: Vec<String>| items.join("; ");
        if !self.frames.is_empty() {
            let counts = list(
                self.frames
                    .iter()
                    .map(|(what, n)| format!("{what} {n} presented"))
                    .collect(),
            );
            if self.frames.iter().any(|(_, n)| *n > 0) {
                println!("RAN {test} frame: the frame digests compare pictures: {counts}");
            } else {
                println!(
                    "NOT_RUN {test} frame-constant: no span presented a frame, the frame digests \
                     compare nothing: {counts}"
                );
            }
        }
        if !self.pcm.is_empty() {
            let counts = list(
                self.pcm
                    .iter()
                    .map(|(what, n, nz)| format!("{what} {n} samples, {nz} not zero"))
                    .collect(),
            );
            if self.pcm.iter().any(|(_, _, nz)| *nz > 0) {
                println!("RAN {test} pcm: the PCM digests compare sound: {counts}");
            } else if self.pcm.iter().any(|(_, n, _)| *n > 0) {
                println!(
                    "NOT_RUN {test} pcm-silent: every PCM sample written was 0, so the PCM \
                     digests compare the stream's instants and length but no sound: {counts}"
                );
            } else {
                println!(
                    "NOT_RUN {test} pcm-empty: no span wrote a PCM sample, the PCM digests \
                     compare nothing: {counts}"
                );
            }
        }
        if !self.poll_ff.is_empty() {
            let counts = list(
                self.poll_ff
                    .iter()
                    .map(|(what, n)| format!("{what} {n} instructions skipped"))
                    .collect(),
            );
            if self.poll_ff.iter().any(|(_, n)| *n > 0) {
                println!(
                    "RAN {test} poll-ff: equal reports and canonical traces with poll \
                     fast-forward on and off: {counts}"
                );
            } else {
                println!(
                    "NOT_RUN {test} poll-ff: poll fast-forward skipped 0 instructions on every \
                     span, so on and off compare nothing: {counts}"
                );
            }
        }
    }
}

pub enum Leg {
    Report(String),
    NotRun(String),
}

/// The inputs of one wasm boot: the arguments of `crates/pemu-wasm/js/abi_parity.cjs`.
pub struct WasmBoot<'a> {
    pub image: &'a Path,
    pub pattern: &'a str,
    pub prefix: bool,
    pub max_insns: u64,
    pub max_block_insns: u16,
    pub max_slice: u64,
    pub poll_ff: bool,
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Node and `jsc`, each found or with the reason it is absent.
fn engines() -> [(&'static str, Result<PathBuf, String>); 2] {
    let node = on_path("node").ok_or_else(|| "node is not on PATH".to_string());
    let jsc = on_path("jsc")
        .or_else(|| {
            let p = PathBuf::from(MACOS_JSC);
            (cfg!(target_os = "macos") && p.is_file()).then_some(p)
        })
        .ok_or_else(|| {
            if cfg!(target_os = "macos") {
                // Every macOS ships the shell inside JavaScriptCore; its absence is a broken host.
                "jsc missing at the system path: broken host".to_string()
            } else {
                "no jsc: the jsc leg is macOS-only".to_string()
            }
        });
    [("node", node), ("jsc", jsc)]
}

pub fn wasm_legs(boot: &WasmBoot<'_>) -> Vec<(&'static str, Leg)> {
    wasm_legs_scripted(boot, None)
}

/// [`wasm_legs`] with a script: a JSON file `{"inputs": [{at, event}], "until_ps": "<ps>"}` whose
/// inputs the runner journals before it runs, for runs such as the Audio demo that need presses
/// and microphone chunks.
pub fn wasm_legs_scripted(boot: &WasmBoot<'_>, script: Option<&Path>) -> Vec<(&'static str, Leg)> {
    let wasm = match std::env::var_os(WASM_ENV) {
        Some(p) if Path::new(&p).is_file() => PathBuf::from(p),
        Some(_) => {
            return engines()
                .map(|(e, _)| (e, Leg::NotRun(format!("{WASM_ENV} names no file"))))
                .into();
        }
        None => {
            return engines()
                .map(|(e, _)| {
                    (
                        e,
                        Leg::NotRun(format!(
                            "{WASM_ENV} is unset; `cargo xtask ci t1` step node-jsc-parity \
                             builds the wasm32 module and sets it"
                        )),
                    )
                })
                .into();
        }
    };
    let hex: String = boot
        .pattern
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let args = [
        wasm.display().to_string(),
        boot.image.display().to_string(),
        hex,
        u8::from(boot.prefix).to_string(),
        boot.max_insns.to_string(),
        boot.max_block_insns.to_string(),
        boot.max_slice.to_string(),
        u8::from(boot.poll_ff).to_string(),
    ]
    .into_iter()
    .chain(script.map(|p| p.display().to_string()))
    .collect::<Vec<String>>();
    engines()
        .into_iter()
        .map(|(engine, found)| {
            let exe = match found {
                Ok(exe) => exe,
                Err(why) => return (engine, Leg::NotRun(why)),
            };
            let mut cmd = Command::new(exe);
            cmd.arg(RUNNER);
            if engine == "jsc" {
                cmd.arg("--");
            }
            let out = match cmd.args(&args).output() {
                Ok(out) => out,
                Err(e) => return (engine, Leg::NotRun(format!("{engine} did not start: {e}"))),
            };
            let stdout = String::from_utf8_lossy(&out.stdout);
            let report = stdout
                .lines()
                .find_map(|l| l.strip_prefix("REPORT "))
                .unwrap_or_else(|| {
                    panic!(
                        "the {engine} leg ran and reported nothing ({}):\n{stdout}{}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr)
                    )
                });
            (engine, Leg::Report(report.to_string()))
        })
        .collect()
}

/// Compares every leg that ran with `native` and prints `NOT_RUN` for every leg that did not.
#[track_caller]
pub fn assert_legs(test: &str, what: &str, native: &str, legs: Vec<(&'static str, Leg)>) {
    for (engine, leg) in legs {
        match leg {
            Leg::Report(report) => {
                assert_eq!(
                    report, native,
                    "{test}: {what}: {engine} differs from native"
                );
                println!("RAN {test} {engine}: {what} equals native");
            }
            Leg::NotRun(why) => println!("NOT_RUN {test} {engine}: {why}"),
        }
    }
}

#[test]
fn the_scratch_directory_is_removed_when_the_parent_panics() {
    let dir = std::env::temp_dir().join(format!("pemu-det-drop-{}", std::process::id()));
    let inner = dir.clone();
    let result = std::panic::catch_unwind(move || {
        let scratch = Scratch(inner);
        std::fs::create_dir_all(&scratch.0).unwrap();
        std::fs::write(scratch.0.join("snap.bin"), b"x").unwrap();
        panic!("a child gave no answer");
    });
    assert!(result.is_err());
    assert!(!dir.exists(), "the scratch directory survived the panic");
}
