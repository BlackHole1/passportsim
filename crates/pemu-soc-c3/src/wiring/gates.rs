//! What the SYSTEM peripheral clock and reset enables do to the blocks they name, as the
//! `probe_campaign_regs` captures show it (`specs/blocks/system.toml` rows
//! `SYSTEM_PERIP_CLK_EN0/1` and `SYSTEM_PERIP_RST_EN0/1`).
//!
//! A block held in reset reads 0 and drops writes, clock on or off. With the reset released and
//! the clock off, every write is dropped whole and a read answers per [`OffRead`]. Raising a
//! reset bit resets the block ([`apply_raised`]); SYSTIMER's counters therefore restart at IDF's
//! pulse in `esp_timer_impl_early_init`, not at the chip reset, as the `probe_campaign_reset`
//! capture confirms. SARADC keeps its registers across `APB_SARADC_RST` on the device.

use pemu_core::sched::PeriphId;

use crate::periph::{Cx, DeviceVisitor, Devices, Peripheral, id, system};
use crate::wiring::reset;

pub const OFF_PERIP_RST_EN0: u32 = 0x018;

pub const OFF_PERIP_RST_EN1: u32 = 0x01C;

/// What a read of a block returns while its clock is off and its reset released.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum OffRead {
    /// The last value the block returned while clocked, latch slot `.0` (I2S0, AES, SPI2).
    /// UNVERIFIED: one latch per block, since each probe read one register.
    Latch(usize),
    /// The stored value (LEDC, I2C0). UNVERIFIED: a read side effect would happen.
    Stored,
    /// 0 (SHA).
    Zero,
}

/// One reset line of `PERIP_RST_EN0/1`; the same bit of `PERIP_CLK_EN0/1` clocks the block.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Line {
    pub bank: usize,
    pub bit: u32,
    /// The clock-off rule; `None` gates nothing on the clock bit (UNVERIFIED for the blocks no
    /// capture gated).
    pub off: Option<OffRead>,
}

/// The reset line of `block` (IDF `soc/esp32c3/register/soc/system_reg.h`, `SYSTEM_*_RST`).
///
/// Two modeled blocks with a reset bit have none: SARADC keeps its registers across the pulse,
/// and USJ's reset would drop the USB link, which this access-time path cannot reach
/// (UNVERIFIED).
pub const fn line_of(block: PeriphId) -> Option<Line> {
    use OffRead::{Latch, Stored, Zero};
    let (bank, bit, off) = match block {
        // SPI01: one bit for the cache host and the command host.
        id::SPI0 | id::SPI1 => (0, 1, None),
        id::UART0 => (0, 2, None),
        id::UART1 => (0, 5, None),
        id::SPI2 => (0, 6, Some(Latch(2))),
        id::I2C0 => (0, 7, Some(Stored)),
        id::UHCI0 => (0, 8, None),
        id::RMT => (0, 9, None),
        id::LEDC => (0, 11, Some(Stored)),
        id::TIMG0 => (0, 13, None),
        id::EFUSE => (0, 14, None),
        id::TIMG1 => (0, 15, None),
        id::TWAI => (0, 19, None),
        id::I2S0 => (0, 21, Some(Latch(0))),
        id::SYSTIMER => (0, 29, None),
        id::AES => (1, 1, Some(Latch(1))),
        id::SHA => (1, 2, Some(Zero)),
        id::RSA => (1, 3, None),
        id::DS => (1, 4, None),
        id::HMAC => (1, 5, None),
        id::GDMA => (1, 6, None),
        _ => return None,
    };
    Some(Line { bank, bit, off })
}

const GATED: [PeriphId; 21] = [
    id::UART0,
    id::SPI1,
    id::SPI0,
    id::EFUSE,
    id::UART1,
    id::I2C0,
    id::UHCI0,
    id::RMT,
    id::LEDC,
    id::TIMG0,
    id::TIMG1,
    id::SYSTIMER,
    id::SPI2,
    id::TWAI,
    id::I2S0,
    id::AES,
    id::SHA,
    id::RSA,
    id::DS,
    id::HMAC,
    id::GDMA,
];

/// What an access to a block finds at its gate.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Gate {
    /// The access reaches the block.
    Open,
    /// The access reaches the block, and a read's value goes into latch `slot`.
    Latching(usize),
    /// Held in reset: a read returns 0 and a write is dropped, clock on or off.
    Held,
    /// Clock off: a write is dropped and a read answers as the [`OffRead`] says.
    ClockOff(OffRead),
}

/// The gate an access to `block` meets under the enables `sys` holds. `mmio` asks it before the
/// access reaches the block, so a held block's read side effects (a FIFO pop) do not happen.
#[inline]
pub fn gate(sys: &system::Model, block: PeriphId) -> Gate {
    let Some(line) = line_of(block) else {
        return Gate::Open;
    };
    let bit = 1 << line.bit;
    if sys.perip_rst_en(line.bank) & bit != 0 {
        return Gate::Held;
    }
    match line.off {
        None => Gate::Open,
        Some(off) if sys.perip_clk_en(line.bank) & bit == 0 => Gate::ClockOff(off),
        Some(OffRead::Latch(slot)) => Gate::Latching(slot),
        Some(_) => Gate::Open,
    }
}

/// The two reset enables, `[PERIP_RST_EN0, PERIP_RST_EN1]`, before a SYSTEM write.
pub fn reset_enables(sys: &system::Model) -> [u32; 2] {
    [sys.perip_rst_en(0), sys.perip_rst_en(1)]
}

/// Resets every block whose reset bit the SYSTEM write just raised (`before` against the
/// enables now) and clears its latch. Returns how many blocks it reset.
pub fn apply_raised(devices: &mut Devices, before: [u32; 2], cx: &mut Cx<'_>) -> u32 {
    let now = reset_enables(&devices.system);
    let raised = [now[0] & !before[0], now[1] & !before[1]];
    if raised == [0, 0] {
        return 0;
    }
    let mut count = 0;
    for block in GATED {
        let Some(line) = line_of(block) else {
            continue;
        };
        if raised[line.bank] & (1 << line.bit) == 0 {
            continue;
        }
        let mut v = ResetOne { cx: &mut *cx };
        devices.visit(block, &mut v);
        if let Some(OffRead::Latch(slot)) = line.off {
            devices.system.set_latch(slot, 0);
        }
        count += 1;
    }
    count
}

/// One `Peripheral::reset` with [`reset::PERIPHERAL_RESET`].
struct ResetOne<'a, 'b> {
    cx: &'a mut Cx<'b>,
}

impl DeviceVisitor for ResetOne<'_, '_> {
    fn visit<P: Peripheral>(&mut self, dev: &mut P) {
        dev.reset(reset::PERIPHERAL_RESET, self.cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::periph::BLOCKS;

    #[test]
    fn the_gated_list_is_every_block_with_a_line() {
        let with_line: Vec<PeriphId> = BLOCKS
            .iter()
            .map(|b| b.id)
            .filter(|&b| line_of(b).is_some())
            .collect();
        assert_eq!(with_line, GATED.to_vec());
        assert!(line_of(id::SARADC).is_none());
        assert_eq!(
            line_of(id::SYSTIMER),
            Some(Line {
                bank: 0,
                bit: 29,
                off: None
            })
        );
        assert!(line_of(id::USJ).is_none());
        assert!(line_of(id::SYSTEM).is_none());
    }

    #[test]
    fn the_gate_follows_the_enables() {
        use pemu_core::fidelity::FidelityLedger;
        use pemu_core::regstore::Size;
        use pemu_core::time::VTime;
        let mut sys = system::Model::default();
        let mut l = FidelityLedger::default();
        let t = VTime(0);
        // Power-on: RST_EN0 0, RST_EN1 0x1FE (the crypto blocks and GDMA held).
        assert_eq!(gate(&sys, id::UART0), Gate::Open);
        assert_eq!(gate(&sys, id::AES), Gate::Held);
        assert_eq!(gate(&sys, id::GDMA), Gate::Held);
        assert_eq!(
            gate(&sys, id::I2S0),
            Gate::ClockOff(OffRead::Latch(0)),
            "EN0 bit 21 is off after reset"
        );
        assert_eq!(gate(&sys, id::SYSTIMER), Gate::Open);
        let en0 = sys.perip_clk_en(0);
        sys.store(0x010, Size::B4, en0 | 1 << 21, t, &mut l);
        assert_eq!(gate(&sys, id::I2S0), Gate::Latching(0));
        // Clock I2S0 off: its latch answers. SARADC's clock bit changes nothing.
        sys.store(0x010, Size::B4, en0 & !(1 << 21 | 1 << 28), t, &mut l);
        assert_eq!(gate(&sys, id::I2S0), Gate::ClockOff(OffRead::Latch(0)));
        assert_eq!(gate(&sys, id::SARADC), Gate::Open);
        // SPI2 latches, LEDC and I2C0 answer from their registers, UART0 ignores its clock.
        sys.store(0x010, Size::B4, 0, t, &mut l);
        assert_eq!(gate(&sys, id::SPI2), Gate::ClockOff(OffRead::Latch(2)));
        assert_eq!(gate(&sys, id::LEDC), Gate::ClockOff(OffRead::Stored));
        assert_eq!(gate(&sys, id::I2C0), Gate::ClockOff(OffRead::Stored));
        assert_eq!(gate(&sys, id::UART0), Gate::Open);
        sys.store(0x010, Size::B4, 1 << 6 | 1 << 7 | 1 << 11, t, &mut l);
        assert_eq!(gate(&sys, id::SPI2), Gate::Latching(2));
        assert_eq!(gate(&sys, id::LEDC), Gate::Open);
        assert_eq!(gate(&sys, id::I2C0), Gate::Open);
        // Hold it in reset: held wins.
        sys.store(OFF_PERIP_RST_EN0, Size::B4, 1 << 21 | 1 << 2, t, &mut l);
        assert_eq!(gate(&sys, id::I2S0), Gate::Held);
        assert_eq!(gate(&sys, id::UART0), Gate::Held);
        // Release AES and clock it.
        sys.store(OFF_PERIP_RST_EN1, Size::B4, 0x1F8, t, &mut l);
        assert_eq!(gate(&sys, id::AES), Gate::ClockOff(OffRead::Latch(1)));
        assert_eq!(gate(&sys, id::SHA), Gate::ClockOff(OffRead::Zero));
        sys.store(0x014, Size::B4, 1 << 1 | 1 << 2, t, &mut l);
        assert_eq!(gate(&sys, id::AES), Gate::Latching(1));
        assert_eq!(gate(&sys, id::SHA), Gate::Open);
    }
}
