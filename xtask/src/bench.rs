//! `cargo xtask bench`: the F-suites, their metrics, the JSON history with its 10 % regression
//! gate, and `--check-model`.
//!
//! F1 to F7 run on the machine over the pinned corpus images: each boots `official` or `pk` from
//! reset to a console line, then drives its own body (idle, journaled clicks, a demo card, F7's
//! scripted BLE central, [`BleLeg`]) while the runner times each 100 ms virtual window. A suite
//! whose corpus id is absent is NOT_RUN, never a failure ([`corpus_image`]). `rom-boot` boots the
//! bundled ROM with no corpus: it is not an F-suite and never stands in for one in a gate; it
//! keeps `--check-model` answerable on a host without a corpus.
//!
//! The machine reads no host clock, so busy speed S and idle cost c come from the cost model
//! `host = insns / S + idle x c` over whole windows ([`Metrics::from_phases`]). Each suite runs a
//! calibration pass ([`CALIBRATION_PS`]), the boot, an unmeasured setup and the body; every
//! reported quantity but S comes from the workload's own phase, so a gate defined "after boot"
//! never sees a boot window ([`SuiteRun::measured`]).
//!
//! A repeat loop reports its fastest run on the performance cluster, not the median ([`settled`],
//! [`reported_run`], [`Cores`]). A run on a contended host or on the wrong core cluster is printed
//! NOT MEASURED and never baselines ([`contention`], [`finish`]). The gates are the 10 % trend
//! gate ([`regressions`]), the M5 and M6 hard gates under `--gate-exits` ([`EXIT_GATES`])
//! and `--check-model` ([`model_report`]); busy-MIPS floors belong to `bench-browser`, not here.
//! The history lives under the data root ([`history_path`]).
//!
//! Files under `bench/`: `model.rs` (the cost model and `--check-model`), `metrics.rs`,
//! `history.rs` (the history and its trend gate), `suites.rs` (the workload table), `run.rs` (the
//! F-suite runner), `gates.rs` (the hard gates), `cli.rs` (the command line and the record keys),
//! `wav.rs` (the audio artifact), plus `browser.rs` (`bench-browser`) and `cores.rs`.
//!
//! [`BleLeg`]: suites::BleLeg
//! [`corpus_image`]: run::corpus_image
//! [`Metrics::from_phases`]: metrics::Metrics::from_phases
//! [`CALIBRATION_PS`]: suites::CALIBRATION_PS
//! [`SuiteRun::measured`]: run::SuiteRun::measured
//! [`settled`]: run::settled
//! [`reported_run`]: run::reported_run
//! [`Cores`]: history::Cores
//! [`contention`]: history::contention
//! [`finish`]: cli::finish
//! [`regressions`]: history::regressions
//! [`EXIT_GATES`]: gates::EXIT_GATES
//! [`model_report`]: model::model_report
//! [`history_path`]: cli::history_path

pub(crate) mod browser;
mod cli;
pub(crate) mod cores;
mod gates;
mod history;
mod metrics;
mod model;
mod run;
mod suites;
#[cfg(test)]
mod tests;
mod wav;

pub use cli::run;
pub(crate) use history::{cores, cpu_model, load_average};
pub use suites::any_suite_runnable;
