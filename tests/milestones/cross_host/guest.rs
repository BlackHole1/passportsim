//! The parity guest: a merged 8 MB flash image the bundled ROM boots, built from code so the
//! scenario needs no corpus (a committed probe ELF would need the corpus's bootloader and
//! partition table).
//!
//! One ESP image at flash offset 0, entered like a bootloader, plus a code page and a data page
//! for the flash windows. Each phase prints its number and a running digest (`s0`, a multiply and
//! exclusive-or over every value read) through the ROM's `ets_printf` on both consoles.
//!
//! | Phase | What runs | The host-dependent part it exercises |
//! |---|---|---|
//! | 0 | the ROM boot: banner, SPI1 reads of the image header and segments, the checksum | the engine over ROM code, SPI1 command timing under `device`, both consoles and their pacing |
//! | 1 | 1500 rounds of `mul`, `divu`, `remu`, `mulhu` at the reset clock (XTAL / 2) | integer arithmetic and the per-quotient-bit divide cost of `device` |
//! | 2 | every peripheral clock on, then `SOC_CLK_SEL` to the PLL at 160 MHz | the clock rebase: the time base of every later instant changes mid-run |
//! | 3 | eight `RNG_DATA` reads | `DetRng` |
//! | 4 | one SHA-256 block through the accelerator | the SHA model and `sha_block_ps` |
//! | 5 | WREN, a 16-byte page program and the WIP poll over SPI1 | the flash store and `flash_delta`, the WIP time of `device` |
//! | 6 | two MMU entries, the ICache on, a flash-resident kernel over 24 KB of DROM called twice, the programmed bytes read back | the MMU, the cache model (`fifo16k` under `device`: 768 lines over a 16 KB cache, then 64 code lines one per line) |
//! | 7 | the ST7789 init, a 16 x 16 window of pixels, `DISPON`, the backlight on LEDC | SPI2, GDMA, the panel decoder, the frame port |
//! | 8 | I2S0 TX from a two-descriptor GDMA ring: a 500 Hz stereo triangle at 16 kHz for eight 16 ms periods | the I2S pacing events, the PCM ring |
//! | 9 | a second 4 x 4 window of pixels | a frame presented after the snapshot instant |
//! | 10 | the USB Serial/JTAG bytes and the button press journaled at the snapshot instant, an ADC one-shot of the ladder, `SYSTIMER` | the journal, the USJ OUT pump, the ladder and ADC, a time-derived register |
//!
//! Floats never reach this machine; the host analyses the PCM. [`SNAP`] and [`END`] are the
//! breakpoints: the snapshot after the second I2S period, the ring still running, and the final
//! `j .`.

use super::asm::*;

/// Where the ROM loads and enters the guest's code: the bootloader IRAM of `pemu_api`'s snapshot
/// guest.
const IRAM: u32 = 0x403C_E000;
/// Where the ROM loads the guest's data: the start of internal DRAM, clear of the ROM's own data
/// and of the code segment's DRAM alias at 0x3FCC_E000.
const DRAM: u32 = 0x3FC8_0000;
/// The flash page the flash-resident kernel lives in, and its IBUS address through MMU entry 0.
const KERNEL_FLASH: usize = 0x1_0000;
const IROM: u32 = 0x4200_0000;
/// The flash page of the data the kernel reads, and its DBUS address through MMU entry 1.
const TABLE_FLASH: usize = 0x2_0000;
const DROM_TABLE: u32 = 0x3C01_0000;
/// Cache lines the kernel reads: 24 KB, more than the 16 KB cache holds.
const TABLE_LINES: u32 = 768;
/// Where the page program writes: inside the data page, still erased, at DBUS 0x3C01_F000.
const PROGRAM_FLASH: u32 = 0x2_F000;
const PROGRAM_DROM: u32 = DROM_TABLE + (PROGRAM_FLASH - TABLE_FLASH as u32);

// Peripheral blocks (`pemu_soc_c3::periph` `c3_devices!`).
const SPI1: u32 = 0x6000_2000;
const GPIO_OUT: u32 = 0x6000_4004;
const LEDC: u32 = 0x6001_9000;
const SYSTIMER: u32 = 0x6002_3000;
const SPI2: u32 = 0x6002_4000;
const RNG_DATA: u32 = 0x6002_60B0;
const I2S0: u32 = 0x6002_D000;
const SHA: u32 = 0x6003_B000;
const GDMA: u32 = 0x6003_F000;
const SARADC: u32 = 0x6004_0000;
const USJ: u32 = 0x6004_3000;
const SYSTEM: u32 = 0x600C_0000;
const EXTMEM: u32 = 0x600C_4000;
const MMU: u32 = 0x600C_5000;

pub struct Guest {
    pub flash: Vec<u8>,
    /// Breakpoint where the snapshot is taken.
    pub snap: u32,
    /// Breakpoint of the final `j .`.
    pub end: u32,
}

/// Offsets inside the data segment at [`DRAM`].
mod data {
    /// The `ets_printf` format of a phase line.
    pub const FMT_PHASE: u32 = 0x000;
    /// The format of the input line.
    pub const FMT_INPUT: u32 = 0x020;
    /// GDMA descriptors of the display transactions, 12 bytes each.
    pub const LCD_DESC: u32 = 0x100;
    /// Their payloads, 16 bytes each (the pixel runs have their own buffers).
    pub const LCD_BUF: u32 = 0x200;
    /// 16 x 16 pixels, RGB565 big-endian.
    pub const PIXELS: u32 = 0x400;
    /// 4 x 4 pixels.
    pub const PIXELS2: u32 = 0x600;
    /// The two I2S descriptors.
    pub const PCM_DESC: u32 = 0x700;
    /// The two PCM buffers, [`super::PCM_BYTES`] each.
    pub const PCM_A: u32 = 0x800;
    pub const PCM_B: u32 = 0xC00;
    /// End of the segment.
    pub const END: u32 = 0x1000;
}

/// One SPI2 transaction of the panel: the bytes, `dc`, and whether CS is released after it.
struct Lcd {
    bytes: Vec<u8>,
    dc: bool,
    release: bool,
}

fn command(cmd: u8, params: &[u8]) -> Vec<Lcd> {
    let mut out = vec![Lcd {
        bytes: vec![cmd],
        dc: false,
        release: params.is_empty(),
    }];
    if !params.is_empty() {
        out.push(Lcd {
            bytes: params.to_vec(),
            dc: true,
            release: true,
        });
    }
    out
}

/// The ST7789 init and first picture (phase 7), and the second picture (phase 9).
fn panel_transactions() -> (Vec<Lcd>, Vec<Lcd>) {
    let pixels: Vec<u8> = (0..16u16)
        .flat_map(|y| {
            (0..16u16).flat_map(move |x| ((x * 2) << 11 | (y * 4) << 5 | (x + y)).to_be_bytes())
        })
        .collect();
    let mut first = Vec::new();
    first.extend(command(0x11, &[])); // SLPOUT
    first.extend(command(0x3A, &[0x55])); // COLMOD 16 bpp
    first.extend(command(0x36, &[0x00])); // MADCTL
    first.extend(command(0x2A, &[0, 0, 0, 15])); // CASET 0..15
    first.extend(command(0x2B, &[0, 0, 0, 15])); // RASET 0..15
    first.extend(command(0x2C, &[])); // RAMWR, CS kept for the pixels
    first.last_mut().unwrap().release = false;
    first.push(Lcd {
        bytes: pixels,
        dc: true,
        release: true,
    });
    first.extend(command(0x29, &[])); // DISPON
    let mut second = Vec::new();
    second.extend(command(0x2A, &[0, 4, 0, 7]));
    second.extend(command(0x2B, &[0, 4, 0, 7]));
    second.extend(command(0x2C, &[]));
    second.last_mut().unwrap().release = false;
    second.push(Lcd {
        bytes: (0..16u16)
            .flat_map(|i| (0xF800 ^ (i * 0x0841)).to_be_bytes())
            .collect(),
        dc: true,
        release: true,
    });
    (first, second)
}

/// Bytes of one PCM buffer: 256 stereo 16-bit frames, 16 ms at 16 kHz.
const PCM_BYTES: u32 = 1024;

/// I2S periods the guest plays: 128 ms, past the 1602 samples `audio_capture::fundamental_hz`
/// needs at 16 kHz, so the host-side analysis runs its whole float path.
const PERIODS: u32 = 8;

/// The 500 Hz triangle, stereo (left full, right half), 256 frames per buffer at 16 kHz: a whole
/// number of its 32-frame periods, so the ring plays one continuous tone.
fn pcm_buffer() -> Vec<u8> {
    (0..PCM_BYTES / 4)
        .flat_map(|i| {
            let k = (i % 32) as i32;
            let v = (if k < 16 { k } else { 32 - k }) * 2000 - 16000;
            let (l, r) = (v as i16, (v / 2) as i16);
            [l.to_le_bytes(), r.to_le_bytes()].concat()
        })
        .collect()
}

/// A GDMA descriptor (IDF `hal/include/hal/dma_types.h`): `size`, `length`, `suc_eof` and the owner
/// bit, the buffer, the next descriptor.
fn descriptor(len: u32, buffer: u32, next: u32) -> [u8; 12] {
    let w0 = len | len << 12 | 1 << 30 | 1 << 31;
    let mut out = [0; 12];
    for (i, w) in [w0, buffer, next].into_iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// The offset of register `i` of a generated table.
fn off(regs: &[pemu_core::regstore::RegSpec], i: usize) -> u32 {
    u32::from(regs[i].off)
}

pub fn build(ets_printf: u32) -> Guest {
    use pemu_soc_c3::r#gen::{regs_gdma, regs_ledc, regs_saradc, regs_systimer};
    use pemu_soc_c3::periph::{gdma, spi2};

    let (first, second) = panel_transactions();
    let mut dram = vec![0u8; data::END as usize];
    let put = |dram: &mut Vec<u8>, at: u32, bytes: &[u8]| {
        dram[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
    };
    put(&mut dram, data::FMT_PHASE, b"D3 %d %08x\n\0");
    put(&mut dram, data::FMT_INPUT, b"D3 in %d adc %d\n\0");
    // Every display transaction gets a descriptor; small payloads share `LCD_BUF`.
    let mut lcd = Vec::new();
    for (i, t) in first.iter().chain(&second).enumerate() {
        let desc = DRAM + data::LCD_DESC + 12 * i as u32;
        let buffer = match t.bytes.len() {
            512 => DRAM + data::PIXELS,
            32 => DRAM + data::PIXELS2,
            n => {
                assert!(n <= 16);
                DRAM + data::LCD_BUF + 16 * i as u32
            }
        };
        put(&mut dram, buffer - DRAM, &t.bytes);
        put(
            &mut dram,
            desc - DRAM,
            &descriptor(t.bytes.len() as u32, buffer, 0),
        );
        lcd.push((desc, t.bytes.len() as u32, t.dc, t.release));
    }
    assert!(data::LCD_DESC + 12 * lcd.len() as u32 <= data::LCD_BUF);
    let pcm = pcm_buffer();
    let (desc_a, desc_b) = (DRAM + data::PCM_DESC, DRAM + data::PCM_DESC + 12);
    put(&mut dram, data::PCM_A, &pcm);
    put(&mut dram, data::PCM_B, &pcm);
    put(
        &mut dram,
        data::PCM_DESC,
        &descriptor(PCM_BYTES, DRAM + data::PCM_A, desc_b),
    );
    put(
        &mut dram,
        data::PCM_DESC + 12,
        &descriptor(PCM_BYTES, DRAM + data::PCM_B, desc_a),
    );

    let mut a = Asm::new(IRAM);
    // `s0` = s0 * s1 ^ reg: the running digest.
    let mix = |a: &mut Asm, reg: u32| {
        a.mul(S0, S0, S1);
        a.xor(S0, S0, reg);
    };
    let print = |a: &mut Asm, phase: u32| {
        a.li(A0, DRAM + data::FMT_PHASE);
        a.li(A1, phase);
        a.mv(A2, S0);
        a.call_abs(ets_printf);
    };
    a.li(S1, 0x0100_0193);
    a.li(S0, 0x811C_9DC5);
    print(&mut a, 0);

    // Phase 1: integer arithmetic at the reset clock.
    a.li(S2, 1500);
    a.li(A3, 12345);
    let top = a.label();
    a.bind(top);
    a.li(T1, 1_103_515_245);
    a.mul(A3, A3, T1);
    a.li(T1, 12345);
    a.add(A3, A3, T1);
    a.srli(T2, A3, 16);
    a.andi(T2, T2, 0xFF);
    a.addi(T2, T2, 1);
    a.divu(A4, A3, T2);
    a.li(T1, 97);
    a.remu(A0, A3, T1);
    a.xor(A0, A0, A4);
    mix(&mut a, A0);
    a.addi(S2, S2, -1);
    a.bne(S2, ZERO, top);
    a.mulhu(A0, A3, S1);
    mix(&mut a, A0);
    print(&mut a, 1);

    // Phase 2: every peripheral clocked and out of reset, as the drivers leave the blocks they use
    // (`pemu_machine`'s DMA tests do the same), then the CPU on the PLL at 160 MHz.
    for (reg, value) in [(0x10, u32::MAX), (0x14, u32::MAX), (0x18, 0), (0x1C, 0)] {
        a.store(SYSTEM + reg, value);
    }
    a.store(SYSTEM + 0x008, 1); // CPU_PER_CONF.CPUPERIOD_SEL: 160 MHz on the PLL
    a.store(SYSTEM + 0x058, 1 << 10); // SYSCLK_CONF.SOC_CLK_SEL: PLL
    a.store(
        SYSTIMER + off(&regs_systimer::REGS, regs_systimer::idx::SYSTIMER_UNIT0_OP),
        1 << 30,
    );
    a.wait(
        SYSTIMER + off(&regs_systimer::REGS, regs_systimer::idx::SYSTIMER_UNIT0_OP),
        1 << 29,
        true,
    );
    a.load(
        A0,
        SYSTIMER
            + off(
                &regs_systimer::REGS,
                regs_systimer::idx::SYSTIMER_UNIT0_VALUE_LO,
            ),
    );
    mix(&mut a, A0);
    print(&mut a, 2);

    // Phase 3: the guest entropy source.
    for _ in 0..8 {
        a.load(A0, RNG_DATA);
        mix(&mut a, A0);
    }
    print(&mut a, 3);

    // Phase 4: one SHA-256 block (mode 2) of words derived from the digest so far.
    a.store(SHA, 2);
    for i in 0..16 {
        a.li(T1, 0x0101_0101u32.wrapping_mul(i + 1));
        a.xor(T1, T1, S0);
        a.li(T0, SHA + 0x80 + 4 * i);
        a.sw(T1, T0, 0);
    }
    a.store(SHA + 0x10, 1);
    a.wait(SHA + 0x18, 1, false);
    for i in 0..8 {
        a.load(A0, SHA + 0x40 + 4 * i);
        mix(&mut a, A0);
    }
    print(&mut a, 4);

    // Phase 5: program 16 bytes at `PROGRAM_FLASH` over SPI1 (the sequence of `pemu_api`'s
    // snapshot guest), waiting for each command's bit to clear, as `device` needs.
    const WREN: u32 = 1 << 30;
    const PP: u32 = 1 << 25;
    const RDSR: u32 = 1 << 27;
    a.store(SPI1, WREN);
    a.wait(SPI1, WREN, false);
    for i in 0..4 {
        a.li(T1, 0x5041_5249u32.rotate_left(8 * i) ^ 0x0F0F_0F0F);
        a.xor(T1, T1, S0);
        a.li(T0, SPI1 + 0x58 + 4 * i);
        a.sw(T1, T0, 0);
    }
    a.store(SPI1 + 0x04, PROGRAM_FLASH | 16 << 24);
    a.store(SPI1, PP);
    a.wait(SPI1, PP, false);
    let wip = a.label();
    a.bind(wip);
    a.store(SPI1, RDSR);
    a.wait(SPI1, RDSR, false);
    a.load(T2, SPI1 + 0x2C);
    a.andi(T2, T2, 1);
    a.bne(T2, ZERO, wip);
    print(&mut a, 5);

    // Phase 6: the flash windows through the MMU and the cache.
    a.store(EXTMEM, 1); // ICACHE_CTRL.ICACHE_ENABLE
    a.store(EXTMEM + 0x004, 0); // ICACHE_CTRL1: neither bus shut
    a.store(MMU, (KERNEL_FLASH >> 16) as u32);
    a.store(MMU + 4, (TABLE_FLASH >> 16) as u32);
    a.store(EXTMEM + 0x028, 1); // ICACHE_SYNC_CTRL.INVALIDATE_ENA
    a.wait(EXTMEM + 0x028, 1 << 1, true);
    for _ in 0..2 {
        a.li(A0, DROM_TABLE);
        a.li(A1, TABLE_LINES);
        a.li(A2, 32);
        a.call_abs(IROM);
        mix(&mut a, A0);
    }
    for i in 0..4 {
        a.load(A0, PROGRAM_DROM + 4 * i);
        mix(&mut a, A0);
    }
    print(&mut a, 6);

    // Phase 7: the panel over SPI2 and GDMA channel 0, then the backlight.
    let dma = |i: usize| GDMA + off(&regs_gdma::REGS, i);
    let transact = |a: &mut Asm, (desc, len, dc, release): (u32, u32, bool, bool)| {
        a.store(GPIO_OUT, u32::from(dc) << 20);
        a.store(dma(gdma::layout(0).out_peri_sel), gdma::PERI_SPI2);
        a.store(dma(gdma::layout(0).out_link), (desc & 0xF_FFFF) | 1 << 21);
        a.store(SPI2 + 0x1C, len * 8 - 1);
        a.store(SPI2 + 0x10, spi2::USER_USR_MOSI);
        a.store(SPI2 + 0x30, spi2::DMA_CONF_TX_ENA);
        a.store(
            SPI2 + 0x20,
            if release {
                0
            } else {
                spi2::MISC_CS_KEEP_ACTIVE
            },
        );
        a.store(SPI2, spi2::CMD_USR);
        a.wait(SPI2, spi2::CMD_USR, false);
    };
    for t in &lcd[..first.len()] {
        transact(&mut a, *t);
    }
    let ledc = |i: usize| LEDC + off(&regs_ledc::REGS, i);
    a.store(
        ledc(regs_ledc::idx::LEDC_LSTIMER0_CONF),
        10 | 0x100 << 4 | 1 << 25,
    );
    a.store(ledc(regs_ledc::idx::LEDC_LSCH0_DUTY), 512 << 4);
    a.store(ledc(regs_ledc::idx::LEDC_LSCH0_CONF0), 1 << 2 | 1 << 4);
    print(&mut a, 7);

    // Phase 8: I2S0 TX at 16 kHz, 16 bits, two slots, from the ring on GDMA channel 1
    // (`pemu_machine`'s I2S tests program the same clock registers).
    let ch1 = gdma::layout(1);
    a.store(dma(ch1.out_peri_sel), gdma::PERI_I2S0);
    a.store(dma(ch1.out_link), (desc_a & 0xF_FFFF) | 1 << 21);
    a.store(I2S0 + 0x034, 2 << 27 | 1 << 26 | 39); // TX_CLKM_CONF
    a.store(I2S0 + 0x03C, 15 << 18 | 1); // TX_CLKM_DIV_CONF
    a.store(
        I2S0 + 0x02C,
        15 | 7 << 7 | 15 << 13 | 15 << 18 | 15 << 24 | 1 << 29,
    ); // TX_CONF1
    a.store(I2S0 + 0x054, 0b11 | 1 << 16); // TX_TDM_CTRL: two slots
    a.store(I2S0 + 0x024, 1 << 19 | 1 << 15 | 1 << 2); // TX_CONF with TX_START
    let snap = a.label();
    for period in 0..PERIODS {
        if period == 2 {
            a.bind(snap);
            a.addi(ZERO, ZERO, 0);
        }
        a.wait(dma(ch1.int_raw), gdma::int::OUT_EOF, true);
        a.store(dma(ch1.int_clr), gdma::int::OUT_EOF);
    }
    a.store(I2S0 + 0x024, 1 << 19 | 1 << 15); // TX_START cleared
    a.store(dma(ch1.out_link), 1 << 20); // OUTLINK_STOP
    print(&mut a, 8);

    // Phase 9: a second picture, after the snapshot instant.
    for t in &lcd[first.len()..] {
        transact(&mut a, *t);
    }
    print(&mut a, 9);

    // Phase 10: the journaled input. The bytes arrive one USB packet per SOF millisecond; the
    // guest waits for the first and reads while `SERIAL_OUT_EP_DATA_AVAIL` holds.
    a.li(S3, 0);
    a.wait(USJ + 0x04, 1 << 2, true);
    let more = a.label();
    let done = a.label();
    a.bind(more);
    a.load(T2, USJ + 0x04);
    a.andi(T2, T2, 1 << 2);
    a.beq(T2, ZERO, done);
    a.load(A0, USJ);
    a.andi(A0, A0, 0xFF);
    mix(&mut a, A0);
    a.addi(S3, S3, 1);
    a.j(more);
    a.bind(done);
    // The ladder through an ADC1 one-shot on channel 0 at 12 dB.
    let adc = |i: usize| SARADC + off(&regs_saradc::REGS, i);
    let sample = 1 << 31 | 3 << 23;
    a.store(adc(regs_saradc::idx::APB_SARADC_INT_CLR), 1 << 31);
    a.store(adc(regs_saradc::idx::APB_SARADC_ONETIME_SAMPLE), sample);
    a.store(
        adc(regs_saradc::idx::APB_SARADC_ONETIME_SAMPLE),
        sample | 1 << 29,
    );
    a.wait(adc(regs_saradc::idx::APB_SARADC_INT_RAW), 1 << 31, true);
    a.load(S4, adc(regs_saradc::idx::APB_SARADC_1_DATA_STATUS));
    a.li(T1, 0xFFF);
    a.and(S4, S4, T1);
    mix(&mut a, S4);
    a.store(
        SYSTIMER + off(&regs_systimer::REGS, regs_systimer::idx::SYSTIMER_UNIT0_OP),
        1 << 30,
    );
    a.wait(
        SYSTIMER + off(&regs_systimer::REGS, regs_systimer::idx::SYSTIMER_UNIT0_OP),
        1 << 29,
        true,
    );
    a.load(
        A0,
        SYSTIMER
            + off(
                &regs_systimer::REGS,
                regs_systimer::idx::SYSTIMER_UNIT0_VALUE_LO,
            ),
    );
    mix(&mut a, A0);
    a.li(A0, DRAM + data::FMT_INPUT);
    a.mv(A1, S3);
    a.mv(A2, S4);
    a.call_abs(ets_printf);
    print(&mut a, 10);
    let end = a.label();
    a.bind(end);
    a.j(end);
    let (snap, end) = (a.addr(snap), a.addr(end));
    let code = a.finish();

    let mut flash = vec![0xFFu8; 8 << 20];
    let image = esp_image(IRAM, &[(IRAM, &code), (DRAM, &dram)]);
    assert!(
        image.len() < KERNEL_FLASH,
        "the image stays below the kernel page"
    );
    flash[..image.len()].copy_from_slice(&image);
    let kernel = kernel();
    flash[KERNEL_FLASH..KERNEL_FLASH + kernel.len()].copy_from_slice(&kernel);
    // The table: an LCG's words, fixed by the seed.
    let mut x: u32 = 0x2545_F491;
    for i in 0..(TABLE_LINES as usize * 32 / 4) {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        flash[TABLE_FLASH + 4 * i..TABLE_FLASH + 4 * i + 4].copy_from_slice(&x.to_le_bytes());
    }
    assert!(TABLE_FLASH + TABLE_LINES as usize * 32 <= PROGRAM_FLASH as usize);
    Guest { flash, snap, end }
}

/// The flash-resident kernel at [`IROM`]: `a0` the table, `a1` the lines, `a2` the stride; one
/// load a line folded into `a3`, then 64 code blocks each alone in its own 32-byte line, so the
/// fetches miss too. Returns the fold in `a0`.
fn kernel() -> Vec<u8> {
    let mut a = Asm::new(IROM);
    a.li(A3, 0);
    let top = a.label();
    a.bind(top);
    a.lw(T0, A0, 0);
    a.slli(T1, A3, 5);
    a.add(A3, A3, T1);
    a.xor(A3, A3, T0);
    a.add(A0, A0, A2);
    a.addi(A1, A1, -1);
    a.bne(A1, ZERO, top);
    let blocks: Vec<Label> = (0..=64).map(|_| a.label()).collect();
    a.j(blocks[0]);
    for (k, pair) in blocks.windows(2).enumerate() {
        let line = IROM + 0x100 + 32 * k as u32;
        while a.here() < line {
            a.addi(ZERO, ZERO, 0);
        }
        a.bind(pair[0]);
        a.addi(A3, A3, k as i32 + 1);
        a.slli(T1, A3, 3);
        a.xor(A3, A3, T1);
        a.j(pair[1]);
    }
    a.bind(blocks[64]);
    a.mv(A0, A3);
    a.ret();
    a.finish()
}

/// An ESP image (the header of `esp_image_format.h` as `pemu_api`'s snapshot guest writes it:
/// DIO, 8 MB at 80 MHz, chip id 5) with `segments`, entered at `entry`, and the SHA-256 of all of
/// it appended, which the ROM checks before it enters.
fn esp_image(entry: u32, segments: &[(u32, &[u8])]) -> Vec<u8> {
    let mut out = vec![0xE9, segments.len() as u8, 2, 0x3F];
    out.extend_from_slice(&entry.to_le_bytes());
    out.extend_from_slice(&[0xEE, 0, 0, 0]);
    out.extend_from_slice(&5u16.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.push(1); // hash_appended: the ROM verifies the SHA-256 below with its SHA engine
    let mut checksum = 0xEFu8;
    for (addr, bytes) in segments {
        let mut body = bytes.to_vec();
        body.resize(body.len().next_multiple_of(4), 0);
        out.extend_from_slice(&addr.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        checksum = body.iter().fold(checksum, |c, b| c ^ b);
        out.extend_from_slice(&body);
    }
    while out.len() % 16 != 15 {
        out.push(0);
    }
    out.push(checksum);
    let digest = pemu_loader::sha256(&out);
    out.extend_from_slice(&digest);
    out
}
