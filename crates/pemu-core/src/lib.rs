//! Deterministic core: `VTime`, `Clock`, `Scheduler`, `DetRng`, the input journal, reset causes,
//! fidelity classes, the generated `IrqSource`, `RegStore`, `HostIo` rings, trace records, the
//! snapshot codec and the AES block cipher the SoC and the BLE controller share.

pub mod aes;
pub mod clock;
pub mod fidelity;
pub mod hostio;
pub mod input;
pub mod irq_source;
pub mod journal;
pub mod regstore;
pub mod reset;
pub mod rng;
pub mod sched;
pub mod snap;
pub mod time;
pub mod trace;

/// Re-export of `serde`, so a crate that may depend only on `pemu-core` (such as `pemu-rv32`)
/// derives with `#[serde(crate = "pemu_core::serde")]` without a direct dependency.
pub use serde;
