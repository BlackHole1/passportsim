//! The four DMA joins the machine applies: `Wiring::Spi2Transfer` (GDMA TX to the panel),
//! `Wiring::I2sPeriod` (one audio period between the GDMA ring, the codec and `HostIo`),
//! `Wiring::ShaDma` and `Wiring::AesDma`. The DMA adapters are SoC wiring; the machine counts
//! refused accesses in [`Machine::dma_faults`] and drives the GDMA interrupt levels.
//!
//! A pixel transaction that releases CS completes a frame for the host (a new generation and an
//! `EventKind::Frame`). The panel has no notion of a frame, and the LVGL flush releases CS on the
//! last chunk of every band, so one flushed band is one generation (UNVERIFIED).

use pemu_core::hostio::{EventKind, HostEvent};
use pemu_core::time::VTime;
use pemu_soc_c3::dma::{ArenaDma, DmaView};
use pemu_soc_c3::periph::gdma;
use pemu_soc_c3::periph::i2s0::Dir;
use pemu_soc_c3::wiring::i2s::GdmaI2s;
use pemu_soc_c3::wiring::{
    aes as aes_wiring, i2s as i2s_wiring, sha as sha_wiring, spi2 as spi2_wiring,
};

use crate::machine::Machine;

impl Machine {
    pub(crate) fn apply_spi2_transfer(&mut self, now: VTime) {
        let gpio_out = self.soc.devices.gpio.out_levels();
        let soc = &mut self.soc;
        let mut mem = ArenaDma::new(
            DmaView::new(&soc.pages, &mut soc.arena),
            &mut self.dma_faults,
        );
        let applied = spi2_wiring::apply(
            now,
            &mut soc.devices.spi2,
            &mut soc.devices.gdma,
            gpio_out,
            &mut mem,
            &mut self.board,
        );
        if let Some((ch, level)) = applied.gdma_irq {
            self.irq.set_source(gdma::source(ch), level);
        }
        let rows = self.publish_frame();
        if let (Some(_), Some(transfer)) = (rows, applied.transfer.as_ref())
            && transfer.dc
            && transfer.cs_release
        {
            let generation = self.io.frame.present();
            self.io.events.emit(HostEvent {
                kind: EventKind::Frame,
                vt: now,
                arg: generation,
            });
        }
    }

    /// The SHA model schedules its own completion; the TX walk may move the GDMA channel's level,
    /// which is driven here.
    pub(crate) fn apply_sha_dma(&mut self, now: VTime) {
        let soc = &mut self.soc;
        let mut mem = ArenaDma::new(
            DmaView::new(&soc.pages, &mut soc.arena),
            &mut self.dma_faults,
        );
        let applied = sha_wiring::run(
            &mut soc.devices.sha,
            &mut soc.devices.gdma,
            &mut mem,
            now,
            &mut self.sched,
        );
        if let Some((ch, level)) = applied.gdma_irq {
            self.irq.set_source(gdma::source(ch), level);
        }
    }

    /// The AES model schedules its own completion; either walk may move its GDMA channel's level,
    /// which is driven here.
    pub(crate) fn apply_aes_dma(&mut self, now: VTime) {
        let soc = &mut self.soc;
        let mut mem = ArenaDma::new(
            DmaView::new(&soc.pages, &mut soc.arena),
            &mut self.dma_faults,
        );
        let applied = aes_wiring::run(
            &mut soc.devices.aes,
            &mut soc.devices.gdma,
            &mut mem,
            now,
            &mut self.sched,
        );
        for (ch, level) in [applied.tx_irq, applied.rx_irq].into_iter().flatten() {
            self.irq.set_source(gdma::source(ch), level);
        }
    }

    /// Copies the panel's dirty rows and flags to `HostIo::frame`; returns the rows copied. Every
    /// host-visible change goes through here. The frame generation is not guest state, so a restore
    /// repaints without presenting a generation.
    pub(crate) fn publish_frame(&mut self) -> Option<(u16, u16)> {
        self.board.lcd.publish(&mut self.io.frame)
    }

    pub(crate) fn apply_i2s_period(&mut self, dir: Dir, now: VTime) {
        let mic = self.mic_path();
        let soc = &mut self.soc;
        let mut mem = ArenaDma::new(
            DmaView::new(&soc.pages, &mut soc.arena),
            &mut self.dma_faults,
        );
        let mut dma = GdmaI2s {
            gdma: &mut soc.devices.gdma,
            mem: &mut mem,
            irq: [None; gdma::CHANNELS],
        };
        let serviced = i2s_wiring::service_with_mic(
            &mut soc.devices.i2s0,
            dir,
            now,
            &mut self.sched,
            &mut dma,
            &mut self.board,
            &mut self.io,
            mic,
        );
        for (ch, level) in dma.irq.iter().enumerate() {
            if let Some(level) = level {
                self.irq.set_source(gdma::source(ch), *level);
            }
        }
        if serviced.width_refused {
            self.pcm_width_faults += 1;
        }
    }

    /// Open while the ES8311 ADC is powered and unmuted; microphone on slot 0 only unless
    /// `ADCDAT_SEL` is 0 (5 = ADC+DACR, whose DAC right loopback is not modelled; class C,
    /// UNVERIFIED). The analog PGA, `ADC_SCALE` and ADC volume gain are not applied to injected
    /// samples yet.
    pub(crate) fn mic_path(&self) -> i2s_wiring::MicPath {
        let adc = self.board.codec.adc_state();
        i2s_wiring::MicPath {
            open: adc.active,
            first_slot_only: adc.adcdat_sel != 0,
        }
    }

    /// I2S periods refused for a sample width other than 16 bits.
    pub fn pcm_width_faults(&self) -> u64 {
        self.pcm_width_faults
    }

    /// DMA accesses that named a byte no mapped page backs.
    pub fn dma_faults(&self) -> u64 {
        self.dma_faults
    }
}

#[cfg(test)]
mod adapter_tests {
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;
    use pemu_soc_c3::Soc;
    use pemu_soc_c3::periph::gdma::{DmaMem, Engine, PERI_I2S0, int, layout};
    use pemu_soc_c3::periph::i2s0::I2sDma;

    use super::*;

    const T: VTime = VTime(1);
    const DRAM: u32 = 0x3FC8_0000;

    fn reg(i: usize) -> u32 {
        u32::from(pemu_soc_c3::r#gen::regs_gdma::REGS[i].off)
    }

    fn desc(soc: &mut Soc, at: u32, size: u32, length: u32, buffer: u32, next: u32) {
        let w0 = size | (length << 12) | (1 << 30) | (1 << 31);
        for (i, w) in [w0, buffer, next].into_iter().enumerate() {
            assert!(matches!(
                soc.store_mem(at + 4 * i as u32, 4, w),
                pemu_soc_c3::Stored::Wrote { .. }
            ));
        }
    }

    #[test]
    fn the_i2s_adapter_moves_one_descriptor_per_period_both_ways() {
        let mut soc = Soc::default();
        let mut gdma = Engine::default();
        let mut l = FidelityLedger::default();
        desc(&mut soc, DRAM, 8, 8, DRAM + 0x100, DRAM + 0x0C);
        desc(&mut soc, DRAM + 0x0C, 8, 8, DRAM + 0x200, DRAM);
        desc(&mut soc, DRAM + 0x20, 6, 0, DRAM + 0x300, 0);
        for i in 0..8u32 {
            soc.store_mem(DRAM + 0x100 + i, 1, i + 1);
            soc.store_mem(DRAM + 0x200 + i, 1, 0x80 + i);
        }
        let lay = layout(1);
        gdma.store(reg(lay.out_peri_sel), Size::B4, PERI_I2S0, T, &mut l);
        gdma.store(reg(lay.in_peri_sel), Size::B4, PERI_I2S0, T, &mut l);
        gdma.store(
            reg(lay.int_ena),
            Size::B4,
            int::OUT_EOF | int::IN_SUC_EOF,
            T,
            &mut l,
        );
        gdma.store(
            reg(lay.out_link),
            Size::B4,
            (DRAM & 0xF_FFFF) | (1 << 21),
            T,
            &mut l,
        );
        gdma.store(
            reg(lay.in_link),
            Size::B4,
            ((DRAM + 0x20) & 0xF_FFFF) | (1 << 22),
            T,
            &mut l,
        );

        let mut faults = 0;
        let mut mem = ArenaDma::new(DmaView::new(&soc.pages, &mut soc.arena), &mut faults);
        let mut dma = GdmaI2s {
            gdma: &mut gdma,
            mem: &mut mem,
            irq: [None; gdma::CHANNELS],
        };
        assert_eq!(dma.period_bytes(Dir::Tx), Some(8));
        let mut out = Vec::new();
        dma.take_tx(&mut out);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(dma.irq[1], Some(true), "OUT_EOF raised the pair's source");
        assert_eq!(dma.period_bytes(Dir::Tx), Some(8));
        out.clear();
        dma.take_tx(&mut out);
        assert_eq!(out[0], 0x80);

        assert_eq!(dma.period_bytes(Dir::Rx), Some(6));
        dma.put_rx(&[9, 8, 7, 6, 5, 4]);
        assert_eq!(faults, 0);
        assert_eq!(soc.load_mem(DRAM + 0x300, 4), Some(0x0607_0809));
        // Word 0 now carries length 6 and `suc_eof`, owner cleared.
        assert_eq!(
            soc.load_mem(DRAM + 0x20, 4),
            Some(6 | (6 << 12) | (1 << 30))
        );
    }

    #[test]
    fn a_descriptor_outside_mapped_memory_counts_a_fault_and_reads_zeros() {
        let mut soc = Soc::default();
        let mut faults = 0;
        let mut mem = ArenaDma::new(DmaView::new(&soc.pages, &mut soc.arena), &mut faults);
        let mut buf = [0xAA; 4];
        // Inside the DMA window and backed by nothing.
        mem.read(0x3FC0_0000, &mut buf);
        assert_eq!(buf, [0; 4]);
        mem.write(0x3FC0_0000, &[1]);
        assert_eq!(faults, 2);
    }
}

#[cfg(all(test, feature = "bundled-rom"))]
mod machine_tests {
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_rv32::bus::{Bus, HartView};

    use crate::config::{Assets, MachineConfig};

    use super::*;

    const DRAM: u32 = 0x3FC8_0000;
    const SPI2: u32 = 0x6002_4000;
    const GDMA: u32 = 0x6003_F000;
    const GPIO_OUT: u32 = 0x6000_4004;

    /// A machine with every peripheral clocked and out of reset, as drivers leave the blocks they
    /// use (IDF `periph_module_enable`): at power-on GDMA, SHA and AES are held in reset, and a
    /// held block ignores the guest.
    fn machine() -> Machine {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let mut m = Machine::new(MachineConfig::default(), assets).expect("composes");
        enable_peripherals(&mut m);
        m
    }

    fn enable_peripherals(m: &mut Machine) {
        const SYSTEM: u32 = 0x600C_0000;
        for (off, val) in [(0x10, u32::MAX), (0x14, u32::MAX), (0x18, 0), (0x1C, 0)] {
            mmio(m, SYSTEM + off, val);
        }
    }

    fn mmio(m: &mut Machine, addr: u32, val: u32) {
        m.with_bus(|bus, hart| {
            let view = HartView {
                insns: hart.insns,
                extra: hart.extra,
                pc: hart.pc,
            };
            let _ = bus.store_slow(addr, 4, val, &view);
        });
        m.apply_pending_wiring();
    }

    fn poke(m: &mut Machine, addr: u32, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            m.soc.store_mem(addr + i as u32, 1, u32::from(*b));
        }
    }

    fn transact(m: &mut Machine, at: u32, buffer: u32, data: &[u8], dc: bool, release: bool) {
        let n = data.len() as u32;
        let w0 = n | (n << 12) | (1 << 30) | (1 << 31);
        for (i, w) in [w0, buffer, 0].into_iter().enumerate() {
            m.soc.store_mem(at + 4 * i as u32, 4, w);
        }
        poke(m, buffer, data);
        mmio(m, GPIO_OUT, if dc { 1 << 20 } else { 0 });
        let lay = gdma::layout(0);
        let off = |i: usize| u32::from(pemu_soc_c3::r#gen::regs_gdma::REGS[i].off);
        mmio(m, GDMA + off(lay.out_peri_sel), gdma::PERI_SPI2);
        mmio(m, GDMA + off(lay.out_link), (at & 0xF_FFFF) | (1 << 21));
        mmio(m, SPI2 + 0x1C, n * 8 - 1);
        mmio(m, SPI2 + 0x10, pemu_soc_c3::periph::spi2::USER_USR_MOSI);
        mmio(m, SPI2 + 0x30, pemu_soc_c3::periph::spi2::DMA_CONF_TX_ENA);
        let misc = if release {
            0
        } else {
            pemu_soc_c3::periph::spi2::MISC_CS_KEEP_ACTIVE
        };
        mmio(m, SPI2 + 0x20, misc);
        mmio(m, SPI2, pemu_soc_c3::periph::spi2::CMD_USR);
    }

    #[test]
    fn an_i2s_stream_at_an_undecoded_width_counts_a_fault() {
        use pemu_core::regstore::Size;
        let mut m = machine();
        let now = m.now();
        // TX at 16 kHz and a 24-bit slot (`bits_mod` 23), started.
        let conf1 = 15 | (7 << 7) | (23 << 13) | (15 << 18) | (15 << 24) | (1 << 29);
        for (off, val) in [
            (0x034, (2 << 27) | (1 << 26) | 39),
            (0x03C, (15 << 18) | 1),
            (0x02C, conf1),
            (0x054, 0b11 | (1 << 16)),
            (0x024, (1 << 19) | (1 << 15) | (1 << 2)),
        ] {
            m.soc
                .devices
                .i2s0
                .store(off, Size::B4, val, now, &mut m.ledger);
        }
        assert_eq!(m.soc.devices.i2s0.format(Dir::Tx).bits, 24);
        m.apply_i2s_period(Dir::Tx, now);
        assert_eq!(m.pcm_width_faults(), 1);
        assert_eq!(
            m.receipt().fault_counters,
            Some(crate::machine::FaultCounters {
                dma_faults: 0,
                pcm_width_faults: 1
            }),
            "the receipt reports it"
        );
        assert_eq!(
            m.io.audio_out.head(),
            0,
            "no 16-bit bytes were read as 24-bit PCM"
        );
        let snap = m.snapshot(pemu_core::snap::SnapOpts::default());
        let mut fresh = machine();
        fresh.restore(&snap).expect("restores");
        assert_eq!(fresh.pcm_width_faults(), 1);
    }

    #[test]
    fn a_restored_static_screen_is_on_the_frame_port() {
        let mut m = machine();
        transact(&mut m, DRAM, DRAM + 0x100, &[0x2C], false, false);
        transact(
            &mut m,
            DRAM + 0x20,
            DRAM + 0x200,
            &[0xF8, 0x00, 0x07, 0xE0],
            true,
            true,
        );
        let snap = m.snapshot(pemu_core::snap::SnapOpts::default());
        let mut fresh = machine();
        assert_eq!(&fresh.io.frame.pixels()[..2], &[0, 0]);
        fresh.restore(&snap).expect("restores");
        assert_eq!(fresh.io.frame.pixels(), m.io.frame.pixels());
        assert_eq!(fresh.io.frame.powered(), m.io.frame.powered());
    }

    #[test]
    fn a_rail_edge_reaches_the_frame_port() {
        let mut m = machine();
        assert!(m.io.frame.powered());
        m.apply_board_effect(pemu_board::passport::BoardEffect {
            rail: Some(pemu_board::power::PowerEdge::Off),
            ..Default::default()
        });
        assert!(!m.io.frame.powered(), "power_down publishes the dark panel");
    }

    #[test]
    fn an_ledc_only_change_updates_the_frame_backlight() {
        const LEDC: u32 = 0x6001_9000;
        use pemu_soc_c3::r#gen::regs_ledc::{REGS, idx};
        let off = |i: usize| LEDC + u32::from(REGS[i].off);
        let mut m = machine();
        assert_eq!(m.io.frame.backlight(), 0);
        mmio(
            &mut m,
            off(idx::LEDC_LSTIMER0_CONF),
            10 | (0x100 << 4) | (1 << 25),
        );
        mmio(&mut m, off(idx::LEDC_LSCH0_DUTY), 512 << 4);
        mmio(&mut m, off(idx::LEDC_LSCH0_CONF0), (1 << 2) | (1 << 4));
        assert_ne!(
            m.io.frame.backlight(),
            0,
            "the LEDC apply publishes the backlight"
        );
        // The LEDC hands the board the integer duty `DUTY_R >> 4`, so the board must not shift
        // it again (that would read duty 512 as 32 of 1024).
        assert_eq!(m.board().backlight.brightness().percent(), 50);
        assert_eq!(m.io.frame.backlight(), 512 << 4);
    }

    #[test]
    fn an_spi2_transfer_reaches_the_panel_and_the_frame_port() {
        let mut m = machine();
        let before = m.unapplied_wiring_by_kind();
        transact(&mut m, DRAM, DRAM + 0x100, &[0x2C], false, false);
        transact(
            &mut m,
            DRAM + 0x20,
            DRAM + 0x200,
            &[0xF8, 0x00, 0x07, 0xE0],
            true,
            true,
        );
        let lcd = &m.board().lcd;
        assert_eq!(lcd.ramwr_count(), 1);
        assert_eq!(lcd.pixels_written(), 2);
        assert_eq!(m.dma_faults(), 0);
        assert_eq!(
            m.unapplied_wiring_by_kind(),
            before,
            "nothing was counted as dropped"
        );
        let raw = m.board().lcd.raw()[..2].to_vec();
        assert_ne!(raw, [0, 0]);
        assert_eq!(&m.io.frame.pixels()[..2], &raw[..]);
        assert_eq!(
            m.io.frame.generation(),
            1,
            "a released pixel run is one frame"
        );
        let events: Vec<HostEvent> = m.io.events.slices(0).iter().copied().collect();
        assert!(
            events
                .iter()
                .any(|e| e.kind == EventKind::Frame && e.arg == 1)
        );
        assert!(
            m.io.frame.powered(),
            "the panel is on the board rail, which is up when the machine starts"
        );
    }

    fn rx(m: &mut Machine, slots: u8, start: bool) {
        use pemu_core::regstore::Size;
        let now = m.now();
        let conf1 = 15 | (7 << 7) | (15 << 13) | (15 << 18) | (15 << 24) | (1 << 29);
        let mono = if slots == 1 { 1 << 5 } else { 0 };
        let conf = (1 << 19) | (1 << 15) | mono | if start { 1 << 2 } else { 0 };
        for (off, val) in [
            (0x030, (2 << 27) | (1 << 26) | 39),
            (0x038, (15 << 18) | 1),
            (0x028, conf1),
            (0x050, 0b11 | (1 << 16)),
            (0x020, conf),
        ] {
            m.soc
                .devices
                .i2s0
                .store(off, Size::B4, val, now, &mut m.ledger);
        }
        assert_eq!(m.soc.devices.i2s0.running(Dir::Rx), start);
        m.apply_i2s_period(Dir::Rx, now);
    }

    fn codec_capture(m: &mut Machine, muted: bool) {
        let codec = &mut m.board.codec;
        codec.write_reg(0x0E, 0x00);
        codec.write_reg(0x14, 0x1A);
        codec.write_reg(0x0A, if muted { 0x40 } else { 0x00 });
        assert_eq!(m.mic_path().open, !muted);
    }

    fn mic(m: &mut Machine, seq: u64, samples: Vec<i16>) {
        use pemu_core::input::InputEvent;
        m.input(
            crate::machine::At::Now,
            InputEvent::MicChunk { seq, samples },
        )
        .expect("now is not in the past");
        m.run(crate::run::RunLimits::insns(1_000));
    }

    /// As on silicon, mic audio arriving while the path is closed is lost, and so is what was
    /// buffered when it closed; nothing is delivered late and every lost sample is counted.
    #[test]
    fn mic_audio_is_dropped_while_rx_or_the_codec_capture_path_is_not_running() {
        let mut m = machine();
        codec_capture(&mut m, false);
        mic(&mut m, 0, vec![1; 480]);
        let ring = &m.io.audio_in;
        assert_eq!((ring.head(), ring.len(), ring.dropped()), (0, 0, 480));

        rx(&mut m, 1, true);
        mic(&mut m, 1, vec![2; 480]);
        assert_eq!((m.io.audio_in.len(), m.io.audio_in.dropped()), (480, 480));

        codec_capture(&mut m, true);
        mic(&mut m, 2, vec![3; 240]);
        assert_eq!((m.io.audio_in.len(), m.io.audio_in.dropped()), (0, 1_200));

        // Stopping RX drops the buffered chunk rather than keeping it for the next start.
        codec_capture(&mut m, false);
        mic(&mut m, 3, vec![4; 240]);
        assert_eq!(m.io.audio_in.len(), 240);
        rx(&mut m, 1, false);
        assert_eq!((m.io.audio_in.len(), m.io.audio_in.dropped()), (0, 1_440));
        rx(&mut m, 1, true);
        assert_eq!(
            m.io.audio_in.len(),
            0,
            "nothing stale comes back on restart"
        );
        assert_eq!(m.io.audio_in.underflows(), 0);
        assert_eq!(m.unapplied_inputs(), 0);

        m.board.codec.write_reg(0x44, 0x10);
        assert!(m.mic_path().first_slot_only);
    }

    #[test]
    fn a_full_stereo_mic_ring_drops_the_oldest_whole_frames() {
        let mut m = machine();
        codec_capture(&mut m, false);
        rx(&mut m, 2, true);
        assert_eq!(m.soc.devices.i2s0.format(Dir::Rx).slots, 2);
        let capacity = m.io.audio_in.capacity();
        let frames = |from: i16, n: usize| -> Vec<i16> {
            (0..n as i16)
                .flat_map(|k| [from + k, -(from + k)])
                .collect()
        };
        let first = capacity / 2 - 1;
        mic(&mut m, 0, frames(0, first));
        mic(&mut m, 1, frames(20_000, 3));
        let ring = &m.io.audio_in;
        assert_eq!(ring.len() % 2, 0);
        assert_eq!(ring.dropped(), 4);
        let kept: Vec<i16> = ring.slices(ring.tail()).iter().copied().collect();
        assert_eq!(&kept[..2], &[2, -2], "the oldest kept frame is whole");
        assert_eq!(&kept[kept.len() - 6..], &frames(20_000, 3)[..]);
        mic(&mut m, 2, vec![5, -5, 6]);
        let ring = &m.io.audio_in;
        assert_eq!(ring.len() % 2, 0);
        assert_eq!(ring.dropped(), 4 + 2 + 1);
        assert_eq!(m.unapplied_inputs(), 0);
    }

    /// A one-descriptor GDMA RX chain on I2S0 at [`DRAM`], started, with `RXEOF_NUM` set to the
    /// buffer length, so the next RX period fills that buffer.
    fn rx_descriptor(m: &mut Machine, buffer: u32, bytes: u32) {
        use pemu_core::regstore::Size;
        let w0 = bytes | (1 << 31);
        for (i, w) in [w0, buffer, 0].into_iter().enumerate() {
            m.soc.store_mem(DRAM + 4 * i as u32, 4, w);
        }
        let lay = gdma::layout(0);
        let off = |i: usize| u32::from(pemu_soc_c3::r#gen::regs_gdma::REGS[i].off);
        mmio(m, GDMA + off(lay.in_peri_sel), gdma::PERI_I2S0);
        mmio(m, GDMA + off(lay.in_link), (DRAM & 0xF_FFFF) | (1 << 22));
        let now = m.now();
        m.soc
            .devices
            .i2s0
            .store(0x064, Size::B4, bytes, now, &mut m.ledger);
    }

    #[test]
    fn a_snapshot_mid_mic_ring_restores_and_continues_under_both_executors() {
        use crate::executor::Executor;
        const BUFFER: u32 = DRAM + 0x1000;
        let mut m = machine();
        codec_capture(&mut m, false);
        rx_descriptor(&mut m, BUFFER, 480);
        rx(&mut m, 1, true);
        mic(&mut m, 0, (0..500).collect());
        mic(&mut m, 1, (0..300).map(|k| -k).collect());
        let mut out = [0i16; 160];
        m.io.audio_in.pop_or_silence(&mut out);
        m.io.audio_in.count_dropped(7);
        let snap = m.snapshot(pemu_core::snap::SnapOpts::default());
        let continue_with = |m: &mut Machine| -> Vec<i16> {
            // One RX period of 240 mono frames at 16 kHz is 15 ms.
            let until = VTime(m.now().0 + VTime::from_ms(20).0);
            m.run(crate::run::RunLimits {
                until: Some(until),
                max_insns: None,
                stops: Default::default(),
            });
            let read: Vec<i16> = (0..240)
                .map(|i| m.soc.load_mem(BUFFER + 2 * i, 2).unwrap_or(0xFFFF_FFFF) as u16 as i16)
                .collect();
            mic(m, 2, (0..capacity_of(m)).map(|k| k as i16).collect());
            m.run(crate::run::RunLimits::insns(20_000));
            read
        };
        let original = continue_with(&mut m);
        let want: Vec<i16> = (160..400).collect();
        assert_eq!(original, want, "the RX period read the ring from its tail");
        for executor in [Executor::Engine, Executor::Reference] {
            let mut fresh = machine();
            fresh.restore(&snap).expect("restores");
            fresh.set_executor(executor);
            let ring = &fresh.io.audio_in;
            assert_eq!(
                (ring.tail(), ring.head(), ring.underflows(), ring.dropped()),
                (160, 800, 0, 7),
                "{executor:?}"
            );
            assert_eq!(continue_with(&mut fresh), want, "{executor:?}");
            assert_eq!(fresh.io.audio_in, m.io.audio_in, "{executor:?}");
            assert_eq!(fresh.state_hash(), m.state_hash(), "{executor:?}");
        }
    }

    fn capacity_of(m: &Machine) -> usize {
        m.io.audio_in.capacity()
    }

    /// The register sequence of `esp_sha_dma_process` on a channel other than pair 0, a
    /// two-descriptor chain split mid-block, completion at `blocks x sha_block_ps` on source 49.
    #[test]
    fn a_sha_dma_run_is_fed_from_the_bound_channel_and_survives_a_snapshot() {
        const SHA: u32 = 0x6003_B000;
        const DESC: u32 = DRAM + 0x2000;
        const BUF: u32 = DRAM + 0x2100;
        // "a" x 1000, padded to 16 blocks (FIPS 180-4 section 5.1.1).
        let mut data = vec![b'a'; 1000];
        data.push(0x80);
        data.resize(1016, 0);
        data.extend_from_slice(&8000u64.to_be_bytes());
        let want = "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3";

        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let cfg = MachineConfig {
            profile: crate::config::TimingProfileId::Device,
            ..MachineConfig::default()
        };
        let mut m = Machine::new(cfg, assets).expect("composes");
        enable_peripherals(&mut m);
        // A spin loop, so running the clock executes nothing that touches the blocks.
        let spin = pemu_soc_c3::mem::SRAM1_IRAM_BASE;
        m.soc.store_mem(spin, 4, 0x0000_006F);
        m.hart.pc = spin;

        poke(&mut m, BUF, &data);
        let (split, n) = (100u32, data.len() as u32);
        for (at, len, eof, buf, next) in [
            (DESC, split, false, BUF, DESC + 12),
            (DESC + 12, n - split, true, BUF + split, 0),
        ] {
            let w0 = len | (len << 12) | (u32::from(eof) << 30) | (1 << 31);
            for (i, w) in [w0, buf, next].into_iter().enumerate() {
                m.soc.store_mem(at + 4 * i as u32, 4, w);
            }
        }
        let lay = gdma::layout(1);
        let off = |i: usize| u32::from(pemu_soc_c3::r#gen::regs_gdma::REGS[i].off);
        mmio(&mut m, GDMA + off(lay.out_peri_sel), gdma::PERI_SHA);
        mmio(
            &mut m,
            GDMA + off(lay.out_link),
            (DESC & 0xF_FFFF) | (1 << 21),
        );
        mmio(&mut m, SHA, pemu_soc_c3::periph::sha::MODE_SHA256);
        mmio(&mut m, SHA + 0x28, 1);
        mmio(&mut m, SHA + 0x0C, 16);
        let t0 = m.now();
        mmio(&mut m, SHA + 0x1C, 1);
        assert_eq!(
            m.applied_wiring_by_kind().sha_dma,
            1,
            "the trigger's wiring was applied"
        );
        assert!(m.soc.devices.sha.busy(), "BUSY until the completion");
        let hex = |d: [u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            hex(m.soc.devices.sha.digest()),
            want,
            "the bound channel's bytes were hashed"
        );
        let block_ps = m.profile.sha_block_ps;
        assert!(block_ps > 0, "the device profile charges SHA blocks");

        let snap = m.snapshot(pemu_core::snap::SnapOpts::default());
        let complete = |m: &mut Machine| {
            let until = VTime(t0.0 + 16 * block_ps);
            m.run(crate::run::RunLimits {
                until: Some(VTime(until.0 - 1_000_000)),
                max_insns: None,
                stops: Default::default(),
            });
            assert!(
                m.soc.devices.sha.busy(),
                "not 1 us before 16 blocks of sha_block_ps (the run stops at an instruction boundary)"
            );
            assert!(!m.irq.source(pemu_core::irq_source::irq::SHA));
            m.run(crate::run::RunLimits {
                until: Some(VTime(until.0 + 1_000_000)),
                max_insns: None,
                stops: Default::default(),
            });
            assert!(!m.soc.devices.sha.busy(), "BUSY cleared at the completion");
            assert!(
                m.irq.source(pemu_core::irq_source::irq::SHA),
                "the completion raised source 49 under INT_ENA"
            );
        };
        complete(&mut m);
        let mut fresh = Machine::new(
            MachineConfig {
                profile: crate::config::TimingProfileId::Device,
                ..MachineConfig::default()
            },
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned"),
        )
        .expect("composes");
        fresh.restore(&snap).expect("restores");
        assert!(
            fresh.soc.devices.sha.busy(),
            "the run in flight was restored"
        );
        complete(&mut fresh);
        assert_eq!(hex(fresh.soc.devices.sha.digest()), want);
        assert_eq!(fresh.state_hash(), m.state_hash());
    }

    /// The register sequence of `esp_aes_process_dma` with TX and RX on different pairs, neither 0,
    /// over the SP 800-38A F.2.1 CBC-AES128 example: `STATE` 1 until `blocks x aes_block_ps`, 2
    /// after, completion on source 48.
    #[test]
    fn an_aes_dma_run_goes_from_the_tx_channel_to_the_rx_channel_and_survives_a_snapshot() {
        const AES: u32 = 0x6003_A000;
        const TX_DESC: u32 = DRAM + 0x2000;
        const RX_DESC: u32 = DRAM + 0x2020;
        const SRC: u32 = DRAM + 0x2100;
        const DST: u32 = DRAM + 0x2200;
        let unhex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
                .collect()
        };
        let key = unhex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = unhex("000102030405060708090a0b0c0d0e0f");
        let plain = unhex(
            "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51\
             30c81c46a35ce411e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710",
        );
        let want = "7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2\
                    73bed6b8e3c1743b7116e69e222295163ff1caa1681fac09120eca307586e1a7";
        let fresh_machine = || {
            let assets =
                Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                    .expect("the bundled ROM is pinned");
            let cfg = MachineConfig {
                profile: crate::config::TimingProfileId::Device,
                ..MachineConfig::default()
            };
            let mut m = Machine::new(cfg, assets).expect("composes");
            enable_peripherals(&mut m);
            m
        };
        let mut m = fresh_machine();
        let spin = pemu_soc_c3::mem::SRAM1_IRAM_BASE;
        m.soc.store_mem(spin, 4, 0x0000_006F);
        m.hart.pc = spin;

        poke(&mut m, SRC, &plain);
        for (at, buf) in [(TX_DESC, SRC), (RX_DESC, DST)] {
            let w0 = 64 | (64 << 12) | (1 << 30) | (1 << 31);
            for (i, w) in [w0, buf, 0].into_iter().enumerate() {
                m.soc.store_mem(at + 4 * i as u32, 4, w);
            }
        }
        let off = |i: usize| u32::from(pemu_soc_c3::r#gen::regs_gdma::REGS[i].off);
        let (tx, rx) = (gdma::layout(1), gdma::layout(2));
        mmio(&mut m, GDMA + off(tx.out_peri_sel), gdma::PERI_AES);
        mmio(&mut m, GDMA + off(rx.in_peri_sel), gdma::PERI_AES);
        mmio(
            &mut m,
            GDMA + off(tx.out_link),
            (TX_DESC & 0xF_FFFF) | (1 << 21),
        );
        mmio(
            &mut m,
            GDMA + off(rx.in_link),
            (RX_DESC & 0xF_FFFF) | (1 << 22),
        );
        for (i, word) in key.chunks(4).enumerate() {
            let w = u32::from_le_bytes(word.try_into().expect("whole words"));
            mmio(&mut m, AES + 4 * i as u32, w);
        }
        mmio(&mut m, AES + 0x40, pemu_soc_c3::periph::aes::MODE_ENC_128);
        mmio(&mut m, AES + 0x94, pemu_soc_c3::periph::aes::BLOCK_CBC);
        for (i, word) in iv.chunks(4).enumerate() {
            let w = u32::from_le_bytes(word.try_into().expect("whole words"));
            mmio(&mut m, AES + 0x50 + 4 * i as u32, w);
        }
        mmio(&mut m, AES + 0xB0, 1);
        mmio(&mut m, AES + 0x90, 1);
        mmio(&mut m, AES + 0x98, 4);
        let t0 = m.now();
        mmio(&mut m, AES + 0x48, 1);
        assert_eq!(
            m.applied_wiring_by_kind().aes_dma,
            1,
            "the trigger's wiring was applied"
        );
        let out: Vec<u8> = (0..64)
            .map(|i| m.soc.load_mem(DST + i, 1).expect("DRAM") as u8)
            .collect();
        let hex = |d: &[u8]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(&out), want, "the RX channel received the ciphertext");
        let owner = m.soc.load_mem(RX_DESC, 4).expect("DRAM") >> 31;
        assert_eq!(owner, 0, "the output descriptor is back with the CPU");
        assert_eq!(
            m.soc.devices.aes.state(),
            pemu_soc_c3::periph::aes::STATE_BUSY
        );
        let block_ps = m.profile.aes_block_ps;
        assert!(block_ps > 0, "the device profile charges AES blocks");

        let snap = m.snapshot(pemu_core::snap::SnapOpts::default());
        let complete = |m: &mut Machine| {
            let until = VTime(t0.0 + 4 * block_ps);
            m.run(crate::run::RunLimits {
                until: Some(VTime(until.0 - 100_000)),
                max_insns: None,
                stops: Default::default(),
            });
            assert!(
                m.soc.devices.aes.busy(),
                "not 0.1 us before 4 blocks of aes_block_ps"
            );
            assert!(!m.irq.source(pemu_core::irq_source::irq::AES));
            m.run(crate::run::RunLimits {
                until: Some(VTime(until.0 + 100_000)),
                max_insns: None,
                stops: Default::default(),
            });
            assert_eq!(
                m.soc.devices.aes.state(),
                pemu_soc_c3::periph::aes::STATE_DONE,
                "DONE at the completion"
            );
            assert!(
                m.irq.source(pemu_core::irq_source::irq::AES),
                "the completion raised source 48 under INT_ENA"
            );
        };
        complete(&mut m);
        let mut fresh = fresh_machine();
        fresh.restore(&snap).expect("restores");
        assert!(
            fresh.soc.devices.aes.busy(),
            "the run in flight was restored"
        );
        complete(&mut fresh);
        assert_eq!(hex(fresh.soc.devices.aes.iv()), &want[96..]);
        assert_eq!(fresh.state_hash(), m.state_hash());
    }
}
