//! Verification tooling: console normalizer, bands, QEMU trace ingest, per-block LCS diff, known
//! diffs, coverage histograms, call-trace diff, the boot-phase diff and the calibration fit.

pub mod audio;
pub mod bands;
pub mod calibrate;
pub mod calltrace;
pub mod goldens;
pub mod hist;
pub mod known_diffs;
pub mod lcs;
pub mod normalize;
pub mod phase;
pub mod qemu_ingest;
pub mod spec_toml;
