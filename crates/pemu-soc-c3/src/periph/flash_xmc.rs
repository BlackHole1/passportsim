//! The on-package 8 MB XMC NOR flash behind SPI1 (`specs/blocks/flash_xmc.toml`). It has no MMIO
//! window; it is reached through the SPI1 command host (`super::spi_mem`).
//!
//! This file is the part's state machine: status registers, write-enable latch, WIP timing,
//! JEDEC identity and command decode. The array is [`crate::flash_store::FlashStore`], shared with
//! the cache read path, so one transaction is two calls: [`XmcChip::accept`] for the chip
//! registers, then [`apply`] for the array work it returns.
//!
//! The identity `20 40 17` is class A (a device capture; `esp_flash_api.c` reads it twice and
//! refuses a mismatch). The status layout is the Winbond-compatible one the generic driver assumes
//! (class B). The `device` WIP durations are NOR typicals, UNVERIFIED for this part. SFDP answers
//! erased bytes (class C): `is_xmc_chip_strict` accepts the identity, so nothing is known to read
//! it.

use pemu_core::serde::{Deserialize, Serialize};

use crate::flash_store::{BLOCK_LEN, ERASED, FlashStore, PAGE_LEN};
use crate::mem::FLASH_LEN;

/// JEDEC identity in the order `0x9F` streams it: manufacturer, memory type, capacity.
pub const JEDEC_ID: [u8; 3] = [0x20, 0x40, 0x17];

/// Capacity the generic driver derives from the identity: `1 << (id & 0xFF)` = 8 MiB.
pub const DETECTED_SIZE: u32 = 1 << JEDEC_ID[2];

/// Bytes of a 32 KB block erase (`0x52`).
pub const BLOCK32_LEN: u32 = 0x8000;

/// Bytes of one program page: a page program that runs past the end of its page continues at the
/// start of the same page. 256 is every generic-driver part's page (`spi_flash_chip_generic.c`
/// `page_size`); UNVERIFIED for this part, so the wrap is class C.
pub const PROGRAM_PAGE: u32 = 0x100;

/// Longest data phase of one transaction: the 64-byte W0 to W15 buffer of the host.
pub const MAX_DATA: usize = 64;

/// `SR1` bit 0, set while a program, erase or status write is in progress.
pub const SR1_WIP: u8 = 1 << 0;
/// `SR1` bit 1, the write-enable latch.
pub const SR1_WEL: u8 = 1 << 1;
/// `SR2` bit 7, the suspend flag. Auto-suspend is not configured, so it stays 0.
pub const SR2_SUS: u8 = 1 << 7;

/// Flash command opcodes the part answers.
pub mod op {
    /// Write status register, 1 or 2 bytes.
    pub const WRSR: u8 = 0x01;
    /// Page program.
    pub const PP: u8 = 0x02;
    /// Read data.
    pub const READ: u8 = 0x03;
    /// Write disable.
    pub const WRDI: u8 = 0x04;
    /// Read status register 1.
    pub const RDSR: u8 = 0x05;
    /// Write enable.
    pub const WREN: u8 = 0x06;
    /// Fast read.
    pub const FAST_READ: u8 = 0x0B;
    /// Sector erase, 4 KB.
    pub const SE: u8 = 0x20;
    /// Write status register 2.
    pub const WRSR2: u8 = 0x31;
    /// Read status register 2.
    pub const RDSR2: u8 = 0x35;
    /// Dual output read.
    pub const DOR: u8 = 0x3B;
    /// Read unique id.
    pub const RDUID: u8 = 0x4B;
    /// Block erase, 32 KB.
    pub const BE32K: u8 = 0x52;
    /// Read SFDP.
    pub const RDSFDP: u8 = 0x5A;
    /// Chip erase, the `0x60` spelling.
    pub const CE_60: u8 = 0x60;
    /// Reset enable.
    pub const RSTEN: u8 = 0x66;
    /// Quad output read.
    pub const QOR: u8 = 0x6B;
    /// Erase / program suspend.
    pub const PES: u8 = 0x75;
    /// Erase / program resume.
    pub const PER: u8 = 0x7A;
    /// Read JEDEC identity.
    pub const RDID: u8 = 0x9F;
    /// Reset.
    pub const RST: u8 = 0x99;
    /// High performance mode.
    pub const HPM: u8 = 0xA3;
    /// Release from deep power-down.
    pub const RES: u8 = 0xAB;
    /// Deep power-down.
    pub const DP: u8 = 0xB9;
    /// Dual I/O read.
    pub const DIOR: u8 = 0xBB;
    /// Chip erase, the `0xC7` spelling.
    pub const CE_C7: u8 = 0xC7;
    /// Block erase, 64 KB.
    pub const BE: u8 = 0xD8;
    /// Quad I/O read.
    pub const QIOR: u8 = 0xEB;
}

/// How long the part keeps `WIP` set per operation class, in picoseconds. A machine runs the
/// `flash_*_ps` rows of `specs/timing-profiles.toml`; the constants below are the same numbers for
/// tests and a part built without a machine.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct FlashTiming {
    /// Page program, and the status writes that share its duration.
    pub pp_ps: u64,
    /// Sector erase, 4 KB.
    pub se_ps: u64,
    /// Block erase; the 32 KB erase shares it (UNVERIFIED: only the 64 KB figure is known).
    pub be_ps: u64,
    pub ce_ps: u64,
}

impl FlashTiming {
    /// The `fast` profile: `WIP` clears at a completion event scheduled at `now`.
    pub const FAST: FlashTiming = FlashTiming {
        pp_ps: 0,
        se_ps: 0,
        be_ps: 0,
        ce_ps: 0,
    };

    /// The `device` profile: PP 0.7 ms, SE 45 ms, BE64K 150 ms, CE 20 s. NOR typicals,
    /// UNVERIFIED for this part.
    pub const DEVICE: FlashTiming = FlashTiming {
        pp_ps: 700_000_000,
        se_ps: 45_000_000_000,
        be_ps: 150_000_000_000,
        ce_ps: 20_000_000_000_000,
    };
}

impl Default for FlashTiming {
    fn default() -> FlashTiming {
        FlashTiming::FAST
    }
}

/// Array work one transaction left for the flash store.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub enum ArrayOp {
    /// Stream `len` bytes out of the array from `addr` (`0x03`, `0x0B` and the dual and quad
    /// spellings, which differ only in line count).
    Read {
        addr: u32,
        /// Bytes to stream, at most [`MAX_DATA`].
        len: u8,
    },
    /// Program `len` bytes at `addr` (`0x02`), clearing bits only.
    Program {
        addr: u32,
        /// Bytes to program, at most [`MAX_DATA`].
        len: u8,
    },
    /// Erase the 4 KB sector holding `addr` (`0x20`).
    EraseSector(u32),
    /// Erase the 32 KB block holding `addr` (`0x52`).
    EraseBlock32(u32),
    /// Erase the 64 KB block holding `addr` (`0xD8`).
    EraseBlock(u32),
    /// Erase the whole array (`0x60` or `0xC7`).
    EraseChip,
}

/// Flash pages one transaction changed, so the caller can invalidate what the cache holds.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Written {
    /// Index of the first changed [`PAGE_LEN`] page.
    pub first_page: u32,
    pub pages: u32,
}

/// What [`XmcChip::accept`] made of one chip-select transaction.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Accepted {
    /// Array work left for [`apply`], if any.
    pub array: Option<ArrayOp>,
    /// Picoseconds the part keeps `WIP` set, 0 under the `fast` profile.
    pub busy_ps: u64,
    /// The opcode named no command this part answers, so the transaction did nothing. The caller
    /// records it.
    pub unknown: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct XmcChip {
    sr1: u8,
    sr2: u8,
    /// Deep power-down: after `0xB9` the part answers only `0xAB`.
    powered_down: bool,
    /// `0x66` seen and not yet consumed: only then does `0x99` reset the part.
    reset_armed: bool,
    timing: FlashTiming,
}

impl Default for XmcChip {
    fn default() -> XmcChip {
        XmcChip::new(FlashTiming::FAST)
    }
}

impl XmcChip {
    /// A part at power-on: `SR1` and `SR2` both 0, so `bootloader_flash_unlock_default` issues no
    /// status write.
    pub fn new(timing: FlashTiming) -> XmcChip {
        XmcChip {
            sr1: 0,
            sr2: 0,
            powered_down: false,
            reset_armed: false,
            timing,
        }
    }

    /// Status register 1: `WIP` bit 0, `WEL` bit 1, block protection above.
    pub fn sr1(&self) -> u8 {
        self.sr1
    }

    /// Status register 2: `QE` bit 1, `SUS` bit 7. `QE` stays 0 in this DIO build.
    pub fn sr2(&self) -> u8 {
        self.sr2
    }

    pub fn busy(&self) -> bool {
        self.sr1 & SR1_WIP != 0
    }

    pub fn powered_down(&self) -> bool {
        self.powered_down
    }

    pub fn timing(&self) -> FlashTiming {
        self.timing
    }

    /// Selects the WIP durations; the machine sets them from the timing profile.
    pub fn set_timing(&mut self, timing: FlashTiming) {
        self.timing = timing;
    }

    /// Completion of the operation that set `WIP`: `WIP` and `WEL` clear, which the generic
    /// driver checks after every program and erase.
    pub fn complete(&mut self) {
        self.sr1 &= !(SR1_WIP | SR1_WEL);
    }

    /// The power-on state, which `0x66` + `0x99` restores: `WEL`, `WIP`, `SUS` and deep
    /// power-down clear, the array untouched. No MCU reset scope reaches the part.
    pub fn reset_chip(&mut self) {
        self.sr1 &= !(SR1_WIP | SR1_WEL);
        self.sr2 &= !SR2_SUS;
        self.powered_down = false;
        self.reset_armed = false;
    }

    /// Runs the chip-register half of one transaction: `opcode` with `addr` and data-out `tx`,
    /// filling `rx` with what it can answer without the array. Bytes a command does not drive stay
    /// [`ERASED`]. The array half comes back in [`Accepted::array`] for [`apply`].
    pub fn accept(&mut self, opcode: u8, addr: u32, tx: &[u8], rx: &mut [u8]) -> Accepted {
        rx.fill(ERASED);
        // In deep power-down the part answers 0xAB and nothing else.
        if self.powered_down && opcode != op::RES {
            return Accepted::default();
        }
        // The reset latch is consumed by the next command, whatever it is.
        let armed = core::mem::take(&mut self.reset_armed);
        if self.busy() && !matches!(opcode, op::RDSR | op::RDSR2 | op::RSTEN | op::RST) {
            // While `WIP` is set the part answers only the status reads and the `0x66` + `0x99`
            // abort pair, as deep power-down ignores all but `0xAB`. Otherwise a second program
            // would reach the array and its window would end at the first one's deadline, since
            // only the command that set `WIP` schedules a completion. Class C, the usual NOR rule;
            // no ROM or IDF path reaches it, since all poll `WIP` first.
            return Accepted::default();
        }
        let mut out = Accepted::default();
        match opcode {
            op::WREN => self.sr1 |= SR1_WEL,
            op::WRDI => self.sr1 &= !SR1_WEL,
            op::RDSR => rx.fill(self.sr1),
            op::RDSR2 => rx.fill(self.sr2),
            op::RDID => {
                for (i, byte) in rx.iter_mut().enumerate() {
                    *byte = JEDEC_ID[i % JEDEC_ID.len()];
                }
            }
            op::WRSR | op::WRSR2 => {
                if self.sr1 & SR1_WEL == 0 {
                    // A status write without WEL is ignored.
                    return out;
                }
                let (first, second) = (tx.first().copied(), tx.get(1).copied());
                match (opcode, first, second) {
                    (op::WRSR, Some(v), second) => {
                        self.write_sr1(v);
                        if let Some(v) = second {
                            self.sr2 = v;
                        }
                    }
                    (op::WRSR2, Some(v), _) => self.sr2 = v,
                    _ => return out,
                }
                self.sr1 |= SR1_WIP;
                out.busy_ps = self.timing.pp_ps;
            }
            op::READ | op::FAST_READ | op::DOR | op::QOR | op::DIOR | op::QIOR => {
                out.array = Some(ArrayOp::Read {
                    addr,
                    len: data_len(rx.len()),
                });
            }
            op::RDSFDP => {
                // Class C: nothing in this configuration is known to read SFDP.
            }
            op::RDUID => {
                // Synthesized, never copied from a device: zeros are unmistakably synthetic
                // (docs/secrets.md). Class C.
                let skip = rx.len().saturating_sub(8);
                rx[skip..].fill(0);
            }
            op::PP => {
                if self.sr1 & SR1_WEL == 0 {
                    return out;
                }
                out.array = Some(ArrayOp::Program {
                    addr,
                    len: data_len(tx.len()),
                });
                self.sr1 |= SR1_WIP;
                out.busy_ps = self.timing.pp_ps;
            }
            op::SE | op::BE32K | op::BE | op::CE_60 | op::CE_C7 => {
                if self.sr1 & SR1_WEL == 0 {
                    return out;
                }
                let (array, busy_ps) = match opcode {
                    op::SE => (ArrayOp::EraseSector(addr), self.timing.se_ps),
                    op::BE32K => (ArrayOp::EraseBlock32(addr), self.timing.be_ps),
                    op::BE => (ArrayOp::EraseBlock(addr), self.timing.be_ps),
                    _ => (ArrayOp::EraseChip, self.timing.ce_ps),
                };
                out.array = Some(array);
                self.sr1 |= SR1_WIP;
                out.busy_ps = busy_ps;
            }
            op::DP => self.powered_down = true,
            op::RES => {
                self.powered_down = false;
                // The release command also streams an electronic identity. UNVERIFIED for this
                // part; the ROM `SPI_WakeUp` path never reads it.
                rx.fill(0x16);
            }
            op::RSTEN => self.reset_armed = true,
            op::RST => {
                if armed {
                    self.reset_chip();
                }
            }
            // High performance mode is not configured, and `spi_flash_hal_device_config`
            // disables auto-suspend, so the part never sees a real suspend.
            op::HPM | op::PES | op::PER => {}
            _ => out.unknown = true,
        }
        out
    }

    /// `WRSR` data byte into `SR1`, keeping `WIP` (hardware state) and `WEL` (cleared by the
    /// completion, not by the written value).
    fn write_sr1(&mut self, value: u8) {
        self.sr1 = (self.sr1 & (SR1_WIP | SR1_WEL)) | (value & !(SR1_WIP | SR1_WEL));
    }
}

/// Performs the array half of a transaction against `flash` and returns the pages a change
/// touched.
///
/// The part decodes only the low 23 bits of an address ([`decoded`]), so 0x800000 reads what
/// 0x000000 holds. Class A for a read (the `probe_campaign_regs` capture's `FLASH|read_0x800000`
/// reads the bootloader header back). A program or erase lands on the same mirror, UNVERIFIED.
pub fn apply(op: ArrayOp, tx: &[u8], rx: &mut [u8], flash: &mut FlashStore) -> Option<Written> {
    match op {
        ArrayOp::Read { addr, len } => {
            let len = (len as usize).min(rx.len());
            for (i, slot) in rx[..len].iter_mut().enumerate() {
                *slot = flash.read_byte(decoded(addr.wrapping_add(i as u32)));
            }
        }
        ArrayOp::Program { addr, len } => {
            let len = (len as usize).min(tx.len());
            program_page(flash, decoded(addr), &tx[..len]);
        }
        ArrayOp::EraseSector(addr) => flash.erase_sector(decoded(addr)),
        ArrayOp::EraseBlock32(addr) => {
            let first = decoded(addr) & !(BLOCK32_LEN - 1);
            for page in 0..BLOCK32_LEN / PAGE_LEN {
                flash.erase_sector(first + page * PAGE_LEN);
            }
        }
        ArrayOp::EraseBlock(addr) => flash.erase_block(decoded(addr)),
        ArrayOp::EraseChip => flash.erase_chip(),
    }
    span(op)
}

/// The array address the part decodes from a command's address: the low 23 bits.
pub const fn decoded(addr: u32) -> u32 {
    addr & (FLASH_LEN - 1)
}

/// The flash pages `op` changes, or `None` for a read. The SPI1 host takes
/// `super::Wiring::FlashWritten` from this before the array half runs, so the page the cache
/// drops and the page the array changes cannot disagree.
pub fn span(op: ArrayOp) -> Option<Written> {
    match op {
        ArrayOp::Read { .. } => None,
        // A program cannot leave its PROGRAM_PAGE, which nests inside one PAGE_LEN page.
        ArrayOp::Program { addr, .. } => written(decoded(addr), 1),
        ArrayOp::EraseSector(addr) => written(decoded(addr) & !(PAGE_LEN - 1), PAGE_LEN),
        ArrayOp::EraseBlock32(addr) => written(decoded(addr) & !(BLOCK32_LEN - 1), BLOCK32_LEN),
        ArrayOp::EraseBlock(addr) => written(decoded(addr) & !(BLOCK_LEN - 1), BLOCK_LEN),
        ArrayOp::EraseChip => written(0, FLASH_LEN),
    }
}

/// Programs `data` at `addr` with the page-program wrap of [`PROGRAM_PAGE`]. `data` is at most
/// [`MAX_DATA`] bytes, so the tail never wraps twice.
fn program_page(flash: &mut FlashStore, addr: u32, data: &[u8]) {
    let page = addr & !(PROGRAM_PAGE - 1);
    let head = (PROGRAM_PAGE - (addr - page)) as usize;
    flash.program(addr, &data[..head.min(data.len())]);
    if let Some(tail) = data.get(head..) {
        flash.program(page, tail);
    }
}

/// The page range `[addr, addr + len)` covers, or `None` when it starts past the part (a guard:
/// [`span`] decodes first).
fn written(addr: u32, len: u32) -> Option<Written> {
    if addr >= FLASH_LEN {
        return None;
    }
    let end = addr.saturating_add(len).min(FLASH_LEN);
    let first_page = addr / PAGE_LEN;
    Some(Written {
        first_page,
        pages: end.div_ceil(PAGE_LEN) - first_page,
    })
}

/// Data-phase length as a byte count, clamped to the 64-byte host buffer.
fn data_len(len: usize) -> u8 {
    len.min(MAX_DATA) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip() -> (XmcChip, FlashStore) {
        (XmcChip::default(), FlashStore::erased())
    }

    /// Runs one transaction as the SPI1 host does: chip registers, then array work. `data` is the
    /// shared W0 to W15 buffer, read as data-out and overwritten by data-in.
    fn transact(
        chip: &mut XmcChip,
        flash: &mut FlashStore,
        opcode: u8,
        addr: u32,
        data: &mut [u8],
    ) -> Accepted {
        let tx = data.to_vec();
        let out = chip.accept(opcode, addr, &tx, data);
        if let Some(array) = out.array {
            apply(array, &tx, data, flash);
        }
        out
    }

    #[test]
    fn the_identity_is_the_device_value_and_never_changes() {
        // esp_flash_api.c reads the identity twice and refuses a mismatch; the bootloader folds
        // the three bytes into 0x204017.
        let (mut chip, mut flash) = chip();
        let mut first = [0u8; 3];
        transact(&mut chip, &mut flash, op::RDID, 0, &mut first);
        let mut second = [0u8; 3];
        transact(&mut chip, &mut flash, op::RDID, 0, &mut second);
        assert_eq!(first, [0x20, 0x40, 0x17]);
        assert_eq!(first, second);
        // The image header check compares against exactly this capacity.
        assert_eq!(DETECTED_SIZE, FLASH_LEN);
        assert_eq!(DETECTED_SIZE, 8 << 20);
        let mut long = [0u8; 6];
        transact(&mut chip, &mut flash, op::RDID, 0, &mut long);
        assert_eq!(long, [0x20, 0x40, 0x17, 0x20, 0x40, 0x17]);
    }

    #[test]
    fn status_starts_clear_so_the_bootloader_unlock_writes_nothing() {
        // bootloader_flash_unlock_default reads SR1 and SR2, and with both 0 issues no WRSR.
        let (mut chip, mut flash) = chip();
        let mut sr1 = [0xAAu8; 1];
        transact(&mut chip, &mut flash, op::RDSR, 0, &mut sr1);
        let mut sr2 = [0xAAu8; 1];
        transact(&mut chip, &mut flash, op::RDSR2, 0, &mut sr2);
        assert_eq!((sr1[0], sr2[0]), (0, 0));
        assert!(!chip.busy());
    }

    #[test]
    fn the_write_enable_latch_gates_every_program_and_erase() {
        // 0x06 sets WEL, 0x04 clears it, a program or erase without it is ignored, and
        // completion clears it.
        let (mut chip, mut flash) = chip();
        transact(&mut chip, &mut flash, op::PP, 0x1000, &mut [0x00]);
        assert_eq!(flash.read_byte(0x1000), ERASED, "no WEL, no program");
        assert!(!chip.busy());

        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        assert_eq!(chip.sr1(), SR1_WEL);
        transact(&mut chip, &mut flash, op::WRDI, 0, &mut []);
        assert_eq!(chip.sr1(), 0);

        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        let out = transact(&mut chip, &mut flash, op::PP, 0x1000, &mut [0x5A]);
        assert_eq!(flash.read_byte(0x1000), 0x5A);
        assert_eq!(out.busy_ps, 0, "the fast profile completes at now");
        assert_eq!(chip.sr1(), SR1_WIP | SR1_WEL, "busy until the completion");
        chip.complete();
        assert_eq!(chip.sr1(), 0, "WIP and WEL both clear at completion");
    }

    #[test]
    fn every_erase_kind_clears_its_own_span_and_nothing_above_it() {
        let (mut chip, mut flash) = chip();
        let fill = |flash: &mut FlashStore| {
            for page in 0..32u32 {
                flash.program(page * PAGE_LEN, &[0x00]);
            }
        };
        let enable = |chip: &mut XmcChip, flash: &mut FlashStore| {
            transact(chip, flash, op::WREN, 0, &mut []);
        };

        fill(&mut flash);
        enable(&mut chip, &mut flash);
        let out = transact(&mut chip, &mut flash, op::SE, 0x123, &mut []);
        assert_eq!(flash.read_byte(0), ERASED);
        assert_eq!(flash.read_byte(PAGE_LEN), 0x00, "the next sector is kept");
        assert_eq!(out.array, Some(ArrayOp::EraseSector(0x123)));
        chip.complete();

        fill(&mut flash);
        enable(&mut chip, &mut flash);
        transact(&mut chip, &mut flash, op::BE32K, 0x7FFF, &mut []);
        assert_eq!(flash.read_byte(0x7000), ERASED);
        assert_eq!(flash.read_byte(0x8000), 0x00, "32 KB, not 64 KB");
        chip.complete();

        fill(&mut flash);
        enable(&mut chip, &mut flash);
        transact(&mut chip, &mut flash, op::BE, 0xFFFF, &mut []);
        assert_eq!(flash.read_byte(0xF000), ERASED);
        assert_eq!(flash.read_byte(BLOCK_LEN), 0x00, "the next block is kept");
        chip.complete();

        fill(&mut flash);
        enable(&mut chip, &mut flash);
        transact(&mut chip, &mut flash, op::CE_60, 0, &mut []);
        assert_eq!(flash.read_byte(BLOCK_LEN), ERASED);
        assert!(flash.delta().is_empty(), "an erased chip equals the image");
        chip.complete();

        fill(&mut flash);
        enable(&mut chip, &mut flash);
        let out = transact(&mut chip, &mut flash, op::CE_C7, 0, &mut []);
        assert_eq!(out.array, Some(ArrayOp::EraseChip));
        assert_eq!(flash.read_byte(0), ERASED);
    }

    #[test]
    fn wip_lasts_the_profile_duration_of_each_operation() {
        let mut chip = XmcChip::new(FlashTiming::DEVICE);
        let mut flash = FlashStore::erased();
        let run = |chip: &mut XmcChip, flash: &mut FlashStore, opcode: u8| -> u64 {
            transact(chip, flash, op::WREN, 0, &mut []);
            let out = transact(chip, flash, opcode, 0, &mut [0x00]);
            assert!(chip.busy(), "{opcode:#04X} must set WIP");
            chip.complete();
            assert!(!chip.busy());
            out.busy_ps
        };
        assert_eq!(run(&mut chip, &mut flash, op::PP), 700_000_000);
        assert_eq!(run(&mut chip, &mut flash, op::SE), 45_000_000_000);
        assert_eq!(run(&mut chip, &mut flash, op::BE), 150_000_000_000);
        assert_eq!(run(&mut chip, &mut flash, op::CE_60), 20_000_000_000_000);
        assert_eq!(run(&mut chip, &mut flash, op::WRSR), 700_000_000);

        chip.set_timing(FlashTiming::FAST);
        assert_eq!(run(&mut chip, &mut flash, op::SE), 0);
        assert_eq!(chip.timing(), FlashTiming::FAST);
    }

    #[test]
    fn a_status_write_needs_the_latch_and_keeps_the_hardware_bits() {
        // WRSR takes one or two bytes and needs WEL; the driver keeps only QE of SR2.
        let (mut chip, mut flash) = chip();
        transact(&mut chip, &mut flash, op::WRSR, 0, &mut [0x3C]);
        assert_eq!(chip.sr1(), 0, "no WEL, no status write");

        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::WRSR, 0, &mut [0x3C, 0x02]);
        assert_eq!(chip.sr1(), 0x3C | SR1_WIP | SR1_WEL);
        assert_eq!(chip.sr2(), 0x02, "the second byte is SR2");
        chip.complete();
        assert_eq!(chip.sr1(), 0x3C, "the written bits stay, WIP and WEL go");

        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::WRSR, 0, &mut [0xFF]);
        chip.complete();
        assert_eq!(chip.sr1(), 0xFC);

        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::WRSR2, 0, &mut [0x00]);
        chip.complete();
        assert_eq!(chip.sr2(), 0x00);
    }

    #[test]
    fn reads_stream_the_array_in_every_read_mode() {
        // The read spellings differ only in line count, which this model does not represent.
        let (mut chip, mut flash) = chip();
        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(
            &mut chip,
            &mut flash,
            op::PP,
            0x2000,
            &mut [0x11, 0x22, 0x33],
        );
        chip.complete();
        for opcode in [
            op::READ,
            op::FAST_READ,
            op::DOR,
            op::QOR,
            op::DIOR,
            op::QIOR,
        ] {
            let mut rx = [0u8; 4];
            transact(&mut chip, &mut flash, opcode, 0x2000, &mut rx);
            assert_eq!(rx, [0x11, 0x22, 0x33, ERASED], "{opcode:#04X}");
        }
    }

    #[test]
    fn programming_clears_bits_only_and_the_written_pages_are_reported() {
        // A program ands into the array, so a bit can only go back to 1 by erasing.
        let (mut chip, mut flash) = chip();
        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        let tx = [0xF0u8, 0x0F];
        let mut rx = [0u8; 0];
        let out = chip.accept(op::PP, 0x3800, &tx, &mut rx);
        assert_eq!(
            out.array,
            Some(ArrayOp::Program {
                addr: 0x3800,
                len: 2
            })
        );
        let written = apply(out.array.expect("a program"), &tx, &mut rx, &mut flash);
        assert_eq!(
            written,
            Some(Written {
                first_page: 3,
                pages: 1
            }),
            "a program stays inside its program page, so inside one 4 KB page"
        );
        assert_eq!(flash.read_byte(0x3800), 0xF0);
        assert_eq!(flash.read_byte(0x3801), 0x0F);
        chip.complete();

        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::PP, 0x3800, &mut [0xFF]);
        assert_eq!(flash.read_byte(0x3800), 0xF0, "a program cannot set a bit");
    }

    #[test]
    fn a_page_program_wraps_inside_its_program_page() {
        let (mut chip, mut flash) = chip();
        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        let tx: Vec<u8> = (1..=8).collect();
        let mut rx = [0u8; 0];
        let out = chip.accept(op::PP, 0xFC, &tx, &mut rx);
        let written = apply(out.array.expect("a program"), &tx, &mut rx, &mut flash);

        assert_eq!(
            (0xFC..0x104)
                .map(|a| flash.read_byte(a))
                .collect::<Vec<u8>>(),
            vec![1, 2, 3, 4, ERASED, ERASED, ERASED, ERASED]
        );
        assert_eq!(
            (0x00..0x04)
                .map(|a| flash.read_byte(a))
                .collect::<Vec<u8>>(),
            vec![5, 6, 7, 8]
        );
        assert_eq!(
            written,
            Some(Written {
                first_page: 0,
                pages: 1
            }),
            "a wrapped program cannot reach a second page"
        );

        chip.complete();
        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::PP, 0x41FF, &mut [0xAA, 0xBB]);
        assert_eq!(flash.read_byte(0x41FF), 0xAA);
        assert_eq!(
            flash.read_byte(0x4100),
            0xBB,
            "wrapped to 0x4100, not 0x4200"
        );
        assert_eq!(flash.read_byte(0x4200), ERASED);
    }

    /// The `probe_campaign_regs` capture: 0x800000 reads what 0x000000 holds; a program or erase
    /// there lands on the same mirror (UNVERIFIED).
    #[test]
    fn an_address_beyond_the_part_reaches_the_cell_8_mb_below_it() {
        let (mut chip, mut flash) = chip();
        assert_eq!(FLASH_LEN, 0x80_0000);
        assert_eq!(decoded(FLASH_LEN), 0);
        assert_eq!(decoded(FLASH_LEN + 0x1234), 0x1234);
        flash.program(0x10, &[0x12, 0x34, 0x56, 0x78]);
        flash.program(FLASH_LEN - 2, &[0xAB, 0xCD]);

        let mut rx = [0x00u8; 4];
        transact(&mut chip, &mut flash, op::READ, FLASH_LEN + 0x10, &mut rx);
        assert_eq!(rx, [0x12, 0x34, 0x56, 0x78], "0x800010 reads 0x000010");
        let mut rx = [0x00u8; 4];
        transact(&mut chip, &mut flash, op::READ, FLASH_LEN - 2, &mut rx);
        assert_eq!(
            rx,
            [0xAB, 0xCD, ERASED, ERASED],
            "a read across the end of the part continues at its start"
        );

        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::PP, FLASH_LEN + 0x20, &mut [0x00]);
        chip.complete();
        assert_eq!(
            flash.read_byte(0x20),
            0x00,
            "the program landed on the mirror"
        );

        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::SE, FLASH_LEN, &mut []);
        chip.complete();
        assert_eq!(
            flash.read_byte(0x10),
            ERASED,
            "the erase landed on sector 0"
        );
        assert_eq!(
            flash.read_byte(FLASH_LEN - 2),
            0xAB,
            "the last sector is untouched"
        );

        assert_eq!(
            span(ArrayOp::EraseSector(FLASH_LEN + 0x1000)),
            Some(Written {
                first_page: 1,
                pages: 1
            }),
            "the written page is the mirrored one, never one past the part"
        );
        assert_eq!(
            span(ArrayOp::Program {
                addr: FLASH_LEN + 0x7F_F000,
                len: 1
            }),
            Some(Written {
                first_page: 2047,
                pages: 1
            })
        );
        assert_eq!(written(FLASH_LEN, PAGE_LEN), None, "the guard of `written`");
    }

    #[test]
    fn deep_power_down_answers_only_the_release_command() {
        // The ROM SPI_WakeUp path issues 0xAB at attach.
        let (mut chip, mut flash) = chip();
        transact(&mut chip, &mut flash, op::DP, 0, &mut []);
        assert!(chip.powered_down());
        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        assert_eq!(chip.sr1(), 0, "no command is answered while powered down");
        let mut rx = [0u8; 1];
        transact(&mut chip, &mut flash, op::RES, 0, &mut rx);
        assert!(!chip.powered_down());
        assert_eq!(rx, [0x16]);
        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        assert_eq!(chip.sr1(), SR1_WEL);
    }

    #[test]
    fn the_reset_pair_clears_the_latches_and_keeps_the_array() {
        // Only the pair resets, and the array survives, which makes the flash image durable.
        let (mut chip, mut flash) = chip();
        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::PP, 0x10, &mut [0x42]);
        assert!(chip.busy());

        transact(&mut chip, &mut flash, op::RST, 0, &mut []);
        assert!(chip.busy(), "0x99 alone does not reset");

        transact(&mut chip, &mut flash, op::RSTEN, 0, &mut []);
        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::RST, 0, &mut []);
        assert!(chip.busy(), "the latch is consumed by the command between");

        transact(&mut chip, &mut flash, op::RSTEN, 0, &mut []);
        transact(&mut chip, &mut flash, op::RST, 0, &mut []);
        assert_eq!(chip.sr1(), 0);
        assert_eq!(chip.sr2() & SR2_SUS, 0);
        assert_eq!(flash.read_byte(0x10), 0x42, "the array survives the reset");
    }

    /// Only the command that sets `WIP` schedules a completion, so a second program accepted
    /// during it would end with the first one's window.
    #[test]
    fn a_busy_part_answers_only_the_status_reads_and_the_reset_pair() {
        let (mut chip, mut flash) = chip();
        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::PP, 0x1000, &mut [0x00]);
        assert!(chip.busy());
        assert_eq!(chip.sr1() & SR1_WEL, SR1_WEL, "WEL is still set");

        let out = chip.accept(op::PP, 0x2000, &[0x00], &mut []);
        assert_eq!(out, Accepted::default());
        assert_eq!(flash.read_byte(0x2000), ERASED);
        for opcode in [op::SE, op::BE, op::CE_60, op::WRSR, op::READ, op::RDID] {
            let mut rx = [0x11u8; 4];
            assert_eq!(
                chip.accept(opcode, 0, &[0x00], &mut rx),
                Accepted::default()
            );
            assert_eq!(rx, [ERASED; 4], "{opcode:#04X} drives no data");
        }

        let mut rx = [0u8; 1];
        transact(&mut chip, &mut flash, op::RDSR, 0, &mut rx);
        assert_eq!(rx[0], SR1_WIP | SR1_WEL);
        transact(&mut chip, &mut flash, op::RDSR2, 0, &mut rx);
        assert_eq!(rx[0], 0);

        // The pair still aborts the operation (`bootloader_flash_reset_chip`).
        transact(&mut chip, &mut flash, op::RSTEN, 0, &mut []);
        transact(&mut chip, &mut flash, op::RST, 0, &mut []);
        assert!(!chip.busy());
        assert_eq!(chip.sr1(), 0);

        transact(&mut chip, &mut flash, op::WREN, 0, &mut []);
        transact(&mut chip, &mut flash, op::PP, 0x2000, &mut [0x00]);
        assert_eq!(flash.read_byte(0x2000), 0x00);
    }

    #[test]
    fn sfdp_and_the_unique_id_are_declared_approximations() {
        let (mut chip, mut flash) = chip();
        let mut rx = [0x00u8; 8];
        transact(&mut chip, &mut flash, op::RDSFDP, 0, &mut rx);
        assert_eq!(rx, [ERASED; 8]);
        // The unique id is synthesized, never a device value (docs/secrets.md).
        let mut rx = [0xAAu8; 12];
        transact(&mut chip, &mut flash, op::RDUID, 0, &mut rx);
        assert_eq!(rx[..4], [ERASED; 4], "the four dummy bytes");
        assert_eq!(rx[4..], [0; 8]);
    }

    #[test]
    fn an_opcode_the_part_does_not_answer_is_reported_rather_than_guessed() {
        let (mut chip, mut flash) = chip();
        let mut rx = [0x00u8; 2];
        let out = transact(&mut chip, &mut flash, 0x1D, 0, &mut rx);
        assert!(out.unknown);
        assert_eq!(out.array, None);
        assert_eq!(rx, [ERASED; 2]);
        // Unconfigured features answer a deliberate no-op, not "unknown".
        for opcode in [op::HPM, op::PES, op::PER] {
            let out = transact(&mut chip, &mut flash, opcode, 0, &mut []);
            assert!(!out.unknown, "{opcode:#04X}");
            assert_eq!(out.busy_ps, 0);
        }
    }
}
