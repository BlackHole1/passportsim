//! Machine composition: run loop, stops, poll and ROM delay fast-forward, hang detector,
//! snapshot, fork, pacing policy types and state hash.

pub mod apply;
pub mod config;
pub mod determinism;
pub mod disable;
pub mod dma;
pub mod executor;
#[cfg(all(test, feature = "bundled-rom"))]
mod ff_tests;
pub mod flash;
pub mod fork;
pub mod hang;
pub mod hle;
pub mod machine;
pub mod poll_ff;
pub mod rom_delay;
pub mod run;
pub mod sleep;
pub mod snapshot;
pub mod state_hash;
pub mod stops;
pub mod wiring_counts;

pub use executor::Executor;
pub use machine::{Machine, MachineApi};
pub use run::{IdleAction, IdlePolicy, SkipToNextEvent};
pub use snapshot::SnapshotMachine;
