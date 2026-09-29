//! Magic PCs: the nested-call return address and the worker and ISR entry addresses, all in an
//! unused range of the rev101 ROM.
//!
//! The range and its five allocations come from `specs/magic-pcs.toml`, which `pemu-loader`
//! proves unused against every pinned ROM at load. The runtime half of the proof: translating any
//! other PC of the range raises `Tripwire(MagicRangeFetch)`.

use pemu_loader::rom::{MAGIC_PCS_TOML, magic_range};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MagicPcs {
    /// `ra` of every nested guest call.
    pub ret: u32,
    pub wifi_worker: u32,
    pub bt_worker: u32,
    pub wifi_isr: u32,
    pub bt_isr: u32,
}

/// In `MagicKind` order.
const SPEC_NAMES: [&str; 5] = ["RETURN", "WIFI_WORKER", "BT_WORKER", "WIFI_ISR", "BT_ISR"];

impl MagicPcs {
    /// `None` when the spec file does not name all five.
    pub fn from_spec() -> Option<MagicPcs> {
        let mut pcs = [0u32; 5];
        let mut name: Option<&str> = None;
        for line in MAGIC_PCS_TOML.lines().map(str::trim) {
            if line == "[[magic]]" {
                name = None;
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.split('#').next()?.trim();
            match key.trim() {
                "name" => {
                    name = SPEC_NAMES
                        .iter()
                        .copied()
                        .find(|n| value == format!("\"{n}\""))
                }
                "pc" => {
                    let Some(found) = name.take() else { continue };
                    let hex = value.strip_prefix("0x").or(value.strip_prefix("0X"))?;
                    let slot = SPEC_NAMES.iter().position(|n| *n == found)?;
                    pcs[slot] = u32::from_str_radix(hex, 16).ok()?;
                }
                _ => {}
            }
        }
        if pcs.contains(&0) {
            return None;
        }
        Some(MagicPcs {
            ret: pcs[0],
            wifi_worker: pcs[1],
            bt_worker: pcs[2],
            wifi_isr: pcs[3],
            bt_isr: pcs[4],
        })
    }

    /// The inclusive range of `specs/magic-pcs.toml` `[range]`. `None` when the spec states no
    /// bounds, in which case the loader already refused every ROM.
    pub fn range() -> Option<(u32, u32)> {
        magic_range()
    }

    pub fn kind_at(&self, pc: u32) -> Option<MagicKind> {
        Some(match pc {
            _ if pc == self.ret => MagicKind::Return,
            _ if pc == self.wifi_worker => MagicKind::WifiWorker,
            _ if pc == self.bt_worker => MagicKind::BtWorker,
            _ if pc == self.wifi_isr => MagicKind::WifiIsr,
            _ if pc == self.bt_isr => MagicKind::BtIsr,
            _ => return None,
        })
    }

    pub fn pc_of(&self, kind: MagicKind) -> u32 {
        match kind {
            MagicKind::Return => self.ret,
            MagicKind::WifiWorker => self.wifi_worker,
            MagicKind::BtWorker => self.bt_worker,
            MagicKind::WifiIsr => self.wifi_isr,
            MagicKind::BtIsr => self.bt_isr,
        }
    }
}

/// Which magic PC a `HookKind::Magic` hook sits on.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MagicKind {
    /// Return from a nested guest call.
    Return,
    WifiWorker,
    BtWorker,
    /// Bumps `HleSection::isr_generation`, as does `BtIsr`.
    WifiIsr,
    BtIsr,
}

impl MagicKind {
    /// In `specs/magic-pcs.toml` order.
    pub const ALL: [MagicKind; 5] = [
        MagicKind::Return,
        MagicKind::WifiWorker,
        MagicKind::BtWorker,
        MagicKind::WifiIsr,
        MagicKind::BtIsr,
    ];

    pub fn is_isr(self) -> bool {
        matches!(self, MagicKind::WifiIsr | MagicKind::BtIsr)
    }

    /// A worker task created by `xTaskCreatePinnedToCore` enters through `vPortTaskWrapper`.
    pub fn is_worker(self) -> bool {
        matches!(self, MagicKind::WifiWorker | MagicKind::BtWorker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_come_from_the_spec_file() {
        let pcs = MagicPcs::from_spec().expect("specs/magic-pcs.toml names five allocations");
        // Asserted so a spec edit that moves one is visible.
        assert_eq!(pcs.ret, 0x4005_ECC0);
        assert_eq!(pcs.wifi_worker, 0x4005_ECC4);
        assert_eq!(pcs.bt_worker, 0x4005_ECC8);
        assert_eq!(pcs.wifi_isr, 0x4005_ECCC);
        assert_eq!(pcs.bt_isr, 0x4005_ECD0);
        for kind in MagicKind::ALL {
            assert_eq!(pcs.kind_at(pcs.pc_of(kind)), Some(kind));
        }
    }

    #[test]
    fn every_allocation_lies_inside_the_proved_range() {
        let pcs = MagicPcs::from_spec().expect("spec");
        let (lo, hi) = MagicPcs::range().expect("specs/magic-pcs.toml states the range");
        for kind in MagicKind::ALL {
            let pc = pcs.pc_of(kind);
            assert!(
                pc >= lo && pc <= hi,
                "{kind:?} at {pc:#x} outside {lo:#x}..={hi:#x}"
            );
        }
    }

    #[test]
    fn an_unallocated_pc_of_the_range_is_a_magic_range_fetch() {
        let pcs = MagicPcs::from_spec().expect("spec");
        let (lo, hi) = MagicPcs::range().expect("range");
        for pc in [pcs.bt_isr + 4, hi & !3] {
            assert!(pc >= lo && pc <= hi, "{pc:#x}");
            assert_eq!(pcs.kind_at(pc), None, "{pc:#x}");
        }
        assert_eq!(pcs.kind_at(lo), Some(MagicKind::Return));
    }

    #[test]
    fn an_ordinary_guest_pc_is_not_magic() {
        let pcs = MagicPcs::from_spec().expect("spec");
        // Two app-flash PCs of the corpus hook table.
        for pc in [0x4201_71D6, 0x4204_B470] {
            assert_eq!(pcs.kind_at(pc), None);
        }
    }

    #[test]
    fn isr_and_worker_entries_are_classified() {
        assert!(MagicKind::WifiIsr.is_isr() && MagicKind::BtIsr.is_isr());
        assert!(MagicKind::WifiWorker.is_worker() && MagicKind::BtWorker.is_worker());
        assert!(!MagicKind::Return.is_isr() && !MagicKind::Return.is_worker());
    }
}
