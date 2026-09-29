//! High-level emulation: hooks, magic PCs, binding profiles, `guest_call`, continuations, radio
//! workers, tripwires, observe hooks and log synthesis, bound against the ESP-IDF 5.5.3 symbol
//! data under `specs/hle/idf-5.5.3/`.

pub mod binding;
pub mod continuation;
pub mod core;
pub mod guest_call;
pub mod hooks;
pub mod image_symbols;
pub mod log_synth;
pub mod magic;
pub mod observe;
#[cfg(test)]
mod rtos_model;
#[cfg(test)]
pub mod test_guest;
pub mod tripwire;
#[cfg(test)]
mod u4_gate;
#[cfg(test)]
mod u5_worker;
pub mod worker;

pub use binding::{RadioModule, bind};
