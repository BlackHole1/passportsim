//! EXTMEM, the cache controller (`specs/blocks/extmem.toml`).
//!
//! There is no cache: a flash window read goes to the flash store through the MMU. What remains
//! are five status bits the ROM and HAL spin on, each of which hangs boot if it never reads done
//! (the five `wait` rows of the block file). `SYNC_DONE`, `PRELOAD_DONE`, `AUTOLOAD_DONE` and
//! `CACHE_STATE` idle are constants that [`Extmem::settle`] restores after every access;
//! `FREEZE_DONE` follows `FREEZE_ENA`, because `Cache_Freeze_ICache_Enable` polls it for 1 and
//! `_Disable` for 0.
//!
//! An invalidate (`ICACHE_SYNC_CTRL.INVALIDATE_ENA`) and a change of `ICACHE_CTRL.ICACHE_ENABLE`
//! or `ICACHE_CTRL1.SHUT_*` return `Wiring::CacheCtrl`, which rebuilds the window pages. Turning a
//! closed gate into a cache error is not wired (`crate::cold::CacheGate`). Everything else is
//! storage at its reset value.

use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use crate::r#gen::regs_extmem::{BLOCK_SIZE, REG_COUNT, REGS, idx};
use crate::periph::store_only::{RegBank, TouchTag};
use crate::periph::{Block, Cx, Peripheral, RegRead, RegWrite, Wiring, block};

/// `EXTMEM_ICACHE_ENABLE`, bit 0 of `ICACHE_CTRL`: the cache gate both windows sit behind.
pub const ICACHE_ENABLE: u32 = 1 << 0;

/// `EXTMEM_ICACHE_SHUT_IBUS` and `EXTMEM_ICACHE_SHUT_DBUS`, bits 0 and 1 of `ICACHE_CTRL1`:
/// `Cache_Suspend_ICache` sets both and `Cache_Resume_ICache` clears them.
pub const ICACHE_SHUT: u32 = 0b11;

pub const INVALIDATE_ENA: u32 = 1 << 0;

/// `EXTMEM_ICACHE_SYNC_DONE`, bit 1 of `ICACHE_SYNC_CTRL`: reads 1.
pub const SYNC_DONE: u32 = 1 << 1;

/// `EXTMEM_ICACHE_PRELOAD_DONE`, bit 1 of `ICACHE_PRELOAD_CTRL`: reads 1.
pub const PRELOAD_DONE: u32 = 1 << 1;

/// `EXTMEM_ICACHE_AUTOLOAD_DONE`, bit 3 of `ICACHE_AUTOLOAD_CTRL`: reads 1.
pub const AUTOLOAD_DONE: u32 = 1 << 3;

pub const FREEZE_ENA: u32 = 1 << 0;

/// `EXTMEM_ICACHE_FREEZE_DONE`, bit 2 of `ICACHE_FREEZE`: reads the current `FREEZE_ENA`.
pub const FREEZE_DONE: u32 = 1 << 2;

pub const ICACHE_STATE: u32 = 0xFFF;

/// The value `EXTMEM_ICACHE_STATE` reads: 1, idle, which `Cache_Disable_ICache` polls for.
pub const ICACHE_STATE_IDLE: u32 = 1;

const TOUCH_BITS: usize = 64;

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Extmem {
    #[serde(with = "reg_values")]
    regs: RegStore<REG_COUNT>,
    /// Bit per register already reported to the fidelity ledger. Part of the snapshot, so a
    /// restored machine does not report a register twice.
    touched: [u64; REG_COUNT.div_ceil(TOUCH_BITS)],
    /// The offsets the table does not name (0x104 to 0x3F8): plain storage that reports once, as
    /// a `StoreOnly` block does.
    spare: RegBank,
    /// An `INVALIDATE_ENA` write since [`Extmem::take_invalidate`] last looked. Not snapshot
    /// state: the machine consumes it right after the write, before a snapshot can be taken.
    #[serde(skip)]
    invalidate: bool,
}

impl Default for Extmem {
    fn default() -> Self {
        let mut extmem = Extmem {
            regs: RegStore::new(&REGS),
            touched: [0; REG_COUNT.div_ceil(TOUCH_BITS)],
            spare: RegBank::new(BLOCK_SIZE),
            invalidate: false,
        };
        extmem.settle();
        extmem
    }
}

impl Extmem {
    pub const TAG: TouchTag = TouchTag {
        periph: <block::Extmem as Block>::ID,
        allowlisted: false,
    };

    #[inline]
    pub fn reg(&self, index: usize) -> u32 {
        self.regs.get(index)
    }

    /// Whether the cache gate is open: `ICACHE_ENABLE` set and neither `SHUT` bit set.
    #[inline]
    pub fn cache_open(&self) -> bool {
        self.reg(idx::EXTMEM_ICACHE_CTRL) & ICACHE_ENABLE != 0
            && self.reg(idx::EXTMEM_ICACHE_CTRL1) & ICACHE_SHUT == 0
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        if !Extmem::names_any(off, size) {
            // Wholly outside the table: one bank access at the guest's width, so the first touch
            // carries that width.
            return self.spare.read(off, size, now, ledger, Extmem::TAG);
        }
        let mut val = 0;
        for i in 0..size as u32 {
            let at = off.wrapping_add(i);
            let byte = match Extmem::index_of(at) {
                Some(index) => {
                    self.touch(index, size, now, ledger, TouchAccess::Read);
                    self.regs.read(index, (at % 4) as u8, Size::B1)
                }
                // A misaligned access straddling the table's end: its spare half goes a byte at a
                // time, since the run is 1 to 3 bytes and `Size` has no 3.
                None => self.spare.read(at, Size::B1, now, ledger, Extmem::TAG),
            };
            val |= byte << (i * 8);
        }
        val
    }

    /// Writes the low `size` bytes of `val` at `off` and returns the cross-block effect. The done
    /// bits are re-settled afterwards.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> Wiring {
        if !Extmem::names_any(off, size) {
            self.spare.write(off, size, val, now, ledger, Extmem::TAG);
            return Wiring::None;
        }
        let mut wiring = Wiring::None;
        for i in 0..size as u32 {
            let at = off.wrapping_add(i);
            let byte = (val >> (i * 8)) & 0xFF;
            let Some(index) = Extmem::index_of(at) else {
                self.spare
                    .write(at, Size::B1, byte, now, ledger, Extmem::TAG);
                continue;
            };
            self.touch(index, size, now, ledger, TouchAccess::Write);
            let lane = (at % 4) as u8;
            let delta = self.regs.write(index, lane, Size::B1, byte);
            if Extmem::drives_cache(index, lane, delta.before, delta.after) {
                wiring = Wiring::CacheCtrl;
                self.invalidate |= index == idx::EXTMEM_ICACHE_SYNC_CTRL;
            }
        }
        self.settle();
        wiring
    }

    /// Restores the reset values of the table and the done bits. Every reset scope restores
    /// EXTMEM (`reset_domains` in the block file); only the `CPU0_` fan-out, which reaches no
    /// peripheral, leaves it alone.
    pub fn reset_block(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.regs.reset(kind.scope);
        self.spare.reset();
        self.settle();
    }

    /// Holds the four constant done bits at their completed value and makes `FREEZE_DONE` follow
    /// `FREEZE_ENA`. The table resets `SYNC_DONE` to 0 (the CSV gives the register reset 0x1 from
    /// `INVALIDATE_ENA` alone), so this also sets it before the first read.
    fn settle(&mut self) {
        for (index, done) in [
            (idx::EXTMEM_ICACHE_SYNC_CTRL, SYNC_DONE),
            (idx::EXTMEM_ICACHE_PRELOAD_CTRL, PRELOAD_DONE),
            (idx::EXTMEM_ICACHE_AUTOLOAD_CTRL, AUTOLOAD_DONE),
        ] {
            self.regs.set(index, self.regs.get(index) | done);
        }
        let state = self.regs.get(idx::EXTMEM_CACHE_STATE);
        self.regs.set(
            idx::EXTMEM_CACHE_STATE,
            (state & !ICACHE_STATE) | ICACHE_STATE_IDLE,
        );
        let freeze = self.regs.get(idx::EXTMEM_ICACHE_FREEZE);
        let done = if freeze & FREEZE_ENA != 0 {
            FREEZE_DONE
        } else {
            0
        };
        self.regs
            .set(idx::EXTMEM_ICACHE_FREEZE, (freeze & !FREEZE_DONE) | done);
    }

    /// Whether writing byte `lane` of register `index` from `before` to `after` changes the window
    /// gating or asks for an invalidate. `INVALIDATE_ENA` is a start bit that resets to 1, so it
    /// fires on a write to its own byte, not on a change of the whole register.
    fn drives_cache(index: usize, lane: u8, before: u32, after: u32) -> bool {
        const fn lane_of(mask: u32) -> u8 {
            (mask.trailing_zeros() / 8) as u8
        }
        match index {
            idx::EXTMEM_ICACHE_SYNC_CTRL => {
                lane == lane_of(INVALIDATE_ENA) && after & INVALIDATE_ENA != 0
            }
            idx::EXTMEM_ICACHE_CTRL => (before ^ after) & ICACHE_ENABLE != 0,
            idx::EXTMEM_ICACHE_CTRL1 => (before ^ after) & ICACHE_SHUT != 0,
            _ => false,
        }
    }

    pub fn take_invalidate(&mut self) -> bool {
        std::mem::take(&mut self.invalidate)
    }

    /// Table index of the register holding block offset `off`. A binary search, since this runs
    /// per byte of every access; the table is in offset order (`the_table_is_in_offset_order`).
    #[inline]
    fn index_of(off: u32) -> Option<usize> {
        let reg = u16::try_from(off & !3).ok()?;
        REGS.binary_search_by_key(&reg, |spec| spec.off).ok()
    }

    #[inline]
    fn names_any(off: u32, size: Size) -> bool {
        (0..size as u32).any(|i| Extmem::index_of(off.wrapping_add(i)).is_some())
    }

    fn touch(
        &mut self,
        index: usize,
        size: Size,
        now: VTime,
        ledger: &mut FidelityLedger,
        access: TouchAccess,
    ) {
        let bit = 1u64 << (index % TOUCH_BITS);
        if self.touched[index / TOUCH_BITS] & bit != 0 {
            return;
        }
        self.touched[index / TOUCH_BITS] |= bit;
        ledger.first_touch(FirstTouch {
            periph: Extmem::TAG.periph,
            off: u32::from(REGS[index].off),
            access,
            size: size as u8,
            now,
            allowlisted: Extmem::TAG.allowlisted,
        });
    }
}

pub type Model = Extmem;

impl Peripheral for Extmem {
    const ID: PeriphId = <block::Extmem as Block>::ID;
    const BASE: u32 = <block::Extmem as Block>::BASE;
    const SIZE: u32 = <block::Extmem as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        let _ = cx;
        self.reset_block(kind);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    /// A cache-control write returns `Wiring::CacheCtrl`; `crate::SocBus` turns it into `OkStop`,
    /// because the page entries change after the access.
    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        RegWrite {
            stop: false,
            wiring: self.store(off, size, val, cx.now, cx.ledger),
        }
    }

    /// The class the rows of `specs/blocks/extmem.toml` give the register at `off`, `U` for the
    /// rest.
    fn fidelity(&self, off: u32) -> Fidelity {
        Extmem::index_of(off).map_or(Fidelity::U, |index| REGS[index].class)
    }
}

/// Snapshot form of the register values: `RegStore` holds a `&'static` table and is not itself
/// serializable, so the section carries the values and restore rebuilds the store.
mod reg_values {
    use super::{REG_COUNT, REGS};
    use pemu_core::regstore::RegStore;
    use pemu_core::serde::de::Deserializer;
    use pemu_core::serde::ser::Serializer;
    use pemu_core::serde::{Deserialize, Serialize};

    pub fn serialize<S: Serializer>(regs: &RegStore<REG_COUNT>, s: S) -> Result<S::Ok, S::Error> {
        let vals: Vec<u32> = (0..REG_COUNT).map(|i| regs.get(i)).collect();
        vals.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<RegStore<REG_COUNT>, D::Error> {
        let vals = Vec::<u32>::deserialize(d)?;
        let mut regs = RegStore::new(&REGS);
        for (i, val) in vals.iter().enumerate().take(REG_COUNT) {
            regs.set(i, *val);
        }
        Ok(regs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::regstore::FieldAccess;
    use pemu_core::reset::ResetCause;

    const T: VTime = VTime(3);

    fn read(extmem: &mut Extmem, off: u32) -> u32 {
        extmem.load(off, Size::B4, T, &mut FidelityLedger::default())
    }

    fn write(extmem: &mut Extmem, off: u32, val: u32) -> Wiring {
        extmem.store(off, Size::B4, val, T, &mut FidelityLedger::default())
    }

    /// Pins every bit constant to the generated table, so a moved field cannot leave a done bit
    /// pointing at the wrong bit.
    #[test]
    fn the_bit_constants_are_the_generated_fields() {
        let rows: [(&str, &str, u32, FieldAccess); 10] = [
            (
                "EXTMEM_ICACHE_CTRL",
                "EXTMEM_ICACHE_ENABLE",
                ICACHE_ENABLE,
                FieldAccess::Rw,
            ),
            (
                "EXTMEM_ICACHE_SYNC_CTRL",
                "EXTMEM_ICACHE_INVALIDATE_ENA",
                INVALIDATE_ENA,
                FieldAccess::Rw,
            ),
            (
                "EXTMEM_ICACHE_SYNC_CTRL",
                "EXTMEM_ICACHE_SYNC_DONE",
                SYNC_DONE,
                FieldAccess::Ro,
            ),
            (
                "EXTMEM_ICACHE_PRELOAD_CTRL",
                "EXTMEM_ICACHE_PRELOAD_DONE",
                PRELOAD_DONE,
                FieldAccess::Ro,
            ),
            (
                "EXTMEM_ICACHE_AUTOLOAD_CTRL",
                "EXTMEM_ICACHE_AUTOLOAD_DONE",
                AUTOLOAD_DONE,
                FieldAccess::Ro,
            ),
            (
                "EXTMEM_CACHE_STATE",
                "EXTMEM_ICACHE_STATE",
                ICACHE_STATE,
                FieldAccess::Ro,
            ),
            (
                "EXTMEM_ICACHE_FREEZE",
                "EXTMEM_ICACHE_FREEZE_ENA",
                FREEZE_ENA,
                FieldAccess::Rw,
            ),
            (
                "EXTMEM_ICACHE_FREEZE",
                "EXTMEM_ICACHE_FREEZE_DONE",
                FREEZE_DONE,
                FieldAccess::Ro,
            ),
            (
                "EXTMEM_ICACHE_CTRL1",
                "EXTMEM_ICACHE_SHUT_IBUS",
                ICACHE_SHUT & 1,
                FieldAccess::Rw,
            ),
            // Both halves are pinned: a moved DBUS bit would make `cache_open` ignore a shut DBUS
            // and `drives_cache` miss the `Cache_Suspend_ICache` write.
            (
                "EXTMEM_ICACHE_CTRL1",
                "EXTMEM_ICACHE_SHUT_DBUS",
                ICACHE_SHUT & 2,
                FieldAccess::Rw,
            ),
        ];
        for (reg, field, mask, access) in rows {
            let spec = REGS.iter().find(|s| s.name == reg).expect(reg);
            let f = spec.fields.iter().find(|f| f.name == field).expect(field);
            let bits = ((1u64 << f.width) - 1) << f.shift;
            assert_eq!(bits as u32, mask, "{reg}.{field}");
            assert_eq!(f.access, access, "{reg}.{field}");
        }
        assert_eq!(ICACHE_SHUT, 0b11);
    }

    #[test]
    fn the_five_polled_status_bits_read_complete() {
        let mut extmem = Extmem::default();
        // `Cache_Invalidate_ICache_Items`: SYNC_ADDR, SYNC_SIZE, INVALIDATE_ENA, then spin on
        // SYNC_DONE, which is set from the first read.
        assert_eq!(read(&mut extmem, 0x028) & SYNC_DONE, SYNC_DONE);
        write(&mut extmem, 0x02C, 0x4200_0000);
        write(&mut extmem, 0x030, 0x1_0000);
        write(&mut extmem, 0x028, INVALIDATE_ENA);
        assert_eq!(read(&mut extmem, 0x028) & SYNC_DONE, SYNC_DONE);
        assert_eq!(read(&mut extmem, 0x02C), 0x4200_0000);
        assert_eq!(read(&mut extmem, 0x030), 0x1_0000);

        assert_eq!(read(&mut extmem, 0x034) & PRELOAD_DONE, PRELOAD_DONE);
        assert_eq!(read(&mut extmem, 0x040) & AUTOLOAD_DONE, AUTOLOAD_DONE);
        assert_eq!(read(&mut extmem, 0x0B0) & ICACHE_STATE, ICACHE_STATE_IDLE);
        // Starting a preload or an autoload does not make them read busy.
        write(&mut extmem, 0x034, INVALIDATE_ENA);
        write(&mut extmem, 0x040, 1 << 2);
        assert_eq!(read(&mut extmem, 0x034) & PRELOAD_DONE, PRELOAD_DONE);
        assert_eq!(read(&mut extmem, 0x040) & AUTOLOAD_DONE, AUTOLOAD_DONE);
        write(&mut extmem, 0x028, 0);
        write(&mut extmem, 0x0B0, 0xFFF);
        assert_eq!(read(&mut extmem, 0x028) & SYNC_DONE, SYNC_DONE);
        assert_eq!(read(&mut extmem, 0x0B0) & ICACHE_STATE, ICACHE_STATE_IDLE);
    }

    #[test]
    fn freeze_done_follows_freeze_ena_in_both_directions() {
        let mut extmem = Extmem::default();
        // `Cache_Freeze_ICache_Enable` writes 1 to bit 0 and polls DONE == 1; `_Disable` clears it
        // and polls DONE == 0.
        assert_eq!(read(&mut extmem, 0x0CC) & FREEZE_DONE, 0);
        write(&mut extmem, 0x0CC, FREEZE_ENA | (1 << 1));
        assert_eq!(read(&mut extmem, 0x0CC) & FREEZE_DONE, FREEZE_DONE);
        assert_eq!(read(&mut extmem, 0x0CC) & FREEZE_ENA, FREEZE_ENA);
        write(&mut extmem, 0x0CC, 0);
        assert_eq!(read(&mut extmem, 0x0CC) & FREEZE_DONE, 0);
        // A byte write moves the bit too: it is settled after every access.
        extmem.store(
            0x0CC,
            Size::B1,
            FREEZE_ENA,
            T,
            &mut FidelityLedger::default(),
        );
        assert_eq!(read(&mut extmem, 0x0CC) & FREEZE_DONE, FREEZE_DONE);
    }

    #[test]
    fn only_the_three_cache_control_writes_produce_wiring() {
        let mut extmem = Extmem::default();
        assert!(matches!(
            write(&mut extmem, 0x028, INVALIDATE_ENA),
            Wiring::CacheCtrl
        ));
        assert!(matches!(write(&mut extmem, 0x028, 0), Wiring::None));
        assert!(matches!(
            write(&mut extmem, 0x000, ICACHE_ENABLE),
            Wiring::CacheCtrl
        ));
        assert!(matches!(
            write(&mut extmem, 0x000, ICACHE_ENABLE),
            Wiring::None
        ));
        // Both buses reset shut, so `Cache_Resume_ICache` opening them is a change.
        assert!(!extmem.cache_open());
        assert!(matches!(write(&mut extmem, 0x004, 0), Wiring::CacheCtrl));
        assert!(extmem.cache_open());
        assert!(matches!(
            write(&mut extmem, 0x004, ICACHE_SHUT),
            Wiring::CacheCtrl
        ));
        assert!(!extmem.cache_open());
        assert!(matches!(write(&mut extmem, 0x02C, 0x1234), Wiring::None));
        assert!(matches!(write(&mut extmem, 0x0E8, 0xFFFF), Wiring::None));
        assert!(matches!(write(&mut extmem, 0x3FC, 0), Wiring::None));
    }

    /// `INVALIDATE_ENA` resets to 1, so a write that cannot reach bit 0 must not invalidate:
    /// otherwise any byte write in the register costs an `OkStop` and a full window rebuild.
    #[test]
    fn only_a_write_to_the_invalidate_byte_asks_for_the_invalidate() {
        let mut extmem = Extmem::default();
        let mut ledger = FidelityLedger::default();
        assert_eq!(read(&mut extmem, 0x028) & INVALIDATE_ENA, INVALIDATE_ENA);
        for lane in 1..4 {
            assert!(matches!(
                extmem.store(0x028 + lane, Size::B1, 0x00, T, &mut ledger),
                Wiring::None
            ));
            assert!(matches!(
                extmem.store(0x028 + lane, Size::B1, 0xFF, T, &mut ledger),
                Wiring::None
            ));
        }
        assert!(matches!(
            extmem.store(0x028, Size::B1, INVALIDATE_ENA, T, &mut ledger),
            Wiring::CacheCtrl
        ));
        assert!(matches!(
            extmem.store(0x028, Size::B1, 0x00, T, &mut ledger),
            Wiring::None
        ));
        // A halfword or word write covering byte 0 still fires, which is how the ROM writes it.
        assert!(matches!(
            extmem.store(0x028, Size::B2, INVALIDATE_ENA, T, &mut ledger),
            Wiring::CacheCtrl
        ));
        assert!(matches!(
            write(&mut extmem, 0x028, INVALIDATE_ENA),
            Wiring::CacheCtrl
        ));
        assert!(matches!(
            extmem.store(0x02A, Size::B2, 0xFFFF, T, &mut ledger),
            Wiring::None
        ));
    }

    #[test]
    fn the_reset_values_of_the_table_stand_and_a_reset_restores_them() {
        let mut extmem = Extmem::default();
        // CTRL1 resets to 3 (both buses shut); the window constants and DATE read back.
        assert_eq!(read(&mut extmem, 0x004), 0x3);
        assert_eq!(read(&mut extmem, 0x054), 0x4200_0000);
        assert_eq!(read(&mut extmem, 0x058), 0x427F_FFFF);
        assert_eq!(read(&mut extmem, 0x05C), 0x3C00_0000);
        assert_eq!(read(&mut extmem, 0x060), 0x3C7F_FFFF);
        assert_eq!(read(&mut extmem, 0x3FC), 0x0200_7160);
        assert!(!extmem.cache_open(), "the cache starts disabled and shut");

        write(&mut extmem, 0x000, ICACHE_ENABLE);
        write(&mut extmem, 0x004, 0);
        write(&mut extmem, 0x0CC, FREEZE_ENA);
        assert!(extmem.cache_open());
        // A CPU reset keeps every digital register (`ResetFanout::CpuAndPms`).
        extmem.reset_block(ResetKind::of(ResetCause::RTC_SW_CPU).expect("a documented cause"));
        assert!(extmem.cache_open());
        extmem.reset_block(ResetKind::of(ResetCause::POWERON).expect("a documented cause"));
        assert!(!extmem.cache_open());
        assert_eq!(read(&mut extmem, 0x004), 0x3);
        assert_eq!(read(&mut extmem, 0x0CC) & FREEZE_DONE, 0);
        assert_eq!(read(&mut extmem, 0x028) & SYNC_DONE, SYNC_DONE);
    }

    #[test]
    fn the_window_the_table_does_not_name_is_storage_and_is_reported_once() {
        let mut extmem = Extmem::default();
        let mut ledger = FidelityLedger::default();
        // The table names 0x000 to 0x100 and 0x3FC; 0x104 to 0x3F8 is the hole.
        assert_eq!(Extmem::index_of(0x100), Some(idx::EXTMEM_CLOCK_GATE));
        assert_eq!(Extmem::index_of(0x104), None);
        assert_eq!(Extmem::index_of(0x3FC), Some(idx::EXTMEM_DATE));
        extmem.store(0x104, Size::B4, 0xABCD, T, &mut ledger);
        assert_eq!(extmem.load(0x104, Size::B4, T, &mut ledger), 0xABCD);
        assert_eq!(extmem.fidelity(0x104), Fidelity::U);
        extmem.load(0x028, Size::B4, VTime(1), &mut ledger);
        extmem.load(0x028, Size::B4, VTime(2), &mut ledger);
        extmem.store(0x0B0, Size::B4, 0, VTime(3), &mut ledger);
        let rows: Vec<_> = ledger
            .first_touches()
            .iter()
            .map(|t| (t.off, t.access))
            .collect();
        assert_eq!(
            rows,
            vec![
                (0x104, TouchAccess::Write),
                (0x028, TouchAccess::Read),
                (0x0B0, TouchAccess::Write),
            ]
        );
        for (off, class) in [
            (0x028, Fidelity::B),
            (0x034, Fidelity::B),
            (0x040, Fidelity::B),
            (0x0B0, Fidelity::B),
            (0x0CC, Fidelity::B),
            // Named in the block file only as a declared approximation.
            (0x008, Fidelity::C),
        ] {
            assert_eq!(extmem.fidelity(off), class, "{off:#05X}");
        }
    }

    /// Registers with modeled behavior report B, the eight the boot path touches without the model
    /// acting on them report C, and the rest, named in no row, report U.
    #[test]
    fn every_register_the_model_implements_is_class_b() {
        let extmem = Extmem::default();
        let modeled = [
            0x028, 0x034, 0x040, 0x0B0, 0x0CC, // the polled done bits
            0x000, 0x004, // ICACHE_CTRL and ICACHE_CTRL1, the cache and bus gates
            0x02C, 0x030, // the invalidate address and size
            0x054, 0x058, 0x05C, 0x060, // the four window constants
            0x3FC, // DATE
        ];
        for off in modeled {
            assert_eq!(extmem.fidelity(off), Fidelity::B, "{off:#05X}");
        }
        // Stored, with the effect deliberately not implemented.
        let approximated = [0x008, 0x078, 0x07C, 0x084, 0x088, 0x0A8, 0x0AC, 0x0C4];
        for off in approximated {
            assert_eq!(extmem.fidelity(off), Fidelity::C, "{off:#05X}");
        }
        for spec in REGS.iter() {
            let off = u32::from(spec.off);
            let want = if modeled.contains(&off) {
                Fidelity::B
            } else if approximated.contains(&off) {
                Fidelity::C
            } else {
                Fidelity::U
            };
            assert_eq!(extmem.fidelity(off), want, "{} at {off:#05X}", spec.name);
        }
        assert_eq!(extmem.fidelity(0x104), Fidelity::U);
    }

    #[test]
    fn the_table_is_in_offset_order() {
        assert!(REGS.windows(2).all(|w| w[0].off < w[1].off));
        for off in (0..BLOCK_SIZE).step_by(4) {
            let scan = u16::try_from(off)
                .ok()
                .and_then(|reg| REGS.iter().position(|spec| spec.off == reg));
            assert_eq!(Extmem::index_of(off), scan, "{off:#05X}");
            assert_eq!(Extmem::index_of(off + 3), scan, "{off:#05X} + 3");
        }
    }

    /// The `spare` bank reports an unnamed offset at the guest's width, as a `StoreOnly` block
    /// does, not at one byte.
    #[test]
    fn an_access_outside_the_table_is_reported_at_its_own_width() {
        for (off, size, width) in [
            (0x104, Size::B4, 4u8),
            (0x200, Size::B2, 2),
            (0x300, Size::B1, 1),
            (0x400, Size::B4, 4),
        ] {
            let mut extmem = Extmem::default();
            let mut ledger = FidelityLedger::default();
            extmem.load(off, size, T, &mut ledger);
            let rows: Vec<_> = ledger
                .first_touches()
                .iter()
                .map(|t| (t.off, t.size, t.access))
                .collect();
            assert_eq!(rows, vec![(off, width, TouchAccess::Read)], "{off:#05X}");
            let mut ledger = FidelityLedger::default();
            let mut extmem = Extmem::default();
            extmem.store(off, size, 0xAAAA_AAAA, T, &mut ledger);
            let rows: Vec<_> = ledger
                .first_touches()
                .iter()
                .map(|t| (t.off, t.size, t.access))
                .collect();
            assert_eq!(rows, vec![(off, width, TouchAccess::Write)], "{off:#05X}");
            assert_eq!(
                extmem.load(off, size, T, &mut ledger),
                0xAAAA_AAAA & ((1u64 << (u32::from(width) * 8)) - 1) as u32
            );
        }
        // A register the table names is still reported at its own offset.
        let mut extmem = Extmem::default();
        let mut ledger = FidelityLedger::default();
        extmem.load(0x3FC, Size::B4, T, &mut ledger);
        let rows: Vec<_> = ledger
            .first_touches()
            .iter()
            .map(|t| (t.off, t.size))
            .collect();
        assert_eq!(rows, vec![(0x3FC, 4)]);
    }

    #[test]
    fn narrow_accesses_reach_only_the_bytes_they_address() {
        let mut extmem = Extmem::default();
        let mut ledger = FidelityLedger::default();
        // Nothing widens a byte write into a read-modify-write of the register.
        extmem.store(0x02C, Size::B4, 0x1122_3344, T, &mut ledger);
        assert_eq!(extmem.load(0x02C, Size::B1, T, &mut ledger), 0x44);
        assert_eq!(extmem.load(0x02E, Size::B2, T, &mut ledger), 0x1122);
        extmem.store(0x02D, Size::B1, 0xAA, T, &mut ledger);
        assert_eq!(extmem.load(0x02C, Size::B4, T, &mut ledger), 0x1122_AA44);
        extmem.store(0x02E, Size::B4, 0xFFFF_FFFF, T, &mut ledger);
        assert_eq!(extmem.load(0x02C, Size::B4, T, &mut ledger), 0xFFFF_AA44);
        assert_eq!(extmem.load(0x030, Size::B4, T, &mut ledger), 0x0000_FFFF);
        // Reserved bits ignore writes: SYNC_SIZE is 23 bits wide.
        extmem.store(0x030, Size::B4, 0xFFFF_FFFF, T, &mut ledger);
        assert_eq!(extmem.load(0x030, Size::B4, T, &mut ledger), 0x7F_FFFF);
    }
}
