//! What a modeled block sits on: its register bank (generated from `specs/c3-registers.csv`)
//! and the slice of `Cx` it reaches.
//!
//! [`Regs`] is the bank: the generated table, the [`RegStore`] that gives every bit its access
//! semantics, the first-touch bitset, and RW storage (reset 0, reported on first access) for the
//! window addresses no row names. [`Touches`] is the bookkeeping half alone. [`Ports`] is the
//! context.
//!
//! [`RegStore`] borrows a `&'static [RegSpec; N]` that no `Deserialize` can recover, so the
//! marker type [`Table`] supplies it at the type level and a bank stays `Default` and
//! snapshot-able without a hand-written serde impl per block.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;

use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, LedgerSubject, TouchAccess};
use pemu_core::regstore::{Delta, RegSpec, RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventHandle, EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::de::Error as _;
use pemu_core::serde::{Deserialize, Deserializer, Serialize, Serializer};
use pemu_core::time::VTime;

use crate::intc::IrqFabric;
use crate::periph::Cx;

/// The part of `Cx` a block model reaches. Models take this so their tests need no `DmaView`,
/// `TraceSink` or `TimingProfile`.
pub struct Ports<'a> {
    /// Exact virtual time of the access.
    pub now: VTime,
    /// Event scheduler; a counter schedules its alarm here instead of ticking.
    pub sched: &'a mut Scheduler,
    pub irq: &'a mut IrqFabric,
    pub ledger: &'a mut FidelityLedger,
}

impl<'a> Ports<'a> {
    pub fn of(cx: &'a mut Cx<'_>) -> Ports<'a> {
        Ports {
            now: cx.now,
            sched: cx.sched,
            irq: cx.irq,
            ledger: cx.ledger,
        }
    }

    /// Schedules `tag` of block `periph` at `at`, cancelling `handle` first, and returns the new
    /// handle. `at` before `now` is clamped to `now`.
    pub fn rearm(
        &mut self,
        handle: &mut Option<EventHandle>,
        periph: PeriphId,
        tag: u16,
        at: VTime,
    ) {
        self.disarm(handle);
        *handle = Some(self.sched.schedule(
            self.now,
            at,
            EventKey {
                owner: Owner::Periph(periph),
                tag,
            },
        ));
    }

    pub fn disarm(&mut self, handle: &mut Option<EventHandle>) {
        if let Some(h) = handle.take() {
            self.sched.cancel(h);
        }
    }
}

/// The generated register table of one block, as a type-level fact. `N` is the block's
/// `REG_COUNT`.
pub trait Table<const N: usize> {
    const BLOCK: &'static str;

    /// A function rather than an associated constant, because a constant may not refer to a
    /// `static` (E0013) and `crate::gen` emits `REGS` as one.
    fn specs() -> &'static [RegSpec; N];
}

/// Register bank of block `T`. An access of 1, 2 or 4 bytes reads or changes only the addressed
/// bytes; nothing widens a byte write into a read-modify-write.
pub struct Regs<T: Table<N>, const N: usize> {
    store: RegStore<N>,
    /// Bit per register: already reported to the ledger. Snapshotted, so a restored machine does
    /// not report a register twice.
    touched: Vec<u64>,
    /// Word-aligned offsets no table row names: plain RW storage, reset 0. A `BTreeMap` for
    /// deterministic iteration order.
    extra: BTreeMap<u32, u32>,
    block: PhantomData<fn() -> T>,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct RegsState {
    vals: Vec<u32>,
    touched: Vec<u64>,
    extra: BTreeMap<u32, u32>,
}

impl<T: Table<N>, const N: usize> Default for Regs<T, N> {
    fn default() -> Self {
        Regs {
            store: RegStore::new(T::specs()),
            touched: vec![0; N.div_ceil(64)],
            extra: BTreeMap::new(),
            block: PhantomData,
        }
    }
}

impl<T: Table<N>, const N: usize> Regs<T, N> {
    fn to_state(&self) -> RegsState {
        RegsState {
            vals: (0..N).map(|i| self.store.get(i)).collect(),
            touched: self.touched.clone(),
            extra: self.extra.clone(),
        }
    }

    /// The bank a [`RegsState`] describes, or why it does not fit. The table is not in the
    /// snapshot, so the lengths are all a decoder can check; a section that does not fit is
    /// refused, never restored in part.
    fn from_state(state: RegsState) -> Result<Self, String> {
        if state.vals.len() != N || state.touched.len() != N.div_ceil(64) {
            return Err(format!(
                "{}: snapshot has {} of {N} registers and {} of {} touch words",
                T::BLOCK,
                state.vals.len(),
                state.touched.len(),
                N.div_ceil(64),
            ));
        }
        let mut regs = Regs::<T, N>::default();
        for (i, v) in state.vals.iter().enumerate() {
            regs.store.set(i, *v);
        }
        regs.touched = state.touched;
        regs.extra = state.extra;
        Ok(regs)
    }
}

impl<T: Table<N>, const N: usize> Serialize for Regs<T, N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_state().serialize(s)
    }
}

impl<'de, T: Table<N>, const N: usize> Deserialize<'de> for Regs<T, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Regs::from_state(RegsState::deserialize(d)?).map_err(D::Error::custom)
    }
}

impl<T: Table<N>, const N: usize> Regs<T, N> {
    /// Index of the register at word offset `off`, by binary search over the ascending table.
    pub fn index_of(&self, off: u32) -> Option<usize> {
        let off = u16::try_from(off).ok()?;
        T::specs().binary_search_by_key(&off, |s| s.off).ok()
    }

    pub fn spec(&self, idx: usize) -> &'static RegSpec {
        &T::specs()[idx]
    }

    /// Stored value of register `idx`, with no read side effect.
    pub fn get(&self, idx: usize) -> u32 {
        self.store.get(idx)
    }

    /// Hardware-side update of register `idx`, bypassing access semantics: RO status, W1C raw
    /// bits and counters.
    pub fn set(&mut self, idx: usize, v: u32) {
        self.store.set(idx, v);
    }

    pub fn field(&self, idx: usize, shift: u8, width: u8) -> u32 {
        (self.store.get(idx) >> shift) & mask(width)
    }

    pub fn set_field(&mut self, idx: usize, shift: u8, width: u8, v: u32) {
        let m = mask(width) << shift;
        self.store
            .set(idx, (self.store.get(idx) & !m) | ((v << shift) & m));
    }

    /// Completes the effect of self-clearing bits of register `idx`.
    pub fn clear_sc(&mut self, idx: usize, mask: u32) {
        self.store.clear_sc(idx, mask);
    }

    /// Restores every register this reset clears. A `CpuAndPms` reset leaves a digital block
    /// alone; SENSITIVE uses [`Regs::reset_all`] because its lock bits read 0 after every reset.
    pub fn reset(&mut self, kind: ResetKind) {
        let mut cleared = false;
        for idx in 0..N {
            if kind.clears(T::specs()[idx].domain) {
                self.store.set(idx, T::specs()[idx].reset);
                cleared = true;
            }
        }
        if cleared {
            self.extra.clear();
        }
    }

    /// Restores every register whatever the reset kind.
    pub fn reset_all(&mut self) {
        for idx in 0..N {
            self.store.set(idx, T::specs()[idx].reset);
        }
        self.extra.clear();
    }

    /// Fidelity class of the register at `off`, `U` for an address the table does not name.
    pub fn class_at(&self, off: u32) -> Fidelity {
        self.index_of(off & !3)
            .map_or(Fidelity::U, |idx| T::specs()[idx].class)
    }

    pub fn is_touched(&self, off: u32) -> bool {
        match self.index_of(off & !3) {
            Some(idx) => self.touched[idx / 64] & (1 << (idx % 64)) != 0,
            None => false,
        }
    }

    /// Reads `size` bytes at `off`, applying read side effects and reporting first touches.
    /// Bytes outside the window read 0 and are not reported.
    pub fn read(
        &mut self,
        off: u32,
        size: Size,
        periph: PeriphId,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> u32 {
        let origin = TouchAt { periph, size, now };
        let mut val = 0;
        for (byte, chunk_off, chunk) in chunks(off, size) {
            let part = match self.index_of(chunk_off & !3) {
                Some(idx) => {
                    self.touch(idx, TouchAccess::Read, origin, ledger);
                    self.store.read(idx, (chunk_off & 3) as u8, chunk)
                }
                None => self.read_extra(chunk_off, chunk, origin, ledger),
            };
            val |= part << (byte * 8);
        }
        val
    }

    /// Writes the low `size` bytes of `val` at `off` and reports first touches. The [`Delta`] is
    /// `Some` only for an access inside one register, which is every access a driver makes; a
    /// crossing or unnamed access is still stored.
    pub fn write(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        periph: PeriphId,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> Option<Delta> {
        let origin = TouchAt { periph, size, now };
        let mut delta = None;
        for (byte, chunk_off, chunk) in chunks(off, size) {
            let part = val >> (byte * 8);
            match self.index_of(chunk_off & !3) {
                Some(idx) => {
                    self.touch(idx, TouchAccess::Write, origin, ledger);
                    let d = self.store.write(idx, (chunk_off & 3) as u8, chunk, part);
                    if byte == 0 && chunk as u32 == size as u32 {
                        delta = Some(d);
                    }
                }
                None => self.write_extra(chunk_off, chunk, part, origin, ledger),
            }
        }
        delta
    }

    fn read_extra(
        &mut self,
        off: u32,
        width: Size,
        origin: TouchAt,
        ledger: &mut FidelityLedger,
    ) -> u32 {
        self.note_extra(off & !3, TouchAccess::Read, origin, ledger);
        let word = self.extra.get(&(off & !3)).copied().unwrap_or(0);
        (word >> ((off & 3) * 8)) & byte_mask(width)
    }

    fn write_extra(
        &mut self,
        off: u32,
        width: Size,
        val: u32,
        origin: TouchAt,
        ledger: &mut FidelityLedger,
    ) {
        self.note_extra(off & !3, TouchAccess::Write, origin, ledger);
        let shift = (off & 3) * 8;
        let m = byte_mask(width) << shift;
        let word = self.extra.entry(off & !3).or_insert(0);
        *word = (*word & !m) | ((val << shift) & m);
    }

    /// Reports the first touch of an unnamed word, once. Presence in `extra` is the "already
    /// reported" bit, because `FidelityLedger::first_touch` scans the whole ledger and a hot
    /// unnamed address would pay that per access. After a reset clears `extra` the word may be
    /// offered again; the ledger keeps the earliest entry.
    fn note_extra(
        &mut self,
        word: u32,
        access: TouchAccess,
        origin: TouchAt,
        ledger: &mut FidelityLedger,
    ) {
        if self.extra.contains_key(&word) {
            return;
        }
        self.extra.insert(word, 0);
        note(ledger, word, access, origin);
    }

    /// Reports the first touch of register `idx`, once per machine.
    fn touch(
        &mut self,
        idx: usize,
        access: TouchAccess,
        origin: TouchAt,
        ledger: &mut FidelityLedger,
    ) {
        let bit = 1u64 << (idx % 64);
        if self.touched[idx / 64] & bit != 0 {
            return;
        }
        self.touched[idx / 64] |= bit;
        note(ledger, u32::from(T::specs()[idx].off), access, origin);
    }
}

/// First-touch bookkeeping and class lookup for a block whose state is not a [`Regs`] bank:
/// `intc`, whose state is the [`crate::intc::IrqFabric`] the machine owns.
#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde", bound = "")]
pub struct Touches<T: Table<N>, const N: usize> {
    touched: Vec<u64>,
    /// Unnamed word offsets already reported, so a hot unnamed address does not scan the ledger.
    unnamed: BTreeSet<u32>,
    #[serde(skip)]
    block: PhantomData<fn() -> T>,
}

impl<T: Table<N>, const N: usize> Default for Touches<T, N> {
    fn default() -> Self {
        Touches {
            touched: vec![0; N.div_ceil(64)],
            unnamed: BTreeSet::new(),
            block: PhantomData,
        }
    }
}

impl<T: Table<N>, const N: usize> Touches<T, N> {
    pub fn read(
        &mut self,
        off: u32,
        size: Size,
        periph: PeriphId,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        self.report(off, size, TouchAccess::Read, periph, now, ledger);
    }

    pub fn write(
        &mut self,
        off: u32,
        size: Size,
        periph: PeriphId,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        self.report(off, size, TouchAccess::Write, periph, now, ledger);
    }

    /// Fidelity class of the register at `off`, `U` for an address the table does not name.
    pub fn class_at(&self, off: u32) -> Fidelity {
        index_of::<T, N>(off & !3).map_or(Fidelity::U, |idx| T::specs()[idx].class)
    }

    pub fn is_touched(&self, off: u32) -> bool {
        match index_of::<T, N>(off & !3) {
            Some(idx) => self.touched[idx / 64] & (1 << (idx % 64)) != 0,
            None => false,
        }
    }

    fn report(
        &mut self,
        off: u32,
        size: Size,
        access: TouchAccess,
        periph: PeriphId,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        let origin = TouchAt { periph, size, now };
        for (_, chunk_off, _) in chunks(off, size) {
            let word = chunk_off & !3;
            match index_of::<T, N>(word) {
                Some(idx) => {
                    let bit = 1u64 << (idx % 64);
                    if self.touched[idx / 64] & bit != 0 {
                        continue;
                    }
                    self.touched[idx / 64] |= bit;
                    note(ledger, word, access, origin);
                }
                None => {
                    if self.unnamed.insert(word) {
                        note(ledger, word, access, origin);
                    }
                }
            }
        }
    }
}

fn index_of<T: Table<N>, const N: usize>(off: u32) -> Option<usize> {
    let off = u16::try_from(off).ok()?;
    T::specs().binary_search_by_key(&off, |s| s.off).ok()
}

/// What a first-touch entry carries besides the offset and the access kind.
#[derive(Copy, Clone)]
pub struct TouchAt {
    pub periph: PeriphId,
    pub size: Size,
    pub now: VTime,
}

// Helpers for a block that keeps a bare `RegStore` and its own first-touch bitmap.

/// Table index of the register holding block offset `off` in `specs`, and the byte inside it.
/// Binary search: the generated tables are ascending (their `table_is_consistent` test).
pub fn reg_at(specs: &[RegSpec], off: u32) -> Option<(usize, u8)> {
    let aligned = u16::try_from(off & !3).ok()?;
    let i = specs.binary_search_by_key(&aligned, |r| r.off).ok()?;
    Some((i, (off & 3) as u8))
}

/// Reports the first touch of register `i` of `specs` once; the `touched` bitmap is snapshotted
/// so a restored machine does not report it again.
pub fn touch(
    touched: &mut [u64],
    specs: &[RegSpec],
    i: usize,
    access: TouchAccess,
    at: TouchAt,
    ledger: &mut FidelityLedger,
) {
    if first(touched, i) {
        note(ledger, u32::from(specs[i].off), access, at);
    }
}

/// [`touch`] that also notes the class the register's spec row claims.
pub fn touch_classed(
    touched: &mut [u64],
    specs: &[RegSpec],
    i: usize,
    access: TouchAccess,
    at: TouchAt,
    ledger: &mut FidelityLedger,
) {
    if first(touched, i) {
        let off = u32::from(specs[i].off);
        ledger.note(
            LedgerSubject::Register {
                periph: at.periph,
                off,
            },
            specs[i].class,
        );
        note(ledger, off, access, at);
    }
}

/// An offset inside the block window with no register: it reads 0, the write is dropped, and the
/// ledger names the block and the word once.
pub fn hole(off: u32, access: TouchAccess, at: TouchAt, ledger: &mut FidelityLedger) {
    note(ledger, off & !3, access, at);
}

/// [`hole`] with an explicit class `U` note. Without it `FidelityLedger::class_of` would fall
/// back to the block's note and a hole would read back as a modeled register.
pub fn hole_classed(off: u32, access: TouchAccess, at: TouchAt, ledger: &mut FidelityLedger) {
    let off = off & !3;
    ledger.note(
        LedgerSubject::Register {
            periph: at.periph,
            off,
        },
        Fidelity::U,
    );
    note(ledger, off, access, at);
}

fn first(touched: &mut [u64], i: usize) -> bool {
    let bit = 1u64 << (i % 64);
    let word = &mut touched[i / 64];
    let fresh = *word & bit == 0;
    *word |= bit;
    fresh
}

/// The stored values of `regs` in table order: what a snapshot carries of a bare `RegStore`.
pub fn store_values<const N: usize>(regs: &RegStore<N>) -> Vec<u32> {
    (0..N).map(|i| regs.get(i)).collect()
}

/// The inverse of [`store_values`]. A register `vals` does not reach keeps its reset value and
/// values past the table are dropped.
pub fn store_from<const N: usize>(specs: &'static [RegSpec; N], vals: &[u32]) -> RegStore<N> {
    let mut regs = RegStore::new(specs);
    for (i, v) in vals.iter().enumerate().take(N) {
        regs.set(i, *v);
    }
    regs
}

/// Defines `mod regs_serde` for `#[serde(with = "regs_serde")]` on a `RegStore<REG_COUNT>`
/// field, over the calling module's generated `REGS` and `REG_COUNT`.
macro_rules! store_serde {
    () => {
        /// Serde glue for the `regs` field.
        mod regs_serde {
            use pemu_core::regstore::RegStore;
            use pemu_core::serde::{Deserialize, Deserializer, Serialize, Serializer};

            use super::{REG_COUNT, REGS};

            pub fn serialize<S: Serializer>(
                regs: &RegStore<REG_COUNT>,
                s: S,
            ) -> Result<S::Ok, S::Error> {
                $crate::regs::store_values(regs).serialize(s)
            }

            pub fn deserialize<'de, D: Deserializer<'de>>(
                d: D,
            ) -> Result<RegStore<REG_COUNT>, D::Error> {
                Ok($crate::regs::store_from(
                    &REGS,
                    &Vec::<u32>::deserialize(d)?,
                ))
            }
        }
    };
}
pub(crate) use store_serde;

/// One ledger entry. No register a bank models is allowlisted; the radio pages are.
fn note(ledger: &mut FidelityLedger, off: u32, access: TouchAccess, origin: TouchAt) {
    ledger.first_touch(FirstTouch {
        periph: origin.periph,
        off,
        access,
        size: origin.size as u8,
        now: origin.now,
        allowlisted: false,
    });
}

/// Splits an access at register boundaries: `(byte index in the access, offset, width)` per
/// register, widest chunk first.
fn chunks(off: u32, size: Size) -> impl Iterator<Item = (u32, u32, Size)> {
    let total = size as u32;
    let mut byte = 0;
    core::iter::from_fn(move || {
        if byte >= total {
            return None;
        }
        let at = off.wrapping_add(byte);
        let room = 4 - (at & 3);
        let width = match (total - byte).min(room) {
            n if n >= 4 => Size::B4,
            n if n >= 2 => Size::B2,
            _ => Size::B1,
        };
        let out = (byte, at, width);
        byte += width as u32;
        Some(out)
    })
}

/// The bits of the word at `off & !3` that an access of `size` bytes at `off` writes. For a rule
/// stated over the value written; [`Delta::after`] also carries the bytes not addressed.
pub const fn write_bits(off: u32, size: Size, val: u32) -> u32 {
    let shift = (off & 3) * 8;
    (val << shift) & (byte_mask(size) << shift)
}

const fn byte_mask(size: Size) -> u32 {
    match size {
        Size::B1 => 0xFF,
        Size::B2 => 0xFFFF,
        Size::B4 => u32::MAX,
    }
}

/// Mask of `width` bits at bit 0; width 32 shifts in two steps, because `1u32 << 32` overflows.
const fn mask(width: u8) -> u32 {
    if width >= 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::regstore::{FieldAccess, FieldSpec};
    use pemu_core::reset::{ResetCause, ResetScope};

    use crate::r#gen::{DOMAIN_CHIP_SYSTEM_CORE, field, reg};

    /// Two adjacent registers and then a gap: a named word, an unnamed word after it, and an
    /// access that crosses from one into the other.
    static SPECS: [RegSpec; 2] = [
        reg(
            "TEST_A",
            0x000,
            0x1234_5678,
            &FIELDS_A,
            DOMAIN_CHIP_SYSTEM_CORE,
            false,
            Fidelity::B,
            "test fixture of the module documentation",
        ),
        reg(
            "TEST_B",
            0x004,
            0,
            &FIELDS_B,
            DOMAIN_CHIP_SYSTEM_CORE,
            false,
            Fidelity::C,
            "test fixture of the module documentation",
        ),
    ];
    static FIELDS_A: [FieldSpec; 1] = [field("A", 0, 32, FieldAccess::Rw, 0x1234_5678)];
    static FIELDS_B: [FieldSpec; 1] = [field("B", 0, 32, FieldAccess::Rw, 0)];

    struct TestBlock;

    impl Table<2> for TestBlock {
        const BLOCK: &'static str = "test";

        fn specs() -> &'static [RegSpec; 2] {
            &SPECS
        }
    }

    type Bank = Regs<TestBlock, 2>;

    const ID: PeriphId = crate::periph::id::INTC;

    fn ledger() -> FidelityLedger {
        FidelityLedger::default()
    }

    /// A short list keeps the reset value of what it does not reach; extra values are dropped.
    #[test]
    fn a_bare_store_round_trips_through_its_values() {
        use crate::r#gen::regs_efuse::{REG_COUNT, REGS};
        let mut store = RegStore::new(&REGS);
        store.set(3, 0xDEAD_BEEF);
        let vals = store_values(&store);
        assert_eq!(vals.len(), REG_COUNT);
        assert_eq!(store_values(&store_from(&REGS, &vals)), vals);
        let short = store_from(&REGS, &vals[..2]);
        assert_eq!(short.get(3), REGS[3].reset);
        let mut long = vals.clone();
        long.push(7);
        assert_eq!(store_values(&store_from(&REGS, &long)), vals);
    }

    #[test]
    fn a_bare_store_reports_each_register_once() {
        let mut ledger = ledger();
        let mut touched = [0u64; 2];
        let at = TouchAt {
            periph: ID,
            size: Size::B4,
            now: VTime(5),
        };
        let (i, byte) = reg_at(&SPECS, 0x005).expect("0x004 is named");
        assert_eq!((i, byte), (1, 1));
        assert_eq!(reg_at(&SPECS, 0x008), None);
        touch(&mut touched, &SPECS, i, TouchAccess::Read, at, &mut ledger);
        touch(&mut touched, &SPECS, i, TouchAccess::Write, at, &mut ledger);
        assert_eq!(ledger.cursor(), 1);
        hole(0x00B, TouchAccess::Read, at, &mut ledger);
        assert_eq!(ledger.cursor(), 2);
    }

    #[test]
    fn chunks_split_an_access_at_register_boundaries() {
        // Width as a byte count, because `Size` is not `Debug`.
        let split = |off: u32, size: Size| -> Vec<(u32, u32, u32)> {
            chunks(off, size)
                .map(|(byte, at, width)| (byte, at, width as u32))
                .collect()
        };
        assert_eq!(split(0, Size::B1), vec![(0, 0, 1)]);
        assert_eq!(split(3, Size::B1), vec![(0, 3, 1)]);
        assert_eq!(split(0, Size::B2), vec![(0, 0, 2)]);
        assert_eq!(split(2, Size::B2), vec![(0, 2, 2)]);
        assert_eq!(split(0, Size::B4), vec![(0, 0, 4)]);
        assert_eq!(split(3, Size::B2), vec![(0, 3, 1), (1, 4, 1)]);
        assert_eq!(split(2, Size::B4), vec![(0, 2, 2), (2, 4, 2)]);
        // A chunk need not be aligned inside its register: `RegStore` addresses a register by
        // its first byte and a width.
        assert_eq!(split(1, Size::B4), vec![(0, 1, 2), (2, 3, 1), (3, 4, 1)]);
        assert_eq!(split(3, Size::B4), vec![(0, 3, 1), (1, 4, 2), (3, 6, 1)]);

        for size in [Size::B1, Size::B2, Size::B4] {
            for off in 0..4 {
                let mut covered = 0;
                for (byte, at, width) in chunks(off, size) {
                    assert_eq!(byte, covered, "{off} leaves a hole");
                    assert_eq!(at, off + byte);
                    assert_eq!(
                        at & !3,
                        (at + width as u32 - 1) & !3,
                        "a chunk stays inside one register"
                    );
                    covered += width as u32;
                }
                assert_eq!(covered, size as u32, "{off} is not covered");
            }
        }
    }

    #[test]
    fn a_crossing_access_reads_and_writes_both_registers() {
        let mut bank = Bank::default();
        let mut l = ledger();
        bank.write(0x002, Size::B4, 0xAABB_CCDD, ID, VTime(0), &mut l);
        assert_eq!(bank.get(0), 0xCCDD_5678);
        assert_eq!(bank.get(1), 0x0000_AABB);
        assert_eq!(
            bank.read(0x002, Size::B4, ID, VTime(0), &mut l),
            0xAABB_CCDD
        );
    }

    #[test]
    fn a_crossing_or_unnamed_write_reports_no_delta() {
        let mut bank = Bank::default();
        let mut l = ledger();
        assert!(
            bank.write(0x000, Size::B4, 1, ID, VTime(0), &mut l)
                .is_some()
        );
        assert!(
            bank.write(0x001, Size::B1, 1, ID, VTime(0), &mut l)
                .is_some()
        );
        assert!(
            bank.write(0x002, Size::B4, 1, ID, VTime(0), &mut l)
                .is_none()
        );
        assert!(
            bank.write(0x008, Size::B4, 1, ID, VTime(0), &mut l)
                .is_none()
        );
    }

    #[test]
    fn an_unnamed_address_stores_reads_back_and_is_reported_once() {
        // An unnamed word inside a known block: RW storage, reset 0, reported once.
        let mut bank = Bank::default();
        let mut l = ledger();
        assert_eq!(bank.read(0x008, Size::B4, ID, VTime(0), &mut l), 0);
        assert_eq!(bank.class_at(0x008), Fidelity::U, "no row, no class");
        assert!(!bank.is_touched(0x008), "the bit belongs to a named row");

        bank.write(0x008, Size::B4, 0xDEAD_BEEF, ID, VTime(1), &mut l);
        assert_eq!(
            bank.read(0x008, Size::B4, ID, VTime(2), &mut l),
            0xDEAD_BEEF
        );
        assert_eq!(bank.read(0x009, Size::B1, ID, VTime(3), &mut l), 0xBE);
        bank.write(0x00A, Size::B1, 0x11, ID, VTime(4), &mut l);
        assert_eq!(
            bank.read(0x008, Size::B4, ID, VTime(5), &mut l),
            0xDE11_BEEF
        );

        let entries: Vec<_> = l
            .first_touches()
            .iter()
            .filter(|e| e.periph == ID && e.off == 0x008)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].now, VTime(0));
        assert_eq!(entries[0].access, TouchAccess::Read);
    }

    #[test]
    fn a_named_register_is_reported_once_per_machine() {
        let mut bank = Bank::default();
        let mut l = ledger();
        bank.read(0x000, Size::B4, ID, VTime(7), &mut l);
        assert!(bank.is_touched(0x000));
        for t in 8..12 {
            bank.write(0x000, Size::B4, t, ID, VTime(t.into()), &mut l);
        }
        let entries: Vec<_> = l
            .first_touches()
            .iter()
            .filter(|e| e.off == 0x000)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].now, VTime(7));
    }

    #[test]
    fn a_reset_restores_the_table_and_drops_the_unnamed_words() {
        let mut bank = Bank::default();
        let mut l = ledger();
        bank.write(0x000, Size::B4, 0, ID, VTime(0), &mut l);
        bank.write(0x008, Size::B4, 0xFFFF_FFFF, ID, VTime(0), &mut l);
        assert_eq!(bank.get(0), 0);

        let chip = ResetKind::of(ResetCause::POWERON).expect("documented cause");
        bank.reset(chip);
        assert_eq!(bank.get(0), SPECS[0].reset, "the table row is restored");
        assert_eq!(
            bank.read(0x008, Size::B4, ID, VTime(1), &mut l),
            0,
            "an unnamed word has no reset value of its own, so it drops"
        );
        assert!(bank.is_touched(0x000), "the first touch is not a reset");

        // A reset that restores no register (CpuAndPms) leaves the unnamed words alone.
        bank.write(0x008, Size::B4, 0x55, ID, VTime(2), &mut l);
        let none = ResetKind::of(ResetCause::RTC_SW_CPU).expect("documented cause");
        assert!(!none.clears(SPECS[0].domain), "the fixture is digital");
        bank.reset(none);
        assert_eq!(bank.read(0x008, Size::B4, ID, VTime(3), &mut l), 0x55);
    }

    #[test]
    fn reset_all_restores_every_register_whatever_the_kind() {
        // SENSITIVE: every register, lock bits included, after every reset kind.
        let mut bank = Bank::default();
        let mut l = ledger();
        bank.write(0x000, Size::B4, 0, ID, VTime(0), &mut l);
        bank.write(0x008, Size::B4, 1, ID, VTime(0), &mut l);
        bank.reset_all();
        assert_eq!(bank.get(0), SPECS[0].reset);
        assert_eq!(bank.read(0x008, Size::B4, ID, VTime(1), &mut l), 0);
    }

    #[test]
    fn the_snapshot_state_round_trips_and_a_short_one_is_refused() {
        // A state whose lengths do not fit the table is refused rather than restored in part.
        let mut bank = Bank::default();
        let mut l = ledger();
        bank.write(0x000, Size::B4, 0xC0FF_EE00, ID, VTime(0), &mut l);
        bank.write(0x008, Size::B4, 0x99, ID, VTime(0), &mut l);

        let state = bank.to_state();
        assert_eq!(state.vals, vec![0xC0FF_EE00, 0]);
        assert_eq!(state.touched, vec![1]);
        assert_eq!(state.extra, BTreeMap::from([(0x008, 0x99)]));

        let back = Bank::from_state(state).expect("the state fits the table");
        assert_eq!(back.get(0), 0xC0FF_EE00);
        assert!(back.is_touched(0x000), "and does not report a second time");
        assert!(!back.is_touched(0x004));
        assert_eq!(back.to_state().extra, BTreeMap::from([(0x008, 0x99)]));

        let short = RegsState {
            vals: vec![0],
            touched: vec![0],
            extra: BTreeMap::new(),
        };
        let Err(err) = Bank::from_state(short) else {
            panic!("one value cannot fill two registers")
        };
        assert!(
            err.starts_with("test: snapshot has 1 of 2 registers"),
            "{err}"
        );

        let wide = RegsState {
            vals: vec![0, 0],
            touched: vec![0, 0],
            extra: BTreeMap::new(),
        };
        assert!(
            Bank::from_state(wide).map(|_| ()).is_err(),
            "two touch words for one"
        );
    }

    #[test]
    fn touches_reports_each_register_once_and_each_unnamed_word_once() {
        let mut t = Touches::<TestBlock, 2>::default();
        let mut l = ledger();
        t.read(0x000, Size::B4, ID, VTime(1), &mut l);
        t.write(0x000, Size::B4, ID, VTime(2), &mut l);
        assert!(t.is_touched(0x000));
        assert_eq!(t.class_at(0x000), Fidelity::B);
        assert_eq!(t.class_at(0x004), Fidelity::C);
        assert_eq!(t.class_at(0x008), Fidelity::U);

        t.write(0x002, Size::B4, ID, VTime(3), &mut l);
        assert!(t.is_touched(0x004));

        for now in 4..8 {
            t.read(0x008, Size::B4, ID, VTime(now), &mut l);
        }
        let unnamed: Vec<_> = l
            .first_touches()
            .iter()
            .filter(|e| e.off == 0x008)
            .collect();
        assert_eq!(unnamed.len(), 1, "an unnamed word is reported once");
        assert_eq!(unnamed[0].now, VTime(4));
        assert_eq!(
            l.first_touches().len(),
            3,
            "two registers and one unnamed word"
        );
    }

    #[test]
    fn write_bits_moves_the_value_into_the_access_window() {
        // Bytes outside the access are 0, so a model that clears by the written value clears
        // nothing outside it.
        assert_eq!(write_bits(0x000, Size::B4, 0xAABB_CCDD), 0xAABB_CCDD);
        assert_eq!(write_bits(0x001, Size::B1, 0xFF), 0x0000_FF00);
        assert_eq!(write_bits(0x002, Size::B2, 0x1234), 0x1234_0000);
        assert_eq!(write_bits(0x003, Size::B1, 0x00), 0);
        assert_eq!(
            write_bits(0x000, Size::B1, 0xFFFF),
            0x0000_00FF,
            "the access truncates"
        );
    }

    #[test]
    fn mask_covers_the_full_width() {
        assert_eq!(mask(0), 0);
        assert_eq!(mask(1), 1);
        assert_eq!(mask(20), 0x000F_FFFF);
        assert_eq!(mask(31), 0x7FFF_FFFF);
        assert_eq!(mask(32), u32::MAX);
        assert_eq!(byte_mask(Size::B1), 0xFF);
        assert_eq!(byte_mask(Size::B2), 0xFFFF);
        assert_eq!(byte_mask(Size::B4), u32::MAX);
    }

    #[test]
    fn a_field_reads_and_writes_only_its_bits() {
        let mut bank = Bank::default();
        bank.set(1, 0);
        bank.set_field(1, 8, 4, 0xF);
        assert_eq!(bank.get(1), 0x0000_0F00);
        assert_eq!(bank.field(1, 8, 4), 0xF);
        bank.set_field(1, 8, 4, 0xFF);
        assert_eq!(bank.get(1), 0x0000_0F00, "the value is masked to the width");
        bank.set_field(1, 0, 32, u32::MAX);
        assert_eq!(bank.get(1), u32::MAX);
        assert_eq!(bank.field(1, 0, 32), u32::MAX);
    }

    #[test]
    fn the_scope_of_a_reset_kind_decides_what_the_bank_restores() {
        // The fixture is a digital block, so every reset scope restores it.
        for scope in ResetScope::ALL {
            assert!(
                pemu_core::regstore::resets_in(SPECS[0].domain, scope),
                "{scope:?}"
            );
        }
    }
}
