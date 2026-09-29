//! The SHA accelerator, block mode and DMA mode (`specs/blocks/sha.toml`, TRM 21). The ROM loader
//! and the bootloader hash every segment through it, so the digest must be bit-exact: a wrong
//! byte makes the bootloader refuse the app.
//!
//! `START` loads the initial state for `MODE` and compresses `TEXT`; `CONTINUE` compresses into
//! the state in `H`. Padding is the caller's.
//!
//! Byte order: `TEXT` holds the stream as little-endian words, so message word *j* is `TEXT[j]`
//! reversed; `H[i]` is state word *i* reversed, so copying `H` out little-endian yields the
//! big-endian digest. Proven against the ROM and IDF callers, UNVERIFIED on silicon beyond that.
//!
//! DMA mode (mbedTLS above `SHA_DMA_MODE_THRESHOLD`, 128 bytes; IDF `hal/sha_hal.c`
//! `sha_hal_hash_dma`): a write of 1 to `DMA_START` or `DMA_CONTINUE` sets `BUSY` and returns
//! `Wiring::ShaDma`. `crate::wiring::sha` pulls `BLOCK_NUM x 64` bytes from the TX descriptor walk
//! into [`Sha::dma_feed`], which schedules completion `blocks x sha_block_ps` later. Completion
//! clears `BUSY` and latches the interrupt (source 49 while `INT_ENA` bit 0) until `CLEAR_IRQ`.
//! Block and DMA runs share one state. Block mode raises no interrupt.
//!
//! UNVERIFIED (class C): a short chain compresses the whole blocks it delivered and leaves `BUSY`
//! set, which the hang detector reports on `sha.busy`; `BLOCK_NUM` 0 completes at once; a DMA
//! trigger while `BUSY` is ignored; a DMA run leaves `TEXT` alone; the interrupt latches whatever
//! `INT_ENA` says, the enable gating only the level.

use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::regstore::Size;
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::{Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};

/// Block offsets (IDF `soc/esp32c3/include/soc/hwcrypto_reg.h`).
pub mod reg {
    /// Algorithm select.
    pub const MODE: u32 = 0x00;
    /// Block count of a DMA run.
    pub const BLOCK_NUM: u32 = 0x0C;
    /// Load the initial state and compress `TEXT`.
    pub const START: u32 = 0x10;
    /// Compress `TEXT` into the current state.
    pub const CONTINUE: u32 = 0x14;
    /// 0 once the digest is computed.
    pub const BUSY: u32 = 0x18;
    /// DMA run, from the initial state.
    pub const DMA_START: u32 = 0x1C;
    /// DMA run, from the current state.
    pub const DMA_CONTINUE: u32 = 0x20;
    /// Clear the completion interrupt.
    pub const CLEAR_IRQ: u32 = 0x24;
    /// Enable the completion interrupt (source 49).
    pub const INT_ENA: u32 = 0x28;
    pub const H: u32 = 0x40;
    pub const H_END: u32 = 0x60;
    pub const TEXT: u32 = 0x80;
    pub const TEXT_END: u32 = 0xC0;
}

pub const MODE_SHA1: u32 = 0;
pub const MODE_SHA224: u32 = 1;
pub const MODE_SHA256: u32 = 2;

pub const TAG_DONE: u16 = 0;

/// Event tag of the completion of a DMA run: clears `BUSY` and latches the interrupt.
pub const TAG_DMA_DONE: u16 = 1;

/// `BLOCK_NUM` bits 5 to 0: a DMA run is at most 63 blocks.
pub const BLOCK_NUM_MASK: u32 = 0x3F;

/// Bytes of one message block (FIPS 180-4 §5.2.1).
pub const BLOCK_BYTES: u32 = 64;

/// A DMA run a trigger asked for and the wiring has not yet fed.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct DmaRun {
    /// `DMA_START`: load the initial state of `MODE` first. `DMA_CONTINUE` leaves `H` as it is.
    pub restart: bool,
    /// Blocks to compress, `BLOCK_NUM` at the trigger.
    pub blocks: u32,
}

impl DmaRun {
    /// Bytes the run reads from the TX descriptor walk.
    pub fn bytes(&self) -> u32 {
        self.blocks * BLOCK_BYTES
    }
}

/// What one register write asks of its caller.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Stored {
    /// The write scheduled an event or started a DMA run, so the run loop must stop after it.
    pub stop: bool,
    /// The write started a DMA run: return `Wiring::ShaDma`.
    pub dma: bool,
}

/// `sha_block_ps` of the `device` timing profile: 2.8 us per block, UNVERIFIED. The `fast`
/// profile is 0, which is [`Sha`]'s default.
pub const DEVICE_BLOCK_PS: u64 = 2_800_000;

/// State words of one compression: 5 for SHA-1, 8 for the SHA-2 modes.
const STATE_WORDS: usize = 8;
const BLOCK_WORDS: usize = 16;

/// SHA-1 initial state (FIPS 180-4 §5.3.1).
const IV_SHA1: [u32; STATE_WORDS] = [
    0x6745_2301,
    0xEFCD_AB89,
    0x98BA_DCFE,
    0x1032_5476,
    0xC3D2_E1F0,
    0,
    0,
    0,
];

/// SHA-224 initial state (FIPS 180-4 §5.3.2).
const IV_SHA224: [u32; STATE_WORDS] = [
    0xC105_9ED8,
    0x367C_D507,
    0x3070_DD17,
    0xF70E_5939,
    0xFFC0_0B31,
    0x6858_1511,
    0x64F9_8FA7,
    0xBEFA_4FA4,
];

/// SHA-256 initial state (FIPS 180-4 §5.3.3).
const IV_SHA256: [u32; STATE_WORDS] = [
    0x6A09_E667,
    0xBB67_AE85,
    0x3C6E_F372,
    0xA54F_F53A,
    0x510E_527F,
    0x9B05_688C,
    0x1F83_D9AB,
    0x5BE0_CD19,
];

/// SHA-224 and SHA-256 round constants (FIPS 180-4 §4.2.2).
const K256: [u32; 64] = [
    0x428A_2F98,
    0x7137_4491,
    0xB5C0_FBCF,
    0xE9B5_DBA5,
    0x3956_C25B,
    0x59F1_11F1,
    0x923F_82A4,
    0xAB1C_5ED5,
    0xD807_AA98,
    0x1283_5B01,
    0x2431_85BE,
    0x550C_7DC3,
    0x72BE_5D74,
    0x80DE_B1FE,
    0x9BDC_06A7,
    0xC19B_F174,
    0xE49B_69C1,
    0xEFBE_4786,
    0x0FC1_9DC6,
    0x240C_A1CC,
    0x2DE9_2C6F,
    0x4A74_84AA,
    0x5CB0_A9DC,
    0x76F9_88DA,
    0x983E_5152,
    0xA831_C66D,
    0xB003_27C8,
    0xBF59_7FC7,
    0xC6E0_0BF3,
    0xD5A7_9147,
    0x06CA_6351,
    0x1429_2967,
    0x27B7_0A85,
    0x2E1B_2138,
    0x4D2C_6DFC,
    0x5338_0D13,
    0x650A_7354,
    0x766A_0ABB,
    0x81C2_C92E,
    0x9272_2C85,
    0xA2BF_E8A1,
    0xA81A_664B,
    0xC24B_8B70,
    0xC76C_51A3,
    0xD192_E819,
    0xD699_0624,
    0xF40E_3585,
    0x106A_A070,
    0x19A4_C116,
    0x1E37_6C08,
    0x2748_774C,
    0x34B0_BCB5,
    0x391C_0CB3,
    0x4ED8_AA4A,
    0x5B9C_CA4F,
    0x682E_6FF3,
    0x748F_82EE,
    0x78A5_636F,
    0x84C8_7814,
    0x8CC7_0208,
    0x90BE_FFFA,
    0xA450_6CEB,
    0xBEF9_A3F7,
    0xC671_78F2,
];

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Sha {
    mode: u32,
    block_num: u32,
    int_ena: u32,
    /// Compression state, in the standard's word order and orientation.
    state: [u32; STATE_WORDS],
    /// The message block as the guest wrote it: `TEXT` words, little-endian views of the stream.
    text: [u32; BLOCK_WORDS],
    busy: bool,
    dma: Option<DmaRun>,
    /// The DMA completion interrupt, latched until `CLEAR_IRQ`.
    irq_raw: bool,
    block_ps: u64,
    #[serde(
        deserialize_with = "pemu_core::snap::exact_vec::<_, _, { (Sha::SIZE.div_ceil(4) as usize).div_ceil(64) }>"
    )]
    touched: Vec<u64>,
}

impl Default for Sha {
    fn default() -> Sha {
        Sha {
            mode: 0,
            block_num: 0,
            int_ena: 0,
            state: [0; STATE_WORDS],
            text: [0; BLOCK_WORDS],
            busy: false,
            dma: None,
            irq_raw: false,
            block_ps: 0,
            touched: vec![0; (Sha::SIZE.div_ceil(4) as usize).div_ceil(64)],
        }
    }
}

impl Sha {
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// The compression state, in the standard's orientation rather than the register one.
    pub fn state(&self) -> [u32; STATE_WORDS] {
        self.state
    }

    pub fn set_block_ps(&mut self, ps: u64) {
        self.block_ps = ps;
    }

    /// The digest so far as the standard's byte string. The caller takes the 20, 28 or 32 bytes
    /// its mode defines.
    pub fn digest(&self) -> [u8; STATE_WORDS * 4] {
        let mut out = [0u8; STATE_WORDS * 4];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.touch(off, TouchAccess::Read, size, now, ledger);
        let (mask, shift) = window(off, size);
        (self.word(off & !3) & mask) >> shift
    }

    /// Writes the low `size` bytes of `val` at `off` and runs whatever a trigger started. `stop`
    /// is set when the write scheduled an event or started a DMA run, so the run loop recomputes
    /// its budget and `BUSY` cannot clear a slice late.
    #[must_use]
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
        sched: &mut Scheduler,
    ) -> Stored {
        self.touch(off, TouchAccess::Write, size, now, ledger);
        let aligned = off & !3;
        let (mask, shift) = window(off, size);
        let written = ((u64::from(val) << shift) as u32) & mask;
        let merged = (self.word(aligned) & !mask) | written;
        let mut out = Stored::default();
        match aligned {
            reg::MODE => self.mode = merged,
            // Bits 31 to 6 are reserved and read 0.
            reg::BLOCK_NUM => self.block_num = merged & BLOCK_NUM_MASK,
            reg::INT_ENA => self.int_ena = merged,
            reg::START | reg::CONTINUE if written & 1 != 0 => {
                self.compress(aligned == reg::START);
                self.busy = true;
                // The digest is ready, but BUSY is state the guest polls, so even an immediate
                // completion is an event at `now`.
                self.schedule_done(now, self.block_ps, TAG_DONE, sched);
                out.stop = true;
            }
            reg::DMA_START | reg::DMA_CONTINUE if written & 1 != 0 && !self.busy => {
                self.busy = true;
                self.dma = Some(DmaRun {
                    restart: aligned == reg::DMA_START,
                    blocks: self.block_num & BLOCK_NUM_MASK,
                });
                out = Stored {
                    stop: true,
                    dma: true,
                };
            }
            reg::CLEAR_IRQ if written & 1 != 0 => self.irq_raw = false,
            reg::TEXT..reg::TEXT_END => {
                self.text[((aligned - reg::TEXT) / 4) as usize] = merged;
            }
            reg::H..reg::H_END => {
                // Swapped on write as well as on read, so a resumed digest writes back exactly
                // what it read.
                self.state[((aligned - reg::H) / 4) as usize] = merged.swap_bytes();
            }
            _ => {}
        }
        out
    }

    pub fn take_dma(&mut self) -> Option<DmaRun> {
        self.dma.take()
    }

    /// Compresses the bytes the TX walk delivered for `run` and schedules its completion. Returns
    /// false for a short walk, which leaves `BUSY` set (UNVERIFIED).
    pub fn dma_feed(
        &mut self,
        run: DmaRun,
        data: &[u8],
        now: VTime,
        sched: &mut Scheduler,
    ) -> bool {
        if run.restart {
            self.load_iv();
        }
        let usable = data.len().min(run.bytes() as usize);
        for chunk in data[..usable].chunks_exact(BLOCK_BYTES as usize) {
            let mut block = [0u32; BLOCK_WORDS];
            for (out, word) in block.iter_mut().zip(chunk.chunks_exact(4)) {
                *out = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            self.compress_block(&block);
        }
        if usable < run.bytes() as usize {
            return false;
        }
        let ps = self.block_ps.saturating_mul(u64::from(run.blocks));
        self.schedule_done(now, ps, TAG_DMA_DONE, sched);
        true
    }

    /// The completion of one block: `BUSY` clears.
    pub fn complete(&mut self) {
        self.busy = false;
    }

    /// The completion of a DMA run. One that finds the block idle was overtaken by a reset (the
    /// scheduler does not cancel it) and latches nothing.
    pub fn complete_dma(&mut self) {
        if self.busy {
            self.busy = false;
            self.irq_raw = true;
        }
    }

    /// Level of source 49: the latched completion while `INT_ENA` bit 0 is set.
    pub fn irq_level(&self) -> bool {
        self.irq_raw && self.int_ena & 1 != 0
    }

    pub fn sync_irq(&self, irq: &mut crate::intc::IrqFabric) {
        irq.set_source(pemu_core::irq_source::irq::SHA, self.irq_level());
    }

    fn schedule_done(&self, now: VTime, ps: u64, tag: u16, sched: &mut Scheduler) {
        sched.schedule(
            now,
            VTime(now.0.saturating_add(ps)),
            EventKey {
                owner: Owner::Periph(Sha::ID),
                tag,
            },
        );
    }

    /// Clears the block for a reset that reaches it, keeping the timing profile and the
    /// first-touch state, which are per machine.
    pub fn apply_reset(&mut self, kind: ResetKind) {
        if !kind.clears(pemu_core::regstore::RESET_BY_ALL_SCOPES) {
            return;
        }
        let touched = std::mem::take(&mut self.touched);
        let block_ps = self.block_ps;
        *self = Sha {
            touched,
            block_ps,
            ..Sha::default()
        };
    }

    fn word(&self, at: u32) -> u32 {
        match at {
            reg::MODE => self.mode,
            reg::BLOCK_NUM => self.block_num,
            reg::BUSY => u32::from(self.busy),
            reg::INT_ENA => self.int_ena,
            reg::TEXT..reg::TEXT_END => self.text[((at - reg::TEXT) / 4) as usize],
            // The register holds the state word reversed.
            reg::H..reg::H_END => self.state[((at - reg::H) / 4) as usize].swap_bytes(),
            // Write-only triggers and unnamed offsets read 0.
            _ => 0,
        }
    }

    /// Compresses the block in `TEXT`, loading the initial state for `MODE` first when `restart`.
    fn compress(&mut self, restart: bool) {
        if restart {
            self.load_iv();
        }
        // TEXT holds little-endian words and the standard reads big-endian.
        let mut block = [0u32; BLOCK_WORDS];
        for (out, word) in block.iter_mut().zip(self.text.iter()) {
            *out = word.swap_bytes();
        }
        self.compress_block(&block);
    }

    fn load_iv(&mut self) {
        self.state = match self.mode {
            MODE_SHA1 => IV_SHA1,
            MODE_SHA224 => IV_SHA224,
            // Values other than the three modes are UNVERIFIED; SHA-256 is what every caller uses.
            _ => IV_SHA256,
        };
    }

    /// One compression of `block` (message words in the standard's orientation) with the
    /// function `MODE` selects.
    fn compress_block(&mut self, block: &[u32; BLOCK_WORDS]) {
        if self.mode == MODE_SHA1 {
            sha1_block(&mut self.state, block);
        } else {
            sha256_block(&mut self.state, block);
        }
    }

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
            periph: Sha::ID,
            off: off & !3,
            access,
            size: size as u8,
            now,
            allowlisted: false,
        });
    }
}

fn window(off: u32, size: Size) -> (u32, u32) {
    let shift = (off % 4) * 8;
    let bits = (size as u32) * 8;
    let mask = (((1u64 << bits) - 1) << shift) as u32;
    (mask, shift)
}

/// One SHA-1 compression (FIPS 180-4 §6.1.2).
fn sha1_block(state: &mut [u32; STATE_WORDS], block: &[u32; BLOCK_WORDS]) {
    let mut w = [0u32; 80];
    w[..BLOCK_WORDS].copy_from_slice(block);
    for t in BLOCK_WORDS..80 {
        w[t] = (w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = [state[0], state[1], state[2], state[3], state[4]];
    for (t, word) in w.iter().enumerate() {
        let (f, k) = match t {
            0..20 => ((b & c) | (!b & d), 0x5A82_7999),
            20..40 => (b ^ c ^ d, 0x6ED9_EBA1),
            40..60 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6),
        };
        let tmp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = tmp;
    }
    for (slot, add) in state.iter_mut().zip([a, b, c, d, e]) {
        *slot = slot.wrapping_add(add);
    }
}

/// One SHA-224 or SHA-256 compression; the two differ only in the initial state
/// (FIPS 180-4 §6.2.2).
fn sha256_block(state: &mut [u32; STATE_WORDS], block: &[u32; BLOCK_WORDS]) {
    let mut w = [0u32; 64];
    w[..BLOCK_WORDS].copy_from_slice(block);
    for t in BLOCK_WORDS..64 {
        let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
        let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
        w[t] = w[t - 16]
            .wrapping_add(s0)
            .wrapping_add(w[t - 7])
            .wrapping_add(s1);
    }
    let mut v = *state;
    for (word, k) in w.iter().zip(K256.iter()) {
        let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
        let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
        let t1 = v[7]
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(*k)
            .wrapping_add(*word);
        let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
        let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
        let t2 = s0.wrapping_add(maj);
        v = [
            t1.wrapping_add(t2),
            v[0],
            v[1],
            v[2],
            v[3].wrapping_add(t1),
            v[4],
            v[5],
            v[6],
        ];
    }
    for (slot, add) in state.iter_mut().zip(v) {
        *slot = slot.wrapping_add(add);
    }
}

impl Peripheral for Sha {
    const ID: PeriphId = super::id::SHA;
    const BASE: u32 = 0x6003_B000;
    const SIZE: u32 = 0x1000;

    /// Every reset scope clears SHA. The IDF reset of this block also resets DS and HMAC, which
    /// are store-only rows of their own.
    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.apply_reset(kind);
        self.sync_irq(cx.irq);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    /// A completion-scheduling write returns `stop`, so the event fires at the next instruction
    /// boundary rather than a slice late. A DMA trigger also returns `Wiring::ShaDma`.
    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let stored = self.store(off, size, val, cx.now, cx.ledger, cx.sched);
        self.sync_irq(cx.irq);
        RegWrite {
            stop: stored.stop,
            wiring: if stored.dma {
                Wiring::ShaDma
            } else {
                Wiring::None
            },
        }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        match tag {
            TAG_DONE => self.complete(),
            TAG_DMA_DONE => {
                self.complete_dma();
                self.sync_irq(cx.irq);
            }
            _ => {}
        }
        Wiring::None
    }

    /// `BUSY` is cleared by the completion event and nothing else, so a poll of it can be
    /// fast-forwarded to that event.
    fn stable_until(&self, off: u32, cx: &Cx) -> Stability {
        let _ = cx;
        match off & !3 {
            reg::BUSY => Stability::UntilNextEvent,
            _ => Stability::Never,
        }
    }

    /// The class `specs/blocks/sha.toml` gives the register at `off`, `U` for the rest. The block
    /// has no rows in `specs/c3-registers.csv`, so codegen renders its classes into
    /// [`crate::gen::classes::sha`].
    fn fidelity(&self, off: u32) -> Fidelity {
        crate::r#gen::classes::sha::class_at(off)
    }
}

pub type Model = Sha;

#[cfg(test)]
mod tests {
    use super::*;

    struct Host {
        sha: Sha,
        sched: Scheduler,
        ledger: FidelityLedger,
        now: VTime,
        stopped: bool,
    }

    impl Host {
        fn new() -> Host {
            Host {
                sha: Sha::default(),
                sched: Scheduler::default(),
                ledger: FidelityLedger::default(),
                now: VTime(0),
                stopped: false,
            }
        }

        fn read(&mut self, off: u32) -> u32 {
            self.sha.load(off, Size::B4, self.now, &mut self.ledger)
        }

        fn write(&mut self, off: u32, val: u32) {
            self.stopped = self
                .sha
                .store(
                    off,
                    Size::B4,
                    val,
                    self.now,
                    &mut self.ledger,
                    &mut self.sched,
                )
                .stop;
        }

        fn pump(&mut self) {
            if let Some(t) = self.sched.next_time() {
                self.now = self.now.max(t);
            }
            while let Some(key) = self.sched.pop_due(self.now) {
                assert_eq!(key.owner, Owner::Periph(Sha::ID));
                assert_eq!(key.tag, TAG_DONE);
                self.sha.complete();
            }
        }

        /// The register sequence of `bootloader_sha.c` through ROM `ets_sha_update`: per padded
        /// block, write `TEXT`, trigger `START` then `CONTINUE`, poll `BUSY`.
        fn hash(&mut self, mode: u32, message: &[u8]) -> Vec<u8> {
            self.write(reg::MODE, mode);
            let mut padded = message.to_vec();
            padded.push(0x80);
            while padded.len() % 64 != 56 {
                padded.push(0);
            }
            padded.extend_from_slice(&(message.len() as u64 * 8).to_be_bytes());
            for (n, block) in padded.chunks(64).enumerate() {
                for (i, word) in block.chunks(4).enumerate() {
                    let bytes: [u8; 4] = word.try_into().expect("a 64-byte block splits evenly");
                    self.write(reg::TEXT + 4 * i as u32, u32::from_le_bytes(bytes));
                }
                self.write(if n == 0 { reg::START } else { reg::CONTINUE }, 1);
                assert_eq!(self.read(reg::BUSY), 1, "BUSY is set until the completion");
                self.pump();
                assert_eq!(self.read(reg::BUSY), 0);
            }
            let mut out = Vec::new();
            for i in 0..(reg::H_END - reg::H) / 4 {
                out.extend_from_slice(&self.read(reg::H + 4 * i).to_le_bytes());
            }
            out.truncate(match mode {
                MODE_SHA1 => 20,
                MODE_SHA224 => 28,
                _ => 32,
            });
            out
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// FIPS 180-4 vectors, compared as the byte string the ROM compares.
    #[test]
    fn the_standard_vectors_come_out_of_the_h_window_byte_for_byte() {
        let mut h = Host::new();
        assert_eq!(
            hex(&h.hash(MODE_SHA256, b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&h.hash(MODE_SHA256, b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // 56 bytes: the two-block case, proving CONTINUE carries the state.
        assert_eq!(
            hex(&h.hash(
                MODE_SHA256,
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&h.hash(MODE_SHA1, b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&h.hash(MODE_SHA224, b"abc")),
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
        );
        assert_eq!(
            hex(&h.hash(MODE_SHA256, &vec![b'a'; 1000])),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn one_changed_message_byte_changes_the_digest() {
        let mut h = Host::new();
        let good = h.hash(MODE_SHA256, b"abc");
        let bad = h.hash(MODE_SHA256, b"abd");
        assert_ne!(good, bad);
        assert_eq!(good.len(), 32);
    }

    /// Both halves are asserted against the state itself, so a swap on one side alone cannot
    /// pass.
    #[test]
    fn the_text_and_h_windows_carry_the_documented_byte_order() {
        let mut h = Host::new();
        h.write(reg::MODE, MODE_SHA256);
        let mut block = [0u8; 64];
        block[..3].copy_from_slice(b"abc");
        block[3] = 0x80;
        block[63] = 24;
        for (i, word) in block.chunks(4).enumerate() {
            let bytes: [u8; 4] = word.try_into().expect("64 splits by 4");
            h.write(reg::TEXT + 4 * i as u32, u32::from_le_bytes(bytes));
        }
        assert_eq!(h.read(reg::TEXT), 0x8063_6261);
        h.write(reg::START, 1);
        h.pump();

        let state = h.sha.state();
        assert_eq!(state[0], 0xBA78_16BF, "the standard's first state word");
        assert_eq!(
            h.read(reg::H),
            0xBF16_78BA,
            "the register is that word reversed"
        );
        assert_eq!(&h.sha.digest()[..4], &[0xBA, 0x78, 0x16, 0xBF]);

        // Writing a state word back reverses it again, so a resumed digest round-trips.
        let saved: Vec<u32> = (0..8).map(|i| h.read(reg::H + 4 * i)).collect();
        for (i, word) in saved.iter().enumerate() {
            h.write(reg::H + 4 * i as u32, *word);
        }
        assert_eq!(h.sha.state(), state);
    }

    #[test]
    fn busy_is_set_until_the_completion_event_of_the_profile() {
        // The fast profile completes at now, the device profile 2.8 us later.
        let mut h = Host::new();
        h.write(reg::MODE, MODE_SHA256);
        assert!(!h.stopped, "a MODE write schedules nothing");
        h.write(reg::START, 1);
        assert!(h.sha.busy());
        assert_eq!(h.sched.next_time(), Some(VTime(0)));
        // A write that scheduled the completion ends the slice; one that scheduled nothing does
        // not.
        assert!(h.stopped, "START scheduled the completion");
        h.pump();
        assert!(!h.sha.busy());
        assert_eq!(h.read(reg::BUSY), 0);

        h.sha.set_block_ps(DEVICE_BLOCK_PS);
        h.write(reg::CONTINUE, 1);
        assert!(h.stopped, "CONTINUE scheduled the completion");
        assert_eq!(h.sched.next_time(), Some(VTime(DEVICE_BLOCK_PS)));
        assert_eq!(h.read(reg::BUSY), 1);
        h.pump();
        assert_eq!(h.read(reg::BUSY), 0);
        assert_eq!(h.now, VTime(DEVICE_BLOCK_PS));

        h.write(reg::START, 0);
        assert!(!h.sha.busy());
        assert!(!h.stopped, "nothing was scheduled, so nothing stops");
        assert_eq!(h.sched.next_time(), None);
    }

    #[test]
    fn narrow_accesses_reach_only_the_bytes_they_address() {
        let mut h = Host::new();
        h.write(reg::TEXT, 0x1122_3344);
        assert_eq!(h.sha.load(reg::TEXT, Size::B1, h.now, &mut h.ledger), 0x44);
        assert_eq!(
            h.sha.load(reg::TEXT + 3, Size::B1, h.now, &mut h.ledger),
            0x11
        );
        assert_eq!(
            h.sha.load(reg::TEXT + 2, Size::B2, h.now, &mut h.ledger),
            0x1122
        );
        let stored = h.sha.store(
            reg::TEXT + 1,
            Size::B1,
            0xAA,
            h.now,
            &mut h.ledger,
            &mut h.sched,
        );
        assert_eq!(
            stored,
            Stored::default(),
            "a data-register write schedules nothing"
        );
        assert_eq!(h.read(reg::TEXT), 0x1122_AA44);
    }

    #[test]
    fn a_dma_trigger_records_a_run_and_asks_for_the_wiring() {
        // The completion waits for the bytes, so a trigger schedules nothing itself.
        let mut h = Host::new();
        h.write(reg::BLOCK_NUM, 4);
        let zero = h.sha.store(
            reg::DMA_START,
            Size::B4,
            0,
            h.now,
            &mut h.ledger,
            &mut h.sched,
        );
        assert_eq!(zero, Stored::default());
        assert!(!h.sha.busy());
        for (trigger, restart) in [(reg::DMA_START, true), (reg::DMA_CONTINUE, false)] {
            let stored = h
                .sha
                .store(trigger, Size::B4, 1, h.now, &mut h.ledger, &mut h.sched);
            assert_eq!(
                stored,
                Stored {
                    stop: true,
                    dma: true
                }
            );
            assert!(h.sha.busy());
            assert_eq!(
                h.sched.next_time(),
                None,
                "the completion waits for the bytes"
            );
            assert_eq!(h.sha.take_dma(), Some(DmaRun { restart, blocks: 4 }));
            assert_eq!(h.sha.take_dma(), None, "taken once");
            h.sha.complete_dma();
        }
    }

    #[test]
    fn the_registers_land_in_the_ledger_with_their_spec_classes() {
        let mut h = Host::new();
        h.write(reg::BLOCK_NUM, 4);
        assert_eq!(h.read(reg::BLOCK_NUM), 4);
        h.write(reg::DMA_START, 1);
        assert!(
            h.sha.busy(),
            "a DMA run is in progress until it is fed and completes"
        );
        h.write(reg::INT_ENA, 1);
        assert_eq!(h.read(reg::INT_ENA), 1);
        assert_eq!(h.read(reg::CLEAR_IRQ), 0, "a write-trigger reads 0");
        assert_eq!(h.read(0x2C), 0, "DATE is unmodeled");

        let touches: Vec<_> = h
            .ledger
            .first_touches()
            .iter()
            .map(|t| (t.off, t.access))
            .collect();
        assert_eq!(
            touches,
            vec![
                (reg::BLOCK_NUM, TouchAccess::Write),
                (reg::DMA_START, TouchAccess::Write),
                (reg::INT_ENA, TouchAccess::Write),
                (reg::CLEAR_IRQ, TouchAccess::Read),
                (0x2C, TouchAccess::Read),
            ]
        );
        assert!(h.ledger.first_touches().iter().all(|t| t.periph == Sha::ID));
        assert_eq!(h.sha.fidelity(reg::BUSY), Fidelity::B);
        assert_eq!(h.sha.fidelity(reg::H + 4), Fidelity::A);
        // The registers the `sha256_1m` digest depends on are class A, tied to a device capture;
        // the interrupt pair is C (no IDF caller).
        assert_eq!(h.sha.fidelity(reg::MODE), Fidelity::A);
        assert_eq!(h.sha.fidelity(reg::START), Fidelity::B);
        assert_eq!(h.sha.fidelity(reg::CONTINUE), Fidelity::A);
        assert_eq!(h.sha.fidelity(reg::TEXT), Fidelity::A);
        assert_eq!(h.sha.fidelity(reg::TEXT_END - 4), Fidelity::A);
        assert_eq!(h.sha.fidelity(reg::BLOCK_NUM), Fidelity::A);
        assert_eq!(h.sha.fidelity(reg::DMA_START), Fidelity::A);
        assert_eq!(h.sha.fidelity(reg::DMA_CONTINUE), Fidelity::A);
        assert_eq!(h.sha.fidelity(reg::INT_ENA), Fidelity::C);
        assert_eq!(h.sha.fidelity(reg::CLEAR_IRQ), Fidelity::C);
        assert_eq!(h.sha.fidelity(0x2C), Fidelity::U);
    }

    #[test]
    fn a_reset_clears_the_state_and_keeps_the_reported_touches() {
        use pemu_core::reset::{ResetCause, ResetKind};

        let mut h = Host::new();
        h.sha.set_block_ps(DEVICE_BLOCK_PS);
        let digest = h.hash(MODE_SHA256, b"abc");
        let before = h.ledger.first_touches().len();

        h.sha
            .apply_reset(ResetKind::of(ResetCause(0x01)).expect("a documented power-on reset"));
        assert_eq!(h.sha.state(), [0; STATE_WORDS]);
        assert_eq!(h.read(reg::H), 0);
        assert_eq!(h.read(reg::MODE), 0);
        assert_eq!(h.read(reg::TEXT), 0);
        assert_eq!(
            h.ledger.first_touches().len(),
            before,
            "first-touch state is per machine, not per reset"
        );
        assert_eq!(h.hash(MODE_SHA256, b"abc"), digest);

        // A CPU reset reaches only the hart and the PMS block.
        let cpu = ResetKind::of(ResetCause(0x0C)).expect("a documented CPU reset");
        let kept = h.sha.state();
        h.sha.apply_reset(cpu);
        assert_eq!(h.sha.state(), kept);
    }
}
