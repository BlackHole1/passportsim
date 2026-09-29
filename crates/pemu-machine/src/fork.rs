//! `Machine::fork`: a new machine composed from the same configuration and `Arc<Assets>`, with
//! this machine's snapshot restored into it. ROM and flash base are shared; RAM and the flash
//! overlay are copied. The executor, both fast-forward switches and `max_slice` are carried over:
//! they set how the parent was run, not guest state, and change no result.

use std::sync::Arc;

use pemu_core::snap::{LivePolicy, SnapError, SnapOpts};
use pemu_soc_c3::flash_store::FlashStore;

use crate::machine::Machine;

impl Machine {
    /// Fork this machine. With a live bridge attached, `LivePolicy::Refuse` returns
    /// [`SnapError::LiveBridge`] and `LivePolicy::LinkDown` journals a link-down in the copy at the
    /// fork instant.
    pub fn fork(&self, live: LivePolicy) -> Result<Machine, SnapError> {
        if live == LivePolicy::Refuse {
            self.refuse_live_bridge()?;
        }
        let flash = FlashStore::new(Arc::clone(self.soc.flash.image())).map_err(|_| {
            SnapError::Malformed {
                at: "flash_delta",
                reason: "the base image is not a flash image",
            }
        })?;
        let mut copy = Machine::compose(self.cfg.clone(), Arc::clone(&self.assets), Some(flash))
            .map_err(|_| SnapError::Malformed {
                at: "header",
                reason: "the configuration no longer composes a machine",
            })?;
        copy.restore(&self.snapshot(SnapOpts::default()))?;
        copy.set_executor(self.executor);
        copy.set_rom_delay_ff(self.rom_delay.enabled);
        copy.set_max_slice(self.max_slice);
        if live == LivePolicy::LinkDown {
            copy.link_down();
        }
        Ok(copy)
    }
}
