//! APB_CTRL, the block IDF headers call SYSCON, at 0x60026000 (`specs/blocks/apb_ctrl.toml`).
//!
//! Everything is storage except `SYSCON_RND_DATA`, which reads a fresh word from the machine's
//! deterministic random stream (`Cx::rng`), never a host RNG: the same seed reads the same
//! sequence, so a boot is reproducible. That costs entropy quality, hence class C. The stream
//! lives in the `rng` snapshot section, so a reset does not rewind it, and nothing gates the
//! register on the entropy-source enables.

use pemu_core::fidelity::{Fidelity, FidelityLedger, TouchAccess};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::rng::RngView;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::block::ApbCtrl;
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};
use crate::r#gen::regs_apb_ctrl::{REG_COUNT, REGS, idx};
use crate::regs::{self, TouchAt};

const TOUCH_WORDS: usize = REG_COUNT.div_ceil(64);

/// Model of the `apb_ctrl` row of the `c3_devices!` table.
#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    #[serde(with = "regs_serde")]
    regs: RegStore<REG_COUNT>,
    touched: [u64; TOUCH_WORDS],
    /// Words drawn from the stream since power-on, for the receipt and for a determinism test.
    rng_reads: u64,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            regs: RegStore::new(&REGS),
            touched: [0; TOUCH_WORDS],
            rng_reads: 0,
        }
    }
}

impl Model {
    /// Words `SYSCON_RND_DATA` has drawn from the stream since power-on.
    pub fn rng_reads(&self) -> u64 {
        self.rng_reads
    }

    /// The register table.
    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    /// Restores the registers of the scopes this reset clears; the random stream is not rewound.
    pub fn reset_to(&mut self, kind: ResetKind) {
        if kind.fanout.reaches_all_blocks() {
            self.regs.reset(kind.scope);
        }
    }

    /// Reads `size` bytes at block offset `off` and reports the first touch.
    ///
    /// Any read of `SYSCON_RND_DATA` draws one word, whatever the width: the register is a port,
    /// so a narrow read consumes a word and returns its addressed bytes (UNVERIFIED).
    pub fn load(
        &mut self,
        off: u32,
        size: Size,
        now: VTime,
        rng: &mut RngView<'_>,
        ledger: &mut FidelityLedger,
    ) -> u32 {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((i, byte)) = regs::reg_at(&REGS, off) else {
            regs::hole(off, TouchAccess::Read, at, ledger);
            return 0;
        };
        regs::touch(&mut self.touched, &REGS, i, TouchAccess::Read, at, ledger);
        if i == idx::SYSCON_RND_DATA {
            self.regs.set(i, rng.next_u32());
            self.rng_reads = self.rng_reads.saturating_add(1);
        }
        self.regs.read(i, byte, size)
    }

    /// Writes the low `size` bytes of `val` at block offset `off` and reports the first touch.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((i, byte)) = regs::reg_at(&REGS, off) else {
            regs::hole(off, TouchAccess::Write, at, ledger);
            return;
        };
        regs::touch(&mut self.touched, &REGS, i, TouchAccess::Write, at, ledger);
        self.regs.write(i, byte, size, val);
    }
}

impl Peripheral for Model {
    const ID: PeriphId = <ApbCtrl as Block>::ID;
    const BASE: u32 = <ApbCtrl as Block>::BASE;
    const SIZE: u32 = <ApbCtrl as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        let _ = cx;
        self.reset_to(kind);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, &mut cx.rng, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        self.store(off, size, val, cx.now, cx.ledger);
        RegWrite {
            stop: false,
            wiring: Wiring::None,
        }
    }

    /// `SYSCON_RND_DATA` answers a different word on every read; everything else changes only on
    /// a guest write.
    fn stable_until(&self, off: u32, cx: &Cx) -> Stability {
        let _ = cx;
        match regs::reg_at(&REGS, off).map(|(i, _)| i) {
            Some(idx::SYSCON_RND_DATA) => Stability::Never,
            _ => Stability::UntilInput,
        }
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        regs::reg_at(&REGS, off).map_or(Fidelity::U, |(i, _)| REGS[i].class)
    }
}

crate::regs::store_serde!();

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::reset::ResetCause;
    use pemu_core::rng::{DetRng, RngStream};

    const OFF_SYSCLK_CONF: u32 = 0x000;
    const OFF_WIFI_CLK_EN: u32 = 0x014;
    const OFF_CLKGATE_FORCE_ON: u32 = 0x0A4;
    const OFF_RND_DATA: u32 = 0x0B0;
    const OFF_DATE: u32 = 0x3FC;

    const T: VTime = VTime(11);

    /// The model, its ledger and a deterministic stream.
    struct Harness {
        m: Model,
        l: FidelityLedger,
        rng: DetRng,
    }

    impl Harness {
        fn with_seed(seed: u64) -> Harness {
            Harness {
                m: Model::default(),
                l: FidelityLedger::default(),
                rng: DetRng::new(seed),
            }
        }

        fn read(&mut self, off: u32) -> u32 {
            let mut view = self.rng.stream(RngStream::GUEST_ENTROPY);
            self.m.load(off, Size::B4, T, &mut view, &mut self.l)
        }

        fn read_sized(&mut self, off: u32, size: Size) -> u32 {
            let mut view = self.rng.stream(RngStream::GUEST_ENTROPY);
            self.m.load(off, size, T, &mut view, &mut self.l)
        }

        fn write(&mut self, off: u32, val: u32) {
            self.m.store(off, Size::B4, val, T, &mut self.l);
        }
    }

    #[test]
    fn identity_matches_the_c3_devices_row() {
        assert_eq!(<Model as Peripheral>::ID, <ApbCtrl as Block>::ID);
        assert_eq!(<Model as Peripheral>::BASE, 0x6002_6000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x1000);
    }

    #[test]
    fn rnd_data_comes_from_the_deterministic_stream() {
        let mut a = Harness::with_seed(7);
        let first: Vec<u32> = (0..8).map(|_| a.read(OFF_RND_DATA)).collect();
        assert_eq!(a.m.rng_reads(), 8);

        let mut b = Harness::with_seed(7);
        let again: Vec<u32> = (0..8).map(|_| b.read(OFF_RND_DATA)).collect();
        assert_eq!(first, again, "the same seed replays the same words");

        let mut c = Harness::with_seed(8);
        let other: Vec<u32> = (0..8).map(|_| c.read(OFF_RND_DATA)).collect();
        assert_ne!(first, other, "a different seed gives a different stream");

        // `esp_random` XORs successive reads, so two reads in a row must differ.
        assert!(
            first.windows(2).any(|w| w[0] != w[1]),
            "the register is not constant"
        );
    }

    #[test]
    fn rnd_data_ignores_writes() {
        let mut h = Harness::with_seed(1);
        let expected = h.read(OFF_RND_DATA);
        let mut probe = Harness::with_seed(1);
        assert_eq!(probe.read(OFF_RND_DATA), expected);

        h.write(OFF_RND_DATA, 0xDEAD_BEEF);
        assert_ne!(h.read(OFF_RND_DATA), 0xDEAD_BEEF);
        assert_eq!(h.m.rng_reads(), 2);
    }

    #[test]
    fn a_narrow_read_of_rnd_data_still_draws_a_word() {
        let mut h = Harness::with_seed(3);
        let mut expect = DetRng::new(3);
        let word = expect.stream(RngStream::GUEST_ENTROPY).next_u32();
        assert_eq!(h.read_sized(OFF_RND_DATA, Size::B1), word & 0xFF);
        assert_eq!(h.m.rng_reads(), 1);
    }

    #[test]
    fn a_reset_does_not_rewind_the_stream() {
        let mut h = Harness::with_seed(5);
        let first = h.read(OFF_RND_DATA);
        h.m.reset_to(ResetKind::of(ResetCause::POWERON).expect("documented"));
        let second = h.read(OFF_RND_DATA);
        let mut expect = DetRng::new(5);
        let mut view = expect.stream(RngStream::GUEST_ENTROPY);
        assert_eq!(first, view.next_u32());
        assert_eq!(second, view.next_u32());
    }

    #[test]
    fn the_rest_of_the_block_is_storage() {
        let mut h = Harness::with_seed(1);
        assert_eq!(h.read(OFF_WIFI_CLK_EN), 0xFFFC_E030, "reset value");
        assert_eq!(h.read(OFF_SYSCLK_CONF), 0x1, "reset value");
        assert_eq!(h.read(OFF_DATE), 0x0200_7210, "DATE constant");

        h.write(OFF_WIFI_CLK_EN, 0);
        assert_eq!(h.read(OFF_WIFI_CLK_EN), 0);
        h.write(OFF_CLKGATE_FORCE_ON, 0x3F);
        assert_eq!(h.read(OFF_CLKGATE_FORCE_ON), 0x3F);

        h.m.reset_to(ResetKind::of(ResetCause::POWERON).expect("documented"));
        assert_eq!(h.read(OFF_WIFI_CLK_EN), 0xFFFC_E030);
    }

    /// QEMU answers "QEMU" at 0x600263F8 as an emulator marker; silicon does not.
    #[test]
    fn the_reserved_word_does_not_announce_an_emulator() {
        let mut h = Harness::with_seed(1);
        assert_eq!(h.read(0x3F8), 0);
    }

    #[test]
    fn fidelity_comes_from_the_generated_table() {
        let m = Model::default();
        assert_eq!(
            m.fidelity(OFF_RND_DATA),
            Fidelity::C,
            "a deliberate approximation"
        );
        assert_eq!(m.fidelity(OFF_WIFI_CLK_EN), Fidelity::B);
        assert_eq!(m.fidelity(OFF_DATE), Fidelity::U);
    }
}
