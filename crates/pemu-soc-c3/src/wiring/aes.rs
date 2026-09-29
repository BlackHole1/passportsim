//! `Wiring::AesDma`: the one place the AES block and its two GDMA channels meet
//! (`specs/blocks/aes.toml`).
//!
//! A `TRIGGER` write with `DMA_ENABLE` records a run of `BLOCK_NUM` blocks; this step pulls
//! `BLOCK_NUM x 16` bytes through the TX walk, has the model transform them and pushes the result
//! through the RX walk (TRM 18.5.1). Both channels are found by `PERI_SEL`, never by pair number:
//! mbedTLS allocates them separately and reconnects the TX one, shared with SHA, before every
//! run (IDF `esp_crypto_shared_gdma.c`).
//!
//! The run completes only when TX delivered the whole source and RX took the whole result;
//! otherwise the block stays at `STATE` 1 waiting for its DMA (UNVERIFIED). The RX walk hands its
//! last descriptor back with owner 0, which is what `esp_aes_dma_done` polls.

use pemu_core::sched::Scheduler;
use pemu_core::time::VTime;

use crate::periph::aes::Aes;
use crate::periph::gdma::{CHANNELS, DmaMem, Engine, PERI_AES};

/// What one `Wiring::AesDma` did.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Applied {
    /// A run was pending and was fed.
    pub ran: bool,
    /// Bytes the TX walk delivered.
    pub bytes_in: u32,
    /// Bytes the RX walk took.
    pub bytes_out: u32,
    /// The run got its whole source and delivered its whole result; its completion is scheduled.
    pub completes: bool,
    /// New level of the TX channel's interrupt source, when the walk changed it.
    pub tx_irq: Option<(usize, bool)>,
    /// New level of the RX channel's interrupt source, when the walk changed it.
    pub rx_irq: Option<(usize, bool)>,
}

pub fn run(
    aes: &mut Aes,
    gdma: &mut Engine,
    mem: &mut dyn DmaMem,
    now: VTime,
    sched: &mut Scheduler,
) -> Applied {
    let Some(run) = aes.take_dma() else {
        return Applied::default();
    };
    let mut applied = Applied {
        ran: true,
        ..Applied::default()
    };
    let source = match gdma.tx_channel_of(PERI_AES) {
        Some(ch) if run.blocks > 0 => {
            debug_assert!(ch < CHANNELS);
            let pull = gdma.tx_pull(ch, run.bytes(), mem);
            applied.tx_irq = pull.irq.map(|level| (ch, level));
            pull.bytes
        }
        _ => Vec::new(),
    };
    applied.bytes_in = source.len() as u32;
    let result = aes.dma_transform(run, &source);
    if !result.out.is_empty()
        && let Some(ch) = gdma.rx_channel_of(PERI_AES)
    {
        let push = gdma.rx_push(ch, &result.out, mem);
        applied.rx_irq = push.irq.map(|level| (ch, level));
        applied.bytes_out = push.taken;
    }
    applied.completes = result.whole && applied.bytes_out as usize == result.out.len();
    if applied.completes {
        aes.schedule_dma_done(run, now, sched);
    }
    applied
}

#[cfg(test)]
mod tests {
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;
    use pemu_core::sched::Owner;

    use crate::periph::Peripheral;
    use crate::periph::aes::{
        self, BLOCK_CBC, BLOCK_CFB8, BLOCK_CFB128, BLOCK_CTR, BLOCK_ECB, BLOCK_OFB, MODE_DEC_128,
        MODE_DEC_256, MODE_ENC_128, MODE_ENC_256, STATE_BUSY, STATE_DONE, STATE_IDLE, TAG_DMA_DONE,
        reg,
    };
    use crate::periph::gdma::layout;

    use super::*;

    const DRAM: u32 = 0x3FC8_0000;
    const TX_DESC: u32 = DRAM;
    const RX_DESC: u32 = DRAM + 0x200;
    const SRC: u32 = DRAM + 0x1000;
    const DST: u32 = DRAM + 0x8000;
    const OUT_LINK_START: u32 = 1 << 21;
    const IN_LINK_START: u32 = 1 << 22;

    /// NIST SP 800-38A appendix F: the four-block plaintext every example uses.
    const PLAIN: &str = "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51\
                         30c81c46a35ce411e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710";
    /// SP 800-38A F.1 to F.5 keys and F.2 to F.5 IV and initial counter block.
    const KEY128: &str = "2b7e151628aed2a6abf7158809cf4f3c";
    const KEY256: &str = "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4";
    const IV: &str = "000102030405060708090a0b0c0d0e0f";
    const ICB: &str = "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff";

    /// SP 800-38A appendix F ciphertexts of [`PLAIN`]: (block mode, key, IV, ciphertext). The
    /// first block of each is the published example; the rest was recomputed with OpenSSL.
    const VECTORS: [(u32, &str, &str, &str); 12] = [
        (
            BLOCK_ECB,
            KEY128,
            IV,
            "3ad77bb40d7a3660a89ecaf32466ef97f5d3d58503b9699de785895a96fdbaaf\
             43b1cd7f598ece23881b00e3ed0306887b0c785e27e8ad3f8223207104725dd4",
        ),
        (
            BLOCK_CBC,
            KEY128,
            IV,
            "7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2\
             73bed6b8e3c1743b7116e69e222295163ff1caa1681fac09120eca307586e1a7",
        ),
        (
            BLOCK_OFB,
            KEY128,
            IV,
            "3b3fd92eb72dad20333449f8e83cfb4a7789508d16918f03f53c52dac54ed825\
             9740051e9c5fecf64344f7a82260edcc304c6528f659c77866a510d9c1d6ae5e",
        ),
        (
            BLOCK_CTR,
            KEY128,
            ICB,
            "874d6191b620e3261bef6864990db6ce9806f66b7970fdff8617187bb9fffdff\
             5ae4df3edbd5d35e5b4f09020db03eab1e031dda2fbe03d1792170a0f3009cee",
        ),
        (
            BLOCK_CFB8,
            KEY128,
            IV,
            "3b79424c9c0dd436bace9e0ed4586a4f32b9ded50ae3ba69d472e88267fb5052\
             70cbad1e257691f7c47c5038297edda32ff26d0ed19174096161ecc14086dd62",
        ),
        (
            BLOCK_CFB128,
            KEY128,
            IV,
            "3b3fd92eb72dad20333449f8e83cfb4ac8a64537a0b3a93fcde3cdad9f1ce58b\
             26751f67a3cbb140b1808cf187a4f4dfc04b05357c5d1c0eeac4c66f9ff7f2e6",
        ),
        (
            BLOCK_ECB,
            KEY256,
            IV,
            "f3eed1bdb5d2a03c064b5a7e3db181f8591ccb10d410ed26dc5ba74a31362870\
             b6ed21b99ca6f4f9f153e7b1beafed1d23304b7a39f9f3ff067d8d8f9e24ecc7",
        ),
        (
            BLOCK_CBC,
            KEY256,
            IV,
            "f58c4c04d6e5f1ba779eabfb5f7bfbd69cfc4e967edb808d679f777bc6702c7d\
             39f23369a9d9bacfa530e26304231461b2eb05e2c39be9fcda6c19078c6a9d1b",
        ),
        (
            BLOCK_OFB,
            KEY256,
            IV,
            "dc7e84bfda79164b7ecd8486985d38604febdc6740d20b3ac88f6ad82a4fb08d\
             71ab47a086e86eedf39d1c5bba97c4080126141d67f37be8538f5a8be740e484",
        ),
        (
            BLOCK_CTR,
            KEY256,
            ICB,
            "601ec313775789a5b7a7f504bbf3d228f443e3ca4d62b59aca84e990cacaf5c5\
             2b0930daa23de94ce87017ba2d84988ddfc9c58db67aada613c2dd08457941a6",
        ),
        (
            BLOCK_CFB8,
            KEY256,
            IV,
            "dc1f1a8520a64db55fcc8ac554844e889700adc6e10c63cf2d8cd2d8ce668f3e\
             b9191719c47444fb43bff9b9883c2cd051120402009f974998c89d195722a75b",
        ),
        (
            BLOCK_CFB128,
            KEY256,
            IV,
            "dc7e84bfda79164b7ecd8486985d386039ffed143b28b1c832113c6331e5407b\
             df10132415e54b92a13ed0a8267ae2f975a385741ab9cef82031623d55b1e471",
        ),
    ];

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    struct Ram(Vec<u8>);

    impl Ram {
        fn new() -> Ram {
            Ram(vec![0; 0x2_0000])
        }

        /// Writes one `dma_descriptor_t` at `at` as `dma_desc_populate` fills it.
        fn desc(&mut self, at: u32, len: u32, suc_eof: bool, buffer: u32, next: u32) {
            let w0 = len | (len << 12) | (u32::from(suc_eof) << 30) | (1 << 31);
            for (i, word) in [w0, buffer, next].into_iter().enumerate() {
                self.write(at + 4 * i as u32, &word.to_le_bytes());
            }
        }

        fn word0(&mut self, at: u32) -> u32 {
            let mut w = [0u8; 4];
            self.read(at, &mut w);
            u32::from_le_bytes(w)
        }

        fn bytes(&mut self, at: u32, len: usize) -> Vec<u8> {
            let mut out = vec![0; len];
            self.read(at, &mut out);
            out
        }
    }

    impl DmaMem for Ram {
        fn read(&mut self, addr: u32, out: &mut [u8]) {
            for (i, b) in out.iter_mut().enumerate() {
                let at = addr.wrapping_add(i as u32).wrapping_sub(DRAM) as usize;
                *b = self.0.get(at).copied().unwrap_or(0);
            }
        }

        fn write(&mut self, addr: u32, data: &[u8]) {
            for (i, b) in data.iter().enumerate() {
                let at = addr.wrapping_add(i as u32).wrapping_sub(DRAM) as usize;
                if let Some(slot) = self.0.get_mut(at) {
                    *slot = *b;
                }
            }
        }
    }

    /// The AES block, the GDMA engine and guest memory, driven in the register order of IDF
    /// `esp_aes_process_dma`.
    struct Rig {
        aes: Aes,
        gdma: Engine,
        ram: Ram,
        sched: Scheduler,
        ledger: FidelityLedger,
        now: VTime,
        /// Different pairs, neither 0, so a model that assumed a pair number reads the wrong one.
        tx: usize,
        rx: usize,
        /// `INC_SEL` for a CTR run: false INC32, true INC128.
        inc128: bool,
    }

    impl Rig {
        fn new() -> Rig {
            let mut rig = Rig {
                aes: Aes::default(),
                gdma: Engine::default(),
                ram: Ram::new(),
                sched: Scheduler::default(),
                ledger: FidelityLedger::default(),
                now: VTime(0),
                tx: 2,
                rx: 1,
                inc128: false,
            };
            // SPI2 on pair 0 both ways, so the AES channels must be found by code.
            rig.gdma_write(layout(0).out_peri_sel, 0);
            rig.gdma_write(layout(0).in_peri_sel, 0);
            rig.gdma_write(layout(rig.tx).out_peri_sel, PERI_AES);
            rig.gdma_write(layout(rig.rx).in_peri_sel, PERI_AES);
            rig
        }

        fn gdma_write(&mut self, index: usize, val: u32) {
            let off = u32::from(crate::r#gen::regs_gdma::REGS[index].off);
            let _ = self
                .gdma
                .store(off, Size::B4, val, self.now, &mut self.ledger);
        }

        fn write(&mut self, off: u32, val: u32) -> aes::Stored {
            self.aes.store(
                off,
                Size::B4,
                val,
                self.now,
                &mut self.ledger,
                &mut self.sched,
            )
        }

        fn read(&mut self, off: u32) -> u32 {
            self.aes.load(off, Size::B4, self.now, &mut self.ledger)
        }

        fn put(&mut self, base: u32, data: &[u8]) {
            for (i, word) in data.chunks(4).enumerate() {
                let w = u32::from_le_bytes(word.try_into().expect("whole words"));
                let _ = self.write(base + 4 * i as u32, w);
            }
        }

        fn iv(&mut self) -> Vec<u8> {
            (0..4)
                .flat_map(|i| self.read(reg::IV + 4 * i).to_le_bytes())
                .collect()
        }

        fn chain(&mut self, desc: u32, buf: u32, parts: &[usize]) {
            let mut at = buf;
            for (i, len) in parts.iter().enumerate() {
                let last = i + 1 == parts.len();
                let next = if last { 0 } else { desc + 12 * (i as u32 + 1) };
                self.ram
                    .desc(desc + 12 * i as u32, *len as u32, last, at, next);
                at += *len as u32;
            }
        }

        fn start(&mut self) {
            let (tx, rx) = (layout(self.tx), layout(self.rx));
            self.gdma_write(tx.out_link, (TX_DESC & 0xF_FFFF) | OUT_LINK_START);
            self.gdma_write(rx.in_link, (RX_DESC & 0xF_FFFF) | IN_LINK_START);
        }

        fn apply(&mut self) -> Applied {
            run(
                &mut self.aes,
                &mut self.gdma,
                &mut self.ram,
                self.now,
                &mut self.sched,
            )
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

        /// One DMA run as `esp_aes_process_dma` does it, through DMA_EXIT. Returns what the RX
        /// walk wrote.
        #[allow(clippy::too_many_arguments)]
        fn dma(
            &mut self,
            mode: u32,
            key: &[u8],
            block_mode: u32,
            iv: Option<&[u8]>,
            source: &[u8],
            src_parts: &[usize],
            dst_parts: &[usize],
        ) -> Vec<u8> {
            self.put(reg::KEY, key);
            let _ = self.write(reg::MODE, mode);
            let _ = self.write(reg::BLOCK_MODE, block_mode);
            if block_mode == BLOCK_CTR {
                let _ = self.write(reg::INC_SEL, u32::from(self.inc128));
            }
            if let Some(iv) = iv {
                self.put(reg::IV, iv);
            }
            self.ram.write(SRC, source);
            self.chain(TX_DESC, SRC, src_parts);
            self.chain(RX_DESC, DST, dst_parts);
            self.start();
            let _ = self.write(reg::DMA_ENABLE, 1);
            let _ = self.write(reg::BLOCK_NUM, source.len().div_ceil(16) as u32);
            let stored = self.write(reg::TRIGGER, 1);
            assert!(
                stored.stop && stored.dma,
                "a DMA trigger asks for the wiring"
            );
            assert_eq!(self.read(reg::STATE), STATE_BUSY);
            let applied = self.apply();
            assert!(applied.completes, "{applied:?}");
            self.pump();
            assert_eq!(self.read(reg::STATE), STATE_DONE, "aes_hal_wait_done exits");
            let last = RX_DESC + 12 * (dst_parts.len() as u32 - 1);
            assert_eq!(
                self.ram.word0(last) >> 31,
                0,
                "esp_aes_dma_done: the output chain's last descriptor is back with the CPU"
            );
            let _ = self.write(reg::DMA_EXIT, 0);
            let _ = self.write(reg::DMA_ENABLE, 0);
            assert_eq!(self.read(reg::STATE), STATE_IDLE);
            let total: usize = dst_parts.iter().sum();
            self.ram.bytes(DST, total)
        }
    }

    #[test]
    fn sp_800_38a_vectors_for_every_block_mode_both_directions_and_both_key_sizes() {
        let plain = bytes(PLAIN);
        for (block_mode, key, iv, cipher) in VECTORS {
            let key = bytes(key);
            let (enc, dec) = if key.len() == 16 {
                (MODE_ENC_128, MODE_DEC_128)
            } else {
                (MODE_ENC_256, MODE_DEC_256)
            };
            let iv = (block_mode != BLOCK_ECB).then(|| bytes(iv));
            let mut rig = Rig::new();
            let got = rig.dma(enc, &key, block_mode, iv.as_deref(), &plain, &[64], &[64]);
            assert_eq!(
                hex(&got),
                cipher,
                "mode {block_mode} key {} encrypt",
                key.len()
            );
            let mut rig = Rig::new();
            let got = rig.dma(
                dec,
                &key,
                block_mode,
                iv.as_deref(),
                &bytes(cipher),
                &[64],
                &[64],
            );
            assert_eq!(
                hex(&got),
                PLAIN,
                "mode {block_mode} key {} decrypt",
                key.len()
            );
        }
    }

    /// IDF programs OFB and CTR with `ESP_AES_DECRYPT`; both use only the forward cipher
    /// (SP 800-38A 6.4, 6.5), so either direction gives the published ciphertext.
    #[test]
    fn ofb_and_ctr_ignore_the_direction_as_idf_programs_them() {
        let plain = bytes(PLAIN);
        for (block_mode, key, iv, cipher) in VECTORS {
            if block_mode != BLOCK_OFB && block_mode != BLOCK_CTR {
                continue;
            }
            let key = bytes(key);
            let mode = if key.len() == 16 {
                MODE_DEC_128
            } else {
                MODE_DEC_256
            };
            let mut rig = Rig::new();
            let got = rig.dma(
                mode,
                &key,
                block_mode,
                Some(&bytes(iv)),
                &plain,
                &[64],
                &[64],
            );
            assert_eq!(hex(&got), cipher, "mode {block_mode}");
        }
    }

    /// `IV_MEM` after a run is the chaining value IDF reads back (`aes_hal_read_iv`): two runs of
    /// two blocks give the one-run answer.
    #[test]
    fn the_iv_after_a_run_continues_the_next_run_for_every_mode() {
        let plain = bytes(PLAIN);
        for (block_mode, key, iv, cipher) in VECTORS {
            if block_mode == BLOCK_ECB {
                continue;
            }
            let key = bytes(key);
            let mode = if key.len() == 16 {
                MODE_ENC_128
            } else {
                MODE_ENC_256
            };
            let cipher = bytes(cipher);
            let mut rig = Rig::new();
            let first = rig.dma(
                mode,
                &key,
                block_mode,
                Some(&bytes(iv)),
                &plain[..32],
                &[32],
                &[32],
            );
            let carried = rig.iv();
            let expected: Vec<u8> = match block_mode {
                // The last ciphertext block, or the last 16 ciphertext bytes.
                BLOCK_CBC | BLOCK_CFB128 | BLOCK_CFB8 => cipher[16..32].to_vec(),
                // The last forward-cipher output: ciphertext xor plaintext.
                BLOCK_OFB => (16..32).map(|i| cipher[i] ^ plain[i]).collect(),
                // The next counter block: ICB + 2 under INC32.
                _ => {
                    let mut c = bytes(ICB);
                    let low = u32::from_be_bytes([c[12], c[13], c[14], c[15]]).wrapping_add(2);
                    c[12..].copy_from_slice(&low.to_be_bytes());
                    c
                }
            };
            assert_eq!(hex(&carried), hex(&expected), "mode {block_mode}");
            let second = rig.dma(mode, &key, block_mode, None, &plain[32..], &[32], &[32]);
            let mut joined = first;
            joined.extend_from_slice(&second);
            assert_eq!(hex(&joined), hex(&cipher), "mode {block_mode} continued");
        }
        // ECB leaves IV_MEM as it was.
        let mut rig = Rig::new();
        let marker = [0xA5u8; 16];
        let _ = rig.dma(
            MODE_ENC_128,
            &bytes(KEY128),
            BLOCK_ECB,
            Some(&marker),
            &plain,
            &[64],
            &[64],
        );
        assert_eq!(rig.iv(), marker);
    }

    /// Source and result each span a descriptor boundary mid-block, as the alignment buffers of
    /// `generate_descriptor_list` split them, and the chain is longer than the run.
    #[test]
    fn a_run_reads_block_num_blocks_across_a_descriptor_chain_boundary() {
        let plain = bytes(PLAIN);
        let cipher = bytes(VECTORS[1].3);
        let mut rig = Rig::new();
        let got = rig.dma(
            MODE_ENC_128,
            &bytes(KEY128),
            BLOCK_CBC,
            Some(&bytes(IV)),
            &plain,
            &[7, 40, 17],
            &[20, 12, 32],
        );
        assert_eq!(hex(&got), hex(&cipher));

        let mut rig = Rig::new();
        rig.put(reg::KEY, &bytes(KEY128));
        let _ = rig.write(reg::MODE, MODE_ENC_128);
        let _ = rig.write(reg::BLOCK_MODE, BLOCK_ECB);
        rig.ram.write(SRC, &plain);
        rig.chain(TX_DESC, SRC, &[64]);
        rig.chain(RX_DESC, DST, &[32]);
        rig.start();
        let _ = rig.write(reg::DMA_ENABLE, 1);
        let _ = rig.write(reg::BLOCK_NUM, 2);
        let _ = rig.write(reg::TRIGGER, 1);
        let applied = rig.apply();
        assert_eq!((applied.bytes_in, applied.bytes_out), (32, 32));
        assert!(applied.completes);
        assert_eq!(hex(&rig.ram.bytes(DST, 32)), &VECTORS[0].3[..64]);
    }

    /// TRM 18.5.1 TEXT-PADDING, which matches IDF's own padding of a CTR tail.
    #[test]
    fn a_partial_last_block_is_padded_with_zeros() {
        let plain = bytes(PLAIN);
        let mut padded = plain[..40].to_vec();
        padded.resize(48, 0);
        let mut rig = Rig::new();
        let want = rig.dma(
            MODE_ENC_128,
            &bytes(KEY128),
            BLOCK_CTR,
            Some(&bytes(ICB)),
            &padded,
            &[48],
            &[48],
        );
        assert_eq!(&want[..40], &bytes(VECTORS[3].3)[..40]);

        let mut rig = Rig::new();
        rig.put(reg::KEY, &bytes(KEY128));
        let _ = rig.write(reg::MODE, MODE_ENC_128);
        let _ = rig.write(reg::BLOCK_MODE, BLOCK_CTR);
        rig.put(reg::IV, &bytes(ICB));
        rig.ram.write(SRC, &plain[..40]);
        rig.chain(TX_DESC, SRC, &[40]);
        rig.chain(RX_DESC, DST, &[48]);
        rig.start();
        let _ = rig.write(reg::DMA_ENABLE, 1);
        let _ = rig.write(reg::BLOCK_NUM, 3);
        let _ = rig.write(reg::TRIGGER, 1);
        let applied = rig.apply();
        assert_eq!((applied.bytes_in, applied.bytes_out), (40, 48));
        assert!(applied.completes, "the padded source is the whole run");
        assert_eq!(rig.ram.bytes(DST, 48), want);
    }

    #[test]
    fn a_starved_run_stays_busy() {
        let setup = |rig: &mut Rig, blocks: u32, src: &[usize], dst: &[usize]| {
            rig.put(reg::KEY, &bytes(KEY128));
            let _ = rig.write(reg::MODE, MODE_ENC_128);
            let _ = rig.write(reg::BLOCK_MODE, BLOCK_ECB);
            rig.ram.write(SRC, &bytes(PLAIN));
            rig.chain(TX_DESC, SRC, src);
            rig.chain(RX_DESC, DST, dst);
            rig.start();
            let _ = rig.write(reg::DMA_ENABLE, 1);
            let _ = rig.write(reg::BLOCK_NUM, blocks);
            let _ = rig.write(reg::TRIGGER, 1);
            let applied = rig.apply();
            rig.pump();
            applied
        };

        // No TX channel bound to AES.
        let mut rig = Rig::new();
        let tx = layout(rig.tx).out_peri_sel;
        rig.gdma_write(tx, 0x3F);
        let applied = setup(&mut rig, 4, &[64], &[64]);
        assert!(
            applied.ran && !applied.completes && applied.bytes_in == 0,
            "{applied:?}"
        );
        assert_eq!(rig.read(reg::STATE), STATE_BUSY);
        assert_eq!(rig.sched.next_time(), None);

        // A source of two blocks for a run of four: the two are transformed and delivered.
        let mut rig = Rig::new();
        let applied = setup(&mut rig, 4, &[32], &[64]);
        assert_eq!((applied.bytes_in, applied.bytes_out), (32, 32));
        assert!(!applied.completes);
        assert_eq!(rig.read(reg::STATE), STATE_BUSY);
        assert_eq!(hex(&rig.ram.bytes(DST, 32)), &VECTORS[0].3[..64]);

        // An RX chain too short for the result.
        let mut rig = Rig::new();
        let applied = setup(&mut rig, 4, &[64], &[48]);
        assert_eq!((applied.bytes_in, applied.bytes_out), (64, 48));
        assert!(!applied.completes);
        assert_eq!(rig.read(reg::STATE), STATE_BUSY);

        // No RX channel bound to AES.
        let mut rig = Rig::new();
        let rx = layout(rig.rx).in_peri_sel;
        rig.gdma_write(rx, 0x3F);
        let applied = setup(&mut rig, 4, &[64], &[64]);
        assert!(!applied.completes && applied.bytes_out == 0, "{applied:?}");
        assert_eq!(rig.read(reg::STATE), STATE_BUSY);

        // BLOCK_NUM 0: nothing moves and the run completes at once.
        let mut rig = Rig::new();
        let applied = setup(&mut rig, 0, &[64], &[64]);
        assert!(applied.completes && applied.bytes_in == 0 && applied.bytes_out == 0);
        assert_eq!(rig.read(reg::STATE), STATE_DONE);
    }

    #[test]
    fn the_run_completes_after_its_blocks_at_the_profile_s_block_cost() {
        const BLOCK_PS: u64 = 372_000;
        let mut rig = Rig::new();
        rig.aes.set_op_ps(BLOCK_PS);
        rig.put(reg::KEY, &bytes(KEY128));
        let _ = rig.write(reg::MODE, MODE_ENC_128);
        let _ = rig.write(reg::BLOCK_MODE, BLOCK_ECB);
        let _ = rig.write(reg::INT_ENA, 1);
        rig.ram.write(SRC, &bytes(PLAIN));
        rig.chain(TX_DESC, SRC, &[64]);
        rig.chain(RX_DESC, DST, &[64]);
        rig.start();
        let _ = rig.write(reg::DMA_ENABLE, 1);
        let _ = rig.write(reg::BLOCK_NUM, 4);
        let _ = rig.write(reg::TRIGGER, 1);
        assert!(rig.apply().completes);
        assert_eq!(rig.sched.next_time(), Some(VTime(4 * BLOCK_PS)));
        assert_eq!(rig.read(reg::STATE), STATE_BUSY);
        assert!(!rig.aes.irq_level());
        rig.pump();
        assert_eq!(rig.now, VTime(4 * BLOCK_PS));
        assert_eq!(rig.read(reg::STATE), STATE_DONE);
        assert!(rig.aes.irq_level(), "the completion raised source 48");

        let mut fast = Rig::new();
        let _ = fast.dma(
            MODE_ENC_128,
            &bytes(KEY128),
            BLOCK_ECB,
            None,
            &bytes(PLAIN),
            &[64],
            &[64],
        );
        assert_eq!(
            fast.now,
            VTime(0),
            "fast completes at the trigger's instant"
        );
    }

    #[test]
    fn a_reserved_block_mode_completes_without_output_and_no_run_does_nothing() {
        let mut rig = Rig::new();
        assert_eq!(rig.apply(), Applied::default());
        rig.put(reg::KEY, &bytes(KEY128));
        let _ = rig.write(reg::MODE, MODE_ENC_128);
        let _ = rig.write(reg::BLOCK_MODE, 6);
        rig.ram.write(SRC, &bytes(PLAIN));
        rig.chain(TX_DESC, SRC, &[64]);
        rig.chain(RX_DESC, DST, &[64]);
        rig.start();
        let _ = rig.write(reg::DMA_ENABLE, 1);
        let _ = rig.write(reg::BLOCK_NUM, 4);
        let _ = rig.write(reg::TRIGGER, 1);
        let applied = rig.apply();
        assert!(applied.completes && applied.bytes_out == 0, "{applied:?}");
        rig.pump();
        assert_eq!(rig.read(reg::STATE), STATE_DONE);
        assert_eq!(rig.ram.bytes(DST, 64), vec![0; 64]);
    }

    /// TRM 18.5.3: with the low counter word at 0xFFFFFFFF, INC32 wraps inside the low word and
    /// INC128 carries into byte 11. The SP 800-38A counters never carry, so without this test a
    /// model that read the bit backwards would pass every vector.
    #[test]
    fn inc_sel_chooses_the_counter_width_of_a_dma_run() {
        let key = bytes(KEY128);
        let mut counter = [0u8; 16];
        counter[11] = 0x01;
        counter[12..].copy_from_slice(&[0xFF; 4]);
        for (inc128, byte11) in [(false, 0x01u8), (true, 0x02)] {
            let mut rig = Rig::new();
            rig.inc128 = inc128;
            let stream = rig.dma(
                MODE_ENC_128,
                &key,
                BLOCK_CTR,
                Some(&counter),
                &[0; 32],
                &[32],
                &[32],
            );
            let mut next = counter;
            next[11] = byte11;
            next[12..].copy_from_slice(&[0; 4]);
            let mut after = next;
            after[15] = 1;
            assert_eq!(
                hex(&rig.iv()),
                hex(&after),
                "INC_SEL {inc128}: IV_MEM after"
            );
            let mut ecb = Rig::new();
            let want = ecb.dma(MODE_ENC_128, &key, BLOCK_ECB, None, &next, &[16], &[16]);
            assert_eq!(
                hex(&stream[16..]),
                hex(&want),
                "INC_SEL {inc128}: the second block's key stream is E(the incremented counter)"
            );
        }
    }
}
