//! RV32IMC CPU: decoder, predecoded `Op`, semantics, CSRs, traps, PMP, the block-cache engine,
//! the reference single-step interpreter and the `Bus` trait. Behavior follows the ESP32-C3 TRM
//! chapter 1 (RISC-V CPU).

pub mod bus;
pub mod cache;
pub mod cost;
pub mod csr;
pub mod decode;
pub mod disasm;
pub mod engine;
pub mod exec;
pub mod fuse;
pub mod kbench;
pub mod op;
pub mod pmp;
pub mod refstep;
pub mod resume;
pub mod spmon;
pub mod trap;
