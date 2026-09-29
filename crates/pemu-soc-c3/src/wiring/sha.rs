//! `Wiring::ShaDma`: the one place the SHA block and the GDMA TX channel bound to it meet
//! (`specs/blocks/sha.toml`).
//!
//! A `DMA_START` or `DMA_CONTINUE` write records a run of `BLOCK_NUM` blocks; this step pulls
//! `BLOCK_NUM x 64` bytes through the TX descriptor walk and hands them to the model. The channel
//! is found by its `PERI_SEL` code, never by pair number, because the pair mbedTLS gets depends
//! on allocation order (IDF `esp_crypto_shared_gdma.c` reconnects it before every run).
//!
//! The walk stops at the first `suc_eof` descriptor or when the run has its bytes. A short walk,
//! or no channel bound to SHA, leaves the block waiting (UNVERIFIED).

use pemu_core::sched::Scheduler;
use pemu_core::time::VTime;

use crate::periph::gdma::{CHANNELS, DmaMem, Engine, PERI_SHA};
use crate::periph::sha::Sha;

/// What one `Wiring::ShaDma` did.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Applied {
    /// A run was pending and was fed.
    pub ran: bool,
    /// Bytes the TX walk delivered.
    pub bytes: u32,
    /// The run got all its bytes and its completion is scheduled.
    pub completes: bool,
    /// New level of a GDMA channel's interrupt source, when the walk changed it.
    pub gdma_irq: Option<(usize, bool)>,
}

/// Applies `Wiring::ShaDma`.
pub fn run(
    sha: &mut Sha,
    gdma: &mut Engine,
    mem: &mut dyn DmaMem,
    now: VTime,
    sched: &mut Scheduler,
) -> Applied {
    let Some(run) = sha.take_dma() else {
        return Applied::default();
    };
    let mut applied = Applied {
        ran: true,
        ..Applied::default()
    };
    let bytes = match gdma.tx_channel_of(PERI_SHA) {
        Some(ch) if run.blocks > 0 => {
            debug_assert!(ch < CHANNELS);
            let pull = gdma.tx_pull(ch, run.bytes(), mem);
            applied.gdma_irq = pull.irq.map(|level| (ch, level));
            pull.bytes
        }
        _ => Vec::new(),
    };
    applied.bytes = bytes.len() as u32;
    applied.completes = sha.dma_feed(run, &bytes, now, sched);
    applied
}

#[cfg(test)]
mod tests {
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;
    use pemu_core::sched::Owner;

    use crate::periph::Peripheral;
    use crate::periph::gdma::layout;
    use crate::periph::sha::{self, MODE_SHA1, MODE_SHA224, MODE_SHA256, TAG_DMA_DONE, reg};

    use super::*;

    const DRAM: u32 = 0x3FC8_0000;
    const DESC: u32 = DRAM;
    const BUF: u32 = DRAM + 0x100;
    const OUT_LINK_START: u32 = 1 << 21;

    /// Guest memory behind [`DmaMem`], based at [`DRAM`].
    struct Ram(Vec<u8>);

    impl Ram {
        fn new() -> Ram {
            Ram(vec![0; 0x2_0000])
        }

        /// Writes one TX `dma_descriptor_t` at `at` as `esp_sha_dma_process` fills it.
        fn desc(&mut self, at: u32, len: u32, suc_eof: bool, buffer: u32, next: u32) {
            let w0 = len | (len << 12) | (u32::from(suc_eof) << 30) | (1 << 31);
            for (i, word) in [w0, buffer, next].into_iter().enumerate() {
                self.write(at + 4 * i as u32, &word.to_le_bytes());
            }
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

    /// The SHA block, the GDMA engine and guest memory, driven in the register order of the IDF
    /// mbedTLS port (`esp_sha_dma_process`, `sha_hal_hash_dma`).
    struct Rig {
        sha: Sha,
        gdma: Engine,
        ram: Ram,
        sched: Scheduler,
        ledger: FidelityLedger,
        now: VTime,
        /// Not 0, so a model that assumed a pair number would read the wrong channel.
        ch: usize,
    }

    impl Rig {
        fn new() -> Rig {
            let mut rig = Rig {
                sha: Sha::default(),
                gdma: Engine::default(),
                ram: Ram::new(),
                sched: Scheduler::default(),
                ledger: FidelityLedger::default(),
                now: VTime(0),
                ch: 2,
            };
            // Another peripheral on channel 0 (SPI2), so the SHA channel is found by code.
            rig.gdma_write(layout(0).out_peri_sel, 0);
            rig.gdma_write(layout(rig.ch).out_peri_sel, PERI_SHA);
            rig
        }

        fn gdma_write(&mut self, index: usize, val: u32) {
            let off = u32::from(crate::r#gen::regs_gdma::REGS[index].off);
            let _ = self
                .gdma
                .store(off, Size::B4, val, self.now, &mut self.ledger);
        }

        fn sha_write(&mut self, off: u32, val: u32) -> sha::Stored {
            self.sha.store(
                off,
                Size::B4,
                val,
                self.now,
                &mut self.ledger,
                &mut self.sched,
            )
        }

        fn sha_read(&mut self, off: u32) -> u32 {
            self.sha.load(off, Size::B4, self.now, &mut self.ledger)
        }

        fn start_tx(&mut self, head: u32) {
            let l = layout(self.ch);
            self.gdma_write(l.out_link, (head & 0xF_FFFF) | OUT_LINK_START);
        }

        fn apply(&mut self) -> Applied {
            run(
                &mut self.sha,
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
                assert_eq!(key.owner, Owner::Periph(Sha::ID));
                match key.tag {
                    TAG_DMA_DONE => self.sha.complete_dma(),
                    _ => self.sha.complete(),
                }
            }
        }

        fn dma_run(&mut self, parts: &[&[u8]], first: bool) -> Applied {
            let mut at = BUF;
            let mut descs = Vec::new();
            for part in parts {
                self.ram.write(at, part);
                descs.push((at, part.len() as u32));
                at += (part.len() as u32).next_multiple_of(4);
            }
            for (i, (buf, len)) in descs.iter().enumerate() {
                let last = i + 1 == descs.len();
                let next = if last { 0 } else { DESC + 12 * (i as u32 + 1) };
                self.ram.desc(DESC + 12 * i as u32, *len, last, *buf, next);
            }
            self.start_tx(DESC);
            let total: usize = parts.iter().map(|p| p.len()).sum();
            self.sha_write(reg::BLOCK_NUM, (total / 64) as u32);
            let stored = self.sha_write(
                if first {
                    reg::DMA_START
                } else {
                    reg::DMA_CONTINUE
                },
                1,
            );
            assert!(
                stored.stop && stored.dma,
                "a DMA trigger stops and asks for the wiring"
            );
            assert_eq!(self.sha_read(reg::BUSY), 1, "BUSY is set from the trigger");
            let applied = self.apply();
            self.pump();
            applied
        }

        fn digest(&mut self, len: usize) -> Vec<u8> {
            let mut out = Vec::new();
            for i in 0..8 {
                out.extend_from_slice(&self.sha_read(reg::H + 4 * i).to_le_bytes());
            }
            out.truncate(len);
            out
        }

        /// Hashes `message` as mbedTLS does a long update: runs of at most `per_run` blocks,
        /// restoring `H` before each `DMA_CONTINUE` (`esp_internal_sha_update_state`).
        fn hash(&mut self, mode: u32, message: &[u8], per_run: usize) -> Vec<u8> {
            self.sha_write(reg::MODE, mode);
            let padded = pad(message);
            let mut first = true;
            for run in padded.chunks(per_run * 64) {
                if !first {
                    let saved: Vec<u32> = (0..8).map(|i| self.sha_read(reg::H + 4 * i)).collect();
                    for (i, w) in saved.iter().enumerate() {
                        self.sha_write(reg::H + 4 * i as u32, *w);
                    }
                }
                let applied = self.dma_run(&[run], first);
                assert!(applied.completes, "{applied:?}");
                assert_eq!(self.sha_read(reg::BUSY), 0);
                first = false;
            }
            self.digest(match mode {
                MODE_SHA1 => 20,
                MODE_SHA224 => 28,
                _ => 32,
            })
        }
    }

    /// FIPS 180-4 §5.1.1 padding.
    fn pad(message: &[u8]) -> Vec<u8> {
        let mut padded = message.to_vec();
        padded.push(0x80);
        while padded.len() % 64 != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&(message.len() as u64 * 8).to_be_bytes());
        padded
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn dma_mode_gives_the_standard_vectors_for_every_mode() {
        let two = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        let many = vec![b'a'; 1000];
        let cases: [(u32, &[u8], &str); 9] = [
            (
                MODE_SHA256,
                b"abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                MODE_SHA256,
                two,
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                MODE_SHA256,
                &many,
                "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3",
            ),
            (
                MODE_SHA1,
                b"abc",
                "a9993e364706816aba3e25717850c26c9cd0d89d",
            ),
            (MODE_SHA1, two, "84983e441c3bd26ebaae4aa1f95129e5e54670f1"),
            (MODE_SHA1, &many, "291e9a6c66994949b57ba5e650361e98fc36b1ba"),
            (
                MODE_SHA224,
                b"abc",
                "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7",
            ),
            (
                MODE_SHA224,
                two,
                "75388b16512776cc5dba5da1fd890150b0c6455cb4f58b1952522525",
            ),
            (
                MODE_SHA224,
                &many,
                "4e8f0ce90b64661a2b5e84be6d93a7d9b76871062f1814433d04a03d",
            ),
        ];
        for (mode, message, want) in cases {
            let mut rig = Rig::new();
            assert_eq!(
                hex(&rig.hash(mode, message, 63)),
                want,
                "mode {mode}, {} bytes",
                message.len()
            );
            // The same message split into one-block runs continues across DMA_CONTINUE.
            let mut rig = Rig::new();
            assert_eq!(
                hex(&rig.hash(mode, message, 1)),
                want,
                "mode {mode}, one block per run"
            );
        }
    }

    /// mbedTLS chains its buffered block (no EOF) to the input (EOF); the split here is not on
    /// a block boundary, so the walk must join the two.
    #[test]
    fn a_run_reads_across_a_descriptor_chain_boundary() {
        let mut rig = Rig::new();
        rig.sha_write(reg::MODE, MODE_SHA256);
        let padded = pad(&[b'a'; 1000]);
        assert_eq!(padded.len(), 1024);
        let (head, tail) = padded.split_at(100);
        let applied = rig.dma_run(&[head, tail], true);
        assert_eq!(applied.bytes, 1024);
        assert!(applied.completes);
        assert_eq!(
            hex(&rig.digest(32)),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    /// TRM 21.4.2.1: block mode and DMA mode share one state.
    #[test]
    fn dma_continues_a_block_mode_digest_and_the_other_way_round() {
        let padded = pad(&[b'a'; 1000]);
        let want = "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3";
        let block = |rig: &mut Rig, bytes: &[u8], first: bool| {
            for (i, word) in bytes.chunks(4).enumerate() {
                let w = u32::from_le_bytes(word.try_into().expect("64 splits by 4"));
                rig.sha_write(reg::TEXT + 4 * i as u32, w);
            }
            let _ = rig.sha_write(if first { reg::START } else { reg::CONTINUE }, 1);
            rig.pump();
        };

        let mut rig = Rig::new();
        rig.sha_write(reg::MODE, MODE_SHA256);
        block(&mut rig, &padded[..64], true);
        assert!(rig.dma_run(&[&padded[64..]], false).completes);
        assert_eq!(hex(&rig.digest(32)), want, "START, then DMA_CONTINUE");

        let mut rig = Rig::new();
        rig.sha_write(reg::MODE, MODE_SHA256);
        assert!(rig.dma_run(&[&padded[..960]], true).completes);
        block(&mut rig, &padded[960..], false);
        assert_eq!(hex(&rig.digest(32)), want, "DMA_START, then CONTINUE");
    }

    #[test]
    fn dma_start_restarts_and_dma_continue_does_not() {
        let mut rig = Rig::new();
        rig.sha_write(reg::MODE, MODE_SHA256);
        let abc = pad(b"abc");
        let want = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(rig.dma_run(&[&abc], true).completes);
        assert_eq!(hex(&rig.digest(32)), want);
        assert!(rig.dma_run(&[&abc], false).completes);
        assert_ne!(
            hex(&rig.digest(32)),
            want,
            "DMA_CONTINUE compressed into the old state"
        );
        assert!(rig.dma_run(&[&abc], true).completes);
        assert_eq!(
            hex(&rig.digest(32)),
            want,
            "DMA_START loaded the initial state again"
        );
    }

    /// `BLOCK_NUM` keeps bits 5 to 0 (TRM register 21.10).
    #[test]
    fn a_run_reads_block_num_blocks_and_block_num_is_six_bits() {
        let mut rig = Rig::new();
        rig.sha_write(reg::BLOCK_NUM, 0xFFFF_FFC1);
        assert_eq!(rig.sha_read(reg::BLOCK_NUM), 1);

        rig.sha_write(reg::MODE, MODE_SHA256);
        let abc = pad(b"abc");
        let mut chain = abc.clone();
        chain.extend_from_slice(&[0x5A; 64]);
        rig.ram.write(BUF, &chain);
        rig.ram.desc(DESC, 128, true, BUF, 0);
        rig.start_tx(DESC);
        rig.sha_write(reg::BLOCK_NUM, 1);
        let _ = rig.sha_write(reg::DMA_START, 1);
        let applied = rig.apply();
        rig.pump();
        assert_eq!(applied.bytes, 64);
        assert_eq!(
            hex(&rig.digest(32)),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn busy_holds_for_the_run_s_blocks_at_the_profile_s_block_cost() {
        let mut rig = Rig::new();
        rig.sha.set_block_ps(sha::DEVICE_BLOCK_PS);
        rig.sha_write(reg::MODE, MODE_SHA256);
        let data = pad(&[b'a'; 1000]);
        rig.ram.write(BUF, &data);
        rig.ram.desc(DESC, 1024, true, BUF, 0);
        rig.start_tx(DESC);
        rig.sha_write(reg::BLOCK_NUM, 16);
        let _ = rig.sha_write(reg::DMA_START, 1);
        let applied = rig.apply();
        assert!(applied.completes);
        assert_eq!(
            rig.sched.next_time(),
            Some(VTime(16 * sha::DEVICE_BLOCK_PS))
        );
        assert_eq!(rig.sha_read(reg::BUSY), 1);
        rig.pump();
        assert_eq!(rig.now, VTime(16 * sha::DEVICE_BLOCK_PS));
        assert_eq!(rig.sha_read(reg::BUSY), 0);

        let mut fast = Rig::new();
        fast.sha_write(reg::MODE, MODE_SHA256);
        let _ = fast.dma_run(&[&data], true);
        assert_eq!(
            fast.now,
            VTime(0),
            "fast completes at the trigger's instant"
        );
    }

    /// TRM 21.4.4: source 49 follows the latch while `INT_ENA` is set; block mode never raises it.
    #[test]
    fn a_dma_completion_raises_the_interrupt_until_clear_irq() {
        let mut rig = Rig::new();
        rig.sha_write(reg::MODE, MODE_SHA256);
        rig.sha_write(reg::INT_ENA, 1);
        let abc = pad(b"abc");
        for (i, word) in abc.chunks(4).enumerate() {
            let w = u32::from_le_bytes(word.try_into().expect("64 splits by 4"));
            rig.sha_write(reg::TEXT + 4 * i as u32, w);
        }
        let _ = rig.sha_write(reg::START, 1);
        rig.pump();
        assert!(!rig.sha.irq_level(), "block mode raises no interrupt");

        rig.sha.set_block_ps(sha::DEVICE_BLOCK_PS);
        rig.ram.write(BUF, &abc);
        rig.ram.desc(DESC, 64, true, BUF, 0);
        rig.start_tx(DESC);
        rig.sha_write(reg::BLOCK_NUM, 1);
        let _ = rig.sha_write(reg::DMA_START, 1);
        let _ = rig.apply();
        assert!(!rig.sha.irq_level(), "not before the completion");
        rig.pump();
        assert!(rig.sha.irq_level(), "the completion raised it");

        rig.sha_write(reg::INT_ENA, 0);
        assert!(!rig.sha.irq_level(), "INT_ENA gates the level");
        rig.sha_write(reg::INT_ENA, 1);
        assert!(rig.sha.irq_level(), "the latch survived the enable toggle");
        rig.sha_write(reg::CLEAR_IRQ, 0);
        assert!(rig.sha.irq_level(), "CLEAR_IRQ written 0 clears nothing");
        rig.sha_write(reg::CLEAR_IRQ, 1);
        assert!(!rig.sha.irq_level(), "CLEAR_IRQ cleared it");
        assert_eq!(rig.sha_read(reg::CLEAR_IRQ), 0, "a write-trigger reads 0");
    }

    #[test]
    fn a_starved_run_keeps_busy_and_a_trigger_while_busy_is_ignored() {
        let mut rig = Rig::new();
        rig.gdma_write(layout(rig.ch).out_peri_sel, 0x3F);
        rig.sha_write(reg::MODE, MODE_SHA256);
        let abc = pad(b"abc");
        rig.ram.write(BUF, &abc);
        rig.ram.desc(DESC, 64, true, BUF, 0);
        rig.start_tx(DESC);
        rig.sha_write(reg::BLOCK_NUM, 1);
        let _ = rig.sha_write(reg::DMA_START, 1);
        let applied = rig.apply();
        assert!(
            applied.ran && !applied.completes && applied.bytes == 0,
            "{applied:?}"
        );
        assert_eq!(rig.sched.next_time(), None);
        assert_eq!(rig.sha_read(reg::BUSY), 1);
        let again = rig.sha_write(reg::DMA_START, 1);
        assert_eq!(again, sha::Stored::default(), "ignored while BUSY");

        // A chain of one block for a run of two: the one block is compressed, BUSY stays.
        let mut rig = Rig::new();
        rig.sha_write(reg::MODE, MODE_SHA256);
        rig.ram.write(BUF, &abc);
        rig.ram.desc(DESC, 64, true, BUF, 0);
        rig.start_tx(DESC);
        rig.sha_write(reg::BLOCK_NUM, 2);
        let _ = rig.sha_write(reg::DMA_START, 1);
        let applied = rig.apply();
        assert_eq!(applied.bytes, 64);
        assert!(!applied.completes);
        assert_eq!(rig.sha_read(reg::BUSY), 1);
        assert_eq!(
            hex(&rig.digest(32)),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "the block that arrived was compressed"
        );
    }

    #[test]
    fn the_wiring_without_a_pending_run_does_nothing() {
        let mut rig = Rig::new();
        assert_eq!(rig.apply(), Applied::default());
        assert!(!rig.sha.busy());
        assert_eq!(
            rig.sha.fidelity(reg::DMA_START),
            rig.sha.fidelity(reg::DMA_CONTINUE)
        );
    }
}
