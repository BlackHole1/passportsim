//! Interrupt fabric (TRM interrupt matrix; `specs/irq-sources.toml`, `specs/blocks/intc.toml`):
//! 62 sources, MAP, 31 lines, PRI and THRESH, and the `intc` block model that is its register
//! window at 0x600C2000.
//!
//! A peripheral drives a level on its source. One MAP register per source picks a CPU line 1 to
//! 31; sources sharing a line are ORed. Per line the fabric applies the edge latch, the enable
//! and the priority; the threshold is global. Taking the trap (`mstatus.MIE`) is the machine's
//! decision. MAP 0 means "not connected" and line 0 is reserved for exceptions. IDF parks a
//! source on line 6, which it never enables, to mask it without touching the others.
//!
//! Every mutation ends in [`IrqFabric::resync`], so the pending set depends only on the state
//! after the last change. Register writes and source changes both end the CPU block, so the CPU
//! sees a changed line at the next instruction boundary (`g3-behavior g3-irq-latency`).

use pemu_core::fidelity::Fidelity;
use pemu_core::irq_source::{IrqSource, SOURCE_COUNT};
use pemu_core::regstore::RegSpec;
use pemu_core::regstore::Size;
use pemu_core::reset::{ResetDomain, ResetKind};
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};

use crate::r#gen::regs_intc;
use crate::periph::{Block, Cx, Peripheral, RegRead, RegWrite, Wiring};
use crate::regs::{Table, Touches};

/// Number of CPU interrupt lines, including the reserved line 0.
pub const LINE_COUNT: usize = 32;

/// First MAP register; source `s` maps at `4 * s`.
pub const OFF_MAP: u32 = 0x000;
/// Last MAP register, source 61.
pub const OFF_MAP_LAST: u32 = 0x0F4;
/// Raw level of sources 0 to 31, RO.
pub const OFF_INTR_STATUS_0: u32 = 0x0F8;
/// Raw level of sources 32 to 61, RO.
pub const OFF_INTR_STATUS_1: u32 = 0x0FC;
/// Block clock gate, bit 0 CLK_EN, stored only.
pub const OFF_CLOCK_GATE: u32 = 0x100;
pub const OFF_CPU_INT_ENABLE: u32 = 0x104;
/// Per-line type: 0 level, 1 edge.
pub const OFF_CPU_INT_TYPE: u32 = 0x108;
/// Per-line edge-latch clear.
pub const OFF_CPU_INT_CLEAR: u32 = 0x10C;
/// Per-line deliverable status, RO.
pub const OFF_CPU_INT_EIP_STATUS: u32 = 0x110;
/// First per-line priority register; line `n` is at `0x114 + 4 * n`.
pub const OFF_CPU_INT_PRI: u32 = 0x114;
pub const OFF_CPU_INT_PRI_LAST: u32 = 0x190;
/// Global priority threshold, 4 bits.
pub const OFF_CPU_INT_THRESH: u32 = 0x194;
/// Block version register `INTERRUPT_CORE0_INTERRUPT_DATE`, 28 bits RW, reset 0x0200_7210. Not
/// fabric state, but it must read its constant as every other block's DATE register does.
pub const OFF_DATE: u32 = 0x7FC;

const DATE_MASK: u32 = 0x0FFF_FFFF;

/// Interrupt fabric: 62 sources routed by MAP onto 31 CPU lines with PRI and THRESH.
///
/// Evaluation is bit-parallel: `line_in[n] = (src_level & line_srcs[n]) != 0`; edge latch from
/// `line_in & !prev_in` on edge lines; `pending = ((line_in & !edge_type) | latch) & enable`;
/// scan the set bits of `pending` for PRI. `tests::reference` is the same algorithm line by line.
pub struct IrqFabric {
    /// Bit s = level of source s.
    src_level: u64,
    /// 0 = unrouted; 1..=31 = CPU line.
    map: [u8; SOURCE_COUNT],
    /// Derived on MAP writes.
    line_srcs: [u64; LINE_COUNT],
    enable: u32,
    edge_type: u32,
    latch: u32,
    prev_in: u32,
    pri: [u8; LINE_COUNT],
    thresh: u8,
    /// CPU_INT_CLEAR as last written: an RW store that evaluation reads.
    clear: u32,
    /// CLOCK_GATE as last written, stored only.
    clock_gate: u32,
    /// DATE as last written, reset to the generated table's value.
    date: u32,
    /// Bumped on any change; read by the poll tracker.
    epoch: u64,
    /// None = dirty.
    cached: Option<Option<u8>>,
}

impl IrqFabric {
    /// A fabric at its reset values: every source low and unrouted, every line disabled and
    /// level-triggered, PRI and THRESH 0, CLOCK_GATE 1, and a dirty cache.
    ///
    /// These are the generated `intc` table's reset column. MAP, TYPE, PRI and THRESH resets are
    /// UNVERIFIED on silicon, but the ROM `_init` writes ENABLE = 0 and THRESH = 1 before anything
    /// is delivered, so no boot path observes the difference.
    pub fn new() -> Self {
        IrqFabric {
            src_level: 0,
            map: [0; SOURCE_COUNT],
            line_srcs: [0; LINE_COUNT],
            enable: 0,
            edge_type: 0,
            latch: 0,
            prev_in: 0,
            pri: [0; LINE_COUNT],
            thresh: 0,
            clear: 0,
            clock_gate: 1,
            date: regs_intc::REGS[regs_intc::idx::INTERRUPT_CORE0_INTERRUPT_DATE].reset,
            epoch: 0,
            cached: None,
        }
    }
}

/// The saved state of an [`IrqFabric`]: every field except the derived `line_srcs` (rebuilt from
/// `map`) and the delivery cache (starts dirty).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct IrqFabricState {
    pub src_level: u64,
    pub map: Vec<u8>,
    pub enable: u32,
    pub edge_type: u32,
    pub latch: u32,
    pub prev_in: u32,
    pub pri: Vec<u8>,
    pub thresh: u8,
    pub clear: u32,
    pub clock_gate: u32,
    pub date: u32,
    /// Change counter the poll tracker reads.
    pub epoch: u64,
}

impl IrqFabric {
    /// The saved form of this fabric.
    pub fn state(&self) -> IrqFabricState {
        IrqFabricState {
            src_level: self.src_level,
            map: self.map.to_vec(),
            enable: self.enable,
            edge_type: self.edge_type,
            latch: self.latch,
            prev_in: self.prev_in,
            pri: self.pri.to_vec(),
            thresh: self.thresh,
            clear: self.clear,
            clock_gate: self.clock_gate,
            date: self.date,
            epoch: self.epoch,
        }
    }

    /// The fabric `state` describes, with `line_srcs` rebuilt and the cache dirty. `None` when a
    /// table has the wrong length or a MAP or PRI value is out of range (a corrupt snapshot).
    pub fn from_state(state: &IrqFabricState) -> Option<IrqFabric> {
        let map: [u8; SOURCE_COUNT] = state.map.as_slice().try_into().ok()?;
        let pri: [u8; LINE_COUNT] = state.pri.as_slice().try_into().ok()?;
        if map.iter().any(|m| *m > 0x1F) || pri.iter().any(|p| *p > 0xF) || state.thresh > 0xF {
            return None;
        }
        let mut fabric = IrqFabric {
            src_level: state.src_level,
            map,
            line_srcs: [0; LINE_COUNT],
            enable: state.enable,
            edge_type: state.edge_type,
            latch: state.latch,
            prev_in: state.prev_in,
            pri,
            thresh: state.thresh,
            clear: state.clear,
            clock_gate: state.clock_gate,
            date: state.date,
            epoch: state.epoch,
            cached: None,
        };
        fabric.rebuild_line_srcs();
        Some(fabric)
    }
}

impl Default for IrqFabric {
    fn default() -> Self {
        Self::new()
    }
}

impl IrqFabric {
    /// Sets the level of source `s`; dirty only on change.
    pub fn set_source(&mut self, s: IrqSource, level: bool) {
        let bit = 1u64 << s.0;
        if (self.src_level & bit != 0) == level {
            return;
        }
        self.src_level ^= bit;
        self.resync();
    }

    /// Level of source `s` as a peripheral last drove it (INTR_STATUS).
    pub fn source(&self, s: IrqSource) -> bool {
        self.src_level & (1u64 << s.0) != 0
    }

    /// CPU line source `s` is routed to, or 0 for "not connected".
    pub fn route(&self, s: IrqSource) -> u8 {
        self.map[usize::from(s.0)]
    }

    pub fn priority(&self, n: u8) -> u8 {
        self.pri[usize::from(n) & (LINE_COUNT - 1)]
    }

    pub fn threshold(&self) -> u8 {
        self.thresh
    }

    /// Bumped on any change; read by the poll tracker.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The whole 32-bit register at `off & !3`. An offset the generated table does not name
    /// reads 0.
    pub fn read(&self, off: u32) -> u32 {
        match off & !3 {
            word @ OFF_MAP..=OFF_MAP_LAST => u32::from(self.map[(word / 4) as usize]),
            OFF_INTR_STATUS_0 => self.src_level as u32,
            OFF_INTR_STATUS_1 => (self.src_level >> 32) as u32,
            OFF_CLOCK_GATE => self.clock_gate,
            OFF_CPU_INT_ENABLE => self.enable,
            OFF_CPU_INT_TYPE => self.edge_type,
            OFF_CPU_INT_CLEAR => self.clear,
            OFF_CPU_INT_EIP_STATUS => self.eip(),
            word @ OFF_CPU_INT_PRI..=OFF_CPU_INT_PRI_LAST => {
                u32::from(self.pri[((word - OFF_CPU_INT_PRI) / 4) as usize])
            }
            OFF_CPU_INT_THRESH => u32::from(self.thresh),
            OFF_DATE => self.date,
            _ => 0,
        }
    }

    /// Always stop = true. A MAP write re-routes immediately. `val` is the whole register word:
    /// the block model splices a narrow write in first, since every writable field is RW.
    pub fn write(&mut self, off: u32, val: u32) -> RegWrite {
        match off & !3 {
            word @ OFF_MAP..=OFF_MAP_LAST => {
                // 5-bit RW field; a value above 31 cannot be stored.
                self.map[(word / 4) as usize] = (val & 0x1F) as u8;
                self.rebuild_line_srcs();
            }
            OFF_CLOCK_GATE => self.clock_gate = val & 0x1,
            OFF_CPU_INT_ENABLE => self.enable = val,
            OFF_CPU_INT_TYPE => self.edge_type = val,
            OFF_CPU_INT_CLEAR => self.clear = val,
            word @ OFF_CPU_INT_PRI..=OFF_CPU_INT_PRI_LAST => {
                self.pri[((word - OFF_CPU_INT_PRI) / 4) as usize] = (val & 0xF) as u8;
            }
            OFF_CPU_INT_THRESH => self.thresh = (val & 0xF) as u8,
            OFF_DATE => self.date = val & DATE_MASK,
            // INTR_STATUS_0/1 and EIP_STATUS are RO.
            _ => {}
        }
        self.resync();
        RegWrite {
            stop: true,
            wiring: Wiring::None,
        }
    }

    /// Highest PRI, then lowest line; PRI != 0 && PRI >= THRESH.
    ///
    /// The test is `>=` because IDF raises THRESH to 4 inside a critical section and the PRI-4
    /// panic lines 24 to 27 must still fire there (`g3-behavior g3-irq-latency`). Equality on
    /// silicon is UNVERIFIED.
    pub fn deliverable(&mut self) -> Option<u8> {
        if let Some(cached) = self.cached {
            return cached;
        }
        let eip = self.eip();
        let mut best: Option<u8> = None;
        let mut best_pri = 0u8;
        for n in 1..LINE_COUNT {
            if eip & (1 << n) == 0 {
                continue;
            }
            if best.is_none() || self.pri[n] > best_pri {
                best_pri = self.pri[n];
                best = Some(n as u8);
            }
        }
        self.cached = Some(best);
        best
    }

    /// Any pending line, ignoring MIE: the WFI wake condition.
    pub fn wfi_wake(&self) -> bool {
        self.eip() != 0
    }

    /// CPU_INT_EIP_STATUS: `pending[n] && PRI[n] != 0 && PRI[n] >= THRESH`.
    pub fn eip(&self) -> u32 {
        let pending = self.pending();
        let mut eip = 0;
        for n in 1..LINE_COUNT {
            if pending & (1 << n) != 0 && self.pri[n] != 0 && self.pri[n] >= self.thresh {
                eip |= 1 << n;
            }
        }
        eip
    }

    /// Restores the fabric registers this reset clears. Source levels are not registers: each
    /// peripheral lowers its own source in its own `reset`, so clearing them here would hide a
    /// source that a block outside this reset's fan-out still drives.
    pub fn reset(&mut self, kind: ResetKind) {
        if !kind.clears(INTC_DOMAIN) {
            return;
        }
        let src_level = self.src_level;
        *self = IrqFabric::new();
        self.src_level = src_level;
        self.resync();
    }

    /// `pending[n]`, bit-parallel. Line 0 is reserved for exceptions and never pends. The latch
    /// is masked by `edge_type` here rather than dropped in [`IrqFabric::resync`], so a line
    /// switched to level and back keeps what it had latched.
    fn pending(&self) -> u32 {
        let line_in = self.line_in();
        ((line_in & !self.edge_type) | (self.latch & self.edge_type)) & self.enable & !1
    }

    /// `line_in[n] = OR of src_level[s] over all s with MAP[s] == n`.
    fn line_in(&self) -> u32 {
        let mut line_in = 0;
        for n in 1..LINE_COUNT {
            if self.src_level & self.line_srcs[n] != 0 {
                line_in |= 1 << n;
            }
        }
        line_in
    }

    fn rebuild_line_srcs(&mut self) {
        self.line_srcs = [0; LINE_COUNT];
        for (s, line) in self.map.iter().enumerate() {
            if *line != 0 {
                self.line_srcs[usize::from(*line)] |= 1u64 << s;
            }
        }
    }

    /// Advances the edge latch and marks the pending set dirty after any change.
    ///
    /// A rising edge sets the latch and then CLEAR clears it, in that order, so a CLEAR bit held
    /// at 1 also blocks new latching (UNVERIFIED). IDF never clears the bits it sets in CLEAR, so
    /// the other order would leave an edge line latched for good after one
    /// `rv_utils_intr_edge_ack`. Both steps are masked by `edge_type`.
    fn resync(&mut self) {
        let line_in = self.line_in();
        self.latch |= line_in & !self.prev_in & self.edge_type;
        self.latch &= !(self.clear & self.edge_type);
        self.prev_in = line_in;
        self.epoch = self.epoch.wrapping_add(1);
        self.cached = None;
    }
}

/// Reset domain of every `intc` register: Chip, System or Core reset (the block file's
/// `reset_domains` row). `tests::the_table_has_one_reset_domain` keeps it in step.
const INTC_DOMAIN: ResetDomain = ResetDomain(0x7);

/// The generated `intc` register table.
pub struct Regs;

impl Table<{ regs_intc::REG_COUNT }> for Regs {
    const BLOCK: &'static str = "intc";

    fn specs() -> &'static [RegSpec; regs_intc::REG_COUNT] {
        &regs_intc::REGS
    }
}

/// Model of the `intc` row of the `c3_devices!` table. The state is [`IrqFabric`], which the
/// machine owns and hands out through `Cx::irq`, so this model keeps only first-touch
/// bookkeeping. An unnamed window address reads 0 and ignores the write rather than storing it:
/// a second copy of a fabric register could disagree with the fabric.
#[derive(Default, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    touches: Touches<Regs, { regs_intc::REG_COUNT }>,
}

impl Peripheral for Model {
    const ID: PeriphId = crate::periph::id::INTC;
    const BASE: u32 = <crate::periph::block::Intc as Block>::BASE;
    const SIZE: u32 = <crate::periph::block::Intc as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        cx.irq.reset(kind);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        self.touches.read(off, size, Self::ID, cx.now, cx.ledger);
        let word = cx.irq.read(off);
        RegRead {
            val: narrow(word, off, size),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        self.touches.write(off, size, Self::ID, cx.now, cx.ledger);
        // Only the addressed bytes change: splice them into the stored word.
        let word = splice(cx.irq.read(off), off, size, val);
        cx.irq.write(off, word)
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.touches.class_at(off)
    }
}

/// The `size` bytes of the register word at `off & !3` that an access at `off` returns, shifted
/// down to bit 0. Bytes past the end of the register read 0.
fn narrow(word: u32, off: u32, size: Size) -> u32 {
    let shift = (off & 3) * 8;
    let bytes = byte_mask(size) << shift;
    (word & bytes) >> shift
}

/// The register word at `off & !3` after an access of `size` bytes at `off` wrote `val`.
fn splice(word: u32, off: u32, size: Size, val: u32) -> u32 {
    let shift = (off & 3) * 8;
    let bytes = byte_mask(size) << shift;
    (word & !bytes) | ((val << shift) & bytes)
}

const fn byte_mask(size: Size) -> u32 {
    match size {
        Size::B1 => 0xFF,
        Size::B2 => 0xFFFF,
        Size::B4 => u32::MAX,
    }
}

/// A whole owned `Cx` for the model tests. `Cx::dma` ties every borrow to one lifetime, so a test
/// builds the context inside [`TestPorts::with`] rather than holding it.
#[cfg(test)]
pub(crate) mod testing {
    use pemu_core::clock::TimingProfile;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::irq_source::IrqSource;
    use pemu_core::rng::{DetRng, RngStream};
    use pemu_core::sched::Scheduler;
    use pemu_core::time::VTime;
    use pemu_core::trace::TraceSink;

    use super::IrqFabric;
    use crate::dma::DmaView;
    use crate::periph::Cx;

    pub(crate) struct TestPorts {
        pub now: VTime,
        pub sched: Scheduler,
        pub irq: IrqFabric,
        pub rng: DetRng,
        pub ledger: FidelityLedger,
        pub trace: TraceSink,
        profile: TimingProfile,
    }

    impl TestPorts {
        pub(crate) fn new() -> TestPorts {
            TestPorts {
                now: VTime(0),
                sched: Scheduler::new(),
                irq: IrqFabric::new(),
                rng: DetRng::new(1),
                ledger: FidelityLedger::default(),
                trace: TraceSink::default(),
                profile: TimingProfile::default(),
            }
        }

        pub(crate) fn with<R>(&mut self, f: impl FnOnce(&mut Cx) -> R) -> R {
            let mut dma = DmaView::detached();
            let mut cx = Cx {
                now: self.now,
                sched: &mut self.sched,
                irq: &mut self.irq,
                dma: &mut dma,
                rng: self.rng.stream(RngStream::GUEST_ENTROPY),
                profile: &self.profile,
                ledger: &mut self.ledger,
                trace: &mut self.trace,
            };
            f(&mut cx)
        }

        pub(crate) fn source(&self, s: IrqSource) -> bool {
            self.irq.source(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::irq_source::{SOURCES, irq};
    use pemu_core::regstore::resets_in;
    use pemu_core::reset::{ResetCause, ResetScope};

    /// The evaluation algorithm written plainly, one step per line and no bit tricks, so a
    /// failure means the bit-parallel fabric disagrees with the specification.
    struct Reference {
        src_level: [bool; SOURCE_COUNT],
        map: [u8; SOURCE_COUNT],
        edge: [bool; LINE_COUNT],
        clear: [bool; LINE_COUNT],
        enable: [bool; LINE_COUNT],
        pri: [u8; LINE_COUNT],
        thresh: u8,
        latch: [bool; LINE_COUNT],
        prev: [bool; LINE_COUNT],
    }

    impl Reference {
        fn new() -> Reference {
            Reference {
                src_level: [false; SOURCE_COUNT],
                map: [0; SOURCE_COUNT],
                edge: [false; LINE_COUNT],
                clear: [false; LINE_COUNT],
                enable: [false; LINE_COUNT],
                pri: [0; LINE_COUNT],
                thresh: 0,
                latch: [false; LINE_COUNT],
                prev: [false; LINE_COUNT],
            }
        }

        /// One evaluation round; returns `eip`.
        fn step(&mut self) -> [bool; LINE_COUNT] {
            let mut eip = [false; LINE_COUNT];
            for (n, out) in eip.iter_mut().enumerate().skip(1) {
                let line_in =
                    (0..SOURCE_COUNT).any(|s| self.src_level[s] && usize::from(self.map[s]) == n);
                let raw = if self.edge[n] {
                    if line_in && !self.prev[n] {
                        self.latch[n] = true;
                    }
                    if self.clear[n] {
                        self.latch[n] = false;
                    }
                    self.latch[n]
                } else {
                    line_in
                };
                self.prev[n] = line_in;
                let pending = raw && self.enable[n];
                *out = pending && self.pri[n] != 0 && self.pri[n] >= self.thresh;
            }
            eip
        }

        /// Highest PRI, on a tie the lowest line.
        fn deliver(&self, eip: &[bool; LINE_COUNT]) -> Option<u8> {
            let mut best = None;
            for (n, on) in eip.iter().enumerate().skip(1) {
                if *on && best.is_none_or(|b: u8| self.pri[n] > self.pri[usize::from(b)]) {
                    best = Some(n as u8);
                }
            }
            best
        }
    }

    fn eip_mask(eip: &[bool; LINE_COUNT]) -> u32 {
        eip.iter()
            .enumerate()
            .skip(1)
            .fold(0, |m, (n, on)| if *on { m | 1 << n } else { m })
    }

    /// A deterministic stream: no host randomness reaches a core crate.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as u32
        }

        fn below(&mut self, n: u32) -> u32 {
            self.next() % n
        }
    }

    #[test]
    fn map_offsets_follow_the_generated_source_table() {
        // The MAP register of source s is at 0x600C2000 + 4 * s; specs/irq-sources.toml carries
        // the offsets.
        for info in SOURCES {
            assert_eq!(info.map_off, u32::from(info.source.0) * 4, "{}", info.name);
            assert!(info.map_off <= OFF_MAP_LAST, "{}", info.name);
        }
        assert_eq!(SOURCE_COUNT, 62);
        assert_eq!(OFF_MAP_LAST, 4 * (SOURCE_COUNT as u32 - 1));
    }

    #[test]
    fn cpu_side_offsets_follow_the_generated_register_table() {
        // Against the table specs/c3-registers.csv generates.
        let off = |idx: usize| u32::from(regs_intc::REGS[idx].off);
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_INTR_STATUS_0),
            OFF_INTR_STATUS_0
        );
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_INTR_STATUS_1),
            OFF_INTR_STATUS_1
        );
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_CLOCK_GATE),
            OFF_CLOCK_GATE
        );
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_CPU_INT_ENABLE),
            OFF_CPU_INT_ENABLE
        );
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_CPU_INT_TYPE),
            OFF_CPU_INT_TYPE
        );
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_CPU_INT_CLEAR),
            OFF_CPU_INT_CLEAR
        );
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_CPU_INT_EIP_STATUS),
            OFF_CPU_INT_EIP_STATUS
        );
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_CPU_INT_PRI_0),
            OFF_CPU_INT_PRI
        );
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_CPU_INT_PRI_31),
            OFF_CPU_INT_PRI_LAST
        );
        assert_eq!(
            off(regs_intc::idx::INTERRUPT_CORE0_CPU_INT_THRESH),
            OFF_CPU_INT_THRESH
        );
        assert_eq!(OFF_CPU_INT_PRI_LAST, OFF_CPU_INT_PRI + 4 * 31);
    }

    #[test]
    fn reset_values_match_the_generated_table() {
        // Every register reads its `RegSpec::reset` out of a fresh fabric, so the constructor and
        // the generated table cannot drift apart.
        let mut fabric = IrqFabric::new();
        for spec in regs_intc::REGS.iter() {
            let off = u32::from(spec.off);
            assert_eq!(fabric.read(off), spec.reset, "{} at {off:#05X}", spec.name);
        }
        assert_eq!(fabric.deliverable(), None);
        assert!(!fabric.wfi_wake());
        assert_eq!(fabric.epoch(), 0);
    }

    #[test]
    fn the_date_register_reads_its_constant() {
        // The window's last register is the block version, 0x0200_7210 (IDF
        // interrupt_core0_reg.h). The model has to serve it or the ROM would read 0.
        let spec = &regs_intc::REGS[regs_intc::idx::INTERRUPT_CORE0_INTERRUPT_DATE];
        assert_eq!(u32::from(spec.off), OFF_DATE);
        assert_eq!(spec.reset, 0x0200_7210);

        let mut fabric = IrqFabric::new();
        assert_eq!(fabric.read(OFF_DATE), 0x0200_7210);
        fabric.write(OFF_DATE, u32::MAX);
        assert_eq!(fabric.read(OFF_DATE), DATE_MASK);
        // And a reset restores it with the rest of the window.
        fabric.reset(ResetKind::of(ResetCause::POWERON).expect("documented cause"));
        assert_eq!(fabric.read(OFF_DATE), spec.reset);

        // A byte read of the window's last register returns the top byte of the constant.
        assert_eq!(narrow(fabric.read(OFF_DATE), OFF_DATE + 3, Size::B1), 0x02);
    }

    #[test]
    fn the_table_gives_every_register_one_reset_domain() {
        for spec in regs_intc::REGS.iter() {
            assert_eq!(spec.domain, INTC_DOMAIN, "{}", spec.name);
        }
        for scope in ResetScope::ALL {
            assert!(resets_in(INTC_DOMAIN, scope));
        }
    }

    #[test]
    fn a_reset_restores_the_registers_and_keeps_the_source_levels() {
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_MAP + 4 * u32::from(irq::SPI2.0), 19);
        fabric.write(OFF_CPU_INT_ENABLE, 1 << 19);
        fabric.write(OFF_CPU_INT_PRI + 4 * 19, 3);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        fabric.set_source(irq::SPI2, true);
        assert_eq!(fabric.deliverable(), Some(19));

        // A system reset restores the matrix; the source stays high, because the block driving
        // it lowers it in its own reset.
        let sys = ResetKind::of(ResetCause::RTC_SW_SYS).expect("documented cause");
        fabric.reset(sys);
        assert_eq!(fabric.read(OFF_CPU_INT_ENABLE), 0);
        assert_eq!(fabric.read(OFF_MAP + 4 * u32::from(irq::SPI2.0)), 0);
        assert_eq!(fabric.read(OFF_CLOCK_GATE), 1);
        assert!(fabric.source(irq::SPI2));
        assert_eq!(fabric.deliverable(), None);

        // A CPU-only reset reaches the hart and SENSITIVE, not the matrix.
        fabric.write(OFF_CPU_INT_ENABLE, 0x55);
        let cpu = ResetKind::of(ResetCause::RTC_SW_CPU).expect("documented cause");
        fabric.reset(cpu);
        assert_eq!(fabric.read(OFF_CPU_INT_ENABLE), 0x55);
    }

    #[test]
    fn a_map_write_reroutes_immediately() {
        // The SPI master masks one source by re-muxing it to the parked line 6
        // (INT_MUX_DISABLED_INTNO) and unmasks it by routing it back, expecting the pending level
        // to be delivered at once.
        const SPI2_MAP: u32 = OFF_MAP + 4 * 19;
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_CPU_INT_ENABLE, 1 << 19);
        fabric.write(OFF_CPU_INT_PRI + 4 * 19, 1);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        fabric.write(SPI2_MAP, 19);
        fabric.set_source(irq::SPI2, true);
        assert_eq!(fabric.deliverable(), Some(19));

        fabric.write(SPI2_MAP, 6);
        assert_eq!(fabric.deliverable(), None, "line 6 is never enabled");
        assert_eq!(
            fabric.read(OFF_INTR_STATUS_0) >> 19 & 1,
            1,
            "the source is still high"
        );

        fabric.write(SPI2_MAP, 19);
        assert_eq!(
            fabric.deliverable(),
            Some(19),
            "unmasking delivers the pending level"
        );
        assert_eq!(fabric.route(irq::SPI2), 19);
    }

    #[test]
    fn an_unrouted_source_reaches_no_line() {
        // MAP value 0 means "not connected"; line 0 is reserved for exceptions.
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_CPU_INT_ENABLE, u32::MAX);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        for n in 0..LINE_COUNT as u32 {
            fabric.write(OFF_CPU_INT_PRI + 4 * n, 1);
        }
        fabric.set_source(irq::FROM_CPU_INTR0, true);
        assert_eq!(fabric.deliverable(), None);
        assert_eq!(fabric.eip(), 0);
    }

    #[test]
    fn the_threshold_masks_by_priority_and_equality_delivers() {
        // IDF's critical sections set THRESH = 4 and the PRI-4 panic lines must still fire.
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_MAP + 4 * u32::from(irq::TG1_WDT.0), 24);
        fabric.write(OFF_MAP + 4 * u32::from(irq::SYSTIMER_TARGET0.0), 5);
        fabric.write(OFF_CPU_INT_ENABLE, (1 << 24) | (1 << 5));
        fabric.write(OFF_CPU_INT_PRI + 4 * 24, 4);
        fabric.write(OFF_CPU_INT_PRI + 4 * 5, 1);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        fabric.set_source(irq::SYSTIMER_TARGET0, true);
        assert_eq!(fabric.deliverable(), Some(5));

        fabric.write(OFF_CPU_INT_THRESH, 4);
        assert_eq!(fabric.deliverable(), None, "PRI 1 is masked by THRESH 4");
        fabric.set_source(irq::TG1_WDT, true);
        assert_eq!(
            fabric.deliverable(),
            Some(24),
            "PRI 4 equals THRESH 4 and delivers"
        );

        // A line with PRI 0 never delivers, whatever the threshold.
        fabric.write(OFF_CPU_INT_THRESH, 0);
        fabric.write(OFF_CPU_INT_PRI + 4 * 24, 0);
        fabric.write(OFF_CPU_INT_PRI + 4 * 5, 0);
        assert_eq!(fabric.deliverable(), None);
    }

    #[test]
    fn a_priority_tie_takes_the_lowest_line() {
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_MAP + 4 * u32::from(irq::SYSTIMER_TARGET0.0), 5);
        fabric.write(OFF_MAP + 4 * u32::from(irq::FROM_CPU_INTR0.0), 3);
        fabric.write(OFF_CPU_INT_ENABLE, (1 << 5) | (1 << 3));
        fabric.write(OFF_CPU_INT_PRI + 4 * 5, 1);
        fabric.write(OFF_CPU_INT_PRI + 4 * 3, 1);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        fabric.set_source(irq::SYSTIMER_TARGET0, true);
        fabric.set_source(irq::FROM_CPU_INTR0, true);
        assert_eq!(fabric.deliverable(), Some(3));
        fabric.write(OFF_CPU_INT_PRI + 4 * 5, 2);
        assert_eq!(
            fabric.deliverable(),
            Some(5),
            "the higher priority wins the tie-break"
        );
    }

    #[test]
    fn two_sources_on_one_line_are_ored() {
        // Several sources may share a line; the line is the OR of their levels.
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_MAP + 4 * u32::from(irq::CORE0_IRAM0_PMS.0), 26);
        fabric.write(OFF_MAP + 4 * u32::from(irq::CORE0_DRAM0_PMS.0), 26);
        fabric.write(OFF_CPU_INT_ENABLE, 1 << 26);
        fabric.write(OFF_CPU_INT_PRI + 4 * 26, 4);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        fabric.set_source(irq::CORE0_IRAM0_PMS, true);
        fabric.set_source(irq::CORE0_DRAM0_PMS, true);
        assert_eq!(fabric.deliverable(), Some(26));
        fabric.set_source(irq::CORE0_IRAM0_PMS, false);
        assert_eq!(
            fabric.deliverable(),
            Some(26),
            "the other source holds the line"
        );
        fabric.set_source(irq::CORE0_DRAM0_PMS, false);
        assert_eq!(fabric.deliverable(), None);
    }

    #[test]
    fn an_edge_line_latches_a_rising_source_until_clear() {
        // An edge line latches on a rising source edge and the latch clears while CPU_INT_CLEAR
        // holds the bit. A level line follows its source instead.
        const LINE: u32 = 7;
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_MAP + 4 * u32::from(irq::GPIO.0), LINE);
        fabric.write(OFF_CPU_INT_ENABLE, 1 << LINE);
        fabric.write(OFF_CPU_INT_PRI + 4 * LINE, 1);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        fabric.write(OFF_CPU_INT_TYPE, 1 << LINE);

        fabric.set_source(irq::GPIO, true);
        assert_eq!(fabric.deliverable(), Some(LINE as u8));
        fabric.set_source(irq::GPIO, false);
        assert_eq!(
            fabric.deliverable(),
            Some(LINE as u8),
            "the latch holds the line"
        );

        fabric.write(OFF_CPU_INT_CLEAR, 1 << LINE);
        assert_eq!(fabric.deliverable(), None);
        assert_eq!(
            fabric.read(OFF_CPU_INT_CLEAR),
            1 << LINE,
            "CLEAR is an RW store"
        );

        // A CLEAR bit left at 1 also blocks new latching.
        fabric.set_source(irq::GPIO, true);
        assert_eq!(fabric.deliverable(), None);
        fabric.write(OFF_CPU_INT_CLEAR, 0);
        fabric.set_source(irq::GPIO, false);
        fabric.set_source(irq::GPIO, true);
        assert_eq!(fabric.deliverable(), Some(LINE as u8));

        fabric.write(OFF_CPU_INT_TYPE, 0);
        fabric.set_source(irq::GPIO, false);
        assert_eq!(fabric.deliverable(), None);
    }

    #[test]
    fn a_saved_fabric_restores_its_latch_levels_and_routing() {
        // An edge latched before the save still delivers after restore, with no source high.
        const LINE: u32 = 9;
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_MAP + 4 * u32::from(irq::GPIO.0), LINE);
        fabric.write(OFF_MAP + 4 * u32::from(irq::SYSTIMER_TARGET0.0), 3);
        fabric.write(OFF_CPU_INT_ENABLE, (1 << LINE) | (1 << 3));
        fabric.write(OFF_CPU_INT_PRI + 4 * LINE, 2);
        fabric.write(OFF_CPU_INT_PRI + 4 * 3, 1);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        fabric.write(OFF_CPU_INT_TYPE, 1 << LINE);
        fabric.set_source(irq::GPIO, true);
        fabric.set_source(irq::GPIO, false);
        fabric.set_source(irq::SYSTIMER_TARGET0, true);

        let state = fabric.state();
        let mut restored = IrqFabric::from_state(&state).expect("a saved state restores");
        assert_eq!(restored.state(), state);
        for off in (0..0x800).step_by(4) {
            assert_eq!(restored.read(off), fabric.read(off), "register {off:#x}");
        }
        assert_eq!(
            restored.deliverable(),
            Some(LINE as u8),
            "the latch survived"
        );
        restored.set_source(irq::SYSTIMER_TARGET0, false);
        fabric.set_source(irq::SYSTIMER_TARGET0, false);
        assert_eq!(restored.state(), fabric.state(), "routing was rebuilt");

        let mut short = state.clone();
        short.map.pop();
        assert!(IrqFabric::from_state(&short).is_none());
        let mut wide = state;
        wide.pri[1] = 0x10;
        assert!(IrqFabric::from_state(&wide).is_none());
    }

    #[test]
    fn wfi_wake_follows_eip_and_ignores_the_enable_of_other_lines() {
        // WFI wakes on any eip[n].
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_MAP + 4 * u32::from(irq::SYSTIMER_TARGET0.0), 5);
        fabric.write(OFF_CPU_INT_PRI + 4 * 5, 1);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        fabric.set_source(irq::SYSTIMER_TARGET0, true);
        assert!(!fabric.wfi_wake(), "the line is not enabled yet");
        fabric.write(OFF_CPU_INT_ENABLE, 1 << 5);
        assert!(fabric.wfi_wake());
        assert_eq!(fabric.read(OFF_CPU_INT_EIP_STATUS), 1 << 5);
    }

    #[test]
    fn the_delivery_cache_follows_every_change() {
        let mut fabric = IrqFabric::new();
        fabric.write(OFF_MAP + 4 * u32::from(irq::UART0.0), 5);
        fabric.write(OFF_CPU_INT_ENABLE, 1 << 5);
        fabric.write(OFF_CPU_INT_PRI + 4 * 5, 1);
        fabric.write(OFF_CPU_INT_THRESH, 1);
        let epoch = fabric.epoch();
        assert_eq!(fabric.deliverable(), None);
        fabric.set_source(irq::UART0, true);
        assert!(fabric.epoch() > epoch, "a source change bumps the epoch");
        assert_eq!(fabric.deliverable(), Some(5), "the cache was invalidated");
        let epoch = fabric.epoch();
        fabric.set_source(irq::UART0, true);
        assert_eq!(fabric.epoch(), epoch, "an unchanged level is not a change");
    }

    #[test]
    fn narrow_accesses_touch_only_the_addressed_bytes() {
        assert_eq!(narrow(0x1122_3344, 0, Size::B4), 0x1122_3344);
        assert_eq!(narrow(0x1122_3344, 1, Size::B1), 0x33);
        assert_eq!(narrow(0x1122_3344, 2, Size::B2), 0x1122);
        assert_eq!(splice(0x1122_3344, 0, Size::B1, 0xFF), 0x1122_33FF);
        assert_eq!(splice(0x1122_3344, 2, Size::B2, 0xBEEF), 0xBEEF_3344);
        assert_eq!(splice(0x1122_3344, 0, Size::B4, 0), 0);

        let mut fabric = IrqFabric::new();
        let map = OFF_MAP + 4 * u32::from(irq::I2C_EXT0.0);
        fabric.write(map, splice(fabric.read(map), map, Size::B1, 0xE5));
        assert_eq!(
            fabric.route(irq::I2C_EXT0),
            5,
            "only the low 5 bits are stored"
        );
    }

    #[test]
    fn the_fabric_follows_the_reference_model() {
        // The bit-parallel fabric against the reference, over a random walk of every guest
        // operation; the reference steps once per mutation.
        let mut rng = Lcg(0x5EED_1701);
        for round in 0..200u32 {
            let mut fabric = IrqFabric::new();
            let mut model = Reference::new();
            for op in 0..200u32 {
                match rng.below(7) {
                    0 => {
                        let s = rng.below(SOURCE_COUNT as u32);
                        let level = rng.below(2) == 1;
                        fabric.set_source(IrqSource(s as u8), level);
                        model.src_level[s as usize] = level;
                    }
                    1 => {
                        let s = rng.below(SOURCE_COUNT as u32);
                        let line = rng.below(8);
                        fabric.write(OFF_MAP + 4 * s, line);
                        model.map[s as usize] = line as u8;
                    }
                    2 => {
                        let v = rng.next();
                        fabric.write(OFF_CPU_INT_ENABLE, v);
                        for n in 0..LINE_COUNT {
                            model.enable[n] = v >> n & 1 == 1;
                        }
                    }
                    3 => {
                        let v = rng.next();
                        fabric.write(OFF_CPU_INT_TYPE, v);
                        for n in 0..LINE_COUNT {
                            model.edge[n] = v >> n & 1 == 1;
                        }
                    }
                    4 => {
                        let v = rng.next();
                        fabric.write(OFF_CPU_INT_CLEAR, v);
                        for n in 0..LINE_COUNT {
                            model.clear[n] = v >> n & 1 == 1;
                        }
                    }
                    5 => {
                        let n = rng.below(LINE_COUNT as u32);
                        let p = rng.below(5);
                        fabric.write(OFF_CPU_INT_PRI + 4 * n, p);
                        model.pri[n as usize] = p as u8;
                    }
                    _ => {
                        let t = rng.below(5);
                        fabric.write(OFF_CPU_INT_THRESH, t);
                        model.thresh = t as u8;
                    }
                }
                let expected = model.step();
                assert_eq!(
                    fabric.eip(),
                    eip_mask(&expected),
                    "round {round} op {op}: eip"
                );
                assert_eq!(
                    fabric.deliverable(),
                    model.deliver(&expected),
                    "round {round} op {op}: delivered line"
                );
                assert_eq!(
                    fabric.wfi_wake(),
                    eip_mask(&expected) != 0,
                    "round {round} op {op}: wfi wake"
                );
            }
        }
    }
}
