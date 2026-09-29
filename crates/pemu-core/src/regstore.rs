//! Register storage with per-field access semantics. A 1, 2 or 4 byte access at `byte_off`
//! touches only the addressed bytes; bytes past byte 3 read 0 and ignore writes.
//!
//! Per-bit semantics, by the access (IDF register header notation) of the covering field:
//!
//! | Access | Read | Write (addressed bytes only) | Reported in `Delta` |
//! |---|---|---|---|
//! | `Rw` | stored | stores the written bits | |
//! | `Ro` | stored | ignored | |
//! | `Wo` | 0 | stores the written bits | |
//! | `W1c` | stored | 1 clears, 0 keeps | `w1c`: bits written 1 |
//! | `W1s` | stored | 1 sets, 0 keeps | |
//! | `Wt` | 0 | nothing stored | `triggers`: bits written 1 |
//! | `Sc` | stored | 1 sets, 0 keeps | `triggers`: bits written 1 |
//! | `Rc` | stored, then the returned bits clear | ignored | |
//!
//! UNVERIFIED: WO reading 0 is a design choice, as are the items marked below.

use crate::fidelity::Fidelity;
use crate::reset::{ResetDomain, ResetScope};

/// Access semantics of one register field (module table).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FieldAccess {
    Rw,
    Ro,
    Wo,
    W1c,
    W1s,
    Wt,
    Sc,
    Rc,
}

pub struct FieldSpec {
    pub name: &'static str,
    pub shift: u8,
    pub width: u8,
    pub access: FieldAccess,
    pub reset: u32,
}

/// One register of a block, generated from specs.
pub struct RegSpec {
    pub name: &'static str,
    pub off: u16,
    pub reset: u32,
    pub fields: &'static [FieldSpec],
    /// From the block's reset_domains rows.
    pub domain: ResetDomain,
    /// Eligible for oracle read comparison.
    pub stable_read: bool,
    pub class: Fidelity,
    pub cite: &'static str,
}

#[derive(Copy, Clone)]
pub enum Size {
    B1 = 1,
    B2 = 2,
    B4 = 4,
}

/// Bit of `scope` in a `ResetDomain` mask: Chip 0x1, System 0x2, Core 0x4, each tested alone.
/// UNVERIFIED encoding: a design choice.
pub const fn scope_bit(scope: ResetScope) -> u8 {
    match scope {
        ResetScope::Chip => 0x1,
        ResetScope::System => 0x2,
        ResetScope::Core => 0x4,
    }
}

pub const RESET_BY_ALL_SCOPES: ResetDomain = ResetDomain(0x7);

pub const fn resets_in(domain: ResetDomain, scope: ResetScope) -> bool {
    domain.0 & scope_bit(scope) != 0
}

/// Generated per block into `pemu-soc-c3/src/gen/regs_<block>.rs`.
pub struct RegStore<const N: usize> {
    vals: [u32; N],
    specs: &'static [RegSpec; N],
}

impl<const N: usize> RegStore<N> {
    /// Every register at its reset value; `const` so a generated file can build it in a static.
    /// UNVERIFIED: a design choice.
    pub const fn new(specs: &'static [RegSpec; N]) -> Self {
        let mut vals = [0u32; N];
        let mut i = 0;
        while i < N {
            vals[i] = specs[i].reset;
            i += 1;
        }
        RegStore { vals, specs }
    }

    /// The addressed bytes of register `idx`, shifted to bit 0; returned RC bits clear afterwards.
    pub fn read(&mut self, idx: usize, byte_off: u8, size: Size) -> u32 {
        let (bytes, shift) = window(byte_off, size);
        let m = Masks::of(&self.specs[idx]);
        let stored = self.vals[idx];
        self.vals[idx] = stored & !(m.rc & bytes);
        (stored & !m.hidden & bytes) >> shift
    }

    /// Writes the low `size` bytes of `v` at `byte_off`; the `Delta` is what the owner must act on.
    pub fn write(&mut self, idx: usize, byte_off: u8, size: Size, v: u32) -> Delta {
        let (bytes, shift) = window(byte_off, size);
        let w = ((u64::from(v) << shift) as u32) & bytes;
        let m = Masks::of(&self.specs[idx]);
        let before = self.vals[idx];
        let stored = m.stored & bytes;
        let after = (((before & !stored) | (w & stored)) & !(w & m.w1c)) | (w & m.w1s);
        self.vals[idx] = after;
        Delta {
            before,
            after,
            w1c: w & m.w1c,
            triggers: w & m.triggers,
        }
    }

    /// Restores the reset value of every register whose domain a reset of `scope` clears.
    pub fn reset(&mut self, scope: ResetScope) {
        for (val, spec) in self.vals.iter_mut().zip(self.specs.iter()) {
            if resets_in(spec.domain, scope) {
                *val = spec.reset;
            }
        }
    }

    /// The stored value without side effects, WO and WT bits included.
    pub fn get(&self, idx: usize) -> u32 {
        self.vals[idx]
    }

    /// Hardware-side update bypassing access semantics, for RO status, W1C raw bits and counters.
    /// UNVERIFIED: a design choice.
    pub fn set(&mut self, idx: usize, v: u32) {
        self.vals[idx] = v;
    }

    /// Clears the SC bits of `mask` in register `idx`, the only way an SC bit clears, so a
    /// busy-wait cannot end on an unrelated write.
    /// UNVERIFIED: a design choice.
    pub fn clear_sc(&mut self, idx: usize, mask: u32) {
        let sc = Masks::of(&self.specs[idx]).sc;
        self.vals[idx] &= !(mask & sc);
    }

    pub fn spec(&self, idx: usize) -> &'static RegSpec {
        let specs: &'static [RegSpec; N] = self.specs;
        &specs[idx]
    }

    pub fn index_of(&self, off: u16) -> Option<usize> {
        self.specs.iter().position(|s| s.off == off)
    }
}

/// Result of a register write, at register bit positions. `w1c` holds the W1C bits written 1, set
/// or not (`before & w1c` is what cleared); `triggers` the WT and SC bits written 1.
/// UNVERIFIED: a design choice.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Delta {
    pub before: u32,
    pub after: u32,
    pub w1c: u32,
    pub triggers: u32,
}

/// Bit masks of one register, by what an access does to each bit.
#[derive(Copy, Clone, Default)]
struct Masks {
    /// A write stores the written bit: RW, WO.
    stored: u32,
    /// Reads 0: WO, WT.
    hidden: u32,
    w1c: u32,
    /// A written 1 sets the bit and a written 0 keeps it: W1S, SC.
    w1s: u32,
    triggers: u32,
    sc: u32,
    rc: u32,
}

impl Masks {
    /// Masks of `spec`: an empty field list is plain RW; RO and reserved bits are in no mask.
    fn of(spec: &RegSpec) -> Masks {
        if spec.fields.is_empty() {
            return Masks {
                stored: u32::MAX,
                ..Masks::default()
            };
        }
        let mut m = Masks::default();
        for f in spec.fields {
            let bits = field_bits(f);
            match f.access {
                FieldAccess::Rw => m.stored |= bits,
                FieldAccess::Ro => {}
                FieldAccess::Wo => {
                    m.stored |= bits;
                    m.hidden |= bits;
                }
                FieldAccess::W1c => m.w1c |= bits,
                FieldAccess::W1s => m.w1s |= bits,
                FieldAccess::Wt => {
                    m.hidden |= bits;
                    m.triggers |= bits;
                }
                FieldAccess::Sc => {
                    m.w1s |= bits;
                    m.triggers |= bits;
                    m.sc |= bits;
                }
                FieldAccess::Rc => m.rc |= bits,
            }
        }
        m
    }
}

/// Bits no field covers read their stored reset value and ignore writes.
/// UNVERIFIED: a design choice; generated per-block tests check it against the register's CSV row.
pub fn reserved_bits(spec: &RegSpec) -> u32 {
    if spec.fields.is_empty() {
        return 0;
    }
    let mut covered = 0u32;
    for f in spec.fields {
        covered |= field_bits(f);
    }
    !covered
}

fn field_bits(f: &FieldSpec) -> u32 {
    if f.shift >= 32 {
        return 0;
    }
    let ones = if f.width >= 32 {
        u64::from(u32::MAX)
    } else {
        (1u64 << f.width) - 1
    };
    (ones << f.shift) as u32
}

/// Mask of the bytes an access addresses and the bit shift of `byte_off`; bytes past 3 drop.
fn window(byte_off: u8, size: Size) -> (u32, u32) {
    if byte_off >= 4 {
        return (0, 0);
    }
    let shift = u32::from(byte_off) * 8;
    let ones: u64 = match size {
        Size::B1 => 0xFF,
        Size::B2 => 0xFFFF,
        Size::B4 => 0xFFFF_FFFF,
    };
    ((ones << shift) as u32, shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn field(name: &'static str, shift: u8, width: u8, access: FieldAccess) -> FieldSpec {
        FieldSpec {
            name,
            shift,
            width,
            access,
            reset: 0,
        }
    }

    const fn reg(
        name: &'static str,
        off: u16,
        reset: u32,
        fields: &'static [FieldSpec],
        domain: ResetDomain,
    ) -> RegSpec {
        RegSpec {
            name,
            off,
            reset,
            fields,
            domain,
            stable_read: false,
            class: Fidelity::B,
            cite: "hand-written test table",
        }
    }

    /// Every access type as one full-width field, then MIXED, then a register without fields.
    static SPECS: [RegSpec; 10] = [
        reg(
            "RW",
            0x00,
            0x1122_3344,
            &[field("F", 0, 32, FieldAccess::Rw)],
            RESET_BY_ALL_SCOPES,
        ),
        reg(
            "RO",
            0x04,
            0x1122_3344,
            &[field("F", 0, 32, FieldAccess::Ro)],
            RESET_BY_ALL_SCOPES,
        ),
        reg(
            "WO",
            0x08,
            0,
            &[field("F", 0, 32, FieldAccess::Wo)],
            RESET_BY_ALL_SCOPES,
        ),
        reg(
            "W1C",
            0x0C,
            0,
            &[field("F", 0, 32, FieldAccess::W1c)],
            RESET_BY_ALL_SCOPES,
        ),
        reg(
            "W1S",
            0x10,
            0,
            &[field("F", 0, 32, FieldAccess::W1s)],
            RESET_BY_ALL_SCOPES,
        ),
        reg(
            "WT",
            0x14,
            0,
            &[field("F", 0, 32, FieldAccess::Wt)],
            RESET_BY_ALL_SCOPES,
        ),
        reg(
            "SC",
            0x18,
            0,
            &[field("F", 0, 32, FieldAccess::Sc)],
            RESET_BY_ALL_SCOPES,
        ),
        reg(
            "RC",
            0x1C,
            0,
            &[field("F", 0, 32, FieldAccess::Rc)],
            RESET_BY_ALL_SCOPES,
        ),
        reg(
            "MIXED",
            0x20,
            0x0000_0003,
            &MIXED_FIELDS,
            RESET_BY_ALL_SCOPES,
        ),
        reg("PLAIN", 0x28, 0xCAFE_F00D, &[], RESET_BY_ALL_SCOPES),
    ];

    /// Bits 1:0 reserved, then RW 5:2, RO 9:6, WO 13:10, W1C 17:14, W1S 21:18, WT 25:22,
    /// SC 29:26, RC 31:30.
    static MIXED_FIELDS: [FieldSpec; 8] = [
        field("RW", 2, 4, FieldAccess::Rw),
        field("RO", 6, 4, FieldAccess::Ro),
        field("WO", 10, 4, FieldAccess::Wo),
        field("W1C", 14, 4, FieldAccess::W1c),
        field("W1S", 18, 4, FieldAccess::W1s),
        field("WT", 22, 4, FieldAccess::Wt),
        field("SC", 26, 4, FieldAccess::Sc),
        field("RC", 30, 2, FieldAccess::Rc),
    ];

    const MIXED: usize = 8;
    const PLAIN: usize = 9;

    static RESET_STORE: RegStore<10> = RegStore::new(&SPECS);

    const SIZES: [Size; 3] = [Size::B1, Size::B2, Size::B4];

    /// Stored value before each access of the table test.
    const S: u32 = 0xA5C3_0F96;
    /// Register image of the written bytes: byte `i` of a write is byte `i` of `P`.
    const P: u32 = 0x5A3C_F069;

    struct Case {
        idx: usize,
        /// Stored byte after a write, from the stored byte and the written byte.
        write: fn(u8, u8) -> u8,
        reads: bool,
        w1c: bool,
        triggers: bool,
        rc: bool,
    }

    const fn case(idx: usize, write: fn(u8, u8) -> u8, reads: bool) -> Case {
        Case {
            idx,
            write,
            reads,
            w1c: false,
            triggers: false,
            rc: false,
        }
    }

    static CASES: [Case; 8] = [
        case(0, |_, p| p, true),
        case(1, |s, _| s, true),
        case(2, |_, p| p, false),
        Case {
            w1c: true,
            ..case(3, |s, p| s & !p, true)
        },
        case(4, |s, p| s | p, true),
        Case {
            triggers: true,
            ..case(5, |s, _| s, false)
        },
        Case {
            triggers: true,
            ..case(6, |s, p| s | p, true)
        },
        Case {
            rc: true,
            ..case(7, |s, _| s, true)
        },
    ];

    fn byte(v: u32, i: u32) -> u8 {
        (v >> (i * 8)) as u8
    }

    #[test]
    fn every_access_type_at_every_width_and_offset() {
        for c in &CASES {
            let name = SPECS[c.idx].name;
            for size in SIZES {
                for off in 0..4u8 {
                    let first = u32::from(off);
                    let addressed = |i: u32| i >= first && i < first + size as u32;
                    let at = format!("{name} size {} offset {off}", size as u8);

                    let mut st = RegStore::new(&SPECS);
                    st.set(c.idx, S);
                    let d = st.write(c.idx, off, size, P >> (first * 8));
                    let (mut after, mut reported) = (0, 0);
                    for i in 0..4 {
                        let (s, p) = (byte(S, i), byte(P, i));
                        let b = if addressed(i) { (c.write)(s, p) } else { s };
                        after |= u32::from(b) << (i * 8);
                        if addressed(i) {
                            reported |= u32::from(p) << (i * 8);
                        }
                    }
                    let w1c = if c.w1c { reported } else { 0 };
                    let triggers = if c.triggers { reported } else { 0 };
                    let want = Delta {
                        before: S,
                        after,
                        w1c,
                        triggers,
                    };
                    assert_eq!(d, want, "write {at}");
                    assert_eq!(st.get(c.idx), after, "stored after write {at}");

                    let mut st = RegStore::new(&SPECS);
                    st.set(c.idx, S);
                    let (mut val, mut left) = (0, 0);
                    for i in 0..4 {
                        let s = byte(S, i);
                        if addressed(i) && c.reads {
                            val |= u32::from(s) << ((i - first) * 8);
                        }
                        let kept = if addressed(i) && c.rc { 0 } else { s };
                        left |= u32::from(kept) << (i * 8);
                    }
                    assert_eq!(st.read(c.idx, off, size), val, "read {at}");
                    assert_eq!(st.get(c.idx), left, "stored after read {at}");
                }
            }
        }
    }

    #[test]
    fn w1c_narrow_writes_clear_only_addressed_ones() {
        let mut st = RegStore::new(&SPECS);
        st.set(3, 0xFFFF_FFFF);
        let d = st.write(3, 2, Size::B1, 0x81);
        assert_eq!(d.after, 0xFF7E_FFFF);
        assert_eq!(d.w1c, 0x0081_0000);
        assert_eq!(d.triggers, 0);
        let d = st.write(3, 1, Size::B2, 0x0180);
        assert_eq!(d.before, 0xFF7E_FFFF);
        assert_eq!(d.after, 0xFF7E_7FFF);
        assert_eq!(
            d.w1c, 0x0001_8000,
            "bits written 1, also when already clear"
        );
        let d = st.write(3, 0, Size::B1, 0xFFFF_FF00);
        assert_eq!(
            (d.after, d.w1c),
            (0xFF7E_7FFF, 0),
            "bytes above the size drop"
        );
    }

    #[test]
    fn w1s_narrow_writes_set_only_addressed_ones() {
        let mut st = RegStore::new(&SPECS);
        assert_eq!(st.write(4, 3, Size::B1, 0x80).after, 0x8000_0000);
        assert_eq!(st.write(4, 2, Size::B2, 0x0001).after, 0x8001_0000);
        let d = st.write(4, 0, Size::B4, 0);
        assert_eq!((d.before, d.after), (0x8001_0000, 0x8001_0000));
        assert_eq!(st.read(4, 2, Size::B1), 0x01);
    }

    /// An RC field 11:4 that spans bytes 0 and 1, next to an RW field 3:0.
    static RC_SPAN: [RegSpec; 1] = [reg(
        "RC_SPAN",
        0,
        0,
        &[
            field("RW", 0, 4, FieldAccess::Rw),
            field("RC", 4, 8, FieldAccess::Rc),
        ],
        RESET_BY_ALL_SCOPES,
    )];

    #[test]
    fn rc_reads_clear_only_returned_bits() {
        let mut st = RegStore::new(&SPECS);
        st.set(7, 0x1234_5678);
        assert_eq!(st.read(7, 1, Size::B1), 0x56);
        assert_eq!(st.get(7), 0x1234_0078);
        assert_eq!(st.read(7, 2, Size::B2), 0x1234);
        assert_eq!(st.get(7), 0x0000_0078);
        assert_eq!(st.read(7, 0, Size::B4), 0x78);
        assert_eq!(st.read(7, 0, Size::B4), 0);
        st.set(7, 0x0F);
        let d = st.write(7, 0, Size::B4, 0xFFFF_FFFF);
        assert_eq!((d.after, d.triggers), (0x0F, 0), "RC bits ignore writes");

        let mut st = RegStore::new(&RC_SPAN);
        st.set(0, 0xFFF);
        assert_eq!(st.read(0, 0, Size::B1), 0xFF);
        assert_eq!(st.get(0), 0xF0F, "RW bits stay; unread RC bits 11:8 stay");
        assert_eq!(st.read(0, 1, Size::B1), 0x0F);
        assert_eq!(st.get(0), 0x00F);
    }

    #[test]
    fn one_register_holds_every_field_type() {
        let mut st = RegStore::new(&SPECS);
        assert_eq!(st.get(MIXED), 0x3);
        assert_eq!(
            st.read(MIXED, 0, Size::B4),
            0x3,
            "reserved bits read their reset value"
        );

        let d = st.write(MIXED, 0, Size::B4, 0xFFFF_FFFF);
        let want = Delta {
            before: 0x0000_0003,
            after: 0x3C3C_3C3F,
            w1c: 0x0003_C000,
            triggers: 0x3FC0_0000,
        };
        assert_eq!(
            d, want,
            "RW, WO, SC stored; W1S set; RO, RC, WT, reserved untouched"
        );
        assert_eq!(st.read(MIXED, 0, Size::B4), 0x3C3C_003F, "WO and WT read 0");

        let d = st.write(MIXED, 1, Size::B1, 0);
        assert_eq!(
            (d.after, d.w1c, d.triggers),
            (0x3C3C_003F, 0, 0),
            "WO 13:10 cleared"
        );

        st.set(
            MIXED,
            st.get(MIXED) | 0x0003_C000 | 0x0000_03C0 | 0xC000_0000,
        );
        assert_eq!(st.get(MIXED), 0xFC3F_C3FF);
        let d = st.write(MIXED, 2, Size::B1, 0x01);
        let want = Delta {
            before: 0xFC3F_C3FF,
            after: 0xFC3E_C3FF,
            w1c: 0x0001_0000,
            triggers: 0,
        };
        assert_eq!(d, want, "W1C bit 16 of field 17:14 clears alone");

        assert_eq!(st.read(MIXED, 2, Size::B2), 0xFC3E);
        assert_eq!(st.get(MIXED), 0x3C3E_C3FF, "RC 31:30 cleared by the read");

        let d = st.write(MIXED, 3, Size::B1, 0xFF);
        let want = Delta {
            before: 0x3C3E_C3FF,
            after: 0x3C3E_C3FF,
            w1c: 0,
            triggers: 0x3F00_0000,
        };
        assert_eq!(
            d, want,
            "WT 25:24 and SC 29:26 trigger; WT 23:22 not addressed"
        );

        st.clear_sc(MIXED, u32::MAX);
        assert_eq!(st.get(MIXED), 0x003E_C3FF, "only SC bits clear");

        st.set(MIXED, 0);
        assert_eq!(
            st.write(MIXED, 0, Size::B1, 0xFF).after,
            0x3C,
            "reserved 1:0 ignore writes"
        );

        st.reset(ResetScope::System);
        assert_eq!(st.get(MIXED), 0x3);
    }

    const NONE: ResetDomain = ResetDomain(0);
    const CHIP: ResetDomain = ResetDomain(scope_bit(ResetScope::Chip));
    const CHIP_SYSTEM: ResetDomain =
        ResetDomain(scope_bit(ResetScope::Chip) | scope_bit(ResetScope::System));
    const CORE: ResetDomain = ResetDomain(scope_bit(ResetScope::Core));

    static DOMAINS: [RegSpec; 5] = [
        reg("NONE", 0x0, 0x100, &[], NONE),
        reg("CHIP", 0x4, 0x101, &[], CHIP),
        reg("CHIP_SYSTEM", 0x8, 0x102, &[], CHIP_SYSTEM),
        reg("ALL", 0xC, 0x103, &[], RESET_BY_ALL_SCOPES),
        reg("CORE", 0x10, 0x104, &[], CORE),
    ];

    #[test]
    fn reset_restores_by_scope_and_domain() {
        // Per register: restored by Chip, System, Core.
        let table = [
            [false, false, false],
            [true, false, false],
            [true, true, false],
            [true, true, true],
            [false, false, true],
        ];
        let scopes = [ResetScope::Chip, ResetScope::System, ResetScope::Core];
        for (s, scope) in scopes.into_iter().enumerate() {
            let mut st = RegStore::new(&DOMAINS);
            for idx in 0..DOMAINS.len() {
                st.set(idx, 0xFFFF_FFFF);
            }
            st.reset(scope);
            for (idx, row) in table.iter().enumerate() {
                let want = if row[s] {
                    0x100 + idx as u32
                } else {
                    0xFFFF_FFFF
                };
                assert_eq!(st.get(idx), want, "{} after {scope:?}", DOMAINS[idx].name);
            }
        }
        assert_eq!(scope_bit(ResetScope::Chip), 0x1);
        assert_eq!(scope_bit(ResetScope::System), 0x2);
        assert_eq!(scope_bit(ResetScope::Core), 0x4);
    }

    #[test]
    fn register_without_fields_is_plain_rw() {
        let mut st = RegStore::new(&SPECS);
        assert_eq!(st.get(PLAIN), 0xCAFE_F00D);
        let d = st.write(PLAIN, 1, Size::B1, 0xAA);
        assert_eq!((d.after, d.w1c, d.triggers), (0xCAFE_AA0D, 0, 0));
        assert_eq!(st.read(PLAIN, 1, Size::B2), 0xFEAA);
        assert_eq!(st.get(PLAIN), 0xCAFE_AA0D);
    }

    #[test]
    fn static_store_holds_reset_values_and_finds_registers() {
        assert_eq!(RESET_STORE.get(0), 0x1122_3344);
        assert_eq!(RESET_STORE.get(PLAIN), 0xCAFE_F00D);
        assert_eq!(RESET_STORE.spec(MIXED).name, "MIXED");
        assert_eq!(RESET_STORE.spec(5).fields[0].access, FieldAccess::Wt);
        assert_eq!(RESET_STORE.index_of(0x28), Some(PLAIN));
        assert_eq!(RESET_STORE.index_of(0x24), None);
    }

    #[test]
    fn reserved_bits_name_the_uncovered_bits() {
        assert_eq!(
            reserved_bits(&SPECS[MIXED]),
            0x3,
            "only bits 1:0 are left out of MIXED_FIELDS"
        );
        assert_eq!(reserved_bits(&SPECS[0]), 0, "one field covers the register");
        assert_eq!(
            reserved_bits(&SPECS[PLAIN]),
            0,
            "an empty field list is plain RW, not reserved"
        );
        assert_eq!(reserved_bits(&RC_SPAN[0]), 0xFFFF_F000);
    }

    #[test]
    fn edges_of_fields_and_windows() {
        assert_eq!(field_bits(&field("A", 0, 32, FieldAccess::Rw)), u32::MAX);
        assert_eq!(field_bits(&field("B", 28, 8, FieldAccess::Rw)), 0xF000_0000);
        assert_eq!(field_bits(&field("C", 32, 1, FieldAccess::Rw)), 0);
        assert_eq!(field_bits(&field("D", 5, 0, FieldAccess::Rw)), 0);

        let mut st = RegStore::new(&SPECS);
        assert_eq!(
            st.read(0, 4, Size::B4),
            0,
            "byte offset 4 addresses nothing"
        );
        let d = st.write(0, 4, Size::B4, u32::MAX);
        assert_eq!((d.before, d.after), (0x1122_3344, 0x1122_3344));
        assert_eq!(st.read(0, 3, Size::B4), 0x11, "bytes past byte 3 drop");
        assert_eq!(st.write(0, 2, Size::B4, 0xFFFF_FFFF).after, 0xFFFF_3344);
    }

    #[test]
    fn clear_sc_leaves_other_bits() {
        let mut st = RegStore::new(&SPECS);
        let d = st.write(6, 0, Size::B4, 0x8000_0001);
        assert_eq!((d.after, d.triggers), (0x8000_0001, 0x8000_0001));
        let d = st.write(6, 0, Size::B4, 0);
        assert_eq!(
            (d.after, d.triggers),
            (0x8000_0001, 0),
            "a written 0 keeps a pending SC bit, so it cannot end a busy-wait early"
        );
        st.clear_sc(6, 0x1);
        assert_eq!(
            st.get(6),
            0x8000_0000,
            "SC bit stays set until its owner clears it"
        );
        st.set(0, 0xFFFF_FFFF);
        st.clear_sc(0, u32::MAX);
        assert_eq!(st.get(0), 0xFFFF_FFFF, "RW register has no SC bits");
    }
}
