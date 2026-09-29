//! `cargo xtask bench-k`: workload K, the kernel gate.
//!
//! The spike's fw-Og kernel runs through the exact-deadline engine with all feature overheads
//! compiled in, and its native median of 7 runs must be at least 0.90 x the spike `blockx` median
//! measured in the same job on the same host. The **ratio** is the gate. Absolute numbers are
//! recorded with a host fingerprint and never compared across hosts, which is why this command
//! measures both sides itself instead of reading a stored baseline. bench-Og is recorded as the
//! upper-bound workload; only fw-Og gates.
//!
//! Both sides run the same guest image and iteration count and report `retired instructions /
//! wall seconds of the whole run, including cold translation` (design-facts perf-A). Our
//! side runs `pemu_rv32::kbench` in slices as the machine's run loop does, so the exact-budget
//! path and the in-block resume token are measured; the spike side builds and runs
//! `<data root>/spikes/rv32-interp` with its `blockx` engine, the one with exact deadlines. Each
//! side is checked against the spike's two anchors, the checksum in `a0` and the retired count
//! (ours is exactly [`EXIT_ECALL`] lower), so a run that executed the wrong thing, or less of the
//! right thing, fails.
//!
//! The kernels are built in the same job from `bench/kernels/` with the ESP toolchain ([`GCC`]);
//! without `riscv32-esp-elf-gcc` the command stops rather than measure something else. It must be
//! built in release: a debug engine measures the optimizer and not the design.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use pemu_rv32::kbench::{EXIT_ECALL, KernelMachine};

use crate::bench::cores::{self, CoreTime};
use crate::hostdirs;

/// Ratio of the spike `blockx` median our median must reach.
pub(crate) const GATE: f64 = 0.90;

/// Runs per side, of which the median is taken.
pub(crate) const DEFAULT_REPEAT: usize = 7;

/// Instructions per engine call, the spike's default slice (design-facts perf-A).
pub(crate) const DEFAULT_SLICE: u64 = 1_000_000;

/// The pinned ESP toolchain, below `$HOME` (the same path `xtask riscv-tests` uses).
pub(crate) const GCC: &str =
    ".espressif/tools/riscv32-esp-elf/esp-14.2.0_20251107/riscv32-esp-elf/bin/riscv32-esp-elf-gcc";

/// `riscv32-esp-elf-objcopy` next to [`GCC`].
const OBJCOPY: &str = "riscv32-esp-elf-objcopy";

/// Compiler arguments of the spike's workload build script (`bench/kernels/README.md`).
const GCC_ARGS: [&str; 6] = [
    "-march=rv32imc_zicsr_zifencei",
    "-mabi=ilp32",
    "-ffreestanding",
    "-nostdlib",
    "-nostartfiles",
    "-fno-builtin",
];

/// One kernel of workload K.
pub(crate) struct Workload {
    /// Name, which is also the kernel source stem plus its optimization level.
    pub(crate) name: &'static str,
    /// Source file under `bench/kernels/`.
    source: &'static str,
    /// Optimization level the image is built at.
    opt: &'static str,
    /// Iteration count handed to the kernel in `a0`.
    pub(crate) iters: u32,
    /// Checksum the kernel returns in `a0` at that iteration count (`bench/kernels/README.md`).
    pub(crate) checksum: u32,
    /// Instructions the kernel retires at that iteration count, the spike's count
    /// (`bench/kernels/README.md`). The engine's is [`EXIT_ECALL`] lower; see there.
    pub(crate) insns: u64,
    /// True for the workload the gate is taken on: fw-Og, the design reference.
    pub(crate) gates: bool,
}

/// Workload K: fw-Og is the design reference and the gate, bench-Og the upper bound.
pub(crate) const WORKLOADS: [Workload; 2] = [
    Workload {
        name: "fw-Og",
        source: "fw.c",
        opt: "-Og",
        iters: 10_000,
        checksum: 0x7A19_B27C,
        insns: 413_918_117,
        gates: true,
    },
    Workload {
        name: "bench-Og",
        source: "bench.c",
        opt: "-Og",
        iters: 350,
        checksum: 0x50F7_A0A8,
        insns: 391_184_244,
        gates: false,
    },
];

/// Entry point of `cargo xtask bench-k`.
pub fn run(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args)?;
    if cfg!(debug_assertions) {
        return Err(
            "bench-k measures the engine and must be built in release; run\n  \
             cargo run --release -p xtask -- bench-k"
                .to_string(),
        );
    }
    let host = Fingerprint::of_host();
    // Both sides run at the class asked for here: our engine on this thread, the spike in a child
    // process, which inherits it (`crate::bench::cores`).
    let qos = cores::request_interactive();
    let repo = crate::util::workspace_root();
    let out = repo.join("target/bench-kernels");
    let gcc = toolchain(GCC)?;

    println!("xtask bench-k: workload K, the kernel gate");
    println!("host: {host}");
    println!("measuring thread: QoS {qos}; the spike inherits it");
    println!(
        "gate: fw-Og median of {} >= {GATE:.2} x the spike blockx median, same job, same host",
        options.repeat
    );
    println!();

    let spike = options
        .spike
        .then(spike_dir)
        .transpose()?
        .map(|dir| -> Result<PathBuf, String> {
            build_spike(&dir)?;
            Ok(dir.join("target/release/rv32-bench"))
        })
        .transpose()?;

    let mut failures = Vec::new();
    for workload in &WORKLOADS {
        let image = build_kernel(&gcc, &repo, &out, workload)?;
        let ours = measure_engine(&image, workload, &options)?;
        let theirs = match &spike {
            Some(bin) => Some(measure_spike(bin, workload, &options)?),
            None => None,
        };
        report(workload, &ours, theirs.as_ref(), &host, &options);
        if let Some(theirs) = &theirs {
            failures.extend(gate_failure(workload, &ours, theirs));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

/// Command-line options.
struct Options {
    repeat: usize,
    slice: u64,
    max_block_insns: u16,
    /// Measure the spike as well. Off records our numbers only, which is what a host with no
    /// preserved spike can still do.
    spike: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut o = Options {
            repeat: DEFAULT_REPEAT,
            slice: DEFAULT_SLICE,
            max_block_insns: 64,
            spike: true,
        };
        let mut i = 0;
        while i < args.len() {
            let value = || -> Result<&String, String> {
                args.get(i + 1)
                    .ok_or_else(|| format!("{} needs a value", args[i]))
            };
            match args[i].as_str() {
                "--repeat" => {
                    o.repeat = value()?.parse().map_err(|_| "--repeat takes a number")?;
                    i += 2;
                }
                "--slice" => {
                    o.slice = value()?.parse().map_err(|_| "--slice takes a number")?;
                    i += 2;
                }
                "--max-block-insns" => {
                    o.max_block_insns = value()?
                        .parse()
                        .map_err(|_| "--max-block-insns takes a number")?;
                    i += 2;
                }
                "--no-spike" => {
                    o.spike = false;
                    i += 1;
                }
                other => {
                    return Err(format!(
                        "unknown argument `{other}`\nusage: cargo run --release -p xtask -- \
                         bench-k [--repeat N] [--slice N] [--max-block-insns N] [--no-spike]"
                    ));
                }
            }
        }
        if o.repeat == 0 || o.repeat.is_multiple_of(2) {
            return Err("--repeat must be odd and non-zero, so a median exists".to_string());
        }
        if o.slice == 0 {
            return Err("--slice must be at least 1".to_string());
        }
        Ok(o)
    }
}

// ------------------------------------------------------------------------------------------------
// Host fingerprint (every baseline is keyed by it, and numbers never cross hosts)
// ------------------------------------------------------------------------------------------------

/// OS, architecture and CPU model of the host the numbers were measured on.
struct Fingerprint {
    os: &'static str,
    arch: &'static str,
    cpu: String,
}

impl Fingerprint {
    fn of_host() -> Fingerprint {
        Fingerprint {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            cpu: crate::bench::cpu_model(),
        }
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} \"{}\"", self.os, self.arch, self.cpu)
    }
}

// ------------------------------------------------------------------------------------------------
// Building the guest kernels
// ------------------------------------------------------------------------------------------------

/// `<data root>/spikes/rv32-interp`, the preserved interpreter spike.
pub(crate) fn spike_dir() -> Result<PathBuf, String> {
    // The host's own data root (the override first, then the host default), so `bench-browser`
    // on Windows finds the spike where it finds the corpus.
    let root = hostdirs::data_root_of(&hostdirs::HostDirs::from_process(), None)?;
    let dir = root.join("spikes/rv32-interp");
    if !dir.join("Cargo.toml").is_file() {
        return Err(format!(
            "the preserved spike is not at {}; run with --no-spike to record our numbers only \
             (the gate needs both sides in the same job)",
            dir.display()
        ));
    }
    Ok(dir)
}

/// Absolute path of a toolchain program below `$HOME`, or the reason it cannot be used.
pub(crate) fn toolchain(relative: &str) -> Result<PathBuf, String> {
    let path = hostdirs::home()?.join(relative);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{} is missing; bench-k builds its kernels with the pinned ESP toolchain",
            path.display()
        ))
    }
}

/// Builds one kernel image and returns its bytes.
pub(crate) fn build_kernel(
    gcc: &Path,
    repo: &Path,
    out: &Path,
    workload: &Workload,
) -> Result<Vec<u8>, String> {
    std::fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let kernels = repo.join("bench/kernels");
    let elf = out.join(format!("{}.elf", workload.name));
    let bin = out.join(format!("{}.bin", workload.name));
    let status = Command::new(gcc)
        .args(GCC_ARGS)
        .arg("-fno-tree-loop-distribute-patterns")
        .arg(workload.opt)
        .arg("-T")
        .arg(kernels.join("link.ld"))
        .arg(kernels.join("start.S"))
        .arg(kernels.join(workload.source))
        .arg("-o")
        .arg(&elf)
        .arg("-lgcc")
        .status()
        .map_err(|e| format!("cannot run {}: {e}", gcc.display()))?;
    if !status.success() {
        return Err(format!("building {} failed", workload.name));
    }
    let objcopy = gcc.with_file_name(OBJCOPY);
    let status = Command::new(&objcopy)
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin)
        .status()
        .map_err(|e| format!("cannot run {}: {e}", objcopy.display()))?;
    if !status.success() {
        return Err(format!("objcopy of {} failed", workload.name));
    }
    std::fs::read(&bin).map_err(|e| format!("cannot read {}: {e}", bin.display()))
}

// ------------------------------------------------------------------------------------------------
// The engine side (the machine is `pemu_rv32::kbench`, shared with `bench-browser`)
// ------------------------------------------------------------------------------------------------

/// One measured run of one side.
struct Run {
    mips: f64,
    insns: u64,
    checksum: u32,
    /// Engine counters, for our side only (`EngineStats`).
    translations: u64,
    block_execs: u64,
    chain_hits: u64,
    slow_stores: u64,
    partial_blocks: u64,
    resumes: u64,
    /// CPU time of the run, all of it and the part on the performance cluster, or `None` where
    /// the host does not split it (`crate::bench::cores`).
    cores: Option<CoreTime>,
}

/// The runs of one side, with the median of their throughput.
struct Series {
    runs: Vec<Run>,
}

impl Series {
    fn median(&self) -> f64 {
        let mut mips: Vec<f64> = self.runs.iter().map(|r| r.mips).collect();
        mips.sort_by(|a, b| a.partial_cmp(b).expect("no NaN throughput"));
        mips[mips.len() / 2]
    }

    fn min(&self) -> f64 {
        self.runs.iter().map(|r| r.mips).fold(f64::MAX, f64::min)
    }

    fn max(&self) -> f64 {
        self.runs.iter().map(|r| r.mips).fold(0.0, f64::max)
    }

    fn first(&self) -> &Run {
        &self.runs[0]
    }

    /// Share of this side's CPU time, over all its runs, on the performance cluster.
    fn perf_share(&self) -> Option<f64> {
        cores::perf_share(self.runs.iter().map(|r| r.cores))
    }
}

/// The gate failure of `workload`, if it gates and failed: its ratio under [`GATE`] with a
/// verdict that stands ([`judge`]). A FAIL that a side's placement could have caused is not one;
/// [`report`] prints its NOT MEASURED line.
fn gate_failure(workload: &Workload, ours: &Series, theirs: &Series) -> Option<String> {
    let ratio = ours.median() / theirs.median();
    (workload.gates && judge(ratio, ours, theirs) == Ok(false)).then(|| {
        format!(
            "{}: {:.3} of the spike blockx median, below the {GATE:.2} gate",
            workload.name, ratio
        )
    })
}

/// The gate verdict on `ratio`, the median of `ours` over the median of `theirs`: `Ok(true)` for
/// PASS, `Ok(false)` for FAIL, or why the run cannot say.
///
/// A side under [`cores::RESIDENT_SHARE`] on the performance cluster ran on a mix of two core
/// speeds and can only have been slowed, which a ratio does not cancel: with six busy threads
/// beside it on an Apple M3 Pro, six runs read fw-Og ratios from 0.96 to 1.26 with 55 to 85 % of
/// either process's time on performance cores, against 0.97 to 1.00 with both at 99.7 % or more.
/// So a PASS needs the spike on the performance cluster and a FAIL needs our engine there.
fn judge(ratio: f64, ours: &Series, theirs: &Series) -> Result<bool, String> {
    let (o, t) = (ours.perf_share(), theirs.perf_share());
    let measures = |s: Option<f64>| cores::classify(&[s]).measures();
    let pass = ratio >= GATE;
    if (pass && measures(t)) || (!pass && measures(o)) {
        return Ok(pass);
    }
    let pct =
        |s: Option<f64>| s.map_or("no reading".to_string(), |s| format!("{:.0} %", s * 100.0));
    Err(format!(
        "core cluster {}: the engine ran {} and the spike {} of their CPU time on the performance \
         cluster, where {:.0} % is a measurement, and a {} the {} could have caused is not one",
        cores::classify(&[o, t]).as_str(),
        pct(o),
        pct(t),
        cores::RESIDENT_SHARE * 100.0,
        if pass { "PASS" } else { "FAIL" },
        if pass {
            "spike's placement"
        } else {
            "engine's placement"
        },
    ))
}

/// Runs `workload` through the engine `options.repeat` times.
fn measure_engine(image: &[u8], workload: &Workload, options: &Options) -> Result<Series, String> {
    let mut runs = Vec::with_capacity(options.repeat);
    for _ in 0..options.repeat {
        let mut machine = KernelMachine::new(image, workload.iters, options.max_block_insns)?;
        let cpu = CoreTime::now();
        let started = Instant::now();
        machine
            .run(options.slice)
            .map_err(|e| format!("{}: {e}", workload.name))?;
        let secs = started.elapsed().as_secs_f64();
        let cores = cpu.zip(CoreTime::now()).map(|(a, b)| b.since(a));
        let stats = machine.stats();
        let checksum = machine.checksum();
        if checksum != workload.checksum {
            return Err(format!(
                "{}: checksum 0x{checksum:08x}, expected 0x{:08x} (design-facts perf-A \
                 correctness anchor): the engine ran the wrong thing",
                workload.name, workload.checksum
            ));
        }
        // The second anchor: the same checksum reached with a different number of instructions
        // is a different program, whatever the throughput says (`bench/kernels/README.md`).
        let expected = workload.insns - EXIT_ECALL;
        if machine.insns() != expected {
            return Err(format!(
                "{}: the engine retired {} instructions, expected {expected} (the spike's {} \
                 less the exit `ecall` the hook takes): the kernel or the toolchain is not the \
                 one `bench/kernels/README.md` anchors",
                workload.name,
                machine.insns(),
                workload.insns
            ));
        }
        runs.push(Run {
            mips: machine.insns() as f64 / secs / 1e6,
            insns: machine.insns(),
            checksum,
            translations: stats.blocks_built,
            block_execs: stats.block_execs,
            chain_hits: stats.chain_hits,
            slow_stores: machine.slow_stores(),
            partial_blocks: stats.partial_blocks,
            resumes: stats.resumes,
            cores,
        });
    }
    Ok(Series { runs })
}

// ------------------------------------------------------------------------------------------------
// The spike side
// ------------------------------------------------------------------------------------------------

/// Builds the preserved spike in release, in this job: the gate compares sides of one job.
fn build_spike(dir: &Path) -> Result<(), String> {
    let status = Command::new("cargo")
        .current_dir(dir)
        .args(["build", "--release", "--quiet"])
        .status()
        .map_err(|e| format!("cannot run cargo in {}: {e}", dir.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("building the spike in {} failed", dir.display()))
    }
}

/// Runs the spike's `blockx` engine, the exact-deadline one and therefore the comparable one.
///
/// The spike prints one line as each run ends, so the child's CPU time is read as each line
/// arrives (`CoreTime::of_process`), and the difference between two readings is the core-class
/// split of the run in between, the same reading the engine side takes around each run.
/// The first run's reading starts at the spawn and so also holds the spike's
/// start-up, a few milliseconds against about a second of run.
fn measure_spike(bin: &Path, workload: &Workload, options: &Options) -> Result<Series, String> {
    use std::io::BufRead;

    let mut child = Command::new(bin)
        .args(["--workload", workload.name])
        .args(["--engines", "blockx"])
        .args(["--iters", &workload.iters.to_string()])
        .args(["--repeat", &options.repeat.to_string()])
        .args(["--slice", &options.slice.to_string()])
        .args(["--expect", &format!("0x{:08x}", workload.checksum)])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
    let pid = child.id();
    let mut last = CoreTime::of_process(pid);
    let mut lines = Vec::new();
    let stdout = child.stdout.take().expect("stdout is piped");
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("spike output is not UTF-8 text: {e}"))?;
        let cores = if line.contains("engine=blockx") {
            let now = CoreTime::of_process(pid);
            let spent = last.zip(now).map(|(a, b)| b.since(a));
            last = now;
            spent
        } else {
            None
        };
        lines.push((line, cores));
    }
    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for {}: {e}", bin.display()))?;
    if !status.success() {
        return Err(format!("{} exited with {status}", bin.display()));
    }
    let mut runs = Vec::new();
    for (line, cores) in &lines {
        let line = line.as_str();
        if !line.contains("engine=blockx") {
            continue;
        }
        if !line.contains("check=OK") {
            return Err(format!("the spike reported a checksum mismatch: {line}"));
        }
        runs.push(Run {
            mips: field(line, "mips=")?,
            insns: field(line, "insns=")? as u64,
            checksum: workload.checksum,
            translations: field(line, "translations=")? as u64,
            block_execs: field(line, "blocks=")? as u64,
            chain_hits: 0,
            slow_stores: field(line, "slow_stores=")? as u64,
            partial_blocks: 0,
            resumes: 0,
            cores: *cores,
        });
    }
    if runs.len() != options.repeat {
        return Err(format!(
            "the spike printed {} blockx runs, expected {}",
            runs.len(),
            options.repeat
        ));
    }
    // The spike checks its own checksum (`--expect` above, `check=OK`); the retired count is the
    // anchor this side adds, so both sides are held to the same two numbers and a ratio is only
    // ever taken between two runs of the same program (`bench/kernels/README.md`).
    if let Some(run) = runs.iter().find(|r| r.insns != workload.insns) {
        return Err(format!(
            "{}: the spike retired {} instructions, expected {}: the two sides are not running \
             the same kernel",
            workload.name, run.insns, workload.insns
        ));
    }
    Ok(Series { runs })
}

/// Value of `key` in one `k=v ...` line of the spike's output.
fn field(line: &str, key: &str) -> Result<f64, String> {
    let at = line
        .find(key)
        .ok_or_else(|| format!("the spike line has no `{key}`: {line}"))?;
    let rest = &line[at + key.len()..];
    let end = rest.find(' ').unwrap_or(rest.len());
    rest[..end].parse().map_err(|_| {
        format!(
            "the spike printed `{key}{}`, which is not a number",
            &rest[..end]
        )
    })
}

// ------------------------------------------------------------------------------------------------
// Reporting
// ------------------------------------------------------------------------------------------------

/// Prints one workload's record: the absolute numbers with the host fingerprint, then the ratio
/// that is the gate.
fn report(
    workload: &Workload,
    ours: &Series,
    theirs: Option<&Series>,
    host: &Fingerprint,
    options: &Options,
) {
    let run = ours.first();
    println!(
        "workload {} (iters {}, slice {}, max_block_insns {}, {} runs) on {host}",
        workload.name, workload.iters, options.slice, options.max_block_insns, options.repeat
    );
    println!(
        "  engine  median {:>7.1} Minsn/s  (min {:.1}, max {:.1})  insns {}  checksum 0x{:08x}",
        ours.median(),
        ours.min(),
        ours.max(),
        run.insns,
        run.checksum
    );
    println!(
        "          translations {}  block execs {}  insns/block {:.2}  chain hits {:.1} %  \
         slow stores {}  partial blocks {}  resumes {}",
        run.translations,
        run.block_execs,
        run.insns as f64 / run.block_execs.max(1) as f64,
        100.0 * run.chain_hits as f64 / run.block_execs.max(1) as f64,
        run.slow_stores,
        run.partial_blocks,
        run.resumes,
    );
    match theirs {
        Some(theirs) => {
            let t = theirs.first();
            println!(
                "  spike   median {:>7.1} Minsn/s  (min {:.1}, max {:.1})  insns {}  \
                 translations {}  slow stores {}",
                theirs.median(),
                theirs.min(),
                theirs.max(),
                t.insns,
                t.translations,
                t.slow_stores
            );
            let share = |s: Option<f64>| {
                s.map_or("no reading".to_string(), |s| format!("{:.1} %", s * 100.0))
            };
            println!(
                "  cores   engine {} and spike {} of their CPU time on the performance cluster",
                share(ours.perf_share()),
                share(theirs.perf_share())
            );
            let ratio = ours.median() / theirs.median();
            let verdict = match judge(ratio, ours, theirs) {
                _ if !workload.gates => {
                    "recorded, not gated (bench-Og is the upper bound)".to_string()
                }
                Ok(true) => "PASS".to_string(),
                Ok(false) => "FAIL".to_string(),
                Err(why) => format!("NOT MEASURED, {why}"),
            };
            println!("  ratio   {ratio:.3} of the spike blockx median: {verdict}");
        }
        None => println!("  spike   not measured (--no-spike): recorded only, no gate"),
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_on(all: u64, perf: u64) -> Run {
        Run {
            mips: 470.0,
            insns: 1,
            checksum: 0,
            translations: 0,
            block_execs: 0,
            chain_hits: 0,
            slow_stores: 0,
            partial_blocks: 0,
            resumes: 0,
            cores: Some(CoreTime { all, perf }),
        }
    }

    fn side(runs: &[(u64, u64)]) -> Series {
        Series {
            runs: runs.iter().map(|&(all, perf)| run_on(all, perf)).collect(),
        }
    }

    /// Classifies as the host this runs on would; the verdicts below need two core classes.
    fn two_classes() -> bool {
        cores::core_classes().is_some_and(|n| n > 1)
    }

    #[test]
    fn a_verdict_stands_only_when_the_placement_could_not_have_caused_it() {
        let resident = side(&[(100, 100), (100, 99), (100, 100)]);
        assert_eq!(resident.perf_share(), Some(299.0 / 300.0));
        assert_eq!(judge(1.0, &resident, &resident), Ok(true));
        assert_eq!(judge(0.8, &resident, &resident), Ok(false));
        if !two_classes() {
            return;
        }
        // One run of seven at 60 % is 94.3 % of the side's time: under the 95 %.
        let mut moved = vec![(100, 100); 6];
        moved.push((100, 60));
        let moved = side(&moved);
        // Our engine moved: a PASS stands (it can only have been faster), a FAIL does not.
        assert_eq!(judge(0.988, &moved, &resident), Ok(true));
        let why = judge(0.85, &moved, &resident).expect_err("a slowed engine explains a FAIL");
        assert!(
            why.starts_with("core cluster mixed")
                && why.contains("the engine ran 94 %")
                && why.contains("FAIL the engine's placement"),
            "{why}"
        );
        // The spike moved: a FAIL stands (the spike can only have been faster), a PASS does not.
        assert_eq!(judge(0.85, &resident, &moved), Ok(false));
        let why = judge(1.2, &resident, &moved).expect_err("a slowed spike explains a PASS");
        assert!(
            why.contains("the spike 94 %") && why.contains("PASS the spike's"),
            "{why}"
        );
        // Both sides clamped to the efficiency cluster: one kind of core, and not the one the
        // targets are for, so neither verdict stands.
        let clamped = side(&[(100, 0), (100, 1)]);
        // The gate: only a FAIL that stands fails the command, and only on the gating workload.
        let slow = Series {
            runs: vec![Run {
                mips: 400.0,
                ..run_on(100, 100)
            }],
        };
        let failure = gate_failure(&WORKLOADS[0], &slow, &resident).expect("0.851 fails");
        assert!(
            failure.contains("0.851 of the spike blockx median"),
            "{failure}"
        );
        assert_eq!(
            gate_failure(&WORKLOADS[1], &slow, &resident),
            None,
            "bench-Og never gates"
        );
        assert_eq!(gate_failure(&WORKLOADS[0], &resident, &resident), None);
        let slow_moved = Series {
            runs: moved
                .runs
                .iter()
                .map(|r| Run { mips: 400.0, ..*r })
                .collect(),
        };
        assert_eq!(gate_failure(&WORKLOADS[0], &slow_moved, &resident), None);
        let why = judge(1.0, &clamped, &clamped).expect_err("clamped");
        assert!(why.starts_with("core cluster efficiency"), "{why}");
        assert!(judge(0.5, &clamped, &clamped).is_err());
    }

    /// The spike side's reading, through a stand-in spike that prints the spike's run lines:
    /// every run line gets a reading of the child and nothing else does.
    #[cfg(target_os = "macos")]
    #[test]
    fn each_spike_run_line_gets_the_childs_core_time() {
        let dir = std::env::temp_dir().join(format!("pemu-bench-k-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("spike.sh");
        let w = &WORKLOADS[0];
        let line = format!(
            "engine=blockx check=OK mips=470.0 insns={} translations=352 blocks=1 slow_stores=0",
            w.insns
        );
        // A busy loop between lines, so each run has CPU time of its own to read.
        let script = format!(
            "#!/bin/sh\necho header\nfor i in 1 2 3; do\n  n=0; while [ $n -lt 20000 ]; do \
             n=$((n+1)); done\n  echo '{line}'\ndone\n"
        );
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        let options = Options {
            repeat: 3,
            slice: DEFAULT_SLICE,
            max_block_insns: 64,
            spike: true,
        };
        let series = measure_spike(&bin, w, &options).expect("the stand-in spike parses");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(series.runs.len(), 3);
        for run in &series.runs {
            let t = run
                .cores
                .expect("every run line has a reading of the child");
            assert!(t.all > 0 && t.perf <= t.all, "{t:?}");
        }
    }
}
