//! [`MachineApi`] and the facade's small types.

use pemu_core::hostio::HostIo;
use pemu_core::input::InputEvent;
use pemu_core::journal::Origin;
use pemu_core::time::VTime;
use pemu_loader::app_desc::AppDesc;
use pemu_soc_c3::Soc;

use super::{Machine, Receipt};
use crate::run::{RunLimits, RunOutcome};

/// The machine facade as a trait. `Machine` implements it; `MockMachine` (`pemu-testkit`) is the
/// test double. Constructors stay inherent so the trait is object safe; `snapshot`, `restore`,
/// `fork` and `state_hash` are not part of it.
pub trait MachineApi {
    /// Run until a limit or a stop; never reads host time.
    fn run(&mut self, lim: RunLimits) -> RunOutcome;
    fn input(&mut self, at: At, ev: InputEvent) -> Result<u64, InputError>;
    /// Journal an input with the origin it came from. The default drops the origin, for a test
    /// double; a backend over a real machine forwards it.
    fn input_from(
        &mut self,
        at: At,
        origin: pemu_core::journal::Origin,
        ev: InputEvent,
    ) -> Result<u64, InputError> {
        let _ = origin;
        self.input(at, ev)
    }
    fn io(&mut self) -> &mut HostIo;
    fn now(&self) -> VTime;
    fn guest_mem(&mut self) -> GuestMem<'_>;
    /// Drains ledger deltas since the last call.
    fn receipt(&mut self) -> Receipt;
    /// Whether any input this machine was built from carries device-derived bytes.
    ///
    /// Required, not defaulted: "not tainted" is the unsafe answer, and `pemu_host::boot_cache`
    /// would put a forgetful backend's entries on disk.
    fn is_tainted(&self) -> bool;
    /// The guest-heap blocks the bound radio modules hold for the blobs they replaced, so
    /// `inspect heap` can label them.
    fn heap_ledger(&self) -> Vec<crate::hle::HleHeapBlock> {
        Vec::new()
    }
    /// Live host bridges this machine holds open. While nonzero, pacing is fixed at
    /// `Wall { rate: 1 }` and a pause is refused, because the peer answers in host time.
    fn live_bridges(&self) -> u32 {
        0
    }
    /// The snapshot bytes of the bound radio module `module`, or `None` when none is bound. The
    /// `ble_*` commands read the scripted central through this, decoding with the module's own
    /// codec (`pemu_radio::ble::vhci::BleState::decode`). Reading changes no `state_hash`.
    fn radio_module_state(&self, module: &str) -> Option<&[u8]> {
        let _ = module;
        None
    }
    /// The app descriptor of the running firmware, which `status` reports as the build.
    fn app_desc(&self) -> Option<AppDesc> {
        None
    }
}

impl MachineApi for Machine {
    fn run(&mut self, lim: RunLimits) -> RunOutcome {
        Machine::run(self, lim)
    }

    fn input(&mut self, at: At, ev: InputEvent) -> Result<u64, InputError> {
        Machine::input(self, at, ev)
    }

    fn input_from(&mut self, at: At, origin: Origin, ev: InputEvent) -> Result<u64, InputError> {
        Machine::input_from(self, at, origin, ev)
    }

    fn io(&mut self) -> &mut HostIo {
        Machine::io(self)
    }

    fn now(&self) -> VTime {
        Machine::now(self)
    }

    fn guest_mem(&mut self) -> GuestMem<'_> {
        Machine::guest_mem(self)
    }

    fn receipt(&mut self) -> Receipt {
        Machine::receipt(self)
    }

    fn is_tainted(&self) -> bool {
        Machine::is_tainted(self)
    }

    fn heap_ledger(&self) -> Vec<crate::hle::HleHeapBlock> {
        Machine::heap_ledger(self)
    }

    fn live_bridges(&self) -> u32 {
        Machine::live_bridges(self)
    }

    fn radio_module_state(&self, module: &str) -> Option<&[u8]> {
        Machine::radio_module_state(self, module)
    }

    fn app_desc(&self) -> Option<AppDesc> {
        self.assets().app_desc()
    }
}

/// When a journaled input applies.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum At {
    Now,
    Vt(VTime),
}

/// Error of `Machine::input`. Field-less because `pemu_testkit::mock_machine` builds it as
/// `InputError {}`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputError {}

/// Read-only view of guest memory for pemu-introspect. It reads through [`Soc::load_mem`], so it
/// sees the arena as a guest load does and never reaches a peripheral: an introspection read must
/// have no side effect.
pub struct GuestMem<'a> {
    pub(super) soc: Option<&'a Soc>,
}

impl GuestMem<'_> {
    /// A view over no memory: every read answers `None`.
    pub fn empty() -> GuestMem<'static> {
        GuestMem { soc: None }
    }

    /// The `size` bytes at `addr` (1, 2 or 4), or `None` when the range is not backed memory.
    pub fn load(&self, addr: u32, size: u8) -> Option<u32> {
        self.soc?.load_mem(addr, size)
    }

    pub fn is_empty(&self) -> bool {
        self.soc.is_none()
    }
}

impl Default for GuestMem<'_> {
    fn default() -> Self {
        GuestMem::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_facade<M: MachineApi>() {}

    #[test]
    fn machine_implements_the_object_safe_facade() {
        assert_facade::<Machine>();
        let api: Option<&mut dyn MachineApi> = None;
        assert!(api.is_none());
    }

    #[test]
    fn at_keeps_its_virtual_time() {
        assert_eq!(At::Vt(VTime(7)), At::Vt(VTime(7)));
        assert_ne!(At::Now, At::Vt(VTime(0)));
    }
}
