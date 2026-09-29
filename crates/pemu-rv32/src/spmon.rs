//! CPU-side mirror of the ASSIST_DEBUG hardware stack guard (`specs/blocks/assist_debug.toml`),
//! checked after every write to `x2`.
//!
//! The mirror keeps every function prologue at one predictable branch ([`SpMonitor::armed`]):
//! IDF turns the monitor off and on around every interrupt entry and exit. The CPU checks the
//! bounds and ends the block on a violation; the peripheral latches RAW and SP_PC, drives source
//! 54 and handles INTR_CLR. Every ASSIST_DEBUG write returns `OkStop`, after which the engine
//! re-reads `Bus::sp_monitor()`, so a new bound applies from the next instruction.
//!
//! Checking on `sp` writes rather than continuously, with inclusive bounds, is UNVERIFIED on
//! silicon; it is enough for IDF, whose stacks grow down through `addi sp, sp, -N`.

use pemu_core::serde::{Deserialize, Serialize};

/// Mirror of `INTR_ENA` bits 8 and 9 and the bounds (roles per IDF `assist_debug_ll.h:13,64-79`).
#[derive(Copy, Clone, Default, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct SpMonitor {
    pub on_min: bool,
    pub on_max: bool,
    pub min: u32,
    pub max: u32,
}

impl SpMonitor {
    /// Either monitor bit is on: a gate that keeps [`SpMonitor::check`] off the hot path, not its
    /// precondition.
    #[inline(always)]
    pub fn armed(&self) -> bool {
        self.on_min || self.on_max
    }

    /// Checks a freshly written `sp` against the enabled bounds (inclusive, UNVERIFIED).
    #[inline(always)]
    pub fn check(&self, sp: u32) -> Option<SpSpill> {
        if self.on_min && sp < self.min {
            Some(SpSpill::Min)
        } else if self.on_max && sp > self.max {
            Some(SpSpill::Max)
        } else {
            None
        }
    }
}

/// Which stack bound a write to `x2` violated, carried by `Exit::SpSpill`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SpSpill {
    Min,
    Max,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(on_min: bool, on_max: bool) -> SpMonitor {
        SpMonitor {
            on_min,
            on_max,
            min: 0x3FC8_1000,
            max: 0x3FC8_2000,
        }
    }

    #[test]
    fn default_monitor_is_off_and_never_spills() {
        let m = SpMonitor::default();
        assert_eq!(m.check(0), None);
        assert_eq!(m.check(u32::MAX), None);
    }

    #[test]
    fn armed_follows_either_monitor_bit() {
        assert!(!SpMonitor::default().armed());
        assert!(!monitor(false, false).armed());
        assert!(monitor(true, false).armed());
        assert!(monitor(false, true).armed());
        assert!(monitor(true, true).armed());
    }

    #[test]
    fn disabled_bounds_never_spill() {
        let m = monitor(false, false);
        assert_eq!(m.check(0), None);
        assert_eq!(m.check(u32::MAX), None);
    }

    #[test]
    fn min_bound_spills_only_strictly_below_when_enabled() {
        let m = monitor(true, false);
        assert_eq!(m.check(0x3FC8_0FFF), Some(SpSpill::Min));
        assert_eq!(m.check(0x3FC8_1000), None);
        assert_eq!(m.check(u32::MAX), None);
    }

    #[test]
    fn max_bound_spills_only_strictly_above_when_enabled() {
        let m = monitor(false, true);
        assert_eq!(m.check(0x3FC8_2001), Some(SpSpill::Max));
        assert_eq!(m.check(0x3FC8_2000), None);
        assert_eq!(m.check(0), None);
    }

    #[test]
    fn both_bounds_in_range_does_not_spill() {
        let m = monitor(true, true);
        assert_eq!(m.check(0x3FC8_1800), None);
        assert_eq!(m.check(0x3FC8_0000), Some(SpSpill::Min));
        assert_eq!(m.check(0x3FC8_3000), Some(SpSpill::Max));
    }

    #[test]
    fn min_is_checked_before_max_when_bounds_cross() {
        let m = SpMonitor {
            on_min: true,
            on_max: true,
            min: 0x2000,
            max: 0x1000,
        };
        assert_eq!(m.check(0x1800), Some(SpSpill::Min));
    }
}
