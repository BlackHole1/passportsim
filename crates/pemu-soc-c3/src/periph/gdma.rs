//! GDMA in the C3 layout (`specs/blocks/gdma.toml`): three channels, an interrupt block per
//! channel at stride 0x10, an RX block at 0x70 and a TX block at 0xD0, both at channel stride 0xC0.
//! Getting the layout wrong means the LVGL flush never completes, so [`layout`] and its test pin
//! the offsets. Routing is by `PERI_SEL` code, never channel number, because the pair a driver
//! gets depends on allocation order.
//!
//! The link registers hold 20 bits and the DMA address is `0x3FC00000 | addr20`. Every descriptor
//! and buffer lives in internal DRAM 0x3FC80000 to 0x3FCDFFFF, where that rebuild is the identity.
//!
//! Nothing moves on its own: the bound peripheral drives the walk (`wiring/spi2.rs`,
//! `wiring/i2s.rs`). Guest memory is reached through [`DmaMem`].

use pemu_core::fidelity::{Fidelity, FidelityLedger};
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::Size;
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::time::VTime;

use crate::r#gen::regs_gdma::{BLOCK_SIZE, REG_COUNT, REGS, idx};

use super::reg_file::{RegFile, RegTable};
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring, block};

impl RegTable<REG_COUNT> for block::Gdma {
    const SPECS: &'static [pemu_core::regstore::RegSpec; REG_COUNT] = &REGS;
}

/// Channels of the C3 GDMA: three pairs, each an RX and a TX half.
pub const CHANNELS: usize = 3;

/// `PERI_SEL` code of SPI2 (IDF `soc/esp32c3/include/soc/gdma_channel.h`).
pub const PERI_SPI2: u32 = 0;
/// `PERI_SEL` code of I2S0.
pub const PERI_I2S0: u32 = 3;
/// `PERI_SEL` code of the AES accelerator (IDF `SOC_GDMA_TRIG_PERIPH_AES0`).
pub const PERI_AES: u32 = 6;
/// `PERI_SEL` code of the SHA accelerator (IDF `SOC_GDMA_TRIG_PERIPH_SHA0`).
pub const PERI_SHA: u32 = 7;
/// `PERI_SEL` reset value, meaning "not connected".
pub const PERI_NONE: u32 = 0x3F;

/// `PERI_SEL.peri_*_sel` (bits 5 to 0).
const PERI_SEL: u32 = 0x3F;
/// `IN_CONF1.in_check_owner` / `OUT_CONF1.out_check_owner` (bit 12).
const CHECK_OWNER: u32 = 1 << 12;
/// `OUT_CONF0.out_rst` / `IN_CONF0.in_rst` (bit 0).
const CONF0_RST: u32 = 1 << 0;
/// `OUT_CONF0.out_auto_wrback` (bit 2): clear the descriptor owner bit after use.
const OUT_AUTO_WRBACK: u32 = 1 << 2;
/// `*_LINK.*link_addr` (bits 19 to 0).
const LINK_ADDR: u32 = 0x000F_FFFF;

/// Interrupt bits of `INT_RAW_CHn`, `INT_ST_CHn`, `INT_ENA_CHn` and `INT_CLR_CHn`.
pub mod int {
    /// One inbound descriptor finished.
    pub const IN_DONE: u32 = 1 << 0;
    /// An inbound descriptor completed successfully; the row `gdma.i2s_in_suc_eof` polls it.
    pub const IN_SUC_EOF: u32 = 1 << 1;
    /// An inbound descriptor completed with an error.
    pub const IN_ERR_EOF: u32 = 1 << 2;
    /// One outbound descriptor finished.
    pub const OUT_DONE: u32 = 1 << 3;
    /// An outbound descriptor with EOF finished; the row `gdma.i2s_out_eof` polls it.
    pub const OUT_EOF: u32 = 1 << 4;
    /// An inbound descriptor failed the owner check.
    pub const IN_DSCR_ERR: u32 = 1 << 5;
    /// An outbound descriptor failed the owner check.
    pub const OUT_DSCR_ERR: u32 = 1 << 6;
    /// The inbound chain ran out of descriptors.
    pub const IN_DSCR_EMPTY: u32 = 1 << 7;
    /// The outbound chain reached its last EOF descriptor.
    pub const OUT_TOTAL_EOF: u32 = 1 << 8;
}

/// Descriptors a walk may visit on top of its byte budget before it raises `*_DSCR_ERR`.
///
/// A cyclic chain of zero-length descriptors spends no budget and would otherwise spin forever
/// inside one MMIO write, where neither the hang detector nor `wall_budget_ms` can end it. Slack
/// on top of the byte budget fires only on a chain that moves nothing. UNVERIFIED: the hardware
/// has no such limit; `*_DSCR_ERR` is the closest thing it has.
const DESCRIPTOR_SLACK: u32 = 64;

/// Guest memory as the DMA engine sees it, after [`dma_addr`]. Out-of-range bytes read 0 and drop
/// writes, as an unmapped DMA address does.
pub trait DmaMem {
    fn read(&mut self, addr: u32, out: &mut [u8]);
    fn write(&mut self, addr: u32, data: &[u8]);
}

/// The DMA-visible address of a 20-bit link or descriptor word: `0x3FC00000 | addr20`.
pub fn dma_addr(raw: u32) -> u32 {
    0x3FC0_0000 | (raw & LINK_ADDR)
}

/// One `dma_descriptor_t`: word 0 packs `size` (bits 11 to 0), `length` (bits 23 to 12),
/// `suc_eof` (bit 30) and `owner` (bit 31); word 1 is the buffer and word 2 the next descriptor,
/// 0 ending the chain.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Descriptor {
    pub at: u32,
    pub size: u32,
    /// Bytes the buffer holds, for a TX descriptor.
    pub length: u32,
    /// Last descriptor of this transfer.
    pub suc_eof: bool,
    /// The DMA engine owns the descriptor.
    pub owner: bool,
    pub buffer: u32,
    /// Next descriptor, already rebuilt through [`dma_addr`]; 0 ends the chain.
    pub next: u32,
}

impl Descriptor {
    /// Reads the three words at `at`, rebuilding both address words through [`dma_addr`]. A
    /// `next` of 0 stays 0: it ends the chain, and the rebuild would make it the window base.
    pub fn read(at: u32, mem: &mut dyn DmaMem) -> Descriptor {
        let mut words = [0u8; 12];
        mem.read(at, &mut words);
        let word = |i: usize| {
            u32::from_le_bytes([
                words[i * 4],
                words[i * 4 + 1],
                words[i * 4 + 2],
                words[i * 4 + 3],
            ])
        };
        let w0 = word(0);
        let next = word(2);
        Descriptor {
            at,
            size: w0 & 0xFFF,
            length: (w0 >> 12) & 0xFFF,
            suc_eof: w0 & (1 << 30) != 0,
            owner: w0 & (1 << 31) != 0,
            buffer: dma_addr(word(1)),
            next: if next == 0 { 0 } else { dma_addr(next) },
        }
    }

    /// Writes word 0 back with `length`, `suc_eof` and `owner` as given.
    fn write_back(&self, mem: &mut dyn DmaMem) {
        let w0 = (self.size & 0xFFF)
            | ((self.length & 0xFFF) << 12)
            | (u32::from(self.suc_eof) << 30)
            | (u32::from(self.owner) << 31);
        mem.write(self.at, &w0.to_le_bytes());
    }
}

/// Register indices of one channel, derived from the CH0 constants and the table strides, so a
/// table change fails a test instead of mis-decoding.
#[derive(Copy, Clone, Debug)]
pub struct Layout {
    pub int_raw: usize,
    pub int_st: usize,
    pub int_ena: usize,
    pub int_clr: usize,
    pub in_conf0: usize,
    pub in_conf1: usize,
    pub in_link: usize,
    pub in_suc_eof_des_addr: usize,
    pub in_dscr: usize,
    pub in_peri_sel: usize,
    pub out_conf0: usize,
    pub out_conf1: usize,
    pub out_link: usize,
    pub out_eof_des_addr: usize,
    pub out_dscr: usize,
    pub out_peri_sel: usize,
}

/// Distance in table entries between the interrupt blocks of two channels (offset stride 0x10).
const INT_STRIDE: usize = idx::GDMA_INT_RAW_CH1 - idx::GDMA_INT_RAW_CH0;
/// Distance in table entries between the RX and TX blocks of two channels (offset stride 0xC0).
const CH_STRIDE: usize = idx::GDMA_IN_CONF0_CH1 - idx::GDMA_IN_CONF0_CH0;

pub const fn layout(ch: usize) -> Layout {
    let i = INT_STRIDE * ch;
    let c = CH_STRIDE * ch;
    Layout {
        int_raw: idx::GDMA_INT_RAW_CH0 + i,
        int_st: idx::GDMA_INT_ST_CH0 + i,
        int_ena: idx::GDMA_INT_ENA_CH0 + i,
        int_clr: idx::GDMA_INT_CLR_CH0 + i,
        in_conf0: idx::GDMA_IN_CONF0_CH0 + c,
        in_conf1: idx::GDMA_IN_CONF1_CH0 + c,
        in_link: idx::GDMA_IN_LINK_CH0 + c,
        in_suc_eof_des_addr: idx::GDMA_IN_SUC_EOF_DES_ADDR_CH0 + c,
        in_dscr: idx::GDMA_IN_DSCR_CH0 + c,
        in_peri_sel: idx::GDMA_IN_PERI_SEL_CH0 + c,
        out_conf0: idx::GDMA_OUT_CONF0_CH0 + c,
        out_conf1: idx::GDMA_OUT_CONF1_CH0 + c,
        out_link: idx::GDMA_OUT_LINK_CH0 + c,
        out_eof_des_addr: idx::GDMA_OUT_EOF_DES_ADDR_CH0 + c,
        out_dscr: idx::GDMA_OUT_DSCR_CH0 + c,
        out_peri_sel: idx::GDMA_OUT_PERI_SEL_CH0 + c,
    }
}

/// Interrupt source of channel `ch`: pairs 0 to 2 are sources 44 to 46, level `INT_ST_CHn != 0`.
pub const fn source(ch: usize) -> IrqSource {
    match ch {
        0 => irq::DMA_CH0,
        1 => irq::DMA_CH1,
        _ => irq::DMA_CH2,
    }
}

/// One half of a channel: the descriptor walk state of its RX or TX side.
#[derive(
    Copy,
    Clone,
    Default,
    PartialEq,
    Eq,
    Debug,
    pemu_core::serde::Serialize,
    pemu_core::serde::Deserialize,
)]
#[serde(crate = "pemu_core::serde")]
struct Half {
    /// `start` was written and no `stop`, reset or chain end has ended the walk.
    running: bool,
    /// Descriptor the walk is at, already rebuilt through [`dma_addr`]; 0 ends the chain.
    cur: u32,
    /// Bytes already moved out of or into the current descriptor.
    off: u32,
}

/// What one register access asks the machine to do after the model returns.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Step {
    /// New level of the channel's interrupt source, when it changed.
    pub irq: [Option<bool>; CHANNELS],
    /// Finish this instruction, then leave the block (`OkStop`).
    pub stop: bool,
}

/// Bytes a TX descriptor walk produced.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TxPull {
    pub bytes: Vec<u8>,
    /// A descriptor with `suc_eof` finished, so `OUT_EOF` and `OUT_TOTAL_EOF` are raised.
    pub eof: bool,
    /// New level of the channel's interrupt source, when it changed.
    pub irq: Option<bool>,
}

/// What an RX descriptor walk consumed.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct RxPush {
    pub taken: u32,
    /// A descriptor completed, so `IN_SUC_EOF` is raised.
    pub eof: bool,
    /// New level of the channel's interrupt source, when it changed.
    pub irq: Option<bool>,
}

#[derive(Default, pemu_core::serde::Serialize, pemu_core::serde::Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Engine {
    regs: RegFile<block::Gdma, REG_COUNT>,
    rx: [Half; CHANNELS],
    tx: [Half; CHANNELS],
    /// Level this model last drove per source, so a change is reported once.
    irq: [bool; CHANNELS],
}

impl Engine {
    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.regs.read(off, size, now, ledger)
    }

    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> Step {
        let mut step = Step::default();
        for (i, delta) in self.regs.write(off, size, val, now, ledger).iter() {
            for ch in 0..CHANNELS {
                let l = layout(ch);
                if i == l.out_link {
                    // stop 20, start 21, restart 22: write-only pulses that read back 0.
                    self.link(ch, false, delta.triggers >> 20);
                    self.regs.clear_sc(i, 0x7 << 20);
                } else if i == l.in_link {
                    // stop 21, start 22, restart 23.
                    self.link(ch, true, delta.triggers >> 21);
                    self.regs.clear_sc(i, 0x7 << 21);
                } else if i == l.out_conf0 {
                    if rising(delta.before, delta.after, CONF0_RST) {
                        self.tx[ch] = Half::default();
                    }
                } else if i == l.in_conf0 {
                    if rising(delta.before, delta.after, CONF0_RST) {
                        self.rx[ch] = Half::default();
                    }
                } else if i == l.int_clr {
                    let raw = self.regs.get(l.int_raw);
                    self.regs.set(l.int_raw, raw & !delta.triggers);
                    step.irq[ch] = self.sync_irq(ch).or(step.irq[ch]);
                } else if i == l.int_ena {
                    step.irq[ch] = self.sync_irq(ch).or(step.irq[ch]);
                }
            }
        }
        step
    }

    /// The TX channel bound to peripheral code `peri` through `OUT_PERI_SEL`.
    pub fn tx_channel_of(&self, peri: u32) -> Option<usize> {
        (0..CHANNELS).find(|ch| self.regs.get(layout(*ch).out_peri_sel) & PERI_SEL == peri)
    }

    /// The RX channel bound to peripheral code `peri` through `IN_PERI_SEL`.
    pub fn rx_channel_of(&self, peri: u32) -> Option<usize> {
        (0..CHANNELS).find(|ch| self.regs.get(layout(*ch).in_peri_sel) & PERI_SEL == peri)
    }

    /// The descriptor the running TX walk of channel `ch` is at and the bytes already taken from
    /// it; `None` when stopped or at the chain's end. The I2S period sizes itself from it.
    pub fn tx_position(&self, ch: usize) -> Option<(u32, u32)> {
        let half = self.tx.get(ch)?;
        (half.running && half.cur != 0).then_some((half.cur, half.off))
    }

    /// [`Engine::tx_position`] for the RX walk: the descriptor and the bytes already written
    /// into it.
    pub fn rx_position(&self, ch: usize) -> Option<(u32, u32)> {
        let half = self.rx.get(ch)?;
        (half.running && half.cur != 0).then_some((half.cur, half.off))
    }

    /// Walks the outbound chain of channel `ch` for at most `max` bytes.
    ///
    /// It stops at the first `suc_eof` descriptor (the SPI rule); the I2S ring, where every
    /// descriptor carries EOF, continues from `next` on the next call. The owner bit clears only
    /// under `out_auto_wrback`, which neither driver sets.
    pub fn tx_pull(&mut self, ch: usize, max: u32, mem: &mut dyn DmaMem) -> TxPull {
        let l = layout(ch);
        let mut out = TxPull::default();
        let mut visits = max.saturating_add(DESCRIPTOR_SLACK);
        while self.tx[ch].running && (out.bytes.len() as u32) < max {
            let at = self.tx[ch].cur;
            if at == 0 {
                self.tx[ch].running = false;
                break;
            }
            let Some(left) = visits.checked_sub(1) else {
                self.regs.raise(l.int_raw, int::OUT_DSCR_ERR);
                self.tx[ch].running = false;
                break;
            };
            visits = left;
            let d = Descriptor::read(at, mem);
            self.regs.set(l.out_dscr, at);
            if self.checks_owner(l.out_conf1) && !d.owner {
                self.regs.raise(l.int_raw, int::OUT_DSCR_ERR);
                self.tx[ch].running = false;
                break;
            }
            let want = (max - out.bytes.len() as u32).min(d.length.saturating_sub(self.tx[ch].off));
            if want > 0 {
                let from = out.bytes.len();
                out.bytes.resize(from + want as usize, 0);
                mem.read(
                    d.buffer.wrapping_add(self.tx[ch].off),
                    &mut out.bytes[from..],
                );
                self.tx[ch].off += want;
            }
            if self.tx[ch].off < d.length {
                break;
            }
            self.tx[ch].off = 0;
            let mut bits = int::OUT_DONE;
            if d.suc_eof {
                // Only the EOF descriptor sets it: the TX ISR reads it to find the completed
                // buffer.
                self.regs.set(l.out_eof_des_addr, at);
                bits |= int::OUT_EOF | int::OUT_TOTAL_EOF;
                out.eof = true;
            }
            self.regs.raise(l.int_raw, bits);
            if self.regs.get(l.out_conf0) & OUT_AUTO_WRBACK != 0 {
                Descriptor { owner: false, ..d }.write_back(mem);
            }
            self.tx[ch].cur = d.next;
            self.tx[ch].running = d.next != 0;
            if d.suc_eof {
                break;
            }
        }
        out.irq = self.sync_irq(ch);
        out
    }

    /// Walks the inbound chain of channel `ch`, filling descriptor buffers with `data`.
    ///
    /// A descriptor completes when full, and the last one when `data` runs out (the end of the
    /// peripheral's period). A chain that runs out of descriptors raises `IN_DSCR_EMPTY`.
    pub fn rx_push(&mut self, ch: usize, data: &[u8], mem: &mut dyn DmaMem) -> RxPush {
        let l = layout(ch);
        let mut out = RxPush::default();
        let mut visits = u32::try_from(data.len())
            .unwrap_or(u32::MAX)
            .saturating_add(DESCRIPTOR_SLACK);
        while self.rx[ch].running && (out.taken as usize) < data.len() {
            let at = self.rx[ch].cur;
            if at == 0 {
                self.regs.raise(l.int_raw, int::IN_DSCR_EMPTY);
                self.rx[ch].running = false;
                break;
            }
            let Some(left) = visits.checked_sub(1) else {
                self.regs.raise(l.int_raw, int::IN_DSCR_ERR);
                self.rx[ch].running = false;
                break;
            };
            visits = left;
            let d = Descriptor::read(at, mem);
            self.regs.set(l.in_dscr, at);
            if self.checks_owner(l.in_conf1) && !d.owner {
                self.regs.raise(l.int_raw, int::IN_DSCR_ERR);
                self.rx[ch].running = false;
                break;
            }
            let room = d.size.saturating_sub(self.rx[ch].off);
            let n = room.min(data.len() as u32 - out.taken);
            if n > 0 {
                let from = out.taken as usize;
                mem.write(
                    d.buffer.wrapping_add(self.rx[ch].off),
                    &data[from..from + n as usize],
                );
                self.rx[ch].off += n;
                out.taken += n;
            }
            if self.rx[ch].off < d.size && (out.taken as usize) < data.len() {
                break;
            }
            let done = Descriptor {
                length: self.rx[ch].off,
                suc_eof: true,
                owner: false,
                ..d
            };
            done.write_back(mem);
            self.rx[ch].off = 0;
            self.regs.set(l.in_suc_eof_des_addr, at);
            self.regs.raise(l.int_raw, int::IN_DONE | int::IN_SUC_EOF);
            out.eof = true;
            self.rx[ch].cur = d.next;
            self.rx[ch].running = d.next != 0;
        }
        out.irq = self.sync_irq(ch);
        out
    }

    pub fn irq_level(&self, ch: usize) -> bool {
        self.regs.get(layout(ch).int_st) != 0
    }

    pub fn regs(&self) -> &RegFile<block::Gdma, REG_COUNT> {
        &self.regs
    }

    /// A `stop`, `start` or `restart` pulse, shifted so bit 0 is `stop`. `restart` resumes at the
    /// next descriptor after the last one finished; no driver uses it.
    fn link(&mut self, ch: usize, inbound: bool, pulses: u32) {
        let l = layout(ch);
        let (reg, half) = if inbound {
            (l.in_link, &mut self.rx[ch])
        } else {
            (l.out_link, &mut self.tx[ch])
        };
        let addr = dma_addr(self.regs.get(reg));
        if pulses & 0b001 != 0 {
            half.running = false;
        }
        if pulses & 0b010 != 0 {
            *half = Half {
                running: true,
                cur: addr,
                off: 0,
            };
        }
        if pulses & 0b100 != 0 {
            half.running = half.cur != 0;
        }
    }

    /// `*_check_owner`: neither driver sets it; a guest that does gets `*_DSCR_ERR` on a
    /// descriptor it does not own.
    fn checks_owner(&self, conf1: usize) -> bool {
        self.regs.get(conf1) & CHECK_OWNER != 0
    }

    /// Recomputes `INT_ST_CHn` and returns the channel's new level when it changed.
    fn sync_irq(&mut self, ch: usize) -> Option<bool> {
        let l = layout(ch);
        let st = self.regs.get(l.int_raw) & self.regs.get(l.int_ena);
        self.regs.set(l.int_st, st);
        let level = st != 0;
        (level != self.irq[ch]).then(|| {
            self.irq[ch] = level;
            level
        })
    }
}

/// Whether `mask` went from clear to set, which is how `gdma_reset` drives `*_rst`.
fn rising(before: u32, after: u32, mask: u32) -> bool {
    before & mask == 0 && after & mask != 0
}

impl Peripheral for Engine {
    const ID: PeriphId = <block::Gdma as Block>::ID;
    const BASE: u32 = <block::Gdma as Block>::BASE;
    const SIZE: u32 = BLOCK_SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.regs.reset(kind);
        self.rx = [Half::default(); CHANNELS];
        self.tx = [Half::default(); CHANNELS];
        for ch in 0..CHANNELS {
            if let Some(level) = self.sync_irq(ch) {
                cx.irq.set_source(source(ch), level);
            }
        }
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let step = self.store(off, size, val, cx.now, cx.ledger);
        for ch in 0..CHANNELS {
            if let Some(level) = step.irq[ch] {
                cx.irq.set_source(source(ch), level);
            }
        }
        RegWrite {
            stop: step.stop,
            wiring: Wiring::None,
        }
    }

    /// Never: `INT_RAW_CHn` changes when the bound peripheral runs a transfer, which is not an
    /// event of this block, so no bound this model could state would hold.
    fn stable_until(&self, _off: u32, _cx: &Cx) -> Stability {
        Stability::Never
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.regs.class(off)
    }
}

pub type Model = Engine;

#[cfg(test)]
mod tests {
    use crate::r#gen::waits;

    use super::*;

    const T: VTime = VTime(7);
    /// Base of the internal DRAM the descriptors and buffers live in.
    const DRAM: u32 = 0x3FC8_0000;

    /// A byte array behind [`DmaMem`], based at [`DRAM`]: addresses outside it read 0 and drop
    /// writes, as an unmapped DMA address does.
    struct Ram(Vec<u8>);

    impl Ram {
        fn new() -> Ram {
            Ram(vec![0; 0x4000])
        }

        fn slot(&self, addr: u32) -> Option<usize> {
            usize::try_from(addr.wrapping_sub(DRAM))
                .ok()
                .filter(|i| *i < self.0.len())
        }

        #[allow(clippy::too_many_arguments)]
        fn desc(
            &mut self,
            at: u32,
            size: u32,
            length: u32,
            suc_eof: bool,
            owner: bool,
            buffer: u32,
            next: u32,
        ) {
            let w0 = size | (length << 12) | (u32::from(suc_eof) << 30) | (u32::from(owner) << 31);
            for (i, word) in [w0, buffer, next].into_iter().enumerate() {
                self.write(at + 4 * i as u32, &word.to_le_bytes());
            }
        }

        fn word(&mut self, at: u32) -> u32 {
            let mut b = [0u8; 4];
            self.read(at, &mut b);
            u32::from_le_bytes(b)
        }

        fn fill(&mut self, at: u32, len: u32, seed: u8) {
            let bytes: Vec<u8> = (0..len).map(|i| seed.wrapping_add(i as u8)).collect();
            self.write(at, &bytes);
        }
    }

    impl DmaMem for Ram {
        fn read(&mut self, addr: u32, out: &mut [u8]) {
            for (i, b) in out.iter_mut().enumerate() {
                *b = self
                    .slot(addr.wrapping_add(i as u32))
                    .map_or(0, |i| self.0[i]);
            }
        }

        fn write(&mut self, addr: u32, data: &[u8]) {
            for (i, b) in data.iter().enumerate() {
                if let Some(i) = self.slot(addr.wrapping_add(i as u32)) {
                    self.0[i] = *b;
                }
            }
        }
    }

    fn off(reg: &str) -> u32 {
        u32::from(
            REGS.iter()
                .find(|spec| spec.name == reg)
                .unwrap_or_else(|| panic!("gdma has no register {reg}"))
                .off,
        )
    }

    /// One LVGL flush band: 9600 bytes in descriptors of 4092, 4092 and 1416 bytes, the last
    /// carrying `suc_eof`.
    fn flush_chain(ram: &mut Ram) -> [u32; 3] {
        let at = [DRAM, DRAM + 0x0C, DRAM + 0x18];
        let buf = [DRAM + 0x1000, DRAM + 0x2000, DRAM + 0x3000];
        let len = [4092, 4092, 1416];
        for i in 0..3 {
            let last = i == 2;
            ram.desc(
                at[i],
                len[i],
                len[i],
                last,
                true,
                buf[i],
                if last { 0 } else { at[i + 1] },
            );
            ram.fill(buf[i], len[i], (i as u8 + 1) * 0x10);
        }
        at
    }

    fn start_tx(g: &mut Engine, l: &mut FidelityLedger, ch: usize, peri: u32, at: u32) {
        let lay = layout(ch);
        g.store(off_of(lay.out_peri_sel), Size::B4, peri, T, l);
        g.store(
            off_of(lay.out_link),
            Size::B4,
            (at & LINK_ADDR) | (1 << 21),
            T,
            l,
        );
    }

    fn off_of(i: usize) -> u32 {
        u32::from(REGS[i].off)
    }

    #[test]
    fn the_layout_is_the_c3_one() {
        assert_eq!(CHANNELS, 3);
        assert_eq!(off_of(layout(0).int_raw), 0x000);
        assert_eq!(off_of(layout(1).int_raw), 0x010, "interrupt stride 0x10");
        assert_eq!(off_of(layout(2).int_raw), 0x020);
        assert_eq!(off_of(layout(0).int_clr), 0x00C);

        assert_eq!(off_of(layout(0).in_conf0), 0x070, "RX block base");
        assert_eq!(off_of(layout(0).out_conf0), 0x0D0, "TX block base");
        assert_eq!(off_of(layout(1).in_conf0), 0x130, "channel stride 0xC0");
        assert_eq!(off_of(layout(2).in_conf0), 0x1F0);
        assert_eq!(off_of(layout(1).out_conf0), 0x190);
        assert_eq!(off_of(layout(2).out_conf0), 0x250);

        type Pick = fn(Layout) -> usize;
        type Row = (&'static str, Pick, [u32; 3]);
        let rows: [Row; 6] = [
            ("IN_LINK", |l| l.in_link, [0x080, 0x140, 0x200]),
            (
                "IN_SUC_EOF_DES_ADDR",
                |l| l.in_suc_eof_des_addr,
                [0x088, 0x148, 0x208],
            ),
            ("IN_PERI_SEL", |l| l.in_peri_sel, [0x0A0, 0x160, 0x220]),
            ("OUT_LINK", |l| l.out_link, [0x0E0, 0x1A0, 0x260]),
            (
                "OUT_EOF_DES_ADDR",
                |l| l.out_eof_des_addr,
                [0x0E8, 0x1A8, 0x268],
            ),
            ("OUT_PERI_SEL", |l| l.out_peri_sel, [0x100, 0x1C0, 0x280]),
        ];
        for (name, pick, offs) in rows {
            for (ch, want) in offs.into_iter().enumerate() {
                assert_eq!(off_of(pick(layout(ch))), want, "{name} of CH{ch}");
            }
        }

        assert_eq!(source(0), irq::DMA_CH0);
        assert_eq!(source(1), irq::DMA_CH1);
        assert_eq!(source(2), irq::DMA_CH2);
        assert_eq!(source(0).0, 44, "pair 0 is source 44");
    }

    #[test]
    fn the_link_address_is_rebuilt_from_twenty_bits() {
        assert_eq!(dma_addr(0x8_0000), DRAM);
        assert_eq!(dma_addr(DRAM), DRAM, "idempotent inside the DRAM window");
        assert_eq!(dma_addr(0x3FCD_FFFC), 0x3FCD_FFFC);
        assert_eq!(dma_addr(0xFFFF_FFFF), 0x3FCF_FFFF);
    }

    #[test]
    fn a_tx_walk_follows_the_chain_to_its_eof_descriptor() {
        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        let at = flush_chain(&mut ram);
        start_tx(&mut g, &mut l, 0, PERI_SPI2, at[0]);

        let pull = g.tx_pull(0, 9600, &mut ram);
        assert_eq!(pull.bytes.len(), 9600);
        assert!(pull.eof, "the last descriptor carries suc_eof");
        assert_eq!(pull.irq, None, "INT_ENA is 0, so no source changed");
        assert_eq!(&pull.bytes[..4], &[0x10, 0x11, 0x12, 0x13]);
        assert_eq!(&pull.bytes[4092..4096], &[0x20, 0x21, 0x22, 0x23]);
        assert_eq!(&pull.bytes[8184..8188], &[0x30, 0x31, 0x32, 0x33]);

        let lay = layout(0);
        assert_eq!(g.regs().get(lay.out_eof_des_addr), at[2]);
        assert_eq!(g.regs().get(lay.out_dscr), at[2]);
        let raw = g.regs().get(lay.int_raw);
        assert_eq!(
            raw,
            int::OUT_DONE | int::OUT_EOF | int::OUT_TOTAL_EOF,
            "OUT_DONE per descriptor, EOF and TOTAL_EOF on the suc_eof one"
        );
        assert_eq!(
            ram.word(at[0]) >> 31,
            1,
            "owner is kept: out_auto_wrback is 0"
        );

        assert_eq!(g.tx_pull(0, 16, &mut ram).bytes, Vec::<u8>::new());
    }

    /// A peripheral that takes the chain in pieces sees every byte exactly once.
    #[test]
    fn a_short_tx_pull_resumes_inside_the_same_descriptor() {
        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        let at = flush_chain(&mut ram);
        start_tx(&mut g, &mut l, 0, PERI_SPI2, at[0]);

        let first = g.tx_pull(0, 4000, &mut ram);
        assert_eq!(first.bytes.len(), 4000);
        assert!(!first.eof);
        assert_eq!(g.regs().get(layout(0).int_raw), 0, "no descriptor finished");

        // The first descriptor finishes here without EOF, so `OUT_EOF_DES_ADDR` stays at reset.
        let second = g.tx_pull(0, 92, &mut ram);
        assert_eq!(second.bytes.len(), 92, "the rest of the first descriptor");
        assert_eq!(
            second.bytes[0],
            0x10u8.wrapping_add(4000u32 as u8),
            "the walk resumed inside the descriptor, at byte 4000"
        );
        assert!(!second.eof);
        assert_eq!(g.regs().get(layout(0).int_raw), int::OUT_DONE);
        assert_eq!(g.regs().get(layout(0).out_dscr), at[0]);
        assert_eq!(
            g.regs().get(layout(0).out_eof_des_addr),
            0,
            "a descriptor without suc_eof does not name itself as the EOF descriptor"
        );

        let rest = g.tx_pull(0, 9600, &mut ram);
        assert_eq!(rest.bytes.len(), 5508, "the other two descriptors");
        assert!(rest.eof);
        assert_eq!(rest.bytes[0], 0x20, "and it continued at the next one");
        assert_eq!(
            g.regs().get(layout(0).out_eof_des_addr),
            at[2],
            "only the suc_eof descriptor names itself"
        );
    }

    /// A zero-length cyclic chain must end the walk rather than spin inside one MMIO write. This
    /// test hangs the suite if the bound is ever removed, which is the point.
    #[test]
    fn a_cyclic_chain_of_empty_descriptors_ends_the_walk_instead_of_spinning() {
        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        let lay = layout(0);
        ram.desc(DRAM, 0, 0, false, true, DRAM + 0x1000, DRAM);
        start_tx(&mut g, &mut l, 0, PERI_SPI2, DRAM);

        let pull = g.tx_pull(0, 16, &mut ram);
        assert_eq!(pull.bytes, Vec::<u8>::new(), "nothing was ever moved");
        assert_eq!(
            g.regs().get(lay.int_raw) & int::OUT_DSCR_ERR,
            int::OUT_DSCR_ERR,
            "the malformed chain ends the transfer"
        );
        assert_eq!(
            g.tx_pull(0, 16, &mut ram).bytes,
            Vec::<u8>::new(),
            "and the walk stays stopped"
        );

        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        let lay = layout(1);
        ram.desc(DRAM, 0, 0, false, true, DRAM + 0x1000, DRAM);
        g.store(
            off_of(lay.in_link),
            Size::B4,
            (DRAM & LINK_ADDR) | (1 << 22),
            T,
            &mut l,
        );
        let push = g.rx_push(1, &[1, 2, 3, 4, 5, 6, 7, 8], &mut ram);
        assert_eq!(push.taken, 0);
        assert_eq!(
            g.regs().get(lay.int_raw) & int::IN_DSCR_ERR,
            int::IN_DSCR_ERR
        );
        assert_eq!(g.rx_push(1, &[9], &mut ram).taken, 0, "stopped");
    }

    /// The bound can only fire on a chain that moves nothing.
    #[test]
    fn a_finely_split_chain_is_not_cut_short_by_the_bound() {
        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        let n = DESCRIPTOR_SLACK * 4;
        for i in 0..n {
            let at = DRAM + 0x0C * i;
            let last = i == n - 1;
            let next = if last { 0 } else { at + 0x0C };
            ram.desc(at, 1, 1, last, true, DRAM + 0x1000 + i, next);
        }
        ram.fill(DRAM + 0x1000, n, 0x40);
        start_tx(&mut g, &mut l, 0, PERI_SPI2, DRAM);

        let pull = g.tx_pull(0, n, &mut ram);
        assert_eq!(pull.bytes.len(), n as usize, "every descriptor was walked");
        assert!(pull.eof);
        assert_eq!(
            g.regs().get(layout(0).int_raw) & int::OUT_DSCR_ERR,
            0,
            "a chain that moves bytes never hits the bound"
        );
    }

    #[test]
    fn both_descriptor_address_words_are_rebuilt_and_a_zero_next_stays_zero() {
        let mut ram = Ram::new();
        ram.desc(DRAM, 4, 4, false, true, 0x8_1000, 0x8_2000);
        let d = Descriptor::read(DRAM, &mut ram);
        assert_eq!(d.buffer, DRAM + 0x1000, "the buffer word is rebuilt");
        assert_eq!(d.next, DRAM + 0x2000, "and so is the next word");

        ram.desc(DRAM + 0x0C, 4, 4, true, true, 0x8_1000, 0);
        assert_eq!(Descriptor::read(DRAM + 0x0C, &mut ram).next, 0);
    }

    #[test]
    fn a_transfer_finds_its_channel_by_peri_sel() {
        let (mut g, mut l) = (Engine::default(), FidelityLedger::default());
        assert_eq!(g.tx_channel_of(PERI_SPI2), None, "nothing is connected yet");
        for ch in 0..CHANNELS {
            assert_eq!(g.regs().get(layout(ch).out_peri_sel) & PERI_SEL, PERI_NONE);
        }
        g.store(
            off_of(layout(1).out_peri_sel),
            Size::B4,
            PERI_SPI2,
            T,
            &mut l,
        );
        g.store(
            off_of(layout(2).out_peri_sel),
            Size::B4,
            PERI_I2S0,
            T,
            &mut l,
        );
        g.store(
            off_of(layout(2).in_peri_sel),
            Size::B4,
            PERI_I2S0,
            T,
            &mut l,
        );
        assert_eq!(g.tx_channel_of(PERI_SPI2), Some(1));
        assert_eq!(g.tx_channel_of(PERI_I2S0), Some(2));
        assert_eq!(g.rx_channel_of(PERI_I2S0), Some(2));
        assert_eq!(g.rx_channel_of(PERI_SPI2), None, "SPI2 RX is not connected");
    }

    /// `gdma_reset` (`*_rst` 1 then 0) ends the walk.
    #[test]
    fn the_link_pulses_read_back_zero_and_a_reset_ends_the_walk() {
        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        let at = flush_chain(&mut ram);
        let lay = layout(0);
        start_tx(&mut g, &mut l, 0, PERI_SPI2, at[0]);
        assert_eq!(
            g.load(off_of(lay.out_link), Size::B4, T, &mut l),
            at[0] & LINK_ADDR,
            "the address stays, the start pulse does not"
        );

        g.store(off_of(lay.out_conf0), Size::B4, CONF0_RST, T, &mut l);
        g.store(off_of(lay.out_conf0), Size::B4, 0, T, &mut l);
        assert_eq!(g.tx_pull(0, 16, &mut ram).bytes, Vec::<u8>::new());

        start_tx(&mut g, &mut l, 0, PERI_SPI2, at[0]);
        g.store(off_of(lay.out_link), Size::B4, 1 << 20, T, &mut l);
        assert_eq!(
            g.tx_pull(0, 16, &mut ram).bytes,
            Vec::<u8>::new(),
            "stopped"
        );
    }

    #[test]
    fn an_unowned_descriptor_raises_out_dscr_err_when_the_owner_is_checked() {
        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        let lay = layout(0);
        ram.desc(DRAM, 16, 16, true, false, DRAM + 0x1000, 0);
        ram.fill(DRAM + 0x1000, 16, 0xA0);

        g.store(off_of(lay.out_conf1), Size::B4, CHECK_OWNER, T, &mut l);
        start_tx(&mut g, &mut l, 0, PERI_SPI2, DRAM);
        let pull = g.tx_pull(0, 16, &mut ram);
        assert_eq!(pull.bytes, Vec::<u8>::new());
        assert_eq!(g.regs().get(lay.int_raw), int::OUT_DSCR_ERR);

        // Without the check, the same descriptor is walked.
        let (mut g, mut l) = (Engine::default(), FidelityLedger::default());
        start_tx(&mut g, &mut l, 0, PERI_SPI2, DRAM);
        assert_eq!(g.tx_pull(0, 16, &mut ram).bytes.len(), 16);
    }

    /// `IN_SUC_EOF` is what the `gdma.i2s_in_suc_eof` row polls.
    #[test]
    fn an_rx_walk_writes_the_descriptor_back_and_raises_in_suc_eof() {
        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        let lay = layout(1);
        ram.desc(DRAM, 8, 0, false, true, DRAM + 0x1000, DRAM + 0x0C);
        ram.desc(DRAM + 0x0C, 8, 0, false, true, DRAM + 0x2000, 0);

        g.store(off_of(lay.in_peri_sel), Size::B4, PERI_I2S0, T, &mut l);
        g.store(
            off_of(lay.in_link),
            Size::B4,
            (DRAM & LINK_ADDR) | (1 << 22),
            T,
            &mut l,
        );
        g.store(off_of(lay.int_ena), Size::B4, int::IN_SUC_EOF, T, &mut l);

        let push = g.rx_push(1, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], &mut ram);
        assert_eq!(push.taken, 10);
        assert!(push.eof);
        assert_eq!(push.irq, Some(true), "source 45 rose once");

        let mut first = [0u8; 8];
        ram.read(DRAM + 0x1000, &mut first);
        assert_eq!(first, [1, 2, 3, 4, 5, 6, 7, 8]);
        let mut second = [0u8; 2];
        ram.read(DRAM + 0x2000, &mut second);
        assert_eq!(second, [9, 10]);

        assert_eq!((ram.word(DRAM) >> 12) & 0xFFF, 8, "length written back");
        assert_eq!((ram.word(DRAM) >> 30) & 1, 1, "suc_eof written back");
        assert_eq!(
            (ram.word(DRAM + 0x0C) >> 12) & 0xFFF,
            2,
            "the short last descriptor is completed too"
        );
        assert_eq!(g.regs().get(lay.in_suc_eof_des_addr), DRAM + 0x0C);
        assert_eq!(
            g.regs().get(lay.int_raw) & (int::IN_DONE | int::IN_SUC_EOF),
            int::IN_DONE | int::IN_SUC_EOF
        );

        let empty = g.rx_push(1, &[11], &mut ram);
        assert_eq!(empty.taken, 0, "the chain ended");
    }

    #[test]
    fn int_st_is_raw_and_ena_and_drives_the_channel_source() {
        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        let at = flush_chain(&mut ram);
        let lay = layout(0);
        g.store(off_of(lay.int_ena), Size::B4, int::OUT_EOF, T, &mut l);
        start_tx(&mut g, &mut l, 0, PERI_SPI2, at[0]);

        assert!(!g.irq_level(0));
        let pull = g.tx_pull(0, 9600, &mut ram);
        assert_eq!(pull.irq, Some(true));
        assert!(g.irq_level(0));
        assert_eq!(
            g.load(off_of(lay.int_st), Size::B4, T, &mut l),
            int::OUT_EOF
        );

        let step = g.store(off_of(lay.int_clr), Size::B4, int::OUT_EOF, T, &mut l);
        assert_eq!(step.irq[0], Some(false));
        assert_eq!(step.irq[1], None, "only channel 0 changed");
        assert!(!g.irq_level(0));
        assert_eq!(
            g.load(off_of(lay.int_raw), Size::B4, T, &mut l),
            int::OUT_DONE | int::OUT_TOTAL_EOF,
            "INT_CLR clears only the bits written 1"
        );
    }

    #[test]
    fn auto_wrback_clears_the_descriptor_owner_bit() {
        let (mut g, mut l, mut ram) = (Engine::default(), FidelityLedger::default(), Ram::new());
        ram.desc(DRAM, 4, 4, true, true, DRAM + 0x1000, 0);
        g.store(
            off_of(layout(0).out_conf0),
            Size::B4,
            OUT_AUTO_WRBACK,
            T,
            &mut l,
        );
        start_tx(&mut g, &mut l, 0, PERI_SPI2, DRAM);
        g.tx_pull(0, 4, &mut ram);
        assert_eq!(ram.word(DRAM) >> 31, 0);
    }

    /// The largest generated table in the crate: every one of its rows must be findable.
    #[test]
    fn the_register_lookup_finds_every_row_of_the_generated_table() {
        for (i, spec) in REGS.iter().enumerate() {
            let found = RegFile::<block::Gdma, REG_COUNT>::index_of(u32::from(spec.off));
            assert_eq!(found, Some(i), "{}", spec.name);
        }
        assert_eq!(RegFile::<block::Gdma, REG_COUNT>::index_of(0x400), None);
    }

    #[test]
    fn every_row_of_the_block_file_names_a_register_and_field_of_the_table() {
        let rows: Vec<_> = waits::of_block("gdma").collect();
        assert_eq!(rows.len(), 2, "the two I2S EOF rows");
        for row in rows {
            let spec = REGS
                .iter()
                .find(|spec| spec.name == row.register)
                .unwrap_or_else(|| panic!("{}: no register {}", row.id, row.register));
            assert!(
                spec.fields.iter().any(|f| f.name == row.field),
                "{}: no field {}",
                row.id,
                row.field
            );
            assert_eq!(u32::from(spec.off), off("GDMA_INT_RAW_CH0"));
        }
    }

    /// At class U the hang detector would take a poll of channel 1's `OUT_EOF` as a loop on a
    /// register nothing can change, because the event that sets it is I2S0's.
    #[test]
    fn every_channels_int_raw_is_a_modeled_stable_read() {
        let e = Engine::default();
        for ch in 0..CHANNELS {
            let spec = &REGS[layout(ch).int_raw];
            assert!(spec.stable_read, "{}", spec.name);
            assert_eq!(
                e.fidelity(u32::from(spec.off)),
                Fidelity::B,
                "{}",
                spec.name
            );
        }
    }
}
