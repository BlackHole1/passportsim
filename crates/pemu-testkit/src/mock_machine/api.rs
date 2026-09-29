//! `MachineApi` for `MockMachine`: every call is recorded, then answered from the script.

use pemu_core::hostio::HostIo;
use pemu_core::input::InputEvent;
use pemu_core::time::VTime;
use pemu_machine::MachineApi;
use pemu_machine::machine::{At, GuestMem, InputError, Receipt};
use pemu_machine::run::{RunLimits, RunOutcome};

use super::{MockCall, MockMachine};

impl MachineApi for MockMachine {
    /// Release due outputs and stop at a scripted stop, a matcher hit or a limit. With no limit
    /// and nothing left to release, the mock reports `Deadlock` at the current time.
    /// `ff_insns` and `idle_ps` are always 0.
    fn run(&mut self, lim: RunLimits) -> RunOutcome {
        let start = self.insns_at(self.vt());
        let (reason, vt) = self.advance(&lim);
        let insns = self.insns_at(vt) - start;
        self.record(MockCall::Run {
            until: lim.until,
            max_insns: lim.max_insns,
            stops: lim.stops,
            reason: reason.clone(),
            vt,
            insns,
        });
        RunOutcome {
            reason,
            vt,
            insns,
            ff_insns: 0,
            idle_ps: 0,
        }
    }

    /// Journal the input and schedule matching reactions; an instant before `now` is refused.
    fn input(&mut self, at: At, ev: InputEvent) -> Result<u64, InputError> {
        let (input, result) = self.journal_input(at, &ev);
        self.record(MockCall::Input {
            at,
            input,
            result: result.clone(),
        });
        result
    }

    /// An empty `HostIo`; scripted outputs are read through the mock's inherent methods.
    fn io(&mut self) -> &mut HostIo {
        self.record(MockCall::Io);
        self.host_io()
    }

    fn now(&self) -> VTime {
        let vt = self.vt();
        self.record(MockCall::Now { vt });
        vt
    }

    /// An empty guest view; the mock exposes no guest memory.
    fn guest_mem(&mut self) -> GuestMem<'_> {
        self.record(MockCall::GuestMem);
        GuestMem::empty()
    }

    /// A mock is built from no input, so nothing device-derived can have reached it. Stated
    /// rather than defaulted, so a backend cannot answer the unsafe word by forgetting.
    fn is_tainted(&self) -> bool {
        false
    }
    fn receipt(&mut self) -> Receipt {
        let vt = self.vt();
        self.record(MockCall::Receipt { vt });
        Receipt::default()
    }
}
