//! Command API: registry, commands, output shaping, redaction, matchers, receipts, scenarios,
//! instances, clock leases and the per-host availability table.

pub mod args;
pub mod artifact_io;
pub mod commands;
pub mod elf;
pub mod error;
pub mod host_support;
pub mod instance;
pub mod lease;
pub mod matchers;
pub mod output;
pub mod pool;
pub mod receipt;
pub mod redact;
pub mod registry;
pub mod scenario;
pub mod secret_set;
pub mod session;
pub mod shape;
pub mod spec;
