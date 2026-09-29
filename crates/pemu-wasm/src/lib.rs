//! Raw C ABI over the machine and the command API. No wasm-bindgen. [`layout`] is the single
//! source of the boundary: [`tsgen`] renders it as `web/src/worker/layout.ts` and [`abi`] is the
//! only code that crosses it.

pub mod abi;
pub mod commands;
pub mod config;
pub mod input_batch;
pub mod instance;
pub mod introspect;
pub mod io_view;
pub mod layout;
pub mod tsgen;
