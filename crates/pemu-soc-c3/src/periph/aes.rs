//! The AES accelerator, typical and DMA-AES modes (`specs/blocks/aes.toml`, TRM chapter 18).
//! Nothing on the boot path uses it. mbedTLS on the C3 runs every AES operation, GCM included,
//! through `esp_aes_process_dma` (IDF `mbedtls/port/aes/dma/esp_aes.c`), so TLS reaches this block
//! only in DMA mode.
//!
//! Typical mode (TRM 18.4): one `TRIGGER` with `DMA_ENABLE` 0 is one 16-byte block; the guest
//! polls `STATE` for 0 and reads `TEXT_OUT`.
//!
//! DMA mode (TRM 18.5): a `TRIGGER` sets `STATE` to 1, records a run of `BLOCK_NUM` blocks and
//! returns `Wiring::AesDma`. `crate::wiring::aes` moves the bytes from the TX channel bound to AES
//! through [`Aes::dma_transform`] into the RX channel; `BLOCK_NUM x aes_block_ps` later `STATE`
//! reads 2 and the interrupt latches, and a `DMA_EXIT` write returns `STATE` to 0.
//!
//! Byte order (TRM 18.4.2): the data windows hold bytes in stream order, little-endian inside each
//! word, as the `memcpy` of IDF `aes_ll.h` produces. `ENDIAN` (0x44) is not a C3 register; it
//! stores and is not honored.
//!
//! UNVERIFIED (class C): a TX or RX chain that runs out transforms what it can and leaves `STATE`
//! at 1, which the hang detector reports on `aes.state`; `BLOCK_NUM` 0 completes at once; a
//! `TRIGGER` or `DMA_EXIT` while `STATE` is 1 is ignored; a reserved `MODE` or `BLOCK_MODE`
//! transforms nothing and still completes; the interrupt latches whatever `INT_ENA` says, the
//! enable gating only the level.

use pemu_core::aes::{decrypt_block, encrypt_block, expand_key};
use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::regstore::Size;
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::{Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};

/// Block offsets (TRM 18.6 and 18.7, IDF `soc/esp32c3/include/soc/hwcrypto_reg.h`).
pub mod reg {
    /// Key window, up to 256 bits.
    pub const KEY: u32 = 0x00;
    pub const KEY_END: u32 = 0x20;
    /// Input block of the typical mode.
    pub const TEXT_IN: u32 = 0x20;
    pub const TEXT_IN_END: u32 = 0x30;
    /// Output block of the typical mode, read-only.
    pub const TEXT_OUT: u32 = 0x30;
    pub const TEXT_OUT_END: u32 = 0x40;
    /// Key length and direction.
    pub const MODE: u32 = 0x40;
    /// Not a C3 register: stored, not honored.
    pub const ENDIAN: u32 = 0x44;
    /// Start one operation.
    pub const TRIGGER: u32 = 0x48;
    /// 0 idle, 1 work, 2 done (DMA mode).
    pub const STATE: u32 = 0x4C;
    /// `IV_MEM`, the IV or initial counter block in stream order (TRM 18.5.5). After a run it
    /// holds the chaining value IDF reads back with `aes_hal_read_iv`: the last ciphertext block
    /// for CBC and CFB128, the last forward-cipher output for OFB, the next counter block for CTR,
    /// the last 16 ciphertext bytes for CFB8. ECB leaves it alone.
    pub const IV: u32 = 0x50;
    pub const IV_END: u32 = 0x60;
    /// First byte of the GCM windows of other chips (`H`, `J0`, `T0`). They are not C3 registers
    /// (`SOC_AES_SUPPORT_GCM` is unset and TRM table 18.5-1 marks block mode 6 reserved), so they
    /// store and stay class U, and a guest that reaches one shows in the ledger.
    pub const GCM: u32 = 0x60;
    pub const GCM_END: u32 = 0x90;
    /// Working mode: 0 typical, 1 DMA.
    pub const DMA_ENABLE: u32 = 0x90;
    /// Block cipher mode of the DMA mode: ECB, CBC, OFB, CTR, CFB8 or CFB128 (TRM table 18.5-1),
    /// as NIST SP 800-38A defines them. OFB and CTR use only the forward cipher, which IDF relies
    /// on when it programs them with `ESP_AES_DECRYPT`.
    pub const BLOCK_MODE: u32 = 0x94;
    /// Blocks of one DMA run.
    pub const BLOCK_NUM: u32 = 0x98;
    /// CTR increment: 0 INC32, 1 INC128.
    pub const INC_SEL: u32 = 0x9C;
    /// GCM additional-data block count (not a C3 register, stored).
    pub const AAD_BLOCK_NUM: u32 = 0xA0;
    /// GCM remainder bit count (not a C3 register, stored).
    pub const REMAINDER_BIT_NUM: u32 = 0xA4;
    /// GCM continue (not a C3 register, stored).
    pub const CONTINUE: u32 = 0xA8;
    /// Clear the completion interrupt.
    pub const INT_CLEAR: u32 = 0xAC;
    /// Enable the completion interrupt: source 48 follows it while bit 0 is set, until a write of
    /// 1 to [`INT_CLEAR`] (IDF `esp_aes_complete_isr` under `CONFIG_MBEDTLS_AES_USE_INTERRUPT`).
    pub const INT_ENA: u32 = 0xB0;
    /// Block version (stored).
    pub const DATE: u32 = 0xB4;
    /// Leave the DONE state of a DMA run.
    pub const DMA_EXIT: u32 = 0xB8;
    pub const END: u32 = 0xBC;
}

/// `MODE` selecting AES-128 encryption (TRM table 18.3-2).
pub const MODE_ENC_128: u32 = 0;
pub const MODE_ENC_256: u32 = 2;
pub const MODE_DEC_128: u32 = 4;
pub const MODE_DEC_256: u32 = 6;

/// `BLOCK_MODE` ECB (TRM table 18.5-1).
pub const BLOCK_ECB: u32 = 0;
pub const BLOCK_CBC: u32 = 1;
pub const BLOCK_OFB: u32 = 2;
pub const BLOCK_CTR: u32 = 3;
pub const BLOCK_CFB8: u32 = 4;
pub const BLOCK_CFB128: u32 = 5;

/// `STATE` idle: the typical-mode poll waits for it, and `DMA_EXIT` returns a finished run to it.
pub const STATE_IDLE: u32 = 0;
pub const STATE_BUSY: u32 = 1;
/// `STATE` once a DMA run has completed, until `DMA_EXIT` (`aes_hal_wait_done`).
pub const STATE_DONE: u32 = 2;

/// Event tag of the typical-mode completion that returns `STATE` to idle.
pub const TAG_DONE: u16 = 0;
/// Event tag of the completion of a DMA run: `STATE` reads DONE and the interrupt latches.
pub const TAG_DMA_DONE: u16 = 1;

/// `MODE` bits 2 to 0 (TRM register 18.4).
const MODE_MASK: u32 = 0x7;
/// `BLOCK_MODE` bits 2 to 0 (TRM register 18.6).
const BLOCK_MODE_MASK: u32 = 0x7;

pub use pemu_core::aes::BLOCK_BYTES;
const KEY_BYTES: usize = 32;
/// Words of the GCM windows of other chips.
const GCM_WORDS: usize = ((reg::GCM_END - reg::GCM) / 4) as usize;
/// Words of the first-touch bitmap, one bit per 4-byte slot of the window.
const TOUCH_WORDS: usize = (Aes::SIZE.div_ceil(4) as usize).div_ceil(64);

/// A DMA run a `TRIGGER` write asked for and the wiring has not yet fed.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct DmaRun {
    /// Blocks to transform, `BLOCK_NUM` at the trigger.
    pub blocks: u32,
}

impl DmaRun {
    /// Bytes the run reads from the TX walk and writes into the RX walk, saturating.
    pub fn bytes(&self) -> u32 {
        self.blocks.saturating_mul(BLOCK_BYTES as u32)
    }
}

/// What one register write asks of its caller.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Stored {
    /// The write scheduled an event or started a DMA run, so the run loop must stop after it.
    pub stop: bool,
    /// The write started a DMA run: return `Wiring::AesDma`.
    pub dma: bool,
}

/// What [`Aes::dma_transform`] made of the bytes the TX walk delivered.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Transformed {
    pub out: Vec<u8>,
    /// The source had the run's whole length (after the TEXT-PADDING of a partial last block).
    pub whole: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Aes {
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, KEY_BYTES>")]
    key: Vec<u8>,
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, BLOCK_BYTES>")]
    text_in: Vec<u8>,
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, BLOCK_BYTES>")]
    text_out: Vec<u8>,
    mode: u32,
    endian: u32,
    state: u32,
    /// `IV_MEM` in stream order.
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, BLOCK_BYTES>")]
    iv: Vec<u8>,
    /// The GCM windows of other chips: stored, not honored ([`reg::GCM`]).
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, GCM_WORDS>")]
    gcm: Vec<u32>,
    dma_enable: u32,
    block_mode: u32,
    block_num: u32,
    inc_sel: u32,
    aad_block_num: u32,
    remainder_bit_num: u32,
    continue_: u32,
    int_ena: u32,
    date: u32,
    /// The DMA run the last trigger asked for, until the wiring feeds it.
    run: Option<DmaRun>,
    /// The DMA completion interrupt, latched until `INT_CLEAR`.
    irq_raw: bool,
    op_ps: u64,
    /// One bit per 4-byte slot of the window: already reported to the ledger.
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, TOUCH_WORDS>")]
    touched: Vec<u64>,
}

impl Default for Aes {
    fn default() -> Aes {
        Aes {
            key: vec![0; KEY_BYTES],
            text_in: vec![0; BLOCK_BYTES],
            text_out: vec![0; BLOCK_BYTES],
            mode: 0,
            endian: 0,
            state: STATE_IDLE,
            iv: vec![0; BLOCK_BYTES],
            gcm: vec![0; GCM_WORDS],
            dma_enable: 0,
            block_mode: 0,
            block_num: 0,
            inc_sel: 0,
            aad_block_num: 0,
            remainder_bit_num: 0,
            continue_: 0,
            int_ena: 0,
            date: 0,
            run: None,
            irq_raw: false,
            op_ps: 0,
            touched: vec![0; TOUCH_WORDS],
        }
    }
}

impl Aes {
    pub fn busy(&self) -> bool {
        self.state == STATE_BUSY
    }

    pub fn state(&self) -> u32 {
        self.state
    }

    /// Picoseconds one block takes in either mode, set from the profile's `aes_block_ps`.
    pub fn set_op_ps(&mut self, ps: u64) {
        self.op_ps = ps;
    }

    pub fn text_out(&self) -> &[u8] {
        &self.text_out
    }

    pub fn iv(&self) -> &[u8] {
        &self.iv
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.touch(off, TouchAccess::Read, size, now, ledger);
        let (mask, shift) = window_bits(off, size);
        (self.word(off & !3) & mask) >> shift
    }

    /// Writes the low `size` bytes of `val` at `off` and starts whatever a trigger asked for.
    /// `stop` is set when the write scheduled a completion or started a DMA run, so `STATE`
    /// cannot change a slice late.
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
        let (mask, shift) = window_bits(off, size);
        let written = ((u64::from(val) << shift) as u32) & mask;
        let merged = (self.word(aligned) & !mask) | written;
        let mut out = Stored::default();
        match aligned {
            reg::KEY..reg::KEY_END => put_word(&mut self.key, aligned - reg::KEY, merged),
            reg::TEXT_IN..reg::TEXT_IN_END => {
                put_word(&mut self.text_in, aligned - reg::TEXT_IN, merged);
            }
            reg::TEXT_OUT..reg::TEXT_OUT_END => {}
            reg::MODE => self.mode = merged & MODE_MASK,
            reg::ENDIAN => self.endian = merged,
            reg::IV..reg::IV_END => put_word(&mut self.iv, aligned - reg::IV, merged),
            reg::GCM..reg::GCM_END => self.gcm[((aligned - reg::GCM) / 4) as usize] = merged,
            reg::DMA_ENABLE => self.dma_enable = merged & 1,
            reg::BLOCK_MODE => self.block_mode = merged & BLOCK_MODE_MASK,
            reg::BLOCK_NUM => self.block_num = merged,
            reg::INC_SEL => self.inc_sel = merged & 1,
            reg::AAD_BLOCK_NUM => self.aad_block_num = merged,
            reg::REMAINDER_BIT_NUM => self.remainder_bit_num = merged,
            reg::CONTINUE => self.continue_ = merged,
            reg::INT_ENA => self.int_ena = merged & 1,
            reg::DATE => self.date = merged,
            reg::INT_CLEAR if written & 1 != 0 => self.irq_raw = false,
            // Any write releases a finished run (IDF `aes_ll_dma_exit` writes 0). One still
            // running is left alone (UNVERIFIED).
            reg::DMA_EXIT => {
                if self.state == STATE_DONE {
                    self.state = STATE_IDLE;
                }
            }
            // A trigger while an operation runs is ignored (UNVERIFIED).
            reg::TRIGGER if written & 1 != 0 && self.state != STATE_BUSY => {
                self.state = STATE_BUSY;
                if self.dma_enable & 1 != 0 {
                    self.run = Some(DmaRun {
                        blocks: self.block_num,
                    });
                    out = Stored {
                        stop: true,
                        dma: true,
                    };
                } else {
                    self.transform();
                    // The result is ready, but STATE is state the guest polls, so even an
                    // immediate completion is an event at `now`.
                    self.schedule(now, self.op_ps, TAG_DONE, sched);
                    out.stop = true;
                }
            }
            _ => {}
        }
        out
    }

    /// The typical-mode completion: `STATE` returns to idle.
    pub fn complete(&mut self) {
        if self.state == STATE_BUSY {
            self.state = STATE_IDLE;
        }
    }

    /// The DMA run the last trigger asked for, taken by the wiring that feeds it.
    pub fn take_dma(&mut self) -> Option<DmaRun> {
        self.run.take()
    }

    /// Transforms the bytes the TX walk delivered for `run` and leaves the chaining value in
    /// `IV_MEM`. A partial last block is zero-padded (TRM 18.5.1); a source still short of the
    /// run is transformed as far as it goes and reported not `whole` (UNVERIFIED). A reserved
    /// mode transforms nothing and reports `whole`, so the run still completes.
    pub fn dma_transform(&mut self, run: DmaRun, data: &[u8]) -> Transformed {
        let need = run.bytes() as usize;
        let mut source = data[..data.len().min(need)].to_vec();
        source.resize(source.len().next_multiple_of(BLOCK_BYTES), 0);
        let whole = source.len() >= need;
        Transformed {
            out: self.crypt(&source),
            whole,
        }
    }

    /// Schedules the completion of `run`, `run.blocks x aes_block_ps` after `now`.
    pub fn schedule_dma_done(&self, run: DmaRun, now: VTime, sched: &mut Scheduler) {
        let ps = self.op_ps.saturating_mul(u64::from(run.blocks));
        self.schedule(now, ps, TAG_DMA_DONE, sched);
    }

    /// The completion of a DMA run: `STATE` reads DONE and the interrupt latches. One that finds
    /// no run in progress was overtaken by a reset (the scheduler does not cancel it).
    pub fn complete_dma(&mut self) {
        if self.state == STATE_BUSY {
            self.state = STATE_DONE;
            self.irq_raw = true;
        }
    }

    /// Level of interrupt source 48: the latched completion while `INT_ENA` bit 0 is set.
    pub fn irq_level(&self) -> bool {
        self.irq_raw && self.int_ena & 1 != 0
    }

    pub fn sync_irq(&self, irq: &mut crate::intc::IrqFabric) {
        irq.set_source(pemu_core::irq_source::irq::AES, self.irq_level());
    }

    /// Clears the block for a reset that reaches it, keeping the timing profile and the
    /// first-touch state.
    pub fn apply_reset(&mut self, kind: ResetKind) {
        if !kind.clears(pemu_core::regstore::RESET_BY_ALL_SCOPES) {
            return;
        }
        let touched = std::mem::take(&mut self.touched);
        let op_ps = self.op_ps;
        *self = Aes {
            touched,
            op_ps,
            ..Aes::default()
        };
    }

    fn schedule(&self, now: VTime, ps: u64, tag: u16, sched: &mut Scheduler) {
        sched.schedule(
            now,
            VTime(now.0.saturating_add(ps)),
            EventKey {
                owner: Owner::Periph(Aes::ID),
                tag,
            },
        );
    }

    fn word(&self, at: u32) -> u32 {
        match at {
            reg::KEY..reg::KEY_END => take_word(&self.key, at - reg::KEY),
            reg::TEXT_IN..reg::TEXT_IN_END => take_word(&self.text_in, at - reg::TEXT_IN),
            reg::TEXT_OUT..reg::TEXT_OUT_END => take_word(&self.text_out, at - reg::TEXT_OUT),
            reg::MODE => self.mode,
            reg::ENDIAN => self.endian,
            reg::STATE => self.state,
            reg::IV..reg::IV_END => take_word(&self.iv, at - reg::IV),
            reg::GCM..reg::GCM_END => self.gcm[((at - reg::GCM) / 4) as usize],
            reg::DMA_ENABLE => self.dma_enable,
            reg::BLOCK_MODE => self.block_mode,
            reg::BLOCK_NUM => self.block_num,
            reg::INC_SEL => self.inc_sel,
            reg::AAD_BLOCK_NUM => self.aad_block_num,
            reg::REMAINDER_BIT_NUM => self.remainder_bit_num,
            reg::CONTINUE => self.continue_,
            reg::INT_ENA => self.int_ena,
            reg::DATE => self.date,
            // TRIGGER, INT_CLEAR, DMA_EXIT and unnamed offsets read 0.
            _ => 0,
        }
    }

    /// The key schedule and direction `MODE` selects, or `None` for a reserved value.
    fn schedule_for_mode(&self) -> Option<(Vec<[u8; 4]>, bool)> {
        let (key_bytes, decrypt) = match self.mode {
            MODE_ENC_128 => (16, false),
            MODE_ENC_256 => (32, false),
            MODE_DEC_128 => (16, true),
            MODE_DEC_256 => (32, true),
            _ => return None,
        };
        Some((expand_key(&self.key[..key_bytes]), decrypt))
    }

    /// One typical-mode block. A reserved `MODE` leaves the output alone.
    fn transform(&mut self) {
        let Some((schedule, decrypt)) = self.schedule_for_mode() else {
            return;
        };
        let mut block = [0u8; BLOCK_BYTES];
        block.copy_from_slice(&self.text_in);
        if decrypt {
            decrypt_block(&mut block, &schedule);
        } else {
            encrypt_block(&mut block, &schedule);
        }
        self.text_out.copy_from_slice(&block);
    }

    /// `source` (whole blocks) through the `BLOCK_MODE` mode from the chaining value in `IV_MEM`,
    /// which is left holding the next run's value. A reserved mode returns nothing.
    fn crypt(&mut self, source: &[u8]) -> Vec<u8> {
        let Some((w, decrypt)) = self.schedule_for_mode() else {
            return Vec::new();
        };
        let forward = |block: &[u8; BLOCK_BYTES]| {
            let mut out = *block;
            encrypt_block(&mut out, &w);
            out
        };
        let mut iv = [0u8; BLOCK_BYTES];
        iv.copy_from_slice(&self.iv);
        let mut out = Vec::with_capacity(source.len());
        for chunk in source.chunks_exact(BLOCK_BYTES) {
            let mut input = [0u8; BLOCK_BYTES];
            input.copy_from_slice(chunk);
            let block = match self.block_mode {
                // SP 800-38A 6.1.
                BLOCK_ECB => {
                    let mut b = input;
                    if decrypt {
                        decrypt_block(&mut b, &w);
                    } else {
                        encrypt_block(&mut b, &w);
                    }
                    b
                }
                // SP 800-38A 6.2: the chaining value is the last ciphertext block.
                BLOCK_CBC => {
                    if decrypt {
                        let mut b = input;
                        decrypt_block(&mut b, &w);
                        let plain = xor(&b, &iv);
                        iv = input;
                        plain
                    } else {
                        let cipher = forward(&xor(&input, &iv));
                        iv = cipher;
                        cipher
                    }
                }
                // SP 800-38A 6.4: the chaining value is the last output of the forward cipher.
                BLOCK_OFB => {
                    iv = forward(&iv);
                    xor(&input, &iv)
                }
                // SP 800-38A 6.5 and appendix B.1: the chaining value is the next counter block.
                BLOCK_CTR => {
                    let stream = forward(&iv);
                    iv = increment(iv, self.inc_sel & 1 != 0);
                    xor(&input, &stream)
                }
                // SP 800-38A 6.3 with s = 8: sixteen segments per block, the ciphertext byte
                // shifted into the register after each.
                BLOCK_CFB8 => {
                    let mut b = [0u8; BLOCK_BYTES];
                    for (i, byte) in input.iter().enumerate() {
                        let o = forward(&iv);
                        b[i] = byte ^ o[0];
                        let feedback = if decrypt { *byte } else { b[i] };
                        iv.copy_within(1.., 0);
                        iv[BLOCK_BYTES - 1] = feedback;
                    }
                    b
                }
                // SP 800-38A 6.3 with s = 128: the chaining value is the last ciphertext block.
                BLOCK_CFB128 => {
                    let b = xor(&input, &forward(&iv));
                    iv = if decrypt { input } else { b };
                    b
                }
                // TRM table 18.5-1: 6 and 7 are reserved.
                _ => return Vec::new(),
            };
            out.extend_from_slice(&block);
        }
        self.iv.copy_from_slice(&iv);
        out
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
            periph: Aes::ID,
            off: off & !3,
            access,
            size: size as u8,
            now,
            allowlisted: false,
        });
    }
}

fn xor(a: &[u8; BLOCK_BYTES], b: &[u8; BLOCK_BYTES]) -> [u8; BLOCK_BYTES] {
    let mut out = [0u8; BLOCK_BYTES];
    for (o, (x, y)) in out.iter_mut().zip(a.iter().zip(b.iter())) {
        *o = x ^ y;
    }
    out
}

/// The next counter block (SP 800-38A appendix B.1, TRM 18.5.3): the low 32 bits of the block,
/// read big-endian, plus one modulo 2^32 (INC32), or the whole block plus one modulo 2^128
/// (INC128).
fn increment(block: [u8; BLOCK_BYTES], all: bool) -> [u8; BLOCK_BYTES] {
    let mut out = block;
    if all {
        out = u128::from_be_bytes(block).wrapping_add(1).to_be_bytes();
    } else {
        let low = u32::from_be_bytes([block[12], block[13], block[14], block[15]]).wrapping_add(1);
        out[12..].copy_from_slice(&low.to_be_bytes());
    }
    out
}

/// Mask and shift of an access of `size` bytes at `off` inside its 32-bit register.
fn window_bits(off: u32, size: Size) -> (u32, u32) {
    let shift = (off % 4) * 8;
    let bits = (size as u32) * 8;
    let mask = (((1u64 << bits) - 1) << shift) as u32;
    (mask, shift)
}

/// The word at byte offset `at` of a byte window, little-endian.
fn take_word(bytes: &[u8], at: u32) -> u32 {
    let at = at as usize;
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Writes `val` at byte offset `at` of a byte window, little-endian.
fn put_word(bytes: &mut [u8], at: u32, val: u32) {
    bytes[at as usize..at as usize + 4].copy_from_slice(&val.to_le_bytes());
}

impl Peripheral for Aes {
    const ID: PeriphId = super::id::AES;
    const BASE: u32 = 0x6003_A000;
    const SIZE: u32 = 0x1000;

    /// Every reset scope clears AES.
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

    /// A completion-scheduling write returns `stop` so the event fires at the next instruction
    /// boundary; a DMA trigger also returns `Wiring::AesDma`.
    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let stored = self.store(off, size, val, cx.now, cx.ledger, cx.sched);
        self.sync_irq(cx.irq);
        RegWrite {
            stop: stored.stop,
            wiring: if stored.dma {
                Wiring::AesDma
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

    /// `STATE` moves only on the completion events and the guest's own writes, so a poll of it
    /// can be fast-forwarded to the next event.
    fn stable_until(&self, off: u32, cx: &Cx) -> Stability {
        let _ = cx;
        match off & !3 {
            reg::STATE => Stability::UntilNextEvent,
            _ => Stability::Never,
        }
    }

    /// The class `specs/blocks/aes.toml` gives the register at `off`, `U` for the rest. The block
    /// has no rows in `specs/c3-registers.csv`, so codegen renders its classes into
    /// [`crate::gen::classes::aes`].
    fn fidelity(&self, off: u32) -> Fidelity {
        crate::r#gen::classes::aes::class_at(off)
    }
}

pub type Model = Aes;

#[cfg(test)]
mod tests {
    use super::*;

    struct Host {
        aes: Aes,
        sched: Scheduler,
        ledger: FidelityLedger,
        now: VTime,
        stored: Stored,
    }

    impl Host {
        fn new() -> Host {
            Host {
                aes: Aes::default(),
                sched: Scheduler::default(),
                ledger: FidelityLedger::default(),
                now: VTime(0),
                stored: Stored::default(),
            }
        }

        fn read(&mut self, off: u32) -> u32 {
            self.aes.load(off, Size::B4, self.now, &mut self.ledger)
        }

        fn write(&mut self, off: u32, val: u32) {
            self.stored = self.aes.store(
                off,
                Size::B4,
                val,
                self.now,
                &mut self.ledger,
                &mut self.sched,
            );
        }

        fn put_bytes(&mut self, base: u32, bytes: &[u8]) {
            for (i, word) in bytes.chunks(4).enumerate() {
                let w: [u8; 4] = word.try_into().expect("a whole number of words");
                self.write(base + 4 * i as u32, u32::from_le_bytes(w));
            }
        }

        fn pump(&mut self) {
            if let Some(t) = self.sched.next_time() {
                self.now = self.now.max(t);
            }
            while let Some(key) = self.sched.pop_due(self.now) {
                assert_eq!(key.owner, Owner::Periph(Aes::ID));
                match key.tag {
                    TAG_DMA_DONE => self.aes.complete_dma(),
                    _ => self.aes.complete(),
                }
            }
        }

        /// The sequence `aes_hal_transform_block` performs.
        fn transform(&mut self, mode: u32, key: &[u8], block: &[u8]) -> Vec<u8> {
            self.put_bytes(reg::KEY, key);
            self.put_bytes(reg::TEXT_IN, block);
            self.write(reg::MODE, mode);
            self.write(reg::TRIGGER, 1);
            assert_eq!(self.read(reg::STATE), STATE_BUSY);
            self.pump();
            assert_eq!(
                self.read(reg::STATE),
                STATE_IDLE,
                "the aes.state poll exits"
            );
            (0..4)
                .flat_map(|i| self.read(reg::TEXT_OUT + 4 * i).to_le_bytes())
                .collect()
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// FIPS-197 Appendix C.1 and C.3 vectors.
    #[test]
    fn the_fips_197_vectors_come_out_of_the_text_out_window() {
        let mut h = Host::new();
        let plain: Vec<u8> = (0..16).map(|i| i * 0x11).collect();
        let key128: Vec<u8> = (0..16).collect();
        let key256: Vec<u8> = (0..32).collect();

        let cipher = h.transform(MODE_ENC_128, &key128, &plain);
        assert_eq!(hex(&cipher), "69c4e0d86a7b0430d8cdb78070b4c55a");
        assert_eq!(h.transform(MODE_DEC_128, &key128, &cipher), plain);

        let cipher = h.transform(MODE_ENC_256, &key256, &plain);
        assert_eq!(hex(&cipher), "8ea2b7ca516745bfeafc49904b496089");
        assert_eq!(h.transform(MODE_DEC_256, &key256, &cipher), plain);
    }

    #[test]
    fn a_mode_the_map_does_not_name_leaves_the_output_alone() {
        // Odd MODE values are reserved, so no transform is invented.
        let mut h = Host::new();
        let key: Vec<u8> = (0..16).collect();
        let first = h.transform(MODE_ENC_128, &key, &[0x01; 16]);
        h.put_bytes(reg::TEXT_IN, &[0x02; 16]);
        h.write(reg::MODE, 1);
        h.write(reg::TRIGGER, 1);
        h.pump();
        assert_eq!(h.aes.text_out(), &first[..]);
        assert_eq!(h.read(reg::STATE), STATE_IDLE, "the guest still gets out");
    }

    #[test]
    fn the_state_poll_waits_for_the_profile_duration() {
        // Now under the fast profile, aes_block_ps under the device one.
        let mut h = Host::new();
        h.aes.set_op_ps(1_500_000);
        h.write(reg::MODE, MODE_ENC_128);
        assert!(!h.stored.stop, "a MODE write schedules nothing");
        h.write(reg::TRIGGER, 1);
        assert_eq!(h.read(reg::STATE), STATE_BUSY);
        assert_eq!(
            h.stored,
            Stored {
                stop: true,
                dma: false
            },
            "TRIGGER in the typical mode scheduled the completion and asks for no wiring"
        );
        assert_eq!(h.sched.next_time(), Some(VTime(1_500_000)));
        h.pump();
        assert_eq!(h.read(reg::STATE), STATE_IDLE);
        h.write(reg::TRIGGER, 0);
        assert_eq!(h.stored, Stored::default());
        assert_eq!(h.sched.next_time(), None);
        assert_eq!(h.read(reg::TRIGGER), 0, "a write-trigger reads 0");
    }

    #[test]
    fn a_dma_trigger_records_a_run_and_dma_exit_releases_the_done_state() {
        // The completion waits for the bytes, so TRIGGER schedules nothing itself. A second
        // trigger while it runs is ignored, and DMA_EXIT written 0 returns STATE to 0.
        let mut h = Host::new();
        h.write(reg::DMA_ENABLE, 1);
        h.write(reg::BLOCK_NUM, 7);
        h.write(reg::TRIGGER, 1);
        assert_eq!(
            h.stored,
            Stored {
                stop: true,
                dma: true
            }
        );
        assert_eq!(h.read(reg::STATE), STATE_BUSY);
        assert_eq!(
            h.sched.next_time(),
            None,
            "the completion waits for the bytes"
        );
        h.write(reg::TRIGGER, 1);
        assert_eq!(h.stored, Stored::default(), "ignored while STATE is 1");
        h.write(reg::DMA_EXIT, 0);
        assert_eq!(
            h.read(reg::STATE),
            STATE_BUSY,
            "DMA_EXIT does not end a running run"
        );
        let run = h.aes.take_dma().expect("the run was recorded");
        assert_eq!(run, DmaRun { blocks: 7 });
        assert_eq!(run.bytes(), 112);
        assert_eq!(h.aes.take_dma(), None, "taken once");

        h.aes.schedule_dma_done(run, h.now, &mut h.sched);
        h.pump();
        assert_eq!(h.read(reg::STATE), STATE_DONE, "aes_hal_wait_done exits");
        h.write(reg::DMA_EXIT, 0);
        assert_eq!(h.read(reg::STATE), STATE_IDLE);
        assert_eq!(h.read(reg::DMA_EXIT), 0, "a write-only register reads 0");
    }

    #[test]
    fn the_completion_interrupt_latches_and_int_clear_clears_it() {
        // IDF esp_aes_complete_isr writes INT_CLEAR 1.
        let mut h = Host::new();
        h.write(reg::DMA_ENABLE, 1);
        h.write(reg::INT_ENA, 1);
        h.write(reg::BLOCK_NUM, 1);
        h.write(reg::TRIGGER, 1);
        let run = h.aes.take_dma().expect("a run");
        assert!(!h.aes.irq_level(), "not before the completion");
        h.aes.schedule_dma_done(run, h.now, &mut h.sched);
        h.pump();
        assert!(h.aes.irq_level());
        h.write(reg::INT_ENA, 0);
        assert!(!h.aes.irq_level(), "INT_ENA gates the level");
        h.write(reg::INT_ENA, 1);
        assert!(h.aes.irq_level(), "the latch survived the enable toggle");
        h.write(reg::INT_CLEAR, 0);
        assert!(h.aes.irq_level(), "INT_CLEAR written 0 clears nothing");
        h.write(reg::INT_CLEAR, 1);
        assert!(!h.aes.irq_level());
        assert_eq!(h.read(reg::INT_CLEAR), 0, "write-only");

        // The typical mode raises no interrupt.
        h.write(reg::DMA_EXIT, 0);
        h.write(reg::DMA_ENABLE, 0);
        h.write(reg::TRIGGER, 1);
        h.pump();
        assert!(!h.aes.irq_level());
    }

    #[test]
    fn the_registers_keep_their_documented_widths_and_text_out_is_read_only() {
        // The reserved bits read 0.
        let mut h = Host::new();
        for (off, width_mask) in [
            (reg::MODE, 0x7),
            (reg::DMA_ENABLE, 0x1),
            (reg::BLOCK_MODE, 0x7),
            (reg::INC_SEL, 0x1),
            (reg::INT_ENA, 0x1),
            (reg::BLOCK_NUM, 0xFFFF_FFFF),
        ] {
            h.write(off, 0xFFFF_FFFF);
            assert_eq!(h.read(off), width_mask, "offset {off:#X}");
            h.write(off, 0);
        }
        h.write(reg::TEXT_OUT, 0x1234_5678);
        assert_eq!(h.read(reg::TEXT_OUT), 0, "TEXT_OUT is read-only");
        h.put_bytes(reg::IV, &(0u8..16).collect::<Vec<_>>());
        assert_eq!(h.read(reg::IV), 0x0302_0100);
        assert_eq!(h.aes.iv(), &(0u8..16).collect::<Vec<_>>()[..]);
    }

    #[test]
    fn the_registers_land_in_the_ledger_with_their_spec_classes() {
        let mut h = Host::new();
        h.write(reg::ENDIAN, 0x3F);
        h.write(reg::GCM, 0x1122_3344);
        h.write(reg::GCM_END - 4, 0x5566_7788);
        h.write(reg::CONTINUE, 1);
        h.write(reg::AAD_BLOCK_NUM, 2);
        assert_eq!(h.read(reg::ENDIAN), 0x3F);
        assert_eq!(h.read(reg::GCM), 0x1122_3344);
        assert_eq!(h.read(reg::GCM_END - 4), 0x5566_7788);
        assert_eq!(h.read(reg::CONTINUE), 1);
        assert_eq!(h.read(reg::AAD_BLOCK_NUM), 2);
        assert_eq!(h.read(reg::END), 0, "past the registers the block names");
        assert_eq!(
            h.stored,
            Stored::default(),
            "the GCM registers of other chips start nothing"
        );
        // Class A: the rows the `probe_crypto` device capture proves.
        for off in [
            reg::KEY,
            reg::KEY_END - 4,
            reg::MODE,
            reg::TRIGGER,
            reg::IV,
            reg::IV_END - 4,
            reg::DMA_ENABLE,
            reg::BLOCK_MODE,
            reg::BLOCK_NUM,
        ] {
            assert_eq!(h.aes.fidelity(off), Fidelity::A, "offset {off:#X}");
        }
        for off in [
            reg::TEXT_IN,
            reg::TEXT_OUT,
            reg::STATE,
            reg::INC_SEL,
            reg::INT_CLEAR,
            reg::INT_ENA,
            reg::DMA_EXIT,
        ] {
            assert_eq!(h.aes.fidelity(off), Fidelity::B, "offset {off:#X}");
        }
        for off in [
            reg::ENDIAN,
            reg::GCM,
            reg::AAD_BLOCK_NUM,
            reg::REMAINDER_BIT_NUM,
            reg::CONTINUE,
            reg::DATE,
        ] {
            assert_eq!(h.aes.fidelity(off), Fidelity::U, "offset {off:#X}");
        }
        assert!(h.ledger.first_touches().iter().all(|t| t.periph == Aes::ID));
        assert_eq!(
            h.ledger
                .first_touches()
                .iter()
                .filter(|t| t.off == reg::ENDIAN)
                .count(),
            1,
            "each register is reported once"
        );
    }

    #[test]
    fn a_reset_clears_the_key_and_the_blocks_and_keeps_the_reported_touches() {
        // A key left in the window must not survive a reset that clears the block.
        use pemu_core::reset::{ResetCause, ResetKind};

        let mut h = Host::new();
        let key: Vec<u8> = (0..16).collect();
        h.transform(MODE_ENC_128, &key, &[0x5A; 16]);
        h.put_bytes(reg::IV, &[0x77; 16]);
        h.write(reg::DMA_ENABLE, 1);
        h.write(reg::TRIGGER, 1);
        let before = h.ledger.first_touches().len();

        h.aes
            .apply_reset(ResetKind::of(ResetCause(0x01)).expect("a documented power-on reset"));
        assert_eq!(h.read(reg::KEY), 0);
        assert_eq!(h.read(reg::TEXT_IN), 0);
        assert_eq!(h.read(reg::TEXT_OUT), 0);
        assert_eq!(h.read(reg::IV), 0);
        assert_eq!(h.read(reg::DMA_ENABLE), 0);
        assert_eq!(h.read(reg::STATE), STATE_IDLE);
        assert_eq!(h.aes.take_dma(), None, "the pending run went with it");
        assert_eq!(
            h.ledger.first_touches().len(),
            before,
            "first-touch state is per machine, not per reset"
        );

        // A CPU reset reaches only the hart and the PMS block.
        h.write(reg::MODE, MODE_ENC_256);
        let cpu = ResetKind::of(ResetCause(0x0C)).expect("a documented CPU reset");
        h.aes.apply_reset(cpu);
        assert_eq!(h.read(reg::MODE), MODE_ENC_256);
    }

    #[test]
    fn narrow_accesses_reach_only_the_bytes_they_address() {
        // Byte i of the stream is byte i of the window.
        let mut h = Host::new();
        h.write(reg::TEXT_IN, 0x1122_3344);
        assert_eq!(
            h.aes.load(reg::TEXT_IN, Size::B1, h.now, &mut h.ledger),
            0x44
        );
        assert_eq!(
            h.aes.load(reg::TEXT_IN + 3, Size::B1, h.now, &mut h.ledger),
            0x11
        );
        let stored = h.aes.store(
            reg::TEXT_IN + 2,
            Size::B2,
            0xBEEF,
            h.now,
            &mut h.ledger,
            &mut h.sched,
        );
        assert_eq!(
            stored,
            Stored::default(),
            "a data-register write schedules nothing"
        );
        assert_eq!(h.read(reg::TEXT_IN), 0xBEEF_3344);
    }

    #[test]
    fn the_ctr_increment_is_32_or_128_bits_wide() {
        // INC32 wraps inside the low word, INC128 carries.
        let mut block = [0u8; BLOCK_BYTES];
        block[11] = 0x01;
        block[12..].copy_from_slice(&[0xFF; 4]);
        let inc32 = increment(block, false);
        assert_eq!(&inc32[11..], &[0x01, 0, 0, 0, 0]);
        let inc128 = increment(block, true);
        assert_eq!(&inc128[11..], &[0x02, 0, 0, 0, 0]);
        assert_eq!(increment([0xFF; 16], true), [0; 16]);
    }
}
