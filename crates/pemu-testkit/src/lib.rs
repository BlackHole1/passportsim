//! Test support: corpus locator, `RegHarness`, `MockBoard`, golden runner, tier naming and filter
//! sets, and `MockMachine`, a scripted `MachineApi`. A host crate, so `std` is used freely;
//! nothing here is reachable from the core crates.

pub mod corpus;
pub mod golden;
pub mod mock_board;
pub mod mock_machine;
// Our side of the boot-phase oracle diff.
pub mod oracle_run;
pub mod reg_harness;
pub mod tiers;
