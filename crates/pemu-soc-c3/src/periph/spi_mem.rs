//! The two SPI_MEM instances (`specs/blocks/spi0.toml`, `specs/blocks/spi1.toml`): SPI1, the flash
//! command host, and SPI0, the cache host. They share one layout and one model; only SPI1 carries
//! the transaction engine and the [`XmcChip`], because cache reads resolve through the MMU
//! straight to the flash store, never through SPI0.
//!
//! Line timing is not modeled. A `SPI_MEM_CMD` write that sets a trigger bit runs the whole
//! transaction and the bit reads back 0 inside the same access, the completion predicate of every
//! ROM and IDF wait loop. The transaction is decoded from the `USR` phase registers or from a
//! legacy command bit, which implies the opcode.
//!
//! The array half needs [`FlashStore`], which lives on `Soc`, and `Cx` carries no flash view. So
//! the half is latched and `SocBus::store_slow` calls [`SpiMem::service`] inside the same guest
//! write: a read has filled `W` before the guest polls `CMD` again, and a program has reached the
//! store before the write's `Wiring` is handled.
//!
//! A program or erase returns `Wiring::FlashWritten` with its first page; [`SpiMem::service`]
//! returns the full range, since a 64 KB erase changes sixteen pages. A write that latched nothing
//! leaves [`SpiMem::is_pending`] false and the bus skips the call.

use std::marker::PhantomData;

use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::regstore::{RegSpec, RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::de::Error as _;
use pemu_core::serde::{Deserialize, Deserializer, Serialize, Serializer};
use pemu_core::time::VTime;

use super::flash_xmc::{self, ArrayOp, MAX_DATA, Written, XmcChip, op};
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};
use crate::flash_store::{ERASED, FlashStore};
use crate::r#gen::{regs_spi0, regs_spi1};

/// Registers of one SPI_MEM instance; both generated tables hold the same 47.
pub const REG_COUNT: usize = regs_spi1::REG_COUNT;

use regs_spi1::idx;

pub const TAG_WIP: u16 = 0;

/// Event tag of the end of a clocked command's bus time.
pub const TAG_CMD: u16 = 1;

/// Picoseconds per SPI_MEM core clock cycle: 80 MHz through `CLK_EQU_SYSCLK`. Class C during the
/// XTAL-clocked ROM phase, where the source is not re-derived.
const SPI_MEM_SRC_PS: u64 = 12_500;

/// `SPI_MEM_CMD` trigger bits.
pub mod cmd {
    /// Legacy read of `ADDR[23:0]`.
    pub const FLASH_READ: u32 = 1 << 31;
    pub const FLASH_WREN: u32 = 1 << 30;
    pub const FLASH_WRDI: u32 = 1 << 29;
    pub const FLASH_RDID: u32 = 1 << 28;
    pub const FLASH_RDSR: u32 = 1 << 27;
    pub const FLASH_WRSR: u32 = 1 << 26;
    pub const FLASH_PP: u32 = 1 << 25;
    pub const FLASH_SE: u32 = 1 << 24;
    pub const FLASH_BE: u32 = 1 << 23;
    pub const FLASH_CE: u32 = 1 << 22;
    /// Deep power-down.
    pub const FLASH_DP: u32 = 1 << 21;
    /// Release from deep power-down.
    pub const FLASH_RES: u32 = 1 << 20;
    /// High performance mode.
    pub const FLASH_HPM: u32 = 1 << 19;
    pub const USR: u32 = 1 << 18;
    /// Program / erase marker for the auto-suspend logic.
    pub const FLASH_PE: u32 = 1 << 17;
}

/// `SPI_MEM_USER` phase enables.
mod user {
    pub const USR_DUMMY: u32 = 1 << 29;
    pub const FWRITE_QIO: u32 = 1 << 15;
    pub const FWRITE_DIO: u32 = 1 << 14;
    pub const FWRITE_QUAD: u32 = 1 << 13;
    pub const FWRITE_DUAL: u32 = 1 << 12;
    pub const USR_MOSI: u32 = 1 << 27;
    pub const USR_MISO: u32 = 1 << 28;
    pub const USR_ADDR: u32 = 1 << 30;
    pub const USR_COMMAND: u32 = 1 << 31;
}

/// `SPI_MEM_CTRL.WRSR_2B`: a legacy status access moves two bytes instead of one.
const CTRL_WRSR_2B: u32 = 1 << 22;

/// `SPI_MEM_CTRL` line modes: quad and dual command, quad and dual address with data (`QIO`,
/// `DIO`), and quad and dual data alone.
mod lines {
    pub const FREAD_QIO: u32 = 1 << 24;
    pub const FREAD_DIO: u32 = 1 << 23;
    pub const FREAD_QUAD: u32 = 1 << 20;
    pub const FREAD_DUAL: u32 = 1 << 14;
    pub const FCMD_QUAD: u32 = 1 << 8;
    pub const FCMD_DUAL: u32 = 1 << 7;
}

pub trait SpiMemBlock: Block {
    const SPECS: &'static [RegSpec; REG_COUNT];
    /// True for SPI1, the flash command host; false for SPI0, which needs no engine.
    const FLASH_HOST: bool;
}

impl SpiMemBlock for super::block::Spi0 {
    const SPECS: &'static [RegSpec; REG_COUNT] = &regs_spi0::REGS;
    const FLASH_HOST: bool = false;
}

impl SpiMemBlock for super::block::Spi1 {
    const SPECS: &'static [RegSpec; REG_COUNT] = &regs_spi1::REGS;
    const FLASH_HOST: bool = true;
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub enum Port {
    Buffer,
    /// `RD_STATUS[15:0]`, which legacy `RDSR` fills and legacy `WRSR` reads.
    Status,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Txn {
    opcode: u8,
    addr: u32,
    tx_len: usize,
    rx_len: usize,
    src: Port,
    dest: Port,
    /// Second command of the two-byte legacy status read: with `CTRL.WRSR_2B`, `RD_STATUS[15:0]`
    /// is `SR1 | SR2 << 8`, answered as `0x05` then `0x35` (the packing is UNVERIFIED).
    pair: Option<u8>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct Latched {
    op: ArrayOp,
    tx: Vec<u8>,
    rx_len: usize,
    dest: Port,
}

pub struct SpiMem<B> {
    regs: RegStore<REG_COUNT>,
    /// One bit per 4-byte slot of the window: already reported to the ledger.
    touched: Vec<u64>,
    /// The part behind the command host. SPI0 has none and never uses this one.
    chip: XmcChip,
    latched: Option<Latched>,
    block: PhantomData<fn() -> B>,
}

impl<B: SpiMemBlock> Default for SpiMem<B> {
    fn default() -> SpiMem<B> {
        let mut m = SpiMem {
            regs: RegStore::new(B::SPECS),
            touched: vec![0; (B::SIZE.div_ceil(4) as usize).div_ceil(64)],
            chip: XmcChip::default(),
            latched: None,
            block: PhantomData,
        };
        m.settle_unstored();
        m
    }
}

/// The register bits the device does not store, per instance, as `(register, bits that read 0,
/// bits that read 1)`, from the `probe_campaign_regs` capture. The shared layout names these
/// fields on both instances; silicon implements them on one only.
///
/// SPI0: `USER` bits 31, 30, 28 (the ROM writes 0xF00000C0, the device reads 0x200000C0);
/// `MOSI_DLEN` and `MISO_DLEN` (0xFF written, 0 read); `MISC` bit 1 reads 0, and bits 3 and 5,
/// which hardware sets at the end of a cache transfer, read 1. The model has no cache transfer,
/// so it holds them set from reset (UNVERIFIED before the first transfer).
///
/// SPI1: `CTRL2` bits 9:0 (`CS_HOLD_TIME`, `CS_SETUP_TIME`) read 0 whatever the ROM or IDF
/// writes.
const fn unstored(flash_host: bool) -> &'static [(usize, u32, u32)] {
    if flash_host {
        &[(idx::SPI_MEM_CTRL2, 0x3FF, 0)]
    } else {
        &[
            (idx::SPI_MEM_USER, 1 << 31 | 1 << 30 | 1 << 28, 0),
            (idx::SPI_MEM_MOSI_DLEN, u32::MAX, 0),
            (idx::SPI_MEM_MISO_DLEN, u32::MAX, 0),
            (idx::SPI_MEM_MISC, 1 << 1, 1 << 3 | 1 << 5),
        ]
    }
}

impl<B: SpiMemBlock> SpiMem<B> {
    pub fn chip(&self) -> &XmcChip {
        &self.chip
    }

    pub fn chip_mut(&mut self) -> &mut XmcChip {
        &mut self.chip
    }

    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    pub fn is_pending(&self) -> bool {
        self.latched.is_some()
    }

    /// Performs the array half of the latched transaction against `flash` and returns the pages a
    /// change touched, so the caller can invalidate the cache. Nothing latched does nothing.
    pub fn service(&mut self, flash: &mut FlashStore) -> Option<Written> {
        let latched = self.latched.take()?;
        let mut rx = vec![ERASED; latched.rx_len];
        let written = flash_xmc::apply(latched.op, &latched.tx, &mut rx, flash);
        self.put(latched.dest, &rx);
        written
    }

    /// Removes the flash bytes this host carries for a range `secret(start, end)` says an export
    /// erases, and returns whether anything changed. A latched program into such a range gets
    /// 0xFF data. The `W` buffer becomes 0xFF whenever any byte is neither 0xFF nor 0x00, because
    /// no register says where its bytes came from (bytes above the last length are an earlier
    /// transaction's). RAM copies are `pemu-api` `redact`'s job.
    pub fn redact_flash(&mut self, secret: &dyn Fn(u32, u32) -> bool) -> bool {
        let span = |addr: u32, len: usize| {
            let len = u32::try_from(len.max(1)).unwrap_or(u32::MAX);
            secret(addr, addr.saturating_add(len))
        };
        let mut changed = false;
        if let Some(latched) = &mut self.latched
            && let ArrayOp::Program { addr, len } = latched.op
            && span(addr, usize::from(len))
            && latched.tx.iter().any(|&b| b != ERASED)
        {
            latched.tx.fill(ERASED);
            changed = true;
        }
        if B::FLASH_HOST && self.buffer(MAX_DATA).iter().any(|&b| b != ERASED && b != 0) {
            self.put_buffer(&[ERASED; MAX_DATA]);
            changed = true;
        }
        changed
    }

    fn index_of(&self, off: u32) -> Option<usize> {
        u16::try_from(off & !3)
            .ok()
            .and_then(|off| self.regs.index_of(off))
    }

    /// Reports the first touch of the register holding `off`, including an offset the table does
    /// not name: a guest reaching it is what the ledger is for.
    fn touch(
        &mut self,
        off: u32,
        access: TouchAccess,
        size: Size,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        let slot = (off / 4) as usize;
        let Some(word) = self.touched.get_mut(slot / 64) else {
            return;
        };
        let bit = 1u64 << (slot % 64);
        if *word & bit != 0 {
            return;
        }
        *word |= bit;
        ledger.first_touch(FirstTouch {
            periph: B::ID,
            off: off & !3,
            access,
            size: size as u8,
            now,
            allowlisted: false,
        });
    }

    /// Value of `size` bytes at `off`, with read side effects; reports the first touch.
    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.touch(off, TouchAccess::Read, size, now, ledger);
        match self.index_of(off) {
            Some(i) => self.regs.read(i, (off % 4) as u8, size),
            None => 0,
        }
    }

    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
        sched: &mut Scheduler,
    ) -> Started {
        self.store_timed(off, size, val, now, ledger, sched, false)
    }

    /// [`SpiMem::store`] under a timing profile: with `clocked` (`spi1_clocked`), a command keeps
    /// its trigger bit set for its bus time ([`SpiMem::bus_ps`]) and the [`TAG_CMD`] event clears
    /// it. The transaction's effects happen at the write either way.
    #[allow(clippy::too_many_arguments)]
    pub fn store_timed(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
        sched: &mut Scheduler,
        clocked: bool,
    ) -> Started {
        self.touch(off, TouchAccess::Write, size, now, ledger);
        let Some(i) = self.index_of(off) else {
            return Started::default();
        };
        let delta = self.regs.write(i, (off % 4) as u8, size, val);
        self.settle_unstored();
        if delta.triggers == 0 {
            return Started::default();
        }
        // Every SC bit of this block self-clears inside the access. `RegStore::clear_sc` masks by
        // the register's SC fields, so this reaches `SPI_MEM_CMD` (whose `MST_ST` and `SLV_ST`
        // read 0 with it) and the two auto-suspend registers, which the device clears when the
        // suspend completes.
        let hold = clocked && i == idx::SPI_MEM_CMD && B::FLASH_HOST;
        if !hold {
            self.regs.clear_sc(i, delta.triggers);
        }
        if i != idx::SPI_MEM_CMD {
            // Only a command starts a transaction. No auto-suspend engine runs, which keeps the
            // suspend registers class C.
            return Started::default();
        }
        if !B::FLASH_HOST {
            // SPI0 needs no transaction engine: cache fills resolve through the MMU.
            return Started::default();
        }
        if !hold {
            return self.start(delta.triggers, now, sched);
        }
        let Some(txn) = self.decode(delta.triggers) else {
            self.regs.clear_sc(i, delta.triggers);
            return Started::default();
        };
        let busy = self.bus_ps(delta.triggers, &txn);
        let mut started = self.start(delta.triggers, now, sched);
        sched.schedule(
            now,
            VTime(now.0.saturating_add(busy)),
            EventKey {
                owner: Owner::Periph(B::ID),
                tag: TAG_CMD,
            },
        );
        started.stop = true;
        started
    }

    /// Picoseconds a transaction occupies the bus: command, address, dummy and data phases over
    /// the lines `CTRL` and `USER` select, at the clock `SPI_MEM_CLOCK` divides from 80 MHz.
    /// Chip-select setup and hold are left out (class C). A legacy command sends 8 command bits, a
    /// 24-bit address when it has one, and its data on one line.
    pub fn bus_ps(&self, triggers: u32, txn: &Txn) -> u64 {
        let ctrl = self.regs.get(idx::SPI_MEM_CTRL);
        let user = self.regs.get(idx::SPI_MEM_USER);
        let usr = triggers & cmd::USR != 0;
        let width = |quad: bool, dual: bool| -> u64 {
            if quad {
                4
            } else if dual {
                2
            } else {
                1
            }
        };
        let cmd_lines = width(ctrl & lines::FCMD_QUAD != 0, ctrl & lines::FCMD_DUAL != 0);
        let addr_lines = width(
            ctrl & lines::FREAD_QIO != 0 || (txn.tx_len > 0 && user & user::FWRITE_QIO != 0),
            ctrl & lines::FREAD_DIO != 0 || (txn.tx_len > 0 && user & user::FWRITE_DIO != 0),
        );
        let read_lines = width(
            ctrl & (lines::FREAD_QIO | lines::FREAD_QUAD) != 0,
            ctrl & (lines::FREAD_DIO | lines::FREAD_DUAL) != 0,
        );
        let write_lines = width(
            user & (user::FWRITE_QIO | user::FWRITE_QUAD) != 0,
            user & (user::FWRITE_DIO | user::FWRITE_DUAL) != 0,
        );
        let (cmd_bits, addr_bits, dummy) = if usr {
            let user1 = self.regs.get(idx::SPI_MEM_USER1);
            let cmd_bits = u64::from((self.regs.get(idx::SPI_MEM_USER2) >> 28) + 1);
            let addr_bits = if user & user::USR_ADDR != 0 {
                u64::from(((user1 >> 26) & 0x3F) + 1)
            } else {
                0
            };
            let dummy = if user & user::USR_DUMMY != 0 {
                u64::from((user1 & 0x3F) + 1)
            } else {
                0
            };
            (cmd_bits, addr_bits, dummy)
        } else {
            let addressed = matches!(txn.opcode, op::READ | op::PP | op::SE | op::BE);
            (8, if addressed { 24 } else { 0 }, 0)
        };
        let (read_lines, write_lines) = if usr {
            (read_lines, write_lines)
        } else {
            (1, 1)
        };
        let cycles = cmd_bits.div_ceil(cmd_lines)
            + addr_bits.div_ceil(if usr { addr_lines } else { 1 })
            + dummy
            + (txn.rx_len as u64 * 8).div_ceil(read_lines)
            + (txn.tx_len as u64 * 8).div_ceil(write_lines);
        let clock = self.regs.get(idx::SPI_MEM_CLOCK);
        let divide = if clock >> 31 & 1 == 1 {
            1
        } else {
            u64::from((clock >> 16 & 0xFF) + 1)
        };
        cycles.saturating_mul(SPI_MEM_SRC_PS * divide)
    }

    pub fn complete(&mut self) {
        self.chip.complete();
    }

    /// Restores every register the reset reaches and drops the latched transaction. The part is
    /// outside the SoC and no MCU reset reaches it, so [`XmcChip`] is left alone.
    pub fn apply_reset(&mut self, kind: ResetKind) {
        for i in 0..REG_COUNT {
            let spec = self.regs.spec(i);
            if kind.clears(spec.domain) {
                self.regs.set(i, spec.reset);
            }
        }
        // Every row of both tables carries `DOMAIN_CHIP_SYSTEM_CORE`, so one scope check covers
        // the block, as in `Sha`, `Rsa` and `Aes`.
        if kind.clears(pemu_core::regstore::RESET_BY_ALL_SCOPES) {
            self.latched = None;
        }
        self.settle_unstored();
    }

    fn settle_unstored(&mut self) {
        for &(i, zero, one) in unstored(B::FLASH_HOST) {
            let v = self.regs.get(i);
            self.regs.set(i, (v & !zero) | one);
        }
    }

    fn buffer(&self, len: usize) -> Vec<u8> {
        (0..len.min(MAX_DATA))
            .map(|i| (self.regs.get(idx::SPI_MEM_W0 + i / 4) >> (8 * (i % 4))) as u8)
            .collect()
    }

    fn put_buffer(&mut self, bytes: &[u8]) {
        for (i, byte) in bytes.iter().take(MAX_DATA).enumerate() {
            let reg = idx::SPI_MEM_W0 + i / 4;
            let shift = 8 * (i % 4);
            let kept = self.regs.get(reg) & !(0xFF << shift);
            self.regs.set(reg, kept | (u32::from(*byte) << shift));
        }
    }

    /// Writes `bytes` into `RD_STATUS[15:0]`, where a legacy `RDSR` leaves the status the ROM
    /// masks.
    fn put_status(&mut self, bytes: &[u8]) {
        let mut status = 0u32;
        for (i, byte) in bytes.iter().take(2).enumerate() {
            status |= u32::from(*byte) << (8 * i);
        }
        let kept = self.regs.get(idx::SPI_MEM_RD_STATUS) & !0xFFFF;
        self.regs.set(idx::SPI_MEM_RD_STATUS, kept | status);
    }

    fn take(&self, port: Port, len: usize) -> Vec<u8> {
        match port {
            Port::Buffer => self.buffer(len),
            Port::Status => {
                let status = self.regs.get(idx::SPI_MEM_RD_STATUS);
                (0..len.min(2)).map(|i| (status >> (8 * i)) as u8).collect()
            }
        }
    }

    fn put(&mut self, port: Port, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match port {
            Port::Buffer => self.put_buffer(bytes),
            Port::Status => self.put_status(bytes),
        }
    }

    /// Decodes the transaction the trigger bits of a `CMD` write name. `USR` wins over a legacy
    /// bit (`spimem_flash_ll_user_start` may set `FLASH_PE` beside it); among legacy bits the
    /// highest wins.
    fn decode(&self, triggers: u32) -> Option<Txn> {
        if triggers & cmd::USR != 0 {
            return self.decode_usr();
        }
        let addr = self.regs.get(idx::SPI_MEM_ADDR);
        let status_len = if self.regs.get(idx::SPI_MEM_CTRL) & CTRL_WRSR_2B != 0 {
            2
        } else {
            1
        };
        let plain = |opcode: u8| Txn {
            opcode,
            addr: 0,
            tx_len: 0,
            rx_len: 0,
            src: Port::Buffer,
            dest: Port::Buffer,
            pair: None,
        };
        let addressed = |opcode: u8| Txn {
            addr: addr & 0xFF_FFFF,
            ..plain(opcode)
        };
        Some(match triggers {
            // No ROM or IDF path uses the legacy read, so its length encoding is taken from the
            // page program's (class C).
            t if t & cmd::FLASH_READ != 0 => Txn {
                rx_len: (addr >> 24) as usize,
                ..addressed(op::READ)
            },
            t if t & cmd::FLASH_WREN != 0 => plain(op::WREN),
            t if t & cmd::FLASH_WRDI != 0 => plain(op::WRDI),
            t if t & cmd::FLASH_RDID != 0 => Txn {
                rx_len: 3,
                ..plain(op::RDID)
            },
            t if t & cmd::FLASH_RDSR != 0 => Txn {
                rx_len: status_len,
                dest: Port::Status,
                pair: (status_len == 2).then_some(op::RDSR2),
                ..plain(op::RDSR)
            },
            t if t & cmd::FLASH_WRSR != 0 => Txn {
                tx_len: status_len,
                src: Port::Status,
                ..plain(op::WRSR)
            },
            t if t & cmd::FLASH_PP != 0 => Txn {
                tx_len: ((addr >> 24) as usize).min(MAX_DATA),
                ..addressed(op::PP)
            },
            t if t & cmd::FLASH_SE != 0 => addressed(op::SE),
            t if t & cmd::FLASH_BE != 0 => addressed(op::BE),
            t if t & cmd::FLASH_CE != 0 => plain(op::CE_60),
            t if t & cmd::FLASH_DP != 0 => plain(op::DP),
            t if t & cmd::FLASH_RES != 0 => plain(op::RES),
            t if t & cmd::FLASH_HPM != 0 => plain(op::HPM),
            // FLASH_PE alone is an auto-suspend marker, disabled in this build.
            _ => return None,
        })
    }

    fn decode_usr(&self) -> Option<Txn> {
        let user = self.regs.get(idx::SPI_MEM_USER);
        if user & user::USR_COMMAND == 0 {
            // Every ROM and IDF flash transaction sends a command byte.
            return None;
        }
        // The command byte is the low byte of USR_COMMAND_VALUE; the generic chip driver never
        // uses the 16-bit form.
        let opcode = self.regs.get(idx::SPI_MEM_USER2) as u8;
        let addr = if user & user::USR_ADDR != 0 {
            // LSB-aligned, USR_ADDR_BITLEN + 1 bits wide: 24 in every ROM and IDF path.
            let bits = ((self.regs.get(idx::SPI_MEM_USER1) >> 26) & 0x3F) + 1;
            let value = self.regs.get(idx::SPI_MEM_ADDR);
            if bits >= 32 {
                value
            } else {
                value & ((1 << bits) - 1)
            }
        } else {
            0
        };
        let bytes = |reg: usize| (self.regs.get(reg) & 0x3FF) as usize / 8 + 1;
        Some(Txn {
            opcode,
            addr,
            tx_len: if user & user::USR_MOSI != 0 {
                bytes(idx::SPI_MEM_MOSI_DLEN).min(MAX_DATA)
            } else {
                0
            },
            rx_len: if user & user::USR_MISO != 0 {
                bytes(idx::SPI_MEM_MISO_DLEN).min(MAX_DATA)
            } else {
                0
            },
            src: Port::Buffer,
            dest: Port::Buffer,
            pair: None,
        })
    }

    /// Runs the transaction a `CMD` write started: the chip-register half now, the array half
    /// latched for [`SpiMem::service`].
    fn start(&mut self, triggers: u32, now: VTime, sched: &mut Scheduler) -> Started {
        let Some(txn) = self.decode(triggers) else {
            return Started::default();
        };
        let tx = self.take(txn.src, txn.tx_len);
        let mut rx = vec![ERASED; txn.rx_len];
        let was_busy = self.chip.busy();
        let accepted = self.chip.accept(txn.opcode, txn.addr, &tx, &mut rx);
        if let (Some(pair), 2..) = (txn.pair, rx.len()) {
            let mut high = [ERASED; 1];
            self.chip.accept(pair, 0, &[], &mut high);
            rx[1] = high[0];
        }
        self.put(txn.dest, &rx);
        let mut started = Started::default();
        if !was_busy && self.chip.busy() {
            // The duration the guest polls is register state, so even an immediate completion is
            // an event at `now`.
            sched.schedule(
                now,
                VTime(now.0.saturating_add(accepted.busy_ps)),
                EventKey {
                    owner: Owner::Periph(B::ID),
                    tag: TAG_WIP,
                },
            );
            // The write stops the slice so the completion cannot fire a slice late. A status
            // write latches no array work and would otherwise return `stop` false.
            started.stop = true;
        }
        let Some(op) = accepted.array else {
            return started;
        };
        started.stop = true;
        started.wrote = first_page(op);
        self.latched = Some(Latched {
            op,
            tx,
            rx_len: txn.rx_len,
            dest: txn.dest,
        });
        started
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Started {
    /// The write must end the slice: a transaction is latched for [`SpiMem::service`], or the
    /// `WIP` completion was scheduled. Both mean `RegWrite::stop`.
    pub stop: bool,
    /// A program or erase changed flash from this page on, so the cache must drop it.
    pub wrote: Option<u32>,
}

/// The first flash page a program or erase changes, or `None` for a read. This is
/// [`flash_xmc::span`], so a transaction past 0x800000 reports the mirrored page.
fn first_page(op: ArrayOp) -> Option<u32> {
    flash_xmc::span(op).map(|w| w.first_page)
}

impl<B: SpiMemBlock> Peripheral for SpiMem<B> {
    const ID: PeriphId = B::ID;
    const BASE: u32 = B::BASE;
    const SIZE: u32 = B::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        let _ = cx;
        self.apply_reset(kind);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let started = self.store_timed(
            off,
            size,
            val,
            cx.now,
            cx.ledger,
            cx.sched,
            cx.profile.spi1_clocked,
        );
        RegWrite {
            stop: started.stop,
            wiring: started
                .wrote
                .map_or(Wiring::None, |phys_page| Wiring::FlashWritten { phys_page }),
        }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        let _ = cx;
        match tag {
            TAG_WIP => self.complete(),
            // The end of a clocked command: every trigger bit still set clears.
            TAG_CMD => {
                let held = self.regs.get(idx::SPI_MEM_CMD);
                self.regs.clear_sc(idx::SPI_MEM_CMD, held);
            }
            _ => {}
        }
        Wiring::None
    }

    /// `RD_STATUS` carries `WIP`, which only the completion event changes, so a poll of it can be
    /// fast-forwarded to that event.
    fn stable_until(&self, off: u32, cx: &Cx) -> Stability {
        let _ = cx;
        match self.index_of(off) {
            Some(i) if i == idx::SPI_MEM_RD_STATUS && B::FLASH_HOST => Stability::UntilNextEvent,
            // A clocked command's trigger bits clear only at its `TAG_CMD` event.
            Some(i) if i == idx::SPI_MEM_CMD && B::FLASH_HOST => Stability::UntilNextEvent,
            _ => Stability::Never,
        }
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.index_of(off)
            .map_or(Fidelity::U, |i| self.regs.spec(i).class)
    }
}

/// Snapshot form of a SPI_MEM model: register values, touch bits and the part. `RegStore` holds a
/// `'static` table beside its values, so only the values travel.
#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct Snapshot {
    regs: Vec<u32>,
    touched: Vec<u64>,
    chip: XmcChip,
    latched: Option<Latched>,
}

const WRONG_SHAPE: &str = "SPI_MEM snapshot has the wrong register or touch-bit count";

impl<B: SpiMemBlock> SpiMem<B> {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            regs: (0..REG_COUNT).map(|i| self.regs.get(i)).collect(),
            touched: self.touched.clone(),
            chip: self.chip.clone(),
            latched: self.latched.clone(),
        }
    }

    fn restore(snap: Snapshot) -> Result<SpiMem<B>, &'static str> {
        let mut model = SpiMem::<B>::default();
        if snap.regs.len() != REG_COUNT || snap.touched.len() != model.touched.len() {
            return Err(WRONG_SHAPE);
        }
        for (i, val) in snap.regs.iter().enumerate() {
            model.regs.set(i, *val);
        }
        model.touched = snap.touched;
        model.chip = snap.chip;
        model.latched = snap.latched;
        Ok(model)
    }
}

impl<B: SpiMemBlock> Serialize for SpiMem<B> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.snapshot().serialize(s)
    }
}

impl<'de, B: SpiMemBlock> Deserialize<'de> for SpiMem<B> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<SpiMem<B>, D::Error> {
        SpiMem::restore(Snapshot::deserialize(d)?).map_err(D::Error::custom)
    }
}

pub type Spi1Model = SpiMem<super::block::Spi1>;

/// The former name of [`Spi1Model`], still used by `tests/milestones/m10.rs`.
pub type Spi1Mem = Spi1Model;

pub type Spi0Model = SpiMem<super::block::Spi0>;

#[cfg(test)]
mod tests {
    use pemu_core::fidelity::FirstTouch;

    use super::super::id;
    use super::*;
    use crate::flash_store::{PAGE_LEN, PAGES};
    use crate::periph::flash_xmc::{FlashTiming, SR1_WEL, SR1_WIP};

    mod off {
        pub const CMD: u32 = 0x00;
        pub const ADDR: u32 = 0x04;
        pub const CTRL: u32 = 0x08;
        pub const USER: u32 = 0x18;
        pub const USER1: u32 = 0x1C;
        pub const USER2: u32 = 0x20;
        pub const MOSI_DLEN: u32 = 0x24;
        pub const MISO_DLEN: u32 = 0x28;
        pub const RD_STATUS: u32 = 0x2C;
        pub const FSM: u32 = 0x54;
        pub const W0: u32 = 0x58;
        pub const FLASH_SUS_CTRL: u32 = 0x9C;
        pub const SUS_STATUS: u32 = 0xA4;
    }

    /// The command host, its part and the array behind it, driven the way the machine will:
    /// a register write, then the array half, then the events that came due.
    struct Host {
        spi1: Spi1Model,
        spi0: Spi0Model,
        flash: FlashStore,
        sched: Scheduler,
        ledger: FidelityLedger,
        now: VTime,
        last: Started,
    }

    impl Host {
        fn new() -> Host {
            Host {
                spi1: Spi1Model::default(),
                spi0: Spi0Model::default(),
                flash: FlashStore::erased(),
                sched: Scheduler::default(),
                ledger: FidelityLedger::default(),
                now: VTime(0),
                last: Started::default(),
            }
        }

        fn read(&mut self, off: u32) -> u32 {
            self.spi1.load(off, Size::B4, self.now, &mut self.ledger)
        }

        /// One 32-bit register write, followed by the array half the machine owes the model.
        fn write(&mut self, off: u32, val: u32) -> Option<Written> {
            self.last = self.spi1.store(
                off,
                Size::B4,
                val,
                self.now,
                &mut self.ledger,
                &mut self.sched,
            );
            assert!(
                !self.spi1.is_pending() || self.last.stop,
                "latched array work must stop the slice"
            );
            if self.last.stop {
                return self.spi1.service(&mut self.flash);
            }
            None
        }

        fn pump(&mut self) {
            while let Some(key) = self.sched.pop_due(self.now) {
                assert_eq!(key.owner, Owner::Periph(id::SPI1));
                assert_eq!(key.tag, TAG_WIP);
                self.spi1.complete();
            }
        }

        fn advance_to_next_event(&mut self) {
            if let Some(t) = self.sched.next_time() {
                self.now = t;
            }
            self.pump();
        }

        fn buffer(&self, len: usize) -> Vec<u8> {
            self.spi1.buffer(len)
        }

        fn put_buffer(&mut self, bytes: &[u8]) {
            for (i, chunk) in bytes.chunks(4).enumerate() {
                let mut word = 0u32;
                for (b, byte) in chunk.iter().enumerate() {
                    word |= u32::from(*byte) << (8 * b);
                }
                self.write(off::W0 + 4 * i as u32, word);
            }
        }

        fn write_enable(&mut self) {
            self.write(off::CMD, cmd::FLASH_WREN);
        }
    }

    #[test]
    fn the_two_instances_share_one_register_layout() {
        assert_eq!(regs_spi0::REG_COUNT, regs_spi1::REG_COUNT);
        for (a, b) in regs_spi0::REGS.iter().zip(regs_spi1::REGS.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.off, b.off);
            assert_eq!(a.reset, b.reset);
        }
        assert_eq!(regs_spi1::REGS[idx::SPI_MEM_CMD].off, off::CMD as u16);
        assert_eq!(regs_spi1::REGS[idx::SPI_MEM_FSM].off, off::FSM as u16);
        assert_eq!(regs_spi1::REGS[idx::SPI_MEM_W0].off, off::W0 as u16);
        assert_eq!(<Spi1Model as Peripheral>::BASE, 0x6000_2000);
        assert_eq!(<Spi0Model as Peripheral>::BASE, 0x6000_3000);
    }

    #[test]
    fn registers_come_up_at_their_spec_reset_values() {
        // The bootloader saves and restores CTRL, USER, USER1 and USER2 around every user command,
        // so their reset values matter.
        let mut h = Host::new();
        assert_eq!(h.read(off::CTRL), 0x002C_A000);
        assert_eq!(h.read(off::USER), 0x8000_0000);
        assert_eq!(h.read(off::USER1), 0x5C00_0007);
        assert_eq!(h.read(off::USER2), 0x7000_0000);
        assert_eq!(h.read(off::CMD), 0);
        assert_eq!(h.read(0x3FC), 0x0200_7170, "DATE is constant");
        assert_eq!(h.read(0x030), 0);
        assert_eq!(h.read(0xFFC), 0);
    }

    /// Every command bit self-clears inside the access and the SPI0 FSM reads idle, which is the
    /// whole of ROM `Wait_SPI_Idle`.
    #[test]
    fn the_wait_spi_idle_predicates_hold_on_the_first_read() {
        let mut h = Host::new();
        let mut ledger = FidelityLedger::default();
        let mut sched = Scheduler::default();

        // Step 1: SPI0 FSM & 0x70 == 0.
        let fsm = h.spi0.load(off::FSM, Size::B4, VTime(0), &mut ledger);
        assert_eq!(fsm & 0x70, 0);
        assert_eq!(fsm, 0x200, "the lock-delay field keeps its reset value");

        // Step 2: SPI1 CMD & 0xF == 0 (MST_ST), and CMD == 0 after every command bit.
        for bit in [
            cmd::FLASH_WREN,
            cmd::FLASH_WRDI,
            cmd::FLASH_RDID,
            cmd::FLASH_RDSR,
            cmd::FLASH_RES,
            cmd::USR,
            cmd::USR | cmd::FLASH_PE,
        ] {
            h.write(off::CMD, bit);
            assert_eq!(h.read(off::CMD), 0, "{bit:#X} must self-clear");
        }

        // Step 3: the RDSR loop. With nothing in progress WIP is already 0.
        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(h.read(off::RD_STATUS) & 1, 0);

        let started = h.spi0.store(
            off::CMD,
            Size::B4,
            cmd::FLASH_CE,
            VTime(0),
            &mut ledger,
            &mut sched,
        );
        assert_eq!(started, Started::default(), "SPI0 drives no transaction");
        assert_eq!(h.spi0.load(off::CMD, Size::B4, VTime(0), &mut ledger), 0);
    }

    /// The bits the device does not store read what the `probe_campaign_regs` capture reads.
    #[test]
    fn the_unstored_bits_read_the_device_values_after_the_boot_writes() {
        const MISC: u32 = 0x34;
        const CTRL2: u32 = 0x10;
        let mut h = Host::new();
        let mut ledger = FidelityLedger::default();
        let mut sched = Scheduler::default();
        let t = VTime(0);
        assert_eq!(
            h.spi0.load(MISC, Size::B4, t, &mut ledger),
            0x28,
            "SPI0 MISC from reset"
        );
        for (o, v) in [
            (off::USER, 0xF000_00C0),
            (off::MOSI_DLEN, 0xFF),
            (off::MISO_DLEN, 0xFF),
            (MISC, 0x2),
        ] {
            h.spi0.store(o, Size::B4, v, t, &mut ledger, &mut sched);
        }
        let spi0: Vec<u32> = [off::USER, off::MOSI_DLEN, off::MISO_DLEN, MISC]
            .iter()
            .map(|&o| h.spi0.load(o, Size::B4, t, &mut ledger))
            .collect();
        assert_eq!(spi0, [0x2000_00C0, 0, 0, 0x28]);
        for v in [0x41, 0x21, 0x20, 0x3E0] {
            h.write(CTRL2, v);
        }
        assert_eq!(h.read(CTRL2), 0, "SPI1 CTRL2");
        // SPI1 keeps the bits SPI0 drops: its USER phases and data lengths drive the engine.
        h.write(off::USER, 0xF000_00C0);
        h.write(off::MOSI_DLEN, 0xFF);
        assert_eq!(h.read(off::USER), 0xF000_00C0);
        assert_eq!(h.read(off::MOSI_DLEN), 0xFF);
        assert_eq!(
            h.read(MISC),
            0x2,
            "SPI1 MISC keeps its reset value, as the device reads it"
        );
        // A reset restores the SPI0 view, not the CSV value 0x2.
        let sys = pemu_core::reset::ResetCause::RTC_SW_SYS;
        h.spi0
            .apply_reset(pemu_core::reset::ResetKind::of(sys).unwrap());
        assert_eq!(h.spi0.load(MISC, Size::B4, t, &mut ledger), 0x28);
        // The CSV reset 0x80000000 is `USR_COMMAND`, a bit SPI0 does not store.
        assert_eq!(h.spi0.load(off::USER, Size::B4, t, &mut ledger), 0);
    }

    #[test]
    fn the_legacy_identity_and_status_commands_answer_where_the_rom_reads_them() {
        // RDID leaves the three identity bytes in W0; RDSR leaves the status in RD_STATUS[15:0].
        let mut h = Host::new();
        h.write(off::CMD, cmd::FLASH_RDID);
        assert_eq!(h.read(off::W0), 0x0017_4020);

        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(h.read(off::RD_STATUS), 0x0000);
        h.write_enable();
        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(h.read(off::RD_STATUS), u32::from(SR1_WEL));
        h.write(off::CMD, cmd::FLASH_WRDI);
        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(h.read(off::RD_STATUS), 0);

        // CTRL.WRSR_2B makes the legacy status access two bytes wide.
        h.write(off::CTRL, 0x002C_A000 | CTRL_WRSR_2B);
        h.write_enable();
        h.write(off::RD_STATUS, 0x0200);
        h.write(off::CMD, cmd::FLASH_WRSR);
        h.advance_to_next_event();
        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(
            h.read(off::RD_STATUS),
            0x0200,
            "the second byte went to SR2 and comes back from it"
        );
    }

    #[test]
    fn the_legacy_page_program_writes_the_array_and_reports_its_pages() {
        // ROM SPI_page_program: ADDR carries the address in [23:0] and the byte count in
        // [31:24], the payload comes from W0 upward, and the completion predicate is CMD == 0.
        let mut h = Host::new();
        h.put_buffer(&[0xDE, 0xAD, 0xBE, 0xEF]);
        h.write_enable();
        assert_eq!(
            h.write(off::ADDR, 0x0400_1FFC),
            None,
            "a plain ADDR write starts nothing"
        );
        let written = h.write(off::CMD, cmd::FLASH_PP);
        assert_eq!(h.read(off::CMD), 0);
        assert_eq!(
            written,
            Some(Written {
                first_page: 1,
                pages: 1
            }),
            "the payload ends at the 4 KB page boundary"
        );
        assert_eq!(h.last.wrote, Some(1), "Wiring::FlashWritten names page 1");
        assert!(h.last.stop);
        for (i, byte) in [0xDE, 0xAD, 0xBE, 0xEF].iter().enumerate() {
            assert_eq!(h.flash.read_byte(0x1FFC + i as u32), *byte);
        }

        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(h.read(off::RD_STATUS), u32::from(SR1_WIP | SR1_WEL));
        h.advance_to_next_event();
        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(h.read(off::RD_STATUS), 0);

        h.put_buffer(&[0x00]);
        let written = h.write(off::CMD, cmd::FLASH_PP);
        assert_eq!(written, None);
        assert_eq!(h.flash.read_byte(0x1FFC), 0xDE);
    }

    #[test]
    fn every_legacy_erase_kind_clears_its_span_and_charges_its_own_wip() {
        // The device profile's WIP durations are NOR typicals, UNVERIFIED for this part.
        let mut h = Host::new();
        h.spi1.chip_mut().set_timing(FlashTiming::DEVICE);
        let dirty = |h: &mut Host| {
            for page in 0..32u32 {
                h.flash.program(page * PAGE_LEN, &[0x00]);
            }
        };

        dirty(&mut h);
        h.write_enable();
        h.write(off::ADDR, 0x0000_0123);
        let written = h.write(off::CMD, cmd::FLASH_SE);
        assert_eq!(
            written,
            Some(Written {
                first_page: 0,
                pages: 1
            })
        );
        assert_eq!(h.flash.read_byte(0), ERASED);
        assert_eq!(h.flash.read_byte(PAGE_LEN), 0x00);
        assert_eq!(h.sched.next_time(), Some(VTime(45_000_000_000)));
        h.advance_to_next_event();

        dirty(&mut h);
        h.write_enable();
        h.write(off::ADDR, 0x0001_0000);
        let written = h.write(off::CMD, cmd::FLASH_BE);
        assert_eq!(
            written,
            Some(Written {
                first_page: 16,
                pages: 16
            })
        );
        assert_eq!(h.flash.read_byte(0x1_0000), ERASED);
        assert_eq!(h.flash.read_byte(0xF000), 0x00, "the block below is kept");
        assert_eq!(h.sched.next_time(), Some(VTime(h.now.0 + 150_000_000_000)));
        h.advance_to_next_event();

        dirty(&mut h);
        h.write_enable();
        let written = h.write(off::CMD, cmd::FLASH_CE);
        assert_eq!(
            written,
            Some(Written {
                first_page: 0,
                pages: PAGES
            })
        );
        assert_eq!(h.flash.read_byte(0), ERASED);
        assert_eq!(
            h.sched.next_time(),
            Some(VTime(h.now.0 + 20_000_000_000_000))
        );

        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(h.read(off::RD_STATUS) & u32::from(SR1_WIP), 1);
        h.advance_to_next_event();
        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(h.read(off::RD_STATUS), 0);
    }

    /// Every write that starts the `WIP` window must report `stop`, including the status write,
    /// which latches no array work and so has no other reason to.
    #[test]
    fn a_write_that_schedules_the_wip_completion_stops_the_slice() {
        let mut h = Host::new();
        h.spi1.chip_mut().set_timing(FlashTiming::DEVICE);

        h.write(off::ADDR, 0x0000_1000);
        assert!(!h.last.stop);
        assert_eq!(h.sched.next_time(), None);

        h.write_enable();
        assert!(!h.last.stop);
        assert_eq!(h.sched.next_time(), None);

        // A status write: WIP is set, the completion scheduled, nothing latched for `service`.
        h.write(off::RD_STATUS, 0x02);
        h.write(off::CMD, cmd::FLASH_WRSR);
        assert!(!h.spi1.is_pending(), "a status write latches no array work");
        assert!(h.last.stop, "the write scheduled the WIP completion");
        assert_eq!(h.last.wrote, None);
        assert_eq!(
            h.sched.next_time(),
            Some(VTime(FlashTiming::DEVICE.pp_ps)),
            "the completion is the PP duration away, well inside a slice"
        );

        h.advance_to_next_event();
        h.write_enable();
        h.put_buffer(&[0x00]);
        h.write(off::ADDR, 0x0100_1000);
        h.write(off::CMD, cmd::FLASH_PP);
        assert!(h.last.stop);
    }

    /// A program or erase while `WIP` is set is ignored; only status reads and the reset pair are
    /// answered.
    #[test]
    fn a_program_arriving_while_the_part_is_busy_is_ignored() {
        let mut h = Host::new();
        h.spi1.chip_mut().set_timing(FlashTiming::DEVICE);

        h.write_enable();
        h.put_buffer(&[0x00]);
        h.write(off::ADDR, 0x0100_1000);
        assert_eq!(
            h.write(off::CMD, cmd::FLASH_PP),
            Some(Written {
                first_page: 1,
                pages: 1
            })
        );
        let due = h.sched.next_time().expect("the first completion");

        h.write(off::ADDR, 0x0100_2000);
        let written = h.write(off::CMD, cmd::FLASH_PP);
        assert_eq!(written, None, "the busy part accepts no program");
        assert_eq!(h.last.wrote, None);
        assert!(!h.last.stop);
        assert_eq!(h.flash.read_byte(0x2000), ERASED, "the array is untouched");
        assert_eq!(h.sched.next_time(), Some(due), "no second WIP window");

        // The status read the ROM and the chip driver poll is answered throughout.
        h.write(off::CMD, cmd::FLASH_RDSR);
        assert_eq!(h.read(off::RD_STATUS), u32::from(SR1_WIP | SR1_WEL));

        // After the completion the same command lands, and needs its own WREN.
        h.advance_to_next_event();
        h.write(off::CMD, cmd::FLASH_PP);
        assert_eq!(h.flash.read_byte(0x2000), ERASED, "WEL was cleared");
        h.write_enable();
        assert_eq!(
            h.write(off::CMD, cmd::FLASH_PP),
            Some(Written {
                first_page: 2,
                pages: 1
            })
        );
        assert_eq!(h.flash.read_byte(0x2000), 0x00);
    }

    #[test]
    fn a_user_transaction_reads_the_array_into_the_w_buffer() {
        // The esp_rom_spiflash_read shape: command byte from USER2, 24-bit address from ADDR,
        // MISO length from MISO_DLEN, data into W0 upward.
        let mut h = Host::new();
        h.flash
            .program(0x34_1000, &[0xE9, 0x04, 0x02, 0x20, 0x11, 0x22]);

        h.write(off::USER, 1 << 31 | 1 << 30 | 1 << 29 | 1 << 28);
        h.write(off::USER1, 23 << 26 | 7);
        h.write(off::USER2, 7 << 28 | u32::from(op::FAST_READ));
        h.write(off::MISO_DLEN, 6 * 8 - 1);
        h.write(off::ADDR, 0x34_1000);
        h.write(off::CMD, cmd::USR);

        assert_eq!(h.read(off::CMD), 0);
        assert_eq!(h.buffer(6), vec![0xE9, 0x04, 0x02, 0x20, 0x11, 0x22]);
        assert_eq!(h.read(off::W0), 0x2002_04E9);
        assert_eq!(h.last.wrote, None, "a read changes no flash page");

        h.write(off::W0 + 4 * 4, 0x5A5A_5A5A);
        h.write(off::MISO_DLEN, 4 * 8 - 1);
        h.write(off::CMD, cmd::USR);
        assert_eq!(h.read(off::W0 + 4 * 4), 0x5A5A_5A5A);

        h.write(off::ADDR, 0xFF34_1000);
        h.write(off::MISO_DLEN, 8 - 1);
        h.write(off::CMD, cmd::USR);
        assert_eq!(h.buffer(1), vec![0xE9]);
    }

    /// `spi1_clocked`: the `CMD` trigger bit stays set for the command's bus time. A FAST_READ of
    /// 6 bytes with 8 dummy cycles is 8 + 24 + 8 + 48 SPI clocks: 1.1 us at 80 MHz.
    #[test]
    fn a_clocked_command_holds_its_trigger_bit_for_its_bus_time() {
        const CLOCK: u32 = 0x14;
        let mut h = Host::new();
        h.flash.program(0x1000, &[1, 2, 3, 4, 5, 6]);
        h.write(off::USER, 1 << 31 | 1 << 30 | 1 << 29 | 1 << 28);
        h.write(off::USER1, 23 << 26 | 7);
        h.write(off::USER2, 7 << 28 | u32::from(op::FAST_READ));
        h.write(off::MISO_DLEN, 6 * 8 - 1);
        h.write(off::ADDR, 0x1000);
        for (clock, ps) in [(1u32 << 31, 1_100_000u64), (3 << 16, 4_400_000)] {
            h.write(CLOCK, clock);
            let started = h.spi1.store_timed(
                off::CMD,
                Size::B4,
                cmd::USR,
                h.now,
                &mut h.ledger,
                &mut h.sched,
                true,
            );
            assert!(started.stop, "the scheduled end stops the slice");
            h.spi1.service(&mut h.flash);
            assert_eq!(
                h.buffer(6),
                vec![1, 2, 3, 4, 5, 6],
                "the read ran at the write"
            );
            assert_eq!(h.read(off::CMD), cmd::USR, "the trigger bit is held");
            assert_eq!(h.sched.next_time(), Some(VTime(h.now.0 + ps)));
            h.now = VTime(h.now.0 + ps);
            let key = h.sched.pop_due(h.now).expect("the command's end is due");
            assert_eq!(key.tag, TAG_CMD);
            let held = h.spi1.regs.get(idx::SPI_MEM_CMD);
            h.spi1.regs.clear_sc(idx::SPI_MEM_CMD, held);
            assert_eq!(h.read(off::CMD), 0, "the event clears it");
        }
    }

    #[test]
    fn a_user_transaction_runs_the_status_and_program_commands_too() {
        // The same USR path carries WREN, RDSR and PP when the chip driver issues them
        // (bootloader_flash_execute_command_common).
        let mut h = Host::new();
        let user_command = |h: &mut Host, opcode: u8| {
            h.write(off::USER, 1 << 31);
            h.write(off::USER2, 7 << 28 | u32::from(opcode));
            h.write(off::MOSI_DLEN, 0);
            h.write(off::MISO_DLEN, 0);
            h.write(off::CMD, cmd::USR);
        };

        user_command(&mut h, op::WREN);
        h.write(off::USER, 1 << 31 | 1 << 28);
        h.write(off::USER2, 7 << 28 | u32::from(op::RDSR));
        h.write(off::MISO_DLEN, 7);
        h.write(off::CMD, cmd::USR);
        assert_eq!(h.buffer(1), vec![SR1_WEL], "a USR RDSR answers in W0");

        h.put_buffer(&[0x0F, 0xF0]);
        h.write(off::USER, 1 << 31 | 1 << 30 | 1 << 27);
        h.write(off::USER1, 23 << 26);
        h.write(off::USER2, 7 << 28 | u32::from(op::PP));
        h.write(off::MOSI_DLEN, 2 * 8 - 1);
        h.write(off::ADDR, 0x20_0000);
        let written = h.write(off::CMD, cmd::USR | cmd::FLASH_PE);
        assert_eq!(
            written,
            Some(Written {
                first_page: 0x200,
                pages: 1
            })
        );
        assert_eq!(h.flash.read_byte(0x20_0000), 0x0F);
        assert_eq!(h.flash.read_byte(0x20_0001), 0xF0);
        h.advance_to_next_event();

        h.write(off::USER, 0);
        let written = h.write(off::CMD, cmd::USR);
        assert_eq!(written, None);
        assert!(!h.spi1.is_pending());
    }

    /// The part decodes the low 23 bits (the `probe_campaign_regs` capture), so the page a
    /// transaction above 0x800000 reports is the mirrored one, never outside 0..PAGES. The program
    /// and erase half is UNVERIFIED.
    #[test]
    fn a_transaction_beyond_the_part_reaches_the_mirror_below_it() {
        let mut h = Host::new();
        h.flash.program(0x10, &[0x12, 0x34, 0x56, 0x78]);

        h.write(off::USER, 1 << 31 | 1 << 30 | 1 << 28);
        h.write(off::USER1, 23 << 26);
        h.write(off::USER2, 7 << 28 | u32::from(op::READ));
        h.write(off::MISO_DLEN, 4 * 8 - 1);
        h.write(off::ADDR, 0x80_0010);
        h.write(off::CMD, cmd::USR);
        assert_eq!(
            h.buffer(4),
            vec![0x12, 0x34, 0x56, 0x78],
            "0x800010 reads 0x000010"
        );

        // Every span agrees with `service`, so no transaction can hand
        // `Engine::invalidate_flash_page` an index outside 0..PAGES.
        for (cmd, addr, page) in [
            (cmd::FLASH_PP, 0x0480_0001, 0),
            (cmd::FLASH_SE, 0x00FF_FFFF, 2047),
            (cmd::FLASH_BE, 0x00F0_0000, 1792),
        ] {
            h.put_buffer(&[0x00, 0x00, 0x00, 0x00]);
            h.write_enable();
            h.write(off::ADDR, addr);
            assert_eq!(
                h.write(off::CMD, cmd).map(|w| w.first_page),
                Some(page),
                "{cmd:#010X} at {addr:#010X}"
            );
            assert_eq!(h.last.wrote, Some(page), "{cmd:#010X} at {addr:#010X}");
            h.advance_to_next_event();
        }
        assert_eq!(
            h.flash.read_byte(0x1),
            0x00,
            "the program at 0x800001 landed at 1"
        );
        assert_eq!(h.flash.read_byte(0x7F_F000), ERASED);
    }

    #[test]
    fn first_touches_are_reported_once_per_register_with_the_spec_class() {
        let mut h = Host::new();
        h.read(off::CMD);
        h.write(off::CMD, cmd::FLASH_WREN);
        h.read(off::FSM);
        h.read(off::FSM + 2);
        h.read(0x030);
        let touches: Vec<_> = h
            .ledger
            .first_touches()
            .iter()
            .map(|t: &FirstTouch| (t.periph, t.off, t.access, t.allowlisted))
            .collect();
        assert_eq!(
            touches,
            vec![
                (id::SPI1, off::CMD, TouchAccess::Read, false),
                (id::SPI1, off::FSM, TouchAccess::Read, false),
                (id::SPI1, 0x030, TouchAccess::Read, false),
            ]
        );
        assert_eq!(h.spi1.fidelity(off::CMD), Fidelity::B);
        assert_eq!(h.spi1.fidelity(off::FSM), Fidelity::B);
        assert_eq!(
            h.spi1.fidelity(off::ADDR),
            Fidelity::B,
            "the address the transaction decodes, specs/blocks/spi1.toml"
        );
        assert_eq!(h.spi1.fidelity(0x030), Fidelity::U, "not a register");
    }

    #[test]
    fn a_reset_restores_the_registers_and_leaves_the_part_alone() {
        use pemu_core::reset::{ResetCause, ResetKind};

        let mut h = Host::new();
        h.write(off::CTRL, 0);
        h.write_enable();
        h.write(off::ADDR, 0x0100_2000);
        h.write(off::CMD, cmd::FLASH_SE);
        assert!(h.spi1.chip().busy());

        // Read every register asserted below, so the count can only grow if the reset itself made
        // one report twice.
        for off in [off::CTRL, off::USER1, off::FSM] {
            h.read(off);
        }
        let before = h.ledger.first_touches().len();
        let kind = ResetKind::of(ResetCause(0x01)).expect("a documented power-on reset");
        h.spi1.apply_reset(kind);

        assert_eq!(
            h.read(off::CTRL),
            0x002C_A000,
            "the spec reset value is back"
        );
        assert_eq!(h.read(off::USER1), 0x5C00_0007);
        assert_eq!(h.read(off::FSM), 0x200);
        assert!(h.spi1.chip().busy(), "no MCU reset reaches the part");
        assert_eq!(h.flash.read_byte(0x2000), ERASED, "the erase stands");
        assert_eq!(
            h.ledger.first_touches().len(),
            before,
            "first-touch state is per machine, not per reset"
        );

        h.write(off::CTRL, 0);
        let cpu = ResetKind::of(ResetCause(0x0C)).expect("a documented CPU reset");
        h.spi1.apply_reset(cpu);
        assert_eq!(h.read(off::CTRL), 0, "a CPU reset keeps the block");
    }

    #[test]
    fn an_export_clears_the_w_buffer_and_latched_program_of_a_secret_range() {
        let secret = |start: u32, end: u32| start < 0x0001_0000 && end > 0x0000_9000;
        let read = |addr: u32| {
            let mut h = Host::new();
            h.flash.program(addr, &[0x11, 0x22, 0x33, 0x44]);
            h.write(off::USER, 1 << 31 | 1 << 30 | 1 << 29 | 1 << 28);
            h.write(off::USER1, 23 << 26 | 7);
            h.write(off::USER2, 7 << 28 | u32::from(op::FAST_READ));
            h.write(off::MISO_DLEN, 4 * 8 - 1);
            h.write(off::ADDR, addr);
            h.write(off::CMD, cmd::USR);
            assert_eq!(h.buffer(4), vec![0x11, 0x22, 0x33, 0x44]);
            h
        };
        let mut inside = read(0x0000_9000);
        assert!(inside.spi1.redact_flash(&secret));
        assert_eq!(inside.buffer(MAX_DATA), vec![ERASED; MAX_DATA]);
        assert!(!inside.spi1.redact_flash(&secret), "nothing left to clear");
        let mut moved = read(0x0000_9000);
        moved.write(off::ADDR, 0x0002_0000);
        assert!(moved.spi1.redact_flash(&secret));
        assert_eq!(moved.buffer(MAX_DATA), vec![ERASED; MAX_DATA]);
        let mut outside = read(0x0002_0000);
        assert!(outside.spi1.redact_flash(&secret));
        assert_eq!(outside.buffer(4), vec![ERASED; 4]);

        let mut h = Host::new();
        h.put_buffer(&[0xDE, 0xAD, 0xBE, 0xEF]);
        h.write_enable();
        h.write(off::ADDR, 0x0400_9000);
        let started = h.spi1.store(
            off::CMD,
            Size::B4,
            cmd::FLASH_PP,
            h.now,
            &mut h.ledger,
            &mut h.sched,
        );
        assert!(started.stop && h.spi1.is_pending());
        assert!(h.spi1.redact_flash(&secret));
        let latched = h.spi1.latched.as_ref().expect("still latched");
        assert_eq!(latched.tx, vec![ERASED; 4], "the program data is gone");
        assert_eq!(
            h.buffer(4),
            vec![ERASED; 4],
            "and the legacy program's payload left in W with it"
        );
    }

    #[test]
    fn a_snapshot_round_trip_keeps_the_registers_the_part_and_the_latched_work() {
        let mut h = Host::new();
        h.write(off::CTRL, 0x0011_2233);
        let ctrl = h.read(off::CTRL);
        assert_ne!(ctrl, 0x002C_A000, "the write changed the register");
        h.write_enable();
        h.write(off::ADDR, 0x0000_3000);
        let started = h.spi1.store(
            off::CMD,
            Size::B4,
            cmd::FLASH_SE,
            h.now,
            &mut h.ledger,
            &mut h.sched,
        );
        assert!(started.stop && h.spi1.is_pending());

        let snap = h.spi1.snapshot();
        let mut restored = Spi1Model::restore(snap).expect("a snapshot of this block");
        assert_eq!(
            restored.load(off::CTRL, Size::B4, h.now, &mut h.ledger),
            ctrl
        );
        assert!(restored.chip().busy());
        assert!(restored.is_pending());
        let written = restored.service(&mut h.flash);
        assert_eq!(
            written,
            Some(Written {
                first_page: 3,
                pages: 1
            })
        );
        assert!(!restored.is_pending());

        let mut wrong = h.spi1.snapshot();
        wrong.regs.pop();
        assert_eq!(Spi1Model::restore(wrong).err(), Some(WRONG_SHAPE));
    }

    /// The auto-suspend registers carry SC bits as `SPI_MEM_CMD` does, so the guest never reads
    /// back a 1 it wrote. No suspend engine runs (class C).
    #[test]
    fn the_suspend_bits_self_clear_like_the_device() {
        let mut h = Host::new();
        // FLASH_PER (bit 0) and FLASH_PES (bit 1) are SC; the wait enables (bits 2 and 3) are R/W.
        h.write(off::FLASH_SUS_CTRL, 0b1111);
        let ctrl = h.read(off::FLASH_SUS_CTRL);
        assert_eq!(ctrl & 0b11, 0, "FLASH_PER and FLASH_PES self-clear");
        assert_eq!(
            ctrl & 0b1100,
            0b1100,
            "the R/W wait enables of the same register keep what was written"
        );

        // FLASH_SUS (bit 0) is SC, WAIT_PESR_CMD_2B (bit 1) is R/W.
        h.write(off::SUS_STATUS, 0b11);
        let status = h.read(off::SUS_STATUS);
        assert_eq!(status & 1, 0, "FLASH_SUS self-clears");
        assert_eq!(status & 0b10, 0b10, "the R/W bit beside it is kept");

        // A suspend write starts nothing: the part must not see a transaction it was never given.
        assert!(
            !h.spi1.is_pending(),
            "a suspend write latches no array work"
        );
        assert!(!h.last.stop, "a suspend write does not stop the slice");
    }
}
