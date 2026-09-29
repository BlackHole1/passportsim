//! [`RegFile`]: the register file a modeled block sits on, as `store_only.rs` is for unmodeled
//! ones. Unlike `RegBank` it applies the access column of the spec rows
//! (`specs/c3-registers.csv`: WT reads 0, SC clears when its effect completes, W1C) and still
//! reports one first touch per register.

use core::marker::PhantomData;

use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::regstore::{Delta, RegSpec, RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::serde::de::{Deserialize, Deserializer};
use pemu_core::serde::ser::{Serialize, Serializer};
use pemu_core::time::VTime;

use super::Block;

/// A block driven by its generated `RegSpec` table. The table travels with the block marker, so
/// [`RegFile`] can rebuild a `RegStore` on snapshot restore, where only the values are on the
/// wire.
pub trait RegTable<const N: usize>: Block {
    /// The block's generated register table, in offset order.
    const SPECS: &'static [RegSpec; N];
}

/// The register file of a modeled block. Accesses split at register boundaries and at the
/// widths `Size` can express; bytes with no register read 0 and ignore writes.
pub struct RegFile<B: RegTable<N>, const N: usize> {
    regs: RegStore<N>,
    /// Bit per 4-byte slot: already reported. Snapshotted, so a restored machine does not report
    /// a register twice.
    touched: Vec<u64>,
    block: PhantomData<fn() -> B>,
}

/// Snapshot form of a [`RegFile`]: values and first-touch bits, without the `&'static` table.
#[derive(pemu_core::serde::Serialize, pemu_core::serde::Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct RegFileState {
    vals: Vec<u32>,
    touched: Vec<u64>,
}

impl<B: RegTable<N>, const N: usize> Default for RegFile<B, N> {
    fn default() -> Self {
        RegFile {
            regs: RegStore::new(B::SPECS),
            touched: vec![0; (B::SIZE.div_ceil(4) as usize).div_ceil(64)],
            block: PhantomData,
        }
    }
}

impl<B: RegTable<N>, const N: usize> Serialize for RegFile<B, N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        RegFileState {
            vals: (0..N).map(|i| self.regs.get(i)).collect(),
            touched: self.touched.clone(),
        }
        .serialize(s)
    }
}

impl<'de, B: RegTable<N>, const N: usize> Deserialize<'de> for RegFile<B, N> {
    /// Refuses a state whose lengths do not match the table and window rather than truncating
    /// or padding it.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let state = RegFileState::deserialize(d)?;
        let mut file = RegFile::<B, N>::default();
        if state.vals.len() != N || state.touched.len() != file.touched.len() {
            return Err(<D::Error as pemu_core::serde::de::Error>::custom(
                "register file snapshot has the wrong register count or first-touch length",
            ));
        }
        for (i, val) in state.vals.iter().enumerate() {
            file.regs.set(i, *val);
        }
        file.touched = state.touched;
        Ok(file)
    }
}

impl<B: RegTable<N>, const N: usize> RegFile<B, N> {
    /// Index in the generated table of the register at block offset `off`.
    ///
    /// A binary search (the tables are in offset order): the SPI2 window on this file is the
    /// hottest MMIO page of a boot by a factor of thirty.
    pub fn index_of(off: u32) -> Option<usize> {
        let off = u16::try_from(off).ok()?;
        B::SPECS.binary_search_by_key(&off, |spec| spec.off).ok()
    }

    /// Reads `size` bytes at `off` with the access semantics of each field, reporting the first
    /// touch of every register the access covers.
    pub fn read(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        let mut val = 0;
        let mut parts = Parts::of(off, size, B::SIZE);
        while let Some(part) = parts.next() {
            self.touch(part.reg_off, TouchAccess::Read, size, now, ledger);
            if let Some(i) = Self::index_of(part.reg_off) {
                let bits = self.regs.read(i, part.byte_off, part.width) & mask(part.width);
                val |= bits << (part.done * 8);
            }
        }
        val
    }

    /// Writes the low `size` bytes of `val` at `off` and returns each covered register's `Delta`
    /// in offset order: `triggers` carries the WT and SC bits written 1, `w1c` the W1C bits.
    pub fn write(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> Covered {
        let mut out = Covered::default();
        let mut parts = Parts::of(off, size, B::SIZE);
        while let Some(part) = parts.next() {
            self.touch(part.reg_off, TouchAccess::Write, size, now, ledger);
            let Some(i) = Self::index_of(part.reg_off) else {
                continue;
            };
            let bits = (val >> (part.done * 8)) & mask(part.width);
            out.merge(i, self.regs.write(i, part.byte_off, part.width, bits));
        }
        out
    }

    /// Stored value of register `idx`, the owner's view including WO and WT bits.
    pub fn get(&self, idx: usize) -> u32 {
        self.regs.get(idx)
    }

    /// Hardware-side update of register `idx`, bypassing access semantics.
    pub fn set(&mut self, idx: usize, val: u32) {
        self.regs.set(idx, val);
    }

    /// Sets the bits of `mask` in register `idx` from the hardware side.
    pub fn raise(&mut self, idx: usize, mask: u32) {
        self.regs.set(idx, self.regs.get(idx) | mask);
    }

    /// Clears the bits of `mask` in register `idx` from the hardware side.
    pub fn lower(&mut self, idx: usize, mask: u32) {
        self.regs.set(idx, self.regs.get(idx) & !mask);
    }

    /// Completes the effect of the SC bits of `mask` in register `idx`.
    pub fn clear_sc(&mut self, idx: usize, mask: u32) {
        self.regs.clear_sc(idx, mask);
    }

    /// Restores every register this reset clears, keeping the first-touch bits.
    pub fn reset(&mut self, kind: ResetKind) {
        for i in 0..N {
            if kind.clears(B::SPECS[i].domain) {
                self.regs.set(i, B::SPECS[i].reset);
            }
        }
    }

    /// Fidelity class of the register at `off`; `U` for an offset with no register.
    pub fn class(&self, off: u32) -> Fidelity {
        Self::index_of(off & !3).map_or(Fidelity::U, |i| B::SPECS[i].class)
    }

    /// The generated table behind this file.
    pub fn store(&self) -> &RegStore<N> {
        &self.regs
    }

    fn touch(
        &mut self,
        reg_off: u32,
        access: TouchAccess,
        size: Size,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        let slot = (reg_off / 4) as usize;
        let bit = 1u64 << (slot % 64);
        let Some(word) = self.touched.get_mut(slot / 64) else {
            return;
        };
        if *word & bit != 0 {
            return;
        }
        *word |= bit;
        ledger.first_touch(FirstTouch {
            periph: B::ID,
            off: reg_off,
            access,
            size: size as u8,
            now,
            allowlisted: false,
        });
    }
}

/// One piece of an access: the bytes of a single register at a width `Size` can express.
#[derive(Copy, Clone)]
struct Part {
    /// Byte index of the piece inside the whole access.
    done: u32,
    /// Offset of the register the piece belongs to.
    reg_off: u32,
    /// Offset of the piece inside that register.
    byte_off: u8,
    /// Width of the piece: never 3, which `Size` cannot express.
    width: Size,
}

/// Splits an access at register boundaries and `Size` widths, stopping at the end of the
/// window. An iterator so the hot path does not allocate.
struct Parts {
    off: u32,
    total: u32,
    window: u32,
    done: u32,
}

impl Parts {
    fn of(off: u32, size: Size, window: u32) -> Parts {
        Parts {
            off,
            total: size as u32,
            window,
            done: 0,
        }
    }

    #[allow(clippy::should_implement_trait)]
    fn next(&mut self) -> Option<Part> {
        if self.done >= self.total {
            return None;
        }
        let addr = self.off.checked_add(self.done)?;
        if addr >= self.window {
            return None;
        }
        let byte_off = addr % 4;
        let width = match (4 - byte_off).min(self.total - self.done) {
            4 => Size::B4,
            2 | 3 => Size::B2,
            _ => Size::B1,
        };
        let part = Part {
            done: self.done,
            reg_off: addr - byte_off,
            byte_off: byte_off as u8,
            width,
        };
        self.done += width as u32;
        Some(part)
    }
}

/// The registers one access changed, with what each changed by: a fixed pair, since a 4-byte
/// access spans at most two registers and the hot path must not allocate.
#[derive(Copy, Clone, Default, Debug)]
pub struct Covered {
    items: [Option<(usize, Delta)>; 2],
}

impl Covered {
    /// The covered registers in offset order, each with the merged `Delta` of its pieces.
    pub fn iter(&self) -> impl Iterator<Item = (usize, Delta)> + '_ {
        self.items.iter().flatten().copied()
    }

    /// Folds one piece's `Delta` into the register's: `after` is the last piece's, and the
    /// trigger and W1C bits are the union, so a model sees one event per register.
    fn merge(&mut self, reg: usize, delta: Delta) {
        for slot in &mut self.items {
            match slot {
                Some((seen, merged)) if *seen == reg => {
                    merged.after = delta.after;
                    merged.w1c |= delta.w1c;
                    merged.triggers |= delta.triggers;
                    return;
                }
                Some(_) => {}
                None => {
                    *slot = Some((reg, delta));
                    return;
                }
            }
        }
        debug_assert!(false, "an access spans at most two registers");
    }
}

/// Mask of the low `size` bytes.
fn mask(size: Size) -> u32 {
    match size {
        Size::B1 => 0xFF,
        Size::B2 => 0xFFFF,
        Size::B4 => u32::MAX,
    }
}

#[cfg(test)]
mod tests {
    use pemu_core::snap::{SectionId, serde_from_section, serde_section};

    use super::*;
    use crate::r#gen::regs_spi2::REG_COUNT;
    use crate::periph::block;

    type Spi2File = RegFile<block::Spi2, REG_COUNT>;

    fn decode(vals: usize, touched: usize) -> Result<Spi2File, pemu_core::snap::SnapError> {
        let state = RegFileState {
            vals: vec![0; vals],
            touched: vec![0; touched],
        };
        let section = serde_section(&state, 1, "test").expect("encodes");
        serde_from_section(&section, SectionId::soc("spi2"), 1, "test")
    }

    #[test]
    fn a_corrupt_register_file_length_is_refused() {
        let words = Spi2File::default().touched.len();
        assert!(decode(REG_COUNT, words).is_ok());
        for (vals, touched) in [
            (REG_COUNT - 1, words),
            (REG_COUNT + 1, words),
            (0, words),
            (REG_COUNT, words + 1),
            (REG_COUNT, 0),
        ] {
            assert!(
                decode(vals, touched).is_err(),
                "{vals} values and {touched} touch words were accepted"
            );
        }
    }
}
