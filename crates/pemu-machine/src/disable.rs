//! Models disabled for one machine (`MachineConfig::disabled_models`, `--disable-model`).
//!
//! A disabled block keeps its `c3_devices!` row, but every access to its window reaches a
//! [`RegBank`] instead: it stores and reads back, records first touches, never stops and raises no
//! wiring or interrupt. A guest busy-waiting on the block is then reported as `StopReason::Stuck`.
//! The model still receives resets, which the guest cannot see; its scheduled events are dropped
//! (counted in [`Machine::disabled_model_events`]), since a store-only stub raises nothing.

use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_rv32::bus::HartView;
use pemu_soc_c3::SocCx;
use pemu_soc_c3::periph::store_only::{RegBank, TouchTag};
use pemu_soc_c3::periph::{BLOCKS, lookup};

use crate::machine::Machine;

#[derive(Debug, Default)]
pub(crate) struct DisabledModels {
    mask: u128,
    banks: Vec<(PeriphId, RegBank)>,
    pub(crate) dropped_events: u64,
}

impl DisabledModels {
    #[inline]
    pub(crate) fn contains(&self, id: PeriphId) -> bool {
        u32::from(id.0) < 128 && self.mask & (1u128 << id.0) != 0
    }

    fn bank(&mut self, id: PeriphId) -> Option<&mut RegBank> {
        self.banks
            .iter_mut()
            .find(|(b, _)| *b == id)
            .map(|(_, bank)| bank)
    }

    #[inline]
    pub(crate) fn load(
        &mut self,
        addr: u32,
        size: u8,
        hart: &HartView,
        cx: &mut SocCx,
    ) -> Option<u32> {
        if self.mask == 0 {
            return None;
        }
        let (id, off) = lookup(addr)?;
        if !self.contains(id) {
            return None;
        }
        let width = pemu_soc_c3::mmio::reg_size(size)?;
        cx.set_now(hart);
        let (now, ledger) = (cx.now, &mut *cx.periph.ledger);
        Some(self.bank(id)?.read(off, width, now, ledger, tag(id)))
    }

    #[inline]
    pub(crate) fn store(
        &mut self,
        addr: u32,
        size: u8,
        val: u32,
        hart: &HartView,
        cx: &mut SocCx,
    ) -> bool {
        if self.mask == 0 {
            return false;
        }
        let Some((id, off)) = lookup(addr) else {
            return false;
        };
        let Some(width) = pemu_soc_c3::mmio::reg_size(size) else {
            return false;
        };
        if !self.contains(id) {
            return false;
        }
        cx.set_now(hart);
        let (now, ledger) = (cx.now, &mut *cx.periph.ledger);
        match self.bank(id) {
            Some(bank) => {
                bank.write(off, width, val, now, ledger, tag(id));
                true
            }
            None => false,
        }
    }
}

impl DisabledModels {
    pub(crate) fn disable(&mut self, name: &str) -> bool {
        let Some(block) = BLOCKS.iter().find(|b| b.name == name) else {
            return false;
        };
        if block.id.0 >= 128 {
            return false;
        }
        if !self.contains(block.id) {
            self.mask |= 1u128 << block.id.0;
            self.banks.push((block.id, RegBank::new(block.size)));
        }
        true
    }
}

/// The `soc.disabled` snapshot section: each disabled block's store by name, in name order. Which
/// blocks are disabled is configuration, checked by the run identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub(crate) struct DisabledSection {
    banks: Vec<(String, RegBank)>,
    dropped_events: u64,
}

/// [`DisabledSection`] read with the store's fields exposed, in the same postcard layout, so a
/// restore can refuse a store whose window is not its block's before anything is written.
#[derive(Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub(crate) struct DisabledShape {
    banks: Vec<(String, BankShape)>,
    /// Read only to consume its bytes (postcard is positional).
    _dropped_events: u64,
}

#[derive(Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct BankShape {
    vals: Vec<u32>,
    touched: Vec<u64>,
}

impl DisabledModels {
    pub(crate) fn section(&self) -> DisabledSection {
        let mut banks: Vec<(String, RegBank)> = self
            .banks
            .iter()
            .map(|(id, bank)| (BLOCKS[usize::from(id.0)].name.to_string(), bank.clone()))
            .collect();
        banks.sort_by(|a, b| a.0.cmp(&b.0));
        DisabledSection {
            banks,
            dropped_events: self.dropped_events,
        }
    }

    /// Whether `shape` names exactly this machine's disabled blocks, each with a store of its
    /// block's window. The configuration decides the blocks, so a mismatch is a corrupt section.
    pub(crate) fn fits(&self, shape: &DisabledShape) -> bool {
        let mut own: Vec<(&str, u32)> = self
            .banks
            .iter()
            .map(|(id, _)| {
                let block = &BLOCKS[usize::from(id.0)];
                (block.name, block.size)
            })
            .collect();
        own.sort_unstable();
        own.len() == shape.banks.len()
            && own
                .iter()
                .zip(&shape.banks)
                .all(|((name, size), (got, bank))| {
                    let regs = size.div_ceil(4) as usize;
                    *name == got.as_str()
                        && bank.vals.len() == regs
                        && bank.touched.len() == regs.div_ceil(64)
                })
    }

    pub(crate) fn restore(&mut self, s: DisabledSection) {
        for (name, bank) in s.banks {
            if let Some(block) = BLOCKS.iter().find(|b| b.name == name)
                && let Some(own) = self.bank(block.id)
            {
                *own = bank;
            }
        }
        self.dropped_events = s.dropped_events;
    }
}

fn tag(id: PeriphId) -> TouchTag {
    TouchTag {
        periph: id,
        allowlisted: false,
    }
}

impl Machine {
    /// Replaces the block's model with a register store and records it in
    /// `MachineConfig::disabled_models`, so it changes run identity. `false` for an unknown block.
    /// Call it before the first run: the store starts at zero, not at what the model held.
    pub fn disable_model(&mut self, name: &str) -> bool {
        if !self.disabled.disable(name) {
            return false;
        }
        if !self.cfg.disabled_models.iter().any(|n| n == name) {
            self.cfg.disabled_models.push(name.to_string());
        }
        self.poll.invalidate();
        true
    }

    pub fn disabled_model_events(&self) -> u64 {
        self.disabled.dropped_events
    }

    pub fn model_disabled(&self, id: PeriphId) -> bool {
        self.disabled.contains(id)
    }
}
