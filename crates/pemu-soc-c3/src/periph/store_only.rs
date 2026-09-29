//! The unmodeled default: reads return stored values, writes are stored, and the first touch per
//! register goes into the ledger.

use std::marker::PhantomData;

use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::regstore::Size;
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::{Block, Cx, Peripheral, RegRead, RegWrite, Wiring};

/// Identity attached to the first touches a `RegBank` reports.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TouchTag {
    /// Block whose registers the bank holds.
    pub periph: PeriphId,
    /// Radio pages stay allowlisted as U.
    pub allowlisted: bool,
}

/// Byte-addressed store of a block's 32-bit registers with first-touch tracking. A narrow access
/// touches only its bytes of the little-endian slot; bytes outside the window read 0, ignore
/// writes and are not reported.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct RegBank {
    /// One value per register.
    vals: Vec<u32>,
    /// Bit per register: already reported. Snapshotted, so a restored machine does not report a
    /// register twice.
    touched: Vec<u64>,
}

impl RegBank {
    /// A bank for a window of `size` bytes, all zero.
    pub fn new(size: u32) -> Self {
        let regs = size.div_ceil(4) as usize;
        RegBank {
            vals: vec![0; regs],
            touched: vec![0; regs.div_ceil(64)],
        }
    }

    /// Zeroes every register (UNVERIFIED: every reset kind, no per-register reset values). First
    /// touches are kept, so each register is reported once per machine.
    pub fn reset(&mut self) {
        self.vals.fill(0);
    }

    /// Reads `size` bytes at `off`, reporting the first touch of each register.
    pub fn read(
        &mut self,
        off: u32,
        size: Size,
        now: VTime,
        ledger: &mut FidelityLedger,
        tag: TouchTag,
    ) -> u32 {
        let mut val = 0;
        for i in 0..size as u32 {
            let Some((idx, shift)) = self.slot(off, i) else {
                continue;
            };
            self.touch(idx, TouchAccess::Read, size, now, ledger, tag);
            val |= ((self.vals[idx] >> shift) & 0xFF) << (i * 8);
        }
        val
    }

    /// Writes the low `size` bytes of `val` at `off`, reporting the first touch of each register.
    pub fn write(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
        tag: TouchTag,
    ) {
        for i in 0..size as u32 {
            let Some((idx, shift)) = self.slot(off, i) else {
                continue;
            };
            self.touch(idx, TouchAccess::Write, size, now, ledger, tag);
            let byte = (val >> (i * 8)) & 0xFF;
            self.vals[idx] = (self.vals[idx] & !(0xFF << shift)) | (byte << shift);
        }
    }

    /// The stored value of the register at `off` (aligned down to 4), without an access and
    /// without a report; 0 outside the window.
    pub fn get(&self, off: u32) -> u32 {
        self.vals.get((off / 4) as usize).copied().unwrap_or(0)
    }

    /// Sets the register at `off` (aligned down to 4) without an access or a report; an offset
    /// outside the window is ignored.
    pub fn set(&mut self, off: u32, val: u32) {
        if let Some(slot) = self.vals.get_mut((off / 4) as usize) {
            *slot = val;
        }
    }

    /// Whether the register at `off` (aligned down to 4) was already reported.
    pub fn is_touched(&self, off: u32) -> bool {
        let idx = (off / 4) as usize;
        self.touched
            .get(idx / 64)
            .is_some_and(|w| w & (1 << (idx % 64)) != 0)
    }

    /// Register index and bit shift of byte `i` of an access at `off`, if inside the window.
    fn slot(&self, off: u32, i: u32) -> Option<(usize, u32)> {
        let addr = off.checked_add(i)?;
        let idx = (addr / 4) as usize;
        (idx < self.vals.len()).then_some((idx, (addr % 4) * 8))
    }

    fn touch(
        &mut self,
        idx: usize,
        access: TouchAccess,
        size: Size,
        now: VTime,
        ledger: &mut FidelityLedger,
        tag: TouchTag,
    ) {
        let bit = 1u64 << (idx % 64);
        let Some(word) = self.touched.get_mut(idx / 64) else {
            return;
        };
        if *word & bit != 0 {
            return;
        }
        *word |= bit;
        ledger.first_touch(FirstTouch {
            periph: tag.periph,
            off: (idx * 4) as u32,
            access,
            size: size as u8,
            now,
            allowlisted: tag.allowlisted,
        });
    }
}

/// Store-only model of block `B`: class U, never stops, no wiring. `ALLOWLISTED` marks its first
/// touches as allowlisted (the radio pages); it is not part of the snapshot.
#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde", bound = "")]
pub struct StoreOnly<B, const ALLOWLISTED: bool = false> {
    bank: RegBank,
    #[serde(skip)]
    block: PhantomData<fn() -> B>,
}

impl<B: Block, const ALLOWLISTED: bool> Default for StoreOnly<B, ALLOWLISTED> {
    fn default() -> Self {
        StoreOnly {
            bank: RegBank::new(B::SIZE),
            block: PhantomData,
        }
    }
}

impl<B: Block, const ALLOWLISTED: bool> StoreOnly<B, ALLOWLISTED> {
    pub const TAG: TouchTag = TouchTag {
        periph: B::ID,
        allowlisted: ALLOWLISTED,
    };

    /// Stored value of `size` bytes at `off`; reports first touches.
    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.bank.read(off, size, now, ledger, Self::TAG)
    }

    /// Stores `size` bytes of `val` at `off`; reports first touches.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        self.bank.write(off, size, val, now, ledger, Self::TAG)
    }

    /// The register bank.
    pub fn bank(&self) -> &RegBank {
        &self.bank
    }
}

impl<B: Block, const ALLOWLISTED: bool> Peripheral for StoreOnly<B, ALLOWLISTED> {
    const ID: PeriphId = B::ID;
    const BASE: u32 = B::BASE;
    const SIZE: u32 = B::SIZE;

    fn reset(&mut self, _kind: ResetKind, _cx: &mut Cx) {
        self.bank.reset();
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        self.store(off, size, val, cx.now, cx.ledger);
        RegWrite {
            stop: false,
            wiring: Wiring::None,
        }
    }

    fn fidelity(&self, _off: u32) -> Fidelity {
        Fidelity::U
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestBlock;

    impl Block for TestBlock {
        const NAME: &'static str = "test";
        const ID: PeriphId = PeriphId(9);
        const BASE: u32 = 0x6000_0000;
        const SIZE: u32 = 0x10;
    }

    type Model = StoreOnly<TestBlock>;

    const T: VTime = VTime(42);

    #[test]
    fn identity_comes_from_the_block() {
        assert_eq!(<Model as Peripheral>::ID, PeriphId(9));
        assert_eq!(<Model as Peripheral>::BASE, 0x6000_0000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x10);
        assert_eq!(
            Model::TAG,
            TouchTag {
                periph: PeriphId(9),
                allowlisted: false
            }
        );
        let m = Model::default();
        assert_eq!(m.fidelity(0), Fidelity::U);
        assert_eq!(m.bank(), &RegBank::new(0x10));
    }

    #[test]
    fn read_after_write_and_untouched_reads_zero() {
        let mut m = Model::default();
        let mut l = FidelityLedger::default();
        assert_eq!(m.load(0, Size::B4, T, &mut l), 0);
        m.store(4, Size::B4, 0xDEAD_BEEF, T, &mut l);
        assert_eq!(m.load(4, Size::B4, T, &mut l), 0xDEAD_BEEF);
        m.store(4, Size::B4, 0x0123_4567, T, &mut l);
        assert_eq!(m.load(4, Size::B4, T, &mut l), 0x0123_4567);
        assert_eq!(m.load(8, Size::B4, T, &mut l), 0);
    }

    #[test]
    fn narrow_widths_touch_only_addressed_bytes() {
        let mut m = Model::default();
        let mut l = FidelityLedger::default();
        m.store(0, Size::B4, 0x1122_3344, T, &mut l);
        assert_eq!(m.load(0, Size::B1, T, &mut l), 0x44);
        assert_eq!(m.load(1, Size::B1, T, &mut l), 0x33);
        assert_eq!(m.load(3, Size::B1, T, &mut l), 0x11);
        assert_eq!(m.load(1, Size::B2, T, &mut l), 0x2233);
        assert_eq!(m.load(2, Size::B2, T, &mut l), 0x1122);
        m.store(1, Size::B1, 0xAA, T, &mut l);
        assert_eq!(m.load(0, Size::B4, T, &mut l), 0x1122_AA44);
        m.store(2, Size::B2, 0xBEEF, T, &mut l);
        assert_eq!(m.load(0, Size::B4, T, &mut l), 0xBEEF_AA44);
        m.store(0, Size::B1, 0x1_FF, T, &mut l);
        assert_eq!(
            m.load(0, Size::B4, T, &mut l),
            0xBEEF_AAFF,
            "only the low byte of a 1-byte write is stored"
        );
    }

    #[test]
    fn access_crossing_a_register_boundary_splits_bytes() {
        let mut m = Model::default();
        let mut l = FidelityLedger::default();
        m.store(2, Size::B4, 0xCAFE_F00D, T, &mut l);
        assert_eq!(m.load(0, Size::B4, T, &mut l), 0xF00D_0000);
        assert_eq!(m.load(4, Size::B4, T, &mut l), 0x0000_CAFE);
        assert_eq!(m.load(2, Size::B4, T, &mut l), 0xCAFE_F00D);
        let offs: Vec<_> = l.first_touches().iter().map(|t| t.off).collect();
        assert_eq!(offs, vec![0, 4]);
    }

    #[test]
    fn first_touch_is_reported_once_per_offset() {
        let mut m = Model::default();
        let mut l = FidelityLedger::default();
        assert!(!m.bank().is_touched(0));
        m.load(0, Size::B4, VTime(1), &mut l);
        m.store(0, Size::B4, 5, VTime(2), &mut l);
        m.load(1, Size::B1, VTime(3), &mut l);
        m.store(6, Size::B2, 7, VTime(4), &mut l);
        m.load(4, Size::B2, VTime(5), &mut l);
        m.store(0xC, Size::B1, 1, VTime(6), &mut l);
        assert!(m.bank().is_touched(3));
        assert!(m.bank().is_touched(4));
        assert!(!m.bank().is_touched(8));
        assert_eq!(
            l.first_touches(),
            &[
                FirstTouch {
                    periph: PeriphId(9),
                    off: 0,
                    access: TouchAccess::Read,
                    size: 4,
                    now: VTime(1),
                    allowlisted: false,
                },
                FirstTouch {
                    periph: PeriphId(9),
                    off: 4,
                    access: TouchAccess::Write,
                    size: 2,
                    now: VTime(4),
                    allowlisted: false,
                },
                FirstTouch {
                    periph: PeriphId(9),
                    off: 0xC,
                    access: TouchAccess::Write,
                    size: 1,
                    now: VTime(6),
                    allowlisted: false,
                },
            ]
        );
    }

    #[test]
    fn bytes_outside_the_window_read_zero_and_are_not_reported() {
        let mut m = Model::default();
        let mut l = FidelityLedger::default();
        assert_eq!(m.load(0x10, Size::B4, T, &mut l), 0);
        assert_eq!(m.load(u32::MAX - 1, Size::B4, T, &mut l), 0);
        m.store(0x14, Size::B4, 0xFFFF_FFFF, T, &mut l);
        assert!(l.first_touches().is_empty());
        m.store(0xE, Size::B4, 0xFFFF_FFFF, T, &mut l);
        assert_eq!(m.load(0xC, Size::B4, T, &mut l), 0xFFFF_0000);
        assert_eq!(l.first_touches().len(), 1);
    }

    #[test]
    fn reset_restores_values_and_keeps_touches() {
        let mut m = Model::default();
        let mut l = FidelityLedger::default();
        m.store(8, Size::B4, 0x55, T, &mut l);
        let mut bank = m.bank().clone();
        bank.reset();
        assert_eq!(bank.read(8, Size::B4, T, &mut l, Model::TAG), 0);
        assert!(bank.is_touched(8));
        assert_eq!(l.first_touches().len(), 1);
    }
}
