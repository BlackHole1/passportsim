//! `Wiring::Spi2Transfer`: the one place the SPI2 master, its GDMA TX channel and the GPIO
//! output level meet, and the only place any of them reaches the board
//! (`specs/blocks/spi2.toml`).
//!
//! `dc` is sampled from `GPIO_OUT` bit 20 when `CMD.usr` sets, `cs_release` is
//! `!MISC.cs_keep_active`, and the bytes come from the TX descriptor walk. D/C is sampled, never
//! inferred from `GPIO_ENABLE`: the LCD post-transaction callback disables the output driver
//! after every transaction, so the enable says nothing about the level.

use pemu_board::traits::BoardPorts;
use pemu_core::time::VTime;

use crate::periph::gdma::{CHANNELS, DmaMem, Engine, PERI_SPI2};
use crate::periph::spi2::Master;

/// The D/C GPIO of the panel.
pub const DC_GPIO: u8 = 20;

/// Offset of `GPIO_OUT` in the `gpio` block window.
pub const GPIO_OUT_OFF: u32 = 0x04;

/// One transaction as the panel sees it (`BoardPorts::spi2`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Transfer {
    /// D/C line level: false is a command byte, true is parameters or pixels.
    pub dc: bool,
    /// The bytes clocked out, in descriptor order.
    pub bytes: Vec<u8>,
    /// Whether CS released afterwards: `!MISC.cs_keep_active`.
    pub cs_release: bool,
}

/// What one `Wiring::Spi2Transfer` did.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Applied {
    /// The transaction handed to the board, absent when it had no DMA write phase.
    pub transfer: Option<Transfer>,
    /// New level of a GDMA channel's interrupt source, when the descriptor walk changed it.
    pub gdma_irq: Option<(usize, bool)>,
}

/// The D/C level of a transaction, sampled from `GPIO_OUT`.
pub fn dc_level(gpio_out: u32) -> bool {
    gpio_out & (1 << DC_GPIO) != 0
}

/// Collects the bytes of the transaction `CMD.usr` started, without touching the board.
///
/// The TX channel is the one whose `OUT_PERI_SEL` is the SPI2 code, never a fixed pair. No write
/// phase, no `dma_tx_ena` or no length gives no transfer; an unconnected channel gives an empty
/// one, which is what the hardware would clock out.
pub fn collect(
    spi2: &mut Master,
    gdma: &mut Engine,
    gpio_out: u32,
    mem: &mut dyn DmaMem,
) -> Applied {
    let Some(request) = spi2.take_pending() else {
        return Applied::default();
    };
    if !request.feeds_the_panel() {
        return Applied::default();
    }
    let mut applied = Applied {
        transfer: Some(Transfer {
            dc: dc_level(gpio_out),
            bytes: Vec::new(),
            cs_release: !request.cs_keep_active,
        }),
        gdma_irq: None,
    };
    if let Some(ch) = gdma.tx_channel_of(PERI_SPI2) {
        debug_assert!(ch < CHANNELS);
        let pull = gdma.tx_pull(ch, request.bytes, mem);
        if let Some(transfer) = applied.transfer.as_mut() {
            transfer.bytes = pull.bytes;
        }
        applied.gdma_irq = pull.irq.map(|level| (ch, level));
    }
    applied
}

/// The whole `Wiring::Spi2Transfer` step: collect the bytes, then deliver them to the panel.
/// `gpio_out` is `GPIO_OUT` at the moment `CMD.usr` set.
pub fn apply(
    now: VTime,
    spi2: &mut Master,
    gdma: &mut Engine,
    gpio_out: u32,
    mem: &mut dyn DmaMem,
    board: &mut dyn BoardPorts,
) -> Applied {
    let applied = collect(spi2, gdma, gpio_out, mem);
    if let Some(transfer) = applied.transfer.as_ref() {
        board.spi2(now, transfer.dc, &transfer.bytes, transfer.cs_release);
    }
    applied
}

#[cfg(test)]
mod tests {
    use pemu_board::power::RailState;
    use pemu_board::traits::PcmFormat;
    use pemu_board::usb_plug::UsbHostState;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;

    use crate::periph::gdma::layout;
    use crate::periph::spi2::{CMD_USR, DMA_CONF_TX_ENA, MISC_CS_KEEP_ACTIVE, USER_USR_MOSI};

    use super::*;

    const T: VTime = VTime(5_000);
    const DRAM: u32 = 0x3FC8_0000;

    /// Guest memory behind [`DmaMem`], based at [`DRAM`].
    struct Ram(Vec<u8>);

    impl Ram {
        fn desc(&mut self, at: u32, length: u32, suc_eof: bool, buffer: u32, next: u32) {
            let w0 = length | (length << 12) | (u32::from(suc_eof) << 30) | (1 << 31);
            for (i, word) in [w0, buffer, next].into_iter().enumerate() {
                self.write(at + 4 * i as u32, &word.to_le_bytes());
            }
        }
    }

    impl DmaMem for Ram {
        fn read(&mut self, addr: u32, out: &mut [u8]) {
            for (i, b) in out.iter_mut().enumerate() {
                let at = (addr.wrapping_add(i as u32).wrapping_sub(DRAM)) as usize;
                *b = self.0.get(at).copied().unwrap_or(0);
            }
        }

        fn write(&mut self, addr: u32, data: &[u8]) {
            for (i, b) in data.iter().enumerate() {
                let at = (addr.wrapping_add(i as u32).wrapping_sub(DRAM)) as usize;
                if let Some(slot) = self.0.get_mut(at) {
                    *slot = *b;
                }
            }
        }
    }

    /// A board that records the panel transport; the other ports answer zero, low or off.
    #[derive(Default)]
    struct Panel(Vec<(VTime, bool, Vec<u8>, bool)>);

    impl BoardPorts for Panel {
        fn spi2(&mut self, t: VTime, dc: bool, data: &[u8], cs_release: bool) {
            self.0.push((t, dc, data.to_vec(), cs_release));
        }

        fn i2c_start(&mut self, _t: VTime, _addr: u8, _read: bool) -> bool {
            false
        }

        fn i2c_write(&mut self, _t: VTime, _byte: u8) -> bool {
            false
        }

        fn i2c_read(&mut self, _t: VTime) -> u8 {
            0
        }

        fn i2c_stop(&mut self, _t: VTime) {}

        fn i2s_dac(&mut self, _t: VTime, _fmt: PcmFormat, _frames: &[i16]) {}

        fn i2s_adc(&mut self, _t: VTime, _fmt: PcmFormat, _out: &mut [i16]) {}

        fn adc_mv(&self, _t: VTime, _unit: u8, _channel: u8) -> u32 {
            0
        }

        fn gpio_out(&mut self, _t: VTime, _pin: u8, _level: bool, _oe: bool) {}

        fn gpio_in(&self, _t: VTime, _pin: u8) -> bool {
            false
        }

        fn ledc(&mut self, _t: VTime, _channel: u8, _duty: u32, _duty_res: u8, _freq_hz: u32) {}

        fn rail(&self) -> RailState {
            RailState::Off
        }

        fn usb(&self) -> UsbHostState {
            UsbHostState::U0
        }
    }

    const USER: u32 = 0x10;
    const MS_DLEN: u32 = 0x1C;
    const MISC: u32 = 0x20;
    const DMA_CONF: u32 = 0x30;

    /// The SPI2 half of `spi_hal_setup_trans` plus `spi_hal_user_start`.
    fn transact(m: &mut Master, l: &mut FidelityLedger, bytes: u32, cs_keep_active: bool) {
        m.store(MS_DLEN, Size::B4, bytes * 8 - 1, T, l);
        m.store(USER, Size::B4, USER_USR_MOSI, T, l);
        m.store(DMA_CONF, Size::B4, DMA_CONF_TX_ENA, T, l);
        let misc = if cs_keep_active {
            MISC_CS_KEEP_ACTIVE
        } else {
            0
        };
        m.store(MISC, Size::B4, misc, T, l);
        m.store(0x00, Size::B4, CMD_USR, T, l);
    }

    /// `gdma_connect` then `gdma_start` for the outbound chain at `at`.
    fn start_tx(g: &mut Engine, l: &mut FidelityLedger, ch: usize, at: u32) {
        let lay = layout(ch);
        let reg = |i: usize| u32::from(crate::r#gen::regs_gdma::REGS[i].off);
        g.store(reg(lay.out_peri_sel), Size::B4, PERI_SPI2, T, l);
        g.store(
            reg(lay.out_link),
            Size::B4,
            (at & 0xF_FFFF) | (1 << 21),
            T,
            l,
        );
    }

    fn parts() -> (Master, Engine, FidelityLedger, Ram, Panel) {
        (
            Master::default(),
            Engine::default(),
            FidelityLedger::default(),
            Ram(vec![0; 0x4000]),
            Panel::default(),
        )
    }

    #[test]
    fn dc_is_sampled_from_gpio_out_bit_20() {
        assert_eq!(DC_GPIO, 20);
        assert_eq!(GPIO_OUT_OFF, 0x04);
        assert!(!dc_level(0));
        assert!(dc_level(1 << 20));
        assert!(!dc_level(!(1 << 20)), "no other pin is the D/C line");
        assert!(dc_level(0xFFFF_FFFF));
    }

    /// One 240 by 20 LVGL band: the RAMWR byte with CS kept, then 9600 pixel bytes from three
    /// descriptors with CS released on the last chunk.
    #[test]
    fn one_flush_band_reaches_the_panel_as_a_command_then_its_pixels() {
        let (mut spi2, mut gdma, mut l, mut ram, mut board) = parts();

        // The command byte: one descriptor, D/C low, CS kept for the pixels that follow.
        ram.desc(DRAM, 1, true, DRAM + 0x1000, 0);
        ram.write(DRAM + 0x1000, &[0x2C]);
        start_tx(&mut gdma, &mut l, 0, DRAM);
        transact(&mut spi2, &mut l, 1, true);
        let applied = apply(T, &mut spi2, &mut gdma, 0, &mut ram, &mut board);
        assert_eq!(
            applied.transfer,
            Some(Transfer {
                dc: false,
                bytes: vec![0x2C],
                cs_release: false,
            })
        );

        // The pixels: 4092 + 4092 + 1416 bytes, D/C high, CS released at the end.
        let at = [DRAM + 0x20, DRAM + 0x2C, DRAM + 0x38];
        let len = [4092u32, 4092, 1416];
        for i in 0..3 {
            let last = i == 2;
            ram.desc(
                at[i],
                len[i],
                last,
                DRAM + 0x1000 + 0x1000 * i as u32,
                if last { 0 } else { at[i + 1] },
            );
            let fill: Vec<u8> = (0..len[i])
                .map(|b| (i as u8 + 1).wrapping_add(b as u8))
                .collect();
            ram.write(DRAM + 0x1000 + 0x1000 * i as u32, &fill);
        }
        start_tx(&mut gdma, &mut l, 0, at[0]);
        transact(&mut spi2, &mut l, 9600, false);
        let applied = apply(T, &mut spi2, &mut gdma, 1 << 20, &mut ram, &mut board);
        let pixels = applied.transfer.expect("the pixels reached the panel");
        assert!(pixels.dc, "D/C is high for pixel bytes");
        assert!(pixels.cs_release, "the last chunk releases CS");
        assert_eq!(pixels.bytes.len(), 9600);
        assert_eq!(&pixels.bytes[..2], &[1, 2]);
        assert_eq!(&pixels.bytes[4092..4094], &[2, 3]);
        assert_eq!(&pixels.bytes[8184..8186], &[3, 4]);

        let calls: Vec<_> = board
            .0
            .iter()
            .map(|(t, dc, bytes, release)| (*t, *dc, bytes.len(), *release))
            .collect();
        assert_eq!(calls, vec![(T, false, 1, false), (T, true, 9600, true)]);
    }

    #[test]
    fn a_transaction_without_a_dma_write_phase_never_reaches_the_panel() {
        let (mut spi2, mut gdma, mut l, mut ram, mut board) = parts();
        spi2.store(MS_DLEN, Size::B4, 8 * 8 - 1, T, &mut l);
        spi2.store(0x00, Size::B4, CMD_USR, T, &mut l);
        let applied = apply(T, &mut spi2, &mut gdma, 0, &mut ram, &mut board);
        assert_eq!(applied, Applied::default());
        assert!(board.0.is_empty());

        // And a wiring step with nothing pending is a no-op.
        assert_eq!(
            apply(T, &mut spi2, &mut gdma, 0, &mut ram, &mut board),
            Applied::default()
        );
    }

    #[test]
    fn the_channel_is_found_by_peri_sel_and_its_interrupt_change_is_reported() {
        let (mut spi2, mut gdma, mut l, mut ram, mut board) = parts();
        let lay = layout(2);
        let reg = |i: usize| u32::from(crate::r#gen::regs_gdma::REGS[i].off);
        gdma.store(
            reg(lay.int_ena),
            Size::B4,
            crate::periph::gdma::int::OUT_EOF,
            T,
            &mut l,
        );
        ram.desc(DRAM, 4, true, DRAM + 0x1000, 0);
        ram.write(DRAM + 0x1000, &[0xAA, 0xBB, 0xCC, 0xDD]);
        start_tx(&mut gdma, &mut l, 2, DRAM);
        transact(&mut spi2, &mut l, 4, false);

        let applied = apply(T, &mut spi2, &mut gdma, 1 << 20, &mut ram, &mut board);
        assert_eq!(
            applied.transfer.map(|t| t.bytes),
            Some(vec![0xAA, 0xBB, 0xCC, 0xDD])
        );
        assert_eq!(applied.gdma_irq, Some((2, true)), "source 46 rose");
        assert!(gdma.irq_level(2));
    }

    #[test]
    fn an_unconnected_channel_clocks_out_no_bytes() {
        let (mut spi2, mut gdma, mut l, mut ram, mut board) = parts();
        transact(&mut spi2, &mut l, 4, true);
        let applied = apply(T, &mut spi2, &mut gdma, 0, &mut ram, &mut board);
        assert_eq!(
            applied.transfer,
            Some(Transfer {
                dc: false,
                bytes: Vec::new(),
                cs_release: false,
            })
        );
        assert_eq!(board.0.len(), 1);
    }
}
