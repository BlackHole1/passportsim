//! MMIO dispatch: [`crate::periph::MMIO_MAP`] holds one `PeriphId` per 4 KB page of the peripheral
//! window (IDF `soc/esp32c3/include/soc/reg_base.h`), up to WORLD_CNTL at 0x600D0000, then a
//! `match` on the id reaches the model. [`crate::periph::lookup`] resolves the one page two blocks
//! share (RTC_CNTL 0x60008000 and eFuse 0x60008800).
//!
//! Every block of the table has a model; one with no behavior is a `StoreOnly` alias that stores,
//! reads back and records first touches, so a bring-up run reaches the ledger rather than a panic.
//! An address no block claims reads 0, drops the write and is recorded once under
//! [`crate::periph::id::UNMAPPED`].
//!
//! Before an access reaches a block it passes the SYSTEM clock and reset gates
//! ([`crate::wiring::gates`]).

use pemu_core::regstore::Size;
use pemu_core::sched::PeriphId;
use pemu_core::time::VTime;

use crate::mem::MMIO_BASE;
use crate::periph::{
    Cx, DeviceVisitor, Devices, MMIO_MAP, MMIO_PAGES, Peripheral, RegRead, RegWrite, Wiring, id,
    lookup,
};
use crate::wiring::gates::{self, Gate, OffRead};
use pemu_core::fidelity::{FidelityLedger, FirstTouch, TouchAccess};

/// Page index of `addr` in [`MMIO_MAP`], or `None` outside the map.
#[inline]
pub fn page_index(addr: u32) -> Option<usize> {
    let page = (addr.checked_sub(MMIO_BASE)? >> 12) as usize;
    (page < MMIO_PAGES).then_some(page)
}

/// The block [`MMIO_MAP`] holds for `addr`, before the split slot is resolved.
#[inline]
pub fn block_at(addr: u32) -> PeriphId {
    page_index(addr).map_or(id::UNMAPPED, |page| MMIO_MAP[page])
}

/// Access width as a [`Size`]; `None` for a width no register access uses.
#[inline]
pub const fn reg_size(bytes: u8) -> Option<Size> {
    match bytes {
        1 => Some(Size::B1),
        2 => Some(Size::B2),
        4 => Some(Size::B4),
        _ => None,
    }
}

/// Reads `size` bytes at `addr` from the block that claims it.
pub fn read(devices: &mut Devices, addr: u32, size: Size, cx: &mut Cx) -> RegRead {
    match lookup(addr) {
        Some((periph, off)) => {
            let latch = match gates::gate(&devices.system, periph) {
                Gate::Open => None,
                Gate::Latching(slot) => Some(slot),
                Gate::Held => {
                    return RegRead {
                        val: 0,
                        stop: false,
                    };
                }
                Gate::ClockOff(OffRead::Latch(slot)) => {
                    return RegRead {
                        val: devices.system.latch(slot),
                        stop: false,
                    };
                }
                Gate::ClockOff(OffRead::Zero) => {
                    return RegRead {
                        val: 0,
                        stop: false,
                    };
                }
                Gate::ClockOff(OffRead::Stored) => None,
            };
            let mut visitor = Read {
                off,
                size,
                cx,
                out: RegRead {
                    val: 0,
                    stop: false,
                },
            };
            devices.visit(periph, &mut visitor);
            let out = visitor.out;
            if let Some(slot) = latch {
                devices.system.set_latch(slot, out.val);
            }
            out
        }
        None => {
            record_unmapped(cx.ledger, cx.now, addr, size as u8, TouchAccess::Read);
            RegRead {
                val: 0,
                stop: false,
            }
        }
    }
}

/// Writes the low `size` bytes of `val` at `addr` into the block that claims it. Always inlined
/// into `SocBus::store_slow`, its one caller, on the path of every peripheral store.
#[inline(always)]
pub fn write(devices: &mut Devices, addr: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
    match lookup(addr) {
        Some((periph, off)) => {
            if matches!(
                gates::gate(&devices.system, periph),
                Gate::Held | Gate::ClockOff(_)
            ) {
                return RegWrite {
                    stop: false,
                    wiring: Wiring::None,
                };
            }
            let enables = (periph == id::SYSTEM).then(|| gates::reset_enables(&devices.system));
            let mut visitor = Write {
                off,
                size,
                val,
                cx,
                out: RegWrite {
                    stop: false,
                    wiring: Wiring::None,
                },
            };
            devices.visit(periph, &mut visitor);
            let mut out = visitor.out;
            if let Some(before) = enables
                && gates::apply_raised(devices, before, cx) > 0
            {
                // Reset blocks re-drove their interrupt sources and dropped their scheduled work;
                // leave the block so the next instruction sees both.
                out.stop = true;
            }
            out
        }
        None => {
            record_unmapped(cx.ledger, cx.now, addr, size as u8, TouchAccess::Write);
            RegWrite {
                stop: false,
                wiring: Wiring::None,
            }
        }
    }
}

/// Records an access no block claims, once per 4-byte address. The subject is
/// [`id::UNMAPPED`] with the offset inside the window, a space `crate::UNBACKED` stays out of. It
/// takes the ledger, not a [`Cx`], because the width rejection of `crate::SocBus` has none; a
/// rejected width is recorded as it came, since that is the anomaly.
pub(crate) fn record_unmapped(
    ledger: &mut FidelityLedger,
    now: VTime,
    addr: u32,
    size: u8,
    access: TouchAccess,
) {
    ledger.first_touch(FirstTouch {
        periph: id::UNMAPPED,
        off: addr.wrapping_sub(MMIO_BASE) & !3,
        access,
        size,
        now,
        allowlisted: false,
    });
}

/// The `match` on `PeriphId` of the read path, as `Devices::visit` spells it.
struct Read<'c, 'a> {
    off: u32,
    size: Size,
    cx: &'c mut Cx<'a>,
    out: RegRead,
}

impl DeviceVisitor for Read<'_, '_> {
    fn visit<P: Peripheral>(&mut self, dev: &mut P) {
        self.out = dev.read(self.off, self.size, self.cx);
    }
}

/// The `match` on `PeriphId` of the write path.
struct Write<'c, 'a> {
    off: u32,
    size: Size,
    val: u32,
    cx: &'c mut Cx<'a>,
    out: RegWrite,
}

impl DeviceVisitor for Write<'_, '_> {
    fn visit<P: Peripheral>(&mut self, dev: &mut P) {
        self.out = dev.write(self.off, self.size, self.val, self.cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::MMIO_LEN;

    #[test]
    fn the_index_formula_holds_at_both_ends_of_the_table() {
        assert_eq!(MMIO_MAP.len(), 0xD1);
        assert_eq!(page_index(MMIO_BASE), Some(0));
        assert_eq!(page_index(MMIO_BASE + 0xFFF), Some(0));
        assert_eq!(page_index(MMIO_BASE + 0x1000), Some(1));
        assert_eq!(page_index(0x600D_0000), Some(0xD0));
        assert_eq!(page_index(0x600D_0FFF), Some(0xD0));
        // One page past the last entry, and below the window: outside the table.
        assert_eq!(page_index(0x600D_1000), None);
        assert_eq!(page_index(MMIO_BASE - 1), None);
        assert_eq!(page_index(0), None);
        assert_eq!(page_index(u32::MAX), None);
        assert_eq!(block_at(MMIO_BASE), id::UART0);
        assert_eq!(block_at(0x600D_0000), id::WORLD_CNTL);
        assert_eq!(block_at(0x600D_1000), id::UNMAPPED);
        // The window is 1 MB but the map stops at the last block; the pages above are holes.
        assert!(MMIO_PAGES < (MMIO_LEN >> 12) as usize);
    }

    /// Counts the models a dispatch reaches, without touching them.
    #[derive(Default)]
    struct Count(usize);

    impl DeviceVisitor for Count {
        fn visit<P: Peripheral>(&mut self, _dev: &mut P) {
            self.0 += 1;
        }
    }

    #[test]
    fn every_page_of_the_map_reaches_a_model_rather_than_a_panic() {
        // Every block without a behavioral model reaches a StoreOnly alias, so the dispatch
        // resolves every page of the map.
        let mut devices = Devices::default();
        let mut seen = Count::default();
        let mut pages = 0;
        for (page, periph) in MMIO_MAP.iter().enumerate() {
            if *periph == id::UNMAPPED {
                continue;
            }
            pages += 1;
            assert!(
                devices.visit(*periph, &mut seen),
                "page {page:#X} has no model",
            );
        }
        assert_eq!(seen.0, pages);
        // Every address a block claims resolves to that block with an offset inside its window.
        for addr in [
            MMIO_BASE,
            0x6000_87FC,
            0x6000_8800,
            0x6001_CCD4,
            0x600C_5000,
            0x600C_51FC,
            0x600D_0000,
        ] {
            let (periph, off) = lookup(addr).unwrap_or_else(|| panic!("{addr:#X}"));
            assert!(devices.visit(periph, &mut seen), "{addr:#X}");
            assert!(off < 0x1000, "{addr:#X}");
        }
        // A hole inside the window resolves to nothing, and the read path answers 0 for it.
        assert_eq!(lookup(0x6000_1000), None);
        assert_eq!(lookup(0x600D_1000), None);
    }

    #[test]
    fn reg_size_covers_every_access_width() {
        assert!(matches!(reg_size(1), Some(Size::B1)));
        assert!(matches!(reg_size(2), Some(Size::B2)));
        assert!(matches!(reg_size(4), Some(Size::B4)));
        assert!(reg_size(3).is_none());
        assert!(reg_size(0).is_none());
        assert!(reg_size(8).is_none());
    }
}
